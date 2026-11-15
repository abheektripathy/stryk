use crate::{wallet::Connected, web};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Function, Object, Promise, Reflect};
use serde_json::{json, Value};
use solana_address::Address;
use solana_instruction::Instruction;
use std::{cell::Cell, future::Future, rc::Rc};
use stryk_client::api::{self, Bid, Market, Request};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::{future_to_promise, JsFuture};
use web_sys::Storage;

const PENDING_FILL: &str =
    "A transaction is awaiting reconciliation. Check its status before sending another.";
const FILL_BUSY: &str =
    "This wallet is reviewing or checking a fill in another tab. Retry shortly.";

#[derive(Clone)]
pub struct FillStatus {
    pub message: String,
    pub position: String,
    pub finalized: bool,
}

pub async fn execute(
    market: Market,
    request: Request,
    bid: Bid,
    wallet: Connected,
) -> Result<FillStatus, String> {
    let key = pending_key(&market, wallet.address);
    with_fill_lock(
        &key,
        execute_locked(market, request, bid, wallet, key.clone()),
    )
    .await
}

async fn execute_locked(
    market: Market,
    request: Request,
    bid: Bid,
    wallet: Connected,
    key: String,
) -> Result<FillStatus, String> {
    if request.terms.writer != wallet.address
        || request.id != bid.id
        || bid.valid_until <= web::now()
    {
        return Err("The reviewed quote is no longer valid".into());
    }
    let storage = recovery_storage()?;
    if storage
        .get_item(&key)
        .map_err(|_| "Could not read transaction recovery storage")?
        .is_some()
    {
        return Err(PENDING_FILL.into());
    }
    let quote = api::quote(&request.terms, &market, &bid).ok_or("Invalid quote terms")?;
    let signature: [u8; 64] = STANDARD
        .decode(&bid.signature)
        .map_err(|_| "Invalid maker signature")?
        .try_into()
        .map_err(|_| "Invalid maker signature length")?;
    VerifyingKey::from_bytes(&quote.maker)
        .map_err(|_| "Invalid maker key")?
        .verify_strict(&quote.digest(), &Signature::from_bytes(&signature))
        .map_err(|_| "Invalid maker signature")?;
    let instructions = prepare_fill(&quote, &signature).await?;
    let (signature, wire, last_valid) =
        signed_transaction(&wallet, &market.cluster, &instructions).await?;
    if bid.valid_until <= web::now() {
        return Err("Quote expired during wallet review".into());
    }
    let position = stryk_client::position(&market.address, &wallet.address, &quote.rfq_id);
    let pending = Pending {
        signature: signature.clone(),
        position: position.to_string(),
        last_valid,
        wire,
        quote_deadline: bid.valid_until,
    };
    let record = serde_json::to_string(&pending).map_err(|error| error.to_string())?;
    storage
        .set_item(&key, &record)
        .map_err(|_| "Failed to save the pending transaction")?;
    if let Err(error) = retry_pending(&pending).await {
        if error["code"].as_i64() == Some(-32002)
            && error["data"]["err"].get("InstructionError").is_some()
        {
            let _ = storage.remove_item(&key);
            return Err(
                "The fill failed preflight. Check balances and request a fresh quote.".into(),
            );
        }
    }
    recover_locked(market.address, wallet.address, &storage, &key)
        .await?
        .ok_or_else(|| "No pending transaction found".into())
}

async fn signed_transaction(
    wallet: &Connected,
    cluster: &str,
    instructions: &[Instruction],
) -> Result<(String, String, u64), String> {
    let block = rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}])).await?;
    let hash: Address = block["value"]["blockhash"]
        .as_str()
        .ok_or("Missing blockhash")?
        .parse()
        .map_err(|_| "Invalid blockhash")?;
    let last_valid = block["value"]["lastValidBlockHeight"]
        .as_u64()
        .ok_or("Missing block height")?;
    let transaction = stryk_client::unsigned(&wallet.address, hash.to_bytes(), instructions);
    let unsigned = stryk_client::wire(&transaction).map_err(|error| error.to_string())?;
    if unsigned.len() > 1232 {
        return Err("Transaction exceeds the legacy packet limit".into());
    }
    let signed = wallet.sign(cluster, &unsigned).await?;
    let signature = stryk_client::checked_signed_wire(&signed, &transaction)
        .ok_or("Wallet changed the reviewed transaction")?;
    Ok((signature, STANDARD.encode(signed), last_valid))
}

