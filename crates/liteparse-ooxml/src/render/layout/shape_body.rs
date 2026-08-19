//! §20.1.2.1.1 / §20.1.10.60: laying a DrawingML text body out inside the
//! rectangle a shape gives it.
//!
//! This is the **format-neutral half** of what `build/floating.rs` does for a
//! `wps:txbx`. Everything here reads `a:bodyPr` and a `Vec<LayoutBlock>` and
//! names no `w:` type, because `a:bodyPr` is DrawingML: a PPTX `p:txBody` and a
//! DOCX `wps:txbx` declare insets, anchor, autofit and overflow with the same
//! element and the same defaults. Only the *front* half — turning markup into
//! `LayoutBlock`s — differs per format, so only that stayed behind.
//!
//! Output is **shape-local Pt with origin at the shape's top-left**. The caller
//! shifts it onto the page (DOCX: the stacker, by `(fs.x, shape_y)`; PPTX: by
//! the shape's `slide_rect` origin).

use crate::render::dimension::Pt;
use crate::render::geometry::PtSize;
use crate::render::layout::draw_command::DrawCommand;
use crate::render::layout::section::{LayoutBlock, PageParity};

/// §20.1.2.1.1 spec defaults for an absent inset: 91440 EMU horizontal,
/// 45720 EMU vertical.
///
/// Materializing these is not optional. `BodyProperties` keeps an absent
/// attribute as `None` rather than substituting the default, so a consumer that
/// reads the field directly lays the body out at the full shape width — which
/// on the PPTX corpus is 47.2% of text shapes, every one of them wrong.
const DEFAULT_INSET_LR: f32 = 91440.0 / 12700.0; // ≈ 7.2pt
const DEFAULT_INSET_TB: f32 = 45720.0 / 12700.0; // ≈ 3.6pt

/// The four §20.1.2.1.1 insets, with defaults applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyInsets {
    pub left: Pt,
    pub top: Pt,
    pub right: Pt,
    pub bottom: Pt,
}

impl BodyInsets {
    /// Read the insets off an `a:bodyPr`, substituting the spec default for
    /// each attribute that is absent.
    ///
    /// Insets are **signed** in the schema (`ST_Coordinate32`) and are not
    /// clamped here, matching the parser's deliberate choice not to clamp them.
    /// A negative inset inflates the body past its box, which is what Word
    /// draws.
    pub fn resolve(body_pr: Option<&crate::model::BodyProperties>) -> Self {
        let default_lr = Pt::new(DEFAULT_INSET_LR);
        let default_tb = Pt::new(DEFAULT_INSET_TB);
        match body_pr {
            None => Self {
                left: default_lr,
                top: default_tb,
                right: default_lr,
                bottom: default_tb,
            },
            Some(bp) => Self {
                left: bp.left_inset.map_or(default_lr, Pt::from),
                top: bp.top_inset.map_or(default_tb, Pt::from),
                right: bp.right_inset.map_or(default_lr, Pt::from),
                bottom: bp.bottom_inset.map_or(default_tb, Pt::from),
            },
        }
    }

    /// Width left for the body after the horizontal insets, floored at zero.
    pub fn content_width(&self, extent: PtSize) -> Pt {
        (extent.width - self.left - self.right).max(Pt::ZERO)
    }

    /// Height left for the body after the vertical insets, floored at zero.
    pub fn content_height(&self, extent: PtSize) -> Pt {
        (extent.height - self.top - self.bottom).max(Pt::ZERO)
    }
}

/// §20.1.10.60 `ST_TextAnchoringType`: where a shape's text body sits inside
/// the box its insets leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyAnchor {
    Top,
    Center,
    Bottom,
}

impl BodyAnchor {
    /// Total over `TextAnchoringType`, with the §20.1.2.1.1 default for an
    /// absent attribute.
    pub fn resolve(anchor: Option<crate::model::TextAnchoringType>) -> Self {
        use crate::model::TextAnchoringType as T;
        match anchor {
            None | Some(T::Top) => Self::Top,
            Some(T::Center) => Self::Center,
            Some(T::Bottom) => Self::Bottom,
            // §20.1.10.60 `just`/`dist` stretch the *inter-line* spacing to
            // fill the box, which this sub-layout has no line-level control
            // over. Degrading to `Top` is the closest honest reading: a
            // justified body also begins at the top, it simply is not
            // stretched to reach the bottom.
            Some(anchor @ (T::Justified | T::Distributed)) => {
                log::warn!(
                    "shape text: anchor={anchor:?} distributes lines to fill the body \
                     (§20.1.10.60), which is not modelled — anchoring to the top instead"
                );
                Self::Top
            }
        }
    }

