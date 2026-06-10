#!/usr/bin/env bash
# Stage 1b — real screen capture. Run this ON bulbasaur (it needs the local GPU
# and X display). Starts a headless Xvfb display with a moving test app
# (glxgears), then runs arcadia capturing it via ximagesrc.
#
#   ssh cwnelson@bulbasaur 'cd arcadia && scripts/run-x11.sh'
# then open http://100.78.86.4:8080 from a tailnet device and click Connect.
#
# Note: processes are detached with `setsid -f` — a plain `&` over SSH gets
# SIGHUP'd when the session closes and dies. arcadia itself runs in the
# foreground so Ctrl-C (or closing the SSH session) stops the stream.
set -uo pipefail

DISPLAY_NUM="${DISPLAY_NUM:-:99}"
RES="${RES:-1280x720x24}"

cleanup_prev() {
  pkill -f 'target/debug/arcadia' 2>/dev/null || true
  pkill glxgears 2>/dev/null || true
  pkill -f "Xvfb ${DISPLAY_NUM}" 2>/dev/null || true
  sleep 1
}

echo "== starting Xvfb ${DISPLAY_NUM} (${RES}) =="
cleanup_prev
setsid -f Xvfb "${DISPLAY_NUM}" -screen 0 "${RES}" </dev/null >/tmp/xvfb.log 2>&1
sleep 2

echo "== starting glxgears on ${DISPLAY_NUM} =="
DISPLAY="${DISPLAY_NUM}" setsid -f glxgears </dev/null >/tmp/glxgears.log 2>&1
sleep 2

pgrep -a Xvfb || { echo "Xvfb failed — see /tmp/xvfb.log"; exit 1; }
pgrep -a glxgears || echo "warning: glxgears not running — see /tmp/glxgears.log"

echo "== launching arcadia (ximagesrc capture of ${DISPLAY_NUM}) =="
echo "   open http://100.78.86.4:8080 from a tailnet device, then Connect."
source "$HOME/.cargo/env"
exec ./target/debug/arcadia --source x11 --display "${DISPLAY_NUM}" --bind 0.0.0.0:8080
