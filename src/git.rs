//! Thin wrapper around the `git` binary. We shell out on purpose: it is
//! always present where an agent runs, and libgit2 buys nothing here.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What we are diffing against.
#[derive(Debug, Clone)]
pub enum DiffMode {
    /// Working tree (tracked changes) against a commit, plus untracked files.
    WorkingTreeVs { rev: String, label: String },
    /// Index only (`git diff --cached`).
    Staged,
    /// An explicit revision expression such as `main..feature` or `abc123`.
    Range { expr: String, to: String },
    /// A diff supplied by the caller (the watch daemon's incremental review).
    /// Changed files are read from `root` when present there, otherwise from
    /// the working tree.
    Snapshot { label: String, root: PathBuf },
}

impl DiffMode {
    pub fn label(&self) -> String {
        match self {
            DiffMode::WorkingTreeVs { label, .. } => label.clone(),
            DiffMode::Staged => "staged changes".to_string(),
            DiffMode::Range { expr, .. } => format!("range {expr}"),
            DiffMode::Snapshot { label, .. } => label.clone(),
        }
    }

    pub fn includes_untracked(&self) -> bool {
        matches!(self, DiffMode::WorkingTreeVs { .. })
    }
}

pub struct Repo {
    pub root: PathBuf,
    pub standalone: bool,
}

impl Repo {
    pub fn discover(start: &Path) -> Result<Self> {
        let out = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git; is it installed?")?;
        if !out.status.success() {
            bail!("not inside a git repository: {}", start.display());
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Self { root: PathBuf::from(root), standalone: false })
    }

    /// Watch also supports a plain working folder; ordinary reviews still require Git.
    pub fn discover_watch(start: &Path) -> Result<Self> {
        if let Ok(repo) = Self::discover(start) {
            return Ok(repo);
        }
        let root = std::fs::canonicalize(start).context("resolving watch folder")?;
        // Do not reinterpret a broken checkout as a standalone folder.
        if root.ancestors().any(|p| p.join(".git").exists()) {
            bail!("cannot discover Git repository at {}", root.display());
        }
        Ok(Self { root, standalone: true })
    }

    /// In a plain folder, only source files are candidates. Respect ignore files,
    /// prune generated directories and nested repositories, and never follow symlinks.
    pub fn watch_files(&self) -> Result<Vec<String>> {
        if !self.standalone {
            return self.dirty_files();
        }
        let root = self.root.clone();
        let walker = ignore::WalkBuilder::new(&self.root)
            .require_git(false)
            .max_filesize(Some(400_000))
            .filter_entry(move |e| {
                let rel = e.path().strip_prefix(&root).unwrap_or(e.path()).to_string_lossy();
                !crate::search::is_junk(&rel)
                    && (e.depth() == 0 || !e.path().is_dir() || !e.path().join(".git").exists())
            })
            .build();
        let mut files = Vec::new();
        for entry in walker {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let rel = entry.path().strip_prefix(&self.root)?.to_string_lossy().replace('\\', "/");
                if crate::search::is_code_file(&rel) {
                    files.push(rel);
                }
            }
        }
        Ok(files)
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git").arg("-C").arg(&self.root).args(args).output().context("failed to run git")?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn git_ok(&self, args: &[&str]) -> Option<String> {
        self.git(args).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn head_short(&self) -> String {
        self.git_ok(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "(no commits)".into())
    }

    pub fn current_branch(&self) -> Option<String> {
        self.git_ok(&["symbolic-ref", "--short", "-q", "HEAD"])
    }

    /// Best guess at the integration branch: `origin/HEAD` if set, else the
    /// first of the usual names that exists locally.
    pub fn default_branch(&self) -> Option<String> {
        if let Some(s) = self.git_ok(&["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]) {
            return Some(s);
        }
        for c in ["main", "master", "develop", "trunk"] {
            if self.rev_exists(&format!("refs/heads/{c}")) {
                return Some(c.to_string());
            }
        }
        None
    }

    pub fn rev_exists(&self, rev: &str) -> bool {
        self.git_ok(&["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")]).is_some()
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.git_ok(&["merge-base", a, b])
    }

