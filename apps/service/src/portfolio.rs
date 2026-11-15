use crate::chain::{read, Account, Chain, MAX_SPOT_AGE};
use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use solana_address::Address;
use sqlx::{
    postgres::{PgPoolOptions, PgRow},
    types::Json as SqlJson,
    PgPool, Postgres, Row, Transaction,
};
use std::{collections::HashMap, sync::atomic::Ordering::Relaxed, time::Duration};
use stryk_client::{
    api::{Portfolio, Position, Spot},
    ID,
};

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    market: Address,
}

#[derive(Deserialize)]
pub struct Page {
    owner: AddressString,
    cursor: Option<String>,
    history: Option<bool>,
}

#[derive(Deserialize)]
struct AddressString(String);

const SIGNATURE_PAGE: usize = 100;
const RECEIPT_BATCH: i64 = 24;
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(600);
const MAX_ENTRY_SPOT_AGE: i64 = MAX_SPOT_AGE + stryk_core::MAX_QUOTE_SECONDS + 5;

#[derive(Debug, Eq, PartialEq)]
struct Scan {
    committed: Option<String>,
    committed_slot: Option<i64>,
    head: Option<String>,
    head_slot: Option<i64>,
    before: Option<String>,
    ready: bool,
    archive: bool,
}

#[derive(Deserialize)]
struct SignatureRow {
    signature: String,
    slot: i64,
    err: Option<Value>,
}

struct Receipt {
    signature: String,
    instruction_index: i32,
    position: String,
    phase: i16,
}

