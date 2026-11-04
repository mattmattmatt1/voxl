#!/bin/bash
# Plasma on kwin_wayland --virtual, captured through the KDE remote-desktop
# portal so pointer and keyboard go into the session. --virtual keeps this
# compositor off the machine's monitors. A GPU that can do OpenGL is required;
# the NVIDIA container runtime needs the extra EGL setup below.
set -euo pipefail

log() { printf 'kde-view: %s\n' "$*"; }

dump() {
    echo "----- voxl -----"
    cat /tmp/voxl.log 2>/dev/null || true
    echo "----- kwin -----"
    grep -E 'OpenGL|compositor|EGL|GBM|error|Error|Failed|master|mode' /tmp/kwin.log 2>/dev/null | head -40 || true
    echo "----- portal-kde -----"
    grep -E 'xdp-kde-remotedesktop|xdp-kde-screencast|MegaAuth|failed|error|Error' /tmp/portal-kde.log 2>/dev/null | head -40 || true
    echo "----- portal -----"
    tail -20 /tmp/portal.log 2>/dev/null || true
}

export XDG_RUNTIME_DIR=/tmp/voxl-rt
mkdir -p "$XDG_RUNTIME_DIR" /tmp/home/.config/xdg-desktop-portal
chmod 700 "$XDG_RUNTIME_DIR"
export HOME=/tmp/home
export XDG_CURRENT_DESKTOP=KDE
export XDG_SESSION_DESKTOP=KDE
export XDG_SESSION_TYPE=wayland
export KDE_FULL_SESSION=true
export KDE_SESSION_VERSION=6
export QT_QPA_PLATFORM=wayland
export GDK_BACKEND=wayland
export WAYLAND_DISPLAY=wayland-0
export KWIN_WAYLAND_NO_PERMISSION_CHECKS=1
export QT_LOGGING_RULES="xdp-kde*.debug=true"
export RUST_LOG=info
# No password is stored in this script. Set VOXL_PASSWORD to choose one;
# otherwise this run makes a random one and prints a sign-in link.
generated_pw=0
if [ -z "${VOXL_PASSWORD:-}" ]; then
    VOXL_PASSWORD="$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
    generated_pw=1
fi
export VOXL_PASSWORD
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
# Plasma ships plasma-applications.menu. Without this prefix, kbuildsycoca
# looks for applications.menu, builds an empty app cache, and Kickoff
# drops every entry as invalid.
export XDG_MENU_PREFIX=plasma-
unset DISPLAY || true

# NVIDIA's container runtime mounts the EGL driver but not the GBM backend or
# the vendor JSON. Without those, KWin never starts OpenGL and screencast
# refuses. Skip this on a machine that has no NVIDIA EGL library.
if compgen -G '/usr/lib/*/libEGL_nvidia.so*' > /dev/null || compgen -G '/usr/lib/libEGL_nvidia.so*' > /dev/null; then
    mkdir -p /usr/share/glvnd/egl_vendor.d /usr/share/egl/egl_external_platform.d
    cat > /usr/share/glvnd/egl_vendor.d/10_nvidia.json << 'EOF'
{
    "file_format_version" : "1.0.0",
    "ICD" : { "library_path" : "libEGL_nvidia.so.0" }
}
EOF
    cat > /usr/share/egl/egl_external_platform.d/15_nvidia_gbm.json << 'EOF'
{
    "file_format_version" : "1.0.0",
    "ICD" : { "library_path" : "libnvidia-egl-gbm.so.1" }
}
EOF
    cat > /usr/share/egl/egl_external_platform.d/10_nvidia_wayland.json << 'EOF'
{
    "file_format_version" : "1.0.0",
    "ICD" : { "library_path" : "libnvidia-egl-wayland.so.1" }
}
EOF
    export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/10_nvidia.json
    log "using the NVIDIA EGL driver"
else
    log "no NVIDIA EGL library; KWin uses the container GL driver"
fi

# Falkon is Qt WebEngine, which refuses to run as root unless the Chromium
# sandbox is off. The menu starts whatever /usr/bin/falkon is, so wrap it.
export QTWEBENGINE_DISABLE_SANDBOX=1
if [ -x /usr/bin/falkon ] && [ ! -f /usr/bin/falkon.real ]; then
    mv /usr/bin/falkon /usr/bin/falkon.real
    cat > /usr/bin/falkon << 'EOF'
#!/bin/sh
export QTWEBENGINE_DISABLE_SANDBOX=1
export QTWEBENGINE_CHROMIUM_FLAGS="--no-sandbox ${QTWEBENGINE_CHROMIUM_FLAGS:-}"
exec /usr/bin/falkon.real "$@"
EOF
    chmod 755 /usr/bin/falkon
fi

printf '%s\n' '[Basic Settings]' 'Indexing-Enabled=false' > "$HOME/.config/baloofilerc"
mkdir -p "$HOME/.config"
cat > "$HOME/.config/mimeapps.list" << 'EOF'
[Default Applications]
x-scheme-handler/http=org.kde.falkon.desktop
x-scheme-handler/https=org.kde.falkon.desktop
text/html=org.kde.falkon.desktop
EOF
xdg-user-dirs-update
kbuildsycoca6 --noincremental > /tmp/sycoca.log 2>&1 || true
printf '%s\n' '[preferred]' 'default=kde' > "$HOME/.config/xdg-desktop-portal/portals.conf"

