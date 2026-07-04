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

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::time::SystemTime;

use crate::buffer::{Buffer, RenderSnapshot};
use crate::cursor::Selection;
use crate::diff::DiffState;
use crate::editor::{Direction, EditorState};
use crate::git::head_blob_text;
use crate::github::{GitHubClient, GitHubValidationCache, IssueStatus};
use crate::inline::{
    GitHubContext, GitHubRef, NakedUrl, RawGitHubMatch, detect_github_references_in_line,
    detect_naked_urls,
};
use crate::marker::MarkerKind;
use crate::paste::{PasteContext, transform_paste};

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
    /// (buffer version, scanned line range) the cached detection reflects; lets
    /// `refresh_detection` skip the rescan when neither the text nor the scanned window
    /// changed (cursor-only rebuilds). Reset when the GitHub context changes.
    detection_key: Option<(u64, Range<usize>)>,
    /// Active `#`/`@` autocomplete popup, if the cursor is inside a trigger token.
    autocomplete: Option<AutocompleteState>,

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
            detection_key: None,
            autocomplete: None,
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
        let result = f(&mut self.state);
        self.recompute_diff();
        self.maybe_autosave();
        result
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
        if let Some(refs) = self.github_refs_by_line.get(&line)
            && let Some(m) = refs.iter().find(|m| m.byte_range.contains(&offset))
        {
            return Some(m.reference.url());
        }
        if let Some(urls) = self.naked_urls_by_line.get(&line)
            && let Some(u) = urls.iter().find(|u| u.byte_range.contains(&offset))
        {
            return Some(u.url.clone());
        }
        // Markdown link [text](url): the styled region spans the whole link source.
        self.state
            .buffer
            .render_snapshot()
            .inline_styles_for_line(line)
            .into_iter()
            .find(|r| r.link_url.is_some() && r.full_range.contains(&offset))
            .and_then(|r| r.link_url)
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
        self.detection_key = Some((version, lines));
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

        self.autocomplete = Some(AutocompleteState {
            trigger,
            trigger_offset,
            prefix,
            suggestions: old
                .as_ref()
                .filter(|_| same_trigger)
                .map(|ac| ac.suggestions.clone())
                .unwrap_or_default(),
            selected_index: old
                .as_ref()
                .filter(|_| same_trigger)
                .map(|ac| ac.selected_index)
                .unwrap_or(0),
            loading: false,
            fetched_prefix: old
                .filter(|_| same_trigger)
                .and_then(|ac| ac.fetched_prefix),
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
        let content = self.state.buffer.text();
        std::fs::write(&path, &content)?;
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

    /// Reload the buffer from disk if the on-disk content differs (skipping our own
    /// saves via `last_save_mtime`). Returns true if the buffer changed.
    pub fn reload_from_disk(&mut self) -> bool {
        let Some(path) = self.file_path.clone() else {
            return false;
        };
        if let Some(last) = self.last_save_mtime
            && let Ok(meta) = std::fs::metadata(&path)
            && let Ok(mtime) = meta.modified()
            && mtime == last
        {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            return false;
        };
        if content != self.state.buffer.text() {
            let cursor_line = self.state.buffer.byte_to_line(self.state.selection.head);
            self.set_text(&content);
            let line = cursor_line.min(self.state.buffer.line_count().saturating_sub(1));
            let offset = self.state.buffer.line_to_byte(line);
            self.state.selection = Selection::new(offset, offset);
        }
        self.refresh_git_base();
        true
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
        assert_eq!(e.selection_range(), Some(0..e.len()), "Shift+Ctrl+Home extends");

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
        assert_eq!(e.text(), "```\n```", "triple backtick auto-closes the fence");
        assert!(e.cursor_in_code_block(), "cursor sits inside the new code block");
        // Tab inside a code block indents with spaces (mirrors the shell's Tab arm),
        // never a stray fence character.
        if e.cursor_in_code_block() {
            e.insert_str("    ");
        }
        assert_eq!(e.text(), "```    \n```", "Tab in code block inserts 4 spaces");

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
}
