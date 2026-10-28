//! Neon HUD look: near-black background, cyan accent, monospace letter-spaced labels.

use egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, Sense, Stroke, StrokeKind, TextFormat, Ui, Vec2,
    WidgetText, text::LayoutJob,
};

pub const BG: Color32 = Color32::from_rgb(6, 8, 12);
pub const PANEL: Color32 = Color32::from_rgb(10, 14, 20);
pub const CARD: Color32 = Color32::from_rgb(14, 19, 27);
pub const WELL: Color32 = Color32::from_rgb(5, 7, 11);
pub const ACCENT: Color32 = Color32::from_rgb(64, 232, 255);
pub const DIM: Color32 = Color32::from_rgb(78, 92, 112);
pub const TEXT: Color32 = Color32::from_rgb(226, 238, 255);
pub const MUTED: Color32 = Color32::from_rgb(110, 125, 148);
pub const OK: Color32 = Color32::from_rgb(92, 255, 160);
pub const WARN: Color32 = Color32::from_rgb(255, 184, 76);
pub const BAD: Color32 = Color32::from_rgb(255, 80, 110);

pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Use the fonts every Windows 10/11 install already has instead of bundling ~1 MB of fonts.
fn load_system_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    let dir =
        std::env::var_os("WINDIR").map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), Into::into).join("Fonts");
    let mut defs = egui::FontDefinitions::default();
    let mut load = |key: &str, files: &[&str]| -> bool {
        let found = files.iter().find_map(|f| std::fs::read(dir.join(f)).ok());
        if let Some(bytes) = found {
            defs.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
        }
        defs.font_data.contains_key(key)
    };
    let ui = load("ui", &["segoeui.ttf", "arial.ttf"]);
    let mono = load("mono", &["CascadiaMono.ttf", "consola.ttf", "cour.ttf"]);
    let sym = load("symbol", &["seguisym.ttf"]);
    let pick =
        |names: &[(&str, bool)]| names.iter().filter(|(_, ok)| *ok).map(|(n, _)| n.to_string()).collect::<Vec<_>>();
    defs.families.insert(FontFamily::Proportional, pick(&[("ui", ui), ("mono", mono), ("symbol", sym)]));
    defs.families.insert(FontFamily::Monospace, pick(&[("mono", mono), ("ui", ui), ("symbol", sym)]));
    ctx.set_fonts(defs);
}

pub fn apply(ctx: &egui::Context) {
    load_system_fonts(ctx);
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|style| {
        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = WELL;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = alpha(ACCENT, 60);
        v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        v.hyperlink_color = ACCENT;
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            w.corner_radius = CornerRadius::same(6);
        }
        v.widgets.inactive.bg_fill = CARD;
        v.widgets.inactive.weak_bg_fill = CARD;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, alpha(ACCENT, 50));
        v.widgets.hovered.weak_bg_fill = alpha(ACCENT, 25);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
        v.widgets.active.weak_bg_fill = alpha(ACCENT, 45);
        v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        style.spacing.scroll.bar_width = 4.0;
    });
}

/// Monospace, upper-case, letter-spaced text — the HUD label style.
pub fn hud(text: &str, size: f32, color: Color32) -> WidgetText {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        TextFormat {
            font_id: FontId::new(size, FontFamily::Monospace),
            color,
            extra_letter_spacing: size * 0.18,
            ..Default::default()
        },
    );
    job.into()
}

/// A dark card with a thin accent edge.
pub fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    card_outlined(ui, alpha(ACCENT, 38), add);
}

/// A card with its own border colour, for one that should stand out (e.g. a task in progress).
pub fn card_outlined(ui: &mut Ui, border: Color32, add: impl FnOnce(&mut Ui)) {
    // Inner width = outer width minus margins and stroke, so cards never push the panel wider.
    let inner = (ui.available_width() - 2.0 * 12.0 - 2.0).max(0.0);
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(inner);
            add(ui)
        });
}

