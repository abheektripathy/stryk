mod chain;
mod historical;
mod ops;
mod portfolio;
mod rfq;
mod settlement;

use anyhow::{Context, Result};
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use clap::Parser;
use solana_address::Address;
use std::{net::SocketAddr, path::PathBuf};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

#[derive(Parser)]
struct Config {
    #[arg(long, env = "STRYK_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(
        long,
        env = "STRYK_RPC",
        default_value = "http://127.0.0.1:18899",
        hide_env_values = true
    )]
    rpc: String,
    #[arg(long, env = "STRYK_ARCHIVE_RPC", hide_env_values = true)]
    archive_rpc: Option<String>,
    #[arg(long, env = "STRYK_MARKET")]
    market: Address,
    #[arg(long, env = "STRYK_AUDIENCE", default_value = "stryk-localnet")]
    audience: String,
    #[arg(long, env = "STRYK_CLUSTER", default_value = "solana:localnet")]
    cluster: String,
    #[arg(long, env = "STRYK_FIXTURE_SPOT")]
    fixture_spot: Option<u64>,
    #[arg(long, env = "STRYK_WEB_DIR")]
    web_dir: Option<PathBuf>,
    #[arg(long, env = "STRYK_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    #[arg(long, env = "STRYK_SETTLEMENT_PAYER")]
    settlement_payer: Option<PathBuf>,
    #[arg(long, env = "STRYK_FIXTURE_PROOF")]
    fixture_proof: Option<Address>,
    #[arg(long, env = "STRYK_PYTH_POSTER", requires = "settlement_payer")]
    pyth_poster: Option<PathBuf>,
    #[arg(long, env = "STRYK_PYTH_ARCHIVE", requires = "pyth_poster")]
    pyth_archive: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let config = Config::parse();
    anyhow::ensure!(
        (config.fixture_spot.is_none() && config.fixture_proof.is_none())
            || (config.cluster == "solana:localnet" && config.listen.ip().is_loopback()),
        "fixture inputs are available only on localnet through a loopback listener"
    );
    let operations = std::sync::Arc::new(ops::Operations::default());
    let chain = chain::Chain::new(config.rpc, operations.clone())?;
    let expected_genesis = match config.cluster.as_str() {
        "solana:localnet" => None,
        "solana:devnet" => Some(stryk_client::DEVNET_GENESIS),
        "solana:testnet" => Some("4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY"),
        "solana:mainnet" => Some("5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"),
        _ => anyhow::bail!("unsupported Solana cluster"),
    };
    if let Some(expected) = expected_genesis {
        let actual = chain.rpc("getGenesisHash", serde_json::json!([])).await?;
        anyhow::ensure!(
            actual.as_str() == Some(expected),
            "RPC genesis hash differs from the configured Solana cluster"
        );
    }
    let historical = if let Some(poster) = config.pyth_poster {
        anyhow::ensure!(
            config.cluster == "solana:devnet",
            "historical poster requires devnet"
        );
        let (_, accounts) = chain.accounts(&[config.market]).await?;
        let market = accounts[0]
            .as_ref()
            .context("historical poster requires a market")?
            .stryk(1)?;
        anyhow::ensure!(
            chain::read::<32>(market, 161)? == stryk_core::SOL_USD_FEED,
            "historical poster requires the SOL/USD feed"
        );
        Some(historical::Historical::new(
            std::env::var("STRYK_PYTH_API_KEY").context("STRYK_PYTH_API_KEY required")?,
            poster.canonicalize()?,
            config
                .settlement_payer
                .as_ref()
                .context("STRYK_SETTLEMENT_PAYER required")?
                .canonicalize()?,
            config
                .pyth_archive
                .context("STRYK_PYTH_ARCHIVE is required with the historical poster")?,
            Address::new_from_array(chain::read(market, 97)?),
        )?)
    } else {
        None
    };
    if let Some(path) = config.settlement_payer {
        let payer =
            solana_keypair::read_keypair_file(path).map_err(|error| anyhow::anyhow!("{error}"))?;
        tokio::spawn(settlement::run(
            chain.clone(),
            payer,
            config.market,
            config.fixture_proof,
            historical,
        ));
    }
    let (fills, _) = tokio::sync::broadcast::channel(128);
    let portfolio = match config.database_url {
        Some(url) => Some(portfolio::Store::connect(&url, config.market).await?),
        None => None,
    };
    let state = rfq::State::new(
        chain.clone(),
        config.market,
        config.cluster,
        config.audience,
        config.fixture_spot,
        fills.clone(),
        portfolio.clone(),
    );
    let mut app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(rfq::ready))
        .route("/metrics", get(rfq::metrics))
        .route("/v1/markets", get(rfq::markets))
        .route("/v1/markets/{address}/expiries", get(rfq::expiries))
        .route("/v1/expiries/{address}", get(rfq::expiry_state))
        .route("/v1/rfqs", post(rfq::request))
        .route("/v1/rfqs/{id}", get(rfq::get))
        .route("/v1/solana", post(rfq::solana))
        .route("/v1/maker", get(rfq::maker))
        .with_state(state);
    if let Some(store) = portfolio {
        let archive = config
            .archive_rpc
            .map(|url| chain::Chain::new(url, operations))
            .transpose()?;
        if let Some(archive) = &archive {
            let primary = chain.rpc("getGenesisHash", serde_json::json!([])).await?;
            let secondary = archive.rpc("getGenesisHash", serde_json::json!([])).await?;
            anyhow::ensure!(
                primary.is_string() && primary == secondary,
                "archive RPC cluster differs from primary"
            );
        }
        tokio::spawn(store.clone().run(chain, archive, fills));
        app = app.merge(
            Router::new()
                .route("/v1/positions", get(portfolio::positions))
                .route("/v1/positions/{address}", get(portfolio::position))
                .route("/v1/income/{owner}", get(portfolio::income))
                .with_state(store),
        );
    }
    if let Some(dir) = config.web_dir {
        app = app
            .fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html"))));
    }
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "Stryk listening");
    axum::serve(
        listener,
        app.layer(DefaultBodyLimit::max(8 * 1024))
            .layer(TraceLayer::new_for_http())
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
