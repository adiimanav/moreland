// SPDX-License-Identifier: Apache-2.0

//! Finger contacts as a virtual multitouch screen.

use crate::{abs, key, Raw, AXIS_MAX, AXIS_RESOLUTION};
use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, PropType, UinputAbsSetup,
};
use protocol::{Action, PointerEvent, Tool};
use std::path::PathBuf;

/// Contacts tracked at once. Ten fingers; Android hands out small ids.
pub const MAX_SLOTS: usize = 10;

/// Which contacts are down, and the events each pointer event implies.
pub struct TouchState {
    active: [bool; MAX_SLOTS],
    next_tracking_id: i32,
}

impl Default for TouchState {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            active: [false; MAX_SLOTS],
            next_tracking_id: 1,
        }
    }

    pub(crate) fn contacts(&self) -> usize {
        self.active.iter().filter(|a| **a).count()
    }

    fn position(event: &PointerEvent) -> (i32, i32) {
        let scale = |v: f32| (v.clamp(0.0, 1.0) * AXIS_MAX as f32).round() as i32;
        (scale(event.x), scale(event.y))
    }

    /// Events implied by one finger pointer. Pen pointers are not touches and
    /// produce nothing here.
    pub fn apply(&mut self, event: &PointerEvent) -> Vec<Raw> {
        if event.tool != Tool::Finger {
            return Vec::new();
        }
        let slot = usize::from(event.pointer_id);
        if slot >= MAX_SLOTS {
            return Vec::new();
        }

        let mut out = Vec::new();
        match event.action {
            Action::Down => {
                // A second Down for a live slot would leak its tracking id.
                if self.active[slot] {
                    return out;
                }
                let first = self.contacts() == 0;
                self.active[slot] = true;
                let (x, y) = Self::position(event);
                out.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
                out.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, self.next_tracking_id));
                self.next_tracking_id = (self.next_tracking_id + 1) & 0xffff;
                out.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
                out.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
                if first {
                    out.push(key(KeyCode::BTN_TOUCH, 1));
                    out.push(abs(AbsoluteAxisCode::ABS_X, x));
                    out.push(abs(AbsoluteAxisCode::ABS_Y, y));
                }
            }
            Action::Move => {
                if !self.active[slot] {
                    return out;
                }
                let (x, y) = Self::position(event);
                out.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
                out.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
                out.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
            }
            Action::Up | Action::Cancel => {
                if !self.active[slot] {
                    return out;
                }
                self.active[slot] = false;
                out.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
                out.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
                if self.contacts() == 0 {
                    out.push(key(KeyCode::BTN_TOUCH, 0));
                }
            }
            Action::HoverMove | Action::HoverExit => {}
        }
        out
    }

    /// Lift every contact. Used when the session ends so no touch is left
    /// stuck down on the desktop.
    pub fn release_all(&mut self) -> Vec<Raw> {
        let mut out = Vec::new();
        for slot in 0..MAX_SLOTS {
            if self.active[slot] {
                self.active[slot] = false;
                out.push(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot as i32));
                out.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
            }
        }
        if !out.is_empty() {
            out.push(key(KeyCode::BTN_TOUCH, 0));
        }
        out
    }
}

/// A virtual multitouch screen.
pub struct Touchscreen {
    device: VirtualDevice,
    state: TouchState,
    pending: Vec<InputEvent>,
}

