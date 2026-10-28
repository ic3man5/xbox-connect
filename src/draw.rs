//! Live controller view: the bundled line drawing on a shaded body with a neon rim.
//! Presses light up the drawing's own button shapes; stick caps move with the sticks.

use std::time::{Duration, Instant};

use egui::{self, Align2, Color32, FontFamily, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use crate::art::{self, Art, Region};
use crate::pad::{self, Pad};
use crate::theme::*;

const FULL_UV: Rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
const LINE_IDLE: Color32 = Color32::from_rgb(110, 125, 148);
/// Bumper hump centres and top edge in design px (measured on the drawing).
const LB_X: f32 = 255.0;
const RB_X: f32 = 769.0;
const BUMPER_TOP: f32 = 14.0;

/// Holds the rasterised artwork; rebuilt (debounced) when the on-screen size or state changes.
#[derive(Default)]
pub struct Canvas {
    art: Option<Art>,
    pending: Option<(f32, bool, Instant)>,
}

pub fn controller(ui: &mut egui::Ui, pad: &Pad, live: bool, canvas: &mut Canvas) {
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let painter = ui.painter_at(rect);
    let ctx = ui.ctx().clone();
    let ppp = ctx.pixels_per_point();
    let accent = if live { ACCENT } else { DIM };
    let line_col = if live { accent } else { LINE_IDLE };
    let on = |m: u32| pad.connected && pad.down(m);
    let mono = |size: f32| FontId::new(size, FontFamily::Monospace);

    // Leave room for the gauges and a margin so the reticle frames the body; cap the art at
    // ~900 px wide so a maximised window stays balanced against the side panel.
    // (0.05 < 900/1024, so the clamp bounds are always ordered.)
    let s =
        ((rect.width() * 0.82) / art::ART_W).min((rect.height() - 120.0) / art::ART_H).clamp(0.05, 900.0 / art::ART_W);
    let origin = rect.center() - Vec2::new(art::ART_W, art::ART_H) * s / 2.0 + Vec2::new(0.0, 20.0);
    let p = |(x, y): (f32, f32)| origin + Vec2::new(x, y) * s;

    // Raster resolution: never beyond the GPU's texture limit (or 4096 px), never absurdly small.
    let max_side = ctx.input(|i| i.max_texture_side).min(4096) as f32 - 4.0;
    let want = (s * ppp).min(max_side / art::ART_W).max(0.25);
    let stale = canvas.art.as_ref().is_none_or(|a| (a.scale / want - 1.0).abs() > 0.01 || a.live != live);
    if stale {
        let now = Instant::now();
        let same = canvas.pending.is_some_and(|(sc, l, _)| (sc / want - 1.0).abs() <= 0.01 && l == live);
        if !same {
            canvas.pending = Some((want, live, now));
        }
        let settled = canvas.pending.is_some_and(|(_, _, t)| now.duration_since(t) > Duration::from_millis(120));
        if canvas.art.is_none() || settled || canvas.art.as_ref().is_some_and(|a| a.live != live) {
            canvas.art = Some(Art::build(&ctx, want, live, ACCENT));
            canvas.pending = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(130));
        }
    }
    let Some(art) = canvas.art.as_ref() else { return };
    // Raster px -> screen points (also handles drawing a slightly stale raster while resizing).
    let f = s / art.scale;
    let art_rect = Rect::from_min_size(origin, Vec2::new(art.px[0] as f32, art.px[1] as f32) * f);
    let mask_rect = |m: &art::Mask, offset: Vec2| {
        Rect::from_min_size(
            origin + Vec2::new(m.min[0] as f32, m.min[1] as f32) * f + offset,
            Vec2::new(m.size[0] as f32, m.size[1] as f32) * f,
        )
    };
    let paint_mask = |m: &art::Mask, offset: Vec2, tint: Color32| {
        painter.image(m.tex.id(), mask_rect(m, offset), FULL_UV, tint);
    };

    // HUD reticle, sized to always fit inside the canvas.
    let hub = p((art::ART_W / 2.0, art::ART_H / 2.0));
    let rr = rect.width().min(rect.height()) * 0.46;
    painter.circle_stroke(hub, rr, Stroke::new(1.0_f32, alpha(accent, 16)));
    painter.circle_stroke(hub, rr * 0.82, Stroke::new(1.0_f32, alpha(accent, 8)));
    for i in 0..24 {
        let dir = Vec2::angled((i as f32 * 15.0).to_radians());
        let len = if i % 6 == 0 { 12.0 } else { 6.0 };
        painter.line_segment([hub + dir * rr, hub + dir * (rr - len)], Stroke::new(1.0_f32, alpha(accent, 40)));
    }

    // Trigger gauges, anchored to the bumper humps with a leader line.
    for (cx, v, label) in [(LB_X, pad.lt, "LT"), (RB_X, pad.rt, "RT")] {
        let v = if pad.connected { v } else { 0.0 };
        let bump = p((cx, BUMPER_TOP));
        let gauge_bottom = bump.y - 10.0 * s.max(0.5);
        let text_col = alpha(accent, if v > 0.0 { 255 } else { 150 });
        let text = format!("{label} {:>3.0}%", v * 100.0);
        painter.line_segment(
            [Pos2::new(bump.x, gauge_bottom + 2.0), Pos2::new(bump.x, bump.y)],
            Stroke::new(1.0_f32, alpha(accent, 90)),
        );
        if s < 0.40 {
            painter.text(Pos2::new(bump.x, gauge_bottom - 2.0), Align2::CENTER_BOTTOM, text, mono(10.0), text_col);
            continue;
        }
        let gap = 2.0;
        let compact = s < 0.6;
        // Compact: 10 short segments with the label to the left on the same row.
        let (n, seg_w, seg_h) = if compact {
            (10usize, (4.0 * s).max(3.0), 5.0)
        } else {
            let gw = (art::ART_W * s * 0.16).min(150.0);
            let n = ((gw + gap) / (3.0 + gap)).floor().clamp(4.0, 16.0) as usize;
            (n, (gw - gap * (n - 1) as f32) / n as f32, 8.0)
        };
        let gw = n as f32 * seg_w + (n - 1) as f32 * gap;
        let top = gauge_bottom - seg_h;
        for i in 0..n {
            let r = Rect::from_min_size(
                Pos2::new(bump.x - gw / 2.0 + i as f32 * (seg_w + gap), top),
                Vec2::new(seg_w, seg_h),
            );
            let lit = (i as f32 + 0.5) / n as f32 <= v;
            painter.rect_filled(r, 1.5, if lit { accent } else { alpha(accent, 35) });
        }
        if compact {
            painter.text(
                Pos2::new(bump.x - gw / 2.0 - 10.0, top + seg_h / 2.0),
                Align2::RIGHT_CENTER,
                text,
                mono(10.0),
                text_col,
            );
        } else {
            painter.text(Pos2::new(bump.x, top - 4.0), Align2::CENTER_BOTTOM, text, mono(10.0), text_col);
        }
    }

    // Body (shaded fill + halo).
    painter.image(art.body.id(), art_rect, FULL_UV, Color32::WHITE);

    // Pressed-state fills, using the drawing's own shapes.
    // Shape-following glow: the region's own mask, drawn slightly enlarged and faint.
    let fill = |r: Region, color: Color32| {
        if let Some(m) = art.region(r) {
            let rc = mask_rect(m, Vec2::ZERO);
            for (grow, a) in [(9.0, 22), (5.0, 45), (2.5, 80)] {
                painter.image(m.tex.id(), rc.expand(grow * s.max(0.5)), FULL_UV, alpha(color, a));
            }
            // Translucent so the drawing's glyphs (LB/RB labels, View/Menu/Share icons) still read.
            paint_mask(m, Vec2::ZERO, alpha(color, 150));
        }
    };
    for (r, m) in [
        (Region::Lb, pad::LB),
        (Region::Rb, pad::RB),
        (Region::View, pad::VIEW),
        (Region::Menu, pad::MENU),
        (Region::Share, pad::SHARE),
        (Region::Up, pad::DPAD_UP),
        (Region::Down, pad::DPAD_DOWN),
        (Region::Left, pad::DPAD_LEFT),
        (Region::Right, pad::DPAD_RIGHT),
    ] {
        if on(m) {
            fill(r, accent);
        }
    }
    let face_cols = [
        (pad::A, Color32::from_rgb(92, 220, 110)),
        (pad::B, Color32::from_rgb(240, 74, 84)),
        (pad::X, Color32::from_rgb(64, 150, 255)),
        (pad::Y, Color32::from_rgb(250, 200, 60)),
    ];
    for ((r, _, _), (bit, col)) in art.faces.iter().zip(face_cols) {
        if on(bit) {
            fill(*r, col);
        }
    }

    // Guide: a lit emblem ring (no logo) that glows while connected and floods white on press.
    let guide = p(art::GUIDE);
    let gr = art::GUIDE_R * s;
    let pressed_guide = on(pad::GUIDE);
    if pressed_guide {
        // Bright backlit flash, like the real button.
        if let Some(m) = art.region(Region::Guide) {
            let rc = mask_rect(m, Vec2::ZERO);
            painter.circle_stroke(
                rc.center(),
                rc.width() / 2.0 + 3.0 * s.max(0.6),
                Stroke::new(6.0 * s.max(0.6), alpha(Color32::WHITE, 70)),
            );
            paint_mask(m, Vec2::ZERO, alpha(Color32::WHITE, 235));
        }
    } else if live {
        // Backlit look: soft wide strokes around the emblem ring (filled glows read as grey discs).
        // Offset outward so the glow surrounds the ring without greying the button face.
        for (w, a) in [(9.0, 14), (5.0, 32), (2.5, 70)] {
            let w = w * s.max(0.6);
            painter.circle_stroke(guide, gr * 0.62 + w / 2.0, Stroke::new(w, alpha(Color32::WHITE, a)));
        }
    }
    let ring_col = if pressed_guide {
        BG
    } else if live {
        alpha(Color32::WHITE, 200)
    } else {
        LINE_IDLE
    };
    painter.circle_stroke(guide, gr * 0.62, Stroke::new(1.5 * s.max(0.7), ring_col));

    // Line work; A/B/X/Y keep their real colours (letters bright, rings softer).
    painter.image(art.lines.id(), art_rect, FULL_UV, line_col);
    for ((_, letter, ring), (bit, col)) in art.faces.iter().zip(face_cols) {
        let pressed = on(bit);
        if let Some(m) = ring {
            paint_mask(
                m,
                Vec2::ZERO,
                if pressed {
                    col
                } else if live {
                    alpha(col, 140)
                } else {
                    alpha(LINE_IDLE, 160)
                },
            );
        }
        if let Some(m) = letter {
            paint_mask(
                m,
                Vec2::ZERO,
                if pressed {
                    BG
                } else if live {
                    col
                } else {
                    alpha(col, 110)
                },
            );
        }
    }

    // Sticks: a faint gate at rest; the cap moves with the stick and a dot marks its centre.
    let travel = 14.0 * s;
    let sticks = [
        (pad.left, pad::LS, Region::LeftCap, art::LEFT_STICK),
        (pad.right, pad::RS, Region::RightCap, art::RIGHT_STICK),
    ];
    for (i, ((sx, sy), bit, region, centre)) in sticks.into_iter().enumerate() {
        let c = p(centre);
        let off = if pad.connected { Vec2::new(sx, -sy) * travel } else { Vec2::ZERO };
        painter.circle_stroke(c, travel + art::STICK_CAP_R * s * 0.35, Stroke::new(1.0_f32, alpha(accent, 50)));
        // The moved cap occludes the well beneath it, so stray arcs of the base don't show.
        if off.length() > 0.5 {
            painter.circle_filled(c + off, art::STICK_CAP_R * s + 4.0, Color32::from_rgb(21, 27, 35));
        }
        if on(bit)
            && let Some(m) = art.region(region)
        {
            let rc = mask_rect(m, off);
            for (grow, a) in [(6.0, 30), (3.0, 60)] {
                painter.image(m.tex.id(), rc.expand(grow * s.max(0.5)), FULL_UV, alpha(accent, a));
            }
            paint_mask(m, off, alpha(accent, 170));
        }
        if let Some(m) = &art.caps[i] {
            paint_mask(m, off, line_col);
        }
        if pad.connected {
            painter.circle_filled(c + off, 2.5 * s.max(0.8), alpha(if on(bit) { BG } else { accent }, 230));
        }
    }

    if !pad.connected {
        painter.text(
            Pos2::new(rect.center().x, art_rect.bottom() + 16.0),
            Align2::CENTER_CENTER,
            "NO INPUT SIGNAL",
            FontId::new(12.0, FontFamily::Monospace),
            alpha(MUTED, 220),
        );
    }
}
