// SPDX-License-Identifier: Apache-2.0

//! The S Pen as a virtual screen tablet.
//!
//! libinput classifies a device with `BTN_TOOL_PEN`, absolute X/Y and pressure
//! as a graphics tablet, and `INPUT_PROP_DIRECT` marks it as drawing straight
//! on a display (a Cintiq rather than an Intuos). KWin and applications then
//! treat it as a pen: pressure and tilt reach Krita, hover shows the brush
//! cursor, and the barrel button acts as a stylus button.

use crate::{abs, key, Raw, AXIS_MAX, AXIS_RESOLUTION};
use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, PropType, UinputAbsSetup,
};
use protocol::{
    Action, PointerEvent, Tool, BUTTON_STYLUS_PRIMARY, BUTTON_STYLUS_SECONDARY,
};
use std::path::PathBuf;

/// Pressure range reported to the system; libinput normalises it anyway.
const PRESSURE_MAX: i32 = 4095;

/// Tilt range in degrees, as the kernel defines `ABS_TILT_X/Y`.
const TILT_MAX: i32 = 90;

fn tool_key(tool: Tool) -> KeyCode {
    match tool {
        Tool::Eraser => KeyCode::BTN_TOOL_RUBBER,
        _ => KeyCode::BTN_TOOL_PEN,
    }
}

/// Where the pen is relative to the glass, and what the host was last told.
#[derive(Default)]
pub struct PenState {
    /// The tool currently in range, if any.
    tool: Option<Tool>,
    touching: bool,
    buttons: u8,
}

impl PenState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn in_range(&self) -> bool {
        self.tool.is_some()
    }

    pub fn is_touching(&self) -> bool {
        self.touching
    }

    /// Android's `AXIS_TILT` (angle from perpendicular) and `AXIS_ORIENTATION`
    /// (direction of the lean, clockwise from the top of the screen) as the
    /// kernel's two signed per-axis angles. X is positive to the right, Y is
    /// positive towards the user (down the screen).
    fn tilt_axes(tilt: f32, orientation: f32) -> (i32, i32) {
        let degrees = tilt.to_degrees().clamp(0.0, TILT_MAX as f32);
        let x = orientation.sin() * degrees;
        let y = -orientation.cos() * degrees;
        (x.round() as i32, y.round() as i32)
    }

    fn release(&mut self) -> Vec<Raw> {
        let mut out = Vec::new();
        if self.touching {
            out.push(abs(AbsoluteAxisCode::ABS_PRESSURE, 0));
            out.push(key(KeyCode::BTN_TOUCH, 0));
        }
        if self.buttons & BUTTON_STYLUS_PRIMARY != 0 {
            out.push(key(KeyCode::BTN_STYLUS, 0));
        }
        if self.buttons & BUTTON_STYLUS_SECONDARY != 0 {
            out.push(key(KeyCode::BTN_STYLUS2, 0));
        }
        if let Some(tool) = self.tool {
            out.push(key(tool_key(tool), 0));
        }
        *self = Self::default();
        out
    }

    /// Events implied by one pen or eraser pointer. Fingers produce nothing.
    pub fn apply(&mut self, event: &PointerEvent) -> Vec<Raw> {
        if event.tool == Tool::Finger {
            return Vec::new();
        }

        match event.action {
            Action::HoverExit | Action::Cancel => return self.release(),
            Action::Up => {
                // Lifting leaves the pen hovering; only contact ends.
                let mut out = Vec::new();
                if self.touching {
                    self.touching = false;
                    out.push(abs(AbsoluteAxisCode::ABS_PRESSURE, 0));
                    out.push(key(KeyCode::BTN_TOUCH, 0));
                }
                return out;
            }
            Action::Down | Action::Move | Action::HoverMove => {}
        }

        let mut out = Vec::new();

        // Proximity first, so the tool is announced before its coordinates.
        // Swapping pen for eraser is a leave and an enter.
        if self.tool != Some(event.tool) {
            if let Some(old) = self.tool {
                out.push(key(tool_key(old), 0));
            }
            out.push(key(tool_key(event.tool), 1));
            self.tool = Some(event.tool);
        }

        let scale = |v: f32| (v.clamp(0.0, 1.0) * AXIS_MAX as f32).round() as i32;
        out.push(abs(AbsoluteAxisCode::ABS_X, scale(event.x)));
        out.push(abs(AbsoluteAxisCode::ABS_Y, scale(event.y)));
        let (tilt_x, tilt_y) = Self::tilt_axes(event.tilt, event.orientation);
        out.push(abs(AbsoluteAxisCode::ABS_TILT_X, tilt_x));
        out.push(abs(AbsoluteAxisCode::ABS_TILT_Y, tilt_y));

        // Down and Move both mean the tip is on the glass; a Move without a
        // preceding Down (a dropped event) still has to register contact.
        let in_contact = matches!(event.action, Action::Down | Action::Move);
        if in_contact {
            let pressure = (event.pressure.clamp(0.0, 1.0) * PRESSURE_MAX as f32).round() as i32;
            out.push(abs(AbsoluteAxisCode::ABS_PRESSURE, pressure));
        } else {
            out.push(abs(AbsoluteAxisCode::ABS_PRESSURE, 0));
        }
        if in_contact != self.touching {
            self.touching = in_contact;
            out.push(key(KeyCode::BTN_TOUCH, i32::from(in_contact)));
        }

        let changed = self.buttons ^ event.buttons;
        if changed & BUTTON_STYLUS_PRIMARY != 0 {
            out.push(key(
                KeyCode::BTN_STYLUS,
                i32::from(event.buttons & BUTTON_STYLUS_PRIMARY != 0),
            ));
        }
        if changed & BUTTON_STYLUS_SECONDARY != 0 {
            out.push(key(
                KeyCode::BTN_STYLUS2,
                i32::from(event.buttons & BUTTON_STYLUS_SECONDARY != 0),
            ));
        }
        self.buttons = event.buttons;
        out
    }

    pub fn release_all(&mut self) -> Vec<Raw> {
        self.release()
    }
}

