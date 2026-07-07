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

/// Structural words shared across diagram types → `@keyword`, plus every diagram-type
/// header (so the first word always colors). Stored lowercase; matched case-insensitively.
/// Type-SPECIFIC vocabularies live in [`mermaid_type_keywords`] so a word like gitGraph's
/// `order` or requirement's `contains` can't false-positive on a flowchart node id.
const MERMAID_COMMON: &[&str] = &[
    // diagram-type headers
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
    "requirement",
    "requirementdiagram",
    "c4context",
    "c4container",
    "c4component",
    "c4dynamic",
    "c4deployment",
    "sankey",
    "xychart",
    "block",
    "packet",
    "architecture",
    "radar",
    "kanban",
    "treemap",
    "zenuml",
    "info",
    // structural leads common to many types
    "subgraph",
    "end",
    "direction",
    "click",
    "style",
    "classdef",
    "linkstyle",
    "callback",
    "href",
    "acctitle",
    "accdescr",
    "title",
    "section",
];

/// Diagram directions → `@constant` (distinct from keywords). Only honored in flowchart/graph
/// so a bare `lr`/`bt` node id elsewhere isn't miscolored.
const MERMAID_DIRECTIONS: &[&str] = &["tb", "td", "bt", "rl", "lr"];

/// Keyword vocabulary specific to a diagram type — applied ON TOP of [`MERMAID_COMMON`] only
/// when the block's header matches, so cross-type collisions (a flowchart node named `commit`
/// or `system`) don't light up. Keyed by the lowercased first header word.
fn mermaid_type_keywords(header: &str) -> &'static [&'static str] {
    match header {
        "sequencediagram" => &[
            "participant",
            "actor",
            "as",
            "activate",
            "deactivate",
            "note",
            "over",
            "left",
            "right",
            "of",
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
            "box",
            "create",
            "destroy",
            "links",
            "properties",
            "details",
            "autonumber",
        ],
        "classdiagram" => &["class", "namespace", "cssclass", "note", "link"],
        "statediagram" => &[
            "state",
            "note",
            "hide",
            "empty",
            "description",
            "fork",
            "join",
            "choice",
            "as",
            "direction",
        ],
        "erdiagram" => &["pk", "fk", "uk"],
        "gantt" => &[
            "dateformat",
            "axisformat",
            "excludes",
            "includes",
            "todaymarker",
            "inclusiveenddates",
            "topaxis",
            "tickinterval",
            "weekday",
            "weekend",
            "done",
            "active",
            "crit",
            "milestone",
            "after",
            "until",
        ],
        "pie" => &["showdata"],
        "gitgraph" => &[
            "commit",
            "branch",
            "merge",
            "checkout",
            "switch",
            "cherry",
            "pick",
            "order",
            "tag",
            "type",
            "id",
            "reverse",
            "normal",
            "highlight",
        ],
        "requirement" | "requirementdiagram" => &[
            "requirement",
            "functionalrequirement",
            "performancerequirement",
            "interfacerequirement",
            "physicalrequirement",
            "designconstraint",
            "element",
            "satisfies",
            "traces",
            "contains",
            "copies",
            "derives",
            "refines",
            "verifies",
            "risk",
            "verifymethod",
        ],
        "c4context" | "c4container" | "c4component" | "c4dynamic" | "c4deployment" => &[
            "person",
            "person_ext",
            "system",
            "system_ext",
            "systemdb",
            "systemqueue",
            "container",
            "containerdb",
            "containerqueue",
            "component",
            "rel",
            "birel",
            "rel_u",
            "rel_d",
            "rel_l",
            "rel_r",
            "boundary",
            "enterprise_boundary",
            "system_boundary",
            "container_boundary",
            "node",
            "deployment_node",
        ],
        "quadrantchart" => &["quadrant", "axis"],
        "xychart" => &["bar", "line", "axis"],
        "block" => &["columns", "space"],
        "mindmap" => &["icon"],
        _ => &[],
    }
}

/// The diagram type = the first alphabetic word, skipping a leading `---` front-matter block,
/// `%%` directive/comment lines, and blank lines. Lowercased; empty if none found.
fn mermaid_header(src: &str) -> String {
    let mut in_frontmatter = false;
    for line in src.lines() {
        let t = line.trim();
        if t == "---" {
            in_frontmatter = !in_frontmatter;
            continue;
        }
        if in_frontmatter || t.is_empty() || t.starts_with("%%") {
            continue;
        }
        let word: String = t
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        return word.to_ascii_lowercase();
    }
    String::new()
}

/// Characters that make up flow/sequence/edge operators (`-->`, `-.->`, `==>`, `..>`,
/// `<|--`, `--)`, …). Letters `x`/`o` (arrowheads) are excluded from the run so it never eats
/// an adjacent identifier letter (e.g. the `oo` in `foo-->bar`) — a trailing `x`/`o` head is
/// re-attached explicitly below. `|` is excluded so an edge label `-->|yes|` keeps the arrow
/// and the label as separate tokens.
fn is_arrow_char(c: u8) -> bool {
    matches!(c, b'-' | b'.' | b'=' | b'<' | b'>' | b'~' | b'*')
}

