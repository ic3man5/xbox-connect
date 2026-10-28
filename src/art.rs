//! Renders the bundled CC0 line drawing of an Xbox Series controller
//! (assets/xbox-series.svg, from "Gamepad Line Art Pack" by gidedin) into egui textures.
//!
//! build.rs packs the SVG's curves into compact varint deltas; here a small scanline rasteriser turns
//! them into textures (no SVG/image dependencies). From the line
//! raster we flood-fill the drawing's own enclosed areas to get exact masks for the body,
//! grip panels, bumpers, buttons and D-pad arms, so highlights match the artwork.

use std::sync::OnceLock;

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};

static ART: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/art.bin"));

/// Design space = the pack's 1024-px-wide PNG coordinates.
pub const ART_W: f32 = 1024.0;
pub const ART_H: f32 = 693.0;
const VIEW_X: f32 = 59.0;
const VIEW_Y: f32 = 20.0;
const VIEW_W: f32 = 1417.0;

/// Feature centres and radii, measured on the 1024-px raster of the drawing.
pub const LEFT_STICK: (f32, f32) = (243.0, 208.5);
pub const RIGHT_STICK: (f32, f32) = (644.0, 368.5);
pub const STICK_CAP_R: f32 = 62.0;
pub const DPAD: (f32, f32) = (373.5, 366.5);
pub const GUIDE: (f32, f32) = (511.5, 118.5);
pub const GUIDE_R: f32 = 41.5;
pub const VIEW: (f32, f32) = (448.0, 213.5);
pub const MENU: (f32, f32) = (575.5, 213.5);
pub const SHARE: (f32, f32) = (512.0, 268.0);
pub const FACE_R: f32 = 32.5;
pub const A: (f32, f32) = (771.5, 285.5);
pub const B: (f32, f32) = (839.0, 221.5);
pub const X: (f32, f32) = (704.5, 221.5);
pub const Y: (f32, f32) = (771.5, 153.0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Region {
    Lb,
    Rb,
    Guide,
    View,
    Menu,
    Share,
    A,
    B,
    X,
    Y,
    Up,
    Down,
    Left,
    Right,
    LeftCap,
    RightCap,
    GripL,
    GripR,
}

/// Seed point inside each enclosed area (design px), chosen clear of glyph ink.
fn seed(r: Region) -> (f32, f32) {
    let face = |c: (f32, f32)| (c.0 - 24.0, c.1);
    match r {
        Region::Lb => (300.0, 32.0),
        Region::Rb => (724.0, 32.0),
        Region::Guide => GUIDE,
        Region::View => (VIEW.0, VIEW.1 + 15.0),
        Region::Menu => (MENU.0, MENU.1 + 15.0),
        Region::Share => (SHARE.0 - 26.0, SHARE.1),
        Region::A => face(self::A),
        Region::B => face(self::B),
        Region::X => face(self::X),
        Region::Y => face(self::Y),
        Region::Up | Region::Down | Region::Left | Region::Right => DPAD,
        Region::LeftCap => LEFT_STICK,
        Region::RightCap => RIGHT_STICK,
        Region::GripL => (55.0, 560.0),
        Region::GripR => (ART_W - 55.0, 560.0),
    }
}

const REGIONS: [Region; 18] = [
    Region::Lb,
    Region::Rb,
    Region::Guide,
    Region::View,
    Region::Menu,
    Region::Share,
    Region::A,
    Region::B,
    Region::X,
    Region::Y,
    Region::Up,
    Region::Down,
    Region::Left,
    Region::Right,
    Region::LeftCap,
    Region::RightCap,
    Region::GripL,
    Region::GripR,
];

type Contour = Vec<(f32, f32)>;

/// Contours decoded from the build-time packed drawing (see build.rs), flattened, in design px.
fn contours() -> &'static [Contour] {
    static C: OnceLock<Vec<Contour>> = OnceLock::new();
    C.get_or_init(|| {
        let k = ART_W / VIEW_W;
        let map = |x: f32, y: f32| ((x * 0.25 - VIEW_X) * k, (y * 0.25 - VIEW_Y) * k);
        // Zigzag LEB128 varints (see build.rs).
        let mut bytes = ART.iter().copied();
        let mut next_int = || {
            let (mut z, mut shift) = (0u32, 0);
            for b in bytes.by_ref() {
                if shift < 32 {
                    z |= ((b & 0x7f) as u32) << shift;
                }
                shift += 7;
                if b & 0x80 == 0 {
                    break;
                }
            }
            ((z >> 1) as i32) ^ -((z & 1) as i32)
        };
        // Counts are clamped to the data size so malformed data can't loop or allocate unboundedly.
        let count = (next_int().max(0) as usize).min(ART.len());
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let cubics = next_int().clamp(0, ART.len() as i32);
            let mut pos = (next_int() as f32, next_int() as f32);
            let mut cur = vec![map(pos.0, pos.1)];
            for _ in 0..cubics {
                let mut rel = || (pos.0 + next_int() as f32, pos.1 + next_int() as f32);
                let (p1, p2, p3) = (rel(), rel(), rel());
                let len = (p3.0 - pos.0).abs() + (p3.1 - pos.1).abs();
                let steps = ((len / 24.0).ceil() as usize).clamp(1, 12);
                for s in 1..=steps {
                    let t = s as f32 / steps as f32;
                    let u = 1.0 - t;
                    let (a, b, c, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    cur.push(map(
                        a * pos.0 + b * p1.0 + c * p2.0 + e * p3.0,
                        a * pos.1 + b * p1.1 + c * p2.1 + e * p3.1,
                    ));
                }
                pos = p3;
            }
            if cur.len() > 2 {
                out.push(cur);
            }
        }
        out
    })
}
/// Nonzero-winding coverage raster (0..=255) at `scale` px per design px. 4x4 supersampling.
pub fn rasterize(scale: f32, w: usize, h: usize) -> Vec<u8> {
    const SS: usize = 4;
    struct Edge {
        y0: f32,
        y1: f32,
        x0: f32,
        dxdy: f32,
        dir: i32,
    }
    let mut edges: Vec<Edge> = Vec::new();
    for c in contours() {
        for i in 0..c.len() {
            let (a, b) = (c[i], c[(i + 1) % c.len()]);
            let (ax, ay, bx, by) =
                (a.0 * scale * SS as f32, a.1 * scale * SS as f32, b.0 * scale * SS as f32, b.1 * scale * SS as f32);
            if (ay - by).abs() < 1e-6 {
                continue;
            }
            let (dir, (x0, y0), (x1, y1)) = if ay < by { (1, (ax, ay), (bx, by)) } else { (-1, (bx, by), (ax, ay)) };
            edges.push(Edge { y0, y1, x0, dxdy: (x1 - x0) / (y1 - y0), dir });
        }
    }
    edges.sort_by(|a, b| a.y0.total_cmp(&b.y0));

    let mut acc = vec![0u16; w * h];
    let mut active: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut xs: Vec<(f32, i32)> = Vec::new();
    for sy in 0..h * SS {
        let y = sy as f32 + 0.5;
        while next < edges.len() && edges[next].y0 <= y {
            active.push(next);
            next += 1;
        }
        active.retain(|&e| edges[e].y1 > y);
        xs.clear();
        for &e in &active {
            let ed = &edges[e];
            if ed.y0 <= y {
                xs.push((ed.x0 + (y - ed.y0) * ed.dxdy, ed.dir));
            }
        }
        xs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let row = (sy / SS) * w;
        let mut wind = 0;
        for k in 0..xs.len() {
            let before = wind;
            wind += xs[k].1;
            if before == 0 && wind != 0 {
                let start = xs[k].0;
                // Find where winding returns to zero.
                let mut w2 = wind;
                let mut j = k + 1;
                while j < xs.len() {
                    w2 += xs[j].1;
                    if w2 == 0 {
                        break;
                    }
                    j += 1;
                }
                let end = if j < xs.len() { xs[j].0 } else { (w * SS) as f32 };
                let (c0, c1) = ((start.ceil().max(0.0)) as usize, (end.ceil().max(0.0) as usize).min(w * SS));
                for c in c0..c1 {
                    acc[row + c / SS] += 1;
                }
            }
        }
    }
    acc.iter().map(|&v| ((v as u32 * 255) / (SS * SS) as u32).min(255) as u8).collect()
}

