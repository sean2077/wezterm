use crate::termwindow::render::window_buttons::traffic_light_width;
use crate::termwindow::{PaneInformation, TabInformation, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{ConfigHandle, DimensionContext, TabBarColors};
use finl_unicode::grapheme_clusters::Graphemes;
use mlua::FromLua;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, Cell, CellAttributes};
use termwiz::color::{AnsiColor, ColorSpec, SrgbaTuple};
use termwiz::escape::csi::Sgr;
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, ControlCode, CSI};
use termwiz::surface::SEQ_ZERO;
use termwiz_funcs::{format_as_escapes, FormatColor, FormatItem};
use wezterm_term::{Line, Progress};
use window::{IntegratedTitleButton, IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle};

#[derive(Clone, Debug, PartialEq)]
pub struct TabBarState {
    line: Line,
    items: Vec<TabEntry>,
    /// When a tab shows the built-in indeterminate progress spinner, the instant
    /// at which the tab bar should be rebuilt to advance to the next frame;
    /// None when no spinner is visible.
    next_progress_frame_due: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarItem {
    None,
    LeftStatus,
    RightStatus,
    Tab { tab_idx: usize, active: bool },
    NewTabButton,
    WindowButton(IntegratedTitleButton),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabEntry {
    pub item: TabBarItem,
    pub title: Line,
    pub right_title: Option<Line>,
    /// Drives the full waiting-border ring in the fancy tab bar.
    pub agent_waiting: bool,
    /// The tab's accent color, declared by format-tab-title with U+001D.
    /// Colors the separator and door when this tab is active.
    pub accent: Option<SrgbaTuple>,
    x: usize,
    width: usize,
}

#[derive(Clone, Debug)]
struct TitleText {
    items: Vec<FormatItem>,
    len: usize,
    /// True when the built-in title path rendered an indeterminate spinner
    /// frame. A custom format-tab-title callback always leaves this false.
    has_indeterminate: bool,
    /// Text after a U+001F marker in the callback result. The tab bar renders
    /// this as a right-floated element so it stays on the right edge even with
    /// a proportional title font.
    right: Option<Line>,
    /// Width of the fixed left block (index + badge), marked by U+001E in the
    /// callback result. Zero when the callback does not use the marker.
    left_len: usize,
    /// Foreground color in effect at the first U+001D marker.
    accent: Option<SrgbaTuple>,
}

const RIGHT_MARKER: char = '\u{1f}';
const TITLE_MARKER: char = '\u{1e}';
const ACCENT_MARKER: char = '\u{1d}';

/// Strip U+001D markers from a format-tab-title result. The foreground color
/// in effect at the first marker (`{Foreground={Color=...}}, {Text='\x1d'}`)
/// is the tab's accent; only explicit RGB colors count.
fn take_accent(items: Vec<FormatItem>) -> (Vec<FormatItem>, Option<SrgbaTuple>) {
    let mut fg: Option<FormatColor> = None;
    let mut accent = None;
    let mut seen = false;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            FormatItem::Foreground(color) => {
                fg = Some(color.clone());
                out.push(FormatItem::Foreground(color));
            }
            FormatItem::ResetAttributes => {
                fg = None;
                out.push(FormatItem::ResetAttributes);
            }
            FormatItem::Text(text) if text.contains(ACCENT_MARKER) => {
                if !seen {
                    seen = true;
                    accent = match &fg {
                        Some(FormatColor::Color(s)) => s.parse::<SrgbaTuple>().ok(),
                        _ => None,
                    };
                }
                let rest: String = text.chars().filter(|&c| c != ACCENT_MARKER).collect();
                if !rest.is_empty() {
                    out.push(FormatItem::Text(rest));
                }
            }
            other => out.push(other),
        }
    }
    (out, accent)
}

/// Split a FormatItem list at the first occurrence of `marker`, returning the
/// items before it, the items after it, and whether the marker was present.
fn split_marker(items: Vec<FormatItem>, marker: char) -> (Vec<FormatItem>, Vec<FormatItem>, bool) {
    for (idx, item) in items.iter().enumerate() {
        if let FormatItem::Text(s) = item {
            if let Some(pos) = s.find(marker) {
                let mut left: Vec<FormatItem> = items[..idx].to_vec();
                let left_text = s[..pos].to_string();
                if !left_text.is_empty() {
                    left.push(FormatItem::Text(left_text));
                }
                let mut right: Vec<FormatItem> = Vec::new();
                let right_text = s[pos + marker.len_utf8()..].to_string();
                if !right_text.is_empty() {
                    right.push(FormatItem::Text(right_text));
                }
                right.extend(items[idx + 1..].iter().cloned());
                return (left, right, true);
            }
        }
    }
    (items, vec![], false)
}

fn split_right(items: Vec<FormatItem>) -> (Vec<FormatItem>, Option<Vec<FormatItem>>) {
    let (left, right, found) = split_marker(items, RIGHT_MARKER);
    (
        left,
        if found && !right.is_empty() {
            Some(right)
        } else {
            None
        },
    )
}

