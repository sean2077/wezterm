//! Windows-only quake dropdown support.
//!
//! The hotkey is registered against the window handle so that `WM_HOTKEY`
//! arrives in this window's `do_wnd_proc`. Window calls that may synchronously
//! dispatch messages back into the window proc (`SetWindowPos`, `ShowWindow`,
//! `SetWindowLongW`) are only made after the `WindowInner` borrow has been
//! released; otherwise `rc_from_hwnd(...).borrow_mut()` in those handlers
//! would panic on a double borrow.

use super::{rc_from_hwnd, HWindow, WindowInner};
use config::{DeferredKeyCode, KeyNoAction, QuakeAlignment, QuakeConfig, QuakeEdge, QuakeMonitor};
use std::sync::atomic::{AtomicBool, Ordering};
use wezterm_input_types::{KeyCode, Modifiers};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

const QUAKE_HOTKEY_ID: i32 = 1;

static QUAKE_HOTKEY_REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub(super) struct QuakeState {
    hotkey_spec: Option<KeyNoAction>,
    registered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

pub(super) fn hotkey_available() -> bool {
    QUAKE_HOTKEY_REGISTERED.load(Ordering::Relaxed)
}

/// Register, unregister or re-register the configured global hotkey and keep
/// the tool-window style in sync. Safe to call with `WindowInner` borrowed:
/// it never calls a function that can synchronously re-enter the window proc.
pub(super) fn sync_hotkey(inner: &mut WindowInner) {
    let desired = if inner.config.quake.enabled {
        inner.config.quake.hotkey.clone()
    } else {
        None
    };
    let was_registered = inner.quake.hotkey_spec.is_some();

    if inner.quake.hotkey_spec == desired && (desired.is_none() || inner.quake.registered) {
        schedule_window_styles(
            inner.hwnd,
            inner.config.quake.enabled && inner.quake.registered,
        );
        return;
    }

    unregister_hotkey(inner);
    inner.quake.hotkey_spec = desired.clone();

    let Some(spec) = desired else {
        if was_registered && !is_visible(inner.hwnd.0) {
            show_async(inner.hwnd.0);
        }
        schedule_window_styles(inner.hwnd, false);
        return;
    };

    match register_hotkey(inner.hwnd.0, &spec) {
        Ok(()) => {
            inner.quake.registered = true;
            QUAKE_HOTKEY_REGISTERED.store(true, Ordering::Relaxed);
            log::info!("quake: registered global hotkey");
        }
        Err(err) => {
            inner.quake.registered = false;
            QUAKE_HOTKEY_REGISTERED.store(false, Ordering::Relaxed);
            log::error!("quake: {err}");
            if !is_visible(inner.hwnd.0) {
                show_async(inner.hwnd.0);
            }
        }
    }

    schedule_window_styles(
        inner.hwnd,
        inner.config.quake.enabled && inner.quake.registered,
    );
}

pub(super) fn unregister_hotkey(inner: &mut WindowInner) {
    if inner.quake.registered {
        if !inner.hwnd.0.is_null() {
            unsafe {
                UnregisterHotKey(inner.hwnd.0, QUAKE_HOTKEY_ID);
            }
        }
        inner.quake.registered = false;
        QUAKE_HOTKEY_REGISTERED.store(false, Ordering::Relaxed);
    }
}

/// `WM_HOTKEY` handler. Reads the config, releases the borrow, then toggles.
pub(super) fn wm_hotkey(hwnd: HWND, wparam: WPARAM) -> Option<LRESULT> {
    if wparam as i32 != QUAKE_HOTKEY_ID {
        return None;
    }
    let Some(inner) = rc_from_hwnd(hwnd) else {
        return Some(0);
    };
    let config = {
        let inner = inner.borrow();
        inner.config.quake.clone()
    };
    if config.enabled {
        toggle(hwnd, &config);
    }
    Some(0)
}

/// Hide the window when the new foreground window is outside this window's
/// root-owner family. Called after `FocusChanged(false)` has been dispatched.
pub(super) fn focus_lost(hwnd: HWND, config: &QuakeConfig) {
    if !config.enabled || !config.hide_on_focus_loss || hwnd.is_null() {
        return;
    }
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return;
    }
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return;
    }
    let own_root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    let foreground_root = unsafe { GetAncestor(foreground, GA_ROOTOWNER) };
    if own_root == foreground_root {
        return;
    }
    unsafe {
        ShowWindowAsync(hwnd, SW_HIDE);
    }
}

