//! Native office-format readers.
//!
//! These bypass the LibreOffice→PDF→projection pipeline entirely. Office
//! formats carry explicit structure (heading levels, list numbering, merged
//! table cells, notes) that rendering to PDF destroys and projection then has
//! to reverse-engineer from coordinates. Reading it directly is both faster and
//! strictly more faithful.
//!
//! Each reader emits [`markdown_layout::Block`](crate::markdown_layout::Block),
//! the same model the PDF path produces, so `render_blocks` and every
//! text-level heuristic downstream are shared between the two pipelines.

#[cfg(feature = "docx-native")]
pub mod docx;
#[cfg(feature = "docx-native")]
pub mod docx_layout;
#[cfg(feature = "office-native")]
pub(crate) mod inline;
#[cfg(feature = "pptx-native")]
pub mod pptx;

/// The input's bytes when it is a `.docx` the native path should try, `None`
/// otherwise. Path inputs are matched on extension only (`.doc`/`.docm`/
/// `.dotx` stay on the conversion path); byte inputs by container sniffing.
/// A path that fails to read returns `None` so the conversion path surfaces
/// its usual error for missing/unreadable files.
#[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
pub(crate) fn docx_bytes(input: &crate::types::PdfInput) -> Option<std::borrow::Cow<'_, [u8]>> {
    use crate::types::PdfInput;
    match input {
        PdfInput::Path(p) => {
            let ext = std::path::Path::new(p).extension()?;
            if !ext.eq_ignore_ascii_case("docx") {
                return None;
            }
            std::fs::read(p).ok().map(std::borrow::Cow::Owned)
        }
        PdfInput::Bytes(b) => (crate::conversion::guess_extension_from_data(b).as_deref()
            == Some("docx"))
        .then_some(std::borrow::Cow::Borrowed(b.as_slice())),
    }
}
