//! Parser and GPU mesh for Wallpaper Engine puppet models (`.mdl`).
//!
//! Wallpaper Engine stores puppet meshes in a compact binary format.  We only
//! need the rest-pose geometry: positions and texture coordinates.  Bones are
//! ignored (the rest pose is what a scene shows before any animation).
//!
//! Layout (verified against MDLV0016/0017/0019/0023):
//! * `MDLVxxxx` signature at offset 0.
//! * material path (NUL terminated) at offset 24.
//! * a marker `00 <09|0d|0f|11|13|15|17> 00 80 01`, followed by a `u32`
//!   vertex-byte count, then the vertex block.
//! * vertex stride is 52 or 80 bytes: position (3 x f32) at +0, UV (2 x f32)
//!   at `stride - 8`.
//! * the index block follows the vertex block: `u32` byte count, then `u16`
//!   indices.

use anyhow::{bail, Result};
use glow::HasContext;

use super::gl::bytemuck_cast;

#[derive(Debug, Clone, Copy)]
pub struct MeshVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
pub struct RawVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub bone_indices: [u32; 4],
    pub bone_weights: [f32; 4],
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BonePose {
    pub tx: f32,
    pub ty: f32,
    pub rz: f32,
    pub sx: f32,
    pub sy: f32,
}

#[derive(Debug, Clone)]
pub struct MdlsBone {
    pub parent: i32,
    pub rest_local: BonePose,
    pub inv_rest_world: [f32; 6],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimMode {
    Loop,
    Mirror,
    Single,
}

#[derive(Debug, Clone)]
pub struct MdlaAnim {
    pub id: u32,
    pub mode: AnimMode,
    pub fps: f32,
    pub frame_count: usize,
    pub tracks: Vec<Vec<BonePose>>,
}

#[derive(Debug, Clone, Copy)]
pub struct PuppetAnimLayer {
    pub anim_id: Option<u32>,
    pub rate: f32,
    pub blend: f32,
    pub additive: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedMdl {
    pub vertices: Vec<MeshVertex>,
    pub raw_vertices: Vec<RawVertex>,
    pub bones: Vec<MdlsBone>,
    pub base_pose: Vec<BonePose>,
    pub anims: Vec<MdlaAnim>,
    pub indices: Vec<u16>,
    pub bounds_min: [f32; 2],
    pub bounds_max: [f32; 2],
    pub material: Option<String>,
}

impl ParsedMdl {
    pub fn has_animations(&self) -> bool {
        !self.bones.is_empty() && !self.anims.is_empty()
    }

    pub fn evaluate_vertices(&self, time: f32, layers: &[PuppetAnimLayer]) -> Vec<MeshVertex> {
        if self.bones.is_empty() || self.anims.is_empty() {
            return self.vertices.clone();
        }
        let nbones = self.bones.len();
        let mut pose = self.base_pose.clone();

        if layers.is_empty() {
            if let Some(anim) = self.anims.first() {
                for b in 0..nbones.min(anim.tracks.len()) {
                    pose[b] = sample_track(&anim.tracks[b], anim.fps, anim.frame_count, anim.mode, time);
                }
            }
        } else {
            for layer in layers {
                if layer.blend <= 0.0001 {
                    continue;
                }
                let anim = match layer.anim_id {
                    Some(id) => self.anims.iter().find(|a| a.id == id),
                    None => self.anims.first(),
                };
                let Some(anim) = anim else {
                    continue;
                };
                let t = time * layer.rate.max(0.0);
                for b in 0..nbones.min(anim.tracks.len()) {
                    let kf = sample_track(&anim.tracks[b], anim.fps, anim.frame_count, anim.mode, t);
                    let base = self.base_pose[b];
                    let w = layer.blend;
                    pose[b].tx += w * (kf.tx - base.tx);
                    pose[b].ty += w * (kf.ty - base.ty);
                    pose[b].rz += w * (kf.rz - base.rz);
                    pose[b].sx += w * (kf.sx - base.sx);
                    pose[b].sy += w * (kf.sy - base.sy);
                }
            }
        }

        skin_vertices(&self.raw_vertices, &self.bones, &pose)
    }
}

pub struct Mesh {
    pub vao: glow::VertexArray,
    vbo: glow::Buffer,
    #[allow(dead_code)]
    ibo: glow::Buffer,
    pub index_count: i32,
}

impl Mesh {
    pub fn new(gl: &glow::Context, mdl: &ParsedMdl) -> Result<Self> {
        if mdl.vertices.is_empty() || mdl.indices.is_empty() {
            bail!("empty puppet mesh");
        }
        let mut data = Vec::with_capacity(mdl.vertices.len() * 4);
        for v in &mdl.vertices {
            data.extend_from_slice(&[v.pos[0], v.pos[1], v.uv[0], v.uv[1]]);
        }
        unsafe {
            let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
            let ibo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;

            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&data), glow::DYNAMIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);

            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ibo));
            let index_bytes = std::slice::from_raw_parts(
                mdl.indices.as_ptr() as *const u8,
                mdl.indices.len() * 2,
            );
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, index_bytes, glow::STATIC_DRAW);

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
            Ok(Self {
                vao,
                vbo,
                ibo,
                index_count: mdl.indices.len() as i32,
            })
        }
    }

    pub fn update_vertices(&self, gl: &glow::Context, vertices: &[MeshVertex]) {
        let mut data = Vec::with_capacity(vertices.len() * 4);
        for v in vertices {
            data.extend_from_slice(&[v.pos[0], v.pos[1], v.uv[0], v.uv[1]]);
        }
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytemuck_cast(&data));
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
        }
    }

    pub fn draw(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_elements(glow::TRIANGLES, self.index_count, glow::UNSIGNED_SHORT, 0);
            gl.bind_vertex_array(None);
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
            gl.delete_buffer(self.ibo);
        }
    }
}