impl Store {
    pub async fn connect(database_url: &str, market: Address) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO stryk_scan (market,slot) VALUES ($1,0) ON CONFLICT DO NOTHING")
            .bind(market.to_string())
            .execute(&pool)
            .await?;
        Ok(Self { pool, market })
    }

    pub async fn remember_quote_spot(
        &self,
        position: Address,
        valid_until: i64,
        spot: &Spot,
    ) -> Result<()> {
        let insert = sqlx::query(
            "INSERT INTO stryk_quote_spots (market,position,valid_until,spot)
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(self.market.to_string())
        .bind(position.to_string())
        .bind(valid_until)
        .bind(SqlJson(spot))
        .execute(&self.pool);
        tokio::time::timeout(Duration::from_millis(200), insert).await??;
        Ok(())
    }

    pub async fn run(
        self,
        chain: Chain,
        archive: Option<Chain>,
        fills: tokio::sync::broadcast::Sender<Position>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        let mut snapshot_due = tokio::time::Instant::now();
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if tokio::time::Instant::now() >= snapshot_due {
                match self.snapshot(&chain).await {
                    Ok(()) => snapshot_due = tokio::time::Instant::now() + SNAPSHOT_INTERVAL,
                    Err(error) => {
                        tracing::warn!(%error, "portfolio snapshot failed");
                        snapshot_due = tokio::time::Instant::now() + Duration::from_secs(15);
                    }
                }
            }
            let ingest = self.ingest(&chain, archive.as_ref(), &fills).await;
            if let Err(error) = &ingest {
                tracing::warn!(%error, "finalized receipt indexing stalled");
            }
            let healthy = ingest.is_ok();
            chain
                .ops
                .receipt_scan_healthy
                .store(u64::from(healthy), Relaxed);
            if let Err(error) = sqlx::query("UPDATE stryk_scan SET indexing_healthy=$2 WHERE market=$1 AND indexing_healthy IS DISTINCT FROM $2")
                .bind(self.market.to_string()).bind(healthy).execute(&self.pool).await {
                tracing::warn!(%error, "could not update portfolio health");
            }
        }
    }

    async fn snapshot(&self, chain: &Chain) -> Result<()> {
        let (slot, accounts) = chain
            .filtered_accounts(5, &[(1, self.market.as_ref())])
            .await?;
        chain.ops.finalized_slot.fetch_max(slot, Relaxed);
        let (_, expiries) = chain
            .filtered_accounts(2, &[(1, self.market.as_ref())])
            .await?;
        let expiries = expiry_times(expiries)?;
        let market = self.market.to_string();
        // shortcut: rebuild uses a full RPC snapshot, partition it if the provider caps this market's history.
        for batch in accounts.chunks(100) {
            let mut tx = self.pool.begin().await?;
            for account in batch {
                let position = decode_position(account, self.market, &expiries, slot)?;
                upsert(&mut tx, &market, position).await?;
            }
            tx.commit().await?;
        }
        sqlx::query("UPDATE stryk_scan SET slot=GREATEST(slot,$2) WHERE market=$1")
            .bind(&market)
            .bind(i64::try_from(slot)?)
            .execute(&self.pool)
            .await?;
        // Only prune after a finalized snapshot could have joined every expired candidate.
        let block_time = chain.rpc("getBlockTime", json!([slot])).await;
        if let Ok(Some(time)) = block_time.map(|value| value.as_i64()) {
            sqlx::query(
                "DELETE FROM stryk_quote_spots WHERE market=$1 AND position IN (
                    SELECT position FROM stryk_quote_spots
                    WHERE market=$1 AND valid_until < $2
                    ORDER BY valid_until,position LIMIT 100000
                )",
            )
            .bind(&market)
            .bind(time.saturating_sub(3_600))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn ingest(
        &self,
        chain: &Chain,
        archive: Option<&Chain>,
        fills: &tokio::sync::broadcast::Sender<Position>,
    ) -> Result<()> {
        let market = self.market.to_string();
        let row = sqlx::query("SELECT * FROM stryk_scan WHERE market=$1")
            .bind(&market)
            .fetch_one(&self.pool)
            .await?;
        let scan = Scan::from_row(&row)?;
        let checkpoint = scan.committed_slot.unwrap_or_default();
        chain
            .ops
            .receipt_scan_slot
            .fetch_max(u64::try_from(checkpoint)?, Relaxed);
        if scan.ready {
            let finalized = self.apply_batch(chain, archive, &market, scan).await?;
            for position in finalized {
                let _ = fills.send(position);
            }
            return Ok(());
        }

        let source = if scan.archive {
            archive.context("archival RPC required for pending scan")?
        } else {
            chain
        };
        if scan.before.is_none() {
            let finalized = chain
                .rpc("getSlot", json!([{"commitment":"finalized"}]))
                .await?
                .as_u64()
                .context("missing finalized slot")?;
            chain.ops.finalized_slot.fetch_max(finalized, Relaxed);
        }
        let mut options = json!({"limit":SIGNATURE_PAGE,"commitment":"finalized"});
        if let Some(before) = &scan.before {
            options["before"] = json!(before);
        }
        let page: Vec<SignatureRow> = serde_json::from_value(
            source
                .rpc("getSignaturesForAddress", json!([ID.to_string(), options]))
                .await?,
        )?;
        if let Some(newest) = page.first() {
            chain
                .ops
                .latest_signature_slot
                .fetch_max(u64::try_from(newest.slot)?, Relaxed);
        }
        let plan = page_boundary(&scan, &page)?;
        let mut tx = self.pool.begin().await?;
        lock_scan(&mut tx, &market, &scan).await?;
        if plan.gap {
            anyhow::ensure!(
                !scan.archive && archive.is_some(),
                "finalized checkpoint missing from available history; configure an archival RPC"
            );
            tracing::warn!(%market, "switching signature scan to archival RPC");
            sqlx::query("DELETE FROM stryk_signature_queue WHERE market=$1")
                .bind(&market)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE stryk_scan SET scan_head=NULL,scan_head_slot=NULL,scan_before=NULL,scan_archive=true WHERE market=$1")
                .bind(market).execute(&mut *tx).await?;
        } else if let Some(head) = plan.rows.first() {
            for row in plan.rows {
                sqlx::query("INSERT INTO stryk_signature_queue (market,signature,slot,succeeded) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                    .bind(&market).bind(&row.signature).bind(row.slot).bind(row.err.is_none()).execute(&mut *tx).await?;
            }
            sqlx::query("UPDATE stryk_scan SET scan_head=COALESCE(scan_head,$2),scan_head_slot=COALESCE(scan_head_slot,$3),scan_before=$4,scan_ready=$5 WHERE market=$1")
                .bind(market).bind(&head.signature).bind(head.slot).bind(&plan.rows.last().expect("nonempty page").signature).bind(plan.complete)
                .execute(&mut *tx).await?;
        } else if scan.head.is_some() && plan.complete {
            sqlx::query("UPDATE stryk_scan SET scan_ready=true WHERE market=$1")
                .bind(market)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn apply_batch(
        &self,
        chain: &Chain,
        archive: Option<&Chain>,
        market: &str,
        scan: Scan,
    ) -> Result<Vec<Position>> {
        let batch: Vec<(i64, String, i64, bool)> = sqlx::query_as(
            "SELECT id,signature,slot,succeeded FROM stryk_signature_queue WHERE market=$1 ORDER BY id DESC LIMIT $2"
        ).bind(market).bind(RECEIPT_BATCH).fetch_all(&self.pool).await?;
        let (_, cursor, slot, _) = batch.last().context("empty ready signature batch")?;
        let mut tasks = tokio::task::JoinSet::new();
        for (_, signature, slot, succeeded) in &batch {
            if *succeeded {
                let (chain, archive, market, signature) = (
                    chain.clone(),
                    archive.cloned(),
                    market.to_owned(),
                    signature.clone(),
                );
                let slot = *slot;
                tasks.spawn(async move {
                    transaction_receipts(&chain, archive.as_ref(), &market, &signature, slot).await
                });
            }
        }
        let mut receipts = Vec::new();
        while let Some(result) = tasks.join_next().await {
            receipts.extend(result??);
        }

        let mut positions = HashMap::new();
        let mut observed_slot = u64::try_from(*slot)?;
        if !receipts.is_empty() {
            let mut addresses = receipts
                .iter()
                .map(|receipt| receipt.position.parse::<Address>())
                .collect::<Result<Vec<_>, _>>()?;
            addresses.sort_unstable();
            addresses.dedup();
            for addresses in addresses.chunks(100) {
                let (observed, accounts) =
                    chain.finalized_accounts(addresses, observed_slot).await?;
                observed_slot = observed;
                let accounts = accounts
                    .into_iter()
                    .map(|account| account.context("finalized position missing"))
                    .collect::<Result<Vec<_>>>()?;
                let mut expiries = accounts
                    .iter()
                    .map(|account| read(account.stryk(5)?, 129).map(Address::new_from_array))
                    .collect::<Result<Vec<_>>>()?;
                expiries.sort_unstable();
                expiries.dedup();
                let (_, accounts_expiry) =
                    chain.finalized_accounts(&expiries, observed_slot).await?;
                let expiry_times = expiry_times(accounts_expiry.into_iter().flatten())?;
                for account in &accounts {
                    let position =
                        decode_position(account, self.market, &expiry_times, observed_slot)?;
                    positions.insert(position.address, position);
                }
            }
            for receipt in &receipts {
                let position = receipt.position.parse::<Address>()?;
                anyhow::ensure!(
                    positions
                        .get(&position)
                        .is_some_and(|row| receipt.phase == 0 || row.outcome != 0),
                    "receipt conflicts with finalized position"
                );
            }
        }

        let mut tx = self.pool.begin().await?;
        lock_scan(&mut tx, market, &scan).await?;
        let mut finalized = Vec::new();
        for receipt in &receipts {
            if receipt.phase == 0 {
                let key: Address = receipt.position.parse()?;
                if let Some(position) = positions.get(&key) {
                    let mut position = position.clone();
                    position.opened_signature = Some(receipt.signature.clone());
                    finalized.push(position);
                }
            }
        }
        for position in positions.into_values() {
            upsert(&mut tx, market, position).await?;
        }
        for receipt in receipts {
            sqlx::query("INSERT INTO stryk_receipts (market,signature,instruction_index,position,phase) VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                .bind(market).bind(receipt.signature).bind(receipt.instruction_index)
                .bind(receipt.position).bind(receipt.phase).execute(&mut *tx).await?;
        }
        let drained = Some(cursor) == scan.head.as_ref();
        if drained {
            anyhow::ensure!(Some(*slot) == scan.head_slot, "signature head slot changed");
        }
        sqlx::query("DELETE FROM stryk_signature_queue WHERE market=$1 AND id >= $2")
            .bind(market)
            .bind(batch.last().context("empty signature batch")?.0)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE stryk_scan SET signature=$2, signature_slot=$3, slot=GREATEST(slot,$4),
            scan_head=CASE WHEN $5 THEN NULL ELSE scan_head END,
            scan_head_slot=CASE WHEN $5 THEN NULL ELSE scan_head_slot END,
            scan_before=CASE WHEN $5 THEN NULL ELSE scan_before END,
            scan_ready=scan_ready AND NOT $5, scan_archive=scan_archive AND NOT $5 WHERE market=$1",
        )
        .bind(market)
        .bind(cursor)
        .bind(slot)
        .bind(i64::try_from(observed_slot)?)
        .bind(drained)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        chain
            .ops
            .receipt_scan_slot
            .fetch_max(u64::try_from(*slot)?, Relaxed);
        Ok(finalized)
    }
}

impl Scan {
    fn from_row(row: &PgRow) -> Result<Self> {
        Ok(Self {
            committed: row.try_get("signature")?,
            committed_slot: row.try_get("signature_slot")?,
            head: row.try_get("scan_head")?,
            head_slot: row.try_get("scan_head_slot")?,
            before: row.try_get("scan_before")?,
            ready: row.try_get("scan_ready")?,
            archive: row.try_get("scan_archive")?,
        })
    }
}

struct PageBoundary<'a> {
    rows: &'a [SignatureRow],
    complete: bool,
    gap: bool,
}

fn page_boundary<'a>(scan: &Scan, page: &'a [SignatureRow]) -> Result<PageBoundary<'a>> {
    anyhow::ensure!(
        page.len() <= SIGNATURE_PAGE,
        "RPC returned an oversized signature page"
    );
    anyhow::ensure!(
        page.windows(2).all(|pair| pair[0].slot >= pair[1].slot),
        "RPC signatures out of slot order"
    );
    anyhow::ensure!(
        !page
            .iter()
            .any(|row| Some(&row.signature) == scan.before.as_ref()),
        "RPC repeated the pagination anchor"
    );
    let anchor = page
        .iter()
        .position(|row| Some(&row.signature) == scan.committed.as_ref());
    let complete = anchor.is_some() || (scan.committed.is_none() && page.len() < SIGNATURE_PAGE);
    let older_than_anchor = page
        .last()
        .is_some_and(|row| scan.committed_slot.is_some_and(|slot| row.slot < slot));
    Ok(PageBoundary {
        rows: &page[..anchor.unwrap_or(page.len())],
        complete,
        gap: !complete && (page.len() < SIGNATURE_PAGE || older_than_anchor),
    })
}

