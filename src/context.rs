//! The context engine. Turns "what changed" into a token-budgeted pack of
//! the diff, the full changed files, and the parts of the repo that matter
//! for judging the change: definitions of things the diff uses, call sites
//! of things the diff changed, imported modules, and matching tests.

use crate::diff::{self, FileDiff, Status};
use crate::git::{DiffMode, Repo};
use crate::lang::{self, Definition, Lang};
use crate::search::{self, Hit, is_test_path, join_rel, path_dir, path_stem};
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Options {
    pub budget_tokens: usize,
    pub max_file_lines: usize,
    pub window: usize,
    pub with_context: bool,
    pub include_tests: bool,
    pub include_untracked: bool,
    pub paths: Vec<String>,
    pub ignore: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            budget_tokens: 80_000,
            max_file_lines: 400,
            window: 40,
            with_context: true,
            include_tests: true,
            include_untracked: true,
            paths: Vec::new(),
            ignore: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: Status,
    pub lang: Option<&'static str>,
    pub added: usize,
    pub removed: usize,
    /// Numbered listing of the post-change file (full or windowed).
    pub listing: Option<String>,
    pub listing_note: Option<String>,
    pub changed_ranges: Vec<(u32, u32)>,
    pub changed_symbols: Vec<String>,
    #[serde(skip)]
    pub search_symbols: Vec<String>,
    #[serde(skip)]
    pub referenced: Vec<String>,
    #[serde(skip)]
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnippetKind {
    Definition,
    CallSite,
    Import,
    Test,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub kind: SnippetKind,
    pub reason: String,
    pub text: String,
    pub score: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub files_changed: usize,
    pub files_scanned: usize,
    pub estimated_tokens: usize,
    pub diff_tokens: usize,
    pub files_tokens: usize,
    pub snippet_tokens: usize,
    pub snippets_kept: usize,
    pub snippets_dropped: usize,
    pub build_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub repo_root: String,
    pub mode_label: String,
    pub head: String,
    pub branch: Option<String>,
    pub diff_text: String,
    pub files: Vec<ChangedFile>,
    pub snippets: Vec<Snippet>,
    /// (changed file, files that import it)
    pub importers: Vec<(String, Vec<String>)>,
    pub stats: Stats,
}

impl ContextPack {
    pub fn is_empty(&self) -> bool {
        self.diff_text.trim().is_empty()
    }
}

/// Fast token estimate. Code averages ~3.3 chars per token on modern tokenizers.
pub fn estimate_tokens(s: &str) -> usize {
    s.len() * 10 / 33 + 1
}

pub fn number_lines(lines: &[&str], start: u32) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 8).sum());
    for (i, l) in lines.iter().enumerate() {
        out.push_str(&format!("{:>5}| {}\n", start as usize + i, l));
    }
    out
}

fn is_container(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "abstract_class_declaration"
            | "class_definition"
            | "impl_item"
            | "trait_item"
            | "mod_item"
            | "internal_module"
            | "interface_declaration"
    )
}

fn merge_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = out.last_mut()
            && s <= last.1 + 1
        {
            last.1 = last.1.max(e);
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Numbered listing limited to windows around the changed ranges.
pub fn windowed_listing(lines: &[&str], ranges: &[(u32, u32)], window: usize) -> String {
    let n = lines.len() as u32;
    if n == 0 {
        return String::new();
    }
    let expanded: Vec<(u32, u32)> =
        ranges.iter().map(|&(s, e)| (s.saturating_sub(window as u32).max(1), (e + window as u32).min(n))).collect();
    let merged = merge_ranges(expanded);
    let mut out = String::new();
    let mut last_end = 0u32;
    for (s, e) in merged {
        if s > last_end + 1 {
            out.push_str(&format!("     ... ({} lines omitted)\n", s - last_end - 1));
        }
        out.push_str(&number_lines(&lines[(s - 1) as usize..e as usize], s));
        last_end = e;
    }
    if last_end < n {
        out.push_str(&format!("     ... ({} lines omitted)\n", n - last_end));
    }
    out
}

fn glob_matches(globs: &[String], rel: &str) -> bool {
    if globs.is_empty() {
        return false;
    }
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        if let Ok(gl) = globset::Glob::new(g) {
            b.add(gl);
        }
    }
    b.build().map(|s| s.is_match(rel)).unwrap_or(false)
}

