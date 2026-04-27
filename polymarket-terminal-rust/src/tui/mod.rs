//! Terminal UI module using ratatui + crossterm.
//!
//! Provides:
//! - `TuiState`   – shared mutable state for the TUI
//! - `TuiHandle`  – cheaply-cloneable handle for pushing updates from other tasks
//! - `TuiLogLayer`– tracing subscriber layer that captures log lines into TuiState
//! - `run_tui`    – async function that owns the terminal and renders 200ms loop

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Terminal;
use tokio::sync::RwLock;
use tracing::{Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

// ── ActiveMarket ──────────────────────────────────────────────────────────────

/// Market info forwarded to the TUI for orderbook labeling and entry price display.
#[derive(Clone)]
pub struct ActiveMarket {
    pub yes_token_id: String,
    pub no_token_id: String,
    pub tick_size: f64,
    pub question: String,
    pub end_time_ms: i64,
    /// Chainlink symbol for this market (e.g. "btc/usd").
    pub chainlink_symbol: String,
    /// Resolution timestamp in ms — Chainlink price at this moment decides YES/NO.
    pub resolution_ts_ms: i64,
    /// Locked reference price captured at monitor start (None until set by strategy).
    pub locked_ref_price: Option<f64>,
}

// ── SimPosition ───────────────────────────────────────────────────────────────

/// Simulated (or live) position currently being held.
#[derive(Clone)]
pub struct SimPosition {
    pub yes_price: f64,
    pub no_price: f64,
    pub shares: f64,
    pub yes_filled: bool,
    pub no_filled: bool,
    pub status: String,
    pub pnl: Option<f64>,
}

// ── Conviction types ──────────────────────────────────────────────────────────

/// A single conviction trade (one entry + optional exit).
#[derive(Clone)]
pub struct ConvictionTrade {
    /// Unique ID assigned by TUI state on insertion.
    pub id: usize,
    pub timestamp_ms: i64,
    /// "YES" or "NO"
    pub side: String,
    /// Human label: "PANIC YES +234%" or "TREND ↑"
    pub signal: String,
    pub entry_price: f64,
    pub shares: f64,
    pub exit_price: Option<f64>,
    pub pnl: Option<f64>,
    /// "Open", "WIN", "LOSS", "Cut"
    pub status: String,
}

/// Onchain activity log entry for conviction strategy.
#[derive(Clone)]
pub struct OnchainEvent {
    pub timestamp_ms: i64,
    pub asset: String,
    pub action: String, // "Redeemed", "Merged", etc.
    pub detail: String,
}

/// All trades recorded under one detected market.
#[derive(Clone)]
pub struct ConvictionMarket {
    pub asset: String,
    pub start_ms: i64,
    pub end_time_ms: i64,
    pub trades: Vec<ConvictionTrade>,
}

// ── TuiState ─────────────────────────────────────────────────────────────────

pub struct TuiState {
    pub phase: String,
    pub log_lines: VecDeque<String>,
    pub dry_run: bool,
    /// Active markets for maker strategy, keyed by asset name.
    pub maker_active_markets: BTreeMap<String, ActiveMarket>,
    /// Active positions for maker strategy, keyed by asset name.
    pub maker_sim_positions: BTreeMap<String, SimPosition>,
    /// Simulated USDC balance (dry_run only)
    pub sim_balance: f64,
    pub sim_pnl_total: f64,
    pub sim_trade_count: u32,
    pub sim_win_count: u32,

    // ── conviction ──
    pub conviction_markets: Vec<ConvictionMarket>,
    pub conviction_total_pnl: f64,
    pub conviction_trade_count: u32,
    pub conviction_win_count: u32,
    /// Monotonically increasing ID counter for conviction trades.
    pub conviction_next_id: usize,
    /// Active markets keyed by asset (max 4 shown in TUI simultaneously).
    pub active_markets: HashMap<String, ActiveMarket>,
    /// Open conviction positions per asset: (side, entry_price).
    pub conviction_open_positions: HashMap<String, (Option<String>, Option<f64>)>,
    /// Onchain activity log for conviction strategy.
    pub onchain_events: Vec<OnchainEvent>,
    /// Live USDC balance fetched from Polygon RPC (live mode only).
    pub live_usdc_balance: Option<f64>,
    /// Live POL (native) balance fetched from Polygon RPC (live mode only).
    pub live_pol_balance: Option<f64>,
}

impl TuiState {
    fn new(dry_run: bool, sim_initial_balance: f64) -> Self {
        Self {
            phase: "Idle".to_string(),
            log_lines: VecDeque::new(),
            dry_run,
            maker_active_markets: BTreeMap::new(),
            maker_sim_positions: BTreeMap::new(),
            sim_balance: sim_initial_balance,
            sim_pnl_total: 0.0,
            sim_trade_count: 0,
            sim_win_count: 0,
            conviction_markets: Vec::new(),
            conviction_total_pnl: 0.0,
            conviction_trade_count: 0,
            conviction_win_count: 0,
            conviction_next_id: 0,
            active_markets: HashMap::new(),
            conviction_open_positions: HashMap::new(),
            onchain_events: Vec::new(),
            live_usdc_balance: None,
            live_pol_balance: None,
        }
    }

    fn push_log(&mut self, line: String) {
        self.log_lines.push_back(line);
        while self.log_lines.len() > 200 {
            self.log_lines.pop_front();
        }
    }
}

// ── TuiHandle ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TuiHandle {
    pub state: Arc<RwLock<TuiState>>,
}

impl TuiHandle {
    pub fn new(dry_run: bool) -> Self {
        Self::with_balance(dry_run, 100.0)
    }

    pub fn with_balance(dry_run: bool, initial_balance: f64) -> Self {
        Self {
            state: Arc::new(RwLock::new(TuiState::new(dry_run, initial_balance))),
        }
    }

    pub async fn set_phase(&self, phase: &str) {
        self.state.write().await.phase = phase.to_string();
    }

    pub async fn set_market(&self, asset: &str, market: Option<ActiveMarket>) {
        let mut s = self.state.write().await;
        if let Some(m) = market {
            s.maker_active_markets.insert(asset.to_string(), m);
        } else {
            s.maker_active_markets.remove(asset);
        }
    }

    // ── Conviction multi-market ───────────────────────────────────────────────

    /// Register or update one active market in the conviction TUI (max 4 shown).
    pub async fn set_market_for_asset(&self, asset: &str, market: ActiveMarket) {
        let mut s = self.state.write().await;
        s.active_markets.insert(asset.to_string(), market);
    }

