use crate::rpc::Rpc;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::read_keypair_file;
use solana_signer::Signer;
use std::path::Path;
use stryk_client::{
    capacity, expiry, lane, market, program_data, ConfigureLaneInstruction,
    InitializeCapacityInstruction, InitializeExpiryInstruction, InitializeMarketInstruction,
    SetLaneEnabledInstruction, SetMarketPausedInstruction, SYSTEM, WSOL,
};

#[derive(Subcommand)]
pub enum Action {
    /// Create the SOL market. Use the program upgrade authority as the wallet.
    InitializeMarket {
        #[arg(long)]
        quote_mint: Address,
        #[arg(long)]
        receiver: Address,
        #[arg(long)]
        domain: Address,
        /// USDC atoms per SOL.
        #[arg(long, default_value_t = 1_000_000)]
        strike_tick: u64,
        /// WSOL atoms.
        #[arg(long, default_value_t = 1_000_000)]
        min_quantity: u64,
        /// WSOL atoms.
        #[arg(long, default_value_t = 10_000_000_000)]
        max_quantity: u64,
        #[arg(long, default_value_t = 120)]
        fill_cutoff: u32,
        /// Seconds after expiry allowed for a verified price fix.
        #[arg(long, default_value_t = 86_400)]
        fix_timeout: u32,
        #[arg(long, default_value_t = 100)]
        confidence_bps: u16,
        /// Set false to allow ad hoc expiry timestamps for a disposable market.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        weekly: bool,
    },
    /// Create an expiry. Weekly markets require Friday 08:00 UTC.
    CreateExpiry {
        #[arg(long)]
        market: Address,
        /// Unix timestamp in seconds.
        #[arg(long)]
        expires_at: i64,
    },
    /// Register a maker lane. The maker creates inventory ATAs on first deposit.
    RegisterLane {
        #[arg(long)]
        market: Address,
        #[arg(long)]
        maker: Address,
        #[arg(long, default_value_t = 0)]
        lane: u16,
    },
    /// Commit the immutable strike-notional budget for a lane and expiry.
    SetCapacity {
        #[arg(long)]
        market: Address,
        #[arg(long)]
        lane: Address,
        #[arg(long)]
        expiry: Address,
        /// Total strike notional in USDC atoms.
        #[arg(long)]
        limit: u64,
    },
    /// Pause fills or pass `--paused false` to resume.
    PauseMarket {
        #[arg(long)]
        market: Address,
        #[arg(long, action = clap::ArgAction::Set)]
        paused: bool,
    },
    /// Enable or disable a maker lane for new fills.
    SetLaneEnabled {
        #[arg(long)]
        market: Address,
        #[arg(long)]
        lane: Address,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
    },
}

pub fn run(rpc: &Rpc, wallet: &Path, action: Action) -> Result<()> {
    let wallet = read_keypair_file(wallet).map_err(|error| anyhow!("{error}"))?;
    let admin = wallet.pubkey();
    let (address, instruction): (Address, Instruction) = match action {
        Action::InitializeMarket {
            quote_mint,
            receiver,
            domain,
            strike_tick,
            min_quantity,
            max_quantity,
            fill_cutoff,
            fix_timeout,
            confidence_bps,
            weekly,
        } => {
            let market = market(&WSOL, &quote_mint, 1);
            (
                market,
                InitializeMarketInstruction {
                    admin,
                    program_data: program_data(),
                    base_mint: WSOL,
                    quote_mint,
                    market,
                    system_program: SYSTEM,
                    receiver_program: receiver,
                    version: 1,
                    domain,
                    receiver,
                    feed: Address::new_from_array(stryk_core::SOL_USD_FEED),
                    strike_tick,
                    min_quantity,
                    max_quantity,
                    fill_cutoff,
                    fix_timeout,
                    confidence_bps,
                    weekly,
                }
                .into(),
            )
        }
        Action::CreateExpiry { market, expires_at } => {
            let expiry = expiry(&market, expires_at);
            (
                expiry,
                InitializeExpiryInstruction {
                    admin,
                    market,
                    expiry,
                    system_program: SYSTEM,
                    expires_at: expires_at.try_into()?,
                }
                .into(),
            )
        }
        Action::RegisterLane {
            market,
            maker,
            lane: lane_id,
        } => {
            let lane = lane(&market, &maker, lane_id);
            (
                lane,
                ConfigureLaneInstruction {
                    admin,
                    market,
                    maker,
                    lane,
                    lane_id,
                    system_program: SYSTEM,
                }
                .into(),
            )
        }
        Action::SetCapacity {
            market,
            lane,
            expiry,
            limit,
        } => {
            let capacity = capacity(&lane, &expiry);
            (
                capacity,
                InitializeCapacityInstruction {
                    admin,
                    market,
                    lane,
                    expiry,
                    capacity,
                    limit,
                    system_program: SYSTEM,
                }
                .into(),
            )
        }
        Action::PauseMarket { market, paused } => (
            market,
            SetMarketPausedInstruction {
                admin,
                market,
                paused,
            }
            .into(),
        ),
        Action::SetLaneEnabled {
            market,
            lane,
            enabled,
        } => (
            lane,
            SetLaneEnabledInstruction {
                admin,
                market,
                lane,
                enabled,
            }
            .into(),
        ),
    };
    let signature = rpc.send(&wallet, &[], &[instruction])?;
    println!(
        "{}",
        serde_json::json!({"address": address.to_string(), "signature": signature})
    );
    Ok(())
}
