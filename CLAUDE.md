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

**Stage 0 (prove the hardware) — not started.** No application code yet; this is
a fresh scaffold (README, roadmap, this file). The build toolkit
(GStreamer vs Pion vs aiortc) is **not yet chosen** — do not write pipeline code
against a specific toolkit until that decision is made.

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