    /// Remove a market from the conviction TUI (call when task finishes).
    pub async fn remove_market_for_asset(&self, asset: &str) {
        let mut s = self.state.write().await;
        s.active_markets.remove(asset);
        s.conviction_open_positions.remove(asset);
    }

    pub async fn set_position(&self, asset: &str, pos: Option<SimPosition>) {
        let mut s = self.state.write().await;
        if let Some(p) = pos {
            s.maker_sim_positions.insert(asset.to_string(), p);
        } else {
            s.maker_sim_positions.remove(asset);
        }
    }

    /// Record the result of a completed trade cycle (simulation mode).
    /// `pnl` = profit (positive) or loss (negative) for this cycle.
    /// `cost` = total USDC spent on orders this cycle.
    pub async fn record_trade_result(&self, cost: f64, pnl: f64) {
        let mut s = self.state.write().await;
        s.sim_balance += pnl; // net change: pnl = recovered - cost
        s.sim_pnl_total += pnl;
        s.sim_trade_count += 1;
        if pnl > 0.0 {
            s.sim_win_count += 1;
        }
    }

    pub async fn log(&self, line: String) {
        self.state.write().await.push_log(line);
    }

    // ── Conviction helpers ────────────────────────────────────────────────────

    /// Register a new market in the conviction trade history.
    pub async fn push_conviction_market(&self, asset: &str, start_ms: i64, end_time_ms: i64) {
        self.state.write().await.conviction_markets.push(ConvictionMarket {
            asset: asset.to_string(),
            start_ms,
            end_time_ms,
            trades: Vec::new(),
        });
    }

    /// Append a conviction trade to the market for `asset`.
    /// Returns the trade's unique ID for later updates.
    pub async fn push_conviction_trade(&self, asset: &str, mut trade: ConvictionTrade) -> usize {
        let mut s = self.state.write().await;
        let id = s.conviction_next_id;
        s.conviction_next_id += 1;
        s.conviction_trade_count += 1;
        trade.id = id;
        // Find the most-recent market for this asset
        if let Some(m) = s.conviction_markets.iter_mut().rev().find(|m| m.asset == asset) {
            m.trades.push(trade);
        }
        id
    }

    /// Mark a conviction trade as pending resolution (market ended, awaiting on-chain result).
    /// Does NOT set P&L or exit price — those come later via `update_conviction_trade`.
    pub async fn set_trade_resolving(&self, id: usize) {
        let mut s = self.state.write().await;
        for market in &mut s.conviction_markets {
            for trade in &mut market.trades {
                if trade.id == id && trade.pnl.is_none() {
                    trade.status = "Resolving".to_string();
                    return;
                }
            }
        }
    }

    /// Close a conviction trade by ID, recording the exit price and P&L.
    /// Does NOT update totals — call `record_conviction_result` separately after this.
    pub async fn update_conviction_trade(&self, id: usize, exit_price: f64, pnl: f64) {
        let mut s = self.state.write().await;
        for market in &mut s.conviction_markets {
            for trade in &mut market.trades {
                if trade.id == id && trade.pnl.is_none() {
                    trade.exit_price = Some(exit_price);
                    trade.pnl = Some(pnl);
                    trade.status = if pnl >= 0.0 { "WIN".to_string() } else { "LOSS".to_string() };
                    return;
                }
            }
        }
    }

    /// Set or clear the open conviction position for a specific asset.
    pub async fn set_locked_ref_price(&self, asset: &str, price: f64) {
        let mut s = self.state.write().await;
        if let Some(am) = s.active_markets.get_mut(asset) {
            am.locked_ref_price = Some(price);
        }
    }

    pub async fn set_conviction_position_for(&self, asset: &str, side: Option<String>, entry: Option<f64>) {
        let mut s = self.state.write().await;
        if side.is_none() && entry.is_none() {
            s.conviction_open_positions.remove(asset);
        } else {
            s.conviction_open_positions.insert(asset.to_string(), (side, entry));
        }
    }

    /// Push an onchain event to the activity log.
    pub async fn push_onchain_event(&self, event: OnchainEvent) {
        let mut s = self.state.write().await;
        s.onchain_events.push(event);
        if s.onchain_events.len() > 50 {
            s.onchain_events.remove(0);
        }
    }

    /// Record a resolved conviction trade result.
    /// `recovered` = exit_price × shares returned to balance.
    /// `pnl` = (exit - entry) × shares for cumulative display.
    pub async fn record_conviction_result(&self, recovered: f64, pnl: f64) {
        let mut s = self.state.write().await;
        s.sim_balance += recovered;
        s.conviction_total_pnl += pnl;
        if pnl >= 0.0 {
            s.conviction_win_count += 1;
        }
    }

    /// Deduct trade cost from sim_balance when entering a position.
    pub async fn deduct_trade_cost(&self, cost: f64) {
        self.state.write().await.sim_balance -= cost;
    }

    /// Update live USDC + POL balances fetched from Polygon RPC.
    pub async fn set_live_balances(&self, usdc: f64, pol: f64) {
        let mut s = self.state.write().await;
        s.live_usdc_balance = Some(usdc);
        s.live_pol_balance  = Some(pol);
    }

    /// Update unrealised P&L display every 30s for an open position.
    /// Replaces the previous unrealised adjustment with the new one.
    pub async fn update_sim_balance_unrealised(&self, prev_unrealised: f64, new_unrealised: f64) {
        let mut s = self.state.write().await;
        // Undo previous adjustment, apply new one
        s.conviction_total_pnl -= prev_unrealised;
        s.conviction_total_pnl += new_unrealised;
    }
}

// ── TuiLogLayer ──────────────────────────────────────────────────────────────

/// Tracing layer that captures INFO+ log events into TuiState.
pub struct TuiLogLayer {
    pub handle: TuiHandle,
}

