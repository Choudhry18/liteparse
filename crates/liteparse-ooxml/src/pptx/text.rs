//! DrawingML text bodies (§21.1.2) — schema and lowering into the shared
//! content model.
//!
//! This is the seam the whole PPTX reader hangs off. Everything downstream —
//! `collect_fragments`, `fit_lines`, `layout_paragraph`, `DrawCommand`,
//! `raster.rs`, `layout_to_pages`, `parse_from_native_blocks` — is typed on
//! [`model::Inline`] / [`model::Block`], so lowering `a:txBody` into those
//! types buys the entire back half of the stack. Bypassing them would mean
//! reimplementing it.
//!
//! ## Two element names, one grammar
//!
//! A slide shape's text body is `<p:txBody>` (PresentationML); a table cell's
//! and a graphic frame's is `<a:txBody>` (DrawingML). Both are `CT_TextBody`
//! with identical children, so one schema type serves both.
//!
//! quick-xml's serde layer matches on the local name with the prefix dropped
//! (the property `pptx::package` documents the hard way for `r:id`), so both
//! spellings arrive here as `txBody` and the union is free. That is a
//! convenience, not a guarantee we rely on: callers hand us the element's
//! bytes, and nothing here inspects the prefix.
//!
//! ## What this module does NOT do
//!
//! Lowering is deliberately **cascade-free**. `a:pPr` is preserved in
//! PPTX-native form ([`TextParagraphProperties`]) rather than flattened,
//! because resolving it requires the placeholder cascade — shape `a:lstStyle`
//! → layout placeholder → master `p:txStyles` → presentation
//! `defaultTextStyle` — which is a separate step with a different match rule
//! at each level. Run properties *are* lowered to [`model::RunProperties`]
//! immediately: every field there is `Option`, so it is already the right
//! carrier for unresolved direct formatting, and the cascade fills the `None`s
//! later via `FragmentCtx::paragraph_run_defaults`.

use serde::Deserialize;

use crate::model::dimension::{Dimension, Emu, HundredthPoints, ThousandthPercent};
use crate::model::{
    Alignment, BodyProperties, BreakKind, FontSlot, Hyperlink, HyperlinkTarget, Inline, RelId,
    RunElement, RunProperties, StrikeStyle, TextRun, ThemeFontRef, UnderlineStyle,
};

use crate::docx::error::Result;
use crate::docx::parse::drawing::schema::color::to_drawing_color;
use crate::docx::parse::drawing::schema::fill::SolidFillXml;
use crate::docx::parse::drawing::schema::shape::BodyPrXml;
use crate::docx::parse::serde_xml;

/// The deepest list level DrawingML addresses (`a:lvl1pPr` … `a:lvl9pPr`,
/// and `a:pPr/@lvl` in `0..=8`). §21.1.2.2.5.
pub const MAX_LIST_LEVEL: u8 = 9;

/// §22.9.2.7 ST_OnOff as a DrawingML *attribute* (`@b="1"`, `@i="0"`).
///
/// Deliberately not `AttrBool` and not serde's `bool`:
///
/// - serde's `bool` accepts only `"true"`/`"false"`, so `b="1"` — which is
///   how PowerPoint actually writes it — would silently read as absent and
///   lose every bold run.
/// - `AttrBool` maps anything unrecognised to `false`. Here that would be an
///   *explicit off* that overrides an inherited value, so a typo'd attribute
///   would strip bold that the master legitimately supplies.
///
/// Unrecognised values return `None` = inherit, per ECMA-376 §17.17's rule
/// that an invalid value is treated as absent.
fn opt_ooxml_bool<'de, D>(d: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(raw) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    Ok(match raw.trim() {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    })
}

// ── Lowered model ────────────────────────────────────────────────────────────

/// A lowered `CT_TextBody`.
#[derive(Clone, Debug, Default)]
pub struct TextBody {
    /// §21.1.2.1.1 `a:bodyPr` — insets, anchoring, autofit, rotation. Shared
    /// verbatim with the DOCX textbox path.
    pub body_pr: Option<BodyProperties>,
    /// §21.1.2.4.12 `a:lstStyle` — this shape's own per-level overrides, the
    /// innermost level of the cascade.
    pub list_style: ListStyle,
    pub paragraphs: Vec<TextParagraph>,
}

impl TextBody {
    /// True when the body carries no text at all. Empty bodies are common —
    /// every unfilled placeholder on a slide has one — and callers generally
    /// want to skip them rather than emit an empty block.
    pub fn is_empty(&self) -> bool {
        self.paragraphs.iter().all(|p| p.text().is_empty())
    }

    /// Plain text, one line per paragraph. Diagnostics and corpus probes only;
    /// the real output path goes through the block model.
    pub fn plain_text(&self) -> String {
        self.paragraphs
            .iter()
            .map(TextParagraph::text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A lowered `a:p`.
///
/// `properties` stays PPTX-native and unresolved on purpose — see the module
/// docs. `content` is already in the shared model and needs no further work.
#[derive(Clone, Debug, Default)]
pub struct TextParagraph {
    pub properties: TextParagraphProperties,
    pub content: Vec<Inline>,
    /// §21.1.2.2.3 `a:endParaRPr` — formatting of the paragraph mark. Carries
    /// the run formatting of an *empty* paragraph, so it is the only place the
    /// intended font size of a blank line survives.
    pub end_run_properties: Option<RunProperties>,
}

impl TextParagraph {
    /// Concatenated text of every run, with `a:br` rendered as `\n`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        collect_text(&self.content, &mut out);
        out
    }
}

fn collect_text(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::TextRun(run) => {
                for element in &run.content {
                    match element {
                        RunElement::Text(t) => out.push_str(t),
                        RunElement::LineBreak(_) => out.push('\n'),
                        RunElement::Tab => out.push('\t'),
                        _ => {}
                    }
                }
            }
            Inline::Hyperlink(link) => collect_text(&link.content, out),
            _ => {}
        }
    }
}

/// §21.1.2.4.12 `a:lstStyle` — nine optional per-level property sets.
///
/// Indexed by `a:pPr/@lvl`, i.e. **zero-based**, while the elements are named
/// one-based (`a:lvl1pPr` holds the properties for `lvl="0"`). Every read goes
/// through [`ListStyle::level`] so that off-by-one lives in exactly one place.
#[derive(Clone, Debug, Default)]
pub struct ListStyle {
    levels: [Option<Box<TextParagraphProperties>>; MAX_LIST_LEVEL as usize],
}

impl ListStyle {
    /// Properties declared for a zero-based level, if any. Out-of-range levels
    /// yield `None` rather than panicking — `@lvl` is author-supplied.
    pub fn level(&self, level: u8) -> Option<&TextParagraphProperties> {
        self.levels
            .get(level as usize)
            .and_then(|slot| slot.as_deref())
    }

    pub fn is_empty(&self) -> bool {
        self.levels.iter().all(Option::is_none)
    }
}

