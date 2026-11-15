use crate::{
    submit,
    wallet::{Connected, Discovery},
};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use solana_address::Address;
use stryk_client::api::{
    self, Auction, Bid, Expiry, ExpiryState, IncomePoint, Market, Portfolio, Position, Request,
    Spot, Terms,
};
use stryk_core::{strike_atoms, BASE_DECIMALS, QUOTE_DECIMALS};

const DEPLOYMENT: (Option<&str>, Option<&str>, Option<&str>, Option<&str>) = (
    option_env!("STRYK_WEB_CLUSTER"),
    option_env!("STRYK_WEB_MARKET"),
    option_env!("STRYK_WEB_DOMAIN"),
    option_env!("STRYK_WEB_QUOTE_MINT"),
);

fn deployment_matches(market: &Market) -> bool {
    if market.version != api::VERSION
        || market.base != stryk_client::WSOL
        || market.address != stryk_client::market(&market.base, &market.quote, 1)
    {
        return false;
    }
    if cfg!(feature = "local-wallet")
        && market.cluster == "solana:localnet"
        && web_sys::window()
            .and_then(|window| window.location().hostname().ok())
            .as_deref()
            == Some("127.0.0.1")
    {
        return true;
    }
    let (Some(cluster), Some(address), Some(domain), Some(quote)) = DEPLOYMENT else {
        return false;
    };
    market.cluster == cluster
        && address.parse::<Address>() == Ok(market.address)
        && domain.parse::<Address>() == Ok(market.domain)
        && quote.parse::<Address>() == Ok(market.quote)
}

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Earn {},
    #[route("/earn/sol?:strategy&:expiry&:strike")]
    Ticket {
        strategy: String,
        expiry: Option<String>,
        strike: Option<u64>,
    },
    #[route("/dashboard")]
    Dashboard {},
    #[route("/positions/:address")]
    PositionDetail { address: String },
}

#[derive(Clone)]
struct Markets(Option<Market>);

/// Why no market is usable, shown to the user instead of a silent "offline".
#[derive(Clone, PartialEq)]
struct MarketIssue(Option<String>);

fn market_issue(response: Option<&Result<Vec<Market>, String>>) -> Option<String> {
    let markets = match response? {
        Err(_) => {
            return Some("Can't reach the Stryk service at /v1/markets. Is it running?".into())
        }
        Ok(markets) => markets,
    };
    let Some(market) = markets.first() else {
        return Some("The Stryk service has no markets yet.".into());
    };
    if deployment_matches(market) {
        return market
            .paused
            .then(|| "This market is paused. Quotes and fills are disabled.".into());
    }
    let location = web_sys::window().map(|window| window.location());
    let host = location
        .as_ref()
        .and_then(|l| l.hostname().ok())
        .unwrap_or_default();
    if cfg!(feature = "local-wallet") && market.cluster == "solana:localnet" && host != "127.0.0.1"
    {
        let port = location
            .as_ref()
            .and_then(|l| l.port().ok())
            .filter(|p| !p.is_empty());
        let url = match port {
            Some(port) => format!("http://127.0.0.1:{port}"),
            None => "http://127.0.0.1".into(),
        };
        return Some(format!(
            "The local demo only trusts its market on 127.0.0.1, not {host}. Open {url} instead."
        ));
    }
    Some("This build isn't pinned to the market the service returned. Rebuild with the STRYK_WEB_* deployment values.".into())
}

#[component]
pub fn App() -> Element {
    let mut markets = use_resource(|| async { get::<Vec<Market>>("/v1/markets").await });
    let market = markets
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|markets| markets.first())
        .filter(|market| deployment_matches(market))
        .cloned();
    use_context_provider(|| Signal::new(Markets(market.clone())));
    use_context_provider(|| Signal::new(None::<Connected>));
    use_context_provider(|| Signal::new(Picker(false)));
    use_context_provider(|| Signal::new(MarketIssue(None)));
    let mut context = use_context::<Signal<Markets>>();
    let mut issue = use_context::<Signal<MarketIssue>>();
    let (latest, failures) = use_sticky(markets);
    use_effect(move || {
        let next = latest
            .read()
            .as_ref()
            .and_then(|markets| markets.first())
            .filter(|market| deployment_matches(market))
            .cloned();
        let merged = steady_market(context.peek().0.as_ref(), next);
        if context.peek().0 != merged {
            context.set(Markets(merged));
        }
        // A single failed refresh keeps the last good market on screen; only a
        // run of failures, or never having loaded, surfaces the problem.
        let next = MarketIssue(match latest.read().as_ref() {
            Some(markets) if failures() < 3 => market_issue(Some(&Ok(markets.clone()))),
            _ => market_issue(markets.read().as_ref()),
        });
        if *issue.peek() != next {
            issue.set(next);
        }
    });
    use_future(move || async move {
        loop {
            TimeoutFuture::new(5_000).await;
            markets.restart();
        }
    });
    rsx! { Router::<Route> {} }
}

/// Keeps the last successful value of a polled resource, so a failed refresh
/// never blanks the page. The second signal counts consecutive failures.
fn use_sticky<T: Clone + PartialEq + 'static, E: 'static>(
    resource: Resource<Result<T, E>>,
) -> (Signal<Option<T>>, Signal<u32>) {
    let mut good = use_signal(|| None::<T>);
    let mut failures = use_signal(|| 0_u32);
    use_effect(move || match &*resource.read() {
        Some(Ok(value)) => {
            if good.peek().as_ref() != Some(value) {
                good.set(Some(value.clone()));
            }
            if *failures.peek() != 0 {
                failures.set(0);
            }
        }
        Some(Err(_)) => {
            let count = *failures.peek();
            failures.set(count.saturating_add(1));
        }
        None => {}
    });
    (good, failures)
}

/// Carries the previous spot and maker indication across a refresh that briefly
/// lacks them, so prices and rates don't blink out between oracle updates. The
/// service still refuses quotes without a fresh spot.
fn steady_market(previous: Option<&Market>, next: Option<Market>) -> Option<Market> {
    let mut next = next?;
    if let Some(previous) = previous.filter(|previous| previous.address == next.address) {
        if next.spot.is_none() {
            next.spot.clone_from(&previous.spot);
        }
        if next.indication.is_none() {
            next.indication.clone_from(&previous.indication);
        }
    }
    Some(next)
}

/// Upcoming expiries for the current market, refreshed every 10 seconds and
/// keyed on the market address only, so spot ticks don't refetch them. Returns
/// the last good list, plus an error only when nothing has loaded yet.
fn use_expiries() -> (Memo<Option<Vec<Expiry>>>, Memo<Option<String>>) {
    let market = use_context::<Signal<Markets>>();
    let address = use_memo(move || market.read().0.as_ref().map(|market| market.address));
    let mut response = use_resource(move || {
        let address = address();
        async move {
            match address {
                Some(address) => get::<Vec<Expiry>>(&format!("/v1/markets/{address}/expiries"))
                    .await
                    .map(Some),
                None => Ok(None),
            }
        }
    });
    use_future(move || async move {
        loop {
            TimeoutFuture::new(10_000).await;
            response.restart();
        }
    });
    let (good, _) = use_sticky(response);
    let expiries = use_memo(move || good.read().clone().flatten());
    let error = use_memo(move || {
        if expiries.read().is_some() {
            return None;
        }
        response
            .read()
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .cloned()
    });
    (expiries, error)
}

#[derive(Clone, Copy, PartialEq)]
struct Picker(bool);

/// Assets shown on Earn. Only assets with a deployed market whose base mint the
/// browser recognises can open a ticket; the rest are listed as upcoming.
const ASSETS: [(&str, &str); 3] = [("SOL", "Solana"), ("JUP", "Jupiter"), ("PEPE", "Pepe")];

#[component]
fn Page(#[props(into)] title: String, children: Element) -> Element {
    let issue = use_context::<Signal<MarketIssue>>();
    rsx! {
        Header {}
        main { class: "page",
            h1 { class: "page-title", "{title}" }
            if let Some(message) = issue().0 {
                div { class: "issue-banner", role: "alert",
                    span { class: "alert-icon", "aria-hidden": "true", "!" }
                    p { "{message}" }
                }
            }
            {children}
        }
    }
}

#[component]
fn Window(
    #[props(into)] path: String,
    #[props(into, default)] class: String,
    children: Element,
) -> Element {
    rsx! {
        section { class: "window {class}",
            div { class: "title-bar",
                span { class: "close-box", "aria-hidden": "true" }
                span { class: "stripes", "aria-hidden": "true" }
                span { class: "window-path", "{path}" }
                span { class: "stripes", "aria-hidden": "true" }
                span { class: "zoom-box", "aria-hidden": "true" }
            }
            div { class: "window-body", {children} }
        }
    }
}

#[component]
fn TokenIcon(#[props(into)] symbol: String) -> Element {
    let class = format!("token token-{}", symbol.to_ascii_lowercase());
    let glyph = match symbol.as_str() {
        "USDC" => "$",
        "WSOL" | "SOL" => "◎",
        other => &other[..1],
    }
    .to_string();
    rsx! { span { class: "{class}", "aria-hidden": "true", "{glyph}" } }
}

