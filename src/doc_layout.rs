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

use parley::{Affinity, Cluster, Cursor, PositionedLayoutItem, Selection};
use vello::Scene;
use vello::kurbo::{Affine, Rect, Stroke};
use vello::peniko::{Brush, Color, Fill, ImageBrush};

use crate::buffer::RenderSnapshot;
use crate::consts::{MAX_CONTENT_WIDTH, UI_LINE_HEIGHT};
use crate::diff::{DiffState, InlineChange};
use crate::editor::EditorTheme;
use crate::github::GitHubValidationCache;
use crate::image_cache::{ImageCache, ImageState};
use crate::inline::{
    GitHubContext, NakedUrl, RawGitHubMatch, StyledRegion, github_refs_to_styled_regions,
    naked_urls_to_styled_regions,
};
use crate::render::{ImageRef, InlineImageRef, LineRender, build_line_render};
use crate::segment_map::SegmentMap;
use crate::text_engine::{StyleRun, TextEngine, peniko_color, peniko_color_alpha};

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

/// Stable per-frame layout constants: padding and font in logical px, plus
/// `device_width`, `scale`, and the theme foreground baked to a `Color`. Bundled
/// so `DocLayout::build` isn't a wall of `f32`s; all are fixed for a given surface
/// and theme. Viewport state (`measure_to_y`) is deliberately kept a separate arg.
pub struct LayoutParams {
    pub device_width: f32,
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
fn build_ghosts_before(
    engine: &mut TextEngine,
    diff: Option<&DiffState>,
    new_line: usize,
    theme: &EditorTheme,
    params: &LayoutParams,
    max_advance: f32,
) -> Vec<Ghost> {
    let LayoutParams {
        scale,
        base_font_size,
        line_height,
        fg,
        ..
    } = *params;
    let Some(d) = diff else { return Vec::new() };
    let Some(old_range) = d.ghost_lines_before(new_line) else {
        return Vec::new();
    };
    let old = &d.old_snapshot;
    let mut out = Vec::new();
    for old_line in old_range {
        if old_line >= old.line_count() {
            break;
        }
        // Only the few visible ghost lines need styling, so compute per line instead
        // of bucketing the entire HEAD snapshot on every rebuild.
        let styles = old.tree_styles_for_line(old_line);
        let lr = build_line_render(old, old_line, theme, base_font_size, usize::MAX, &styles, &[]);
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
        out.push(Ghost {
            height: layout.height(),
            layout,
            inline,
        });
    }
    out
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
                    let brush =
                        Some((loaded.brush.clone(), loaded.width as f32, loaded.height as f32));
                    (w, h, InlineImageDraw { brush })
                }
                _ => (
                    INLINE_IMG_PLACEHOLDER_W * scale,
                    INLINE_IMG_PLACEHOLDER_H * scale,
                    InlineImageDraw { brush: None },
                ),
            };
            inline_boxes.push((ii.display_offset, w, h));
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
    /// Vertical padding (device px) above/below an image block, and the placeholder
    /// box colors — baked from `scale`/theme at build so the draw pass is self-contained.
    img_vpad: f32,
    img_label_size: f32,
    image_border: Color,
    image_bg: Color,
    /// Background chip painted behind inline `code` spans.
    code_bg: Color,
    /// Per-line x-offsets (from the line origin) of each blockquote gutter rule, parallel
    /// to `layouts`. Empty for non-quote lines. Painted as continuous vertical rects.
    quote_bars: Vec<Vec<f32>>,
    /// Total ghost-block height above each real line, parallel to `layouts`.
    ghost_height: Vec<f32>,
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
    /// Surface width in device px (for full-width diff row backgrounds).
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
    ) -> Self {
        let LayoutParams {
            device_width,
            scale,
            pad_x,
            pad_top,
            pad_bottom,
            base_font_size,
            line_height,
            fg,
        } = *params;
        // Cap the body width for readability and center it: the left margin is the base
        // padding, widened to a centering inset once the window exceeds MAX_CONTENT_WIDTH
        // plus that padding. `left` is the draw origin and both margins for `max_advance`.
        let left = (pad_x * scale).max((device_width - MAX_CONTENT_WIDTH * scale) / 2.0);
        let max_advance = (device_width - 2.0 * left).max(1.0);
        let n = snapshot.line_count();
        cache.begin();
        render_cache.begin();
        height_cache.begin();
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
        let mut preedit_caret: Option<(usize, usize)> = None;
        // Bucket inline styles per line once (O(n + styles)) instead of the O(n²)
        // per-line `styles_in_range` scan — the dominant per-keystroke cost on large
        // docs. (Ghost lines style themselves lazily; see `build_ghosts_before`.)
        let line_styles = snapshot.inline_styles_by_line();
        let mut layouts = Vec::with_capacity(n);
        let mut renders = Vec::with_capacity(n);
        let mut line_ranges = Vec::with_capacity(n);
        let mut line_diffs = Vec::with_capacity(n);
        let mut ghosts = Vec::with_capacity(n);
        let mut ghost_height = Vec::with_capacity(n);
        let mut quote_bars: Vec<Vec<f32>> = Vec::with_capacity(n);
        let mut image_blocks: Vec<Option<ImageBlock>> = Vec::with_capacity(n);
        let mut inline_draws: Vec<Vec<InlineImageDraw>> = Vec::with_capacity(n);
        let mut image_urls: Vec<String> = Vec::new();
        // Content width available to an image (device px), same basis as `max_advance`.
        let content_w = max_advance;
        let img_vpad = IMG_VPAD * scale;
        // Each line's total height = its ghost block above + the real line.
        let mut heights = Vec::with_capacity(n);
        // `i` indexes several parallel per-line inputs (styles, markers, diff).
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
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
                let rkey = render_key(version, i, cursor_key_for(&range, cursor_offset));
                let h = height_cache.get(rkey).unwrap_or_else(|| {
                    let byte_len = range.len().saturating_sub(1) as f32; // minus trailing '\n'
                    let est_rows = (byte_len * k / max_advance).ceil().max(1.0);
                    est_rows * min_row
                });
                heights.push(h);
                layouts.push(empty_layout.clone());
                renders.push(empty_render.clone());
                line_ranges.push(range);
                line_diffs.push(LineDiff::default());
                ghosts.push(Vec::new());
                ghost_height.push(0.0);
                quote_bars.push(Vec::new());
                image_blocks.push(None);
                inline_draws.push(Vec::new());
                continue;
            }
            // Ghost (deleted) lines rendered before this line, from the HEAD snapshot.
            let line_ghosts = build_ghosts_before(engine, diff, i, theme, params, max_advance);
            let gh: f32 = line_ghosts.iter().map(|g| g.height).sum();

            // Line byte range + render key, computed once and shared by the render cache,
            // the height cache, and the diff mapping below. (`line_markers(i).range` is
            // byte-identical but far pricier — it runs full marker extraction per line.)
            let range = snapshot.line_byte_range(i);
            let rkey = render_key(version, i, cursor_key_for(&range, cursor_offset));

            let extra = github.map(|g| g.extra_regions(i)).unwrap_or_default();
            // Reuse the cached render when nothing about this line changed. Lines with
            // GitHub extra regions bypass the cache (validation can change them without
            // a version bump); all others key on (version, line, cursor-on-line).
            let lr = if extra.is_empty() {
                render_cache.get_or_build(rkey, || {
                    Rc::new(build_line_render(
                        snapshot,
                        i,
                        theme,
                        base_font_size,
                        cursor_offset,
                        &line_styles[i],
                        &[],
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
                ))
            };
            let (inline_boxes, line_inline_draws) =
                resolve_inline_images(&lr.inline_images, images, &mut image_urls, max_advance, scale);
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
                    let caret_off =
                        caret_disp + pv.cursor.map(|(s, _)| s).unwrap_or(pv.text.len());
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
                .filter_map(|&b| Cluster::from_byte_index(&layout, b).and_then(|c| c.visual_offset()))
                .collect();
            // Standalone image: the block (image or placeholder) sets the line height in
            // place of the (empty) text layout, and the URL is collected for loading.
            let (image_block, line_h) = match &lr.image {
                Some(img) => {
                    if !image_urls.iter().any(|u| u == &img.url) {
                        image_urls.push(img.url.clone());
                    }
                    let (block, block_h) =
                        build_image_block(images, img, content_w, scale, img_vpad);
                    (Some(block), block_h)
                }
                None => (None, layout.height()),
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
        }
        cache.sweep();
        render_cache.sweep();
        height_cache.sweep();
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
            quote_bars,
            image_blocks,
            inline_draws,
            image_urls,
            img_vpad,
            img_label_size: 14.0 * scale,
            image_border: peniko_color(theme.comment),
            image_bg: peniko_color_alpha(theme.comment, 0.08),
            code_bg: peniko_color_alpha(theme.comment, 0.22),
            diff_colors,
            // A thin rule (~1/6 of a mono cell), floored so it stays visible when small.
            quote_bar_width: (k * 0.16).max(1.5),
            quote_bar_color: peniko_color(theme.comment),
            tops: compute_tops(&heights, pad_top * scale),
            measured_start: measured_start.min(measured_count),
            measured_count,
            preedit_caret,
            scroll_y: 0.0,
            width: device_width,
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
    fn line_of(&self, buffer_off: usize) -> usize {
        let n = self.line_ranges.len();
        if n == 0 {
            return 0;
        }
        self.line_ranges
            .partition_point(|r| r.start <= buffer_off)
            .saturating_sub(1)
            .min(n - 1)
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
        self.renders
            .get(line)
            .is_some_and(|r| r.image.is_some())
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
    fn caret_rect_at(&self, line: usize, display_off: usize, caret_width: f32) -> Option<ScreenRect> {
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
            let sel = Selection::new(
                Cursor::from_byte_index(layout, ds, Affinity::Downstream),
                Cursor::from_byte_index(layout, de, Affinity::Upstream),
            );
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
        let line = self.tops[1..].partition_point(|&b| b <= self.scroll_y).min(n - 1);
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
        self.tops.last().copied().unwrap_or(self.pad_top) + self.pad_bottom
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

    /// Fill the word-change rects of `ranges` within `layout`, offset to `top_y`.
    fn fill_word_ranges(
        &self,
        scene: &mut Scene,
        layout: &parley::Layout<Brush>,
        ranges: &[Range<usize>],
        top_y: f64,
        color: Color,
    ) {
        for r in ranges {
            let sel = Selection::new(
                Cursor::from_byte_index(layout, r.start, Affinity::Downstream),
                Cursor::from_byte_index(layout, r.end, Affinity::Upstream),
            );
            for (bb, _) in sel.geometry(layout) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    color,
                    None,
                    &Rect::new(
                        bb.x0 + self.pad_x as f64,
                        bb.y0 + top_y,
                        bb.x1 + self.pad_x as f64,
                        bb.y1 + top_y,
                    ),
                );
            }
        }
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
                let y = gy as f64 + pib.y as f64;
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
            // Ghost (deleted) rows stacked in this line's ghost block.
            let mut gy = self.tops[i] - self.scroll_y;
            for ghost in &self.ghosts[i] {
                let top = gy as f64;
                let bottom = (gy + ghost.height) as f64;
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    self.diff_colors.deleted_bg,
                    None,
                    &Rect::new(self.pad_x as f64, top, (self.width - self.pad_x) as f64, bottom),
                );
                self.fill_word_ranges(
                    scene,
                    &ghost.layout,
                    &ghost.inline,
                    top,
                    self.diff_colors.deleted_inline,
                );
                engine.draw_line(scene, &ghost.layout, (self.pad_x, gy));
                gy += ghost.height;
            }
            // Background chips behind inline code, under the real line's glyphs.
            let code = &self.renders[i].code_ranges;
            if !code.is_empty() {
                self.fill_word_ranges(scene, &self.layouts[i], code, gy as f64, self.code_bg);
            }
            // Inline images: paint each at its laid-out box position (a loaded image, or a
            // faint bordered placeholder). Parley left a gap in the glyphs where the box is.
            self.draw_inline_images(scene, i, gy);
            // The real line, below its ghost block.
            engine.draw_line(scene, &self.layouts[i], (self.pad_x, gy));
        }
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

    /// Distinct standalone-image URLs across the materialized lines. The shell diffs
    /// these against the shared cache to spawn loads.
    pub fn image_urls(&self) -> &[String] {
        &self.image_urls
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
                        if failed { "broken image" } else { "loading image…" }.to_string()
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
                &Rect::new(self.pad_x as f64, top, (self.width - self.pad_x) as f64, bottom),
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
    use crate::consts::{FONT_SIZE, LINE_HEIGHT, PADDING};
    use crate::text_engine::peniko_color;

    /// Device-px content width shared by the image-sizing tests (scale 1.0).
    const TEST_CONTENT_W: f32 = 800.0;

    /// Frame constants for the headless tests — the same source of truth the shell uses,
    /// so the tests track any change to the real frame rather than duplicating literals.
    fn test_params(theme: &EditorTheme, device_width: f32) -> LayoutParams {
        LayoutParams {
            device_width,
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
        assert!((dest_w - content_w).abs() < 0.5, "wide image fills content width");
        assert!((dest_h - content_w * 100.0 / 2000.0).abs() < 0.5, "aspect preserved");

        // Tall but wide enough: 1000x2000 → still fills the width (no cap), height
        // follows aspect (1600), rather than being shrunk to fit a height limit.
        let big = cache_image(&cache, "big", 1000, 2000);
        let (block, _) = build_image_block(&cache, &big, content_w, 1.0, vpad);
        let ImageBlockKind::Loaded { dest_w, dest_h, .. } = block.kind else {
            panic!("expected loaded");
        };
        assert!((dest_w - content_w).abs() < 0.5, "fills width even when tall");
        assert!((dest_h - content_w * 2000.0 / 1000.0).abs() < 0.5, "height uncapped");

        // Small: 40x30, under the width → shown at intrinsic size (no upscale).
        let small = cache_image(&cache, "small", 40, 30);
        let (block, block_h) = build_image_block(&cache, &small, content_w, 1.0, vpad);
        let ImageBlockKind::Loaded { dest_w, dest_h, .. } = block.kind else {
            panic!("expected loaded");
        };
        assert_eq!((dest_w, dest_h), (40.0, 30.0), "small image is not upscaled");
        assert!((block_h - (30.0 + 2.0 * vpad)).abs() < 0.5, "block adds vertical padding");
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
        assert_eq!(out_runs[1].range, 6..8, "run at/after caret shifts by preedit.len()");
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
            layouts: heights.iter().map(|_| Rc::new(parley::Layout::new())).collect(),
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
                    })
                })
                .collect(),
            line_ranges: heights.iter().map(|_| 0..0).collect(),
            line_diffs: heights.iter().map(|_| LineDiff::default()).collect(),
            ghosts: heights.iter().map(|_| Vec::new()).collect(),
            ghost_height: heights.iter().map(|_| 0.0).collect(),
            quote_bars: heights.iter().map(|_| Vec::new()).collect(),
            image_blocks: heights.iter().map(|_| None).collect(),
            inline_draws: heights.iter().map(|_| Vec::new()).collect(),
            image_urls: Vec::new(),
            img_vpad: 0.0,
            img_label_size: 14.0,
            image_border: Color::TRANSPARENT,
            image_bg: Color::TRANSPARENT,
            code_bg: Color::TRANSPARENT,
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
                engine, cache, &mut RenderCache::new(), &mut HeightCache::new(), 0, &snapshot,
                &theme, None, None, &ImageCache::new(), 0, &params, None, 0, f32::INFINITY,
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
                    engine, lc, rc, &mut HeightCache::new(), 7, &snapshot, &theme, None, None,
                    &ImageCache::new(), cursor, &params, None, 0, f32::INFINITY,
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
        assert!(doc.layouts[anchor].height() > 0.0, "anchor line materialized");
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
}
