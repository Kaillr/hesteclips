//! Non-destructive clip editing: probe, decode for preview, and render edits.
//!
//! An edit never touches the original recording: it's an [`Edit`] (trim range and
//! per-track volume) rendered into a new file. Where the original, the edit and
//! the render live is the app's business; this crate only probes and renders.
//!
//! Backed by the `ffmpeg`/`ffprobe` binaries ([`ffmpeg`], [`ffprobe`]).

mod mp4;
pub mod pcm;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Min, max and mean square of every `per` samples: the per-sample pass of a
/// waveform, here so it runs optimized even in debug builds (millions of
/// samples per track).
pub fn sample_peaks(samples: &[f32], per: usize) -> Vec<[f32; 3]> {
    samples
        .chunks(per.max(1))
        .map(|c| {
            let (mut lo, mut hi, mut sq) = (f32::MAX, f32::MIN, 0.0f32);
            for &s in c {
                lo = lo.min(s);
                hi = hi.max(s);
                sq += s * s;
            }
            [lo, hi, sq / c.len() as f32]
        })
        .collect()
}

/// RGBA pixels (`width` x `height`, alpha ignored) as a JPEG.
pub fn encode_jpeg(rgba: &[u8], width: u16, height: u16, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(64 * 1024);
    jpeg_encoder::Encoder::new(&mut out, quality).encode(rgba, width, height, jpeg_encoder::ColorType::Rgba).ok()?;
    Some(out)
}

/// Sample rate audio is decoded at for preview playback and waveforms.
pub const PREVIEW_RATE: u32 = 48_000;

/// An `ffmpeg` command. See [`tool`].
pub fn ffmpeg() -> Command {
    tool("ffmpeg")
}

/// An `ffmpeg` for background work (scrub frames, thumbnails): at lower CPU
/// priority, so playback and the game always come first.
pub fn ffmpeg_background() -> Command {
    let mut cmd = tool("ffmpeg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    #[cfg(unix)]
    {
        // `nice` in front of it: the same, without unsafe pre-exec hooks.
        let program = cmd.get_program().to_owned();
        let mut niced = Command::new("nice");
        niced.args(["-n", "10"]).arg(program);
        cmd = niced;
    }
    cmd
}

/// An `ffprobe` command. See [`tool`].
pub fn ffprobe() -> Command {
    tool("ffprobe")
}

/// A command for one of the ffmpeg tools: a copy shipped next to the app wins
/// over the one on `PATH`. On Windows it runs without a console window (the
/// app is a GUI program, so each run would otherwise flash one).
pub fn tool(name: &str) -> Command {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name).with_extension(std::env::consts::EXE_EXTENSION)))
        .filter(|p| p.is_file());
    #[allow(unused_mut)]
    let mut cmd = Command::new(bundled.as_deref().unwrap_or(Path::new(name)));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Probed facts about a clip.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    /// The video's bitrate, when the file says.
    pub video_kbps: Option<u32>,
    pub audio: Vec<AudioStream>,
    /// Permanent id from the clip tag, once the clip has one (see [`clip_tag`]).
    pub id: Option<String>,
    /// Its colours are stored with BT.601's matrix (standard definition),
    /// not BT.709's (ours, and HD's).
    pub bt601: bool,
}

impl ClipInfo {
    pub fn frame_duration(&self) -> f64 {
        1.0 / self.fps
    }

    /// Snap a time to the start of the frame that contains it.
    pub fn snap(&self, t: f64) -> f64 {
        let f = (t * self.fps + 1e-6).floor();
        (f / self.fps).clamp(0.0, self.duration)
    }

    pub fn frame_index(&self, t: f64) -> u64 {
        (t * self.fps + 1e-6).floor().max(0.0) as u64
    }

    /// The source tracks you can mix. Our recordings put a premixed "Mix" on
    /// track 1 followed by the separate tracks; the mix is rebuilt from them on
    /// export, so only those are offered. Files without that layout expose every
    /// audio stream.
    pub fn source_tracks(&self) -> Vec<&AudioStream> {
        let has_mix = self.audio.first().is_some_and(|a| a.title.as_deref() == Some("Mix"));
        if has_mix && self.audio.len() > 1 {
            self.audio[1..].iter().collect()
        } else {
            self.audio.iter().collect()
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioStream {
    /// Index among the file's audio streams (`0:a:N`).
    pub index: usize,
    pub title: Option<String>,
    /// Part of the clip's mix. False for tracks recorded only for editing (e.g.
    /// a voice chat kept out of shared clips): the rebuilt mix leaves them out
    /// unless you choose to add them.
    pub in_mix: bool,
}

impl AudioStream {
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| format!("Track {}", self.index + 1))
    }
}

/// What's in a clip: MP4 and MOV (ours, and most others) read here from
/// their index, anything else by ffprobe. HASTECLIP_FFPROBE=1 asks ffprobe
/// every time, to compare.
pub fn probe(source: &Path) -> Result<ClipInfo> {
    if std::env::var_os("HASTECLIP_FFPROBE").is_none()
        && let Ok(info) = probe_mp4(source)
    {
        return Ok(info);
    }
    probe_ffprobe(source)
}

/// [`probe`] from an MP4's or MOV's index, the same facts ffprobe gives.
fn probe_mp4(source: &Path) -> Result<ClipInfo> {
    let movie = mp4::read(source)?;
    let video = movie.tracks.iter().find(|t| t.kind == mp4::Kind::Video).context("no video stream")?;
    let tag = movie.comment.as_deref().map(parse_clip_tag).unwrap_or_default();
    let audio = movie
        .tracks
        .iter()
        .filter(|t| t.kind == mp4::Kind::Audio)
        .enumerate()
        .map(|(index, t)| AudioStream {
            index,
            title: t.name.clone().filter(|n| n != "SoundHandler" && n != "AudioHandler"),
            in_mix: tag.mix.as_ref().is_none_or(|m| m.contains(&index)),
        })
        .collect();
    Ok(ClipInfo {
        duration: movie.seconds,
        fps: if video.samples > 0 && video.seconds > 0.0 { video.samples as f64 / video.seconds } else { 60.0 },
        width: video.width,
        height: video.height,
        video_kbps: (video.seconds > 0.0).then(|| (video.bytes as f64 * 8.0 / video.seconds / 1000.0) as u32).filter(|&k| k > 0),
        audio,
        id: tag.id,
        // As ffprobe's colour space: BT.601's matrices, else untagged (2 is
        // "unspecified") guessed by its size, as players do.
        bt601: match video.matrix {
            Some(5 | 6) => true,
            Some(2) | None => video.height < 720,
            Some(_) => false,
        },
    })
}

/// [`probe`] by ffprobe.
fn probe_ffprobe(source: &Path) -> Result<ClipInfo> {
    let out = ffprobe()
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration:format_tags=comment:stream=index,codec_type,width,height,avg_frame_rate,r_frame_rate,bit_rate,color_space:stream_tags=title,handler_name,name")
        .args(["-of", "json"])
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .context("failed to run ffprobe — is ffmpeg installed?")?;
    if !out.status.success() {
        bail!("couldn't read this video");
    }

    #[derive(Deserialize)]
    struct Probe {
        streams: Vec<Stream>,
        format: Format,
    }
    #[derive(Deserialize)]
    struct Format {
        duration: Option<String>,
        #[serde(default)]
        tags: FormatTags,
    }
    #[derive(Deserialize, Default)]
    struct FormatTags {
        /// MP4 lowercases it, MKV doesn't.
        #[serde(alias = "COMMENT")]
        comment: Option<String>,
    }
    #[derive(Deserialize)]
    struct Stream {
        codec_type: String,
        width: Option<u32>,
        height: Option<u32>,
        avg_frame_rate: Option<String>,
        r_frame_rate: Option<String>,
        bit_rate: Option<String>,
        color_space: Option<String>,
        #[serde(default)]
        tags: Tags,
    }
    #[derive(Deserialize, Default)]
    struct Tags {
        title: Option<String>,
        handler_name: Option<String>,
        /// Some muxers (e.g. the mp4 "segment" path) store the name here.
        name: Option<String>,
    }

    let p: Probe = serde_json::from_slice(&out.stdout).context("unexpected ffprobe output")?;
    let duration: f64 = p.format.duration.as_deref().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    // Older recordings have no tag: every track after the mix was in it.
    let tag = p.format.tags.comment.as_deref().map(parse_clip_tag).unwrap_or_default();
    let mix_members = tag.mix;
    let video = p.streams.iter().find(|s| s.codec_type == "video").context("no video stream")?;
    let fps = [&video.avg_frame_rate, &video.r_frame_rate]
        .into_iter()
        .flatten()
        .filter_map(|r| parse_rate(r))
        .next()
        .unwrap_or(60.0);
    let audio = p
        .streams
        .iter()
        .filter(|s| s.codec_type == "audio")
        .enumerate()
        .map(|(index, s)| AudioStream {
            index,
            // MKV carries our names in title; MP4 in handler_name or name. Ignore
            // the generic handler names muxers write by default.
            title: [&s.tags.title, &s.tags.name, &s.tags.handler_name]
                .into_iter()
                .flatten()
                .find(|t| !t.is_empty() && *t != "SoundHandler" && *t != "AudioHandler")
                .cloned(),
            in_mix: mix_members.as_ref().is_none_or(|m| m.contains(&index)),
        })
        .collect();
    Ok(ClipInfo {
        duration,
        fps,
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        video_kbps: video.bit_rate.as_deref().and_then(|b| b.parse::<u64>().ok()).map(|b| (b / 1000) as u32).filter(|&k| k > 0),
        audio,
        id: tag.id,
        // Untagged: as players guess, by its size.
        bt601: match video.color_space.as_deref() {
            Some("bt470bg" | "smpte170m") => true,
            Some("unknown") | None => video.height.is_some_and(|h| h < 720),
            Some(_) => false,
        },
    })
}

/// What our file comment says about a clip.
#[derive(Debug, Default, PartialEq)]
pub struct ClipTag {
    /// Audio streams that make up the mix (`0:a:N` indices).
    pub mix: Option<Vec<usize>>,
    /// Permanent id, tying the file to its assets (original, edit) wherever it's
    /// moved or renamed.
    pub id: Option<String>,
}

/// What clips from the app's earlier name carry in front of the same fields.
const LEGACY_TAG: &str = concat!("heste", "clips:");

/// The file comment for a clip: `hasteclip:mix=1,2 id=3f9c…`. Recordings start
/// with just the mix; the id is added the first time the clip is edited.
pub fn clip_tag(mix: &[usize], id: Option<&str>) -> String {
    let list: Vec<String> = mix.iter().map(|n| n.to_string()).collect();
    let mut tag = format!("hasteclip:mix={}", list.join(","));
    if let Some(id) = id {
        tag += &format!(" id={id}");
    }
    tag
}

/// The clip tag of a render of `edit`: mix members in the output's own layout
/// (track 1 is the new mix, then each source in order), so reopening the render
/// starts with the same tracks muted.
pub fn render_tag(edit: &Edit, id: Option<&str>) -> String {
    let mix: Vec<usize> = edit.tracks.iter().enumerate().filter(|(_, t)| !t.muted).map(|(i, _)| i + 1).collect();
    clip_tag(&mix, id)
}

pub fn parse_clip_tag(comment: &str) -> ClipTag {
    // The older spelling of the tag (the first versions of this app, and the
    // clips imported from them) reads the same.
    let Some(rest) = comment.strip_prefix("hasteclip:").or_else(|| comment.strip_prefix(LEGACY_TAG)) else { return ClipTag::default() };
    let mut tag = ClipTag::default();
    for field in rest.split_whitespace() {
        if let Some(list) = field.strip_prefix("mix=") {
            tag.mix = Some(list.split(',').filter_map(|n| n.trim().parse().ok()).collect());
        } else if let Some(id) = field.strip_prefix("id=") {
            tag.id = Some(id.to_owned()).filter(|id| !id.is_empty());
        }
    }
    tag
}

fn parse_rate(r: &str) -> Option<f64> {
    let (n, d) = r.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then_some(n / d)
}

/// A volume keyframe on a track: at `t` seconds (source time) the gain is `db`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VolumePoint {
    pub t: f64,
    pub db: f32,
}

/// One source track's mix setting.
///
/// Volume is `gain` everywhere, unless `points` has keyframes: then it follows
/// them, linear in dB between points and flat before the first / after the last
/// (like volume rubber-banding in Premiere).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackEdit {
    pub index: usize,
    /// Linear gain; 1.0 = unchanged. Used when there are no keyframes.
    pub gain: f32,
    pub muted: bool,
    #[serde(default)]
    pub points: Vec<VolumePoint>,
}

impl TrackEdit {
    /// Volume in dB at source time `t` (ignores mute).
    pub fn db_at(&self, t: f64) -> f32 {
        let pts = &self.points;
        match pts.len() {
            0 => to_db(self.gain),
            _ if t <= pts[0].t => pts[0].db,
            _ if t >= pts[pts.len() - 1].t => pts[pts.len() - 1].db,
            _ => {
                // Ramps follow the fader, so a fade from silence sounds even
                // instead of staying inaudible for most of its length.
                let i = pts.partition_point(|p| p.t <= t);
                let (a, b) = (pts[i - 1], pts[i]);
                let f = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
                let (pa, pb) = (fader_pos(a.db), fader_pos(b.db));
                fader_db(pa + (pb - pa) * f)
            }
        }
    }

