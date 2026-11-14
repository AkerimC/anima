//! Where things can be worn or held on a character sprite, found once from
//! the picture itself: the head (crown to neck), the eyes, the hands, the
//! neck, the back and the feet. All in texture units: x 0..1 left to
//! right, y 0..1 top to bottom, as the sprite file has it (calm pose).

use image::RgbaImage;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchors {
    /// left, crown (top of the hair, below ear tips), right, neck
    pub head: [f32; 4],
    /// the two eyes, left and right in the picture, and their size
    pub eyes: [[f32; 2]; 2],
    pub eye_size: [f32; 2],
    /// whether the eyes were seen, not just assumed
    pub eyes_found: bool,
    /// hands, left and right in the picture
    pub hands: [[f32; 2]; 2],
    pub hands_found: [bool; 2],
    pub neck: [f32; 2],
    pub back: [f32; 2],
    pub feet: [f32; 2],
    /// left, top, right, bottom of everything drawn
    pub body: [f32; 4],
    pub skin: [f32; 3],
}

fn hsv(p: &image::Rgba<u8>) -> (f32, f32, f32) {
    let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
    let v = r.max(g).max(b);
    let c = v - r.min(g).min(b);
    let s = if v > 0.0 { c / v } else { 0.0 };
    let h = if c <= 0.0 {
        0.0
    } else if v == r {
        60.0 * ((g - b) / c).rem_euclid(6.0)
    } else if v == g {
        60.0 * ((b - r) / c + 2.0)
    } else {
        60.0 * ((r - g) / c + 4.0)
    };
    (h, s, v)
}