#[component]
fn Header() -> Element {
    let market = use_context::<Signal<Markets>>();
    let mut connected = use_context::<Signal<Option<Connected>>>();
    let mut picker = use_context::<Signal<Picker>>();
    let discovery = use_hook(|| Discovery::start().ok().map(std::rc::Rc::new));
    let mut error = use_signal(String::new);
    let route = use_route::<Route>();
    let chain = market
        .read()
        .0
        .as_ref()
        .map(|market| market.cluster.clone())
        .unwrap_or_default();
    use_future(move || async move {
        loop {
            TimeoutFuture::new(750).await;
            let cluster = market
                .peek()
                .0
                .as_ref()
                .map(|market| market.cluster.clone());
            let stale = cluster.as_deref().is_some_and(|cluster| {
                connected
                    .peek()
                    .as_ref()
                    .is_some_and(|wallet| !wallet.is_current(cluster))
            });
            if stale {
                connected.set(None);
            }
        }
    });
    #[cfg(feature = "local-wallet")]
    if let Some(discovery) = &discovery {
        let _ = discovery.install_fixture(&chain);
    }
    let cluster = market
        .read()
        .0
        .as_ref()
        .map(|m| m.cluster.trim_start_matches("solana:").to_string())
        .unwrap_or_else(|| "solana".into());
    let on_earn = matches!(route, Route::Earn {} | Route::Ticket { .. });
    let on_dashboard = matches!(route, Route::Dashboard {} | Route::PositionDetail { .. });
    let mut minute = use_signal(now);
    use_future(move || async move {
        loop {
            TimeoutFuture::new(15_000).await;
            minute.set(now());
        }
    });
    let (_, _, _, hour, min) = date_parts(minute());
    let spot = market
        .read()
        .0
        .as_ref()
        .and_then(|market| market.spot.as_ref())
        .map(|spot| format!("SOL ${}", usd(spot.price)));
    rsx! {
        header { class: "topbar",
            Link { class: "wordmark", to: Route::Earn {}, "stryk" }
            nav { class: "primary", "aria-label": "Main",
                Link { class: if on_earn { "active" } else { "" }, to: Route::Earn {}, "Earn" }
                Link { class: if on_dashboard { "active" } else { "" }, to: Route::Dashboard {}, "Dashboard" }
            }
            div { class: "topbar-end",
                span { class: "network", title: "Solana cluster", span { class: "dot", "aria-hidden": "true" } "{cluster}" }
                if let Some(spot) = spot { span { class: "menu-spot", title: "Pyth SOL/USD", "{spot}" } }
                span { class: "menu-clock", title: "UTC", "{hour:02}:{min:02} UTC" }
                div { class: "connect",
                    if let Some(wallet) = connected() {
                        button { class: "connect-button", title: "Disconnect wallet", onclick: move |_| connected.set(None),
                            span { class: "connect-icon", TokenIcon { symbol: "SOL" } }
                            span { class: "connect-label", "{short(wallet.address)}" }
                        }
                    } else {
                        button { class: "connect-button", "aria-expanded": picker().0, "aria-haspopup": "dialog",
                            onclick: move |_| { let open = picker().0; picker.set(Picker(!open)); },
                            span { class: "connect-icon", TokenIcon { symbol: "SOL" } }
                            span { class: "connect-label", "Connect" }
                        }
                    }
                    if picker().0 && connected().is_none() {
                        div { class: "wallet-picker", role: "dialog", "aria-label": "Wallets",
                            p { class: "picker-title", "~/wallets" }
                            if let Some(discovery) = discovery {
                                for (name, wallet) in discovery.wallets() {
                                    button { key: "{name}", onclick: {
                                        let chain = chain.clone();
                                        move |_| {
                                            let (wallet, chain) = (wallet.clone(), chain.clone());
                                            spawn(async move {
                                                match Connected::connect(wallet, &chain).await {
                                                    Ok(wallet) => { connected.set(Some(wallet)); picker.set(Picker(false)); error.set(String::new()); }
                                                    Err(message) => error.set(message),
                                                }
                                            });
                                        }
                                    }, "{name}" }
                                }
                                if discovery.wallets().is_empty() { span { class: "muted", "No compatible Solana wallet found" } }
                            }
                            if !error().is_empty() { p { class: "alert", role: "alert", "{error}" } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Earn() -> Element {
    let mut put = use_signal(|| false);
    let market = use_context::<Signal<Markets>>();
    let (expiries, _) = use_expiries();
    let next = expiries.read().as_ref().and_then(|items| {
        items
            .iter()
            .find(|expiry| expiry.expires_at > now())
            .cloned()
    });
    let current = market.read().0.clone();
    let weekly = current.as_ref().is_none_or(|market| market.weekly);
    let rate_name = if weekly { "APR" } else { "Return" };
    let (sold, range) = current
        .as_ref()
        .zip(next.as_ref())
        .map(|(market, expiry)| {
            let sold = (expiry.capacity_total > 0).then(|| {
                (expiry.capacity_filled as f64 * 100.0 / expiry.capacity_total as f64).min(100.0)
            });
            let center = market.spot.as_ref().map_or(0, |spot| spot.price);
            let rates: Vec<f64> = strike_grid(market, put(), center)
                .into_iter()
                .filter_map(|strike| indicative(market, expiry, put(), strike))
                .map(|(term, apr)| if market.weekly { apr } else { term })
                .collect();
            let range = rates
                .iter()
                .copied()
                .reduce(f64::max)
                .zip(rates.iter().copied().reduce(f64::min));
            (sold, range)
        })
        .unwrap_or((None, None));
    let strategy = if put() { "put" } else { "call" };
    let kind_name = if put() {
        "Cash-secured put"
    } else {
        "Covered call"
    };
    let sold_pct = sold.unwrap_or(0.0);
    let rows: Vec<_> = ASSETS
        .iter()
        .map(|(symbol, name)| {
            let live = *symbol == "SOL" && current.is_some();
            let (max, min) = match (live, range) {
                (true, Some((max, min))) => (format!("{max:.2}%"), format!("{min:.2}%")),
                _ => ("—".to_string(), "—".to_string()),
            };
            (*symbol, *name, live, max, min)
        })
        .collect();
    rsx! {
        Page { title: "Earn yield upfront",
            Window { path: "~/assets",
                div { class: "cap-strip",
                    div { class: "cap-bar", role: "progressbar", "aria-label": "Share of the current expiry cap sold",
                        "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": "{sold_pct:.2}",
                        div { class: "cap-fill", style: "width: {sold_pct:.2}%;" }
                        span { if sold.is_some() { "{sold_pct:.2}% of cap sold" } else { "cap unavailable" } }
                    }
                }
                div { class: "folder-tabs", role: "tablist", "aria-label": "Strategy",
                    button { class: if !put() { "folder-tab active" } else { "folder-tab" }, role: "tab", "aria-selected": !put(),
                        onclick: move |_| put.set(false), span { "covered calls" } }
                    button { class: if put() { "folder-tab active" } else { "folder-tab" }, role: "tab", "aria-selected": put(),
                        onclick: move |_| put.set(true), span { "cash secured puts" } }
                }
                div { class: "table-wrap",
                    table { class: "grid-table assets",
                        thead {
                            tr {
                                th { "Asset" }
                                th { "Chain" }
                                th { class: "num", "Type" }
                                th { class: "num", "Max {rate_name}" }
                                th { class: "num", "Min {rate_name}" }
                                th { span { class: "sr-only", "Action" } }
                            }
                        }
                        tbody {
                            for (symbol, name, live, max, min) in rows {
                                tr { key: "{symbol}", class: if live { "" } else { "pending" },
                                    td { div { class: "asset-cell", TokenIcon { symbol } strong { title: "{name}", "{symbol}" } } }
                                    td { div { class: "chain-cell", span { class: "chain-mark", "aria-hidden": "true" } "Solana" } }
                                    td { class: "num", "{kind_name}" }
                                    td { class: if live { "num rate" } else { "num muted" }, "{max}" }
                                    td { class: if live { "num rate" } else { "num muted" }, "{min}" }
                                    td { class: "action-cell",
                                        if live {
                                            Link { class: "earn-button", to: Route::Ticket { strategy: strategy.into(), expiry: None, strike: None },
                                                TokenIcon { symbol } span { "Earn on {symbol}" } }
                                        } else {
                                            span { class: "earn-button disabled", "aria-disabled": "true",
                                                TokenIcon { symbol }
                                                span { if symbol == "SOL" { "Market offline" } else { "{symbol} · soon" } } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
enum QuoteState {
    Idle,
    Requesting,
    NoQuote,
    Quoted(Box<ReviewedQuote>),
    Failed(String),
}

#[derive(Clone)]
struct ReviewedQuote {
    request: Request,
    bid: Bid,
    spot: Option<Spot>,
}

#[component]
fn Ticket(strategy: String, expiry: Option<String>, strike: Option<u64>) -> Element {
    let nav = use_navigator();
    let route = use_route::<Route>();
    let route_seen = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(route.clone())));
    let kind = if strategy == "put" { 1 } else { 0 };
    let expiry_choice = expiry
        .as_deref()
        .and_then(|value| value.parse::<Address>().ok());
    let market_ctx = use_context::<Signal<Markets>>();
    let connected = use_context::<Signal<Option<Connected>>>();
    let market = market_ctx.read().0.clone();
    let mut wallet_balances = use_resource(move || {
        let market = market_ctx.peek().0.clone();
        let writer = connected().map(|wallet| wallet.address);
        async move {
            let wallet = writer?;
            let market = market?;
            let base = submit::token_balance(wallet, market.base).await.ok();
            let quote = submit::token_balance(wallet, market.quote).await.ok();
            Some((wallet, base, quote))
        }
    });
    let (expires, expiry_failure) = use_expiries();
    // Strikes stay anchored to the spot seen when the ticket opened and only
    // re-center once spot drifts more than two ticks, so the row never jumps.
    let anchor = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(None::<(Address, u64)>)));
    let mut size = use_signal(|| "1".to_string());
    let mut wrap_size = use_signal(String::new);
    let mut quote = use_signal(|| QuoteState::Idle);
    let costs = use_resource(move || {
        let market = market_ctx.peek().0.clone();
        let quoted = quote();
        async move {
            let (Some(market), QuoteState::Quoted(terms)) = (market, quoted) else {
                return None;
            };
            let ReviewedQuote { request, bid, .. } = *terms;
            submit::fill_cost(&market, &request, &bid)
                .await
                .ok()
                .map(|cost| (request.id, cost))
        }
    });
    let mut revision = use_signal(|| 0_u64);
    let mut submitting = use_signal(|| false);
    let mut pending = use_signal(|| false);
    let mut outcome = use_signal(String::new);
    let mut fill_status = use_signal(|| None::<submit::FillStatus>);
    let mut clock = use_signal(now);
    use_future(move || async move {
        loop {
            TimeoutFuture::new(1_000).await;
            clock.set(now());
        }
    });
    let pending_owner = use_memo(move || {
        let wallet = connected()?.address;
        let market = market_ctx.read();
        let market = market.0.as_ref()?;
        Some((
            wallet,
            market.address,
            market.domain,
            market.cluster.clone(),
        ))
    });
    let mut recovering = use_signal(|| false);
    use_future(move || async move {
        loop {
            TimeoutFuture::new(5_000).await;
            if !*pending.peek() || *submitting.peek() || *recovering.peek() {
                continue;
            }
            let Some((writer, _, domain, _)) = pending_owner() else {
                continue;
            };
            let Some(market) = market_ctx.peek().0.clone() else {
                continue;
            };
            recovering.set(true);
            let result = submit::recover(market.clone(), writer).await;
            recovering.set(false);
            if pending_owner
                .peek()
                .as_ref()
                .is_some_and(|(owner, _, active_domain, _)| {
                    *owner == writer && *active_domain == domain
                })
            {
                pending.set(submit::has_pending(&market, writer));
                match result {
                    Ok(Some(status)) => {
                        fill_status.set(Some(status));
                        outcome.set(String::new());
                    }
                    Err(message) => {
                        fill_status.set(None);
                        outcome.set(message);
                    }
                    Ok(None) => {}
                }
            }
        }
    });
    use_effect(move || {
        let Some((writer, _, _, _)) = pending_owner() else {
            pending.set(false);
            return;
        };
        let Some(market) = market_ctx.peek().0.clone() else {
            return;
        };
        let found = submit::has_pending(&market, writer);
        pending.set(found);
        if found {
            recovering.set(true);
            spawn(async move {
                let result = submit::recover(market.clone(), writer).await;
                recovering.set(false);
                if pending_owner
                    .peek()
                    .as_ref()
                    .is_some_and(|(owner, _, domain, _)| {
                        *owner == writer && *domain == market.domain
                    })
                {
                    pending.set(submit::has_pending(&market, writer));
                    match result {
                        Ok(Some(status)) => {
                            fill_status.set(Some(status));
                            outcome.set(String::new());
                        }
                        Err(message) => {
                            fill_status.set(None);
                            outcome.set(message);
                        }
                        Ok(None) => {}
                    }
                }
            });
        }
    });
    use_effect(move || {
        let _ = connected();
        let current = *revision.peek();
        revision.set(current.wrapping_add(1));
        quote.set(QuoteState::Idle);
        outcome.set(String::new());
        fill_status.set(None);
    });
    if *route_seen.borrow() != route {
        route_seen.replace(route);
        let current = *revision.peek();
        revision.set(current.wrapping_add(1));
        quote.set(QuoteState::Idle);
        outcome.set(String::new());
        fill_status.set(None);
    }
    let expiry_error = expiry_failure();
    let expiry_list = expires().unwrap_or_default();
    let cutoff = market
        .as_ref()
        .map_or(0, |market| i64::from(market.fill_cutoff));
    let open_until = clock().saturating_add(cutoff);
    let expiry = expiry_list
        .iter()
        .find(|item| {
            item.expires_at > open_until
                && expiry_choice.is_none_or(|choice| item.address == choice)
        })
        .cloned();
    let selected_expiry = expiry.as_ref().map(|item| item.address.to_string());
    let unavailable_expiry = expiry_choice.is_some() && expiry.is_none() && !expiry_list.is_empty();
    let center = market.as_ref().and_then(|market| {
        let spot = market.spot.as_ref()?.price;
        let kept = anchor.get().filter(|(address, center)| {
            *address == market.address
                && spot.abs_diff(*center) <= market.strike_tick.saturating_mul(2)
        });
        let center = kept.map_or(spot, |(_, center)| center);
        anchor.set(Some((market.address, center)));
        Some(center)
    });
    let mut strikes = market
        .as_ref()
        .zip(center)
        .map(|(market, center)| strike_grid(market, kind == 1, center))
        .unwrap_or_default();
    let selected_strike = strike
        .filter(|strike| {
            *strike > 0
                && market.as_ref().is_some_and(|market| {
                    market.strike_tick > 0 && strike.is_multiple_of(market.strike_tick)
                })
        })
        .or_else(|| strikes.get(strikes.len() / 2).copied());
    if let Some(strike) = selected_strike {
        if !strikes.is_empty() && !strikes.contains(&strike) {
            strikes.push(strike);
            strikes.sort_unstable();
        }
    }
    let quote_market = market.clone();
    let quote_expiry = expiry.clone();
    let amount = parse_atoms(&size(), BASE_DECIMALS);
    let quoted = match quote() {
        QuoteState::Quoted(terms)
            if clock() < terms.bid.valid_until
                && market.as_ref().is_some_and(|market| {
                    !market.paused && market.address == terms.request.terms.market
                })
                && expiry
                    .as_ref()
                    .is_some_and(|expiry| expiry.address == terms.request.terms.expiry)
                && selected_strike == Some(terms.request.terms.strike)
                && amount == Some(terms.request.terms.quantity)
                && kind == terms.request.terms.kind
                && connected()
                    .is_some_and(|wallet| wallet.address == terms.request.terms.writer) =>
        {
            Some(*terms)
        }
        _ => None,
    };
    let costs = (*costs.read())
        .flatten()
        .filter(|(id, _)| quoted.as_ref().is_some_and(|quote| quote.request.id == *id));
    let notional = amount
        .zip(selected_strike)
        .and_then(|(amount, strike)| strike_atoms(amount, strike));
    let amount_text = amount
        .map(|amount| format_atoms(amount, BASE_DECIMALS))
        .unwrap_or_else(|| "—".into());
    let notional_text = notional
        .map(|notional| format_atoms(notional, QUOTE_DECIMALS))
        .unwrap_or_else(|| "—".into());
    let buy = kind == 1;
    let balance = (*wallet_balances.read())
        .flatten()
        .filter(|(owner, _, _)| connected().is_some_and(|wallet| wallet.address == *owner))
        .and_then(|(_, base, quote)| if buy { quote } else { base });
    let maximum = market
        .as_ref()
        .zip(balance)
        .and_then(|(market, available)| {
            let maximum = if buy {
                let strike = selected_strike?;
                (u128::from(available) * 1_000_000_000 / u128::from(strike))
                    .min(u128::from(market.max_quantity)) as u64
            } else {
                available.min(market.max_quantity)
            };
            (maximum >= market.min_quantity).then_some(maximum)
        });
    let can_quote = market.as_ref().is_some_and(|market| {
        !market.paused
            && market.spot.is_some()
            && balance.is_none_or(|available| {
                if buy {
                    notional.is_some_and(|locked| locked <= available)
                } else {
                    amount.is_some_and(|size| size <= available)
                }
            })
            && amount
                .is_some_and(|amount| (market.min_quantity..=market.max_quantity).contains(&amount))
    }) && expiry.is_some()
        && selected_strike.is_some()
        && connected().is_some()
        && !submitting()
        && !matches!(quote(), QuoteState::Requesting);
    let recover_market = market.clone();
    let wrap_market = market.clone();
    let unwrap_market = market.clone();

    let mut picker = use_context::<Signal<Picker>>();
    let mut wrap_open = use_signal(|| false);
    let weekly = market.as_ref().is_none_or(|market| market.weekly);
    let rate_name = if weekly { "APR" } else { "TERM" };
    let spot_text = market
        .as_ref()
        .and_then(|market| market.spot.as_ref())
        .map(|spot| (usd(spot.price), utc(spot.observed_at)));
    let cap_pct = expiry
        .as_ref()
        .filter(|expiry| expiry.capacity_total > 0)
        .map(|expiry| {
            (expiry.capacity_filled as f64 * 100.0 / expiry.capacity_total as f64).min(100.0)
        });
    let cap_left = expiry
        .as_ref()
        .filter(|expiry| expiry.capacity_total > 0)
        .map(|expiry| {
            format_atoms(
                expiry.capacity_total.saturating_sub(expiry.capacity_filled),
                QUOTE_DECIMALS,
            )
        });
    let expiry_when = expiry.as_ref().map(|expiry| {
        let (_, month, day, hour, minute) = date_parts(expiry.expires_at);
        format!("{month} {day} {hour:02}:{minute:02} UTC")
    });
    let heading_date = expiry.as_ref().map(|expiry| {
        (
            long_date(expiry.expires_at),
            time_left(expiry.expires_at - clock()),
        )
    });
    let expiry_short = expiry
        .as_ref()
        .map(|expiry| expiry_label(expiry.expires_at))
        .unwrap_or_else(|| "expiry".into());
    let spot_price = market
        .as_ref()
        .and_then(|market| market.spot.as_ref())
        .map(|spot| spot.price);
    let strike_count = strikes.len();
    let strike_rows: Vec<(u64, Option<f64>, Option<f64>)> = strikes
        .iter()
        .map(|strike| {
            let distance = spot_price
                .filter(|spot| *spot > 0)
                .map(|spot| (*strike as f64 - spot as f64) * 100.0 / spot as f64);
            let rate = market
                .as_ref()
                .zip(expiry.as_ref())
                .and_then(|(market, expiry)| indicative(market, expiry, buy, *strike))
                .map(|(term, apr)| if weekly { apr } else { term });
            (*strike, distance, rate)
        })
        .collect();
    let quoted_rate = (|| {
        let quote = quoted.as_ref()?;
        let seconds = expiry.as_ref()?.expires_at.checked_sub(clock())?;
        premium_rates(
            quote.bid.premium,
            amount?,
            if buy {
                notional?
            } else {
                quote.spot.as_ref()?.price
            },
            buy,
            seconds,
        )
    })();
    let indicative_rate = selected_strike.and_then(|strike| {
        market
            .as_ref()
            .zip(expiry.as_ref())
            .and_then(|(market, expiry)| indicative(market, expiry, buy, strike))
    });
    let headline = quoted_rate
        .or(indicative_rate)
        .map(|(term, apr)| format!("{:.2}%", if weekly { apr } else { term }));
    let headline_label = match (quoted_rate.is_some(), weekly) {
        (true, true) => "APR",
        (true, false) => "term return",
        (false, true) => "est. APR",
        (false, false) => "est. term return",
    };
    // Premium = strike notional × maker premium rate, the same basis as the APR.
    let estimate = market
        .as_ref()
        .and_then(|market| market.indication.as_ref())
        .zip(notional)
        .map(|(indication, notional)| {
            u128::from(notional) * u128::from(indication.premium_bps) / 10_000
        });
    let upfront = match (&quoted, estimate) {
        (Some(quote), _) => format!(
            "{} USDC upfront",
            format_atoms(quote.bid.premium, QUOTE_DECIMALS)
        ),
        (None, Some(estimate)) => {
            format!("≈ {} USDC upfront", format_atoms(estimate, QUOTE_DECIMALS))
        }
        (None, None) => "— USDC upfront".into(),
    };
    let strike_text = selected_strike
        .map(|strike| format!("${}", format_atoms(strike, QUOTE_DECIMALS)))
        .unwrap_or_else(|| "strike".into());
    let insufficient = balance.is_some_and(|available| {
        if buy {
            notional.is_some_and(|locked| locked > available)
        } else {
            amount.is_some_and(|size| size > available)
        }
    });
    let size_bounds = market.as_ref().and_then(|market| {
        amount
            .filter(|amount| !(market.min_quantity..=market.max_quantity).contains(amount))
            .map(|_| {
                format!(
                    "SIZE MUST BE {}–{} SOL",
                    format_atoms(market.min_quantity, BASE_DECIMALS),
                    format_atoms(market.max_quantity, BASE_DECIMALS)
                )
            })
    });
    let step = 10_u64.pow(u32::from(BASE_DECIMALS)) / 10;
    let quote_ready = quoted
        .as_ref()
        .map(|terms| terms.bid.valid_until.saturating_sub(clock()));
    let mut log = vec![match quote() {
        QuoteState::Idle if connected().is_some() => {
            "indicative rate · press Earn for a firm quote".to_string()
        }
        QuoteState::Idle => "indicative rate · connect a wallet to trade".into(),
        QuoteState::Requesting => "requesting quote from makers…".into(),
        QuoteState::NoQuote => "no maker quoted these terms · try another strike or size".into(),
        QuoteState::Failed(message) => message.to_lowercase(),
        QuoteState::Quoted(terms) if clock() >= terms.bid.valid_until => {
            "quote expired · press Earn to refresh".into()
        }
        QuoteState::Quoted(terms) => format!(
            "best bid {} USDC · valid {}s",
            format_atoms(terms.bid.premium, QUOTE_DECIMALS),
            terms.bid.valid_until - clock()
        ),
    }];
    if !outcome().is_empty() {
        log.push(outcome());
    }
    if let Some(status) = fill_status() {
        log.push(status.message.clone());
    }
    let requesting = matches!(quote(), QuoteState::Requesting);
    let flow = FillSignals {
        quote,
        submitting,
        fill_status,
        outcome,
        pending,
        connected,
        balances: wallet_balances,
    };
    let go_quote = quoted.clone();
    let cta_label = if requesting {
        "Requesting quote…"
    } else if submitting() {
        "Waiting for wallet…"
    } else {
        "Earn upfront premium now"
    };
    let cta_disabled = connected().is_some()
        && (submitting() || requesting || pending() || (go_quote.is_none() && !can_quote));

    rsx! {
        Page { title: "Earn yield upfront",
            div { class: "ticket-layout",
                Window { path: if buy { "~/earn/SOL/cash-secured-put" } else { "~/earn/SOL/covered-call" }, class: "ticket-window",
                    section { class: "ticket-bar", "aria-label": "Option ticket",
                        label { class: "bar-cell asset-select",
                            TokenIcon { symbol: "SOL" }
                            select { "aria-label": "Asset",
                                option { value: "SOL", selected: true, "SOL" }
                                option { value: "JUP", disabled: true, "JUP · soon" }
                                option { value: "PEPE", disabled: true, "PEPE · soon" }
                            }
                        }
                        label { class: "bar-cell",
                            select {
                                "aria-label": "Strategy",
                                value: "{kind}",
                                disabled: submitting(),
                                onchange: move |event| {
                                    let strategy = if event.value() == "1" { "put" } else { "call" };
                                    nav.push(Route::Ticket { strategy: strategy.into(), expiry: expiry_choice.map(|address| address.to_string()), strike: None });
                                },
                                option { value: "0", "Covered call" }
                                option { value: "1", "Cash-secured put" }
                            }
                        }
                        label { class: "bar-cell",
                            select {
                                "aria-label": "Expiry",
                                value: selected_expiry.clone().unwrap_or_default(),
                                disabled: submitting(),
                                onchange: move |event| {
                                    let value = event.value();
                                    nav.push(Route::Ticket { strategy: if buy { "put" } else { "call" }.into(), expiry: (!value.is_empty()).then_some(value), strike: selected_strike });
                                },
                                if unavailable_expiry { option { value: "", disabled: true, "Selected expiry is unavailable" } }
                                if expiry_list.is_empty() { option { value: "", if expiry_error.is_some() { "Expiries unavailable" } else { "Loading expiries…" } } }
                                for item in expiry_list.iter().filter(|item| item.expires_at > open_until) {
                                    option { key: "{item.address}", value: "{item.address}", title: "{utc(item.expires_at)}", "{expiry_label(item.expires_at)}" }
                                }
                            }
                        }
                        span { class: "bar-spacer" }
                        if let Some((price, observed)) = &spot_text {
                            span { class: "bar-cell spot", title: "SOL/USD observed {observed}", "${price}" }
                        } else {
                            span { class: "bar-cell spot muted", "$—" }
                        }
                    }
                    div { class: "ticket-body",
                        if let Some(error) = expiry_error { p { class: "notice", role: "status", "{error}" } }
                        div { class: "strike-picker",
                            div { class: "picker-head",
                                div {
                                    h2 { class: "ticket-heading",
                                        if buy { "Choose the price you're happy to buy SOL at" } else { "Choose the price you're happy to sell SOL at" }
                                    }
                                    p { class: "picker-sub",
                                        if let Some((date, left)) = &heading_date { "Expires {date} · {left}" } else { "Loading expiry…" }
                                    }
                                }
                            }
                            ul { class: "strikes", role: "radiogroup", "aria-label": "Strike price",
                                for (strike, distance, rate) in strike_rows {
                                    li { key: "{strike}",
                                        button {
                                            class: if selected_strike == Some(strike) { "strike selected" } else { "strike" },
                                            role: "radio",
                                            "aria-checked": selected_strike == Some(strike),
                                            disabled: submitting(),
                                            onclick: {
                                                let expiry = selected_expiry.clone();
                                                move |_| { nav.push(Route::Ticket { strategy: if buy { "put" } else { "call" }.into(), expiry: expiry.clone(), strike: Some(strike) }); }
                                            },
                                            span { class: "strike-price", "${format_atoms(strike, QUOTE_DECIMALS)}" }
                                            span { class: "strike-rate",
                                                if let Some(rate) = rate { "{rate:.2}% " } else { "— " }
                                                small { "{rate_name}" }
                                            }
                                            span { class: "strike-distance",
                                                if let Some(distance) = distance { "{distance:+.1}%" span { class: "from-spot", " from spot" } } else { "—" }
                                            }
                                        }
                                    }
                                }
                                if strike_count == 0 {
                                    for index in 0..6 { li { key: "placeholder-{index}", span { class: "strike placeholder", "aria-hidden": "true" } } }
                                }
                            }
                        }
                        div { class: "amount",
                            button { class: "amount-max", disabled: maximum.is_none(), onclick: move |_| {
                                if let Some(max) = maximum {
                                    size.set(format_atoms(max, BASE_DECIMALS));
                                    revision += 1;
                                    quote.set(QuoteState::Idle);
                                }
                            }, "MAX" }
                            div { class: "amount-step",
                                button { "aria-label": "Increase size by 0.1 SOL", disabled: submitting(), onclick: move |_| {
                                    let current = parse_atoms(&size(), BASE_DECIMALS).unwrap_or(0);
                                    size.set(format_atoms(current.saturating_add(step), BASE_DECIMALS));
                                    revision += 1;
                                    quote.set(QuoteState::Idle);
                                }, "+" }
                                button { "aria-label": "Decrease size by 0.1 SOL", disabled: submitting(), onclick: move |_| {
                                    let next = parse_atoms(&size(), BASE_DECIMALS).unwrap_or(0).saturating_sub(step);
                                    size.set(if next == 0 { String::new() } else { format_atoms(next, BASE_DECIMALS) });
                                    revision += 1;
                                    quote.set(QuoteState::Idle);
                                }, "−" }
                            }
                            input { id: "size", inputmode: "decimal", placeholder: "0", value: "{size}", autocomplete: "off",
                                disabled: submitting(),
                                oninput: move |event| { size.set(event.value()); revision += 1; quote.set(QuoteState::Idle); } }
                            div { class: "amount-asset",
                                label { r#for: "size", "SOL" }
                                small {
                                    if let Some(available) = balance {
                                        if buy { "{format_atoms(available, QUOTE_DECIMALS)} USDC available" } else { "{format_atoms(available, BASE_DECIMALS)} WSOL available" }
                                    } else if connected().is_some() { "balance —" } else { "not connected" }
                                }
                            }
                            TokenIcon { symbol: "SOL" }
                        }
                        div { class: "amount-notes",
                            if insufficient {
                                small { class: "warn", "INSUFFICIENT BALANCE" }
                            } else if let Some(message) = size_bounds {
                                small { class: "warn", "{message}" }
                            } else {
                                small { class: "muted", if buy { "LOCKS {notional_text} USDC" } else { "LOCKS {amount_text} WSOL" } }
                            }
                            if !buy && connected().is_some() {
                                button { class: "link-button", "aria-expanded": wrap_open(), onclick: move |_| wrap_open.toggle(), "WRAP SOL" }
                            }
                        }
                        if !buy && connected().is_some() && wrap_open() {
                            div { class: "wrap-panel",
                                div { class: "amount compact",
                                    input { "aria-label": "SOL to wrap", inputmode: "decimal", placeholder: "0.00 SOL", value: "{wrap_size}",
                                        oninput: move |event| wrap_size.set(event.value()) }
                                    button { class: "ghost", disabled: submitting() || parse_atoms(&wrap_size(), BASE_DECIMALS).is_none_or(|amount| amount == 0),
                                        onclick: move |_| {
                                            let (Some(market), Some(wallet), Some(amount)) = (wrap_market.clone(), connected(), parse_atoms(&wrap_size(), BASE_DECIMALS)) else { return; };
                                            submitting.set(true);
                                            fill_status.set(None);
                                            spawn(async move {
                                                let result = submit::wsol(market.cluster, wallet, Some(amount)).await;
                                                submitting.set(false);
                                                wallet_balances.restart();
                                                match result {
                                                    Ok(message) => { wrap_size.set(String::new()); outcome.set(message); }
                                                    Err(message) => outcome.set(message),
                                                }
                                            });
                                        },
                                        "Wrap SOL"
                                    }
                                    button { class: "ghost", disabled: submitting() || balance.is_none_or(|amount| amount == 0),
                                        onclick: move |_| {
                                            let (Some(market), Some(wallet)) = (unwrap_market.clone(), connected()) else { return; };
                                            submitting.set(true);
                                            fill_status.set(None);
                                            spawn(async move {
                                                let result = submit::wsol(market.cluster, wallet, None).await;
                                                submitting.set(false);
                                                wallet_balances.restart();
                                                match result { Ok(message) | Err(message) => outcome.set(message) }
                                            });
                                        },
                                        "Unwrap all"
                                    }
                                }
                            }
                        }
                        div { class: "outcome",
                            div { class: "outcome-head", "Now" }
                            div { class: "outcome-now",
                                div { class: "outcome-rate",
                                    p { class: "rate-line",
                                        span { class: "rate-num", if let Some(rate) = &headline { "{rate}" } else { "—" } }
                                        span { class: "rate-label", "{headline_label}" }
                                    }
                                    p { class: "upfront", "{upfront}" }
                                }
                                div { class: if requesting || submitting() { "log busy" } else { "log" }, role: "status", "aria-live": "polite",
                                    for (index, line) in log.iter().enumerate() {
                                        p { key: "{index}", "~/earn/ {line}" }
                                    }
                                }
                            }
                            div { class: "outcome-head", "On {expiry_short}" }
                            div { class: "outcome-split",
                                div { class: "outcome-side",
                                    small { "If SOL " strong { if buy { "BELOW" } else { "AT OR BELOW" } } " {strike_text}" }
                                    p {
                                        TokenIcon { symbol: "SOL" }
                                        strong { if buy { "Buy {amount_text} WSOL" } else { "Get {amount_text} WSOL back" } }
                                    }
                                }
                                div { class: "outcome-side right",
                                    small { "If SOL " strong { if buy { "AT OR ABOVE" } else { "ABOVE" } } " {strike_text}" }
                                    p {
                                        strong { if buy { "Get {notional_text} USDC back" } else { "Receive {notional_text} USDC" } }
                                        TokenIcon { symbol: "USDC" }
                                    }
                                }
                            }
                        }
                        div { class: "cta",
                            button { class: "primary", disabled: cta_disabled,
                                onclick: move |_| {
                                    let Some(wallet) = connected() else { picker.set(Picker(true)); return; };
                                    let Some(market) = quote_market.clone() else { return; };
                                    if let Some(reviewed) = go_quote.clone() {
                                        spawn(flow.sign(market, reviewed, wallet));
                                        return;
                                    }
                                    let Some(expiry) = quote_expiry.clone() else { return; };
                                    let (Some(strike), Some(quantity)) = (selected_strike, parse_atoms(&size(), BASE_DECIMALS)) else { return; };
                                    let writer = wallet.address;
                                    let terms = Terms { version: api::VERSION, market: market.address, expiry: expiry.address, writer, kind, strike, quantity };
                                    let run = revision();
                                    quote.set(QuoteState::Requesting);
                                    fill_status.set(None);
                                    outcome.set(String::new());
                                    spawn(async move {
                                        let result = request_quote(terms).await;
                                        if revision() != run || connected().is_none_or(|wallet| wallet.address != writer) { return; }
                                        match result {
                                            Ok(Some(reviewed)) => {
                                                quote.set(QuoteState::Quoted(Box::new(reviewed.clone())));
                                                flow.sign(market, reviewed, wallet).await;
                                            }
                                            Ok(None) => quote.set(QuoteState::NoQuote),
                                            Err(message) => quote.set(QuoteState::Failed(message)),
                                        }
                                    });
                                },
                                "{cta_label}"
                            }
                            if let Some(status) = fill_status().filter(|status| status.finalized) {
                                Link { class: "earn-button", to: Route::PositionDetail { address: status.position }, "View position →" }
                            }
                        }
                    }
                }
                aside { class: "ticket-side",
                    Window { path: "Get Info", class: "info-window",
                        dl { class: "info",
                            div { dt { "Expiry" } dd { if let Some(when) = &expiry_when { "{when}" } else { "—" } } }
                            div { dt { "Strike" } dd { "{strike_text}" } }
                            div { dt { "Spot" } dd { title: spot_text.as_ref().map(|(_, observed)| observed.clone()).unwrap_or_default(),
                                if let Some((price, _)) = &spot_text { "${price}" } else { "—" } } }
                            div { dt { "You lock" } dd { if buy { "{notional_text} USDC" } else { "{amount_text} WSOL" } } }
                            div { dt { "Quote" } dd { if let Some(seconds) = quote_ready { "valid {seconds}s" } else { "indicative" } } }
                            if let Some((_, (fee, rent))) = costs {
                                div { dt { "Network" } dd { title: "Includes a fresh position, two escrows, and any missing wallet token accounts. The escrow rent returns at settlement.",
                                    "{format_atoms(fee, BASE_DECIMALS)} SOL + ≤{format_atoms(rent, BASE_DECIMALS)} rent" } }
                            }
                        }
                        if connected().is_some() {
                            button { class: "ghost wide", disabled: submitting() || recovering(), onclick: move |_| {
                                let (Some(market), Some(wallet)) = (recover_market.clone(), connected()) else { return; };
                                submitting.set(true);
                                spawn(async move {
                                    let result = submit::recover(market.clone(), wallet.address).await;
                                    submitting.set(false);
                                    if connected().is_none_or(|connected| connected.address != wallet.address) { return; }
                                    pending.set(submit::has_pending(&market, wallet.address));
                                    match result {
                                        Ok(Some(status)) => { fill_status.set(Some(status)); outcome.set(String::new()); }
                                        Err(message) => { fill_status.set(None); outcome.set(message); }
                                        Ok(None) if fill_status.peek().is_none() => outcome.set("No pending transaction".into()),
                                        Ok(None) => {}
                                    }
                                });
                            }, "Check transaction status" }
                        }
                        details { class: "fine-print",
                            summary { "If the expiry price is unavailable" }
                            p { "If the fix deadline passes without a valid price, both parties receive their original collateral. You keep the upfront premium." }
                        }
                    }
                    Window { path: "Cap", class: "cap-window",
                        div { class: "cap-bar", role: "progressbar", "aria-label": "Share of this expiry's cap sold",
                            "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": "{cap_pct.unwrap_or(0.0):.1}",
                            div { class: "cap-fill", style: "width: {cap_pct.unwrap_or(0.0):.2}%;" }
                        }
                        dl { class: "info",
                            div { dt { "Sold" } dd { if let Some(pct) = cap_pct { "{pct:.1}%" } else { "—" } } }
                            div { dt { "Left" } dd { if let Some(left) = &cap_left { "{left} USDC" } else { "—" } } }
                        }
                    }
                }
            }
        }
    }
}

/// Signals the fill flow updates, bundled so the one-click path and a re-sign
/// of an already reviewed quote share the same completion handling.
#[derive(Clone, Copy)]
struct FillSignals {
    quote: Signal<QuoteState>,
    submitting: Signal<bool>,
    fill_status: Signal<Option<submit::FillStatus>>,
    outcome: Signal<String>,
    pending: Signal<bool>,
    connected: Signal<Option<Connected>>,
    balances: Resource<Option<WalletBalances>>,
}

/// Wallet, WSOL balance and USDC balance, in atoms.
type WalletBalances = (Address, Option<u64>, Option<u64>);

impl FillSignals {
    async fn sign(mut self, market: Market, reviewed: ReviewedQuote, wallet: Connected) {
        let writer = wallet.address;
        self.submitting.set(true);
        self.fill_status.set(None);
        self.outcome
            .set("review the transaction in your wallet…".into());
        let ReviewedQuote { request, bid, .. } = reviewed;
        let result = submit::execute(market.clone(), request, bid, wallet).await;
        self.submitting.set(false);
        if self
            .connected
            .peek()
            .as_ref()
            .is_none_or(|wallet| wallet.address != writer)
        {
            return;
        }
        self.pending.set(submit::has_pending(&market, writer));
        match result {
            Ok(status) => {
                self.balances.restart();
                self.quote.set(QuoteState::Idle);
                self.fill_status.set(Some(status));
                self.outcome.set(String::new());
            }
            Err(message) => self.outcome.set(message),
        }
    }
}

fn strike_grid(market: &Market, put: bool, spot: u64) -> Vec<u64> {
    let tick = market.strike_tick;
    let Some(center) = spot.checked_div(tick).filter(|center| *center > 0) else {
        return Vec::new();
    };
    let start = if put { -6 } else { 1 };
    (start..start + 6)
        .filter_map(|offset| {
            center
                .checked_add_signed(offset)?
                .checked_mul(tick)
                .filter(|strike| *strike > 0)
        })
        .collect()
}

fn indicative(market: &Market, expiry: &Expiry, put: bool, strike: u64) -> Option<(f64, f64)> {
    let at = now();
    // Display only: the last indication stays on screen between maker updates.
    let indication = market.indication.as_ref()?;
    let collateral = if put {
        strike
    } else {
        market.spot.as_ref()?.price
    };
    let seconds = expiry.expires_at.checked_sub(at)?;
    if collateral == 0 || seconds <= 0 {
        return None;
    }
    let term = strike as f64 * f64::from(indication.premium_bps) / (collateral as f64 * 100.0);
    Some((term, term * 31_536_000.0 / seconds as f64))
}

#[component]
fn Dashboard() -> Element {
    let connected = use_context::<Signal<Option<Connected>>>();
    let market = use_context::<Signal<Markets>>();
    let spot = market
        .read()
        .0
        .as_ref()
        .and_then(|market| market.spot.clone());
    let mut history = use_signal(|| false);
    let mut cursor = use_signal(|| None::<String>);
    let mut daily_income = use_resource(move || {
        let owner = connected().map(|wallet| wallet.address);
        async move {
            let Some(owner) = owner else { return Ok(None) };
            get::<Vec<IncomePoint>>(&format!("/v1/income/{owner}"))
                .await
                .map(|rows| Some((owner, rows)))
        }
    });
    let (income_kept, _) = use_sticky(daily_income);
    let mut portfolio = use_resource(move || {
        let owner = connected().map(|wallet| wallet.address);
        let history = history();
        let cursor = cursor();
        async move {
            match owner {
                Some(owner) => {
                    let mut path = format!("/v1/positions?owner={owner}&history={history}");
                    if let Some(cursor) = &cursor {
                        path.push_str("&cursor=");
                        path.push_str(cursor);
                    }
                    get::<Portfolio>(&path)
                        .await
                        .map(|page| Some((owner, history, cursor, page)))
                }
                None => Ok(None),
            }
        }
    });
    use_future(move || async move {
        loop {
            TimeoutFuture::new(5_000).await;
            if connected().is_some() {
                portfolio.restart();
                daily_income.restart();
            }
        }
    });
    use_effect(move || {
        let _ = connected();
        cursor.set(None);
    });
    let (kept, _) = use_sticky(portfolio);
    let view = portfolio.read();
    let page = match (kept.read().as_ref().and_then(Option::as_ref), view.as_ref()) {
        (Some((owner, page_history, page_cursor, portfolio)), _)
            if connected().is_some_and(|wallet| wallet.address == *owner)
                && history() == *page_history
                && cursor().as_ref() == page_cursor.as_ref() =>
        {
            Ok((portfolio.clone(), page_cursor.clone()))
        }
        (_, Some(Ok(None))) => Err("wallet not connected"),
        (_, Some(Err(_))) => Err("portfolio temporarily unavailable"),
        _ => Err("loading finalized positions"),
    };
    let income_rows = income_kept
        .read()
        .as_ref()
        .and_then(Option::as_ref)
        .filter(|(owner, _)| connected().is_some_and(|wallet| wallet.address == *owner))
        .map(|(_, rows)| rows.clone());
    let mut picker = use_context::<Signal<Picker>>();
    if connected().is_none() {
        return rsx! {
            Page { title: "Dashboard",
                Window { path: "~/positions", class: "connect-window",
                    div { class: "connect-prompt",
                        span { class: "alert-icon", "aria-hidden": "true", "!" }
                        div {
                            p { strong { "No wallet connected" } }
                            p { class: "muted", "Connect a Solana wallet to see your positions, premiums and settlements." }
                        }
                        button { class: "ghost", onclick: move |_| picker.set(Picker(true)), "Connect wallet" }
                    }
                }
            }
        };
    }
    rsx! {
        Page { title: "Dashboard",
            div { class: "dash-top",
                Window { path: "~/income", class: "income-window",
                    match &page {
                        Ok((portfolio, _)) => {
                            let income = portfolio.income_atoms.parse::<u128>().ok().map(|atoms| format_atoms(atoms, QUOTE_DECIMALS)).unwrap_or_else(|| "—".into());
                            let open = portfolio.positions.iter().filter(|position| position.outcome == 0).count();
                            rsx! {
                                div { class: "income-summary",
                                    small { "TOTAL UPFRONT PREMIUM" }
                                    p { class: "rate-line", span { class: "rate-num", "{income}" } span { class: "rate-label", " USDC" } }
                                    dl { class: "mini-stats",
                                        div { dt { "Finalized slot" } dd { "{portfolio.as_of_slot}" } }
                                        div { dt { if history() { "Settled on page" } else { "Open on page" } } dd { if history() { "{portfolio.positions.len()}" } else { "{open}" } } }
                                    }
                                    if !portfolio.indexing_healthy {
                                        p { class: "notice", role: "status", "Indexing delayed. Showing the last available finalized positions." }
                                    }
                                }
                            }
                        }
                        Err(message) => rsx! { p { class: "empty", "v1 ~/income: {message}" } },
                    }
                }
                Window { path: "~/chart", class: "chart-window",
                    if let Some(rows) = income_rows.filter(|rows| !rows.is_empty()) {
                        IncomeChart { rows }
                    } else if page.is_ok() {
                        p { class: "empty", "v1 ~/dashboard/~/chart: no premiums in the last 30 days" }
                    } else if let Err(message) = &page {
                        p { class: "empty", "v1 ~/dashboard/~/chart: {message}" }
                    }
                }
            }
            Window { path: "~/positions",
                div { class: "folder-tabs", role: "tablist", "aria-label": "Positions",
                    button { class: if !history() { "folder-tab active" } else { "folder-tab" }, role: "tab", "aria-selected": !history(),
                        onclick: move |_| { cursor.set(None); history.set(false); }, span { "positions" } }
                    button { class: if history() { "folder-tab active" } else { "folder-tab" }, role: "tab", "aria-selected": history(),
                        onclick: move |_| { cursor.set(None); history.set(true); }, span { "history" } }
                }
                div { class: "table-wrap",
                    table { class: "grid-table positions",
                        thead {
                            tr {
                                th { "Asset" }
                                th { "Type" }
                                th { "Maturity" }
                                th { class: "num", "Size" }
                                th { class: "num", "Collateral" }
                                th { class: "num", "Strike" }
                                th { class: "num", "Premium" }
                                th { class: "num", "Price" }
                                th { "Outcome" }
                            }
                        }
                        tbody {
                            match &page {
                                Ok((portfolio, _)) if !portfolio.positions.is_empty() => rsx! {
                                    for position in portfolio.positions.iter() {
                                        tr { key: "{position.address}",
                                            td { div { class: "asset-cell", TokenIcon { symbol: "SOL" } strong { "SOL" } } }
                                            td {
                                                Link { to: Route::PositionDetail { address: position.address.to_string() },
                                                    if position.kind == 0 { "Covered call" } else { "Cash-secured put" }
                                                }
                                            }
                                            td { title: "{utc(position.expires_at)}", "{expiry_label(position.expires_at)}" }
                                            td { class: "num", "{format_atoms(position.quantity, BASE_DECIMALS)}" }
                                            td { class: "num",
                                                if position.kind == 0 { "{format_atoms(position.quantity, BASE_DECIMALS)} WSOL" }
                                                else { "{format_atoms(position.strike_amount, QUOTE_DECIMALS)} USDC" }
                                            }
                                            td { class: "num", "${format_atoms(position.strike, QUOTE_DECIMALS)}" }
                                            td { class: "num rate", "{format_atoms(position.premium, QUOTE_DECIMALS)}" }
                                            if let Some(entry) = &position.entry_spot {
                                                td { class: "num", title: "Entry spot observed {utc(entry.observed_at)}", "${usd(entry.price)}" }
                                            } else if let Some(now_spot) = spot.as_ref().filter(|_| position.outcome == 0) {
                                                td { class: "num", title: "SOL now · observed {utc(now_spot.observed_at)}", "${usd(now_spot.price)}" }
                                            } else {
                                                td { class: "num muted", "—" }
                                            }
                                            td { span { class: "pill outcome-{position.outcome}", "{position_status(position.outcome, position.expires_at)}" } }
                                        }
                                    }
                                },
                                Ok(_) => rsx! {
                                    tr { class: "empty-row", td { colspan: "9", p { class: "empty", if history() { "v1 ~/positions: no settled positions yet" } else { "v1 ~/positions: no active positions" } } } }
                                },
                                Err(message) => rsx! {
                                    tr { class: "empty-row", td { colspan: "9", p { class: "empty", "v1 ~/positions: {message}" } } }
                                },
                            }
                        }
                    }
                }
                if let Ok((portfolio, page_cursor)) = &page {
                    if page_cursor.is_some() || portfolio.next_cursor.is_some() {
                        div { class: "pager", role: "group", "aria-label": "Pages",
                            if page_cursor.is_some() { button { class: "ghost", onclick: move |_| cursor.set(None), "Newest" } }
                            if let Some(next) = portfolio.next_cursor.clone() { button { class: "ghost", onclick: move |_| cursor.set(Some(next.clone())), "Older" } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn IncomeChart(rows: Vec<IncomePoint>) -> Element {
    if rows.is_empty() {
        return rsx! {};
    }
    const DAY: i64 = 86_400;
    let today = now().div_euclid(DAY) * DAY;
    let values: Vec<_> = (0..30)
        .rev()
        .map(|days| {
            let day = today - days * DAY;
            let premium = rows
                .iter()
                .find(|row| row.day == day)
                .and_then(|row| row.premium.parse::<u128>().ok())
                .unwrap_or(0);
            (day, premium)
        })
        .collect();
    let peak = values
        .iter()
        .map(|(_, premium)| *premium)
        .max()
        .unwrap_or(0)
        .max(1);
    rsx! {
        figure { class: "income",
            figcaption { "Daily premiums · last 30 days (UTC)" }
            div { class: "income-bars", "aria-hidden": "true",
                for (day, premium) in &values {
                    span { key: "{day}",
                        style: "height: {(*premium as f64 / peak as f64 * 100.0):.2}%;",
                        title: "{utc(*day)} · {format_atoms(*premium, QUOTE_DECIMALS)} USDC" }
                }
            }
            details { class: "income-data",
                summary { "Daily amounts" }
                ul {
                    for (day, premium) in values.into_iter().filter(|(_, premium)| *premium > 0) {
                        li { key: "{day}", "{utc(day)} · {format_atoms(premium, QUOTE_DECIMALS)} USDC" }
                    }
                }
            }
        }
    }
}

#[component]
fn PositionDetail(address: String) -> Element {
    let market = use_context::<Signal<Markets>>();
    let connected = use_context::<Signal<Option<Connected>>>();
    let mut submitting = use_signal(|| false);
    let mut notice = use_signal(String::new);
    let mut position = use_resource(move || {
        let path = format!("/v1/positions/{address}");
        async move {
            let response =
                reqwest::get(absolute(&path).map_err(|_| "Finalized position unavailable")?)
                    .await
                    .map_err(|_| "Finalized position unavailable")?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err("Waiting for finalized position");
            }
            let position = response
                .error_for_status()
                .map_err(|_| "Finalized position unavailable")?
                .json::<Position>()
                .await
                .map_err(|_| "Finalized position unavailable")?;
            let state = get::<ExpiryState>(&format!("/v1/expiries/{}", position.expiry))
                .await
                .ok();
            Ok((position, state))
        }
    });
    use_future(move || async move {
        loop {
            TimeoutFuture::new(5_000).await;
            let refresh = position.peek().as_ref().is_none_or(|value| match value {
                Ok((row, _)) => row.outcome == 0,
                Err(_) => true,
            });
            if refresh {
                position.restart();
            }
        }
    });
    let (kept, _) = use_sticky(position);
    let result = kept().map(Ok).or_else(|| position.read().clone());
    rsx! {
        Page { title: "Position",
            match result.as_ref() {
                None => rsx! { Window { path: "~/positions", p { class: "empty", role: "status", "v1 ~/positions: loading finalized position" } } },
                Some(Err(error)) => rsx! { Window { path: "~/positions", p { class: "empty", role: "status", "v1 ~/positions: {error}" } } },
                Some(Ok((position, state))) => {
                    let base = format_atoms(position.quantity, BASE_DECIMALS);
                    let locked_quote = format_atoms(position.strike_amount, QUOTE_DECIMALS);
                    let put = position.kind == 1;
                    let settlement_amount = match (position.outcome, put) {
                        (1, true) | (2 | 3, false) => Some(format!("{base} WSOL")),
                        (1, false) | (2 | 3, true) => Some(format!("{locked_quote} USDC")),
                        _ => None,
                    };
                    let status = match (position.outcome, state.as_ref().map(|expiry| expiry.status)) {
                        (0, Some(1 | 2)) => "Ready to settle",
                        (0, Some(0)) if now() >= position.expires_at => "Awaiting expiry fix",
                        _ => position_status(position.outcome, position.expires_at),
                    };
                    let row = position.clone();
                    let deployment = market.read();
                    let cluster = deployment.0.as_ref().map(|market| market.cluster.as_str());
                    let historical_return = deployment.0.as_ref().and_then(|market| premium_return(
                        position.premium,
                        position.quantity,
                        if put { position.strike_amount } else { position.entry_spot.as_ref().map_or(0, |spot| spot.price) },
                        put,
                        position.expires_at.saturating_sub(position.opened_at),
                        !market.weekly,
                    ));
                    let address = position.address.to_string();
                    let path = format!("~/positions/{}…{}", &address[..4], &address[address.len() - 4..]);
                    rsx! {
                        Window { path,
                            div { class: "position-head",
                                div { class: "asset-cell", TokenIcon { symbol: "SOL" } strong { if put { "SOL · Cash-secured put" } else { "SOL · Covered call" } } }
                                span { class: "pill outcome-{position.outcome}", "{status}" }
                                Link { class: "earn-button", to: Route::Dashboard {}, "← Dashboard" }
                            }
                            div { class: "ticket-body",
                                div { class: "outcome",
                                    div { class: "outcome-head", "Position" }
                                    dl { class: "receipt",
                                        dt { "Position" } dd { "{position.address}" }
                                        dt { "Expiry" } dd { "{utc(position.expires_at)}" }
                                        dt { "Size" } dd { "{base} SOL" }
                                        dt { "Strike" } dd { "{format_atoms(position.strike, QUOTE_DECIMALS)} USDC per SOL" }
                                        dt { "Original collateral" } dd { if put { "{locked_quote} USDC" } else { "{base} WSOL" } }
                                        dt { "Upfront premium" } dd { class: "rate", "{format_atoms(position.premium, QUOTE_DECIMALS)} USDC" }
                                        if let Some(spot) = &position.entry_spot {
                                            dt { "Indicative entry spot" } dd { "${usd(spot.price)} · {utc(spot.observed_at)}" }
                                        }
                                        if let Some(returns) = historical_return {
                                            dt { "Premium return at fill" } dd { title: "Premium divided by collateral value at entry, using the term from fill to expiry.", "{returns}" }
                                        }
                                    }
                                    div { class: "outcome-head", "Settlement" }
                                    dl { class: "receipt",
                                        if position.outcome == 0 {
                                            dt { "At expiry" }
                                            dd {
                                                if put { "Below strike, receive {base} WSOL. At or above strike, receive {locked_quote} USDC." }
                                                else { "Above strike, receive {locked_quote} USDC. At or below strike, receive {base} WSOL." }
                                            }
                                        }
                                        if let Some(fix) = state.as_ref().and_then(|expiry| expiry.fix.as_ref()) {
                                            dt { "Verified SOL/USD fix" } dd { "${format_atoms(fix.price, QUOTE_DECIMALS)} · {utc(fix.observed_at)}" }
                                        }
                                        if let Some(amount) = settlement_amount {
                                            dt { "Paid at settlement" } dd { "{amount}" }
                                        }
                                        if let Some(signature) = &position.opened_signature {
                                            dt { "Fill signature" }
                                            dd { TransactionSignature { signature: signature.clone(), cluster: cluster.map(str::to_owned) } }
                                        }
                                        if let Some(signature) = &position.settled_signature {
                                            dt { "Settlement signature" }
                                            dd { TransactionSignature { signature: signature.clone(), cluster: cluster.map(str::to_owned) } }
                                        }
                                    }
                                }
                                if status == "Ready to settle" && connected().is_some() {
                                    div { class: "cta",
                                        button { class: "primary", disabled: submitting(), onclick: move |_| {
                                            let Some(market) = market.peek().0.clone() else { return; };
                                            let Some(wallet) = connected() else { return; };
                                            let row = row.clone();
                                            submitting.set(true);
                                            spawn(async move {
                                                match submit::settle(market, wallet, row).await {
                                                    Ok(message) | Err(message) => notice.set(message),
                                                }
                                                submitting.set(false);
                                            });
                                        }, "Settle position" }
                                        if !notice().is_empty() { p { class: "transaction-status", role: "status", "{notice}" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn premium_rates(
    premium: u64,
    quantity: u64,
    reference: u64,
    put: bool,
    seconds: i64,
) -> Option<(f64, f64)> {
    let collateral = if put {
        reference as f64
    } else {
        quantity as f64 * reference as f64 / 1_000_000_000.0
    };
    if collateral <= 0.0 || seconds <= 0 {
        return None;
    }
    let term = 100.0 * premium as f64 / collateral;
    Some((term, term * 31_536_000.0 / seconds as f64))
}

fn premium_return(
    premium: u64,
    quantity: u64,
    reference: u64,
    put: bool,
    seconds: i64,
    demo: bool,
) -> Option<String> {
    let (term, apr) = premium_rates(premium, quantity, reference, put, seconds)?;
    Some(if demo {
        format!("{term:.2}% term return · demo expiry")
    } else {
        format!("{term:.2}% term return · {apr:.2}% APR")
    })
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn date_parts(timestamp: i64) -> (u32, &'static str, u32, u32, u32) {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp as f64 * 1_000.0));
    (
        date.get_utc_full_year(),
        MONTHS[date.get_utc_month() as usize % 12],
        date.get_utc_date(),
        date.get_utc_hours(),
        date.get_utc_minutes(),
    )
}

/// Compact expiry label, for example `Nov_13 22:16`.
fn expiry_label(timestamp: i64) -> String {
    let (_, month, day, hour, minute) = date_parts(timestamp);
    format!("{month}_{day:02} {hour:02}:{minute:02}")
}

/// Long expiry date, for example `Nov 13th, 2026`.
fn long_date(timestamp: i64) -> String {
    let (year, month, day, _, _) = date_parts(timestamp);
    let suffix = match (day % 10, day % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{month} {day}{suffix}, {year}")
}

fn time_left(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = (seconds as f64 / 86_400.0).round() as i64;
    if seconds >= 86_400 {
        format!("in {days} day{}", if days == 1 { "" } else { "s" })
    } else if seconds >= 3_600 {
        format!("in {}h {}m", seconds / 3_600, seconds % 3_600 / 60)
    } else {
        format!("in {}m", (seconds / 60).max(1))
    }
}

#[component]
fn TransactionSignature(signature: String, cluster: Option<String>) -> Element {
    let suffix = match cluster.as_deref() {
        Some("solana:mainnet") => "",
        Some("solana:devnet") => "?cluster=devnet",
        Some("solana:testnet") => "?cluster=testnet",
        _ => return rsx! { "{signature}" },
    };
    rsx! {
        a { href: "https://explorer.solana.com/tx/{signature}{suffix}", target: "_blank", rel: "noopener noreferrer", "{signature}" }
    }
}

fn position_status(outcome: u8, expires_at: i64) -> &'static str {
    match outcome {
        0 if now() < expires_at => "Active",
        0 => "Awaiting expiry fix or settlement",
        1 => "Assigned",
        2 => "Expired",
        3 => "Refunded after oracle timeout",
        _ => "Unknown outcome",
    }
}

pub async fn get<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let url = absolute(path)?;
    reqwest::get(url)
        .await
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .await
        .map_err(|err| err.to_string())
}

pub fn absolute(path: &str) -> Result<String, String> {
    let origin = web_sys::window()
        .ok_or("Browser unavailable")?
        .location()
        .origin()
        .map_err(|_| "Origin unavailable")?;
    Ok(format!("{origin}{path}"))
}

async fn request_quote(terms: Terms) -> Result<Option<ReviewedQuote>, String> {
    let request: Request = reqwest::Client::new()
        .post(absolute("/v1/rfqs")?)
        .json(&terms)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())?;
    if request.terms != terms {
        return Err("Terms changed".into());
    }
    for _ in 0..22 {
        TimeoutFuture::new(150).await;
        match get::<Auction>(&format!("/v1/rfqs/{}", request.id)).await? {
            Auction::Pending => continue,
            Auction::NoQuote => return Ok(None),
            Auction::Quoted { bid, spot, .. } if bid.id == request.id => {
                return Ok(Some(ReviewedQuote { request, bid, spot }))
            }
            _ => return Err("Unexpected RFQ response".into()),
        }
    }
    Err("Quote request timed out".into())
}

pub fn now() -> i64 {
    (js_sys::Date::now() / 1_000.0) as i64
}

/// Dollar price in USDC atoms, rounded to cents for display.
fn usd(atoms: u64) -> String {
    format!(
        "{:.2}",
        atoms as f64 / 10_f64.powi(i32::from(QUOTE_DECIMALS))
    )
}

fn short(address: Address) -> String {
    let text = address.to_string();
    format!("{}…{}", &text[..4], &text[text.len() - 4..])
}

fn utc(timestamp: i64) -> String {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp as f64 * 1_000.0))
        .to_utc_string()
        .into()
}

fn format_atoms(amount: impl Into<u128>, decimals: u8) -> String {
    let amount = amount.into();
    let scale = 10_u128.pow(u32::from(decimals));
    let remainder = amount % scale;
    if remainder == 0 {
        return (amount / scale).to_string();
    }
    let fraction = format!("{remainder:0width$}", width = usize::from(decimals));
    format!("{}.{}", amount / scale, fraction.trim_end_matches('0'))
}

fn parse_atoms(text: &str, decimals: u8) -> Option<u64> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let fraction_length = u8::try_from(fraction.len()).ok()?;
    let padding = decimals.checked_sub(fraction_length)?;
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().ok()?
    };
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()?
    };
    let amount = whole
        .checked_mul(10_u64.pow(u32::from(decimals)))?
        .checked_add(fraction.checked_mul(10_u64.pow(u32::from(padding)))?)?;
    (amount > 0).then_some(amount)
}
