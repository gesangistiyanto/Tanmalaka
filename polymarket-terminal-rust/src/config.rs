use anyhow::{bail, Context, Result};
use std::env;

use chrono::{FixedOffset, Timelike, Utc};

#[derive(Debug, Clone, PartialEq)]
pub enum WalletMode {
    Eoa,
    Proxy,
}

#[derive(Debug, Clone)]
pub struct Config {
    // Wallet
    pub private_key: String,
    pub proxy_wallet: Option<String>,
    pub wallet_mode: WalletMode,

    // API credentials (optional — auto-derived via L1 auth if absent)
    pub clob_api_key: Option<String>,
    pub clob_api_secret: Option<String>,
    pub clob_api_passphrase: Option<String>,

    // Endpoints
    pub clob_host: String,
    pub gamma_host: String,
    pub polygon_rpc_url: String,

    // Maker MM settings
    pub assets: Vec<String>,      // e.g. ["btc", "eth"]
    pub duration: String,         // "5m" or "15m"
    pub trade_size: f64,          // shares per side (maker-MM)
    pub conviction_trade_usd: f64, // USDC budget per conviction trade
    pub max_combined: f64,        // max (YES_bid + NO_bid) to enter
    pub cut_loss_time: u64,       // seconds before close to force-exit
    pub entry_window: u64,        // max seconds after open to enter
    pub poll_interval_ms: u64,    // market detector poll interval (ms)
    pub reentry_delay_ms: u64,    // delay between re-entry cycles (ms)
    pub reentry_enabled: bool,
    pub min_price: f64,           // min bid to qualify for entry
    pub max_price: f64,           // max bid to qualify for entry
    pub cancel_cheap_on_exp_fill: bool,
    pub poll_ms: u64,            // milliseconds between orderbook checks during entry wait
    pub sim_initial_balance: f64, // starting USDC balance for simulation tracking

    pub dry_run: bool,

    /// UTC time windows when conviction strategy is paused.
    /// Each entry is (start_min, end_min) in minutes from midnight UTC.
    pub conviction_stop_windows: Vec<(u32, u32)>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let private_key = env::var("PRIVATE_KEY").context("PRIVATE_KEY not set")?;

