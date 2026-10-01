//! HesteFiles integration: the account token (kept in the OS keychain), the default
//! clips folder (kept in a small config file), and a folders-only browser used both
//! to pick that default and to pick a one-off folder when sharing.
//!
//! Network calls run on short-lived threads; results come back over a channel that
//! `poll` drains each frame, so the UI never blocks on the server.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use hestefiles::{Account, BaseFolder, Client};
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
#[derive(Default, Serialize, Deserialize)]
struct Config {
    default_folder: Option<FolderRef>,
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
}

pub struct Cloud {
    client: Option<Client>,
    pub connection: Connection,
    /// Text field buffer for pasting a token.
    pub token_input: String,
    pub default_folder: Option<FolderRef>,
    pub browser: FolderBrowser,
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
            browser: FolderBrowser::default(),
            ctx,
            tx,
            rx,
        };
        // Reading the keychain can block on a system permission prompt, so never do
        // it on the UI thread.
        cloud.connection = Connection::Connecting;
        cloud.spawn(|| Evt::StoredToken(keychain().and_then(|k| k.get_password().ok())));
        cloud
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
        let config = Config { default_folder: self.default_folder.clone() };
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
                        if let Some(k) = keychain() {
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
            }
        }
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
        let (Some(at), Some(client)) = (to, self.client.clone()) else { return };
        self.spawn(move || {
            let result = client
                .directory(&at.base_id, &at.path)
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

        if let Some(to) = go {
            self.navigate(to);
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

fn base_icon(base: &BaseFolder) -> &'static str {
    if base.owner { "🏠" } else { "👥" }
}
