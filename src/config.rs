//! `.nitpick.toml` in the repo root, layered over an optional user-level
//! file (`~/.config/nitpick/config.toml`). Every key is optional; CLI flags
//! and environment variables override both.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelSpec {
    One(String),
    Many(Vec<String>),
}

impl ModelSpec {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            ModelSpec::One(s) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            ModelSpec::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub model: Option<ModelSpec>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub base: Option<String>,
    pub fail_on: Option<String>,
    pub budget_tokens: Option<usize>,
    pub max_file_lines: Option<usize>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning: Option<String>,
    pub structured: Option<bool>,
    pub timeout_secs: Option<u64>,
    pub include_tests: Option<bool>,
    pub ignore: Vec<String>,
    pub instructions: Option<String>,
    /// API key in the user-level config only, for agents launched from a GUI
    /// where the shell environment is not available. Ignored in repo files.
    pub api_key: Option<String>,
    pub watch: WatchConfig,
}

/// `[watch]`: the background reviewer driven by agent hooks. Unset keys fall
/// back to the top-level value, then to the defaults documented in STARTER.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    pub enabled: Option<bool>,
    pub model: Option<ModelSpec>,
    /// Lowest severity delivered to the agent while it works (default medium).
    pub deliver: Option<String>,
    /// Lowest severity that keeps the agent from stopping (default: top-level fail_on, else high).
    pub fail_on: Option<String>,
    /// Quiet period after the last edit before a review starts (default 20).
    pub debounce_secs: Option<u64>,
    /// Review anyway once edits have been arriving for this long (default 120).
    pub max_wait_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub budget_tokens: Option<usize>,
    /// How long the stop hook waits for an in-flight review (default 120).
    pub stop_wait_secs: Option<u64>,
    /// How many times in a row the stop hook may send the agent back (default 2).
    pub max_stop_blocks: Option<u32>,
    pub instructions: Option<String>,
}

pub const FILE_NAMES: &[&str] = &[".nitpick.toml", "nitpick.toml"];

/// `~/.config/nitpick/config.toml` (or `$XDG_CONFIG_HOME/nitpick/config.toml`).
pub fn user_config_path() -> Option<PathBuf> {
    // The XDG spec says a relative value must be ignored.
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && Path::new(&x).is_absolute()
    {
        return Some(PathBuf::from(x).join("nitpick").join("config.toml"));
    }
    let home = std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())?;
    Some(PathBuf::from(home).join(".config").join("nitpick").join("config.toml"))
}

fn read(p: &Path) -> Result<FileConfig> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

/// Repo config layered over the user config: any key set in the repo wins.
pub fn load(root: &Path) -> Result<FileConfig> {
    let mut base = match user_config_path() {
        Some(p) if p.is_file() => read(&p)?,
        _ => FileConfig::default(),
    };
    for name in FILE_NAMES {
        let p = root.join(name);
        if p.is_file() {
            let mut repo = read(&p)?;
            // A key in the repo must never be able to point at a secret.
            repo.api_key = None;
            merge(&mut base, repo);
            return Ok(base);
        }
    }
    Ok(base)
}

fn merge(base: &mut FileConfig, over: FileConfig) {
    macro_rules! take {
        ($($f:ident),*) => { $( if over.$f.is_some() { base.$f = over.$f; } )* };
    }
    take!(
        model,
        provider,
        base_url,
        api_key_env,
        base,
        fail_on,
        budget_tokens,
        max_file_lines,
        max_tokens,
        temperature,
        reasoning,
        structured,
        timeout_secs,
        include_tests,
        instructions
    );
    // Ignore globs accumulate: the user's list plus the repo's.
    for g in over.ignore {
        if !base.ignore.contains(&g) {
            base.ignore.push(g);
        }
    }
    let w = &mut base.watch;
    let o = over.watch;
    macro_rules! take_w {
        ($($f:ident),*) => { $( if o.$f.is_some() { w.$f = o.$f; } )* };
    }
    take_w!(
        enabled,
        model,
        deliver,
        fail_on,
        debounce_secs,
        max_wait_secs,
        timeout_secs,
        budget_tokens,
        stop_wait_secs,
        max_stop_blocks,
        instructions
    );
}

pub const DEFAULT_MODEL: &str = "stealth/space-bunny-alpha";

pub const STARTER: &str = r#"# nitpick configuration. Every key is optional.
# CLI flags and NITPICK_* environment variables override these values.

# One model or a list. Several models run in parallel and their findings are merged.
model = "stealth/space-bunny-alpha"
# model = ["stealth/space-bunny-alpha", "nvidia/nemotron-3-ultra-550b-a55b:free"]

# openrouter (default) | ollama | llamacpp | openai (any OpenAI-compatible endpoint)
# provider = "openrouter"
# base_url = "http://localhost:11434/v1"
# api_key_env = "NITPICK_OPENROUTER_API_KEY"

# Branch to diff against. Auto-detected from origin/HEAD, main, or master when unset.
# base = "main"

# Exit 1 when any finding is at or above this severity: blocker | high | medium | low | nit
fail_on = "high"

# Approximate token budget for the context sent to the model.
budget_tokens = 80000

# Files longer than this are shown as windows around the changed lines.
max_file_lines = 400

# Files to leave out of the review entirely (gitignore-style globs).
ignore = ["**/*.lock", "**/*.snap", "**/generated/**"]

# Reasoning effort for models that support it (OpenRouter): none | low | medium | high.
# reasoning = "medium"

# Set to false to never request structured output (response_format).
# structured = true

# Extra instructions for the reviewer. Project conventions, what to be strict about, what to ignore.
instructions = """
"""

# Background review while an agent works (driven by the agent's hooks; see `nitpick watch --help`).
[watch]
# enabled = true
# A cheaper or free model for the many small reviews. Defaults to the model above.
# model = "nvidia/nemotron-3-ultra-550b-a55b:free"
# Lowest severity handed to the agent mid-task: blocker | high | medium | low | nit
deliver = "medium"
# Lowest severity that sends the agent back to work when it tries to stop. Defaults to fail_on above.
# fail_on = "high"
# Seconds of quiet after the last edit before a review starts, and the longest a review is postponed.
debounce_secs = 20
max_wait_secs = 120
# Per-request timeout for watch reviews; free models can hang.
timeout_secs = 180
# Extra instructions for the background reviewer only.
# instructions = ""
"#;