impl<S: Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: Context<'_, S>,
    ) {
        let meta = event.metadata();

        // Only INFO and above
        if *meta.level() > Level::INFO {
            return;
        }

        let level_str = match *meta.level() {
            Level::ERROR => "ERROR",
            Level::WARN  => "WARN ",
            Level::INFO  => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };

        // Extract message via a visitor
        let mut msg = String::new();
        let mut visitor = MessageVisitor(&mut msg);
        event.record(&mut visitor);

        // Format: HH:MM:SS [LEVEL] target: message
        let now = chrono::Local::now();
        let line = format!(
            "{} [{}] {}: {}",
            now.format("%H:%M:%S"),
            level_str,
            meta.target(),
            msg,
        );

        // Non-blocking best-effort: use try_write to avoid blocking async task
        if let Ok(mut state) = self.handle.state.try_write() {
            state.push_log(line);
        }
        // If the lock is contended we simply drop the log line rather than block.
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

// ── run_tui ──────────────────────────────────────────────────────────────────

pub async fn run_tui(
    handle: TuiHandle,
    ob: crate::ws::orderbook::OrderbookWatcher,
    prices: crate::ws::price::PriceWatcher,
    config: crate::config::Config,
    wallet_address: String,
) {
    if let Err(e) = run_tui_inner(handle, ob, prices, config, wallet_address).await {
        // Can't use tracing here since we may have already torn down the terminal
        eprintln!("TUI error: {}", e);
    }
}

async fn run_tui_inner(
    handle: TuiHandle,
    ob: crate::ws::orderbook::OrderbookWatcher,
    prices: crate::ws::price::PriceWatcher,
    config: crate::config::Config,
    wallet_address: String,
) -> anyhow::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let quit = Arc::new(AtomicBool::new(false));
    let quit2 = quit.clone();

    // Keyboard event task
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(event)) = events.next().await {
            if let Event::Key(key) = event {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') {
                    quit2.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    });

    // Render loop — runs every 200ms
    loop {
        if quit.load(Ordering::SeqCst) {
            break;
        }

        // Read state under lock (short critical section)
        let (phase, dry_run, log_lines, maker_active_markets, maker_sim_positions, sim_balance, sim_pnl, sim_trades, sim_wins) = {
            let state = handle.state.read().await;
            (
                state.phase.clone(),
                state.dry_run,
                state.log_lines.iter().cloned().collect::<Vec<_>>(),
                state.maker_active_markets.clone(),
                state.maker_sim_positions.clone(),
                state.sim_balance,
                state.sim_pnl_total,
                state.sim_trade_count,
                state.sim_win_count,
            )
        };

        // Draw
        terminal.draw(|f| {
            draw_ui(
                f,
                &phase,
                dry_run,
                &wallet_address,
                &config,
                &ob,
                &prices,
                &maker_active_markets,
                &maker_sim_positions,
                sim_balance,
                sim_pnl,
                sim_trades,
                sim_wins,
                &log_lines,
            );
        })?;

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

// ── UI Drawing ────────────────────────────────────────────────────────────────

fn draw_ui(
    f: &mut ratatui::Frame,
    phase: &str,
    dry_run: bool,
    wallet_address: &str,
    config: &crate::config::Config,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    prices: &crate::ws::price::PriceWatcher,
    maker_active_markets: &BTreeMap<String, ActiveMarket>,
    maker_sim_positions: &BTreeMap<String, SimPosition>,
    sim_balance: f64,
    sim_pnl: f64,
    sim_trades: u32,
    sim_wins: u32,
    log_lines: &[String],
) {
    let size = f.area();

    // ── Outer vertical split: header / status / orderbooks / logs ────────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header bar
            Constraint::Length(3), // status bar (phase + wallet)
            Constraint::Min(10),   // markets + logs
        ])
        .split(size);

    // ── Header ────────────────────────────────────────────────────────────────
    let wallet_mode = format!("{:?}", config.wallet_mode);
    let mode_str = if dry_run { "SIMULATION" } else { "LIVE" };
    let assets_str = config.assets.join(",").to_uppercase();
    let header_text = format!(
        " Polymarket Maker  |  {}  |  {}  |  {} {}",
        wallet_mode.to_uppercase(),
        mode_str,
        assets_str,
        config.duration,
    );
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, outer[0]);

    // ── Status row: Phase | Wallet | [SimBalance] ─────────────────────────────
    let status_constraints = if dry_run {
        vec![Constraint::Percentage(30), Constraint::Percentage(30), Constraint::Percentage(40)]
    } else {
        vec![Constraint::Percentage(50), Constraint::Percentage(50)]
    };
    let status_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(status_constraints)
        .split(outer[1]);

    let phase_widget = Paragraph::new(format!(" Phase: {}", phase))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(phase_widget, status_chunks[0]);

    let truncated_wallet = if wallet_address.len() > 20 {
        format!("{}…{}", &wallet_address[..8], &wallet_address[wallet_address.len() - 6..])
    } else {
        wallet_address.to_string()
    };
    let wallet_widget = Paragraph::new(format!(" Wallet: {}", truncated_wallet))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(wallet_widget, status_chunks[1]);

    if dry_run && status_chunks.len() > 2 {
        let win_rate = if sim_trades > 0 {
            format!("  WR {}/{}", sim_wins, sim_trades)
        } else {
            String::new()
        };
        let pnl_sym = if sim_pnl >= 0.0 { "+" } else { "" };
        let pnl_col = if sim_pnl >= 0.0 { Color::Green } else { Color::Red };
        let bal_col = if sim_balance >= 0.0 { Color::Cyan } else { Color::Red };
        let sim_widget = Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(" Sim Balance: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.2}", sim_balance), Style::default().fg(bal_col).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  P&L: {}{:.2}", pnl_sym, sim_pnl), Style::default().fg(pnl_col)),
            Span::styled(win_rate, Style::default().fg(Color::DarkGray)),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(sim_widget, status_chunks[2]);
    }

    // ── Main area: markets (horizontal) / logs ─────────────────────────────
    let main_area = outer[2];
    let log_height = 8.min(main_area.height / 3).max(4); // fixed small log height
    let market_area_height = main_area.height.saturating_sub(log_height);

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(market_area_height),
            Constraint::Length(log_height),
        ])
        .split(main_area);

    let market_area = main_chunks[0];
    let log_area = main_chunks[1];

    if maker_active_markets.is_empty() {
        let waiting = Paragraph::new(" Waiting for markets...")
            .block(Block::default().borders(Borders::ALL).title(" MARKETS "));
        f.render_widget(waiting, market_area);
    } else {
        let n = maker_active_markets.len();
        let market_constraints: Vec<Constraint> = (0..n)
            .map(|_| Constraint::Ratio(1, n as u32))
            .collect();
        let market_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(market_constraints)
            .split(market_area);

        for (i, (asset, am)) in maker_active_markets.iter().enumerate() {
            let area = market_chunks[i];
            let pos = maker_sim_positions.get(asset);

            let pos_height = if pos.is_some() { 7 } else { 0 };
            let ob_height = area.height.saturating_sub(pos_height);

            let col_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(ob_height),
                    Constraint::Length(pos_height),
                ])
                .split(area);

            render_active_market(f, col_chunks[0], ob, prices, am, pos);
            if let Some(p) = pos {
                render_position_panel(f, col_chunks[1], p);
            }
        }
    }

    // ── Log panel ─────────────────────────────────────────────────────────────
    let log_block = Block::default().borders(Borders::ALL).title(" LOG ");
    let inner = log_block.inner(log_area);
    f.render_widget(log_block, log_area);

    let visible_height = inner.height as usize;
    let start = if log_lines.len() > visible_height {
        log_lines.len() - visible_height
    } else {
        0
    };

    let items: Vec<ListItem> = log_lines[start..]
        .iter()
        .map(|line| {
            let style = if line.contains("[ERROR]") {
                Style::default().fg(Color::Red)
            } else if line.contains("[WARN ]") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![Span::styled(line.as_str(), style)]))
        })
        .collect();

    let log_list = List::new(items);
    f.render_widget(log_list, inner);
}

