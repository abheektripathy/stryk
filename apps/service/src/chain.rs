use crate::ops::Operations;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use solana_address::{address, Address};
use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};
use stryk_client::{api, ata, capacity, lane, ID};
use stryk_core::{parse_price_update, strike_atoms, Kind, SOL_USD_FEED};

pub const PYTH_RECEIVER: Address = address!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
pub const PYTH_COMPAT_RECEIVER: Address = address!("rec2HHDDnjLfj4kE7VyEtFA1HPGQLK33259532cRyHp");
const SOL_USD_SHARD_0: Address = address!("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE");
const SOL_USD_COMPAT_SHARD_0: Address = address!("7AviUf9nL62mcxNbQGKm4nKDQnPjswo6c5MX4D57HmyE");
pub const MAX_SPOT_AGE: i64 = 120;

pub fn sol_usd_account(receiver: Address) -> Option<Address> {
    match receiver {
        PYTH_RECEIVER => Some(SOL_USD_SHARD_0),
        PYTH_COMPAT_RECEIVER => Some(SOL_USD_COMPAT_SHARD_0),
        _ => None,
    }
}

#[derive(Debug)]
pub struct RpcError(pub Value);

impl std::fmt::Display for RpcError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "RPC error {}", self.0)
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone)]
pub struct Chain {
    client: reqwest::Client,
    url: String,
    pub ops: Arc<Operations>,
}

pub struct Account {
    pub address: Address,
    pub owner: Address,
    pub data: Vec<u8>,
}

pub struct Inventory {
    slot: u64,
    base: u64,
    quote: u64,
    remaining: u64,
}

impl Inventory {
    pub fn eligible_slot(&self, terms: &api::Terms, premium: u64) -> Option<u64> {
        let notional = strike_atoms(terms.quantity, terms.strike)?;
        let collateral = match Kind::try_from(terms.kind).ok()? {
            Kind::Call => self.quote.checked_sub(premium)? >= notional,
            Kind::Put => self.quote >= premium && self.base >= terms.quantity,
        };
        (collateral && self.remaining >= notional).then_some(self.slot)
    }
}

