//! Minimal unified-diff parser. Only what the context engine needs: which
//! files changed, how, and which new-file line ranges were touched.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HunkLine {
    pub kind: LineKind,
    pub text: String,
    pub new_no: Option<u32>,
    pub old_no: Option<u32>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: Status,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    /// The raw text of this file's section, verbatim.
    pub raw: String,
}

impl FileDiff {
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("?")
    }

    /// Inclusive 1-based line ranges in the new file that each hunk covers.
    pub fn new_ranges(&self) -> Vec<(u32, u32)> {
        self.hunks
            .iter()
            .filter(|h| h.new_len > 0)
            .map(|h| (h.new_start, h.new_start + h.new_len - 1))
            .collect()
    }

    /// Line numbers (new file) that were added or modified.
    pub fn added_lines(&self) -> impl Iterator<Item = (u32, &str)> {
        self.hunks.iter().flat_map(|h| h.lines.iter()).filter_map(|l| {
            if l.kind == LineKind::Added { Some((l.new_no?, l.text.as_str())) } else { None }
        })
    }

    pub fn added_count(&self) -> usize {
        self.hunks.iter().flat_map(|h| h.lines.iter()).filter(|l| l.kind == LineKind::Added).count()
    }

    pub fn removed_count(&self) -> usize {
        self.hunks.iter().flat_map(|h| h.lines.iter()).filter(|l| l.kind == LineKind::Removed).count()
    }
}

fn strip_prefix_path(s: &str) -> Option<String> {
    let s = s.trim_end();
    let s = s.split('\t').next().unwrap_or(s);
    if s == "/dev/null" {
        return None;
    }
    let s = s.strip_prefix("a/").or_else(|| s.strip_prefix("b/")).unwrap_or(s);
    Some(s.trim_matches('"').to_string())
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    // @@ -a,b +c,d @@ optional context
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let spec = &rest[..end];
    let mut parts = spec.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse = |s: &str| -> Option<(u32, u32)> {
        let mut it = s.split(',');
        let start: u32 = it.next()?.parse().ok()?;
        let len: u32 = match it.next() {
            Some(l) => l.parse().ok()?,
            None => 1,
        };
        Some((start, len))
    };
    let (os, ol) = parse(old)?;
    let (ns, nl) = parse(new)?;
    Some((os, ol, ns, nl))
}

pub fn parse(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let mut in_hunk = false;
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    for line in text.split_inclusive('\n') {
        let l = line.strip_suffix('\n').unwrap_or(line);
        if let Some(rest) = l.strip_prefix("diff --git ") {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            in_hunk = false;
            // "a/x b/y" — paths with spaces are rare; take the split at " b/".
            let (a, b) = match rest.find(" b/") {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, rest),
            };
            cur = Some(FileDiff {
                old_path: strip_prefix_path(a),
                new_path: strip_prefix_path(b),
                status: Status::Modified,
                binary: false,
                hunks: Vec::new(),
                raw: String::new(),
            });
        }
        let Some(f) = cur.as_mut() else { continue };
        f.raw.push_str(line);

        if in_hunk && (l.starts_with(' ') || l.starts_with('+') || l.starts_with('-') || l.starts_with('\\') || l.is_empty()) {
            if l.starts_with("--- ") && !l.starts_with("--- a/") && f.hunks.is_empty() {
                // not a hunk line; fallthrough handled below
            }
            let hunk = f.hunks.last_mut().expect("hunk exists while in_hunk");
            let (kind, text) = match l.chars().next() {
                Some('+') => (LineKind::Added, &l[1..]),
                Some('-') => (LineKind::Removed, &l[1..]),
                Some(' ') => (LineKind::Context, &l[1..]),
                Some('\\') => continue, // "\ No newline at end of file"
                _ => (LineKind::Context, l),
            };
            let (o, n) = match kind {
                LineKind::Added => {
                    new_no += 1;
                    (None, Some(new_no))
                }
                LineKind::Removed => {
                    old_no += 1;
                    (Some(old_no), None)
                }
                LineKind::Context => {
                    old_no += 1;
                    new_no += 1;
                    (Some(old_no), Some(new_no))
                }
            };
            hunk.lines.push(HunkLine { kind, text: text.to_string(), new_no: n, old_no: o });
            continue;
        }

        if let Some(h) = parse_hunk_header(l) {
            in_hunk = true;
            old_no = h.0.saturating_sub(1);
            new_no = h.2.saturating_sub(1);
            f.hunks.push(Hunk { old_start: h.0, old_len: h.1, new_start: h.2, new_len: h.3, lines: Vec::new() });
            continue;
        }
        in_hunk = false;
        if l.starts_with("new file mode") {
            f.status = Status::Added;
        } else if l.starts_with("deleted file mode") {
            f.status = Status::Deleted;
        } else if l.starts_with("rename from ") || l.starts_with("rename to ") {
            f.status = Status::Renamed;
        } else if l.starts_with("Binary files") || l.starts_with("GIT binary patch") {
            f.binary = true;
        } else if let Some(p) = l.strip_prefix("--- ") {
            f.old_path = strip_prefix_path(p);
        } else if let Some(p) = l.strip_prefix("+++ ") {
            f.new_path = strip_prefix_path(p);
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    for f in &mut files {
        if f.old_path.is_none() && f.new_path.is_some() && f.status == Status::Modified {
            f.status = Status::Added;
        }
        if f.new_path.is_none() && f.old_path.is_some() {
            f.status = Status::Deleted;
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,4 @@\n fn a() {}\n-fn b() {}\n+fn b() { 1 }\n+fn c() {}\n fn d() {}\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";

    #[test]
    fn parses_files_and_hunks() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "src/a.rs");
        assert_eq!(files[0].status, Status::Modified);
        assert_eq!(files[0].new_ranges(), vec![(1, 4)]);
        let added: Vec<_> = files[0].added_lines().collect();
        assert_eq!(added, vec![(2, "fn b() { 1 }"), (3, "fn c() {}")]);
        assert_eq!(files[1].status, Status::Added);
        assert_eq!(files[1].new_ranges(), vec![(1, 2)]);
        assert!(files[1].raw.starts_with("diff --git a/new.txt"));
    }

    #[test]
    fn hunk_header_without_len() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1, 1, 1)));
        assert_eq!(parse_hunk_header("@@ -10,0 +11,3 @@ fn x"), Some((10, 0, 11, 3)));
    }
}
