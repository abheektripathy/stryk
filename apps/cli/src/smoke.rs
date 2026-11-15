use crate::rpc::Rpc;
use anyhow::{ensure, Context, Result};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_system_interface::instruction::{create_account, transfer};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};
use stryk_client::*;
use stryk_core::{Kind, Outcome, Quote};

const BASE: u64 = 1_000_000_000;
const CASH: u64 = 1_000_000;
const QUANTITY: u64 = 2 * BASE;
const PREMIUM: u64 = 6 * CASH;
const EXPIRY_SECONDS: i64 = 18;

fn token(data: Vec<u8>, accounts: Vec<AccountMeta>) -> Instruction {
    Instruction {
        program_id: TOKEN,
        accounts,
        data,
    }
}

fn initialize_quote_mint(payer: &Address, mint: &Address, rent: u64) -> [Instruction; 2] {
    let mut data = vec![20, 6];
    data.extend_from_slice(payer.as_ref());
    data.push(1);
    data.extend_from_slice(payer.as_ref());
    [
        create_account(payer, mint, rent, 82, &TOKEN),
        token(data, vec![AccountMeta::new(*mint, false)]),
    ]
}

fn fund(
    rpc: &Rpc,
    admin: &Keypair,
    owner: Address,
    quote: Address,
    base_amount: u64,
    quote_amount: u64,
) -> Result<()> {
    let payer = admin.pubkey();
    let base_account = ata(&owner, &WSOL);
    let mut mint_to = vec![14];
    mint_to.extend_from_slice(&quote_amount.to_le_bytes());
    mint_to.push(6);
    rpc.send(
        admin,
        &[],
        &[
            prepare_ata(payer, owner, WSOL),
            prepare_ata(payer, owner, quote),
            transfer(&payer, &base_account, base_amount),
            token(vec![17], vec![AccountMeta::new(base_account, false)]),
            token(
                mint_to,
                vec![
                    AccountMeta::new(quote, false),
                    AccountMeta::new(ata(&owner, &quote), false),
                    AccountMeta::new_readonly(payer, true),
                ],
            ),
        ],
    )?;
    Ok(())
}

fn initialize_fixture(
    rpc: &Rpc,
    admin: &Keypair,
    receiver: Address,
    feed: Address,
    observed: i64,
) -> Result<Address> {
    let mut data = vec![0; 134];
    data[..8].copy_from_slice(&[34, 241, 35, 99, 157, 126, 244, 205]);
    data[40] = 1;
    data[41..73].copy_from_slice(feed.as_ref());
    data[73..81].copy_from_slice(&(150_i64 * 100_000_000).to_le_bytes());
    data[89..93].copy_from_slice(&(-8_i32).to_le_bytes());
    data[93..101].copy_from_slice(&observed.to_le_bytes());
    data[101..109].copy_from_slice(&(observed - 1).to_le_bytes());
    write_fixture(rpc, admin, receiver, &data)
}

fn write_fixture(rpc: &Rpc, admin: &Keypair, receiver: Address, data: &[u8]) -> Result<Address> {
    let update = Keypair::new();
    let rent = rpc
        .call(
            "getMinimumBalanceForRentExemption",
            serde_json::json!([data.len()]),
        )?
        .as_u64()
        .context("fixture rent")?;
    let mut args = vec![1];
    args.extend_from_slice(data);
    rpc.send(
        admin,
        &[&update],
        &[
            create_account(
                &admin.pubkey(),
                &update.pubkey(),
                rent,
                data.len() as u64,
                &receiver,
            ),
            Instruction {
                program_id: receiver,
                accounts: vec![AccountMeta::new(update.pubkey(), true)],
                data: args,
            },
        ],
    )?;
    Ok(update.pubkey())
}

pub fn run(rpc: &Rpc, admin: Keypair, receiver: Address) -> Result<()> {
    let setup = bootstrap(rpc, &admin, receiver, EXPIRY_SECONDS, 0, 10_000 * CASH)?;
    verify(rpc, &admin, receiver, setup, false)
}

pub fn timeout(rpc: &Rpc, admin: Keypair, receiver: Address) -> Result<()> {
    let setup = bootstrap(rpc, &admin, receiver, EXPIRY_SECONDS, 0, 10_000 * CASH)?;
    verify(rpc, &admin, receiver, setup, true)
}

