#!/usr/bin/env bash
# Report whether this machine can run moreland, and if not, what blocks it.
#
# Nothing here writes anything or needs root — it only reads. The distro is
# detected solely to name packages; it is not what decides the answer. See the
# note under "Distribution" for why.
set -uo pipefail

pass() { printf '  \033[1;32m✓\033[0m %s\n' "$*"; }
fail() { printf '  \033[1;31m✗\033[0m %s\n' "$*"; }
warn() { printf '  \033[1;33m!\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
head_() { printf '\n\033[1;36m%s\033[0m\n' "$*"; }

BLOCKERS=()
block() { BLOCKERS+=("$1"); }

# ---------------------------------------------------------------- session ---
head_ "Session"

if [ "${XDG_SESSION_TYPE:-}" = "wayland" ] && [ -n "${WAYLAND_DISPLAY:-}" ]; then
    pass "Wayland session (WAYLAND_DISPLAY=$WAYLAND_DISPLAY)"
else
    fail "not a Wayland session (XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-unset})"
    info "moreland is Wayland-only; there is no X11 path."
    block "not running Wayland"
fi

# Mirror the daemon's own detection: confirm each environment marker with a
# live IPC round-trip. A stale HYPRLAND_INSTANCE_SIGNATURE inherited by a
# long-lived systemd user service otherwise names a compositor that exited
# hours ago.
COMPOSITOR="unsupported"
if { [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] \
     || [ "${XDG_CURRENT_DESKTOP:-}" = "Hyprland" ]; } \
   && hyprctl version >/dev/null 2>&1; then
    COMPOSITOR="Hyprland"
elif { [ -n "${SWAYSOCK:-}" ] || [ "${XDG_CURRENT_DESKTOP:-}" = "sway" ]; } \
     && swaymsg -t get_version >/dev/null 2>&1; then
    COMPOSITOR="Sway"
fi

case "$COMPOSITOR" in
    Hyprland) pass "compositor: Hyprland — supported, verified" ;;
    Sway)     warn "compositor: Sway — capture should work, output creation unimplemented"
              block "Sway virtual-output creation is not implemented" ;;
    *)        fail "compositor: ${XDG_CURRENT_DESKTOP:-unknown} — no virtual-output backend"
              block "no virtual-output backend for ${XDG_CURRENT_DESKTOP:-unknown}" ;;
esac