fn call_format_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    hover: bool,
    tab_max_width: usize,
) -> Option<TitleText> {
    match config::run_immediate_with_lua_config(|lua| {
        if let Some(lua) = lua {
            let tabs = lua.create_sequence_from(tab_info.iter().cloned())?;
            let panes = lua.create_sequence_from(pane_info.iter().cloned())?;

            let v = config::lua::emit_sync_callback(
                &*lua,
                (
                    "format-tab-title".to_string(),
                    (
                        tab.clone(),
                        tabs,
                        panes,
                        (**config).clone(),
                        hover,
                        tab_max_width,
                    ),
                ),
            )?;
            match &v {
                mlua::Value::Nil => Ok(None),
                mlua::Value::Table(_) => {
                    let items = <Vec<FormatItem>>::from_lua(v, &*lua)?;
                    let (items, accent) = take_accent(items);
                    let (left_items, right_items) = split_right(items);
                    // U+001E separates the fixed left block (index + badge)
                    // from the title. The tab bar measures the left block and
                    // passes the rest of the share to the title as its budget.
                    let (left_items, title_items, has_title_marker) =
                        split_marker(left_items, TITLE_MARKER);
                    let left_len = if has_title_marker {
                        let esc = format_as_escapes(left_items.clone())?;
                        parse_status_text(&esc, CellAttributes::default()).len()
                    } else {
                        0
                    };
                    let mut all_items = left_items;
                    all_items.extend(title_items);
                    let esc = format_as_escapes(all_items.clone())?;
                    let line = parse_status_text(&esc, CellAttributes::default());
                    let right = match right_items {
                        Some(items) => {
                            let esc = format_as_escapes(items)?;
                            Some(parse_status_text(&esc, CellAttributes::default()))
                        }
                        None => None,
                    };

                    Ok(Some(TitleText {
                        items: all_items,
                        len: line.len(),
                        has_indeterminate: false,
                        right,
                        left_len,
                        accent,
                    }))
                }
                _ => {
                    let s = String::from_lua(v, &*lua)?;
                    let (left, right) = match s.find(RIGHT_MARKER) {
                        Some(pos) => (
                            s[..pos].to_string(),
                            Some(s[pos + RIGHT_MARKER.len_utf8()..].to_string()),
                        ),
                        None => (s, None),
                    };
                    let line = parse_status_text(&left, CellAttributes::default());
                    let right = right.map(|r| parse_status_text(&r, CellAttributes::default()));
                    Ok(Some(TitleText {
                        len: line.len(),
                        items: vec![FormatItem::Text(left)],
                        has_indeterminate: false,
                        right,
                        left_len: 0,
                        accent: None,
                    }))
                }
            }
        } else {
            Ok(None)
        }
    }) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("format-tab-title: {}", err);
            None
        }
    }
}

/// pct is a percentage in the range 0-100.
/// We want to map it to one of the nerdfonts:
///
/// * `md-checkbox_blank_circle_outline` (0xf0130) for an empty circle
/// * `md_circle_slice_1..=7` (0xf0a9e ..= 0xf0aa4) for a partly filled
///   circle
/// * `md_circle_slice_8` (0xf0aa5) for a filled circle
///
/// We use an empty circle for values close to 0%, a filled circle for values
/// close to 100%, and a partly filled circle for the rest (roughly evenly
/// distributed).
fn pct_to_glyph(pct: u8) -> char {
    match pct {
        0..=5 => '\u{f0130}',    // empty circle
        6..=18 => '\u{f0a9e}',   // centered at 12 (slightly smaller than 12.5)
        19..=31 => '\u{f0a9f}',  // centered at 25
        32..=43 => '\u{f0aa0}',  // centered at 37.5
        44..=56 => '\u{f0aa1}',  // half-filled circle, centered at 50
        57..=68 => '\u{f0aa2}',  // centered at 62.5
        69..=81 => '\u{f0aa3}',  // centered at 75
        82..=94 => '\u{f0aa4}',  // centered at 88 (slightly larger than 87.5)
        95..=100 => '\u{f0aa5}', // filled circle
        // Any other value is mapped to a filled circle.
        _ => '\u{f0aa5}',
    }
}

/// How long each indeterminate progress spinner frame is shown before the next
/// is due.
const INDETERMINATE_SPINNER_INTERVAL: Duration = Duration::from_millis(100);

/// Reference instant for the indeterminate spinner. The displayed frame and its
/// next-due time are both derived from the elapsed time since this instant, so
/// they stay in step no matter when a repaint happens to rebuild the tab bar.
static SPINNER_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Renders `value` as a braille cell whose lit dots count up in the cell's
/// reading order, down the left column then down the right column. Stepping
/// `value` through 0..=255 reproduces the `dots8Bit` animation of
/// https://github.com/sindresorhus/cli-spinners without a lookup table.
fn braille_counter(value: u8) -> char {
    // Unicode braille dot bit values in reading order: dots 1, 2, 3, 7 fill the
    // left column and dots 4, 5, 6, 8 the right column.
    const DOTS: [u32; 8] = [0x01, 0x02, 0x04, 0x40, 0x08, 0x10, 0x20, 0x80];
    let mut pattern = 0u32;
    for (bit, dot) in DOTS.iter().enumerate() {
        if value & (1 << bit) != 0 {
            pattern |= dot;
        }
    }
    char::from_u32(0x2800 + pattern).expect("braille pattern is a valid codepoint")
}

/// Returns the spinner glyph to show for the current moment, advancing one
/// frame per INDETERMINATE_SPINNER_INTERVAL. `seed` offsets the starting frame
/// so that tabs busy at the same time do not animate in lock step.
fn indeterminate_spinner_glyph(seed: u64) -> char {
    let elapsed = SPINNER_EPOCH.elapsed().as_millis() as u64;
    let interval = INDETERMINATE_SPINNER_INTERVAL.as_millis() as u64;
    // braille_counter wraps at 256, matching the animation's frame count.
    braille_counter((elapsed / interval + seed) as u8)
}

