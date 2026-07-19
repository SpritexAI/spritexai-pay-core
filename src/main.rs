//! SpritEXAI Pay — binary entrypoint.
//!
//! Wires configuration, storage and the HTTP surface together, then serves until a
//! shutdown signal arrives. All domain logic lives in the library crate.
//! Authored and maintained by Mohammad Sijan / SpritexAI.

use anyhow::Context;
use spritexai_pay::{config, db, http};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = config::Config::from_env().context("invalid configuration")?;
    tracing::info!(addr = %cfg.bind_addr, db = %cfg.database_url, "starting spritexai-pay");

    let pool = db::connect(&cfg.database_url)
        .await
        .context("database initialization failed")?;

    http::serve(cfg, pool).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,spritexai_pay=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
