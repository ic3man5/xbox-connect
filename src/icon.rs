//! Tray/window icons: the controller silhouette from the bundled line drawing, tinted.

pub const ACTIVE: [u8; 3] = [64, 232, 255];
pub const IDLE: [u8; 3] = [150, 156, 168];

pub fn rgba(size: u32, color: [u8; 3]) -> Vec<u8> {
    crate::art::silhouette(size).into_iter().flat_map(|a| [color[0], color[1], color[2], a]).collect()
}
