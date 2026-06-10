// arcadia — Stage 1 browser client.
//
// The browser is the WebRTC answerer. It opens a WebSocket to the signaling
// server, receives bulbasaur's SDP offer + ICE, answers, and renders the
// incoming H.264 track in a <video>. A getStats() loop shows the latency
// readout (RTT, fps, bitrate, jitter) — the Stage-1 measurement.

const $ = (id) => document.getElementById(id);
let pc, ws, statsTimer;

function setState(text, cls) {
  $("statetext").textContent = text;
  $("state").className = "dot" + (cls ? " " + cls : "");
}

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/ws`;
}

async function connect() {
  $("connect").disabled = true;
  setState("connecting…");

  // No ICE servers: over Tailscale the host candidate connects directly.
  pc = new RTCPeerConnection({ iceServers: [] });
  pc.addTransceiver("video", { direction: "recvonly" });

  pc.ontrack = (ev) => {
    setState("streaming", "on");
    $("video").srcObject = ev.streams[0];
  };

  pc.onicecandidate = (ev) => {
    if (ev.candidate && ws && ws.readyState === WebSocket.OPEN) {
      ws.send(
        JSON.stringify({
          type: "ice",
          candidate: ev.candidate.candidate,
          sdpMLineIndex: ev.candidate.sdpMLineIndex,
        })
      );
    }
  };

  pc.onconnectionstatechange = () => {
    const s = pc.connectionState;
    setState(s, s === "connected" ? "on" : s === "failed" ? "err" : null);
  };

  ws = new WebSocket(wsUrl());
  ws.onmessage = async (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.type === "offer") {
      await pc.setRemoteDescription({ type: "offer", sdp: msg.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      ws.send(JSON.stringify({ type: "answer", sdp: answer.sdp }));
    } else if (msg.type === "ice") {
      try {
        await pc.addIceCandidate({
          candidate: msg.candidate,
          sdpMLineIndex: msg.sdpMLineIndex,
        });
      } catch (e) {
        console.warn("addIceCandidate failed", e);
      }
    }
  };
  ws.onclose = () => {
    setState("disconnected", "err");
    $("connect").disabled = false;
  };
  ws.onerror = () => setState("ws error", "err");

  startStats();
}

function startStats() {
  clearInterval(statsTimer);
  let lastBytes = 0;
  let lastTs = 0;
  statsTimer = setInterval(async () => {
    if (!pc) return;
    const stats = await pc.getStats();
    const line = {};
    stats.forEach((r) => {
      if (r.type === "inbound-rtp" && r.kind === "video") {
        line.fps = r.framesPerSecond ?? "-";
        line.decoded = r.framesDecoded ?? 0;
        line.keyframes = r.keyFramesDecoded ?? 0;
        line.jitterMs = r.jitter != null ? (r.jitter * 1000).toFixed(1) : "-";
        if (r.frameWidth) line.res = `${r.frameWidth}x${r.frameHeight}`;
        const now = r.timestamp;
        const bytes = r.bytesReceived ?? 0;
        if (lastTs) {
          // bytes*8 bits over (now-lastTs) ms == kbits/s
          line.kbps = (((bytes - lastBytes) * 8) / (now - lastTs)).toFixed(0);
        }
        lastBytes = bytes;
        lastTs = now;
      }
      if (r.type === "candidate-pair" && r.nominated && r.state === "succeeded") {
        line.rttMs =
          r.currentRoundTripTime != null
            ? (r.currentRoundTripTime * 1000).toFixed(1)
            : "-";
      }
    });
    $("stats").textContent =
      `res:       ${line.res ?? "-"}\n` +
      `fps:       ${line.fps ?? "-"}\n` +
      `decoded:   ${line.decoded ?? 0} frames\n` +
      `keyframes: ${line.keyframes ?? 0}\n` +
      `bitrate:   ${line.kbps ?? "-"} kbps\n` +
      `rtt:       ${line.rttMs ?? "-"} ms\n` +
      `jitter:    ${line.jitterMs ?? "-"} ms`;
  }, 1000);
}

$("connect").addEventListener("click", connect);
