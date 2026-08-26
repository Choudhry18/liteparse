//! Small helpers shared by the hand-rolled SpreadsheetML readers.
//!
//! Every part of the XLSX reader parses with `quick_xml::Reader` events rather
//! than the serde layer the DOCX vendor uses, for two reasons that are specific
//! to this format:
//!
//! * **Whitespace.** `PackageContents::from_bytes` runs
//!   [`substitute_whitespace_only_runs`](crate::docx::whitespace_workaround),
//!   which protects whitespace-only text from quick-xml's serde trimmer — but
//!   it is namespace-gated to WordprocessingML, so it deliberately no-ops on
//!   SpreadsheetML. A serde schema here would silently drop a cell whose whole
//!   value is `" "`. The event reader does not trim, so it never has the
//!   problem.
//!
//! * **Size.** Sheets are the bulk of a workbook, with single sheets running
//!   past 32 MB. serde materializes an intermediate `Vec` per element;
//!   streaming does not.

use quick_xml::events::{BytesCData, BytesRef, BytesStart, BytesText};

use crate::docx::error::Result;

/// Strip a namespace prefix from a qualified name (`x:sheetData` →
/// `sheetData`).
///
/// SpreadsheetML is usually written with the default namespace and no prefix,
/// but Excel Online, WPS and XlsIO all appear in the corpus and prefixed
/// output is legal, so no reader here may match on the qualified name.
pub(crate) fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Read an attribute by its **qualified** name, unescaped.
///
/// Qualified rather than local because `<sheet name="…" r:id="rId1"/>` is
/// exactly the collision `pptx::package` documents: quick-xml's serde layer
/// drops attribute prefixes, so `r:id` and a hypothetical `id` would be
/// indistinguishable. Matching the written name keeps them apart.
///
/// A malformed attribute is skipped rather than fatal, matching the fail-open
/// posture of `docx/parse/primitives/lenient.rs`.
pub(crate) fn attr(e: &BytesStart<'_>, want: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.as_ref() == want).then(|| match a.unescape_value() {
            Ok(v) => v.into_owned(),
            // An attribute with a broken entity keeps its raw form rather than
            // vanishing; for `r=`/`ref=` the raw form still parses.
            Err(_) => String::from_utf8_lossy(&a.value).into_owned(),
        })
    })
}

/// Read an attribute and parse it, treating both "absent" and "unparseable"
/// as `None`.
///
/// Deliberately lossy: a `<row r="abc">` is corruption, and the reader's job
/// is to keep reading the other 40,000 rows.
pub(crate) fn attr_parse<T: std::str::FromStr>(e: &BytesStart<'_>, want: &[u8]) -> Option<T> {
    attr(e, want)?.trim().parse().ok()
}

/// Read a boolean attribute, accepting OOXML's `1`/`0`/`true`/`false`
/// (§22.9.2.7) plus the `on`/`off` some producers write.
pub(crate) fn attr_bool(e: &BytesStart<'_>, want: &[u8], default: bool) -> bool {
    match attr(e, want).as_deref() {
        Some("1" | "true" | "on") => true,
        Some("0" | "false" | "off") => false,
        _ => default,
    }
}

/// quick-xml's leaf errors do not implement `Into<ParseError>` directly; they
/// go through `quick_xml::Error` and then the crate's existing `DeError` arm.
pub(crate) fn xml_err(e: impl Into<quick_xml::Error>) -> quick_xml::DeError {
    quick_xml::DeError::from(e.into())
}

/// Decode a text event: encoding, then XML entities, then SpreadsheetML's own
/// `_xHHHH_` escapes.
pub(crate) fn decode_text(t: &BytesText<'_>) -> Result<String> {
    let raw = t.xml10_content().map_err(xml_err)?;
    let unescaped = match quick_xml::escape::unescape(&raw) {
        Ok(unescaped) => unescaped,
        // A malformed entity costs one cell's fidelity, not the workbook.
        Err(e) => {
            log::warn!("undecodable entity in SpreadsheetML text: {e}");
            std::borrow::Cow::Borrowed(raw.as_ref())
        }
    };
    Ok(decode_control_escapes(&unescaped))
}

/// Decode a CDATA section. **Not** entity-unescaped: a literal `&amp;` inside
/// CDATA is five characters, not one.
pub(crate) fn decode_cdata(c: &BytesCData<'_>) -> Result<String> {
    Ok(decode_control_escapes(&c.decode().map_err(xml_err)?))
}

