#[rustfmt::skip]
mod generated;

pub use generated::*;
pub mod api;
use solana_address::{address, Address};
use solana_ed25519_program::new_ed25519_instruction_with_signature;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_transaction::Transaction;
use stryk_core::Quote;

pub const SYSTEM: Address = address!("11111111111111111111111111111111");
pub const TOKEN: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ATA: Address = address!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const WSOL: Address = address!("So11111111111111111111111111111111111111112");
pub const LOADER: Address = address!("BPFLoaderUpgradeab1e11111111111111111111111");
pub const INSTRUCTIONS: Address = address!("Sysvar1nstructions1111111111111111111111111");
pub const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
const ED25519: Address = address!("Ed25519SigVerify111111111111111111111111111");

pub fn market(base: &Address, quote: &Address, version: u16) -> Address {
    pda(&[
        b"market",
        base.as_ref(),
        quote.as_ref(),
        &version.to_le_bytes(),
    ])
}

pub fn expiry(market: &Address, expiry: i64) -> Address {
    pda(&[b"expiry", market.as_ref(), &expiry.to_le_bytes()])
}

pub fn lane(market: &Address, maker: &Address, lane: u16) -> Address {
    pda(&[
        b"maker",
        market.as_ref(),
        maker.as_ref(),
        &lane.to_le_bytes(),
    ])
}

pub fn capacity(lane: &Address, expiry: &Address) -> Address {
    pda(&[b"cap", lane.as_ref(), expiry.as_ref()])
}

pub fn position(market: &Address, writer: &Address, rfq_id: &[u8; 16]) -> Address {
    pda(&[b"position", market.as_ref(), writer.as_ref(), rfq_id])
}

pub fn ata(owner: &Address, mint: &Address) -> Address {
    Address::find_program_address(&[owner.as_ref(), TOKEN.as_ref(), mint.as_ref()], &ATA).0
}

pub fn program_data() -> Address {
    Address::find_program_address(&[ID.as_ref()], &LOADER).0
}

fn pda(seeds: &[&[u8]]) -> Address {
    Address::find_program_address(seeds, &ID).0
}

pub fn prepare_ata(payer: Address, owner: Address, mint: Address) -> Instruction {
    Instruction {
        program_id: ATA,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(ata(&owner, &mint), false),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(SYSTEM, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
        data: vec![1],
    }
}

pub fn wsol_instructions(owner: Address, lamports: Option<u64>) -> Vec<Instruction> {
    let wrapped = ata(&owner, &WSOL);
    match lamports {
        Some(lamports) => {
            let mut transfer_data = 2_u32.to_le_bytes().to_vec();
            transfer_data.extend_from_slice(&lamports.to_le_bytes());
            vec![
                prepare_ata(owner, owner, WSOL),
                Instruction {
                    program_id: SYSTEM,
                    accounts: vec![
                        AccountMeta::new(owner, true),
                        AccountMeta::new(wrapped, false),
                    ],
                    data: transfer_data,
                },
                Instruction {
                    program_id: TOKEN,
                    accounts: vec![AccountMeta::new(wrapped, false)],
                    data: vec![17],
                },
            ]
        }
        None => vec![Instruction {
            program_id: TOKEN,
            accounts: vec![
                AccountMeta::new(wrapped, false),
                AccountMeta::new(owner, false),
                AccountMeta::new_readonly(owner, true),
            ],
            data: vec![9],
        }],
    }
}

pub fn settle(
    market: Address,
    position: Address,
    expiry: Address,
    writer: Address,
    maker: Address,
    base: Address,
    quote: Address,
) -> Instruction {
    SettlePositionInstruction {
        market,
        expiry,
        position,
        writer,
        maker,
        base_mint: base,
        quote_mint: quote,
        writer_base: ata(&writer, &base),
        writer_quote: ata(&writer, &quote),
        maker_base: ata(&maker, &base),
        maker_quote: ata(&maker, &quote),
        escrow_base: ata(&position, &base),
        escrow_quote: ata(&position, &quote),
        token_program: TOKEN,
    }
    .into()
}

pub fn fill_instructions(quote: &Quote, signature: &[u8; 64]) -> [Instruction; 2] {
    let writer = Address::new_from_array(quote.writer);
    let market = Address::new_from_array(quote.market);
    let expiry = Address::new_from_array(quote.expiry);
    let lane = Address::new_from_array(quote.lane);
    let position = position(&market, &writer, &quote.rfq_id);
    let base = Address::new_from_array(quote.base_mint);
    let cash = Address::new_from_array(quote.quote_mint);
    let rfq = u128::from_le_bytes(quote.rfq_id);
    [
        new_ed25519_instruction_with_signature(&quote.digest(), signature, &quote.maker),
        FillInstruction {
            writer,
            market,
            expiry,
            lane,
            capacity: capacity(&lane, &expiry),
            position,
            base_mint: base,
            quote_mint: cash,
            writer_base: ata(&writer, &base),
            writer_quote: ata(&writer, &cash),
            lane_base: ata(&lane, &base),
            lane_quote: ata(&lane, &cash),
            escrow_base: ata(&position, &base),
            escrow_quote: ata(&position, &cash),
            instructions: INSTRUCTIONS,
            token_program: TOKEN,
            system_program: SYSTEM,
            ata_program: ATA,
            rfq_lo: rfq as u64,
            rfq_hi: (rfq >> 64) as u64,
            kind: quote.kind as u8,
            strike: quote.strike,
            quantity: quote.quantity,
            premium: quote.premium,
            valid_until: quote.valid_until,
        }
        .into(),
    ]
}

pub fn unsigned(payer: &Address, blockhash: [u8; 32], instructions: &[Instruction]) -> Transaction {
    Transaction::new_unsigned(Message::new_with_blockhash(
        instructions,
        Some(payer),
        &blockhash.into(),
    ))
}

pub fn wire(transaction: &Transaction) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(transaction)
}

