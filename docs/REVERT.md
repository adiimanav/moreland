# Reverting Everything

Complete inventory of what this project changes, and how to undo each item.

Paths below use `$REPO` for wherever you cloned this:
`export REPO=~/src/moreland`

Ordered from least to most invasive. Nothing here is destructive to unrelated
data, but read each section before running it.

## Quick assessment

**As of Stage 1, almost nothing persistent has been changed.**

- No system configuration files written
- No systemd units installed
- No udev rules added
- **`hyprland.conf` has not been touched**
- Nothing installed on the tablet

The virtual output and monitor settings are **runtime-only**. Restarting
Hyprland — or simply rebooting — clears them with no further action.

## 1. Hyprland virtual output (runtime only)

Created by:

```bash
hyprctl output create headless moreland
hyprctl keyword monitor "moreland,1920x1200@60,1920x0,1"
```

Remove:

```bash
hyprctl output remove moreland
```

Verify it is gone:

```bash
hyprctl monitors all | grep -c moreland    # expect 0
```

Both commands affect only the running compositor. Neither was written to
`~/.config/hypr/hyprland.conf`, so a Hyprland restart also clears them.

**If you added a `monitor=` line to `hyprland.conf` yourself**, remove it
manually — this project did not put it there.

### Workspace displacement

Creating an output moves a workspace onto it (workspace 3 here). Removing the
output returns the workspace to the remaining monitor automatically. If windows
end up somewhere unexpected:

```bash
hyprctl dispatch moveworkspacetomonitor 3 eDP-1
```

## 2. Test processes and files

Development spawned terminals on the virtual output's workspace:

```bash
pkill -x kitty          # -x matches the exact process name, not command lines
```

> Do not use `pkill -f <pattern>` where the pattern also appears in your own
> command line — it matches and kills the invoking shell. This happened during
> development and produced a confusing exit code 144.

A 200 MB throughput-benchmark file was pushed to the tablet and deleted in the
same step. Confirm:

```bash
adb shell ls -la /data/local/tmp/usbtest.bin    # expect: No such file
```

Local scratch copies live in the session scratchpad under `/tmp` and disappear
on reboot.

## 3. Build artifacts

```bash
cd "$REPO"
cargo clean
```

Downloaded crate sources also sit in `~/.cargo/registry/`. They are shared with
every other Rust project on the machine, so removing them is usually
undesirable. If you genuinely want to:

```bash
rm -rf ~/.cargo/registry/src/*/wayland-protocols-0.32.13
rm -rf ~/.cargo/registry/src/*/gbm-0.18.0
```

## 4. The project directory

```bash
rm -rf "$REPO"
```

This removes all source, docs, and build output, including
`.claude/settings.local.json`.

## 5. ADB server

A background ADB server was started on TCP port 5037:

```bash
adb kill-server
```

Host-side authorization keys live in `~/.android/adbkey` and
`~/.android/adbkey.pub`. They are shared by every ADB client on the machine —
Android Studio, other tooling — so deleting them forces re-authorization of
*all* your devices. Usually leave them alone:

```bash
rm -f ~/.android/adbkey ~/.android/adbkey.pub    # only if you mean it
```

## 6. Tablet settings

USB debugging was enabled manually and is not required by anything else this
project does once removed.

On the Xiaomi Pad 6 (HyperOS/MIUI):

1. Settings → Additional settings → Developer options
2. **Revoke USB debugging authorisations** — drops this host's RSA key
3. Turn off **USB debugging**
4. Optionally turn off Developer options entirely

Confirm from the host:

```bash
adb devices -l          # expect an empty list
lsusb | grep -i xiaomi  # expect 2717:ff40 (MTP only), not ff48 (MTP + ADB)
```

The USB product ID flipping back from `ff48` to `ff40` is the reliable signal
that the ADB interface is gone.

## 7. Installed packages

```bash
sudo pacman -Rns vulkan-radeon android-udev
```

Two cautions:

- **`vulkan-radeon` is worth keeping.** It is the Vulkan driver for your AMD
  iGPU and was simply missing before this project — `vulkaninfo` reported only
  the NVIDIA GPU. Removing it re-opens a genuine gap in your graphics stack that
  has nothing to do with this project.
- `wayland-utils` was **already installed** beforehand. Do not remove it
  attributing it to this project.

## 8. The daemon and its service

Installed by `install.sh`, entirely inside `$HOME`:

```bash
systemctl --user disable --now moreland.service
rm -f ~/.config/systemd/user/moreland.service
rm -f ~/.local/bin/moreland
systemctl --user daemon-reload
```

Stopping the service with `systemctl` sends SIGTERM, which unwinds the RAII
guards and removes the virtual output and adb forward. If the daemon was ever
hard-killed (SIGKILL) instead, a phantom monitor may remain:

```bash
hyprctl output remove moreland
adb forward --remove-all
```

**No udev rule was ever installed.** The daemon detects devices through
`adb track-devices`, so there is nothing in `/etc/udev/rules.d` to remove.

### Stage 4 — Android app

Installed by sideloading, so removal is a normal uninstall:

```bash
adb uninstall com.moreland.display
adb shell rm -f /sdcard/Download/moreland.apk
```

Or from the tablet: long-press the **Moreland** icon → Uninstall.

The Android build also added SDK components under `/opt/android-sdk`, shared
with any other Android work on this machine — leave them unless you are certain
nothing else uses them:

```bash
# only if you are sure
rm -rf /opt/android-sdk/platforms/android-34 /opt/android-sdk/build-tools/34.0.0
```

Gradle caches its distribution and dependencies in `~/.gradle`, also shared:

```bash
rm -rf ~/.gradle/caches ~/.gradle/wrapper    # affects all Gradle projects
```

### Stage 3 — ADB port forwards

Forwards do not survive an ADB server restart or a device unplug, but to clear
them explicitly:

```bash
adb forward --remove-all
```

## Full revert, in order

```bash
# 1. compositor state
hyprctl output remove moreland

# 2. stray test processes
pkill -x kitty

# 3. future stages (harmless if not yet installed)
systemctl --user disable --now moreland.service 2>/dev/null
rm -f ~/.config/systemd/user/moreland.service
systemctl --user daemon-reload
adb uninstall com.moreland.display 2>/dev/null
adb forward --remove-all 2>/dev/null

# 4. adb server
adb kill-server

# 5. project tree
rm -rf "$REPO"

# 6. packages — consider keeping vulkan-radeon
sudo pacman -Rns android-udev
```

Then disable USB debugging on the tablet (section 6).

## Verifying a clean revert

```bash
hyprctl monitors all | grep -c moreland              # 0
systemctl --user list-units | grep -c moreland # 0
adb devices -l                                        # empty (after tablet step)
ls "$REPO"          # No such file or directory
```
