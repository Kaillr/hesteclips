//! Control from other apps on this computer: HasteClip's Stream Deck
//! plugin, or anything else that speaks it (`docs/control.md` says what's
//! said, line by line).
//!
//! It comes in on the app's own port (see `tray::claim`): a connection
//! that opens with the word `ctrl`, where a later launch says `show` or
//! `quit`. Only the HasteClip that runs has the port (a development test
//! instance beside it doesn't), so only it answers, and only to this
//! computer. From then on each side writes one JSON object a line: the
//! client says hello, then asks for things; the app says hello back, tells
//! it what it's doing (again whenever any of that changes), and answers
//! each request that has an id.
//!
//! What has to happen the moment it's asked for happens on the connection's
//! own thread, as a shortcut's does: saving a clip, taking a screenshot,
//! marking the moment, a sound's mute and volume. While a game covers the
//! window the app may draw no frames for seconds. The rest (scenes,
//! sources, presets, starting and stopping) waits for the app's next frame,
//! which the connection wakes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::App;
use crate::settings::{CaptureTarget, LayerKey, RecordSettings, SourceKind, Visual};

/// The protocol's version, in each side's hello. What's new is only ever
/// added, so a client of an older one is still answered.
pub const VERSION: u64 = 1;

/// The longest line read: anything longer isn't a request.
const MAX_LINE: usize = 64 * 1024;

/// Lines waiting to be written to a client. One that stops reading isn't
/// waited for (see `spawn_writer`).
const BACKLOG: usize = 256;

/// What a client asks for: `{"do":"save_clip"}`, with an `"id"` of its own
/// choosing when it wants to hear how it went.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Request {
    /// Save a clip while clipping. While recording, the moment's marked
    /// too (and only that, without clipping), as the shortcut does.
    SaveClip,
    Screenshot,
    /// Mark the moment in the recording.
    Mark,
    /// Clipping on or off; `on` left out, the other way.
    Clipping { on: Option<bool> },
    Recording { on: Option<bool> },
    /// Going live is asked first, in the app, as everywhere (`go_live.rs`);
    /// ending isn't.
    Stream { on: Option<bool> },
    /// A scene by its id, or the next one (`step: 1`) or the one before
    /// (`-1`), round the end.
    Scene { scene: Option<u64>, step: Option<i64> },
    /// Use a preset, as picking it on the Sources page does.
    Preset { preset: u64 },
    /// A video source shown or hidden in the scene shown (its eye): `screen`,
    /// `webcam:<key>` or `visual:<key>`; `on` left out, the other way.
    Source { source: String, on: Option<bool> },
    /// A sound muted or not; `muted` left out, the other way.
    Mute { sound: String, muted: Option<bool> },
    /// A sound's volume: to `db`, or `by` so many dB up (down below 0),
    /// within the fader's range.
    Volume { sound: String, db: Option<f32>, by: Option<f32> },
    /// The window, in front.
    Open,
}

/// What the app's doing, as clients are told it: whole, when they've said
/// hello and whenever any of it changes.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct State {
    pub clipping: bool,
    pub recording: bool,
    pub streaming: bool,
    /// Whole seconds since the recording started, while recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_secs: Option<u64>,
    /// Whole seconds since going live, while live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_secs: Option<u64>,
    /// How far back a clip reaches.
    pub clip_secs: u32,
    /// Whether screenshots and streaming are there on this system.
    pub can_screenshot: bool,
    pub can_stream: bool,
    /// The scene shown, and the preset's scenes in order.
    pub scene: u64,
    pub scenes: Vec<Item>,
    /// The preset in use, and every preset in order.
    pub preset: u64,
    pub presets: Vec<Item>,
    /// The preset's video sources: the scene shown's, the one in front
    /// first, then those it doesn't have.
    pub sources: Vec<VideoSource>,
    /// The preset's sounds, as the mixer lists them.
    pub sounds: Vec<Sound>,
}

/// A scene or a preset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: u64,
    pub name: String,
}

/// One of the preset's video sources.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoSource {
    /// `screen`, `webcam:<key>` or `visual:<key>`.
    pub id: String,
    pub name: String,
    /// What it is: `screen`, `apps`, `webcam`, `display`, `window`,
    /// `image`, `colour`, `text`, `web` or `video`.
    pub kind: &'static str,
    /// In the scene shown.
    pub in_scene: bool,
    /// Shown in it (its eye open).
    pub on: bool,
}

