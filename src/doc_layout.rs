//! Document viewport: per-line Parley layouts stacked by a prefix-sum height
//! model, plus scroll (see MIGRATION-PLAN.md, Phase 3). Replaces gpui's `ListState`
//! (which gave virtualization + scroll-to-reveal for free) with hand-rolled math.
//!
//! The height model is the top defect surface the plan flags: `tops` has length
//! `n + 1` where `tops[i]` is the top y of line `i` and `tops[n]` is the bottom of
//! the last line. One off-by-one here misplaces everything below it, so the pure
//! prefix-sum + visible-range functions are unit-tested independent of any GPU.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use parley::{Affinity, Cluster, Cursor, PositionedLayoutItem};
use ropey::Rope;
use vello::Scene;
use vello::kurbo::{Affine, Rect, Stroke};
use vello::peniko::{Brush, Color, Fill, ImageBrush};

use crate::buffer::RenderSnapshot;
use crate::consts::{MAX_CONTENT_WIDTH, UI_LINE_HEIGHT};
use crate::diff::{DiffState, InlineChange};
use crate::editor::EditorTheme;
use crate::image_cache::{ImageCache, ImageState};
use crate::inline::{
    GitHubContext, MathSpan, NakedUrl, RawGitHubMatch, StyledRegion, github_refs_to_styled_regions,
    naked_urls_to_styled_regions,
};
#[cfg(feature = "math")]
use crate::math;
#[cfg(feature = "mermaid")]
use crate::mermaid;
#[cfg(feature = "math")]
use crate::render::InlineMathRef;
use crate::render::{
    ImageRef, InlineImageRef, LineRender, TableCtx, build_cell_render, build_line_render,
};
use crate::segment_map::SegmentMap;
use crate::table::{Align, RowKind, TableInfo, TableRow};
use crate::text_engine::{
    StyleRun, TextEngine, display_range_selection, peniko_color, peniko_color_alpha,
};
use crate::validation::GitHubValidationCache;

/// A screen-space rectangle (device px), already offset by padding + scroll.
pub type ScreenRect = (f64, f64, f64, f64);

/// A borrowed view of the in-progress IME composition for `DocLayout::build`: the
/// composing `text` and its caret/selection `cursor` (byte offsets *within* `text`,
/// `None` = hidden caret). Spliced into the caret line at render time and drawn
/// underlined; never mutates the buffer.
pub struct PreeditView<'a> {
    pub text: &'a str,
    pub cursor: Option<(usize, usize)>,
}

/// GitHub autolink data threaded into layout so validated refs render as colored,
/// possibly-shortened links. Borrowed from `core::Editor` for the build call.
pub struct GithubRenderData<'a> {
    pub refs_by_line: &'a HashMap<usize, Vec<RawGitHubMatch>>,
    pub urls_by_line: &'a HashMap<usize, Vec<NakedUrl>>,
    pub cache: &'a GitHubValidationCache,
    pub context: Option<&'a GitHubContext>,
}

impl GithubRenderData<'_> {
    /// The extra styled regions (validated refs + naked URLs) for one line.
    fn extra_regions(&self, line: usize) -> Vec<StyledRegion> {
        let mut v = self
            .refs_by_line
            .get(&line)
            .map(|m| github_refs_to_styled_regions(m, self.cache))
            .unwrap_or_default();
        if let Some(urls) = self.urls_by_line.get(&line) {
            v.extend(naked_urls_to_styled_regions(urls, self.cache, self.context));
        }
        v
    }
}

/// Content hash identifying a laid-out line. Two lines with the same key produce
/// byte-identical Parley layouts, so a cache can skip re-shaping.
fn line_key(
    text: &str,
    scale: f32,
    font_size: f32,
    line_height: f32,
    max_advance: f32,
    runs: &[StyleRun],
    content_start: usize,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    scale.to_bits().hash(&mut h);
    font_size.to_bits().hash(&mut h);
    line_height.to_bits().hash(&mut h);
    max_advance.to_bits().hash(&mut h);
    // Same display text can have different content-start columns (real bullet vs a
    // literally-typed "• "), which change the hanging indent — so key on it.
    content_start.hash(&mut h);
    for r in runs {
        r.range.start.hash(&mut h);
        r.range.end.hash(&mut h);
        for c in r.color.components {
            c.to_bits().hash(&mut h);
        }
        (r.bold, r.italic, r.mono, r.underline, r.strikethrough).hash(&mut h);
    }
    h.finish()
}

/// A swept, content-addressed cache shared by the shell across rebuilds: values
/// keyed by a `u64` content hash and reset each frame via `begin`/`sweep`. `begin`
/// marks a new frame; every `get`/`set`/`get_or_build` records its key as used; and
/// `sweep` drops entries not touched that frame, so the cache stays bounded to the
/// current document (avoiding re-shaping unchanged lines every keystroke). `V` is
/// `Rc`-wrapped for the layout/render caches, so a hit is a refcount bump rather than
/// a deep copy of shaped glyphs or a `LineRender`.
pub struct SweptCache<V> {
    map: HashMap<u64, V>,
    used: HashSet<u64>,
}

// Manual `Default` (not derived) so it doesn't require `V: Default` — the maps are
// always default-constructible regardless of the value type.
impl<V> Default for SweptCache<V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            used: HashSet::new(),
        }
    }
}

impl<V: Clone> SweptCache<V> {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&mut self) {
        self.used.clear();
    }

    fn get(&mut self, key: u64) -> Option<V> {
        let v = self.map.get(&key).cloned();
        if v.is_some() {
            self.used.insert(key);
        }
        v
    }

    fn get_or_build(&mut self, key: u64, build: impl FnOnce() -> V) -> V {
        self.used.insert(key);
        self.map.entry(key).or_insert_with(build).clone()
    }

    fn set(&mut self, key: u64, value: V) {
        self.used.insert(key);
        self.map.insert(key, value);
    }

    fn sweep(&mut self) {
        let used = &self.used;
        self.map.retain(|k, _| used.contains(k));
    }
}

/// Per-line Parley layout cache: skips re-shaping lines whose content + style is
/// unchanged.
pub type LineCache = SweptCache<Rc<parley::Layout<Brush>>>;

/// Key for the per-line render cache. Within one buffer `version`, line `line_idx`
/// has fixed text + tree context, so its `LineRender` depends only on whether the
/// cursor sits on it (`cursor_key` = the offset when on-line, else a sentinel — an
/// off-line render is cursor-independent). Lines carrying GitHub `extra_regions`
/// bypass the cache (those can change without a version bump, on validation).
fn render_key(version: u64, line_idx: usize, cursor_key: usize) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    version.hash(&mut h);
    line_idx.hash(&mut h);
    cursor_key.hash(&mut h);
    h.finish()
}

/// Render-key cursor sentinel for a table row when the cursor is inside the table block
/// but on a *different* row: all such rows share one reveal state (distinct from grid's
/// `usize::MAX` and from an on-row cursor offset, which is a real, smaller position).
const REVEAL_ELSEWHERE: usize = usize::MAX - 1;

/// The cursor's contribution to a line's render: the offset if the cursor is on the
/// line (markers/regions there stay revealed), else a sentinel. Matches the on-line
/// test in `build_line_render`.
fn cursor_key_for(range: &Range<usize>, cursor: usize) -> usize {
    let on = if range.start == range.end {
        cursor == range.start
    } else {
        cursor >= range.start && cursor <= range.end
    };
    if on { cursor } else { usize::MAX }
}

/// Per-line `LineRender` cache: skips the tree-sitter style queries + segment-map
/// build for lines whose render is unchanged — so cursor moves, scroll, and
/// async-validation rebuilds (which don't bump the buffer version) recompute only the
/// handful of lines that changed.
pub type RenderCache = SweptCache<Rc<LineRender>>;

/// Per-line measured *height* cache (device px, the real line only — ghost blocks are
/// added separately). Keyed by the same `render_key` as the render cache, so it holds
/// exact heights for lines materialized on any recent frame; un-materialized (head/tail
/// virtualized) lines read their height from here when present, falling back to the
/// char-count estimate. This is what lets the layout drop to O(visible) without the
/// scroll positions of already-seen regions drifting.
pub type HeightCache = SweptCache<f32>;

/// One shaped, wrapped table cell: its layout (drawing) plus the display↔buffer
/// `SegmentMap` and empty-cell caret landing, both used to hit-test a click inside the
/// cell. `None` for a ragged/missing cell.
struct CellSlot {
    layout: Rc<parley::Layout<Brush>>,
    map: SegmentMap,
    /// For an empty cell, the caret offset just inside the opening pipe (so a click lands
    /// off the pipe); `None` for a non-empty cell, which hit-tests via `layout`/`map`.
    empty_landing: Option<usize>,
    /// Display-byte ranges of the cell's inline-code spans, used to paint the translucent
    /// chip behind the code glyphs (mirrors body lines' `code_ranges`).
    code_ranges: Vec<Range<usize>>,
}
type CellLayout = Option<CellSlot>;
type GridLayouts = Vec<Vec<CellLayout>>;
/// One measured table cell (`None` for a missing cell): display text + style runs, the
/// cell's `SegmentMap`, and its empty-cell landing — carried from the column pre-pass to
/// the shaping pass. A full grid of them.
struct MeasuredCell {
    text: String,
    runs: Vec<StyleRun>,
    map: SegmentMap,
    empty_landing: Option<usize>,
    code_ranges: Vec<Range<usize>>,
}
type CellData = Option<MeasuredCell>;
type GridCells = Vec<Vec<CellData>>;

/// Caret landing for a click on a table cell: its content start for a non-empty cell,
/// else a spot one space in past the opening `|` (a trimmed-empty cell's content range
/// collapses onto the closing-pipe boundary, so `content.start` would sit on that pipe).
fn cell_caret_landing(rope: &Rope, content: &Range<usize>) -> usize {
    if content.start < content.end {
        return content.start;
    }
    let closing = content.start;
    let mut open = closing;
    while open > 0 && rope.get_byte(open - 1) != Some(b'|') {
        open -= 1;
    }
    (open + 1).min(closing.saturating_sub(1)).max(open)
}

/// A grid-rendered table's geometry + shaped cells, cached once per edit and shared
/// (via `Rc`) by every row of the table. `col_x`/`col_w` are device px from the line
/// origin (before `pad_x`); `row_heights[0]` is the header, `row_heights[1 + i]` the
/// i-th body row; `row_layouts` is parallel (one shaped, wrapped cell layout per column,
/// `None` for a ragged/missing cell).
pub struct TableLayout {
    /// Left edge (device px, from the line origin) of each column's cell box.
    col_x: Vec<f32>,
    /// Content width (device px) available to each column's text (inside cell padding).
    col_w: Vec<f32>,
    aligns: Vec<Align>,
    /// Header then body row heights (device px), including vertical cell padding.
    row_heights: Vec<f32>,
    /// Grid height (device px) of the delimiter buffer-line. Zero: the delimiter row is
    /// not drawn in grid mode (the header's background fill already sets it apart), so the
    /// header sits directly above the first body row with no gap.
    delim_height: f32,
    /// Shaped cell layouts, parallel to `row_heights` rows × columns.
    row_layouts: GridLayouts,
}

impl TableLayout {
    /// Row index into `row_heights`/`row_layouts` for a header/body kind (`None` for the
    /// delimiter, which has no cells).
    fn row_index(kind: RowKind) -> Option<usize> {
        match kind {
            RowKind::Header => Some(0),
            RowKind::Body(i) => Some(i + 1),
            RowKind::Delimiter => None,
        }
    }

    /// Height (device px) of the line playing `kind` in this table.
    fn height(&self, kind: RowKind) -> f32 {
        match Self::row_index(kind) {
            Some(r) => self.row_heights.get(r).copied().unwrap_or(0.0),
            None => self.delim_height,
        }
    }

    /// Map a click at cell-local X `lx` (device px from the line origin, i.e.
    /// `click_x - pad_x`) on the grid row playing `kind` to a buffer offset inside the
    /// clicked cell. Picks the column by `col_x` boundaries (clamping to the grid on
    /// either side), then hit-tests within the cell's own layout → display offset →
    /// buffer offset via its `SegmentMap`; an empty cell lands just inside its opening
    /// pipe. Returns `None` for the (zero-height, caret-free) delimiter row.
    fn hit_test(&self, kind: RowKind, lx: f32, cell_pad_x: f32) -> Option<usize> {
        let row_idx = Self::row_index(kind)?;
        let row = self.row_layouts.get(row_idx)?;
        let cols = self.col_x.len();
        if cols == 0 {
            return None;
        }
        let mut col = self
            .col_x
            .partition_point(|&cx| cx <= lx)
            .saturating_sub(1)
            .min(cols - 1);
        // Ragged row: clamp down to the last cell actually present in this row.
        while col > 0 && row.get(col).map(|c| c.is_none()).unwrap_or(true) {
            col -= 1;
        }
        let slot = row.get(col)?.as_ref()?;
        if let Some(landing) = slot.empty_landing {
            return Some(landing);
        }
        let align = self.aligns.get(col).copied().unwrap_or(Align::Left);
        let extra = (self.col_w[col] - slot.layout.width()).max(0.0);
        let shift = match align {
            Align::Left => 0.0,
            Align::Right => extra,
            Align::Center => extra / 2.0,
        };
        let local_x = (lx - (self.col_x[col] + cell_pad_x + shift)).max(0.0);
        let display_off = Cursor::from_point(&slot.layout, local_x, 0.0).index();
        Some(slot.map.display_to_buffer(display_off))
    }
}

/// Per-table grid-layout cache: keyed on `(version, block start, scale, font, width)`,
/// swept each frame like the other caches. `Rc`-wrapped so every row of one table shares
/// one build and a cache hit is a refcount bump.
pub type TableCache = SweptCache<Rc<TableLayout>>;

/// Horizontal / vertical padding (logical px) inside a table cell.
const TABLE_CELL_PAD_X: f32 = 8.0;
const TABLE_CELL_PAD_Y: f32 = 4.0;
/// Cell border width (logical px).
const TABLE_BORDER: f32 = 1.0;
/// A single column may grow to this multiple of its equal share of the width before it
/// is capped (wider content then wraps within the column).
const TABLE_COL_CAP_FACTOR: f32 = 2.5;
/// Grid-render only tables under these limits; larger tables fall through to raw pipe
/// text so the O(cells) pre-pass + cache can't blow up on pathological input.
const TABLE_MAX_BODY_ROWS: usize = 200;
const TABLE_MAX_CELLS: usize = 1000;

/// Whether a table is small enough to grid-render (else it renders as raw pipe text).
fn table_grid_ok(t: &TableInfo) -> bool {
    t.body.len() <= TABLE_MAX_BODY_ROWS
        && (t.body.len() + 1).saturating_mul(t.ncols.max(1)) <= TABLE_MAX_CELLS
}

/// Content hash keying a `TableLayout`: same text + width + font ⇒ identical grid.
fn table_key(
    version: u64,
    block_start: usize,
    scale: f32,
    font_size: f32,
    max_advance: f32,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    version.hash(&mut h);
    block_start.hash(&mut h);
    scale.to_bits().hash(&mut h);
    font_size.to_bits().hash(&mut h);
    max_advance.to_bits().hash(&mut h);
    h.finish()
}

