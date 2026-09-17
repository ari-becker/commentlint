//! Comment extraction: parses source with tree-sitter, collects comment
//! nodes, merges runs of adjacent line comments, and strips comment syntax so
//! only the prose remains.

use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::languages::Lang;

/// One comment (or run of adjacent line comments) found in a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    /// 1-based line of the first character of the comment.
    pub line: usize,
    /// 1-based column (in bytes) of the first character of the comment.
    pub column: usize,
    /// The comment with its delimiters removed and lines joined by newlines.
    pub text: String,
}

/// A raw comment node before merging and cleaning.
#[derive(Clone, Debug)]
struct RawComment {
    start_row: usize,
    start_col: usize,
    end_row: usize,
    raw: String,
    /// True when this block is a single line comment or a run of them, and
    /// so may absorb the next adjacent line comment.
    line_run: bool,
}

/// Parses `source` as `lang` and returns every comment worth evaluating.
pub fn extract_comments(lang: Lang, source: &str) -> Result<Vec<Comment>> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.grammar())
        .with_context(|| format!("failed to load the {} grammar", lang.name()))?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter returned no syntax tree")?;

    let mut raw = Vec::new();
    collect(tree.root_node(), source.as_bytes(), &mut raw);
    Ok(merge_and_clean(raw))
}

/// Parses the file at `path`, choosing the grammar from its name. Returns
/// `Ok(None)` when the file type is not supported or the file is not UTF-8.
pub fn extract_from_file(path: &Path) -> Result<Option<(Lang, Vec<Comment>)>> {
    let Some(lang) = Lang::from_path(path) else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let Ok(source) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let comments = extract_comments(lang, &source)?;
    Ok(Some((lang, comments)))
}

fn is_comment_kind(kind: &str) -> bool {
    kind.ends_with("comment")
}

