//! Serde schema types for OOXML property elements.
//!
//! Schema types carry the `Xml` suffix and are `pub(crate)` — never exported
//! from `docx::`. They mirror OOXML grammar; `From<_Xml> for ModelType`
//! conversions live alongside each schema.

pub mod border;
pub mod cnf_style;
pub mod fonts;
pub mod insets;
pub mod lang;
pub mod measure;
pub mod paragraph;
pub mod run;
pub mod section;
pub mod shading;
pub mod table;
pub mod tabs;
