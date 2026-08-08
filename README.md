# Moreland

**Use an Android tablet as a second monitor on Linux/Wayland, over USB.**
An alternative for Hyprland - wired, not wireless.

Plug the tablet in and a virtual monitor appears. Unplug it and the monitor
disappears. Wayland-native, hardware-encoded, zero-copy - no VNC, no RDP, no X11.

<sub>_More land: more screen real estate. And it lives next door to Wayland and
Hyprland._</sub>

```
~24 ms    host → rendered on tablet
60 fps    sustained at 1920x1200
16 Mbps   6% of the measured USB 2.0 ceiling
0.6%      of one CPU core for capture
```

> **Scope.** Verified on exactly one setup: Hyprland + AMD VA-API + a Xiaomi
> Pad 6. Other GPUs are plausible and untested; other compositors need work.
> See [Compatibility](#compatibility). Every number here is measured on that
> hardware, not estimated - see [`docs/`](docs/) for how.

Plug the cable in, and the tablet becomes a monitor - no pairing, no app to
launch on the host, no settings dialog:

<img src="docs/media/moreland_demo.gif" alt="Plugging a tablet in over USB and it appearing as a second monitor in Hyprland" width="720">

## How it works

```
Hyprland  ──IPC──▶  headless output, sized to your tablet's aspect ratio
                          │
     ext-image-copy-capture-v1  (damage-driven, DMA-BUF)
                          ▼
              DMA-BUF on the GPU          ← pixels never touch the CPU
                          │  vapostproc: XR24 → NV12, in-GPU
                          ▼
              vah264enc  VBR, no B-frames
                          │  [len][pts][flags][Annex-B]
                          ▼
      adb forward ──▶ localabstract socket ──▶ USB
                          ▼
        MediaCodec (hardware) ──▶ SurfaceView
```

The whole path from compositor to encoder is zero-copy: the buffer Hyprland
renders into is the same buffer the video encoder reads.

## Requirements

**Host**

- Hyprland (see [Compatibility](#compatibility) for others)
- A GPU with VA-API encode - AMD, Intel, or NVIDIA via `nvidia-vaapi-driver`
- `gstreamer`, `gst-plugins-base`, `gst-plugin-va`, `libva`
- `android-tools` (adb), Rust toolchain

Arch / EndeavourOS:

```bash
sudo pacman -S --needed rust gstreamer gst-plugins-base gst-plugins-good \
                        gst-plugin-va libva android-tools android-udev wayland-utils
```

**Tablet**

- Android 10+ (API 29), with hardware H.264 decode - effectively any tablet
- USB debugging enabled
- A USB **data** cable - charge-only cables will not enumerate

USB 2.0 is fine. It measured 13× more bandwidth than this needs; USB 3 buys
nothing here.

## Install

```bash
git clone https://github.com/adiimanav/moreland.git moreland && cd moreland
./install.sh
```

Installs `~/.local/bin/moreland` and a systemd **user** unit. Nothing needs
root and nothing is written outside `$HOME`.

### Enable USB debugging

Settings → About tablet → tap **Build number** (MIUI/HyperOS: **OS version**)
seven times → back → Developer options → **USB debugging**. Replug and accept
the RSA prompt.

Verify: `adb devices -l` should list the tablet as `device`.

### Build and install the tablet app

Needs the Android SDK (`ANDROID_HOME`), platform 34, and JDK 17:

```bash
cd android
ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

<details>
<summary><code>INSTALL_FAILED_USER_RESTRICTED</code> on Xiaomi / MIUI / HyperOS</summary>

MIUI blocks ADB installs by default. Either enable Developer options →
**Install via USB**, or sideload without an account:

```bash
adb push app/build/outputs/apk/release/app-release.apk /sdcard/Download/
```

then on the tablet: **Files → Downloads → tap the APK → Install**.

</details>

### Run

```bash
moreland                                          # foreground
systemctl --user enable --now moreland.service    # on login
journalctl --user -u moreland -f                  # logs
```

Plug the tablet in. A monitor appears; drag windows to it.

## Usage

```
moreland                  watch for the tablet; stream whenever plugged in
moreland --once           stream one session, then exit
moreland --seconds 15     stop after 15 s and print latency statistics

--max-width <PX>           cap the auto-detected width   [default: 1920]
--native                   stream at the tablet's full panel resolution
--width <PX> --height <PX> pin an explicit resolution (both required)
--fps <N>                  virtual output refresh rate   [default: 60]
--bitrate <KBPS>           H.264 target bitrate          [default: 20000]
--position <X>             x offset of the virtual output
```

**Resolution is automatic.** The daemon reads the tablet's panel size over ADB
and picks a matching aspect ratio, capped at `--max-width` so the encoder is not
asked to do more than the screen can show:

```
2880x1800 (16:10)  ->  1920x1200
2560x1440 (16:9)   ->  1920x1080
1280x800           ->  1280x800     (already below the cap)
```

`--native` streams the panel's full resolution. On a 2880×1800 tablet that is
2.5× the pixels of 1080p and will push the encoder hard for detail you cannot
resolve on an 11" screen - measure before keeping it.

## Performance

Measured at 1920×1200@60 on a Ryzen 7 5800HS with AMD Vega, streaming to a
Xiaomi Pad 6 over USB 2.0.

| Stage                              | Latency                              |
| ---------------------------------- | ------------------------------------ |
| Capture → DMA-BUF                  | **2.1 ms**                           |
| VAAPI encode                       | **6.3 ms**                           |
| Transport + decode + display queue | **24.2 ms**                          |
| Panel scanout                      | 7–16 ms (not measurable in software) |
| **Glass to glass**                 | **~33–48 ms**                        |

Sustained 60.07 fps, 99.7% of frames acknowledged as rendered, 16.2 Mbps.

**Sub-20 ms is not achievable** with compressed video over USB to an Android
device - the display pipeline on the tablet dominates and is not ours to
control. For calibration, [scrcpy](https://github.com/Genymobile/scrcpy) - the
most optimised project in this space - measures 25–45 ms running the same
pipeline in the opposite direction. This is fine for video, documents,
terminals, and browsing; it is not fine for gaming or stylus work.

## Compatibility

|                       | Status                                                                                              |
| --------------------- | --------------------------------------------------------------------------------------------------- |
| **Hyprland**          | Verified                                                                                            |
| Sway / wlroots        | Capture should work unchanged; output creation unimplemented                                        |
| KDE Plasma (KWin)     | Needs investigation - see [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md)                          |
| GNOME (Mutter)        | Requires a portal/PipeWire capture backend; Mutter implements neither wlr nor ext capture protocols |
| AMD VA-API            | Verified                                                                                            |
| Intel / NVIDIA VA-API | Plausible, untested - modifiers are probed at runtime                                               |
| Android 10+           | Verified on 14; nothing vendor-specific required                                                    |

Only **one** stage is compositor-specific: creating the headless output. Capture
uses the standard `ext-image-copy-capture-v1` protocol, encoding uses VA-API,
transport uses ADB. Adding a compositor means implementing create/remove in
[`crates/daemon/src/output.rs`](crates/daemon/src/output.rs) and nothing else.

## Known limitations

- **No touch or pen input.** The tablet is a display only. `zwlr_virtual_pointer_v1`
  would add absolute-pointer input without root; true multi-touch needs `uinput`.
- **The app must stay foregrounded.** Switching apps on the tablet stops the stream.
- **No audio.** Video only.
- **Idle output drops to ~1 fps.** Correct, not a bug: Hyprland does not render a
  static headless output, so a motionless screen costs almost nothing. It jumps
  straight back to 60 fps on damage.
- **Mild softness** from upscaling and H.264 on dark backgrounds. Raise
  `--bitrate` or `--max-width` if it bothers you.
- **Resizing the virtual output mid-session** restarts the pipeline - the encoder
  and decoder are both configured for a fixed geometry.

## Security

Read before publishing or installing.

**No network exposure.** `adb forward` binds `127.0.0.1` only. Nothing listens
on an external interface, and no traffic leaves the machine or the USB cable.

**The trust boundary is your local machine and your tablet.** Two consequences
on a shared or multi-user system:

- Any local process able to reach `127.0.0.1:27183` can push frames to the
  tablet while the daemon is running.
- Any app on the tablet can connect to the `localabstract:moreland` socket.
  Abstract Unix sockets carry no filesystem permissions.

Neither is exploitable for anything beyond drawing on the tablet's screen, and
neither reaches back into the host. But this is a single-user desktop tool and
is not hardened for a hostile local user.

**The app requests no Android permissions at all** - not even `INTERNET`. It can
draw to its own surface and nothing else.

**Wire data is bounds-checked** on both sides: magic and version on the stream
header, and a frame-length cap on the device so a malformed length cannot drive
a huge allocation.

**Two `unsafe` blocks**, both mapping buffers the kernel just gave us, both
commented with their invariants.

**Enabling USB debugging is the real security decision here**, and it is not
specific to this project. An authorised ADB host has broad access to the device.
Revoke authorisations when you are done: Developer options → _Revoke USB
debugging authorisations_.

**Build the APK yourself.** Release builds are signed with the Android _debug_
key, which is a shared, publicly-known key - fine for sideloading something you
compiled, meaningless as provenance. Do not trust a prebuilt APK from anyone,
including this repo.

## Documentation

Each stage is documented with what was built, what was measured, and what went
wrong:

| Doc                                             |                                                                |
| ----------------------------------------------- | -------------------------------------------------------------- |
| [00-system-survey.md](docs/00-system-survey.md) | Hardware probing, transport evaluation, architecture decisions |
| [01-capture.md](docs/01-capture.md)             | Virtual output and zero-copy capture                           |
| [02-encode.md](docs/02-encode.md)               | VA-API encoding and tuning                                     |
| [03-transport.md](docs/03-transport.md)         | Wire protocol and USB transport                                |
| [04-android-app.md](docs/04-android-app.md)     | The tablet app                                                 |
| [05-daemon.md](docs/05-daemon.md)               | Hotplug detection and the service                              |
| [COMPATIBILITY.md](docs/COMPATIBILITY.md)       | What other compositors and GPUs need                           |
| [REVERT.md](docs/REVERT.md)                     | How to undo everything                                         |

## Uninstall

```bash
systemctl --user disable --now moreland.service
rm -f ~/.local/bin/moreland ~/.config/systemd/user/moreland.service
adb uninstall com.moreland.display
```

Full inventory in [`docs/REVERT.md`](docs/REVERT.md).

## Contributing

Especially wanted:

- **Compositor backends** - Sway is the smallest step; KWin the most requested
- **A portal/PipeWire capture backend**, which would make GNOME and KDE work at once
- **Intel and NVIDIA reports**, positive or negative
- **Touch input** via `zwlr_virtual_pointer_v1`

Please include your compositor, GPU, driver version, and tablet model. A
documented failure is more useful than silence.

## License

Apache-2.0. See [LICENSE](LICENSE).
