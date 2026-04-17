//! Polymarket CLOB API authentication helpers.
//!
//! Two auth layers:
//!   L1 — wallet-based (EIP-191 personal_sign). Used for API key derivation.
//!   L2 — HMAC-SHA256 with the API secret. Used for all regular API calls.
//!
//! Order signing uses EIP-712 against the CTF Exchange contract domain.

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;

use super::types::{SignedOrder, Side};
use crate::config::{Config, WalletMode};

// ── Contract addresses ────────────────────────────────────────────────────────

pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
pub const NEG_RISK_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

// ── L1 auth (EIP-712 ClobAuth — used for /auth/api-key) ──────────────────────

pub struct L1Headers {
    pub poly_address: String,
    pub poly_signature: String,
    pub poly_timestamp: String,
    pub poly_nonce: String,
}

const CLOB_AUTH_MSG: &str = "This message attests that I control the given wallet";

/// Build an EIP-712 ClobAuth signature, matching the SDK's `buildClobEip712Signature`.
///
/// Domain:  { name: "ClobAuthDomain", version: "1", chainId: 137 }
/// Types:   ClobAuth(address address, string timestamp, uint256 nonce, string message)
fn build_clob_eip712_signature(signer: &PrivateKeySigner, timestamp: u64, nonce: u64) -> Result<String> {
    // EIP-712 domain separator for ClobAuth
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId)"
    );
    let name_hash = keccak256(b"ClobAuthDomain");
    let version_hash = keccak256(b"1");
    let chain_id: [u8; 32] = U256::from(137u64).to_be_bytes();

    let mut domain_buf = Vec::with_capacity(4 * 32);
    domain_buf.extend_from_slice(domain_typehash.as_slice());
    domain_buf.extend_from_slice(name_hash.as_slice());
    domain_buf.extend_from_slice(version_hash.as_slice());
    domain_buf.extend_from_slice(&chain_id);
    let domain_sep = keccak256(&domain_buf);

    // ClobAuth struct hash
    let type_hash = keccak256(
        b"ClobAuth(address address,string timestamp,uint256 nonce,string message)"
    );
    let address = signer.address();
    let ts_str = timestamp.to_string();
    let ts_hash = keccak256(ts_str.as_bytes());
    let msg_hash = keccak256(CLOB_AUTH_MSG.as_bytes());
    let nonce_bytes: [u8; 32] = U256::from(nonce).to_be_bytes();

    let mut struct_buf = Vec::with_capacity(5 * 32);
    struct_buf.extend_from_slice(type_hash.as_slice());
    // address padded to 32 bytes
    struct_buf.extend_from_slice(&[0u8; 12]);
    struct_buf.extend_from_slice(address.as_slice());
    struct_buf.extend_from_slice(ts_hash.as_slice());
    struct_buf.extend_from_slice(&nonce_bytes);
    struct_buf.extend_from_slice(msg_hash.as_slice());
    let struct_hash = keccak256(&struct_buf);

    // EIP-712 signing hash: \x19\x01 || domainSep || structHash
    let mut hash_buf = Vec::with_capacity(2 + 32 + 32);
    hash_buf.extend_from_slice(b"\x19\x01");
    hash_buf.extend_from_slice(domain_sep.as_slice());
    hash_buf.extend_from_slice(struct_hash.as_slice());
    let signing_hash = keccak256(&hash_buf);

    let sig = signer.sign_hash_sync(&signing_hash)?;
    Ok(format!("0x{}", hex::encode(sig.as_bytes())))
}

/// Build L1 auth headers for API key derivation.
pub fn build_l1_headers(signer: &PrivateKeySigner) -> Result<L1Headers> {
    let timestamp = unix_ts();
    let nonce = 0u64;

    let sig = build_clob_eip712_signature(signer, timestamp, nonce)?;

    Ok(L1Headers {
        poly_address: format!("{}", signer.address()),
        poly_signature: sig,
        poly_timestamp: timestamp.to_string(),
        poly_nonce: nonce.to_string(),
    })
}

