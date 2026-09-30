//! `.nitpick.toml` in the repo root. Every key is optional; CLI flags and
//! environment variables override it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

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
    pub timeout_secs: Option<u64>,
    pub include_tests: Option<bool>,
    pub ignore: Vec<String>,
    pub instructions: Option<String>,
}

pub const FILE_NAMES: &[&str] = &[".nitpick.toml", "nitpick.toml"];

pub fn load(root: &Path) -> Result<FileConfig> {
    for name in FILE_NAMES {
        let p = root.join(name);
        if p.is_file() {
            let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            return toml::from_str(&text).with_context(|| format!("parsing {}", p.display()));
        }
    }
    Ok(FileConfig::default())
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

# Extra instructions for the reviewer. Project conventions, what to be strict about, what to ignore.
instructions = """
"""
"#;
