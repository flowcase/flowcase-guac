mod bridge;
mod cli;
mod guacd;
mod token;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use tower_http::services::ServeDir;
use tracing::info;
use tracing_subscriber::EnvFilter;

const LISTEN_ADDR: &str = "0.0.0.0:8080";

/// Default to the relative `public/` dir next to the binary's working
/// directory; the legacy Node version did the same. Override with
/// `FLOWCASE_GUAC_PUBLIC_DIR` if running outside the repo root.
fn public_dir() -> PathBuf {
    std::env::var_os("FLOWCASE_GUAC_PUBLIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("public"))
}

/// Default to a co-located guacd unless `FLOWCASE_GUACD_ADDR` is set.
fn guacd_addr() -> Result<SocketAddr> {
    std::env::var("FLOWCASE_GUACD_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:4822".to_string())
        .parse::<SocketAddr>()
        .context("FLOWCASE_GUACD_ADDR must be an addr:port")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = cli::Cli::parse();
    let key = args.key_bytes().context("validating AES key")?;
    let public = public_dir();
    let guacd = guacd_addr()?;

    info!(
        listen = LISTEN_ADDR,
        public = %public.display(),
        guacd = %guacd,
        "starting flowcase-guac"
    );

    let state = bridge::BridgeState {
        key: Arc::new(key),
        guacd_addr: guacd,
        public_dir: Arc::new(public.clone()),
    };

    // /vnc.html is dual-purpose (WS upgrade vs static GET); everything
    // else under public/ (js/, styles.css) is served by ServeDir.
    let static_files = ServeDir::new(public);
    let app = Router::new()
        .route("/vnc.html", get(bridge::handle_vnc_html))
        .with_state(state)
        .fallback_service(static_files);

    let addr: SocketAddr = LISTEN_ADDR.parse().expect("LISTEN_ADDR is a constant");
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::select! {
        result = axum::serve(listener, app) => result.context("http server")?,
        _ = tokio::signal::ctrl_c() => info!("ctrl_c received, shutting down"),
    }

    Ok(())
}