/// One of the preset's sounds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sound {
    pub id: String,
    pub name: String,
    /// `microphone`, `desktop`, `app` or `video`.
    pub kind: &'static str,
    /// Captured at all (its switch).
    pub on: bool,
    pub muted: bool,
    pub db: f32,
}

/// What a connection does itself, the moment it's asked: the app's
/// `QuickSave` (a stand-in in the tests).
pub trait AtOnce: Send + Sync {
    /// Save a clip, if clipping runs (marking the moment while recording,
    /// as `QuickSave::save` does). Whether a clip was asked for.
    fn save(&self) -> bool;
    /// Take a screenshot, where they can be. Whether it was asked for.
    fn screenshot(&self) -> bool;
    /// Mark the moment in the recording. Whether one runs to mark.
    fn mark(&self) -> bool;
}

impl AtOnce for crate::service::QuickSave {
    fn save(&self) -> bool {
        crate::service::QuickSave::save(self)
    }
    fn screenshot(&self) -> bool {
        crate::service::QuickSave::screenshot(self)
    }
    fn mark(&self) -> bool {
        self.mark_moment()
    }
}

/// A request for the app's next frame, with what its connection did
/// already.
pub struct Asked {
    pub request: Request,
    /// Done already, the moment it was asked: a clip saved, a screenshot
    /// taken, a mute or a volume heard. What's left is the app's to keep in
    /// step (the clip's card, the settings).
    pub done: bool,
    pub reply: Reply,
}

/// Where a request's answer goes, if its client wants one.
pub struct Reply(Option<(u64, SyncSender<String>)>);

impl Reply {
    fn to(id: Option<u64>, out: &SyncSender<String>) -> Self {
        Self(id.map(|id| (id, out.clone())))
    }

    /// No answer (answered already, or never asked for).
    pub fn none() -> Self {
        Self(None)
    }

    pub fn ok(self) {
        if let Some((id, out)) = self.0 {
            let _ = out.try_send(line(&json!({ "reply": id, "ok": true })));
        }
    }

    /// Not done, and why, in a few words.
    pub fn no(self, why: &str) {
        if let Some((id, out)) = self.0 {
            let _ = out.try_send(line(&json!({ "reply": id, "ok": false, "why": why })));
        }
    }
}

/// The app's end: requests to do, and the state to tell.
pub struct Control {
    hub: Arc<Hub>,
    asked: Receiver<Asked>,
    /// The state as last told.
    told: Option<State>,
}

/// What the app and the connections share.
struct Hub {
    asked: Sender<Asked>,
    clients: Mutex<Vec<Client>>,
    next_client: AtomicU64,
    /// Someone said hello since the state was last told: it's told again.
    joined: AtomicBool,
    /// The sounds as last told, as changed since by the connections: what a
    /// mute or a volume step starts from.
    sounds: Mutex<Vec<Sound>>,
    at_once: Arc<dyn AtOnce>,
    live: Arc<capture::mixer::LiveAudio>,
    ctx: egui::Context,
}

struct Client {
    id: u64,
    out: SyncSender<String>,
    socket: TcpStream,
}

/// How connections come in: handed to `tray::serve`, which listens.
#[derive(Clone)]
pub struct Door(Arc<Hub>);

impl Control {
    pub fn new(ctx: egui::Context, at_once: Arc<dyn AtOnce>, live: Arc<capture::mixer::LiveAudio>) -> Self {
        let (tx, rx) = mpsc::channel();
        let hub = Hub { asked: tx, clients: Mutex::default(), next_client: AtomicU64::new(1), joined: AtomicBool::new(false), sounds: Mutex::default(), at_once, live, ctx };
        Self { hub: Arc::new(hub), asked: rx, told: None }
    }

    pub fn door(&self) -> Door {
        Door(self.hub.clone())
    }

    /// What was asked for since the last call.
    pub fn take(&self) -> Vec<Asked> {
        self.asked.try_iter().collect()
    }

    /// Whether anyone listens: the state's only worked out then.
    pub fn listened(&self) -> bool {
        self.hub.joined.load(Ordering::Relaxed) || !self.hub.clients.lock().unwrap().is_empty()
    }

