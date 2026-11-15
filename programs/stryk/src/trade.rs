use crate::{liquidity::lane_seeds, state::*, StrykError, ID};
use quasar_lang::{cpi::Seed, prelude::*, sysvars::Sysvar as _};
use quasar_spl::prelude::*;
use stryk_core::{fill_time_is_valid, parse_price_update, strike_atoms, Kind, Outcome, Quote};

const INSTRUCTIONS_ID: Address = address!("Sysvar1nstructions1111111111111111111111111");
const ED25519_ID: Address = address!("Ed25519SigVerify111111111111111111111111111");

#[derive(Accounts)]
#[instruction(rfq_lo: u64, rfq_hi: u64)]
pub struct Fill {
    #[account(mut)]
    pub writer: Signer,
    pub market: Account<Market>,
    #[account(has_one(market))]
    pub expiry: Account<Expiry>,
    #[account(has_one(market))]
    pub lane: Account<MakerLane>,
    #[account(mut, has_one(lane), has_one(expiry))]
    pub capacity: Account<Capacity>,
    #[account(init, payer = writer, address = Position::seeds(market.address(), writer.address(), rfq_lo, rfq_hi))]
    pub position: Account<Position>,
    pub base_mint: Account<Mint>,
    pub quote_mint: Account<Mint>,
    #[account(mut, associated_token(mint = base_mint, authority = writer, token_program = token_program))]
    pub writer_base: Account<Token>,
    #[account(mut, associated_token(mint = quote_mint, authority = writer, token_program = token_program))]
    pub writer_quote: Account<Token>,
    #[account(mut, associated_token(mint = base_mint, authority = lane, token_program = token_program))]
    pub lane_base: Account<Token>,
    #[account(mut, associated_token(mint = quote_mint, authority = lane, token_program = token_program))]
    pub lane_quote: Account<Token>,
    #[account(mut, init(idempotent), payer = writer,
        associated_token(mint = base_mint, authority = position, token_program = token_program, system_program = system_program, ata_program = ata_program))]
    pub escrow_base: Account<Token>,
    #[account(mut, init(idempotent), payer = writer,
        associated_token(mint = quote_mint, authority = position, token_program = token_program, system_program = system_program, ata_program = ata_program))]
    pub escrow_quote: Account<Token>,
    /// CHECK: The fixed runtime sysvar address binds the contents to this transaction.
    #[account(address = INSTRUCTIONS_ID)]
    pub instructions: UncheckedAccount,
    pub token_program: Program<TokenProgram>,
    pub system_program: Program<SystemProgram>,
    pub ata_program: Program<AssociatedTokenProgram>,
}

