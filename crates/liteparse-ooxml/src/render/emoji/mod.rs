//! The Skia-free half of dxpdf's emoji handling.
//!
//! `cluster` is pure Unicode segmentation (UAX #29 / UTS #51). `resolve` picks
//! a color emoji typeface through the `FontRegistry` — typeface *identity*
//! only, no rasterization. `raster` and `shape` drive Skia colour-font
//! painting upstream and are omitted; cluster advances fall back to the
//! cmap-only path in `layout::measurer`.

pub mod cluster;
pub mod resolve;
