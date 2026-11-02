use crate::config::RESERVED;
use crate::{Error, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Runs git with hooks disabled, so run-owned worktrees never execute
/// repository hooks. Returns (exit code, stdout, stderr).
fn run_git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<(Option<i32>, Vec<u8>, String)> {
    let mut command = Command::new("git");
    for var in crate::process::GIT_REDIRECTS {
        command.env_remove(var);
    }
    let out = command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Infra(format!("cannot run git: {e}")))?;
    Ok((
        out.status.code(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    ))
}

fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String> {
    match run_git(dir, args)? {
        (Some(0), out, _) => Ok(String::from_utf8_lossy(&out).trim_end().to_owned()),
        (_, _, err) => {
            let shown: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
            Err(Error::Infra(format!(
                "git {} failed: {err}",
                shown.join(" ")
            )))
        }
    }
}

pub struct Prepared {
    pub top: PathBuf,
    pub base: String,
}

pub enum Merge {
    Clean(String),
    Conflict(Vec<String>),
}

/// Checks the user's repository and pins its HEAD. Never modifies it.
pub fn prepare(repo: &Path) -> Result<Prepared> {
    let top = git(repo, &["rev-parse", "--show-toplevel"])
        .map_err(|e| Error::Invalid(format!("{} is not a Git repository: {e}", repo.display())))?;
    let top = PathBuf::from(top);
    if !git(&top, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        return Err(Error::Invalid(format!(
            "{} has uncommitted or untracked changes; commit or remove them first",
            top.display()
        )));
    }
    let base = git(&top, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|e| Error::Invalid(format!("the repository needs at least one commit: {e}")))?;
    if !git(
        &top,
        &["ls-tree", "-r", "--name-only", &base, "--", RESERVED],
    )?
    .is_empty()
    {
        return Err(Error::Invalid(format!(
            "{RESERVED}/ is reserved for hivequeen and must not be tracked"
        )));
    }
    for var in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        git(&top, &["var", var])
            .map_err(|e| Error::Infra(format!("Git identity is not configured: {e}")))?;
    }
    Ok(Prepared { top, base })
}

/// Creates a detached run-owned worktree at `commit`.
pub fn add_cell(top: &Path, cell: &Path, commit: &str) -> Result<()> {
    git(
        top,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            OsStr::new("--detach"),
            cell.as_os_str(),
            OsStr::new(commit),
        ],
    )
    .map(drop)
}

/// Records everything in the cell except the reserved directory as one commit
/// on top of `parent`, and points `refname` at it. Commits an agent made in
/// the cell are flattened into this snapshot.
pub fn snapshot(
    top: &Path,
    cell: &Path,
    parent: &str,
    refname: &str,
    message: &str,
) -> Result<String> {
    let exclude = format!(":(exclude){RESERVED}");
    git(cell, &["add", "-A", "--", ".", &exclude])?;
    git(
        cell,
        &[
            "rm",
            "-r",
            "--cached",
            "--quiet",
            "--ignore-unmatch",
            "--",
            RESERVED,
        ],
    )?;
    let changes = git(
        cell,
        &["diff-index", "--cached", "--no-renames", "-r", parent],
    )?;
    for line in changes.lines() {
        let (meta, path) = line.split_once('\t').unwrap_or((line, ""));
        let mut fields = meta.trim_start_matches(':').split(' ');
        let (old, new) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
        if old == "160000" || new == "160000" {
            return Err(Error::Step(format!(
                "submodule changes are not supported: {path}"
            )));
        }
        if new == "120000" && old != "120000" {
            return Err(Error::Step(format!(
                "new symlinks are not supported: {path}"
            )));
        }
    }
    let tree = git(cell, &["write-tree"])?;
    let commit = git(
        cell,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            parent,
            "-m",
            message,
        ],
    )?;
    set_ref(top, refname, &commit)?;
    Ok(commit)
}

