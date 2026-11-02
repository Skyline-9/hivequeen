# Hivequeen Implementation Plan

**Goal:** A small personal CLI that builds a defensible answer or code patch from independent agent sessions, saved evidence, and explicit checks.

**Status:** Implemented on `build/initial-implementation` after explicit implementation approval. No push or publication authorized. The implementation exceeds the 1,800-line production-code target (about 2,600 lines); this overrun is reported for review.

**Identity:** Hivequeen, executable `hivequeen`. The name nods to the Hive Queen in Orson Scott Card's *Ender's Game*: one mind that directs many workers at once. It is a generic compound word, not a use of the book's characters, artwork, or text. The crates.io lookup returned 404 on 2026-11-02, and a web search found only small unrelated hobby repositories with that name. This is not trademark clearance or a guarantee of global uniqueness. Check again before publishing. General techniques such as dependency ordering and subprocesses are not claimed as novel inventions.

**Vocabulary:** the *queen* is the single coordinator thread. A *cell* is a run-owned Git worktree for one editing step. The *comb* is the run-owned integration ref (`refs/hivequeen/<run>/comb`) into which finished cells are merged in a fixed order.

**Product principle:** Show what was observed, what disagreed, and why the final decision holds. Prefer two independent attempts and one explicit check over a large autonomous hierarchy. There are no mandatory agent roles or fixed sequences.

## 1. Hard requirements

- Rust, newest stable compiler and newest stable dependency releases; no nightly or prerelease requirement.
- Factory Droid is the default first-class backend. Claude Code and OpenAI Codex also have complete native support from the first release, including mixed-backend workflows.
- Minimal implementation and dependencies. One crate, one public binary, standard library first.
- Clippy and rustfmt are required gates. Unit tests follow *The Rust Programming Language*, Chapter 11.
- Personal/local use: no product login, identity system, billing, telemetry, HTTP server, or hosted storage.
- Independently written code, naming, documentation, and interaction design. Implement from this specification and public vendor interfaces, not another product's source or reverse-engineered internals.
- Preserve the user's checkout. No automatic commits, merges, pushes, stashing, resets, or cleanup of unknown files.
- Never call prose, an exit code alone, or an old output file proof of verified completion.

## 2. Keep it small

Ship only:

1. A TOML file describing ordered independent steps.
2. Three agent CLI adapters and ordinary command steps.
3. Bounded parallelism for independent scratch steps.
4. Atomic local receipts and conservative resume.
5. Optional human decision steps.
6. One private Git worktree (cell) per editing step, running in parallel and integrated in declaration order.
7. A Markdown decision record and optional patch.

Do not build an async runtime, SQL database, actor system, daemon, plugin API, general JSON Schema engine, distributed locking, cross-run cache, event-sourcing framework, UI, automatic model fallback, automatic conflict resolution, or generic templating language.

Use straightforward structs, enums, functions, `Result`, standard threads/channels, and `std::process::Command`. A helper becomes an abstraction only when a second real use needs it. Use one internal error enum with `Display`, `Error`, and the required conversions; no error framework dependency.

**Size guardrail:** Aim for at most 1,800 handwritten production Rust lines (the extra 300 over a serialized design pays for per-step cells and ordered integration), excluding tests, fixtures, documentation, and the lockfile. This is a scope budget, not an excuse to compress formatting, use clever macros, remove checks, or hide code in generated files. Remove features before expanding architecture; report a necessary budget overrun for review.

## 3. Current toolchain and dependency baseline

Verified against the official stable channel and crates.io on **2026-11-02**:

| Component | Newest stable version observed | Purpose |
| --- | --- | --- |
| Rust | **1.99.1**, released 2026-10-15 | Compiler and matching bundled tools |
| `clap` | **4.6.7** | CLI parsing, `derive` feature |
| `serde` | **1.0.229** | Typed records, `derive` feature |
| `serde_json` | **1.0.154** | Receipts and backend results |
| `toml` | **1.1.8+spec-1.1.0** | Workflow/configuration |
| `nix` | **0.31.3** | Unix process-group signals, `signal`/`process` features only |
| `signal-hook` | **0.4.6** | Safe SIGINT/SIGTERM notification |
| `sha2` | **0.11.1** | Input/output SHA-256 digests |
| `tempfile` | **3.27.0** | Dev dependency for isolated filesystem tests |

