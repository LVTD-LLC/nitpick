//! Background review while an agent works.
//!
//! The agent harness calls `nitpick hook <harness> <event>` from its hooks
//! (see `hooks.rs`). Those hooks are thin: an edit touches a trigger file and
//! makes sure a worker process is running; every later hook drains the inbox
//! and hands any findings back to the agent. The worker waits for a quiet
//! period, diffs each changed file against the copy it reviewed last time
//! (the "shadow"), runs that small diff through the normal context engine
//! and model call, and writes findings to the inbox. All state lives under
//! `<git-dir>/nitpick/`, so nothing needs a socket or a long-lived daemon.

use crate::config::FileConfig;
use crate::context;
use crate::git::{DiffMode, Repo};
use crate::review::{self, Finding, Severity};
use crate::run::{self, Overrides, Progress, Settings};
use crate::search;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Appended to the reviewer instructions for every incremental review.
pub const INSTRUCTIONS: &str = "This is an incremental review of an AI coding agent's work in progress, taken automatically while the agent is still working. The diff is what changed since the previous review a few minutes ago, not a finished pull request. Judge only what is written: do not report incomplete work, TODOs, missing tests, or references to code that may be written next. Report only problems you are confident about from the code in front of you: definite bugs, call sites that now break, security holes, data loss. When in doubt, leave it out.";

/// A worker lock whose mtime is older than this belongs to a dead process.
const LOCK_STALE: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(5);
const MAX_FILE_BYTES: u64 = 400_000;
/// After this many failed reviews in a row the pending changes are written
/// off, so a dead provider cannot queue the same diff forever.
const MAX_FAILURES: u32 = 3;

// ---------------------------------------------------------------------------
// settings

pub struct WatchSettings {
    pub enabled: bool,
    pub deliver: Severity,
    pub fail_on: Severity,
    pub debounce: Duration,
    pub max_wait: Duration,
    pub stop_wait: Duration,
    pub max_stop_blocks: u32,
    pub review: Settings,
}

fn parse_sev(s: Option<&str>, what: &str) -> Result<Option<Severity>> {
    match s {
        Some(s) => s.parse().map(Some).map_err(|_| anyhow::anyhow!("invalid {what} `{s}` in [watch]")),
        None => Ok(None),
    }
}

pub fn settings(file: &FileConfig) -> Result<WatchSettings> {
    let w = &file.watch;
    let env_on = std::env::var("NITPICK_WATCH")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
        .unwrap_or(true);
    let mut o = Overrides::default();
    if let Some(m) = &w.model {
        o.models = m.clone().into_vec();
    }
    o.timeout_secs = Some(w.timeout_secs.unwrap_or(180));
    o.budget = Some(w.budget_tokens.unwrap_or(40_000));
    o.fail_on = parse_sev(w.fail_on.as_deref(), "fail_on")?;
    o.focus.push(INSTRUCTIONS.to_string());
    if let Some(i) = &w.instructions
        && !i.trim().is_empty()
    {
        o.focus.push(i.trim().to_string());
    }
    let review = run::resolve(file, &o)?;
    Ok(WatchSettings {
        enabled: w.enabled.unwrap_or(true) && env_on,
        deliver: parse_sev(w.deliver.as_deref(), "deliver")?.unwrap_or(Severity::Medium),
        fail_on: review.fail_on,
        debounce: Duration::from_secs(w.debounce_secs.unwrap_or(20)),
        max_wait: Duration::from_secs(w.max_wait_secs.unwrap_or(120)),
        stop_wait: Duration::from_secs(w.stop_wait_secs.unwrap_or(120)),
        max_stop_blocks: w.max_stop_blocks.unwrap_or(0),
        review,
    })
}

// ---------------------------------------------------------------------------
// state on disk

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub at: u64,
    pub files: Vec<String>,
    pub models: Vec<String>,
    pub summary: String,
    pub findings: Vec<Finding>,
    pub errors: Vec<String>,
    pub elapsed_ms: u128,
}

pub struct State {
    pub dir: PathBuf,
}

