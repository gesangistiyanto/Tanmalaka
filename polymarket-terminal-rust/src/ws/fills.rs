//! Real-time order fill detection via Polymarket RTDS WebSocket.
//!
//! Connects to wss://ws-live-data.polymarket.com and subscribes to the
//! activity/trades topic. Emits a FillEvent whenever one of the watched
//! tokens is filled for our wallet.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const WS_URL: &str = "wss://ws-live-data.polymarket.com";
const PING_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const CHANNEL_CAP: usize = 256;

// ── Fill event ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FillEvent {
    pub token_id: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub condition_id: String,
}

// ── RTDS message types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ActivityPayload {
    #[serde(alias = "proxy_wallet")]
    #[serde(default)]
    proxy_wallet: String,

    #[serde(alias = "proxyWallet")]
    #[serde(default)]
    proxy_wallet2: String,

    #[serde(default)]
    asset: String,

    #[serde(default)]
    side: String,

    #[serde(default)]
    size: String,

    #[serde(default)]
    price: String,

    #[serde(default)]
    #[serde(alias = "condition_id")]
    condition_id: String,
}

impl ActivityPayload {
    fn wallet(&self) -> &str {
        if !self.proxy_wallet.is_empty() {
            &self.proxy_wallet
        } else {
            &self.proxy_wallet2
        }
    }
}

// ── Watcher ───────────────────────────────────────────────────────────────────

type WatchedTokens = Arc<RwLock<HashSet<String>>>;

/// Handle for the fill watcher.
#[derive(Clone)]
pub struct FillWatcher {
    watched: WatchedTokens,
    wallet: String,
    tx: broadcast::Sender<FillEvent>,
}

impl FillWatcher {
    /// Register a token ID to watch for fills.
    pub async fn watch(&self, token_id: &str) {
        self.watched.write().await.insert(token_id.to_string());
    }

    /// Deregister a token ID.
    pub async fn unwatch(&self, token_id: &str) {
        self.watched.write().await.remove(token_id);
    }

    /// Subscribe to fill events. Returns a broadcast receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<FillEvent> {
        self.tx.subscribe()
    }
}

/// Spawn the background RTDS task and return a watcher handle.
pub fn spawn(wallet_address: String) -> FillWatcher {
    let watched: WatchedTokens = Arc::new(RwLock::new(HashSet::new()));
    let (tx, _) = broadcast::channel(CHANNEL_CAP);

    let watched2 = watched.clone();
    let tx2 = tx.clone();
    let wallet = wallet_address.clone();

    tokio::spawn(async move {
        let mut reconnect_delay = Duration::from_secs(2);

        loop {
            info!("Fill WS: connecting to RTDS...");
            match connect_async(WS_URL).await {
                Err(e) => {
                    error!("Fill WS connect error: {}", e);
                }
                Ok((mut ws, _)) => {
                    info!("Fill WS: connected, subscribing to activity...");
                    reconnect_delay = Duration::from_secs(2);

                    let sub = serde_json::json!({
                        "action": "subscribe",
                        "subscriptions": [{ "topic": "activity", "type": "trades" }]
                    });
                    if let Err(e) = ws.send(Message::Text(sub.to_string())).await {
                        error!("Fill WS: subscribe send error: {}", e);
                        continue;
                    }

                    let mut ping_interval = tokio::time::interval(PING_INTERVAL);

                    loop {
                        tokio::select! {
                            _ = ping_interval.tick() => {
                                if let Err(e) = ws.send(Message::Text("ping".into())).await {
                                    warn!("Fill WS: ping error: {}", e);
                                    break;
                                }
                            }
                            msg = ws.next() => {
                                match msg {
                                    None | Some(Err(_)) => break,
                                    Some(Ok(Message::Text(text))) => {
                                        handle_message(&text, &wallet, &watched2, &tx2).await;
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

                    info!("Fill WS: disconnected");
                }
            }

            sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
        }
    });

    FillWatcher {
        watched,
        wallet: wallet_address,
        tx,
    }
}

async fn handle_message(
    text: &str,
    wallet: &str,
    watched: &WatchedTokens,
    tx: &broadcast::Sender<FillEvent>,
) {
    if text.trim() == "pong" || text.trim() == "ping" {
        return;
    }

    let msg: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    if msg.get("type").and_then(|v| v.as_str()) == Some("ping") {
        return;
    }

    if msg.get("topic").and_then(|v| v.as_str()) != Some("activity") {
        return;
    }

    let payload: ActivityPayload = match msg
        .get("payload")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => return,
    };

    // Filter: must be our wallet
    if !payload.wallet().eq_ignore_ascii_case(wallet) {
        return;
    }

    let token_id = payload.asset.clone();
    if token_id.is_empty() {
        return;
    }

    // Filter: must be a watched token
    if !watched.read().await.contains(&token_id) {
        return;
    }

    let size = payload.size.parse::<f64>().unwrap_or(0.0);
    let price = payload.price.parse::<f64>().unwrap_or(0.0);

    if size <= 0.0 {
        return;
    }

    let fill = FillEvent {
        token_id: token_id.clone(),
        side: payload.side.to_uppercase(),
        size,
        price,
        condition_id: payload.condition_id.clone(),
    };

    debug!("Fill WS: {} {} {} shares @ ${:.3}", fill.side, &token_id[..token_id.len().min(16)], fill.size, fill.price);

    let _ = tx.send(fill);
}
