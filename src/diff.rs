//! Inline diff computation for the git HEAD-vs-working view.
//!
//! To render the diff, we:
//! 1. Snapshot the HEAD version of the file (via RenderSnapshot) as the base
//! 2. Compute line-level hunks between the HEAD base and the current buffer
//! 3. For replacement hunks, compute word-level changes for precise highlighting
//! 4. Store DiffState for rendering ghost (deleted) lines and highlighting additions

use crate::buffer::RenderSnapshot;
use imara_diff::{Algorithm, Diff, InternedInput};
use std::ops::Range;

/// A word-level change within a line.
#[derive(Debug, Clone)]
pub struct InlineChange {
    /// Byte range within the line (relative to line start).
    pub range: Range<usize>,
}

/// Per-line inline changes, indexed by line offset within hunk.
/// Empty vec means no word-level changes for that line.
type LineInlineChanges = Vec<Vec<InlineChange>>;

/// A single diff hunk representing a contiguous change.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Line range in the old snapshot (deletions).
    pub old_lines: Range<usize>,
    /// Line range in the current buffer (additions).
    pub new_lines: Range<usize>,
    /// Word-level deletions within old lines, indexed by line offset within hunk.
    pub old_inline_changes: LineInlineChanges,
    /// Word-level additions within new lines, indexed by line offset within hunk.
    pub new_inline_changes: LineInlineChanges,
}

/// State for an active diff review session.
#[derive(Clone)]
pub struct DiffState {
    /// Snapshot of the document before the agent edit.
    /// Used for rendering ghost lines with proper markdown styling.
    pub old_snapshot: RenderSnapshot,
    /// The hunks representing changes.
    pub hunks: Vec<DiffHunk>,
}

impl DiffState {
    /// Compute diff state from old snapshot and new text.
    pub fn compute(old_snapshot: RenderSnapshot, old_text: &str, new_text: &str) -> Self {
        let hunks = compute_line_hunks(old_text, new_text);
        Self {
            old_snapshot,
            hunks,
        }
    }

    /// Check if there are any hunks (i.e. the buffer differs from the HEAD base).
    pub fn has_hunks(&self) -> bool {
        !self.hunks.is_empty()
    }

    /// The hunk whose `new_lines` contains `new_line`, via binary search. Hunks are
    /// ascending and non-overlapping in `new_lines`, so the first hunk ending past
    /// `new_line` is the only possible container.
    fn hunk_by_new_line(&self, new_line: usize) -> Option<&DiffHunk> {
        let i = self.hunks.partition_point(|h| h.new_lines.end <= new_line);
        self.hunks
            .get(i)
            .filter(|h| h.new_lines.contains(&new_line))
    }

    /// The hunk whose `old_lines` contains `old_line`, via binary search (same
    /// ascending, non-overlapping invariant on `old_lines`).
    fn hunk_by_old_line(&self, old_line: usize) -> Option<&DiffHunk> {
        let i = self.hunks.partition_point(|h| h.old_lines.end <= old_line);
        self.hunks
            .get(i)
            .filter(|h| h.old_lines.contains(&old_line))
    }

    /// Get the old line range for a hunk if it should render ghost lines before this new line.
    /// Returns the range of old lines to render as ghosts.
    pub fn ghost_lines_before(&self, new_line: usize) -> Option<Range<usize>> {
        // A deletion hunk has an empty `new_lines`, so several hunks can share the
        // same `new_lines.start`; scan just that tied run for one with deletions.
        let start = self.hunks.partition_point(|h| h.new_lines.start < new_line);
        self.hunks[start..]
            .iter()
            .take_while(|h| h.new_lines.start == new_line)
            .find(|h| !h.old_lines.is_empty())
            .map(|h| h.old_lines.clone())
    }

    /// Check if a line in the new buffer is an addition (part of a hunk's new_lines).
    pub fn is_addition(&self, new_line: usize) -> bool {
        self.hunk_by_new_line(new_line).is_some()
    }

    /// Get inline changes for an old line (ghost line) if any.
    /// Returns byte ranges within the line that were deleted.
    pub fn old_inline_changes(&self, old_line: usize) -> Option<&[InlineChange]> {
        let hunk = self.hunk_by_old_line(old_line)?;
        let line_offset = old_line - hunk.old_lines.start;
        hunk.old_inline_changes
            .get(line_offset)
            .filter(|c| !c.is_empty())
            .map(|c| c.as_slice())
    }

