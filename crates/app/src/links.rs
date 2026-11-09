//! Share links: a clip uploaded to HasteClip's server, with a link that plays
//! it in any browser and right in chats (the server is
//! <https://github.com/hasteclip/api>).
//!
//! Nothing leaves the computer until you pick Share → Share with a link… and
//! Upload, and then only that clip as it looks now: its video and its mix, without
//! the file's metadata (repackaged, not re-encoded, so it's quick), and a
//! picture for the link's preview. A shared clip is at most [`LINK_MAX_MB`]
//! (what the server takes, and what plays right in a chat): a bigger one
//! goes up as a copy that fits. Before uploading, its resolution, frame
//! rate and the size it fits under can be changed in the dialog (the
//! editor's export settings), and a new version of it later goes up the
//! same way.
//!
//! The server knows this app only by a random token it hands out the first
//! time (kept in `<config_dir>/hasteclip/share.json`): there's no account.
//!
//! Shared clips are kept in the library's `.hasteclip/links.json`, by the
//! clip's path inside the library, like collections: renaming or moving a
//! clip keeps its link. Stop sharing deletes the clip from the server.
//!
//! A link shows its clip as it is now, as a shared file does in a cloud
//! drive: the clip on this computer is the real one, the server's a copy
//! kept up to date. Renamed (or moved to another game), its page is
//! renamed; edited, reverted or saved smaller, the new version is uploaded
//! under the same link (which plays the old one until it's all there);
//! deleted, its link stops working, once Undo's had its moment. Without a
//! connection, all of that waits and happens once there is one.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::App;
use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Dismissed as _, Kind};

/// The server. `HASTECLIP_API` points the app at another one (a local one
/// while working on it).
fn api() -> String {
    std::env::var("HASTECLIP_API").unwrap_or_else(|_| "https://api.hasteclip.com".into())
}

/// A shared clip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub id: String,
    pub url: String,
    /// When it was shared, in seconds since 1970.
    pub shared: u64,
    /// What the server has: the name and game on its page, and which file
    /// it was made from. `None` in links kept before these were: taken to
    /// be the clip as it is.
    #[serde(default)]
    pub named: Option<Named>,
    #[serde(default)]
    pub version: Option<Version>,
    /// How it goes up, as chosen when it was shared: a new version goes up
    /// the same way. `None` in links kept before it was asked: as
    /// [`default_output`] makes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<media::Output>,
}

/// What a shared clip's page calls it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Named {
    pub title: String,
    pub game: Option<String>,
}

/// Which file a clip is: its size and when the file was made. A saved
/// edit, a revert or a smaller copy is a new file (its date in the library
/// is kept, so not the modified time); a rename or move is the same one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Version {
    size: u64,
    /// Milliseconds since 1970, where the file system keeps it.
    made: Option<u64>,
}

impl Version {
    pub fn of(clip: &Path) -> Option<Self> {
        let meta = std::fs::metadata(clip).ok()?;
        let made = meta.created().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
        Some(Self { size: meta.len(), made })
    }
}

/// A shared clip deleted here: its link is stopped once Undo has had its
/// moment ([`UNDO_FOR`]), as soon as the server can be reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Deleted {
    /// Where the clip was in the library.
    key: String,
    link: Link,
    /// When, in seconds since 1970.
    at: u64,
}

/// How long a deleted clip's link waits before it's stopped: past the
/// Undo in the toast, so Undo brings the link back too.
const UNDO_FOR: Duration = Duration::from_secs(20);

#[derive(Default, Serialize, Deserialize)]
struct File {
    clips: HashMap<String, Link>,
    #[serde(default)]
    deleted: Vec<Deleted>,
}

/// The shared clips of one library.
#[derive(Default)]
pub struct Links {
    lib: PathBuf,
    clips: HashMap<String, Link>,
    deleted: Vec<Deleted>,
    /// Shared clips whose link shows an older version or name, as of the
    /// last look (for their cards, without looking at their files every frame).
    behind: std::collections::HashSet<String>,
    /// Links that couldn't be brought up to date, by id: when to try again,
    /// and how many tries so far.
    retry: HashMap<String, (Instant, u32)>,
    /// When the clips were last looked at for changes.
    looked: Option<Instant>,
    /// Goes up with every change to which clips are shared.
    pub changes: u64,
}