/// A virtual screen tablet.
pub struct Pen {
    device: VirtualDevice,
    state: PenState,
    pending: Vec<InputEvent>,
}

impl Pen {
    pub fn create(name: &str) -> Result<Self> {
        let position = |code| {
            UinputAbsSetup::new(code, AbsInfo::new(0, 0, AXIS_MAX, 0, 0, AXIS_RESOLUTION))
        };
        let pressure = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_PRESSURE,
            AbsInfo::new(0, 0, PRESSURE_MAX, 0, 0, 0),
        );
        let tilt = |code| {
            UinputAbsSetup::new(code, AbsInfo::new(0, -TILT_MAX, TILT_MAX, 0, 0, 0))
        };

        let mut keys = AttributeSet::<KeyCode>::new();
        for code in [
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_TOOL_PEN,
            KeyCode::BTN_TOOL_RUBBER,
            KeyCode::BTN_STYLUS,
            KeyCode::BTN_STYLUS2,
        ] {
            keys.insert(code);
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT);

        let device = VirtualDevice::builder()
            .context("opening /dev/uinput — is it writable by your user?")?
            .name(name)
            .with_properties(&props)?
            .with_keys(&keys)?
            .with_absolute_axis(&position(AbsoluteAxisCode::ABS_X))?
            .with_absolute_axis(&position(AbsoluteAxisCode::ABS_Y))?
            .with_absolute_axis(&pressure)?
            .with_absolute_axis(&tilt(AbsoluteAxisCode::ABS_TILT_X))?
            .with_absolute_axis(&tilt(AbsoluteAxisCode::ABS_TILT_Y))?
            .build()
            .context("creating the virtual pen")?;

