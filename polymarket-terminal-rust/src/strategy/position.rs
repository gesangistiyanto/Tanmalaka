/// State of one side (YES or NO) of an active position.
#[derive(Debug, Clone)]
pub struct SideState {
    pub token_id: String,
    pub buy_price: f64,
    pub cost: f64,
    pub order_id: String,
    pub filled: bool,
    pub baseline: f64, // on-chain balance at order placement (for net-balance calc)
    pub clob_filled: bool,
    pub last_clob_check_ms: u64,
}

impl SideState {
    pub fn new(token_id: &str, buy_price: f64, shares: f64, order_id: &str, baseline: f64) -> Self {
        Self {
            token_id: token_id.to_string(),
            buy_price,
            cost: buy_price * shares,
            order_id: order_id.to_string(),
            filled: false,
            baseline,
            clob_filled: false,
            last_clob_check_ms: 0,
        }
    }
}

/// Status of the overall position.
#[derive(Debug, Clone, PartialEq)]
pub enum PositionStatus {
    Monitoring,
    Holding { side: String },
    Done,
}

/// Full state of one maker rebate position (both YES and NO sides).
#[derive(Debug, Clone)]
pub struct Position {
    pub asset: String,
    pub condition_id: String,
    pub question: String,
    pub end_time_ms: i64,
    pub market_open_time_ms: i64,
    pub tick_size: f64,
    pub neg_risk: bool,
    pub target_shares: f64,
    pub yes: SideState,
    pub no: SideState,
    pub status: PositionStatus,
    pub total_profit: f64,
    pub one_sided: bool,

    // Ghost fill detection
    pub ghost_fill_since_ms: Option<i64>,
    pub both_filled_since_ms: Option<i64>,
    pub first_fill_time_ms: Option<i64>,
    pub merge_fail_count: u32,
}

impl Position {
    pub fn ms_remaining(&self) -> i64 {
        self.end_time_ms - now_ms()
    }

    pub fn is_done(&self) -> bool {
        self.status == PositionStatus::Done
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
