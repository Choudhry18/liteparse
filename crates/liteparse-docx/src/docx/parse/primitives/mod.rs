//! Reusable schema atoms shared across OOXML serde parsers.
//!
//! Each submodule owns one category of primitive and its `Deserialize` impl.
//! All types here are pure schema-layer concerns — they may wrap model types
//! (e.g., `Dimension<U>`) but never leak serde into the model layer.

pub mod colors;
pub(crate) mod integer_measure;
pub mod lenient;
pub mod st_enums;
pub mod toggles;
pub mod units;

pub use colors::{HexColor, RgbHexU32};
pub(crate) use toggles::last_toggle;
pub use toggles::{AttrBool, OnOff};
