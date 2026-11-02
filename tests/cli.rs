mod common;

use common::{Env, TestResult, bin, run_dir, text};
use std::fs;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

const TWO_STEPS: &str = "name = \"cmd\"\n[[steps]]\nid = \"a\"\nexec = [\"/bin/echo\", \"hello\"]\n\
                         [[steps]]\nid = \"b\"\nneeds = [\"a\"]\nexec = [\"/bin/sh\", \"-c\", \"cat \\\"$HIVEQUEEN_INPUT_A_ANSWER\\\"\"]\n";

#[test]
fn check_accepts_valid_and_rejects_invalid_workflows() -> TestResult {
    let env = Env::new()?;
    let good = env.write("good.toml", TWO_STEPS)?;
    let out = env.cmd().arg("check").arg(&good).output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("ok: cmd: 2 step(s)"));
    let bad = env.write(
        "bad.toml",
        "name = \"x\"\n[[steps]]\nid = \"a\"\nprompt = \"p\"\nexec = [\"true\"]\n",
    )?;
    let out = env.cmd().arg("check").arg(&bad).output()?;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    Ok(())
}

#[test]
fn command_workflow_succeeds_and_reports() -> TestResult {
    let env = Env::new()?;
    let wf = env.write("wf.toml", TWO_STEPS)?;
    let out = env.cmd().arg("run").arg(&wf).output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let dir = run_dir(&out);
    assert_eq!(
        fs::read_to_string(dir.join("steps/b/answer.md"))?,
        "hello\n"
    );
    let status = env.cmd().arg("status").arg(&dir).output()?;
    assert!(text(&status).contains("Succeeded"), "{}", text(&status));
    let json = env.cmd().args(["status", "--json"]).arg(&dir).output()?;
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(parsed["steps"][1]["state"], "Succeeded");
    let report = env.cmd().arg("report").arg(&dir).output()?;
    assert!(
        text(&report).contains("Outcome: succeeded"),
        "{}",
        text(&report)
    );
    Ok(())
}

#[test]
fn failed_check_exits_one() -> TestResult {
    let env = Env::new()?;
    let wf = env.write(
        "wf.toml",
        "name = \"f\"\n[[steps]]\nid = \"a\"\nexec = [\"/bin/sh\", \"-c\", \"exit 7\"]\n",
    )?;
    let out = env.cmd().arg("run").arg(&wf).output()?;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(fs::read_to_string(run_dir(&out).join("decision.md"))?.contains("exit 7"));
    Ok(())
}

#[test]
fn offline_approval_then_resume() -> TestResult {
    let env = Env::new()?;
    let wf = env.write("wf.toml", "name = \"g\"\n[[steps]]\nid = \"ok\"\ngate = \"Proceed?\"\n[[steps]]\nid = \"after\"\nneeds = [\"ok\"]\nexec = [\"/bin/echo\", \"done\"]\n")?;
    let out = env.cmd().arg("run").arg(&wf).output()?;
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let dir = run_dir(&out);
    let approve = env.cmd().arg("approve").arg(&dir).arg("ok").output()?;
    assert_eq!(approve.status.code(), Some(0), "{}", text(&approve));
    let resumed = env.cmd().arg("resume").arg(&dir).output()?;
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed));
    let wrong = env.cmd().arg("approve").arg(&dir).arg("ok").output()?;
    assert_eq!(
        wrong.status.code(),
        Some(2),
        "approving a finished gate is invalid: {}",
        text(&wrong)
    );
    Ok(())
}

