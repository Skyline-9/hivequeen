use crate::backend::{Backend, Capabilities, Request};
use crate::config::{self, Kind, RESERVED, Settings, Workflow};
use crate::process::{self, Exit, Invocation, Limits};
use crate::store::{self, Approval, Receipt, RunState, State, StepRecord};
use crate::workspace::{self, Merge};
use crate::{Error, Result, at, now_unix};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

const MAX_PROMPT: usize = 128 * 1024;
const POLL: Duration = Duration::from_millis(100);
const DRAFT_TIMEOUT: Duration = Duration::from_secs(1_200);

pub struct Options {
    pub state_dir: Option<PathBuf>,
    pub settings: Settings,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Waiting,
    Interrupted,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Succeeded => 0,
            Outcome::Failed => 1,
            Outcome::Waiting | Outcome::Interrupted => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Succeeded => "succeeded",
            Outcome::Failed => "failed",
            Outcome::Waiting => "waiting for approval",
            Outcome::Interrupted => "interrupted",
        }
    }
}

type Done = (usize, Result<Exit>);

fn workflow_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

pub fn check(path: &Path) -> Result<String> {
    let (wf, _) = config::load(path)?;
    let found = config::resources(&wf, workflow_dir(path))?;
    Ok(format!(
        "{}: {} step(s), {} input file(s), parallel {}",
        wf.name,
        wf.steps.len(),
        found.len(),
        wf.parallel
    ))
}

pub fn run(path: &Path, repo: Option<&Path>, opts: &Options) -> Result<(PathBuf, Outcome)> {
    let (wf, text) = config::load(path)?;
    let found = config::resources(&wf, workflow_dir(path))?;
    let prepared = match (wf.steps.iter().any(|s| s.edit), repo) {
        (true, Some(r)) => Some(workspace::prepare(r)?),
        (true, None) => {
            return Err(Error::Invalid(
                "this workflow has editing steps; pass --repo".into(),
            ));
        }
        (false, Some(_)) => {
            return Err(Error::Invalid(
                "--repo is only used by workflows with edit = true".into(),
            ));
        }
        (false, None) => None,
    };
    let root = store::state_root(opts.state_dir.as_deref())?;
    let caps = probe_needed(&wf, &opts.settings, &root, &vec![true; wf.steps.len()])?;
    let dir = store::new_dir(&root.join("runs"))?;
    let lock = store::lock(&dir)?;
    store::write_atomic(&dir.join("workflow.toml"), text.as_bytes())?;
    let res = dir.join("resources");
    fs::create_dir(&res).map_err(at(&res))?;
    let mut resources = BTreeMap::new();
    for r in &found {
        let bytes = fs::read(&r.source).map_err(at(&r.source))?;
        store::write_atomic(&res.join(&r.name), &bytes)?;
        resources.insert(r.name.clone(), store::digest(&bytes));
    }
    let now = now_unix();
    let base = prepared.as_ref().map(|p| p.base.clone());
    let st = RunState {
        workflow_digest: store::digest(text.as_bytes()),
        resources,
        repo: prepared.map(|p| p.top),
        tip: base.clone(),
        base,
        started_unix: now,
        deadline_unix: now + wf.timeout_secs,
        steps: wf
            .steps
            .iter()
            .map(|s| StepRecord {
                id: s.id.clone(),
                ..StepRecord::default()
            })
            .collect(),
    };
    let mut run = Run::new(dir, wf, st, opts, caps, lock);
    run.save()?;
    run.announce();
    let outcome = run.drive()?;
    Ok((run.dir, outcome))
}

pub fn resume(run_ref: &str, opts: &Options) -> Result<(PathBuf, Outcome)> {
    let dir = find_run(run_ref, opts)?;
    let lock = store::lock(&dir)?;
    let mut st: RunState = store::read_json(&dir.join("state.json"))?;
    let wf_path = dir.join("workflow.toml");
    let text = fs::read_to_string(&wf_path).map_err(at(&wf_path))?;
    if store::digest(text.as_bytes()) != st.workflow_digest {
        return Err(Error::Infra(
            "integrity error: the run's workflow.toml changed".into(),
        ));
    }
    let wf = config::parse(&text)?;
    config::validate(&wf)?;
    for (name, digest) in &st.resources {
        if store::digest_file(&dir.join("resources").join(name))? != *digest {
            return Err(Error::Infra(format!(
                "integrity error: resource {name} changed"
            )));
        }
    }
    for r in &mut st.steps {
        let ambiguous = match r.state {
            State::Running => true,
            State::Pending => dir.join("cells").join(&r.id).exists(),
            State::Integrating => r.cell_commit.is_none(),
            State::Succeeded if store::completed(&dir.join("steps").join(&r.id))?.is_none() => {
                return Err(Error::Infra(format!(
                    "integrity error: step {} is marked succeeded without a receipt",
                    r.id
                )));
            }
            _ => false,
        };
        if ambiguous {
            r.state = State::Interrupted;
            r.detail = Some(
                "the coordinator stopped while this step was running; it is never relaunched"
                    .into(),
            );
        }
    }
    let pending: Vec<bool> = st.steps.iter().map(|r| r.state == State::Pending).collect();
    let root = store::state_root(opts.state_dir.as_deref())?;
    let caps = probe_needed(&wf, &opts.settings, &root, &pending)?;
    let mut run = Run::new(dir, wf, st, opts, caps, lock);
    run.save()?;
    if run.st.steps.iter().any(|r| r.state == State::Interrupted) {
        run.write_report(Outcome::Interrupted)?;
        eprintln!(
            "hivequeen: this run was interrupted mid-step; stop any leftover work and start a new run"
        );
        return Ok((run.dir, Outcome::Interrupted));
    }
    let outcome = run.drive()?;
    Ok((run.dir, outcome))
}

pub fn approve(run_ref: &str, step: &str, opts: &Options) -> Result<String> {
    let dir = find_run(run_ref, opts)?;
    let st: RunState = store::read_json(&dir.join("state.json"))?;
    let rec = st
        .steps
        .iter()
        .find(|r| r.id == step)
        .ok_or_else(|| Error::Invalid(format!("the run has no step {step:?}")))?;
    if rec.state != State::Waiting {
        return Err(Error::Invalid(format!(
            "step {step} is {:?}, not waiting for approval",
            rec.state
        )));
    }
    let approval = Approval {
        step: step.to_owned(),
        workflow_digest: st.workflow_digest.clone(),
        inputs_digest: rec.inputs_digest.clone().unwrap_or_default(),
        approved_unix: now_unix(),
    };
    let adir = dir.join("approvals");
    fs::create_dir_all(&adir).map_err(at(&adir))?;
    let mut bytes = serde_json::to_vec_pretty(&approval)?;
    bytes.push(b'\n');
    store::write_new(&adir.join(format!("{step}.json")), &bytes)?;
    Ok(format!(
        "approved {step}. A running coordinator picks this up; otherwise run: hivequeen resume {}",
        dir.display()
    ))
}