    /// Linear gain at `t`, mute applied.
    pub fn gain_at(&self, t: f64) -> f32 {
        if self.muted { 0.0 } else { from_db(self.db_at(t)) }
    }

    /// Whether this track plays back exactly as recorded.
    pub fn is_unity(&self) -> bool {
        !self.muted && self.points.iter().all(|p| p.db.abs() < 0.05) && (self.points.len() > 0 || (self.gain - 1.0).abs() < 0.005)
    }

    /// An ffmpeg `volume` expression for this track, with `t` = seconds since `start`.
    fn volume_expr(&self, start: f64) -> String {
        if self.muted {
            return "0".into();
        }
        if self.points.is_empty() {
            return format!("{:.5}", self.gain);
        }
        // Nested if(): piecewise-linear dB, converted to linear gain. Ramps follow
        // the fader (see `db_at`): straight in dB down to -30 dB; one that dips
        // below is split into short straight pieces that track the curve.
        let mut pts: Vec<(f64, f32)> = Vec::new();
        for (i, p) in self.points.iter().enumerate() {
            if let Some(prev) = i.checked_sub(1).map(|j| self.points[j]) {
                if prev.db.min(p.db) < -30.0 {
                    const PIECES: usize = 8;
                    for k in 1..PIECES {
                        let t = prev.t + (p.t - prev.t) * k as f64 / PIECES as f64;
                        pts.push((t - start, self.db_at(t)));
                    }
                }
            }
            pts.push((p.t - start, p.db));
        }
        let mut db = format!("{:.3}", pts[pts.len() - 1].1);
        for w in pts.windows(2).rev() {
            let ((t0, d0), (t1, d1)) = (w[0], w[1]);
            let seg = format!("({d0:.3}+({:.3})*(t-({t0:.4}))/{:.4})", d1 - d0, (t1 - t0).max(1e-6));
            db = format!("if(lt(t,{t1:.4}),{seg},{db})");
        }
        db = format!("if(lt(t,{:.4}),{:.3},{db})", pts[0].0, pts[0].1);
        // At the bottom of the fader: true silence, not just very quiet.
        format!("if(lte({db},{SILENT_DB}),0,pow(10,({db})/20))")
    }
}

/// Volume this low is silence (shown as −∞): the bottom of the fader.
pub const SILENT_DB: f32 = -96.0;
/// The top of the fader.
pub const FADER_MAX_DB: f32 = 24.0;
/// The fader law, as (position 0..=1, dB) points with straight lines in dB
/// between them: like a mixing desk, the top three quarters are even in dB
/// (equal distance sounds like an equal step louder, 0 dB about two thirds
/// up), and the bottom quarter tapers through −30 and −60 down to silence.
const FADER: [(f32, f32); 4] = [(0.0, SILENT_DB), (0.08, -60.0), (0.25, -30.0), (1.0, FADER_MAX_DB)];

/// Fader position (0..=1) for a volume in dB.
pub fn fader_pos(db: f32) -> f32 {
    let db = db.clamp(SILENT_DB, FADER_MAX_DB);
    for w in FADER.windows(2) {
        let ((p0, d0), (p1, d1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (db - d0) / (d1 - d0) * (p1 - p0);
        }
    }
    1.0
}

/// Volume in dB at fader position `pos` (0..=1).
pub fn fader_db(pos: f32) -> f32 {
    let pos = pos.clamp(0.0, 1.0);
    for w in FADER.windows(2) {
        let ((p0, d0), (p1, d1)) = (w[0], w[1]);
        if pos <= p1 {
            return d0 + (pos - p0) / (p1 - p0) * (d1 - d0);
        }
    }
    FADER_MAX_DB
}

/// Everything about how a clip has been edited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    pub start: f64,
    pub end: f64,
    /// Stretches cut out between `start` and `end`, in order: what's kept is
    /// the parts between them (see [`Edit::parts`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cuts: Vec<Cut>,
    pub tracks: Vec<TrackEdit>,
    /// How the saved file is made: size, rate, bitrate, audio.
    #[serde(default)]
    pub output: Output,
}

/// A stretch cut out of the middle of a clip: `from` to `to` seconds of the
/// recording. One of no length is a split: the clip in two parts, nothing
/// cut out (yet).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    pub from: f64,
    pub to: f64,
}

/// The sound crossfades over this long each side of a join between parts
/// kept (seconds), so a cut in the middle of a sound doesn't click.
pub const JOIN_FADE: f64 = 0.005;

/// Half of each join's crossfade, for `parts` as saved ([`Edit::kept`]):
/// [`JOIN_FADE`], less where a part beside it is shorter than that takes
/// from it at both its ends.
pub fn join_fades(parts: &[(f64, f64)]) -> Vec<f64> {
    parts.windows(2).map(|w| JOIN_FADE.min((w[0].1 - w[0].0) / 2.0).min((w[1].1 - w[1].0) / 2.0)).collect()
}

/// How a saved clip is made, when not just like the recording: smaller
/// (resolution, frame rate, bitrate or a size to fit under) and with fewer
/// audio tracks. The default changes nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Output {
    /// Height in pixels (the width follows the shape); only below the source's.
    #[serde(default)]
    pub height: Option<u32>,
    /// Frames per second; only below the source's.
    #[serde(default)]
    pub fps: Option<u32>,
    /// Video bitrate.
    #[serde(default)]
    pub video_kbps: Option<u32>,
    /// Aim for a file no bigger than this many megabytes (MiB): picks the
    /// video bitrate from the length, over `video_kbps`.
    #[serde(default)]
    pub max_mb: Option<u32>,
    /// Only the mix (track 1), not each source on its own track too.
    #[serde(default)]
    pub mix_only: bool,
    /// Frame blending: the rate the picture is mixed down from (a whole
    /// multiple of the frame rate, below the source's), each new frame the
    /// average of the frames in its slot, like motion blur. `None` just
    /// drops the in-between frames.
    #[serde(default)]
    pub blend_fps: Option<u32>,
    /// How each blended frame weighs the frames in it.
    #[serde(default)]
    pub blend_shape: BlendShape,
    /// The part of the picture kept (black bars cut off, or a 16:9 out of
    /// an ultrawide screen); the resolution is then this part's.
    #[serde(default)]
    pub crop: Option<Crop>,
}

/// A part of the picture, in the recording's pixels: `w`×`h` with its top
/// left corner `x` across and `y` down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Crop {
    /// The smallest a side can be, in pixels.
    pub const MIN: u32 = 32;

    /// The whole of a `width`×`height` picture.
    pub fn whole(width: u32, height: u32) -> Self {
        Self { x: 0, y: 0, w: width, h: height }
    }

    /// The biggest part of a `width`×`height` picture shaped `aw`:`ah`, its
    /// middle as near `centre` (in pixels) as the picture allows.
    pub fn largest(width: u32, height: u32, aw: u32, ah: u32, centre: (f64, f64)) -> Self {
        let (fw, fh) = (width as f64, height as f64);
        let shape = aw as f64 / ah.max(1) as f64;
        let (w, h) = if fw / fh > shape { (fh * shape, fh) } else { (fw, fw / shape) };
        let x = (centre.0 - w / 2.0).clamp(0.0, fw - w);
        let y = (centre.1 - h / 2.0).clamp(0.0, fh - h);
        Self { x: x.round() as u32, y: y.round() as u32, w: w.round() as u32, h: h.round() as u32 }
    }

    /// Made to fit a `width`×`height` picture: inside it, at least
    /// [`Self::MIN`] a side, and even everywhere (H.264's colour is kept for
    /// 2×2 pixels at a time). `None` if that's the whole picture.
    pub fn within(self, width: u32, height: u32) -> Option<Self> {
        let even = |v: u32| v / 2 * 2;
        let w = even(self.w.clamp(Self::MIN.min(width), width));
        let h = even(self.h.clamp(Self::MIN.min(height), height));
        let x = even(self.x.min(width - w));
        let y = even(self.y.min(height - h));
        let whole = w + 1 >= width && h + 1 >= height;
        (!whole).then_some(Self { x, y, w, h })
    }

    /// Its middle, in pixels.
    pub fn centre(&self) -> (f64, f64) {
        (self.x as f64 + self.w as f64 / 2.0, self.y as f64 + self.h as f64 / 2.0)
    }
}

/// How a blended frame weighs the frames in it, as live capture can
/// (`capture::BlendShape`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendShape {
    /// The frames in its own slot, each the same.
    #[default]
    Even,
    /// The frames from the middle of the slot before to the middle of the
    /// one after, the nearer its own middle the more (by cos² of how far):
    /// trails fade out at their ends, so frames hand over smoothly.
    Soft,
}

/// AAC bitrate of each audio track a render writes.
pub const RENDER_AUDIO_KBPS: u32 = 192;

/// The rates a clip of `src_fps` can be blended down from for output at
/// `out` fps: whole multiples of it, up to the clip's own rate.
pub fn blend_rates(out: u32, src_fps: f64) -> Vec<u32> {
    (2..).map(|k| out * k).take_while(|&r| out > 0 && r as f64 <= src_fps + 0.5).collect()
}

impl Output {
    /// The settings that actually change something for `info` (a height or
    /// rate at or above the source's is the source's, a crop of the whole
    /// picture none).
    pub fn effective(&self, info: &ClipInfo) -> Self {
        let (_, ph) = self.picture(info);
        let mut e = Self {
            height: self.height.filter(|&h| h < ph),
            fps: self.fps.filter(|&f| (f as f64) < info.fps - 0.5),
            video_kbps: self.video_kbps.filter(|&k| info.video_kbps.is_none_or(|src| k < src)),
            max_mb: self.max_mb,
            mix_only: self.mix_only,
            blend_fps: None,
            blend_shape: BlendShape::Even,
            crop: self.crop.and_then(|c| c.within(info.width, info.height)),
        };
        e.fit_picture(info);
        let out = e.fps.map_or(info.fps, f64::from).round() as u32;
        e.blend_fps = self.blend_fps.filter(|b| e.fps.is_some() && blend_rates(out, info.fps).contains(b));
        if e.blend_fps.is_some() {
            e.blend_shape = self.blend_shape;
        }
        e
    }

    /// With a size to fit under and the resolution or rate left as they
    /// are: the biggest picture the bitrate can carry (about 0.05 bits per
    /// pixel), as an encoder can't make a big picture small enough: a
    /// minute of 3600×2338 asked for 10 MB came out at 14.
    fn fit_picture(&mut self, info: &ClipInfo) {
        let Some(mb) = self.max_mb else { return };
        if self.height.is_some() && self.fps.is_some() {
            return;
        }
        let audio = if self.mix_only { RENDER_AUDIO_KBPS } else { RENDER_AUDIO_KBPS * (info.source_tracks().len() as u32 + 1) };
        let audio = if info.audio.is_empty() { 0 } else { audio };
        let kbps = fit_kbps(mb, info.duration, audio) as f64 * 1000.0;
        let (pw, ph) = self.picture(info);
        let aspect = pw as f64 / ph.max(1) as f64;
        let src_fps = info.fps.round() as u32;
        // Best first: full rate down to 30, then smaller.
        let heights = [2160, 1440, 1080, 720, 540, 480, 360];
        let rates: Vec<u32> = [src_fps, 30].into_iter().filter(|&f| f <= src_fps).collect();
        for h in heights.into_iter().filter(|&h| h <= ph) {
            for &f in &rates {
                let (h, f) = (self.height.unwrap_or(h), self.fps.unwrap_or(f));
                if kbps / (h as f64 * h as f64 * aspect * f as f64) >= 0.05 {
                    self.height = self.height.or(Some(h).filter(|&h| h < ph));
                    self.fps = self.fps.or(Some(f).filter(|&f| (f as f64) < info.fps - 0.5));
                    return;
                }
            }
        }
        self.height = self.height.or(Some(360).filter(|&h| h < ph));
        self.fps = self.fps.or(Some(30).filter(|&f| (f as f64) < info.fps - 0.5));
    }

    /// Whether the video must be re-encoded whole (else it can be copied).
    pub fn changes_video(&self, info: &ClipInfo) -> bool {
        let e = self.effective(info);
        e.height.is_some() || e.fps.is_some() || e.video_kbps.is_some() || e.max_mb.is_some() || e.crop.is_some()
    }

    /// The picture before it's scaled: the crop's, else the recording's.
    pub fn picture(&self, info: &ClipInfo) -> (u32, u32) {
        match self.crop.and_then(|c| c.within(info.width, info.height)) {
            Some(c) => (c.w, c.h),
            None => (info.width, info.height),
        }
    }

    /// Output frame size for `info`: even, the picture's shape.
    pub fn size(&self, info: &ClipInfo) -> (u32, u32) {
        let (pw, ph) = self.picture(info);
        match self.effective(info).height {
            None => (pw, ph),
            Some(h) => {
                let w = (pw as f64 * h as f64 / ph.max(1) as f64 / 2.0).round() as u32 * 2;
                (w.max(2), h / 2 * 2)
            }
        }
    }

