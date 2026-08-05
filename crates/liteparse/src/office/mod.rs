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