pub fn status(run_ref: &str, json: bool, opts: &Options) -> Result<String> {
    let dir = find_run(run_ref, opts)?;
    let path = dir.join("state.json");
    if json {
        return fs::read_to_string(&path).map_err(at(&path));
    }
    let st: RunState = store::read_json(&path)?;
    let mut out = format!("run {}\n", dir.display());
    for r in &st.steps {
        let state = format!("{:?}", r.state);
        let _ = writeln!(
            out,
            "  {:<20} {state:<12} {}",
            r.id,
            r.detail.as_deref().unwrap_or("")
        );
    }
    Ok(out)
}

pub fn report(run_ref: &str, opts: &Options) -> Result<String> {
    let dir = find_run(run_ref, opts)?;
    let st: RunState = store::read_json(&dir.join("state.json"))?;
    let wf_path = dir.join("workflow.toml");
    let wf = config::parse(&fs::read_to_string(&wf_path).map_err(at(&wf_path))?)?;
    config::validate(&wf)?;
    let states: Vec<State> = st.steps.iter().map(|r| r.state).collect();
    let outcome = if states
        .iter()
        .any(|s| matches!(s, State::Running | State::Integrating))
    {
        "in progress"
    } else {
        classify(&states, false).map_or("inconsistent", Outcome::label)
    };
    let patch = st.base.as_ref().map(|_| patch_status(&wf, &st));
    Ok(render(&dir, &wf, &st, outcome, patch))
}

pub fn doctor(backends: &[Backend], opts: &Options) -> Result<(bool, String)> {
    let list = if backends.is_empty() {
        Backend::ALL.to_vec()
    } else {
        backends.to_vec()
    };
    let root = store::state_root(opts.state_dir.as_deref())?;
    let mut ok = true;
    let mut out = String::new();
    for b in list {
        let dir = store::new_dir(&root.join("probes"))?;
        let probed = b.probe(&opts.settings, &dir);
        let _ = fs::remove_dir_all(&dir);
        match probed {
            Ok(c) => {
                let missing: Vec<&str> = b
                    .required()
                    .iter()
                    .copied()
                    .filter(|f| !c.supports(f))
                    .collect();
                if missing.is_empty() {
                    let _ = writeln!(
                        out,
                        "{}: ok ({}, {})",
                        b.name(),
                        b.program(&opts.settings),
                        c.version
                    );
                } else {
                    ok = false;
                    let _ = writeln!(
                        out,
                        "{}: missing required flags {}",
                        b.name(),
                        missing.join(" ")
                    );
                }
            }
            Err(e) => {
                ok = false;
                let _ = writeln!(out, "{}: unavailable: {e}", b.name());
            }
        }
    }
    Ok((ok, out))
}

/// Makes exactly one fresh agent call and writes the proposed workflow. Never
/// executes anything the proposal contains.
pub fn draft(question: &str, backend: Backend, out: &Path, opts: &Options) -> Result<String> {
    if out.exists() {
        return Err(Error::Invalid(format!(
            "{} already exists; choose another --out",
            out.display()
        )));
    }
    let root = store::state_root(opts.state_dir.as_deref())?;
    let dir = store::new_dir(&root.join("drafts"))?;
    let caps = backend
        .probe(&opts.settings, &dir)
        .map_err(|e| Error::Infra(format!("{} is not usable: {e}", backend.name())))?;
    let prompt = dir.join("prompt.md");
    store::write_atomic(&prompt, draft_prompt(question).as_bytes())?;
    let req = Request {
        prompt: &prompt,
        cwd: &dir,
        model: None,
        edit: false,
        git: false,
    };
    let inv = backend.invocation(&req, &opts.settings, &caps)?;
    let limits = Limits::new(DRAFT_TIMEOUT, opts.cancel.clone());
    let stdout = dir.join("stdout.log");
    let exit = process::execute(&inv, &stdout, &dir.join("stderr.log"), &limits)?;
    if !exit.success() {
        return Err(Error::Step(format!(
            "the draft call failed ({exit}); logs are in {}",
            dir.display()
        )));
    }
    let answer = backend.parse(&fs::read_to_string(&stdout).map_err(at(&stdout))?)?;
    let toml_text = extract_toml(&answer.text);
    let wf = config::parse(&toml_text)?;
    config::validate(&wf)?;
    for s in &wf.steps {
        if let Some(m) = &s.model
            && Some(m.as_str()) != s.backend().configured_model(&opts.settings)
        {
            return Err(Error::Invalid(format!(
                "the draft chose model {m:?} for step {}, which you have not configured; nothing was written",
                s.id
            )));
        }
    }
    store::write_new(out, toml_text.as_bytes())?;
    let mut summary = format!(
        "wrote {} ({}). Nothing was executed.\n",
        out.display(),
        wf.name
    );
    for s in &wf.steps {
        let what = match s.kind() {
            Kind::Agent(_) => format!(
                "{} agent{}",
                s.backend().name(),
                if s.edit { ", edits code" } else { "" }
            ),
            Kind::Exec(argv) => format!("command {argv:?}"),
            Kind::Gate(_) => "approval gate".into(),
        };
        let _ = writeln!(summary, "  {}: {what}", s.id);
    }
    let inputs: Vec<&str> = wf
        .steps
        .iter()
        .flat_map(|s| &s.inputs)
        .map(String::as_str)
        .collect();
    if !inputs.is_empty() {
        let _ = writeln!(
            summary,
            "It expects these input files next to it: {}",
            inputs.join(", ")
        );
    }
    let _ = write!(
        summary,
        "Review it, then run: hivequeen run {}",
        out.display()
    );
    Ok(summary)
}

fn draft_prompt(question: &str) -> String {
    format!(
        "Write a hivequeen workflow in TOML for the task below. Reply with one ```toml block only.\n\n\
         Schema: top-level name (string), optional parallel (1-8), optional timeout_secs. Then 1-16 [[steps]] \
         tables, each with id ([a-z][a-z0-9_-]*), optional needs (list of earlier ids), optional inputs (file \
         names), and exactly one of: prompt (agent instructions), exec (argv list, no shell string), or gate \
         (a question for a human). Agent steps may set backend = \"droid\" | \"claude\" | \"codex\". Do not set \
         model. Set edit = true only on steps that must change code, and give every editing step a later exec \
         check that needs it. Prefer two independent agent attempts and one explicit check.\n\n## Task\n\n{question}\n"
    )
}

fn extract_toml(text: &str) -> String {
    let body = text
        .split_once("```toml")
        .and_then(|(_, rest)| rest.split_once("```"))
        .map_or(text, |(inner, _)| inner);
    format!("{}\n", body.trim())
}