// ── Position panel ────────────────────────────────────────────────────────────

fn render_position_panel(f: &mut ratatui::Frame, area: Rect, pos: &SimPosition) {
    let title_style = match pos.status.as_str() {
        s if s.contains("MERGED") || s.contains("Done") => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        s if s.contains("Fill") || s.contains("filled") => Style::default().fg(Color::Cyan),
        _ => Style::default().fg(Color::Yellow),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(ratatui::text::Line::from(vec![
            Span::styled(" POSITION ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" [{}] ", pos.status), title_style),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Left: YES/NO bids and fill status
    let mut left: Vec<Line> = vec![];

    let yes_style = if pos.yes_filled {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let no_style = if pos.no_filled {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let yes_sym = if pos.yes_filled { "✓" } else { "○" };
    let no_sym  = if pos.no_filled  { "✓" } else { "○" };

    left.push(Line::from(Span::styled(
        format!(" {} YES  bid ${:.4}  ×{:.0} = ${:.2}",
            yes_sym, pos.yes_price, pos.shares, pos.yes_price * pos.shares),
        yes_style,
    )));
    left.push(Line::from(Span::styled(
        format!(" {} NO   bid ${:.4}  ×{:.0} = ${:.2}",
            no_sym, pos.no_price, pos.shares, pos.no_price * pos.shares),
        no_style,
    )));
    let total_cost = (pos.yes_price + pos.no_price) * pos.shares;
    left.push(Line::from(Span::styled(
        format!("   Total cost:  ${:.2}", total_cost),
        Style::default().fg(Color::White),
    )));

    let height = inner.height as usize;
    if left.len() > height { left.truncate(height); }
    f.render_widget(Paragraph::new(left), halves[0]);

    // Right: P&L and balance estimate
    let mut right: Vec<Line> = vec![];
    let rebate_est = pos.shares * 0.002; // ~0.2% maker rebate estimate
    right.push(Line::from(Span::styled(
        format!(" Rebate est:   +${:.4}", rebate_est),
        Style::default().fg(Color::Cyan),
    )));

    if let Some(pnl) = pos.pnl {
        let (sym, col) = if pnl >= 0.0 { ("+", Color::Green) } else { ("", Color::Red) };
        right.push(Line::from(Span::styled(
            format!(" Realized P&L: {}${:.4}", sym, pnl),
            Style::default().fg(col).add_modifier(Modifier::BOLD),
        )));
    } else {
        // Estimate: if both fill, pnl ≈ shares - total_cost
        let pnl_est = pos.shares - total_cost;
        let (sym, col) = if pnl_est >= 0.0 { ("+", Color::Cyan) } else { ("", Color::Yellow) };
        right.push(Line::from(Span::styled(
            format!(" P&L if fill:  {}${:.4}", sym, pnl_est),
            Style::default().fg(col),
        )));
    }

    // Simulated balance (cost deducted)
    right.push(Line::from(Span::styled(
        format!(" Cost locked:  ${:.2}", total_cost),
        Style::default().fg(Color::DarkGray),
    )));

    if right.len() > height { right.truncate(height); }
    f.render_widget(Paragraph::new(right), halves[1]);
}

// ── Active market rendering ───────────────────────────────────────────────────

fn fmt_remaining(end_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let secs = ((end_ms - now) / 1000).max(0);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

fn render_active_market(
    f: &mut ratatui::Frame,
    area: Rect,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    prices: &crate::ws::price::PriceWatcher,
    am: &ActiveMarket,
    sim_position: Option<&SimPosition>,
) {
    let ts = am.tick_size;

    // Merged market view: derive YES from both YES and NO boards, not just one side.
    let yes_best_bid = ob.best_bid(&am.yes_token_id)
        .or_else(|| ob.best_ask(&am.no_token_id).map(|a| round_tick(1.0 - a, ts)));
    let yes_best_ask = ob.best_ask(&am.yes_token_id)
        .or_else(|| ob.best_bid(&am.no_token_id).map(|b| round_tick(1.0 - b, ts)));

    // Synthetic mid: use merged best_bid if available, else merged best_ask - 1 tick
    let yes_mid = yes_best_bid
        .or_else(|| yes_best_ask.map(|a| round_tick(a - ts, ts)));

    let yes_entry = yes_mid.map(|mid| {
        let mut entry = round_tick(mid + ts, ts);
        if let Some(ask) = yes_best_ask {
            if entry >= ask - ts {
                entry = round_tick(ask - 2.0 * ts, ts);
            }
        }
        entry
    });

    let ref_price = am.locked_ref_price
        .or_else(|| prices.reference_price(&am.chainlink_symbol, am.resolution_ts_ms));
    let cur_price = prices.current_price(&am.chainlink_symbol);
    let price_winning_yes = cur_price.zip(ref_price).map(|(c, r)| c > r);

    let q: String = am.question.chars().take(30).collect();
    let remaining_str = format!(" ⏱{}", fmt_remaining(am.end_time_ms));
    let price_str = match (cur_price, ref_price) {
        (Some(cur), Some(ref_p)) => {
            let delta = cur - ref_p;
            if delta.abs() < 0.005 {
                format!(" | {} {:.2} (ref: same)", am.chainlink_symbol.to_uppercase(), cur)
            } else {
                let sym = if delta >= 0.0 { "+" } else { "" };
                let pct = (delta / ref_p) * 100.0;
                format!(" | {} {:.2} Δ{}{:.2} ({}{:.3}%)",
                    am.chainlink_symbol.to_uppercase(), cur, sym, delta, sym, pct)
            }
        }
        (Some(cur), None) => format!(" | {} {:.2} (ref:...)", am.chainlink_symbol.to_uppercase(), cur),
        _ => format!(" | {} --", am.chainlink_symbol.to_uppercase()),
    };
    let direction = match price_winning_yes {
        Some(true) => "UP winning ↑",
        Some(false) => "DOWN winning ↓",
        None => "Awaiting data...",
    };
    let dir_style = match price_winning_yes {
        Some(true) => Style::default().fg(Color::Green),
        Some(false) => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::DarkGray),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(ratatui::text::Line::from(vec![
            Span::styled(format!(" {} ", q), Style::default().fg(Color::White)),
            Span::styled(remaining_str, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled(price_str, Style::default().fg(Color::Cyan)),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Fixed entry marker: use active position's price (stable) if available,
    // else the dynamically-computed yes_entry (shows where we'd enter next).
    let marker_price = sim_position.map(|p| p.yes_price).or(yes_entry);

    // Split: YES orderbook left, price panel right (if wide enough)
    let (ob_area, price_area) = if inner.width >= 80 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(22)])
            .split(inner);
        (chunks[0], Some(chunks[1]))
    } else {
        (inner, None)
    };

    render_yes_book(f, ob_area, ob, am, marker_price);

    if let Some(price_rect) = price_area {
        render_price_panel(f, price_rect, &am.chainlink_symbol, cur_price, ref_price, direction, dir_style);
    }
}

// ── YES orderbook with block-size bars ────────────────────────────────────────

fn render_yes_book(
    f: &mut ratatui::Frame,
    area: Rect,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    am: &ActiveMarket,
    marker_price: Option<f64>,
) {
    // Build a merged YES book from both boards.
    // Direct YES asks/bids are preferred; if missing, derive them from the opposite NO board.
    let mut asks = ob.top_asks(&am.yes_token_id, 6);
    if asks.is_empty() {
        asks = ob.top_bids(&am.no_token_id, 6)
            .into_iter()
            .map(|(p, s)| (round_tick(1.0 - p, am.tick_size), s))
            .collect();
        asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    }
    let mut bids = ob.top_bids(&am.yes_token_id, 6);
    if bids.is_empty() {
        bids = ob.top_asks(&am.no_token_id, 6)
            .into_iter()
            .map(|(p, s)| (round_tick(1.0 - p, am.tick_size), s))
            .collect();
        bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    }

    let max_size: f64 = asks.iter().chain(bids.iter())
        .map(|(_, s)| *s)
        .fold(0.0_f64, f64::max)
        .max(1.0);

    // Fixed columns: " A 0.2100 " = 10, "  12345.6 " = 10 → ~22 fixed; rest for bar
    let bar_max = (area.width as usize).saturating_sub(22).min(28).max(4);

    // Use the same fallback so spread/mid lines reflect actual market data
    let best_bid = bids.first().map(|(p, _)| *p);
    let best_ask = asks.first().map(|(p, _)| *p);

    let mut lines: Vec<Line> = vec![];

    // Liquidity depth: YES bid vol vs NO ask vol (mirrors strategy compute_liquidity)
    // USD-weighted bid depth: YES bids vs NO bids (both buy sides).
    // YES bids = bullish pressure; NO bids = bearish pressure.
    // (YES bids ≠ NO asks — those are always mirrored in a binary market.)
    let yes_bid_usd: f64 = ob.top_bids(&am.yes_token_id, 10).iter().map(|(p, s)| p * s).sum();
    let no_bid_usd:  f64 = ob.top_bids(&am.no_token_id,  10).iter().map(|(p, s)| p * s).sum();
    let liq_ratio    = if no_bid_usd  > 0.0 { yes_bid_usd / no_bid_usd  } else { f64::INFINITY };
    let no_liq_ratio = if yes_bid_usd > 0.0 { no_bid_usd  / yes_bid_usd } else { f64::INFINITY };
    let liq_style = if liq_ratio >= 2.0 || no_liq_ratio >= 2.0 {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(" YES bids:${:.0}  NO bids:${:.0}  ratio:{:.1}x",
                yes_bid_usd, no_bid_usd,
                if liq_ratio >= 1.0 { liq_ratio } else { no_liq_ratio }),
            liq_style,
        ),
    ]));

    // Entry header: show marker price if position open
    if let Some(mp) = marker_price {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" Entry UP=${:.4}", mp),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ]));
    } else if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
        let spread = ask - bid;
        lines.push(Line::from(Span::styled(
            format!(" Spread ${:.4} ({:.0}t)   ask {:.4}  bid {:.4}",
                spread, spread / am.tick_size, ask, bid),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " Awaiting orderbook...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if asks.is_empty() && bids.is_empty() {
        lines.push(Line::from(Span::styled(
            " No orderbook data yet",
            Style::default().fg(Color::DarkGray),
        )));
        let h = area.height as usize;
        if lines.len() > h { lines.truncate(h); }
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    // Asks — reverse so best ask nearest to mid
    for (i, (price, size)) in asks.iter().rev().enumerate() {
        let is_best = i == asks.len().saturating_sub(1);
        // Mark the ask closest to (or just above) our entry bid
        let is_marker = marker_price.map(|m| (*price - m).abs() < am.tick_size * 1.5).unwrap_or(false);
        let bar = size_bar(*size, max_size, bar_max);
        let style = if is_best {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red)
        };
        let size_text = if is_marker {
            format!(" {:.1} ★", size)
        } else {
            format!(" {:.1}", size)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" A {:.4} ", price), style),
            Span::styled(bar, Style::default().fg(Color::Red)),
            Span::styled(size_text, if is_marker {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }),
        ]));
    }

    // Mid divider
    let mid_label = match (best_bid, best_ask) {
        (Some(b), Some(a)) => format!(" ─── mid {:.4} ", (b + a) / 2.0),
        _ => " ───────────────".to_string(),
    };
    let pad = "─".repeat(area.width.saturating_sub(mid_label.len() as u16) as usize);
    lines.push(Line::from(Span::styled(
        format!("{}{}", mid_label, pad),
        Style::default().fg(Color::White),
    )));

    // Bids — best bid first
    for (i, (price, size)) in bids.iter().enumerate() {
        let is_best = i == 0;
        // Mark the bid at (or nearest to) our fixed entry price
        let is_marker = marker_price.map(|m| (m - *price).abs() < am.tick_size * 0.5).unwrap_or(false);
        let bar = size_bar(*size, max_size, bar_max);
        let style = if is_best {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        let size_text = if is_marker {
            format!(" {:.1} ★", size)
        } else {
            format!(" {:.1}", size)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" B {:.4} ", price), style),
            Span::styled(bar, Style::default().fg(Color::Green)),
            Span::styled(size_text, if is_marker {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }),
        ]));
    }

    let h = area.height as usize;
    if lines.len() > h { lines.truncate(h); }
    f.render_widget(Paragraph::new(lines), area);
}

