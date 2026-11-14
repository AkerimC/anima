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

/// True when the sibling happy pose is drawn facing the opposite way from the calm sprite
/// (all Uma Musume `[A]` -> `[B]` sprites face left in `[B]` and right in `[A]`).
pub(super) fn happy_opposite(path: &Path) -> bool {
    happy_pose(path).is_some()
}

/// Where the eyes are, for the sleep mask: the left and right eye as
/// (x0, y0, x1, y1) in texture units. The head is found from the outline
/// first (a chibi's head is wide, its neck narrow, ears too thin to count),
/// then the eyes inside it: dark iris next to a white sparkle. Where they
/// can't be made out, the place a chibi's eyes usually are in its head.
#[allow(dead_code)]
pub(super) fn mask_eyes(img: &image::RgbaImage) -> Option<([f32; 4], [f32; 4], [f32; 3])> {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let solid = |x: u32, y: u32| img.get_pixel(x, y)[3] > 40;
    let rows: Vec<u32> = (0..h).map(|y| (0..w).filter(|&x| solid(x, y)).count() as u32).collect();
    let top = rows.iter().position(|&c| c > 0)?;
    let bot = rows.iter().rposition(|&c| c > 0)?;
    if bot <= top + 20 {
        return None;
    }
    let hh_all = (bot - top) as f32;
    let smooth: Vec<f32> = (0..hu).map(|y| (y.saturating_sub(3)..(y + 4).min(hu)).map(|k| rows[k] as f32).sum::<f32>() / 7.0).collect();
    let upper = smooth[top..top + (hh_all * 0.5) as usize].iter().copied().fold(0.0, f32::max);
    // the head: from where the outline gets wide (below the ear tips) down to the neck
    let htop = (top..bot).find(|&y| smooth[y] >= upper * 0.45)?;
    let (lo, hi) = (top + (hh_all * 0.38) as usize, top + (hh_all * 0.62) as usize);
    let mut neck = (lo..hi).min_by(|&a, &b| smooth[a].total_cmp(&smooth[b]))?;
    if ((neck - htop) as f32) < hh_all * 0.32 {
        neck = htop + (hh_all * 0.4) as usize;
    }
    let head_rows = (neck - htop).max(1) as u32;
    let cols: Vec<usize> = (0..wu).filter(|&x| (htop..neck).filter(|&y| solid(x as u32, y as u32)).count() as u32 * 10 >= head_rows * 4).collect();
    let (hl, hr) = (*cols.first()?, *cols.last()?);
    let (hw, hh) = ((hr - hl) as f32, (neck - htop) as f32);
    let cx = (hl + hr) as f32 * 0.5;
    // the eyes, looked for only in the middle band of the head
    let (y0, y1) = (htop + (hh * 0.38) as usize, (htop + (hh * 0.85) as usize).min(hu - 1));
    let (x0, x1) = (hl + (hw * 0.08) as usize, hr.saturating_sub((hw * 0.08) as usize));
    let cw = x1.saturating_sub(x0) + 1;
    let mut white = vec![false; cw * (y1 - y0 + 1)];
    let mut iris = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            let p = img.get_pixel(x as u32, y as u32);
            if p[3] < 220 {
                continue;
            }
            let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
            let v = r.max(g).max(b);
            let s = if v > 0.0 { (v - r.min(g).min(b)) / v } else { 0.0 };
            if v < 0.6 && s > 0.3 {
                iris.push((x, y));
            } else if v > 0.93 && s < 0.12 {
                white[(y - y0) * cw + (x - x0)] = true;
            }
        }
    }
    let rad = ((hw * 0.05) as i32).max(3);
    let sparkle = |x: usize, y: usize| {
        (-rad..=rad).step_by(2).any(|dy| {
            (-rad..=rad).step_by(2).any(|dx| {
                let (px, py) = (x as i32 + dx, y as i32 + dy);
                px >= x0 as i32 && px <= x1 as i32 && py >= y0 as i32 && py <= y1 as i32 && white[(py - y0 as i32) as usize * cw + (px - x0 as i32) as usize]
            })
        })
    };
    let good: Vec<(usize, usize)> = iris.into_iter().filter(|&(x, y)| sparkle(x, y)).collect();
    let band = (hh * 0.08) as usize;
    let side = |y: usize, left: bool| good.iter().filter(|p| p.1.abs_diff(y) <= band && ((p.0 as f32) < cx) == left).count();
    let mut eye_y = htop as f32 + hh * 0.7;
    if good.len() >= 15 {
        let mut count = vec![0u32; y1 - y0 + 1];
        for &(_, y) in &good {
            count[y - y0] += 1;
        }
        let near = |k: usize| (k.saturating_sub(4)..(k + 5).min(count.len())).map(|j| count[j]).sum::<u32>();
        if let Some(k) = (0..count.len()).max_by_key(|&k| near(k)) {
            let y = k + y0;
            if side(y, true) >= 4 && side(y, false) >= 4 {
                eye_y = y as f32;
            }
        }
    }
    // each eye: a box either side of the middle of the face
    let (ew, eh) = (hw * 0.22, hh * 0.12);
    let b = |x: f32| [(x - ew * 0.5) / w as f32, (eye_y - eh * 0.5) / h as f32, (x + ew * 0.5) / w as f32, (eye_y + eh * 0.5) / h as f32];
    Some((b(cx - hw * 0.19), b(cx + hw * 0.19), [1.0, 0.9, 0.85]))
}