/// Auto-derive or create API credentials from the private key.
/// Tries `GET /auth/derive-api-key` first, falls back to `POST /auth/api-key`.
pub fn derive_or_create_api_key(
    signer: &PrivateKeySigner,
    host: &str,
) -> Result<super::types::ApiCreds> {
    let hdrs = build_l1_headers(signer)?;

    // Helper to set L1 headers on a ureq request
    let set_l1 = |req: ureq::Request| -> ureq::Request {
        req.set("POLY_ADDRESS", &hdrs.poly_address)
           .set("POLY_SIGNATURE", &hdrs.poly_signature)
           .set("POLY_TIMESTAMP", &hdrs.poly_timestamp)
           .set("POLY_NONCE", &hdrs.poly_nonce)
    };

    // Try derive first
    let derive_url = format!("{}/auth/derive-api-key", host);
    let resp = set_l1(ureq::get(&derive_url)).call();

    let (status, text) = match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => anyhow::bail!("Derive API key request failed: {}", e),
    };

    if status >= 200 && status < 300 {
        return parse_api_key_response(&text);
    }

    // Derive failed, try create
    tracing::info!("Derive failed ({}), trying create...", status);
    let hdrs2 = build_l1_headers(signer)?;  // fresh timestamp
    let create_url = format!("{}/auth/api-key", host);
    let resp2 = set_l1(ureq::post(&create_url))
        .set("POLY_ADDRESS", &hdrs2.poly_address)
        .set("POLY_SIGNATURE", &hdrs2.poly_signature)
        .set("POLY_TIMESTAMP", &hdrs2.poly_timestamp)
        .set("POLY_NONCE", &hdrs2.poly_nonce)
        .call();

    let (status2, text2) = match resp2 {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => anyhow::bail!("Create API key request failed: {}", e),
    };

    if status2 >= 200 && status2 < 300 {
        return parse_api_key_response(&text2);
    }

    anyhow::bail!("Failed to derive or create API key: {} / {}", text, text2)
}

fn parse_api_key_response(text: &str) -> Result<super::types::ApiCreds> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Failed to parse API key response: {} — {}", e, text))?;

    Ok(super::types::ApiCreds {
        key: v["apiKey"].as_str().unwrap_or_default().to_string(),
        secret: v["secret"].as_str().unwrap_or_default().to_string(),
        passphrase: v["passphrase"].as_str().unwrap_or_default().to_string(),
    })
}

// ── L2 auth (HMAC-SHA256 — used for all authenticated API calls) ──────────────

pub struct L2Headers {
    pub poly_address: String,      // signer wallet address
    pub poly_signature: String,    // HMAC-SHA256 of the message (URL-safe base64)
    pub poly_timestamp: String,
    pub poly_api_key: String,      // API key
    pub poly_passphrase: String,   // API passphrase
}

/// Build L2 auth headers using API key/secret/passphrase (HMAC-SHA256).
///
/// Matches the official Polymarket CLOB client SDK header format:
///   POLY_ADDRESS    = signer wallet address
///   POLY_API_KEY    = API key
///   POLY_PASSPHRASE = API passphrase
///   POLY_SIGNATURE  = HMAC-SHA256 (URL-safe base64)
///   POLY_TIMESTAMP  = Unix timestamp (seconds)
pub fn build_l2_headers(
    signer_address: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    method: &str,
    path: &str,
    body: &str,
) -> Result<L2Headers> {
    let timestamp = unix_ts();

    // Message format: timestamp + method + path + body
    // Note: the official SDK does NOT uppercase the method in the HMAC message
    let mut msg = format!("{}{}{}", timestamp, method, path);
    if !body.is_empty() {
        msg.push_str(body);
    }

    // Decode base64 secret (handle both standard and URL-safe base64)
    let sanitized_secret = api_secret.replace('-', "+").replace('_', "/");
    let key_bytes = B64.decode(&sanitized_secret).unwrap_or_else(|_| api_secret.as_bytes().to_vec());

    // Compute HMAC-SHA256
    let mut mac = Hmac::<Sha256>::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("HMAC key error: {}", e))?;
    mac.update(msg.as_bytes());
    let result = mac.finalize().into_bytes();

    // Encode as URL-safe base64 ('+' -> '-', '/' -> '_'), keeping '=' padding
    let sig = B64.encode(result).replace('+', "-").replace('/', "_");

    Ok(L2Headers {
        poly_address: signer_address.to_string(),
        poly_signature: sig,
        poly_timestamp: timestamp.to_string(),
        poly_api_key: api_key.to_string(),
        poly_passphrase: api_passphrase.to_string(),
    })
}