    pub fn rev_parse(&self, rev: &str) -> Option<String> {
        self.git_ok(&["rev-parse", "--verify", "-q", rev])
    }

    /// Decide what to diff. Priority: explicit range, staged, explicit base,
    /// then auto-detect: branch vs its merge-base with the default branch, or
    /// working tree vs HEAD when already on the default branch.
    pub fn resolve_mode(&self, base: Option<&str>, staged: bool, range: Option<&str>) -> Result<DiffMode> {
        if let Some(expr) = range {
            let to = expr.rsplit("..").next().unwrap_or(expr).trim_start_matches('.').to_string();
            let to = if to.is_empty() { "HEAD".to_string() } else { to };
            return Ok(DiffMode::Range { expr: expr.to_string(), to });
        }
        if staged {
            return Ok(DiffMode::Staged);
        }
        if let Some(base) = base {
            if !self.rev_exists(base) {
                bail!("base revision `{base}` does not exist");
            }
            return Ok(self.working_tree_vs_base(base));
        }
        let head = self.rev_parse("HEAD");
        let Some(head) = head else {
            // Fresh repo with no commits: everything is untracked.
            return Ok(DiffMode::WorkingTreeVs { rev: String::new(), label: "working tree (no commits yet)".into() });
        };
        let default = self.default_branch();
        let current = self.current_branch();
        let default_short = default.as_deref().map(|d| d.rsplit('/').next().unwrap_or(d).to_string());

        if let Some(default) = default.as_deref() {
            let on_default = current.as_deref() == default_short.as_deref();
            if !on_default {
                if let Some(mb) = self.merge_base("HEAD", default)
                    && mb != head
                {
                    let short = mb[..mb.len().min(10)].to_string();
                    return Ok(DiffMode::WorkingTreeVs {
                        rev: mb,
                        label: format!("working tree vs merge-base with {default} ({short})"),
                    });
                }
            } else {
                // On the default branch: compare with its upstream if it diverged.
                let upstream = format!("origin/{}", default_short.as_deref().unwrap_or(default));
                if self.rev_exists(&upstream)
                    && let Some(mb) = self.merge_base("HEAD", &upstream)
                    && mb != head
                {
                    let short = mb[..mb.len().min(10)].to_string();
                    return Ok(DiffMode::WorkingTreeVs {
                        rev: mb,
                        label: format!("working tree vs merge-base with {upstream} ({short})"),
                    });
                }
            }
        }
        Ok(DiffMode::WorkingTreeVs { rev: "HEAD".into(), label: "working tree vs HEAD".into() })
    }

    fn working_tree_vs_base(&self, base: &str) -> DiffMode {
        match self.merge_base("HEAD", base) {
            Some(mb) => {
                let short = &mb[..mb.len().min(10)];
                DiffMode::WorkingTreeVs {
                    rev: mb.clone(),
                    label: format!("working tree vs merge-base with {base} ({short})"),
                }
            }
            None => DiffMode::WorkingTreeVs { rev: base.to_string(), label: format!("working tree vs {base}") },
        }
    }