    /// Output frame rate for `info`.
    pub fn frame_rate(&self, info: &ClipInfo) -> f64 {
        self.effective(info).fps.map_or(info.fps, f64::from)
    }

    /// Audio tracks a render of `edit` writes.
    pub fn audio_tracks(&self, edit: &Edit) -> u32 {
        match edit.tracks.len() {
            0 => 0,
            n if self.mix_only => 1.min(n as u32),
            n => n as u32 + 1,
        }
    }

    /// The video bitrate a render of `duration` seconds uses: the size
    /// target's, else the one chosen, else the source's (`None` if unknown).
    pub fn video_kbps_for(&self, info: &ClipInfo, edit: &Edit) -> Option<u32> {
        let e = self.effective(info);
        if let Some(mb) = e.max_mb {
            return Some(fit_kbps(mb, edit.duration(), self.audio_tracks(edit) * RENDER_AUDIO_KBPS));
        }
        if let Some(k) = e.video_kbps {
            return Some(k);
        }
        // Smaller or fewer frames and no bitrate chosen: the source's, scaled
        // down with the pixels a second (to the ¾ power: smaller pictures need
        // a bit more per pixel to look as good).
        let src = info.video_kbps?;
        let (w, h) = self.size(info);
        let ratio = (w as f64 * h as f64 * self.frame_rate(info)) / (info.width as f64 * info.height as f64 * info.fps).max(1.0);
        Some(((src as f64 * ratio.min(1.0).powf(0.75)) as u32).max(500))
    }

    /// Roughly how big a render of `edit` comes out, in bytes.
    pub fn estimate_bytes(&self, info: &ClipInfo, edit: &Edit) -> Option<u64> {
        let video = self.video_kbps_for(info, edit)? as f64;
        let audio = (self.audio_tracks(edit) * RENDER_AUDIO_KBPS) as f64;
        Some(((video + audio) * 1000.0 / 8.0 * edit.duration()) as u64)
    }
}

/// Video bitrate (kbps) for a file of `duration` s to come in under `mb` MiB
/// next to `audio_kbps` of audio. Hardware encoders overshoot a little, and
/// the container takes a few percent, so it aims 10% low; never below 200.
pub fn fit_kbps(mb: u32, duration: f64, audio_kbps: u32) -> u32 {
    let total = mb as f64 * 1024.0 * 1024.0 * 8.0 / 1000.0 / duration.max(0.1) * 0.90;
    (total - audio_kbps as f64).max(200.0) as u32
}

impl Edit {
    /// The same edit of a copy of the source whose time `by` seconds is the
    /// source's time zero (one trimmed to start `by` in).
    pub fn shifted(&self, by: f64) -> Self {
        let mut e = self.clone();
        e.start = (e.start - by).max(0.0);
        e.end = (e.end - by).max(e.start);
        for c in &mut e.cuts {
            c.from = (c.from - by).max(e.start);
            c.to = (c.to - by).max(c.from);
        }
        for t in &mut e.tracks {
            for p in &mut t.points {
                p.t -= by;
            }
        }
        e
    }

    /// The identity edit for a clip: full length, every source at unity.
    pub fn new(info: &ClipInfo) -> Self {
        Self {
            start: 0.0,
            end: info.duration,
            cuts: Vec::new(),
            tracks: info
                .source_tracks()
                .iter()
                // A track that wasn't in the clip's mix starts muted, so the rebuilt
                // mix matches the original until you choose to bring it in.
                .map(|a| TrackEdit { index: a.index, gain: 1.0, muted: !a.in_mix, points: Vec::new() })
                .collect(),
            output: Output::default(),
        }
    }

    /// Whether this edit changes anything versus the original.
    pub fn is_identity(&self, info: &ClipInfo) -> bool {
        let eps = info.frame_duration() / 2.0;
        // Unchanged = as recorded: unity volume, and muted only where the track
        // wasn't in the mix to begin with.
        let as_recorded = |t: &TrackEdit| {
            let in_mix = info.audio.iter().find(|a| a.index == t.index).is_none_or(|a| a.in_mix);
            TrackEdit { muted: false, ..t.clone() }.is_unity() && t.muted == !in_mix
        };
        self.start <= eps
            && self.end >= info.duration - eps
            && self.kept().len() == 1
            && self.tracks.iter().all(as_recorded)
            && !self.output.changes_video(info)
            && !(self.output.mix_only && self.tracks.len() > 1)
    }

    /// How long the saved clip is: the parts kept, together.
    pub fn duration(&self) -> f64 {
        self.parts().iter().map(|(a, b)| (b - a).max(0.0)).sum()
    }

    /// The parts kept, in order, as `(from, to)` seconds of the recording:
    /// `start` to `end` less the cuts. The two parts at a split touch.
    pub fn parts(&self) -> Vec<(f64, f64)> {
        let mut parts = Vec::with_capacity(self.cuts.len() + 1);
        let mut from = self.start;
        for c in &self.cuts {
            parts.push((from, c.from));
            from = c.to;
        }
        parts.push((from, self.end));
        parts
    }

    /// Keep `parts` (in order, apart or touching: touching, they stay split).
    pub fn set_parts(&mut self, parts: &[(f64, f64)]) {
        let (Some(first), Some(last)) = (parts.first(), parts.last()) else { return };
        self.start = first.0;
        self.end = last.1;
        self.cuts = parts.windows(2).map(|w| Cut { from: w[0].1, to: w[1].0 }).collect();
    }

    /// The parts as the saved clip has them: split ones with nothing cut out
    /// between them are one.
    pub fn kept(&self) -> Vec<(f64, f64)> {
        let mut kept: Vec<(f64, f64)> = Vec::new();
        for (a, b) in self.parts() {
            match kept.last_mut() {
                Some(last) if a - last.1 < 1e-4 => last.1 = b,
                _ => kept.push((a, b)),
            }
        }
        kept
    }

    /// Where `t` seconds of the recording is in the saved clip, if it's kept.
    pub fn to_output(&self, t: f64) -> Option<f64> {
        output_time(&self.kept(), t)
    }

    /// The recording's time at `t` seconds into the saved clip.
    pub fn to_source(&self, t: f64) -> f64 {
        source_time(&self.kept(), t)
    }
}

/// Where `t` seconds of a recording is in a clip made of `parts` of it (in
/// order, as [`Edit::kept`] has them), if it's in it (the end of the last
/// part counts).
pub fn output_time(parts: &[(f64, f64)], t: f64) -> Option<f64> {
    let mut at = 0.0;
    for (i, &(a, b)) in parts.iter().enumerate() {
        if t >= a - 1e-9 && (t < b || (i + 1 == parts.len() && t <= b + 1e-9)) {
            return Some(at + (t - a).max(0.0));
        }
        at += b - a;
    }
    None
}

/// The recording's time at `t` seconds into a clip made of `parts` of it.
pub fn source_time(parts: &[(f64, f64)], t: f64) -> f64 {
    let mut at = 0.0;
    for &(a, b) in parts {
        if t < at + (b - a) {
            return a + (t - at).max(0.0);
        }
        at += b - a;
    }
    parts.last().map_or(t, |&(_, b)| b)
}

/// Where `dest` is written until it's whole: `.writing/<name>` next to it,
/// as the recorder does (`capture::output::in_progress`, which says why the
/// file itself never starts with a dot). Makes the folder.
fn writing_path(dest: &Path) -> std::io::Result<PathBuf> {
    let dir = dest.parent().unwrap_or(Path::new(".")).join(".writing");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(dest.file_name().unwrap_or_default()))
}

/// Decode one audio stream to interleaved stereo f32 at [`PREVIEW_RATE`].
pub fn decode_audio(source: &Path, stream: usize) -> Result<Vec<f32>> {
    let mut child = ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(source)
        .args(["-map", &format!("0:a:{stream}"), "-ac", "2", "-ar", &PREVIEW_RATE.to_string()])
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut bytes = Vec::new();
    child.stdout.take().context("no ffmpeg output")?.read_to_end(&mut bytes)?;
    child.wait()?;
    Ok(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// An RGBA frame.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode the exact frame at `t`, scaled to `width` (aspect kept).
pub fn frame_at(source: &Path, t: f64, width: u32) -> Result<Frame> {
    frame(source, t, width, true)
}

/// ffmpeg's output arguments for raw NV12 frames at the video's own size,
/// studio range (`nv12_len` bytes each).
pub const NV12_OUT: [&str; 6] = ["-vf", "scale=out_range=tv", "-pix_fmt", "nv12", "-f", "rawvideo"];

/// The bytes in an NV12 frame `width`×`height`: the luma, then Cb, Cr pairs
/// at half the size each way.
pub fn nv12_len(width: u32, height: u32) -> usize {
    (width * height + 2 * width.div_ceil(2) * height.div_ceil(2)) as usize
}

/// Decode the exact frame at `t` as NV12 at the video's own size
/// (`width`×`height`).
pub fn nv12_at(source: &Path, t: f64, (width, height): (u32, u32)) -> Result<Vec<u8>> {
    let out = ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{t:.4}"), "-i"])
        .arg(source)
        .args(["-frames:v", "1"])
        .args(NV12_OUT)
        .arg("-")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("failed to run ffmpeg")?;
    let len = nv12_len(width, height);
    if out.stdout.len() < len {
        bail!("no frame at {t:.2}s");
    }
    let mut data = out.stdout;
    data.truncate(len);
    Ok(data)
}

fn frame(source: &Path, t: f64, width: u32, exact: bool) -> Result<Frame> {
    let info_h = |w: u32, src_w: u32, src_h: u32| -> u32 {
        // Even height, as ffmpeg's scale=-2 would produce.
        (((w as f64) * src_h as f64 / src_w.max(1) as f64 / 2.0).round() as u32 * 2).max(2)
    };
    // Need the source size to know how many bytes come back.
    let (sw, sh) = video_size(source)?;
    let height = info_h(width, sw, sh);
    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    if !exact {
        cmd.arg("-noaccurate_seek");
    }
    let out = cmd
        .args(["-ss", &format!("{t:.4}"), "-i"])
        .arg(source)
        .args(["-frames:v", "1", "-vf", &format!("scale={width}:{height}:flags=bilinear")])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("failed to run ffmpeg")?;
    let expected = (width * height * 4) as usize;
    if out.stdout.len() < expected {
        bail!("no frame at {t:.2}s");
    }
    Ok(Frame { width, height, rgba: out.stdout[..expected].to_vec() })
}

fn video_size(source: &Path) -> Result<(u32, u32)> {
    let out = ffprobe()
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height"])
        .args(["-of", "csv=p=0:s=x"])
        .arg(source)
        .output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let (w, h) = s.trim().split_once('x').context("no video size")?;
    Ok((w.parse()?, h.parse()?))
}

/// Timeline thumbnails: the keyframes of `source`, `height` px tall, sent as
/// `(time, frame)` as soon as each is decoded.
///
/// Keyframes are found from the file's index (no decoding), then decoded with
/// `-skip_frame nokey` so only they are ever touched. That's the cheapest way to
/// get many frames out of a long recording: our keyframes are every 1–2 s, and a
/// keyframe decodes on its own. The clip is split into a few time ranges decoded
/// in parallel with the hardware decoder, so the whole strip fills in at once
/// rather than left to right.
///
/// At most about `max` keyframes become thumbnails, spread evenly over the clip;
/// `on_keys` gets their times before any is decoded, so a timeline can lay out
/// every cell once and fill each in when its own picture arrives, never showing
/// a stand-in that changes later.
pub fn keyframe_strip(
    source: &Path,
    height: u32,
    max: usize,
    on_keys: impl FnOnce(&[f64]),
    mut on_frame: impl FnMut(f64, Frame) -> bool,
) -> Result<()> {
    let keys = keyframe_times(source)?;
    if keys.is_empty() {
        bail!("no keyframes");
    }
    let wanted = spread(&keys, max);
    on_keys(&wanted);
    let (sw, sh) = video_size(source)?;
    // Even width, aspect kept.
    let width = (((height as f64) * sw as f64 / sh.max(1) as f64 / 2.0).round() as u32 * 2).max(2);
    let frame_bytes = (width * height * 4) as usize;

    // Split the keyframes into contiguous ranges, one ffmpeg each.
    let jobs = keys.len().clamp(1, 4);
    let per = keys.len().div_ceil(jobs);
    let (tx, rx) = std::sync::mpsc::channel::<(f64, Frame)>();
    let mut children = Vec::new();
    for chunk in keys.chunks(per) {
        let (first, last) = (chunk[0], *chunk.last().unwrap());
        let times = chunk.to_vec();
        let wanted = wanted.clone();
        let mut cmd = ffmpeg_background();
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if cfg!(target_os = "macos") {
            cmd.args(["-hwaccel", "videotoolbox"]);
        }
        let mut child = cmd
            // Seeking lands on the last keyframe at or before the time, so aim just
            // past this range's first one: aiming before it would start a keyframe
            // early and label every picture with the next one's time.
            .args(["-skip_frame", "nokey", "-noaccurate_seek", "-ss", &format!("{:.4}", first + 0.001)])
            // A little past the last keyframe so it's included, but not the next range's first.
            .args(["-t", &format!("{:.4}", last - first + 0.01), "-i"])
            .arg(source)
            .args(["-an", "-vf", &format!("scale={width}:{height}:flags=bilinear"), "-fps_mode", "passthrough"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to run ffmpeg")?;
        let mut out = child.stdout.take().context("no ffmpeg output")?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for t in times {
                let mut rgba = vec![0u8; frame_bytes];
                if out.read_exact(&mut rgba).is_err() {
                    break;
                }
                if wanted.binary_search_by(|k| k.total_cmp(&t)).is_ok() && tx.send((t, Frame { width, height, rgba })).is_err() {
                    break;
                }
            }
        });
        children.push(child);
    }
    drop(tx);
    let mut cancelled = false;
    for (t, f) in rx {
        if !on_frame(t, f) {
            cancelled = true;
            break;
        }
    }
    for mut c in children {
        if cancelled {
            let _ = c.kill();
        }
        let _ = c.wait();
    }
    Ok(())
}

/// About `max` of `keys` (sorted), evenly spread: each at least 1/`max` of the
/// span after the one before. Always keeps the first.
fn spread(keys: &[f64], max: usize) -> Vec<f64> {
    let span = keys.last().unwrap_or(&0.0) - keys.first().unwrap_or(&0.0);
    let gap = span / max.max(1) as f64;
    let mut out: Vec<f64> = Vec::new();
    for &k in keys {
        if out.last().is_none_or(|l| k - l >= gap) {
            out.push(k);
        }
    }
    out
}

/// Presentation times of every keyframe, read from the container index.
pub fn keyframe_times(source: &Path) -> Result<Vec<f64>> {
    keyframe_flags(source, None)
}

/// [`keyframe_times`], of the whole file or of `read` (ffprobe's
/// `-read_intervals`).
fn keyframe_flags(source: &Path, read: Option<&str>) -> Result<Vec<f64>> {
    let mut cmd = ffprobe();
    cmd.args(["-v", "error", "-select_streams", "v:0"]);
    if let Some(read) = read {
        cmd.args(["-read_intervals", read]);
    }
    let out = cmd
        .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .context("failed to run ffprobe")?;
    let mut keys: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (t, flags) = l.split_once(',')?;
            flags.contains('K').then(|| t.parse().ok()).flatten()
        })
        .collect();
    keys.sort_by(f64::total_cmp);
    keys.dedup();
    Ok(keys)
}