    /// Get inline changes for a new line (addition) if any.
    /// Returns byte ranges within the line that were added.
    pub fn new_inline_changes(&self, new_line: usize) -> Option<&[InlineChange]> {
        let hunk = self.hunk_by_new_line(new_line)?;
        let line_offset = new_line - hunk.new_lines.start;
        hunk.new_inline_changes
            .get(line_offset)
            .filter(|c| !c.is_empty())
            .map(|c| c.as_slice())
    }
}

/// Normalize text for diffing: ensure consistent trailing newline.
/// This prevents spurious diffs on the last line when one text has a trailing
/// newline and the other doesn't.
fn normalize_for_diff(text: &str) -> std::borrow::Cow<'_, str> {
    if text.ends_with('\n') {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(format!("{text}\n"))
    }
}

/// Compute line-level hunks between old and new text, with word-level detail for replacements.
fn compute_line_hunks(old_text: &str, new_text: &str) -> Vec<DiffHunk> {
    // Normalize trailing newlines to avoid spurious diffs on the last line.
    // imara-diff includes newlines as part of each line, so "line3" vs "line3\n"
    // would otherwise show as a change.
    let old_normalized = normalize_for_diff(old_text);
    let new_normalized = normalize_for_diff(new_text);

    let old_lines: Vec<&str> = old_normalized.lines().collect();
    let new_lines: Vec<&str> = new_normalized.lines().collect();

    let input = InternedInput::new(&*old_normalized, &*new_normalized);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    diff.hunks()
        .map(|hunk| {
            let old_range = hunk.before.start as usize..hunk.before.end as usize;
            let new_range = hunk.after.start as usize..hunk.after.end as usize;

            // Compute word-level changes for replacement hunks
            let (old_inline, new_inline) = if !old_range.is_empty() && !new_range.is_empty() {
                compute_word_changes(&old_lines[old_range.clone()], &new_lines[new_range.clone()])
            } else {
                (vec![], vec![])
            };

            DiffHunk {
                old_lines: old_range,
                new_lines: new_range,
                old_inline_changes: old_inline,
                new_inline_changes: new_inline,
            }
        })
        .collect()
}

/// Compute word-level changes between old and new line sets by pairing old line `i` with
/// new line `i` and diffing each pair independently (like GitHub). Cost is bounded per
/// line, so there is no whole-hunk size cap, and words are never misattributed across
/// lines. Extra lines on either side (unequal counts) are wholly added/deleted and need
/// no intra-line emphasis — the row tint already covers them.
fn compute_word_changes(
    old_lines: &[&str],
    new_lines: &[&str],
) -> (LineInlineChanges, LineInlineChanges) {
    let mut old_changes: LineInlineChanges = vec![Vec::new(); old_lines.len()];
    let mut new_changes: LineInlineChanges = vec![Vec::new(); new_lines.len()];
    for i in 0..old_lines.len().min(new_lines.len()) {
        let (dels, adds) = word_diff_line(old_lines[i], new_lines[i]);
        old_changes[i] = dels;
        new_changes[i] = adds;
    }
    (old_changes, new_changes)
}

/// Word-diff two single lines, returning the line-relative changed ranges on each side.
fn word_diff_line(old: &str, new: &str) -> (Vec<InlineChange>, Vec<InlineChange>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    let old_tokens = tokenize_with_positions(old);
    let new_tokens = tokenize_with_positions(new);
    let old_words: Vec<&str> = old_tokens.iter().map(|(s, _)| *s).collect();
    let new_words: Vec<&str> = new_tokens.iter().map(|(s, _)| *s).collect();
    // Diff treating each token as a "line". Tokens never contain a newline (single-line
    // input), so joining by '\n' for the interner is safe.
    let old_joined = old_words.join("\n");
    let new_joined = new_words.join("\n");
    let input = InternedInput::new(old_joined.as_str(), new_joined.as_str());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let mut dels = Vec::new();
    let mut adds = Vec::new();
    for hunk in diff.hunks() {
        for idx in hunk.before.start as usize..hunk.before.end as usize {
            if let Some((_, r)) = old_tokens.get(idx) {
                dels.push(InlineChange { range: r.clone() });
            }
        }
        for idx in hunk.after.start as usize..hunk.after.end as usize {
            if let Some((_, r)) = new_tokens.get(idx) {
                adds.push(InlineChange { range: r.clone() });
            }
        }
    }
    (dels, adds)
}