fn find_run(run_ref: &str, opts: &Options) -> Result<PathBuf> {
    let direct = PathBuf::from(run_ref);
    if direct.join("state.json").is_file() {
        return Ok(direct);
    }
    let named = store::state_root(opts.state_dir.as_deref())?
        .join("runs")
        .join(run_ref);
    if named.join("state.json").is_file() {
        Ok(named)
    } else {
        Err(Error::Invalid(format!("no run named {run_ref}")))
    }
}

fn probe_needed(
    wf: &Workflow,
    settings: &Settings,
    root: &Path,
    include: &[bool],
) -> Result<HashMap<Backend, Capabilities>> {
    let mut caps = HashMap::new();
    for (i, s) in wf.steps.iter().enumerate() {
        let b = s.backend();
        if s.prompt.is_none() || !include[i] || caps.contains_key(&b) {
            continue;
        }
        let scratch = store::new_dir(&root.join("probes"))?;
        let probed = b.probe(settings, &scratch);
        let _ = fs::remove_dir_all(&scratch);
        let c = probed.map_err(|e| Error::Infra(format!("{} is not usable: {e}", b.name())))?;
        caps.insert(b, c);
    }
    Ok(caps)
}

fn can_start(needs: &[State]) -> bool {
    needs.iter().all(|s| *s == State::Succeeded)
}

fn classify(states: &[State], cancelled: bool) -> Result<Outcome> {
    if cancelled || states.contains(&State::Interrupted) {
        Ok(Outcome::Interrupted)
    } else if states.iter().any(|s| s.is_bad()) {
        Ok(Outcome::Failed)
    } else if states.contains(&State::Waiting) {
        Ok(Outcome::Waiting)
    } else if states.iter().all(|s| *s == State::Succeeded) {
        Ok(Outcome::Succeeded)
    } else {
        let open = states.iter().filter(|s| !s.is_terminal()).count();
        Err(Error::Infra(format!(
            "invariant violated: no runnable work, but {open} step(s) are unfinished"
        )))
    }
}

fn patch_status(wf: &Workflow, st: &RunState) -> String {
    let steps = || wf.steps.iter().zip(&st.steps);
    let integrated = steps()
        .filter(|(s, _)| s.edit)
        .all(|(_, r)| r.state == State::Succeeded);
    let checked = steps().any(|(s, r)| {
        s.exec.is_some()
            && !s.edit
            && r.state == State::Succeeded
            && r.tip.is_some()
            && r.tip == st.tip
    });
    match (integrated, checked) {
        (true, true) => {
            "verified: every editing step integrated and a check passed on the final tip".into()
        }
        (false, _) => "unverified: not every editing step integrated".into(),
        (true, false) => "unverified: no check passed on the final integration tip".into(),
    }
}

fn build_prompt(
    prompt: &str,
    answers: &[(String, String)],
    files: &[(String, Vec<u8>)],
    inputs: &Path,
    work: &Path,
    edit: bool,
) -> String {
    let mut p = format!("{}\n", prompt.trim_end());
    if edit {
        let _ = writeln!(
            p,
            "\nYou are working in {}. Make your changes there and do not modify {RESERVED}/.",
            work.display()
        );
    }
    for (id, text) in answers {
        let _ = writeln!(p, "\n## Answer from step {id}\n\n{}", text.trim_end());
    }
    for (name, bytes) in files {
        match std::str::from_utf8(bytes) {
            Ok(t) => {
                let _ = writeln!(p, "\n## Input file {name}\n\n````\n{}\n````", t.trim_end());
            }
            Err(_) => {
                let _ = writeln!(
                    p,
                    "\n## Input file {name}\n\nBinary file at {}.",
                    inputs.join(name).display()
                );
            }
        }
    }
    p
}

fn quote(text: &str) -> String {
    text.trim_end()
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">\n".to_owned()
            } else {
                format!("> {l}\n")
            }
        })
        .collect()
}

fn render(
    dir: &Path,
    wf: &Workflow,
    st: &RunState,
    outcome: &str,
    patch: Option<String>,
) -> String {
    let mut o = format!("# Decision record: {}\n\n", wf.name);
    let _ = writeln!(
        o,
        "- Outcome: {outcome}\n- Run: {}\n- Workflow sha256: {}",
        dir.display(),
        st.workflow_digest
    );
    if let (Some(repo), Some(base)) = (&st.repo, &st.base) {
        let _ = writeln!(o, "- Repository: {} at {base}", repo.display());
    }
    o.push_str("\n## Inputs\n\n");
    if st.resources.is_empty() {
        o.push_str("No input files.\n");
    }
    for (name, digest) in &st.resources {
        let _ = writeln!(o, "- `{name}` sha256 {digest}");
    }
    o.push_str("\n## Steps\n");
    for (s, r) in wf.steps.iter().zip(&st.steps) {
        let _ = writeln!(o, "\n### {} ({:?})\n", s.id, r.state);
        let kind = match s.kind() {
            Kind::Agent(_) => format!(
                "agent {}, model requested {}, reported {}, session {}",
                r.backend.as_deref().unwrap_or(s.backend().name()),
                r.model_requested.as_deref().unwrap_or("backend default"),
                r.model_reported.as_deref().unwrap_or("unknown"),
                r.session.as_deref().unwrap_or("unknown")
            ),
            Kind::Exec(argv) => format!("command `{}`", argv.join(" ")),
            Kind::Gate(question) => format!("approval gate: {question}"),
        };
        let _ = writeln!(o, "- Kind: {kind}");
        let fields = [
            ("Detail", &r.detail),
            ("Exit", &r.exit),
            ("Backend version", &r.version),
            ("Tree", &r.tip),
            ("Cell commit", &r.cell_commit),
            ("Answer sha256", &r.answer_digest),
        ];
        for (label, value) in fields {
            if let Some(v) = value {
                let _ = writeln!(o, "- {label}: {v}");
            }
        }
        if s.prompt.is_some() && r.started_unix.is_some() {
            let usage = r
                .usage
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), ToString::to_string);
            let _ = writeln!(o, "- Usage: {usage}");
        }
        if r.started_unix.is_some() {
            let _ = writeln!(o, "- Invocation: `{}`", r.command.join(" "));
            let _ = writeln!(
                o,
                "- Logs: steps/{0}/stdout.log, steps/{0}/stderr.log",
                s.id
            );
        }
        if let Ok(text) = fs::read_to_string(dir.join("steps").join(&s.id).join("answer.md"))
            && !text.trim().is_empty()
        {
            let label = if s.prompt.is_some() {
                "Answer (a claim by this agent, not independently verified)"
            } else {
                "Output"
            };
            let _ = write!(o, "\n{label}:\n\n{}", quote(&text));
        }
    }
    if let Some(status) = patch {
        let _ = writeln!(
            o,
            "\n## Code result\n\n- Patch: changes.patch ({status})\n- Final tip: {}\n- Apply it yourself with `git apply`; hivequeen never changes your checkout.",
            st.tip.as_deref().unwrap_or("none")
        );
    }
    o
}

