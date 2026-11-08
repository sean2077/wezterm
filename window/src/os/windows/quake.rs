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
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use wezterm_input_types::{KeyCode, Modifiers};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

const QUAKE_HOTKEY_ID: i32 = 1;

/// Windows (by HWND) that currently own the global quake hotkey.  The
/// registration itself is per-window, so the "available" answer must be too:
/// a second window failing to register must not make the first window think
/// its hotkey is gone.
static QUAKE_HOTKEY_WINDOWS: LazyLock<Mutex<HashSet<isize>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn hotkey_key(hwnd: HWindow) -> isize {
    hwnd.0 as isize
}

fn set_hotkey_registered(hwnd: HWindow, registered: bool) {
    let key = hotkey_key(hwnd);
    let mut windows = QUAKE_HOTKEY_WINDOWS.lock().unwrap();
    if registered {
        windows.insert(key);
    } else {
        windows.remove(&key);
    }
}

/// Whether this window owns the quake hotkey.
pub(super) fn hotkey_available(hwnd: HWindow) -> bool {
    QUAKE_HOTKEY_WINDOWS
        .lock()
        .unwrap()
        .contains(&hotkey_key(hwnd))
}

#[derive(Default)]
pub(super) struct QuakeState {
    hotkey_spec: Option<KeyNoAction>,
    registered: bool,
    /// Runtime fullscreen state. `ToggleFullScreen` flips this while quake
    /// mode is active, and F3 re-shows the window with the same state.
    fullscreen: bool,
    /// The config value last copied into `fullscreen`, so a config reload can
    /// reset the runtime state when `config.quake.fullscreen` changes.
    config_fullscreen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Register, unregister or re-register the configured global hotkey and keep
/// the tool-window style in sync. Safe to call with `WindowInner` borrowed:
/// it never calls a function that can synchronously re-enter the window proc.
pub(super) fn sync_hotkey(inner: &mut WindowInner) {
    if inner.quake.config_fullscreen != inner.config.quake.fullscreen {
        inner.quake.config_fullscreen = inner.config.quake.fullscreen;
        inner.quake.fullscreen = inner.config.quake.fullscreen;
    }
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
            set_hotkey_registered(inner.hwnd, true);
            log::info!("quake: registered global hotkey");
        }
        Err(err) => {
            inner.quake.registered = false;
            set_hotkey_registered(inner.hwnd, false);
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
        set_hotkey_registered(inner.hwnd, false);
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
    let (config, fullscreen) = {
        let inner = inner.borrow();
        (inner.config.quake.clone(), inner.quake.fullscreen)
    };
    if config.enabled {
        toggle(hwnd, &config, fullscreen);
    }
    Some(0)
}

/// Hide the window when the new foreground window is outside this window's
/// root-owner family. Called after `FocusChanged(false)` has been dispatched.
pub(super) fn focus_lost(hwnd: HWND, config: &QuakeConfig) {
    if !config.enabled || !config.hide_on_focus_loss || hwnd.is_null() {
        return;
    }
    // If the hotkey could not be registered the window is a visible fallback;
    // hiding it here would leave no way back (no hotkey, no taskbar entry).
    let registered = match rc_from_hwnd(hwnd) {
        Some(inner) => inner.borrow().quake.registered,
        None => false,
    };
    if !registered {
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
    let (enabled, registered) = {
        let inner = inner.borrow();
        (inner.config.quake.enabled, inner.quake.registered)
    };
    // Only hide on close when the hotkey is actually there to bring the window
    // back.  A failed registration falls back to normal window behavior.
    if enabled && registered {
        unsafe {
            ShowWindowAsync(hwnd, SW_HIDE);
        }
        true
    } else {
        false
    }
}

fn toggle(hwnd: HWND, config: &QuakeConfig, fullscreen: bool) {
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    let iconic = unsafe { IsIconic(hwnd) } != 0;
    if visible && !iconic {
        unsafe {
            ShowWindowAsync(hwnd, SW_HIDE);
        };
        return;
    }
    if let Err(err) = show_quake(hwnd, config, fullscreen) {
        log::error!("quake: {err}");
    }
}

fn show_quake(hwnd: HWND, config: &QuakeConfig, fullscreen: bool) -> Result<(), String> {
    let bounds = quake_bounds(config, fullscreen)?;

    // An iconified window (for example after Win+M) has to be restored before
    // it can be placed; SetWindowPos alone does not update its restored rect.
    if unsafe { IsIconic(hwnd) } != 0 {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }

    apply_bounds(hwnd, config, &bounds)?;

    unsafe {
        // The borrow is no longer held here, so a synchronous ShowWindow is
        // safe: WM_SETFOCUS/WM_SIZE handlers may borrow WindowInner again.
        ShowWindow(hwnd, SW_SHOW);
        if SetForegroundWindow(hwnd) == 0 {
            log::warn!("quake: SetForegroundWindow was refused");
        }
    }
    // Moving the hidden window across monitors can make the GUI queue a
    // "keep rows/cols" resize during WM_DPICHANGED.  Its queued task runs
    // after this handler, so re-apply the quake bounds once more behind it.
    reapply_bounds_async(hwnd, config, fullscreen);
    Ok(())
}

/// Re-apply the quake bounds from a queued main-thread task.  Used after a
/// show or fullscreen toggle so a speculative GUI resize cannot win.
fn reapply_bounds_async(hwnd: HWND, config: &QuakeConfig, fullscreen: bool) {
    let config = config.clone();
    promise::spawn::spawn(async move {
        match quake_bounds(&config, fullscreen)
            .and_then(|bounds| apply_bounds(hwnd, &config, &bounds))
        {
            Ok(()) => log::debug!("quake: re-applied bounds"),
            Err(err) => log::warn!("quake: re-applying bounds failed: {err}"),
        }
    })
    .detach();
}

/// Move and size the quake window to `bounds`. Callers must not hold a
/// `WindowInner` borrow: SetWindowPos can synchronously re-enter the window
/// proc, which borrows `WindowInner` again.
fn apply_bounds(hwnd: HWND, config: &QuakeConfig, bounds: &Bounds) -> Result<(), String> {
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
    }
    Ok(())
}

/// Toggle quake fullscreen from the GUI's `ToggleFullScreen` action. The
/// bounds are applied asynchronously because the caller holds a `WindowInner`
/// borrow, and SetWindowPos may re-enter the window proc synchronously.
pub(super) fn toggle_fullscreen(inner: &mut WindowInner) {
    if !inner.config.quake.enabled {
        return;
    }
    inner.quake.fullscreen = !inner.quake.fullscreen;
    let config = inner.config.quake.clone();
    let fullscreen = inner.quake.fullscreen;
    let hwnd = inner.hwnd;
    promise::spawn::spawn(async move {
        match quake_bounds(&config, fullscreen)
            .and_then(|bounds| apply_bounds(hwnd.0, &config, &bounds))
        {
            Ok(()) => {
                log::info!("quake: fullscreen={fullscreen}");
                reapply_bounds_async(hwnd.0, &config, fullscreen);
            }
            Err(err) => {
                log::warn!("quake: {err}");
                // The state was flipped before the asynchronous apply; roll it
                // back so the next F3 matches what is actually on screen.
                if let Some(inner) = rc_from_hwnd(hwnd.0) {
                    let mut inner = inner.borrow_mut();
                    if inner.quake.fullscreen == fullscreen {
                        inner.quake.fullscreen = !fullscreen;
                    }
                }
            }
        }
    })
    .detach();
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

fn quake_bounds(config: &QuakeConfig, fullscreen: bool) -> Result<Bounds, String> {
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

    let mut effective = config.clone();
    let area = if fullscreen {
        // Extend one pixel above the monitor so WezTerm does not infer a
        // FULL_SCREEN state; restoring the work-area height through
        // SetWindowPos is ignored while FULL_SCREEN.
        effective.width_percent = 100;
        effective.height_percent = 100;
        RECT {
            left: info.rcMonitor.left,
            top: info.rcMonitor.top - 1,
            right: info.rcMonitor.right,
            bottom: info.rcMonitor.bottom,
        }
    } else {
        info.rcWork
    };
    bounds_for_work_area(area, &effective)
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
