use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use std::time::{SystemTime, UNIX_EPOCH};
use stryk_client::api::{
    self, Auction, Bid, MakerInput, MakerMessage, MakerOutput, Market, Request, Terms,
};
use stryk_core::{strike_atoms, Kind};
use tungstenite::{connect, Message, WebSocket};

pub fn run(
    service: &str,
    key: &Keypair,
    audience: &str,
    expected_market: Address,
    expected_domain: Address,
    lane_id: u16,
    premium_bps: u16,
) -> Result<()> {
    ensure!(
        (1..=10_000).contains(&premium_bps),
        "premium bps outside bounds"
    );
    let http = reqwest::Url::parse(service)?;
    let Some(scheme) = (match http.scheme() {
        "https" => Some("wss"),
        "http" if matches!(http.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => {
            Some("ws")
        }
        _ => None,
    }) else {
        anyhow::bail!("use HTTPS for a remote maker service");
    };
    let markets: Vec<Market> = reqwest::blocking::get(http.join("/v1/markets")?)?
        .error_for_status()?
        .json()?;
    let market = markets
        .iter()
        .find(|market| market.address == expected_market && market.domain == expected_domain)
        .context("service market or deployment domain differs from maker configuration")?;
    let mut websocket = http.join("/v1/maker")?;
    websocket
        .set_scheme(scheme)
        .map_err(|_| anyhow!("invalid websocket scheme"))?;
    let (mut socket, _) = connect(websocket.as_str())?;
    let MakerOutput::Challenge { challenge } = receive(&mut socket)? else {
        anyhow::bail!("expected maker challenge");
    };
    let prefix = format!("STRYK_MAKER_V1\n{audience}\n");
    ensure!(
        challenge.strip_prefix(&prefix).is_some_and(
            |nonce| nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        ),
        "unexpected maker authentication audience or nonce"
    );
    let maker = key.pubkey();
    send(
        &mut socket,
        MakerInput::Authenticate {
            maker: maker.to_string(),
            signature: STANDARD.encode(key.sign_message(challenge.as_bytes()).as_ref()),
        },
    )?;
    ensure!(
        matches!(receive(&mut socket)?, MakerOutput::Ready),
        "maker authentication rejected"
    );
    send(
        &mut socket,
        MakerInput::Subscribe {
            market: market.address.to_string(),
        },
    )?;
    println!("Maker {maker} subscribed to {}", market.address);
    loop {
        match receive(&mut socket)? {
            MakerOutput::RequestIndication => {
                send(
                    &mut socket,
                    MakerInput::Indication {
                        lane_id,
                        premium_bps,
                    },
                )?;
            }
            MakerOutput::Rfq { request } => {
                let terms = &request.terms;
                if terms.writer == maker
                    || terms.market != market.address
                    || Kind::try_from(terms.kind).is_err()
                {
                    continue;
                }
                let Some(notional) = strike_atoms(terms.quantity, terms.strike) else {
                    continue;
                };
                let premium =
                    (u128::from(notional) * u128::from(premium_bps) / 10_000).max(1) as u64;
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
                let mut bid = Bid {
                    id: request.id,
                    maker,
                    lane_id,
                    premium,
                    valid_until: (now + 30).min(request.max_valid_until),
                    signature: String::new(),
                };
                if bid.valid_until <= now + 2 {
                    continue;
                }
                let quote = api::quote(terms, market, &bid).context("invalid RFQ terms")?;
                bid.signature = STANDARD.encode(key.sign_message(&quote.digest()).as_ref());
                send(&mut socket, MakerInput::Quote { bid })?;
            }
            MakerOutput::Acknowledged {
                id,
                accepted,
                rejection,
            } => {
                if let Some(reason) = rejection {
                    println!(
                        "{id} leads={accepted} reason={}",
                        serde_json::to_string(&reason)?
                    );
                } else {
                    println!("{id} leads={accepted}");
                }
            }
            MakerOutput::Filled { position } => println!("{}", serde_json::to_string(&position)?),
            _ => anyhow::bail!("unexpected maker message"),
        }
    }
}

fn receive(socket: &mut WebSocket<impl std::io::Read + std::io::Write>) -> Result<MakerOutput> {
    loop {
        match socket.read()? {
            Message::Text(text) => {
                let message: MakerMessage<MakerOutput> = serde_json::from_str(&text)?;
                ensure!(
                    message.version == api::VERSION,
                    "unsupported maker protocol version"
                );
                return Ok(message.message);
            }
            Message::Close(_) => anyhow::bail!("maker connection closed"),
            _ => {}
        }
    }
}

fn send(
    socket: &mut WebSocket<impl std::io::Read + std::io::Write>,
    message: MakerInput,
) -> Result<()> {
    socket.send(Message::Text(
        serde_json::to_string(&MakerMessage {
            version: api::VERSION,
            message,
        })?
        .into(),
    ))?;
    Ok(())
}

pub fn inventory(
    rpc: &crate::rpc::Rpc,
    key: &Keypair,
    market: Address,
    lane_id: u16,
    mint: Address,
    amount: u64,
    deposit: bool,
) -> Result<String> {
    let maker = key.pubkey();
    let lane = stryk_client::lane(&market, &maker, lane_id);
    let wallet = stryk_client::ata(&maker, &mint);
    let inventory = stryk_client::ata(&lane, &mint);
    let movement = if deposit {
        stryk_client::DepositInstruction {
            maker,
            market,
            lane,
            mint,
            wallet,
            inventory,
            token_program: stryk_client::TOKEN,
            amount,
        }
        .into()
    } else {
        stryk_client::WithdrawInstruction {
            maker,
            market,
            lane,
            mint,
            wallet,
            inventory,
            token_program: stryk_client::TOKEN,
            amount,
        }
        .into()
    };
    rpc.send(
        key,
        &[],
        &[
            stryk_client::prepare_ata(maker, maker, mint),
            stryk_client::prepare_ata(maker, lane, mint),
            movement,
        ],
    )
}

pub fn inspect(
    rpc: &crate::rpc::Rpc,
    maker: Address,
    market: Address,
    expiry: Address,
    lane_id: u16,
) -> Result<()> {
    let lane = stryk_client::lane(&market, &maker, lane_id);
    let market_data = rpc.stryk_account(market, 1)?;
    let base = Address::new_from_array(
        market_data
            .get(33..65)
            .context("market base mint")?
            .try_into()?,
    );
    let quote = Address::new_from_array(
        market_data
            .get(65..97)
            .context("market quote mint")?
            .try_into()?,
    );
    let capacity = rpc.stryk_account(stryk_client::capacity(&lane, &expiry), 4)?;
    let limit = u64::from_le_bytes(capacity.get(65..73).context("capacity limit")?.try_into()?);
    let filled = u64::from_le_bytes(
        capacity
            .get(73..81)
            .context("capacity filled")?
            .try_into()?,
    );
    let positions = rpc.call("getProgramAccounts", serde_json::json!([
        stryk_client::ID.to_string(), {"encoding":"base64", "commitment":"finalized",
          "filters":[{"memcmp":{"offset":0,"bytes":"BQ==","encoding":"base64"}},
                     {"memcmp":{"offset":97,"bytes":STANDARD.encode(lane.as_ref()),"encoding":"base64"}},
                     {"memcmp":{"offset":129,"bytes":STANDARD.encode(expiry.as_ref()),"encoding":"base64"}},
                     {"memcmp":{"offset":259,"bytes":"AA==","encoding":"base64"}}]}
    ]))?;
    let mut exposure = Vec::new();
    for position in positions.as_array().context("positions missing")? {
        let data = STANDARD.decode(
            position["account"]["data"][0]
                .as_str()
                .context("position data")?,
        )?;
        let value = match data.get(257) {
            Some(0) => {
                u64::from_le_bytes(data.get(225..233).context("quote collateral")?.try_into()?)
            }
            Some(1) => {
                u64::from_le_bytes(data.get(217..225).context("base collateral")?.try_into()?)
            }
            _ => anyhow::bail!("invalid position strategy"),
        };
        exposure.push(serde_json::json!({
            "position":position["pubkey"],"strategy":data[257],
            "delivery_atoms":value.to_string(),"delivery_mint":if data[257] == 0 { quote } else { base }.to_string()
        }));
    }
    let free_balance = |mint| -> Result<u64> {
        let account = stryk_client::ata(&lane, &mint);
        let Some(data) = rpc.account(account)? else {
            return Ok(0);
        };
        ensure!(
            data.get(..32) == Some(mint.as_ref()) && data.get(32..64) == Some(lane.as_ref()),
            "unexpected maker inventory token account"
        );
        Ok(u64::from_le_bytes(
            data.get(64..72).context("SPL token amount")?.try_into()?,
        ))
    };
    println!(
        "{}",
        serde_json::json!({
            "lane":lane.to_string(),"cap_limit_atoms":limit.to_string(),"cap_filled_atoms":filled.to_string(),
            "free_wsol_atoms":free_balance(base)?.to_string(),
            "free_usdc_atoms":free_balance(quote)?.to_string(),
            "finalized_open_exposure":exposure
        })
    );
    Ok(())
}

pub fn take(
    rpc: &crate::rpc::Rpc,
    service: &str,
    writer: &Keypair,
    expiry: Address,
    kind: u8,
    strike: u64,
    quantity: u64,
) -> Result<()> {
    ensure!(
        Kind::try_from(kind).is_ok(),
        "strategy must be 0 (call) or 1 (put)"
    );
    let url = reqwest::Url::parse(service)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let markets: Vec<Market> = client
        .get(url.join("/v1/markets")?)
        .send()?
        .error_for_status()?
        .json()?;
    let market = markets.first().context("market unavailable")?;
    ensure!(
        market.cluster == rpc.cluster,
        "RFQ market cluster differs from the configured RPC cluster"
    );
    let terms = Terms {
        version: api::VERSION,
        market: market.address,
        expiry,
        writer: writer.pubkey(),
        kind,
        strike,
        quantity,
    };
    let request: Request = client
        .post(url.join("/v1/rfqs")?)
        .json(&terms)
        .send()?
        .error_for_status()?
        .json()?;
    ensure!(request.terms == terms, "service returned different terms");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let bid = loop {
        ensure!(
            std::time::Instant::now() < deadline,
            "auction did not finish"
        );
        let auction: Auction = client
            .get(url.join(&format!("/v1/rfqs/{}", request.id))?)
            .send()?
            .error_for_status()?
            .json()?;
        match auction {
            Auction::Pending => std::thread::sleep(std::time::Duration::from_millis(150)),
            Auction::NoQuote => anyhow::bail!("no eligible quote"),
            Auction::Quoted { bid, .. } => break bid,
        }
    };
    ensure!(bid.id == request.id, "wrong quote request");
    let quote = api::quote(&terms, market, &bid).context("invalid bid terms")?;
    let signature: [u8; 64] = STANDARD
        .decode(&bid.signature)?
        .try_into()
        .map_err(|_| anyhow!("invalid maker signature length"))?;
    let instructions = stryk_client::fill_instructions(&quote, &signature);
    let position = stryk_client::position(&market.address, &writer.pubkey(), &quote.rfq_id);
    let signature = rpc
        .send(writer, &[], &instructions)
        .with_context(|| format!("RFQ {} position {position}", request.id))?;
    println!(
        "{}",
        serde_json::json!({
            "rfq_id":bid.id.to_string(), "maker":bid.maker.to_string(), "premium":bid.premium.to_string(),
            "position":position.to_string(), "fill":signature,
        })
    );
    Ok(())
}
