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
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub async fn run_session(socket: WebSocket, args: Arc<Args>) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // gst callback threads -> this task -> browser
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let (pipeline, webrtc) =
        build_pipeline(&args, out_tx.clone()).context("building pipeline")?;

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
            "ximagesrc display-name={} use-damage=false show-pointer=false \
             ! video/x-raw,framerate=60/1",
            args.display
        ),
        other => bail!("unknown --source {other} (use: test | x11)"),
    };

    // NOTE: vah264enc property/profile names are version-sensitive — confirm with
    // `gst-inspect-1.0 vah264enc` on bulbasaur. Renoir's VAProfileH264ConstrainedBaseline
    // (verified in Stage 0) is the broadest for cross-browser WebRTC decode; if the
    // profile caps fail to negotiate, fall back to `profile=main`.
    let desc = format!(
        "{source} \
         ! videoconvert ! video/x-raw,format=NV12 \
         ! vah264enc name=enc rate-control=cbr bitrate={bitrate} key-int-max=30 b-frames=0 \
         ! video/x-h264,profile=constrained-baseline ! h264parse \
         ! rtph264pay pt=96 config-interval=-1 aggregate-mode=zero-latency mtu=1200 \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96 \
         ! webrtcbin name=sendrecv bundle-policy=max-bundle",
        source = source_chain,
        bitrate = args.bitrate,
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
