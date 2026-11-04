//! Demo recordings: the real app, driven by a script and recorded to a video,
//! for the website. Only in a build with the `demo` feature, and only on when
//! `HASTECLIP_DEMO_SCRIPT=<script.json>` is set
//! (with `HASTECLIP_LIBRARY` pointing at a copy of a library, so nothing real
//! changes); otherwise every call here is one cheap check.
//!
//! The script moves a drawn pointer, clicks, drags and presses keys, fed to
//! egui as input, so the app does exactly what it does for a person: hovers,
//! menus, animations. It aims at named spots (`mark`): buttons and menu items
//! by their label, clip cards by their title, the library's filters by name,
//! and a few controls (play, the trim handles) by a fixed name. Each frame the
//! app draws is read back from the GPU and piped to ffmpeg, so the window
//! needn't be in front.
//!
//! The script is JSON: where the video goes, optionally clips that saving
//! hands over (`saves`, see `capture_thread`), and the steps (see `Step`):
//!
//! ```json
//! { "out": "demo.mp4", "saves": ["CS2/clip_2026-10-14_10-47-46 - Ace on Ancient.mp4"], "steps": [
//!   { "clipping": true },                    // start clipping (nothing is recorded)
//!   { "shortcut": "save" },                  // the save shortcut, as from a game
//!   { "wait": 0.8 },                         // hold still
//!   { "move": "clip:1", "secs": 0.9 },       // glide onto the first card
//!   { "click": true },                       // (or "double": true, "right": true)
//!   { "play": true },                        // play, if it isn't
//!   { "trim": "in", "to": "marker:1", "before": 3 },  // a trim handle to a moment
//!   { "seek": "marker:1", "before": 1 },     // the playhead to a moment
//!   { "drag": "clip:2", "to": "filter:Best of the week" },  // or "by": [x, y]
//!   { "key": "cmd+s" },  { "type": "text" },  { "scroll": 300 },
//!   { "pointer": false }                     // hide the pointer
//! ] }
//! ```
//!
//! Targets: `preview:x,y` (a point of the Sources page's preview, as
//! fractions of it), `card:<source>` and `grip:<source>` (a picture
//! source's card under it, and its handle: `card:Webcam`), `clip:<title>`
//! or `clip:N` (the Nth card on screen),
//! `filter:<name>` (the library's games and collections), `button:<label>`
//! (buttons and menu items, by label or tooltip), `select:<value>` (a
//! dropdown, by what it shows), `play`, `trim:in`,
//! `trim:out`, the editor's `part:N` and `split:N` (the Nth part kept, the
//! Nth split), or a point `[x, y]` in the window's points. Moments:
//! `marker:N` (the open clip's Nth marker, put in with B) or seconds.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc;

use egui::{Color32, Context, Event, Id, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};
use serde::Deserialize;

/// The script, when `HASTECLIP_DEMO_SCRIPT` names one. Only in a build with
/// the `demo` feature: in any other, demos are off and all of this is unused.
fn script_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::var_os("HASTECLIP_DEMO_SCRIPT").map(PathBuf::from)?;
        if !cfg!(feature = "demo") {
            fail(&format!("this build can't record demos ({}): build with `--features demo`", path.display()));
        }
        Some(path)
    })
    .as_ref()
}

/// Whether a demo is being recorded.
pub fn on() -> bool {
    script_path().is_some()
}

/// Name a spot on screen, this frame, for the script to aim at. Free when no
/// demo is running.
pub fn mark(ctx: &Context, name: impl FnOnce() -> String, rect: Rect) {
    if !on() {
        return;
    }
    let name = name();
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Marks>(marks_id()).now.insert(name, rect));
}

/// The open clip's timeline, as the ruler draws it: where it is, the span of
/// time it shows (start, length) and the markers' times, so `seek:
/// "marker:N"` can click exactly where a marker is.
pub fn timeline(ctx: &Context, rect: Rect, view: (f64, f64), markers: impl Iterator<Item = f64>) {
    if !on() {
        return;
    }
    let mut m: Vec<f64> = markers.collect();
    m.sort_by(f64::total_cmp);
    ctx.data_mut(|d| d.insert_temp(markers_id(), Timeline { rect, view, markers: m }));
}

#[derive(Clone)]
struct Timeline {
    rect: Rect,
    view: (f64, f64),
    markers: Vec<f64>,
}

impl Timeline {
    /// Seconds into the clip of a moment the script names.
    fn time_of(&self, m: &Moment) -> f64 {
        match m {
            Moment::Secs(s) => *s,
            Moment::Named(name) => {
                let n: usize = name.strip_prefix("marker:").and_then(|n| n.parse().ok()).unwrap_or_else(|| fail(&format!("a moment is \"marker:N\" or seconds, not \"{name}\"")));
                *self.markers.get(n.saturating_sub(1)).unwrap_or_else(|| fail(&format!("the open clip has {} markers, not {n} (put them in with B in the player)", self.markers.len())))
            }
        }
    }

