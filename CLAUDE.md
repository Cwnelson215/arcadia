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

**Stage 4 (run real games — launcher) — WORKING (2026-06-11).**
Turns arcadia from "streams a throwaway Xvfb test display" into "pick Steam / an
emulator from a web menu and play it." The capture→encode→WebRTC→input pipeline
is **unchanged** — the work is a launcher plus pointing capture at a *real*
display. **Verified end-to-end on bulbasaur from a tailnet client (2026-06-11):**
accelerated Xorg `:99` (radeonsi/renoir), launcher runs games, and **RetroArch +
Steam (Big Picture, logged in) both stream and take keyboard/mouse/gamepad input**
— no window manager needed.
- **Still pending / deferred:** `--audio pulse` end-to-end (arcadia currently runs
  `--audio none`; switch to `scripts/run-session.sh` for sound); **gamescope** (NOT
  in Debian trixie repos — `apt-cache policy gamescope` = no candidate; would need
  a source build/Flatpak, and it isn't needed since Big Picture/RetroArch `-f` go
  fullscreen on their own); **Esc-to-the-game** (needs real HTTPS — see the
  Esc-deferred gotcha); password gate + truly-public access.
- **HTTPS / `tailscale serve`:** a `tailscale serve --bg 8080` config exists
  (`https://bulbasaur.tail71e22f.ts.net` → `127.0.0.1:8080`) but the cert came back
  **self-signed** (HTTPS-certs provisioning incomplete). **Use the plain
  `http://100.78.86.4:8080` URL** for now; finish HTTPS only when re-adding Esc.
- **Launcher:** `src/launcher.rs` — a `games.toml` registry + single-slot process
  supervisor. Spawns one game at a time onto the capture display with
  `DISPLAY=:99` + `PULSE_SERVER=127.0.0.1`; launching another stops the first.
  Optional per-game `gamescope = true` wraps it in `gamescope -W 1280 -H 720 -f
  -- <cmd>`. Exposed by the axum server (`src/signaling.rs`) as HTTP
  `GET /api/games`, `POST /api/launch {id}`, `POST /api/stop` — independent of any
  WebRTC session. Web menu in `web/`. Run via `scripts/run-session.sh`.
- **Access stays on Tailscale** (client runs the Tailscale app). Truly-public
  access (router port-forward + DDNS + TURN) and a password gate are **deferred**
  — a public path can't carry the WebRTC UDP media without TURN, and there's no
  router access today.
