use anyhow::{Context, Result};
use nuthatch_x402::{AppState, Config, NuthatchHttpBackend, UnconfiguredFacilitator, serve};
use std::{net::SocketAddr, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("nuthatch_x402=info")
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "x402.toml".into());
    let config = Config::load(&path)?;
    let address: SocketAddr = std::env::var("NUTHATCH_X402_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8402".into())
        .parse()
        .context("parse NUTHATCH_X402_LISTEN")?;
    let backend = Arc::new(NuthatchHttpBackend::new(config.provider.upstream.clone()));
    let state = AppState::new(config, Arc::new(UnconfiguredFacilitator), backend);
    tracing::info!(%address, "nuthatch-x402 listening; payment signatures are refused until a facilitator adapter is configured");
    serve(address, state).await
}
