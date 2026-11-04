#!/bin/bash
# Nested headless Wayland (labwc + xdg-desktop-portal-wlr + PipeWire).
# Proves voxl picks Wayland by default and returns a real screen.
set -euo pipefail

log() { printf 'wayland-test: %s\n' "$*"; }

dump() {
    echo "----- voxl -----"
    cat /tmp/voxl.log 2>/dev/null || true
    echo "----- labwc -----"
    cat /tmp/labwc.log 2>/dev/null || true
    echo "----- portal -----"
    cat /tmp/portal.log 2>/dev/null || true
    echo "----- portal-wlr -----"
    cat /tmp/portal-wlr.log 2>/dev/null || true
    echo "----- pipewire -----"
    cat /tmp/pipewire.log 2>/dev/null || true
    echo "----- wireplumber -----"
    cat /tmp/wireplumber.log 2>/dev/null || true
}
trap dump EXIT

export XDG_RUNTIME_DIR=/tmp/voxl-rt
mkdir -p "$XDG_RUNTIME_DIR" /tmp/home
chmod 700 "$XDG_RUNTIME_DIR"
export HOME=/tmp/home
export XDG_CURRENT_DESKTOP=wlroots
export XDG_SESSION_TYPE=wayland
export GDK_BACKEND=wayland
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_HEADLESS_OUTPUTS=1
export WAYLAND_DISPLAY=wayland-0
export CARGO_HOME=/tmp/cargo
export CARGO_TARGET_DIR=/tmp/voxl-target
export RUST_LOG=info
export VOXL_PASSWORD=test

if ! command -v dbus-launch >/dev/null; then
    log "dbus-launch missing"
    exit 1
fi
eval "$(dbus-launch --sh-syntax)"
dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE DISPLAY HOME XDG_RUNTIME_DIR || true

log "pipewire"
pipewire > /tmp/pipewire.log 2>&1 &
sleep 0.3
# Session manager. Without it the portal node and voxl never get linked.
if command -v wireplumber >/dev/null; then
    wireplumber > /tmp/wireplumber.log 2>&1 &
    sleep 0.4
else
    log "wireplumber missing"
    exit 1
fi

gcc -O2 -o /tmp/colorwin /src/docker/wayland/colorwin.c $(pkg-config --cflags --libs gtk+-3.0)

log "labwc"
labwc -s /tmp/colorwin > /tmp/labwc.log 2>&1 &
for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { log "no wayland socket"; exit 1; }
sleep 0.4

output="$(wlr-randr 2>/dev/null | awk '/^[^ ]/ { print $1; exit }')"
log "output=${output:-unknown}"
mkdir -p "$HOME/.config/xdg-desktop-portal-wlr"
cat > "$HOME/.config/xdg-desktop-portal-wlr/config" << EOF
[screencast]
max_fps=60
chooser_type=none
output_name=${output:-HEADLESS-1}
EOF

portal_bin() {
    local name="$1"
    local p
    for p in "/usr/libexec/$name" "/usr/lib/x86_64-linux-gnu/libexec/$name"; do
        [ -x "$p" ] && { echo "$p"; return; }
    done
    command -v "$name"
}
wlr_portal="$(portal_bin xdg-desktop-portal-wlr)"
xdp="$(portal_bin xdg-desktop-portal)"
log "portals $wlr_portal $xdp"
"$wlr_portal" -r > /tmp/portal-wlr.log 2>&1 &
sleep 0.3
"$xdp" -r > /tmp/portal.log 2>&1 &
sleep 0.6

# Ubuntu's libyuv-dev ships the library and headers without a pkg-config file.
if ! pkg-config --exists libyuv; then
    mkdir -p /usr/lib/x86_64-linux-gnu/pkgconfig
    cat > /usr/lib/x86_64-linux-gnu/pkgconfig/libyuv.pc << 'EOF'
prefix=/usr
libdir=${prefix}/lib/x86_64-linux-gnu
includedir=${prefix}/include

Name: libyuv
Description: YUV conversion and scaling
Version: 0
Libs: -L${libdir} -lyuv
Cflags: -I${includedir}
EOF
fi

log "build"
cd /src
cargo build --bin voxl

log "run voxl (no --backend: Wayland must win because WAYLAND_DISPLAY is set)"
/tmp/voxl-target/debug/voxl --addr=127.0.0.1 --port=6901 --audio-enabled=false \
    > /tmp/voxl.log 2>&1 &
voxl_pid=$!

shot=""
for _ in $(seq 1 40); do
    if curl -sf -u ":$VOXL_PASSWORD" -o /tmp/shot.png http://127.0.0.1:6901/screenshot; then
        shot=1
        break
    fi
    if ! kill -0 "$voxl_pid" 2>/dev/null; then
        log "voxl exited"
        exit 1
    fi
    sleep 0.5
done
[ -n "$shot" ] || { log "screenshot timed out"; exit 1; }

python3 - << 'PY'
from PIL import Image
im = Image.open("/tmp/shot.png").convert("RGB")
w, h = im.size
px = list(im.getdata())
sat = 0
for r, g, b in px:
    hi, lo = max(r, g, b), min(r, g, b)
    if hi > 180 and lo < 80 and (hi - lo) > 100:
        sat += 1
print(f"shot {w}x{h} saturated={sat} of {len(px)}")
if sat < 500:
    raise SystemExit("frame is not the colored Wayland client")
PY

if grep -q "falling back to X11" /tmp/voxl.log; then
    log "unexpected X11 fallback"
    exit 1
fi
grep -q "desktop backend: wayland (default)" /tmp/voxl.log
log "ok"
trap - EXIT
kill "$voxl_pid" 2>/dev/null || true
if [ -n "${HOST_UID:-}" ]; then
    chown "${HOST_UID}:${HOST_GID:-$HOST_UID}" /src/Cargo.lock 2>/dev/null || true
fi
exit 0