pub fn parse_mdl(bytes: &[u8]) -> Option<ParsedMdl> {
    if bytes.len() < 32 || !bytes.starts_with(b"MDLV") {
        return None;
    }
    let material = {
        let start = bytes
            .windows(9)
            .position(|w| w == b"materials/")
            .map(|p| p)
            .or_else(|| bytes.windows(8).position(|w| w == b"material/"))?;
        let end = bytes[start..].iter().position(|&b| b == 0)? + start;
        std::str::from_utf8(&bytes[start..end])
            .ok()
            .map(str::to_string)
    };

    // The material path is followed by padding and a five-byte vertex-block
    // marker `00 <stride-variant> 00 80 01`, then a 32-bit vertex byte count.
    let search_from = material
        .as_ref()
        .and_then(|_| bytes.windows(9).position(|w| w == b"materials/"))
        .unwrap_or(0);
    let marker = bytes[search_from..]
        .windows(5)
        .position(|w| {
            w[0] == 0
                && matches!(w[1], 9 | 13 | 15 | 17 | 19 | 21 | 23)
                && w[2] == 0
                && w[3] == 0x80
                && w[4] == 0x01
        })
        .map(|p| p + search_from)?;
    let n = u32::from_le_bytes(bytes.get(marker + 5..marker + 9)?.try_into().ok()?) as usize;
    let data_start = marker + 9;
    if n == 0 || data_start + n + 8 > bytes.len() {
        return None;
    }
    let index_bytes = u32::from_le_bytes(
        bytes
            .get(data_start + n..data_start + n + 4)?
            .try_into()
            .ok()?,
    ) as usize;
    let indices_start = data_start + n + 4;
    if index_bytes == 0 || indices_start + index_bytes > bytes.len() {
        return None;
    }
    let index_count = index_bytes / 2;
    let mut indices = Vec::with_capacity(index_count);
    for i in 0..index_count {
        let off = indices_start + i * 2;
        indices.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
    }
    let vertex_count = (*indices.iter().max()? as usize) + 1;
    if vertex_count == 0 || n % vertex_count != 0 {
        return None;
    }
    let stride = n / vertex_count;
    if stride < 20 || stride % 4 != 0 {
        return None;
    }

    let mut raw_vertices = Vec::with_capacity(vertex_count);
    for i in 0..vertex_count {
        let base = data_start + i * stride;
        let x = f32::from_le_bytes(bytes.get(base..base + 4)?.try_into().ok()?);
        let y = f32::from_le_bytes(bytes.get(base + 4..base + 8)?.try_into().ok()?);
        let uv_base = base + stride - 8;
        let u = f32::from_le_bytes(bytes.get(uv_base..uv_base + 4)?.try_into().ok()?);
        let v = f32::from_le_bytes(bytes.get(uv_base + 4..uv_base + 8)?.try_into().ok()?);

        let (bone_indices, bone_weights) = if stride == 52 {
            read_skin_attrs(bytes, base + 12)
        } else if stride == 80 {
            read_skin_attrs(bytes, base + 40)
        } else {
            ([0; 4], [1.0, 0.0, 0.0, 0.0])
        };

        raw_vertices.push(RawVertex {
            pos: [x, y],
            uv: [u, v],
            bone_indices,
            bone_weights,
        });
    }

    let search_after = indices_start + index_bytes;
    let bones = parse_mdls(bytes, search_after).unwrap_or_default();
    let anims = if !bones.is_empty() {
        parse_mdla(bytes, search_after, bones.len()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let base_pose = compute_base_pose(&bones, &anims);

    let vertices = if !bones.is_empty() && !anims.is_empty() {
        skin_vertices(&raw_vertices, &bones, &base_pose)
    } else {
        raw_vertices
            .iter()
            .map(|rv| MeshVertex {
                pos: rv.pos,
                uv: rv.uv,
            })
            .collect()
    };

    let mut bounds_min = [f32::INFINITY; 2];
    let mut bounds_max = [f32::NEG_INFINITY; 2];
    for vtx in &vertices {
        let [x, y] = vtx.pos;
        if x.is_finite() && y.is_finite() {
            bounds_min[0] = bounds_min[0].min(x);
            bounds_min[1] = bounds_min[1].min(y);
            bounds_max[0] = bounds_max[0].max(x);
            bounds_max[1] = bounds_max[1].max(y);
        }
    }
    if !bounds_min[0].is_finite() {
        bounds_min = [0.0, 0.0];
        bounds_max = [0.0, 0.0];
    }

    Some(ParsedMdl {
        vertices,
        raw_vertices,
        bones,
        base_pose,
        anims,
        indices,
        bounds_min,
        bounds_max,
        material,
    })
}

fn read_skin_attrs(bytes: &[u8], off: usize) -> ([u32; 4], [f32; 4]) {
    let Some(slice) = bytes.get(off..off + 32) else {
        return ([0; 4], [1.0, 0.0, 0.0, 0.0]);
    };
    let mut idx = [0u32; 4];
    let mut w = [0.0f32; 4];
    for k in 0..4 {
        idx[k] = u32::from_le_bytes(slice[k * 4..k * 4 + 4].try_into().unwrap_or([0; 4]));
        w[k] = f32::from_le_bytes(slice[16 + k * 4..20 + k * 4].try_into().unwrap_or([0; 4]));
        if !w[k].is_finite() {
            w[k] = 0.0;
        }
    }
    (idx, w)
}

fn mat2d_from_pose(p: BonePose) -> [f32; 6] {
    let (s, c) = p.rz.sin_cos();
    let sx = if p.sx.is_finite() && p.sx.abs() > 1e-5 { p.sx } else { 1.0 };
    let sy = if p.sy.is_finite() && p.sy.abs() > 1e-5 { p.sy } else { 1.0 };
    [c * sx, s * sx, -s * sy, c * sy, p.tx, p.ty]
}

fn mat2d_mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

fn mat2d_inv(m: [f32; 6]) -> [f32; 6] {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-8 {
        return [1.0, 0.0, 0.0, 1.0, -m[4], -m[5]];
    }
    let inv = 1.0 / det;
    let i0 = m[3] * inv;
    let i1 = -m[1] * inv;
    let i2 = -m[2] * inv;
    let i3 = m[0] * inv;
    [
        i0,
        i1,
        i2,
        i3,
        -(i0 * m[4] + i2 * m[5]),
        -(i1 * m[4] + i3 * m[5]),
    ]
}

fn parse_mdls(bytes: &[u8], from: usize) -> Option<Vec<MdlsBone>> {
    let mdls_off = bytes.get(from..)?.windows(8).position(|w| w.starts_with(b"MDLS000"))? + from;
    let mut pos = mdls_off + 9;
    let _end_size = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?);
    let nbones = u32::from_le_bytes(bytes.get(pos + 4..pos + 8)?.try_into().ok()?) as usize;
    pos += 8;
    if nbones == 0 || nbones > 512 {
        return None;
    }

    let mut bones = Vec::with_capacity(nbones);
    let mut world = Vec::with_capacity(nbones);
    for _ in 0..nbones {
        let z = bytes.get(pos..)?.iter().position(|&b| b == 0)?;
        pos += z + 1;
        let _unk = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?);
        let parent = i32::from_le_bytes(bytes.get(pos + 4..pos + 8)?.try_into().ok()?);
        let msize = u32::from_le_bytes(bytes.get(pos + 8..pos + 12)?.try_into().ok()?) as usize;
        pos += 12;
        if msize < 64 || pos + msize > bytes.len() {
            return None;
        }
        let mut m = [0.0f32; 16];
        for k in 0..16 {
            m[k] = f32::from_le_bytes(bytes[pos + k * 4..pos + k * 4 + 4].try_into().ok()?);
        }
        pos += msize;
        let z2 = bytes.get(pos..)?.iter().position(|&b| b == 0)?;
        pos += z2 + 1;

        let sx = m[0].hypot(m[1]).max(1e-5);
        let sy = m[4].hypot(m[5]).max(1e-5);
        let rz = m[1].atan2(m[0]);
        let rest_local = BonePose {
            tx: m[12],
            ty: m[13],
            rz,
            sx,
            sy,
        };
        let local_m = [m[0], m[1], m[4], m[5], m[12], m[13]];
        let w_m = if parent >= 0 && (parent as usize) < world.len() {
            mat2d_mul(world[parent as usize], local_m)
        } else {
            local_m
        };
        world.push(w_m);
        bones.push(MdlsBone {
            parent,
            rest_local,
            inv_rest_world: mat2d_inv(w_m),
        });
    }
    Some(bones)
}

