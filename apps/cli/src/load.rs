use anyhow::{ensure, Context, Result};
use reqwest::{Client, StatusCode, Url};
use solana_address::Address;
use std::time::Duration;
use stryk_client::api::{Auction, Market, Request, Terms, VERSION};
use tokio::{task::JoinSet, time::Instant};

#[tokio::main(flavor = "current_thread")]
pub async fn run(
    service: &str,
    expiry: Address,
    writer: Address,
    winner: Address,
    rate: u64,
    seconds: u64,
    overload: bool,
) -> Result<()> {
    ensure!(
        (1..=400).contains(&rate) && (1..=90).contains(&seconds),
        "load bounds exceeded"
    );
    let service = Url::parse(service)?;
    ensure!(
        service.host_str() == Some("127.0.0.1"),
        "load requires an IPv4 loopback service"
    );
    let client = Client::builder().timeout(Duration::from_secs(3)).build()?;
    let markets: Vec<Market> = client
        .get(service.join("/v1/markets")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let market = markets.first().context("missing market")?;
    ensure!(
        market.cluster == "solana:localnet",
        "load requires the local fixture market"
    );

    let started = Instant::now();
    let mut tasks = JoinSet::new();
    let mut latencies = Vec::new();
    let mut full = 0;
    let mut errors = 0;
    for sequence in 0..rate * seconds {
        tokio::time::sleep_until(started + Duration::from_micros(sequence * 1_000_000 / rate))
            .await;
        let terms = Terms {
            version: VERSION,
            market: market.address,
            expiry,
            writer,
            kind: (sequence % 2) as u8,
            strike: 150_000_000,
            quantity: 1_000_000_000,
        };
        tasks.spawn(sample(client.clone(), service.clone(), terms, winner));
        while let Some(result) = tasks.try_join_next() {
            record(result?, &mut latencies, &mut full, &mut errors);
        }
    }
    while let Some(result) = tasks.join_next().await {
        record(result?, &mut latencies, &mut full, &mut errors);
    }
    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    println!(
        "{}",
        serde_json::json!({
            "rate":rate, "seconds":seconds, "requests":rate * seconds,
            "quoted":latencies.len(), "over_capacity":full, "failed":errors,
            "p50_ms":p50, "p95_ms":p95, "wall_ms":started.elapsed().as_millis(),
        })
    );
    ensure!(
        errors == 0 && !latencies.is_empty(),
        "load had failed or incorrect auctions"
    );
    if overload {
        ensure!(full > 0, "expected bounded admission under overload");
    } else {
        ensure!(
            full == 0 && p95.is_some_and(|p95| p95 <= 2500),
            "auction throughput target missed"
        );
    }
    Ok(())
}

fn percentile(values: &[u128], percent: usize) -> Option<u128> {
    let index = (values.len() * percent).div_ceil(100).checked_sub(1)?;
    values.get(index).copied()
}

fn record(result: Result<Option<u128>>, samples: &mut Vec<u128>, full: &mut u64, errors: &mut u64) {
    match result {
        Ok(Some(latency)) => samples.push(latency),
        Ok(None) => *full += 1,
        Err(error) => {
            *errors += 1;
            if *errors <= 5 {
                eprintln!("{error:#}");
            }
        }
    }
}

async fn sample(
    client: Client,
    service: Url,
    terms: Terms,
    winner: Address,
) -> Result<Option<u128>> {
    let start = Instant::now();
    let response = client
        .post(service.join("/v1/rfqs")?)
        .json(&terms)
        .send()
        .await?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Ok(None);
    }
    ensure!(
        response.status().is_success(),
        "auction refused: {} {}",
        response.status(),
        response.text().await?
    );
    let request: Request = response.json().await?;
    ensure!(request.terms == terms, "auction changed terms");
    tokio::time::sleep_until(start + Duration::from_millis(2050)).await;
    let url = service.join(&format!("/v1/rfqs/{}", request.id))?;
    loop {
        let result: Auction = client
            .get(url.clone())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match result {
            Auction::Quoted { bid, .. } => {
                ensure!(
                    bid.id == request.id && bid.maker == winner && bid.premium == 2_850_000,
                    "winning bid differed"
                );
                return Ok(Some(start.elapsed().as_millis()));
            }
            Auction::Pending if start.elapsed() < Duration::from_secs(3) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            _ => anyhow::bail!("auction did not return a winning bid"),
        }
    }
}
