use crate::chain::{Chain, Inventory, RpcError, MAX_SPOT_AGE};
use crate::portfolio::Store;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, State as AxumState,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::json;
use solana_address::Address;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{atomic::Ordering::Relaxed, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use stryk_client::api::{
    self, Auction, Bid, MakerInput, MakerMessage, MakerOutput, Market, Request, Terms,
};
use stryk_core::{strike_atoms, Kind};
use tokio::{
    sync::{broadcast, Mutex as AsyncMutex, Semaphore},
    time::Instant,
};

const WINDOW: Duration = Duration::from_secs(2);
const RETENTION: Duration = Duration::from_secs(60);
const MAX_BOOK: usize = 8192;
const MAX_ACTIVE_PER_IP: usize = 256;
const SNAPSHOT_AGE: Duration = Duration::from_millis(750);
const MAX_INVENTORY_KEYS: usize = 256;

type InventoryKey = (Address, Address, u16);
type InventorySnapshot = (Instant, Option<Inventory>);

#[derive(Clone)]
pub struct State(Arc<Inner>);

struct Inner {
    chain: Chain,
    market: Address,
    cluster: String,
    audience: String,
    fixture_spot: Option<u64>,
    market_snapshot: AsyncMutex<Option<(Instant, Market)>>,
    expiry_snapshot: AsyncMutex<Option<(Instant, HashMap<Address, i64>)>>,
    inventories: AsyncMutex<HashMap<InventoryKey, InventorySnapshot>>,
    requests: broadcast::Sender<Request>,
    fills: broadcast::Sender<api::Position>,
    portfolio: Option<Store>,
    auctions: Mutex<HashMap<u128, Entry>>,
    indications: Mutex<HashMap<Address, api::Indication>>,
    sockets: Semaphore,
    rpc_requests: Semaphore,
}

struct Entry {
    request: Request,
    market: Market,
    origin: IpAddr,
    close: Instant,
    best: Option<(Bid, u64, Instant)>,
    result_logged: bool,
}

pub struct ApiError(StatusCode, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"code":self.1}))).into_response()
    }
}

impl State {
    pub fn new(
        chain: Chain,
        market: Address,
        cluster: String,
        audience: String,
        fixture_spot: Option<u64>,
        fills: broadcast::Sender<api::Position>,
        portfolio: Option<Store>,
    ) -> Self {
        Self(Arc::new(Inner {
            chain,
            market,
            cluster,
            audience,
            fixture_spot,
            market_snapshot: AsyncMutex::new(None),
            expiry_snapshot: AsyncMutex::new(None),
            inventories: AsyncMutex::new(HashMap::new()),
            requests: broadcast::channel(512).0,
            fills,
            portfolio,
            auctions: Mutex::new(HashMap::new()),
            indications: Mutex::new(HashMap::new()),
            sockets: Semaphore::new(32),
            rpc_requests: Semaphore::new(64),
        }))
    }

