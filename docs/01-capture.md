# Stage 1 — Virtual Output and Zero-Copy Capture

**Status: complete and measured.**

Produce a Hyprland virtual output and pull its frames onto the GPU as DMA-BUFs,
without ever touching pixels on the CPU.

## Virtual output

Hyprland creates headless outputs over its IPC:

```bash
hyprctl output create headless moreland
hyprctl keyword monitor "moreland,1920x1200@60,1920x0,1"
```

### Name the output explicitly

The unnamed form is a trap. `hyprctl output create headless` produced
`HEADLESS-2` on first use here, not `HEADLESS-1`, because Hyprland's counter
persists across creates and never resets. Any code guessing the name is a latent
bug.

Hyprland accepts an explicit name, which makes the result deterministic — the
daemon never has to diff `hyprctl monitors` to discover what it just made. The
name lives in `capture::VIRTUAL_OUTPUT_NAME`.

Removal:

```bash
hyprctl output remove moreland
```

## Capture protocol

`ext-image-copy-capture-v1` (the modern replacement for `wlr-screencopy`),
paired with `ext-image-capture-source-v1` to turn a `wl_output` into a source.
Both are v1 in Hyprland 0.56.2, and both have Rust bindings in
`wayland-protocols 0.32.13` under the `staging` feature.

Object flow:

```
ext_output_image_capture_source_manager_v1
    .create_source(wl_output)          -> ext_image_capture_source_v1
ext_image_copy_capture_manager_v1
    .create_session(source, options)   -> ext_image_copy_capture_session_v1
        events: buffer_size, shm_format, dmabuf_device, dmabuf_format, done
    .create_frame()                    -> ext_image_copy_capture_frame_v1
        attach_buffer(wl_buffer); damage_buffer(...); capture()
        events: transform, damage, presentation_time, ready | failed
```

The session and its buffer pool are created **once** and reused. Only the
`frame` object is per-capture — the protocol permits exactly one in flight at a
time (`duplicate_frame` error otherwise), so each frame is destroyed before the
next is created.

## Buffer allocation

The compositor advertises which DRM device it wants buffers on
(`dmabuf_device`, a `dev_t`) and which format/modifier pairs it can write. Both
are honoured rather than hardcoded.

On this machine the compositor asks for `/dev/dri/renderD129` — the AMD iGPU,
which is also where the VAAPI encoder lives. The zero-copy assumption from the
architecture review holds in practice, confirmed rather than assumed.

`dev_t` resolution: scan `/dev/dri`, compare `st_rdev`. If the match is a
primary (`cardN`) node, hop through `/sys/class/drm/<card>/device/drm` to its
sibling render node — render nodes need no DRM master and are what VAAPI opens.

Negotiated result:

```
format      XR24                    (opaque; alpha is meaningless here)
modifier    0x0200000000000901      AMD GFX9 tiled, no DCC
device      /dev/dri/renderD129
```

### The modifier must be constrained to what the encoder can read

Left to itself, GBM picks the most capable modifier the compositor offers. Here
that was `0x020000044051ba01` — a **DCC (delta-colour-compressed)** tiling.
VCN cannot read compressed surfaces, so GStreamer would have silently fallen
back to a CPU copy: no error, no warning, just a quietly broken zero-copy path.

`vapostproc` advertises exactly one XR24 modifier on its `memory:DMABuf` sink
pad — `0x0200000000000901` — and the compositor's 11-modifier list happens to
include it. `CaptureConfig::allowed_modifiers` intersects the two and pins the
result to a single value so GBM cannot substitute.

Measured cost of the constraint:

| Modifier | Frame age | Encoder-readable |
|---|---|---|
| `0x0200000000000901` (tiled, no DCC) | 2.59 ms | **yes** |
| `0x020000044051ba01` (DCC) | 2.03 ms | no |

DCC is 0.56 ms faster because compression means less memory bandwidth for the
compositor's write. Paying that to keep the pipeline zero-copy is obviously
correct — the alternative costs a full CPU copy plus a GPU re-upload.

The accepted set lives in `capture::ENCODER_MODIFIERS`, with LINEAR
(`0x0`) as a universal fallback.

Buffers come from GBM and are exported per-plane as DMA-BUF fds, then wrapped
into `wl_buffer`s through `zwp_linux_dmabuf_v1.create_params` / `create_immed`.
The same fds feed VAAPI in Stage 2.

## Measurements

1920×1200@60, release build, 180–300 frame runs.

| Metric | DMA-BUF | shm |
|---|---|---|
| Sustained rate | **60.01 fps** | 60 fps |
| Frame pacing | 16.37–16.98 ms (0.6 ms spread) | — |
| Dropped frames | **0** | 0 |
| **Frame age at delivery** | **2.13 ms** median, 2.57 ms p95 | 3.92 ms median |
| CPU (this process) | ~0.6% of one core | see caveat |

### Reading the latency numbers

The probe reports two different things and they are easy to confuse:

- **Capture latency ≈ 16.66 ms** — time from issuing `capture()` to `ready`.
  This is the *polling interval*: waiting for the compositor's next frame to
  exist. It is **not** added pipeline latency.
- **Frame age ≈ 2.13 ms** — time between the compositor's presentation
  timestamp and the frame reaching us. This is what enters the latency budget.

The frame handed to the encoder is fresh, not a frame period old. The original
estimate for this stage was 1–16 ms; the measured value is **2.1 ms**.

### shm caveat

The shm CPU figure sampled as "0.0%", which is misleading twice over: it samples
only *this* process, and the 9.2 MB/frame copy is performed by **Hyprland**, so
the cost lands on the compositor. The 10 ms tick resolution also cannot resolve
small values. Trust the **+1.8 ms latency delta**, not the CPU numbers.

The true zero-copy win is larger than 1.8 ms regardless: the shm path would
additionally require a CPU→GPU upload before encoding, which DMA-BUF skips.

shm exists only for bring-up and PNG dumps, and is not on the production path.

## Damage-driven capture

On an **idle** output, capture drops to roughly 1 fps (p95 latency 1000 ms).
This is correct behaviour, not a bug: Hyprland does not render a static headless
output, so a motionless tablet screen costs almost nothing in CPU, GPU, and USB
bandwidth. Adding continuously-damaging content takes it straight to a locked
60.00 fps.

Two consequences downstream:

- The encoder must tolerate **variable frame intervals** and not assume 60 Hz.
- The Android decoder will hold the last frame during idle. That is correct for
  a static screen and must not be treated as a stall.

## Code

```
crates/capture/
  src/lib.rs                 required globals, VIRTUAL_OUTPUT_NAME
  src/discovery.rs           global and output enumeration
  src/dmabuf.rs              GBM allocation, dev_t -> render node resolution
  src/shm.rs                 memfd buffers (bring-up path)
  src/session.rs             persistent capture session, both buffer modes
  src/bin/capture_probe.rs   verification and benchmark harness
```

## Verifying

```bash
# enumerate outputs
./target/release/capture-probe

# benchmark zero-copy capture
./target/release/capture-probe moreland --dmabuf --frames 180

# visual check (shm only — DMA-BUF pixels are never CPU-mapped)
./target/release/capture-probe moreland --shm --frames 30 --png /tmp/frame.png
```

For a meaningful frame-rate measurement the output needs continuous damage,
otherwise you are measuring the idle path:

```bash
hyprctl dispatch exec "[workspace 3 silent] kitty sh -c 'while true; do date +%s.%N; done'"
```

Capture was verified visually via PNG dump: a terminal and the status bar
rendered on the virtual output, correct colours, correct geometry at 1920×1200.
