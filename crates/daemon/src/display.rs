// SPDX-License-Identifier: Apache-2.0

//! The one place that turns geometry into a compositor request.
//!
//! Hyprland and labwc spell the same thing differently: a monitor rule or a
//! Lua table on the first, `wlr-randr` arguments on the second. Rendering both
//! from one [`Layout`] keeps them from drifting apart, and it is what
//! `moreland --print-rule` prints, so the `monitor=` line in `hyprland.conf`
//! and the rule the daemon issues on connect cannot disagree.

/// A virtual output's geometry, as the compositor sees it.
#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
    /// Mode in the compositor's pre-rotation frame. A tablet panel is native
    /// landscape even when the tablet is mounted upright, so this stays
    /// `1920x1200` while `transform` decides which way it is turned.
    pub mode: (u32, u32),
    pub refresh: u32,
    pub position: (i32, i32),
    pub scale: f64,
    /// Hyprland transform id, 0-7. Odd ids are quarter turns, so they swap the
    /// logical width and height.
    pub transform: i32,
}

impl Layout {
    /// The size the compositor lays out after rotation.
    pub fn logical_size(&self) -> (u32, u32) {
        let (width, height) = self.mode;
        if self.transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// The `monitor=` rule. Also what `hyprctl keyword monitor` accepts.
    ///
    /// `transform` is written out rather than left to a default, because a rule
    /// that omits it means transform 0 and snaps a rotated output back to
    /// landscape every time it connects.
    pub fn hyprland_rule(&self) -> String {
        let (width, height) = self.mode;
        let (x, y) = self.position;
        format!(
            "{},{width}x{height}@{},{x}x{y},{},transform,{}",
            self.name, self.refresh, self.scale, self.transform
        )
    }

    /// The same rule for Hyprland's Lua config parser, which refuses `keyword`.
    pub fn hyprland_lua(&self) -> String {
        let (width, height) = self.mode;
        let (x, y) = self.position;
        format!(
            "hl.monitor({{ output = {}, mode = \"{width}x{height}@{}\", \
             position = \"{x}x{y}\", scale = {}, transform = {} }})",
            lua_string(&self.name),
            self.refresh,
            self.scale,
            self.transform
        )
    }

    /// `wlr-randr --transform` spelling, for compositors without a rule.
    ///
    /// `None` means the id is outside Hyprland's 0-7 range; the CLI rejects
    /// those before they reach here.
    pub fn wlr_randr_transform(&self) -> Option<&'static str> {
        Some(match self.transform {
            0 => "normal",
            1 => "90",
            2 => "180",
            3 => "270",
            4 => "flipped",
            5 => "flipped-90",
            6 => "flipped-180",
            7 => "flipped-270",
            _ => return None,
        })
    }
}

/// The pre-rotation mode to request for `panel`, capped on the panel's long
/// edge exactly as before rotation support existed.
///
/// `--max-width` has always meant "the width of the compositor mode", and the
/// mode stays in the panel's native landscape frame while `transform` rotates
/// it. Making the cap follow the upright image instead would quietly raise the
/// stream size on every rotated setup, which is a change nobody asked for.
/// The upright width is therefore the panel's short edge, and `--width` is the
/// way to pin a specific stream size.
pub fn panel_mode(panel: (u32, u32), max_width: u32) -> (u32, u32) {
    transport::adb::stream_resolution(panel, max_width)
}

/// Escape a value for a double-quoted Lua string.
pub fn lua_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }

    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(transform: i32) -> Layout {
        Layout {
            name: "moreland".into(),
            mode: (1920, 1200),
            refresh: 90,
            position: (-1200, -840),
            scale: 1.0,
            transform,
        }
    }

    #[test]
    fn quarter_turns_swap_the_logical_size() {
        assert_eq!(layout(0).logical_size(), (1920, 1200));
        assert_eq!(layout(1).logical_size(), (1200, 1920));
        assert_eq!(layout(2).logical_size(), (1920, 1200));
        assert_eq!(layout(3).logical_size(), (1200, 1920));
    }

    #[test]
    fn hyprland_rule_always_carries_the_transform() {
        assert_eq!(
            layout(1).hyprland_rule(),
            "moreland,1920x1200@90,-1200x-840,1,transform,1"
        );
        assert!(layout(1).hyprland_lua().contains("transform = 1"));
    }

    #[test]
    fn every_transform_has_a_wlr_randr_name() {
        let names = [
            "normal",
            "90",
            "180",
            "270",
            "flipped",
            "flipped-90",
            "flipped-180",
            "flipped-270",
        ];
        for (id, name) in names.iter().enumerate() {
            assert_eq!(layout(id as i32).wlr_randr_transform(), Some(*name));
        }
        assert_eq!(layout(8).wlr_randr_transform(), None);
    }

    #[test]
    fn width_cap_still_applies_to_the_panel_mode() {
        // Rotation does not change the stream size: the cap is on the mode,
        // which stays in the panel's landscape frame.
        assert_eq!(panel_mode((1920, 1200), 1000), (1000, 624));
        assert_eq!(panel_mode((2304, 1440), 1920), (1920, 1200));
    }
}
