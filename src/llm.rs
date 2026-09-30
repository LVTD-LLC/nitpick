//! OpenAI-compatible chat client. OpenRouter, Ollama, llama.cpp and any
//! OpenAI-style endpoint all speak the same shape; only the base URL, auth
//! and structured-output support differ.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProviderKind {
    Openrouter,
    Ollama,
    Llamacpp,
    Openai,
}

impl ProviderKind {
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Openrouter => "https://openrouter.ai/api/v1",
            ProviderKind::Ollama => "http://localhost:11434/v1",
            ProviderKind::Llamacpp => "http://localhost:8080/v1",
            ProviderKind::Openai => "https://api.openai.com/v1",
        }
    }

    pub fn key_env_vars(self) -> &'static [&'static str] {
        match self {
            ProviderKind::Openrouter => &["NITPICK_OPENROUTER_API_KEY", "OPENROUTER_API_KEY"],
            ProviderKind::Openai => &["NITPICK_OPENAI_API_KEY", "OPENAI_API_KEY"],
            ProviderKind::Ollama | ProviderKind::Llamacpp => &[],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ProviderKind::Openrouter => "openrouter",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Llamacpp => "llama.cpp",
            ProviderKind::Openai => "openai",
        }
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace(['-', '.'], "").as_str() {
            "openrouter" => ProviderKind::Openrouter,
            "ollama" => ProviderKind::Ollama,
            "llamacpp" | "llama" => ProviderKind::Llamacpp,
            "openai" | "compatible" | "custom" => ProviderKind::Openai,
            other => bail!("unknown provider `{other}` (expected openrouter, ollama, llamacpp, openai)"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl Provider {
    pub fn resolve(
        kind: ProviderKind,
        base_url: Option<String>,
        api_key: Option<String>,
        key_env: Option<&str>,
    ) -> Result<Self> {
        let base_url =
            base_url.unwrap_or_else(|| kind.default_base_url().to_string()).trim_end_matches('/').to_string();
        let mut key = api_key.filter(|k| !k.trim().is_empty());
        if key.is_none()
            && let Some(env) = key_env
        {
            key = std::env::var(env).ok().filter(|k| !k.trim().is_empty());
        }
        if key.is_none() {
            for env in kind.key_env_vars() {
                if let Ok(k) = std::env::var(env)
                    && !k.trim().is_empty()
                {
                    key = Some(k);
                    break;
                }
            }
        }
        if key.is_none() && !kind.key_env_vars().is_empty() {
            bail!("no API key for {}. Set {} or pass --api-key.", kind.name(), kind.key_env_vars().join(" or "));
        }
        Ok(Self { kind, base_url, api_key: key })
    }
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub elapsed_ms: u128,
    pub used_schema: bool,
}

#[derive(Debug, Clone)]
pub struct RequestOpts {
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout: Duration,
    /// OpenRouter reasoning effort: none | low | medium | high. Unset leaves the model default.
    pub reasoning: Option<String>,
    /// Never send response_format; rely on the prompt and lenient parsing.
    pub no_structured: bool,
}

fn body(
    provider: &Provider,
    model: &str,
    system: &str,
    user: &str,
    schema: Option<&Value>,
    opts: &RequestOpts,
    max_tokens: u32,
) -> Value {
    let mut b = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "temperature": opts.temperature,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if let Some(schema) = schema {
        b["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "code_review", "strict": true, "schema": schema}
        });
    }
    if provider.kind == ProviderKind::Openrouter {
        b["usage"] = json!({"include": true});
        if let Some(effort) = opts.reasoning.as_deref() {
            b["reasoning"] =
                if effort == "none" { json!({"exclude": true, "effort": "low"}) } else { json!({"effort": effort}) };
        }
    }
    b
}

/// With NITPICK_DEBUG_DIR set, write every request and response to disk.
fn debug_dump(model: &str, request_no: u32, request: &Value, status: u16, response: &str) {
    let Ok(dir) = std::env::var("NITPICK_DEBUG_DIR") else { return };
    let safe: String = model.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let _ = std::fs::create_dir_all(&dir);
    let base = std::path::Path::new(&dir).join(format!("{safe}.{request_no}"));
    let _ = std::fs::write(
        format!("{}.request.json", base.display()),
        serde_json::to_string_pretty(request).unwrap_or_default(),
    );
    let _ = std::fs::write(format!("{}.response.{status}.txt", base.display()), response);
}

fn extract_content(v: &Value) -> Option<String> {
    let msg = v.get("choices")?.get(0)?.get("message")?;
    match msg.get("content") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let text: String = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
            if text.trim().is_empty() { None } else { Some(text) }
        }
        _ => {
            // Some backends put tool-style structured output elsewhere.
            msg.get("reasoning_content")
                .and_then(Value::as_str)
                .map(String::from)
                .filter(|s| s.trim_start().starts_with('{'))
        }
    }
}