/// Copy `from`..`to` of `source` to `dest` without re-encoding: every track,
/// the clip tag and the track names. `from` should be a keyframe (a copy can
/// only start on one). Returns where `from` landed in the copy.
pub fn trim_copy(source: &Path, from: f64, to: f64, dest: &Path) -> Result<f64> {
    run(ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-ss", &format!("{from:.6}"), "-to", &format!("{to:.6}"), "-i"])
        .arg(source)
        .args(["-map", "0", "-c", "copy", "-map_metadata", "0", "-avoid_negative_ts", "make_zero"])
        .arg(dest))?;
    keyframe_times(dest)?.first().copied().context("the copy has no picture")
}

/// Render `edit` of `source` to `dest` (written to a hidden partial first, so a
/// half-finished file never appears under its real name). `id` goes into the
/// output's clip tag.
pub fn render_to(source: &Path, info: &ClipInfo, edit: &Edit, dest: &Path, id: Option<&str>) -> Result<()> {
    render_with_progress(source, info, edit, dest, id, |_| {})
}

/// Render with progress callbacks (0.0..=1.0).
///
/// "Smart render": only the frames between a cut and the nearest keyframe are
/// re-encoded; everything between the first and last keyframe inside each
/// part kept is stream-copied untouched. A 20 s trim of a 60 fps 4K-wide
/// recording drops from ~20 s (full re-encode) to ~3 s, and the copied part
/// is bit-identical to the original. A part with no keyframes to copy from is
/// encoded whole.
pub fn render_with_progress(
    source: &Path,
    info: &ClipInfo,
    edit: &Edit,
    dest: &Path,
    id: Option<&str>,
    progress: impl Fn(f32),
) -> Result<()> {
    let ext = dest.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
    // One scratch dir per render: several can run at once (an edit and a "save as
    // new" of the same clip), and they must never share intermediate files.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("hasteclip-render-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| -> Result<()> {
        progress(0.0);
        let video = work.join(format!("video.{ext}"));
        let pieces = plan(source, info, edit);
        let weights = Weights::new(&pieces, edit.duration());
        render_video(source, info, edit, &pieces, &work, &video, &|f| progress(f * weights.video))?;
        // In a hidden folder so the library never shows a half-written clip,
        // under its real name so ffmpeg picks the container from its extension.
        let partial = writing_path(dest)?;
        let muxed = mux_audio(source, info, edit, id, &video, &partial, &|f| progress(weights.video + f * (1.0 - weights.video)))
            .and_then(|()| Ok(std::fs::rename(&partial, dest)?));
        if muxed.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        muxed?;
        progress(1.0);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// A stretch of the saved video and how it's made: encoded again, or copied
/// as it is (from a keyframe on). In order, they're the video.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece {
    Encode(f64, f64),
    Copy(f64, f64),
}

impl Piece {
    /// Rough relative cost, so one progress bar moves at an even pace:
    /// encoding ~1 unit per second of video, copying far less.
    fn cost(&self) -> f64 {
        match *self {
            Piece::Encode(a, b) => b - a,
            Piece::Copy(a, b) => (b - a) * 0.03,
        }
    }
}

/// The video's pieces: for each part kept, a full re-encode, or re-encoded
/// edges around a stream-copied middle (its first to last keyframe).
fn plan(source: &Path, info: &ClipInfo, edit: &Edit) -> Vec<Piece> {
    let kept = edit.kept();
    // A new size, rate or bitrate: every frame is encoded again.
    if edit.output.changes_video(info) {
        return kept.into_iter().map(|(a, b)| Piece::Encode(a, b)).collect();
    }
    let fd = info.frame_duration();
    let keys = keyframes(source, &kept).unwrap_or_default();
    let h264 = std::sync::OnceLock::new();
    let mut pieces = Vec::new();
    for (a, b) in kept {
        // Keyframes inside the part, with a frame of margin from its ends.
        let inside: Vec<f64> = keys.iter().copied().filter(|&k| k >= a - fd / 2.0 && k <= b - fd).collect();
        match (inside.first(), inside.last()) {
            (Some(&k1), Some(&k2)) if k2 - k1 >= 1.0 && *h264.get_or_init(|| is_h264(source)) => {
                if k1 - a > 1e-3 {
                    pieces.push(Piece::Encode(a, k1));
                }
                pieces.push(Piece::Copy(k1, k2));
                if b - k2 > 1e-3 {
                    pieces.push(Piece::Encode(k2, b));
                }
            }
            _ => pieces.push(Piece::Encode(a, b)),
        }
    }
    pieces
}

/// Rough relative cost of each step (see [`Piece::cost`]), so one progress
/// bar moves at an even pace; the audio mux costs far less than encoding.
struct Weights {
    video: f32,
}

impl Weights {
    fn new(pieces: &[Piece], dur: f64) -> Self {
        let video: f64 = pieces.iter().map(Piece::cost).sum();
        let audio = dur * 0.04 + 0.3;
        Self { video: (video / (video + audio)) as f32 }
    }
}

/// Whether this ffmpeg has the filter `name` (builds differ), asked once.
fn has_filter(name: &str) -> bool {
    static FILTERS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let list = FILTERS.get_or_init(|| {
        ffmpeg().args(["-hide_banner", "-filters"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    });
    list.lines().any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// The frames it takes to blend `looks` frames into each at `rate`, and
/// each one's weight: its offset from the output frame's slot (in frames) and
/// weight. Even: the slot's own frames, the same. Soft: from the middle of
/// the slot before to the middle of the one after, by cos² of how far from
/// its own middle (live capture's soft blending: `capture::BlendShape`).
fn blend_taps(looks: u32, shape: BlendShape) -> Vec<(i64, f64)> {
    let n = looks as i64;
    match shape {
        BlendShape::Even => (0..n).map(|i| (i, 1.0)).collect(),
        BlendShape::Soft => (n / 2 - n..n / 2 + n)
            .map(|i| {
                // How far from the slot's middle, in slots (-1 to 1).
                let off = (2 * i + 1 - n) as f64 / (2 * n) as f64;
                (i, (std::f64::consts::FRAC_PI_2 * off).cos().powi(2))
            })
            .filter(|(_, w)| *w > 1e-9)
            .collect(),
    }
}

/// The filter graph that blends `looks` frames into each at `rate` (see
/// `blend_taps`). Only the kept averages are made: each tap's branch takes
/// every `looks`th frame from its offset and `mix` averages the branches'
/// next ones (`tmix` then `fps` averaged at every frame and kept one in
/// `looks`: 480 to 60 fps took 2.4x as long). Taps before the first frame see
/// it (padded with copies of it); taps after the last, the last.
fn blend_graph(looks: u32, rate: f64, shape: BlendShape) -> String {
    let taps = blend_taps(looks, shape);
    let pad = taps.iter().map(|t| -t.0).max().unwrap_or(0).max(0);
    let mut graph = if pad > 0 { format!("tpad=start={pad}:start_mode=clone,") } else { String::new() };
    graph += &format!("split={}{}", taps.len(), (0..taps.len()).map(|i| format!("[b{i}]")).collect::<String>());
    for (i, (off, _)) in taps.iter().enumerate() {
        graph += &format!(";[b{i}]trim=start_frame={},framestep={looks},setpts=N/{rate}/TB[g{i}]", off + pad);
    }
    // As many frames as the slots: the branch at offset 0 has one per slot,
    // and goes first (`duration=first`); one ending sooner holds its last.
    let order: Vec<usize> = (0..taps.len()).filter(|&i| taps[i].0 == 0).chain((0..taps.len()).filter(|&i| taps[i].0 != 0)).collect();
    let inputs: String = order.iter().map(|i| format!("[g{i}]")).collect();
    match shape {
        BlendShape::Even => graph += &format!(";{inputs}mix=inputs={}", taps.len()),
        BlendShape::Soft => {
            let weights: Vec<String> = order.iter().map(|&i| format!("{:.6}", taps[i].1)).collect();
            graph += &format!(";{inputs}mix=inputs={}:weights='{}':duration=first", taps.len(), weights.join(" "));
        }
    }
    graph
}

/// Video only, its `pieces` (see [`plan`]) cut frame-exactly and joined,
/// into `out`. `progress` gets 0..=1.
fn render_video(source: &Path, info: &ClipInfo, edit: &Edit, pieces: &[Piece], work: &Path, out: &Path, progress: &dyn Fn(f32)) -> Result<()> {
    let output = edit.output.effective(info);
    let kbps = if edit.output.changes_video(info) {
        edit.output.video_kbps_for(info, edit).map(u64::from).unwrap_or(8_000)
    } else {
        source_video_kbps(source).unwrap_or(20_000)
    };
    // Cropped first, so nothing after works on what's cut off. Smaller and
    // fewer frames: scaled (Lanczos, sharp) and dropped evenly.
    let mut filters = Vec::new();
    if let Some(c) = output.crop {
        filters.push(format!("crop={}:{}:{}:{}:exact=1", c.w, c.h, c.x, c.y));
    }
    if output.fps.is_some() {
        let rate = edit.output.frame_rate(info);
        match output.blend_fps {
            // Frames at the blend rate, each output frame the average of the
            // ones in its slot (or around it, softly: `blend_graph`). Mixed
            // as light (what a screen gives off), not as pixel values, which
            // are gamma-encoded: a fifth of a bright ghost's value is about
            // 3% of its light, not 20%. Recordings are BT.709, limited; a
            // clip's colours are read by its own matrix (BT.601's read as
            // BT.709's came out 8.5 levels off in RGB), and saved BT.709. 16
            // bits keep the faintest ghosts (72 dB from float) with half the
            // memory to move.
            Some(b) => {
                let light = has_filter("zscale");
                if light {
                    let matrix = if info.bt601 { "smpte170m" } else { "bt709" };
                    filters.push(format!("setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace={matrix}"));
                    filters.push("zscale=tin=iec61966-2-1:t=linear,format=gbrp16le".to_owned());
                }
                if (b as f64) < info.fps - 0.5 {
                    filters.push(format!("fps={b}"));
                }
                let looks = (b as f64 / rate).round() as u32;
                filters.push(blend_graph(looks, rate, output.blend_shape));
                // One frame per slot already: in the rate's time base. (`fps`
                // took soft's last frame off, its end then the last frame's
                // time.)
                filters.push(format!("settb=1/{rate}"));
                if light {
                    filters.push("zscale=tin=linear:t=iec61966-2-1,format=gbrp".to_owned());
                    filters.push("scale=out_color_matrix=bt709:out_range=tv,format=yuv420p".to_owned());
                    filters.push("setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned());
                }
            }
            None => filters.push(format!("fps={rate}")),
        }
    }
    if output.height.is_some() {
        let (w, h) = edit.output.size(info);
        filters.push(format!("scale={w}:{h}:flags=lanczos"));
    }
    // A size to fit under: hold the encoder to the bitrate over short spans,
    // not just on average.
    let cap: Vec<String> = match output.max_mb {
        Some(_) => vec!["-maxrate".into(), format!("{kbps}k"), "-bufsize".into(), format!("{}k", kbps * 2)],
        None => Vec::new(),
    };
    // Every part gets the source's timescale. Otherwise the muxer picks one per
    // part (copied middle 1/19200, encoded edges 1/15360) and concat, which
    // copies timestamps without rescaling, plays the middle at the wrong rate:
    // a 60 fps trim came out as 48 fps with a fifth of its frames gone.
    let timescale = source_timescale(source).unwrap_or(600).to_string();
    let encoder = video_encoder();
    // VA-API encodes from GPU memory: the frames go up last.
    if encoder == "h264_vaapi" {
        filters.push("format=nv12,hwupload".to_owned());
    }
    // Each part from its own reading of the source, joined before the
    // filters: frame rates change over the whole, so parts that don't last a
    // whole number of the new frames don't each round off (the sound would
    // drift from the picture a little at every join).
    let encode = |parts: &[(f64, f64)], dest: &Path, progress: &dyn Fn(f32)| -> Result<()> {
        let mut cmd = ffmpeg();
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]).args(vaapi_device(encoder));
        for (from, to) in parts {
            cmd.args(["-ss", &format!("{from:.6}"), "-to", &format!("{to:.6}"), "-i"]).arg(source);
        }
        if let [_] = parts {
            cmd.args(["-map", "0:v:0"]);
            if !filters.is_empty() {
                cmd.args(["-vf", &filters.join(",")]);
            }
        } else {
            let inputs: String = (0..parts.len()).map(|i| format!("[{i}:v:0]")).collect();
            let graph = std::iter::once(format!("{inputs}concat=n={}:v=1:a=0", parts.len())).chain(filters.iter().cloned()).collect::<Vec<_>>().join(",");
            cmd.args(["-filter_complex", &graph]);
        }
        cmd.args(["-an", "-c:v", encoder, "-b:v", &format!("{kbps}k")])
            .args(&cap)
            // Same profile as our recordings so the segments can be joined.
            .args(["-profile:v", "high"])
            .args(if encoder == "h264_vaapi" { &[][..] } else { &["-pix_fmt", "yuv420p"][..] })
            .args(["-video_track_timescale", &timescale])
            .arg(dest);
        run_progress(&mut cmd, parts.iter().map(|(a, b)| b - a).sum(), progress)
    };

    // All of it encoded again (a piece for each part): in one go, on the
    // graphics card where it can be (much faster), else by ffmpeg.
    let encoded: Option<Vec<(f64, f64)>> = pieces.iter().map(|p| if let Piece::Encode(a, b) = *p { Some((a, b)) } else { None }).collect();
    if let Some(parts) = encoded {
        let rate = edit.output.frame_rate(info);
        let whole_rate = (rate - rate.round()).abs() < 1e-6 && rate >= 1.0;
        // A video to be shown turned (a phone's) ffmpeg turns as it renders
        // it; the card's frames are as stored, so it's left to ffmpeg.
        if whole_rate && std::env::var_os("HASTECLIP_EXPORT_FFMPEG").is_none() && !mp4::turned(source) {
            let out_fps = rate.round() as u32;
            let job = capture::export::Job {
                source,
                fps: info.fps,
                parts: &parts,
                crop: output.crop.map(|c| [c.x, c.y, c.w, c.h]),
                size: edit.output.size(info),
                out_fps,
                blend: output.blend_fps.map(|b| {
                    let shape = match output.blend_shape {
                        BlendShape::Even => capture::BlendShape::Even,
                        BlendShape::Soft => capture::BlendShape::Soft,
                    };
                    (b, shape)
                }),
                kbps: kbps as u32,
                fit: output.max_mb.is_some(),
            };
            match capture::export::render_video(&job, out, progress) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => eprintln!("export: the graphics card couldn't make it ({e:#}); ffmpeg does"),
            }
        }
        return encode(&parts, out, progress);
    }

    // Each piece on its own, encoded or copied, then joined. Its share of the
    // progress by the same cost model as `Weights`.
    let total: f64 = pieces.iter().map(Piece::cost).sum::<f64>().max(1e-6);
    let mut done = 0.0;
    let mut parts = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let path = work.join(format!("piece{i}.mp4"));
        let (from, share) = ((done / total) as f32, (piece.cost() / total) as f32);
        match *piece {
            Piece::Encode(a, b) => encode(&[(a, b)], &path, &|f| progress(from + f * share))?,
            Piece::Copy(a, b) => run(ffmpeg()
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(["-ss", &format!("{a:.6}"), "-to", &format!("{b:.6}"), "-i"])
                .arg(source)
                .args(["-map", "0:v:0", "-an", "-c", "copy", "-avoid_negative_ts", "make_zero"])
                .args(["-video_track_timescale", &timescale])
                .arg(&path))?,
        }
        done += piece.cost();
        progress((done / total) as f32);
        parts.push(path);
    }

    let list = work.join("parts.txt");
    let body: String = parts.iter().map(|p| format!("file '{}'\n", p.display())).collect();
    std::fs::write(&list, body)?;
    run(ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-c", "copy"])
        .arg(out))?;
    progress(1.0);
    Ok(())
}

