//! System tray icon on its own thread with a blocking message loop, so it stays
//! responsive (and costs nothing) while the main window is hidden — or if the UI failed.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, MSG, SM_CXSMICON, SetTimer, TranslateMessage,
};

use crate::bt::{Link, Shared, lock};
use crate::{APP_NAME, icon, settings, show_window};

pub fn spawn(shared: Arc<Shared>) {
    let _ = std::thread::Builder::new().name("tray".into()).spawn(move || run(shared));
}

fn any_connected(shared: &Shared) -> bool {
    lock(&shared.ctrls).iter().any(|c| c.link == Link::Connected)
}

/// The tray's own icon size (16 px at 100% scaling, 20 at 125%, ...). Rendering at exactly this
/// size avoids Windows blurring a larger or smaller image.
fn icon_size() -> u32 {
    unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32
}

fn tray_icon(active: bool, size: u32) -> Option<Icon> {
    Icon::from_rgba(icon::rgba(size, if active { icon::ACTIVE } else { icon::IDLE }), size, size).ok()
}

fn tooltip(shared: &Shared) -> String {
    let ctrls = shared.snapshot();
    let status = if ctrls.is_empty() {
        if shared.scanned.load(Ordering::Relaxed) {
            "No paired controller".to_string()
        } else {
            "Searching…".to_string()
        }
    } else {
        ctrls
            .iter()
            .map(|c| match (c.link, c.battery) {
                (Link::Connected, Some(b)) => format!("Connected · {b}%"),
                (Link::Connected, None) => "Connected".into(),
                _ if c.pairing_lost => "Pairing lost: re-pair in the window".into(),
                (Link::Disconnected, _) => "Disconnected".into(),
                (Link::Connecting, _) => "Connecting…".into(),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    // Tooltips are limited to 127 characters.
    format!("{APP_NAME} — {status}").chars().take(127).collect()
}

fn run(shared: Arc<Shared>) {
    // This thread starts before the window makes the process DPI aware; without this, the icon
    // size above would be the virtualised 16 px even at 150% scaling.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let show = MenuItem::new("Show", true, None);
    let keep = CheckMenuItem::new("Auto-reconnect", true, shared.maintain.load(Ordering::Relaxed), None);
    let startup = CheckMenuItem::new("Start with Windows", true, shared.autostart.load(Ordering::Relaxed), None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &show,
        &PredefinedMenuItem::separator(),
        &keep,
        &startup,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let mut active = any_connected(&shared);
    let mut size = icon_size();
    let mut tip = tooltip(&shared);
    let mut builder =
        TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip(&tip);
    if let Some(i) = tray_icon(active, size) {
        builder = builder.with_icon(i);
    }
    let Ok(tray) = builder.build() else { return };

    // Wake once a second to refresh the icon/tooltip; tray and menu clicks wake us immediately.
    unsafe { SetTimer(None, 0, 1000, None) };
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_window(&shared);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == show.id() {
                show_window(&shared);
            } else if ev.id == quit.id() {
                drop(tray); // remove the icon rather than leaving a ghost behind
                std::process::exit(0);
            } else if ev.id == keep.id() {
                let on = keep.is_checked();
                shared.maintain.store(on, Ordering::Relaxed);
                settings::set_keep_connected(on);
            } else if ev.id == startup.id() {
                let on = startup.is_checked();
                if settings::set_autostart(on).is_ok() {
                    shared.autostart.store(on, Ordering::Relaxed);
                }
            }
        }

        if shared.ui_failed.load(Ordering::Relaxed) && show.is_enabled() {
            show.set_text("Window unavailable (see %LOCALAPPDATA%\\XboxConnect\\ui-error.log)");
            show.set_enabled(false);
        }

        // Keep menu checks in sync with changes made in the window.
        keep.set_checked(shared.maintain.load(Ordering::Relaxed));
        startup.set_checked(shared.autostart.load(Ordering::Relaxed));

        // Redraw on a status change, or when the display scaling (and so the icon size) changed.
        let (now, now_size) = (any_connected(&shared), icon_size());
        if (now, now_size) != (active, size) {
            (active, size) = (now, now_size);
            let _ = tray.set_icon(tray_icon(active, size));
        }
        let t = tooltip(&shared);
        if t != tip {
            tip = t;
            let _ = tray.set_tooltip(Some(&tip));
        }
    }
}