Use Rust 2024 edition and `rust-version = "1.99.1"`. Commit `Cargo.lock`. Cargo's TOML requirement is `toml = "1.1.8"`; the build metadata is recorded in the lockfile but does not affect version matching. Use the versions above as normal Cargo requirements, not wildcards.

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.99.1"
profile = "minimal"
components = ["clippy", "rustfmt"]
```

Clippy/rustfmt come from that compiler toolchain, not independently versioned crate dependencies.

**Freshness rule:** At implementation kickoff and before release, reread the official stable manifest and registry metadata. If a newer stable release exists, update the toolchain, manifest requirements, and lockfile together, review any migrations, and rerun all gates. `cargo update` alone cannot upgrade a dependency beyond its manifest's major-version constraint. Do not claim these versions remain newest indefinitely.

Keep only enabled dependency features actually used. Do not add Tokio, SQLite, UUID, a hashing wrapper, an assertion library, a mocking framework, or property-testing dependencies preemptively. Transitive versions are resolved and reviewed in Cargo.lock; their compatibility cannot be proven by this plan.

## 4. Interface

```text
hivequeen draft "<question>" --backend droid|claude|codex
hivequeen check workflow.toml
hivequeen run workflow.toml [--repo /absolute/path]
hivequeen status <run>
hivequeen resume <run>
hivequeen approve <run> <step>
hivequeen report <run>
hivequeen doctor [droid|claude|codex]
```

- `draft` makes exactly one fresh agent call and writes a candidate TOML file plus a readable summary. It never executes the proposed commands. The user reviews the file and invokes `run`.
- `check` parses and validates without starting agents or executing check commands.
- `run`/`resume` remain foreground processes. Ctrl-C or SIGTERM stops dispatch, terminates owned processes, and records interruption.
- `status`/`report` read receipts; no separate log-follow service. Logs are ordinary files.
- `approve` records a local workflow decision, not an identity claim. A running coordinator polls for it; otherwise the user invokes `resume`.
- `doctor` checks executable/version/headless capabilities without starting a model request. Only a workflow's referenced backend CLIs are required.
- Exit codes: 0 success, 1 step/check failed, 2 invalid input, 3 waiting/interrupted/recovery needed, 4 infrastructure failure. SIGINT exits 130 after cleanup.
- Prints resolved backends/models, commands, process limit, deadline, and output location before dispatch. Machine-readable status is optional `--json`, not a separate API.

## 5. Workflow

TOML is the only configuration format. No YAML compatibility layer or textual shell interpolation.

```toml
name = "explain-a-regression"
parallel = 2
timeout_secs = 3600

[[steps]]
id = "inspect"
backend = "droid"
prompt = "Find the smallest evidence-backed explanation. State uncertainty."
inputs = ["question.md"]

[[steps]]
id = "challenge"
backend = "codex"
prompt = "Independently seek a counterexample to the suspected explanation."
inputs = ["question.md"]

[[steps]]
id = "decide"
backend = "claude"
needs = ["inspect", "challenge"]
prompt = "Compare the supplied answers. Preserve disagreements and unsupported claims."

