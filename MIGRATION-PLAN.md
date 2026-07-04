# writ: gpui → winit+wgpu+Vello+Parley migration plan

> **Status: COMPLETE.** This migration shipped — gpui is fully removed and writ runs on
> winit + wgpu + Vello + Parley (all 8 phases done). This document is kept as a historical
> record of the plan; see the README for the current architecture.

## Strategy

This is a one-shot clean cut: gpui, gpui_platform, and the `[patch.crates-io]` block are deleted at the end, and no gpui-compat shim runs in parallel during the migration. We take a walking-skeleton approach — get pixels on screen against the real published stack (winit 0.30 + wgpu 29 + Vello 0.9 + Parley 0.11) as fast as possible with a hard-coded string (Phase 0–1), then layer document rendering, scrolling, input, diff, and chrome onto that skeleton one independently-runnable milestone at a time. The ~8600 lines of gpui-free logic (`marker.rs`, `parser.rs`, `cursor.rs`, `highlight.rs`, `inline.rs` with its `full_range`/`content_range` hidden-marker model, `buffer.rs` rope+`RenderSnapshot`, `diff.rs`, `github.rs`/`git.rs`, `paste.rs`, `demo.rs`, `writd.rs`) stay untouched as pure logic and are the ballast that makes this feasible; the work is concentrated in `line.rs`, the `Render` half of `editor/mod.rs`, `main.rs`, and the chrome files. The load-bearing new invariant threaded through every render/input phase is the **display→buffer segment map** that replaces gpui's `buffer_to_visual_pos`/`visual_to_buffer_pos`, because Parley lays out the *display* string (markers hidden) while cursor/selection/click/diff math all speak *buffer* bytes.

**Read this first — version reconciliation is DONE (spike ran 2026-07-01, `~/Documents/writ-spike`).** A throwaway crate pinning the exact published stack (`parley 0.11.0`, `vello 0.9.0`, `wgpu 29.0.3`, `winit 0.30.13`, `fontique 0.11.0`) was compiled and run on the M2 (Asahi/Mesa/Vulkan). Resolved facts — trust these over any clone-derived note below:
- **HARD GATE PASSED:** `parley::CHROMIUM_LINE_BREAK_OVERRIDE` and `RangedBuilder::set_line_break_override` DO exist on published parley 0.11 (`break_overrides.rs:55`, `builder.rs:58`). No `parley_core` split problem. Browser-grade wrapping is a go, no version bump needed. The setter is on the **builder, before `build()`** (NOT a `Layout` method): `builder.set_line_break_override(Some(CHROMIUM_LINE_BREAK_OVERRIDE));`.
- **`ranged_builder` HAS the 4th `quantize: bool` arg on published 0.11** (`context.rs:87`): `lcx.ranged_builder(&mut fcx, text, scale, true)`. The summary claim that published 0.11 *drops* it was WRONG; the detailed API tables at the bottom of this doc were right. Use `quantize=true`.
- **`Scene::draw_glyphs(&FontData)` confirmed** (vello `scene.rs:455`), and `parley::Run::font()` returns `&FontData` (`run.rs:45`) — they line up; pass `run.font()` straight in. Do NOT copy the vello_editor example's `&Font` (that's the 0.8 path).
- **NEW — wgpu 29 present path:** `Surface::get_current_texture()` returns a `CurrentSurfaceTexture` **enum, not `Result`** (`surface_texture.rs:55`). Match `Success(t) | Suboptimal(t) => t`, skip frame on `Timeout/Occluded/Outdated/Lost/Validation`. Present = render to `RenderSurface.target_view` via `Renderer::render_to_texture` → `RenderSurface.blitter.copy(...)` into the frame view → `surface_texture.present()`. `vello::util::{RenderContext, RenderSurface}` provides `create_surface`/`resize_surface` and owns `target_texture`/`target_view`/`blitter` — use it, don't hand-roll the wgpu instance/adapter/device. `RendererOptions` fields: `{ use_cpu, antialiasing_support, num_init_threads, pipeline_cache }`.
- **GPU path proven on Asahi:** winit window + wgpu Vulkan adapter (device 0) + Vello render/blit/present ran for 5s with zero errors. `WGPU_BACKEND=vulkan` selects the Asahi ICD.

The working, compiling call sites for all of the above live in `~/Documents/writ-spike/src/{main.rs,text_probe.rs}` — copy from there into Phase 0/1 rather than re-deriving.

## Phases

### Phase 0 — Walking-skeleton shell: blank window clears to a color via Vello
Draws from: `logic-integration-teardown` (async runtime, deps, rustls), `render-shell` (implicit across all specs).

**API spike: DONE** (see the resolved-facts block under Strategy). The working shell + text call sites are in `~/Documents/writ-spike`; lift them into writ rather than re-deriving. What remains in Phase 0 is wiring that proven shell into writ's tree additively (deps + tokio runtime + rustls), not API discovery.

Tasks (ordered):
1. Add `parley`, `vello`, `wgpu`, `winit`, and `fontique = { version = "0.11", features = ["fontconfig-dlopen"] }` to `Cargo.toml`. Do **not** yet remove gpui — logic still references it until Phase 2. (This is additive; a mixed build is expected mid-migration.)
2. Create a new winit `ApplicationHandler`: `resumed` builds a `Window` + wgpu `Instance`/`Surface`/`Device`/`Queue` (prefer Vulkan backend for Mesa/Asahi) + a Vello `Renderer`. `RedrawRequested` → `Scene::reset()`, fill the whole surface with a clear color, `render_to_texture`, present. `Resized` → reconfigure surface.
3. Stand up an owned **tokio** runtime in the shell entrypoint and move `rustls::crypto::ring::default_provider().install_default()` (was `main.rs:125`) here so HTTPS survives gpui removal. This unblocks dropping `async_compat`'s `.compat()` later.
4. Keep `vello_cpu`/`vello_hybrid` behind a cargo feature (separate crates under `vello/sparse_strips`, not vello features) wired to the same `Scene`, so an Asahi/Vulkan fallback exists from day one.

Definition of Done: `cargo run` opens a resizable window that clears to a solid color via Vello on the M2/Mesa/Vulkan path; the CPU-fallback feature also clears the window; a tokio runtime is live.

Top risks: Vello-on-Asahi (Vulkan via Mesa) surface-format/present-mode issues — mitigate by validating the CPU fallback in the same phase. Version reconciliation surprises — the spike de-risks this before any porting.

---

### Phase 1 — One static syntax-highlighted, Chromium-wrapped markdown line
Draws from: `text-core`, `linebreak-fidelity`, `logic-integration-teardown` (theme/config deglue).

Tasks:
1. Introduce `TextEngine { fcx: FontContext, lcx: LayoutContext<Brush> }` where `Brush = vello::peniko::Brush`. Own it in the shell, pass `&mut` into text-core. Hold `CHROMIUM_LINE_BREAK_OVERRIDE` directly — it is already `&'static LineBreakOverrideFn`, no leak/Box.
2. **Deglue `editor/theme.rs`** now (it is pure color data): `gpui::Rgba/rgb` → `vello::peniko::Color` via `Color::from_rgba8(r,g,b,0xFF)`; delete the `Global` impl. Add `hsla_to_alphacolor` / theme-color→`peniko::Brush::Solid` helpers. Integer color values map 1:1; re-audit any 0..1 channel blending.
3. **Deglue `editor/config.rs`**: `Rems`/`Pixels`/`px` → plain `f32` logical px. Convert the *actual* defaults `Rems(0.0)/1.6/4.8/1.6` by ×~16.0 rem base (NOT 1:1 — that shrinks padding ~10×). `max_line_width: Option<Pixels>` → `Option<f32>`.
4. Build `build_line_layout` core over a hard-coded markdown string: `lcx.ranged_builder(...)`, `push_default` (base family/size/brush/line-height), `set_line_break_override(Some(CHROMIUM_LINE_BREAK_OVERRIDE))` **before** `build`, `break_all_lines(Some(max_advance))`, `align(...)`. Use `StyleProperty::FontWeight(BOLD)`, `FontStyle::Italic`, `FontFamily(GenericFamily::Monospace.into())` (NOTE: `FontFamily` has **no** `Named` variant; named families go through `FontFamily::named()`/`FontFamilyName::Named`), `Brush`, `Underline`, `Strikethrough`.
5. Port the draw pass from `vello_editor/src/text.rs` (lines ~399–466) into `draw_line(scene, &LineLayout, origin)`: iterate `layout.lines()` → `line.items()` → `PositionedLayoutItem::GlyphRun` → `scene.draw_glyphs(font).brush(&style.brush).font_size(fs).transform(t).glyph_transform(synthesis.skew().map(|a| Affine::skew(a.to_radians().tan() as f64, 0.0))).normalized_coords(run.normalized_coords()).draw(Fill::NonZero, glyphs)`. Underline/strikethrough via `Scene::stroke` off `RunMetrics`.
6. Resolve Fontique families for `DEFAULT_TEXT_FONT`/`DEFAULT_CODE_FONT` (Liberation Sans/Mono) on Asahi; decide whether to bundle fonts for reproducible Chromium wrapping.

Definition of Done: the window renders one static, correctly syntax-highlighted markdown line whose soft-wrap points obey the Chromium override (a golden test asserts no break before `) ] } . , : ;` nor after `( [ {`, and non-ASCII quotes/CJK brackets defer to UAX-14).