if [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && [ "$COMPOSITOR" != "Hyprland" ]; then
    warn "HYPRLAND_INSTANCE_SIGNATURE is set but no Hyprland answers"
    info "Stale environment from an earlier session. Harmless here, but a"
    info "systemd user service started under Hyprland and left enabled will"
    info "inherit it. Reset with: systemctl --user import-environment"
fi

# ------------------------------------------------------- capture protocol ---
head_ "Capture protocol (ext-image-copy-capture-v1)"

if ! command -v wayland-info >/dev/null 2>&1; then
    warn "wayland-info not installed — cannot check (package: wayland-utils)"
else
    PROTOCOLS=$(wayland-info 2>/dev/null | grep -oP "interface: '\K[^']+")
    have() { printf '%s\n' "$PROTOCOLS" | grep -qx "$1"; }

    if have ext_image_copy_capture_manager_v1; then
        pass "ext_image_copy_capture_manager_v1"
    else
        fail "ext_image_copy_capture_manager_v1 — ABSENT"
        block "compositor does not implement ext-image-copy-capture-v1"
    fi

    if have ext_output_image_capture_source_manager_v1; then
        pass "ext_output_image_capture_source_manager_v1"
    else
        fail "ext_output_image_capture_source_manager_v1 — ABSENT"
    fi

    if have zwp_linux_dmabuf_v1; then
        pass "zwp_linux_dmabuf_v1 (zero-copy import)"
    else
        fail "zwp_linux_dmabuf_v1 — ABSENT"
        block "no linux-dmabuf; the zero-copy path cannot work"
    fi

    # KWin exposes capture only through its own privileged protocol plus the
    # portal, so it never appears in a plain registry listing. Say so rather
    # than leaving the absence above looking like a packaging fault.
    if [ "${XDG_CURRENT_DESKTOP:-}" = "KDE" ]; then
        info ""
        info "KWin implements neither the ext- nor the wlr- capture protocols."
        info "It exposes capture through zkde_screencast_unstable_v1 (privileged,"
        info "portal-only) instead. Its stream_virtual_output request creates a"
        info "virtual output AND returns a PipeWire stream in one call, so a KDE"
        info "backend is one PipeWire capture path, not two separate problems."
        info "See docs/COMPATIBILITY.md."
    fi
fi

# ---------------------------------------------------------------- encoder ---
head_ "Encoder (VA-API H.264)"

if command -v gst-inspect-1.0 >/dev/null 2>&1; then
    for element in vapostproc vah264enc h264parse; do
        if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
            pass "GStreamer element: $element"
        else
            fail "GStreamer element missing: $element"
            block "GStreamer element $element is unavailable"
        fi
    done
else
    fail "gst-inspect-1.0 not found"
    block "GStreamer is not installed"
fi

if command -v vainfo >/dev/null 2>&1; then
    if vainfo 2>/dev/null | grep -q 'VAProfileH264.*VAEntrypointEncSlice'; then
        pass "VA-API H.264 encode: $(vainfo 2>/dev/null | grep -oP 'Driver version: \K.*' | head -1)"
    else
        fail "no VA-API H.264 encode entrypoint"
        block "GPU/driver exposes no VA-API H.264 encoder"
    fi
else
    warn "vainfo not installed — cannot confirm the encoder (package: libva-utils)"
fi

ls /dev/dri/renderD* >/dev/null 2>&1 \
    && pass "render nodes: $(ls -m /dev/dri/renderD* 2>/dev/null)" \
    || { fail "no /dev/dri/renderD* render node"; block "no DRM render node"; }

# --------------------------------------------------------------- transport ---
head_ "Transport (ADB)"

if command -v adb >/dev/null 2>&1; then
    pass "adb present: $(adb version 2>/dev/null | head -1)"
    DEVICES=$(adb devices 2>/dev/null | awk 'NR>1 && $2=="device" {print $1}')
    if [ -n "$DEVICES" ]; then
        for serial in $DEVICES; do
            MODEL=$(adb -s "$serial" shell getprop ro.product.model 2>/dev/null | tr -d '\r')
            SIZE=$(adb -s "$serial" shell wm size 2>/dev/null | tr -d '\r' | head -1)
            pass "device $serial — ${MODEL:-unknown} (${SIZE:-size unknown})"
            if adb -s "$serial" shell pm list packages com.moreland.display 2>/dev/null \
                | grep -q com.moreland.display; then
                pass "  tablet app installed"
            else
                warn "  tablet app NOT installed — see README 'Install'"
            fi
        done
    else
        warn "no authorised device — plug the tablet in and accept the USB-debugging prompt"
    fi
else
    fail "adb not found"
    block "adb is not installed"
fi

# ------------------------------------------------------------ distribution ---
head_ "Distribution"

DISTRO_ID=$(. /etc/os-release 2>/dev/null && echo "${ID:-unknown}")
DISTRO_LIKE=$(. /etc/os-release 2>/dev/null && echo "${ID_LIKE:-}")
DISTRO_NAME=$(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")
info "$DISTRO_NAME (kernel $(uname -r))"
info ""
info "The distribution is not what decides this. Every check above depends on"
info "the compositor, the GStreamer version and the VA-API driver, and each of"
info "those ships on every mainstream distro. A distro matters only in that it"
info "picks your default desktop and how new your GStreamer is."
info ""
info "Requirements, whatever the distro:"
info "  • Hyprland (any recent release) or another compositor implementing"
info "    ext-image-copy-capture-v1 — wlroots 0.18+ compositors do"
info "  • GStreamer 1.22+ for the va plugin (vapostproc, vah264enc);"
info "    developed against 1.28"
info "  • Mesa with radeonsi/iHD VA-API, or nvidia-vaapi-driver"

case "$DISTRO_ID $DISTRO_LIKE" in
    *arch*)
        info ""
        info "Install (verified on this family):"
        info "  sudo pacman -S --needed rust gstreamer gst-plugins-base \\"
        info "      gst-plugins-good gst-plugin-va libva libva-utils \\"
        info "      android-tools android-udev wayland-utils"
        ;;
    *fedora*|*rhel*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo dnf install rust cargo gstreamer1-plugins-base \\"
        info "      gstreamer1-plugins-good gstreamer1-plugins-bad-free \\"
        info "      libva libva-utils android-tools wayland-utils"
        ;;
    *debian*|*ubuntu*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo apt install rustc cargo gstreamer1.0-plugins-base \\"
        info "      gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \\"
        info "      libva2 vainfo adb wayland-utils"
        info "  Check GStreamer is 1.22+; older stable releases lack the va plugin."
        ;;
    *suse*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo zypper install rust cargo gstreamer-plugins-base \\"
        info "      gstreamer-plugins-good gstreamer-plugins-bad libva2 \\"
        info "      libva-utils android-tools wayland-utils"
        ;;
    *)
        info ""
        info "Unrecognised distribution; install the equivalents of the above."
        ;;
esac

# ---------------------------------------------------------------- verdict ---
head_ "Verdict"

if [ ${#BLOCKERS[@]} -eq 0 ]; then
    printf '  \033[1;32mREADY\033[0m — every requirement is met.\n\n'
    exit 0
fi

printf '  \033[1;31mBLOCKED\033[0m — %d issue(s):\n\n' "${#BLOCKERS[@]}"
for b in "${BLOCKERS[@]}"; do printf '    • %s\n' "$b"; done
printf '\n  See docs/COMPATIBILITY.md.\n\n'
exit 1