    async fn market(&self) -> Result<Market, ApiError> {
        let now = millis() / 1_000;
        let mut snapshot = self.0.market_snapshot.lock().await;
        if snapshot
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= SNAPSHOT_AGE)
        {
            let fresh = self
                .0
                .chain
                .market(self.0.market, self.0.cluster.clone(), now)
                .await
                .map_err(rpc_error)?;
            *snapshot = Some((Instant::now(), fresh));
        }
        let mut market = snapshot.as_ref().expect("market snapshot").1.clone();
        market.spot = market
            .spot
            .filter(|spot| now.saturating_sub(spot.observed_at) <= MAX_SPOT_AGE);
        if let Some(price) = self.0.fixture_spot.filter(|price| *price > 0) {
            market.spot = Some(api::Spot {
                price,
                observed_at: now,
            });
        }
        if market.spot.is_some() && !market.paused {
            let mut indications = self.0.indications.lock().expect("indication lock");
            indications.retain(|_, indication| indication.valid_until > now);
            market.indication = indications
                .values()
                .max_by_key(|row| row.premium_bps)
                .cloned();
        }
        Ok(market)
    }

    async fn expiry(&self, address: Address) -> Result<Option<i64>, ApiError> {
        let mut snapshot = self.0.expiry_snapshot.lock().await;
        if snapshot
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= SNAPSHOT_AGE)
        {
            let fresh = self
                .0
                .chain
                .open_expiries(self.0.market)
                .await
                .map_err(rpc_error)?;
            *snapshot = Some((Instant::now(), fresh));
        }
        Ok(snapshot
            .as_ref()
            .and_then(|(_, entries)| entries.get(&address))
            .copied())
    }

    async fn eligible(
        &self,
        market: &Market,
        terms: &Terms,
        bid: &Bid,
    ) -> anyhow::Result<Option<u64>> {
        let key = (terms.expiry, bid.maker, bid.lane_id);
        let mut inventories = self.0.inventories.lock().await;
        if let Some((at, inventory)) = inventories.get(&key) {
            if at.elapsed() < SNAPSHOT_AGE {
                return Ok(inventory
                    .as_ref()
                    .and_then(|row| row.eligible_slot(terms, bid.premium)));
            }
        }
        inventories.retain(|_, (at, _)| at.elapsed() < SNAPSHOT_AGE);
        if inventories.len() >= MAX_INVENTORY_KEYS {
            return Ok(None);
        }
        let inventory = self.0.chain.inventory(market, key.0, key.1, key.2).await?;
        let slot = inventory
            .as_ref()
            .and_then(|row| row.eligible_slot(terms, bid.premium));
        inventories.insert(key, (Instant::now(), inventory));
        Ok(slot)
    }
}

fn rpc_error(error: anyhow::Error) -> ApiError {
    tracing::warn!(%error, "chain read failed");
    ApiError(StatusCode::SERVICE_UNAVAILABLE, "chain_unavailable")
}

fn millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn markets(AxumState(state): AxumState<State>) -> Result<Json<Vec<Market>>, ApiError> {
    Ok(Json(vec![state.market().await?]))
}

pub async fn ready(
    AxumState(state): AxumState<State>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let market = state.market().await?;
    if market.spot.is_none() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "spot_unavailable",
        ));
    }
    Ok(Json(
        json!({"market":market.address.to_string(),"slot":market.as_of_slot}),
    ))
}

pub async fn metrics(AxumState(state): AxumState<State>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.0.chain.ops.render(state.0.requests.receiver_count()),
    )
}

pub async fn solana(
    AxumState(state): AxumState<State>,
    Json(request): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let method = request["method"].as_str().unwrap_or_default();
    let params = request["params"]
        .as_array()
        .filter(|params| params.len() <= 3)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid_rpc_params"))?;
    match method {
        "getLatestBlockhash"
        | "getBlockHeight"
        | "getSignatureStatuses"
        | "getAccountInfo"
        | "getBalance"
        | "getMinimumBalanceForRentExemption"
        | "getFeeForMessage" => {}
        "sendTransaction"
            if params
                .first()
                .and_then(|value| value.as_str())
                .filter(|bytes| bytes.len() <= 1644)
                .and_then(|bytes| STANDARD.decode(bytes).ok())
                .is_some_and(|bytes| {
                    stryk_client::is_fill_wire(&bytes, &state.0.market)
                        || stryk_client::is_wsol_wire(&bytes)
                        || stryk_client::is_settlement_wire(&bytes, &state.0.market)
                }) => {}
        _ => return Err(ApiError(StatusCode::BAD_REQUEST, "unsupported_rpc_method")),
    }
    let _permit = state
        .0
        .rpc_requests
        .try_acquire()
        .map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "rpc_capacity"))?;
    let response = state.0.chain.rpc(method, request["params"].clone()).await;
    match response {
        Ok(value) => Ok(Json(json!({"result":value}))),
        Err(error) => match error.downcast::<RpcError>() {
            Ok(error) => Ok(Json(json!({"error":error.0}))),
            Err(error) => Err(rpc_error(error)),
        },
    }
}