/// One chat completion. Tries structured output first, then falls back to a
/// plain request if the backend rejects the schema. Retries transient errors.
pub async fn complete(
    client: &reqwest::Client,
    provider: &Provider,
    model: &str,
    system: &str,
    user: &str,
    schema: &Value,
    opts: &RequestOpts,
) -> Result<Completion> {
    let url = format!("{}/chat/completions", provider.base_url);
    let started = Instant::now();
    let mut use_schema = !opts.no_structured;
    let mut attempt = 0u32;
    let mut max_tokens = opts.max_tokens;
    let mut last_err: Option<anyhow::Error> = None;
    let mut request_no = 0u32;

    while attempt < 4 {
        attempt += 1;
        let b = body(provider, model, system, user, if use_schema { Some(schema) } else { None }, opts, max_tokens);
        let mut req = client.post(&url).timeout(opts.timeout).json(&b);
        if let Some(k) = &provider.api_key {
            req = req.bearer_auth(k);
        }
        if provider.kind == ProviderKind::Openrouter {
            req = req.header("HTTP-Referer", "https://nitpick.sh").header("X-Title", "nitpick");
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow::anyhow!("request failed: {e}"));
                if e.is_timeout() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                continue;
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        request_no += 1;
        debug_dump(model, request_no, &b, status.as_u16(), &text);
        if status.is_success() && text.trim().is_empty() {
            last_err = Some(anyhow::anyhow!("empty response body (HTTP {status})"));
            if use_schema {
                use_schema = false;
            } else {
                tokio::time::sleep(Duration::from_millis(1000 * attempt as u64)).await;
            }
            continue;
        }
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| text.chars().take(300).collect());
            // Any failure while asking for structured output is first blamed
            // on the schema: dropping it is free, and the parser copes.
            if use_schema {
                use_schema = false;
                attempt -= 1;
                continue;
            }
            if status.as_u16() == 429 || status.is_server_error() {
                last_err = Some(anyhow::anyhow!("HTTP {status}: {msg}"));
                tokio::time::sleep(Duration::from_millis(1500 * attempt as u64)).await;
                continue;
            }
            bail!("HTTP {status}: {msg}");
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("non-JSON response body: {}", text.chars().take(200).collect::<String>()))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string();
            let code = err.get("code").and_then(Value::as_u64).unwrap_or(0);
            let etype = err.pointer("/metadata/error_type").and_then(Value::as_str).unwrap_or("");
            let lower = msg.to_ascii_lowercase();
            if use_schema {
                use_schema = false;
                attempt -= 1;
                continue;
            }
            let transient = code == 429
                || code >= 500
                || etype.contains("unavailable")
                || lower.contains("rate")
                || lower.contains("overloaded")
                || lower.contains("try again")
                || lower.contains("timeout")
                || lower.contains("injected");
            if transient {
                last_err = Some(anyhow::anyhow!("provider error {code}: {msg}"));
                tokio::time::sleep(Duration::from_millis(1500 * attempt as u64)).await;
                continue;
            }
            bail!("{msg}");
        }
        let finish = v.pointer("/choices/0/finish_reason").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(content) = extract_content(&v) else {
            if finish == "length" && max_tokens < 64_000 {
                // Reasoning models can spend the whole budget thinking. Give them more room once.
                let reasoning =
                    v.pointer("/usage/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64).unwrap_or(0);
                last_err = Some(anyhow::anyhow!(
                    "model hit max_tokens={max_tokens} before producing output ({reasoning} reasoning tokens)"
                ));
                max_tokens = (max_tokens * 2).min(64_000);
                continue;
            }
            if use_schema {
                use_schema = false;
                continue;
            }
            bail!("empty response from model (finish_reason={finish})");
        };
        if finish == "length" && !content.trim_end().ends_with('}') && max_tokens < 64_000 {
            last_err = Some(anyhow::anyhow!("output truncated at max_tokens={max_tokens}"));
            max_tokens = (max_tokens * 2).min(64_000);
            continue;
        }
        let usage = v.get("usage");
        return Ok(Completion {
            content,
            prompt_tokens: usage.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64),
            completion_tokens: usage.and_then(|u| u.get("completion_tokens")).and_then(Value::as_u64),
            cost_usd: usage.and_then(|u| u.get("cost")).and_then(Value::as_f64),
            elapsed_ms: started.elapsed().as_millis(),
            used_schema: use_schema,
        });
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("gave up after {attempt} attempts")))
}
