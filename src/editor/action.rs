//! Cursor movement direction, used by the editor's movement methods.

/// Cursor movement direction.
#[derive(Clone, Debug, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}
