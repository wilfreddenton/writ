//! Hand-written syntax-highlight tokenizers for languages that have no publishable
//! tree-sitter grammar crate: Mermaid diagram source and LaTeX math. Each is a small
//! lexical scanner (keyword-match + punctuation/arrow patterns) that emits `HighlightSpan`s
//! using the shared capture categories (see [`highlight_id`]), so a `Highlighter` treats
//! them as ordinary registered languages and they render through the same theme color map
//! as the tree-sitter grammars. Token→category choices follow the consensus of the
//! canonical tree-sitter grammars (monaqa/tree-sitter-mermaid, latex-lsp/tree-sitter-latex)
//! and their Helix/nvim overrides.
//!
//! Byte scanning is UTF-8-safe here because every span is emitted over an ASCII run
//! (keywords, arrows, commands, numbers); non-ASCII bytes only ever fall through as plain
//! text, so no span can land on a char interior.

use crate::highlight::{HighlightSpan, highlight_id};

fn span(range: std::ops::Range<usize>, highlight_id: usize) -> HighlightSpan {
    HighlightSpan {
        range,
        highlight_id,
    }
}

// --- Mermaid ---------------------------------------------------------------------------

/// Diagram-type + structural statement words → `@keyword`. Matched case-insensitively
/// (the grammar itself lexes several of these case-insensitively). Stored lowercase.
const MERMAID_KEYWORDS: &[&str] = &[
    // diagram types
    "graph",
    "flowchart",
    "sequencediagram",
    "classdiagram",
    "statediagram",
    "erdiagram",
    "gantt",
    "pie",
    "journey",
    "gitgraph",
    "mindmap",
    "timeline",
    "quadrantchart",
    "requirementdiagram",
    // structural / statement leads
    "subgraph",
    "end",
    "participant",
    "actor",
    "as",
    "activate",
    "deactivate",
    "note",
    "over",
    "loop",
    "alt",
    "else",
    "opt",
    "par",
    "and",
    "rect",
    "break",
    "critical",
    "option",
    "class",
    "state",
    "section",
    "title",
    "direction",
    "acctitle",
    "accdescr",
    // gantt
    "dateformat",
    "axisformat",
    "excludes",
    "includes",
    "todaymarker",
    "inclusiveenddates",
    "topaxis",
    "tickinterval",
    // flowchart directives / git
    "click",
    "style",
    "linkstyle",
    "classdef",
    "callback",
    "link",
    "href",
    "commit",
    "branch",
    "checkout",
    "merge",
    "autonumber",
    // ER key modifiers
    "pk",
    "fk",
    "uk",
];

/// Flowchart directions → `@constant` (unanimous across grammars; distinct from keywords).
const MERMAID_DIRECTIONS: &[&str] = &["tb", "td", "bt", "rl", "lr"];

/// Characters that make up flow/sequence/edge operators (`-->`, `-.->`, `==>`, `..>`,
/// `--)`, …). Letters `x`/`o` (arrowheads) are excluded so a run never eats an adjacent
/// identifier letter (e.g. the `oo` in `foo-->bar`); `|` is excluded so an edge label
/// `-->|yes|` keeps the arrow (`-->`) and the label (`|yes|`) as separate tokens.
fn is_arrow_char(c: u8) -> bool {
    matches!(c, b'-' | b'.' | b'=' | b'<' | b'>' | b'~' | b'*')
}

