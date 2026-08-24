//! A1-style cell references (§18.17.2.4) and the ranges built from them.
//!
//! Everything in a worksheet is addressed this way — `<c r="B7">`,
//! `<mergeCell ref="A1:C3">`, `<dimension ref="A1:Z100">` — so this is the one
//! piece the sheet reader cannot proceed without.
//!
//! Indices are **zero-based** throughout. XLSX rows are one-based in the file
//! (`A1` is row 1) and columns are letters; both are converted at the boundary
//! so no other module has to remember which convention it is holding.

/// A single cell address, zero-based on both axes. `A1` is `(0, 0)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellRef {
    pub row: u32,
    pub col: u32,
}

/// A rectangular range, inclusive on both ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeRef {
    pub start: CellRef,
    pub end: CellRef,
}

impl RangeRef {
    pub fn rows(&self) -> u32 {
        self.end.row.saturating_sub(self.start.row) + 1
    }

    pub fn cols(&self) -> u32 {
        self.end.col.saturating_sub(self.start.col) + 1
    }

    pub fn contains(&self, cell: CellRef) -> bool {
        cell.row >= self.start.row
            && cell.row <= self.end.row
            && cell.col >= self.start.col
            && cell.col <= self.end.col
    }
}

/// Parse a column label (`A`, `Z`, `AA`, `XFD`) to a zero-based index.
///
/// Bijective base-26: there is no zero digit, so `AA` is 26 rather than 0.
/// Returns `None` on an empty or non-alphabetic label, and on anything past
/// Excel's `XFD` limit — a label that long is corruption, and accepting it
/// would let a bad `r=` allocate an absurd column.
pub fn parse_column(label: &str) -> Option<u32> {
    if label.is_empty() || label.len() > 3 {
        return None;
    }
    let mut n: u32 = 0;
    for b in label.bytes() {
        let digit = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a',
            _ => return None,
        };
        n = n * 26 + digit as u32 + 1;
    }
    Some(n - 1)
}

/// Render a zero-based column index back to its label. Inverse of
/// [`parse_column`]; used by diagnostics and by the emitter when it has to
/// name a cell a human will look up in Excel.
pub fn column_label(mut col: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (col % 26) as u8);
        match col / 26 {
            0 => break,
            n => col = n - 1,
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII only")
}

/// Parse a single cell reference. Absolute markers (`$A$1`) are accepted and
/// ignored — they are a formula concern, and a `ref=` attribute may still
/// carry them.
pub fn parse_cell(s: &str) -> Option<CellRef> {
    let s = s.trim();
    let mut letters = String::new();
    let mut digits = String::new();
    for ch in s.chars() {
        match ch {
            '$' => continue,
            'A'..='Z' | 'a'..='z' if digits.is_empty() => letters.push(ch),
            '0'..='9' => digits.push(ch),
            // A sheet-qualified ref (`Sheet1!A1`) or anything else is not a
            // plain cell address; the caller decides what to do with `None`.
            _ => return None,
        }
    }
    let col = parse_column(&letters)?;
    // One-based in the file. Row 0 does not exist, so it is malformed input
    // rather than something to clamp.
    let row: u32 = digits.parse().ok()?;
    Some(CellRef {
        row: row.checked_sub(1)?,
        col,
    })
}

/// Parse a range. A bare cell (`A1`) is a legal one-cell range — Excel writes
/// `<dimension ref="A1"/>` for a single-cell sheet.
///
/// Ranges are normalized so `start` is the top-left corner: `C3:A1` and
/// `A1:C3` are the same rectangle, and a reader that trusted the written order
/// would compute a negative span.
pub fn parse_range(s: &str) -> Option<RangeRef> {
    let (a, b) = match s.split_once(':') {
        Some((a, b)) => (a, b),
        None => (s, s),
    };
    let a = parse_cell(a)?;
    let b = parse_cell(b)?;
    Some(RangeRef {
        start: CellRef {
            row: a.row.min(b.row),
            col: a.col.min(b.col),
        },
        end: CellRef {
            row: a.row.max(b.row),
            col: a.col.max(b.col),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_are_bijective_base_26() {
        assert_eq!(parse_column("A"), Some(0));
        assert_eq!(parse_column("Z"), Some(25));
        // The trap: base-26 with a zero digit would make this 0, not 26.
        assert_eq!(parse_column("AA"), Some(26));
        assert_eq!(parse_column("AB"), Some(27));
        assert_eq!(parse_column("BA"), Some(52));
        // Excel's last column.
        assert_eq!(parse_column("XFD"), Some(16_383));
    }

    #[test]
    fn column_label_round_trips() {
        for col in [0u32, 25, 26, 27, 51, 52, 701, 702, 16_383] {
            assert_eq!(parse_column(&column_label(col)), Some(col), "col {col}");
        }
    }

    #[test]
    fn oversized_or_empty_column_labels_are_rejected() {
        assert_eq!(parse_column(""), None);
        assert_eq!(parse_column("AAAA"), None);
        assert_eq!(parse_column("A1"), None);
    }

    #[test]
    fn cell_refs_are_zero_based() {
        assert_eq!(parse_cell("A1"), Some(CellRef { row: 0, col: 0 }));
        assert_eq!(parse_cell("B7"), Some(CellRef { row: 6, col: 1 }));
        assert_eq!(parse_cell("AA100"), Some(CellRef { row: 99, col: 26 }));
    }

    #[test]
    fn absolute_markers_are_ignored() {
        assert_eq!(parse_cell("$B$7"), parse_cell("B7"));
        assert_eq!(parse_cell("$B7"), parse_cell("B7"));
    }

    #[test]
    fn malformed_cell_refs_return_none() {
        assert_eq!(parse_cell("A0"), None, "row 0 does not exist");
        assert_eq!(parse_cell("1A"), None);
        assert_eq!(parse_cell("Sheet1!A1"), None);
        assert_eq!(parse_cell(""), None);
    }

    #[test]
    fn a_bare_cell_is_a_one_cell_range() {
        let r = parse_range("A1").unwrap();
        assert_eq!(r.rows(), 1);
        assert_eq!(r.cols(), 1);
    }

    #[test]
    fn ranges_are_normalized_to_top_left_first() {
        // A reader that trusted the written order would compute a negative
        // span here and silently drop the merge.
        let written = parse_range("C3:A1").unwrap();
        assert_eq!(written, parse_range("A1:C3").unwrap());
        assert_eq!(written.rows(), 3);
        assert_eq!(written.cols(), 3);
    }

    #[test]
    fn range_containment_is_inclusive() {
        let r = parse_range("B2:D4").unwrap();
        assert!(r.contains(parse_cell("B2").unwrap()));
        assert!(r.contains(parse_cell("D4").unwrap()));
        assert!(!r.contains(parse_cell("A2").unwrap()));
        assert!(!r.contains(parse_cell("E4").unwrap()));
    }
}
