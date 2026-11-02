use clap::{Parser, Subcommand};
use hivequeen::{Backend, Error, Options};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Run agent, command, and approval steps as a local workflow, with parallel
/// editing in isolated Git worktrees.
#[derive(Parser)]
#[command(name = "hivequeen", version)]
struct Cli {
    /// Directory for run state [default: $XDG_STATE_HOME/hivequeen]
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Backend settings file [default: $XDG_CONFIG_HOME/hivequeen/hivequeen.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask one agent to propose a workflow file. Nothing it proposes is run.
    Draft {
        question: String,
        #[arg(long, value_enum, default_value_t = Backend::Droid)]
        backend: Backend,
        #[arg(long, default_value = "workflow.toml")]
        out: PathBuf,
    },
    /// Validate a workflow without running anything.
    Check { workflow: PathBuf },
    /// Run a workflow in the foreground.
    Run {
        workflow: PathBuf,
        /// Repository for editing steps; it must be clean and is never modified.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Show step states for a run (id or path).
    Status {
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Continue a run that is waiting or stopped between steps.
    Resume { run: String },
    /// Approve a gate step that is waiting.
    Approve { run: String, step: String },
    /// Print the decision record for a run.
    Report { run: String },
    /// Check that backend CLIs are installed and support the needed flags.
    Doctor {
        #[arg(value_enum)]
        backends: Vec<Backend>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cancel = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let registered = [SIGINT, SIGTERM]
        .into_iter()
        .try_for_each(|sig| signal_hook::flag::register(sig, cancel.clone()).map(drop))
        .and_then(|()| signal_hook::flag::register(SIGINT, interrupted.clone()).map(drop));
    if let Err(e) = registered {
        eprintln!("hivequeen: cannot install signal handlers: {e}");
        return ExitCode::from(4);
    }
    let code = match dispatch(cli, cancel) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hivequeen: {e}");
            e.exit_code()
        }
    };
    let code = if interrupted.load(Ordering::Relaxed) {
        130
    } else {
        code
    };
    ExitCode::from(u8::try_from(code).unwrap_or(4))
}

fn dispatch(cli: Cli, cancel: Arc<AtomicBool>) -> Result<i32, Error> {
    let opts = || -> Result<Options, Error> {
        let settings = hivequeen::load_settings(cli.config.as_deref())?;
        Ok(Options {
            state_dir: cli.state_dir.clone(),
            settings,
            cancel: cancel.clone(),
        })
    };
    let print = |text: String| {
        println!("{}", text.trim_end());
        Ok(0)
    };
    match &cli.command {
        Cmd::Draft {
            question,
            backend,
            out,
        } => print(hivequeen::draft(question, *backend, out, &opts()?)?),
        Cmd::Check { workflow } => print(format!("ok: {}", hivequeen::check(workflow)?)),
        Cmd::Run { workflow, repo } => {
            finished(hivequeen::run(workflow, repo.as_deref(), &opts()?)?)
        }
        Cmd::Resume { run } => finished(hivequeen::resume(run, &opts()?)?),
        Cmd::Status { run, json } => print(hivequeen::status(run, *json, &opts()?)?),
        Cmd::Approve { run, step } => print(hivequeen::approve(run, step, &opts()?)?),
        Cmd::Report { run } => print(hivequeen::report(run, &opts()?)?),
        Cmd::Doctor { backends } => {
            let (ok, text) = hivequeen::doctor(backends, &opts()?)?;
            print!("{text}");
            Ok(if ok { 0 } else { 4 })
        }
    }
}

fn finished((dir, outcome): (PathBuf, hivequeen::Outcome)) -> Result<i32, Error> {
    println!("{}", dir.display());
    eprintln!(
        "hivequeen: {}; decision record at {}",
        outcome.label(),
        dir.join("decision.md").display()
    );
    Ok(outcome.exit_code())
}