[[steps]]
id = "check"
needs = ["decide"]
exec = ["/bin/sh", "check-decision.sh"]
inputs = ["check-decision.sh"]
```

The example's Markdown and shell script are supplied resources, not assumed built-ins. The script receives `HIVEQUEEN_INPUT_DECIDE_ANSWER`, the copied `decide` answer path, and can check required headings/evidence. All referenced resource files are snapshotted before execution.

### Schema

- `Workflow { name, parallel, timeout_secs, steps }`.
- `Step { id, needs, inputs, prompt, backend, model, exec, gate, edit, timeout_secs }`.
- Exactly one of `prompt`, `exec`, or `gate` must be supplied. No untagged heuristic that guesses step type.
- `backend` defaults to `droid`; `model` is optional and belongs only to the selected backend. Never translate model IDs between vendors.
- `needs` lists predecessor IDs; dependencies are AND. Give each consumer the immutable answers of its direct predecessors. Additional file inputs must be explicitly listed.
- IDs use `[a-z][a-z0-9_-]{0,63}`; reserve `answer` for the runner-captured final response.
- Allow 1..16 steps per workflow. Default global parallelism 2, allowed range 1..8. Default run timeout 3,600 seconds and step timeout 1,200 seconds; positive values only, maximum 86,400 seconds. The run deadline wins.
- Each step launches at most once per run. No implicit retries, additional agent spawning, or graph mutation.
- `edit = true` requires `--repo` and runs in its own cell (section 8). Independent editing steps run in parallel; results integrate in declaration order. Scratch agents and cells share the same `parallel` limit.
- `exec` is an argv array, not a shell string. Shell scripts require an explicit `/bin/sh` argv entry.
- A gate waits for a receipt bound to the run, step ID, workflow digest, and predecessor-answer digests.
- Use Serde `deny_unknown_fields`; reject duplicate/unknown IDs, missing resources, unknown needs, invalid combinations, model-without-agent fields, more than 16 steps, and cycles with a readable cycle path.
- File inputs are regular files, not symlinks/directories; resolve them against the workflow directory. Accept explicit source paths but copy only declared inputs. Reject colliding copy destinations. No recursive project copying.

### Backend configuration

Optional `hivequeen.toml` contains typed per-backend executable/model/options. Workflow settings override only the matching backend's defaults. The draft writer cannot change executable paths, permission settings, or models not already configured by the user.

Backend options are a small enum, not `extra_args`: Droid reasoning/autonomy, Claude tool rules/effort/max turns, Codex reasoning/sandbox. Required machine-output, cwd, session-freshness, and no-bypass controls cannot be overridden.

For the first release, native permissions are documented directly rather than falsely unified into a universal permission model. Default workflows capture final responses through the runner, so a researcher need not edit any files.

## 6. First-class agent adapters

Implement `Backend::{Droid, Claude, Codex}` in one `backend.rs`, with only:

```rust
fn probe(&self) -> Result<Capabilities>;
fn invocation(&self, request: &Request) -> Result<Invocation>;
fn parse(&self, raw: &str, exit: ExitStatus) -> Result<Answer>;
```

These are method signatures, not a plugin trait. `Invocation` holds program, argv, cwd, and stdin mode. `Answer` holds final text, session/model metadata when available, optional usage, and diagnostics. Preserve raw backend output.

| Backend | Invocation and prompt | Completion |
| --- | --- | --- |
| Factory Droid, default | `droid exec --cwd <absolute> --output-format json -f <prompt-file>` | Exit 0 and successful valid JSON result |
| Claude Code | `claude -p --output-format json --permission-mode dontAsk`, prompt from stdin, process cwd set | Exit 0 and successful valid result envelope |
| OpenAI Codex | `codex --ask-for-approval never exec --json --sandbox read-only --cd <absolute> -`, prompt from stdin | Exit 0 and valid successful terminal JSONL turn |

Details that must not disappear in a generic command wrapper:

- **Droid:** Default research invocation omits `--auto`. For editing, explicitly configured `--auto low`/`medium`; configured `--model` and `--reasoning-effort`. Parse result/session/usage and failures, not prose.
- **Claude:** Default research permits no interactive approvals. Use explicit read-oriented tool selection where supported. Editing uses `acceptEdits`; shell operations require user-configured allow rules. Parse error/subtype/denial fields. Optional `--max-turns`, `--model`, `--effort`, and no-session-persistence depend on capability checks. Do not automatically enable bare mode or change the user's CLI setup.
- **Codex:** Editing uses `workspace-write`, never danger-full-access. In owned non-Git scratch directories, use supported `--skip-git-repo-check`. Parse JSONL even when chunked on disk: unknown events are retained, failed/truncated/missing terminal turns fail; recoverable nonterminal error events do not necessarily fail the attempt. Optional ephemeral mode is capability-gated.
- Never use vendor resume/continue/fork/mission flags, unsafe permission bypasses, or an automatic fallback vendor. Each step gets a new session.
- Read the full bounded prompt through a regular file as stdin when needed, then EOF occurs naturally; no stdin pump/IPC thread. Droid consumes its prompt file directly.
- Record requested and reported models separately. Missing token/cost data is unknown, not zero; retain vendor metric names rather than summing unlike fields.
- Each backend gets probe tests, invocation tests, success/failure fixtures, malformed-output fixtures, documentation, and an optional real smoke test.
- A missing optional vendor CLI must not break workflows using another vendor. No paid call during `doctor`.
- Vendor permissions and project configuration remain in force. Fresh sessions may still load project instructions/hooks; this runner is not a sandbox and does not rewrite vendor settings.

## 7. Execution and storage

### A single state writer

Store under `$XDG_STATE_HOME/hivequeen/runs/<id>` or `$HOME/.local/state/hivequeen/runs/<id>`, outside the user's checkout. Permit `--state-dir` for explicit relocation.

Use `std::fs::File::try_lock` on `run.lock` for one coordinator. No database or lock crate. IDs combine time, process ID, and an atomic counter, with exclusive directory creation to handle collisions.

```text
<run>/
  run.lock
  workflow.toml
  resources/
  state.json
  approvals/
  steps/<id>/
    prompt.md
    stdout.log
    stderr.log
    answer.md
    receipt.json
    scratch/
  cells/<id>/           one worktree per editing or tree-checking step
  decision.md
  changes.patch         optional verified code result
