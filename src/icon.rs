//! Tray/window icons: the controller silhouette from the bundled line drawing, tinted, on a dark
//! rounded badge with a coloured rim, so it reads on light and dark taskbars alike.

pub const ACTIVE: [u8; 3] = [64, 232, 255];
pub const IDLE: [u8; 3] = [150, 156, 168];

const BADGE: [u8; 3] = [14, 19, 27];

pub fn rgba(size: u32, color: [u8; 3]) -> Vec<u8> {
    let s = size as usize;
    let radius = size as f32 * 0.24;
    let rim = (size as f32 / 16.0).max(1.0); // ~2 px at 32 px, so it survives scaling to 16 px
    // The controller fills the badge but always keeps at least 1 px of dark badge between it and
    // the rim, or at tray sizes the two merge into one blob. (It's wider than tall, so it sits centred.)
    let inner = size - 2 * (rim.ceil() as u32 + 1);
    let glyph = crate::art::silhouette(inner);
    let pad = (s - inner as usize) / 2;

    let mut out = Vec::with_capacity(s * s * 4);
    for y in 0..s {
        for x in 0..s {
            // 4x4 supersampled coverage of the rounded square and of its inner (rim-less) area.
            let (mut body, mut core) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (px, py) = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                    body += inside(px, py, 0.0, size as f32, radius) as u32;
                    core += inside(px, py, rim, size as f32 - rim, (radius - rim).max(0.0)) as u32;
                }
            }
            let body = body as f32 / 16.0;
            let core = core as f32 / 16.0;
            // Rim in the status colour, dark fill inside it, controller on top.
            let g = if x >= pad && y >= pad && x - pad < inner as usize && y - pad < inner as usize {
                glyph[(y - pad) * inner as usize + (x - pad)] as f32 / 255.0
            } else {
                0.0
            };
            let mix = |c: usize| {
                let base = BADGE[c] as f32 * core + color[c] as f32 * (body - core).max(0.0);
                let base = if body > 0.0 { base / body } else { 0.0 };
                (base * (1.0 - g) + color[c] as f32 * g).round() as u8
            };
            out.extend_from_slice(&[mix(0), mix(1), mix(2), (body.max(g) * 255.0).round() as u8]);
        }
    }
    out
}

/// Is (x, y) inside the square [lo, hi]² with corners rounded by `r`?
fn inside(x: f32, y: f32, lo: f32, hi: f32, r: f32) -> bool {
    if x < lo || y < lo || x > hi || y > hi {
        return false;
    }
    let cx = x.clamp(lo + r, hi - r);
    let cy = y.clamp(lo + r, hi - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

#[cfg(test)]
mod tests {
    /// `XC_ICON_DUMP=dir cargo test dump_icons` writes raw RGBA previews for eyeballing.
    #[test]
    fn dump_icons() {
        let Some(dir) = std::env::var_os("XC_ICON_DUMP") else { return };
        for (name, c) in [("active", super::ACTIVE), ("idle", super::IDLE)] {
            for size in [16u32, 20, 32] {
                let path = std::path::Path::new(&dir).join(format!("{name}-{size}.rgba"));
                std::fs::write(path, super::rgba(size, c)).unwrap();
            }
        }
    }
}
