//! Synthetic PDFs for ruled-table rows that span columns because a vertical
//! rule stops (issue 554). Each control is a case that a simpler rule breaks:
//! a merge on crossing text alone splits nothing correctly when the rule is
//! drawn, and a merge on missing rules alone destroys open-bodied tables.
use liteparse::types::PdfInput;
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};

const PAGE_H: f32 = 842.0;
/// Column boundaries, the same as in the PDFs attached to the issue.
const XS: [f32; 4] = [57.0, 190.0, 400.0, 538.0];
const GRID_TOP: f32 = 100.0;
const PITCH: f32 = 22.0;

/// A one-page PDF built from drawing operations. All coordinates use a
/// top-left origin. The builder flips y for the content stream.
struct Page {
    ops: String,
}

impl Page {
    fn new() -> Self {
        Page {
            ops: String::from("0.5 w\n"),
        }
    }

    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.ops += &format!("{x1} {} m {x2} {} l S\n", PAGE_H - y1, PAGE_H - y2);
    }

    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.ops += &format!("{x0} {} {} {} re S\n", PAGE_H - y1, x1 - x0, y1 - y0);
    }

    fn text(&mut self, x: f32, baseline: f32, s: &str, bold: bool) {
        let font = if bold { "F2" } else { "F1" };
        self.ops += &format!(
            "BT /{font} 10 Tf {x} {} Td ({s}) Tj ET\n",
            PAGE_H - baseline
        );
    }

    fn pdf(self) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R /F2 5 0 R >> >> /Contents 6 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_string(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", self.ops.len(), self.ops),
        ];
        let mut data = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(data.len());
            data.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
        }
        let xref = data.len();
        data.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        data.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        data
    }
}

async fn parse(page: Page) -> liteparse::ParseResult {
    LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        output_format: OutputFormat::Markdown,
        extract_blocks: true,
        quiet: true,
        ..LiteParseConfig::default()
    })
    .parse_input(PdfInput::Bytes(page.pdf()))
    .await
    .unwrap()
}

fn markdown(result: &liteparse::ParseResult) -> &str {
    &result.pages[0].markdown
}

