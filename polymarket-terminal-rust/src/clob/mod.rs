//! Polymarket CLOB REST client — orders, cancellations, open order queries.

pub mod auth;
pub mod types;

use anyhow::{bail, Context, Result};
use alloy::signers::local::PrivateKeySigner;
use reqwest::{Client, Method};
use tracing::{debug, warn};

use auth::{build_l1_headers, build_l2_headers, build_signed_order};
use types::*;
use crate::config::Config;

pub struct ClobClient {
    http: Client,
    base: String,
    signer: PrivateKeySigner,
    pub(crate) config: Config,
    pub eoa_address: String,
    pub wallet_address: String,
    creds: Option<ApiCreds>,
}

impl ClobClient {
    /// Create a new client. Call `init()` to derive / load API credentials.
    pub fn new(signer: PrivateKeySigner, config: &Config) -> Self {
        let eoa = format!("{}", signer.address());
        let wallet = config.wallet_address(&eoa);
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap(),
            base: config.clob_host.clone(),
            eoa_address: eoa,
            wallet_address: wallet,
            signer,
            config: config.clone(),
            creds: None,
        }
    }

    /// Load credentials from config or derive them via L1 auth.
    pub async fn init(&mut self) -> Result<()> {
        if let (Some(k), Some(s), Some(p)) = (
            &self.config.clob_api_key,
            &self.config.clob_api_secret,
            &self.config.clob_api_passphrase,
        ) {
            self.creds = Some(ApiCreds {
                key: k.clone(),
                secret: s.clone(),
                passphrase: p.clone(),
            });
            tracing::info!("CLOB: using API credentials from env");
        } else {
            tracing::info!("CLOB: deriving API credentials via L1 EIP-712 auth...");
            let creds = auth::derive_or_create_api_key(&self.signer, &self.base)?;
            tracing::info!(
                "CLOB: API credentials derived — key={}… passphrase_len={}",
                &creds.key[..creds.key.len().min(8)],
                creds.passphrase.len(),
            );
            self.creds = Some(creds);
        }
        Ok(())
    }

    // ── Authenticated request builder ─────────────────────────────────────────

    fn creds(&self) -> Result<&ApiCreds> {
        self.creds.as_ref().ok_or_else(|| anyhow::anyhow!("CLOB client not initialized"))
    }

    /// Authenticated request using ureq (preserves header casing).
    /// reqwest lowercases all header names (POLY_ADDRESS → poly_address)
    /// which causes 401 on Polymarket's API that requires uppercase headers.
    fn authenticated_request_sync(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String)> {
        let creds = self.creds()?;
        let body_str = body.unwrap_or("");

        let hdrs = build_l2_headers(
            &self.eoa_address,
            &creds.key,
            &creds.secret,
            &creds.passphrase,
            method,
            path,
            body_str,
        )?;

        let url = format!("{}{}", self.base, path);
        let req = match method {
            "GET" => ureq::get(&url),
            "POST" => ureq::post(&url),
            "DELETE" => ureq::delete(&url),
            _ => bail!("Unsupported method: {}", method),
        };

        let req = req
            .set("POLY_ADDRESS", &hdrs.poly_address)
            .set("POLY_SIGNATURE", &hdrs.poly_signature)
            .set("POLY_TIMESTAMP", &hdrs.poly_timestamp)
            .set("POLY_API_KEY", &hdrs.poly_api_key)
            .set("POLY_PASSPHRASE", &hdrs.poly_passphrase);

        let response = if let Some(b) = body {
            req.set("Content-Type", "application/json")
               .send_string(b)
        } else {
            req.call()
        };

        match response {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.into_string().unwrap_or_default();
                Ok((status, text))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                Ok((code, text))
            }
            Err(e) => bail!("HTTP request failed: {}", e),
        }
    }

    /// Fetch the market's required fee rate in basis points.
    /// Matches SDK's _resolveFeeRateBps: GET /fee-rate?token_id=...
    fn get_fee_rate(&self, token_id: &str) -> u64 {
        let url = format!("{}/fee-rate?token_id={}", self.base, token_id);
        match ureq::get(&url).call() {
            Ok(resp) => {
                if let Ok(text) = resp.into_string() {
                    // Response is {"base_fee": 1000} or similar
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        return v["base_fee"].as_u64().unwrap_or(0);
                    }
                }
                0
            }
            Err(_) => 0,
        }
    }

    // ── Order operations ──────────────────────────────────────────────────────

    /// Place a GTC limit BUY order. Returns order ID on success.
    pub async fn place_limit_buy(
        &self,
        token_id: &str,
        shares: f64,
        price: f64,
        tick_size: f64,
        neg_risk: bool,
    ) -> Result<PlaceOrderResponse> {
        if self.config.dry_run {
            let fake_id = format!("sim-{}", auth::unix_ts());
            return Ok(PlaceOrderResponse {
                success: true,
                error_msg: String::new(),
                order_id: fake_id,
                status: "live".to_string(),
                taking_amount: "0".to_string(),
            });
        }

        // Fetch market fee rate (like SDK's _resolveFeeRateBps)
        let fee_rate_bps = self.get_fee_rate(token_id);

        let signed = build_signed_order(
            &self.signer,
            &self.config,
            token_id,
            Side::Buy,
            price,
            shares,
            tick_size,
            neg_risk,
            fee_rate_bps,
        )?;

        let creds = self.creds()?;
        let req = PlaceOrderRequest {
            order: signed,
            owner: creds.key.clone(),  // Must be the API key, not wallet address
            order_type: "GTC".to_string(),
            defer_exec: false,
        };

        let body = serde_json::to_string(&req)?;
        debug!("Placing order: {}", body);

        let (status, text) = self.authenticated_request_sync("POST", "/order", Some(&body))?;

        if status < 200 || status >= 300 {
            bail!("Place order failed ({} {}): {}", status, 
                  if status == 401 { "Unauthorized" } else { "Bad Request" }, text);
        }

        serde_json::from_str::<PlaceOrderResponse>(&text)
            .context("Failed to parse place order response")
    }

    /// Place a FOK (Fill-Or-Kill) market BUY order. Fills immediately at best
    /// available price up to `price`, or cancels entirely. No minimum shares.
    pub async fn place_market_buy(
        &self,
        token_id: &str,
        shares: f64,
        price: f64,
        tick_size: f64,
        neg_risk: bool,
    ) -> Result<PlaceOrderResponse> {
        if self.config.dry_run {
            let fake_id = format!("sim-{}", auth::unix_ts());
            return Ok(PlaceOrderResponse {
                success: true,
                error_msg: String::new(),
                order_id: fake_id,
                status: "matched".to_string(),
                taking_amount: "0".to_string(),
            });
        }

        let fee_rate_bps = self.get_fee_rate(token_id);

        let signed = build_signed_order(
            &self.signer,
            &self.config,
            token_id,
            Side::Buy,
            price,
            shares,
            tick_size,
            neg_risk,
            fee_rate_bps,
        )?;

        let creds = self.creds()?;
        let req = PlaceOrderRequest {
            order: signed,
            owner: creds.key.clone(),
            order_type: "FOK".to_string(),
            defer_exec: false,
        };

        let body = serde_json::to_string(&req)?;
        debug!("Placing FOK order: {}", body);

        let (status, text) = self.authenticated_request_sync("POST", "/order", Some(&body))?;

        if status < 200 || status >= 300 {
            bail!("Place FOK order failed ({} {}): {}", status,
                  if status == 401 { "Unauthorized" } else { "Bad Request" }, text);
        }

        serde_json::from_str::<PlaceOrderResponse>(&text)
            .context("Failed to parse place order response")
    }

    /// Cancel a specific order by ID. Returns true if cancelled.
    pub async fn cancel_order(&self, order_id: &str) -> bool {
        if self.config.dry_run || order_id.is_empty() || order_id.starts_with("sim-") {
            return true;
        }

        let cancel_body = serde_json::json!({"orderID": order_id}).to_string();
        match self.authenticated_request_sync("DELETE", "/order", Some(&cancel_body)) {
            Ok((status, _)) => {
                if status >= 200 && status < 300 {
                    debug!("Cancelled order {}", &order_id[..order_id.len().min(16)]);
                    true
                } else {
                    warn!("Cancel order {} failed: {}", &order_id[..order_id.len().min(16)], status);
                    false
                }
            }
            Err(e) => {
                warn!("Cancel order error: {}", e);
                false
            }
        }
    }

    /// Get order status. Returns "open", "filled", "partial", "cancelled", or "unknown".
    pub async fn order_status(&self, order_id: &str) -> &'static str {
        if order_id.is_empty() || order_id.starts_with("sim-") || order_id.starts_with("filled-") {
            return "unknown";
        }

        let path = format!("/order/{}", order_id);
        match self.authenticated_request_sync("GET", &path, None) {
            Ok((status, text)) if status >= 200 && status < 300 => {
                match serde_json::from_str::<OrderStatus>(&text) {
                    Ok(o) => match o.status.as_str() {
                        "FILLED" | "FILLED_FULLY" => "filled",
                        "PARTIAL_FILLED" | "FILLED_PARTIALLY" => "partial",
                        "CANCELLED" | "CANCELLED_BY_USER" | "EXPIRED" => "cancelled",
                        "OPEN" => "open",
                        _ => "unknown",
                    },
                    Err(_) => "unknown",
                }
            }
            _ => "unknown",
        }
    }

    /// Get open orders, optionally filtered by asset_id (token ID).
    pub async fn open_orders(&self, asset_id: Option<&str>) -> Vec<OpenOrder> {
        if self.config.dry_run {
            return vec![];
        }

        let path = match asset_id {
            Some(id) => format!("/orders?asset_id={}", id),
            None => "/orders".to_string(),
        };

        match self.authenticated_request_sync("GET", &path, None) {
            Ok((status, text)) if status >= 200 && status < 300 => {
                serde_json::from_str::<Vec<OpenOrder>>(&text).unwrap_or_default()
            }
            _ => vec![],
        }
    }
}
