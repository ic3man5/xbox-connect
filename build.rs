//! Packs assets/xbox-series.svg into a compact binary (~25% of the SVG's size) so the exe
//! embeds only what the renderer needs and the app ships no SVG parser.
//!
//! Format: a stream of zigzag LEB128 varints. Contour count; then per contour: cubic count,
//! start x, start y (absolute), then per cubic the three points as deltas from the previous
//! on-curve point. Coordinates are in the SVG's path units with each path's translate applied.

use std::{env, fs, path::Path};

fn main() {
    let src = "assets/xbox-series.svg";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=build.rs");
    let svg = fs::read_to_string(src).expect("read svg");

    let mut contours: Vec<(i16, i16, Vec<[i16; 6]>)> = Vec::new();
    let mut rest = svg.as_str();
    while let Some(i) = rest.find("<path d=\"") {
        rest = &rest[i + 9..];
        let end = rest.find('"').expect("path end");
        let d = &rest[..end];
        rest = &rest[end..];
        let tag_end = rest.find("/>").unwrap_or(rest.len());
        let (mut tx, mut ty) = (0i32, 0i32);
        if let Some(t) = rest[..tag_end].find("transform=\"") {
            let attr = &rest[t + 11..];
            let attr = &attr[..attr.find('"').expect("transform end")];
            let args = attr
                .strip_prefix("translate(")
                .and_then(|a| a.strip_suffix(')'))
                .unwrap_or_else(|| panic!("unsupported transform {attr:?}: only translate() is handled"));
            let v: Vec<i32> = args
                .split([' ', ','])
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<f32>().unwrap().round() as i32)
                .collect();
            assert_eq!(v.len(), 2, "translate needs x and y");
            tx = v[0];
            ty = v[1];
        }

        let mut nums: Vec<i32> = Vec::new();
        let mut cmd = ' ';
        let mut tokens = Vec::new();
        let mut num = String::new();
        for ch in d.chars().chain(std::iter::once(' ')) {
            if ch.is_ascii_digit() || ch == '.' || (ch == '-' && num.is_empty()) {
                num.push(ch);
                continue;
            }
            if !num.is_empty() {
                tokens.push(Err(num.parse::<f32>().unwrap().round() as i32));
                num.clear();
            }
            if ch == '-' {
                num.push(ch);
            } else if ch.is_ascii_alphabetic() {
                tokens.push(Ok(ch));
            }
        }
        let mut cur: Option<(i16, i16, Vec<[i16; 6]>)> = None;
        let p = |v: i32, t: i32| i16::try_from(v + t).expect("coordinate fits i16");
        for tok in tokens {
            match tok {
                Ok(c) => {
                    cmd = c;
                    nums.clear();
                    if (c == 'Z' || c == 'z')
                        && let Some(c) = cur.take()
                    {
                        contours.push(c);
                    }
                }
                Err(v) => {
                    nums.push(v);
                    match cmd {
                        'M' if nums.len() == 2 => {
                            if let Some(c) = cur.take() {
                                contours.push(c);
                            }
                            cur = Some((p(nums[0], tx), p(nums[1], ty), Vec::new()));
                            nums.clear();
                            cmd = 'm'; // further pairs would be implicit line-tos
                        }
                        'm' => panic!("implicit line-to after M is not supported"),
                        'C' if nums.len() == 6 => {
                            let c = cur.as_mut().expect("C before M");
                            c.2.push([
                                p(nums[0], tx),
                                p(nums[1], ty),
                                p(nums[2], tx),
                                p(nums[3], ty),
                                p(nums[4], tx),
                                p(nums[5], ty),
                            ]);
                            nums.clear();
                        }
                        'M' | 'C' => {}
                        other => panic!("unsupported SVG command {other}"),
                    }
                }
            }
        }
        if let Some(c) = cur.take() {
            contours.push(c);
        }
    }

    fn varint(out: &mut Vec<u8>, v: i32) {
        let mut z = ((v << 1) ^ (v >> 31)) as u32; // zigzag
        loop {
            let byte = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    let mut out = Vec::new();
    varint(&mut out, i32::try_from(contours.len()).expect("contour count"));
    for (x, y, cubics) in &contours {
        varint(&mut out, i32::try_from(cubics.len()).expect("cubic count"));
        let (mut px, mut py) = (*x as i32, *y as i32);
        varint(&mut out, px);
        varint(&mut out, py);
        for c in cubics {
            for k in 0..3 {
                varint(&mut out, c[k * 2] as i32 - px);
                varint(&mut out, c[k * 2 + 1] as i32 - py);
            }
            (px, py) = (c[4] as i32, c[5] as i32);
        }
    }
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("art.bin"), out).expect("write art.bin");
}
