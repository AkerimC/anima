//! Voice lines for characters: a `voice/` folder next to the character's
//! file, with files named by cue (`pat_1.ogg`) or by Blue Archive's line
//! names (`contrib/ba-voice.py` fetches those, see `crate::blue_archive`).
//! A character never talks over herself; different characters talk over
//! each other freely.
//!
//! Sound effects shared by everyone (`~/.local/share/anima/sounds/`:
//! `pick_up.wav`, `release.wav`, or the game's, `contrib/ba-voice.py
//! --effects`) play on top, whenever they happen.
//!
//! Sounds come from where the character is: panned by its place on the
//! screen and quieter the farther it is from the cursor, following it while
//! it moves ([`Spot`]). They're decoded here (WAV, Ogg Vorbis) and streamed
//! to pw-play, or paplay, as raw stereo.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A moment a character can say something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cue {
    Hello,
    Pat,
    /// landing after being thrown hard
    Hurt,
    Victory,
    Ex,
    Gesture,
    Chase,
    Idle,
}


// sounds of ours playing right now (the desktop audio monitor hears them)
static SOUNDING: AtomicUsize = AtomicUsize::new(0);

/// Whether any voice line or effect of ours is playing.
pub fn sounding() -> bool {
    SOUNDING.load(Ordering::Acquire) > 0
}

/// Where a character's sounds come from: left/right gain, updated every
/// frame from its position (see [`Spot::set`]) and read while they play.
#[derive(Clone, Default)]
pub struct Spot(Arc<Mutex<[f32; 2]>>);

impl Spot {
    /// `x`: the character's horizontal centre, 0..1 across the screen (off
    /// screen beyond that); `distance`: from the cursor, in pixels.
    pub fn set(&self, x: f32, distance: f32) {
        // equal-power pan by place on screen, halving about every 800 px
        let pan = (x.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).clamp(0.0, std::f32::consts::FRAC_PI_2);
        let near = 1.0 / (1.0 + distance.max(0.0) / 800.0);
        *self.0.lock().unwrap() = [pan.cos() * near * std::f32::consts::SQRT_2, pan.sin() * near * std::f32::consts::SQRT_2].map(|g| g.min(1.0));
    }

    fn gains(&self) -> [f32; 2] {
        *self.0.lock().unwrap()
    }
}

pub struct Voice {
    lines: Vec<(Cue, PathBuf)>,
    // this character is saying something
    speaking: Arc<AtomicBool>,
    next_idle: Instant,
    seed: u32,
}

/// Which cue a file is for: Blue Archive's line names
/// (`crate::blue_archive::voice_cue`), or the cue's own name (`pat.ogg`,
/// `hello_2.wav`, ...).
fn cue_of(path: &Path) -> Option<Cue> {
    let stem = path.file_stem()?.to_str()?;
    let mut base = stem;
    while let Some((head, tail)) = base.rsplit_once('_') {
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        base = head;
    }
    crate::blue_archive::voice_cue(base).or_else(|| {
        let cue = match base.to_ascii_lowercase().as_str() {
            "hello" => Cue::Hello,
            "pat" => Cue::Pat,
            "hurt" => Cue::Hurt,
            "victory" => Cue::Victory,
            "ex" => Cue::Ex,
            "gesture" => Cue::Gesture,
            "chase" => Cue::Chase,
            "idle" => Cue::Idle,
            _ => return None,
        };
        Some(cue)
    })
}

impl Voice {
    /// The `voice/` folder next to `character` (a model or sprite file).
    pub fn load(character: &Path) -> Option<Self> {
        let dir = character.parent()?.join("voice");
        let mut lines: Vec<(Cue, PathBuf)> = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ogg") || x.eq_ignore_ascii_case("wav")))
            .filter_map(|p| Some((cue_of(&p)?, p)))
            .collect();
        if lines.is_empty() {
            return None;
        }
        lines.sort_by(|a, b| a.1.cmp(&b.1));
        log::info!("voice: {} lines in {}", lines.len(), dir.display());
        let now = Instant::now();
        let seed = character.as_os_str().len() as u32 ^ std::process::id().rotate_left(7);
        let mut v = Self { lines, speaking: Arc::default(), next_idle: now, seed };
        v.next_idle = now + v.idle_gap();
        Some(v)
    }

