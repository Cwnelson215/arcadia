# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with
code in this repository.

## What This Repo Is

**arcadia** — a self-hosted, Stadia-style cloud-gaming service for the home lab.
It runs a game on **bulbasaur**, hardware-encodes the video, streams it over
**Tailscale** via **WebRTC** to a remote browser client, and sends control
inputs back so the game is playable remotely.

This is a **learning project built from scratch** — the goal is to understand
the cloud-gaming pipeline (capture → encode → transport → input injection), not
to ship a product. Off-the-shelf hosts (Sunshine + Moonlight) are used only as a
Stage-0 baseline. The full plan, difficulty assessment, and staged roadmap live
in [`ROADMAP.md`](./ROADMAP.md) — **read it first**.

## Current status

**Build toolkit — DECIDED (2026-06-10): GStreamer (`webrtcbin`) + Rust
(`gstreamer-rs`).** All pipeline code lives in `src/` (a Cargo binary). Do not
reintroduce Pion/aiortc.

**Stage 3 (make it feel good) — DONE (2026-06-11).** User picked latency,
gamepad, reconnect+bitrate; **audio deferred**.
- **Latency tuning:** `vah264enc target-usage=7`, `webrtcbin latency=40` (from the
  200 ms default), browser sets the video receiver's `playoutDelayHint=0`.
  Measured jitter-buffer ~9 ms, smooth 60 fps. (`fps` in `getStats` reads low on a
  *static* screen — it counts rendered frames; it climbs to 60 under motion.)