/// Returns the instant at which the spinner next advances a frame, snapped to
/// the frame grid measured from SPINNER_EPOCH. Because the result falls on a
/// grid boundary rather than a fixed offset from now, repeated rebuilds within
/// one frame all return the same instant and the animation advances steadily
/// even when unrelated repaints rebuild the tab bar in between.
fn next_spinner_frame_due() -> Instant {
    let interval = INDETERMINATE_SPINNER_INTERVAL.as_nanos();
    let elapsed = SPINNER_EPOCH.elapsed().as_nanos();
    let next_frame = elapsed / interval + 1;
    *SPINNER_EPOCH + Duration::from_nanos((next_frame * interval) as u64)
}

/// Scrambles a tab id into a spinner phase offset. Tab ids are usually handed
/// out sequentially, which would leave adjacent tabs only one frame apart; the
/// splitmix64 finalizer avalanches the low bits so their spinners spread across
/// the animation instead.
fn spinner_phase(tab_id: usize) -> u64 {
    let mut z = tab_id as u64;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

fn compute_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    hover: bool,
    tab_max_width: usize,
) -> TitleText {
    let title = call_format_tab_title(tab, tab_info, pane_info, config, hover, tab_max_width);

    match title {
        Some(title) => title,
        None => {
            let mut items = vec![];
            let mut len = 0;
            let mut has_indeterminate = false;

            if let Some(pane) = &tab.active_pane {
                let mut title = if tab.tab_title.is_empty() {
                    pane.title.clone()
                } else {
                    tab.tab_title.clone()
                };

                let classic_spacing = if config.use_fancy_tab_bar { "" } else { " " };
                if config.show_tab_index_in_tab_bar {
                    let index = format!(
                        "{classic_spacing}{}: ",
                        tab.tab_index
                            + if config.tab_and_split_indices_are_zero_based {
                                0
                            } else {
                                1
                            }
                    );
                    len += unicode_column_width(&index, None);
                    items.push(FormatItem::Text(index));

                    title = format!("{}{classic_spacing}", title);
                }

                match pane.progress {
                    Progress::None => {}
                    Progress::Percentage(pct) | Progress::Error(pct) => {
                        let graphic = format!("{} ", pct_to_glyph(pct));
                        len += unicode_column_width(&graphic, None);
                        let color = if matches!(pane.progress, Progress::Percentage(_)) {
                            FormatItem::Foreground(FormatColor::AnsiColor(AnsiColor::Green))
                        } else {
                            FormatItem::Foreground(FormatColor::AnsiColor(AnsiColor::Red))
                        };
                        items.push(color);
                        items.push(FormatItem::Text(graphic));
                        items.push(FormatItem::Foreground(FormatColor::Default));
                    }
                    Progress::Indeterminate => {
                        has_indeterminate = true;
                        let graphic = format!(
                            "{} ",
                            indeterminate_spinner_glyph(spinner_phase(tab.tab_id))
                        );
                        len += unicode_column_width(&graphic, None);
                        items.push(FormatItem::Foreground(FormatColor::AnsiColor(
                            AnsiColor::Green,
                        )));
                        items.push(FormatItem::Text(graphic));
                        items.push(FormatItem::Foreground(FormatColor::Default));
                    }
                }

                // We have a preferred soft minimum on tab width to make it
                // easier to click on tab titles, but we'll still go below
                // this if there are too many tabs to fit the window at
                // this width.
                if !config.use_fancy_tab_bar {
                    while len + unicode_column_width(&title, None) < 5 {
                        title.push(' ');
                    }
                }

                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            } else {
                let title = " no pane ".to_string();
                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            };

            TitleText {
                len,
                items,
                has_indeterminate,
                right: None,
                left_len: 0,
                accent: None,
            }
        }
    }
}

fn is_tab_hover(mouse_x: Option<usize>, x: usize, tab_title_len: usize) -> bool {
    return mouse_x
        .map(|mouse_x| mouse_x >= x && mouse_x < x + tab_title_len)
        .unwrap_or(false);
}

/// Width of the integrated window buttons drawn inside the tab bar, in
/// pixels.  These are real elements, not characters, so they cannot be
/// estimated in cells: a Windows glyph button is about four cells wide at
/// 96 DPI and scales with the DPI.
fn window_buttons_width(config: &ConfigHandle, metrics: &RenderMetrics, dpi: f32) -> f32 {
    let count = config.integrated_title_buttons.len() as f32;
    if count == 0. {
        return 0.;
    }
    match config.integrated_title_button_style {
        IntegratedTitleButtonStyle::MacOsNative => {
            if cfg!(target_os = "macos") {
                // Drawn in the native titlebar frame, outside the bar.
                0.
            } else {
                count * traffic_light_width(metrics)
            }
        }
        IntegratedTitleButtonStyle::Windows => {
            // Matches window_buttons::windows::window_button_element:
            // 10pt glyph, 17pt/18pt left padding, 18pt right padding, all
            // scaled by 72/96 and then converted at the window DPI.
            let scale = 72.0 / 96.0;
            let px = |pt: f32| (pt * scale * dpi / 72.0).floor();
            let glyph = px(10.);
            let right = px(18.);
            config
                .integrated_title_buttons
                .iter()
                .map(|button| match button {
                    IntegratedTitleButton::Hide => glyph + px(17.) + right,
                    _ => glyph + px(18.) + right,
                })
                .sum()
        }
        IntegratedTitleButtonStyle::Gnome => {
            // 8px glyph + 7px padding + 1px border + 7px margin on each side.
            count * 38.
        }
    }
}

