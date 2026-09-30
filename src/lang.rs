//! Language layer: detection, tree-sitter definition extraction, import
//! extraction and identifier harvesting. Everything degrades gracefully to
//! "unknown language" so unsupported files still get reviewed.

use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "rs" => Lang::Rust,
            "go" => Lang::Go,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Rust => "rust",
            Lang::Go => "go",
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Node kinds that introduce a named definition, and the field holding the name.
    fn def_kinds(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &[
                ("function_declaration", "name"),
                ("generator_function_declaration", "name"),
                ("class_declaration", "name"),
                ("abstract_class_declaration", "name"),
                ("method_definition", "name"),
                ("method_signature", "name"),
                ("variable_declarator", "name"),
                ("interface_declaration", "name"),
                ("type_alias_declaration", "name"),
                ("enum_declaration", "name"),
                ("internal_module", "name"),
            ],
            Lang::Python => &[("function_definition", "name"), ("class_definition", "name")],
            Lang::Rust => &[
                ("function_item", "name"),
                ("function_signature_item", "name"),
                ("struct_item", "name"),
                ("enum_item", "name"),
                ("union_item", "name"),
                ("trait_item", "name"),
                ("impl_item", "type"),
                ("type_item", "name"),
                ("const_item", "name"),
                ("static_item", "name"),
                ("mod_item", "name"),
                ("macro_definition", "name"),
            ],
            Lang::Go => &[
                ("function_declaration", "name"),
                ("method_declaration", "name"),
                ("type_spec", "name"),
                ("const_spec", "name"),
                ("var_spec", "name"),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub name: String,
    pub kind: String,
    /// 1-based inclusive.
    pub start_line: u32,
    pub end_line: u32,
}

impl Definition {
    pub fn overlaps(&self, ranges: &[(u32, u32)]) -> bool {
        ranges.iter().any(|&(s, e)| self.start_line <= e && self.end_line >= s)
    }
}

/// Extract named definitions with tree-sitter. Returns an empty list if the
/// language is unsupported or parsing fails; never errors.
pub fn definitions(lang: Lang, src: &str) -> Vec<Definition> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(src, None) else { return Vec::new() };
    let mut out = Vec::new();
    let kinds = lang.def_kinds();
    walk(tree.root_node(), src.as_bytes(), lang, kinds, &mut out);
    out
}

fn walk(node: Node, src: &[u8], lang: Lang, kinds: &[(&str, &str)], out: &mut Vec<Definition>) {
    let kind = node.kind();
    if let Some((_, field)) = kinds.iter().find(|(k, _)| *k == kind)
        && let Some(def) = definition_from(node, field, src, lang)
    {
        out.push(def);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, lang, kinds, out);
    }
}

fn definition_from(node: Node, field: &str, src: &[u8], lang: Lang) -> Option<Definition> {
    let kind = node.kind();
    // JS/TS `const x = ...`: only count function-valued or top-level declarators.
    if kind == "variable_declarator" {
        let value_kind = node.child_by_field_name("value").map(|v| v.kind()).unwrap_or("");
        let is_fn = matches!(value_kind, "arrow_function" | "function_expression" | "function" | "generator_function");
        let top_level = node
            .parent()
            .and_then(|p| p.parent())
            .map(|gp| gp.kind() == "program" || gp.kind() == "export_statement")
            .unwrap_or(false);
        if !is_fn && !top_level {
            return None;
        }
    }
    let name_node = node.child_by_field_name(field)?;
    let mut name = name_node.utf8_text(src).ok()?.trim().to_string();
    if lang == Lang::Rust
        && kind == "impl_item"
        && let Some(t) = node.child_by_field_name("trait")
        && let Ok(tn) = t.utf8_text(src)
    {
        name = format!("{tn} for {name}");
    }
    // Strip generics from impl targets like `Foo<T>`.
    if let Some(i) = name.find('<') {
        name.truncate(i);
    }
    if name.is_empty() {
        return None;
    }
    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    Some(Definition { name, kind: kind.to_string(), start_line: start, end_line: end })
}

