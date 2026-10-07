//! HesteFiles integration: the account token (kept in the OS keychain), the default
//! clips folder (kept in a small config file), a folders-only browser used both
//! to pick that default and to pick a one-off folder when sharing, and uploads,
//! which run in the background with their progress shown on the clip's card.
//!
//! Network calls run on short-lived threads; results come back over a channel that
//! `poll` drains each frame, so the UI never blocks on the server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use hestefiles::{Account, BaseFolder, Client, UploadProgress};
use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "hesteclips";
const KEYCHAIN_USER: &str = "hestefiles-token";

/// A folder in HesteFiles: a base folder plus a path inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRef {
    pub base_id: String,
    pub base_name: String,
    /// `/`-separated with no leading/trailing slash; `""` = the base folder itself.
    pub path: String,
}

impl FolderRef {
    fn base(base: &BaseFolder) -> Self {
        Self { base_id: base.id.clone(), base_name: base.name.clone(), path: String::new() }
    }

    /// "mikhail's Folder / Videos / Clips"
    pub fn display(&self) -> String {
        self.segments().fold(self.base_name.clone(), |acc, s| format!("{acc} / {s}"))
    }

    fn segments(&self) -> impl Iterator<Item = &str> {
        self.path.split('/').filter(|s| !s.is_empty())
    }

    fn child(&self, name: &str) -> Self {
        let path =
            if self.path.is_empty() { name.to_owned() } else { format!("{}/{name}", self.path) };
        Self { path, ..self.clone() }
    }

    /// The same base folder truncated to its first `depth` path segments.
    fn ancestor(&self, depth: usize) -> Self {
        let path = self.segments().take(depth).collect::<Vec<_>>().join("/");
        Self { path, ..self.clone() }
    }

    fn parent(&self) -> Option<Self> {
        let depth = self.segments().count();
        (depth > 0).then(|| self.ancestor(depth - 1))
    }
}

/// What's persisted between launches (the token lives in the keychain, not here).
#[derive(Serialize, Deserialize)]
struct Config {
    default_folder: Option<FolderRef>,
    /// Upload clips into a folder for their game (inside the folder picked).
    #[serde(default = "yes")]
    game_folders: bool,
    /// Give uploads a public link (copied when done), as last chosen.
    #[serde(default)]
    public_links: bool,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self { default_folder: None, game_folders: true, public_links: false }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hesteclips").join("hestefiles.json"))
}

fn keychain() -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).ok()
}

pub enum Connection {
    Disconnected,
    Connecting,
    Connected(Account),
    Failed(String),
}

enum Evt {
    StoredToken(Option<String>),
    Validated { client: Client, result: Result<Account, String> },
    BaseFolders(Result<Vec<BaseFolder>, String>),
    Dir { at: FolderRef, result: Result<(Vec<String>, bool), String> },
    /// A folder made in the browser: where, its name, and whether it was
    /// there already.
    FolderMade { parent: FolderRef, name: String, result: Result<bool, String> },
    /// Its public link, when one was asked for.
    Uploaded { id: u64, result: Result<Option<String>, String> },
}

