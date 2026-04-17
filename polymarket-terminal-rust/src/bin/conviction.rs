//! Polymarket Conviction Bot — standalone binary.
//!
//! Watches detected markets and fires conviction trades based on:
//!   - PANIC signal: sudden 2× price jump → contrarian market order
//!   - TREND signal: stable consecutive price rises → momentum market order
//!
//! Run modes:
//!   ./conviction               — plain log output (no TUI)
//!   ./conviction --tui         — ratatui TUI

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Timelike;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use polymarket_maker::{clob, config, market, onchain, strategy, tui, ws};
use polymarket_maker::onchain::ctf::CtfClient;

#[tokio::main]
async fn main() -> Result<()> {
    let use_tui = std::env::args().any(|a| a == "--tui");

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("polymarket_maker=info".parse()?)
        .add_directive("info".parse()?);

    if use_tui {
        let handle = tui::TuiHandle::new(false);
        let layer = tui::TuiLogLayer { handle: handle.clone() };
        tracing_subscriber::registry()
            .with(env_filter)
            .with(layer)
            .init();

        let cfg = config::Config::from_env().context("Failed to load config")?;
        {
            let mut s = handle.state.write().await;
            s.dry_run = cfg.dry_run;
            s.sim_balance = cfg.sim_initial_balance;
        }
        run_conviction(cfg, Some(handle)).await
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
        let cfg = config::Config::from_env().context("Failed to load config")?;
        run_conviction(cfg, None).await
    }
}

async fn run_conviction(cfg: config::Config, tui_handle: Option<tui::TuiHandle>) -> Result<()> {
    info!("=== Polymarket Conviction Bot ===");
    info!("Assets       : {:?}", cfg.assets);
    info!("Duration     : {}", cfg.duration);
    info!("Cut-loss at  : {}s before close", cfg.cut_loss_time);
    info!("Poll ms      : {}ms", cfg.poll_ms);
    info!("Mode         : {}", if cfg.dry_run { "SIMULATION" } else { "LIVE" });
    info!("==========================================");

    let wallet_addr: String = match cfg.private_key.parse::<PrivateKeySigner>() {
        Ok(signer) => format!("{}", signer.address()),
        Err(_) => "(invalid key)".to_string(),
    };
    info!("EOA address  : {}", wallet_addr);

    // ── WebSocket services ────────────────────────────────────────────────────
    let orderbook = ws::orderbook::spawn(vec![]);
    let price_watcher = ws::price::spawn();
    info!("WebSocket services started");

    // ── TUI ───────────────────────────────────────────────────────────────────
    if let Some(ref handle) = tui_handle {
        let ob2  = orderbook.clone();
        let pw2  = price_watcher.clone();
        let cfg2 = cfg.clone();
        let wa   = wallet_addr.clone();
        tokio::spawn(tui::run_conviction_tui(handle.clone(), ob2, pw2, cfg2, wa));
    }

    // ── Live balance poller (only in live mode) ───────────────────────────────
    // USDC fetched from wallet address (proxy in proxy mode), POL from EOA (gas payer).
    if !cfg.dry_run {
        if let Some(ref handle) = tui_handle {
            let handle2   = handle.clone();
            let rpc_url   = cfg.polygon_rpc_url.clone();
            let usdc_addr = cfg.wallet_address(&wallet_addr); // proxy or EOA for USDC
            let pol_addr  = wallet_addr.clone();              // always EOA for gas
            tokio::spawn(async move {
                loop {
                    match onchain::ctf::fetch_live_balances(&rpc_url, &usdc_addr, &pol_addr).await {
                        Ok((usdc, pol)) => {
                            handle2.set_live_balances(usdc, pol).await;
                        }
                        Err(e) => {
                            tracing::warn!("Balance fetch failed: {}", e);
                        }
                    }
                    sleep(Duration::from_secs(30)).await;
                }
            });
        }
    }

    // ── CLOB client (live mode only) ──────────────────────────────────────────
    let clob_client: Option<Arc<clob::ClobClient>> = if cfg.dry_run {
        info!("Mode: SIMULATION — orders will NOT be submitted");
        None
    } else {
        match cfg.private_key.parse::<PrivateKeySigner>() {
            Ok(signer) => {
                let mut c = clob::ClobClient::new(signer, &cfg);
                match c.init().await {
                    Ok(()) => {
                        info!("CLOB client ready — live orders enabled");
                        Some(Arc::new(c))
                    }
                    Err(e) => {
                        warn!("CLOB init failed ({}), falling back to simulation", e);
                        None
                    }
                }
            }
            Err(e) => {
                warn!("Invalid private key ({}), falling back to simulation", e);
                None
            }
        }
    };

    // ── CTF client for on-chain redemption (live mode only) ───────────────────
    let ctf_client: Option<Arc<CtfClient>> = if cfg.dry_run {
        None
    } else {
        match cfg.private_key.parse::<PrivateKeySigner>() {
            Ok(signer) => {
                match CtfClient::new(signer, &cfg).await {
                    Ok(c) => {
                        info!("CTF client ready — on-chain redemption enabled");
                        Some(c)
                    }
                    Err(e) => {
                        warn!("CTF client init failed ({}), redemption disabled", e);
                        None
                    }
                }
            }
            Err(e) => {
                warn!("Invalid private key ({}), CTF client disabled", e);
                None
            }
        }
    };

    // ── Market detector ───────────────────────────────────────────────────────
    let mut market_rx = market::detector::spawn(cfg.clone());
    info!(
        "Market detector started — polling {} {} markets every {}s",
        cfg.assets.join(",").to_uppercase(),
        cfg.duration,
        cfg.poll_interval_ms / 1000,
    );

    // Per-asset concurrency state
    let running: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // Queued next market per asset (waiting for current to finish)
    let pending: Arc<Mutex<HashMap<String, market::types::Market>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some(m) = market_rx.recv().await {
        let asset = m.asset.clone();

        // Subscribe WS tokens immediately so data starts accumulating
        orderbook.subscribe(&[&m.yes_token_id, &m.no_token_id]);
        price_watcher.subscribe(&m.chainlink_symbol());

        if running.lock().await.contains(&asset) {
            // Already monitoring this asset — queue the next market
            info!(
                "Conviction[{}]: queuing next market \"{}\"",
                asset.to_uppercase(),
                m.question.chars().take(40).collect::<String>()
            );
            pending.lock().await.insert(asset, m);
            continue;
        }

        // First market for this asset: set TUI and launch task
        if let Some(ref t) = tui_handle {
            t.set_market_for_asset(&asset, build_active_market(&m, &cfg)).await;
        }
        running.lock().await.insert(asset.clone());

        // Reconnect WS so server re-sends fresh snapshots for all subscribed tokens
        orderbook.reconnect();

        tokio::spawn(run_market_task(
            m,
            cfg.clone(),
            orderbook.clone(),
            price_watcher.clone(),
            running.clone(),
            pending.clone(),
            tui_handle.clone(),
            clob_client.clone(),
            ctf_client.clone(),
        ));
    }

    warn!("Market detector channel closed — shutting down");
    Ok(())
}

