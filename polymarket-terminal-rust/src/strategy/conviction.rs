//! Conviction strategy v2 — three signals, liquidity-first priority.
//!
//! 1. LIQUIDITY DEPTH (checked first at window open)
//!    - Sum YES bid volume vs NO ask volume
//!    - If one side dominates AND price < 70c → enter that side
//!
//! 2. PANIC BUY (during 2-min window)
//!    - Previous tick < 40c required
//!    - Bids jump > +5 ticks → buy OPPOSITE side
//!
//! 3. FOLLOW MOMENTUM (during 2-min window)
//!    - Price > 50c AND +5 tick jump AND still < 70c
//!    - Buy THIS side (ride the momentum)
//!
//! Only ONE entry per market. Trade stays "Open" until resolution.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

use crate::clob::ClobClient;
use crate::config::Config;
use crate::market::types::Market;
use crate::onchain::ctf::CtfClient;
use crate::tui::{ConvictionTrade, OnchainEvent, TuiHandle};
use crate::ws::orderbook::OrderbookWatcher;
use crate::ws::price::PriceWatcher;

/// How far before market end to start monitoring (builds price history).
const WATCH_WINDOW_MS: i64 = 2 * 60 * 1_000;
/// Max entry price for all signals (85c).
const MAX_ENTRY_PRICE: f64 = 0.85;
/// Max entry price alias for LIQUIDITY signal (same cap).
const MAX_ENTRY_PRICE_LIQ: f64 = 0.85;
/// Min price for momentum signal (50c) — skip low-confidence moves.
const MOMENTUM_MIN_PRICE: f64 = 0.50;
/// Panic detection: previous tick must be below this.
const PANIC_PREV_MAX: f64 = 0.40;
/// Tick jump threshold for both panic and momentum.
const TICK_JUMP: f64 = 5.0;
/// Time window for net-move momentum detection (30 seconds).
const MOMENTUM_WINDOW_MS: i64 = 30_000;
/// Minimum liquidity ratio to trigger depth strategy.
const LIQUIDITY_RATIO_MIN: f64 = 2.0;
/// Number of price snapshots to keep per side.
const HISTORY_LEN: usize = 20;
/// If neither YES nor NO orderbook has been updated within this window,
/// the data is considered stale (internet lag). Signals are suppressed and
/// price history is cleared to avoid fake spikes on reconnect.
const STALE_THRESHOLD_MS: i64 = 10_000;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Snap {
    price: f64,
    ts_ms: i64,
}

struct ConvPos {
    side: String,
    entry_price: f64,
    shares: f64,
    trade_id: usize,
}


// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_once(
    market: &Market,
    config: &Config,
    ob: &OrderbookWatcher,
    prices: &PriceWatcher,
    tui: Option<&TuiHandle>,
    clob: Option<&ClobClient>,
    ctf: Option<Arc<CtfClient>>,
) {
    let tag = format!("[{}]", market.asset.to_uppercase());
    let ts = market.tick_size;
    let yes_id = &market.yes_token_id;
    let no_id = &market.no_token_id;
    let symbol = market.chainlink_symbol();

    info!(
        "🎯 Conviction{}: monitoring \"{}\"",
        tag,
        market.question.chars().take(50).collect::<String>()
    );

    // ── Bootstrap REST for pre-open markets ───────────────────────────────────
    ob.bootstrap_rest(&config.clob_host, yes_id).await;
    ob.bootstrap_rest(&config.clob_host, no_id).await;

    let open_ms = market.end_time.timestamp_millis() - (config.slot_secs() as i64 * 1_000);
    let wait_until = open_ms + 5_000;
    while now_ms() < wait_until && !ob.has_data(yes_id) && !ob.has_data(no_id) {
        let chunk = ((wait_until - now_ms()) as u64).min(15_000);
        sleep(Duration::from_millis(chunk)).await;
        if !ob.has_data(yes_id) { ob.bootstrap_rest(&config.clob_host, yes_id).await; }
        if !ob.has_data(no_id)  { ob.bootstrap_rest(&config.clob_host, no_id).await; }
    }


    // ── Wait until 1 minute into slot (PRICE MOVE + MOMENTUM start here) ──────
    // PANIC and LIQUIDITY wait for the 2-min window; these two start earlier.
    let watch_start_ms = market.end_time.timestamp_millis() - WATCH_WINDOW_MS;
    let monitor_start_ms = (open_ms + 60_000).min(watch_start_ms);
    let wait_ms = (monitor_start_ms - now_ms()).max(0) as u64;
    if wait_ms > 0 {
        info!(
            "⏳ Conviction{}: waiting {}s until monitoring starts",
            tag, wait_ms / 1_000
        );
        sleep(Duration::from_millis(wait_ms)).await;
    }
    // ── Lock in reference price ───────────────────────────────────────────────
    // Wait up to 10s for the price WS to deliver data (multi-ticker race).
    // Priority 1: history at resolution_ts (exact slot-start price).
    // Priority 2: current price (fallback when history not yet received).
    let locked_ref_price: Option<f64> = {
        let deadline = now_ms() + 10_000;
        loop {
            let p = prices
                .reference_price(&symbol, market.resolution_ts * 1_000)
                .or_else(|| prices.current_price(&symbol));
            if p.is_some() || now_ms() >= deadline {
                break p;
            }
            sleep(Duration::from_millis(500)).await;
        }
    };

    info!(
        "🟢 Conviction{}: monitoring started — ref_price={}",
        tag,
        locked_ref_price.map(|p| format!("{:.4}", p)).unwrap_or("(unavailable — PRICE_MOVE disabled)".into())
    );
    if let (Some(t), Some(r)) = (tui, locked_ref_price) {
        t.set_locked_ref_price(&market.asset, r).await;
    }

    if let Some(t) = tui {
        t.push_conviction_market(
            &market.asset,
            monitor_start_ms,
            market.end_time.timestamp_millis(),
        ).await;
    }

    // Price histories for panic/momentum detection
    let mut yes_hist: VecDeque<Snap> = VecDeque::new();
    let mut no_hist: VecDeque<Snap> = VecDeque::new();

    let mut position: Option<ConvPos> = None;
    let mut strategy_used: Option<&'static str> = None;
    let mut last_balance_update_ms: i64 = 0;
    let mut last_unrealised: f64 = 0.0;
    let mut liquidity_checked = false; // run LIQUIDITY once at 2-min window open
    const BALANCE_UPDATE_INTERVAL_MS: i64 = 30_000;

    // ── Main monitoring loop ─────────────────────────────────────────────────
    loop {
        let now = now_ms();
        let ms_left = market.end_time.timestamp_millis() - now;
        let in_two_min_window = ms_left <= WATCH_WINDOW_MS;

        // ── Cut-loss / market close ───────────────────────────────────────────
        if ms_left <= config.cut_loss_time as i64 * 1_000 {
            if position.is_some() {
                info!("⏰ Conviction{}: cut-loss reached — stopping watch, resolving after close", tag);
            }
            break;
        }

        // ── Staleness guard ───────────────────────────────────────────────────
        // If neither token has received a WS update in the last STALE_THRESHOLD_MS,
        // the orderbook data is stale (internet lag). Clear price history so that
        // when the connection recovers and prices "jump" to catch up, we don't
        // mistake a lag-catchup burst for a real panic/momentum spike.
        let yes_age = now - ob.last_update_ms(yes_id);
        let no_age  = now - ob.last_update_ms(no_id);
        let data_stale = yes_age > STALE_THRESHOLD_MS && no_age > STALE_THRESHOLD_MS;
        if data_stale {
            if !yes_hist.is_empty() || !no_hist.is_empty() {
                warn!(
                    "⚠️ Conviction{}: orderbook stale (yes={}ms no={}ms) — clearing history, suppressing signals",
                    tag, yes_age, no_age
                );
                yes_hist.clear();
                no_hist.clear();
            }
            ob.wait_for_update(
                &[yes_id.as_str(), no_id.as_str()],
                Duration::from_millis(config.poll_ms),
            ).await;
            continue;
        }

        // ── Sample orderbook prices ───────────────────────────────────────────
        let yes_ask = ob.best_ask(yes_id);
        let no_bid = ob.best_bid(no_id);

        if let Some(p) = yes_ask {
            yes_hist.push_back(Snap { price: p, ts_ms: now });
            if yes_hist.len() > HISTORY_LEN { yes_hist.pop_front(); }
        }
        if let Some(p) = no_bid {
            no_hist.push_back(Snap { price: p, ts_ms: now });
            if no_hist.len() > HISTORY_LEN { no_hist.pop_front(); }
        }

        // ── Signal detection (flat only) ───────────────────────────────────────
        if position.is_none() {
            let can_enter = ms_left > config.cut_loss_time as i64 * 1_000;

            // ── LIQUIDITY DEPTH (once, at 2-min window open) ──────────────────
            if can_enter && strategy_used.is_none() && in_two_min_window && !liquidity_checked {
                liquidity_checked = true;
                let (yes_liquidity, no_liquidity) = compute_liquidity(ob, yes_id, no_id);
                let liq_ratio    = if no_liquidity  > 0.0 { yes_liquidity / no_liquidity  } else { f64::INFINITY };
                let no_liq_ratio = if yes_liquidity > 0.0 { no_liquidity  / yes_liquidity } else { f64::INFINITY };
                let yes_ask_liq = ob.best_ask(yes_id).unwrap_or(1.0);
                let no_bid_liq  = ob.best_bid(no_id).unwrap_or(0.0);
                let no_ask_liq  = (no_bid_liq + ts).min(0.99);

                if liq_ratio >= LIQUIDITY_RATIO_MIN && yes_ask_liq < MAX_ENTRY_PRICE_LIQ {
                    let shares = shares_for_usd(config.conviction_trade_usd,yes_ask_liq);
                    info!("📊 Conviction{}: LIQUIDITY UP dominant (ratio={:.2}, ask={:.4}) → buying UP",
                        tag, liq_ratio, yes_ask_liq);
                    if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                        id: 0, timestamp_ms: now, side: "UP".to_string(),
                        signal: format!("LIQUIDITY UP {:.1}x", liq_ratio),
                        entry_price: yes_ask_liq, shares,
                        exit_price: None, pnl: None, status: "Open".to_string(),
                    }).await {
                        if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("UP".to_string()), Some(yes_ask_liq)).await; }
                        position = Some(ConvPos { side: "UP".to_string(), entry_price: yes_ask_liq, shares, trade_id: id });
                        strategy_used = Some("liquidity");
                    }
                } else if no_liq_ratio >= LIQUIDITY_RATIO_MIN && no_ask_liq < MAX_ENTRY_PRICE_LIQ {
                    let shares = shares_for_usd(config.conviction_trade_usd,no_ask_liq);
                    info!("📊 Conviction{}: LIQUIDITY DOWN dominant (ratio={:.2}, ask≈{:.4}) → buying DOWN",
                        tag, no_liq_ratio, no_ask_liq);
                    if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                        id: 0, timestamp_ms: now, side: "DOWN".to_string(),
                        signal: format!("LIQUIDITY DOWN {:.1}x", no_liq_ratio),
                        entry_price: no_ask_liq, shares,
                        exit_price: None, pnl: None, status: "Open".to_string(),
                    }).await {
                        if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("DOWN".to_string()), Some(no_ask_liq)).await; }
                        position = Some(ConvPos { side: "DOWN".to_string(), entry_price: no_ask_liq, shares, trade_id: id });
                        strategy_used = Some("liquidity");
                    }
                } else {
                    info!("📊 Conviction{}: LIQUIDITY not triggered (UP={:.0} DOWN={:.0} ratio={:.2}/{:.2})",
                        tag, yes_liquidity, no_liquidity, liq_ratio, no_liq_ratio);
                }
            }

            // ── PRICE MOVE: Chainlink current vs locked reference ─────────────────
            if can_enter && strategy_used.is_none() && position.is_none() {
                let cur_p = prices.current_price(&symbol);
                if let (Some(ref_p), Some(cur_p)) = (locked_ref_price, cur_p) {
                    let pct = (cur_p - ref_p) / ref_p * 100.0;
                    if pct >= 0.035 {
                        if let Some(yes_ask_val) = yes_ask.filter(|&p| p < MAX_ENTRY_PRICE) {
                            let shares = shares_for_usd(config.conviction_trade_usd,yes_ask_val);
                            info!("💹 Conviction{}: PRICE MOVE UP +{:.4}% (ref={:.4} cur={:.4}) → entering UP @${:.4}",
                                tag, pct, ref_p, cur_p, yes_ask_val);
                            if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                                id: 0, timestamp_ms: now, side: "UP".to_string(),
                                signal: format!("PRICE +{:.3}%", pct),
                                entry_price: yes_ask_val, shares,
                                exit_price: None, pnl: None, status: "Open".to_string(),
                            }).await {
                                if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("UP".to_string()), Some(yes_ask_val)).await; }
                                position = Some(ConvPos { side: "UP".to_string(), entry_price: yes_ask_val, shares, trade_id: id });
                                strategy_used = Some("price_move");
                            }
                        }
                    } else if pct <= -0.030 {
                        if let Some(no_ask_est) = no_bid.map(|b| (b + ts).min(0.99)).filter(|&p| p < MAX_ENTRY_PRICE) {
                            let shares = shares_for_usd(config.conviction_trade_usd,no_ask_est);
                            info!("💹 Conviction{}: PRICE MOVE DOWN {:.4}% (ref={:.4} cur={:.4}) → entering DOWN @${:.4}",
                                tag, pct, ref_p, cur_p, no_ask_est);
                            if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                                id: 0, timestamp_ms: now, side: "DOWN".to_string(),
                                signal: format!("PRICE {:.3}%", pct),
                                entry_price: no_ask_est, shares,
                                exit_price: None, pnl: None, status: "Open".to_string(),
                            }).await {
                                if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("DOWN".to_string()), Some(no_ask_est)).await; }
                                position = Some(ConvPos { side: "DOWN".to_string(), entry_price: no_ask_est, shares, trade_id: id });
                                strategy_used = Some("price_move");
                            }
                        }
                    }
                }
            }

            // ── MOMENTUM + LIQUIDITY: tick move confirmed by orderbook depth ─────────
            // Only enter if bid depth also favours the same direction.
            // Momentum alone can be noise; liquidity backing makes it conviction.
            if can_enter && strategy_used.is_none() && position.is_none() {
                let (yes_liq, no_liq) = compute_liquidity(ob, yes_id, no_id);

                let yes_net = net_tick_move(&yes_hist, ts, TICK_JUMP, MOMENTUM_WINDOW_MS);
                if yes_net {
                    let curr_p   = yes_hist.back().map(|s| s.price).unwrap_or(0.0);
                    let oldest_p = yes_window_oldest(&yes_hist, MOMENTUM_WINDOW_MS).map(|s| s.price).unwrap_or(curr_p);
                    let net_ticks = (curr_p - oldest_p) / ts;
                    let liq_ok = yes_liq > no_liq;
                    if oldest_p >= MOMENTUM_MIN_PRICE && curr_p < MAX_ENTRY_PRICE {
                        if liq_ok {
                            let shares = shares_for_usd(config.conviction_trade_usd,curr_p);
                            info!("📈 Conviction{}: MOMENTUM UP +{:.0}t + liquidity UP (${:.0}>${:.0}) → entering UP @${:.4}",
                                tag, net_ticks, yes_liq, no_liq, curr_p);
                            if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                                id: 0, timestamp_ms: now, side: "UP".to_string(),
                                signal: format!("MOM+LIQ UP +{:.0}t", net_ticks),
                                entry_price: curr_p, shares,
                                exit_price: None, pnl: None, status: "Open".to_string(),
                            }).await {
                                if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("UP".to_string()), Some(curr_p)).await; }
                                position = Some(ConvPos { side: "UP".to_string(), entry_price: curr_p, shares, trade_id: id });
                                strategy_used = Some("momentum");
                            }
                        } else {
                            info!("📈 Conviction{}: MOMENTUM UP +{:.0}t skipped — liquidity opposes (UP=${:.0} DOWN=${:.0})",
                                tag, net_ticks, yes_liq, no_liq);
                        }
                    }
                }

                if position.is_none() {
                    let no_net = net_tick_move(&no_hist, ts, TICK_JUMP, MOMENTUM_WINDOW_MS);
                    if no_net {
                        let curr_p   = no_hist.back().map(|s| s.price).unwrap_or(0.0);
                        let oldest_p = yes_window_oldest(&no_hist, MOMENTUM_WINDOW_MS).map(|s| s.price).unwrap_or(curr_p);
                        let net_ticks = (curr_p - oldest_p) / ts;
                        let no_ask_est = (curr_p + ts).min(0.99);
                        let liq_ok = no_liq > yes_liq;
                        if oldest_p >= MOMENTUM_MIN_PRICE && no_ask_est < MAX_ENTRY_PRICE {
                            if liq_ok {
                                let shares = shares_for_usd(config.conviction_trade_usd,no_ask_est);
                                info!("📈 Conviction{}: MOMENTUM DOWN +{:.0}t + liquidity DOWN (${:.0}>${:.0}) → entering DOWN @${:.4}",
                                    tag, net_ticks, no_liq, yes_liq, no_ask_est);
                                if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                                    id: 0, timestamp_ms: now, side: "DOWN".to_string(),
                                    signal: format!("MOM+LIQ DOWN +{:.0}t", net_ticks),
                                    entry_price: no_ask_est, shares,
                                    exit_price: None, pnl: None, status: "Open".to_string(),
                                }).await {
                                    if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some("DOWN".to_string()), Some(no_ask_est)).await; }
                                    position = Some(ConvPos { side: "DOWN".to_string(), entry_price: no_ask_est, shares, trade_id: id });
                                    strategy_used = Some("momentum");
                                }
                            } else {
                                info!("📈 Conviction{}: MOMENTUM DOWN +{:.0}t skipped — liquidity opposes (DOWN=${:.0} UP=${:.0})",
                                    tag, net_ticks, no_liq, yes_liq);
                            }
                        }
                    }
                }
            }

            // ── PANIC: price spiked >5 ticks ─────────────────────────────────────
            // < 50c → fade (buy opposite): cheap side spikes are often overreactions
            // ≥ 50c → follow (buy same):   expensive side spikes signal real conviction
            if can_enter && in_two_min_window {
                // UP panic: YES price spiked
                let up_panic: Option<(f64, f64, &str)> = {
                    if let (Some(prev), Some(curr)) = (yes_hist.len().checked_sub(2), yes_hist.len().checked_sub(1)) {
                        let prev_p = yes_hist[prev].price;
                        let curr_p = yes_hist[curr].price;
                        let tick_delta = (curr_p - prev_p) / ts;
                        if prev_p < PANIC_PREV_MAX && tick_delta >= TICK_JUMP {
                            if curr_p < 0.50 {
                                // cheap → fade → buy DOWN
                                let no_ask_est = no_bid.map(|b| (b + ts).min(0.99)).unwrap_or(1.0 - curr_p);
                                if no_ask_est < MAX_ENTRY_PRICE { Some((no_ask_est, tick_delta, "DOWN")) } else { None }
                            } else {
                                // expensive → follow → buy UP
                                if let Some(ask) = yes_ask.filter(|&p| p < MAX_ENTRY_PRICE) {
                                    Some((ask, tick_delta, "UP"))
                                } else { None }
                            }
                        } else { None }
                    } else { None }
                };

                // DOWN panic: NO price spiked
                let down_panic: Option<(f64, f64, &str)> = {
                    if let (Some(prev), Some(curr)) = (no_hist.len().checked_sub(2), no_hist.len().checked_sub(1)) {
                        let prev_p = no_hist[prev].price;
                        let curr_p = no_hist[curr].price;
                        let tick_delta = (curr_p - prev_p) / ts;
                        if prev_p < PANIC_PREV_MAX && tick_delta >= TICK_JUMP {
                            if curr_p < 0.50 {
                                // cheap → fade → buy UP
                                if let Some(ask) = yes_ask.filter(|&p| p < MAX_ENTRY_PRICE) {
                                    Some((ask, tick_delta, "UP"))
                                } else { None }
                            } else {
                                // expensive → follow → buy DOWN
                                let no_ask_est = (curr_p + ts).min(0.99);
                                if no_ask_est < MAX_ENTRY_PRICE { Some((no_ask_est, tick_delta, "DOWN")) } else { None }
                            }
                        } else { None }
                    } else { None }
                };

                // Mode A: no position yet — standard first entry
                if strategy_used.is_none() && position.is_none() {
                    if let Some((price, tick_delta, side)) = up_panic {
                        let shares = shares_for_usd(config.conviction_trade_usd,price);
                        info!("🚨 Conviction{}: PANIC UP +{:.0}t → buying {} @${:.4}", tag, tick_delta, side, price);
                        if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                            id: 0, timestamp_ms: now, side: side.to_string(),
                            signal: format!("PANIC UP +{:.0}t", tick_delta),
                            entry_price: price, shares,
                            exit_price: None, pnl: None, status: "Open".to_string(),
                        }).await {
                            if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some(side.to_string()), Some(price)).await; }
                            position = Some(ConvPos { side: side.to_string(), entry_price: price, shares, trade_id: id });
                            strategy_used = Some("panic");
                        }
                    } else if let Some((price, tick_delta, side)) = down_panic {
                        let shares = shares_for_usd(config.conviction_trade_usd,price);
                        info!("🚨 Conviction{}: PANIC DOWN +{:.0}t → buying {} @${:.4}", tag, tick_delta, side, price);
                        if let Some(id) = push_trade(tui, clob, market, ConvictionTrade {
                            id: 0, timestamp_ms: now, side: side.to_string(),
                            signal: format!("PANIC DOWN +{:.0}t", tick_delta),
                            entry_price: price, shares,
                            exit_price: None, pnl: None, status: "Open".to_string(),
                        }).await {
                            if let Some(t) = tui { t.set_conviction_position_for(&market.asset, Some(side.to_string()), Some(price)).await; }
                            position = Some(ConvPos { side: side.to_string(), entry_price: price, shares, trade_id: id });
                            strategy_used = Some("panic");
                        }
                    }
                }

            }

        }

        // ── Unrealised P&L balance update every 30s ───────────────────────────
        if now - last_balance_update_ms >= BALANCE_UPDATE_INTERVAL_MS {
            if let Some(ref pos) = position {
                let cur_val = side_value(ob, yes_id, no_id, &pos.side, ts);
                let new_unrealised = (cur_val - pos.entry_price) * pos.shares;
                if let Some(t) = tui {
                    t.update_sim_balance_unrealised(last_unrealised, new_unrealised).await;
                }
                last_unrealised = new_unrealised;
            }
            last_balance_update_ms = now;
        }

        ob.wait_for_update(
            &[yes_id.as_str(), no_id.as_str()],
            Duration::from_millis(config.poll_ms),
        )
        .await;
    }

    // ── Post-market: mark Resolving, then poll in background ─────────────────
    // Never flag WIN/LOSS immediately — resolution can take minutes on-chain.
    if let Some(pos) = position {
        info!("📊 Conviction{}: market ended — spawning background resolver", tag);

        if let Some(t) = tui {
            t.set_conviction_position_for(&market.asset, None, None).await;
            t.update_sim_balance_unrealised(last_unrealised, 0.0).await;
            t.set_trade_resolving(pos.trade_id).await;
        }

        let ob2     = ob.clone();
        let prices2 = prices.clone();
        let market2 = market.clone();
        let tui2    = tui.cloned();
        let ctf2    = ctf.clone();
        let sym2    = symbol.clone();
        tokio::spawn(async move {
            resolve_in_background(pos, market2, ob2, prices2, sym2, tui2, ctf2).await;
        });
    }

    info!("📊 Conviction{}: session complete", tag);
}

