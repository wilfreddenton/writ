//! Headless editor model — the gpui-free core (see MIGRATION-PLAN.md, Phase 2).
//!
//! Wraps [`EditorState`] (the pure edit engine: buffer, cursor, movement,
//! insert/delete, enter/tab, checkbox toggle) with the orchestration that used to
//! live on the gpui `Editor` entity but has no rendering dependency: inline git
//! diff against HEAD, GitHub-ref/naked-URL detection, and file load/save. No
//! `gpui::Context`/`Window` in the call graph — the winit shell drives this
//! directly, and unit tests exercise it with no UI at all.
//!
//! Async GitHub *validation* and autocomplete *fetching* (which used `cx.spawn`)
//! are intentionally not ported here yet; they return to the shell on real tokio
//! in a later phase. Detection (the synchronous scan) lives here.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use regex::Regex;
use std::time::SystemTime;

use crate::buffer::{Buffer, RenderSnapshot};
use crate::cursor::Selection;
use crate::diff::DiffState;
use crate::editor::{Direction, EditorState};
use crate::fold;
use crate::git::head_blob_text;
use crate::github::GitHubClient;
#[cfg(feature = "math")]
use crate::inline::detect_inline_math;
use crate::inline::{
    GitHubContext, GitHubRef, MathSpan, NakedUrl, RawGitHubMatch, detect_github_references_in_line,
    detect_naked_urls,
};
use crate::marker::MarkerKind;
use crate::paste::{PasteContext, transform_paste};
use crate::text_input::TextField;
use crate::validation::{GitHubValidationCache, IssueStatus};

/// The kind of autocomplete triggered at the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutocompleteTrigger {
    /// Issue/PR autocomplete triggered by `#`.
    Issue,
    /// User autocomplete triggered by `@`.
    User,
}

/// A single suggestion row. Built on the main thread from fetched GitHub data.
#[derive(Clone)]
pub enum AutocompleteSuggestion {
    IssueOrPr {
        number: u64,
        /// Unicode symbol (● issue, ⎇ PR).
        symbol: String,
        status: IssueStatus,
        title: String,
    },
    User {
        login: String,
        name: Option<String>,
    },
}

/// State for the autocomplete popup while the cursor is inside a `#`/`@` token.
#[derive(Clone)]
pub struct AutocompleteState {
    pub trigger: AutocompleteTrigger,
    /// Byte offset of the trigger char (`#`/`@`) — replacement starts here.
    pub trigger_offset: usize,
    /// The text typed after the trigger (e.g. "123" for `#123`).
    pub prefix: String,
    pub suggestions: Vec<AutocompleteSuggestion>,
    pub selected_index: usize,
    pub loading: bool,
    /// The prefix we last kicked off a fetch for (dedup).
    pub fetched_prefix: Option<String>,
}

/// Whether the find bar shows just the search field (Find) or search + replace (Replace).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FindMode {
    Find,
    Replace,
}

/// Which text field of the find bar currently receives keystrokes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldFocus {
    Search,
    Replace,
}

/// State for the find (and replace) bar. `matches` are buffer byte ranges, sorted
/// left-to-right and non-overlapping (regex `find_iter` semantics); `active` indexes
/// into it. `scanned` records the `(version, query, case, regex)` the current `matches`
/// reflect so a cursor-only redraw can skip the rescan.
pub struct FindState {
    pub search: TextField,
    pub replace: TextField,
    pub mode: FindMode,
    pub focus: FieldFocus,
    /// Whether the find bar owns keyboard focus. While `true`, keys type into the
    /// focused field; while `false` (bar open but the document was clicked) keys fall
    /// through to the document so you can edit the buffer with the bar still visible.
    pub focused: bool,
    pub case_sensitive: bool,
    pub regex: bool,
    pub matches: Vec<Range<usize>>,
    pub active: Option<usize>,
    scanned: Option<(u64, String, bool, bool)>,
}

pub struct Editor {
    pub state: EditorState,
    file_path: Option<PathBuf>,
    input_blocked: bool,
    /// When set (via `--autosave`, used by the GhostText daemon), every buffer edit
    /// writes back to `file_path` so an external watcher sees the change immediately.
    autosave: bool,

    // --- GitHub autolink detection ---
    github_context: Option<GitHubContext>,
    github_client: Option<GitHubClient>,
    /// Shared (Arc<Mutex>) validation cache; the shell's tokio tasks write results
    /// into it off-thread and the render path reads it to color validated refs.
    github_validation_cache: GitHubValidationCache,
    naked_urls_by_line: HashMap<usize, Vec<NakedUrl>>,
    github_refs_by_line: HashMap<usize, Vec<RawGitHubMatch>>,
    /// Inline `$…$` math spans per line, from the viewport-windowed detection pass. Always
    /// present but only populated when the `math` feature is on (empty otherwise).
    math_spans_by_line: HashMap<usize, Vec<MathSpan>>,
    /// (buffer version, scanned line range) the cached detection reflects; lets
    /// `refresh_detection` skip the rescan when neither the text nor the scanned window
    /// changed (cursor-only rebuilds). Reset when the GitHub context changes.
    detection_key: Option<(u64, Range<usize>)>,
    /// Active `#`/`@` autocomplete popup, if the cursor is inside a trigger token.
    autocomplete: Option<AutocompleteState>,
    /// Active find bar, if find (Ctrl+F) is open. While `Some`, the shell routes all
    /// keystrokes to it instead of the document.
    find: Option<FindState>,
    /// Whether the right-docked outline panel is open. Reserves a horizontal strip so
    /// the document region insets (see `outline_width`).
    outline_open: bool,
    /// Byte offsets of folded headings (session UI state, never written to the file).
    /// Anchored by offset so a single-splice edit remaps them exactly; the hidden-line
    /// derivation re-reads live headings each frame, so a fold's reach tracks edits.
    folded_headings: HashSet<usize>,

    // --- inline git diff against HEAD ---
    /// (raw HEAD text, rendered snapshot of it) reused as the diff base.
    head_base: Option<(String, RenderSnapshot)>,
    diff_state: Option<DiffState>,

    // --- file watching ---
    /// Debounced watcher: coalesces the burst of fs events a single save emits (and
    /// handles atomic-save/rename) into one notification instead of several.
    file_watcher: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    file_watcher_rx: Option<mpsc::Receiver<()>>,
    /// mtime after our own last save, so the watcher can skip self-writes.
    last_save_mtime: Option<SystemTime>,
}

impl Editor {
    pub fn new(content: &str) -> Self {
        Self {
            state: EditorState::new(content),
            file_path: None,
            input_blocked: false,
            autosave: false,
            github_context: None,
            github_client: None,
            github_validation_cache: GitHubValidationCache::new(),
            naked_urls_by_line: HashMap::new(),
            github_refs_by_line: HashMap::new(),
            math_spans_by_line: HashMap::new(),
            detection_key: None,
            autocomplete: None,
            find: None,
            outline_open: false,
            folded_headings: HashSet::new(),
            head_base: None,
            diff_state: None,
            file_watcher: None,
            file_watcher_rx: None,
            last_save_mtime: None,
        }
    }

    /// Open a file from disk into a fresh editor. Loads content and refreshes the
    /// git diff base. Returns an empty buffer if the file can't be read.
    pub fn open(path: &Path) -> Self {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let mut editor = Self::new(&content);
        editor.file_path = Some(path.to_path_buf());
        editor.refresh_git_base();
        editor
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: PathBuf) {
        self.file_path = Some(path);
    }

    // --- buffer queries -----------------------------------------------------

    pub fn text(&self) -> String {
        self.state.text()
    }

    pub fn len(&self) -> usize {
        self.state.buffer.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn cursor_position(&self) -> usize {
        self.state.cursor().offset
    }

    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        let selection = &self.state.selection;
        (!selection.is_collapsed()).then(|| selection.range())
    }

    /// The currently selected text, if any (for clipboard copy/cut).
    pub fn selected_text(&self) -> Option<String> {
        self.selection_range()
            .map(|r| self.state.buffer.slice_cow(r).into_owned())
    }

    pub fn set_cursor(&mut self, offset: usize) {
        self.state.set_cursor(offset);
    }

