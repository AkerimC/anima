use anima::render::we_model::{parse_mdl, BonePose, ParsedMdl};

fn mat2d(p: BonePose) -> [f32; 6] {
    let (s, c) = p.rz.sin_cos();
    let sx = if p.sx.is_finite() && p.sx.abs() > 1e-5 { p.sx } else { 1.0 };
    let sy = if p.sy.is_finite() && p.sy.abs() > 1e-5 { p.sy } else { 1.0 };
    [c * sx, s * sx, -s * sy, c * sy, p.tx, p.ty]
}
fn mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

fn skin(m: &ParsedMdl, poses: &[BonePose]) -> Vec<[f32; 2]> {
    let nb = m.bones.len();
    let mut world = vec![[1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0]; nb];
    let mut sm = vec![[1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0]; nb];
    for i in 0..nb {
        let local = mat2d(poses[i]);
        let parent = m.bones[i].parent;
        let w = if parent >= 0 && (parent as usize) < i {
            mul(world[parent as usize], local)
        } else {
            local
        };
        world[i] = w;
        sm[i] = mul(w, m.bones[i].inv_rest_world);
    }
    m.raw_vertices
        .iter()
        .map(|rv| {
            let (px, py) = (rv.pos[0], rv.pos[1]);
            let mut sx = 0.0;
            let mut sy = 0.0;
            let mut ws = 0.0;
            for k in 0..4 {
                let w = rv.bone_weights[k];
                let bi = rv.bone_indices[k] as usize;
                if w > 0.0 && bi < nb {
                    let a = sm[bi];
                    sx += w * (a[0] * px + a[2] * py + a[4]);
                    sy += w * (a[1] * px + a[3] * py + a[5]);
                    ws += w;
                }
            }
            [sx / ws, sy / ws]
        })
        .collect()
}

fn bounds(p: &[[f32; 2]]) -> [f32; 4] {
    let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for q in p {
        b[0] = b[0].min(q[0]);
        b[1] = b[1].min(q[1]);
        b[2] = b[2].max(q[0]);
        b[3] = b[3].max(q[1]);
    }
    b
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let m = parse_mdl(&std::fs::read(&path).unwrap()).expect("parse");
    println!(
        "bones={} anims={} verts={}",
        m.bones.len(),
        m.anims.len(),
        m.raw_vertices.len()
    );
    let b = bounds(&skin(&m, &m.base_pose));
    println!(
        "base pose bounds x[{:.1},{:.1}] y[{:.1},{:.1}] size {:.1}x{:.1}",
        b[0],
        b[2],
        b[1],
        b[3],
        b[2] - b[0],
        b[3] - b[1]
    );
    // At t=0 every layer is a no-op: the mesh must equal the base pose.
    let e0: f32 = m
        .evaluate_vertices(0.0, &[])
        .iter()
        .zip(skin(&m, &m.base_pose))
        .map(|(a, c)| ((a.pos[0] - c[0]).powi(2) + (a.pos[1] - c[1]).powi(2)).sqrt())
        .fold(0.0, f32::max);
    println!("evaluate(t=0) vs base pose max err: {:.4}", e0);
    for anim in m.anims.iter().take(4) {
        let layer = anima::render::we_model::PuppetAnimLayer {
            anim_id: Some(anim.id),
            rate: 1.0,
            blend: 1.0,
            additive: true,
        };
        let mut max = 0.0f32;
        for step in 0..20 {
            let t = step as f32 * 0.1;
            let v = m.evaluate_vertices(t, std::slice::from_ref(&layer));
            for (p, q) in v.iter().zip(skin(&m, &m.base_pose)) {
                max = max.max(((p.pos[0] - q[0]).powi(2) + (p.pos[1] - q[1]).powi(2)).sqrt());
            }
        }
        println!(
            "anim {:>4} {:>12} frames={:<4} max displacement over 2s: {:.1}",
            anim.id,
            format!("{:?}", anim.mode),
            anim.frame_count,
            max
        );
    }
}
