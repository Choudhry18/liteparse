//! PresentationML package walk: `ppt/presentation.xml` → ordered slides,
//! each joined to its layout, master and notes part.
//!
//! This stage is deliberately *byte-level*. It resolves the part graph and
//! hands back raw XML plus the relationships needed to resolve `r:embed` /
//! `r:id` references later. Nothing here understands a shape. Keeping the
//! graph walk separate from the schema layer is what makes it testable
//! against a corpus without a working reader.
//!
//! Everything it stands on — [`PackageContents`], [`resolve_target`],
//! [`rels_path_for`], [`Relationships`] — is the OOXML packaging layer
//! vendored for DOCX, reused here unchanged. Only [`RelationshipType`]'s
//! PresentationML variants were added.

use crate::docx::error::{ParseError, Result};
use crate::docx::relationships::{RelationshipType, Relationships};
use crate::docx::zip::{PackageContents, part_directory, rels_path_for, resolve_target};

/// EMU per point (§20.1.2.1: 914400 EMU per inch, 72 pt per inch).
pub const EMU_PER_POINT: f64 = 12700.0;

/// The presentation-level facts every slide is laid out against.
#[derive(Clone, Debug)]
pub struct PresentationInfo {
    /// `<p:sldSz>` in EMU. Every slide in a deck shares one size.
    pub slide_width_emu: i64,
    pub slide_height_emu: i64,
    /// `<p:notesSz>` in EMU, when declared.
    pub notes_width_emu: Option<i64>,
    pub notes_height_emu: Option<i64>,
}

impl PresentationInfo {
    /// Slide size in points, the unit the layout engine and `TextItem` use.
    pub fn slide_size_pt(&self) -> (f64, f64) {
        (
            self.slide_width_emu as f64 / EMU_PER_POINT,
            self.slide_height_emu as f64 / EMU_PER_POINT,
        )
    }
}

/// One part's raw bytes plus the relationships declared *by that part*.
///
/// The rels travel with the bytes because `r:id` values are scoped to the
/// part that uses them: a slide's `rId2` and its layout's `rId2` are
/// unrelated. Resolving an image or hyperlink requires knowing which part
/// the reference came from.
#[derive(Clone, Debug)]
pub struct Part {
    /// Package-normalized path (lowercased, no leading `/`).
    pub path: String,
    pub xml: Vec<u8>,
    pub rels: Relationships,
}

impl Part {
    /// Resolve an `r:id` declared by this part to an absolute package path.
    ///
    /// Returns `None` for external targets (hyperlinks), which have no part.
    pub fn resolve_rel(&self, rel_id: &str) -> Option<String> {
        let rel = self.rels.find_by_id(rel_id)?;
        if rel.target_mode == crate::docx::relationships::TargetMode::External {
            return None;
        }
        Some(resolve_target(part_directory(&self.path), &rel.target))
    }
}

/// A slide joined to the parts it inherits from.
///
/// `layout` and `master` are `Option` because the cascade must degrade
/// rather than fail: a slide with no reachable layout still has its own
/// directly-applied text and geometry, and refusing to parse it would be
/// the fail-closed behaviour this vendor has already had to retire four
/// times (see `ATTRIBUTION.md`).
#[derive(Clone, Debug)]
pub struct SlideParts {
    pub slide: Part,
    pub layout: Option<Part>,
    pub master: Option<Part>,
    pub notes: Option<Part>,
}

/// The whole deck, walked but not yet understood.
///
/// Deliberately neither `Clone` nor `Debug`: it owns every media part, so a
/// clone copies the whole deck's images (390 MB across the corpus) and a
/// debug-print dumps them.
pub struct PresentationPackage {
    pub info: PresentationInfo,
    /// Slides in **presentation order** — `<p:sldIdLst>`, not part order.
    pub slides: Vec<SlideParts>,
    /// `ppt/theme/themeN.xml` for the first master, when present.
    pub theme: Option<Vec<u8>>,
    /// The notes master, shared by every notes slide.
    pub notes_master: Option<Part>,
    /// Raw media parts, keyed by normalized package path.
    pub package: PackageContents,
}

