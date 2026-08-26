//! Rich text: the `<si>` / `<is>` grammar (§18.4.8, §18.4.12) shared by the
//! shared-string table and by inline strings written straight into a sheet.
//!
//! Two things here are not obvious from the schema:
//!
//! * **`<rPh>` must be stripped.** An `<si>` may carry phonetic runs —
//!   *furigana*, the ruby Excel draws above a Japanese cell (§18.4.6). Their
//!   `<t>` elements sit at the same depth as the real ones, so a reader that
//!   concatenates every `<t>` emits `一般競争入札イッパンキョウソウニュウサツ`:
//!   the content immediately followed by its own pronunciation. Rich text is
//!   common in real workbooks, so this path is not an edge case.
//!
//! * **Runs are kept, not flattened.** A reader that concatenates `<r><t>`
//!   and discards every `<rPr>` throws away emphasis (bold, italic, size,
//!   font) before the emitter ever sees it.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::io::BufRead;

use crate::docx::error::Result;
use crate::xlsx::xml::{attr, decode_cdata, decode_general_ref, decode_text, local_name};

/// Character formatting on one run (§18.8.7 `CT_RPrElt`).
///
/// Colour is deliberately absent: an `<rPr>` colour may be `rgb`, `indexed`
/// (the legacy palette), `theme` + `tint`, or `auto`, and resolving those needs
/// the workbook theme and the styles palette. Nothing downstream reads a run
/// colour yet, so the honest move is to not half-resolve it here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunProps {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    /// `<sz val="11"/>` in points, when stated.
    pub size: Option<f32>,
    /// `<rFont val="Calibri"/>`, when stated.
    pub font: Option<String>,
    pub vert_align: Option<VertAlign>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertAlign {
    Superscript,
    Subscript,
}

/// One formatting run within a cell's text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub props: RunProps,
}

/// A cell's text as the file states it: an ordered list of runs.
///
/// A plain string is one run with default props, which keeps the common case
/// from needing a separate variant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub runs: Vec<TextRun>,
}

impl RichText {
    /// The whole string with formatting dropped.
    pub fn plain(&self) -> String {
        match self.runs.as_slice() {
            [] => String::new(),
            [only] => only.text.clone(),
            runs => runs.iter().map(|r| r.text.as_str()).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|r| r.text.is_empty())
    }

    /// True when the text carries more than one distinct formatting run — the
    /// case an emitter has to render with inline emphasis rather than as a
    /// plain string.
    pub fn is_rich(&self) -> bool {
        self.runs.len() > 1
            || self
                .runs
                .first()
                .is_some_and(|r| r.props != RunProps::default())
    }
}

