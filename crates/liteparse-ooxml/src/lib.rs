//! Native OOXML structure reader.
//!
//! Vendored, Skia-free subset of [dxpdf](https://github.com/nerdy-pro/dxpdf)
//! 0.4.0 (MIT — see `LICENSE` and `ATTRIBUTION.md`). Upstream is a DOCX→PDF
//! engine; this crate keeps only `parse → resolve`, which is where the
//! document's *structure* lives, and drops the Skia-bound layout/paint half.
//!
//! Named `-ooxml` rather than `-docx` because much of the vendored code is
//! format-neutral OOXML — ZIP packaging, relationships, DrawingML, theme,
//! line breaking, measurement — and is shared with the PPTX reader landing
//! alongside it. `docx` is the DOCX entry point.
//!
//! The whole point is that a DOCX already states, explicitly, everything the
//! LibreOffice→PDF→projection path has to reverse-engineer from coordinates:
//! heading levels (`outlineLvl`), list numbering (including `startOverride`),
//! merged cells (`gridSpan`/`vMerge`), emphasis, footnotes, reading order.
//!
//! ```no_run
//! let bytes = std::fs::read("report.docx")?;
//! let doc = liteparse_ooxml::docx::parse(&bytes)?;
//! let resolved = liteparse_ooxml::render::resolve::resolve(doc);
//! # Ok::<(), liteparse_ooxml::Error>(())
//! ```

pub mod docx;
pub mod error;
pub mod field;
pub mod model;
pub mod pptx;
pub mod render;
pub mod xlsx;

pub use error::Error;
