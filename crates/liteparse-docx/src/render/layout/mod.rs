//! Only the Skia-free part of dxpdf's layout stage.
//!
//! `draw_command` is vendored because `resolve::shape_visuals` builds shape
//! draw commands during resolve. The measurement/pagination modules
//! (`build`, `line`, `measurer`, `page`, `paragraph`, `section`, `table`, …)
//! are not: they are what stage 2 would port onto harfrust + skrifa.

pub mod draw_command;
