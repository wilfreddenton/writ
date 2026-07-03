use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter as TSHighlighter,
};

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

struct LanguageConfig {
    config: HighlightConfiguration,
}

pub struct Highlighter {
    inner: TSHighlighter,
    languages: HashMap<String, Arc<LanguageConfig>>,
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

        // Register Rust
        if let Some(config) = Self::create_rust_config() {
            let config = Arc::new(config);
            languages.insert("rust".to_string(), Arc::clone(&config));
            languages.insert("rs".to_string(), Arc::clone(&config));
        }

        // Register Bash
        if let Some(config) = Self::create_bash_config() {
            let config = Arc::new(config);
            languages.insert("bash".to_string(), Arc::clone(&config));
            languages.insert("sh".to_string(), Arc::clone(&config));
            languages.insert("shell".to_string(), Arc::clone(&config));
        }

        Self { inner, languages }
    }

    fn create_rust_config() -> Option<LanguageConfig> {
        let language = tree_sitter_rust::LANGUAGE.into();

        // Use Zed's highlights.scm for better Rust coverage
        let highlights_query = include_str!("../queries/rust/highlights.scm");

        // Create highlight configuration
        let mut config = match HighlightConfiguration::new(
            language,
            "rust",
            highlights_query,
            "", // injection query (not used)
            "", // locals query (not used for now)
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to create Rust highlight config: {}", e);
                return None;
            }
        };

        // Configure which highlight names we recognize
        config.configure(HIGHLIGHT_NAMES);

        Some(LanguageConfig { config })
    }

    fn create_bash_config() -> Option<LanguageConfig> {
        let language = tree_sitter_bash::LANGUAGE.into();
        let highlights_query = tree_sitter_bash::HIGHLIGHT_QUERY;

        let mut config =
            match HighlightConfiguration::new(language, "bash", highlights_query, "", "") {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to create Bash highlight config: {}", e);
                    return None;
                }
            };

        config.configure(HIGHLIGHT_NAMES);

        Some(LanguageConfig { config })
    }

    pub fn supports_language(&self, lang: &str) -> bool {
        self.languages.contains_key(normalized_lang(lang).as_ref())
    }

    pub fn highlight(&mut self, code: &str, language: &str) -> Vec<HighlightSpan> {
        let Some(lang_config) = self.languages.get(normalized_lang(language).as_ref()) else {
            return Vec::new();
        };

        // Run the highlighter
        let highlights = match self.inner.highlight(
            &lang_config.config,
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
                                if last.highlight_id == highlight_id
                                    && last.range.end == start =>
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