/// Tokenize Mermaid source. Colors comments (`%%`), quoted strings, edge labels, arrows,
/// `<<stereotypes>>`, `[*]` state markers, `:::class` operators, keywords (type-aware),
/// directions, and numbers. Keyword matching is suppressed inside node-shape brackets
/// (`[...]`/`(...)`/`{...}`) so label text like `A[state of the art]` stays plain. Node ids,
/// brackets, and pipes fall through as foreground.
pub fn highlight_mermaid(src: &str) -> Vec<HighlightSpan> {
    let kw = highlight_id("keyword");
    let constant = highlight_id("constant");
    let op = highlight_id("operator");
    let comment = highlight_id("comment");
    let string = highlight_id("string");
    let number = highlight_id("number");
    let type_id = highlight_id("type");

    let header = mermaid_header(src);
    let type_kw = mermaid_type_keywords(&header);
    let is_flowchart = matches!(header.as_str(), "flowchart" | "graph");

    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    // Node-shape bracket nesting; keyword/arrow/number matching is suppressed while > 0 so
    // a label's inner text stays plain. Reset at each newline (a label can't span lines) to
    // stay robust against unbalanced brackets during editing.
    let mut depth: i32 = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            depth = 0;
            i += 1;
            continue;
        }
        // `%%` comment to end of line — also covers `%%{ init: … }%%` directive lines.
        if c == b'%' && i + 1 < b.len() && b[i + 1] == b'%' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            spans.push(span(start..i, comment));
            continue;
        }
        // Quoted string (colored even inside a label).
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
        // `[*]` state start/end marker → constant (before generic bracket handling).
        if c == b'[' && b[i + 1..].starts_with(b"*]") {
            spans.push(span(i..i + 3, constant));
            i += 3;
            continue;
        }
        // Node-shape brackets: track depth (contents plain); the bracket glyphs render fg.
        if matches!(c, b'[' | b'(' | b'{') {
            depth += 1;
            i += 1;
            continue;
        }
        if matches!(c, b']' | b')' | b'}') {
            depth = (depth - 1).max(0);
            i += 1;
            continue;
        }
        // Inside a label everything but a quoted string is plain text.
        if depth > 0 {
            i += 1;
            continue;
        }
        // `<<stereotype>>` (class/state) → type; also stops the trailing `>>` from reading
        // as an operator.
        if c == b'<' && b[i + 1..].starts_with(b"<") {
            let start = i;
            let mut j = i + 2;
            while j + 1 < b.len() && !(b[j] == b'>' && b[j + 1] == b'>') && b[j] != b'\n' {
                j += 1;
            }
            if j + 1 < b.len() && b[j] == b'>' && b[j + 1] == b'>' {
                spans.push(span(start..j + 2, type_id));
                i = j + 2;
                continue;
            }
        }
        // `:::className` class-apply operator (flowchart) → operator + the class as type.
        if c == b':' && b[i + 1..].starts_with(b"::") {
            spans.push(span(i..i + 3, op));
            i += 3;
            let ns = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if ns < i {
                spans.push(span(ns..i, type_id));
            }
            continue;
        }
        // Edge label `|text|` (e.g. `A -->|yes| B`) → string, distinct from the arrow before
        // it. Requires a non-empty `|…|` on the line; a bare `|` falls through as plain.
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
        // Edge/arrow operator: a run of arrow chars, plus an optional trailing `x`/`o`
        // arrowhead (`--x`, `--o`) when it's not the start of an identifier.
        if is_arrow_char(c) {
            let start = i;
            while i < b.len() && is_arrow_char(b[i]) {
                i += 1;
            }
            let connector = b[start..i].iter().any(|&x| matches!(x, b'-' | b'=' | b'>'));
            if connector
                && i < b.len()
                && matches!(b[i], b'x' | b'o')
                && !(i + 1 < b.len() && (b[i + 1].is_ascii_alphanumeric() || b[i + 1] == b'_'))
            {
                i += 1; // re-attach the arrowhead
            }
            if i - start >= 2 && connector {
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
        // Word: direction (flowchart only) / keyword (common + type) / plain identifier.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = src[start..i].to_ascii_lowercase();
            if is_flowchart && MERMAID_DIRECTIONS.contains(&word.as_str()) {
                spans.push(span(start..i, constant));
            } else if MERMAID_COMMON.contains(&word.as_str()) || type_kw.contains(&word.as_str()) {
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
    fn mermaid_keywords_suppressed_inside_labels() {
        // `end`/`state` are keywords at statement level but must stay plain inside a label.
        let src = "flowchart TD\n  A[end of state] --> B";
        let c = cats(src, &highlight_mermaid(src));
        assert_eq!(cat_of(&c, "end"), None);
        assert_eq!(cat_of(&c, "state"), None);
        assert_eq!(cat_of(&c, "-->"), Some("operator"));
    }

    #[test]
    fn mermaid_type_aware_keywords() {
        // `commit` is a gitGraph keyword...
        let git = "gitGraph\n  commit\n  branch dev";
        let c = cats(git, &highlight_mermaid(git));
        assert_eq!(cat_of(&c, "commit"), Some("keyword"));
        assert_eq!(cat_of(&c, "branch"), Some("keyword"));
        // ...but a flowchart node id named `commit` must NOT color as a keyword.
        let flow = "flowchart LR\n  commit --> push";
        let c = cats(flow, &highlight_mermaid(flow));
        assert_eq!(cat_of(&c, "commit"), None);
    }

    #[test]
    fn mermaid_stereotype_marker_and_arrowhead() {
        let src = "stateDiagram-v2\n  [*] --> S\n  S --x T\n  note <<fork>>";
        let c = cats(src, &highlight_mermaid(src));
        assert_eq!(cat_of(&c, "[*]"), Some("constant"));
        assert_eq!(cat_of(&c, "--x"), Some("operator")); // arrowhead re-attached
        assert_eq!(cat_of(&c, "<<fork>>"), Some("type"));
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