    /// Tell everyone `state`, if it changed (or someone new said hello).
    pub fn tell(&mut self, state: State) {
        let joined = self.hub.joined.swap(false, Ordering::Relaxed);
        if !joined && self.told.as_ref() == Some(&state) {
            return;
        }
        *self.hub.sounds.lock().unwrap() = state.sounds.clone();
        self.hub.broadcast(&line(&json!({ "state": &state })));
        self.told = Some(state);
    }
}

impl Door {
    /// A connection that said `ctrl`: served on a thread of its own.
    pub fn open(&self, stream: TcpStream) {
        let hub = self.0.clone();
        let _ = std::thread::Builder::new().name("control".into()).spawn(move || serve(stream, &hub));
    }
}

impl Hub {
    /// A client that said hello: it hears the state from now on (and at
    /// once, `Control::tell` seeing `joined`).
    fn join(&self, out: SyncSender<String>, socket: TcpStream) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().push(Client { id, out, socket });
        self.joined.store(true, Ordering::Relaxed);
        self.ctx.request_repaint();
        id
    }

    fn leave(&self, id: u64) {
        self.clients.lock().unwrap().retain(|c| c.id != id);
    }

    fn broadcast(&self, line: &str) {
        self.clients.lock().unwrap().retain(|c| match c.out.try_send(line.to_owned()) {
            Ok(()) => true,
            // Not reading what it's sent: let go of.
            Err(TrySendError::Full(_)) => {
                let _ = c.socket.shutdown(Shutdown::Both);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// Do `request`: here and now what has to happen at once (see the
    /// module's doc), the rest in the app's next frame.
    fn ask(&self, request: Request, reply: Reply) {
        match request {
            Request::SaveClip => {
                if self.at_once.save() {
                    reply.ok();
                    self.forward(Request::SaveClip, true, Reply::none());
                } else {
                    // Clipping not on (or not yet seen to be): the app
                    // saves one if it is, or says why not.
                    self.forward(Request::SaveClip, false, reply);
                }
            }
            Request::Screenshot => {
                if self.at_once.screenshot() {
                    reply.ok();
                    self.forward(Request::Screenshot, true, Reply::none());
                } else {
                    self.forward(Request::Screenshot, false, reply);
                }
            }
            Request::Mark => {
                if self.at_once.mark() {
                    reply.ok();
                } else {
                    reply.no("Not recording");
                }
            }
            Request::Mute { sound, muted } => {
                let mut sounds = self.sounds.lock().unwrap();
                let Some(s) = sounds.iter_mut().find(|s| s.id == sound) else {
                    reply.no("No such sound");
                    return;
                };
                s.muted = muted.unwrap_or(!s.muted);
                self.live.channel(&s.id).set_volume(crate::sources_ui::from_db(s.db), s.muted);
                let muted = Some(s.muted);
                drop(sounds);
                reply.ok();
                self.forward(Request::Mute { sound, muted }, true, Reply::none());
            }
            Request::Volume { sound, db, by } => {
                let mut sounds = self.sounds.lock().unwrap();
                let Some(s) = sounds.iter_mut().find(|s| s.id == sound) else {
                    reply.no("No such sound");
                    return;
                };
                let Some(to) = db.or(by.map(|by| s.db + by)).filter(|d| d.is_finite()) else {
                    reply.no("Say how loud");
                    return;
                };
                s.db = fader_db(to);
                self.live.channel(&s.id).set_volume(crate::sources_ui::from_db(s.db), s.muted);
                let db = Some(s.db);
                drop(sounds);
                reply.ok();
                self.forward(Request::Volume { sound, db, by: None }, true, Reply::none());
            }
            other => self.forward(other, false, reply),
        }
    }

    /// Hand `request` to the app's next frame, and wake it.
    fn forward(&self, request: Request, done: bool, reply: Reply) {
        // KDE under Wayland: hidden, the app draws no frames to do it, so
        // it's brought back first, as the tray's Start clipping is. (Only
        // what's done already can wait for it to be shown again.)
        #[cfg(target_os = "linux")]
        if !done && crate::kwin_window::hidden() {
            crate::kwin_window::bring_back();
        }
        let _ = self.asked.send(Asked { request, done, reply });
        self.ctx.request_repaint();
    }
}

/// One client, from its hello until it goes.
fn serve(stream: TcpStream, hub: &Hub) {
    let _ = stream.set_nodelay(true);
    // The hello comes at once, or it isn't a client.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let (Ok(writer), Ok(socket)) = (stream.try_clone(), stream.try_clone()) else { return };
    let mut reader = BufReader::new(stream);
    if !read_line(&mut reader).is_some_and(|l| said_hello(&l)) {
        return;
    }
    // From then on it can be quiet for as long as it likes.
    let _ = socket.set_read_timeout(None);
    let (out, queue) = mpsc::sync_channel(BACKLOG);
    spawn_writer(writer, queue);
    let _ = out.try_send(line(&json!({ "hello": VERSION, "app": "HasteClip", "version": env!("CARGO_PKG_VERSION") })));
    let id = hub.join(out.clone(), socket);
    while let Some(text) = read_line(&mut reader) {
        if text.trim().is_empty() {
            continue;
        }
        match parse(&text) {
            Ok((id, request)) => hub.ask(request, Reply::to(id, &out)),
            // Something this version doesn't know (a newer client's).
            Err(id) => Reply::to(id, &out).no("Not understood"),
        }
    }
    hub.leave(id);
}

/// Write what's queued for a client, as it comes. One that doesn't read
/// it isn't waited for: after a few seconds stuck, the connection's closed
/// (and its reader, seeing that, lets go of it).
fn spawn_writer(mut socket: TcpStream, queue: Receiver<String>) {
    let _ = socket.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = std::thread::Builder::new().name("control out".into()).spawn(move || {
        for line in queue {
            if socket.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
        let _ = socket.shutdown(Shutdown::Both);
    });
}

/// The next line, without its line ending; `None` at the end, or for one
/// that's too long or isn't text.
fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut buf = Vec::new();
    let read = reader.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).ok()?;
    if read == 0 || buf.len() > MAX_LINE {
        return None;
    }
    let text = String::from_utf8(buf).ok()?;
    Some(text.trim_end_matches(['\r', '\n']).to_owned())
}

/// A client's hello: `{"hello":1}` (the version it speaks), at least.
fn said_hello(text: &str) -> bool {
    serde_json::from_str::<Value>(text).ok().and_then(|v| v.get("hello")?.as_u64()).is_some_and(|v| v >= 1)
}

/// A request and its id; for one that isn't understood, its id (if it has
/// one, to say so).
fn parse(text: &str) -> Result<(Option<u64>, Request), Option<u64>> {
    let value: Value = serde_json::from_str(text).map_err(|_| None)?;
    let id = value.get("id").and_then(Value::as_u64);
    serde_json::from_value(value).map(|r| (id, r)).map_err(|_| id)
}

/// `value` as a line to send.
fn line(value: &Value) -> String {
    let mut text = value.to_string();
    text.push('\n');
    text
}

/// `db` within the fader's range, to a tenth, as the fader keeps it.
fn fader_db(db: f32) -> f32 {
    (db.clamp(crate::sources_ui::FADER_MIN_DB, crate::sources_ui::FADER_MAX_DB) * 10.0).round() / 10.0
}

/// A video source's id: `screen`, `webcam:<key>`, `visual:<key>`.
pub fn layer_id(layer: LayerKey) -> String {
    match layer {
        LayerKey::Screen => "screen".to_owned(),
        LayerKey::Webcam(key) => format!("webcam:{key}"),
        LayerKey::Visual(key) => format!("visual:{key}"),
    }
}

/// The video source an id names.
pub fn parse_layer(id: &str) -> Option<LayerKey> {
    if id == "screen" {
        return Some(LayerKey::Screen);
    }
    let (kind, key) = id.split_once(':')?;
    let key = key.parse().ok()?;
    match kind {
        "webcam" => Some(LayerKey::Webcam(key)),
        "visual" => Some(LayerKey::Visual(key)),
        _ => None,
    }
}

fn layer_kind(s: &RecordSettings, layer: LayerKey) -> &'static str {
    match layer {
        LayerKey::Screen if matches!(s.capture, CaptureTarget::Apps { .. }) => "apps",
        LayerKey::Screen => "screen",
        LayerKey::Webcam(_) => "webcam",
        LayerKey::Visual(key) => match s.visual(key).map(|v| &v.visual) {
            Some(Visual::Display { .. }) => "display",
            Some(Visual::Window { .. }) => "window",
            Some(Visual::Image { .. }) => "image",
            Some(Visual::Colour { .. }) => "colour",
            Some(Visual::Text(_)) => "text",
            Some(Visual::Browser { .. }) => "web",
            Some(Visual::Video { .. }) => "video",
            None => "visual",
        },
    }
}

fn sound_kind(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Microphone { .. } => "microphone",
        SourceKind::Desktop { .. } => "desktop",
        SourceKind::App { .. } => "app",
        SourceKind::Video { .. } => "video",
    }
}

