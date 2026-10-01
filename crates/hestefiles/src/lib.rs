//! Client for the HesteFiles REST API (cloud storage for sharing clips).
//!
//! Every request carries the account token in the `X-AccountToken` header.
//! Responses are wrapped as `{"success": {...}}` or `{"error": {"code", "message"}}`
//! regardless of HTTP status, so we always parse the body and map `error` to
//! [`Error::Api`].
//!
//! The API is read-only for now (validate token, list folders). Upload lands when
//! the server exposes it.

use std::time::Duration;

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

#[derive(Debug, Clone)]
pub struct Directory {
    pub folders: Vec<FolderEntry>,
    pub files: Vec<FileEntry>,
    pub read_only: bool,
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
    /// no leading or trailing slash; `""` is the base folder itself.
    pub fn directory(&self, base_folder_id: &str, path: &str) -> Result<Directory> {
        #[derive(Serialize)]
        struct Req<'a> {
            base_folder_id: &'a str,
            path: &'a str,
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
                .send_json(Req { base_folder_id, path: path.trim_matches('/') })?,
        )?;
        Ok(Directory {
            folders: resp.directory_list.folders,
            files: resp.directory_list.files,
            read_only: resp.read_only,
        })
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
