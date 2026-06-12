# arcadia

A self-hosted, Stadia-style cloud-gaming service for the home lab: run a game on
**bulbasaur**, hardware-encode the video, stream it over **Tailscale** to a
remote device (browser/laptop/phone), and send control inputs back to play.

Built from scratch as a learning project — the goal is to understand the
cloud-gaming pipeline end to end, not just run an off-the-shelf host. (The
off-the-shelf path, Sunshine + Moonlight, is used only as a Stage-0 baseline to
de-risk the hardware and set a latency target to beat.)

## Status

**Stage 3 (make it feel good) — DONE (2026-06-11).** **Latency tuning**
(`vah264enc target-usage=7`, `webrtcbin latency=40`, browser `playoutDelayHint=0`
→ jitter-buffer ~9 ms, smooth 60 fps); **gamepad** (browser Gamepad API → data
channel → a uinput virtual Xbox-360 pad, so SDL games auto-map it — works on
mobile + a Bluetooth controller too); **reconnect + adaptive bitrate** (client
auto-reconnect with backoff; a loss-feedback AIMD loop nudges the encoder
bitrate, since `rtpgccbwe` isn't packaged); and **audio** (`--audio test|pulse` →
Opus → a second WebRTC media stream; real desktop audio is captured from a
PulseAudio null-sink monitor). Next: Stage 4 (cluster-native / gamescope). See
[`ROADMAP.md`](./ROADMAP.md).

**Stage 2 (input round-trip → playable) — DONE (2026-06-11).** The browser
captures keyboard + mouse (Pointer Lock for relative mouse-look) and sends events
over a **WebRTC data channel**; the server injects them into the headless X
display via **XTEST** (`x11rb`), so apps respond. Verified: typed keys land in an
`xterm`, and `xeyes` tracks the mouse. It's playable.

**Stage 1 (one-way video) — DONE (2026-06-10).** A Rust binary
(`cargo run -- --source test|x11`) captures a source, hardware-encodes it to
H.264 via VA-API, and streams it over WebRTC to a zero-install browser on a
tailnet device. Both halves verified at **1280×720 @ 60 fps**, ~30–40 ms network
RTT: a synthetic test pattern (`videotestsrc`) and real screen capture
(`ximagesrc` on a headless Xvfb display). Toolkit chosen: **GStreamer
(`webrtcbin`) + Rust (`gstreamer-rs`)**.

**Stage 0 (prove the hardware) — DONE (2026-06-10).** The Vega iGPU exposes
hardware H.264 (Constrained Baseline / Main / High) and HEVC (Main / Main10)
encode via VA-API (`radeonsi`); ffmpeg drove a `h264_vaapi` test encode; and
`/dev/uinput` is present for later input injection.

## Run it

On bulbasaur (push to `main` deploys via GitHub Actions; for a manual sync:
`tar czf - --exclude=.git --exclude=target . | ssh cwnelson@bulbasaur 'tar xzf - -C arcadia'`):

```bash
cargo run -- --source test                 # synthetic test pattern
scripts/run-x11.sh                          # Xvfb + xterm/xeyes, then --source x11
```

then open `http://100.78.86.4:8080` from a tailnet device, click **Connect**, and
click the video to capture mouse + keyboard (Esc releases).

## The pipeline (what gets built)

1. **Capture** — game renders to a headless display (`gamescope` / `Xvfb`).
2. **Encode** — hardware **H.264 via VA-API** on bulbasaur's AMD Vega iGPU
   (VCN 2.1), tuned for low latency.
3. **Transport** — **WebRTC** over Tailscale (no NAT traversal, no TURN).
4. **Input** — browser captures keyboard/mouse/gamepad → WebRTC data channel →
   server injects via `uinput`.
5. **Signaling/session** — small web server for the SDP/ICE handshake, game
   list, and launching the game.

## Target host

bulbasaur — AMD Ryzen 5 PRO 4650G (Renoir APU, Vega 7 iGPU, VCN 2.1 H.264/HEVC
hardware encode), 30 GiB RAM. Reached over Tailscale (`100.78.86.4`). See the
root `~/Dev/portfolio/CLAUDE.md` for full host details.

## Open decision

The build toolkit is not yet chosen — **GStreamer (`webrtcbin`)** is the
recommended default; **Pion (Go)** and **aiortc (Python)** are alternatives. See
the roadmap's "Notes / decisions still open".
