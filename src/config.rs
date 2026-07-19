//! Runtime configuration, sourced entirely from the environment.
//!
//! Redis is intentionally optional: when `REDIS_URL` is absent the engine falls back
//! to SQLite-backed idempotency and an in-process retry queue, which is all a
//! single-instance self-hosted deployment needs.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    // Shared secret for HMAC on the inbound SMS webhook (and outbound merchant
    // webhooks in M3). Required in production; a dev default keeps local runs easy.
    pub sms_hmac_secret: String,
    // Secret used to sign OUTBOUND merchant webhooks. Distinct from the inbound SMS
    // secret so the two trust boundaries can be rotated independently.
    pub webhook_hmac_secret: String,
    // Consumed by the retry/idempotency queue in M3; absent → SQLite fallback path.
    #[allow(dead_code)]
    pub redis_url: Option<String>,
    // Directory of built dashboard assets. Served (with SPA fallback) only if it
    // exists — self-hosted API-only deployments simply omit it.
    pub static_dir: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let port: u16 = env_or("PORT", "8080").parse()?;
        let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));

        Ok(Self {
            bind_addr,
            database_url: env_or("DATABASE_URL", "sqlite://spritexai_pay.db?mode=rwc"),
            sms_hmac_secret: env_or("SMS_HMAC_SECRET", "dev-insecure-secret-change-me"),
            webhook_hmac_secret: env_or("WEBHOOK_HMAC_SECRET", "dev-insecure-webhook-secret"),
            redis_url: std::env::var("REDIS_URL").ok().filter(|s| !s.is_empty()),
            static_dir: env_or("STATIC_DIR", "./static"),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
