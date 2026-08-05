//! Border sub-schemas (§17.3.4 pBdr, §17.4.39 tblBorders, §17.4.66 tcBorders).
//!
//! `BorderXml` matches a single `<w:top>`/`<w:bottom>`/etc. element. The
//! container structs (paragraph / table / table-cell borders) share the same
//! inner `BorderXml` but differ in which sides are allowed. Each container
//! accepts both modern (`start`/`end`) and legacy (`left`/`right`) side
//! names per OOXML bidi handling.

use serde::Deserialize;

use crate::docx::model::dimension::{Dimension, EighthPoints, Points};
use crate::docx::model::{Border, Color, ParagraphBorders, TableBorders, TableCellBorders};
use crate::docx::parse::primitives::HexColor;
use crate::docx::parse::primitives::st_enums::StBorderType;
use crate::docx::parse::primitives::units::deserialize_optional_nonnegative_dimension;

/// A single `<w:top w:val="..." w:sz="..." w:space="..." w:color="..."/>` etc.
#[derive(Clone, Copy, Debug, Deserialize)]
pub(crate) struct BorderXml {
    #[serde(
        rename = "@val",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    val: StBorderType,
    #[serde(
        rename = "@sz",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    sz: Option<Dimension<EighthPoints>>,
    #[serde(
        rename = "@space",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    space: Option<Dimension<Points>>,
    #[serde(
        rename = "@color",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    color: Option<HexColor>,
}

impl From<BorderXml> for Border {
    fn from(x: BorderXml) -> Self {
        Self {
            style: x.val.into(),
            width: x.sz.unwrap_or_default(),
            space: x.space.unwrap_or_default(),
            color: x.color.map_or(Color::Auto, Into::into),
        }
    }
}

/// `<w:pBdr>` — five sides plus `between`.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub(crate) struct ParagraphBordersXml {
    #[serde(default)]
    top: Option<BorderXml>,
    #[serde(default)]
    bottom: Option<BorderXml>,
    #[serde(default, alias = "start")]
    left: Option<BorderXml>,
    #[serde(default, alias = "end")]
    right: Option<BorderXml>,
    #[serde(default)]
    between: Option<BorderXml>,
}

impl From<ParagraphBordersXml> for ParagraphBorders {
    fn from(x: ParagraphBordersXml) -> Self {
        Self {
            top: x.top.map(Into::into),
            bottom: x.bottom.map(Into::into),
            left: x.left.map(Into::into),
            right: x.right.map(Into::into),
            between: x.between.map(Into::into),
        }
    }
}

/// `<w:tblBorders>` — six sides (adds `insideH`, `insideV`).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub(crate) struct TableBordersXml {
    #[serde(default)]
    top: Option<BorderXml>,
    #[serde(default)]
    bottom: Option<BorderXml>,
    #[serde(default, alias = "start")]
    left: Option<BorderXml>,
    #[serde(default, alias = "end")]
    right: Option<BorderXml>,
    #[serde(rename = "insideH", default)]
    inside_h: Option<BorderXml>,
    #[serde(rename = "insideV", default)]
    inside_v: Option<BorderXml>,
}

impl From<TableBordersXml> for TableBorders {
    fn from(x: TableBordersXml) -> Self {
        Self {
            top: x.top.map(Into::into),
            bottom: x.bottom.map(Into::into),
            left: x.left.map(Into::into),
            right: x.right.map(Into::into),
            inside_h: x.inside_h.map(Into::into),
            inside_v: x.inside_v.map(Into::into),
        }
    }
}

/// `<w:tcBorders>` — eight sides (adds diagonal `tl2br`, `tr2bl`).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub(crate) struct TableCellBordersXml {
    #[serde(default)]
    top: Option<BorderXml>,
    #[serde(default)]
    bottom: Option<BorderXml>,
    #[serde(default, alias = "start")]
    left: Option<BorderXml>,
    #[serde(default, alias = "end")]
    right: Option<BorderXml>,
    #[serde(rename = "insideH", default)]
    inside_h: Option<BorderXml>,
    #[serde(rename = "insideV", default)]
    inside_v: Option<BorderXml>,
    #[serde(rename = "tl2br", default)]
    tl2br: Option<BorderXml>,
    #[serde(rename = "tr2bl", default)]
    tr2bl: Option<BorderXml>,
}