/// Tokenize a line into a gap-free stream of `(token, byte_range)` covering every byte:
/// maximal runs of word chars (alphanumeric + `_`), whitespace runs, and punctuation
/// runs each form a token. Because no bytes are dropped, punctuation- and whitespace-only
/// edits produce a highlighted token (GitHub does this; the old alnum-only tokenizer left
/// them invisible). `-` is punctuation, so hyphenated parts diff separately.
fn tokenize_with_positions(text: &str) -> Vec<(&str, Range<usize>)> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Punct,
    }
    fn classify(c: char) -> Class {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Punct
        }
    }

    let mut tokens = Vec::new();
    let mut iter = text.char_indices().peekable();
    while let Some(&(start, c)) = iter.peek() {
        let class = classify(c);
        let mut end = start;
        while let Some(&(i, ch)) = iter.peek() {
            if classify(ch) == class {
                end = i + ch.len_utf8();
                iter.next();
            } else {
                break;
            }
        }
        tokens.push((&text[start..end], start..end));
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_line_hunks_addition() {
        let old = "line1\nline2\n";
        let new = "line1\ninserted\nline2\n";
        let hunks = compute_line_hunks(old, new);

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 1..1); // empty range = pure insertion
        assert_eq!(hunks[0].new_lines, 1..2); // one line inserted
    }

    #[test]
    fn test_compute_line_hunks_deletion() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nline3\n";
        let hunks = compute_line_hunks(old, new);

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 1..2); // one line deleted
        assert_eq!(hunks[0].new_lines, 1..1); // empty range = pure deletion
    }

    #[test]
    fn test_compute_line_hunks_modification() {
        let old = "line1\nold content\nline3\n";
        let new = "line1\nnew content\nline3\n";
        let hunks = compute_line_hunks(old, new);

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 1..2); // one line removed
        assert_eq!(hunks[0].new_lines, 1..2); // one line added
    }

    #[test]
    fn test_word_level_changes() {
        let old = "The Histogram algorithm was originally ported from git and has since undergone significant optimization.";
        let new =
            "The Histogram algorithm, originally ported from git, has been extensively optimized.";
        let hunks = compute_line_hunks(&format!("{}\n", old), &format!("{}\n", new));

        assert_eq!(hunks.len(), 1);
        assert!(
            !hunks[0].old_inline_changes.is_empty(),
            "Should have old inline changes"
        );
        assert!(
            !hunks[0].new_inline_changes.is_empty(),
            "Should have new inline changes"
        );

        // Print what we found
        eprintln!("Old inline changes: {:?}", hunks[0].old_inline_changes);
        eprintln!("New inline changes: {:?}", hunks[0].new_inline_changes);
    }

    #[test]
    fn test_identical_lines_no_hunks() {
        let text = "line1\nline2\nline3\n";
        let hunks = compute_line_hunks(text, text);
        assert_eq!(hunks.len(), 0, "Identical text should produce no hunks");
    }

    #[test]
    fn test_trailing_newline_difference() {
        // Text with and without trailing newline - should produce no hunks
        // because we normalize trailing newlines before diffing
        let old = "line1\nline2\nline3";
        let new = "line1\nline2\nline3\n";
        let hunks = compute_line_hunks(old, new);
        assert_eq!(
            hunks.len(),
            0,
            "Trailing newline difference should not produce hunks"
        );
    }

    #[test]
    fn test_trailing_whitespace_difference() {
        // Lines with trailing spaces - this IS a real content change
        let old = "line1\nline2  \nline3\n";
        let new = "line1\nline2\nline3\n";
        let hunks = compute_line_hunks(old, new);
        assert_eq!(hunks.len(), 1, "Trailing whitespace is a real change");
        assert_eq!(hunks[0].old_lines, 1..2);
        assert_eq!(hunks[0].new_lines, 1..2);
    }
}
