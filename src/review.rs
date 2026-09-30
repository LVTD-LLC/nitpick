//! Review result types, the JSON schema we ask models for, lenient parsing
//! of what they actually return, cross-model merging, and rendering.

use crate::context::Stats;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Nit,
    Low,
    Medium,
    High,
    Blocker,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Blocker => "Blocker",
            Severity::High => "High",
            Severity::Medium => "Medium",
            Severity::Low => "Low",
            Severity::Nit => "Nit",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Nit => "nit",
        }
    }
}

impl FromStr for Severity {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "blocker" | "critical" | "fatal" | "p0" => Severity::Blocker,
            "high" | "major" | "error" | "severe" | "p1" => Severity::High,
            "medium" | "moderate" | "warning" | "warn" | "normal" | "p2" => Severity::Medium,
            "low" | "minor" | "info" | "informational" | "p3" => Severity::Low,
            "nit" | "nitpick" | "style" | "suggestion" | "trivial" | "cosmetic" | "p4" => Severity::Nit,
            _ => return Err(()),
        })
    }
}

impl<'de> Deserialize<'de> for Severity {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(s.parse().unwrap_or(Severity::Medium))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Comment,
    RequestChanges,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::Comment => "comment",
            Verdict::RequestChanges => "request changes",
        }
    }
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?.to_ascii_lowercase();
        Ok(if s.contains("request") || s.contains("reject") || s.contains("block") || s.contains("changes") {
            Verdict::RequestChanges
        } else if s.contains("approv") || s.contains("lgtm") || s.contains("accept") || s.contains("pass") {
            Verdict::Approve
        } else {
            Verdict::Comment
        })
    }
}

fn de_opt_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Number(n) => n.as_u64().map(|x| x as u32),
        serde_json::Value::String(s) => {
            s.trim().split(|c: char| !c.is_ascii_digit()).next().and_then(|x| x.parse().ok())
        }
        _ => None,
    })
}

fn de_opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) if !s.trim().is_empty() && s.trim() != "null" => Some(s),
        serde_json::Value::Null => None,
        serde_json::Value::String(_) => None,
        other => Some(other.to_string()),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default = "default_severity", alias = "level", alias = "priority")]
    pub severity: Severity,
    #[serde(default = "default_category", alias = "type", alias = "kind")]
    pub category: String,
    #[serde(default, alias = "path", alias = "filename", alias = "file_path")]
    pub file: String,
    #[serde(
        default,
        deserialize_with = "de_opt_u32",
        alias = "line_number",
        alias = "start_line",
        alias = "lineNumber"
    )]
    pub line: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_u32", alias = "endLine", alias = "line_end")]
    pub end_line: Option<u32>,
    #[serde(default, alias = "summary", alias = "issue", alias = "headline")]
    pub title: String,
    #[serde(
        default,
        alias = "description",
        alias = "message",
        alias = "detail",
        alias = "details",
        alias = "explanation",
        alias = "rationale"
    )]
    pub body: String,
    #[serde(
        default,
        deserialize_with = "de_opt_string",
        alias = "fix",
        alias = "recommendation",
        alias = "suggested_fix",
        alias = "remediation"
    )]
    pub suggestion: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

fn default_severity() -> Severity {
    Severity::Medium
}

fn default_category() -> String {
    "other".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    #[serde(default, alias = "overview", alias = "assessment")]
    pub summary: String,
    #[serde(default = "default_verdict", alias = "decision", alias = "recommendation", alias = "status")]
    pub verdict: Verdict,
    #[serde(default, alias = "issues", alias = "comments", alias = "problems", alias = "review_comments")]
    pub findings: Vec<Finding>,
}

fn default_verdict() -> Verdict {
    Verdict::Comment
}

pub const CATEGORIES: &[&str] = &[
    "bug",
    "security",
    "performance",
    "correctness",
    "error_handling",
    "concurrency",
    "data_loss",
    "api",
    "test",
    "docs",
    "style",
    "other",
];

pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "verdict", "findings"],
        "properties": {
            "summary": {"type": "string", "description": "Two to four sentences: what the change does and your overall assessment."},
            "verdict": {"type": "string", "enum": ["approve", "comment", "request_changes"]},
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["severity", "category", "file", "line", "end_line", "title", "body", "suggestion"],
                    "properties": {
                        "severity": {"type": "string", "enum": ["blocker", "high", "medium", "low", "nit"]},
                        "category": {"type": "string", "enum": CATEGORIES},
                        "file": {"type": "string", "description": "Repo-relative path exactly as shown in the input."},
                        "line": {"type": ["integer", "null"], "description": "Line number in the post-change file, from the numbered listing."},
                        "end_line": {"type": ["integer", "null"]},
                        "title": {"type": "string", "description": "One line, under 80 characters."},
                        "body": {"type": "string", "description": "Why this is a problem and how it manifests. Concrete: inputs, state, consequence."},
                        "suggestion": {"type": ["string", "null"], "description": "Concrete fix, with code if short."}
                    }
                }
            }
        }
    })
}

/// Accept fenced JSON, leading prose, trailing junk, and so on.
pub fn parse_lenient(text: &str) -> Result<Review> {
    let t = text.trim();
    if let Ok(r) = serde_json::from_str::<Review>(t) {
        return Ok(r);
    }
    let body = strip_fence(t);
    if let Ok(r) = serde_json::from_str::<Review>(body) {
        return Ok(r);
    }
    let start = body.find('{');
    let end = body.rfind('}');
    if let (Some(s), Some(e)) = (start, end)
        && e > s
    {
        let candidate = &body[s..=e];
        if let Ok(r) = serde_json::from_str::<Review>(candidate) {
            return Ok(r);
        }
        // Some models return a bare findings array under a different key.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            return from_loose_value(v).context("response JSON does not match the review schema");
        }
    }
    let excerpt: String = t.chars().take(300).collect();
    bail!("model did not return JSON. Response started with: {excerpt:?}")
}

fn from_loose_value(v: serde_json::Value) -> Result<Review> {
    let obj = v.as_object().context("not an object")?;
    let findings_v = obj
        .get("findings")
        .or_else(|| obj.get("issues"))
        .or_else(|| obj.get("comments"))
        .or_else(|| obj.get("problems"))
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    let findings: Vec<Finding> = serde_json::from_value(findings_v).unwrap_or_default();
    let summary = obj.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let verdict = obj.get("verdict").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or(Verdict::Comment);
    Ok(Review { summary, verdict, findings })
}

fn strip_fence(t: &str) -> &str {
    let t = t.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        let rest = rest.trim_start();
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
        return rest;
    }
    t
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelResult {
    pub model: String,
    pub review: Option<Review>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Merged {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
}

fn title_words(t: &str) -> HashSet<String> {
    t.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(String::from)
        .collect()
}

fn same_finding(a: &Finding, b: &Finding) -> bool {
    if a.file != b.file {
        return false;
    }
    let near = match (a.line, b.line) {
        (Some(x), Some(y)) => x.abs_diff(y) <= 6,
        (None, None) => true,
        _ => false,
    };
    if !near {
        return false;
    }
    let (wa, wb) = (title_words(&a.title), title_words(&b.title));
    if wa.is_empty() || wb.is_empty() {
        return a.category == b.category;
    }
    let inter = wa.intersection(&wb).count() as f32;
    let union = wa.union(&wb).count() as f32;
    let jaccard = inter / union;
    jaccard >= 0.34 || (a.category == b.category && a.line == b.line)
}

/// Merge findings from several models; agreeing findings collapse into one
/// with every model listed. Verdict is derived from the merged findings.
pub fn merge(results: &[ModelResult]) -> Merged {
    let mut findings: Vec<Finding> = Vec::new();
    for r in results {
        let Some(review) = &r.review else { continue };
        for f in &review.findings {
            let mut f = f.clone();
            f.models = vec![r.model.clone()];
            if f.file.starts_with("./") {
                f.file = f.file[2..].to_string();
            }
            if let Some(existing) = findings.iter_mut().find(|e| same_finding(e, &f)) {
                if !existing.models.contains(&r.model) {
                    existing.models.push(r.model.clone());
                }
                if f.severity > existing.severity {
                    existing.severity = f.severity;
                }
                if existing.suggestion.is_none() {
                    existing.suggestion = f.suggestion.clone();
                }
                if f.body.len() > existing.body.len() + 80 {
                    existing.body = f.body.clone();
                }
            } else {
                findings.push(f);
            }
        }
    }
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.models.len().cmp(&a.models.len()))
            .then(a.file.cmp(&b.file))
            .then(a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0)))
    });
    let verdict = if findings.iter().any(|f| f.severity >= Severity::High) {
        Verdict::RequestChanges
    } else if findings.is_empty() {
        Verdict::Approve
    } else {
        Verdict::Comment
    };
    Merged { verdict, findings }
}