async fn prepare_fill(
    quote: &stryk_core::Quote,
    signature: &[u8; 64],
) -> Result<Vec<Instruction>, String> {
    let payer = Address::new_from_array(quote.writer);
    let mut instructions = Vec::with_capacity(4);
    for mint in [quote.base_mint, quote.quote_mint].map(Address::new_from_array) {
        let account = token_account(payer, mint).await?;
        if account.is_null() {
            instructions.push(stryk_client::prepare_ata(payer, payer, mint));
        }
    }
    instructions.extend(stryk_client::fill_instructions(quote, signature));
    Ok(instructions)
}

pub async fn wsol(
    cluster: String,
    wallet: Connected,
    amount: Option<u64>,
) -> Result<String, String> {
    if let Some(amount) = amount {
        let balance = rpc(
            "getBalance",
            json!([wallet.address.to_string(), {"commitment":"confirmed"}]),
        )
        .await?;
        let spendable = balance["value"]
            .as_u64()
            .ok_or("Native SOL balance unavailable")?
            .saturating_sub(3_000_000);
        if amount == 0 || amount > spendable {
            return Err("Insufficient native SOL for that amount, rent and fees".into());
        }
    }
    let instructions = stryk_client::wsol_instructions(wallet.address, amount);
    send_wallet_transaction(&wallet, &cluster, &instructions).await
}

pub async fn settle(
    market: Market,
    wallet: Connected,
    position: stryk_client::api::Position,
) -> Result<String, String> {
    let mut instructions = Vec::with_capacity(5);
    for owner in [position.writer, position.maker] {
        for mint in [market.base, market.quote] {
            if token_account(owner, mint).await?.is_null() {
                instructions.push(stryk_client::prepare_ata(wallet.address, owner, mint));
            }
        }
    }
    instructions.push(stryk_client::settle(
        market.address,
        position.address,
        position.expiry,
        position.writer,
        position.maker,
        market.base,
        market.quote,
    ));
    send_wallet_transaction(&wallet, &market.cluster, &instructions).await
}

