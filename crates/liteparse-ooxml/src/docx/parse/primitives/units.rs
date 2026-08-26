//! `Deserialize` for `Dimension<U>`. Makes OOXML numeric attributes with a
//! unit marker (twips, EMU, half-points, etc.) usable directly in schema
//! structs without hand-written wrapper types.

use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StringDeserializer};
use serde::{Deserialize, Deserializer};

use super::integer_measure::IntegerMeasure;
use crate::model::dimension::{Dimension, Unit};

/// Optional non-negative measurement. A value that is unparseable **or**
/// negative degrades to `None` — "unspecified", so the style cascade supplies
/// the value — rather than failing the document. dxpdf errors on both cases;
/// here, one malformed measurement (e.g. `w:before="-100"`) must not cost
/// the whole parse.
pub(crate) fn deserialize_optional_nonnegative_dimension<'de, D, U>(
    deserializer: D,
) -> Result<Option<Dimension<U>>, D::Error>
where
    D: Deserializer<'de>,
    U: Unit,
{
    // Take the raw text first: a failed measurement parse is a value we want
    // to discard, not an error to propagate out of the containing struct.
    let Some(raw) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let de: StringDeserializer<ValueError> = raw.into_deserializer();
    Ok(IntegerMeasure::deserialize(de)
        .ok()
        .filter(|m| !m.is_negative())
        .map(|m| Dimension::new(m.value())))
}

impl<'de, U: Unit> Deserialize<'de> for Dimension<U> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Dimension::new(IntegerMeasure::deserialize(d)?.value()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::{Emu, HalfPoints, Twips};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TwipsVal {
        #[serde(rename = "@val")]
        val: Dimension<Twips>,
    }

    #[derive(Deserialize)]
    struct Sample {
        #[serde(rename = "@w")]
        w: Dimension<Emu>,
        #[serde(rename = "@h")]
        h: Dimension<HalfPoints>,
    }

    #[derive(Deserialize)]
    struct OptionalNonnegativeTwips {
        #[serde(
            rename = "@val",
            default,
            deserialize_with = "deserialize_optional_nonnegative_dimension"
        )]
        val: Option<Dimension<Twips>>,
    }

    #[test]
    fn twips_attribute_deserializes() {
        let v: TwipsVal = quick_xml::de::from_str(r#"<x val="720"/>"#).unwrap();
        assert_eq!(v.val.raw(), 720);
    }

    #[test]
    fn mixed_unit_attributes() {
        let s: Sample = quick_xml::de::from_str(r#"<ext w="914400" h="400"/>"#).unwrap();
        assert_eq!(s.w.raw(), 914_400);
        assert_eq!(s.h.raw(), 400);
    }

    #[test]
    fn negative_values_preserved() {
        let v: TwipsVal = quick_xml::de::from_str(r#"<x val="-120"/>"#).unwrap();
        assert_eq!(v.val.raw(), -120);
    }

    #[test]
    fn non_integer_rejected() {
        let r: Result<TwipsVal, _> = quick_xml::de::from_str(r#"<x val="abc"/>"#);
        assert!(
            r.is_err(),
            "expected error, got {:?}",
            r.map(|v| v.val.raw())
        );
    }

    /// A negative fraction must degrade to `None`, not be silently truncated
    /// to `0` — that would masquerade as an explicit zero.
    #[test]
    fn negative_fractions_are_unspecified_for_optional_nonnegative_dimensions() {
        for raw in ["-0.1", "-0.49"] {
            let value: OptionalNonnegativeTwips =
                quick_xml::de::from_str(&format!(r#"<x val="{raw}"/>"#))
                    .unwrap_or_else(|e| panic!("{raw:?} must not fail the parse: {e}"));
            assert_eq!(
                value.val.map(|d| d.raw()),
                None,
                "{raw:?} must be unspecified, not coerced"
            );
        }
    }

    /// An unparseable measurement degrades the same way.
    #[test]
    fn unparseable_optional_nonnegative_dimension_is_unspecified() {
        let value: OptionalNonnegativeTwips = quick_xml::de::from_str(r#"<x val="bogusValue"/>"#)
            .expect("a bogus measurement must not fail the parse");
        assert_eq!(value.val.map(|d| d.raw()), None);
    }

    /// Good values still parse — the leniency must not swallow real input.
    #[test]
    fn valid_optional_nonnegative_dimension_still_parses() {
        let value: OptionalNonnegativeTwips = quick_xml::de::from_str(r#"<x val="240"/>"#).unwrap();
        assert_eq!(value.val.map(|d| d.raw()), Some(240));
    }
}