/// Resolve an import specifier from `from_file` to a repo-relative path, if it
/// points at a file in this repo.
fn resolve_import(lang: Lang, from_file: &str, spec: &str, files: &HashSet<String>) -> Option<String> {
    let dir = path_dir(from_file);
    let exists = |p: &str| files.contains(p);
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            let base = if spec.starts_with('.') {
                join_rel(dir, spec)
            } else if let Some(rest) = spec.strip_prefix("@/") {
                format!("src/{rest}")
            } else {
                let rest = spec.strip_prefix("~/")?;
                format!("src/{rest}")
            };
            let base = base.trim_end_matches(".js").trim_end_matches(".ts").to_string();
            let candidates = [
                format!("{base}.ts"),
                format!("{base}.tsx"),
                format!("{base}.js"),
                format!("{base}.jsx"),
                format!("{base}.mjs"),
                format!("{base}/index.ts"),
                format!("{base}/index.tsx"),
                format!("{base}/index.js"),
                base.clone(),
            ];
            candidates.into_iter().find(|c| exists(c))
        }
        Lang::Python => {
            let (dots, name) = {
                let dots = spec.chars().take_while(|&c| c == '.').count();
                (dots, &spec[dots..])
            };
            let rel_path = name.replace('.', "/");
            let mut roots: Vec<String> = Vec::new();
            if dots > 0 {
                let mut d = dir.to_string();
                for _ in 1..dots {
                    d = path_dir(&d).to_string();
                }
                roots.push(d);
            } else {
                roots.push(String::new());
                roots.push("src".into());
                roots.push(dir.to_string());
                // walk up a few parents so `app.models` resolves inside monorepos
                let mut d = dir.to_string();
                for _ in 0..3 {
                    d = path_dir(&d).to_string();
                    roots.push(d.clone());
                }
            }
            for r in roots {
                let base = join_rel(&r, &rel_path);
                for c in [format!("{base}.py"), format!("{base}/__init__.py")] {
                    if exists(&c) {
                        return Some(c);
                    }
                }
            }
            None
        }
        Lang::Rust => {
            if let Some(m) = spec.strip_prefix("mod:") {
                let is_mod_root =
                    from_file.ends_with("/mod.rs") || from_file.ends_with("/lib.rs") || from_file.ends_with("/main.rs");
                let base = if is_mod_root { dir.to_string() } else { from_file.trim_end_matches(".rs").to_string() };
                for c in [join_rel(&base, &format!("{m}.rs")), join_rel(&base, &format!("{m}/mod.rs"))] {
                    if exists(&c) {
                        return Some(c);
                    }
                }
                return None;
            }
            let path = spec.strip_prefix("crate::").or_else(|| spec.strip_prefix("super::"))?;
            let segs: Vec<&str> = path.split("::").collect();
            let src_root = {
                // nearest ancestor dir that contains lib.rs or main.rs
                let mut d = dir.to_string();
                loop {
                    if exists(&join_rel(&d, "lib.rs")) || exists(&join_rel(&d, "main.rs")) {
                        break d;
                    }
                    if d.is_empty() {
                        break "src".to_string();
                    }
                    d = path_dir(&d).to_string();
                }
            };
            let root = if spec.starts_with("super::") { path_dir(dir).to_string() } else { src_root };
            // try longest module path first: a::b::c -> a/b/c.rs, a/b.rs, a.rs
            for take in (1..=segs.len()).rev() {
                let p = segs[..take].join("/");
                for c in [join_rel(&root, &format!("{p}.rs")), join_rel(&root, &format!("{p}/mod.rs"))] {
                    if exists(&c) {
                        return Some(c);
                    }
                }
            }
            None
        }
        Lang::Go => None,
    }
}

/// Lines and definitions of a file, or `None` if it is binary or unreadable.
type FileInfo = Option<(Vec<String>, Vec<Definition>)>;

