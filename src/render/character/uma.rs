//! Uma Musume chibi sprites (umamusu.wiki names): a calm "[A]"/_0010 pose,
//! a happy "[B]"/_0011 one next to it, both drawn looking right.

use std::path::{Path, PathBuf};

fn name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// Uma sprites look to the right (others are taken to look left).
pub(super) fn faces_right(path: &Path) -> bool {
    let n = name(path);
    n.contains("_0010.") || n.contains("_0011.")
}

/// The happy pose that goes with a calm one, shown while jumping or petted.
pub(super) fn happy_pose(path: &Path) -> Option<PathBuf> {
    let n = name(path);
    n.contains("_0010.")
        .then(|| path.with_file_name(n.replace("[A]", "[B]").replace("_0010.", "_0011.")))
}

/// Where the eyes are on a sprite, for closing them in its sleep: the left
/// and right eye as (x0, y0, x1, y1) in texture units, and the skin colour
/// between them. None when they can't be told apart from the hair.
pub(super) fn find_eyes(img: &image::RgbaImage) -> Option<([f32; 4], [f32; 4], [f32; 3])> {
    let (w, h) = img.dimensions();
    let (mut top, mut bot, mut left, mut right) = (h, 0, w, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 40 {
            top = top.min(y);
            bot = bot.max(y);
            left = left.min(x);
            right = right.max(x);
        }
    }
    if bot <= top + 20 || right <= left + 20 {
        return None;
    }
    let (hh, ww) = ((bot - top) as f32, (right - left) as f32);
    let hsv = |p: &image::Rgba<u8>| {
        let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
        let v = r.max(g).max(b);
        let s = if v > 0.0 { (v - r.min(g).min(b)) / v } else { 0.0 };
        (s, v)
    };
    let (y0, y1) = (top + (hh * 0.15) as u32, top + (hh * 0.6) as u32);
    let (x0, x1) = (left + (ww * 0.12) as u32, right - (ww * 0.12) as u32);
    let cols = (x1 - x0 + 1) as usize;
    let mut white = vec![false; cols * (y1 - y0 + 1) as usize];
    let mut iris = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            let p = img.get_pixel(x, y);
            if p[3] < 220 {
                continue;
            }
            let (s, v) = hsv(p);
            if v < 0.6 && s > 0.3 {
                iris.push((x, y));
            } else if v > 0.93 && s < 0.12 {
                white[(y - y0) as usize * cols + (x - x0) as usize] = true;
            }
        }
    }
    // iris next to a white sparkle: an eye, not hair
    let rad = ((ww * 0.04) as i32).max(3);
    let near_white = |x: u32, y: u32| {
        (-rad..=rad).step_by(2).any(|dy| {
            (-rad..=rad).step_by(2).any(|dx| {
                let (px, py) = (x as i32 + dx, y as i32 + dy);
                px >= x0 as i32 && px <= x1 as i32 && py >= y0 as i32 && py <= y1 as i32 && white[(py - y0 as i32) as usize * cols + (px - x0 as i32) as usize]
            })
        })
    };
    let good: Vec<(u32, u32)> = iris.into_iter().filter(|&(x, y)| near_white(x, y)).collect();
    if good.len() < 20 {
        return None;
    }
    let mut rows = vec![0u32; (y1 - y0 + 1) as usize];
    for &(_, y) in &good {
        rows[(y - y0) as usize] += 1;
    }
    let near = |k: usize| (k.saturating_sub(5)..(k + 6).min(rows.len())).map(|j| rows[j]).sum::<u32>();
    let best = (0..rows.len()).max_by_key(|&k| near(k))? as u32 + y0;
    let spread = (hh * 0.06) as u32;
    let mid = (left + right) as f32 * 0.5;
    let boxed = |side: bool| -> Option<[f32; 4]> {
        let mut xs: Vec<u32> = Vec::new();
        let mut ys: Vec<u32> = Vec::new();
        for &(x, y) in &good {
            if y.abs_diff(best) <= spread && ((x as f32) < mid) == side {
                xs.push(x);
                ys.push(y);
            }
        }
        if xs.len() < 6 {
            return None;
        }
        xs.sort_unstable();
        ys.sort_unstable();
        let k = xs.len() / 10;
        Some([xs[k] as f32 / w as f32, ys[k] as f32 / h as f32, xs[xs.len() - 1 - k] as f32 / w as f32, ys[ys.len() - 1 - k] as f32 / h as f32])
    };
    let (l, r) = (boxed(true)?, boxed(false)?);
    // eyes sit in the upper middle of a chibi, and neither is huge
    let at = (best - top) as f32 / hh;
    let wide = |b: [f32; 4]| (b[2] - b[0]) * w as f32 / ww;
    if !(0.3..=0.58).contains(&at) || wide(l) > 0.28 || wide(r) > 0.28 || l[2] >= r[0] {
        return None;
    }
    // the skin between and just under the eyes
    let sy = ((l[3].max(r[3]) * h as f32) as u32 + 2).min(h - 1);
    let (sx0, sx1) = ((l[2] * w as f32) as u32, (r[0] * w as f32) as u32);
    let mut skin = Vec::new();
    for x in sx0..=sx1 {
        let p = img.get_pixel(x, sy);
        let (s, v) = hsv(p);
        if p[3] > 220 && v > 0.75 && s < 0.4 {
            skin.push([p[0] as f32, p[1] as f32, p[2] as f32]);
        }
    }
    if skin.len() < 2 {
        return None;
    }
    let n = skin.len() as f32;
    let sum = skin.iter().fold([0.0; 3], |a, c| [a[0] + c[0], a[1] + c[1], a[2] + c[2]]);
    Some((l, r, [sum[0] / n / 255.0, sum[1] / n / 255.0, sum[2] / n / 255.0]))
}

