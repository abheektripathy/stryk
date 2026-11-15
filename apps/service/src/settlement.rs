use crate::chain::{read, sol_usd_account, Account, Chain};
use crate::historical::Historical;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use solana_address::{address, Address};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::Ordering::Relaxed,
    time::Duration,
};
use stryk_client::{ata, prepare_ata, unsigned, wire, FixExpiryInstruction, MarkNoFixInstruction};
use stryk_core::{parse_price_update, SOL_USD_FEED};

const CLOCK: Address = address!("SysvarC1ock11111111111111111111111111111111");

pub async fn run(
    chain: Chain,
    payer: Keypair,
    market: Address,
    proof: Option<Address>,
    historical: Option<Historical>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retries = HashMap::new();
    let mut fixing = HashMap::new();
    loop {
        interval.tick().await;
        let scan = tick(
            &chain,
            &payer,
            market,
            proof,
            historical.as_ref(),
            &mut fixing,
            &mut retries,
        )
        .await;
        chain
            .ops
            .settlement_scan_healthy
            .store(u64::from(scan.is_ok()), Relaxed);
        if let Err(error) = scan {
            tracing::warn!(%error, "settlement scan failed");
        }
    }
}

async fn tick(
    chain: &Chain,
    payer: &Keypair,
    market: Address,
    proof: Option<Address>,
    historical: Option<&Historical>,
    fixing: &mut HashMap<Address, tokio::time::Instant>,
    retries: &mut HashMap<Address, (tokio::time::Instant, u32)>,
) -> Result<()> {
    let (_, accounts) = chain.accounts(&[market, CLOCK]).await?;
    let [Some(market_account), Some(clock)] = accounts.as_slice() else {
        bail!("market or Clock missing");
    };
    let market_data = market_account.stryk(1)?;
    let base = Address::new_from_array(read(market_data, 33)?);
    let quote = Address::new_from_array(read(market_data, 65)?);
    let receiver = Address::new_from_array(read(market_data, 97)?);
    let feed = read::<32>(market_data, 161)?;
    let confidence_bps = u16::from_le_bytes(read(market_data, 225)?);
    let now = i64::from_le_bytes(read(&clock.data, 32)?);
    let mut live_account = sol_usd_account(receiver).filter(|_| feed == SOL_USD_FEED);
    let mut spot = None;
    let (_, expiries) = chain.filtered_accounts(2, &[(1, market.as_ref())]).await?;
    fixing.retain(|_, retry| *retry > tokio::time::Instant::now());
    let mut elapsed_by_expiry = HashMap::new();
    let mut eligible = HashSet::new();
    let mut pending_fixes = 0;
    let mut no_fixes = 0;
    let mut next_deadline = None::<i64>;
    for account in expiries {
        let data = account.stryk(2)?;
        if data.get(1..33) != Some(market.as_ref()) {
            continue;
        }
        let expires_at = i64::from_le_bytes(read(data, 65)?);
        if expires_at > now {
            continue;
        }
        let expiry = account.address;
        elapsed_by_expiry.insert(expiry, now.saturating_sub(expires_at) as u64);
        let status = *data.last().context("missing expiry status")?;
        if status != 0 {
            eligible.insert(expiry);
            no_fixes += u64::from(status == 2);
            continue;
        }
        let deadline = i64::from_le_bytes(read(data, 73)?);
        pending_fixes += 1;
        next_deadline = Some(next_deadline.map_or(deadline, |current| current.min(deadline)));
        let (_, confirmed) = chain.accounts(&[expiry]).await?;
        if let Some(confirmed) = confirmed.first().and_then(Option::as_ref) {
            if confirmed.stryk(2)?.last() != Some(&0) {
                continue;
            }
        }
        let instruction: Instruction = if now >= deadline {
            MarkNoFixInstruction { expiry }.into()
        } else {
            if let Some(address) = live_account.take().filter(|_| proof.is_none()) {
                match chain.accounts(&[address]).await {
                    Ok((_, accounts)) => spot = accounts.into_iter().next().flatten(),
                    Err(error) => {
                        tracing::warn!(%expiry, %address, %error, "live price proof unavailable")
                    }
                }
            }
            let live_proof = || {
                let account = spot.as_ref().filter(|account| account.owner == receiver)?;
                let (price, _) = parse_price_update(&account.data, &feed)?;
                price
                    .is_valid_fix(expires_at, now, confidence_bps)
                    .then_some(account.address)
            };
            let price_update = if let Some(price_update) = proof.or_else(live_proof) {
                price_update
            } else if let Some(poster) = historical {
                if fixing.contains_key(&expiry) {
                    continue;
                }
                fixing.insert(
                    expiry,
                    tokio::time::Instant::now() + Duration::from_secs(45),
                );
                match poster
                    .post(chain, expiry, expires_at, now, confidence_bps)
                    .await
                {
                    Ok(price_update) => price_update,
                    Err(error) => {
                        tracing::warn!(%expiry, seconds_to_deadline = deadline - now, %error, "historical price proof unavailable");
                        continue;
                    }
                }
            } else {
                tracing::warn!(%expiry, seconds_to_deadline = deadline - now, "awaiting historical price proof");
                continue;
            };
            FixExpiryInstruction {
                market,
                expiry,
                price_update,
            }
            .into()
        };
        match send(chain, payer, &[instruction]).await {
            Ok(signature) if now >= deadline => {
                tracing::warn!(%expiry, %signature, "oracle timeout submitted");
            }
            Ok(signature) => tracing::info!(%expiry, %signature, "expiry fix submitted"),
            Err(error) => {
                chain.ops.settlement_failed.fetch_add(1, Relaxed);
                tracing::warn!(%error, %expiry, "expiry transition failed");
            }
        }
    }
    chain.ops.pending_fixes.store(pending_fixes, Relaxed);
    chain.ops.no_fix_expiries.store(no_fixes, Relaxed);
    chain.ops.next_fix_deadline_seconds.store(
        next_deadline.map_or(0, |deadline| deadline.saturating_sub(now).max(0) as u64),
        Relaxed,
    );
    if elapsed_by_expiry.is_empty() {
        chain.ops.oldest_unsettled_seconds.store(0, Relaxed);
        return Ok(());
    }
    let (_, positions) = chain
        .filtered_accounts(5, &[(1, market.as_ref()), (259, &[0])])
        .await?;
    let oldest = positions
        .iter()
        .try_fold(0, |oldest, account| -> Result<u64> {
            let expiry = Address::new_from_array(read(account.stryk(5)?, 129)?);
            Ok(oldest.max(elapsed_by_expiry.get(&expiry).copied().unwrap_or_default()))
        })?;
    chain.ops.oldest_unsettled_seconds.store(oldest, Relaxed);
    let now = tokio::time::Instant::now();
    retries.retain(|_, (deadline, _)| *deadline + Duration::from_secs(120) >= now);
    let mut count = 0;
    for account in positions {
        let data = account.stryk(5)?;
        if data.get(1..33) != Some(market.as_ref()) || data.last() != Some(&0) {
            continue;
        }
        let expiry = Address::new_from_array(read(data, 129)?);
        if !eligible.contains(&expiry) {
            continue;
        }
        if retries
            .get(&account.address)
            .is_some_and(|(deadline, _)| *deadline > now)
        {
            continue;
        }
        let result = settle(chain, payer, market, &account, expiry, base, quote).await;
        let (delay, attempt) = match &result {
            Ok(_) => (5, 0),
            Err(_) => {
                let attempt = retries
                    .get(&account.address)
                    .map_or(1, |(_, attempt)| (attempt + 1).min(5));
                (1_u64 << attempt, attempt)
            }
        };
        retries.insert(account.address, (now + Duration::from_secs(delay), attempt));
        match result {
            Ok(Some(signature)) => {
                tracing::info!(position = %account.address, %signature, "settlement submitted")
            }
            Ok(None) => {}
            Err(error) => {
                chain.ops.settlement_failed.fetch_add(1, Relaxed);
                tracing::warn!(position = %account.address, %error, "position settlement failed")
            }
        }
        count += 1;
        if count == 8 {
            break;
        }
    }
    Ok(())
}

