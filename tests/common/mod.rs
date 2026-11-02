use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

pub fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hivequeen"))
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A temporary state directory and settings file that point every backend at
/// the fake agent fixture.
pub struct Env {
    pub dir: tempfile::TempDir,
}

impl Env {
    pub fn new() -> std::io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let fake = fixture("fake-agent.sh");
        let mut settings = String::new();
        for kind in ["droid", "claude", "codex"] {
            let wrapper = dir.path().join(format!("fake-{kind}"));
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nFAKE_KIND=${{FAKE_KIND:-{kind}}} exec {:?} \"$@\"\n",
                    fake.display().to_string()
                ),
            )?;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
            settings.push_str(&format!(
                "[{kind}]\nprogram = {:?}\n",
                wrapper.display().to_string()
            ));
        }
        fs::write(dir.path().join("hivequeen.toml"), settings)?;
        Ok(Env { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn cmd(&self) -> Command {
        let mut c = bin();
        c.arg("--state-dir").arg(self.path().join("state"));
        c.arg("--config").arg(self.path().join("hivequeen.toml"));
        c
    }

    pub fn write(&self, name: &str, body: &str) -> std::io::Result<PathBuf> {
        let path = self.path().join(name);
        fs::write(&path, body)?;
        Ok(path)
    }
}

pub fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

pub fn run_dir(out: &Output) -> PathBuf {
    PathBuf::from(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .unwrap_or("")
            .trim(),
    )
}