    fn rand(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.seed >> 8) as f32 / (1u32 << 24) as f32
    }

    fn idle_gap(&mut self) -> Duration {
        Duration::from_secs_f32(90.0 + self.rand() * 150.0)
    }

    /// Say a line for `cue` from `spot` unless this character is still
    /// talking or has no such line. True if it plays.
    pub fn say(&mut self, cue: Cue, volume: f32, spot: &Spot) -> bool {
        let picks: Vec<usize> = (0..self.lines.len()).filter(|&i| self.lines[i].0 == cue).collect();
        if picks.is_empty() || self.speaking.load(Ordering::Acquire) {
            return false;
        }
        self.speaking.store(true, Ordering::Release);
        let i = picks[((self.rand() * picks.len() as f32) as usize).min(picks.len() - 1)];
        let speaking = self.speaking.clone();
        play(self.lines[i].1.clone(), volume, spot.clone(), move || speaking.store(false, Ordering::Release));
        true
    }

    /// While idling: a monologue every couple of minutes.
    pub fn idle(&mut self, volume: f32, spot: &Spot) {
        let now = Instant::now();
        if now >= self.next_idle {
            self.next_idle = now + self.idle_gap();
            self.say(Cue::Idle, volume, spot);
        }
    }
}

/// A sound effect, the same for every character.
#[derive(Clone, Copy, Debug)]
pub enum Effect {
    PickUp,
    Release,
}

impl Effect {
    /// Files to look for in `~/.local/share/anima/sounds/`: the effect's own
    /// name, then Blue Archive's.
    fn files(self) -> [&'static str; 2] {
        let own = match self {
            Effect::PickUp => "pick_up.wav",
            Effect::Release => "release.wav",
        };
        [own, crate::blue_archive::effect_file(self)]
    }
}

/// Play `effect` from `spot` if its file is there; doesn't wait for voice
/// lines.
pub fn effect(effect: Effect, volume: f32, spot: &Spot) {
    let Some(dir) = dirs::data_dir().map(|d| d.join("anima/sounds")) else { return };
    if let Some(path) = effect.files().iter().map(|f| dir.join(f)).find(|p| p.is_file()) {
        play(path, volume, spot.clone(), || {});
    }
}

/// Decode and stream `path` on its own thread; `done` runs when it's over.
fn play(path: PathBuf, volume: f32, spot: Spot, done: impl FnOnce() + Send + 'static) {
    SOUNDING.fetch_add(1, Ordering::AcqRel);
    let done = move || {
        // a little longer: the player's buffer still drains after we're done
        std::thread::sleep(Duration::from_millis(300));
        SOUNDING.fetch_sub(1, Ordering::AcqRel);
        done();
    };
    let done = Arc::new(Mutex::new(Some(done)));
    let finish = done.clone();
    let spawned = std::thread::Builder::new().name("anima-sound".into()).spawn(move || {
        if let Err(e) = stream(&path, volume, &spot) {
            log::debug!("sound {}: {e:#}", path.display());
        }
        if let Some(f) = finish.lock().unwrap().take() {
            f();
        }
    });
    if spawned.is_err() {
        if let Some(f) = done.lock().unwrap().take() {
            f();
        }
    }
}

