use crate::chain::{Account, Chain, PYTH_COMPAT_RECEIVER, PYTH_RECEIVER};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use solana_address::{address, Address};
use std::{path::PathBuf, process::Stdio, time::Duration};
use stryk_core::{parse_price_update, SOL_USD_FEED};

const WORMHOLE: Address = address!("HDwcJBJXjL9FpJ7UBsYBtaDjsBUhuLCUYoz3zr8SWWaQ");
const COMPAT_WORMHOLE: Address = address!("HDw2E7P8X1SkCyjvoGsfBGAVUutKcj874bXjHrpVYrVL");
const FEED: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

pub struct Historical {
    client: reqwest::Client,
    key: String,
    poster: PathBuf,
    payer: PathBuf,
    archive: PathBuf,
    receiver: Address,
    wormhole: Address,
}

#[derive(Deserialize)]
struct Update {
    binary: Binary,
    parsed: Vec<Observation>,
}

#[derive(Deserialize)]
struct Binary {
    data: Vec<String>,
    encoding: String,
}

#[derive(Deserialize)]
struct Observation {
    id: String,
    price: Price,
    metadata: Metadata,
}

#[derive(Deserialize)]
struct Price {
    publish_time: i64,
    price: String,
    conf: String,
    expo: i32,
}

#[derive(Deserialize)]
struct Metadata {
    prev_publish_time: i64,
}

impl Historical {
    pub fn new(
        key: String,
        poster: PathBuf,
        payer: PathBuf,
        archive: PathBuf,
        receiver: Address,
    ) -> Result<Self> {
        let wormhole = match receiver {
            PYTH_RECEIVER => WORMHOLE,
            PYTH_COMPAT_RECEIVER => COMPAT_WORMHOLE,
            _ => bail!("unsupported Pyth receiver {receiver}"),
        };
        std::fs::create_dir_all(&archive)?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            key,
            poster,
            payer,
            archive,
            receiver,
            wormhole,
        })
    }

    pub async fn post(
        &self,
        chain: &Chain,
        expiry: Address,
        time: i64,
        now: i64,
        confidence_bps: u16,
    ) -> Result<Address> {
        let receipt = self.archive.join(format!("{expiry}.address"));
        match std::fs::read_to_string(&receipt) {
            Ok(posted) => {
                let address = posted.trim().parse()?;
                if self
                    .verify(chain, address, time, now, confidence_bps)
                    .await?
                {
                    return Ok(address);
                }
                tracing::warn!(%expiry, %address, "archived Pyth proof account is missing");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let path = self.archive.join(format!("{expiry}.json"));
        let result: Value = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.client
                    .get(format!(
                        "https://pyth.dourolabs.app/hermes/v2/updates/price/{time}"
                    ))
                    .bearer_auth(&self.key)
                    .query(&[("ids[]", FEED), ("encoding", "base64"), ("parsed", "true")])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?
            }
            Err(error) => return Err(error.into()),
        };
        let update: Update = serde_json::from_value(result.clone())?;
        let [observation] = update.parsed.as_slice() else {
            bail!("expected one SOL/USD observation")
        };
        let price = stryk_core::Price {
            value: observation.price.price.parse()?,
            confidence: observation.price.conf.parse()?,
            exponent: observation.price.expo,
            published_at: observation.price.publish_time,
            previous_at: observation.metadata.prev_publish_time,
        };
        anyhow::ensure!(
            observation.id == FEED && price.is_valid_fix(time, now, confidence_bps),
            "historical observation fails expiry policy"
        );
        let [payload] = update.binary.data.as_slice() else {
            bail!("expected one signed price payload")
        };
        anyhow::ensure!(
            update.binary.encoding == "base64" && payload.len() <= 6_000,
            "unexpected price payload"
        );
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(&result)?)?;
        std::fs::rename(&temporary, &path)?;
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(&self.poster)
                .kill_on_drop(true)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .arg("--keypair")
                .arg(&self.payer)
                .args([
                    "--url",
                    "https://api.devnet.solana.com",
                    "--wormhole",
                    &self.wormhole.to_string(),
                    "post-price-update",
                    "--payload",
                    payload,
                ])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("Pyth poster timed out")??;
        anyhow::ensure!(
            output.status.success(),
            "Pyth poster exited with {}",
            output.status
        );
        let stdout = String::from_utf8(output.stdout)?;
        for line in stdout.lines() {
            if let Some(address) = line.strip_prefix("Price update posted to account: ") {
                let address: Address = address.trim().parse()?;
                let temporary = receipt.with_extension("tmp");
                std::fs::write(&temporary, address.to_string())?;
                std::fs::rename(&temporary, receipt)?;
                anyhow::ensure!(
                    self.verify(chain, address, time, now, confidence_bps)
                        .await?,
                    "posted account {address} is not visible on the settlement RPC"
                );
                return Ok(address);
            }
        }
        bail!("missing receiver account in Pyth poster output")
    }

    async fn verify(
        &self,
        chain: &Chain,
        address: Address,
        time: i64,
        now: i64,
        confidence_bps: u16,
    ) -> Result<bool> {
        let (_, accounts) = chain.accounts(&[address]).await?;
        let Some(account) = accounts.first().and_then(Option::as_ref) else {
            return Ok(false);
        };
        anyhow::ensure!(
            valid_proof(account, self.receiver, time, now, confidence_bps),
            "posted account {address} fails the market receiver or expiry policy"
        );
        Ok(true)
    }
}

fn valid_proof(account: &Account, receiver: Address, time: i64, now: i64, bps: u16) -> bool {
    account.owner == receiver
        && parse_price_update(&account.data, &SOL_USD_FEED)
            .is_some_and(|(price, _)| price.is_valid_fix(time, now, bps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_proof_must_still_match_the_market_and_expiry() {
        let mut account = Account {
            address: Address::default(),
            owner: PYTH_COMPAT_RECEIVER,
            data: include_bytes!("../../../crates/stryk-core/tests/fixtures/sol-usd-devnet.bin")
                .to_vec(),
        };
        let (price, _) = parse_price_update(&account.data, &SOL_USD_FEED).unwrap();
        let time = price.published_at;
        assert!(valid_proof(&account, PYTH_COMPAT_RECEIVER, time, time, 100));
        assert!(!valid_proof(&account, PYTH_RECEIVER, time, time, 100));
        assert!(!valid_proof(
            &account,
            PYTH_COMPAT_RECEIVER,
            time + 1,
            time + 1,
            100
        ));
        account.data[40] = 0;
        assert!(!valid_proof(
            &account,
            PYTH_COMPAT_RECEIVER,
            time,
            time,
            100
        ));
    }
}
