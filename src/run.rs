//! Shared review machinery: resolving settings from flags, environment and
//! config, and running a context pack through every configured model. Used
//! by the `review` command and by the watch worker.

use crate::config::FileConfig;
use crate::context::ContextPack;
use crate::llm::{self, ProviderKind};
use crate::review::{self, ModelResult, Severity};
use crate::{context, prompt};
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};

/// Everything a review needs besides the diff. Built by [`resolve`].
pub struct Settings {
    pub models: Vec<String>,
    pub provider_kind: ProviderKind,
    pub provider_base_url: Option<String>,
    pub provider_key_env: Option<String>,
    pub api_key: Option<String>,
    pub fail_on: Severity,
    pub instructions: Vec<String>,
    pub request: llm::RequestOpts,
    pub ctx: context::Options,
}

/// Per-invocation overrides, i.e. CLI flags. Everything is optional; unset
/// fields fall through to the config file and then to the defaults.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub models: Vec<String>,
    pub provider: Option<ProviderKind>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub fail_on: Option<Severity>,
    pub focus: Vec<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub timeout_secs: Option<u64>,
    pub reasoning: Option<String>,
    pub no_structured: bool,
    pub budget: Option<usize>,
    pub max_file_lines: Option<usize>,
    pub no_context: bool,
    pub no_tests: bool,
    pub no_untracked: bool,
    pub paths: Vec<String>,
}

pub fn resolve(file: &FileConfig, o: &Overrides) -> Result<Settings> {
    let models: Vec<String> = if !o.models.is_empty() {
        o.models.clone()
    } else if let Some(m) = file.model.clone() {
        m.into_vec()
    } else {
        vec![crate::config::DEFAULT_MODEL.to_string()]
    };
    let models: Vec<String> = models.into_iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if models.is_empty() {
        bail!("no model configured");
    }

    let kind = match (o.provider, file.provider.as_deref()) {
        (Some(k), _) => k,
        (None, Some(p)) => p.parse()?,
        (None, None) => ProviderKind::Openrouter,
    };
    let base_url = o.base_url.clone().or(file.base_url.clone());

    let fail_on = match (o.fail_on, file.fail_on.as_deref()) {
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
    instructions.extend(o.focus.iter().cloned());

    let request = llm::RequestOpts {
        max_tokens: o.max_tokens.or(file.max_tokens).unwrap_or(16_000),
        temperature: o.temperature.or(file.temperature).unwrap_or(0.1),
        timeout: Duration::from_secs(o.timeout_secs.or(file.timeout_secs).unwrap_or(300)),
        reasoning: o.reasoning.clone().or(file.reasoning.clone()).map(|r| r.trim().to_ascii_lowercase()),
        no_structured: o.no_structured || file.structured == Some(false),
    };
    if let Some(r) = &request.reasoning
        && !matches!(r.as_str(), "none" | "low" | "medium" | "high")
    {
        bail!("invalid reasoning effort `{r}` (expected none, low, medium, high)");
    }

    let ctx = context::Options {
        budget_tokens: o.budget.or(file.budget_tokens).unwrap_or(80_000),
        max_file_lines: o.max_file_lines.or(file.max_file_lines).unwrap_or(400),
        window: 40,
        with_context: !o.no_context,
        include_tests: !o.no_tests && file.include_tests.unwrap_or(true),
        include_untracked: !o.no_untracked,
        paths: o.paths.clone(),
        ignore: file.ignore.clone(),
    };

    Ok(Settings {
        models,
        provider_kind: kind,
        provider_base_url: base_url,
        provider_key_env: file.api_key_env.clone(),
        api_key: o.api_key.clone().or(file.api_key.clone()),
        fail_on,
        instructions,
        request,
        ctx,
    })
}

impl Settings {
    pub fn provider(&self) -> Result<llm::Provider> {
        llm::Provider::resolve(
            self.provider_kind,
            self.provider_base_url.clone(),
            self.api_key.clone(),
            self.provider_key_env.as_deref(),
        )
    }
}

/// How chatty [`review_pack`] is on stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Quiet,
    Normal,
    Verbose,
}

/// Send the pack to every model in parallel. Errors from individual models
/// are captured in the results; only a complete failure is an `Err`.
pub fn review_pack(pack: &ContextPack, s: &Settings, progress: Progress) -> Result<Vec<ModelResult>> {
    let provider = s.provider()?;
    let trace = crate::telemetry::Span::root("review");
    let user = prompt::user_message(pack, &s.instructions);
    let schema = review::schema();
    let system = prompt::system(&schema);
    if progress != Progress::Quiet {
        let st = &pack.stats;
        eprintln!(
            "nitpick: {} · {} file(s) · ~{} tokens context ({} snippets, {} dropped, {}ms) · {} via {}",
            pack.mode_label,
            st.files_changed,
            st.estimated_tokens,
            st.snippets_kept,
            st.snippets_dropped,
            st.build_ms,
            s.models.join(", "),
            provider.kind.name()
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let results: Vec<ModelResult> = rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(format!("nitpick/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        let futs = s.models.iter().map(|model| {
            let trace = &trace;
            let client = &client;
            let provider = &provider;
            let user = &user;
            let schema = &schema;
            let system = system.as_str();
            let request = &s.request;
            async move {
                let started = Instant::now();
                match llm::complete(client, provider, model, system, user, schema, request, trace).await {
                    Ok(c) => {
                        if progress == Progress::Verbose {
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
                                if progress == Progress::Normal {
                                    eprintln!(
                                        "nitpick: {model} done in {:.1}s, {} finding(s)",
                                        c.elapsed_ms as f64 / 1000.0,
                                        r.findings.len()
                                    );
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
                        if progress != Progress::Quiet {
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

    let failed = results.iter().filter(|r| r.review.is_none()).count();
    trace.finish(
        "$ai_trace",
        serde_json::json!({
            "models_count": results.len(), "failed_models": failed,
            "findings_count": results.iter().filter_map(|r| r.review.as_ref()).map(|r| r.findings.len()).sum::<usize>(),
            "files_changed": pack.stats.files_changed, "context_tokens_estimated": pack.stats.estimated_tokens,
            "context_build_ms": pack.stats.build_ms,
        }),
        failed > 0,
    );
    if failed > 0 {
        crate::telemetry::exception("model_review_failed");
    }
    // Worker reviews flush here rather than retaining data until the worker exits.
    crate::telemetry::flush_worker();
    if results.iter().all(|r| r.review.is_none()) {
        let errs: Vec<String> =
            results.iter().map(|r| format!("{}: {}", r.model, r.error.as_deref().unwrap_or("?"))).collect();
        bail!("every model failed:\n  {}", errs.join("\n  "));
    }
    Ok(results)
}