    /// Produce a unified diff for the mode. Untracked files are appended as
    /// synthetic "new file" diffs so they get reviewed too.
    pub fn diff(&self, mode: &DiffMode, paths: &[String], include_untracked: bool) -> Result<String> {
        let mut args: Vec<String> =
            vec!["diff".into(), "--no-color".into(), "--no-ext-diff".into(), "-U3".into(), "--find-renames".into()];
        match mode {
            DiffMode::WorkingTreeVs { rev, .. } => {
                if !rev.is_empty() {
                    args.push(rev.clone());
                }
            }
            DiffMode::Staged => args.push("--cached".into()),
            DiffMode::Range { expr, .. } => args.push(expr.clone()),
            DiffMode::Snapshot { .. } => bail!("snapshot mode supplies its own diff"),
        }
        if !paths.is_empty() {
            args.push("--".into());
            args.extend(paths.iter().cloned());
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = match mode {
            DiffMode::WorkingTreeVs { rev, .. } if rev.is_empty() => String::new(),
            _ => self.git(&argv)?,
        };

        if include_untracked && mode.includes_untracked() {
            for file in self.untracked_files()? {
                if !paths.is_empty() && !paths.iter().any(|p| file.starts_with(p.trim_end_matches('/'))) {
                    continue;
                }
                let abs = self.root.join(&file);
                let Ok(meta) = std::fs::metadata(&abs) else { continue };
                if meta.len() > 400_000 {
                    continue;
                }
                // Exit status is 1 when files differ, so ignore it and take stdout.
                let o = Command::new("git")
                    .arg("-C")
                    .arg(&self.root)
                    .args(["diff", "--no-color", "--no-ext-diff", "-U3", "--no-index", "--", "/dev/null", &file])
                    .output()?;
                out.push_str(&String::from_utf8_lossy(&o.stdout));
            }
        }
        Ok(out)
    }

    pub fn untracked_files(&self) -> Result<Vec<String>> {
        let out = self.git(&["ls-files", "--others", "--exclude-standard"])?;
        Ok(out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
    }

    /// Absolute path of the git directory for this checkout. For a linked
    /// worktree this is `.git/worktrees/<name>`, so per-checkout state kept
    /// under it never mixes with another worktree's.
    pub fn git_dir(&self) -> Result<PathBuf> {
        let out = self.git(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(out.trim()))
    }

    /// Repo-relative paths of every tracked file with uncommitted changes
    /// plus every untracked, non-ignored file. Renames report the new path.
    pub fn dirty_files(&self) -> Result<Vec<String>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])
            .output()
            .context("failed to run git status")?;
        if !out.status.success() {
            bail!("git status failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut files: Vec<String> = Vec::new();
        // Each entry is "XY path" (two status letters, a space, the path).
        for entry in text.split('\0') {
            let b = entry.as_bytes();
            if b.len() < 4 || b[2] != b' ' {
                continue;
            }
            files.push(entry[3..].to_string());
        }
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// `Some(true)` when `ancestor` is reachable from `rev` (or equal to
    /// it), `Some(false)` when git says it is not, `None` when git could not
    /// answer (unknown revision, locked repo), which callers must not read
    /// as either.
    pub fn is_ancestor(&self, ancestor: &str, rev: &str) -> Option<bool> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["merge-base", "--is-ancestor", ancestor, rev])
            .output()
            .ok()?;
        match out.status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        }
    }

    /// Content of `rel` at HEAD, or `None` if it is not tracked there.
    pub fn show_head(&self, rel: &str) -> Option<Vec<u8>> {
        self.git_bytes(&["show", &format!("HEAD:{rel}")])
    }

    /// Read the post-change content of a file for the given mode. Returns
    /// `None` for binary or missing files.
    pub fn read_file(&self, mode: &DiffMode, rel: &str) -> Result<Option<String>> {
        let bytes = match mode {
            DiffMode::WorkingTreeVs { .. } => match std::fs::read(self.root.join(rel)) {
                Ok(b) => b,
                Err(_) => return Ok(None),
            },
            DiffMode::Staged => match self.git_bytes(&["show", &format!(":{rel}")]) {
                Some(b) => b,
                None => return Ok(None),
            },
            DiffMode::Range { to, .. } => match self.git_bytes(&["show", &format!("{to}:{rel}")]) {
                Some(b) => b,
                None => return Ok(None),
            },
            DiffMode::Snapshot { root, .. } => {
                let snap = root.join(rel);
                match std::fs::read(&snap).or_else(|_| std::fs::read(self.root.join(rel))) {
                    Ok(b) => b,
                    Err(_) => return Ok(None),
                }
            }
        };
        if bytes.iter().take(8000).any(|&b| b == 0) {
            return Ok(None);
        }
        Ok(String::from_utf8(bytes).ok())
    }

    fn git_bytes(&self, args: &[&str]) -> Option<Vec<u8>> {
        let out = Command::new("git").arg("-C").arg(&self.root).args(args).output().ok()?;
        if out.status.success() { Some(out.stdout) } else { None }
    }
}