async fn lock_scan(
    tx: &mut Transaction<'_, Postgres>,
    market: &str,
    expected: &Scan,
) -> Result<()> {
    let row = sqlx::query("SELECT * FROM stryk_scan WHERE market=$1 FOR UPDATE")
        .bind(market)
        .fetch_one(&mut **tx)
        .await?;
    anyhow::ensure!(
        Scan::from_row(&row)? == *expected,
        "signature scan advanced concurrently"
    );
    Ok(())
}

async fn transaction_receipts(
    chain: &Chain,
    archive: Option<&Chain>,
    market: &str,
    signature: &str,
    slot: i64,
) -> Result<Vec<Receipt>> {
    let params = json!([signature, {"encoding":"json","commitment":"finalized","maxSupportedTransactionVersion":0}]);
    let primary = chain.rpc("getTransaction", params.clone()).await;
    let result = match primary {
        Ok(value) if !value.is_null() => value,
        _ if archive.is_some() => {
            archive
                .expect("checked archive")
                .rpc("getTransaction", params)
                .await?
        }
        other => other?,
    };
    anyhow::ensure!(
        !result.is_null(),
        "finalized transaction {signature} absent from RPC"
    );
    anyhow::ensure!(
        result["slot"].as_i64() == Some(slot)
            && result["transaction"]["signatures"][0].as_str() == Some(signature),
        "finalized transaction differs from the indexed signature and slot"
    );
    anyhow::ensure!(
        result["meta"].get("err").is_some_and(Value::is_null),
        "unexpected finalized transaction status"
    );
    let message = &result["transaction"]["message"];
    let static_keys = message["accountKeys"]
        .as_array()
        .context("missing finalized transaction keys")?;
    let loaded = &result["meta"]["loadedAddresses"];
    let keys = static_keys
        .iter()
        .chain(loaded["writable"].as_array().into_iter().flatten())
        .chain(loaded["readonly"].as_array().into_iter().flatten())
        .map(|key| key.as_str().context("invalid transaction account key"))
        .collect::<Result<Vec<_>>>()?;
    let instructions = message["instructions"]
        .as_array()
        .context("missing finalized instructions")?;
    let mut receipts = Vec::new();
    let mut collect = |instruction, index: i32| -> Result<()> {
        if let Some((position, phase)) = receipt(instruction, &keys, market)? {
            receipts.push(Receipt {
                signature: signature.to_owned(),
                instruction_index: index,
                position: position.to_owned(),
                phase,
            });
        }
        Ok(())
    };
    for (index, instruction) in instructions.iter().enumerate() {
        collect(instruction, i32::try_from(index)?)?;
    }
    if let Some(groups) = result["meta"]["innerInstructions"].as_array() {
        for group in groups {
            let parent = group["index"]
                .as_u64()
                .context("missing outer instruction index")?;
            let nested = group["instructions"]
                .as_array()
                .context("missing inner instructions")?;
            anyhow::ensure!(
                parent < instructions.len() as u64 && nested.len() < 65_536,
                "invalid nested instruction indices"
            );
            for (index, instruction) in nested.iter().enumerate() {
                let index = ((parent + 1) << 16) + index as u64;
                collect(instruction, i32::try_from(index)?)?;
            }
        }
    }
    Ok(receipts)
}

