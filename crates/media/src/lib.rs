//! Non-destructive clip editing: probe, decode for preview, and render edits.
//!
//! An edit never touches the original recording: it's an [`Edit`] (trim range and
//! per-track volume) rendered into a new file. Where the original, the edit and
//! the render live is the app's business; this crate only probes and renders.
//!
//! Backed by the `ffmpeg`/`ffprobe` binaries.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Sample rate audio is decoded at for preview playback and waveforms.
pub const PREVIEW_RATE: u32 = 48_000;

/// Probed facts about a clip.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub audio: Vec<AudioStream>,
    /// Permanent id from the clip tag, once the clip has one (see [`clip_tag`]).
    pub id: Option<String>,
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

pub fn probe(source: &Path) -> Result<ClipInfo> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration:format_tags=comment:stream=index,codec_type,width,height,avg_frame_rate,r_frame_rate:stream_tags=title,handler_name,name")
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
        audio,
        id: tag.id,
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

/// The file comment for a clip: `hesteclips:mix=1,2 id=3f9c…`. Recordings start
/// with just the mix; the id is added the first time the clip is edited.
pub fn clip_tag(mix: &[usize], id: Option<&str>) -> String {
    let list: Vec<String> = mix.iter().map(|n| n.to_string()).collect();
    let mut tag = format!("hesteclips:mix={}", list.join(","));
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
    let Some(rest) = comment.strip_prefix("hesteclips:") else { return ClipTag::default() };
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
                let i = pts.partition_point(|p| p.t <= t);
                let (a, b) = (pts[i - 1], pts[i]);
                let f = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
                a.db + (b.db - a.db) * f
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
        // Nested if(): piecewise-linear dB, converted to linear gain.
        let pts: Vec<(f64, f32)> = self.points.iter().map(|p| (p.t - start, p.db)).collect();
        let mut db = format!("{:.3}", pts[pts.len() - 1].1);
        for w in pts.windows(2).rev() {
            let ((t0, d0), (t1, d1)) = (w[0], w[1]);
            let seg = format!("({d0:.3}+({:.3})*(t-({t0:.4}))/{:.4})", d1 - d0, (t1 - t0).max(1e-6));
            db = format!("if(lt(t,{t1:.4}),{seg},{db})");
        }
        db = format!("if(lt(t,{:.4}),{:.3},{db})", pts[0].0, pts[0].1);
        format!("pow(10,({db})/20)")
    }
}

/// Everything about how a clip has been edited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    pub start: f64,
    pub end: f64,
    pub tracks: Vec<TrackEdit>,
}