impl Links {
    pub fn load(lib: &Path) -> Self {
        let file: File = std::fs::read(file_path(lib)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { lib: lib.to_path_buf(), clips: file.clips, deleted: file.deleted, ..Default::default() }
    }

    pub fn library(&self) -> &Path {
        &self.lib
    }

    pub fn get(&self, clip: &Path) -> Option<&Link> {
        self.clips.get(&self.key(clip)?)
    }

    pub fn set(&mut self, clip: &Path, link: Link) {
        let Some(key) = self.key(clip) else { return };
        self.behind.remove(&key);
        self.retry.remove(&link.id);
        self.clips.insert(key, link);
        self.changes += 1;
        self.save();
    }

    pub fn remove(&mut self, clip: &Path) {
        if let Some(key) = self.key(clip)
            && self.clips.remove(&key).is_some()
        {
            self.behind.remove(&key);
            self.changes += 1;
            self.save();
        }
    }

    /// The shared clips, by where they are.
    pub fn all(&self) -> Vec<(PathBuf, Link)> {
        self.clips.iter().map(|(key, link)| (self.lib.join(key), link.clone())).collect()
    }

    /// Whether the clip's link shows an older version or name of it.
    pub fn behind(&self, clip: &Path) -> bool {
        self.key(clip).is_some_and(|k| self.behind.contains(&k))
    }

    /// The clip was deleted here: its link waits [`UNDO_FOR`], then stops.
    pub fn deleted(&mut self, clip: &Path) {
        let Some(key) = self.key(clip) else { return };
        if let Some(link) = self.clips.remove(&key) {
            self.behind.remove(&key);
            self.deleted.push(Deleted { key, link, at: now() });
            self.changes += 1;
            self.save();
        }
    }

    /// The clip was put back (Undo): its link stays, if it hasn't stopped yet.
    pub fn undeleted(&mut self, clip: &Path) {
        let Some(key) = self.key(clip) else { return };
        if let Some(i) = self.deleted.iter().position(|d| d.key == key) {
            let d = self.deleted.remove(i);
            self.clips.insert(key, d.link);
            self.changes += 1;
            self.save();
        }
    }

    /// A deleted clip's link has stopped (or was gone already): forgotten.
    fn forget_deleted(&mut self, id: &str) {
        self.retry.remove(id);
        let before = self.deleted.len();
        self.deleted.retain(|d| d.link.id != id);
        if self.deleted.len() != before {
            self.save();
        }
    }

    /// Bringing the link with `id` up to date didn't work: it's tried again
    /// later, a little later each time.
    fn failed(&mut self, id: &str) {
        let tries = self.retry.get(id).map_or(0, |(_, n)| *n);
        self.retry.insert(id.to_owned(), (Instant::now() + backoff(tries), tries + 1));
    }

    /// Bring a link up to date again now, not when it would next try.
    pub fn try_now(&mut self, clip: &Path) {
        if let Some(link) = self.get(clip) {
            let id = link.id.clone();
            self.retry.remove(&id);
            self.looked = None;
        }
    }

    /// A clip was renamed or moved: its link goes with it (and is renamed
    /// on the server soon after).
    pub fn follow(&mut self, from: &Path, to: &Path) {
        let (Some(from), Some(to)) = (self.key(from), self.key(to)) else { return };
        if let Some(link) = self.clips.remove(&from) {
            self.behind.remove(&from);
            self.clips.insert(to, link);
            self.changes += 1;
            self.looked = None;
            self.save();
        }
    }

    fn key(&self, clip: &Path) -> Option<String> {
        let rel = clip.strip_prefix(&self.lib).ok()?;
        Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
    }

    fn save(&self) {
        let path = file_path(&self.lib);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File { clips: self.clips.clone(), deleted: self.deleted.clone() };
        if let Ok(json) = serde_json::to_vec_pretty(&file) {
            // Written aside, then swapped in: never half a file.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Links to another server (`HASTECLIP_API`) are kept apart, like its token.
fn file_path(lib: &Path) -> PathBuf {
    let name = if std::env::var_os("HASTECLIP_API").is_some() { "links-dev.json" } else { "links.json" };
    lib.join(crate::store::DIR).join(name)
}

/// How far a share has come: bytes sent of the total (0 while the clip is
/// still being got ready), and whether it was called off.
#[derive(Default)]
pub struct Progress {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// A copy is made to go up first (the clip doesn't fit its size, or
    /// it goes up smaller), and how far it's come (0..=1, `f32` bits).
    making: AtomicBool,
    made: AtomicU32,
    /// The copy is made (or wasn't needed): the clip is being got ready to
    /// go up, until `total` is set.
    readying: AtomicBool,
    /// When the first bytes were confirmed and how many were by then, and
    /// the speed since, for how long is left.
    speed: Mutex<Speed>,
}

/// The upload's speed, from what the storage has confirmed.
#[derive(Default)]
struct Speed {
    since: Option<(std::time::Instant, u64)>,
    /// Bytes a second, smoothed over the last few seconds.
    rate: f64,
    at: Option<(std::time::Instant, u64)>,
}

impl Progress {
    /// 0..=1: the upload, or with a smaller copy made first, half that and
    /// half the upload.
    pub fn fraction(&self) -> f32 {
        let (sent, total) = (self.sent.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        let up = if total == 0 { 0.0 } else { (sent as f64 / total as f64).min(1.0) as f32 };
        if self.making.load(Ordering::Relaxed) { 0.5 * f32::from_bits(self.made.load(Ordering::Relaxed)) + 0.5 * up } else { up }
    }

    /// How far the smaller copy has come, while it's being made.
    fn making(&self) -> Option<f32> {
        (self.making.load(Ordering::Relaxed) && !self.readying.load(Ordering::Relaxed) && self.total.load(Ordering::Relaxed) == 0).then(|| f32::from_bits(self.made.load(Ordering::Relaxed)))
    }

    /// How long is left at the speed it's going, once it's going.
    fn left(&self) -> Option<Duration> {
        let (sent, total) = (self.sent.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        let now = std::time::Instant::now();
        let mut s = self.speed.lock().unwrap();
        let since = *s.since.get_or_insert((now, sent));
        if let Some((t, n)) = s.at {
            let dt = now.duration_since(t).as_secs_f64();
            if dt >= 0.5 {
                // Over about the last 5 seconds, so a burst doesn't swing it.
                let rate = sent.saturating_sub(n) as f64 / dt;
                let k = (dt / 5.0).min(1.0);
                s.rate = if s.rate == 0.0 { rate } else { s.rate + (rate - s.rate) * k };
                s.at = Some((now, sent));
            }
        } else {
            s.at = Some((now, sent));
        }
        // A couple of seconds in before it's worth saying.
        (now.duration_since(since.0) >= Duration::from_secs(2) && s.rate > 0.0 && total > sent).then(|| Duration::from_secs_f64((total - sent) as f64 / s.rate))
    }

    fn cancelled(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) { Err("cancelled".into()) } else { Ok(()) }
    }
}

/// Upload `clip` as `output` makes it, and get its link; `named` is what
/// its page calls it.
pub fn share(clip: &Path, named: &Named, output: &media::Output, progress: &Progress) -> Result<Link, String> {
    // Which file, as it was before it was read: if it changes meanwhile,
    // the link is brought up to date after.
    let version = Version::of(clip);
    let done = upload(clip, named, output, progress, None)?;
    Ok(Link { id: done.id, url: done.url, shared: now(), named: Some(named.clone()), version, output: Some(*output) })
}

/// Upload a new version of a shared clip under its link, which plays the
/// old one until this one's all there. It goes up as the clip did.
pub fn replace(clip: &Path, link: &Link, named: &Named, progress: &Progress) -> Result<Link, String> {
    let version = Version::of(clip);
    upload(clip, named, &link.output.unwrap_or_else(default_output), progress, Some(&link.id))?;
    Ok(Link { named: Some(named.clone()), version, ..link.clone() })
}

/// Rename a shared clip's page, or give it another game.
pub fn rename(link: &Link, named: &Named) -> Result<(), String> {
    let server = Server::new();
    server
        .authed_raw(|s, token| {
            s.agent
                .patch(format!("{}/v1/clips/{}", api(), link.id))
                .header("Authorization", format!("Bearer {token}"))
                .send_json(serde_json::json!({ "title": named.title, "game": named.game }))
        })
        .map(drop)
}

/// The most a shared clip can be, in MB as the app counts them (MiB). The
/// server takes nothing bigger (`MAX_CLIP_MB`: sharing is free, its storage
/// isn't), and up to it a link's video plays right in a chat: Discord's
/// line, reported at 100 MB and not documented, counted as its own limits
/// are (its upload limit is "20 MiB" in its API docs, "20MB" to people).
pub(crate) const LINK_MAX_MB: u32 = 100;

/// How a clip goes up unless something else is chosen: as it is, or, past
/// [`LINK_MAX_MB`], as a copy that fits (the biggest picture that does, at
/// up to 60 fps), made as Save smaller makes one. Only its mix goes up
/// either way.
pub fn default_output() -> media::Output {
    media::Output { max_mb: Some(LINK_MAX_MB), mix_only: true, ..Default::default() }
}

/// `output` within what a link can be: never more than [`LINK_MAX_MB`].
fn capped(output: &media::Output) -> media::Output {
    media::Output { max_mb: Some(output.max_mb.map_or(LINK_MAX_MB, |mb| mb.min(LINK_MAX_MB))), ..*output }
}

/// Whether a copy of a clip `duration` seconds long can fit in `mb` MB at
/// all: even the smallest picture takes about [`LEAST_VIDEO_KBPS`], and the
/// mix its own.
fn fits_in(mb: u32, duration: f64) -> bool {
    let kbps = f64::from(mb) * 1024.0 * 1024.0 * 8.0 / 1000.0 / duration.max(0.1) * 0.9;
    kbps - f64::from(media::RENDER_AUDIO_KBPS) >= LEAST_VIDEO_KBPS
}

/// About the least a picture worth watching takes, in kbps: 360p at 30 fps,
/// soft. Under 100 MB, that's a clip of about 25 minutes.
const LEAST_VIDEO_KBPS: f64 = 250.0;

/// Upload `clip` as a new shared clip, or as the new version of shared clip
/// `replacing`, made as `output` asks ([`made_to_upload`]).
fn upload(clip: &Path, named: &Named, output: &media::Output, progress: &Progress, replacing: Option<&str>) -> Result<Shared, String> {
    let work = Scratch::new()?;
    let video = work.0.join("video.mp4");
    let source = made_to_upload(clip, &work.0, output, progress)?;
    progress.readying.store(true, Ordering::Relaxed);
    web_copy(&source, &video)?;
    progress.cancelled()?;
    let info = media::probe(&video).map_err(|e| e.to_string())?;
    let thumb = work.0.join("thumb.jpg");
    let thumb = picture(&video, info.duration, &thumb).then_some(thumb);
    let size = std::fs::metadata(&video).map_err(|e| e.to_string())?.len();
    let thumb_bytes = thumb.as_ref().and_then(|t| std::fs::read(t).ok());

    let server = Server::new();
    let start = match replacing {
        None => format!("{}/v1/clips", api()),
        Some(id) => format!("{}/v1/clips/{id}/replace", api()),
    };
    let upload: Upload = server.authed(|s, token| {
        s.agent.post(&start).header("Authorization", format!("Bearer {token}")).send_json(serde_json::json!({
            "size": size,
            "duration_ms": (info.duration * 1000.0).round() as i64,
            "width": info.width,
            "height": info.height,
            "fps": info.fps,
            "title": named.title,
            "game": named.game,
            "thumb_size": thumb_bytes.as_ref().map(Vec::len),
        }))
    })?;
    progress.total.store(size + thumb_bytes.as_ref().map_or(0, |b| b.len() as u64), Ordering::Relaxed);

    let uploaded = (|| {
        let etags = put_parts(&video, size, &upload, progress)?;
        progress.cancelled()?;
        if let (Some(link), Some(bytes)) = (&upload.thumb, &thumb_bytes) {
            // The preview picture is nice to have, not worth failing over
            // (what it counted is taken back if it doesn't go).
            let lane = crate::upload_watch::Lane::new();
            let _ = put(&lane, link, bytes, Some("image/jpeg"), progress);
        }
        let complete = match replacing {
            None => format!("{}/v1/clips/{}/complete", api(), upload.id),
            Some(id) => format!("{}/v1/clips/{id}/replace/complete", api()),
        };
        server.authed::<Shared>(|s, token| s.agent.post(&complete).header("Authorization", format!("Bearer {token}")).send_json(serde_json::json!({ "etags": etags })))
    })();
    // Don't leave half a clip counting against what can be shared. (Half a
    // new version is dropped by the server, or by the next one.)
    if uploaded.is_err() && replacing.is_none() {
        let _ = delete(&server, &upload.id);
    }
    uploaded
}

/// Delete the clip from the server: the link stops working.
pub fn unshare(link: &Link) -> Result<(), String> {
    delete(&Server::new(), &link.id)
}

/// What the server says of a shared clip.
#[derive(Debug, PartialEq)]
pub enum Status {
    /// Still shared: how many times its page was opened.
    Shared { views: i64 },
    /// Gone: the server takes down clips nobody has watched in a long while,
    /// and ones that break its rules.
    Gone,
}

/// Ask the server about the clip. `None` when it can't be asked.
pub fn status(link: &Link) -> Option<Status> {
    let server = Server::new();
    let token = server.saved_token()?;
    let mut res = server.agent.get(format!("{}/v1/clips", api())).header("Authorization", format!("Bearer {token}")).call().ok()?;
    if !res.status().is_success() {
        return None;
    }
    #[derive(Deserialize)]
    struct Listed {
        id: String,
        state: String,
        views: i64,
    }
    let clips: Vec<Listed> = res.body_mut().read_json().ok()?;
    Some(clips.iter().find(|c| c.id == link.id && c.state == "ready").map_or(Status::Gone, |c| Status::Shared { views: c.views }))
}

fn delete(server: &Server, id: &str) -> Result<(), String> {
    let Some(token) = server.saved_token() else {
        return Err("it was shared from another install of HasteClip".into());
    };
    let res = server.agent.delete(format!("{}/v1/clips/{id}", api())).header("Authorization", format!("Bearer {token}")).call();
    match res {
        Ok(r) if r.status().is_success() || r.status() == 404 => Ok(()),
        Ok(mut r) => Err(error_of(&mut r)),
        Err(e) => Err(offline(e)),
    }
}

/// The clip as a browser plays it: an MP4 with its video and mix only, the
/// moov box at the front (it starts playing before it's all downloaded),
/// and no metadata (the library's own tags stay home).
fn web_copy(clip: &Path, out: &Path) -> Result<(), String> {
    let probe = media::ffprobe()
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0"])
        .arg(clip)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let streams: Vec<(String, String)> = String::from_utf8_lossy(&probe.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(',').map(str::trim);
            let (a, b) = (f.next()?, f.next()?);
            // Whichever order ffprobe prints them in.
            Some(if a == "video" || a == "audio" { (a.to_owned(), b.to_owned()) } else { (b.to_owned(), a.to_owned()) })
        })
        .collect();
    let video = streams.iter().find(|(t, _)| t == "video").map(|(_, c)| c.as_str());
    let audio = streams.iter().find(|(t, _)| t == "audio").map(|(_, c)| c.as_str());
    if video != Some("h264") {
        return Err("its video isn't H.264, which browsers need. Save a copy with Edit → Save smaller and share that".into());
    }
    if audio.is_some_and(|a| a != "aac") {
        return Err("its sound isn't AAC, which browsers need. Save a copy with Edit → Save smaller and share that".into());
    }
    let done = media::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(clip)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-dn", "-sn", "-movflags", "+faststart", "-f", "mp4"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if done.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&done.stderr);
    Err(why.lines().last().unwrap_or("ffmpeg failed").to_owned())
}

/// What `clip` goes up as, made in `work` as `output` asks (within
/// [`LINK_MAX_MB`]): the clip itself when nothing changes its picture and
/// it fits the size, else a copy made as Save smaller makes one, with the
/// mix. One too long to fit at all isn't made.
fn made_to_upload(clip: &Path, work: &Path, output: &media::Output, progress: &Progress) -> Result<PathBuf, String> {
    let output = capped(output);
    let mb = output.max_mb.unwrap_or(LINK_MAX_MB);
    let info = media::probe(clip).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(clip).map_err(|e| e.to_string())?.len();
    let mut edit = media::Edit::new(&info);
    edit.output = media::Output { mix_only: true, ..output };
    let fits = |bytes: u64| bytes <= u64::from(mb) << 20;
    if !reshapes(&info, &output) && fits(size) {
        return Ok(clip.to_path_buf());
    }
    if !fits_in(mb, info.duration) {
        return Err(too_long(mb));
    }
    progress.making.store(true, Ordering::Relaxed);
    let copy = work.join("copy.mp4");
    loop {
        progress.made.store(0, Ordering::Relaxed);
        media::render_with_progress(clip, &info, &edit, &copy, None, |f| progress.made.store(f.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed))
            .map_err(|e| format!("couldn't make the copy to upload: {e}"))?;
        progress.cancelled()?;
        let size = std::fs::metadata(&copy).map_err(|e| e.to_string())?.len();
        if fits(size) {
            return Ok(copy);
        }
        // The encoder went over: a picture too busy for the bitrate. macOS's
        // won't go under what it spends on a frame of that size however low
        // it's asked to (1080p noise asked for 5 Mbit/s came out at 39; at
        // 720p, 3.5), so asking again for less does nothing: a smaller
        // picture, until it fits. Not even the smallest does: it can't be
        // shared (the server takes nothing bigger).
        let (_, h) = edit.output.size(&info);
        let Some(next) = SMALLER.into_iter().find(|&s| s < h) else { return Err(too_long(mb)) };
        edit.output.height = Some(next);
    }
}

/// Why a clip can't go up: it doesn't fit in `mb` MB.
fn too_long(mb: u32) -> String {
    format!("it doesn't fit in {mb} MB, even small. Trim it in the editor and share the part you want")
}

/// Whether `output` changes the picture of a clip that's `info`, a size to
/// fit under aside (that only matters to a clip bigger than it).
pub(crate) fn reshapes(info: &media::ClipInfo, output: &media::Output) -> bool {
    media::Output { max_mb: None, ..*output }.changes_video(info)
}

/// The picture heights a copy steps down through when it comes out too big.
const SMALLER: [u32; 5] = [1080, 720, 540, 480, 360];

/// The picture a link's preview shows: a frame a second in (or from the
/// middle of a shorter clip), at most 1280 wide. `false` if there isn't one.
fn picture(video: &Path, duration: f64, out: &Path) -> bool {
    let at = (duration / 2.0).min(1.0);
    media::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &format!("{at:.3}"), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale='min(1280,iw)':-2", "-q:v", "3"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        && std::fs::metadata(out).is_ok_and(|m| m.len() > 0 && m.len() < 5 << 20)
}

/// What the server answers a new share with: where to upload each part.
#[derive(Deserialize)]
struct Upload {
    id: String,
    part_size: u64,
    parts: Vec<String>,
    thumb: Option<String>,
}

#[derive(Deserialize)]
struct Shared {
    id: String,
    url: String,
}

/// Parts going up at once: enough to fill a connection that's far from the
/// server, few enough to be gentle on a slow one.
const PARALLEL: usize = 3;

/// `PUT` bytes to a signed link (`content_type` when it was signed for
/// one); the ETag. The bar counts what the storage confirms as it goes. A
/// try that stalls (see `upload_watch`) or loses its connection is made
/// again, with twice the patience each time (one that was only slow gets
/// waited for), as long as each gets further than the last: a line that's
/// gone for good doesn't keep it going. What a try counted is taken back
/// when it doesn't go.
fn put(lane: &crate::upload_watch::Lane, link: &str, bytes: &[u8], content_type: Option<&str>, progress: &Progress) -> Result<String, String> {
    let len = bytes.len() as u64;
    let (mut patience, mut best, mut other_errors) = (1u32, 0u64, 0);
    loop {
        progress.cancelled()?;
        // Counted up to the part's size (headers and TLS make the
        // confirmed bytes a little more).
        let counted = AtomicU64::new(0);
        let (res, stalled) = lane.watch(
            patience,
            |n| {
                let now = n.min(len);
                let before = counted.swap(now, Ordering::Relaxed);
                progress.sent.fetch_add(now.saturating_sub(before), Ordering::Relaxed);
            },
            |agent| {
                let mut req = agent.put(link);
                if let Some(t) = content_type {
                    req = req.header("Content-Type", t);
                }
                req.send(bytes)
            },
        );
        let got = counted.load(Ordering::Relaxed);
        let failed = |progress: &Progress| {
            progress.sent.fetch_sub(got, Ordering::Relaxed);
        };
        match res {
            Ok(r) if r.status().is_success() => {
                // All of it, whatever was confirmed along the way.
                progress.sent.fetch_add(len - got, Ordering::Relaxed);
                let etag = r.headers().get("etag").and_then(|v| v.to_str().ok());
                return etag.map(str::to_owned).ok_or_else(|| "the storage didn't confirm a part".into());
            }
            // Signed for exactly this; asking again won't change it.
            Ok(r) if r.status() == 403 => {
                failed(progress);
                return Err(format!("the storage refused the upload ({})", r.status()));
            }
            Ok(r) => {
                failed(progress);
                other_errors += 1;
                if other_errors > 1 {
                    return Err(format!("the storage answered {}", r.status()));
                }
            }
            Err(e) => {
                failed(progress);
                // Gone again without getting further: the line is down.
                if got <= best && patience > 1 {
                    return Err(if stalled { "the upload stopped getting through. Check your connection and try again".into() } else { offline(e) });
                }
                best = best.max(got);
                patience *= 2;
            }
        }
    }
}

/// Upload the video's parts, a few at a time; their ETags, in order.
fn put_parts(video: &Path, size: u64, upload: &Upload, progress: &Progress) -> Result<Vec<String>, String> {
    let n = upload.parts.len();
    let next = AtomicUsize::new(0);
    let etags: Mutex<Vec<Option<String>>> = Mutex::new(vec![None; n]);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..PARALLEL.min(n) {
            s.spawn(|| {
                let mut file = match std::fs::File::open(video) {
                    Ok(f) => f,
                    Err(e) => return *failed.lock().unwrap() = Some(e.to_string()),
                };
                // Its own connections, so what they confirm is its part's.
                let lane = crate::upload_watch::Lane::new();
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || failed.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(e) = progress.cancelled() {
                        return *failed.lock().unwrap() = Some(e);
                    }
                    let start = i as u64 * upload.part_size;
                    let len = upload.part_size.min(size - start);
                    buf.resize(len as usize, 0);
                    if let Err(e) = file.seek(SeekFrom::Start(start)).and_then(|_| file.read_exact(&mut buf)) {
                        return *failed.lock().unwrap() = Some(e.to_string());
                    }
                    match put(&lane, &upload.parts[i], &buf, None, progress) {
                        Ok(etag) => etags.lock().unwrap()[i] = Some(etag),
                        Err(e) => return *failed.lock().unwrap() = Some(e),
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    etags.into_inner().unwrap().into_iter().map(|e| e.ok_or_else(|| "a part of the clip didn't upload".to_owned())).collect()
}

struct Server {
    /// For the API: quick answers.
    agent: ureq::Agent,
}

impl Server {
    fn new() -> Self {
        Self { agent: ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).http_status_as_error(false).build().into() }
    }

    /// This app's token, signing up for one the first time.
    fn token(&self) -> Result<String, String> {
        if let Some(t) = self.saved_token() {
            return Ok(t);
        }
        let mut res = self.agent.post(format!("{}/v1/uploaders", api())).send_empty().map_err(offline)?;
        if !res.status().is_success() {
            return Err(error_of(&mut res));
        }
        #[derive(Deserialize)]
        struct Token {
            token: String,
        }
        let token = res.body_mut().read_json::<Token>().map_err(|e| e.to_string())?.token;
        let path = token_path().ok_or("no settings folder")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, serde_json::json!({ "token": token }).to_string()).map_err(|e| e.to_string())?;
        Ok(token)
    }

    fn saved_token(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct Saved {
            token: String,
        }
        let saved: Saved = serde_json::from_slice(&std::fs::read(token_path()?).ok()?).ok()?;
        Some(saved.token)
    }

    /// A request with the token, read as `T`.
    fn authed<T: serde::de::DeserializeOwned>(
        &self,
        send: impl Fn(&Self, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<T, String> {
        self.authed_raw(send)?.body_mut().read_json().map_err(|e| e.to_string())
    }

    /// A request with the token, answered with success. A token the server
    /// doesn't know (it forgot it) is replaced by a new one, once. A clip it
    /// doesn't have is [`NOT_THERE`].
    fn authed_raw(&self, send: impl Fn(&Self, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<ureq::http::Response<ureq::Body>, String> {
        for retry in [false, true] {
            let token = self.token()?;
            let mut res = send(self, &token).map_err(offline)?;
            if res.status() == 401 && !retry {
                if let Some(p) = token_path() {
                    let _ = std::fs::remove_file(p);
                }
                continue;
            }
            if res.status() == 404 {
                return Err(NOT_THERE.into());
            }
            if !res.status().is_success() {
                return Err(error_of(&mut res));
            }
            return Ok(res);
        }
        Err("the server didn't take this app's token".into())
    }
}

/// Where the token is kept. Another server (`HASTECLIP_API`) gets a token
/// file of its own, so trying one out never touches the real token.
fn token_path() -> Option<PathBuf> {
    let name = if std::env::var_os("HASTECLIP_API").is_some() { "share-dev.json" } else { "share.json" };
    dirs::config_dir().map(|d| d.join("hasteclip").join(name))
}

/// The server's own words for what went wrong (`{"error": "…"}`).
fn error_of(res: &mut ureq::http::Response<ureq::Body>) -> String {
    let status = res.status();
    #[derive(Deserialize)]
    struct Error {
        error: String,
    }
    match res.body_mut().read_json::<Error>() {
        Ok(e) => e.error.trim_end_matches('.').to_owned(),
        Err(_) => format!("the server answered {status}"),
    }
}

/// The server can't be reached (no connection, or it's down).
const OFFLINE: &str = "HasteClip's server can't be reached. Check your connection";
/// The server doesn't have the clip: unwatched for 90 days, or taken down.
const NOT_THERE: &str = "it isn't on HasteClip's server anymore";

fn offline(e: ureq::Error) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(_) => OFFLINE.into(),
        e => e.to_string(),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A link said to be gone ([`NOT_THERE`]) is only taken to be once the
/// list of what's shared says so too: a server that doesn't know a request
/// (an older one) answers it the same way, and the link mustn't be
/// forgotten over that.
fn confirm_gone<T>(link: &Link, result: Result<T, String>) -> Result<T, String> {
    match result {
        Err(e) if e == NOT_THERE && status(link) != Some(Status::Gone) => Err("the server couldn't take it".into()),
        r => r,
    }
}

/// What a clip's page (and its link in a chat) is called: the clip's own
/// name; or, for one still named after when it was saved ("23:28:15", which
/// says nothing to whoever opens it), what happened in it ("3 kills on
/// Mirage"), else its game ("osu! clip", "osu! recording").
fn page_title(clip: &crate::clips::Clip) -> String {
    let stem = clip.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (kind, rest) = match stem.strip_prefix("clip_") {
        Some(rest) => ("clip", Some(rest)),
        None => ("recording", stem.strip_prefix("recording_")),
    };
    let saved = rest.filter(|r| r.get(..19).is_some_and(|ts| chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d_%H-%M-%S").is_ok()));
    let Some(rest) = saved else { return clip.title() };
    match rest[19..].trim().strip_prefix("- ") {
        Some(happened) if !happened.trim().is_empty() => happened.trim().to_owned(),
        _ => match clip.folder.as_deref().filter(|f| *f != crate::clips::DESKTOP) {
            Some(game) => format!("{game} {kind}"),
            None => format!("{}{}", kind[..1].to_uppercase(), &kind[1..]),
        },
    }
}

/// How long ago `at` ([`now`]'s seconds) was.
fn now_secs_since(at: u64) -> Duration {
    Duration::from_secs(now().saturating_sub(at))
}

/// A folder for the web copy and its picture, gone when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("hasteclip-share-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a link job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Doing {
    /// Uploading the clip for a link.
    Sharing,
    /// Uploading a new version of it under its link.
    Updating,
    /// Renaming its page.
    Renaming,
    /// Stopping its link: Stop sharing, or the clip was deleted.
    Stopping,
}

/// Something running for a clip's link: one at a time per clip.
pub struct Job {
    clip: PathBuf,
    doing: Doing,
    /// The link's id, when it has one.
    id: Option<String>,
    /// Started by itself, keeping a link up to date (not by a click): it
    /// says nothing if it fails, and tries again later.
    quiet: bool,
    progress: Arc<Progress>,
    done: Receiver<Result<Done, String>>,
}

enum Done {
    Shared(Link),
    /// Brought up to date: the link is this now.
    Updated(Link),
    Unshared,
}

/// What a clip's card shows of its link.
pub(crate) enum LinkState {
    /// Uploading for a link, this far (0..=1).
    Uploading(f32),
    /// Uploading a new version under its link, this far.
    Updating(f32),
    /// Its link shows an older version or name of it, until it can be
    /// brought up to date.
    Behind,
    Shared,
}

/// How often shared clips are looked at for changes.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// How long to wait before trying again to bring a link up to date, after
/// `tries` that didn't work: half a minute, doubling, at most half an hour.
fn backoff(tries: u32) -> Duration {
    Duration::from_secs(30 << tries.min(6)).min(Duration::from_secs(30 * 60))
}

/// The server being asked whether a clip's link still works.
pub struct Check {
    clip: PathBuf,
    done: Receiver<Option<Status>>,
}

/// The Share with a link dialog, for one clip.
pub struct Dialog {
    clip: PathBuf,
    /// Why the last try didn't work.
    error: Option<String>,
    /// Its old link had stopped working.
    note: Option<String>,
    /// How many times its page was opened, once the server says.
    views: Option<i64>,
    /// When its link was copied, to say so on the button for a moment.
    copied: Option<Instant>,
    /// How it goes up, before it's shared. `None` when the clip can't be
    /// read: it goes up as [`default_output`] makes it.
    form: Option<UploadForm>,
}

/// How a clip goes up, as the dialog shows it: what the clip is, and an
/// edit of all of it whose output is how it's made ([`default_output`] to
/// start with), changed with the editor's export settings.
struct UploadForm {
    info: media::ClipInfo,
    edit: media::Edit,
    /// The clip's file size.
    bytes: u64,
    /// The settings are shown (else just what goes up).
    open: bool,
}

impl UploadForm {
    fn of(clip: &Path) -> Option<Self> {
        let info = media::probe(clip).ok()?;
        let mut edit = media::Edit::new(&info);
        edit.output = default_output();
        let bytes = std::fs::metadata(clip).ok()?.len();
        Some(Self { info, edit, bytes, open: false })
    }

    /// The most it can take, in MB: the size chosen, within [`LINK_MAX_MB`].
    fn most(&self) -> u32 {
        capped(&self.edit.output).max_mb.unwrap_or(LINK_MAX_MB)
    }

    /// It goes up as it is: nothing changes its picture, and it fits.
    fn as_it_is(&self) -> bool {
        !reshapes(&self.info, &self.edit.output) && self.bytes <= u64::from(self.most()) << 20
    }

    /// Even a copy can't fit: it's too long for the size.
    fn too_long(&self) -> bool {
        !self.as_it_is() && !fits_in(self.most(), self.info.duration)
    }

    /// What goes up, in short ("As it is, 42 MB"; "A copy under 100 MB";
    /// "Too long to fit in 100 MB"), and in a line under it its picture
    /// ("1920×1080 · 60 fps · 15 Mbps"), or what to do.
    fn what(&self) -> (String, String) {
        use crate::export_ui::{human_bytes, mbps, picture_words};
        let info = &self.info;
        let mb = self.most();
        if self.as_it_is() {
            let kbps = info.video_kbps.map_or(String::new(), |k| format!(" · {}", mbps(k)));
            return (format!("As it is, {}", human_bytes(self.bytes)), format!("{}×{} · {:.0} fps{kbps}", info.width, info.height, info.fps));
        }
        if self.too_long() {
            return (format!("Too long to fit in {mb} MB"), "Trim it in the editor, then share the part you want.".to_owned());
        }
        let edit = media::Edit { output: capped(&self.edit.output), ..self.edit.clone() };
        let what = if reshapes(info, &edit.output) {
            let est = edit.output.estimate_bytes(info, &edit).unwrap_or(self.bytes).min(u64::from(mb) << 20);
            format!("A copy, about {}", human_bytes(est))
        } else {
            format!("A copy under {mb} MB")
        };
        (what, picture_words(info, &edit))
    }
}

/// What was clicked in the dialog.
enum Pick {
    Upload,
    CancelUpload,
    Copy,
    Stop,
    /// Bring its link up to date now.
    TryNow,
    Close,
}

const GONE: &str = "Its link had stopped working: nobody watched it for 90 days, or it was taken down";

impl App {
    /// Open the Share with a link dialog for `clip`.
    pub(crate) fn open_link_dialog(&mut self, clip: PathBuf) {
        self.check_link(&clip);
        // Not shared yet: how it would go up (reading its file is quick).
        let form = if self.links.get(&clip).is_none() { UploadForm::of(&clip) } else { None };
        self.link_dialog = Some(Dialog { clip, error: None, note: None, views: None, copied: None, form });
    }

    /// Copy the link of a clip that's shared.
    pub(crate) fn copy_link(&mut self, clip: &Path) {
        let Some(link) = self.links.get(clip) else { return };
        self.ctx().copy_text(link.url.clone());
        self.toast_icon(icon::LINK, format!("Link copied: paste it anywhere ({})", crate::hotkey_label_cmd("V")));
        // Said if it turns out to have stopped working.
        self.check_link(clip);
    }

    /// What the clip's card shows of its link, if it has one (or one's on
    /// its way).
    pub(crate) fn link_state(&self, clip: &Path) -> Option<LinkState> {
        match self.link_jobs.iter().find(|j| j.clip == clip) {
            Some(j) if j.doing == Doing::Sharing => return Some(LinkState::Uploading(j.progress.fraction())),
            Some(j) if j.doing == Doing::Updating => return Some(LinkState::Updating(j.progress.fraction())),
            _ => {}
        }
        self.links.get(clip)?;
        Some(if self.links.behind(clip) { LinkState::Behind } else { LinkState::Shared })
    }

    /// A clip was renamed or moved: its link goes with it, and so does
    /// anything on its way.
    pub(crate) fn follow_links(&mut self, from: &Path, to: &Path) {
        self.links.follow(from, to);
        let jobs = self.link_jobs.iter_mut().map(|j| &mut j.clip);
        let checks = self.link_checks.iter_mut().map(|c| &mut c.clip);
        for clip in jobs.chain(checks).chain(self.link_dialog.iter_mut().map(|d| &mut d.clip)) {
            if clip == from {
                *clip = to.to_path_buf();
            }
        }
    }

    /// Ask the server whether the clip's link still works, and how many
    /// times it was watched.
    fn check_link(&mut self, clip: &Path) {
        let Some(link) = self.links.get(clip).cloned() else { return };
        if self.link_checks.iter().any(|c| c.clip == clip) {
            return;
        }
        let (tx, done) = std::sync::mpsc::channel();
        let ctx = self.ctx();
        std::thread::spawn(move || {
            let _ = tx.send(status(&link));
            ctx.request_repaint();
        });
        self.link_checks.push(Check { clip: clip.to_path_buf(), done });
    }

    /// What a clip's page should call it: its name and game, as now.
    fn named(&self, clip: &Path) -> Named {
        let found = self.clips.iter().find(|c| c.path == clip);
        Named { title: found.map(page_title).unwrap_or_default(), game: found.and_then(|c| c.folder.clone()) }
    }

    /// Upload the clip for a link, made as `output` asks.
    fn start_share(&mut self, clip: PathBuf, output: media::Output) {
        if self.link_jobs.iter().any(|j| j.clip == clip) {
            return;
        }
        let named = self.named(&clip);
        let file = clip.clone();
        self.start_link_job(clip, Doing::Sharing, None, false, move |progress| share(&file, &named, &output, progress).map(Done::Shared));
    }

    /// Delete the clip from the server; its link stops working.
    fn stop_sharing(&mut self, clip: PathBuf) {
        let Some(link) = self.links.get(&clip).cloned() else { return };
        // Bringing it up to date is moot now.
        if let Some(i) = self.link_jobs.iter().position(|j| j.clip == clip && j.quiet) {
            self.link_jobs.remove(i).progress.cancel.store(true, Ordering::Relaxed);
        }
        if self.link_jobs.iter().any(|j| j.clip == clip) {
            return;
        }
        let id = Some(link.id.clone());
        self.start_link_job(clip, Doing::Stopping, id, false, move |_| unshare(&link).map(|()| Done::Unshared));
    }

    fn start_link_job(&mut self, clip: PathBuf, doing: Doing, id: Option<String>, quiet: bool, work: impl FnOnce(&Progress) -> Result<Done, String> + Send + 'static) {
        let progress = Arc::new(Progress::default());
        let (tx, done) = std::sync::mpsc::channel();
        let (p, ctx) = (progress.clone(), self.ctx());
        std::thread::spawn(move || {
            let _ = tx.send(work(&p));
            ctx.request_repaint();
        });
        self.link_jobs.push(Job { clip, doing, id, quiet, progress, done });
    }

    /// Every couple of seconds: links that don't show their clip as it is
    /// now are brought up to date (a new name, or a new version uploaded),
    /// and the links of clips deleted a while ago stop. What can't be done
    /// now (no connection) is tried again later.
    fn sync_links(&mut self) {
        if self.links.looked.is_some_and(|t| t.elapsed() < LOOK_EVERY) {
            return;
        }
        self.links.looked = Some(Instant::now());
        let now = Instant::now();
        let busy = |app: &App, clip: &Path, id: &str| app.link_jobs.iter().any(|j| j.clip == clip) || app.links.retry.get(id).is_some_and(|(at, _)| *at > now);
        let due: Vec<Deleted> = self.links.deleted.iter().filter(|d| now_secs_since(d.at) >= UNDO_FOR).cloned().collect();
        for d in due {
            let clip = self.links.lib.join(&d.key);
            if busy(self, &clip, &d.link.id) {
                continue;
            }
            let id = Some(d.link.id.clone());
            self.start_link_job(clip, Doing::Stopping, id, true, move |_| unshare(&d.link).map(|()| Done::Unshared));
        }
        let mut behind = std::collections::HashSet::new();
        for (clip, link) in self.links.all() {
            // Gone from here some other way (renamed outside HasteClip, say):
            // its link stays as it is. Being saved: once it's done.
            if !self.clips.iter().any(|c| c.path == clip) || self.renders.iter().any(|j| !j.as_new && j.source == clip) {
                continue;
            }
            let named = self.named(&clip);
            let version = Version::of(&clip);
            // A link kept before they were known: taken to be up to date.
            let (Some(had_named), Some(had_version)) = (&link.named, link.version) else {
                self.links.set(&clip, Link { named: Some(named), version, ..link });
                continue;
            };
            let new_file = Some(had_version) != version;
            if !new_file && *had_named == named {
                continue;
            }
            if let Some(key) = self.links.key(&clip) {
                behind.insert(key);
            }
            if busy(self, &clip, &link.id) {
                continue;
            }
            let id = Some(link.id.clone());
            if new_file {
                let file = clip.clone();
                self.start_link_job(clip, Doing::Updating, id, true, move |p| confirm_gone(&link, replace(&file, &link, &named, p)).map(Done::Updated));
            } else {
                self.start_link_job(clip, Doing::Renaming, id, true, move |_| confirm_gone(&link, rename(&link, &named)).map(|()| Done::Updated(Link { named: Some(named), ..link })));
            }
        }
        self.links.behind = behind;
    }

    /// Each frame: how the uploads, unshares and checks went, once done.
    pub(crate) fn pump_links(&mut self) {
        self.sync_links();
        if !self.link_jobs.is_empty() {
            // Their progress, on the clips and in the dialog.
            self.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let mut i = 0;
        while i < self.link_jobs.len() {
            let result = match self.link_jobs[i].done.try_recv() {
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("it stopped unexpectedly".to_owned()),
                Ok(result) => result,
            };
            let job = self.link_jobs.remove(i);
            self.link_job_done(job, result);
        }
        let mut i = 0;
        while i < self.link_checks.len() {
            let status = match self.link_checks[i].done.try_recv() {
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => None,
                Ok(status) => status,
            };
            let check = self.link_checks.remove(i);
            self.link_checked(&check.clip, status);
        }
    }

    fn link_job_done(&mut self, job: Job, result: Result<Done, String>) {
        let shown = self.link_dialog.as_ref().is_some_and(|d| d.clip == job.clip);
        let title = self.clips.iter().find(|c| c.path == job.clip).map(|c| c.title()).unwrap_or_default();
        // A deleted clip's link, stopped (or already gone): it's forgotten.
        if job.quiet && job.doing == Doing::Stopping {
            match result {
                Ok(_) => self.links.forget_deleted(job.id.as_deref().unwrap_or_default()),
                Err(_) => self.links.failed(job.id.as_deref().unwrap_or_default()),
            }
            return;
        }
        let ours = |links: &Links| links.get(&job.clip).is_some_and(|l| Some(&l.id) == job.id.as_ref());
        match result {
            // Kept, unless the link was stopped (or shared anew) meanwhile.
            Ok(Done::Updated(link)) => {
                if ours(&self.links) {
                    self.links.set(&job.clip, link);
                }
            }
            Err(e) if e == NOT_THERE && job.quiet => {
                if ours(&self.links) {
                    self.links.remove(&job.clip);
                    match self.link_dialog.as_mut().filter(|_| shown) {
                        Some(d) => d.note = Some(format!("{GONE}. Upload it again for a new link.")),
                        None => self.toast_error(format!("{title}: {}. Share it again for a new one", GONE.to_lowercase())),
                    }
                }
            }
            Err(_) if job.quiet => self.links.failed(job.id.as_deref().unwrap_or_default()),
            Ok(Done::Shared(link)) => {
                self.links.set(&job.clip, link);
                match self.link_dialog.as_mut().filter(|_| shown) {
                    Some(d) => d.views = Some(0),
                    // Ready while the dialog's closed: a button to copy it,
                    // never straight onto the clipboard (it may hold
                    // something else by now).
                    None => self.toast_action(icon::LINK, format!("{title} is shared"), "Copy link", crate::shell::ToastAction::CopyLink(job.clip)),
                }
            }
            Ok(Done::Unshared) => {
                self.links.remove(&job.clip);
                if shown {
                    self.link_dialog = None;
                }
                self.toast_icon(icon::LINK_BREAK, "Stopped sharing: the link doesn't work anymore");
            }
            // Called off: nothing to say.
            Err(_) if job.progress.cancel.load(Ordering::Relaxed) => {}
            Err(e) => {
                let sharing = job.doing == Doing::Sharing;
                match self.link_dialog.as_mut().filter(|_| shown) {
                    Some(d) => d.error = Some(if sharing { format!("Couldn't upload it: {e}") } else { format!("Couldn't stop sharing: {e}") }),
                    None if sharing => self.toast_error(format!("Couldn't share {title}: {e}")),
                    None => self.toast_error(format!("Couldn't stop sharing {title}: {e}")),
                }
            }
        }
    }

    fn link_checked(&mut self, clip: &Path, status: Option<Status>) {
        let dialog = self.link_dialog.as_mut().filter(|d| d.clip == clip);
        match status {
            Some(Status::Shared { views }) => {
                if let Some(d) = dialog {
                    d.views = Some(views);
                }
            }
            Some(Status::Gone) => {
                self.links.remove(clip);
                match dialog {
                    Some(d) => d.note = Some(format!("{GONE}. Upload it again for a new link.")),
                    None => self.toast_error(format!("{GONE}. Share it again for a new one")),
                }
            }
            // The server can't be asked: the link stays as it is.
            None => {}
        }
    }

    /// "Share with a link": what it does, and Upload. Then how far the upload
    /// is (it goes on with the dialog closed), then the link to copy, how
    /// many have watched it, and Stop sharing.
    pub(crate) fn link_dialog(&mut self, ctx: &egui::Context) {
        let Some(clip) = self.link_dialog.as_ref().map(|d| d.clip.clone()) else { return };
        // Its clip is gone (deleted, or another library): nothing to show.
        let Some(found) = self.clips.iter().find(|c| c.path == clip).cloned() else {
            self.link_dialog = None;
            return;
        };
        let tex = self.thumbs.get(ctx, &found).and_then(|t| t.texture.clone());
        let job = self.link_jobs.iter().find(|j| j.clip == clip);
        let sent = |j: &Job| (j.progress.sent.load(Ordering::Relaxed), j.progress.total.load(Ordering::Relaxed), j.progress.making(), j.progress.fraction(), j.progress.left());
        let uploading = job.filter(|j| j.doing == Doing::Sharing).map(sent);
        let updating = job.filter(|j| j.doing == Doing::Updating).map(sent);
        let stopping = job.is_some_and(|j| j.doing == Doing::Stopping);
        let link = self.links.get(&clip).cloned();
        let behind = updating.is_none() && self.links.behind(&clip);
        let Some(d) = &self.link_dialog else { return };
        let (error, note, views) = (d.error.clone(), d.note.clone(), d.views);
        let copied = d.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        if copied {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let under = format!("{} · {}", found.folder.as_deref().unwrap_or("Other"), found.human_size());
        // How it goes up, shown and changed while the dialog's drawn.
        let mut form = self.link_dialog.as_mut().and_then(|d| d.form.take());
        // A value being typed as the frame began: Enter and Esc are its
        // (applying it, putting it back), not the dialog's.
        let typing = ctx.memory(|m| m.focused().is_some());
        let mut pick = None;
        let modal = ui_kit::dialog(ctx, "share_link", 420.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, if link.is_some() { "Shared with a link" } else { "Share with a link" });
            crate::library::clip_row(ui, &found, tex.as_ref(), Some(&under));
            ui.add_space(12.0);
            if stopping {
                ui_kit::dialog_text(ui, "Stopping sharing…");
            } else if let Some(link) = &link {
                if link_row(ui, &link.url, copied) {
                    pick = Some(Pick::Copy);
                }
                ui.add_space(8.0);
                let watched = match views {
                    Some(0) => "Not watched yet. ".to_owned(),
                    Some(1) => "Watched once. ".to_owned(),
                    Some(n) => format!("Watched {n} times. "),
                    None => String::new(),
                };
                ui_kit::dialog_text(ui, &format!("{watched}Anyone with the link can watch it, in a browser or right in a chat. A link nobody watches for 90 days stops working."));
                // It follows the clip: a new version on its way, or waiting
                // to be.
                if let Some((sent, total, making, f, left)) = updating {
                    ui.add_space(10.0);
                    progress_bar(ui, f);
                    ui.add_space(8.0);
                    let how = match (making, total) {
                        (Some(made), _) => format!("Making the copy to upload… {}%", ((made * 100.0) as u32).min(99)),
                        (None, 0) => "Getting the new version ready…".to_owned(),
                        (None, _) => format!("Updating the link to this version… {}", going(sent, total, left)),
                    };
                    ui.label(theme::text(how, 13.5, W::Medium).color(p.text));
                    ui.add_space(4.0);
                    ui_kit::dialog_text(ui, "Until it's all there, the link plays the version from before.");
                } else if behind {
                    ui.add_space(8.0);
                    ui_kit::dialog_text(ui, "The link still shows it as it was before. It's brought up to date by itself as soon as HasteClip's server can be reached.");
                }
            } else if let Some((sent, total, making, f, left)) = uploading {
                progress_bar(ui, f);
                ui.add_space(8.0);
                // No total yet: the copy to upload is still being made, or
                // got ready to go up.
                let how = match (making, total) {
                    (Some(made), _) => format!("Making the copy to upload… {}%", ((made * 100.0) as u32).min(99)),
                    (None, 0) => "Getting it ready to upload…".to_owned(),
                    (None, _) => format!("Uploading… {}", going(sent, total, left)),
                };
                ui.label(theme::text(how, 13.5, W::Medium).color(p.text));
                ui.add_space(4.0);
                ui_kit::dialog_text(ui, "You can close this: it goes on uploading, and the clip shows how far it's come. You'll be told when its link is ready.");
            } else {
                if let Some(n) = &note {
                    ui_kit::dialog_text(ui, n);
                }
                ui_kit::dialog_text(ui, "Uploads the clip to HasteClip for a link that plays it in any browser, and right in chats. Anyone with the link can watch it, until you stop sharing.");
                if let Some(f) = &mut form {
                    upload_form(ui, f);
                }
            }
            if let Some(e) = &error {
                ui.add_space(4.0);
                ui.add(egui::Label::new(theme::text(e, 12.5, W::Regular).color(p.danger)).wrap());
            }
            // Too long to fit even as a copy: nothing to upload, until it's
            // trimmed (or a bigger size is chosen).
            let blocked = link.is_none() && uploading.is_none() && form.as_ref().is_some_and(UploadForm::too_long);
            ui_kit::dialog_buttons(ui, |ui| {
                if stopping {
                    if ui.add(ui_kit::button("Close", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                } else if link.is_some() {
                    if ui.add(ui_kit::button("Done", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                    if behind && ui.add(ui_kit::button("Try now", false).ghost()).clicked() {
                        pick = Some(Pick::TryNow);
                    }
                    if Btn::new(Kind::DangerText, "Stop sharing").tip("Deletes it from HasteClip: the link stops working for everyone").show(ui).clicked() {
                        pick = Some(Pick::Stop);
                    }
                } else if uploading.is_some() {
                    if ui.add(ui_kit::button("Close", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                    if ui.add(ui_kit::button("Cancel upload", false).ghost()).clicked() {
                        pick = Some(Pick::CancelUpload);
                    }
                } else {
                    let upload = Btn::primary(if error.is_some() { "Try again" } else { "Upload" }).with_icon(icon::CLOUD_ARROW_UP).enabled(!blocked);
                    let upload = if blocked { upload.tip(format!("It doesn't fit in {LINK_MAX_MB} MB: trim it in the editor first")) } else { upload };
                    if upload.show(ui).clicked() {
                        pick = Some(Pick::Upload);
                    }
                    if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
                        pick = Some(Pick::Close);
                    }
                }
            });
            // Enter does the main thing: upload, or copy the link.
            if !stopping && uploading.is_none() && !typing && !blocked && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                pick = Some(if link.is_some() { Pick::Copy } else { Pick::Upload });
            }
        });
        let output = form.as_ref().map_or_else(default_output, |f| f.edit.output);
        if let Some(d) = &mut self.link_dialog {
            d.form = form;
        }
        match pick {
            Some(Pick::Upload) => {
                if let Some(d) = &mut self.link_dialog {
                    d.error = None;
                    d.note = None;
                }
                self.start_share(clip, output);
            }
            Some(Pick::CancelUpload) => {
                // Let go of it now; it stops (and its half on the server goes)
                // at the next part.
                if let Some(i) = self.link_jobs.iter().position(|j| j.doing == Doing::Sharing && j.clip == clip) {
                    self.link_jobs.remove(i).progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            Some(Pick::TryNow) => self.links.try_now(&clip),
            Some(Pick::Copy) => {
                if let Some(link) = &link {
                    ctx.copy_text(link.url.clone());
                }
                if let Some(d) = &mut self.link_dialog {
                    d.copied = Some(Instant::now());
                }
            }
            Some(Pick::Stop) => {
                if let Some(d) = &mut self.link_dialog {
                    d.error = None;
                }
                self.stop_sharing(clip);
            }
            Some(Pick::Close) => self.link_dialog = None,
            None if modal.dismissed() && !typing => self.link_dialog = None,
            None => {}
        }
    }
}

/// Before it's shared: what goes up ("As it is, 42 MB"; "A copy under
/// 100 MB"), and on asking the settings it's made with, the editor's
/// export settings as far as an upload can change them: always a size, up
/// to [`LINK_MAX_MB`].
fn upload_form(ui: &mut egui::Ui, f: &mut UploadForm) {
    let p = theme::pal(ui);
    ui.add_space(10.0);
    let (what, under) = f.what();
    ui.horizontal(|ui| {
        ui.label(theme::text(what, 13.5, W::Medium).color(if f.too_long() { p.danger } else { p.text }));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, caret) = if f.open { ("Done", icon::CARET_UP) } else { ("Change", icon::CARET_DOWN) };
            if Btn::ghost(label).trail(caret).small().tip("The resolution, frame rate and size it goes up at").show(ui).clicked() {
                f.open = !f.open;
            }
        });
    });
    ui_kit::hint(ui, &under);
    if f.open {
        ui.add_space(4.0);
        let form = crate::export_ui::Form {
            id: "share",
            crop: None,
            more: false,
            size_hint: "The most the upload can take: up to 100 MB",
            cap_mb: Some(LINK_MAX_MB),
            size_note: Some((LINK_MAX_MB, "the most")),
        };
        crate::export_ui::rows(ui, &f.info, &mut f.edit, form);
        if f.edit.output != default_output() {
            ui.add_space(4.0);
            if Btn::ghost("Back to the usual").small().tip(format!("As it is, or a copy under {LINK_MAX_MB} MB when it's bigger")).show(ui).clicked() {
                f.edit.output = default_output();
            }
        }
    }
}

/// The link, and a Copy button beside it. True when Copy was clicked.
fn link_row(ui: &mut egui::Ui, url: &str, copied: bool) -> bool {
    let p = theme::pal(ui);
    const COPY_W: f32 = 96.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        egui::Frame::new().fill(p.control).corner_radius(ui_kit::RADIUS).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
            ui.set_width(ui.available_width() - COPY_W - 8.0);
            ui.add(egui::Label::new(theme::text(url, 13.5, W::Regular).color(p.text)).truncate());
        });
        Btn::primary(if copied { "Copied" } else { "Copy" }).with_icon(if copied { icon::CHECK } else { icon::COPY }).min_width(COPY_W).show(ui).clicked()
    })
    .inner
}

/// A bar filled `f` (0..=1) of the way.
/// "12% · 15 MB of 123 MB · about 2 minutes left": how far an upload has
/// come, and how long is left once that can be said.
fn going(sent: u64, total: u64, left: Option<Duration>) -> String {
    let pct = (sent * 100).checked_div(total).unwrap_or(0).min(99);
    let mut s = format!("{pct}% · {} of {}", crate::export_ui::human_bytes(sent), crate::export_ui::human_bytes(total));
    if let Some(left) = left {
        s += &format!(" · {}", time_left(left));
    }
    s
}

/// "about 2 minutes left", rounded the way it's read.
fn time_left(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..10 => "a few seconds left".to_owned(),
        10..60 => format!("about {} seconds left", secs.div_ceil(10) * 10),
        60..90 => "about a minute left".to_owned(),
        90..3600 => format!("about {} minutes left", (secs + 30) / 60),
        _ => {
            let h = (secs + 1800) / 3600;
            if h == 1 { "about an hour left".to_owned() } else { format!("about {h} hours left") }
        }
    }
}

fn progress_bar(ui: &mut egui::Ui, f: f32) {
    let p = theme::pal(ui);
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 3, p.control);
    ui.painter().rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.left() + r.width() * f.clamp(0.0, 1.0), r.bottom())), 3, p.text);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the storage's signed part link: takes each PUT's body
    /// at `rate` bytes a second and answers with an ETag. `stall` stops the
    /// first `stall` tries for good partway (connected, reading nothing);
    /// `refuse` answers 403. Returns its link and how many tries it saw.
    fn storage(rate: f64, stall: usize, refuse: bool) -> (String, Arc<AtomicUsize>) {
        use std::io::Write as _;
        let listener = crate::upload_watch::tests::small_buffer_listener();
        let url = format!("http://{}/part?signed", listener.local_addr().unwrap());
        let tries = Arc::new(AtomicUsize::new(0));
        let seen = tries.clone();
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(mut c) = c else { return };
                let n = seen.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && c.read(&mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&head).to_lowercase();
                    let len: usize = text.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    if refuse {
                        let _ = c.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let t0 = std::time::Instant::now();
                    let (mut got, mut buf) = (0usize, [0u8; 4096]);
                    while got < len {
                        if n < stall && got >= len / 3 {
                            std::thread::sleep(Duration::from_secs(60));
                            return;
                        }
                        match c.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(k) => got += k,
                        }
                        let ahead = got as f64 / rate - t0.elapsed().as_secs_f64();
                        if ahead > 0.0 {
                            std::thread::sleep(Duration::from_secs_f64(ahead));
                        }
                    }
                    let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nETag: \"part{n}\"\r\nContent-Length: 0\r\n\r\n").as_bytes());
                });
            }
        });
        (url, tries)
    }

    #[test]
    fn time_left_reads_as_said() {
        let t = |s| time_left(Duration::from_secs(s));
        assert_eq!(t(4), "a few seconds left");
        assert_eq!(t(23), "about 30 seconds left");
        assert_eq!(t(70), "about a minute left");
        assert_eq!(t(150), "about 3 minutes left");
        assert_eq!(t(3700), "about an hour left");
        assert_eq!(t(9000), "about 3 hours left");
        assert_eq!(going(15_000_000, 123_000_000, None), format!("12% · {} of {}", crate::export_ui::human_bytes(15_000_000), crate::export_ui::human_bytes(123_000_000)));
    }

    /// Over HTTPS to a real server: the lane's own connections carry TLS,
    /// and the bar follows what the server confirmed.
    #[test]
    #[ignore = "uploads 3 MB to httpbin.org: cargo test -p hasteclip over_https -- --ignored --nocapture"]
    fn a_part_goes_up_over_https() {
        let progress = Progress::default();
        progress.total.store(3_000_000, Ordering::Relaxed);
        let part = vec![1u8; 3_000_000];
        let watch = std::thread::scope(|s| {
            let up = s.spawn(|| put(&crate::upload_watch::Lane::new(), "https://httpbin.org/put", &part, None, &progress));
            let t0 = std::time::Instant::now();
            while !up.is_finished() {
                std::thread::sleep(Duration::from_millis(300));
                eprintln!("{:>5} ms: {} of 3 MB, {:?}", t0.elapsed().as_millis(), progress.sent.load(Ordering::Relaxed) / 1000, progress.left());
            }
            up.join().unwrap()
        });
        // httpbin answers with no ETag: the bytes went, the part "wasn't confirmed".
        assert_eq!(watch.unwrap_err(), "the storage didn't confirm a part");
        assert_eq!(progress.sent.load(Ordering::Relaxed), 3_000_000);
    }

    /// A slow part goes up, the bar ending at exactly its size.
    #[test]
    fn a_slow_part_goes_up() {
        let (url, tries) = storage(400_000.0, 0, false);
        let progress = Progress::default();
        let part = vec![1u8; 800_000];
        let etag = put(&crate::upload_watch::Lane::new(), &url, &part, None, &progress).unwrap();
        assert_eq!(etag, "\"part0\"");
        assert_eq!(progress.sent.load(Ordering::Relaxed), part.len() as u64);
        assert_eq!(tries.load(Ordering::Relaxed), 1);
    }

    /// A part that stalls is tried again, and goes up; what the stalled try
    /// counted isn't counted twice.
    #[test]
    fn a_stalled_part_is_tried_again() {
        let (url, tries) = storage(400_000.0, 1, false);
        let progress = Progress::default();
        let part = vec![1u8; 800_000];
        let etag = put(&crate::upload_watch::Lane::new(), &url, &part, None, &progress).unwrap();
        assert_eq!(etag, "\"part1\"");
        assert_eq!(progress.sent.load(Ordering::Relaxed), part.len() as u64);
        assert_eq!(tries.load(Ordering::Relaxed), 2);
    }

    /// A line that stays down: it gives up, says so, and takes back what it
    /// had counted.
    #[test]
    fn a_dead_line_gives_up() {
        let (url, _) = storage(400_000.0, usize::MAX, false);
        let progress = Progress::default();
        let t0 = std::time::Instant::now();
        let err = put(&crate::upload_watch::Lane::new(), &url, &vec![1u8; 800_000], None, &progress).unwrap_err();
        assert!(err.contains("stopped getting through"), "{err}");
        assert_eq!(progress.sent.load(Ordering::Relaxed), 0);
        assert!(t0.elapsed() < Duration::from_secs(30), "gave up after {:?}", t0.elapsed());
    }

    /// Refused: no second try.
    #[test]
    fn a_refused_part_stops_at_once() {
        let (url, tries) = storage(400_000.0, 0, true);
        let progress = Progress::default();
        let err = put(&crate::upload_watch::Lane::new(), &url, &vec![1u8; 100_000], None, &progress).unwrap_err();
        assert!(err.contains("refused"), "{err}");
        assert_eq!(tries.load(Ordering::Relaxed), 1);
        assert_eq!(progress.sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn links_follow_their_clip_and_are_saved() {
        let lib = std::env::temp_dir().join(format!("hc-links-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let link = Link { id: "aB3xY9k2Qp".into(), url: "https://clips.hasteclip.com/aB3xY9k2Qp".into(), shared: 1, named: None, version: None, output: None };
        let mut links = Links::load(&lib);
        links.set(&lib.join("Game").join("Ace.mp4"), link.clone());
        links.follow(&lib.join("Game").join("Ace.mp4"), &lib.join("Ace!.mp4"));
        let again = Links::load(&lib);
        assert_eq!(again.get(&lib.join("Ace!.mp4")), Some(&link));
        assert_eq!(again.get(&lib.join("Game").join("Ace.mp4")), None);
        let mut again = again;
        again.remove(&lib.join("Ace!.mp4"));
        assert_eq!(Links::load(&lib).get(&lib.join("Ace!.mp4")), None);
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// A deleted clip's link waits for Undo (kept across a restart), and
    /// Undo brings it back.
    #[test]
    fn deleted_links_wait_for_undo() {
        let lib = std::env::temp_dir().join(format!("hc-links-gone-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let link = Link { id: "aB3xY9k2Qp".into(), url: "u".into(), shared: 1, named: None, version: None, output: None };
        let clip = lib.join("Game").join("Ace.mp4");
        let mut links = Links::load(&lib);
        links.set(&clip, link.clone());
        links.deleted(&clip);
        assert_eq!(links.get(&clip), None);
        let mut again = Links::load(&lib);
        assert_eq!(again.deleted.len(), 1, "it waits, even after a restart");
        again.undeleted(&clip);
        assert_eq!(again.get(&clip), Some(&link));
        assert!(Links::load(&lib).deleted.is_empty());
        // Gone for good once stopped.
        again.deleted(&clip);
        again.forget_deleted(&link.id);
        let last = Links::load(&lib);
        assert!(last.deleted.is_empty() && last.get(&clip).is_none());
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// An edit is a new file; a rename isn't.
    #[test]
    fn versions_tell_edits_from_renames() {
        let dir = std::env::temp_dir().join(format!("hc-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Ace.mp4");
        std::fs::write(&clip, b"the recording").unwrap();
        let first = Version::of(&clip).unwrap();
        let renamed = dir.join("Ace clutch.mp4");
        std::fs::rename(&clip, &renamed).unwrap();
        assert_eq!(Version::of(&renamed), Some(first));
        // Saved as an edit is: written aside, renamed over it.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("staged.mp4"), b"the edit").unwrap();
        std::fs::rename(dir.join("staged.mp4"), &renamed).unwrap();
        assert_ne!(Version::of(&renamed), Some(first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page is called what says something to whoever opens the link.
    #[test]
    fn pages_are_called_what_happened_or_the_game() {
        let clip = |rel: &str| crate::clips::Clip {
            path: PathBuf::from("/lib").join(rel),
            name: String::new(),
            modified: SystemTime::UNIX_EPOCH,
            created: None,
            size_bytes: 0,
            id: None,
            original: None,
            folder: rel.split_once('/').map(|(f, _)| f.to_owned()),
            kept_bytes: 0,
        };
        assert_eq!(page_title(&clip("osu!/clip_2026-10-19_23-28-15.mp4")), "osu! clip");
        assert_eq!(page_title(&clip("Counter-Strike 2/clip_2026-10-19_23-28-15 - 3 kills on Mirage.mp4")), "3 kills on Mirage");
        assert_eq!(page_title(&clip("Desktop/clip_2026-10-19_23-28-15.mp4")), "Clip");
        assert_eq!(page_title(&clip("clip_2026-10-19_23-28-15 (edit).mp4")), "Clip");
        assert_eq!(page_title(&clip("osu!/Ace clutch.mp4")), "Ace clutch");
        assert_eq!(page_title(&clip("osu!/recording_2026-10-19_23-28-15.mp4")), "osu! recording");
        assert_eq!(page_title(&clip("recording_2026-10-19_23-28-15.mp4")), "Recording");
    }

    /// Noise asked to fit a bitrate macOS's encoder won't go down to at the
    /// picture it was planned at: the copy is made smaller until it fits,
    /// instead of going out over.
    #[test]
    #[ignore = "encodes 10 s of 1080p60 noise a few times: cargo test -p hasteclip busy_copies_fit -- --ignored"]
    #[cfg(target_os = "macos")]
    fn busy_copies_fit() {
        let dir = std::env::temp_dir().join(format!("hc-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Noise.mp4");
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "nullsrc=s=1920x1080:r=60,geq=random(1)*255:128:128", "-f", "lavfi", "-i", "sine=f=440"])
            .args(["-t", "10", "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "60M", "-c:a", "aac", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        // 10 s in 12 MB: under 10 Mbit/s, which the plan puts at 1080p, and
        // macOS's encoder makes 44 MB of there.
        let small = made_to_upload(&clip, &dir, &media::Output { max_mb: Some(12), ..default_output() }, &Progress::default()).unwrap();
        let size = std::fs::metadata(&small).unwrap().len();
        assert!(size <= 12 << 20, "{:.1} MB, over 12", size as f64 / 1e6);
        // And it uses what it may: aiming lower made 1.9 MB of it.
        assert!(size >= 6 << 20, "{:.1} MB of the 12 it could have", size as f64 / 1e6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clip too big to play in a chat goes as a copy that fits, still an
    /// H.264 MP4 a browser plays; a small one as it is.
    #[test]
    #[ignore = "encodes ~100 MB of video: cargo test -p hasteclip copies_fit_chats -- --ignored"]
    fn copies_fit_chats() {
        let dir = std::env::temp_dir().join(format!("hc-fit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Big.mp4");
        // Noise at a high bitrate: past the limit in a few seconds.
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "nullsrc=s=1920x1080:r=60,geq=random(1)*255:128:128", "-f", "lavfi", "-i", "sine=f=440"])
            .args(["-t", "5", "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "200M", "-maxrate", "200M", "-bufsize", "200M", "-c:a", "aac", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let big = std::fs::metadata(&clip).unwrap().len();
        assert!(big > u64::from(LINK_MAX_MB) << 20, "the test clip is {big} bytes");
        let progress = Progress::default();
        let small = made_to_upload(&clip, &dir, &default_output(), &progress).unwrap();
        let size = std::fs::metadata(&small).unwrap().len();
        assert!(size <= u64::from(LINK_MAX_MB) << 20, "{size} bytes");
        let probe = media::probe(&small).unwrap();
        assert!(probe.width > 0 && probe.duration > 4.5, "{probe:?}");
        assert!(progress.made.load(Ordering::Relaxed) > 0);
        let out = dir.join("web.mp4");
        web_copy(&small, &out).unwrap();
        // A small one stays as it is.
        assert_eq!(made_to_upload(&out, &dir, &default_output(), &Progress::default()).unwrap(), out);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clip that fits goes up as it is, whatever size it's to fit under;
    /// one asked to be smaller goes as a copy.
    #[test]
    fn a_clip_goes_as_it_is_unless_its_picture_changes() {
        let dir = std::env::temp_dir().join(format!("hc-asis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Small.mp4");
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x240:r=30:d=2", "-f", "lavfi", "-i", "sine=f=440:d=2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&clip)
            .status()
            .unwrap();
        if !made.success() {
            // An ffmpeg without x264 (an LGPL build) can't make the test clip.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let progress = Progress::default();
        assert_eq!(made_to_upload(&clip, &dir, &default_output(), &progress).unwrap(), clip);
        let original = media::Output { mix_only: true, ..Default::default() };
        assert_eq!(made_to_upload(&clip, &dir, &original, &progress).unwrap(), clip);
        assert!(progress.making().is_none(), "nothing was made");
        let smaller = media::Output { height: Some(144), ..default_output() };
        let copy = made_to_upload(&clip, &dir, &smaller, &progress).unwrap();
        assert_ne!(copy, clip);
        assert_eq!(media::probe(&copy).unwrap().height, 144);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dialog says what goes up: the clip itself while it fits, a copy
    /// under the size once it doesn't, never more than a link can be, and
    /// when even a copy can't fit, that it's too long.
    #[test]
    fn the_dialog_says_what_goes_up() {
        let info = media::ClipInfo { duration: 60.0, fps: 60.0, width: 1920, height: 1080, video_kbps: Some(40_000), audio: Vec::new(), id: None, bt601: false };
        let mut edit = media::Edit::new(&info);
        edit.output = default_output();
        let form = |bytes: u64, edit: &media::Edit, duration: f64| UploadForm { info: media::ClipInfo { duration, ..info.clone() }, edit: edit.clone(), bytes, open: false };
        assert_eq!(form(50 << 20, &edit, 60.0).what().0, "As it is, 50 MB");
        let (what, picture) = form(300 << 20, &edit, 60.0).what();
        assert_eq!(what, "A copy under 100 MB");
        assert!(picture.contains("fps"), "{picture}");
        // No size at all is still the most a link can be.
        edit.output.max_mb = None;
        assert_eq!(form(300 << 20, &edit, 60.0).what().0, "A copy under 100 MB");
        // An hour doesn't fit in 100 MB however small: said, and not uploaded.
        let hour = form(3 << 30, &edit, 3600.0);
        assert!(hour.too_long());
        assert_eq!(hour.what().0, "Too long to fit in 100 MB");
        assert!(!form(300 << 20, &edit, 60.0).too_long());
    }

    #[test]
    fn copies_fit_in_a_link_up_to_a_length() {
        assert!(fits_in(LINK_MAX_MB, 60.0));
        assert!(fits_in(LINK_MAX_MB, 20.0 * 60.0), "20 minutes, small");
        assert!(!fits_in(LINK_MAX_MB, 40.0 * 60.0), "40 minutes is too long");
        assert!(!fits_in(10, 10.0 * 60.0), "10 minutes in 10 MB");
        assert_eq!(capped(&media::Output::default()).max_mb, Some(LINK_MAX_MB));
        assert_eq!(capped(&media::Output { max_mb: Some(500), ..Default::default() }).max_mb, Some(LINK_MAX_MB));
        assert_eq!(capped(&media::Output { max_mb: Some(25), ..Default::default() }).max_mb, Some(25));
    }

    #[test]
    fn retries_wait_longer_each_time() {
        assert_eq!(backoff(0), Duration::from_secs(30));
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(20), Duration::from_secs(30 * 60));
    }

    #[test]
    fn the_web_copy_is_a_faststart_mp4_with_the_mix_and_no_tags() {
        let dir = std::env::temp_dir().join(format!("hc-webcopy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Clip.mp4");
        // Two sound tracks (the mix, then a source) and a tag, like a clip.
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=black:s=64x64:d=2", "-f", "lavfi", "-i", "sine=f=440:d=2", "-f", "lavfi", "-i", "sine=f=880:d=2"])
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-c:a", "aac", "-metadata", "comment=hasteclip:id=secret", "-shortest"])
            .arg(&clip)
            .status()
            .unwrap();
        if !made.success() {
            // An ffmpeg without x264 (an LGPL build) can't make the test clip.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let out = dir.join("web.mp4");
        web_copy(&clip, &out).unwrap();
        let probe = media::ffprobe()
            .args(["-v", "error", "-show_entries", "stream=codec_type:format_tags=comment", "-of", "json"])
            .arg(&out)
            .output()
            .unwrap();
        let json = String::from_utf8_lossy(&probe.stdout).into_owned();
        // moov before mdat: it can play before it's all downloaded.
        let bytes = std::fs::read(&out).unwrap();
        let at = |tag: &[u8]| bytes.windows(4).position(|w| w == tag).unwrap();
        let (moov, mdat) = (at(b"moov"), at(b"mdat"));
        assert!(picture(&out, 2.0, &dir.join("thumb.jpg")));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(json.matches("\"audio\"").count(), 1, "{json}");
        assert!(!json.contains("secret"), "{json}");
        assert!(moov < mdat);
    }

    /// Share a clip for real, watch it, rename it, replace it with a shorter
    /// version, stop sharing it.
    #[test]
    #[ignore = "uploads to a real server: HASTECLIP_API=http://localhost:8080 cargo test -p hasteclip share_end_to_end -- --ignored"]
    fn share_end_to_end() {
        assert!(std::env::var_os("HASTECLIP_API").is_some(), "point HASTECLIP_API at a server that isn't the real one");
        let dir = std::env::temp_dir().join(format!("hc-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Clip.mp4");
        // Noise at a high bitrate: ~20 MB, a few parts to upload.
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "nullsrc=s=1280x720:r=30,geq=random(1)*255:128:128", "-f", "lavfi", "-i", "sine=f=440"])
            .args(["-t", "4", "-c:v", "libx264", "-b:v", "40M", "-c:a", "aac", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let progress = Progress::default();
        let named = Named { title: "End to end".into(), game: Some("Test Game".into()) };
        let link = share(&clip, &named, &default_output(), &progress).unwrap();
        assert!(link.url.ends_with(&link.id), "{link:?}");
        assert_eq!((link.named.as_ref(), link.version), (Some(&named), Version::of(&clip)));
        assert_eq!(progress.sent.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
        assert_eq!(status(&link), Some(Status::Shared { views: 0 }));

        // The video comes from the bucket, by way of the link.
        let mut res = ureq::get(format!("{}.mp4", link.url)).header("Range", "bytes=0-11").call().unwrap();
        let start = res.body_mut().read_to_vec().unwrap();
        assert_eq!(&start[4..8], b"ftyp");
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains("og:video") && page.contains("End to end") && page.contains("Test Game"));
        assert!(page.contains(".mp4?v=1"), "files linked by version");
        assert!(page.contains(r#"property="twitter:card" content="player""#), "a player in chats");
        // A chat app looks before it plays: answered, not refused.
        let head = ureq::head(format!("{}.mp4?v=1", link.url)).config().max_redirects(0).build().call().unwrap();
        assert_eq!(head.status(), 200);
        let length = head.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        let res = ureq::get(format!("{}.mp4?v=1", link.url)).header("Range", "bytes=0-0").call().unwrap();
        let total = res.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit('/').next()?.parse::<u64>().ok());
        assert!(length.is_some() && length == total, "{length:?} bytes, the file is {total:?}");

        // Renamed, and moved to another game.
        let renamed = Named { title: "Renamed".into(), game: None };
        rename(&link, &renamed).unwrap();
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains("Renamed") && !page.contains("Test Game"), "{page}");

        // A shorter version, under the same link.
        let size = |url: &str| -> u64 {
            let res = ureq::get(url).header("Range", "bytes=0-0").call().unwrap();
            let range = res.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            range.rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0)
        };
        let before = size(&format!("{}.mp4", link.url));
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&clip)
            .args(["-t", "1.5", "-c", "copy"])
            .arg(dir.join("Trimmed.mp4"))
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::rename(dir.join("Trimmed.mp4"), &clip).unwrap();
        let progress = Progress::default();
        let replaced = replace(&clip, &link, &renamed, &progress).unwrap();
        assert_eq!((replaced.id.as_str(), replaced.url.as_str()), (link.id.as_str(), link.url.as_str()), "the same link");
        assert_eq!(replaced.version, Version::of(&clip));
        let after = size(&format!("{}.mp4?v=2", link.url));
        assert!(after > 0 && after < before, "the new version plays: {after} < {before}");
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains(".mp4?v=2"), "the page links the new version");

        unshare(&link).unwrap();
        assert_eq!(status(&link), Some(Status::Gone));
        assert_eq!(ureq::get(&link.url).config().http_status_as_error(false).build().call().unwrap().status(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
