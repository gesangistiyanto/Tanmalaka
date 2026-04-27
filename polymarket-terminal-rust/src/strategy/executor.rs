//! Maker rebate strategy executor.
//!
//! Strategy:
//!   1. Wait for market open (10s after eventStartTime)
//!   2. Subscribe to real-time orderbook for YES and NO tokens
//!   3. Wait until bid prices are in range and combined <= maxCombined
//!   4. Place GTC limit BUY orders on both sides (maker — top of book)
//!   5. Monitor fills via onchain balance checks + WS fill signals
//!   6. When both sides fill: merge YES+NO → USDC
//!   7. Handle cut-loss, ghost fills, and one-sided fills

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::position::{now_ms, Position, PositionStatus, SideState};
use crate::clob::ClobClient;
use crate::config::Config;
use crate::market::types::Market;
use crate::onchain::ctf::CtfClient;
use crate::tui::{SimPosition, TuiHandle};
use crate::ws::fills::{FillEvent, FillWatcher};
use crate::ws::orderbook::OrderbookWatcher;

const ENTRY_DELAY_MS: i64 = 10_000; // wait 10s after market open before placing orders
const CLOB_MIN_SHARES: f64 = 5.0;
const FAST_POLL_COUNT: usize = 10; // poll every 1s for first 10 iterations, then event-driven

/// Result returned by `run_once`.
pub struct CycleResult {
    pub one_sided: bool,
}

