//! Parser for `word/numbering.xml` — single-pass serde over the whole file.
//! Picture bullets' `<w:pict>` contents are deserialized via the VML schema.

use serde::Deserialize;

use crate::docx::error::Result;
use crate::docx::model::{
    AbstractNumId, AbstractNumbering, Alignment, Indentation, LevelOverride, LevelSuffix, NumId,
    NumPicBullet, NumPicBulletId, NumberFormat, NumberingDefinitions, NumberingInstance,
    NumberingLevelDefinition, RunProperties,
};
use crate::docx::parse::primitives::OnOff;
use crate::docx::parse::primitives::st_enums::{StJc, StNumberFormat};
use crate::docx::parse::properties::schema::paragraph::PPrXml;
use crate::docx::parse::properties::schema::run::RPrXml;
use crate::docx::parse::serde_xml::from_xml;

pub fn parse_numbering(data: &[u8]) -> Result<NumberingDefinitions> {
    if data.is_empty() {
        return Ok(NumberingDefinitions::default());
    }
    let schema: NumberingXml = from_xml(data)?;
    Ok(schema.into())
}

// ── serde schema ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct NumberingXml {
    #[serde(rename = "$value", default)]
    children: Vec<NumberingChildXml>,
}

#[derive(Deserialize)]
enum NumberingChildXml {
    #[serde(rename = "abstractNum")]
    AbstractNum(AbstractNumXml),
    #[serde(rename = "num")]
    Num(NumXml),
    #[serde(rename = "numPicBullet")]
    NumPicBullet(Box<NumPicBulletXml>),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct AbstractNumXml {
    /// The definition is keyed by this id; without it there is nothing to key.
    #[serde(
        rename = "@abstractNumId",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    abstract_num_id: Option<i64>,
    #[serde(rename = "lvl", default)]
    levels: Vec<LvlXml>,
}

#[derive(Deserialize)]
struct LvlXml {
    /// An unusable level index falls back to the top level.
    #[serde(
        rename = "@ilvl",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    ilvl: u8,
    #[serde(
        rename = "numFmt",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    num_fmt: Option<StNumberFormat>,
    #[serde(rename = "lvlText", default)]
    lvl_text: Option<ValString>,
    #[serde(
        rename = "start",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    start: Option<u32>,
    #[serde(
        rename = "lvlJc",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    lvl_jc: Option<StJc>,
    #[serde(
        rename = "suff",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    suff: Option<StLevelSuffix>,
    #[serde(rename = "isLgl", default)]
    is_lgl: Option<OnOff>,
    #[serde(rename = "pPr", default)]
    p_pr: Option<PPrXml>,
    #[serde(rename = "rPr", default)]
    r_pr: Option<RPrXml>,
    #[serde(
        rename = "lvlPicBulletId",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    lvl_pic_bullet_id: Option<i64>,
}

/// §17.18.53 ST_LevelSuffix.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum StLevelSuffix {
    Tab,
    Space,
    Nothing,
}

impl From<StLevelSuffix> for LevelSuffix {
    fn from(s: StLevelSuffix) -> Self {
        match s {
            StLevelSuffix::Tab => Self::Tab,
            StLevelSuffix::Space => Self::Space,
            StLevelSuffix::Nothing => Self::Nothing,
        }
    }
}

#[derive(Deserialize)]
struct NumXml {
    /// The instance is keyed by this id; without it there is nothing to key.
    #[serde(
        rename = "@numId",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    num_id: Option<i64>,
    #[serde(
        rename = "abstractNumId",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    abstract_num_id: Option<i64>,
    #[serde(rename = "lvlOverride", default)]
    overrides: Vec<LvlOverrideXml>,
}

#[derive(Deserialize)]
struct LvlOverrideXml {
    #[serde(
        rename = "@ilvl",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    ilvl: u8,
    #[serde(
        rename = "startOverride",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_val_attr"
    )]
    start_override: Option<u32>,
    #[serde(rename = "lvl", default)]
    lvl: Option<LvlXml>,
}

#[derive(Deserialize)]
struct NumPicBulletXml {
    /// The picture bullet is keyed by this id.
    #[serde(
        rename = "@numPicBulletId",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    num_pic_bullet_id: Option<i64>,
    #[serde(rename = "pict", default)]
    pict: Option<crate::docx::parse::vml::schema::PictXml>,
}

#[derive(Deserialize)]
struct ValString {
    #[serde(rename = "@val")]
    val: String,
}

// ── schema → model ────────────────────────────────────────────────────────

