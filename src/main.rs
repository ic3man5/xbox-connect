#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod bt;
mod draw;
mod host;
mod icon;
mod pad;
mod settings;
mod theme;
mod tray;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui::{Align, Layout, RichText, Stroke, Vec2};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, IsIconic, IsWindowVisible, SW_SHOWNORMAL,
};
use windows::core::{BOOL, w};

use bt::{CtrlInfo, Link, Shared, format_address};
use theme::*;
pub const APP_NAME: &str = "Xbox Connect";

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

pub fn dark_title_bar(raw: isize) {
    let dark = BOOL(1);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd(raw),
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Called from the tray thread: ask the UI thread to show the window.
pub fn show_window(_shared: &Shared) {
    host::request_show();
}

fn log_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(&std::env::var_os("LOCALAPPDATA")?).join("XboxConnect");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("v{} at unix time {secs}", env!("CARGO_PKG_VERSION"))
}

fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        if let Some(dir) = log_dir() {
            let _ = std::fs::write(dir.join("crash.log"), format!("{}\n{info}\n", stamp()));
        }
    }));
}

/// The window couldn't be created: keep running tray-only and leave a note for the user.
pub fn report_ui_failure(shared: &Shared, error: &str) {
    shared.hwnd.store(0, Ordering::Relaxed);
    shared.ui_failed.store(true, Ordering::Relaxed);
    if let Some(dir) = log_dir() {
        let _ = std::fs::write(dir.join("ui-error.log"), format!("{}\n{error}\n", stamp()));
    }
}

const SHOW_EVENT: windows::core::PCWSTR = w!("Local\\XboxConnect-show-7f3c2a9e-5b1d-4e8a-9c64-1a2b3c4d5e6f");