/// Run the maker rebate strategy for a single market.
/// Returns true if the cycle ended one-sided (avoid re-entry).
pub async fn run_once(
    market: &Market,
    config: &Config,
    clob: Arc<ClobClient>,
    ctf: Arc<CtfClient>,
    ob: &OrderbookWatcher,
    fills: &FillWatcher,
    tui: Option<TuiHandle>,
) -> CycleResult {
    let tag = format!("[{}]", market.asset.to_uppercase());
    let label: String = market.question.chars().take(50).collect();

    // ── Subscribe to orderbook early so data accumulates during pre-open wait ──
    ob.subscribe(&[&market.yes_token_id, &market.no_token_id]);

    // Bootstrap from REST immediately so TUI has data during the pre-open wait.
    // bootstrap_rest is a no-op if WS already delivered a snapshot.
    ob.bootstrap_rest(&config.clob_host, &market.yes_token_id).await;
    ob.bootstrap_rest(&config.clob_host, &market.no_token_id).await;

    // ── Wait for market open ──────────────────────────────────────────────────
    let open_ms = market.event_start_time.timestamp_millis();
    let entry_not_before = open_ms + ENTRY_DELAY_MS;
    let mut remaining_wait = entry_not_before - now_ms();
    if remaining_wait > 0 {
        info!("⏳ MakerMM{}: waiting {}s for market to stabilize...", tag, remaining_wait / 1000);
        // Sleep in 15s chunks, retrying REST bootstrap each time so the TUI
        // shows orderbook data even for pre-open markets.
        while remaining_wait > 0 {
            let chunk = remaining_wait.min(15_000);
            sleep(Duration::from_millis(chunk as u64)).await;
            remaining_wait -= chunk;
            if !ob.has_data(&market.yes_token_id) {
                ob.bootstrap_rest(&config.clob_host, &market.yes_token_id).await;
            }
            if !ob.has_data(&market.no_token_id) {
                ob.bootstrap_rest(&config.clob_host, &market.no_token_id).await;
            }
        }
    }

    info!("🎯 MakerMM{}: entering — {}", tag, label);

    // Wait for initial snapshot (up to 5s), then fall back to REST bootstrap
    if !ob.has_data(&market.yes_token_id) || !ob.has_data(&market.no_token_id) {
        info!("⏳ MakerMM{}: waiting for orderbook snapshot...", tag);
        ob.wait_for_update(
            &[&market.yes_token_id, &market.no_token_id],
            Duration::from_secs(5),
        )
        .await;
        // Still no data — WS server may not have the token yet (pre-open market)
        if !ob.has_data(&market.yes_token_id) {
            ob.bootstrap_rest(&config.clob_host, &market.yes_token_id).await;
        }
        if !ob.has_data(&market.no_token_id) {
            ob.bootstrap_rest(&config.clob_host, &market.no_token_id).await;
        }
    }

    // ── Entry condition loop ──────────────────────────────────────────────────
    let ts = market.tick_size;
    let min_price = config.min_price;
    let max_price = config.max_price;
    let max_combined = config.max_combined;
    let poll_dur = Duration::from_millis(config.poll_ms);

    let (yes_bid, no_bid) = loop {
        let ms_left = market.end_time.timestamp_millis() - now_ms();
        
        // No-Trade Zone: Don't start NEW trades if we're too close to cut-loss
        // (Giving ourselves a 30s buffer to complete a trade normally)
        if ms_left <= (config.cut_loss_time + 30) as i64 * 1000 {
            warn!("MakerMM{}: too close to cut-loss zone — skipping new entry", tag);
            return CycleResult { one_sided: false };
        }

        let yes_best_bid = ob.best_bid(&market.yes_token_id);
        let yes_ask     = ob.best_ask(&market.yes_token_id);
        let no_best_bid = ob.best_bid(&market.no_token_id);
        let no_ask      = ob.best_ask(&market.no_token_id);

        // Merged market view: infer each side from the opposite board too.
        // YES value can be seen directly from YES bid/ask, or indirectly from NO ask/bid.
        // This avoids the "horse blinders" problem where one board looks empty and we
        // accidentally buy/sell against a stale or one-sided quote.
        let merged_yes_bid = yes_best_bid
            .or_else(|| no_ask.map(|a| round_tick(1.0 - a, ts)));
        let merged_yes_ask = yes_ask
            .or_else(|| no_best_bid.map(|b| round_tick(1.0 - b, ts)));
        let merged_no_bid = no_best_bid
            .or_else(|| yes_ask.map(|a| round_tick(1.0 - a, ts)));
        let merged_no_ask = no_ask
            .or_else(|| yes_best_bid.map(|b| round_tick(1.0 - b, ts)));

        // Use a merged fair view, preferring actual bids, otherwise ask-derived values.
        let yes_mid = merged_yes_bid
            .or_else(|| merged_yes_ask.map(|a| round_tick(a - ts, ts)));
        let no_mid  = merged_no_bid
            .or_else(|| merged_no_ask.map(|a| round_tick(a - ts, ts)));

        match (yes_mid, no_mid) {
            (None, _) | (_, None) => {
                let has_any = ob.has_data(&market.yes_token_id) || ob.has_data(&market.no_token_id);
                if !has_any {
                    info!("⏳ MakerMM{}: waiting for orderbook snapshot \
                        (yes=...{} no=...{})",
                        tag,
                        &market.yes_token_id[market.yes_token_id.len().saturating_sub(12)..],
                        &market.no_token_id[market.no_token_id.len().saturating_sub(12)..]);
                    // Try REST bootstrap while waiting for WS snapshot
                    ob.bootstrap_rest(&config.clob_host, &market.yes_token_id).await;
                    ob.bootstrap_rest(&config.clob_host, &market.no_token_id).await;
                } else {
                    info!("⚠️  MakerMM{}: no price data \
                        (yes_bid={:?} yes_ask={:?} no_bid={:?} no_ask={:?})",
                        tag, merged_yes_bid, merged_yes_ask, merged_no_bid, merged_no_ask);
                }
                ob.wait_for_update(&[&market.yes_token_id, &market.no_token_id], poll_dur).await;
                continue;
            }
            (Some(y_mid), Some(n_mid)) => {
                // Market Chasing Logic: Try to bid at the best bid for both sides
                let mut y = round_tick(y_mid + ts, ts);
                if let Some(ask) = merged_yes_ask {
                    if y >= ask - ts {
                        y = round_tick(ask - 2.0 * ts, ts);
                    }
                }

                let mut n = round_tick(n_mid + ts, ts);
                if let Some(ask) = merged_no_ask {
                    if n >= ask - ts {
                        n = round_tick(ask - 2.0 * ts, ts);
                    }
                }

                // Range check: ensures we're not entering a market that's too lopsided
                let cheap_bid = y.min(n);
                if cheap_bid < min_price || cheap_bid > max_price {
                    info!("🚫 MakerMM{}: cheap bid ${:.3} outside range [{}-{}] — waiting",
                        tag, cheap_bid, min_price, max_price);
                    ob.wait_for_update(&[&market.yes_token_id, &market.no_token_id], poll_dur).await;
                    continue;
                }

                // Safety Filter: Ensure combined price is profitable.
                // If the sum of best bids exceeds our limit, we must adjust (usually the more expensive side).
                if y + n > max_combined {
                    if y <= n {
                        n = round_tick(max_combined - y, ts);
                    } else {
                        y = round_tick(max_combined - n, ts);
                    }
                }

                if y <= 0.01 || n <= 0.01 {
                    info!("🚫 MakerMM{}: bids ${:.3}/${:.3} out of bounds — waiting", tag, y, n);
                    ob.wait_for_update(&[&market.yes_token_id, &market.no_token_id], poll_dur).await;
                    continue;
                }

                let combined = ((y + n) * 10000.0).round() / 10000.0;
                let yes_src = if yes_best_bid.is_some() { "bid" } else if no_ask.is_some() { "opp-ask-derived" } else { "ask-derived" };
                let no_src  = if no_best_bid.is_some() { "bid" } else if yes_ask.is_some() { "opp-ask-derived" } else { "ask-derived" };
                info!("✅ MakerMM{}: ready — YES ${:.4}({}) + NO ${:.4}({}) = ${:.4}",
                    tag, y, yes_src, n, no_src, combined);
                break (y, n);
            }
        }
    };

    // Keep orderbook subscription alive (fill monitor takes over, but
    // re-entry needs live book data and TUI shows it continuously).

    let target_shares = config.trade_size;
    if target_shares < CLOB_MIN_SHARES {
        warn!("MakerMM{}: shares {} < CLOB min {} — skipping", tag, target_shares, CLOB_MIN_SHARES);
        return CycleResult { one_sided: false };
    }

    // ── Simulation (dry_run) — no real orders, orderbook-based fill detection ─
    if config.dry_run {
        let sim = sim_monitor_loop(
            market, config, yes_bid, no_bid, ob, tui.as_ref(),
        ).await;
        let cost = (yes_bid + no_bid) * target_shares;
        if let Some(ref t) = tui {
            t.record_trade_result(cost, sim.pnl).await;
            t.set_position(&market.asset, None).await;
        }
        let sign = if sim.pnl >= 0.0 { "+" } else { "" };
        info!("MakerMM{}: 🎮 [SIM] done | P&L: {}${:.4}", tag, sign, sim.pnl);
        return CycleResult { one_sided: sim.one_sided };
    }

    // ── Balance check ─────────────────────────────────────────────────────────
    match ctf.usdc_balance().await {
        Ok(bal) => {
            let needed = (yes_bid + no_bid) * target_shares;
            if bal < needed {
                error!("MakerMM{}: insufficient balance ${:.2} (need ${:.2})", tag, bal, needed);
                return CycleResult { one_sided: false };
            }
        }
        Err(e) => warn!("MakerMM{}: balance check failed: {}", tag, e),
    }

    // ── Snapshot baseline balances before placing orders ─────────────────────
    let (yes_baseline, no_baseline) = tokio::join!(
        async { ctf.token_balance(&market.yes_token_id).await.unwrap_or(0.0) },
        async { ctf.token_balance(&market.no_token_id).await.unwrap_or(0.0) },
    );

    // ── Place orders ──────────────────────────────────────────────────────────
    info!("MakerMM{}: 📝 placing BUY — YES ${:.3}×{} + NO ${:.3}×{} = ${:.2}",
        tag, yes_bid, target_shares, no_bid, target_shares,
        (yes_bid + no_bid) * target_shares);

    let (yes_res, no_res) = tokio::join!(
        clob.place_limit_buy(&market.yes_token_id, target_shares, yes_bid, ts, market.neg_risk),
        clob.place_limit_buy(&market.no_token_id, target_shares, no_bid, ts, market.neg_risk),
    );

    let yes_buy = match yes_res {
        Ok(r) => r,
        Err(e) => {
            error!("MakerMM{}: YES order failed: {}", tag, e);
            return CycleResult { one_sided: false };
        }
    };
    let no_buy = match no_res {
        Ok(r) => r,
        Err(e) => {
            error!("MakerMM{}: NO order failed: {}", tag, e);
            // Cancel YES if it succeeded
            if yes_buy.success {
                clob.cancel_order(&yes_buy.order_id).await;
            }
            return CycleResult { one_sided: false };
        }
    };

    if !yes_buy.success || !no_buy.success {
        error!("MakerMM{}: order placement failed — YES:{} NO:{}", tag, yes_buy.success, no_buy.success);
        clob.cancel_order(&yes_buy.order_id).await;
        clob.cancel_order(&no_buy.order_id).await;
        return CycleResult { one_sided: false };
    }

    info!("MakerMM{}: 📬 orders placed — YES id=...{} NO id=...{}",
        tag,
        &yes_buy.order_id[yes_buy.order_id.len().saturating_sub(8)..],
        &no_buy.order_id[no_buy.order_id.len().saturating_sub(8)..]);

    // Push initial position to TUI
    if let Some(ref t) = tui {
        t.set_position(&market.asset, Some(SimPosition {
            yes_price: yes_bid,
            no_price: no_bid,
            shares: target_shares,
            yes_filled: false,
            no_filled: false,
            status: "Monitoring".to_string(),
            pnl: None,
        })).await;
    }

    // ── Build position state ──────────────────────────────────────────────────
    let mut pos = Position {
        asset: market.asset.clone(),
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        end_time_ms: market.end_time.timestamp_millis(),
        market_open_time_ms: open_ms,
        tick_size: ts,
        neg_risk: market.neg_risk,
        target_shares,
        yes: SideState::new(&market.yes_token_id, yes_bid, target_shares, &yes_buy.order_id, yes_baseline),
        no: SideState::new(&market.no_token_id, no_bid, target_shares, &no_buy.order_id, no_baseline),
        status: PositionStatus::Monitoring,
        total_profit: 0.0,
        one_sided: false,
        ghost_fill_since_ms: None,
        both_filled_since_ms: None,
        first_fill_time_ms: None,
        merge_fail_count: 0,
    };

    // ── Register tokens for fill watch ────────────────────────────────────────
    fills.watch(&market.yes_token_id).await;
    fills.watch(&market.no_token_id).await;
    let mut fill_rx = fills.subscribe();

    // ── Monitor loop ──────────────────────────────────────────────────────────
    let result = monitor_loop(&mut pos, &clob, &ctf, &ob, &mut fill_rx, &tag, tui.as_ref()).await;

    // ── Cleanup ───────────────────────────────────────────────────────────────
    fills.unwatch(&market.yes_token_id).await;
    fills.unwatch(&market.no_token_id).await;
    clob.cancel_order(&pos.yes.order_id).await;
    clob.cancel_order(&pos.no.order_id).await;

    // ── Auto-redeem if holding single side ───────────────────────────────────
    if let PositionStatus::Holding { ref side } = pos.status.clone() {
        tokio::spawn({
            let ctf2 = ctf.clone();
            let cid = pos.condition_id.clone();
            let neg = pos.neg_risk;
            let end = pos.end_time_ms;
            let side = side.clone();
            let tag2 = tag.clone();
            async move {
                wait_and_redeem(ctf2, &cid, neg, end, &side, &tag2).await;
            }
        });
    }

    // Record result to sim balance tracker
    let cost = pos.yes.cost + pos.no.cost;
    if let Some(ref t) = tui {
        // In simulation: if both filled → pnl = merge_shares - cost (positive)
        // If not filled → positions are cancelled, cost is returned (pnl = 0 in sim since no real spend)
        // filled → +profit; cut-loss with no fills → 0
        t.record_trade_result(cost, pos.total_profit).await;
        t.set_position(&pos.asset, None).await;
        }

    let sign = if pos.total_profit >= 0.0 { "+" } else { "" };
    info!("MakerMM{}: 📊 done | P&L: {}${:.4}", tag, sign, pos.total_profit);

    CycleResult { one_sided: pos.one_sided }
}