/// Widths of the fixed elements that share the fancy tab bar with the tabs.
/// The tab layout (build_fancy_tab_bar) and the title budget / hover pass use
/// the same numbers, so the callback's budget matches what is rendered.
pub(crate) struct FancySideWidths {
    /// Left group: leading padding, left status text and left-aligned window
    /// buttons.  The tabs start after this.
    pub left_px: f32,
    /// Right status text and right-floated window buttons.
    pub right_px: f32,
    /// The new-tab (`+`) button, which also consumes bar space.
    pub new_tab_px: f32,
}

pub(crate) fn fancy_side_widths(
    config: &ConfigHandle,
    metrics: &RenderMetrics,
    dpi: f32,
    left_status: &str,
    right_status: &str,
) -> FancySideWidths {
    let cell = metrics.cell_size.width as f32;
    let cell_h = metrics.cell_size.height as f32;
    let ratio = (config.tab_bar_side_reserve_ratio as f32).clamp(0.0, 1.0);
    // Status elements render as a text run with 0.5 cell of left padding.
    let status_px = |text: &str| {
        if text.is_empty() {
            0.
        } else {
            (parse_status_text(text, CellAttributes::default()).len() as f32 * cell + 0.5 * cell)
                * ratio
        }
    };
    let integrated = config
        .window_decorations
        .contains(window::WindowDecorations::INTEGRATED_BUTTONS);
    let buttons_px = if integrated {
        window_buttons_width(config, metrics, dpi)
    } else {
        0.
    };
    let buttons_at_left = integrated
        && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
        && !(config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            && cfg!(target_os = "macos"));
    // The left group always carries 0.5 cell of leading padding, except when
    // the window buttons occupy that spot.
    let leading = if buttons_at_left { 0. } else { 0.5 * cell };
    let new_tab_px = if config.show_new_tab_button_in_tab_bar {
        // Poly height/2 + 1 cell of horizontal padding + 2px border + 0.5 cell
        // left margin, matching the NewTabButton element.
        0.5 * cell_h + 1.5 * cell + 2.
    } else {
        0.
    };
    FancySideWidths {
        left_px: leading + status_px(left_status) + if buttons_at_left { buttons_px } else { 0. },
        right_px: status_px(right_status) + if buttons_at_left { 0. } else { buttons_px },
        new_tab_px,
    }
}

/// Everything the tab builder needs to compute exact budgets and hover ranges
/// for uniform tabs.  All widths are pixels measured from the left edge of
/// the tab bar.
pub(crate) struct FancyTabMetrics {
    /// Outer width each tab receives in uniform mode.
    pub share_px: f32,
    /// Title-font cell width.
    pub cell_px: f32,
    /// Tab border width in pixels.
    pub border_px: f32,
    /// Close-button element width when shown.
    pub close_px: f32,
    /// Where the first tab starts (the left group's width).
    pub left_px: f32,
    /// Mouse x in tab-bar pixels, when the pointer is on the bar row.
    pub mouse_px: Option<f32>,
}

pub(crate) fn fancy_tab_metrics(
    config: &ConfigHandle,
    metrics: &RenderMetrics,
    dpi: f32,
    window_px: f32,
    left_status: &str,
    right_status: &str,
    tab_count: usize,
    mouse_px: Option<f32>,
) -> FancyTabMetrics {
    let cell_px = (metrics.cell_size.width as f32).max(1.);
    let side = fancy_side_widths(config, metrics, dpi, left_status, right_status);
    let uniform = (window_px - side.left_px - side.right_px - side.new_tab_px).max(0.);
    let share_px = if tab_count > 0 {
        (uniform / tab_count as f32).max(0.)
    } else {
        0.
    };
    let border_px = config
        .tab_bar_tab_border_width
        .evaluate_as_pixels(DimensionContext {
            dpi,
            pixel_max: window_px,
            pixel_cell: cell_px,
        });
    FancyTabMetrics {
        share_px,
        cell_px,
        border_px,
        close_px: metrics.cell_size.height as f32 / 2.,
        left_px: side.left_px,
        mouse_px,
    }
}

/// `agent_state` is written as `state` or `state:<unix-epoch>` by the
/// notification helpers. Only the state prefix matters for the tab ring.
/// Epoch carried by an `agent_state` value such as `waiting:1762500000`.
pub(crate) fn agent_state_epoch(value: &str) -> Option<u64> {
    value
        .split_once(':')
        .and_then(|(_, epoch)| epoch.parse::<u64>().ok())
}