```

Only the coordinator writes `state.json`. Worker threads return outcomes over `std::sync::mpsc`; they never race on shared state or a shared notes file. Write each JSON snapshot to a same-directory temporary file, sync, rename, then sync the directory. Check every result.

`State` is a small enum: `Pending`, `Running`, `Waiting`, `Integrating`, `Succeeded`, `Failed`, `Conflicted`, `Blocked`, `Interrupted`. `Integrating` means an editing step finished and is queued for ordered integration. A failed prerequisite blocks its descendants. Independent branches can finish; the run still fails. No-runnable-work with unexplained pending steps is an invariant error, not success.

Persist `Running` before spawn; an exit-0 result without its committed validated receipt cannot release consumers. Keep the exact workflow, input digests, executable versions/options, exit status, output digest, and timestamps. A mismatched output digest is an integrity error.

No cross-run cache. Resume trusts only matching snapshotted inputs and successful hashed answers. A preexisting arbitrary file never counts as a step completed.

### Process lifetime

`process.rs` spawns children in owned POSIX process groups using `CommandExt::process_group`, with stdout/stderr redirected to files. Poll child status and cancellation every 100 ms. On cancellation/deadline, TERM the owned group, wait up to 5 seconds, KILL if necessary, and reap the child. Clean up remaining group members before recording success. Deliberately escaped daemons are outside the guarantee.

Use `signal-hook` to set a cancellation flag; no custom unsafe signal handler. `nix` supplies group signalling. No background supervisor process.

Prompt limit: 128 KiB. Combined log limit: 100 MiB, checked every poll; a fast writer may overshoot within one interval. Stop and fail rather than silently truncate logs or claim a hard OS resource limit.

**Crash boundary:** SIGKILL can leave children alive. Resume must never signal a persisted PID blindly or relaunch an interrupted step. Mark ambiguous steps interrupted and explain that the user must stop remaining work and start a new run. Completed unaffected steps remain inspectable. A run waiting at a gate, or stopped between steps with no ambiguous attempt, can resume pending work; a killed active attempt cannot transparently resume. No separate pause command or crash failover in this release.

An approval is an immutable decision file written atomically. A live coordinator polls it; an offline approval takes effect on resume. Validate its workflow/input hashes. Report queued/waiting status accurately.

## 8. Code work: one cell per editing step, ordered integration

Editing steps run in parallel, each in its own worktree. Integration is deterministic and never resolves conflicts automatically.

**Setup**

- Require a clean committed source tree and pin HEAD as `base`. Refuse dirty source; never auto-stash.
- All run-owned refs live under `refs/hivequeen/<run>/`. Never create, move, or delete the user's branches, tags, HEAD, or index.
- The comb is the ref `refs/hivequeen/<run>/comb`, starting at `base`; no worktree is needed because `git merge-tree` merges without one. Its current commit is the *integration tip*.

**Cells**

- When an `edit = true` step becomes runnable, the queen creates a detached cell worktree at the current integration tip. Because a step only runs after its `needs` are integrated, every cell already contains its predecessors' edits.
- Edit steps with no dependency path between them run concurrently, up to `parallel`. A cell counts as one slot.
- Only the queen runs Git commands against the shared repository (worktree add, commit, update-ref, merge). Workers only run the agent inside their own cell. This avoids `index.lock` races without a lock crate.
- Put temporary step inputs and outputs beneath a reserved `.hivequeen/` directory inside the cell, so workspace-write backends can reach them. Reject source commits that already track that directory, and exclude it from every snapshot even if an agent stages it.
- Parallel scratch research cannot modify cells or the comb through orchestrator-provided paths. Backend tool access is still not a security boundary.

**Snapshot and ordered integration**

- When an editing step exits successfully, the queen snapshots its cell: stage tracked changes, deletions, mode changes, and nonignored new regular files, excluding `.hivequeen/`. Record one commit with `git commit-tree` on `refs/hivequeen/<run>/cells/<step>`, using the user's configured Git identity. A missing identity is an infrastructure failure; never invent one. Agent-created commits in the cell are flattened into this snapshot. Reject submodule changes and new symlink/special files.
- **Integration order is the stable dependency order, not completion order.** That order takes the first declared step whose needs are already placed, so it equals declaration order whenever the file lists dependencies first. A finished cell waits until every editing step earlier in that order has integrated, failed, or been blocked. Pure declaration order could deadlock when an earlier-declared editing step depends on a later one. Either way, the result does not depend on which agent finishes first.
- Integrate with `git merge-tree --write-tree` followed by `commit-tree` (Git 2.38 or newer), chosen because it needs no worktree. When the tip has not moved since the cell started, the cell commit becomes the tip directly. Advance `refs/hivequeen/<run>/comb` on success.
- **A merge conflict fails that step** with state `Conflicted` and keeps its cell ref, conflict file list, and diff for inspection. The comb stays at its pre-merge commit. Later independent cells still integrate; descendants of the conflicted step are blocked; the run fails. No automatic or agent-driven conflict resolution in this release.
- A failed or interrupted editing step never integrates. Preserve its cell for inspection and require a new run; do not resume editing on a half-known tree.

**Checks and output**

- A command step that `needs` an editing step runs in a fresh cell at the integration tip that contains all its editing ancestors. Each editing step must have a downstream command check.
- `changes.patch` is the binary-safe diff from `base` to the final integration tip. It is labeled verified only when every editing step integrated and at least one command check passed against that exact final tip. Otherwise it is written and labeled unverified, with the reason.
- Never apply the patch to the user's checkout or create commits on the user's branches.
- No automatic deletion of dirty cells or the comb; cleanup is a separate, explicitly requested manual operation that also removes `refs/hivequeen/<run>/`.
## 9. Completion and the decision record

The runner writes `answer.md` from normalized successful agent output, hashes it, and validates it is nonempty. This verifies the output contract, not factual accuracy. A command check or critical reader must establish stronger claims.

After all terminal steps, write `decision.md` deterministically with:

- Question and inputs.
- Each answer and its evidence references.
- Explicit disagreements and unresolved points as supplied by the answers, labeled as agent claims rather than independently established facts.
- Exact command checks, exits, and log paths.
- Backend/model/session details and known/unknown usage.
- Patch path where applicable; failed/blocked/interrupted steps prominently shown.

Do not invoke a model merely to format the record. A workflow may explicitly include a comparison step, as in the example. Do not infer consensus or silently convert uncertainty into fact.

## 10. Files and interfaces

Keep eight source files; split only when a concrete readability problem warrants it:

```text
src/main.rs        Clap and thin command dispatch
src/lib.rs         minimal public entry points
src/config.rs      typed TOML, validation, dependency order
src/backend.rs     the three native adapters
src/process.rs     child lifecycle, signals and deadlines
src/store.rs       lock, snapshots, receipts, hashes
src/runner.rs      scheduling, approval, decision record
src/workspace.rs   scratch dirs, cells, comb, ordered integration
tests/cli.rs       real binary/process/filesystem tests
tests/common/mod.rs
tests/fixtures/    small TOML/JSON/JSONL and shell fixtures
```

Use private helpers and ordinary records. Core function contracts:

```rust
fn load(path: &Path) -> Result<Workflow>;
fn validate(workflow: &Workflow) -> Result<()>;
fn execute(invocation: Invocation, limits: Limits) -> Result<ProcessResult>;
fn run(workflow: Workflow, options: Options) -> Result<RunOutcome>;
fn resume(run_dir: &Path) -> Result<RunOutcome>;
```

`Result<T>` aliases the crate error enum; `Path` is the standard type. Keep main free of business logic. Do not expose every module/type solely to integration tests.

## 11. Idiomatic tests, following The Book

Unit tests belong **inside the source file they test**, not in a parallel `tests/unit` tree. Use ordinary test functions and standard assertions:

```rust
// At the end of src/runner.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_dependency_prevents_dispatch() {
        assert!(!can_start(&[State::Succeeded, State::Failed]));
    }

    #[test]
    fn all_successful_dependencies_allow_dispatch() {
        assert!(can_start(&[State::Succeeded, State::Succeeded]));
    }
}
```

Rules from Chapter 11:

- `#[cfg(test)] mod tests`, `use super::*`, and `#[test]` for colocated unit tests.
- Test private helpers directly where useful; do not broaden public API for tests.
- Use `assert!`, `assert_eq!`, and `assert_ne!` with useful messages. Test one behavior per named test.
- Tests may return `Result<(), E>` and use `?` for successful setup paths. Assert expected `Err` variants directly rather than propagating the expected failure with `?`.
- Use `#[should_panic(expected = "...")]` only for an intentional panic contract, never for ordinary invalid workflow/backend input. Do not combine it with a `Result`-returning test.
- `tests/cli.rs` exercises public CLI/process behavior only. Shared integration helpers go in `tests/common/mod.rs`, as described in The Book.
- Never rely on test ordering, global cwd/environment mutation, shared temporary paths, exact timing, or a paid network call. Use tempfile and process-local environment overrides.
- Real vendor smoke tests are `#[ignore]`, separately requested and reported; fake subprocess fixtures run without provider access.
- Keep parser/graph/state tests small and table-driven where cases share a contract. No snapshot, mocking, assertion, or async test framework needed.