/// 4-connected flood fill over pixels whose line coverage is below 50%.
fn flood(lines: &[u8], w: usize, h: usize, seeds: &[(usize, usize)]) -> Vec<bool> {
    let mut m = vec![false; w * h];
    let mut stack: Vec<usize> = Vec::new();
    for &(x, y) in seeds {
        if x < w && y < h {
            // Nudge the seed off any ink.
            let found = (0..6i32)
                .flat_map(|r| (-r..=r).flat_map(move |dy| (-r..=r).map(move |dx| (dx, dy))))
                .map(|(dx, dy)| (x as i32 + dx, y as i32 + dy))
                .find(|&(px, py)| {
                    px >= 0
                        && py >= 0
                        && (px as usize) < w
                        && (py as usize) < h
                        && lines[py as usize * w + px as usize] < 128
                });
            if let Some((px, py)) = found {
                let i = py as usize * w + px as usize;
                if !m[i] {
                    m[i] = true;
                    stack.push(i);
                }
            }
        }
    }
    // Mark on push so the stack never holds a pixel twice (bounded by w*h).
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let mut visit = |j: usize, stack: &mut Vec<usize>| {
            if !m[j] && lines[j] < 128 {
                m[j] = true;
                stack.push(j);
            }
        };
        if x > 0 {
            visit(i - 1, &mut stack);
        }
        if x + 1 < w {
            visit(i + 1, &mut stack);
        }
        if y > 0 {
            visit(i - w, &mut stack);
        }
        if y + 1 < h {
            visit(i + w, &mut stack);
        }
    }
    m
}