// ── Monitor loop ──────────────────────────────────────────────────────────────

async fn monitor_loop(
    pos: &mut Position,
    clob: &ClobClient,
    ctf: &CtfClient,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    fill_rx: &mut tokio::sync::broadcast::Receiver<FillEvent>,
    tag: &str,
    tui: Option<&TuiHandle>,
) -> Result<()> {
    let mut fast_count = 0usize;

    loop {
        if pos.status == PositionStatus::Done {
            return Ok(());
        }

        // ── Onchain balance check (source of truth) ───────────────────────────
        let (yes_bal, no_bal) = tokio::join!(
            ctf.token_balance(&pos.yes.token_id),
            ctf.token_balance(&pos.no.token_id),
        );

        let yes_bal = yes_bal.unwrap_or(0.0);
        let no_bal = no_bal.unwrap_or(0.0);

        let yes_net = (yes_bal - pos.yes.baseline).max(0.0);
        let no_net = (no_bal - pos.no.baseline).max(0.0);

        // Sync fill flags from onchain
        let prev_yes = pos.yes.filled;
        let prev_no  = pos.no.filled;
        if !pos.yes.filled && yes_net >= pos.target_shares * 0.99 {
            pos.yes.filled = true;
            info!("MakerMM{}: ✅ YES filled (onchain) {:.4} shares", tag, yes_net);
        }
        if !pos.no.filled && no_net >= pos.target_shares * 0.99 {
            pos.no.filled = true;
            info!("MakerMM{}: ✅ NO filled (onchain) {:.4} shares", tag, no_net);
        }
        // Push fill status update to TUI when something changed
        if pos.yes.filled != prev_yes || pos.no.filled != prev_no {
            if let Some(t) = tui {
                let status = match (pos.yes.filled, pos.no.filled) {
                    (true, true)  => "Both Filled",
                    (true, false) => "YES Filled",
                    (false, true) => "NO Filled",
                    _             => "Monitoring",
                };
                t.set_position(&pos.asset, Some(SimPosition {
                    yes_price: pos.yes.buy_price,
                    no_price: pos.no.buy_price,
                    shares: pos.target_shares,
                    yes_filled: pos.yes.filled,
                    no_filled: pos.no.filled,
                    status: status.to_string(),
                    pnl: None,
                })).await;
            }
        }

        // ── Cancel cheap when expensive fills first ───────────────────────────
        if clob.config.cancel_cheap_on_exp_fill {
            let exp_is_yes = pos.yes.buy_price >= pos.no.buy_price;
            let (exp, cheap) = if exp_is_yes {
                (&pos.yes.clone(), &pos.no.clone())
            } else {
                (&pos.no.clone(), &pos.yes.clone())
            };

            if exp.filled && !cheap.filled {
                info!("MakerMM{}: ⚡ {} filled first — cancelling cheap {} order", tag,
                    if exp_is_yes { "YES" } else { "NO" },
                    if exp_is_yes { "NO" } else { "YES" });
                clob.cancel_order(&cheap.order_id).await;
                pos.status = PositionStatus::Holding {
                    side: if exp_is_yes { "yes".into() } else { "no".into() },
                };
                return Ok(());
            }
        }

        // ── Both sides filled → merge ─────────────────────────────────────────
        if yes_net >= pos.target_shares * 0.5 && no_net >= pos.target_shares * 0.5 {
            pos.both_filled_since_ms = None;
            pos.yes.filled = true;
            pos.no.filled = true;

            let merge_shares = (yes_net.min(no_net) * 1_000_000.0).floor() / 1_000_000.0;
            let is_full = yes_net >= pos.target_shares * 0.99 && no_net >= pos.target_shares * 0.99;
            info!("MakerMM{}: 🎉 {} fill — YES={:.4} NO={:.4}, merging {:.4}",
                tag, if is_full { "FULL" } else { "PARTIAL" }, yes_net, no_net, merge_shares);

            match ctf.merge_positions(&pos.condition_id, merge_shares, pos.neg_risk).await {
                Ok(()) => {
                    let total_cost = pos.yes.cost + pos.no.cost;
                    pos.total_profit = merge_shares - total_cost;
                    pos.status = PositionStatus::Done;
                    info!("MakerMM{}: 🎉 MERGED {:.4} shares → ${:.4} | cost ${:.4} | P&L ${:.4}",
                        tag, merge_shares, merge_shares, total_cost, pos.total_profit);
                    if let Some(t) = tui {
                        t.set_position(&pos.asset, Some(SimPosition {
                            yes_price: pos.yes.buy_price,
                            no_price: pos.no.buy_price,
                            shares: pos.target_shares,
                            yes_filled: true,
                            no_filled: true,
                            status: "MERGED ✓".to_string(),
                            pnl: Some(pos.total_profit),
                        })).await;
                    }
                    return Ok(());
                }
                Err(e) => {
                    pos.merge_fail_count += 1;
                    let backoff = (5 * pos.merge_fail_count).min(30) as u64;
                    warn!("MakerMM{}: merge failed (attempt {}) — retrying in {}s: {}",
                        tag, pos.merge_fail_count, backoff, e);
                    sleep(Duration::from_secs(backoff)).await;
                    continue;
                }
            }
        }

        // ── WS fill fallback: both WS-signalled but onchain not reflected ─────
        if pos.yes.filled && pos.no.filled
            && yes_net < pos.target_shares * 0.5
            && no_net < pos.target_shares * 0.5
        {
            let now = now_ms();
            let since = pos.both_filled_since_ms.get_or_insert(now);
            let waited_s = (now - *since) / 1000;
            if waited_s >= 15 {
                warn!("MakerMM{}: both WS-filled but onchain not reflecting after {}s — merging with target", tag, waited_s);
                let _ = ctf.merge_positions(&pos.condition_id, pos.target_shares, pos.neg_risk).await;
                pos.status = PositionStatus::Done;
                return Ok(());
            }
            info!("MakerMM{}: ⏳ both WS-filled, waiting for onchain ({}s / 15s)...", tag, waited_s);
        }

        // ── Cut-loss check ────────────────────────────────────────────────────
        let ms_left = pos.end_time_ms - now_ms();
        if ms_left <= clob.config.cut_loss_time as i64 * 1000 {
            // EMERGENCY HEDGE (LIVE)
            if (pos.yes.filled && !pos.no.filled) || (yes_net > 0.0 && no_net < 1.0) {
                let best_ask = ob.best_ask(&pos.no.token_id).unwrap_or(0.99);
                let combined = pos.yes.buy_price + best_ask;
                
                if combined > 1.01 {
                    warn!("MakerMM{}: ⚠️ CUT-LOSS! Hedge NO is too expensive (${:.3} + YES ${:.3} = ${:.3}) — skipping hedge to avoid guaranteed loss", 
                        tag, best_ask, pos.yes.buy_price, combined);
                    clob.cancel_order(&pos.no.order_id).await;
                    pos.status = PositionStatus::Holding { side: "yes".into() };
                    return Ok(());
                }

                warn!("MakerMM{}: CUT-LOSS! Hedging YES -> NO via Market Buy at ${:.3}", tag, best_ask);
                clob.cancel_order(&pos.no.order_id).await;
                match clob.place_market_buy(&pos.no.token_id, pos.target_shares, best_ask, pos.tick_size, pos.neg_risk).await {
                    Ok(_) => {
                        info!("MakerMM{}: ✅ Market buy NO submitted for hedge", tag);
                        sleep(Duration::from_millis(1000)).await; 
                        continue; 
                    }
                    Err(e) => error!("MakerMM{}: Hedge failed: {}", tag, e),
                }
            } else if (!pos.yes.filled && pos.no.filled) || (no_net > 0.0 && yes_net < 1.0) {
                let best_ask = ob.best_ask(&pos.yes.token_id).unwrap_or(0.99);
                let combined = pos.no.buy_price + best_ask;

                if combined > 1.01 {
                    warn!("MakerMM{}: ⚠️ CUT-LOSS! Hedge YES is too expensive (${:.3} + NO ${:.3} = ${:.3}) — skipping hedge to avoid guaranteed loss", 
                        tag, best_ask, pos.no.buy_price, combined);
                    clob.cancel_order(&pos.yes.order_id).await;
                    pos.status = PositionStatus::Holding { side: "no".into() };
                    return Ok(());
                }

                warn!("MakerMM{}: CUT-LOSS! Hedging NO -> YES via Market Buy at ${:.3}", tag, best_ask);
                clob.cancel_order(&pos.yes.order_id).await;
                match clob.place_market_buy(&pos.yes.token_id, pos.target_shares, best_ask, pos.tick_size, pos.neg_risk).await {
                    Ok(_) => {
                        info!("MakerMM{}: ✅ Market buy YES submitted for hedge", tag);
                        sleep(Duration::from_millis(1000)).await;
                        continue;
                    }
                    Err(e) => error!("MakerMM{}: Hedge failed: {}", tag, e),
                }
            } else if !pos.yes.filled && !pos.no.filled {
                info!("MakerMM{}: 🚫 Cut-loss - no fills, exiting safely", tag);
                clob.cancel_order(&pos.yes.order_id).await;
                clob.cancel_order(&pos.no.order_id).await;
                pos.status = PositionStatus::Done;
                return Ok(());
            }
        }

        // ── Status log ────────────────────────────────────────────────────────
        let now = now_ms();
        if pos.yes.filled != pos.no.filled {
            let filled_side = if pos.yes.filled { "YES" } else { "NO" };
            let (target_side, target_token_id, filled_price) = if pos.yes.filled {
                ("NO", &pos.no.token_id, pos.yes.buy_price)
            } else {
                ("YES", &pos.yes.token_id, pos.no.buy_price)
            };

            // Dynamic Chasing Logic:
            // If the other side isn't filled, check if we can improve the price
            let best_bid = ob.best_bid(target_token_id);
            if let Some(bid) = best_bid {
                let next_bid = round_tick(bid + pos.tick_size, pos.tick_size);
                let current_order_price = if pos.yes.filled { pos.no.buy_price } else { pos.yes.buy_price };
                
                // Only move if:
                // 1. Next bid is different from current order price
                // 2. Combined price (filled_price + next_bid) is still <= max_combined
                if (next_bid - current_order_price).abs() > 0.0001 && (filled_price + next_bid) <= clob.config.max_combined {
                    info!("MakerMM{}: 🏃 Chasing {}! Moving order from ${:.3} to ${:.3} (Total: ${:.3} <= ${:.3})",
                        tag, target_side, current_order_price, next_bid, (filled_price + next_bid), clob.config.max_combined);
                    
                    let order_to_cancel = if pos.yes.filled { &pos.no.order_id } else { &pos.yes.order_id };
                    clob.cancel_order(order_to_cancel).await;
                    
                    match clob.place_limit_buy(target_token_id, pos.target_shares, next_bid, pos.tick_size, pos.neg_risk).await {
                        Ok(res) if res.success => {
                            if pos.yes.filled {
                                pos.no.order_id = res.order_id;
                                pos.no.buy_price = next_bid;
                            } else {
                                pos.yes.order_id = res.order_id;
                                pos.yes.buy_price = next_bid;
                            }
                            info!("MakerMM{}: ✅ {} order updated to ${:.3}", tag, target_side, next_bid);
                        }
                        Ok(_) => error!("MakerMM{}: ❌ {} order update failed (success=false)", tag, target_side),
                        Err(e) => error!("MakerMM{}: ❌ {} order update error: {}", tag, target_side, e),
                    }
                }
            }

            match pos.first_fill_time_ms {
                None => {
                    pos.first_fill_time_ms = Some(now);
                    info!("MakerMM{}: ⏳ {} filled first — waiting for other side...", tag, filled_side);
                }
                Some(t) if (now - t) > 5 * 60_000 && (now - t) % (5 * 60_000) < 5000 => {
                    let mins = (now - t) / 60_000;
                    info!("MakerMM{}: ⏳ still waiting for {} — {}m elapsed", tag, if pos.yes.filled { "NO" } else { "YES" }, mins);
                }
                _ => {}
            }
        }

        // ── Wait: fast poll for first 10s, then event-driven ─────────────────
        fast_count += 1;
        if fast_count < FAST_POLL_COUNT {
            sleep(Duration::from_secs(1)).await;
        } else {
            // Event-driven: wait for a fill signal or 5s timeout
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match fill_rx.recv().await {
                        Ok(ev) if ev.token_id == pos.yes.token_id || ev.token_id == pos.no.token_id => {
                            let which = if ev.token_id == pos.yes.token_id { "YES" } else { "NO" };
                            info!("MakerMM{}: ⚡ {} fill signal (WS) {:.2} shares @ ${:.3}", tag, which, ev.size, ev.price);
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => break, // receiver lag or channel closed
                    }
                }
            }).await;
        }
    }
}

