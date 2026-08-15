//! VML CSS style and length parsing.

use crate::docx::model::*;

/// Parse a VML `style` attribute — semicolon-separated CSS2 properties.
pub(super) fn parse_style(s: Option<String>) -> VmlStyle {
    let s = match s {
        Some(s) => s,
        None => return VmlStyle::default(),
    };

    let mut style = VmlStyle::default();

    for decl in s.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((key, val)) = decl.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();

        match key {
            "position" => {
                style.position = match val {
                    "static" => Some(CssPosition::Static),
                    "relative" => Some(CssPosition::Relative),
                    "absolute" => Some(CssPosition::Absolute),
                    _ => {
                        log::warn!("vml-style: unsupported position value {:?}", val);
                        None
                    }
                };
            }
            "left" => style.left = parse_length(val),
            "top" => style.top = parse_length(val),
            "width" => style.width = parse_length(val),
            "height" => style.height = parse_length(val),
            "margin-left" => style.margin_left = parse_length(val),
            "margin-top" => style.margin_top = parse_length(val),
            "margin-right" => style.margin_right = parse_length(val),
            "margin-bottom" => style.margin_bottom = parse_length(val),
            "z-index" => style.z_index = val.parse::<i64>().ok(),
            "rotation" => style.rotation = val.parse::<f64>().ok(),
            "flip" => {
                style.flip = match val {
                    "x" => Some(VmlFlip::X),
                    "y" => Some(VmlFlip::Y),
                    "xy" | "yx" => Some(VmlFlip::XY),
                    _ => {
                        log::warn!("vml-style: unsupported flip value {:?}", val);
                        None
                    }
                };
            }
            "visibility" => {
                style.visibility = match val {
                    "visible" => Some(CssVisibility::Visible),
                    "hidden" => Some(CssVisibility::Hidden),
                    "inherit" => Some(CssVisibility::Inherit),
                    _ => {
                        log::warn!("vml-style: unsupported visibility value {:?}", val);
                        None
                    }
                };
            }
            "mso-position-horizontal" => {
                style.mso_position_horizontal = match val {
                    "absolute" => Some(MsoPositionH::Absolute),
                    "left" => Some(MsoPositionH::Left),
                    "center" => Some(MsoPositionH::Center),
                    "right" => Some(MsoPositionH::Right),
                    "inside" => Some(MsoPositionH::Inside),
                    "outside" => Some(MsoPositionH::Outside),
                    _ => {
                        log::warn!(
                            "vml-style: unsupported mso-position-horizontal value {:?}",
                            val
                        );
                        None
                    }
                };
            }
            "mso-position-horizontal-relative" => {
                style.mso_position_horizontal_relative = match val {
                    "margin" => Some(MsoPositionHRelative::Margin),
                    "page" => Some(MsoPositionHRelative::Page),
                    "text" => Some(MsoPositionHRelative::Text),
                    "char" => Some(MsoPositionHRelative::Char),
                    "left-margin-area" => Some(MsoPositionHRelative::LeftMarginArea),
                    "right-margin-area" => Some(MsoPositionHRelative::RightMarginArea),
                    "inner-margin-area" => Some(MsoPositionHRelative::InnerMarginArea),
                    "outer-margin-area" => Some(MsoPositionHRelative::OuterMarginArea),
                    _ => {
                        log::warn!(
                            "vml-style: unsupported mso-position-horizontal-relative value {:?}",
                            val
                        );
                        None
                    }
                };
            }
            "mso-position-vertical" => {
                style.mso_position_vertical = match val {
                    "absolute" => Some(MsoPositionV::Absolute),
                    "top" => Some(MsoPositionV::Top),
                    "center" => Some(MsoPositionV::Center),
                    "bottom" => Some(MsoPositionV::Bottom),
                    "inside" => Some(MsoPositionV::Inside),
                    "outside" => Some(MsoPositionV::Outside),
                    _ => {
                        log::warn!(
                            "vml-style: unsupported mso-position-vertical value {:?}",
                            val
                        );
                        None
                    }
                };
            }
            "mso-position-vertical-relative" => {
                style.mso_position_vertical_relative = match val {
                    "margin" => Some(MsoPositionVRelative::Margin),
                    "page" => Some(MsoPositionVRelative::Page),
                    "text" => Some(MsoPositionVRelative::Text),
                    "line" => Some(MsoPositionVRelative::Line),
                    "top-margin-area" => Some(MsoPositionVRelative::TopMarginArea),
                    "bottom-margin-area" => Some(MsoPositionVRelative::BottomMarginArea),
                    "inner-margin-area" => Some(MsoPositionVRelative::InnerMarginArea),
                    "outer-margin-area" => Some(MsoPositionVRelative::OuterMarginArea),
                    _ => {
                        log::warn!(
                            "vml-style: unsupported mso-position-vertical-relative value {:?}",
                            val
                        );
                        None
                    }
                };
            }
            "mso-wrap-distance-left" => {
                style.mso_wrap_distance_left = parse_length(val);
            }
            "mso-wrap-distance-right" => {
                style.mso_wrap_distance_right = parse_length(val);
            }
            "mso-wrap-distance-top" => {
                style.mso_wrap_distance_top = parse_length(val);
            }
            "mso-wrap-distance-bottom" => {
                style.mso_wrap_distance_bottom = parse_length(val);
            }
            "mso-wrap-style" => {
                style.mso_wrap_style = match val {
                    "square" => Some(MsoWrapStyle::Square),
                    "none" => Some(MsoWrapStyle::None),
                    "tight" => Some(MsoWrapStyle::Tight),
                    "through" => Some(MsoWrapStyle::Through),
                    _ => {
                        log::warn!("vml-style: unsupported mso-wrap-style value {:?}", val);
                        None
                    }
                };
            }
            _ => log::warn!("vml-style: unsupported property {:?}: {:?}", key, val),
        }
    }

    style
}

