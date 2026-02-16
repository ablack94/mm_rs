use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};

type HmacSha512 = Hmac<Sha512>;

/// Generate the API-Sign header for Kraken REST requests.
pub fn sign_request(urlpath: &str, nonce: &str, post_data: &str, secret_b64: &str) -> Result<String> {
    let secret = B64.decode(secret_b64)?;
    let sha_input = format!("{}{}", nonce, post_data);
    let sha_digest = Sha256::digest(sha_input.as_bytes());
    let mut hmac_input = urlpath.as_bytes().to_vec();
    hmac_input.extend_from_slice(&sha_digest);
    let mut mac = HmacSha512::new_from_slice(&secret)?;
    mac.update(&hmac_input);
    Ok(B64.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_produces_base64() {
        // Just verify it doesn't panic and returns base64
        let result = sign_request(
            "/0/private/GetWebSocketsToken",
            "1234567890",
            "nonce=1234567890",
            &B64.encode(b"test-secret-key-bytes-here-32bb"),
        );
        assert!(result.is_ok());
        let sig = result.unwrap();
        assert!(B64.decode(&sig).is_ok());
    }
}
