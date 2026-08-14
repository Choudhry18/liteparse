//! Shared helpers for serde-driven OOXML parsers.

use quick_xml::events::Event;
use quick_xml::{Reader, Writer};
use serde::de::DeserializeOwned;

use crate::docx::error::Result;

/// Wordprocessing extension-namespace prefixes (Word 2010+, `w14`/`w15`/
/// `w16*`) whose *elements* are stripped before deserialization.
///
/// quick-xml's serde layer matches struct fields on local names with the
/// namespace prefix dropped, so `<w14:shadow>` (a DrawingML text effect)
/// lands on the same field as `<w:shadow>` (the §17.3.2.31 boolean). That
/// collision either fails the whole part with `duplicate field` when both
/// appear in one parent, or — worse — silently deserializes the extension
/// element into the transitional field when only the extension is present
/// (an attribute-only `<w14:shadow …>` reads as toggle-on). None of these
/// namespaces are modeled, so their elements are dropped wholesale rather
/// than matched namespace-aware; last-wins duplicate tolerance is
/// deliberately NOT the fix (it would let an unrelated extension element
/// overwrite a real property).
///
/// Matching is by prefix, not by declared namespace URI — every known
/// producer uses these conventional prefixes (they are what documents list
/// in `mc:Ignorable`, which is itself prefix-based). Attributes
/// (`w14:paraId`, `w14:textId`) are left untouched: their local names do
/// not collide with modeled attributes, and stripping them would churn
/// every modern Word document for no gain.
const EXTENSION_ELEMENT_PREFIXES: &[&[u8]] = &[
    b"w14",
    b"w15",
    b"w16",
    b"w16se",
    b"w16cid",
    b"w16cex",
    b"w16sdtdh",
    b"w16du",
];

/// Cheap gate for the filter pass: element-start markers for the prefixes
/// above (`<w16` covers the whole `w16*` family). Attribute occurrences
/// (` w14:paraId=`) do not match, so parts that only carry extension
/// *attributes* — i.e. most modern Word parts — skip the filter entirely.
const EXTENSION_ELEMENT_MARKERS: &[&[u8]] = &[b"<w14:", b"<w15:", b"<w16"];

/// Deserialize an OOXML part into a schema type, mapping quick-xml's error
/// into the crate's `ParseError`.
pub fn from_xml<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    if EXTENSION_ELEMENT_MARKERS
        .iter()
        .any(|m| contains_subslice(data, m))
    {
        let filtered = strip_extension_elements(data)?;
        return Ok(quick_xml::de::from_reader(filtered.as_slice())?);
    }
    Ok(quick_xml::de::from_reader(data)?)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_extension_element(name: quick_xml::name::QName) -> bool {
    name.prefix()
        .is_some_and(|p| EXTENSION_ELEMENT_PREFIXES.contains(&p.as_ref()))
}

/// Re-emit the part with every extension-namespace element subtree removed.
/// All other events pass through verbatim (text stays in its escaped form,
/// so the round-trip does not re-escape anything).
fn strip_extension_elements(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::from_reader(data);
    let mut writer = Writer::new(Vec::with_capacity(data.len()));
    let mut buf = Vec::new();
    let mut skip_buf = Vec::new();
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?;
        match ev {
            Event::Eof => break,
            Event::Start(ref e) if is_extension_element(e.name()) => {
                let end = e.to_end().into_owned();
                reader
                    .read_to_end_into(end.name(), &mut skip_buf)
                    .map_err(quick_xml::DeError::from)?;
            }
            Event::Empty(ref e) if is_extension_element(e.name()) => {}
            ev => writer.write_event(ev)?,
        }
        buf.clear();
    }
    Ok(writer.into_inner())
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::docx::parse::primitives::OnOff;

    #[derive(Debug, Deserialize)]
    struct Props {
        #[serde(rename = "shadow", default)]
        shadow: Vec<OnOff>,
        #[serde(rename = "b", default)]
        b: Vec<OnOff>,
    }

    /// The `worldbank.docx` shape: `w:shadow` and `w14:shadow` in one parent,
    /// non-adjacent, so serde sees the local name `shadow` twice and errors
    /// with `duplicate field`. The filter must keep the transitional element
    /// and drop the extension one.
    #[test]
    fn w14_collision_with_transitional_element_is_resolved() {
        let xml = r#"<rPr xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">
            <shadow val="0"/>
            <b/>
            <w14:shadow w14:blurRad="0" w14:dist="0"><w14:srgbClr w14:val="000000"/></w14:shadow>
        </rPr>"#;
        let p: Props = from_xml(xml.as_bytes()).expect("collision must not be fatal");
        assert_eq!(p.shadow, vec![OnOff(false)], "w:shadow val=0 must survive");
        assert_eq!(p.b, vec![OnOff(true)]);
    }

    /// A lone `w14:shadow` (no transitional sibling) previously deserialized
    /// into the `shadow` field as toggle-ON — silent corruption, no error.
    #[test]
    fn lone_w14_element_does_not_impersonate_transitional_field() {
        let xml = r#"<rPr><w14:shadow w14:blurRad="50800" w14:dist="38100"/></rPr>"#;
        let p: Props = from_xml(xml.as_bytes()).unwrap();
        assert!(
            p.shadow.is_empty(),
            "extension element must not set the boolean"
        );
    }

    /// Extension *attributes* are untouched — the filter only strips elements,
    /// and parts with only attribute occurrences must skip the filter pass.
    #[test]
    fn extension_attributes_pass_through() {
        #[derive(Debug, Deserialize)]
        struct Para {
            #[serde(rename = "@paraId", default)]
            para_id: Option<String>,
            #[serde(rename = "b", default)]
            b: Vec<OnOff>,
        }
        let xml = r#"<p w14:paraId="12AB34CD"><b/></p>"#;
        let p: Para = from_xml(xml.as_bytes()).unwrap();
        assert_eq!(p.para_id.as_deref(), Some("12AB34CD"));
        assert_eq!(p.b, vec![OnOff(true)]);
    }

    /// Escaped text must survive the filter round-trip in its escaped form.
    #[test]
    fn filter_preserves_escaped_text() {
        #[derive(Debug, Deserialize)]
        struct T {
            #[serde(rename = "t")]
            t: String,
        }
        let xml = r#"<r><w14:glow w14:rad="0"/><t>a &amp; b &lt; c</t></r>"#;
        let p: T = from_xml(xml.as_bytes()).unwrap();
        assert_eq!(p.t, "a & b < c");
    }

    /// `w16*` family prefixes are covered by the `<w16` marker and the exact
    /// prefix list; unrelated prefixes (`wp14`) are not stripped.
    #[test]
    fn w16_family_stripped_wp14_kept() {
        #[derive(Debug, Deserialize)]
        struct T {
            #[serde(rename = "sizeRelH", default)]
            size_rel_h: Option<String>,
            #[serde(rename = "shadow", default)]
            shadow: Vec<OnOff>,
        }
        let xml = r#"<x>
            <w16se:symEx w16se:font="Wingdings"/>
            <shadow/>
            <wp14:sizeRelH>keep</wp14:sizeRelH>
        </x>"#;
        let p: T = from_xml(xml.as_bytes()).unwrap();
        assert_eq!(p.size_rel_h.as_deref(), Some("keep"));
        assert_eq!(p.shadow, vec![OnOff(true)]);
    }
}
