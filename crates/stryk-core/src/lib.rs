#![no_std]

use core::cmp::Ordering;
use sha2::{Digest, Sha256};

pub const BASE_DECIMALS: u8 = 9;
pub const QUOTE_DECIMALS: u8 = 6;
pub const MAX_QUOTE_SECONDS: i64 = 45;
pub const OBSERVATION_SECONDS: i64 = 30;
pub const QUOTE_BYTES: usize = 349;
pub const SOL_USD_FEED: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    Call = 0,
    Put = 1,
}

impl TryFrom<u8> for Kind {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Call),
            1 => Ok(Self::Put),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum Outcome {
    Active = 0,
    Assigned = 1,
    Expired = 2,
    NoFix = 3,
}

pub fn strike_atoms(quantity: u64, strike: u64) -> Option<u64> {
    if quantity == 0 || strike == 0 {
        return None;
    }
    let notional = u128::from(quantity) * u128::from(strike);
    let scale = 10_u128.pow(u32::from(BASE_DECIMALS));
    (notional / scale + u128::from(notional % scale != 0))
        .try_into()
        .ok()
}

pub fn fill_time_is_valid(now: i64, expiry: i64, cutoff: u32, valid_until: i64) -> bool {
    let Some(fill_cutoff) = expiry.checked_sub(i64::from(cutoff)) else {
        return false;
    };
    now < valid_until
        && valid_until <= fill_cutoff
        && valid_until
            .checked_sub(now)
            .is_some_and(|remaining| remaining <= MAX_QUOTE_SECONDS)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Price {
    pub value: i64,
    pub confidence: u64,
    pub exponent: i32,
    pub published_at: i64,
    pub previous_at: i64,
}

impl Price {
    pub fn is_valid_fix(self, expiry: i64, now: i64, confidence_bps: u16) -> bool {
        self.value > 0
            && (-18..=0).contains(&self.exponent)
            && self.previous_at < expiry
            && expiry <= self.published_at
            && self.published_at <= expiry.saturating_add(OBSERVATION_SECONDS)
            && self.published_at <= now
            && u128::from(self.confidence) * 10_000
                <= self.value as u128 * u128::from(confidence_bps)
    }

    pub fn outcome(self, kind: Kind, strike: u64) -> Option<Outcome> {
        if self.value <= 0 || !(-18..=0).contains(&self.exponent) {
            return None;
        }
        let price_scale = 10_u128.pow(self.exponent.unsigned_abs());
        let quote_scale = 10_u128.pow(u32::from(QUOTE_DECIMALS));
        let cmp = (self.value as u128 * quote_scale).cmp(&(u128::from(strike) * price_scale));
        let assigned = matches!(
            (kind, cmp),
            (Kind::Call, Ordering::Greater) | (Kind::Put, Ordering::Less)
        );
        Some(if assigned {
            Outcome::Assigned
        } else {
            Outcome::Expired
        })
    }
}

/// Decode a fully verified Pyth Core PriceUpdateV2. The caller checks the account owner.
pub fn parse_price_update(data: &[u8], feed: &[u8; 32]) -> Option<(Price, u64)> {
    const DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];
    fn read<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
        data.get(offset..offset + N)?.try_into().ok()
    }

    if read::<8>(data, 0)? != DISCRIMINATOR || *data.get(40)? != 1 || read::<32>(data, 41)? != *feed
    {
        return None;
    }
    Some((
        Price {
            value: i64::from_le_bytes(read(data, 73)?),
            confidence: u64::from_le_bytes(read(data, 81)?),
            exponent: i32::from_le_bytes(read(data, 89)?),
            published_at: i64::from_le_bytes(read(data, 93)?),
            previous_at: i64::from_le_bytes(read(data, 101)?),
        },
        u64::from_le_bytes(read(data, 125)?),
    ))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Quote {
    pub domain: [u8; 32],
    pub program: [u8; 32],
    pub market: [u8; 32],
    pub expiry: [u8; 32],
    pub rfq_id: [u8; 16],
    pub writer: [u8; 32],
    pub maker: [u8; 32],
    pub lane: [u8; 32],
    pub base_mint: [u8; 32],
    pub quote_mint: [u8; 32],
    pub kind: Kind,
    pub strike: u64,
    pub quantity: u64,
    pub premium: u64,
    pub valid_until: i64,
}

