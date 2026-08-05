//! OOXML color-attribute primitives.
//!
//! - [`HexColor`] — ST_HexColor (§17.3.4.1): either the literal `"auto"`
//!   sentinel or a 6-digit RGB hex. Used by `<w:color>` and DrawingML color
//!   choices where "auto" is spec-legal.
//! - [`RgbHexU32`] — ST_HexColorRGB (§20.1.10.41): strictly a 6-digit RGB hex.
//!   Used where the spec disallows "auto".
//!
//! Both fail deserialization on malformed input (strict per plan §Decisions).

use serde::{Deserialize, Deserializer};

use crate::docx::model::Color;

/// Parse exactly six ASCII hex digits into a packed `0xRRGGBB`. ST_HexColor
/// (§17.3.4.1) and ST_HexColorRGB (§20.1.10.41) are both defined as a 6-digit
/// RGB hex; anything else (3-digit shorthand, 8-digit ARGB, a leading sign)
/// is rejected rather than silently mis-decoded by `from_str_radix`.
fn parse_rgb_hex6(s: &str) -> Result<u32, &'static str> {
    if s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(u32::from_str_radix(s, 16).expect("six hex digits always fit in u32"))
    } else {
        Err("expected a 6-digit RGB hex value")
    }
}

/// OOXML `ST_HexColor` (§17.3.4.1): `"auto"` or 6-digit RGB hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HexColor {
    Auto,
    Rgb(u32),
}

impl HexColor {
    /// RGB value if concrete; `None` if `Auto`.
    pub fn rgb(self) -> Option<u32> {
        match self {
            HexColor::Auto => None,
            HexColor::Rgb(v) => Some(v),
        }
    }
}

impl From<HexColor> for Color {
    fn from(h: HexColor) -> Self {
        match h {
            HexColor::Auto => Self::Auto,
            HexColor::Rgb(v) => Self::Rgb(v),
        }
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.eq_ignore_ascii_case("auto") {
            return Ok(HexColor::Auto);
        }
        parse_rgb_hex6(&s)
            .map(HexColor::Rgb)
            .map_err(serde::de::Error::custom)
    }
}

/// OOXML `ST_HexColorRGB` (§20.1.10.41): strictly a 6-digit RGB hex.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RgbHexU32(pub u32);

impl<'de> Deserialize<'de> for RgbHexU32 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_rgb_hex6(&s)
            .map(RgbHexU32)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct HexVal {
        #[serde(rename = "@val")]
        val: HexColor,
    }

    #[derive(Deserialize)]
    struct RgbVal {
        #[serde(rename = "@val")]
        val: RgbHexU32,
    }

    #[test]
    fn hex_color_rgb() {
        let v: HexVal = quick_xml::de::from_str(r#"<x val="4F81BD"/>"#).unwrap();
        assert_eq!(v.val, HexColor::Rgb(0x4F81BD));
        assert_eq!(v.val.rgb(), Some(0x4F81BD));
    }

    #[test]
    fn hex_color_auto_is_case_insensitive() {
        for raw in ["auto", "AUTO", "Auto"] {
            let xml = format!(r#"<x val="{raw}"/>"#);
            let v: HexVal = quick_xml::de::from_str(&xml).unwrap();
            assert_eq!(v.val, HexColor::Auto);
            assert!(v.val.rgb().is_none());
        }
    }

    #[test]
    fn hex_color_rejects_garbage() {
        let r: Result<HexVal, _> = quick_xml::de::from_str(r#"<x val="notahex"/>"#);
        assert!(r.is_err());
    }

    #[test]
    fn rgb_hex_accepts_six_digit() {
        let v: RgbVal = quick_xml::de::from_str(r#"<x val="DEADBE"/>"#).unwrap();
        assert_eq!(v.val.0, 0xDEADBE);
    }

    #[test]
    fn rgb_hex_rejects_auto() {
        let r: Result<RgbVal, _> = quick_xml::de::from_str(r#"<x val="auto"/>"#);
        assert!(
            r.is_err(),
            "RgbHexU32 must reject 'auto' per ST_HexColorRGB"
        );
    }

    #[test]
    fn rgb_hex_rejects_garbage() {
        let r: Result<RgbVal, _> = quick_xml::de::from_str(r#"<x val="xyz123"/>"#);
        assert!(r.is_err());
    }

    #[test]
    fn hex_enforces_exactly_six_digits() {
        // ST_HexColor / ST_HexColorRGB are strictly 6-digit RGB. Shorthand,
        // 8-digit ARGB, and a signed value must be rejected, not silently
        // mis-decoded by from_str_radix (e.g. "FFF" -> 0x000FFF).
        for bad in ["FFF", "FFFFF", "FFFFFFF", "FFFFFFFF", "+F0F0F0", " F0F0F0"] {
            let xml = format!(r#"<x val="{bad}"/>"#);
            assert!(
                quick_xml::de::from_str::<HexVal>(&xml).is_err(),
                "HexColor must reject {bad:?}"
            );
            assert!(
                quick_xml::de::from_str::<RgbVal>(&xml).is_err(),
                "RgbHexU32 must reject {bad:?}"
            );
        }
        // The exact 6-digit form still parses.
        let v: RgbVal = quick_xml::de::from_str(r#"<x val="0A0B0C"/>"#).unwrap();
        assert_eq!(v.val.0, 0x0A0B0C);
    }
}