    /// How far below the top inset a `text_height`-tall body sits in a
    /// `box_height`-tall box.
    ///
    /// The slack is floored at zero, so a body taller than its box anchors to
    /// the top and overflows *downward* whatever the attribute says.
    /// `@vertOverflow` defaults to `overflow` — Word draws overflowing shape
    /// text rather than clipping it — and centring a body that does not fit
    /// would put its first lines above the shape, over whatever sits there. So
    /// the choice here is only ever about where the spare room goes. A body
    /// that asks for `clip` is trimmed afterwards, in `overflow_keeps`, not by
    /// moving it.
    pub fn offset(self, box_height: Pt, text_height: Pt) -> Pt {
        let slack = (box_height - text_height).max(Pt::ZERO);
        match self {
            Self::Top => Pt::ZERO,
            Self::Center => slack * 0.5,
            Self::Bottom => slack,
        }
    }
}

/// The height `blocks` need inside a body `extent_width` wide, insets included.
///
/// For the caller that must size a box *before* it can place text in it: a
/// DrawingML table row is a minimum height that grows to fit its tallest cell
/// (§21.1.3.18), so the row's rectangle is not known until every cell in it has
/// been measured, and the cell cannot be laid out until the rectangle is.
///
/// Only the width is taken, because only the width changes the answer — height
/// feeds the anchor and `@vertOverflow`, both of which move or drop lines that
/// are already stacked. So this measures exactly what [`layout_shape_body`]
/// will stack, without needing a provisional height to hand it.
pub fn measure_shape_body(
    blocks: &[LayoutBlock],
    extent_width: Pt,
    body_pr: Option<&crate::model::BodyProperties>,
    line_height: Pt,
) -> Pt {
    let insets = BodyInsets::resolve(body_pr);
    let content_width = (extent_width - insets.left - insets.right).max(Pt::ZERO);
    if content_width <= Pt::ZERO {
        // Nothing can wrap here, so the body contributes only its own insets.
        // Returning zero would let a zero-width column collapse a row that
        // still has to draw its borders.
        return insets.top + insets.bottom;
    }
    let result = crate::render::layout::section::stack_blocks(
        blocks,
        content_width,
        line_height,
        None,
        PageParity::Odd,
    );
    insets.top + result.height + insets.bottom
}

/// Lay `blocks` out inside a shape of size `extent`, honouring the body's
/// insets, anchor and `@vertOverflow`.
///
/// Returns shape-local Pt draw commands with origin at the shape's top-left.
/// Returns empty when the insets leave no width to wrap in.
///
/// `line_height` is the fallback line height for content that does not state
/// one (empty paragraphs, image-only lines). The caller is responsible for
/// having already applied any `@fontScale` shrink to it — the same shrink must
/// reach the fragments themselves, which are built before this is called.
///
/// Parity is `Odd`: shape text is laid out before the shape is placed, so a
/// §20.4.3.1 `inside`/`outside` float nested in the body has no parity to
/// resolve against. The same structural limit as the table-cell path.
pub fn layout_shape_body(
    blocks: &[LayoutBlock],
    extent: PtSize,
    body_pr: Option<&crate::model::BodyProperties>,
    line_height: Pt,
) -> Vec<DrawCommand> {
    let insets = BodyInsets::resolve(body_pr);
    let content_width = insets.content_width(extent);
    if content_width <= Pt::ZERO {
        return Vec::new();
    }

    let result = crate::render::layout::section::stack_blocks(
        blocks,
        content_width,
        line_height,
        None,
        PageParity::Odd,
    );

    // §20.1.10.60: `bIns` closes off the bottom of the box the body sits in,
    // and `anchor` decides where in that box it sits.
    let content_height = insets.content_height(extent);
    let anchor = BodyAnchor::resolve(body_pr.and_then(|bp| bp.anchor));
    let body_top = insets.top + anchor.offset(content_height, result.height);

    // `@vertOverflow` decides what happens to the part of the body that does
    // not fit. `Overflow` — the spec default — keeps everything, so the common
    // path is untouched.
    let overflow = body_pr.and_then(|bp| bp.vert_overflow).unwrap_or_default();
    let box_bottom = insets.top + content_height;

    let mut commands = Vec::with_capacity(result.commands.len());
    for mut cmd in result.commands {
        cmd.shift(insets.left, body_top);
        if !overflow_keeps(overflow, &cmd, box_bottom) {
            continue;
        }
        commands.push(cmd);
    }
    commands
}