/// True when the value says the pane is waiting for input and its optional
/// `:epoch` timestamp has not exceeded `ttl_seconds`.  A zero TTL disables the
/// expiry (upstream behavior); a value without a numeric epoch never expires.
pub(crate) fn agent_state_is_waiting(value: &str, now_epoch: u64, ttl_seconds: u64) -> bool {
    let Some((state, epoch)) = value.split_once(':') else {
        return value == "waiting";
    };
    if state != "waiting" {
        return false;
    }
    let Ok(epoch) = epoch.parse::<u64>() else {
        return true;
    };
    if ttl_seconds == 0 {
        return true;
    }
    now_epoch < epoch.saturating_add(ttl_seconds)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_state_epoch, agent_state_is_waiting, take_accent, FormatColor, FormatItem, SrgbaTuple,
    };

    #[test]
    fn detects_waiting_agent_state() {
        assert!(agent_state_is_waiting("waiting", 100, 0));
        assert!(agent_state_is_waiting("waiting:1762500000", 100, 0));
        assert!(agent_state_is_waiting("waiting:100", 100, 10));
        assert!(agent_state_is_waiting("waiting:100", 109, 10));
        assert!(!agent_state_is_waiting("waiting:100", 110, 10));
        // A non-numeric epoch cannot be aged out.
        assert!(agent_state_is_waiting("waiting:soon", 1000, 10));
        assert!(agent_state_is_waiting("waiting:", 1000, 10));
        assert!(!agent_state_is_waiting("running:1762500000", 100, 0));
        assert!(!agent_state_is_waiting("done:1762500000", 100, 0));
        assert!(!agent_state_is_waiting("error:1762500000", 100, 0));
        assert!(!agent_state_is_waiting("waiting-room", 100, 0));
        assert!(!agent_state_is_waiting("", 100, 0));
    }

    #[test]
    fn extracts_the_agent_state_epoch() {
        assert_eq!(agent_state_epoch("waiting:1762500000"), Some(1762500000));
        assert_eq!(agent_state_epoch("waiting"), None);
        assert_eq!(agent_state_epoch("waiting:soon"), None);
    }

    fn fg(s: &str) -> FormatItem {
        FormatItem::Foreground(FormatColor::Color(s.to_string()))
    }

    fn text(s: &str) -> FormatItem {
        FormatItem::Text(s.to_string())
    }

    #[test]
    fn accent_is_the_foreground_at_the_marker_and_the_marker_is_removed() {
        let (items, accent) = take_accent(vec![
            fg("#a02a28"),
            text("\u{1d}"),
            fg("#eff1f5"),
            text("1|"),
        ]);
        assert_eq!(accent, "#a02a28".parse::<SrgbaTuple>().ok());
        assert_eq!(items, vec![fg("#a02a28"), fg("#eff1f5"), text("1|")]);
    }

    #[test]
    fn accent_marker_inside_text_keeps_the_rest_and_first_marker_wins() {
        let (items, accent) = take_accent(vec![
            fg("#123456"),
            text("a\u{1d}b"),
            fg("#654321"),
            text("\u{1d}c"),
        ]);
        assert_eq!(accent, "#123456".parse::<SrgbaTuple>().ok());
        assert_eq!(
            items,
            vec![fg("#123456"), text("ab"), fg("#654321"), text("c")]
        );
    }

    #[test]
    fn no_marker_or_no_rgb_foreground_means_no_accent() {
        let (items, accent) = take_accent(vec![fg("#123456"), text("plain")]);
        assert_eq!(accent, None);
        assert_eq!(items, vec![fg("#123456"), text("plain")]);
        let (_, accent) = take_accent(vec![
            fg("#123456"),
            FormatItem::ResetAttributes,
            text("\u{1d}"),
        ]);
        assert_eq!(accent, None);
    }
}

impl TabBarState {
    pub fn default() -> Self {
        Self {
            line: Line::with_width(1, SEQ_ZERO),
            items: vec![TabEntry {
                item: TabBarItem::None,
                title: Line::from_text(" ", &CellAttributes::blank(), 1, None),
                right_title: None,
                agent_waiting: false,
                accent: None,
                x: 1,
                width: 1,
            }],
            next_progress_frame_due: None,
        }
    }

    pub fn line(&self) -> &Line {
        &self.line
    }

    pub fn items(&self) -> &[TabEntry] {
        &self.items
    }

    pub fn next_progress_frame_due(&self) -> Option<Instant> {
        self.next_progress_frame_due
    }

    fn integrated_title_buttons(
        mouse_x: Option<usize>,
        x: &mut usize,
        config: &ConfigHandle,
        items: &mut Vec<TabEntry>,
        line: &mut Line,
        colors: &TabBarColors,
    ) {
        let default_cell = if config.use_fancy_tab_bar {
            CellAttributes::default()
        } else {
            colors.new_tab().as_cell_attributes()
        };

        let default_cell_hover = if config.use_fancy_tab_bar {
            CellAttributes::default()
        } else {
            colors.new_tab_hover().as_cell_attributes()
        };

        let window_hide =
            parse_status_text(&config.tab_bar_style.window_hide, default_cell.clone());
        let window_hide_hover = parse_status_text(
            &config.tab_bar_style.window_hide_hover,
            default_cell_hover.clone(),
        );

        let window_maximize =
            parse_status_text(&config.tab_bar_style.window_maximize, default_cell.clone());
        let window_maximize_hover = parse_status_text(
            &config.tab_bar_style.window_maximize_hover,
            default_cell_hover.clone(),
        );

        let window_close =
            parse_status_text(&config.tab_bar_style.window_close, default_cell.clone());
        let window_close_hover = parse_status_text(
            &config.tab_bar_style.window_close_hover,
            default_cell_hover.clone(),
        );

        for button in &config.integrated_title_buttons {
            use IntegratedTitleButton as Button;
            let title = match button {
                Button::Hide => {
                    let hover = is_tab_hover(mouse_x, *x, window_hide_hover.len());

                    if hover {
                        &window_hide_hover
                    } else {
                        &window_hide
                    }
                }
                Button::Maximize => {
                    let hover = is_tab_hover(mouse_x, *x, window_maximize_hover.len());

                    if hover {
                        &window_maximize_hover
                    } else {
                        &window_maximize
                    }
                }
                Button::Close => {
                    let hover = is_tab_hover(mouse_x, *x, window_close_hover.len());

                    if hover {
                        &window_close_hover
                    } else {
                        &window_close
                    }
                }
            };

            line.append_line(title.to_owned(), SEQ_ZERO);

            let width = title.len();
            items.push(TabEntry {
                item: TabBarItem::WindowButton(*button),
                title: title.to_owned(),
                right_title: None,
                agent_waiting: false,
                accent: None,
                x: *x,
                width,
            });

            *x += width;
        }
    }

