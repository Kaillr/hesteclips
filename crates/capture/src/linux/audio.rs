//! Sound on Linux, all through PipeWire: mics, everything the computer plays
//! (the desktop), and single apps.
//!
//! Each source is a capture stream of ours, 48 kHz stereo float (PipeWire
//! converts whatever the device or app plays):
//!
//! - **Mic**: connected to its device by the session manager (WirePlumber).
//! - **Desktop and apps**: connected by us, the way a patchbay would — every
//!   app's playback stream is linked straight into ours, and PipeWire adds up
//!   what arrives. The desktop is every app except HesteClips itself (its
//!   clip previews and sounds), and, when asked, except the apps that are
//!   sources of their own; an app source is the playback of programs by that
//!   name. Like WASAPI's process loopback on Windows, and unlike recording
//!   the speakers' monitor, it hears apps whatever device they play on.
//!
//! A watcher on PipeWire's registry links and unlinks as apps open and close
//! their sound, so an app source starts by itself when its app does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::types::ObjectType;

use crate::Device;
use crate::audio::AudioDevices;
use crate::mixer::{Channel, RATE, SourceFeed, SourceStatus};
use crate::sources::{AudioSource, SourceKind};

/// What we know about one PipeWire node.
#[derive(Debug, Clone, Default)]
struct Node {
    class: String,
    /// `node.name`: a device's stable id.
    name: String,
    /// `node.description`: a device's name for people.
    description: String,
    /// For a playback stream: the program, by the id app sources use.
    app: Option<App>,
    pid: Option<u32>,
}

/// A program playing sound.
#[derive(Debug, Clone, PartialEq, Eq)]
struct App {
    /// What an app source names it by: the program's file name, or for one
    /// that runs inside another (Wine, Proton) its own name — `Game.exe`
    /// rather than `wine64-preloader`.
    id: String,
    /// For people: the name it gives itself.
    name: String,
}

impl App {
    fn of(props: &spa::utils::dict::DictRef) -> Option<Self> {
        let binary = props.get(*pw::keys::APP_PROCESS_BINARY).map(str::trim).filter(|s| !s.is_empty());
        let name = props.get(*pw::keys::APP_NAME).map(str::trim).filter(|s| !s.is_empty());
        let hosted = binary.is_none_or(|b| {
            let b = b.to_ascii_lowercase();
            b.contains("wine") || b.contains("preloader") || b == "pressure-vessel-adverb" || b == "python3"
        });
        let id = if hosted { name.or(binary)? } else { binary? };
        Some(Self { id: id.to_owned(), name: name.unwrap_or(id).to_owned() })
    }

    fn is(&self, id: &str) -> bool {
        self.id.eq_ignore_ascii_case(id) || self.name.eq_ignore_ascii_case(id)
    }
}

#[derive(Debug, Clone)]
struct Port {
    node: u32,
    output: bool,
    /// `audio.channel`: FL, FR, MONO, FC, …
    channel: String,
}

/// The registry as last seen.
#[derive(Default)]
struct Graph {
    nodes: HashMap<u32, Node>,
    ports: HashMap<u32, Port>,
    /// `default.audio.source` / `default.audio.sink`, by node name.
    default_source: Option<String>,
    default_sink: Option<String>,
    changed: bool,
}

const PLAYBACK: &str = "Stream/Output/Audio";

impl Graph {
    fn add(&mut self, global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
        let Some(props) = global.props else { return };
        match global.type_ {
            ObjectType::Node => {
                let get = |k: &str| props.get(k).unwrap_or_default().to_owned();
                let node = Node {
                    class: get(*pw::keys::MEDIA_CLASS),
                    name: get(*pw::keys::NODE_NAME),
                    description: props.get(*pw::keys::NODE_DESCRIPTION).or(props.get(*pw::keys::NODE_NICK)).unwrap_or_default().to_owned(),
                    app: App::of(props),
                    pid: props.get(*pw::keys::APP_PROCESS_ID).and_then(|p| p.parse().ok()),
                };
                self.nodes.insert(global.id, node);
            }
            ObjectType::Port => {
                let Some(node) = props.get(*pw::keys::NODE_ID).and_then(|n| n.parse().ok()) else { return };
                let output = props.get(*pw::keys::PORT_DIRECTION) == Some("out");
                if props.get(*pw::keys::PORT_MONITOR) == Some("true") {
                    return;
                }
                let channel = props.get(*pw::keys::AUDIO_CHANNEL).unwrap_or_default().to_owned();
                self.ports.insert(global.id, Port { node, output, channel });
            }
            _ => return,
        }
        self.changed = true;
    }

