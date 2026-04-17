//! Real-time orderbook via Polymarket CLOB WebSocket.
//!
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/market and
//! maintains a local bid/ask snapshot for subscribed token IDs.
//!
//! Message protocol:
//!   Subscribe  → {"assets_ids": ["tokenId"], "type": "market"}
//!   Snapshot   ← {"event_type": "book", "asset_id": "...", "bids": [...], "asks": [...]}
//!   Delta      ← {"event_type": "price_change", "asset_id": "...", "changes": [...]}

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL: Duration = Duration::from_secs(10);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

// ── Shared state ──────────────────────────────────────────────────────────────

/// One side of the orderbook: price → size.
type PriceLevel = HashMap<u64, u64>; // price in ticks (×1e6), size in micro-shares (×1e6)

#[derive(Default)]
pub struct TokenBook {
    pub bids: PriceLevel, // buy side
    pub asks: PriceLevel, // sell side
    /// Unix ms timestamp of the last WS or REST update. 0 = never updated.
    pub last_update_ms: i64,
}

impl TokenBook {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids
            .iter()
            .filter(|(_, &sz)| sz > 0)
            .max_by_key(|(&p, _)| p)
            .map(|(&p, _)| p as f64 / 1_000_000.0)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks
            .iter()
            .filter(|(_, &sz)| sz > 0)
            .min_by_key(|(&p, _)| p)
            .map(|(&p, _)| p as f64 / 1_000_000.0)
    }

    pub fn has_data(&self) -> bool {
        !self.bids.is_empty() || !self.asks.is_empty()
    }
}

/// Thread-safe orderbook state shared between the WS task and strategy.
pub type Books = Arc<DashMap<String, TokenBook>>;

// ── WS message types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Level {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct Change {
    side: String,
    price: String,
    size: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn parse_price(s: &str) -> u64 {
    (s.parse::<f64>().unwrap_or(0.0) * 1_000_000.0).round() as u64
}

fn parse_size(s: &str) -> u64 {
    (s.parse::<f64>().unwrap_or(0.0) * 1_000_000.0).round() as u64
}

// ── Watcher ───────────────────────────────────────────────────────────────────