/// §21.1.2.2.7 `a:pPr` in PPTX-native, unresolved form.
///
/// Deliberately *not* [`crate::model::ParagraphProperties`]: that type is
/// shaped for `w:pPr` and has no home for a DrawingML bullet or an outline
/// level expressed as `@lvl`. Conversion happens after the cascade, when the
/// inherited values are known.
#[derive(Clone, Debug, Default)]
pub struct TextParagraphProperties {
    /// §21.1.2.2.7 `@lvl` — zero-based outline depth, `0..=8`.
    pub level: Option<u8>,
    pub alignment: Option<Alignment>,
    /// `@marL` — left margin. Where the *text* starts.
    pub margin_left: Option<Dimension<Emu>>,
    pub margin_right: Option<Dimension<Emu>>,
    /// `@indent` — first-line indent relative to `margin_left`. Negative for a
    /// hanging indent, which is how nearly every bulleted list is authored:
    /// the bullet sits at `marL + indent` and the text at `marL`.
    pub indent: Option<Dimension<Emu>>,
    pub default_tab_size: Option<Dimension<Emu>>,
    pub rtl: Option<bool>,
    pub line_spacing: Option<Spacing>,
    pub space_before: Option<Spacing>,
    pub space_after: Option<Spacing>,
    /// The bullet declared at this level, if the author said anything at all.
    /// `None` means "inherit"; `Some(Bullet::None)` means "explicitly no
    /// bullet" (`a:buNone`) and must override an inherited bullet.
    pub bullet: Option<Bullet>,
    /// §21.1.2.2.9 `a:defRPr` — run defaults for this level.
    pub default_run_properties: Option<RunProperties>,
}

/// §20.1.10.65 CT_TextSpacing — a spacing value is either a percentage of the
/// line height or an absolute point measure, never both.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Spacing {
    /// `a:spcPct` — thousandths of a percent (`100000` = 100%).
    Percent(Dimension<ThousandthPercent>),
    /// `a:spcPts` — hundredths of a point.
    Points(Dimension<HundredthPoints>),
}

/// §21.1.2.4 — the bullet declared for a paragraph level.
#[derive(Clone, Debug, PartialEq)]
pub enum Bullet {
    /// `a:buNone` — explicitly unbulleted. Distinct from an absent bullet:
    /// this one overrides an inherited bullet during the cascade.
    None,
    /// `a:buChar` — a literal glyph, typically from a symbol font named by
    /// `a:buFont` (e.g. `` in Wingdings).
    Character { char: String, font: Option<String> },
    /// `a:buAutoNum` — an auto-incrementing number.
    AutoNumber {
        scheme: AutoNumberScheme,
        /// `@startAt` — first number in the sequence. Defaults to 1.
        start_at: Option<u32>,
    },
}

/// §20.1.10.61 ST_TextAutonumberScheme. Only the schemes seen in practice
/// are named; the rest round-trip through `Other` so an unrecognised scheme
/// degrades to a plain number rather than killing the parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoNumberScheme {
    /// `1.` `2.` `3.`
    ArabicPeriod,
    /// `1)` `2)` `3)`
    ArabicParenR,
    /// `(1)` `(2)` `(3)`
    ArabicParenBoth,
    /// `a.` `b.` `c.`
    AlphaLcPeriod,
    /// `A.` `B.` `C.`
    AlphaUcPeriod,
    /// `a)` `b)` `c)`
    AlphaLcParenR,
    /// `A)` `B)` `C)`
    AlphaUcParenR,
    /// `i.` `ii.` `iii.`
    RomanLcPeriod,
    /// `I.` `II.` `III.`
    RomanUcPeriod,
    Other(String),
}

impl AutoNumberScheme {
    fn from_str(s: &str) -> Self {
        match s {
            "arabicPeriod" => Self::ArabicPeriod,
            "arabicParenR" => Self::ArabicParenR,
            "arabicParenBoth" => Self::ArabicParenBoth,
            "alphaLcPeriod" => Self::AlphaLcPeriod,
            "alphaUcPeriod" => Self::AlphaUcPeriod,
            "alphaLcParenR" => Self::AlphaLcParenR,
            "alphaUcParenR" => Self::AlphaUcParenR,
            "romanLcPeriod" => Self::RomanLcPeriod,
            "romanUcPeriod" => Self::RomanUcPeriod,
            other => Self::Other(other.to_string()),
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Parse a `<p:txBody>` or `<a:txBody>` element from its raw XML bytes.
///
/// The bytes must be a standalone, namespace-declaring element — i.e. what a
/// two-pass extraction over the slide part hands back, not a fragment whose
/// prefixes resolve only in the parent's scope.
pub fn parse_text_body(data: &[u8]) -> Result<TextBody> {
    let xml: TextBodyXml = serde_xml::from_xml(data)?;
    Ok(xml.into_model())
}

/// §19.3.1.52 `p:txStyles` — a master's three whole-part text styles.
///
/// The outer two levels of the text cascade are *not* placeholder shapes.
/// A master carries one `p:txStyles` for the whole part, and a placeholder
/// reaches the right child by its **kind**, not by matching a shape. See
/// [`pptx::textcascade`] for the routing.
///
/// [`pptx::textcascade`]: crate::pptx::textcascade
#[derive(Clone, Debug, Default)]
pub struct TextStyles {
    /// `p:titleStyle` — `title` and `ctrTitle` placeholders.
    pub title: ListStyle,
    /// `p:bodyStyle` — everything body-ish.
    pub body: ListStyle,
    /// `p:otherStyle` — `dt`, `ftr`, `sldNum` and the rest of the chrome.
    pub other: ListStyle,
}

impl TextStyles {
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.body.is_empty() && self.other.is_empty()
    }
}

/// Parse `p:txStyles` out of a **whole slide master part**.
///
/// Takes `p:sldMaster`'s bytes rather than the `p:txStyles` element, matching
/// [`parse_shape_tree`]: callers hold parts, not subtrees.
///
/// A master with no `p:txStyles` yields an empty [`TextStyles`] rather than
/// an error — masters routinely declare one, so an empty result means
/// something is genuinely unusual, not a normal case to special-case around.
///
/// [`parse_shape_tree`]: crate::pptx::parse_shape_tree
pub fn parse_text_styles(data: &[u8]) -> Result<TextStyles> {
    let xml: SlideMasterStylesXml = serde_xml::from_xml(data)?;
    Ok(xml
        .tx_styles
        .map(TxStylesXml::into_model)
        .unwrap_or_default())
}

/// Parse the text of a **whole SmartArt data part** (`ppt/diagrams/data*.xml`).
///
/// SmartArt is the one payload whose content is not in the slide at all: the
/// `p:graphicFrame` carries only `dgm:relIds`, and `@r:dm` points here. A
/// shape-tree walk therefore cannot see any of it, which made SmartArt the
/// largest single content gap for a native reader.
///
/// Returns one [`TextBody`] per `dgm:pt` that has one, in document order.
/// Nothing is filtered by node type: text-bearing points are untyped in
/// practice, so the `doc` and `pres` presentation nodes contribute nothing
/// and there is no duplication to guard against.
///
/// The caller resolves the relationship, because relationship resolution needs
/// the owning [`Part`](crate::pptx::Part) and this layer only sees bytes.
pub fn parse_diagram_text(data: &[u8]) -> Result<Vec<TextBody>> {
    let xml: DiagramDataXml = serde_xml::from_xml(data)?;
    Ok(xml
        .pt_lst
        .map(|l| l.points)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pt| pt.text)
        .map(TextBodyXml::into_model)
        .filter(|b| !b.is_empty())
        .collect())
}

/// §21.4.2.11 `dgm:dataModel`.
#[derive(Debug, Deserialize, Default)]
struct DiagramDataXml {
    #[serde(rename = "ptLst", default)]
    pt_lst: Option<DiagramPtLstXml>,
}

#[derive(Debug, Deserialize, Default)]
struct DiagramPtLstXml {
    #[serde(rename = "pt", default)]
    points: Vec<DiagramPtXml>,
}

/// §21.4.2.25 `dgm:pt`. `dgm:t` is an ordinary DrawingML text body, so the
/// same lowering serves it — the grammar is shared even though the namespace
/// is not.
#[derive(Debug, Deserialize, Default)]
struct DiagramPtXml {
    #[serde(rename = "t", default)]
    text: Option<TextBodyXml>,
}

/// Parse `p:defaultTextStyle` out of a **whole `ppt/presentation.xml` part**.
///
/// The outermost level, and the only one a non-placeholder shape can reach:
/// most non-placeholder runs that declare no size get it from here, so this
/// is not a rare fallback.
///
/// Absent yields an empty [`ListStyle`].
pub fn parse_default_text_style(data: &[u8]) -> Result<ListStyle> {
    let xml: PresentationStylesXml = serde_xml::from_xml(data)?;
    Ok(xml
        .default_text_style
        .map(ListStyleXml::into_model)
        .unwrap_or_default())
}

// ── Schema ───────────────────────────────────────────────────────────────────
//
// Field coverage is driven by what real decks actually use rather than by
// reading the spec front to back. Anything not modelled is left to serde's
// unknown-field tolerance, matching the fail-open posture ATTRIBUTION.md
// records for the DOCX vendor.

/// `pub(crate)` so the shape-tree walk can nest it directly as a serde field
/// rather than re-extracting the subtree's bytes; [`parse_text_body`] is the
/// standalone-bytes entry point used by tests and corpus probes.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct TextBodyXml {
    #[serde(rename = "bodyPr", default)]
    body_pr: Option<BodyPrXml>,
    #[serde(rename = "lstStyle", default)]
    lst_style: Option<ListStyleXml>,
    #[serde(rename = "p", default)]
    paragraphs: Vec<TextParagraphXml>,
}