    fn remove(&mut self, id: u32) {
        if self.nodes.remove(&id).is_some() || self.ports.remove(&id).is_some() {
            self.changed = true;
        }
    }

    /// Playback streams of other programs.
    fn playback(&self) -> impl Iterator<Item = (u32, &Node)> {
        let own = std::process::id();
        self.nodes.iter().filter(move |(_, n)| n.class == PLAYBACK && n.pid != Some(own)).map(|(id, n)| (*id, n))
    }
}

/// The one of our two channels (0 = left, 1 = right) each of a stream's
/// channels goes into: sides and rears to their side, centre and mono to both
/// (PipeWire sums what's linked into a port), the subwoofer nowhere.
fn route(channel: &str, index: usize) -> &'static [usize] {
    match channel {
        "FL" | "SL" | "RL" | "FLC" | "TFL" | "TRL" | "TSL" | "FLW" | "RLC" | "LLFE" => &[0],
        "FR" | "SR" | "RR" | "FRC" | "TFR" | "TRR" | "TSR" | "FRW" | "RRC" | "RLFE" => &[1],
        "LFE" | "LFE2" => &[],
        "MONO" | "FC" | "RC" | "TC" | "TFC" | "TRC" | "BC" => &[0, 1],
        // Unpositioned (AUX0, AUX1…): alternate.
        _ if index % 2 == 0 => &[0],
        _ => &[1],
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Desktop, app and mic capture for a set of sources.
pub(crate) struct SystemAudio {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl SystemAudio {
    pub(crate) fn start(sources: Vec<(AudioSource, Arc<SourceFeed>, Arc<Channel>)>) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>>>();
        let thread = thread::Builder::new().name("pipewire-audio".into()).spawn(move || {
            if let Err(e) = run(sources, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        let stop = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("PipeWire didn't answer — is it running?"))??;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub(crate) fn stop(self) {
        drop(self);
    }
}

impl Drop for SystemAudio {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One source's stream and the links into it.
struct Capture {
    source: AudioSource,
    channel: Arc<Channel>,
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    /// Ours, by (their output port, our input port).
    links: HashMap<(u32, u32), pw::link::Link>,
}

fn run(sources: Vec<(AudioSource, Arc<SourceFeed>, Arc<Channel>)>, ready: &mpsc::Sender<Result<pw::channel::Sender<()>>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).map_err(|e| anyhow!("can't connect to PipeWire: {e}"))?;
    let registry = core.get_registry_rc()?;

    let graph = Rc::new(RefCell::new(Graph::default()));
    let _registry_listener = {
        let (g1, g2) = (graph.clone(), graph.clone());
        registry
            .add_listener_local()
            .global(move |global| g1.borrow_mut().add(global))
            .global_remove(move |id| g2.borrow_mut().remove(id))
            .register()
    };

    // App sources' apps, which a desktop source may leave out.
    let app_ids: Vec<String> = sources
        .iter()
        .filter_map(|(s, ..)| match &s.kind {
            SourceKind::App { bundle_id } => Some(bundle_id.clone()),
            _ => None,
        })
        .collect();

    let mut captures = Vec::new();
    for (source, feed, channel) in sources {
        match open_stream(&core, &source, feed) {
            Ok((stream, listener)) => {
                channel.set_status(match source.kind {
                    SourceKind::App { .. } => SourceStatus::WaitingForApp,
                    _ => SourceStatus::Live,
                });
                captures.push(Capture { source, channel, stream, _listener: listener, links: HashMap::new() });
            }
            Err(e) => {
                eprintln!("audio source \"{}\": {e:#}", source.name);
                channel.set_status(SourceStatus::Unavailable);
            }
        }
    }
    let captures = Rc::new(RefCell::new(captures));

    // Relink a few times a second, when the registry changed.
    let timer = {
        let (graph, captures, core) = (graph.clone(), captures.clone(), core.clone());
        mainloop.loop_().add_timer(move |_| {
            let mut g = graph.borrow_mut();
            // Our own streams' ports show up once they're connected: keep
            // checking until every stream is linked.
            let unlinked = captures.borrow().iter().any(|c| c.stream.node_id() == pw::constants::ID_ANY);
            if !g.changed && !unlinked {
                return;
            }
            g.changed = false;
            for c in captures.borrow_mut().iter_mut() {
                relink(&core, &g, c, &app_ids);
            }
        })
    };
    timer.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(250))).into_result()?;

    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let quit = mainloop.downgrade();
    let _stop = stop_rx.attach(mainloop.loop_(), move |()| {
        if let Some(l) = quit.upgrade() {
            l.quit();
        }
    });
    let _ = ready.send(Ok(stop_tx));
    mainloop.run();

    for c in captures.borrow_mut().iter_mut() {
        for (_, link) in c.links.drain() {
            let _ = core.destroy_object(link);
        }
        let _ = c.stream.disconnect();
        c.channel.set_status(SourceStatus::Off);
    }
    Ok(())
}

/// Our capture stream for `source`, feeding `feed`.
fn open_stream(core: &pw::core::CoreRc, source: &AudioSource, feed: Arc<SourceFeed>) -> Result<(pw::stream::StreamRc, pw::stream::StreamListener<()>)> {
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_NAME => format!("hesteclips.{}", source.id),
        *pw::keys::NODE_DESCRIPTION => format!("HesteClips: {}", source.name),
        *pw::keys::APP_NAME => "HesteClips",
        // Small packets, so the meters move smoothly.
        *pw::keys::NODE_LATENCY => format!("{}/{RATE}", RATE / 100),
    };
    let device = match &source.kind {
        SourceKind::Microphone { device } => Some(device.clone()),
        _ => None,
    };
    if let Some(device) = &device {
        props.insert("target.object", device.as_str());
    }
    let stream = pw::stream::StreamRc::new(core.clone(), &format!("hesteclips-{}", source.id), props)?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(offset..offset + size) else { return };
            let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let frames = samples.len() / 2;
            if frames == 0 {
                return;
            }
            // When the first of these samples was captured: the graph's cycle
            // started at `now`, after they came in, plus however long the
            // device holds them.
            let start = stream
                .time()
                .ok()
                .filter(|t| t.now() > 0)
                .map(|t| {
                    let rate = t.rate();
                    let delay = if rate.denom > 0 { t.delay() as f64 * rate.num as f64 / rate.denom as f64 } else { 0.0 };
                    t.now() as f64 / 1e9 - delay.clamp(0.0, 0.5)
                })
                .unwrap_or_else(super::host_now)
                - frames as f64 / RATE as f64;
            feed.push(start, &samples, 2);
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(2);
    let mut position = [0u32; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let pod = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map_err(|_| anyhow!("bad audio format"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad audio format"))?];
    // Mics are connected by the session manager; desktop and apps by us.
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
    if device.is_some() {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    stream.connect(spa::utils::Direction::Input, None, flags, &mut params)?;
    Ok((stream, listener))
}

/// Make `c`'s links match what it should hear now, and update its status.
fn relink(core: &pw::core::CoreRc, g: &Graph, c: &mut Capture, app_ids: &[String]) {
    // A mic: only its status (the session manager links it).
    if let SourceKind::Microphone { device } = &c.source.kind {
        let present = g.nodes.values().any(|n| n.name == *device && n.class.starts_with("Audio/Source"));
        c.channel.set_status(if present { SourceStatus::Live } else { SourceStatus::Unavailable });
        return;
    }
    let ours = c.stream.node_id();
    if ours == pw::constants::ID_ANY {
        return;
    }
    // Our two input ports, left and right.
    let mut inputs = [None, None];
    for (id, p) in &g.ports {
        if p.node == ours && !p.output {
            match p.channel.as_str() {
                "FL" => inputs[0] = Some(*id),
                "FR" => inputs[1] = Some(*id),
                _ => {}
            }
        }
    }
    let wanted_node = |n: &Node| -> bool {
        // A stream that doesn't say what plays it can't be an app source's.
        let Some(app) = &n.app else { return matches!(c.source.kind, SourceKind::Desktop { .. }) };
        match &c.source.kind {
            SourceKind::App { bundle_id } => app.is(bundle_id),
            SourceKind::Desktop { exclude_app_sources } => !*exclude_app_sources || !app_ids.iter().any(|a| app.is(a)),
            SourceKind::Microphone { .. } => false,
        }
    };
    let sources: Vec<u32> = g.playback().filter(|(_, n)| wanted_node(n)).map(|(id, _)| id).collect();
    if let SourceKind::App { .. } = c.source.kind {
        c.channel.set_status(if sources.is_empty() { SourceStatus::WaitingForApp } else { SourceStatus::Live });
    }
    let mut want = HashSet::new();
    for node in &sources {
        let mut outs: Vec<(u32, &Port)> = g.ports.iter().filter(|(_, p)| p.node == *node && p.output).map(|(id, p)| (*id, p)).collect();
        outs.sort_by_key(|(id, _)| *id);
        for (index, (port, p)) in outs.into_iter().enumerate() {
            for &side in route(&p.channel, index) {
                if let Some(input) = inputs[side] {
                    want.insert((port, input));
                }
            }
        }
    }
    // Gone: links whose ports went away are gone already; unwanted ones are removed.
    let stale: Vec<(u32, u32)> = c.links.keys().filter(|k| !want.contains(k)).copied().collect();
    for key in stale {
        if let Some(link) = c.links.remove(&key) {
            if g.ports.contains_key(&key.0) && g.ports.contains_key(&key.1) {
                let _ = core.destroy_object(link);
            }
        }
    }
    for (out_port, in_port) in want {
        if c.links.contains_key(&(out_port, in_port)) {
            continue;
        }
        let Some(out_node) = g.ports.get(&out_port).map(|p| p.node) else { continue };
        let link = core.create_object::<pw::link::Link>(
            "link-factory",
            &pw::properties::properties! {
                "link.output.node" => out_node.to_string(),
                "link.output.port" => out_port.to_string(),
                "link.input.node" => ours.to_string(),
                "link.input.port" => in_port.to_string(),
                // Gone with us if we go (a crash included).
                "object.linger" => "false",
            },
        );
        match link {
            Ok(link) => {
                c.links.insert((out_port, in_port), link);
            }
            Err(e) => eprintln!("audio source \"{}\": couldn't link: {e}", c.source.name),
        }
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// A look at the registry: every node and port, and the default devices.
fn snapshot() -> Result<Graph> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let graph = Rc::new(RefCell::new(Graph::default()));
    // The "default" metadata says which devices are the defaults.
    let metadata: Rc<RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> = Rc::new(RefCell::new(None));
    let _listener = {
        let (g1, meta, reg) = (graph.clone(), metadata.clone(), registry.downgrade());
        registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Metadata {
                    if global.props.and_then(|p| p.get("metadata.name")) != Some("default") {
                        return;
                    }
                    let Some(reg) = reg.upgrade() else { return };
                    let Ok(m) = reg.bind::<pw::metadata::Metadata, _>(global) else { return };
                    let g2 = g1.clone();
                    let listener = m
                        .add_listener_local()
                        .property(move |_, key, _, value| {
                            let name = value.and_then(json_name);
                            match key {
                                Some("default.audio.source") => g2.borrow_mut().default_source = name,
                                Some("default.audio.sink") => g2.borrow_mut().default_sink = name,
                                _ => {}
                            }
                            0
                        })
                        .register();
                    *meta.borrow_mut() = Some((m, listener));
                } else {
                    g1.borrow_mut().add(global);
                }
            })
            .register()
    };
    // Once for the registry, once more for the metadata it led to.
    roundtrip(&mainloop, &core)?;
    roundtrip(&mainloop, &core)?;
    // The listeners still hold the graph: take what it found.
    Ok(std::mem::take(&mut *graph.borrow_mut()))
}

/// `{"name":"alsa_input.usb-…"}` → the name.
fn json_name(value: &str) -> Option<String> {
    let rest = &value[value.find("\"name\"")? + 6..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_owned())
}

/// Wait until PipeWire has answered everything asked so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Result<()> {
    let done = Rc::new(std::cell::Cell::new(false));
    let pending = core.sync(0)?;
    let (done2, quit) = (done.clone(), mainloop.downgrade());
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done2.set(true);
                if let Some(l) = quit.upgrade() {
                    l.quit();
                }
            }
        })
        .register();
    // A timer in case PipeWire never answers, so the UI can't hang.
    let quit = mainloop.downgrade();
    let timeout = mainloop.loop_().add_timer(move |_| {
        if let Some(l) = quit.upgrade() {
            l.quit();
        }
    });
    timeout.update_timer(Some(Duration::from_secs(2)), None).into_result()?;
    mainloop.run();
    if done.get() { Ok(()) } else { Err(anyhow!("PipeWire didn't answer")) }
}

