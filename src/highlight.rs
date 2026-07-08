use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter as TSHighlighter,
};

use crate::tokenize;

pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "embedded",
    "function",
    "function.definition",
    "function.method",
    "function.special",
    "function.special.definition",
    "keyword",
    "keyword.control",
    "lifetime",
    "number",
    "operator",
    "property",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "type",
    "type.builtin",
    "type.interface",
    "variable",
    "variable.parameter",
    "variable.special",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub highlight_id: usize,
}

/// Resolve a capture name (e.g. `"keyword"`, `"operator"`) to its `highlight_id` — the
/// index into [`HIGHLIGHT_NAMES`] that [`EditorTheme::color_for_highlight`] maps to a
/// color. For the hand-written tokenizers (mermaid, latex), which emit categories by name
/// rather than via a tree-sitter query. An unknown name resolves to a sentinel that renders
/// as the plain foreground.
///
/// [`EditorTheme::color_for_highlight`]: crate::editor::EditorTheme::color_for_highlight
pub fn highlight_id(name: &str) -> usize {
    HIGHLIGHT_NAMES
        .iter()
        .position(|&n| n == name)
        .unwrap_or(usize::MAX)
}

/// Language keys are stored lowercase. Only allocate when the input actually has an
/// uppercase byte — the overwhelmingly common case (already-lowercase fence infos)
/// borrows unchanged.
fn normalized_lang(lang: &str) -> Cow<'_, str> {
    if lang.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(lang.to_lowercase())
    } else {
        Cow::Borrowed(lang)
    }
}

/// How a registered language produces highlight spans. Most languages use a tree-sitter
/// grammar + query; a few (mermaid, latex) have no publishable grammar crate and are
/// lexed by a pure-Rust tokenizer instead. Both go through the same `highlight()` entry
/// point, so callers (the code-block highlight cache, revealed-math styling) never
/// special-case which backend a language uses.
enum Backend {
    /// Boxed — a `HighlightConfiguration` is large, and it lives behind an `Arc` anyway.
    TreeSitter(Box<HighlightConfiguration>),
    /// `fn(source) -> spans`, each span's `highlight_id` from [`highlight_id`].
    Tokenizer(fn(&str) -> Vec<HighlightSpan>),
}

pub struct Highlighter {
    inner: TSHighlighter,
    languages: HashMap<String, Arc<Backend>>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        let inner = TSHighlighter::new();
        let mut languages = HashMap::new();

        let mut register = |aliases: &[&str], backend: Backend| {
            let backend = Arc::new(backend);
            for a in aliases {
                languages.insert((*a).to_string(), Arc::clone(&backend));
            }
        };

        // Tree-sitter grammars. Rust uses Zed's highlights.scm (broader coverage than the
        // upstream query); Bash uses its crate's bundled query.
        let rust = tree_sitter_rust::LANGUAGE.into();
        if let Some(config) =
            Self::create_config(rust, "rust", include_str!("../queries/rust/highlights.scm"))
        {
            register(&["rust", "rs"], Backend::TreeSitter(Box::new(config)));
        }
        let bash = tree_sitter_bash::LANGUAGE.into();
        if let Some(config) = Self::create_config(bash, "bash", tree_sitter_bash::HIGHLIGHT_QUERY) {
            register(
                &["bash", "sh", "shell"],
                Backend::TreeSitter(Box::new(config)),
            );
        }

        // Tokenizer-backed languages (no publishable grammar crate). Registered ungated:
        // a ```mermaid / ```latex code fence highlights through the normal code-block path,
        // and revealed `$…$` math styles via `highlight(content, "latex")`.
        register(
            &["mermaid"],
            Backend::Tokenizer(tokenize::highlight_mermaid),
        );
        register(
            &["latex", "tex"],
            Backend::Tokenizer(tokenize::highlight_latex),
        );