impl TextBodyXml {
    pub(crate) fn into_model(self) -> TextBody {
        TextBody {
            body_pr: self.body_pr.map(Into::into),
            list_style: self
                .lst_style
                .map(ListStyleXml::into_model)
                .unwrap_or_default(),
            paragraphs: self
                .paragraphs
                .into_iter()
                .map(TextParagraphXml::into_model)
                .collect(),
        }
    }
}

/// `p:sldMaster`, read only for its `p:txStyles`. Every other child — the
/// shape tree, the colour map, the layout id list — is another module's job,
/// and serde's unknown-field tolerance drops them.
#[derive(Debug, Deserialize, Default)]
struct SlideMasterStylesXml {
    #[serde(rename = "txStyles", default)]
    tx_styles: Option<TxStylesXml>,
}

#[derive(Debug, Deserialize, Default)]
struct TxStylesXml {
    #[serde(rename = "titleStyle", default)]
    title: Option<ListStyleXml>,
    #[serde(rename = "bodyStyle", default)]
    body: Option<ListStyleXml>,
    #[serde(rename = "otherStyle", default)]
    other: Option<ListStyleXml>,
}

impl TxStylesXml {
    fn into_model(self) -> TextStyles {
        // The three children are `CT_TextListStyle`, the same type as
        // `a:lstStyle`, so the nine-level schema is reused verbatim rather
        // than re-declared.
        let lower = |s: Option<ListStyleXml>| s.map(ListStyleXml::into_model).unwrap_or_default();
        TextStyles {
            title: lower(self.title),
            body: lower(self.body),
            other: lower(self.other),
        }
    }
}

/// `p:presentation`, read only for its `p:defaultTextStyle`.
#[derive(Debug, Deserialize, Default)]
struct PresentationStylesXml {
    #[serde(rename = "defaultTextStyle", default)]
    default_text_style: Option<ListStyleXml>,
}

#[derive(Debug, Deserialize, Default)]
struct ListStyleXml {
    #[serde(rename = "lvl1pPr", default)]
    lvl1: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl2pPr", default)]
    lvl2: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl3pPr", default)]
    lvl3: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl4pPr", default)]
    lvl4: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl5pPr", default)]
    lvl5: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl6pPr", default)]
    lvl6: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl7pPr", default)]
    lvl7: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl8pPr", default)]
    lvl8: Option<TextParagraphPropertiesXml>,
    #[serde(rename = "lvl9pPr", default)]
    lvl9: Option<TextParagraphPropertiesXml>,
}