/// Walk a `.pptx` container into its part graph.
pub fn walk(data: &[u8]) -> Result<PresentationPackage> {
    let package = PackageContents::from_bytes(data)?;

    // The presentation part is reached through the package root rels rather
    // than assumed to be `ppt/presentation.xml`. The path is conventional,
    // not mandated, and the 9 docProps-less decks in the corpus are already
    // proof that "every producer follows convention" is a bad bet.
    let root_rels = load_rels(&package, "");
    let pres_path = root_rels
        .find_by_type(&RelationshipType::OfficeDocument)
        .map(|r| resolve_target("", &r.target))
        .unwrap_or_else(|| "ppt/presentation.xml".to_string());

    let pres_xml = package
        .get_part(&pres_path)
        .ok_or_else(|| ParseError::MissingPart(pres_path.clone()))?
        .to_vec();
    let pres_rels = load_rels(&package, &pres_path);
    let pres = Part {
        path: pres_path,
        xml: pres_xml,
        rels: pres_rels,
    };

    let parsed = parse_presentation(&pres.xml)?;
    let info = PresentationInfo {
        // §19.2.1.39 makes sldSz optional in schema terms; default to 4:3
        // (9144000 × 6858000 EMU) rather than error, so a malformed deck
        // still yields text at a plausible scale.
        slide_width_emu: parsed.slide_size.map_or(9_144_000, |s| s.0),
        slide_height_emu: parsed.slide_size.map_or(6_858_000, |s| s.1),
        notes_width_emu: parsed.notes_size.map(|s| s.0),
        notes_height_emu: parsed.notes_size.map(|s| s.1),
    };

    let notes_master = pres
        .resolve_rel_of_type(&RelationshipType::NotesMaster)
        .and_then(|p| load_part(&package, &p));

    let mut slides = Vec::new();
    for rel_id in parsed.slide_rel_ids {
        // Order is driven by sldIdLst. Part names are NOT a valid ordering:
        // `slide10.xml` sorts before `slide2.xml` lexically, and hidden or
        // deleted slides leave gaps in the numbering.
        let Some(path) = pres.resolve_rel(&rel_id) else {
            log::warn!("slide r:id {} does not resolve to a part", rel_id);
            continue;
        };
        let Some(slide) = load_part(&package, &path) else {
            log::warn!("slide part missing: {}", path);
            continue;
        };

        let layout = slide
            .resolve_rel_of_type(&RelationshipType::SlideLayout)
            .and_then(|p| load_part(&package, &p));
        // The master hangs off the *layout*, not the slide — a slide has no
        // direct master relationship.
        let master = layout
            .as_ref()
            .and_then(|l| l.resolve_rel_of_type(&RelationshipType::SlideMaster))
            .and_then(|p| load_part(&package, &p));
        let notes = slide
            .resolve_rel_of_type(&RelationshipType::NotesSlide)
            .and_then(|p| load_part(&package, &p));

        slides.push(SlideParts {
            slide,
            layout,
            master,
            notes,
        });
    }

    // Theme comes off the first master. Decks with multiple masters can
    // carry multiple themes; resolving per-master is a colour-fidelity
    // concern deferred until `phClr` substitution lands.
    let theme = slides
        .iter()
        .find_map(|s| s.master.as_ref())
        .and_then(|m| m.resolve_rel_of_type(&RelationshipType::Theme))
        .and_then(|p| package.get_part(&p).map(|b| b.to_vec()));

    Ok(PresentationPackage {
        info,
        slides,
        theme,
        notes_master,
        package,
    })
}

impl Part {
    fn resolve_rel_of_type(&self, ty: &RelationshipType) -> Option<String> {
        let rel = self.rels.find_by_type(ty)?;
        Some(resolve_target(part_directory(&self.path), &rel.target))
    }
}

/// Load a part plus its own `.rels`. A part with no `.rels` file is normal
/// (most layouts have one, many slides do not), so an absent rels file
/// yields an empty set rather than an error.
fn load_part(package: &PackageContents, path: &str) -> Option<Part> {
    let xml = package.get_part(path)?.to_vec();
    let rels = load_rels(package, path);
    Some(Part {
        path: path.to_string(),
        xml,
        rels,
    })
}