/// Measure a table's column widths (a per-cell pre-pass), then re-lay each cell wrapped
/// to its column to get row heights, returning the full `TableLayout`. Columns are capped
/// at a multiple of their equal share and, if the total still overflows `max_advance`,
/// scaled down proportionally so the grid always fits the content width.
fn build_table_layout(
    engine: &mut TextEngine,
    snapshot: &RenderSnapshot,
    theme: &EditorTheme,
    table: &TableInfo,
    line_styles: &[Vec<StyledRegion>],
    params: &LayoutParams,
    max_advance: f32,
) -> TableLayout {
    let LayoutParams {
        scale,
        base_font_size,
        line_height,
        fg,
        ..
    } = *params;
    let ncols = table.ncols.max(1);
    let cell_pad_x = TABLE_CELL_PAD_X * scale;
    let cell_pad_y = TABLE_CELL_PAD_Y * scale;
    let border = TABLE_BORDER * scale;
    let n_lines = snapshot.line_count();

    // Build every cell's (text, runs) once — header cells get a bold base run under
    // their content colors. Cells absent (ragged rows) stay `None`.
    let rows: Vec<&TableRow> = std::iter::once(&table.header)
        .chain(table.body.iter())
        .collect();
    let mut grid: GridCells = Vec::with_capacity(rows.len());
    let mut natural = vec![0f32; ncols];
    for (r, row) in rows.iter().enumerate() {
        let line_idx = snapshot
            .rope
            .byte_to_line(row.line.start)
            .min(n_lines.saturating_sub(1));
        let empty: Vec<StyledRegion> = Vec::new();
        let styles = line_styles.get(line_idx).unwrap_or(&empty);
        let is_header = r == 0;
        let mut rowdata = Vec::with_capacity(ncols);
        // Ragged rows have fewer cells than `ncols`, so index rather than iterate cells.
        #[allow(clippy::needless_range_loop)]
        for c in 0..ncols {
            match row.cells.get(c) {
                Some(cell) => {
                    let (text, mut runs, map, code_ranges) =
                        build_cell_render(snapshot, theme, cell.content.clone(), styles);
                    if is_header && !text.is_empty() {
                        // Bold the whole header cell; content color runs still win their spans.
                        let mut base = StyleRun::new(0..text.len(), fg);
                        base.bold = true;
                        runs.insert(0, base);
                    }
                    let w = engine
                        .build_line(&text, scale, base_font_size, line_height, fg, None, &runs)
                        .width();
                    natural[c] = natural[c].max(w);
                    let empty_landing = (cell.content.start >= cell.content.end)
                        .then(|| cell_caret_landing(&snapshot.rope, &cell.content));
                    rowdata.push(Some(MeasuredCell {
                        text,
                        runs,
                        map,
                        empty_landing,
                        code_ranges,
                    }));
                }
                None => rowdata.push(None),
            }
        }
        grid.push(rowdata);
    }

    // Column widths: cap each at a multiple of the equal share, then scale the set down
    // if the total (plus per-cell padding + borders) still overflows the content width.
    let overhead = ncols as f32 * 2.0 * cell_pad_x + (ncols as f32 + 1.0) * border;
    let avail = (max_advance - overhead).max(1.0);
    let col_cap = (avail / ncols as f32) * TABLE_COL_CAP_FACTOR;
    let mut col_w: Vec<f32> = natural.iter().map(|&w| w.min(col_cap).max(1.0)).collect();
    let sum: f32 = col_w.iter().sum();
    if sum > avail {
        let s = avail / sum;
        for w in &mut col_w {
            *w *= s;
        }
    }

    // Column x-origins (prefix sum of box widths + border).
    let mut col_x = Vec::with_capacity(ncols);
    let mut x = border;
    for &w in &col_w {
        col_x.push(x);
        x += w + 2.0 * cell_pad_x + border;
    }

    // Re-lay each cell wrapped to its column width, tracking the tallest cell per row.
    let mut row_layouts: GridLayouts = Vec::with_capacity(rows.len());
    let mut row_heights = Vec::with_capacity(rows.len());
    for rowdata in grid {
        let mut layouts = Vec::with_capacity(ncols);
        let mut max_h = 0f32;
        for (c, cell) in rowdata.into_iter().enumerate() {
            match cell {
                Some(MeasuredCell {
                    text,
                    runs,
                    map,
                    empty_landing,
                    code_ranges,
                }) => {
                    let layout = Rc::new(engine.build_line(
                        &text,
                        scale,
                        base_font_size,
                        line_height,
                        fg,
                        Some(col_w[c].max(1.0)),
                        &runs,
                    ));
                    max_h = max_h.max(layout.height());
                    layouts.push(Some(CellSlot {
                        layout,
                        map,
                        empty_landing,
                        code_ranges,
                    }));
                }
                None => layouts.push(None),
            }
        }
        // An empty row still gets one line's height so the grid never collapses to 0.
        if max_h <= 0.0 {
            max_h = base_font_size * line_height * scale;
        }
        row_heights.push(max_h + 2.0 * cell_pad_y);
        row_layouts.push(layouts);
    }

    TableLayout {
        col_x,
        col_w,
        aligns: table.aligns.clone(),
        row_heights,
        delim_height: 0.0,
        row_layouts,
    }
}

/// Vertical padding (logical px) above and below an image block.
const IMG_VPAD: f32 = 8.0;
/// Block height (logical px) reserved for a loading/failed image placeholder.
const IMG_PLACEHOLDER_H: f32 = 120.0;
/// Placeholder box size (logical px) reserved inline for an image that hasn't loaded
/// yet (or failed) — small, since the real size is unknown until decode.
const INLINE_IMG_PLACEHOLDER_W: f32 = 80.0;
const INLINE_IMG_PLACEHOLDER_H: f32 = 20.0;

/// A standalone image to paint on a line: its load state plus the sizing the draw
/// pass needs. `dest_*` are device px on screen; `nat_*` are the image's intrinsic
/// pixel dimensions (draw_image paints at native size, so the transform scales
/// `dest/nat`).
struct ImageBlock {
    alt: String,
    kind: ImageBlockKind,
}

enum ImageBlockKind {
    Loaded {
        brush: ImageBrush,
        nat_w: f32,
        nat_h: f32,
        dest_w: f32,
        dest_h: f32,
    },
    Loading,
    Failed,
}

/// Draw data for one inline image, indexed by its Parley inline-box `id`. The box's
/// on-line position + size come from the laid-out `PositionedInlineBox`; this carries
/// only what's needed to paint there — the brush + intrinsic pixel size for a loaded
/// image, or `None` for a loading/failed/absent placeholder rect.
struct InlineImageDraw {
    /// `(brush, nat_w, nat_h)` for a loaded image; `None` paints a placeholder box.
    brush: Option<(ImageBrush, f32, f32)>,
    /// Device-px to shift the drawn image DOWN from its laid-out box position. Parley
    /// bottom-aligns an inline box to the text baseline; math has content below the
    /// baseline (descenders), so its image is shifted down by its descent to sit the
    /// math baseline on the text baseline. `0.0` for ordinary images.
    baseline_offset: f32,
}

/// Inline git-diff decorations for one real line (added-line bg + word ranges).
#[derive(Default)]
struct LineDiff {
    is_addition: bool,
    /// Display byte ranges of word-level additions within the line.
    inline: Vec<Range<usize>>,
}

/// A deleted (ghost) line, rendered from the HEAD snapshot above the buffer line
/// it was removed before. Inert: not in the hit-test/cursor index.
struct Ghost {
    layout: parley::Layout<Brush>,
    height: f32,
    /// Display byte ranges of word-level deletions within the ghost line.
    inline: Vec<Range<usize>>,
}

/// The four translucent inline-diff colors, baked from the theme at build time.
struct DiffColors {
    added_bg: Color,
    added_inline: Color,
    deleted_bg: Color,
    deleted_inline: Color,
}

/// Stable per-frame layout constants: padding and font in logical px, plus the doc's
/// horizontal region (`content_x0`/`content_w`), `scale`, and the theme foreground baked
/// to a `Color`. Bundled so `DocLayout::build` isn't a wall of `f32`s; all are fixed for a
/// given surface and theme. Viewport state (`measure_to_y`) is deliberately kept a
/// separate arg.
pub struct LayoutParams {
    /// Left edge (device px) of the document region within the surface. `0.0` unless a
    /// panel (e.g. the outline) claims a strip; the body centers within
    /// `[content_x0, content_x0 + content_w]` and every glyph/caret/hit-test inherits it.
    pub content_x0: f32,
    /// Width (device px) of the document region — the surface width minus any panel inset.
    pub content_w: f32,
    pub scale: f32,
    pub pad_x: f32,
    pub pad_top: f32,
    pub pad_bottom: f32,
    pub base_font_size: f32,
    pub line_height: f32,
    pub fg: Color,
}

/// Build the ghost (deleted) lines that render above buffer line `new_line`,
/// laying out each from the HEAD snapshot. `usize::MAX` cursor keeps every marker
/// hidden in ghosts (the cursor is never "on" a ghost line).
/// Max ghost (deleted) lines shaped per block. A huge deletion only *shapes* the lines
/// nearest the real line (the ones that can be on screen); the rest contribute estimated
/// height only — bounds per-frame shaping regardless of deletion size.
const GHOST_SHAPE_CAP: usize = 300;

/// Cheap estimate (no shaping) of a deletion hunk's block height above `new_line`:
/// deleted-line count × body row height. Used for off-screen (virtualized) lines.
fn estimate_ghost_height(diff: Option<&DiffState>, new_line: usize, min_row: f32) -> f32 {
    diff.and_then(|d| d.ghost_lines_before(new_line))
        .map(|r| r.len() as f32 * min_row)
        .unwrap_or(0.0)
}

/// Build the ghost (deleted) lines that render above buffer line `new_line`. Returns the
/// shaped ghosts (bottom-aligned to the real line by the draw pass) and the block's TOTAL
/// height — shaped rows plus the estimated height of any lines beyond `GHOST_SHAPE_CAP`.
fn build_ghosts_before(
    engine: &mut TextEngine,
    diff: Option<&DiffState>,
    new_line: usize,
    theme: &EditorTheme,
    params: &LayoutParams,
    max_advance: f32,
    min_row: f32,
) -> (Vec<Ghost>, f32) {
    let LayoutParams {
        scale,
        base_font_size,
        line_height,
        fg,
        ..
    } = *params;
    let Some(d) = diff else {
        return (Vec::new(), 0.0);
    };
    let Some(old_range) = d.ghost_lines_before(new_line) else {
        return (Vec::new(), 0.0);
    };
    let old = &d.old_snapshot;
    // Shape only the last GHOST_SHAPE_CAP lines (nearest the real line, most likely on
    // screen); estimate the height of the earlier ones.
    let count = old_range.len();
    let shape_from = old_range.start + count.saturating_sub(GHOST_SHAPE_CAP);
    let mut block_height = (shape_from - old_range.start) as f32 * min_row;
    let mut out = Vec::new();
    for old_line in shape_from..old_range.end {
        if old_line >= old.line_count() {
            break;
        }
        // Only the few visible ghost lines need styling, so compute per line instead
        // of bucketing the entire HEAD snapshot on every rebuild.
        let styles = old.tree_styles_for_line(old_line);
        let lr = build_line_render(
            old,
            old_line,
            theme,
            base_font_size,
            usize::MAX,
            &styles,
            &[],
            None,
            &[],
            &[],
        );
        let layout = engine.build_line_hanging(
            &lr.text,
            scale,
            lr.font_size,
            line_height,
            fg,
            Some(max_advance),
            &lr.runs,
            lr.content_start,
            &[],
        );
        let line_start = old.line_markers(old_line).range.start;
        let inline = d
            .old_inline_changes(old_line)
            .map(|changes| map_changes_to_display(&lr.map, line_start, changes))
            .unwrap_or_default();
        block_height += layout.height();
        out.push(Ghost {
            height: layout.height(),
            layout,
            inline,
        });
    }
    (out, block_height)
}

/// Map line-relative inline-change byte ranges through a line's segment map into
/// non-empty display ranges. `base` is the line's buffer start byte.
fn map_changes_to_display(
    map: &SegmentMap,
    base: usize,
    changes: &[InlineChange],
) -> Vec<Range<usize>> {
    changes
        .iter()
        .filter_map(|c| {
            let dr = map.buffer_range_to_display(base + c.range.start..base + c.range.end);
            (!dr.is_empty()).then_some(dr)
        })
        .collect()
}

/// Build the image block for a standalone image and its device-px block height (used
/// as the line height). Fits to `content_w`, preserving aspect: an image at least as
/// wide as the body fills the full content width; a narrower one keeps its intrinsic
/// size (never upscaled). Height follows from the aspect — no cap, so wide/landscape
/// images aren't shrunk below the body width. Loading/failed states get a placeholder.
fn build_image_block(
    images: &ImageCache,
    img: &ImageRef,
    content_w: f32,
    scale: f32,
    vpad: f32,
) -> (ImageBlock, f32) {
    match images.get(&img.url) {
        Some(ImageState::Loaded(loaded)) => {
            // Intrinsic (logical display) size in device px (×scale). For SVG this is
            // the vector's logical size, decoupled from the supersampled raster dims.
            let iw = loaded.display_w * scale;
            let ih = loaded.display_h * scale;
            let dw = iw.min(content_w);
            let dh = if iw > 0.0 { ih * dw / iw } else { 0.0 };
            let block = ImageBlock {
                alt: img.alt.clone(),
                kind: ImageBlockKind::Loaded {
                    brush: loaded.brush.clone(),
                    nat_w: loaded.width as f32,
                    nat_h: loaded.height as f32,
                    dest_w: dw,
                    dest_h: dh,
                },
            };
            (block, dh + 2.0 * vpad)
        }
        Some(ImageState::Failed) => (
            ImageBlock {
                alt: img.alt.clone(),
                kind: ImageBlockKind::Failed,
            },
            IMG_PLACEHOLDER_H * scale,
        ),
        // Loading, or not yet spawned: a pending placeholder box.
        Some(ImageState::Loading) | None => (
            ImageBlock {
                alt: img.alt.clone(),
                kind: ImageBlockKind::Loading,
            },
            IMG_PLACEHOLDER_H * scale,
        ),
    }
}

/// Resolve a line's inline images to `(inline_boxes, draws)`: for each, the box size to
/// reserve in the layout (natural device size, shrunk to fit `max_advance`) and its draw
/// spec. Loading/failed/absent images get a small fixed placeholder box. Every referenced
/// URL is collected into `image_urls` (deduped) so the load pass fetches it.
fn resolve_inline_images(
    inline_images: &[InlineImageRef],
    images: &ImageCache,
    image_urls: &mut Vec<String>,
    max_advance: f32,
    scale: f32,
) -> (Vec<(usize, f32, f32)>, Vec<InlineImageDraw>) {
    let mut inline_boxes: Vec<(usize, f32, f32)> = Vec::new();
    let draws: Vec<InlineImageDraw> = inline_images
        .iter()
        .map(|ii| {
            if !image_urls.iter().any(|u| u == &ii.url) {
                image_urls.push(ii.url.clone());
            }
            let (w, h, draw) = match images.get(&ii.url) {
                Some(ImageState::Loaded(loaded)) => {
                    let mut w = loaded.display_w * scale;
                    let mut h = loaded.display_h * scale;
                    if w > max_advance && w > 0.0 {
                        h *= max_advance / w;
                        w = max_advance;
                    }
                    let brush = Some((
                        loaded.brush.clone(),
                        loaded.width as f32,
                        loaded.height as f32,
                    ));
                    (
                        w,
                        h,
                        InlineImageDraw {
                            brush,
                            baseline_offset: 0.0,
                        },
                    )
                }
                _ => (
                    INLINE_IMG_PLACEHOLDER_W * scale,
                    INLINE_IMG_PLACEHOLDER_H * scale,
                    InlineImageDraw {
                        brush: None,
                        baseline_offset: 0.0,
                    },
                ),
            };
            inline_boxes.push((ii.display_offset, w, h));
            draw
        })
        .collect();
    (inline_boxes, draws)
}