/// Whether `@vertOverflow` keeps `cmd`, given the bottom of the body's box.
///
/// Total over [`TextVertOverflow`] with no catch-all, so a new value of the
/// attribute has to state its own behaviour here.
///
/// **This drops whole commands, which is a line-granular approximation of what
/// Word does.** Word clips at the pixel, so a line straddling the box edge
/// shows its top sliver; here it disappears. Real clipping needs a canvas clip
/// that survives into paint, and draw commands are flattened into one flat
/// per-page list with no scoping — so it would mean a new `DrawCommand`
/// wrapper variant and an arm in every consumer. Dropping is the safe
/// direction (`clip`'s contract is that nothing paints outside the box), and
/// no corpus document asks for `clip` at all — 4 explicit `overflow`, 10
/// `bodyPr` with the attribute absent, zero `clip` or `ellipsis` — so this is
/// worth revisiting only once a real document needs the sliver.
fn overflow_keeps(
    overflow: crate::model::TextVertOverflow,
    cmd: &DrawCommand,
    box_bottom: Pt,
) -> bool {
    use crate::model::TextVertOverflow;

    match overflow {
        TextVertOverflow::Overflow => true,
        // `ellipsis` is `clip` plus an indicator on the last visible line.
        // Choosing that line and refitting it around the ellipsis glyph is a
        // decision this sub-layout does not make, so the indicator is dropped
        // and the clipping is honoured — the same text as `clip`, which is far
        // closer to Word than not clipping at all.
        TextVertOverflow::Clip | TextVertOverflow::Ellipsis => cmd
            .vertical_span()
            .is_none_or(|(_, bottom)| bottom <= box_bottom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BodyProperties, TextAnchoringType, TextVertOverflow};

    /// An `a:bodyPr` that is *present* but declares no insets — the shape the
    /// PPTX corpus is full of.
    fn body_pr() -> BodyProperties {
        BodyProperties {
            rotation: None,
            vert: None,
            wrap: None,
            left_inset: None,
            top_inset: None,
            right_inset: None,
            bottom_inset: None,
            anchor: None,
            vert_overflow: None,
            auto_fit: None,
        }
    }

    #[test]
    fn absent_bodypr_takes_spec_default_insets() {
        let insets = BodyInsets::resolve(None);
        assert_eq!(insets.left, Pt::new(DEFAULT_INSET_LR));
        assert_eq!(insets.top, Pt::new(DEFAULT_INSET_TB));
        assert_eq!(insets.right, Pt::new(DEFAULT_INSET_LR));
        assert_eq!(insets.bottom, Pt::new(DEFAULT_INSET_TB));
    }

    #[test]
    fn present_bodypr_with_absent_attrs_still_takes_defaults() {
        // The case that matters: `a:bodyPr` is declared on 100% of PPTX text
        // shapes, but half of them declare no insets. Reading the `None` as
        // zero would widen the body by 14.4pt.
        let insets = BodyInsets::resolve(Some(&body_pr()));
        assert_eq!(insets, BodyInsets::resolve(None));
    }

    #[test]
    fn content_box_floors_at_zero() {
        let insets = BodyInsets::resolve(None);
        let tiny = PtSize {
            width: Pt::new(1.0),
            height: Pt::new(1.0),
        };
        assert_eq!(insets.content_width(tiny), Pt::ZERO);
        assert_eq!(insets.content_height(tiny), Pt::ZERO);
    }

    #[test]
    fn anchor_defaults_to_top_and_degrades_justified() {
        assert_eq!(BodyAnchor::resolve(None), BodyAnchor::Top);
        assert_eq!(
            BodyAnchor::resolve(Some(TextAnchoringType::Center)),
            BodyAnchor::Center
        );
        assert_eq!(
            BodyAnchor::resolve(Some(TextAnchoringType::Justified)),
            BodyAnchor::Top
        );
        assert_eq!(
            BodyAnchor::resolve(Some(TextAnchoringType::Distributed)),
            BodyAnchor::Top
        );
    }

    #[test]
    fn anchor_offset_splits_slack_and_floors_at_zero() {
        let box_h = Pt::new(100.0);
        assert_eq!(BodyAnchor::Top.offset(box_h, Pt::new(40.0)), Pt::ZERO);
        assert_eq!(
            BodyAnchor::Center.offset(box_h, Pt::new(40.0)),
            Pt::new(30.0)
        );
        assert_eq!(
            BodyAnchor::Bottom.offset(box_h, Pt::new(40.0)),
            Pt::new(60.0)
        );
        // Overfull body anchors to the top whatever the attribute says.
        assert_eq!(BodyAnchor::Bottom.offset(box_h, Pt::new(160.0)), Pt::ZERO);
    }

    #[test]
    fn zero_width_body_lays_out_nothing() {
        let tiny = PtSize {
            width: Pt::new(2.0),
            height: Pt::new(50.0),
        };
        assert!(layout_shape_body(&[], tiny, None, Pt::new(12.0)).is_empty());
    }

    #[test]
    fn overflow_default_keeps_everything() {
        assert_eq!(TextVertOverflow::default(), TextVertOverflow::Overflow);
    }
}
