#![no_std]

use quasar_lang::prelude::*;

mod authority;
mod liquidity;
pub mod state;
mod trade;
use authority::*;
use liquidity::*;
use trade::*;

declare_id!("76CMRtnnJCAA9udEy9Rdq1G6PcxLpbouZWYWxoqtE6VP");

#[program]
pub mod stryk {
    use super::*;

    #[instruction(discriminator = 1)]
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_market(
        ctx: Ctx<InitializeMarket>,
        version: u16,
        domain: Address,
        receiver: Address,
        feed: Address,
        strike_tick: u64,
        min_quantity: u64,
        max_quantity: u64,
        fill_cutoff: u32,
        fix_timeout: u32,
        confidence_bps: u16,
        weekly: bool,
    ) -> Result<(), ProgramError> {
        ctx.accounts.initialize(
            version,
            domain,
            receiver,
            feed,
            strike_tick,
            min_quantity,
            max_quantity,
            fill_cutoff,
            fix_timeout,
            confidence_bps,
            weekly,
        )
    }

    #[instruction(discriminator = 2)]
    pub fn initialize_expiry(
        ctx: Ctx<InitializeExpiry>,
        expires_at: u64,
    ) -> Result<(), ProgramError> {
        ctx.accounts
            .initialize(i64::try_from(expires_at).map_err(|_| StrykError::InvalidTerms)?)
    }

    #[instruction(discriminator = 3)]
    pub fn configure_lane(ctx: Ctx<ConfigureLane>, lane_id: u16) -> Result<(), ProgramError> {
        ctx.accounts.initialize(lane_id, ctx.bumps.lane)
    }

    #[instruction(discriminator = 4)]
    pub fn initialize_capacity(
        ctx: Ctx<InitializeCapacity>,
        limit: u64,
    ) -> Result<(), ProgramError> {
        ctx.accounts.initialize(limit)
    }

    #[instruction(discriminator = 5)]
    pub fn set_market_paused(ctx: Ctx<SetMarketPaused>, paused: bool) -> Result<(), ProgramError> {
        ctx.accounts.market.paused = u8::from(paused);
        Ok(())
    }

    #[instruction(discriminator = 6)]
    pub fn set_lane_enabled(ctx: Ctx<SetLaneEnabled>, enabled: bool) -> Result<(), ProgramError> {
        ctx.accounts.lane.enabled = u8::from(enabled);
        Ok(())
    }

    #[instruction(discriminator = 7)]
    pub fn deposit(ctx: Ctx<MoveInventory>, amount: u64) -> Result<(), ProgramError> {
        ctx.accounts.deposit(amount)
    }

    #[instruction(discriminator = 8)]
    pub fn withdraw(ctx: Ctx<MoveInventory>, amount: u64) -> Result<(), ProgramError> {
        ctx.accounts.withdraw(amount)
    }

    #[instruction(discriminator = 9)]
    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        ctx: Ctx<Fill>,
        rfq_lo: u64,
        rfq_hi: u64,
        kind: u8,
        strike: u64,
        quantity: u64,
        premium: u64,
        valid_until: i64,
    ) -> Result<(), ProgramError> {
        let mut rfq_id = [0; 16];
        rfq_id[..8].copy_from_slice(&rfq_lo.to_le_bytes());
        rfq_id[8..].copy_from_slice(&rfq_hi.to_le_bytes());
        ctx.accounts.fill(
            rfq_id,
            kind,
            strike,
            quantity,
            premium,
            valid_until,
            ctx.bumps.position,
        )
    }

    #[instruction(discriminator = 10)]
    pub fn fix_expiry(ctx: Ctx<FixExpiry>) -> Result<(), ProgramError> {
        ctx.accounts.fix()
    }

    #[instruction(discriminator = 11)]
    pub fn mark_no_fix(ctx: Ctx<MarkNoFix>) -> Result<(), ProgramError> {
        ctx.accounts.mark()
    }

    #[instruction(discriminator = 12)]
    pub fn settle_position(ctx: Ctx<SettlePosition>) -> Result<(), ProgramError> {
        ctx.accounts.settle()
    }
}

#[error_code]
pub enum StrykError {
    InvalidMarket,
    InvalidTerms,
    MarketPaused,
    LaneDisabled,
    CapacityExceeded,
    QuoteTime,
    QuoteSignature,
    InvalidPriceProof,
    FixWindow,
    FixUnavailable,
    AlreadySettled,
}