impl Touchscreen {
    /// Create the device. Needs write access to `/dev/uinput`.
    pub fn create(name: &str) -> Result<Self> {
        let axis = |code, max| {
            UinputAbsSetup::new(code, AbsInfo::new(0, 0, max, 0, 0, AXIS_RESOLUTION))
        };
        let slots = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_SLOT,
            AbsInfo::new(0, 0, MAX_SLOTS as i32 - 1, 0, 0, 0),
        );
        let tracking = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_MT_TRACKING_ID,
            AbsInfo::new(0, 0, 0xffff, 0, 0, 0),
        );

        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_TOUCH);
        // DIRECT marks the device as a touchscreen, not a touchpad: contacts
        // are absolute positions on the surface.
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT);

        let device = VirtualDevice::builder()
            .context("opening /dev/uinput — is it writable by your user?")?
            .name(name)
            .with_properties(&props)?
            .with_keys(&keys)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, AXIS_MAX))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, AXIS_MAX))?
            .with_absolute_axis(&slots)?
            .with_absolute_axis(&tracking)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, AXIS_MAX))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, AXIS_MAX))?
            .build()
            .context("creating the virtual touchscreen")?;

        Ok(Self {
            device,
            state: TouchState::new(),
            pending: Vec::new(),
        })
    }

    /// The `/dev/input/eventN` node, once the kernel has created it. Returns
    /// `None` until then, so the caller can retry.
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

    /// Queue one pointer. Nothing is written until [`Self::flush`], so a
    /// whole `MotionEvent` lands as one multitouch frame.
    pub fn apply(&mut self, event: &PointerEvent) {
        let raw = self.state.apply(event);
        self.queue(raw);
    }

    /// Whether any finger is down.
    pub fn is_touching(&self) -> bool {
        self.state.contacts() > 0
    }

    /// Lift every contact as part of the current frame, e.g. when a palm lands.
    pub fn cancel_all(&mut self) {
        let raw = self.state.release_all();
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
            .context("writing to the virtual touchscreen")?;
        self.pending.clear();
        Ok(())
    }

    /// Lift everything still down and write it out at once.
    pub fn release_all(&mut self) -> Result<()> {
        self.cancel_all();
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::POINTER_FLAG_LAST;

    fn finger(action: Action, id: u8, x: f32, y: f32) -> PointerEvent {
        PointerEvent {
            action,
            tool: Tool::Finger,
            pointer_id: id,
            buttons: 0,
            flags: POINTER_FLAG_LAST,
            x,
            y,
            pressure: 1.0,
            tilt: 0.0,
            orientation: 0.0,
        }
    }

    fn has(events: &[Raw], code: AbsoluteAxisCode, value: i32) -> bool {
        events.contains(&abs(code, value))
    }

    #[test]
    fn first_touch_presses_btn_touch_and_scales_position() {
        let mut state = TouchState::new();
        let out = state.apply(&finger(Action::Down, 0, 0.5, 1.0));
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_POSITION_X, 16384));
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_POSITION_Y, AXIS_MAX));
        assert!(out.contains(&key(KeyCode::BTN_TOUCH, 1)));
    }

    #[test]
    fn btn_touch_tracks_the_first_down_and_last_up_only() {
        let mut state = TouchState::new();
        state.apply(&finger(Action::Down, 0, 0.1, 0.1));
        let second = state.apply(&finger(Action::Down, 1, 0.9, 0.9));
        assert!(!second.contains(&key(KeyCode::BTN_TOUCH, 1)));

        let lift_one = state.apply(&finger(Action::Up, 0, 0.1, 0.1));
        assert!(!lift_one.contains(&key(KeyCode::BTN_TOUCH, 0)));

        let lift_two = state.apply(&finger(Action::Up, 1, 0.9, 0.9));
        assert!(lift_two.contains(&key(KeyCode::BTN_TOUCH, 0)));
        assert!(has(&lift_two, AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
    }

    #[test]
    fn tracking_ids_are_unique_per_contact() {
        let mut state = TouchState::new();
        let a = state.apply(&finger(Action::Down, 0, 0.0, 0.0));
        state.apply(&finger(Action::Up, 0, 0.0, 0.0));
        let b = state.apply(&finger(Action::Down, 0, 0.0, 0.0));
        let id = |events: &[Raw]| {
            events
                .iter()
                .find(|e| e.1 == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0)
                .map(|e| e.2)
        };
        assert_ne!(id(&a), id(&b));
    }

    #[test]
    fn move_without_down_and_stray_up_are_ignored() {
        let mut state = TouchState::new();
        assert!(state.apply(&finger(Action::Move, 2, 0.3, 0.3)).is_empty());
        assert!(state.apply(&finger(Action::Up, 2, 0.3, 0.3)).is_empty());
    }

    #[test]
    fn duplicate_down_does_not_leak_a_contact() {
        let mut state = TouchState::new();
        state.apply(&finger(Action::Down, 0, 0.2, 0.2));
        assert!(state.apply(&finger(Action::Down, 0, 0.2, 0.2)).is_empty());
    }

    #[test]
    fn positions_outside_the_surface_are_clamped() {
        let mut state = TouchState::new();
        let out = state.apply(&finger(Action::Down, 0, -3.0, 7.0));
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_POSITION_X, 0));
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_POSITION_Y, AXIS_MAX));
    }

    #[test]
    fn pen_pointers_are_not_touches() {
        let mut state = TouchState::new();
        let mut pen = finger(Action::Down, 0, 0.5, 0.5);
        pen.tool = Tool::Pen;
        assert!(state.apply(&pen).is_empty());
    }

    #[test]
    fn release_all_lifts_every_contact() {
        let mut state = TouchState::new();
        state.apply(&finger(Action::Down, 0, 0.1, 0.1));
        state.apply(&finger(Action::Down, 4, 0.8, 0.8));
        let out = state.release_all();
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_SLOT, 0));
        assert!(has(&out, AbsoluteAxisCode::ABS_MT_SLOT, 4));
        assert!(out.contains(&key(KeyCode::BTN_TOUCH, 0)));
        assert!(state.release_all().is_empty());
    }
}
