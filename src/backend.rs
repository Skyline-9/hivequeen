use crate::config::Settings;
use crate::process::{self, Invocation};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Droid,
    Claude,
    Codex,
}

/// What an installed CLI reports about itself. Optional flags are used only
/// when its help text lists them.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub version: String,
    pub help: String,
}

impl Capabilities {
    pub fn supports(&self, flag: &str) -> bool {
        self.help
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '=' | '<'))
            .any(|word| word == flag)
    }
}

pub struct Request<'a> {
    pub prompt: &'a Path,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    pub edit: bool,
    /// Whether `cwd` is inside a Git worktree.
    pub git: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    pub text: String,
    pub session: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Value>,
    pub diagnostics: Vec<String>,
}

impl Backend {
    pub const ALL: [Backend; 3] = [Backend::Droid, Backend::Claude, Backend::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Droid => "droid",
            Backend::Claude => "claude",
            Backend::Codex => "codex",
        }
    }

    pub fn program(self, s: &Settings) -> String {
        let configured = match self {
            Backend::Droid => &s.droid.program,
            Backend::Claude => &s.claude.program,
            Backend::Codex => &s.codex.program,
        };
        configured.clone().unwrap_or_else(|| self.name().to_owned())
    }

    pub fn configured_model(self, s: &Settings) -> Option<&str> {
        match self {
            Backend::Droid => s.droid.model.as_deref(),
            Backend::Claude => s.claude.model.as_deref(),
            Backend::Codex => s.codex.model.as_deref(),
        }
    }

    /// Flags every invocation of this backend depends on.
    pub fn required(self) -> &'static [&'static str] {
        match self {
            Backend::Droid => &["--output-format", "--file", "--cwd"],
            Backend::Claude => &["--print", "--output-format", "--permission-mode"],
            Backend::Codex => &["--ask-for-approval", "--json", "--sandbox", "--cd"],
        }
    }

    fn help_args(self) -> &'static [&'static [&'static str]] {
        match self {
            Backend::Droid => &[&["exec", "--help"]],
            Backend::Claude => &[&["--help"]],
            Backend::Codex => &[&["--help"], &["exec", "--help"]],
        }
    }

    /// Reads version and help text. Never starts a model request.
    pub fn probe(self, s: &Settings, scratch: &Path) -> Result<Capabilities> {
        let program = self.program(s);
        let (exit, version) = process::capture(&program, &["--version"], scratch)?;
        if !exit.success() {
            return Err(Error::Step(format!("{program} --version failed ({exit})")));
        }
        let mut help = String::new();
        for args in self.help_args() {
            help.push_str(&process::capture(&program, args, scratch)?.1);
            help.push('\n');
        }
        let version = version.lines().next().unwrap_or("").trim().to_owned();
        Ok(Capabilities { version, help })
    }

    pub fn invocation(
        self,
        req: &Request<'_>,
        s: &Settings,
        caps: &Capabilities,
    ) -> Result<Invocation> {
        let need = |flag: &str| {
            if caps.supports(flag) {
                Ok(())
            } else {
                Err(Error::Step(format!(
                    "this {} does not support {flag}",
                    self.name()
                )))
            }
        };
        for flag in self.required() {
            need(flag)?;
        }
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut inv = Invocation::new(self.program(s), req.cwd);
        let model = req.model.or(self.configured_model(s));
        match self {
            Backend::Droid => {
                inv.arg("exec")
                    .arg("--cwd")
                    .arg(path(req.cwd))
                    .arg("--output-format")
                    .arg("json");
                inv.arg("--file").arg(path(req.prompt));
                if req.edit {
                    need("--auto")?;
                    let level = match s.droid.edit_autonomy.unwrap_or_default() {
                        crate::config::Autonomy::Low => "low",
                        crate::config::Autonomy::Medium => "medium",
                        crate::config::Autonomy::High => "high",
                    };
                    inv.arg("--auto").arg(level);
                }
                if let Some(effort) = &s.droid.reasoning_effort {
                    need("--reasoning-effort")?;
                    inv.arg("--reasoning-effort").arg(effort);
                }
            }
            Backend::Claude => {
                let mode = if req.edit { "acceptEdits" } else { "dontAsk" };
                inv.arg("--print")
                    .arg("--output-format")
                    .arg("json")
                    .arg("--permission-mode")
                    .arg(mode);
                let tools = match (&s.claude.allowed_tools[..], req.edit) {
                    ([], false) => "Read,Grep,Glob".to_owned(),
                    (tools, _) => tools.join(","),
                };
                if !tools.is_empty() {
                    need("--allowedTools")?;
                    inv.arg("--allowedTools").arg(tools);
                }
                if let Some(effort) = &s.claude.effort {
                    need("--effort")?;
                    inv.arg("--effort").arg(effort);
                }
                if let Some(turns) = s.claude.max_turns {
                    need("--max-turns")?;
                    inv.arg("--max-turns").arg(turns.to_string());
                }
                if caps.supports("--no-session-persistence") {
                    inv.arg("--no-session-persistence");
                }
                inv.stdin = Some(req.prompt.to_path_buf());
            }
            Backend::Codex => {
                let sandbox = if req.edit {
                    "workspace-write"
                } else {
                    "read-only"
                };
                inv.arg("--ask-for-approval")
                    .arg("never")
                    .arg("exec")
                    .arg("--json");
                inv.arg("--sandbox")
                    .arg(sandbox)
                    .arg("--cd")
                    .arg(path(req.cwd));
                if !req.git {
                    need("--skip-git-repo-check")?;
                    inv.arg("--skip-git-repo-check");
                }
                if caps.supports("--ephemeral") {
                    inv.arg("--ephemeral");
                }
                if let Some(effort) = &s.codex.reasoning_effort {
                    need("--config")?;
                    inv.arg("--config")
                        .arg(format!("model_reasoning_effort=\"{effort}\""));
                }
                inv.stdin = Some(req.prompt.to_path_buf());
            }
        }
        if let Some(model) = model {
            need("--model")?;
            inv.arg("--model").arg(model);
        }
        if self == Backend::Codex {
            inv.arg("-");
        }
        Ok(inv)
    }

    /// Parses machine output. Exit status is checked by the caller first;
    /// exit 0 with a failed result is still a failure.
    pub fn parse(self, raw: &str) -> Result<Answer> {
        match self {
            Backend::Droid | Backend::Claude => parse_envelope(raw),
            Backend::Codex => parse_events(raw),
        }
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn parse_envelope(raw: &str) -> Result<Answer> {
    let last = raw
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let v: Value = serde_json::from_str(raw.trim())
        .or_else(|_| serde_json::from_str(last))
        .map_err(|e| Error::Step(format!("malformed result: {e}")))?;
    if v["type"] != "result" {
        return Err(Error::Step("malformed result: no result envelope".into()));
    }
    let result = v["result"].as_str().unwrap_or("");
    if v["is_error"] != false || v["subtype"] != "success" {
        let subtype = v["subtype"].as_str().unwrap_or("unknown");
        return Err(Error::Step(format!(
            "backend reported failure ({subtype}): {result}"
        )));
    }
    if result.trim().is_empty() {
        return Err(Error::Step("backend returned an empty answer".into()));
    }
    let mut diagnostics = Vec::new();
    if let Some(denials) = v["permission_denials"].as_array().filter(|d| !d.is_empty()) {
        diagnostics.push(format!("{} permission denial(s)", denials.len()));
    }
    let model = text(&v["model"]).or_else(|| {
        let names: Vec<&str> = v["modelUsage"]
            .as_object()?
            .keys()
            .map(String::as_str)
            .collect();
        (!names.is_empty()).then(|| names.join(","))
    });
    Ok(Answer {
        text: result.to_owned(),
        session: text(&v["session_id"]),
        model,
        usage: v.get("usage").cloned(),
        diagnostics,
    })
}

fn parse_events(raw: &str) -> Result<Answer> {
    let mut answer = Answer::default();
    let mut completed = false;
    for (n, line) in raw
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| Error::Step(format!("malformed JSONL at line {}: {e}", n + 1)))?;
        match v["type"].as_str().unwrap_or("") {
            "thread.started" => answer.session = text(&v["thread_id"]),
            "item.completed" if v["item"]["type"] == "agent_message" => {
                answer.text = v["item"]["text"].as_str().unwrap_or("").to_owned();
            }
            "turn.completed" => {
                completed = true;
                answer.usage = v.get("usage").cloned();
            }
            "turn.failed" => {
                let why = v["error"]["message"].as_str().unwrap_or("unknown error");
                return Err(Error::Step(format!("codex turn failed: {why}")));
            }
            "error" => answer
                .diagnostics
                .push(v["message"].as_str().unwrap_or("error event").to_owned()),
            _ => {}
        }
    }
    if !completed {
        return Err(Error::Step("codex output has no completed turn".into()));
    }
    if answer.text.trim().is_empty() {
        return Err(Error::Step("codex returned an empty answer".into()));
    }
    Ok(answer)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Autonomy, ClaudeSettings, CodexSettings, DroidSettings};
    use std::path::PathBuf;

    const DROID_OK: &str = include_str!("../tests/fixtures/droid-success.json");
    const DROID_ERROR: &str = include_str!("../tests/fixtures/droid-error.json");
    const CLAUDE_OK: &str = include_str!("../tests/fixtures/claude-success.json");
    const CLAUDE_DENIED: &str = include_str!("../tests/fixtures/claude-denied.json");
    const CODEX_OK: &str = include_str!("../tests/fixtures/codex-success.jsonl");
    const CODEX_FAILED: &str = include_str!("../tests/fixtures/codex-failed.jsonl");
    const CODEX_RECOVERED: &str = include_str!("../tests/fixtures/codex-recovered.jsonl");

    fn caps(flags: &str) -> Capabilities {
        Capabilities {
            version: "1.0".into(),
            help: flags.into(),
        }
    }

    fn all_caps() -> Capabilities {
        caps(
            "--output-format --file --cwd --auto --model --reasoning-effort --print --permission-mode \
              --allowedTools --effort --max-turns --no-session-persistence --ask-for-approval --json \
              --sandbox --cd --skip-git-repo-check --ephemeral --config",
        )
    }

    fn argv(
        b: Backend,
        edit: bool,
        git: bool,
        model: Option<&str>,
        s: &Settings,
    ) -> Result<Vec<String>> {
        let (prompt, cwd) = (PathBuf::from("/run/prompt.md"), PathBuf::from("/run/work"));
        let req = Request {
            prompt: &prompt,
            cwd: &cwd,
            model,
            edit,
            git,
        };
        b.invocation(&req, s, &all_caps()).map(|inv| inv.argv())
    }

    fn has(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn droid_research_is_read_only() -> Result<()> {
        let a = argv(Backend::Droid, false, false, None, &Settings::default())?;
        assert!(
            has(
                &a,
                &[
                    "droid",
                    "exec",
                    "--cwd",
                    "/run/work",
                    "--output-format",
                    "json",
                    "--file",
                    "/run/prompt.md"
                ]
            ),
            "{a:?}"
        );
        assert!(
            !a.iter()
                .any(|x| x == "--auto" || x == "--skip-permissions-unsafe"),
            "{a:?}"
        );
        Ok(())
    }

    #[test]
    fn droid_edit_uses_configured_autonomy() -> Result<()> {
        assert!(has(
            &argv(Backend::Droid, true, true, None, &Settings::default())?,
            &["--auto", "low"]
        ));
        let s = Settings {
            droid: DroidSettings {
                edit_autonomy: Some(Autonomy::Medium),
                reasoning_effort: Some("high".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let a = argv(Backend::Droid, true, true, None, &s)?;
        assert!(
            has(&a, &["--auto", "medium"]) && has(&a, &["--reasoning-effort", "high"]),
            "{a:?}"
        );
        let s_high = Settings {
            droid: DroidSettings {
                edit_autonomy: Some(Autonomy::High),
                ..Default::default()
            },
            ..Default::default()
        };
        let a_high = argv(Backend::Droid, true, true, None, &s_high)?;
        assert!(has(&a_high, &["--auto", "high"]), "{a_high:?}");
        Ok(())
    }

    #[test]
    fn claude_reads_prompt_from_stdin_without_approvals() -> Result<()> {
        let (prompt, cwd) = (PathBuf::from("/p.md"), PathBuf::from("/w"));
        let req = Request {
            prompt: &prompt,
            cwd: &cwd,
            model: None,
            edit: false,
            git: false,
        };
        let inv = Backend::Claude.invocation(&req, &Settings::default(), &all_caps())?;
        assert_eq!(inv.stdin.as_deref(), Some(Path::new("/p.md")));
        assert_eq!(inv.cwd, cwd);
        let a = inv.argv();
        assert!(
            has(
                &a,
                &[
                    "--print",
                    "--output-format",
                    "json",
                    "--permission-mode",
                    "dontAsk"
                ]
            ),
            "{a:?}"
        );
        assert!(has(&a, &["--allowedTools", "Read,Grep,Glob"]), "{a:?}");
        Ok(())
    }

    #[test]
    fn claude_edit_accepts_edits_with_configured_tools() -> Result<()> {
        let s = Settings {
            claude: ClaudeSettings {
                allowed_tools: vec!["Bash(cargo test *)".into()],
                max_turns: Some(8),
                ..Default::default()
            },
            ..Default::default()
        };
        let a = argv(Backend::Claude, true, true, None, &s)?;
        assert!(
            has(&a, &["--permission-mode", "acceptEdits"])
                && has(&a, &["--allowedTools", "Bash(cargo test *)"])
                && has(&a, &["--max-turns", "8"]),
            "{a:?}"
        );
        Ok(())
    }

    #[test]
    fn codex_sandbox_follows_edit_mode() -> Result<()> {
        let read = argv(Backend::Codex, false, false, None, &Settings::default())?;
        assert!(
            has(
                &read,
                &[
                    "codex",
                    "--ask-for-approval",
                    "never",
                    "exec",
                    "--json",
                    "--sandbox",
                    "read-only",
                    "--cd",
                    "/run/work"
                ]
            ),
            "{read:?}"
        );
        assert!(
            read.iter().any(|x| x == "--skip-git-repo-check")
                && read.last().map(String::as_str) == Some("-"),
            "{read:?}"
        );
        let s = Settings {
            codex: CodexSettings {
                reasoning_effort: Some("high".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let edit = argv(Backend::Codex, true, true, None, &s)?;
        assert!(
            has(&edit, &["--sandbox", "workspace-write"])
                && !edit.iter().any(|x| x.contains("danger")),
            "{edit:?}"
        );
        assert!(
            !edit.iter().any(|x| x == "--skip-git-repo-check")
                && has(&edit, &["--config", "model_reasoning_effort=\"high\""]),
            "{edit:?}"
        );
        Ok(())
    }

    #[test]
    fn models_pass_through_opaquely_and_sessions_are_fresh() -> Result<()> {
        for b in Backend::ALL {
            let a = argv(
                b,
                false,
                false,
                Some("custom:vendor/Model-X"),
                &Settings::default(),
            )?;
            assert!(
                has(&a, &["--model", "custom:vendor/Model-X"]),
                "{b:?}: {a:?}"
            );
            for banned in [
                "-s",
                "--session-id",
                "--fork",
                "--resume",
                "--continue",
                "resume",
                "--mission",
            ] {
                assert!(
                    !a.iter().any(|x| x == banned),
                    "{b:?} must not pass {banned}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn step_model_overrides_configured_model() -> Result<()> {
        let s = Settings {
            droid: DroidSettings {
                model: Some("configured".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(has(
            &argv(Backend::Droid, false, false, None, &s)?,
            &["--model", "configured"]
        ));
        assert!(has(
            &argv(Backend::Droid, false, false, Some("chosen"), &s)?,
            &["--model", "chosen"]
        ));
        Ok(())
    }

    #[test]
    fn unsupported_capability_fails() {
        let (prompt, cwd) = (PathBuf::from("/p"), PathBuf::from("/w"));
        let req = Request {
            prompt: &prompt,
            cwd: &cwd,
            model: Some("m"),
            edit: false,
            git: false,
        };
        let result = Backend::Droid.invocation(
            &req,
            &Settings::default(),
            &caps("--output-format --file --cwd"),
        );
        assert!(
            matches!(result, Err(Error::Step(ref m)) if m.contains("--model")),
            "{result:?}"
        );
    }

    #[test]
    fn envelopes_parse_session_and_usage() -> Result<()> {
        let a = Backend::Droid.parse(DROID_OK)?;
        assert_eq!(a.text, "The regression comes from the cache key.");
        assert_eq!(
            a.session.as_deref(),
            Some("8af22e0a-d222-42c6-8c7e-7a059e391b0b")
        );
        assert!(a.usage.is_some());
        let c = Backend::Claude.parse(CLAUDE_OK)?;
        assert_eq!(c.model.as_deref(), Some("claude-sonnet-5"));
        Ok(())
    }

    #[test]
    fn missing_usage_is_unknown_not_zero() -> Result<()> {
        let a = Backend::Droid
            .parse(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#)?;
        assert_eq!(a.usage, None);
        Ok(())
    }

    #[test]
    fn reported_errors_fail_despite_exit_zero() {
        assert!(matches!(
            Backend::Droid.parse(DROID_ERROR),
            Err(Error::Step(_))
        ));
        match Backend::Claude.parse(CLAUDE_DENIED) {
            Err(Error::Step(m)) => assert!(m.contains("error_max_turns"), "{m}"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn malformed_and_empty_output_fail() {
        for raw in [
            "",
            "not json",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"  "}"#,
            r#"{"result":"no envelope"}"#,
        ] {
            assert!(
                matches!(Backend::Claude.parse(raw), Err(Error::Step(_))),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn codex_success_keeps_last_message_and_usage() -> Result<()> {
        let a = Backend::Codex.parse(CODEX_OK)?;
        assert_eq!(a.text, "Repo contains docs, sdk, and examples directories.");
        assert_eq!(a.session.as_deref(), Some("thread_123"));
        assert_eq!(
            a.usage.as_ref().and_then(|u| u["output_tokens"].as_u64()),
            Some(122)
        );
        Ok(())
    }

    #[test]
    fn codex_turn_failure_fails() {
        assert!(
            matches!(Backend::Codex.parse(CODEX_FAILED), Err(Error::Step(ref m)) if m.contains("rate limited"))
        );
    }

    #[test]
    fn codex_without_terminal_turn_fails() {
        let partial: String = CODEX_OK
            .lines()
            .filter(|l| !l.contains("turn.completed"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(
            matches!(Backend::Codex.parse(&partial), Err(Error::Step(ref m)) if m.contains("completed turn"))
        );
    }

    #[test]
    fn codex_truncated_line_fails() {
        let truncated = format!("{CODEX_OK}{{\"type\":\"item.comp");
        assert!(
            matches!(Backend::Codex.parse(&truncated), Err(Error::Step(ref m)) if m.contains("malformed"))
        );
    }

    #[test]
    fn codex_recoverable_errors_and_unknown_events_are_kept() -> Result<()> {
        let a = Backend::Codex.parse(CODEX_RECOVERED)?;
        assert_eq!(a.text, "Done after a retry.");
        assert_eq!(
            a.diagnostics,
            vec!["stream disconnected; retrying".to_string()]
        );
        Ok(())
    }
}
