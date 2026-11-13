//! Now and then one stands still and holds up a little cardboard sign with
//! something sweet on it ("Pet me!"). The sign is a picture made once, on
//! first use, from a system font.

use std::path::PathBuf;

pub const SIGNS: [&str; 8] = ["Pet me!", "Pat pat?", "Headpats\nplease", "Hi there!", "Play with\nme!", "Cheer\nup!", "Snack\ntime?", "Hug?"];

fn font() -> Option<ab_glyph::FontVec> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans:bold"]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    ab_glyph::FontVec::try_from_vec(std::fs::read(path.trim()).ok()?).ok()
}

/// The picture of sign `k`, made if it isn't there yet.
pub fn picture(k: usize) -> Option<PathBuf> {
    let path = crate::util::data_dir().join("props/signs").join(format!("sign{}_v2.png", k % SIGNS.len()));
    if !path.is_file() {
        let _ = std::fs::create_dir_all(path.parent()?);
        make(SIGNS[k % SIGNS.len()], &font()?).save(&path).ok()?;
    }
    Some(path)
}

fn make(text: &str, font: &ab_glyph::FontVec) -> image::RgbaImage {
    use crate::render::text::{rasterize, HAlign, VAlign};
    let (w, h) = (420u32, 300u32);
    let mut img = image::RgbaImage::new(w, h);
    let (board, edge, ink, heart) = ([255, 248, 230], [150, 100, 64], [170, 30, 75], [240, 110, 140]);
    // a rounded board with a brown rim
    let r = 34.0f32;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let qx = (fx - w as f32 * 0.5).abs() - (w as f32 * 0.5 - r - 4.0);
            let qy = (fy - h as f32 * 0.5).abs() - (h as f32 * 0.5 - r - 4.0);
            let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
            if d < 0.0 {
                let c = if d > -8.0 { edge } else { board };
                let a = (-d).min(1.0);
                img.put_pixel(x, y, image::Rgba([c[0], c[1], c[2], (a * 255.0) as u8]));
            }
        }
    }
    // little hearts in two corners
    for (cx, cy, s) in [(30.0f32, 30.0f32, 11.0f32), (w as f32 - 30.0, h as f32 - 30.0, 11.0)] {
        for y in (cy - s * 1.6) as u32..(cy + s * 1.6) as u32 {
            for x in (cx - s * 1.6) as u32..(cx + s * 1.6) as u32 {
                let (u, v) = ((x as f32 - cx) / s, -(y as f32 - cy) / s + 0.2);
                if (u * u + v * v - 1.0).powi(3) - u * u * v * v * v <= 0.0 {
                    img.put_pixel(x, y, image::Rgba([heart[0], heart[1], heart[2], 255]));
                }
            }
        }
    }
    let lines = text.lines().count().max(1) as f32;
    let px = if lines > 1.0 { 112.0 } else { 140.0 };
    let t = rasterize(font, text, px, 0.0, [0.0, -14.0], HAlign::Center, VAlign::Center, None, 0, Some([w - 24, h - 20]));
    let (ox, oy) = (12, 10);
    for y in 0..t.height.min(h - oy) {
        for x in 0..t.width.min(w - ox) {
            let a = t.pixels[((y * t.width + x) * 4 + 3) as usize] as f32 / 255.0;
            if a > 0.0 {
                let p = img.get_pixel_mut(x + ox, y + oy);
                for c in 0..3 {
                    p[c] = (p[c] as f32 * (1.0 - a) + ink[c] as f32 * a) as u8;
                }
            }
        }
    }
    img
}