/// Resolve a line's inline `$…$` math spans to `(inline_boxes, draws)`, mirroring
/// [`resolve_inline_images`]. Each span's `(latex, size, color)` becomes a [`math::MathJob`]
/// keyed into the shared image cache; a loaded render supplies the box size + a
/// `baseline_offset` (its descent below the math baseline, so it sits on the text baseline),
/// and loading/failed spans get a placeholder box. Jobs are collected into `math_sources`
/// (deduped) for the shell to render off-thread.
#[cfg(feature = "math")]
fn resolve_inline_math(
    inline_math: &[InlineMathRef],
    images: &ImageCache,
    math_sources: &mut Vec<(String, math::MathJob)>,
    font_px: f32,
    fg: (f32, f32, f32),
    max_advance: f32,
    scale: f32,
) -> (Vec<(usize, f32, f32)>, Vec<InlineImageDraw>) {
    let mut inline_boxes: Vec<(usize, f32, f32)> = Vec::new();
    let draws: Vec<InlineImageDraw> = inline_math
        .iter()
        .map(|im| {
            let job = math::MathJob {
                latex: im.latex.clone(),
                display: false,
                font_px,
                scale,
                fg,
            };
            let key = math::key_for(&job);
            if !math_sources.iter().any(|(k, _)| k == &key) {
                math_sources.push((key.clone(), job));
            }
            let (w, h, draw) = match images.get(&key) {
                Some(ImageState::Loaded(loaded)) => {
                    let mut w = loaded.display_w * scale;
                    let mut h = loaded.display_h * scale;
                    // Descent (px below the math baseline) → shift the image down so the
                    // math baseline lands on the text baseline Parley aligns the box to.
                    let descent =
                        (loaded.display_h - loaded.baseline.unwrap_or(loaded.display_h)) * scale;
                    if w > max_advance && w > 0.0 {
                        let s = max_advance / w;
                        w = max_advance;
                        h *= s;
                    }
                    let brush = Some((
                        loaded.brush.clone(),
                        loaded.width as f32,
                        loaded.height as f32,
                    ));
                    (
                        w,
                        h,
                        InlineImageDraw {
                            brush,
                            baseline_offset: descent,
                        },
                    )
                }
                _ => (
                    INLINE_IMG_PLACEHOLDER_W * scale,
                    INLINE_IMG_PLACEHOLDER_H * scale,
                    InlineImageDraw {
                        brush: None,
                        baseline_offset: 0.0,
                    },
                ),
            };
            inline_boxes.push((im.display_offset, w, h));
            draw
        })
        .collect();
    (inline_boxes, draws)
}

/// Splice an in-progress IME `preedit` string into a line's display `text` at display
/// byte `caret_disp`, returning the new text plus the shifted style runs with an added
/// underline run over the composition span. Runs starting at/after the caret shift right
/// by `preedit.len()` (the caret sits between clusters, so a straddling run is rare and
/// the same shift keeps it aligned). Pure (no layout) so it's unit-testable headlessly.
fn splice_preedit(
    text: &str,
    runs: &[StyleRun],
    caret_disp: usize,
    preedit: &str,
    fg: Color,
) -> (String, Vec<StyleRun>) {
    let spliced_text = format!("{}{}{}", &text[..caret_disp], preedit, &text[caret_disp..]);
    let shift = preedit.len();
    let mut spliced_runs: Vec<StyleRun> = runs
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if r.range.start >= caret_disp {
                r.range = r.range.start + shift..r.range.end + shift;
            }
            r
        })
        .collect();
    // Underline the composition; keep it mono so metrics match the body (stable caret math).
    let mut under = StyleRun::new(caret_disp..caret_disp + shift, fg);
    under.underline = true;
    under.mono = true;
    spliced_runs.push(under);
    (spliced_text, spliced_runs)
}

/// Prefix-sum tops for `heights`: `out[0] = pad_top`, `out[i+1] = out[i] +
/// heights[i]`. Length is `heights.len() + 1`.
fn compute_tops(heights: &[f32], pad_top: f32) -> Vec<f32> {
    let mut tops = Vec::with_capacity(heights.len() + 1);
    let mut y = pad_top;
    tops.push(y);
    for &h in heights {
        y += h;
        tops.push(y);
    }
    tops
}

/// Extra device px measured past the viewport bottom so gentle wheel-scrolling stays
/// on materialized lines before a rebuild extends the range.
const MEASURE_OVERSCAN_PX: f32 = 800.0;
/// Extra lines materialized past the cursor line (so scroll-to-cursor lands on a real
/// layout even when the cursor briefly outruns the scroll position).
const MEASURE_OVERSCAN_LINES: usize = 30;

pub struct DocLayout {
    layouts: Vec<Rc<parley::Layout<Brush>>>,
    /// Per-line render results (shared with the RenderCache via `Rc`), parallel to
    /// `layouts`. Their `.map` is the display↔buffer map for cursor/click math.
    renders: Vec<Rc<LineRender>>,
    /// Per-line buffer byte ranges (incl. trailing newline), parallel to `layouts`.
    line_ranges: Vec<Range<usize>>,
    /// Per-line inline git-diff decorations, parallel to `layouts`.
    line_diffs: Vec<LineDiff>,
    /// Ghost (deleted) lines rendered *above* each real line, parallel to `layouts`.
    ghosts: Vec<Vec<Ghost>>,
    /// Per-line standalone-image block, parallel to `layouts`. `Some` overrides the
    /// line's height with the image (or placeholder) block and is painted by
    /// `draw_images`; `None` for ordinary lines.
    image_blocks: Vec<Option<ImageBlock>>,
    /// Per-line inline-image draw data, parallel to `layouts`. Each inner vec is indexed
    /// by the Parley inline-box `id` (= its order on the line). Empty for lines without
    /// inline images.
    inline_draws: Vec<Vec<InlineImageDraw>>,
    /// Distinct image URLs across the materialized lines, for the shell to kick off
    /// loads (diffed against the shared cache).
    image_urls: Vec<String>,
    /// `(cache key, source)` of each materialized mermaid diagram, for the shell to
    /// kick off off-thread rendering (mirrors `image_urls`).
    #[cfg(feature = "mermaid")]
    mermaid_sources: Vec<(String, String)>,
    /// `(cache key, job)` of each materialized math render (block + inline), for the shell
    /// to kick off off-thread rendering.
    #[cfg(feature = "math")]
    math_sources: Vec<(String, math::MathJob)>,
    /// Vertical padding (device px) above/below an image block, and the placeholder
    /// box colors — baked from `scale`/theme at build so the draw pass is self-contained.
    img_vpad: f32,
    img_label_size: f32,
    image_border: Color,
    image_bg: Color,
    /// Background chip painted behind inline `code` spans.
    code_bg: Color,
    /// Per-line grid-table row, parallel to `layouts`. `Some((layout, kind))` means the
    /// line renders as a table row (its text layout is empty); `None` otherwise.
    table_lines: Vec<Option<(Rc<TableLayout>, RowKind)>>,
    /// Baked table chrome (device px + theme colors) for the grid draw path.
    table_cell_pad_x: f32,
    table_cell_pad_y: f32,
    table_border: f32,
    table_border_color: Color,
    table_bg: Color,
    table_header_bg: Color,
    /// Per-line x-offsets (from the line origin) of each blockquote gutter rule, parallel
    /// to `layouts`. Empty for non-quote lines. Painted as continuous vertical rects.
    quote_bars: Vec<Vec<f32>>,
    /// Total ghost-block height above each real line, parallel to `layouts`.
    ghost_height: Vec<f32>,
    /// Deleted (ghost) rows at the very end of the document — a deletion hunk anchored
    /// past the last line, which has no host line to hang above. Drawn below the last
    /// real line; without this, trailing deletions (esp. with no final newline) vanish.
    trailing_ghosts: Vec<Ghost>,
    trailing_ghost_height: f32,
    /// Top y of each line's *ghost block*; the real line begins at
    /// `tops[i] + ghost_height[i]`. Length `layouts.len() + 1`. Device px.
    tops: Vec<f32>,
    /// The materialized (fully laid-out) band is `[measured_start, measured_count)`.
    /// Lines outside it are height-estimated placeholders. `measured_start` is 0 and
    /// `measured_count` is `line_count()` when the whole document was laid out.
    measured_start: usize,
    measured_count: usize,
    /// `(line, display byte offset)` of the IME composition caret within the spliced
    /// caret line, when a preedit is active. Drives the drawn caret + candidate popup.
    preedit_caret: Option<(usize, usize)>,
    diff_colors: DiffColors,
    /// Width (device px) and color of the painted blockquote gutter rules.
    quote_bar_width: f32,
    quote_bar_color: Color,
    pub scroll_y: f32,
    /// Right edge of the document region in device px (`content_x0 + content_w`, the
    /// surface width unless a panel claims a strip). Full-width diff row backgrounds fill
    /// to here so they stop at the panel edge, not the window edge.
    width: f32,
    pad_top: f32,
    pad_bottom: f32,
    pad_x: f32,
}

