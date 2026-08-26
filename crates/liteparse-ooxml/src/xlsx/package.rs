//! SpreadsheetML package walk: `xl/workbook.xml` → ordered worksheets, joined
//! to the shared-string table and the style table they depend on.
//!
//! Deliberately byte-level, the same split `pptx::package` uses: this stage
//! resolves the part graph and hands back raw XML, and nothing here understands
//! a cell. Everything it stands on — [`PackageContents`], [`resolve_target`],
//! [`rels_path_for`], [`Relationships`] — is the OOXML packaging layer vendored
//! for DOCX, reused unchanged.
//!
//! **Sheets are reached through relationships, never by globbing
//! `xl/worksheets/sheetN.xml`.** Two independent reasons, both real in the
//! corpus: the part name is conventional rather than mandated, and the numeric
//! suffix is not the tab order — `sheet10.xml` sorts before `sheet2.xml`, and a
//! deleted sheet leaves a gap. Only `<sheets>` states the order the user sees.

use crate::docx::error::{ParseError, Result};
use crate::docx::relationships::{RelationshipType, Relationships};
use crate::docx::zip::{PackageContents, part_directory, rels_path_for, resolve_target};
use crate::xlsx::xml::{attr, local_name};

/// A worksheet as the workbook part declares it, before its XML is parsed.
#[derive(Clone, Debug)]
pub struct SheetEntry {
    pub name: String,
    /// `state="hidden"` / `"veryHidden"`. A hidden sheet is usually scratch
    /// data or a lookup table, so this is the emitter's decision to make, not
    /// the reader's — it is reported rather than filtered here.
    pub visible: bool,
    /// Package-normalized path of the worksheet part.
    pub path: String,
    /// The worksheet's own relationships, for its hyperlinks and drawings.
    /// `r:id` values are scoped to the part that uses them, so these cannot be
    /// looked up in the workbook's set.
    pub rels: Relationships,
    /// True for `<chartsheet>` / `<dialogsheet>` parts, which appear in
    /// `<sheets>` alongside worksheets but hold no cell grid.
    pub is_worksheet: bool,
}

/// The workbook graph, walked but not yet understood.
pub struct WorkbookPackage {
    /// Sheets in **tab order** — `<sheets>`, not part order.
    pub sheets: Vec<SheetEntry>,
    pub shared_strings_xml: Option<Vec<u8>>,
    pub styles_xml: Option<Vec<u8>>,
    /// `xl/theme/theme1.xml`. Needed only for colour: a meaningful share of
    /// the colours `styles.xml` defines are `theme=` references, which
    /// resolve to nothing without it.
    pub theme_xml: Option<Vec<u8>>,
    /// `<workbookPr date1904="1"/>`: the epoch serial dates count from. Excel
    /// for Mac wrote 1904-based workbooks for years, so this is not a
    /// historical curiosity — reading it wrong shifts every date in the file
    /// by 1,462 days.
    pub date1904: bool,
    /// The raw container, kept for drawings and media the emitter may reach.
    pub package: PackageContents,
}

/// Walk an `.xlsx` container into its part graph.
pub fn walk(data: &[u8]) -> Result<WorkbookPackage> {
    let package = PackageContents::from_bytes(data)?;

    // The workbook part is reached through the package root rels rather than
    // assumed to be `xl/workbook.xml` — the path is conventional, not
    // mandated.
    let root_rels = load_rels(&package, "");
    let wb_path = root_rels
        .find_by_type(&RelationshipType::OfficeDocument)
        .map(|r| resolve_target("", &r.target))
        .unwrap_or_else(|| "xl/workbook.xml".to_string());

    let wb_xml = package
        .get_part(&wb_path)
        .ok_or_else(|| ParseError::MissingPart(wb_path.clone()))?
        .to_vec();
    let wb_rels = load_rels(&package, &wb_path);
    let wb_dir = part_directory(&wb_path).to_string();

    let parsed = parse_workbook(&wb_xml)?;

    let mut sheets = Vec::new();
    for entry in parsed.sheets {
        let Some(rel) = entry
            .rel_id
            .as_deref()
            .and_then(|id| wb_rels.find_by_id(id))
        else {
            // A sheet with no resolvable relationship has no part to read. It
            // is skipped rather than fatal, and named in the log so a corpus
            // run can tell "we dropped a sheet" from "the file had none".
            log::warn!("sheet {:?} does not resolve to a part", entry.name);
            continue;
        };
        let path = resolve_target(&wb_dir, &rel.target);
        if package.get_part(&path).is_none() {
            log::warn!("sheet {:?} points at a missing part {}", entry.name, path);
            continue;
        }
        sheets.push(SheetEntry {
            name: entry.name,
            visible: entry.visible,
            rels: load_rels(&package, &path),
            // A chartsheet is declared in `<sheets>` exactly like a worksheet;
            // the only distinguishing signal at this level is the relationship
            // type, since both resolve to a real part.
            is_worksheet: rel.rel_type == RelationshipType::Worksheet,
            path,
        });
    }

    let part_of_type = |ty: &RelationshipType| -> Option<Vec<u8>> {
        let rel = wb_rels.find_by_type(ty)?;
        let path = resolve_target(&wb_dir, &rel.target);
        package.get_part(&path).map(|b| b.to_vec())
    };
    let shared_strings_xml = part_of_type(&RelationshipType::SharedStrings);
    let styles_xml = part_of_type(&RelationshipType::Styles);
    let theme_xml = part_of_type(&RelationshipType::Theme);

    Ok(WorkbookPackage {
        sheets,
        shared_strings_xml,
        styles_xml,
        theme_xml,
        date1904: parsed.date1904,
        package,
    })
}

