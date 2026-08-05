//! VML color parsing.

use crate::docx::model::*;

/// Parse a VML color value (§14.1.2.1): `#RRGGBB`, `RRGGBB` hex, or named color.
///
/// Returns `None` for an unrecognized value; every call site treats a color as
/// optional (an unparseable `fillcolor` simply means "no color"), so this
/// mirrors the sibling `parse_style`/`parse_formula`/`parse_length` helpers
/// rather than raising a document-fatal error.
pub(super) fn parse_color(s: &str) -> Option<VmlColor> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        return Some(VmlColor::Rgb(r, g, b));
    }
    parse_named_color(s).map(VmlColor::Named)
}

fn parse_named_color(s: &str) -> Option<VmlNamedColor> {
    // Case-insensitive match per CSS spec.
    Some(match s.to_ascii_lowercase().as_str() {
        // CSS2.1 standard colors.
        "black" => VmlNamedColor::Black,
        "silver" => VmlNamedColor::Silver,
        "gray" | "grey" => VmlNamedColor::Gray,
        "white" => VmlNamedColor::White,
        "maroon" => VmlNamedColor::Maroon,
        "red" => VmlNamedColor::Red,
        "purple" => VmlNamedColor::Purple,
        "fuchsia" => VmlNamedColor::Fuchsia,
        "green" => VmlNamedColor::Green,
        "lime" => VmlNamedColor::Lime,
        "olive" => VmlNamedColor::Olive,
        "yellow" => VmlNamedColor::Yellow,
        "navy" => VmlNamedColor::Navy,
        "blue" => VmlNamedColor::Blue,
        "teal" => VmlNamedColor::Teal,
        "aqua" => VmlNamedColor::Aqua,
        "orange" => VmlNamedColor::Orange,
        // SVG/CSS3 extended colors.
        "aliceblue" => VmlNamedColor::AliceBlue,
        "antiquewhite" => VmlNamedColor::AntiqueWhite,
        "beige" => VmlNamedColor::Beige,
        "bisque" => VmlNamedColor::Bisque,
        "blanchedalmond" => VmlNamedColor::BlanchedAlmond,
        "blueviolet" => VmlNamedColor::BlueViolet,
        "brown" => VmlNamedColor::Brown,
        "burlywood" => VmlNamedColor::BurlyWood,
        "cadetblue" => VmlNamedColor::CadetBlue,
        "chartreuse" => VmlNamedColor::Chartreuse,
        "chocolate" => VmlNamedColor::Chocolate,
        "coral" => VmlNamedColor::Coral,
        "cornflowerblue" => VmlNamedColor::CornflowerBlue,
        "cornsilk" => VmlNamedColor::Cornsilk,
        "crimson" => VmlNamedColor::Crimson,
        "cyan" => VmlNamedColor::Cyan,
        "darkblue" => VmlNamedColor::DarkBlue,
        "darkcyan" => VmlNamedColor::DarkCyan,
        "darkgoldenrod" => VmlNamedColor::DarkGoldenrod,
        "darkgray" | "darkgrey" => VmlNamedColor::DarkGray,
        "darkgreen" => VmlNamedColor::DarkGreen,
        "darkkhaki" => VmlNamedColor::DarkKhaki,
        "darkmagenta" => VmlNamedColor::DarkMagenta,
        "darkolivegreen" => VmlNamedColor::DarkOliveGreen,
        "darkorange" => VmlNamedColor::DarkOrange,
        "darkorchid" => VmlNamedColor::DarkOrchid,
        "darkred" => VmlNamedColor::DarkRed,
        "darksalmon" => VmlNamedColor::DarkSalmon,
        "darkseagreen" => VmlNamedColor::DarkSeaGreen,
        "darkslateblue" => VmlNamedColor::DarkSlateBlue,
        "darkslategray" | "darkslategrey" => VmlNamedColor::DarkSlateGray,
        "darkturquoise" => VmlNamedColor::DarkTurquoise,
        "darkviolet" => VmlNamedColor::DarkViolet,
        "deeppink" => VmlNamedColor::DeepPink,
        "deepskyblue" => VmlNamedColor::DeepSkyBlue,
        "dimgray" | "dimgrey" => VmlNamedColor::DimGray,
        "dodgerblue" => VmlNamedColor::DodgerBlue,
        "firebrick" => VmlNamedColor::Firebrick,
        "floralwhite" => VmlNamedColor::FloralWhite,
        "forestgreen" => VmlNamedColor::ForestGreen,
        "gainsboro" => VmlNamedColor::Gainsboro,
        "ghostwhite" => VmlNamedColor::GhostWhite,
        "gold" => VmlNamedColor::Gold,
        "goldenrod" => VmlNamedColor::Goldenrod,
        "greenyellow" => VmlNamedColor::GreenYellow,
        "honeydew" => VmlNamedColor::Honeydew,
        "hotpink" => VmlNamedColor::HotPink,
        "indianred" => VmlNamedColor::IndianRed,
        "indigo" => VmlNamedColor::Indigo,
        "ivory" => VmlNamedColor::Ivory,
        "khaki" => VmlNamedColor::Khaki,
        "lavender" => VmlNamedColor::Lavender,
        "lavenderblush" => VmlNamedColor::LavenderBlush,
        "lawngreen" => VmlNamedColor::LawnGreen,
        "lemonchiffon" => VmlNamedColor::LemonChiffon,
        "lightblue" => VmlNamedColor::LightBlue,
        "lightcoral" => VmlNamedColor::LightCoral,
        "lightcyan" => VmlNamedColor::LightCyan,
        "lightgoldenrodyellow" => VmlNamedColor::LightGoldenrodYellow,
        "lightgray" | "lightgrey" => VmlNamedColor::LightGray,
        "lightgreen" => VmlNamedColor::LightGreen,
        "lightpink" => VmlNamedColor::LightPink,
        "lightsalmon" => VmlNamedColor::LightSalmon,
        "lightseagreen" => VmlNamedColor::LightSeaGreen,
        "lightskyblue" => VmlNamedColor::LightSkyBlue,
        "lightslategray" | "lightslategrey" => VmlNamedColor::LightSlateGray,
        "lightsteelblue" => VmlNamedColor::LightSteelBlue,
        "lightyellow" => VmlNamedColor::LightYellow,
        "limegreen" => VmlNamedColor::LimeGreen,
        "linen" => VmlNamedColor::Linen,
        "magenta" => VmlNamedColor::Magenta,
        "mediumaquamarine" => VmlNamedColor::MediumAquamarine,
        "mediumblue" => VmlNamedColor::MediumBlue,
        "mediumorchid" => VmlNamedColor::MediumOrchid,
        "mediumpurple" => VmlNamedColor::MediumPurple,
        "mediumseagreen" => VmlNamedColor::MediumSeaGreen,
        "mediumslateblue" => VmlNamedColor::MediumSlateBlue,
        "mediumspringgreen" => VmlNamedColor::MediumSpringGreen,
        "mediumturquoise" => VmlNamedColor::MediumTurquoise,
        "mediumvioletred" => VmlNamedColor::MediumVioletRed,
        "midnightblue" => VmlNamedColor::MidnightBlue,
        "mintcream" => VmlNamedColor::MintCream,
        "mistyrose" => VmlNamedColor::MistyRose,
        "moccasin" => VmlNamedColor::Moccasin,
        "navajowhite" => VmlNamedColor::NavajoWhite,
        "oldlace" => VmlNamedColor::OldLace,
        "olivedrab" => VmlNamedColor::OliveDrab,
        "orangered" => VmlNamedColor::OrangeRed,
        "orchid" => VmlNamedColor::Orchid,
        "palegoldenrod" => VmlNamedColor::PaleGoldenrod,
        "palegreen" => VmlNamedColor::PaleGreen,
        "paleturquoise" => VmlNamedColor::PaleTurquoise,
        "palevioletred" => VmlNamedColor::PaleVioletRed,
        "papayawhip" => VmlNamedColor::PapayaWhip,
        "peachpuff" => VmlNamedColor::PeachPuff,
        "peru" => VmlNamedColor::Peru,
        "pink" => VmlNamedColor::Pink,
        "plum" => VmlNamedColor::Plum,
        "powderblue" => VmlNamedColor::PowderBlue,
        "rosybrown" => VmlNamedColor::RosyBrown,
        "royalblue" => VmlNamedColor::RoyalBlue,
        "saddlebrown" => VmlNamedColor::SaddleBrown,
        "salmon" => VmlNamedColor::Salmon,
        "sandybrown" => VmlNamedColor::SandyBrown,
        "seagreen" => VmlNamedColor::SeaGreen,
        "seashell" => VmlNamedColor::Seashell,
        "sienna" => VmlNamedColor::Sienna,
        "skyblue" => VmlNamedColor::SkyBlue,
        "slateblue" => VmlNamedColor::SlateBlue,
        "slategray" | "slategrey" => VmlNamedColor::SlateGray,
        "snow" => VmlNamedColor::Snow,
        "springgreen" => VmlNamedColor::SpringGreen,
        "steelblue" => VmlNamedColor::SteelBlue,
        "tan" => VmlNamedColor::Tan,
        "thistle" => VmlNamedColor::Thistle,
        "tomato" => VmlNamedColor::Tomato,
        "turquoise" => VmlNamedColor::Turquoise,
        "violet" => VmlNamedColor::Violet,
        "wheat" => VmlNamedColor::Wheat,
        "whitesmoke" => VmlNamedColor::WhiteSmoke,
        "yellowgreen" => VmlNamedColor::YellowGreen,
        // VML system colors.
        "buttonface" => VmlNamedColor::ButtonFace,
        "buttonhighlight" => VmlNamedColor::ButtonHighlight,
        "buttonshadow" => VmlNamedColor::ButtonShadow,
        "buttontext" => VmlNamedColor::ButtonText,
        "captiontext" => VmlNamedColor::CaptionText,
        "graytext" => VmlNamedColor::GrayText,
        "highlight" => VmlNamedColor::Highlight,
        "highlighttext" => VmlNamedColor::HighlightText,
        "inactiveborder" => VmlNamedColor::InactiveBorder,
        "inactivecaption" => VmlNamedColor::InactiveCaption,
        "inactivecaptiontext" => VmlNamedColor::InactiveCaptionText,
        "infobackground" => VmlNamedColor::InfoBackground,
        "infotext" => VmlNamedColor::InfoText,
        "menu" => VmlNamedColor::Menu,
        "menutext" => VmlNamedColor::MenuText,
        "scrollbar" => VmlNamedColor::Scrollbar,
        "threeddarkshadow" => VmlNamedColor::ThreeDDarkShadow,
        "threedface" => VmlNamedColor::ThreeDFace,
        "threedhighlight" => VmlNamedColor::ThreeDHighlight,
        "threedlightshadow" => VmlNamedColor::ThreeDLightShadow,
        "threedshadow" => VmlNamedColor::ThreeDShadow,
        "window" => VmlNamedColor::Window,
        "windowframe" => VmlNamedColor::WindowFrame,
        "windowtext" => VmlNamedColor::WindowText,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_with_hash() {
        assert_eq!(
            parse_color("#4F81BD"),
            Some(VmlColor::Rgb(0x4F, 0x81, 0xBD))
        );
    }

    #[test]
    fn hex_without_hash() {
        assert_eq!(parse_color("FF0000"), Some(VmlColor::Rgb(255, 0, 0)));
    }

    #[test]
    fn hex_is_case_insensitive() {
        assert_eq!(
            parse_color("#abcdef"),
            Some(VmlColor::Rgb(0xAB, 0xCD, 0xEF))
        );
    }

    #[test]
    fn named_color_basic() {
        assert_eq!(
            parse_color("red"),
            Some(VmlColor::Named(VmlNamedColor::Red))
        );
    }

    #[test]
    fn named_color_is_case_insensitive() {
        // §14.1.2.1 matches CSS color names case-insensitively.
        assert_eq!(
            parse_color("RED"),
            Some(VmlColor::Named(VmlNamedColor::Red))
        );
        assert_eq!(
            parse_color("Red"),
            Some(VmlColor::Named(VmlNamedColor::Red))
        );
    }

    #[test]
    fn gray_grey_spelling_aliases() {
        let gray = Some(VmlColor::Named(VmlNamedColor::Gray));
        assert_eq!(parse_color("gray"), gray);
        assert_eq!(parse_color("grey"), gray);
        let dark = Some(VmlColor::Named(VmlNamedColor::DarkGray));
        assert_eq!(parse_color("darkgray"), dark);
        assert_eq!(parse_color("darkgrey"), dark);
    }

    #[test]
    fn vml_system_color() {
        assert_eq!(
            parse_color("threeddarkshadow"),
            Some(VmlColor::Named(VmlNamedColor::ThreeDDarkShadow))
        );
    }

    #[test]
    fn unrecognized_name_is_none() {
        // Callers `.and_then` this, so an unknown color degrades to "no color".
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn three_digit_hex_is_not_six_digit_and_falls_through_to_none() {
        // Not 6 hex digits → not treated as RGB; "fff" is not a named color.
        assert_eq!(parse_color("#fff"), None);
    }

    #[test]
    fn six_non_hex_chars_are_not_rgb() {
        // Length 6 but not all hex digits → named lookup (which also fails).
        assert_eq!(parse_color("gggggg"), None);
    }
}