fn parse_mdla(bytes: &[u8], from: usize, expected_bones: usize) -> Option<Vec<MdlaAnim>> {
    let mdla_off = bytes.get(from..)?.windows(8).position(|w| w.starts_with(b"MDLA000"))? + from;
    let mut p = mdla_off + 17;
    let mut anims = Vec::new();

    while p < bytes.len() {
        let mut best: Option<(usize, AnimMode, usize)> = None;
        for (pat, mode) in [
            (&b"loop\0"[..], AnimMode::Loop),
            (&b"mirror\0"[..], AnimMode::Mirror),
            (&b"single\0"[..], AnimMode::Single),
        ] {
            let mut search = p;
            while let Some(rel) = bytes.get(search..)?.windows(pat.len()).position(|w| w == pat) {
                let idx = search + rel;
                let after = idx + pat.len();
                if let Some(hdr) = bytes.get(after..after + 16) {
                    let fps = f32::from_le_bytes(hdr[0..4].try_into().unwrap());
                    let fcount = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
                    let abones = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
                    if abones == expected_bones
                        && fps.is_finite()
                        && fps > 0.0
                        && fps <= 240.0
                        && fcount > 0
                        && fcount <= 100_000
                    {
                        if best.map_or(true, |(b_idx, _, _)| idx < b_idx) {
                            best = Some((idx, mode, pat.len()));
                        }
                        break;
                    }
                }
                search = idx + 1;
            }
        }
        let Some((idx, mode, pat_len)) = best else {
            break;
        };

        // `anim_id` is stored 8 bytes before `name\0`, which immediately precedes `mode\0`.
        let name_end = idx.saturating_sub(1);
        let name_start = bytes[p..name_end]
            .iter()
            .rposition(|&b| b == 0)
            .map(|r| p + r + 1)
            .unwrap_or(p);
        let id = if name_start >= 8 {
            u32::from_le_bytes(bytes[name_start - 8..name_start - 4].try_into().unwrap_or([0; 4]))
        } else {
            0
        };

        let mut pos = idx + pat_len;
        let hdr = bytes.get(pos..pos + 16)?;
        let fps = f32::from_le_bytes(hdr[0..4].try_into().ok()?);
        let frame_count = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as usize;
        let abones = u32::from_le_bytes(hdr[12..16].try_into().ok()?) as usize;
        pos += 16;

        let mut tracks = Vec::with_capacity(abones);
        let mut ok = true;
        for _ in 0..abones {
            let Some(thdr) = bytes.get(pos..pos + 8) else {
                ok = false;
                break;
            };
            let bbytes = u32::from_le_bytes(thdr[4..8].try_into().unwrap()) as usize;
            pos += 8;
            if pos + bbytes > bytes.len() {
                ok = false;
                break;
            }
            let nframes = bbytes / 36;
            let mut frames = Vec::with_capacity(nframes);
            for f in 0..nframes {
                let foff = pos + f * 36;
                let tx = f32::from_le_bytes(bytes[foff..foff + 4].try_into().unwrap());
                let ty = f32::from_le_bytes(bytes[foff + 4..foff + 8].try_into().unwrap());
                let rz = f32::from_le_bytes(bytes[foff + 20..foff + 24].try_into().unwrap());
                let sx = f32::from_le_bytes(bytes[foff + 24..foff + 28].try_into().unwrap());
                let sy = f32::from_le_bytes(bytes[foff + 28..foff + 32].try_into().unwrap());
                frames.push(BonePose { tx, ty, rz, sx, sy });
            }
            pos += bbytes;
            tracks.push(frames);
        }
        if !ok {
            break;
        }
        anims.push(MdlaAnim {
            id,
            mode,
            fps,
            frame_count,
            tracks,
        });
        p = pos;
    }
    Some(anims)
}

