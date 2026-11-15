#![allow(dead_code)] // Account constraints consume fields that handlers don't read.

use crate::{state::*, StrykError, ID};
use quasar_lang::{pda::based_try_find_program_address, prelude::*, sysvars::Sysvar as _};
use quasar_spl::prelude::*;
use stryk_core::{BASE_DECIMALS, QUOTE_DECIMALS};

const NATIVE_MINT: Address = address!("So11111111111111111111111111111111111111112");
const LOADER: Address = address!("BPFLoaderUpgradeab1e11111111111111111111111");
const FRIDAY_0800_UTC: i64 = 86_400 + 8 * 3_600;
const WEEK: i64 = 7 * 86_400;

#[derive(Accounts)]
#[instruction(version: u16)]
pub struct InitializeMarket {
    #[account(mut)]
    pub admin: Signer,
    /// CHECK: Address, Loader v3 ownership, account variant, and upgrade authority are verified in initialize.
    pub program_data: UncheckedAccount,
    pub base_mint: Account<Mint>,
    pub quote_mint: Account<Mint>,
    #[account(init, payer = admin, address = Market::seeds(base_mint.address(), quote_mint.address(), version))]
    pub market: Account<Market>,
    pub system_program: Program<SystemProgram>,
    /// CHECK: The handler binds the configured address to an executable program.
    pub receiver_program: UncheckedAccount,
}

impl InitializeMarket {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        &mut self,
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
        let (program_data, _) = based_try_find_program_address(&[ID.as_ref()], &LOADER)?;
        require_keys_eq!(
            *self.program_data.address(),
            program_data,
            StrykError::InvalidMarket
        );
        let account = self.program_data.to_account_view();
        require!(account.owned_by(&LOADER), StrykError::InvalidMarket);
        let data = account.try_borrow()?;
        // Loader v3 ProgramData encodes variant 3, an eight-byte slot, then an optional authority.
        require!(
            data.get(..4) == Some(&3_u32.to_le_bytes())
                && data.get(12) == Some(&1)
                && data.get(13..45) == Some(self.admin.address().as_ref()),
            ProgramError::IncorrectAuthority
        );
        require!(
            self.receiver_program.address() == &receiver
                && self.receiver_program.to_account_view().executable(),
            StrykError::InvalidMarket
        );
        require!(
            *self.base_mint.address() == NATIVE_MINT
                && self.quote_mint.address() != &NATIVE_MINT
                && self.base_mint.decimals() == BASE_DECIMALS
                && self.quote_mint.decimals() == QUOTE_DECIMALS
                && version > 0
                && domain != SYSTEM_PROGRAM_ID
                && feed != SYSTEM_PROGRAM_ID
                && receiver != SYSTEM_PROGRAM_ID
                && strike_tick > 0
                && min_quantity > 0
                && min_quantity <= max_quantity
                && fill_cutoff > 0
                && fix_timeout > 30
                && (1..=10_000).contains(&confidence_bps),
            StrykError::InvalidMarket
        );
        self.market.set_inner(MarketInner {
            admin: *self.admin.address(),
            base_mint: *self.base_mint.address(),
            quote_mint: *self.quote_mint.address(),
            receiver,
            domain: domain.to_bytes(),
            feed: feed.to_bytes(),
            strike_tick,
            min_quantity,
            max_quantity,
            fill_cutoff,
            fix_timeout,
            confidence_bps,
            version,
            weekly: u8::from(weekly),
            paused: 0,
        });
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(expires_at: u64)]
pub struct InitializeExpiry {
    #[account(mut)]
    pub admin: Signer,
    #[account(has_one(admin))]
    pub market: Account<Market>,
    #[account(init, payer = admin, address = Expiry::seeds(market.address(), expires_at))]
    pub expiry: Account<Expiry>,
    pub system_program: Program<SystemProgram>,
}

impl InitializeExpiry {
    pub fn initialize(&mut self, expiry: i64) -> Result<(), ProgramError> {
        let now = Clock::get()?.unix_timestamp.get();
        require!(
            expiry > now.saturating_add(i64::from(self.market.fill_cutoff.get()))
                && (self.market.weekly == 0 || expiry.rem_euclid(WEEK) == FRIDAY_0800_UTC),
            StrykError::InvalidTerms
        );
        let fix_deadline = expiry
            .checked_add(i64::from(self.market.fix_timeout.get()))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        self.expiry.set_inner(ExpiryInner {
            market: *self.market.address(),
            proof: SYSTEM_PROGRAM_ID,
            expires_at: expiry,
            fix_deadline,
            price: 0,
            confidence: 0,
            exponent: 0,
            published_at: 0,
            previous_at: 0,
            posted_slot: 0,
            status: Expiry::OPEN,
        });
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(lane_id: u16)]
pub struct ConfigureLane {
    #[account(mut)]
    pub admin: Signer,
    #[account(has_one(admin))]
    pub market: Account<Market>,
    /// CHECK: The maker supplies a key only and can also be the market admin.
    #[account(dup)]
    pub maker: UncheckedAccount,
    #[account(init, payer = admin, address = MakerLane::seeds(market.address(), maker.address(), lane_id))]
    pub lane: Account<MakerLane>,
    pub system_program: Program<SystemProgram>,
}

impl ConfigureLane {
    pub fn initialize(&mut self, lane_id: u16, bump: u8) -> Result<(), ProgramError> {
        self.lane.set_inner(MakerLaneInner {
            market: *self.market.address(),
            maker: *self.maker.address(),
            lane_id,
            bump,
            enabled: 1,
        });
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeCapacity {
    #[account(mut)]
    pub admin: Signer,
    #[account(has_one(admin))]
    pub market: Account<Market>,
    #[account(has_one(market))]
    pub lane: Account<MakerLane>,
    #[account(has_one(market))]
    pub expiry: Account<Expiry>,
    #[account(init, payer = admin, address = Capacity::seeds(lane.address(), expiry.address()))]
    pub capacity: Account<Capacity>,
    pub system_program: Program<SystemProgram>,
}

impl InitializeCapacity {
    pub fn initialize(&mut self, limit: u64) -> Result<(), ProgramError> {
        require!(
            limit > 0 && self.expiry.expires_at.get() > Clock::get()?.unix_timestamp.get(),
            StrykError::InvalidTerms
        );
        self.capacity.set_inner(CapacityInner {
            lane: *self.lane.address(),
            expiry: *self.expiry.address(),
            limit,
            filled: 0,
        });
        Ok(())
    }
}

#[derive(Accounts)]
pub struct SetMarketPaused {
    pub admin: Signer,
    #[account(mut, has_one(admin))]
    pub market: Account<Market>,
}

#[derive(Accounts)]
pub struct SetLaneEnabled {
    pub admin: Signer,
    #[account(has_one(admin))]
    pub market: Account<Market>,
    #[account(mut, has_one(market))]
    pub lane: Account<MakerLane>,
}