pub fn race(rpc: &Rpc, admin: &Keypair, receiver: Address) -> Result<()> {
    let setup = bootstrap(rpc, admin, receiver, 60, 2, 140 * CASH)?;
    let second_writer = Keypair::new();
    rpc.send(
        admin,
        &[],
        &[transfer(&admin.pubkey(), &second_writer.pubkey(), BASE)],
    )?;
    fund(
        rpc,
        admin,
        second_writer.pubkey(),
        setup.cash,
        10 * BASE,
        10_000 * CASH,
    )?;
    let funded = 146 * CASH;
    crate::maker::inventory(
        rpc,
        &setup.maker,
        setup.market,
        0,
        setup.cash,
        5_000 * CASH - funded,
        false,
    )?;
    let make = |writer: &Keypair, maker: &Keypair, id: u8| -> Result<([Instruction; 2], Address)> {
        let quote = Quote {
            domain: setup.domain.to_bytes(),
            program: ID.to_bytes(),
            market: setup.market.to_bytes(),
            expiry: setup.expiry.to_bytes(),
            rfq_id: [id; 16],
            writer: writer.pubkey().to_bytes(),
            maker: maker.pubkey().to_bytes(),
            lane: lane(&setup.market, &maker.pubkey(), 0).to_bytes(),
            base_mint: WSOL.to_bytes(),
            quote_mint: setup.cash.to_bytes(),
            kind: Kind::Call,
            strike: 140 * CASH,
            quantity: BASE,
            premium: 6 * CASH,
            valid_until: (rpc.clock()? + 35).min(setup.expires_at - 1),
        };
        let signature = maker.sign_message(&quote.digest());
        let instructions = fill_instructions(&quote, signature.as_array());
        let address = position(&setup.market, &writer.pubkey(), &quote.rfq_id);
        Ok((instructions, address))
    };
    let (first, first_pda) = make(&setup.writer, &setup.maker, 20)?;
    let (second, second_pda) = make(&second_writer, &setup.maker, 21)?;
    let paired = |first: &[Instruction], second: &[Instruction]| {
        let gate = std::sync::Barrier::new(2);
        let first_writer = &setup.writer;
        let send = |writer: &Keypair, instructions: &[Instruction]| {
            gate.wait();
            let start = Instant::now();
            (rpc.send(writer, &[], instructions), start.elapsed())
        };
        thread::scope(|scope| {
            let task = scope.spawn(|| send(first_writer, first));
            let second = send(&second_writer, second);
            (task.join().expect("concurrent writer"), second)
        })
    };
    let ((first_result, first_latency), (second_result, second_latency)) = paired(&first, &second);
    ensure!(
        first_result.is_ok() ^ second_result.is_ok(),
        "exactly one shared-lane fill must succeed"
    );
    let (winner, winner_pda, signature, latency, loser, loser_pda) = match first_result {
        Ok(signature) => (
            &setup.writer,
            first_pda,
            signature,
            first_latency,
            &second_writer,
            second_pda,
        ),
        Err(_) => (
            &second_writer,
            second_pda,
            second_result?,
            second_latency,
            &setup.writer,
            first_pda,
        ),
    };
    ensure!(
        rpc.account(loser_pda)?.is_none(),
        "loser created a position"
    );
    ensure!(
        rpc.account(ata(&loser_pda, &WSOL))?.is_none(),
        "loser created an escrow"
    );
    ensure!(
        rpc.balance(ata(&winner_pda, &WSOL))? == BASE,
        "winning base collateral mismatch"
    );
    ensure!(
        rpc.balance(ata(&winner_pda, &setup.cash))? == 140 * CASH,
        "winning strike collateral mismatch"
    );
    ensure!(
        rpc.balance(ata(&setup.lane, &setup.cash))? == 0,
        "maker spent beyond available balance"
    );
    ensure!(
        rpc.balance(ata(&winner.pubkey(), &setup.cash))? == 10_006 * CASH,
        "winning premium mismatch"
    );
    ensure!(
        rpc.balance(ata(&loser.pubkey(), &setup.cash))? == 10_000 * CASH,
        "loser balance changed"
    );
    rpc.measure("shared_lane_single_winner", &signature, latency)?;
    let loser_cash = ata(&loser.pubkey(), &setup.cash);
    let loser_base = ata(&loser.pubkey(), &WSOL);
    let (cash_before, base_before) = (rpc.balance(loser_cash)?, rpc.balance(loser_base)?);
    let (requote, requote_position) = make(loser, &setup.other_makers[0], 26)?;
    let start = Instant::now();
    let recovered = rpc.send(loser, &[], &requote)?;
    ensure!(
        rpc.balance(loser_cash)? == cash_before + 6 * CASH
            && rpc.balance(loser_base)? == base_before - BASE
            && rpc.balance(ata(&requote_position, &WSOL))? == BASE
            && rpc.balance(ata(&requote_position, &setup.cash))? == 140 * CASH
            && rpc.account(loser_pda)?.is_none(),
        "fresh quote did not recover from the funding race"
    );
    rpc.measure("funding_race_requote", &recovered, start.elapsed())?;
    let (first, _) = make(&setup.writer, &setup.other_makers[0], 22)?;
    let (second, _) = make(&second_writer, &setup.other_makers[1], 23)?;
    let ((first_result, first_latency), (second_result, second_latency)) = paired(&first, &second);
    rpc.measure("independent_lane_a", &first_result?, first_latency)?;
    rpc.measure("independent_lane_b", &second_result?, second_latency)?;
    let drain_maker = &setup.other_makers[0];
    let drain_lane = lane(&setup.market, &drain_maker.pubkey(), 0);
    let inventory = ata(&drain_lane, &setup.cash);
    let excess = rpc.balance(inventory)?;
    let (fill, position) = make(&setup.writer, drain_maker, 24)?;
    let gate = std::sync::Barrier::new(2);
    let (taken, withdrawn) = thread::scope(|scope| {
        let fill = scope.spawn(|| {
            gate.wait();
            rpc.send(&setup.writer, &[], &fill)
        });
        gate.wait();
        let withdrawn =
            crate::maker::inventory(rpc, drain_maker, setup.market, 0, setup.cash, excess, false);
        (fill.join().expect("concurrent fill"), withdrawn)
    });
    ensure!(
        taken.is_ok() ^ withdrawn.is_ok(),
        "exactly one fill or full withdrawal must succeed"
    );
    if taken.is_ok() {
        ensure!(
            rpc.balance(inventory)? == excess - funded,
            "withdraw changed filled lane balance"
        );
        ensure!(
            rpc.balance(ata(&position, &setup.cash))? == 140 * CASH,
            "withdraw affected booked escrow"
        );
    } else {
        ensure!(
            rpc.balance(inventory)? == 0 && rpc.account(position)?.is_none(),
            "fill changed a drained lane"
        );
    }
    println!(
        "{}",
        serde_json::json!({"label":"fill_withdraw_race","fill_won":taken.is_ok()})
    );
    let cap_maker = &setup.other_makers[1];
    let cap_lane = lane(&setup.market, &cap_maker.pubkey(), 0);
    let cap = capacity(&cap_lane, &setup.expiry);
    let excess = rpc.balance(ata(&cap_lane, &setup.cash))?;
    let cap_data = rpc.account(cap)?.context("capacity account")?;
    let used = u64::from_le_bytes(cap_data[73..81].try_into()?);
    let maximum = u64::from_le_bytes(cap_data[65..73].try_into()?);
    ensure!(
        maximum == 140 * CASH && used == maximum && excess >= funded,
        "capacity fixture did not reach its exact limit with available inventory"
    );
    let (blocked, position) = make(&setup.writer, cap_maker, 25)?;
    let error = rpc
        .send(&setup.writer, &[], &blocked)
        .expect_err("filled lane still accepted an order beyond cap");
    ensure!(error.to_string().contains("InstructionError"), "{error}");
    ensure!(
        rpc.account(position)?.is_none() && rpc.balance(ata(&cap_lane, &setup.cash))? == excess,
        "above-cap fill changed balances"
    );
    println!(
        "{}",
        serde_json::json!({"label":"capacity_boundary","filled_atoms":used,"limit_atoms":maximum})
    );
    Ok(())
}