impl ListStyleXml {
    fn into_model(self) -> ListStyle {
        // `lvlNpPr` is one-based, `@lvl` is zero-based: lvl1pPr lands at index 0.
        let lower = |p: Option<TextParagraphPropertiesXml>| p.map(|p| Box::new(p.into_model()));
        ListStyle {
            levels: [
                lower(self.lvl1),
                lower(self.lvl2),
                lower(self.lvl3),
                lower(self.lvl4),
                lower(self.lvl5),
                lower(self.lvl6),
                lower(self.lvl7),
                lower(self.lvl8),
                lower(self.lvl9),
            ],
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct TextParagraphXml {
    #[serde(rename = "pPr", default)]
    p_pr: Option<TextParagraphPropertiesXml>,
    /// `a:r`, `a:br` and `a:fld` interleave freely and their order *is* the
    /// reading order, so they are collected as an ordered `$value` union
    /// rather than as three independent `Vec`s.
    #[serde(rename = "$value", default)]
    children: Vec<TextParaChildXml>,
}

#[derive(Debug, Deserialize)]
enum TextParaChildXml {
    #[serde(rename = "r")]
    Run(TextRunXml),
    #[serde(rename = "br")]
    Break(BreakXml),
    #[serde(rename = "fld")]
    Field(FieldXml),
    #[serde(rename = "endParaRPr")]
    EndParaRunProperties(TextCharPropertiesXml),
    /// `a:pPr` also arrives through `$value`; it is captured by the named
    /// field above and ignored here.
    #[serde(other)]
    Other,
}

impl TextParagraphXml {
    fn into_model(self) -> TextParagraph {
        let mut content: Vec<Inline> = Vec::new();
        let mut end_run_properties = None;

        for child in self.children {
            match child {
                TextParaChildXml::Run(run) => run.push_into(&mut content),
                TextParaChildXml::Break(br) => {
                    // `a:br` is a sibling of `a:r`, not a child, so it becomes
                    // its own single-element run. It carries `a:rPr` because
                    // the break's height comes from that formatting.
                    content.push(Inline::TextRun(Box::new(TextRun {
                        style_id: None,
                        properties: br
                            .r_pr
                            .map(TextCharPropertiesXml::into_run_properties)
                            .unwrap_or_default(),
                        content: vec![RunElement::LineBreak(BreakKind::TextWrapping)],
                        rsids: Default::default(),
                    })));
                }
                TextParaChildXml::Field(fld) => fld.push_into(&mut content),
                TextParaChildXml::EndParaRunProperties(rpr) => {
                    end_run_properties = Some(rpr.into_run_properties());
                }
                TextParaChildXml::Other => {}
            }
        }

        TextParagraph {
            properties: self
                .p_pr
                .map(TextParagraphPropertiesXml::into_model)
                .unwrap_or_default(),
            content,
            end_run_properties,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct TextRunXml {
    #[serde(rename = "rPr", default)]
    r_pr: Option<TextCharPropertiesXml>,
    /// `a:t` preserves whitespace unconditionally — DrawingML has no
    /// `xml:space` opt-in the way `w:t` does — so leading and trailing spaces
    /// are content and must not be trimmed.
    #[serde(rename = "t", default)]
    t: String,
}

impl TextRunXml {
    fn push_into(self, out: &mut Vec<Inline>) {
        let hyperlink = self.r_pr.as_ref().and_then(|p| p.hyperlink_target());
        let run = Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: self
                .r_pr
                .map(TextCharPropertiesXml::into_run_properties)
                .unwrap_or_default(),
            content: vec![RunElement::Text(self.t)],
            rsids: Default::default(),
        }));
        push_maybe_linked(run, hyperlink, out);
    }
}

#[derive(Debug, Deserialize, Default)]
struct BreakXml {
    #[serde(rename = "rPr", default)]
    r_pr: Option<TextCharPropertiesXml>,
}

/// §21.1.2.2.4 `a:fld` — a text field (slide number, date, …). Common in
/// footer placeholders, so dropping it would silently lose the footer line
/// on most decks.
///
/// The element carries both a `@type` instruction and an `a:t` holding the
/// value PowerPoint last rendered. We take the cached `a:t`: it is what the
/// author saw, it needs no evaluation context, and for the one field whose
/// value we *could* compute — `slidenum` — the cache already agrees. A field
/// whose cache is missing contributes nothing rather than a placeholder.
#[derive(Debug, Deserialize, Default)]
struct FieldXml {
    #[serde(rename = "rPr", default)]
    r_pr: Option<TextCharPropertiesXml>,
    #[serde(rename = "t", default)]
    t: Option<String>,
}

impl FieldXml {
    fn push_into(self, out: &mut Vec<Inline>) {
        let Some(text) = self.t.filter(|t| !t.is_empty()) else {
            return;
        };
        let hyperlink = self.r_pr.as_ref().and_then(|p| p.hyperlink_target());
        let run = Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: self
                .r_pr
                .map(TextCharPropertiesXml::into_run_properties)
                .unwrap_or_default(),
            content: vec![RunElement::Text(text)],
            rsids: Default::default(),
        }));
        push_maybe_linked(run, hyperlink, out);
    }
}

/// Wrap a run in a hyperlink, merging into the previous link when the target
/// matches so that a link split across several `a:r` elements — which is the
/// norm, since PowerPoint splits runs on spell-check state — emits as one
/// link rather than several adjacent ones.
fn push_maybe_linked(run: Inline, target: Option<HyperlinkTarget>, out: &mut Vec<Inline>) {
    let Some(target) = target else {
        out.push(run);
        return;
    };
    if let Some(Inline::Hyperlink(prev)) = out.last_mut()
        && same_target(&prev.target, &target)
    {
        prev.content.push(run);
        return;
    }
    out.push(Inline::Hyperlink(Hyperlink {
        target,
        content: vec![run],
    }));
}

fn same_target(a: &HyperlinkTarget, b: &HyperlinkTarget) -> bool {
    match (a, b) {
        (HyperlinkTarget::ExternalRel(x), HyperlinkTarget::ExternalRel(y)) => x == y,
        (HyperlinkTarget::ExternalUrl(x), HyperlinkTarget::ExternalUrl(y)) => x == y,
        (HyperlinkTarget::Internal { anchor: x }, HyperlinkTarget::Internal { anchor: y }) => {
            x == y
        }
        _ => false,
    }
}

// ── Paragraph properties ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct TextParagraphPropertiesXml {
    #[serde(
        rename = "@lvl",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    lvl: Option<u8>,
    #[serde(
        rename = "@algn",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    algn: Option<StTextAlignType>,
    #[serde(
        rename = "@marL",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_l: Option<Dimension<Emu>>,
    #[serde(
        rename = "@marR",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_r: Option<Dimension<Emu>>,
    #[serde(
        rename = "@indent",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    indent: Option<Dimension<Emu>>,
    #[serde(
        rename = "@defTabSz",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    def_tab_sz: Option<Dimension<Emu>>,
    #[serde(rename = "@rtl", default, deserialize_with = "opt_ooxml_bool")]
    rtl: Option<bool>,
    #[serde(rename = "lnSpc", default)]
    ln_spc: Option<SpacingXml>,
    #[serde(rename = "spcBef", default)]
    spc_bef: Option<SpacingXml>,
    #[serde(rename = "spcAft", default)]
    spc_aft: Option<SpacingXml>,
    #[serde(rename = "buNone", default)]
    bu_none: Option<EmptyXml>,
    #[serde(rename = "buChar", default)]
    bu_char: Option<BuCharXml>,
    #[serde(rename = "buAutoNum", default)]
    bu_auto_num: Option<BuAutoNumXml>,
    #[serde(rename = "buFont", default)]
    bu_font: Option<TextFontXml>,
    #[serde(rename = "defRPr", default)]
    def_r_pr: Option<TextCharPropertiesXml>,
}

