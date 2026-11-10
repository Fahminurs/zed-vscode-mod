//! Syntax colours for fenced code blocks, using the same tree-sitter grammars and
//! `highlights.scm` queries as Zed. Grammars/queries are built lazily per language.

use std::{cell::RefCell, collections::HashMap, ops::Range};

use gpui::{Hsla, rgb};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

macro_rules! q {
    ($lang:literal) => {
        include_str!(concat!("../../grammars/src/", $lang, "/highlights.scm"))
    };
}

/// Fence info string (```python) → (grammar, highlight query).
fn spec(info: &str) -> Option<(Language, &'static str)> {
    let name = info.split_whitespace().next()?.to_ascii_lowercase();
    Some(match name.as_str() {
        "py" | "python" | "python3" => (tree_sitter_python::LANGUAGE.into(), q!("python")),
        "c" | "h" => (tree_sitter_c::LANGUAGE.into(), q!("c")),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" => {
            (tree_sitter_cpp::LANGUAGE.into(), q!("cpp"))
        }
        "rs" | "rust" => (tree_sitter_rust::LANGUAGE.into(), q!("rust")),
        "js" | "javascript" | "jsx" | "mjs" | "cjs" => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            q!("javascript"),
        ),
        "ts" | "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            q!("typescript"),
        ),
        "tsx" => (tree_sitter_typescript::LANGUAGE_TSX.into(), q!("tsx")),
        "go" | "golang" => (tree_sitter_go::LANGUAGE.into(), q!("go")),
        "sh" | "bash" | "shell" | "zsh" | "console" => {
            (tree_sitter_bash::LANGUAGE.into(), q!("bash"))
        }
        "json" | "jsonc" => (tree_sitter_json::LANGUAGE.into(), q!("json")),
        "css" => (tree_sitter_css::LANGUAGE.into(), q!("css")),
        "yaml" | "yml" => (tree_sitter_yaml::LANGUAGE.into(), q!("yaml")),
        _ => return None,
    })
}

/// VS Code "Dark Modern" token colours, keyed by capture name (most specific first).
fn color_for(capture: &str) -> Option<u32> {
    const TABLE: &[(&str, u32)] = &[
        ("comment", 0x6a9955),
        ("string.escape", 0xd7ba7d),
        ("string.regex", 0xd16969),
        ("string", 0xce9178),
        ("character", 0xce9178),
        ("number", 0xb5cea8),
        ("float", 0xb5cea8),
        ("boolean", 0x569cd6),
        ("constant.builtin", 0x569cd6),
        ("constant", 0x4fc1ff),
        ("keyword.control", 0xc586c0),
        ("keyword.directive", 0xc586c0),
        ("keyword.import", 0xc586c0),
        ("keyword.return", 0xc586c0),
        ("keyword.conditional", 0xc586c0),
        ("keyword.repeat", 0xc586c0),
        ("keyword.exception", 0xc586c0),
        ("preproc", 0xc586c0),
        ("keyword", 0x569cd6),
        ("operator", 0xd4d4d4),
        ("function.builtin", 0xdcdcaa),
        ("function", 0xdcdcaa),
        ("method", 0xdcdcaa),
        ("constructor", 0x4ec9b0),
        ("type.builtin", 0x4ec9b0),
        ("type", 0x4ec9b0),
        ("concept", 0x4ec9b0),
        ("namespace", 0x4ec9b0),
        ("module", 0x4ec9b0),
        ("attribute", 0x9cdcfe),
        ("property", 0x9cdcfe),
        ("label", 0xc8c8c8),
        ("tag", 0x569cd6),
        ("variable.special", 0x569cd6),
        ("variable.builtin", 0x569cd6),
        ("variable.parameter", 0x9cdcfe),
        ("variable", 0x9cdcfe),
        ("punctuation", 0xd4d4d4),
        ("embedded", 0xd4d4d4),
    ];
    TABLE
        .iter()
        .find(|(name, _)| capture == *name || capture.starts_with(&format!("{name}.")))
        .map(|&(_, c)| c)
}

struct Loaded {
    parser: Parser,
    query: Query,
    /// Colour per capture index.
    colors: Vec<Option<Hsla>>,
}

thread_local! {
    // Grammars and queries are only built for languages that actually appear in a note.
    static CACHE: RefCell<HashMap<String, Option<Loaded>>> = RefCell::new(HashMap::new());
}

/// Coloured byte ranges of `code` for the fence `info` string (empty when unknown).
pub fn highlight(info: &str, code: &str) -> Vec<(Range<usize>, Hsla)> {
    let key = info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let entry = cache.entry(key.clone()).or_insert_with(|| {
            let (language, source) = spec(&key)?;
            let mut parser = Parser::new();
            parser.set_language(&language).ok()?;
            let query = Query::new(&language, source).ok()?;
            let colors = query
                .capture_names()
                .iter()
                .map(|n| color_for(n).map(|c| rgb(c).into()))
                .collect();
            Some(Loaded {
                parser,
                query,
                colors,
            })
        });
        let Some(loaded) = entry else {
            return Vec::new();
        };
        let Some(tree) = loaded.parser.parse(code, None) else {
            return Vec::new();
        };
        // Later patterns override earlier ones, inner nodes override outer ones (as in Zed).
        let mut spans: Vec<(Range<usize>, usize, Hsla)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut captures = cursor.captures(&loaded.query, tree.root_node(), code.as_bytes());
        while let Some((m, ix)) = captures.next() {
            let cap = &m.captures[*ix];
            if let Some(color) = loaded.colors[cap.index as usize] {
                spans.push((cap.node.byte_range(), m.pattern_index, color));
            }
        }
        spans.sort_by(|a, b| (b.0.len()).cmp(&a.0.len()).then(a.1.cmp(&b.1)));
        let mut per_byte: Vec<Option<Hsla>> = vec![None; code.len()];
        for (range, _, color) in spans {
            for slot in &mut per_byte[range] {
                *slot = Some(color);
            }
        }
        let mut out: Vec<(Range<usize>, Hsla)> = Vec::new();
        let mut i = 0;
        while i < per_byte.len() {
            let Some(color) = per_byte[i] else {
                i += 1;
                continue;
            };
            let start = i;
            while i < per_byte.len() && per_byte[i] == Some(color) {
                i += 1;
            }
            out.push((start..i, color));
        }
        out
    })
}