/// A clip on its way to HesteFiles.
pub struct Upload {
    id: u64,
    /// The clip in the library.
    pub clip: PathBuf,
    pub to: FolderRef,
    /// 0..=1 as f32 bits, written by the upload thread.
    progress: Arc<AtomicU32>,
    /// The server is joining the chunks (the last few percent).
    merging: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Upload {
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    pub fn merging(&self) -> bool {
        self.merging.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// How an upload ended, for the app to tell the user.
pub enum UploadDone {
    /// `link`: the public link, when one was asked for.
    Uploaded { clip: PathBuf, to: FolderRef, link: Option<String> },
    Failed { clip: PathBuf, error: String },
    Cancelled { clip: PathBuf },
}

/// Folders-only browser state. `at == None` is the top level: the list of base
/// folders (personal + shared).
#[derive(Default)]
pub struct FolderBrowser {
    /// `None` while loading.
    bases: Option<Result<Vec<BaseFolder>, String>>,
    at: Option<FolderRef>,
    /// Subfolders of `at` plus whether it's read-only; `None` while loading.
    listing: Option<Result<(Vec<String>, bool), String>>,
    /// "New folder" is open: the name being typed.
    new_folder: Option<NewFolder>,
}

#[derive(Default)]
struct NewFolder {
    name: String,
    error: Option<String>,
    /// Being made on the server.
    busy: bool,
    focused: bool,
}

pub struct Cloud {
    client: Option<Client>,
    pub connection: Connection,
    /// Text field buffer for pasting a token.
    pub token_input: String,
    pub default_folder: Option<FolderRef>,
    /// Upload clips into a folder for their game, as last chosen.
    pub game_folders: bool,
    /// Give uploads a public link, as last chosen.
    pub public_links: bool,
    pub browser: FolderBrowser,
    pub uploads: Vec<Upload>,
    next_upload: u64,
    finished: Vec<UploadDone>,
    ctx: egui::Context,
    tx: Sender<Evt>,
    rx: Receiver<Evt>,
}

impl Cloud {
    /// Loads the saved default folder and, if a token is in the keychain, starts
    /// validating it in the background.
    pub fn new(ctx: egui::Context) -> Self {
        let config: Config = config_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let mut cloud = Self {
            client: None,
            connection: Connection::Disconnected,
            token_input: String::new(),
            default_folder: config.default_folder,
            game_folders: config.game_folders,
            public_links: config.public_links,
            browser: FolderBrowser::default(),
            uploads: Vec::new(),
            next_upload: 0,
            finished: Vec::new(),
            ctx,
            tx,
            rx,
        };
        cloud.connection = Connection::Connecting;
        // Dev aid: `HESTEFILES_TOKEN=…` connects with that token, skipping the
        // keychain (and its password prompt on every rebuild).
        if let Ok(token) = std::env::var("HESTEFILES_TOKEN") {
            cloud.connect(&token);
            return cloud;
        }
        // Reading the keychain can block on a system permission prompt, so never do
        // it on the UI thread.
        cloud.spawn(|| Evt::StoredToken(keychain().and_then(|k| k.get_password().ok())));
        cloud
    }

    /// Where the connected account's profile picture is.
    pub fn profile_picture(&self) -> Option<String> {
        let (Connection::Connected(account), Some(client)) = (&self.connection, &self.client) else { return None };
        let href = account.user_info.profile_picture()?;
        Some(client.url(href))
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.connection, Connection::Connected(_))
    }

    /// Validate `token`; on success it's saved to the keychain.
    pub fn connect(&mut self, token: &str) {
        let token = token.trim();
        if token.is_empty() {
            return;
        }
        self.connection = Connection::Connecting;
        let client = Client::new(hestefiles::DEFAULT_DOMAIN, token);
        self.spawn(move || {
            let result = client.validate_token().map_err(|e| e.to_string());
            Evt::Validated { client, result }
        });
    }

    /// Forget the token. The default folder is kept so reconnecting with a fresh
    /// token (they expire) doesn't lose it.
    pub fn disconnect(&mut self) {
        if let Some(k) = keychain() {
            let _ = k.delete_credential();
        }
        self.client = None;
        self.connection = Connection::Disconnected;
    }

    pub fn set_default_folder(&mut self, folder: Option<FolderRef>) {
        self.default_folder = folder;
        self.save_config();
    }

    pub fn set_game_folders(&mut self, on: bool) {
        if self.game_folders != on {
            self.game_folders = on;
            self.save_config();
        }
    }

    pub fn set_public_links(&mut self, on: bool) {
        if self.public_links != on {
            self.public_links = on;
            self.save_config();
        }
    }

    fn save_config(&self) {
        let config =
            Config { default_folder: self.default_folder.clone(), game_folders: self.game_folders, public_links: self.public_links };
        if let (Some(path), Ok(json)) = (config_path(), serde_json::to_vec_pretty(&config)) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, json);
        }
    }

