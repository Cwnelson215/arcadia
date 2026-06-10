//! HTTP + WebSocket signaling server (axum).
//!
//! Serves the static `web/` client and a `/ws` endpoint. Each WebSocket
//! connection is one streaming session: it spins up a fresh pipeline (so a page
//! reload cleanly reconnects) and relays SDP/ICE between the browser and
//! webrtcbin. Stage 1 assumes a single viewer at a time.

use crate::{pipeline, Args};
use anyhow::Result;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    args: Arc<Args>,
}

pub async fn serve(addr: SocketAddr, args: Args) -> Result<()> {
    let web_dir = args.web_dir.clone();
    let state = AppState {
        args: Arc::new(args),
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(&web_dir))
        .with_state(state);

    info!("arcadia listening on http://{addr}  — open it from a tailnet device");
    info!("serving static client from ./{web_dir}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    info!("browser connected — starting streaming session");
    if let Err(e) = pipeline::run_session(socket, state.args.clone()).await {
        error!("session ended with error: {e:#}");
    }
    info!("streaming session closed");
}
