//! Wallpaper Engine `.pkg` archive reader and `.tex` texture decoder.
//!
//! Supports:
//! * `PKGV0001` .. `PKGV0029` virtual filesystem archives (`scene.pkg`, `gifscene.pkg`)
//! * `TEXV0005` / `TEXI0001` / `TEXB0001..0004` textures:
//!   - Uncompressed RGBA8, RG88, R8
//!   - LZ4-compressed mipmaps (pure-Rust fast decoder, no external C library needed)
//!   - DXT1 (BC1), DXT3 (BC2), DXT5 (BC3) block decompression
//!   - Embedded PNG / JPEG / WebP / GIF payloads inside `.tex` containers

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use image::RgbaImage;

/// Virtual filesystem backed by a directory on disk and an optional `.pkg`
/// archive (`scene.pkg` or `gifscene.pkg`).
#[derive(Debug, Clone, Default)]
pub struct PackageFs {
    root: PathBuf,
    entries: HashMap<String, Vec<u8>>,
}

impl PackageFs {
    /// Open a wallpaper directory, automatically indexing `scene.pkg` or
    /// `gifscene.pkg` if present.
    pub fn open(root: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        for name in ["scene.pkg", "gifscene.pkg"] {
            let pkg_path = root.join(name);
            if pkg_path.is_file() {
                if let Ok(parsed) = parse_pkg_file(&pkg_path) {
                    for (k, v) in parsed {
                        entries.insert(normalize_key(&k), v);
                    }
                }
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            entries,
        })
    }

    /// Open a standalone `.pkg` file directly.
    pub fn from_pkg_file(pkg_path: &Path) -> Result<Self> {
        let root = pkg_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let parsed = parse_pkg_file(pkg_path)?;
        let mut entries = HashMap::with_capacity(parsed.len());
        for (k, v) in parsed {
            entries.insert(normalize_key(&k), v);
        }
        Ok(Self { root, entries })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn has_pkg_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn pkg_file_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// Read a relative path from disk first, falling back to the `.pkg` archive
    /// and finally Wallpaper Engine's global `assets/` directory.
    pub fn read(&self, rel: &str) -> Result<Vec<u8>> {
        let key = normalize_key(rel);
        let disk_path = self.root.join(&key);
        if disk_path.is_file() {
            return fs::read(&disk_path)
                .with_context(|| format!("reading {}", disk_path.display()));
        }
        if let Some(bytes) = self.entries.get(&key) {
            return Ok(bytes.clone());
        }
        // Case-insensitive fallback for Windows-authored Workshop packages.
        let lower = key.to_ascii_lowercase();
        for (k, v) in &self.entries {
            if k.to_ascii_lowercase() == lower {
                return Ok(v.clone());
            }
        }
        if let Some(we_path) = resolve_we_asset(&key) {
            if let Ok(bytes) = fs::read(&we_path) {
                return Ok(bytes);
            }
        }
        bail!("asset not found in package or disk: {rel}")
    }

    pub fn read_to_string(&self, rel: &str) -> Result<String> {
        let bytes = self.read(rel)?;
        String::from_utf8(bytes).with_context(|| format!("invalid UTF-8 in {rel}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        let key = normalize_key(rel);
        if self.root.join(&key).is_file() || self.entries.contains_key(&key) {
            return true;
        }
        let lower = key.to_ascii_lowercase();
        if self
            .entries
            .keys()
            .any(|k| k.to_ascii_lowercase() == lower)
        {
            return true;
        }
        resolve_we_asset(&key).is_some()
    }

    /// If `rel` names a video texture (an MP4 stream inside a `.tex`), return
    /// the cached file holding that stream plus the size the container records
    /// for it.  The stream is extracted once into the texture cache directory;
    /// `mpv` plays it from there at full frame rate instead of the sampled
    /// atlas that `decode_tex` would build.
    pub fn video_file(&self, rel: &str) -> Option<(PathBuf, u32, u32)> {
        for cand in texture_candidates(rel) {
            if !cand.ends_with(".tex") {
                continue;
            }
            let Ok(bytes) = self.read(&cand) else {
                continue;
            };
            let Ok((hdr, payload)) = split_tex(&bytes) else {
                continue;
            };
            if !is_mp4(&payload) {
                continue;
            }
            let path = mp4_cache_file(&payload)?;
            return Some((path, hdr.img_width, hdr.img_height));
        }
        None
    }

    /// Load an image or `.tex` texture by relative path (or material path).
    pub fn load_rgba(&self, rel: &str) -> Result<DecodedTex> {
        let key = normalize_key(rel);

        for cand in &texture_candidates(rel) {
            if let Ok(bytes) = self.read(cand) {
                if cand.ends_with(".tex") || bytes.starts_with(b"TEXV") {
                    return decode_tex(&bytes)
                        .with_context(|| format!("decoding texture {cand}"));
                }
                let img = image::load_from_memory(&bytes)
                    .with_context(|| format!("decoding image {cand}"))?
                    .to_rgba8();
                return Ok(DecodedTex {
                    image: img,
                    spritesheet: None,
                    clamp_uvs: false,
                    no_interpolation: false,
                    is_r8: false,
                });
            }
        }

        // If `rel` points to a `.json` material file, resolve its `passes[0].textures[0]`.
        for cand in [key.clone(), format!("materials/{key}")] {
            if cand.ends_with(".json") {
                if let Ok(text) = self.read_to_string(&cand) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(tex_name) = val
                            .get("passes")
                            .and_then(|p| p.as_array())
                            .and_then(|a| a.first())
                            .and_then(|p| p.get("textures"))
                            .and_then(|t| t.as_array())
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                        {
                            return self.load_rgba(tex_name);
                        }
                    }
                }
            }
        }

        bail!("could not resolve texture or material: {rel}")
    }

    /// Unpack all entries from the `.pkg` archive into `dest_dir`.
    pub fn unpack_to(&self, dest_dir: &Path) -> Result<usize> {
        let mut count = 0;
        for (rel, data) in &self.entries {
            let out_path = dest_dir.join(rel);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out_path, data)?;
            count += 1;
        }
        Ok(count)
    }
}