pub struct RenderInput<'a> {
    pub mode_label: &'a str,
    pub branch: Option<&'a str>,
    pub head: &'a str,
    pub stats: &'a Stats,
    pub results: &'a [ModelResult],
    pub merged: &'a Merged,
    pub fail_on: Severity,
    pub total_ms: u128,
}

fn fmt_tokens(n: usize) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

pub fn render_markdown(input: &RenderInput) -> String {
    let mut out = String::new();
    let m = input.merged;
    out.push_str(&format!("# nitpick: {}\n\n", m.verdict.label()));
    let mut meta = vec![format!("`{}`", input.mode_label)];
    if let Some(b) = input.branch {
        meta.push(format!("branch `{b}`"));
    }
    meta.push(format!("head `{}`", input.head));
    meta.push(format!("{} file(s)", input.stats.files_changed));
    meta.push(format!("~{} tokens of context", fmt_tokens(input.stats.estimated_tokens)));
    let ok_models = input.results.iter().filter(|r| r.review.is_some()).count();
    meta.push(format!("{ok_models}/{} model(s)", input.results.len()));
    meta.push(format!("{:.1}s", input.total_ms as f64 / 1000.0));
    out.push_str(&meta.join(" · "));
    out.push_str("\n\n");

    for r in input.results {
        if let Some(rev) = &r.review {
            let cost = r.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default();
            out.push_str(&format!(
                "**{}** ({}, {:.1}s{}): {}\n\n",
                r.model,
                rev.verdict.label(),
                r.elapsed_ms as f64 / 1000.0,
                cost,
                rev.summary.trim()
            ));
        }
    }

    if m.findings.is_empty() {
        out.push_str("No findings.\n");
    } else {
        out.push_str(&format!("## Findings ({})\n", m.findings.len()));
        let mut current: Option<Severity> = None;
        for f in &m.findings {
            if current != Some(f.severity) {
                current = Some(f.severity);
                out.push_str(&format!("\n### {}\n\n", f.severity.label()));
            }
            let loc = match (f.line, f.end_line) {
                (Some(l), Some(e)) if e > l => format!("{}:{}-{}", f.file, l, e),
                (Some(l), _) => format!("{}:{}", f.file, l),
                _ => f.file.clone(),
            };
            let agree = if input.results.len() > 1 {
                format!(" ({}/{} models)", f.models.len(), input.results.len())
            } else {
                String::new()
            };
            out.push_str(&format!("- **{loc}** {} `[{}]`{}\n", f.title.trim(), f.category, agree));
            for line in f.body.trim().lines() {
                out.push_str(&format!("  {}\n", line.trim_end()));
            }
            if let Some(s) = &f.suggestion {
                let s = s.trim();
                if !s.is_empty() {
                    if s.contains('\n') {
                        out.push_str("  Suggestion:\n");
                        for line in s.lines() {
                            out.push_str(&format!("  {}\n", line.trim_end()));
                        }
                    } else {
                        out.push_str(&format!("  Suggestion: {s}\n"));
                    }
                }
            }
        }
    }

    let errors: Vec<&ModelResult> = input.results.iter().filter(|r| r.error.is_some()).collect();
    if !errors.is_empty() {
        out.push_str("\n## Errors\n\n");
        for r in errors {
            out.push_str(&format!("- {}: {}\n", r.model, r.error.as_deref().unwrap_or("")));
        }
    }

    let failing = m.findings.iter().filter(|f| f.severity >= input.fail_on).count();
    out.push('\n');
    if failing > 0 {
        out.push_str(&format!("{failing} finding(s) at or above `{}`. Exit code 1.\n", input.fail_on.as_str()));
    } else {
        out.push_str(&format!("No findings at or above `{}`. Exit code 0.\n", input.fail_on.as_str()));
    }
    out
}