fn collect(node: Node<'_>, source: &[u8], out: &mut Vec<RawComment>) {
    if is_comment_kind(node.kind()) {
        let raw = node.utf8_text(source).unwrap_or_default().to_string();
        let start = node.start_position();
        let mut end = node.end_position();
        // Some grammars (Rust doc comments, for one) include the trailing
        // newline in the node, which would make a one-line comment look like
        // it spans two rows.
        if end.column == 0 && end.row > start.row {
            end.row -= 1;
        }
        let raw = raw.trim_end_matches(['\n', '\r']).to_string();
        out.push(RawComment {
            start_row: start.row,
            start_col: start.column,
            end_row: end.row,
            line_run: start.row == end.row && !line_prefix(&raw).is_empty(),
            raw,
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// Merges consecutive single-line comments that start at the same column and
/// use the same delimiter into one block, then strips comment delimiters.
fn merge_and_clean(raw: Vec<RawComment>) -> Vec<Comment> {
    let mut blocks: Vec<RawComment> = Vec::new();
    for c in raw {
        if let Some(prev) = blocks.last_mut()
            && c.line_run
            && prev.line_run
            && prev.end_row + 1 == c.start_row
            && prev.start_col == c.start_col
            && line_prefix(&prev.raw) == line_prefix(&c.raw)
        {
            prev.raw.push('\n');
            prev.raw.push_str(&c.raw);
            prev.end_row = c.end_row;
            continue;
        }
        blocks.push(c);
    }

    blocks
        .into_iter()
        .filter_map(|b| {
            let text = clean(&b.raw);
            if text.is_empty() {
                return None;
            }
            Some(Comment {
                line: b.start_row + 1,
                column: b.start_col + 1,
                text,
            })
        })
        .collect()
}

/// The delimiter that opens a line comment, used so that `//` and `///` runs
/// are not merged together.
fn line_prefix(raw: &str) -> &str {
    const PREFIXES: [&str; 12] = ["////", "//!", "///", "//", "#!", "#", "--", ";;", ";", "%%", "%", "(*"];
    for p in PREFIXES {
        if raw.starts_with(p) {
            return p;
        }
    }
    ""
}

/// Strips comment delimiters and leading decoration from every line.
pub fn clean(raw: &str) -> String {
    const OPENERS: [&str; 22] = [
        "/**", "/*!", "/*", "////", "//!", "///", "//", "<!--", "#!", "#=", "#[[", "#", "--[[", "--", "=begin", ";",
        "%%", "%", "(**", "(*", "{-|", "{-",
    ];
    const CLOSERS: [&str; 7] = ["*/", "-->", "=end", "=#", "]]", "*)", "-}"];

    let mut lines = Vec::new();
    for line in raw.lines() {
        let mut s = line.trim();
        for c in CLOSERS {
            if let Some(rest) = s.strip_suffix(c) {
                s = rest.trim_end();
            }
        }
        let mut stripped = false;
        for o in OPENERS {
            if let Some(rest) = s.strip_prefix(o) {
                s = rest;
                stripped = true;
                break;
            }
        }
        if !stripped {
            // Continuation lines inside block comments are often decorated
            // with a leading asterisk.
            s = s.strip_prefix('*').unwrap_or(s);
        }
        s = s.trim();
        if s == "=end" || s == "=begin" {
            continue;
        }
        lines.push(s.to_string());
    }
    // Drop leading and trailing blank lines; keep interior paragraph breaks.
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_line_comments_merge() {
        let src =
            "// The parser reads the file.\n// It then emits tokens.\nfn main() {}\n\n/// Docs here.\nfn f() {}\n";
        let cs = extract_comments(Lang::Rust, src).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].line, 1);
        assert_eq!(cs[0].column, 1);
        assert_eq!(cs[0].text, "The parser reads the file.\nIt then emits tokens.");
        assert_eq!(cs[1].line, 5);
        assert_eq!(cs[1].text, "Docs here.");
    }

    #[test]
    fn rust_doc_comments_merge() {
        let src = "/// Reads the file.\n/// Emits tokens.\nfn f() {}\n//! Inner one.\n//! Second inner.\n";
        let cs = extract_comments(Lang::Rust, src).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].text, "Reads the file.\nEmits tokens.");
        assert_eq!(cs[1].line, 4);
        assert_eq!(cs[1].text, "Inner one.\nSecond inner.");
    }

    #[test]
    fn block_comment_is_cleaned() {
        let src =
            "int x; /* The value was set by the caller. */\n/**\n * Line one.\n * Line two.\n */\nvoid f(void);\n";
        let cs = extract_comments(Lang::C, src).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].line, 1);
        assert_eq!(cs[0].column, 8);
        assert_eq!(cs[0].text, "The value was set by the caller.");
        assert_eq!(cs[1].text, "Line one.\nLine two.");
    }

    #[test]
    fn python_hash_comments() {
        let src = "#!/usr/bin/env python3\nx = 1  # trailing note\n\n# first\n# second\ny = 2\n";
        let cs = extract_comments(Lang::Python, src).unwrap();
        let texts: Vec<_> = cs.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["/usr/bin/env python3", "trailing note", "first\nsecond"]);
        assert_eq!(cs[1].line, 2);
        assert_eq!(cs[1].column, 8);
    }

    #[test]
    fn ocaml_and_haskell_block_delimiters() {
        let cs = extract_comments(
            Lang::OCaml,
            "(* The lexer reads bytes. *)\nlet x = 1\n(** Doc\n    comment *)\n",
        )
        .unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].text, "The lexer reads bytes.");
        assert_eq!(cs[1].text, "Doc\ncomment");
        let cs = extract_comments(
            Lang::Haskell,
            "{- The parser emits tokens. -}\n-- first\n-- second\nx = 1\n",
        )
        .unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].text, "The parser emits tokens.");
        assert_eq!(cs[1].text, "first\nsecond");
    }

    #[test]
    fn erlang_julia_gleam_zig_swift() {
        let cs = extract_comments(Lang::Erlang, "%% Module doc.\n% Another.\n-module(m).\n").unwrap();
        assert_eq!(
            cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Module doc.", "Another."]
        );
        let cs = extract_comments(Lang::Julia, "#= Block\n   here =#\nx = 1 # tail\n").unwrap();
        assert_eq!(
            cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Block\nhere", "tail"]
        );
        let cs = extract_comments(Lang::Gleam, "//// Module doc.\n/// Fn doc.\npub fn f() { 1 }\n").unwrap();
        assert_eq!(
            cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Module doc.", "Fn doc."]
        );
        let cs = extract_comments(Lang::Zig, "//! Root doc.\n/// Fn doc.\npub fn f() void {}\n").unwrap();
        assert_eq!(
            cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Root doc.", "Fn doc."]
        );
        let cs = extract_comments(Lang::Swift, "/* Block. */\n// Line one.\n// Line two.\nlet x = 1\n").unwrap();
        assert_eq!(
            cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Block.", "Line one.\nLine two."]
        );
    }

    #[test]
    fn every_language_loads_its_grammar() {
        for lang in [
            Lang::Bash,
            Lang::C,
            Lang::CMake,
            Lang::CSharp,
            Lang::Cpp,
            Lang::Css,
            Lang::Dart,
            Lang::Elixir,
            Lang::Elm,
            Lang::Erlang,
            Lang::FSharp,
            Lang::FSharpSignature,
            Lang::Gleam,
            Lang::Go,
            Lang::Haskell,
            Lang::Hcl,
            Lang::Html,
            Lang::Java,
            Lang::JavaScript,
            Lang::Julia,
            Lang::Kotlin,
            Lang::Lua,
            Lang::Nix,
            Lang::ObjC,
            Lang::OCaml,
            Lang::OCamlInterface,
            Lang::Php,
            Lang::Python,
            Lang::R,
            Lang::Ruby,
            Lang::Rust,
            Lang::Scala,
            Lang::Swift,
            Lang::Toml,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::Yaml,
            Lang::Zig,
        ] {
            extract_comments(lang, "").unwrap_or_else(|e| panic!("{}: {e}", lang.name()));
        }
    }

    #[test]
    fn different_columns_do_not_merge() {
        let src = "// a comment here\n    // an indented one\n";
        let cs = extract_comments(Lang::JavaScript, src).unwrap();
        assert_eq!(cs.len(), 2);
    }

    #[test]
    fn unsupported_extension_is_none() {
        assert!(Lang::from_path(Path::new("foo.unknownext")).is_none());
        assert_eq!(Lang::from_path(Path::new("a/b/c.rs")), Some(Lang::Rust));
    }
}
