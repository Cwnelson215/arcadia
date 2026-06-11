//! GStreamer pipeline + webrtcbin wiring for one streaming session.
//!
//! bulbasaur is the WebRTC *offerer*. When the pipeline reaches PLAYING,
//! webrtcbin fires `on-negotiation-needed`; we create an offer, set it as the
//! local description, and send it to the browser over the WebSocket. ICE
//! candidates are relayed both ways. The browser's answer/ICE are applied back
//! onto webrtcbin.
//!
//! GStreamer signal callbacks run on streaming/internal threads; they push
//! outbound JSON into an mpsc channel that this async task drains to the
//! WebSocket. Inbound browser messages are applied to webrtcbin directly (gst
//! elements are Send+Sync).

use crate::Args;
use anyhow::{anyhow, bail, Context, Result};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use crate::gamepad;
use crate::input;
use serde_json::{json, Value};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub async fn run_session(socket: WebSocket, args: Arc<Args>) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // gst callback threads -> this task -> browser
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let (pipeline, webrtc) =
        build_pipeline(&args, out_tx.clone()).context("building pipeline")?;

    // webrtcbin only allows create-data-channel once it's realized (its SCTP
    // transport exists) — i.e. at least READY. READY doesn't trigger
    // negotiation (that waits for PLAYING), so the channel still makes the first
    // offer alongside the video.
    pipeline
        .set_state(gst::State::Ready)
        .context("setting pipeline to READY")?;

    // Input round-trip: a data channel the browser sends key/mouse/gamepad
    // events on. Created *before* PLAYING so it appears in the SDP offer and
    // negotiates alongside the video. Keyboard/mouse and gamepad both inject via
    // uinput, on separate threads (each owns its own virtual devices).
    let (input_tx, input_rx) = std_mpsc::channel::<input::InputEvent>();
    std::thread::spawn(move || input::run(input_rx));

    let (gamepad_tx, gamepad_rx) = std_mpsc::channel::<input::GamepadState>();
    std::thread::spawn(move || gamepad::run(gamepad_rx));

    // The H.264 encoder, for adaptive-bitrate requests (changeable while PLAYING).
    let enc = pipeline.by_name("enc").context("encoder 'enc' not found")?;

    // The ximagesrc capture element (only with --source x11), so the browser can
    // toggle the captured cursor's visibility live as it (un)locks the pointer.
    let cap = pipeline.by_name("cap");

    // "create-data-channel" returns a *nullable* GstWebRTCDataChannel, so it must
    // be received as Option<_> (else glib's conversion panics).
    let data_channel = webrtc
        .emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
            "create-data-channel",
            &[&"input", &None::<gst::Structure>],
        )
        .context("webrtcbin create-data-channel returned null")?;
    data_channel.connect_closure(
        "on-open",
        false,
        glib::closure!(move |_dc: &gst_webrtc::WebRTCDataChannel| {
            info!("input data channel open");
        }),
    );
    data_channel.connect_closure(
        "on-message-string",
        false,
        glib::closure!(move |_dc: &gst_webrtc::WebRTCDataChannel, msg: String| {
            if let Some(ev) = input::InputEvent::from_json(&msg) {
                match ev {
                    input::InputEvent::Gamepad(state) => {
                        let _ = gamepad_tx.send(state);
                    }
                    input::InputEvent::Bitrate { kbps } => {
                        let clamped = kbps.clamp(1000, 20000);
                        enc.set_property("bitrate", clamped);
                    }
                    input::InputEvent::Capture { on } => {
                        // Show the captured X cursor only while the browser holds
                        // pointer lock. No-op for --source test (no `cap`).
                        if let Some(cap) = &cap {
                            cap.set_property("show-pointer", on);
                        }
                    }
                    other => {
                        let _ = input_tx.send(other);
                    }
                }
            }
        }),
    );

    let bus = pipeline.bus().expect("pipeline has a bus");
    let mut bus_stream = bus.stream();

    pipeline
        .set_state(gst::State::Playing)
        .context("setting pipeline to PLAYING")?;

    let result = loop {
        tokio::select! {
            // outbound signaling -> browser
            Some(msg) = out_rx.recv() => {
                if ws_tx.send(Message::Text(msg)).await.is_err() {
                    break Ok(());
                }
            }
            // inbound signaling <- browser
            ws_msg = ws_rx.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(txt))) => {
                        if let Err(e) = handle_browser_msg(&webrtc, &txt) {
                            warn!("ignoring bad signaling msg: {e:#}");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Err(e)) => break Err(anyhow!("websocket error: {e}")),
                    _ => {}
                }
            }
            // pipeline bus (errors / EOS)
            Some(msg) = bus_stream.next() => {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        break Err(anyhow!(
                            "pipeline error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        ));
                    }
                    MessageView::Eos(_) => {
                        info!("pipeline reached EOS");
                        break Ok(());
                    }
                    _ => {}
                }
            }
            else => break Ok(()),
        }
    };

    if let Err(e) = pipeline.set_state(gst::State::Null) {
        error!("failed to NULL the pipeline: {e}");
    }
    result
}