async fn send_wallet_transaction(
    wallet: &Connected,
    cluster: &str,
    instructions: &[Instruction],
) -> Result<String, String> {
    let (signature, wire, _) = signed_transaction(wallet, cluster, instructions).await?;
    let result = rpc(
        "sendTransaction",
        json!([wire, {"encoding":"base64","preflightCommitment":"confirmed"}]),
    )
    .await;
    if !result
        .as_ref()
        .is_ok_and(|value| value.as_str() == Some(&signature))
    {
        return Err(format!(
            "Wallet transaction submission uncertain · {signature}"
        ));
    }
    for _ in 0..20 {
        let status = rpc("getSignatureStatuses", json!([[signature]])).await?;
        let status = &status["value"][0];
        if matches!(
            status["confirmationStatus"].as_str(),
            Some("confirmed" | "finalized")
        ) {
            return if status["err"].is_null() {
                Ok(format!("Transaction confirmed · {signature}"))
            } else {
                Err(format!("Wallet transaction failed · {signature}"))
            };
        }
        TimeoutFuture::new(500).await;
    }
    Ok(format!("Wallet transaction submitted · {signature}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
    signature: String,
    position: String,
    last_valid: u64,
    #[serde(default)]
    wire: String,
    #[serde(default)]
    quote_deadline: i64,
}

fn pending_key(market: &Market, writer: Address) -> String {
    format!(
        "stryk/pending/{}/{}/{writer}",
        market.cluster, market.domain
    )
}

async fn with_fill_lock<T: 'static>(
    key: &str,
    operation: impl Future<Output = Result<T, String>> + 'static,
) -> Result<T, String> {
    let window = web_sys::window().ok_or("Browser unavailable")?;
    // web-sys currently gates its typed Web Locks bindings behind an unstable cfg.
    let locks = Reflect::get(&window.navigator(), &"locks".into())
        .ok()
        .filter(|locks| locks.is_object())
        .ok_or("Fills require Web Locks. Open Stryk over HTTPS in a supported browser.")?;
    let request = Reflect::get(&locks, &"request".into())
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
        .ok_or("Browser lock request unavailable")?;
    let options = Object::new();
    Reflect::set(&options, &"ifAvailable".into(), &JsValue::TRUE)
        .map_err(|_| "Could not configure the browser lock")?;
    let result = Rc::new(Cell::new(None));
    let output = result.clone();
    let callback = Closure::once(move |lock: JsValue| -> Promise {
        if lock.is_null() {
            return Promise::resolve(&JsValue::UNDEFINED);
        }
        future_to_promise(async move {
            output.set(Some(operation.await));
            Ok(JsValue::UNDEFINED)
        })
    });
    let promise = request
        .call3(&locks, &key.into(), &options, callback.as_ref())
        .and_then(JsValue::dyn_into::<Promise>)
        .map_err(|_| "Could not request the wallet fill lock")?;
    JsFuture::from(promise)
        .await
        .map_err(|_| "Wallet fill lock failed")?;
    result.take().ok_or(FILL_BUSY)?
}

fn recovery_storage() -> Result<Storage, String> {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .ok_or_else(|| "Transaction recovery storage is unavailable".into())
}

pub fn has_pending(market: &Market, writer: Address) -> bool {
    recovery_storage()
        .ok()
        .and_then(|storage| {
            storage
                .get_item(&pending_key(market, writer))
                .ok()
                .flatten()
        })
        .is_some()
}

pub async fn recover(market: Market, writer: Address) -> Result<Option<FillStatus>, String> {
    let key = pending_key(&market, writer);
    let lock_key = key.clone();
    with_fill_lock(&lock_key, async move {
        recover_locked(market.address, writer, &recovery_storage()?, &key).await
    })
    .await
}

async fn recover_locked(
    market: Address,
    writer: Address,
    storage: &Storage,
    key: &str,
) -> Result<Option<FillStatus>, String> {
    let Some(data) = storage
        .get_item(key)
        .map_err(|_| "Could not read transaction recovery storage")?
    else {
        return Ok(None);
    };
    let pending: Pending =
        serde_json::from_str(&data).map_err(|_| "Invalid pending transaction record")?;
    for attempt in 0..24 {
        let statuses = rpc(
            "getSignatureStatuses",
            json!([[pending.signature], {"searchTransactionHistory":true}]),
        )
        .await
        .map_err(|_| format!("Submission uncertain · {}", pending.signature))?;
        let status = statuses["value"]
            .as_array()
            .and_then(|statuses| statuses.first())
            .ok_or_else(|| format!("Submission uncertain · {}", pending.signature))?;
        let commitment = status["confirmationStatus"].as_str();
        if commitment == Some("finalized") && !status["err"].is_null() {
            let _ = storage.remove_item(key);
            return Err(format!("Transaction failed · {}", pending.signature));
        }
        if commitment == Some("finalized") {
            let _ = storage.remove_item(key);
            return Ok(Some(FillStatus {
                message: format!("Finalized · {}", pending.signature),
                position: pending.position,
                finalized: true,
            }));
        }
        if commitment == Some("confirmed") && status["err"].is_null() {
            return Ok(Some(FillStatus {
                message: format!("Confirmed · {} · Finality pending", pending.signature),
                position: pending.position,
                finalized: false,
            }));
        }
        let height = rpc("getBlockHeight", json!([{"commitment":"finalized"}]))
            .await
            .map_err(|_| format!("Submission uncertain · {}", pending.signature))?;
        if height
            .as_u64()
            .is_some_and(|height| height > pending.last_valid)
        {
            let response = rpc(
                "getAccountInfo",
                json!([pending.position, {"commitment":"finalized","encoding":"base64"}]),
            )
            .await
            .map_err(|_| format!("Submission uncertain · {}", pending.signature))?;
            let account = response
                .get("value")
                .ok_or_else(|| format!("Submission uncertain · {}", pending.signature))?;
            if canonical_position(account, writer, market, &pending.position) {
                let _ = storage.remove_item(key);
                return Ok(Some(FillStatus {
                    message: format!("Finalized · {}", pending.position),
                    position: pending.position,
                    finalized: true,
                }));
            }
            // The position can be pre-funded under the System Program before an unfilled RFQ.
            if account.is_null()
                || account["owner"].as_str() == Some(&stryk_client::SYSTEM.to_string())
            {
                let _ = storage.remove_item(key);
                return Err("Transaction expired. Request a fresh quote.".into());
            }
            return Err(format!("Submission uncertain · {}", pending.signature));
        }
        if status.is_null()
            && attempt % 4 == 0
            && !pending.wire.is_empty()
            && web::now() < pending.quote_deadline
        {
            // Only retry the already approved transaction. A duplicate submission has the same signature.
            let _ = retry_pending(&pending).await;
        }
        TimeoutFuture::new(500).await;
    }
    Err(format!(
        "Submitted · {} · Check status to reconcile",
        pending.signature
    ))
}

fn canonical_position(account: &Value, writer: Address, market: Address, position: &str) -> bool {
    if account["owner"].as_str() != Some(&stryk_client::ID.to_string()) {
        return false;
    }
    let Some(data) = account["data"][0]
        .as_str()
        .and_then(|value| STANDARD.decode(value).ok())
    else {
        return false;
    };
    let Some(rfq_id) = data.get(161..177).and_then(|bytes| bytes.try_into().ok()) else {
        return false;
    };
    data.len() == 260
        && data[0] == 5
        && data[1..33] == *market.as_ref()
        && data[33..65] == *writer.as_ref()
        && stryk_client::position(&market, &writer, rfq_id).to_string() == position
}

pub async fn rpc(method: &str, params: Value) -> Result<Value, String> {
    rpc_result(method, params)
        .await?
        .map_err(|error| error.to_string())
}

async fn retry_pending(pending: &Pending) -> Result<Value, Value> {
    rpc_result(
        "sendTransaction",
        json!([pending.wire, {"encoding":"base64","preflightCommitment":"confirmed","maxRetries":0}]),
    ).await.map_err(|error| json!(error))?
}

async fn rpc_result(method: &str, params: Value) -> Result<Result<Value, Value>, String> {
    let value: Value = reqwest::Client::new()
        .post(web::absolute("/v1/solana")?)
        .json(&json!({"method":method,"params":params}))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())?;
    if let Some(error) = value.get("error") {
        return Ok(Err(error.clone()));
    }
    value
        .get("result")
        .cloned()
        .map(Ok)
        .ok_or_else(|| "Missing RPC response".into())
}