pub fn demo(rpc: &Rpc, admin: Keypair, directory: &Path, expiry_seconds: i64) -> Result<()> {
    ensure!(
        (15..=3600).contains(&expiry_seconds),
        "fixture expiry must be 15–3600 seconds away"
    );
    let setup = bootstrap(
        rpc,
        &admin,
        crate::rpc::FIXTURE_RECEIVER,
        expiry_seconds,
        2,
        10_000 * CASH,
    )?;
    std::fs::create_dir_all(directory)?;
    for (name, key) in ["a", "b", "c"]
        .into_iter()
        .zip(std::iter::once(&setup.maker).chain(setup.other_makers.iter()))
    {
        solana_keypair::write_keypair_file(key, directory.join(format!("maker-{name}.json")))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    solana_keypair::write_keypair_file(&setup.writer, directory.join("writer.json"))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let proof = initialize_fixture(
        rpc,
        &admin,
        crate::rpc::FIXTURE_RECEIVER,
        setup.feed,
        setup.expires_at,
    )?;
    println!(
        "{}",
        serde_json::json!({
            "market":setup.market.to_string(), "cash":setup.cash.to_string(),
            "expiry":setup.expiry.to_string(), "expires_at":setup.expires_at,
            "writer":setup.writer.pubkey().to_string(), "domain":setup.domain.to_string(),
            "maker_a":setup.maker.pubkey().to_string(),
            "maker_b":setup.other_makers.first().context("maker B")?.pubkey().to_string(),
            "proof":proof.to_string(),
        })
    );
    Ok(())
}

struct Setup {
    maker: Keypair,
    other_makers: Vec<Keypair>,
    writer: Keypair,
    cash: Address,
    domain: Address,
    feed: Address,
    market: Address,
    lane: Address,
    expiry: Address,
    expires_at: i64,
}

fn bootstrap(
    rpc: &Rpc,
    admin: &Keypair,
    receiver: Address,
    expiry_seconds: i64,
    additional_makers: usize,
    last_lane_limit: u64,
) -> Result<Setup> {
    let maker = Keypair::new();
    let writer = if additional_makers > 0 {
        Keypair::new_from_array([42; 32])
    } else {
        Keypair::new()
    };
    let quote_mint = Keypair::new();
    let cash = quote_mint.pubkey();
    let domain = Keypair::new().pubkey();
    let feed = Keypair::new().pubkey();
    let admin_key = admin.pubkey();
    let maker_key = maker.pubkey();
    let writer_key = writer.pubkey();
    let market = market(&WSOL, &cash, 1);
    let lane = lane(&market, &maker_key, 0);
    let rent = rpc
        .call("getMinimumBalanceForRentExemption", serde_json::json!([82]))?
        .as_u64()
        .context("mint rent")?;
    let init = initialize_quote_mint(&admin_key, &cash, rent);
    rpc.send(admin, &[&quote_mint], &init)?;
    rpc.send(
        admin,
        &[],
        &[
            transfer(&admin_key, &writer_key, BASE),
            transfer(&admin_key, &maker_key, BASE),
        ],
    )?;
    fund(rpc, admin, maker_key, cash, 20 * BASE, 10_000 * CASH)?;
    fund(rpc, admin, writer_key, cash, 10 * BASE, 10_000 * CASH)?;
    rpc.send(
        admin,
        &[],
        &[
            InitializeMarketInstruction {
                admin: admin_key,
                program_data: program_data(),
                base_mint: WSOL,
                quote_mint: cash,
                market,
                system_program: SYSTEM,
                receiver_program: receiver,
                version: 1,
                domain,
                receiver,
                feed,
                strike_tick: CASH,
                min_quantity: 1,
                max_quantity: 10 * BASE,
                fill_cutoff: 1,
                fix_timeout: 31,
                confidence_bps: 100,
                weekly: false,
            }
            .into(),
            ConfigureLaneInstruction {
                admin: admin_key,
                market,
                maker: maker_key,
                lane,
                system_program: SYSTEM,
                lane_id: 0,
            }
            .into(),
            prepare_ata(admin_key, lane, WSOL),
            prepare_ata(admin_key, lane, cash),
        ],
    )?;
    for (mint, amount) in [(WSOL, 10 * BASE), (cash, 5_000 * CASH)] {
        rpc.send(
            &maker,
            &[],
            &[DepositInstruction {
                maker: maker_key,
                market,
                lane,
                mint,
                wallet: ata(&maker_key, &mint),
                inventory: ata(&lane, &mint),
                token_program: TOKEN,
                amount,
            }
            .into()],
        )?;
    }
    let mut other_makers = Vec::new();
    for _ in 0..additional_makers {
        let key = Keypair::new();
        let owner = key.pubkey();
        let second_lane = stryk_client::lane(&market, &owner, 0);
        rpc.send(admin, &[], &[transfer(&admin_key, &owner, BASE)])?;
        fund(rpc, admin, owner, cash, 20 * BASE, 10_000 * CASH)?;
        rpc.send(
            admin,
            &[],
            &[ConfigureLaneInstruction {
                admin: admin_key,
                market,
                maker: owner,
                lane: second_lane,
                system_program: SYSTEM,
                lane_id: 0,
            }
            .into()],
        )?;
        for (mint, amount) in [(WSOL, 10 * BASE), (cash, 5_000 * CASH)] {
            crate::maker::inventory(rpc, &key, market, 0, mint, amount, true)?;
        }
        other_makers.push(key);
    }
    let expires_at = rpc.clock()? + expiry_seconds;
    let expiry = expiry(&market, expires_at);
    let capacity = capacity(&lane, &expiry);
    rpc.send(
        admin,
        &[],
        &[
            InitializeExpiryInstruction {
                admin: admin_key,
                market,
                expiry,
                system_program: SYSTEM,
                expires_at: expires_at.try_into()?,
            }
            .into(),
            InitializeCapacityInstruction {
                admin: admin_key,
                market,
                lane,
                expiry,
                capacity,
                system_program: SYSTEM,
                limit: 10_000 * CASH,
            }
            .into(),
        ],
    )?;
    for (index, maker) in other_makers.iter().enumerate() {
        let lane = stryk_client::lane(&market, &maker.pubkey(), 0);
        rpc.send(
            admin,
            &[],
            &[InitializeCapacityInstruction {
                admin: admin_key,
                market,
                lane,
                expiry,
                capacity: stryk_client::capacity(&lane, &expiry),
                system_program: SYSTEM,
                limit: if index + 1 == other_makers.len() {
                    last_lane_limit
                } else {
                    10_000 * CASH
                },
            }
            .into()],
        )?;
    }
    Ok(Setup {
        maker,
        other_makers,
        writer,
        cash,
        domain,
        feed,
        market,
        lane,
        expiry,
        expires_at,
    })
}

fn verify(
    rpc: &Rpc,
    admin: &Keypair,
    receiver: Address,
    setup: Setup,
    timeout: bool,
) -> Result<()> {
    let Setup {
        maker,
        writer,
        cash,
        domain,
        feed,
        market,
        lane,
        expiry,
        expires_at,
        ..
    } = setup;
    let maker_key = maker.pubkey();
    let writer_key = writer.pubkey();
    let settler = Keypair::new();
    rpc.send(
        admin,
        &[],
        &[transfer(&admin.pubkey(), &settler.pubkey(), BASE)],
    )?;
    let proof = if timeout {
        None
    } else {
        Some(initialize_fixture(rpc, admin, receiver, feed, expires_at)?)
    };
    let invalid_proofs = if let Some(proof) = proof {
        let data = rpc.account(proof)?.context("fixture account missing")?;
        let mut invalid = Vec::new();
        for (offset, bytes) in [
            (40, vec![0]),
            (41, vec![data[41] ^ 1]),
            (81, 150_000_001_u64.to_le_bytes().to_vec()),
            (93, (expires_at - 1).to_le_bytes().to_vec()),
            (101, expires_at.to_le_bytes().to_vec()),
        ] {
            let mut raw = data.clone();
            raw[offset..offset + bytes.len()].copy_from_slice(&bytes);
            invalid.push(write_fixture(rpc, admin, receiver, &raw)?);
        }
        invalid
    } else {
        Vec::new()
    };
    let mut positions = Vec::new();
    for (index, (kind, strike, mut outcome)) in [
        (Kind::Call, 140 * CASH, Outcome::Assigned),
        (Kind::Put, 160 * CASH, Outcome::Assigned),
        (Kind::Call, 160 * CASH, Outcome::Expired),
        (Kind::Put, 140 * CASH, Outcome::Expired),
        (Kind::Call, 150 * CASH, Outcome::Expired),
    ]
    .into_iter()
    .enumerate()
    {
        if timeout {
            if index > 1 {
                break;
            }
            outcome = Outcome::NoFix;
        }
        let quote = Quote {
            domain: domain.to_bytes(),
            program: ID.to_bytes(),
            market: market.to_bytes(),
            expiry: expiry.to_bytes(),
            rfq_id: [index as u8; 16],
            writer: writer_key.to_bytes(),
            maker: maker_key.to_bytes(),
            lane: lane.to_bytes(),
            base_mint: WSOL.to_bytes(),
            quote_mint: cash.to_bytes(),
            kind,
            strike,
            quantity: QUANTITY,
            premium: PREMIUM,
            valid_until: expires_at - 1,
        };
        let signature: [u8; 64] = maker.sign_message(&quote.digest()).as_ref().try_into()?;
        let position = position(&market, &writer_key, &quote.rfq_id);
        let instructions = fill_instructions(&quote, &signature);
        let writer_cash = ata(&writer_key, &cash);
        let writer_base = ata(&writer_key, &WSOL);
        let lane_cash = ata(&lane, &cash);
        let lane_base = ata(&lane, &WSOL);
        let (cash_before, base_before) = (rpc.balance(writer_cash)?, rpc.balance(writer_base)?);
        let (lane_cash_before, lane_base_before) =
            (rpc.balance(lane_cash)?, rpc.balance(lane_base)?);
        if index == 0 && !timeout {
            rpc.send(
                admin,
                &[],
                &[
                    transfer(&admin.pubkey(), &position, 1_000_000),
                    prepare_ata(admin.pubkey(), position, WSOL),
                    prepare_ata(admin.pubkey(), position, cash),
                ],
            )?;
        }
        let incomplete = rpc.account(position)?;
        if index == 0 {
            let mut unsigned_writer = instructions.clone();
            unsigned_writer[1].accounts[0].is_signer = false;
            let error = rpc
                .send(admin, &[], &unsigned_writer)
                .expect_err("writer signer required");
            ensure!(error.to_string().contains("InstructionError"), "{error}");
            for (slot, wrong) in [
                (1, maker_key),
                (3, maker_key),
                (5, writer_key),
                (6, cash),
                (8, lane_base),
                (12, lane_base),
                (15, SYSTEM),
                (17, SYSTEM),
            ] {
                let mut changed = instructions.clone();
                changed[1].accounts[slot].pubkey = wrong;
                let error = rpc
                    .send(&writer, &[], &changed)
                    .expect_err("misbound account");
                ensure!(error.to_string().contains("InstructionError"), "{error}");
            }
            for forged in [
                Quote {
                    domain: [41; 32],
                    ..quote
                },
                Quote {
                    strike: strike + CASH,
                    ..quote
                },
                Quote {
                    valid_until: rpc.clock()? - 1,
                    ..quote
                },
                Quote {
                    valid_until: expires_at + 60,
                    ..quote
                },
            ] {
                let changed = fill_instructions(&forged, &signature);
                let error = rpc
                    .send(&writer, &[], &changed)
                    .expect_err("invalid signed terms");
                ensure!(error.to_string().contains("InstructionError"), "{error}");
            }
            if !timeout {
                for paused in [true, false] {
                    rpc.send(
                        admin,
                        &[],
                        &[SetMarketPausedInstruction {
                            admin: admin.pubkey(),
                            market,
                            paused,
                        }
                        .into()],
                    )?;
                    if paused {
                        let error = rpc
                            .send(&writer, &[], &instructions)
                            .expect_err("paused market accepted fill");
                        ensure!(error.to_string().contains("InstructionError"), "{error}");
                    }
                }
                let freeze = |instruction| {
                    token(
                        vec![instruction],
                        vec![
                            AccountMeta::new(writer_cash, false),
                            AccountMeta::new_readonly(cash, false),
                            AccountMeta::new_readonly(admin.pubkey(), true),
                        ],
                    )
                };
                rpc.send(admin, &[], &[freeze(10)])?;
                let escrow_balance = (
                    rpc.balance(ata(&position, &WSOL))?,
                    rpc.balance(ata(&position, &cash))?,
                );
                let mut failed_late = instructions.to_vec();
                failed_late.insert(
                    0,
                    Instruction {
                        program_id: solana_address::address!(
                            "ComputeBudget111111111111111111111111111111"
                        ),
                        accounts: Vec::new(),
                        data: [vec![2], 300_000_u32.to_le_bytes().to_vec()].concat(),
                    },
                );
                let error = rpc
                    .send(&writer, &[], &failed_late)
                    .expect_err("frozen premium destination");
                ensure!(
                    error.to_string().contains("AccountFrozen")
                        || error.to_string().contains("0x11"),
                    "{error}"
                );
                ensure!(
                    (
                        rpc.balance(ata(&position, &WSOL))?,
                        rpc.balance(ata(&position, &cash))?
                    ) == escrow_balance,
                    "late CPI changed escrows"
                );
                rpc.send(admin, &[], &[freeze(11)])?;
            }
            ensure!(
                rpc.account(position)? == incomplete
                    && rpc.balance(writer_cash)? == cash_before
                    && rpc.balance(writer_base)? == base_before
                    && rpc.balance(lane_cash)? == lane_cash_before
                    && rpc.balance(lane_base)? == lane_base_before,
                "altered quote changed balances or the position"
            );
        }
        let started = Instant::now();
        let sig = rpc.send(&writer, &[], &instructions)?;
        rpc.measure(
            if kind == Kind::Call {
                "fill_call"
            } else {
                "fill_put"
            },
            &sig,
            started.elapsed(),
        )?;
        if index == 0 {
            let mut replay = vec![Instruction {
                program_id: solana_address::address!("ComputeBudget111111111111111111111111111111"),
                accounts: Vec::new(),
                data: [vec![2], 400_000_u32.to_le_bytes().to_vec()].concat(),
            }];
            replay.extend(instructions);
            let error = rpc
                .send(&writer, &[], &replay)
                .expect_err("filled RFQ replay");
            ensure!(error.to_string().contains("InstructionError"), "{error}");
        }
        let notional = stryk_core::strike_atoms(QUANTITY, strike).context("smoke strike")?;
        ensure!(
            rpc.balance(ata(&position, &WSOL))? == QUANTITY,
            "base escrow mismatch"
        );
        ensure!(
            rpc.balance(ata(&position, &cash))? == notional,
            "cash escrow mismatch"
        );
        let cash_locked = if kind == Kind::Put { notional } else { 0 };
        let writer_base_locked = if kind == Kind::Call { QUANTITY } else { 0 };
        ensure!(
            rpc.balance(writer_cash)? == cash_before + PREMIUM - cash_locked,
            "writer premium/collateral mismatch"
        );
        ensure!(
            rpc.balance(writer_base)? == base_before - writer_base_locked,
            "writer base mismatch"
        );
        ensure!(
            rpc.balance(lane_cash)? == lane_cash_before - PREMIUM - (notional - cash_locked),
            "maker cash mismatch"
        );
        ensure!(
            rpc.balance(lane_base)? == lane_base_before - (QUANTITY - writer_base_locked),
            "maker base mismatch"
        );
        println!("{kind:?} fill {sig} position {position}");
        positions.push((position, kind, notional, outcome, quote, signature));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let settle_after = if timeout { expires_at + 31 } else { expires_at };
    if timeout {
        let transition = MarkNoFixInstruction { expiry }.into();
        let error = rpc
            .send(admin, &[], &[transition])
            .expect_err("no-fix before deadline");
        ensure!(error.to_string().contains("InstructionError"), "{error}");
        ensure!(
            rpc.account(expiry)?.and_then(|data| data.last().copied()) == Some(0),
            "early no-fix changed expiry"
        );
    }
    while rpc.clock()? < settle_after {
        ensure!(
            Instant::now() < deadline,
            "validator Clock did not reach expiry"
        );
        thread::sleep(Duration::from_millis(300));
    }
    let ix = match proof {
        Some(price_update) => FixExpiryInstruction {
            market,
            expiry,
            price_update,
        }
        .into(),
        None => MarkNoFixInstruction { expiry }.into(),
    };
    for price_update in invalid_proofs {
        let error = rpc
            .send(
                admin,
                &[],
                &[FixExpiryInstruction {
                    market,
                    expiry,
                    price_update,
                }
                .into()],
            )
            .expect_err("invalid expiry proof");
        ensure!(error.to_string().contains("InstructionError"), "{error}");
        ensure!(
            rpc.account(expiry)?.and_then(|data| data.last().copied()) == Some(0),
            "invalid proof changed expiry"
        );
    }
    let started = Instant::now();
    let transition = rpc.send(&settler, &[], &[ix])?;
    rpc.measure(
        if timeout { "mark_no_fix" } else { "fix" },
        &transition,
        started.elapsed(),
    )?;
    println!("expiry {expiry} transition {transition}");
    let fixed = rpc.stryk_account(expiry, 2)?;
    ensure!(
        fixed.last().copied() == Some(if timeout { 2 } else { 1 }),
        "unexpected expiry outcome"
    );
    if let Some(price_update) = proof {
        let error = rpc
            .send(
                admin,
                &[],
                &[FixExpiryInstruction {
                    market,
                    expiry,
                    price_update,
                }
                .into()],
            )
            .expect_err("accepted an attempt to replace the recorded fix");
        ensure!(error.to_string().contains("InstructionError"), "{error}");
        ensure!(
            rpc.stryk_account(expiry, 2)? == fixed,
            "recorded fix changed"
        );
    }
    println!(
        "Unrelated fee payer {} recorded the expiry transition.",
        settler.pubkey()
    );
    for (index, (position, kind, notional, outcome, quote, signature)) in
        positions.into_iter().enumerate()
    {
        let (writer_base, writer_quote) = (ata(&writer_key, &WSOL), ata(&writer_key, &cash));
        let (maker_base, maker_quote) = (ata(&maker_key, &WSOL), ata(&maker_key, &cash));
        let escrow_base = ata(&position, &WSOL);
        let escrow_quote = ata(&position, &cash);
        let (base_out, quote_out) = match (kind, outcome == Outcome::Assigned) {
            (Kind::Call, true) | (Kind::Put, false) => (maker_base, writer_quote),
            (Kind::Put, true) | (Kind::Call, false) => (writer_base, maker_quote),
        };
        let recreate_destination = index == 0 && !timeout;
        if recreate_destination {
            rpc.send(
                &maker,
                &[],
                &[token(
                    vec![9],
                    vec![
                        AccountMeta::new(maker_base, false),
                        AccountMeta::new(maker_key, false),
                        AccountMeta::new_readonly(maker_key, true),
                    ],
                )],
            )?;
            ensure!(
                rpc.account(maker_base)?.is_none(),
                "maker destination remained open"
            );
        }
        let base_before = if recreate_destination {
            0
        } else {
            rpc.balance(base_out)?
        };
        let cash_before = rpc.balance(quote_out)?;
        let donation = if index < 2 && !timeout { 7_000_000 } else { 0 };
        let sync = index == 1 && donation > 0;
        if donation > 0 {
            let mut instructions = vec![transfer(&admin.pubkey(), &escrow_base, donation)];
            if sync {
                instructions.push(token(vec![17], vec![AccountMeta::new(escrow_base, false)]));
            }
            rpc.send(admin, &[], &instructions)?;
        }
        let original_base = if kind == Kind::Call {
            writer_base
        } else {
            maker_base
        };
        let original_before = rpc.balance(original_base)?;
        let rent_recipient_before = rpc.lamports(writer_key)?;
        let close_proceeds =
            rpc.lamports(escrow_base)? - QUANTITY - if sync { donation } else { 0 }
                + rpc.lamports(escrow_quote)?;
        let settlement =
            stryk_client::settle(market, position, expiry, writer_key, maker_key, WSOL, cash);
        let original_quote = if kind == Kind::Call {
            maker_quote
        } else {
            writer_quote
        };
        let quote_donation = if recreate_destination { 9 * CASH } else { 0 };
        let original_quote_before = rpc.balance(original_quote)?;
        if quote_donation > 0 {
            let mut data = vec![14];
            data.extend_from_slice(&quote_donation.to_le_bytes());
            data.push(6);
            rpc.send(
                admin,
                &[],
                &[token(
                    data,
                    vec![
                        AccountMeta::new(cash, false),
                        AccountMeta::new(escrow_quote, false),
                        AccountMeta::new_readonly(admin.pubkey(), true),
                    ],
                )],
            )?;
        }
        let mut instructions = Vec::new();
        if recreate_destination {
            instructions.push(prepare_ata(settler.pubkey(), maker_key, WSOL));
            let mut wrong_destination = settlement.clone();
            wrong_destination.accounts[8].pubkey = maker_quote;
            let error = rpc
                .send(&settler, &[], &[instructions[0].clone(), wrong_destination])
                .expect_err("wrong settlement recipient");
            ensure!(error.to_string().contains("InstructionError"), "{error}");
            ensure!(
                rpc.account(maker_base)?.is_none()
                    && rpc.balance(escrow_quote)? == notional + quote_donation,
                "wrong recipient changed settlement accounts"
            );
        }
        instructions.push(settlement.clone());
        let started = Instant::now();
        let sig = rpc.send(&settler, &[], &instructions)?;
        let label = match (kind, recreate_destination) {
            (Kind::Call, true) => "settle_call_missing_destination",
            (Kind::Call, false) => "settle_call",
            (Kind::Put, _) => "settle_put",
        };
        rpc.measure(label, &sig, started.elapsed())?;
        ensure!(
            rpc.balance(base_out)? == base_before + QUANTITY,
            "base delivery mismatch"
        );
        ensure!(
            rpc.balance(quote_out)? == cash_before + notional,
            "cash delivery mismatch"
        );
        if quote_donation > 0 {
            ensure!(
                rpc.balance(original_quote)? == original_quote_before + quote_donation,
                "donated USDC reached the wrong recipient"
            );
        }
        if donation > 0 {
            ensure!(
                rpc.balance(original_base)? == original_before + if sync { donation } else { 0 },
                "native donation reached the wrong recipient"
            );
        }
        let after_rent = rpc.lamports(writer_key)?;
        ensure!(
            after_rent == rent_recipient_before + close_proceeds,
            "incorrect escrow close proceeds"
        );
        ensure!(
            rpc.account(escrow_base)?.is_none() && rpc.account(escrow_quote)?.is_none(),
            "escrows remained open"
        );
        let receipt = rpc.account(position)?.context("terminal receipt")?;
        ensure!(
            receipt.last() == Some(&(outcome as u8)),
            "wrong settlement status"
        );
        if recreate_destination {
            let error = rpc
                .send(admin, &[], &[settlement])
                .expect_err("duplicate settlement");
            ensure!(error.to_string().contains("InstructionError"), "{error}");
            ensure!(
                rpc.account(position)?.as_deref() == Some(receipt.as_slice()),
                "duplicate settlement changed the receipt"
            );
            let error = rpc
                .send(&writer, &[], &fill_instructions(&quote, &signature))
                .expect_err("terminal RFQ replay");
            ensure!(error.to_string().contains("InstructionError"), "{error}");
            ensure!(
                rpc.account(position)?.as_deref() == Some(receipt.as_slice()),
                "terminal RFQ replay changed the receipt"
            );
        }
        println!("{kind:?} {outcome:?} settlement {sig}");
    }
    Ok(())
}
