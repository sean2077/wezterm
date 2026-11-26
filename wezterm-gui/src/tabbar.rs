use crate::termwindow::render::window_buttons::traffic_light_width;
use crate::termwindow::{PaneInformation, TabInformation, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{ConfigHandle, DimensionContext, TabBarColors};
use finl_unicode::grapheme_clusters::Graphemes;
use mlua::{FromLua, IntoLua};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
    /// Fixed left block (index + badge) marked by U+001E.
    left: Option<Line>,
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

struct TabTitleCallback<'lua> {
    lua: &'lua mlua::Lua,
    tabs: Vec<mlua::AnyUserData<'lua>>,
    panes: Vec<mlua::AnyUserData<'lua>>,
    config: mlua::Value<'lua>,
}

impl<'lua> TabTitleCallback<'lua> {
    fn new(
        lua: &'lua mlua::Lua,
        tabs: &[TabInformation],
        panes: &[PaneInformation],
        config: &ConfigHandle,
    ) -> mlua::Result<Option<Self>> {
        if !config::lua::has_event_handler(lua, "format-tab-title")? {
            return Ok(None);
        }
        Ok(Some(Self {
            lua,
            tabs: tabs
                .iter()
                .map(|tab| lua.create_userdata(tab.clone()))
                .collect::<mlua::Result<_>>()?,
            panes: panes
                .iter()
                .map(|pane| lua.create_userdata(pane.clone()))
                .collect::<mlua::Result<_>>()?,
            config: (**config).clone().into_lua(lua)?,
        }))
    }

    fn call(
        &self,
        tab: &TabInformation,
        hover: bool,
        tab_max_width: usize,
    ) -> anyhow::Result<Option<TitleText>> {
        let lua = self.lua;
        // The arrays and config are mutable Lua tables. Give each invocation
        // fresh tables while sharing the read-only tab and pane userdata.
        let tabs = lua.create_sequence_from(self.tabs.iter().cloned())?;
        let panes = lua.create_sequence_from(self.panes.iter().cloned())?;
        let config = clone_lua_snapshot(lua, &self.config)?;
        let v = config::lua::emit_sync_callback(
            lua,
            (
                "format-tab-title".to_owned(),
                (tab.clone(), tabs, panes, config, hover, tab_max_width),
            ),
        )?;
        match &v {
            mlua::Value::Nil => Ok(None),
            mlua::Value::Table(_) => {
                let items = <Vec<FormatItem>>::from_lua(v, lua)?;
                let (items, accent) = take_accent(items);
                // Parse both sides in one pass so the floating suffix keeps
                // the foreground, background and attributes at U+001F.
                let esc = format_as_escapes(items.clone())?;
                let (line, right) = parse_status_text_impl(
                    &esc,
                    CellAttributes::default(),
                    Some(ControlCode::UnitSeparator),
                );
                let (left_items, _, _) = split_marker(items, RIGHT_MARKER);
                // U+001E separates the fixed left block (index + badge)
                // from the title. The tab bar measures the left block and
                // passes the rest of the share to the title as its budget.
                let (left_items, title_items, has_title_marker) =
                    split_marker(left_items, TITLE_MARKER);
                let left = if has_title_marker {
                    let esc = format_as_escapes(left_items.clone())?;
                    Some(parse_status_text(&esc, CellAttributes::default()))
                } else {
                    None
                };
                let mut all_items = left_items;
                all_items.extend(title_items);

                Ok(Some(TitleText {
                    items: all_items,
                    len: line.len(),
                    has_indeterminate: false,
                    right,
                    left,
                    accent,
                }))
            }
            _ => {
                let s = String::from_lua(v, lua)?;
                let (line, right) = parse_status_text_impl(
                    &s,
                    CellAttributes::default(),
                    Some(ControlCode::UnitSeparator),
                );
                let left = match s.find(RIGHT_MARKER) {
                    Some(pos) => s[..pos].to_string(),
                    None => s,
                };
                Ok(Some(TitleText {
                    len: line.len(),
                    items: vec![FormatItem::Text(left)],
                    has_indeterminate: false,
                    right,
                    left: None,
                    accent: None,
                }))
            }
        }
    }
}

