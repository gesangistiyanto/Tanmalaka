use polymarket_maker::{clob, config, market, onchain, strategy, tui, ws};

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    // ── CLI args ──────────────────────────────────────────────────────────────
    let use_tui = std::env::args().any(|a| a == "--tui");

    // ── Logging ───────────────────────────────────────────────────────────────
    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("polymarket_maker=info".parse()?)
        .add_directive("info".parse()?);

    if use_tui {
        // Create a TuiHandle early so we can wire the tracing layer before any
        // log events fire. dry_run is initially false; we fix it after loading
        // the config (the shared Arc means the TUI sees the updated value).
        // TuiHandle is created before config so we can wire tracing.
        // Balance is patched in after config loads.
        let handle = tui::TuiHandle::new(false);
        let layer = tui::TuiLogLayer {
            handle: handle.clone(),
        };
        tracing_subscriber::registry()
            .with(env_filter)
            .with(layer)
            .init();

        // ── Config ────────────────────────────────────────────────────────────
        let config = Config::from_env().context("Failed to load config")?;

        // Patch dry_run + initial balance into the already-shared state
        {
            let mut state = handle.state.write().await;
            state.dry_run = config.dry_run;
            state.sim_balance = config.sim_initial_balance;
        }

        run_inner(config, Some(handle)).await
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .init();

        // ── Config ────────────────────────────────────────────────────────────
        let config = Config::from_env().context("Failed to load config")?;

        run_inner(config, None).await
    }
}

async fn run_inner(config: Config, tui_handle: Option<tui::TuiHandle>) -> Result<()> {
    info!("=== Polymarket Maker Rebate Bot ===");
    info!("Wallet mode  : {:?}", config.wallet_mode);
    info!("Assets       : {:?}", config.assets);
    info!("Duration     : {}", config.duration);
    info!("Trade size   : {} shares/side", config.trade_size);
    info!("Max combined : ${}", config.max_combined);
    info!("Price range  : ${} – ${}", config.min_price, config.max_price);
    info!("Cut-loss at  : {}s before close", config.cut_loss_time);
    info!("Re-entry     : {}", if config.reentry_enabled { "enabled" } else { "disabled" });
    info!("Mode         : {}", if config.dry_run { "SIMULATION" } else { "LIVE" });
    info!("==========================================");

    // ── Signer ───────────────────────────────────────────────────────────────
    let signer: PrivateKeySigner = config
        .private_key
        .parse()
        .context("Invalid PRIVATE_KEY")?;
    let eoa = format!("{}", signer.address());
    info!("EOA address  : {}", eoa);
    if !config.proxy_wallet.as_deref().unwrap_or("").is_empty() {
        info!("Proxy wallet : {}", config.proxy_wallet.as_deref().unwrap());
    }

    // ── CLOB client ───────────────────────────────────────────────────────────
    let mut clob = clob::ClobClient::new(signer.clone(), &config);
    clob.init().await.context("CLOB client init failed")?;
    info!("CLOB: wallet address: {}", clob.wallet_address);
    let clob = Arc::new(clob);

    // ── On-chain CTF client ───────────────────────────────────────────────────
    let ctf = onchain::ctf::CtfClient::new(signer.clone(), &config)
        .await
        .context("CTF client init failed")?;

    // ── WebSocket services ────────────────────────────────────────────────────
    let orderbook = ws::orderbook::spawn(vec![]);
    let fills = ws::fills::spawn(clob.wallet_address.clone());
    let price_watcher = ws::price::spawn();

    info!("WebSocket services started");

    // ── TUI (if enabled) ─────────────────────────────────────────────────────
    if let Some(ref handle) = tui_handle {
        let wallet_addr = clob.wallet_address.clone();
        let ob2 = orderbook.clone();
        let pw2 = price_watcher.clone();
        let cfg2 = config.clone();
        tokio::spawn(tui::run_tui(handle.clone(), ob2, pw2, cfg2, wallet_addr));
    }

    // ── Market detector ───────────────────────────────────────────────────────
    let mut market_rx = market::detector::spawn(config.clone());
    info!(
        "Market detector started — polling {} {} up-down markets every {}s",
        config.assets.join(",").to_uppercase(),
        config.duration,
        config.poll_interval_ms / 1000,
    );

    // Per-asset concurrency control
    let running: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let pending: Arc<Mutex<HashMap<String, market::types::Market>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ── Main event loop ───────────────────────────────────────────────────────
    while let Some(market) = market_rx.recv().await {
        let asset = market.asset.clone();

        // Subscribe to orderbook and price early so data accumulates
        orderbook.subscribe(&[&market.yes_token_id, &market.no_token_id]);
        let chainlink_sym = market.chainlink_symbol();
        price_watcher.subscribe(&chainlink_sym);

        let is_busy = running.lock().await.contains(&asset);
        if is_busy {
            info!(
                "MakerMM[{}]: asset busy — queuing \"{}\"",
                asset.to_uppercase(),
                market.question.chars().take(40).collect::<String>()
            );
            pending.lock().await.insert(asset.clone(), market);
            continue;
            // NOTE: TUI market update happens when execution actually starts (in run_asset)
        }

        let clob2 = clob.clone();
        let ctf2 = ctf.clone();
        let ob2 = orderbook.clone();
        let fills2 = fills.clone();
        let cfg2 = config.clone();
        let running2 = running.clone();
        let pending2 = pending.clone();
        let tui2 = tui_handle.clone();

        running.lock().await.insert(asset.clone());

        tokio::spawn(run_asset(market, cfg2, clob2, ctf2, ob2, fills2, running2, pending2, tui2));
    }

    warn!("Market detector channel closed — shutting down");
    Ok(())
}