Required checks:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --doc
cargo build --locked --release --bin hivequeen
```

Run normal Clippy defaults with warnings denied. Do not enable every restriction/pedantic lint or add broad `allow` attributes. Fix warnings; document a narrowly scoped exception only when genuinely necessary. Doc tests must run even if another check uses `--all-targets`.

## 12. Implementation tasks

Each task: write the specified failing tests, observe the relevant failure, implement the smallest change, rerun green, then format and Clippy-check the changed code. No product code before plan approval.

### Task 1: Foundation and workflow

**Files:** Cargo files, toolchain file, `main.rs`, `lib.rs`, `config.rs`.

- [ ] Refresh newest stable versions, create one crate with the exact toolchain and minimal dependency features, commit-ready lockfile, and thin binary.
- [ ] In `config.rs::tests`, add `unknown_fields_fail`, `unknown_dependency_fails`, `cycle_reports_path`, `duplicate_id_fails`, `defaults_are_bounded`, and `command_is_not_shell_interpolated`.
- [ ] Run `cargo test --locked --lib config::tests` red.
- [ ] Implement `load`/`validate`, stable source-order dispatch, strict field combinations, and backend-scoped model resolution.
- [ ] Rerun green; confirm TOML resources resolve relative to the workflow, not the caller's cwd.

### Task 2: Receipts and ownership

**Files:** `store.rs`; store tests remain in that file.

- [ ] Test `write_is_atomic`, `write_error_is_not_missing`, `second_owner_is_refused`, `changed_answer_fails_integrity`, and `old_file_is_not_completion`.
- [ ] Run `cargo test --locked --lib store::tests` red.
- [ ] Implement std file locks, checked sync/rename snapshots, exclusive IDs, SHA-256 receipts, and immutable approval files.
- [ ] Rerun green with independent temporary directories. Add no database or event journal.

### Task 3: Native backends

**Files:** `backend.rs`, vendor result fixtures.

- [ ] Add colocated parameterized tests for all three invocations, opaque model handling, new-session flags, absent usage, malformed output, unsupported capabilities, and success/failure despite exit 0.
- [ ] Add Codex JSONL tests for missing terminal event, terminal failure, unknown events, and recoverable intermediate errors; Claude denial/error fixtures; Droid session/usage fixtures.
- [ ] Run `cargo test --locked --lib backend::tests` red.
- [ ] Implement probe/invocation/parse for all three, with Droid default and no shell-wrapper shortcut.
- [ ] Rerun green; an unavailable unreferenced backend does not fail another backend's workflow.

### Task 4: Child processes and cancellation

**Files:** `process.rs`, portable shell fixtures for Linux/macOS.

- [ ] Add colocated tests for spawn failure, nonzero exit, literal argv metacharacters, file-based stdin, timeout, and output-limit failure.
- [ ] Run `cargo test --locked --lib process::tests` red.
- [ ] Implement polling, owned process groups, signal-hook cancellation, TERM/KILL/reap, and redirected logs.
- [ ] Add real subprocess checks under `tests/cli.rs` for a child/grandchild tree, early parent exit, and coordinator interruption. Use handshakes/files, not timing-sensitive sleeps as assertions.
- [ ] Rerun unit and integration cases green; state the SIGKILL limitation rather than implementing a daemon.

### Task 5: Scheduling and the decision record

**Files:** `runner.rs`; runner tests remain colocated.

- [ ] Add `failed_dependency_prevents_dispatch`, `fanin_waits_for_both`, `parallel_limit_holds`, `independent_branch_finishes`, `gate_hash_mismatch_fails`, and `unexplained_pending_is_error`.
- [ ] Add waiting/pending resume, committed-answer reuse, interrupted-step refusal, deadline, and storage-failure tests. Use a small private clock parameter where needed, not a generalized clock framework.
- [ ] Run `cargo test --locked --lib runner::tests` red.
- [ ] Implement the single-writer scheduler, bounded thread workers, approval polling, immutable input handoff, and deterministic report. No retries or hidden calls.
- [ ] Rerun green; demonstrate a mixed-backend fixture with one shared process limit and preserved disagreements.

### Task 6: Cells and ordered integration

**Files:** `workspace.rs`; Git tests remain in that file using temporary repositories.

- [ ] Test dirty-source refusal and that the user's HEAD, index, branches, and files are unchanged after a run.
- [ ] Test `independent_edits_overlap`: two editing steps with no dependency path are running at the same time (prove it with handshake files, not sleeps).
- [ ] Test `integration_follows_declaration_order`: when the second-declared step finishes first, it still integrates second, and the final tree is identical across both completion orders.
- [ ] Test `conflict_fails_step_and_keeps_comb`: two cells edit the same line; the later-declared step becomes `Conflicted`, the comb tip is unchanged, its descendants are blocked, and an unrelated third cell still integrates.
- [ ] Test `dependent_cell_starts_from_integrated_tip`, `missing_git_identity_is_infrastructure_failure`, reserved `.hivequeen/` exclusion, binary/new/deleted files, and submodule/symlink refusal.
- [ ] Test that failed or interrupted editing steps never integrate or resume, and that a patch is labeled unverified unless a check passed on the final tip.
- [ ] Run `cargo test --locked --lib workspace::tests` red.
- [ ] Implement cells, queen-only Git operations, snapshot commits under `refs/hivequeen/<run>/`, the declaration-order integration queue, and patch export. Refuse conflicts rather than resolving them.
- [ ] Rerun green and apply the resulting patch in a separate clean test clone.

### Task 7: CLI and first usable release

**Files:** finish `main.rs`/`lib.rs`, `tests/cli.rs`, `tests/common/mod.rs`, README, LICENSE, CI.

- [ ] Test public commands, all exit classes, draft-not-executed, missing CLI diagnostics, offline approval then resume, and interrupted-state reporting.
- [ ] Run `cargo test --locked --test cli` red; implement the thin command dispatch and one-call draft writer.
- [ ] Add one command-only example and one mixed-backend example with complete prompts/input/check files. No template generator needed.
- [ ] Run all five gates in section 11. Test Linux and macOS; run the binary without Rust/Node/Python installed. Report OS/libc requirements instead of claiming all builds are static.
- [ ] Verify `cargo install --locked --path . --bin hivequeen --root <temporary-prefix>` and optional authorized smoke tests separately for each backend.
- [ ] Check the production LOC budget, direct-dependency list, and name availability again. Propose MIT for original code; do not publish anything without an explicit request.

## 13. Done means

- The latest stable version checks are dated and the lockfile/toolchain agree.
- Formatting, Clippy, unit tests, integration tests, doc tests, and release build pass.
- Default operation uses Droid; Claude and Codex have equally real tested adapters.
- Independent scratch and editing steps overlap; integration order matches declaration order, and conflicts fail loudly.
- Failed checks, malformed backend results, storage errors, corrupt answers, and ambiguous crashes never become success.
- A fresh installation can run the command-only example with no provider configured.
- A user can inspect the exact evidence/checks and choose whether to apply the patch.
- No comparison branding, copied implementation material, unnecessary framework, or unrequested service appears in the product.

## References

- [Official stable toolchain manifest](https://static.rust-lang.org/dist/channel-rust-stable.toml), observed 2026-11-02.
- crates.io registry metadata for [clap](https://crates.io/api/v1/crates/clap), [serde](https://crates.io/api/v1/crates/serde), [serde_json](https://crates.io/api/v1/crates/serde_json), [toml](https://crates.io/api/v1/crates/toml), [nix](https://crates.io/api/v1/crates/nix), [signal-hook](https://crates.io/api/v1/crates/signal-hook), [sha2](https://crates.io/api/v1/crates/sha2), [tempfile](https://crates.io/api/v1/crates/tempfile), observed 2026-11-02.
- *The Rust Programming Language*: [Writing Tests](https://doc.rust-lang.org/book/ch11-01-writing-tests.html), [Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html).
- [Clippy usage](https://doc.rust-lang.org/clippy/usage.html).
- Public CLI contracts: [Droid](https://docs.factory.com/droid-exec/overview.md), [Claude Code](https://code.claude.com/docs/en/headless.md), [Codex](https://developers.openai.com/codex/noninteractive). The implementation must verify installed CLI capabilities; documentation is not proof of live compatibility.
