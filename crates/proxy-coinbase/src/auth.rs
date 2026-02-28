//! Coinbase CDP API authentication via Ed25519 JWT.
//!
//! Each request requires a fresh JWT signed with the API key's Ed25519 private key.
//! The JWT expires after 120 seconds and includes the request URI.

use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64URL};
use base64::Engine as _;
use ed25519_dalek::{SigningKey, Signer};
use rand::RngCore;
use serde_json::json;

/// Generate a JWT for a Coinbase REST API request.
///
/// - `api_key`: The API key ID (UUID format, e.g., "03f51d72-...")
/// - `api_secret`: Base64-encoded Ed25519 secret (64 bytes: 32-byte seed + 32-byte public key)
/// - `method`: HTTP method ("GET", "POST")
/// - `host`: API host ("api.coinbase.com")
/// - `path`: Request path with query string (e.g., "/api/v3/brokerage/accounts")
pub fn build_jwt(
    api_key: &str,
    api_secret: &str,
    method: &str,
    host: &str,
    path: &str,
) -> Result<String, String> {
    let secret_bytes = BASE64.decode(api_secret)
        .map_err(|e| format!("Failed to base64-decode API secret: {}", e))?;

    if secret_bytes.len() < 32 {
        return Err(format!("API secret too short: {} bytes (need >= 32)", secret_bytes.len()));
    }

    // First 32 bytes are the Ed25519 seed (private key)
    let seed: [u8; 32] = secret_bytes[..32]
        .try_into()
        .map_err(|_| "Failed to extract 32-byte seed from secret".to_string())?;

    let signing_key = SigningKey::from_bytes(&seed);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Random nonce (16 bytes as hex)
    let mut nonce_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce: String = nonce_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    // URI claim: "METHOD host/path" — must NOT include query string
    let path_no_qs = path.split('?').next().unwrap_or(path);
    let uri = format!("{} {}{}", method, host, path_no_qs);

    // JWT header
    let header = json!({
        "alg": "EdDSA",
        "kid": api_key,
        "typ": "JWT",
        "nonce": nonce
    });

    // JWT payload — Coinbase CDP expects singular "uri" string, not "uris" array
    let payload = json!({
        "sub": api_key,
        "iss": "cdp",
        "nbf": now,
        "exp": now + 120,
        "uri": uri
    });

    let header_b64 = BASE64URL.encode(header.to_string().as_bytes());
    let payload_b64 = BASE64URL.encode(payload.to_string().as_bytes());
    let message = format!("{}.{}", header_b64, payload_b64);

    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = BASE64URL.encode(signature.to_bytes());

    Ok(format!("{}.{}", message, sig_b64))
}

/// Generate a JWT for Coinbase WebSocket authentication.
/// WebSocket JWTs do NOT include the `uris` claim.
pub fn build_ws_jwt(
    api_key: &str,
    api_secret: &str,
) -> Result<String, String> {
    let secret_bytes = BASE64.decode(api_secret)
        .map_err(|e| format!("Failed to base64-decode API secret: {}", e))?;

    if secret_bytes.len() < 32 {
        return Err(format!("API secret too short: {} bytes", secret_bytes.len()));
    }

    let seed: [u8; 32] = secret_bytes[..32]
        .try_into()
        .map_err(|_| "Failed to extract seed".to_string())?;

    let signing_key = SigningKey::from_bytes(&seed);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut nonce_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce: String = nonce_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    let header = json!({
        "alg": "EdDSA",
        "kid": api_key,
        "typ": "JWT",
        "nonce": nonce
    });

    let payload = json!({
        "sub": api_key,
        "iss": "cdp",
        "nbf": now,
        "exp": now + 120
    });

    let header_b64 = BASE64URL.encode(header.to_string().as_bytes());
    let payload_b64 = BASE64URL.encode(payload.to_string().as_bytes());
    let message = format!("{}.{}", header_b64, payload_b64);

    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = BASE64URL.encode(signature.to_bytes());

    Ok(format!("{}.{}", message, sig_b64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::VerifyingKey;

    fn test_keypair() -> (String, String) {
        let mut rng = rand::thread_rng();
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();

        // Coinbase format: 32-byte seed + 32-byte public key, base64-encoded
        let mut secret = Vec::with_capacity(64);
        secret.extend_from_slice(signing_key.as_bytes());
        secret.extend_from_slice(verifying_key.as_bytes());

        let api_key = "test-key-id".to_string();
        let api_secret = BASE64.encode(&secret);
        (api_key, api_secret)
    }

    #[test]
    fn test_build_jwt_produces_valid_format() {
        let (key, secret) = test_keypair();
        let jwt = build_jwt(&key, &secret, "GET", "api.coinbase.com", "/api/v3/brokerage/accounts").unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have 3 parts");

        // Decode header
        let header_json: serde_json::Value = serde_json::from_slice(
            &BASE64URL.decode(parts[0]).unwrap()
        ).unwrap();
        assert_eq!(header_json["alg"], "EdDSA");
        assert_eq!(header_json["typ"], "JWT");
        assert_eq!(header_json["kid"], "test-key-id");

        // Decode payload
        let payload_json: serde_json::Value = serde_json::from_slice(
            &BASE64URL.decode(parts[1]).unwrap()
        ).unwrap();
        assert_eq!(payload_json["iss"], "cdp");
        assert_eq!(payload_json["sub"], "test-key-id");
        assert!(payload_json["uri"].as_str().unwrap()
            .contains("GET api.coinbase.com"));
    }

    #[test]
    fn test_build_jwt_signature_verifies() {
        let (key, secret) = test_keypair();
        let jwt = build_jwt(&key, &secret, "GET", "api.coinbase.com", "/path").unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        let message = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = BASE64URL.decode(parts[2]).unwrap();

        // Recover public key from secret
        let secret_bytes = BASE64.decode(&secret).unwrap();
        let pub_bytes: [u8; 32] = secret_bytes[32..64].try_into().unwrap();
        let verifying_key = VerifyingKey::from_bytes(&pub_bytes).unwrap();

        let signature = ed25519_dalek::Signature::from_bytes(
            sig_bytes.as_slice().try_into().unwrap()
        );
        verifying_key.verify_strict(message.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn test_build_ws_jwt_has_no_uris() {
        let (key, secret) = test_keypair();
        let jwt = build_ws_jwt(&key, &secret).unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        let payload_json: serde_json::Value = serde_json::from_slice(
            &BASE64URL.decode(parts[1]).unwrap()
        ).unwrap();

        assert!(payload_json.get("uri").is_none(), "WS JWT should not have uri claim");
        assert_eq!(payload_json["iss"], "cdp");
    }

    #[test]
    fn test_each_jwt_is_unique() {
        let (key, secret) = test_keypair();
        let jwt1 = build_jwt(&key, &secret, "GET", "host", "/path").unwrap();
        let jwt2 = build_jwt(&key, &secret, "GET", "host", "/path").unwrap();
        assert_ne!(jwt1, jwt2, "Each JWT should have a unique nonce");
    }
}
