/// A Polymarket binary market ready for the maker rebate strategy.
#[derive(Debug, Clone)]
pub struct Market {
    pub asset: String,
    pub condition_id: String,
    pub question: String,
    pub end_time: chrono::DateTime<chrono::Utc>,
    pub event_start_time: chrono::DateTime<chrono::Utc>,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub neg_risk: bool,
    pub tick_size: f64,
    /// Unix timestamp (seconds) from the slug — the Chainlink price at this exact
    /// second is the reference price that determines YES/NO outcome.
    pub resolution_ts: i64,
    /// True when this market was already in progress when we detected it.
    pub is_current: bool,
}

impl Market {
    /// Chainlink symbol for this market's asset (e.g. "btc" → "btc/usd").
    pub fn chainlink_symbol(&self) -> String {
        format!("{}/usd", self.asset.to_lowercase())
    }
}