fn expiry_times(accounts: impl IntoIterator<Item = Account>) -> Result<HashMap<Address, i64>> {
    accounts
        .into_iter()
        .map(|account| {
            Ok((
                account.address,
                i64::from_le_bytes(read(account.stryk(2)?, 65)?),
            ))
        })
        .collect()
}

fn decode_position(
    account: &Account,
    market: Address,
    expiries: &HashMap<Address, i64>,
    slot: u64,
) -> Result<Position> {
    let data = account.stryk(5)?;
    anyhow::ensure!(
        data.get(1..33) == Some(market.as_ref()),
        "position belongs to another market"
    );
    let expiry = Address::new_from_array(read(data, 129)?);
    let settled_at = i64::from_le_bytes(read(data, 249)?);
    Ok(Position {
        address: account.address,
        writer: Address::new_from_array(read(data, 33)?),
        maker: Address::new_from_array(read(data, 65)?),
        expiry,
        expires_at: *expiries.get(&expiry).context("position expiry missing")?,
        kind: read::<1>(data, 257)?[0],
        outcome: read::<1>(data, 259)?[0],
        strike: u64::from_le_bytes(read(data, 209)?),
        quantity: u64::from_le_bytes(read(data, 217)?),
        strike_amount: u64::from_le_bytes(read(data, 225)?),
        premium: u64::from_le_bytes(read(data, 233)?),
        opened_at: i64::from_le_bytes(read(data, 241)?),
        settled_at: (settled_at != 0).then_some(settled_at),
        opened_signature: None,
        settled_signature: None,
        entry_spot: None,
        as_of_slot: slot,
    })
}