fn compute_base_pose(bones: &[MdlsBone], anims: &[MdlaAnim]) -> Vec<BonePose> {
    let mut base: Vec<BonePose> = bones.iter().map(|b| b.rest_local).collect();
    if anims.is_empty() {
        return base;
    }
    // Use the median of frame 0 across animations (or frame 0 of the single animation)
    // so that bones moved only at frame 0 of one blink track still get the neutral base pose.
    for b in 0..bones.len() {
        let mut txs: Vec<f32> = anims
            .iter()
            .filter_map(|a| a.tracks.get(b).and_then(|t| t.first()).map(|p| p.tx))
            .collect();
        let mut tys: Vec<f32> = anims
            .iter()
            .filter_map(|a| a.tracks.get(b).and_then(|t| t.first()).map(|p| p.ty))
            .collect();
        let mut rzs: Vec<f32> = anims
            .iter()
            .filter_map(|a| a.tracks.get(b).and_then(|t| t.first()).map(|p| p.rz))
            .collect();
        let mut sxs: Vec<f32> = anims
            .iter()
            .filter_map(|a| a.tracks.get(b).and_then(|t| t.first()).map(|p| p.sx))
            .collect();
        let mut sys: Vec<f32> = anims
            .iter()
            .filter_map(|a| a.tracks.get(b).and_then(|t| t.first()).map(|p| p.sy))
            .collect();
        if !txs.is_empty() {
            txs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            tys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            rzs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sxs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = txs.len() / 2;
            base[b] = BonePose {
                tx: txs[mid],
                ty: tys[mid],
                rz: rzs[mid],
                sx: sxs[mid],
                sy: sys[mid],
            };
        }
    }
    base
}

