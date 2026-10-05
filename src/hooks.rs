//! `nitpick hook <harness> <event>`: the command an agent harness runs from
//! its lifecycle hooks. Reads the harness's JSON payload on stdin, does the
//! cheap part of the work (record an edit, start the worker, drain the
//! inbox, or run the stop check), and prints whatever JSON that harness
//! understands. Also `nitpick watch install`, which writes the hook entries
//! into the harness's settings.
//!
//! Nothing here may break the agent: any error is logged to stderr and the
//! command exits 0 with no output.

use crate::git::Repo;
use crate::watch::{self, State};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    /// Claude Code (settings.json hooks or a plugin's hooks/hooks.json).
    Claude,
    /// OpenAI Codex (hooks.json; same wire format as Claude Code).
    Codex,
    /// Cursor (.cursor/hooks.json).
    Cursor,
    /// pi coding agent (a TypeScript extension that calls `nitpick hook generic`).
    Pi,
    /// OpenCode (a TypeScript plugin that calls `nitpick hook generic`).
    Opencode,
    /// OpenClaw (a TypeScript plugin that calls `nitpick hook generic`).
    Openclaw,
    /// Any other harness: plain JSON in, plain JSON out. See `nitpick hook --help`.
    Generic,
}

impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Cursor => "cursor",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
            Harness::Openclaw => "openclaw",
            Harness::Generic => "generic",
        }
    }
}

/// Extra inputs for harnesses whose shims cannot write to stdin.
#[derive(Debug, Clone, Default)]
pub struct HookArgs {
    pub cwd: Option<PathBuf>,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Event {
    /// A session began: make the current working tree the baseline.
    SessionStart,
    /// A tool ran (an edit, a shell command): schedule a review, deliver waiting findings.
    Tool,
    /// The user sent a prompt: deliver waiting findings.
    Prompt,
    /// The agent wants to finish: review what is left and decide whether to let it.
    Stop,
}

/// Tools that cannot change files. Anything else counts as an edit; the
/// worker checks git for what actually changed, so a false positive only
/// costs a short wait.
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "TodoRead",
    "Task",
    "Agent",
    "AskUserQuestion",
    "ExitPlanMode",
    "EnterPlanMode",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
    "ToolSearch",
    "Skill",
    "update_plan",
    "spawn_agent",
    "view_image",
    "read_file",
    "list_dir",
    "grep_search",
    "file_search",
    "codebase_search",
    "web_search",
    "fetch_rules",
];

fn is_read_only(tool: &str) -> bool {
    READ_ONLY_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(tool)) || tool.starts_with("mcp__") && tool.contains("read")
}

struct Output {
    /// Text the agent should see.
    context: Option<String>,
    /// Stop only: send the agent back with this.
    block: Option<String>,
    /// Text for the user, not the agent.
    note: Option<String>,
}

fn read_stdin() -> Value {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    if buf.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(&buf).unwrap_or_else(|e| {
        eprintln!("nitpick hook: stdin is not JSON ({e}); continuing without it");
        Value::Object(Default::default())
    })
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str)).filter(|s| !s.is_empty())
}

pub const SESSION_NOTE: &str = "nitpick watch is active in this workspace: a background reviewer (a different model) checks the edits you make and may add notes prefixed [nitpick] with findings. Treat them as a second opinion: fix real problems when you reach a stopping point, dismiss wrong ones in a sentence. Nothing is required of you until a note appears.";

pub fn run(harness: Harness, event: Event, args: &HookArgs) -> i32 {
    match run_inner(harness, event, args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nitpick hook: {e:#}");
            0
        }
    }
}

