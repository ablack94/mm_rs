use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Sign a Coinbase Advanced Trade API request using legacy HMAC-SHA256.
///
/// Returns the hex-encoded signature (lowercase).
///
/// The message is: `timestamp + method + request_path + body`
/// - `timestamp`: Unix epoch seconds as string
/// - `method`: Uppercase HTTP method (GET, POST)
/// - `request_path`: Path only, no host, no query params (e.g., "/api/v3/brokerage/orders")
/// - `body`: Request body string (empty string for GET)
pub fn sign_request(
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
    api_secret: &str,
) -> String {
    let message = format!("{}{}{}{}", timestamp, method, request_path, body);
    let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    hex_encode(mac.finalize().into_bytes())
}

/// Simple hex encoding (avoids adding hex crate dependency).
pub fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{:02x}", b)).collect()
}

/// Raw HMAC-SHA256 of a message, returned as lowercase hex.
/// Used for Coinbase WS authentication where the message format differs from REST.
pub fn hmac_sha256_hex(message: &str, secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    hex_encode(mac.finalize().into_bytes())
}

/// Get current Unix timestamp as string.
pub fn timestamp_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_produces_hex() {
        let sig = sign_request(
            "1234567890",
            "GET",
            "/api/v3/brokerage/accounts",
            "",
            "test-secret",
        );
        // Should be a valid hex string (64 chars for SHA-256)
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sign_lowercase() {
        let sig = sign_request(
            "1234567890",
            "POST",
            "/api/v3/brokerage/orders",
            r#"{"product_id":"BTC-USD"}"#,
            "my-secret-key",
        );
        assert_eq!(sig, sig.to_lowercase());
    }

    #[test]
    fn test_sign_deterministic() {
        let sig1 = sign_request("100", "GET", "/path", "", "secret");
        let sig2 = sign_request("100", "GET", "/path", "", "secret");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_sign_different_inputs() {
        let sig1 = sign_request("100", "GET", "/path", "", "secret");
        let sig2 = sign_request("101", "GET", "/path", "", "secret");
        assert_ne!(sig1, sig2);
    }
}