    /// Where a time is on the timeline, as the ruler draws it.
    fn x_of(&self, t: f64) -> f32 {
        let (v0, span) = self.view;
        self.rect.left() + (((t - v0) / span.max(0.001)).clamp(0.0, 1.0) as f32) * self.rect.width()
    }
}

/// Whether the open clip is playing, as its play button shows: so `play` can
/// press it only when it needs to.
pub fn playing(ctx: &Context, playing: bool) {
    if on() {
        ctx.data_mut(|d| d.insert_temp(playing_id(), playing));
    }
}

fn playing_id() -> Id {
    Id::new("demo_playing")
}
fn marks_id() -> Id {
    Id::new("demo_marks")
}
fn markers_id() -> Id {
    Id::new("demo_markers")
}

/// This frame's marks, and last frame's (what the script aims with, since
/// this frame's are still being drawn).
#[derive(Clone, Default)]
struct Marks {
    now: HashMap<String, Rect>,
    last: HashMap<String, Rect>,
}

#[derive(Deserialize)]
struct Script {
    /// Where the video goes (relative to the script).
    out: PathBuf,
    /// Frames a second.
    #[serde(default = "default_fps")]
    fps: u32,
    /// Clips that saving hands over, in turn (paths inside the library). They
    /// leave the library when the demo starts and come back as new clips.
    #[serde(default)]
    saves: Vec<PathBuf>,
    /// Clipping is on from the first frame (as it is when you use the app),
    /// not turned on by a step.
    #[serde(default)]
    #[allow(dead_code)] // read by `clipping_from_start`
    clipping: bool,
    steps: Vec<Step>,
}

/// Whether the script has clipping on from the start.
pub fn clipping_from_start() -> bool {
    script_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|v| v["clipping"] == true)
}

fn default_fps() -> u32 {
    60
}