eval "$(dbus-launch --sh-syntax)"
dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE XDG_MENU_PREFIX HOME XDG_RUNTIME_DIR QT_QPA_PLATFORM QT_LOGGING_RULES LANG LC_ALL || true

log "pipewire"
pipewire > /tmp/pipewire.log 2>&1 &
sleep 0.3
wireplumber > /tmp/wireplumber.log 2>&1 &
sleep 0.4

log "kwin"
kwin_wayland --virtual --width 1280 --height 720 --socket wayland-0 \
    --no-lockscreen --no-global-shortcuts \
    > /tmp/kwin.log 2>&1 &
kwin_pid=$!
for _ in $(seq 1 80); do
    [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
    if ! kill -0 "$kwin_pid" 2>/dev/null; then
        log "kwin exited"
        dump
        exit 1
    fi
    sleep 0.25
done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { log "no wayland socket"; dump; exit 1; }
# The socket file appears before KWin calls listen(). plasmashell then dies with
# "Connection refused" and the panel never comes up.
python3 - << 'PY'
import os, socket, sys, time
path = os.environ["XDG_RUNTIME_DIR"] + "/" + os.environ["WAYLAND_DISPLAY"]
for _ in range(80):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(path)
        s.close()
        sys.exit(0)
    except OSError:
        time.sleep(0.1)
sys.exit(1)
PY
[ $? -eq 0 ] || { log "wayland socket never accepted"; dump; exit 1; }

plasmashell > /tmp/plasma.log 2>&1 &
plasma_pid=$!
sleep 1
if ! kill -0 "$plasma_pid" 2>/dev/null || grep -q 'Connection refused' /tmp/plasma.log; then
    log "plasmashell missed the display, trying once more"
    kill "$plasma_pid" 2>/dev/null || true
    sleep 0.5
    plasmashell > /tmp/plasma.log 2>&1 &
fi
sleep 1
konsole --hide-menubar -e bash -c 'printf "voxl on Plasma\n"; sleep infinity' > /tmp/konsole.log 2>&1 &
sleep 2

portal_bin() {
    local name="$1"
    local p
    for p in "/usr/libexec/$name" "/usr/lib/x86_64-linux-gnu/libexec/$name"; do
        [ -x "$p" ] && { echo "$p"; return; }
    done
    command -v "$name"
}

log "portals"
/usr/libexec/xdg-permission-store > /tmp/perm.log 2>&1 &
sleep 0.3
"$(portal_bin xdg-desktop-portal-kde)" > /tmp/portal-kde.log 2>&1 &
sleep 0.4
"$(portal_bin xdg-desktop-portal)" -r > /tmp/portal.log 2>&1 &
sleep 0.6

# Host apps with no app id, which is what voxl is. Skips the share dialog.
for app in "" voxl voxl-server; do
    dbus-send --session --print-reply \
        --dest=org.freedesktop.impl.portal.PermissionStore \
        /org/freedesktop/impl/portal/PermissionStore \
        org.freedesktop.impl.portal.PermissionStore.SetPermission \
        string:kde-authorized boolean:true string:remote-desktop \
        "string:${app}" array:string:yes >> /tmp/perm.log 2>&1 || log "permission for '${app}' failed"
done

log "run voxl"
# Bind every interface inside the container. The docker publish flag chooses
# who can reach that port on the host. Set VOXL_ALLOWED_HOSTS when a TLS
# proxy presents a hostname; names are refused unless they are listed.
voxl_addr="${VOXL_ADDR:-0.0.0.0}"
voxl_args=(--addr="$voxl_addr" --port=6901 --audio-enabled=false)
if [ -n "${VOXL_ALLOWED_HOSTS:-}" ]; then
    voxl_args+=(--allowed-hosts="$VOXL_ALLOWED_HOSTS")
fi
/tmp/voxl-target/debug/voxl "${voxl_args[@]}" \
    > /tmp/voxl.log 2>&1 &
voxl_pid=$!
if [ "$generated_pw" -eq 1 ]; then
    log "open http://localhost:6901/?token=${VOXL_PASSWORD}"
fi

cleanup() { kill "$voxl_pid" "$kwin_pid" 2>/dev/null || true; }
trap cleanup TERM INT

ready=""
for _ in $(seq 1 45); do
    if ! kill -0 "$voxl_pid" 2>/dev/null; then
        log "voxl exited"
        dump
        exit 1
    fi
    if curl -sf -u ":$VOXL_PASSWORD" -o /tmp/shot.png http://127.0.0.1:6901/screenshot; then
        if python3 - << 'PY'
from PIL import Image
im = Image.open("/tmp/shot.png").convert("RGB")
w, h = im.size
px = list(im.getdata())
colors = {(r // 16, g // 16, b // 16) for r, g, b in px[::8]}
print(f"shot {w}x{h} tones={len(colors)}")
raise SystemExit(0 if len(colors) >= 12 else 1)
PY
        then
            ready=1
            break
        fi
    fi
    sleep 2
done
[ -n "$ready" ] || { log "desktop never appeared"; dump; exit 1; }

if grep -q "falling back to X11" /tmp/voxl.log; then
    log "unexpected X11 fallback"
    dump
    exit 1
fi
grep -q "desktop backend: wayland (default)" /tmp/voxl.log
grep -q "wayland remote desktop" /tmp/voxl.log || { log "no remote-desktop input"; dump; exit 1; }
log "ready"
wait "$voxl_pid"
