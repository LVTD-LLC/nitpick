mod config;
mod context;
mod diff;
mod git;
mod lang;
mod llm;
mod prompt;
mod review;
mod search;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use llm::ProviderKind;
use review::{ModelResult, Severity};
use std::path::Path;
use std::time::{Duration, Instant};

/// AI code review for AI agents.
///
/// Sends your git diff, the full changed files, and the relevant code from
/// the rest of the repo to a model on OpenRouter (or a local Ollama /
/// llama.cpp server) and prints structured findings. Exit code 1 when there
/// are findings at or above --fail-on, so an agent can loop until clean.
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

    /// Max completion tokens (default 8000).
    #[arg(long)]
    max_tokens: Option<u32>,

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

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Init { force }) => run_init(force),
        Some(Command::Context(args)) => run_context(args),
        Some(Command::Review(args)) => run_review(args),
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

struct Settings {
    models: Vec<String>,
    provider: llm::Provider,
    fail_on: Severity,
    instructions: Vec<String>,
    request: llm::RequestOpts,
    ctx: context::Options,
}

fn settings(args: &ReviewArgs, repo: &git::Repo, need_provider: bool) -> Result<(Settings, git::DiffMode)> {
    let file = config::load(&repo.root)?;

    let models: Vec<String> = if !args.model.is_empty() {
        args.model.clone()
    } else if let Some(m) = file.model.clone() {
        m.into_vec()
    } else {
        vec![config::DEFAULT_MODEL.to_string()]
    };
    let models: Vec<String> = models.into_iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if models.is_empty() {
        bail!("no model configured");
    }

    let kind = match (args.provider, file.provider.as_deref()) {
        (Some(k), _) => k,
        (None, Some(p)) => p.parse()?,
        (None, None) => ProviderKind::Openrouter,
    };
    let base_url = args.base_url.clone().or(file.base_url.clone());
    let provider = if need_provider {
        llm::Provider::resolve(kind, base_url, args.api_key.clone(), file.api_key_env.as_deref())?
    } else {
        llm::Provider { kind, base_url: base_url.unwrap_or_default(), api_key: None }
    };

    let fail_on = match (args.fail_on, file.fail_on.as_deref()) {
        (Some(s), _) => s,
        (None, Some(s)) => s.parse().map_err(|_| anyhow::anyhow!("invalid fail_on `{s}` in config"))?,
        (None, None) => Severity::High,
    };

    let mut instructions: Vec<String> = Vec::new();
    if let Some(i) = &file.instructions
        && !i.trim().is_empty()
    {
        instructions.push(i.trim().to_string());
    }
    instructions.extend(args.focus.iter().cloned());

    let request = llm::RequestOpts {
        max_tokens: args.max_tokens.or(file.max_tokens).unwrap_or(8000),
        temperature: args.temperature.or(file.temperature).unwrap_or(0.1),
        timeout: Duration::from_secs(args.timeout.or(file.timeout_secs).unwrap_or(300)),
    };

    let ctx = context::Options {
        budget_tokens: args.budget.or(file.budget_tokens).unwrap_or(80_000),
        max_file_lines: args.max_file_lines.or(file.max_file_lines).unwrap_or(400),
        window: 40,
        with_context: !args.no_context,
        include_tests: !args.no_tests && file.include_tests.unwrap_or(true),
        include_untracked: !args.no_untracked,
        paths: args.paths.clone(),
        ignore: file.ignore.clone(),
    };

    let base = args.base.as_deref().or(file.base.as_deref());
    let mode = repo.resolve_mode(base, args.staged, args.range.as_deref())?;

    Ok((Settings { models, provider, fail_on, instructions, request, ctx }, mode))
}

fn run_context(args: ReviewArgs) -> Result<i32> {
    let repo = git::Repo::discover(Path::new("."))?;
    let (s, mode) = settings(&args, &repo, false)?;
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
    let (s, mode) = settings(&args, &repo, true)?;
    let pack = context::build(&repo, &mode, &s.ctx)?;
    if pack.is_empty() {
        if args.json {
            println!("{{\"verdict\":\"approve\",\"findings\":[],\"note\":\"no changes to review\",\"mode\":{:?}}}", mode.label());
        } else {
            println!("No changes to review ({}).", mode.label());
        }
        return Ok(0);
    }

    let user = prompt::user_message(&pack, &s.instructions);
    let schema = review::schema();
    if !args.quiet {
        let st = &pack.stats;
        eprintln!(
            "nitpick: {} · {} file(s) · ~{} tokens context ({} snippets, {} dropped, {}ms) · {} via {}",
            mode.label(),
            st.files_changed,
            st.estimated_tokens,
            st.snippets_kept,
            st.snippets_dropped,
            st.build_ms,
            s.models.join(", "),
            s.provider.kind.name()
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let results: Vec<ModelResult> = rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(format!("nitpick/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        let futs = s.models.iter().map(|model| {
            let client = &client;
            let provider = &s.provider;
            let user = &user;
            let schema = &schema;
            let request = &s.request;
            let quiet = args.quiet;
            let verbose = args.verbose;
            async move {
                let started = Instant::now();
                let res = llm::complete(client, provider, model, prompt::SYSTEM, user, schema, request).await;
                match res {
                    Ok(c) => {
                        if verbose {
                            eprintln!(
                                "nitpick: {model} answered in {:.1}s (prompt {} / completion {} tokens{}{})",
                                c.elapsed_ms as f64 / 1000.0,
                                c.prompt_tokens.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
                                c.completion_tokens.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
                                c.cost_usd.map(|x| format!(", ${x:.4}")).unwrap_or_default(),
                                if c.used_schema { "" } else { ", no structured output" }
                            );
                        }
                        match review::parse_lenient(&c.content) {
                            Ok(r) => {
                                if !quiet && !verbose {
                                    eprintln!("nitpick: {model} done in {:.1}s, {} finding(s)", c.elapsed_ms as f64 / 1000.0, r.findings.len());
                                }
                                ModelResult {
                                    model: model.clone(),
                                    review: Some(r),
                                    error: None,
                                    elapsed_ms: c.elapsed_ms,
                                    prompt_tokens: c.prompt_tokens,
                                    completion_tokens: c.completion_tokens,
                                    cost_usd: c.cost_usd,
                                }
                            }
                            Err(e) => ModelResult {
                                model: model.clone(),
                                review: None,
                                error: Some(format!("{e:#}")),
                                elapsed_ms: c.elapsed_ms,
                                prompt_tokens: c.prompt_tokens,
                                completion_tokens: c.completion_tokens,
                                cost_usd: c.cost_usd,
                            },
                        }
                    }
                    Err(e) => {
                        if !quiet {
                            eprintln!("nitpick: {model} failed: {e:#}");
                        }
                        ModelResult {
                            model: model.clone(),
                            review: None,
                            error: Some(format!("{e:#}")),
                            elapsed_ms: started.elapsed().as_millis(),
                            prompt_tokens: None,
                            completion_tokens: None,
                            cost_usd: None,
                        }
                    }
                }
            }
        });
        Ok::<_, anyhow::Error>(futures::future::join_all(futs).await)
    })?;

    if results.iter().all(|r| r.review.is_none()) {
        let errs: Vec<String> = results.iter().map(|r| format!("{}: {}", r.model, r.error.as_deref().unwrap_or("?"))).collect();
        bail!("every model failed:\n  {}", errs.join("\n  "));
    }

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