/// The baseline recorded for a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Shadow {
    /// Never reviewed: HEAD is the baseline.
    Missing,
    /// Reviewed as deleted.
    Deleted,
    Content(Vec<u8>),
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SSZ` without pulling in a date crate.
pub fn timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Filesystem-safe name for a repo-relative path. Lossless for ordinary
/// paths; very long ones are hashed and keep their tail for readability.
fn key(rel: &str) -> String {
    let mut s = String::with_capacity(rel.len());
    for b in rel.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    if s.len() > 180 {
        // FNV-1a: stable across processes and Rust versions, which a name on
        // disk needs and `DefaultHasher` does not promise.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in rel.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let tail = &s[s.len() - 120..];
        return format!("{h:016x}-{tail}");
    }
    s
}

impl State {
    fn directory(repo: &Repo) -> Result<PathBuf> {
        if !repo.standalone {
            return Ok(repo.git_dir()?.join("nitpick"));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("cannot determine home directory for watch state")?;
        Ok(PathBuf::from(home).join(".local/state/nitpick/workspaces").join(key(&repo.root.to_string_lossy())))
    }

    pub fn open(repo: &Repo) -> Result<State> {
        let dir = Self::directory(repo)?;
        for sub in ["shadow", "inbox", "delivered", "tmp"] {
            std::fs::create_dir_all(dir.join(sub)).with_context(|| format!("creating {}", dir.join(sub).display()))?;
        }
        Ok(State { dir })
    }

    /// The state directory if it exists, without creating it.
    pub fn existing(repo: &Repo) -> Option<State> {
        let dir = Self::directory(repo).ok()?;
        if dir.is_dir() { Some(State { dir }) } else { None }
    }

    // -- trigger: "an edit happened" ------------------------------------------

    fn trigger_path(&self) -> PathBuf {
        self.dir.join("trigger")
    }

    /// Record an edit. The file's content is the time of the first edit in
    /// this burst, its mtime the most recent one.
    pub fn touch_trigger(&self) {
        let p = self.trigger_path();
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
            let _ = f.set_modified(SystemTime::now());
        } else if let Err(e) = std::fs::write(&p, now_secs().to_string()) {
            eprintln!("nitpick: cannot write {}: {e}", p.display());
        }
    }

    /// (first edit, last edit) of the current burst, if any.
    pub fn trigger_times(&self) -> Option<(SystemTime, SystemTime)> {
        let p = self.trigger_path();
        let meta = std::fs::metadata(&p).ok()?;
        let last = meta.modified().ok()?;
        let first = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|s| UNIX_EPOCH + Duration::from_secs(s))
            .unwrap_or(last);
        Some((first, last))
    }

    pub fn clear_trigger(&self) {
        let _ = std::fs::remove_file(self.trigger_path());
    }

    // -- shadows: what each file looked like when it was last reviewed --------
    //
    // `<key>` holds the content, `<key>.p` the path, and `<key>.d` marks a
    // file that was reviewed as deleted (so its absence is the baseline and
    // not "never reviewed", which would fall back to HEAD).

    pub fn shadow(&self, rel: &str) -> Shadow {
        let k = key(rel);
        let dir = self.dir.join("shadow");
        match std::fs::read(dir.join(&k)) {
            Ok(b) => Shadow::Content(b),
            Err(_) if dir.join(format!("{k}.d")).exists() => Shadow::Deleted,
            Err(_) => Shadow::Missing,
        }
    }

    /// Record the reviewed state of a file: its content, or `None` for
    /// "reviewed as deleted".
    pub fn set_shadow(&self, rel: &str, content: Option<&[u8]>) {
        let k = key(rel);
        let dir = self.dir.join("shadow");
        let _ = std::fs::write(dir.join(format!("{k}.p")), rel);
        match content {
            Some(bytes) => {
                let _ = std::fs::write(dir.join(&k), bytes);
                let _ = std::fs::remove_file(dir.join(format!("{k}.d")));
            }
            None => {
                let _ = std::fs::remove_file(dir.join(&k));
                let _ = std::fs::write(dir.join(format!("{k}.d")), "");
            }
        }
    }

    /// Forget a file entirely: it is back at its committed state.
    pub fn remove_shadow(&self, rel: &str) {
        let k = key(rel);
        let dir = self.dir.join("shadow");
        for name in [k.clone(), format!("{k}.p"), format!("{k}.d")] {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }

    pub fn shadow_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.dir.join("shadow")) else { return out };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".p")
                && let Ok(rel) = std::fs::read_to_string(e.path())
                && !rel.is_empty()
            {
                out.push(rel);
            }
        }
        out.sort();
        out
    }

    // -- worker lock ------------------------------------------------------------

    fn lock_path(&self) -> PathBuf {
        self.dir.join("worker.lock")
    }

    pub fn worker_alive(&self) -> bool {
        match std::fs::metadata(self.lock_path()).and_then(|m| m.modified()) {
            Ok(t) => SystemTime::now().duration_since(t).map(|d| d < LOCK_STALE).unwrap_or(true),
            Err(_) => false,
        }
    }

    /// Take the worker lock. `None` if a live worker holds it.
    pub fn try_lock(&self) -> Option<Lock> {
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(self.lock_path()) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Some(Lock::new(self.lock_path()));
                }
                Err(_) if !self.worker_alive() => {
                    let _ = std::fs::remove_file(self.lock_path());
                }
                Err(_) => return None,
            }
        }
        None
    }

    // -- inbox ------------------------------------------------------------------

    pub fn push_report(&self, r: &Report) {
        let name = format!("{:013}.json", now_millis());
        if let Ok(json) = serde_json::to_vec_pretty(r) {
            let _ = std::fs::write(self.dir.join("inbox").join(name), json);
        }
    }

    fn read_reports(dir: &Path) -> Vec<(PathBuf, Report)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else { return out };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read(&p)
                && let Ok(r) = serde_json::from_slice::<Report>(&text)
            {
                out.push((p, r));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Undelivered reports, oldest first, left in place.
    pub fn peek_reports(&self) -> Vec<Report> {
        Self::read_reports(&self.dir.join("inbox")).into_iter().map(|(_, r)| r).collect()
    }

    /// Undelivered reports, oldest first, moved to `delivered/`.
    pub fn drain_reports(&self) -> Vec<Report> {
        let mut out = Vec::new();
        for (p, r) in Self::read_reports(&self.dir.join("inbox")) {
            let dest = self.dir.join("delivered").join(p.file_name().unwrap_or_default());
            if std::fs::rename(&p, &dest).is_err() {
                // Same filesystem normally; fall back to copy, and remove
                // regardless so the report is not delivered twice.
                let _ = std::fs::copy(&p, &dest);
                let _ = std::fs::remove_file(&p);
            }
            out.push(r);
        }
        out
    }

    /// Every report ever produced, newest first.
    pub fn all_reports(&self) -> Vec<Report> {
        let mut all = Self::read_reports(&self.dir.join("delivered"));
        all.extend(Self::read_reports(&self.dir.join("inbox")));
        all.sort_by(|a, b| b.0.cmp(&a.0));
        all.into_iter().map(|(_, r)| r).collect()
    }

    // -- the commit the baselines were taken against ---------------------------------

    pub fn head(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("head")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn set_head(&self, sha: Option<&str>) {
        match sha {
            Some(sha) => {
                let _ = std::fs::write(self.dir.join("head"), sha);
            }
            None => {
                let _ = std::fs::remove_file(self.dir.join("head"));
            }
        }
    }

    // -- counters and log ---------------------------------------------------------

    fn counter(&self, name: &str) -> u32 {
        std::fs::read_to_string(self.dir.join(name)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    pub fn set_counter(&self, name: &str, v: u32) {
        if v == 0 {
            let _ = std::fs::remove_file(self.dir.join(name));
        } else {
            let _ = std::fs::write(self.dir.join(name), v.to_string());
        }
    }

    pub fn log(&self, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(self.dir.join("log")) {
            let _ = writeln!(f, "{} {msg}", timestamp(now_secs()));
        }
    }

    pub fn tail_log(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join("log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines.iter().rev().take(n).rev().map(|s| s.to_string()).collect()
    }
}

/// Holds `worker.lock` and keeps its mtime fresh until dropped.
pub struct Lock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Lock {
    fn new(path: PathBuf) -> Lock {
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (path.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                std::thread::park_timeout(HEARTBEAT);
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
                    let _ = f.set_modified(SystemTime::now());
                }
            }
        });
        Lock { path, stop, thread: Some(thread) }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// what changed since the last review

pub struct Change {
    pub rel: String,
    pub base: Option<Vec<u8>>,
    pub current: Option<Vec<u8>>,
}

/// Every file whose working-tree content differs from the copy reviewed last
/// time (or from HEAD, when it has never been reviewed). Covers edits made
/// through any tool, not just the ones a hook saw.
pub fn pending_changes(repo: &Repo, state: &State, ignore: &[String]) -> Result<Vec<Change>> {
    // A checkout, reset or rebase moves HEAD somewhere the baselines were not
    // taken from; the file contents changed, but not because the agent edited
    // them. Start over from the new tree instead of reviewing the switch.
    let head = if repo.standalone { None } else { repo.rev_parse("HEAD") };
    if let (Some(recorded), Some(cur)) = (state.head(), head.as_deref())
        && recorded != cur
        && repo.is_ancestor(&recorded, cur) == Some(false)
    {
        state.log(&format!(
            "HEAD moved from {} to {}: baseline reset",
            &recorded[..10.min(recorded.len())],
            &cur[..10.min(cur.len())]
        ));
        snapshot(repo, state)?;
        return Ok(Vec::new());
    }
    let mut paths: BTreeSet<String> = repo.watch_files()?.into_iter().collect();
    paths.extend(state.shadow_paths().into_iter().filter(|rel| !repo.standalone || !repo.root.join(rel).exists()));
    let mut out = Vec::new();
    for rel in paths {
        if search::is_junk(&rel) || context::glob_matches(ignore, &rel) {
            continue;
        }
        let abs = repo.root.join(&rel);
        let current = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => {
                if m.len() > MAX_FILE_BYTES {
                    continue;
                }
                match std::fs::read(&abs) {
                    Ok(b) if is_binary(&b) => continue,
                    Ok(b) => Some(b),
                    Err(_) => None,
                }
            }
            _ => None,
        };
        let base = match state.shadow(&rel) {
            Shadow::Content(b) => Some(b),
            Shadow::Deleted => None,
            Shadow::Missing if repo.standalone => None,
            Shadow::Missing => repo.show_head(&rel),
        };
        if base.as_deref().is_some_and(is_binary) {
            continue;
        }
        if base == current {
            continue;
        }
        out.push(Change { rel, base, current });
    }
    Ok(out)
}

/// Make the current working tree the baseline: everything dirty now is
/// treated as already reviewed. Called at session start so a user's own
/// uncommitted work is not blamed on the agent.
pub fn snapshot(repo: &Repo, state: &State) -> Result<usize> {
    let dirty: BTreeSet<String> = repo.watch_files()?.into_iter().collect();
    let mut paths = dirty.clone();
    paths.extend(state.shadow_paths());
    let mut n = 0;
    for rel in paths {
        if search::is_junk(&rel) {
            continue;
        }
        let abs = repo.root.join(&rel);
        if repo.standalone && !dirty.contains(&rel) && abs.exists() {
            state.remove_shadow(&rel);
            continue;
        }
        let current = std::fs::read(&abs).ok().filter(|b| !is_binary(b) && b.len() as u64 <= MAX_FILE_BYTES);
        if !dirty.contains(&rel) && current == repo.show_head(&rel) {
            // Back to its committed state (or committed and gone): no baseline needed.
            state.remove_shadow(&rel);
            continue;
        }
        state.set_shadow(&rel, current.as_deref());
        n += 1;
    }
    state.set_head(repo.rev_parse("HEAD").as_deref());
    state.clear_trigger();
    Ok(n)
}

struct TempTree {
    dir: PathBuf,
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write the before/after copies into a scratch tree and let git diff the
/// two directories in one go. Returns the unified diff and the root holding
/// the "after" copies, which the context engine reads changed files from so
/// line numbers match the bytes that were reviewed.
fn snapshot_diff(state: &State, changes: &[Change]) -> Result<(String, PathBuf, TempTree)> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!("{}-{}-{}", now_millis(), std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let tmp = TempTree { dir: state.dir.join("tmp").join(name) };
    let (a, b) = (tmp.dir.join("a"), tmp.dir.join("b"));
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    for c in changes {
        for (root, content) in [(&a, &c.base), (&b, &c.current)] {
            if let Some(bytes) = content {
                let p = root.join(&c.rel);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, bytes)?;
            }
        }
    }
    // Files present on one side only come out as additions or deletions
    // against /dev/null. Exit status is 1 when anything differs; only stdout
    // matters.
    let out = Command::new("git")
        .arg("-C")
        .arg(&tmp.dir)
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U3",
            "--src-prefix=",
            "--dst-prefix=",
            "--no-index",
            "--",
            "a",
            "b",
        ])
        .output()
        .context("running git diff --no-index")?;
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), b, tmp))
}

/// Review one batch of changes. Updates the shadows on success (and after
/// repeated failure, so a dead provider does not block forever). Findings at
/// or above `deliver` go to the inbox when `deliver_to_agent` is set.
pub fn review_changes(
    repo: &Repo,
    state: &State,
    ws: &WatchSettings,
    changes: &[Change],
    progress: Progress,
    deliver_to_agent: bool,
) -> Result<Report> {
    let started = std::time::Instant::now();
    let (diff, b_root, _tmp) = snapshot_diff(state, changes)?;
    let files: Vec<String> = changes.iter().map(|c| c.rel.clone()).collect();
    let mode = DiffMode::Snapshot {
        label: format!("changes since the last background review ({} file(s))", changes.len()),
        root: b_root,
    };
    let mut options = ws.review.ctx.clone();
    // Outside Git send only the edited code, not unrelated files in the folder.
    options.with_context &= !repo.standalone;
    let pack = context::build_from_diff(repo, &mode, &diff, &options)?;
    let update_shadows = || {
        for c in changes {
            state.set_shadow(&c.rel, c.current.as_deref());
        }
        state.set_head(repo.rev_parse("HEAD").as_deref());
    };
    if pack.is_empty() {
        update_shadows();
        return Ok(Report {
            at: now_secs(),
            files,
            models: ws.review.models.clone(),
            summary: "nothing reviewable changed".into(),
            findings: Vec::new(),
            errors: Vec::new(),
            elapsed_ms: started.elapsed().as_millis(),
        });
    }
    let results = match run::review_pack(&pack, &ws.review, progress) {
        Ok(r) => {
            state.set_counter("failures", 0);
            r
        }
        Err(e) => {
            let n = state.counter("failures") + 1;
            state.log(&format!("review failed ({n}/{MAX_FAILURES}): {e:#}"));
            if n >= MAX_FAILURES {
                update_shadows();
                state.set_counter("failures", 0);
                state.log("giving up on these changes; baseline advanced");
            } else {
                state.set_counter("failures", n);
            }
            return Err(e);
        }
    };
    let merged = review::merge(&results);
    update_shadows();
    let findings: Vec<Finding> = merged.findings.into_iter().filter(|f| f.severity >= ws.deliver).collect();
    let summary = results
        .iter()
        .filter_map(|r| r.review.as_ref())
        .map(|r| r.summary.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let report = Report {
        at: now_secs(),
        files,
        models: results.iter().filter(|r| r.review.is_some()).map(|r| r.model.clone()).collect(),
        summary,
        findings,
        errors: results.iter().filter_map(|r| r.error.as_ref().map(|e| format!("{}: {e}", r.model))).collect(),
        elapsed_ms: started.elapsed().as_millis(),
    };
    let top = report.findings.iter().map(|f| f.severity).max();
    state.log(&format!(
        "reviewed {} file(s) with {} in {:.1}s: {} finding(s){} [{}]",
        report.files.len(),
        report.models.join(","),
        report.elapsed_ms as f64 / 1000.0,
        report.findings.len(),
        top.map(|s| format!(", top {}", s.as_str())).unwrap_or_default(),
        report.files.join(", ")
    ));
    if deliver_to_agent && !report.findings.is_empty() {
        state.push_report(&report);
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// the worker process

/// Start a detached worker unless one is already running. Its stdio is
/// closed so the hook that spawned it can return immediately.
pub fn ensure_worker(repo: &Repo, state: &State) -> Result<()> {
    if state.worker_alive() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating the nitpick binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(["watch", "worker"])
        .current_dir(&repo.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("spawning the watch worker")?;
    Ok(())
}

/// Body of `nitpick watch worker`: wait for the edits to go quiet, review
/// what changed, repeat while edits keep arriving, then exit.
pub fn worker(repo: &Repo) -> Result<()> {
    let file = crate::config::load(&repo.root)?;
    let ws = settings(&file)?;
    if !ws.enabled {
        return Ok(());
    }
    let state = State::open(repo)?;
    let Some(_lock) = state.try_lock() else { return Ok(()) };
    state.log("worker started");
    loop {
        while let Some((first, last)) = state.trigger_times() {
            let now = SystemTime::now();
            let since_last = now.duration_since(last).unwrap_or_default();
            let since_first = now.duration_since(first).unwrap_or_default();
            if since_last >= ws.debounce || since_first >= ws.max_wait {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        state.clear_trigger();
        let changes = pending_changes(repo, &state, &ws.review.ctx.ignore)?;
        if changes.is_empty() {
            if state.trigger_times().is_none() {
                break;
            }
            continue;
        }
        if let Err(e) = review_changes(repo, &state, &ws, &changes, Progress::Quiet, true) {
            state.log(&format!("worker: {e:#}"));
        }
        if state.trigger_times().is_none() {
            break;
        }
    }
    state.log("worker idle, exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// the stop hook

pub struct StopOutcome {
    /// Send the agent back to work with this text.
    pub block: Option<String>,
    /// Tell the user this, without involving the agent.
    pub note: Option<String>,
}

/// Advisory stop schedules background work and returns immediately. When a
/// completion gate is configured, wait and review leftovers, then block at
/// most `max_stop_blocks` times in a row.
pub fn on_stop(repo: &Repo, state: &State, ws: &WatchSettings) -> StopOutcome {
    if ws.max_stop_blocks == 0 {
        // Advisory mode: keep pending findings for the next tool/prompt hook.
        // Never wait for a provider or start a synchronous review here.
        state.touch_trigger();
        if let Err(e) = ensure_worker(repo, state) {
            state.log(&format!("stop: could not schedule review: {e:#}"));
        }
        return StopOutcome { block: None, note: None };
    }
    let deadline = std::time::Instant::now() + ws.stop_wait;
    while state.worker_alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    // Hold the worker lock while reviewing inline so a worker that starts
    // in the meantime (or one that outlived the wait) cannot review the same
    // files at the same time.
    if let Some(_lock) = state.try_lock() {
        state.clear_trigger();
        if let Ok(changes) = pending_changes(repo, state, &ws.review.ctx.ignore)
            && !changes.is_empty()
        {
            let _ = review_changes(repo, state, ws, &changes, Progress::Quiet, true);
        }
    }
    let waiting = state.peek_reports();
    let failing = waiting.iter().flat_map(|r| r.findings.iter()).filter(|f| f.severity >= ws.fail_on).count();
    if failing == 0 {
        state.set_counter("stop_blocks", 0);
        let n: usize = waiting.iter().map(|r| r.findings.len()).sum();
        let note = (n > 0).then(|| {
            format!(
                "nitpick: {n} finding(s) below `{}` are waiting; the agent sees them on the next message. `nitpick watch log` shows them now.",
                ws.fail_on.as_str()
            )
        });
        return StopOutcome { block: None, note };
    }
    let blocks = state.counter("stop_blocks") + 1;
    if blocks > ws.max_stop_blocks {
        state.set_counter("stop_blocks", 0);
        let _ = state.drain_reports();
        return StopOutcome {
            block: None,
            note: Some(format!(
                "nitpick: {failing} finding(s) at or above `{}` are still open after {} attempt(s); letting the agent stop. `nitpick watch log` has the details.",
                ws.fail_on.as_str(),
                ws.max_stop_blocks
            )),
        };
    }
    state.set_counter("stop_blocks", blocks);
    let reports = state.drain_reports();
    StopOutcome { block: Some(format_for_agent(&reports, Some(ws.fail_on))), note: None }
}

// ---------------------------------------------------------------------------
// rendering

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Models sometimes leave `title` empty; promote the body's first sentence.
fn title_and_body(f: &Finding) -> (&str, &str) {
    if !f.title.trim().is_empty() {
        return (f.title.as_str(), f.body.as_str());
    }
    let body = f.body.trim();
    let end = body.find(['.', '\n']).map(|i| i + 1).unwrap_or(body.len());
    let (head, tail) = body.split_at(end.min(body.len()));
    (head.trim_end_matches('.'), tail)
}

/// The note handed to the agent. `at_stop` carries the threshold that
/// stopped it; `None` is the mid-task wording.
pub fn format_for_agent(reports: &[Report], at_stop: Option<Severity>) -> String {
    let mut all: Vec<(&Report, &Finding)> =
        reports.iter().flat_map(|r| r.findings.iter().map(move |f| (r, f))).collect();
    all.sort_by(|a, b| b.1.severity.cmp(&a.1.severity).then(a.1.file.cmp(&b.1.file)));
    let n = all.len();
    let mut s = String::new();
    match at_stop {
        Some(th) => {
            let above = all.iter().filter(|(_, f)| f.severity >= th).count();
            s.push_str(&format!(
                "[nitpick] A background review of the edits made in this session found {above} issue(s) at or above `{}`. Fix each one, or state in one line why it is wrong, then finish.\n",
                th.as_str()
            ));
        }
        None => s.push_str(&format!(
            "[nitpick] While you were working, a background review of your recent edits found {n} issue(s). Finish the step you are on, then address them.\n"
        )),
    }
    for (_, f) in all.iter().take(10) {
        let loc = match (f.line, f.end_line) {
            (Some(l), Some(e)) if e > l => format!("{}:{}-{}", f.file, l, e),
            (Some(l), _) => format!("{}:{}", f.file, l),
            _ => f.file.clone(),
        };
        let (title, body) = title_and_body(f);
        s.push_str(&format!(
            "- {} {loc} {} [{}]\n",
            f.severity.as_str().to_ascii_uppercase(),
            clip(title, 100),
            f.category
        ));
        if !body.trim().is_empty() {
            s.push_str(&format!("  {}\n", clip(body, 500).replace('\n', " ")));
        }
        if let Some(fix) = f.suggestion.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
            s.push_str(&format!("  Fix: {}\n", clip(fix, 300).replace('\n', " ")));
        }
    }
    if n > 10 {
        s.push_str(&format!("- …and {} more; run `nitpick watch log` for all of them.\n", n - 10));
    }
    let mut files: Vec<&str> = reports.iter().flat_map(|r| r.files.iter().map(String::as_str)).collect();
    files.sort();
    files.dedup();
    let mut models: Vec<&str> = reports.iter().flat_map(|r| r.models.iter().map(String::as_str)).collect();
    models.sort();
    models.dedup();
    s.push_str(&format!(
        "(reviewed {} with {}; a second opinion from a different model, so verify before changing code)\n",
        clip(&files.join(", "), 200),
        models.join(", ")
    ));
    s
}

/// Human-readable dump of reports for `nitpick watch log`.
pub fn render_reports(reports: &[Report]) -> String {
    let mut s = String::new();
    for r in reports {
        s.push_str(&format!(
            "## {}  {} file(s), {} finding(s), {} in {:.1}s\n",
            timestamp(r.at),
            r.files.len(),
            r.findings.len(),
            r.models.join(", "),
            r.elapsed_ms as f64 / 1000.0
        ));
        s.push_str(&format!("files: {}\n", r.files.join(", ")));
        if !r.summary.is_empty() {
            s.push_str(&format!("{}\n", r.summary));
        }
        for f in &r.findings {
            let loc = match f.line {
                Some(l) => format!("{}:{l}", f.file),
                None => f.file.clone(),
            };
            let (title, body) = title_and_body(f);
            s.push_str(&format!("- **{loc}** ({}) {} `[{}]`\n", f.severity.as_str(), title.trim(), f.category));
            for line in body.trim().lines() {
                s.push_str(&format!("  {}\n", line.trim_end()));
            }
            if let Some(fix) = &f.suggestion
                && !fix.trim().is_empty()
            {
                s.push_str(&format!("  Suggestion: {}\n", fix.trim()));
            }
        }
        for e in &r.errors {
            s.push_str(&format!("- error: {e}\n"));
        }
        s.push('\n');
    }
    s
}

/// `nitpick watch status`.
pub fn status(repo: &Repo) -> Result<String> {
    let file = crate::config::load(&repo.root)?;
    let ws = settings(&file)?;
    let mut s = String::new();
    s.push_str(&format!(
        "enabled: {}\n",
        if ws.enabled { "yes" } else { "no (watch.enabled = false or NITPICK_WATCH=0)" }
    ));
    s.push_str(&format!("model: {}\n", ws.review.models.join(", ")));
    s.push_str(if ws.max_stop_blocks == 0 { "stop: advisory (never waits or blocks)\n" } else { "stop: blocking\n" });
    s.push_str(&format!(
        "deliver >= {}, stop on >= {}, debounce {}s, max wait {}s\n",
        ws.deliver.as_str(),
        ws.fail_on.as_str(),
        ws.debounce.as_secs(),
        ws.max_wait.as_secs()
    ));
    for (p, global) in crate::hooks::claude_duplicates(&repo.root) {
        s.push_str(&format!(
            "warning: {} also registers nitpick's hooks, so Claude Code runs each one twice; the plugin already provides them. Remove that copy with `nitpick watch uninstall claude{}`.\n",
            p.display(),
            if global { " --global" } else { "" }
        ));
    }
    let Some(state) = State::existing(repo) else {
        s.push_str("state: none yet (no hook has fired in this checkout)\n");
        return Ok(s);
    };
    s.push_str(&format!("state: {}\n", state.dir.display()));
    s.push_str(&format!("worker: {}\n", if state.worker_alive() { "running" } else { "idle" }));
    match state.trigger_times() {
        Some((_, last)) => {
            let ago = SystemTime::now().duration_since(last).unwrap_or_default().as_secs();
            s.push_str(&format!("last edit: {ago}s ago (review pending)\n"));
        }
        None => s.push_str("last edit: none pending\n"),
    }
    let changes = pending_changes(repo, &state, &ws.review.ctx.ignore)?;
    if changes.is_empty() {
        s.push_str("unreviewed changes: none\n");
    } else {
        s.push_str(&format!(
            "unreviewed changes: {}\n",
            changes.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let waiting: usize = state.peek_reports().iter().map(|r| r.findings.len()).sum();
    s.push_str(&format!("findings waiting for the agent: {waiting}\n"));
    s.push_str(&format!("baselines (reviewed files): {}\n", state.shadow_paths().len()));
    let log = state.tail_log(8);
    if !log.is_empty() {
        s.push_str("recent:\n");
        for l in log {
            s.push_str(&format!("  {l}\n"));
        }
    }
    Ok(s)
}

/// `nitpick watch run`: review whatever is unreviewed right now, in the
/// foreground, and print the result. Nothing goes to the inbox: whoever
/// ran this is reading the output.
pub fn run_once(repo: &Repo, progress: Progress, json: bool) -> Result<i32> {
    let file = crate::config::load(&repo.root)?;
    let ws = settings(&file)?;
    let state = State::open(repo)?;
    let changes = pending_changes(repo, &state, &ws.review.ctx.ignore)?;
    if changes.is_empty() {
        if json {
            println!("{{\"verdict\":\"approve\",\"findings\":[],\"note\":\"nothing changed since the last review\"}}");
        } else {
            println!("Nothing changed since the last review.");
        }
        return Ok(0);
    }
    let report = review_changes(repo, &state, &ws, &changes, progress, false)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_reports(std::slice::from_ref(&report)));
    }
    let failing = report.findings.iter().any(|f| f.severity >= ws.fail_on);
    Ok(if failing { 1 } else { 0 })
}

pub fn reset(repo: &Repo) -> Result<()> {
    let Some(state) = State::existing(repo) else { return Ok(()) };
    if state.worker_alive() {
        bail!("a watch worker is running; wait for it to finish (see `nitpick watch status`)");
    }
    std::fs::remove_dir_all(&state.dir).with_context(|| format!("removing {}", state.dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_stop_returns_with_worker_busy_and_preserves_inbox() {
        let (_dir, repo) = git_repo();
        let state = State::open(&repo).unwrap();
        // A live lock must not cause stop to wait, nor may it drain findings.
        std::fs::write(state.lock_path(), "test").unwrap();
        state.push_report(&Report {
            at: 1,
            files: vec!["a.py".into()],
            models: vec![],
            summary: String::new(),
            findings: vec![],
            errors: vec![],
            elapsed_ms: 0,
        });
        let ws = settings(&FileConfig::default()).unwrap();
        assert_eq!(ws.max_stop_blocks, 0);
        let start = std::time::Instant::now();
        let out = on_stop(&repo, &state, &ws);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(out.block.is_none() && out.note.is_none());
        assert_eq!(state.peek_reports().len(), 1);
        assert!(state.trigger_times().is_some());
    }

    #[test]
    fn standalone_watch_baselines_edits_additions_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state = State { dir: state_dir.path().into() };
        for sub in ["shadow", "tmp"] {
            std::fs::create_dir_all(state.dir.join(sub)).unwrap();
        }
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "private notes").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.py\n").unwrap();
        std::fs::write(dir.path().join("ignored.py"), "secret = 1").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/dependency.js"), "x = 1").unwrap();
        let repo = Repo::discover_watch(dir.path()).unwrap();
        assert!(repo.standalone);
        assert_eq!(snapshot(&repo, &state).unwrap(), 1);
        assert!(pending_changes(&repo, &state, &[]).unwrap().is_empty());
        std::fs::write(dir.path().join("a.py"), "x = 2\ny = 3\n").unwrap();
        let changes = pending_changes(&repo, &state, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].base.as_deref(), Some(b"x = 1\n".as_slice()));
        let (diff, root, _tmp) = snapshot_diff(&state, &changes).unwrap();
        let opts = context::Options { with_context: false, ..Default::default() };
        let pack =
            context::build_from_diff(&repo, &DiffMode::Snapshot { label: "test".into(), root }, &diff, &opts).unwrap();
        assert_eq!(pack.files.len(), 1);
        assert!(pack.files[0].listing.as_ref().unwrap().contains("    2| y = 3"));
        // A file newly excluded by .gitignore must not leak through its old shadow.
        std::fs::write(dir.path().join(".gitignore"), "ignored.py\na.py\n").unwrap();
        assert!(pending_changes(&repo, &state, &[]).unwrap().is_empty());
        snapshot(&repo, &state).unwrap();
        assert_eq!(state.shadow("a.py"), Shadow::Missing);
        std::fs::write(dir.path().join(".gitignore"), "ignored.py\n").unwrap();
        snapshot(&repo, &state).unwrap();
        std::fs::remove_file(dir.path().join("a.py")).unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();
        let changes = pending_changes(&repo, &state, &[]).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.rel == "a.py" && c.current.is_none()));
        assert!(changes.iter().any(|c| c.rel == "new.rs" && c.base.is_none()));
        assert!(!dir.path().join(".git").exists());
    }

    #[test]
    fn key_is_filesystem_safe_and_stable() {
        assert_eq!(key("src/main.rs"), "src%2Fmain.rs");
        assert_eq!(key("a b/ü.txt"), "a%20b%2F%C3%BC.txt");
        let long = "x/".repeat(200);
        let k = key(&long);
        assert!(k.len() < 160);
        assert_eq!(k, key(&long));
    }

    fn git_repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let st = Command::new("git").arg("-C").arg(dir.path()).args(args).status().unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        let repo = Repo::discover(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn reviewed_deletion_is_not_pending_again() {
        let (dir, repo) = git_repo();
        let state = State::open(&repo).unwrap();
        // A fresh edit is pending against HEAD.
        std::fs::write(dir.path().join("a.py"), "x = 2\n").unwrap();
        let p = pending_changes(&repo, &state, &[]).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].base.as_deref(), Some(b"x = 1\n".as_slice()));
        // Reviewed: now the same content is the baseline.
        state.set_shadow("a.py", Some(b"x = 2\n"));
        assert!(pending_changes(&repo, &state, &[]).unwrap().is_empty());
        // Deleted, then reviewed as deleted: stays quiet even though git still lists it.
        std::fs::remove_file(dir.path().join("a.py")).unwrap();
        assert_eq!(pending_changes(&repo, &state, &[]).unwrap().len(), 1);
        state.set_shadow("a.py", None);
        assert_eq!(state.shadow("a.py"), Shadow::Deleted);
        assert!(pending_changes(&repo, &state, &[]).unwrap().is_empty());
        // Recreated: pending again, against the deleted baseline.
        std::fs::write(dir.path().join("a.py"), "x = 3\n").unwrap();
        let p = pending_changes(&repo, &state, &[]).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].base.is_none());
        // Restored to HEAD and snapshotted: the baseline disappears.
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        snapshot(&repo, &state).unwrap();
        assert_eq!(state.shadow("a.py"), Shadow::Missing);
        assert!(state.shadow_paths().is_empty());
    }

    #[test]
    fn head_moving_to_unrelated_commit_resets_baseline() {
        let (dir, repo) = git_repo();
        let state = State::open(&repo).unwrap();
        snapshot(&repo, &state).unwrap();
        state.set_shadow("a.py", Some(b"reviewed\n"));
        // A commit on top keeps the baseline; a switch to an unrelated branch drops it.
        std::fs::write(dir.path().join("b.py"), "y = 1\n").unwrap();
        repo.git(&["add", "."]).unwrap();
        repo.git(&["commit", "-qm", "two"]).unwrap();
        assert_eq!(pending_changes(&repo, &state, &[]).unwrap().len(), 1, "a.py differs from its shadow");
        repo.git(&["checkout", "-q", "--orphan", "other"]).unwrap();
        repo.git(&["commit", "-qm", "orphan"]).unwrap();
        assert!(pending_changes(&repo, &state, &[]).unwrap().is_empty());
        assert_eq!(state.shadow("a.py"), Shadow::Missing);
    }

    #[test]
    fn timestamp_civil_dates() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(timestamp(1_775_000_000), "2026-03-31 23:33:20Z");
    }

    #[test]
    fn format_for_agent_orders_and_clips() {
        let f = |sev: Severity, title: &str| Finding {
            severity: sev,
            category: "bug".into(),
            file: "a.rs".into(),
            line: Some(3),
            end_line: None,
            title: title.into(),
            body: "b".repeat(900),
            suggestion: Some("fix it".into()),
            models: vec![],
        };
        let r = Report {
            at: 0,
            files: vec!["a.rs".into()],
            models: vec!["m".into()],
            summary: String::new(),
            findings: vec![f(Severity::Medium, "medium one"), f(Severity::Blocker, "blocker one")],
            errors: vec![],
            elapsed_ms: 1000,
        };
        let text = format_for_agent(&[r], None);
        let b = text.find("BLOCKER").unwrap();
        let m = text.find("MEDIUM").unwrap();
        assert!(b < m);
        assert!(text.contains("found 2 issue(s)"));
        assert!(text.contains('…'));
        assert!(text.contains("Fix: fix it"));
    }

    #[test]
    fn empty_title_falls_back_to_first_sentence() {
        let f = Finding {
            severity: Severity::High,
            category: "bug".into(),
            file: "a.rs".into(),
            line: None,
            end_line: None,
            title: "  ".into(),
            body: "Returns the wrong element. Then more detail.".into(),
            suggestion: None,
            models: vec![],
        };
        let (t, b) = title_and_body(&f);
        assert_eq!(t, "Returns the wrong element");
        assert_eq!(b.trim(), "Then more detail.");
    }
}