struct Run {
    dir: PathBuf,
    wf: Workflow,
    st: RunState,
    settings: Settings,
    caps: HashMap<Backend, Capabilities>,
    cancel: Arc<AtomicBool>,
    order: Vec<usize>,
    _lock: File,
}

impl Run {
    fn new(
        dir: PathBuf,
        wf: Workflow,
        st: RunState,
        opts: &Options,
        caps: HashMap<Backend, Capabilities>,
        lock: File,
    ) -> Self {
        let order = config::order(&wf);
        Run {
            dir,
            wf,
            st,
            settings: opts.settings.clone(),
            caps,
            cancel: opts.cancel.clone(),
            order,
            _lock: lock,
        }
    }

    fn save(&self) -> Result<()> {
        store::write_json(&self.dir.join("state.json"), &self.st)
    }

    fn step_dir(&self, i: usize) -> PathBuf {
        self.dir.join("steps").join(&self.wf.steps[i].id)
    }

    fn top(&self) -> Result<PathBuf> {
        self.st
            .repo
            .clone()
            .ok_or_else(|| Error::Infra("this run has no repository".into()))
    }

    fn refname(&self, tail: &str) -> String {
        let id = self
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("refs/hivequeen/{id}/{tail}")
    }

    fn need_states(&self, i: usize) -> Vec<State> {
        let idx = config::index(&self.wf);
        self.wf.steps[i]
            .needs
            .iter()
            .map(|n| self.st.steps[idx[n.as_str()]].state)
            .collect()
    }

    fn announce(&self) {
        eprintln!("hivequeen: run {}", self.dir.display());
        eprintln!(
            "  {} step(s), parallel {}, deadline in {}s",
            self.wf.steps.len(),
            self.wf.parallel,
            self.wf.timeout_secs
        );
        for (i, s) in self.wf.steps.iter().enumerate() {
            let what = match s.kind() {
                Kind::Agent(_) => {
                    let b = s.backend();
                    let model = s
                        .model
                        .as_deref()
                        .or(b.configured_model(&self.settings))
                        .unwrap_or("backend default");
                    let version = self.caps.get(&b).map_or("", |c| c.version.as_str());
                    format!("{} ({version}), model {model}", b.program(&self.settings))
                }
                Kind::Exec(argv) => format!("command {argv:?}"),
                Kind::Gate(_) => "approval gate".into(),
            };
            let cell = if config::uses_tree(&self.wf, i) {
                ", in its own cell"
            } else {
                ""
            };
            eprintln!("  {}: {what}{cell}", s.id);
        }
    }

    fn drive(&mut self) -> Result<Outcome> {
        let (tx, rx) = mpsc::channel::<Done>();
        let mut running = 0;
        if let Err(e) = self.schedule(&tx, &rx, &mut running) {
            // Never leave owned children behind when the coordinator fails.
            self.cancel.store(true, Ordering::Relaxed);
            while running > 0 {
                let Ok((i, result)) = rx.recv() else { break };
                running -= 1;
                let _ = self.finish(i, result);
            }
            return Err(e);
        }
        self.conclude()
    }