fn stream(path: &Path, volume: f32, spot: &Spot) -> anyhow::Result<()> {
    let (rate, pcm) = decode(path)?;
    let rate_s = rate.to_string();
    let spawn = |cmd: &mut Command| cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let mut child = spawn(Command::new("pw-play").args(["--raw", "--format", "f32", "--channels", "2", "--rate", &rate_s, "-"]))
        .or_else(|_| spawn(Command::new("paplay").args(["--raw", "--format=float32le", "--channels=2", &format!("--rate={rate}")])))?;
    let mut out = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    // 20 ms at a time, the gains ramped across each chunk so moving doesn't click
    let chunk = (rate as usize / 50).max(1);
    let v = volume.clamp(0.0, 1.0);
    let mut g = spot.gains().map(|x| x * v);
    let mut bytes = Vec::with_capacity(chunk * 8);
    for frames in pcm.chunks(chunk * 2) {
        let target = spot.gains().map(|x| x * v);
        let n = (frames.len() / 2).max(1) as f32;
        bytes.clear();
        for (i, f) in frames.chunks_exact(2).enumerate() {
            let k = i as f32 / n;
            let l = f[0] * (g[0] + (target[0] - g[0]) * k);
            let r = f[1] * (g[1] + (target[1] - g[1]) * k);
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
        g = target;
        if out.write_all(&bytes).is_err() {
            break;
        }
    }
    drop(out);
    child.wait()?;
    Ok(())
}

/// Sample rate and interleaved stereo samples of a WAV (PCM) or Ogg Vorbis
/// file; mono is doubled, extra channels dropped.
fn decode(path: &Path) -> anyhow::Result<(u32, Vec<f32>)> {
    let is_ogg = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("ogg"));
    let (rate, channels, samples) = if is_ogg {
        let mut ogg = lewton::inside_ogg::OggStreamReader::new(BufReader::new(File::open(path)?))?;
        let (rate, channels) = (ogg.ident_hdr.audio_sample_rate, ogg.ident_hdr.audio_channels as usize);
        let mut samples = Vec::new();
        while let Some(packet) = ogg.read_dec_packet_itl()? {
            samples.extend(packet.into_iter().map(|s| s as f32 / 32768.0));
        }
        (rate, channels, samples)
    } else {
        decode_wav(path)?
    };
    let channels = channels.max(1);
    let stereo = samples
        .chunks_exact(channels)
        .flat_map(|f| [f[0], if channels > 1 { f[1] } else { f[0] }])
        .collect();
    Ok((rate, stereo))
}

/// 16-bit PCM WAV: (rate, channels, samples).
fn decode_wav(path: &Path) -> anyhow::Result<(u32, usize, Vec<f32>)> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    anyhow::ensure!(data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE", "not a WAV file");
    let (mut rate, mut channels, mut bits) = (0u32, 0usize, 0u16);
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into()?) as usize;
        let body = &data[pos + 8..(pos + 8 + len).min(data.len())];
        if id == b"fmt " && body.len() >= 16 {
            anyhow::ensure!(u16::from_le_bytes([body[0], body[1]]) == 1, "only PCM WAV");
            channels = u16::from_le_bytes([body[2], body[3]]) as usize;
            rate = u32::from_le_bytes(body[4..8].try_into()?);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            anyhow::ensure!(bits == 16, "only 16-bit WAV");
            let samples = body.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0).collect();
            return Ok((rate, channels, samples));
        }
        pos += 8 + len + (len & 1);
    }
    anyhow::bail!("no audio data")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_names() {
        let c = |n: &str| cue_of(Path::new(&format!("Hoshino_(Swimsuit)_{n}.ogg")));
        assert_eq!(c("Lobby_3"), Some(Cue::Pat));
        assert_eq!(c("EventLobby_1"), None);
        assert_eq!(c("MemorialLobby_2_1"), None);
        assert_eq!(c("ExSkill_2"), Some(Cue::Ex));
        assert_eq!(c("ExSkill_Level_1"), None);
        assert_eq!(c("LogIn_1"), Some(Cue::Hello));
        assert_eq!(c("Battle_Victory_2"), Some(Cue::Victory));
        assert_eq!(c("Battle_Damage_3"), Some(Cue::Hurt));
        assert_eq!(c("Cafe_monolog_5"), Some(Cue::Idle));
        assert_eq!(c("Title"), None);
        assert_eq!(cue_of(Path::new("pat_2.ogg")), Some(Cue::Pat));
        assert_eq!(cue_of(Path::new("hello.wav")), Some(Cue::Hello));
    }

    #[test]
    fn pan_and_distance() {
        let s = Spot::default();
        s.set(0.0, 0.0);
        let [l, r] = s.gains();
        assert!(l > 0.99 && r < 0.01);
        s.set(0.5, 0.0);
        let [l, r] = s.gains();
        assert!((l - r).abs() < 1e-4 && l > 0.99);
        s.set(0.5, 800.0);
        assert!((s.gains()[0] - 0.5).abs() < 0.01);
    }
}