/// Handle to the orderbook watcher for subscribing and reading prices.
#[derive(Clone)]
pub struct OrderbookWatcher {
    books: Books,
    /// Broadcasts the token_id that was just updated. Only callers watching
    /// specific token IDs will be woken — old-market snapshots are ignored.
    update_tx: Arc<broadcast::Sender<String>>,
    subscribe_tx: tokio::sync::mpsc::UnboundedSender<Vec<String>>,
    /// Sending any value here triggers a graceful disconnect → immediate reconnect,
    /// causing the server to re-send fresh snapshots for all subscribed tokens.
    reconnect_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl OrderbookWatcher {
    /// Subscribe to orderbook updates for the given token IDs.
    /// Safe to call multiple times — duplicates are filtered in the WS task.
    pub fn subscribe(&self, token_ids: &[&str]) {
        let ids: Vec<String> = token_ids.iter().map(|s| s.to_string()).collect();
        for id in &ids {
            self.books.entry(id.clone()).or_default();
        }
        let _ = self.subscribe_tx.send(ids);
    }

    /// Force an immediate reconnect of the WS connection.
    ///
    /// The WS task will close the current connection and reconnect instantly
    /// (no backoff delay).  On reconnect the server re-sends full snapshots for
    /// every subscribed token, which is the most reliable way to get fresh data
    /// when transitioning to a new market.
    pub fn reconnect(&self) {
        let _ = self.reconnect_tx.send(());
    }

    /// Fetch the current orderbook from the CLOB REST API and populate the local
    /// book.  Used as a bootstrap when the WS server hasn't sent a snapshot yet
    /// (e.g. a market that hasn't opened, or new tokens not yet known to the WS).
    pub async fn bootstrap_rest(&self, clob_host: &str, token_id: &str) {
        if self.has_data(token_id) {
            return;
        }
        let url = format!("{}/book?token_id={}", clob_host, token_id);
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let resp = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!(
                    "Orderbook REST {}: status {}",
                    &token_id[token_id.len().saturating_sub(12)..],
                    r.status()
                );
                return;
            }
            Err(e) => {
                warn!(
                    "Orderbook REST {}: {}",
                    &token_id[token_id.len().saturating_sub(12)..],
                    e
                );
                return;
            }
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Orderbook REST parse error: {}", e);
                return;
            }
        };
        let mut entry = self.books.entry(token_id.to_string()).or_default();
        entry.bids.clear();
        entry.asks.clear();
        if let Some(bids) = json.get("bids").and_then(|v| v.as_array()) {
            for level in bids {
                let p = parse_price(
                    level.get("price").and_then(|v| v.as_str()).unwrap_or("0"),
                );
                let s = parse_size(
                    level.get("size").and_then(|v| v.as_str()).unwrap_or("0"),
                );
                if p > 0 && s > 0 {
                    entry.bids.insert(p, s);
                }
            }
        }
        if let Some(asks) = json.get("asks").and_then(|v| v.as_array()) {
            for level in asks {
                let p = parse_price(
                    level.get("price").and_then(|v| v.as_str()).unwrap_or("0"),
                );
                let s = parse_size(
                    level.get("size").and_then(|v| v.as_str()).unwrap_or("0"),
                );
                if p > 0 && s > 0 {
                    entry.asks.insert(p, s);
                }
            }
        }
        entry.last_update_ms = now_ms();
        info!(
            "📦 Orderbook REST: bootstrapped ...{} — {}b/{}a",
            &token_id[token_id.len().saturating_sub(12)..],
            entry.bids.len(),
            entry.asks.len()
        );
        drop(entry); // release DashMap ref before sending
        let _ = self.update_tx.send(token_id.to_string());
    }

    /// Unsubscribe (remove local book; WS channel stays open).
    pub fn unsubscribe(&self, token_id: &str) {
        self.books.remove(token_id);
    }

    /// Best bid for a token, or None if no data.
    pub fn best_bid(&self, token_id: &str) -> Option<f64> {
        self.books.get(token_id)?.best_bid()
    }

    /// Best ask for a token, or None if no data.
    pub fn best_ask(&self, token_id: &str) -> Option<f64> {
        self.books.get(token_id)?.best_ask()
    }

    /// Whether we have received at least one snapshot for this token.
    pub fn has_data(&self, token_id: &str) -> bool {
        self.books
            .get(token_id)
            .map(|b| b.has_data())
            .unwrap_or(false)
    }

    /// Top N bids sorted descending by price → Vec<(price, size)>
    pub fn top_bids(&self, token_id: &str, n: usize) -> Vec<(f64, f64)> {
        let book = match self.books.get(token_id) {
            Some(b) => b,
            None => return vec![],
        };
        let mut levels: Vec<(u64, u64)> = book
            .bids
            .iter()
            .filter(|(_, &sz)| sz > 0)
            .map(|(&p, &s)| (p, s))
            .collect();
        levels.sort_by(|a, b| b.0.cmp(&a.0));
        levels
            .into_iter()
            .take(n)
            .map(|(p, s)| (p as f64 / 1_000_000.0, s as f64 / 1_000_000.0))
            .collect()
    }

    /// Top N asks sorted ascending by price → Vec<(price, size)>
    pub fn top_asks(&self, token_id: &str, n: usize) -> Vec<(f64, f64)> {
        let book = match self.books.get(token_id) {
            Some(b) => b,
            None => return vec![],
        };
        let mut levels: Vec<(u64, u64)> = book
            .asks
            .iter()
            .filter(|(_, &sz)| sz > 0)
            .map(|(&p, &s)| (p, s))
            .collect();
        levels.sort_by(|a, b| a.0.cmp(&b.0));
        levels
            .into_iter()
            .take(n)
            .map(|(p, s)| (p as f64 / 1_000_000.0, s as f64 / 1_000_000.0))
            .collect()
    }

    /// Unix ms timestamp of the last update for a token. Returns 0 if never updated.
    pub fn last_update_ms(&self, token_id: &str) -> i64 {
        self.books
            .get(token_id)
            .map(|b| b.last_update_ms)
            .unwrap_or(0)
    }

    /// All currently tracked token IDs.
    pub fn tracked_tokens(&self) -> Vec<String> {
        self.books.iter().map(|e| e.key().clone()).collect()
    }

    /// Wait until any of the given tokens receives an update, or timeout.
    ///
    /// Uses the per-token broadcast channel so updates to *other* tokens
    /// (e.g. previous-market snapshots) do NOT wake this call.
    pub async fn wait_for_update(&self, token_ids: &[&str], timeout: Duration) {
        let mut rx = self.update_tx.subscribe();
        let ids: HashSet<String> = token_ids.iter().map(|s| s.to_string()).collect();

        tokio::time::timeout(timeout, async move {
            loop {
                match rx.recv().await {
                    Ok(token_id) if ids.contains(&token_id) => break,
                    Ok(_) => continue,                  // different token — ignore
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await
        .ok();
    }
}

/// Spawn the background WebSocket task and return a watcher handle.
pub fn spawn(subscribed: Vec<String>) -> OrderbookWatcher {
    let books: Books = Arc::new(DashMap::new());
    let (update_tx, _) = broadcast::channel::<String>(512);
    let update_tx = Arc::new(update_tx);
    let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    let (reconn_tx, mut reconn_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let books2 = books.clone();
    let update_tx2 = update_tx.clone();
    let initial_subs = subscribed.clone();

    tokio::spawn(async move {
        let mut reconnect_delay = Duration::from_secs(2);
        let mut manual_reconnect = false;
        let mut pending_subs: HashSet<String> = initial_subs.into_iter().collect();

        loop {
            // Wait until we have at least one subscription before connecting.
            while pending_subs.is_empty() {
                match sub_rx.recv().await {
                    Some(ids) => pending_subs.extend(ids),
                    None => return,
                }
            }

            info!("🔌 Orderbook WS: connecting...");
            match connect_async(WS_URL).await {
                Err(e) => {
                    error!("Orderbook WS connect error: {}", e);
                }
                Ok((mut ws, _)) => {
                    info!("✅ Orderbook WS: connected");
                    reconnect_delay = Duration::from_secs(2);

                    // Subscribe to all pending tokens
                    if !pending_subs.is_empty() {
                        let sub_list: Vec<&String> = pending_subs.iter().collect();
                        let first = sub_list[0];
                        let msg = serde_json::json!({
                            "assets_ids": sub_list,
                            "type": "market"
                        });
                        info!(
                            "📋 Orderbook WS: subscribing {} tokens (first: ...{})",
                            sub_list.len(),
                            &first[first.len().saturating_sub(12)..]
                        );
                        if let Err(e) = ws.send(Message::Text(msg.to_string())).await {
                            error!("Orderbook WS subscribe error: {}", e);
                        }
                    }

                    let mut ping_interval = tokio::time::interval(PING_INTERVAL);

                    loop {
                        tokio::select! {
                            // Manual reconnect request — break immediately, no backoff.
                            // Drain any subscribe messages queued in sub_rx first so
                            // their tokens land in pending_subs before we reconnect.
                            // This prevents a race where subscribe() + reconnect() are
                            // called back-to-back and the new tokens miss the initial
                            // subscription message (which is the only one the server
                            // sends full snapshots for).
                            Some(()) = reconn_rx.recv() => {
                                while let Ok(ids) = sub_rx.try_recv() {
                                    for id in ids {
                                        books2.entry(id.clone()).or_default();
                                        pending_subs.insert(id);
                                    }
                                }
                                info!(
                                    "🔄 Orderbook WS: reconnect — cycling ({} tokens)",
                                    pending_subs.len()
                                );
                                manual_reconnect = true;
                                break;
                            }

                            // New subscription request — only send for truly new tokens
                            Some(ids) = sub_rx.recv() => {
                                let new_ids: Vec<String> = ids.into_iter()
                                    .filter(|id| !pending_subs.contains(id))
                                    .collect();
                                if new_ids.is_empty() {
                                    continue;
                                }
                                let msg = serde_json::json!({
                                    "assets_ids": new_ids,
                                    "type": "market"
                                });
                                if let Err(e) = ws.send(Message::Text(msg.to_string())).await {
                                    warn!("Orderbook WS: subscribe send error: {}", e);
                                    break;
                                }
                                for id in &new_ids {
                                    books2.entry(id.clone()).or_default();
                                }
                                pending_subs.extend(new_ids.iter().cloned());
                                info!("➕ Orderbook WS: subscribed {} new tokens", new_ids.len());
                            }

                            // Ping
                            _ = ping_interval.tick() => {
                                if let Err(e) = ws.send(Message::Text("ping".into())).await {
                                    warn!("Orderbook WS: ping error: {}", e);
                                    break;
                                }
                            }

                            // Incoming message
                            msg = ws.next() => {
                                match msg {
                                    None | Some(Err(_)) => break,
                                    Some(Ok(Message::Text(text))) => {
                                        handle_message(&text, &books2, &update_tx2);
                                    }
                                    Some(Ok(Message::Ping(data))) => {
                                        let _ = ws.send(Message::Pong(data)).await;
                                    }
                                    Some(Ok(Message::Close(_))) => break,
                                    _ => {}
                                }
                            }
                        }
                    }

                    info!("⚡ Orderbook WS: disconnected");
                }
            }

            if manual_reconnect {
                // Drain any extra reconnect signals that queued up
                while reconn_rx.try_recv().is_ok() {}
                manual_reconnect = false;
                reconnect_delay = Duration::from_secs(2); // reset exponential backoff
                // No sleep — reconnect immediately
            } else {
                sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
            }
        }
    });

    OrderbookWatcher {
        books,
        update_tx,
        subscribe_tx: sub_tx,
        reconnect_tx: reconn_tx,
    }
}

fn handle_message(
    text: &str,
    books: &Books,
    update_tx: &broadcast::Sender<String>,
) {
    if text.trim() == "pong" || text.trim() == "ping" {
        return;
    }

    debug!("Orderbook WS raw: {}…", &text[..text.len().min(300)]);

    let events: Vec<serde_json::Value> = if text.trim_start().starts_with('[') {
        serde_json::from_str(text).unwrap_or_default()
    } else {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .into_iter()
            .collect()
    };

    for event in events {
        let event_type = event
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // asset_id may arrive as a string or a large integer
        let asset_id = match event.get("asset_id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(other) => {
                debug!("Orderbook WS: unexpected asset_id type: {:?}", other);
                continue;
            }
            None => {
                debug!(
                    "Orderbook WS: event has no asset_id (event_type={})",
                    event_type
                );
                continue;
            }
        };

        if !books.contains_key(&asset_id) {
            debug!(
                "Orderbook WS: received event for untracked asset {} (event_type={})",
                &asset_id[..asset_id.len().min(20)],
                event_type
            );
            continue;
        }

        let updated = match event_type {
            "book" => {
                let mut entry = books.entry(asset_id.clone()).or_default();
                // Save previous state to detect actual changes
                let prev_bid_count = entry.bids.len();
                let prev_ask_count = entry.asks.len();
                let prev_best_bid  = entry.best_bid();
                let prev_best_ask  = entry.best_ask();

                entry.bids.clear();
                entry.asks.clear();
                if let Some(bids) = event.get("bids").and_then(|v| v.as_array()) {
                    for level in bids {
                        let p = parse_price(
                            level.get("price").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        let s = parse_size(
                            level.get("size").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        if p > 0 && s > 0 {
                            entry.bids.insert(p, s);
                        }
                    }
                }
                if let Some(asks) = event.get("asks").and_then(|v| v.as_array()) {
                    for level in asks {
                        let p = parse_price(
                            level.get("price").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        let s = parse_size(
                            level.get("size").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        if p > 0 && s > 0 {
                            entry.asks.insert(p, s);
                        }
                    }
                }
                let new_bid_count = entry.bids.len();
                let new_ask_count = entry.asks.len();
                let changed = new_bid_count != prev_bid_count
                    || new_ask_count != prev_ask_count
                    || entry.best_bid() != prev_best_bid
                    || entry.best_ask() != prev_best_ask;
                // Snapshots are too frequent for INFO — always debug.
                // The TUI and strategy read the DashMap directly.
                {
                    let _ = changed; // suppress unused warning
                    debug!(
                        "Orderbook WS: snapshot ...{} — {}b/{}a",
                        &asset_id[asset_id.len().saturating_sub(12)..],
                        new_bid_count,
                        new_ask_count,
                    );
                }
                true
            }
            "price_change" => {
                let mut entry = books.entry(asset_id.clone()).or_default();
                if let Some(changes) = event.get("changes").and_then(|v| v.as_array()) {
                    for change in changes {
                        let side =
                            change.get("side").and_then(|v| v.as_str()).unwrap_or("");
                        let p = parse_price(
                            change.get("price").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        let s = parse_size(
                            change.get("size").and_then(|v| v.as_str()).unwrap_or("0"),
                        );
                        if p == 0 {
                            continue;
                        }
                        let map = if side.eq_ignore_ascii_case("SELL") {
                            &mut entry.asks
                        } else {
                            &mut entry.bids
                        };
                        if s == 0 {
                            map.remove(&p);
                        } else {
                            map.insert(p, s);
                        }
                    }
                }
                true
            }
            _ => false,
        };

        if updated {
            // Stamp the update time so callers can detect stale data during lag.
            if let Some(mut entry) = books.get_mut(&asset_id) {
                entry.last_update_ms = now_ms();
            }
            // Send the specific token_id that changed. Watchers for OTHER tokens
            // will simply ignore this message and keep waiting.
            let _ = update_tx.send(asset_id);
        }
    }
}
