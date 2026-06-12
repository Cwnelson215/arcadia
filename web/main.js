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
    const v = $("video");
    v.srcObject = ev.streams[0];
    // Audio + video share one stream; unmute (the Connect click is the gesture
    // that satisfies autoplay) so the audio track is audible.
    v.muted = false;
    v.play().catch(() => {});
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
    // Tuned gentle for a jittery WAN: NACK/RTX (server-side) recovers most small
    // loss, so don't react to baseline loss and don't half-quality on a blip.
    if (line.lost != null && line.recv != null) {
      const dLost = Math.max(0, line.lost - lastLost);
      const dRecv = Math.max(0, line.recv - lastRecv);
      lastLost = line.lost;
      lastRecv = line.recv;
      if (dRecv > 0) {
        const lossFrac = dLost / (dLost + dRecv);
        const prev = targetKbps;
        if (lossFrac > 0.03) targetKbps = Math.max(1000, Math.floor(targetKbps * 0.9));
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

// ---- game launcher -----------------------------------------------------------
// List games from the server, launch one (the server runs it on the capture
// display), then connect the stream. Stop kills the running game. The launcher
// is independent of the WebRTC session — launching a new game while connected
// just changes what the (continuously captured) display shows.

async function loadGames() {
  const el = $("games");
  try {
    const res = await fetch("/api/games");
    const { games, current } = await res.json();
    if (!games || games.length === 0) {
      el.textContent = "no games configured (edit games.toml)";
      return;
    }
    el.textContent = "";
    for (const g of games) {
      const b = document.createElement("button");
      b.className = "game" + (g.id === current ? " running" : "");
      b.textContent = g.name;
      b.title = g.command + (g.args && g.args.length ? " " + g.args.join(" ") : "");
      b.addEventListener("click", () => launchGame(g.id));
      el.appendChild(b);
    }
  } catch (e) {
    el.textContent = "failed to load games";
  }
}

async function launchGame(id) {
  try {
    const res = await fetch("/api/launch", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id }),
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok || !body.ok) {
      setState("launch failed: " + (body.error || res.status), "err");
      await loadGames();
      return;
    }
    await loadGames();
    if (!wantConnected) start(); // connect the stream if not already
  } catch (e) {
    setState("launch failed", "err");
  }
}

async function stopGame() {
  try {
    await fetch("/api/stop", { method: "POST" });
  } catch (e) {}
  await loadGames();
}

$("stop").addEventListener("click", stopGame);
loadGames();

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

// Clicking the video captures mouse + keyboard via pointer lock (windowed). Esc
// releases (browser default). The captured cursor is shown server-side only
// while locked. (Esc-to-the-game via Keyboard Lock needs a secure HTTPS context
// and was removed for now — to be re-added once HTTPS is in place.)
function updateHint() {
  const locked = inputActive();
  const hint = $("hint");
  hint.className = locked ? "locked" : "";
  hint.textContent = locked
    ? "captured — Esc releases the mouse · use “Back (Esc)” / “Steam ▾” above to send those to the game"
    : "click the video to capture mouse + keyboard";
}

videoEl.addEventListener("click", () => {
  if (pc && !inputActive()) videoEl.requestPointerLock();
});

// Manual fullscreen toggle — fullscreens the wrapper (#stage), NOT the <video>
// (fullscreening the video makes the browser overlay native media controls).
$("fullscreen").addEventListener("click", async () => {
  try {
    if (!document.fullscreenElement) await $("stage").requestFullscreen();
    else await document.exitFullscreen();
  } catch (e) {}
});

// On-screen controls for inputs the browser otherwise swallows. The browser
// consumes the physical Esc key to release pointer lock, so it never reaches the
// game — and there's no keyboard chord for the Big Picture menu. These buttons
// inject SYNTHETIC events over the input data channel (server-side uinput), which
// the browser never sees, so they work over plain HTTP without the Keyboard Lock
// API. Click them with the mouse free (i.e. after Esc has released the capture).

// Tap a key: down now, up shortly after (the host treats a hold as one down).
function tapKey(code, ms = 80) {
  sendInput({ t: "k", code, down: true });
  setTimeout(() => sendInput({ t: "k", code, down: false }), ms);
}

// Tap one W3C-standard gamepad button index on the virtual pad (the host creates
// the uinput controller lazily on first gamepad event). Index 16 = Guide/Steam,
// which opens the Big Picture menu.
function tapGamepadButton(index, ms = 120) {
  const press = Array.from({ length: 17 }, (_, i) => (i === index ? 1 : 0));
  const release = new Array(17).fill(0);
  sendInput({ t: "g", a: [0, 0, 0, 0], b: press });
  setTimeout(() => sendInput({ t: "g", a: [0, 0, 0, 0], b: release }), ms);
}

$("esc").addEventListener("click", () => tapKey("Escape"));
$("steam").addEventListener("click", () => tapGamepadButton(16));

document.addEventListener("pointerlockchange", () => {
  const locked = inputActive();
  // Show the captured cursor only while we hold the pointer.
  sendInput({ t: "c", on: locked });
  updateHint();
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
  if (!inputActive()) return;
  e.preventDefault();
  if (e.repeat) return; // hold = one down; the host handles auto-repeat
  sendInput({ t: "k", code: e.code, down: true });
});

document.addEventListener("keyup", (e) => {
  if (!inputActive()) return;
  e.preventDefault();
  sendInput({ t: "k", code: e.code, down: false });
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