/// One thing the script does. Targets: `"clip:<title>"` or `"clip:2"` (the
/// second card on screen), `"filter:<name>"`,
/// `"button:<label>"` (buttons and menu items), `"play"`, `"trim:in"`,
/// `"trim:out"`, `"timeline"`, or a point `[x, y]` in the window's points.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Step {
    /// Hold still.
    Wait { wait: f64 },
    /// Glide the pointer onto a target.
    Move {
        #[serde(rename = "move")]
        to: Target,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Click where the pointer is (twice for a double click; with the right
    /// button for a menu).
    Click {
        #[allow(dead_code)]
        click: bool,
        #[serde(default)]
        double: bool,
        #[serde(default)]
        right: bool,
        /// Keys held, like `"cmd"` (select a clip) or `"shift"`.
        #[serde(default)]
        mods: Option<String>,
    },
    /// Press the button on a target (or where the pointer is), move by `by`
    /// points or onto `to`, let go.
    Drag {
        drag: Target,
        #[serde(default)]
        by: Option<[f32; 2]>,
        #[serde(default)]
        to: Option<Target>,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Press a key, with modifiers: `"space"`, `"cmd+s"`, `"alt+f10"`, `"e"`.
    Key { key: String },
    /// Type text into whatever has focus.
    Type {
        #[serde(rename = "type")]
        text: String,
    },
    /// Scroll by points (positive goes down the page).
    Scroll {
        scroll: f32,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Move the open clip's playhead to a marker (`"marker:1"` is the first),
    /// `before` seconds ahead of it, by clicking the timeline there.
    Seek {
        seek: Moment,
        #[serde(default)]
        before: f64,
    },
    /// Make the open clip play (`true`) or pause (`false`): moves to its play
    /// button and clicks it, unless it's already that way.
    Play { play: bool },
    /// Drag a trim handle (`"in"` or `"out"`) to a moment: `"marker:N"`, or
    /// seconds into the clip, `before` (or `after`, negative) seconds off it.
    Trim {
        trim: String,
        to: Moment,
        #[serde(default)]
        before: f64,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Put back what the scene changed, so it can end as it began (a looping
    /// video): `"saves"` takes the clips saving handed over back out of the
    /// library, `"collections"` empties every collection.
    Reset { reset: String },
    /// Show or hide the pointer.
    Pointer { pointer: bool },
    /// Start (`true`) or stop clipping, as the app does at launch. Nothing is
    /// recorded in a demo; saving hands over the script's `saves`.
    Clipping { clipping: bool },
    /// Press a global shortcut, as from inside a game: `"save"` (save a clip),
    /// `"clipping"`, `"record"`.
    Shortcut { shortcut: String },
}

/// What the app does for a step that's more than input: taken by the app
/// each frame ([`Demo::take_action`]).
pub enum Action {
    Unsave,
    EmptyCollections,
    Clipping(bool),
    Shortcut(crate::settings::ShortcutAction),
}

fn default_move() -> f64 {
    0.7
}

/// A moment in the open clip: `"marker:N"` (the Nth marker) or seconds.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Moment {
    Named(String),
    Secs(f64),
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Target {
    Named(String),
    Point([f32; 2]),
}

/// A running demo: the script's place, the pointer, the recorder.
pub struct Demo {
    steps: Vec<Step>,
    step: usize,
    /// When the current step started (egui time), and where the pointer was.
    started: Option<(f64, Pos2)>,
    pointer: Pos2,
    shown: bool,
    down: bool,
    /// Frames waiting to be asked for or written.
    recorder: Option<Recorder>,
    /// The first frame's time: the video's zero.
    zero: Option<f64>,
    done: bool,
    /// Waiting for a target that isn't drawn yet, since when.
    waiting: Option<f64>,
    /// A seek step's target point, once worked out.
    seek_at: Option<Pos2>,
    /// How far into a click: press, release, (press, release).
    click_frame: u8,
    /// Where a drag took hold.
    grabbed: Option<Pos2>,
    /// When a trim's pull started (step time).
    pull_started: Option<f64>,
    /// For the app to do this frame.
    action: Option<Action>,
    /// Before recording starts: the last frame's time, and since when frames
    /// have come on time.
    last_frame: Option<f64>,
    steady_since: Option<f64>,
}

/// Seconds of steady frames before the recording starts.
const WARM_UP: f64 = 1.0;

impl Demo {
    /// The demo the environment asks for, if any. Exits with a message if the
    /// script can't be read: a demo run has nothing better to do.
    pub fn from_env(ctx: &Context) -> Option<Self> {
        let path = script_path()?;
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("can't read {}: {e}", path.display())));
        let script: Script = serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
        let out = path.parent().unwrap_or(std::path::Path::new(".")).join(&script.out);
        hide_saves(&script.saves);
        eprintln!("demo: {} steps → {}", script.steps.len(), out.display());
        ctx.request_repaint();
        Some(Self {
            steps: script.steps,
            step: 0,
            started: None,
            pointer: pos2(600.0, 420.0),
            shown: false,
            down: false,
            recorder: Some(Recorder::new(out, script.fps)),
            zero: None,
            done: false,
            waiting: None,
            seek_at: None,
            click_frame: 0,
            grabbed: None,
            pull_started: None,
            action: None,
            last_frame: None,
            steady_since: None,
        })
    }

    /// Before each frame: drop the real mouse and keyboard (a stray hand
    /// mustn't steer the demo), then add this frame's scripted input. Also
    /// hands the recorder the frame the GPU sent back.
    pub fn input(&mut self, ctx: &Context, raw: &mut egui::RawInput) {
        // The GPU's copies of earlier frames arrive as events.
        let mut frames = Vec::new();
        raw.events.retain(|e| match e {
            Event::Screenshot { image, .. } => {
                frames.push(image.clone());
                false
            }
            Event::PointerMoved(_) | Event::PointerButton { .. } | Event::MouseWheel { .. } | Event::Key { .. } | Event::ModifiersChanged(_) | Event::Text(_) | Event::PointerGone => false,
            _ => true,
        });
        let now = raw.time.unwrap_or(0.0);
        if let Some(rec) = &mut self.recorder {
            for f in frames {
                rec.frame(f, now - self.zero.unwrap_or(now));
            }
        }
        if self.done {
            return;
        }
        // Start once the app has settled (the library loaded, thumbnails in):
        // WARM_UP seconds of frames each on time.
        if self.zero.is_none() {
            let steady = self.last_frame.is_some_and(|l| now - l < 0.025);
            self.last_frame = Some(now);
            self.steady_since = if steady { self.steady_since.or(Some(now)) } else { None };
            if self.steady_since.is_none_or(|s| now - s < WARM_UP) {
                return;
            }
            self.zero = Some(now);
        }
        // The step's events go after the pointer's move to where it was, then
        // one to where the step took it: a click lands where the pointer is.
        // Hidden, it's not over anything: no hover (a card would start playing
        // under it).
        let hidden = !self.shown;
        raw.events.push(if hidden { Event::PointerGone } else { Event::PointerMoved(self.pointer) });
        let at = raw.events.len();
        self.step_script(ctx, raw, now);
        if !self.shown && raw.events.len() == at {
            // Still hidden after the step: stays gone.
        } else if raw.events.len() == at || !matches!(raw.events.last(), Some(Event::PointerMoved(_))) {
            raw.events.push(Event::PointerMoved(self.pointer));
        }
    }

    fn step_script(&mut self, ctx: &Context, raw: &mut egui::RawInput, now: f64) {
        let marks = ctx.data(|d| d.get_temp::<Marks>(marks_id())).unwrap_or_default().last;
        let Some(step) = self.steps.get(self.step).cloned() else {
            // Done: a moment for the last frames to come back, then quit.
            if self.started.is_none() {
                self.started = Some((now, self.pointer));
            }
            if now - self.started.map_or(now, |s| s.0) > 0.3 {
                self.done = true;
                if let Some(rec) = self.recorder.take() {
                    rec.finish();
                }
                // Prepared clips that weren't saved go back where they were.
                if let Some(q) = SAVES.get().and_then(|q| q.lock().ok()) {
                    let lib = library();
                    for (held, rel) in q.iter() {
                        let _ = std::fs::rename(held, lib.join(rel));
                    }
                }
                let _ = std::fs::remove_dir(waiting_room());
                eprintln!("demo: done");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        };
        let (start, from) = *self.started.get_or_insert((now, self.pointer));
        let t = now - start;
        let next = |s: &mut Self| {
            s.step += 1;
            s.started = None;
            s.waiting = None;
            s.seek_at = None;
            s.grabbed = None;
            s.pull_started = None;
        };
        let resolve = |s: &mut Self, target: &Target| -> Option<Pos2> {
            match target {
                Target::Point([x, y]) => Some(pos2(*x, *y)),
                Target::Named(name) if name.starts_with("preview:") => {
                    // A point of the Sources page's preview, as fractions of it.
                    let at: Vec<f32> = name["preview:".len()..].split(',').filter_map(|v| v.trim().parse().ok()).collect();
                    let [fx, fy] = at[..] else { fail(&format!("a point of the preview is \"preview:x,y\" (fractions of it), not \"{name}\"")) };
                    match marks.get("preview") {
                        Some(r) => Some(pos2(r.left() + fx * r.width(), r.top() + fy * r.height())),
                        None => {
                            let since = *s.waiting.get_or_insert(now);
                            if now - since > 5.0 {
                                fail("the Sources page's preview isn't on screen");
                            }
                            None
                        }
                    }
                }
                Target::Named(name) => match marks.get(name).copied().or_else(|| nth_clip(&marks, name)).or_else(|| by_prefix(&marks, name)) {
                    Some(r) => Some(r.center()),
                    None => {
                        let since = *s.waiting.get_or_insert(now);
                        if now - since > 5.0 {
                            let mut known: Vec<&String> = marks.keys().collect();
                            known.sort();
                            fail(&format!("nothing on screen is called \"{name}\". On screen now: {known:?}"));
                        }
                        None
                    }
                },
            }
        };
        let modifiers = Modifiers::NONE;
        match step {
            Step::Wait { wait } => {
                if t >= wait {
                    next(self);
                }
            }
            Step::Reset { reset } => {
                self.action = Some(match reset.as_str() {
                    "saves" => Action::Unsave,
                    "collections" => Action::EmptyCollections,
                    other => fail(&format!("reset \"saves\" or \"collections\", not \"{other}\"")),
                });
                next(self);
            }
            Step::Pointer { pointer } => {
                self.shown = pointer;
                next(self);
            }
            Step::Trim { trim, to, before, secs } => {
                // Held by its top, over the picture lane: lower down it'd
                // cross a sound lane's volume line and show its level.
                let Some(handle) = marks.get(&format!("trim:{trim}")).copied() else {
                    resolve(self, &Target::Named(format!("trim:{trim}")));
                    self.started = None;
                    return;
                };
                // Once held, the handle moves with the pointer: the pull goes
                // from where it took hold, never from where the handle is now
                // (that ran away, the whole way in a few frames).
                let grab = self.grabbed.unwrap_or_else(|| pos2(handle.center().x, handle.top() + 24.0));
                let Some(tl) = ctx.data(|d| d.get_temp::<Timeline>(markers_id())) else { return };
                let end = pos2(tl.x_of(tl.time_of(&to) - before), grab.y);
                // A long way takes longer, as a hand does.
                let reach = if from.distance(grab) > 1.0 { (from.distance(grab) as f64 / 1100.0).clamp(0.4, 1.2) } else { 0.0 };
                if t < reach {
                    self.shown = true;
                    self.pointer = glide(from, grab, t, reach);
                    return;
                }
                if !self.down {
                    self.pointer = grab;
                    raw.events.push(Event::PointerButton { pos: grab, button: PointerButton::Primary, pressed: true, modifiers });
                    self.down = true;
                    // Where it's held and where it's pulled to are fixed now.
                    self.grabbed = Some(grab);
                    self.seek_at = Some(end);
                    self.pull_started = Some(t);
                    return;
                }
                let end = self.seek_at.unwrap_or(end);
                let pull = t - self.pull_started.unwrap_or(reach);
                // A long pull takes longer (`secs` is for a short one), at a
                // hand's pace: about 440 points a second, at most 3 s.
                let secs = secs.max((grab.distance(end) as f64 / 440.0).min(3.0));
                self.pointer = grab + (end - grab) * ease_gentle((pull / secs).clamp(0.0, 1.0)) as f32;
                if pull >= secs + 0.05 {
                    raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed: false, modifiers });
                    self.down = false;
                    eprintln!("demo: trim {trim} pulled {:.0} points in {secs:.2} s", grab.distance(end));
                    next(self);
                }
            }
            Step::Clipping { clipping } => {
                self.action = Some(Action::Clipping(clipping));
                next(self);
            }
            Step::Shortcut { shortcut } => {
                use crate::settings::ShortcutAction as S;
                let a = match shortcut.as_str() {
                    "save" => S::SaveClip,
                    "clipping" => S::ToggleBuffer,
                    "record" => S::ToggleRecord,
                    other => fail(&format!("not a shortcut: \"{other}\" (save, clipping, record)")),
                };
                self.action = Some(Action::Shortcut(a));
                next(self);
            }
            Step::Play { play } => {
                let playing = ctx.data(|d| d.get_temp::<bool>(playing_id()));
                // Already so (and not mid-press): nothing to do.
                if playing == Some(play) && !self.down && self.click_frame == 0 {
                    next(self);
                    return;
                }
                let Some(to) = resolve(self, &Target::Named("play".into())) else {
                    self.started = None;
                    return;
                };
                let secs = 0.6;
                self.shown = true;
                if t < secs {
                    self.pointer = glide(from, to, t, secs);
                    return;
                }
                // Press, then release a frame later.
                let pressed = self.click_frame == 0;
                raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed, modifiers });
                self.down = pressed;
                if pressed {
                    self.click_frame = 1;
                } else {
                    self.click_frame = 0;
                    next(self);
                }
            }
            Step::Move { to, secs } => {
                let Some(to) = resolve(self, &to) else {
                    self.started = None;
                    return;
                };
                self.shown = true;
                self.pointer = glide(from, to, t, secs);
                if t >= secs {
                    next(self);
                }
            }
            Step::Click { double, right, mods, .. } => {
                let modifiers = mods.as_deref().map_or(Modifiers::NONE, |m| parse_key(&format!("{m}+a")).1);
                // Press and release on frames of their own, like a hand: a
                // press, a frame held, a release (twice for a double click).
                let frames = if double { 4 } else { 2 };
                let n = self.click_frame;
                if n == 0 && modifiers != Modifiers::NONE {
                    raw.events.push(Event::ModifiersChanged(modifiers));
                }
                if n < frames {
                    let pressed = n.is_multiple_of(2);
                    let button = if right { PointerButton::Secondary } else { PointerButton::Primary };
                    raw.events.push(Event::PointerButton { pos: self.pointer, button, pressed, modifiers });
                    self.down = pressed && !right;
                    self.click_frame += 1;
                } else {
                    self.click_frame = 0;
                    raw.events.push(Event::ModifiersChanged(Modifiers::NONE));
                    next(self);
                }
            }
            Step::Drag { drag, by, to, secs } => {
                // Found until it's grabbed: what it is may change once pulled
                // (a split pulled open is two handles).
                let Some(grab) = self.grabbed.or_else(|| resolve(self, &drag)) else {
                    self.started = None;
                    return;
                };
                // First glide onto what's grabbed, then hold and pull.
                // A long way takes longer, as a hand does.
                let reach = if from.distance(grab) > 1.0 { (from.distance(grab) as f64 / 1100.0).clamp(0.4, 1.2) } else { 0.0 };
                if t < reach {
                    self.shown = true;
                    self.pointer = glide(from, grab, t, reach);
                    return;
                }
                if !self.down {
                    self.pointer = grab;
                    raw.events.push(Event::PointerButton { pos: grab, button: PointerButton::Primary, pressed: true, modifiers });
                    self.down = true;
                    self.grabbed = Some(grab);
                    return;
                }
                // Pull from where it was grabbed: what's dragged moves with the
                // pointer, so its mark does too.
                let grab = self.grabbed.unwrap_or(grab);
                let end = match (&to, by) {
                    (Some(target), _) => match resolve(self, target) {
                        Some(p) => p,
                        None => return,
                    },
                    (None, Some([dx, dy])) => grab + vec2(dx, dy),
                    (None, None) => grab,
                };
                let pull = t - reach;
                self.pointer = glide(grab, end, pull, secs);
                if pull >= secs + 0.05 {
                    raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed: false, modifiers });
                    self.down = false;
                    next(self);
                }
            }
            Step::Key { key } => {
                let (key, mods) = parse_key(&key);
                for pressed in [true, false] {
                    raw.events.push(Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: mods });
                }
                next(self);
            }
            Step::Type { text } => {
                raw.events.push(Event::Text(text));
                next(self);
            }
            Step::Scroll { scroll, secs } => {
                // Spread over the step's frames, eased, like a trackpad.
                let dt = f64::from(raw.predicted_dt);
                let a = ease(((t - dt) / secs).clamp(0.0, 1.0));
                let b = ease((t / secs).clamp(0.0, 1.0));
                let delta = (b - a) as f32 * scroll;
                raw.events.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -delta), phase: egui::TouchPhase::Move, modifiers });
                if t >= secs {
                    next(self);
                }
            }
            Step::Seek { seek, before } => {
                if self.seek_at.is_none() {
                    let Some(tl) = ctx.data(|d| d.get_temp::<Timeline>(markers_id())) else { return };
                    let at = tl.time_of(&seek) - before;
                    self.seek_at = Some(pos2(tl.x_of(at), tl.rect.center().y));
                }
                let to = self.seek_at.unwrap_or(self.pointer);
                let secs = 0.6;
                self.shown = true;
                self.pointer = glide(from, to, t, secs);
                if t >= secs {
                    // Down this frame, up the next.
                    let pressed = !self.down;
                    raw.events.push(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed, modifiers });
                    self.down = pressed;
                    if !pressed {
                        next(self);
                    }
                }
            }
        }
    }

    /// A step's action for the app, if this frame has one.
    pub fn take_action(&mut self) -> Option<Action> {
        self.action.take()
    }

    /// After the app has drawn: the pointer on top, last frame's marks kept
    /// for aiming, and the frame asked for.
    pub fn after(&mut self, ctx: &Context) {
        ctx.data_mut(|d| {
            let m = d.get_temp_mut_or_default::<Marks>(marks_id());
            m.last = std::mem::take(&mut m.now);
        });
        draw_window_buttons(ctx);
        if self.shown {
            draw_pointer(ctx, self.pointer, self.down);
        }
        // Every pass, though egui draws twice per repaint request: the
        // recorder keeps one picture per frame slot (asking for every other
        // one missed frames whose slot rounded the same way).
        if self.recorder.is_some() && self.zero.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        // Every frame, for an even video.
        ctx.request_repaint();
    }
}

