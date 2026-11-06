#!/usr/bin/env bash
# Install the moreland daemon and its user service.
#
# Nothing here needs root, and nothing is written outside $HOME.
# To undo everything, see docs/REVERT.md.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
APP_DIR="$HOME/.local/share/applications"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warn\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror\033[0m %s\n' "$*" >&2; exit 1; }

say "Building the daemon"
# The KDE Plasma backend needs libpipewire, the plasma-wayland-protocols XML
# and clang (for the PipeWire bindings) at build time. Build it whenever they
# are present, not only from a Plasma session: a Plasma-enabled daemon still
# drives Hyprland and labwc, so one install serves every desktop the user logs
# into. Installed from Hyprland without it, the service fails on Plasma.
# MORELAND_PLASMA=1 or 0 overrides the guess.
have_plasma_deps() {
    pkg-config --exists libpipewire-0.3 2>/dev/null || return 1
    command -v clang >/dev/null || return 1
    # The same places crates/capture/build.rs looks.
    if [[ -n ${MORELAND_ZKDE_SCREENCAST_XML:-} ]]; then
        [[ -f $MORELAND_ZKDE_SCREENCAST_XML ]]
        return
    fi
    local dirs="${XDG_DATA_DIRS:-}" root
    for root in ${dirs//:/ } /usr/share /usr/local/share /run/current-system/sw/share; do
        [[ -f $root/plasma-wayland-protocols/zkde-screencast-unstable-v1.xml ]] && return 0
    done
    return 1
}
FEATURES=""
case "${MORELAND_PLASMA:-auto}" in
    1) FEATURES="--features plasma" ;;
    0) ;;
    *)
        if have_plasma_deps; then
            FEATURES="--features plasma"
        elif [[ ${XDG_CURRENT_DESKTOP:-} == *KDE* ]]; then
            die "the KDE Plasma backend needs libpipewire, plasma-wayland-protocols and clang to build; install them (Arch: pacman -S pipewire plasma-wayland-protocols clang) and re-run, or set MORELAND_PLASMA=0"
        fi
        ;;
esac
if [[ -n $FEATURES ]]; then
    say "Including the KDE Plasma backend"
fi
# shellcheck disable=SC2086
cargo build --release --manifest-path "$REPO/Cargo.toml" $FEATURES

say "Installing moreland to $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m755 "$REPO/target/release/moreland" "$BIN_DIR/moreland"

say "Installing the user service to $UNIT_DIR"
mkdir -p "$UNIT_DIR"
sed "s|ExecStart=%h/.local/bin/moreland|ExecStart=$BIN_DIR/moreland|" \
    "$REPO/systemd/moreland.service" > "$UNIT_DIR/moreland.service"
systemctl --user daemon-reload

# The desktop entry exists for exactly one reason: KWin only advertises
# zkde_screencast_unstable_v1 to a client that declares it. It does nothing on
# Hyprland, Sway or GNOME, so it goes with the KDE backend: installed whenever
# that is built in, which covers installing from Hyprland for a Plasma second
# session, and left out of an install that could not use it anyway.
#
# Set MORELAND_INSTALL_DESKTOP_ENTRY=1 to install it regardless.
wants_desktop_entry() {
    [ "${MORELAND_INSTALL_DESKTOP_ENTRY:-0}" = "1" ] && return 0
    [[ -n $FEATURES ]]
}

if wants_desktop_entry; then
    say "Installing the KDE desktop entry to $APP_DIR"
    # Nothing here may be fatal: a KDE-shaped step must never fail an install.
    if mkdir -p "$APP_DIR" 2>/dev/null &&
       sed "s|^Exec=.*|Exec=$BIN_DIR/moreland|" \
           "$REPO/desktop/moreland.desktop" > "$APP_DIR/moreland.desktop" 2>/dev/null; then
        # KWin reads the service cache rather than the directory, so a stale
        # cache hides the grant entirely.
        if command -v kbuildsycoca6 >/dev/null 2>&1; then
            kbuildsycoca6 --noincremental >/dev/null 2>&1 \
                || warn "kbuildsycoca6 failed; run it by hand"
        fi
    else
        warn "Could not install the desktop entry"
    fi
else
    say "No KDE backend in this build: skipping the KDE desktop entry"
    # Deliberately not removed. Someone who uses both Plasma and Hyprland
    # would lose the grant they still need in the other session.
    if [ -f "$APP_DIR/moreland.desktop" ]; then
        warn "An earlier install left $APP_DIR/moreland.desktop; harmless here."
        printf '    Remove it with: rm %s/moreland.desktop\n' "$APP_DIR"
    fi
fi

# The APK is not built here: it needs the Android SDK, and a stale APK is
# worse than an absent one.
if command -v adb >/dev/null 2>&1; then
    if adb shell pm list packages com.moreland.display 2>/dev/null \
        | grep -q com.moreland.display; then
        say "Tablet app is installed"
    else
        warn "Tablet app is NOT installed. Build and install it with:"
        printf '    cd %s/android\n' "$REPO"
        printf '    ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease\n'
        printf '    adb install -r app/build/outputs/apk/release/app-release.apk\n'
        printf '  If adb install is refused (MIUI), sideload from the tablet:\n'
        printf '    adb push app/build/outputs/apk/release/app-release.apk /sdcard/Download/\n'
    fi
else
    warn "adb not found on PATH; the daemon needs it at runtime"
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on your PATH" ;;
esac

cat <<EOF

$(say "Done")

  Run it now:        moreland
  Start on login:    systemctl --user enable --now moreland.service
  Follow the logs:   journalctl --user -u moreland -f
  One-off test:      moreland --seconds 15

  Uninstall:         see docs/REVERT.md
EOF