static RE_JS_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)(?:^\s*(?:import|export)\b[^;'"\n]*?\bfrom\s*['"]([^'"]+)['"]|\brequire\(\s*['"]([^'"]+)['"]\s*\)|\bimport\(\s*['"]([^'"]+)['"]\s*\)|^\s*import\s+['"]([^'"]+)['"])"#).unwrap()
});
static RE_PY_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:from\s+([\w\.]+)\s+import\b|import\s+([\w\.]+(?:\s*,\s*[\w\.]+)*))").unwrap()
});
static RE_RS_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:use\s+([\w:]+)|mod\s+(\w+)\s*;)").unwrap());
static RE_GO_IMPORT_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)import\s*\((.*?)\)").unwrap());
static RE_GO_IMPORT_ONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*import\s+(?:\w+\s+)?"([^"]+)""#).unwrap());
static RE_GO_QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());

/// Raw module specifiers imported by a file (unresolved).
pub fn imports(lang: Lang, src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            for c in RE_JS_IMPORT.captures_iter(src) {
                if let Some(m) = c.get(1).or(c.get(2)).or(c.get(3)).or(c.get(4)) {
                    out.push(m.as_str().to_string());
                }
            }
        }
        Lang::Python => {
            for c in RE_PY_IMPORT.captures_iter(src) {
                if let Some(m) = c.get(1) {
                    out.push(m.as_str().to_string());
                } else if let Some(m) = c.get(2) {
                    for part in m.as_str().split(',') {
                        out.push(part.trim().to_string());
                    }
                }
            }
        }
        Lang::Rust => {
            for c in RE_RS_IMPORT.captures_iter(src) {
                if let Some(m) = c.get(1) {
                    out.push(m.as_str().to_string());
                } else if let Some(m) = c.get(2) {
                    out.push(format!("mod:{}", m.as_str()));
                }
            }
        }
        Lang::Go => {
            for c in RE_GO_IMPORT_BLOCK.captures_iter(src) {
                for q in RE_GO_QUOTED.captures_iter(&c[1]) {
                    out.push(q[1].to_string());
                }
            }
            for c in RE_GO_IMPORT_ONE.captures_iter(src) {
                out.push(c[1].to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

static RE_IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // keywords across supported languages
        "abstract",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "crate",
        "def",
        "default",
        "defer",
        "del",
        "delete",
        "dyn",
        "elif",
        "else",
        "enum",
        "except",
        "export",
        "extends",
        "extern",
        "false",
        "finally",
        "fn",
        "for",
        "from",
        "func",
        "function",
        "global",
        "goto",
        "if",
        "impl",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "is",
        "lambda",
        "let",
        "loop",
        "match",
        "mod",
        "move",
        "mut",
        "new",
        "nonlocal",
        "not",
        "null",
        "of",
        "or",
        "and",
        "package",
        "pass",
        "private",
        "protected",
        "pub",
        "public",
        "raise",
        "range",
        "readonly",
        "ref",
        "return",
        "select",
        "self",
        "static",
        "struct",
        "super",
        "switch",
        "this",
        "throw",
        "trait",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "unsafe",
        "use",
        "var",
        "void",
        "where",
        "while",
        "with",
        "yield",
        "None",
        "True",
        "False",
        "Self",
        "chan",
        "go",
        "map",
        "declare",
        "namespace",
        "module",
        "keyof",
        "satisfies",
        "override",
        "constructor",
        "get",
        "set",
        "then",
        "any",
        "unknown",
        "never",
        "object",
        "symbol",
        "bigint",
        // primitive and std names that appear everywhere
        "string",
        "number",
        "boolean",
        "int",
        "str",
        "bool",
        "float",
        "usize",
        "isize",
        "u8",
        "u16",
        "u32",
        "u64",
        "i8",
        "i16",
        "i32",
        "i64",
        "f32",
        "f64",
        "char",
        "byte",
        "error",
        "nil",
        "Vec",
        "Option",
        "Result",
        "Some",
        "Ok",
        "Err",
        "Box",
        "String",
        "Promise",
        "Array",
        "Object",
        "Number",
        "Boolean",
        "Error",
        "Date",
        "Map",
        "Set",
        "JSON",
        "Math",
        "console",
        "log",
        "println",
        "print",
        "format",
        "len",
        "push",
        "pop",
        "iter",
        "into",
        "unwrap",
        "clone",
        "collect",
        "expect",
        "list",
        "dict",
        "tuple",
        "isinstance",
        "super",
        "Record",
        "Partial",
        "Pick",
        "Omit",
        "Readonly",
        "Required",
        "Exclude",
        "Extract",
        "NonNullable",
        "ReturnType",
        "Parameters",
        "Awaited",
        "props",
        "state",
        "value",
        "data",
        "item",
        "items",
        "index",
        "result",
        "response",
        "request",
        "context",
        "args",
        "kwargs",
        "options",
        "config",
        "params",
        "name",
        "path",
        "file",
        "text",
        "line",
        "lines",
        "count",
        "size",
        "length",
        "key",
        "keys",
        "values",
        "entries",
        "message",
        "type",
        "kind",
        "status",
        "code",
        "body",
        "header",
        "headers",
        "test",
        "tests",
        "describe",
        "expect",
        "assert",
        "should",
        "mock",
        "before",
        "after",
        "each",
        "useState",
        "useEffect",
        "useMemo",
        "useCallback",
        "useRef",
        "React",
        "Fragment",
        "className",
        "children",
        "process",
        "env",
        "require",
        "exports",
        "__dirname",
        "__filename",
        "setTimeout",
        "setInterval",
        "toString",
        "valueOf",
        "hasOwnProperty",
        "prototype",
        "apply",
        "call",
        "bind",
        "async",
        "fetch",
        "json",
        "as_ref",
        "as_str",
        "to_string",
        "to_owned",
        "from_str",
        "unwrap_or",
        "unwrap_or_else",
        "unwrap_or_default",
        "ok_or",
        "ok_or_else",
        "map_err",
        "and_then",
        "borrow",
        "borrow_mut",
        "as_mut",
        "iter_mut",
        "into_iter",
        "filter",
        "find",
        "reduce",
        "some",
        "every",
        "forEach",
        "includes",
        "indexOf",
        "slice",
        "splice",
        "concat",
        "join",
        "split",
        "trim",
        "replace",
        "starts_with",
        "ends_with",
        "startsWith",
        "endsWith",
        "toLowerCase",
        "toUpperCase",
        "lower",
        "upper",
        "strip",
        "append",
        "extend",
        "insert",
        "remove",
        "contains",
        "sort",
        "sorted",
        "reverse",
        "enumerate",
        "zip",
        "min",
        "max",
        "sum",
        "abs",
        "round",
        "floor",
        "ceil",
        "self_",
        "cls",
    ]
    .into_iter()
    .collect()
});

