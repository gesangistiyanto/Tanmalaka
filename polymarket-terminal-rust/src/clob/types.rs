use serde::{Deserialize, Serialize};

// ── API credentials ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ApiCreds {
    /// Polymarket returns "apiKey" from /auth/api-key endpoint.
    #[serde(rename = "apiKey", alias = "key")]
    pub key: String,
    pub secret: String,
    pub passphrase: String,
}

// ── Order request ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Gtc,
    Fak,
}

/// Signed order struct sent to POST /order
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOrder {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub signature_type: u8,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRequest {
    // Field order MUST match the SDK's orderToJson() output — the HMAC is
    // computed over the raw JSON string so field ordering matters.
    #[serde(rename = "deferExec")]
    pub defer_exec: bool,
    pub order: SignedOrder,
    pub owner: String,
    pub order_type: String,
}

// ── Order response ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderResponse {
    pub success: bool,
    #[serde(default)]
    pub error_msg: String,
    #[serde(rename = "orderID", default)]
    pub order_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub taking_amount: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderStatus {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub size_matched: String,
}

// ── Cancel response ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CancelResponse {
    pub not_canceled: Option<Vec<serde_json::Value>>,
    pub canceled: Option<Vec<serde_json::Value>>,
}

// ── Open orders ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrder {
    #[serde(alias = "order_id")]
    pub id: String,
    pub asset_id: Option<String>,
    pub status: Option<String>,
}

// ── Price ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PriceResponse {
    pub price: Option<String>,
}