/// The rows of a 3-column grid. `None` is a row whose text spans the grid.
type Rows<'a> = &'a [Option<[&'a str; 3]>];

const SECTION_ROWS: Rows = &[
    Some(["Item", "Description", "Amount"]),
    Some(["A-100", "Steel bracket", "12.50"]),
    Some(["A-200", "Copper fitting", "8.75"]),
    None,
    Some(["B-100", "Brass valve", "31.00"]),
    Some(["B-200", "Rubber seal", "2.10"]),
];
const SECTION_TEXT: &str = "Group B: parts that ship from the second warehouse, priced in euro";

fn row_y(r: usize) -> (f32, f32) {
    let y0 = GRID_TOP + PITCH * r as f32;
    (y0, y0 + PITCH)
}

fn grid_text(p: &mut Page, rows: Rows, spanning_text: &str) {
    for (r, row) in rows.iter().enumerate() {
        let baseline = row_y(r).0 + 15.0;
        match row {
            None => p.text(XS[0] + 6.0, baseline, spanning_text, false),
            Some(cells) => {
                for (x, s) in XS.iter().zip(cells) {
                    p.text(x + 6.0, baseline, s, r == 0);
                }
            }
        }
    }
}

fn grid_hlines_and_frame(p: &mut Page, n_rows: usize) {
    for r in 0..=n_rows {
        let y = row_y(r).0;
        p.line(XS[0], y, XS[3], y);
    }
    let bottom = row_y(n_rows).0;
    p.line(XS[0], GRID_TOP, XS[0], bottom);
    p.line(XS[3], GRID_TOP, XS[3], bottom);
}

#[derive(Debug, Clone, Copy)]
enum Style {
    /// Each interior rule is drawn one row at a time.
    PerRowSegments,
    /// Each cell is a stroked rect. The spanning row is one wide rect.
    CellRects,
    /// Each interior rule is two long strokes, broken only at the spanning row.
    /// Clustering same-x segments fills that gap, so only raw segments show it.
    TwoLongStrokes,
    /// Per-row segments inset 1.5pt from each horizontal rule.
    InsetSegments,
}

fn section_row_table(style: Style) -> Page {
    let mut p = Page::new();
    let n = SECTION_ROWS.len();
    let spanning = SECTION_ROWS.iter().position(Option::is_none).unwrap();
    match style {
        Style::CellRects => {
            for (r, row) in SECTION_ROWS.iter().enumerate() {
                let (y0, y1) = row_y(r);
                if row.is_none() {
                    p.rect(XS[0], y0, XS[3], y1);
                } else {
                    for c in 0..3 {
                        p.rect(XS[c], y0, XS[c + 1], y1);
                    }
                }
            }
        }
        Style::TwoLongStrokes => {
            grid_hlines_and_frame(&mut p, n);
            for x in [XS[1], XS[2]] {
                p.line(x, GRID_TOP, x, row_y(spanning).0);
                p.line(x, row_y(spanning).1, x, row_y(n).0);
            }
        }
        Style::PerRowSegments | Style::InsetSegments => {
            grid_hlines_and_frame(&mut p, n);
            let inset = if matches!(style, Style::InsetSegments) {
                1.5
            } else {
                0.0
            };
            for r in (0..n).filter(|&r| r != spanning) {
                let (y0, y1) = row_y(r);
                for x in [XS[1], XS[2]] {
                    p.line(x, y0 + inset, x, y1 - inset);
                }
            }
        }
    }
    grid_text(&mut p, SECTION_ROWS, SECTION_TEXT);
    p
}

#[tokio::test]
async fn section_row_without_interior_rules_spans_all_columns() {
    for style in [
        Style::PerRowSegments,
        Style::CellRects,
        Style::TwoLongStrokes,
        Style::InsetSegments,
    ] {
        let result = parse(section_row_table(style)).await;
        let md = markdown(&result);
        assert!(
            md.contains(&format!("<td colspan=\"3\">{SECTION_TEXT}</td>")),
            "{style:?}: the section row must be one cell:\n{md}"
        );
        assert!(
            md.contains("<td>B-100</td>\n<td>Brass valve</td>\n<td>31.00</td>"),
            "{style:?}: the rows below keep their columns:\n{md}"
        );
    }
}

/// The issue layout: a frame to the bottom of the page and a ruled header with
/// `header_rows` rows of 3 cells. The interior rules end at the header's
/// bottom rule.
fn framed_header_page(header_rows: &[[&str; 3]]) -> Page {
    let mut p = Page::new();
    p.rect(XS[0], 60.0, XS[3], 780.0);
    let header_bottom = 60.0 + 25.0 * header_rows.len() as f32;
    for (r, row) in header_rows.iter().enumerate() {
        let y = 60.0 + 25.0 * (r + 1) as f32;
        p.line(XS[0], y, XS[3], y);
        for (x, s) in XS.iter().zip(row) {
            p.text(x + 6.0, y - 9.0, s, false);
        }
    }
    for x in [XS[1], XS[2]] {
        p.line(x, 60.0, x, header_bottom);
    }
    p.text(63.0, header_bottom + 18.0, "1. Section", true);
    for i in 0..3 {
        let line = vec![format!("row_{}", i + 1); 10].join(", ");
        p.text(63.0, header_bottom + 31.0 + 13.0 * i as f32, &line, false);
    }
    p
}

const HEADER_1: [&str; 3] = ["Lorem ipsum", "Dolor sit amet", "Page 1"];
const HEADER_2: [&str; 3] = ["Consectetur", "Adipiscing elit", "No. 42"];

#[tokio::test]
async fn open_body_row_under_a_ruled_header_keeps_its_lines_in_order() {
    let row_1 = ["row_1"; 10].join(", ");
    let row_2 = ["row_2"; 10].join(", ");

    // Two header rows: a table whose last row is one cell, in reading order.
    let result = parse(framed_header_page(&[HEADER_1, HEADER_2])).await;
    let md = markdown(&result);
    assert!(
        md.contains(&format!("<td colspan=\"3\">1. Section {row_1} {row_2}")),
        "the body row must be one cell, in reading order:\n{md}"
    );
    // The layout JSON keeps kind "table" and one cell per column.
    let blocks = result.pages[0].blocks.as_ref().unwrap();
    let table = blocks.iter().find(|b| b.kind == "table").unwrap();
    let rows = table.rows.as_ref().unwrap();
    let body = rows.last().unwrap();
    assert_eq!(body.len(), 3);
    assert_eq!(body[0].colspan, Some(3));
    assert!(body[1].text.is_empty() && body[1].bbox.is_none());
    let bbox = body[0].bbox.as_ref().unwrap();
    assert!(
        (bbox.x - XS[0]).abs() < 1.0 && (bbox.x + bbox.width - XS[3]).abs() < 1.0,
        "the spanning cell covers the full width: {bbox:?}"
    );

    // One header row: 2 of the 6 cells are covered, so the grid fails the
    // empty-cell gate as before. The text must still come out in order.
    let result = parse(framed_header_page(&[HEADER_1])).await;
    let md = markdown(&result);
    assert!(
        md.contains(&format!("{row_1} {row_2}")),
        "the body lines must stay whole and in order:\n{md}"
    );
}

#[tokio::test]
async fn row_merges_only_across_the_rule_that_stops() {
    let mut p = Page::new();
    let n = SECTION_ROWS.len();
    let spanning = SECTION_ROWS.iter().position(Option::is_none).unwrap();
    grid_hlines_and_frame(&mut p, n);
    p.line(XS[2], GRID_TOP, XS[2], row_y(n).0);
    p.line(XS[1], GRID_TOP, XS[1], row_y(spanning).0);
    p.line(XS[1], row_y(spanning).1, XS[1], row_y(n).0);
    grid_text(
        &mut p,
        SECTION_ROWS,
        "Note: the B parts ship from warehouse two",
    );
    p.text(XS[2] + 6.0, row_y(spanning).0 + 15.0, "40.00", false);

    let result = parse(p).await;
    let md = markdown(&result);
    assert!(
        md.contains(
            "<td colspan=\"2\">Note: the B parts ship from warehouse two</td>\n<td>40.00</td>"
        ),
        "only the first two columns merge:\n{md}"
    );
}

#[tokio::test]
async fn header_only_dividers_keep_an_open_body_in_columns() {
    let rows: Rows = &[
        Some(["Item", "Description", "Amount"]),
        Some(["A-100", "Steel bracket", "12.50"]),
        Some(["A-200", "Copper fitting", "8.75"]),
        Some(["A-300", "Brass valve", "31.00"]),
    ];
    let mut p = Page::new();
    grid_hlines_and_frame(&mut p, rows.len());
    for x in [XS[1], XS[2]] {
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH);
    }
    grid_text(&mut p, rows, "");

    let result = parse(p).await;
    let md = markdown(&result);
    assert!(
        md.contains("| A-100 | Steel bracket | 12.50 |"),
        "no text crosses the open boundaries, so the body keeps its columns:\n{md}"
    );
    assert!(!md.contains("colspan"), "{md}");
}

#[tokio::test]
async fn text_run_across_a_drawn_rule_still_splits() {
    let rows: Rows = &[
        Some(["Item", "Description", "Amount"]),
        Some(["A-100", "Steel bracket", "12.50"]),
        Some(["A-200", "Copper fitting", "8.75"]),
    ];
    let n = rows.len() + 1;
    let mut p = Page::new();
    grid_hlines_and_frame(&mut p, n);
    for x in [XS[1], XS[2]] {
        p.line(x, GRID_TOP, x, row_y(n).0);
    }
    grid_text(&mut p, rows, "");
    let baseline = row_y(rows.len()).0 + 15.0;
    p.text(XS[0] + 6.0, baseline, "A-300", false);
    // One text object for two cells: "Rubber seal" (53.91pt in Helvetica)
    // ends at x=397.4, and "2.10" starts at x=403.0, past the rule at x=400.
    p.text(
        XS[2] + 3.0 - 53.91 - 2.0 * 2.78,
        baseline,
        "Rubber seal  2.10",
        false,
    );

    let result = parse(p).await;
    let md = markdown(&result);
    assert!(
        md.contains("| A-300 | Rubber seal | 2.10 |"),
        "a drawn rule is a cell edge, even under one text run:\n{md}"
    );
    assert!(!md.contains("colspan"), "{md}");
}

#[tokio::test]
async fn joined_cell_run_under_header_only_dividers_still_splits() {
    // No rule at x=400 below the header, and one text run crosses it. The
    // run's words leave a column gap at the boundary ("seal" ends at 397.4,
    // "2.10" starts at 403.0), so the run is two cells, not a spanning row.
    let rows: Rows = &[
        Some(["Item", "Description", "Amount"]),
        Some(["A-100", "Steel bracket", "12.50"]),
        Some(["A-200", "Copper fitting", "8.75"]),
    ];
    let n = rows.len() + 1;
    let mut p = Page::new();
    grid_hlines_and_frame(&mut p, n);
    for x in [XS[1], XS[2]] {
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH);
    }
    grid_text(&mut p, rows, "");
    let baseline = row_y(rows.len()).0 + 15.0;
    p.text(XS[0] + 6.0, baseline, "A-300", false);
    p.text(
        XS[2] + 3.0 - 53.91 - 2.0 * 2.78,
        baseline,
        "Rubber seal  2.10",
        false,
    );

    let result = parse(p).await;
    let md = markdown(&result);
    assert!(md.contains("| A-300 | Rubber seal | 2.10 |"), "{md}");
    assert!(!md.contains("colspan"), "{md}");
}
