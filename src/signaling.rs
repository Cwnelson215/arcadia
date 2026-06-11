//! HTTP + WebSocket signaling server (axum).
//!
//! Serves the static `web/` client and a `/ws` endpoint. Each WebSocket
//! connection is one streaming session: it spins up a fresh pipeline (so a page
//! reload cleanly reconnects) and relays SDP/ICE between the browser and
//! webrtcbin. Stage 1 assumes a single viewer at a time.

use crate::launcher::Launcher;
use crate::{pipeline, Args};
use anyhow::Result;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    args: Arc<Args>,
    launcher: Arc<Launcher>,
}

pub async fn serve(addr: SocketAddr, args: Args) -> Result<()> {
    let web_dir = args.web_dir.clone();
    let launcher = Arc::new(Launcher::load(&args.games_config, &args.display)?);
    let state = AppState {
        args: Arc::new(args),
        launcher,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/games", get(list_games))
        .route("/api/launch", post(launch_game))
        .route("/api/stop", post(stop_game))
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

// ---- launcher API -----------------------------------------------------------
// The browser lists games, starts one (which the launcher runs on the capture
// display), then connects the stream. Independent of any WebRTC session.

async fn list_games(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "games": state.launcher.games(),
        "current": state.launcher.current_id(),
    }))
}

#[derive(Deserialize)]
struct LaunchReq {
    id: String,
}

async fn launch_game(
    State(state): State<AppState>,
    Json(req): Json<LaunchReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.launcher.launch(&req.id) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "current": state.launcher.current_id() })),
        ),
        Err(e) => {
            error!("launch '{}' failed: {e:#}", req.id);
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": format!("{e:#}") })),
            )
        }
    }
}

async fn stop_game(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match state.launcher.stop() {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("{e:#}") })),
        ),
    }
}