impl TextParagraphPropertiesXml {
    fn into_model(self) -> TextParagraphProperties {
        // §21.1.2.4: the three bullet elements are a choice, at most one
        // present. `buNone` wins if a producer emits more than one, because
        // "explicitly none" is the only reading that can't invent a glyph.
        let bullet = if self.bu_none.is_some() {
            Some(Bullet::None)
        } else if let Some(auto) = self.bu_auto_num {
            Some(Bullet::AutoNumber {
                scheme: AutoNumberScheme::from_str(&auto.ty),
                start_at: auto.start_at,
            })
        } else {
            self.bu_char.and_then(|c| {
                // A `buChar` with no `@char` names no glyph; treat it as
                // absent rather than substituting a default bullet.
                (!c.char.is_empty()).then(|| Bullet::Character {
                    char: c.char,
                    font: self.bu_font.and_then(|f| f.typeface),
                })
            })
        };

        TextParagraphProperties {
            // §21.1.2.2.7 constrains `@lvl` to 0..=8. A larger value is
            // author error; clamp rather than drop, so the paragraph still
            // inherits from the deepest level that exists.
            level: self.lvl.map(|l| l.min(MAX_LIST_LEVEL - 1)),
            alignment: self.algn.map(Into::into),
            margin_left: self.mar_l,
            margin_right: self.mar_r,
            indent: self.indent,
            default_tab_size: self.def_tab_sz,
            rtl: self.rtl,
            line_spacing: self.ln_spc.and_then(SpacingXml::into_model),
            space_before: self.spc_bef.and_then(SpacingXml::into_model),
            space_after: self.spc_aft.and_then(SpacingXml::into_model),
            bullet,
            default_run_properties: self
                .def_r_pr
                .map(TextCharPropertiesXml::into_run_properties),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct EmptyXml {}

#[derive(Debug, Deserialize, Default)]
struct SpacingXml {
    #[serde(rename = "spcPct", default)]
    spc_pct: Option<SpacingPercentXml>,
    #[serde(rename = "spcPts", default)]
    spc_pts: Option<SpacingPointsXml>,
}

impl SpacingXml {
    fn into_model(self) -> Option<Spacing> {
        // §20.1.10.65 is a choice, but a producer emitting both should not
        // pick arbitrarily — percent is the PowerPoint-authored default and
        // by far the more common of the two in practice.
        if let Some(pct) = self.spc_pct.and_then(|v| v.val) {
            return Some(Spacing::Percent(pct));
        }
        self.spc_pts.and_then(|v| v.val).map(Spacing::Points)
    }
}

// Two concrete structs rather than one generic over `Unit`: deriving
// `Deserialize` on a generic would demand `U: Deserialize + Default`, which
// the unit markers deliberately are not.
#[derive(Debug, Deserialize, Default)]
struct SpacingPercentXml {
    #[serde(
        rename = "@val",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    val: Option<Dimension<ThousandthPercent>>,
}

#[derive(Debug, Deserialize, Default)]
struct SpacingPointsXml {
    #[serde(
        rename = "@val",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    val: Option<Dimension<HundredthPoints>>,
}

#[derive(Debug, Deserialize, Default)]
struct BuCharXml {
    #[serde(rename = "@char", default)]
    char: String,
}

#[derive(Debug, Deserialize, Default)]
struct BuAutoNumXml {
    #[serde(rename = "@type", default)]
    ty: String,
    #[serde(
        rename = "@startAt",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    start_at: Option<u32>,
}

/// §20.1.10.63 ST_TextAlignType.
#[derive(Clone, Copy, Debug, Deserialize)]
enum StTextAlignType {
    #[serde(rename = "l")]
    L,
    #[serde(rename = "ctr")]
    Ctr,
    #[serde(rename = "r")]
    R,
    #[serde(rename = "just")]
    Just,
    #[serde(rename = "justLow")]
    JustLow,
    #[serde(rename = "dist")]
    Dist,
    #[serde(rename = "thaiDist")]
    ThaiDist,
}

impl From<StTextAlignType> for Alignment {
    fn from(a: StTextAlignType) -> Self {
        match a {
            // `l`/`r` are physical in DrawingML and logical in the model.
            // Bidi paragraphs carry `@rtl` separately, which is what a
            // consumer needs to flip them; mapping to Start/End here keeps a
            // single alignment vocabulary across both pipelines.
            StTextAlignType::L => Alignment::Start,
            StTextAlignType::Ctr => Alignment::Center,
            StTextAlignType::R => Alignment::End,
            StTextAlignType::Just | StTextAlignType::JustLow => Alignment::Both,
            StTextAlignType::Dist => Alignment::Distribute,
            StTextAlignType::ThaiDist => Alignment::Thai,
        }
    }
}

// ── Run properties ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct TextCharPropertiesXml {
    /// §20.1.10.72 ST_TextFontSize — **hundredths of a point**, not the
    /// half-points `w:sz` uses. Conflating them scales every slide 50x.
    #[serde(
        rename = "@sz",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    sz: Option<Dimension<HundredthPoints>>,
    #[serde(rename = "@b", default, deserialize_with = "opt_ooxml_bool")]
    b: Option<bool>,
    #[serde(rename = "@i", default, deserialize_with = "opt_ooxml_bool")]
    i: Option<bool>,
    #[serde(
        rename = "@u",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    u: Option<StTextUnderlineType>,
    #[serde(
        rename = "@strike",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    strike: Option<StTextStrikeType>,
    #[serde(
        rename = "@cap",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    cap: Option<StTextCapsType>,
    /// §20.1.10.68 — character spacing in hundredths of a point.
    #[serde(
        rename = "@spc",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    spc: Option<Dimension<HundredthPoints>>,
    #[serde(
        rename = "@kern",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    kern: Option<Dimension<HundredthPoints>>,
    /// §20.1.10.79 — super/subscript as a percentage of the font size.
    #[serde(
        rename = "@baseline",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    baseline: Option<Dimension<ThousandthPercent>>,
    /// §21.1.2.3.9. The DOCX schema's `solidFill` is the same §20.1.8.54
    /// element with the same `EG_ColorChoice` child, so it is shared rather
    /// than re-declared — an srgb-only local type would silently drop every
    /// run whose colour is a scheme reference rather than a literal RGB.
    #[serde(rename = "solidFill", default)]
    solid_fill: Option<SolidFillXml>,
    #[serde(rename = "latin", default)]
    latin: Option<TextFontXml>,
    #[serde(rename = "ea", default)]
    ea: Option<TextFontXml>,
    #[serde(rename = "cs", default)]
    cs: Option<TextFontXml>,
    #[serde(rename = "hlinkClick", default)]
    hlink_click: Option<HlinkXml>,
}

impl TextCharPropertiesXml {
    fn into_run_properties(self) -> RunProperties {
        let mut props = RunProperties {
            font_size: self.sz.map(|s| s.to_half_points()),
            bold: self.b,
            italic: self.i,
            underline: self.u.map(Into::into),
            strike: self.strike.map(Into::into),
            // Unresolved on purpose: a scheme reference needs the theme *and*
            // the master's `p:clrMap`, which live at the slide, not here.
            drawing_color: self
                .solid_fill
                .and_then(|f| f.color)
                .and_then(to_drawing_color),
            // `@spc` is hundredths of a point; `w:spacing` is twips.
            spacing: self.spc.map(|s| Dimension::new((s.raw() * 20) / 100)),
            kerning: self.kern.map(|k| k.to_half_points()),
            ..Default::default()
        };

        // §20.1.10.66 ST_TextCapsType maps onto the two model flags; `none`
        // is an explicit off, which the cascade needs to distinguish from
        // absent.
        match self.cap {
            Some(StTextCapsType::All) => props.all_caps = Some(true),
            Some(StTextCapsType::Small) => props.small_caps = Some(true),
            Some(StTextCapsType::None) => {
                props.all_caps = Some(false);
                props.small_caps = Some(false);
            }
            None => {}
        }

        // `@baseline` is a percentage offset; the model carries a discrete
        // super/sub choice. Sign is the only distinction that survives, which
        // is what markdown and the layout stack can express anyway.
        if let Some(baseline) = self.baseline {
            props.vertical_align = match baseline.raw() {
                r if r > 0 => Some(crate::model::VerticalAlign::Superscript),
                r if r < 0 => Some(crate::model::VerticalAlign::Subscript),
                _ => None,
            };
        }

        if let Some(latin) = self.latin {
            let slot = latin.into_slot();
            props.fonts.ascii = slot.clone();
            props.fonts.high_ansi = slot;
        }
        if let Some(ea) = self.ea {
            props.fonts.east_asian = ea.into_slot();
        }
        if let Some(cs) = self.cs {
            props.fonts.complex_script = cs.into_slot();
        }

        props
    }

    /// The hyperlink this run participates in, if any. Kept separate from
    /// `into_run_properties` because a link is *structure* — it wraps the run
    /// in `Inline::Hyperlink` — not character formatting.
    fn hyperlink_target(&self) -> Option<HyperlinkTarget> {
        let hlink = self.hlink_click.as_ref()?;
        if let Some(action) = &hlink.action {
            // `ppaction://hlinksldjump` with no r:id is an in-deck jump whose
            // destination lives in the action string, not the rels.
            if hlink.id.is_none() && action.starts_with("ppaction://") {
                return Some(HyperlinkTarget::Internal {
                    anchor: action.clone(),
                });
            }
        }
        let id = hlink.id.as_ref()?;
        // An empty `r:id` is how PowerPoint spells "the link was removed".
        // Resolving it would produce a dangling relationship lookup.
        (!id.is_empty()).then(|| HyperlinkTarget::ExternalRel(RelId::new(id.clone())))
    }
}

/// §21.1.2.3.5 `a:hlinkClick`.
///
/// `@r:id` is spelled `@id` here because quick-xml's serde layer drops the
/// namespace prefix. That is safe **only** because `CT_Hyperlink` declares no
/// competing unprefixed `@id` — the same reason `a:blip/@r:embed` gets away
/// with it. Do not copy this pattern onto `p:sldId`/`p:sldMasterId`/
/// `p:sldLayoutId`, where an unprefixed `@id` does exist and the collision
/// silently binds the wrong value; those go through the namespace-aware
/// reader in `pptx::package`.
#[derive(Debug, Deserialize, Default)]
struct HlinkXml {
    #[serde(rename = "@id", default)]
    id: Option<String>,
    #[serde(rename = "@action", default)]
    action: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TextFontXml {
    #[serde(rename = "@typeface", default)]
    typeface: Option<String>,
}

impl TextFontXml {
    fn into_slot(self) -> FontSlot {
        let Some(typeface) = self.typeface else {
            return FontSlot::default();
        };
        // §20.1.10.53: `+mj-lt` / `+mn-ea` … are theme references, not font
        // names. Emitting them as an explicit family would put the literal
        // string "+mn-lt" into a font lookup.
        match typeface.as_str() {
            "+mj-lt" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MajorHAnsi),
            },
            "+mj-ea" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MajorEastAsia),
            },
            "+mj-cs" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MajorBidi),
            },
            "+mn-lt" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorHAnsi),
            },
            "+mn-ea" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorEastAsia),
            },
            "+mn-cs" => FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorBidi),
            },
            _ => FontSlot::from_name(typeface),
        }
    }
}