    /// Build a new tab bar from the current state
    /// mouse_x is some if the mouse is on the same row as the tab bar.
    /// title_width is the total number of cell columns in the window.
    /// window allows access to the tabs associated with the window.
    pub fn new(
        title_width: usize,
        mouse_x: Option<usize>,
        tab_info: &[TabInformation],
        pane_info: &[PaneInformation],
        colors: Option<&TabBarColors>,
        config: &ConfigHandle,
        left_status: &str,
        right_status: &str,
        fancy_title_width: Option<usize>,
        fancy_tab: Option<&FancyTabMetrics>,
    ) -> Self {
        let colors = colors.cloned().unwrap_or_else(TabBarColors::default);

        let active_cell_attrs = colors.active_tab().as_cell_attributes();
        let inactive_hover_attrs = colors.inactive_tab_hover().as_cell_attributes();
        let inactive_cell_attrs = colors.inactive_tab().as_cell_attributes();
        let new_tab_hover_attrs = colors.new_tab_hover().as_cell_attributes();
        let new_tab_attrs = colors.new_tab().as_cell_attributes();

        let new_tab = parse_status_text(
            &config.tab_bar_style.new_tab,
            if config.use_fancy_tab_bar {
                CellAttributes::default()
            } else {
                new_tab_attrs.clone()
            },
        );
        let new_tab_hover = parse_status_text(
            &config.tab_bar_style.new_tab_hover,
            if config.use_fancy_tab_bar {
                CellAttributes::default()
            } else {
                new_tab_hover_attrs.clone()
            },
        );

        let use_integrated_title_buttons = config
            .window_decorations
            .contains(window::WindowDecorations::INTEGRATED_BUTTONS);

        // We ultimately want to produce a line looking like this:
        // ` | tab1-title x | tab2-title x |  +      . - X `
        // Where the `+` sign will spawn a new tab (or show a context
        // menu with tab creation options) and the other three chars
        // are symbols representing minimize, maximize and close.

        let mut active_tab_no = 0;
        let number_of_tabs = tab_info.len();
        // The first pass needs a budget for format-tab-title. In fancy mode
        // the callback used to receive config.tab_max_width regardless of the
        // window size; use the title font's cell width to turn the real window
        // width into a title-cell budget.
        let first_pass_max = if config.use_fancy_tab_bar {
            if config.tab_bar_uniform_width {
                if let Some(fancy) = fancy_tab {
                    // Uniform tabs: the real per-tab share in title cells.
                    // The second pass charges padding, borders, the close
                    // button and the right suffix exactly.
                    let share_cells = (fancy.share_px / fancy.cell_px).floor() as usize;
                    share_cells.saturating_sub(2).max(1)
                } else {
                    fancy_title_width.unwrap_or(config.tab_max_width).max(1)
                }
            } else {
                // The title budget must exclude the left/right status text and
                // the integrated window buttons; otherwise the padded title is
                // wider than the actual tab and gets truncated.
                let status_cells = parse_status_text(left_status, CellAttributes::default()).len()
                    + parse_status_text(right_status, CellAttributes::default()).len();
                let window_buttons = if config
                    .window_decorations
                    .contains(window::WindowDecorations::INTEGRATED_BUTTONS)
                {
                    3
                } else {
                    0
                };
                let close_button = if config.show_close_tab_button_in_tabs {
                    3
                } else {
                    0
                };
                // Match the 0.6 side-reserve factor used by the fancy layout,
                // plus roughly the new-tab button.
                let reserved = (((status_cells + window_buttons) as f32)
                    * (config.tab_bar_side_reserve_ratio as f32).clamp(0.0, 1.0))
                    as usize
                    + 2
                    + close_button;
                let width = fancy_title_width.unwrap_or(config.tab_max_width);
                (width.saturating_sub(reserved) / number_of_tabs.max(1)).max(1)
            }
        } else {
            config.tab_max_width
        };

        let tab_titles: Vec<TitleText> = if config.show_tabs_in_tab_bar {
            tab_info
                .iter()
                .map(|tab| {
                    if tab.is_active {
                        active_tab_no = tab.tab_index;
                    }
                    compute_tab_title(tab, tab_info, pane_info, config, false, first_pass_max)
                })
                .collect()
        } else {
            vec![]
        };
        let titles_len: usize = tab_titles.iter().map(|s| s.len).sum();

        let available_cells =
            title_width.saturating_sub(number_of_tabs.saturating_sub(1) + new_tab.len());
        let tab_width_max = if config.use_fancy_tab_bar {
            first_pass_max
        } else {
            (if available_cells >= titles_len {
                // We can render each title with its full width
                usize::max_value()
            } else {
                // We need to clamp the length to balance them out
                available_cells / number_of_tabs.max(1)
            })
            .min(config.tab_max_width)
        };

        let mut line = Line::with_width(0, SEQ_ZERO);

        let mut x = 0;
        let mut items = vec![];
        let mut has_indeterminate_progress = false;

        let black_cell = Cell::blank_with_attrs(
            CellAttributes::default()
                .set_background(ColorSpec::TrueColor(*colors.background()))
                .clone(),
        );

        // macOS draws the traffic lights itself; elsewhere the fork renders
        // them, so the buttons go through the regular item path below.
        let native_macos_buttons = config.integrated_title_button_style
            == IntegratedTitleButtonStyle::MacOsNative
            && cfg!(target_os = "macos");

        if use_integrated_title_buttons
            && native_macos_buttons
            && config.use_fancy_tab_bar == false
            && config.tab_bar_at_bottom == false
        {
            for _ in 0..10 as usize {
                line.insert_cell(0, black_cell.clone(), title_width, SEQ_ZERO);
                x += 1;
            }
        }

        if use_integrated_title_buttons
            && !native_macos_buttons
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
        {
            Self::integrated_title_buttons(mouse_x, &mut x, config, &mut items, &mut line, &colors);
        }

        let left_status_line = parse_status_text(left_status, black_cell.attrs().clone());
        if left_status_line.len() > 0 {
            items.push(TabEntry {
                item: TabBarItem::LeftStatus,
                title: left_status_line.clone(),
                right_title: None,
                agent_waiting: false,
                accent: None,
                x,
                width: left_status_line.len(),
            });
            x += left_status_line.len();
            line.append_line(left_status_line, SEQ_ZERO);
        }

        // Uniform tabs have a fixed pixel share; track their true on-screen range so
        // hover does not rely on title-cell counts.
        let mut tab_x_px = fancy_tab.map(|fancy| fancy.left_px).unwrap_or(0.);

        for (tab_idx, tab_title) in tab_titles.iter().enumerate() {
            let tab_title_len = tab_title.len.min(tab_width_max);
            let active = tab_idx == active_tab_no;
            let hover = !active
                && match fancy_tab {
                    Some(fancy) if config.use_fancy_tab_bar && config.tab_bar_uniform_width => {
                        fancy.mouse_px.map_or(false, |mouse| {
                            mouse >= tab_x_px && mouse < tab_x_px + fancy.share_px
                        })
                    }
                    _ => is_tab_hover(mouse_x, x, tab_title_len),
                };

            // Recompute the title so that it factors in both the hover state
            // and the adjusted maximum tab width based on available space.
            // Under uniform tabs the callback marks its fixed left block with
            // U+001E; the share's content box, minus that block, the floated
            // suffix and the close button, is the title's exact budget in
            // title-font cells.  The same cells cap the rendered line, so the
            // ellipsis is not clipped by a stricter cell count.
            let (second_pass_max, line_cap) = match fancy_tab {
                Some(fancy) if config.use_fancy_tab_bar && config.tab_bar_uniform_width => {
                    let right_px = tab_title.right.as_ref().map(|r| r.len()).unwrap_or(0) as f32
                        * fancy.cell_px;
                    let close_px = if config.show_close_tab_button_in_tabs {
                        fancy.close_px
                    } else {
                        0.
                    };
                    let content_px =
                        (fancy.share_px - fancy.cell_px - 2. * fancy.border_px).max(0.);
                    let inline_px = (content_px - close_px - right_px).max(0.);
                    let inline_cells = (inline_px / fancy.cell_px).floor() as usize;
                    (
                        inline_cells.saturating_sub(tab_title.left_len).max(1),
                        inline_cells.max(1),
                    )
                }
                _ => (tab_title_len, tab_width_max),
            };
            let tab_title = compute_tab_title(
                &tab_info[tab_idx],
                tab_info,
                pane_info,
                config,
                hover,
                second_pass_max,
            );

            let cell_attrs = if active {
                &active_cell_attrs
            } else if hover {
                &inactive_hover_attrs
            } else {
                &inactive_cell_attrs
            };

            let tab_start_idx = x;

            has_indeterminate_progress |= tab_title.has_indeterminate;

            let esc = format_as_escapes(tab_title.items.clone()).expect("already parsed ok above");
            let mut tab_line = parse_status_text(
                &esc,
                if config.use_fancy_tab_bar {
                    CellAttributes::default()
                } else {
                    cell_attrs.clone()
                },
            );

            let title = tab_line.clone();
            if tab_line.len() > line_cap {
                tab_line.resize(line_cap, SEQ_ZERO);
            }

            let width = tab_line.len();

            items.push(TabEntry {
                item: TabBarItem::Tab { tab_idx, active },
                title,
                right_title: tab_title.right.clone(),
                agent_waiting: tab_info[tab_idx].agent_waiting,
                accent: tab_title.accent,
                x: tab_start_idx,
                width,
            });

            line.append_line(tab_line, SEQ_ZERO);
            x += width;
            if let Some(fancy) = fancy_tab {
                if config.use_fancy_tab_bar && config.tab_bar_uniform_width {
                    tab_x_px += fancy.share_px;
                }
            }
        }

        // New tab button
        if config.show_new_tab_button_in_tab_bar {
            let hover = is_tab_hover(mouse_x, x, new_tab_hover.len());

            let new_tab_button = if hover { &new_tab_hover } else { &new_tab };

            let button_start = x;
            let width = new_tab_button.len();

            line.append_line(new_tab_button.clone(), SEQ_ZERO);

            items.push(TabEntry {
                item: TabBarItem::NewTabButton,
                title: new_tab_button.clone(),
                right_title: None,
                agent_waiting: false,
                accent: None,
                x: button_start,
                width,
            });

            x += width;
        }

        // Reserve place for integrated title buttons
        let title_width = if use_integrated_title_buttons
            && !native_macos_buttons
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            let window_hide =
                parse_status_text(&config.tab_bar_style.window_hide, CellAttributes::default());
            let window_hide_hover = parse_status_text(
                &config.tab_bar_style.window_hide_hover,
                CellAttributes::default(),
            );

            let window_maximize = parse_status_text(
                &config.tab_bar_style.window_maximize,
                CellAttributes::default(),
            );
            let window_maximize_hover = parse_status_text(
                &config.tab_bar_style.window_maximize_hover,
                CellAttributes::default(),
            );
            let window_close = parse_status_text(
                &config.tab_bar_style.window_close,
                CellAttributes::default(),
            );
            let window_close_hover = parse_status_text(
                &config.tab_bar_style.window_close_hover,
                CellAttributes::default(),
            );

            let hide_len = window_hide.len().max(window_hide_hover.len());
            let maximize_len = window_maximize.len().max(window_maximize_hover.len());
            let close_len = window_close.len().max(window_close_hover.len());

            let mut width_to_reserve = 0;
            for button in &config.integrated_title_buttons {
                use IntegratedTitleButton as Button;
                let button_len = match button {
                    Button::Hide => hide_len,
                    Button::Maximize => maximize_len,
                    Button::Close => close_len,
                };
                width_to_reserve += button_len;
            }

            title_width.saturating_sub(width_to_reserve)
        } else {
            title_width
        };