impl Edit {
    /// The identity edit for a clip: full length, every source at unity.
    pub fn new(info: &ClipInfo) -> Self {
        Self {
            start: 0.0,
            end: info.duration,
            tracks: info
                .source_tracks()
                .iter()
                // A track that wasn't in the clip's mix starts muted, so the rebuilt
                // mix matches the original until you choose to bring it in.
                .map(|a| TrackEdit { index: a.index, gain: 1.0, muted: !a.in_mix, points: Vec::new() })
                .collect(),
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
        self.start <= eps && self.end >= info.duration - eps && self.tracks.iter().all(as_recorded)
    }

    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

fn hidden_sibling(source: &Path, suffix: &str) -> PathBuf {
    let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    source.with_file_name(format!(".{name}{suffix}"))
}

/// Decode one audio stream to interleaved stereo f32 at [`PREVIEW_RATE`].
pub fn decode_audio(source: &Path, stream: usize) -> Result<Vec<f32>> {
    let mut child = Command::new("ffmpeg")
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

fn frame(source: &Path, t: f64, width: u32, exact: bool) -> Result<Frame> {
    let info_h = |w: u32, src_w: u32, src_h: u32| -> u32 {
        // Even height, as ffmpeg's scale=-2 would produce.
        (((w as f64) * src_h as f64 / src_w.max(1) as f64 / 2.0).round() as u32 * 2).max(2)
    };
    // Need the source size to know how many bytes come back.
    let (sw, sh) = video_size(source)?;
    let height = info_h(width, sw, sh);
    let mut cmd = Command::new("ffmpeg");
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
    let out = Command::new("ffprobe")
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
pub fn keyframe_strip(source: &Path, height: u32, mut on_frame: impl FnMut(f64, Frame) -> bool) -> Result<()> {
    let keys = keyframe_times(source)?;
    if keys.is_empty() {
        bail!("no keyframes");
    }
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
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if cfg!(target_os = "macos") {
            cmd.args(["-hwaccel", "videotoolbox"]);
        }
        let mut child = cmd
            .args(["-skip_frame", "nokey", "-noaccurate_seek", "-ss", &format!("{:.4}", (first - 0.01).max(0.0))])
            // A little past the last keyframe so it's included, but not the next range's first.
            .args(["-t", &format!("{:.4}", last - first + 0.02), "-i"])
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
                if out.read_exact(&mut rgba).is_err() || tx.send((t, Frame { width, height, rgba })).is_err() {
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

/// Presentation times of every keyframe, read from the container index.
pub fn keyframe_times(source: &Path) -> Result<Vec<f64>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
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

/// Render `edit` of `source` to `dest` (written to a hidden partial first, so a
/// half-finished file never appears under its real name). `id` goes into the
/// output's clip tag.
pub fn render_to(source: &Path, info: &ClipInfo, edit: &Edit, dest: &Path, id: Option<&str>) -> Result<()> {
    render_with_progress(source, info, edit, dest, id, |_| {})
}

/// Render with progress callbacks (0.0..=1.0).
///
/// "Smart render": only the frames between a cut and the nearest keyframe are
/// re-encoded; everything between the first and last keyframe inside the trim is
/// stream-copied untouched. A 20 s trim of a 60 fps 4K-wide recording drops from
/// ~20 s (full re-encode) to ~3 s, and the copied part is bit-identical to the
/// original. Falls back to a full re-encode when there's no keyframe to copy from.
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
    let work = std::env::temp_dir().join(format!("hesteclips-render-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| -> Result<()> {
        progress(0.0);
        let video = work.join(format!("video.{ext}"));
        let plan = plan(source, info, edit);
        let weights = Weights::new(&plan, edit.duration());
        render_video(source, edit, &plan, &work, &video, &|f| progress(f * weights.video))?;
        // `.name.rendering.mp4`: hidden (dotfile) so the library never shows a
        // half-written clip, and ending in the real extension so ffmpeg picks the
        // container. (Not `with_extension`, which would replace `.mp4` and could
        // eat into the name.)
        let partial = hidden_sibling(dest, &format!(".rendering.{ext}"));
        mux_audio(source, info, edit, id, &video, &partial, &|f| progress(weights.video + f * (1.0 - weights.video)))?;
        std::fs::rename(&partial, dest)?;
        progress(1.0);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// How the video gets made: one full re-encode, or re-encoded edges around a
/// stream-copied middle (start/end keyframes).
enum Plan {
    Encode,
    Smart { k1: f64, k2: f64 },
}

fn plan(source: &Path, info: &ClipInfo, edit: &Edit) -> Plan {
    // Keyframes inside the trim, with a frame of margin from the cuts.
    let fd = info.frame_duration();
    let keys: Vec<f64> = keyframes(source)
        .unwrap_or_default()
        .into_iter()
        .filter(|&k| k >= edit.start - fd / 2.0 && k <= edit.end - fd)
        .collect();
    match (keys.first(), keys.last()) {
        (Some(&k1), Some(&k2)) if k2 - k1 >= 1.0 && is_h264(source) => Plan::Smart { k1, k2 },
        _ => Plan::Encode,
    }
}

/// Rough relative cost of each step, so one progress bar moves at an even pace:
/// encoding ~1 unit per second of video, copying and the audio mux far less.
struct Weights {
    video: f32,
}

impl Weights {
    fn new(plan: &Plan, dur: f64) -> Self {
        let video = match plan {
            Plan::Encode => dur,
            Plan::Smart { k1, k2 } => (dur - (k2 - k1)) + (k2 - k1) * 0.03,
        };
        let audio = dur * 0.04 + 0.3;
        Self { video: (video / (video + audio)) as f32 }
    }
}

/// Video only, trimmed frame-exactly, into `out`. `progress` gets 0..=1.
fn render_video(source: &Path, edit: &Edit, plan: &Plan, work: &Path, out: &Path, progress: &dyn Fn(f32)) -> Result<()> {
    let kbps = source_video_kbps(source).unwrap_or(20_000);
    // Every part gets the source's timescale. Otherwise the muxer picks one per
    // part (copied middle 1/19200, encoded edges 1/15360) and concat, which
    // copies timestamps without rescaling, plays the middle at the wrong rate:
    // a 60 fps trim came out as 48 fps with a fifth of its frames gone.
    let timescale = source_timescale(source).unwrap_or(600).to_string();
    let encode = |from: f64, to: f64, dest: &Path, progress: &dyn Fn(f32)| -> Result<()> {
        run_progress(
            Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(["-ss", &format!("{from:.6}"), "-to", &format!("{to:.6}"), "-i"])
                .arg(source)
                .args(["-map", "0:v:0", "-an", "-c:v", video_encoder(), "-b:v", &format!("{kbps}k")])
                // Same profile as our recordings so the segments can be joined.
                .args(["-profile:v", "high", "-pix_fmt", "yuv420p"])
                .args(["-video_track_timescale", &timescale])
                .arg(dest),
            to - from,
            progress,
        )
    };

    let Plan::Smart { k1, k2 } = *plan else {
        return encode(edit.start, edit.end, out, progress);
    };

    // Share of the video work per part, by the same cost model as `Weights`.
    let (head, mid, tail) = (k1 - edit.start, (k2 - k1) * 0.03, edit.end - k2);
    let total = (head + mid + tail).max(1e-6);
    let (w_head, w_mid) = ((head / total) as f32, (mid / total) as f32);
    let mut parts = Vec::new();
    if head > 1e-3 {
        let path = work.join("head.mp4");
        encode(edit.start, k1, &path, &|f| progress(f * w_head))?;
        parts.push(path);
    }
    let path = work.join("mid.mp4");
    run(Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-ss", &format!("{k1:.6}"), "-to", &format!("{k2:.6}"), "-i"])
        .arg(source)
        .args(["-map", "0:v:0", "-an", "-c", "copy", "-avoid_negative_ts", "make_zero"])
        .args(["-video_track_timescale", &timescale])
        .arg(&path))?;
    parts.push(path);
    progress(w_head + w_mid);
    if tail > 1e-3 {
        let path = work.join("tail.mp4");
        encode(k2, edit.end, &path, &|f| progress(w_head + w_mid + f * (1.0 - w_head - w_mid)))?;
        parts.push(path);
    }

    let list = work.join("parts.txt");
    let body: String = parts.iter().map(|p| format!("file '{}'\n", p.display())).collect();
    std::fs::write(&list, body)?;
    run(Command::new("ffmpeg")
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

    for (i, t) in edit.tracks.iter().enumerate() {
        // eval=frame re-evaluates per audio frame so keyframed volume ramps; `t`
        // there counts from the trim start because the input is seeked.
        graph.push(format!(
            "[1:a:{}]volume='{}':eval=frame,asplit=2[m{i}][s{i}]",
            t.index,
            t.volume_expr(edit.start)
        ));
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
            graph.push("anullsrc=r=48000:cl=stereo[mix]".into());
        } else {
            graph.push(format!(
                "{}amix=inputs={}:normalize=0:duration=longest[mix]",
                mix_inputs.concat(),
                mix_inputs.len()
            ));
        }
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video);
    cmd.args(["-ss", &format!("{:.6}", edit.start), "-t", &format!("{:.6}", edit.duration()), "-i"]).arg(source);
    cmd.args(["-map", "0:v:0", "-c:v", "copy"]);
    cmd.args(["-metadata", &format!("comment={}", render_tag(edit, id))]);
    if has_audio {
        cmd.args(["-filter_complex", &graph.join(";"), "-map", "[mix]"]);
        for m in &maps {
            cmd.args(["-map", m]);
        }
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-shortest"]);
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

/// Keyframe timestamps of the video stream.
fn keyframes(source: &Path) -> Result<Vec<f64>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-skip_frame", "nokey"])
        .args(["-show_entries", "frame=pts_time", "-of", "csv=p=0"])
        .arg(source)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect())
}

fn is_h264(source: &Path) -> bool {
    Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(source)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "h264")
}

fn video_encoder() -> &'static str {
    if cfg!(target_os = "macos") { "h264_videotoolbox" } else { "libx264" }
}

/// Ticks per second of the video track (the denominator of its time base).
fn source_timescale(source: &Path) -> Option<u32> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=time_base", "-of", "csv=p=0"])
        .arg(source)
        .output()
        .ok()?;
    let tb = String::from_utf8_lossy(&out.stdout);
    let (_, den) = tb.trim().split_once('/')?;
    den.parse().ok().filter(|&d| d > 0)
}

fn source_video_kbps(source: &Path) -> Option<u64> {
    let out = Command::new("ffprobe")
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
    if x <= 1e-5 { -90.0 } else { 20.0 * x.log10() }
}

/// dB → linear gain.
pub fn from_db(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_tags() {
        assert_eq!(clip_tag(&[1, 2], None), "hesteclips:mix=1,2");
        assert_eq!(clip_tag(&[1], Some("ab12")), "hesteclips:mix=1 id=ab12");
        assert_eq!(parse_clip_tag("hesteclips:mix=1,2,3"), ClipTag { mix: Some(vec![1, 2, 3]), id: None });
        assert_eq!(parse_clip_tag("hesteclips:mix=1 id=ab12"), ClipTag { mix: Some(vec![1]), id: Some("ab12".into()) });
        assert_eq!(parse_clip_tag("hesteclips:mix="), ClipTag { mix: Some(vec![]), id: None });
        assert_eq!(parse_clip_tag("made with something else"), ClipTag::default());
    }
}
