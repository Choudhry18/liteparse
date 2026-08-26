//! Number formatting — the seam [`Workbook::format_code`] left open.
//!
//! A cell stores `0.155`; the sheet shows `15.5%`. The gap between the two is
//! ECMA-376 §18.8.31's format grammar, and the finance-slice census said it is
//! not a small one: **50.0% of documents need multi-section `pos;neg;zero;text`
//! dispatch**, 36.0% need quoted literals, 34.9% need `_` skip-width padding,
//! 23.2% `?` placeholders, 23.0% `*` fill, 14.4% `[Red]`. Excel's Accounting
//! code — `_("$"* #,##0.00_);…` in 99 documents — exercises five of those at
//! once.
//!
//! So the interpreter is [`ssf_rs`], a port of SheetJS's `ssf`, and this module
//! is only the adapter: which of [`CellValue`]'s six shapes gets fed to it,
//! what happens when it refuses, and what the caller is handed back.
//!
//! # Padding is preserved, not trimmed
//!
//! `_(* #,##0_)` renders `1234` as `" 1,235 "`. Those spaces are the format
//! doing its job — aligning a column — and stripping them here would be this
//! layer deciding how the value gets *presented*. A markdown emitter should
//! trim; a fixed-width one should not. The reader keeps what the format says.
//!
//! Colour and condition brackets (`[Red]`, `[<100]`) are consumed by the
//! interpreter and never reach the string, which is correct for a text target
//! and lossy for a styled one. Cell colour is out of scope for the reader
//! entirely (see the module docs for [`super`]).

use ssf_rs::Value;

use super::sheet::{Cell, CellValue};
use super::styles::GENERAL;
use super::{RichText, Workbook};

/// Render a value with a format code, exactly as the interpreter sees it.
///
/// Returns `Err` when the code is one `ssf` will not evaluate — a fifth
/// section, a malformed bracket. Callers that must produce *something* want
/// [`Workbook::display_text`], which applies the fallback; this one exists so a
/// corpus run can count the refusals instead of hiding them.
pub fn render(code: &str, value: &CellValue, date1904: bool) -> Result<String, String> {
    match value {
        CellValue::Number(n) => {
            let out = ssf_rs::format(code, &Value::Num(*n), date1904)?;
            // Deliberate divergence from the oracle — see `is_spliced_exponent`.
            if is_spliced_exponent(*n, code, &out) {
                return ssf_rs::format(GENERAL, &Value::Num(*n), date1904);
            }
            // Nothing to protect: a number has no source text of its own, so
            // any `NaN` in the output came from the interpreter.
            Ok(strip_nan(code, out, None))
        }
        CellValue::Bool(b) => Ok(bool_text(*b).to_string()),
        // An error is what the sheet displays verbatim; there is no format
        // section for it. Excel shows `#REF!` under any code.
        CellValue::Error(e) => Ok(e.clone()),
        // ECMA-376 Strict's literal ISO date. It is already a display string,
        // and re-deriving a serial to re-format it would only lose precision.
        CellValue::Date(d) => Ok(d.clone()),
        CellValue::Text(t) => Ok(render_text(code, &t.plain())),
        // A shared-string index means nothing without the table; the resolving
        // path is on `Workbook`.
        CellValue::SharedString(_) => Err("shared string needs the workbook table".into()),
    }
}