Top risks: font resolution on Asahi (bold/italic synthesis silently no-ops if faces missing in Fontique — verify). `quantize`/scale mismatch on HiDPI blurs glyphs or wraps at wrong width. Bundle-vs-system fonts affects wrap reproducibility.

---

### Phase 2 — Headless plain `Editor` struct (gpui Entity removed from logic)
Draws from: `editor-core` (RE-PLUMB), `logic-integration-teardown` (http re-point, config/action deglue).

Tasks:
1. Lift the non-render half of `editor/mod.rs` out of the gpui `Entity`/`Context<Self>` into a plain `struct Editor` owning `Buffer`, `Cursor`/`Selection`, autocomplete state machine, github-ref detection, `DiffState` management, checkbox propagation, file-watching, and save. Every `dispatch_action` becomes a direct `Editor` method.
2. **Rewrite `http.rs`**: drop the `gpui::http_client::{HttpClient, AsyncBody, Request, Response, ...}` trait impl for a thin inherent wrapper — `pub struct HttpClient { client: reqwest::Client }` with `async fn get_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>>` = `client.get(url).send().await?.error_for_status()?.bytes().await?.to_vec()`, Chrome UA folded into the builder. Remove `async_compat` `.compat()` from `http.rs` **and** `github.rs` (now on real tokio). `Editor` holds `Arc<HttpClient>`; `GitHubClient` stays by value.
3. **Deglue `config.rs`** (delete `Global` impl) and `editor/action.rs` (`gpui::Point<gpui::Pixels> hovered_ref_position` → `(f32, f32)`; audit for any gpui `Action` derive and remove if present). `EditorAction` stays a plain enum for the winit key handler.
4. Confirm `buffer/cursor/marker/diff/highlight/inline` compile with zero gpui refs once the Entity is gone.

Definition of Done: `Editor::new(&str) -> Editor` exists; a headless test opens a doc, applies edit-ops, toggles a checkbox, detects a github ref, and computes diff state — all with no gpui in the call graph; `cargo test` on the ported `cursor.rs`/`inline.rs` logic passes. HTTP image fetch and GitHub GraphQL run on tokio without `.compat()`.

Top risks: dropping `.compat()` before the tokio reactor exists compiles but panics — Phase 0 already installed the runtime, keep the ordering. The autocomplete/hover popup anchor (old `CursorScreenPosition`/`HoveredRefScreenPosition` gpui Globals) must become **returned geometry**, not a global — thread the contract now even though paint returns it in Phase 3–4.

---

### Phase 3 — Full document renders and scrolls
Draws from: `viewport-scroll`, `text-core`.

Tasks:
1. Replace `list_state: ListState` with `viewport: Viewport { scroll_y: f32, size: (f32,f32) }` + `line_layouts: Vec<LineEntry { layout: Option<Layout<Brush>>, height: f32, content_hash, dirty }>` + prefix-sum `line_tops: Vec<f32>` (len n+1). Total height = padding_top + `line_tops[n]` + padding_bottom.
2. `ensure_line_laid_out(i, width)`: build via `build_line_layout` from Phase 1, cache; `rebuild_line_tops()` on any height change.
3. `resize_and_invalidate(buffer_version)` replaces `sync_list_state` — resize to `buffer.line_count()`, mark edited window dirty. **Invalidate a conservative block window, not just the edited line** (fenced code / list continuation / blockquotes restyle neighbors); key the cache on per-line `StyledRegion` hash to catch cross-line restyle.
4. `visible_line_range()` via `partition_point` on `line_tops`; render loop iterates visible lines, `Affine::translate((padding_x, line_top(i) - scroll_y))`, `draw_line`. `WindowEvent::MouseWheel` → `viewport.scroll_by` (clamp `[0, (total-size.1).max(0)]`) → `request_redraw`. `Resized` → update size, mark all dirty, rebuild tops.
5. Full display→buffer `segment` map produced per `LineLayout` (from the ported `build_styled_content` hidden-marker/collapsed logic) — load-bearing for Phase 4/5.

Definition of Done: a long markdown document renders with correct variable-height lines (headings, wrapped paragraphs) and scrolls smoothly via trackpad wheel; scrollbar thumb tracks; a resize re-wraps and re-lays out.

Top risks: no lazy height measurement in Parley — laying out all lines up front is O(n) on big docs; mitigate with estimate-then-refine and reconcile `scroll_y` when above-viewport estimates correct (else scrollbar jitters). Off-by-one in `line_tops` (n vs n+1) misplaces everything below. Re-measuring dirty lines needs `&mut LayoutContext` — do it in a pre-pass, not inside the immutable draw iteration.

---

### Phase 4 — Typing, cursor, selection, click/drag hit-testing, IME
Draws from: `text-core`, `viewport-scroll`.

Tasks:
1. Wire winit `KeyboardInput` + `Ime` events → `Editor` edit-ops. **IME** (`Ime::Preedit`/`Commit`) needs `Window::set_ime_allowed(true)` and `set_ime_cursor_area`; render preedit as a styled underlined run.
2. Cursor render: `Cluster::from_byte_index(layout, display_off).visual_offset()` + `line.metrics()` baseline/ascent/descent → thin `kurbo::Rect` `Scene::fill`. Return the cursor's absolute screen rect from the paint pass for popup anchoring (fulfills the Phase 2 geometry contract; deletes `CursorScreenPosition` global).
3. Selection: **delete** `apply_selection_to_runs`/`apply_background_to_runs` run-splitting. Map selection buffer range → display range via segment map, assemble fill rects from cluster advances (Parley has no per-run background), `Scene::fill` **before** glyphs.
4. Hit-testing: `x_to_offset` via `Cluster::from_point(layout, x, y).text_range().start` → segment map → buffer offset; collapsed-URL runs map proportionally via `Cluster::advance()`. Wire click/drag/hover/link-open/Ctrl-click/checkbox-toggle into winit pointer handlers as direct `Editor` calls. Point→line via `partition_point` on `line_tops` then `Cluster::from_point`.
5. Scroll-to-cursor: `byte_to_line` → `line_top` → `Cursor::from_byte_index` + `geometry(...)` sub-row band (not whole line, for tall wrapped lines) → reveal with clamp + near-bottom nudge. Port drag auto-scroll and click-below-content.
6. Markers: blockquote bar / indent bg / list bullet / ordered marker — bar+indent as `Scene::fill(Rect)`, bullet as a prepended styled run or mini-layout; `marker_offset_px` shifts the glyph `Affine` origin **and** reduces `break_all_lines` max_advance so wrap-indent matches. `monospace_char_width` from a one-glyph Parley layout of `'M'`.
7. Delete `buffer_to_visual_pos`, `visual_to_buffer_pos`, `hidden_bytes_before`, `shape_line`, `CollapsedDisplayText::map_x_to_buffer_offset`; port line.rs unit tests to assert segment-map round-trips.

Definition of Done: typing (incl. multibyte/emoji), selection, click-to-place, drag-select, IME preedit/commit, link open, and checkbox toggle all work; cursor stays visible on scroll-to-cursor in wrapped lines.

Top risks: **IME** correctness across Wayland/X11 on Asahi (test both). The segment map is now load-bearing for *all* cursor math — multibyte/collapsed-URL off-by-ones misplace the caret; the round-trip tests are the safety net. Half-open selection edges at wrap boundaries need care.

---

### Phase 5 — Inline git diff renders
Draws from: `inline-git-diff`.