        Self { inner, languages }
    }

    /// Build a tree-sitter highlight config from a grammar + `highlights.scm`, configured to
    /// the shared [`HIGHLIGHT_NAMES`]. `None` (with a log) if the query fails to compile.
    fn create_config(
        language: tree_sitter::Language,
        name: &str,
        highlights_query: &str,
    ) -> Option<HighlightConfiguration> {
        let mut config = match HighlightConfiguration::new(language, name, highlights_query, "", "")
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to create {name} highlight config: {e}");
                return None;
            }
        };
        config.configure(HIGHLIGHT_NAMES);
        Some(config)
    }

    pub fn supports_language(&self, lang: &str) -> bool {
        self.languages.contains_key(normalized_lang(lang).as_ref())
    }

    pub fn highlight(&mut self, code: &str, language: &str) -> Vec<HighlightSpan> {
        let Some(backend) = self.languages.get(normalized_lang(language).as_ref()) else {
            return Vec::new();
        };

        let config = match backend.as_ref() {
            Backend::Tokenizer(f) => return f(code),
            Backend::TreeSitter(config) => config.as_ref(),
        };

        // Run the highlighter
        let highlights = match self.inner.highlight(
            config,
            code.as_bytes(),
            None,     // cancellation flag
            |_| None, // injection callback (not used)
        ) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };

        // Convert events to spans
        let mut spans: Vec<HighlightSpan> = Vec::new();
        let mut current_highlight: Option<usize> = None;

        for event in highlights {
            match event {
                Ok(HighlightEvent::Source { start, end }) => {
                    if let Some(highlight_id) = current_highlight {
                        // Coalesce with the previous span when it carries the same id and
                        // butts right up against this one — tree-sitter emits Source events
                        // token-by-token, so a run of one color arrives as many adjacent spans.
                        match spans.last_mut() {
                            Some(last)
                                if last.highlight_id == highlight_id && last.range.end == start =>
                            {
                                last.range.end = end;
                            }
                            _ => spans.push(HighlightSpan {
                                range: start..end,
                                highlight_id,
                            }),
                        }
                    }
                }
                Ok(HighlightEvent::HighlightStart(Highlight(id))) => {
                    current_highlight = Some(id);
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    current_highlight = None;
                }
                Err(_) => break,
            }
        }

        spans
    }

    pub fn capture_name(highlight_id: usize) -> &'static str {
        HIGHLIGHT_NAMES
            .get(highlight_id)
            .copied()
            .unwrap_or("unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlighter_creation() {
        let highlighter = Highlighter::new();
        assert!(highlighter.supports_language("rust"));
        assert!(highlighter.supports_language("rs"));
        assert!(highlighter.supports_language("Rust")); // case insensitive
        assert!(highlighter.supports_language("bash"));
        assert!(highlighter.supports_language("sh"));
        assert!(highlighter.supports_language("shell"));
        assert!(!highlighter.supports_language("python"));
    }

    #[test]
    fn test_highlight_rust_simple() {
        let mut highlighter = Highlighter::new();
        let code = "let x = 42;";
        let spans = highlighter.highlight(code, "rust");

        // Should have at least some highlights
        assert!(!spans.is_empty(), "Should produce some highlight spans");

        // Print spans for debugging
        for span in &spans {
            eprintln!(
                "  {:?}: {} @ {:?}",
                &code[span.range.clone()],
                Highlighter::capture_name(span.highlight_id),
                span.range
            );
        }

        // Check for keyword "let"
        let let_spans: Vec<_> = spans
            .iter()
            .filter(|s| s.range == (0..3) && Highlighter::capture_name(s.highlight_id) == "keyword")
            .collect();
        assert!(!let_spans.is_empty(), "Should have @keyword for 'let'");

        // Check for number "42"
        let num_spans: Vec<_> = spans
            .iter()
            .filter(|s| s.range == (8..10) && Highlighter::capture_name(s.highlight_id) == "number")
            .collect();
        assert!(!num_spans.is_empty(), "Should have @number for '42'");
    }

    #[test]
    fn test_highlight_rust_function() {
        let mut highlighter = Highlighter::new();
        let code = "fn main() {}";
        let spans = highlighter.highlight(code, "rust");

        // Print spans for debugging
        for span in &spans {
            eprintln!(
                "  {:?}: {} @ {:?}",
                &code[span.range.clone()],
                Highlighter::capture_name(span.highlight_id),
                span.range
            );
        }

        // Check for keyword "fn"
        let fn_spans: Vec<_> = spans
            .iter()
            .filter(|s| s.range == (0..2) && Highlighter::capture_name(s.highlight_id) == "keyword")
            .collect();
        assert!(!fn_spans.is_empty(), "Should have @keyword for 'fn'");

        // Check for function name "main"
        let main_spans: Vec<_> = spans
            .iter()
            .filter(|s| {
                s.range == (3..7)
                    && Highlighter::capture_name(s.highlight_id).starts_with("function")
            })
            .collect();
        assert!(!main_spans.is_empty(), "Should have @function* for 'main'");
    }

    #[test]
    fn test_highlight_no_overlap() {
        let mut highlighter = Highlighter::new();
        let code = "fn foo(x: i32) -> Result<i32, Error> { Ok(x) }";
        let spans = highlighter.highlight(code, "rust");

        // Print spans for debugging
        for span in &spans {
            eprintln!(
                "  {:?}: {} @ {:?}",
                &code[span.range.clone()],
                Highlighter::capture_name(span.highlight_id),
                span.range
            );
        }

        // Verify no overlapping spans
        for (i, span1) in spans.iter().enumerate() {
            for span2 in spans.iter().skip(i + 1) {
                let overlaps =
                    span1.range.start < span2.range.end && span2.range.start < span1.range.end;
                assert!(
                    !overlaps,
                    "Spans should not overlap: {:?} and {:?}",
                    span1, span2
                );
            }
        }
    }
}
