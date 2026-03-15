//! Coinbase CDP API JWT authentication (ES256).
//!
//! Each Coinbase API request requires a freshly generated JWT signed with
//! ECDSA P-256 (ES256). The JWT is scoped to the exact HTTP method and path.
//!
//! ## JWT structure
//! - **Header:** `{"alg":"ES256","typ":"JWT","kid":"{key_name}","nonce":"{uuid4_hex}"}`
//! - **Claims:** `{"sub":"{key_name}","iss":"cdp","nbf":{ts},"exp":{ts+120},"uri":"{METHOD} api.coinbase.com{path}"}`
//! - **Signature:** ECDSA P-256 with SHA-256, compact r||s (64 bytes)
//!
//! Reference: <https://docs.cdp.coinbase.com/coinbase-app/authentication-authorization/api-key-authentication>

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use p256::SecretKey;
use serde_json::json;
use uuid::Uuid;

use crate::types::AppError;
use super::HttpMethod;

/// Build a JWT for authenticating a Coinbase CDP API request.
///
/// - `key_name`: CDP key name, e.g., `"organizations/{org_id}/apiKeys/{key_id}"`
/// - `signing_key`: pre-parsed ECDSA P-256 signing key
/// - `method`: HTTP method (GET or POST)
/// - `path`: Request path, e.g., `"/v2/accounts"`
///
/// Returns the signed JWT string suitable for `Authorization: Bearer {jwt}`.
pub fn build_jwt(
    key_name: &str,
    signing_key: &SigningKey,
    method: HttpMethod,
    path: &str,
) -> Result<String, AppError> {
    let now = current_timestamp();
    let nonce = Uuid::new_v4().to_string().replace('-', "");

    // Build header and claims as JSON, then base64url-encode them.
    let header = json!({
        "alg": "ES256",
        "typ": "JWT",
        "kid": key_name,
        "nonce": nonce,
    });

    let uri = format!("{method} api.coinbase.com{path}");
    let claims = json!({
        "sub": key_name,
        "iss": "cdp",
        "nbf": now,
        "exp": now + 120,
        "uri": uri,
    });

    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string().as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");

    // Sign with ECDSA P-256 (SHA-256 hash + compact r||s signature).
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("{signing_input}.{sig_b64}"))
}

/// Parse a PEM-encoded ECDSA P-256 private key.
///
/// Supports both formats:
/// - **PKCS#8** (`-----BEGIN PRIVATE KEY-----`) — standard format
/// - **SEC1/EC** (`-----BEGIN EC PRIVATE KEY-----`) — Coinbase CDP format
///
/// The PEM may contain literal `\n` escape sequences (common in TOML/env
/// vars) which we normalize first.
pub fn parse_pem_key(pem: &str) -> Result<SigningKey, AppError> {
    // Normalize escaped newlines (TOML string may contain literal \n).
    let normalized = pem.replace("\\n", "\n");

    // Try PKCS#8 first (standard format), then SEC1/EC (Coinbase CDP format).
    SigningKey::from_pkcs8_pem(&normalized)
        .or_else(|_| {
            SecretKey::from_sec1_pem(&normalized)
                .map(SigningKey::from)
        })
        .map_err(|e| AppError::Coinbase(format!("Invalid ECDSA P-256 key: {e}")))
}

/// Current Unix timestamp in seconds. Extracted as a function for testability.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::HttpMethod;
    use p256::ecdsa::{signature::Verifier, VerifyingKey};

    /// Generate a fresh P-256 key pair for testing. Returns (PEM string, SigningKey).
    fn test_key_pair() -> (String, SigningKey) {
        use p256::pkcs8::EncodePrivateKey;
        use p256::SecretKey;
        use rand_core::OsRng;

        let secret = SecretKey::random(&mut OsRng);
        let signing_key = SigningKey::from(&secret);
        let pem = secret
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .expect("PKCS8 PEM encoding");

        (pem.to_string(), signing_key)
    }

    #[test]
    fn build_jwt_produces_three_part_token() {
        let (_, key) = test_key_pair();
        let jwt = build_jwt(
            "organizations/org1/apiKeys/key1",
            &key,
            HttpMethod::Get,
            "/v2/accounts",
        )
        .expect("build_jwt");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must have 3 dot-separated parts");
    }

    #[test]
    fn jwt_header_contains_required_fields() {
        let (_, key) = test_key_pair();
        let jwt = build_jwt(
            "organizations/org1/apiKeys/key1",
            &key,
            HttpMethod::Get,
            "/v2/accounts",
        )
        .unwrap();

        let header_b64 = jwt.split('.').next().unwrap();
        let header_bytes = URL_SAFE_NO_PAD.decode(header_b64).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&header_bytes).unwrap();

        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "organizations/org1/apiKeys/key1");
        assert!(header["nonce"].is_string());
        // Nonce should be a hex UUID (32 chars, no dashes).
        let nonce = header["nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 32);
    }

    #[test]
    fn jwt_claims_contain_required_fields() {
        let (_, key) = test_key_pair();
        let jwt = build_jwt(
            "organizations/org1/apiKeys/key1",
            &key,
            HttpMethod::Post,
            "/api/v3/brokerage/orders",
        )
        .unwrap();

        let claims_b64 = jwt.split('.').nth(1).unwrap();
        let claims_bytes = URL_SAFE_NO_PAD.decode(claims_b64).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&claims_bytes).unwrap();

        assert_eq!(claims["sub"], "organizations/org1/apiKeys/key1");
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(
            claims["uri"],
            "POST api.coinbase.com/api/v3/brokerage/orders"
        );

        let nbf = claims["nbf"].as_u64().unwrap();
        let exp = claims["exp"].as_u64().unwrap();
        assert_eq!(exp - nbf, 120, "JWT validity window should be 120 seconds");
    }

    #[test]
    fn jwt_signature_is_valid() {
        let (_, signing_key) = test_key_pair();
        let verifying_key = VerifyingKey::from(&signing_key);

        let jwt = build_jwt(
            "organizations/org1/apiKeys/key1",
            &signing_key,
            HttpMethod::Get,
            "/v2/accounts",
        )
        .unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let signature = Signature::from_slice(&sig_bytes).unwrap();

        verifying_key
            .verify(signing_input.as_bytes(), &signature)
            .expect("signature verification should succeed");
    }

    #[test]
    fn parse_pem_with_escaped_newlines() {
        let (pem, _) = test_key_pair();
        // Simulate how PEM looks in a TOML string (literal \n instead of newlines).
        let escaped = pem.replace('\n', "\\n");
        let result = parse_pem_key(&escaped);
        assert!(result.is_ok(), "Should handle escaped newlines");
    }

    #[test]
    fn invalid_pem_returns_error() {
        let result = parse_pem_key("not a valid key");
        assert!(result.is_err());
    }
}
