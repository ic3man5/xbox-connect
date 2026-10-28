#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adv;
mod art;
mod bt;
mod draw;
mod events;
mod host;
mod icon;
mod pad;
mod pair;
mod reconnect;
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

use bt::{CtrlInfo, Fullscreen, Link, Repair, Shared, format_address};
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
    shared.fullscreen.store(settings::fullscreen_mode(), Ordering::Relaxed);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Input,
    Events,
}

pub struct App {
    shared: Arc<Shared>,
    pad: Arc<std::sync::Mutex<pad::Pad>>,
    demo: Option<std::time::Instant>,
    canvas: draw::Canvas,
    selected: usize,
    tab: Tab,
    /// Jump the Events list to its newest line on the next frame (set when the tab is opened).
    scroll_events: bool,
    /// First click on a re-pair control arms it for a few seconds; the second one starts it.
    confirm_repair: Option<(u64, std::time::Instant)>,
    /// The address of the last pairing job started (0 = any controller), for "Try again".
    last_repair: u64,
    /// The window is visible and not minimised this frame (animations only run then).
    shown: bool,
}

impl App {
    fn new(shared: Arc<Shared>, ctx: egui::Context) -> Self {
        let demo = std::env::args().any(|a| a == "--demo").then(std::time::Instant::now);
        let pad = pad::spawn_poller(ctx, shared.clone());
        Self {
            shared,
            pad,
            demo,
            canvas: draw::Canvas::default(),
            selected: 0,
            tab: Tab::Input,
            scroll_events: false,
            confirm_repair: None,
            last_repair: 0,
            shown: false,
        }
    }

    fn raw_hwnd(&self) -> HWND {
        hwnd(self.shared.hwnd.load(Ordering::Relaxed))
    }
}
fn link_style(c: &CtrlInfo) -> (&'static str, egui::Color32) {
    match c.link {
        Link::Connected => ("LINKED", OK),
        _ if c.pairing_lost => ("PAIRING LOST", BAD),
        Link::Disconnected => ("OFFLINE", WARN),
        Link::Connecting => ("CONNECTING", ACCENT),
    }
}