/// Run the maker rebate strategy for one asset, with optional re-entry
/// and queued-market handoff.
///
/// Returns a boxed Send future to break the circular Send inference that
/// would occur with a plain `async fn` that spawns itself.
fn run_asset(
    market: market::types::Market,
    config: Config,
    clob: Arc<clob::ClobClient>,
    ctf: Arc<onchain::ctf::CtfClient>,
    ob: ws::orderbook::OrderbookWatcher,
    fills: ws::fills::FillWatcher,
    running: Arc<Mutex<HashSet<String>>>,
    pending: Arc<Mutex<HashMap<String, market::types::Market>>>,
    tui_handle: Option<tui::TuiHandle>,
) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
        let asset = market.asset.clone();
        let current_market = market;

        // Minimum time remaining to bother re-entering:
        //   reentry_delay + 10s entry wait + cut_loss_time
        let min_reentry_ms = config.reentry_delay_ms as i64
            + 10_000
            + config.cut_loss_time as i64 * 1000;

        loop {
            // Force a WS reconnect so the server sends fresh snapshots for all
            // subscribed tokens before each attempt.
            ob.reconnect();

            // Update TUI to the market currently being executed (not just when detected)
            if let Some(ref t) = tui_handle {
                let chainlink_sym = current_market.chainlink_symbol();
                let am = tui::ActiveMarket {
                    yes_token_id: current_market.yes_token_id.clone(),
                    no_token_id: current_market.no_token_id.clone(),
                    tick_size: current_market.tick_size,
                    question: current_market.question.chars().take(60).collect(),
                    end_time_ms: current_market.end_time.timestamp_millis(),
                    chainlink_symbol: chainlink_sym,
                    resolution_ts_ms: current_market.resolution_ts * 1_000,
                    locked_ref_price: None,
                };
                t.set_market(&asset, Some(am)).await;
            }

            let result = strategy::executor::run_once(
                &current_market,
                &config,
                clob.clone(),
                ctf.clone(),
                &ob,
                &fills,
                tui_handle.clone(),
            )
            .await;

            if result.one_sided {
                warn!(
                    "MakerMM[{}]: cycle ended one-sided — stopping re-entry",
                    asset.to_uppercase()
                );
                break;
            }

            let ms_left = current_market.end_time.timestamp_millis() - now_ms();

            if config.reentry_enabled && ms_left > min_reentry_ms {
                info!(
                    "MakerMM[{}]: waiting {}s for re-entry ({}s remaining)...",
                    asset.to_uppercase(),
                    config.reentry_delay_ms / 1000,
                    ms_left / 1000
                );
                sleep(Duration::from_millis(config.reentry_delay_ms)).await;
                continue;
            }

            break;
        }

        if let Some(ref t) = tui_handle {
            t.set_market(&asset, None).await;
            t.set_position(&asset, None).await;
        }

        running.lock().await.remove(&asset);

        // Hand off to queued market (if any)
        let queued = { pending.lock().await.remove(&asset) };
        if let Some(queued) = queued {
            let secs_left = ((queued.end_time.timestamp_millis() - now_ms()) / 1000).max(0) as u64;

            if secs_left > config.cut_loss_time {
                info!(
                    "MakerMM[{}]: executing queued \"{}\" ({}s left)",
                    asset.to_uppercase(),
                    queued.question.chars().take(40).collect::<String>(),
                    secs_left
                );
                // Subscribe orderbook for the queued market's tokens
                ob.subscribe(&[&queued.yes_token_id, &queued.no_token_id]);

                running.lock().await.insert(asset.clone());

                tokio::spawn(run_asset(
                    queued,
                    config,
                    clob,
                    ctf,
                    ob,
                    fills,
                    running,
                    pending,
                    tui_handle,
                ));
            } else {
                warn!(
                    "MakerMM[{}]: queued market expired ({}s) — discarding",
                    asset.to_uppercase(),
                    secs_left
                );
            }
        }
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