        let status_space_available = title_width.saturating_sub(x);

        let mut right_status_line = parse_status_text(right_status, black_cell.attrs().clone());
        items.push(TabEntry {
            item: TabBarItem::RightStatus,
            title: right_status_line.clone(),
            right_title: None,
            agent_waiting: false,
            accent: None,
            x,
            width: status_space_available,
        });

        while right_status_line.len() > status_space_available {
            right_status_line.remove_cell(0, SEQ_ZERO);
        }

        line.append_line(right_status_line, SEQ_ZERO);
        while line.len() < title_width {
            line.insert_cell(x, black_cell.clone(), title_width, SEQ_ZERO);
        }

        if use_integrated_title_buttons
            && !native_macos_buttons
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            x = title_width;
            Self::integrated_title_buttons(mouse_x, &mut x, config, &mut items, &mut line, &colors);
        }

        Self {
            line,
            items,
            next_progress_frame_due: has_indeterminate_progress.then(next_spinner_frame_due),
        }
    }

    pub fn compute_ui_items(&self, y: usize, cell_height: usize, cell_width: usize) -> Vec<UIItem> {
        let mut items = vec![];

        for entry in self.items.iter() {
            items.push(UIItem {
                x: entry.x * cell_width,
                width: entry.width * cell_width,
                y,
                height: cell_height,
                item_type: UIItemType::TabBar(entry.item),
            });
        }

        items
    }
}