/// Returns false if another instance is already running, after asking it (via a named event)
/// to show its window. The running instance shows itself through winit, so its own window
/// state stays consistent, and it can take the foreground because we just launched.
fn single_instance() -> bool {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\XboxConnect-7f3c2a9e-5b1d-4e8a-9c64-1a2b3c4d5e6f"));
        if mutex.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS {
            std::mem::forget(mutex); // hold it for the life of the process
            return true;
        }
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        // The other instance may still be starting up; give it a moment to create the event.
        for _ in 0..20 {
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) {
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

/// Waits for "show yourself" signals from later launches (e.g. the Start Menu shortcut).
fn listen_for_show_requests() {
    let Ok(ev) = (unsafe { CreateEventW(None, false, false, SHOW_EVENT) }) else { return };
    let ev = ev.0 as isize;
    let _ = std::thread::Builder::new().name("show-listener".into()).spawn(move || {
        loop {
            let h = windows::Win32::Foundation::HANDLE(ev as *mut _);
            if unsafe { WaitForSingleObject(h, INFINITE) } == WAIT_OBJECT_0 {
                // The UI may still be starting; keep trying briefly so the request isn't lost.
                for _ in 0..50 {
                    if host::request_show() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            } else {
                return;
            }
        }
    });
}

fn main() {
    install_panic_log();
    if !single_instance() {
        return;
    }
    listen_for_show_requests();
    settings::heal_autostart();

    let shared = Arc::new(Shared::default());
    shared.maintain.store(settings::keep_connected(), Ordering::Relaxed);
    shared.autostart.store(settings::autostart_enabled(), Ordering::Relaxed);
    // Keep-alive and tray don't depend on the UI, so they survive a graphics failure.
    bt::spawn(shared.clone());
    tray::spawn(shared.clone());

    let minimized = std::env::args().any(|a| a == "--minimized");
    if let Err(e) = host::run(shared.clone(), minimized) {
        // The event loop itself couldn't start: tray-only until Quit.
        report_ui_failure(&shared, &e);
        loop {
            std::thread::park();
        }
    }
}

pub struct App {
    shared: Arc<Shared>,
    pad: Arc<std::sync::Mutex<pad::Pad>>,
    demo: Option<std::time::Instant>,
    canvas: draw::Canvas,
    selected: usize,
}

impl App {
    fn new(shared: Arc<Shared>, ctx: egui::Context) -> Self {
        let demo = std::env::args().any(|a| a == "--demo").then(std::time::Instant::now);
        let pad = pad::spawn_poller(ctx, shared.clone());
        Self { shared, pad, demo, canvas: draw::Canvas::default(), selected: 0 }
    }

    fn raw_hwnd(&self) -> HWND {
        hwnd(self.shared.hwnd.load(Ordering::Relaxed))
    }
}
fn link_style(link: Link) -> (&'static str, egui::Color32) {
    match link {
        Link::Connected => ("LINKED", OK),
        Link::Disconnected => ("OFFLINE", WARN),
        Link::Connecting => ("CONNECTING", ACCENT),
    }
}

impl App {
    /// One UI frame (called by the host only while the window is shown).
    pub fn ui(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let visible = unsafe { IsWindowVisible(self.raw_hwnd()).as_bool() };

        let pad = match self.demo {
            Some(t0) => pad::demo(t0.elapsed().as_secs_f32()),
            None => bt::lock(&self.pad).clone(),
        };
        let ctrls = if self.demo.is_some() {
            // Keep demo screenshots consistent: pretend the demo pad is linked over Bluetooth.
            vec![CtrlInfo {
                id: String::new(),
                name: "Xbox Wireless Controller".into(),
                address: 0x0000_0123_4567_89AB,
                link: Link::Connected,
                battery: Some(82),
                firmware: Some("5.23.6.0".into()),
            }]
        } else {
            self.shared.snapshot()
        };
        if self.selected >= ctrls.len() {
            self.selected = 0;
        }
        let sel: Option<&CtrlInfo> = ctrls.get(self.selected);
        let live = sel.is_some_and(|c| c.link == Link::Connected) || pad.connected;

        egui::SidePanel::right("hud")
            .exact_width({
                let w = ctx.screen_rect().width();
                (w * 0.34).clamp(240.0, if w > 1200.0 { 320.0 } else { 290.0 })
            })
            .resizable(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(16)))
            .show(&ctx, |ui| {
                // Short windows: settings pinned at the bottom and the cards scroll.
                // Tall windows: settings follow the cards, so there's no big empty gap.
                let tall = ctx.screen_rect().height() > 900.0;
                if !tall {
                    egui::TopBottomPanel::bottom("hud-actions")
                        .frame(
                            egui::Frame::new().fill(PANEL).inner_margin(egui::Margin { top: 10, ..Default::default() }),
                        )
                        .show_separator_line(false)
                        .show_inside(ui, |ui| self.side_footer(ui));
                }
                let out = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        self.side_panel(ui, &ctrls, &pad);
                        if tall {
                            ui.add_space(14.0);
                            self.side_footer(ui);
                        }
                    });
                // Fade the bottom edge when content continues below, so the cut reads as scrollable.
                let r = out.inner_rect;
                if out.content_size.y > r.height() + out.state.offset.y + 1.0 {
                    let fade = egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom() - 24.0), r.right_bottom());
                    let mut mesh = egui::Mesh::default();
                    let (clear, solid) = (alpha(PANEL, 0), PANEL);
                    mesh.colored_vertex(fade.left_top(), clear);
                    mesh.colored_vertex(fade.right_top(), clear);
                    mesh.colored_vertex(fade.right_bottom(), solid);
                    mesh.colored_vertex(fade.left_bottom(), solid);
                    mesh.add_triangle(0, 1, 2);
                    mesh.add_triangle(0, 2, 3);
                    ui.painter().add(egui::Shape::mesh(mesh));
                }
            });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(16))).show(
            &ctx,
            |ui| {
                // Faint dot grid.
                let r = ui.max_rect();
                let painter = ui.painter();
                let step = 22.0;
                let mut y = r.top();
                while y < r.bottom() {
                    let mut x = r.left();
                    while x < r.right() {
                        painter.circle_filled(egui::pos2(x, y), 0.9, alpha(ACCENT, 14));
                        x += step;
                    }
                    y += step;
                }
                ui.horizontal(|ui| {
                    ui.label(hud("Live input", 11.0, MUTED));
                    if pad.connected {
                        ui.label(hud(&format!("· {}", pad.name), 11.0, ACCENT));
                    }
                });
                draw::controller(ui, &pad, live, &mut self.canvas);
            },
        );

        // Input changes trigger repaints from the input thread; here we only refresh Bluetooth status
        // (or animate the demo). Nothing is scheduled while hidden or minimised.
        if visible && !unsafe { IsIconic(self.raw_hwnd()).as_bool() } {
            ctx.request_repaint_after(Duration::from_millis(if self.demo.is_some() { 16 } else { 1000 }));
        }
    }
}