        let proxy_wallet = env::var("PROXY_WALLET_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty());

        let wallet_mode = match env::var("WALLET_MODE").ok().as_deref() {
            Some("eoa") => WalletMode::Eoa,
            Some("proxy") => WalletMode::Proxy,
            _ => {
                if proxy_wallet.is_some() {
                    WalletMode::Proxy
                } else {
                    WalletMode::Eoa
                }
            }
        };

        if wallet_mode == WalletMode::Proxy && proxy_wallet.is_none() {
            bail!("WALLET_MODE=proxy but PROXY_WALLET_ADDRESS is not set");
        }

        let assets: Vec<String> = env::var("MAKER_MM_ASSETS")
            .unwrap_or_else(|_| "btc".to_string())
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        if assets.is_empty() {
            bail!("MAKER_MM_ASSETS must not be empty");
        }

        let trade_size: f64 = env::var("MAKER_MM_TRADE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0);

        if trade_size < 5.0 {
            bail!("MAKER_MM_TRADE_SIZE must be >= 5 (CLOB minimum order size)");
        }

        let max_combined: f64 = env::var("MAKER_MM_MAX_COMBINED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.98);

        if max_combined <= 0.0 || max_combined >= 1.0 {
            bail!("MAKER_MM_MAX_COMBINED must be between 0 and 1 exclusive");
        }

        Ok(Config {
            private_key,
            proxy_wallet,
            wallet_mode,
            clob_api_key: env::var("CLOB_API_KEY").ok().filter(|s| !s.is_empty()),
            clob_api_secret: env::var("CLOB_API_SECRET").ok().filter(|s| !s.is_empty()),
            clob_api_passphrase: env::var("CLOB_API_PASSPHRASE")
                .ok()
                .filter(|s| !s.is_empty()),
            clob_host: env::var("CLOB_HOST")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            gamma_host: env::var("GAMMA_HOST")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".to_string()),
            polygon_rpc_url: env::var("POLYGON_RPC_URL")
                .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".to_string()),
            assets,
            duration: env::var("MAKER_MM_DURATION").unwrap_or_else(|_| "15m".to_string()),
            trade_size,
            conviction_trade_usd: env_f64("CONVICTION_TRADE_USD", 5.0),
            max_combined,
            cut_loss_time: env_u64("MAKER_MM_CUT_LOSS_TIME", 60),
            entry_window: env_u64("MAKER_MM_ENTRY_WINDOW", 45),
            poll_interval_ms: env_u64("MAKER_MM_POLL_INTERVAL", 5) * 1000,
            reentry_delay_ms: env_u64("MAKER_MM_REENTRY_DELAY", 30) * 1000,
            reentry_enabled: env::var("MAKER_MM_REENTRY_ENABLED")
                .map(|s| s != "false")
                .unwrap_or(true),
            min_price: env_f64("MAKER_MM_MIN_PRICE", 0.30),
            max_price: env_f64("MAKER_MM_MAX_PRICE", 0.69),
            cancel_cheap_on_exp_fill: env::var("MAKER_MM_CANCEL_CHEAP_ON_EXP_FILL")
                .map(|s| s == "true")
                .unwrap_or(false),
            poll_ms: env_u64("MAKER_MM_POLL_MS", 1000),
            sim_initial_balance: env_f64("SIM_INITIAL_BALANCE", 100.0),
            dry_run: env::var("DRY_RUN")
                .map(|s| s == "true")
                .unwrap_or(false),
            conviction_stop_windows: parse_stop_windows(
                &env::var("CONVICTION_STOP_WINDOWS").unwrap_or_default()
            ),
        })
    }

    /// Returns the wallet address that holds funds and CTF tokens.
    pub fn wallet_address(&self, eoa: &str) -> String {
        match self.wallet_mode {
            WalletMode::Eoa => eoa.to_string(),
            WalletMode::Proxy => self.proxy_wallet.clone().unwrap_or_else(|| eoa.to_string()),
        }
    }

    /// Slot duration in seconds based on the configured duration.
    pub fn slot_secs(&self) -> u64 {
        match self.duration.as_str() {
            "5m" => 300,
            "15m" => 900,
            other => {
                tracing::warn!("Unknown duration '{}', defaulting to 900s", other);
                900
            }
        }
    }

    /// Returns true when the current UTC+7 time falls inside any conviction stop window.
    pub fn is_in_stop_window(&self) -> bool {
        if self.conviction_stop_windows.is_empty() {
            return false;
        }
        let wib = FixedOffset::east_opt(7 * 3600).unwrap();
        let now = Utc::now().with_timezone(&wib);
        let current_min = now.hour() * 60 + now.minute();
        self.conviction_stop_windows.iter().any(|&(start, end)| {
            if start <= end {
                current_min >= start && current_min < end
            } else {
                // window crosses midnight (e.g. 23:00–01:00)
                current_min >= start || current_min < end
            }
        })
    }
}

/// Parse "HH:MM-HH:MM,HH:MM-HH:MM" into Vec<(start_min, end_min)>.
fn parse_stop_windows(s: &str) -> Vec<(u32, u32)> {
    s.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() { return None; }
            let (a, b) = part.split_once('-')?;
            let parse_hhmm = |t: &str| -> Option<u32> {
                let (h, m) = t.trim().split_once(':')?;
                let h: u32 = h.parse().ok()?;
                let m: u32 = m.parse().ok()?;
                if h > 23 || m > 59 { return None; }
                Some(h * 60 + m)
            };
            Some((parse_hhmm(a)?, parse_hhmm(b)?))
        })
        .collect()
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