// ── EIP-712 order signing ─────────────────────────────────────────────────────

/// EIP-712 type string for a Polymarket order.
const ORDER_TYPE_STR: &str = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

fn order_typehash() -> [u8; 32] {
    *keccak256(ORDER_TYPE_STR.as_bytes())
}

fn eip712_domain_separator(exchange_addr: Address, neg_risk: bool) -> [u8; 32] {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name = keccak256(b"Polymarket CTF Exchange");
    let version = keccak256(b"1");
    let chain_id = U256::from(137u64);

    // ABI-encode the domain struct
    let mut buf = Vec::with_capacity(5 * 32);
    buf.extend_from_slice(domain_typehash.as_slice());
    buf.extend_from_slice(name.as_slice());
    buf.extend_from_slice(version.as_slice());
    let chain_bytes: [u8; 32] = chain_id.to_be_bytes();
    buf.extend_from_slice(&chain_bytes);
    // address padded to 32 bytes (left-padded with zeros)
    buf.extend_from_slice(&[0u8; 12]);
    buf.extend_from_slice(exchange_addr.as_slice());

    *keccak256(&buf)
}

struct RawOrder {
    salt: U256,
    maker: Address,
    signer: Address,
    taker: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
}

fn u256_to_bytes(v: U256) -> [u8; 32] { v.to_be_bytes() }
fn addr_to_bytes(a: Address) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(a.as_slice());
    b
}
fn u8_to_bytes(v: u8) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[31] = v;
    b
}

fn hash_order_struct(o: &RawOrder) -> [u8; 32] {
    let type_hash = order_typehash();
    let mut buf = Vec::with_capacity(13 * 32);

    buf.extend_from_slice(&type_hash);
    buf.extend_from_slice(&u256_to_bytes(o.salt));
    buf.extend_from_slice(&addr_to_bytes(o.maker));
    buf.extend_from_slice(&addr_to_bytes(o.signer));
    buf.extend_from_slice(&addr_to_bytes(o.taker));
    buf.extend_from_slice(&u256_to_bytes(o.token_id));
    buf.extend_from_slice(&u256_to_bytes(o.maker_amount));
    buf.extend_from_slice(&u256_to_bytes(o.taker_amount));
    buf.extend_from_slice(&u256_to_bytes(o.expiration));
    buf.extend_from_slice(&u256_to_bytes(o.nonce));
    buf.extend_from_slice(&u256_to_bytes(o.fee_rate_bps));
    buf.extend_from_slice(&u8_to_bytes(o.side));
    buf.extend_from_slice(&u8_to_bytes(o.signature_type));

    *keccak256(&buf)
}

fn eip712_signing_hash(order: &RawOrder, exchange: Address, neg_risk: bool) -> B256 {
    let domain_sep = eip712_domain_separator(exchange, neg_risk);
    let order_hash = hash_order_struct(order);

    let mut buf = Vec::with_capacity(2 + 32 + 32);
    buf.extend_from_slice(b"\x19\x01");
    buf.extend_from_slice(&domain_sep);
    buf.extend_from_slice(&order_hash);

    keccak256(&buf)
}