impl Quote {
    pub fn encode(&self) -> [u8; QUOTE_BYTES] {
        let mut bytes = [0; QUOTE_BYTES];
        let mut offset = 0;
        for field in [
            b"STRYK_RFQ_V1".as_slice(),
            &self.domain,
            &self.program,
            &self.market,
            &self.expiry,
            &self.rfq_id,
            &self.writer,
            &self.maker,
            &self.lane,
            &self.base_mint,
            &self.quote_mint,
            &[self.kind as u8],
            &self.strike.to_le_bytes(),
            &self.quantity.to_le_bytes(),
            &self.premium.to_le_bytes(),
            &self.valid_until.to_le_bytes(),
        ] {
            bytes[offset..offset + field.len()].copy_from_slice(field);
            offset += field.len();
        }
        bytes
    }

    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_v1_has_stable_order_and_digest() {
        let quote = Quote {
            domain: [1; 32],
            program: [2; 32],
            market: [3; 32],
            expiry: [4; 32],
            rfq_id: [10; 16],
            writer: [5; 32],
            maker: [6; 32],
            lane: [7; 32],
            base_mint: [8; 32],
            quote_mint: [9; 32],
            kind: Kind::Put,
            strike: 150_000_000,
            quantity: 10_000_001,
            premium: 123_456,
            valid_until: -4,
        };
        assert_eq!(&quote.encode()[140..156], &[10; 16]);
        assert_eq!(&quote.encode()[317..325], &150_000_000_u64.to_le_bytes());
        assert_eq!(
            quote.digest(),
            [
                0x0c, 0xdc, 0x3f, 0xbd, 0x0a, 0x0a, 0xd4, 0x37, 0x4c, 0x2c, 0xa4, 0x33, 0x34, 0x18,
                0x29, 0x08, 0xd0, 0xaf, 0x76, 0xda, 0x4f, 0x38, 0xb8, 0x79, 0xc9, 0x5a, 0x1f, 0x8c,
                0x67, 0xab, 0x6e, 0xfa,
            ]
        );
    }

    #[test]
    fn strike_rounds_up_and_does_not_truncate() {
        assert_eq!(strike_atoms(10_000_001, 150_000_000), Some(1_500_001));
        assert_eq!(strike_atoms(1, 1), Some(1));
        assert_eq!(strike_atoms(u64::MAX, u64::MAX), None);
        assert_eq!(strike_atoms(0, 100), None);
    }

    #[test]
    fn quote_cutoff_is_exclusive() {
        assert!(fill_time_is_valid(115, 280, 120, 160));
        assert!(!fill_time_is_valid(114, 280, 120, 160));
        assert!(!fill_time_is_valid(160, 280, 120, 160));
        assert!(!fill_time_is_valid(159, 280, 120, 161));
    }

    #[test]
    fn fix_requires_first_observation_within_confidence_and_time_bounds() {
        let price = Price {
            value: 15_000_000_000,
            confidence: 150_000_000,
            exponent: -8,
            published_at: 130,
            previous_at: 99,
        };
        assert!(price.is_valid_fix(100, 130, 100));
        assert!(!price.is_valid_fix(100, 129, 100));
        assert!(!price.is_valid_fix(100, 130, 99));
        assert!(!Price {
            previous_at: 100,
            ..price
        }
        .is_valid_fix(100, 130, 100));
        assert!(!Price {
            published_at: 131,
            ..price
        }
        .is_valid_fix(100, 131, 100));
    }

    #[test]
    fn pyth_receiver_decodes_finalized_devnet_account() {
        let fixture = *include_bytes!("../tests/fixtures/sol-usd-devnet.bin");
        let (price, slot) = parse_price_update(&fixture, &SOL_USD_FEED).unwrap();
        assert_eq!(price.value, 8_031_105_824);
        assert_eq!(price.confidence, 2_347_266);
        assert_eq!(price.exponent, -8);
        assert_eq!(price.previous_at, 1_794_601_012);
        assert_eq!(price.published_at, 1_794_601_013);
        assert_eq!(slot, 521_941_745);
        assert!(price.is_valid_fix(price.published_at, price.published_at, 100));
        assert_eq!(parse_price_update(&fixture, &[0; 32]), None);
        assert_eq!(parse_price_update(&fixture[..132], &SOL_USD_FEED), None);
        let mut partial = fixture;
        partial[40] = 0;
        assert_eq!(parse_price_update(&partial, &SOL_USD_FEED), None);
    }

    #[test]
    fn settlement_compares_exact_oracle_value() {
        let price = Price {
            value: 15_000_000_000,
            confidence: 0,
            exponent: -8,
            published_at: 130,
            previous_at: 99,
        };
        for kind in [Kind::Call, Kind::Put] {
            assert_eq!(price.outcome(kind, 150_000_000), Some(Outcome::Expired));
        }
        let up = Price {
            value: price.value + 1,
            ..price
        };
        let down = Price {
            value: price.value - 1,
            ..price
        };
        assert_eq!(up.outcome(Kind::Call, 150_000_000), Some(Outcome::Assigned));
        assert_eq!(up.outcome(Kind::Put, 150_000_000), Some(Outcome::Expired));
        assert_eq!(
            down.outcome(Kind::Call, 150_000_000),
            Some(Outcome::Expired)
        );
        assert_eq!(
            down.outcome(Kind::Put, 150_000_000),
            Some(Outcome::Assigned)
        );
    }
}