    /// Line index containing buffer `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.state.buffer.byte_to_line(offset)
    }

    pub fn is_dirty(&self) -> bool {
        self.state.buffer.is_dirty()
    }

    pub fn mark_clean(&mut self) {
        self.state.buffer.mark_clean();
    }

    pub fn set_input_blocked(&mut self, blocked: bool) {
        self.input_blocked = blocked;
    }

    pub fn set_autosave(&mut self, autosave: bool) {
        self.autosave = autosave;
    }

    /// Write the buffer back to disk after an edit when autosave is on (GhostText).
    /// Guarded by `is_dirty` so a no-op "edit" doesn't churn the file.
    fn maybe_autosave(&mut self) {
        if self.autosave
            && self.state.buffer.is_dirty()
            && let Err(e) = self.save()
        {
            eprintln!("[writ] autosave failed: {e}");
        }
    }

    pub fn input_blocked(&self) -> bool {
        self.input_blocked
    }

    // --- edit operations (delegate to the pure engine, then sync diff) ------

    /// Single choke point for buffer-mutating engine ops: run `f`, then resync the inline
    /// diff so a new mutator can't silently skip the refresh. Cursor-only ops that don't
    /// touch the buffer (click/drag/move/select_all) deliberately bypass this.
    fn edit<R>(&mut self, f: impl FnOnce(&mut EditorState) -> R) -> R {
        // Snapshot the text (only when something is folded) so fold offsets can be remapped
        // across the edit from the actual splice — computed from the common prefix/suffix,
        // NOT the caret, since some edits (e.g. checkbox toggle) mutate away from the caret.
        let before = (!self.folded_headings.is_empty()).then(|| self.state.buffer.text());
        let result = f(&mut self.state);
        if let Some(before) = before {
            let after = self.state.buffer.text();
            if before != after {
                self.remap_folds(&before, &after);
            }
        }
        self.recompute_diff();
        self.maybe_autosave();
        result
    }

    /// Remap folded heading/list offsets across a single-splice edit, derived from the
    /// common prefix/suffix of the old and new text. Offsets before the splice are fixed,
    /// those after shift by the length delta, and any inside the replaced region drop.
    fn remap_folds(&mut self, before: &str, after: &str) {
        let (bb, ab) = (before.as_bytes(), after.as_bytes());
        let mut prefix = 0;
        while prefix < bb.len() && prefix < ab.len() && bb[prefix] == ab[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < bb.len() - prefix
            && suffix < ab.len() - prefix
            && bb[bb.len() - 1 - suffix] == ab[ab.len() - 1 - suffix]
        {
            suffix += 1;
        }
        let old_end = bb.len() - suffix; // end (in `before`) of the replaced region
        let delta = ab.len() as isize - bb.len() as isize;
        let old = std::mem::take(&mut self.folded_headings);
        self.folded_headings = old
            .into_iter()
            .filter_map(|o| {
                if o <= prefix {
                    Some(o)
                } else if o >= old_end {
                    Some((o as isize + delta) as usize)
                } else {
                    None // the anchor line was edited → fold no longer meaningful
                }
            })
            .collect();
    }

    pub fn insert_str(&mut self, text: &str) {
        self.edit(|s| s.insert_text(text));
    }

    /// Insert clipboard text with context-aware paste normalization (CRLF→LF,
    /// curly→straight quotes, blockquote-prefix continuation, code-block literal).
    pub fn paste(&mut self, text: &str) {
        let ctx = PasteContext::from_buffer(&self.state.buffer, self.cursor_position());
        let transformed = transform_paste(text, &ctx);
        self.edit(|s| s.insert_text(&transformed));
    }

    pub fn type_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    pub fn backspace(&mut self) {
        self.edit(|s| s.delete_backward());
    }

    pub fn delete_forward(&mut self) {
        self.edit(|s| s.delete_forward());
    }

    pub fn enter(&mut self) {
        self.edit(|s| s.enter());
    }

    pub fn shift_enter(&mut self) {
        self.edit(|s| s.shift_enter());
    }

    pub fn shift_alt_enter(&mut self) {
        self.edit(|s| s.shift_alt_enter());
    }

    pub fn tab(&mut self) {
        self.edit(|s| s.tab());
    }

    pub fn shift_tab(&mut self) {
        self.edit(|s| s.shift_tab());
    }

    pub fn move_in_direction(&mut self, direction: Direction, extend: bool) {
        let new_cursor = self.state.cursor_in_direction(direction);
        if extend {
            self.state.selection = self.state.selection.extend_to(new_cursor.offset);
        } else {
            self.state.selection = Selection::new(new_cursor.offset, new_cursor.offset);
        }
    }

    pub fn select_all(&mut self) {
        self.state.selection = Selection::select_all(&self.state.buffer);
    }

    pub fn cursor_in_code_block(&self) -> bool {
        self.state.cursor_in_code_block()
    }

    /// Smart space (suppressed at line/blockquote-content start; literal in code blocks).
    /// Returns false if the space was suppressed so the caller can decide the fallback.
    pub fn try_insert_space(&mut self) -> bool {
        self.edit(|s| s.try_insert_space())
    }

    /// After typing `>`, auto-insert the trailing space of a blockquote marker.
    pub fn maybe_complete_blockquote_marker(&mut self) {
        self.edit(|s| s.maybe_complete_blockquote_marker());
    }

    /// After typing the third ``` `/`~`, auto-insert the closing fence.
    pub fn maybe_complete_code_fence(&mut self) {
        self.edit(|s| s.maybe_complete_code_fence());
    }

    pub fn click(&mut self, buffer_offset: usize, shift_held: bool, click_count: usize) {
        self.state
            .handle_click(buffer_offset, shift_held, click_count);
    }

    pub fn drag(&mut self, buffer_offset: usize) {
        self.state.handle_drag(buffer_offset);
    }

    pub fn toggle_checkbox(&mut self, line_number: usize) {
        self.edit(|s| s.toggle_checkbox(line_number));
    }

    /// If buffer `offset` lands on a checkbox marker (`[ ]`/`[x]`), the line to toggle.
    /// Lets a click on the box flip it instead of just placing the caret.
    pub fn checkbox_at(&self, offset: usize) -> Option<usize> {
        let line = self.state.buffer.byte_to_line(offset);
        let markers = self.state.buffer.line_markers(line);
        markers
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::Checkbox { .. }) && m.range.contains(&offset))
            .then_some(line)
    }

    pub fn undo(&mut self) {
        if let Some(cursor_pos) = self.state.buffer.undo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            self.recompute_diff();
            self.maybe_autosave();
        }
    }

    pub fn redo(&mut self) {
        if let Some(cursor_pos) = self.state.buffer.redo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            self.recompute_diff();
            self.maybe_autosave();
        }
    }

    pub fn can_undo(&self) -> bool {
        self.state.buffer.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.state.buffer.can_redo()
    }

    pub fn set_text(&mut self, content: &str) {
        self.state.buffer = content.parse().unwrap_or_default();
        self.state.selection = Selection::new(0, 0);
        self.recompute_diff();
    }

    // --- GitHub autolink detection ------------------------------------------

    pub fn set_github_context(&mut self, context: GitHubContext) {
        self.github_context = Some(context);
        self.detection_key = None; // force the next refresh_detection to rescan
    }

    pub fn set_github_client(&mut self, client: GitHubClient) {
        self.github_client = Some(client);
    }

    pub fn github_client(&self) -> Option<&GitHubClient> {
        self.github_client.as_ref()
    }

    pub fn github_context(&self) -> Option<&GitHubContext> {
        self.github_context.as_ref()
    }

    /// The URL to open for a Ctrl/Cmd-click at buffer `offset`: a GitHub ref's web page,
    /// a naked URL, or a markdown link target — whichever covers the offset.
    pub fn link_at(&mut self, offset: usize) -> Option<String> {
        let line = self.line_of(offset);
        let raw = if let Some(m) = self
            .github_refs_by_line
            .get(&line)
            .and_then(|refs| refs.iter().find(|m| m.byte_range.contains(&offset)))
        {
            m.reference.url()
        } else if let Some(u) = self
            .naked_urls_by_line
            .get(&line)
            .and_then(|urls| urls.iter().find(|u| u.byte_range.contains(&offset)))
        {
            u.url.clone()
        } else {
            // Markdown link [text](url) / image: the region spans the whole link source.
            self.state
                .buffer
                .render_snapshot()
                .inline_styles_for_line(line)
                .into_iter()
                .find(|r| r.link_url.is_some() && r.full_range.contains(&offset))
                .and_then(|r| r.link_url)?
        };
        Some(self.resolve_link_target(raw))
    }

    /// A web URL / absolute path opens as-is; a relative target (a markdown link or image
    /// path) resolves against the document's directory — matching how images are loaded,
    /// so Ctrl-clicking a relative link/image opens the right file, not one relative to cwd.
    fn resolve_link_target(&self, url: String) -> String {
        if url.contains("://") || url.starts_with("mailto:") || Path::new(&url).is_absolute() {
            return url;
        }
        match self.file_path.as_ref().and_then(|p| p.parent()) {
            Some(dir) => dir.join(&url).to_string_lossy().into_owned(),
            None => url,
        }
    }

    pub fn github_validation_cache(&self) -> &GitHubValidationCache {
        &self.github_validation_cache
    }

    /// Clear the GitHub ref-validation and autocomplete caches so every detected ref
    /// re-validates from scratch — the Ctrl+R escape hatch for stale/invalid results.
    pub fn revalidate_github_refs(&self) {
        self.github_validation_cache.clear();
        if let Some(client) = self.github_client.as_ref() {
            client.clear_autocomplete_cache();
            client.clear_user_cache();
        }
    }

    pub fn github_refs_by_line(&self) -> &HashMap<usize, Vec<RawGitHubMatch>> {
        &self.github_refs_by_line
    }

    pub fn naked_urls_by_line(&self) -> &HashMap<usize, Vec<NakedUrl>> {
        &self.naked_urls_by_line
    }

    /// All GitHub references detected across the document: inline refs plus the
    /// github-ref-bearing naked URLs.
    pub fn detected_refs(&self) -> Vec<GitHubRef> {
        let mut refs: Vec<GitHubRef> = Vec::new();
        for m in self.github_refs_by_line().values().flatten() {
            refs.push(m.reference.clone());
        }
        for u in self.naked_urls_by_line().values().flatten() {
            if let Some(r) = &u.github_ref {
                refs.push(r.clone());
            }
        }
        refs
    }

    /// GitHub references detected on lines within `lines` only — the viewport-bounded
    /// counterpart of `detected_refs`, used to validate just the visible refs.
    pub fn detected_refs_in_lines(&self, lines: Range<usize>) -> Vec<GitHubRef> {
        let mut refs: Vec<GitHubRef> = Vec::new();
        for (line, matches) in self.github_refs_by_line() {
            if lines.contains(line) {
                refs.extend(matches.iter().map(|m| m.reference.clone()));
            }
        }
        for (line, urls) in self.naked_urls_by_line() {
            if lines.contains(line) {
                refs.extend(urls.iter().filter_map(|u| u.github_ref.clone()));
            }
        }
        refs
    }

    /// Detect GitHub refs and naked URLs across a line range, returning both keyed
    /// by line index. Pure scan over the render snapshot (no network).
    pub fn detect_links(
        &mut self,
        start_line: usize,
        end_line: usize,
    ) -> (
        HashMap<usize, Vec<RawGitHubMatch>>,
        HashMap<usize, Vec<NakedUrl>>,
    ) {
        let snapshot = self.state.buffer.render_snapshot();
        let mut github_matches_by_line = HashMap::new();
        let mut urls_by_line = HashMap::new();
        // Bucket inline styles once (O(n + styles)); per-line lookup was O(n²) here too.
        let styles_by_line = snapshot.inline_styles_by_line();

        #[allow(clippy::needless_range_loop)]
        for line_idx in start_line..end_line.min(snapshot.line_count()) {
            let line = snapshot.line_markers(line_idx);
            let line_range = line.range.clone();
            let line_text = snapshot
                .rope
                .slice(
                    snapshot.rope.byte_to_char(line_range.start)
                        ..snapshot.rope.byte_to_char(line_range.end),
                )
                .to_string();

            let inline_styles = &styles_by_line[line_idx];
            let code_ranges: Vec<_> = inline_styles
                .iter()
                .filter(|s| s.style.code)
                .map(|s| s.full_range.clone())
                .collect();

            if let Some(github_context) = &self.github_context {
                let matches = detect_github_references_in_line(
                    &line_text,
                    line_range.start,
                    Some(github_context),
                    &code_ranges,
                );
                if !matches.is_empty() {
                    github_matches_by_line.insert(line_idx, matches);
                }
            }

            let link_ranges: Vec<_> = inline_styles
                .iter()
                .filter(|s| s.link_url.is_some())
                .map(|s| s.full_range.clone())
                .collect();
            let urls = detect_naked_urls(&line_text, line_range.start, &code_ranges, &link_ranges);
            if !urls.is_empty() {
                urls_by_line.insert(line_idx, urls);
            }
        }

        (github_matches_by_line, urls_by_line)
    }

    /// Detect inline `$…$` math over `[start, end)` (the same viewport window as
    /// `detect_links`). `$$…$$` display blocks are collected once per build in the layout;
    /// here they're only used to exclude a `$` that belongs to display math.
    #[cfg(feature = "math")]
    fn detect_math(&mut self, start: usize, end: usize) -> HashMap<usize, Vec<MathSpan>> {
        let snapshot = self.state.buffer.render_snapshot();
        let block_ranges: Vec<Range<usize>> = snapshot
            .math_blocks()
            .into_iter()
            .map(|m| m.block)
            .collect();
        let mut by_line = HashMap::new();
        for line_idx in start..end {
            let range = snapshot.line_byte_range(line_idx);
            let text = snapshot
                .rope
                .slice(
                    snapshot.rope.byte_to_char(range.start)..snapshot.rope.byte_to_char(range.end),
                )
                .to_string();
            if !text.contains('$') {
                continue;
            }
            let code_ranges: Vec<Range<usize>> = snapshot
                .inline_styles_for_line(line_idx)
                .iter()
                .filter(|s| s.style.code)
                .map(|s| s.full_range.clone())
                .collect();
            let spans = detect_inline_math(&text, range.start, &code_ranges, &block_ranges);
            if !spans.is_empty() {
                by_line.insert(line_idx, spans);
            }
        }
        by_line
    }

    /// Re-run GitHub-ref / naked-URL detection over `lines` (clamped to the buffer) and
    /// cache the results. The caller passes the viewport (plus overscan + cursor line);
    /// every consumer — visible-line coloring, cursor-line autocomplete, and
    /// `detected_refs_in_lines` validation — is viewport/cursor-scoped, so a whole-buffer
    /// scan is unnecessary and would be O(total lines) per keystroke on large docs.
    /// Skips the rescan when neither the buffer version nor the window changed.
    pub fn refresh_detection(&mut self, lines: Range<usize>) {
        let version = self.state.buffer.version();
        let n = self.state.buffer.line_count();
        let lines = lines.start.min(n)..lines.end.min(n);
        if self.detection_key.as_ref() == Some(&(version, lines.clone())) {
            return;
        }
        let (refs, urls) = self.detect_links(lines.start, lines.end);
        self.github_refs_by_line = refs;
        self.naked_urls_by_line = urls;
        #[cfg(feature = "math")]
        {
            self.math_spans_by_line = self.detect_math(lines.start, lines.end);
        }
        self.detection_key = Some((version, lines));
    }

    /// Inline `$…$` math spans per line (empty for lines without any). Populated by the
    /// viewport-windowed `refresh_detection` scan (only under the `math` feature).
    pub fn math_spans_by_line(&self) -> &HashMap<usize, Vec<MathSpan>> {
        &self.math_spans_by_line
    }

    // --- find bar -----------------------------------------------------------

    /// Open the find bar in Find or Replace mode, focus the search field, and rescan.
    /// An already-open bar keeps its typed query/replacement (only the mode/focus change),
    /// so toggling Find↔Replace doesn't lose what you typed.
    pub fn open_find(&mut self, replace: bool) {
        let mode = if replace {
            FindMode::Replace
        } else {
            FindMode::Find
        };
        match self.find.as_mut() {
            Some(find) => {
                find.mode = mode;
                find.focus = FieldFocus::Search;
                find.focused = true;
            }
            None => {
                self.find = Some(FindState {
                    search: TextField::new(),
                    replace: TextField::new(),
                    mode,
                    focus: FieldFocus::Search,
                    focused: true,
                    case_sensitive: false,
                    regex: false,
                    matches: Vec::new(),
                    active: None,
                    scanned: None,
                });
            }
        }
        self.find_rescan();
    }

    pub fn close_find(&mut self) {
        self.find = None;
    }

    pub fn find_state(&self) -> Option<&FindState> {
        self.find.as_ref()
    }

    pub fn find_state_mut(&mut self) -> Option<&mut FindState> {
        self.find.as_mut()
    }

    /// Toggle the right-docked outline panel, returning the new open state.
    pub fn toggle_outline(&mut self) -> bool {
        self.outline_open = !self.outline_open;
        self.outline_open
    }

    pub fn outline_open(&self) -> bool {
        self.outline_open
    }

    /// Force the outline open/closed. Used by the headless snapshot path to render a
    /// golden frame with the panel reserved.
    pub fn set_outline_open(&mut self, open: bool) {
        self.outline_open = open;
    }

    // --- heading folding (session UI state; see `fold`) ----------------------

    pub fn line_count(&self) -> usize {
        self.state.buffer.line_count()
    }

    /// Merged, sorted line ranges the layout should collapse to zero height.
    pub fn hidden_line_ranges(&self) -> Vec<Range<usize>> {
        fold::hidden_line_ranges(
            self.state.buffer.headings(),
            self.state.buffer.list_items(),
            &self.folded_headings,
            self.state.buffer.line_count(),
        )
    }

    pub fn is_heading_folded(&self, byte_offset: usize) -> bool {
        self.folded_headings.contains(&byte_offset)
    }

    /// Whether `byte_offset` anchors a list-item fold (vs a heading) — lets the shell
    /// route list-chevron clicks to the item semantics instead of the heading level ones.
    pub fn is_list_fold_offset(&self, byte_offset: usize) -> bool {
        self.state
            .buffer
            .list_items()
            .iter()
            .any(|i| i.byte_offset == byte_offset)
    }

    /// Apply a fold-gutter click at `byte_offset`, dispatching on the modifier scope and
    /// the fold kind (heading vs list item). Ctrl = breadth (all at this heading level /
    /// list depth), Shift = depth (recursive: this item + everything nested), Ctrl+Shift =
    /// both, plain = just this one. The recursive/plain cases are kind-agnostic.
    pub fn apply_fold_gesture(&mut self, byte_offset: usize, ctrl: bool, shift: bool) {
        let is_list = self.is_list_fold_offset(byte_offset);
        match (ctrl, shift) {
            (true, true) if is_list => self.toggle_fold_list_level_deep_at(byte_offset),
            (true, false) if is_list => self.toggle_fold_list_level_at(byte_offset),
            (true, true) => self.toggle_fold_level_deep_at(byte_offset),
            (true, false) => self.toggle_fold_level_at(byte_offset),
            (false, true) => self.toggle_fold_recursive(byte_offset),
            (false, false) => self.toggle_fold(byte_offset),
        }
    }

    /// Toggle the fold on the heading anchored at `byte_offset` (a gutter-chevron click).
    pub fn toggle_fold(&mut self, byte_offset: usize) {
        if !self.folded_headings.remove(&byte_offset) {
            self.folded_headings.insert(byte_offset);
        }
        self.clamp_cursor_to_visible();
    }

    /// Recursive fold toggle (Shift+click a chevron): fold the heading together with all
    /// its descendant subheadings, or — if it's already folded — unfold it and them. Folds
    /// the descendants too so unfolding the parent later reveals a still-collapsed subtree,
    /// matching the TextMate/VS Code "fold recursively" convention.
    pub fn toggle_fold_recursive(&mut self, byte_offset: usize) {
        let offs = self.recursive_fold_set(byte_offset);
        if !offs.is_empty() {
            self.apply_group_fold(byte_offset, offs);
        }
    }

    /// Fold or unfold `offs` as a group, keyed on the anchor: fold (insert all) when the
    /// anchor is currently unfolded, else unfold (remove all). Re-clamps the caret.
    fn apply_group_fold(&mut self, byte_offset: usize, offs: Vec<usize>) {
        let folding = !self.folded_headings.contains(&byte_offset);
        for off in offs {
            if folding {
                self.folded_headings.insert(off);
            } else {
                self.folded_headings.remove(&off);
            }
        }
        self.clamp_cursor_to_visible();
    }

    /// The anchor `byte_offset` plus every foldable descendant nested inside its extent:
    /// deeper headings under a heading, or nested items under a list item. Empty if the
    /// offset isn't a foldable anchor. Kind is resolved from the offset (heading vs list).
    fn recursive_fold_set(&self, byte_offset: usize) -> Vec<usize> {
        let line_count = self.state.buffer.line_count();
        let headings = self.state.buffer.headings();
        if let Some(idx) = headings.iter().position(|h| h.byte_offset == byte_offset) {
            if !fold::heading_is_foldable(headings, idx, line_count) {
                return Vec::new();
            }
            let extent = fold::heading_extent(headings, idx, line_count);
            let mut offs = vec![byte_offset];
            for (j, h) in headings.iter().enumerate().skip(idx + 1) {
                if h.line >= extent.end {
                    break;
                }
                if fold::heading_is_foldable(headings, j, line_count) {
                    offs.push(h.byte_offset);
                }
            }
            return offs;
        }
        let items = self.state.buffer.list_items();
        if let Some(idx) = items.iter().position(|i| i.byte_offset == byte_offset) {
            if !fold::list_item_is_foldable(items, idx) {
                return Vec::new();
            }
            let extent = fold::list_item_extent(items, idx);
            let mut offs = vec![byte_offset];
            for (j, it) in items.iter().enumerate().skip(idx + 1) {
                if it.line >= extent.end {
                    break;
                }
                if fold::list_item_is_foldable(items, j) {
                    offs.push(it.byte_offset);
                }
            }
            return offs;
        }
        Vec::new()
    }

    /// Ctrl+click a list chevron: fold every foldable list item at the clicked item's
    /// nesting depth (the list analogue of [`toggle_fold_level_at`] for headings). Additive
    /// — leaves heading folds and other-depth list folds alone; toggles the group off if the
    /// clicked item is already folded.
    pub fn toggle_fold_list_level_at(&mut self, byte_offset: usize) {
        self.toggle_list_level(byte_offset, false);
    }

    /// Ctrl+Shift+click a list chevron: like [`toggle_fold_list_level_at`] but folds this
    /// depth AND every deeper one, so expanding a peer reveals its children collapsed.
    pub fn toggle_fold_list_level_deep_at(&mut self, byte_offset: usize) {
        self.toggle_list_level(byte_offset, true);
    }

    fn toggle_list_level(&mut self, byte_offset: usize, deep: bool) {
        let items = self.state.buffer.list_items();
        let Some(depth) = items
            .iter()
            .find(|i| i.byte_offset == byte_offset)
            .map(|i| i.depth)
        else {
            return;
        };
        let offs: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(i, it)| {
                fold::list_item_is_foldable(items, *i)
                    && if deep {
                        it.depth >= depth
                    } else {
                        it.depth == depth
                    }
            })
            .map(|(_, it)| it.byte_offset)
            .collect();
        self.apply_group_fold(byte_offset, offs);
    }

    /// Fold the thing the cursor is in: the innermost foldable list item the caret sits
    /// inside, else the nearest heading section at or above the caret.
    pub fn fold_at_cursor(&mut self) {
        let line = self.line_of(self.cursor_position());
        let line_count = self.state.buffer.line_count();
        let items = self.state.buffer.list_items();
        let list_off = items
            .iter()
            .enumerate()
            .filter(|(i, it)| {
                fold::list_item_is_foldable(items, *i)
                    && (it.line..fold::list_item_extent(items, *i).end).contains(&line)
            })
            .max_by_key(|(_, it)| it.line)
            .map(|(_, it)| it.byte_offset);
        if let Some(off) = list_off {
            self.folded_headings.insert(off);
            self.clamp_cursor_to_visible();
            return;
        }
        let headings = self.state.buffer.headings();
        let Some(idx) = fold::section_heading(headings, line) else {
            return;
        };
        if !fold::heading_is_foldable(headings, idx, line_count) {
            return;
        }
        let off = headings[idx].byte_offset;
        self.folded_headings.insert(off);
        self.clamp_cursor_to_visible();
    }

    /// Unfold the section the cursor is in.
    pub fn unfold_at_cursor(&mut self) {
        let line = self.line_of(self.cursor_position());
        let headings = self.state.buffer.headings();
        let Some(idx) = fold::section_heading(headings, line) else {
            return;
        };
        let off = headings[idx].byte_offset;
        self.folded_headings.remove(&off);
    }

    pub fn fold_all_headings(&mut self) {
        let line_count = self.state.buffer.line_count();
        let headings = self.state.buffer.headings();
        let offs: Vec<usize> = headings
            .iter()
            .enumerate()
            .filter(|(i, _)| fold::heading_is_foldable(headings, *i, line_count))
            .map(|(_, h)| h.byte_offset)
            .collect();
        self.folded_headings.extend(offs);
        self.clamp_cursor_to_visible();
    }

    pub fn unfold_all(&mut self) {
        self.folded_headings.clear();
    }

    /// Ctrl+click a chevron: fold every section at the clicked heading's level (a mouse
    /// path to [`fold_to_level`]). Toggles — if that heading is already folded, unfold all.
    pub fn toggle_fold_level_at(&mut self, byte_offset: usize) {
        self.toggle_level(byte_offset, false);
    }

    /// Ctrl+Shift+click a chevron: like [`toggle_fold_level_at`] but deep — every section
    /// at this level *and* deeper folds, so expanding one reveals its children collapsed.
    pub fn toggle_fold_level_deep_at(&mut self, byte_offset: usize) {
        self.toggle_level(byte_offset, true);
    }

    fn toggle_level(&mut self, byte_offset: usize, deep: bool) {
        let level = self
            .state
            .buffer
            .headings()
            .iter()
            .find(|h| h.byte_offset == byte_offset)
            .map(|h| h.level);
        let Some(level) = level else {
            return;
        };
        if self.folded_headings.contains(&byte_offset) {
            self.unfold_all();
        } else if deep {
            self.fold_to_level_deep(level);
        } else {
            self.fold_to_level(level);
        }
    }

    /// Replace the fold set with every foldable heading whose level satisfies `level_ok`.
    fn fold_headings_where(&mut self, level_ok: impl Fn(u8) -> bool) {
        let line_count = self.state.buffer.line_count();
        let headings = self.state.buffer.headings();
        self.folded_headings = headings
            .iter()
            .enumerate()
            .filter(|(i, h)| {
                level_ok(h.level) && fold::heading_is_foldable(headings, *i, line_count)
            })
            .map(|(_, h)| h.byte_offset)
            .collect();
        self.clamp_cursor_to_visible();
    }

    /// Fold to a heading depth: collapse exactly the sections at `level`, replacing any
    /// current folds. Headings shallower than `level` stay visible (with their bodies),
    /// and everything below a level-`level` heading hides — so expanding one reveals its
    /// whole subtree at once. Level 1 leaves only the top-level headings showing.
    pub fn fold_to_level(&mut self, level: u8) {
        self.fold_headings_where(|l| l == level);
    }

    /// Deep variant of [`fold_to_level`]: fold every foldable heading at `level` or deeper,
    /// pre-collapsing descendants so expanding a section reveals its children still folded.
    pub fn fold_to_level_deep(&mut self, level: u8) {
        self.fold_headings_where(|l| l >= level);
    }

    /// Auto-unfold any folded section the caret has entered (search-jump, outline click,
    /// arrow keys). A no-op when the caret is already on a visible line. Also drops folds
    /// whose heading was edited away (stale offset no longer matches any heading start).
    /// Returns `true` if the fold set changed (the caller must relayout).
    pub fn reveal_cursor(&mut self) -> bool {
        if self.folded_headings.is_empty() {
            return false;
        }
        let cursor_line = self.line_of(self.cursor_position());
        let line_count = self.state.buffer.line_count();
        let to_remove: Vec<usize> = {
            let headings = self.state.buffer.headings();
            let items = self.state.buffer.list_items();
            self.folded_headings
                .iter()
                .copied()
                .filter(
                    |&off| match fold::extent_for_offset(headings, items, off, line_count) {
                        Some(ext) => ext.contains(&cursor_line),
                        None => true, // stale offset (heading/item edited away): drop
                    },
                )
                .collect()
        };
        for off in &to_remove {
            self.folded_headings.remove(off);
        }
        !to_remove.is_empty()
    }

    /// Keep the "caret is never on a hidden line" invariant after a fold: if a collapse
    /// hid the caret, move it up to the heading line that folds the region.
    fn clamp_cursor_to_visible(&mut self) {
        let ranges = self.hidden_line_ranges();
        if ranges.is_empty() {
            return;
        }
        let cursor_line = self.line_of(self.cursor_position());
        if let Some(r) = ranges.iter().find(|r| r.contains(&cursor_line)) {
            let heading_line = r.start.saturating_sub(1);
            let off = self.state.buffer.line_to_byte(heading_line);
            self.state.selection = Selection::new(off, off);
        }
    }

    /// Rebuild the match list for the current query. Uses the regex crate for BOTH
    /// literal and regex modes so byte offsets stay exact and case-folding is correct:
    /// a literal query is `regex::escape`d, and `(?i)` is prepended when case-insensitive.
    /// An invalid pattern (e.g. a half-typed `(`) yields no matches rather than panicking.
    /// Sets the document selection to the active match so it gets caret/highlight/scroll
    /// for free. A no-op when `(version, query, case, regex)` is unchanged.
    pub fn find_rescan(&mut self) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let query = find.search.text().to_string();
        let case = find.case_sensitive;
        let regex = find.regex;
        let focused = find.focused;
        let version = self.state.buffer.version();
        if find.scanned.as_ref() == Some(&(version, query.clone(), case, regex)) {
            return;
        }

        if query.is_empty() {
            let find = self.find.as_mut().expect("find open");
            find.matches.clear();
            find.active = None;
            find.scanned = Some((version, query, case, regex));
            return;
        }

        let pattern = Self::build_find_pattern(&query, regex, case);

        let matches: Vec<Range<usize>> = match Regex::new(&pattern) {
            Ok(re) => {
                let text = self.state.buffer.text();
                re.find_iter(&text).map(|m| m.start()..m.end()).collect()
            }
            Err(_) => Vec::new(),
        };

        let cursor = self.cursor_position();
        let active = (!matches.is_empty())
            .then(|| matches.iter().position(|m| m.start >= cursor).unwrap_or(0));
        // Only pull the document selection onto the active match when the bar owns focus.
        // While the document is focused (bar open but unfocused), a rescan triggered by a
        // buffer edit must not yank the caret away from where the user is typing.
        if let Some(idx) = active
            && focused
        {
            let r = matches[idx].clone();
            self.state.selection = Selection::new(r.start, r.end);
        }

        let find = self.find.as_mut().expect("find open");
        find.matches = matches;
        find.active = active;
        find.scanned = Some((version, query, case, regex));
    }

    /// Advance to the next match (wrapping), moving the document selection onto it.
    /// Returns the new active match range so the shell can scroll it into view.
    pub fn find_next(&mut self) -> Option<Range<usize>> {
        self.find_step(true)
    }

    /// Retreat to the previous match (wrapping); otherwise like [`find_next`].
    pub fn find_prev(&mut self) -> Option<Range<usize>> {
        self.find_step(false)
    }

    fn find_step(&mut self, forward: bool) -> Option<Range<usize>> {
        let find = self.find.as_mut()?;
        let n = find.matches.len();
        if n == 0 {
            return None;
        }
        let cur = find.active.unwrap_or(0);
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        find.active = Some(next);
        let r = find.matches[next].clone();
        self.state.selection = Selection::new(r.start, r.end);
        Some(r)
    }

    pub fn find_toggle_case(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.case_sensitive = !find.case_sensitive;
        }
        self.find_rescan();
    }

    pub fn find_toggle_regex(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.regex = !find.regex;
        }
        self.find_rescan();
    }

    /// Swap keyboard focus between the search and replace fields. A no-op in Find mode,
    /// where there is no replace field to focus.
    pub fn find_toggle_field(&mut self) {
        if let Some(find) = self.find.as_mut()
            && find.mode == FindMode::Replace
        {
            find.focus = match find.focus {
                FieldFocus::Search => FieldFocus::Replace,
                FieldFocus::Replace => FieldFocus::Search,
            };
        }
    }

    /// Recompile the exact `Regex` `find_rescan` used for the current query (literal
    /// queries are `regex::escape`d, `(?i)` prepended when case-insensitive) so replace
    /// can expand `$1`/`${name}` capture groups against a matched slice. `None` for an
    /// empty or invalid pattern.
    /// Build the regex source for a find query: literal queries are `regex::escape`d, and
    /// `(?i)` is prepended when the search is case-insensitive.
    fn build_find_pattern(query: &str, regex: bool, case_sensitive: bool) -> String {
        let escaped;
        let base = if regex {
            query
        } else {
            escaped = regex::escape(query);
            escaped.as_str()
        };
        if case_sensitive {
            base.to_string()
        } else {
            format!("(?i){base}")
        }
    }

    fn find_regex(&self) -> Option<Regex> {
        let find = self.find.as_ref()?;
        let query = find.search.text();
        if query.is_empty() {
            return None;
        }
        let pattern = Self::build_find_pattern(query, find.regex, find.case_sensitive);
        Regex::new(&pattern).ok()
    }

    /// Replace the active match with the replacement text (one undo step), then rescan
    /// and land on the next match. In regex mode the replacement's `$1`/`${name}` groups
    /// expand from the matched slice; in literal mode it is inserted verbatim. No-op when
    /// there is no active match.
    pub fn find_replace_current(&mut self) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let Some(active) = find.active else {
            return;
        };
        let range = find.matches[active].clone();
        let replacement = find.replace.text().to_string();
        let regex_mode = find.regex;

        let expanded = if regex_mode {
            match self.find_regex() {
                Some(re) => {
                    let text = self.state.buffer.text();
                    re.replace(&text[range.clone()], replacement.as_str())
                        .into_owned()
                }
                None => replacement,
            }
        } else {
            replacement
        };

        // Replace the match range in a SINGLE undo step (a bare select+insert_text is a
        // delete then an insert — two undo entries — so one Ctrl+Z wouldn't fully revert
        // it). Same coalesce pattern as `find_replace_all`.
        self.edit(|s| {
            let head = s.buffer.undo_head();
            let text_before = s.buffer.text();
            let cursor_before = s.cursor().offset;
            s.buffer.replace(range.clone(), &expanded, cursor_before);
            let text_after = s.buffer.text();
            let cursor_after = (range.start + expanded.len()).min(text_after.len());
            s.buffer
                .coalesce_since(head, &text_before, &text_after, cursor_before, cursor_after);
            s.selection = Selection::new(cursor_after, cursor_after);
        });

        // Rescan advances `active`/selection to the first match at/after the new caret
        // (skipping any occurrence the replacement itself introduced), and the shell
        // scrolls that selection into view.
        self.find_rescan();
    }

    /// Replace every current match in a single undo step, keeping the bar open. Regex
    /// mode expands capture groups per match; literal mode inserts verbatim. Matches are
    /// applied right-to-left so earlier byte offsets stay valid as the text shifts.
    pub fn find_replace_all(&mut self) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let matches = find.matches.clone();
        let replacement = find.replace.text().to_string();
        let re = self.find_regex();
        let regex_mode = find.regex;

        self.edit(|s| {
            let head = s.buffer.undo_head();
            let text_before = s.buffer.text();
            let cursor_before = s.cursor().offset;

            for range in matches.iter().rev() {
                let expanded = match (regex_mode, &re) {
                    (true, Some(re)) => re
                        .replace(&text_before[range.clone()], replacement.as_str())
                        .into_owned(),
                    _ => replacement.clone(),
                };
                s.buffer.replace(range.clone(), &expanded, cursor_before);
            }

            let text_after = s.buffer.text();
            let cursor_after = cursor_before.min(text_after.len());
            // Collapse the per-match edits into one minimal undo entry.
            s.buffer
                .coalesce_since(head, &text_before, &text_after, cursor_before, cursor_after);
            s.selection = Selection::new(cursor_after, cursor_after);
        });

        self.find_rescan();
    }

    // --- GitHub autocomplete (#/@) ------------------------------------------

    pub fn autocomplete(&self) -> Option<&AutocompleteState> {
        self.autocomplete.as_ref()
    }

    pub fn close_autocomplete(&mut self) {
        self.autocomplete = None;
    }

    /// Move the selection up/down (wrapping). No-op if no popup / no suggestions.
    pub fn autocomplete_move(&mut self, forward: bool) {
        if let Some(ac) = &mut self.autocomplete
            && !ac.suggestions.is_empty()
        {
            let n = ac.suggestions.len();
            ac.selected_index = if forward {
                (ac.selected_index + 1) % n
            } else {
                (ac.selected_index + n - 1) % n
            };
        }
    }

    /// Set the selected suggestion index directly (mouse hover/click).
    pub fn autocomplete_select(&mut self, index: usize) {
        if let Some(ac) = &mut self.autocomplete
            && index < ac.suggestions.len()
        {
            ac.selected_index = index;
        }
    }

    /// Re-evaluate the popup against the current cursor. Returns true when the shell
    /// should (re)fetch suggestions for the new trigger/prefix.
    pub fn update_autocomplete_from_cursor(&mut self) -> bool {
        if self.github_context.is_none() || self.github_client.is_none() {
            self.autocomplete = None;
            return false;
        }

        let cursor = self.state.cursor().offset;
        let cursor_line = self.state.buffer.byte_to_line(cursor);

        // Cursor inside an already-detected issue ref → offer that ref's number.
        if let Some(refs) = self.github_refs_by_line.get(&cursor_line) {
            for github_match in refs {
                if cursor >= github_match.byte_range.start
                    && cursor <= github_match.byte_range.end
                    && let GitHubRef::Issue { number, .. } = &github_match.reference
                {
                    let prefix = number.to_string();
                    let trigger_offset = github_match.byte_range.start;
                    return self.set_autocomplete_state(
                        AutocompleteTrigger::Issue,
                        trigger_offset,
                        prefix,
                    );
                }
            }
        }

        // Otherwise scan back from the cursor for a `#`/`@` trigger.
        if cursor > 0 {
            let line_start = self.state.buffer.line_to_byte(cursor_line);
            let line_text = self.state.buffer.slice_cow(line_start..cursor).into_owned();
            if let Some((trigger, trigger_offset, prefix)) =
                Self::detect_autocomplete_trigger(&line_text, line_start)
            {
                return self.set_autocomplete_state(trigger, trigger_offset, prefix);
            }
        }

        self.autocomplete = None;
        false
    }

    /// Scan `line_text` (from line start up to the cursor) for the rightmost valid
    /// `#`/`@` trigger, returning its type, absolute offset, and typed prefix.
    fn detect_autocomplete_trigger(
        line_text: &str,
        line_start: usize,
    ) -> Option<(AutocompleteTrigger, usize, String)> {
        let triggers = [
            ('#', AutocompleteTrigger::Issue),
            ('@', AutocompleteTrigger::User),
        ];
        let mut best: Option<(AutocompleteTrigger, usize, String)> = None;

        for (trigger_char, trigger_type) in triggers {
            let Some(pos) = line_text.rfind(trigger_char) else {
                continue;
            };
            let at_boundary = pos == 0
                || line_text
                    .as_bytes()
                    .get(pos - 1)
                    .is_none_or(|&b| b == b' ' || b == b'\t' || b == b'\n');
            if !at_boundary {
                continue;
            }
            let prefix = line_text[pos + 1..].to_string();
            let valid = match trigger_type {
                // `# ` is a heading, not an issue ref.
                AutocompleteTrigger::Issue => !prefix.starts_with([' ', '\t']),
                AutocompleteTrigger::User => {
                    prefix.is_empty()
                        || (prefix.chars().all(|c| c.is_alphanumeric() || c == '-')
                            && !prefix.starts_with('-'))
                }
            };
            if !valid {
                continue;
            }
            let trigger_offset = line_start + pos;
            if best
                .as_ref()
                .is_none_or(|(_, off, _)| trigger_offset > *off)
            {
                best = Some((trigger_type, trigger_offset, prefix));
            }
        }
        best
    }

    /// Update the popup for a detected trigger, preserving suggestions/selection when
    /// only the prefix grew within the same trigger. Returns true if a fetch is needed.
    fn set_autocomplete_state(
        &mut self,
        trigger: AutocompleteTrigger,
        trigger_offset: usize,
        prefix: String,
    ) -> bool {
        let changed = self
            .autocomplete
            .as_ref()
            .map(|ac| ac.trigger != trigger || ac.prefix != prefix)
            .unwrap_or(true);
        if !changed {
            return false;
        }

        let old = self.autocomplete.take();
        let same_trigger = old
            .as_ref()
            .map(|ac| ac.trigger == trigger)
            .unwrap_or(false);
        let should_fetch = match trigger {
            AutocompleteTrigger::Issue => {
                let already = old
                    .as_ref()
                    .filter(|_| same_trigger)
                    .and_then(|ac| ac.fetched_prefix.as_ref())
                    == Some(&prefix);
                !already
            }
            AutocompleteTrigger::User => true,
        };

        // `old` is owned, so when the trigger matches MOVE its suggestions/selection/prefix
        // into the new state (a prefix keystroke keeps them) — no per-keystroke Vec clone.
        let (suggestions, selected_index, fetched_prefix) = match old.filter(|_| same_trigger) {
            Some(ac) => (ac.suggestions, ac.selected_index, ac.fetched_prefix),
            None => (Vec::new(), 0, None),
        };
        self.autocomplete = Some(AutocompleteState {
            trigger,
            trigger_offset,
            prefix,
            suggestions,
            selected_index,
            loading: false,
            fetched_prefix,
        });
        should_fetch
    }

    /// Mark the popup loading and return the (trigger, prefix) the shell should fetch.
    pub fn begin_autocomplete_fetch(&mut self) -> Option<(AutocompleteTrigger, String)> {
        let ac = self.autocomplete.as_mut()?;
        ac.loading = true;
        ac.fetched_prefix = Some(ac.prefix.clone());
        Some((ac.trigger, ac.prefix.clone()))
    }

    /// Install fetched suggestions if the popup is still on the same trigger+prefix.
    pub fn apply_autocomplete_suggestions(
        &mut self,
        trigger: AutocompleteTrigger,
        prefix: &str,
        suggestions: Vec<AutocompleteSuggestion>,
    ) {
        if let Some(ac) = &mut self.autocomplete
            && ac.trigger == trigger
            && ac.prefix == prefix
        {
            ac.suggestions = suggestions;
            ac.loading = false;
            ac.selected_index = 0;
        }
    }

    /// Replace the trigger token (`#…`/`@…`) with the selected suggestion. Returns
    /// true if a suggestion was accepted (buffer changed), closing the popup.
    pub fn accept_autocomplete_suggestion(&mut self) -> bool {
        let Some(ac) = self.autocomplete.take() else {
            return false;
        };
        if ac.suggestions.is_empty() {
            return false;
        }
        let replacement = match &ac.suggestions[ac.selected_index] {
            AutocompleteSuggestion::IssueOrPr { number, .. } => format!("#{number}"),
            AutocompleteSuggestion::User { login, .. } => format!("@{login}"),
        };
        let cursor = self.state.cursor().offset;
        // Select the trigger token (`#…`/`@…`) and replace it through the normal insert
        // path, so diff-recompute and undo semantics match a hand-typed edit.
        self.state.selection = Selection::new(ac.trigger_offset, cursor);
        self.insert_str(&replacement);
        true
    }

    // --- inline git diff ----------------------------------------------------

    pub fn diff_state(&self) -> Option<&DiffState> {
        self.diff_state.as_ref()
    }

    /// Recompute the inline diff of the current buffer against the cached HEAD base.
    pub fn recompute_diff(&mut self) {
        self.diff_state = self
            .head_base
            .as_ref()
            .and_then(|(base_text, base_snapshot)| {
                let current = self.state.buffer.text();
                let state = DiffState::compute(base_snapshot.clone(), base_text, &current);
                state.has_hunks().then_some(state)
            });
    }

    /// Load the git HEAD blob for the current file, snapshot it as the diff base,
    /// and recompute. No-op (clears the base) if there's no file or no HEAD blob.
    pub fn refresh_git_base(&mut self) {
        self.head_base = self
            .file_path
            .as_ref()
            .and_then(|path| head_blob_text(path))
            .map(|text| {
                let mut base: Buffer = text.parse().expect("Buffer parsing is infallible");
                let snapshot = base.render_snapshot();
                (text, snapshot)
            });
        self.recompute_diff();
    }

    /// Directly set the diff base (test/headless helper) from raw text.
    pub fn set_head_base(&mut self, base_text: &str) {
        let mut base: Buffer = base_text.parse().expect("Buffer parsing is infallible");
        let snapshot = base.render_snapshot();
        self.head_base = Some((base_text.to_string(), snapshot));
        self.recompute_diff();
    }

    // --- file I/O -----------------------------------------------------------

    /// Save the buffer to `file_path` (if set), recording the write mtime so the
    /// file watcher can distinguish our own write from an external edit.
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(path) = self.file_path.clone() else {
            return Ok(());
        };
        // Stream the rope's chunks straight to the file — no whole-document String alloc.
        let file = std::fs::File::create(&path)?;
        self.state
            .buffer
            .rope()
            .write_to(std::io::BufWriter::new(file))?;
        if let Ok(metadata) = std::fs::metadata(&path) {
            self.last_save_mtime = metadata.modified().ok();
        }
        self.state.buffer.mark_clean();
        Ok(())
    }

    /// Poll the file watcher; if the file changed on disk (and it wasn't our own
    /// save), reload it and refresh the diff base. Returns true if reloaded.
    /// Hand the file-watch receiver to the shell so it can forward change
    /// notifications into the event loop (waking it) instead of polling on a timer.
    pub fn take_file_watch_rx(&mut self) -> Option<mpsc::Receiver<()>> {
        self.file_watcher_rx.take()
    }

    pub fn last_save_mtime(&self) -> Option<SystemTime> {
        self.last_save_mtime
    }

    /// The blocking half of an external-file reload: read the file + the git HEAD blob.
    /// Pure/`Send` (no editor state, no `Rc`), so the shell runs it on a blocking worker
    /// off the render thread — the diff hot path must not touch disk on the UI thread.
    /// Returns `(new_content, head_base_text)`, or `None` to skip (our own save, or a
    /// read error). The cheap parse/snapshot/diff is done on the main thread by
    /// [`apply_reload`].
    pub fn read_reload(
        path: &Path,
        last_save_mtime: Option<SystemTime>,
    ) -> Option<(String, Option<String>)> {
        if let Some(last) = last_save_mtime
            && let Ok(meta) = std::fs::metadata(path)
            && let Ok(mtime) = meta.modified()
            && mtime == last
        {
            return None; // our own write
        }
        let content = std::fs::read_to_string(path).ok()?;
        Some((content, head_blob_text(path)))
    }

    /// The main-thread half of a reload: swap in the freshly-read `content` (preserving
    /// the cursor line) and set the diff base from `base_text`, then recompute the diff.
    /// Only parse/snapshot/diff work — no IO.
    pub fn apply_reload(&mut self, content: String, base_text: Option<String>) {
        if !self.state.buffer.content_eq(&content) {
            // Persist folds across the reload: remap their offsets from the actual
            // splice (external edits are usually a localized change), the same way a
            // local edit does. `set_text` leaves `folded_headings` untouched, so without
            // this the old offsets would land on the wrong lines in the new content.
            let before = (!self.folded_headings.is_empty()).then(|| self.state.buffer.text());
            let cursor_line = self.state.buffer.byte_to_line(self.state.selection.head);
            self.set_text(&content);
            if let Some(before) = before {
                self.remap_folds(&before, &content);
            }
            let line = cursor_line.min(self.state.buffer.line_count().saturating_sub(1));
            let offset = self.state.buffer.line_to_byte(line);
            self.state.selection = Selection::new(offset, offset);
        }
        match base_text {
            Some(text) => {
                let mut base: Buffer = text.parse().expect("Buffer parsing is infallible");
                let snapshot = base.render_snapshot();
                self.head_base = Some((text, snapshot));
            }
            None => self.head_base = None,
        }
        self.recompute_diff();
    }

    /// Start watching `file_path` for external modifications.
    pub fn watch_file(&mut self) -> notify::Result<()> {
        let Some(path) = self.file_path.clone() else {
            return Ok(());
        };
        let (tx, rx) = mpsc::channel();
        // 150ms debounce collapses a save's event burst (and atomic-save temp→rename)
        // into a single reload notification.
        let mut debouncer = new_debouncer(
            Duration::from_millis(150),
            None,
            move |result: DebounceEventResult| {
                if let Ok(events) = result
                    && events
                        .iter()
                        .any(|e| matches!(e.kind, EventKind::Modify(_) | EventKind::Create(_)))
                {
                    let _ = tx.send(());
                }
            },
        )?;
        debouncer.watch(&path, RecursiveMode::NonRecursive)?;
        self.file_watcher = Some(debouncer);
        self.file_watcher_rx = Some(rx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Secondary find-feature spike: measure whether a full-document scan per keystroke
    /// (rope→String, then `match_indices`) is cheap enough to run live. Not asserted on
    /// (timings are flaky in CI); run with `--ignored --nocapture` to see the numbers.
    #[test]
    #[ignore = "manual measurement; prints timing, no assertions"]
    fn find_scan_cost_on_large_document() {
        // ~10k lines of representative markdown (headings, prose, lists, code).
        let unit = "\
# Heading with a searchable word target here
Some prose paragraph mentioning target and other words in a sentence.
- a list item with target inside it
- another item, plainer
```
let code = target(); // fenced block line
```
> a blockquote line about targets and things
";
        let reps = 10_000 / unit.lines().count();
        let mut src = String::with_capacity(unit.len() * reps);
        for _ in 0..reps {
            src.push_str(unit);
        }
        let mut buffer = Buffer::new();
        buffer.insert(0, &src, 0);
        let line_count = buffer.text().lines().count();

        for query in ["target", "the", "nonexistent_zzz"] {
            let t = Instant::now();
            let text = buffer.text();
            let count = text.match_indices(query).count();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            println!(
                "scan {line_count} lines for {query:?}: {count} hits in {ms:.3} ms (text()+match_indices)",
            );
        }
    }

    /// Autosave (used by the GhostText daemon via `--autosave`) writes every edit back to
    /// the file with no explicit save — the daemon relays that to the browser.
    #[test]
    fn autosave_writes_on_every_edit() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("writ_autosave_test_{}.md", std::process::id()));
        std::fs::write(&path, "start\n").unwrap();

        let mut e = Editor::open(&path);
        e.set_autosave(true);
        e.set_cursor(e.len());
        e.insert_str("X");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "start\nX");

        e.backspace();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "start\n");

        // Without autosave, edits stay in memory only.
        e.set_autosave(false);
        e.insert_str("Y");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "start\n");

        std::fs::remove_file(&path).ok();
    }

    /// Ctrl/Cmd-click link resolution: a markdown link target, a naked URL, and a
    /// non-link position (None). Drives opening links in the browser.
    #[test]
    fn link_at_resolves_links() {
        let mut e = Editor::new("See [docs](https://example.com/docs) and https://plain.url/x\n");
        // Inside the markdown link source → its target URL (no detection needed).
        let md = e.text().find("docs]").unwrap();
        assert_eq!(
            e.link_at(md).as_deref(),
            Some("https://example.com/docs"),
            "markdown link target"
        );
        // Naked URL needs detection to populate the per-line index.
        e.refresh_detection(0..usize::MAX);
        let naked = e.text().find("plain.url").unwrap();
        assert_eq!(
            e.link_at(naked).as_deref(),
            Some("https://plain.url/x"),
            "naked URL"
        );
        // Plain text position → no link.
        assert_eq!(e.link_at(0), None);
    }

    /// Ctrl-click resolution: a relative link/image target resolves against the doc's
    /// directory; web/absolute targets pass through unchanged.
    #[test]
    fn link_at_resolves_relative_against_doc_dir() {
        let mut e = Editor::new("![img](assets/p.png) and [w](https://x.com)\n");
        e.set_file_path(std::path::PathBuf::from("/home/u/notes/doc.md"));
        let rel = e.text().find("assets").unwrap();
        assert_eq!(
            e.link_at(rel).as_deref(),
            Some("/home/u/notes/assets/p.png"),
            "relative image path resolves against the doc dir"
        );
        let web = e.text().find("x.com").unwrap();
        assert_eq!(
            e.link_at(web).as_deref(),
            Some("https://x.com"),
            "web URL as-is"
        );
    }

    /// The input behaviors restored from the gpui `on_key_down` (Home/End/doc-boundary
    /// movement, select-all, smart space, blockquote/fence auto-completion) — wired
    /// through `Editor`, so a future shell refactor can't silently drop them again.

    #[test]
    fn restored_editor_input_behaviors() {
        // Home/End = line boundary; Ctrl variants = doc boundary; Shift extends.
        let mut e = Editor::new("hello\nworld two\n");
        e.set_cursor(3);
        e.move_in_direction(Direction::LineEnd, false);
        assert_eq!(e.cursor_position(), 5, "End → end of line");
        e.move_in_direction(Direction::LineStart, false);
        assert_eq!(e.cursor_position(), 0, "Home → line start");
        e.move_in_direction(Direction::DocEnd, false);
        assert_eq!(e.cursor_position(), e.len(), "Ctrl+End → doc end");
        e.move_in_direction(Direction::DocStart, true);
        assert_eq!(
            e.selection_range(),
            Some(0..e.len()),
            "Shift+Ctrl+Home extends"
        );

        e.select_all();
        assert_eq!(e.selection_range(), Some(0..e.len()), "Ctrl+A selects all");

        // Smart space: suppressed at line start, inserted mid-line.
        let mut e = Editor::new("hello\n");
        e.set_cursor(0);
        assert!(!e.try_insert_space(), "space suppressed at line start");
        assert_eq!(e.text(), "hello\n");
        e.set_cursor(3);
        assert!(e.try_insert_space(), "space inserted mid-line");
        assert_eq!(e.text(), "hel lo\n");

        // Blockquote marker auto-spaces after `>`.
        let mut e = Editor::new("");
        e.insert_str(">");
        e.maybe_complete_blockquote_marker();
        assert_eq!(e.text(), "> ", "`>` completes to `> `");

        // Code fence auto-closes after the third backtick.
        let mut e = Editor::new("");
        for _ in 0..3 {
            e.insert_str("`");
            e.maybe_complete_code_fence();
        }
        assert_eq!(
            e.text(),
            "```\n```",
            "triple backtick auto-closes the fence"
        );
        assert!(
            e.cursor_in_code_block(),
            "cursor sits inside the new code block"
        );
        // Tab inside a code block indents with spaces (mirrors the shell's Tab arm),
        // never a stray fence character.
        if e.cursor_in_code_block() {
            e.insert_str("    ");
        }
        assert_eq!(
            e.text(),
            "```    \n```",
            "Tab in code block inserts 4 spaces"
        );

        // Paste routes through transform_paste: CRLF→LF, curly quotes→straight,
        // blockquote continuation. (Regression: shell was inserting raw text.)
        let mut e = Editor::new("> ");
        e.set_cursor(e.len());
        e.paste("line 1\r\nline 2 \u{201C}quoted\u{201D}");
        assert_eq!(
            e.text(),
            "> line 1\n> line 2 \"quoted\"",
            "paste normalizes CRLF, quotes, and continues the blockquote"
        );
    }

    /// Phase 2 DoD: a headless editor opens a doc, applies edits, toggles a
    /// checkbox, detects a GitHub ref, and computes diff state — no gpui.
    #[test]
    fn headless_edit_checkbox_github_diff() {
        // --- edit ops ---
        let mut editor = Editor::new("hello");
        editor.set_cursor(editor.len());
        editor.type_char('!');
        assert_eq!(editor.text(), "hello!");
        editor.backspace();
        assert_eq!(editor.text(), "hello");

        // --- checkbox toggle ---
        let mut editor = Editor::new("- [ ] task\n");
        editor.toggle_checkbox(0);
        assert!(
            editor.text().starts_with("- [x]"),
            "checkbox should be checked: {:?}",
            editor.text()
        );

        // --- GitHub ref detection ---
        let mut editor = Editor::new("See #123 for details\n");
        editor.set_github_context(GitHubContext {
            owner: "wilfreddenton".into(),
            repo: "writ".into(),
        });
        editor.refresh_detection(0..usize::MAX);
        assert!(
            editor.github_refs_by_line().contains_key(&0),
            "should detect #123 on line 0"
        );

        // --- inline diff against HEAD ---
        let mut editor = Editor::new("line1\nline2\n");
        assert!(editor.diff_state().is_none(), "no base yet");
        editor.set_head_base("line1\n");
        assert!(
            editor.diff_state().is_some(),
            "adding line2 vs HEAD should produce diff hunks"
        );
        // Reverting to match HEAD clears the diff.
        editor.set_text("line1\n");
        assert!(editor.diff_state().is_none(), "matching HEAD => no hunks");
    }

    fn ctx() -> GitHubContext {
        GitHubContext {
            owner: "rust-lang".into(),
            repo: "rust".into(),
        }
    }

    fn issue_suggestion(number: u64, title: &str) -> AutocompleteSuggestion {
        AutocompleteSuggestion::IssueOrPr {
            number,
            symbol: "●".into(),
            status: IssueStatus::Open,
            title: title.into(),
        }
    }

    #[test]
    fn checkbox_at_detects_box_not_content() {
        let editor = Editor::new("- [ ] task\n");
        let text = editor.text();
        // A hit anywhere on `[ ]` toggles line 0.
        let box_off = text.find('[').unwrap();
        assert_eq!(editor.checkbox_at(box_off), Some(0));
        assert_eq!(editor.checkbox_at(box_off + 1), Some(0));
        // A hit on the content is not a checkbox click.
        assert_eq!(editor.checkbox_at(text.find("task").unwrap()), None);
        // A non-checkbox line has no box.
        let plain = Editor::new("just a paragraph\n");
        assert_eq!(plain.checkbox_at(3), None);
    }

    #[test]
    fn autocomplete_issue_trigger_and_accept() {
        let mut editor = Editor::new("Working on #12\n");
        editor.set_github_context(ctx());
        editor.set_github_client(GitHubClient::new("dummy".into()));
        editor.refresh_detection(0..usize::MAX);
        editor.set_cursor(14); // end of "#12"

        // Cursor inside the detected issue ref opens Issue autocomplete for "12".
        assert!(editor.update_autocomplete_from_cursor());
        let ac = editor.autocomplete().expect("popup open");
        assert_eq!(ac.trigger, AutocompleteTrigger::Issue);
        assert_eq!(ac.prefix, "12");

        // Install suggestions and accept a different one — the ref token is replaced.
        editor.begin_autocomplete_fetch();
        editor.apply_autocomplete_suggestions(
            AutocompleteTrigger::Issue,
            "12",
            vec![issue_suggestion(999, "some issue")],
        );
        assert!(editor.accept_autocomplete_suggestion());
        assert_eq!(editor.text(), "Working on #999\n");
        assert!(editor.autocomplete().is_none(), "popup closes on accept");
    }

    #[test]
    fn detected_refs_in_lines_is_viewport_bounded() {
        let mut editor = Editor::new("See #1 here\n\n\nAnd #2 there\n");
        editor.set_github_context(ctx());
        editor.set_github_client(GitHubClient::new("dummy".into()));
        editor.refresh_detection(0..usize::MAX);

        let numbers = |refs: Vec<GitHubRef>| {
            refs.iter()
                .filter_map(|r| match r {
                    GitHubRef::Issue { number, .. } => Some(*number),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // Line 0 only sees #1, not the line-3 #2.
        let line0 = numbers(editor.detected_refs_in_lines(0..1));
        assert!(line0.contains(&1), "line 0 range should include #1");
        assert!(!line0.contains(&2), "line 0 range should exclude line-3 #2");

        // A wide range covers both refs.
        let all = numbers(editor.detected_refs_in_lines(0..10));
        assert!(
            all.contains(&1) && all.contains(&2),
            "wide range should include both refs"
        );
    }

    #[test]
    fn folding_collapses_reveals_and_survives_edits() {
        let mut editor = Editor::new("# A\nbody1\nbody2\n## B\nsub\n# C\ntail\n");

        // Caret in A's body → folding A collapses lines 1..5 (through the deeper ## B),
        // and relocates the caret onto the heading line so it never sits on a hidden line.
        editor.set_cursor(editor.state.buffer.line_to_byte(1));
        editor.fold_at_cursor();
        assert_eq!(editor.hidden_line_ranges(), vec![1..5]);
        assert_eq!(editor.line_of(editor.cursor_position()), 0);

        // Moving the caret into the folded region auto-reveals it.
        editor.set_cursor(editor.state.buffer.line_to_byte(2));
        assert!(editor.reveal_cursor());
        assert!(editor.hidden_line_ranges().is_empty());

        // Fold the last section, then insert above it: the fold's byte anchor remaps.
        let c_off = editor.state.buffer.headings()[2].byte_offset;
        editor.toggle_fold(c_off);
        assert!(!editor.hidden_line_ranges().is_empty());
        editor.set_cursor(editor.state.buffer.line_to_byte(1));
        editor.insert_str("x");
        let c_off2 = editor.state.buffer.headings()[2].byte_offset;
        assert_eq!(c_off2, c_off + 1);
        assert!(editor.is_heading_folded(c_off2), "fold survived the edit");

        editor.fold_all_headings();
        assert!(!editor.hidden_line_ranges().is_empty());
        editor.unfold_all();
        assert!(editor.hidden_line_ranges().is_empty());
    }

    #[test]
    fn fold_to_level_collapses_that_depth() {
        // # A(0) a(1) ## B(2) b(3) # C(4) c(5)
        let mut editor = Editor::new("# A\na\n## B\nb\n# C\nc\n");

        // Level 1: fold both H1 sections → only the two H1 lines stay visible.
        editor.fold_to_level(1);
        assert_eq!(editor.hidden_line_ranges(), vec![1..4, 5..7]);

        // Level 2: fold only the H2 section; H1 lines and their intro bodies stay visible.
        editor.fold_to_level(2);
        assert_eq!(editor.hidden_line_ranges(), vec![3..4]);

        // Replaces prior folds rather than accumulating; a level with no headings clears.
        editor.fold_to_level(4);
        assert!(editor.hidden_line_ranges().is_empty());
    }

    #[test]
    fn recursive_fold_pre_folds_descendants() {
        // # A(0) a(1) ## B(2) b(3) ### C(4) c(5) # D(6) d(7)
        let mut editor = Editor::new("# A\na\n## B\nb\n### C\nc\n# D\nd\n");
        let a = editor.state.buffer.headings()[0].byte_offset;
        let b = editor.state.buffer.headings()[1].byte_offset;
        let c = editor.state.buffer.headings()[2].byte_offset;

        // Recursively folding A folds A and its descendants B and C (but not sibling D).
        editor.toggle_fold_recursive(a);
        assert!(editor.is_heading_folded(a));
        assert!(editor.is_heading_folded(b));
        assert!(editor.is_heading_folded(c));

        // Unfolding just A (plain toggle) reveals B, which is still folded underneath.
        editor.toggle_fold(a);
        assert!(!editor.is_heading_folded(a));
        assert!(editor.is_heading_folded(b), "descendant stays folded");
        assert_eq!(editor.hidden_line_ranges(), vec![3..6]); // B's subtree still collapsed

        // Recursive toggle again (now A unfolded) folds the whole A subtree back down.
        editor.toggle_fold_recursive(a);
        assert!(editor.is_heading_folded(a) && editor.is_heading_folded(c));
    }

    #[test]
    fn list_folding_task_list_children_survive_checkbox_and_reveal() {
        // A nested task list (the primary fold use case):
        //   0 - [ ] parent   1   - [ ] child1   2   - [ ] child2   3 - [ ] sibling
        let mut editor =
            Editor::new("- [ ] parent\n  - [ ] child1\n  - [ ] child2\n- [ ] sibling\n");
        let items = editor.state.buffer.list_items().to_vec();
        let parent = items
            .iter()
            .find(|i| i.line == 0)
            .expect("parent")
            .byte_offset;
        let sibling = items
            .iter()
            .find(|i| i.line == 3)
            .expect("sibling")
            .byte_offset;

        // Folding the parent hides its two child items; the sibling is untouched.
        editor.toggle_fold(parent);
        assert_eq!(editor.hidden_line_ranges(), vec![1..3]);
        assert!(editor.is_list_fold_offset(parent) && !editor.is_list_fold_offset(999));
        assert!(!editor.is_heading_folded(sibling));

        // The parent checkbox still toggles while its children are folded (checking a task
        // also strikes it through), and the fold offset survives the edit — kids stay hidden.
        editor.toggle_checkbox(0);
        assert_eq!(editor.text().lines().next(), Some("- [x] ~~parent~~"));
        assert_eq!(
            editor.hidden_line_ranges(),
            vec![1..3],
            "fold survived the toggle"
        );

        // Moving the caret into a hidden child auto-reveals the fold.
        editor.set_cursor(editor.state.buffer.line_to_byte(1));
        assert!(editor.reveal_cursor());
        assert!(editor.hidden_line_ranges().is_empty());
    }

    #[test]
    fn list_folding_only_items_with_sublists_are_foldable() {
        // Leaves must NOT be foldable — regression for tree-sitter extending a leaf's
        // range into a trailing blank line (`- c`) or the next sibling's indent (`- c1`).
        //   0 - a  1 - b  2 - c  3 (blank)  4 - p  5   - c1  6   - c2  7 - q
        let mut editor = Editor::new("- a\n- b\n- c\n\n- p\n  - c1\n  - c2\n- q\n");
        let items = editor.state.buffer.list_items().to_vec();
        let foldable = |line: usize| -> bool {
            let idx = items.iter().position(|i| i.line == line).unwrap();
            crate::fold::list_item_is_foldable(&items, idx)
        };
        for leaf in [0, 1, 2, 5, 6, 7] {
            assert!(!foldable(leaf), "line {leaf} is a leaf, must not fold");
        }
        assert!(foldable(4), "`- p` has a sublist, must fold");

        // Folding `- p` hides only its two children (5,6) — not the blank, not `- q`.
        let p = items.iter().find(|i| i.line == 4).unwrap().byte_offset;
        editor.toggle_fold(p);
        assert_eq!(editor.hidden_line_ranges(), vec![5..7]);
    }

    #[test]
    fn click_snaps_out_of_list_marker_prefix() {
        // Line 1 "  - [ ] child": indent 4..6, bullet 6..8, `[` at 8, content 12.
        let mut editor = Editor::new("- p\n  - [ ] child\n");
        // Click in the indent → snaps back to the line start (nearer boundary).
        editor.click(5, false, 1);
        assert_eq!(editor.cursor_position(), 4);
        // Click just after the bullet → snaps forward to the checkbox `[`.
        editor.click(7, false, 1);
        assert_eq!(editor.cursor_position(), 8);
        // The boundaries themselves are reachable; a plain paragraph is untouched.
        editor.click(4, false, 1);
        assert_eq!(editor.cursor_position(), 4);
        editor.click(8, false, 1);
        assert_eq!(editor.cursor_position(), 8);
        let mut para = Editor::new("hello world\n");
        para.click(3, false, 1);
        assert_eq!(para.cursor_position(), 3);
    }

    #[test]
    fn list_folding_ctrl_click_folds_all_at_depth() {
        // Two top-level items with sublists, plus a deeper level:
        //   0 - p   1   - a   2     - x   3   - b   4 - q   5   - c
        let mut editor = Editor::new("- p\n  - a\n    - x\n  - b\n- q\n  - c\n");
        let items = editor.state.buffer.list_items().to_vec();
        let off = |line: usize| items.iter().find(|i| i.line == line).unwrap().byte_offset;

        // Ctrl+click a top-level (depth-1) item folds BOTH top-level foldable items (p, q),
        // not the deeper `- a`. Additive; doesn't touch depth-2.
        editor.toggle_fold_list_level_at(off(0));
        assert!(editor.is_heading_folded(off(0)) && editor.is_heading_folded(off(4)));
        assert!(
            !editor.is_heading_folded(off(1)),
            "depth-2 item `- a` untouched"
        );

        // Toggling the same group off clears it.
        editor.toggle_fold_list_level_at(off(0));
        assert!(editor.hidden_line_ranges().is_empty());

        // Deep variant folds this depth and deeper: p, q (depth 1) AND a (depth 2).
        editor.toggle_fold_list_level_deep_at(off(0));
        assert!(
            editor.is_heading_folded(off(0))
                && editor.is_heading_folded(off(4))
                && editor.is_heading_folded(off(1))
        );
    }

    #[test]
    fn list_folding_recursive_and_survives_edit() {
        // 3-deep nested list: 0 a  1  b  2   c  3 d(sibling)
        let mut editor = Editor::new("- a\n  - b\n    - c\n- d\n");
        let items = editor.state.buffer.list_items().to_vec();
        let a = items.iter().find(|i| i.line == 0).unwrap().byte_offset;
        let b = items.iter().find(|i| i.line == 1).unwrap().byte_offset;

        // Recursive fold on the root folds the whole subtree (a's descendant b too).
        editor.toggle_fold_recursive(a);
        assert!(editor.is_heading_folded(a) && editor.is_heading_folded(b));

        // Plain-unfolding a reveals b, which stays folded underneath.
        editor.toggle_fold(a);
        assert!(editor.is_heading_folded(b));
        assert_eq!(editor.hidden_line_ranges(), vec![2..3]); // c still hidden under b

        // Fold survives an edit above it: insert a new first line, offsets remap.
        editor.toggle_fold_recursive(a);
        editor.set_cursor(0);
        editor.insert_str("- z\n");
        let b2 = editor
            .state
            .buffer
            .list_items()
            .iter()
            .find(|i| i.line == 2)
            .unwrap()
            .byte_offset;
        assert!(
            editor.is_heading_folded(b2),
            "fold tracked the shifted item"
        );
    }

    #[test]
    fn ctrl_click_folds_all_at_level() {
        // # A(0) a(1) ## B(2) b(3) # C(4) c(5) ## D(6) d(7)
        let mut editor = Editor::new("# A\na\n## B\nb\n# C\nc\n## D\nd\n");
        let a = editor.state.buffer.headings()[0].byte_offset;
        let b = editor.state.buffer.headings()[1].byte_offset;
        let c = editor.state.buffer.headings()[2].byte_offset;
        let d = editor.state.buffer.headings()[3].byte_offset;

        // Ctrl+clicking the H2 "B" folds every H2 (B and D), not just B.
        editor.toggle_fold_level_at(b);
        assert!(editor.is_heading_folded(b) && editor.is_heading_folded(d));
        assert!(!editor.is_heading_folded(a) && !editor.is_heading_folded(c));

        // Ctrl+clicking a now-folded heading unfolds everything.
        editor.toggle_fold_level_at(b);
        assert!(editor.hidden_line_ranges().is_empty());
    }

    #[test]
    fn ctrl_shift_click_folds_level_and_deeper() {
        // # A(0) ## B(1) ### C(2) c(3) # D(4)
        let mut editor = Editor::new("# A\n## B\n### C\nc\n# D\n");
        let a = editor.state.buffer.headings()[0].byte_offset;
        let b = editor.state.buffer.headings()[1].byte_offset;
        let c = editor.state.buffer.headings()[2].byte_offset;

        // Ctrl+Shift on the H2 folds this level and deeper: B and C, but not the H1s.
        editor.toggle_fold_level_deep_at(b);
        assert!(editor.is_heading_folded(b) && editor.is_heading_folded(c));
        assert!(!editor.is_heading_folded(a));

        // Toggles off when the clicked heading is already folded.
        editor.toggle_fold_level_deep_at(b);
        assert!(editor.hidden_line_ranges().is_empty());
    }

    #[test]
    fn autocomplete_user_trigger() {
        let mut editor = Editor::new("cc @tor\n");
        editor.set_github_context(ctx());
        editor.set_github_client(GitHubClient::new("dummy".into()));
        editor.refresh_detection(0..usize::MAX);
        editor.set_cursor(7); // end of "@tor"

        assert!(editor.update_autocomplete_from_cursor());
        let ac = editor.autocomplete().expect("popup open");
        assert_eq!(ac.trigger, AutocompleteTrigger::User);
        assert_eq!(ac.prefix, "tor");
    }

    #[test]
    fn autocomplete_needs_client_and_context() {
        // Heading `# ` is not an issue trigger, and no client ⇒ never opens.
        let mut editor = Editor::new("# heading\n");
        editor.set_github_context(ctx());
        editor.set_cursor(2);
        assert!(!editor.update_autocomplete_from_cursor());
        assert!(editor.autocomplete().is_none());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("writ-core-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");
        std::fs::write(&path, "original\n").unwrap();

        let mut editor = Editor::open(&path);
        assert_eq!(editor.text(), "original\n");
        editor.set_cursor(editor.len());
        editor.insert_str("more\n");
        editor.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\nmore\n");
        assert!(!editor.is_dirty(), "saved buffer is clean");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Type `query` into the open find bar's search field and rescan.
    fn set_query(editor: &mut Editor, query: &str) {
        editor.find_state_mut().unwrap().search.set_text(query);
        editor.find_rescan();
    }

    #[test]
    fn find_literal_and_case_toggle() {
        let mut e = Editor::new("Cat cat CAT cot\n");
        e.set_cursor(0);
        e.open_find(false);

        // Case-insensitive (default): all three "cat"s match, not "cot".
        set_query(&mut e, "cat");
        assert_eq!(e.find_state().unwrap().matches.len(), 3);

        // Toggling case-sensitive drops "Cat" and "CAT", leaving only "cat".
        e.find_toggle_case();
        assert_eq!(e.find_state().unwrap().matches.len(), 1);
    }

    #[test]
    fn find_regex_mode_and_invalid_pattern() {
        let mut e = Editor::new("a table, a tible, a topple\n");
        e.set_cursor(0);
        e.open_find(false);
        e.find_toggle_regex();

        set_query(&mut e, "t.ble");
        assert_eq!(
            e.find_state().unwrap().matches.len(),
            2,
            "t.ble matches table and tible"
        );

        // A half-typed pattern must not panic — it just yields no matches.
        set_query(&mut e, "(");
        assert!(e.find_state().unwrap().matches.is_empty());
        assert!(e.find_state().unwrap().active.is_none());
    }

    #[test]
    fn find_adjacent_matches_are_non_overlapping() {
        let mut e = Editor::new("aaaa\n");
        e.set_cursor(0);
        e.open_find(false);
        set_query(&mut e, "aa");
        // Non-overlapping left-to-right: [0..2, 2..4], not 3.
        assert_eq!(e.find_state().unwrap().matches.len(), 2);
    }

    #[test]
    fn find_next_cycles_and_sets_selection() {
        let mut e = Editor::new("x . x . x\n");
        e.set_cursor(0);
        e.open_find(false);
        set_query(&mut e, "x");
        let m = &e.find_state().unwrap().matches;
        assert_eq!(m.len(), 3);
        let ranges: Vec<_> = m.clone();

        // active starts at the first match at/after the caret (offset 0).
        assert_eq!(e.find_state().unwrap().active, Some(0));
        assert_eq!(e.selection_range(), Some(ranges[0].clone()));

        assert_eq!(e.find_next(), Some(ranges[1].clone()));
        assert_eq!(e.find_state().unwrap().active, Some(1));
        assert_eq!(e.selection_range(), Some(ranges[1].clone()));

        assert_eq!(e.find_next(), Some(ranges[2].clone()));
        assert_eq!(e.find_next(), Some(ranges[0].clone()), "wraps to start");
        assert_eq!(e.find_state().unwrap().active, Some(0));

        assert_eq!(e.find_prev(), Some(ranges[2].clone()), "prev wraps back");
    }

    #[test]
    fn find_active_picks_match_at_or_after_caret() {
        // Caret just past the second "x": active is the third match (first at/after caret).
        let mut e = Editor::new("x . x . x\n");
        let third = e.text().rfind('x').unwrap();
        e.set_cursor(e.text()[..third].rfind('x').unwrap() + 1);
        e.open_find(false);
        set_query(&mut e, "x");
        assert_eq!(e.find_state().unwrap().active, Some(2));

        // Caret past every match wraps active back to the first.
        let mut e = Editor::new("x . x . x\n");
        e.set_cursor(e.len());
        e.open_find(false);
        set_query(&mut e, "x");
        assert_eq!(
            e.find_state().unwrap().active,
            Some(0),
            "wraps to first match"
        );
    }

    fn set_replace(editor: &mut Editor, replacement: &str) {
        editor
            .find_state_mut()
            .unwrap()
            .replace
            .set_text(replacement);
    }

    #[test]
    fn find_replace_current_literal_advances() {
        let mut e = Editor::new("cat cat cat\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "cat");
        set_replace(&mut e, "dog");
        assert_eq!(e.find_state().unwrap().active, Some(0));

        e.find_replace_current();
        assert_eq!(e.text(), "dog cat cat\n");
        // Rescan finds the remaining two "cat"s and lands on the next one.
        assert_eq!(e.find_state().unwrap().matches.len(), 2);
        assert_eq!(e.find_state().unwrap().active, Some(0));
        assert_eq!(e.selection_range(), Some(4..7));
    }

    #[test]
    fn find_replace_current_noop_without_active() {
        let mut e = Editor::new("hello\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "zzz");
        set_replace(&mut e, "x");
        assert!(e.find_state().unwrap().active.is_none());
        e.find_replace_current();
        assert_eq!(e.text(), "hello\n");
    }

    #[test]
    fn find_replace_all_literal_longer_and_shorter() {
        // Longer replacement.
        let mut e = Editor::new("a a a\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "a");
        set_replace(&mut e, "bb");
        e.find_replace_all();
        assert_eq!(e.text(), "bb bb bb\n");
        assert!(e.find_state().unwrap().matches.is_empty());

        // Shorter replacement.
        let mut e = Editor::new("aa aa\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "aa");
        set_replace(&mut e, "x");
        e.find_replace_all();
        assert_eq!(e.text(), "x x\n");
    }

    #[test]
    fn find_replace_current_regex_expands_capture() {
        let mut e = Editor::new("user_id and role_id\n");
        e.set_cursor(0);
        e.open_find(true);
        e.find_toggle_regex();
        set_query(&mut e, r"(\w+)_id");
        set_replace(&mut e, "${1}Id");
        e.find_replace_current();
        assert_eq!(e.text(), "userId and role_id\n");
    }

    #[test]
    fn find_replace_all_regex_expands_every_capture() {
        let mut e = Editor::new("user_id and role_id\n");
        e.set_cursor(0);
        e.open_find(true);
        e.find_toggle_regex();
        set_query(&mut e, r"(\w+)_id");
        set_replace(&mut e, "${1}Id");
        e.find_replace_all();
        assert_eq!(e.text(), "userId and roleId\n");
    }

    #[test]
    fn find_replace_current_replacement_containing_query_no_loop() {
        let mut e = Editor::new("cat cat\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "cat");
        set_replace(&mut e, "cat!");
        e.find_replace_current();
        assert_eq!(e.text(), "cat! cat\n");
        // The occurrence inside the just-inserted replacement is skipped; active is the
        // next real match past the caret, so repeated replace can't spin forever.
        assert_eq!(e.find_state().unwrap().matches.len(), 2);
        assert_eq!(e.find_state().unwrap().active, Some(1));
        assert_eq!(e.selection_range(), Some(5..8));
    }

    #[test]
    fn find_replace_all_is_single_undo_step() {
        let mut e = Editor::new("a a a\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "a");
        set_replace(&mut e, "bb");
        e.find_replace_all();
        assert_eq!(e.text(), "bb bb bb\n");
        e.undo();
        assert_eq!(
            e.text(),
            "a a a\n",
            "one undo reverts the whole replace-all"
        );
    }

    #[test]
    fn folds_persist_and_remap_across_reload() {
        let mut e = Editor::new("# One\nbody\n## Two\nmore\n");
        let two = e.text().find("## Two").unwrap();
        e.toggle_fold(two);
        assert!(e.folded_headings.contains(&two));
        // An external edit prepends a line; the reload should shift the fold offset so it
        // still lands on the (moved) heading rather than pointing at stale bytes.
        let prefix = "intro line\n";
        e.apply_reload(format!("{prefix}# One\nbody\n## Two\nmore\n"), None);
        let two_after = e.text().find("## Two").unwrap();
        assert_eq!(two_after, two + prefix.len());
        assert!(
            e.folded_headings.contains(&two_after),
            "fold should remap to the heading's new offset after reload"
        );
    }

    #[test]
    fn find_replace_current_is_single_undo_step() {
        let mut e = Editor::new("a a a\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "a");
        set_replace(&mut e, "bb");
        e.find_replace_current();
        assert_eq!(e.text(), "bb a a\n");
        e.undo();
        assert_eq!(e.text(), "a a a\n", "one undo reverts a single replace");
    }

    #[test]
    fn open_find_focuses_the_bar() {
        let mut e = Editor::new("hi\n");
        e.open_find(false);
        assert!(e.find_state().unwrap().focused, "opens focused");

        // Simulate a document click unfocusing the bar; re-opening (Ctrl+F) refocuses it.
        e.find_state_mut().unwrap().focused = false;
        e.open_find(false);
        assert!(e.find_state().unwrap().focused, "re-opening refocuses");
    }

    #[test]
    fn rescan_while_unfocused_does_not_move_selection() {
        let mut e = Editor::new("cat cat cat\n");
        e.set_cursor(0);
        e.open_find(false);
        set_query(&mut e, "cat");
        // Focused: the active match owns the document selection.
        assert_eq!(e.selection_range(), Some(0..3));

        // Unfocus the bar (as a document click does) and edit the buffer in the doc.
        e.find_state_mut().unwrap().focused = false;
        e.set_cursor(9);
        e.insert_str("X");
        e.find_rescan();

        // Highlights track the edit, but the caret stays where the user typed — the rescan
        // must not yank the selection onto a match.
        assert_eq!(e.find_state().unwrap().matches.len(), 2);
        assert!(
            e.selection_range().is_none(),
            "unfocused rescan leaves the doc caret alone"
        );
    }

    #[test]
    fn undo_of_replace_restores_text_and_rescan_refreshes_matches() {
        let mut e = Editor::new("cat cat cat\n");
        e.set_cursor(0);
        e.open_find(true);
        set_query(&mut e, "cat");
        set_replace(&mut e, "dog");
        e.find_replace_all();
        assert_eq!(e.text(), "dog dog dog\n");
        assert!(e.find_state().unwrap().matches.is_empty());

        // The find-bar undo passthrough: undo the replace, then rescan the stale matches.
        e.undo();
        e.find_rescan();
        assert_eq!(e.text(), "cat cat cat\n");
        assert_eq!(
            e.find_state().unwrap().matches.len(),
            3,
            "rescan after undo restores the full match set"
        );
    }
}
