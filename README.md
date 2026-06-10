# arcadia

A self-hosted, Stadia-style cloud-gaming service for the home lab: run a game on
**bulbasaur**, hardware-encode the video, stream it over **Tailscale** to a
remote device (browser/laptop/phone), and send control inputs back to play.

Built from scratch as a learning project — the goal is to understand the
cloud-gaming pipeline end to end, not just run an off-the-shelf host. (The
off-the-shelf path, Sunshine + Moonlight, is used only as a Stage-0 baseline to
de-risk the hardware and set a latency target to beat.)

## Status

**Stage 1 (one-way video) — DONE (2026-06-10).** A Rust binary
(`cargo run -- --source test|x11`) captures a source, hardware-encodes it to
H.264 via VA-API, and streams it over WebRTC to a zero-install browser on a
tailnet device. Both halves verified at **1280×720 @ 60 fps**, ~30–40 ms network
RTT: a synthetic test pattern (`videotestsrc`) and real screen capture
(`ximagesrc` on a headless Xvfb display). Toolkit chosen: **GStreamer
(`webrtcbin`) + Rust (`gstreamer-rs`)**. Next: Stage 2 (input round-trip →
playable). See [`ROADMAP.md`](./ROADMAP.md) for the full staged plan.

**Stage 0 (prove the hardware) — DONE (2026-06-10).** The Vega iGPU exposes
hardware H.264 (Constrained Baseline / Main / High) and HEVC (Main / Main10)
encode via VA-API (`radeonsi`); ffmpeg drove a `h264_vaapi` test encode; and
`/dev/uinput` is present for later input injection.

## Run it

On bulbasaur (after `scripts/sync.sh` from the workstation):

```bash
cargo run -- --source test                 # synthetic test pattern
scripts/run-x11.sh                          # Xvfb + glxgears, then --source x11
```

then open `http://100.78.86.4:8080` from a tailnet device and click **Connect**.

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
