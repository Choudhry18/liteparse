//! Font-handle placeholder for the Skia-free subset.
//!
//! Upstream this module is a 1300-line `FontRegistry` built on `skia_safe`.
//! The structure path (`docx::parse` → `render::resolve`) never resolves a
//! typeface; the only reference is `draw_command::DrawCommand::Text`, which
//! names `TypefaceEntry` in one field.
//!
//! This is the seam where a real font handle lands if/when the layout stage is
//! ported (harfrust + skrifa, per stage 2 of the native-office plan). Until
//! then it is deliberately opaque and uninhabitable-by-accident: nothing in the
//! structure path constructs one.
#[derive(Clone, Debug)]
pub struct TypefaceEntry {
    _private: (),
}