pub fn find(img: &RgbaImage) -> Option<Anchors> {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let solid = |x: usize, y: usize| img.get_pixel(x as u32, y as u32)[3] > 40;
    let rows: Vec<usize> = (0..hu).map(|y| (0..wu).filter(|&x| solid(x, y)).count()).collect();
    let top = rows.iter().position(|&c| c > 0)?;
    let bot = rows.iter().rposition(|&c| c > 0)?;
    if bot <= top + 40 {
        return None;
    }
    let left = (0..wu).find(|&x| (top..=bot).step_by(2).any(|y| solid(x, y)))?;
    let right = (0..wu).rev().find(|&x| (top..=bot).step_by(2).any(|y| solid(x, y)))?;
    let all = (bot - top) as f32;

    // the head: from where the outline gets wide (below ear tips) to the neck,
    // the narrowest part between a third and two thirds of the way down
    let smooth: Vec<f32> = (0..hu).map(|y| (y.saturating_sub(3)..(y + 4).min(hu)).map(|k| rows[k] as f32).sum::<f32>() / 7.0).collect();
    let upper = smooth[top..top + (all * 0.5) as usize].iter().copied().fold(0.0, f32::max);
    let htop = (top..bot).find(|&y| smooth[y] >= upper * 0.45)?;
    let (lo, hi) = (top + (all * 0.38) as usize, top + (all * 0.62) as usize);
    let mut neck = (lo..hi).min_by(|&a, &b| smooth[a].total_cmp(&smooth[b]))?;
    if ((neck - htop) as f32) < all * 0.32 {
        neck = htop + (all * 0.4) as usize;
    }
    let head_rows = (neck - htop).max(1);
    let cols: Vec<usize> = (0..wu).filter(|&x| (htop..neck).filter(|&y| solid(x, y)).count() * 10 >= head_rows * 4).collect();
    let (hl, hr) = (*cols.first()?, *cols.last()?);
    let (hw, hh) = ((hr - hl) as f32, (neck - htop) as f32);
    let cx = (hl + hr) as f32 * 0.5;
    let mid = (hl + hr) / 2;
    let span = ((hr - hl) / 6).max(1);
    let crown = (top..neck).find(|&y| (mid - span..=mid + span).filter(|&x| solid(x, y)).count() > span).unwrap_or(htop);

    // skin: the light, warm, not too saturated colour of the lower face
    let mut tones: Vec<[f32; 3]> = Vec::new();
    let (fy0, fy1) = (htop + (hh * 0.68) as usize, neck);
    let (fx0, fx1) = ((cx - hw * 0.16) as usize, (cx + hw * 0.16) as usize);
    for y in fy0..fy1 {
        for x in fx0..=fx1 {
            let p = img.get_pixel(x as u32, y as u32);
            let (hue, s, v) = hsv(p);
            if p[3] > 220 && v > 0.72 && (0.06..0.5).contains(&s) && (hue < 45.0 || hue > 340.0) {
                tones.push([p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]);
            }
        }
    }
    let skin = if tones.len() >= 12 {
        let mut m = [0.0; 3];
        for c in 0..3 {
            let mut v: Vec<f32> = tones.iter().map(|t| t[c]).collect();
            v.sort_by(|a, b| a.total_cmp(b));
            m[c] = v[v.len() / 2];
        }
        m
    } else {
        [1.0, 0.88, 0.8]
    };
    let is_skin = |x: usize, y: usize| {
        let p = img.get_pixel(x as u32, y as u32);
        if p[3] < 200 {
            return false;
        }
        let d = (0..3).map(|c| (p[c] as f32 / 255.0 - skin[c]).powi(2)).sum::<f32>().sqrt();
        d < 0.13
    };

    // the eyes: dark iris next to a white sparkle, in the middle of the head,
    // with cheek showing under them (hair has none) and the two either side
    // of the middle of the face
    let (y0, y1) = (htop + (hh * 0.36) as usize, (htop + (hh * 0.86) as usize).min(hu - 1));
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
            let (_, s, v) = hsv(p);
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
    let (ew, eh) = (hw * 0.2, hh * 0.13);
    let mut eyes = None;
    if good.len() >= 15 {
        let mut count = vec![0u32; y1 - y0 + 1];
        for &(_, y) in &good {
            count[y - y0] += 1;
        }
        let near = |k: usize| (k.saturating_sub(4)..(k + 5).min(count.len())).map(|j| count[j]).sum::<u32>();
        let mut order: Vec<usize> = (0..count.len()).filter(|&k| count[k] > 0).collect();
        order.sort_by_key(|&k| std::cmp::Reverse(near(k)));
        for k in order.into_iter().take(12) {
            let y = k + y0;
            let pick = |left: bool| -> Option<(f32, f32)> {
                let ps: Vec<&(usize, usize)> = good.iter().filter(|p| p.1.abs_diff(y) <= band && ((p.0 as f32) < cx) == left).collect();
                (ps.len() >= 4).then(|| {
                    let n = ps.len() as f32;
                    (ps.iter().map(|p| p.0 as f32).sum::<f32>() / n, ps.iter().map(|p| p.1 as f32).sum::<f32>() / n)
                })
            };
            let (Some(l), Some(r)) = (pick(true), pick(false)) else { continue };
            let apart = r.0 - l.0;
            let centred = ((l.0 + r.0) * 0.5 - cx).abs() < hw * 0.15;
            if !centred || !(hw * 0.2..=hw * 0.75).contains(&apart) {
                continue;
            }
            // cheek under them
            let ey = (l.1 + r.1) * 0.5;
            let (cy0, cy1) = ((ey + eh * 0.9) as usize, ((ey + eh * 2.3) as usize).min(neck));
            let (cx0, cx1) = ((l.0 - ew * 0.3) as usize, (r.0 + ew * 0.3) as usize);
            let mut seen = 0;
            let mut skinny = 0;
            for yy in (cy0..cy1).step_by(2) {
                for xx in (cx0..=cx1).step_by(2) {
                    seen += 1;
                    skinny += usize::from(is_skin(xx, yy));
                }
            }
            if seen > 0 && skinny * 100 >= seen * 30 {
                eyes = Some((l, r));
                break;
            }
        }
    }
    let eyes_found = eyes.is_some();
    let ((lx, ly), (rx, ry)) = eyes.unwrap_or({
        let y = htop as f32 + hh * 0.7;
        ((cx - hw * 0.19, y), (cx + hw * 0.19, y))
    });

    // the hands: skin below the neck, off to each side of the chest, in the
    // upper part of the body (legs further down are skin too)
    let below = (bot - neck) as f32;
    let (by0, by1) = (neck + (below * 0.05) as usize, neck + (below * 0.6) as usize);
    let bw = (right - left) as f32;
    let body_cx = (left + right) as f32 * 0.5;
    let mut sides: [Vec<(f32, f32)>; 2] = [Vec::new(), Vec::new()];
    for y in by0..by1 {
        for x in left..=right {
            if !is_skin(x, y) {
                continue;
            }
            let d = x as f32 - cx;
            if d.abs() < bw * 0.1 {
                continue;
            }
            sides[usize::from(d > 0.0)].push((x as f32, y as f32));
        }
    }
    let hand = |k: usize| -> Option<(f32, f32)> {
        let ps = &sides[k];
        if ps.len() < 20 {
            return None;
        }
        // the far end of the arm: the outermost half of that skin
        let mut xs: Vec<f32> = ps.iter().map(|p| p.0).collect();
        xs.sort_by(|a, b| a.total_cmp(b));
        let cut = xs[xs.len() / 2];
        let out: Vec<&(f32, f32)> = ps.iter().filter(|p| if k == 0 { p.0 <= cut } else { p.0 >= cut }).collect();
        let n = out.len() as f32;
        Some((out.iter().map(|p| p.0).sum::<f32>() / n, out.iter().map(|p| p.1).sum::<f32>() / n))
    };
    // not seen: at the side of the body, a third of the way down from the neck
    let gy = (neck as f32 + below * 0.35) as usize;
    let edge_l = (left..=right).find(|&x| solid(x, gy)).unwrap_or(left) as f32;
    let edge_r = (left..=right).rev().find(|&x| solid(x, gy)).unwrap_or(right) as f32;
    let inset = (edge_r - edge_l) * 0.1;
    let found = [hand(0), hand(1)];
    let hands = [
        found[0].unwrap_or((edge_l + inset, gy as f32)),
        found[1].unwrap_or((edge_r - inset, gy as f32)),
    ];

    let (fw, fh) = (w as f32, h as f32);
    let u = |x: f32| x / fw;
    let v = |y: f32| y / fh;
    Some(Anchors {
        head: [u(hl as f32), v(crown as f32), u(hr as f32), v(neck as f32)],
        eyes: [[u(lx), v(ly)], [u(rx), v(ry)]],
        eye_size: [u(ew), v(eh)],
        eyes_found,
        hands: [[u(hands[0].0), v(hands[0].1)], [u(hands[1].0), v(hands[1].1)]],
        hands_found: [found[0].is_some(), found[1].is_some()],
        neck: [u(cx), v(neck as f32)],
        back: [u(cx), v(neck as f32 + below * 0.22)],
        feet: [u(body_cx), v(bot as f32)],
        body: [u(left as f32), v(top as f32), u(right as f32), v(bot as f32)],
        skin,
    })
}