pub async fn expiries(
    AxumState(state): AxumState<State>,
    Path(address): Path<String>,
) -> Result<Json<Vec<api::Expiry>>, ApiError> {
    if address != state.0.market.to_string() {
        return Err(ApiError(StatusCode::NOT_FOUND, "unknown_market"));
    }
    Ok(Json(
        state
            .0
            .chain
            .expiries(state.0.market)
            .await
            .map_err(rpc_error)?,
    ))
}

pub async fn expiry_state(
    AxumState(state): AxumState<State>,
    Path(address): Path<String>,
) -> Result<Json<api::ExpiryState>, ApiError> {
    let expiry = address
        .parse()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid_expiry"))?;
    let status = state
        .0
        .chain
        .expiry_state(expiry, state.0.market)
        .await
        .map_err(rpc_error)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown_expiry"))?;
    Ok(Json(status))
}

pub async fn request(
    AxumState(state): AxumState<State>,
    ConnectInfo(origin): ConnectInfo<SocketAddr>,
    Json(terms): Json<Terms>,
) -> Result<Json<Request>, ApiError> {
    let now = millis();
    let market = state.market().await?;
    if market.paused || market.spot.is_none() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "quoting_unavailable",
        ));
    }
    if terms.version != api::VERSION
        || terms.market != market.address
        || terms.writer == stryk_client::SYSTEM
        || Kind::try_from(terms.kind).is_err()
        || !(market.min_quantity..=market.max_quantity).contains(&terms.quantity)
        || !terms.strike.is_multiple_of(market.strike_tick)
        || strike_atoms(terms.quantity, terms.strike).is_none()
    {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_terms"));
    }
    let expiry = state
        .expiry(terms.expiry)
        .await?
        .ok_or(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "unknown_expiry"))?;
    let cutoff = expiry - i64::from(market.fill_cutoff);
    let now = now.max(millis());
    if cutoff <= now / 1_000 + 5 {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "expiry_closed"));
    }
    let close = Instant::now() + WINDOW;
    let mut book = state.0.auctions.lock().expect("auction lock");
    book.retain(|_, entry| close.saturating_duration_since(entry.close) < RETENTION);
    let active = book
        .values()
        .filter(|entry| entry.origin == origin.ip() && entry.close > Instant::now())
        .count();
    if book.len() >= MAX_BOOK || active >= MAX_ACTIVE_PER_IP {
        state.0.chain.ops.rfq_rejected.fetch_add(1, Relaxed);
        return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "rfq_capacity"));
    }
    let id = loop {
        let id = rand::random::<u128>();
        if !book.contains_key(&id) {
            break id;
        }
    };
    let request = Request {
        terms,
        id,
        deadline_ms: now + WINDOW.as_millis() as i64,
        max_valid_until: cutoff,
    };
    book.insert(
        id,
        Entry {
            request: request.clone(),
            market,
            origin: origin.ip(),
            close,
            best: None,
            result_logged: false,
        },
    );
    drop(book);
    let _ = state.0.requests.send(request.clone());
    tracing::debug!(rfq_id = %id, %expiry, "RFQ opened");
    Ok(Json(request))
}

