mod admin;
mod load;
mod maker;
mod rpc;
mod smoke;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use solana_address::Address;
use solana_keypair::read_keypair_file;
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:18899")]
    rpc: String,
    /// Allow operator commands against a verified Solana devnet RPC.
    #[arg(long)]
    devnet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Exercise both assignments on a disposable local validator.
    Smoke {
        #[arg(long)]
        admin: PathBuf,
        #[arg(long)]
        fixture_receiver: Address,
        #[arg(long)]
        timeout: bool,
        #[arg(long, conflicts_with = "timeout")]
        race: bool,
    },
    /// Initialize a local market with three funded makers and one writer.
    Demo {
        #[arg(long)]
        admin: PathBuf,
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value_t = 300)]
        expiry_seconds: i64,
    },
    /// Initialize markets, expiries and maker capacity.
    Admin {
        #[arg(long)]
        wallet: PathBuf,
        #[command(subcommand)]
        action: admin::Action,
    },
    /// Quote deterministic premiums for an initialized maker lane.
    Maker {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        service: String,
        #[arg(long, default_value = "stryk-localnet")]
        audience: String,
        #[arg(long)]
        market: Address,
        #[arg(long)]
        domain: Address,
        #[arg(long, default_value_t = 0)]
        lane: u16,
        #[arg(long, default_value_t = 150)]
        premium_bps: u16,
    },
    /// Deposit or withdraw free maker inventory. Amount is in mint atoms.
    Inventory {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long)]
        market: Address,
        #[arg(long)]
        mint: Address,
        #[arg(long, default_value_t = 0)]
        lane: u16,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        deposit: bool,
    },
    /// Inspect free and locked inventory and the current expiry cap.
    Inspect {
        #[arg(long)]
        maker: Address,
        #[arg(long)]
        market: Address,
        #[arg(long)]
        expiry: Address,
        #[arg(long, default_value_t = 0)]
        lane: u16,
    },
    /// Accept and fill an RFQ with a disposable localnet or devnet wallet.
    Take {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        service: String,
        #[arg(long)]
        expiry: Address,
        #[arg(long)]
        kind: u8,
        #[arg(long)]
        strike: u64,
        #[arg(long)]
        quantity: u64,
    },
    /// Measure the auction on a disposable local service with funded makers.
    Load {
        #[arg(long)]
        service: String,
        #[arg(long)]
        expiry: Address,
        #[arg(long)]
        writer: Address,
        #[arg(long)]
        winner: Address,
        #[arg(long, default_value_t = 100)]
        rate: u64,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        overload: bool,
    },
    /// Publish an existing receiver account, without choosing the expiry price.
    Fix {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long)]
        market: Address,
        #[arg(long)]
        expiry: Address,
        #[arg(long)]
        proof: Address,
    },
    /// Mark an expiry after the fix deadline.
    NoFix {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long)]
        expiry: Address,
    },
    /// Settle a position as any fee payer.
    Settle {
        #[arg(long)]
        wallet: PathBuf,
        #[arg(long)]
        position: Address,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let operator_rpc = || rpc::Rpc::operator(&cli.rpc, cli.devnet);
    if cli.devnet
        && !matches!(
            &cli.command,
            Command::Admin { .. }
                | Command::Inventory { .. }
                | Command::Inspect { .. }
                | Command::Take { .. }
                | Command::Fix { .. }
                | Command::NoFix { .. }
                | Command::Settle { .. }
        )
    {
        anyhow::bail!(
            "--devnet is available for admin, inventory, inspect, take, fix, no-fix, and settle"
        );
    }
    match cli.command {
        Command::Load {
            service,
            expiry,
            writer,
            winner,
            rate,
            seconds,
            overload,
        } => load::run(&service, expiry, writer, winner, rate, seconds, overload),
        Command::Smoke {
            admin,
            fixture_receiver,
            timeout,
            race,
        } => {
            let rpc = rpc::Rpc::new(cli.rpc)?;
            let key = read_keypair_file(admin).map_err(|err| anyhow!("{err}"))?;
            if timeout {
                smoke::timeout(&rpc, key, fixture_receiver)
            } else if race {
                smoke::race(&rpc, &key, fixture_receiver)
            } else {
                smoke::run(&rpc, key, fixture_receiver)
            }
        }
        Command::Demo {
            admin,
            directory,
            expiry_seconds,
        } => smoke::demo(
            &rpc::Rpc::new(cli.rpc)?,
            read_keypair_file(admin).map_err(|err| anyhow!("{err}"))?,
            &directory,
            expiry_seconds,
        ),
        Command::Admin { wallet, action } => admin::run(&operator_rpc()?, &wallet, action),
        Command::Maker {
            wallet,
            service,
            audience,
            market,
            domain,
            lane,
            premium_bps,
        } => maker::run(
            &service,
            &read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?,
            &audience,
            market,
            domain,
            lane,
            premium_bps,
        ),
        Command::Inventory {
            wallet,
            market,
            mint,
            lane,
            amount,
            deposit,
        } => {
            let rpc = operator_rpc()?;
            let key = read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?;
            let signature = maker::inventory(&rpc, &key, market, lane, mint, amount, deposit)?;
            println!("{signature}");
            Ok(())
        }
        Command::Inspect {
            maker,
            market,
            expiry,
            lane,
        } => maker::inspect(&operator_rpc()?, maker, market, expiry, lane),
        Command::Take {
            wallet,
            service,
            expiry,
            kind,
            strike,
            quantity,
        } => maker::take(
            &operator_rpc()?,
            &service,
            &read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?,
            expiry,
            kind,
            strike,
            quantity,
        ),
        Command::Fix {
            wallet,
            market,
            expiry,
            proof,
        } => {
            let rpc = operator_rpc()?;
            let wallet = read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?;
            println!(
                "{}",
                rpc.send(
                    &wallet,
                    &[],
                    &[stryk_client::FixExpiryInstruction {
                        market,
                        expiry,
                        price_update: proof,
                    }
                    .into()]
                )?
            );
            Ok(())
        }
        Command::NoFix { wallet, expiry } => {
            let rpc = operator_rpc()?;
            let wallet = read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?;
            println!(
                "{}",
                rpc.send(
                    &wallet,
                    &[],
                    &[stryk_client::MarkNoFixInstruction { expiry }.into()]
                )?
            );
            Ok(())
        }
        Command::Settle { wallet, position } => {
            let rpc = operator_rpc()?;
            let wallet = read_keypair_file(wallet).map_err(|err| anyhow!("{err}"))?;
            println!("{}", rpc::settle(&rpc, &wallet, position)?);
            Ok(())
        }
    }
}