#[test]
fn mixed_backends_share_one_run() -> TestResult {
    let env = Env::new()?;
    env.write("question.md", "Why is the build slow?")?;
    let wf = env.write(
        "wf.toml",
        "name = \"mixed\"\nparallel = 3\n\
         [[steps]]\nid = \"inspect\"\nprompt = \"Explain.\"\ninputs = [\"question.md\"]\n\
         [[steps]]\nid = \"challenge\"\nbackend = \"codex\"\nprompt = \"Disagree.\"\ninputs = [\"question.md\"]\n\
         [[steps]]\nid = \"decide\"\nbackend = \"claude\"\nneeds = [\"inspect\", \"challenge\"]\nprompt = \"Compare.\"\n\
         [[steps]]\nid = \"check\"\nneeds = [\"decide\"]\nexec = [\"/bin/sh\", \"-c\", \"grep -q claude \\\"$HIVEQUEEN_INPUT_DECIDE_ANSWER\\\"\"]\n",
    )?;
    let log = env.path().join("calls.log");
    let out = env
        .cmd()
        .arg("run")
        .arg(&wf)
        .env("FAKE_LOG", &log)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let dir = run_dir(&out);
    let prompt = fs::read_to_string(dir.join("steps/decide/prompt.md"))?;
    assert!(
        prompt.contains("answer from droid") && prompt.contains("answer from codex"),
        "{prompt}"
    );
    let calls = fs::read_to_string(&log)?;
    let mut kinds: Vec<&str> = calls.lines().filter_map(|l| l.split(' ').next()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["claude", "codex", "droid"], "{calls}");
    let record = fs::read_to_string(dir.join("decision.md"))?;
    assert!(
        record.contains("session t-1") && record.contains("reported fake-claude"),
        "{record}"
    );
    Ok(())
}

#[test]
fn each_backend_output_format_is_parsed() -> TestResult {
    for kind in ["droid", "claude", "codex"] {
        let env = Env::new()?;
        let wf = env.write(
            "wf.toml",
            &format!(
                "name = \"one\"\n[[steps]]\nid = \"a\"\nbackend = \"{kind}\"\nprompt = \"Hi.\"\n"
            ),
        )?;
        let out = env
            .cmd()
            .arg("run")
            .arg(&wf)
            .env("FAKE_KIND", kind)
            .env("FAKE_REPLY", "parsed fine")
            .output()?;
        assert_eq!(out.status.code(), Some(0), "{kind}: {}", text(&out));
        assert_eq!(
            fs::read_to_string(run_dir(&out).join("steps/a/answer.md"))?,
            "parsed fine"
        );
    }
    Ok(())
}

#[test]
fn draft_writes_a_file_and_runs_nothing() -> TestResult {
    let env = Env::new()?;
    let marker = env.path().join("executed");
    let proposal = format!(
        "```toml\nname = \\\"drafted\\\"\n[[steps]]\nid = \\\"touch\\\"\nexec = [\\\"/usr/bin/touch\\\", \\\"{}\\\"]\n```",
        marker.display()
    )
    .replace('\n', "\\n");
    let out_file = env.path().join("proposal.toml");
    let out = env
        .cmd()
        .args(["draft", "make a marker", "--out"])
        .arg(&out_file)
        .env("FAKE_REPLY", &proposal)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Nothing was executed"));
    assert!(fs::read_to_string(&out_file)?.contains("name = \"drafted\""));
    assert!(!marker.exists(), "draft must not execute the proposal");
    let again = env
        .cmd()
        .args(["draft", "x", "--out"])
        .arg(&out_file)
        .output()?;
    assert_eq!(
        again.status.code(),
        Some(2),
        "an existing file is never replaced"
    );
    Ok(())
}

#[test]
fn missing_backend_cli_is_reported() -> TestResult {
    let env = Env::new()?;
    let settings = env.write(
        "missing.toml",
        "[codex]\nprogram = \"/nonexistent/codex\"\n",
    )?;
    let mut c = bin();
    c.arg("--state-dir")
        .arg(env.path().join("state"))
        .arg("--config")
        .arg(&settings);
    let out = c.args(["doctor", "codex"]).output()?;
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(text(&out).contains("codex: unavailable"), "{}", text(&out));
    let wf = env.write(
        "wf.toml",
        "name = \"c\"\n[[steps]]\nid = \"a\"\nbackend = \"codex\"\nprompt = \"x\"\n",
    )?;
    let mut c = bin();
    c.arg("--state-dir")
        .arg(env.path().join("state"))
        .arg("--config")
        .arg(&settings);
    let out = c.arg("run").arg(&wf).output()?;
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    let ok = env.cmd().args(["doctor", "droid"]).output()?;
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
    Ok(())
}