impl Chain {
    pub fn new(url: String, ops: Arc<Operations>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(4))
                .build()?,
            url,
            ops,
        })
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.ops.rpc_rate_limited.fetch_add(1, Relaxed);
        }
        let value: Value = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?
            .json()
            .await
            .map_err(reqwest::Error::without_url)?;
        if let Some(error) = value.get("error") {
            return Err(RpcError(error.clone()).into());
        }
        Ok(value.get("result").context("missing RPC result")?.clone())
    }

    pub async fn accounts(&self, addresses: &[Address]) -> Result<(u64, Vec<Option<Account>>)> {
        self.read_accounts(addresses, "confirmed", None).await
    }

    pub async fn finalized_accounts(
        &self,
        addresses: &[Address],
        min_slot: u64,
    ) -> Result<(u64, Vec<Option<Account>>)> {
        self.read_accounts(addresses, "finalized", Some(min_slot))
            .await
    }

    async fn read_accounts(
        &self,
        addresses: &[Address],
        commitment: &str,
        min_slot: Option<u64>,
    ) -> Result<(u64, Vec<Option<Account>>)> {
        let addresses_text: Vec<_> = addresses.iter().map(ToString::to_string).collect();
        let mut config = json!({"encoding":"base64","commitment":commitment});
        if let Some(slot) = min_slot {
            config["minContextSlot"] = json!(slot);
        }
        let result = self
            .rpc("getMultipleAccounts", json!([addresses_text, config]))
            .await?;
        let slot = result["context"]["slot"]
            .as_u64()
            .context("missing RPC slot")?;
        let values = result["value"].as_array().context("missing RPC accounts")?;
        anyhow::ensure!(values.len() == addresses.len(), "wrong RPC account count");
        let accounts = addresses
            .iter()
            .zip(values)
            .map(|(address, value)| {
                if value.is_null() {
                    return Ok(None);
                }
                Ok(Some(decode(*address, value)?))
            })
            .collect::<Result<_>>()?;
        Ok((slot, accounts))
    }

    pub async fn program_accounts(&self, discriminator: u8) -> Result<(u64, Vec<Account>)> {
        self.filtered_accounts(discriminator, &[]).await
    }

    pub async fn filtered_accounts(
        &self,
        discriminator: u8,
        fields: &[(usize, &[u8])],
    ) -> Result<(u64, Vec<Account>)> {
        let filters: Vec<_> = std::iter::once((0, [discriminator].as_slice()))
            .chain(fields.iter().copied())
            .map(|(offset, bytes)| json!({"memcmp":{"offset":offset,"bytes":STANDARD.encode(bytes),"encoding":"base64"}}))
            .collect();
        let result = self
            .rpc(
                "getProgramAccounts",
                json!([
                    ID.to_string(), {
                        "encoding": "base64", "commitment":"finalized", "withContext":true,
                        "filters":filters
                    }
                ]),
            )
            .await?;
        let slot = result["context"]["slot"]
            .as_u64()
            .context("missing RPC slot")?;
        let values = result["value"].as_array().context("missing RPC accounts")?;
        let accounts = values
            .iter()
            .map(|value| {
                let address = value["pubkey"]
                    .as_str()
                    .context("account address")?
                    .parse()?;
                decode(address, &value["account"])
            })
            .collect::<Result<_>>()?;
        Ok((slot, accounts))
    }

    pub async fn market(&self, address: Address, cluster: String, now: i64) -> Result<api::Market> {
        let addresses = if cluster == "solana:localnet" {
            vec![address]
        } else {
            vec![address, SOL_USD_SHARD_0, SOL_USD_COMPAT_SHARD_0]
        };
        let (slot, accounts) = self.accounts(&addresses).await?;
        let account = accounts
            .first()
            .and_then(Option::as_ref)
            .context("market missing")?;
        let data = account.stryk(1)?;
        let receiver = Address::new_from_array(read(data, 97)?);
        let confidence_bps = u16::from_le_bytes(read(data, 225)?);
        let feed = read(data, 161)?;
        let spot_account = match receiver {
            PYTH_RECEIVER => accounts.get(1),
            PYTH_COMPAT_RECEIVER => accounts.get(2),
            _ => None,
        };
        let spot = spot_account
            .and_then(Option::as_ref)
            .filter(|account| account.owner == receiver && feed == SOL_USD_FEED)
            .and_then(|account| parse_price_update(&account.data, &feed))
            .and_then(|(price, _)| {
                if !price.is_valid_fix(price.published_at, now, confidence_bps)
                    || now.saturating_sub(price.published_at) > MAX_SPOT_AGE
                {
                    return None;
                }
                Some(api::Spot {
                    price: display_price(price.value, price.exponent)?,
                    observed_at: price.published_at,
                })
            });
        Ok(api::Market {
            version: api::VERSION,
            address,
            domain: Address::new_from_array(read(data, 129)?),
            base: Address::new_from_array(read(data, 33)?),
            quote: Address::new_from_array(read(data, 65)?),
            strike_tick: u64::from_le_bytes(read(data, 193)?),
            min_quantity: u64::from_le_bytes(read(data, 201)?),
            max_quantity: u64::from_le_bytes(read(data, 209)?),
            fill_cutoff: u32::from_le_bytes(read(data, 217)?),
            weekly: read::<1>(data, 229)?[0] != 0,
            paused: read::<1>(data, 230)?[0] != 0,
            spot,
            indication: None,
            as_of_slot: slot,
            cluster,
        })
    }

    pub async fn expiries(&self, market: Address) -> Result<Vec<api::Expiry>> {
        let (_, accounts) = self.filtered_accounts(2, &[(1, market.as_ref())]).await?;
        let (capacity_slot, capacities) = self.program_accounts(4).await?;
        let (lane_slot, lanes) = self
            .filtered_accounts(3, &[(1, market.as_ref()), (68, &[1])])
            .await?;
        let enabled: std::collections::HashSet<_> = lanes
            .iter()
            .filter_map(|lane| {
                let data = lane.stryk(3).ok()?;
                (data.get(1..33)? == market.as_ref() && data.get(68) == Some(&1))
                    .then_some(lane.address)
            })
            .collect();
        let mut totals = std::collections::HashMap::<Address, (u128, u128)>::new();
        for account in capacities {
            let data = account.stryk(4)?;
            if enabled.contains(&Address::new_from_array(read(data, 1)?)) {
                let entry = totals
                    .entry(Address::new_from_array(read(data, 33)?))
                    .or_default();
                entry.0 += u128::from(u64::from_le_bytes(read(data, 65)?));
                entry.1 += u128::from(u64::from_le_bytes(read(data, 73)?));
            }
        }
        let mut expiries = Vec::new();
        for account in accounts {
            let data = account.stryk(2)?;
            if data.get(1..33) != Some(market.as_ref()) {
                continue;
            }
            let (capacity_total, capacity_filled) =
                totals.remove(&account.address).unwrap_or_default();
            expiries.push(api::Expiry {
                address: account.address,
                expires_at: i64::from_le_bytes(read(data, 65)?),
                fix_deadline: i64::from_le_bytes(read(data, 73)?),
                capacity_total,
                capacity_filled,
                capacity_as_of_slot: capacity_slot.min(lane_slot),
            });
        }
        expiries.sort_unstable_by_key(|expiry| expiry.expires_at);
        Ok(expiries)
    }

    pub async fn open_expiries(
        &self,
        market: Address,
    ) -> Result<std::collections::HashMap<Address, i64>> {
        let (_, accounts) = self
            .filtered_accounts(2, &[(1, market.as_ref()), (125, &[0])])
            .await?;
        let mut expiries = std::collections::HashMap::new();
        for account in accounts {
            let data = account.stryk(2)?;
            if data.get(1..33) == Some(market.as_ref()) && data.last() == Some(&0) {
                expiries.insert(account.address, i64::from_le_bytes(read(data, 65)?));
            }
        }
        Ok(expiries)
    }

    pub async fn expiry_state(
        &self,
        expiry: Address,
        market: Address,
    ) -> Result<Option<api::ExpiryState>> {
        let (_, accounts) = self.finalized_accounts(&[expiry], 0).await?;
        let Some(account) = accounts.first().and_then(Option::as_ref) else {
            return Ok(None);
        };
        let data = account.stryk(2)?;
        if data.get(1..33) != Some(market.as_ref()) {
            return Ok(None);
        }
        let status = read::<1>(data, 125)?[0];
        let fix = if status == 1 {
            Some(api::Spot {
                price: display_price(
                    i64::from_le_bytes(read(data, 81)?),
                    i32::from_le_bytes(read(data, 97)?),
                )
                .context("fix out of range")?,
                observed_at: i64::from_le_bytes(read(data, 101)?),
            })
        } else {
            None
        };
        Ok(Some(api::ExpiryState { status, fix }))
    }

    pub async fn inventory(
        &self,
        market: &api::Market,
        expiry: Address,
        maker: Address,
        lane_id: u16,
    ) -> Result<Option<Inventory>> {
        let lane = lane(&market.address, &maker, lane_id);
        let (slot, accounts) = self
            .accounts(&[
                lane,
                capacity(&lane, &expiry),
                ata(&lane, &market.base),
                ata(&lane, &market.quote),
            ])
            .await?;
        let [Some(lane), Some(capacity), Some(base), Some(quote)] = accounts.as_slice() else {
            return Ok(None);
        };
        if lane.stryk(3)?.get(68) != Some(&1) {
            return Ok(None);
        }
        let capacity = capacity.stryk(4)?;
        let limit = u64::from_le_bytes(read(capacity, 65)?);
        let filled = u64::from_le_bytes(read(capacity, 73)?);
        Ok(Some(Inventory {
            slot,
            base: base.amount(&market.base)?,
            quote: quote.amount(&market.quote)?,
            remaining: limit.checked_sub(filled).context("capacity exceeded")?,
        }))
    }

    pub async fn registered(&self, market: Address, maker: Address, lane_id: u16) -> Result<bool> {
        let lane = lane(&market, &maker, lane_id);
        let (_, accounts) = self.accounts(&[lane]).await?;
        let Some(account) = accounts.first().and_then(Option::as_ref) else {
            return Ok(false);
        };
        let data = account.stryk(3)?;
        Ok(data.get(1..33) == Some(market.as_ref())
            && data.get(33..65) == Some(maker.as_ref())
            && data.get(68) == Some(&1))
    }
}