/// `clip:N`: the Nth clip card on screen, reading left to right, top down
/// (so a script needn't know the clips' names).
fn nth_clip(marks: &HashMap<String, Rect>, name: &str) -> Option<Rect> {
    let n: usize = name.strip_prefix("clip:")?.parse().ok()?;
    let mut cards: Vec<Rect> = marks.iter().filter(|(k, _)| k.starts_with("clip:")).map(|(_, r)| *r).collect();
    cards.sort_by(|a, b| (a.top().round(), a.left()).partial_cmp(&(b.top().round(), b.left())).unwrap_or(std::cmp::Ordering::Equal));
    cards.get(n.checked_sub(1)?).copied()
}

/// The one spot whose name starts with `name` ("clip:6 kills" for "clip:6
/// kills (1 headshot) on Nuke · 00:38:21", "button:Stop clipping" for its
/// tooltip with the key).
fn by_prefix(marks: &HashMap<String, Rect>, name: &str) -> Option<Rect> {
    let mut found = marks.iter().filter(|(k, _)| k.starts_with(name));
    let first = found.next()?;
    found.next().is_none().then(|| *first.1)
}

fn ease(t: f64) -> f64 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// Eased in and out, but without `ease`'s burst in the middle (at most about
/// 1.6 times the average speed, not 3): a long pull reads as a steady hand.
fn ease_gentle(t: f64) -> f64 {
    (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
}

fn glide(from: Pos2, to: Pos2, t: f64, secs: f64) -> Pos2 {
    let k = if secs <= 0.0 { 1.0 } else { ease((t / secs).clamp(0.0, 1.0)) } as f32;
    from + (to - from) * k
}

fn parse_key(s: &str) -> (Key, Modifiers) {
    let mut mods = Modifiers::NONE;
    let mut key = None;
    for part in s.to_lowercase().split('+') {
        match part {
            "cmd" | "command" => mods |= Modifiers::COMMAND,
            "ctrl" => mods |= Modifiers::CTRL,
            "alt" | "option" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            k => key = Key::from_name(k).or_else(|| Key::from_name(&capitalised(k))),
        }
    }
    (key.unwrap_or_else(|| fail(&format!("not a key: \"{s}\""))), mods)
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The macOS window buttons, drawn: the screenshots are only of what the app
/// paints, and the real ones belong to the system. Centred over the rail like
/// `shell::place_window_buttons` does.
fn draw_window_buttons(ctx: &Context) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Debug, Id::new("demo_window_buttons")));
    let (d, gap) = (12.0, 8.0);
    let left = (crate::shell::RAIL_W - (d * 3.0 + gap * 2.0)) / 2.0;
    for (i, (fill, edge)) in [(0xff5f57, 0xe0443e), (0xfebc2e, 0xdea123), (0x28c840, 0x1aab29)].into_iter().enumerate() {
        let c = pos2(left + d / 2.0 + i as f32 * (d + gap), 26.0);
        let rgb = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
        painter.circle(c, d / 2.0 - 0.25, rgb(fill), egui::Stroke::new(0.5, rgb(edge)));
    }
}