fn build_pipeline(
    args: &Args,
    out_tx: mpsc::UnboundedSender<String>,
) -> Result<(gst::Pipeline, gst::Element)> {
    let source_chain = match args.source.as_str() {
        "test" => {
            "videotestsrc is-live=true pattern=smpte \
             ! timeoverlay halignment=right valignment=bottom font-desc=\"Sans 36\" \
             ! video/x-raw,width=1280,height=720,framerate=60/1"
                .to_string()
        }
        "x11" => format!(
            "ximagesrc display-name={} use-damage=false show-pointer=false name=cap \
             ! video/x-raw,framerate=60/1",
            args.display
        ),
        other => bail!("unknown --source {other} (use: test | x11)"),
    };

    // Optional audio: a second media branch (Opus) linked into the same
    // webrtcbin via the `sendrecv.` reference, so it negotiates as a second
    // m-line alongside the video.
    let audio_chain = match args.audio.as_str() {
        "none" => String::new(),
        "test" => " audiotestsrc is-live=true wave=sine freq=440 volume=0.2 \
                    ! audioconvert ! audioresample ! opusenc \
                    ! rtpopuspay pt=97 \
                    ! application/x-rtp,media=audio,encoding-name=OPUS,payload=97 \
                    ! sendrecv."
            .to_string(),
        "pulse" => format!(
            " pulsesrc device={} ! audioconvert ! audioresample ! opusenc \
              ! rtpopuspay pt=97 \
              ! application/x-rtp,media=audio,encoding-name=OPUS,payload=97 \
              ! sendrecv.",
            args.audio_device
        ),
        other => bail!("unknown --audio {other} (use: none | test | pulse)"),
    };

    // NOTE: vah264enc property/profile names are version-sensitive — confirm with
    // `gst-inspect-1.0 vah264enc` on bulbasaur. Renoir's VAProfileH264ConstrainedBaseline
    // (verified in Stage 0) is the broadest for cross-browser WebRTC decode; if the
    // profile caps fail to negotiate, fall back to `profile=main`.
    let desc = format!(
        "{source} \
         ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12 \
         ! vah264enc name=enc rate-control=cbr bitrate={bitrate} key-int-max=30 b-frames=0 \
           target-usage=7 \
         ! video/x-h264,profile=constrained-baseline ! h264parse \
         ! rtph264pay pt=96 config-interval=-1 aggregate-mode=zero-latency mtu=1200 \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96 \
         ! webrtcbin name=sendrecv bundle-policy=max-bundle latency=40{audio}",
        source = source_chain,
        bitrate = args.bitrate,
        audio = audio_chain,
    );
    info!("gstreamer pipeline:\n  {desc}");

    let pipeline = gst::parse::launch(&desc)
        .context("gst::parse::launch failed")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("parsed element is not a Pipeline"))?;

    let webrtc = pipeline
        .by_name("sendrecv")
        .context("webrtcbin 'sendrecv' not found in pipeline")?;

    // No external STUN — Tailscale provides a direct host ICE candidate.
    webrtc.set_property_from_str("stun-server", "");

    // on-negotiation-needed -> create + send offer
    let webrtc_weak = webrtc.downgrade();
    let tx_offer = out_tx.clone();
    webrtc.connect_closure(
        "on-negotiation-needed",
        false,
        glib::closure!(move |_w: &gst::Element| {
            let Some(webrtc) = webrtc_weak.upgrade() else {
                return;
            };
            let tx = tx_offer.clone();
            let webrtc_for_cb = webrtc.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                on_offer_created(&webrtc_for_cb, reply, tx.clone());
            });
            webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        }),
    );

    // on-ice-candidate -> send to browser
    let tx_ice = out_tx.clone();
    webrtc.connect_closure(
        "on-ice-candidate",
        false,
        glib::closure!(move |_w: &gst::Element, mlineindex: u32, candidate: String| {
            let msg = json!({
                "type": "ice",
                "candidate": candidate,
                "sdpMLineIndex": mlineindex,
            });
            let _ = tx_ice.send(msg.to_string());
        }),
    );

    Ok((pipeline, webrtc))
}

fn on_offer_created(
    webrtc: &gst::Element,
    reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    tx: mpsc::UnboundedSender<String>,
) {
    let reply = match reply {
        Ok(Some(r)) => r,
        Ok(None) => {
            warn!("create-offer produced no reply");
            return;
        }
        Err(e) => {
            warn!("create-offer failed: {e:?}");
            return;
        }
    };

    let offer = match reply.get::<gst_webrtc::WebRTCSessionDescription>("offer") {
        Ok(o) => o,
        Err(e) => {
            warn!("offer missing from reply: {e}");
            return;
        }
    };

    webrtc.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

    let sdp_text = offer.sdp().as_text().unwrap_or_default();
    info!("offer SDP:\n{sdp_text}");
    let msg = json!({ "type": "offer", "sdp": sdp_text });
    let _ = tx.send(msg.to_string());
    info!("sent offer to browser");
}

fn handle_browser_msg(webrtc: &gst::Element, txt: &str) -> Result<()> {
    let v: Value = serde_json::from_str(txt)?;
    match v.get("type").and_then(Value::as_str) {
        Some("answer") => {
            let sdp = v
                .get("sdp")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("answer missing sdp"))?;
            let sdp_msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .map_err(|_| anyhow!("failed to parse answer SDP"))?;
            let answer = gst_webrtc::WebRTCSessionDescription::new(
                gst_webrtc::WebRTCSDPType::Answer,
                sdp_msg,
            );
            info!("answer SDP:\n{sdp}");
            webrtc.emit_by_name::<()>(
                "set-remote-description",
                &[&answer, &None::<gst::Promise>],
            );
            info!("applied browser answer");
        }
        Some("ice") => {
            let candidate = v
                .get("candidate")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("ice missing candidate"))?;
            let mline = v
                .get("sdpMLineIndex")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
        }
        other => warn!("ignoring signaling msg of type {other:?}"),
    }
    Ok(())
}