/// Add the audio (new mix on track 1, then each source at its volume) to the
/// rendered video, producing the final file.
fn mux_audio(source: &Path, info: &ClipInfo, edit: &Edit, id: Option<&str>, video: &Path, out: &Path, progress: &dyn Fn(f32)) -> Result<()> {
    let mut graph: Vec<String> = Vec::new();
    let mut mix_inputs: Vec<String> = Vec::new();
    let mut maps: Vec<String> = Vec::new();
    let mut titles: Vec<String> = Vec::new();
    // The sound is read from a second before the first part: a seek lands on
    // the sound's own frames, up to one (21 ms) after where it was asked, so
    // asking early and cutting the parts out by time keeps all of them, in
    // time with the picture (a part cut from a seek's start came out short).
    let from = (edit.start - 1.0).max(0.0);
    // The parts, in seconds from there, and how far each join's crossfade
    // reaches either side.
    let kept: Vec<(f64, f64)> = edit.kept().iter().map(|(a, b)| (a - from, b - from)).collect();
    let fades = join_fades(&kept);
    // Every sound exactly as long as the parts: padded with silence where a
    // track runs short, trimmed where a crossfade's rounding runs over. The
    // picture is never cut to the sound: ended by its shortest stream
    // (`-shortest`), a file whose sound came out a few milliseconds short
    // (a muted mix, silence made in whole AAC frames) lost the picture's
    // last frame, or four with NVIDIA's B-frames.
    let len = edit.duration();
    let exact = format!("apad=whole_dur={len:.6},atrim=end={len:.6}");

    for (i, t) in edit.tracks.iter().enumerate() {
        // eval=frame re-evaluates per audio frame so keyframed volume ramps; `t`
        // there counts from where the input is read from (and the volume is
        // set before the parts are cut out, on the recording's own time).
        let level = format!("[1:a:{}]volume='{}':eval=frame", t.index, t.volume_expr(from));
        if let [(a, b)] = kept[..] {
            graph.push(format!("{level},atrim=start={a:.6}:end={b:.6},asetpts=PTS-STARTPTS,{exact},asplit=2[m{i}][s{i}]"));
        } else {
            // Each part, running on into what's cut out either side of a
            // join by as much as that join crossfades over, then joined: the
            // crossfades take back what's added, so the length is the parts'.
            let n = kept.len();
            graph.push(format!("{level},asplit={n}{}", (0..n).map(|k| format!("[p{i}_{k}]")).collect::<String>()));
            for (k, &(a, b)) in kept.iter().enumerate() {
                let before = if k > 0 { fades[k - 1] } else { 0.0 };
                let after = fades.get(k).copied().unwrap_or(0.0);
                graph.push(format!("[p{i}_{k}]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS[q{i}_{k}]", a - before, b + after));
            }
            let mut joined = format!("[q{i}_0]");
            for k in 1..n {
                let to = format!("[j{i}_{k}]");
                graph.push(format!("{joined}[q{i}_{k}]acrossfade=d={:.6}:c1=tri:c2=tri{to}", 2.0 * fades[k - 1]));
                joined = to;
            }
            graph.push(format!("{joined}{exact},asplit=2[m{i}][s{i}]"));
        }
        if t.muted {
            graph.push(format!("[m{i}]anullsink"));
        } else {
            mix_inputs.push(format!("[m{i}]"));
        }
        maps.push(format!("[s{i}]"));
        titles.push(
            info.audio
                .iter()
                .find(|a| a.index == t.index)
                .map(|a| a.label())
                .unwrap_or_else(|| format!("Track {}", t.index + 1)),
        );
    }
    let has_audio = !edit.tracks.is_empty();
    if has_audio {
        if mix_inputs.is_empty() {
            // Everything muted: keep a silent mix so the layout stays the same.
            graph.push(format!("anullsrc=r=48000:cl=stereo,atrim=end={len:.6}[mix]"));
        } else {
            graph.push(format!(
                "{}amix=inputs={}:normalize=0:duration=longest[mix]",
                mix_inputs.concat(),
                mix_inputs.len()
            ));
        }
    }

    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video);
    // To the last part's end (the parts are cut out of that in the graph).
    cmd.args(["-ss", &format!("{from:.6}"), "-t", &format!("{:.6}", edit.end - from), "-i"]).arg(source);
    cmd.args(["-map", "0:v:0", "-c:v", "copy"]);
    // Only the mix: the file's one track is the mix, and it's in its own mix.
    let tag = if edit.output.mix_only { clip_tag(&[0], id) } else { render_tag(edit, id) };
    cmd.args(["-metadata", &format!("comment={tag}")]);
    if has_audio {
        if edit.output.mix_only {
            // The sources aren't written: each one's split ends here.
            for m in &maps {
                graph.push(format!("{m}anullsink"));
            }
            maps.clear();
            titles.clear();
        }
        cmd.args(["-filter_complex", &graph.join(";"), "-map", "[mix]"]);
        for m in &maps {
            cmd.args(["-map", m]);
        }
        cmd.args(["-c:a", "aac", "-b:a", &format!("{RENDER_AUDIO_KBPS}k")]);
        let all_titles = std::iter::once("Mix".to_owned()).chain(titles);
        for (i, t) in all_titles.enumerate() {
            cmd.args([format!("-metadata:s:a:{i}"), format!("title={t}")]);
            cmd.args([format!("-metadata:s:a:{i}"), format!("handler_name={t}")]);
        }
    }
    cmd.args(["-movflags", "+faststart"]).arg(out);
    let r = run_progress(&mut cmd, edit.duration(), progress);
    if r.is_err() {
        let _ = std::fs::remove_file(out);
    }
    r
}

/// Run an ffmpeg command, turning failure into an error with its last log line.
fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("render failed: {}", err.lines().last().unwrap_or("unknown error"));
    }
    Ok(())
}

/// Run ffmpeg reporting progress through `-progress`: `out_time_us` against the
/// expected output `duration`, as 0..=1.
fn run_progress(cmd: &mut Command, duration: f64, progress: &dyn Fn(f32)) -> Result<()> {
    use std::io::BufRead;
    let mut child = cmd
        .args(["-progress", "pipe:1", "-nostats"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().expect("piped");
    // Drain stderr on its own thread so a chatty ffmpeg can't block on a full pipe.
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::BufReader::new(stderr).read_to_string(&mut s);
        s
    });
    for line in std::io::BufReader::new(child.stdout.take().expect("piped")).lines().map_while(|l| l.ok()) {
        if let Some(us) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = us.trim().parse::<f64>() {
                progress(((us / 1e6) / duration.max(1e-6)).clamp(0.0, 1.0) as f32);
            }
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("render failed: {}", err.lines().last().unwrap_or("unknown error"));
    }
    progress(1.0);
    Ok(())
}

/// The keyframes in `spans` (and up to a second before each), as
/// [`keyframe_times`] reads them: from the packets' flags, nothing decoded,
/// and only what's kept read (the render reads it next anyway). Decoding
/// every keyframe of the file took 1.1 s for a 2-minute 1080p60 clip (and
/// read all 411 MB of it); this, for 20 s of it, 0.07 s (72 MB).
fn keyframes(source: &Path, spans: &[(f64, f64)]) -> Result<Vec<f64>> {
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    let read: Vec<String> = spans.iter().map(|(a, b)| format!("{:.6}%{:.6}", (a - 1.0).max(0.0), b)).collect();
    keyframe_flags(source, Some(&read.join(",")))
}

fn is_h264(source: &Path) -> bool {
    ffprobe()
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(source)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "h264")
}