fn clone_lua_snapshot<'lua>(
    lua: &'lua mlua::Lua,
    value: &mlua::Value<'lua>,
) -> mlua::Result<mlua::Value<'lua>> {
    if let mlua::Value::Table(table) = value {
        let copy = lua.create_table_with_capacity(table.raw_len(), 0)?;
        for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
            let (key, value) = pair?;
            copy.raw_set(
                clone_lua_snapshot(lua, &key)?,
                clone_lua_snapshot(lua, &value)?,
            )?;
        }
        Ok(mlua::Value::Table(copy))
    } else {
        Ok(value.clone())
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
    config: &ConfigHandle,
    callback: Option<&TabTitleCallback>,
    hover: bool,
    tab_max_width: usize,
) -> TitleText {
    let title = callback.and_then(|callback| match callback.call(tab, hover, tab_max_width) {
        Ok(title) => title,
        Err(err) => {
            log::warn!("format-tab-title: {err:#}");
            None
        }
    });

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
                left: None,
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

/// Pixel allocations shared by the Lua title budget and the fancy bar layout.
pub(crate) struct FancyTabMetrics {
    tab_pixels: usize,
    tab_count: usize,
    pub cell_px: f32,
    pub border_px: f32,
    close_px: f32,
    pub left_px: f32,
    pub left_status_px: f32,
    pub right_status_px: f32,
    pub left_padding: f32,
    pub native_button_margin: f32,
    pub show_new_tab: bool,
}

impl FancyTabMetrics {
    pub fn tab_width(&self, index: usize) -> f32 {
        if self.tab_count == 0 {
            return 0.;
        }
        let base = self.tab_pixels / self.tab_count;
        let extra = usize::from(index < self.tab_pixels % self.tab_count);
        (base + extra) as f32
    }

    pub fn tab_start(&self, index: usize) -> f32 {
        if self.tab_count == 0 {
            return self.left_px;
        }
        let base = self.tab_pixels / self.tab_count;
        let extra_before = index.min(self.tab_pixels % self.tab_count);
        self.left_px + (base * index + extra_before) as f32
    }

    pub fn chrome(&self, tab_width: f32) -> (f32, f32) {
        let border = self.border_px.min((tab_width * 0.5).floor());
        let padding = (self.cell_px * 0.5)
            .floor()
            .min(((tab_width - 2. * border) * 0.5).floor());
        (border, padding)
    }

    pub fn content_width(&self, tab_width: f32) -> f32 {
        let (border, padding) = self.chrome(tab_width);
        tab_width - 2. * (border + padding)
    }

    pub fn close_width(&self, content_width: f32) -> f32 {
        if content_width >= self.close_px {
            self.close_px
        } else {
            0.
        }
    }

    fn title_columns(&self, index: usize, suffix_px: f32) -> usize {
        let content = self.content_width(self.tab_width(index));
        ((content - self.close_width(content) - suffix_px).max(0.) / self.cell_px).floor() as usize
    }
}

pub(crate) fn fancy_tab_metrics(
    config: &ConfigHandle,
    metrics: &RenderMetrics,
    dpi: f32,
    window_px: f32,
    status_widths: (f32, f32),
    tab_count: usize,
    is_full_screen: bool,
) -> FancyTabMetrics {
    let cell_px = (metrics.cell_size.width as f32).max(1.);
    let cell_h = metrics.cell_size.height as f32;
    let half_cell = (0.5 * cell_px).floor();
    let quarter_cell = (0.25 * cell_px).floor();
    let integrated = config
        .window_decorations
        .contains(window::WindowDecorations::INTEGRATED_BUTTONS);
    let buttons_px = if integrated {
        window_buttons_width(config, metrics, dpi)
    } else {
        0.
    };
    let native_mac = integrated
        && config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
        && cfg!(target_os = "macos");
    let buttons_at_left = integrated
        && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left;
    let (left_padding, native_button_margin) = if native_mac && !is_full_screen {
        (70., (4. * cell_px).floor())
    } else if buttons_at_left && !native_mac {
        (0., 0.)
    } else {
        (half_cell, 0.)
    };
    let new_tab_width = (0.5 * cell_h).floor() + 3. * half_cell + 2.;
    let side_fixed = buttons_px + left_padding + native_button_margin;
    let show_new_tab =
        config.show_new_tab_button_in_tab_bar && window_px >= side_fixed + new_tab_width;
    let free_pixels =
        (window_px - side_fixed - if show_new_tab { new_tab_width } else { 0. }).max(0.);
    let ratio = (config.tab_bar_side_reserve_ratio as f32).clamp(0.0, 1.0);
    let mut left_status_px = (status_widths.0 * ratio).floor();
    let mut right_status_px = (status_widths.1 * ratio).floor();
    let status_total = left_status_px + right_status_px;
    if status_total > free_pixels {
        left_status_px = (left_status_px * free_pixels / status_total).floor();
        right_status_px = (right_status_px * free_pixels / status_total).floor();
    }
    // The padding alone is wider than these allocations. Omit these labels.
    if left_status_px <= half_cell {
        left_status_px = 0.;
    }
    if right_status_px <= half_cell {
        right_status_px = 0.;
    }
    let tab_pixels = (free_pixels - left_status_px - right_status_px).max(0.) as usize;

    FancyTabMetrics {
        tab_pixels,
        tab_count,
        cell_px,
        border_px: config
            .tab_bar_tab_border_width
            .evaluate_as_pixels(DimensionContext {
                dpi,
                pixel_max: window_px,
                pixel_cell: cell_px,
            })
            .max(0.),
        close_px: if config.show_close_tab_button_in_tabs {
            (0.5 * cell_h).floor() + half_cell + 2. * quarter_cell
        } else {
            0.
        },
        left_px: left_padding
            + native_button_margin
            + left_status_px
            + if buttons_at_left { buttons_px } else { 0. },
        left_status_px,
        right_status_px,
        left_padding,
        native_button_margin,
        show_new_tab,
    }
}

/// Returns whether the pane is waiting and the time at which its ring expires.
pub(crate) fn agent_waiting_state(
    value: &str,
    now: SystemTime,
    ttl_seconds: u64,
) -> (bool, Option<SystemTime>) {
    let Some((state, epoch)) = value.split_once(':') else {
        return (value == "waiting", None);
    };
    if state != "waiting" {
        return (false, None);
    }
    if ttl_seconds == 0 {
        return (true, None);
    }

    // Preserve the untimed form used by callers without a numeric timestamp.
    if epoch.is_empty() || !epoch.bytes().all(|b| b.is_ascii_digit()) {
        return (true, None);
    }
    let Some(timestamp) = epoch
        .parse::<u64>()
        .ok()
        .and_then(|epoch| UNIX_EPOCH.checked_add(Duration::from_secs(epoch)))
    else {
        return (false, None);
    };
    // Allow modest clock skew between the pane and GUI without scheduling a
    // timer arbitrarily far into the future for a malformed terminal value.
    if timestamp
        .duration_since(now)
        .is_ok_and(|d| d > Duration::from_secs(300))
    {
        return (false, None);
    }
    match timestamp.checked_add(Duration::from_secs(ttl_seconds)) {
        Some(expiry) if expiry > now => (true, Some(expiry)),
        _ => (false, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_waiting_state, take_accent, Duration, FormatColor, FormatItem, SrgbaTuple, UNIX_EPOCH,
    };
    use mlua::FromLua;

    #[test]
    fn detects_waiting_agent_state() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        let active = now + Duration::from_secs(10);
        assert_eq!(agent_waiting_state("waiting", now, 0), (true, None));
        assert_eq!(
            agent_waiting_state("waiting:100", now, 10),
            (true, Some(active))
        );
        assert_eq!(
            agent_waiting_state("waiting:100", active, 10),
            (false, None)
        );
        assert_eq!(
            agent_waiting_state("waiting:105", now, 10),
            (true, Some(active + Duration::from_secs(5)))
        );
        assert_eq!(agent_waiting_state("waiting:soon", now, 10), (true, None));
        assert_eq!(agent_waiting_state("waiting:", now, 10), (true, None));
        for value in ["running:100", "done:100", "error:100", "waiting-room", ""] {
            assert_eq!(agent_waiting_state(value, now, 10), (false, None));
        }
    }

    #[test]
    fn rejects_unrepresentable_or_far_future_waiting_expiry() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(agent_waiting_state("waiting:500", now, 10), (false, None));
        assert_eq!(
            agent_waiting_state(&format!("waiting:{}", u64::MAX), now, 10),
            (false, None)
        );
        assert_eq!(
            agent_waiting_state("waiting:100", now, u64::MAX),
            (false, None)
        );
    }

    #[test]
    fn narrow_tab_allocations_have_no_invisible_close_button() {
        let layout = super::FancyTabMetrics {
            tab_pixels: 38,
            tab_count: 3,
            cell_px: 9.,
            border_px: 1.,
            close_px: 16.,
            left_px: 17.,
            left_status_px: 5.,
            right_status_px: 5.,
            left_padding: 4.,
            native_button_margin: 0.,
            show_new_tab: true,
        };
        assert_eq!(layout.tab_width(0), 13.);
        assert_eq!(layout.tab_start(1), 30.);
        assert_eq!(layout.tab_start(2), 43.);
        assert_eq!(layout.tab_start(3), 55.);
        assert_eq!(layout.content_width(13.), 3.);
        assert_eq!(layout.close_width(3.), 0.);
        assert_eq!(layout.close_width(16.), 16.);
        assert_eq!(layout.title_columns(2, 11.), 0);
    }

    #[test]
    fn callback_config_snapshot_isolates_nested_mutations() -> mlua::Result<()> {
        let lua = mlua::Lua::new();
        let snapshot: mlua::Value = lua.load("return { nested = { value = 1 } }").eval()?;
        let first = super::clone_lua_snapshot(&lua, &snapshot)?;
        let second = super::clone_lua_snapshot(&lua, &snapshot)?;
        let nested = |value| mlua::Table::from_lua(value, &lua)?.get::<_, mlua::Table>("nested");
        nested(first)?.set("value", 2)?;
        assert_eq!(nested(second)?.get::<_, i64>("value")?, 1);
        Ok(())
    }

    fn tab() -> super::TabInformation {
        super::TabInformation {
            tab_id: 0,
            tab_index: 0,
            is_active: true,
            is_last_active: true,
            active_pane: None,
            window_id: 0,
            tab_title: String::new(),
            agent_waiting: false,
        }
    }

    #[test]
    fn unused_title_callback_skips_snapshots_but_sees_later_registration() -> anyhow::Result<()> {
        let lua = mlua::Lua::new();
        let config = config::ConfigHandle::default_config();
        assert!(super::TabTitleCallback::new(&lua, &[], &[], &config)?.is_none());

        // Registration may occur after startup; never cache a negative lookup
        // across tab bar rebuilds or across Lua contexts after config reload.
        let callback = lua.create_function(|_, _: mlua::MultiValue| Ok("custom title"))?;
        config::lua::register_event(&lua, ("format-tab-title".into(), callback))?;
        let callbacks = super::TabTitleCallback::new(&lua, &[], &[], &config)?.unwrap();
        assert_eq!(callbacks.call(&tab(), false, 30)?.unwrap().len, 12);

        let reloaded = mlua::Lua::new();
        assert!(super::TabTitleCallback::new(&reloaded, &[], &[], &config)?.is_none());
        Ok(())
    }

    #[test]
    fn floating_title_suffix_inherits_styles_and_honors_resets() -> anyhow::Result<()> {
        use super::{format_as_escapes, parse_status_text, CellAttributes};
        use termwiz::cell::Intensity;
        use termwiz::color::ColorAttribute;

        // Exercise both supported callback result forms. The separator must
        // change placement without resetting SGR attributes; an explicit reset
        // inside the suffix must still restore the default attributes.
        for source in [
            r##"return function()
                return {
                    { Foreground = { Color = '#12ab34' } },
                    { Background = { Color = '#112233' } },
                    { Attribute = { Intensity = 'Bold' } },
                    { Text = 'A\31B' },
                    'ResetAttributes',
                    { Text = 'C' },
                }
            end"##,
            r#"return function()
                return '\27[38;2;18;171;52m\27[48;2;17;34;51m\27[1mA\31B\27[0mC'
            end"#,
        ] {
            let lua = mlua::Lua::new();
            let callback: mlua::Function = lua.load(source).eval()?;
            config::lua::register_event(&lua, ("format-tab-title".into(), callback))?;
            let config = config::ConfigHandle::default_config();
            let callbacks = super::TabTitleCallback::new(&lua, &[], &[], &config)?.unwrap();
            let title = callbacks.call(&tab(), false, 30)?.unwrap();
            assert_eq!(title.len, 1);
            let left =
                parse_status_text(&format_as_escapes(title.items)?, CellAttributes::default());
            let right = title.right.unwrap();
            assert_eq!(right.len(), 2);
            let before = left.get_cell(0).unwrap();
            let inherited = right.get_cell(0).unwrap();
            assert_ne!(before.attrs().foreground(), ColorAttribute::Default);
            assert_ne!(before.attrs().background(), ColorAttribute::Default);
            assert_eq!(before.attrs().intensity(), Intensity::Bold);
            assert_eq!(inherited.attrs(), before.attrs());
            assert_eq!(
                right.get_cell(1).unwrap().attrs(),
                &CellAttributes::default()
            );
        }
        Ok(())
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
        fancy_tab: Option<&FancyTabMetrics>,
        mouse_px: Option<f32>,
        measure_suffix: impl Fn(&Line) -> f32,
    ) -> Self {
        let build = |callback: Option<&TabTitleCallback<'_>>| {
            Self::build(
                title_width,
                mouse_x,
                tab_info,
                colors,
                config,
                left_status,
                right_status,
                fancy_tab,
                mouse_px,
                &measure_suffix,
                callback,
            )
        };
        if !config.show_tabs_in_tab_bar || tab_info.is_empty() {
            return build(None);
        }
        match config::run_immediate_with_lua_config(|lua| {
            let callback = match lua.as_deref() {
                Some(lua) => TabTitleCallback::new(lua, tab_info, pane_info, config)?,
                None => None,
            };
            Ok(build(callback.as_ref()))
        }) {
            Ok(state) => state,
            Err(err) => {
                log::warn!("format-tab-title: {err:#}");
                build(None)
            }
        }
    }

    fn build(
        title_width: usize,
        mouse_x: Option<usize>,
        tab_info: &[TabInformation],
        colors: Option<&TabBarColors>,
        config: &ConfigHandle,
        left_status: &str,
        right_status: &str,
        fancy_tab: Option<&FancyTabMetrics>,
        mouse_px: Option<f32>,
        measure_suffix: &dyn Fn(&Line) -> f32,
        callback: Option<&TabTitleCallback>,
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
        let first_pass_max = |index| {
            fancy_tab.map_or(config.tab_max_width, |fancy| {
                fancy.title_columns(index, 0.).max(1)
            })
        };

        let tab_titles: Vec<TitleText> = if config.show_tabs_in_tab_bar {
            tab_info
                .iter()
                .map(|tab| {
                    if tab.is_active {
                        active_tab_no = tab.tab_index;
                    }
                    compute_tab_title(tab, config, callback, false, first_pass_max(tab.tab_index))
                })
                .collect()
        } else {
            vec![]
        };
        let titles_len: usize = tab_titles.iter().map(|s| s.len).sum();

        let available_cells =
            title_width.saturating_sub(number_of_tabs.saturating_sub(1) + new_tab.len());
        let retro_tab_width_max = (if available_cells >= titles_len {
            usize::MAX
        } else {
            available_cells / number_of_tabs.max(1)
        })
        .min(config.tab_max_width);

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

        for (tab_idx, tab_title) in tab_titles.iter().enumerate() {
            let tab_width_max = if config.use_fancy_tab_bar {
                first_pass_max(tab_idx)
            } else {
                retro_tab_width_max
            };
            let tab_title_len = tab_title.len.min(tab_width_max);
            let active = tab_idx == active_tab_no;
            let hover = !active
                && match fancy_tab {
                    Some(fancy) if config.tab_bar_uniform_width => {
                        let start = fancy.tab_start(tab_idx);
                        mouse_px.is_some_and(|mouse| {
                            mouse >= start && mouse < start + fancy.tab_width(tab_idx)
                        })
                    }
                    _ => is_tab_hover(mouse_x, x, tab_title_len),
                };

            // Keep the fixed title prefix, suffix and close button inside the
            // same pixel allocation used by layout.
            let (second_pass_max, line_cap) = match fancy_tab {
                Some(fancy) => {
                    let suffix_px = tab_title
                        .right
                        .as_ref()
                        .map(&measure_suffix)
                        .unwrap_or_default();
                    let prefix_px = tab_title
                        .left
                        .as_ref()
                        .map(&measure_suffix)
                        .unwrap_or_default();
                    (
                        fancy.title_columns(tab_idx, suffix_px + prefix_px).max(1),
                        fancy.title_columns(tab_idx, suffix_px),
                    )
                }
                None => (tab_title_len, tab_width_max),
            };
            let tab_title = if callback.is_some() {
                compute_tab_title(&tab_info[tab_idx], config, callback, hover, second_pass_max)
            } else {
                tab_title.clone()
            };

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

            if tab_line.len() > line_cap {
                tab_line.resize(line_cap, SEQ_ZERO);
            }
            let title = tab_line.clone();

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
    parse_status_text_impl(text, default_cell, None).0
}

fn parse_status_text_impl(
    text: &str,
    default_cell: CellAttributes,
    split_at: Option<ControlCode>,
) -> (Line, Option<Line>) {
    let mut pen = default_cell.clone();
    let mut cells = vec![];
    let mut left_cells = None;
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
                if split_at == Some(c) && left_cells.is_none() {
                    left_cells = Some(std::mem::take(&mut cells));
                }
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
    let line = Line::from_cells(cells, SEQ_ZERO);
    match left_cells {
        Some(left) => (Line::from_cells(left, SEQ_ZERO), Some(line)),
        None => (line, None),
    }
}
