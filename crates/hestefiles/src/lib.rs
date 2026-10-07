//! Client for the HesteFiles REST API (cloud storage for sharing clips).
//!
//! Every request carries the account token in the `X-AccountToken` header.
//! Responses are wrapped as `{"success": {...}}` or `{"error": {"code", "message"}}`
//! regardless of HTTP status, so we always parse the body and map `error` to
//! [`Error::Api`].
//!
//! Uploads are chunked: `prepare_upload` reserves the file, each chunk (at most
//! 2 MB) is posted on its own, then `merge_chunks` starts a background job that
//! joins them, polled with `get_merge_status`. See [`Client::upload`].

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const DEFAULT_DOMAIN: &str = "https://hestefiles.jakobjohannes.com";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server answered with `{"error": {...}}`, e.g. `NOT_FOUND` for a bad token
    /// or path.
    #[error("{message} ({code})")]
    Api { code: String, message: String },
    #[error("network error: {0}")]
    Http(#[from] ureq::Error),
    #[error("unexpected response from server")]
    Malformed,
    #[error("couldn't read the file: {0}")]
    Io(#[from] std::io::Error),
    #[error("upload cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    /// `"never"` or a timestamp like `"10/03/2026, 10:00"`.
    pub expires: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub username: String,
    /// An address, or the literal `"No email"`.
    pub email: String,
    /// Their profile picture, in three sizes (maybe relative to the site:
    /// see [`Client::url`]).
    #[serde(default)]
    pub profile_picture_32px: Option<String>,
    #[serde(default)]
    pub profile_picture_64px: Option<String>,
    #[serde(default)]
    pub profile_picture_256px: Option<String>,
}

impl UserInfo {
    /// Their profile picture: the 64 px one (sharp at 32 px on a 2× display),
    /// else whichever size there is.
    pub fn profile_picture(&self) -> Option<&str> {
        [&self.profile_picture_64px, &self.profile_picture_256px, &self.profile_picture_32px]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .find(|u| !u.trim().is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub token_info: TokenInfo,
    pub user_info: UserInfo,
}

/// A top-level folder the user can access: their personal folder (id `"root"`) or
/// a folder shared with them.
#[derive(Debug, Clone, Deserialize)]
pub struct BaseFolder {
    pub name: String,
    pub id: String,
    pub read_only: bool,
    pub owner: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FolderEntry {
    pub name: String,
    pub size_b: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size_b: u64,
    /// MIME type, or `"file"` when unknown.
    #[serde(rename = "type")]
    pub mime: String,
}

/// What [`Client::directory`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Include {
    All,
    Files,
    Folders,
}

#[derive(Debug, Clone)]
pub struct Directory {
    pub folders: Vec<FolderEntry>,
    pub files: Vec<FileEntry>,
    pub read_only: bool,
}

/// Largest chunk the server accepts.
pub const MAX_CHUNK: u64 = 2 * 1024 * 1024;

/// Characters HesteFiles doesn't allow in a folder's name.
pub const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '<', '>', '%', '|', '"', '\'', '`'];

/// A folder name HesteFiles takes, made from `name`: without the characters
/// it doesn't allow (`\/:*?<>%|"'` and the backtick), at most 150 characters.
/// `None` if nothing's left.
pub fn folder_name(name: &str) -> Option<String> {
    let kept: String = name.chars().filter(|&c| !c.is_control() && !FORBIDDEN.contains(&c)).collect();
    let name: String = kept.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(150).collect();
    let name = name.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn folder_names() {
        use super::folder_name;
        assert_eq!(folder_name("osu!").as_deref(), Some("osu!"));
        assert_eq!(folder_name("Tom Clancy's Rainbow Six: Siege").as_deref(), Some("Tom Clancys Rainbow Six Siege"));
        assert_eq!(folder_name("100% Orange Juice").as_deref(), Some("100 Orange Juice"));
        assert_eq!(folder_name("'`%"), None);
        assert_eq!(folder_name(&"a".repeat(200)).map(|n| n.len()), Some(150));
    }
}

/// Where an upload is, for progress display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UploadProgress {
    /// Bytes sent so far, of the file's size.
    Sending { sent: u64, total: u64 },
    /// All chunks are up; the server is joining them (0..=100).
    Merging { percent: u8 },
}

/// One authenticated connection to a HesteFiles server. Calls block; run them off
/// the UI thread.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base_url: String,
    token: String,
}