/// The H.264 encoder for re-encoded cuts: the platform's hardware one on
/// macOS; elsewhere x264 when this ffmpeg build has it (LGPL builds don't),
/// else Media Foundation on Windows, or on Linux the GPU's (NVENC, VA-API)
/// or OpenH264 — whichever really works (an ffmpeg lists NVENC without an
/// NVIDIA card).
fn video_encoder() -> &'static str {
    static CHOSEN: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CHOSEN.get_or_init(|| {
        if cfg!(target_os = "macos") {
            return "h264_videotoolbox";
        }
        let listed = ffmpeg_list("-encoders");
        let has = |name: &str| listed.split_whitespace().any(|w| w == name);
        if has("libx264") {
            return "libx264";
        }
        if cfg!(windows) && has("h264_mf") {
            return "h264_mf";
        }
        if cfg!(target_os = "linux") {
            if let Some(e) = ["h264_nvenc", "h264_vaapi", "libopenh264"].into_iter().find(|e| has(e) && encodes(e)) {
                return e;
            }
        }
        "libx264"
    })
}

/// `-encoders` / `-decoders` output of the ffmpeg in use.
fn ffmpeg_list(what: &str) -> String {
    ffmpeg()
        .args(["-hide_banner", what])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Where VA-API encodes: the first GPU render node.
fn vaapi_device(encoder: &str) -> Vec<String> {
    if encoder != "h264_vaapi" {
        return Vec::new();
    }
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("renderD")).collect())
        .unwrap_or_default();
    nodes.sort();
    let node = nodes.first().map_or("/dev/dri/renderD128".to_owned(), |n| format!("/dev/dri/{n}"));
    vec!["-vaapi_device".to_owned(), node]
}

/// Whether `encoder` can really encode a few frames here.
fn encodes(encoder: &str) -> bool {
    let mut filters = "format=yuv420p".to_owned();
    if encoder == "h264_vaapi" {
        filters = "format=nv12,hwupload".to_owned();
    }
    ffmpeg()
        .args(["-hide_banner", "-loglevel", "error"])
        .args(vaapi_device(encoder))
        .args(["-f", "lavfi", "-i", "color=c=black:s=320x180:r=30", "-frames:v", "5", "-vf", &filters, "-c:v", encoder, "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What's wrong with the ffmpeg in use, if anything: missing, or unable to
/// read or write H.264 (Fedora's own `ffmpeg-free` can do neither without
/// extra codecs). Takes a moment: call it off the UI thread.
pub fn problem() -> Option<String> {
    if ffmpeg().arg("-version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        return Some(if cfg!(target_os = "linux") {
            "ffmpeg isn't installed, so clips can't be played, edited or shown. Install it with your package manager \
             (on Fedora, RPM Fusion's `ffmpeg`)."
                .into()
        } else {
            "ffmpeg isn't installed, so clips can't be played, edited or shown.".into()
        });
    }
    let decoders = ffmpeg_list("-decoders");
    let decodes = decoders.split_whitespace().any(|w| w == "h264" || w == "h264_cuvid" || w == "libopenh264");
    // Listed doesn't mean usable: Fedora's ffmpeg lists OpenH264 against a stub library.
    let usable = decodes
        && ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=64x64:r=30", "-frames:v", "2"])
            .args(["-c:v", video_encoder(), "-f", "h264", "-"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success() && !o.stdout.is_empty())
            .is_some_and(|o| {
                let mut child = match ffmpeg()
                    .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "-", "-f", "null", "-"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(_) => return false,
                };
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(&o.stdout);
                }
                child.wait().is_ok_and(|s| s.success())
            });
    if usable {
        return None;
    }
    Some(if cfg!(target_os = "linux") {
        "This ffmpeg can't read or write H.264 video, so clips can't be played, edited or shown. Fedora's own \
         ffmpeg leaves it out: install RPM Fusion's (`sudo dnf swap ffmpeg-free ffmpeg --allowerasing`), or the \
         `openh264` package."
            .into()
    } else {
        "This ffmpeg can't read or write H.264 video, so clips can't be played, edited or shown.".into()
    })
}

/// Ticks per second of the video track (the denominator of its time base).
fn source_timescale(source: &Path) -> Option<u32> {
    let out = ffprobe()
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=time_base", "-of", "csv=p=0"])
        .arg(source)
        .output()
        .ok()?;
    let tb = String::from_utf8_lossy(&out.stdout);
    let (_, den) = tb.trim().split_once('/')?;
    den.parse().ok().filter(|&d| d > 0)
}

fn source_video_kbps(source: &Path) -> Option<u64> {
    let out = ffprobe()
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=bit_rate", "-of", "csv=p=0"])
        .arg(source)
        .output()
        .ok()?;
    let bps: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some((bps / 1000).max(2_000))
}

/// Peak and RMS of a block of interleaved samples, in linear units.
pub fn levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    for &s in samples {
        peak = peak.max(s.abs());
        sum += (s as f64) * (s as f64);
    }
    (peak, (sum / samples.len() as f64).sqrt() as f32)
}

/// Linear amplitude → dBFS (floored at -90).
pub fn to_db(x: f32) -> f32 {
    if x <= 1.6e-5 { SILENT_DB } else { 20.0 * x.log10() }
}