/// Tokenize Mermaid source. Colors comments (`%%`), quoted strings, edge/arrow operators,
/// diagram/structural keywords, directions, and numbers; identifiers, node text, brackets,
/// and label pipes fall through as plain foreground (matching how those captures resolve in
/// the theme). Best-effort — a keyword word inside a bracketed label may color, which is an
/// acceptable cosmetic edge for edit-time source reveal.
pub fn highlight_mermaid(src: &str) -> Vec<HighlightSpan> {
    let kw = highlight_id("keyword");
    let constant = highlight_id("constant");
    let op = highlight_id("operator");
    let comment = highlight_id("comment");
    let string = highlight_id("string");
    let number = highlight_id("number");

    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // `%%` comment to end of line — also covers `%%{ init: … }%%` directive lines.
        if c == b'%' && i + 1 < b.len() && b[i + 1] == b'%' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            spans.push(span(start..i, comment));
            continue;
        }
        // Quoted string label.
        if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += 1;
            }
            if i < b.len() && b[i] == b'"' {
                i += 1;
            }
            spans.push(span(start..i, string));
            continue;
        }
        // Edge label `|text|` (e.g. `A -->|yes| B`): the label reads as a string, distinct
        // from the arrow before it. Requires a non-empty `|…|` on the same line — a bare `|`
        // (or ER `||` cardinality) falls through as plain punctuation.
        if c == b'|' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'|' && b[j] != b'\n' {
                j += 1;
            }
            if j < b.len() && b[j] == b'|' && j > i + 1 {
                spans.push(span(i..j + 1, string));
                i = j + 1;
                continue;
            }
            i += 1;
            continue;
        }
        // Edge/arrow operator: a run of arrow chars that actually forms a connector.
        if is_arrow_char(c) {
            let start = i;
            while i < b.len() && is_arrow_char(b[i]) {
                i += 1;
            }
            let run = &b[start..i];
            if run.len() >= 2 && run.iter().any(|&x| matches!(x, b'-' | b'=' | b'>')) {
                spans.push(span(start..i, op));
            }
            continue;
        }
        // Number (pie values, gantt data).
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            spans.push(span(start..i, number));
            continue;
        }
        // Word: keyword / direction / plain identifier.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = src[start..i].to_ascii_lowercase();
            if MERMAID_DIRECTIONS.contains(&word.as_str()) {
                spans.push(span(start..i, constant));
            } else if MERMAID_KEYWORDS.contains(&word.as_str()) {
                spans.push(span(start..i, kw));
            }
            continue;
        }
        i += 1;
    }
    spans
}

// --- LaTeX (math mode) -----------------------------------------------------------------

