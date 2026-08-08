# Stage 4 — Android Decoder App

**Status: complete, verified end to end on device.**

Confirmed visually by capturing the tablet's own framebuffer with
`adb exec-out screencap` while streaming: the Hyprland desktop, status bar and
terminal colours all render correctly at the panel's full 2880x1800, upscaled
from the 1920x1200 stream with no distortion (both are 16:10).

## What it does

```
LocalServerSocket("moreland")        abstract Unix socket
  -> StreamHeader                     configure decoder
  -> MediaCodec (async, low-latency)  hardware H.264
  -> SurfaceView                      direct render
  -> ack per rendered frame           device -> host, 8 bytes
```

The host reaches the socket through
`adb forward tcp:27183 localabstract:moreland`, so traffic never touches
Android's TCP stack or `netd`.

## Design decisions

### Big-endian wire format pays off here

`Protocol.kt` reads every field with `DataInputStream.readInt()`,
`readUnsignedShort()`, `readLong()`. No byte-swapping code exists on the device
side at all, because the wire format was chosen to match Java's native order
back in Stage 3.

### The surface follows the stream, not the other way round

This is the opposite of the obvious design, and getting it backwards was the
first thing that broke on device.

**The failed version:** create a `VideoStream` in `surfaceCreated`, stop it in
`surfaceDestroyed`. That ties the stream's lifetime to the surface's — and the
abstract socket name is a **process-wide** resource. When a surface was
recreated, a second instance tried to bind a name the first still held:

```
W Moreland: accept loop error: Address already in use    (x6)
I Moreland: host connected
I Moreland: stream 1920x1200@60 video/avc
W Moreland: session ended: The surface has been released
```

Two instances, and the one that *won* the socket was the stale one holding a
released surface. The host saw a successful connect followed by a broken pipe
about a second later — a confusing symptom for a lifecycle bug.

**The fix:** exactly one `VideoStream` per process, created in `onCreate` and
stopped in `onDestroy`. The surface is a `@Volatile` *property*, swapped by
`setSurface()` as the view's surface comes and goes. `awaitSurface()` waits for
one that reports `isValid` before configuring the codec, and detaching closes
the current session so its codec never renders into a dead surface.

Belt and braces on the host side: the daemon issues
`am force-stop com.moreland.display` before `am start`, so a stale process
from a previous run can never be the one holding the socket name.

### Async MediaCodec with backpressure

`onInputBufferAvailable` pushes indices into a `LinkedBlockingQueue`; the socket
reader takes one before filling it. When the decoder has no free buffers the
reader blocks, which propagates backpressure naturally rather than queueing
frames that are already stale.

`onOutputBufferAvailable` calls `releaseOutputBuffer(index, true)` immediately —
render first, bookkeeping after — and the acknowledgement is *queued* to a
separate thread so a blocked socket write can never stall a decoder callback.

### Two attempts at low latency

```kotlin
format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)              // API 30+
format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1)  // Qualcomm
```

Stage 0 found this device declares **no `low-latency` feature** in any vendor
codec XML and uses the legacy `OMX.qcom` decoders, so the standard key may be
ignored. The Qualcomm vendor extension is the fallback; unknown vendor keys are
ignored rather than rejected, so setting it costs nothing.

Either way, the fallback that actually matters is structural: the AMD encoder
**cannot emit B-frames**, so the decoder has nothing to reorder and can output
every frame immediately. That was established in Stage 0 and verified in Stage 2
(`has_b_frames=0`).

### Timestamp units differ across the boundary

The wire carries **nanoseconds**; `MediaCodec` wants **microseconds**. The
conversion happens at exactly two points — `queueInputBuffer(…, ptsNs / 1000, …)`
and `pendingAcks.offer(info.presentationTimeUs * 1000)`.

## Measuring latency across two machines

Host and tablet have unrelated monotonic clocks. Their timestamps **cannot** be
compared directly, and any figure derived from subtracting one from the other
would be fiction.

A round trip can be measured. The app acks each frame as it hands it to the
display, and the host times send→ack. That bounds
**transport + decode + render + ack transport**.

It is an upper bound on the interesting quantity, not the quantity itself: it
includes the return trip. A separate baseline echo measurement quantifies that
return trip so it can be subtracted — see *Decomposing the round trip* below.

## Build

Toolchain found already present: JDK 17, Gradle 9.6.1, SDK at `/opt/android-sdk`
with build-tools 36.0.0 and `platforms/android-36`, licenses accepted.

