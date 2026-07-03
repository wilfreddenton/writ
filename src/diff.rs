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

    /// Get the old line range for a hunk if it should render ghost lines before this new line.
    /// Returns the range of old lines to render as ghosts.
    pub fn ghost_lines_before(&self, new_line: usize) -> Option<Range<usize>> {
        for hunk in &self.hunks {
            if hunk.new_lines.start == new_line && !hunk.old_lines.is_empty() {
                return Some(hunk.old_lines.clone());
            }
        }
        None
    }

    /// Check if a line in the new buffer is an addition (part of a hunk's new_lines).
    pub fn is_addition(&self, new_line: usize) -> bool {
        self.hunks.iter().any(|h| h.new_lines.contains(&new_line))
    }

    /// Get inline changes for an old line (ghost line) if any.
    /// Returns byte ranges within the line that were deleted.
    pub fn old_inline_changes(&self, old_line: usize) -> Option<&[InlineChange]> {
        for hunk in &self.hunks {
            if hunk.old_lines.contains(&old_line) {
                let line_offset = old_line - hunk.old_lines.start;
                if let Some(changes) = hunk.old_inline_changes.get(line_offset)
                    && !changes.is_empty()
                {
                    return Some(changes);
                }
            }
        }
        None
    }

    /// Get inline changes for a new line (addition) if any.
    /// Returns byte ranges within the line that were added.
    pub fn new_inline_changes(&self, new_line: usize) -> Option<&[InlineChange]> {
        for hunk in &self.hunks {
            if hunk.new_lines.contains(&new_line) {
                let line_offset = new_line - hunk.new_lines.start;
                if let Some(changes) = hunk.new_inline_changes.get(line_offset)
                    && !changes.is_empty()
                {
                    return Some(changes);
                }
            }
        }
        None
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

/// Maximum number of lines in a hunk to compute word-level diffs.
/// Larger hunks are shown as full line additions/deletions for readability.
/// This matches Zed's approach (MAX_WORD_DIFF_LINE_COUNT: 5).
const MAX_WORD_DIFF_LINE_COUNT: usize = 5;

/// Compute word-level changes between old and new line sets.
/// Returns (old_changes, new_changes) for highlighting deleted/added words.
/// Returns empty vecs if the hunk is too large (exceeds MAX_WORD_DIFF_LINE_COUNT).
fn compute_word_changes(
    old_lines: &[&str],
    new_lines: &[&str],
) -> (LineInlineChanges, LineInlineChanges) {
    // Skip word-level diff for large hunks - they'd be overwhelming to read
    let total_lines = old_lines.len() + new_lines.len();
    if total_lines > MAX_WORD_DIFF_LINE_COUNT {
        return (vec![], vec![]);
    }

    // Join lines for word-level diff
    let old_text = old_lines.join("\n");
    let new_text = new_lines.join("\n");

    // Tokenize into words (keeping track of positions)
    let old_tokens = tokenize_with_positions(&old_text);
    let new_tokens = tokenize_with_positions(&new_text);

    if old_tokens.is_empty() || new_tokens.is_empty() {
        return (vec![], vec![]);
    }

    // Build word strings joined by newline for line-based diffing
    let old_words: Vec<&str> = old_tokens.iter().map(|(s, _)| *s).collect();
    let new_words: Vec<&str> = new_tokens.iter().map(|(s, _)| *s).collect();

    let old_joined = old_words.join("\n");
    let new_joined = new_words.join("\n");

    // Diff treating each word as a "line"
    let input = InternedInput::new(old_joined.as_str(), new_joined.as_str());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    // Collect deleted/added word positions
    let mut old_deleted_ranges: Vec<Range<usize>> = vec![];
    let mut new_added_ranges: Vec<Range<usize>> = vec![];

    for hunk in diff.hunks() {
        for word_idx in hunk.before.start as usize..hunk.before.end as usize {
            if let Some((_, range)) = old_tokens.get(word_idx) {
                old_deleted_ranges.push(range.clone());
            }
        }
        for word_idx in hunk.after.start as usize..hunk.after.end as usize {
            if let Some((_, range)) = new_tokens.get(word_idx) {
                new_added_ranges.push(range.clone());
            }
        }
    }

    // Convert absolute positions to per-line positions
    let old_changes = ranges_to_per_line_changes(&old_text, old_lines, &old_deleted_ranges);
    let new_changes = ranges_to_per_line_changes(&new_text, new_lines, &new_added_ranges);

    (old_changes, new_changes)
}

/// Tokenize text into words with their byte positions.
/// Returns Vec<(word, byte_range)>.
fn tokenize_with_positions(text: &str) -> Vec<(&str, Range<usize>)> {
    let mut tokens = vec![];
    let mut start = None;

    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start {
            tokens.push((&text[s..i], s..i));
            start = None;
        }
    }

    // Handle last token
    if let Some(s) = start {
        tokens.push((&text[s..], s..text.len()));
    }

    tokens
}

/// Convert absolute byte ranges to per-line Vec<Vec<InlineChange>>.
/// Returns a dense vec indexed by line offset within the hunk.
fn ranges_to_per_line_changes(
    full_text: &str,
    lines: &[&str],
    ranges: &[Range<usize>],
) -> Vec<Vec<InlineChange>> {
    // Initialize with empty vecs for each line
    let mut per_line: Vec<Vec<InlineChange>> = vec![vec![]; lines.len()];

    if ranges.is_empty() {
        return per_line;
    }

    // Build line start offsets
    let mut line_starts = vec![0usize];
    let mut pos = 0;
    for line in lines {
        pos += line.len() + 1; // +1 for newline
        line_starts.push(pos.min(full_text.len()));
    }

    // Group ranges by line
    for range in ranges {
        // Find which line this range belongs to
        for (line_idx, window) in line_starts.windows(2).enumerate() {
            let line_start = window[0];
            let line_end = window[1];

            if range.start >= line_start && range.start < line_end {
                let relative_range =
                    (range.start - line_start)..(range.end - line_start).min(lines[line_idx].len());

                per_line[line_idx].push(InlineChange {
                    range: relative_range,
                });
                break;
            }
        }
    }

    per_line
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
