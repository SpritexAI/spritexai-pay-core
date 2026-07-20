//! Admin authentication — single-operator login for the self-hosted console.
//!
//! Deliberately minimal: one admin password (from `ADMIN_PASSWORD`), and a
//! stateless signed bearer token so there's no session table to keep. The token
//! is `<expiry_unix>.<hmac>` where the HMAC (keyed by `AUTH_SECRET`) covers the
//! expiry — tamper the expiry and the signature no longer verifies. Multi-user /
//! per-tenant auth is Phase 3; this is the single-merchant boundary.
//! Author: Mohammad Sijan (SpritexAI).

use crate::crypto;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long an issued token stays valid. A self-hosted operator shouldn't be
/// re-logging-in constantly; the token is revocable only by rotating AUTH_SECRET.
// ponytail: fixed TTL, no refresh/blacklist — add a token version in AUTH_SECRET
// rotation if you need instant revocation.
const TOKEN_TTL_SECS: u64 = 7 * 24 * 60 * 60;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Constant-time check of a submitted password against the configured admin one.
/// Both sides are HMAC'd first so the comparison never runs over the raw secret
/// and always compares equal-length digests regardless of input length.
pub fn password_matches(auth_secret: &str, admin_password: &str, submitted: &str) -> bool {
    let expected = crypto::sign(auth_secret.as_bytes(), admin_password.as_bytes());
    // verify() recomputes HMAC(submitted) and constant-time compares to `expected`.
    crypto::verify(auth_secret.as_bytes(), submitted.as_bytes(), &expected)
}

/// Issue a signed bearer token valid for [`TOKEN_TTL_SECS`].
pub fn issue_token(auth_secret: &str) -> String {
    let expiry = now_secs() + TOKEN_TTL_SECS;
    let sig = crypto::sign(auth_secret.as_bytes(), expiry.to_string().as_bytes());
    format!("{expiry}.{sig}")
}

/// True iff the token is well-formed, correctly signed, and not expired.
pub fn verify_token(auth_secret: &str, token: &str) -> bool {
    let Some((expiry_str, sig)) = token.split_once('.') else {
        return false;
    };
    if !crypto::verify(auth_secret.as_bytes(), expiry_str.as_bytes(), sig) {
        return false;
    }
    match expiry_str.parse::<u64>() {
        Ok(expiry) => expiry > now_secs(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-auth-secret";

    #[test]
    fn round_trips_a_valid_token() {
        let t = issue_token(SECRET);
        assert!(verify_token(SECRET, &t));
    }

    #[test]
    fn rejects_tampered_expiry() {
        let t = issue_token(SECRET);
        let (_, sig) = t.split_once('.').unwrap();
        // Push expiry far into the future but keep the old signature.
        let forged = format!("9999999999.{sig}");
        assert!(!verify_token(SECRET, &forged));
    }

    #[test]
    fn rejects_wrong_secret_and_garbage() {
        let t = issue_token(SECRET);
        assert!(!verify_token("other-secret", &t));
        assert!(!verify_token(SECRET, "not-a-token"));
        assert!(!verify_token(SECRET, "123.deadbeef"));
    }

    #[test]
    fn password_check_is_exact() {
        assert!(password_matches(SECRET, "hunter2", "hunter2"));
        assert!(!password_matches(SECRET, "hunter2", "hunter3"));
        assert!(!password_matches(SECRET, "hunter2", ""));
    }
}
