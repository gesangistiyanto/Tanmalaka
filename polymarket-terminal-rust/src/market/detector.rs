//! Market detector — polls the Gamma API for upcoming/current Up-Down markets.
//!
//! Slot timing:
//!   5m  markets: 300-second slots  → slug btc-updown-5m-{timestamp}
//!   15m markets: 900-second slots  → slug btc-updown-15m-{timestamp}
//!
//! The detector polls every `poll_interval` and emits each new market once via
//! the returned channel.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::types::Market;
use crate::config::Config;

// ── Gamma API response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(alias = "condition_id")]
    condition_id: Option<String>,
    #[serde(alias = "conditionId")]
    condition_id2: Option<String>,

    question: Option<String>,
    title: Option<String>,

    // Dates can come as ISO strings or Unix timestamp numbers
    #[serde(alias = "end_date_iso")]
    #[serde(alias = "endDateIso")]
    end_date: Option<serde_json::Value>,
    #[serde(alias = "endDate")]
    end_date2: Option<serde_json::Value>,

    #[serde(alias = "event_start_time")]
    event_start_time: Option<serde_json::Value>,
    #[serde(alias = "eventStartTime")]
    event_start_time2: Option<serde_json::Value>,

    #[serde(alias = "clob_token_ids")]
    clob_token_ids: Option<Value>,
    #[serde(alias = "clobTokenIds")]
    clob_token_ids2: Option<Value>,

    tokens: Option<Vec<Value>>,

    // Tick size can come back as a JSON string "0.01" or a number 0.01
    #[serde(alias = "minimum_tick_size")]
    minimum_tick_size: Option<serde_json::Value>,
    #[serde(alias = "minimumTickSize")]
    minimum_tick_size2: Option<serde_json::Value>,
    #[serde(alias = "orderPriceMinTickSize")]
    order_price_min_tick_size: Option<serde_json::Value>,

    #[serde(alias = "neg_risk")]
    neg_risk: Option<bool>,
    #[serde(alias = "negRisk")]
    neg_risk2: Option<bool>,
}

impl GammaMarket {
    fn condition_id(&self) -> Option<&str> {
        self.condition_id.as_deref().or(self.condition_id2.as_deref())
    }

    fn question(&self) -> String {
        self.question
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("")
            .to_string()
    }

    fn end_date(&self) -> Option<String> {
        // endDate (full ISO datetime) must take priority over endDateIso (date-only "2026-04-03")
        // which parse_dt cannot handle.
        val_to_dt_str(self.end_date2.as_ref())
            .or_else(|| val_to_dt_str(self.end_date.as_ref()))
    }

    fn event_start_time(&self) -> Option<String> {
        // Same priority: full datetime first
        val_to_dt_str(self.event_start_time2.as_ref())
            .or_else(|| val_to_dt_str(self.event_start_time.as_ref()))
    }

    fn token_ids(&self) -> Option<(String, String)> {
        // Token IDs can arrive as strings OR large integers — always coerce to String.
        let val_to_id = |v: &Value| -> Option<String> {
            match v {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        };

        // Try clobTokenIds first (JSON array or string-encoded array)
        let raw = self.clob_token_ids.as_ref().or(self.clob_token_ids2.as_ref());
        if let Some(raw) = raw {
            let ids = match raw {
                Value::Array(arr) => arr.clone(),
                Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
                _ => vec![],
            };
            if ids.len() >= 2 {
                if let (Some(yes), Some(no)) = (val_to_id(&ids[0]), val_to_id(&ids[1])) {
                    return Some((yes, no));
                }
            }
        }

        // Fallback: tokens array (each element has tokenId / token_id field)
        if let Some(tokens) = &self.tokens {
            if tokens.len() >= 2 {
                let get_id = |t: &Value| -> Option<String> {
                    let v = t.get("tokenId").or_else(|| t.get("token_id"))?;
                    val_to_id(v)
                };
                if let (Some(yes), Some(no)) = (get_id(&tokens[0]), get_id(&tokens[1])) {
                    return Some((yes, no));
                }
            }
        }

        None
    }

    fn tick_size(&self) -> f64 {
        let parse = |v: &serde_json::Value| -> Option<f64> {
            match v {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => s.parse().ok(),
                _ => None,
            }
        };
        self.order_price_min_tick_size.as_ref().and_then(parse)
            .or_else(|| self.minimum_tick_size.as_ref().and_then(parse))
            .or_else(|| self.minimum_tick_size2.as_ref().and_then(parse))
            .unwrap_or(0.01)
    }

    fn neg_risk(&self) -> bool {
        self.neg_risk.or(self.neg_risk2).unwrap_or(false)
    }

    fn try_into_market(self, asset: &str, is_current: bool, resolution_ts: i64) -> Option<Market> {
        let condition_id = match self.condition_id() {
            Some(id) => id.to_string(),
            None => {
                warn!("Market parse: no condition_id (cond1={:?} cond2={:?})",
                    self.condition_id, self.condition_id2);
                return None;
            }
        };
        let (yes_token_id, no_token_id) = match self.token_ids() {
            Some(ids) => ids,
            None => {
                warn!("Market parse: no token_ids — clob_token_ids={:?} clob_token_ids2={:?} tokens={:?}",
                    self.clob_token_ids, self.clob_token_ids2, self.tokens);
                return None;
            }
        };

        let end_date_str = match self.end_date() {
            Some(s) => s,
            None => {
                warn!("Market parse: no endDate (end_date={:?} end_date2={:?})",
                    self.end_date, self.end_date2);
                return None;
            }
        };
        let end_time = match parse_dt(&end_date_str) {
            Some(dt) => dt,
            None => {
                warn!("Market parse: cannot parse endDate {:?}", end_date_str);
                return None;
            }
        };
        let event_start_time = self
            .event_start_time()
            .as_deref()
            .and_then(parse_dt)
            .unwrap_or(end_time);

        Some(Market {
            asset: asset.to_string(),
            condition_id,
            question: self.question(),
            end_time,
            event_start_time,
            yes_token_id,
            no_token_id,
            neg_risk: self.neg_risk(),
            tick_size: self.tick_size(),
            resolution_ts,
            is_current,
        })
    }
}

/// Convert a serde_json::Value (string or number) to a date string for parse_dt.
fn val_to_dt_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    // ISO 8601
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // Unix timestamp string
    if let Ok(ts) = s.parse::<i64>() {
        return Utc.timestamp_opt(ts, 0).single();
    }
    None
}