/// The macOS arrow, drawn: white with a dark edge, pressed in a little while
/// the button is down.
fn draw_pointer(ctx: &Context, at: Pos2, down: bool) {
    let layer = egui::LayerId::new(egui::Order::Debug, Id::new("demo_pointer"));
    let painter = ctx.layer_painter(layer);
    let s = if down { 0.92 } else { 1.0 };
    let shape: Vec<Pos2> = [(0.0, 0.0), (0.0, 17.0), (4.6, 12.9), (7.5, 19.5), (10.2, 18.3), (7.3, 11.8), (13.0, 11.8)]
        .iter()
        .map(|&(x, y)| at + vec2(x, y) * s)
        .collect();
    let shadow: Vec<Pos2> = shape.iter().map(|p| *p + vec2(0.0, 1.2)).collect();
    painter.add(egui::Shape::convex_polygon(shadow, Color32::from_black_alpha(70), egui::Stroke::NONE));
    // Not convex (the tail): filled as convex pieces under one outline.
    for piece in [&[0, 1, 2][..], &[0, 2, 5], &[0, 5, 6], &[2, 3, 4, 5]] {
        painter.add(egui::Shape::convex_polygon(piece.iter().map(|&i| shape[i]).collect(), Color32::WHITE, egui::Stroke::NONE));
    }
    let mut outline = shape.clone();
    outline.push(shape[0]);
    painter.add(egui::Shape::line(outline, egui::Stroke::new(1.1, Color32::from_gray(20))));
}