pub async fn get(
    AxumState(state): AxumState<State>,
    Path(id): Path<u128>,
) -> Result<Json<Auction>, ApiError> {
    let (writer, bid, inventory_slot, spot) = {
        let mut book = state.0.auctions.lock().expect("auction lock");
        let entry = book
            .get_mut(&id)
            .filter(|entry| Instant::now().saturating_duration_since(entry.close) < RETENTION)
            .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown_rfq"))?;
        if Instant::now() < entry.close {
            return Ok(Json(Auction::Pending));
        }
        if !entry.result_logged {
            let counts = &state.0.chain.ops;
            let has_bid = entry
                .best
                .as_ref()
                .is_some_and(|(bid, _, _)| bid.valid_until > millis() / 1_000);
            if has_bid {
                counts.rfq_quoted.fetch_add(1, Relaxed);
            } else {
                counts.rfq_no_quote.fetch_add(1, Relaxed);
            }
            tracing::debug!(rfq_id = %id, quoted = has_bid, "RFQ closed");
            entry.result_logged = true;
        }
        let Some((bid, slot, _)) = entry
            .best
            .as_ref()
            .filter(|(bid, _, _)| bid.valid_until > millis() / 1_000)
        else {
            return Ok(Json(Auction::NoQuote));
        };
        (
            entry.request.terms.writer,
            bid.clone(),
            *slot,
            entry.market.spot.clone(),
        )
    };
    if let (Some(store), Some(spot)) = (&state.0.portfolio, &spot) {
        let position = stryk_client::position(&state.0.market, &writer, &id.to_le_bytes());
        if let Err(error) = store
            .remember_quote_spot(position, bid.valid_until, spot)
            .await
        {
            tracing::warn!(%error, %id, %position, "could not preserve indicative quote spot");
        }
    }
    if bid.valid_until <= millis() / 1_000 {
        return Ok(Json(Auction::NoQuote));
    }
    Ok(Json(Auction::Quoted {
        bid,
        inventory_slot,
        spot,
    }))
}

pub async fn maker(AxumState(state): AxumState<State>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(2048)
        .max_frame_size(2048)
        .on_upgrade(move |socket| async move {
            if let Ok(_permit) = state.0.sockets.try_acquire() {
                if let Err(error) = connected(&state, socket).await {
                    tracing::debug!(%error, "maker disconnected");
                }
            }
        })
}