fn normalize_key(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Relative paths a texture name can resolve to, most specific first.
fn texture_candidates(rel: &str) -> Vec<String> {
    let key = normalize_key(rel);
    // Texture names may end in a space before the extension
    // (`materials/Miles & Gwen Parts 1 .tex`).  `normalize_key` trims the
    // whole path, so try the untrimmed stem first.
    let raw = rel
        .replace('\\', "/")
        .trim_start()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    let mut candidates = Vec::new();
    if raw != key {
        candidates.push(format!("{raw}.tex"));
        candidates.push(format!("materials/{raw}.tex"));
    }
    candidates.extend([
        key.clone(),
        format!("materials/{key}"),
        format!("materials/{key}.tex"),
        format!("{key}.tex"),
        format!("materials/{key}.png"),
        format!("{key}.png"),
    ]);
    candidates
}

/// Whether a decompressed `.tex` payload is an MP4 stream (video texture).
fn is_mp4(payload: &[u8]) -> bool {
    payload.len() >= 12 && &payload[4..8] == b"ftyp"
}

pub fn resolve_we_asset(rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches('/');
    let home = std::env::var_os("HOME")?;
    let home = Path::new(&home);
    let roots = [
        home.join(".local/share/Steam/steamapps/common/wallpaper_engine/assets"),
        home.join(".steam/steam/steamapps/common/wallpaper_engine/assets"),
    ];
    for base in &roots {
        if !base.is_dir() {
            continue;
        }
        let direct = base.join(rel);
        if direct.is_file() {
            return Some(direct);
        }
        // Check presets/*/<rel> and effects/*/<rel> when Wallpaper Engine groups
        // built-in preset/effect assets into subdirectories.  Always scan real
        // effect/preset directories across all entries before falling back to
        // `preview/` subdirectories, which often hold older stripped shaders.
        if rel.starts_with("particles/") || rel.starts_with("materials/presets/") {
            if let Ok(entries) = fs::read_dir(base.join("presets")) {
                let dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                for dir in &dirs {
                    let cand = dir.join(rel);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
                for dir in &dirs {
                    let cand_prev = dir.join("preview").join(rel);
                    if cand_prev.is_file() {
                        return Some(cand_prev);
                    }
                }
            }
        }
        if rel.starts_with("materials/effects/")
            || rel.starts_with("shaders/effects/")
            || rel.starts_with("effects/")
        {
            if let Ok(entries) = fs::read_dir(base.join("effects")) {
                let dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                for dir in &dirs {
                    let cand = dir.join(rel);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
                for dir in &dirs {
                    let cand_prev = dir.join("preview").join(rel);
                    if cand_prev.is_file() {
                        return Some(cand_prev);
                    }
                }
            }
        }
    }
    None
}

/// Parse a Wallpaper Engine `PKGV` file into `(path, bytes)` entries.
pub fn parse_pkg_file(path: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_pkg_bytes(&data)
}

pub fn parse_pkg_bytes(data: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let mut r = BinReader::new(data);
    let magic = r.read_len_prefixed_string()?;
    if !magic.starts_with("PKGV") {
        bail!("invalid .pkg magic header: {magic}");
    }
    let file_count = r.read_u32()? as usize;
    if file_count > 100_000 {
        bail!("unreasonable .pkg file count: {file_count}");
    }

    struct EntryMeta {
        path: String,
        offset: usize,
        length: usize,
    }

    let mut metas = Vec::with_capacity(file_count.min(4096));
    for _ in 0..file_count {
        let path = r.read_len_prefixed_string()?;
        let offset = r.read_u32()? as usize;
        let length = r.read_u32()? as usize;
        metas.push(EntryMeta {
            path,
            offset,
            length,
        });
    }

    let data_base = r.pos;
    let mut map = HashMap::with_capacity(metas.len());
    for m in metas {
        let start = data_base.saturating_add(m.offset);
        let end = start.saturating_add(m.length);
        if end <= data.len() {
            map.insert(normalize_key(&m.path), data[start..end].to_vec());
        }
    }
    Ok(map)
}

#[derive(Debug, Clone, Copy)]
pub struct SpritesheetMeta {
    pub cols: u32,
    pub rows: u32,
    pub frames: u32,
    pub duration: f32,
}

#[derive(Debug, Clone)]
pub struct DecodedTex {
    pub image: RgbaImage,
    pub spritesheet: Option<SpritesheetMeta>,
    pub clamp_uvs: bool,
    pub no_interpolation: bool,
    pub is_r8: bool,
}

/// Header fields of a `.tex` container plus the size hints WE stores in it.
struct TexHeader {
    format: u32,
    flags: u32,
    img_width: u32,
    img_height: u32,
    mip_width: u32,
    mip_height: u32,
}

/// Split a `.tex` container into its header and the first mipmap payload
/// (LZ4 blocks already decompressed).
fn split_tex(data: &[u8]) -> Result<(TexHeader, Vec<u8>)> {
    let mut r = BinReader::new(data);
    let magic1 = r.read_c_magic(9)?;
    if !magic1.starts_with("TEXV") {
        bail!("invalid .tex magic: {magic1}");
    }
    let magic2 = r.read_c_magic(9)?;
    if !magic2.starts_with("TEXI") {
        bail!("invalid .tex info header: {magic2}");
    }

    let format = r.read_u32()?;
    let flags = r.read_u32()?;
    let _tex_width = r.read_u32()?;
    let _tex_height = r.read_u32()?;
    let img_width = r.read_u32()?;
    let img_height = r.read_u32()?;
    let _unk = r.read_u32()?;

    let texb = r.read_c_magic(9)?;
    if !texb.starts_with("TEXB") {
        bail!("invalid .tex body header: {texb}");
    }
    let texb_ver: u32 = texb[4..].parse().unwrap_or(1);

    let image_count = r.read_u32()?;
    if texb_ver >= 3 {
        let _free_image_format = r.read_i32()?;
    }
    if texb_ver >= 4 {
        let _is_video_mp4 = r.read_u32()?;
    }

    if image_count == 0 {
        bail!(".tex contains 0 images");
    }

    let mipmap_count = r.read_u32()?;
    if mipmap_count == 0 {
        bail!(".tex image contains 0 mipmaps");
    }

    // Read the first (highest-resolution) mipmap.
    let mip_width = r.read_u32()?.max(1);
    let mip_height = r.read_u32()?.max(1);
    let (lz4_compressed, decompressed_size) = if texb_ver >= 2 {
        let comp = r.read_u32()? != 0;
        let decomp = r.read_u32()? as usize;
        (comp, decomp)
    } else {
        (false, 0)
    };
    let byte_count = r.read_u32()? as usize;
    let raw_slice = r.read_bytes(byte_count)?;

    let payload: Vec<u8> = if lz4_compressed {
        lz4_decompress_block(raw_slice, decompressed_size)?
    } else {
        raw_slice.to_vec()
    };

    Ok((
        TexHeader {
            format,
            flags,
            img_width,
            img_height,
            mip_width,
            mip_height,
        },
        payload,
    ))
}

/// Decode a Wallpaper Engine `.tex` binary buffer into an `RgbaImage`.
pub fn decode_tex(data: &[u8]) -> Result<DecodedTex> {
    let (hdr, payload) = split_tex(data)?;
    let TexHeader {
        format,
        flags,
        img_width,
        img_height,
        mip_width,
        mip_height,
    } = hdr;
    let no_interpolation = (flags & 1) != 0;
    let clamp_uvs = (flags & 2) != 0;

    // Video textures (`flags & 32`) carry a raw MP4 stream.  Decode it into a
    // spritesheet atlas so image layers can play it back like an animated
    // texture.
    if is_mp4(&payload) {
        let (image, spritesheet) = decode_mp4_atlas(&payload, img_width, img_height)
            .context("decoding embedded MP4 in .tex")?;
        return Ok(DecodedTex {
            image,
            spritesheet,
            clamp_uvs: true,
            no_interpolation,
            is_r8: false,
        });
    }

    // Check if payload is actually an embedded standard image (PNG / JPEG / WebP / GIF).
    if is_embedded_image(&payload) {
        let decoded = image::load_from_memory(&payload)
            .context("decoding embedded image in .tex")?
            .to_rgba8();
        let cropped = crop_if_needed(decoded, img_width, img_height, flags);
        let spritesheet = parse_texs_spritesheet(data, cropped.width(), cropped.height());
        return Ok(DecodedTex {
            image: cropped,
            spritesheet,
            clamp_uvs,
            no_interpolation,
            is_r8: false,
        });
    }

    let rgba = match format {
        // 0 = RGBA8888
        0 => decode_rgba8(&payload, mip_width, mip_height)?,
        // 4 = DXT5 (BC3)
        4 => decode_dxt5(&payload, mip_width, mip_height)?,
        // 6 = DXT3 (BC2)
        6 => decode_dxt3(&payload, mip_width, mip_height)?,
        // 7 = DXT1 (BC1)
        7 => decode_dxt1(&payload, mip_width, mip_height)?,
        // 8 = RG88
        8 => decode_rg88(&payload, mip_width, mip_height)?,
        // 9 = R8
        9 => decode_r8(&payload, mip_width, mip_height)?,
        other => bail!("unsupported .tex pixel format: {other}"),
    };

    let full = RgbaImage::from_raw(mip_width, mip_height, rgba)
        .context("constructing RgbaImage from decoded .tex")?;
    let cropped = crop_if_needed(full, img_width, img_height, flags);
    let spritesheet = parse_texs_spritesheet(data, cropped.width(), cropped.height());

    Ok(DecodedTex {
        image: cropped,
        spritesheet,
        clamp_uvs,
        no_interpolation,
        is_r8: format == 9,
    })
}

fn parse_texs_spritesheet(data: &[u8], img_w: u32, img_h: u32) -> Option<SpritesheetMeta> {
    let idx = data.windows(7).position(|w| w == b"TEXS000")?;
    if idx + 13 > data.len() {
        return None;
    }
    let ver = data.get(idx + 7).copied().unwrap_or(b'2');
    let frames = u32::from_le_bytes(data[idx + 9..idx + 13].try_into().ok()?);
    if frames == 0 || frames > 4096 {
        return None;
    }
    let frame0_off = if ver >= b'3' { idx + 21 } else { idx + 13 };
    if frame0_off + 28 > data.len() {
        return None;
    }
    let f = |off: usize| -> f32 {
        f32::from_le_bytes(data[off..off + 4].try_into().unwrap_or([0; 4]))
    };
    let dt = f(frame0_off + 4);
    let xax_x = f(frame0_off + 16);
    let xax_y = f(frame0_off + 20);
    let yax_x = f(frame0_off + 24);
    let yax_y = f(frame0_off + 28);
    let fw = xax_x.hypot(xax_y);
    let fh = yax_x.hypot(yax_y);
    if fw < 1.0 || fh < 1.0 {
        return None;
    }
    let cols = (img_w as f32 / fw).round().max(1.0) as u32;
    let rows = (img_h as f32 / fh).round().max(1.0) as u32;
    let frame_time = if dt > 0.0001 { dt } else { 1.0 / 30.0 };
    Some(SpritesheetMeta {
        cols,
        rows,
        frames,
        duration: (frame_time * frames as f32).max(0.01),
    })
}

/// Decode an MP4 payload with `ffmpeg` into a grid atlas of frames.
///
/// The atlas is capped at roughly 24 Mpx (≈96 MiB RGBA) and 8192 px per side,
/// so long or 4K clips are downscaled; short WE video textures (transition
/// clips, glitch overlays) fit comfortably.
///
/// Two things keep a 4K video texture (24 s of 3840x2160 at 60 fps, 45 MB in
/// the `.pkg`) from freezing the switch for seconds:
/// * long clips are decoded from keyframes only (see `sparse` below), and
/// * the finished atlas is cached under `~/.cache/anima/atlas`, so switching
///   back to the wallpaper costs a disk read instead of an ffmpeg run.
fn decode_mp4_atlas(
    mp4: &[u8],
    hint_w: u32,
    hint_h: u32,
) -> Result<(RgbaImage, Option<SpritesheetMeta>)> {
    let cache = atlas_cache_path(mp4);
    if let Some(path) = &cache {
        if let Some(hit) = read_atlas_cache(path) {
            log::debug!("video texture: cache hit {}", path.display());
            return Ok(hit);
        }
    }

    let decoded = decode_mp4_atlas_uncached(mp4, hint_w, hint_h)?;
    if let Some(path) = &cache {
        match write_atlas_cache(path, &decoded.0, decoded.1.as_ref()) {
            Ok(()) => {
                log::debug!("video texture: cached {}", path.display());
                prune_atlas_cache(path);
            }
            Err(err) => log::debug!("video texture: not cached: {err:#}"),
        }
    }
    Ok(decoded)
}

fn decode_mp4_atlas_uncached(
    mp4: &[u8],
    hint_w: u32,
    hint_h: u32,
) -> Result<(RgbaImage, Option<SpritesheetMeta>)> {
    use std::process::Command;

    // Prefer the extracted copy the video renderer also uses; only fall back
    // to a scratch file when the cache directory is unusable.
    let cached = mp4_cache_file(mp4);
    let tmp = match &cached {
        Some(path) => path.clone(),
        None => {
            let path = std::env::temp_dir().join(format!(
                "anima-tex-{}-{:x}.mp4",
                std::process::id(),
                mp4.len() ^ (mp4.as_ptr() as usize)
            ));
            fs::write(&path, mp4)?;
            path
        }
    };
    let result = (|| -> Result<(RgbaImage, Option<SpritesheetMeta>)> {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_packets",
                "-show_entries",
                "stream=width,height,nb_read_packets,r_frame_rate",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&tmp)
            .output()
            .context("running ffprobe (is ffmpeg installed?)")?;
        let text = String::from_utf8_lossy(&probe.stdout);
        let mut src_w = hint_w;
        let mut src_h = hint_h;
        let mut frames = 0u32;
        let mut fps = 30.0f32;
        for line in text.lines() {
            let (k, v) = match line.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            let v = v.trim();
            match k.trim() {
                "width" => src_w = v.parse().unwrap_or(src_w),
                "height" => src_h = v.parse().unwrap_or(src_h),
                "nb_read_packets" => frames = v.parse().unwrap_or(0),
                "r_frame_rate" => {
                    if let Some((n, d)) = v.split_once('/') {
                        let n: f32 = n.parse().unwrap_or(30.0);
                        let d: f32 = d.parse().unwrap_or(1.0);
                        if d > 0.0 && n > 0.0 {
                            fps = n / d;
                        }
                    }
                }
                _ => {}
            }
        }
        if src_w == 0 || src_h == 0 {
            bail!("could not determine video size");
        }
        let duration = if frames > 0 { frames as f32 / fps } else { 1.0 };
        // Limit the number of frames we keep; resample longer clips.
        const MAX_FRAMES: u32 = 64;
        let (keep, out_fps) = if frames == 0 {
            (MAX_FRAMES, fps)
        } else if frames > MAX_FRAMES {
            (MAX_FRAMES, MAX_FRAMES as f32 / duration.max(0.01))
        } else {
            (frames, fps)
        };
        const MAX_PIXELS: f32 = 24.0 * 1024.0 * 1024.0;
        const MAX_SIDE: f32 = 8192.0;
        let aspect = src_w as f32 / src_h as f32;
        let per_frame = (MAX_PIXELS / keep as f32).min((src_w * src_h) as f32);
        let mut fh = (per_frame / aspect).sqrt().floor().max(1.0);
        let mut fw = (fh * aspect).floor().max(1.0);
        let mut cols = ((keep as f32 * fh / fw).sqrt().ceil() as u32).clamp(1, keep);
        let mut rows = (keep + cols - 1) / cols;
        let over = ((cols as f32 * fw) / MAX_SIDE).max((rows as f32 * fh) / MAX_SIDE);
        if over > 1.0 {
            fw = (fw / over).floor().max(1.0);
            fh = (fh / over).floor().max(1.0);
        }
        // Even dimensions keep ffmpeg's scaler happy.
        let fw = ((fw as u32) & !1).max(2);
        let fh = ((fh as u32) & !1).max(2);
        // A long clip is sampled down to `keep` frames anyway, and decoding
        // 1440 4K frames to keep 64 of them costs seconds (5.8 s for a 24 s
        // 60 fps clip here, against 2.1 s when only keyframes are decoded).
        // Keyframes are evenly spaced, so the sampled result looks the same.
        let sparse = frames > keep * 3;
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-v").arg("error");
        if sparse {
            cmd.arg("-skip_frame").arg("nokey");
        }
        let out = cmd
            .arg("-i")
            .arg(&tmp)
            .args([
                "-vf",
                &format!("fps={out_fps},scale={fw}:{fh}:flags=bicubic"),
                "-frames:v",
                &keep.to_string(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ])
            .output()
            .context("running ffmpeg (is ffmpeg installed?)")?;
        let frame_bytes = (fw * fh * 4) as usize;
        let decoded = (out.stdout.len() / frame_bytes) as u32;
        if decoded == 0 {
            bail!("ffmpeg produced no frames");
        }
        cols = ((decoded as f32 * fh as f32 / fw as f32).sqrt().ceil() as u32).clamp(1, decoded);
        rows = (decoded + cols - 1) / cols;
        let mut atlas = RgbaImage::new(cols * fw, rows * fh);
        for i in 0..decoded {
            let frame = &out.stdout[i as usize * frame_bytes..(i as usize + 1) * frame_bytes];
            let ox = (i % cols) * fw;
            let oy = (i / cols) * fh;
            for y in 0..fh {
                let src = &frame[(y * fw * 4) as usize..((y + 1) * fw * 4) as usize];
                let dst_start = (((oy + y) * cols * fw + ox) * 4) as usize;
                atlas.as_mut()[dst_start..dst_start + src.len()].copy_from_slice(src);
            }
        }
        let meta = (decoded > 1).then(|| SpritesheetMeta {
            cols,
            rows,
            frames: decoded,
            duration: (decoded as f32 / out_fps).max(0.01),
        });
        Ok((atlas, meta))
    })();
    if cached.is_none() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ---------------------------------------------------------------------------
// Decoded video-texture atlas cache
// ---------------------------------------------------------------------------

/// Bump when the atlas layout or sampling changes, so a file written by an
/// older build is not mistaken for one with the new meaning.
const ATLAS_CACHE_VERSION: u32 = 1;
/// The cache is a pure optimisation, so it may be dropped at any time.  One
/// 4K video texture atlas is ~100 MiB; keep a handful of them.
const ATLAS_CACHE_BUDGET: u64 = 512 * 1024 * 1024;
const ATLAS_CACHE_MAGIC: &[u8; 8] = b"ANIMATL1";
/// magic + width, height, cols, rows, frames + duration.
const ATLAS_HEADER: usize = 8 + 5 * 4 + 4;

fn atlas_cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("anima").join("atlas"))
}

fn atlas_cache_path(mp4: &[u8]) -> Option<PathBuf> {
    let dir = atlas_cache_dir()?;
    Some(dir.join(format!("{:016x}-{ATLAS_CACHE_VERSION}.rgba", atlas_hash(mp4))))
}

/// Extract a video stream next to the atlases, once.  `mpv` needs a file and
/// re-extracting 45 MB on every load would show up in the switch time; the
/// directory is pruned by the same budget as the atlas cache.
fn mp4_cache_file(mp4: &[u8]) -> Option<PathBuf> {
    let dir = atlas_cache_dir()?;
    let path = dir.join(format!("{:016x}-{ATLAS_CACHE_VERSION}.mp4", atlas_hash(mp4)));
    if path.is_file() {
        return Some(path);
    }
    fs::create_dir_all(&dir).ok()?;
    if let Err(err) = crate::util::atomic_write(&path, mp4) {
        log::debug!("video texture: cannot cache {}: {err}", path.display());
        return None;
    }
    Some(path)
}

/// Identity of a video payload for caching: its length plus blocks spread
/// across it.  Hashing all 45 MB on every load would cost a good part of what
/// the cache saves, and a false hit would need identical length *and*
/// identical bytes at every sampled offset.
fn atlas_hash(mp4: &[u8]) -> u64 {
    const BLOCK: usize = 4096;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    mix(&(mp4.len() as u64).to_le_bytes());
    if mp4.len() <= 32 * BLOCK {
        mix(mp4);
    } else {
        let stride = (mp4.len() / 32).max(BLOCK);
        let mut off = 0;
        while off + BLOCK <= mp4.len() {
            mix(&mp4[off..off + BLOCK]);
            off += stride;
        }
        mix(&mp4[mp4.len() - BLOCK..]);
    }
    h
}

fn read_atlas_cache(path: &Path) -> Option<(RgbaImage, Option<SpritesheetMeta>)> {
    let data = fs::read(path).ok()?;
    if data.len() < ATLAS_HEADER || &data[..8] != ATLAS_CACHE_MAGIC {
        return None;
    }
    let word = |i: usize| -> u32 {
        let at = 8 + i * 4;
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    };
    let (w, h) = (word(0), word(1));
    let (cols, rows, frames) = (word(2), word(3), word(4));
    let duration = f32::from_le_bytes([data[28], data[29], data[30], data[31]]);
    if w == 0 || h == 0 || frames == 0 {
        return None;
    }
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if data.len() != ATLAS_HEADER + expected {
        return None;
    }
    let image = RgbaImage::from_raw(w, h, data[ATLAS_HEADER..].to_vec())?;
    let meta = (frames > 1).then(|| SpritesheetMeta {
        cols,
        rows,
        frames,
        duration: duration.max(0.01),
    });
    Some((image, meta))
}

fn write_atlas_cache(
    path: &Path,
    atlas: &RgbaImage,
    meta: Option<&SpritesheetMeta>,
) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let (cols, rows, frames, duration) = match meta {
        Some(m) => (m.cols, m.rows, m.frames, m.duration),
        None => (1, 1, 1, 1.0),
    };
    let mut header = [0u8; ATLAS_HEADER];
    header[..8].copy_from_slice(ATLAS_CACHE_MAGIC);
    for (i, v) in [atlas.width(), atlas.height(), cols, rows, frames]
        .into_iter()
        .enumerate()
    {
        header[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    header[28..32].copy_from_slice(&duration.to_le_bytes());

    // Stream to a sibling file first: a half-written atlas must never be read
    // back as a valid cache entry.
    let tmp = path.with_extension("rgba.tmp");
    {
        let mut f = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
        f.write_all(&header)?;
        f.write_all(atlas.as_raw())?;
        f.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Drop the oldest cache files until the directory fits the budget again.
fn prune_atlas_cache(newest: &Path) {
    let Some(dir) = newest.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total += meta.len();
        files.push((mtime, meta.len(), entry.path()));
    }
    if total <= ATLAS_CACHE_BUDGET {
        return;
    }
    files.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, len, path) in files {
        if total <= ATLAS_CACHE_BUDGET {
            break;
        }
        if path == newest {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            log::debug!("video texture: evicted {}", path.display());
        }
    }
}

fn crop_if_needed(img: RgbaImage, img_w: u32, img_h: u32, _flags: u32) -> RgbaImage {
    if img_w > 0 && img_h > 0 && img_w <= img.width() && img_h <= img.height() {
        if img_w != img.width() || img_h != img.height() {
            return image::imageops::crop_imm(&img, 0, 0, img_w, img_h).to_image();
        }
    }
    img
}

fn is_embedded_image(buf: &[u8]) -> bool {
    if buf.len() < 12 {
        return false;
    }
    // PNG, JPEG, GIF, RIFF/WEBP, BMP
    buf.starts_with(b"\x89PNG\r\n\x1a\n")
        || buf.starts_with(b"\xFF\xD8\xFF")
        || buf.starts_with(b"GIF87a")
        || buf.starts_with(b"GIF89a")
        || (buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP")
}

/// Pure-Rust LZ4 raw block decompressor.
pub fn lz4_decompress_block(input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let cap = if expected_size > 0 {
        expected_size.min(256 * 1024 * 1024)
    } else {
        (input.len() * 4).min(64 * 1024 * 1024)
    };
    let mut out = Vec::with_capacity(cap);
    let mut pos = 0;

    while pos < input.len() {
        let token = input[pos];
        pos += 1;

        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if pos >= input.len() {
                    bail!("truncated LZ4 literal length");
                }
                let b = input[pos];
                pos += 1;
                lit_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }

        if pos + lit_len > input.len() {
            bail!("LZ4 literal overruns input buffer");
        }
        out.extend_from_slice(&input[pos..pos + lit_len]);
        pos += lit_len;

        if pos >= input.len() {
            break;
        }

        if pos + 2 > input.len() {
            bail!("truncated LZ4 match offset");
        }
        let offset = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
        pos += 2;
        if offset == 0 || offset > out.len() {
            bail!("invalid LZ4 match offset {offset} (out len {})", out.len());
        }

        let mut match_len = (token & 0x0F) as usize + 4;
        if (token & 0x0F) == 15 {
            loop {
                if pos >= input.len() {
                    bail!("truncated LZ4 match length");
                }
                let b = input[pos];
                pos += 1;
                match_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }

        let start = out.len() - offset;
        for i in 0..match_len {
            let b = out[start + i];
            out.push(b);
        }
    }

    Ok(out)
}

fn decode_rgba8(data: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let needed = (w as usize) * (h as usize) * 4;
    if data.len() < needed {
        bail!("RGBA8 buffer too short: {} < {}", data.len(), needed);
    }
    Ok(data[..needed].to_vec())
}

fn decode_rg88(data: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let pixels = (w as usize) * (h as usize);
    if data.len() < pixels * 2 {
        bail!("RG88 buffer too short");
    }
    let mut out = Vec::with_capacity(pixels * 4);
    for i in 0..pixels {
        let r = data[i * 2];
        let g = data[i * 2 + 1];
        out.extend_from_slice(&[r, r, r, g]);
    }
    Ok(out)
}

fn decode_r8(data: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let pixels = (w as usize) * (h as usize);
    if data.len() < pixels {
        bail!("R8 buffer too short");
    }
    let mut out = Vec::with_capacity(pixels * 4);
    for &v in &data[..pixels] {
        out.extend_from_slice(&[v, v, v, v]);
    }
    Ok(out)
}

fn decode_dxt1(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let bw = ((width + 3) / 4) as usize;
    let bh = ((height + 3) / 4) as usize;
    if data.len() < bw * bh * 8 {
        bail!("DXT1 buffer too short");
    }
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut offset = 0;

    for by in 0..bh {
        for bx in 0..bw {
            let c0 = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let c1 = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let bits = u32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            offset += 8;

            let palette = dxt_color_palette(c0, c1, true);
            for py in 0..4usize {
                let y = by * 4 + py;
                if y >= height as usize {
                    continue;
                }
                for px in 0..4usize {
                    let x = bx * 4 + px;
                    if x >= width as usize {
                        continue;
                    }
                    let idx = ((bits >> (2 * (py * 4 + px))) & 0x03) as usize;
                    let dst = (y * (width as usize) + x) * 4;
                    out[dst..dst + 4].copy_from_slice(&palette[idx]);
                }
            }
        }
    }
    Ok(out)
}

fn decode_dxt3(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let bw = ((width + 3) / 4) as usize;
    let bh = ((height + 3) / 4) as usize;
    if data.len() < bw * bh * 16 {
        bail!("DXT3 buffer too short");
    }
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut offset = 0;

    for by in 0..bh {
        for bx in 0..bw {
            let alpha_bits = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
            let c0 = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
            let c1 = u16::from_le_bytes([data[offset + 10], data[offset + 11]]);
            let bits = u32::from_le_bytes([
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);
            offset += 16;

            let palette = dxt_color_palette(c0, c1, false);
            for py in 0..4usize {
                let y = by * 4 + py;
                if y >= height as usize {
                    continue;
                }
                for px in 0..4usize {
                    let x = bx * 4 + px;
                    if x >= width as usize {
                        continue;
                    }
                    let p_idx = py * 4 + px;
                    let c_idx = ((bits >> (2 * p_idx)) & 0x03) as usize;
                    let a4 = ((alpha_bits >> (4 * p_idx)) & 0x0F) as u8;
                    let dst = (y * (width as usize) + x) * 4;
                    out[dst] = palette[c_idx][0];
                    out[dst + 1] = palette[c_idx][1];
                    out[dst + 2] = palette[c_idx][2];
                    out[dst + 3] = a4 | (a4 << 4);
                }
            }
        }
    }
    Ok(out)
}

fn decode_dxt5(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let bw = ((width + 3) / 4) as usize;
    let bh = ((height + 3) / 4) as usize;
    if data.len() < bw * bh * 16 {
        bail!("DXT5 buffer too short");
    }
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut offset = 0;

    for by in 0..bh {
        for bx in 0..bw {
            let a0 = data[offset];
            let a1 = data[offset + 1];
            let mut alpha_bits: u64 = 0;
            for i in 0..6 {
                alpha_bits |= (data[offset + 2 + i] as u64) << (8 * i);
            }
            let alphas = dxt5_alpha_palette(a0, a1);

            let c0 = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
            let c1 = u16::from_le_bytes([data[offset + 10], data[offset + 11]]);
            let bits = u32::from_le_bytes([
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);
            offset += 16;

            let palette = dxt_color_palette(c0, c1, false);
            for py in 0..4usize {
                let y = by * 4 + py;
                if y >= height as usize {
                    continue;
                }
                for px in 0..4usize {
                    let x = bx * 4 + px;
                    if x >= width as usize {
                        continue;
                    }
                    let p_idx = py * 4 + px;
                    let c_idx = ((bits >> (2 * p_idx)) & 0x03) as usize;
                    let a_idx = ((alpha_bits >> (3 * p_idx)) & 0x07) as usize;
                    let dst = (y * (width as usize) + x) * 4;
                    out[dst] = palette[c_idx][0];
                    out[dst + 1] = palette[c_idx][1];
                    out[dst + 2] = palette[c_idx][2];
                    out[dst + 3] = alphas[a_idx];
                }
            }
        }
    }
    Ok(out)
}

fn rgb565(c: u16) -> [u8; 3] {
    let r = ((c >> 11) & 0x1F) as u8;
    let g = ((c >> 5) & 0x3F) as u8;
    let b = (c & 0x1F) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    ]
}

fn dxt_color_palette(c0: u16, c1: u16, dxt1: bool) -> [[u8; 4]; 4] {
    let [r0, g0, b0] = rgb565(c0);
    let [r1, g1, b1] = rgb565(c1);
    let mut p = [[0u8; 4]; 4];
    p[0] = [r0, g0, b0, 255];
    p[1] = [r1, g1, b1, 255];
    if !dxt1 || c0 > c1 {
        p[2] = [
            ((2 * r0 as u16 + r1 as u16) / 3) as u8,
            ((2 * g0 as u16 + g1 as u16) / 3) as u8,
            ((2 * b0 as u16 + b1 as u16) / 3) as u8,
            255,
        ];
        p[3] = [
            ((r0 as u16 + 2 * r1 as u16) / 3) as u8,
            ((g0 as u16 + 2 * g1 as u16) / 3) as u8,
            ((b0 as u16 + 2 * b1 as u16) / 3) as u8,
            255,
        ];
    } else {
        p[2] = [
            ((r0 as u16 + r1 as u16) / 2) as u8,
            ((g0 as u16 + g1 as u16) / 2) as u8,
            ((b0 as u16 + b1 as u16) / 2) as u8,
            255,
        ];
        p[3] = [0, 0, 0, 0];
    }
    p
}

fn dxt5_alpha_palette(a0: u8, a1: u8) -> [u8; 8] {
    let mut a = [0u8; 8];
    a[0] = a0;
    a[1] = a1;
    if a0 > a1 {
        for i in 1..7 {
            a[i + 1] = (((7 - i as u16) * a0 as u16 + (i as u16) * a1 as u16) / 7) as u8;
        }
    } else {
        for i in 1..5 {
            a[i + 1] = (((5 - i as u16) * a0 as u16 + (i as u16) * a1 as u16) / 5) as u8;
        }
        a[6] = 0;
        a[7] = 255;
    }
    a
}

struct BinReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BinReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos.saturating_add(len) > self.data.len() {
            bail!("unexpected EOF at offset {} (need {len} bytes)", self.pos);
        }
        let s = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(s)
    }

    fn read_u32(&mut self) -> Result<u32> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let b = self.read_bytes(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_len_prefixed_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        if len > 65536 {
            bail!("string length too large: {len}");
        }
        let b = self.read_bytes(len)?;
        Ok(String::from_utf8_lossy(b).into_owned())
    }

    fn read_c_magic(&mut self, max_len: usize) -> Result<String> {
        let b = self.read_bytes(max_len)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(max_len);
        Ok(String::from_utf8_lossy(&b[..end]).into_owned())
    }
}

#[allow(dead_code)]
fn _cursor_unused(_: Cursor<&[u8]>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkg_roundtrip_parse() {
        let mut buf = Vec::new();
        // Magic "PKGV0002"
        buf.extend_from_slice(&(8u32).to_le_bytes());
        buf.extend_from_slice(b"PKGV0002");
        // 1 file
        buf.extend_from_slice(&(1u32).to_le_bytes());
        // path = "scene.json"
        let path = b"scene.json";
        buf.extend_from_slice(&(path.len() as u32).to_le_bytes());
        buf.extend_from_slice(path);
        let payload = br#"{"general":{"clearcolor":"0.1 0.2 0.3"}}"#;
        buf.extend_from_slice(&(0u32).to_le_bytes()); // offset
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);

        let parsed = parse_pkg_bytes(&buf).unwrap();
        assert_eq!(parsed.get("scene.json").unwrap(), payload);
    }

    #[test]
    fn test_lz4_decompress_block() {
        // Simple literal + match LZ4 block:
        // Token: 4 literals (0x40), match_len = 4 (0x00) -> 0x40
        // Literals: "ABCD"
        // Offset: 4 (0x04, 0x00) -> repeats "ABCD"
        let compressed = [0x40u8, b'A', b'B', b'C', b'D', 0x04, 0x00];
        let out = lz4_decompress_block(&compressed, 8).unwrap();
        assert_eq!(&out, b"ABCDABCD");
    }

    #[test]
    fn test_tex_rgba8_decode() {
        let mut tex = Vec::new();
        tex.extend_from_slice(b"TEXV0005\0");
        tex.extend_from_slice(b"TEXI0001\0");
        tex.extend_from_slice(&0u32.to_le_bytes()); // format 0 = RGBA8
        tex.extend_from_slice(&0u32.to_le_bytes()); // flags
        tex.extend_from_slice(&2u32.to_le_bytes()); // tex_w = 2
        tex.extend_from_slice(&2u32.to_le_bytes()); // tex_h = 2
        tex.extend_from_slice(&2u32.to_le_bytes()); // img_w = 2
        tex.extend_from_slice(&2u32.to_le_bytes()); // img_h = 2
        tex.extend_from_slice(&0u32.to_le_bytes()); // unk
        tex.extend_from_slice(b"TEXB0003\0");
        tex.extend_from_slice(&1u32.to_le_bytes()); // image_count = 1
        tex.extend_from_slice(&(-1i32).to_le_bytes()); // free_image_format
        tex.extend_from_slice(&1u32.to_le_bytes()); // mipmap_count = 1
        tex.extend_from_slice(&2u32.to_le_bytes()); // mip_w = 2
        tex.extend_from_slice(&2u32.to_le_bytes()); // mip_h = 2
        tex.extend_from_slice(&0u32.to_le_bytes()); // lz4 = 0
        tex.extend_from_slice(&16u32.to_le_bytes()); // decomp_size
        tex.extend_from_slice(&16u32.to_le_bytes()); // byte_count = 16
        let pixels = [255u8, 0, 0, 255].repeat(4);
        tex.extend_from_slice(&pixels);

        let decoded = decode_tex(&tex).unwrap();
        assert_eq!(decoded.image.dimensions(), (2, 2));
        assert_eq!(decoded.image.as_raw(), &pixels[..]);
    }
    /// `TEXV0005` container holding `payload` as its only mipmap.
    fn tex_with_payload(flags: u32, img_w: u32, img_h: u32, payload: &[u8]) -> Vec<u8> {
        let mut tex = Vec::new();
        tex.extend_from_slice(b"TEXV0005\0");
        tex.extend_from_slice(b"TEXI0001\0");
        tex.extend_from_slice(&0u32.to_le_bytes()); // format 0 = RGBA8
        tex.extend_from_slice(&flags.to_le_bytes());
        tex.extend_from_slice(&img_w.to_le_bytes()); // tex_w
        tex.extend_from_slice(&img_h.to_le_bytes()); // tex_h
        tex.extend_from_slice(&img_w.to_le_bytes()); // img_w
        tex.extend_from_slice(&img_h.to_le_bytes()); // img_h
        tex.extend_from_slice(&0u32.to_le_bytes()); // unk
        tex.extend_from_slice(b"TEXB0003\0");
        tex.extend_from_slice(&1u32.to_le_bytes()); // image_count = 1
        tex.extend_from_slice(&(-1i32).to_le_bytes()); // free_image_format
        tex.extend_from_slice(&1u32.to_le_bytes()); // mipmap_count = 1
        tex.extend_from_slice(&img_w.to_le_bytes()); // mip_w
        tex.extend_from_slice(&img_h.to_le_bytes()); // mip_h
        tex.extend_from_slice(&0u32.to_le_bytes()); // lz4 = 0
        tex.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // decomp_size
        tex.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // byte_count
        tex.extend_from_slice(payload);
        tex
    }

    fn tex_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("anima-tex-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn texture_candidates_try_the_exact_name_before_generated_paths() {
        let cands = texture_candidates("materials/video.tex");
        assert_eq!(cands[0], "materials/video.tex");
        assert!(cands.contains(&"materials/materials/video.tex".to_string()));

        // A missing extension is completed under `materials/` and the root.
        let cands = texture_candidates("grid");
        assert_eq!(cands[0], "grid");
        assert!(cands.contains(&"materials/grid.tex".to_string()));
        assert!(cands.contains(&"grid.png".to_string()));
    }

    #[test]
    fn is_mp4_recognises_streams_only() {
        assert!(is_mp4(b"\0\0\0\x18ftypisom\0\0\0\0"));
        assert!(!is_mp4(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR"));
        // Too short to hold both the size and the `ftyp` box name.
        assert!(!is_mp4(b"\0\0\0\x18fty"));
    }

    #[test]
    fn split_tex_exposes_the_video_payload_untouched() {
        let mp4 = b"\0\0\0\x18ftypisom\0\0\0\x08free".to_vec();
        // flags bit 5 marks a video texture; the size hints come from the header.
        let tex = tex_with_payload(32, 3840, 2160, &mp4);
        let (hdr, payload) = split_tex(&tex).unwrap();
        assert_eq!((hdr.img_width, hdr.img_height), (3840, 2160));
        assert_eq!(hdr.flags & 32, 32, "video flag survives the split");
        assert_eq!(payload, mp4);
    }

    #[test]
    fn video_file_skips_regular_images() {
        let dir = tex_temp_dir("no-video");
        let pixels = [255u8, 0, 0, 255].repeat(4);
        fs::write(dir.join("layer.tex"), tex_with_payload(0, 2, 2, &pixels)).unwrap();
        let pkg = PackageFs::open(&dir).unwrap();
        assert!(pkg.video_file("layer").is_none());
        assert!(pkg.video_file("does-not-exist").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    fn atlas_temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("anima-atlas-{tag}-{}", std::process::id()))
    }

    #[test]
    fn atlas_cache_roundtrip_restores_frames_and_meta() {
        let path = atlas_temp("roundtrip");
        let mut img = RgbaImage::new(4, 2);
        for (i, px) in img.pixels_mut().enumerate() {
            px.0 = [i as u8, 7, 9, 255];
        }
        let meta = SpritesheetMeta {
            cols: 2,
            rows: 1,
            frames: 2,
            duration: 3.5,
        };
        write_atlas_cache(&path, &img, Some(&meta)).unwrap();
        let (back, meta_back) = read_atlas_cache(&path).unwrap();
        assert_eq!(back.dimensions(), (4, 2));
        assert_eq!(back.as_raw(), img.as_raw());
        let meta_back = meta_back.unwrap();
        assert_eq!((meta_back.cols, meta_back.rows, meta_back.frames), (2, 1, 2));
        assert!((meta_back.duration - 3.5).abs() < 1e-6);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn atlas_cache_rejects_truncated_or_foreign_files() {
        let path = atlas_temp("bad");
        // not ours
        fs::write(&path, b"definitely not an atlas").unwrap();
        assert!(read_atlas_cache(&path).is_none());
        // right magic, wrong payload length
        let mut short = Vec::new();
        short.extend_from_slice(ATLAS_CACHE_MAGIC);
        for v in [8u32, 8, 1, 1, 1] {
            short.extend_from_slice(&v.to_le_bytes());
        }
        short.extend_from_slice(&1.0f32.to_le_bytes());
        short.extend_from_slice(&[0u8; 16]);
        fs::write(&path, &short).unwrap();
        assert!(read_atlas_cache(&path).is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn atlas_hash_tracks_length_and_content() {
        let a = vec![7u8; 4096 * 40];
        let mut b = a.clone();
        assert_eq!(atlas_hash(&a), atlas_hash(&b));
        b[0] ^= 0xFF;
        assert_ne!(atlas_hash(&a), atlas_hash(&b));
        let mut c = a.clone();
        c.push(1);
        assert_ne!(atlas_hash(&a), atlas_hash(&c));
    }
}