/// Whether the interpreter spliced digits into an exponent string and produced
/// something that reads as a different number entirely.
///
/// SheetJS formats by placing digits into a string produced by JS's
/// number-to-string, which switches to exponent notation outside
/// `[1e-6, 1e21)`. Neither implementation handles the switch: `1e-7` under
/// `0.00000000` is `"1e-7.00000000"` in SheetJS and `"1.2e-70000"` here; `1e21`
/// under `#,##0.00` is `"1e,+21.00"` in both.
///
/// This is the one place the port is deliberately not followed, because the
/// two failures are not equally bad — `"1.2e-70000"` reads as a nineteen-digit
/// number. `General` renders the value truthfully (`1.23E-10`), and a truthful
/// number under the wrong presentation beats a presentable number that is off
/// by orders of magnitude.
///
/// The detection is on the **output**, not the input, and the difference is
/// load-bearing: `1.23e-7` under `0.00` is `"0.00"` in both implementations,
/// because the rounding discards the mantissa before the splice can go wrong.
/// Rejecting every out-of-range magnitude would have broken those, which the
/// oracle diff duly reported.
///
/// The corpus reaches an out-of-range magnitude in **1,436 cells across 9 of
/// 1,178 workbooks**, so the guard is not hypothetical and is also not the
/// common path.
fn is_spliced_exponent(n: f64, code: &str, out: &str) -> bool {
    let mag = n.abs();
    if n == 0.0 || !n.is_finite() || (1e-6..1e21).contains(&mag) {
        return false;
    }
    has_exponent_marker(out) && !code_writes_exponents(code)
}

/// `e`/`E` followed by a sign or digit — an exponent, not a letter that happens
/// to sit next to a number.
///
/// Group separators and decimal points are skipped over, because the splice
/// puts them *inside* the exponent: `1e21` under `#,##0.00` comes out as
/// `"1e,+21.00"`, with the thousands comma landing between the `e` and its
/// sign. Requiring the sign to be adjacent misses precisely the case that
/// looks most like a real number.
fn has_exponent_marker(s: &str) -> bool {
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if !matches!(c, b'e' | b'E') {
            continue;
        }
        let next = bytes[i + 1..]
            .iter()
            .find(|b| !matches!(b, b',' | b'.' | b' '));
        if next.is_some_and(|n| matches!(n, b'+' | b'-') || n.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Whether the code itself asks for scientific notation (`0.00E+00`).
///
/// Only unquoted `E` is a placeholder: `"Revenue"` and `\e` are literals, and a
/// code carrying one must not be mistaken for a scientific format — that would
/// disable the guard for exactly the currency and label formats it exists for.
fn code_writes_exponents(code: &str) -> bool {
    let mut in_quotes = false;
    let mut in_bracket = false;
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            '[' if !in_quotes => in_bracket = true,
            ']' if !in_quotes => in_bracket = false,
            'e' | 'E' if !in_quotes && !in_bracket => return true,
            _ => {}
        }
    }
    false
}

/// The sentinel [`Workbook::fill_split`] substitutes for a `*` token: a quoted
/// literal the interpreter places verbatim, made of a character no format code
/// and no cell value contains.
const FILL_SENTINEL: char = '\u{1}';

