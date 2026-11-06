//! Lighthouse server binary.

use anyhow::Context;
use lighthouse_config::Config;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let path = std::env::var_os("LIGHTHOUSE_CONFIG")
        .map_or_else(|| PathBuf::from("lighthouse.toml"), PathBuf::from);
    let config = Config::load(&path).with_context(|| format!("cannot load {}", path.display()))?;
    let bind = config.server.bind.clone();
    let app = lighthouse_server::build(config).await?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    tracing::info!(%bind, version = env!("CARGO_PKG_VERSION"), "lighthouse-server started");
    tokio::spawn(app.scheduler.run());
    axum_serve(listener, app.router).await
}

async fn axum_serve(
    listener: tokio::net::TcpListener,
    router: lighthouse_api::Router,
) -> anyhow::Result<()> {
    lighthouse_api::serve(listener, router)
        .await
        .context("server stopped")
}