// ── Simulation monitor loop ───────────────────────────────────────────────────

struct SimResult {
    pnl: f64,
    one_sided: bool,
}

/// Dry-run fill simulation.
///
/// Fill logic (neg_risk UP-DOWN markets):
///   YES filled when best_bid(YES) drops below our entry price
///     — our limit buy was at the top of the book and got taken.
///   NO filled when best_bid(NO) drops below our entry price,
///     OR equivalently when best_ask(YES) rises above (1 − no_entry)
///     — both signals mean the NO bid was consumed.
///
/// P&L:
///   Both filled → rebate profit = (1 − yes_entry − no_entry) × shares
///   One-sided at cut-loss → assume worst-case loss = −held_cost
///   No fills at cut-loss → 0
async fn sim_monitor_loop(
    market: &Market,
    config: &Config,
    yes_entry: f64,
    no_entry: f64,
    ob: &OrderbookWatcher,
    tui: Option<&TuiHandle>,
) -> SimResult {
    let tag = format!("[{}]", market.asset.to_uppercase());
    let shares = config.trade_size;

    info!(
        "MakerMM{}: [SIM] entering — YES ${:.3}×{} + NO ${:.3}×{} = ${:.2}",
        tag,
        yes_entry,
        shares,
        no_entry,
        shares,
        (yes_entry + no_entry) * shares
    );

    if let Some(t) = tui {
        t.set_position(&market.asset, Some(SimPosition {
            yes_price: yes_entry,
            no_price: no_entry,
            shares,
            yes_filled: false,
            no_filled: false,
            status: "Sim: Monitoring".to_string(),
            pnl: None,
        }))
        .await;
    }

    let mut yes_filled = false;
    let mut no_filled = false;
    let mut tui_yes = false;
    let mut tui_no  = false;

    // Track active simulated entry prices for chasing
    let mut cur_yes_entry = yes_entry;
    let mut cur_no_entry  = no_entry;

    loop {
        let ms_left = market.end_time.timestamp_millis() - now_ms();
        if ms_left <= config.cut_loss_time as i64 * 1000 {
            // EMERGENCY HEDGE LOGIC (SIMULATION)
            let (pnl, one_sided, status_text) = match (yes_filled, no_filled) {
                (true, true) => {
                    ((1.0 - cur_yes_entry - cur_no_entry) * shares, false, "Sim: MERGED ✓")
                }
                (true, false) => {
                    // Holding YES only -> Force buy NO at market price (best_ask)
                    let market_no = ob.best_ask(&market.no_token_id).unwrap_or(0.99);
                    let hedge_pnl = (1.0 - cur_yes_entry - market_no) * shares;

                    if cur_yes_entry + market_no > 1.01 {
                        warn!("MakerMM{}: [SIM] Cut-loss! Hedge NO too expensive (${:.3}), holding YES instead", tag, market_no);
                        (0.0, false, "Sim: Holding YES") // PnL 0 because we don't know resolution yet
                    } else {
                        warn!("MakerMM{}: [SIM] Cut-loss! Hedge YES at ${:.3} + NO(market) ${:.3} -> P&L ${:.4}", 
                            tag, cur_yes_entry, market_no, hedge_pnl);
                        (hedge_pnl, false, "Sim: HEDGED YES")
                    }
                }
                (false, true) => {
                    // Holding NO only -> Force buy YES at market price (best_ask)
                    let market_yes = ob.best_ask(&market.yes_token_id).unwrap_or(0.99);
                    let hedge_pnl = (1.0 - market_yes - cur_no_entry) * shares;

                    if cur_no_entry + market_yes > 1.01 {
                        warn!("MakerMM{}: [SIM] Cut-loss! Hedge YES too expensive (${:.3}), holding NO instead", tag, market_yes);
                        (0.0, false, "Sim: Holding NO")
                    } else {
                        warn!("MakerMM{}: [SIM] Cut-loss! Hedge NO at ${:.3} + YES(market) ${:.3} -> P&L ${:.4}", 
                            tag, cur_no_entry, market_yes, hedge_pnl);
                        (hedge_pnl, false, "Sim: HEDGED NO")
                    }
                }
                (false, false) => {
                    info!("MakerMM{}: 🚫 [SIM] cut-loss — no fills, no P&L", tag);
                    (0.0, false, "Sim: CutLoss")
                }
            };

            if let Some(t) = tui {
                t.set_position(&market.asset, Some(SimPosition {
                    yes_price: cur_yes_entry,
                    no_price: cur_no_entry,
                    shares,
                    yes_filled,
                    no_filled,
                    status: status_text.to_string(),
                    pnl: Some(pnl),
                }))
                .await;
            }
            return SimResult { pnl, one_sided };
        }

        // ── Orderbook-based fill detection ────────────────────────────────────
        let yes_best_bid = ob.best_bid(&market.yes_token_id);
        let yes_best_ask = ob.best_ask(&market.yes_token_id);
        let no_best_bid  = ob.best_bid(&market.no_token_id);
        let no_best_ask  = ob.best_ask(&market.no_token_id);

        let ts = market.tick_size;

        if !yes_filled {
            // YES bid taken when the best bid drops below our entry
            if yes_best_bid.map(|b| b < cur_yes_entry).unwrap_or(false) {
                yes_filled = true;
                info!(
                    "MakerMM{}: ✅ [SIM] YES filled at ${:.3} (market bid now ${:.3})",
                    tag,
                    cur_yes_entry,
                    yes_best_bid.unwrap_or(0.0)
                );
            } else {
                // CHASING: If not filled, check if we can improve our price
                if let Some(y_bid) = yes_best_bid {
                    let next_bid = round_tick(y_bid + ts, ts);
                    if (next_bid - cur_yes_entry).abs() > 0.0001 && next_bid + cur_no_entry <= config.max_combined {
                        cur_yes_entry = next_bid;
                    }
                }
            }
        }

        if !no_filled {
            // NO bid taken when:
            //   a) NO best_bid drops below our entry, OR
            //   b) YES ask rises above (1 − no_entry)  [neg_risk complement]
            let threshold = 1.0 - cur_no_entry;
            let by_no_bid  = no_best_bid.map(|b| b < cur_no_entry).unwrap_or(false);
            let by_yes_ask = yes_best_ask.map(|a| a > threshold).unwrap_or(false);
            if by_no_bid || by_yes_ask {
                no_filled = true;
                info!(
                    "MakerMM{}: ✅ [SIM] NO filled at ${:.3} (NO bid={:?} YES ask={:?})",
                    tag,
                    cur_no_entry,
                    no_best_bid,
                    yes_best_ask
                );
            } else {
                // CHASING: If not filled, check if we can improve our price
                if let Some(n_bid) = no_best_bid {
                    let next_bid = round_tick(n_bid + ts, ts);
                    if (next_bid - cur_no_entry).abs() > 0.0001 && next_bid + cur_yes_entry <= config.max_combined {
                        cur_no_entry = next_bid;
                    }
                }
            }
        }

        // ── Both filled → profit ──────────────────────────────────────────────
        if yes_filled && no_filled {
            let pnl = (1.0 - cur_yes_entry - cur_no_entry) * shares;
            info!(
                "MakerMM{}: 🎉 [SIM] BOTH filled — YES ${:.3} + NO ${:.3} → P&L +${:.4}",
                tag,
                cur_yes_entry,
                cur_no_entry,
                pnl
            );
            if let Some(t) = tui {
                t.set_position(&market.asset, Some(SimPosition {
                    yes_price: cur_yes_entry,
                    no_price: cur_no_entry,
                    shares,
                    yes_filled: true,
                    no_filled: true,
                    status: "Sim: MERGED ✓".to_string(),
                    pnl: Some(pnl),
                }))
                .await;
            }
            return SimResult { pnl, one_sided: false };
        }

        // Update TUI only when fill state or price changes
        if yes_filled != tui_yes || no_filled != tui_no || true { // price chasing always updates TUI
            tui_yes = yes_filled;
            tui_no  = no_filled;
            if let Some(t) = tui {
                let status = match (yes_filled, no_filled) {
                    (true, false) => "Sim: YES Filled",
                    (false, true) => "Sim: NO Filled",
                    _             => "Sim: Monitoring",
                };
                t.set_position(&market.asset, Some(SimPosition {
                    yes_price: cur_yes_entry,
                    no_price: cur_no_entry,
                    shares,
                    yes_filled,
                    no_filled,
                    status: status.to_string(),
                    pnl: None,
                }))
                .await;
            }
        }

        ob.wait_for_update(
            &[&market.yes_token_id, &market.no_token_id],
            Duration::from_millis(config.poll_ms),
        )
        .await;
    }
}