/// `WM_CLOSE` policy: in quake mode a close request hides the window instead
/// of destroying it. Returns true when the caller must not dispatch the
/// regular `CloseRequested` event.
pub(super) fn close_hides(hwnd: HWND) -> bool {
    let Some(inner) = rc_from_hwnd(hwnd) else {
        return false;
    };
    let enabled = {
        let inner = inner.borrow();
        inner.config.quake.enabled
    };
    if enabled {
        unsafe {
            ShowWindowAsync(hwnd, SW_HIDE);
        }
    }
    enabled
}

fn toggle(hwnd: HWND, config: &QuakeConfig) {
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    let iconic = unsafe { IsIconic(hwnd) } != 0;
    if visible && !iconic {
        unsafe {
            ShowWindowAsync(hwnd, SW_HIDE);
        }
        return;
    }
    if let Err(err) = show_quake(hwnd, config) {
        log::error!("quake: {err}");
    }
}

fn show_quake(hwnd: HWND, config: &QuakeConfig) -> Result<(), String> {
    let bounds = quake_bounds(config)?;

    // An iconified window (for example after Win+M) has to be restored before
    // it can be placed; SetWindowPos alone does not update its restored rect.
    if unsafe { IsIconic(hwnd) } != 0 {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }

    let insert_after = if config.always_on_top {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };

    unsafe {
        // Move first and acknowledge the monitor transition (WM_DPICHANGED is
        // handled by DefWindowProc), then apply the final size.
        if SetWindowPos(
            hwnd,
            insert_after,
            bounds.x,
            bounds.y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        ) == 0
        {
            return Err(format!(
                "SetWindowPos(move) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if SetWindowPos(
            hwnd,
            insert_after,
            0,
            0,
            bounds.width,
            bounds.height,
            SWP_NOMOVE | SWP_NOACTIVATE,
        ) == 0
        {
            return Err(format!(
                "SetWindowPos(size) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // The borrow is no longer held here, so a synchronous ShowWindow is
        // safe: WM_SETFOCUS/WM_SIZE handlers may borrow WindowInner again.
        ShowWindow(hwnd, SW_SHOW);
        if SetForegroundWindow(hwnd) == 0 {
            log::warn!("quake: SetForegroundWindow was refused");
        }
    }
    Ok(())
}

fn register_hotkey(hwnd: HWND, spec: &KeyNoAction) -> Result<(), String> {
    let vk = keycode_to_vk(&spec.key)?;
    let modifiers = modifiers_to_mod(spec.mods);
    if unsafe { RegisterHotKey(hwnd, QUAKE_HOTKEY_ID, modifiers, vk) } == 0 {
        return Err(format!(
            "RegisterHotKey {:?} failed: {}",
            spec,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn keycode_to_vk(key: &DeferredKeyCode) -> Result<u32, String> {
    let key = match key {
        DeferredKeyCode::KeyCode(key) => key,
        DeferredKeyCode::Either { mapped, .. } => mapped,
    };
    match key {
        KeyCode::Function(n) if (1..=24).contains(n) => Ok(VK_F1 as u32 + u32::from(*n) - 1),
        KeyCode::Char(c) if c.is_ascii_alphanumeric() => Ok(c.to_ascii_uppercase() as u32),
        KeyCode::Char(' ') => Ok(VK_SPACE as u32),
        KeyCode::Char('\t') => Ok(VK_TAB as u32),
        KeyCode::Char('\r') | KeyCode::Char('\n') => Ok(VK_RETURN as u32),
        KeyCode::Char('\u{1b}') => Ok(VK_ESCAPE as u32),
        other => Err(format!(
            "unsupported quake hotkey key {other:?}; use F1-F24, A-Z or 0-9"
        )),
    }
}

fn modifiers_to_mod(mods: Modifiers) -> UINT {
    let mut out: UINT = MOD_NOREPEAT as UINT;
    if mods.intersects(Modifiers::CTRL | Modifiers::LEFT_CTRL | Modifiers::RIGHT_CTRL) {
        out |= MOD_CONTROL as UINT;
    }
    if mods.intersects(Modifiers::ALT | Modifiers::LEFT_ALT | Modifiers::RIGHT_ALT) {
        out |= MOD_ALT as UINT;
    }
    if mods.intersects(Modifiers::SHIFT | Modifiers::LEFT_SHIFT | Modifiers::RIGHT_SHIFT) {
        out |= MOD_SHIFT as UINT;
    }
    if mods.intersects(Modifiers::SUPER) {
        out |= MOD_WIN as UINT;
    }
    out
}

fn quake_bounds(config: &QuakeConfig) -> Result<Bounds, String> {
    let monitor = match config.monitor {
        QuakeMonitor::Cursor => {
            let mut point: POINT = unsafe { std::mem::zeroed() };
            if unsafe { GetCursorPos(&mut point) } == 0 {
                return Err(format!(
                    "GetCursorPos failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) }
        }
        QuakeMonitor::Foreground => unsafe {
            MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY)
        },
        QuakeMonitor::Primary => unsafe {
            MonitorFromWindow(std::ptr::null_mut(), MONITOR_DEFAULTTOPRIMARY)
        },
    };

    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if monitor.is_null() || unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return Err(format!(
            "GetMonitorInfoW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    bounds_for_work_area(info.rcWork, config)
}

fn bounds_for_work_area(work: RECT, config: &QuakeConfig) -> Result<Bounds, String> {
    if !(10..=100).contains(&config.width_percent) || !(10..=100).contains(&config.height_percent) {
        return Err("quake width_percent/height_percent must be between 10 and 100".into());
    }
    let work_width = i64::from(work.right) - i64::from(work.left);
    let work_height = i64::from(work.bottom) - i64::from(work.top);
    if work_width <= 0 || work_height <= 0 {
        return Err("invalid monitor work area".into());
    }
    let width = (work_width * i64::from(config.width_percent) / 100).max(1) as i32;
    let height = (work_height * i64::from(config.height_percent) / 100).max(1) as i32;
    let x = match config.alignment {
        QuakeAlignment::Start => work.left,
        QuakeAlignment::Center => work.left + ((work_width as i32) - width) / 2,
        QuakeAlignment::End => work.right - width,
    };
    let y = match config.edge {
        QuakeEdge::Top => work.top,
        QuakeEdge::Bottom => work.bottom - height,
    };
    Ok(Bounds {
        x,
        y,
        width,
        height,
    })
}

fn is_visible(hwnd: HWND) -> bool {
    !hwnd.is_null() && unsafe { IsWindowVisible(hwnd) } != 0
}

fn show_async(hwnd: HWND) {
    if !hwnd.is_null() {
        unsafe {
            ShowWindowAsync(hwnd, SW_SHOW);
        }
    }
}

/// Apply or remove WS_EX_TOOLWINDOW off the window-proc call stack. The style
/// change can dispatch WM_STYLECHANGED synchronously, so it runs in a spawned
/// task rather than while a WindowInner borrow is held.
fn schedule_window_styles(hwnd: HWindow, use_tool_window: bool) {
    promise::spawn::spawn(async move {
        unsafe {
            let style = GetWindowLongW(hwnd.0, GWL_EXSTYLE);
            let tool = WS_EX_TOOLWINDOW as i32;
            let desired = if use_tool_window {
                style | tool
            } else {
                style & !tool
            };
            if desired != style {
                SetWindowLongW(hwnd.0, GWL_EXSTYLE, desired);
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_area() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        }
    }

    #[test]
    fn quake_bounds_are_anchored_to_the_work_area() {
        let top = QuakeConfig {
            width_percent: 100,
            height_percent: 45,
            edge: QuakeEdge::Top,
            alignment: QuakeAlignment::Center,
            ..Default::default()
        };
        assert_eq!(
            bounds_for_work_area(work_area(), &top).unwrap(),
            Bounds {
                x: 0,
                y: 0,
                width: 1920,
                height: 468
            }
        );

        let bottom = QuakeConfig {
            edge: QuakeEdge::Bottom,
            alignment: QuakeAlignment::End,
            width_percent: 50,
            height_percent: 25,
            ..Default::default()
        };
        assert_eq!(
            bounds_for_work_area(work_area(), &bottom).unwrap(),
            Bounds {
                x: 960,
                y: 780,
                width: 960,
                height: 260
            }
        );
    }

    #[test]
    fn quake_bounds_reject_invalid_percentages() {
        let mut config = QuakeConfig::default();
        config.width_percent = 9;
        assert!(bounds_for_work_area(work_area(), &config).is_err());
        config.width_percent = 100;
        config.height_percent = 101;
        assert!(bounds_for_work_area(work_area(), &config).is_err());
    }

    #[test]
    fn quake_hotkey_maps_function_keys_and_modifiers() {
        let spec = DeferredKeyCode::KeyCode(KeyCode::Function(3));
        assert_eq!(keycode_to_vk(&spec).unwrap(), VK_F3 as u32);
        let spec = DeferredKeyCode::KeyCode(KeyCode::Char('a'));
        assert_eq!(keycode_to_vk(&spec).unwrap(), 'A' as u32);
        assert_eq!(modifiers_to_mod(Modifiers::NONE), MOD_NOREPEAT as UINT);
        assert_eq!(
            modifiers_to_mod(Modifiers::CTRL | Modifiers::ALT),
            (MOD_NOREPEAT | MOD_CONTROL | MOD_ALT) as UINT
        );
    }
}
