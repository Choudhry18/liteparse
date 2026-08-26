//! Parser for `word/styles.xml` — parses style definitions as-is.
//! No inheritance resolution — `basedOn` references are preserved.

use serde::Deserialize;

use crate::docx::error::Result;
use crate::docx::model::*;
use crate::docx::parse::properties::schema::paragraph::PPrXml;
use crate::docx::parse::properties::schema::run::RPrXml;
use crate::docx::parse::properties::schema::table::{TblPrXml, TcPrXml, TrPrXml};
use crate::docx::parse::serde_xml::from_xml;

/// Parse `word/styles.xml` into a raw `StyleSheet`.
pub fn parse_styles(data: &[u8]) -> Result<StyleSheet> {
    if data.is_empty() {
        return Ok(StyleSheet::default());
    }
    from_xml::<StylesXml>(data).map(Into::into)
}

#[derive(Deserialize, Default)]
struct StylesXml {
    #[serde(rename = "docDefaults", default)]
    doc_defaults: Option<DocDefaultsXml>,
    #[serde(rename = "latentStyles", default)]
    latent_styles: Option<LatentStylesXml>,
    #[serde(rename = "style", default)]
    styles: Vec<StyleXml>,
}

#[derive(Deserialize)]
struct DocDefaultsXml {
    #[serde(rename = "rPrDefault", default)]
    r_pr_default: Option<RPrDefaultXml>,
    #[serde(rename = "pPrDefault", default)]
    p_pr_default: Option<PPrDefaultXml>,
}

#[derive(Deserialize)]
struct RPrDefaultXml {
    #[serde(rename = "rPr", default)]
    r_pr: Option<RPrXml>,
}

#[derive(Deserialize)]
struct PPrDefaultXml {
    #[serde(rename = "pPr", default)]
    p_pr: Option<PPrXml>,
}

#[derive(Deserialize)]
struct StyleXml {
    /// `@w:type` — style kind. Absent defaults to `paragraph` per §17.7.
    #[serde(
        rename = "@type",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    ty: StStyleType,
    #[serde(rename = "@styleId", default)]
    style_id: Option<String>,
    #[serde(rename = "@default", default)]
    default: Option<AttrBool>,

    #[serde(rename = "name", default)]
    name: Option<ValString>,
    #[serde(rename = "basedOn", default)]
    based_on: Option<ValString>,
    #[serde(rename = "pPr", default)]
    p_pr: Option<PPrXml>,
    #[serde(rename = "rPr", default)]
    r_pr: Option<RPrXml>,
    #[serde(rename = "tblPr", default)]
    tbl_pr: Option<TblPrXml>,
    #[serde(rename = "tblStylePr", default)]
    tbl_style_pr: Vec<TblStylePrXml>,
}

#[derive(Deserialize)]
struct TblStylePrXml {
    #[serde(
        rename = "@type",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    ty: Option<StTblStylePrType>,
    #[serde(rename = "pPr", default)]
    p_pr: Option<PPrXml>,
    #[serde(rename = "rPr", default)]
    r_pr: Option<RPrXml>,
    #[serde(rename = "tblPr", default)]
    tbl_pr: Option<TblPrXml>,
    #[serde(rename = "trPr", default)]
    tr_pr: Option<TrPrXml>,
    #[serde(rename = "tcPr", default)]
    tc_pr: Option<TcPrXml>,
}

#[derive(Deserialize)]
struct LatentStylesXml {
    #[serde(rename = "@defLockedState", default)]
    def_locked_state: Option<AttrBool>,
    #[serde(
        rename = "@defUIPriority",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    def_ui_priority: Option<u32>,
    #[serde(rename = "@defSemiHidden", default)]
    def_semi_hidden: Option<AttrBool>,
    #[serde(rename = "@defUnhideWhenUsed", default)]
    def_unhide_when_used: Option<AttrBool>,
    #[serde(rename = "@defQFormat", default)]
    def_q_format: Option<AttrBool>,
    #[serde(
        rename = "@count",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    count: Option<u32>,
    #[serde(rename = "lsdException", default)]
    exceptions: Vec<LsdExceptionXml>,
}

