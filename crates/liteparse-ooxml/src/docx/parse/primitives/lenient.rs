//! Tolerant deserializers for OOXML simple-type (`ST_*`) attribute values.
//!
//! One bad attribute value must not fail the whole document. dxpdf models
//! each `ST_*` simple type as a plain serde enum, so an unrecognized value
//! (a newer spec revision, a producer quirk, or plain corruption) aborts the
//! entire parse; these helpers swallow that failure instead.
//!
//! ECMA-376 §17.17 treats an invalid attribute value as if the attribute were
//! absent, so the correct degradation is "unspecified", not "some other value".
//! That matters: absent means *inherit from the style chain*, whereas
//! substituting a default would silently override an inherited value.
//!
//! These helpers only apply where absence is representable. Required
//! attributes with no meaningful "unspecified" state are handled at their own
//! definition instead.

use serde::Deserialize;
use serde::de::value::Error as ValueError;
use serde::de::value::StrDeserializer;
use serde::de::{Deserializer, IntoDeserializer, Visitor};

/// Deserializer over one raw attribute string that *coerces* to the target
/// type instead of requiring the target to be string-shaped.
///
/// `serde`'s own `StrDeserializer` forwards every `deserialize_*` hook to
/// `visit_str`, so `u32::deserialize` on it fails. That matters a great deal
/// here, because these helpers deliberately swallow conversion failures: using
/// it would have turned every numeric attribute (`gridSpan`, `numId`, `ilvl`,
/// `outlineLvl`, …) silently into `None`, corrupting documents rather than
/// merely being strict about them. This wrapper parses the integer/float/bool
/// forms and leaves everything else to `visit_str`, which is what the
/// string-shaped types (the `ST_*` enums) want.
struct AttrValueDeserializer<'a>(&'a str);

macro_rules! coerce {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
            match self.0.trim().parse::<$ty>() {
                Ok(n) => v.$visit(n),
                Err(e) => Err(serde::de::Error::custom(e)),
            }
        }
    };
}

impl<'de> Deserializer<'de> for AttrValueDeserializer<'_> {
    type Error = ValueError;

    fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_str(self.0)
    }

    coerce!(deserialize_i8, visit_i8, i8);
    coerce!(deserialize_i16, visit_i16, i16);
    coerce!(deserialize_i32, visit_i32, i32);
    coerce!(deserialize_i64, visit_i64, i64);
    coerce!(deserialize_u8, visit_u8, u8);
    coerce!(deserialize_u16, visit_u16, u16);
    coerce!(deserialize_u32, visit_u32, u32);
    coerce!(deserialize_u64, visit_u64, u64);
    coerce!(deserialize_f32, visit_f32, f32);
    coerce!(deserialize_f64, visit_f64, f64);
    coerce!(deserialize_bool, visit_bool, bool);

    /// Unit-variant enums (every `ST_*` type) need `visit_enum`, not
    /// `visit_str`, so this one hook delegates to serde's string deserializer
    /// rather than falling through to `deserialize_any`.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, Self::Error> {
        let d: StrDeserializer<'_, ValueError> = self.0.into_deserializer();
        d.deserialize_enum(name, variants, v)
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_some(self)
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct newtype_struct
        seq tuple tuple_struct map struct identifier ignored_any
    }
}

/// For `Option<StX>` attribute fields: unknown value → `None`.
///
/// Pair with `#[serde(default)]` so an absent attribute and an unparseable one
/// both land on `None`.
pub fn opt_attr<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    // Attribute values always arrive as strings, so we can take the raw text
    // first and only then attempt the enum conversion — the failure is ours to
    // swallow rather than the outer deserializer's to propagate.
    let Some(raw) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    Ok(T::deserialize(AttrValueDeserializer(&raw)).ok())
}

/// For `<w:x w:val="…"/>` elements whose `@val` is an `StX`: an unknown value
/// makes the whole element `None`, exactly as if it had not been written.
///
/// Collapses the `ValAttr<T>` wrapper away, so the field type is `Option<T>`
/// rather than `Option<ValAttr<T>>`.
pub fn opt_val_attr<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    /// `@val` is taken as a string so an unrecognized token is a value we can
    /// inspect rather than a deserializer error we cannot intercept.
    #[derive(Deserialize)]
    struct RawVal {
        #[serde(rename = "@val")]
        val: Option<String>,
    }

    let Some(raw) = Option::<RawVal>::deserialize(d)?.and_then(|r| r.val) else {
        return Ok(None);
    };
    Ok(T::deserialize(AttrValueDeserializer(&raw)).ok())
}

/// For *required* `StX` attributes that have a spec default: unknown value →
/// `T::default()`, i.e. treated as if the attribute were absent, which is what
/// §17.17 prescribes and what the field's own `#[serde(default)]` already does.
///
/// Only correct where `T`'s `Default` *is* the spec default. Where no default
/// is meaningful, make the field `Option<T>` with [`opt_attr`] instead and let
/// the consumer drop the containing element.
pub fn or_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let raw = String::deserialize(d)?;
    Ok(T::deserialize(AttrValueDeserializer(&raw)).unwrap_or_default())
}

