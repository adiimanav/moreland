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
cargo build --release --manifest-path "$REPO/Cargo.toml"

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
# Hyprland, Sway or GNOME, so it is installed only on Plasma rather than left
# sitting in a Hyprland user's application directory.
#
# Set MORELAND_INSTALL_DESKTOP_ENTRY=1 to install it anyway, for someone who
# installs from a TTY or uses Plasma as a second session.
wants_desktop_entry() {
    [ "${MORELAND_INSTALL_DESKTOP_ENTRY:-0}" = "1" ] && return 0
    case "${XDG_CURRENT_DESKTOP:-}" in
        *KDE*) return 0 ;;
    esac
    return 1
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
    say "Not a Plasma session: skipping the KDE desktop entry"
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