async fn connected(state: &State, mut socket: WebSocket) -> anyhow::Result<()> {
    let challenge = format!(
        "STRYK_MAKER_V1\n{}\n{:032x}",
        state.0.audience,
        rand::random::<u128>()
    );
    send(
        &mut socket,
        MakerOutput::Challenge {
            challenge: challenge.clone(),
        },
    )
    .await?;
    let MakerInput::Authenticate { maker, signature } =
        tokio::time::timeout(Duration::from_secs(5), recv(&mut socket)).await??
    else {
        anyhow::bail!("maker authentication required");
    };
    let maker: Address = maker.parse()?;
    anyhow::ensure!(
        verify(maker, challenge.as_bytes(), &signature),
        "maker signature rejected"
    );
    send(&mut socket, MakerOutput::Ready).await?;
    let MakerInput::Subscribe { market } =
        tokio::time::timeout(Duration::from_secs(5), recv(&mut socket)).await??
    else {
        anyhow::bail!("market subscription required");
    };
    anyhow::ensure!(market == state.0.market.to_string(), "unknown market");
    let mut requests = state.0.requests.subscribe();
    let mut fills = state.0.fills.subscribe();
    let mut checks = tokio::task::JoinSet::new();
    let mut indications = tokio::time::interval(Duration::from_secs(10));
    indications.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut indication_pending = false;
    loop {
        tokio::select! {
            _ = indications.tick() => {
                send(&mut socket, MakerOutput::RequestIndication).await?;
                indication_pending = true;
            }
            request = requests.recv() => {
                send(&mut socket, MakerOutput::Rfq { request: request? }).await?;
            }
            notice = fills.recv() => {
                match notice {
                    Ok(position) if position.maker == maker => send(&mut socket, MakerOutput::Filled { position: Box::new(position) }).await?,
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
            input = recv(&mut socket) => {
                let bid = match input? {
                    MakerInput::Indication { lane_id, premium_bps }
                        if indication_pending
                            && (1..=10_000).contains(&premium_bps) =>
                    {
                        indication_pending = false;
                        if !state.0.chain.registered(state.0.market, maker, lane_id).await.unwrap_or(false) {
                            continue;
                        }
                        let observed_at = millis() / 1_000;
                        let mut indications = state.0.indications.lock().expect("indication lock");
                        indications.retain(|_, row| row.valid_until > observed_at);
                        if indications.len() < 32 || indications.contains_key(&maker) {
                            indications.insert(maker, api::Indication { premium_bps, observed_at, valid_until: observed_at + 20 });
                        }
                        continue;
                    }
                    MakerInput::Quote { bid } => bid,
                    _ => continue,
                };
                if checks.len() < 32 && bid.maker == maker {
                    let state = state.clone();
                    let received = Instant::now();
                    checks.spawn(async move {
                        let id = bid.id;
                        let result = submit(&state, bid, received).await;
                        (id, result)
                    });
                } else {
                    let rejection = if bid.maker != maker { api::BidRejection::WrongMaker } else { api::BidRejection::QueueFull };
                    send(&mut socket, MakerOutput::Acknowledged { id: bid.id.to_string(), accepted: false, rejection: Some(rejection) }).await?;
                }
            }
            Some(checked) = checks.join_next(), if !checks.is_empty() => {
                let (id, result) = checked?;
                send(&mut socket, MakerOutput::Acknowledged { id: id.to_string(), accepted: result.is_ok(), rejection: result.err() }).await?;
            }
        }
    }
    Ok(())
}

async fn recv(socket: &mut WebSocket) -> anyhow::Result<MakerInput> {
    while let Some(message) = socket.recv().await {
        match message? {
            Message::Text(text) => {
                let message: MakerMessage<MakerInput> = serde_json::from_str(&text)?;
                anyhow::ensure!(
                    message.version == api::VERSION,
                    "unsupported maker protocol version"
                );
                return Ok(message.message);
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    anyhow::bail!("maker websocket closed")
}

async fn send(socket: &mut WebSocket, message: MakerOutput) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(1),
        socket.send(Message::Text(
            serde_json::to_string(&MakerMessage {
                version: api::VERSION,
                message,
            })?
            .into(),
        )),
    )
    .await??;
    Ok(())
}

async fn submit(state: &State, bid: Bid, received: Instant) -> Result<(), api::BidRejection> {
    use api::BidRejection;
    let (request, market, close) = {
        let book = state.0.auctions.lock().expect("auction lock");
        let Some(entry) = book.get(&bid.id).filter(|entry| received < entry.close) else {
            return Err(BidRejection::AuctionClosed);
        };
        (entry.request.clone(), entry.market.clone(), entry.close)
    };
    let now = millis() / 1_000;
    if bid.premium == 0
        || bid.maker == request.terms.writer
        || bid.valid_until <= now + 2
        || bid.valid_until > (now + 45).min(request.max_valid_until)
    {
        return Err(BidRejection::InvalidTerms);
    }
    let Some(quote) = api::quote(&request.terms, &market, &bid) else {
        return Err(BidRejection::InvalidTerms);
    };
    if !verify(bid.maker, &quote.digest(), &bid.signature) {
        return Err(BidRejection::InvalidSignature);
    }
    let slot = tokio::time::timeout_at(close, state.eligible(&market, &request.terms, &bid))
        .await
        .map_err(|_| BidRejection::AuctionClosed)?
        .map_err(|_| BidRejection::InventoryUnavailable)?
        .ok_or(BidRejection::InsufficientInventoryOrCap)?;
    let mut book = state.0.auctions.lock().expect("auction lock");
    let Some(entry) = book
        .get_mut(&bid.id)
        .filter(|entry| Instant::now() < entry.close)
    else {
        return Err(BidRejection::AuctionClosed);
    };
    let leads = entry.best.as_ref().is_none_or(|(before, _, arrival)| {
        bid.premium > before.premium
            || (bid.premium == before.premium && (received, bid.maker) < (*arrival, before.maker))
    });
    if leads {
        entry.best = Some((bid, slot, received));
        Ok(())
    } else {
        Err(BidRejection::Outbid)
    }
}

fn verify(key: Address, message: &[u8], signature: &str) -> bool {
    let Ok(signature) = STANDARD.decode(signature).and_then(|bytes| {
        bytes
            .try_into()
            .map_err(|_| base64::DecodeError::InvalidLength(0))
    }) else {
        return false;
    };
    VerifyingKey::from_bytes(key.as_array()).is_ok_and(|key| {
        key.verify_strict(message, &Signature::from_bytes(&signature))
            .is_ok()
    })
}
