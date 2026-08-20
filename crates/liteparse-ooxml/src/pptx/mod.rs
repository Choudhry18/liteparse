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

pub mod cascade;
pub mod geometry;
pub mod media;
pub mod package;
pub mod shapes;
pub mod text;
pub mod textcascade;

pub use cascade::{
    BackgroundSource, CascadeStats, MatchRule, PlaceholderGeometry, apply_inherited_geometry,
    apply_slide, background_fill, resolve_background,
};
pub use geometry::{GeometryStats, SlideRect, apply_slide_geometry};
pub use media::MediaCache;
pub use package::{EMU_PER_POINT, Part, PresentationInfo, PresentationPackage, SlideParts, walk};
pub use shapes::{
    AutoShape, Background, Connector, GraphicFrame, GraphicFramePayload, Group, Placeholder,
    PlaceholderKind, Shape, ShapeKind, ShapeStyle, SlidePart, Table, TableCell,
    TableCellProperties, TableRow, parse_shape_tree, parse_slide_part, shows_inherited_shapes,
    visit_all,
};
pub use text::{
    AutoNumberScheme, Bullet, ListStyle, Spacing, TextBody, TextParagraph, TextParagraphProperties,
    TextStyles, parse_default_text_style, parse_diagram_text, parse_text_body, parse_text_styles,
};
pub use textcascade::{
    DeckTextDefaults, PlaceholderTextStyles, ResolvedTextStyle, SizeSource, TextCascade,
    TextStyleClass,
};
