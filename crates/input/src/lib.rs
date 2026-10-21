// SPDX-License-Identifier: Apache-2.0

//! Virtual input devices fed from the tablet.
//!
//! The tablet reports what it sees ([`protocol::PointerEvent`]); this crate
//! turns that into uinput devices the compositor treats as real hardware. A
//! finger becomes a multitouch screen contact. The S Pen becomes a tablet
//! tool, with pressure, tilt, hover and buttons, so drawing applications see
//! what they would on a Wacom screen.
//!
//! Translation lives in [`touch::TouchState`] and [`pen::PenState`], which only
//! produce plain `(type, code, value)` triples so they can be tested without a
//! kernel. The device wrappers just write them out.

pub mod pen;
pub mod touch;

use anyhow::Result;
use evdev::{AbsoluteAxisCode, EventType, KeyCode};
use protocol::{PointerEvent, Tool};
use std::path::PathBuf;

pub use pen::Pen;
pub use touch::Touchscreen;

/// Range of the virtual devices' absolute position axes. Events arrive as
/// fractions of the surface, so this only sets the resolution of the mapping.
pub(crate) const AXIS_MAX: i32 = 32767;

/// Rough units per millimetre, so libinput sees a plausible physical size.
/// A 12-inch tablet is about 266 mm wide; the exact value only affects
/// gesture thresholds, not where touches land.
pub(crate) const AXIS_RESOLUTION: i32 = 120;

pub(crate) type Raw = (u16, u16, i32);

pub(crate) fn abs(code: AbsoluteAxisCode, value: i32) -> Raw {
    (EventType::ABSOLUTE.0, code.0, value)
}

pub(crate) fn key(code: KeyCode, value: i32) -> Raw {
    (EventType::KEY.0, code.0, value)
}

/// Both devices, with the rules for how they interact.
pub struct RemoteInput {
    pub touch: Touchscreen,
    pub pen: Pen,
    palm_rejection: bool,
}

impl RemoteInput {
    /// Create both devices. With `palm_rejection`, a finger that lands while
    /// the pen is in range is ignored, and fingers already down are cancelled
    /// when the pen touches the glass.
    pub fn create(palm_rejection: bool) -> Result<Self> {
        Ok(Self {
            touch: Touchscreen::create("Moreland Touchscreen")?,
            pen: Pen::create("Moreland Pen")?,
            palm_rejection,
        })
    }

    /// `/dev/input/eventN` nodes of the touchscreen and the pen, once the
    /// kernel has created them.
    pub fn event_nodes(&mut self) -> Result<Option<(PathBuf, PathBuf)>> {
        Ok(match (self.touch.event_node()?, self.pen.event_node()?) {
            (Some(touch), Some(pen)) => Some((touch, pen)),
            _ => None,
        })
    }

    /// Route one pointer to the right device. Events accumulate until the last
    /// pointer of a `MotionEvent`, then both devices flush together.
    pub fn handle(&mut self, event: &PointerEvent) -> Result<()> {
        match event.tool {
            Tool::Finger => {
                let new_contact = event.action == protocol::Action::Down;
                if !(self.palm_rejection && new_contact && self.pen.in_range()) {
                    self.touch.apply(event);
                }
            }
            Tool::Pen | Tool::Eraser => {
                self.pen.apply(event);
                if self.palm_rejection && self.pen.is_touching() && self.touch.is_touching() {
                    self.touch.cancel_all();
                }
            }
        }
        if event.is_last() {
            self.touch.flush()?;
            self.pen.flush()?;
        }
        Ok(())
    }

    /// Lift everything. Called when the session ends so nothing stays stuck.
    pub fn release_all(&mut self) -> Result<()> {
        self.touch.release_all()?;
        self.pen.release_all()
    }
}
