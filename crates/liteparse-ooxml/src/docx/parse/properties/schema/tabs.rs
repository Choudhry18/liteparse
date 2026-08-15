//! Tabs sub-schema (§17.3.1.38 w:tabs).

use serde::Deserialize;

use crate::docx::model::TabStop;
use crate::docx::model::dimension::{Dimension, Twips};
use crate::docx::parse::primitives::st_enums::{StTabJc, StTabTlc};

/// `<w:tabs>` — a list of `<w:tab>` stops.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TabsXml {
    #[serde(rename = "tab", default)]
    pub stops: Vec<TabXml>,
}

/// `<w:tab w:pos="..." w:val="..." w:leader="..."/>` inside a `<w:tabs>`.
#[derive(Clone, Copy, Debug, Deserialize)]
pub(crate) struct TabXml {
    #[serde(
        rename = "@pos",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    pos: Option<Dimension<Twips>>,
    #[serde(
        rename = "@val",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    val: StTabJc,
    #[serde(
        rename = "@leader",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    leader: StTabTlc,
}

/// A tab stop with no usable `@pos` cannot be placed, so it is dropped rather
/// than collapsed onto position zero.
fn to_tab_stop(x: TabXml) -> Option<TabStop> {
    Some(TabStop {
        position: x.pos?,
        alignment: x.val.into(),
        leader: x.leader.into(),
    })
}

impl From<TabsXml> for Vec<TabStop> {
    fn from(x: TabsXml) -> Self {
        // `<w:tab val="clear"/>` removes an inherited stop. We keep these in
        // the model so the style cascade (§17.7.2) can apply them when
        // merging a paragraph's tabs with its parent style's tabs. The
        // layout build step (`render::layout::build::convert`) drops them
        // before emitting layout tab stops.
        x.stops.into_iter().filter_map(to_tab_stop).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::model::{TabAlignment, TabLeader};

    fn parse(xml: &str) -> Vec<TabStop> {
        let t: TabsXml = quick_xml::de::from_str(xml).unwrap();
        t.into()
    }

    #[test]
    fn single_tab_with_leader() {
        let ts = parse(r#"<tabs><tab pos="1440" val="center" leader="dot"/></tabs>"#);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].position.raw(), 1440);
        assert_eq!(ts[0].alignment, TabAlignment::Center);
        assert_eq!(ts[0].leader, TabLeader::Dot);
    }

    #[test]
    fn tab_defaults_left_and_no_leader() {
        let ts = parse(r#"<tabs><tab pos="720"/></tabs>"#);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].alignment, TabAlignment::Left);
        assert_eq!(ts[0].leader, TabLeader::None);
    }

    #[test]
    fn clear_tabs_preserved_for_style_cascade() {
        // Clear entries must survive parsing so the style cascade can use
        // them to remove inherited tabs at matching positions
        // (`render::resolve::properties::merge_paragraph_properties`).
        // The layout build step filters them out before rendering.
        let ts = parse(
            r#"<tabs>
                <tab pos="1440" val="left"/>
                <tab pos="2880" val="clear"/>
                <tab pos="4320" val="right"/>
            </tabs>"#,
        );
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].position.raw(), 1440);
        assert_eq!(ts[0].alignment, TabAlignment::Left);
        assert_eq!(ts[1].position.raw(), 2880);
        assert_eq!(ts[1].alignment, TabAlignment::Clear);
        assert_eq!(ts[2].position.raw(), 4320);
        assert_eq!(ts[2].alignment, TabAlignment::Right);
    }

    #[test]
    fn legacy_num_becomes_left() {
        let ts = parse(r#"<tabs><tab pos="720" val="num"/></tabs>"#);
        assert_eq!(ts[0].alignment, TabAlignment::Left);
    }

    #[test]
    fn empty_tabs() {
        let ts = parse(r#"<tabs/>"#);
        assert!(ts.is_empty());
    }
}
