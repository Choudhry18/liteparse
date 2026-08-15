//! Private helper functions for section layout.

use super::super::draw_command::{DrawCommand, LayoutedPage};
use super::super::fragment::Fragment;
use super::super::page::PageConfig;
use super::super::paragraph::{ParagraphStyle, layout_paragraph};
use super::{FOOTNOTE_SEPARATOR_GAP, FOOTNOTE_SEPARATOR_LINE_WIDTH, FOOTNOTE_SEPARATOR_RATIO};
use crate::render::dimension::Pt;

/// Split a fragment slice at break markers of a given kind.
/// Returns a vec of slices; the break fragments themselves are excluded.
fn split_at_breaks(fragments: &[Fragment], is_break: fn(&Fragment) -> bool) -> Vec<&[Fragment]> {
    let has_break = fragments.iter().any(is_break);
    if !has_break {
        return vec![fragments];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    for (i, frag) in fragments.iter().enumerate() {
        if is_break(frag) {
            chunks.push(&fragments[start..i]);
            start = i + 1;
        }
    }
    chunks.push(&fragments[start..]);
    chunks
}

/// Split a fragment slice at `Fragment::ColumnBreak` markers.
/// Returns a vec of slices; the column break fragments themselves are excluded.
pub(super) fn split_at_column_breaks(fragments: &[Fragment]) -> Vec<&[Fragment]> {
    split_at_breaks(fragments, |f| matches!(f, Fragment::ColumnBreak))
}

/// §17.3.3.1: split a fragment slice at `Fragment::PageBreak` markers.
/// Returns a vec of slices; the page break fragments themselves are excluded.
pub(super) fn split_at_page_breaks(fragments: &[Fragment]) -> Vec<&[Fragment]> {
    split_at_breaks(fragments, Fragment::is_page_break)
}

pub(super) fn render_page_footnotes(
    page: &mut LayoutedPage,
    config: &PageConfig,
    footnotes: &[(&[Fragment], &ParagraphStyle)],
    default_line_height: Pt,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
    separator_indent: Pt,
    page_bottom: Pt,
) {
    let content_width = config.content_width();
    let constraints = super::super::BoxConstraints::tight_width(content_width, Pt::INFINITY);
    // Layout all footnotes to compute total height.
    let mut footnote_layouts = Vec::new();
    let mut total_height = FOOTNOTE_SEPARATOR_GAP; // separator line + gap above first note
    for (frags, style) in footnotes {
        let para = layout_paragraph(
            frags,
            &constraints,
            style,
            default_line_height,
            measure_text,
        );
        total_height += para.size.height;
        footnote_layouts.push(para);
    }

    let footnote_top = page_bottom - total_height;

    // §17.11.23: separator line positioned per default paragraph indent.
    let sep_x = config.margins.left + separator_indent;
    let sep_width = content_width * FOOTNOTE_SEPARATOR_RATIO;
    page.commands.push(DrawCommand::Line {
        line: crate::render::geometry::PtLineSegment::new(
            crate::render::geometry::PtOffset::new(sep_x, footnote_top),
            crate::render::geometry::PtOffset::new(sep_x + sep_width, footnote_top),
        ),
        color: crate::render::resolve::color::RgbColor::BLACK,
        width: FOOTNOTE_SEPARATOR_LINE_WIDTH,
    });

    // Render footnote paragraphs.
    let mut cursor_y = footnote_top + FOOTNOTE_SEPARATOR_GAP;
    for para in footnote_layouts {
        for mut cmd in para.commands {
            cmd.shift_y(cursor_y);
            cmd.shift_x(config.margins.left);
            page.commands.push(cmd);
        }
        cursor_y += para.size.height;
    }
}

/// §17.4.28: compute the table's x offset based on alignment and indent.
pub(super) fn table_x_offset(
    alignment: Option<crate::model::Alignment>,
    indent: Pt,
    table_width: Pt,
    content_width: Pt,
    margin_left: Pt,
) -> Pt {
    use crate::model::Alignment;
    match alignment {
        Some(Alignment::Center) => margin_left + (content_width - table_width) * 0.5,
        Some(Alignment::End) => margin_left + content_width - table_width,
        _ => margin_left + indent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Alignment;

    fn page_break() -> Fragment {
        Fragment::PageBreak {
            line_height: Pt::ZERO,
        }
    }

    /// A distinguishable non-break fragment.
    fn item(text: &str) -> Fragment {
        Fragment::Bookmark { name: text.into() }
    }

    fn names(chunks: &[&[Fragment]]) -> Vec<Vec<String>> {
        chunks
            .iter()
            .map(|c| {
                c.iter()
                    .map(|f| match f {
                        Fragment::Bookmark { name, .. } => name.to_string(),
                        _ => "?".to_string(),
                    })
                    .collect()
            })
            .collect()
    }

    // ── §17.3.3.1 break chunking ─────────────────────────────────────────

    #[test]
    fn no_break_yields_the_whole_slice_as_one_chunk() {
        let frags = vec![item("a"), item("b")];
        assert_eq!(names(&split_at_page_breaks(&frags)), vec![vec!["a", "b"]]);
    }

    #[test]
    fn a_break_splits_and_is_itself_excluded() {
        let frags = vec![item("a"), page_break(), item("b")];
        assert_eq!(
            names(&split_at_page_breaks(&frags)),
            vec![vec!["a"], vec!["b"]],
            "the break fragment is dropped, not carried into either chunk"
        );
    }

    /// A leading break must produce an *empty* first chunk, not be swallowed:
    /// that empty chunk is what makes `<w:br type="page"/>` at the start of a
    /// paragraph break *before* its content. `layout.rs` skips empty chunks
    /// after acting on the break, so dropping it here would lose the break.
    #[test]
    fn a_leading_break_yields_an_empty_first_chunk() {
        let frags = vec![page_break(), item("a")];
        let chunks = split_at_page_breaks(&frags);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].is_empty(), "empty chunk carries the break");
        assert_eq!(names(&chunks)[1], vec!["a"]);
    }

    #[test]
    fn a_trailing_break_yields_an_empty_last_chunk() {
        let frags = vec![item("a"), page_break()];
        let chunks = split_at_page_breaks(&frags);
        assert_eq!(chunks.len(), 2);
        assert_eq!(names(&chunks)[0], vec!["a"]);
        assert!(chunks[1].is_empty());
    }

    /// Consecutive breaks each produce their own chunk boundary, so N breaks
    /// yield N+1 chunks however they cluster.
    #[test]
    fn consecutive_breaks_each_yield_a_boundary() {
        let frags = vec![item("a"), page_break(), page_break(), item("b")];
        let chunks = split_at_page_breaks(&frags);
        assert_eq!(chunks.len(), 3, "two breaks → three chunks");
        assert!(chunks[1].is_empty(), "the gap between the two breaks");
    }

    /// Column and page breaks are split independently — a column break is not
    /// a page boundary and vice versa.
    #[test]
    fn column_and_page_breaks_are_split_independently() {
        let frags = vec![item("a"), Fragment::ColumnBreak, item("b")];
        assert_eq!(
            split_at_page_breaks(&frags).len(),
            1,
            "a column break is not a page boundary"
        );
        assert_eq!(
            names(&split_at_column_breaks(&frags)),
            vec![vec!["a"], vec!["b"]]
        );
    }

    // ── §17.4.28 table alignment ─────────────────────────────────────────

    #[test]
    fn table_x_offset_centers_and_right_aligns_within_the_content_area() {
        let (indent, width, content, margin) =
            (Pt::ZERO, Pt::new(100.0), Pt::new(400.0), Pt::new(72.0));
        assert_eq!(
            table_x_offset(Some(Alignment::Center), indent, width, content, margin).raw(),
            72.0 + 150.0,
        );
        assert_eq!(
            table_x_offset(Some(Alignment::End), indent, width, content, margin).raw(),
            72.0 + 300.0,
        );
    }

    #[test]
    fn table_x_offset_applies_indent_when_left_aligned() {
        let (width, content, margin) = (Pt::new(100.0), Pt::new(400.0), Pt::new(72.0));
        for alignment in [None, Some(Alignment::Start), Some(Alignment::Both)] {
            assert_eq!(
                table_x_offset(alignment, Pt::new(20.0), width, content, margin).raw(),
                92.0,
                "{alignment:?} places at margin + indent"
            );
        }
    }

    /// §17.4.51: `tblInd` is deliberately ignored for centered and right-
    /// aligned tables — they are positioned as a unit within the content area,
    /// matching Word. Pinned because nothing in the signature says so.
    #[test]
    fn table_x_offset_ignores_indent_when_centered_or_right_aligned() {
        let (width, content, margin) = (Pt::new(100.0), Pt::new(400.0), Pt::new(72.0));
        for alignment in [Alignment::Center, Alignment::End] {
            let without = table_x_offset(Some(alignment), Pt::ZERO, width, content, margin);
            let with = table_x_offset(Some(alignment), Pt::new(50.0), width, content, margin);
            assert_eq!(without, with, "{alignment:?} ignores tblInd");
        }
    }
}