/// §20.1.10.82 ST_TextUnderlineType — a large enum of which only the common
/// values are modelled. Unlisted values fall through `lenient::opt_attr` to
/// `None` (= inherit), per ECMA-376 §17.17.
#[derive(Clone, Copy, Debug, Deserialize)]
enum StTextUnderlineType {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "words")]
    Words,
    #[serde(rename = "sng")]
    Sng,
    #[serde(rename = "dbl")]
    Dbl,
    #[serde(rename = "heavy")]
    Heavy,
    #[serde(rename = "dotted")]
    Dotted,
    #[serde(rename = "dash")]
    Dash,
    #[serde(rename = "wavy")]
    Wavy,
}

impl From<StTextUnderlineType> for UnderlineStyle {
    fn from(u: StTextUnderlineType) -> Self {
        match u {
            StTextUnderlineType::None => UnderlineStyle::None,
            StTextUnderlineType::Words => UnderlineStyle::Words,
            StTextUnderlineType::Sng => UnderlineStyle::Single,
            StTextUnderlineType::Dbl => UnderlineStyle::Double,
            StTextUnderlineType::Heavy => UnderlineStyle::Thick,
            StTextUnderlineType::Dotted => UnderlineStyle::Dotted,
            StTextUnderlineType::Dash => UnderlineStyle::Dash,
            StTextUnderlineType::Wavy => UnderlineStyle::Wave,
        }
    }
}

/// §20.1.10.78 ST_TextStrikeType.
//
// The shared `Strike` postfix mirrors the spec's own value names, which is
// what makes these schema enums greppable against ECMA-376; renaming them to
// satisfy the lint would break that correspondence.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, Deserialize)]
enum StTextStrikeType {
    #[serde(rename = "noStrike")]
    NoStrike,
    #[serde(rename = "sngStrike")]
    SngStrike,
    #[serde(rename = "dblStrike")]
    DblStrike,
}

impl From<StTextStrikeType> for StrikeStyle {
    fn from(s: StTextStrikeType) -> Self {
        match s {
            StTextStrikeType::NoStrike => StrikeStyle::None,
            StTextStrikeType::SngStrike => StrikeStyle::Single,
            StTextStrikeType::DblStrike => StrikeStyle::Double,
        }
    }
}