pub fn checked_signed_wire(bytes: &[u8], reviewed: &Transaction) -> Option<String> {
    let signed = decode_wire(bytes)?;
    if signed.message == reviewed.message
        && signed.signatures.len() == 1
        && signed.signatures[0] != Default::default()
    {
        return Some(signed.signatures[0].to_string());
    }
    None
}

pub fn is_fill_wire(bytes: &[u8], market: &Address) -> bool {
    let Some(tx) = decode_wire(bytes) else {
        return false;
    };
    let [prep @ .., proof, fill] = tx.message.instructions.as_slice() else {
        return false;
    };
    let account = |index: u8| tx.message.account_keys.get(usize::from(index));
    let fill_key = |index: usize| fill.accounts.get(index).and_then(|index| account(*index));
    let expected_ata =
        |mint_index,
         ata_index,
         candidate: &solana_message::compiled_instruction::CompiledInstruction| {
            let (Some(writer), Some(mint), Some(ata)) =
                (fill_key(0), fill_key(mint_index), fill_key(ata_index))
            else {
                return false;
            };
            let keys: Vec<_> = candidate
                .accounts
                .iter()
                .copied()
                .filter_map(account)
                .collect();
            account(candidate.program_id_index) == Some(&ATA)
                && candidate.data == [1]
                && keys == [writer, ata, writer, mint, &SYSTEM, &TOKEN]
        };
    let prep_valid = match prep {
        [] => true,
        [one] => expected_ata(6, 8, one) || expected_ata(7, 9, one),
        [base, quote] => expected_ata(6, 8, base) && expected_ata(7, 9, quote),
        _ => false,
    };
    tx.signatures.len() == 1
        && tx.message.header.num_required_signatures == 1
        && account(proof.program_id_index) == Some(&ED25519)
        && account(fill.program_id_index) == Some(&ID)
        && fill.data.len() == 50
        && fill.data.first() == Some(&9)
        && fill.accounts.first() == Some(&0)
        && fill_key(1) == Some(market)
        && prep_valid
}