impl Client {
    pub fn new(domain: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            // Errors arrive as JSON bodies with 4xx statuses; read them ourselves.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .new_agent();
        Self {
            agent,
            base_url: format!("{}/rest", domain.trim_end_matches('/')),
            token: token.trim().to_owned(),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// A full address for one the server gave, which may be relative to the
    /// site (`/static/…`).
    pub fn url(&self, href: &str) -> String {
        if href.starts_with("http://") || href.starts_with("https://") {
            href.to_owned()
        } else {
            let site = self.base_url.trim_end_matches("/rest");
            format!("{site}/{}", href.trim_start_matches('/'))
        }
    }

    /// Check the token and return who it belongs to.
    pub fn validate_token(&self) -> Result<Account> {
        self.get("validate_token")
    }

    pub fn base_folders(&self) -> Result<Vec<BaseFolder>> {
        #[derive(Deserialize)]
        struct Resp {
            base_folders_list: Vec<BaseFolder>,
        }
        Ok(self.get::<Resp>("get_base_folders")?.base_folders_list)
    }

    /// List one directory. `path` is relative to the base folder, `/`-separated, with
    /// no leading or trailing slash; `""` is the base folder itself. `include`
    /// says what to list (asking for less is quicker).
    pub fn directory(&self, base_folder_id: &str, path: &str, include: Include) -> Result<Directory> {
        #[derive(Serialize)]
        struct Req<'a> {
            base_folder_id: &'a str,
            path: &'a str,
            include: Include,
        }
        #[derive(Deserialize)]
        struct List {
            #[serde(default)]
            folders: Vec<FolderEntry>,
            #[serde(default)]
            files: Vec<FileEntry>,
        }
        #[derive(Deserialize)]
        struct Resp {
            directory_list: List,
            #[serde(default)]
            read_only: bool,
        }
        let resp: Resp = self.parse(
            self.agent
                .post(format!("{}/get_directory", self.base_url))
                .header("X-AccountToken", &self.token)
                .send_json(Req { base_folder_id, path: path.trim_matches('/'), include })?,
        )?;
        Ok(Directory {
            folders: resp.directory_list.folders,
            files: resp.directory_list.files,
            read_only: resp.read_only,
        })
    }

    /// Make a folder named `name` in `path` of `base_folder_id` (see
    /// [`folder_name`] for the names allowed). Fine if it's there already:
    /// returns whether it was.
    pub fn new_folder(&self, base_folder_id: &str, path: &str, name: &str) -> Result<bool> {
        #[derive(Serialize)]
        struct Req<'a> {
            base_folder_id: &'a str,
            path: &'a str,
            folder_name: &'a str,
            exist_ok: bool,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            already_exists: bool,
        }
        let resp: Resp = self.parse(
            self.agent
                .post(format!("{}/new_folder", self.base_url))
                .header("X-AccountToken", &self.token)
                .send_json(Req { base_folder_id, path: path.trim_matches('/'), folder_name: name, exist_ok: true })?,
        )?;
        Ok(resp.already_exists)
    }