Tasks:
1. `diff.rs` stays verbatim; stop cloning `DiffState` per frame — borrow `&Option<DiffState>` in the render loop.
2. Precompute 4 `peniko::AlphaColor<Srgb>` with alpha baked in (`diff_added_bg` 0.05, `diff_deleted_bg` 0.05, `diff_added_inline` 0.25, `diff_deleted_inline` 0.25) from `theme.red/green`.
3. `draw_line_bg(scene, layout, row_transform, width, brush)`: `let m = layout.get(0).unwrap().metrics();` (NOT `layout.lines()[0]` — `lines()` is an iterator) → fill `Rect::new(0.0, m.block_min_coord, max_line_width, m.block_max_coord)`.
4. `draw_range_fills(scene, layout, row_transform, start, end, brush)`: `Selection::new(Cursor::from_byte_index(layout, start, Affinity::Downstream), Cursor::from_byte_index(layout, end, Affinity::Upstream))` → `geometry_with(layout, |bb, _| scene.fill(Fill::NonZero, row_transform, brush, None, &Rect::new(bb.x0,bb.y0,bb.x1,bb.y1)))`. Handle **N rects per range** (Chromium wrap splits a logical line). Map `InlineChange` line-relative RAW byte ranges through the **same buffer→display segment map** from Phase 3 for both live and `old_snapshot`.
5. Additions: `is_addition(ix)` → `draw_line_bg(diff_added_bg)` before glyphs; `new_inline_changes(ix)` → `draw_range_fills(diff_added_inline)`.
6. Ghost (deleted) lines: `ghost_lines_before(ix)` → build/**cache** a Parley `Layout` per `old_snapshot` line (keyed by snapshot generation + `old_ix`, invalidated on `recompute_diff`), reserve a virtual row of that height, `draw_line_bg(diff_deleted_bg)`, `draw_range_fills(diff_deleted_inline)` for `old_inline_changes`, draw ghost glyphs, advance paint-y. Ghost rows are inert (not in the hit-test index — replaces `block_input=true`).
7. Interleave ghost heights into `line_tops`/scroll/hit-test math so clicks and scroll-to-cursor stay aligned near hunks.

Definition of Done: an edited file against HEAD shows added-line and inline-word backgrounds under the glyphs, and deleted ghost lines above their position; scrolling and clicking near hunks stays aligned; ghost layouts are cached.

Top risks: ghost rows break the buffer-line↔y bijection that gpui's list handled for free — the interleave math is the top defect surface. Draw order: faint 0.05 bg before glyphs, 0.25 inline before glyphs (after glyphs washes out text); define diff-vs-selection overlap order. Empty-line ghosts still need a full-width bg from line metrics.

---

### Phase 6 — Chrome & overlays (status bar, title bar, CSD, autocomplete + hover popover)
Draws from: `chrome-overlays`.

Tasks:
1. Keep `build_context_display()` + its tests verbatim. `StatusBarInfo`/`FileInfo` gpui Globals → plain shell fields recomputed each frame, fed by `visible_line_range()`.
2. Shared helpers: `draw_panel(scene, rect, radius, bg, border, shadow)` = `draw_blurred_rounded_rect` (shadow brush is concrete `peniko::Color`, not a `BrushRef`) + `fill(RoundedRect)` + `stroke`. `layout_row`/`draw_layout` reuse the Phase 1 text path (note `align` in this stack takes `(Alignment, AlignmentOptions)`, **no** max_advance arg).
3. `StatusBar::draw` / `TitleBar::draw` (filename + dirty `*` + ellipsis via `Layout::width()`, three `kurbo::Circle` traffic lights, hover lighten).
4. CSD `ChromeShell`: `WindowAttributes::with_decorations(false)`, blurred rounded shadow + border, `INSET`; port pure `resize_edge()` → `ResizeDirection`; `Window::set_cursor(CursorIcon::*)` per edge on every `CursorMoved` (reset on leave); `drag_window()` / `drag_resize_window()` — both return `Result`, **fall back to server-side decorations** if unsupported under Wayland. wgpu surface `alpha_mode: PreMultiplied` for transparent corners. `actions!` → shell match: `set_minimized`/`set_maximized` toggle/`event_loop.exit()`.
5. Overlays: keep pure `popup_x` clamp + `space_below`/`space_above` flip; `draw_panel` + `push_clip_layer` rows + selection-bg `Rect` + Parley text + `pop_layer`. Issue/PR titles reuse a standalone `render_styled_line(scene, layout, offset)` with a colored `#123` prefix run. Anchor from the cursor/hovered-ref rects **returned** by the editor paint (Phase 4).
6. **HitRegion registry** rebuilt per frame, routed top-down (overlays > buttons > CSD resize band > editor) with a `consumed` flag — replaces gpui `stop_propagation()`.

Definition of Done: status bar, title bar with working traffic lights/drag/resize, autocomplete dropdown, and GitHub hover popover all render and route pointer/scroll correctly above the editor; CSD works on Asahi/Wayland or cleanly falls back to SSD.

Top risks: event-routing order (clicks falling through or stealing editor drags) — the top-down `consumed` flag is the mitigation. CSD under Wayland SSD — detect `Result` failure and fall back. sRGB-vs-linear color conversion shifting all UI colors. Resize cursor sticking if not reset on band-leave.

---

### Phase 7 — gpui fully removed & `cargo publish --dry-run` passes
Draws from: `logic-integration-teardown`.

Tasks:
1. Delete `gpui` (`Cargo.toml:34`), `gpui_platform` (`:39`), `async-compat` (`:29`), and the entire `[patch.crates-io]` block (`:75`). Delete `.cargo/config.toml` `[env] RUST_FONTCONFIG_DLOPEN` (superseded by the `fontique` `fontconfig-dlopen` feature; keep the wild/clang linker config).
2. `cargo update`; verify `cargo metadata --format-version 1` shows **no** `source = git+...`.
3. CI: drop `libasound2-dev`/`libwayland-dev`/`libxcb1-dev`/`libxkbcommon-x11-dev`/`libfontconfig1-dev` and `CARGO_NET_GIT_FETCH_WITH_CLI`/profile-trim env; keep `libxkbcommon-dev`; add `mesa-vulkan-drivers` + `libvulkan-dev`; add a `cargo publish --dry-run --locked` step.
4. Rewrite README (drop the GPUI-component/`cx.new(...)` embeddability section → plain `Editor::new(&str)` + winit/wgpu/Vello runtime; drop `Rems` from the config example; update the Linux note to `fontconfig-dlopen`, no `fontconfig-devel`).
5. Publishability gate: `cargo publish --dry-run --locked` + `cargo package`; fix stray path/git deps, removed-gpui doctests, and the `writd` bin; bump version (semver major if embeddability was public API).

Definition of Done: `rg gpui src/` is empty; `cargo metadata` has zero git sources; `cargo publish --dry-run --locked` passes; CI is green on the trimmed package set.

Top risks: `fontique` major-version drift breaking feature unification (two `fontique` majors = cargo error). `dlopen` silently losing font enumeration in AppImage/bundle builds (fine on Asahi/Fedora). `publish --dry-run` failing on the `writd` bin or doctests referencing removed gpui APIs — treat README/doctest cleanup as a publish blocker, not cosmetic.

## Critical path & effort

Dependency chain: **Phase 0 (shell)** → **Phase 1 (text engine + one line)** → **Phase 2 (headless Editor)** → **Phase 3 (doc + scroll)** → **Phase 4 (input/IME)** → then **Phase 5 (diff)** and **Phase 6 (chrome)** can run in parallel (both depend on 4's returned-geometry + segment map, not on each other) → **Phase 7 (teardown/publish)**.

Rough effort by spec: text-core **XL** (the long pole), viewport-scroll **L**, chrome-overlays **L**, inline-git-diff **M**, logic-integration-teardown **M**, linebreak-fidelity **S** (folded into Phase 1 as a single override call). The genuine long poles are **Phase 1+4 (text-core, XL)** — per-line Parley layout, the display↔buffer segment map, cursor/selection/hit-test, IME — and secondarily **Phase 3 (bespoke viewport virtualization)**, since writ has *no* scroller today and inherits none from Parley/Vello. Everything downstream (diff, chrome) is comparatively mechanical once the segment map and paint-returns-geometry contracts are solid. Budget the segment map and viewport height model as the two areas most likely to overrun.

## Top risks & mitigations (cross-cutting)

- **Vello on Asahi (Vulkan via Mesa):** GPU-path instability could block all rendering. Mitigation: bring up `vello_cpu`/`vello_hybrid` fallback in Phase 0 alongside the wgpu path and keep it building every phase; prefer explicit Vulkan backend selection; validate present-mode/alpha-mode early (also needed for CSD transparency).
- **Bespoke viewport virtualization:** gpui's `list`/`ListState` did height, virtualization, and scroll-to-reveal for free; all of it is now hand-rolled prefix-sum math with no lazy measurement. Mitigation: prefix-sum `line_tops` with n+1 length and heavy unit tests (top/bottom/short-doc/ghost-interleave); estimate-then-refine heights with `scroll_y` reconciliation for large docs.
- **IME:** never exercised under gpui's abstraction; Wayland/X11 preedit on Asahi is fiddly. Mitigation: dedicate Phase 4 tasks to `set_ime_allowed`/`set_ime_cursor_area` + preedit rendering; test both Wayland and X11 sessions explicitly.
- **0.x API churn / clone-vs-crates.io drift:** the verification clone diverges from published parley 0.11 / vello 0.9 (`quantize` arg, `draw_glyphs(&FontData|&Font)`, `parley_core` split for `CHROMIUM_LINE_BREAK_OVERRIDE`). Mitigation: the Phase 0 reconciliation spike pins exact published versions and adapts every cited call site up front; if published 0.11 lacks the break-override export, escalate a version bump before committing to the Chromium-wrap goal.
- **`http.rs` HttpClient re-point:** it currently implements gpui's `HttpClient` trait and bridges tokio→smol via `async_compat`; naively dropping `.compat()` compiles but panics with no reactor. Mitigation: install the tokio runtime + rustls ring provider in Phase 0, re-point `http.rs` to an inherent `get_bytes` and strip `.compat()` from both `http.rs` and `github.rs` only in Phase 2 (after the runtime exists), keeping the ordering load-bearing.
- **Segment-map correctness (implicit cross-cut):** Parley lays out display text, all editing/diff math speaks buffer bytes; a single off-by-one poisons cursor, selection, click, and diff highlights simultaneously. Mitigation: make the display↔buffer segment map a first-class tested type in Phase 3 with round-trip tests (incl. multibyte/emoji and collapsed URLs) before Phase 4/5 build on it.

## Start here (day one)

1. **API reconciliation spike: DONE** (2026-07-01, `~/Documents/writ-spike`) — see the resolved-facts block under Strategy. All four unknowns answered on the published stack and the GPU path proven on Asahi/Vulkan. Next actions start at step 2.
2. **Blank-window shell:** implement the winit `ApplicationHandler` + wgpu surface + Vello `Renderer` that clears the window to a color (Phase 0 task 2), with the tokio runtime + rustls ring provider installed. Add the parley/vello/wgpu/winit/fontique deps additively (leave gpui in for now).
3. **First real glyphs:** in that shell, stand up `TextEngine`, port `editor/theme.rs` colors to `peniko::Color`, and render one hard-coded syntax-highlighted markdown line with `set_line_break_override(Some(CHROMIUM_LINE_BREAK_OVERRIDE))` + `break_all_lines(Some(width))` — proving the Parley→Vello glyph path and Chromium wrapping end-to-end before touching `Editor`.

---

# Migration plan addendum: hardened shell / cursor+hit-test / input+IME specs

This addendum folds three published-API-verified subsystem specs into the existing writ migration plan. All signatures below were opened and confirmed against staged published sources (winit 0.30.13, vello 0.9.0 incl. `vello::util`, parley 0.11.0). They slot in as follows:

- **Phase 0 (app-shell):** the outer event loop, wgpu/Vello GPU context, present path, resize/DPI, redraw policy, CSD decision.
- **Phase 4 (cursor/selection/hit-test + IME/input):** per-line Parley layout store, `Cluster`/`Cursor`/`Selection` geometry, the retained display↔buffer segment map, and the winit `window_event` dispatcher with IME preedit + clipboard.

Cross-cutting invariants that bind the two phases:
- **Single wgpu version.** Depend on `vello::wgpu` (re-exported wgpu 29.0.3, `vello lib.rs:143` / `Cargo.toml:106`). No independent `wgpu` in the tree or surface/device handles won't typecheck.
- **`&FontData`, not `&Font`.** `Scene::draw_glyphs(&mut self, font: &FontData)` (`scene.rs:455`). The vello_editor example targets vello 0.8 and uses `&Font`; do not copy its glyph call.
- **No `render_to_surface` in Vello 0.9.** Present = render to intermediate STORAGE texture, then `TextureBlitter::copy` to the swapchain, then `present()`.

---

## App-shell (Phase 0): winit 0.30 + wgpu 29 + Vello 0.9

writ has no explicit shell today — gpui owns loop/window/surface/renderer. The migration builds all of it from nothing and (optionally) reimplements CSD.

### Verified target API (published signatures)

winit 0.30.13:
- `trait ApplicationHandler<T: 'static = ()>` (`application.rs:8`; note the `T: 'static` bound). Required: `resumed(&mut self, &ActiveEventLoop)` (`:83`), `window_event(&mut self, &ActiveEventLoop, WindowId, WindowEvent)` (`:93`). Provided: `new_events(_, StartCause)` (`:15`), `about_to_wait(&ActiveEventLoop)` (`:121`), `suspended(&ActiveEventLoop)` (`:186`), `user_event(_, T)` (`:88`), `exiting` (`:194`).
- `EventLoop::with_user_event() -> EventLoopBuilder<T>` (`event_loop.rs:215`); `EventLoopBuilder::build(&mut self) -> Result<EventLoop<T>, EventLoopError>` (`:115`); `EventLoop::run_app<A: ApplicationHandler<T>>(self, &mut A) -> Result<(), EventLoopError>` (`:264`); `create_proxy(&self) -> EventLoopProxy<T>` (`:270`); `EventLoopProxy::send_event(&self, T) -> Result<(), EventLoopClosed<T>>` (`:567`).
- `ActiveEventLoop::create_window(&self, WindowAttributes) -> Result<Window, OsError>` (`:378`); `set_control_flow(&self, ControlFlow)` (`:452`); `exit(&self)` (`:464`).
- `Window`: `id() -> WindowId` (`window.rs:495`), `scale_factor() -> f64` (`:563`), `request_redraw(&self)` (`:597`), `pre_present_notify(&self)` (`:636`), `inner_size() -> PhysicalSize<u32>` (`:758`), `drag_resize_window(&self, ResizeDirection) -> Result<(), ExternalError>` (`:1537`).
- `WindowEvent` (`event.rs:152`): `Resized(PhysicalSize<u32>)` (`:161`), `CloseRequested` (`:171`), `ScaleFactorChanged { scale_factor: f64, inner_size_writer: InnerSizeWriter }` (`:379`), `RedrawRequested` (`:437`).

Vello 0.9.0:
- `Renderer::new(device: &Device, options: RendererOptions) -> Result<Self>` (`lib.rs:432`).
- `Renderer::render_to_texture(&mut self, &Device, &Queue, &Scene, &TextureView, &RenderParams) -> Result<()>` (`lib.rs:474`). **No `render_to_surface`.** `render_to_texture_async` is `#[deprecated]` (`:642`).
- `RendererOptions { use_cpu: bool, antialiasing_support: AaSupport, num_init_threads: Option<NonZeroUsize>, pipeline_cache: Option<wgpu::PipelineCache> }` (`:373`); `Default` at `:408` (`use_cpu:false`, `AaSupport::all()`, `NonZeroUsize::new(1)`, `None`).
- `RenderParams { base_color: peniko::Color, width: u32, height: u32, antialiasing_method: AaConfig }` (`:357`); `AaConfig { Area, Msaa8, Msaa16 }` (`:175`); `AaSupport { area, msaa8, msaa16 }` + `AaSupport::all()` (`:203/216`).
- `Scene::{ new() (:54), reset(&mut self) (:59), fill (:316), push_layer/pop_layer (:105/251), draw_glyphs(&mut self, &FontData) -> DrawGlyphs (:455) }`.

Vello 0.9 `util` (the wgpu glue writ must lean on):
- `RenderContext { pub instance: Instance, pub devices: Vec<DeviceHandle> }` (`util.rs:16`); `RenderContext::new()` (`:32`) reads `Backends::from_env().unwrap_or_default()` (`:33`); `InstanceDescriptor` built at `:37-48` (fields `display: None`, `backends`, `flags`, `memory_budget_thresholds`, `backend_options`).
- `DeviceHandle { adapter: Adapter, pub device: Device, pub queue: Queue }` (`:21`); `adapter(&self) -> &Adapter` (`:216`).
- `async create_surface<'w>(&mut self, impl Into<SurfaceTarget<'w>>, u32, u32, wgpu::PresentMode) -> Result<RenderSurface<'w>>` (`:52`).
- `resize_surface(&self, &mut RenderSurface, u32, u32)` (`:118`) — no internal assert, but panics indirectly via wgpu validation in `create_targets`/`configure_surface` when a dimension is 0; caller MUST guard.
- `async device(&mut self, Option<&Surface>) -> Option<usize>` (`:144`, async).
- `RenderSurface<'s> { pub surface, pub config: SurfaceConfiguration, pub dev_id: usize, pub format: TextureFormat, pub target_texture: Texture, pub target_view: TextureView, pub blitter: TextureBlitter }` (`:222`; `format` at `:226`).
- Surface config (`:83-98`): format = `find(Rgba8Unorm | Bgra8Unorm)` else `UnsupportedSurfaceFormat`; `usage: RENDER_ATTACHMENT`; `alpha_mode: CompositeAlphaMode::Auto`; caller `present_mode`; `desired_maximum_frame_latency: 2`; `view_formats: vec![]`.
- Intermediate target (`:195-212`): `usage: STORAGE_BINDING | TEXTURE_BINDING`, `format: Rgba8Unorm`, sample_count 1.
- `block_on_wgpu(device, fut)` (`:256`).

Present path (example `main.rs:305-368`): `render_to_texture(&surface.target_view, params)` → `surface.get_current_texture()` → encoder + `surface.blitter.copy(device, &mut encoder, &surface.target_view, &frame_view)` → `queue.submit([encoder.finish()])` → `pre_present_notify()` → `surface_texture.present()` → `device.poll(wgpu::PollType::Poll)`.

### Ordered task list

1. **Cargo.** Drop `gpui`, `gpui_platform`, the `[patch.crates-io] gpui` and zed git deps. Add `winit = "0.30"`, `vello = "0.9"`, `pollster = "0.4"`. Use `vello::wgpu` everywhere; add no separate `wgpu`.
2. **App state struct** (model on example `main.rs:88-113`): `context: RenderContext`, `renderers: Vec<Option<Renderer>>`, `state: RenderState<'s>` (`Active { surface: Box<RenderSurface>, window: Arc<Window> }` | `Suspended(Option<Arc<Window>>)`), `scene: Scene`, plus writ document model, theme, dirty flag. Preserve the Active-fields drop order (surface before window).
3. **Backend preference (Asahi/Mesa → Vulkan).** Set `WGPU_BACKEND=vulkan` (optionally `WGPU_POWER_PREF`) in-process **before** `RenderContext::new()`; or construct the `RenderContext` fields yourself (they're `pub`) with `Instance::new(InstanceDescriptor{ backends: Backends::VULKAN, ..Default::default() })` mirroring `util.rs:37-48`. Prefer the env approach for the one-shot.
4. **`impl ApplicationHandler`:**
   - `resumed`: create window if `Suspended`; `block_on(create_surface(window.clone(), w, h, present_mode))`; `present_mode = Mailbox if supported else AutoVsync`; `renderers.resize_with(devices.len(), || None)`; `renderers[dev_id].get_or_insert_with(|| Renderer::new(&device, RendererOptions{ use_cpu: cpu_fallback, antialiasing_support: AaSupport::all(), num_init_threads: NonZeroUsize::new(1), pipeline_cache: None }))`; transition to `Active`; `set_control_flow(Wait)`.
   - `window_event`: guard `state.window.id() == window_id`, then:
     - `CloseRequested` → `event_loop.exit()`.
     - `Resized(size)` → **only if `width>0 && height>0`** `context.resize_surface(&mut surface, w, h)`; update editor width; `request_redraw()`.
     - `ScaleFactorChanged { scale_factor, .. }` → store scale for parley layout (do NOT hardcode 1.0); the follow-up `Resized` reconfigures the surface.
     - `RedrawRequested` → if dirty `scene.reset()` + rebuild; `render_to_texture(&device, &queue, &scene, &surface.target_view, &RenderParams{ base_color: theme_bg, width, height, antialiasing_method: AaConfig::Area })`; blit; submit; `pre_present_notify()`; `present()`; `device.poll(PollType::Poll)`.
     - Input events → forward to editor subsystem (Phase 4); set dirty + `request_redraw()`.
   - `suspended`: stash `Arc<Window>` into `Suspended`, drop the surface.
   - `about_to_wait`: leave default unless switching to animation-driven redraw. Never render from here.
5. **Redraw policy:** default `ControlFlow::Wait` (matches writ's reactive model). If cursor blink is added, use `new_events` + `WaitUntil(next_blink)`, never `Poll`.
6. **Surface-format awareness:** read `surface.format`; the blitter is already built for it. Set `RenderParams.base_color` (linear) to the theme background.
7. **CPU fallback:** thread a `cpu_fallback: bool` (CLI/env) into the single `RendererOptions.use_cpu`. On adapter failure / known-bad Vulkan driver, recreate the `Renderer` with `use_cpu: true` (same Scene/pipeline). `vello_cpu`/`vello_hybrid` are out of scope.
8. **CSD decision (blocks visual parity):** ship SSD first (`.with_decorations(true)`, delete `src/window.rs`); port `WindowShadow`/`resize_edge` to Vello-drawn chrome + `window.drag_resize_window` as a later pass.
9. **Delete/replace:** `src/window.rs` and the gpui parts of `src/main.rs`; keep `Config`, GitHub, file-watch, demo wiring re-hosted on the new loop; route file-watch/demo/GitHub async via `EventLoopProxy<UserEvent>` → `user_event`.

### Residual open questions

1. **CPU-fallback scope:** does the mandate mean runtime `use_cpu: true` (one-shot achievable) or integrating the separate `vello_cpu`/`vello_hybrid` sparse-strips crates (larger port, non-`Scene` API)? Recommend the former.
2. **CSD parity:** is exact rounded-corner + 10px-shadow parity required for the one-shot, or is SSD acceptable initially?
3. **Vulkan compute on M2/Asahi:** confirm the Honeykrisp/venus adapter exposes compute (Vello requires compute shaders, `lib.rs:19`); if unstable, `use_cpu` is the (slow) safety net.
4. **Zero-dimension resizes** on Wayland/minimize (`Resized(0,0)`) — guarded above; confirm no other path calls `resize_surface` unguarded.
5. **Async-on-sync-loop:** `create_surface`/`device` are async; `resumed` is sync. `block_on_wgpu` deadlocks if the future awaits non-GPU work — fine here, but keep the futures GPU-only.

---

## Cursor / selection / hit-test (Phase 4): Parley 0.11 geometry over a retained segment map

writ keeps cursor/selection as **buffer byte offsets** with **marker-atomic** motion, and mediates buffer↔display through a per-line segment map (`src/line.rs`). The design keeps all of that and delegates only *geometry* and *point hit-testing* to Parley, via **one `parley::Layout` per buffer line**.

### Verified target API (published signatures)

Layout construction — `context.rs`:
- `LayoutContext::ranged_builder(&mut self, fcx: &mut FontContext, text: &str, scale: f32, quantize: bool) -> RangedBuilder<'_, B>` (`:87`; 4th param `quantize: bool` — `true` snaps to pixels). `tree_builder` shares the trailing `quantize: bool` (`:160`).

Hit-test / cluster geometry — `layout/cluster.rs`:
- `Cluster::from_point(&Layout<B>, x: f32, y: f32) -> Option<(Cluster<'_,B>, ClusterSide)>` (`:68`; nearest). `from_point_exact` (`:60`; None outside a glyph). `from_byte_index(&Layout<B>, byte_index: usize) -> Option<Cluster<'_,B>>` (`:36`).
- `enum ClusterSide { Left, Right }` (`:27`). `text_range(&self) -> Range<usize>` (`:147`). `visual_offset(&self) -> Option<f32>` (`:407`). `advance(&self) -> f32` (`:158`). `is_rtl/is_word_boundary/is_line_break(->Option<BreakReason>)/is_hard_line_break/is_start_of_line/is_end_of_line` (`:163/178/238/192/225/230`).
- `enum Affinity { Downstream = 0, Upstream = 1 }` (`:454`; re-exported via `parley::*`).

Logical/visual cursor — `editing/cursor.rs`:
- `Cursor { index: usize, affinity: Affinity }` (`:16`). `from_byte_index(layout, index, affinity) -> Self` (`:23`). `from_point(layout, x, y) -> Self` (`:44`; folds side+RTL+hard-break affinity, out-of-bounds falls back to `layout.data.text_len`). `index(&self) -> usize` (`:97`). `affinity(&self) -> Affinity` (`:105`). `geometry(&self, layout, width: f32) -> BoundingBox` (`:277`). `next_visual/previous_visual` (`:154/119`). `refresh(&self, layout) -> Self` (`:112`).

Selection — `editing/selection.rs`:
- `Selection { anchor: Cursor, focus: Cursor, anchor_base, h_pos }` (`:16`). `new(anchor, focus)` (`:33`); `From<Cursor>` collapsed (`:639`). `from_byte_index(layout, index, affinity)` (`:44`); `from_point(layout, x, y)` (`:49`); `word_from_point` (`:54`); `line_from_point` (`:75`); `hard_line_from_point` (`:90`). `is_collapsed()` (`:117`); `anchor()/focus()` (`:125/132`); `text_range(&self) -> Range<usize>` (`:168`). Motion: `next_visual/previous_visual(layout, extend)` (`:180/201`); `move_lines(layout, delta: isize, extend)` (`:266`); `line_start/line_end(layout, extend)` (`:323/376`). Extension: `extend(focus)` (`:488`); `extend_to_point(layout, x, y)` (`:436`); `shift_click_extension(layout, x, y)` (`:464`). Geometry: `geometry(layout) -> Vec<(BoundingBox, usize)>` (`:497`); `geometry_with(layout, f: impl FnMut(BoundingBox, usize))` (`:509`; allocation-free, RTL/wrap/trailing-newline-aware, `usize` = line index).

Support types:
- `BoundingBox { x0, y0, x1, y1: f64 }` (`util.rs:16`); `new` at `:30`; `width/height/union` at `:38/46/54` (re-exported `parley::BoundingBox`, `lib.rs:133`).
- `Line::metrics(&self) -> &LineMetrics` (`layout/line.rs:24`); `Line::text_range` (`:33`); `Line::break_reason` (`:28`). `LineMetrics { ascent, descent, offset, advance, inline_min_coord, block_min_coord, block_max_coord, .. }` (`:109-143`).
- `Layout::len() -> usize` (`layout.rs:92`); `Layout::get(index) -> Option<Line>` (`:105`). **`line_for_offset` (`:207`) and `line_for_byte_index` (`:184`) are `pub(crate)` — not callable.**
- `Scene::fill(style: Fill, transform: Affine, brush: impl Into<BrushRef>, brush_transform: Option<Affine>, shape: &impl Shape)` (`scene.rs:316`); `Scene::draw_glyphs(&mut self, &FontData)` (`:455`).

**Coordinate convention (critical):** Parley `from_point`/`geometry` are layout-local (line origin, y ∈ `[block_min_coord, block_max_coord]`). Per line: `x_local = mouse.x - line_left`, `y_local = mouse.y - line_top`; when drawing, translate each `BoundingBox` by `Affine::translate((line_left, line_top))`.

**Byte-unit consistency:** writ's visual counters, `display_text`, and Parley `text_range`/`Cursor.index` are all UTF-8 byte offsets into the display string, so `cluster.text_range().start` feeds `visual_to_buffer_pos` directly — no char/byte conversion at the Parley boundary.

### Ordered task list

1. **Per-line layout store.** Replace the gpui `Line` element with a struct holding: buffer `LineMarkers`, built `display_text`, `content_range`, `heading_marker_len`, `hidden_regions: Vec<HiddenRegion>`, `collapsed_regions: Vec<CollapsedDisplayText>`, a cached `parley::Layout<Brush>`, and stacked `top_y/left_x/height`. Reuse `build_styled_content` (`line.rs:1307`) verbatim for `display_text`/runs; feed into `ranged_builder(fcx, display_text, scale, quantize=true)`, push styles, `build` + `break_all_lines`.
2. **Keep the segment map.** Retain `buffer_to_visual_pos`/`visual_to_buffer_pos` (`line.rs:86-153`) and `Line::buffer_to_visual_pos` special cases (`:994-1023`) unchanged — Parley never sees hidden markers.
3. **Hit-test entry point.** `fn hit_test(&self, x, y) -> usize`: find the line whose `[top_y, top_y+height)` contains `y`; compute `x_local`/`y_local`; if inside a `CollapsedDisplayText.visual_range`, use the re-shape path (task 6); else `let db = parley::Cursor::from_point(&layout, x_local, y_local).index()` (folds side/RTL/affinity, preferred over hand-rolling from `Cluster::from_point`); then `visual_to_buffer_pos(db.saturating_sub(prefix_len), &content_range, heading_marker_len, &hidden_regions, line_range.end)`.
4. **Wire events.** In the winit path, translate `MouseInput`/`CursorMoved` (physical→logical, minus scroll) into `hit_test`, then feed the existing `EditorState::handle_click(offset, shift, click_count)` (`editor/mod.rs:1296`) / drag `extend_to`. Keep `click_count` word/line selection buffer-side.
5. **Keep buffer-side motion.** Leave `Cursor::move_left/right/up/down` and `Selection::extend_to` as-is. Do **not** route arrows through Parley `next_visual` (breaks atomic markers/collapsed URLs). Optionally add a soft-wrap mode later via `Selection::move_lines`.
6. **Collapsed-region proportional hit-test.** Reimplement `CollapsedDisplayText::map_x_to_buffer_offset` (`line.rs:59-80`) against Parley: build a scratch `Layout` from `buffer_text`, `Cluster::from_point(&scratch, x_offset, mid_y)` → `buffer_range.start + cluster.text_range().start`. Cache the scratch layout keyed by `buffer_text`.
7. **Caret rendering.** Per line with cursor: `db = Line::buffer_to_visual_pos(cursor_offset, display_text)` (guard `cursor_in_marker_area`, `line.rs:988`); `rect = parley::Cursor::from_byte_index(&layout, db + prefix_len, Affinity::Downstream).geometry(&layout, CARET_W)`; `scene.fill(Fill::NonZero, Affine::translate((left_x, top_y)), caret_brush, None, &Rect::new(rect.x0, rect.y0, rect.x1, rect.y1))`.
8. **Selection rendering.** Per overlapping line: map `sel_start`/`sel_end` clamped to line range through `buffer_to_visual_pos` (as `compute_visual_selection_range`, `line.rs:1025-1039`), build `Selection::new(Cursor::from_byte_index(vs+prefix), Cursor::from_byte_index(ve+prefix))`, then `sel.geometry_with(&layout, |b, _| scene.fill(Fill::NonZero, Affine::translate((left_x, top_y)), sel_brush, None, &Rect::new(b.x0, b.y0, b.x1, b.y1)))`. Paint selection **before** glyphs.
9. **Relayout invalidation.** `display_text` expands/collapses markers with cursor presence (`line.rs:764-810`); rebuild the affected line's `Layout` and `refresh(layout)` any cached Parley `Cursor`/`Selection` (`cursor.rs:112`, `selection.rs:146`) whenever the cursor enters/leaves that line or a collapsed region.
10. **Tests.** Port roundtrip tests (`line.rs:1901-2024`); add: `from_point` at line-edges matches the old gpui offset; caret x from `Cursor::geometry` equals `Cluster::visual_offset`; selection rects cover expected clusters.

### Residual open questions

1. **No public `line_for_offset`/`line_for_byte_index`** (`pub(crate)`) — this is the decisive argument for one-layout-per-line; writ owns the line→top_y map and localizes coordinates before calling public `Cluster`/`Cursor` APIs. If a single document layout is ever needed, spans must be reconstructed manually from `Layout::len()`/`Layout::get(i).metrics()` + `Line::text_range`.
2. **Click-past-line-end** must yield `display_text.len()` (Parley fallback = `text_len`) and map through `visual_to_buffer_pos` clamped to `line_end`, preserving writ's old gpui `Err(idx)` fallthrough (`line.rs:1663-1665`).
3. **Cross-line selection newline band.** `geometry_with` adds trailing whitespace (`NEWLINE_WHITESPACE_WIDTH_RATIO = 0.25 × (ascent+descent)`, `selection.rs:514/542-546`) per layout; with per-line layouts a hard-newline-spanning selection may not get that band automatically — decide whether to synthesize it.
4. **Affinity at soft-wrap.** writ's `Cursor` has no affinity field; nearly irrelevant in the non-wrapping model except at the trailing newline, but enabling soft-wrap later requires storing affinity alongside the buffer offset (a model change to scope).
5. **Prefix bookkeeping.** Keep `+prefix_len`/`saturating_sub(prefix_len)` around `Cluster` byte indices if the prefix is baked into the same `display_text`; recompute from the prefix run's byte length if it becomes a separate inline box.
6. **Collapsed re-shape cost** is heavier than gpui `shape_line`; cache per collapsed region.

---

## Input / IME / clipboard (Phase 4): winit `window_event` dispatcher → `EditorAction`

Replaces gpui's listeners with a winit `window_event` dispatcher translating `WindowEvent` into writ's edit methods / `EditorAction`, adds IME preedit (gpui had none), and swaps in a third-party clipboard crate.

### Verified target API (published signatures)

winit 0.30.13 events:
- `WindowEvent` (`event.rs:152`): `KeyboardInput { device_id, event: KeyEvent, is_synthetic: bool }` (`:205`), `ModifiersChanged(Modifiers)` (`:222`), `Ime(Ime)` (`:231`), `CursorMoved { device_id, position: PhysicalPosition<f64> }` (`:242`), `MouseWheel { device_id, delta: MouseScrollDelta, phase: TouchPhase }` (`:275`), `MouseInput { device_id, state: ElementState, button: MouseButton }` (`:278`).
- `KeyEvent` (`event.rs:523`): `physical_key: PhysicalKey` (`:549`), `logical_key: Key` (`:573`), `text: Option<SmolStr>` (`:594`; layout/dead-key resolved), `location: KeyLocation` (`:609`), `state: ElementState` (`:614`), `repeat: bool` (`:647`).
- `Key<Str = SmolStr> { Named(NamedKey), Character(Str) }` (`keyboard.rs:1472`); `Key::as_ref(&self) -> Key<&str>` (`:1555`); `Key::to_text(&self) -> Option<&str>` (`:1600`; also `Key<Str: AsRef<str>>::to_text` at `:1576`). `PhysicalKey { Code(KeyCode), .. }` (`:225`).
- `NamedKey` (`:755`): `Backspace:419 Enter:430 Space:440 Tab:442 Delete:469 End:471 Home:475 ArrowDown:483 ArrowLeft:485 ArrowRight:487 ArrowUp:489 Escape:571`.
- `Modifiers` (`event.rs:660`): `state(&self) -> ModifiersState` (`:671`). `ModifiersState` (`keyboard.rs:1693`): `shift_key/control_key/alt_key/super_key(&self) -> bool` (`:1707/1712/1717/1722`).
- `ElementState { Pressed, Released }` (`event.rs:921`); `is_pressed(self) -> bool` (`:928`). `MouseButton { Left, Right, Middle, Back, Forward, Other(u16) }` (`:941`; `Middle:944`). `MouseScrollDelta { LineDelta(f32, f32), PixelDelta(PhysicalPosition<f64>) }` (`:953/959/974`).
- `Ime { Enabled, Preedit(String, Option<(usize, usize)>), Commit(String), Disabled }` (`event.rs:774`; `Enabled:780 Preedit:789 Commit:794 Disabled:802`; the `(begin,end)` is byte-indexed within the preedit, `None` hides the cursor, empty string clears).
- Window IME control (`window.rs`): `set_ime_allowed(&self, bool)` (`:1283`; required to receive any `Ime` event), `set_ime_cursor_area<P: Into<Position>, S: Into<Size>>(&self, P, S)` (`:1248`), `set_ime_purpose(&self, ImePurpose)` (`:1294`, optional).

Preedit rendering / click mapping:
- `Scene::draw_glyphs(&mut self, &FontData) -> DrawGlyphs<'_>` (`vello scene.rs:455`).
- `Cluster::from_point(&Layout<B>, f32, f32) -> Option<(Cluster, ClusterSide)>` (`cluster.rs:68`); `Cluster::from_byte_index(&Layout<B>, usize) -> Option<Cluster>` (`:36`); `ClusterSide` (`:27`). `Cluster<'a, B: Brush>` over `&Layout<B>`.
- `LayoutContext::ranged_builder(&mut self, fcx, text, scale, quantize: bool)` (`context.rs:87`).

Clipboard (pattern only — example's own crate, not a published Parley/Vello/winit source): `clipboard-rs = "0.3.3"` (`vello_editor/Cargo.toml:29`). `use clipboard_rs::{Clipboard, ClipboardContext};` — `set_text`/`get_text` are **`Clipboard` trait methods**, so import the trait. `ClipboardContext::new() -> Result<Self>`; `set_text(&self, String) -> Result<()>` (owned); `get_text(&self) -> Result<String>`. `arboard = "3"` is an equivalent-shape alternative.

Existing writ targets (unchanged): `EditorState::handle_click(offset, shift, click_count)` (`editor/mod.rs:1296`), `handle_drag(offset)` (`:1314`); `Editor::insert_text/enter/shift_enter/shift_alt_enter/tab/shift_tab/delete_backward/delete_forward`; `PasteContext::from_buffer(&Buffer, usize)` + `transform_paste(&str, &PasteContext) -> String` (`paste.rs`). `EditorAction` (`action.rs:11`): `Type(char), Enter, ShiftEnter, ShiftAltEnter, Tab, ShiftTab, Backspace, Move(Direction), Click{offset,shift,click_count}, Drag{offset}, ToggleCheckbox{line_number}, UpdateHover{..}, OpenLink{url}`; `Direction = Left|Right|Up|Down` (`:53`).

### Ordered task list

1. **Add clipboard dep** `clipboard-rs = "0.3.3"` (target-gated windows/macos/linux) or `arboard = "3"`; wrap in a `writ::clipboard` façade returning `Option<String>`/`Result`, importing the `Clipboard` trait alongside `ClipboardContext`. Remove all `gpui::ClipboardItem` usage.
2. **Input state on the app/editor struct:** `modifiers: ModifiersState`, `cursor_pos: PhysicalPosition<f64>`, `mouse_primary_down: bool`, click-count tracker `(Instant, PhysicalPosition, usize)`, `composing: bool`, `preedit: Option<(String, Option<(usize,usize)>)>`, `last_sent_ime_cursor_area`.
3. **`window.set_ime_allowed(true)`** after window creation.
4. **Write the `window_event` dispatcher** with arms for `ModifiersChanged`, `KeyboardInput`, `Ime`, `MouseInput`, `CursorMoved`, `MouseWheel`, plus `RedrawRequested`/`Resized`/`CloseRequested`/`Focused`.
5. **Keyboard:** gate on `event.state == Pressed`, `!is_synthetic`, and `!self.composing`. Match `event.logical_key` (`NamedKey` arms **before** any `text` fallback, so Enter=`"\r"`/Tab=`"\t"` don't double-fire). Compute `action_mod = if macos { super_key } else { control_key }` and `shift` from cached `ModifiersState`. Preserve `try_insert_space`, `maybe_complete_blockquote_marker` (`">"`), `maybe_complete_code_fence` (`` "`" ``/`"~"`), and `scroll_to_cursor_pending`.
6. **Text insertion:** prefer `event.text` (SmolStr) over a hand-built char; iterate graphemes if multi-char; keep the special-character post-processing.
7. **Extend `EditorAction`** OR keep keys as direct method calls (recommended for least churn). If unifying, add `DeleteForward, MoveLineStart{extend}, MoveLineEnd{extend}, MoveDocStart{extend}, MoveDocEnd{extend}, SelectAll, Copy, Cut, Paste, Undo, Redo, Save`, and give `Move` an `extend: bool` (currently hardcoded `false` at `mod.rs:3377`).
8. **Mouse:** manual click-count tracking (~500ms + few-px radius); resolve `cursor_pos → buffer offset` via `Cluster::from_point` (subtract editor inset/scroll first); emit `Click`/`Drag`; keep autoscroll edge-zone logic from `on_drag_move` (`mod.rs:3877`) driven from `CursorMoved` while primary is down; reset `is_selecting` on `MouseInput{Released, Left}`.
9. **Scroll:** `MouseWheel` → `LineDelta(_, y) => y * line_height`; `PixelDelta(p) => p.y`; feed the scroll state.
10. **IME:** `Enabled` → `composing = true`; `Preedit(s, cur)` → empty clears else store `(s, cur)`; `Commit(s)` → clear preedit + `insert_text(&s)`; `Disabled` → clear preedit + `composing = false`. Suppress key handling while composing.
11. **IME cursor area:** after each edit, compute the caret box via `Cluster::from_byte_index` and call `set_ime_cursor_area(PhysicalPosition, PhysicalSize)` only when changed (dedupe), adding the editor inset.
12. **Preedit rendering:** draw the preedit string as underlined glyphs at the caret via `Scene::draw_glyphs(&FontData)`; never commit to the buffer.
13. **Clipboard paste** keeps `PasteContext::from_buffer` + `transform_paste` unchanged; only the text source changes.
14. **Delete gpui listeners** (`on_key_down`/`on_modifiers_changed`/`ctrl_held`) and keep the underlying `EditorState`/`Editor` edit methods intact.

Concrete key→behavior mapping: `Backspace`→`delete_backward`; `Delete`→`delete_forward`; `Arrow*`(+shift)→`move_in_direction(dir, extend)`; `Home`/`End`(+action_mod=doc)→`move_to_line_start`/`move_to_start`, `move_to_line_end`/`move_to_end`; `Enter`(+shift/+shift+alt)→`enter`/`shift_enter`/`shift_alt_enter`; `Tab`(+shift)→`tab`/`shift_tab` (code block → 4 spaces); `Space`→`try_insert_space` gate; `Ctrl/Cmd`+`a`→writ select-all (Parley analogue is `PlainEditor::select_all` at `editing/editor.rs:562`, **not** a `Selection` method); `+c/x/v`→copy/cut/paste; `+z`(+shift redo)/`+y`→undo/redo; `+s`→save; `+r`→refresh GitHub refs; other `Character(s)`/`text`→`insert_text` + completion helpers.

### Residual open questions

1. **`EditorAction::Move` lacks `extend`** and keyboard never went through `EditorAction`. Recommend keeping the winit handler calling `Editor` methods directly for the initial port (least churn), reserving `EditorAction` for programmatic/demo use.
2. **`text` vs `key_char` ordering:** winit `KeyEvent.text` is present for Enter/Tab too — `NamedKey` arms must precede the `text` fallback.
3. **IME gating / composing:** writ has no composing concept; add the flag + preedit overlay or IME keystrokes double-insert.
4. **Preedit-not-in-buffer:** the Line/marker/tree-sitter pipeline must render preedit as a transient Vello overlay, not buffer text, or markdown parsing thrashes on partial composition.
5. **`set_ime_cursor_area` on X11** may obscure the exclusion area (winit PR 3966); Wayland/fcitx5/ibus (Asahi) is the primary path — verify.
6. **Click→offset resolution** now lives in writ: account for editor inset, per-line layout, scroll, wrapped lines; map global y → line → local layout. Biggest new piece; overlaps the cursor/hit-test subsystem above.
7. **`MouseWheel` scaling** is platform-dependent (Wayland `PixelDelta`, X11 `LineDelta`); handle both, `line_height` from `EditorConfig`.
8. **Clipboard crate choice:** `clipboard-rs` constructs a fresh context per op and `.unwrap()`s; prefer a cached context/façade; evaluate `arboard` (Wayland primary-selection). Confirm Wayland clipboard under the shipped winit backend.
9. **Synthetic key events** (`is_synthetic: true`, X11 focus) must be filtered from text.

Relevant files: `/home/wilfred/Documents/writ/src/editor/mod.rs`, `/home/wilfred/Documents/writ/src/editor/action.rs`, `/home/wilfred/Documents/writ/src/paste.rs`, `/home/wilfred/Documents/writ/src/line.rs`, `/home/wilfred/Documents/writ/src/cursor.rs`, `/home/wilfred/Documents/writ/src/window.rs`, `/home/wilfred/Documents/writ/src/main.rs`.

---

## Published API reconciliation table

Confirmed signatures across all three subsystems — the day-one API spike is largely pre-done from this table.

### winit 0.30.13

| Item | Signature | Source |
|---|---|---|
| `ApplicationHandler` | `trait ApplicationHandler<T: 'static = ()>` | `application.rs:8` |
| `resumed` | `fn resumed(&mut self, &ActiveEventLoop)` | `application.rs:83` |
| `window_event` | `fn window_event(&mut self, &ActiveEventLoop, WindowId, WindowEvent)` | `application.rs:93` |
| `new_events` / `about_to_wait` / `suspended` / `user_event` | provided methods | `application.rs:15/121/186/88` |
| `EventLoop::with_user_event` | `-> EventLoopBuilder<T>` | `event_loop.rs:215` |
| `EventLoopBuilder::build` | `(&mut self) -> Result<EventLoop<T>, EventLoopError>` | `event_loop.rs:115` |
| `EventLoop::run_app` | `<A: ApplicationHandler<T>>(self, &mut A) -> Result<(), EventLoopError>` | `event_loop.rs:264` |
| `EventLoop::create_proxy` | `(&self) -> EventLoopProxy<T>` | `event_loop.rs:270` |
| `EventLoopProxy::send_event` | `(&self, T) -> Result<(), EventLoopClosed<T>>` | `event_loop.rs:567` |
| `ActiveEventLoop::create_window` | `(&self, WindowAttributes) -> Result<Window, OsError>` | `event_loop.rs:378` |
| `ActiveEventLoop::set_control_flow` / `exit` | `(&self, ControlFlow)` / `(&self)` | `event_loop.rs:452/464` |
| `WindowEvent` variants | `Resized(PhysicalSize<u32>)`, `CloseRequested`, `ScaleFactorChanged{scale_factor:f64, inner_size_writer:InnerSizeWriter}`, `RedrawRequested`, `KeyboardInput{device_id,event:KeyEvent,is_synthetic:bool}`, `ModifiersChanged(Modifiers)`, `Ime(Ime)`, `CursorMoved{device_id,position:PhysicalPosition<f64>}`, `MouseWheel{device_id,delta:MouseScrollDelta,phase:TouchPhase}`, `MouseInput{device_id,state:ElementState,button:MouseButton}` | `event.rs:161/171/379/437/205/222/231/242/275/278` |
| `KeyEvent` | `{ physical_key:PhysicalKey, logical_key:Key, text:Option<SmolStr>, location:KeyLocation, state:ElementState, repeat:bool }` | `event.rs:523` (`549/573/594/609/614/647`) |
| `Key` | `enum Key<Str=SmolStr> { Named(NamedKey), Character(Str) }`; `as_ref(&self)->Key<&str>`; `to_text(&self)->Option<&str>` | `keyboard.rs:1472/1555/1600` |
| `NamedKey` | `Backspace/Enter/Space/Tab/Delete/End/Home/ArrowDown/ArrowLeft/ArrowRight/ArrowUp/Escape` | `keyboard.rs:419/430/440/442/469/471/475/483/485/487/489/571` |
| `Modifiers` / `ModifiersState` | `state(&self)->ModifiersState`; `shift_key/control_key/alt_key/super_key(&self)->bool` | `event.rs:671`, `keyboard.rs:1707/1712/1717/1722` |
| `ElementState` | `{ Pressed, Released }`; `is_pressed(self)->bool` | `event.rs:921/928` |
| `MouseButton` | `{ Left, Right, Middle, Back, Forward, Other(u16) }` | `event.rs:941` |
| `MouseScrollDelta` | `{ LineDelta(f32,f32), PixelDelta(PhysicalPosition<f64>) }` | `event.rs:953/959/974` |
| `Ime` | `{ Enabled, Preedit(String, Option<(usize,usize)>), Commit(String), Disabled }` | `event.rs:774/780/789/794/802` |
| `Window::request_redraw` / `pre_present_notify` / `inner_size` / `scale_factor` / `id` | `(&self)` / `(&self)` / `->PhysicalSize<u32>` / `->f64` / `->WindowId` | `window.rs:597/636/758/563/495` |
| `Window::drag_resize_window` | `(&self, ResizeDirection) -> Result<(), ExternalError>` | `window.rs:1537` |
| `Window::set_ime_allowed` | `(&self, allowed: bool)` | `window.rs:1283` |
| `Window::set_ime_cursor_area` | `<P: Into<Position>, S: Into<Size>>(&self, P, S)` | `window.rs:1248` |
| `Window::set_ime_purpose` | `(&self, ImePurpose)` | `window.rs:1294` |

### wgpu surface config (via `vello::wgpu` 29.0.3 / `vello::util`)

| Item | Value / Signature | Source |
|---|---|---|
| wgpu re-export | `pub use wgpu;` (vello 0.9 pins 29.0.3) | `vello lib.rs:143`, `Cargo.toml:106` |
| `RenderContext` | `{ pub instance: Instance, pub devices: Vec<DeviceHandle> }`; `new() -> Self` | `util.rs:16/32` |
| `InstanceDescriptor` fields | `display, backends, flags, memory_budget_thresholds, backend_options` | `util.rs:37-48` |
| `DeviceHandle` | `{ adapter: Adapter, pub device: Device, pub queue: Queue }`; `adapter(&self)->&Adapter` | `util.rs:21/216` |
| `create_surface` | `async <'w>(&mut self, impl Into<SurfaceTarget<'w>>, u32, u32, wgpu::PresentMode) -> Result<RenderSurface<'w>>` | `util.rs:52` |
| `resize_surface` | `(&self, &mut RenderSurface, u32, u32)` — panics via wgpu on 0-dim | `util.rs:118` |
| `device` | `async (&mut self, Option<&Surface>) -> Option<usize>` | `util.rs:144` |
| `RenderSurface` | `{ pub surface, pub config: SurfaceConfiguration, pub dev_id: usize, pub format: TextureFormat, pub target_texture: Texture, pub target_view: TextureView, pub blitter: TextureBlitter }` | `util.rs:222` |
| Surface config | `format = Rgba8Unorm\|Bgra8Unorm`, `usage: RENDER_ATTACHMENT`, `alpha_mode: CompositeAlphaMode::Auto`, `desired_maximum_frame_latency: 2`, `view_formats: vec![]` | `util.rs:83-98` |
| Intermediate target | `usage: STORAGE_BINDING\|TEXTURE_BINDING`, `format: Rgba8Unorm`, sample_count 1 | `util.rs:195-212` |
| `block_on_wgpu` | `(device, fut)` helper | `util.rs:256` |

### Vello 0.9.0 — Scene, Renderer, glyphs

| Item | Signature | Source |
|---|---|---|
| `Renderer::new` | `(device: &Device, options: RendererOptions) -> Result<Self>` | `lib.rs:432` |
| `Renderer::render_to_texture` | `(&mut self, &Device, &Queue, &Scene, &TextureView, &RenderParams) -> Result<()>` — **no `render_to_surface`** | `lib.rs:474` |
| `RendererOptions` | `{ use_cpu: bool, antialiasing_support: AaSupport, num_init_threads: Option<NonZeroUsize>, pipeline_cache: Option<wgpu::PipelineCache> }` | `lib.rs:373` |
| `RenderParams` | `{ base_color: peniko::Color, width: u32, height: u32, antialiasing_method: AaConfig }` | `lib.rs:357` |
| `AaConfig` / `AaSupport` | `{ Area, Msaa8, Msaa16 }` / `{ area, msaa8, msaa16 }` + `all()` | `lib.rs:175/203/216` |
| `Scene::new` / `reset` / `fill` | `()` / `(&mut self)` / `(style: Fill, transform: Affine, brush: impl Into<BrushRef>, brush_transform: Option<Affine>, shape: &impl Shape)` | `scene.rs:54/59/316` |
| `Scene::push_layer` / `pop_layer` | layer ops | `scene.rs:105/251` |
| `Scene::draw_glyphs` | `(&mut self, font: &FontData) -> DrawGlyphs<'_>` — **`&FontData`, not `&Font`** | `scene.rs:455` |

### Parley 0.11.0 — layout, cluster, cursor, selection geometry

| Item | Signature | Source |
|---|---|---|
| `LayoutContext::ranged_builder` | `(&mut self, fcx: &mut FontContext, text: &str, scale: f32, quantize: bool) -> RangedBuilder<'_, B>` | `context.rs:87` |
| `LayoutContext::tree_builder` | `(&mut self, fcx, text, scale, quantize: bool) -> TreeBuilder` | `context.rs:160` |
| `Cluster::from_point` | `(&Layout<B>, x: f32, y: f32) -> Option<(Cluster<'_,B>, ClusterSide)>` | `cluster.rs:68` |
| `Cluster::from_point_exact` | `(&Layout<B>, f32, f32) -> Option<(Cluster, ClusterSide)>` | `cluster.rs:60` |
| `Cluster::from_byte_index` | `(&Layout<B>, byte_index: usize) -> Option<Cluster<'_,B>>` | `cluster.rs:36` |
| `ClusterSide` | `enum { Left, Right }` | `cluster.rs:27` |
| `Cluster::text_range` / `visual_offset` / `advance` | `-> Range<usize>` / `-> Option<f32>` / `-> f32` | `cluster.rs:147/407/158` |
| `Affinity` | `enum { Downstream = 0, Upstream = 1 }` (re-exported `parley::*`) | `cluster.rs:454` |
| `Cursor::from_byte_index` | `(layout, index: usize, affinity: Affinity) -> Cursor` | `cursor.rs:23` |
| `Cursor::from_point` | `(layout, x: f32, y: f32) -> Cursor` (folds side/RTL/affinity; OOB → `text_len`) | `cursor.rs:44` |
| `Cursor::index` / `affinity` / `refresh` | `-> usize` / `-> Affinity` / `(&self, layout) -> Cursor` | `cursor.rs:97/105/112` |
| `Cursor::geometry` | `(&self, layout, width: f32) -> BoundingBox` | `cursor.rs:277` |
| `Cursor::next_visual` / `previous_visual` | `(layout) -> Self` | `cursor.rs:154/119` |
| `Selection::new` | `(anchor: Cursor, focus: Cursor) -> Selection` | `selection.rs:33` |
| `Selection::from_byte_index` / `from_point` | `(layout, index, affinity)` / `(layout, x, y)` | `selection.rs:44/49` |
| `Selection::word_from_point` / `line_from_point` / `hard_line_from_point` | `(layout, x, y)` | `selection.rs:54/75/90` |
| `Selection::is_collapsed` / `anchor` / `focus` / `text_range` | `-> bool` / `-> Cursor` / `-> Cursor` / `-> Range<usize>` | `selection.rs:117/125/132/168` |
| `Selection::next_visual` / `previous_visual` | `(layout, extend: bool) -> Selection` | `selection.rs:180/201` |
| `Selection::move_lines` | `(layout, delta: isize, extend: bool) -> Selection` | `selection.rs:266` |
| `Selection::line_start` / `line_end` | `(layout, extend: bool)` | `selection.rs:323/376` |
| `Selection::extend` / `extend_to_point` / `shift_click_extension` | `(focus: Cursor)` / `(layout, x, y)` / `(layout, x, y)` | `selection.rs:488/436/464` |
| `Selection::geometry` | `(layout) -> Vec<(BoundingBox, usize)>` | `selection.rs:497` |
| `Selection::geometry_with` | `(layout, f: impl FnMut(BoundingBox, usize))` (RTL/wrap/newline-aware; `usize`=line idx) | `selection.rs:509` |
| `BoundingBox` | `{ x0, y0, x1, y1: f64 }`; `new(...)` | `util.rs:16/30` |
| `Layout::len` / `get` | `-> usize` / `(index) -> Option<Line>` | `layout.rs:92/105` |
| `line_for_offset` / `line_for_byte_index` | **`pub(crate)` — not callable** | `layout.rs:207/184` |
| `Line::metrics` / `text_range` / `break_reason` | `-> &LineMetrics` / `-> Range<usize>` / `-> BreakReason` | `layout/line.rs:24/33/28` |
| `PlainEditor::select_all` (select-all analogue; not on `Selection`) | — | `editing/editor.rs:562` |

Clipboard (pattern-only, example crate): `clipboard-rs = "0.3.3"`; `use clipboard_rs::{Clipboard, ClipboardContext}`; `ClipboardContext::new() -> Result<Self>`, `set_text(&self, String) -> Result<()>`, `get_text(&self) -> Result<String>` (`Clipboard` trait methods — import the trait). `arboard = "3"` is an equivalent alternative.