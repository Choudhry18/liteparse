//! The geometry pass (§19.3.1.22, §20.1.7.6) — group composition and rotation.
//!
//! [`crate::pptx::shapes`] preserves group nesting rather than flattening it,
//! and [`crate::pptx::cascade`] fills in the rectangles a shape inherits from
//! its layout. Both leave every transform **as the file declares it**, which for
//! anything inside a `p:grpSp` means it is expressed in that group's *child
//! coordinate space* and does not describe a position on the slide. This module
//! maps them.
//!
//! A group declares its own rectangle (`a:off`/`a:ext`) and a child space
//! (`a:chOff`/`a:chExt`). The mapping is
//!
//! ```text
//! slide = off + (child - chOff) * (ext / chExt)
//! ```
//!
//! then the group's own rotation and flips apply about the group's centre.
//! Frames compose down the tree, so this is an affine map accumulated
//! pre-order — which is exactly the order [`Shape::visit`] documents.
//!
//! ## What the corpus says this has to handle
//!
//! Census over the 45-deck corpus (`pptx_geometry_census`), 9,272 top-level
//! slide shapes and 3,704 inside a group:
//!
//! | question | answer | consequence |
//! |---|---|---|
//! | is the child space ever absent or partial? | **0 / 0 / 0** — `chOff`, `chExt`, and the xor are all never missing on 575 groups | the mapping is a **total function**; no defaulting rules, and a missing term is a bug rather than a case |
//! | is `chExt` ever zero? | **0** | the divide is safe — but it is still guarded, because a zero would produce infinities that propagate silently |
//! | is the scale ever not 1? | **342 of 575 (59%)** | the scale is load-bearing, not a rounding term |
//! | is it ever non-uniform? | **154 (27%)**, `sx/sy` differing by up to 2.18x | two scalars, never one |
//! | how extreme does it get? | **1961x** — a 1696x2204 EMU child space onto a 3.3M x 4.1M rect | dropping the scale does not nudge those shapes, it collapses them to invisibility |
//! | how many *text* shapes are affected? | 457 inside groups, **111 under a non-unit scale** | 111 are wrong by a factor today, 346 by an offset |
//! | do groups rotate? | **28**, of which **3** contain a rotated child | angle composition is real but rare |
//! | do groups flip? | **7** | must be modelled: a flip mirrors the child space, changing the *sign* of the map |
//! | do shapes flip? | 497, but only **9 carry text** | flips are a picture/connector concern |
//! | is rotation ever oblique? | **302 of 759** (132 with text) | the angle must be carried; an axis-aligned box alone loses the reading direction |
//! | is `chExt` a clip? | **no** — 102 children legally sit outside it | never treat it as bounds |
//!
//! ## Why the output is a separate field
//!
//! [`Shape::slide_rect`] is filled here; `Shape::transform` keeps the declared
//! child-space values. The two are different facts about the document, the same
//! way [`Shape::transform_inherited`] and `SizeSource` distinguish a resolved
//! value from a fallen-back one. It also keeps `bench/pptx_corpus/geometry_probe.py`
//! honest: it checks declared EMU against the source XML, and would have nothing
//! to check against if this pass overwrote them.

use crate::model::dimension::{Dimension, Emu, SixtieThousandthDeg};
use crate::model::geometry::{Offset, Rect, Size};

use super::shapes::{Shape, ShapeKind};

/// 60,000ths of a degree in a full turn (§20.1.10.3 ST_Angle).
const FULL_TURN: i64 = 21_600_000;

/// A shape's resolved position on the slide.
///
/// `rect` is the **unrotated** box: the rectangle the shape would occupy with
/// `rotation` at zero. Rotation is carried beside it rather than folded into an
/// axis-aligned bounding box, because 132 corpus shapes carry text at an oblique
/// angle and their reading direction is not recoverable from a bbox. A consumer
/// that only wants a bbox can call [`SlideRect::bounding_box`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlideRect {
    pub rect: Rect<Emu>,
    /// Composed rotation about `rect`'s centre — this shape's own angle plus
    /// every enclosing group's. Normalized to `[0, FULL_TURN)`.
    pub rotation: Dimension<SixtieThousandthDeg>,
    pub flip_h: bool,
    pub flip_v: bool,
    /// True when an enclosing group rotated the shape while a *non-uniform*
    /// scale was in effect. The composed map is then a genuine skew, which
    /// `rotation` + `rect` cannot express exactly, so this records that the
    /// values are the closest similarity rather than the exact map.
    ///
    /// Measured at **0** on the corpus. It exists so that if a deck ever hits
    /// the case, the probe says so instead of the numbers being quietly a few
    /// EMU wrong.
    pub skewed: bool,
}