pub fn parse_status_text(text: &str, default_cell: CellAttributes) -> Line {
    let mut pen = default_cell.clone();
    let mut cells = vec![];
    let mut ignoring = false;
    let mut print_buffer = String::new();

    fn flush_print(buf: &mut String, cells: &mut Vec<Cell>, pen: &CellAttributes) {
        for g in Graphemes::new(buf.as_str()) {
            let cell = Cell::new_grapheme(g, pen.clone(), None);
            let width = cell.width();
            cells.push(cell);
            for _ in 1..width {
                // Line/Screen expect double wide graphemes to be followed by a blank in
                // the next column position, otherwise we'll render incorrectly
                cells.push(Cell::blank_with_attrs(pen.clone()));
            }
        }
        buf.clear();
    }

    let mut parser = Parser::new();
    parser.parse(text.as_bytes(), |action| {
        if ignoring {
            return;
        }
        match action {
            Action::Print(c) => print_buffer.push(c),
            Action::PrintString(s) => print_buffer.push_str(&s),
            Action::Control(c) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
                match c {
                    ControlCode::CarriageReturn | ControlCode::LineFeed => {
                        ignoring = true;
                    }
                    _ => {}
                }
            }
            Action::CSI(csi) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
                match csi {
                    CSI::Sgr(sgr) => match sgr {
                        Sgr::Reset => pen = default_cell.clone(),
                        Sgr::Intensity(i) => {
                            pen.set_intensity(i);
                        }
                        Sgr::Underline(u) => {
                            pen.set_underline(u);
                        }
                        Sgr::Overline(o) => {
                            pen.set_overline(o);
                        }
                        Sgr::VerticalAlign(o) => {
                            pen.set_vertical_align(o);
                        }
                        Sgr::Blink(b) => {
                            pen.set_blink(b);
                        }
                        Sgr::Italic(i) => {
                            pen.set_italic(i);
                        }
                        Sgr::Inverse(inverse) => {
                            pen.set_reverse(inverse);
                        }
                        Sgr::Invisible(invis) => {
                            pen.set_invisible(invis);
                        }
                        Sgr::StrikeThrough(strike) => {
                            pen.set_strikethrough(strike);
                        }
                        Sgr::Foreground(col) => {
                            if let ColorSpec::Default = col {
                                pen.set_foreground(default_cell.foreground());
                            } else {
                                pen.set_foreground(col);
                            }
                        }
                        Sgr::Background(col) => {
                            if let ColorSpec::Default = col {
                                pen.set_background(default_cell.background());
                            } else {
                                pen.set_background(col);
                            }
                        }
                        Sgr::UnderlineColor(col) => {
                            pen.set_underline_color(col);
                        }
                        Sgr::Font(_) => {}
                    },
                    _ => {}
                }
            }
            Action::OperatingSystemCommand(_)
            | Action::DeviceControl(_)
            | Action::Esc(_)
            | Action::KittyImage(_)
            | Action::XtGetTcap(_)
            | Action::Sixel(_) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
            }
        }
    });
    flush_print(&mut print_buffer, &mut cells, &pen);
    Line::from_cells(cells, SEQ_ZERO)
}