pub fn render_json(input: &RenderInput) -> String {
    let v = serde_json::json!({
        "verdict": input.merged.verdict,
        "mode": input.mode_label,
        "branch": input.branch,
        "head": input.head,
        "stats": input.stats,
        "total_ms": input.total_ms,
        "fail_on": input.fail_on.as_str(),
        "failing": input.merged.findings.iter().filter(|f| f.severity >= input.fail_on).count(),
        "models": input.results.iter().map(|r| serde_json::json!({
            "model": r.model,
            "ok": r.review.is_some(),
            "error": r.error,
            "verdict": r.review.as_ref().map(|x| x.verdict),
            "summary": r.review.as_ref().map(|x| x.summary.clone()),
            "elapsed_ms": r.elapsed_ms,
            "prompt_tokens": r.prompt_tokens,
            "completion_tokens": r.completion_tokens,
            "cost_usd": r.cost_usd,
        })).collect::<Vec<_>>(),
        "findings": input.merged.findings,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_parse_fenced() {
        let t = "Here you go:\n```json\n{\"summary\":\"ok\",\"verdict\":\"approve\",\"findings\":[]}\n```";
        let r = parse_lenient(t).unwrap();
        assert_eq!(r.verdict, Verdict::Approve);
    }

    #[test]
    fn lenient_severity_and_line_strings() {
        let t = r#"{"summary":"s","verdict":"changes requested","findings":[{"severity":"critical","category":"bug","file":"a.rs","line":"12","end_line":null,"title":"t","body":"b","suggestion":null}]}"#;
        let r = parse_lenient(t).unwrap();
        assert_eq!(r.verdict, Verdict::RequestChanges);
        assert_eq!(r.findings[0].severity, Severity::Blocker);
        assert_eq!(r.findings[0].line, Some(12));
    }

    #[test]
    fn merge_collapses_agreeing_findings() {
        let f = |sev: Severity, line: u32, title: &str| Finding {
            severity: sev,
            category: "bug".into(),
            file: "a.rs".into(),
            line: Some(line),
            end_line: None,
            title: title.into(),
            body: "".into(),
            suggestion: None,
            models: vec![],
        };
        let mk = |model: &str, findings: Vec<Finding>| ModelResult {
            model: model.into(),
            review: Some(Review { summary: "".into(), verdict: Verdict::Comment, findings }),
            error: None,
            elapsed_ms: 0,
            prompt_tokens: None,
            completion_tokens: None,
            cost_usd: None,
        };
        let results = vec![
            mk(
                "m1",
                vec![
                    f(Severity::High, 10, "Null pointer dereference when list empty"),
                    f(Severity::Nit, 50, "Rename var"),
                ],
            ),
            mk("m2", vec![f(Severity::Medium, 12, "Possible null dereference on empty list")]),
        ];
        let merged = merge(&results);
        assert_eq!(merged.findings.len(), 2);
        assert_eq!(merged.findings[0].models, vec!["m1", "m2"]);
        assert_eq!(merged.findings[0].severity, Severity::High);
        assert_eq!(merged.verdict, Verdict::RequestChanges);
    }
}