fn display_price(value: i64, exponent: i32) -> Option<u64> {
    if value <= 0 || !(-18..=0).contains(&exponent) {
        return None;
    }
    let scale = 10_u128.pow(exponent.unsigned_abs());
    let atoms = u64::try_from((value as u128 * 1_000_000) / scale).ok()?;
    (atoms > 0).then_some(atoms)
}

fn decode(address: Address, value: &Value) -> Result<Account> {
    Ok(Account {
        address,
        owner: value["owner"].as_str().context("account owner")?.parse()?,
        data: STANDARD.decode(value["data"][0].as_str().context("account data")?)?,
    })
}

pub(crate) fn read<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    Ok(data
        .get(offset..offset + N)
        .context("truncated account")?
        .try_into()?)
}

impl Account {
    pub(crate) fn stryk(&self, discriminator: u8) -> Result<&[u8]> {
        anyhow::ensure!(
            self.owner == ID && self.data.first() == Some(&discriminator),
            "unexpected Stryk account {}",
            self.address
        );
        Ok(&self.data)
    }

    fn amount(&self, mint: &Address) -> Result<u64> {
        anyhow::ensure!(
            self.owner == stryk_client::TOKEN && self.data.get(..32) == Some(mint.as_ref()),
            "unexpected token account {}",
            self.address
        );
        Ok(u64::from_le_bytes(read(&self.data, 64)?))
    }
}