impl App {
    /// One UI frame (called by the host only while the window is shown).
    pub fn ui(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let visible = unsafe { IsWindowVisible(self.raw_hwnd()).as_bool() };
        self.shown = visible && !unsafe { IsIconic(self.raw_hwnd()).as_bool() };

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
                pairing_lost: false,
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
                self.tab_bar(ui, &pad);
                match self.tab {
                    Tab::Input => draw::controller(ui, &pad, live, &mut self.canvas),
                    Tab::Events => events_view(ui, &mut self.scroll_events, ctrls.len() <= 1),
                }
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
        if let Some(err) = bt::lock(&self.shared.error).clone() {
            card(ui, |ui| {
                ui.label(RichText::new(err).color(BAD));
            });
        }
        self.repair_card(ui);

        if ctrls.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for (i, c) in ctrls.iter().enumerate() {
                    let (_, col) = link_style(c);
                    let batt = c.battery.map_or(String::new(), |b| format!(" {b}%"));
                    let text = hud(&format!("● {}{batt}", i + 1), 11.0, if i == self.selected { BG } else { col });
                    let b = egui::Button::new(text).fill(if i == self.selected { ACCENT } else { CARD });
                    let tip = format!("{}\n{}\n{}", c.name, format_address(c.address), link_style(c).0);
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
                    RichText::new("Hold the pair button on top of the controller until the light flashes fast.")
                        .color(MUTED)
                        .size(12.0),
                );
                if scanned && !self.repair_busy() {
                    ui.add_space(6.0);
                    if action_button(ui, "Pair a controller", false).clicked() {
                        self.start_repair(0);
                    }
                }
            }),
            Some(c) => {
                let (status, col) = link_style(c);
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
                    if c.link != Link::Connected && !self.repair_busy() {
                        let armed = self
                            .confirm_repair
                            .is_some_and(|(a, t)| a == c.address && t.elapsed() < Duration::from_secs(4));
                        ui.add_space(6.0);
                        let clicked = if c.pairing_lost {
                            // The pad keeps getting refused: re-pairing is the fix, so offer it plainly.
                            ui.label(
                                RichText::new(
                                    "It reaches this PC but gets refused: the controller has forgotten the pairing.",
                                )
                                .color(TEXT)
                                .size(12.0),
                            );
                            ui.add_space(4.0);
                            action_button(
                                ui,
                                if armed { "Click again to re-pair" } else { "Re-pair controller" },
                                false,
                            )
                            .clicked()
                        } else {
                            // Usually it's just switched off; keep re-pairing a quiet secondary option.
                            let text = if armed { "Click again to re-pair" } else { "Won't connect? Re-pair…" };
                            ui.add(egui::Label::new(hud(text, 10.0, ACCENT)).sense(egui::Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                        };
                        if clicked {
                            if armed {
                                self.confirm_repair = None;
                                self.start_repair(c.address);
                            } else {
                                self.confirm_repair = Some((c.address, std::time::Instant::now()));
                            }
                        }
                    }
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
                    // Percentage and segmented gauge on one row: the number at a fixed width (so the
                    // gauge doesn't shift as it changes), the gauge filling the rest.
                    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::hover());
                    let text = battery.map_or("--".to_string(), |b| format!("{b}%"));
                    ui.painter().text(
                        row.left_center(),
                        egui::Align2::LEFT_CENTER,
                        text,
                        egui::FontId::monospace(24.0),
                        col,
                    );
                    let number_w = 72.0; // "100%" at 24 pt mono
                    let r = egui::Rect::from_min_max(
                        egui::pos2(row.left() + number_w, row.center().y - 5.0),
                        egui::pos2(row.right(), row.center().y + 5.0),
                    );
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
        let resp = toggle(ui, &mut keep, "Auto-reconnect").on_hover_text(
            "Reconnect controllers automatically when they're turned on, and keep them connected.\n\
             Off: the app only shows status and leaves reconnecting to Windows.",
        );
        if resp.changed() {
            self.shared.maintain.store(keep, Ordering::Relaxed);
            settings::set_keep_connected(keep);
        }
        // Only meaningful while auto-reconnect is on (that's what it slows down or pauses).
        ui.add_space(4.0);
        ui.add_enabled_ui(keep, |ui| {
            let current = self.shared.fullscreen.load(Ordering::Relaxed) as usize;
            let options = [
                (Fullscreen::Normal.label(), "No change."),
                (Fullscreen::Slow.label(), "Every 20 s; may connect slower."),
                (Fullscreen::Pause.label(), "App stops; Windows may still reconnect."),
            ];
            if let Some(i) = dropdown(ui, "During fullscreen games", current, &options) {
                let mode = Fullscreen::ALL[i] as u8;
                self.shared.fullscreen.store(mode, Ordering::Relaxed);
                settings::set_fullscreen_mode(mode);
            }
        });
        ui.add_space(4.0);
        let mut auto = self.shared.autostart.load(Ordering::Relaxed);
        if toggle(ui, &mut auto, "Start with Windows").changed() && settings::set_autostart(auto).is_ok() {
            self.shared.autostart.store(auto, Ordering::Relaxed);
        }
        ui.add_space(2.0);
        if action_button(ui, "Bluetooth settings", true).clicked() {
            unsafe { ShellExecuteW(None, w!("open"), w!("ms-settings:bluetooth"), None, None, SW_SHOWNORMAL) };
        }
        ui.label(RichText::new("Closing the window keeps it running in the tray.").size(11.0).color(MUTED));
    }
}

impl App {
    fn repair_busy(&self) -> bool {
        bt::lock(&self.shared.repair).busy()
    }

    fn start_repair(&mut self, addr: u64) {
        let mut r = bt::lock(&self.shared.repair);
        if !r.busy() {
            self.last_repair = addr;
            self.shared.repair_cancel.store(false, Ordering::Relaxed);
            *r = Repair::Requested(addr);
        }
    }

    fn cancel_repair(&self) {
        let mut r = bt::lock(&self.shared.repair);
        match *r {
            // Not picked up yet: nothing has happened, so just withdraw it.
            Repair::Requested(_) => *r = Repair::Idle,
            _ => self.shared.repair_cancel.store(true, Ordering::Relaxed),
        }
    }

