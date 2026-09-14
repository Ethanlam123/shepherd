//! Tray icon, badge refresh, and panel show/hide/positioning.

use crate::icon;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};

/// Timestamp (ms) of the last blur-triggered hide. Clicking the tray while the
/// panel is visible first blurs (hides) the panel, then delivers the click -
/// that click must not re-show it.
static LAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Panel", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Shepherd", true, None::<&str>)?;
    let menu = MenuBuilder::new(app)
        .item(&show)
        .separator()
        .item(&quit)
        .build()?;

    TrayIconBuilder::with_id("main")
        .icon(icon::crook_template())
        .icon_as_template(true)
        .tooltip("Shepherd")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_panel(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn toggle(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            note_hide();
        } else {
            show_panel(app);
        }
    }
}

pub(crate) fn show_panel(app: &AppHandle) {
    let Some(win) = app.get_webview_window("panel") else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.set_focus();
        return;
    }
    // ignore clicks that immediately follow a blur-hide (tray click race)
    if now_ms().saturating_sub(LAST_HIDE_MS.load(Ordering::Relaxed)) < 250 {
        return;
    }
    position(&win, app);
    let _ = win.show();
    let _ = win.set_focus();
}

/// Hide on focus loss (popover behavior).
pub fn blur_hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            note_hide();
        }
    }
}

/// Swap the tray icon between the plain crook (template) and the crook with
/// amber count badge. Non-template badge icons need manual light/dark strokes;
/// appearance is re-read on every badge change.
/// ponytail: auto-appearance switches may lag until the next badge change.
pub fn refresh_badge(app: &AppHandle, count: usize) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    if count == 0 {
        let _ = tray.set_icon(Some(icon::crook_template()));
        let _ = tray.set_icon_as_template(true);
    } else {
        let _ = tray.set_icon(Some(icon::badge(count)));
        let _ = tray.set_icon_as_template(false);
    }
}

/// Clamp the panel's centered x so it stays on the tray's monitor with an
/// 8px inset. Pure math, unit-tested.
fn clamp_panel_x(center_x: f64, win_w: f64, mon_x: f64, mon_w: f64) -> f64 {
    let min_x = mon_x + 8.0;
    let max_x = (mon_x + mon_w - win_w - 8.0).max(min_x);
    center_x.clamp(min_x, max_x)
}

/// Center the panel under the tray icon, clamped to its monitor.
fn position(win: &WebviewWindow, app: &AppHandle) {
    let Some(rect) = app.tray_by_id("main").and_then(|t| t.rect().ok().flatten()) else {
        return;
    };
    let scale = win.scale_factor().unwrap_or(2.0);
    let (rx, ry) = match rect.position {
        tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
        tauri::Position::Logical(p) => (p.x * scale, p.y * scale),
    };
    let (rw, rh) = match rect.size {
        tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
        tauri::Size::Logical(s) => (s.width * scale, s.height * scale),
    };
    let win_w = 400.0 * scale;
    let mut x = rx + rw / 2.0 - win_w / 2.0;
    let y = ry + rh + 6.0 * scale;
    if let Ok(Some(mon)) = win.current_monitor() {
        let mon_x = mon.position().x as f64;
        x = clamp_panel_x(x, win_w, mon_x, mon.size().width as f64);
    }
    let _ = win.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
}

fn note_hide() {
    LAST_HIDE_MS.store(now_ms(), Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::clamp_panel_x;

    #[test]
    fn panel_x_stays_centered_when_it_fits() {
        // monitor 0..2000, window 400, tray centered at 1000
        assert!((clamp_panel_x(800.0, 400.0, 0.0, 2000.0) - 800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn panel_x_clamps_on_the_left() {
        // tray icon far left: centered x would be negative
        assert_eq!(clamp_panel_x(-100.0, 400.0, 0.0, 2000.0), 8.0);
    }

    #[test]
    fn panel_x_clamps_on_the_right() {
        // tray icon far right on a 2000px monitor: 1600 > max 1592
        assert_eq!(clamp_panel_x(1900.0, 400.0, 0.0, 2000.0), 1592.0);
    }

    #[test]
    fn panel_x_degrades_to_inset_on_monitor_narrower_than_window() {
        // window wider than monitor: max collapses to min
        assert_eq!(clamp_panel_x(50.0, 800.0, 0.0, 600.0), 8.0);
    }
}
