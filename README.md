# Hivequeen

Hivequeen runs a small TOML workflow of agent steps, command steps, and approval gates on your machine. It keeps every answer as hashed evidence. Editing steps run in parallel, each in its own Git worktree, and their results merge in a fixed order. You get a decision record and, for code work, a patch that you choose whether to apply.

The name is a nod to the Hive Queen in *Ender's Game*: one mind directing many workers. The *queen* is the single coordinator, a *cell* is one step's worktree, and the *comb* is the run-owned branch that finished cells merge into.

## Install

Requires Rust 1.99.1 (pinned in `rust-toolchain.toml`), Git 2.38 or newer (for `git merge-tree --write-tree`), and Linux or macOS.

```sh
cargo install --locked --path .
```

Agent steps need at least one backend CLI: Factory Droid (`droid`, the default), Claude Code (`claude`), or OpenAI Codex (`codex`). Only the backends a workflow uses must be installed.

## Commands

```text
hivequeen draft "<question>" [--backend droid|claude|codex] [--out workflow.toml]
hivequeen check workflow.toml
hivequeen run workflow.toml [--repo /path/to/clean/repo]
hivequeen status <run> [--json]
hivequeen resume <run>
hivequeen approve <run> <step>
hivequeen report <run>
hivequeen doctor [droid|claude|codex]
```

`<run>` is a run directory path or its ID. Global flags: `--state-dir` (default `$XDG_STATE_HOME/hivequeen`, or `~/.local/state/hivequeen`) and `--config` (default `$XDG_CONFIG_HOME/hivequeen/hivequeen.toml`).

`draft` makes one agent call and writes a proposed workflow file. It never runs anything in the proposal and never overwrites an existing file. `doctor` checks versions and flags without starting a model request.

Exit codes: 0 succeeded, 1 a step or check failed, 2 invalid input, 3 waiting for approval or interrupted, 4 infrastructure failure, 130 after Ctrl-C.

## Workflows

```toml
name = "explain-a-regression"
parallel = 2          # 1-8, default 2
timeout_secs = 3600   # whole run, default 3600

[[steps]]
id = "inspect"
prompt = "Find the smallest evidence-backed explanation."
inputs = ["question.md"]

[[steps]]
id = "challenge"
backend = "codex"
prompt = "Independently look for a counterexample."
inputs = ["question.md"]

[[steps]]
id = "decide"
backend = "claude"
needs = ["inspect", "challenge"]
prompt = "Compare the answers. Keep every disagreement."

[[steps]]
id = "check"
needs = ["decide"]
inputs = ["check-decision.sh"]
exec = ["/bin/sh", "check-decision.sh"]
```

Each step sets exactly one of `prompt` (an agent), `exec` (an argv list, never a shell string), or `gate` (a question for a person). Other fields: `id`, `needs`, `inputs`, `backend`, `model`, `edit`, and `timeout_secs` (default 1200). Up to 16 steps. Unknown fields are errors.