impl DocLayout {
    /// Lay out every line of `snapshot` at the current width. `pad_*`/`font_size`
    /// are logical px; `scale` converts to device px (matching the layouts).
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        engine: &mut TextEngine,
        cache: &mut LineCache,
        render_cache: &mut RenderCache,
        height_cache: &mut HeightCache,
        table_cache: &mut TableCache,
        version: u64,
        snapshot: &RenderSnapshot,
        theme: &EditorTheme,
        diff: Option<&DiffState>,
        github: Option<&GithubRenderData>,
        images: &ImageCache,
        cursor_offset: usize,
        params: &LayoutParams,
        // In-progress IME composition to splice into the caret line (render-only; no
        // buffer mutation). `None` outside composition and on the headless snapshot path.
        preedit: Option<&PreeditView>,
        // Materialize (fully lay out) a band of lines around the scroll anchor: from
        // `anchor_line - overscan` downward until the materialized height covers
        // `viewport_h` + overscan on both sides. Lines above and below the band are
        // cheaply height-*estimated* (head/tail virtualization) — O(visible) regardless
        // of scroll depth. `anchor_line = 0` + `viewport_h = f32::INFINITY` lays out the
        // whole document (the headless/first-open path).
        anchor_line: usize,
        viewport_h: f32,
        // Sorted, disjoint line ranges to collapse to zero height (heading folds). Hidden
        // lines are skipped: never materialized, `heights[i] = 0`, so the prefix-sum `tops`
        // ties across them and the whole virtualization stack excludes them for free.
        folds: &[Range<usize>],
        // The current selection (collapsed = an empty range at the caret). A mermaid fold
        // reveals its raw source whenever the selection OVERLAPS it, so a highlight dragged
        // through the diagram shows (and copies) the source, and reveal doesn't flip-flop as
        // the drag head enters/leaves.
        selection: &Range<usize>,
        // Inline `$…$` math spans per line (empty when the `math` feature is off).
        math_spans: &HashMap<usize, Vec<MathSpan>>,
    ) -> Self {
        let LayoutParams {
            content_x0,
            content_w,
            scale,
            pad_x,
            pad_top,
            pad_bottom,
            base_font_size,
            line_height,
            fg,
        } = *params;
        // Cap the body width for readability and center it *within the content region*
        // `[content_x0, content_x0 + content_w]`: the left margin is the base padding,
        // widened to a centering inset once the region exceeds MAX_CONTENT_WIDTH plus that
        // padding. `left` is the draw origin and both margins for `max_advance`; because
        // caret/hit-test/draw all read `self.pad_x = left`, shrinking/shifting the region
        // moves them for free.
        let inset = (content_w - MAX_CONTENT_WIDTH * scale).max(0.0) / 2.0;
        let left = content_x0 + inset.max(pad_x * scale);
        let max_advance = (content_w - 2.0 * (left - content_x0)).max(1.0);
        // Right edge of the doc region (stored as `self.width`), captured before the local
        // `content_w` below is rebound to the image content width (= max_advance).
        let content_right = content_x0 + content_w;
        let n = snapshot.line_count();
        cache.begin();
        render_cache.begin();
        height_cache.begin();
        table_cache.begin();
        // Off-screen height estimate: average glyph advance (device px/char) from one
        // unwrapped sample line, and the body row height. Predicts soft-wrap row count
        // from a line's byte length without laying it out (see writ-virtualization-plan).
        let cal_text = "the quick brown fox jumps over the lazy dog and then runs along";
        let cal = engine.build_line(cal_text, scale, base_font_size, line_height, fg, None, &[]);
        let k = (cal.width() / cal_text.chars().count().max(1) as f32).max(0.1);
        let min_row = base_font_size * line_height * scale;
        let cursor_line = snapshot
            .rope
            .byte_to_line(cursor_offset.min(snapshot.rope.len_bytes()));
        // Line-based table membership (start of the cursor's line), matching
        // `build_line_render`'s reveal predicate so cache keys agree at row edges.
        let cursor_line_start = snapshot.line_byte_range(cursor_line).start;
        // Shared placeholders for estimated (un-laid-out) lines. They're never drawn or
        // cursor-queried (only visible lines are, and those are always materialized), so
        // an empty layout / identity render is safe; `Rc::clone` keeps it O(1).
        let empty_layout: Rc<parley::Layout<Brush>> = Rc::new(parley::Layout::new());
        let empty_render: Rc<LineRender> = Rc::new(LineRender {
            text: String::new(),
            font_size: base_font_size,
            runs: Vec::new(),
            map: SegmentMap::identity("", 0).1,
            content_start: 0,
            quote_bar_bytes: Vec::new(),
            is_hr: false,
            image: None,
            code_ranges: Vec::new(),
            inline_images: Vec::new(),
            inline_math: Vec::new(),
            table: None,
        });
        // The materialized band: [measure_from_line, measured_count). Lines before
        // `measure_from_line` (head) and from `measured_count` on (tail) are estimated.
        // A fixed line count of overscan is always laid out above the anchor (for smooth
        // up-scroll); coverage below is measured in pixels from the anchor.
        let measure_from_line = anchor_line.saturating_sub(MEASURE_OVERSCAN_LINES);
        // Materialize downward from the anchor until this much height is covered — the
        // viewport plus one overscan band — so the viewport can never reach a placeholder.
        let band_span = viewport_h + MEASURE_OVERSCAN_PX;
        let mut band_y = 0.0f32; // height materialized from `anchor_line` down
        let mut past_band = false; // true once the band is tall enough; tail is estimated
        let mut measured_start = n; // first materialized line (n = none materialized yet)
        let mut measured_count = n; // first estimated tail line (n = materialized to end)
        let mut fold_idx = 0usize; // moving cursor into the sorted `folds` ranges
        let mut preedit_caret: Option<(usize, usize)> = None;
        // Bucket inline styles per line once (O(n + styles)) instead of the O(n²)
        // per-line `styles_in_range` scan — the dominant per-keystroke cost on large
        // docs. (Ghost lines style themselves lazily; see `build_ghosts_before`.)
        let line_styles = snapshot.inline_styles_by_line();
        // Scan mermaid fences once, not per line (a per-line info-string check allocates).
        #[cfg(feature = "mermaid")]
        let mermaid_blocks = snapshot.mermaid_blocks();
        let mut layouts = Vec::with_capacity(n);
        let mut renders = Vec::with_capacity(n);
        let mut line_ranges = Vec::with_capacity(n);
        let mut line_diffs = Vec::with_capacity(n);
        let mut ghosts = Vec::with_capacity(n);
        let mut ghost_height = Vec::with_capacity(n);
        let mut quote_bars: Vec<Vec<f32>> = Vec::with_capacity(n);
        let mut image_blocks: Vec<Option<ImageBlock>> = Vec::with_capacity(n);
        let mut inline_draws: Vec<Vec<InlineImageDraw>> = Vec::with_capacity(n);
        let mut table_lines: Vec<Option<(Rc<TableLayout>, RowKind)>> = Vec::with_capacity(n);
        let mut image_urls: Vec<String> = Vec::new();
        #[cfg(feature = "mermaid")]
        let mut mermaid_sources: Vec<(String, String)> = Vec::new();
        #[cfg(feature = "math")]
        let mut math_sources: Vec<(String, math::MathJob)> = Vec::new();
        // Scan display-math blocks once (openers can be off-screen).
        #[cfg(feature = "math")]
        let math_blocks = snapshot.math_blocks();
        // Glyph color for math = theme foreground (rgb), so it's legible on the dark theme.
        #[cfg(feature = "math")]
        let math_fg = {
            let c = fg.components;
            (c[0], c[1], c[2])
        };
        // Content width available to an image (device px), same basis as `max_advance`.
        let content_w = max_advance;
        let img_vpad = IMG_VPAD * scale;
        // Each line's total height = its ghost block above + the real line.
        let mut heights = Vec::with_capacity(n);
        // Cursor component of a line's render key. Table rows share block-reveal state
        // (all flip together when the cursor enters/leaves the block) — except the row
        // the cursor actually sits on, which reveals its own markers. Three distinct
        // sentinels keep grid / reveal-here / reveal-elsewhere from colliding.
        let cursor_component = |i: usize, range: &Range<usize>| -> usize {
            let onkey = cursor_key_for(range, cursor_offset);
            match snapshot
                .table_row_at_line(i)
                .filter(|(t, _)| table_grid_ok(t))
            {
                Some((t, _)) => {
                    if !t.block.contains(&cursor_line_start) {
                        usize::MAX
                    } else if onkey != usize::MAX {
                        onkey
                    } else {
                        REVEAL_ELSEWHERE
                    }
                }
                None => onkey,
            }
        };
        // `i` indexes several parallel per-line inputs (styles, markers, diff).
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            // Folded line: zero height, no materialization, no band accounting. Placed
            // before the estimate/band logic so a fold never perturbs the measured band.
            while fold_idx < folds.len() && folds[fold_idx].end <= i {
                fold_idx += 1;
            }
            if fold_idx < folds.len() && folds[fold_idx].start <= i {
                heights.push(0.0);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                line_ranges.push(snapshot.line_byte_range(i));
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                quote_bars.push(Vec::new());
                image_blocks.push(None);
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Mermaid fence (diagram mode = caret outside the fence). Its non-anchor lines
            // collapse to zero height like a fold; the anchor line draws the diagram
            // (handled in the materialize path below). Inside the fence the caret reveals
            // the raw code — `mermaid` is `None` then and the line renders normally.
            #[cfg(feature = "mermaid")]
            let mermaid = {
                let ls = snapshot.line_byte_range(i).start;
                mermaid_blocks
                    .iter()
                    .find(|m| m.block.contains(&ls))
                    .filter(|m| {
                        let selected =
                            m.block.start < selection.end && selection.start < m.block.end;
                        !selected && !m.block.contains(&cursor_line_start)
                    })
            };
            #[cfg(feature = "mermaid")]
            if let Some(m) = &mermaid
                && i != m.anchor_line
            {
                heights.push(0.0);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                line_ranges.push(snapshot.line_byte_range(i));
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                quote_bars.push(Vec::new());
                image_blocks.push(None);
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Display-math block ($$…$$): same as mermaid — rendered mode when the caret
            // isn't inside and no selection overlaps; non-anchor lines collapse to zero
            // height, the anchor draws the image below.
            #[cfg(feature = "math")]
            let math_block = {
                let ls = snapshot.line_byte_range(i).start;
                math_blocks
                    .iter()
                    .find(|m| m.block.contains(&ls))
                    .filter(|m| {
                        let selected =
                            m.block.start < selection.end && selection.start < m.block.end;
                        !selected && !m.block.contains(&cursor_line_start)
                    })
            };
            #[cfg(feature = "math")]
            if let Some(m) = &math_block
                && i != m.anchor_line
            {
                heights.push(0.0);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                line_ranges.push(snapshot.line_byte_range(i));
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                quote_bars.push(Vec::new());
                image_blocks.push(None);
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Estimate the head (above the band) and the tail (once the band has covered
            // the viewport + overscan); materialize only the band in between.
            if i >= measure_from_line && !past_band && measured_start == n {
                measured_start = i;
            }
            let estimate = i < measure_from_line || past_band;
            if estimate {
                let range = snapshot.line_byte_range(i);
                // Prefer an exact height measured on a recent frame; fall back to the
                // char-count soft-wrap estimate for never-seen lines.
                let rkey = render_key(version, i, cursor_component(i, &range));
                let h = height_cache.get(rkey).unwrap_or_else(|| {
                    let byte_len = range.len().saturating_sub(1) as f32; // minus trailing '\n'
                    let est_rows = (byte_len * k / max_advance).ceil().max(1.0);
                    est_rows * min_row
                });
                // A deletion hunk anchored at this (off-screen) line still consumes height:
                // estimate it (deleted-line count × row height) so the scroll extent stays
                // stable instead of jumping when the line scrolls into the materialized band.
                let gh = estimate_ghost_height(diff, i, min_row);
                heights.push(gh + h);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                line_ranges.push(range);
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(gh);
                quote_bars.push(Vec::new());
                image_blocks.push(None);
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Mermaid anchor (materialized): draw the diagram (or placeholder) in place of
            // the fence text, collect its source for off-thread rendering, and account its
            // height in the band. Non-anchor fence lines were already zero-heighted above.
            #[cfg(feature = "mermaid")]
            if let Some(m) = &mermaid {
                let source = snapshot
                    .rope
                    .slice(
                        snapshot.rope.byte_to_char(m.content.start)
                            ..snapshot.rope.byte_to_char(m.content.end),
                    )
                    .to_string();
                let key = mermaid::key_for(&source);
                if !mermaid_sources.iter().any(|(k, _)| k == &key) {
                    mermaid_sources.push((key.clone(), source));
                }
                let img = ImageRef {
                    url: key,
                    alt: "mermaid diagram".to_string(),
                };
                let (block, block_h) = build_image_block(images, &img, content_w, scale, img_vpad);
                let range = snapshot.line_byte_range(i);
                height_cache.set(render_key(version, i, cursor_component(i, &range)), block_h);
                if i >= anchor_line {
                    band_y += block_h;
                    if band_y >= band_span {
                        past_band = true;
                        measured_count = i + 1;
                    }
                }
                heights.push(block_h);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                quote_bars.push(Vec::new());
                line_ranges.push(range);
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                image_blocks.push(Some(block));
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Display-math anchor (materialized): render the math and draw it centered in
            // place of the raw `$$…$$`; collect the job for the off-thread renderer.
            #[cfg(feature = "math")]
            if let Some(m) = &math_block {
                let latex = snapshot
                    .rope
                    .slice(
                        snapshot.rope.byte_to_char(m.content.start)
                            ..snapshot.rope.byte_to_char(m.content.end),
                    )
                    .to_string();
                let job = math::MathJob {
                    latex,
                    display: true,
                    font_px: base_font_size * 1.1,
                    scale,
                    fg: math_fg,
                };
                let key = math::key_for(&job);
                if !math_sources.iter().any(|(k, _)| k == &key) {
                    math_sources.push((key.clone(), job));
                }
                let img = ImageRef {
                    url: key,
                    alt: "math".to_string(),
                };
                let (block, block_h) = build_image_block(images, &img, content_w, scale, img_vpad);
                let range = snapshot.line_byte_range(i);
                height_cache.set(render_key(version, i, cursor_component(i, &range)), block_h);
                if i >= anchor_line {
                    band_y += block_h;
                    if band_y >= band_span {
                        past_band = true;
                        measured_count = i + 1;
                    }
                }
                heights.push(block_h);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                quote_bars.push(Vec::new());
                line_ranges.push(range);
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                image_blocks.push(Some(block));
                inline_draws.push(Vec::new());
                table_lines.push(None);
                continue;
            }
            // Ghost (deleted) lines rendered before this line, from the HEAD snapshot.
            // `gh` is the full block height (shaped rows + estimated overflow beyond the cap).
            let (line_ghosts, gh) =
                build_ghosts_before(engine, diff, i, theme, params, max_advance, min_row);

            // Line byte range + render key, computed once and shared by the render cache,
            // the height cache, and the diff mapping below. (`line_markers(i).range` is
            // byte-identical but far pricier — it runs full marker extraction per line.)
            let range = snapshot.line_byte_range(i);
            let rkey = render_key(version, i, cursor_component(i, &range));
            // Table membership (under the size cap). `TableCtx` carries the block range so
            // `build_line_render` can decide grid vs. reveal; the `&TableInfo` is re-fetched
            // below (only when grid mode engaged) to build the shared `TableLayout`.
            let table_ctx = snapshot
                .table_row_at_line(i)
                .filter(|(t, _)| table_grid_ok(t))
                .map(|(t, kind)| TableCtx {
                    block: t.block.clone(),
                    kind,
                });

            let extra = github.map(|g| g.extra_regions(i)).unwrap_or_default();
            // Byte ranges on this line to LaTeX-highlight: the content of a revealed inline
            // `$…$` span (caret inside it) or a revealed `$$…$$` block line (caret in the
            // block / a selection overlapping it — the same predicate that stops the block
            // from collapsing to an image). Empty when the `math` feature is off.
            #[cfg(feature = "math")]
            let latex_ranges: Vec<Range<usize>> = {
                let mut lr = Vec::new();
                if let Some(spans) = math_spans.get(&i) {
                    for s in spans {
                        if cursor_offset >= s.full_range.start && cursor_offset <= s.full_range.end
                        {
                            let a = s.content_range.start.max(range.start);
                            let b = s.content_range.end.min(range.end);
                            if a < b {
                                lr.push(a..b);
                            }
                        }
                    }
                }
                for m in &math_blocks {
                    let revealed = (m.block.start < selection.end && selection.start < m.block.end)
                        || m.block.contains(&cursor_line_start);
                    if revealed {
                        let a = m.content.start.max(range.start);
                        let b = m.content.end.min(range.end);
                        if a < b {
                            lr.push(a..b);
                        }
                    }
                }
                lr
            };
            #[cfg(not(feature = "math"))]
            let latex_ranges: Vec<Range<usize>> = Vec::new();
            // Reuse the cached render when nothing about this line changed. Lines with
            // GitHub extra regions bypass the cache (validation can change them without
            // a version bump); all others key on (version, line, cursor-on-line).
            let lr = if extra.is_empty() {
                let tc = table_ctx.clone();
                render_cache.get_or_build(rkey, || {
                    Rc::new(build_line_render(
                        snapshot,
                        i,
                        theme,
                        base_font_size,
                        cursor_offset,
                        &line_styles[i],
                        &[],
                        tc,
                        math_spans.get(&i).map_or(&[][..], |v| v.as_slice()),
                        &latex_ranges,
                    ))
                })
            } else {
                Rc::new(build_line_render(
                    snapshot,
                    i,
                    theme,
                    base_font_size,
                    cursor_offset,
                    &line_styles[i],
                    &extra,
                    table_ctx.clone(),
                    math_spans.get(&i).map_or(&[][..], |v| v.as_slice()),
                    &latex_ranges,
                ))
            };
            #[cfg_attr(not(feature = "math"), allow(unused_mut))]
            let (mut inline_boxes, mut line_inline_draws) = resolve_inline_images(
                &lr.inline_images,
                images,
                &mut image_urls,
                max_advance,
                scale,
            );
            // Inline `$…$` math shares the inline-box id space with images (id = index into
            // the combined slice), so its boxes/draws are appended after the image ones.
            #[cfg(feature = "math")]
            if !lr.inline_math.is_empty() {
                let (math_boxes, math_draws) = resolve_inline_math(
                    &lr.inline_math,
                    images,
                    &mut math_sources,
                    lr.font_size,
                    math_fg,
                    max_advance,
                    scale,
                );
                inline_boxes.extend(math_boxes);
                line_inline_draws.extend(math_draws);
            }
            let key = line_key(
                &lr.text,
                scale,
                lr.font_size,
                line_height,
                max_advance,
                &lr.runs,
                lr.content_start,
            );
            // Lines with inline images bypass the layout cache: the box sizes change when
            // an image loads (without any change to the cache key), so build fresh. They
            // are rare, so the cost is negligible.
            let layout = if inline_boxes.is_empty() {
                cache.get_or_build(key, || {
                    Rc::new(engine.build_line_hanging(
                        &lr.text,
                        scale,
                        lr.font_size,
                        line_height,
                        fg,
                        Some(max_advance),
                        &lr.runs,
                        lr.content_start,
                        &[],
                    ))
                })
            } else {
                Rc::new(engine.build_line_hanging(
                    &lr.text,
                    scale,
                    lr.font_size,
                    line_height,
                    fg,
                    Some(max_advance),
                    &lr.runs,
                    lr.content_start,
                    &inline_boxes,
                ))
            };
            // IME composition: on the caret's line, splice the preedit into a *fresh*
            // layout (unique per keystroke, so it bypasses the line cache). The original
            // `lr` (map/text) is kept for hit-testing; only the drawn layout changes.
            let layout = match preedit {
                Some(pv) if i == cursor_line => {
                    let caret_disp = lr.map.buffer_to_display(cursor_offset);
                    let (sp_text, sp_runs) =
                        splice_preedit(&lr.text, &lr.runs, caret_disp, pv.text, fg);
                    let caret_off = caret_disp + pv.cursor.map(|(s, _)| s).unwrap_or(pv.text.len());
                    preedit_caret = Some((i, caret_off));
                    Rc::new(engine.build_line_hanging(
                        &sp_text,
                        scale,
                        lr.font_size,
                        line_height,
                        fg,
                        Some(max_advance),
                        &sp_runs,
                        lr.content_start,
                        &[],
                    ))
                }
                _ => layout,
            };
            // Inline diff: map added word ranges (line-relative buffer bytes) through
            // this line's segment map into display ranges.
            let line_diff = match diff {
                Some(d) if d.is_addition(i) => {
                    let inline = d
                        .new_inline_changes(i)
                        .map(|changes| map_changes_to_display(&lr.map, range.start, changes))
                        .unwrap_or_default();
                    LineDiff {
                        is_addition: true,
                        inline,
                    }
                }
                _ => LineDiff::default(),
            };
            // Measure each blockquote gutter's x (on the first, un-hung row) so the draw
            // path can paint a continuous rule there. Only quote lines pay this.
            let bars: Vec<f32> = lr
                .quote_bar_bytes
                .iter()
                .filter_map(|&b| {
                    Cluster::from_byte_index(&layout, b).and_then(|c| c.visual_offset())
                })
                .collect();
            // Grid table row: build (or reuse) the shared `TableLayout` and take the row's
            // height from it; the line's own text layout is empty (hidden).
            let table_line: Option<(Rc<TableLayout>, RowKind)> = if lr.table.is_some() {
                snapshot
                    .table_row_at_line(i)
                    .filter(|(t, _)| table_grid_ok(t))
                    .map(|(t, kind)| {
                        let tkey =
                            table_key(version, t.block.start, scale, base_font_size, max_advance);
                        let tl = table_cache.get_or_build(tkey, || {
                            Rc::new(build_table_layout(
                                engine,
                                snapshot,
                                theme,
                                t,
                                &line_styles,
                                params,
                                max_advance,
                            ))
                        });
                        (tl, kind)
                    })
            } else {
                None
            };
            // Standalone image: the block (image or placeholder) sets the line height in
            // place of the (empty) text layout, and the URL is collected for loading.
            let (image_block, line_h) = if let Some((tl, kind)) = &table_line {
                (None, tl.height(*kind))
            } else {
                match &lr.image {
                    Some(img) => {
                        if !image_urls.iter().any(|u| u == &img.url) {
                            image_urls.push(img.url.clone());
                        }
                        let (block, block_h) =
                            build_image_block(images, img, content_w, scale, img_vpad);
                        (Some(block), block_h)
                    }
                    None => (None, layout.height()),
                }
            };
            // Cache the exact real-line height (ghost excluded) so a later frame that
            // only estimates this line reuses it instead of the char-count guess. Same
            // `rkey` as the render cache / estimate-branch lookup.
            height_cache.set(rkey, line_h);
            let total_h = gh + line_h;
            // Count coverage only from the anchor down (lines above are fixed-count
            // overscan); stop once the viewport + overscan below is covered.
            if i >= anchor_line {
                band_y += total_h;
                if band_y >= band_span {
                    past_band = true;
                    measured_count = i + 1;
                }
            }
            heights.push(total_h);
            layouts.push(layout);
            renders.push(lr);
            quote_bars.push(bars);
            line_ranges.push(range);
            line_diffs.push(line_diff);
            ghosts.push(line_ghosts);
            ghost_height.push(gh);
            image_blocks.push(image_block);
            inline_draws.push(line_inline_draws);
            table_lines.push(table_line);
        }
        cache.sweep();
        render_cache.sweep();
        height_cache.sweep();
        table_cache.sweep();
        // Trailing deletions: a hunk anchored at `n` (past the last line) has no host line
        // to hang above; build it here and draw it below the last real line.
        let (trailing_ghosts, trailing_ghost_height) =
            build_ghosts_before(engine, diff, n, theme, params, max_advance, min_row);
        // Row tint ~0.15 and word tint ~0.40 alpha, matching GitHub's dark-mode diff
        // line/word background strengths (the row was previously a near-invisible 0.05).
        let diff_colors = DiffColors {
            added_bg: peniko_color_alpha(theme.green, 0.15),
            added_inline: peniko_color_alpha(theme.green, 0.40),
            deleted_bg: peniko_color_alpha(theme.red, 0.15),
            deleted_inline: peniko_color_alpha(theme.red, 0.40),
        };
        Self {
            layouts,
            renders,
            line_ranges,
            line_diffs,
            ghosts,
            ghost_height,
            trailing_ghosts,
            trailing_ghost_height,
            quote_bars,
            image_blocks,
            inline_draws,
            image_urls,
            #[cfg(feature = "mermaid")]
            mermaid_sources,
            #[cfg(feature = "math")]
            math_sources,
            img_vpad,
            img_label_size: 14.0 * scale,
            image_border: peniko_color(theme.comment),
            image_bg: peniko_color_alpha(theme.comment, 0.08),
            code_bg: peniko_color_alpha(theme.comment, 0.22),
            table_lines,
            table_cell_pad_x: TABLE_CELL_PAD_X * scale,
            table_cell_pad_y: TABLE_CELL_PAD_Y * scale,
            table_border: TABLE_BORDER * scale,
            // Subtle grid chrome, in the status-bar surface/selection tone.
            table_border_color: peniko_color(theme.selection),
            table_bg: peniko_color_alpha(theme.comment, 0.05),
            table_header_bg: peniko_color(theme.surface),
            diff_colors,
            // A thin rule (~1/6 of a mono cell), floored so it stays visible when small.
            quote_bar_width: (k * 0.16).max(1.5),
            quote_bar_color: peniko_color(theme.comment),
            tops: compute_tops(&heights, pad_top * scale),
            measured_start: measured_start.min(measured_count),
            measured_count,
            preedit_caret,
            scroll_y: 0.0,
            width: content_right,
            pad_top: pad_top * scale,
            pad_bottom: pad_bottom * scale,
            pad_x: left,
        }
    }

    /// The display↔buffer map for a line (Phase 4 cursor/click math).
    pub fn line_map(&self, line: usize) -> Option<&SegmentMap> {
        self.renders.get(line).map(|r| &r.map)
    }

    /// Buffer line index containing `buffer_off` (clamped to the last line).
    pub fn line_of(&self, buffer_off: usize) -> usize {
        let n = self.line_ranges.len();
        if n == 0 {
            return 0;
        }
        self.line_ranges
            .partition_point(|r| r.start <= buffer_off)
            .saturating_sub(1)
            .min(n - 1)
    }

    /// Screen-space top y (device px) of line `i`'s real text, or `None` if out of
    /// range. Used to place fold chevrons in the gutter beside heading lines.
    pub fn line_top_screen(&self, line: usize) -> Option<f32> {
        (line < self.layouts.len()).then(|| self.real_top(line) - self.scroll_y)
    }

    /// Height (device px) of line `i`'s laid-out text block, for vertically centering
    /// gutter affordances against it. Zero for estimated/hidden lines.
    pub fn line_text_height(&self, line: usize) -> f32 {
        self.layouts.get(line).map(|l| l.height()).unwrap_or(0.0)
    }

    /// Left draw origin (device px) of the document body — the gutter ends here.
    pub fn body_left(&self) -> f32 {
        self.pad_x
    }

    /// Top y (device px, before scroll) of the *real* text of line `i` — i.e. below
    /// any ghost (deleted) rows stacked above it.
    fn real_top(&self, line: usize) -> f32 {
        self.tops[line] + self.ghost_height[line]
    }

    /// True if `line` currently renders as a standalone image block (its markdown text
    /// is hidden). Used before a click so the caret can be re-placed once the click
    /// reveals the raw text (whose short row no longer sits under the click's y).
    pub fn is_image_line(&self, line: usize) -> bool {
        self.renders.get(line).is_some_and(|r| r.image.is_some())
    }

    /// Buffer offset for horizontal position `x` (device px) within `line`, at its first
    /// row. Unlike `hit_test`, the caller names the line, so a click on a tall image
    /// block can be re-mapped onto the short text row it reveals, at the same `x`.
    pub fn offset_in_line_at_x(&self, line: usize, x: f32) -> Option<usize> {
        let layout = self.layouts.get(line)?;
        let lx = (x - self.pad_x).max(0.0);
        let display_off = Cursor::from_point(layout, lx, 0.0).index();
        Some(self.renders[line].map.display_to_buffer(display_off))
    }

    /// Screen-space caret rectangle at a *display* byte offset within `line`'s layout.
    /// Shared by the buffer caret and the IME preedit caret (which live in different
    /// coordinate spaces: the latter indexes the spliced layout directly).
    fn caret_rect_at(
        &self,
        line: usize,
        display_off: usize,
        caret_width: f32,
    ) -> Option<ScreenRect> {
        let layout = self.layouts.get(line)?;
        let cursor = Cursor::from_byte_index(layout, display_off, Affinity::Downstream);
        let bb = cursor.geometry(layout, caret_width);
        let dx = self.pad_x as f64;
        let dy = (self.real_top(line) - self.scroll_y) as f64;
        Some((bb.x0 + dx, bb.y0 + dy, bb.x1 + dx, bb.y1 + dy))
    }

    /// Screen-space caret rectangle for a buffer offset, or None if empty doc.
    /// `caret_width` is in device px.
    pub fn caret_rect(&self, buffer_off: usize, caret_width: f32) -> Option<ScreenRect> {
        let line = self.line_of(buffer_off);
        let display_off = self.renders.get(line)?.map.buffer_to_display(buffer_off);
        self.caret_rect_at(line, display_off, caret_width)
    }

    /// Screen-space caret rectangle *inside* an active IME composition (the spliced
    /// layout), or None when no preedit is active. Positions the drawn caret + the OS
    /// candidate popup at the composition point.
    pub fn preedit_caret_rect(&self, caret_width: f32) -> Option<ScreenRect> {
        let (line, display_off) = self.preedit_caret?;
        self.caret_rect_at(line, display_off, caret_width)
    }

    /// Screen-space fill rectangles covering the buffer selection range. Handles
    /// multi-line and wrapped-line selections (parley yields one rect per visual row).
    pub fn selection_rects(&self, selection: Range<usize>) -> Vec<ScreenRect> {
        if selection.start >= selection.end {
            return Vec::new();
        }
        let first = self.line_of(selection.start);
        let last = self.line_of(selection.end);
        let mut rects = Vec::new();
        for line in first..=last.min(self.layouts.len().saturating_sub(1)) {
            let range = &self.line_ranges[line];
            let s = selection.start.max(range.start);
            let e = selection.end.min(range.end);
            if s >= e {
                continue;
            }
            let map = &self.renders[line].map;
            let ds = map.buffer_to_display(s);
            let de = map.buffer_to_display(e);
            if ds >= de {
                continue;
            }
            let layout = &self.layouts[line];
            let sel = display_range_selection(layout, ds..de);
            let dx = self.pad_x as f64;
            let dy = (self.real_top(line) - self.scroll_y) as f64;
            for (bb, _) in sel.geometry(layout) {
                rects.push((bb.x0 + dx, bb.y0 + dy, bb.x1 + dx, bb.y1 + dy));
            }
        }
        rects
    }

    /// Map a screen point (device px) to a buffer offset via Parley hit-testing.
    /// Points landing in a ghost (deleted) block are inert — they map to the start
    /// of the real line the ghosts precede.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        let n = self.layouts.len();
        if n == 0 {
            return None;
        }
        let cy = y + self.scroll_y;
        let line = self.tops[..n]
            .partition_point(|&t| t <= cy)
            .saturating_sub(1)
            .min(n - 1);
        let real_top = self.real_top(line);
        if cy < real_top {
            // In the ghost block above the real line: inert.
            return Some(self.line_ranges[line].start);
        }
        let layout = &self.layouts[line];
        let lx = (x - self.pad_x).max(0.0);
        let ly = (cy - real_top).max(0.0);
        // Grid table row: its own text layout is empty (hidden), so hit-test the clicked
        // cell within the shared TableLayout instead of collapsing to the line start.
        if let Some((tl, kind)) = &self.table_lines[line]
            && let Some(off) = tl.hit_test(*kind, lx, self.table_cell_pad_x)
        {
            return Some(off);
        }
        // Image block (standalone image or a mermaid diagram): the text layout is empty,
        // so land the caret at the line's start — for a mermaid fence that reveals the
        // raw source. (The shared empty render's map is anchored at offset 0, so it can't
        // be trusted here.)
        if self.image_blocks[line].is_some() {
            return Some(self.line_ranges[line].start);
        }
        let display_off = Cursor::from_point(layout, lx, ly).index();
        Some(self.renders[line].map.display_to_buffer(display_off))
    }

    /// Scroll the minimum amount so the line containing `buffer_off` is visible.
    /// Reveals the ghost block above too (so a hunk's deletions come into view).
    pub fn scroll_to(&mut self, buffer_off: usize, viewport_h: f32) {
        let line = self.line_of(buffer_off);
        let top = self.tops[line];
        let bottom = self.tops[line + 1];
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if bottom > self.scroll_y + viewport_h {
            self.scroll_y = bottom - viewport_h;
        }
        self.clamp_scroll(viewport_h);
    }

    /// Pin `line`'s top to the viewport top (clamped to the document end). Reads better
    /// than `scroll_to`'s minimal scroll for outline navigation, where the user expects
    /// the clicked heading to jump to the top.
    pub fn scroll_line_to_top(&mut self, line: usize, viewport_h: f32) {
        let idx = line.min(self.tops.len().saturating_sub(1));
        self.scroll_y = self.tops[idx];
        self.clamp_scroll(viewport_h);
    }

    pub fn line_count(&self) -> usize {
        self.layouts.len()
    }

    /// True if the viewport now reaches into height-estimated (un-laid-out) lines above
    /// or below the materialized band — i.e. a scroll outran the band and the shell
    /// should rebuild around the new anchor before this frame draws blanks.
    pub fn needs_remeasure(&self, viewport_h: f32) -> bool {
        let (first, last) = self.visible_range(viewport_h);
        (self.measured_start > 0 && first < self.measured_start)
            || (self.measured_count < self.layouts.len() && last > self.measured_count)
    }

    /// The scroll anchor: the topmost line touching the viewport, and how far its top is
    /// scrolled above the viewport top (device px). Re-pinning to this across a rebuild
    /// keeps visible content from shifting when off-screen height estimates change — the
    /// heart of the anchor-forward virtualization.
    pub fn scroll_anchor(&self) -> (usize, f32) {
        let n = self.layouts.len();
        if n == 0 {
            return (0, 0.0);
        }
        let line = self.tops[1..]
            .partition_point(|&b| b <= self.scroll_y)
            .min(n - 1);
        (line, self.scroll_y - self.tops[line])
    }

    /// The `scroll_y` that pins `line`'s top `offset` px above the viewport top — the
    /// inverse of `scroll_anchor`, used to re-pin after a rebuild.
    pub fn anchor_scroll_y(&self, line: usize, offset: f32) -> f32 {
        let line = line.min(self.tops.len().saturating_sub(1));
        self.tops[line] + offset
    }

    /// Total document height in device px (last line bottom + bottom padding).
    pub fn content_height(&self) -> f32 {
        self.tops.last().copied().unwrap_or(self.pad_top)
            + self.trailing_ghost_height
            + self.pad_bottom
    }

    /// Largest valid scroll offset so the document bottom aligns to the viewport.
    pub fn max_scroll(&self, viewport_h: f32) -> f32 {
        (self.content_height() - viewport_h).max(0.0)
    }

    pub fn scroll_by(&mut self, dy: f32, viewport_h: f32) {
        self.scroll_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll(viewport_h));
    }

    pub fn clamp_scroll(&mut self, viewport_h: f32) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll(viewport_h));
    }

    /// Half-open range of lines intersecting `[scroll_y, scroll_y + viewport_h)`.
    pub fn visible_range(&self, viewport_h: f32) -> (usize, usize) {
        let n = self.layouts.len();
        if n == 0 {
            return (0, 0);
        }
        let top = self.scroll_y;
        let bottom = self.scroll_y + viewport_h;
        // First line whose *bottom* (tops[i+1]) is past the viewport top.
        let first = self.tops[1..].partition_point(|&b| b <= top).min(n);
        // First line whose *top* (tops[i]) is at/after the viewport bottom.
        let last = self.tops[..n].partition_point(|&t| t < bottom).min(n);
        (first, last.max(first))
    }

    /// Fill the display-byte rects of `ranges` within `layout`, offset by `(origin_x,
    /// origin_y)` — the top-left where the layout's glyphs are painted.
    fn fill_display_ranges(
        &self,
        scene: &mut Scene,
        layout: &parley::Layout<Brush>,
        ranges: &[Range<usize>],
        origin_x: f64,
        origin_y: f64,
        color: Color,
    ) {
        for r in ranges {
            let sel = display_range_selection(layout, r.clone());
            for (bb, _) in sel.geometry(layout) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    color,
                    None,
                    &Rect::new(
                        bb.x0 + origin_x,
                        bb.y0 + origin_y,
                        bb.x1 + origin_x,
                        bb.y1 + origin_y,
                    ),
                );
            }
        }
    }

    /// Fill the word-change rects of `ranges` within `layout`, offset to `top_y`. Body
    /// lines are drawn at `self.pad_x`, so that is the x origin.
    fn fill_word_ranges(
        &self,
        scene: &mut Scene,
        layout: &parley::Layout<Brush>,
        ranges: &[Range<usize>],
        top_y: f64,
        color: Color,
    ) {
        self.fill_display_ranges(scene, layout, ranges, self.pad_x as f64, top_y, color);
    }

    /// Paint line `i`'s inline images at their laid-out inline-box positions. `gy` is
    /// the real line's top (device px, scroll-applied). A box's `id` indexes
    /// `inline_draws[i]`: a loaded image is drawn scaled to the box; otherwise a faint
    /// bordered placeholder rect fills the reserved space.
    fn draw_inline_images(&self, scene: &mut Scene, i: usize, gy: f32) {
        let draws = &self.inline_draws[i];
        if draws.is_empty() {
            return;
        }
        for line in self.layouts[i].lines() {
            for item in line.items() {
                let PositionedLayoutItem::InlineBox(pib) = item else {
                    continue;
                };
                let Some(draw) = draws.get(pib.id as usize) else {
                    continue;
                };
                let x = self.pad_x as f64 + pib.x as f64;
                let y = gy as f64 + pib.y as f64 + draw.baseline_offset as f64;
                match &draw.brush {
                    Some((brush, nat_w, nat_h)) => {
                        // draw_image paints at native pixel size, so scale box/intrinsic.
                        let transform = Affine::translate((x, y))
                            * Affine::scale_non_uniform(
                                pib.width as f64 / *nat_w as f64,
                                pib.height as f64 / *nat_h as f64,
                            );
                        scene.draw_image(brush, transform);
                    }
                    None => {
                        let rect = Rect::new(x, y, x + pib.width as f64, y + pib.height as f64)
                            .to_rounded_rect(3.0);
                        scene.fill(Fill::NonZero, Affine::IDENTITY, self.image_bg, None, &rect);
                        scene.stroke(
                            &Stroke::new(1.0),
                            Affine::IDENTITY,
                            self.image_border,
                            None,
                            &rect,
                        );
                    }
                }
            }
        }
    }

    /// Paint visible lines: ghost (deleted) rows above each line, then real glyphs.
    pub fn draw(&self, engine: &TextEngine, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            // Shaped ghost rows sit directly above the real line (bottom-aligned); for a
            // capped huge deletion the estimated overflow is the empty gap above them.
            let real_top = self.real_top(i) - self.scroll_y;
            let shaped_h: f32 = self.ghosts[i].iter().map(|g| g.height).sum();
            let mut gy = real_top - shaped_h;
            for ghost in &self.ghosts[i] {
                self.draw_ghost(engine, scene, ghost, gy, viewport_h);
                gy += ghost.height;
            }
            // Background chips behind inline code, under the real line's glyphs.
            let code = &self.renders[i].code_ranges;
            if !code.is_empty() {
                self.fill_word_ranges(scene, &self.layouts[i], code, real_top as f64, self.code_bg);
            }
            // Inline images: paint each at its laid-out box position (a loaded image, or a
            // faint bordered placeholder). Parley left a gap in the glyphs where the box is.
            self.draw_inline_images(scene, i, real_top);
            // The real line, below its ghost block.
            engine.draw_line(scene, &self.layouts[i], (self.pad_x, real_top));
        }
        // Trailing deletions below the last real line (a hunk anchored past the end).
        let mut gy = self.tops[self.layouts.len()] - self.scroll_y;
        for ghost in &self.trailing_ghosts {
            self.draw_ghost(engine, scene, ghost, gy, viewport_h);
            gy += ghost.height;
        }
    }

    /// Paint one ghost (deleted) row at `gy` — red row band, word-level deletion tint,
    /// then the text — skipping it entirely when it falls outside the viewport.
    fn draw_ghost(
        &self,
        engine: &TextEngine,
        scene: &mut Scene,
        ghost: &Ghost,
        gy: f32,
        viewport_h: f32,
    ) {
        if gy + ghost.height < 0.0 || gy > viewport_h {
            return; // off-screen — cull
        }
        let top = gy as f64;
        let bottom = (gy + ghost.height) as f64;
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            self.diff_colors.deleted_bg,
            None,
            &Rect::new(
                self.pad_x as f64,
                top,
                (self.width - self.pad_x) as f64,
                bottom,
            ),
        );
        self.fill_word_ranges(
            scene,
            &ghost.layout,
            &ghost.inline,
            top,
            self.diff_colors.deleted_inline,
        );
        engine.draw_line(scene, &ghost.layout, (self.pad_x, gy));
    }

    /// Paint blockquote gutter rules: one continuous vertical rect per nesting level,
    /// spanning each quote line's full height (so it covers wrapped rows) and tiling
    /// with adjacent quote lines (so multi-line quotes read as one unbroken bar). Call
    /// BEFORE `draw`. `quote_bars[i]` holds each level's x-offset from the line origin.
    pub fn draw_blockquote_gutters(&self, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            let bars = &self.quote_bars[i];
            if bars.is_empty() {
                continue;
            }
            let top = (self.real_top(i) - self.scroll_y) as f64;
            let bottom = (self.tops[i + 1] - self.scroll_y) as f64;
            for &bx in bars {
                let x = (self.pad_x + bx) as f64;
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    self.quote_bar_color,
                    None,
                    &Rect::new(x, top, x + self.quote_bar_width as f64, bottom),
                );
            }
        }
    }

    /// Paint thematic breaks (`---`) as a horizontal rule across the content width,
    /// centered in the line (whose `---` text is hidden). Call BEFORE `draw`.
    pub fn draw_horizontal_rules(&self, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            if !self.renders[i].is_hr {
                continue;
            }
            let mid = (self.real_top(i) + self.tops[i + 1]) / 2.0 - self.scroll_y;
            let h = self.quote_bar_width as f64;
            let x0 = self.pad_x as f64;
            let x1 = (self.width - self.pad_x) as f64;
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                self.quote_bar_color,
                None,
                &Rect::new(x0, mid as f64 - h / 2.0, x1, mid as f64 + h / 2.0),
            );
        }
    }

    /// Paint grid-table rows: per-cell background + border rects and the shaped cell
    /// glyphs. The header row's distinct background fill separates it from the body, so no
    /// delimiter rule is drawn (the delimiter line has zero grid height). Call BEFORE
    /// `draw` (a grid row's own text layout is empty, so no glyphs collide). Cell layouts
    /// are held in the shared `TableLayout`, so this is pure drawing — no shaping.
    pub fn draw_tables(&self, engine: &TextEngine, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        let pad_x = self.pad_x as f64;
        let cell_pad_x = self.table_cell_pad_x;
        let cell_pad_y = self.table_cell_pad_y;
        let border = self.table_border as f64;
        for i in first..last {
            let Some((tl, kind)) = &self.table_lines[i] else {
                continue;
            };
            // The delimiter row is not drawn: it takes zero grid height, so the header
            // (distinguished by its background fill) sits directly on the first body row.
            let Some(row_idx) = TableLayout::row_index(*kind) else {
                continue;
            };
            let top = (self.real_top(i) - self.scroll_y) as f64;
            let bottom = (self.tops[i + 1] - self.scroll_y) as f64;
            let is_header = matches!(kind, RowKind::Header);
            let bg = if is_header {
                self.table_header_bg
            } else {
                self.table_bg
            };
            for c in 0..tl.col_x.len() {
                let cx = pad_x + tl.col_x[c] as f64;
                let box_w = (tl.col_w[c] + 2.0 * cell_pad_x) as f64;
                let rect = Rect::new(cx, top, cx + box_w, bottom);
                scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &rect);
                scene.stroke(
                    &Stroke::new(border),
                    Affine::IDENTITY,
                    self.table_border_color,
                    None,
                    &rect,
                );
                // Cell glyphs, aligned within the column's content width.
                if let Some(Some(slot)) = tl.row_layouts.get(row_idx).and_then(|r| r.get(c)) {
                    let layout = &slot.layout;
                    let align = tl.aligns.get(c).copied().unwrap_or(Align::Left);
                    let extra = (tl.col_w[c] - layout.width()).max(0.0);
                    let shift = match align {
                        Align::Left => 0.0,
                        Align::Right => extra,
                        Align::Center => extra / 2.0,
                    };
                    let gx = cx as f32 + cell_pad_x + shift;
                    let gy = top as f32 + cell_pad_y;
                    // Chip behind inline code, over the cell bg but under the glyphs.
                    if !slot.code_ranges.is_empty() {
                        self.fill_display_ranges(
                            scene,
                            layout,
                            &slot.code_ranges,
                            gx as f64,
                            gy as f64,
                            self.code_bg,
                        );
                    }
                    engine.draw_line(scene, layout, (gx, gy));
                }
            }
        }
    }

    /// Distinct standalone-image URLs across the materialized lines. The shell diffs
    /// these against the shared cache to spawn loads.
    pub fn image_urls(&self) -> &[String] {
        &self.image_urls
    }

    /// `(cache key, source)` of each materialized mermaid diagram, for the shell to
    /// kick off off-thread rendering (mirrors `image_urls`).
    #[cfg(feature = "mermaid")]
    pub fn mermaid_sources(&self) -> &[(String, String)] {
        &self.mermaid_sources
    }

    /// `(cache key, job)` of each materialized math render, for the shell to kick off
    /// off-thread rendering (mirrors `mermaid_sources`).
    #[cfg(feature = "math")]
    pub fn math_sources(&self) -> &[(String, math::MathJob)] {
        &self.math_sources
    }

    /// Paint standalone-image blocks: a loaded image at its fitted device rect, or a
    /// bordered placeholder box with an alt/status label (pending/broken). Left-aligned
    /// at `pad_x`, inset by `img_vpad`. Call BEFORE `draw` (glyphs, if any, sit on top).
    pub fn draw_images(&self, engine: &mut TextEngine, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            let Some(block) = &self.image_blocks[i] else {
                continue;
            };
            let x = self.pad_x as f64;
            let top = (self.real_top(i) - self.scroll_y + self.img_vpad) as f64;
            match &block.kind {
                ImageBlockKind::Loaded {
                    brush,
                    nat_w,
                    nat_h,
                    dest_w,
                    dest_h,
                } => {
                    // draw_image paints at native pixel size, so scale dest/intrinsic.
                    let transform = Affine::translate((x, top))
                        * Affine::scale_non_uniform(
                            (*dest_w / *nat_w) as f64,
                            (*dest_h / *nat_h) as f64,
                        );
                    scene.draw_image(brush, transform);
                }
                ImageBlockKind::Loading | ImageBlockKind::Failed => {
                    let line_bottom = (self.tops[i + 1] - self.scroll_y) as f64;
                    let x1 = (self.width - self.pad_x) as f64;
                    let bottom = line_bottom - self.img_vpad as f64;
                    let rect = Rect::new(x, top, x1, bottom.max(top)).to_rounded_rect(4.0);
                    scene.fill(Fill::NonZero, Affine::IDENTITY, self.image_bg, None, &rect);
                    scene.stroke(
                        &Stroke::new(1.0),
                        Affine::IDENTITY,
                        self.image_border,
                        None,
                        &rect,
                    );
                    let failed = matches!(block.kind, ImageBlockKind::Failed);
                    let label = if block.alt.is_empty() {
                        if failed {
                            "broken image"
                        } else {
                            "loading image…"
                        }
                        .to_string()
                    } else if failed {
                        format!("⚠ {}", block.alt)
                    } else {
                        block.alt.clone()
                    };
                    let pad = self.img_label_size * 0.5;
                    let layout = engine.build_line(
                        &label,
                        1.0,
                        self.img_label_size,
                        UI_LINE_HEIGHT,
                        self.image_border,
                        Some(((x1 - x) as f32 - 2.0 * pad).max(1.0)),
                        &[],
                    );
                    let ly = top as f32 + ((bottom - top) as f32 - layout.height()) * 0.5;
                    engine.draw_line(scene, &layout, (x as f32 + pad, ly.max(top as f32)));
                }
            }
        }
    }

    /// Paint inline-diff backgrounds for added lines (faint full-row bg + stronger
    /// word-change bg). Call BEFORE `draw` so glyphs sit on top.
    pub fn draw_added_backgrounds(&self, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            let d = &self.line_diffs[i];
            if !d.is_addition {
                continue;
            }
            let top = (self.real_top(i) - self.scroll_y) as f64;
            let bottom = (self.tops[i + 1] - self.scroll_y) as f64;
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                self.diff_colors.added_bg,
                None,
                &Rect::new(
                    self.pad_x as f64,
                    top,
                    (self.width - self.pad_x) as f64,
                    bottom,
                ),
            );
            self.fill_word_ranges(
                scene,
                &self.layouts[i],
                &d.inline,
                top,
                self.diff_colors.added_inline,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{FONT_SIZE, LINE_HEIGHT, OUTLINE_WIDTH, PADDING};
    use crate::text_engine::peniko_color;

    /// Device-px content width shared by the image-sizing tests (scale 1.0).
    const TEST_CONTENT_W: f32 = 800.0;

    /// Frame constants for the headless tests — the same source of truth the shell uses,
    /// so the tests track any change to the real frame rather than duplicating literals.
    fn test_params(theme: &EditorTheme, device_width: f32) -> LayoutParams {
        LayoutParams {
            content_x0: 0.0,
            content_w: device_width,
            scale: 1.0,
            pad_x: PADDING,
            pad_top: PADDING,
            pad_bottom: PADDING * 2.0,
            base_font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
            fg: peniko_color(theme.foreground),
        }
    }

    /// Encode a solid RGBA image of the given size to PNG, decode it into the cache
    /// under `url`, and return that URL's `ImageRef`.
    fn cache_image(cache: &ImageCache, url: &str, w: u32, h: u32) -> ImageRef {
        use image::{ImageFormat, RgbaImage};
        use std::io::Cursor;
        let img = RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        cache.set_loaded(url, crate::image_cache::decode(&buf).unwrap());
        ImageRef {
            url: url.to_string(),
            alt: String::new(),
        }
    }

    /// Any image at least as wide as the body fills the full content width (aspect
    /// preserved), with no height cap — so a tall-but-wide-enough image isn't shrunk
    /// below the body width. A smaller image keeps its intrinsic size (no upscale).
    #[test]
    fn image_block_fills_width_preserving_aspect() {
        let cache = ImageCache::new();
        let content_w = TEST_CONTENT_W;
        let vpad = IMG_VPAD;

        // Wide: 2000x100 → clamps to content width, height scaled to keep aspect.
        let wide = cache_image(&cache, "wide", 2000, 100);
        let (block, _) = build_image_block(&cache, &wide, content_w, 1.0, vpad);
        let ImageBlockKind::Loaded { dest_w, dest_h, .. } = block.kind else {
            panic!("expected loaded");
        };
        assert!(
            (dest_w - content_w).abs() < 0.5,
            "wide image fills content width"
        );
        assert!(
            (dest_h - content_w * 100.0 / 2000.0).abs() < 0.5,
            "aspect preserved"
        );

        // Tall but wide enough: 1000x2000 → still fills the width (no cap), height
        // follows aspect (1600), rather than being shrunk to fit a height limit.
        let big = cache_image(&cache, "big", 1000, 2000);
        let (block, _) = build_image_block(&cache, &big, content_w, 1.0, vpad);
        let ImageBlockKind::Loaded { dest_w, dest_h, .. } = block.kind else {
            panic!("expected loaded");
        };
        assert!(
            (dest_w - content_w).abs() < 0.5,
            "fills width even when tall"
        );
        assert!(
            (dest_h - content_w * 2000.0 / 1000.0).abs() < 0.5,
            "height uncapped"
        );

        // Small: 40x30, under the width → shown at intrinsic size (no upscale).
        let small = cache_image(&cache, "small", 40, 30);
        let (block, block_h) = build_image_block(&cache, &small, content_w, 1.0, vpad);
        let ImageBlockKind::Loaded { dest_w, dest_h, .. } = block.kind else {
            panic!("expected loaded");
        };
        assert_eq!(
            (dest_w, dest_h),
            (40.0, 30.0),
            "small image is not upscaled"
        );
        assert!(
            (block_h - (30.0 + 2.0 * vpad)).abs() < 0.5,
            "block adds vertical padding"
        );
    }

    /// An uncached (still-loading) URL gets a fixed placeholder block height.
    #[test]
    fn image_block_placeholder_height_when_unloaded() {
        let cache = ImageCache::new();
        let img = ImageRef {
            url: "pending.png".to_string(),
            alt: "x".to_string(),
        };
        let (block, block_h) = build_image_block(&cache, &img, TEST_CONTENT_W, 1.0, IMG_VPAD);
        assert!(matches!(block.kind, ImageBlockKind::Loading));
        assert_eq!(block_h, IMG_PLACEHOLDER_H);
    }

    /// The pure IME splice: text is `text[..caret] + preedit + text[caret..]`, runs
    /// before the caret are untouched, runs at/after shift by `preedit.len()`, and
    /// exactly one underline run covers the composition span.
    #[test]
    fn splice_preedit_inserts_underlined_composition() {
        let fg = Color::WHITE;
        let text = "abcdef";
        let before = StyleRun::new(0..2, fg); // entirely before the caret
        let after = StyleRun::new(4..6, fg); // entirely at/after the caret
        let runs = vec![before, after];
        let caret = 3;
        let preedit = "XY"; // len 2
        let (out_text, out_runs) = splice_preedit(text, &runs, caret, preedit, fg);
        assert_eq!(out_text, "abcXYdef");
        assert_eq!(out_runs[0].range, 0..2, "run before caret is unchanged");
        assert_eq!(
            out_runs[1].range,
            6..8,
            "run at/after caret shifts by preedit.len()"
        );
        let unders: Vec<_> = out_runs.iter().filter(|r| r.underline).collect();
        assert_eq!(unders.len(), 1, "exactly one underline run");
        assert_eq!(unders[0].range, 3..5, "underline covers the preedit span");
    }

    #[test]
    fn tops_prefix_sum_has_n_plus_one() {
        let heights = [10.0, 20.0, 5.0];
        let tops = compute_tops(&heights, 4.0);
        assert_eq!(tops.len(), 4); // n + 1
        assert_eq!(tops, vec![4.0, 14.0, 34.0, 39.0]);
    }

    #[test]
    fn tops_empty_doc() {
        let tops = compute_tops(&[], 4.0);
        assert_eq!(tops, vec![4.0]); // just the padding top
    }

    /// Build a DocLayout-like fixture from raw heights to test visibility math
    /// without a GPU/font stack.
    fn fixture(heights: &[f32], pad_top: f32, pad_bottom: f32) -> DocLayout {
        DocLayout {
            layouts: heights
                .iter()
                .map(|_| Rc::new(parley::Layout::new()))
                .collect(),
            renders: heights
                .iter()
                .map(|_| {
                    Rc::new(LineRender {
                        text: String::new(),
                        font_size: 18.0,
                        runs: Vec::new(),
                        map: SegmentMap::identity("", 0).1,
                        content_start: 0,
                        quote_bar_bytes: Vec::new(),
                        is_hr: false,
                        image: None,
                        code_ranges: Vec::new(),
                        inline_images: Vec::new(),
                        inline_math: Vec::new(),
                        table: None,
                    })
                })
                .collect(),
            line_ranges: heights.iter().map(|_| 0..0).collect(),
            line_diffs: heights.iter().map(|_| LineDiff::default()).collect(),
            ghosts: heights.iter().map(|_| Vec::new()).collect(),
            ghost_height: heights.iter().map(|_| 0.0).collect(),
            trailing_ghosts: Vec::new(),
            trailing_ghost_height: 0.0,
            quote_bars: heights.iter().map(|_| Vec::new()).collect(),
            image_blocks: heights.iter().map(|_| None).collect(),
            inline_draws: heights.iter().map(|_| Vec::new()).collect(),
            image_urls: Vec::new(),
            #[cfg(feature = "mermaid")]
            mermaid_sources: Vec::new(),
            #[cfg(feature = "math")]
            math_sources: Vec::new(),
            img_vpad: 0.0,
            img_label_size: 14.0,
            image_border: Color::TRANSPARENT,
            image_bg: Color::TRANSPARENT,
            code_bg: Color::TRANSPARENT,
            table_lines: heights.iter().map(|_| None).collect(),
            table_cell_pad_x: 0.0,
            table_cell_pad_y: 0.0,
            table_border: 0.0,
            table_border_color: Color::TRANSPARENT,
            table_bg: Color::TRANSPARENT,
            table_header_bg: Color::TRANSPARENT,
            quote_bar_width: 2.0,
            quote_bar_color: Color::TRANSPARENT,
            measured_start: 0,
            measured_count: heights.len(),
            preedit_caret: None,
            diff_colors: DiffColors {
                added_bg: Color::TRANSPARENT,
                added_inline: Color::TRANSPARENT,
                deleted_bg: Color::TRANSPARENT,
                deleted_inline: Color::TRANSPARENT,
            },
            tops: compute_tops(heights, pad_top),
            scroll_y: 0.0,
            width: 100.0,
            pad_top,
            pad_bottom,
            pad_x: 0.0,
        }
    }

    #[test]
    fn visible_range_covers_viewport() {
        // 5 lines of height 10, pad_top 0 => tops [0,10,20,30,40,50]
        let doc = fixture(&[10.0; 5], 0.0, 0.0);
        // Viewport [0,25) => lines 0,1,2 (line 2 spans 20..30, intersects).
        assert_eq!(doc.visible_range(25.0), (0, 3));
        // Scrolled to 25, viewport height 20 => [25,45) => lines 2,3,4.
        let mut doc = doc;
        doc.scroll_y = 25.0;
        assert_eq!(doc.visible_range(20.0), (2, 5));
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut doc = fixture(&[10.0; 5], 0.0, 0.0); // content height 50
        doc.scroll_by(1000.0, 20.0);
        assert_eq!(doc.scroll_y, 30.0); // 50 - 20
        doc.scroll_by(-1000.0, 20.0);
        assert_eq!(doc.scroll_y, 0.0);
    }

    #[test]
    fn short_doc_has_no_scroll() {
        let mut doc = fixture(&[10.0; 2], 0.0, 0.0); // content 20 < viewport 100
        doc.scroll_by(50.0, 100.0);
        assert_eq!(doc.scroll_y, 0.0);
        assert_eq!(doc.max_scroll(100.0), 0.0);
    }

    /// The load-bearing Phase 4 path: a real layout, caret geometry, and click
    /// hit-testing must round-trip through Parley + the segment map. Runs headless
    /// (fonts, no GPU).
    #[test]
    fn caret_and_hit_test_roundtrip() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "hello world\nsecond line here\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            &snapshot,
            &theme,
            None,
            None,
            &ImageCache::new(),
            0,
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        // Several buffer offsets (incl. second line) should map to a caret rect
        // whose center hit-tests back to the same offset.
        for &off in &[0usize, 6, 11, 12, 19, 27] {
            let (x0, y0, x1, y1) = doc.caret_rect(off, 2.0).expect("caret rect");
            let cx = ((x0 + x1) / 2.0) as f32;
            let cy = ((y0 + y1) / 2.0) as f32;
            let got = doc.hit_test(cx, cy).expect("hit test");
            assert_eq!(got, off, "offset {off} round-trip (got {got})");
        }
    }

    /// Folding integration: hidden lines get zero height, so `tops` ties across the
    /// folded run, `content_height` drops by exactly the folded lines' heights, and a
    /// click just below the fold resolves to the first visible line after it. This is
    /// the load-bearing virtualization invariant the plan flags as riskiest.
    #[test]
    fn folded_lines_are_zero_height() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "line0\nline1\nline2\nline3\nline4\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let build = |engine: &mut TextEngine, folds: &[Range<usize>]| {
            DocLayout::build(
                engine,
                &mut LineCache::new(),
                &mut RenderCache::new(),
                &mut HeightCache::new(),
                &mut TableCache::new(),
                0,
                &snapshot,
                &theme,
                None,
                None,
                &ImageCache::new(),
                0,
                &params,
                None,
                0,
                f32::INFINITY,
                folds,
                &(0..0),
                &std::collections::HashMap::new(),
            )
        };
        let full = build(&mut engine, &[]);
        let folded = build(&mut engine, std::slice::from_ref(&(1..3))); // hide lines 1 and 2

        // Lines 1 and 2 collapse: their tops tie, and content height drops by exactly
        // the height those two lines occupied in the unfolded layout.
        assert_eq!(folded.tops[1], folded.tops[2]);
        assert_eq!(folded.tops[2], folded.tops[3]);
        let hidden_h = full.tops[3] - full.tops[1];
        assert!((full.content_height() - folded.content_height() - hidden_h).abs() < 1e-3);

        // A click at the y where line 3 now sits resolves to line 3 (not a hidden line).
        let y3 = folded.line_top_screen(3).expect("line 3 visible") + 1.0;
        let hit = folded.hit_test(folded.pad_x + 1.0, y3).expect("hit");
        assert_eq!(folded.line_of(hit), 3);
    }

    /// The outline-panel inset: shrinking the horizontal content region to
    /// `content_w = W - OUTLINE_WIDTH` narrows the doc's draw origin / body width and
    /// caps `self.width` at the region edge (not the window edge), so full-width diff
    /// backgrounds stop at the panel. Caret geometry still round-trips within the region.
    #[test]
    fn content_region_inset_narrows_layout() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "hello world\nsecond line here\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();

        let window_w = 1100.0f32;
        let scale = 1.0f32;
        let content_w = window_w - OUTLINE_WIDTH * scale; // 860, below MAX_CONTENT_WIDTH
        let params = LayoutParams {
            content_x0: 0.0,
            content_w,
            scale,
            pad_x: PADDING,
            pad_top: PADDING,
            pad_bottom: PADDING * 2.0,
            base_font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
            fg: peniko_color(theme.foreground),
        };
        let mut build = |params: &LayoutParams| {
            DocLayout::build(
                &mut engine,
                &mut LineCache::new(),
                &mut RenderCache::new(),
                &mut HeightCache::new(),
                &mut TableCache::new(),
                0,
                &snapshot,
                &theme,
                None,
                None,
                &ImageCache::new(),
                0,
                params,
                None,
                0,
                f32::INFINITY,
                &[],
                &(0..0),
                &std::collections::HashMap::new(),
            )
        };
        let doc = build(&params);
        // `self.width` is the region edge, not the window edge — diff backgrounds fill
        // to here so they stop at the panel.
        assert_eq!(doc.width, content_w);
        // Region narrower than MAX_CONTENT_WIDTH → the body just uses base padding.
        assert_eq!(doc.pad_x, PADDING * scale);
        // The drawable body is inset symmetrically inside the region, never past its edge.
        let body_right = doc.width - doc.pad_x;
        assert!(body_right <= content_w);

        // The full-width case (panel closed) keeps the window edge and centers the body.
        let full = LayoutParams {
            content_w: window_w,
            ..params
        };
        let doc_full = build(&full);
        assert_eq!(doc_full.width, window_w);
        assert!(
            doc_full.pad_x > doc.pad_x,
            "wider region → larger centering inset"
        );

        // Caret still round-trips within the narrowed region.
        for &off in &[0usize, 6, 11] {
            let (x0, _, x1, _) = doc.caret_rect(off, 2.0).expect("caret rect");
            assert!(x0 >= doc.pad_x as f64 - 1.0 && x1 <= doc.width as f64);
        }
    }

    #[test]
    fn scroll_line_to_top_pins_and_clamps() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let src = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut buffer: Buffer = src.parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let params = test_params(&theme, 800.0);
        let viewport_h = 200.0;
        let mut doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            &snapshot,
            &theme,
            None,
            None,
            &ImageCache::new(),
            0,
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );

        // A mid-document line: its top pins to the viewport top exactly.
        doc.scroll_line_to_top(10, viewport_h);
        assert_eq!(doc.scroll_y, doc.tops[10]);

        // Scrolling to a line near the end clamps to `max_scroll` (can't scroll past
        // the document bottom), so `scroll_y` never exceeds the clamp.
        doc.scroll_line_to_top(39, viewport_h);
        assert_eq!(doc.scroll_y, doc.max_scroll(viewport_h));

        // An out-of-range line index saturates to the last entry rather than panicking.
        doc.scroll_line_to_top(9999, viewport_h);
        assert_eq!(doc.scroll_y, doc.max_scroll(viewport_h));
    }

    /// Ghost (deleted) rows offset the real line below them, and clicks in a ghost
    /// block are inert. Locks the Phase 5b interleave math (the plan's defect surface).
    #[test]
    fn ghost_rows_offset_and_are_inert() {
        use crate::buffer::Buffer;
        use crate::diff::DiffState;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let old_text = "line one\nline two\n";
        let new_text = "line one changed here\nline two\n";
        let mut base: Buffer = old_text.parse().unwrap();
        let diff = DiffState::compute(base.render_snapshot(), old_text, new_text);
        assert!(diff.has_hunks());

        let mut buf: Buffer = new_text.parse().unwrap();
        let snapshot = buf.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            &snapshot,
            &theme,
            Some(&diff),
            None,
            &ImageCache::new(),
            usize::MAX,
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        // Line 0's changed version has a deleted ghost stacked above it.
        assert!(doc.ghost_height[0] > 0.0, "expected a ghost above line 0");
        // The real line begins below its ghost block, and the caret follows.
        assert!(doc.real_top(0) > doc.tops[0]);
        let (_, cy0, _, _) = doc.caret_rect(0, 2.0).unwrap();
        assert!(cy0 as f32 >= doc.real_top(0) - 1.0);
        // A click in the ghost block is inert → maps to the real line start (offset 0).
        let ghost_mid = doc.tops[0] + doc.ghost_height[0] * 0.5;
        assert_eq!(doc.hit_test(50.0, ghost_mid), Some(0));
    }

    /// Trailing deletions (a hunk anchored past the last line, with no final newline) must
    /// render as trailing ghost rows below the last line — they used to vanish entirely.
    #[test]
    fn trailing_deletions_render_as_ghosts() {
        use crate::buffer::Buffer;
        use crate::diff::DiffState;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let old_text = "keep\ndelete one\ndelete two\n";
        let new_text = "keep"; // no trailing newline; the last two lines were deleted
        let mut base: Buffer = old_text.parse().unwrap();
        let diff = DiffState::compute(base.render_snapshot(), old_text, new_text);
        assert!(diff.has_hunks());

        let mut buf: Buffer = new_text.parse().unwrap();
        let snapshot = buf.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            &snapshot,
            &theme,
            Some(&diff),
            None,
            &ImageCache::new(),
            usize::MAX,
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        assert!(
            !doc.trailing_ghosts.is_empty(),
            "trailing deletion should render ghost rows (was: vanished)"
        );
        assert!(doc.trailing_ghost_height > 0.0);
        // Trailing ghost height is included in the scroll extent.
        assert!(doc.content_height() > doc.tops[doc.line_count()]);
    }

    /// The layout cache reuses unchanged lines: a warm rebuild (cache populated)
    /// skips Parley shaping and is faster than the cold one. Also exercises that a
    /// bounded set of entries survives (one per distinct line content).
    #[test]
    fn line_cache_reuses_and_speeds_up() {
        use crate::buffer::Buffer;
        use std::time::Instant;
        let mut engine = TextEngine::new();
        let mut cache = LineCache::new();
        let theme = EditorTheme::dracula();
        // Distinct lines so each is a unique cache entry (the real scenario).
        let text: String = (0..400)
            .map(|i| format!("Line {i}: the quick brown fox jumps over the lazy dog.\n"))
            .collect();
        let mut buffer: Buffer = text.parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let params = test_params(&theme, 1000.0);
        let build = |engine: &mut TextEngine, cache: &mut LineCache| {
            DocLayout::build(
                engine,
                cache,
                &mut RenderCache::new(),
                &mut HeightCache::new(),
                &mut TableCache::new(),
                0,
                &snapshot,
                &theme,
                None,
                None,
                &ImageCache::new(),
                0,
                &params,
                None,
                0,
                f32::INFINITY,
                &[],
                &(0..0),
                &std::collections::HashMap::new(),
            )
        };
        let t0 = Instant::now();
        let _ = build(&mut engine, &mut cache);
        let cold = t0.elapsed();
        let t1 = Instant::now();
        let _ = build(&mut engine, &mut cache);
        let warm = t1.elapsed();
        println!("[cache] cold={cold:?} warm={warm:?}");
        assert_eq!(
            cache.map.len(),
            401,
            "one entry per distinct line (+trailing)"
        );
        assert!(
            warm < cold,
            "warm rebuild ({warm:?}) should beat cold ({cold:?})"
        );
    }

    /// The render cache must recompute the cursor's line (marker reveal is
    /// cursor-dependent) while reusing others — the "don't serve a stale cursor
    /// line" correctness catch. Two builds share one RenderCache at the same buffer
    /// version but different cursor positions; the heading's `# ` hides/reveals.
    #[test]
    fn render_cache_recomputes_cursor_line() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut line_cache = LineCache::new();
        let mut render_cache = RenderCache::new();
        // "# Title\n" then a body line. Heading content "Title" starts at buffer 2.
        let mut buffer: Buffer = "# Title\nbody line two\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let build =
            |engine: &mut TextEngine, lc: &mut LineCache, rc: &mut RenderCache, cursor: usize| {
                DocLayout::build(
                    engine,
                    lc,
                    rc,
                    &mut HeightCache::new(),
                    &mut TableCache::new(),
                    7,
                    &snapshot,
                    &theme,
                    None,
                    None,
                    &ImageCache::new(),
                    cursor,
                    &params,
                    None,
                    0,
                    f32::INFINITY,
                    &[],
                    &(0..0),
                    &std::collections::HashMap::new(),
                )
            };

        // Cursor on the body line (offset 10) → heading `# ` is hidden.
        let doc_off = build(&mut engine, &mut line_cache, &mut render_cache, 10);
        assert_eq!(
            doc_off.line_map(0).unwrap().buffer_to_display(2),
            0,
            "with cursor off the heading, `# ` is hidden so content starts at display 0"
        );

        // Same cache + version, cursor now on the heading (offset 0) → `# ` revealed.
        let doc_on = build(&mut engine, &mut line_cache, &mut render_cache, 0);
        assert_eq!(
            doc_on.line_map(0).unwrap().buffer_to_display(2),
            2,
            "cursor on the heading must recompute it (not serve the stale hidden render)"
        );
    }

    /// Virtualization: with a small `measure_to_y`, a large doc materializes only the
    /// top band, leaves the rest height-estimated, keeps the visible range inside the
    /// materialized set (no blank draws), and still round-trips caret↔hit-test on-screen.
    #[test]
    fn virtualized_build_materializes_only_visible() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let text: String = (0..3000)
            .map(|i| format!("Line {i}: the quick brown fox jumps over the lazy dog.\n"))
            .collect();
        let mut buffer: Buffer = text.parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let viewport_h = 600.0f32;
        let params = test_params(&theme, 1000.0);
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            1,
            &snapshot,
            &theme,
            None,
            None,
            &ImageCache::new(),
            0,
            &params,
            None,
            0, // anchor at the top
            viewport_h,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        let n = doc.line_count();
        assert!(n >= 3000);
        assert!(
            doc.measured_count < n,
            "should estimate the tail, got measured_count={} of {n}",
            doc.measured_count
        );
        assert!(
            doc.measured_count >= 10,
            "should materialize at least the visible band, got {}",
            doc.measured_count
        );
        // The viewport never reaches into estimated lines at the top.
        assert!(!doc.needs_remeasure(viewport_h));
        let (first, last) = doc.visible_range(viewport_h);
        assert_eq!(first, 0);
        assert!(
            last <= doc.measured_count,
            "visible range {last} must stay within materialized {}",
            doc.measured_count
        );
        // Caret at the top round-trips through a materialized line.
        let (x0, y0, x1, y1) = doc.caret_rect(0, 2.0).expect("caret");
        let got = doc
            .hit_test(((x0 + x1) / 2.0) as f32, ((y0 + y1) / 2.0) as f32)
            .expect("hit");
        assert_eq!(got, 0);
        // Total content height is finite (estimated tail contributes real numbers).
        assert!(doc.content_height().is_finite() && doc.content_height() > 0.0);
    }

    /// Building around a deep anchor virtualizes BOTH the head (lines above) and the
    /// tail — the O(visible) win regardless of scroll depth — while keeping the band
    /// around the anchor fully laid out.
    #[test]
    fn virtualized_build_around_deep_anchor() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let text: String = (0..3000)
            .map(|i| format!("Line {i}: the quick brown fox jumps over the lazy dog.\n"))
            .collect();
        let mut buffer: Buffer = text.parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let viewport_h = 600.0f32;
        let params = test_params(&theme, 1000.0);
        let anchor = 1500usize;
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            1,
            &snapshot,
            &theme,
            None,
            None,
            &ImageCache::new(),
            0,
            &params,
            None,
            anchor,
            viewport_h,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        let n = doc.line_count();
        // Head virtualized: the band starts near the anchor, not at line 0.
        assert!(
            doc.measured_start > 0 && doc.measured_start <= anchor,
            "head should be virtualized around the anchor, got start={}",
            doc.measured_start
        );
        // Tail virtualized: the band ends past the anchor but well before the end.
        assert!(
            doc.measured_count > anchor && doc.measured_count < n,
            "band should cover the anchor and estimate the tail, got count={} of {n}",
            doc.measured_count
        );
        // Only band lines are laid out; head + tail are empty placeholders (height 0),
        // while the band's lines have real, non-zero layouts.
        assert_eq!(doc.layouts[0].height(), 0.0, "head line placeholder");
        assert_eq!(doc.layouts[n - 1].height(), 0.0, "tail line placeholder");
        assert!(
            doc.layouts[anchor].height() > 0.0,
            "anchor line materialized"
        );
        // The band is far smaller than the document (true O(visible)).
        assert!(
            doc.measured_count - doc.measured_start < 400,
            "materialized band {} should be a small window of {n}",
            doc.measured_count - doc.measured_start
        );
    }

    /// Re-pinning to a captured anchor keeps the anchor line's on-screen position fixed
    /// across a rebuild even when off-screen head heights change (the anti-jump core).
    #[test]
    fn anchor_repin_is_stable() {
        // Heights: first 100 lines are "wrong" in doc A (all 10px) vs doc B (all 40px);
        // the anchor line and below are identical. Pinning the anchor must cancel the
        // head delta so the anchor stays put.
        let mut ha = vec![10.0f32; 200];
        let mut hb = vec![40.0f32; 200];
        for i in 100..200 {
            ha[i] = 25.0;
            hb[i] = 25.0;
        }
        let mut a = fixture(&ha, 4.0, 4.0);
        let b = fixture(&hb, 4.0, 4.0);
        // Scroll doc A so line 100 sits 5px below the viewport top.
        a.scroll_y = a.tops[100] + 5.0;
        let (line, off) = a.scroll_anchor();
        assert_eq!(line, 100);
        assert!((off - 5.0).abs() < 1e-3);
        // Re-pin the same anchor in doc B (whose head is 3× taller).
        let repinned = b.anchor_scroll_y(line, off);
        // Line 100's top is 5px above the viewport top in BOTH — its on-screen position
        // (tops[100] - scroll_y) is identical, so the visible content did not jump.
        assert!((b.tops[100] - repinned - (a.tops[100] - a.scroll_y)).abs() < 1e-3);
    }

    /// The column pre-pass: `col_x` strictly increases, the widest cell drives its
    /// column's width, and the whole grid fits inside `max_advance`.
    #[test]
    fn table_layout_columns_and_widest_cell() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        // Column 1 has a very long body cell; column 0 is short throughout.
        let mut buf: Buffer =
            "| a | b |\n|---|---|\n| x | a much much much longer cell |\n| y | z |\n"
                .parse()
                .unwrap();
        let snap = buf.render_snapshot();
        let styles = snap.inline_styles_by_line();
        let params = test_params(&theme, 1200.0);
        let max_advance = 1000.0;
        let table = snap.table_containing_offset(0).unwrap();
        let tl = build_table_layout(
            &mut engine,
            &snap,
            &theme,
            table,
            &styles,
            &params,
            max_advance,
        );

        assert_eq!(tl.col_x.len(), 2);
        assert!(tl.col_x[1] > tl.col_x[0], "col_x strictly increasing");
        assert!(
            tl.col_w[1] > tl.col_w[0],
            "the long cell's column is wider: {:?}",
            tl.col_w
        );
        // Right edge of the last column's box (col origin + content width + cell padding
        // + trailing border) must fit the content width.
        let cell_pad_x = TABLE_CELL_PAD_X * params.scale;
        let border = TABLE_BORDER * params.scale;
        let total_w =
            tl.col_x.last().unwrap() + tl.col_w.last().unwrap() + 2.0 * cell_pad_x + border;
        assert!(
            total_w <= max_advance + 0.5,
            "grid fits the content width, total_w={total_w}"
        );
        // Header + two body rows → three row heights, all positive.
        assert_eq!(tl.row_heights.len(), 3);
        assert!(tl.row_heights.iter().all(|&h| h > 0.0));
    }

    /// A cell containing inline `` `code` `` carries its display-byte code ranges into the
    /// `CellSlot`, so the draw pass can paint the translucent chip behind the code glyphs.
    #[test]
    fn table_cell_inline_code_reaches_slot() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut buf: Buffer = "| a | b |\n|---|---|\n| plain | `code` |\n"
            .parse()
            .unwrap();
        let snap = buf.render_snapshot();
        let styles = snap.inline_styles_by_line();
        let params = test_params(&theme, 1200.0);
        let table = snap.table_containing_offset(0).unwrap();
        let tl = build_table_layout(&mut engine, &snap, &theme, table, &styles, &params, 1000.0);

        // row_layouts[1] is the single body row; col 1 holds `code`, col 0 is plain.
        let body = &tl.row_layouts[1];
        let code_cell = body[1].as_ref().expect("body code cell present");
        assert!(
            !code_cell.code_ranges.is_empty(),
            "inline-code cell must carry code ranges for the chip"
        );
        let plain_cell = body[0].as_ref().expect("body plain cell present");
        assert!(
            plain_cell.code_ranges.is_empty(),
            "a plain cell has no code ranges"
        );
    }

    /// The size cap: a table exceeding the body-row limit is not grid-rendered
    /// (`table_grid_ok` is false), so it falls through to raw pipe text.
    #[test]
    fn table_size_cap_engages() {
        use crate::buffer::Buffer;
        let mut doc = String::from("| a | b |\n|---|---|\n");
        for i in 0..(TABLE_MAX_BODY_ROWS + 5) {
            doc.push_str(&format!("| r{i} | v{i} |\n"));
        }
        let mut buf: Buffer = doc.parse().unwrap();
        let snap = buf.render_snapshot();
        let table = snap.table_containing_offset(0).unwrap();
        assert!(table.body.len() > TABLE_MAX_BODY_ROWS);
        assert!(
            !table_grid_ok(table),
            "oversized table must not grid-render"
        );

        // A small table is fine.
        let mut small: Buffer = "| a | b |\n|---|---|\n| 1 | 2 |\n".parse().unwrap();
        let snap2 = small.render_snapshot();
        assert!(table_grid_ok(snap2.table_containing_offset(0).unwrap()));
    }

    /// End-to-end grid layout: with the cursor off the table, the table's rows become
    /// `table_lines` entries, their text layouts are empty, `content_height`/`tops`
    /// reflect the grid row heights, and `tops` stays monotonic.
    #[test]
    fn doc_layout_grid_rows_off_cursor() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        // A heading, a paragraph, then a 2-col table (lines 4=header, 5=delim, 6,7=body).
        let mut buf: Buffer = "# Title\n\npara\n\n| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 | 4 |\n"
            .parse()
            .unwrap();
        let snap = buf.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let doc = DocLayout::build(
            &mut engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            &snap,
            &theme,
            None,
            None,
            &ImageCache::new(),
            0, // cursor at doc start, off the table
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        );
        // Table lines are the header/delimiter/body rows.
        for (line, want) in [
            (4, RowKind::Header),
            (5, RowKind::Delimiter),
            (6, RowKind::Body(0)),
            (7, RowKind::Body(1)),
        ] {
            let entry = doc.table_lines[line]
                .as_ref()
                .unwrap_or_else(|| panic!("line {line} should be a grid row"));
            assert_eq!(entry.1, want, "line {line} row kind");
            assert!(
                doc.renders[line].table.is_some(),
                "line {line} render flags a table row"
            );
            assert!(
                doc.renders[line].text.is_empty(),
                "line {line} text is hidden in grid mode"
            );
        }
        // Non-table lines carry no table entry.
        assert!(doc.table_lines[0].is_none());
        assert!(doc.table_lines[2].is_none());
        // `tops` is monotonic and content height is finite/positive.
        for w in doc.tops.windows(2) {
            assert!(w[1] >= w[0], "tops monotonic");
        }
        // Header row height comes from the grid, not a single text line.
        let header_h = doc.tops[5] - doc.real_top(4);
        assert!(header_h > 0.0);
        assert!(doc.content_height() > 0.0 && doc.content_height().is_finite());
    }

    /// The reveal flip: cursor off the table → every row is a grid row (text hidden);
    /// cursor on ANY table line → all rows revert to raw pipe text (no table ref).
    #[test]
    fn doc_layout_table_reveal_flip() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let params = test_params(&theme, 1200.0);
        let build = |engine: &mut TextEngine, cursor: usize| {
            DocLayout::build(
                engine,
                &mut LineCache::new(),
                &mut RenderCache::new(),
                &mut HeightCache::new(),
                &mut TableCache::new(),
                0,
                &snap,
                &theme,
                None,
                None,
                &ImageCache::new(),
                cursor,
                &params,
                None,
                0,
                f32::INFINITY,
                &[],
                &(0..0),
                &std::collections::HashMap::new(),
            )
        };
        // Cursor off the table (past the end): grid mode, header row hidden + flagged.
        let off = build(&mut engine, src.len());
        assert!(
            off.renders[0].table.is_some(),
            "header is a grid row off-cursor"
        );
        assert!(off.renders[0].text.is_empty(), "grid row text hidden");

        // Cursor inside a body row (line 3, offset ~30): all rows revert to raw text.
        let on = build(&mut engine, 30);
        for line in 0..4 {
            assert!(
                on.renders[line].table.is_none(),
                "line {line} reverts to raw text when the cursor is in the table"
            );
            assert!(
                !on.renders[line].text.is_empty(),
                "line {line} shows raw pipe text in reveal mode"
            );
            assert!(on.table_lines[line].is_none());
        }
    }

    /// Build a whole-document layout with the cursor off the table (grid mode).
    fn build_grid_doc(engine: &mut TextEngine, snap: &RenderSnapshot, cursor: usize) -> DocLayout {
        let theme = EditorTheme::dracula();
        let params = test_params(&theme, 1200.0);
        DocLayout::build(
            engine,
            &mut LineCache::new(),
            &mut RenderCache::new(),
            &mut HeightCache::new(),
            &mut TableCache::new(),
            0,
            snap,
            &theme,
            None,
            None,
            &ImageCache::new(),
            cursor,
            &params,
            None,
            0,
            f32::INFINITY,
            &[],
            &(0..0),
            &std::collections::HashMap::new(),
        )
    }

    /// Clicking a grid table row lands the caret inside the clicked cell (character-level
    /// via the cell's own layout + map), not at the line start / column 0.
    #[test]
    fn hit_test_lands_inside_clicked_table_cell() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        // Leading paragraph so the table isn't at offset 0; cursor 0 keeps it grid mode.
        let src = "para\n\n| aa | bb | cc |\n| --- | --- | --- |\n| 11 | 22 | 33 |\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let doc = build_grid_doc(&mut engine, &snap, 0);

        let body_line = 4;
        let (tl, kind) = doc.table_lines[body_line]
            .as_ref()
            .expect("body row is a grid row");
        assert_eq!(*kind, RowKind::Body(0));
        let line_start = snap.line_byte_range(body_line).start;
        let table = snap.table_containing_offset(line_start).unwrap();
        let y = doc.real_top(body_line) + 2.0;
        for c in 0..3 {
            let content = table.body[0].cells[c].content.clone();
            let x = doc.pad_x + tl.col_x[c] + doc.table_cell_pad_x + tl.col_w[c] * 0.5;
            let off = doc.hit_test(x, y).expect("hit-test returns an offset");
            assert!(
                off >= content.start && off <= content.end,
                "click on column {c} lands in its cell content {content:?}, got {off}"
            );
            assert_ne!(off, line_start, "click does not collapse to the line start");
        }
    }

    /// Tier-2 character-level: clicking nearer a cell's end yields a larger buffer offset
    /// than clicking nearer its start.
    #[test]
    fn hit_test_table_cell_start_before_end() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let src = "para\n\n| aaaa | bbbb |\n| --- | --- |\n| wxyz | 2222 |\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let doc = build_grid_doc(&mut engine, &snap, 0);

        let (tl, _) = doc.table_lines[4].as_ref().unwrap();
        let y = doc.real_top(4) + 2.0;
        let base = doc.pad_x + tl.col_x[0] + doc.table_cell_pad_x;
        let near_start = doc.hit_test(base + 1.0, y).unwrap();
        let near_end = doc.hit_test(base + tl.col_w[0] - 1.0, y).unwrap();
        assert!(
            near_end > near_start,
            "click near the cell end ({near_end}) is past click near its start ({near_start})"
        );
    }

    /// Clicks left of / right of the grid clamp to the first / last column.
    #[test]
    fn hit_test_table_clamps_to_edge_columns() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let src = "para\n\n| aa | bb | cc |\n| --- | --- | --- |\n| 11 | 22 | 33 |\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let doc = build_grid_doc(&mut engine, &snap, 0);

        let line_start = snap.line_byte_range(4).start;
        let table = snap.table_containing_offset(line_start).unwrap();
        let y = doc.real_top(4) + 2.0;
        let first = table.body[0].cells[0].content.clone();
        let last = table.body[0].cells[2].content.clone();

        let left = doc.hit_test(0.0, y).unwrap();
        assert!(
            left >= first.start && left <= first.end,
            "far-left click clamps to the first cell {first:?}, got {left}"
        );
        let right = doc.hit_test(1199.0, y).unwrap();
        assert!(
            right >= last.start && right <= last.end,
            "far-right click clamps to the last cell {last:?}, got {right}"
        );
    }

    /// Clicking an empty grid cell lands the caret between its pipes (on a space), not on
    /// a pipe.
    #[test]
    fn hit_test_empty_table_cell_lands_off_pipe() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let src = "para\n\n| a | b |\n| --- | --- |\n|  |  |\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let doc = build_grid_doc(&mut engine, &snap, 0);

        let body_line = 4;
        let (tl, _) = doc.table_lines[body_line].as_ref().unwrap();
        let y = doc.real_top(body_line) + 2.0;
        let x = doc.pad_x + tl.col_x[0] + doc.table_cell_pad_x + tl.col_w[0] * 0.5;
        let off = doc.hit_test(x, y).unwrap();
        assert_eq!(
            snap.rope.get_byte(off),
            Some(b' '),
            "empty-cell click lands on a space between the pipes, not on a pipe"
        );
    }
}