#[derive(Deserialize)]
struct LsdExceptionXml {
    #[serde(rename = "@name", default)]
    name: Option<String>,
    #[serde(rename = "@locked", default)]
    locked: Option<AttrBool>,
    #[serde(
        rename = "@uiPriority",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    ui_priority: Option<u32>,
    #[serde(rename = "@semiHidden", default)]
    semi_hidden: Option<AttrBool>,
    #[serde(rename = "@unhideWhenUsed", default)]
    unhide_when_used: Option<AttrBool>,
    #[serde(rename = "@qFormat", default)]
    q_format: Option<AttrBool>,
}

// ── ST enums local to styles ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
enum StStyleType {
    #[default]
    Paragraph,
    Character,
    Table,
    Numbering,
}

impl From<StStyleType> for StyleType {
    fn from(s: StStyleType) -> Self {
        match s {
            StStyleType::Paragraph => Self::Paragraph,
            StStyleType::Character => Self::Character,
            StStyleType::Table => Self::Table,
            StStyleType::Numbering => Self::Numbering,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StTblStylePrType {
    FirstRow,
    LastRow,
    FirstCol,
    LastCol,
    Band1Vert,
    Band2Vert,
    Band1Horz,
    Band2Horz,
    NeCell,
    NwCell,
    SeCell,
    SwCell,
    WholeTable,
}

impl From<StTblStylePrType> for TableStyleOverrideType {
    fn from(s: StTblStylePrType) -> Self {
        match s {
            StTblStylePrType::FirstRow => Self::FirstRow,
            StTblStylePrType::LastRow => Self::LastRow,
            StTblStylePrType::FirstCol => Self::FirstCol,
            StTblStylePrType::LastCol => Self::LastCol,
            StTblStylePrType::Band1Vert => Self::Band1Vert,
            StTblStylePrType::Band2Vert => Self::Band2Vert,
            StTblStylePrType::Band1Horz => Self::Band1Horz,
            StTblStylePrType::Band2Horz => Self::Band2Horz,
            StTblStylePrType::NeCell => Self::NeCell,
            StTblStylePrType::NwCell => Self::NwCell,
            StTblStylePrType::SeCell => Self::SeCell,
            StTblStylePrType::SwCell => Self::SwCell,
            StTblStylePrType::WholeTable => Self::WholeTable,
        }
    }
}

// ── shared helpers ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ValString {
    #[serde(rename = "@val")]
    val: String,
}

use crate::docx::parse::primitives::AttrBool;

// ── schema → model conversion ────────────────────────────────────────────

impl From<StylesXml> for StyleSheet {
    fn from(x: StylesXml) -> Self {
        let mut sheet = StyleSheet::default();

        if let Some(dd) = x.doc_defaults {
            if let Some(r) = dd.r_pr_default.and_then(|d| d.r_pr) {
                let (rp, _) = r.split();
                sheet.doc_defaults_run = rp;
            }
            if let Some(p) = dd.p_pr_default.and_then(|d| d.p_pr) {
                sheet.doc_defaults_paragraph = p.split().properties;
            }
        }

        for s in x.styles {
            if let Some((id, style)) = convert_style(s) {
                sheet.styles.insert(id, style);
            }
        }

        sheet.latent_styles = x.latent_styles.map(Into::into);
        sheet
    }
}

fn convert_style(s: StyleXml) -> Option<(StyleId, Style)> {
    let id = StyleId::new(s.style_id?);
    let style_type = StyleType::from(s.ty);
    let is_default = s.default.map(|b| b.0).unwrap_or(false);

    let (mut paragraph_properties, mut run_properties_from_ppr): (
        Option<ParagraphProperties>,
        Option<RunProperties>,
    ) = (None, None);
    if let Some(p) = s.p_pr {
        let parsed = p.split();
        paragraph_properties = Some(parsed.properties);
        run_properties_from_ppr = parsed.run_properties;
    }

    let run_properties = s.r_pr.map(|r| r.split().0).or(run_properties_from_ppr);

    let table_properties = s.tbl_pr.map(|t| t.split().0);

    let table_style_overrides = s
        .tbl_style_pr
        .into_iter()
        .filter_map(convert_override)
        .collect();

    Some((
        id,
        Style {
            name: s.name.map(|v| v.val),
            style_type,
            based_on: s.based_on.map(|v| StyleId::new(v.val)),
            is_default,
            paragraph_properties,
            run_properties,
            table_properties,
            table_style_overrides,
        },
    ))
}

/// An unrecognized `@type` leaves no region for the override to apply to, so
/// the whole `tblStylePr` entry is dropped rather than misapplied.
fn convert_override(x: TblStylePrXml) -> Option<TableStyleOverride> {
    Some(TableStyleOverride {
        override_type: x.ty?.into(),
        paragraph_properties: x.p_pr.map(|p| p.split().properties),
        run_properties: x.r_pr.map(|r| r.split().0),
        table_properties: x.tbl_pr.map(|t| t.split().0),
        table_row_properties: x.tr_pr.map(Into::into),
        table_cell_properties: x.tc_pr.map(Into::into),
    })
}

impl From<LatentStylesXml> for LatentStyles {
    fn from(x: LatentStylesXml) -> Self {
        Self {
            default_locked_state: x.def_locked_state.map(|b| b.0),
            default_ui_priority: x.def_ui_priority,
            default_semi_hidden: x.def_semi_hidden.map(|b| b.0),
            default_unhide_when_used: x.def_unhide_when_used.map(|b| b.0),
            default_q_format: x.def_q_format.map(|b| b.0),
            count: x.count,
            exceptions: x.exceptions.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<LsdExceptionXml> for LatentStyleException {
    fn from(x: LsdExceptionXml) -> Self {
        Self {
            name: x.name,
            locked: x.locked.map(|b| b.0),
            ui_priority: x.ui_priority,
            semi_hidden: x.semi_hidden.map(|b| b.0),
            unhide_when_used: x.unhide_when_used.map(|b| b.0),
            q_format: x.q_format.map(|b| b.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn sid(s: &str) -> StyleId {
        StyleId::new(s)
    }

    fn parse(inner: &str) -> StyleSheet {
        let xml = format!("<w:styles {NS}>{inner}</w:styles>");
        parse_styles(xml.as_bytes()).expect("styles must parse")
    }

    #[test]
    fn empty_input_returns_default_stylesheet() {
        let sheet = parse_styles(b"").unwrap();
        assert!(sheet.styles.is_empty());
        assert!(sheet.latent_styles.is_none());
    }

    #[test]
    fn style_type_defaults_to_paragraph_and_maps_all_kinds() {
        let sheet = parse(
            r#"<w:style w:styleId="NoType"><w:name w:val="No Type"/></w:style>
               <w:style w:type="character" w:styleId="Char"/>
               <w:style w:type="table" w:styleId="Tbl"/>
               <w:style w:type="numbering" w:styleId="Num"/>"#,
        );
        assert_eq!(
            sheet.styles[&sid("NoType")].style_type,
            StyleType::Paragraph
        );
        assert_eq!(sheet.styles[&sid("Char")].style_type, StyleType::Character);
        assert_eq!(sheet.styles[&sid("Tbl")].style_type, StyleType::Table);
        assert_eq!(sheet.styles[&sid("Num")].style_type, StyleType::Numbering);
    }

    #[test]
    fn name_based_on_and_default_flag_preserved() {
        let sheet = parse(
            r#"<w:style w:type="paragraph" w:styleId="Heading1" w:default="1">
                 <w:name w:val="heading 1"/>
                 <w:basedOn w:val="Normal"/>
               </w:style>"#,
        );
        let s = &sheet.styles[&sid("Heading1")];
        assert_eq!(s.name.as_deref(), Some("heading 1"));
        assert_eq!(s.based_on, Some(sid("Normal")));
        assert!(s.is_default);
    }

    #[test]
    fn style_without_style_id_is_dropped() {
        // styleId is required; a style missing it can't be referenced, so it's
        // dropped rather than aborting the whole styles.xml parse.
        let sheet = parse(
            r#"<w:style w:type="paragraph"><w:name w:val="Anon"/></w:style>
               <w:style w:type="paragraph" w:styleId="Kept"/>"#,
        );
        assert_eq!(sheet.styles.len(), 1);
        assert!(sheet.styles.contains_key(&sid("Kept")));
    }

    #[test]
    fn doc_defaults_run_and_paragraph_extracted() {
        let sheet = parse(
            r#"<w:docDefaults>
                 <w:rPrDefault><w:rPr><w:b/></w:rPr></w:rPrDefault>
                 <w:pPrDefault><w:pPr><w:jc w:val="center"/></w:pPr></w:pPrDefault>
               </w:docDefaults>"#,
        );
        assert_eq!(sheet.doc_defaults_run.bold, Some(true));
        assert_eq!(
            sheet.doc_defaults_paragraph.alignment,
            Some(Alignment::Center)
        );
    }

    #[test]
    fn all_table_style_override_types_map() {
        let names = [
            "firstRow",
            "lastRow",
            "firstCol",
            "lastCol",
            "band1Vert",
            "band2Vert",
            "band1Horz",
            "band2Horz",
            "neCell",
            "nwCell",
            "seCell",
            "swCell",
            "wholeTable",
        ];
        let inner: String = names
            .iter()
            .map(|t| format!(r#"<w:tblStylePr w:type="{t}"><w:rPr><w:b/></w:rPr></w:tblStylePr>"#))
            .collect();
        let sheet = parse(&format!(
            r#"<w:style w:type="table" w:styleId="TblStyle">{inner}</w:style>"#
        ));
        let got: Vec<TableStyleOverrideType> = sheet.styles[&sid("TblStyle")]
            .table_style_overrides
            .iter()
            .map(|o| o.override_type)
            .collect();
        assert_eq!(got.len(), 13);
        for expected in [
            TableStyleOverrideType::FirstRow,
            TableStyleOverrideType::LastRow,
            TableStyleOverrideType::FirstCol,
            TableStyleOverrideType::LastCol,
            TableStyleOverrideType::Band1Vert,
            TableStyleOverrideType::Band2Vert,
            TableStyleOverrideType::Band1Horz,
            TableStyleOverrideType::Band2Horz,
            TableStyleOverrideType::NeCell,
            TableStyleOverrideType::NwCell,
            TableStyleOverrideType::SeCell,
            TableStyleOverrideType::SwCell,
            TableStyleOverrideType::WholeTable,
        ] {
            assert!(
                got.contains(&expected),
                "missing override type {expected:?}"
            );
        }
    }

    #[test]
    fn latent_styles_and_exceptions_parsed() {
        let sheet = parse(
            r#"<w:latentStyles w:count="371" w:defUIPriority="99">
                 <w:lsdException w:name="Normal" w:uiPriority="0" w:qFormat="1"/>
                 <w:lsdException w:name="heading 1" w:semiHidden="1"/>
               </w:latentStyles>"#,
        );
        let ls = sheet.latent_styles.expect("latentStyles present");
        assert_eq!(ls.count, Some(371));
        assert_eq!(ls.default_ui_priority, Some(99));
        assert_eq!(ls.exceptions.len(), 2);
        assert_eq!(ls.exceptions[0].name.as_deref(), Some("Normal"));
        assert_eq!(ls.exceptions[0].q_format, Some(true));
        assert_eq!(ls.exceptions[1].semi_hidden, Some(true));
    }
}
