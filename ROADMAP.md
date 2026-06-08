# arcadia — feasibility & build roadmap

> Copied from the approved plan
> `~/.claude/plans/if-i-wanted-to-jolly-whisper.md`. This is the working
> reference for the project.

## Context

A Stadia-like service: run a game on bulbasaur, stream the video to a remote
device, send control inputs back. Built **from scratch as a learning project**,
reached **over Tailscale**.

The honest headline: **this is a solved problem** (Sunshine+Moonlight, and
Wolf/Fenrir for the container-native version), so building it from scratch is a
*choice to learn*, not a necessity. A working single-user prototype is a
**moderate** effort (a few focused weekends). A polished, production-grade
service is **hard** (months) and would amount to re-implementing Sunshine/Wolf.
The good news is the two things that usually kill a home build — the video
encoder and internet reachability — are both already handled here.

## Hardware reality (verified on bulbasaur)

| | |
|---|---|
| APU | AMD Ryzen 5 PRO 4650G "Renoir", 6c/12t |
| iGPU | Radeon Vega 7, `amdgpu` driver, `/dev/dri/renderD128` |
| HW encode | **VCN 2.1: H.264 + HEVC encode via VA-API** (no AV1 encode) |
| RAM | 30 GiB (iGPU shares system RAM — plenty of headroom) |
| Disk | 238 GB NVMe (mostly consumed by k3s local-path PVCs) |

**Implication:** hardware encode (the latency-critical stage) is available. The
iGPU caps *which games run well* (1080p60 for lighter titles; AAA will be
GPU-bound), not whether streaming works.

## Why "over Tailscale" is the big unlock

The hard part of internet game streaming is usually NAT traversal + the
unresolved router/port-forward situation. Tailscale removes it entirely:
- Client device joins the tailnet → bulbasaur is reachable at `100.78.86.4`,
  encrypted, no port-forward, no STUN/TURN server needed.
- WebRTC can advertise the Tailscale interface as a host ICE candidate and
  connect directly — you skip the entire ICE/TURN apparatus most builds fight.
- Trade-off: every client must be on the tailnet (fine for personal use; not a
  public "anyone can play" service).

## What gets built (the 5 components)

Standard cloud-gaming pipeline. The "from scratch" line sits **above the codec
and RTP stack** — write the orchestration, not a new H.264 encoder.

1. **Capture** — game renders to a headless display; grab frames.
   - Start simple: `Xvfb` virtual display + `ffmpeg x11grab`.
   - Better: **gamescope** (Valve's headless micro-compositor — purpose-built
     for nested capture/encode; the likely end state).
2. **Encode** — **VA-API H.264** on the Vega VCN, tuned for low latency
   (CBR, GOP≈framerate or intra-refresh, zero B-frames). Latency is won/lost here.
3. **Transport** — **WebRTC** (RTP/SRTP, jitter buffer, FEC, congestion control
   for free; browser client needs nothing installed). Over Tailscale, no TURN.
4. **Input** — client captures keyboard/mouse (Pointer Lock API) + gamepad
   (Gamepad API) → WebRTC **data channel** → server injects via **`uinput`**
   (virtual input device) or `ydotool`.
5. **Signaling/session** — small web server (Node or Go) for the SDP/ICE
   handshake, a game list, and launching the game process.

**Recommended toolkit:** **GStreamer** (`webrtcbin` + `vah264enc` + capture) or
**Pion** (Go WebRTC). Both let you assemble and understand every stage without
reinventing codecs or RTP.

## Staged roadmap (each stage produces something that works)

- **Stage 0 — prove the hardware (½ day).** `vainfo` confirms
  `VAProfileH264 ... EncSlice`. Do a test VA-API encode with ffmpeg. Run a game
  headless under gamescope/Xvfb and confirm it renders.
- **Stage 0.5 — baseline with Sunshine+Moonlight (½ day, optional but smart).**
  Install Sunshine on bulbasaur, Moonlight on a tailnet device. Proves the whole
  path end-to-end and gives a **latency number to beat** before building.
- **Stage 1 — one-way video.** Capture → VA-API encode → WebRTC → browser
  `<video>`. No input yet. Measure glass-to-glass latency.
- **Stage 2 — input round-trip → playable.** Browser input → data channel →
  `uinput` injection. First time it's actually a game console.
- **Stage 3 — make it feel good.** Latency tuning, audio track
  (PipeWire → Opus → WebRTC), gamepad, reconnect, basic bitrate adaptation.
- **Stage 4 — cluster-native (optional, ties into the migration).** Containerize:
  mount `/dev/dri` into the pod (or an AMD device plugin), gamescope in-container,
  per-session pod launched by the signaling server. Compare against **Wolf +
  Fenrir**, which already do exactly this on Kubernetes.

## Difficulty summary

| Target | Difficulty | Rough effort |
|---|---|---|
| Run existing stack (Sunshine+Moonlight over Tailscale) | Easy | A weekend |
| Build a working single-user prototype (Stages 1–2) | Moderate | A few weekends |
| Polished single-user (Stage 3) | Moderate–Hard | Several weeks |
| Multi-user / production (re-doing Wolf) | Hard | Months |

**Where it bites:** low-latency encoder tuning, input-injection plumbing
(`uinput` permissions, gamepad mapping), audio/video sync. **Where you catch a
break:** hardware encode exists, Tailscale kills NAT, WebRTC gives you the hard
networking pieces, and the iGPU — while modest — is genuinely capable for the
kind of games this makes sense for.

## Verification (per stage)

- **Stage 0:** `vainfo` lists H.264 EncSlice; `ffmpeg -hwaccel vaapi ... h264_vaapi`
  produces a file; a game window appears in the headless display.
- **Stage 1:** video plays in a browser on a tailnet device; measure latency
  (on-screen clock filmed alongside the stream, or `getStats()` RTT).
- **Stage 2:** inputs in the browser move the game; measure input lag.
- **Stage 3:** audio in sync; gamepad works; survives a reconnect; latency stays
  under the Stage-0.5 Sunshine baseline.

## Notes / decisions still open

- Pick the build toolkit: **GStreamer `webrtcbin`** vs **Pion (Go)** vs
  **aiortc (Python)**. (Recommendation: GStreamer for max learning-per-stage,
  Pion to live in Go.)
- If this graduates past a prototype it follows the existing per-app pattern
  (own repo, Dockerfile, k8s manifests) under `~/Dev/portfolio/`.
- Run Sunshine first (Stage 0.5) even though the goal is a custom build — it
  de-risks the hardware path and sets a target.