fn size_bar(size: f64, max_size: f64, max_width: usize) -> String {
    const BLOCKS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    if max_size <= 0.0 || max_width == 0 { return " ".repeat(max_width); }
    let frac = (size / max_size).clamp(0.0, 1.0);
    let total_eighths = (frac * max_width as f64 * 8.0).round() as usize;
    let full = total_eighths / 8;
    let partial = total_eighths % 8;
    let mut s = String::with_capacity(max_width);
    for _ in 0..full.min(max_width) { s.push('█'); }
    if full < max_width {
        s.push(BLOCKS[partial]);
        for _ in (full + 1)..max_width { s.push(' '); }
    }
    s
}

fn render_price_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    symbol: &str,
    cur_price: Option<f64>,
    ref_price: Option<f64>,
    direction: &str,
    dir_style: Style,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", symbol.to_uppercase()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = vec![];

    // Current price
    match cur_price {
        Some(p) => lines.push(Line::from(Span::styled(
            format!(" Now: {:.2}", p),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        None => lines.push(Line::from(Span::styled(" Now: ...", Style::default().fg(Color::DarkGray)))),
    }

    // Reference price
    match ref_price {
        Some(p) => lines.push(Line::from(Span::styled(
            format!(" Ref: {:.2}", p),
            Style::default().fg(Color::Cyan),
        ))),
        None => lines.push(Line::from(Span::styled(" Ref: ...", Style::default().fg(Color::DarkGray)))),
    }

    // Delta / outcome
    lines.push(Line::from(Span::styled(
        " ─────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    if let (Some(cur), Some(ref_p)) = (cur_price, ref_price) {
        let delta = cur - ref_p;
        if delta.abs() < 0.005 {
            lines.push(Line::from(Span::styled(
                " Δ no change yet",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                " (Chainlink same)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let pct = (delta / ref_p) * 100.0;
            let (sym, col) = if delta >= 0.0 { ("+", Color::Green) } else { ("", Color::Red) };
            lines.push(Line::from(Span::styled(
                format!(" Δ {}{:.2}", sym, delta),
                Style::default().fg(col),
            )));
            lines.push(Line::from(Span::styled(
                format!("  ({}{:.3}%)", sym, pct),
                Style::default().fg(col),
            )));
            lines.push(Line::from(Span::styled(
                format!(" {}", direction),
                dir_style,
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " Awaiting data...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let height = inner.height as usize;
    if lines.len() > height {
        lines.truncate(height);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ── Generic token pair rendering (fallback when no ActiveMarket) ──────────────

/// Pair tokens: assumes alternating YES/NO or groups of 2. Returns Vec of
/// (label, Option<yes_id>, Option<no_id>).
fn pair_tokens(tokens: &[String]) -> Vec<(String, Option<String>, Option<String>)> {
    if tokens.is_empty() {
        return vec![];
    }
    let mut pairs = vec![];
    let mut i = 0;
    while i < tokens.len() {
        if i + 1 < tokens.len() {
            pairs.push((
                format!("Market {}", i / 2 + 1),
                Some(tokens[i].clone()),
                Some(tokens[i + 1].clone()),
            ));
            i += 2;
        } else {
            pairs.push((
                format!("Market {}", i / 2 + 1),
                Some(tokens[i].clone()),
                None,
            ));
            i += 1;
        }
    }
    pairs
}

fn render_token_pair_generic(
    f: &mut ratatui::Frame,
    area: Rect,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    pair: &(String, Option<String>, Option<String>),
) {
    let (label, yes_id, no_id) = pair;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", label));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match (yes_id, no_id) {
        (Some(yes), Some(no)) => {
            let halves = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);
            render_single_book(f, halves[0], ob, yes, "YES");
            render_single_book(f, halves[1], ob, no, "NO");
        }
        (Some(yes), None) => {
            render_single_book(f, inner, ob, yes, "YES");
        }
        _ => {}
    }
}

fn render_single_book(
    f: &mut ratatui::Frame,
    area: Rect,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    token_id: &str,
    label: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", label));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let asks = ob.top_asks(token_id, 5);
    let bids = ob.top_bids(token_id, 5);

    let mut lines: Vec<Line> = vec![];

    if asks.is_empty() && bids.is_empty() {
        lines.push(Line::from(Span::styled(
            " No data",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, (price, size)) in asks.iter().rev().enumerate() {
            let style = if i == asks.len().saturating_sub(1) {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Red)
            };
            lines.push(Line::from(Span::styled(
                format!(" A {:.4}  {:.2}", price, size),
                style,
            )));
        }

        lines.push(Line::from(Span::styled(
            " ─────────────────",
            Style::default().fg(Color::DarkGray),
        )));

        for (i, (price, size)) in bids.iter().enumerate() {
            let style = if i == 0 {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };
            lines.push(Line::from(Span::styled(
                format!(" B {:.4}  {:.2}", price, size),
                style,
            )));
        }
    }

    let height = inner.height as usize;
    if lines.len() > height {
        lines.truncate(height);
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

// ── Price helper ──────────────────────────────────────────────────────────────

fn round_tick(price: f64, tick: f64) -> f64 {
    let rounded = (price / tick).round() * tick;
    let decimals = tick.to_string()
        .split('.')
        .nth(1)
        .map(|d| d.len())
        .unwrap_or(2);
    let factor = 10f64.powi(decimals as i32);
    ((rounded * factor).round() / factor).clamp(0.01, 0.99)
}

// ══════════════════════════════════════════════════════════════════════════════
// Conviction TUI
// ══════════════════════════════════════════════════════════════════════════════

pub async fn run_conviction_tui(
    handle: TuiHandle,
    ob: crate::ws::orderbook::OrderbookWatcher,
    prices: crate::ws::price::PriceWatcher,
    config: crate::config::Config,
    wallet_address: String,
) {
    if let Err(e) = run_conviction_tui_inner(handle, ob, prices, config, wallet_address).await {
        eprintln!("Conviction TUI error: {}", e);
    }
}

async fn run_conviction_tui_inner(
    handle: TuiHandle,
    ob: crate::ws::orderbook::OrderbookWatcher,
    prices: crate::ws::price::PriceWatcher,
    config: crate::config::Config,
    wallet_address: String,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let quit = Arc::new(AtomicBool::new(false));
    let quit2 = quit.clone();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(event)) = events.next().await {
            if let Event::Key(key) = event {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') {
                    quit2.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    });

    loop {
        if quit.load(Ordering::SeqCst) { break; }

        let (phase, dry_run, log_lines, active_markets, open_positions, conviction_markets,
             conv_pnl, conv_trades, conv_wins, sim_balance, onchain_events,
             live_usdc, live_pol) = {
            let s = handle.state.read().await;
            // Sorted by asset name for stable ordering (max 4)
            let mut am: Vec<(String, ActiveMarket)> = s.active_markets
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            am.sort_by(|a, b| a.0.cmp(&b.0));
            am.truncate(4);
            (
                s.phase.clone(),
                s.dry_run,
                s.log_lines.iter().cloned().collect::<Vec<_>>(),
                am,
                s.conviction_open_positions.clone(),
                s.conviction_markets.clone(),
                s.conviction_total_pnl,
                s.conviction_trade_count,
                s.conviction_win_count,
                s.sim_balance,
                s.onchain_events.clone(),
                s.live_usdc_balance,
                s.live_pol_balance,
            )
        };

        terminal.draw(|f| {
            draw_conviction_ui(
                f, &phase, dry_run, &wallet_address, &config,
                &ob, &prices, &active_markets, &open_positions,
                &conviction_markets, conv_pnl, conv_trades, conv_wins,
                sim_balance, &log_lines, &onchain_events, live_usdc, live_pol,
            );
        })?;

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_conviction_ui(
    f: &mut ratatui::Frame,
    _phase: &str,
    dry_run: bool,
    wallet_address: &str,
    config: &crate::config::Config,
    ob: &crate::ws::orderbook::OrderbookWatcher,
    prices: &crate::ws::price::PriceWatcher,
    active_markets: &[(String, ActiveMarket)],
    open_positions: &HashMap<String, (Option<String>, Option<f64>)>,
    conviction_markets: &[ConvictionMarket],
    conv_pnl: f64,
    conv_trades: u32,
    conv_wins: u32,
    sim_balance: f64,
    log_lines: &[String],
    onchain_events: &[OnchainEvent],
    live_usdc: Option<f64>,
    live_pol: Option<f64>,
) {
    let size = f.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(3), // status
            Constraint::Min(10),   // main
        ])
        .split(size);

    // ── Header ────────────────────────────────────────────────────────────────
    let mode_str = if dry_run { "SIMULATION" } else { "LIVE" };
    let assets_str = config.assets.join(",").to_uppercase();
    let wallet_mode_str = match config.wallet_mode {
        crate::config::WalletMode::Eoa   => "EOA",
        crate::config::WalletMode::Proxy => "PROXY",
    };
    let header_text = format!(
        " Polymarket Conviction  |  {}  |  {}  |  {} {}",
        wallet_mode_str, mode_str, assets_str, config.duration,
    );
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, outer[0]);

    // ── Status: EOA | Proxy/Trading wallet | Balance ──────────────────────────
    let status_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(30), Constraint::Percentage(40)])
        .split(outer[1]);

    // [0] EOA signer address
    let eoa_short = addr_short(wallet_address);
    let eoa_w = Paragraph::new(format!(" EOA: {}", eoa_short))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(eoa_w, status_chunks[0]);

    // [1] Proxy wallet (if proxy mode) or "EOA Mode"
    let proxy_w = match &config.proxy_wallet {
        Some(p) => Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(" Proxy: ", Style::default().fg(Color::DarkGray)),
            Span::styled(addr_short(p), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ])),
        None => Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(" Trading: ", Style::default().fg(Color::DarkGray)),
            Span::styled(eoa_short, Style::default().fg(Color::Cyan)),
        ])),
    };
    let proxy_w = proxy_w.block(Block::default().borders(Borders::ALL));
    f.render_widget(proxy_w, status_chunks[1]);

    let wr = if conv_trades > 0 {
        format!("  WR {}/{}", conv_wins, conv_trades)
    } else {
        String::new()
    };
    let pnl_sym = if conv_pnl >= 0.0 { "+" } else { "" };
    let pnl_col = if conv_pnl >= 0.0 { Color::Green } else { Color::Red };

    let balance_widget = if dry_run {
        // Simulation mode: show tracked sim_balance + P&L
        let bal_col = if sim_balance >= 0.0 { Color::Cyan } else { Color::Red };
        Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(" Sim: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.2}", sim_balance), Style::default().fg(bal_col).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  P&L: {}{:.4}", pnl_sym, conv_pnl), Style::default().fg(pnl_col)),
            Span::styled(wr, Style::default().fg(Color::DarkGray)),
        ]))
        .block(Block::default().borders(Borders::ALL))
    } else {
        // Live mode: show actual USDC from RPC + POL gas
        let usdc_str = match live_usdc {
            Some(u) => format!("${:.4}", u),
            None    => "…".to_string(),
        };
        let pol_str = match live_pol {
            Some(p) => format!("{:.4} POL", p),
            None    => "…".to_string(),
        };
        let usdc_col = live_usdc.map(|u| if u > 0.0 { Color::Cyan } else { Color::Red }).unwrap_or(Color::DarkGray);
        Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(" USDC: ", Style::default().fg(Color::DarkGray)),
            Span::styled(usdc_str, Style::default().fg(usdc_col).add_modifier(Modifier::BOLD)),
            Span::styled("  Gas: ", Style::default().fg(Color::DarkGray)),
            Span::styled(pol_str, Style::default().fg(Color::Yellow)),
            Span::styled(wr, Style::default().fg(Color::DarkGray)),
        ]))
        .block(Block::default().borders(Borders::ALL))
    };
    f.render_widget(balance_widget, status_chunks[2]);

    // ── Main area: orderbook (left 60%) + trade history (right 40%) ───────────
    let main = outer[2];
    let log_height: u16 = (main.height / 3).max(5).min(12);
    let top_height = main.height.saturating_sub(log_height);

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_height), Constraint::Length(log_height)])
        .split(main);

    // Top: orderbook left + (trade history top + onchain activity bottom) right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(main_chunks[0]);

    // Split right panel: trade history (top 60%) + onchain activity (bottom 40%)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(top_chunks[1]);

    // Trade history (top)
    render_conviction_history(f, right_chunks[0], conviction_markets);

    // Onchain activity (bottom)
    render_onchain_events(f, right_chunks[1], onchain_events);

    // Orderbook panels — one per active market, stacked vertically (max 4)
    let ob_area = top_chunks[0];
    if active_markets.is_empty() {
        let waiting = Paragraph::new(" Waiting for market...")
            .block(Block::default().borders(Borders::ALL).title(" ORDERBOOK "));
        f.render_widget(waiting, ob_area);
    } else {
        let n = active_markets.len() as u16;
        let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n.into())).collect();
        let ob_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(ob_area);
        for (i, (asset, am)) in active_markets.iter().enumerate() {
            let pos_entry = open_positions.get(asset);
            let synthetic_pos = pos_entry.and_then(|(side, entry)| {
                entry.map(|e| SimPosition {
                    yes_price: e,
                    no_price: 0.0,
                    shares: 1.0,
                    yes_filled: false,
                    no_filled: false,
                    status: format!("{} Open", side.as_deref().unwrap_or("")),
                    pnl: None,
                })
            });
            render_active_market(f, ob_rows[i], ob, prices, am, synthetic_pos.as_ref());
        }
    }

    // ── Log ───────────────────────────────────────────────────────────────────
    let log_block = Block::default().borders(Borders::ALL).title(" LOG ");
    let inner = log_block.inner(main_chunks[1]);
    f.render_widget(log_block, main_chunks[1]);

    let vis = inner.height as usize;
    let start = log_lines.len().saturating_sub(vis);
    let items: Vec<ListItem> = log_lines[start..].iter().map(|line| {
        let style = if line.contains("[ERROR]") {
            Style::default().fg(Color::Red)
        } else if line.contains("[WARN ]") {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray)
        };
        ListItem::new(Line::from(Span::styled(line.as_str(), style)))
    }).collect();
    f.render_widget(List::new(items), inner);
}

