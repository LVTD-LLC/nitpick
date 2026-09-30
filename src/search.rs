//! Repo-wide search built on the ripgrep crates. One parallel walk that
//! respects .gitignore, skips binaries and junk, and runs every pattern
//! against every file.

use anyhow::Result;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ignore::{WalkBuilder, WalkState};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Hit {
    pub pattern: usize,
    pub path: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    /// Every candidate source file in the repo (relative, forward slashes).
    pub files: Vec<String>,
    pub hits: Vec<Hit>,
}

const JUNK_SUFFIXES: &[&str] = &[
    ".lock",
    ".min.js",
    ".min.css",
    ".map",
    ".svg",
    ".snap",
    ".pb.go",
    ".pb.ts",
    ".d.ts.map",
    ".wasm",
    ".ico",
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".webp",
    ".pdf",
    ".zip",
    ".gz",
    ".tar",
    ".woff",
    ".woff2",
    ".ttf",
    ".otf",
    ".mp4",
    ".mp3",
    ".bin",
    ".exe",
    ".dll",
    ".so",
    ".dylib",
    ".class",
    ".jar",
    ".pyc",
    ".sqlite",
    ".db",
];
const JUNK_NAMES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
    "bun.lockb",
    "flake.lock",
];
const JUNK_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    "coverage",
    ".turbo",
    ".cache",
    "out",
];

const CODE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go", "java", "kt", "kts", "scala", "rb",
    "php", "cs", "fs", "swift", "m", "mm", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "ex", "exs", "erl", "hs", "ml",
    "clj", "cljs", "lua", "dart", "zig", "nim", "vue", "svelte", "astro", "sql", "proto", "graphql", "gql", "sh",
    "bash", "zsh", "ps1", "tf", "hcl", "cmake", "gradle", "r", "jl", "pl", "pm", "elm", "res", "resi", "sol",
];

/// Source code, as opposed to docs, data and config. Call sites and
/// definitions are only looked for in these.
pub fn is_code_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rsplit('.').next() {
        Some(ext) if ext != name => CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        _ => matches!(name, "Makefile" | "Dockerfile" | "Justfile" | "Rakefile"),
    }
}

pub fn is_junk(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if JUNK_NAMES.contains(&name) || JUNK_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    rel.split('/').any(|seg| JUNK_DIRS.contains(&seg))
}

/// Walk the repo once and search each file for each pattern. Patterns are
/// regexes; hit.pattern is the index into `patterns`. `max_hits_per_pattern`
/// bounds runaway matches on common identifiers.
pub fn search(
    root: &Path,
    patterns: &[String],
    exclude: &HashSet<String>,
    max_hits_per_pattern: usize,
) -> Result<SearchResult> {
    let matchers: Vec<RegexMatcher> =
        patterns.iter().map(|p| RegexMatcher::new_line_matcher(p)).collect::<Result<_, _>>()?;
    let matchers = Arc::new(matchers);
    let exclude = Arc::new(exclude.clone());
    let files: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let hits: Arc<Mutex<Vec<Hit>>> = Arc::new(Mutex::new(Vec::new()));
    let counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![0; patterns.len()]));
    let root_buf = root.to_path_buf();

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_filesize(Some(768 * 1024))
        .build_parallel();

    walker.run(|| {
        let matchers = Arc::clone(&matchers);
        let exclude = Arc::clone(&exclude);
        let files = Arc::clone(&files);
        let hits = Arc::clone(&hits);
        let counts = Arc::clone(&counts);
        let root = root_buf.clone();
        let mut searcher =
            SearcherBuilder::new().binary_detection(BinaryDetection::quit(b'\x00')).line_number(true).build();
        Box::new(move |entry| {
            let Ok(entry) = entry else { return WalkState::Continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let rel = match path.strip_prefix(&root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => return WalkState::Continue,
            };
            if is_junk(&rel) {
                return WalkState::Continue;
            }
            files.lock().unwrap().push(rel.clone());
            if exclude.contains(&rel) || matchers.is_empty() {
                return WalkState::Continue;
            }
            for (i, m) in matchers.iter().enumerate() {
                if counts.lock().unwrap()[i] >= max_hits_per_pattern {
                    continue;
                }
                let mut local: Vec<Hit> = Vec::new();
                let _ = searcher.search_path(
                    m,
                    path,
                    UTF8(|lnum, line| {
                        local.push(Hit {
                            pattern: i,
                            path: rel.clone(),
                            line: lnum as u32,
                            text: line.trim_end().to_string(),
                        });
                        Ok(local.len() < 64)
                    }),
                );
                if !local.is_empty() {
                    let mut c = counts.lock().unwrap();
                    let room = max_hits_per_pattern.saturating_sub(c[i]);
                    local.truncate(room);
                    c[i] += local.len();
                    hits.lock().unwrap().extend(local);
                }
            }
            WalkState::Continue
        })
    });

    let mut files = Arc::try_unwrap(files).map(|m| m.into_inner().unwrap()).unwrap_or_default();
    files.sort();
    let mut hits = Arc::try_unwrap(hits).map(|m| m.into_inner().unwrap()).unwrap_or_default();
    hits.sort_by(|a, b| (a.pattern, &a.path, a.line).cmp(&(b.pattern, &b.path, b.line)));
    Ok(SearchResult { files, hits })
}

pub fn path_stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.split('.').next().unwrap_or(name)
}

pub fn path_dir(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

pub fn join_rel(dir: &str, rest: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn is_test_path(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let lower = name.to_ascii_lowercase();
    lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.starts_with("test_")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.go")
        || lower.ends_with("_tests.rs")
        || lower.ends_with("_test.rs")
        || rel.split('/').any(|s| matches!(s, "tests" | "test" | "__tests__" | "spec" | "specs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_rel_resolves_dots() {
        assert_eq!(join_rel("src/a/b", "../c"), "src/a/c");
        assert_eq!(join_rel("src", "./x/y"), "src/x/y");
        assert_eq!(join_rel("", "x"), "x");
    }

    #[test]
    fn junk_detection() {
        assert!(is_junk("package-lock.json"));
        assert!(is_junk("a/node_modules/b.js"));
        assert!(is_junk("x.min.js"));
        assert!(!is_junk("src/main.rs"));
    }

    #[test]
    fn code_files() {
        assert!(is_code_file("src/app.py"));
        assert!(is_code_file("Makefile"));
        assert!(!is_code_file("docs/api.rst"));
        assert!(!is_code_file("README.md"));
        assert!(!is_code_file("package.json"));
    }

    #[test]
    fn test_paths() {
        assert!(is_test_path("src/foo.test.ts"));
        assert!(is_test_path("tests/test_foo.py"));
        assert!(is_test_path("pkg/x_test.go"));
        assert!(!is_test_path("src/foo.ts"));
    }
}
