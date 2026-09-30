//! Prompt assembly. The system prompt sets the reviewer's job and the exact
//! output contract; the user message is the context pack, clearly sectioned
//! so the model knows what changed versus what is merely relevant.

use crate::context::{ContextPack, SnippetKind};

pub const SYSTEM: &str = r#"You are a senior engineer doing a code review. The author is an AI coding agent that will read your review and act on it, so be precise, concrete, and free of pleasantries.

You receive:
1. A unified diff of the change.
2. The full post-change contents of each changed file, with line numbers ("   12| code").
3. Related context from elsewhere in the repository: definitions of symbols the diff uses, call sites of symbols the diff changed, imported modules, and tests. This context did NOT change; it is there so you can judge the change correctly.

Your job: find real problems in the CHANGE. Prioritize, in order:
- Bugs and logic errors: wrong conditions, off-by-one, unhandled cases, broken invariants, incorrect use of an API as defined in the context.
- Regressions: call sites in the context that will now break, changed contracts, behavior that other code relies on.
- Security: injection, unsafe deserialization, auth or permission gaps, secrets, path traversal, SSRF.
- Data loss, concurrency, resource leaks, and error handling that swallows or misreports failures.
- Missing or wrong tests for the new behavior, when tests exist for similar code.
- Performance problems that matter (N+1, quadratic loops over large inputs, blocking calls in async code).

Rules:
- Only report things you are confident are problems given the code in front of you. If you would need to see more code to be sure, say what you would need in the body and lower the severity.
- Do not comment on style, formatting, naming, or comments unless the instructions ask for it or it hides a bug. Never pad the review.
- Cite locations using the repo-relative file path exactly as given and the line number from the numbered post-change listing. For issues in unchanged context, still cite the changed file and line that causes the problem.
- Severity: blocker = must not ship (data loss, security hole, guaranteed crash on a normal path); high = real bug on a realistic path; medium = bug on an edge case or a clear robustness gap; low = worth fixing but not urgent; nit = optional.
- A change with no real problems gets an empty findings list and verdict "approve". That is a good outcome; do not invent findings.
- Respond with a single JSON object and nothing else. No markdown fences, no prose outside the JSON.

Output format (JSON schema):
"#;

/// System prompt with the response schema appended.
pub fn system(schema: &serde_json::Value) -> String {
    let mut s = SYSTEM.to_string();
    s.push_str(&serde_json::to_string(schema).unwrap_or_default());
    s.push_str("\n\nExample of the shape (values are illustrative):\n");
    s.push_str(r#"{"summary":"Adds retry logic to the HTTP client. One real bug in the backoff computation.","verdict":"request_changes","findings":[{"severity":"high","category":"bug","file":"src/client.rs","line":42,"end_line":44,"title":"Backoff overflows after 32 retries","body":"`1 << attempt` is computed in u32; attempt reaches 32 on the last retry and the shift panics in debug builds and wraps to 0 in release, removing the delay entirely.","suggestion":"Use `1u64.checked_shl(attempt).unwrap_or(u64::MAX).min(MAX_BACKOFF)`."}]}"#);
    s
}

pub fn user_message(pack: &ContextPack, instructions: &[String]) -> String {
    let mut s = String::with_capacity(pack.diff_text.len() * 3);
    s.push_str(&format!(
        "<change>\nrepo: {}\ncomparison: {}\nbranch: {}\nhead: {}\nfiles changed: {}\n</change>\n\n",
        pack.repo_root.rsplit('/').next().unwrap_or(&pack.repo_root),
        pack.mode_label,
        pack.branch.as_deref().unwrap_or("(detached)"),
        pack.head,
        pack.files.len()
    ));

    let instructions: Vec<&str> = instructions.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !instructions.is_empty() {
        s.push_str("<instructions>\n");
        for i in instructions {
            s.push_str(i);
            s.push('\n');
        }
        s.push_str("</instructions>\n\n");
    }

    s.push_str("<diff>\n");
    s.push_str(&pack.diff_text);
    if !pack.diff_text.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("</diff>\n\n");

    s.push_str("<changed_files>\n");
    for f in &pack.files {
        let status = serde_json::to_value(f.status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        let lang = f.lang.unwrap_or("text");
        let syms = if f.changed_symbols.is_empty() {
            String::new()
        } else {
            format!(" changed_symbols=\"{}\"", f.changed_symbols.join(","))
        };
        match &f.listing {
            Some(listing) => {
                s.push_str(&format!(
                    "<file path=\"{}\" status=\"{status}\" lang=\"{lang}\" note=\"{}\"{syms}>\n",
                    f.path,
                    f.listing_note.as_deref().unwrap_or("")
                ));
                s.push_str(listing);
                s.push_str("</file>\n");
            }
            None => {
                s.push_str(&format!(
                    "<file path=\"{}\" status=\"{status}\" lang=\"{lang}\" note=\"{}\" />\n",
                    f.path,
                    f.listing_note.as_deref().unwrap_or("no listing")
                ));
            }
        }
    }
    s.push_str("</changed_files>\n\n");

    if !pack.snippets.is_empty() || !pack.importers.is_empty() {
        s.push_str("<related_context note=\"unchanged code, included for reference\">\n");
        for sn in &pack.snippets {
            let kind = match sn.kind {
                SnippetKind::Definition => "definition",
                SnippetKind::CallSite => "call_site",
                SnippetKind::Import => "imported_module",
                SnippetKind::Test => "test",
            };
            s.push_str(&format!(
                "<snippet path=\"{}\" lines=\"{}-{}\" kind=\"{kind}\" reason=\"{}\">\n",
                sn.path, sn.start, sn.end, sn.reason
            ));
            s.push_str(&sn.text);
            s.push_str("</snippet>\n");
        }
        for (path, importers) in &pack.importers {
            s.push_str(&format!("<importers of=\"{path}\">{}</importers>\n", importers.join(", ")));
        }
        s.push_str("</related_context>\n\n");
    }

    s.push_str("Review the change. Respond with JSON only.\n");
    s
}