/// Where a chibi's eyes usually are, when they can't be found: a little
/// below the middle of the head, either side of the centre.
pub(super) fn guess_eyes(img: &image::RgbaImage) -> Option<([f32; 4], [f32; 4], [f32; 3])> {
    let (w, h) = img.dimensions();
    let (mut top, mut bot, mut left, mut right) = (h, 0, w, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 40 {
            top = top.min(y);
            bot = bot.max(y);
            left = left.min(x);
            right = right.max(x);
        }
    }
    if bot <= top || right <= left {
        return None;
    }
    let (hh, ww) = ((bot - top) as f32, (right - left) as f32);
    let (cx, cy) = ((left + right) as f32 * 0.5, top as f32 + hh * 0.43);
    let (ew, eh) = (ww * 0.13, hh * 0.05);
    let b = |x: f32| [(x - ew * 0.5) / w as f32, (cy - eh * 0.5) / h as f32, (x + ew * 0.5) / w as f32, (cy + eh * 0.5) / h as f32];
    Some((b(cx - ww * 0.12), b(cx + ww * 0.12), [1.0, 0.9, 0.85]))
}

/// Eyes for the sleep mask: found ones if they're about where a chibi's eyes
/// should be, else that usual place, so the mask never ends up on the hair.
pub(super) fn mask_eyes(img: &image::RgbaImage) -> Option<([f32; 4], [f32; 4], [f32; 3])> {
    let guess = guess_eyes(img)?;
    let mid = |e: &([f32; 4], [f32; 4], [f32; 3])| ((e.0[0] + e.1[2]) * 0.5, (e.0[1] + e.0[3] + e.1[1] + e.1[3]) * 0.25);
    let (gx, gy) = mid(&guess);
    let gw = guess.1[2] - guess.0[0];
    match find_eyes(img) {
        Some(f) => {
            let (fx, fy) = mid(&f);
            let fw = f.1[2] - f.0[0];
            let near = (fx - gx).abs() < gw * 0.3 && (fy - gy).abs() < (guess.0[3] - guess.0[1]) * 2.0 && fw < gw * 1.4 && fw > gw * 0.5;
            Some(if near { f } else { guess })
        }
        None => Some(guess),
    }
}
