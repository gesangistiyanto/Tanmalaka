/// HMAC comparison test — same inputs as the Node.js script
/// Run: cargo run --bin hmac_test

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn main() {
    let secret = "MJAYx_Fs2btNSHfwrnDr5OqVoYfxnDx9qffazn5LLU8=";
    let ts = "1712345678";

    // Test 1: GET /api-keys (no body)
    {
        let msg = format!("{}GET/api-keys", ts);
        let sig = compute_hmac(secret, &msg);
        println!("=== TEST 1: GET /api-keys (no body) ===");
        println!("message: {:?}", msg);
        println!("secretBytes: {}", hex::encode(decode_secret(secret)));
        println!("signature: {}", sig);
    }

    // Test 2: POST with body
    {
        let body = r#"{"deferExec":false,"order":{"salt":123456789},"owner":"test","orderType":"GTC"}"#;
        let msg = format!("{}POST/order{}", ts, body);
        let sig = compute_hmac(secret, &msg);
        println!("\n=== TEST 2: POST /order (with body) ===");
        println!("message: {:?}", msg);
        println!("secretBytes: {}", hex::encode(decode_secret(secret)));
        println!("signature: {}", sig);
    }

    // Test 3: empty body
    {
        let msg = format!("{}POST/order", ts);
        let sig = compute_hmac(secret, &msg);
        println!("\n=== TEST 3: POST /order (empty string body) ===");
        println!("message: {:?}", msg);
        println!("signature: {}", sig);
    }
}

fn decode_secret(secret: &str) -> Vec<u8> {
    let sanitized = secret.replace('-', "+").replace('_', "/");
    B64.decode(&sanitized).unwrap_or_else(|_| secret.as_bytes().to_vec())
}

fn compute_hmac(secret: &str, message: &str) -> String {
    let key_bytes = decode_secret(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&key_bytes).unwrap();
    mac.update(message.as_bytes());
    let result = mac.finalize().into_bytes();
    let sig = B64.encode(result).replace('+', "-").replace('/', "_");
    sig
}
