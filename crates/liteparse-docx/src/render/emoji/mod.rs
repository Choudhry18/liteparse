//! Only `cluster`, the Skia-free half of dxpdf's emoji handling.
//!
//! `raster`, `resolve` and `shape` drive Skia colour-font (COLR/CBDT) painting
//! and are omitted. `cluster` is pure Unicode segmentation and is referenced by
//! `layout::draw_command`.

pub mod cluster;