async fn upsert(
    tx: &mut Transaction<'_, Postgres>,
    market: &str,
    position: Position,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO stryk_positions (market,address,writer,maker,opened_at,slot,record,entry_spot)
        VALUES ($1,$2,$3,$4,$5,$6,$7,(
            SELECT spot FROM stryk_quote_spots WHERE market=$1 AND position=$2
                AND valid_until > $5 AND (spot->>'observed_at')::bigint BETWEEN $5 - $8 AND $5
        )) ON CONFLICT (market,address) DO UPDATE
            SET slot=EXCLUDED.slot, record=EXCLUDED.record,
                entry_spot=COALESCE(stryk_positions.entry_spot,EXCLUDED.entry_spot)
        WHERE stryk_positions.slot <= EXCLUDED.slot AND (
            (stryk_positions.record->>'outcome') IS DISTINCT FROM (EXCLUDED.record->>'outcome')
            OR (stryk_positions.entry_spot IS NULL AND EXCLUDED.entry_spot IS NOT NULL)
        )",
    )
    .bind(market)
    .bind(position.address.to_string())
    .bind(position.writer.to_string())
    .bind(position.maker.to_string())
    .bind(position.opened_at)
    .bind(i64::try_from(position.as_of_slot)?)
    .bind(SqlJson(position))
    .bind(MAX_ENTRY_SPOT_AGE)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn receipt<'a>(
    instruction: &Value,
    keys: &[&'a str],
    market: &str,
) -> Result<Option<(&'a str, i16)>> {
    let key = |value: &Value| {
        value
            .as_u64()
            .and_then(|index| keys.get(index as usize))
            .copied()
    };
    if key(&instruction["programIdIndex"]) != Some(&ID.to_string()) {
        return Ok(None);
    }
    let data = bs58::decode(
        instruction["data"]
            .as_str()
            .context("missing instruction data")?,
    )
    .into_vec()?;
    let (phase, market_index, position_index) = match data.first() {
        Some(9) => (0, 1, 5),
        Some(12) => (1, 0, 2),
        _ => return Ok(None),
    };
    if key(&instruction["accounts"][market_index]) != Some(market) {
        return Ok(None);
    }
    Ok(key(&instruction["accounts"][position_index]).map(|address| (address, phase)))
}

