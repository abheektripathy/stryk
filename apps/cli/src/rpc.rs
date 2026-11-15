use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::{str::FromStr, thread, time::Duration};
use stryk_client::unsigned;

pub const FIXTURE_RECEIVER: Address =
    solana_address::address!("B7s3eyFtLSijY7bAx99kREdfpMK55MCr5r77j837TmCs");

pub fn settle(rpc: &Rpc, payer: &Keypair, position: Address) -> Result<String> {
    let position_data = rpc.stryk_account(position, 5)?;
    let market = address_at(&position_data, 1)?;
    let writer = address_at(&position_data, 33)?;
    let maker = address_at(&position_data, 65)?;
    let expiry = address_at(&position_data, 129)?;
    let market_data = rpc.stryk_account(market, 1)?;
    let base = address_at(&market_data, 33)?;
    let quote = address_at(&market_data, 65)?;
    let mut instructions = Vec::new();
    for (owner, mint) in [
        (writer, base),
        (writer, quote),
        (maker, base),
        (maker, quote),
    ] {
        if rpc.account(stryk_client::ata(&owner, &mint))?.is_none() {
            instructions.push(stryk_client::prepare_ata(payer.pubkey(), owner, mint));
        }
    }
    instructions.push(stryk_client::settle(
        market, position, expiry, writer, maker, base, quote,
    ));
    rpc.send(payer, &[], &instructions)
}

fn address_at(data: &[u8], offset: usize) -> Result<Address> {
    Ok(Address::new_from_array(
        data.get(offset..offset + 32)
            .context("truncated account")?
            .try_into()?,
    ))
}

pub struct Rpc {
    client: Client,
    url: String,
    pub cluster: &'static str,
}