/// Mics and outputs, by PipeWire node name, with the defaults.
pub(crate) fn list_devices() -> AudioDevices {
    // A snapshot takes a moment; the UI asks for the list now and then.
    static CACHE: Mutex<Option<(std::time::Instant, AudioDevices)>> = Mutex::new(None);
    if let Some((at, devices)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(1) {
            return devices.clone();
        }
    }
    let g = match snapshot() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("PipeWire: {e:#}");
            return AudioDevices::default();
        }
    };
    let devices_of = |class: &str| {
        let mut list: Vec<Device> = g
            .nodes
            .values()
            .filter(|n| n.class.starts_with(class) && !n.name.is_empty())
            .map(|n| Device { id: n.name.clone(), name: if n.description.is_empty() { n.name.clone() } else { n.description.clone() } })
            .collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list.dedup_by(|a, b| a.id == b.id);
        list
    };
    let inputs = devices_of("Audio/Source");
    let outputs = devices_of("Audio/Sink");
    let default_input = g.default_source.clone().filter(|d| inputs.iter().any(|i| i.id == *d)).or_else(|| inputs.first().map(|i| i.id.clone()));
    let default_output = g.default_sink.clone().filter(|d| outputs.iter().any(|o| o.id == *d)).or_else(|| outputs.first().map(|o| o.id.clone()));
    let devices = AudioDevices { inputs, outputs, default_input, default_output };
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), devices.clone()));
    devices
}

/// Programs playing sound right now (other than us), sorted by name.
pub(crate) fn list_apps() -> Vec<Device> {
    let Ok(g) = snapshot() else { return Vec::new() };
    let mut apps: Vec<Device> = g.playback().filter_map(|(_, n)| n.app.as_ref()).map(|a| Device { id: a.id.clone(), name: a.name.clone() }).collect();
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surround_folds_into_stereo() {
        assert_eq!(route("FL", 0), &[0]);
        assert_eq!(route("FR", 1), &[1]);
        assert_eq!(route("FC", 2), &[0, 1]);
        assert_eq!(route("MONO", 0), &[0, 1]);
        assert!(route("LFE", 3).is_empty());
        assert_eq!(route("RR", 5), &[1]);
        assert_eq!(route("AUX0", 0), &[0]);
        assert_eq!(route("AUX1", 1), &[1]);
    }

    #[test]
    fn default_device_names() {
        assert_eq!(json_name(r#"{ "name": "alsa_input.pci-0000_00_1f.3.analog-stereo" }"#).as_deref(), Some("alsa_input.pci-0000_00_1f.3.analog-stereo"));
        assert_eq!(json_name("{}"), None);
    }
}
