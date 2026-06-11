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
  pkill xeyes 2>/dev/null || true
  pkill -f "xterm" 2>/dev/null || true
  pkill -f "Xvfb ${DISPLAY_NUM}" 2>/dev/null || true
  sleep 1
}

echo "== starting Xvfb ${DISPLAY_NUM} (${RES}) =="
cleanup_prev
setsid -f Xvfb "${DISPLAY_NUM}" -screen 0 "${RES}" </dev/null >/tmp/xvfb.log 2>&1
sleep 2
pgrep -a Xvfb || { echo "Xvfb failed — see /tmp/xvfb.log"; exit 1; }

# Input-responsive test apps (needs x11-apps):
#   xterm  — type to verify keyboard injection (chars appear)
#   xeyes  — pupils track the pointer to verify relative-mouse injection
# No window manager here, so X uses PointerRoot focus (keys go to the window
# under the pointer). xterm is started large so the pointer sits over it.
echo "== starting xterm + xeyes on ${DISPLAY_NUM} =="
DISPLAY="${DISPLAY_NUM}" setsid -f xterm -geometry 180x50+0+0 -fa Monospace -fs 14 </dev/null >/tmp/xterm.log 2>&1
sleep 1
DISPLAY="${DISPLAY_NUM}" setsid -f xeyes -geometry 200x200-0-0 </dev/null >/tmp/xeyes.log 2>&1
sleep 1
pgrep -a xterm >/dev/null || echo "warning: xterm not running — is x11-apps installed? see /tmp/xterm.log"
pgrep -a xeyes >/dev/null || echo "warning: xeyes not running — see /tmp/xeyes.log"

echo "== launching arcadia (ximagesrc capture of ${DISPLAY_NUM}) =="
echo "   open http://100.78.86.4:8080 from a tailnet device, then Connect."
source "$HOME/.cargo/env"
exec ./target/debug/arcadia --source x11 --display "${DISPLAY_NUM}" --bind 0.0.0.0:8080
