//! HMAC-SHA256 signing and verification.
//!
//! Used on both trust boundaries: inbound SMS payloads from the paired Android
//! forwarder, and (M3) outbound webhooks to merchant callback URLs. Comparison is
//! constant-time to avoid leaking the signature via timing.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Verify a hex-encoded signature against the body. Constant-time via the `hmac`
/// crate's built-in verification.
pub fn verify(secret: &[u8], body: &[u8], signature_hex: &str) -> bool {
    let Ok(sig) = hex::decode(signature_hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}