struct DefCache<'a> {
    repo: &'a Repo,
    mode: &'a DiffMode,
    cache: HashMap<String, FileInfo>,
}

impl<'a> DefCache<'a> {
    fn get(&mut self, rel: &str) -> Option<&(Vec<String>, Vec<Definition>)> {
        if !self.cache.contains_key(rel) {
            let entry = self.repo.read_file(self.mode, rel).ok().flatten().map(|content| {
                let defs = Lang::from_path(std::path::Path::new(rel))
                    .map(|l| lang::definitions(l, &content))
                    .unwrap_or_default();
                let lines: Vec<String> = content.lines().map(str::to_string).collect();
                (lines, defs)
            });
            self.cache.insert(rel.to_string(), entry);
        }
        self.cache.get(rel).and_then(|e| e.as_ref())
    }
}

fn outline(lines: &[String], defs: &[Definition], rel: &str) -> String {
    if lines.len() <= 120 {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        return number_lines(&refs, 1);
    }
    let mut out = String::new();
    out.push_str(&format!("     (outline of {rel}: {} lines, {} definitions)\n", lines.len(), defs.len()));
    for d in defs.iter().take(80) {
        let l = lines.get((d.start_line - 1) as usize).map(String::as_str).unwrap_or("");
        out.push_str(&format!("{:>5}| {}\n", d.start_line, l.trim_end()));
    }
    out
}