/// dB → linear gain.
pub fn from_db(db: f32) -> f32 {
    if db <= SILENT_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading an MP4's or MOV's index here gives what ffprobe gives, for
    /// files like ours (named tracks, our tag) and others: B-frames, colour
    /// tagged or not and guessed by size, QuickTime's own comment, no sound,
    /// a film's frame rate, a copy starting on a keyframe; a fragmented
    /// one is left to ffprobe.
    #[test]
    fn probing_mp4_matches_ffprobe() {
        let dir = std::env::temp_dir().join(format!("hc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let make = |name: &str, args: &[&str]| -> Option<PathBuf> {
            let out = dir.join(name);
            let ok = ffmpeg()
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(args)
                .arg(&out)
                .stdin(Stdio::null())
                .status()
                .ok()?
                .success();
            ok.then_some(out)
        };
        let tone = |f: u32| format!("sine=f={f}:d=2");
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("ours.mp4", vec![
                "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=60:d=2", "-f", "lavfi", "-i", &tone(440), "-f", "lavfi", "-i", &tone(660), "-f", "lavfi", "-i", &tone(880),
                "-map", "0", "-map", "1", "-map", "2", "-map", "3", "-c:v", "libx264", "-bf", "0", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-c:a", "aac",
                "-metadata", "comment=hasteclip:mix=1,2 id=0123456789abcdef",
                "-metadata:s:a:0", "title=Mix", "-metadata:s:a:0", "handler_name=Mix",
                "-metadata:s:a:1", "title=Desktop", "-metadata:s:a:1", "handler_name=Desktop",
                "-metadata:s:a:2", "handler_name=Microphone",
            ].into_iter().map(String::from).collect()),
            ("bframes.mp4", ["-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=2", "-f", "lavfi", "-i", "sine=d=2", "-c:v", "libx264", "-bf", "3", "-c:a", "aac"].iter().map(|s| s.to_string()).collect()),
            ("small709.mp4", ["-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=1", "-c:v", "libx264", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"].iter().map(|s| s.to_string()).collect()),
            ("big601.mp4", ["-f", "lavfi", "-i", "testsrc2=s=1280x720:r=30:d=1", "-c:v", "libx264", "-colorspace", "smpte170m", "-color_primaries", "smpte170m", "-color_trc", "smpte170m"].iter().map(|s| s.to_string()).collect()),
            ("colr.mp4", ["-f", "lavfi", "-i", "testsrc2=s=1280x720:r=30:d=1", "-c:v", "libx264", "-colorspace", "bt470bg", "-movflags", "+write_colr"].iter().map(|s| s.to_string()).collect()),
            ("quicktime.mov", ["-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=30:d=1", "-f", "lavfi", "-i", "sine=d=1", "-c:v", "libx264", "-c:a", "aac", "-metadata", "comment=hasteclip:mix=1", "-metadata:s:a:0", "title=Game"].iter().map(|s| s.to_string()).collect()),
            ("film.mp4", ["-f", "lavfi", "-i", "testsrc2=s=1280x720:r=24000/1001:d=2", "-c:v", "libx264"].iter().map(|s| s.to_string()).collect()),
            // Copied from a keyframe (as freeing up space trims): the
            // picture edited to start late, its movie length longer than
            // its streams'.
            ("copied.mp4", ["-ss", "0.4", "-to", "1.7", "-i", &dir.join("bframes.mp4").to_string_lossy(), "-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"].iter().map(|s| s.to_string()).collect()),
        ];
        let mut checked = 0;
        for (name, args) in &cases {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let Some(file) = make(name, &args) else {
                eprintln!("{name}: ffmpeg couldn't make it (no ffmpeg or x264?): not checked");
                continue;
            };
            let (ours, theirs) = (probe_mp4(&file).unwrap(), probe_ffprobe(&file).unwrap());
            eprintln!("{name}: {ours:?}");
            assert!((ours.duration - theirs.duration).abs() < 0.002, "{name}: duration {} against {}", ours.duration, theirs.duration);
            assert!((ours.fps - theirs.fps).abs() < 0.01, "{name}: fps {} against {}", ours.fps, theirs.fps);
            assert_eq!((ours.width, ours.height), (theirs.width, theirs.height), "{name}: size");
            let (a, b) = (ours.video_kbps.unwrap_or(0) as f64, theirs.video_kbps.unwrap_or(0) as f64);
            assert!((a - b).abs() <= b * 0.02 + 1.0, "{name}: {a} kbps against {b}");
            let tracks = |i: &ClipInfo| i.audio.iter().map(|a| (a.index, a.title.clone(), a.in_mix)).collect::<Vec<_>>();
            assert_eq!(tracks(&ours), tracks(&theirs), "{name}: audio tracks");
            assert_eq!(ours.id, theirs.id, "{name}: id");
            assert_eq!(ours.bt601, theirs.bt601, "{name}: BT.601");
            checked += 1;
        }
        // Turned to be shown upright (a phone's): known, so exports leave it
        // to ffmpeg, which turns it.
        if let Some(upright) = make("upright.mp4", &["-display_rotation", "90", "-i", &dir.join("small709.mp4").to_string_lossy(), "-c", "copy"]) {
            assert!(mp4::turned(&upright), "turned");
            assert!(!mp4::turned(&dir.join("small709.mp4")), "not turned");
        }
        // Fragmented (a recording a crash cut off): ffprobe reads it.
        if let Some(file) = make("fragmented.mp4", &["-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=2", "-c:v", "libx264", "-movflags", "frag_keyframe+empty_moov"]) {
            assert!(probe_mp4(&file).is_err(), "fragments aren't read here");
            assert!((probe(&file).unwrap().duration - 2.0).abs() < 0.05, "a fragmented file's length");
        }
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("{checked} of {} files checked", cases.len());
    }

    #[test]
    fn fader_law() {
        for db in [SILENT_DB, -60.0, -45.0, -30.0, -6.0, 0.0, 12.0, FADER_MAX_DB] {
            assert!((fader_db(fader_pos(db)) - db).abs() < 1e-3, "{db}");
        }
        assert_eq!(fader_pos(SILENT_DB), 0.0);
        assert_eq!(fader_pos(FADER_MAX_DB), 1.0);
        // Even in dB above -30: each 6 dB is the same distance.
        let step = |a: f32| fader_pos(a + 6.0) - fader_pos(a);
        assert!((step(-24.0) - step(12.0)).abs() < 1e-5);
        assert!((fader_pos(0.0) - 0.667).abs() < 0.01, "0 dB about two thirds up");
        assert_eq!(from_db(SILENT_DB), 0.0);
    }

    #[test]
    fn fades_from_silence_follow_the_fader() {
        let track = TrackEdit {
            index: 0,
            gain: 1.0,
            muted: false,
            points: vec![VolumePoint { t: 0.0, db: SILENT_DB }, VolumePoint { t: 1.0, db: 0.0 }],
        };
        // Halfway through the fade is halfway along the fader, not -48 dB.
        let mid = track.db_at(0.5);
        assert!((fader_pos(mid) - fader_pos(0.0) / 2.0).abs() < 1e-4, "{mid}");
        assert!(mid > -40.0);
        assert_eq!(track.gain_at(0.0), 0.0);
        // The render expression is pieced along the curve and silent at the start.
        let expr = track.volume_expr(0.0);
        assert!(expr.starts_with("if(lte("), "{expr}");
        assert!(expr.matches("lt(t,").count() >= 8, "{expr}");
    }

    fn info() -> ClipInfo {
        ClipInfo { duration: 60.0, fps: 60.0, width: 3600, height: 2338, video_kbps: Some(20_000), audio: Vec::new(), id: None, bt601: false }
    }

    #[test]
    fn output_settings() {
        let info = info();
        let edit = |output| Edit { start: 0.0, end: 60.0, cuts: Vec::new(), tracks: Vec::new(), output };
        // Nothing chosen, or nothing below the source: the video is copied.
        assert!(!Output::default().changes_video(&info));
        assert!(!Output { height: Some(2338), fps: Some(60), video_kbps: Some(30_000), ..Default::default() }.changes_video(&info));
        assert!(edit(Output { height: Some(4000), ..Default::default() }).is_identity(&info));
        // 1080p keeps the shape, even sizes.
        let o = Output { height: Some(1080), ..Default::default() };
        assert_eq!(o.size(&info), (1662, 1080));
        assert!(!edit(o).is_identity(&info));
        // Smaller with no bitrate chosen: less than the source's.
        let k = o.video_kbps_for(&info, &edit(o)).unwrap();
        assert!(k < 20_000 && k > 5_000, "{k}");
        // Under 10 MB for a minute with no audio: ~1.2 Mbps.
        let fit = Output { max_mb: Some(10), ..Default::default() };
        let k = fit.video_kbps_for(&info, &edit(fit)).unwrap();
        assert!((1_150..1_300).contains(&k), "{k}");
        let bytes = fit.estimate_bytes(&info, &edit(fit)).unwrap();
        assert!(bytes < 10 * 1024 * 1024, "{bytes}");
        // ...in a picture that bitrate can carry.
        assert_eq!((fit.size(&info).1, fit.frame_rate(&info)), (720, 30.0));
    }

    #[test]
    fn crop() {
        // 16:9 out of an ultrawide 3440×1440: the 2560 in the middle.
        let c = Crop::largest(3440, 1440, 16, 9, (1720.0, 720.0));
        assert_eq!(c, Crop { x: 440, y: 0, w: 2560, h: 1440 });
        // Pushed against the edge it's nearest.
        assert_eq!(Crop::largest(3440, 1440, 16, 9, (100.0, 0.0)).x, 0);
        // Even, inside the picture, and none for the whole of it.
        assert_eq!(Crop { x: 3, y: 5, w: 641, h: 361 }.within(1920, 1080), Some(Crop { x: 2, y: 4, w: 640, h: 360 }));
        assert_eq!(Crop { x: 1500, y: 0, w: 640, h: 360 }.within(1920, 1080).unwrap().x, 1280);
        assert_eq!(Crop { x: 0, y: 0, w: 4000, h: 1080 }.within(1920, 1080), None);
        assert_eq!(Crop { x: 0, y: 0, w: 4, h: 4 }.within(1920, 1080).unwrap().w, Crop::MIN);

        // The output is the crop's: its size, re-encoded, and smaller sizes
        // offered below its height.
        let mut info = info();
        (info.width, info.height) = (3440, 1440);
        let o = Output { crop: Some(c), ..Default::default() };
        assert!(o.changes_video(&info));
        assert_eq!(o.size(&info), (2560, 1440));
        assert_eq!(Output { height: Some(1080), ..o }.size(&info), (1920, 1080));
        assert_eq!(Output { height: Some(1440), ..o }.effective(&info).height, None);
        // Fewer pixels, so less bitrate for the same look.
        let edit = Edit { start: 0.0, end: 60.0, cuts: Vec::new(), tracks: Vec::new(), output: o };
        assert!(o.video_kbps_for(&info, &edit).unwrap() < 20_000);
        assert!(!edit.is_identity(&info));
        // A crop of all of it is none.
        assert!(!Output { crop: Some(Crop::whole(3440, 1440)), ..Default::default() }.changes_video(&info));
    }

    #[test]
    fn output_saved_with_the_edit() {
        // Edits saved before export settings existed still open.
        let old: Edit = serde_json::from_str(r#"{"start":1.0,"end":5.0,"tracks":[]}"#).unwrap();
        assert_eq!(old.output, Output::default());
        let mut e = old.clone();
        e.output = Output { height: Some(720), max_mb: Some(10), mix_only: true, ..Default::default() };
        let back: Edit = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.output, e.output);
    }

    /// The parts kept are the trim less its cuts; a split (a cut of no
    /// length) shows as two parts but saves as one.
    #[test]
    fn parts_and_cuts() {
        let mut e = Edit { start: 2.0, end: 20.0, cuts: Vec::new(), tracks: Vec::new(), output: Default::default() };
        assert_eq!(e.parts(), [(2.0, 20.0)]);
        e.set_parts(&[(2.0, 5.0), (5.0, 9.0), (12.0, 20.0)]);
        assert_eq!(e.cuts, [Cut { from: 5.0, to: 5.0 }, Cut { from: 9.0, to: 12.0 }]);
        assert_eq!(e.parts(), [(2.0, 5.0), (5.0, 9.0), (12.0, 20.0)]);
        assert_eq!(e.kept(), [(2.0, 9.0), (12.0, 20.0)], "the split joins up");
        assert_eq!(e.duration(), 15.0);
        // The recording's time in the saved clip, and back.
        assert_eq!(e.to_output(1.0), None, "trimmed off");
        assert_eq!(e.to_output(2.0), Some(0.0));
        assert_eq!(e.to_output(8.0), Some(6.0));
        assert_eq!(e.to_output(10.0), None, "cut out");
        assert_eq!(e.to_output(12.5), Some(7.5));
        assert_eq!(e.to_output(20.0), Some(15.0), "the very end counts");
        assert_eq!(e.to_source(6.0), 8.0);
        assert_eq!(e.to_source(7.0), 12.0, "the first frame after the cut");
        assert_eq!(e.to_source(15.0), 20.0);
        // Trimming the recording's start moves the cuts along.
        let s = e.shifted(1.5);
        assert_eq!((s.start, s.end), (0.5, 18.5));
        assert_eq!(s.cuts, [Cut { from: 3.5, to: 3.5 }, Cut { from: 7.5, to: 10.5 }]);
        // Joins crossfade, less where a part is too short for all of it.
        assert_eq!(join_fades(&[(0.0, 1.0), (2.0, 3.0)]), [JOIN_FADE]);
        let short = join_fades(&[(0.0, 1.0), (2.0, 2.004), (3.0, 4.0)]);
        assert!(short.len() == 2 && short.iter().all(|f| (f - 0.002).abs() < 1e-9), "{short:?}");
    }

    /// A cut makes an edit, and a split alone doesn't; edits saved before
    /// cuts open with none, and an edit without cuts saves as it did.
    #[test]
    fn cuts_change_the_clip() {
        let mut info = info();
        info.audio.clear();
        let mut e = Edit::new(&info);
        e.set_parts(&[(0.0, 30.0), (30.0, 60.0)]);
        assert!(e.is_identity(&info), "a split alone changes nothing");
        e.set_parts(&[(0.0, 30.0), (31.0, 60.0)]);
        assert!(!e.is_identity(&info));
        let old: Edit = serde_json::from_str(r#"{"start":1.0,"end":5.0,"tracks":[]}"#).unwrap();
        assert!(old.cuts.is_empty());
        assert!(!serde_json::to_string(&old).unwrap().contains("cuts"));
        let back: Edit = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.cuts, e.cuts);
    }

    /// Soft blending weighs as much in all as even, centred on the slot's
    /// middle (so the picture stays in time with the sound), from the middle
    /// of the slot before to the middle of the one after.
    #[test]
    fn soft_blend_taps() {
        let sum = |t: &[(i64, f64)]| t.iter().map(|x| x.1).sum::<f64>();
        let centre = |t: &[(i64, f64)]| t.iter().map(|(o, w)| *o as f64 * w).sum::<f64>() / sum(t);
        for looks in [2, 3, 4, 8, 16] {
            let even = blend_taps(looks, BlendShape::Even);
            let soft = blend_taps(looks, BlendShape::Soft);
            assert!((sum(&soft) - looks as f64).abs() < 1e-9, "{looks}: weighs {}", sum(&soft));
            assert!((centre(&soft) - centre(&even)).abs() < 1e-9, "{looks}: centred at {}", centre(&soft));
            let (first, last) = (soft[0].0, soft[soft.len() - 1].0);
            assert!(first >= -(looks as i64 + 1) / 2 && last < looks as i64 * 3 / 2, "{looks}: {first}..={last}");
        }
    }

    /// Even blending's graph is as it was; soft's starts with copies of the
    /// first frame and goes on as long as the branch at the slots' starts.
    #[test]
    fn blend_graphs() {
        assert_eq!(
            blend_graph(3, 60.0, BlendShape::Even),
            "split=3[b0][b1][b2];[b0]trim=start_frame=0,framestep=3,setpts=N/60/TB[g0];[b1]trim=start_frame=1,framestep=3,setpts=N/60/TB[g1];[b2]trim=start_frame=2,framestep=3,setpts=N/60/TB[g2];[g0][g1][g2]mix=inputs=3"
        );
        let soft = blend_graph(4, 60.0, BlendShape::Soft);
        assert!(soft.starts_with("tpad=start=2:start_mode=clone,split=8[b0]"), "{soft}");
        assert!(soft.contains("[b2]trim=start_frame=2,framestep=4,"), "{soft}");
        assert!(soft.contains(";[g2][g0][g1][g3]"), "the slots' own branch first: {soft}");
        assert!(soft.ends_with(":duration=first"), "{soft}");
    }

    #[test]
    fn clip_tags() {
        assert_eq!(clip_tag(&[1, 2], None), "hasteclip:mix=1,2");
        assert_eq!(clip_tag(&[1], Some("ab12")), "hasteclip:mix=1 id=ab12");
        assert_eq!(parse_clip_tag("hasteclip:mix=1,2,3"), ClipTag { mix: Some(vec![1, 2, 3]), id: None });
        assert_eq!(parse_clip_tag(&format!("{LEGACY_TAG}mix=1 id=ab12")), ClipTag { mix: Some(vec![1]), id: Some("ab12".into()) }, "the older spelling");
        assert_eq!(parse_clip_tag("hasteclip:mix=1 id=ab12"), ClipTag { mix: Some(vec![1]), id: Some("ab12".into()) });
        assert_eq!(parse_clip_tag("hasteclip:mix="), ClipTag { mix: Some(vec![]), id: None });
        assert_eq!(parse_clip_tag("made with something else"), ClipTag::default());
    }
}

#[cfg(test)]
mod cut_render_tests {
    use super::*;

    /// A 6 s clip made by ffmpeg: a picture that changes every frame, 30 fps
    /// with a keyframe each second (as recordings have), and a tone that's a
    /// quarter of a cycle on each second, so a cut from one whole second to
    /// another jumps from its middle to its trough.
    fn clip(path: &Path) {
        let ok = ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30:duration=6"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440.25:duration=6:sample_rate=48000"])
            .args(["-map", "0", "-map", "1", "-c:v", "libx264", "-g", "30", "-bf", "0", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k"])
            .arg(path)
            .status()
            .unwrap();
        assert!(ok.success());
    }

    /// How far apart two frames are: the mean difference of their pixels.
    fn apart(a: &Frame, b: &Frame) -> f64 {
        a.rgba.iter().zip(&b.rgba).map(|(x, y)| x.abs_diff(*y) as f64).sum::<f64>() / a.rgba.len() as f64
    }

    /// The keyframes a save cuts on, read from the packets' flags over only
    /// the parts kept, are the ones decoding the whole file finds in them.
    #[test]
    fn keyframes_of_the_parts_kept_are_the_decoded_ones() {
        let dir = std::env::temp_dir().join(format!("hc-media-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        clip(&src);
        let spans = [(1.2, 2.6), (3.4, 4.7)];
        let kept = |keys: Vec<f64>| -> Vec<f64> { keys.into_iter().filter(|k| spans.iter().any(|&(a, b)| (a..=b).contains(k))).collect() };
        let read = kept(keyframes(&src, &spans).unwrap());
        // How a save found them before: every keyframe of the file, decoded.
        let out = ffprobe()
            .args(["-v", "error", "-select_streams", "v:0", "-skip_frame", "nokey", "-show_entries", "frame=pts_time", "-of", "csv=p=0"])
            .arg(&src)
            .output()
            .unwrap();
        let decoded = kept(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect());
        assert_eq!(read, [2.0, 4.0]);
        assert_eq!(read, decoded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clip with a stretch cut out of its middle saves as the parts kept,
    /// joined: as long as they are together, the frame after the cut the one
    /// after it in the recording, and the sound joined without a click.
    #[test]
    fn cut_out_of_the_middle() {
        let dir = std::env::temp_dir().join(format!("hc-media-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (src, out) = (dir.join("clip.mp4"), dir.join("cut.mp4"));
        clip(&src);
        let info = probe(&src).unwrap();
        let mut edit = Edit::new(&info);
        // 0.5–2.0 (no two keyframes in it: encoded) and 3.0–4.6 (copied from
        // its keyframes at 3 and 4, its end encoded).
        edit.set_parts(&[(0.5, 2.0), (3.0, 4.6)]);
        assert_eq!(plan(&src, &info, &edit), [Piece::Encode(0.5, 2.0), Piece::Copy(3.0, 4.0), Piece::Encode(4.0, 4.6)]);
        render_to(&src, &info, &edit, &out, None).unwrap();

        let made = probe(&out).unwrap();
        assert!((made.duration - 3.1).abs() < 0.04, "lasts {} s, not 3.1", made.duration);
        // Every frame kept, none more: 45 + 48.
        let frames = ffprobe().args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]).arg(&out).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&frames.stdout).trim(), "93");
        // Either side of the join: the last frame before the cut, the first after.
        let at = |p: &Path, t: f64| frame_at(p, t, 64).unwrap();
        let (before, after) = (at(&out, 1.49), at(&out, 1.51));
        assert!(apart(&before, &at(&src, 1.98)) < apart(&before, &at(&src, 1.95)), "the frame before the cut is the recording's last before it");
        assert!(apart(&after, &at(&src, 3.01)) < apart(&after, &at(&src, 3.05)), "the frame after the cut is the recording's first after it");
        assert!(apart(&after, &at(&src, 3.01)) < apart(&after, &at(&src, 2.01)), "and not one cut out");

        // The sound: no step bigger than the tone itself makes (a hard cut
        // here jumps by its whole height, 0.125), and no gap at the join.
        let sound = decode_audio(&out, 1).unwrap();
        let left: Vec<f32> = sound.chunks(2).map(|s| s[0]).collect();
        let join = (1.5 * PREVIEW_RATE as f64) as usize;
        let step = left[join - 2400..join + 2400].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(step < 0.03, "a click at the join: a step of {step}");
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        assert!(rms(&left[join - 240..join + 240]) > 0.5 * rms(&left[join - 4800..join - 2400]), "the sound dips at the join");
        // As long as the picture, to a few samples: a part short of sound
        // (cut from where a seek landed) leaves the picture a frame short.
        assert!((left.len() as f64 / PREVIEW_RATE as f64 - 3.1).abs() < 0.002, "the sound lasts {} s", left.len() as f64 / PREVIEW_RATE as f64);

        // From 2 s in, where a seek in the sound lands 5 ms late: still every
        // sample and every frame.
        edit.set_parts(&[(2.0, 2.5), (3.0, 3.5)]);
        render_to(&src, &info, &edit, &out, None).unwrap();
        let frames = ffprobe().args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]).arg(&out).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&frames.stdout).trim(), "30");
        let sound = decode_audio(&out, 1).unwrap();
        assert!((sound.len() as f64 / 2.0 / PREVIEW_RATE as f64 - 1.0).abs() < 0.002, "the sound lasts {} s", sound.len() as f64 / 2.0 / PREVIEW_RATE as f64);

        // Every track muted, the mix made of silence: still every frame, and
        // every sound as long as the picture. (Ended by the shortest stream,
        // the silence, made in whole AAC frames, ran short of the parts and
        // took the picture's last frames with it.)
        edit.set_parts(&[(0.5, 2.0), (3.0, 4.6)]);
        edit.tracks.iter_mut().for_each(|t| t.muted = true);
        render_to(&src, &info, &edit, &out, None).unwrap();
        let frames = ffprobe().args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"]).arg(&out).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&frames.stdout).trim(), "93", "every frame, every track muted");
        for stream in 0..2 {
            let secs = decode_audio(&out, stream).unwrap().len() as f64 / 2.0 / PREVIEW_RATE as f64;
            assert!((secs - 3.1).abs() < 0.002, "sound {stream} lasts {secs} s, every track muted");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod jpeg_tests {
    /// Colour accuracy and cost of JPEG settings on a real frame:
    /// `JPEG_FRAME=<1280x720 rgba> cargo test --release -p media jpeg_settings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn jpeg_settings() {
        let Some(path) = std::env::var_os("JPEG_FRAME") else { return };
        let rgba = std::fs::read(path).unwrap();
        let (w, h) = (1280u16, 720u16);
        let grey = [0x2E, 0x32, 0x38];
        for (q, sub) in [(75u8, true), (90, true), (95, true), (90, false), (95, false)] {
            let t = std::time::Instant::now();
            let mut out = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut out, q);
            enc.set_sampling_factor(if sub { jpeg_encoder::SamplingFactor::F_2_2 } else { jpeg_encoder::SamplingFactor::F_1_1 });
            enc.encode(&rgba, w, h, jpeg_encoder::ColorType::Rgba).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            // Decode with a tiny decoder-free check: re-encode isn't possible, so
            // write it out for ffmpeg to read back.
            let file = std::env::temp_dir().join(format!("jt_{q}_{sub}.jpg"));
            std::fs::write(&file, &out).unwrap();
            let back = std::process::Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&file).args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]).output().unwrap().stdout;
            let (mut n, mut err, mut worst) = (0u64, [0i64; 3], 0u8);
            let mut all = 0u64;
            for (a, b) in rgba.chunks_exact(4).zip(back.chunks_exact(4)) {
                for c in 0..3 {
                    all += a[c].abs_diff(b[c]) as u64;
                }
                if a[..3] == grey {
                    n += 1;
                    for c in 0..3 {
                        err[c] += b[c] as i64 - a[c] as i64;
                        worst = worst.max(a[c].abs_diff(b[c]));
                    }
                }
            }
            println!(
                "q{q} {}: {:>3} KB, {ms:.1} ms; mean error {:.2}; {n} px of 2E3238: mean shift ({:+.2},{:+.2},{:+.2}) worst {worst}",
                if sub { "4:2:0" } else { "4:4:4" },
                out.len() / 1024,
                all as f64 / (rgba.len() / 4 * 3) as f64,
                err[0] as f64 / n.max(1) as f64,
                err[1] as f64 / n.max(1) as f64,
                err[2] as f64 / n.max(1) as f64
            );
        }
    }
}