/// Required non-negative measurement: unparseable **or negative** → zero.
///
/// The zero fallback suits DrawingML sizes and offsets, where the value is
/// appearance-only and a zero-size shape is a harmless degradation. Keeps the
/// non-negativity guard that `deserialize_nonnegative_dimension` provides —
/// dropping it would let a negative extent through as a real size.
pub fn nonneg_or_default<'de, D, U>(d: D) -> Result<crate::model::dimension::Dimension<U>, D::Error>
where
    D: Deserializer<'de>,
    U: crate::model::dimension::Unit,
{
    use crate::docx::parse::primitives::integer_measure::IntegerMeasure;
    use crate::model::dimension::Dimension;

    let raw = String::deserialize(d)?;
    Ok(IntegerMeasure::deserialize(AttrValueDeserializer(&raw))
        .ok()
        .filter(|m| !m.is_negative())
        .map_or_else(Dimension::default, |m| Dimension::new(m.value())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum StFake {
        Left,
        Right,
    }

    #[derive(Debug, Deserialize)]
    struct AttrHolder {
        #[serde(rename = "@jc", default, deserialize_with = "opt_attr")]
        jc: Option<StFake>,
    }

    #[derive(Debug, Deserialize)]
    struct ValHolder {
        #[serde(rename = "jc", default, deserialize_with = "opt_val_attr")]
        jc: Option<StFake>,
    }

    fn attr(xml: &str) -> Option<StFake> {
        quick_xml::de::from_str::<AttrHolder>(xml).unwrap().jc
    }

    fn val(xml: &str) -> Option<StFake> {
        quick_xml::de::from_str::<ValHolder>(xml).unwrap().jc
    }

    #[test]
    fn attr_known_value_parses() {
        assert_eq!(attr(r#"<p jc="left"/>"#), Some(StFake::Left));
    }

    #[test]
    fn attr_unknown_value_is_none_not_an_error() {
        assert_eq!(attr(r#"<p jc="bogusValue"/>"#), None);
    }

    #[test]
    fn attr_absent_is_none() {
        assert_eq!(attr("<p/>"), None);
    }

    #[test]
    fn val_known_value_parses() {
        assert_eq!(val(r#"<p><jc val="right"/></p>"#), Some(StFake::Right));
    }

    #[test]
    fn val_unknown_value_is_none_not_an_error() {
        assert_eq!(val(r#"<p><jc val="bogusValue"/></p>"#), None);
    }

    #[test]
    fn val_absent_element_and_absent_attr_are_both_none() {
        assert_eq!(val("<p/>"), None);
        assert_eq!(val("<p><jc/></p>"), None);
    }

    // ── numeric coercion ──────────────────────────────────────────────────
    //
    // These guard a bug that silently corrupts documents rather than failing
    // them: serde's own string deserializer refuses to produce an integer, so
    // routing a numeric attribute through these helpers would swallow every
    // *valid* value as `None`. `gridSpan`, `numId`, `ilvl` and `outlineLvl`
    // all depend on this.

    #[derive(Debug, Deserialize)]
    struct NumAttrHolder {
        #[serde(rename = "@n", default, deserialize_with = "opt_attr")]
        n: Option<u32>,
    }

    #[derive(Debug, Deserialize)]
    struct NumValHolder {
        #[serde(rename = "n", default, deserialize_with = "opt_val_attr")]
        n: Option<i64>,
    }

    #[test]
    fn valid_numeric_attribute_is_not_swallowed() {
        let h: NumAttrHolder = quick_xml::de::from_str(r#"<x n="3"/>"#).unwrap();
        assert_eq!(h.n, Some(3), "a valid number must survive the helper");
    }

    #[test]
    fn valid_numeric_val_element_is_not_swallowed() {
        let h: NumValHolder = quick_xml::de::from_str(r#"<x><n val="-7"/></x>"#).unwrap();
        assert_eq!(h.n, Some(-7));
    }

    #[test]
    fn bogus_numeric_attribute_is_none_not_an_error() {
        let h: NumAttrHolder = quick_xml::de::from_str(r#"<x n="bogusValue"/>"#).unwrap();
        assert_eq!(h.n, None);
    }

    #[test]
    fn out_of_range_numeric_attribute_is_none_not_a_wrapped_value() {
        // u32::MAX + 1 must not wrap around into a plausible-looking number.
        let h: NumAttrHolder = quick_xml::de::from_str(r#"<x n="4294967296"/>"#).unwrap();
        assert_eq!(h.n, None);
    }

    #[test]
    fn or_default_coerces_numbers_and_falls_back() {
        #[derive(Debug, Deserialize)]
        struct H {
            #[serde(rename = "@n", default, deserialize_with = "or_default")]
            n: u32,
        }
        let ok: H = quick_xml::de::from_str(r#"<x n="12"/>"#).unwrap();
        assert_eq!(ok.n, 12);
        let bad: H = quick_xml::de::from_str(r#"<x n="bogusValue"/>"#).unwrap();
        assert_eq!(bad.n, 0);
    }
}