/// Separable box blur of a 0..1 field, radius `r` px, two passes (≈ gaussian).
fn blur(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut a = src.to_vec();
    let mut b = vec![0.0f32; w * h];
    for _ in 0..2 {
        for y in 0..h {
            let mut sum = 0.0;
            for x in 0..w + r {
                if x < w {
                    sum += a[y * w + x];
                }
                if x > 2 * r && x - 2 * r - 1 < w {
                    sum -= a[y * w + x - 2 * r - 1];
                }
                if x >= r && x - r < w {
                    b[y * w + x - r] = sum / (2 * r + 1) as f32;
                }
            }
        }
        for x in 0..w {
            let mut sum = 0.0;
            for y in 0..h + r {
                if y < h {
                    sum += b[y * w + x];
                }
                if y > 2 * r && y - 2 * r - 1 < h {
                    sum -= b[(y - 2 * r - 1) * w + x];
                }
                if y >= r && y - r < h {
                    a[(y - r) * w + x] = sum / (2 * r + 1) as f32;
                }
            }
        }
    }
    a
}

/// A cropped alpha mask uploaded as a white texture; tint it when painting.
pub struct Mask {
    pub tex: TextureHandle,
    /// Top-left in raster px.
    pub min: [usize; 2],
    pub size: [usize; 2],
}

pub struct Art {
    pub px: [usize; 2],
    pub scale: f32,
    pub live: bool,
    pub body: TextureHandle,
    pub lines: TextureHandle,
    /// Per face button: (region, letter glyph, ring outline) so each can take its own colour.
    pub faces: [(Region, Option<Mask>, Option<Mask>); 4],
    pub caps: [Option<Mask>; 2],
    pub regions: Vec<(Region, Mask)>,
}