/// Is this name specific enough to be worth a repo-wide lookup? Filters
/// keywords, primitives, and very short or very common names.
pub fn is_searchable(s: &str) -> bool {
    if s.len() < 3 || STOPWORDS.contains(s) {
        return false;
    }
    let has_upper = s.chars().any(|c| c.is_ascii_uppercase());
    let has_underscore = s.contains('_') && !s.starts_with("__");
    let all_upper = s.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
    // Plain lowercase words need to be longer to be worth a lookup.
    if !has_upper && !has_underscore && s.len() < 6 {
        return false;
    }
    if all_upper && s.len() < 4 {
        return false;
    }
    !s.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// Identifiers in `text` worth looking up elsewhere in the repo.
pub fn interesting_identifiers(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in RE_IDENT.find_iter(text) {
        let s = m.as_str();
        if is_searchable(s) && seen.insert(s.to_string()) {
            out.push(s.to_string());
        }
    }
    out
}

/// Rough def-line check used for files without tree-sitter support.
static RE_DEF_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:fn|def|class|func|struct|enum|trait|interface|type|const|let|var|static|function|mod|impl|macro_rules!|val|object|record|protocol|extension)\s+(?:\([^)]*\)\s*)?(?:mut\s+)?([A-Za-z_]\w*)").unwrap()
});
/// Method-style definitions without a keyword: `  foo(a, b) {` / `  async foo(): T {`.
static RE_DEF_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected|static|async|override|export|default)\s+)*([A-Za-z_]\w*)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;=]+)?\s*\{").unwrap()
});