- Agents receive their prompt plus the answers of the steps they need, and the text of their input files.
- Commands receive `HIVEQUEEN_INPUT_<ID>_ANSWER` (the path to each needed step's answer, with the ID upper-cased and `-` turned into `_`), `HIVEQUEEN_INPUTS` (the directory holding input files), `HIVEQUEEN_RUN`, and `HIVEQUEEN_STEP`.
- Input files are resolved next to the workflow file and snapshotted when the run starts. They must be regular files.
- A step runs at most once per run. There are no retries.

See `examples/` for a command-only workflow that needs no provider, a mixed-backend workflow, and a parallel-editing workflow.

## Editing code

Steps with `edit = true` need `--repo`, which must point at a clean repository with at least one commit.

1. The run pins `HEAD` as the base.
2. Each editing step runs in its own detached worktree (a cell) under the run directory, created from the current integration tip. Independent editing steps run at the same time.
3. When an editing step succeeds, its cell is recorded as one commit under `refs/hivequeen/<run>/cells/<step>`, excluding `.hivequeen/`.
4. Cells merge into `refs/hivequeen/<run>/comb` in dependency order (declaration order when the file lists dependencies first), whichever finishes first. A merge conflict fails that step (`Conflicted`), blocks its dependents, and leaves the tip unchanged. Nothing resolves conflicts automatically.
5. A command step that depends on an editing step runs in a fresh cell at the integration tip. Every editing step must have such a check.
6. `changes.patch` is the diff from the base to the final tip. The decision record marks it verified only if every editing step merged and a check passed on that exact tip.

Hivequeen never changes your branches, `HEAD`, index, or files, and never runs your repository's Git hooks in its worktrees. Agent commits inside a cell are flattened into its snapshot. New symlinks and submodule changes are refused. Hivequeen uses your configured Git identity for its private commits and stops if none is configured.

Steps that run in a cell read copied inputs from `.hivequeen/<name>`, not the cell root. Commands can use `$HIVEQUEEN_INPUTS/<name>`. A conflicted step keeps its proposed changes in `steps/<step>/conflict.diff`, relative to its starting commit.

Cleanup is manual. To remove a run's worktrees and refs:

```sh
git -C /path/to/repo worktree remove --force <run-dir>/cells/<step>   # for each cell
git -C /path/to/repo for-each-ref --format='%(refname)' refs/hivequeen/<run-id>/ | xargs -n1 git -C /path/to/repo update-ref -d
```

## Backends

| Backend | Invocation |
| --- | --- |
| Droid (default) | `droid exec --cwd <dir> --output-format json --file <prompt>`; editing adds `--auto low` (or `medium`, `high`) |
| Claude Code | `claude --print --output-format json --permission-mode dontAsk`, prompt on stdin; research allows `Read,Grep,Glob`; editing uses `acceptEdits` |
| Codex | `codex --ask-for-approval never exec --json --sandbox read-only --cd <dir> -`, prompt on stdin; editing uses `workspace-write` |

Every step starts a fresh session. Hivequeen never passes resume, fork, mission, or permission-bypass flags, and never falls back to another vendor. A successful exit with a failed or malformed result is a failure. Missing token usage is recorded as unknown, not zero.

Optional settings go in `hivequeen.toml`:

```toml
[droid]
program = "droid"
model = "claude-sonnet-5"
reasoning_effort = "high"
edit_autonomy = "low"            # low, medium, or high

[claude]
model = "claude-sonnet-5"
effort = "high"
max_turns = 20
allowed_tools = ["Read", "Edit", "Bash(cargo test *)"]

[codex]
model = "gpt-5.3-codex"
reasoning_effort = "high"
```

A step's `model` overrides the configured model for that backend only. Model IDs are passed through unchanged.

Vendor permissions and project configuration stay in force, and fresh sessions may still load project instructions or hooks. Hivequeen is not a sandbox.

## Runs and resume

Each run directory holds `workflow.toml`, `resources/`, `state.json`, `approvals/`, `steps/<id>/` (prompt, logs, answer, receipt), `cells/`, `decision.md`, and `changes.patch`. Only the coordinator writes `state.json`, using atomic replace, and a lock prevents two coordinators from owning one run.

`resume` continues a run that is waiting at a gate or stopped between steps. It never relaunches a step that was running when the coordinator stopped. That step is marked `Interrupted`, and you start a new run. Ctrl-C or SIGTERM stops dispatch, terminates each step's process group, and records the interruption. If the coordinator itself is killed with SIGKILL, child processes can survive. Stop them yourself.

## Development

The initial implementation exceeds the plan's 1,800-line production-code target (about 2,600 lines). Parallel worktrees, ordered integration, durable evidence, and process handling are implemented rather than trimmed to meet that target.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --doc
cargo build --locked --release --bin hivequeen
```

The tests use fake agent CLIs (`tests/fixtures/fake-agent.sh`) and never contact a provider. A real Droid smoke test is ignored by default because it makes a model request: `cargo test --test cli -- --ignored`.

## License

MIT