`platforms/android-34` and `build-tools;34.0.0` were installed to match the
tablet's API level, and the build pins a **Gradle wrapper at 8.11.1** with AGP
8.7.3 and Kotlin 2.0.21 rather than using the system Gradle 9.6.1 — AGP 8.7
targets Gradle 8.x, and a pinned wrapper makes the build reproducible regardless
of what the system Gradle becomes later.

```bash
cd android
ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease
# -> app/build/outputs/apk/release/app-release.apk   (~2 MB)
```

Release builds are signed with the **debug key** deliberately: the app is
sideloaded and never published, and this avoids provisioning a keystore for
something that would only be a false sense of ceremony.

## Installing — the MIUI obstacle

`adb install` fails on this tablet:

```
INSTALL_FAILED_USER_RESTRICTED: Install canceled by user
```

This is the HyperOS/MIUI restriction predicted in Stage 0. Two ways past it.

**Option A — sideload from the tablet (no account required).** The APK is
already staged:

```bash
adb push app/build/outputs/apk/release/app-release.apk /sdcard/Download/moreland.apk
```

On the tablet: **Files → Downloads → `moreland.apk` → Install**, granting
the install permission when prompted.

**Option B — enable ADB installs.** Settings → Additional settings → Developer
options → **Install via USB**. MIUI gates this behind a Mi account sign-in, and
sometimes a SIM. Option A avoids that entirely.

Verify:

```bash
adb shell pm list packages com.moreland.display
```

## Running end to end

```bash
./target/release/moreland --seconds 20
```

The `moreland` binary orchestrates everything: creates the virtual output,
installs the adb forward, launches the app, then captures → encodes → streams
and reports round-trip statistics. The virtual output is an RAII guard and is
removed on exit unless `--keep-output` is passed.

Diagnostics from the device:

```bash
adb logcat -s Moreland
```

## Code

```
android/
  settings.gradle.kts
  build.gradle.kts
  gradle.properties
  gradle/wrapper/            pinned Gradle 8.11.1
  app/build.gradle.kts
  app/src/main/AndroidManifest.xml
  app/src/main/res/values/styles.xml
  app/src/main/java/com/moreland/display/
    Protocol.kt              wire format, mirrors crates/protocol
    VideoStream.kt           socket, MediaCodec, ack writer
    DisplayActivity.kt       fullscreen surface host

crates/daemon/
  src/hyprland.rs            virtual output RAII guard
  src/main.rs                end-to-end orchestration and measurement
```

## Measurements

15 s run, 1920x1200@60, continuous damage on the virtual output:

```
frames captured   901        (60.07 fps)
frames sent       900
acks received     898        (99.8%; two still in flight at exit)
bytes sent        28.4 MB    (15.1 Mbps)

round trip: host send -> device render -> host ack
  min        19.54 ms
  median     24.52 ms
  p95        34.17 ms
  max        57.29 ms
```

### Decomposing the round trip

A baseline echo over the same `adb forward` path, measured separately:

| Payload | Median RTT |
|---|---|
| 8 bytes | **0.53 ms** |
| 32 KB each way | 42.96 ms (min 8.52 — heavily buffered, not a clean signal) |

The 8-byte figure is what matters: the ack's return trip costs roughly
**0.27 ms**. That is negligible against 24.52 ms, so the round trip is
effectively a **one-way** measurement of host-send → rendered.

### What the 24.5 ms actually contains

Not decode alone. The ack fires immediately after
`releaseOutputBuffer(index, true)`, and that call **can block** when the
surface's buffer queue is full. So the figure includes display backpressure —
transport, decode, *and* waiting for the display pipeline to accept the frame.

That makes it an honest measure of "frame handed to SurfaceFlinger", which is
more of the real path than a bare decode timing would capture. It also makes it
the **largest remaining controllable term**, and the obvious Stage 6 target.

## Latency budget

| Stage | Budget |
|---|---|
| Capture → DMA-BUF | **2.1 ms** (measured) |
| VAAPI encode | **6.3 ms** (measured) |
| Transport + decode + display queue | **24.2 ms** (measured) |
| Panel scanout | 7–16 ms (not measurable in software) |
| **Glass to glass** | **~33–48 ms** |

Within the 35–45 ms committed in the architecture review. 32.6 ms of the budget
is now measured rather than estimated.
