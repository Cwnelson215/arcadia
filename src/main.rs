//! arcadia — Stage 1: one-way video.
//!
//! One binary runs on bulbasaur and does three jobs:
//!   1. serves the static web client (`web/`)
//!   2. runs a WebSocket signaling endpoint (`/ws`) that relays SDP + ICE
//!   3. builds and drives the GStreamer pipeline (capture → VA-API H.264 →
//!      webrtcbin) — see `pipeline.rs`.
//!
//! The browser is the WebRTC answerer; bulbasaur (webrtcbin) is the offerer.
//! Transport is direct over Tailscale, so no STUN/TURN is configured.

use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;

mod gamepad;
mod input;
mod pipeline;
mod signaling;

#[derive(Parser, Debug, Clone)]
#[command(name = "arcadia", about = "Stage 1 one-way video streamer")]
pub struct Args {
    /// Video source: "test" (videotestsrc) or "x11" (ximagesrc)
    #[arg(long, default_value = "test")]
    pub source: String,

    /// X11 display to capture when --source x11
    #[arg(long, default_value = ":99")]
    pub display: String,

    /// Target H.264 bitrate in kbps
    #[arg(long, default_value_t = 15000)]
    pub bitrate: u32,

    /// Address to bind the web + signaling server
    #[arg(long, default_value = "0.0.0.0:8080")]
    pub bind: String,

    /// Directory of the static web client
    #[arg(long, default_value = "web")]
    pub web_dir: String,

    /// Create the virtual gamepad and sweep it for ~8s, then exit (no server).
    /// For verifying uinput access without a browser/controller.
    #[arg(long)]
    pub gamepad_selftest: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,arcadia=debug".into()),
        )
        .init();

    let args = Args::parse();

    if args.gamepad_selftest {
        return gamepad::selftest();
    }

    gstreamer::init()?;
    let addr: SocketAddr = args.bind.parse()?;
    signaling::serve(addr, args).await
}