fn upload_mask(ctx: &Context, name: &str, alpha: &dyn Fn(usize, usize) -> u8, w: usize, h: usize) -> Option<Mask> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if alpha(x, y) > 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 {
        return None;
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut rgba = Vec::with_capacity(cw * ch * 4);
    for y in y0..=y1 {
        for x in x0..=x1 {
            rgba.extend_from_slice(&[255, 255, 255, alpha(x, y)]);
        }
    }
    let img = ColorImage::from_rgba_unmultiplied([cw, ch], &rgba);
    Some(Mask { tex: ctx.load_texture(name, img, TextureOptions::LINEAR), min: [x0, y0], size: [cw, ch] })
}

impl Art {
    /// Builds all textures for a raster of `scale` px per design px.
    pub fn build(ctx: &Context, scale: f32, live: bool, accent: Color32) -> Art {
        let (w, h) = ((ART_W * scale).ceil() as usize + 2, (ART_H * scale).ceil() as usize + 2);
        let lines = rasterize(scale, w, h);
        let at = |p: (f32, f32)| ((p.0 * scale) as usize, (p.1 * scale) as usize);
        let dist = |x: usize, y: usize, c: (f32, f32)| {
            ((x as f32 - c.0 * scale).powi(2) + (y as f32 - c.1 * scale).powi(2)).sqrt()
        };

        // Outside = everything reachable from the border; body = the rest.
        let mut border = Vec::new();
        for x in 0..w {
            border.push((x, 0));
            border.push((x, h - 1));
        }
        for y in 0..h {
            border.push((0, y));
            border.push((w - 1, y));
        }
        let outside = flood(&lines, w, h, &border);
        let grips = flood(&lines, w, h, &[at(seed(Region::GripL)), at(seed(Region::GripR))]);

        // Body texture: top-lit gradient, darker grip panels, neon halo outside.
        let body_f: Vec<f32> = outside.iter().map(|&o| if o { 0.0 } else { 1.0 }).collect();
        let halo = blur(&body_f, w, h, ((10.0 * scale).max(3.0)) as usize);
        // Soft top light centred on the guide button, falling off toward the grips.
        let light = Color32::from_rgb(30, 38, 48);
        let dark = Color32::from_rgb(14, 18, 24);
        let reach = ART_W * 0.62 * scale;
        let rim = (2.0 * scale.max(0.5)).round() as usize;
        let (halo_col, halo_max) = if live { (accent, 150.0) } else { (crate::theme::DIM, 45.0) };
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let px = &mut rgba[i * 4..i * 4 + 4];
                if !outside[i] {
                    let t = (dist(x, y, GUIDE) / reach).clamp(0.0, 1.0).powf(1.2);
                    let mut c = crate::theme::lerp(light, dark, t);
                    if grips[i] {
                        c = crate::theme::lerp(c, Color32::BLACK, 0.35);
                    }
                    // Thin highlight just inside the top edge.
                    if y >= rim && outside[i - rim * w] {
                        c = crate::theme::lerp(c, Color32::WHITE, 0.05);
                    }
                    px.copy_from_slice(&[c.r(), c.g(), c.b(), 255]);
                } else {
                    let a = (halo[i] * halo_max) as u8;
                    px.copy_from_slice(&[halo_col.r(), halo_col.g(), halo_col.b(), a]);
                }
            }
        }
        let body =
            ctx.load_texture("art-body", ColorImage::from_rgba_unmultiplied([w, h], &rgba), TextureOptions::LINEAR);

        // Pull the stick caps and the A/B/X/Y letters out of the line layer so they can move / be coloured.
        let cap_r = (STICK_CAP_R + 2.0) * scale;
        let letter_r = (FACE_R - 6.0) * scale;
        let ring_r = (FACE_R + 4.0) * scale;
        let in_cap = |x: usize, y: usize| dist(x, y, LEFT_STICK) <= cap_r || dist(x, y, RIGHT_STICK) <= cap_r;
        let in_face = |x: usize, y: usize| [A, B, X, Y].iter().any(|&c| dist(x, y, c) <= ring_r);
        let mut line_rgba = vec![0u8; w * h * 4];
        for i in 0..w * h {
            let (x, y) = (i % w, i / w);
            if !in_cap(x, y) && !in_face(x, y) {
                line_rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, lines[i]]);
            }
        }
        let lines_tex = ctx.load_texture(
            "art-lines",
            ColorImage::from_rgba_unmultiplied([w, h], &line_rgba),
            TextureOptions::LINEAR,
        );

        // Masks can come back empty at tiny scales; the drawing then just omits them.
        let caps = [LEFT_STICK, RIGHT_STICK].map(|c| {
            upload_mask(ctx, "art-cap", &|x, y| if dist(x, y, c) <= cap_r { lines[y * w + x] } else { 0 }, w, h)
        });
        let faces = [(Region::A, A), (Region::B, B), (Region::X, X), (Region::Y, Y)].map(|(r, c)| {
            let letter = upload_mask(
                ctx,
                "art-letter",
                &|x, y| if dist(x, y, c) <= letter_r { lines[y * w + x] } else { 0 },
                w,
                h,
            );
            let ring = upload_mask(
                ctx,
                "art-ring",
                &|x, y| {
                    let d = dist(x, y, c);
                    if d > letter_r && d <= ring_r { lines[y * w + x] } else { 0 }
                },
                w,
                h,
            );
            (r, letter, ring)
        });

        // Highlight regions from the drawing's own enclosed areas.
        let mut regions = Vec::new();
        let dpad = flood(&lines, w, h, &[at(DPAD)]);
        for r in REGIONS {
            let mask: Vec<bool> = match r {
                Region::Up | Region::Down | Region::Left | Region::Right => {
                    let (cx, cy) = (DPAD.0 * scale, DPAD.1 * scale);
                    (0..w * h)
                        .map(|i| {
                            let (dx, dy) = ((i % w) as f32 - cx, (i / w) as f32 - cy);
                            dpad[i]
                                && match r {
                                    Region::Up => dy < -dx.abs(),
                                    Region::Down => dy > dx.abs(),
                                    Region::Left => dx < -dy.abs(),
                                    _ => dx > dy.abs(),
                                }
                        })
                        .collect()
                }
                Region::GripL | Region::GripR => continue,
                _ => flood(&lines, w, h, &[at(seed(r))]),
            };
            // Guard against a seed leaking into the whole body.
            let count = mask.iter().filter(|&&b| b).count();
            if count == 0 || count > w * h / 6 {
                continue;
            }
            if let Some(m) = upload_mask(ctx, "art-region", &|x, y| if mask[y * w + x] { 255 } else { 0 }, w, h) {
                regions.push((r, m));
            }
        }

        Art { px: [w, h], scale, live, body, lines: lines_tex, faces, caps, regions }
    }

    pub fn region(&self, r: Region) -> Option<&Mask> {
        self.regions.iter().find(|(k, _)| *k == r).map(|(_, m)| m)
    }
}

/// Silhouette alpha for icons, `size`×`size`, from a 4x raster of the drawing.
pub fn silhouette(size: u32) -> Vec<u8> {
    let big = size as usize * 4;
    let scale = big as f32 / ART_W;
    let (w, h) = (big, big);
    let lines = rasterize(scale, w, h);
    let mut border = Vec::new();
    for x in 0..w {
        border.push((x, 0));
        border.push((x, h - 1));
    }
    for y in 0..h {
        border.push((0, y));
        border.push((w - 1, y));
    }
    let outside = flood(&lines, w, h, &border);
    // Centre the shorter drawing vertically in the square.
    let off = ((ART_W - ART_H) / 2.0 * scale) as isize;
    let s = size as usize;
    let mut out = vec![0u8; s * s];
    for y in 0..s {
        for x in 0..s {
            let mut n = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (bx, by) = (x * 4 + sx, (y * 4 + sy) as isize - off);
                    if by >= 0 && (by as usize) < h && !outside[by as usize * w + bx] {
                        n += 1;
                    }
                }
            }
            out[y * s + x] = (n * 255 / 16) as u8;
        }
    }
    out
}
