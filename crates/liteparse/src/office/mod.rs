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
// Gated on `office-native`, not `docx-native`: `layout_to_pages` and its
// helpers are format-neutral over `&[LayoutedPage]` — only `page_column_counts`
// reads the DOCX model — and the PPTX geometry pass converts through them so
// the two formats cannot disagree on how a baseline becomes a box.
#[cfg(feature = "office-native")]
pub mod docx_layout;
#[cfg(feature = "office-native")]
pub(crate) mod inline;
#[cfg(feature = "pptx-native")]
pub mod pptx;
#[cfg(feature = "pptx-native")]
pub mod pptx_layout;
#[cfg(feature = "xlsx-native")]
pub mod xlsx;
#[cfg(feature = "xlsx-native")]
pub mod xlsx_layout;

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

/// The input's bytes when it is an `.xlsx` the native path should try, `None`
/// otherwise. Mirrors [`docx_bytes`]: extension-matched for paths (`.xls`,
/// `.xlsm`, `.xlsb`, `.csv` stay on the conversion path), container-sniffed
/// for bytes.
#[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
pub(crate) fn xlsx_bytes(input: &crate::types::PdfInput) -> Option<std::borrow::Cow<'_, [u8]>> {
    use crate::types::PdfInput;
    match input {
        PdfInput::Path(p) => {
            let ext = std::path::Path::new(p).extension()?;
            if !ext.eq_ignore_ascii_case("xlsx") {
                return None;
            }
            std::fs::read(p).ok().map(std::borrow::Cow::Owned)
        }
        PdfInput::Bytes(b) => (crate::conversion::guess_extension_from_data(b).as_deref()
            == Some("xlsx"))
        .then_some(std::borrow::Cow::Borrowed(b.as_slice())),
    }
}

/// The input's bytes when it is a `.pptx` the native path should try, `None`
/// otherwise. Mirrors [`docx_bytes`]: extension-matched for paths (`.ppt`,
/// `.pptm`, `.potx` stay on the conversion path), container-sniffed for bytes.
#[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
pub(crate) fn pptx_bytes(input: &crate::types::PdfInput) -> Option<std::borrow::Cow<'_, [u8]>> {
    use crate::types::PdfInput;
    match input {
        PdfInput::Path(p) => {
            let ext = std::path::Path::new(p).extension()?;
            if !ext.eq_ignore_ascii_case("pptx") {
                return None;
            }
            std::fs::read(p).ok().map(std::borrow::Cow::Owned)
        }
        PdfInput::Bytes(b) => (crate::conversion::guess_extension_from_data(b).as_deref()
            == Some("pptx"))
        .then_some(std::borrow::Cow::Borrowed(b.as_slice())),
    }
}