// ── Momentum helpers ──────────────────────────────────────────────────────────

/// Returns true if price rose by >= min_ticks (net) within the last `window_ms` ms.
/// Uses the oldest snapshot within the window as the baseline.
fn net_tick_move(hist: &VecDeque<Snap>, ts: f64, min_ticks: f64, window_ms: i64) -> bool {
    if hist.len() < 2 { return false; }
    let newest = hist.back().unwrap();
    let cutoff = newest.ts_ms - window_ms;
    if let Some(oldest) = hist.iter().find(|s| s.ts_ms >= cutoff) {
        if oldest.ts_ms == newest.ts_ms { return false; }
        (newest.price - oldest.price) / ts >= min_ticks
    } else {
        false
    }
}


/// Return the oldest snapshot within `window_ms` of the newest entry.
fn yes_window_oldest(hist: &VecDeque<Snap>, window_ms: i64) -> Option<&Snap> {
    let newest = hist.back()?;
    let cutoff = newest.ts_ms - window_ms;
    hist.iter().find(|s| s.ts_ms >= cutoff)
}

// ── Liquidity computation ─────────────────────────────────────────────────────

/// Compare USD-weighted demand on each side.
///
/// YES bids = people actively bidding to buy YES (bullish pressure).
/// NO  bids = people actively bidding to buy NO  (bearish pressure).
///
/// Weighting by price×size gives USD value willing to be deployed, which
/// normalises for the fact that YES bids cluster near 0.30c and NO bids
/// near 0.70c in a typical market — raw share counts would be misleading.
fn compute_liquidity(ob: &OrderbookWatcher, yes_id: &str, no_id: &str) -> (f64, f64) {
    let yes_bids: Vec<(f64, f64)> = ob.top_bids(yes_id, 10);
    let no_bids:  Vec<(f64, f64)> = ob.top_bids(no_id,  10);

    let yes_liq: f64 = yes_bids.iter().map(|(p, sz)| p * sz).sum();
    let no_liq:  f64 = no_bids.iter().map(|(p, sz)| p * sz).sum();

    (yes_liq, no_liq)
}

