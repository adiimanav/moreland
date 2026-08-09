# Moreland

Turn a USB-connected Android tablet into a second monitor for Hyprland.

Wayland-native, hardware-encoded, zero-copy. Built for one specific machine and
one specific tablet rather than for generality — every design decision below is
backed by a measurement taken on that hardware.

## Documents

| Doc | Contents |
|---|---|
| [00-system-survey.md](00-system-survey.md) | Hardware inventory, capability probing, transport evaluation, architecture decisions |
| [01-capture.md](01-capture.md) | Stage 1 — Wayland virtual output and zero-copy frame capture |
| [02-encode.md](02-encode.md) | Stage 2 — VAAPI H.264 encoding and tuning |
| [03-transport.md](03-transport.md) | Stage 3 — wire protocol and USB transport |
| [04-android-app.md](04-android-app.md) | Stage 4 — Android decoder app, build and install |
| [05-daemon.md](05-daemon.md) | Stage 5 — daemon, hotplug detection, systemd service |
| [06-plasma-backend.md](06-plasma-backend.md) | Stage 6 — KDE Plasma: why it is blocked, and the grant that unblocks it |
| [COMPATIBILITY.md](COMPATIBILITY.md) | What each compositor and distribution needs |
| [REVERT.md](REVERT.md) | How to undo everything this project touches |

## Architecture

```
Hyprland  ──hyprctl IPC──▶  virtual output "moreland" @ 1920x1200x60
                                  │
   ext_output_image_capture_source_manager_v1
                    +
   ext_image_copy_capture_manager_v1   (damage-driven)
                                  ▼
                    DMA-BUF on amdgpu/renderD129        ← zero copy
                                  │  vapostproc  XR24 -> NV12 (in-GPU)
                                  ▼
        vah264enc — VBR, no B-frames, target-usage 3
                                  │  [u32 len][u64 pts][Annex-B]
                                  ▼
        host 127.0.0.1:27183 ──adb forward──▶ localabstract:moreland
                                  │  USB 2.0 bulk
                                  ▼
      Kotlin app: LocalServerSocket → MediaCodec async → SurfaceView
```

## Stage status

| Stage | Status |
|---|---|
| 0 — Prerequisites | done |
| 1 — Capture | done, measured |
| 2 — VAAPI encode | done, measured |
| 3 — Transport | host side done, verified byte-exact |
| 4 — Android app | done, verified end to end |
| 5 — Daemon + automation | done, verified |
| 6 — KDE Plasma backend | groundwork only: interface grant verified, capture backend unimplemented |
| 7 — Benchmark + tune | next: the 24 ms display-queue term |

## Latency budget

Measured figures replace estimates as each stage lands.

| Stage | Budget |
|---|---|
| Capture → DMA-BUF | **2.1 ms** (measured) |
| VAAPI encode | **6.3 ms** (measured) |
| Transport + decode + display queue | **24.2 ms** (measured) |
| Panel scanout | 7–16 ms (not measurable in software) |
| **Glass to glass** | **~33–48 ms** |

Measured stream: 1920×1200@60, H.264 High/L5.0, no B-frames, **16.2 Mbps** —
5.9% of the measured 275 Mbps USB ceiling.

Target is 35–45 ms typical. Sub-20 ms is not achievable with compressed video
over USB to an Android device — see [00-system-survey.md](00-system-survey.md)
for why.

## Install

```bash
./install.sh
systemctl --user enable --now moreland.service
```

Then plug the tablet in. Nothing needs root; nothing is written outside `$HOME`.

The tablet app must be built and installed once:

```bash
cd android && ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

## Repository layout

```
crates/
  capture/    Wayland virtual output + DMA-BUF frame capture
  encoder/    VAAPI H.264 encode           (stage 2)
  transport/  adb forward + framed socket  (stage 3)
  protocol/   shared wire types            (stage 3)
  daemon/     detection and lifecycle      (stage 5)
android/      Kotlin decoder app           (stage 4)
systemd/      user service units           (stage 5)
docs/
```