fn run_inner(harness: Harness, event: Event, args: &HookArgs) -> Result<i32> {
    let input = read_stdin();
    let cwd = args
        .cwd
        .clone()
        .or_else(|| str_field(&input, &["cwd", "workspace_root", "project_dir"]).map(PathBuf::from))
        .or_else(|| {
            input
                .get("workspace_roots")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("CURSOR_PROJECT_DIR").map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let Ok(repo) = Repo::discover_watch(&cwd) else { return Ok(0) };
    let file = crate::config::load(&repo.root)?;
    let ws = watch::settings(&file)?;
    if !ws.enabled {
        return Ok(0);
    }
    let state = State::open(&repo)?;

    let out = match event {
        Event::SessionStart => {
            // Claude Code and Codex also fire this on resume, compaction and
            // fork, when the same work continues: keep the baselines then and
            // only re-send the note, since the agent's context was rebuilt.
            let source = str_field(&input, &["source", "reason"]).unwrap_or("startup");
            if matches!(source, "compact" | "resume" | "fork" | "reload") {
                state.log(&format!("session {source} ({}): baselines kept", harness.name()));
            } else {
                let n = watch::snapshot(&repo, &state)?;
                state.log(&format!("session start ({}): baseline set, {n} dirty file(s) excluded", harness.name()));
                // A fresh session should not inherit stale blocks or findings
                // the previous agent never got to see.
                let _ = state.drain_reports();
                state.set_counter("stop_blocks", 0);
            }
            Output { context: Some(SESSION_NOTE.to_string()), block: None, note: None }
        }
        Event::Tool => {
            let tool = args.tool.as_deref().or_else(|| str_field(&input, &["tool_name", "tool", "name"])).unwrap_or("");
            // No tool name (Cursor's afterFileEdit, a bare shim call) counts as an edit.
            if tool.is_empty() || !is_read_only(tool) {
                state.touch_trigger();
                watch::ensure_worker(&repo, &state)?;
            }
            let reports = state.drain_reports();
            let context = (!reports.is_empty()).then(|| watch::format_for_agent(&reports, None));
            Output { context, block: None, note: None }
        }
        Event::Prompt => {
            if state.trigger_times().is_some() {
                watch::ensure_worker(&repo, &state)?;
            }
            // Cursor's prompt hook cannot carry context; leave the inbox alone.
            let reports = if harness == Harness::Cursor { Vec::new() } else { state.drain_reports() };
            let context = (!reports.is_empty()).then(|| watch::format_for_agent(&reports, None));
            Output { context, block: None, note: None }
        }
        Event::Stop => {
            let o = watch::on_stop(&repo, &state, &ws);
            if o.block.is_some() {
                state.log("stop: sent the agent back to work");
            }
            Output { context: None, block: o.block, note: o.note }
        }
    };
    emit(harness, event, out);
    Ok(0)
}

fn emit(harness: Harness, event: Event, out: Output) {
    match harness {
        Harness::Claude | Harness::Codex => {
            let mut v = serde_json::Map::new();
            if let Some(reason) = out.block {
                v.insert("decision".into(), json!("block"));
                v.insert("reason".into(), json!(reason));
            }
            if let Some(ctx) = out.context {
                let name = match event {
                    Event::SessionStart => "SessionStart",
                    Event::Tool => "PostToolUse",
                    Event::Prompt => "UserPromptSubmit",
                    Event::Stop => "Stop",
                };
                v.insert("hookSpecificOutput".into(), json!({"hookEventName": name, "additionalContext": ctx}));
            }
            if let Some(note) = out.note {
                v.insert("systemMessage".into(), json!(note));
            }
            if !v.is_empty() {
                println!("{}", Value::Object(v));
            }
        }
        Harness::Cursor => {
            // postToolUse and sessionStart take `additional_context`; stop
            // takes a `followup_message` that starts another turn.
            // beforeSubmitPrompt cannot add context, so Prompt prints nothing
            // and the findings wait for the next postToolUse.
            let mut v = serde_json::Map::new();
            match event {
                Event::Stop => {
                    if let Some(m) = out.block {
                        v.insert("followup_message".into(), json!(m));
                    }
                }
                Event::Tool | Event::SessionStart => {
                    if let Some(ctx) = out.context {
                        v.insert("additional_context".into(), json!(ctx));
                    }
                }
                Event::Prompt => {}
            }
            if !v.is_empty() {
                println!("{}", Value::Object(v));
            }
            if let Some(note) = out.note {
                eprintln!("{note}");
            }
        }
        Harness::Pi | Harness::Opencode | Harness::Openclaw | Harness::Generic => {
            println!(
                "{}",
                json!({
                    "event": match event {
                        Event::SessionStart => "session-start",
                        Event::Tool => "tool",
                        Event::Prompt => "prompt",
                        Event::Stop => "stop",
                    },
                    "context": out.context,
                    "block": out.block.is_some(),
                    "reason": out.block,
                    "note": out.note,
                })
            );
        }
    }
}

// ---------------------------------------------------------------------------
// install / uninstall

const MARKER: &str = "nitpick hook ";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine the home directory")
}

/// Where the hook file lives for a harness, project-local or user-wide.
pub fn settings_path(harness: Harness, global: bool, repo_root: &Path) -> Result<PathBuf> {
    Ok(match (harness, global) {
        (Harness::Claude, false) => repo_root.join(".claude").join("settings.json"),
        (Harness::Claude, true) => home()?.join(".claude").join("settings.json"),
        (Harness::Codex, false) => repo_root.join(".codex").join("hooks.json"),
        (Harness::Codex, true) => {
            std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or(home()?.join(".codex")).join("hooks.json")
        }
        (Harness::Cursor, false) => repo_root.join(".cursor").join("hooks.json"),
        (Harness::Cursor, true) => home()?.join(".cursor").join("hooks.json"),
        (Harness::Pi, false) => repo_root.join(".pi").join("extensions").join("nitpick.ts"),
        (Harness::Pi, true) => std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".pi").join("agent"))
            .join("extensions")
            .join("nitpick.ts"),
        (Harness::Opencode, false) => repo_root.join(".opencode").join("plugins").join("nitpick.ts"),
        (Harness::Opencode, true) => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".config"))
            .join("opencode")
            .join("plugins")
            .join("nitpick.ts"),
        (Harness::Openclaw, false) => repo_root.join(".openclaw").join("extensions").join("nitpick"),
        (Harness::Openclaw, true) => home()?.join(".openclaw").join("extensions").join("nitpick"),
        (Harness::Generic, _) => {
            bail!(
                "there is nothing to install for the generic harness; call `nitpick hook generic <event>` from your own hook"
            )
        }
    })
}

