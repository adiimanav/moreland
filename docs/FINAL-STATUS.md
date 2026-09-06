# Moreland — Final Validation Status

**Version:** v0.1.0  
**Validated:** 2026-09-06  
**Repository:** `Stancherkarma29/moreland`  
**Branch:** `master`  
**Validated commit:** `4e34480` — `Fix Hyprland virtual output layout reflow`

## 1. Executive summary

The Hyprland virtual-output layout reflow problem has been resolved and validated on the reference system.

When Moreland creates its Hyprland headless output, the physical monitors remain in their original positions and the virtual output is placed at the configured coordinates. The fix was validated after a real system reboot and through five complete STOP/START service cycles.

The current implementation is considered stable for the tested setup.

## 2. Problem that was fixed

Creating the Moreland headless output could cause Hyprland to reflow the existing physical outputs. In the failing case, the physical monitors were moved to the right when the `moreland` output was created.

The undesired layout was observed as:

```text
DP-1      1340x0
HDMI-A-1  3260x0
moreland  0x1080
```

The required layout is:

```text
DP-1      0x0
HDMI-A-1  1920x0
moreland  0x1080
```

## 3. Implemented solution

The daemon now records the current Hyprland monitor positions before creating the headless output.

After creation it restores the saved physical-monitor positions and explicitly places the virtual `moreland` output at the configured `x/y` coordinates.

For Hyprland 0.56's Lua configuration parser, the daemon uses `hyprctl eval` with `hl.monitor(...)` rather than the legacy `hyprctl keyword` path.

The relevant implementation is in:

```text
crates/daemon/src/output.rs
```

The implementation also logs the layout at three points:

1. BEFORE headless output creation.
2. AFTER headless output creation.
3. AFTER the layout restoration.

## 4. Final runtime configuration

The validated system uses:

```text
Compositor:       Hyprland
Physical output:  DP-1      1920x1080 @ 60 Hz
Physical output:  HDMI-A-1  1920x1080 @ 60 Hz
Virtual output:   moreland  0x1080
Tablet panel:     1340x800
Streaming mode:   1340x800 @ 90 FPS
ADB device:       R9PW10S31DF
```

The `hyprctl monitors` output reports the headless output using Hyprland's monitor mode. The actual Moreland capture/stream pipeline uses the tablet's detected `1340x800` geometry at the configured 90 FPS.

## 5. Automatic startup

The Moreland user service is enabled and active:

```text
systemctl --user is-enabled moreland.service
→ enabled

systemctl --user is-active moreland.service
→ active
```

The service starts automatically with the graphical session.

## 6. Validation performed

### 6.1 Repository/build validation

The implementation was validated with:

```bash
git diff --check
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

Results:

- `git diff --check` — passed
- `cargo fmt --all -- --check` — passed
- `cargo check --workspace` — passed
- `cargo test --workspace` — passed
- Tests: **10 passed, 0 failed**

### 6.2 Git synchronization

The validated commit is:

```text
4e34480 Fix Hyprland virtual output layout reflow
```

Local and remote `master` were verified at the same commit with zero divergence:

```text
master...origin/master
Divergence: 0 0
```

The remote branch also reported:

```text
4e34480d1e46a08d0c319c4e6562962cee2ada8f refs/heads/master
```

### 6.3 Real reboot validation

After a complete reboot, Moreland started automatically and the layout was:

```text
DP-1      0x0     1920x1080@60
HDMI-A-1  1920x0  1920x1080@60
moreland  0x1080 1920x1080@60
```

The service was both `enabled` and `active` after the reboot.

### 6.4 Five STOP/START cycles

Five controlled service cycles were performed.

For every STOP:

```text
DP-1      0x0
HDMI-A-1  1920x0
```

For every START:

```text
service: active
DP-1      0x0
HDMI-A-1  1920x0
moreland  0x1080
```

Result: **5/5 cycles passed** with no physical-monitor reflow.

### 6.5 Runtime journal validation

The final boot journal showed the expected sequence:

```text
Hyprland layout BEFORE headless create:
  DP-1 0,0
  HDMI-A-1 1920,0

Hyprland layout AFTER headless create:
  DP-1 0,0
  HDMI-A-1 1920,0
  moreland ...

restoring Hyprland layout: virtual output moreland at 0x1080

Hyprland layout AFTER restore:
  DP-1 0,0
  HDMI-A-1 1920,0
  moreland 0,1080
```

The session subsequently reported:

```text
streaming to R9PW10S31DF
```

This confirms that the layout correction does not prevent the real streaming session from starting.

## 7. Known warning

During the controlled STOP/START test, four warnings were observed:

```text
session ended: connecting to the app — is it in the foreground on the tablet?:
connecting to forwarded port 27183: Connection refused (os error 111)
```

These warnings occurred while the service was intentionally being stopped. They were followed by a normal shutdown and subsequent successful startup, device detection, output creation, and streaming.

They are therefore classified as a **known shutdown/reconnect warning**, not as a remaining layout or startup failure.

No persistent `Broken pipe` error was present in the final boot log inspected here.

## 8. Current status

### Resolved

- Hyprland physical-monitor reflow when creating the virtual output.
- Incorrect placement of the Moreland virtual output.
- Automatic startup verification after reboot.
- Repeated creation/removal stability.

### Known warning

- `Connection refused (os error 111)` on forwarded port `27183` during intentional service shutdown/reconnect testing.

### Pending problems

**None identified for the tested setup.**

## 9. PR #4

Pull request #4 remains intentionally **open and unmerged**.

The validated `master` branch contains commit `4e34480`. No merge of PR #4 was performed as part of this validation.

This document does not change the PR state.

## 10. Recovery and verification procedure

If the virtual output ever appears to disturb the physical layout again, first inspect the current state without making changes:

```bash
systemctl --user is-active moreland.service
hyprctl monitors -j | jq -r '.[] | "\(.name) \(.x)x\(.y) \(.width)x\(.height)@\(.refreshRate)"'
```

Expected layout:

```text
DP-1      0x0
HDMI-A-1  1920x0
moreland  0x1080
```

Check the service log:

```bash
journalctl --user -u moreland.service -b --no-pager | tail -n 120
```

Look specifically for the `BEFORE headless create`, `AFTER headless create`, and `AFTER restore` entries from `crates/daemon/src/output.rs`.

Do not apply manual Hyprland position corrections before collecting those observations; the diagnostic sequence is most useful when the original state is preserved.

## 11. Scope of this validation

This validation applies to the tested environment:

- Arch Linux
- Hyprland 0.56.x / Lua configuration parser
- AMD Radeon/Renoir graphics with VA-API
- Moreland v0.1.0
- Samsung Tab S7 Lite SM-T220
- Tablet panel detected as `1340x800`
- USB/ADB transport

The successful result should not be interpreted as proof that every compositor, GPU, Android device, or multi-GPU configuration behaves identically.

## 12. Final conclusion

The Moreland Hyprland layout-reflow issue is **closed for the validated environment**.

The fix is present in commit `4e34480`, is synchronized to `origin/master`, survives a real reboot, and remains stable across five controlled STOP/START cycles while preserving both physical monitor positions.

No further code or systemd changes are required based on the evidence collected during this validation.