// ── Exit / entry price helpers ────────────────────────────────────────────────

fn side_value(ob: &OrderbookWatcher, yes_id: &str, no_id: &str, side: &str, ts: f64) -> f64 {
    if side == "UP" {
        ob.best_ask(yes_id)
            .or_else(|| ob.best_bid(no_id).map(|b| (1.0 - b + ts).min(0.99)))
            .unwrap_or(0.0)
    } else {
        // "DOWN"
        ob.best_bid(no_id)
            .or_else(|| ob.best_ask(yes_id).map(|a| (1.0 - a + ts).min(0.99)))
            .unwrap_or(0.0)
    }
}

/// Place a conviction trade.
///
/// In live mode (`clob` is Some and `dry_run = false`), submits a real limit
/// buy order to the Polymarket CLOB at `trade.entry_price`.  Returns `None`
/// if the order was rejected — the caller should NOT set a position in that case.
///
/// In simulation mode the order is skipped and the trade is recorded in the TUI.
async fn push_trade(
    tui: Option<&TuiHandle>,
    clob: Option<&ClobClient>,
    market: &Market,
    trade: ConvictionTrade,
) -> Option<usize> {
    // Determine which token to buy based on side.
    let token_id = if trade.side == "UP" {
        &market.yes_token_id
    } else {
        &market.no_token_id
    };

    // ── Submit real order in live mode ────────────────────────────────────────
    if let Some(c) = clob {
        if !c.config.dry_run {
            // Try FOK (market fill) first; fall back to GTC limit if no liquidity.
            let fok_result = c.place_market_buy(
                token_id,
                trade.shares,
                trade.entry_price,
                market.tick_size,
                market.neg_risk,
            ).await;

            let order_ok = match fok_result {
                Ok(resp) if resp.success => {
                    info!(
                        "📝 FOK filled: {} {:.2}s @ ${:.4}  order_id={}",
                        trade.side, trade.shares, trade.entry_price,
                        &resp.order_id[..resp.order_id.len().min(16)],
                    );
                    true
                }
                Ok(resp) => {
                    // FOK rejected (no matching liquidity) — fall back to GTC limit
                    warn!(
                        "⚠️ FOK not filled ({}), falling back to GTC limit (side={} shares={:.2} price={:.4})",
                        resp.error_msg, trade.side, trade.shares, trade.entry_price,
                    );
                    match c.place_limit_buy(
                        token_id,
                        trade.shares.max(5.0), // GTC requires min 5 shares
                        trade.entry_price,
                        market.tick_size,
                        market.neg_risk,
                    ).await {
                        Ok(r) if r.success => {
                            info!(
                                "📝 GTC fallback placed: {} {:.2}s @ ${:.4}  order_id={}",
                                trade.side, trade.shares.max(5.0), trade.entry_price,
                                &r.order_id[..r.order_id.len().min(16)],
                            );
                            true
                        }
                        Ok(r) => {
                            warn!("⚠️ GTC fallback rejected: {}", r.error_msg);
                            false
                        }
                        Err(e) => {
                            warn!("⚠️ GTC fallback failed: {}", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "⚠️ Order placement failed: {}  (side={} shares={:.2} price={:.4})",
                        e, trade.side, trade.shares, trade.entry_price,
                    );
                    false
                }
            };

            if !order_ok {
                return None;
            }
        }
    }

    // ── Record in TUI ─────────────────────────────────────────────────────────
    if let Some(t) = tui {
        let cost = trade.entry_price * trade.shares;
        t.deduct_trade_cost(cost).await;
        Some(t.push_conviction_trade(&market.asset, trade).await)
    } else {
        Some(0)
    }
}

/// Compute shares from a USDC budget at a given price.
/// Rounds up to whole integer so maker_amount stays at most 2 decimal places
/// (prices are already multiples of tick_size 0.01).
fn shares_for_usd(usd: f64, price: f64) -> f64 {
    (usd / price).ceil()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── Background resolver ───────────────────────────────────────────────────────

/// Polls every 30s until market resolution is CONFIRMED, then flags WIN/LOSS.
///
/// Live mode  (ctf = Some): gate on `redeem_positions` success — on-chain truth.
///   WIN/LOSS is only flagged AFTER the CTF contract confirms resolution.
///   This prevents false flags caused by last-second price flips.
///
/// Sim mode   (ctf = None): no on-chain gate. Use price signal with strict
///   threshold (≥0.95 / ≤0.05) so a late flip is much less likely to fool us.
async fn resolve_in_background(
    pos:    ConvPos,
    market: Market,
    ob:     OrderbookWatcher,
    prices: PriceWatcher,
    symbol: String,
    tui:    Option<TuiHandle>,
    ctf:    Option<Arc<CtfClient>>,
) {
    const POLL:     Duration = Duration::from_secs(30);
    const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
    /// Price threshold for sim mode — stricter than 0.90 to reduce flip risk.
    const SIM_THRESHOLD: f64 = 0.95;

    let tag    = format!("[{}]", market.asset.to_uppercase());
    let start  = std::time::Instant::now();
    let yes_id = &market.yes_token_id;
    let no_id  = &market.no_token_id;
    let is_live = ctf.is_some();

    info!(
        "🔍 Resolver{}: waiting for {} resolution (poll=30s max=10min)",
        tag,
        if is_live { "on-chain" } else { "price" }
    );

    loop {
        sleep(POLL).await;

        let timed_out = start.elapsed() >= MAX_WAIT;

        // ── Compute price-based outcome (used in both modes) ──────────────────
        let yes_val = ob.best_ask(yes_id).or_else(|| ob.best_bid(yes_id));
        let no_val  = ob.best_bid(no_id).or_else(|| ob.best_ask(no_id));
        let ref_p   = prices.reference_price(&symbol, market.resolution_ts * 1_000);
        let cur_p   = prices.current_price(&symbol);
        let yes_wins_chainlink = ref_p.zip(cur_p).map(|(r, c)| c >= r);

        // Derive exit value for this position's side
        let exit_val: Option<f64> = match pos.side.as_str() {
            "UP" => {
                if yes_wins_chainlink == Some(true)  { Some(1.0) }
                else if yes_wins_chainlink == Some(false) { Some(0.0) }
                else { yes_val }
            }
            "DOWN" => {
                if yes_wins_chainlink == Some(false) { Some(1.0) }
                else if yes_wins_chainlink == Some(true)  { Some(0.0) }
                else { no_val }
            }
            _ => None,
        };

        // ── LIVE MODE: gate on on-chain redeem ────────────────────────────────
        if let Some(ref c) = ctf {
            match c.redeem_positions(&market.condition_id, market.neg_risk).await {
                Ok(()) => {
                    // On-chain confirmed → now determine final outcome from price
                    let exit = exit_val
                        .filter(|&e| e >= SIM_THRESHOLD || e <= (1.0 - SIM_THRESHOLD))
                        .unwrap_or_else(|| {
                            // Price data stale — infer from chainlink direction
                            match yes_wins_chainlink {
                                Some(true)  => if pos.side == "UP"   { 1.0 } else { 0.0 },
                                Some(false) => if pos.side == "DOWN" { 1.0 } else { 0.0 },
                                None        => 0.5, // genuinely unknown, use midpoint
                            }
                        });

                    let pnl     = (exit - pos.entry_price) * pos.shares;
                    let outcome = if pnl >= 0.0 { "WIN" } else { "LOSS" };
                    info!(
                        "✅ Resolver{}: ON-CHAIN CONFIRMED {} {} exit=${:.4} entry=${:.4} P&L={:+.4}",
                        tag, pos.side, outcome, exit, pos.entry_price, pnl
                    );
                    if let Some(ref t) = tui {
                        t.update_conviction_trade(pos.trade_id, exit, pnl).await;
                        t.record_conviction_result(exit * pos.shares, pnl).await;
                    }
                    // Log redeem success to onchain event panel
                    if let Some(ref t) = tui {
                        t.push_onchain_event(crate::tui::OnchainEvent {
                            timestamp_ms: now_ms(),
                            asset:  market.asset.clone(),
                            action: "Redeemed".to_string(),
                            detail: format!("{} {} exit=${:.4} P&L={:+.4}", pos.side, outcome, exit, pnl),
                        }).await;
                    }
                    return;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not resolved") {
                        // Contract not settled yet — keep waiting
                        if timed_out {
                            warn!("⚠️ Resolver{}: on-chain not resolved after 10min — giving up", tag);
                            flag_timed_out(&pos, yes_id, no_id, &ob, &tui).await;
                            return;
                        }
                        info!(
                            "⏳ Resolver{}: {}s — contract not resolved yet (yes={:?} chainlink={:?})",
                            tag,
                            start.elapsed().as_secs(),
                            yes_val.map(|p| format!("{:.4}", p)),
                            cur_p.map(|p| format!("{:.2}", p)),
                        );
                        continue;
                    } else {
                        // Permanent error — fall back to price-based flag
                        warn!("⚠️ Resolver{}: redeem error ({}), falling back to price", tag, msg);
                        flag_from_price(&pos, exit_val, yes_id, no_id, &ob, &tui, &tag).await;
                        return;
                    }
                }
            }
        }

        // ── SIM MODE: price-based with strict threshold ───────────────────────
        if let Some(exit) = exit_val {
            if exit >= SIM_THRESHOLD || exit <= (1.0 - SIM_THRESHOLD) || timed_out {
                flag_from_price(&pos, Some(exit), yes_id, no_id, &ob, &tui, &tag).await;
                return;
            }
        } else if timed_out {
            flag_timed_out(&pos, yes_id, no_id, &ob, &tui).await;
            return;
        }

        info!(
            "⏳ Resolver{}: {}s — waiting for price clarity (yes={:?} no={:?} chainlink={:?})",
            tag,
            start.elapsed().as_secs(),
            yes_val.map(|p| format!("{:.4}", p)),
            no_val.map(|p| format!("{:.4}", p)),
            cur_p.map(|p| format!("{:.2}", p)),
        );
    }
}

/// Flag WIN/LOSS from a known exit value.
async fn flag_from_price(
    pos:    &ConvPos,
    exit_val: Option<f64>,
    yes_id: &str,
    no_id:  &str,
    ob:     &OrderbookWatcher,
    tui:    &Option<TuiHandle>,
    tag:    &str,
) {
    let exit = exit_val.unwrap_or_else(|| {
        ob.best_ask(yes_id).or_else(|| ob.best_bid(no_id)).unwrap_or(pos.entry_price)
    });
    let pnl     = (exit - pos.entry_price) * pos.shares;
    let outcome = if pnl >= 0.0 { "WIN" } else { "LOSS" };
    info!(
        "✅ Resolver{}: PRICE-CONFIRMED {} {} exit=${:.4} P&L={:+.4}",
        tag, pos.side, outcome, exit, pnl
    );
    if let Some(ref t) = tui {
        t.update_conviction_trade(pos.trade_id, exit, pnl).await;
        t.record_conviction_result(exit * pos.shares, pnl).await;
    }
}

/// Flag using last known orderbook price when all else fails (timeout).
async fn flag_timed_out(
    pos:    &ConvPos,
    yes_id: &str,
    no_id:  &str,
    ob:     &OrderbookWatcher,
    tui:    &Option<TuiHandle>,
) {
    let last_val = ob.best_ask(yes_id)
        .or_else(|| ob.best_bid(no_id))
        .unwrap_or(pos.entry_price);
    let pnl = (last_val - pos.entry_price) * pos.shares;
    warn!(
        "⚠️ Resolver: timeout fallback — exit=${:.4} P&L={:+.4}",
        last_val, pnl
    );
    if let Some(ref t) = tui {
        t.update_conviction_trade(pos.trade_id, last_val, pnl).await;
        t.record_conviction_result(last_val * pos.shares, pnl).await;
    }
}

// ── On-chain redemption with retry ───────────────────────────────────────────

