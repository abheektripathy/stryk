use serde::{Deserialize, Deserializer, Serialize, Serializer};
use solana_address::Address;
use std::{fmt::Display, str::FromStr};
use stryk_core::{Kind, Quote};

pub const VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Terms {
    pub version: u8,
    #[serde(with = "text")]
    pub market: Address,
    #[serde(with = "text")]
    pub expiry: Address,
    #[serde(with = "text")]
    pub writer: Address,
    pub kind: u8,
    #[serde(with = "text")]
    pub strike: u64,
    #[serde(with = "text")]
    pub quantity: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub terms: Terms,
    #[serde(with = "text")]
    pub id: u128,
    pub deadline_ms: i64,
    pub max_valid_until: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Bid {
    #[serde(with = "text")]
    pub id: u128,
    #[serde(with = "text")]
    pub maker: Address,
    pub lane_id: u16,
    #[serde(with = "text")]
    pub premium: u64,
    pub valid_until: i64,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Market {
    pub version: u8,
    #[serde(with = "text")]
    pub address: Address,
    #[serde(with = "text")]
    pub domain: Address,
    #[serde(with = "text")]
    pub base: Address,
    #[serde(with = "text")]
    pub quote: Address,
    #[serde(with = "text")]
    pub strike_tick: u64,
    #[serde(with = "text")]
    pub min_quantity: u64,
    #[serde(with = "text")]
    pub max_quantity: u64,
    pub fill_cutoff: u32,
    #[serde(default = "weekly_by_default")]
    pub weekly: bool,
    pub paused: bool,
    pub spot: Option<Spot>,
    #[serde(default)]
    pub indication: Option<Indication>,
    pub as_of_slot: u64,
    pub cluster: String,
}

fn weekly_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Spot {
    #[serde(with = "text")]
    pub price: u64,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Indication {
    pub premium_bps: u16,
    pub observed_at: i64,
    pub valid_until: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Expiry {
    #[serde(with = "text")]
    pub address: Address,
    pub expires_at: i64,
    pub fix_deadline: i64,
    #[serde(with = "text")]
    pub capacity_total: u128,
    #[serde(with = "text")]
    pub capacity_filled: u128,
    #[serde(default)]
    pub capacity_as_of_slot: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExpiryState {
    pub status: u8,
    pub fix: Option<Spot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Position {
    #[serde(with = "text")]
    pub address: Address,
    #[serde(with = "text")]
    pub writer: Address,
    #[serde(with = "text")]
    pub maker: Address,
    #[serde(with = "text")]
    pub expiry: Address,
    pub expires_at: i64,
    pub kind: u8,
    pub outcome: u8,
    #[serde(with = "text")]
    pub strike: u64,
    #[serde(with = "text")]
    pub quantity: u64,
    #[serde(with = "text")]
    pub strike_amount: u64,
    #[serde(with = "text")]
    pub premium: u64,
    pub opened_at: i64,
    pub settled_at: Option<i64>,
    pub opened_signature: Option<String>,
    pub settled_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_spot: Option<Spot>,
    pub as_of_slot: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Portfolio {
    pub positions: Vec<Position>,
    pub next_cursor: Option<String>,
    pub income_atoms: String,
    pub as_of_slot: u64,
    #[serde(default)]
    pub indexing_healthy: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IncomePoint {
    pub day: i64,
    pub premium: String,
}

#[derive(Serialize, Deserialize)]
pub struct MakerMessage<T> {
    pub version: u8,
    #[serde(flatten)]
    pub message: T,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Auction {
    Pending,
    NoQuote,
    Quoted {
        bid: Bid,
        inventory_slot: u64,
        #[serde(default)]
        spot: Option<Spot>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MakerInput {
    Authenticate { maker: String, signature: String },
    Subscribe { market: String },
    Quote { bid: Bid },
    Indication { lane_id: u16, premium_bps: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidRejection {
    WrongMaker,
    QueueFull,
    AuctionClosed,
    InvalidTerms,
    InvalidSignature,
    InventoryUnavailable,
    InsufficientInventoryOrCap,
    Outbid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MakerOutput {
    Challenge {
        challenge: String,
    },
    Ready,
    RequestIndication,
    Rfq {
        request: Request,
    },
    Acknowledged {
        id: String,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rejection: Option<BidRejection>,
    },
    Filled {
        position: Box<Position>,
    },
}

pub fn quote(terms: &Terms, market: &Market, bid: &Bid) -> Option<Quote> {
    if terms.version != VERSION || terms.market != market.address {
        return None;
    }
    Some(Quote {
        domain: market.domain.to_bytes(),
        program: crate::ID.to_bytes(),
        market: market.address.to_bytes(),
        expiry: terms.expiry.to_bytes(),
        rfq_id: bid.id.to_le_bytes(),
        writer: terms.writer.to_bytes(),
        maker: bid.maker.to_bytes(),
        lane: crate::lane(&market.address, &bid.maker, bid.lane_id).to_bytes(),
        base_mint: market.base.to_bytes(),
        quote_mint: market.quote.to_bytes(),
        kind: Kind::try_from(terms.kind).ok()?,
        strike: terms.strike,
        quantity: terms.quantity,
        premium: bid.premium,
        valid_until: bid.valid_until,
    })
}

mod text {
    use super::*;

    pub fn serialize<T: Display, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: FromStr + Display,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let value = text.parse::<T>().map_err(serde::de::Error::custom)?;
        if value.to_string() != text {
            return Err(serde::de::Error::custom("non-canonical value"));
        }
        Ok(value)
    }
}
