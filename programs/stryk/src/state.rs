use quasar_lang::prelude::*;
use stryk_core::Price;

#[account(discriminator = 1, set_inner)]
#[seeds(b"market", base_mint: Address, quote_mint: Address, version: u16)]
pub struct Market {
    pub admin: Address,
    pub base_mint: Address,
    pub quote_mint: Address,
    pub receiver: Address,
    pub domain: [u8; 32],
    pub feed: [u8; 32],
    pub strike_tick: u64,
    pub min_quantity: u64,
    pub max_quantity: u64,
    pub fill_cutoff: u32,
    pub fix_timeout: u32,
    pub confidence_bps: u16,
    pub version: u16,
    pub weekly: u8,
    pub paused: u8,
}

#[account(discriminator = 2, set_inner)]
#[seeds(b"expiry", market: Address, expiry: u64)]
pub struct Expiry {
    pub market: Address,
    pub proof: Address,
    pub expires_at: i64,
    pub fix_deadline: i64,
    pub price: i64,
    pub confidence: u64,
    pub exponent: i32,
    pub published_at: i64,
    pub previous_at: i64,
    pub posted_slot: u64,
    pub status: u8,
}

impl Expiry {
    pub const OPEN: u8 = 0;
    pub const FIXED: u8 = 1;
    pub const NO_FIX: u8 = 2;
}

pub fn fixed_price(expiry: &Account<Expiry>) -> Price {
    Price {
        value: expiry.price.get(),
        confidence: expiry.confidence.get(),
        exponent: expiry.exponent.get(),
        published_at: expiry.published_at.get(),
        previous_at: expiry.previous_at.get(),
    }
}

#[account(discriminator = 3, set_inner)]
#[seeds(b"maker", market: Address, maker: Address, lane_id: u16)]
pub struct MakerLane {
    pub market: Address,
    pub maker: Address,
    pub lane_id: u16,
    pub bump: u8,
    pub enabled: u8,
}

#[account(discriminator = 4, set_inner)]
#[seeds(b"cap", lane: Address, expiry: Address)]
pub struct Capacity {
    pub lane: Address,
    pub expiry: Address,
    pub limit: u64,
    pub filled: u64,
}

#[account(discriminator = 5, set_inner)]
#[seeds(b"position", market: Address, writer: Address, rfq_lo: u64, rfq_hi: u64)]
pub struct Position {
    pub market: Address,
    pub writer: Address,
    pub maker: Address,
    pub lane: Address,
    pub expiry: Address,
    pub rfq_id: [u8; 16],
    pub quote_hash: [u8; 32],
    pub strike: u64,
    pub quantity: u64,
    pub strike_amount: u64,
    pub premium: u64,
    pub opened_at: i64,
    pub settled_at: i64,
    pub kind: u8,
    pub bump: u8,
    pub outcome: u8,
}