pub fn build(repo: &Repo, mode: &DiffMode, opts: &Options) -> Result<ContextPack> {
    let t0 = Instant::now();
    let raw_diff = repo.diff(mode, &opts.paths, opts.include_untracked)?;
    let parsed = diff::parse(&raw_diff);
    let parsed: Vec<FileDiff> =
        parsed.into_iter().filter(|f| !glob_matches(&opts.ignore, f.path()) && !search::is_junk(f.path())).collect();
    // For brand-new text files the full numbered listing below carries the
    // content, so the diff keeps only the header instead of repeating it.
    let diff_text: String = parsed
        .iter()
        .map(|f| {
            if f.status == Status::Added && !f.binary && f.hunks.len() == 1 {
                let header: String =
                    f.raw.lines().take_while(|l| !l.starts_with("@@")).map(|l| format!("{l}\n")).collect();
                format!("{header}(new file, {} lines: full content is in <changed_files>)\n", f.added_count())
            } else {
                f.raw.clone()
            }
        })
        .collect();

    let mut pack = ContextPack {
        repo_root: repo.root.to_string_lossy().to_string(),
        mode_label: mode.label(),
        head: repo.head_short(),
        branch: repo.current_branch(),
        diff_text,
        files: Vec::new(),
        snippets: Vec::new(),
        importers: Vec::new(),
        stats: Stats::default(),
    };
    if parsed.is_empty() {
        pack.stats.build_ms = t0.elapsed().as_millis();
        return Ok(pack);
    }

    // ---- changed files -----------------------------------------------------
    let mut all_defs_in_changed: HashSet<String> = HashSet::new();
    for fd in &parsed {
        let path = fd.path().to_string();
        let lang = Lang::from_path(std::path::Path::new(&path));
        let mut cf = ChangedFile {
            path: path.clone(),
            status: fd.status,
            lang: lang.map(Lang::name),
            added: fd.added_count(),
            removed: fd.removed_count(),
            listing: None,
            listing_note: None,
            changed_ranges: fd.new_ranges(),
            changed_symbols: Vec::new(),
            search_symbols: Vec::new(),
            referenced: Vec::new(),
            imports: Vec::new(),
        };
        if fd.status != Status::Deleted
            && !fd.binary
            && let Some(content) = repo.read_file(mode, &path)?
        {
            let lines: Vec<&str> = content.lines().collect();
            let defs = lang.map(|l| lang::definitions(l, &content)).unwrap_or_default();
            for d in &defs {
                all_defs_in_changed.insert(d.name.clone());
            }
            let precise = fd.changed_ranges();
            let overlapping: Vec<&Definition> = defs.iter().filter(|d| d.overlaps(&precise)).collect();
            // Drop containers (impl blocks, classes, modules) when a
            // definition nested inside them is the real change.
            let innermost: Vec<&Definition> = overlapping
                .iter()
                .filter(|d| {
                    !overlapping
                        .iter()
                        .any(|o| !std::ptr::eq(*o, **d) && o.start_line >= d.start_line && o.end_line <= d.end_line)
                })
                .copied()
                .collect();
            cf.changed_symbols = innermost.iter().map(|d| d.name.clone()).collect::<HashSet<_>>().into_iter().collect();
            cf.changed_symbols.sort();
            // Searching the repo for every use of a big class or module just
            // because a line inside it moved is noise, not context.
            cf.search_symbols = innermost
                .iter()
                .filter(|d| !(is_container(&d.kind) && d.end_line - d.start_line > 60))
                .map(|d| d.name.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let added_text: String = fd.added_lines().map(|(_, t)| t).collect::<Vec<_>>().join("\n");
            cf.referenced = lang::interesting_identifiers(&added_text);
            cf.imports = lang.map(|l| lang::imports(l, &content)).unwrap_or_default();
            if lines.len() <= opts.max_file_lines {
                cf.listing = Some(number_lines(&lines, 1));
                cf.listing_note = Some(format!("full file, {} lines", lines.len()));
            } else {
                cf.listing = Some(windowed_listing(&lines, &cf.changed_ranges, opts.window));
                cf.listing_note =
                    Some(format!("{} lines total; showing ±{} lines around changes", lines.len(), opts.window));
            }
        }
        pack.files.push(cf);
    }
    pack.stats.files_changed = pack.files.len();

    if !opts.with_context {
        finalize(&mut pack, opts, t0);
        return Ok(pack);
    }

    // ---- what to look for ---------------------------------------------------
    let changed_set: HashSet<String> = pack.files.iter().map(|f| f.path.clone()).collect();
    let changed_symbols: Vec<String> = {
        let mut v: Vec<String> = pack.files.iter().flat_map(|f| f.search_symbols.iter().cloned()).collect();
        v.sort();
        v.dedup();
        v.retain(|s| lang::is_searchable(s) && !s.contains(' '));
        v.truncate(40);
        v
    };
    let referenced: Vec<String> = {
        let mut freq: HashMap<&str, usize> = HashMap::new();
        for f in &pack.files {
            for r in &f.referenced {
                *freq.entry(r.as_str()).or_default() += 1;
            }
        }
        let mut v: Vec<(&str, usize)> = freq
            .into_iter()
            .filter(|(s, _)| !all_defs_in_changed.contains(*s) && !changed_symbols.iter().any(|c| c == s))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.into_iter().map(|(s, _)| s.to_string()).take(60).collect()
    };
    let stems: Vec<(String, String)> = pack
        .files
        .iter()
        .filter(|f| f.status != Status::Deleted)
        .map(|f| (f.path.clone(), path_stem(&f.path).to_string()))
        .filter(|(_, s)| {
            s.len() >= 3
                && !matches!(s.as_str(), "index" | "mod" | "main" | "lib" | "init" | "__init__" | "utils" | "types")
        })
        .collect();

    let mut patterns: Vec<String> = Vec::new();
    let sym_pat_idx = if changed_symbols.is_empty() {
        None
    } else {
        patterns.push(format!(
            r"\b(?:{})\b",
            changed_symbols.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join("|")
        ));
        Some(patterns.len() - 1)
    };
    let ref_pat_idx = if referenced.is_empty() {
        None
    } else {
        patterns
            .push(format!(r"\b(?:{})\b", referenced.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join("|")));
        Some(patterns.len() - 1)
    };
    let imp_pat_idx = if stems.is_empty() {
        None
    } else {
        let alts = stems.iter().map(|(_, s)| regex::escape(s)).collect::<Vec<_>>().join("|");
        patterns.push(format!(
            r"^\s*(?:import|from|export|use|mod|require|include)\b[^\n]*\b(?:{alts})\b|require\([^)]*\b(?:{alts})\b"
        ));
        Some(patterns.len() - 1)
    };

    let result = search::search(&repo.root, &patterns, &changed_set, 600)?;
    pack.stats.files_scanned = result.files.len();
    let file_set: HashSet<String> = result.files.iter().cloned().collect();
    let mut cache = DefCache { repo, mode, cache: HashMap::new() };
    let mut snippets: Vec<Snippet> = Vec::new();

    let sym_regex = |list: &[String]| {
        regex::Regex::new(&format!(r"\b(?:{})\b", list.iter().map(|s| regex::escape(s)).collect::<Vec<_>>().join("|")))
            .ok()
    };
    let changed_re = sym_regex(&changed_symbols);
    let referenced_re = sym_regex(&referenced);

    // ---- definitions of referenced symbols ------------------------------------
    if let (Some(idx), Some(re)) = (ref_pat_idx, &referenced_re) {
        let mut found_defs: HashSet<String> = HashSet::new();
        let mut per_file_hits: HashMap<&str, Vec<&Hit>> = HashMap::new();
        for h in result.hits.iter().filter(|h| h.pattern == idx) {
            per_file_hits.entry(h.path.as_str()).or_default().push(h);
        }
        for (path, hits) in per_file_hits {
            if !search::is_code_file(path) {
                continue;
            }
            let Some((lines, defs)) = cache.get(path) else { continue };
            let lines_ref: Vec<&str> = lines.iter().map(String::as_str).collect();
            for h in hits {
                for m in re.find_iter(&h.text) {
                    let name = m.as_str();
                    if found_defs.contains(name) {
                        continue;
                    }
                    let def =
                        defs.iter().find(|d| d.name == name && d.start_line <= h.line && h.line <= d.start_line + 2);
                    let is_def = def.is_some() || (defs.is_empty() && lang::line_defines(&h.text, name));
                    if !is_def {
                        continue;
                    }
                    let (s, e) = match def {
                        Some(d) => (d.start_line, d.end_line.min(d.start_line + 119)),
                        None => (h.line, (h.line + 30).min(lines.len() as u32)),
                    };
                    if s == 0 || e < s || (e as usize) > lines_ref.len() {
                        continue;
                    }
                    found_defs.insert(name.to_string());
                    snippets.push(Snippet {
                        path: path.to_string(),
                        start: s,
                        end: e,
                        kind: SnippetKind::Definition,
                        reason: format!("defines `{name}`, used in the diff"),
                        text: number_lines(&lines_ref[(s - 1) as usize..e as usize], s),
                        score: 3.0,
                    });
                }
            }
        }
    }

    // ---- call sites of changed symbols ------------------------------------------
    if let (Some(idx), Some(re)) = (sym_pat_idx, &changed_re) {
        let mut per_symbol_files: HashMap<String, usize> = HashMap::new();
        let mut by_file: HashMap<&str, Vec<&Hit>> = HashMap::new();
        for h in result.hits.iter().filter(|h| h.pattern == idx) {
            by_file.entry(h.path.as_str()).or_default().push(h);
        }
        let mut files: Vec<&&str> = by_file.keys().collect();
        files.sort();
        for path in files {
            if !search::is_code_file(path) {
                continue;
            }
            let hits = &by_file[*path];
            let Some((lines, _)) = cache.get(path) else { continue };
            let lines_ref: Vec<&str> = lines.iter().map(String::as_str).collect();
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            let mut hit_names: Vec<(u32, String)> = Vec::new();
            let mut per_symbol_here: HashMap<String, usize> = HashMap::new();
            let mut blocked_here: HashSet<String> = HashSet::new();
            let mut seen_here: HashSet<String> = HashSet::new();
            for h in hits.iter() {
                let Some(m) = re.find(&h.text) else { continue };
                let name = m.as_str().to_string();
                if seen_here.insert(name.clone()) {
                    let c = per_symbol_files.entry(name.clone()).or_default();
                    if *c >= 8 {
                        blocked_here.insert(name.clone());
                    } else {
                        *c += 1;
                    }
                }
                if blocked_here.contains(&name) {
                    continue;
                }
                let here = per_symbol_here.entry(name.clone()).or_default();
                if *here >= 3 {
                    continue;
                }
                *here += 1;
                ranges.push((h.line.saturating_sub(4).max(1), (h.line + 4).min(lines_ref.len() as u32)));
                hit_names.push((h.line, name));
            }
            if ranges.is_empty() {
                continue;
            }
            for (s, e) in merge_ranges(ranges) {
                if (e as usize) > lines_ref.len() || s == 0 {
                    continue;
                }
                let mut names: Vec<&str> =
                    hit_names.iter().filter(|(l, _)| *l >= s && *l <= e).map(|(_, n)| n.as_str()).collect();
                names.sort();
                names.dedup();
                snippets.push(Snippet {
                    path: path.to_string(),
                    start: s,
                    end: e,
                    kind: SnippetKind::CallSite,
                    reason: format!("uses `{}`, which the diff changed", names.join("`, `")),
                    text: number_lines(&lines_ref[(s - 1) as usize..e as usize], s),
                    score: 2.0 + if is_test_path(path) { 0.3 } else { 0.0 },
                });
            }
        }
    }

    // ---- imported modules ----------------------------------------------------------
    let mut seen_imports: HashSet<String> = HashSet::new();
    let per_file_imports: Vec<(String, Option<Lang>, Vec<String>)> = pack
        .files
        .iter()
        .map(|f| (f.path.clone(), Lang::from_path(std::path::Path::new(&f.path)), f.imports.clone()))
        .collect();
    for (from, lang, imports) in per_file_imports {
        let Some(lang) = lang else { continue };
        for spec in imports {
            let Some(target) = resolve_import(lang, &from, &spec, &file_set) else { continue };
            if changed_set.contains(&target) || !seen_imports.insert(target.clone()) {
                continue;
            }
            let Some((lines, defs)) = cache.get(&target) else { continue };
            let text = outline(lines, defs, &target);
            let end = lines.len() as u32;
            snippets.push(Snippet {
                path: target.clone(),
                start: 1,
                end,
                kind: SnippetKind::Import,
                reason: format!("imported by `{from}`"),
                text,
                score: 1.0,
            });
        }
    }

    // ---- reverse importers ----------------------------------------------------------
    if let Some(idx) = imp_pat_idx {
        for (path, stem) in &stems {
            let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(stem))).unwrap();
            let mut importers: Vec<String> = result
                .hits
                .iter()
                .filter(|h| h.pattern == idx && re.is_match(&h.text) && &h.path != path)
                .map(|h| h.path.clone())
                .collect();
            importers.sort();
            importers.dedup();
            importers.truncate(25);
            if !importers.is_empty() {
                pack.importers.push((path.clone(), importers));
            }
        }
    }

    // ---- tests ----------------------------------------------------------------------------
    if opts.include_tests {
        let mut seen: HashSet<String> = HashSet::new();
        for (path, stem) in &stems {
            if is_test_path(path) {
                continue;
            }
            let stem_l = stem.to_ascii_lowercase();
            let candidates: Vec<&String> = result
                .files
                .iter()
                .filter(|f| !changed_set.contains(*f) && is_test_path(f))
                .filter(|f| {
                    let name = f.rsplit('/').next().unwrap_or(f).to_ascii_lowercase();
                    let fstem = path_stem(&name).to_string();
                    name.starts_with(&format!("{stem_l}.test."))
                        || name.starts_with(&format!("{stem_l}.spec."))
                        || name.starts_with(&format!("{stem_l}_test."))
                        || name.starts_with(&format!("test_{stem_l}."))
                        || fstem == stem_l
                        || fstem == format!("{stem_l}_tests")
                })
                .take(3)
                .collect();
            for t in candidates {
                if !seen.insert(t.clone()) {
                    continue;
                }
                let Some((lines, defs)) = cache.get(t) else { continue };
                let text = if lines.len() <= 250 {
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    number_lines(&refs, 1)
                } else {
                    outline(lines, defs, t)
                };
                snippets.push(Snippet {
                    path: t.clone(),
                    start: 1,
                    end: lines.len() as u32,
                    kind: SnippetKind::Test,
                    reason: format!("tests for `{path}`"),
                    text,
                    score: 1.5,
                });
            }
        }
    }

    // ---- dedupe overlapping snippets in the same file -----------------------------------
    // Whole-file outlines (imports, big tests) stay separate so they never
    // swallow the precise call-site and definition excerpts.
    let (outlines, precise): (Vec<Snippet>, Vec<Snippet>) = snippets
        .into_iter()
        .partition(|s| s.kind == SnippetKind::Import || (s.kind == SnippetKind::Test && s.start == 1));
    let mut snippets = precise;
    snippets.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    let mut deduped: Vec<Snippet> = Vec::new();
    for s in snippets {
        if let Some(last) = deduped.last_mut()
            && last.path == s.path
            && s.start <= last.end
        {
            // Overlap: keep the wider one, merge reasons, keep the higher score.
            if s.end > last.end || (s.start <= last.start && s.end >= last.end) {
                let (ns, ne) = (last.start.min(s.start), last.end.max(s.end));
                if let Some((lines, _)) = cache.get(&s.path) {
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    if (ne as usize) <= refs.len() {
                        last.text = number_lines(&refs[(ns - 1) as usize..ne as usize], ns);
                        last.start = ns;
                        last.end = ne;
                    }
                }
            }
            if !last.reason.contains(&s.reason) {
                last.reason = format!("{}; {}", last.reason, s.reason);
            }
            last.score = last.score.max(s.score);
            if s.kind == SnippetKind::Definition {
                last.kind = SnippetKind::Definition;
            }
            continue;
        }
        deduped.push(s);
    }
    // An outline of a file we already show precise excerpts from is lower value.
    let precise_paths: HashSet<String> = deduped.iter().map(|s| s.path.clone()).collect();
    for mut o in outlines {
        if o.kind == SnippetKind::Import && precise_paths.contains(&o.path) {
            o.score -= 0.5;
        }
        deduped.push(o);
    }
    pack.snippets = deduped;
    finalize(&mut pack, opts, t0);
    Ok(pack)
}