fn load_rels(package: &PackageContents, part_path: &str) -> Relationships {
    let rels_path = if part_path.is_empty() {
        "_rels/.rels".to_string()
    } else {
        rels_path_for(part_path)
    };
    package
        .get_part(&rels_path)
        .and_then(|b| match Relationships::parse(b) {
            Ok(r) => Some(r),
            Err(e) => {
                // A malformed rels part costs us image/hyperlink/layout
                // resolution for one part, not the deck.
                log::warn!("failed to parse {}: {}", rels_path, e);
                None
            }
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The presentation part is parsed by hand, NOT with serde. This is the one
// place in the vendor that departs from the serde-schema convention, and the
// reason is a hard limitation rather than taste:
//
// quick-xml's serde layer matches attributes on their **local name with the
// namespace prefix dropped** (the same property `serde_xml.rs` documents for
// *elements*). `<p:sldId id="322" r:id="rId2"/>` therefore presents two
// attributes both named `id`, and deserializing it fails with
// `duplicate field @id` — which is exactly what the corpus reported, 45/45.
//
// There is no serde spelling that distinguishes them: `#[serde(rename =
// "@r:id")]` never matches because the prefix is already gone, and
// `alias = "@id"` would bind the *numeric slide id* to the relationship id —
// silent corruption, the class `ATTRIBUTION.md` records as this vendor's
// recurring trap. (`blip@r:embed` gets away with its alias only because no
// competing `@embed` exists on that element.)
//
// So the r:id-bearing lists are read with a namespace-aware reader that
// compares the *qualified* name. The presentation part is small and flat, so
// the cost is ~50 lines and it stays confined to this module.
//
// The same collision exists on `p:sldMasterId` and `p:sldLayoutId`; anything
// added later that reads those must come through here, not through serde.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PresentationXml {
    slide_rel_ids: Vec<String>,
    slide_size: Option<(i64, i64)>,
    notes_size: Option<(i64, i64)>,
}

fn parse_presentation(data: &[u8]) -> Result<PresentationXml> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(data);
    let mut out = PresentationXml::default();
    let mut buf = Vec::new();
    // `sldId` also appears inside `<p:custShowLst>`, whose entries point at
    // slides already listed in sldIdLst. Tracking the enclosing list keeps a
    // custom show from duplicating or reordering the deck.
    let mut in_slide_id_lst = false;

    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?;
        match ev {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => match local_name(e.name().as_ref()) {
                b"sldIdLst" => in_slide_id_lst = !matches!(ev, Event::Empty(_)),
                b"sldId" if in_slide_id_lst => {
                    if let Some(id) = qualified_attr(e, b"r:id") {
                        out.slide_rel_ids.push(id);
                    } else {
                        log::warn!("p:sldId without r:id; slide skipped");
                    }
                }
                b"sldSz" => out.slide_size = size_attrs(e),
                b"notesSz" => out.notes_size = size_attrs(e),
                _ => {}
            },
            Event::End(ref e) => {
                if local_name(e.name().as_ref()) == b"sldIdLst" {
                    in_slide_id_lst = false;
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Strip a namespace prefix from a qualified name (`p:sldId` → `sldId`).
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Read an attribute by its **qualified** name, so `r:id` and `id` stay
/// distinct. Malformed attributes are skipped rather than fatal, matching
/// the fail-open posture of `docx/parse/primitives/lenient.rs`.
fn qualified_attr(e: &quick_xml::events::BytesStart<'_>, want: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.as_ref() == want).then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}

fn size_attrs(e: &quick_xml::events::BytesStart<'_>) -> Option<(i64, i64)> {
    let cx = qualified_attr(e, b"cx")?.parse().ok()?;
    let cy = qualified_attr(e, b"cy")?.parse().ok()?;
    Some((cx, cy))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRES: &str = r#"<?xml version="1.0"?>
<p:presentation xmlns:p="p" xmlns:r="r">
  <p:sldMasterIdLst><p:sldMasterId id="2147483660" r:id="rId1"/></p:sldMasterIdLst>
  <p:sldIdLst>
    <p:sldId id="322" r:id="rId2"/>
    <p:sldId id="260" r:id="rId3"/>
  </p:sldIdLst>
  <p:sldSz cx="12192000" cy="6858000"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#;

    /// The regression this module exists for. `p:sldId` carries both `@id`
    /// and `@r:id`; quick-xml's serde layer drops namespace prefixes on
    /// attributes, so a serde schema sees `duplicate field @id` and the
    /// whole deck fails to parse (observed: 45/45 of the corpus).
    ///
    /// Two things must hold, and the second is the one that matters: the
    /// r:id must be read, AND the numeric `@id` must never stand in for it.
    #[test]
    fn slide_ids_read_r_id_not_the_numeric_id() {
        let p = parse_presentation(PRES.as_bytes()).unwrap();
        assert_eq!(p.slide_rel_ids, vec!["rId2", "rId3"]);
        // If `@id` ever leaks into this list, resolution silently yields no
        // part and the deck comes out empty rather than erroring.
        assert!(!p.slide_rel_ids.iter().any(|s| s == "322" || s == "260"));
    }

    /// The master list has the same `@id`/`@r:id` shape and sits directly
    /// above sldIdLst. Its entries must not be mistaken for slides.
    #[test]
    fn master_ids_do_not_leak_into_the_slide_list() {
        let p = parse_presentation(PRES.as_bytes()).unwrap();
        assert_eq!(p.slide_rel_ids.len(), 2);
        assert!(!p.slide_rel_ids.iter().any(|s| s == "rId1"));
    }

    /// A custom show references slides already in sldIdLst; counting its
    /// entries would duplicate and reorder the deck.
    #[test]
    fn custom_show_slide_ids_are_ignored() {
        let xml = r#"<p:presentation xmlns:p="p" xmlns:r="r">
  <p:sldIdLst><p:sldId id="1" r:id="rId2"/></p:sldIdLst>
  <p:custShowLst><p:custShow name="s" id="0"><p:sldLst>
    <p:sldId id="1" r:id="rId2"/><p:sldId id="1" r:id="rId2"/>
  </p:sldLst></p:custShow></p:custShowLst>
</p:presentation>"#;
        assert_eq!(
            parse_presentation(xml.as_bytes()).unwrap().slide_rel_ids,
            vec!["rId2"]
        );
    }

    #[test]
    fn sizes_are_read_and_converted() {
        let p = parse_presentation(PRES.as_bytes()).unwrap();
        assert_eq!(p.slide_size, Some((12_192_000, 6_858_000)));
        assert_eq!(p.notes_size, Some((6_858_000, 9_144_000)));
        let info = PresentationInfo {
            slide_width_emu: 12_192_000,
            slide_height_emu: 6_858_000,
            notes_width_emu: None,
            notes_height_emu: None,
        };
        // 12192000 / 12700 = 960 pt, the 16:9 widescreen default.
        assert_eq!(info.slide_size_pt(), (960.0, 540.0));
    }

    /// A deck with no `sldSz` still parses, at a plausible scale, rather
    /// than erroring — the fail-open posture the rest of the vendor uses.
    #[test]
    fn missing_slide_size_falls_back_rather_than_failing() {
        let xml = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst/></p:presentation>"#;
        let p = parse_presentation(xml.as_bytes()).unwrap();
        assert_eq!(p.slide_size, None);
        assert!(p.slide_rel_ids.is_empty());
    }

    /// An `sldId` missing its r:id is skipped, not fatal, and does not
    /// shift the positions of the slides around it.
    #[test]
    fn slide_without_rel_id_is_skipped() {
        let xml = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>
  <p:sldId id="1" r:id="rId2"/><p:sldId id="2"/><p:sldId id="3" r:id="rId4"/>
</p:sldIdLst></p:presentation>"#;
        assert_eq!(
            parse_presentation(xml.as_bytes()).unwrap().slide_rel_ids,
            vec!["rId2", "rId4"]
        );
    }
}