        Ok(Self {
            device,
            state: PenState::new(),
            pending: Vec::new(),
        })
    }

    pub fn event_node(&mut self) -> Result<Option<PathBuf>> {
        for node in self.device.enumerate_dev_nodes_blocking()? {
            let node = node?;
            if node
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("event"))
            {
                return Ok(Some(node));
            }
        }
        Ok(None)
    }

    pub fn in_range(&self) -> bool {
        self.state.in_range()
    }

    pub fn is_touching(&self) -> bool {
        self.state.is_touching()
    }

    pub fn apply(&mut self, event: &PointerEvent) {
        let raw = self.state.apply(event);
        self.queue(raw);
    }

    fn queue(&mut self, raw: Vec<Raw>) {
        self.pending
            .extend(raw.into_iter().map(|(t, c, v)| InputEvent::new(t, c, v)));
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.device
            .emit(&self.pending)
            .context("writing to the virtual pen")?;
        self.pending.clear();
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<()> {
        let raw = self.state.release_all();
        self.queue(raw);
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::POINTER_FLAG_LAST;

    fn pen(action: Action, x: f32, y: f32, pressure: f32) -> PointerEvent {
        PointerEvent {
            action,
            tool: Tool::Pen,
            pointer_id: 0,
            buttons: 0,
            flags: POINTER_FLAG_LAST,
            x,
            y,
            pressure,
            tilt: 0.0,
            orientation: 0.0,
        }
    }

    fn has_key(events: &[Raw], code: KeyCode, value: i32) -> bool {
        events.contains(&key(code, value))
    }

    fn has_abs(events: &[Raw], code: AbsoluteAxisCode, value: i32) -> bool {
        events.contains(&abs(code, value))
    }

    #[test]
    fn hover_enters_range_without_touching() {
        let mut state = PenState::new();
        let out = state.apply(&pen(Action::HoverMove, 0.5, 0.5, 0.0));
        assert!(has_key(&out, KeyCode::BTN_TOOL_PEN, 1));
        assert!(!has_key(&out, KeyCode::BTN_TOUCH, 1));
        assert!(has_abs(&out, AbsoluteAxisCode::ABS_PRESSURE, 0));
        assert!(state.in_range() && !state.is_touching());
    }

    #[test]
    fn tool_is_announced_only_once() {
        let mut state = PenState::new();
        state.apply(&pen(Action::HoverMove, 0.1, 0.1, 0.0));
        let out = state.apply(&pen(Action::HoverMove, 0.2, 0.2, 0.0));
        assert!(!has_key(&out, KeyCode::BTN_TOOL_PEN, 1));
    }

    #[test]
    fn contact_reports_scaled_pressure_and_touch() {
        let mut state = PenState::new();
        state.apply(&pen(Action::HoverMove, 0.5, 0.5, 0.0));
        let down = state.apply(&pen(Action::Down, 0.5, 0.5, 0.5));
        assert!(has_key(&down, KeyCode::BTN_TOUCH, 1));
        assert!(has_abs(&down, AbsoluteAxisCode::ABS_PRESSURE, 2048));

        let lift = state.apply(&pen(Action::Up, 0.5, 0.5, 0.0));
        assert!(has_key(&lift, KeyCode::BTN_TOUCH, 0));
        // Still hovering after the lift.
        assert!(state.in_range() && !state.is_touching());
    }

    #[test]
    fn moving_without_a_down_still_registers_contact() {
        let mut state = PenState::new();
        let out = state.apply(&pen(Action::Move, 0.3, 0.3, 0.4));
        assert!(has_key(&out, KeyCode::BTN_TOOL_PEN, 1));
        assert!(has_key(&out, KeyCode::BTN_TOUCH, 1));
    }

    #[test]
    fn leaving_range_lifts_and_clears_everything() {
        let mut state = PenState::new();
        state.apply(&pen(Action::Down, 0.5, 0.5, 0.8));
        let out = state.apply(&pen(Action::HoverExit, 0.5, 0.5, 0.0));
        assert!(has_key(&out, KeyCode::BTN_TOUCH, 0));
        assert!(has_key(&out, KeyCode::BTN_TOOL_PEN, 0));
        assert!(!state.in_range());
    }

    #[test]
    fn eraser_swaps_the_tool_in_range() {
        let mut state = PenState::new();
        state.apply(&pen(Action::HoverMove, 0.5, 0.5, 0.0));
        let mut eraser = pen(Action::HoverMove, 0.5, 0.5, 0.0);
        eraser.tool = Tool::Eraser;
        let out = state.apply(&eraser);
        assert!(has_key(&out, KeyCode::BTN_TOOL_PEN, 0));
        assert!(has_key(&out, KeyCode::BTN_TOOL_RUBBER, 1));
    }

    #[test]
    fn barrel_button_follows_changes_only() {
        let mut state = PenState::new();
        let mut held = pen(Action::HoverMove, 0.5, 0.5, 0.0);
        held.buttons = BUTTON_STYLUS_PRIMARY;
        assert!(has_key(&state.apply(&held), KeyCode::BTN_STYLUS, 1));
        assert!(!has_key(&state.apply(&held), KeyCode::BTN_STYLUS, 1));
        let released = pen(Action::HoverMove, 0.5, 0.5, 0.0);
        assert!(has_key(&state.apply(&released), KeyCode::BTN_STYLUS, 0));
    }

    #[test]
    fn tilt_direction_maps_to_signed_axes() {
        // Leaning towards the top of the screen is negative Y.
        let (x, y) = PenState::tilt_axes(30f32.to_radians(), 0.0);
        assert_eq!((x, y), (0, -30));
        // Leaning right (a quarter turn clockwise) is positive X.
        let (x, y) = PenState::tilt_axes(30f32.to_radians(), std::f32::consts::FRAC_PI_2);
        assert_eq!((x, y), (30, 0));
    }

    #[test]
    fn fingers_are_ignored() {
        let mut state = PenState::new();
        let mut finger = pen(Action::Down, 0.5, 0.5, 1.0);
        finger.tool = Tool::Finger;
        assert!(state.apply(&finger).is_empty());
    }
}