// ── Price helpers ─────────────────────────────────────────────────────────────

fn round_tick(price: f64, tick: f64) -> f64 {
    let rounded = (price / tick).round() * tick;
    let decimals = tick.to_string()
        .split('.')
        .nth(1)
        .map(|d| d.len())
        .unwrap_or(2);
    let factor = 10f64.powi(decimals as i32);
    let clamped = ((rounded * factor).round() / factor).clamp(0.01, 0.99);
    clamped
}

// ── Auto-redeem ───────────────────────────────────────────────────────────────

async fn wait_and_redeem(
    ctf: Arc<CtfClient>,
    condition_id: &str,
    neg_risk: bool,
    end_time_ms: i64,
    holding_side: &str,
    tag: &str,
) {
    let wait_ms = (end_time_ms - now_ms()).max(0) as u64;
    if wait_ms > 0 {
        info!("MakerMM{}: ⏳ holding {} — waiting {}s for market to end...",
            tag, holding_side.to_uppercase(), wait_ms / 1000);
        sleep(Duration::from_millis(wait_ms)).await;
    }

    info!("MakerMM{}: ⏳ market ended — polling for resolution...", tag);
    let max_wait = Duration::from_secs(10 * 60);
    let poll = Duration::from_secs(15);
    let start = std::time::Instant::now();

    while start.elapsed() < max_wait {
        match ctf.is_resolved(condition_id).await {
            Ok(true) => {
                info!("MakerMM{}: ✅ resolved — redeeming {}...", tag, holding_side.to_uppercase());
                match ctf.redeem_positions(condition_id, neg_risk).await {
                    Ok(()) => info!("MakerMM{}: ✅ redemption complete", tag),
                    Err(e) => error!("MakerMM{}: redeem error: {}", tag, e),
                }
                return;
            }
            Ok(false) => {
                info!("MakerMM{}: 🔄 not resolved yet ({}s elapsed) — retrying in {}s...",
                    tag, start.elapsed().as_secs(), poll.as_secs());
            }
            Err(e) => warn!("MakerMM{}: resolution poll error: {}", tag, e),
        }
        sleep(poll).await;
    }

    warn!("MakerMM{}: not resolved after 10min — tokens remain in wallet", tag);
}
