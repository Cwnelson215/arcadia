#!/usr/bin/env bash
# arcadia — real-game session. Run this ON bulbasaur (needs the local GPU).
#
# Brings up the GPU-accelerated headless display (:99 via amdgpu — NOT Xvfb,
# which is software-only and can't run real games), ensures the PulseAudio
# `arcadia` sink, then runs arcadia with the launcher enabled. Games are started
# from the web UI (/api/launch), not here.
#
#   ssh cwnelson@bulbasaur 'cd arcadia && scripts/run-session.sh'
# then open http://100.78.86.4:8080 from a tailnet device.
#
# One-time host setup (Xorg config, Xwrapper.config, steam/retroarch) is in
# arcadia/CLAUDE.md under "Stage 4". Like run-x11.sh, detached processes use
# `setsid -f` (a plain `&` over SSH gets SIGHUP'd); arcadia runs in the
# foreground so Ctrl-C stops the stream (the game keeps running until /api/stop).
set -uo pipefail

# Group preflight: arcadia MUST hold `render` (GPU /dev/dri/renderD128) and
# `input` (/dev/uinput). Without `render`, the GStreamer VA plugin can't open the
# GPU at load, so `vah264enc`/`vapostproc` never register — the WebRTC pipeline
# then fails to build on every connect and the browser reconnect-LOOPS (no error
# in the UI). Without `input`, keyboard/mouse/gamepad injection silently dies.
# A long-lived `systemd --user` manager carries a STALE group set if these were
# granted after it started (a plain `systemctl --user restart arcadia` won't fix
# it). Fail fast with the remedy instead of looping silently.
missing=""
for g in render input; do
  id -nG | tr ' ' '\n' | grep -qx "$g" || missing="${missing} ${g}"
done
if [ -n "${missing}" ]; then
  echo "FATAL: arcadia is missing group(s):${missing}" >&2
  echo "  These are required for the VA encoder (render) and uinput (input)." >&2
  echo "  1. Ensure membership:  sudo usermod -aG render,input ${USER}" >&2
  echo "  2. Refresh the (stale) systemd --user groups WITHOUT a reboot:" >&2
  echo "        sudo systemctl restart user@$(id -u).service" >&2
  echo "     then:  systemctl --user restart arcadia   (or just reboot bulbasaur)" >&2
  exit 1
fi

DISPLAY_NUM="${DISPLAY_NUM:-:99}"
# Basename only: a setuid-root Xorg rejects an absolute -config path and searches
# trusted dirs (/etc/X11, ...). The file lives at /etc/X11/xorg-arcadia.conf.
XORG_CONF="${XORG_CONF:-xorg-arcadia.conf}"
BIND="${BIND:-0.0.0.0:8080}"
# Perf knobs (override per-session, e.g. `FPS=90 scripts/run-session.sh`).
# FPS > 60 needs a matching Xorg modeline at that refresh in xorg-arcadia.conf
# (see CLAUDE.md). LATENCY is webrtcbin's jitter buffer in ms (40 is WAN-safe).
# IDLE_TIMEOUT = seconds with no viewer before the running game is frozen
# (SIGSTOP) to free the GPU; 0 disables.
FPS="${FPS:-60}"
LATENCY="${LATENCY:-40}"
IDLE_TIMEOUT="${IDLE_TIMEOUT:-120}"

# 1. Accelerated Xorg on :99 (GPU-backed via amdgpu, unlike Xvfb).
if ! pgrep -f "Xorg ${DISPLAY_NUM}" >/dev/null; then
  # A leftover Xvfb on the same display (or a stale lock) would block Xorg with
  # "Server is already active" — clear them first.
  pkill -f "Xvfb ${DISPLAY_NUM}" 2>/dev/null || true
  rm -f "/tmp/.X${DISPLAY_NUM#:}-lock" "/tmp/.X11-unix/X${DISPLAY_NUM#:}"
  echo "== starting Xorg ${DISPLAY_NUM} (accelerated, amdgpu) =="
  setsid -f Xorg "${DISPLAY_NUM}" -config "${XORG_CONF}" -nolisten tcp \
    </dev/null >/tmp/xorg-arcadia.log 2>&1
  sleep 3
  pgrep -f "Xorg ${DISPLAY_NUM}" >/dev/null \
    || { echo "Xorg failed to start — see /tmp/xorg-arcadia.log"; exit 1; }
fi

# Sanity gate: the display MUST be GPU-accelerated (radeonsi/AMD), not llvmpipe.
if command -v glxinfo >/dev/null; then
  renderer=$(DISPLAY="${DISPLAY_NUM}" glxinfo 2>/dev/null | grep -i 'OpenGL renderer' || true)
  echo "   ${renderer:-OpenGL renderer: (glxinfo failed)}"
  case "$renderer" in
    *llvmpipe*|*softpipe*|"")
      echo "   WARNING: display is NOT GPU-accelerated — games will be unplayable"
      echo "            check /tmp/xorg-arcadia.log and the Xorg config" ;;
  esac
fi

# 2. PulseAudio + the `arcadia` null sink (idempotent).
echo "== ensuring PulseAudio + arcadia sink =="
pulseaudio --check 2>/dev/null || pulseaudio --start --exit-idle-time=-1
if ! pactl list short sinks 2>/dev/null | grep -qw arcadia; then
  pactl load-module module-null-sink sink_name=arcadia \
    sink_properties=device.description=arcadia >/dev/null
fi
pactl set-default-sink arcadia 2>/dev/null || true
pactl list short modules 2>/dev/null | grep -q module-native-protocol-tcp \
  || pactl load-module module-native-protocol-tcp auth-ip-acl=127.0.0.1 >/dev/null

# 3. arcadia with the launcher (games started from the web UI).
echo "== launching arcadia on http://100.78.86.4:8080 (capture ${DISPLAY_NUM}) =="
source "$HOME/.cargo/env"
exec env PULSE_SERVER=127.0.0.1 ./target/debug/arcadia \
  --source x11 --display "${DISPLAY_NUM}" --audio pulse \
  --games-config games.toml --bind "${BIND}" \
  --fps "${FPS}" --latency "${LATENCY}" --idle-timeout "${IDLE_TIMEOUT}"