#[test]
fn interrupt_stops_children_and_is_recorded() -> TestResult {
    let env = Env::new()?;
    let pidfile = env.path().join("child.pid");
    let wf = env.write(
        "wf.toml",
        &format!("name = \"i\"\n[[steps]]\nid = \"slow\"\nexec = [\"/bin/sh\", \"-c\", \"echo $$ > {}; sleep 60\"]\n", pidfile.display()),
    )?;
    let child = env
        .cmd()
        .arg("run")
        .arg(&wf)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    while !pidfile.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the step never started"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id())?);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT)?;
    let out = child.wait_with_output()?;
    assert_eq!(out.status.code(), Some(130));
    let step_pid: i32 = fs::read_to_string(&pidfile)?.trim().parse()?;
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(step_pid), None).is_err(),
        "the step's process must be gone"
    );
    let dir = run_dir(&out);
    let status = env.cmd().arg("status").arg(&dir).output()?;
    assert!(text(&status).contains("Interrupted"), "{}", text(&status));
    let resumed = env.cmd().arg("resume").arg(&dir).output()?;
    assert_eq!(
        resumed.status.code(),
        Some(3),
        "an interrupted step is never relaunched: {}",
        text(&resumed)
    );
    Ok(())
}

#[test]
#[ignore = "makes a real Droid model request; run only when asked"]
fn real_droid_smoke() -> TestResult {
    let dir = tempfile::tempdir()?;
    let wf = dir.path().join("wf.toml");
    fs::write(
        &wf,
        "name = \"smoke\"\n[[steps]]\nid = \"a\"\nprompt = \"Reply with the single word: ready\"\n",
    )?;
    let out = bin()
        .arg("--state-dir")
        .arg(dir.path().join("state"))
        .arg("run")
        .arg(&wf)
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        !fs::read_to_string(run_dir(&out).join("steps/a/answer.md"))?
            .trim()
            .is_empty()
    );
    Ok(())
}

#[test]
fn inherited_git_variables_cannot_redirect_to_the_user_repository() -> TestResult {
    let env = Env::new()?;
    let repo = env.path().join("repo");
    fs::create_dir(&repo)?;
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
    };
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.name", "T"],
        &["config", "user.email", "t@example.com"],
        &["config", "commit.gpgsign", "false"],
    ] {
        git(args)?;
    }
    fs::write(repo.join("a.txt"), "a\n")?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "init"])?;
    let index_before = fs::read(repo.join(".git/index"))?;
    let wf = env.write(
        "wf.toml",
        "name = \"e\"\n[[steps]]\nid = \"e\"\nedit = true\nexec = [\"/bin/sh\", \"-c\", 'test \"$(git rev-parse --show-toplevel)\" = \"$PWD\" && echo new > new.txt']\n\
         [[steps]]\nid = \"t\"\nneeds = [\"e\"]\nexec = [\"/bin/sh\", \"-c\", \"test -f new.txt\"]\n",
    )?;
    let out = env
        .cmd()
        .arg("run")
        .arg(&wf)
        .arg("--repo")
        .arg(&repo)
        .env("GIT_DIR", repo.join(".git"))
        .env("GIT_WORK_TREE", &repo)
        .env("GIT_INDEX_FILE", repo.join(".git/index"))
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        fs::read(repo.join(".git/index"))?,
        index_before,
        "the user's index must not change"
    );
    let status = git(&["status", "--porcelain"])?;
    assert!(
        status.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    Ok(())
}