/// A settings choice: the label (styled like a toggle's) over a full-width selector showing the
/// current option; clicking opens the list. Returns the index picked this frame, if any.
pub fn dropdown(ui: &mut Ui, label: &str, current: usize, options: &[(&str, &str)]) -> Option<usize> {
    let enabled = ui.is_enabled();
    let text = |s: &str, size: f32, color: Color32| {
        let mut job = LayoutJob::default();
        job.append(
            &s.to_uppercase(),
            0.0,
            TextFormat {
                font_id: FontId::new(size, FontFamily::Monospace),
                color,
                extra_letter_spacing: size * 0.15,
                ..Default::default()
            },
        );
        job
    };
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
    let g = ui.painter().layout_job(text(label, 11.0, alpha(TEXT, if enabled { 218 } else { 110 })));
    ui.painter().galley(egui::pos2(row.left(), row.center().y - g.size().y / 2.0), g, TEXT);

    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
    let hot = resp.hovered() && enabled;
    let p = ui.painter();
    p.rect(
        rect,
        6.0,
        WELL,
        Stroke::new(1.0_f32, if hot { ACCENT } else { alpha(ACCENT, if enabled { 90 } else { 35 }) }),
        StrokeKind::Inside,
    );
    let col = if enabled { ACCENT } else { DIM };
    let g = p.layout_job(text(options.get(current).map_or("", |o| o.0), 10.0, col));
    p.galley(egui::pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, col);
    p.text(
        egui::pos2(rect.right() - 12.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        "▾",
        FontId::proportional(12.0),
        col,
    );

    // The list, drawn in the HUD style rather than egui's stock menu: a card-coloured panel, rows
    // that light up on hover, and an accent bar marking the current choice.
    let mut picked = None;
    let frame = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, alpha(ACCENT, 90)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(4));
    egui::Popup::menu(&resp).width(rect.width()).gap(4.0).frame(frame).show(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, (name, about)) in options.iter().enumerate() {
            let on = i == current;
            let (row, r) = ui.allocate_exact_size(Vec2::new(rect.width() - 8.0, 40.0), Sense::click());
            let p = ui.painter();
            if r.hovered() && enabled {
                p.rect_filled(row, 4.0, alpha(ACCENT, 28));
            }
            if on {
                p.rect_filled(
                    egui::Rect::from_min_size(
                        row.left_top() + Vec2::new(0.0, 6.0),
                        Vec2::new(2.0, row.height() - 12.0),
                    ),
                    1.0,
                    ACCENT,
                );
            }
            let g = p.layout_job(text(name, 10.0, if on { ACCENT } else { TEXT }));
            let name_h = g.size().y;
            p.galley(egui::pos2(row.left() + 10.0, row.top() + 7.0), g, TEXT);
            let mut job = LayoutJob::single_section(
                about.to_string(),
                TextFormat { font_id: FontId::proportional(11.0), color: MUTED, ..Default::default() },
            );
            job.wrap.max_width = row.width() - 16.0;
            job.wrap.max_rows = 1; // one line; the selector's width is the list's width
            p.galley(egui::pos2(row.left() + 10.0, row.top() + 9.0 + name_h), p.layout_job(job), MUTED);
            // A pick made while the control is disabled (e.g. switched off from the tray) is ignored.
            // The full description on hover, in case a longer one is ever cut to fit.
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(*about).clicked() && enabled {
                picked = Some(i);
                ui.close();
            }
        }
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_disabled_hover_text("Only applies while Auto-reconnect is on.");
    picked
}

/// Pill toggle switch with a glowing knob.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let size = Vec2::new(ui.available_width(), 26.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let p = ui.painter();
    let galley = ui.painter().layout_job({
        let mut job = LayoutJob::default();
        job.append(
            &label.to_uppercase(),
            0.0,
            TextFormat {
                font_id: FontId::new(11.0, FontFamily::Monospace),
                color: alpha(TEXT, 218),
                extra_letter_spacing: 1.6,
                ..Default::default()
            },
        );
        job
    });
    p.galley(egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, TEXT);

    let track =
        egui::Rect::from_min_size(egui::pos2(rect.right() - 44.0, rect.center().y - 10.0), Vec2::new(44.0, 20.0));
    p.rect(
        track,
        10.0,
        lerp(WELL, alpha(ACCENT, 70), t),
        Stroke::new(1.0_f32, lerp(DIM, ACCENT, t)),
        StrokeKind::Inside,
    );
    let knob = egui::pos2(egui::lerp(track.left() + 10.0..=track.right() - 10.0, t), track.center().y);
    if t > 0.0 {
        p.circle_filled(knob, 12.0, alpha(ACCENT, (40.0 * t) as u8));
    }
    p.circle_filled(knob, 7.0, lerp(MUTED, ACCENT, t));
    resp
}