/// Merges a cell commit into the integration tip without a worktree.
pub fn integrate(
    top: &Path,
    tip: &str,
    cell_parent: &str,
    cell: &str,
    message: &str,
) -> Result<Merge> {
    if tip == cell_parent {
        return Ok(Merge::Clean(cell.to_owned()));
    }
    let (code, out, err) = run_git(
        top,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            tip,
            cell,
        ],
    )?;
    let out = String::from_utf8_lossy(&out);
    let mut lines = out.lines();
    let tree = lines.next().unwrap_or("").to_owned();
    match code {
        Some(0) => {
            let commit = git(
                top,
                &[
                    "commit-tree",
                    "--no-gpg-sign",
                    &tree,
                    "-p",
                    tip,
                    "-p",
                    cell,
                    "-m",
                    message,
                ],
            )?;
            Ok(Merge::Clean(commit))
        }
        Some(1) => Ok(Merge::Conflict(
            lines.filter(|l| !l.is_empty()).map(str::to_owned).collect(),
        )),
        _ => Err(Error::Infra(format!("git merge-tree failed: {err}"))),
    }
}
pub fn set_ref(top: &Path, refname: &str, commit: &str) -> Result<()> {
    git(top, &["update-ref", refname, commit]).map(drop)
}

/// Binary-safe patch from `base` to `tip`.
pub fn patch(top: &Path, base: &str, tip: &str) -> Result<Vec<u8>> {
    let args = [
        "diff",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-renames",
        base,
        tip,
    ];
    match run_git(top, &args)? {
        (Some(0), out, _) => Ok(out),
        (_, _, err) => Err(Error::Infra(format!("git diff failed: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn repo() -> std::result::Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"])?;
        git(p, &["config", "user.name", "Test"])?;
        git(p, &["config", "user.email", "test@example.com"])?;
        git(p, &["config", "commit.gpgsign", "false"])?;
        fs::write(p.join("a.txt"), "one\ntwo\nthree\n")?;
        fs::write(p.join("gone.txt"), "delete me\n")?;
        git(p, &["add", "-A"])?;
        git(p, &["commit", "-q", "-m", "init"])?;
        Ok(dir)
    }

    fn cell(top: &Path, runs: &Path, name: &str, at: &str) -> Result<PathBuf> {
        let path = runs.join(name);
        add_cell(top, &path, at)?;
        Ok(path)
    }

    #[test]
    fn dirty_source_is_refused() -> TestResult {
        let r = repo()?;
        fs::write(r.path().join("new.txt"), "x")?;
        assert!(
            matches!(prepare(r.path()), Err(Error::Invalid(ref m)) if m.contains("uncommitted"))
        );
        Ok(())
    }

    #[test]
    fn tracked_reserved_directory_is_refused() -> TestResult {
        let r = repo()?;
        fs::create_dir(r.path().join(RESERVED))?;
        fs::write(r.path().join(RESERVED).join("x"), "x")?;
        git(r.path(), &["add", "-A"])?;
        git(r.path(), &["commit", "-q", "-m", "reserved"])?;
        assert!(matches!(prepare(r.path()), Err(Error::Invalid(ref m)) if m.contains(RESERVED)));
        Ok(())
    }

    #[test]
    fn missing_git_identity_is_infrastructure_failure() -> TestResult {
        let r = repo()?;
        git(r.path(), &["config", "user.name", ""])?;
        git(r.path(), &["config", "user.useConfigOnly", "true"])?;
        assert!(matches!(prepare(r.path()), Err(Error::Infra(ref m)) if m.contains("identity")));
        Ok(())
    }

    #[test]
    fn snapshot_captures_changes_and_leaves_the_user_checkout_alone() -> TestResult {
        let (r, runs) = (repo()?, tempfile::tempdir()?);
        let p = prepare(r.path())?;
        let before = git(r.path(), &["status", "--porcelain"])?;
        let c = cell(&p.top, runs.path(), "edit", &p.base)?;
        fs::write(c.join("a.txt"), "one\nTWO\nthree\n")?;
        fs::remove_file(c.join("gone.txt"))?;
        fs::write(c.join("bin.dat"), [0u8, 159, 146, 150, 0, 1])?;
        fs::write(c.join("run.sh"), "#!/bin/sh\n")?;
        fs::set_permissions(c.join("run.sh"), fs::Permissions::from_mode(0o755))?;
        fs::create_dir(c.join(RESERVED))?;
        fs::write(c.join(RESERVED).join("prompt.md"), "private")?;
        git(&c, &["add", "-f", RESERVED])?;
        let commit = snapshot(&p.top, &c, &p.base, "refs/hivequeen/t/cells/edit", "edit")?;
        let files = git(&p.top, &["diff", "--name-status", &p.base, &commit])?;
        assert!(
            files.contains("M\ta.txt")
                && files.contains("D\tgone.txt")
                && files.contains("A\tbin.dat")
                && files.contains("A\trun.sh"),
            "{files}"
        );
        assert!(
            !files.contains(RESERVED),
            "reserved directory leaked: {files}"
        );
        assert_eq!(git(r.path(), &["rev-parse", "HEAD"])?, p.base);
        assert_eq!(git(r.path(), &["status", "--porcelain"])?, before);
        assert_eq!(
            git(&p.top, &["rev-parse", "refs/hivequeen/t/cells/edit"])?,
            commit
        );
        Ok(())
    }

    #[test]
    fn new_symlinks_are_refused() -> TestResult {
        let (r, runs) = (repo()?, tempfile::tempdir()?);
        let p = prepare(r.path())?;
        let c = cell(&p.top, runs.path(), "link", &p.base)?;
        std::os::unix::fs::symlink("a.txt", c.join("link"))?;
        assert!(
            matches!(snapshot(&p.top, &c, &p.base, "refs/hivequeen/t/cells/link", "x"), Err(Error::Step(ref m)) if m.contains("symlink"))
        );
        Ok(())
    }

    #[test]
    fn independent_cells_merge_and_conflicts_keep_the_tip() -> TestResult {
        let (r, runs) = (repo()?, tempfile::tempdir()?);
        let p = prepare(r.path())?;
        let edit = |name: &str, file: &str, body: &str| -> Result<String> {
            let c = cell(&p.top, runs.path(), name, &p.base)?;
            fs::write(c.join(file), body)?;
            snapshot(
                &p.top,
                &c,
                &p.base,
                &format!("refs/hivequeen/t/cells/{name}"),
                name,
            )
        };
        let first = edit("first", "a.txt", "one\nFIRST\nthree\n")?;
        let second = edit("second", "b.txt", "new file\n")?;
        let third = edit("third", "a.txt", "one\nTHIRD\nthree\n")?;
        let Merge::Clean(tip) = integrate(&p.top, &p.base, &p.base, &first, "first")? else {
            panic!("first must fast-forward")
        };
        assert_eq!(
            tip, first,
            "an unchanged tip fast-forwards to the cell commit"
        );
        let Merge::Clean(tip) = integrate(&p.top, &tip, &p.base, &second, "second")? else {
            panic!("independent files merge")
        };
        assert_eq!(
            git(&p.top, &["show", &format!("{tip}:a.txt")])?,
            "one\nFIRST\nthree"
        );
        assert_eq!(git(&p.top, &["show", &format!("{tip}:b.txt")])?, "new file");
        match integrate(&p.top, &tip, &p.base, &third, "third")? {
            Merge::Conflict(files) => assert_eq!(files, vec!["a.txt".to_string()]),
            Merge::Clean(_) => panic!("overlapping edits must conflict"),
        }
        Ok(())
    }

    #[test]
    fn patch_applies_to_a_clean_clone() -> TestResult {
        let (r, runs) = (repo()?, tempfile::tempdir()?);
        let p = prepare(r.path())?;
        let c = cell(&p.top, runs.path(), "edit", &p.base)?;
        fs::write(c.join("a.txt"), "changed\n")?;
        fs::write(c.join("bin.dat"), [0u8, 255, 1, 2])?;
        fs::remove_file(c.join("gone.txt"))?;
        let commit = snapshot(&p.top, &c, &p.base, "refs/hivequeen/t/cells/edit", "edit")?;
        let bytes = patch(&p.top, &p.base, &commit)?;
        let clone = tempfile::tempdir()?;
        git(
            clone.path(),
            &[
                OsStr::new("clone"),
                OsStr::new("-q"),
                r.path().as_os_str(),
                OsStr::new("c"),
            ],
        )?;
        let target = clone.path().join("c");
        fs::write(runs.path().join("changes.patch"), &bytes)?;
        git(
            &target,
            &[
                OsStr::new("apply"),
                runs.path().join("changes.patch").as_os_str(),
            ],
        )?;
        assert_eq!(fs::read(target.join("bin.dat"))?, [0u8, 255, 1, 2]);
        assert!(!target.join("gone.txt").exists());
        Ok(())
    }
}
