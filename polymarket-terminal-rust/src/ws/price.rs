//! Real-time Chainlink prices via Polymarket live-data WebSocket.
//!
//! Connect:   wss://ws-live-data.polymarket.com/
//! Subscribe: {"action":"subscribe","subscriptions":[{"topic":"crypto_prices_chainlink",
//!              "type":"update","filters":"{\"symbol\":\"btc/usd\"}"}]}
//!
//! Initial response (type="subscribe", topic="crypto_prices"):
//!   payload.data = [{timestamp, value}, ...]  — historical prices
//!
//! Live updates (type="update", topic="crypto_prices_chainlink"):
//!   payload = {symbol, timestamp, value, full_accuracy_value}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const WS_URL: &str = "wss://ws-live-data.polymarket.com/";
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

// ── Shared price data ─────────────────────────────────────────────────────────

#[derive(Default)]
pub struct SymbolPrices {
    /// Latest live price.
    pub current: Option<f64>,
    /// Historical + live prices keyed by timestamp_ms.
    pub history: HashMap<i64, f64>,
}

type PriceMap = Arc<DashMap<String, SymbolPrices>>;

// ── PriceWatcher ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PriceWatcher {
    data: PriceMap,
    sub_tx: mpsc::UnboundedSender<String>,
}

impl PriceWatcher {
    /// Subscribe to Chainlink price updates for a symbol (e.g., "btc/usd").
    /// Call this before or after spawning — the background task will (re)subscribe
    /// on the live connection.
    pub fn subscribe(&self, symbol: &str) {
        let sym = symbol.to_lowercase();
        self.data.entry(sym.clone()).or_default();
        let _ = self.sub_tx.send(sym);
    }

    /// Latest live Chainlink price for the symbol.
    pub fn current_price(&self, symbol: &str) -> Option<f64> {
        self.data.get(&symbol.to_lowercase())?.current
    }

    /// Price at (or within ±10 s of) the given resolution timestamp in ms.
    /// History is stored with ms precision (per-second feed timestamps).
    /// Returns `None` if history hasn't been received yet.
    pub fn reference_price(&self, symbol: &str, resolution_ts_ms: i64) -> Option<f64> {
        let entry = self.data.get(&symbol.to_lowercase())?;
        if entry.history.is_empty() {
            return None;
        }

        // Exact ms match (most common — per-second feed has ts on exact second boundaries)
        if let Some(&p) = entry.history.get(&resolution_ts_ms) {
            return Some(p);
        }

        // Closest within ±10 seconds (10_000 ms)
        entry
            .history
            .iter()
            .filter(|(&ts, _)| (ts - resolution_ts_ms).abs() <= 10_000)
            .min_by_key(|(&ts, _)| (ts - resolution_ts_ms).abs())
            .map(|(_, &p)| p)
    }
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

pub fn spawn() -> PriceWatcher {
    let data: PriceMap = Arc::new(DashMap::new());
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<String>();

    let data2 = data.clone();

    // Dispatcher: each new symbol gets its own independent WS task.
    // This avoids re-subscription race conditions where the server only
    // delivers live updates for symbols present at connection time.
    // NOTE: subscribe() already inserts into `data` before sending to the
    // channel, so we use a separate HashSet to track spawned symbols.
    tokio::spawn(async move {
        let mut spawned: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(sym) = sub_rx.recv().await {
            if spawned.contains(&sym) {
                continue;
            }
            spawned.insert(sym.clone());
            let data3 = data2.clone();
            tokio::spawn(async move {
                run_symbol_ws(sym, data3).await;
            });
        }
    });

    PriceWatcher { data, sub_tx }
}

/// Dedicated WS task for a single symbol — reconnects automatically on disconnect.
async fn run_symbol_ws(symbol: String, data: PriceMap) {
    loop {
        info!("Price WS [{}]: connecting", symbol);
        match connect_async(WS_URL).await {
            Err(e) => {
                error!("Price WS [{}]: connect error: {}", symbol, e);
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Ok((mut ws, _)) => {
                let msg = build_sub_msg(&[symbol.clone()]);
                if let Err(e) = ws.send(Message::Text(msg)).await {
                    warn!("Price WS [{}]: subscribe error: {}", symbol, e);
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
                info!("Price WS [{}]: subscribed", symbol);

                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => handle_message(&text, &data),
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                warn!("Price WS [{}]: disconnected — reconnecting in {}s",
                    symbol, RECONNECT_DELAY.as_secs());
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

// ── Message helpers ───────────────────────────────────────────────────────────

/// Build one subscription message covering ALL symbols in a single request.
/// Batching prevents the server from replacing earlier symbol subscriptions
/// when a new per-symbol message is sent.
fn build_sub_msg(symbols: &[String]) -> String {
    let subscriptions: Vec<serde_json::Value> = symbols
        .iter()
        .flat_map(|sym| {
            let filters = format!("{{\"symbol\":\"{}\"}}", sym);
            [
                serde_json::json!({
                    "topic": "crypto_prices",
                    "type": "update",
                    "filters": filters
                }),
                serde_json::json!({
                    "topic": "crypto_prices_chainlink",
                    "type": "update",
                    "filters": filters
                }),
            ]
        })
        .collect();

    serde_json::json!({
        "action": "subscribe",
        "subscriptions": subscriptions
    })
    .to_string()
}

fn handle_message(text: &str, data: &PriceMap) {
    let val: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let topic = val.get("topic").and_then(|v| v.as_str()).unwrap_or("");
    let payload = match val.get("payload") {
        Some(p) => p,
        None => return,
    };

    match (msg_type, topic) {
        // ── Initial subscribe response: bulk historical data ────────────────
        // Response has type:"subscribe", topic:"crypto_prices" or "crypto_prices_chainlink"
        // payload = { symbol, data: [{timestamp_ms, value}, ...] }
        ("subscribe", _) => {
            let symbol = payload
                .get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if symbol.is_empty() {
                return;
            }

            if let Some(arr) = payload.get("data").and_then(|v| v.as_array()) {
                let mut entry = data.entry(symbol.clone()).or_default();
                let mut latest_ts = 0i64;
                let mut latest_price = 0f64;

                for item in arr {
                    let ts = item.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
                    let price = item.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if ts > 0 && price > 0.0 {
                        entry.history.insert(ts, price);
                        if ts > latest_ts {
                            latest_ts = ts;
                            latest_price = price;
                        }
                    }
                }

                if latest_ts > 0 {
                    entry.current = Some(latest_price);
                }

                info!("Price WS [{}]: {} history pts, current={:.5?}, topic={}",
                    symbol, entry.history.len(), entry.current, topic);
            }
        }

        // ── Real-time price update (per-second feed or Chainlink) ─────────
        // payload = { symbol, timestamp_ms, value }
        ("update", "crypto_prices") | ("update", "crypto_prices_chainlink") => {
            let symbol = payload
                .get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if symbol.is_empty() {
                return;
            }

            let ts = payload.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            let price = payload.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);

            if ts > 0 && price > 0.0 {
                let mut entry = data.entry(symbol).or_default();
                entry.history.insert(ts, price);
                entry.current = Some(price);
            }
        }

        _ => {
            debug!("Price WS: unhandled msg type={} topic={}", msg_type, topic);
        }
    }
}