- **The hard prerequisite — a GPU-accelerated headless display.** Xvfb is
  software-only (llvmpipe), so real games — and gamescope itself, which needs
  Vulkan — can't run on it. Stage 4 requires a real **Xorg on `:99` driven by
  `amdgpu`** so games render on the Vega iGPU. The existing pipeline captures it
  unchanged; gamescope then runs *nested* on top (only possible once the display
  is accelerated). Pure headless-gamescope-on-DRM with a custom GStreamer capture
  sink (what Wolf does) is deliberately **out of scope**.

  **Host setup (non-persistent — re-do after reboot unless made a service):**
  ```
  # Display/GPU stack + RetroArch are all in trixie `main`. `xdotool` is used by
  # the launcher's `fit_window` to clamp oversized windows (Steam Big Picture).
  sudo apt install -y xserver-xorg xserver-xorg-video-amdgpu \
       mesa-utils vulkan-tools libvulkan1 mesa-vulkan-drivers retroarch xdotool
  # Steam (non-free, needs the i386 arch) — Debian's package is `steam-installer`.
  # bulbasaur's trixie sources ship only `main non-free-firmware`, so add non-free:
  sudo sed -i 's/main non-free-firmware/main contrib non-free non-free-firmware/' /etc/apt/sources.list
  sudo dpkg --add-architecture i386
  sudo apt update
  sudo apt install -y steam-installer   # provides `steam`; first run bootstraps + asks login
  # NOTE: `gamescope` is NOT in trixie repos (apt-cache policy gamescope = no
  # candidate). Skipped — Big Picture / RetroArch `-f` fullscreen without it.
  # let the user run Xorg headless (no seat/logind):
  printf 'allowed_users=anybody\nneeds_root_rights=yes\n' | sudo tee /etc/X11/Xwrapper.config
  # accelerated virtual-head config (no monitor attached):
  sudo tee /etc/X11/xorg-arcadia.conf >/dev/null <<'EOF'
  Section "Device"
      Identifier "amd"
      Driver "amdgpu"
      Option "AllowEmptyInitialConfiguration" "true"
  EndSection
  Section "Monitor"
      Identifier "vmon"
      Modeline "1280x720" 74.50 1280 1344 1472 1664 720 723 728 748 -hsync +vsync
      Option "Enable" "true"
  EndSection
  Section "Screen"
      Identifier "scr"
      Device "amd"
      Monitor "vmon"
      DefaultDepth 24
      SubSection "Display"
          Depth 24
          Modes "1280x720"
          Virtual 1280 720
      EndSubSection
  EndSection
  EOF
  ```
  `scripts/run-session.sh` then starts Xorg `:99` with this config, ensures the
  PulseAudio `arcadia` sink, and runs arcadia. **Gate:** `DISPLAY=:99 glxinfo |
  grep -i renderer` must report **radeonsi/AMD**, not `llvmpipe` — llvmpipe means
  the GPU isn't bound and games are unplayable. Also confirm `DISPLAY=:99
  vulkaninfo` lists the Vega device (gamescope needs it).

  **Stage-4 gotchas (expect host iteration):**
  - **amdgpu with no monitor may refuse to set a mode.** The virtual Modeline +
    `AllowEmptyInitialConfiguration` above is the fix; if Xorg still won't come
    up (see `/tmp/xorg-arcadia.log`), a cheap **HDMI dummy plug** forces a real
    connector, or fall back to the `modesetting` driver.
  - **Running Xorg as the user needs `Xwrapper.config`** (above) — else `Xorg :99`
    aborts with "only console users are allowed to run the X server".
  - **A setuid-root Xorg rejects an absolute `-config` path** (security): pass the
    **basename** (`-config xorg-arcadia.conf`); Xorg searches `/etc/X11`. An
    absolute path silently falls back to autodetect (llvmpipe). `run-session.sh`
    already does this.
  - **A leftover `Xvfb :99`** from Stage 1 blocks Xorg with "Server is already
    active for display 99" — and its software GL is what makes `glxinfo` read
    `llvmpipe`. `run-session.sh` now kills stale Xvfb + clears `/tmp/.X99-lock`
    first. **Verified 2026-06-11:** with this cleared, `:99` reports
    `AMD Radeon Graphics (radeonsi, renoir)` and the launcher runs glxgears on
    the GPU.
  - **Steam's first run is interactive** (login). Do it once through the stream
    (launch "Steam", Connect, log in) or a local session before it's headless-usable.
  - **Input tuning (2026-06-11): keyboard/mouse moved XTEST → uinput.** Under the
    real Xorg, XTEST and `ximagesrc` grabs both serialize on the single-threaded X
    server, so input bursts stuttered the video. `src/input.rs` now injects
    keyboard + mouse via uinput virtual devices (off the X request path), same as
    the gamepad. Same pass: the capture colorspace convert moved CPU
    `videoconvert` → GPU `vapostproc` (VAMemory NV12) in `src/pipeline.rs`.
    (uinput needs `/dev/uinput` writable — udev rule + `input` group, already set
    up in Stage 3.)
  - **Steam Big Picture window overflow (2026-06-11):** Big Picture (`-gamepadui`,
    the Steam Deck UI) opens a **fixed 1280×800** window — the Deck's native 16:10
    resolution — regardless of the display mode (`xrandr` shows `:99` still at
    1280×720; Steam does **not** mode-switch, so restricting the Xorg mode list is a
    non-fix). With no WM to constrain it, the bottom 80px falls off the 1280×720
    framebuffer and is never captured (reads as the UI's edges being cut off). Fix:
    the per-game **`fit_window`** field in `games.toml` (`= "Steam Big Picture"`).
    The launcher (`src/launcher.rs` `fit_window_async`) polls via **`xdotool`** for
    ~30s after launch and resizes/moves the matching window to 1280×720+0+0;
    steamwebhelper (CEF) reflows the Deck UI to fit. Needs `xdotool` on the host
    (in the Stage-4 apt line). If CEF ever stops reflowing, the fallback is a
    1280×800 Xorg modeline + client letterbox (`<video>` already `object-fit:
    contain`).
  - **Reconnect loop = the service lost the `render` group (2026-06-11).** Symptom:
    the browser connects then immediately reconnect-loops; logs show
    `gst::parse::launch failed: link has no sink` and
    `cannot retrieve class for invalid (unclassed) type '<invalid>'`. Cause: the
    `arcadia` **`systemd --user`** service was running **without the `render`
    (992) and `input` (996) groups** (compare `cat /proc/$(pgrep -f
    target/debug/arcadia)/status | grep Groups` vs `id`). Without `render` the
    GStreamer **VA plugin can't open `/dev/dri/renderD128` at load**, so
    `vah264enc`/`vapostproc` never register → the pipeline can't build → every
    connect fails. The *same pipeline string runs fine under `gst-launch-1.0` in
    an interactive shell* (which has the groups) — that contrast is the tell.
    A long-lived `systemd --user` manager **caches a stale group set** if
    render/input were granted after it started; `systemctl --user restart arcadia`
    does **not** refresh it. Fix: `sudo systemctl restart user@$(id -u).service`
    (or reboot), then restart arcadia. `scripts/run-session.sh` now **preflights**
    render+input and exits with this remedy instead of looping silently.
  - **`steam` not on the service PATH (2026-06-11).** `launch failed: spawning
    'steam' … No such file or directory`: Debian's `steam-installer` puts the
    binary at **`/usr/games/steam`**, and `/usr/games` is absent from a
    `systemd --user` service's PATH (present only in interactive login shells).
    The launcher (`src/launcher.rs` `spawn`) now appends
    `/usr/local/games:/usr/games` to each game's child PATH.
  - **Cursor (2026-06-11):** the captured X cursor is shown only while the browser
    holds pointer lock — `web/main.js` sends `{t:"c",on}` on `pointerlockchange`
    and `src/pipeline.rs` toggles `ximagesrc cap.show-pointer` live (named element
    `cap`; default off). Capture is plain **windowed pointer lock**; **Esc
    releases**. Games launch fullscreen to fill the 1280×720 display (e.g.
    RetroArch `-f` in `games.toml`); no WM is needed (Steam Big Picture + RetroArch
    both take input under bare PointerRoot focus, verified 2026-06-11).
  - **Esc-to-the-game — DEFERRED (don't re-add over HTTP).** Intercepting Esc so it
    reaches the game needs the browser **Keyboard Lock API**, which requires *both*
    fullscreen *and* a **secure (HTTPS) context** — over plain `http://<ip>:8080`
    `navigator.keyboard` is undefined and it silently no-ops. An attempt
    (always-fullscreen + Keyboard Lock + an Esc+Backspace release combo, fronted by
    `tailscale serve` HTTPS) destabilized the WebRTC session (reconnect churn → the
    data channel kept dropping → dead input) and was **reverted** to the simple
    capture. Revisit only once arcadia is served over *real* HTTPS: enable HTTPS
    certs in the tailnet, `sudo tailscale cert bulbasaur.tail71e22f.ts.net`, then
    `sudo tailscale serve --bg 8080`, and load `https://bulbasaur.tail71e22f.ts.net/`.
    `web/index.html` loads `main.js?v=N` — **bump N when changing the client** to
    dodge browser caching (a stale cached client caused an hour of "input dead").
  - **On-screen Back/Steam buttons (2026-06-11) — the HTTP-friendly substitute.**
    Because the browser eats physical Esc (above), there was no way to send Back or
    open the Big Picture menu while playing Steam. Fix that does *not* need HTTPS:
    toolbar buttons in `web/index.html` (`#esc`, `#steam`) that inject **synthetic**
    events over the existing input data channel (so the browser never sees them).
    `web/main.js` `tapKey("Escape")` → `{t:"k"}` → uinput `KEY_ESC` (Big Picture
    "Back"); `tapGamepadButton(16)` → `{t:"g"}` with W3C button 16 → uinput
    `BTN_MODE` (Guide → opens the Steam menu; creates the virtual pad lazily, so a
    phantom controller appears in Steam — harmless). Click them with the mouse free
    (after Esc has released the capture). This is independent of, and does not
    replace, the deferred *physical*-Esc passthrough above.
  - **Browser quirk:** fullscreen targets a wrapper `<div id="stage">`, **not** the
    `<video>`. Fullscreening the `<video>` element makes Chrome overlay native
    media controls (a pause button + running timer in the corner) — which looks
    like an in-stream OSD but isn't. Diagnosed by grabbing `:99` directly
    (`ffmpeg -f x11grab`): the framebuffer was clean, proving it was client-side.
  - **RetroArch OSD:** its menu **widgets** (a pause/clock overlay) and the menu
    clock are on by default. Disabled in `~/.config/retroarch/retroarch.cfg` on
    bulbasaur: `menu_widgets_enable=false`, `menu_timedate_enable=false`,
    `menu_battery_level_enable=false` (host config, not in the repo).