/// Tokenize LaTeX (math) source. Control words (`\frac`, `\alpha`, `\int`) → `@function`;
/// `\begin`/`\end` → `@keyword` with the environment name (`matrix`) → `@type`; control
/// symbols (`\\`, `\{`, `\,`) → `@punctuation.special`; sub/superscript `^`/`_` and the math
/// operators `+ - = < > / *` → `@operator`; numbers → `@number`; `%` comments → `@comment`.
/// Braces and letters fall through as plain foreground. (All commands share one accent color
/// — the symbol-vs-function-macro split some editors add needs a hardcoded macro list and is
/// deliberately skipped for uniformity.)
pub fn highlight_latex(src: &str) -> Vec<HighlightSpan> {
    let func = highlight_id("function");
    let keyword = highlight_id("keyword");
    let type_id = highlight_id("type");
    let punct_special = highlight_id("punctuation.special");
    let op = highlight_id("operator");
    let number = highlight_id("number");
    let comment = highlight_id("comment");

    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // `%` comment to end of line (unescaped — an escaped `\%` is consumed as a control
        // symbol below before we reach the `%`).
        if c == b'%' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            spans.push(span(start..i, comment));
            continue;
        }
        // Control sequence: `\` + letters (control word) or `\` + one non-letter (symbol).
        if c == b'\\' {
            let start = i;
            i += 1;
            if i < b.len() && b[i].is_ascii_alphabetic() {
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let name = &src[start + 1..i];
                if name == "begin" || name == "end" {
                    spans.push(span(start..i, keyword));
                    // The environment name in the following `{…}` is a name role, not text.
                    if i < b.len() && b[i] == b'{' {
                        let name_start = i + 1;
                        let mut j = name_start;
                        while j < b.len() && b[j] != b'}' && b[j] != b'\n' {
                            j += 1;
                        }
                        if name_start < j {
                            spans.push(span(name_start..j, type_id));
                        }
                        i = j; // leave the `}` for the default path
                    }
                } else {
                    spans.push(span(start..i, func));
                }
            } else if i < b.len() {
                i += 1; // control symbol: `\\`, `\{`, `\,`, `\;`, …
                spans.push(span(start..i, punct_special));
            }
            continue;
        }
        // Sub/superscript + binary math operators.
        if matches!(
            c,
            b'^' | b'_' | b'+' | b'-' | b'=' | b'<' | b'>' | b'/' | b'*'
        ) {
            spans.push(span(i..i + 1, op));
            i += 1;
            continue;
        }
        // Number.
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            spans.push(span(start..i, number));
            continue;
        }
        i += 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;

    /// Map each token's text to its capture category, for readable assertions.
    fn cats(src: &str, spans: &[HighlightSpan]) -> Vec<(String, &'static str)> {
        spans
            .iter()
            .map(|s| {
                (
                    src[s.range.clone()].to_string(),
                    Highlighter::capture_name(s.highlight_id),
                )
            })
            .collect()
    }

    fn cat_of<'a>(cats: &'a [(String, &'static str)], text: &str) -> Option<&'a str> {
        cats.iter().find(|(t, _)| t == text).map(|(_, c)| *c)
    }

    #[test]
    fn mermaid_categories() {
        let src = "flowchart LR\n  A[Start] --> B{X}\n  B -->|yes| C\n  %% note\n  pie \"a\" : 42";
        let c = cats(src, &highlight_mermaid(src));
        assert_eq!(cat_of(&c, "flowchart"), Some("keyword"));
        assert_eq!(cat_of(&c, "LR"), Some("constant"));
        assert_eq!(cat_of(&c, "-->"), Some("operator"));
        // The edge label `|yes|` is a string, kept separate from the arrow `-->`.
        assert_eq!(cat_of(&c, "|yes|"), Some("string"));
        assert_eq!(cat_of(&c, "pie"), Some("keyword"));
        assert_eq!(cat_of(&c, "\"a\""), Some("string"));
        assert_eq!(cat_of(&c, "42"), Some("number"));
        // The `%%` comment runs to end of line.
        assert!(c.iter().any(|(t, cat)| t == "%% note" && *cat == "comment"));
        // A plain node id emits no span (renders foreground).
        assert_eq!(cat_of(&c, "Start"), None);
    }

    #[test]
    fn mermaid_arrow_does_not_eat_identifiers() {
        // The `o` in `foo`/`bar` is not an arrow char, so only `-->` is the operator.
        let src = "foo-->bar";
        let c = cats(src, &highlight_mermaid(src));
        assert_eq!(c, vec![("-->".to_string(), "operator")]);
    }

    #[test]
    fn latex_categories() {
        let src = "\\frac{1}{2} + x^2 = \\alpha \\\\ \\begin{matrix} % c";
        let c = cats(src, &highlight_latex(src));
        assert_eq!(cat_of(&c, "\\frac"), Some("function"));
        assert_eq!(cat_of(&c, "\\alpha"), Some("function"));
        assert_eq!(cat_of(&c, "^"), Some("operator"));
        assert_eq!(cat_of(&c, "+"), Some("operator"));
        assert_eq!(cat_of(&c, "="), Some("operator"));
        assert_eq!(cat_of(&c, "1"), Some("number"));
        assert_eq!(cat_of(&c, "\\\\"), Some("punctuation.special"));
        assert_eq!(cat_of(&c, "\\begin"), Some("keyword"));
        assert_eq!(cat_of(&c, "matrix"), Some("type"));
        assert!(c.iter().any(|(t, cat)| t == "% c" && *cat == "comment"));
    }

    #[test]
    fn registered_through_highlighter() {
        let mut h = Highlighter::new();
        assert!(h.supports_language("mermaid"));
        assert!(h.supports_language("latex"));
        assert!(h.supports_language("tex"));
        assert!(!h.highlight("flowchart TD", "mermaid").is_empty());
        assert!(!h.highlight("\\frac{1}{2}", "latex").is_empty());
    }
}
