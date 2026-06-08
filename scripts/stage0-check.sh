#!/usr/bin/env bash
# Stage 0 — prove the hardware on bulbasaur.
# Run this ON bulbasaur (it inspects the local GPU / VA-API encoder):
#   ssh cwnelson@bulbasaur 'bash -s' < scripts/stage0-check.sh
# or copy it over and run directly.
#
# Toolkit-agnostic: confirms the AMD Vega iGPU exposes a hardware H.264 encoder
# via VA-API and that ffmpeg can actually drive it. This is the foundation every
# later stage builds on — if this fails, nothing downstream will work.
set -uo pipefail

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; }
info() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

RENDER_NODE="${RENDER_NODE:-/dev/dri/renderD128}"

info "1. Render node present"
if [[ -e "$RENDER_NODE" ]]; then pass "$RENDER_NODE exists"; else fail "$RENDER_NODE missing"; fi

info "2. amdgpu driver loaded"
if lspci -k 2>/dev/null | grep -A3 -iE 'vga|display' | grep -qi amdgpu; then
  pass "amdgpu bound to the display controller"
else
  fail "amdgpu not shown bound (check 'lspci -k')"
fi

info "3. vainfo — VA-API encode entrypoints"
if command -v vainfo >/dev/null 2>&1; then
  VAINFO="$(vainfo --display drm --device "$RENDER_NODE" 2>&1)"
  echo "$VAINFO" | grep -iE 'VAProfileH264|VAProfileHEVC' | grep -i 'EncSlice' \
    && pass "hardware H.264/HEVC encode entrypoints present" \
    || fail "no H.264/HEVC EncSlice entrypoint found (full vainfo above-able with: vainfo --display drm --device $RENDER_NODE)"
else
  fail "vainfo not installed — 'sudo apt install vainfo libva-utils'"
fi

info "4. ffmpeg test encode (synthetic 1080p -> H.264 via VA-API)"
if command -v ffmpeg >/dev/null 2>&1; then
  OUT="$(mktemp --suffix=.h264)"
  if ffmpeg -hide_banner -loglevel error \
      -vaapi_device "$RENDER_NODE" \
      -f lavfi -i testsrc=size=1920x1080:rate=60:duration=2 \
      -vf 'format=nv12,hwupload' \
      -c:v h264_vaapi -rc_mode CBR -b:v 15M -g 60 -bf 0 \
      "$OUT" -y 2>/tmp/arcadia-ffmpeg.err; then
    SZ="$(stat -c%s "$OUT" 2>/dev/null || echo 0)"
    [[ "$SZ" -gt 0 ]] && pass "encoded $SZ bytes to $OUT (hardware path works)" || fail "ffmpeg produced an empty file"
  else
    fail "h264_vaapi encode failed — see /tmp/arcadia-ffmpeg.err"
  fi
  rm -f "$OUT"
else
  fail "ffmpeg not installed — 'sudo apt install ffmpeg'"
fi

info "5. uinput available (needed later for input injection)"
if [[ -e /dev/uinput ]]; then pass "/dev/uinput present"; else fail "/dev/uinput missing — 'sudo modprobe uinput'"; fi

printf '\n\033[1mStage 0 check complete.\033[0m All PASS == ready to build Stage 1.\n'
