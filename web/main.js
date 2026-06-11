// arcadia — Stage 1 browser client.
//
// The browser is the WebRTC answerer. It opens a WebSocket to the signaling
// server, receives bulbasaur's SDP offer + ICE, answers, and renders the
// incoming H.264 track in a <video>. A getStats() loop shows the latency
// readout (RTT, fps, bitrate, jitter) — the Stage-1 measurement.

const $ = (id) => document.getElementById(id);
const MAX_KBPS = 15000;
let pc, ws, statsTimer;
let wantConnected = false; // user asked to stay connected -> auto-reconnect
let backoff = 500; // reconnect backoff (ms), grows to a 5s cap
let reconnecting = false; // guard against double-scheduling a reconnect
let targetKbps = MAX_KBPS; // adaptive bitrate target sent to the server

function setState(text, cls) {
  $("statetext").textContent = text;
  $("state").className = "dot" + (cls ? " " + cls : "");
}

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/ws`;
}

// Tear down the current peer connection + socket without triggering a reconnect.
function cleanup() {
  if (ws) {
    ws.onclose = ws.onerror = ws.onmessage = null;
    try { ws.close(); } catch (e) {}
    ws = null;
  }
  if (pc) {
    pc.onconnectionstatechange = pc.onicecandidate = pc.ontrack = pc.ondatachannel = null;
    try { pc.close(); } catch (e) {}
    pc = null;
  }
  inputCh = null;
}

function scheduleReconnect() {
  if (!wantConnected || reconnecting) return;
  reconnecting = true;
  cleanup();
  const delay = backoff;
  backoff = Math.min(backoff * 2, 5000);
  setState(`reconnecting in ${(delay / 1000).toFixed(1)}s…`, "err");
  setTimeout(() => {
    reconnecting = false;
    if (wantConnected) connect();
  }, delay);
}

// Entry point: keep a connection up (the server rebuilds a fresh pipeline on
// each WebSocket, so reconnecting is just connecting again).
function start() {
  wantConnected = true;
  backoff = 500;
  connect();
}

async function connect() {
  cleanup();
  $("connect").disabled = true;
  setState("connecting…");

  // No ICE servers: over Tailscale the host candidate connects directly.
  pc = new RTCPeerConnection({ iceServers: [] });
  pc.addTransceiver("video", { direction: "recvonly" });

  pc.ontrack = (ev) => {
    setState("streaming", "on");
    $("video").srcObject = ev.streams[0];
    // Minimize the receiver's playout buffer for low latency (Chrome).
    if ("playoutDelayHint" in ev.receiver) ev.receiver.playoutDelayHint = 0;
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
    if (!pc) return;
    const s = pc.connectionState;
    setState(s, s === "connected" ? "on" : s === "failed" || s === "disconnected" ? "err" : null);
    if (s === "connected") backoff = 500; // healthy -> reset backoff
    if (s === "failed" || s === "disconnected") scheduleReconnect();
  };

  // The server (offerer) creates the "input" data channel; we send
  // key/mouse/gamepad events + bitrate hints on it.
  pc.ondatachannel = (ev) => {
    inputCh = ev.channel;
    inputCh.onopen = () => console.log("input channel open");
    inputCh.onclose = () => {
      inputCh = null;
    };
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
  ws.onclose = () => scheduleReconnect();
  ws.onerror = () => {}; // close fires after error; reconnect handled there

  startStats();
}

function startStats() {
  clearInterval(statsTimer);
  let lastBytes = 0;
  let lastTs = 0;
  let lastLost = 0;
  let lastRecv = 0;
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
        if (r.jitterBufferDelay != null && r.jitterBufferEmittedCount) {
          line.jbMs = ((r.jitterBufferDelay / r.jitterBufferEmittedCount) * 1000).toFixed(0);
        }
        if (r.frameWidth) line.res = `${r.frameWidth}x${r.frameHeight}`;
        const now = r.timestamp;
        const bytes = r.bytesReceived ?? 0;
        if (lastTs) {
          // bytes*8 bits over (now-lastTs) ms == kbits/s
          line.kbps = (((bytes - lastBytes) * 8) / (now - lastTs)).toFixed(0);
        }
        lastBytes = bytes;
        lastTs = now;
        line.lost = r.packetsLost ?? lastLost;
        line.recv = r.packetsReceived ?? lastRecv;
      }
      if (r.type === "candidate-pair" && r.nominated && r.state === "succeeded") {
        line.rttMs =
          r.currentRoundTripTime != null ? (r.currentRoundTripTime * 1000).toFixed(1) : "-";
      }
    });

    // Bitrate adaptation (AIMD on packet loss, since rtpgccbwe isn't available):
    // loss high -> multiplicative decrease; sustained ~0 loss -> additive climb.
    if (line.lost != null && line.recv != null) {
      const dLost = Math.max(0, line.lost - lastLost);
      const dRecv = Math.max(0, line.recv - lastRecv);
      lastLost = line.lost;
      lastRecv = line.recv;
      if (dRecv > 0) {
        const lossFrac = dLost / (dLost + dRecv);
        const prev = targetKbps;
        if (lossFrac > 0.02) targetKbps = Math.max(1000, Math.floor(targetKbps * 0.85));
        else if (lossFrac < 0.005) targetKbps = Math.min(MAX_KBPS, targetKbps + 500);
        if (Math.abs(targetKbps - prev) >= 250 && inputCh && inputCh.readyState === "open") {
          inputCh.send(JSON.stringify({ t: "r", kbps: targetKbps }));
        }
      }
    }

    $("stats").textContent =
      `res:       ${line.res ?? "-"}\n` +
      `fps:       ${line.fps ?? "-"}\n` +
      `decoded:   ${line.decoded ?? 0} frames\n` +
      `keyframes: ${line.keyframes ?? 0}\n` +
      `bitrate:   ${line.kbps ?? "-"} kbps\n` +
      `target:    ${targetKbps} kbps\n` +
      `rtt:       ${line.rttMs ?? "-"} ms\n` +
      `jitter:    ${line.jitterMs ?? "-"} ms\n` +
      `jbuf:      ${line.jbMs ?? "-"} ms`;
  }, 1000);
}

$("connect").addEventListener("click", start);

// ---- input capture (Stage 2) -------------------------------------------------
// Pointer Lock gives relative mouse deltas (movementX/Y) ideal for mouse-look.
// Keyboard + mouse events are sent over the "input" data channel as JSON, but
// only while the pointer is locked to the video (Esc releases).

let inputCh = null;
const videoEl = $("video");

function inputActive() {
  return document.pointerLockElement === videoEl;
}

function sendInput(obj) {
  if (inputCh && inputCh.readyState === "open") {
    inputCh.send(JSON.stringify(obj));
  }
}

videoEl.addEventListener("click", () => {
  if (pc) videoEl.requestPointerLock();
});

document.addEventListener("pointerlockchange", () => {
  const locked = inputActive();
  const hint = $("hint");
  hint.className = locked ? "locked" : "";
  hint.textContent = locked
    ? "playing — mouse + keyboard captured · press Esc to release"
    : "click the video to capture mouse + keyboard · press Esc to release";
});

document.addEventListener("mousemove", (e) => {
  if (inputActive()) sendInput({ t: "m", dx: e.movementX, dy: e.movementY });
});

document.addEventListener("mousedown", (e) => {
  if (inputActive()) {
    e.preventDefault();
    sendInput({ t: "b", button: e.button, down: true });
  }
});

document.addEventListener("mouseup", (e) => {
  if (inputActive()) {
    e.preventDefault();
    sendInput({ t: "b", button: e.button, down: false });
  }
});

document.addEventListener(
  "wheel",
  (e) => {
    if (inputActive()) {
      e.preventDefault();
      sendInput({ t: "w", dy: e.deltaY });
    }
  },
  { passive: false }
);

document.addEventListener("keydown", (e) => {
  if (inputActive()) {
    e.preventDefault();
    if (e.repeat) return; // hold = one down; X server handles auto-repeat
    sendInput({ t: "k", code: e.code, down: true });
  }
});

document.addEventListener("keyup", (e) => {
  if (inputActive()) {
    e.preventDefault();
    sendInput({ t: "k", code: e.code, down: false });
  }
});

// ---- gamepad (Stage 3b) ------------------------------------------------------
// Poll the Gamepad API and stream state over the same data channel. This does
// NOT require Pointer Lock, so it works on mobile + a Bluetooth controller.

let gpIndex = null;
let lastGpSnap = "";

window.addEventListener("gamepadconnected", (e) => {
  gpIndex = e.gamepad.index;
  const hint = $("hint");
  if (hint) hint.textContent = `controller connected (${e.gamepad.id})`;
});
window.addEventListener("gamepaddisconnected", (e) => {
  if (e.gamepad.index === gpIndex) gpIndex = null;
});

function pollGamepad() {
  if (gpIndex != null && inputCh && inputCh.readyState === "open") {
    const gp = (navigator.getGamepads ? navigator.getGamepads() : [])[gpIndex];
    if (gp) {
      const a = [0, 1, 2, 3].map((i) => +(gp.axes[i] ?? 0).toFixed(3));
      const b = gp.buttons.map((btn) => +btn.value.toFixed(3));
      const snap = JSON.stringify([a, b]);
      if (snap !== lastGpSnap) {
        lastGpSnap = snap;
        sendInput({ t: "g", a, b });
      }
    }
  }
  requestAnimationFrame(pollGamepad);
}
requestAnimationFrame(pollGamepad);