impl Fill {
    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        &mut self,
        rfq_id: [u8; 16],
        kind: u8,
        strike: u64,
        quantity: u64,
        premium: u64,
        valid_until: i64,
        bump: u8,
    ) -> Result<(), ProgramError> {
        require!(self.market.paused == 0, StrykError::MarketPaused);
        require!(self.lane.enabled != 0, StrykError::LaneDisabled);
        require!(
            self.base_mint.address() == &self.market.base_mint
                && self.quote_mint.address() == &self.market.quote_mint,
            StrykError::InvalidMarket
        );
        let kind = Kind::try_from(kind).map_err(|_| StrykError::InvalidTerms)?;
        require!(
            self.writer.address() != &self.lane.maker
                && (self.market.min_quantity.get()..=self.market.max_quantity.get())
                    .contains(&quantity)
                && strike.is_multiple_of(self.market.strike_tick.get())
                && premium > 0,
            StrykError::InvalidTerms
        );
        let now = Clock::get()?.unix_timestamp.get();
        require!(
            self.expiry.status == Expiry::OPEN
                && fill_time_is_valid(
                    now,
                    self.expiry.expires_at.get(),
                    self.market.fill_cutoff.get(),
                    valid_until
                ),
            StrykError::QuoteTime
        );
        let notional = strike_atoms(quantity, strike).ok_or(StrykError::InvalidTerms)?;
        let filled = self
            .capacity
            .filled
            .get()
            .checked_add(notional)
            .ok_or(StrykError::CapacityExceeded)?;
        require!(
            filled <= self.capacity.limit.get(),
            StrykError::CapacityExceeded
        );
        let quote = Quote {
            domain: self.market.domain,
            program: ID.to_bytes(),
            market: self.market.address().to_bytes(),
            expiry: self.expiry.address().to_bytes(),
            rfq_id,
            writer: self.writer.address().to_bytes(),
            maker: self.lane.maker.to_bytes(),
            lane: self.lane.address().to_bytes(),
            base_mint: self.base_mint.address().to_bytes(),
            quote_mint: self.quote_mint.address().to_bytes(),
            kind,
            strike,
            quantity,
            premium,
            valid_until,
        };
        let quote_hash = quote.digest();
        let instructions = self.instructions.to_account_view().try_borrow()?;
        require!(
            verified_quote(&instructions, &quote.maker, &quote_hash),
            StrykError::QuoteSignature
        );
        drop(instructions);

        let lane_id = self.lane.lane_id.get().to_le_bytes();
        let lane_bump = [self.lane.bump];
        let seeds = lane_seeds(
            self.market.address(),
            &self.lane.maker,
            &lane_id,
            &lane_bump,
        );
        let token = &self.token_program;
        let bd = self.base_mint.decimals();
        let qd = self.quote_mint.decimals();
        match kind {
            Kind::Call => {
                let needed = notional
                    .checked_add(premium)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                require!(
                    self.writer_base.amount() >= quantity && self.lane_quote.amount() >= needed,
                    ProgramError::InsufficientFunds
                );
                token
                    .transfer_checked(
                        &self.writer_base,
                        &self.base_mint,
                        &self.escrow_base,
                        &self.writer,
                        quantity,
                        bd,
                    )
                    .invoke()?;
                token
                    .transfer_checked(
                        &self.lane_quote,
                        &self.quote_mint,
                        &self.escrow_quote,
                        &self.lane,
                        notional,
                        qd,
                    )
                    .invoke_signed(&seeds)?;
            }
            Kind::Put => {
                require!(
                    self.writer_quote.amount() >= notional
                        && self.lane_base.amount() >= quantity
                        && self.lane_quote.amount() >= premium,
                    ProgramError::InsufficientFunds
                );
                token
                    .transfer_checked(
                        &self.writer_quote,
                        &self.quote_mint,
                        &self.escrow_quote,
                        &self.writer,
                        notional,
                        qd,
                    )
                    .invoke()?;
                token
                    .transfer_checked(
                        &self.lane_base,
                        &self.base_mint,
                        &self.escrow_base,
                        &self.lane,
                        quantity,
                        bd,
                    )
                    .invoke_signed(&seeds)?;
            }
        }
        token
            .transfer_checked(
                &self.lane_quote,
                &self.quote_mint,
                &self.writer_quote,
                &self.lane,
                premium,
                qd,
            )
            .invoke_signed(&seeds)?;
        self.capacity.filled.set(filled);
        self.position.set_inner(PositionInner {
            market: *self.market.address(),
            writer: *self.writer.address(),
            maker: self.lane.maker,
            lane: *self.lane.address(),
            expiry: *self.expiry.address(),
            rfq_id: quote.rfq_id,
            quote_hash,
            strike,
            quantity,
            strike_amount: notional,
            premium,
            opened_at: now,
            settled_at: 0,
            kind: kind as u8,
            bump,
            outcome: Outcome::Active as u8,
        });
        Ok(())
    }
}