pub fn line_defines(line: &str, name: &str) -> bool {
    RE_DEF_KEYWORD.captures_iter(line).any(|c| &c[1] == name)
        || RE_DEF_METHOD.captures(line).map(|c| &c[1] == name).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions() {
        let src = "pub struct Foo<T> { x: T }\nimpl<T> Foo<T> {\n    pub fn new(x: T) -> Self { Self { x } }\n}\nfn helper() {}\n";
        let defs = definitions(Lang::Rust, src);
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Foo", "Foo", "new", "helper"]);
        assert_eq!(defs[2].start_line, 3);
    }

    #[test]
    fn typescript_definitions() {
        let src = "export const add = (a: number, b: number) => a + b;\nexport class Svc {\n  run(): void {}\n}\ninterface Shape { w: number }\nfunction main() {}\n";
        let names: Vec<_> = definitions(Lang::TypeScript, src).into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["add", "Svc", "run", "Shape", "main"]);
    }

    #[test]
    fn python_definitions_and_imports() {
        let src = "from app.models import User\nimport os, sys\n\nclass Repo:\n    def get(self, id):\n        return None\n\ndef top():\n    pass\n";
        let names: Vec<_> = definitions(Lang::Python, src).into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["Repo", "get", "top"]);
        assert_eq!(imports(Lang::Python, src), vec!["app.models", "os", "sys"]);
    }

    #[test]
    fn go_definitions() {
        let src = "package x\n\nimport (\n\t\"fmt\"\n\t\"example.com/m/pkg\"\n)\n\ntype Server struct{}\n\nfunc (s *Server) Start() error { return nil }\n\nfunc helper() {}\n";
        let names: Vec<_> = definitions(Lang::Go, src).into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["Server", "Start", "helper"]);
        assert_eq!(imports(Lang::Go, src), vec!["example.com/m/pkg", "fmt"]);
    }

    #[test]
    fn js_imports() {
        let src = "import x from './x';\nimport { y } from \"../lib/y\";\nconst z = require('zlib');\nexport * from './re';\nimport './side';\n";
        assert_eq!(imports(Lang::JavaScript, src), vec!["../lib/y", "./re", "./side", "./x", "zlib"]);
    }

    #[test]
    fn identifiers_filter_noise() {
        let ids = interesting_identifiers("let count = fetchUserProfile(user_id, 42); return Vec::new()");
        assert_eq!(ids, vec!["fetchUserProfile", "user_id"]);
    }

    #[test]
    fn def_line_regex() {
        assert!(line_defines("pub fn parse_hunk(x: &str) {", "parse_hunk"));
        assert!(line_defines("export const foo = () => {", "foo"));
        assert!(line_defines("def load_config(path):", "load_config"));
        assert!(line_defines("func (s *Server) Start() error {", "Start"));
        assert!(!line_defines("    parse_hunk(line);", "parse_hunk"));
        assert!(line_defines("  async handle(req: Request): Promise<void> {", "handle"));
        assert!(!line_defines("  if (foo(x)) {", "foo"));
    }
}