fn sample_track(
    track: &[BonePose],
    fps: f32,
    frame_count: usize,
    mode: AnimMode,
    time: f32,
) -> BonePose {
    if track.is_empty() {
        return BonePose {
            sx: 1.0,
            sy: 1.0,
            ..Default::default()
        };
    }
    if track.len() == 1 || frame_count <= 1 {
        return track[0];
    }
    let max_idx = (track.len() - 1) as f32;
    let raw_f = (time.max(0.0) * fps).max(0.0);
    let f = match mode {
        AnimMode::Loop => raw_f.rem_euclid(max_idx.max(1.0)),
        AnimMode::Mirror => {
            let period = 2.0 * max_idx.max(1.0);
            let m = raw_f.rem_euclid(period);
            if m <= max_idx {
                m
            } else {
                period - m
            }
        }
        AnimMode::Single => raw_f.min(max_idx),
    };
    let i0 = (f.floor() as usize).min(track.len() - 1);
    let i1 = (i0 + 1).min(track.len() - 1);
    let frac = f - (i0 as f32);
    let a = track[i0];
    let b = track[i1];
    BonePose {
        tx: a.tx + (b.tx - a.tx) * frac,
        ty: a.ty + (b.ty - a.ty) * frac,
        rz: a.rz + (b.rz - a.rz) * frac,
        sx: a.sx + (b.sx - a.sx) * frac,
        sy: a.sy + (b.sy - a.sy) * frac,
    }
}

fn skin_vertices(raw: &[RawVertex], bones: &[MdlsBone], poses: &[BonePose]) -> Vec<MeshVertex> {
    let nbones = bones.len();
    let mut pose_world = vec![[1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0]; nbones];
    let mut skin_mats = vec![[1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0]; nbones];
    for i in 0..nbones {
        let local = mat2d_from_pose(poses[i]);
        let parent = bones[i].parent;
        let w = if parent >= 0 && (parent as usize) < i {
            mat2d_mul(pose_world[parent as usize], local)
        } else {
            local
        };
        pose_world[i] = w;
        skin_mats[i] = mat2d_mul(w, bones[i].inv_rest_world);
    }

    let mut out = Vec::with_capacity(raw.len());
    for rv in raw {
        let [px, py] = rv.pos;
        let mut sx = 0.0f32;
        let mut sy = 0.0f32;
        let mut wsum = 0.0f32;
        for k in 0..4 {
            let w = rv.bone_weights[k];
            let bi = rv.bone_indices[k] as usize;
            if w > 0.0 && bi < nbones {
                let m = skin_mats[bi];
                sx += w * (m[0] * px + m[2] * py + m[4]);
                sy += w * (m[1] * px + m[3] * py + m[5]);
                wsum += w;
            }
        }
        let pos = if wsum > 1e-5 {
            [sx / wsum, sy / wsum]
        } else if !skin_mats.is_empty() {
            let m = skin_mats[0];
            [m[0] * px + m[2] * py + m[4], m[1] * px + m[3] * py + m[5]]
        } else {
            [px, py]
        };
        out.push(MeshVertex { pos, uv: rv.uv });
    }
    out
}