    /// "Live input | Events" tabs on a faint baseline, with the active pad's name at the far end.
    fn tab_bar(&mut self, ui: &mut egui::Ui, pad: &pad::Pad) {
        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (tab, name) in [(Tab::Input, "Live input"), (Tab::Events, "Events")] {
                let on = self.tab == tab;
                let id = ui.next_auto_id();
                let hot = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
                let col = if on {
                    ACCENT
                } else if hot {
                    alpha(ACCENT, 180)
                } else {
                    MUTED
                };
                let r = ui.add(egui::Label::new(hud(name, 11.0, col)).sense(egui::Sense::click()));
                if on {
                    let y = r.rect.bottom() + 6.0;
                    let (a, b) = (egui::pos2(r.rect.left(), y), egui::pos2(r.rect.right(), y));
                    ui.painter().line_segment([a, b], Stroke::new(4.0_f32, alpha(ACCENT, 60)));
                    ui.painter().line_segment([a, b], Stroke::new(2.0_f32, ACCENT));
                }
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                    self.tab = tab;
                    self.scroll_events = tab == Tab::Events;
                    self.shared.events_tab.store(tab == Tab::Events, Ordering::Relaxed);
                }
            }
            if self.tab == Tab::Input && pad.connected {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(hud(&pad.name, 10.0, alpha(ACCENT, 200)));
                });
            }
        });
        let r = row.response.rect;
        let y = r.bottom() + 6.0;
        ui.painter().line_segment(
            [egui::pos2(r.left(), y), egui::pos2(ui.max_rect().right(), y)],
            Stroke::new(1.0_f32, alpha(ACCENT, 30)),
        );
        ui.add_space(8.0);
    }

    /// Pairing progress, shown above everything else while it runs (the controller drops out of
    /// the paired list part-way through a re-pair).
    fn repair_card(&mut self, ui: &mut egui::Ui) {
        let state = bt::lock(&self.shared.repair).clone();
        let (title, col, body) = match &state {
            Repair::Idle => return,
            Repair::Requested(_) | Repair::Waiting => (
                "Waiting for controller",
                ACCENT,
                "Hold the small pair button on top of the controller until the Xbox light flashes fast. \
                 Nothing changes until it does."
                    .to_string(),
            ),
            Repair::Removing => ("Re-pairing", ACCENT, "Controller found. Removing the old pairing…".to_string()),
            Repair::Pairing => ("Pairing", ACCENT, "Controller found, pairing…".to_string()),
            Repair::Done => ("Paired", OK, "It will reconnect on its own from now on.".to_string()),
            Repair::Failed(e) => ("Pairing failed", BAD, e.clone()),
        };
        let busy = state.busy();
        card_outlined(ui, alpha(col, 90), |ui| {
            ui.label(hud(title, 12.0, col));
            ui.label(RichText::new(body).color(TEXT).size(12.0));
            if busy {
                // Indeterminate sweep in the battery-gauge style, so a long wait never looks frozen.
                ui.add_space(6.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), egui::Sense::hover());
                let n = 12;
                let gap = 3.0;
                let w = (r.width() - gap * (n as f32 - 1.0)) / n as f32;
                let head = (ui.input(|i| i.time) * 8.0) as usize % (n + 3);
                for i in 0..n {
                    let seg = egui::Rect::from_min_size(
                        egui::pos2(r.left() + i as f32 * (w + gap), r.top()),
                        Vec2::new(w, r.height()),
                    );
                    let lit = i <= head && head < i + 3;
                    ui.painter().rect_filled(seg, 2.0, if lit { ACCENT } else { alpha(ACCENT, 35) });
                }
                if self.shown {
                    ui.ctx().request_repaint_after(Duration::from_millis(33));
                }
            }
            ui.add_space(8.0);
            // After a failure the old pairing may already be gone, so offer the way back here
            // (the controller may no longer be listed, and "Pair a controller" only shows when none are).
            if matches!(state, Repair::Failed(_)) && action_button(ui, "Try again", false).clicked() {
                let addr = self.last_repair;
                self.start_repair(addr);
                return;
            }
            if secondary_button(ui, if busy { "Cancel" } else { "Dismiss" }).clicked() {
                if busy {
                    self.cancel_repair();
                } else {
                    *bt::lock(&self.shared.repair) = Repair::Idle;
                }
            }
        });
    }
}