pub(crate) fn load_rels(package: &PackageContents, part_path: &str) -> Relationships {
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
                // A malformed rels part costs hyperlink/drawing resolution for
                // one sheet, not the workbook.
                log::warn!("failed to parse {}: {}", rels_path, e);
                None
            }
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// `xl/workbook.xml` is parsed by hand rather than with serde, for the reason
// `pptx::package` documents at length: `<sheet name="X" sheetId="1"
// r:id="rId1"/>` carries both `@sheetId` and `@r:id`, and quick-xml's serde
// layer matches attributes on their local name with the prefix dropped. There
// is no serde spelling that keeps `r:id` distinct, and the failure mode is
// silent — binding the numeric id where the relationship id belongs resolves
// nothing and yields an empty workbook.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WorkbookXml {
    sheets: Vec<DeclaredSheet>,
    date1904: bool,
}

struct DeclaredSheet {
    name: String,
    visible: bool,
    rel_id: Option<String>,
}

fn parse_workbook(data: &[u8]) -> Result<WorkbookXml> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(data);
    let mut out = WorkbookXml::default();
    let mut buf = Vec::new();
    // `<sheet>` also appears inside `<externalReferences>`' cached sheet
    // names, which are another workbook's tabs; counting those would invent
    // sheets that have no part here.
    let mut in_sheets = false;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?;
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                match local_name(e.name().as_ref()) {
                    b"sheets" => in_sheets = !matches!(event, Event::Empty(_)),
                    b"sheet" if in_sheets => out.sheets.push(DeclaredSheet {
                        name: attr(e, b"name").unwrap_or_default(),
                        visible: !matches!(
                            attr(e, b"state").as_deref(),
                            Some("hidden" | "veryHidden")
                        ),
                        rel_id: attr(e, b"r:id"),
                    }),
                    b"workbookPr" => {
                        out.date1904 =
                            matches!(attr(e, b"date1904").as_deref(), Some("1" | "true"))
                                || matches!(
                                    // ECMA-376 Strict renamed the attribute.
                                    attr(e, b"dateCompatibility").as_deref(),
                                    Some("0" | "false")
                                );
                    }
                    _ => {}
                }
            }
            Event::End(ref e) if local_name(e.name().as_ref()) == b"sheets" => in_sheets = false,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKBOOK: &str = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <workbookPr date1904="1"/>
  <sheets>
    <sheet name="Summary" sheetId="3" r:id="rId4"/>
    <sheet name="Scratch" sheetId="1" state="hidden" r:id="rId5"/>
  </sheets>
  <externalReferences><externalReference r:id="rId9"/></externalReferences>
  <definedNames><definedName name="Range1">Summary!$A$1</definedName></definedNames>
</workbook>"#;

    /// The regression this module exists for, inherited from `pptx::package`:
    /// `<sheet>` carries both `@sheetId` and `@r:id`, and binding the numeric
    /// one resolves no part at all.
    #[test]
    fn sheets_read_the_rel_id_not_the_numeric_id() {
        let wb = parse_workbook(WORKBOOK.as_bytes()).unwrap();
        let ids: Vec<_> = wb.sheets.iter().map(|s| s.rel_id.clone()).collect();
        assert_eq!(ids, vec![Some("rId4".into()), Some("rId5".into())]);
    }

    #[test]
    fn tab_order_and_names_come_from_the_sheets_list() {
        let wb = parse_workbook(WORKBOOK.as_bytes()).unwrap();
        // sheetId 3 is written first, so it is the first tab. Sorting by
        // sheetId — or by part name — would reorder the workbook.
        assert_eq!(
            wb.sheets
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Summary", "Scratch"]
        );
    }

    #[test]
    fn hidden_state_is_reported_not_filtered() {
        let wb = parse_workbook(WORKBOOK.as_bytes()).unwrap();
        assert!(wb.sheets[0].visible);
        assert!(!wb.sheets[1].visible);
    }

    /// An external reference caches the *other* workbook's sheet names in
    /// `<sheet>` elements. They have no part here, so counting them would
    /// invent sheets that then fail to resolve.
    #[test]
    fn external_reference_sheets_are_not_counted() {
        let xml = r#"<workbook xmlns:r="r">
          <sheets><sheet name="Real" sheetId="1" r:id="rId1"/></sheets>
          <externalReferences><externalBook><sheetNames>
            <sheet val="Other"/><sheet val="Another"/>
          </sheetNames></externalBook></externalReferences>
        </workbook>"#;
        assert_eq!(parse_workbook(xml.as_bytes()).unwrap().sheets.len(), 1);
    }

    /// Mac Excel workbooks use the 1904 epoch; getting this wrong shifts
    /// every date in them by 1,462 days.
    #[test]
    fn the_1904_epoch_flag_is_read() {
        assert!(parse_workbook(WORKBOOK.as_bytes()).unwrap().date1904);
        let xml = r#"<workbook><workbookPr/><sheets/></workbook>"#;
        assert!(!parse_workbook(xml.as_bytes()).unwrap().date1904);
        let xml = r#"<workbook><sheets/></workbook>"#;
        assert!(!parse_workbook(xml.as_bytes()).unwrap().date1904);
    }

    #[test]
    fn a_sheet_with_no_rel_id_is_kept_out_of_the_list_by_the_walk() {
        // parse_workbook keeps it (it is what the file says); `walk` is what
        // drops it, because it has no part.
        let xml = r#"<workbook><sheets><sheet name="Orphan" sheetId="1"/></sheets></workbook>"#;
        let wb = parse_workbook(xml.as_bytes()).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        assert!(wb.sheets[0].rel_id.is_none());
    }
}