impl App {
    /// Do what controls asked for since the last frame.
    pub(crate) fn pump_control(&mut self, ctx: &egui::Context) {
        for asked in self.control.take() {
            self.do_asked(ctx, asked);
        }
    }

    /// Tell the controls what's going on, when any listen.
    pub(crate) fn tell_control(&mut self) {
        if self.control.listened() {
            let state = self.control_state();
            self.control.tell(state);
        }
    }

    fn do_asked(&mut self, ctx: &egui::Context, asked: Asked) {
        let Asked { request, done, reply } = asked;
        match request {
            // Saved already: its card in the library.
            Request::SaveClip if done => self.clip_saving(),
            Request::SaveClip if self.rec_state.clipping => {
                self.save_clip();
                reply.ok();
            }
            // The moment was marked (`QuickSave::save`).
            Request::SaveClip if self.rec_state.recording => reply.ok(),
            Request::SaveClip => reply.no("Clipping isn't on"),
            Request::Screenshot if done => {}
            Request::Screenshot if capture::SCREENSHOTS => {
                self.take_screenshot();
                reply.ok();
            }
            Request::Screenshot => reply.no("No screenshots on this system yet"),
            // Done on its connection.
            Request::Mark => reply.ok(),
            Request::Clipping { on } => {
                if on.unwrap_or(!self.rec_state.clipping) != self.rec_state.clipping {
                    self.toggle_buffer();
                }
                reply.ok();
            }
            Request::Recording { on } => {
                if on.unwrap_or(!self.rec_state.recording) != self.rec_state.recording {
                    self.toggle_record();
                }
                reply.ok();
            }
            Request::Stream { on } => {
                let live = self.rec_state.streaming;
                match on.unwrap_or(!live) {
                    true if live => reply.ok(),
                    true if !capture::STREAMING => reply.no("No streaming on this system yet"),
                    true => {
                        // Asked first, in the window.
                        self.go_live_open = true;
                        self.show_window(ctx);
                        reply.ok();
                    }
                    false => {
                        self.end_stream();
                        reply.ok();
                    }
                }
            }
            Request::Scene { scene, step } => match self.scene_asked(scene, step) {
                Some(id) => {
                    if id != self.settings.active_scene {
                        self.show_scene(id);
                    }
                    reply.ok();
                }
                None => reply.no("No such scene"),
            },
            Request::Preset { preset } => {
                if !self.settings.presets.iter().any(|p| p.id == preset) {
                    reply.no("No such preset");
                } else if preset == self.settings.active_preset {
                    reply.ok();
                } else if let Some(why) = self.presets_locked() {
                    reply.no(&why);
                } else {
                    // Only clipping: its dialog asks first (clipping starts
                    // again, losing what it kept), in the window.
                    let asks = !self.rec_state.idle();
                    self.pick_preset(preset);
                    if asks {
                        self.show_window(ctx);
                    }
                    reply.ok();
                }
            }
            Request::Source { source, on } => {
                let Some(layer) = parse_layer(&source).filter(|&l| self.has_layer(l)) else {
                    reply.no("No such source");
                    return;
                };
                if !self.settings.layer_order().contains(&layer) {
                    reply.no("Not in this scene");
                    return;
                }
                let shown = self.layer_enabled(layer);
                let want = on.unwrap_or(!shown);
                if want != shown {
                    let was = self.arrangement();
                    self.set_layer_enabled(layer, want);
                    self.keep_for_undo(was);
                }
                reply.ok();
            }
            // Heard already (its connection set it live): into the settings.
            Request::Mute { sound, muted } => {
                if let Some(s) = self.settings.audio_sources.iter_mut().find(|s| s.id == sound) {
                    s.muted = muted.unwrap_or(!s.muted);
                    self.live_audio.channel(&s.id).set_volume(crate::sources_ui::from_db(s.volume_db), s.muted);
                }
                reply.ok();
            }
            Request::Volume { sound, db, .. } => {
                if let (Some(s), Some(db)) = (self.settings.audio_sources.iter_mut().find(|s| s.id == sound), db) {
                    s.volume_db = fader_db(db);
                    self.live_audio.channel(&s.id).set_volume(crate::sources_ui::from_db(s.volume_db), s.muted);
                }
                reply.ok();
            }
            Request::Open => {
                self.show_window(ctx);
                reply.ok();
            }
        }
    }

