use crate::backend::Backend;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_STEPS: usize = 16;
const MAX_PARALLEL: usize = 8;
const MAX_TIMEOUT: u64 = 86_400;
const STEP_TIMEOUT: u64 = 1_200;
/// Directory inside a worktree that holds step inputs and never enters a snapshot.
pub const RESERVED: &str = ".hivequeen";

fn default_parallel() -> usize {
    2
}

fn default_timeout() -> u64 {
    3_600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub name: String,
    #[serde(default = "default_parallel")]
    pub parallel: usize,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub prompt: Option<String>,
    pub backend: Option<Backend>,
    pub model: Option<String>,
    pub exec: Option<Vec<String>>,
    pub gate: Option<String>,
    #[serde(default)]
    pub edit: bool,
    pub timeout_secs: Option<u64>,
}

pub enum Kind<'a> {
    Agent(&'a str),
    Exec(&'a [String]),
    Gate(&'a str),
}

impl Step {
    /// Only valid after `validate`, which guarantees exactly one kind.
    pub fn kind(&self) -> Kind<'_> {
        match (&self.prompt, &self.exec, &self.gate) {
            (Some(p), _, _) => Kind::Agent(p),
            (_, Some(argv), _) => Kind::Exec(argv),
            (_, _, Some(g)) => Kind::Gate(g),
            _ => unreachable!("validate guarantees one of prompt, exec, or gate"),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend.unwrap_or(Backend::Droid)
    }

    pub fn timeout(&self) -> u64 {
        self.timeout_secs.unwrap_or(STEP_TIMEOUT)
    }

    fn is_check(&self) -> bool {
        self.exec.is_some() && !self.edit
    }
}

/// A declared input file, snapshotted into the run under `name`.
#[derive(Debug, PartialEq)]
pub struct Resource {
    pub name: String,
    pub source: PathBuf,
}

pub fn parse(text: &str) -> Result<Workflow> {
    toml::from_str(text).map_err(|e| Error::Invalid(e.to_string()))
}

/// Reads, parses, and validates a workflow file. Returns the raw text too, so
/// the run can snapshot exactly what was validated.
pub fn load(path: &Path) -> Result<(Workflow, String)> {
    let text =
        fs::read_to_string(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let workflow = parse(&text)?;
    validate(&workflow)?;
    Ok((workflow, text))
}

pub fn validate(workflow: &Workflow) -> Result<()> {
    let bad = |m: String| Err(Error::Invalid(m));
    if workflow.name.trim().is_empty() {
        return bad("name must not be empty".into());
    }
    if workflow.steps.is_empty() || workflow.steps.len() > MAX_STEPS {
        return bad(format!("a workflow needs 1 to {MAX_STEPS} steps"));
    }
    if !(1..=MAX_PARALLEL).contains(&workflow.parallel) {
        return bad(format!("parallel must be 1 to {MAX_PARALLEL}"));
    }
    check_timeout("timeout_secs", workflow.timeout_secs)?;
    let mut ids = HashSet::new();
    let mut keys = HashSet::new();
    for s in &workflow.steps {
        if !valid_id(&s.id) {
            return bad(format!(
                "step id {:?} must match [a-z][a-z0-9_-]{{0,63}} and must not be \"answer\"",
                s.id
            ));
        }
        if !ids.insert(s.id.as_str()) {
            return bad(format!("duplicate step id {:?}", s.id));
        }
        if !keys.insert(env_key(&s.id)) {
            return bad(format!(
                "step id {:?} collides with another id once - becomes _",
                s.id
            ));
        }
    }
    let mut names = HashMap::new();
    for s in &workflow.steps {
        let id = &s.id;
        let kinds = [s.prompt.is_some(), s.exec.is_some(), s.gate.is_some()];
        if kinds.iter().filter(|k| **k).count() != 1 {
            return bad(format!(
                "step {id}: set exactly one of prompt, exec, or gate"
            ));
        }
        if s.prompt.is_none() && (s.backend.is_some() || s.model.is_some()) {
            return bad(format!(
                "step {id}: backend and model apply only to prompt steps"
            ));
        }
        if s.gate.is_some() && s.edit {
            return bad(format!("step {id}: a gate cannot edit"));
        }
        if s.exec
            .as_ref()
            .is_some_and(|a| a.first().is_none_or(|p| p.is_empty()))
        {
            return bad(format!(
                "step {id}: exec needs a program as its first entry"
            ));
        }
        if let Some(t) = s.timeout_secs {
            check_timeout(&format!("step {id}: timeout_secs"), t)?;
        }
        let mut seen = HashSet::new();
        for need in &s.needs {
            if need == id {
                return bad(format!("step {id} needs itself"));
            }
            if !ids.contains(need.as_str()) {
                return bad(format!("step {id} needs unknown step {need:?}"));
            }
            if !seen.insert(need) {
                return bad(format!("step {id} lists {need:?} twice"));
            }
        }
        for input in &s.inputs {
            let name = input_name(input)?;
            match names.insert(name.clone(), input.clone()) {
                Some(other) if &other != input => {
                    return bad(format!(
                        "inputs {other:?} and {input:?} would both be copied as {name}"
                    ));
                }
                _ => {}
            }
        }
    }
    if let Some(path) = find_cycle(workflow) {
        return bad(format!("dependency cycle: {}", path.join(" -> ")));
    }
    for (i, s) in workflow.steps.iter().enumerate() {
        if s.edit
            && !descendants(workflow, i)
                .iter()
                .any(|&d| workflow.steps[d].is_check())
        {
            return bad(format!(
                "editing step {} needs a downstream exec check (an exec step without edit that needs it)",
                s.id
            ));
        }
    }
    Ok(())
}

fn check_timeout(what: &str, secs: u64) -> Result<()> {
    if (1..=MAX_TIMEOUT).contains(&secs) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{what} must be 1 to {MAX_TIMEOUT} seconds"
        )))
    }
}

fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    id != "answer"
        && (1..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn input_name(input: &str) -> Result<String> {
    Path::new(input)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| Error::Invalid(format!("input {input:?} does not name a file")))
}

fn find_cycle(workflow: &Workflow) -> Option<Vec<String>> {
    fn visit(
        w: &Workflow,
        idx: &HashMap<&str, usize>,
        i: usize,
        color: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<String>> {
        color[i] = 1;
        stack.push(i);
        for need in &w.steps[i].needs {
            let Some(&j) = idx.get(need.as_str()) else {
                continue;
            };
            if color[j] == 1 {
                let from = stack.iter().position(|&k| k == j).unwrap_or(0);
                let mut path: Vec<String> = stack[from..]
                    .iter()
                    .map(|&k| w.steps[k].id.clone())
                    .collect();
                path.push(w.steps[j].id.clone());
                return Some(path);
            }
            if color[j] == 0
                && let Some(path) = visit(w, idx, j, color, stack)
            {
                return Some(path);
            }
        }
        stack.pop();
        color[i] = 2;
        None
    }
    let idx = index(workflow);
    let mut color = vec![0; workflow.steps.len()];
    for i in 0..workflow.steps.len() {
        if color[i] == 0
            && let Some(path) = visit(workflow, &idx, i, &mut color, &mut Vec::new())
        {
            return Some(path);
        }
    }
    None
}

/// Stable dependency order: repeatedly take the first declared step whose
/// needs are already ordered. Equals declaration order when the file already
/// lists dependencies first.
pub fn order(workflow: &Workflow) -> Vec<usize> {
    let idx = index(workflow);
    let n = workflow.steps.len();
    let mut placed = vec![false; n];
    let mut out = Vec::with_capacity(n);
    while let Some(i) = (0..n).find(|&i| {
        !placed[i]
            && workflow.steps[i]
                .needs
                .iter()
                .all(|x| idx.get(x.as_str()).is_some_and(|&j| placed[j]))
    }) {
        placed[i] = true;
        out.push(i);
    }
    out
}

/// Resolves declared input files against the workflow directory. Only regular
/// files are accepted; symlinks and directories are refused.
pub fn resources(workflow: &Workflow, dir: &Path) -> Result<Vec<Resource>> {
    let mut out: Vec<Resource> = Vec::new();
    for input in workflow.steps.iter().flat_map(|s| &s.inputs) {
        let name = input_name(input)?;
        if out.iter().any(|r| r.name == name) {
            continue;
        }
        let source = dir.join(input);
        let meta = fs::symlink_metadata(&source)
            .map_err(|e| Error::Invalid(format!("input {}: {e}", source.display())))?;
        if !meta.file_type().is_file() {
            return Err(Error::Invalid(format!(
                "input {} must be a regular file, not a symlink or directory",
                source.display()
            )));
        }
        out.push(Resource { name, source });
    }
    Ok(out)
}
pub fn index(workflow: &Workflow) -> HashMap<&str, usize> {
    workflow
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect()
}

pub fn ancestors(workflow: &Workflow, start: usize) -> Vec<usize> {
    let idx = index(workflow);
    let mut seen = vec![false; workflow.steps.len()];
    let mut stack = vec![start];
    while let Some(i) = stack.pop() {
        for need in &workflow.steps[i].needs {
            if let Some(&j) = idx.get(need.as_str())
                && !seen[j]
            {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    (0..seen.len()).filter(|&i| seen[i]).collect()
}

pub fn descendants(workflow: &Workflow, start: usize) -> Vec<usize> {
    (0..workflow.steps.len())
        .filter(|&j| ancestors(workflow, j).contains(&start))
        .collect()
}

/// Whether a step runs inside a cell: it edits, or it inspects edited code.
pub fn uses_tree(workflow: &Workflow, i: usize) -> bool {
    let step = &workflow.steps[i];
    step.gate.is_none()
        && (step.edit
            || ancestors(workflow, i)
                .iter()
                .any(|&a| workflow.steps[a].edit))
}

pub fn env_key(id: &str) -> String {
    id.to_ascii_uppercase().replace('-', "_")
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub droid: DroidSettings,
    #[serde(default)]
    pub claude: ClaudeSettings,
    #[serde(default)]
    pub codex: CodexSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroidSettings {
    pub program: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub edit_autonomy: Option<Autonomy>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSettings {
    pub program: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSettings {
    pub program: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// Loads `hivequeen.toml`. An explicit path must exist; the default location
/// (`$XDG_CONFIG_HOME/hivequeen/hivequeen.toml`) is optional.
pub fn load_settings(explicit: Option<&Path>) -> Result<Settings> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let base = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
            match base.map(|b| b.join("hivequeen/hivequeen.toml")) {
                Some(p) if p.exists() => p,
                _ => return Ok(Settings::default()),
            }
        }
    };
    let text = fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(text: &str) -> String {
        match parse(text).and_then(|w| validate(&w)) {
            Err(Error::Invalid(m)) => m,
            other => panic!("expected an invalid workflow, got {other:?}"),
        }
    }

    fn workflow(steps: &str) -> Workflow {
        let w = parse(&format!("name = \"t\"\n{steps}")).expect("test workflow parses");
        validate(&w).expect("test workflow is valid");
        w
    }

    #[test]
    fn unknown_fields_fail() {
        let m = invalid("name = \"t\"\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\nretries = 3\n");
        assert!(m.contains("retries"), "message names the field: {m}");
    }

    #[test]
    fn unknown_dependency_fails() {
        let m = invalid(
            "name = \"t\"\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\nneeds = [\"ghost\"]\n",
        );
        assert!(m.contains("ghost"), "{m}");
    }

    #[test]
    fn cycle_reports_path() {
        let m = invalid(
            "name = \"t\"\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\nneeds = [\"b\"]\n\
             [[steps]]\nid = \"b\"\nexec = [\"true\"]\nneeds = [\"a\"]\n",
        );
        assert!(m.contains("a -> b -> a"), "{m}");
    }

    #[test]
    fn duplicate_id_fails() {
        let m = invalid(
            "name = \"t\"\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\n",
        );
        assert!(m.contains("duplicate"), "{m}");
    }

    #[test]
    fn env_key_collision_fails() {
        let m = invalid(
            "name = \"t\"\n[[steps]]\nid = \"a-b\"\nexec = [\"true\"]\n[[steps]]\nid = \"a_b\"\nexec = [\"true\"]\n",
        );
        assert!(m.contains("collides"), "{m}");
    }

    #[test]
    fn reserved_and_malformed_ids_fail() {
        for id in ["answer", "A", "1a", "", "a b"] {
            let m = invalid(&format!(
                "name = \"t\"\n[[steps]]\nid = \"{id}\"\nexec = [\"true\"]\n"
            ));
            assert!(m.contains("step id"), "id {id:?}: {m}");
        }
    }

    #[test]
    fn defaults_are_bounded() {
        let w = workflow("[[steps]]\nid = \"a\"\nprompt = \"hi\"\n");
        assert_eq!(w.parallel, 2);
        assert_eq!(w.timeout_secs, 3_600);
        assert_eq!(w.steps[0].timeout(), 1_200);
        assert_eq!(w.steps[0].backend(), Backend::Droid);
        assert!(
            invalid("name = \"t\"\nparallel = 9\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\n")
                .contains("parallel")
        );
        assert!(
            invalid("name = \"t\"\ntimeout_secs = 0\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\n")
                .contains("timeout")
        );
        let many: String = (0..17)
            .map(|i| format!("[[steps]]\nid = \"s{i}\"\nexec = [\"true\"]\n"))
            .collect();
        assert!(invalid(&format!("name = \"t\"\n{many}")).contains("1 to 16"));
    }

    #[test]
    fn command_is_not_shell_interpolated() {
        let w = workflow("[[steps]]\nid = \"a\"\nexec = [\"echo\", \"$HOME; rm -rf *\"]\n");
        assert_eq!(
            w.steps[0].exec.as_deref(),
            Some(&["echo".to_string(), "$HOME; rm -rf *".to_string()][..])
        );
    }

    #[test]
    fn exactly_one_kind_is_required() {
        assert!(invalid("name = \"t\"\n[[steps]]\nid = \"a\"\n").contains("exactly one"));
        assert!(
            invalid("name = \"t\"\n[[steps]]\nid = \"a\"\nprompt = \"x\"\nexec = [\"true\"]\n")
                .contains("exactly one")
        );
    }

    #[test]
    fn model_requires_an_agent_step() {
        let m = invalid("name = \"t\"\n[[steps]]\nid = \"a\"\nexec = [\"true\"]\nmodel = \"m\"\n");
        assert!(m.contains("prompt steps"), "{m}");
    }

    #[test]
    fn editing_step_needs_a_downstream_check() {
        let m = invalid("name = \"t\"\n[[steps]]\nid = \"a\"\nprompt = \"edit\"\nedit = true\n");
        assert!(m.contains("check"), "{m}");
        let w = workflow(
            "[[steps]]\nid = \"a\"\nprompt = \"edit\"\nedit = true\n\
             [[steps]]\nid = \"t\"\nexec = [\"true\"]\nneeds = [\"a\"]\n",
        );
        assert!(uses_tree(&w, 0) && uses_tree(&w, 1));
    }

    #[test]
    fn order_is_stable_dependency_order() {
        let w = workflow(
            "[[steps]]\nid = \"late\"\nexec = [\"true\"]\nneeds = [\"early\"]\n\
             [[steps]]\nid = \"other\"\nexec = [\"true\"]\n\
             [[steps]]\nid = \"early\"\nexec = [\"true\"]\n",
        );
        assert_eq!(order(&w), vec![1, 2, 0]);
    }

    #[test]
    fn resources_resolve_against_the_workflow_directory()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("q.md"), "question")?;
        let w = workflow("[[steps]]\nid = \"a\"\nprompt = \"x\"\ninputs = [\"q.md\"]\n");
        let found = resources(&w, dir.path())?;
        assert_eq!(
            found,
            vec![Resource {
                name: "q.md".into(),
                source: dir.path().join("q.md")
            }]
        );
        Ok(())
    }

    #[test]
    fn symlink_and_missing_inputs_fail() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("real.md"), "x")?;
        std::os::unix::fs::symlink(dir.path().join("real.md"), dir.path().join("link.md"))?;
        for name in ["link.md", "missing.md"] {
            let w = workflow(&format!(
                "[[steps]]\nid = \"a\"\nprompt = \"x\"\ninputs = [\"{name}\"]\n"
            ));
            assert!(
                matches!(resources(&w, dir.path()), Err(Error::Invalid(_))),
                "{name} must be rejected"
            );
        }
        Ok(())
    }

    #[test]
    fn colliding_input_names_fail() {
        let m = invalid(
            "name = \"t\"\n[[steps]]\nid = \"a\"\nprompt = \"x\"\ninputs = [\"a/q.md\", \"b/q.md\"]\n",
        );
        assert!(m.contains("q.md"), "{m}");
    }

    #[test]
    fn autonomy_levels_deserialize() {
        for (raw, expected) in [
            ("low", Autonomy::Low),
            ("medium", Autonomy::Medium),
            ("high", Autonomy::High),
        ] {
            let s: Settings =
                toml::from_str(&format!("[droid]\nedit_autonomy = \"{raw}\"\n")).unwrap();
            assert_eq!(s.droid.edit_autonomy, Some(expected));
        }
    }
}