/// The TypeScript shims, embedded so `install` needs nothing but the binary.
pub const PI_SHIM: &str = include_str!("shims/pi.ts");
pub const OPENCODE_SHIM: &str = include_str!("shims/opencode.ts");
pub const OPENCLAW_SHIM: &[(&str, &str)] = &[
    ("index.ts", include_str!("shims/openclaw/index.ts")),
    ("openclaw.plugin.json", include_str!("shims/openclaw/openclaw.plugin.json")),
    ("package.json", include_str!("shims/openclaw/package.json")),
];

/// Writes (or refreshes) a shim. An existing file is replaced so an upgrade
/// of nitpick upgrades the shim; the caller is told when that happened.
fn write_shim(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() && std::fs::read_to_string(path).ok().as_deref() != Some(content) {
        eprintln!("nitpick: replacing existing {}", path.display());
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {} (fix or move it first)", path.display()))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(v)?))
        .with_context(|| format!("writing {}", path.display()))
}

/// Claude Code and Codex share this shape.
pub fn claude_style_hooks(harness: Harness) -> Value {
    let h = harness.name();
    let entry = |event: &str, timeout: u64, description: &str| {
        let mut hook = json!({ "type": "command", "command": format!("{MARKER}{h} {event}"), "timeout": timeout });
        if harness == Harness::Codex {
            hook["statusMessage"] = json!(description);
        }
        json!([{ "hooks": [hook] }])
    };
    json!({
        "SessionStart": entry("session-start", 30, "nitpick: establish the review baseline"),
        "PostToolUse": entry("tool", 30, "nitpick: schedule background review and deliver findings"),
        "UserPromptSubmit": entry("prompt", 30, "nitpick: deliver pending review findings"),
        "Stop": entry("stop", 600, "nitpick: schedule remaining review; apply the configured stop policy"),
    })
}

/// Cursor's own format. `postToolUse` rather than `afterFileEdit` because
/// only the former can return `additional_context`.
pub fn cursor_hooks() -> Value {
    json!({
        "sessionStart": [{ "command": format!("{MARKER}cursor session-start"), "timeout": 30 }],
        "postToolUse": [{ "command": format!("{MARKER}cursor tool"), "timeout": 30 }],
        "stop": [{ "command": format!("{MARKER}cursor stop"), "timeout": 600, "loop_limit": 3 }]
    })
}

fn has_marker(entry: &Value) -> bool {
    match entry {
        Value::Object(o) => o.values().any(has_marker),
        Value::Array(a) => a.iter().any(has_marker),
        Value::String(s) => s.contains(MARKER),
        _ => false,
    }
}

/// Add nitpick's hooks to the harness; existing nitpick entries are left as
/// is. For the TypeScript harnesses this writes the shim file(s).
pub fn install(harness: Harness, global: bool, repo_root: &Path) -> Result<PathBuf> {
    let path = settings_path(harness, global, repo_root)?;
    match harness {
        Harness::Pi => {
            write_shim(&path, PI_SHIM)?;
            return Ok(path);
        }
        Harness::Opencode => {
            write_shim(&path, OPENCODE_SHIM)?;
            return Ok(path);
        }
        Harness::Openclaw => {
            for (name, content) in OPENCLAW_SHIM {
                write_shim(&path.join(name), content)?;
            }
            return Ok(path);
        }
        _ => {}
    }
    let mut doc = read_json(&path)?;
    if !doc.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    let wanted = match harness {
        Harness::Cursor => cursor_hooks(),
        _ => claude_style_hooks(harness),
    };
    if harness == Harness::Cursor && doc.get("version").is_none() {
        doc["version"] = json!(1);
    }
    let hooks = doc.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("`hooks` in {} is not an object", path.display());
    }
    for (event, entries) in wanted.as_object().unwrap() {
        let list = hooks.as_object_mut().unwrap().entry(event.clone()).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else { bail!("`hooks.{event}` in {} is not an array", path.display()) };
        if list.iter().any(has_marker) {
            continue;
        }
        list.extend(entries.as_array().unwrap().iter().cloned());
    }
    write_json(&path, &doc)?;
    Ok(path)
}