pub async fn token_balance(wallet: Address, mint: Address) -> Result<u64, String> {
    let account = token_account(wallet, mint).await?;
    if account.is_null() {
        return Ok(0);
    }
    if account["owner"].as_str() != Some(&stryk_client::TOKEN.to_string()) {
        return Err("Unexpected token program".into());
    }
    let bytes = STANDARD
        .decode(account["data"][0].as_str().ok_or("Missing token balance")?)
        .map_err(|_| "Invalid token balance")?;
    let amount = bytes.get(64..72).ok_or("Truncated token balance")?;
    if bytes.get(..32) != Some(mint.as_ref()) || bytes.get(32..64) != Some(wallet.as_ref()) {
        return Err("Unexpected token account".into());
    }
    Ok(u64::from_le_bytes(
        amount.try_into().map_err(|_| "Invalid token balance")?,
    ))
}

async fn token_account(wallet: Address, mint: Address) -> Result<Value, String> {
    let ata = stryk_client::ata(&wallet, &mint);
    let result = rpc(
        "getAccountInfo",
        json!([ata.to_string(), {"commitment":"confirmed","encoding":"base64"}]),
    )
    .await?;
    result
        .get("value")
        .cloned()
        .ok_or_else(|| "Missing token account response".into())
}

pub async fn fill_cost(
    market: &Market,
    request: &Request,
    bid: &Bid,
) -> Result<(u64, u64), String> {
    let quote = api::quote(&request.terms, market, bid).ok_or("Invalid quote terms")?;
    let signature: [u8; 64] = STANDARD
        .decode(&bid.signature)
        .map_err(|_| "Invalid maker signature")?
        .try_into()
        .map_err(|_| "Invalid maker signature")?;
    let block = rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}])).await?;
    let hash: Address = block["value"]["blockhash"]
        .as_str()
        .ok_or("Missing blockhash")?
        .parse()
        .map_err(|_| "Invalid blockhash")?;
    let instructions = prepare_fill(&quote, &signature).await?;
    let missing_accounts = instructions.len() - 2;
    let payer = request.terms.writer;
    let transaction = stryk_client::unsigned(&payer, hash.to_bytes(), &instructions);
    let message = transaction.message.serialize();
    let fee = rpc(
        "getFeeForMessage",
        json!([STANDARD.encode(message), {"commitment":"confirmed"}]),
    )
    .await?;
    let position = rpc("getMinimumBalanceForRentExemption", json!([260])).await?;
    let ata = rpc("getMinimumBalanceForRentExemption", json!([165])).await?;
    let rent = position
        .as_u64()
        .and_then(|position| {
            position.checked_add(ata.as_u64()?.checked_mul(2 + missing_accounts as u64)?)
        })
        .ok_or("Rent estimate unavailable")?;
    Ok((
        fee["value"].as_u64().ok_or("Fee estimate unavailable")?,
        rent,
    ))
}