// ── Conviction trade history panel ────────────────────────────────────────────

fn render_conviction_history(
    f: &mut ratatui::Frame,
    area: Rect,
    markets: &[ConvictionMarket],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(ratatui::text::Line::from(vec![
            Span::styled(" TRADE HISTORY ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = vec![];

    if markets.is_empty() {
        lines.push(Line::from(Span::styled(
            " No trades yet",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Render most-recent market first
    for market in markets.iter().rev() {
        let label = format!(
            " ─── {}-{}-{} ",
            market.asset.to_lowercase(),
            fmt_hhmm_short(market.start_ms),
            fmt_hhmm_short(market.end_time_ms),
        );
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        )));

        // Most-recent trade first
        for trade in market.trades.iter().rev() {
            // Signal line
            let (sig_col, sig_icon) = if trade.signal.contains("PANIC") {
                (Color::Red,   " 🚨 ")
            } else if trade.signal.contains("PRICE") {
                (Color::Green, " 💹 ")
            } else if trade.signal.contains("LIQUIDITY") {
                (Color::Blue,  " 📊 ")
            } else {
                (Color::Cyan,  " 📈 ")
            };
            lines.push(Line::from(vec![
                Span::styled(sig_icon, Style::default().fg(sig_col)),
                Span::styled(
                    trade.signal.clone(),
                    Style::default().fg(sig_col).add_modifier(Modifier::BOLD),
                ),
            ]));

            // Entry line
            lines.push(Line::from(Span::styled(
                format!(
                    "    Buy {} @${:.4} ×{:.2}s",
                    trade.side, trade.entry_price, trade.shares
                ),
                Style::default().fg(Color::White),
            )));

            // Status / P&L line
            match (&trade.status[..], trade.pnl) {
                ("Open", _) => lines.push(Line::from(Span::styled(
                    "    ⏳ Open",
                    Style::default().fg(Color::Yellow),
                ))),
                ("Resolving", _) => lines.push(Line::from(Span::styled(
                    "    ⏳ Resolving...",
                    Style::default().fg(Color::LightYellow),
                ))),
                ("WIN", Some(pnl)) => lines.push(Line::from(Span::styled(
                    format!("    ✅ WIN  P&L: +${:.4}", pnl),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ))),
                ("LOSS", Some(pnl)) => lines.push(Line::from(Span::styled(
                    format!("    ❌ LOSS P&L: ${:.4}", pnl),
                    Style::default().fg(Color::Red),
                ))),
                (s, Some(pnl)) => {
                    let col = if pnl >= 0.0 { Color::Green } else { Color::Red };
                    lines.push(Line::from(Span::styled(
                        format!("    {} P&L: {:+.4}", s, pnl),
                        Style::default().fg(col),
                    )));
                }
                _ => {}
            }

            lines.push(Line::from(Span::raw(""))); // blank separator
        }
    }

    let height = inner.height as usize;
    if lines.len() > height {
        // Show most recent (top) trades — already in correct order
        lines.truncate(height);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Shorten a 0x… address to `0x1234…5678` format.
fn addr_short(addr: &str) -> String {
    let a = addr.trim_start_matches("0x");
    if a.len() >= 10 {
        format!("0x{}…{}", &a[..6], &a[a.len() - 4..])
    } else {
        addr.to_string()
    }
}

fn fmt_hhmm(ms: i64) -> String {
    let secs = ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn fmt_hhmm_short(ms: i64) -> String {
    let secs = ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs % 3600) / 60;
    format!("{:02}:{:02}", h, m)
}

// ── Onchain activity panel ────────────────────────────────────────────────────

fn render_onchain_events(
    f: &mut ratatui::Frame,
    area: Rect,
    events: &[OnchainEvent],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(ratatui::text::Line::from(vec![
            Span::styled(" ONCHAIN ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = vec![];

    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            " No onchain activity",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for ev in events.iter().rev().take(inner.height as usize) {
            let ts = fmt_hhmm_short(ev.timestamp_ms);
            let line = Span::styled(
                format!(" {} {} {} {}", ts, ev.asset.to_uppercase(), ev.action, ev.detail),
                Style::default().fg(Color::DarkGray),
            );
            lines.push(Line::from(line));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}
