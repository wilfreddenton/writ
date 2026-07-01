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
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use crate::buffer::{Buffer, RenderSnapshot};
use crate::cursor::Selection;
use crate::diff::DiffState;
use crate::editor::{Direction, EditorState};
use crate::git::head_blob_text;
use crate::github::{GitHubClient, GitHubValidationCache};
use crate::inline::{
    GitHubContext, NakedUrl, RawGitHubMatch, detect_github_references_in_line, detect_naked_urls,
};

pub struct Editor {
    pub state: EditorState,
    file_path: Option<PathBuf>,
    input_blocked: bool,

    // --- GitHub autolink detection ---
    github_context: Option<GitHubContext>,
    github_client: Option<GitHubClient>,
    /// Held for the async ref-validation port (Phase 4) which reads/writes it off
    /// tokio; detection alone (this phase) doesn't touch it.
    #[allow(dead_code)]
    github_validation_cache: GitHubValidationCache,
    naked_urls_by_line: HashMap<usize, Vec<NakedUrl>>,
    github_refs_by_line: HashMap<usize, Vec<RawGitHubMatch>>,

    // --- inline git diff against HEAD ---
    /// (raw HEAD text, rendered snapshot of it) reused as the diff base.
    head_base: Option<(String, RenderSnapshot)>,
    diff_state: Option<DiffState>,

    // --- file watching ---
    file_watcher: Option<notify::RecommendedWatcher>,
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
            github_context: None,
            github_client: None,
            github_validation_cache: GitHubValidationCache::new(),
            naked_urls_by_line: HashMap::new(),
            github_refs_by_line: HashMap::new(),
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

    pub fn set_cursor(&mut self, offset: usize) {
        self.state.set_cursor(offset);
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

    pub fn input_blocked(&self) -> bool {
        self.input_blocked
    }

    // --- edit operations (delegate to the pure engine, then sync diff) ------

    pub fn insert_str(&mut self, text: &str) {
        self.state.insert_text(text);
        self.recompute_diff();
    }

    pub fn type_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    pub fn backspace(&mut self) {
        self.state.delete_backward();
        self.recompute_diff();
    }

    pub fn delete_forward(&mut self) {
        self.state.delete_forward();
        self.recompute_diff();
    }

    pub fn enter(&mut self) {
        self.state.enter();
        self.recompute_diff();
    }

    pub fn shift_enter(&mut self) {
        self.state.shift_enter();
        self.recompute_diff();
    }

    pub fn shift_alt_enter(&mut self) {
        self.state.shift_alt_enter();
        self.recompute_diff();
    }

    pub fn tab(&mut self) {
        self.state.tab();
        self.recompute_diff();
    }

    pub fn shift_tab(&mut self) {
        self.state.shift_tab();
        self.recompute_diff();
    }

    pub fn move_in_direction(&mut self, direction: Direction, extend: bool) {
        let new_cursor = match direction {
            Direction::Left => self.state.cursor().move_left(&self.state.buffer),
            Direction::Right => self.state.cursor().move_right(&self.state.buffer),
            Direction::Up => self.state.cursor().move_up(&self.state.buffer),
            Direction::Down => self.state.cursor().move_down(&self.state.buffer),
        };
        if extend {
            self.state.selection = self.state.selection.extend_to(new_cursor.offset);
        } else {
            self.state.selection = Selection::new(new_cursor.offset, new_cursor.offset);
        }
    }

    pub fn click(&mut self, buffer_offset: usize, shift_held: bool, click_count: usize) {
        self.state
            .handle_click(buffer_offset, shift_held, click_count);
    }

    pub fn drag(&mut self, buffer_offset: usize) {
        self.state.handle_drag(buffer_offset);
    }

    pub fn toggle_checkbox(&mut self, line_number: usize) {
        self.state.toggle_checkbox_for_test(line_number);
        self.recompute_diff();
    }

    pub fn undo(&mut self) {
        if let Some(cursor_pos) = self.state.buffer.undo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            self.recompute_diff();
        }
    }

    pub fn redo(&mut self) {
        if let Some(cursor_pos) = self.state.buffer.redo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            self.recompute_diff();
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
    }

    pub fn set_github_client(&mut self, client: GitHubClient) {
        self.github_client = Some(client);
    }

    pub fn github_refs_by_line(&self) -> &HashMap<usize, Vec<RawGitHubMatch>> {
        &self.github_refs_by_line
    }

    pub fn naked_urls_by_line(&self) -> &HashMap<usize, Vec<NakedUrl>> {
        &self.naked_urls_by_line
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

            let inline_styles = snapshot.inline_styles_for_line(line_idx);
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

    /// Re-run detection across the whole buffer and cache the results.
    pub fn refresh_detection(&mut self) {
        let line_count = self.state.buffer.line_count();
        let (refs, urls) = self.detect_links(0, line_count);
        self.github_refs_by_line = refs;
        self.naked_urls_by_line = urls;
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
    pub fn poll_file_changes(&mut self) -> bool {
        let Some(rx) = &self.file_watcher_rx else {
            return false;
        };
        // Drain all pending events; only reload once.
        let mut changed = false;
        while rx.try_recv().is_ok() {
            changed = true;
        }
        if changed { self.reload_file() } else { false }
    }

    fn reload_file(&mut self) -> bool {
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
        use notify::{EventKind, RecursiveMode, Watcher};

        let Some(path) = self.file_path.clone() else {
            return Ok(());
        };
        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res
                    && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                {
                    let _ = tx.send(());
                }
            })?;
        watcher.watch(&path, RecursiveMode::NonRecursive)?;
        self.file_watcher = Some(watcher);
        self.file_watcher_rx = Some(rx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        editor.refresh_detection();
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