impl SlideRect {
    /// The axis-aligned bounding box, with `rotation` applied about the centre.
    ///
    /// Exact for right angles, and a true bound (never a crop) for oblique
    /// ones.
    pub fn bounding_box(&self) -> Rect<Emu> {
        let rot = self.rotation.raw();
        if rot == 0 {
            return self.rect;
        }
        let theta = (rot as f64 / 60_000.0).to_radians();
        let (sin, cos) = theta.sin_cos();
        let (w, h) = (
            self.rect.size.width.raw() as f64,
            self.rect.size.height.raw() as f64,
        );
        let bw = w * cos.abs() + h * sin.abs();
        let bh = w * sin.abs() + h * cos.abs();
        let cx = self.rect.origin.x.raw() as f64 + w / 2.0;
        let cy = self.rect.origin.y.raw() as f64 + h / 2.0;
        rect_from_centre(cx, cy, bw, bh)
    }
}

/// What the pass did, so a corpus probe can grade it rather than trust it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GeometryStats {
    /// Shapes that got a `slide_rect`.
    pub positioned: usize,
    /// Shapes with no transform at all — after the cascade has run, this should
    /// only ever be a placeholder nothing could resolve.
    pub unpositioned: usize,
    /// Groups whose frame was composed onto children.
    pub groups: usize,
    /// Groups declaring a transform but no child space. The corpus says 0; a
    /// non-zero here means a deck exists that the census did not predict, and
    /// the identity child space used as a fallback is a guess.
    pub groups_without_child_space: usize,
    /// Groups whose `chExt` is zero in an axis. The corpus says 0; the scale
    /// falls back to 1 rather than producing an infinity.
    pub groups_with_zero_child_extent: usize,
    /// See [`SlideRect::skewed`].
    pub skewed: usize,
}

/// Resolve every shape's position on the slide, in place.
///
/// Run **after** [`crate::pptx::apply_inherited_geometry`]: a placeholder whose
/// rectangle only exists on its master has no transform to compose until the
/// cascade has filled it in.
pub fn apply_slide_geometry(shapes: &mut [Shape]) -> GeometryStats {
    let mut stats = GeometryStats::default();
    for shape in shapes {
        compose(shape, Affine::IDENTITY, &mut stats);
    }
    stats
}

/// Pre-order: a group's own rectangle resolves in its parent's frame, and only
/// then does its child frame exist for its descendants.
fn compose(shape: &mut Shape, frame: Affine, stats: &mut GeometryStats) {
    let local = shape.transform;

    // The shape's own box, in whatever space its parent established.
    let own = local.and_then(|t| match (t.offset, t.extent) {
        (Some(off), Some(ext)) => Some((off, ext)),
        // A partial transform is measured at 0 by `pptx_cascade_probe`, which
        // gates on it; treating it as absent here keeps the two consistent
        // rather than inventing the missing half.
        _ => None,
    });

    let placed = own.map(|(off, ext)| frame.place(&off, &ext, local, stats));
    if let Some(placed) = placed {
        if placed.skewed {
            stats.skewed += 1;
        }
        stats.positioned += 1;
        shape.slide_rect = Some(placed);
    } else {
        stats.unpositioned += 1;
        shape.slide_rect = None;
    }

    let ShapeKind::Group(group) = &mut shape.kind else {
        return;
    };
    stats.groups += 1;

    // A group with no rectangle of its own gives its children nothing to map
    // onto; pass the parent frame straight through rather than collapsing them
    // to the origin.
    let Some((off, ext)) = own else {
        for child in &mut group.children {
            compose(child, frame, stats);
        }
        return;
    };

    let (ch_off, ch_ext) = match (group.child_offset, group.child_extent) {
        (Some(o), Some(e)) => (o, e),
        _ => {
            stats.groups_without_child_space += 1;
            // Identity: children are already in the parent's space. This is a
            // guess, which is why it is counted.
            (off, ext)
        }
    };

    let (cw, ch) = (ch_ext.width.raw() as f64, ch_ext.height.raw() as f64);
    let (sx, sy) = if cw == 0.0 || ch == 0.0 {
        stats.groups_with_zero_child_extent += 1;
        (1.0, 1.0)
    } else {
        (ext.width.raw() as f64 / cw, ext.height.raw() as f64 / ch)
    };

    let child_frame = frame.compose_group(&off, &ext, &ch_off, sx, sy, shape.transform);
    for child in &mut group.children {
        compose(child, child_frame, stats);
    }
}

