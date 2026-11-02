use crate::{Error, Result, at};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    #[default]
    Pending,
    Running,
    Waiting,
    /// An editing step finished and is queued for ordered integration.
    Integrating,
    Succeeded,
    Failed,
    Conflicted,
    Blocked,
    Interrupted,
}

impl State {
    pub fn is_terminal(self) -> bool {
        self == State::Succeeded || self.is_bad()
    }

    pub fn is_bad(self) -> bool {
        matches!(
            self,
            State::Failed | State::Conflicted | State::Blocked | State::Interrupted
        )
    }
}

/// The whole run, written only by the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub workflow_digest: String,
    pub resources: BTreeMap<String, String>,
    pub repo: Option<PathBuf>,
    pub base: Option<String>,
    pub tip: Option<String>,
    pub started_unix: u64,
    pub deadline_unix: u64,
    pub steps: Vec<StepRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: String,
    pub state: State,
    pub detail: Option<String>,
    pub started_unix: Option<u64>,
    pub finished_unix: Option<u64>,
    pub exit: Option<String>,
    pub command: Vec<String>,
    pub backend: Option<String>,
    pub version: Option<String>,
    pub model_requested: Option<String>,
    pub model_reported: Option<String>,
    pub session: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub inputs_digest: Option<String>,
    pub answer_digest: Option<String>,
    /// For steps in a cell: the integration tip the cell started from.
    pub tip: Option<String>,
    pub cell_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub step: String,
    pub inputs_digest: String,
    pub answer_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub step: String,
    pub workflow_digest: String,
    pub inputs_digest: String,
    pub approved_unix: u64,
}

pub fn state_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(x) = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(x.join("hivequeen"));
    }
    let home = env::var_os("HOME")
        .ok_or_else(|| Error::Infra("HOME is not set; pass --state-dir".into()))?;
    Ok(PathBuf::from(home).join(".local/state/hivequeen"))
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates a new uniquely named directory under `parent`.
pub fn new_dir(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent).map_err(at(parent))?;
    for _ in 0..100 {
        let path = parent.join(format!("{}-{}", crate::now_unix(), unique_suffix()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(at(&path)(e)),
        }
    }
    Err(Error::Infra(format!(
        "could not create a unique directory in {}",
        parent.display()
    )))
}

/// Takes the single-coordinator lock on a run directory.
pub fn lock(dir: &Path) -> Result<File> {
    let path = dir.join("run.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(at(&path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(Error::Infra(format!(
            "{} is owned by another hivequeen process",
            dir.display()
        ))),
        Err(fs::TryLockError::Error(e)) => Err(at(&path)(e)),
    }
}

fn temp_beside(path: &Path) -> (PathBuf, PathBuf) {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", unique_suffix()));
    (dir, tmp)
}

fn write_synced(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Same-directory temp file, sync, rename, then sync the directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let (dir, tmp) = temp_beside(path);
    let result = write_synced(&tmp, bytes)
        .and_then(|()| fs::rename(&tmp, path))
        .and_then(|()| File::open(&dir)?.sync_all());
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(at(path))
}

/// Like `write_atomic`, but refuses to replace an existing file.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let (dir, tmp) = temp_beside(path);
    // A hard link fails if the target exists, which makes publication both
    // atomic and exclusive.
    let result = write_synced(&tmp, bytes)
        .and_then(|()| fs::hard_link(&tmp, path))
        .and_then(|()| File::open(&dir)?.sync_all());
    let _ = fs::remove_file(&tmp);
    match result {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(Error::Invalid(format!(
            "{} is already recorded and cannot be replaced",
            path.display()
        ))),
        other => other.map_err(at(path)),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).map_err(at(path))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Infra(format!("{}: {e}", path.display())))
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn digest_file(path: &Path) -> Result<String> {
    Ok(digest(&fs::read(path).map_err(at(path))?))
}

/// The committed receipt for a step, verified against its answer. A step
/// directory without a receipt is not complete, whatever else it contains.
pub fn completed(step_dir: &Path) -> Result<Option<Receipt>> {
    let path = step_dir.join("receipt.json");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(at(&path)(e)),
        Ok(_) => {}
    }
    let receipt: Receipt = read_json(&path)?;
    let answer = step_dir.join("answer.md");
    if digest_file(&answer)? != receipt.answer_digest {
        return Err(Error::Infra(format!(
            "integrity error: {} changed after it was recorded",
            answer.display()
        )));
    }
    Ok(Some(receipt))
}

/// Records a finished step: receipt first, so state never points at a step
/// whose answer is not committed.
pub fn commit_answer(step_dir: &Path, receipt: &Receipt) -> Result<()> {
    write_json(&step_dir.join("receipt.json"), receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn receipt(answer: &[u8]) -> Receipt {
        Receipt {
            step: "a".into(),
            inputs_digest: digest(b""),
            answer_digest: digest(answer),
        }
    }

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn write_is_atomic() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.json");
        write_atomic(&path, b"one")?;
        write_atomic(&path, b"two")?;
        assert_eq!(fs::read(&path)?, b"two");
        let names: Vec<_> = fs::read_dir(dir.path())?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()?;
        assert_eq!(
            names.len(),
            1,
            "no temporary files are left behind: {names:?}"
        );
        Ok(())
    }

    #[test]
    fn write_error_is_not_missing() -> TestResult {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("file"), "x")?;
        let result = write_atomic(&dir.path().join("file/inner.json"), b"x");
        assert!(matches!(result, Err(Error::Infra(_))), "{result:?}");
        Ok(())
    }

    #[test]
    fn write_new_refuses_to_replace() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("approval.json");
        write_new(&path, b"first")?;
        assert!(write_new(&path, b"second").is_err());
        assert_eq!(fs::read(&path)?, b"first");
        Ok(())
    }

    #[test]
    fn second_owner_is_refused() -> TestResult {
        let dir = tempfile::tempdir()?;
        let _held = lock(dir.path())?;
        match lock(dir.path()) {
            Err(Error::Infra(m)) => assert!(m.contains("another"), "{m}"),
            other => panic!("expected a refused lock, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn new_dirs_are_unique() -> TestResult {
        let dir = tempfile::tempdir()?;
        let a = new_dir(dir.path())?;
        let b = new_dir(dir.path())?;
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        Ok(())
    }

    #[test]
    fn changed_answer_fails_integrity() -> TestResult {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("answer.md"), "original")?;
        commit_answer(dir.path(), &receipt(b"original"))?;
        assert_eq!(completed(dir.path())?, Some(receipt(b"original")));
        fs::write(dir.path().join("answer.md"), "edited")?;
        match completed(dir.path()) {
            Err(Error::Infra(m)) => assert!(m.contains("integrity"), "{m}"),
            other => panic!("expected an integrity error, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn old_file_is_not_completion() -> TestResult {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("answer.md"),
            "left over from something else",
        )?;
        assert_eq!(completed(dir.path())?, None);
        Ok(())
    }

    #[test]
    fn explicit_state_dir_wins() -> TestResult {
        assert_eq!(
            state_root(Some(Path::new("/tmp/x")))?,
            PathBuf::from("/tmp/x")
        );
        Ok(())
    }
}