async fn settle(
    chain: &Chain,
    payer: &Keypair,
    market: Address,
    account: &Account,
    expiry: Address,
    base: Address,
    quote: Address,
) -> Result<Option<String>> {
    let data = account.stryk(5)?;
    let position = account.address;
    let writer = Address::new_from_array(read(data, 33)?);
    let maker = Address::new_from_array(read(data, 65)?);
    let writer_base = ata(&writer, &base);
    let writer_quote = ata(&writer, &quote);
    let maker_base = ata(&maker, &base);
    let maker_quote = ata(&maker, &quote);
    let (_, accounts) = chain
        .accounts(&[position, writer_base, writer_quote, maker_base, maker_quote])
        .await?;
    if accounts
        .first()
        .and_then(Option::as_ref)
        .context("position missing")?
        .stryk(5)?
        .last()
        != Some(&0)
    {
        return Ok(None);
    }
    let mut instructions = Vec::with_capacity(5);
    for ((owner, mint), account) in [
        (writer, base),
        (writer, quote),
        (maker, base),
        (maker, quote),
    ]
    .into_iter()
    .zip(accounts.into_iter().skip(1))
    {
        if account.is_none() {
            instructions.push(prepare_ata(payer.pubkey(), owner, mint));
        }
    }
    instructions.push(stryk_client::settle(
        market, position, expiry, writer, maker, base, quote,
    ));
    send(chain, payer, &instructions).await.map(Some)
}

async fn send(chain: &Chain, payer: &Keypair, instructions: &[Instruction]) -> Result<String> {
    let blockhash = chain
        .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
        .await?;
    let hash: Address = blockhash["value"]["blockhash"]
        .as_str()
        .context("missing blockhash")?
        .parse()?;
    let mut transaction = unsigned(&payer.pubkey(), hash.to_bytes(), instructions);
    transaction.try_sign(&[payer], hash.to_bytes().into())?;
    let signature = transaction.signatures[0].to_string();
    let bytes = wire(&transaction)?;
    anyhow::ensure!(
        bytes.len() <= 1232,
        "settlement packet exceeds legacy limit"
    );
    let result = chain
        .rpc(
            "sendTransaction",
            json!([
                STANDARD.encode(bytes), {"encoding":"base64","preflightCommitment":"confirmed"},
            ]),
        )
        .await?;
    anyhow::ensure!(
        result.as_str() == Some(&signature),
        "RPC returned an unexpected signature"
    );
    Ok(signature)
}