/// Resolve an entity reference to the text it stands for.
///
/// quick-xml's **raw event reader does not fold entities into `Text`** — it
/// emits `&amp;` as a separate [`Event::GeneralRef`](quick_xml::events::Event)
/// carrying the bare name, splitting the surrounding text in two. Any reader
/// that matches only on `Text` therefore *drops* every `&`, `<`, `>` and every
/// numeric character reference silently, and `R&amp;D &#3585;` arrives as
/// `RD `. (The serde layer the DOCX vendor uses resolves them itself, which is
/// why this only bites the hand-rolled readers here.)
///
/// SpreadsheetML declares no DTD, so only the five predefined entities and
/// numeric refs are legal. An unknown name is kept in its written form rather
/// than dropped — visible, and recoverable by a human, which silence is not.
pub(crate) fn decode_general_ref(r: &BytesRef<'_>) -> Result<String> {
    let name = r.decode().map_err(xml_err)?;
    let written = format!("&{name};");
    match quick_xml::escape::unescape(&written) {
        Ok(resolved) => Ok(resolved.into_owned()),
        Err(e) => {
            log::warn!("unresolvable entity {written}: {e}");
            Ok(written)
        }
    }
}

/// Decode `_xHHHH_` escapes for characters XML 1.0 cannot carry literally.
///
/// Excel writes an in-cell newline as `_x000D_` / `_x000A_`, because those
/// bytes would be normalized away by any conformant XML parser. Left undecoded
/// they surface as literal `_x000D_` in the middle of extracted text — visible
/// garbage in every downstream consumer.
///
/// **Only control characters are decoded**, which is narrower than the spec.
/// The escape is legal for any codepoint, so `_x0041_` officially means `A` —
/// but Excel never writes that form, whereas real strings containing an
/// underscore-and-hex run do exist (identifiers, file stems). Decoding those
/// would corrupt data to satisfy a form nothing produces. The escape-the-escape
/// sequence `_x005F_` is honoured so a literal `_x000D_` in the source round
/// trips.
pub(crate) fn decode_control_escapes(s: &str) -> String {
    if !s.contains("_x") {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        match escape_at(bytes, i) {
            // `_x005F_` escapes the underscore of a following escape.
            Some(0x5F) => {
                out.push('_');
                i += 7;
            }
            Some(c) if c < 0x20 || c == 0x7F => {
                out.push(c as u8 as char);
                i += 7;
            }
            _ => {
                let ch = s[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// The codepoint of an `_xHHHH_` escape starting at `i`, if one is there.
fn escape_at(bytes: &[u8], i: usize) -> Option<u32> {
    let slice = bytes.get(i..i + 7)?;
    if slice[0] != b'_' || slice[1] != b'x' || slice[6] != b'_' {
        return None;
    }
    let hex = std::str::from_utf8(&slice[2..6]).ok()?;
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::Reader;
    use quick_xml::events::Event;

    fn start(xml: &str) -> BytesStart<'static> {
        let mut reader = Reader::from_reader(xml.as_bytes());
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf).unwrap() {
                Event::Start(e) | Event::Empty(e) => return e.into_owned(),
                Event::Eof => panic!("no element"),
                _ => {}
            }
        }
    }

    #[test]
    fn local_name_strips_a_prefix_when_present() {
        assert_eq!(local_name(b"sheetData"), b"sheetData");
        assert_eq!(local_name(b"x:sheetData"), b"sheetData");
    }

    #[test]
    fn qualified_attributes_stay_distinct() {
        let e = start(r#"<sheet name="Q3" sheetId="1" r:id="rId7"/>"#);
        assert_eq!(attr(&e, b"r:id").as_deref(), Some("rId7"));
        assert_eq!(attr(&e, b"sheetId").as_deref(), Some("1"));
        // `id` alone must not resolve to the relationship id.
        assert_eq!(attr(&e, b"id"), None);
    }

    #[test]
    fn attribute_values_are_unescaped() {
        let e = start(r#"<sheet name="R&amp;D &lt;2026&gt;"/>"#);
        assert_eq!(attr(&e, b"name").as_deref(), Some("R&D <2026>"));
    }

    #[test]
    fn unparseable_numbers_are_none_not_zero() {
        // Zero would be a valid row/column index; silently coercing corruption
        // into cell A1 is the failure mode this guards.
        let e = start(r#"<row r="abc"/>"#);
        assert_eq!(attr_parse::<u32>(&e, b"r"), None);
        assert_eq!(attr_parse::<u32>(&e, b"missing"), None);
    }

    #[test]
    fn booleans_accept_both_spellings_and_fall_back_to_the_default() {
        let e = start(r#"<col hidden="1" customWidth="false" bestFit="true"/>"#);
        assert!(attr_bool(&e, b"hidden", false));
        assert!(!attr_bool(&e, b"customWidth", true));
        assert!(attr_bool(&e, b"bestFit", false));
        assert!(attr_bool(&e, b"absent", true));
    }
}
