# arcadia

A self-hosted, Stadia-style cloud-gaming service for the home lab: run a game on
**bulbasaur**, hardware-encode the video, stream it over **Tailscale** to a
remote device (browser/laptop/phone), and send control inputs back to play.

Built from scratch as a learning project — the goal is to understand the
cloud-gaming pipeline end to end, not just run an off-the-shelf host. (The
off-the-shelf path, Sunshine + Moonlight, is used only as a Stage-0 baseline to
de-risk the hardware and set a latency target to beat.)

## Status

**Stage 0 — not started.** See [`ROADMAP.md`](./ROADMAP.md) for the full staged
plan and difficulty assessment.

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