/// The library the demo runs on (`HASTECLIP_LIBRARY`).
fn library() -> PathBuf {
    std::env::var_os("HASTECLIP_LIBRARY").map(PathBuf::from).unwrap_or_else(|| fail("a demo runs on a copy of a library: set HASTECLIP_LIBRARY"))
}

/// Where the prepared clips wait, out of the library's sight.
fn waiting_room() -> PathBuf {
    std::env::temp_dir().join(format!("hasteclip-demo-saves-{}", std::process::id()))
}

/// The clips saving will hand over: moved out of the library now, in order.
fn hide_saves(saves: &[PathBuf]) {
    if saves.is_empty() {
        return;
    }
    let lib = library();
    let room = waiting_room();
    let _ = std::fs::create_dir_all(&room);
    let mut queue = Vec::new();
    for (i, rel) in saves.iter().enumerate() {
        let from = lib.join(rel);
        let held = room.join(format!("{i}"));
        std::fs::rename(&from, &held).unwrap_or_else(|e| fail(&format!("can't set aside {}: {e}", from.display())));
        queue.push((held, rel.clone()));
    }
    let _ = SAVES.set(std::sync::Mutex::new(queue.into()));
}

/// Clips saving handed over: where they are now, where they waited, and
/// their place in the library, for [`take_back_saves`].
static HANDED: std::sync::Mutex<Vec<(PathBuf, PathBuf, PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// Take the clips saving handed over back out of the library (they wait to be
/// saved again, and go home when the demo ends).
pub fn take_back_saves() {
    let handed = std::mem::take(&mut *HANDED.lock().unwrap());
    let Some(queue) = SAVES.get() else { return };
    for (to, held, rel) in handed.into_iter().rev() {
        if std::fs::rename(&to, &held).is_ok() {
            queue.lock().unwrap().push_front((held, rel));
        }
    }
}

static SAVES: OnceLock<std::sync::Mutex<std::collections::VecDeque<(PathBuf, PathBuf)>>> = OnceLock::new();

/// The capture service while a demo runs: says it's clipping when asked to
/// start, and on save hands over the next prepared clip as a just-saved one
/// (a new `clip_<now>` name that keeps what came after it, like " - Ace on
/// Ancient", so it lands in Today under its title). Nothing is recorded.
pub(crate) fn capture_thread(rx: mpsc::Receiver<crate::service::Cmd>, tx: mpsc::Sender<crate::service::Evt>) -> std::thread::JoinHandle<()> {
    use crate::service::{Cmd, Evt};
    std::thread::spawn(move || {
        let mut running = capture::Running::default();
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Start(mode, _) => {
                    running = running.with(mode, true);
                    let _ = tx.send(Evt::State(running));
                }
                Cmd::SaveClip(dir, details) => {
                    let next = SAVES.get().and_then(|q| q.lock().ok()?.pop_front());
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        // About as long as a real save takes.
                        std::thread::sleep(std::time::Duration::from_millis(1300));
                        let Some((held, rel)) = next else {
                            let _ = tx.send(Evt::Error("The demo has no more clips to save (add them to \"saves\")".into()));
                            return;
                        };
                        let stem = rel.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        // What follows the clip's own timestamp: " - Ace on Ancient".
                        let rest = stem.split_once(" - ").map(|(_, r)| format!(" - {r}")).unwrap_or_default();
                        let ext = rel.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
                        let name = format!("clip_{}{rest}.{ext}", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"));
                        // The game it was played in, as a game in the foreground
                        // would have set it: the clip's own folder in the library.
                        let dir = match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                            Some(game) => library().join(game),
                            None => dir,
                        };
                        let _ = std::fs::create_dir_all(&dir);
                        let to = dir.join(name);
                        if let Err(e) = std::fs::rename(&held, &to) {
                            let _ = tx.send(Evt::Error(format!("demo: couldn't hand over a clip: {e}")));
                            return;
                        }
                        HANDED.lock().unwrap().push((to.clone(), held, rel));
                        let _ = tx.send(Evt::Saved(crate::service::landed(to, details)));
                    });
                }
                Cmd::Stop(mode, ..) => {
                    running = running.with(mode, false);
                    let _ = tx.send(Evt::State(running));
                }
                Cmd::StopAndAck(ack) => {
                    let _ = ack.send(());
                }
                Cmd::Quit => break,
                Cmd::UpdateVideo(_) | Cmd::ReplaySeconds(_) | Cmd::SetMic(..) | Cmd::SetBlend(..) | Cmd::Screenshot(..) => {}
            }
        }
    })
}