- **Gamepad:** browser Gamepad API (`web/main.js`, polled via rAF, no Pointer
  Lock needed → works on mobile + BT controller) → `{t:"g",a,b}` on the data
  channel → `src/gamepad.rs` injects a **uinput virtual "Microsoft X-Box 360 pad"**
  (`evdev` crate, VID/PID 0x045e/0x028e, ABS X/Y/RX/RY/Z/RZ/HAT0X/Y + BTN_*) on its
  own thread, created lazily on first event. Gamepads go via **uinput, not XTEST**
  (XTEST can't fake a joystick) — and uinput joysticks are visible to SDL games
  even under Xvfb. Verify headlessly with `arcadia --gamepad-selftest` + `jstest`.
- **Reconnect + bitrate:** `web/main.js` auto-reconnects with capped backoff
  (server already builds a fresh pipeline per WS). Bitrate adaptation is a
  **browser-side AIMD heuristic** (loss>2% → ×0.85; loss<0.5% → +500 kbps, cap
  15 Mbps) sent as `{t:"r",kbps}`; server clamps (1–20 Mbps) and sets
  `vah264enc bitrate` live. (No `rtpgccbwe`/GCC — it's only in `gst-plugins-rs`.)

**Stage-3 gotchas:**
- **uinput needs a udev rule + `input` group.** `/dev/uinput` is `root:root 0600`;
  rule `/etc/udev/rules.d/99-uinput.rules` = `KERNEL=="uinput", GROUP="input",
  MODE="0660"`, `usermod -aG input cwnelson`. **Order matters:** create the rule
  *before* `udevadm trigger`; the static `/dev/uinput` node may not pick up the
  rule on trigger alone — `modprobe -r uinput && modprobe uinput` recreates it as
  `root:input 0660`. Group membership needs a fresh login (each `ssh`/`cargo run`
  is fresh, so it applies). Host tools: `joystick` (`jstest`), `evtest`.

**Stage 2 (input round-trip → playable) — DONE (2026-06-11).** Browser captures
keyboard + mouse and sends events over a WebRTC **data channel**; the server
injects them into the headless X display via **X11 XTEST** (`x11rb`, in-process,
no root). Verified: typed keys reach an `xterm`, `xeyes` tracks the mouse.
- **Injection = XTEST, not uinput** (decided 2026-06-11). `/dev/uinput` is
  root-only *and* Xvfb doesn't read evdev, so uinput can't reach `:99` apps
  without gamescope. XTEST injects straight into the X server. The browser
  capture + data-channel half is mechanism-agnostic — a later uinput+gamescope
  switch reuses it. Code: `src/input.rs` (XTEST injector on its own thread,
  fed by an mpsc channel; `KeyboardEvent.code` → keysym → keycode map; relative
  mouse via `xtest_fake_input(MOTION_NOTIFY, detail=1, …)`; releases held keys on
  disconnect). Wiring in `src/pipeline.rs`; capture in `web/main.js` (Pointer
  Lock, gated on lock). Scope = keyboard + mouse; **gamepad is Stage 3**.
- Test apps via `scripts/run-x11.sh`: `xterm` (keyboard) + `xeyes` (mouse), no WM
  (PointerRoot focus). Needs `x11-apps`.

**Stage-2 gotchas (all cost build/debug time):**
- **`webrtcbin` create-data-channel needs the pipeline ≥ READY** — calling it in
  NULL state returns null + `assertion 'is_closed != TRUE' failed`. Set the
  pipeline to READY *before* `create-data-channel`, then PLAYING (READY doesn't
  trigger negotiation, so the channel still rides the first offer).
- **`gstreamer-webrtc::WebRTCDataChannel` is feature-gated** behind `v1_18`+ —
  enable it in `Cargo.toml` (`features = ["v1_22"]`), else the type doesn't exist.
- **`create-data-channel` returns a *nullable* GstWebRTCDataChannel** — emit as
  `emit_by_name::<Option<WebRTCDataChannel>>(…)` or glib panics with the cryptic
  "expected GstWebRTCDataChannel, got GstWebRTCDataChannel".
- **`tracing` reserves `display`/`debug`** — a local variable named `display`
  passed to `info!`/`error!` resolves to `tracing::field::display` and fails to
  compile. Rename the variable.

**Stage 1 (one-way video) — DONE (2026-06-10).** Working end-to-end on bulbasaur:
capture → VA-API H.264 → WebRTC → browser, verified at 1280×720@60 with ~30–40 ms
network RTT, for both `--source test` (videotestsrc) and `--source x11`
(ximagesrc on a headless Xvfb display showing glxgears). Architecture:
- One Rust binary serves the static `web/` client, runs a `/ws` WebSocket
  signaling endpoint (axum), and builds the GStreamer pipeline. bulbasaur is the
  WebRTC **offerer**; the browser is a zero-install answerer. See `src/main.rs`,
  `src/signaling.rs`, `src/pipeline.rs`; client in `web/`.
- Pipeline: `<source> ! videoconvert ! NV12 ! vah264enc rate-control=cbr
  bitrate=15000 key-int-max=30 b-frames=0 ! constrained-baseline ! h264parse !
  rtph264pay config-interval=-1 mtu=1200 ! webrtcbin`.
- Dev loop: edit on workstation → `scripts/sync.sh` (tar-over-ssh; **bulbasaur
  has no `rsync`**) → `cargo run` on bulbasaur. Encode+capture are host-specific;
  never validate on the workstation.

**⚠️ The Stage-1 gotcha that cost the most time — Tailscale MTU.** `tailscale0`
has a **1280-byte MTU**, but `rtph264pay` defaults to `mtu=1400`. Large keyframe
(IDR) RTP packets then exceed the path MTU, get IP-fragmented over WireGuard, and
lose fragments — so the browser receives bytes and small P-frames but **never
reassembles a keyframe** (symptom: `connectionState=connected`, `bytesReceived>0`,
but `keyFramesDecoded=0` and a permanently black `<video>`). Fix: **`rtph264pay
mtu=1200`**. This will apply to every WebRTC-over-Tailscale stream in this project.

**Stage 0 (prove the hardware) — DONE (2026-06-10).** Verified with
`scripts/stage0-check.sh`: render node present, `amdgpu` bound, VA-API encode
entrypoints (H.264 Constrained Baseline/Main/High + HEVC Main/Main10,
`VAEntrypointEncSlice` via `radeonsi`), a working ffmpeg `h264_vaapi` encode, and
`/dev/uinput` present.

**Host toolchain installed for Stage 1 (Debian, via apt):**
`build-essential`, `rustup` (stable; the toolchain had to be reinstalled once —
a half-installed stable was missing its manifest), the GStreamer dev + runtime
stack (`libgstreamer1.0-dev`, `-plugins-base/-bad` dev, `gstreamer1.0-plugins-
base/good/bad`, **`gstreamer1.0-nice`** = libnice for webrtcbin ICE,
`gstreamer1.0-tools`), and `xvfb`/`xterm`/`mesa-utils` for `--source x11`.
GStreamer is **1.26.2**; the `va` plugin element is `vah264enc` (not
`vah264lpenc`), with props `rate-control`/`bitrate`(kbps)/`key-int-max`/
`b-frames`. Sudo over SSH needs a password (not passwordless).

**Stage 0 remediation done on bulbasaur (host state changed):**
- Installed `vainfo`, `mesa-va-drivers`, and `ffmpeg` (Debian apt).
- Added user `cwnelson` to the `render` and `video` groups. **This was the real
  blocker** — `/dev/dri/renderD128` is `root:render 0660`, so without `render`
  membership both `vainfo` and ffmpeg failed with "Failed to open the given
  device" / "No VA display found", even though the driver was correctly
  installed. Requires a fresh login session to take effect.
- **Gotcha for Stage 4 (containerizing):** the same render-group access applies
  inside a pod — the container must run with the host `render` GID supplementary
  group (or use an AMD device plugin) to open the render node. Existence of the
  device alone is not enough.

## Architecture (target)

The pipeline has five components (see `ROADMAP.md` for detail):

1. **Capture** — game on a headless display (`gamescope` preferred, `Xvfb` to start).
2. **Encode** — **H.264 via VA-API** on the AMD Vega iGPU, tuned for low latency
   (`/dev/dri/renderD128`, VCN 2.1, CBR, zero B-frames).
3. **Transport** — **WebRTC** over Tailscale. No TURN/STUN needed — the tailnet
   host IP works as a direct ICE candidate.
4. **Input** — browser (Pointer Lock + Gamepad API) → WebRTC data channel →
   server injects via `uinput`.
5. **Signaling/session** — small web server for SDP/ICE handshake + game launch.

## Host: bulbasaur (the only place this runs)

- AMD Ryzen 5 PRO 4650G "Renoir" APU, Radeon Vega 7 iGPU, **VCN 2.1 hardware
  H.264/HEVC encode** (no AV1 encode), 30 GiB RAM.
- Reached over Tailscale: `ssh cwnelson@bulbasaur` / `100.78.86.4`.
- Full host detail is in the root `~/Dev/portfolio/CLAUDE.md`.
- **Gotcha:** Tailscale SSH may require an interactive auth approval the first
  time per session. If a `ssh cwnelson@bulbasaur ...` command hangs on a
  `https://login.tailscale.com/a/...` URL, the user must approve it (suggest they
  run the command themselves with the `!` prefix).

## Conventions

- Streaming/encoder work is hardware-specific to this host — test on bulbasaur,
  not on the workstation (the workstation's GPU is not the target).
- Keep the "from scratch" line above the codec/RTP layer: assemble the pipeline,
  don't reimplement encoders or the RTP stack.
- If this graduates past a prototype, follow the portfolio per-app pattern
  (Dockerfile, k8s `base/` + `overlays/prod/`, GitHub Actions → GHCR →
  `kubectl apply -k`) — see `detailing/` as the reference implementation.