    /// Drain finished network calls. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok(evt) = self.rx.try_recv() {
            match evt {
                Evt::StoredToken(Some(token)) => self.connect(&token),
                Evt::StoredToken(None) => self.connection = Connection::Disconnected,
                Evt::Validated { client, result } => match result {
                    Ok(account) => {
                        let from_env = std::env::var("HESTEFILES_TOKEN").is_ok_and(|t| t.trim() == client.token());
                        if let (false, Some(k)) = (from_env, keychain()) {
                            let _ = k.set_password(client.token());
                        }
                        self.client = Some(client);
                        self.token_input.clear();
                        self.connection = Connection::Connected(account);
                    }
                    Err(e) => self.connection = Connection::Failed(e),
                },
                Evt::BaseFolders(result) => self.browser.bases = Some(result),
                // Ignore replies for folders the user has already navigated away from.
                Evt::Dir { at, result } if self.browser.at.as_ref() == Some(&at) => {
                    self.browser.listing = Some(result);
                }
                Evt::Dir { .. } => {}
                Evt::FolderMade { parent, name, result } => {
                    if self.browser.at.as_ref() != Some(&parent) {
                        continue;
                    }
                    match result {
                        // Open it: it's what you made it for.
                        Ok(false) => self.navigate(Some(parent.child(&name))),
                        Ok(true) => {
                            if let Some(n) = &mut self.browser.new_folder {
                                n.busy = false;
                                n.error = Some("A folder with that name is already here.".into());
                            }
                        }
                        Err(e) => {
                            if let Some(n) = &mut self.browser.new_folder {
                                n.busy = false;
                                n.error = Some(e);
                            }
                        }
                    }
                }
                Evt::Uploaded { id, result } => {
                    let Some(i) = self.uploads.iter().position(|u| u.id == id) else { continue };
                    let up = self.uploads.remove(i);
                    self.finished.push(match result {
                        Ok(link) => UploadDone::Uploaded { clip: up.clip, to: up.to, link },
                        Err(_) if up.cancel.load(Ordering::Relaxed) => UploadDone::Cancelled { clip: up.clip },
                        Err(error) => UploadDone::Failed { clip: up.clip, error },
                    });
                }
            }
        }
    }

    /// Start uploading `clip` into `to` in the background. Its progress shows on
    /// the clip's card; how it ended comes back from [`Cloud::take_finished`].
    /// Upload `clip` to `to`, or to a folder named `subfolder` in it (made if
    /// it isn't there; a game's). With `public`, it gets a public link.
    pub fn upload(&mut self, clip: PathBuf, to: FolderRef, subfolder: Option<String>, public: bool) {
        let Some(client) = self.client.clone() else { return };
        let id = self.next_upload;
        self.next_upload += 1;
        let (progress, merging, cancel) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let parent = to;
        let to = subfolder.as_deref().map_or(parent.clone(), |name| parent.child(name));
        self.uploads.push(Upload { id, clip: clip.clone(), to: to.clone(), progress: progress.clone(), merging: merging.clone(), cancel: cancel.clone() });
        let ctx = self.ctx.clone();
        self.spawn(move || {
            let name = clip.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let made = match &subfolder {
                Some(folder) => client.new_folder(&parent.base_id, &parent.path, folder).map(|_| ()),
                None => Ok(()),
            };
            let result = made
                .and_then(|()| client.upload(&clip, &to.base_id, &to.path, &name, public, &cancel, |p| {
                    // Sending is ~95% of the bar; the server's merge fills the rest.
                    let f = match p {
                        UploadProgress::Sending { sent, total } => 0.95 * sent as f32 / total.max(1) as f32,
                        UploadProgress::Merging { percent } => {
                            merging.store(true, Ordering::Relaxed);
                            0.95 + 0.05 * percent as f32 / 100.0
                        }
                    };
                    progress.store(f.to_bits(), Ordering::Relaxed);
                    ctx.request_repaint();
                }))
                .map_err(|e| e.to_string());
            Evt::Uploaded { id, result }
        });
    }

    pub fn upload_for(&self, clip: &std::path::Path) -> Option<&Upload> {
        self.uploads.iter().find(|u| u.clip == clip)
    }

    /// Uploads that ended since the last call.
    pub fn take_finished(&mut self) -> Vec<UploadDone> {
        std::mem::take(&mut self.finished)
    }

    /// Reset the browser and open it at `start` (or the top level).
    pub fn open_browser(&mut self, start: Option<FolderRef>) {
        self.browser = FolderBrowser::default();
        if let Some(client) = self.client.clone() {
            self.spawn(move || {
                Evt::BaseFolders(client.base_folders().map_err(|e| e.to_string()))
            });
        }
        self.navigate(start);
    }

    fn navigate(&mut self, to: Option<FolderRef>) {
        self.browser.at = to.clone();
        self.browser.listing = None;
        self.browser.new_folder = None;
        let (Some(at), Some(client)) = (to, self.client.clone()) else { return };
        self.spawn(move || {
            let result = client
                .directory(&at.base_id, &at.path, hestefiles::Include::Folders)
                .map(|dir| {
                    let mut names: Vec<String> = dir.folders.into_iter().map(|f| f.name).collect();
                    names.sort_by_key(|n| n.to_lowercase());
                    (names, dir.read_only)
                })
                .map_err(|e| e.to_string());
            Evt::Dir { at, result }
        });
    }

    /// The folder the browser is showing, if it's one you can save into.
    pub fn browser_selection(&self) -> Option<FolderRef> {
        match (&self.browser.at, &self.browser.listing) {
            (Some(at), Some(Ok((_, false)))) => Some(at.clone()),
            _ => None,
        }
    }

    /// Breadcrumbs + a clickable list of folders. Files are never shown.
    pub fn browser_ui(&mut self, ui: &mut egui::Ui) {
        let mut go: Option<Option<FolderRef>> = None;

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if ui.link("☁ HesteFiles").clicked() {
                go = Some(None);
            }
            if let Some(at) = &self.browser.at {
                ui.weak("›");
                if ui.link(&at.base_name).clicked() {
                    go = Some(Some(at.ancestor(0)));
                }
                for (i, seg) in at.segments().enumerate() {
                    ui.weak("›");
                    if ui.link(seg).clicked() {
                        go = Some(Some(at.ancestor(i + 1)));
                    }
                }
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
            match &self.browser.at {
                None => match &self.browser.bases {
                    None => {
                        ui.spinner();
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    Some(Ok(bases)) => {
                        for base in bases {
                            let tag = match (base.owner, base.read_only) {
                                (true, _) => "",
                                (false, true) => "  · shared, read-only",
                                (false, false) => "  · shared",
                            };
                            let label = format!("{} {}{tag}", base_icon(base), base.name);
                            if ui.selectable_label(false, label).clicked() {
                                go = Some(Some(FolderRef::base(base)));
                            }
                        }
                    }
                },
                Some(at) => {
                    if let Some(parent) = at.parent() {
                        if ui.selectable_label(false, "⬆ ..").clicked() {
                            go = Some(Some(parent));
                        }
                    } else if ui.selectable_label(false, "⬆ All folders").clicked() {
                        go = Some(None);
                    }
                    match &self.browser.listing {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        Some(Ok((names, _))) if names.is_empty() => {
                            ui.weak("No subfolders");
                        }
                        Some(Ok((names, _))) => {
                            for name in names {
                                if ui.selectable_label(false, format!("📁 {name}")).clicked() {
                                    go = Some(Some(at.child(name)));
                                }
                            }
                        }
                    }
                }
            }
        });

        if let Some(Ok((_, true))) = &self.browser.listing {
            ui.colored_label(ui.visuals().warn_fg_color, "Read-only — pick a folder you can write to.");
        }
        if let (Some(at), Some(Ok((_, false)))) = (self.browser.at.clone(), &self.browser.listing) {
            self.new_folder_ui(ui, &at);
        }

        if let Some(to) = go {
            self.navigate(to);
        }
    }

    /// "New folder" under the list: a name field, made in `at` on Create (and
    /// opened, so it's picked).
    fn new_folder_ui(&mut self, ui: &mut egui::Ui, at: &FolderRef) {
        ui.add_space(4.0);
        let Some(n) = &mut self.browser.new_folder else {
            if ui.button("➕  New folder").clicked() {
                self.browser.new_folder = Some(NewFolder::default());
            }
            return;
        };
        let mut create = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let field = ui.add_enabled(!n.busy, egui::TextEdit::singleline(&mut n.name).hint_text("Folder name").desired_width(220.0));
            if !n.focused {
                field.request_focus();
                n.focused = true;
            }
            if field.changed() {
                n.error = None;
            }
            create = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if n.busy {
                ui.spinner();
            } else {
                create |= ui.button("Create").clicked();
                cancel = ui.button("Cancel").clicked();
            }
        });
        if let Some(e) = &n.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if cancel {
            self.browser.new_folder = None;
            return;
        }
        if !create || n.busy {
            return;
        }
        let name = n.name.trim().to_owned();
        match check_folder_name(&name) {
            Err(e) => n.error = Some(e),
            Ok(()) => {
                let Some(client) = self.client.clone() else { return };
                n.busy = true;
                n.error = None;
                let parent = at.clone();
                self.spawn(move || {
                    let result = client.new_folder(&parent.base_id, &parent.path, &name).map_err(|e| e.to_string());
                    Evt::FolderMade { parent, name, result }
                });
            }
        }
    }

    fn spawn(&self, job: impl FnOnce() -> Evt + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }
}

/// Whether HesteFiles takes `name` for a folder, and why not.
fn check_folder_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Give the folder a name.".into());
    }
    if name.chars().count() > 150 {
        return Err("Names can be at most 150 characters.".into());
    }
    if let Some(c) = name.chars().find(|c| hestefiles::FORBIDDEN.contains(c) || c.is_control()) {
        return Err(format!("Names can't contain “{c}”."));
    }
    Ok(())
}

fn base_icon(base: &BaseFolder) -> &'static str {
    if base.owner { "🏠" } else { "👥" }
}