const ROWS: &str = "SELECT p.record,p.entry_spot,
    (SELECT signature FROM stryk_receipts r WHERE r.market=p.market AND r.position=p.address AND r.phase=0 LIMIT 1) opening,
    (SELECT signature FROM stryk_receipts r WHERE r.market=p.market AND r.position=p.address AND r.phase=1 LIMIT 1) closing
    FROM stryk_positions p";

pub async fn income(
    State(store): State<Store>,
    Path(owner): Path<String>,
) -> Result<Json<Vec<stryk_client::api::IncomePoint>>, StatusCode> {
    let owner: Address = owner.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let rows = sqlx::query(
        "SELECT opened_at / 86400 * 86400 AS day,
                SUM((record->>'premium')::numeric)::text AS premium
         FROM stryk_positions WHERE market=$1 AND writer=$2
           AND opened_at >= (EXTRACT(EPOCH FROM NOW())::bigint / 86400 - 29) * 86400
         GROUP BY day ORDER BY day",
    )
    .bind(store.market.to_string())
    .bind(owner.to_string())
    .fetch_all(&store.pool)
    .await
    .map_err(server_error)?;
    rows.into_iter()
        .map(|row| {
            Ok(stryk_client::api::IncomePoint {
                day: row.try_get("day")?,
                premium: row.try_get("premium")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map(Json)
        .map_err(server_error)
}

fn decode_row(row: sqlx::postgres::PgRow) -> Result<Position> {
    let SqlJson(mut position): SqlJson<Position> = row.try_get("record")?;
    position.opened_signature = row.try_get("opening")?;
    position.settled_signature = row.try_get("closing")?;
    position.entry_spot = row
        .try_get::<Option<SqlJson<Spot>>, _>("entry_spot")?
        .map(|spot| spot.0);
    Ok(position)
}

pub async fn positions(
    State(store): State<Store>,
    Query(page): Query<Page>,
) -> Result<Json<Portfolio>, StatusCode> {
    let owner: Address = page.owner.0.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let (opened_at, address) = match page.cursor.as_deref() {
        Some(cursor) => {
            let (time, address) = cursor.split_once(':').ok_or(StatusCode::BAD_REQUEST)?;
            (
                time.parse::<i64>().map_err(|_| StatusCode::BAD_REQUEST)?,
                address
                    .parse::<Address>()
                    .map_err(|_| StatusCode::BAD_REQUEST)?
                    .to_string(),
            )
        }
        None => (i64::MAX, String::new()),
    };
    let market = store.market.to_string();
    let outcome = match page.history {
        Some(true) => "AND p.record->>'outcome' <> '0'",
        Some(false) => "AND p.record->>'outcome' = '0'",
        None => "",
    };
    let query = format!("{ROWS} WHERE p.market=$1 AND (p.writer=$2 OR p.maker=$2) AND (p.opened_at,p.address)<($3,$4) {outcome} ORDER BY p.opened_at DESC,p.address DESC LIMIT 51");
    let mut tx = store.pool.begin().await.map_err(server_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(server_error)?;
    let rows = sqlx::query(&query)
        .bind(&market)
        .bind(owner.to_string())
        .bind(opened_at)
        .bind(address)
        .fetch_all(&mut *tx)
        .await
        .map_err(server_error)?;
    let mut positions = rows
        .into_iter()
        .map(decode_row)
        .collect::<Result<Vec<_>>>()
        .map_err(internal_error)?;
    let next_cursor = if positions.len() > 50 {
        positions.truncate(50);
        positions
            .last()
            .map(|position| format!("{}:{}", position.opened_at, position.address))
    } else {
        None
    };
    let income_atoms: String = sqlx::query_scalar(
        "SELECT COALESCE(SUM((record->>'premium')::numeric),0)::text FROM stryk_positions WHERE market=$1 AND writer=$2"
    ).bind(&market).bind(owner.to_string()).fetch_one(&mut *tx).await.map_err(server_error)?;
    let scan: (i64, bool) =
        sqlx::query_as("SELECT slot,indexing_healthy AND slot > 0 FROM stryk_scan WHERE market=$1")
            .bind(market)
            .fetch_one(&mut *tx)
            .await
            .map_err(server_error)?;
    tx.commit().await.map_err(server_error)?;
    Ok(Json(Portfolio {
        positions,
        next_cursor,
        income_atoms,
        as_of_slot: scan.0 as u64,
        indexing_healthy: scan.1,
    }))
}

pub async fn position(
    State(store): State<Store>,
    Path(address): Path<String>,
) -> Result<Json<Position>, StatusCode> {
    let address = address
        .parse::<Address>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let query = format!("{ROWS} WHERE p.market=$1 AND p.address=$2");
    let row = sqlx::query(&query)
        .bind(store.market.to_string())
        .bind(address.to_string())
        .fetch_optional(&store.pool)
        .await
        .map_err(server_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(decode_row(row).map_err(internal_error)?))
}

fn server_error(error: sqlx::Error) -> StatusCode {
    internal_error(error.into())
}

fn internal_error(error: anyhow::Error) -> StatusCode {
    tracing::warn!(%error, "portfolio read failed");
    StatusCode::SERVICE_UNAVAILABLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_paging_anchors_on_signature_within_a_slot() {
        let mut scan = Scan {
            committed: Some("previous".to_owned()),
            committed_slot: Some(100),
            head: None,
            head_slot: None,
            before: None,
            ready: false,
            archive: false,
        };
        let row = |signature: &str, slot| SignatureRow {
            signature: signature.to_owned(),
            slot,
            err: None,
        };
        let page = vec![
            row("first", 101),
            row("same-slot", 100),
            row("previous", 100),
            row("older", 99),
        ];
        let boundary = page_boundary(&scan, &page).unwrap();
        assert_eq!(boundary.rows.len(), 2);
        assert!(boundary.complete && !boundary.gap);

        let mut page = Vec::new();
        for number in 0..SIGNATURE_PAGE {
            page.push(row(&number.to_string(), 100));
        }
        let boundary = page_boundary(&scan, &page).unwrap();
        assert_eq!(boundary.rows.len(), SIGNATURE_PAGE);
        assert!(!boundary.complete && !boundary.gap);
        scan.before = page.last().map(|row| row.signature.clone());
        assert!(page_boundary(&scan, &page).is_err());
        assert!(page_boundary(&scan, &[row("older", 99)]).unwrap().gap);
        scan.committed = None;
        scan.committed_slot = None;
        assert!(page_boundary(&scan, &[]).unwrap().complete);
    }
}