pub fn is_settlement_wire(bytes: &[u8], market: &Address) -> bool {
    let Some(tx) = decode_wire(bytes) else {
        return false;
    };
    let Some(payer) = tx.message.account_keys.first().copied() else {
        return false;
    };
    let Some((settle_ix, prep)) = tx.message.instructions.split_last() else {
        return false;
    };
    let account = |index: u8| tx.message.account_keys.get(usize::from(index)).copied();
    let keys: Option<Vec<_>> = settle_ix.accounts.iter().copied().map(account).collect();
    let Some(keys) = keys else { return false };
    let [actual_market, expiry, position, writer, maker, base, quote, ..] = keys.as_slice() else {
        return false;
    };
    if actual_market != market || prep.len() > 4 {
        return false;
    }
    let mut instructions = Vec::with_capacity(prep.len() + 1);
    let mut seen = Vec::with_capacity(prep.len());
    for ix in prep {
        let Some(owner) = ix.accounts.get(2).and_then(|index| account(*index)) else {
            return false;
        };
        let Some(mint) = ix.accounts.get(3).and_then(|index| account(*index)) else {
            return false;
        };
        if (owner != *writer && owner != *maker)
            || (mint != *base && mint != *quote)
            || seen.contains(&(owner, mint))
        {
            return false;
        }
        seen.push((owner, mint));
        instructions.push(prepare_ata(payer, owner, mint));
    }
    instructions.push(settle(
        *market, *position, *expiry, *writer, *maker, *base, *quote,
    ));
    let expected = unsigned(
        &payer,
        tx.message.recent_blockhash.to_bytes(),
        &instructions,
    );
    tx.signatures.len() == 1 && tx.message == expected.message
}

pub fn is_wsol_wire(bytes: &[u8]) -> bool {
    let Some(tx) = decode_wire(bytes) else {
        return false;
    };
    let Some(owner) = tx.message.account_keys.first().copied() else {
        return false;
    };
    let amount = match tx.message.instructions.as_slice() {
        [_] => None,
        [_, transfer, _] if transfer.data.len() == 12 => {
            let Ok(amount) = transfer.data[4..].try_into() else {
                return false;
            };
            let amount = u64::from_le_bytes(amount);
            if amount == 0 {
                return false;
            }
            Some(amount)
        }
        _ => return false,
    };
    let expected = unsigned(
        &owner,
        tx.message.recent_blockhash.to_bytes(),
        &wsol_instructions(owner, amount),
    );
    tx.signatures.len() == 1 && tx.message == expected.message
}

fn decode_wire(bytes: &[u8]) -> Option<Transaction> {
    use bincode::Options;

    if bytes.len() > 1232 {
        return None;
    }
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1232)
        .reject_trailing_bytes()
        .deserialize(bytes)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stryk_core::Kind;

    #[test]
    fn relay_accepts_only_expected_transaction_shapes() {
        let writer = Address::new_from_array([21; 32]);
        let maker = Address::new_from_array([22; 32]);
        let quote_mint = Address::new_from_array([23; 32]);
        let market = market(&WSOL, &quote_mint, 1);
        let expiry = expiry(&market, 1_800_000_000);
        let rfq_id = [4; 16];
        let lane = lane(&market, &maker, 0);
        let quote = Quote {
            domain: [1; 32],
            program: ID.to_bytes(),
            market: market.to_bytes(),
            expiry: expiry.to_bytes(),
            rfq_id,
            writer: writer.to_bytes(),
            maker: maker.to_bytes(),
            lane: lane.to_bytes(),
            base_mint: WSOL.to_bytes(),
            quote_mint: quote_mint.to_bytes(),
            kind: Kind::Put,
            strike: 150_000_000,
            quantity: 1_000_000_000,
            premium: 2_000_000,
            valid_until: 1_799_999_900,
        };
        let bytes = |instructions: &[Instruction]| {
            wire(&unsigned(&writer, [25; 32], instructions)).unwrap()
        };
        let fill = fill_instructions(&quote, &[5; 64]);
        assert!(is_fill_wire(&bytes(&fill), &market));
        let with_destination = [vec![prepare_ata(writer, writer, WSOL)], fill.to_vec()].concat();
        assert!(is_fill_wire(&bytes(&with_destination), &market));
        let foreign_destination = [vec![prepare_ata(writer, maker, WSOL)], fill.to_vec()].concat();
        assert!(!is_fill_wire(&bytes(&foreign_destination), &market));
        assert!(!is_fill_wire(&bytes(&fill), &writer));
        assert!(is_wsol_wire(&bytes(&wsol_instructions(writer, Some(10)))));
        assert!(is_wsol_wire(&bytes(&wsol_instructions(writer, None))));
        assert!(!is_wsol_wire(&bytes(&wsol_instructions(maker, None))));

        let position = position(&market, &writer, &rfq_id);
        let settlement = [
            prepare_ata(writer, maker, WSOL),
            settle(market, position, expiry, writer, maker, WSOL, quote_mint),
        ];
        assert!(is_settlement_wire(&bytes(&settlement), &market));
        let altered = [prepare_ata(writer, maker, TOKEN), settlement[1].clone()];
        assert!(!is_settlement_wire(&bytes(&altered), &market));
    }
}