impl Rpc {
    pub fn new(url: String) -> Result<Self> {
        let url = reqwest::Url::parse(&url)?;
        if !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
            bail!("the localnet smoke harness requires a loopback RPC");
        }
        Self::connect(url, "solana:localnet")
    }

    pub fn operator(url: &str, devnet: bool) -> Result<Self> {
        if !devnet {
            return Self::new(url.to_owned());
        }
        let url = reqwest::Url::parse(url)?;
        anyhow::ensure!(url.scheme() == "https", "devnet RPC requires HTTPS");
        let rpc = Self::connect(url, "solana:devnet")?;
        let genesis = rpc.call("getGenesisHash", json!([]))?;
        anyhow::ensure!(
            genesis.as_str() == Some(stryk_client::DEVNET_GENESIS),
            "RPC genesis hash does not match Solana devnet"
        );
        Ok(rpc)
    }

    fn connect(url: reqwest::Url, cluster: &'static str) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(15)).build()?,
            url: url.into(),
            cluster,
        })
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?
            .json()
            .map_err(reqwest::Error::without_url)?;
        if let Some(error) = response.get("error") {
            bail!("{method}: {error}");
        }
        Ok(response
            .get("result")
            .context("missing RPC result")?
            .clone())
    }

    pub fn latest(&self) -> Result<([u8; 32], u64)> {
        let result = self.call("getLatestBlockhash", json!([{"commitment":"confirmed"}]))?;
        let value = &result["value"];
        let blockhash = Address::from_str(value["blockhash"].as_str().context("blockhash")?)?;
        Ok((
            blockhash.to_bytes(),
            value["lastValidBlockHeight"]
                .as_u64()
                .context("block height")?,
        ))
    }

    pub fn send(
        &self,
        payer: &Keypair,
        extra: &[&Keypair],
        instructions: &[Instruction],
    ) -> Result<String> {
        let (hash, last_valid) = self
            .latest()
            .context("could not fetch a blockhash before signing")?;
        let mut tx = unsigned(&payer.pubkey(), hash, instructions);
        let signers: Vec<_> = std::iter::once(payer)
            .chain(extra.iter().copied())
            .collect();
        tx.try_sign(&signers, hash.into())?;
        let bytes = bincode::serialize(&tx)?;
        if bytes.len() > 1232 {
            bail!("legacy transaction is {} bytes", bytes.len());
        }
        let signature = tx.signatures[0].to_string();
        let result = self
            .call(
                "sendTransaction",
                json!([
                    STANDARD.encode(bytes),
                    {"encoding":"base64","preflightCommitment":"confirmed"}
                ]),
            )
            .map_err(|error| anyhow!("transaction {signature}: {error:#}"))?;
        if result.as_str() != Some(&signature) {
            bail!("transaction {signature}: RPC returned an unexpected signature");
        }
        for _ in 0..60 {
            let result = self
                .call("getSignatureStatuses", json!([[signature]]))
                .with_context(|| format!("confirmation unresolved for {signature}"))?;
            let status = &result["value"][0];
            if let Some(level) = status["confirmationStatus"].as_str() {
                if !status["err"].is_null() {
                    bail!("transaction {signature}: {}", status["err"]);
                }
                if matches!(level, "confirmed" | "finalized") {
                    return Ok(signature);
                }
            }
            let height = self
                .call("getBlockHeight", json!([{"commitment":"confirmed"}]))
                .with_context(|| format!("confirmation unresolved for {signature}"))?;
            if height.as_u64().is_some_and(|height| height > last_valid) {
                bail!("expired transaction {signature}");
            }
            thread::sleep(Duration::from_millis(250));
        }
        Err(anyhow!("confirmation unresolved for {signature}"))
    }

    pub fn account(&self, address: Address) -> Result<Option<Vec<u8>>> {
        Self::account_bytes(&self.account_value(address)?)
    }

    pub fn stryk_account(&self, address: Address, discriminator: u8) -> Result<Vec<u8>> {
        let account = self.account_value(address)?;
        anyhow::ensure!(
            account["owner"].as_str() == Some(&stryk_client::ID.to_string()),
            "account {address} is missing or owned by a different program"
        );
        let data = Self::account_bytes(&account)?.context("Stryk account missing")?;
        anyhow::ensure!(
            data.first() == Some(&discriminator),
            "unexpected Stryk account type at {address}"
        );
        Ok(data)
    }

    fn account_value(&self, address: Address) -> Result<Value> {
        let result = self.call(
            "getAccountInfo",
            json!([address.to_string(), {"encoding":"base64","commitment":"confirmed"}]),
        )?;
        result
            .get("value")
            .cloned()
            .context("RPC account value missing")
    }

    fn account_bytes(value: &Value) -> Result<Option<Vec<u8>>> {
        if value.is_null() {
            return Ok(None);
        }
        let data = value["data"][0].as_str().context("base64 account data")?;
        Ok(Some(STANDARD.decode(data)?))
    }

    pub fn measure(&self, label: &str, signature: &str, latency: Duration) -> Result<()> {
        for _ in 0..15 {
            let result = self.call(
                "getTransaction",
                json!([signature, {"commitment":"confirmed","encoding":"base64","maxSupportedTransactionVersion":0}]),
            )?;
            if result.is_null() {
                thread::sleep(Duration::from_millis(125));
                continue;
            }
            let bytes = STANDARD.decode(
                result["transaction"][0]
                    .as_str()
                    .context("transaction bytes")?,
            )?;
            println!(
                "{}",
                json!({
                    "label":label, "signature":signature, "bytes":bytes.len(),
                    "compute_units":result["meta"]["computeUnitsConsumed"], "fee_lamports":result["meta"]["fee"],
                    "confirmation_ms":latency.as_millis()
                })
            );
            return Ok(());
        }
        bail!("confirmed transaction unavailable: {signature}");
    }

    pub fn clock(&self) -> Result<i64> {
        let data = self
            .account(solana_address::address!(
                "SysvarC1ock11111111111111111111111111111111"
            ))?
            .context("Clock account missing")?;
        Ok(i64::from_le_bytes(
            data.get(32..40).context("Clock timestamp")?.try_into()?,
        ))
    }

    pub fn balance(&self, address: Address) -> Result<u64> {
        let data = self
            .account(address)?
            .with_context(|| format!("token account {address} missing"))?;
        Ok(u64::from_le_bytes(
            data.get(64..72).context("SPL token amount")?.try_into()?,
        ))
    }

    pub fn lamports(&self, address: Address) -> Result<u64> {
        let result = self.call(
            "getBalance",
            json!([address.to_string(), {"commitment":"confirmed"}]),
        )?;
        result["value"].as_u64().context("missing lamport balance")
    }
}
