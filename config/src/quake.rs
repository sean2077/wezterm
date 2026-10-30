use crate::keys::KeyNoAction;
use wezterm_dynamic::{FromDynamic, ToDynamic};

/// Where the quake window is anchored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum QuakeEdge {
    #[default]
    Top,
    Bottom,
}

/// Alignment along the anchored edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum QuakeAlignment {
    Start,
    #[default]
    Center,
    End,
}

/// Which monitor the quake window is placed on when it is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum QuakeMonitor {
    #[default]
    Cursor,
    Foreground,
    Primary,
}

/// Windows-only quake dropdown mode.
///
/// A dedicated WezTerm instance starts hidden, registers `hotkey` globally and
/// toggles between hidden and a work-area anchored rectangle. The window is
/// expected to use `window_decorations = "NONE"` so the outer and client
/// rectangles coincide.
#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct QuakeConfig {
    #[dynamic(default)]
    pub enabled: bool,

    #[dynamic(default)]
    pub hotkey: Option<KeyNoAction>,

    /// Keep the window hidden at startup; the global hotkey shows it.
    #[dynamic(default)]
    pub start_hidden: bool,

    /// Hide the window when it loses focus, unless an owned window has focus.
    #[dynamic(default)]
    pub hide_on_focus_loss: bool,

    /// Keep the window above other windows while it is shown.
    #[dynamic(default = "default_quake_true")]
    pub always_on_top: bool,

    #[dynamic(default)]
    pub edge: QuakeEdge,

    #[dynamic(default)]
    pub alignment: QuakeAlignment,

    #[dynamic(default = "default_quake_width", validate = "validate_quake_percent")]
    pub width_percent: u8,

    #[dynamic(default = "default_quake_height", validate = "validate_quake_percent")]
    pub height_percent: u8,

    #[dynamic(default)]
    pub monitor: QuakeMonitor,
}

impl Default for QuakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hotkey: None,
            start_hidden: false,
            hide_on_focus_loss: false,
            always_on_top: true,
            edge: QuakeEdge::Top,
            alignment: QuakeAlignment::Center,
            width_percent: 100,
            height_percent: 45,
            monitor: QuakeMonitor::Cursor,
        }
    }
}

fn default_quake_true() -> bool {
    true
}

fn default_quake_width() -> u8 {
    100
}

fn default_quake_height() -> u8 {
    45
}

fn validate_quake_percent(value: &u8) -> Result<(), String> {
    if (10..=100).contains(value) {
        Ok(())
    } else {
        Err(format!(
            "quake width_percent/height_percent must be between 10 and 100, got {value}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quake_defaults_are_stable() {
        let q = QuakeConfig::default();
        assert!(!q.enabled);
        assert!(q.hotkey.is_none());
        assert!(!q.start_hidden);
        assert!(!q.hide_on_focus_loss);
        assert!(q.always_on_top);
        assert_eq!(q.edge, QuakeEdge::Top);
        assert_eq!(q.alignment, QuakeAlignment::Center);
        assert_eq!((q.width_percent, q.height_percent), (100, 45));
        assert_eq!(q.monitor, QuakeMonitor::Cursor);
    }

    #[test]
    fn quake_rejects_out_of_range_percentages() {
        for value in [0u8, 9, 101, 255] {
            assert!(validate_quake_percent(&value).is_err());
        }
        assert!(validate_quake_percent(&45).is_ok());
    }
}