/// Parse `xl/sharedStrings.xml` (§18.4.9) into the table cells index into.
///
/// Order is the whole contract: `<c t="s"><v>7</v></c>` means the eighth
/// `<si>`, so an `<si>` that fails to yield text must still occupy its slot.
/// Every `<si>` therefore pushes, empty or not.
pub fn parse_shared_strings(data: &[u8]) -> Result<Vec<RichText>> {
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    let mut nested = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?
        {
            Event::Eof => break,
            Event::Start(ref e) if local_name(e.name().as_ref()) == b"si" => {
                out.push(read_rich_text(&mut reader, &mut nested, b"si")?);
            }
            // `<si/>` is a legal empty string and still holds an index.
            Event::Empty(ref e) if local_name(e.name().as_ref()) == b"si" => {
                out.push(RichText::default());
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Read the body of an `<si>` or `<is>` element, with the reader positioned
/// just after its start tag. Stops on the matching end tag named `end`.
pub(crate) fn read_rich_text<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
    end: &[u8],
) -> Result<RichText> {
    let mut out = RichText::default();
    // Text found directly under <si>, outside any <r>. The overwhelmingly
    // common shape, and it never carries props.
    let mut bare = String::new();
    let mut run_text = String::new();
    let mut props = RunProps::default();
    let mut in_run = false;
    // Phonetic runs are skipped wholesale; see the module docs.
    let mut in_phonetic = false;
    let mut in_text = false;

    loop {
        let event = reader
            .read_event_into(buf)
            .map_err(quick_xml::DeError::from)?;
        match event {
            Event::Eof => break,
            Event::Start(ref e) => match local_name(e.name().as_ref()) {
                b"rPh" => in_phonetic = true,
                b"r" if !in_phonetic => {
                    in_run = true;
                    props = RunProps::default();
                    run_text.clear();
                }
                b"t" => in_text = true,
                name if in_run && !in_phonetic => apply_run_prop(&mut props, name, e),
                _ => {}
            },
            Event::Empty(ref e) => {
                if in_run && !in_phonetic {
                    apply_run_prop(&mut props, local_name(e.name().as_ref()), e);
                }
            }
            // CData is legal in a `<t>` and rare, but a producer that uses it
            // would otherwise lose the whole cell.
            // `GeneralRef` is not optional to handle: the raw reader emits
            // every entity as its own event, so omitting it drops each `&`,
            // `<`, `>` and numeric charref from the text. See
            // `xml::decode_general_ref`.
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if in_text && !in_phonetic => {
                let text = match event {
                    // CData is *not* entity-escaped; unescaping it would turn a
                    // literal `&amp;` inside a CDATA block into `&`.
                    Event::CData(ref c) => decode_cdata(c)?,
                    Event::Text(ref t) => decode_text(t)?,
                    Event::GeneralRef(ref r) => decode_general_ref(r)?,
                    _ => unreachable!("guarded by the pattern above"),
                };
                if in_run {
                    run_text.push_str(&text);
                } else {
                    bare.push_str(&text);
                }
            }
            Event::End(ref e) => match local_name(e.name().as_ref()) {
                b"rPh" => in_phonetic = false,
                b"t" => in_text = false,
                b"r" if in_run => {
                    in_run = false;
                    // An empty run carries no text and no position; dropping it
                    // keeps `is_rich` from firing on formatting-only noise that
                    // some producers emit around edits.
                    if !run_text.is_empty() {
                        out.runs.push(TextRun {
                            text: std::mem::take(&mut run_text),
                            props: std::mem::take(&mut props),
                        });
                    }
                }
                name if name == end => break,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    if !bare.is_empty() {
        // Bare text and runs are mutually exclusive in the schema, but a
        // malformed file can carry both; putting the bare text first preserves
        // document order for the only ordering that could have produced it.
        out.runs.insert(
            0,
            TextRun {
                text: bare,
                props: RunProps::default(),
            },
        );
    }
    Ok(out)
}

fn apply_run_prop(props: &mut RunProps, name: &[u8], e: &BytesStart<'_>) {
    // §18.8: these are CT_BooleanProperty, so `val="0"` turns the property
    // *off*. An absent val means true. Treating the element's presence as
    // "true" would render struck-through text that Excel shows normally.
    let on = || match attr(e, b"val") {
        Some(v) => !matches!(v.as_str(), "0" | "false"),
        None => true,
    };
    match name {
        b"b" => props.bold = on(),
        b"i" => props.italic = on(),
        b"strike" => props.strike = on(),
        // `<u/>` is CT_UnderlineProperty, not a boolean: its val is a style
        // name (`single`, `double`, `singleAccounting`), and `none` is the
        // off switch.
        b"u" => props.underline = !matches!(attr(e, b"val").as_deref(), Some("none")),
        b"sz" => props.size = attr(e, b"val").and_then(|v| v.parse().ok()),
        b"rFont" => props.font = attr(e, b"val"),
        b"vertAlign" => {
            props.vert_align = match attr(e, b"val").as_deref() {
                Some("superscript") => Some(VertAlign::Superscript),
                Some("subscript") => Some(VertAlign::Subscript),
                _ => None,
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xlsx::xml::decode_control_escapes;

    fn read(xml: &str, end: &[u8]) -> RichText {
        let mut reader = Reader::from_reader(xml.as_bytes());
        let mut buf = Vec::new();
        // Advance past the opening tag so the reader is where the real caller
        // leaves it.
        loop {
            match reader.read_event_into(&mut buf).unwrap() {
                Event::Start(ref e) if local_name(e.name().as_ref()) == end => break,
                Event::Eof => panic!("no opening tag"),
                _ => {}
            }
        }
        buf.clear();
        read_rich_text(&mut reader, &mut buf, end).unwrap()
    }

    #[test]
    fn plain_shared_string_is_one_run() {
        let t = read("<si><t>Total assets</t></si>", b"si");
        assert_eq!(t.plain(), "Total assets");
        assert_eq!(t.runs.len(), 1);
        assert!(!t.is_rich());
    }

    #[test]
    fn runs_keep_their_formatting() {
        let t = read(
            r#"<si><r><rPr><b/></rPr><t>Q3</t></r><r><t> actual</t></r></si>"#,
            b"si",
        );
        assert_eq!(t.plain(), "Q3 actual");
        assert_eq!(t.runs.len(), 2);
        assert!(t.runs[0].props.bold);
        assert!(!t.runs[1].props.bold);
        assert!(t.is_rich());
    }

    /// The furigana bug, as a test. Without the `<rPh>` guard this reads
    /// "東京トウキョウ" — the word followed by its own pronunciation.
    #[test]
    fn phonetic_runs_are_stripped() {
        let t = read(
            r#"<si><t>東京</t><rPh sb="0" eb="2"><t>トウキョウ</t></rPh><phoneticPr fontId="1"/></si>"#,
            b"si",
        );
        assert_eq!(t.plain(), "東京");
    }

    /// `<rPh>` also appears interleaved with real `<r>` runs, where a
    /// depth-blind reader would attach the reading to the wrong run.
    #[test]
    fn phonetic_runs_between_real_runs_are_stripped() {
        let t = read(
            r#"<si><r><t>東京</t></r><rPh sb="0" eb="2"><t>トウキョウ</t></rPh><r><t>都</t></r></si>"#,
            b"si",
        );
        assert_eq!(t.plain(), "東京都");
        assert_eq!(t.runs.len(), 2);
    }

    #[test]
    fn val_zero_turns_a_boolean_property_off() {
        // Excel writes `<b val="0"/>` when a style turned bold on and the run
        // turns it back off. Treating presence as true renders it bold.
        let t = read(
            r#"<si><r><rPr><b val="0"/><i/></rPr><t>x</t></r></si>"#,
            b"si",
        );
        assert!(!t.runs[0].props.bold);
        assert!(t.runs[0].props.italic);
    }

    #[test]
    fn underline_none_is_off_but_double_is_on() {
        let t = read(
            r#"<si><r><rPr><u val="none"/></rPr><t>a</t></r><r><rPr><u val="double"/></rPr><t>b</t></r><r><rPr><u/></rPr><t>c</t></r></si>"#,
            b"si",
        );
        assert!(!t.runs[0].props.underline);
        assert!(t.runs[1].props.underline);
        assert!(t.runs[2].props.underline);
    }

    #[test]
    fn size_and_font_are_captured() {
        let t = read(
            r#"<si><r><rPr><sz val="14.5"/><rFont val="Calibri"/></rPr><t>x</t></r></si>"#,
            b"si",
        );
        assert_eq!(t.runs[0].props.size, Some(14.5));
        assert_eq!(t.runs[0].props.font.as_deref(), Some("Calibri"));
    }

    #[test]
    fn inline_string_uses_the_same_grammar() {
        let t = read(r#"<is><r><rPr><i/></rPr><t>note</t></r></is>"#, b"is");
        assert_eq!(t.plain(), "note");
        assert!(t.runs[0].props.italic);
    }

    #[test]
    fn control_escapes_decode_to_the_character() {
        let t = read("<si><t>line one_x000D__x000A_line two</t></si>", b"si");
        assert_eq!(t.plain(), "line one\r\nline two");
    }

    #[test]
    fn printable_escapes_are_left_literal() {
        // `_x0041_` officially means "A", but Excel never writes it and real
        // strings do contain runs like this. Decoding would corrupt them.
        assert_eq!(decode_control_escapes("id_x0041_1"), "id_x0041_1");
    }

    #[test]
    fn escaped_underscore_round_trips_a_literal_escape() {
        assert_eq!(decode_control_escapes("_x005F_x000D_"), "_x000D_");
    }

    #[test]
    fn text_with_no_escape_marker_is_passed_through() {
        assert_eq!(decode_control_escapes("plain text"), "plain text");
        assert_eq!(decode_control_escapes("a_b_c"), "a_b_c");
    }

    #[test]
    fn multibyte_text_around_an_escape_is_not_split() {
        // The decoder walks bytes; slicing on a non-boundary would panic.
        assert_eq!(decode_control_escapes("東京_x000D_都"), "東京\r都");
    }

    #[test]
    fn xml_entities_are_unescaped() {
        let t = read("<si><t>R&amp;D &lt;2026&gt; &#3585;</t></si>", b"si");
        assert_eq!(t.plain(), "R&D <2026> ก");
    }

    #[test]
    fn empty_si_yields_empty_text() {
        let t = read("<si><t/></si>", b"si");
        assert!(t.is_empty());
        assert_eq!(t.plain(), "");
    }

    /// The shared-string table is addressed by position, so an `<si>` that
    /// carries nothing must still take its slot. Dropping it shifts every
    /// string after it — the worst possible failure mode, because the output
    /// stays plausible.
    #[test]
    fn empty_entries_keep_their_index() {
        let sst = r#"<sst count="4" uniqueCount="4">
            <si><t>alpha</t></si>
            <si/>
            <si><t/></si>
            <si><t>delta</t></si>
        </sst>"#;
        let table = parse_shared_strings(sst.as_bytes()).unwrap();
        assert_eq!(table.len(), 4);
        assert_eq!(table[0].plain(), "alpha");
        assert_eq!(table[3].plain(), "delta");
    }

    #[test]
    fn shared_string_table_reads_runs_and_strips_phonetics() {
        let sst = r#"<sst>
            <si><r><rPr><b/></rPr><t>Q3</t></r><r><t> total</t></r></si>
            <si><t>東京</t><rPh sb="0" eb="2"><t>トウキョウ</t></rPh></si>
        </sst>"#;
        let table = parse_shared_strings(sst.as_bytes()).unwrap();
        assert_eq!(table[0].plain(), "Q3 total");
        assert!(table[0].runs[0].props.bold);
        assert_eq!(table[1].plain(), "東京");
    }

    #[test]
    fn an_absent_table_is_an_empty_table() {
        assert!(
            parse_shared_strings(b"<sst count=\"0\"/>")
                .unwrap()
                .is_empty()
        );
    }
}