fn fail(msg: &str) -> ! {
    eprintln!("demo: {msg}");
    std::process::exit(2)
}

/// Frames to ffmpeg, on their own thread: raw RGBA in, H.264 out. Each frame
/// is written once per 1/fps it covers, so a slow frame shows as a held frame,
/// never as the video speeding up.
struct Recorder {
    tx: Option<mpsc::SyncSender<(std::sync::Arc<egui::ColorImage>, f64)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    fn new(out: PathBuf, fps: u32) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(std::sync::Arc<egui::ColorImage>, f64)>(8);
        let thread = std::thread::spawn(move || {
            let mut ffmpeg: Option<(std::process::Child, [usize; 2])> = None;
            let mut written = 0u64;
            let mut fresh = 0u64;
            let mut extra = 0u64;
            let mut held: Option<std::sync::Arc<egui::ColorImage>> = None;
            for (image, at) in rx {
                let size = image.size;
                let (child, want) = ffmpeg.get_or_insert_with(|| {
                    eprintln!("demo: recording {}×{} at {fps} fps", size[0], size[1]);
                    let child = media::ffmpeg()
                        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
                        .arg(format!("{}x{}", size[0], size[1]))
                        .args(["-r", &fps.to_string(), "-i", "-", "-c:v", "libx264", "-preset", "fast", "-crf", "16", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
                        .arg(&out)
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                        .unwrap_or_else(|e| fail(&format!("can't start ffmpeg: {e}")));
                    (child, size)
                });
                if image.size != *want {
                    continue; // the window was resized mid-recording; keep the first size
                }
                let stdin = child.stdin.as_mut().expect("piped");
                // The frame slot this picture belongs in. One that's already
                // filled (the app drew twice in it: egui repaints twice per
                // request, or the display runs at 120 Hz) keeps its picture.
                let due = (at * f64::from(fps)).round() as u64;
                if due < written {
                    extra += 1;
                    continue;
                }
                // Fill the frames up to this one's time with the last picture.
                while written < due {
                    let fill = held.as_ref().unwrap_or(&image);
                    if stdin.write_all(fill.as_raw()).is_err() {
                        return;
                    }
                    written += 1;
                }
                if stdin.write_all(image.as_raw()).is_err() {
                    return;
                }
                written += 1;
                fresh += 1;
                held = Some(image);
            }
            if let Some((mut child, _)) = ffmpeg {
                drop(child.stdin.take());
                let _ = child.wait();
                eprintln!("demo: {written} frames ({:.1} s), {fresh} new from the app, {} held over ({extra} extra pictures left out)", written as f64 / f64::from(fps), written - fresh);
            }
        });
        Self { tx: Some(tx), thread: Some(thread) }
    }

    fn frame(&mut self, image: std::sync::Arc<egui::ColorImage>, at: f64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((image, at));
        }
    }

    fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_read() {
        let s: Script = serde_json::from_str(
            r#"{ "out": "a.mp4", "steps": [
                { "wait": 0.5 }, { "move": "clip:Ace", "secs": 1 }, { "move": [10, 20] }, { "click": true },
                { "drag": "trim:in", "by": [120, 0] }, { "drag": "clip:A", "to": "filter:Best" },
                { "key": "cmd+s" }, { "type": "hi" }, { "scroll": 300 }, { "seek": "marker:1", "before": 1 }, { "pointer": false },
                { "play": true }
            ] }"#,
        )
        .unwrap();
        assert_eq!(s.steps.len(), 12);
        assert!(matches!(s.steps[11], Step::Play { play: true }));
        assert!(matches!(s.steps[0], Step::Wait { .. }));
        assert!(matches!(s.steps[3], Step::Click { .. }));
        assert!(matches!(s.steps[4], Step::Drag { .. }));
        assert!(matches!(s.steps[9], Step::Seek { .. }));
    }

    #[test]
    fn keys_parse() {
        assert_eq!(parse_key("cmd+s"), (Key::S, Modifiers::COMMAND));
        assert_eq!(parse_key("space"), (Key::Space, Modifiers::NONE));
        assert_eq!(parse_key("alt+f10").0, Key::F10);
    }
}