/// Build and sign a GTC limit BUY order for the given token.
///
/// In EOA mode:  maker = signer = EOA address, signatureType = 0
/// In PROXY mode: maker = proxyWallet, signer = EOA, signatureType = 2
pub fn build_signed_order(
    signer: &PrivateKeySigner,
    config: &Config,
    token_id_str: &str,
    side: Side,
    price: f64,
    shares: f64,
    tick_size: f64,
    neg_risk: bool,
    fee_rate_bps: u64,
) -> Result<SignedOrder> {
    let eoa = signer.address();

    let (maker, signature_type): (Address, u8) = match &config.wallet_mode {
        WalletMode::Eoa => (eoa, 0),
        WalletMode::Proxy => {
            let proxy = config
                .proxy_wallet
                .as_deref()
                .unwrap_or("")
                .parse::<Address>()
                .map_err(|e| anyhow::anyhow!("Invalid proxy wallet address: {}", e))?;
            (proxy, 2)
        }
    };

    let taker = Address::ZERO;

    // Amounts in USDC (6 decimals) and shares (6 decimals — CTF tokens use 6 decimals)
    let (maker_amount, taker_amount) = match side {
        Side::Buy => {
            // Paying USDC to receive conditional tokens
            let usdc = (price * shares * 1_000_000.0).round() as u64;
            let tokens = (shares * 1_000_000.0).round() as u64;
            (usdc, tokens)
        }
        Side::Sell => {
            // Offering conditional tokens to receive USDC
            let tokens = (shares * 1_000_000.0).round() as u64;
            let usdc = (price * shares * 1_000_000.0).round() as u64;
            (tokens, usdc)
        }
    };

    let side_num: u8 = match side {
        Side::Buy => 0,
        Side::Sell => 1,
    };
    let side_str = match side {
        Side::Buy => "BUY".to_string(),
        Side::Sell => "SELL".to_string(),
    };

    // Parse tokenId (it's a decimal string representing a large uint256)
    let token_id = U256::from_str_radix(token_id_str, 10)
        .map_err(|_| anyhow::anyhow!("Invalid tokenId: {}", token_id_str))?;

    // Mimic SDK salt: Math.round(Math.random() * Date.now())
    // Must fit in JavaScript's Number.MAX_SAFE_INTEGER (2^53 - 1)
    // to avoid precision loss when the server does parseInt(salt, 10)
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let rnd: f64 = rand::thread_rng().gen::<f64>();  // 0.0 .. 1.0
    let salt_u64: u64 = (rnd * now_ms as f64).round() as u64;
    let salt: U256 = U256::from(salt_u64);

    let raw = RawOrder {
        salt,
        maker,
        signer: eoa,
        taker,
        token_id,
        maker_amount: U256::from(maker_amount),
        taker_amount: U256::from(taker_amount),
        expiration: U256::ZERO, // GTC
        nonce: U256::ZERO,
        fee_rate_bps: U256::from(fee_rate_bps),
        side: side_num,
        signature_type,
    };

    let exchange_addr: Address = if neg_risk {
        NEG_RISK_EXCHANGE.parse().unwrap()
    } else {
        CTF_EXCHANGE.parse().unwrap()
    };

    let hash = eip712_signing_hash(&raw, exchange_addr, neg_risk);
    // Sign the hash directly (no EIP-191 prefix — EIP-712 hash already has \x19\x01)
    let sig = signer.sign_hash_sync(&hash)?;

    Ok(SignedOrder {
        salt: salt_u64,
        maker: format!("{}", maker),
        signer: format!("{}", eoa),
        taker: format!("{}", taker),
        token_id: token_id_str.to_string(),
        maker_amount: maker_amount.to_string(),
        taker_amount: taker_amount.to_string(),
        expiration: "0".to_string(),
        nonce: "0".to_string(),
        fee_rate_bps: fee_rate_bps.to_string(),
        side: side_str,
        signature_type,
        signature: format!("0x{}", hex::encode(sig.as_bytes())),
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
