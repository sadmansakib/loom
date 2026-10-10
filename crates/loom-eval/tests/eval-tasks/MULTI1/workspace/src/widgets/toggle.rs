//! The toggle widget.

/// Schema generation this widget's storage layout speaks.
pub const SCHEMA_VERSION: u32 = 2;

/// Quads emitted when drawing: the frame, one per child, plus
/// the widget's decoration layer.
pub fn quads_drawn(children: u32) -> u32 {
    children + 1 + 4
}