#[cfg(test)]
mod export_bench {
    use super::*;

    /// A video's frames as their luma planes, averaged down in blocks to
    /// about `w` wide (straight from the decoder, so no scaler or colour
    /// conversion of ffmpeg's can differ between files), and its streams'
    /// lengths (video, then each audio).
    fn frames_of(file: &Path, w: u32) -> (Vec<Vec<u8>>, Vec<f64>) {
        let info = probe(file).unwrap();
        let (width, height) = (info.width as usize, info.height as usize);
        let out = ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(file)
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
            .output()
            .unwrap();
        let block = (width / w as usize).max(1);
        let (bw, bh) = (width / block, height / block);
        let frames = out
            .stdout
            .chunks(width * height * 3 / 2)
            .map(|f| {
                let mut small = Vec::with_capacity(bw * bh);
                for by in 0..bh {
                    for bx in 0..bw {
                        let mut sum = 0u32;
                        for y in by * block..(by + 1) * block {
                            sum += f[y * width + bx * block..y * width + (bx + 1) * block].iter().map(|&v| v as u32).sum::<u32>();
                        }
                        small.push((sum / (block * block) as u32) as u8);
                    }
                }
                small
            })
            .collect();
        let lengths = ffprobe()
            .args(["-v", "error", "-show_entries", "stream=duration", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap();
        let lengths = String::from_utf8_lossy(&lengths.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect();
        (frames, lengths)
    }

    /// The same edits made on the graphics card and by ffmpeg come out the
    /// same: as many frames, each showing the same moment (the pictures
    /// differ only by scaling and encoding: under a level in 8×8 blocks),
    /// the sound as long as the picture. HC_SRC: a 60 fps clip with sound;
    /// HC_FAST: a 540 fps one. HASTECLIP_ENCODER picks the card's encoder.
    #[test]
    #[ignore = "needs clips and a graphics card"]
    fn card_exports_match_ffmpegs() {
        let dir = std::env::temp_dir().join(format!("hc-export-match-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = PathBuf::from(std::env::var("HC_SRC").unwrap());
        let fast = std::env::var("HC_FAST").ok().map(PathBuf::from);
        type MakeEdit = Box<dyn Fn(&ClipInfo) -> Edit>;
        let mut cases: Vec<(&str, PathBuf, MakeEdit)> = vec![
            ("cuts at 720p", src.clone(), Box::new(|i: &ClipInfo| {
                let mut e = Edit { start: 3.0, end: 40.0, cuts: vec![Cut { from: 10.0, to: 17.5 }, Cut { from: 25.25, to: 31.0 }], ..Edit::new(i) };
                e.output.height = Some(720);
                e
            })),
            ("crop at 30 fps", src.clone(), Box::new(|i: &ClipInfo| {
                let mut e = Edit { start: 5.0, end: 20.0, ..Edit::new(i) };
                e.output.crop = Some(Crop { x: 320, y: 180, w: 1920, h: 1080 });
                e.output.fps = Some(30);
                e
            })),
        ];
        if let Some(fast) = &fast {
            for shape in [BlendShape::Even, BlendShape::Soft] {
                cases.push((if shape == BlendShape::Even { "blend even" } else { "blend soft" }, fast.clone(), Box::new(move |i: &ClipInfo| {
                    let mut e = Edit { start: 2.0, end: 12.0, ..Edit::new(i) };
                    e.output.fps = Some(60);
                    e.output.blend_fps = Some(540);
                    e.output.blend_shape = shape;
                    e
                })));
            }
        }
        for (name, source, make) in &cases {
            let info = probe(source).unwrap();
            let edit = make(&info);
            let card = dir.join("card.mp4");
            let theirs = dir.join("ffmpeg.mp4");
            let t = std::time::Instant::now();
            render_to(source, &info, &edit, &card, None).unwrap();
            let card_s = t.elapsed().as_secs_f64();
            // SAFETY: the tests here run one at a time (--test-threads=1).
            unsafe { std::env::set_var("HASTECLIP_EXPORT_FFMPEG", "1") };
            let t = std::time::Instant::now();
            render_to(source, &info, &edit, &theirs, None).unwrap();
            let ffmpeg_s = t.elapsed().as_secs_f64();
            unsafe { std::env::remove_var("HASTECLIP_EXPORT_FFMPEG") };
            let (a, a_len) = frames_of(&card, 160);
            let (b, b_len) = frames_of(&theirs, 160);
            let diffs: Vec<f64> = a.iter().zip(&b).map(|(x, y)| x.iter().zip(y).map(|(p, q)| p.abs_diff(*q) as f64).sum::<f64>() / x.len() as f64).collect();
            let worst = diffs.iter().copied().fold(0.0, f64::max);
            let mean = diffs.iter().sum::<f64>() / diffs.len().max(1) as f64;
            eprintln!("{name}: {:.1} s on the card, {:.1} s by ffmpeg; {} frames against {}; off by {mean:.2} on average, {worst:.2} at worst; lengths {a_len:?} against {b_len:?}", card_s, ffmpeg_s, a.len(), b.len());
            assert_eq!(a.len(), b.len(), "{name}: frames");
            assert!(worst < 1.0, "{name}: a frame off by {worst:.2}");
            for (x, y) in a_len.iter().zip(&b_len) {
                assert!((x - y).abs() < 0.03, "{name}: a stream {x} s long against {y}");
            }
            if a_len.len() > 1 {
                assert!((a_len[0] - a_len[1]).abs() < 0.05, "{name}: picture {} s, sound {} s", a_len[0], a_len[1]);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// How long exports take (HC_SRC: a 60 fps clip with sound; HC_FAST: a
    /// 540 fps recording).
    #[test]
    #[ignore = "benchmark"]
    fn export_times() {
        let dir = std::env::temp_dir().join(format!("hc-export-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // CPU seconds this process and the ffmpegs it waited for have used.
        #[cfg(not(target_os = "macos"))]
        let cpu = || {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
            let fields: Vec<f64> = stat.rsplit(')').next().unwrap_or_default().split_whitespace().filter_map(|f| f.parse().ok()).collect();
            // utime, stime, cutime, cstime: fields 14-17; here from ppid
            // (field 4) on, the state (a letter) left out.
            fields.get(10..14).map_or(0.0, |t| t.iter().sum::<f64>() / 100.0)
        };
        #[cfg(target_os = "macos")]
        let cpu = || {
            let used = |who| {
                // SAFETY: getrusage only writes the struct it's given.
                let mut u: libc::rusage = unsafe { std::mem::zeroed() };
                unsafe { libc::getrusage(who, &mut u) };
                let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
                secs(u.ru_utime) + secs(u.ru_stime)
            };
            used(libc::RUSAGE_SELF) + used(libc::RUSAGE_CHILDREN)
        };
        let run = |name: &str, src: &Path, edit: &dyn Fn(&ClipInfo) -> Edit| {
            let info = probe(src).unwrap();
            let e = edit(&info);
            let out = dir.join(format!("{}.mp4", name.replace(' ', "-")));
            let (t, used) = (std::time::Instant::now(), cpu());
            render_to(src, &info, &e, &out, None).unwrap();
            let (secs, used) = (t.elapsed().as_secs_f64(), cpu() - used);
            let o = probe(&out).unwrap();
            eprintln!("{name}: {secs:.1} s for {:.1} s of video ({}x{} at {:.0} fps, {:.1} MB), {:.0}% of a core", e.duration(), o.width, o.height, o.fps, std::fs::metadata(&out).unwrap().len() as f64 / 1048576.0, used / secs * 100.0);
        };
        let src = PathBuf::from(std::env::var("HC_SRC").unwrap());
        run("trim", &src, &|i| Edit { start: 10.0, end: 40.0, ..Edit::new(i) });
        run("1080p", &src, &|i| {
            let mut e = Edit::new(i);
            e.output.height = Some(1080);
            e
        });
        run("discord", &src, &|i| {
            let mut e = Edit::new(i);
            e.output.mix_only = true;
            e.output.max_mb = Some(10);
            e
        });
        if let Ok(fast) = std::env::var("HC_FAST") {
            run("blend 540 to 60", Path::new(&fast), &|i| {
                let mut e = Edit::new(i);
                e.output.fps = Some(60);
                e.output.blend_fps = Some(540);
                e
            });
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