/// The connection history (events.log) as a two-column list: time, then the message (wrapped
/// lines hang under the message). Follows new lines as they arrive; a rule marks each new day.
fn events_view(ui: &mut egui::Ui, jump_to_end: &mut bool, strip_names: bool) {
    egui::Frame::new()
        .fill(alpha(CARD, 220))
        .stroke(Stroke::new(1.0_f32, alpha(ACCENT, 40)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(hud("Connection history", 10.0, MUTED));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let open =
                        ui.add(egui::Label::new(hud("Open log folder ↗", 10.0, ACCENT)).sense(egui::Sense::click()));
                    if open.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                        && let Some(dir) = log_dir()
                    {
                        let dir = windows::core::HSTRING::from(dir.as_os_str());
                        unsafe { ShellExecuteW(None, w!("open"), &dir, None, None, SW_SHOWNORMAL) };
                    }
                });
            });
            ui.add_space(6.0);
            let lines = events::recent();
            let mono = |s: &str, col| RichText::new(s).family(egui::FontFamily::Monospace).size(11.0).color(col);
            egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(hud("Nothing yet.", 11.0, MUTED));
                }
                let mut day = "";
                for line in &lines {
                    // "YYYY-MM-DD HH:MM:SS  message"; anything else is shown whole.
                    let (date, time, msg) = match (line.get(..10), line.get(11..19), line.get(19..)) {
                        (Some(d), Some(t), Some(m)) => (d, t, m.trim_start()),
                        _ => ("", "", line.as_str()),
                    };
                    if date != day {
                        day = date;
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(hud(date, 10.0, MUTED));
                            let (r, _) =
                                ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), egui::Sense::hover());
                            ui.painter().line_segment(
                                [r.left_center(), r.right_center()],
                                Stroke::new(1.0_f32, alpha(ACCENT, 30)),
                            );
                        });
                    }
                    // With one controller, its "Name AA:BB:…:" prefix on every line is just noise.
                    let msg = match msg.split_once(": ") {
                        Some((who, rest)) if strip_names && looks_like_device(who) && !who.starts_with("Pairing") => {
                            rest.to_string()
                        }
                        _ if strip_names => strip_addresses(msg),
                        _ => msg.to_string(),
                    };
                    let msg = msg.as_str();
                    let lower = msg.to_lowercase();
                    let col = if ["fail", "error", "lost", "refused", "without a working link", "isn't"]
                        .iter()
                        .any(|w| lower.contains(w))
                    {
                        BAD
                    } else if lower.contains("disconnected") || lower.contains("turned off") {
                        WARN
                    } else if lower == "connected" || lower.contains("reconnected") || lower.contains("succeeded") {
                        OK
                    } else if lower.contains("pairing mode") || lower.contains("advertising") {
                        ACCENT
                    } else {
                        alpha(TEXT, 170)
                    };
                    ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.add_sized([62.0, 14.0], egui::Label::new(mono(time, MUTED)));
                        ui.add(egui::Label::new(mono(msg, col)).wrap_mode(egui::TextWrapMode::Wrap));
                    });
                }
                if std::mem::take(jump_to_end) {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
        });
}

/// Replaces Bluetooth addresses ("78:86:2E:E1:D5:35") in a log line with "the controller", so
/// sentences that use the address as a noun still read ("Pairing: the controller is in pairing mode").
fn strip_addresses(msg: &str) -> String {
    let b = msg.as_bytes();
    let mut out = String::with_capacity(msg.len());
    let mut i = 0;
    while i < b.len() {
        if b.len() - i >= 17
            && msg.is_char_boundary(i)
            && msg.is_char_boundary(i + 17)
            && looks_like_device(&msg[i..i + 17])
        {
            out.push_str("the controller");
            i += 17;
            continue;
        }
        let ch = msg[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// "Xbox Wireless Controller 78:86:2E:E1:D5:35": ends in a Bluetooth address.
fn looks_like_device(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 17 && {
        let mac = &b[b.len() - 17..];
        (0..17).all(|i| if i % 3 == 2 { mac[i] == b':' } else { mac[i].is_ascii_hexdigit() })
    }
}

/// Quieter, shorter button for Cancel / Dismiss, so it never competes with the main action.
fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), egui::Sense::click());
    let hot = resp.hovered();
    ui.painter().rect(
        rect,
        6.0,
        if hot { alpha(MUTED, 25) } else { egui::Color32::TRANSPARENT },
        Stroke::new(1.0_f32, alpha(MUTED, if hot { 160 } else { 80 })),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label.to_uppercase(),
        egui::FontId::monospace(10.0),
        if hot { TEXT } else { MUTED },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Full-width outlined button with a centred HUD label (and an external-link arrow if `arrow`).
fn action_button(ui: &mut egui::Ui, label: &str, arrow: bool) -> egui::Response {
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
    let arrow_w = if arrow { 22.0 } else { 0.0 };
    let start = rect.center() - Vec2::new((galley.size().x + arrow_w) / 2.0, galley.size().y / 2.0);
    let arrow_at = egui::pos2(start.x + galley.size().x + arrow_w / 2.0 + 2.0, rect.center().y - 1.5);
    p.galley(start, galley, ACCENT);
    if arrow {
        p.text(arrow_at, egui::Align2::CENTER_CENTER, "↗", egui::FontId::proportional(12.0), ACCENT);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