    fn schedule(
        &mut self,
        tx: &Sender<Done>,
        rx: &Receiver<Done>,
        running: &mut usize,
    ) -> Result<()> {
        loop {
            let stopping =
                self.cancel.load(Ordering::Relaxed) || now_unix() >= self.st.deadline_unix;
            let before: Vec<State> = self.st.steps.iter().map(|r| r.state).collect();
            if !stopping {
                self.poll_gates()?;
                self.integrate_ready()?;
                self.block_unreachable()?;
                while *running < self.wf.parallel {
                    let Some(i) = self.next_ready() else { break };
                    if self.start(i, tx)? {
                        *running += 1;
                    }
                }
            }
            let changed = before
                .iter()
                .zip(&self.st.steps)
                .any(|(b, r)| *b != r.state);
            if *running == 0 && (stopping || !changed) {
                return Ok(());
            }
            match rx.recv_timeout(POLL) {
                Ok((i, result)) => {
                    *running -= 1;
                    self.finish(i, result)?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Infra("worker channel closed".into()));
                }
            }
        }
    }

    fn conclude(&mut self) -> Result<Outcome> {
        let cancelled = self.cancel.load(Ordering::Relaxed);
        self.block_unreachable()?;
        if !cancelled && now_unix() >= self.st.deadline_unix {
            for r in &mut self.st.steps {
                if matches!(
                    r.state,
                    State::Pending | State::Waiting | State::Integrating
                ) {
                    r.state = State::Failed;
                    r.detail = Some("the run deadline was reached first".into());
                }
            }
        }
        let states: Vec<State> = self.st.steps.iter().map(|r| r.state).collect();
        self.save()?;
        let outcome = classify(&states, cancelled)?;
        self.write_report(outcome)?;
        Ok(outcome)
    }

    fn write_report(&self, outcome: Outcome) -> Result<()> {
        let patch = match (&self.st.repo, &self.st.base, &self.st.tip) {
            (Some(top), Some(base), Some(tip)) => {
                store::write_atomic(
                    &self.dir.join("changes.patch"),
                    &workspace::patch(top, base, tip)?,
                )?;
                Some(patch_status(&self.wf, &self.st))
            }
            _ => None,
        };
        let text = render(&self.dir, &self.wf, &self.st, outcome.label(), patch);
        store::write_atomic(&self.dir.join("decision.md"), text.as_bytes())
    }

    fn next_ready(&self) -> Option<usize> {
        self.order
            .iter()
            .copied()
            .find(|&i| self.st.steps[i].state == State::Pending && can_start(&self.need_states(i)))
    }

    fn settle(&mut self, i: usize, state: State, detail: String) -> Result<()> {
        eprintln!("hivequeen: {} {state:?}: {detail}", self.st.steps[i].id);
        let r = &mut self.st.steps[i];
        r.state = state;
        r.detail = Some(detail);
        r.finished_unix = Some(now_unix());
        self.save()
    }

    /// Commits the receipt before the state, so consumers are only released
    /// by an answer that is already durable.
    fn succeed(&mut self, i: usize, answer: Option<&[u8]>) -> Result<()> {
        let sdir = self.step_dir(i);
        let path = sdir.join("answer.md");
        if let Some(bytes) = answer {
            fs::create_dir_all(&sdir).map_err(at(&sdir))?;
            store::write_atomic(&path, bytes)?;
        }
        let digest = store::digest_file(&path)?;
        let r = &self.st.steps[i];
        let receipt = Receipt {
            step: r.id.clone(),
            inputs_digest: r.inputs_digest.clone().unwrap_or_default(),
            answer_digest: digest.clone(),
        };
        store::commit_answer(&sdir, &receipt)?;
        let r = &mut self.st.steps[i];
        r.answer_digest = Some(digest);
        r.state = State::Succeeded;
        r.finished_unix = Some(now_unix());
        eprintln!("hivequeen: {} succeeded", r.id);
        self.save()
    }

    fn poll_gates(&mut self) -> Result<()> {
        for i in 0..self.st.steps.len() {
            if self.st.steps[i].state != State::Waiting {
                continue;
            }
            let path = self
                .dir
                .join("approvals")
                .join(format!("{}.json", self.st.steps[i].id));
            if !path.is_file() {
                continue;
            }
            let a: Approval = store::read_json(&path)?;
            let r = &self.st.steps[i];
            if a.step != r.id
                || a.workflow_digest != self.st.workflow_digest
                || Some(&a.inputs_digest) != r.inputs_digest.as_ref()
            {
                self.settle(
                    i,
                    State::Failed,
                    "the approval does not match this run's workflow or inputs".into(),
                )?;
            } else {
                self.succeed(
                    i,
                    Some(format!("Approved at unix time {}.\n", a.approved_unix).as_bytes()),
                )?;
            }
        }
        Ok(())
    }

    /// Integrates finished cells in stable dependency order. A cell waits
    /// while any earlier editing step is still unfinished.
    fn integrate_ready(&mut self) -> Result<()> {
        for k in 0..self.order.len() {
            let i = self.order[k];
            if !self.wf.steps[i].edit {
                continue;
            }
            match self.st.steps[i].state {
                State::Integrating => {
                    let missing = || Error::Infra("integration state is incomplete".into());
                    let top = self.top()?;
                    let tip = self.st.tip.clone().ok_or_else(missing)?;
                    let parent = self.st.steps[i].tip.clone().ok_or_else(missing)?;
                    let cell = self.st.steps[i].cell_commit.clone().ok_or_else(missing)?;
                    let id = self.wf.steps[i].id.clone();
                    match workspace::integrate(
                        &top,
                        &tip,
                        &parent,
                        &cell,
                        &format!("hivequeen: integrate {id}"),
                    )? {
                        Merge::Clean(new) => {
                            workspace::set_ref(&top, &self.refname("comb"), &new)?;
                            self.st.tip = Some(new);
                            self.succeed(i, None)?;
                        }
                        Merge::Conflict(files) => {
                            store::write_atomic(
                                &self.step_dir(i).join("conflict.diff"),
                                &workspace::patch(&top, &parent, &cell)?,
                            )?;
                            let detail = format!(
                                "merge conflict in {}; cell {} is kept at {cell}",
                                files.join(", "),
                                self.dir.join("cells").join(&id).display()
                            );
                            self.settle(i, State::Conflicted, detail)?;
                        }
                    }
                }
                s if s.is_terminal() => {}
                _ => break,
            }
        }
        Ok(())
    }

    fn block_unreachable(&mut self) -> Result<()> {
        for k in 0..self.order.len() {
            let i = self.order[k];
            if self.st.steps[i].state != State::Pending {
                continue;
            }
            let bad = self.need_states(i).iter().position(|s| s.is_bad());
            if let Some(need) = bad.map(|n| self.wf.steps[i].needs[n].clone()) {
                self.settle(
                    i,
                    State::Blocked,
                    format!("blocked because {need} did not succeed"),
                )?;
            }
        }
        Ok(())
    }

    /// Prepares inputs and launches a worker. Returns false when no worker was
    /// started (a gate, or a step that failed before launch).
    fn start(&mut self, i: usize, tx: &Sender<Done>) -> Result<bool> {
        let step = self.wf.steps[i].clone();
        let sdir = self.step_dir(i);
        let needs_dir = sdir.join("needs");
        fs::create_dir_all(&needs_dir).map_err(at(&needs_dir))?;
        let (mut lines, mut env, mut answers) = (String::new(), Vec::new(), Vec::new());
        for need in &step.needs {
            let ndir = self.dir.join("steps").join(need);
            let receipt = store::completed(&ndir)?
                .ok_or_else(|| Error::Infra(format!("step {need} has no committed answer")))?;
            let src = ndir.join("answer.md");
            let bytes = fs::read(&src).map_err(at(&src))?;
            let copy = needs_dir.join(format!("{need}.md"));
            store::write_atomic(&copy, &bytes)?;
            let _ = writeln!(lines, "need {need} {}", receipt.answer_digest);
            env.push((
                format!("HIVEQUEEN_INPUT_{}_ANSWER", config::env_key(need)),
                copy.display().to_string(),
            ));
            answers.push((need.clone(), String::from_utf8_lossy(&bytes).into_owned()));
        }
        let names: Vec<String> = step
            .inputs
            .iter()
            .filter_map(|x| Path::new(x).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        for name in &names {
            let _ = writeln!(
                lines,
                "file {name} {}",
                self.st.resources.get(name).map_or("", String::as_str)
            );
        }
        let tree = config::uses_tree(&self.wf, i);
        if tree {
            let _ = writeln!(lines, "tree {}", self.st.tip.as_deref().unwrap_or(""));
        }
        self.st.steps[i].inputs_digest = Some(store::digest(lines.as_bytes()));
        if let Kind::Gate(question) = step.kind() {
            self.st.steps[i].state = State::Waiting;
            self.save()?;
            eprintln!("hivequeen: {} is waiting for approval: {question}", step.id);
            eprintln!(
                "  approve with: hivequeen approve {} {}",
                self.dir.display(),
                step.id
            );
            return Ok(false);
        }
        let (work, inputs) = if tree {
            let tip = self
                .st
                .tip
                .clone()
                .ok_or_else(|| Error::Infra("no integration tip".into()))?;
            let cell = self.dir.join("cells").join(&step.id);
            workspace::add_cell(&self.top()?, &cell, &tip)?;
            self.st.steps[i].tip = Some(tip);
            let inputs = cell.join(RESERVED);
            fs::create_dir_all(&inputs).map_err(at(&inputs))?;
            (cell, inputs)
        } else {
            let scratch = sdir.join("scratch");
            fs::create_dir_all(&scratch).map_err(at(&scratch))?;
            (scratch.clone(), scratch)
        };
        let mut files = Vec::new();
        for name in &names {
            let src = self.dir.join("resources").join(name);
            let bytes = fs::read(&src).map_err(at(&src))?;
            store::write_atomic(&inputs.join(name), &bytes)?;
            files.push((name.clone(), bytes));
        }
        env.push(("HIVEQUEEN_INPUTS".into(), inputs.display().to_string()));
        env.push(("HIVEQUEEN_RUN".into(), self.dir.display().to_string()));
        env.push(("HIVEQUEEN_STEP".into(), step.id.clone()));
        let mut inv = match step.kind() {
            Kind::Exec(argv) => {
                let mut inv = Invocation::new(argv[0].clone(), &work);
                inv.args = argv[1..].to_vec();
                inv
            }
            Kind::Agent(prompt) => {
                let body = build_prompt(prompt, &answers, &files, &inputs, &work, step.edit);
                if body.len() > MAX_PROMPT {
                    self.settle(
                        i,
                        State::Failed,
                        format!(
                            "the prompt is {} bytes; the limit is {MAX_PROMPT}",
                            body.len()
                        ),
                    )?;
                    return Ok(false);
                }
                let prompt_file = sdir.join("prompt.md");
                store::write_atomic(&prompt_file, body.as_bytes())?;
                let b = step.backend();
                let caps = self
                    .caps
                    .get(&b)
                    .cloned()
                    .ok_or_else(|| Error::Infra(format!("{} was not probed", b.name())))?;
                let req = Request {
                    prompt: &prompt_file,
                    cwd: &work,
                    model: step.model.as_deref(),
                    edit: step.edit,
                    git: tree,
                };
                let inv = match b.invocation(&req, &self.settings, &caps) {
                    Ok(inv) => inv,
                    Err(Error::Step(m)) => {
                        self.settle(i, State::Failed, m)?;
                        return Ok(false);
                    }
                    Err(e) => return Err(e),
                };
                let r = &mut self.st.steps[i];
                r.backend = Some(b.name().into());
                r.version = Some(caps.version);
                r.model_requested = step
                    .model
                    .clone()
                    .or_else(|| b.configured_model(&self.settings).map(str::to_owned));
                inv
            }
            Kind::Gate(_) => unreachable!("gates return before launch"),
        };
        inv.env = env;
        let r = &mut self.st.steps[i];
        r.state = State::Running;
        r.started_unix = Some(now_unix());
        r.command = inv.argv();
        self.save()?;
        eprintln!("hivequeen: started {}", step.id);
        let remaining = self.st.deadline_unix.saturating_sub(now_unix());
        let limits = Limits::new(
            Duration::from_secs(step.timeout().min(remaining)),
            self.cancel.clone(),
        );
        let (out, err) = (sdir.join("stdout.log"), sdir.join("stderr.log"));
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send((i, process::execute(&inv, &out, &err, &limits)));
        });
        Ok(true)
    }

    fn finish(&mut self, i: usize, result: Result<Exit>) -> Result<()> {
        let step = self.wf.steps[i].clone();
        let exit = match result {
            Ok(exit) => exit,
            Err(Error::Step(m)) => return self.settle(i, State::Failed, m),
            Err(e) => return Err(e),
        };
        self.st.steps[i].exit = Some(exit.to_string());
        match exit {
            Exit::Cancelled => {
                return self.settle(
                    i,
                    State::Interrupted,
                    "stopped by cancellation; start a new run".into(),
                );
            }
            e if !e.success() => return self.settle(i, State::Failed, e.to_string()),
            _ => {}
        }
        let sdir = self.step_dir(i);
        let out = sdir.join("stdout.log");
        let raw = String::from_utf8_lossy(&fs::read(&out).map_err(at(&out))?).into_owned();
        let answer = if step.prompt.is_some() {
            match step.backend().parse(&raw) {
                Ok(a) => {
                    let r = &mut self.st.steps[i];
                    r.session = a.session;
                    r.model_reported = a.model;
                    r.usage = a.usage;
                    if !a.diagnostics.is_empty() {
                        r.detail = Some(a.diagnostics.join("; "));
                    }
                    a.text
                }
                Err(Error::Step(m)) => return self.settle(i, State::Failed, m),
                Err(e) => return Err(e),
            }
        } else {
            raw
        };
        if !step.edit {
            return self.succeed(i, Some(answer.as_bytes()));
        }
        store::write_atomic(&sdir.join("answer.md"), answer.as_bytes())?;
        let parent = self.st.steps[i]
            .tip
            .clone()
            .ok_or_else(|| Error::Infra("cell has no start commit".into()))?;
        let cell = self.dir.join("cells").join(&step.id);
        let refname = self.refname(&format!("cells/{}", step.id));
        match workspace::snapshot(
            &self.top()?,
            &cell,
            &parent,
            &refname,
            &format!("hivequeen: {}", step.id),
        ) {
            Ok(commit) => {
                let r = &mut self.st.steps[i];
                r.cell_commit = Some(commit);
                r.state = State::Integrating;
                self.save()
            }
            Err(Error::Step(m)) => self.settle(i, State::Failed, m),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn opts(state: &Path) -> Options {
        Options {
            state_dir: Some(state.to_path_buf()),
            settings: Settings::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn workflow(dir: &Path, body: &str) -> std::io::Result<PathBuf> {
        let path = dir.join("workflow.toml");
        fs::write(&path, format!("name = \"test\"\n{body}"))?;
        Ok(path)
    }

    fn sh(id: &str, needs: &[&str], script: &str) -> String {
        let needs: Vec<String> = needs.iter().map(|n| format!("{n:?}")).collect();
        format!(
            "[[steps]]\nid = {id:?}\nneeds = [{}]\nexec = [\"/bin/sh\", \"-c\", {script:?}]\n",
            needs.join(", ")
        )
    }

    fn edit(id: &str, needs: &[&str], script: &str) -> String {
        format!("{}edit = true\n", sh(id, needs, script))
    }

    fn states(dir: &Path) -> Result<Vec<State>> {
        let st: RunState = store::read_json(&dir.join("state.json"))?;
        Ok(st.steps.iter().map(|r| r.state).collect())
    }

    fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, Box<dyn std::error::Error>> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
        if !out.status.success() {
            return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr)).into());
        }
        Ok(String::from_utf8(out.stdout)?.trim_end().to_owned())
    }

    fn repo() -> std::result::Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "t@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(dir.path(), args)?;
        }
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n")?;
        git(dir.path(), &["add", "-A"])?;
        git(dir.path(), &["commit", "-q", "-m", "init"])?;
        Ok(dir)
    }

    fn tip(run: &Path) -> Result<String> {
        let st: RunState = store::read_json(&run.join("state.json"))?;
        st.tip.ok_or_else(|| Error::Infra("no tip".into()))
    }

    #[test]
    fn failed_dependency_prevents_dispatch() {
        assert!(!can_start(&[State::Succeeded, State::Failed]));
        assert!(!can_start(&[State::Integrating]));
    }

    #[test]
    fn all_successful_dependencies_allow_dispatch() {
        assert!(can_start(&[State::Succeeded, State::Succeeded]));
        assert!(can_start(&[]));
    }

    #[test]
    fn unexplained_pending_is_error() {
        assert!(matches!(
            classify(&[State::Succeeded, State::Pending], false),
            Err(Error::Infra(_))
        ));
        assert_eq!(
            classify(&[State::Succeeded, State::Waiting, State::Pending], false).ok(),
            Some(Outcome::Waiting)
        );
    }

    #[test]
    fn fanin_waits_for_both() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = [
            sh("a", &[], "echo alpha"),
            sh("b", &[], "echo beta"),
            sh(
                "c",
                &["a", "b"],
                "cat \"$HIVEQUEEN_INPUT_A_ANSWER\" \"$HIVEQUEEN_INPUT_B_ANSWER\"",
            ),
        ]
        .concat();
        let (run_dir, outcome) = run(&workflow(dir.path(), &body)?, None, &opts(state.path()))?;
        assert_eq!(outcome, Outcome::Succeeded);
        assert_eq!(
            fs::read_to_string(run_dir.join("steps/c/answer.md"))?,
            "alpha\nbeta\n"
        );
        assert!(fs::read_to_string(run_dir.join("decision.md"))?.contains("Outcome: succeeded"));
        Ok(())
    }

    #[test]
    fn parallel_limit_holds() -> TestResult {
        let (dir, state, work) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let active = work.path().join("active");
        fs::create_dir(&active)?;
        let log = work.path().join("log");
        let script = format!(
            "touch {0}/$HIVEQUEEN_STEP; ls {0} | wc -l >> {1}; sleep 0.4; rm {0}/$HIVEQUEEN_STEP",
            active.display(),
            log.display()
        );
        let body: String = ["a", "b", "c", "d"]
            .iter()
            .map(|id| sh(id, &[], &script))
            .collect();
        let (_, outcome) = run(
            &workflow(dir.path(), &format!("parallel = 2\n{body}"))?,
            None,
            &opts(state.path()),
        )?;
        assert_eq!(outcome, Outcome::Succeeded);
        let peak = fs::read_to_string(&log)?
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .max();
        assert!(peak.is_some_and(|p| p <= 2), "peak concurrency {peak:?}");
        Ok(())
    }

    #[test]
    fn independent_branch_finishes() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = [
            sh("a", &[], "exit 1"),
            sh("b", &[], "echo ok"),
            sh("c", &["a"], "echo never"),
        ]
        .concat();
        let (run_dir, outcome) = run(&workflow(dir.path(), &body)?, None, &opts(state.path()))?;
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(
            states(&run_dir)?,
            vec![State::Failed, State::Succeeded, State::Blocked]
        );
        Ok(())
    }

    #[test]
    fn gate_approval_resumes_without_rerunning_answers() -> TestResult {
        let (dir, state, work) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let count = work.path().join("count");
        let body = [
            sh(
                "a",
                &[],
                &format!("echo ran >> {}; echo answer", count.display()),
            ),
            "[[steps]]\nid = \"ok\"\nneeds = [\"a\"]\ngate = \"Ship it?\"\n".to_owned(),
            sh("c", &["ok"], "echo done"),
        ]
        .concat();
        let o = opts(state.path());
        let (run_dir, outcome) = run(&workflow(dir.path(), &body)?, None, &o)?;
        assert_eq!(outcome, Outcome::Waiting);
        let id = run_dir.to_string_lossy().into_owned();
        approve(&id, "ok", &o)?;
        assert!(
            approve(&id, "ok", &o).is_err(),
            "an approval cannot be replaced"
        );
        let (_, outcome) = resume(&id, &o)?;
        assert_eq!(outcome, Outcome::Succeeded);
        assert_eq!(
            fs::read_to_string(&count)?,
            "ran\n",
            "step a ran exactly once"
        );
        Ok(())
    }

    #[test]
    fn gate_hash_mismatch_fails() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = format!(
            "{}[[steps]]\nid = \"ok\"\nneeds = [\"a\"]\ngate = \"Ship it?\"\n",
            sh("a", &[], "echo hi")
        );
        let o = opts(state.path());
        let (run_dir, _) = run(&workflow(dir.path(), &body)?, None, &o)?;
        let st: RunState = store::read_json(&run_dir.join("state.json"))?;
        let forged = Approval {
            step: "ok".into(),
            workflow_digest: st.workflow_digest,
            inputs_digest: "0".repeat(64),
            approved_unix: 0,
        };
        fs::create_dir_all(run_dir.join("approvals"))?;
        store::write_json(&run_dir.join("approvals/ok.json"), &forged)?;
        let (_, outcome) = resume(&run_dir.to_string_lossy(), &o)?;
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(states(&run_dir)?[1], State::Failed);
        Ok(())
    }

    #[test]
    fn interrupted_step_is_not_relaunched() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = format!(
            "{}[[steps]]\nid = \"ok\"\ngate = \"Go?\"\n{}",
            sh("a", &[], "echo hi"),
            sh("later", &["ok"], "echo later")
        );
        let o = opts(state.path());
        let (run_dir, _) = run(&workflow(dir.path(), &body)?, None, &o)?;
        let mut st: RunState = store::read_json(&run_dir.join("state.json"))?;
        st.steps[2].state = State::Running;
        store::write_json(&run_dir.join("state.json"), &st)?;
        let (_, outcome) = resume(&run_dir.to_string_lossy(), &o)?;
        assert_eq!(outcome, Outcome::Interrupted);
        assert_eq!(states(&run_dir)?[2], State::Interrupted);
        assert!(
            !run_dir.join("steps/later/stdout.log").exists(),
            "the step was not relaunched"
        );
        Ok(())
    }

    #[test]
    fn run_deadline_stops_work() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = format!(
            "timeout_secs = 1\n{}{}",
            sh("slow", &[], "sleep 5"),
            sh("after", &["slow"], "echo no")
        );
        let (run_dir, outcome) = run(&workflow(dir.path(), &body)?, None, &opts(state.path()))?;
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(states(&run_dir)?, vec![State::Failed, State::Blocked]);
        Ok(())
    }

    #[test]
    fn storage_failure_is_reported() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let blocker = state.path().join("file");
        fs::write(&blocker, "not a directory")?;
        let result = run(
            &workflow(dir.path(), &sh("a", &[], "true"))?,
            None,
            &opts(&blocker),
        );
        assert!(
            matches!(result, Err(Error::Infra(_))),
            "{:?}",
            result.map(|r| r.1)
        );
        Ok(())
    }

    #[test]
    fn editing_requires_a_repository() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = [edit("e", &[], "true"), sh("t", &["e"], "true")].concat();
        assert!(matches!(
            run(&workflow(dir.path(), &body)?, None, &opts(state.path())),
            Err(Error::Invalid(_))
        ));
        Ok(())
    }

    fn wait_for(marker: &Path, other: &Path, file: &str) -> String {
        format!(
            "touch {0}; i=0; while [ ! -e {1} ]; do i=$((i+1)); [ $i -gt 100 ] && exit 9; sleep 0.1; done; echo {file} > {file}.txt",
            marker.display(),
            other.display()
        )
    }

    #[test]
    fn independent_edits_overlap() -> TestResult {
        let (dir, state, sync, r) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            repo()?,
        );
        let (m1, m2) = (sync.path().join("one"), sync.path().join("two"));
        let body = [
            edit("e1", &[], &wait_for(&m1, &m2, "one")),
            edit("e2", &[], &wait_for(&m2, &m1, "two")),
            sh("check", &["e1", "e2"], "test -f one.txt && test -f two.txt"),
        ]
        .concat();
        let head = git(r.path(), &["rev-parse", "HEAD"])?;
        let (run_dir, outcome) = run(
            &workflow(dir.path(), &body)?,
            Some(r.path()),
            &opts(state.path()),
        )?;
        assert_eq!(
            outcome,
            Outcome::Succeeded,
            "{}",
            fs::read_to_string(run_dir.join("decision.md"))?
        );
        assert!(fs::read_to_string(run_dir.join("decision.md"))?.contains("(verified:"));
        assert_eq!(git(r.path(), &["rev-parse", "HEAD"])?, head);
        assert_eq!(git(r.path(), &["status", "--porcelain"])?, "");
        assert!(
            !r.path().join("one.txt").exists(),
            "the user's checkout is never changed"
        );
        Ok(())
    }

    #[test]
    fn integration_follows_declaration_order() -> TestResult {
        let r = repo()?;
        let mut trees = Vec::new();
        for (slow, fast) in [("sleep 1; ", ""), ("", "sleep 1; ")] {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let body = [
                edit("first", &[], &format!("{slow}echo 1 > first.txt")),
                edit("second", &[], &format!("{fast}echo 2 > second.txt")),
                sh(
                    "check",
                    &["first", "second"],
                    "test -f first.txt && test -f second.txt",
                ),
            ]
            .concat();
            let (run_dir, outcome) = run(
                &workflow(dir.path(), &body)?,
                Some(r.path()),
                &opts(state.path()),
            )?;
            assert_eq!(outcome, Outcome::Succeeded);
            let t = tip(&run_dir)?;
            let log = git(r.path(), &["log", "--first-parent", "--format=%s", &t])?;
            assert_eq!(
                log.lines().collect::<Vec<_>>(),
                ["hivequeen: integrate second", "hivequeen: first", "init"]
            );
            trees.push(git(r.path(), &["rev-parse", &format!("{t}^{{tree}}")])?);
        }
        assert_eq!(
            trees[0], trees[1],
            "completion order does not change the result"
        );
        Ok(())
    }

    #[test]
    fn conflict_fails_step_and_keeps_comb() -> TestResult {
        let (dir, state, r) = (tempfile::tempdir()?, tempfile::tempdir()?, repo()?);
        let body = [
            edit("e1", &[], "printf 'one\\nFIRST\\nthree\\n' > a.txt"),
            edit(
                "e2",
                &[],
                "sleep 0.3; printf 'one\\nSECOND\\nthree\\n' > a.txt",
            ),
            edit("e3", &[], "echo three > c.txt"),
            sh(
                "check13",
                &["e1", "e3"],
                "grep -q FIRST a.txt && test -f c.txt",
            ),
            sh("check2", &["e2"], "true"),
        ]
        .concat();
        let (run_dir, outcome) = run(
            &workflow(dir.path(), &body)?,
            Some(r.path()),
            &opts(state.path()),
        )?;
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(
            states(&run_dir)?,
            vec![
                State::Succeeded,
                State::Conflicted,
                State::Succeeded,
                State::Succeeded,
                State::Blocked
            ]
        );
        let t = tip(&run_dir)?;
        assert_eq!(
            git(r.path(), &["show", &format!("{t}:a.txt")])?,
            "one\nFIRST\nthree"
        );
        assert!(
            run_dir.join("cells/e2/a.txt").exists(),
            "the conflicted cell is kept for inspection"
        );
        assert!(fs::read_to_string(run_dir.join("steps/e2/conflict.diff"))?.contains("SECOND"));
        assert!(fs::read_to_string(run_dir.join("decision.md"))?.contains("unverified"));
        Ok(())
    }

    #[test]
    fn dependent_cell_starts_from_integrated_tip() -> TestResult {
        let (dir, state, r) = (tempfile::tempdir()?, tempfile::tempdir()?, repo()?);
        let body = [
            edit("e1", &[], "echo one > one.txt"),
            sh("c1", &["e1"], "test -f one.txt"),
            edit("e2", &["c1"], "test -f one.txt && echo two > two.txt"),
            sh("c2", &["e2"], "test -f one.txt && test -f two.txt"),
        ]
        .concat();
        let (run_dir, outcome) = run(
            &workflow(dir.path(), &body)?,
            Some(r.path()),
            &opts(state.path()),
        )?;
        assert_eq!(
            outcome,
            Outcome::Succeeded,
            "{}",
            fs::read_to_string(run_dir.join("decision.md"))?
        );
        let patch = fs::read_to_string(run_dir.join("changes.patch"))?;
        assert!(patch.contains("one.txt") && patch.contains("two.txt"));
        Ok(())
    }

    #[test]
    fn failed_edit_never_integrates() -> TestResult {
        let (dir, state, r) = (tempfile::tempdir()?, tempfile::tempdir()?, repo()?);
        let body = [
            edit("e", &[], "echo partial > p.txt; exit 1"),
            sh("t", &["e"], "true"),
        ]
        .concat();
        let (run_dir, outcome) = run(
            &workflow(dir.path(), &body)?,
            Some(r.path()),
            &opts(state.path()),
        )?;
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(tip(&run_dir)?, git(r.path(), &["rev-parse", "HEAD"])?);
        assert_eq!(fs::read_to_string(run_dir.join("changes.patch"))?, "");
        Ok(())
    }

    #[test]
    fn half_started_step_is_not_relaunched() -> TestResult {
        let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let body = format!(
            "[[steps]]\nid = \"ok\"\ngate = \"Go?\"\n{}",
            sh("later", &["ok"], "echo later")
        );
        let o = opts(state.path());
        let (run_dir, _) = run(&workflow(dir.path(), &body)?, None, &o)?;
        fs::create_dir_all(run_dir.join("cells/later"))?;
        let (_, outcome) = resume(&run_dir.to_string_lossy(), &o)?;
        assert_eq!(outcome, Outcome::Interrupted);
        assert_eq!(states(&run_dir)?[1], State::Interrupted);
        Ok(())
    }
}