**Stage 3 (make it feel good) — DONE (2026-06-11).** Latency, gamepad,
reconnect+bitrate, and **audio** (added same day).
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
- **Audio:** `--audio none|test|pulse` (+ `--audio-device`, default `arcadia.monitor`).
  Adds a second media branch into the same `webrtcbin` via the `sendrecv.`
  reference (`opusenc ! rtpopuspay ! application/x-rtp,...,encoding-name=OPUS,
  payload=97 ! sendrecv.`), negotiated as a second `m=audio OPUS/48000/2` m-line.
  `test` = a 440 Hz tone (proves the path); `pulse` = `pulsesrc device=<monitor>`
  capturing real desktop audio. Browser unmutes the `<video>` on `ontrack` (the
  Connect click is the autoplay gesture). Pipeline string built in
  `src/pipeline.rs` `build_pipeline`.

  **Real audio needs PulseAudio (host setup, non-persistent — re-do after reboot):**
  ```
  sudo apt install -y pulseaudio pulseaudio-utils
  pulseaudio --start --exit-idle-time=-1
  pactl load-module module-null-sink sink_name=arcadia sink_properties=device.description=arcadia
  pactl set-default-sink arcadia                       # apps now output here
  pactl load-module module-native-protocol-tcp auth-ip-acl=127.0.0.1
  ```
  Run arcadia with `PULSE_SERVER=127.0.0.1 ... --audio pulse`. **Gotcha:**
  `XDG_RUNTIME_DIR` is **empty** over Tailscale SSH (no PAM session dir), so the
  default `$XDG_RUNTIME_DIR/pulse/native` socket isn't reliably found across
  sessions — hence the **TCP module + `PULSE_SERVER=127.0.0.1`** (also stops
  arcadia from autospawning its own empty daemon). The game/app must output to
  the `arcadia` sink (it's the default), which feeds `arcadia.monitor`.

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
- Dev loop / deploy: push to `main` → GitHub Actions (`.github/workflows/ci.yml`)
  runs clippy+build on a runner, then joins the tailnet and syncs+builds+restarts
  on bulbasaur over Tailscale SSH (the `arcadia` systemd **user** service, unit at
  `deploy/arcadia.service`). For ad-hoc manual syncs (bulbasaur has **no `rsync`**):
  `tar czf - --exclude=.git --exclude=target . | ssh cwnelson@bulbasaur 'tar xzf - -C arcadia'`,
  then `cargo run` there. Encode+capture are host-specific; never validate on the
  workstation.

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