/// Parse a CSS length value (e.g., "468pt", "0", "3.5in", "50%").
pub(super) fn parse_length(s: &str) -> Option<VmlLength> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Try known unit suffixes.
    let (num_str, unit) = if let Some(n) = s.strip_suffix("pt") {
        (n, VmlLengthUnit::Pt)
    } else if let Some(n) = s.strip_suffix("in") {
        (n, VmlLengthUnit::In)
    } else if let Some(n) = s.strip_suffix("cm") {
        (n, VmlLengthUnit::Cm)
    } else if let Some(n) = s.strip_suffix("mm") {
        (n, VmlLengthUnit::Mm)
    } else if let Some(n) = s.strip_suffix("px") {
        (n, VmlLengthUnit::Px)
    } else if let Some(n) = s.strip_suffix("em") {
        (n, VmlLengthUnit::Em)
    } else if let Some(n) = s.strip_suffix('%') {
        (n, VmlLengthUnit::Percent)
    } else {
        // Find where the numeric part ends and the suffix begins.
        let split = s
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
            .unwrap_or(s.len());
        if split < s.len() {
            log::warn!("vml-length: unsupported unit suffix {:?}", &s[split..]);
            return None;
        }
        (s, VmlLengthUnit::None)
    };

    let value = num_str.trim().parse::<f64>().ok()?;
    Some(VmlLength { value, unit })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_yields_default() {
        let s = parse_style(None);
        assert!(s.position.is_none());
        assert!(s.left.is_none());
    }

    #[test]
    fn position_and_dimensions() {
        let s = parse_style(Some(
            "position:absolute;left:10pt;top:20pt;width:100pt;height:50pt".into(),
        ));
        assert_eq!(s.position, Some(CssPosition::Absolute));
        assert_eq!(
            s.left,
            Some(VmlLength {
                value: 10.0,
                unit: VmlLengthUnit::Pt
            })
        );
        assert_eq!(
            s.width,
            Some(VmlLength {
                value: 100.0,
                unit: VmlLengthUnit::Pt
            })
        );
        assert_eq!(
            s.height,
            Some(VmlLength {
                value: 50.0,
                unit: VmlLengthUnit::Pt
            })
        );
    }

    #[test]
    fn flip_and_visibility() {
        let s = parse_style(Some("flip:xy;visibility:hidden".into()));
        assert_eq!(s.flip, Some(VmlFlip::XY));
        assert_eq!(s.visibility, Some(CssVisibility::Hidden));
    }

    #[test]
    fn mso_positioning() {
        let s = parse_style(Some(
            "mso-position-horizontal:center;mso-position-vertical-relative:line".into(),
        ));
        assert_eq!(s.mso_position_horizontal, Some(MsoPositionH::Center));
        assert_eq!(
            s.mso_position_vertical_relative,
            Some(MsoPositionVRelative::Line)
        );
    }

    #[test]
    fn z_index_and_rotation() {
        let s = parse_style(Some("z-index:5;rotation:45".into()));
        assert_eq!(s.z_index, Some(5));
        assert_eq!(s.rotation, Some(45.0));
    }

    #[test]
    fn whitespace_and_unknown_props_are_ignored() {
        // Extra whitespace, a trailing empty declaration, and an unrecognized
        // property must not derail the recognized ones.
        let s = parse_style(Some("  left : 3pt ; mso-bogus:1 ;".into()));
        assert_eq!(
            s.left,
            Some(VmlLength {
                value: 3.0,
                unit: VmlLengthUnit::Pt
            })
        );
    }

    #[test]
    fn length_units() {
        assert_eq!(
            parse_length("468pt"),
            Some(VmlLength {
                value: 468.0,
                unit: VmlLengthUnit::Pt
            })
        );
        assert_eq!(
            parse_length("3.5in"),
            Some(VmlLength {
                value: 3.5,
                unit: VmlLengthUnit::In
            })
        );
        assert_eq!(
            parse_length("50%"),
            Some(VmlLength {
                value: 50.0,
                unit: VmlLengthUnit::Percent
            })
        );
        assert_eq!(
            parse_length("-10pt"),
            Some(VmlLength {
                value: -10.0,
                unit: VmlLengthUnit::Pt
            })
        );
    }

    #[test]
    fn unitless_length() {
        assert_eq!(
            parse_length("0"),
            Some(VmlLength {
                value: 0.0,
                unit: VmlLengthUnit::None
            })
        );
    }

    #[test]
    fn empty_and_unsupported_unit_are_none() {
        assert!(parse_length("").is_none());
        assert!(parse_length("   ").is_none());
        // Unknown unit suffix → warn + None (rather than a wrong value).
        assert!(parse_length("10xyz").is_none());
        assert!(parse_length("auto").is_none());
    }
}
