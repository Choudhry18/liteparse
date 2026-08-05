//! Skia-free half of dxpdf's render pipeline.
//!
//! Upstream this module is `resolve → layout → subset → paint`. Only `resolve`
//! is vendored: it flattens style inheritance (`basedOn` chains, docDefaults,
//! theme font refs, table conditional formatting) and numbering (abstract +
//! instance + `lvlOverride`/`startOverride`), which is everything the structure
//! path needs. `layout`, `subset` and `painter` are Skia-bound and omitted.
//!
//! `dimension`, `geometry`, `error`, `layout::draw_command` and
//! `emoji::cluster` come along because `resolve` refers to them.

pub mod dimension;
pub mod emoji;
pub mod error;
pub mod fonts;
pub mod geometry;
pub mod layout;
pub mod resolve;
