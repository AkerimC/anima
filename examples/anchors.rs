//! Marks what `anchors::find` sees on every sprite given, into one sheet:
//! cargo run --release --example anchors -- out.png sprite.png...
use anima::render::anchors;
use image::{Rgba, RgbaImage};

fn rect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4], t: i32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for y in y0 - t..=y1 + t {
        for x in x0 - t..=x1 + t {
            let edge = x < x0 + t || x > x1 - t || y < y0 + t || y > y1 - t;
            if edge && x >= 0 && y >= 0 && x < w && y < h {
                img.put_pixel(x as u32, y as u32, Rgba(c));
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, files) = args.split_first().expect("out.png sprites...");
    let cell: u32 = std::env::var("CELL").ok().and_then(|v| v.parse().ok()).unwrap_or(140);
    let cols: u32 = std::env::var("COLS").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
    let rows = (files.len() as u32).div_ceil(cols);
    let mut sheet = RgbaImage::from_pixel(cols * cell, rows * cell, Rgba([40, 40, 40, 255]));
    let (mut eyes, mut hands) = (0, 0);
    for (k, f) in files.iter().enumerate() {
        let Ok(img) = image::open(f) else { continue };
        let mut img = img.to_rgba8();
        let (w, h) = (img.width() as f32, img.height() as f32);
        let t = (img.height() / 200).max(2) as i32;
        if let Some(a) = anchors::find(&img) {
            eyes += usize::from(a.eyes_found);
            hands += usize::from(a.hands_found[0]) + usize::from(a.hands_found[1]);
            let px = |p: [f32; 2]| ((p[0] * w) as i32, (p[1] * h) as i32);
            rect(&mut img, (a.head[0] * w) as i32, (a.head[1] * h) as i32, (a.head[2] * w) as i32, (a.head[3] * h) as i32, [0, 255, 0, 255], t);
            let (ew, eh) = ((a.eye_size[0] * w * 0.5) as i32, (a.eye_size[1] * h * 0.5) as i32);
            for e in a.eyes {
                let (x, y) = px(e);
                let c = if a.eyes_found { [255, 0, 0, 255] } else { [255, 200, 0, 255] };
                rect(&mut img, x - ew, y - eh, x + ew, y + eh, c, t);
            }
            for (i, hnd) in a.hands.iter().enumerate() {
                let (x, y) = px(*hnd);
                let c = if a.hands_found[i] { [0, 160, 255, 255] } else { [255, 0, 255, 255] };
                let r = (w * 0.04) as i32;
                rect(&mut img, x - r, y - r, x + r, y + r, c, t);
            }
            for p in [a.back, a.neck, a.feet] {
                let (x, y) = px(p);
                rect(&mut img, x - 4, y - 4, x + 4, y + 4, [255, 255, 255, 255], t);
            }
        }
        let small = image::imageops::thumbnail(&img, cell * img.width() / img.height().max(1), cell);
        let (cx, cy) = ((k as u32 % cols) * cell, (k as u32 / cols) * cell);
        image::imageops::overlay(&mut sheet, &small, cx as i64, cy as i64);
    }
    sheet.save(out).unwrap();
    println!("{} sprites, eyes seen in {eyes}, hands seen {hands} of {}", files.len(), files.len() * 2);
}
