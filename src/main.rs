mod config;
mod context;
mod diff;
mod git;
mod hooks;
mod lang;
mod llm;
mod prompt;
mod review;
mod run;
mod search;
mod watch;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use hooks::{Event, Harness, HookArgs};
use llm::ProviderKind;
use review::Severity;
use run::Progress;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// AI code review for AI agents.
///
/// Sends your git diff, the full changed files, and the relevant code from
/// the rest of the repo to a model on OpenRouter (or a local Ollama /
/// llama.cpp server) and prints structured findings. Exit code 1 when there
/// are findings at or above --fail-on, so an agent can loop until clean.
///
/// `nitpick watch` reviews in the background while an agent works, driven
/// by the agent's own hooks; see `nitpick watch --help`.
#[derive(Parser, Debug)]
#[command(name = "nitpick", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    review: ReviewArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Review the current changes (the default when no subcommand is given).
    Review(ReviewArgs),
    /// Print the context that would be sent to the model, without calling one.
    Context(ReviewArgs),
    /// Write a starter .nitpick.toml into the repository root.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Background review while an agent works: install the hooks, see what
    /// the reviewer found, or run one incremental review by hand.
    ///
    /// Each edit the agent makes is recorded by a hook. After a quiet period a
    /// worker diffs every changed file against the copy it reviewed last time,
    /// runs that small diff through the normal review, and queues the findings.
    /// The next hook hands them to the agent as a "[nitpick]" note. When the
    /// agent finishes, reviews continue in the background. Set [watch].max_stop_blocks
    /// above zero to wait and send it back for serious findings. Configure it in the [watch] section of .nitpick.toml.
    Watch {
        #[command(subcommand)]
        command: WatchCommand,
    },
    /// Entry point for agent hooks: reads the harness's JSON on stdin and
    /// prints what that harness expects. Installed by `nitpick watch install`.
    ///
    /// The generic harness prints {"context", "block", "reason", "note"} and
    /// takes --cwd and --tool for callers that cannot write stdin.
    Hook {
        #[arg(value_enum)]
        harness: Harness,
        #[arg(value_enum)]
        event: Event,
        /// Repository (or any directory inside it). Defaults to the payload's `cwd`.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Name of the tool that just ran, for the `tool` event.
        #[arg(long)]
        tool: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum WatchCommand {
    /// Add nitpick's hooks to a harness. Project-level by default.
    Install {
        #[arg(value_enum)]
        harness: Harness,
        /// Install for every project (the harness's user-level settings).
        #[arg(long, short)]
        global: bool,
    },
    /// Remove the hooks `install` added.
    Uninstall {
        #[arg(value_enum)]
        harness: Harness,
        #[arg(long, short)]
        global: bool,
    },
    /// Show whether watch is on, what is waiting to be reviewed, and recent activity.
    Status,
    /// Print the findings of past background reviews, newest first.
    Log {
        /// How many reviews to show (default 10).
        #[arg(short, long, default_value_t = 10)]
        count: usize,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Review everything changed since the last background review, now, in
    /// the foreground. Exit 1 on findings at or above [watch].fail_on.
    Run {
        #[arg(long)]
        json: bool,
        #[arg(short, long)]
        quiet: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Forget every baseline and queued finding for this checkout.
    Reset,
    /// Print the hook definitions `install` would write, as JSON.
    Show {
        #[arg(value_enum)]
        harness: Harness,
    },
    /// The background worker. Started by the hooks; not meant to be run by hand.
    #[command(hide = true)]
    Worker,
}

#[derive(Args, Debug, Clone, Default)]
struct ReviewArgs {
    /// Limit the review to these paths (files or directories).
    paths: Vec<String>,

    /// Model id(s). Repeat or comma-separate to run several in parallel.
    #[arg(short, long, env = "NITPICK_MODEL", value_delimiter = ',')]
    model: Vec<String>,

    /// Where to send the request.
    #[arg(long, env = "NITPICK_PROVIDER", value_enum)]
    provider: Option<ProviderKind>,

    /// Override the API base URL (e.g. http://localhost:11434/v1).
    #[arg(long, env = "NITPICK_BASE_URL")]
    base_url: Option<String>,

    /// API key. Defaults to NITPICK_OPENROUTER_API_KEY / OPENROUTER_API_KEY.
    #[arg(long, env = "NITPICK_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Base branch or commit to diff against (default: auto-detect).
    #[arg(short, long)]
    base: Option<String>,

    /// Review only staged changes.
    #[arg(long, conflicts_with_all = ["base", "range"])]
    staged: bool,

    /// Review an explicit revision range, e.g. main..HEAD or abc123.
    #[arg(long, conflicts_with = "base")]
    range: Option<String>,

    /// Do not include untracked files.
    #[arg(long)]
    no_untracked: bool,

    /// Send only the diff and changed files, no related context.
    #[arg(long)]
    no_context: bool,

    /// Do not pull matching test files into the context.
    #[arg(long)]
    no_tests: bool,

    /// Token budget for the context (default 80000).
    #[arg(long)]
    budget: Option<usize>,

    /// Files longer than this are windowed around the changes (default 400).
    #[arg(long)]
    max_file_lines: Option<usize>,

    /// Extra instructions for the reviewer. Repeatable.
    #[arg(short = 'f', long)]
    focus: Vec<String>,

    /// Exit 1 when a finding is at or above this severity (default: high).
    #[arg(long, value_enum)]
    fail_on: Option<Severity>,

    /// Print JSON instead of markdown.
    #[arg(long)]
    json: bool,

    /// Per-model request timeout in seconds (default 300).
    #[arg(long)]
    timeout: Option<u64>,

    /// Max completion tokens (default 16000; doubled automatically if the model runs out).
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Reasoning effort for models that support it: none, low, medium, high.
    #[arg(long, env = "NITPICK_REASONING")]
    reasoning: Option<String>,

    /// Never request structured output (response_format); some backends reject it.
    #[arg(long)]
    no_structured: bool,

    /// Sampling temperature (default 0.1).
    #[arg(long)]
    temperature: Option<f32>,

    /// Suppress progress output on stderr.
    #[arg(short, long)]
    quiet: bool,

    /// Print timing and token details on stderr.
    #[arg(short, long)]
    verbose: bool,
}

impl ReviewArgs {
    fn overrides(&self) -> run::Overrides {
        run::Overrides {
            models: self.model.clone(),
            provider: self.provider,
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            fail_on: self.fail_on,
            focus: self.focus.clone(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            timeout_secs: self.timeout,
            reasoning: self.reasoning.clone(),
            no_structured: self.no_structured,
            budget: self.budget,
            max_file_lines: self.max_file_lines,
            no_context: self.no_context,
            no_tests: self.no_tests,
            no_untracked: self.no_untracked,
            paths: self.paths.clone(),
        }
    }

    fn progress(&self) -> Progress {
        if self.quiet {
            Progress::Quiet
        } else if self.verbose {
            Progress::Verbose
        } else {
            Progress::Normal
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Init { force }) => run_init(force),
        Some(Command::Context(args)) => run_context(args),
        Some(Command::Review(args)) => run_review(args),
        Some(Command::Watch { command }) => run_watch(command),
        Some(Command::Hook { harness, event, cwd, tool }) => Ok(hooks::run(harness, event, &HookArgs { cwd, tool })),
        None => run_review(cli.review),
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("nitpick: error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn run_init(force: bool) -> Result<i32> {
    let repo = git::Repo::discover(Path::new("."))?;
    let path = repo.root.join(".nitpick.toml");
    if path.exists() && !force {
        bail!("{} already exists (use --force to overwrite)", path.display());
    }
    std::fs::write(&path, config::STARTER)?;
    println!("wrote {}", path.display());
    Ok(0)
}

fn settings(args: &ReviewArgs, repo: &git::Repo) -> Result<(run::Settings, git::DiffMode)> {
    let file = config::load(&repo.root)?;
    let s = run::resolve(&file, &args.overrides())?;
    let base = args.base.as_deref().or(file.base.as_deref());
    let mode = repo.resolve_mode(base, args.staged, args.range.as_deref())?;
    Ok((s, mode))
}

fn run_context(args: ReviewArgs) -> Result<i32> {
    let repo = git::Repo::discover(Path::new("."))?;
    let (s, mode) = settings(&args, &repo)?;
    let pack = context::build(&repo, &mode, &s.ctx)?;
    if pack.is_empty() {
        eprintln!("nitpick: no changes to review ({})", mode.label());
        return Ok(0);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&pack)?);
    } else {
        print!("{}", prompt::user_message(&pack, &s.instructions));
    }
    if !args.quiet {
        let st = &pack.stats;
        eprintln!(
            "nitpick: {} file(s), ~{} tokens (diff {}, files {}, {} snippets {}; {} dropped), {} repo files scanned, built in {}ms",
            st.files_changed,
            st.estimated_tokens,
            st.diff_tokens,
            st.files_tokens,
            st.snippets_kept,
            st.snippet_tokens,
            st.snippets_dropped,
            st.files_scanned,
            st.build_ms
        );
    }
    Ok(0)
}

fn run_review(args: ReviewArgs) -> Result<i32> {
    let total = Instant::now();
    let repo = git::Repo::discover(Path::new("."))?;
    let (s, mode) = settings(&args, &repo)?;
    let pack = context::build(&repo, &mode, &s.ctx)?;
    if pack.is_empty() {
        if args.json {
            println!(
                "{{\"verdict\":\"approve\",\"findings\":[],\"note\":\"no changes to review\",\"mode\":{:?}}}",
                mode.label()
            );
        } else {
            println!("No changes to review ({}).", mode.label());
        }
        return Ok(0);
    }

    // Only now do we need credentials: a clean tree should exit 0 without them.
    let results = run::review_pack(&pack, &s, args.progress())?;
    let merged = review::merge(&results);
    let input = review::RenderInput {
        mode_label: &pack.mode_label,
        branch: pack.branch.as_deref(),
        head: &pack.head,
        stats: &pack.stats,
        results: &results,
        merged: &merged,
        fail_on: s.fail_on,
        total_ms: total.elapsed().as_millis(),
    };
    if args.json {
        println!("{}", review::render_json(&input));
    } else {
        print!("{}", review::render_markdown(&input));
    }
    let failing = merged.findings.iter().any(|f| f.severity >= s.fail_on);
    Ok(if failing { 1 } else { 0 })
}

fn run_watch(cmd: WatchCommand) -> Result<i32> {
    let repo = git::Repo::discover_watch(Path::new("."))?;
    match cmd {
        WatchCommand::Install { harness, global } => {
            let path = hooks::install(harness, global, &repo.root)?;
            println!("installed nitpick hooks for {} in {}", harness.name(), path.display());
            println!(
                "scope: {}",
                if global {
                    "global (all workspaces for this user)"
                } else {
                    "this workspace only; add --global for all workspaces"
                }
            );
            match harness {
                Harness::Codex => println!(
                    "Restart Codex, open /hooks (or the app Hooks settings), and enable AND trust all four nitpick hooks. Global hooks appear under User config. Installing the plugin alone does not prove hooks are active."
                ),
                Harness::Cursor => println!("Cursor picks the file up on the next agent run."),
                Harness::Pi => {
                    println!("pi loads it on the next start (project files need the project to be trusted).")
                }
                Harness::Opencode => println!("OpenCode loads it on the next start."),
                Harness::Openclaw => println!(
                    "Enable it with `openclaw plugins enable nitpick` and set plugins.entries.nitpick.hooks.allowConversationAccess = true in openclaw.json."
                ),
                _ => {}
            }
            println!(
                "GUI hooks may lack shell credentials: configure api_key in the user-level nitpick config with owner-only permissions."
            );
            println!(
                "Verify in a fresh session after an edit: `nitpick watch status` must show a completed review, not just enabled: yes."
            );
            Ok(0)
        }
        WatchCommand::Uninstall { harness, global } => {
            match hooks::uninstall(harness, global, &repo.root)? {
                Some(path) => println!("removed nitpick hooks from {}", path.display()),
                None => println!("no nitpick hooks found for {}", harness.name()),
            }
            Ok(0)
        }
        WatchCommand::Show { harness } => {
            let v = match harness {
                Harness::Cursor => serde_json::json!({"version": 1, "hooks": hooks::cursor_hooks()}),
                Harness::Claude | Harness::Codex => serde_json::json!({"hooks": hooks::claude_style_hooks(harness)}),
                Harness::Pi => {
                    print!("{}", hooks::PI_SHIM);
                    return Ok(0);
                }
                Harness::Opencode => {
                    print!("{}", hooks::OPENCODE_SHIM);
                    return Ok(0);
                }
                Harness::Openclaw => {
                    for (name, content) in hooks::OPENCLAW_SHIM {
                        println!("// ---- {name}\n{content}");
                    }
                    return Ok(0);
                }
                Harness::Generic => bail!("the generic harness has no fixed hook file; see `nitpick hook --help`"),
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(0)
        }
        WatchCommand::Status => {
            print!("{}", watch::status(&repo)?);
            Ok(0)
        }
        WatchCommand::Log { count, json } => {
            let Some(state) = watch::State::existing(&repo) else {
                println!("No background reviews yet.");
                return Ok(0);
            };
            let reports: Vec<watch::Report> = state.all_reports().into_iter().take(count).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else if reports.is_empty() {
                println!("No background reviews yet.");
            } else {
                print!("{}", watch::render_reports(&reports));
            }
            Ok(0)
        }
        WatchCommand::Run { json, quiet, verbose } => {
            let progress = if quiet {
                Progress::Quiet
            } else if verbose {
                Progress::Verbose
            } else {
                Progress::Normal
            };
            watch::run_once(&repo, progress, json)
        }
        WatchCommand::Reset => {
            watch::reset(&repo)?;
            println!("watch state cleared");
            Ok(0)
        }
        WatchCommand::Worker => {
            watch::worker(&repo)?;
            Ok(0)
        }
    }
}
