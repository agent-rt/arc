//! `arc-relay` — a zero-knowledge WebSocket relay.
//!
//! It pairs the two peers (controller + runner) of a [`SessionId`] and forwards
//! opaque, end-to-end-encrypted payloads between them. It holds no keys and
//! never sees L2 plaintext.
//!
//! Configuration (environment):
//! * `ARC_RELAY_ADDR` — bind address (default `0.0.0.0:8787`).
//! * `RUST_LOG` — tracing filter (default `info`).
//!
//! [`SessionId`]: arc_proto::id::SessionId

#![forbid(unsafe_code)]

mod connection;
mod hub;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use tracing_subscriber::EnvFilter;

use hub::Hub;

const DEFAULT_ADDR: &str = "0.0.0.0:8787";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let addr: SocketAddr = std::env::var("ARC_RELAY_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_owned())
        .parse()?;

    let hub = Arc::new(Hub::new());
    let app = Router::new()
        .route("/v1/relay", get(ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(hub);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "arc relay listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Upgrades an HTTP request to a relay WebSocket connection.
async fn ws_handler(ws: WebSocketUpgrade, State(hub): State<Arc<Hub>>) -> Response {
    ws.on_upgrade(move |socket| connection::serve(socket, hub))
}
