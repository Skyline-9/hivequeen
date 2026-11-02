use crate::{Error, Result, at};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::fmt;
use std::fs::{self, File};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
const GRACE: Duration = Duration::from_secs(5);
pub const MAX_LOG_BYTES: u64 = 100 * 1024 * 1024;
pub const GIT_REDIRECTS: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Program, argv, working directory, and stdin source. Never a shell string.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdin: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl Invocation {
    pub fn new(program: impl Into<String>, cwd: &Path) -> Self {
        Invocation {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            stdin: None,
            env: Vec::new(),
        }
    }

    pub fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.args.push(a.into());
        self
    }

    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub deadline: Instant,
    pub max_log_bytes: u64,
    pub cancel: Arc<AtomicBool>,
}

impl Limits {
    pub fn new(timeout: Duration, cancel: Arc<AtomicBool>) -> Self {
        Limits {
            deadline: Instant::now() + timeout,
            max_log_bytes: MAX_LOG_BYTES,
            cancel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    TimedOut,
    Cancelled,
    LogLimit,
}

impl Exit {
    pub fn success(&self) -> bool {
        *self == Exit::Code(0)
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "exit {c}"),
            Exit::Signal(s) => write!(f, "killed by signal {s}"),
            Exit::TimedOut => f.write_str("timed out"),
            Exit::Cancelled => f.write_str("cancelled"),
            Exit::LogLimit => f.write_str("output exceeded the log limit"),
        }
    }
}

/// Runs a child in its own process group with output redirected to files,
/// and stops the whole group on exit, cancellation, deadline, or log limit.
pub fn execute(inv: &Invocation, stdout: &Path, stderr: &Path, limits: &Limits) -> Result<Exit> {
    let out = File::create(stdout).map_err(at(stdout))?;
    let err = File::create(stderr).map_err(at(stderr))?;
    let stdin = match &inv.stdin {
        Some(p) => Stdio::from(File::open(p).map_err(at(p))?),
        None => Stdio::null(),
    };
    let mut command = Command::new(&inv.program);
    for var in GIT_REDIRECTS {
        command.env_remove(var);
    }
    let mut child = command
        .args(&inv.args)
        .current_dir(&inv.cwd)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .stdin(stdin)
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| Error::Step(format!("cannot start {}: {e}", inv.program)))?;
    let pgid = Pid::from_raw(i32::try_from(child.id()).map_err(|e| Error::Infra(e.to_string()))?);
    loop {
        if let Some(status) = child.try_wait()? {
            stop(&mut child, pgid)?;
            return Ok(match (status.code(), status.signal()) {
                (Some(c), _) => Exit::Code(c),
                (None, Some(s)) => Exit::Signal(s),
                (None, None) => Exit::Code(-1),
            });
        }
        let reason = if limits.cancel.load(Ordering::Relaxed) {
            Some(Exit::Cancelled)
        } else if Instant::now() >= limits.deadline {
            Some(Exit::TimedOut)
        } else if size(stdout) + size(stderr) > limits.max_log_bytes {
            Some(Exit::LogLimit)
        } else {
            None
        };
        if let Some(reason) = reason {
            stop(&mut child, pgid)?;
            return Ok(reason);
        }
        thread::sleep(POLL);
    }
}

fn size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn group_alive(pgid: Pid) -> bool {
    killpg(pgid, None) != Err(Errno::ESRCH)
}