    /// Upload `file` into `path` of `base_folder_id`, named `filename`. The server
    /// keeps both if the name is taken (the new one gets " - Copy (1)").
    ///
    /// `progress` is called as chunks go up and while the server merges them;
    /// setting `cancel` stops between chunks. A chunk that fails for a network
    /// reason is retried a few times before giving up.
    ///
    /// With `make_public`, the file gets a public link anyone can view and
    /// download it with, which is returned.
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &self,
        file: &Path,
        base_folder_id: &str,
        path: &str,
        filename: &str,
        make_public: bool,
        cancel: &AtomicBool,
        mut progress: impl FnMut(UploadProgress),
    ) -> Result<Option<String>> {
        let mut f = std::fs::File::open(file)?;
        let size = f.metadata()?.len();
        let chunk = self.chunk_size().unwrap_or(MAX_CHUNK);

        #[derive(Serialize)]
        struct Prepare<'a> {
            filename: &'a str,
            base_folder_id: &'a str,
            path: &'a str,
            size: u64,
            chunk_size: u64,
            make_public: bool,
        }
        #[derive(Deserialize)]
        struct Prepared {
            upload_id: String,
            total_chunks: u64,
        }
        let prepared: Prepared = self.parse(
            self.agent
                .post(format!("{}/prepare_upload", self.base_url))
                .header("X-AccountToken", &self.token)
                .send_json(Prepare { filename, base_folder_id, path: path.trim_matches('/'), size, chunk_size: chunk, make_public })?,
        )?;

        // The server doesn't check chunk sizes, so send exactly `chunk` bytes
        // each (the last one shorter): a short chunk would silently truncate.
        let mut buf = vec![0u8; chunk as usize];
        let mut sent = 0u64;
        progress(UploadProgress::Sending { sent, total: size });
        for index in 0..prepared.total_chunks {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let start = index * chunk;
            let len = chunk.min(size - start) as usize;
            f.seek(SeekFrom::Start(start))?;
            f.read_exact(&mut buf[..len])?;
            self.send_chunk(&prepared.upload_id, index, &buf[..len])?;
            sent += len as u64;
            progress(UploadProgress::Sending { sent, total: size });
        }

        #[derive(Deserialize)]
        struct Merge {
            task_id: String,
        }
        let merge: Merge = self.parse(
            self.agent
                .get(format!("{}/merge_chunks", self.base_url))
                .query("ui", &prepared.upload_id)
                .header("X-AccountToken", &self.token)
                .call()?,
        )?;
        #[derive(Deserialize)]
        struct Status {
            percent: f64,
            error: bool,
            done: bool,
            /// Set once done, when the upload asked to be made public.
            #[serde(default)]
            public_link: Option<String>,
        }
        let started = Instant::now();
        loop {
            let status: Status = self.parse(
                self.agent
                    .get(format!("{}/get_merge_status", self.base_url))
                    .query("ti", &merge.task_id)
                    .header("X-AccountToken", &self.token)
                    .call()?,
            )?;
            if status.error {
                return Err(Error::Api { code: "MERGE_FAILED".into(), message: "The server couldn't put the file together.".into() });
            }
            progress(UploadProgress::Merging { percent: status.percent.clamp(0.0, 100.0) as u8 });
            if status.done {
                return Ok(status.public_link);
            }
            // Merging is quick (a few MB a second at worst); don't wait forever.
            if started.elapsed() > Duration::from_secs(600) {
                return Err(Error::Api { code: "MERGE_TIMEOUT".into(), message: "The server took too long to finish the upload.".into() });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// The server's recommended chunk size for this connection, from a short
    /// speed test. Smaller chunks on slow links make progress smoother and a
    /// failed chunk cheaper to resend.
    fn chunk_size(&self) -> Result<u64> {
        let sample = vec![0u8; 512 * 1024];
        let t = Instant::now();
        self.agent
            .post(format!("{}/test_upload_speed", self.base_url))
            .header("X-AccountToken", &self.token)
            .header("Content-Type", "application/octet-stream")
            .send(&sample[..])?;
        let mbps = (sample.len() as f64 * 8.0) / t.elapsed().as_secs_f64().max(1e-3) / 1e6;
        #[derive(Serialize)]
        struct Req {
            mbps: f64,
        }
        #[derive(Deserialize)]
        struct Resp {
            chunk_bytes: u64,
        }
        let r: Resp = self.parse(
            self.agent
                .post(format!("{}/get_recommended_chunk_size", self.base_url))
                .header("X-AccountToken", &self.token)
                .send_json(Req { mbps })?,
        )?;
        Ok(r.chunk_bytes.clamp(64 * 1024, MAX_CHUNK))
    }

    fn send_chunk(&self, upload_id: &str, index: u64, bytes: &[u8]) -> Result<()> {
        let mut attempt = 0;
        loop {
            let result = self
                .agent
                .post(format!("{}/upload_chunk", self.base_url))
                .query("ui", upload_id)
                .query("ci", index.to_string())
                .header("X-AccountToken", &self.token)
                .header("Content-Type", "application/octet-stream")
                // 2 MB at 0.5 Mbit/s is ~35 s: well past the 15 s used for the
                // small JSON calls.
                .config()
                .timeout_global(Some(Duration::from_secs(120)))
                .build()
                .send(bytes)
                .map_err(Error::from)
                .and_then(|r| self.parse::<serde_json::Value>(r));
            match result {
                Ok(_) => return Ok(()),
                // Network trouble or a server hiccup: try again. A clear "no"
                // from the API won't change by retrying.
                Err(Error::Http(_) | Error::Malformed) if attempt < 3 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(500 * attempt));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn get<T: DeserializeOwned>(&self, endpoint: &str) -> Result<T> {
        self.parse(
            self.agent
                .get(format!("{}/{endpoint}", self.base_url))
                .header("X-AccountToken", &self.token)
                .call()?,
        )
    }

    fn parse<T: DeserializeOwned>(&self, resp: ureq::http::Response<ureq::Body>) -> Result<T> {
        #[derive(Deserialize)]
        struct ApiError {
            code: String,
            message: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Envelope {
            Success(serde_json::Value),
            Error(ApiError),
        }
        let envelope: Envelope =
            resp.into_body().read_json().map_err(|_| Error::Malformed)?;
        match envelope {
            Envelope::Success(v) => serde_json::from_value(v).map_err(|_| Error::Malformed),
            Envelope::Error(e) => Err(Error::Api { code: e.code, message: e.message }),
        }
    }
}

#[cfg(test)]
mod account_tests {
    /// As validate_token answers (2026-10-05).
    #[test]
    fn reads_the_profile_picture() {
        let json = r#"{"username": "mikhail", "email": "a@b.c",
            "profile_picture_32px": "http://x/p_32.jpeg",
            "profile_picture_64px": "http://x/p_64.jpeg",
            "profile_picture_256px": "http://x/p_256.jpeg"}"#;
        let user: super::UserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(user.profile_picture(), Some("http://x/p_64.jpeg"));
        let none: super::UserInfo = serde_json::from_str(r#"{"username": "a", "email": "b"}"#).unwrap();
        assert_eq!(none.profile_picture(), None);
    }
}