/// Apply the token budget: diff first, then changed-file listings, then
/// snippets by score. Shrinks listings before dropping them.
fn finalize(pack: &mut ContextPack, opts: &Options, t0: Instant) {
    let budget = opts.budget_tokens;
    let diff_tokens = estimate_tokens(&pack.diff_text);
    let mut files_tokens: usize = pack.files.iter().filter_map(|f| f.listing.as_deref()).map(estimate_tokens).sum();

    if diff_tokens + files_tokens > budget {
        // Shrink listings to tighter windows.
        for window in [20usize, 8] {
            for f in pack.files.iter_mut() {
                if let Some(listing) = &f.listing
                    && estimate_tokens(listing) > 1500
                {
                    // re-window from the listing itself is lossy; instead rebuild from disk is
                    // expensive, so approximate: keep only lines within window of changed ranges.
                    let lines: Vec<&str> = listing.lines().collect();
                    let mut keep: Vec<String> = Vec::new();
                    let mut last_kept: Option<u32> = None;
                    for l in &lines {
                        let no: Option<u32> = l.split('|').next().and_then(|n| n.trim().parse().ok());
                        let Some(no) = no else { continue };
                        let near =
                            f.changed_ranges.iter().any(|&(s, e)| no + window as u32 >= s && no <= e + window as u32);
                        if near {
                            if let Some(lk) = last_kept
                                && no > lk + 1
                            {
                                keep.push(format!("     ... ({} lines omitted)", no - lk - 1));
                            }
                            keep.push((*l).to_string());
                            last_kept = Some(no);
                        }
                    }
                    f.listing = Some(keep.join("\n") + "\n");
                    f.listing_note = Some(format!("showing ±{window} lines around changes (budget)"));
                }
            }
            files_tokens = pack.files.iter().filter_map(|f| f.listing.as_deref()).map(estimate_tokens).sum();
            if diff_tokens + files_tokens <= budget {
                break;
            }
        }
    }
    if diff_tokens + files_tokens > budget {
        for f in pack.files.iter_mut() {
            f.listing = None;
            f.listing_note = Some("omitted (over budget); see diff".into());
        }
        files_tokens = 0;
    }

    let mut remaining = budget.saturating_sub(diff_tokens + files_tokens);
    let mut snippets = std::mem::take(&mut pack.snippets);
    snippets
        .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.path.cmp(&b.path)));
    let mut kept: Vec<Snippet> = Vec::new();
    let mut dropped = 0usize;
    let mut snippet_tokens = 0usize;
    for s in snippets {
        let t = estimate_tokens(&s.text);
        if t <= remaining {
            remaining -= t;
            snippet_tokens += t;
            kept.push(s);
        } else {
            dropped += 1;
        }
    }
    kept.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    pack.stats.snippets_kept = kept.len();
    pack.stats.snippets_dropped = dropped;
    pack.snippets = kept;
    pack.stats.diff_tokens = diff_tokens;
    pack.stats.files_tokens = files_tokens;
    pack.stats.snippet_tokens = snippet_tokens;
    pack.stats.estimated_tokens = diff_tokens + files_tokens + snippet_tokens;
    pack.stats.build_ms = t0.elapsed().as_millis();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_listing_merges_and_elides() {
        let src: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let lines: Vec<&str> = src.iter().map(String::as_str).collect();
        let out = windowed_listing(&lines, &[(10, 12), (15, 15), (90, 90)], 3);
        assert!(out.contains("    7| line 7"));
        assert!(out.contains("   18| line 18"));
        assert!(!out.contains("   19| line 19"));
        assert!(out.contains("... (68 lines omitted)"));
        assert!(out.contains("... (7 lines omitted)"));
    }

    #[test]
    fn resolve_ts_relative() {
        let files: HashSet<String> =
            ["src/a/b.ts", "src/lib/index.ts", "src/x.tsx"].into_iter().map(String::from).collect();
        assert_eq!(resolve_import(Lang::TypeScript, "src/a/c.ts", "./b", &files).as_deref(), Some("src/a/b.ts"));
        assert_eq!(
            resolve_import(Lang::TypeScript, "src/a/c.ts", "../lib", &files).as_deref(),
            Some("src/lib/index.ts")
        );
        assert_eq!(resolve_import(Lang::TypeScript, "src/a/c.ts", "@/x", &files).as_deref(), Some("src/x.tsx"));
        assert_eq!(resolve_import(Lang::TypeScript, "src/a/c.ts", "react", &files), None);
    }

    #[test]
    fn resolve_rust_modules() {
        let files: HashSet<String> =
            ["src/main.rs", "src/git.rs", "src/ctx/mod.rs", "src/ctx/pack.rs"].into_iter().map(String::from).collect();
        assert_eq!(resolve_import(Lang::Rust, "src/main.rs", "mod:git", &files).as_deref(), Some("src/git.rs"));
        assert_eq!(resolve_import(Lang::Rust, "src/main.rs", "mod:ctx", &files).as_deref(), Some("src/ctx/mod.rs"));
        assert_eq!(
            resolve_import(Lang::Rust, "src/git.rs", "crate::ctx::pack::Thing", &files).as_deref(),
            Some("src/ctx/pack.rs")
        );
        assert_eq!(
            resolve_import(Lang::Rust, "src/ctx/mod.rs", "mod:pack", &files).as_deref(),
            Some("src/ctx/pack.rs")
        );
    }

    #[test]
    fn resolve_python() {
        let files: HashSet<String> =
            ["app/models.py", "app/api/views.py", "app/__init__.py"].into_iter().map(String::from).collect();
        assert_eq!(
            resolve_import(Lang::Python, "app/api/views.py", "app.models", &files).as_deref(),
            Some("app/models.py")
        );
        assert_eq!(
            resolve_import(Lang::Python, "app/api/views.py", "..models", &files).as_deref(),
            Some("app/models.py")
        );
        assert_eq!(resolve_import(Lang::Python, "app/api/views.py", "os", &files), None);
    }
}
