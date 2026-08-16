//! Public, serializable view of the markdown classifier's block decomposition.
//!
//! The classifier's internal [`Block`](crate::markdown_layout::Block) is a Rust
//! enum with per-variant payloads. That shape can't cross the napi or PyO3
//! boundary — neither supports enums carrying data — so the public type is a
//! single flat struct discriminated by `kind`, with the variant-specific fields
//! as `Option`s. This mirrors how the crate already exposes
//! `StructureAttributeValue` to the bindings, and keeps one block shape across
//! Rust, JSON, Node, Python, and WASM instead of three divergent ones.
//!
//! Every field that doesn't apply to a block's `kind` is `None` and is skipped
//! during serialization, so a heading serializes as `{kind, text, level, bbox}`
//! rather than a wall of nulls.

use serde::Serialize;

use crate::markdown_layout::{Block, Cell, PositionedBlock};
use crate::types::Rect;

/// One table cell: its rendered text and the region it occupied.
///
/// `bbox` is `None` for cells with no ink behind them — padding inserted to
/// square off a ragged grid, or halves of a merged run split at an estimated
/// position rather than an observed boundary.
#[derive(Debug, Clone, Serialize)]
pub struct LayoutCell {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
}

impl From<&Cell> for LayoutCell {
    fn from(c: &Cell) -> Self {
        LayoutCell {
            text: c.text.clone(),
            bbox: c.bbox.clone(),
        }
    }
}

/// A classified block plus where it sits on the page.
///
/// `kind` discriminates the block; see each field for which kinds populate it.
/// Blocks appear in reading order, matching the order the markdown renderer
/// emits them, so the Nth block here is the Nth block of that page's markdown.
#[derive(Debug, Clone, Serialize)]
pub struct LayoutBlock {
    /// One of `heading`, `paragraph`, `list_item`, `code`, `table`,
    /// `grid_fallback`, `rule`, `figure`.
    pub kind: &'static str,
    /// Rendered text for the text-bearing kinds (`heading`, `paragraph`,
    /// `list_item`). Table text lives in `header`/`rows`; code and grid text in
    /// `lines`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Heading level (1–6), or list nesting depth for `list_item`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// Whether the block's text is uniformly bold / italic. `paragraph` and
    /// `list_item` only; omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    /// `list_item`: whether the list is ordered, and the original marker as it
    /// appeared on the page (`138.`, `iii)`, `•`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// Verbatim source lines for `code` and `grid_fallback`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    /// Best-effort language hint for `code`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// `table`: the header row, when one was detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Vec<LayoutCell>>,
    /// `table`: the body rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<LayoutCell>>>,
    /// `figure`: the image's page-scoped id and encoded format, matching the
    /// `img_{id}.{format}` target the markdown renderer emits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Region of the page this block occupies, in the same top-left, 72-DPI
    /// viewport space as `text_items`. The union of every source line that fed
    /// the block, so a wrapped heading or multi-line paragraph reports its full
    /// band. `None` when the block has no page geometry behind it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
}

impl LayoutBlock {
    /// Base value with every variant-specific field cleared.
    fn of(kind: &'static str, bbox: Option<Rect>) -> Self {
        LayoutBlock {
            kind,
            text: None,
            level: None,
            bold: false,
            italic: false,
            ordered: None,
            marker: None,
            lines: None,
            lang: None,
            header: None,
            rows: None,
            id: None,
            format: None,
            bbox,
        }
    }
}

fn cells(row: &[Cell]) -> Vec<LayoutCell> {
    row.iter().map(LayoutCell::from).collect()
}

impl From<&PositionedBlock> for LayoutBlock {
    fn from(pb: &PositionedBlock) -> Self {
        let bbox = pb.bbox.clone();
        match &pb.block {
            Block::Heading { level, text } => LayoutBlock {
                text: Some(text.clone()),
                level: Some(*level),
                ..LayoutBlock::of("heading", bbox)
            },
            Block::Paragraph { text, bold, italic } => LayoutBlock {
                text: Some(text.clone()),
                bold: *bold,
                italic: *italic,
                ..LayoutBlock::of("paragraph", bbox)
            },
            Block::ListItem {
                ordered,
                marker,
                level,
                text,
                bold,
                italic,
            } => LayoutBlock {
                text: Some(text.clone()),
                level: Some(*level),
                ordered: Some(*ordered),
                marker: Some(marker.clone()),
                bold: *bold,
                italic: *italic,
                ..LayoutBlock::of("list_item", bbox)
            },
            Block::CodeBlock { lines, lang } => LayoutBlock {
                lines: Some(lines.clone()),
                lang: lang.clone(),
                ..LayoutBlock::of("code", bbox)
            },
            Block::Table { header, rows } => LayoutBlock {
                header: header.as_ref().map(|h| cells(h)),
                rows: Some(rows.iter().map(|r| cells(r)).collect()),
                ..LayoutBlock::of("table", bbox)
            },
            Block::GridFallback { lines } => LayoutBlock {
                lines: Some(lines.clone()),
                ..LayoutBlock::of("grid_fallback", bbox)
            },
            Block::HorizontalRule => LayoutBlock::of("rule", bbox),
            Block::Figure { id, format } => LayoutBlock {
                id: Some(id.clone()),
                format: Some(format.clone()),
                ..LayoutBlock::of("figure", bbox)
            },
        }
    }
}