fn read<const N: usize>(data: &[u8], at: usize) -> Option<[u8; N]> {
    data.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn instruction(data: &[u8], index: usize) -> Option<(&[u8], &[u8])> {
    let count = usize::from(u16::from_le_bytes(read(data, 0)?));
    if index >= count {
        return None;
    }
    let offset = usize::from(u16::from_le_bytes(read(data, 2 + 2 * index)?));
    let accounts = usize::from(u16::from_le_bytes(read(data, offset)?));
    let program_start = offset + 2 + 33 * accounts;
    let program_end = program_start + 32;
    let size = usize::from(u16::from_le_bytes(read(data, program_end)?));
    Some((
        data.get(program_start..program_end)?,
        data.get(program_end + 2..program_end + 2 + size)?,
    ))
}

fn verified_quote(data: &[u8], maker: &[u8; 32], digest: &[u8; 32]) -> bool {
    #[cfg(target_os = "solana")]
    if solana_instruction::syscalls::get_stack_height()
        != solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT
    {
        return false;
    }
    let Some(index) = data
        .len()
        .checked_sub(2)
        .and_then(|at| read(data, at))
        .map(u16::from_le_bytes)
        .map(usize::from)
    else {
        return false;
    };
    let Some(previous) = index.checked_sub(1).and_then(|idx| instruction(data, idx)) else {
        return false;
    };
    // Runtime validates the signature. Require the canonical SDK precompile layout and local offsets.
    const HEADER: [u8; 16] = [
        1, 0, 48, 0, 255, 255, 16, 0, 255, 255, 112, 0, 32, 0, 255, 255,
    ];
    previous.0 == ED25519_ID.as_ref()
        && previous.1.len() == 144
        && previous.1[..16] == HEADER
        && &previous.1[16..48] == maker
        && &previous.1[112..] == digest
        && instruction(data, index).is_some_and(|(program, _)| program == ID.as_ref())
}

#[derive(Accounts)]
pub struct FixExpiry {
    pub market: Account<Market>,
    #[account(mut, has_one(market))]
    pub expiry: Account<Expiry>,
    /// CHECK: The handler validates receiver ownership, schema, verification level, and feed.
    pub price_update: UncheckedAccount,
}

impl FixExpiry {
    pub fn fix(&mut self) -> Result<(), ProgramError> {
        let now = Clock::get()?.unix_timestamp.get();
        require!(
            self.expiry.status == Expiry::OPEN && now < self.expiry.fix_deadline.get(),
            StrykError::FixWindow
        );
        let account = self.price_update.to_account_view();
        require!(
            account.owned_by(&self.market.receiver),
            StrykError::InvalidPriceProof
        );
        let (price, slot) = parse_price_update(&account.try_borrow()?, &self.market.feed)
            .ok_or(StrykError::InvalidPriceProof)?;
        require!(
            price.is_valid_fix(
                self.expiry.expires_at.get(),
                now,
                self.market.confidence_bps.get()
            ),
            StrykError::InvalidPriceProof
        );
        self.expiry.proof = *self.price_update.address();
        self.expiry.price.set(price.value);
        self.expiry.confidence.set(price.confidence);
        self.expiry.exponent.set(price.exponent);
        self.expiry.published_at.set(price.published_at);
        self.expiry.previous_at.set(price.previous_at);
        self.expiry.posted_slot.set(slot);
        self.expiry.status = Expiry::FIXED;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct MarkNoFix {
    #[account(mut)]
    pub expiry: Account<Expiry>,
}

impl MarkNoFix {
    pub fn mark(&mut self) -> Result<(), ProgramError> {
        require!(
            self.expiry.status == Expiry::OPEN
                && Clock::get()?.unix_timestamp.get() >= self.expiry.fix_deadline.get(),
            StrykError::FixWindow
        );
        self.expiry.status = Expiry::NO_FIX;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct SettlePosition {
    pub market: Account<Market>,
    #[account(has_one(market))]
    pub expiry: Account<Expiry>,
    #[account(mut, has_one(market), has_one(expiry), has_one(writer), has_one(maker))]
    pub position: Account<Position>,
    /// CHECK: Position binds the writer and rent recipient.
    #[account(mut)]
    pub writer: UncheckedAccount,
    /// CHECK: Position binds the maker.
    pub maker: UncheckedAccount,
    pub base_mint: Account<Mint>,
    pub quote_mint: Account<Mint>,
    #[account(mut, associated_token(mint = base_mint, authority = writer, token_program = token_program))]
    pub writer_base: Account<Token>,
    #[account(mut, associated_token(mint = quote_mint, authority = writer, token_program = token_program))]
    pub writer_quote: Account<Token>,
    #[account(mut, associated_token(mint = base_mint, authority = maker, token_program = token_program))]
    pub maker_base: Account<Token>,
    #[account(mut, associated_token(mint = quote_mint, authority = maker, token_program = token_program))]
    pub maker_quote: Account<Token>,
    #[account(mut, associated_token(mint = base_mint, authority = position, token_program = token_program))]
    pub escrow_base: Account<Token>,
    #[account(mut, associated_token(mint = quote_mint, authority = position, token_program = token_program))]
    pub escrow_quote: Account<Token>,
    pub token_program: Program<TokenProgram>,
}

impl SettlePosition {
    pub fn settle(&mut self) -> Result<(), ProgramError> {
        require!(
            self.base_mint.address() == &self.market.base_mint
                && self.quote_mint.address() == &self.market.quote_mint,
            StrykError::InvalidMarket
        );
        require!(
            self.position.outcome == Outcome::Active as u8,
            StrykError::AlreadySettled
        );
        let now = Clock::get()?.unix_timestamp.get();
        require!(now >= self.expiry.expires_at.get(), StrykError::FixWindow);
        let kind = Kind::try_from(self.position.kind).map_err(|_| StrykError::InvalidTerms)?;
        let outcome = match self.expiry.status {
            Expiry::FIXED => fixed_price(&self.expiry)
                .outcome(kind, self.position.strike.get())
                .ok_or(StrykError::InvalidPriceProof)?,
            Expiry::NO_FIX => Outcome::NoFix,
            _ => return Err(StrykError::FixUnavailable.into()),
        };
        let bump = [self.position.bump];
        let seeds = [
            Seed::from(b"position" as &[u8]),
            Seed::from(self.market.address().as_ref()),
            Seed::from(self.writer.address().as_ref()),
            Seed::from(self.position.rfq_id.as_slice()),
            Seed::from(bump.as_slice()),
        ];
        let legs = match kind {
            Kind::Call => [
                (
                    &self.escrow_base,
                    &self.base_mint,
                    &self.writer_base,
                    &self.maker_base,
                    self.position.quantity.get(),
                ),
                (
                    &self.escrow_quote,
                    &self.quote_mint,
                    &self.maker_quote,
                    &self.writer_quote,
                    self.position.strike_amount.get(),
                ),
            ],
            Kind::Put => [
                (
                    &self.escrow_base,
                    &self.base_mint,
                    &self.maker_base,
                    &self.writer_base,
                    self.position.quantity.get(),
                ),
                (
                    &self.escrow_quote,
                    &self.quote_mint,
                    &self.writer_quote,
                    &self.maker_quote,
                    self.position.strike_amount.get(),
                ),
            ],
        };
        for (vault, mint, supplier, counterparty, booked) in legs {
            let surplus = vault
                .amount()
                .checked_sub(booked)
                .ok_or(ProgramError::InsufficientFunds)?;
            if outcome == Outcome::Assigned {
                self.token_program
                    .transfer_checked(
                        vault,
                        mint,
                        counterparty,
                        &self.position,
                        booked,
                        mint.decimals(),
                    )
                    .invoke_signed(&seeds)?;
                if surplus > 0 {
                    self.token_program
                        .transfer_checked(
                            vault,
                            mint,
                            supplier,
                            &self.position,
                            surplus,
                            mint.decimals(),
                        )
                        .invoke_signed(&seeds)?;
                }
            } else {
                self.token_program
                    .transfer_checked(
                        vault,
                        mint,
                        supplier,
                        &self.position,
                        vault.amount(),
                        mint.decimals(),
                    )
                    .invoke_signed(&seeds)?;
            }
            self.token_program
                .close_account(vault, &self.writer, &self.position)
                .invoke_signed(&seeds)?;
        }
        self.position.outcome = outcome as u8;
        self.position.settled_at.set(now);
        Ok(())
    }
}