/// Run the conviction strategy for one market, then wait until 10s before that
/// market ends before handing off to the queued next market (if any).
///
/// Returns a boxed Send future to break the recursive Send inference.
fn run_market_task(
    market: market::types::Market,
    config: config::Config,
    ob: ws::orderbook::OrderbookWatcher,
    prices: ws::price::PriceWatcher,
    running: Arc<Mutex<HashSet<String>>>,
    pending: Arc<Mutex<HashMap<String, market::types::Market>>>,
    tui_handle: Option<tui::TuiHandle>,
    clob_client: Option<Arc<clob::ClobClient>>,
    ctf_client: Option<Arc<CtfClient>>,
) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
    let asset = market.asset.clone();

    if config.is_in_stop_window() {
        warn!(
            "Conviction[{}]: skipped — currently in stop window (UTC {:02}:{:02})",
            asset.to_uppercase(),
            chrono::Utc::now().hour(),
            chrono::Utc::now().minute(),
        );
        running.lock().await.remove(&asset);
        return;
    }

    strategy::conviction::run_once(
        &market, &config, &ob, &prices,
        tui_handle.as_ref(),
        clob_client.as_deref(),
        ctf_client.clone(),
    ).await;

    // ── Wait until 10s before current market ends, then hand off ─────────────
    // This keeps the current market's orderbook visible in TUI until just before
    // the new market starts, then reconnects the socket for fresh snapshots.
    let switch_at_ms = market.end_time.timestamp_millis() - 10_000;
    let wait_ms = (switch_at_ms - now_ms()).max(0) as u64;
    if wait_ms > 0 {
        info!(
            "Conviction[{}]: strategy done — switching TUI in {}s",
            asset.to_uppercase(),
            wait_ms / 1000
        );
        sleep(Duration::from_millis(wait_ms)).await;
    }

    running.lock().await.remove(&asset);
    // Remove this asset's orderbook panel from TUI once its slot ends
    if let Some(ref t) = tui_handle {
        t.remove_market_for_asset(&asset).await;
    }

    // ── Hand off to queued market if still valid ──────────────────────────────
    let queued = pending.lock().await.remove(&asset);
    if let Some(qm) = queued {
        let ms_left = qm.end_time.timestamp_millis() - now_ms();
        if ms_left > config.cut_loss_time as i64 * 1_000 {
            info!(
                "Conviction[{}]: switching to \"{}\" ({}s left)",
                asset.to_uppercase(),
                qm.question.chars().take(40).collect::<String>(),
                ms_left / 1000
            );

            // Register next market panel in TUI
            if let Some(ref t) = tui_handle {
                t.set_market_for_asset(&qm.asset, build_active_market(&qm, &config)).await;
            }

            // Disconnect + reconnect so WS server sends fresh snapshots for new tokens
            ob.reconnect();

            running.lock().await.insert(asset);
            tokio::spawn(run_market_task(qm, config, ob, prices, running, pending, tui_handle, clob_client, ctf_client));
        } else {
            warn!(
                "Conviction[{}]: queued market expired ({}s left) — discarding",
                asset.to_uppercase(),
                ms_left / 1000
            );
        }
    }
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_active_market(m: &market::types::Market, _cfg: &config::Config) -> tui::ActiveMarket {
    tui::ActiveMarket {
        yes_token_id:     m.yes_token_id.clone(),
        no_token_id:      m.no_token_id.clone(),
        tick_size:        m.tick_size,
        question:         m.question.chars().take(60).collect(),
        end_time_ms:      m.end_time.timestamp_millis(),
        chainlink_symbol: m.chainlink_symbol(),
        resolution_ts_ms: m.resolution_ts * 1_000,
        locked_ref_price: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
