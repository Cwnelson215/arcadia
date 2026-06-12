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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;
use tower_http::services::ServeDir;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    args: Arc<Args>,
    launcher: Arc<Launcher>,
    /// Count of live WebSocket sessions (viewers). Drives idle spin-down.
    viewers: Arc<AtomicUsize>,
    /// Bumped on every connect; lets a reconnect cancel a pending spin-down
    /// timer armed by the previous disconnect.
    idle_gen: Arc<AtomicU64>,
}

pub async fn serve(addr: SocketAddr, args: Args) -> Result<()> {
    let web_dir = args.web_dir.clone();
    let launcher = Arc::new(Launcher::load(&args.games_config, &args.display)?);
    let state = AppState {
        args: Arc::new(args),
        launcher,
        viewers: Arc::new(AtomicUsize::new(0)),
        idle_gen: Arc::new(AtomicU64::new(0)),
    };

    let app = build_router(state, &web_dir);

    info!("arcadia listening on http://{addr}  — open it from a tailnet device");
    info!("serving static client from ./{web_dir}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Assemble the axum router for one server instance. Split out of [`serve`] so
/// tests can exercise the `/api/*` handlers in-process (no socket, no GPU).
fn build_router(state: AppState, web_dir: &str) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/games", get(list_games))
        .route("/api/launch", post(launch_game))
        .route("/api/stop", post(stop_game))
        .fallback_service(ServeDir::new(web_dir))
        .with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    info!("browser connected — starting streaming session");

    // A viewer arrived: thaw any frozen game and invalidate pending spin-down
    // timers (the gen bump makes a stale timer's check below fail).
    if state.viewers.fetch_add(1, SeqCst) == 0 {
        state.launcher.resume();
    }
    state.idle_gen.fetch_add(1, SeqCst);

    if let Err(e) = pipeline::run_session(socket, state.args.clone()).await {
        error!("session ended with error: {e:#}");
    }
    info!("streaming session closed");

    // Last viewer left: arm a debounced spin-down (unless --idle-timeout is 0).
    let was_last = state.viewers.fetch_sub(1, SeqCst) == 1;
    let idle_secs = state.args.idle_timeout;
    if was_last && idle_secs > 0 {
        let gen = state.idle_gen.load(SeqCst);
        let launcher = state.launcher.clone();
        let viewers = state.viewers.clone();
        let idle_gen = state.idle_gen.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(idle_secs as u64)).await;
            // Only freeze if still idle and no viewer connected since arming.
            if viewers.load(SeqCst) == 0 && idle_gen.load(SeqCst) == gen {
                launcher.suspend();
            }
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use clap::Parser;
    use http_body_util::BodyExt;
    use std::io::Write;
    use tower::ServiceExt; // for `oneshot`

    fn test_state() -> AppState {
        let args = Args::try_parse_from(["arcadia"]).expect("parse args");
        // A real config so /api/games returns a non-empty list (and exercises
        // the launcher's serde serialization through the handler).
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(
            br#"
            [[game]]
            id = "retroarch"
            name = "RetroArch"
            command = "retroarch"
            "#,
        )
        .unwrap();
        let launcher = Arc::new(Launcher::load(f.path().to_str().unwrap(), &args.display).unwrap());
        AppState {
            args: Arc::new(args),
            launcher,
            viewers: Arc::new(AtomicUsize::new(0)),
            idle_gen: Arc::new(AtomicU64::new(0)),
        }
    }

    #[tokio::test]
    async fn api_games_returns_list_and_current() {
        let app = build_router(test_state(), "web");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/games")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Shape the web client depends on: { games: [...], current: null|id }.
        assert!(v.get("games").and_then(|g| g.as_array()).is_some());
        assert!(v.as_object().unwrap().contains_key("current"));
        assert_eq!(v["games"][0]["id"], "retroarch");
        assert_eq!(v["current"], serde_json::Value::Null); // nothing launched
    }
}