// ── Detector ──────────────────────────────────────────────────────────────────

/// Start the market detector and return a receiver for new markets.
pub fn spawn(config: Config) -> mpsc::Receiver<Market> {
    let (tx, rx) = mpsc::channel(32);

    tokio::spawn(async move {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let mut seen: HashSet<String> = HashSet::new();
        let slot_secs = config.slot_secs();

        // On first start, also check the current slot so we don't miss a market
        // that opened before the bot started.
        let mut first_run = true;

        loop {
            let now_ts = unix_ts();

            // Try the NEXT slot first, then CURRENT if this is the first run.
            let slots_to_check: Vec<u64> = if first_run {
                let cur = (now_ts / slot_secs) * slot_secs;
                let next = cur + slot_secs;
                vec![cur, next]
            } else {
                let cur = (now_ts / slot_secs) * slot_secs;
                let next = cur + slot_secs;
                vec![next, cur]
            };

            for &slot in &slots_to_check {
                for asset in &config.assets {
                    let slug = format!("{}-updown-{}-{}", asset, config.duration, slot);
                    // Always log on first run so user can verify slug format; then debug level
                    if first_run {
                        info!("Market detector: checking slug {}", slug);
                    } else {
                        debug!("Market detector: checking slug {}", slug);
                    }
                    match fetch_market(&http, &config.gamma_host, &slug).await {
                        Ok(Some(gm)) => {
                            let is_current = slot == (now_ts / slot_secs) * slot_secs;
                            if let Some(market) = gm.try_into_market(asset, is_current, slot as i64) {
                                if seen.contains(&market.condition_id) {
                                    debug!("Already seen {}, skipping", slug);
                                    continue;
                                }

                                let secs_left = (market.end_time.timestamp() - now_ts as i64).max(0) as u64;
                                if secs_left <= config.cut_loss_time {
                                    info!("Market {}: only {}s left (< cut_loss {}s) — skipping", slug, secs_left, config.cut_loss_time);
                                    continue;
                                }

                                // Check entry window (only for NEXT slot)
                                let secs_since_open = (now_ts as i64 - market.event_start_time.timestamp()).max(0) as u64;
                                if !is_current && secs_since_open > config.entry_window {
                                    info!("Market {}: entry window passed ({}s > {}s) — skipping", slug, secs_since_open, config.entry_window);
                                    continue;
                                }

                                info!("Market detector: FOUND {} | {} | {}s left | res_ts={}",
                                    slug,
                                    market.question.chars().take(50).collect::<String>(),
                                    secs_left,
                                    market.resolution_ts,
                                );

                                seen.insert(market.condition_id.clone());
                                if tx.send(market).await.is_err() {
                                    return; // receiver dropped — exit
                                }
                            } else {
                                warn!("Market {}: API returned data but failed to parse (missing tokens/condition_id?)", slug);
                            }
                        }
                        Ok(None) => {
                            if first_run {
                                info!("Market detector: {} — not found yet", slug);
                            } else {
                                debug!("Market detector: {} — not found", slug);
                            }
                        }
                        Err(e) => {
                            warn!("Market detector: {} fetch error: {}", slug, e);
                        }
                    }
                }
            }

            first_run = false;
            sleep(Duration::from_millis(config.poll_interval_ms)).await;
        }
    });

    rx
}

async fn fetch_market(
    http: &Client,
    gamma_host: &str,
    slug: &str,
) -> Result<Option<GammaMarket>> {
    // Gamma API: /markets/slug/{slug} returns a single object (not array)
    let url = format!("{}/markets/slug/{}", gamma_host, slug);
    let resp = http.get(&url).send().await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Gamma API {} returned {}", url, resp.status()));
    }

    let text = resp.text().await?;
    if text.trim() == "null" || text.trim() == "[]" || text.trim().is_empty() {
        return Ok(None);
    }

    // Log truncated response for debugging
    debug!("Gamma API response ({}): {}…", slug, &text[..text.len().min(300)]);

    // API may return a single object or an array
    let market: GammaMarket = if text.trim_start().starts_with('[') {
        let arr: Vec<GammaMarket> = serde_json::from_str(&text)?;
        match arr.into_iter().next() {
            Some(m) => m,
            None => return Ok(None),
        }
    } else {
        serde_json::from_str(&text)?
    };

    Ok(Some(market))
}

fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