/// Sleep mask spots set by hand in the settings (`eyes.json`: sprite path ->
/// [x, y] in texture units), read again whenever the file changes.
#[allow(dead_code)]
pub(super) fn eye_fix(source: &str) -> Option<[f32; 2]> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};
    type Fixes = (Option<Instant>, Option<SystemTime>, std::collections::HashMap<String, [f32; 2]>);
    static FIXES: std::sync::LazyLock<Mutex<Fixes>> = std::sync::LazyLock::new(|| Mutex::new((None, None, std::collections::HashMap::new())));
    let mut f = FIXES.lock().unwrap();
    if f.0.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
        f.0 = Some(Instant::now());
        let path = crate::util::data_dir().join("eyes.json");
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if stamp != f.1 {
            f.1 = stamp;
            f.2 = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        }
    }
    f.2.get(source).copied()
}


/// The head on a sprite, found from its outline: (left, top, right, neck)
/// in texture units. The top is below the ear tips.
pub(super) fn head_box(img: &image::RgbaImage) -> Option<[f32; 4]> {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let solid = |x: u32, y: u32| img.get_pixel(x, y)[3] > 40;
    let rows: Vec<u32> = (0..h).map(|y| (0..w).filter(|&x| solid(x, y)).count() as u32).collect();
    let top = rows.iter().position(|&c| c > 0)?;
    let bot = rows.iter().rposition(|&c| c > 0)?;
    if bot <= top + 20 {
        return None;
    }
    let all = (bot - top) as f32;
    let smooth: Vec<f32> = (0..hu).map(|y| (y.saturating_sub(3)..(y + 4).min(hu)).map(|k| rows[k] as f32).sum::<f32>() / 7.0).collect();
    let upper = smooth[top..top + (all * 0.5) as usize].iter().copied().fold(0.0, f32::max);
    let htop = (top..bot).find(|&y| smooth[y] >= upper * 0.45)?;
    let (lo, hi) = (top + (all * 0.38) as usize, top + (all * 0.62) as usize);
    let mut neck = (lo..hi).min_by(|&a, &b| smooth[a].total_cmp(&smooth[b]))?;
    if ((neck - htop) as f32) < all * 0.32 {
        neck = htop + (all * 0.4) as usize;
    }
    let head_rows = (neck - htop).max(1) as u32;
    let cols: Vec<usize> = (0..wu).filter(|&x| (htop..neck).filter(|&y| solid(x as u32, y as u32)).count() as u32 * 10 >= head_rows * 4).collect();
    let (hl, hr) = (*cols.first()?, *cols.last()?);
    // the top of the hair itself: the first row across the middle of the head that's solid
    let mid = (hl + hr) / 2;
    let span = ((hr - hl) / 6).max(1);
    let crown = (top..neck).find(|&y| (mid - span..=mid + span).filter(|&x| solid(x as u32, y as u32)).count() > span)?;
    Some([hl as f32 / w as f32, crown as f32 / h as f32, hr as f32 / w as f32, neck as f32 / h as f32])
}