/// Remove every hook entry that calls `nitpick hook`.
pub fn uninstall(harness: Harness, global: bool, repo_root: &Path) -> Result<Option<PathBuf>> {
    let path = settings_path(harness, global, repo_root)?;
    if !path.exists() {
        return Ok(None);
    }
    match harness {
        Harness::Pi | Harness::Opencode => {
            std::fs::remove_file(&path)?;
            return Ok(Some(path));
        }
        Harness::Openclaw => {
            // Only what install wrote; anything else in the directory stays.
            for (name, _) in OPENCLAW_SHIM {
                let _ = std::fs::remove_file(path.join(name));
            }
            let _ = std::fs::remove_dir(&path);
            return Ok(Some(path));
        }
        _ => {}
    }
    let mut doc = read_json(&path)?;
    let mut changed = false;
    if let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) {
        let mut empty: Vec<String> = Vec::new();
        for (event, list) in hooks.iter_mut() {
            if let Some(arr) = list.as_array_mut() {
                let before = arr.len();
                arr.retain(|e| !has_marker(e));
                changed |= arr.len() != before;
                if arr.is_empty() {
                    empty.push(event.clone());
                }
            }
        }
        for e in empty {
            hooks.remove(&e);
        }
    }
    if changed {
        write_json(&path, &doc)?;
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_preserves_other_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join(".claude").join("settings.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            r#"{"permissions":{"allow":["Bash(ls)"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .unwrap();
        install(Harness::Claude, false, root).unwrap();
        install(Harness::Claude, false, root).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["permissions"]["allow"][0], "Bash(ls)");
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "one existing entry plus one nitpick entry");
        assert_eq!(doc["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "nitpick hook claude tool");

        let removed = uninstall(Harness::Claude, false, root).unwrap();
        assert!(removed.is_some());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(doc["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn codex_install_labels_hooks_and_preserves_existing_approvals() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(Harness::Codex, false, dir.path()).unwrap();
        let initial = std::fs::read_to_string(&path).unwrap();
        let mut doc: Value = serde_json::from_str(&initial).unwrap();
        assert_eq!(doc["hooks"].as_object().unwrap().len(), 4);
        for groups in doc["hooks"].as_object().unwrap().values() {
            assert!(groups[0]["hooks"][0]["statusMessage"].as_str().unwrap().starts_with("nitpick: "));
        }
        // Reinstall must not rewrite an existing hook definition, which would
        // invalidate its saved trust. This includes older unlabeled hooks.
        doc["hooks"]["Stop"][0]["hooks"][0].as_object_mut().unwrap().remove("statusMessage");
        write_json(&path, &doc).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        install(Harness::Codex, false, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn cursor_install_sets_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = install(Harness::Cursor, false, dir.path()).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["hooks"]["postToolUse"][0]["command"], "nitpick hook cursor tool");
    }

    #[test]
    fn shim_install_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = install(Harness::Pi, false, dir.path()).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("nitpick"));
        let p = install(Harness::Openclaw, false, dir.path()).unwrap();
        assert!(p.join("index.ts").is_file() && p.join("package.json").is_file());
        std::fs::write(p.join("notes.txt"), "mine").unwrap();
        assert!(uninstall(Harness::Openclaw, false, dir.path()).unwrap().is_some());
        assert!(!p.join("index.ts").exists());
        assert!(p.join("notes.txt").is_file(), "user files survive uninstall");
    }

    #[test]
    fn read_only_tools() {
        assert!(is_read_only("Read"));
        assert!(is_read_only("grep"));
        assert!(!is_read_only("Edit"));
        assert!(!is_read_only("apply_patch"));
        assert!(!is_read_only("Bash"));
        assert!(!is_read_only(""));
    }
}