    /// The scene a request names: by id, or a step from the one shown.
    fn scene_asked(&self, scene: Option<u64>, step: Option<i64>) -> Option<u64> {
        let scenes = &self.settings.scenes;
        match (scene, step) {
            (Some(id), _) => scenes.iter().any(|s| s.id == id).then_some(id),
            (None, Some(step)) if !scenes.is_empty() => {
                let at = scenes.iter().position(|s| s.id == self.settings.active_scene).unwrap_or(0) as i64;
                Some(scenes[(at + step).rem_euclid(scenes.len() as i64) as usize].id)
            }
            _ => None,
        }
    }

    /// Whether the preset in use has `layer`.
    fn has_layer(&self, layer: LayerKey) -> bool {
        match layer {
            LayerKey::Screen => true,
            LayerKey::Webcam(key) => self.settings.webcam(key).is_some(),
            LayerKey::Visual(key) => self.settings.visual(key).is_some(),
        }
    }

    /// What the app's doing, as controls are told it.
    pub(crate) fn control_state(&self) -> State {
        let s = &self.settings;
        let r = self.rec_state;
        let order = s.layer_order();
        let mut layers = order.clone();
        let pool = std::iter::once(LayerKey::Screen).chain(s.webcams.iter().map(|w| LayerKey::Webcam(w.key))).chain(s.visuals.iter().map(|v| LayerKey::Visual(v.key)));
        for layer in pool {
            if !layers.contains(&layer) {
                layers.push(layer);
            }
        }
        let since = |on: bool, t: Option<std::time::Instant>| t.filter(|_| on).map(|t| t.elapsed().as_secs());
        State {
            clipping: r.clipping,
            recording: r.recording,
            streaming: r.streaming,
            recording_secs: since(r.recording, self.rec_started),
            stream_secs: since(r.streaming, self.stream_started),
            clip_secs: s.replay_seconds,
            can_screenshot: capture::SCREENSHOTS,
            can_stream: capture::STREAMING,
            scene: s.active_scene,
            scenes: s.scenes.iter().map(|c| Item { id: c.id, name: c.name.clone() }).collect(),
            preset: s.active_preset,
            presets: s.presets.iter().map(|p| Item { id: p.id, name: p.name.clone() }).collect(),
            sources: layers
                .into_iter()
                .map(|l| {
                    let in_scene = order.contains(&l);
                    VideoSource { id: layer_id(l), name: s.layer_title(l), kind: layer_kind(s, l), in_scene, on: in_scene && self.layer_enabled(l) }
                })
                .collect(),
            sounds: s.audio_sources.iter().map(|a| Sound { id: a.id.clone(), name: a.name.clone(), kind: sound_kind(&a.kind), on: a.enabled, muted: a.muted, db: a.volume_db }).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    /// Saves, screenshots and marks, counted; `clipping` says whether a
    /// clip can be saved.
    #[derive(Default)]
    struct Counted {
        clipping: AtomicBool,
        saves: AtomicUsize,
        marks: AtomicUsize,
    }

    impl AtOnce for Counted {
        fn save(&self) -> bool {
            let on = self.clipping.load(Ordering::Relaxed);
            if on {
                self.saves.fetch_add(1, Ordering::Relaxed);
            }
            on
        }
        fn screenshot(&self) -> bool {
            false
        }
        fn mark(&self) -> bool {
            self.marks.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    /// A client, as the Stream Deck plugin is: through the app's port.
    struct Client {
        out: TcpStream,
        lines: BufReader<TcpStream>,
    }

    impl Client {
        fn connect(port: u16) -> Self {
            let out = TcpStream::connect(("127.0.0.1", port)).unwrap();
            out.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let lines = BufReader::new(out.try_clone().unwrap());
            Self { out, lines }
        }

        fn send(&mut self, text: &str) {
            self.out.write_all(text.as_bytes()).unwrap();
        }

        fn next(&mut self) -> Value {
            let text = read_line(&mut self.lines).expect("a line");
            serde_json::from_str(&text).unwrap()
        }
    }

    /// The app's port, served on a spare one, with control.
    fn serving(at_once: Arc<dyn AtOnce>) -> (u16, Control) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let control = Control::new(egui::Context::default(), at_once, capture::mixer::LiveAudio::new());
        // The tray's own commands (show, quit) aren't used here.
        let (tx, _) = mpsc::channel();
        crate::tray::serve(listener, &egui::Context::default(), tx, Some(control.door()));
        (port, control)
    }

    /// A client through the app's port, its hello said and answered.
    fn said_hello_to(port: u16, control: &Control) -> Client {
        let mut client = Client::connect(port);
        client.send("ctrl{\"hello\":1,\"client\":\"test\"}\n");
        let hello = client.next();
        assert_eq!(hello["hello"], VERSION);
        assert_eq!(hello["app"], "HasteClip");
        joined(control);
        client
    }

    /// The next `n` requests handed to the app, waiting a little for them.
    fn asked_n(control: &Control, n: usize) -> Vec<Asked> {
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(control.take());
            if got.len() >= n {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{} of {n} requests came", got.len());
    }

    fn asked(control: &Control) -> Asked {
        asked_n(control, 1).remove(0)
    }

    /// Wait until the hello's been handled (the client's joined).
    fn joined(control: &Control) {
        for _ in 0..200 {
            if control.listened() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the client never joined");
    }

    fn sample_state() -> State {
        State {
            clipping: true,
            clip_secs: 120,
            scene: 1,
            scenes: vec![Item { id: 1, name: "Gameplay".into() }, Item { id: 2, name: "Just chatting".into() }],
            sounds: vec![Sound { id: "src-mic".into(), name: "Microphone".into(), kind: "microphone", on: true, muted: false, db: -3.0 }],
            ..Default::default()
        }
    }

    /// The whole exchange: "ctrl", the hellos, the state (told once, and
    /// again only when it changes), a request handed to the app with its
    /// answer, and one this version doesn't know.
    #[test]
    fn a_client_is_told_and_answered() {
        let (port, mut control) = serving(Arc::new(Counted::default()));
        let mut client = said_hello_to(port, &control);
        control.tell(sample_state());
        let state = client.next();
        assert_eq!(state["state"]["clipping"], true);
        assert_eq!(state["state"]["scenes"][1]["name"], "Just chatting");
        assert!(state["state"].get("recording_secs").is_none(), "left out while not recording");
        // The same again isn't told; a change is.
        control.tell(sample_state());
        control.tell(State { recording: true, recording_secs: Some(3), ..sample_state() });
        assert_eq!(client.next()["state"]["recording_secs"], 3);

        client.send("{\"id\":7,\"do\":\"scene\",\"step\":1}\n");
        let a = asked(&control);
        assert_eq!(a.request, Request::Scene { scene: None, step: Some(1) });
        assert!(!a.done);
        a.reply.no("No such scene");
        let reply = client.next();
        assert_eq!((reply["reply"].as_u64(), reply["ok"].as_bool(), reply["why"].as_str()), (Some(7), Some(false), Some("No such scene")));

        client.send("{\"id\":8,\"do\":\"juggle\"}\n");
        let reply = client.next();
        assert_eq!((reply["reply"].as_u64(), reply["ok"].as_bool()), (Some(8), Some(false)));
    }

    /// Saving a clip and marking the moment happen on the connection, at
    /// once, and are answered from there; the app only keeps in step.
    /// Without clipping, the app's frame decides (and says why not).
    #[test]
    fn a_clip_is_saved_at_once() {
        let at_once = Arc::new(Counted::default());
        let (port, control) = serving(at_once.clone());
        let mut client = said_hello_to(port, &control);

        at_once.clipping.store(true, Ordering::Relaxed);
        client.send("{\"id\":1,\"do\":\"save_clip\"}\n");
        assert_eq!(client.next()["ok"], true, "answered before the app's frame");
        assert_eq!(at_once.saves.load(Ordering::Relaxed), 1);
        let a = asked(&control);
        assert_eq!((a.request, a.done), (Request::SaveClip, true));

        at_once.clipping.store(false, Ordering::Relaxed);
        client.send("{\"id\":2,\"do\":\"save_clip\"}\n");
        let a = asked(&control);
        assert_eq!((a.request.clone(), a.done), (Request::SaveClip, false));
        a.reply.no("Clipping isn't on");
        assert_eq!(client.next()["why"], "Clipping isn't on");

        client.send("{\"id\":3,\"do\":\"mark\"}\n");
        assert_eq!(client.next()["ok"], true);
        assert_eq!(at_once.marks.load(Ordering::Relaxed), 1);
        assert!(control.take().is_empty(), "a mark is the connection's alone");
    }

    /// A mute is heard at once (the live channel) and handed to the app
    /// as what it is now, not "the other way": two presses before the app's
    /// frame are muted, then not. A volume step is in the fader's range.
    #[test]
    fn mute_and_volume_are_heard_at_once() {
        let (port, mut control) = serving(Arc::new(Counted::default()));
        let mut client = said_hello_to(port, &control);
        control.tell(sample_state());
        client.next();

        client.send("{\"do\":\"mute\",\"sound\":\"src-mic\"}\n{\"do\":\"mute\",\"sound\":\"src-mic\"}\n");
        let both = asked_n(&control, 2);
        assert_eq!(both[0].request, Request::Mute { sound: "src-mic".into(), muted: Some(true) });
        assert_eq!(both[1].request, Request::Mute { sound: "src-mic".into(), muted: Some(false) });
        assert!(both[0].done && both[1].done);

        client.send("{\"id\":4,\"do\":\"volume\",\"sound\":\"src-mic\",\"by\":20}\n");
        assert_eq!(client.next()["ok"], true);
        assert_eq!(asked(&control).request, Request::Volume { sound: "src-mic".into(), db: Some(12.0), by: None });
        client.send("{\"id\":5,\"do\":\"mute\",\"sound\":\"src-nope\"}\n");
        assert_eq!(client.next()["why"], "No such sound");
    }

    /// A connection that doesn't say hello (a browser's request to the
    /// port, say) is let go of, and never hears anything.
    #[test]
    fn no_hello_no_control() {
        let (port, control) = serving(Arc::new(Counted::default()));
        let mut client = Client::connect(port);
        client.send("ctrl / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(read_line(&mut client.lines).is_none(), "closed without a word");
        assert!(!control.listened());
    }

    /// Every request, as clients write them.
    #[test]
    fn requests_as_written() {
        let cases = [
            (r#"{"do":"save_clip"}"#, Request::SaveClip),
            (r#"{"do":"clipping","on":false}"#, Request::Clipping { on: Some(false) }),
            (r#"{"do":"recording"}"#, Request::Recording { on: None }),
            (r#"{"do":"stream","on":true}"#, Request::Stream { on: Some(true) }),
            (r#"{"id":3,"do":"scene","scene":5}"#, Request::Scene { scene: Some(5), step: None }),
            (r#"{"do":"preset","preset":2}"#, Request::Preset { preset: 2 }),
            (r#"{"do":"source","source":"webcam:4"}"#, Request::Source { source: "webcam:4".into(), on: None }),
            (r#"{"do":"volume","sound":"a","db":-6.5}"#, Request::Volume { sound: "a".into(), db: Some(-6.5), by: None }),
            (r#"{"do":"open"}"#, Request::Open),
        ];
        for (text, want) in cases {
            assert_eq!(parse(text).map(|(_, r)| r), Ok(want), "{text}");
        }
        assert_eq!(parse(r#"{"id":9,"do":"preset"}"#).map(|(_, r)| r), Err(Some(9)), "a preset needs its id");
        assert_eq!(parse("GET / HTTP/1.1").map(|(_, r)| r), Err(None));
    }

    #[test]
    fn layer_ids_round_trip() {
        for layer in [LayerKey::Screen, LayerKey::Webcam(3), LayerKey::Visual(12)] {
            assert_eq!(parse_layer(&layer_id(layer)), Some(layer));
        }
        assert_eq!(parse_layer("webcam:x"), None);
        assert_eq!(parse_layer("mic:1"), None);
    }
}
