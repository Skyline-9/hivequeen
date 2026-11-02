//! Hivequeen runs a small TOML workflow of agent, command, and approval steps
//! on one machine. Every answer is stored as hashed evidence, and editing steps
//! run in parallel Git worktrees that are merged in a fixed order.

mod backend;
mod config;
mod process;
mod runner;
mod store;
mod workspace;

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub use backend::Backend;
pub use config::{Settings, load_settings};
pub use runner::{Options, Outcome, approve, check, doctor, draft, report, resume, run, status};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The workflow, configuration, or command-line input is invalid.
    Invalid(String),
    /// One step failed. Independent work can still continue.
    Step(String),
    /// Storage, Git, or another part of the environment failed.
    Infra(String),
}

impl Error {
    /// The process exit code for this error class.
    ///
    /// ```
    /// let e = hivequeen::Error::Invalid("unknown field".into());
    /// assert_eq!(e.exit_code(), 2);
    /// ```
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Step(_) => 1,
            Error::Invalid(_) => 2,
            Error::Infra(_) => 4,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid input: {m}"),
            Error::Step(m) => write!(f, "step failed: {m}"),
            Error::Infra(m) => write!(f, "infrastructure failure: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Infra(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Infra(e.to_string())
    }
}

/// Wraps an I/O error with the path it happened on.
pub(crate) fn at(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Infra(format!("{}: {e}", path.display()))
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