impl From<NumberingXml> for NumberingDefinitions {
    fn from(x: NumberingXml) -> Self {
        let mut defs = NumberingDefinitions::default();
        // Picture bullets may contain a VML `<w:pict>` (e.g., an imagedata
        // reference). Numbering has no body content, so no embeds crossing
        // into body convert — pass an empty ctx.
        let mut ctx = crate::docx::parse::body::ConvertCtx::new();
        for child in x.children {
            match child {
                // Definitions with no usable key are dropped: inserting them
                // under a fabricated id would shadow a real definition.
                NumberingChildXml::AbstractNum(a) => {
                    let Some(raw_id) = a.abstract_num_id else {
                        continue;
                    };
                    let id = AbstractNumId::new(raw_id);
                    defs.abstract_nums.insert(
                        id,
                        AbstractNumbering {
                            levels: a.levels.into_iter().map(Into::into).collect(),
                        },
                    );
                }
                NumberingChildXml::Num(n) => {
                    let Some(raw_id) = n.num_id else { continue };
                    defs.numbering_instances
                        .insert(NumId::new(raw_id), convert_num(n));
                }
                NumberingChildXml::NumPicBullet(bullet) => {
                    let Some(raw_id) = bullet.num_pic_bullet_id else {
                        continue;
                    };
                    let id = NumPicBulletId::new(raw_id);
                    let pict = bullet.pict.map(|p| p.into_model(&mut ctx));
                    defs.pic_bullets.insert(id, NumPicBullet { id, pict });
                }
                NumberingChildXml::Unknown => {}
            }
        }
        defs
    }
}

impl From<LvlXml> for NumberingLevelDefinition {
    fn from(x: LvlXml) -> Self {
        let (indentation, run_properties) = extract_level_properties(x.p_pr, x.r_pr);
        Self {
            level: x.ilvl,
            format: x.num_fmt.map(|v| NumberFormat::from(v)),
            level_text: x.lvl_text.map(|v| v.val).unwrap_or_default(),
            start: x.start,
            justification: x.lvl_jc.map(Alignment::from),
            indentation,
            run_properties,
            lvl_pic_bullet_id: x.lvl_pic_bullet_id.map(|v| NumPicBulletId::new(v)),
            suffix: x.suff.map(LevelSuffix::from).unwrap_or_default(),
            is_legal: x.is_lgl.map(|OnOff(b)| b).unwrap_or(false),
        }
    }
}

fn extract_level_properties(
    p_pr: Option<PPrXml>,
    r_pr: Option<RPrXml>,
) -> (Option<Indentation>, Option<RunProperties>) {
    let indentation = p_pr.and_then(|p| p.split().properties.indentation);
    let run_properties = r_pr.map(|r| r.split().0);
    (indentation, run_properties)
}

