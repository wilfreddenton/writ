//! Appearance / frame constants, in one place so the shell (which renders) and the
//! layout tests (which reproduce the frame) share a single source of truth rather than
//! each hardcoding the same numbers. All values are logical px unless noted.

/// Padding around the document body. The bottom uses `2 × PADDING` so the last line
/// clears the status bar.
pub const PADDING: f32 = 24.0;
/// Maximum content (body) width for readability. On windows wider than this plus the
/// side padding, the body is capped at this width and centered instead of sprawling
/// edge-to-edge; narrower windows just use `PADDING` on each side.
pub const MAX_CONTENT_WIDTH: f32 = 880.0;
/// Body font size.
pub const FONT_SIZE: f32 = 18.0;
/// Line height as a multiple of the font size (CSS-unitless semantics).
pub const LINE_HEIGHT: f32 = 1.5;
/// Height of the bottom status bar.
pub const STATUS_BAR_H: f32 = 24.0;
/// Text caret width.
pub const CARET_WIDTH: f32 = 2.0;
/// Scroll distance per wheel line-delta.
pub const WHEEL_LINE_STEP: f32 = 48.0;
