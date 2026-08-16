//! Native PresentationML (`.pptx`) reader.
//!
//! Shares the OOXML packaging, relationships, DrawingML schema, theme,
//! measurement and line-breaking layers with [`crate::docx`] — see
//! `ATTRIBUTION.md` for what is vendored and what is ours.
//!
//! Unlike DOCX, PPTX **carries its own geometry**: every shape declares
//! `<a:off>`/`<a:ext>` in EMU against a fixed `<p:sldSz>`. So the hard part
//! here is not layout, it is the placeholder cascade — a shape that omits
//! `<a:xfrm>` inherits position and size from its layout, which inherits
//! from its master, and the match rule differs at each rung.

pub mod package;
pub mod shapes;
pub mod text;

pub use package::{EMU_PER_POINT, Part, PresentationInfo, PresentationPackage, SlideParts, walk};
pub use shapes::{
    AutoShape, Connector, GraphicFrame, GraphicFramePayload, Group, Placeholder, PlaceholderKind,
    Shape, ShapeKind, Table, TableCell, TableRow, parse_shape_tree, visit_all,
};
pub use text::{
    AutoNumberScheme, Bullet, ListStyle, Spacing, TextBody, TextParagraph, TextParagraphProperties,
    parse_text_body,
};