fn convert_num(n: NumXml) -> NumberingInstance {
    let abstract_num_id = n
        .abstract_num_id
        .map(|v| AbstractNumId::new(v))
        .unwrap_or_else(|| {
            log::warn!(
                "numbering instance numId={:?} has no abstractNumId; defaulting to 0",
                n.num_id
            );
            AbstractNumId::new(0)
        });
    let level_overrides = n
        .overrides
        .into_iter()
        .map(|o| {
            let definition = o.lvl.map(|mut lvl| {
                lvl.ilvl = o.ilvl; // the override's @ilvl wins over the inner lvl's
                NumberingLevelDefinition::from(lvl)
            });
            LevelOverride {
                level: o.ilvl,
                start_override: o.start_override,
                definition,
            }
        })
        .collect();
    NumberingInstance {
        abstract_num_id,
        level_overrides,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Upstream failed the document on a non-integer id. We drop the
    /// definition instead — but the guard it existed for still holds, and is
    /// the important part: `"1.0"` must never be silently accepted *as* id 1,
    /// which would shadow the real abstract numbering 1 and renumber lists.
    fn non_integer_numbering_ids_are_dropped_never_coerced() {
        let xml = br#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="1.0"/></w:numbering>"#;
        let defs = parse_numbering(xml).expect("a bad id must not fail the document");
        assert!(
            defs.abstract_nums.is_empty(),
            "\"1.0\" must not be keyed as any id"
        );
    }

    /// A malformed definition must not take its well-formed siblings with it.
    #[test]
    fn a_bad_numbering_id_does_not_discard_valid_definitions() {
        let xml = br#"
          <w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:abstractNum w:abstractNumId="bogusValue"><w:lvl w:ilvl="0"><w:lvlText w:val="X%1"/></w:lvl></w:abstractNum>
            <w:abstractNum w:abstractNumId="2"><w:lvl w:ilvl="0"><w:lvlText w:val="B%1"/></w:lvl></w:abstractNum>
            <w:num w:numId="12"><w:abstractNumId w:val="2"/></w:num>
          </w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        assert_eq!(defs.abstract_nums.len(), 1);
        assert!(defs.abstract_nums.contains_key(&AbstractNumId::new(2)));
        assert_eq!(defs.numbering_instances.len(), 1);
    }

    #[test]
    fn repeated_abstract_and_concrete_numbering_definitions_are_collected() {
        let xml = br#"
          <w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:lvlText w:val="A%1"/></w:lvl></w:abstractNum>
            <w:num w:numId="11"><w:abstractNumId w:val="1"/></w:num>
            <w:abstractNum w:abstractNumId="2"><w:lvl w:ilvl="0"><w:lvlText w:val="B%1"/></w:lvl></w:abstractNum>
            <w:num w:numId="12"><w:abstractNumId w:val="2"/></w:num>
          </w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        assert_eq!(defs.abstract_nums.len(), 2);
        assert_eq!(defs.numbering_instances.len(), 2);
        assert_eq!(
            defs.numbering_instances[&NumId::new(11)].abstract_num_id,
            AbstractNumId::new(1)
        );
        assert_eq!(
            defs.numbering_instances[&NumId::new(12)].abstract_num_id,
            AbstractNumId::new(2)
        );
    }

    #[test]
    fn num_without_abstract_ref_defaults_to_zero() {
        // `<w:num>` with no `<w:abstractNumId>` binds to abstract 0 (with a warn).
        let xml = br#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:num w:numId="7"/></w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        assert_eq!(
            defs.numbering_instances[&NumId::new(7)].abstract_num_id,
            AbstractNumId::new(0)
        );
    }

    #[test]
    fn lvl_override_uses_override_ilvl_not_inner_lvl() {
        // The override's own @ilvl wins over the nested `<w:lvl w:ilvl>`.
        let xml = br#"
          <w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:num w:numId="9">
              <w:abstractNumId w:val="1"/>
              <w:lvlOverride w:ilvl="2">
                <w:lvl w:ilvl="0"><w:lvlText w:val="X%1"/></w:lvl>
              </w:lvlOverride>
            </w:num>
          </w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        let inst = &defs.numbering_instances[&NumId::new(9)];
        assert_eq!(inst.level_overrides.len(), 1);
        assert_eq!(inst.level_overrides[0].level, 2);
        assert!(inst.level_overrides[0].definition.is_some());
    }

    #[test]
    fn lvl_suff_and_islgl_parse() {
        let xml = br#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:abstractNum w:abstractNumId="1">
              <w:lvl w:ilvl="0">
                <w:numFmt w:val="decimal"/>
                <w:lvlText w:val="%1."/>
                <w:suff w:val="space"/>
                <w:isLgl/>
              </w:lvl>
            </w:abstractNum>
          </w:numbering>"#;
        let lvl = &parse_numbering(xml).unwrap().abstract_nums[&AbstractNumId::new(1)].levels[0];
        assert_eq!(lvl.suffix, LevelSuffix::Space);
        assert!(lvl.is_legal);
    }

    #[test]
    fn lvl_suff_defaults_to_tab_and_islgl_false() {
        let xml = br#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>
          </w:numbering>"#;
        let lvl = &parse_numbering(xml).unwrap().abstract_nums[&AbstractNumId::new(1)].levels[0];
        assert_eq!(lvl.suffix, LevelSuffix::Tab);
        assert!(!lvl.is_legal);
    }

    #[test]
    fn start_override_without_lvl_is_captured() {
        // A `<w:lvlOverride>` may carry only `<w:startOverride>` (no `<w:lvl>`);
        // the old filter_map dropped such overrides entirely.
        let xml = br#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:num w:numId="3">
              <w:abstractNumId w:val="1"/>
              <w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride>
            </w:num>
          </w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        let inst = &defs.numbering_instances[&NumId::new(3)];
        assert_eq!(inst.level_overrides.len(), 1);
        assert_eq!(inst.level_overrides[0].level, 0);
        assert_eq!(inst.level_overrides[0].start_override, Some(5));
        assert!(inst.level_overrides[0].definition.is_none());
    }

    #[test]
    fn unknown_numbering_root_children_do_not_discard_known_definitions() {
        let xml = br#"
          <w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:abstractNum w:abstractNumId="3"/>
            <w:futureExtension><w:nested w:val="ignored"/></w:futureExtension>
            <w:num w:numId="13"><w:abstractNumId w:val="3"/></w:num>
          </w:numbering>"#;
        let defs = parse_numbering(xml).unwrap();
        assert!(defs.abstract_nums.contains_key(&AbstractNumId::new(3)));
        assert!(defs.numbering_instances.contains_key(&NumId::new(13)));
    }
}