/// TERM the group, wait up to the grace period while reaping the leader, then
/// KILL. Processes that left the group on purpose are outside this guarantee.
fn stop(child: &mut Child, pgid: Pid) -> Result<()> {
    if !group_alive(pgid) {
        return Ok(());
    }
    let _ = killpg(pgid, Signal::SIGTERM);
    let until = Instant::now() + GRACE;
    while Instant::now() < until {
        child.try_wait()?;
        if !group_alive(pgid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = killpg(pgid, Signal::SIGKILL);
    child.wait()?;
    let until = Instant::now() + GRACE;
    while group_alive(pgid) {
        if Instant::now() >= until {
            return Err(Error::Infra(format!(
                "process group {pgid} survived SIGKILL"
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Runs a short command and returns its combined output, for capability probes.
pub fn capture(program: &str, args: &[&str], dir: &Path) -> Result<(Exit, String)> {
    let mut inv = Invocation::new(program, dir);
    inv.args = args.iter().map(|a| (*a).to_owned()).collect();
    let (out, err) = (dir.join("probe.out"), dir.join("probe.err"));
    let exit = execute(
        &inv,
        &out,
        &err,
        &Limits::new(Duration::from_secs(30), Arc::new(AtomicBool::new(false))),
    )?;
    let mut text = fs::read_to_string(&out).map_err(at(&out))?;
    text.push_str(&fs::read_to_string(&err).map_err(at(&err))?);
    Ok((exit, text))
}
#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn sh(dir: &Path, script: &str) -> Invocation {
        let mut inv = Invocation::new("/bin/sh", dir);
        inv.arg("-c").arg(script);
        inv
    }

    fn run(dir: &Path, inv: &Invocation, timeout: Duration) -> Result<Exit> {
        let limits = Limits::new(timeout, Arc::new(AtomicBool::new(false)));
        execute(inv, &dir.join("out"), &dir.join("err"), &limits)
    }

    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
            && !fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "))
    }

    #[test]
    fn spawn_failure_is_a_step_error() -> TestResult {
        let dir = tempfile::tempdir()?;
        let inv = Invocation::new("/nonexistent/program", dir.path());
        assert!(matches!(
            run(dir.path(), &inv, Duration::from_secs(5)),
            Err(Error::Step(_))
        ));
        Ok(())
    }

    #[test]
    fn nonzero_exit_is_reported() -> TestResult {
        let dir = tempfile::tempdir()?;
        let exit = run(
            dir.path(),
            &sh(dir.path(), "echo oops >&2; exit 3"),
            Duration::from_secs(5),
        )?;
        assert_eq!(exit, Exit::Code(3));
        assert_eq!(fs::read_to_string(dir.path().join("err"))?, "oops\n");
        Ok(())
    }

    #[test]
    fn argv_metacharacters_are_literal() -> TestResult {
        let dir = tempfile::tempdir()?;
        let mut inv = Invocation::new("/bin/echo", dir.path());
        inv.arg("$HOME;*|`id`");
        assert!(run(dir.path(), &inv, Duration::from_secs(5))?.success());
        assert_eq!(
            fs::read_to_string(dir.path().join("out"))?,
            "$HOME;*|`id`\n"
        );
        Ok(())
    }

    #[test]
    fn stdin_comes_from_a_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("in"), "prompt text")?;
        let mut inv = Invocation::new("/bin/cat", dir.path());
        inv.stdin = Some(dir.path().join("in"));
        assert!(run(dir.path(), &inv, Duration::from_secs(5))?.success());
        assert_eq!(fs::read_to_string(dir.path().join("out"))?, "prompt text");
        Ok(())
    }

    #[test]
    fn timeout_stops_the_child() -> TestResult {
        let dir = tempfile::tempdir()?;
        let exit = run(
            dir.path(),
            &sh(dir.path(), "sleep 30"),
            Duration::from_millis(300),
        )?;
        assert_eq!(exit, Exit::TimedOut);
        Ok(())
    }

    #[test]
    fn output_limit_fails() -> TestResult {
        let dir = tempfile::tempdir()?;
        let limits = Limits {
            max_log_bytes: 1024,
            ..Limits::new(Duration::from_secs(10), Arc::new(AtomicBool::new(false)))
        };
        let inv = sh(dir.path(), "yes hivequeen");
        let exit = execute(
            &inv,
            &dir.path().join("out"),
            &dir.path().join("err"),
            &limits,
        )?;
        assert_eq!(exit, Exit::LogLimit);
        Ok(())
    }

    #[test]
    fn cancellation_stops_the_child() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cancel = Arc::new(AtomicBool::new(true));
        let limits = Limits::new(Duration::from_secs(30), cancel);
        let exit = execute(
            &sh(dir.path(), "sleep 30"),
            &dir.path().join("out"),
            &dir.path().join("err"),
            &limits,
        )?;
        assert_eq!(exit, Exit::Cancelled);
        Ok(())
    }

    #[test]
    fn grandchildren_are_stopped_after_the_parent_exits() -> TestResult {
        let dir = tempfile::tempdir()?;
        let exit = run(
            dir.path(),
            &sh(dir.path(), "sleep 30 & echo $! > pid; exit 0"),
            Duration::from_secs(10),
        )?;
        assert!(exit.success());
        let pid: i32 = fs::read_to_string(dir.path().join("pid"))?.trim().parse()?;
        assert!(!alive(pid), "background grandchild {pid} must not survive");
        Ok(())
    }

    #[test]
    fn capture_returns_output() -> TestResult {
        let dir = tempfile::tempdir()?;
        let (exit, text) = capture("/bin/echo", &["hello"], dir.path())?;
        assert!(exit.success());
        assert_eq!(text.trim(), "hello");
        Ok(())
    }
}