impl From<TableCellBordersXml> for TableCellBorders {
    fn from(x: TableCellBordersXml) -> Self {
        Self {
            top: x.top.map(Into::into),
            bottom: x.bottom.map(Into::into),
            left: x.left.map(Into::into),
            right: x.right.map(Into::into),
            inside_h: x.inside_h.map(Into::into),
            inside_v: x.inside_v.map(Into::into),
            tl2br: x.tl2br.map(Into::into),
            tr2bl: x.tr2bl.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_border_with_all_attrs() {
        let xml = r#"<top val="single" sz="4" space="0" color="FF0000"/>"#;
        let b: BorderXml = quick_xml::de::from_str(xml).unwrap();
        let m: Border = b.into();
        assert_eq!(m.style, crate::docx::model::BorderStyle::Single);
        assert_eq!(m.width.raw(), 4);
        assert_eq!(m.space.raw(), 0);
        assert_eq!(m.color, Color::Rgb(0xFF0000));
    }

    #[test]
    fn border_with_auto_color() {
        let xml = r#"<top val="single" color="auto"/>"#;
        let b: BorderXml = quick_xml::de::from_str(xml).unwrap();
        let m: Border = b.into();
        assert_eq!(m.color, Color::Auto);
    }

    /// §17.18.2 ST_Border: `nil` and `none` both mean "no border", but they
    /// are **not** merged. [MS-OI29500] §17.4.66 separates them in table border
    /// conflict resolution — `nil` suppresses the shared edge, `none` inherits
    /// and yields — so the distinction has to survive parsing. The cascade sees
    /// `Some(Border)` either way, so an explicit child can still override an
    /// inherited side.
    #[test]
    fn border_val_nil_and_none_stay_distinct() {
        let nil: Border = quick_xml::de::from_str::<BorderXml>(r#"<top val="nil"/>"#)
            .unwrap()
            .into();
        let none: Border = quick_xml::de::from_str::<BorderXml>(r#"<top val="none"/>"#)
            .unwrap()
            .into();
        assert_eq!(nil.style, crate::docx::model::BorderStyle::Nil);
        assert_eq!(none.style, crate::docx::model::BorderStyle::None);
        assert_ne!(nil.style, none.style, "merging these loses §17.4.66");
        // Both still paint nothing — only conflict resolution tells them apart.
        assert!(nil.style.draws_nothing() && none.style.draws_nothing());
    }

    #[test]
    fn paragraph_borders_all_nil_preserves_some_per_side() {
        // Word emits this in pPrDefault. Each side must round-trip as
        // `Some(Border { style: Nil })`, not as the parent `pBdr` being
        // None — the merge cascade needs the explicit override.
        let xml = r#"<pBdr>
            <top val="nil"/>
            <left val="nil"/>
            <bottom val="nil"/>
            <right val="nil"/>
            <between val="nil"/>
            <bar val="nil"/>
        </pBdr>"#;
        let px: ParagraphBordersXml = quick_xml::de::from_str(xml).unwrap();
        let p: ParagraphBorders = px.into();
        assert!(p.top.is_some());
        assert_eq!(p.top.unwrap().style, crate::docx::model::BorderStyle::Nil);
        assert!(p.bottom.is_some());
        assert!(p.left.is_some());
        assert!(p.right.is_some());
    }

    #[test]
    fn paragraph_borders_aliases_start_left() {
        let xml = r#"<pBdr>
            <top val="single"/>
            <start val="double"/>
            <end val="thick"/>
        </pBdr>"#;
        let px: ParagraphBordersXml = quick_xml::de::from_str(xml).unwrap();
        let p: ParagraphBorders = px.into();
        assert!(p.top.is_some());
        assert_eq!(
            p.left.unwrap().style,
            crate::docx::model::BorderStyle::Double
        );
        assert_eq!(
            p.right.unwrap().style,
            crate::docx::model::BorderStyle::Thick
        );
    }

    #[test]
    fn table_borders_inside_pair() {
        let xml = r#"<tblBorders>
            <insideH val="single"/>
            <insideV val="dashed"/>
        </tblBorders>"#;
        let tx: TableBordersXml = quick_xml::de::from_str(xml).unwrap();
        let t: TableBorders = tx.into();
        assert!(t.inside_h.is_some());
        assert!(t.inside_v.is_some());
        assert!(t.top.is_none());
    }

    #[test]
    fn cell_borders_diagonals() {
        let xml = r#"<tcBorders>
            <tl2br val="single"/>
            <tr2bl val="dotted"/>
        </tcBorders>"#;
        let cx: TableCellBordersXml = quick_xml::de::from_str(xml).unwrap();
        let c: TableCellBorders = cx.into();
        assert!(c.tl2br.is_some());
        assert!(c.tr2bl.is_some());
    }
}