// ── the affine map ───────────────────────────────────────────────────────────

/// A 2x3 affine map from some shape's coordinate space to the slide's.
///
/// Composed as a matrix rather than as a translation-plus-scale pair because a
/// rotated group makes the two non-commutative: `p = M * c` is the only form
/// that survives nesting to depth 5 without special cases.
///
/// ```text
/// x' = a*x + c*y + e
/// y' = b*x + d*y + f
/// ```
#[derive(Clone, Copy, Debug)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `self ∘ inner` — apply `inner` first, then `self`.
    fn then(&self, inner: &Affine) -> Affine {
        Affine {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    /// Scale factors and rotation carried by this map, by QR-style
    /// decomposition of the basis vectors.
    ///
    /// Returns `(sx, sy, rotation_radians, flip_v, skew)` where `skew` is the
    /// component of the y-basis lying along the x-basis — zero for any
    /// composition of rotation, axis-aligned scale and flips.
    fn decompose(&self) -> (f64, f64, f64, bool, f64) {
        let sx = self.a.hypot(self.b);
        if sx == 0.0 {
            return (0.0, self.c.hypot(self.d), 0.0, false, 0.0);
        }
        let rotation = self.b.atan2(self.a);
        let det = self.a * self.d - self.b * self.c;
        let sy_signed = det / sx;
        let skew = (self.a * self.c + self.b * self.d) / sx;
        (sx, sy_signed.abs(), rotation, sy_signed < 0.0, skew)
    }

    /// Place a shape's declared box, expressed in this frame's space, onto the
    /// slide.
    fn place(
        &self,
        off: &Offset<Emu>,
        ext: &Size<Emu>,
        local: Option<crate::model::Transform2D>,
        _stats: &mut GeometryStats,
    ) -> SlideRect {
        let (w, h) = (ext.width.raw() as f64, ext.height.raw() as f64);
        // Rotation is about the shape's own centre, so the centre is the one
        // point unaffected by the shape's own angle — map that, and the frame's
        // rotation is the only thing that can move it.
        let (cx, cy) = self.apply(off.x.raw() as f64 + w / 2.0, off.y.raw() as f64 + h / 2.0);

        let (fsx, fsy, frame_rot, frame_flip_v, skew) = self.decompose();
        let own_rot = local.and_then(|t| t.rotation).map(|r| r.raw()).unwrap_or(0);
        let rotation = normalize_angle(own_rot + radians_to_ooxml(frame_rot));

        let scaled_w = w * fsx;
        let scaled_h = h * fsy;

        // A skew only arises when a rotating frame sits under a non-uniform
        // scale. Judge it relative to the frame's own scale so the threshold
        // means "off-square by a part in a million", not "off by an EMU".
        let skewed = skew.abs() > fsx.max(fsy) * 1e-6;

        SlideRect {
            rect: rect_from_centre(cx, cy, scaled_w, scaled_h),
            rotation: Dimension::new(rotation),
            flip_h: local.and_then(|t| t.flip_h).unwrap_or(false),
            flip_v: local.and_then(|t| t.flip_v).unwrap_or(false) ^ frame_flip_v,
            skewed,
        }
    }

    /// The frame a group establishes for its children: map the child space onto
    /// the group's rectangle, then apply the group's own rotation and flips
    /// about that rectangle's centre.
    fn compose_group(
        &self,
        off: &Offset<Emu>,
        ext: &Size<Emu>,
        ch_off: &Offset<Emu>,
        sx: f64,
        sy: f64,
        local: Option<crate::model::Transform2D>,
    ) -> Affine {
        // child space -> the group's own rectangle, in the parent's space
        let map = Affine {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: off.x.raw() as f64 - ch_off.x.raw() as f64 * sx,
            f: off.y.raw() as f64 - ch_off.y.raw() as f64 * sy,
        };

        let rot = local.and_then(|t| t.rotation).map(|r| r.raw()).unwrap_or(0);
        let flip_h = local.and_then(|t| t.flip_h).unwrap_or(false);
        let flip_v = local.and_then(|t| t.flip_v).unwrap_or(false);

        let inner = if rot == 0 && !flip_h && !flip_v {
            map
        } else {
            // Both act about the group's centre in the parent's space, so
            // translate to the origin, transform, translate back.
            let gcx = off.x.raw() as f64 + ext.width.raw() as f64 / 2.0;
            let gcy = off.y.raw() as f64 + ext.height.raw() as f64 / 2.0;
            let theta = (rot as f64 / 60_000.0).to_radians();
            let (sin, cos) = theta.sin_cos();
            let (fx, fy) = (
                if flip_h { -1.0 } else { 1.0 },
                if flip_v { -1.0 } else { 1.0 },
            );
            // R * F, then re-centred.
            let a = cos * fx;
            let b = sin * fx;
            let c = -sin * fy;
            let d = cos * fy;
            let about_centre = Affine {
                a,
                b,
                c,
                d,
                e: gcx - (a * gcx + c * gcy),
                f: gcy - (b * gcx + d * gcy),
            };
            about_centre.then(&map)
        };

        self.then(&inner)
    }
}

fn rect_from_centre(cx: f64, cy: f64, w: f64, h: f64) -> Rect<Emu> {
    Rect::new(
        Offset::new(
            Dimension::new((cx - w / 2.0).round() as i64),
            Dimension::new((cy - h / 2.0).round() as i64),
        ),
        Size::new(
            Dimension::new(w.round() as i64),
            Dimension::new(h.round() as i64),
        ),
    )
}

fn radians_to_ooxml(theta: f64) -> i64 {
    (theta.to_degrees() * 60_000.0).round() as i64
}

fn normalize_angle(raw: i64) -> i64 {
    raw.rem_euclid(FULL_TURN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pptx::parse_shape_tree;

    /// `<a:xfrm>` with optional rotation and flips, as a shape declares it.
    fn xfrm(x: i64, y: i64, cx: i64, cy: i64, extra: &str) -> String {
        format!(r#"<a:xfrm {extra}><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>"#)
    }

    fn sp(x: i64, y: i64, cx: i64, cy: i64, extra: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/><p:nvPr/></p:nvSpPr>
               <p:spPr>{}</p:spPr></p:sp>"#,
            xfrm(x, y, cx, cy, extra)
        )
    }

    /// A `p:grpSp` whose `<a:xfrm>` carries both its own rect and the child
    /// space, which is the one element where the two live together.
    fn grp(
        (x, y, cx, cy): (i64, i64, i64, i64),
        ch: Option<(i64, i64, i64, i64)>,
        extra: &str,
        children: &str,
    ) -> String {
        let ch_xml = match ch {
            Some((chx, chy, chcx, chcy)) => {
                format!(r#"<a:chOff x="{chx}" y="{chy}"/><a:chExt cx="{chcx}" cy="{chcy}"/>"#)
            }
            None => String::new(),
        };
        format!(
            r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="2" name="g"/></p:nvGrpSpPr>
               <p:grpSpPr><a:xfrm {extra}><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/>{ch_xml}</a:xfrm></p:grpSpPr>
               {children}</p:grpSp>"#
        )
    }

    fn tree(body: &str) -> Vec<Shape> {
        parse_shape_tree(
            format!(
                r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"#
            )
            .as_bytes(),
        )
        .expect("fixture parses")
    }

    fn kids(shapes: &[Shape]) -> &[Shape] {
        match &shapes[0].kind {
            ShapeKind::Group(g) => &g.children,
            other => panic!("expected a group, got {other:?}"),
        }
    }

    fn rect_of(s: &Shape) -> (i64, i64, i64, i64) {
        let r = s.slide_rect.expect("positioned").rect;
        (
            r.origin.x.raw(),
            r.origin.y.raw(),
            r.size.width.raw(),
            r.size.height.raw(),
        )
    }

    #[test]
    fn top_level_shape_is_unchanged() {
        let mut shapes = tree(&sp(100, 200, 300, 400, ""));
        let stats = apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&shapes[0]), (100, 200, 300, 400));
        assert_eq!(stats.positioned, 1);
        assert_eq!(stats.groups, 0);
    }

    #[test]
    fn identity_child_space_is_a_pure_translation() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            "",
            &sp(10, 10, 50, 50, ""),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (10, 10, 50, 50));
    }

    /// The corpus case that matters: a tiny child space blown up onto a
    /// slide-sized rectangle. Getting this wrong does not shift the shape, it
    /// makes it invisible.
    #[test]
    fn non_unit_scale_multiplies_position_and_size() {
        let mut shapes = tree(&grp(
            (5000, 5000, 10_000, 10_000),
            Some((0, 0, 1000, 1000)),
            "",
            &sp(100, 100, 100, 100, ""),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (6000, 6000, 1000, 1000));
    }

    #[test]
    fn non_uniform_scale_uses_both_axes() {
        // x scales by 2, y by 4 — 27% of corpus groups scale non-uniformly.
        let mut shapes = tree(&grp(
            (0, 0, 2000, 4000),
            Some((0, 0, 1000, 1000)),
            "",
            &sp(0, 0, 100, 100, ""),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (0, 0, 200, 400));
    }

    #[test]
    fn child_offset_is_subtracted_before_scaling() {
        let mut shapes = tree(&grp(
            (7000, 8000, 1000, 1000),
            Some((1000, 1000, 1000, 1000)),
            "",
            &sp(1000, 1000, 100, 100, ""),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (7000, 8000, 100, 100));
    }

    #[test]
    fn nested_groups_multiply_scales() {
        let inner = grp(
            (0, 0, 200, 200),
            Some((0, 0, 100, 100)),
            "",
            &sp(0, 0, 10, 10, ""),
        );
        let mut shapes = tree(&grp(
            (0, 0, 2000, 2000),
            Some((0, 0, 1000, 1000)),
            "",
            &inner,
        ));
        apply_slide_geometry(&mut shapes);
        let inner_shapes = kids(&shapes);
        let ShapeKind::Group(i) = &inner_shapes[0].kind else {
            unreachable!()
        };
        assert_eq!(rect_of(&i.children[0]), (0, 0, 40, 40));
    }

    /// A rotated group moves its children, and the child's own angle adds to
    /// the group's rather than replacing it.
    #[test]
    fn group_rotation_composes_onto_children() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            r#"rot="5400000""#,
            &sp(0, 0, 100, 200, ""),
        ));
        apply_slide_geometry(&mut shapes);
        let child = &kids(&shapes)[0];
        let sr = child.slide_rect.unwrap();
        assert_eq!(sr.rotation.raw(), 5_400_000);
        // The child's centre (50,100) rotates 90° about the group's (500,500).
        let (x, y, w, h) = rect_of(child);
        assert_eq!((x + w / 2, y + h / 2), (900, 50));
        // The unrotated box keeps its own proportions; the angle carries the
        // orientation.
        assert_eq!((w, h), (100, 200));
    }

    #[test]
    fn own_and_group_rotation_add() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            r#"rot="2700000""#,
            &sp(0, 0, 100, 100, r#"rot="2700000""#),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(
            kids(&shapes)[0].slide_rect.unwrap().rotation.raw(),
            5_400_000
        );
    }

    #[test]
    fn rotation_normalizes_into_one_turn() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            r#"rot="5400000""#,
            &sp(0, 0, 100, 100, r#"rot="20000000""#),
        ));
        apply_slide_geometry(&mut shapes);
        let r = kids(&shapes)[0].slide_rect.unwrap().rotation.raw();
        assert!((0..FULL_TURN).contains(&r), "{r} out of range");
        assert_eq!(r, 20_000_000 + 5_400_000 - FULL_TURN);
    }

    /// A flipped group mirrors its child space: the child moves to the other
    /// side of the group's box.
    #[test]
    fn group_flip_mirrors_children() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            r#"flipH="1""#,
            &sp(0, 0, 100, 100, ""),
        ));
        apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (900, 0, 100, 100));
    }

    /// A shape's own flip is its own business — it mirrors the shape's content
    /// inside the same box, so the rect must not move.
    #[test]
    fn own_flip_does_not_move_the_box() {
        let mut shapes = tree(&sp(100, 100, 50, 50, r#"flipH="1""#));
        apply_slide_geometry(&mut shapes);
        let sr = shapes[0].slide_rect.unwrap();
        assert_eq!(rect_of(&shapes[0]), (100, 100, 50, 50));
        assert!(sr.flip_h);
    }

    /// A group flip toggles a child's own flip rather than overwriting it:
    /// mirroring a mirrored shape restores it.
    #[test]
    fn group_flip_toggles_a_child_flip() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 1000, 1000)),
            r#"flipV="1""#,
            &sp(0, 0, 100, 100, r#"flipV="1""#),
        ));
        apply_slide_geometry(&mut shapes);
        assert!(!kids(&shapes)[0].slide_rect.unwrap().flip_v);
    }

    #[test]
    fn a_group_without_a_transform_passes_its_parent_frame_through() {
        let body = format!(
            r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="2" name="g"/></p:nvGrpSpPr>
               <p:grpSpPr/>{}</p:grpSp>"#,
            sp(10, 10, 50, 50, "")
        );
        let mut shapes = tree(&body);
        let stats = apply_slide_geometry(&mut shapes);
        assert_eq!(rect_of(&kids(&shapes)[0]), (10, 10, 50, 50));
        assert_eq!(stats.unpositioned, 1);
    }

    #[test]
    fn zero_child_extent_falls_back_to_unit_scale_and_is_counted() {
        let mut shapes = tree(&grp(
            (0, 0, 1000, 1000),
            Some((0, 0, 0, 0)),
            "",
            &sp(0, 0, 100, 100, ""),
        ));
        let stats = apply_slide_geometry(&mut shapes);
        assert_eq!(stats.groups_with_zero_child_extent, 1);
        // Not an infinity, not a NaN, not a zero-size box.
        assert_eq!(rect_of(&kids(&shapes)[0]), (0, 0, 100, 100));
    }

    #[test]
    fn missing_child_space_is_counted_not_guessed_silently() {
        let mut shapes = tree(&grp(
            (100, 100, 1000, 1000),
            None,
            "",
            &sp(10, 10, 50, 50, ""),
        ));
        let stats = apply_slide_geometry(&mut shapes);
        assert_eq!(stats.groups_without_child_space, 1);
    }

    #[test]
    fn bounding_box_of_a_right_angle_swaps_the_axes() {
        let mut shapes = tree(&sp(0, 0, 100, 200, r#"rot="5400000""#));
        apply_slide_geometry(&mut shapes);
        let bb = shapes[0].slide_rect.unwrap().bounding_box();
        assert_eq!((bb.size.width.raw(), bb.size.height.raw()), (200, 100));
        // Same centre, so the box shifts.
        assert_eq!((bb.origin.x.raw(), bb.origin.y.raw()), (-50, 50));
    }

    #[test]
    fn bounding_box_of_an_unrotated_shape_is_itself() {
        let mut shapes = tree(&sp(7, 9, 100, 200, ""));
        apply_slide_geometry(&mut shapes);
        let sr = shapes[0].slide_rect.unwrap();
        assert_eq!(sr.bounding_box(), sr.rect);
    }

    /// Rotation under a non-uniform scale is a genuine skew, which a rect plus
    /// an angle cannot express. It does not occur on the corpus, so the
    /// contract is that it is *flagged*, not that it is exact.
    #[test]
    fn rotation_under_non_uniform_scale_is_flagged_as_skewed() {
        let inner = grp(
            (0, 0, 100, 100),
            Some((0, 0, 100, 100)),
            r#"rot="2700000""#,
            &sp(0, 0, 10, 10, ""),
        );
        let mut shapes = tree(&grp(
            (0, 0, 2000, 1000),
            Some((0, 0, 1000, 1000)),
            "",
            &inner,
        ));
        let stats = apply_slide_geometry(&mut shapes);
        assert_eq!(stats.skewed, 1);
        let ShapeKind::Group(i) = &kids(&shapes)[0].kind else {
            unreachable!()
        };
        assert!(i.children[0].slide_rect.unwrap().skewed);
    }
}