impl App {
    fn side_panel(&mut self, ui: &mut egui::Ui, ctrls: &[CtrlInfo], pad: &pad::Pad) {
        ui.label(hud("Xbox Connect", 13.0, ACCENT));
        ui.label(hud("Bluetooth keep-alive", 9.0, MUTED));
        ui.add_space(10.0);

        if let Some(err) = bt::lock(&self.shared.error).clone() {
            card(ui, |ui| {
                ui.label(RichText::new(err).color(BAD));
            });
        }

        if ctrls.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, c) in ctrls.iter().enumerate() {
                    let (_, col) = link_style(c.link);
                    let batt = c.battery.map_or(String::new(), |b| format!(" {b}%"));
                    let text = hud(&format!("● {}{batt}", i + 1), 11.0, if i == self.selected { BG } else { col });
                    let b = egui::Button::new(text).fill(if i == self.selected { ACCENT } else { CARD });
                    let tip = format!("{}\n{}\n{}", c.name, format_address(c.address), link_style(c.link).0);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        self.selected = i;
                    }
                }
            });
        }

        match ctrls.get(self.selected) {
            None => card(ui, |ui| {
                let scanned = self.shared.scanned.load(Ordering::Relaxed);
                ui.label(hud(if scanned { "No controller paired" } else { "Scanning…" }, 12.0, WARN));
                ui.label(
                    RichText::new("Hold the pair button on the controller, then add it in Bluetooth settings.")
                        .color(MUTED)
                        .size(12.0),
                );
            }),
            Some(c) => {
                let (status, col) = link_style(c.link);
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 9.0, alpha(col, 40));
                        ui.painter().circle_filled(r.center(), 4.5, col);
                        ui.label(hud(status, 12.0, col));
                    });
                    ui.label(RichText::new(&c.name).size(18.0).strong());
                    ui.label(hud(&format_address(c.address), 10.0, MUTED));
                    ui.label(hud(&format!("Firmware {}", c.firmware.as_deref().unwrap_or("--")), 10.0, MUTED));
                });

                // Battery: GATT reading; falls back to the input API when there's exactly one pad.
                let only_one = ctrls.iter().filter(|c| c.link == Link::Connected).count() == 1 && pad.count == 1;
                let (battery, source) = match (c.battery, pad.battery) {
                    (Some(b), _) => (Some(b), "via Bluetooth"),
                    (None, Some(b)) if only_one && c.link == Link::Connected => (Some(b), "via controller"),
                    _ => (None, ""),
                };
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(hud("Battery", 10.0, MUTED));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(hud(source, 9.0, alpha(MUTED, 160)))
                        });
                    });
                    let level = battery.map(|b| b as f32 / 100.0);
                    let col = match battery {
                        Some(b) if b <= 15 => BAD,
                        Some(b) if b <= 35 => WARN,
                        Some(_) => ACCENT,
                        None => DIM,
                    };
                    ui.label(
                        RichText::new(battery.map_or("--".to_string(), |b| format!("{b}%")))
                            .size(34.0)
                            .family(egui::FontFamily::Monospace)
                            .color(col),
                    );
                    // Segmented gauge.
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), egui::Sense::hover());
                    let n = 12;
                    let gap = 3.0;
                    let w = (r.width() - gap * (n as f32 - 1.0)) / n as f32;
                    for i in 0..n {
                        let seg = egui::Rect::from_min_size(
                            egui::pos2(r.left() + i as f32 * (w + gap), r.top()),
                            Vec2::new(w, r.height()),
                        );
                        let lit = level.is_some_and(|l| (i as f32 + 0.5) / n as f32 <= l);
                        ui.painter().rect_filled(seg, 2.0, if lit { col } else { alpha(DIM, 50) });
                    }
                });

                card(ui, |ui| {
                    // Live input follows whichever pad was used last, so label it by that pad.
                    let src = if pad.connected { pad.name.as_str() } else { "no signal" };
                    ui.label(hud(&format!("Input · {src}"), 10.0, MUTED));
                    ui.add_space(2.0);
                    egui::Grid::new("info").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label(hud("Share", 10.0, MUTED));
                        if pad.has_share {
                            ui.label(hud("Mapped", 10.0, TEXT));
                        } else {
                            ui.label(hud("Not detected", 10.0, MUTED));
                        }
                        ui.end_row();
                        // Elite paddles aren't visible from the front, so show them as indicator chips.
                        ui.label(hud("Paddles", 10.0, MUTED));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for (i, &bit) in pad::PADDLES.iter().enumerate() {
                                let lit = pad.connected && pad.down(bit);
                                let (r, _) = ui.allocate_exact_size(Vec2::new(24.0, 15.0), egui::Sense::hover());
                                ui.painter().rect(
                                    r,
                                    3.0,
                                    if lit { ACCENT } else { egui::Color32::TRANSPARENT },
                                    Stroke::new(1.0_f32, alpha(ACCENT, if lit { 255 } else { 70 })),
                                    egui::StrokeKind::Inside,
                                );
                                ui.painter().text(
                                    r.center(),
                                    egui::Align2::CENTER_CENTER,
                                    format!("P{}", i + 1),
                                    egui::FontId::monospace(9.0),
                                    if lit { BG } else { alpha(MUTED, 200) },
                                );
                            }
                        });
                        ui.end_row();
                        for (label, (x, y)) in [("L stick", pad.left), ("R stick", pad.right)] {
                            ui.label(hud(label, 10.0, MUTED));
                            if pad.connected {
                                ui.label(hud(&format!("{x:+.2} {y:+.2}"), 10.0, TEXT));
                            } else {
                                ui.label(hud("—", 10.0, MUTED));
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        }
    }

    /// Settings and actions, pinned to the bottom of the side panel so they never scroll away.
    fn side_footer(&mut self, ui: &mut egui::Ui) {
        let mut keep = self.shared.maintain.load(Ordering::Relaxed);
        if toggle(ui, &mut keep, "Keep connected").changed() {
            self.shared.maintain.store(keep, Ordering::Relaxed);
            settings::set_keep_connected(keep);
        }
        let mut auto = self.shared.autostart.load(Ordering::Relaxed);
        if toggle(ui, &mut auto, "Start with Windows").changed() && settings::set_autostart(auto).is_ok() {
            self.shared.autostart.store(auto, Ordering::Relaxed);
        }
        ui.add_space(2.0);
        if action_button(ui, "Bluetooth settings").clicked() {
            unsafe { ShellExecuteW(None, w!("open"), w!("ms-settings:bluetooth"), None, None, SW_SHOWNORMAL) };
        }
        ui.label(RichText::new("Closing the window keeps it running in the tray.").size(11.0).color(MUTED));
    }
}

/// Full-width outlined button with a centred HUD label and an external-link arrow.
fn action_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
    let hot = resp.hovered();
    let p = ui.painter();
    p.rect(
        rect,
        6.0,
        if hot { alpha(ACCENT, 25) } else { CARD },
        Stroke::new(1.0_f32, alpha(ACCENT, if hot { 255 } else { 120 })),
        egui::StrokeKind::Inside,
    );
    let galley = ui.painter().layout_job(match hud(label, 11.0, ACCENT) {
        egui::WidgetText::LayoutJob(job) => (*job).clone(),
        other => other.into_layout_job(ui.style(), egui::FontSelection::Default, Align::Center).as_ref().clone(),
    });
    // Label centred as a group with the arrow; the arrow glyph sits low, so lift it slightly.
    let arrow_w = 22.0;
    let start = rect.center() - Vec2::new((galley.size().x + arrow_w) / 2.0, galley.size().y / 2.0);
    let arrow_at = egui::pos2(start.x + galley.size().x + arrow_w / 2.0 + 2.0, rect.center().y - 1.5);
    p.galley(start, galley, ACCENT);
    p.text(arrow_at, egui::Align2::CENTER_CENTER, "↗", egui::FontId::proportional(12.0), ACCENT);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