/// §20.1.10.66 ST_TextCapsType.
#[derive(Clone, Copy, Debug, Deserialize)]
enum StTextCapsType {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "small")]
    Small,
    #[serde(rename = "all")]
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(inner: &str) -> TextBody {
        let xml = format!(
            r#"<p:txBody xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                         xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                         xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">{inner}</p:txBody>"#
        );
        parse_text_body(xml.as_bytes()).expect("parses")
    }

    fn run_props(b: &TextBody, para: usize, run: usize) -> RunProperties {
        match &b.paragraphs[para].content[run] {
            Inline::TextRun(r) => r.properties.clone(),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    /// The 50x trap: `a:rPr/@sz` is hundredths of a point, `w:rPr/@sz` is
    /// half-points. Reading one as the other scales every slide 50x.
    #[test]
    fn font_size_is_hundredths_of_a_point_not_half_points() {
        let b = body(r#"<a:p><a:r><a:rPr sz="1800"/><a:t>x</a:t></a:r></a:p>"#);
        let size = run_props(&b, 0, 0).font_size.expect("size present");
        assert_eq!(size.to_points_f32(), 18.0);
    }

    /// `a:br` is a sibling of `a:r`, not a child, so its position in the
    /// child sequence is the only thing that puts the break in the right
    /// place. A schema using three independent `Vec`s would lose this.
    #[test]
    fn break_keeps_its_position_between_runs() {
        let b = body(r#"<a:p><a:r><a:t>one</a:t></a:r><a:br/><a:r><a:t>two</a:t></a:r></a:p>"#);
        assert_eq!(b.paragraphs[0].text(), "one\ntwo");
    }

    /// `a:fld` is common in footer placeholders; dropping it loses the
    /// footer line almost everywhere it's used.
    #[test]
    fn field_emits_its_cached_value() {
        let b = body(r#"<a:p><a:fld id="{1}" type="slidenum"><a:t>7</a:t></a:fld></a:p>"#);
        assert_eq!(b.paragraphs[0].text(), "7");
    }

    /// A field with no cached value contributes nothing rather than an
    /// empty-string run or a placeholder.
    #[test]
    fn field_without_cached_value_is_dropped() {
        let b = body(r#"<a:p><a:fld id="{1}" type="datetime"/></a:p>"#);
        assert!(b.paragraphs[0].content.is_empty());
    }

    /// `buNone` must survive as an explicit value: during the cascade it has
    /// to override an inherited bullet, which an absent bullet must not do.
    #[test]
    fn bu_none_is_distinct_from_absent() {
        let explicit = body(r#"<a:p><a:pPr><a:buNone/></a:pPr><a:r><a:t>x</a:t></a:r></a:p>"#);
        assert_eq!(explicit.paragraphs[0].properties.bullet, Some(Bullet::None));

        let absent = body(r#"<a:p><a:pPr/><a:r><a:t>x</a:t></a:r></a:p>"#);
        assert_eq!(absent.paragraphs[0].properties.bullet, None);
    }

    #[test]
    fn bullet_char_carries_its_font() {
        let b = body(
            r#"<a:p><a:pPr><a:buFont typeface="Wingdings"/><a:buChar char="v"/></a:pPr></a:p>"#,
        );
        assert_eq!(
            b.paragraphs[0].properties.bullet,
            Some(Bullet::Character {
                char: "v".to_string(),
                font: Some("Wingdings".to_string()),
            })
        );
    }

    #[test]
    fn auto_number_scheme_and_start() {
        let b =
            body(r#"<a:p><a:pPr><a:buAutoNum type="alphaLcParenR" startAt="3"/></a:pPr></a:p>"#);
        assert_eq!(
            b.paragraphs[0].properties.bullet,
            Some(Bullet::AutoNumber {
                scheme: AutoNumberScheme::AlphaLcParenR,
                start_at: Some(3),
            })
        );
    }

    /// An unrecognised scheme degrades to `Other` rather than killing the
    /// parse — the fail-open posture the DOCX vendor settled on.
    #[test]
    fn unknown_auto_number_scheme_is_preserved_not_fatal() {
        let b = body(r#"<a:p><a:pPr><a:buAutoNum type="ealphaFullWidth"/></a:pPr></a:p>"#);
        let Some(Bullet::AutoNumber { scheme, .. }) = &b.paragraphs[0].properties.bullet else {
            panic!("expected an auto-number bullet");
        };
        assert_eq!(scheme, &AutoNumberScheme::Other("ealphaFullWidth".into()));
    }

    /// `a:lvl1pPr` holds the properties for `@lvl="0"`. The element names are
    /// one-based and the attribute is zero-based.
    #[test]
    fn list_style_levels_are_zero_indexed() {
        let b = body(
            r#"<a:lstStyle>
                 <a:lvl1pPr marL="111"/>
                 <a:lvl3pPr marL="333"/>
               </a:lstStyle>"#,
        );
        assert_eq!(
            b.list_style.level(0).unwrap().margin_left.unwrap().raw(),
            111
        );
        assert!(b.list_style.level(1).is_none());
        assert_eq!(
            b.list_style.level(2).unwrap().margin_left.unwrap().raw(),
            333
        );
        assert!(
            b.list_style.level(9).is_none(),
            "out of range yields None, not a panic"
        );
    }

    /// `@lvl` is constrained to 0..=8; a larger author-supplied value clamps
    /// rather than dropping the paragraph's level entirely.
    #[test]
    fn out_of_range_level_clamps() {
        let b = body(r#"<a:p><a:pPr lvl="42"/></a:p>"#);
        assert_eq!(b.paragraphs[0].properties.level, Some(MAX_LIST_LEVEL - 1));
    }

    /// PowerPoint splits a linked phrase across several `a:r` on spell-check
    /// state; emitting one hyperlink per run would produce adjacent duplicate
    /// links in the markdown.
    #[test]
    fn consecutive_runs_sharing_a_target_merge_into_one_link() {
        let b = body(
            r#"<a:p>
                 <a:r><a:rPr><a:hlinkClick r:id="rId2"/></a:rPr><a:t>lite</a:t></a:r>
                 <a:r><a:rPr><a:hlinkClick r:id="rId2"/></a:rPr><a:t>parse</a:t></a:r>
               </a:p>"#,
        );
        assert_eq!(b.paragraphs[0].content.len(), 1);
        let Inline::Hyperlink(link) = &b.paragraphs[0].content[0] else {
            panic!("expected a hyperlink");
        };
        assert_eq!(link.content.len(), 2);
        assert_eq!(b.paragraphs[0].text(), "liteparse");
    }

    #[test]
    fn different_targets_do_not_merge() {
        let b = body(
            r#"<a:p>
                 <a:r><a:rPr><a:hlinkClick r:id="rId2"/></a:rPr><a:t>a</a:t></a:r>
                 <a:r><a:rPr><a:hlinkClick r:id="rId3"/></a:rPr><a:t>b</a:t></a:r>
               </a:p>"#,
        );
        assert_eq!(b.paragraphs[0].content.len(), 2);
    }

    /// An empty `r:id` is how PowerPoint spells a removed link. Resolving it
    /// would produce a dangling relationship lookup.
    #[test]
    fn empty_relationship_id_is_not_a_link() {
        let b = body(r#"<a:p><a:r><a:rPr><a:hlinkClick r:id=""/></a:rPr><a:t>x</a:t></a:r></a:p>"#);
        assert!(matches!(b.paragraphs[0].content[0], Inline::TextRun(_)));
    }

    /// `+mn-lt` is a theme reference, not a font family. Emitting it as an
    /// explicit name would put the literal string into a font lookup.
    #[test]
    fn theme_font_reference_is_not_an_explicit_family() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:latin typeface="+mn-lt"/></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        let fonts = run_props(&b, 0, 0).fonts;
        assert_eq!(fonts.ascii.explicit, None);
        assert_eq!(fonts.ascii.theme, Some(ThemeFontRef::MinorHAnsi));
    }

    #[test]
    fn explicit_font_family_survives() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:latin typeface="Calibri"/></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        let fonts = run_props(&b, 0, 0).fonts;
        assert_eq!(fonts.ascii.explicit.as_deref(), Some("Calibri"));
        assert_eq!(
            fonts.high_ansi.explicit.as_deref(),
            Some("Calibri"),
            "a:latin fills both latin slots"
        );
    }

    /// DrawingML has no `xml:space` opt-in — `a:t` always preserves
    /// whitespace, so trimming would fuse words across runs.
    #[test]
    fn leading_and_trailing_whitespace_is_content() {
        let b = body(r#"<a:p><a:r><a:t>one </a:t></a:r><a:r><a:t> two</a:t></a:r></a:p>"#);
        assert_eq!(b.paragraphs[0].text(), "one  two");
    }

    /// A theme colour is *carried*, not resolved: `schemeClr` needs the theme
    /// and the master's `p:clrMap`, neither of which is a property of the
    /// run. Dropping it here would read downstream as "no colour declared"
    /// and paint the run black.
    #[test]
    fn scheme_color_is_carried_unresolved() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:solidFill><a:schemeClr val="tx1"/></a:solidFill></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        assert!(matches!(
            run_props(&b, 0, 0).drawing_color,
            Some(crate::model::DrawingColor::Scheme {
                name: crate::model::SchemeColorVal::Tx1,
                ..
            })
        ));
    }

    /// The transforms are the reason a `Color` could not carry this: a tinted
    /// scheme reference is a colour *expression*, not a value.
    #[test]
    fn scheme_color_keeps_its_transforms() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:solidFill><a:schemeClr val="accent1"><a:lumMod val="65000"/></a:schemeClr></a:solidFill></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        let props = run_props(&b, 0, 0);
        let color = props.drawing_color.as_ref().expect("colour");
        assert_eq!(color.transforms().len(), 1);
    }

    #[test]
    fn srgb_color_is_read() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:solidFill><a:srgbClr val="FF6600"/></a:solidFill></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        assert!(matches!(
            run_props(&b, 0, 0).drawing_color,
            Some(crate::model::DrawingColor::Srgb { rgb: 0xFF6600, .. })
        ));
    }

    /// `w:rPr`'s colour field stays Word's. Setting both would give the
    /// cascade two sources of truth for one property.
    #[test]
    fn drawingml_does_not_populate_the_word_color_field() {
        let b = body(
            r#"<a:p><a:r><a:rPr><a:solidFill><a:srgbClr val="FF6600"/></a:solidFill></a:rPr><a:t>x</a:t></a:r></a:p>"#,
        );
        assert_eq!(run_props(&b, 0, 0).color, None);
    }

    /// `cap="none"` is an explicit off, which the cascade must be able to
    /// tell apart from "inherit".
    #[test]
    fn caps_none_is_explicit_off() {
        let b = body(r#"<a:p><a:r><a:rPr cap="none"/><a:t>x</a:t></a:r></a:p>"#);
        assert_eq!(run_props(&b, 0, 0).all_caps, Some(false));

        let inherit = body(r#"<a:p><a:r><a:rPr/><a:t>x</a:t></a:r></a:p>"#);
        assert_eq!(run_props(&inherit, 0, 0).all_caps, None);
    }

    #[test]
    fn caps_all_sets_all_caps() {
        let b = body(r#"<a:p><a:r><a:rPr cap="all"/><a:t>x</a:t></a:r></a:p>"#);
        assert_eq!(run_props(&b, 0, 0).all_caps, Some(true));
    }

    /// An empty paragraph's intended formatting survives only on
    /// `a:endParaRPr`, so it must not be discarded.
    #[test]
    fn end_para_run_properties_are_kept() {
        let b = body(r#"<a:p><a:endParaRPr sz="2400" b="1"/></a:p>"#);
        let end = b.paragraphs[0]
            .end_run_properties
            .as_ref()
            .expect("present");
        assert_eq!(end.font_size.unwrap().to_points_f32(), 24.0);
        assert_eq!(end.bold, Some(true));
    }

    #[test]
    fn spacing_percent_and_points_are_distinguished() {
        let b = body(
            r#"<a:p><a:pPr>
                 <a:lnSpc><a:spcPct val="90000"/></a:lnSpc>
                 <a:spcBef><a:spcPts val="600"/></a:spcBef>
               </a:pPr></a:p>"#,
        );
        let p = &b.paragraphs[0].properties;
        assert!(matches!(p.line_spacing, Some(Spacing::Percent(_))));
        let Some(Spacing::Points(pts)) = p.space_before else {
            panic!("expected an absolute spacing");
        };
        assert_eq!(pts.to_points_f32(), 6.0);
    }

    /// `l`/`r` are physical in DrawingML; the model's vocabulary is logical.
    #[test]
    fn alignment_maps_onto_the_shared_vocabulary() {
        for (algn, expected) in [
            ("l", Alignment::Start),
            ("ctr", Alignment::Center),
            ("r", Alignment::End),
            ("just", Alignment::Both),
        ] {
            let b = body(&format!(r#"<a:p><a:pPr algn="{algn}"/></a:p>"#));
            assert_eq!(
                b.paragraphs[0].properties.alignment,
                Some(expected),
                "algn={algn}"
            );
        }
    }

    /// Per ECMA-376 §17.17 an invalid attribute value is treated as absent,
    /// i.e. inherit — never fatal.
    #[test]
    fn unknown_attribute_values_degrade_to_inherit() {
        let b = body(
            r#"<a:p><a:pPr algn="bogus" lvl="notanumber"/><a:r><a:rPr sz="huge"/><a:t>x</a:t></a:r></a:p>"#,
        );
        assert_eq!(b.paragraphs[0].properties.alignment, None);
        assert_eq!(b.paragraphs[0].properties.level, None);
        assert_eq!(run_props(&b, 0, 0).font_size, None);
        assert_eq!(
            b.paragraphs[0].text(),
            "x",
            "content survives a bad attribute"
        );
    }

    /// A table cell spells the same grammar `a:txBody`. Both must parse
    /// identically.
    #[test]
    fn drawingml_spelling_parses_the_same_as_the_presentationml_one() {
        let xml = r#"<a:txBody xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
                       <a:bodyPr/><a:p><a:r><a:t>cell</a:t></a:r></a:p>
                     </a:txBody>"#;
        let b = parse_text_body(xml.as_bytes()).expect("parses");
        assert_eq!(b.paragraphs[0].text(), "cell");
    }

    /// Unmodeled elements must not be fatal — the vendor's fail-open rule.
    #[test]
    fn unmodeled_elements_are_ignored_not_fatal() {
        let b = body(
            r#"<a:p><a:pPr><a:tabLst><a:tab pos="100" algn="l"/></a:tabLst></a:pPr>
                 <a:r><a:rPr><a:effectLst/><a:sym typeface="Wingdings"/></a:rPr><a:t>x</a:t></a:r>
               </a:p>"#,
        );
        assert_eq!(b.paragraphs[0].text(), "x");
    }

    #[test]
    fn body_properties_come_through_the_shared_docx_schema() {
        let b = body(
            r#"<a:bodyPr lIns="0" anchor="ctr"><a:normAutofit fontScale="62500"/></a:bodyPr><a:p/>"#,
        );
        let bp = b.body_pr.expect("present");
        assert_eq!(bp.left_inset.unwrap().raw(), 0);
        assert!(bp.anchor.is_some());
        assert!(bp.auto_fit.is_some());
    }

    #[test]
    fn empty_body_is_reported_empty() {
        assert!(body(r#"<a:bodyPr/><a:p><a:endParaRPr/></a:p>"#).is_empty());
        assert!(!body(r#"<a:p><a:r><a:t>x</a:t></a:r></a:p>"#).is_empty());
    }
}