/// Replace every active `*c` fill token with a quoted [`FILL_SENTINEL`],
/// consuming the repeated character with it.
///
/// Quoted spans and `\`-escapes are stepped over: a `*` inside `"..."` is a
/// literal asterisk, not a fill.
fn substitute_fill(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 8);
    let mut chars = code.chars();
    let mut in_quote = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quote = !in_quote;
                out.push(c);
            }
            '\\' => {
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '*' if !in_quote => {
                chars.next();
                out.push('"');
                out.push(FILL_SENTINEL);
                out.push('"');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Excel writes booleans as `0`/`1` and displays them uppercased, under any
/// format code — the numeric sections do not apply to `t="b"`.
fn bool_text(b: bool) -> &'static str {
    if b { "TRUE" } else { "FALSE" }
}

/// Text only meets the format when the code has something to say about it.
///
/// A code with no sections and no `@` — `0.00`, `#,##0` — leaves a string
/// alone, which is both Excel's behaviour and the reason a shared string used
/// 40,000 times is not run through the interpreter 40,000 times. Anything with
/// a `;` in it goes to the interpreter, because the *fourth* section is the
/// text section and an **empty** one blanks the cell: `#,##0.00;;;` displays
/// nothing at all, and a `@`-check alone would have rendered the string. The
/// oracle diff caught exactly that, on three corpus codes.
fn render_text(code: &str, text: &str) -> String {
    if code == GENERAL || (!code.contains('@') && !code.contains(';')) {
        return text.to_string();
    }
    let out = ssf_rs::format(code, &Value::Text(text.to_string()), false)
        .unwrap_or_else(|_| text.to_string());
    let out = strip_nan(code, out, Some(text));
    // `ssf-rs` 0.1 truncates the text at the fill character when `*`'s fill
    // char also occurs in the text: `@*.` renders `"1. kvartal:"` as `"1."`.
    // (`@* ` is unaffected — the defect is per fill char, and `.` is the one
    // the corpus hits.) A section with `@` places the text verbatim, so an
    // output that lost it is a wrong answer, not a presentation choice; fall
    // back to the raw text — keep the data, lose the fill. Detect on the
    // output, not the input: same rule as the `NaN` guard above, and for the
    // same reason (the first version of that guard rejected inputs and
    // regressed 1,100 correct renders).
    if code.contains('*') && code.contains('@') && !out.contains(text) {
        return text.to_string();
    }
    out
}

/// Remove a `NaN` the interpreter leaked into a cell.
///
/// `*` repeats the next character until the column is full, so with no column
/// width the repeat count is unknown; SheetJS repeats zero times and the port
/// stringifies the count instead — `@*.` renders `"Vareforbrug"` as
/// `"VareforbrugNaN."`. Removing the token reproduces the origin exactly, which
/// is why this is a repair and not a second opinion.
///
/// Gated on the `*` that leaks it, and on the code not writing `NaN` as a
/// literal — and then applied **only outside the value's own text**, because a
/// blind `replace` is a data-loss bug waiting for the right cell. The Zenodo
/// corpus supplied that cell: a conference named `NaNA`, in a paper title, and
/// an ungated strip shortens it to `A`. Skipping the repair whenever the source
/// contains `NaN` would be the opposite error — it leaves the leak in the one
/// title that has both.
///
/// `protect` is the source text a text cell renders from; a number has none,
/// and only renders `NaN` when the value itself is one.
///
/// **4 cells in 24M** hit the real leak; the guard is here because a cell
/// reading `NaN` is not a presentation defect, it is a wrong answer.
fn strip_nan(code: &str, out: String, protect: Option<&str>) -> String {
    if !out.contains("NaN") || code.contains("NaN") || !code.contains('*') {
        return out;
    }
    let Some(text) = protect.filter(|t| t.contains("NaN")) else {
        return out.replace("NaN", "");
    };
    // The format wraps the value in literals, so the value's own text is a
    // contiguous span of the output. Only what surrounds it is the interpreter
    // speaking.
    match out.find(text) {
        Some(at) => {
            let (before, rest) = out.split_at(at);
            let (mid, after) = rest.split_at(text.len());
            format!(
                "{}{mid}{}",
                before.replace("NaN", ""),
                after.replace("NaN", "")
            )
        }
        // The value did not survive verbatim, so there is no span to protect
        // and no safe strip. Leaving the leak is the recoverable failure.
        None => out,
    }
}

impl Workbook {
    /// The string a reader of the sheet sees in this cell.
    ///
    /// Infallible by construction: a format code the interpreter refuses falls
    /// back to `General`, and a `General` that somehow fails falls back to
    /// Rust's own float rendering. A cell never renders as nothing because its
    /// *style* was unusable — losing the number would be a strictly worse
    /// outcome than losing its presentation.
    ///
    /// Returns `None` only for a shared-string index the table cannot resolve,
    /// which is corruption and already warned about by
    /// [`Workbook::cell_text`].
    pub fn display_text(&self, cell: &Cell) -> Option<String> {
        let code = self.format_code(cell);
        if let CellValue::SharedString(_) = cell.value {
            let text = self.cell_text(cell)?;
            return Some(render_text(code, &text.plain()));
        }
        Some(match render(code, &cell.value, self.date1904) {
            Ok(s) => s,
            Err(e) => {
                log::debug!("format code {code:?} refused ({e}); falling back to General");
                self.fallback(&cell.value)
            }
        })
    }

    /// Where in [`Workbook::display_text`] the format asked for a repeat —
    /// §18.8.31's `*c`, which fills the rest of the *cell* with `c`.
    ///
    /// The interpreter cannot honour it: `*` means "until the column is full"
    /// and a formatter has no column. SheetJS repeats zero times and this port
    /// follows, so Excel's Accounting code `_("$"* #,##0.00_)` renders
    /// `" $1,234.50 "` — correct in content, wrong in shape, because Excel
    /// puts the `$` against the cell's left edge and the number against its
    /// right. A painter that knows the box can place both, but only if it
    /// knows where the repeat was; that byte offset is what this returns.
    ///
    /// The offset is recovered rather than tracked: the code's fill tokens are
    /// swapped for a quoted sentinel, the value is re-rendered, and the result
    /// is **accepted only if deleting the sentinel reproduces `display_text`
    /// byte for byte**. Verifying on the output is what makes the substitution
    /// safe to do at all — [`render`]'s two repairs are both gated on the `*`
    /// the substitution removes, so a sentinel render can legitimately differ,
    /// and when it does the answer is `None` rather than a guess. Over the
    /// 1,248-workbook corpus: 193,244 cells carry a `*` code, 181,117 (93.7%)
    /// recover an offset, 12,123 declare the fill in a section this value does
    /// not use, and **4 diverge** — the same four `@*.` cells
    /// [`strip_nan`] documents.
    ///
    /// `None` also for the overwhelming majority of cells, whose code has no
    /// `*` at all; that case costs one `contains` and no render.
    pub fn fill_split(&self, cell: &Cell) -> Option<usize> {
        let code = self.format_code(cell);
        if !code.contains('*') {
            return None;
        }
        let plain = self.display_text(cell)?;
        let sub = substitute_fill(code);
        let rendered = match &cell.value {
            CellValue::SharedString(_) | CellValue::Text(_) => {
                render_text(&sub, &self.cell_text(cell)?.plain())
            }
            value => render(&sub, value, self.date1904).ok()?,
        };
        let mut marks = rendered.match_indices(FILL_SENTINEL);
        let (at, _) = marks.next()?;
        // More than one active fill would need more than one split point, and
        // the painter has one gap to give. The corpus holds none.
        if marks.next().is_some() {
            return None;
        }
        let mut stripped = rendered.clone();
        stripped.remove(at);
        (stripped == plain).then_some(at)
    }

    fn fallback(&self, value: &CellValue) -> String {
        match render(GENERAL, value, self.date1904) {
            Ok(s) => s,
            Err(_) => match value {
                CellValue::Number(n) => n.to_string(),
                _ => String::new(),
            },
        }
    }

    /// Whether a cell's format code makes it a date rather than a number.
    ///
    /// The serial `45000.25` is a number until its format says `m/d/yyyy`, so
    /// this is the only way to tell — nothing in the cell itself distinguishes
    /// a date from a count.
    pub fn is_date_cell(&self, cell: &Cell) -> bool {
        matches!(cell.value, CellValue::Number(_)) && ssf_rs::is_date(self.format_code(cell))
    }
}

impl RichText {
    /// The plain text of this run sequence, formatted by `code`.
    ///
    /// Convenience for a caller holding text it already resolved.
    pub fn display_with(&self, code: &str) -> String {
        render_text(code, &self.plain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xlsx::text::{RichText, RunProps, TextRun};

    fn plain(text: &str) -> RichText {
        RichText {
            runs: vec![TextRun {
                text: text.to_string(),
                props: RunProps::default(),
            }],
        }
    }

    fn num(code: &str, n: f64) -> String {
        render(code, &CellValue::Number(n), false).unwrap()
    }

    /// The six mechanisms the finance-slice census named, each at the value
    /// that exercises it. Failing any one of these is a whole class of the
    /// corpus rendering wrong, not a rounding difference.
    #[test]
    fn the_censused_mechanisms_render() {
        // multi-section dispatch: 50.0% of documents
        let acct = r#"_("$"* #,##0.00_);_("$"* \("$"* #,##0.00\);_("$"* "-"??_);_(@_)"#;
        assert_eq!(num(acct, 1234.5678), " $1,234.57 ");
        assert_eq!(num(acct, -1234.5678), " $($1,234.57)");
        // zero section: `"-"??` is the accounting dash, padded to two digits
        assert_eq!(num(acct, 0.0), " $-   ");
        // quoted literal: 36.0%
        assert_eq!(num(r#"0.00"kg""#, 1234.5678), "1234.57kg");
        // skip-width `_)`: 34.9% — one space, not a dropped character
        assert_eq!(num("0.00_);(0.00)", 1234.5678), "1234.57 ");
        // `[Red]` is consumed, never emitted: 14.4%
        assert_eq!(num(r"#,##0.0;[Red]\-#,##0.0;0", -1234.5678), "-1,234.6");
        // comma scaling: 0.2% of docs, but silently 1000x wrong if unhandled
        assert_eq!(num("#,##0.00,,", 45000.25), "0.05");
        // suppressed sections render empty rather than falling back
        assert_eq!(num("0;;", -1234.5678), "");
        assert_eq!(num("0;;", 0.0), "");
    }

    /// The guard's whole point is that the un-guarded output is not merely
    /// unformatted but *misread*: `1.2e-70000` looks like a huge number.
    #[test]
    fn out_of_range_magnitudes_render_truthfully_instead_of_corrupt() {
        assert_eq!(num("0.00000000", 1.23e-7), "0.000000123");
        assert_eq!(num("#,##0.00", 1e21), "1E+21");
        // …but a code whose rounding never reaches the mantissa is left alone,
        // because it agrees with the oracle and `0.00` is the better answer.
        assert_eq!(num("0.00", 1.23e-7), "0.00");
        assert_eq!(num("#,##0.00", -1.23e-10), "-0.00");
        // Just inside the bounds the format always applies.
        assert_eq!(num("0.00000000", 1e-6), "0.00000100");
        assert_eq!(num("0.00", 0.0), "0.00");
        assert_eq!(num("#,##0.00", 1e20), "100,000,000,000,000,000,000.00");
    }

    /// A scientific code produces an exponent on purpose; treating it as
    /// corruption would route every such cell to `General`. A literal `E` in
    /// quotes is not a placeholder and must not grant the same exemption.
    #[test]
    fn a_scientific_code_keeps_its_exponent() {
        assert_eq!(num("0.00E+00", 1.23e-7), "1.23E-07");
        assert!(code_writes_exponents("0.00E+00"));
        assert!(!code_writes_exponents(r#""Revenue"0.00"#));
        assert!(!code_writes_exponents(r"\e0.00"));
        assert!(!code_writes_exponents("[Red]0.00"));
        assert!(has_exponent_marker("1.2e-70000"));
        assert!(!has_exponent_marker("Revenue"));
    }

    #[test]
    fn a_serial_becomes_a_date_only_because_the_code_says_so() {
        assert_eq!(num("m/d/yyyy", 45000.25), "3/15/2023");
        assert_eq!(num("General", 45000.25), "45000.25");
        assert!(ssf_rs::is_date("[$-409]mmmm d, yyyy;@"));
        assert!(!ssf_rs::is_date("#,##0.00"));
    }

    /// The 1904 epoch is 1,462 days off the 1900 one. A workbook-level flag
    /// that the renderer ignores shifts every date in the file by four years.
    #[test]
    fn the_1904_epoch_is_carried_not_assumed() {
        let v = CellValue::Number(45000.0);
        assert_eq!(render("m/d/yyyy", &v, false).unwrap(), "3/15/2023");
        assert_eq!(render("m/d/yyyy", &v, true).unwrap(), "3/16/2027");
    }

    #[test]
    fn text_meets_the_format_only_through_an_at_section() {
        // No `@` anywhere: the numeric code does not touch a string.
        assert_eq!(render_text("0.00", "hello"), "hello");
        assert_eq!(render_text(GENERAL, "hello"), "hello");
        // The fourth section of Accounting pads text like it pads numbers.
        assert_eq!(render_text(r#"_(* #,##0_);;;_(@_)"#, "hello"), " hello ");
        assert_eq!(render_text(r#""Note: "@"#, "x"), "Note: x");
    }

    /// The fourth section governs text even when it is empty, and empty means
    /// the cell displays nothing. Skipping the interpreter for codes without
    /// `@` renders the string instead — three corpus codes, found by the
    /// SheetJS diff rather than by reading the spec.
    #[test]
    fn an_empty_fourth_section_blanks_text() {
        assert_eq!(render_text(";;;", "NO"), "");
        assert_eq!(render_text("#,##0.00;;;", "0"), "");
        assert_eq!(
            render_text(r#"0.000_ ;[Red]\-0.000_ ;"inactive";"#, "x"),
            ""
        );
    }

    /// The port leaks its `*`-repeat count into the string when there is no
    /// column width to fill. Four corpus cells, found by the oracle diff.
    #[test]
    fn a_leaked_nan_is_removed_but_a_literal_one_is_kept() {
        assert_eq!(render_text("@*.", "Vareforbrug"), "Vareforbrug.");
        assert_eq!(render_text(r#"@" NaN""#, "x"), "x NaN");
        // The Zenodo counterexample: a conference called NaNA, under a fill
        // code. An ungated strip shortens the title.
        assert_eq!(
            render_text("@*.", "Networking and Network Applications (NaNA)"),
            "Networking and Network Applications (NaNA)."
        );
    }

    /// The upstream defect the emitter's recall gate caught: `ssf-rs` 0.1
    /// truncates the text at the fill character when it also occurs in the
    /// text — `@*.` on `"1. kvartal:"` came back `"1."`. The guard keeps the
    /// data and drops the fill, and must not disturb the two behaviours next
    /// to it: an intact render keeps its fill, and the empty fourth section
    /// still blanks (no `@` in that code, so the guard stays out of its way).
    #[test]
    fn a_fill_char_inside_the_text_does_not_truncate_it() {
        assert_eq!(render_text("@*.", "1. kvartal:"), "1. kvartal:");
        assert_eq!(render_text("@*.", "a.b c:"), "a.b c:");
        assert_eq!(render_text("@*.", "no dot"), "no dot.");
        assert_eq!(render_text("#,##0.00;;;", "hidden"), "");
    }

    #[test]
    fn booleans_and_errors_ignore_the_format_code() {
        assert_eq!(
            render("0.00", &CellValue::Bool(true), false).unwrap(),
            "TRUE"
        );
        assert_eq!(
            render("0.00", &CellValue::Bool(false), false).unwrap(),
            "FALSE"
        );
        let err = CellValue::Error("#REF!".into());
        assert_eq!(render("0.00%", &err, false).unwrap(), "#REF!");
    }

    #[test]
    fn a_strict_iso_date_is_already_a_display_string() {
        let d = CellValue::Date("2023-03-15T00:00:00".into());
        assert_eq!(
            render("m/d/yyyy", &d, false).unwrap(),
            "2023-03-15T00:00:00"
        );
    }

    #[test]
    fn a_shared_string_needs_the_workbook() {
        assert!(render("General", &CellValue::SharedString(0), false).is_err());
    }

    #[test]
    fn rich_text_formats_through_its_plain_text() {
        let rt = plain("hello");
        assert_eq!(rt.display_with(r#""Note: "@"#), "Note: hello");
    }
}
