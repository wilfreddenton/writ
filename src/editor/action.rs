//! Cursor movement direction, used by the editor's movement methods.

/// Cursor movement direction.
#[derive(Clone, Debug, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
    /// Start / end of the current line (Home / End).
    LineStart,
    LineEnd,
    /// Start / end of the document (Ctrl+Home / Ctrl+End).
    DocStart,
    DocEnd,
}
