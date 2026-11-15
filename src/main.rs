mod config;
mod db;
mod ledger;
mod matrix;
mod parser;

use std::path::PathBuf;

use anyhow::Context;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/ledger-bot/config.toml"));

    let config = config::Config::from_path(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;

    let pool = db::connect(&config.postgres).await?;
    ledger::migrate(&pool).await?;

    matrix::run(config, pool).await
}
