use std::path::Path;

use liteparse::conversion::convert_data_to_pdf;
use liteparse::ocr_merge::ComplexityReason;
use liteparse::types::PdfInput;
use liteparse::{LiteParse, LiteParseConfig};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_screenshot_image_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let lit = LiteParse::new(LiteParseConfig::default());
    let results = lit
        .screenshot("../../integration_tests_data/receipt.png", None)
        .await
        .expect("Should be able to screenshot converted image");
    assert_eq!(results.len(), 1);
    assert!(results[0].width > 0);
    assert!(results[0].height > 0);
    assert!(!results[0].image_bytes.is_empty());
}

#[tokio::test]
#[serial]
async fn test_screenshot_pdf_integration() {
    let lit = LiteParse::new(LiteParseConfig::default());
    let results = lit
        .screenshot("../../integration_tests_data/sample.pdf", None)
        .await
        .expect("Should be able to screenshot PDF");
    assert_eq!(results.len(), 1);
    assert!(!results[0].image_bytes.is_empty());
}

#[tokio::test]
async fn test_screenshot_rejects_text_file() {
    let dir = tempfile::tempdir().unwrap();
    let txt_path = dir.path().join("notes.txt");
    std::fs::write(&txt_path, "hello").unwrap();
    let lit = LiteParse::new(LiteParseConfig::default());
    let err = lit
        .screenshot(txt_path.to_str().unwrap(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Cannot screenshot text-based format"));
}

#[tokio::test]
#[serial]
async fn test_convert_data_to_pdf_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let fixture_path = "../../integration_tests_data/receipt.png";
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let (converted, _temps) = convert_data_to_pdf(data, None)
        .await
        .expect("Should be able to convert data to PDF");
    assert!(Path::new(&converted.pdf_path).exists());
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_image_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let fixture_path = "../../integration_tests_data/receipt.png";
    let lit = LiteParse::new(LiteParseConfig::default());
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_office_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    // `sample3.doc` is a renamed Word 2007+ (.docx) file, so byte input has no
    // extension to go on and the container sniff routes it down the native
    // DOCX path when that feature is on. Native pagination is host-dependent
    // (font substitution can spill a page — see NATIVE_OFFICE_PLAN.md), so
    // assert a page range rather than pinning LibreOffice's exact count.
    let fixture_path = "../../integration_tests_data/sample3.doc";
    let lit = LiteParse::new(LiteParseConfig::default());
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert!(
        (2..=3).contains(&parsed.pages.len()),
        "expected 2-3 pages, got {}",
        parsed.pages.len()
    );
    assert!(parsed.pages.iter().all(|p| !p.text_items.is_empty()));
}

#[tokio::test]
#[serial]
async fn test_parse_image_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let lit = LiteParse::new(LiteParseConfig::default());
    let parsed = lit
        .parse("../../integration_tests_data/receipt.png")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}

#[tokio::test]
#[serial]
async fn test_parse_office_doc_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let lit = LiteParse::new(LiteParseConfig::default());
    let parsed = lit
        .parse("../../integration_tests_data/sample3.doc")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 2);
}

#[tokio::test]
#[serial]
async fn test_parse_pdf_integration() {
    let lit = LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        extract_document_metadata: true,
        ..LiteParseConfig::default()
    });
    let parsed = lit
        .parse("../../integration_tests_data/sample.pdf")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
    let doc_meta = parsed.doc_meta.expect("doc_meta requested");
    assert!(doc_meta.file_version.is_some());
    assert_eq!(doc_meta.is_encrypted, Some(false));
    assert!(doc_meta.raw_file_size.is_some_and(|size| size > 0));
    assert!(doc_meta.eof_section_count.is_some_and(|count| count > 0));
    assert_eq!(doc_meta.signature_count, Some(0));
}

/// Provenance is opt-in and stays absent on the default path.
#[tokio::test]
#[serial]
async fn test_doc_meta_absent_unless_requested() {
    let lit = LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        ..LiteParseConfig::default()
    });
    let parsed = lit
        .parse("../../integration_tests_data/sample.pdf")
        .await
        .expect("Should be able to parse");
    assert!(parsed.doc_meta.is_none());
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_pdf_integration() {
    let fixture_path = "../../integration_tests_data/sample.pdf";
    let lit = LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        extract_document_metadata: true,
        ..LiteParseConfig::default()
    });
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let expected_size = data.len() as u64;
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
    assert_eq!(
        parsed.doc_meta.and_then(|meta| meta.raw_file_size),
        Some(expected_size)
    );
}

/// Stress test: many concurrent `parse_input` calls on a multi-threaded
/// tokio runtime through a single `Arc<LiteParse>`. Before the PDFium
/// process-global lock was introduced, this scenario caused malloc
/// double-free / heap corruption because PDFium FFI is not thread-safe.
///
/// We intentionally do **not** use `#[serial]` here — this test must run
/// concurrently with itself (across tasks within the test) to exercise the
/// lock. Other tests in this file are `#[serial]` so they won't race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_parse_does_not_crash() {
    use std::sync::Arc;
    use tokio::task::JoinSet;

    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }

    let lit = Arc::new(LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        quiet: true,
        ..LiteParseConfig::default()
    }));

    let bytes = tokio::fs::read("../../integration_tests_data/sample.pdf")
        .await
        .expect("fixture exists");

    let mut set: JoinSet<usize> = JoinSet::new();
    for _ in 0..16 {
        let lit = lit.clone();
        let bytes = bytes.clone();
        set.spawn(async move {
            let parsed = lit
                .parse_input(PdfInput::Bytes(bytes))
                .await
                .expect("parse should succeed");
            parsed.pages.len()
        });
    }

    let mut total = 0;
    while let Some(joined) = set.join_next().await {
        total += joined.expect("task panicked");
    }
    // 16 tasks × 1 page each
    assert_eq!(total, 16);
}

/// A page whose only text is painted by an annotation's `/AP /N` appearance
/// stream extracts as empty (PDFium tokenizes the page content stream only),
/// so it must be distinguishable from a genuinely blank page. See issue #378.
#[tokio::test]
#[serial]
async fn test_annotation_text_complexity_reason() {
    let lit = LiteParse::new(LiteParseConfig::default());
    let stats = lit
        .is_complex(PdfInput::Path(
            "../../integration_tests_data/annotation_text.pdf".into(),
        ))
        .await
        .expect("is_complex should succeed");

    assert_eq!(stats.len(), 1);
    let page = &stats[0];
    assert_eq!(page.text_length, 0, "annotation text is not extractable");
    assert!(page.needs_ocr);
    assert!(page.reasons.contains(&ComplexityReason::NoText));
    assert!(
        page.reasons.contains(&ComplexityReason::AnnotationText),
        "expected annotation-text, got {:?}",
        page.reasons
    );
}

/// Native DOCX path end-to-end: real geometry per page, links as data, and
/// doc-level markdown byte-identical to the pure structure path
/// (`docx_to_blocks` → `render_blocks`) — the invariant that pins the
/// docx_rules_eval corpus score.
///
/// The byte-identity holds under the default `image_mode` because this
/// fixture's only images are header decorations, which the body walk never
/// reaches — no `Block::Figure`s are emitted. A fixture with body images
/// would need `image_mode: Off` here, since the structure-only path cannot
/// produce the layout-assigned figure ids.
#[cfg(feature = "docx-native")]
#[tokio::test]
#[serial]
async fn test_parse_docx_native_integration() {
    let path = "../../docx_files/legal/uk_parl_media_bill_ia.docx";
    let lit = LiteParse::new(LiteParseConfig {
        output_format: liteparse::config::OutputFormat::Markdown,
        quiet: true,
        ..Default::default()
    });
    let parsed = lit.parse(path).await.expect("native parse succeeds");

    let data = std::fs::read(path).expect("fixture readable");
    // `true` mirrors the default `extract_links` the parse above ran with.
    let blocks = liteparse::office::docx::docx_to_blocks(&data, true).expect("blocks");
    let expected = liteparse::markdown_layout::render_blocks(&blocks);
    assert_eq!(
        parsed.text, expected,
        "native pipeline markdown must match the structure path byte-for-byte"
    );

    assert!(
        parsed.pages.len() > 1,
        "multi-page doc: {}",
        parsed.pages.len()
    );
    assert!(
        parsed.pages.iter().all(|p| !p.text_items.is_empty()),
        "every page carries real text geometry"
    );
    let linked = parsed
        .pages
        .iter()
        .flat_map(|p| &p.text_items)
        .filter(|i| i.link.is_some())
        .count();
    assert!(linked > 0, "hyperlinks arrive as TextItem.link");
    assert!(
        !parsed.outline.is_empty(),
        "outline extracted from headings"
    );
    assert!(
        parsed
            .pages
            .iter()
            .all(|p| p.page_width > 0.0 && p.page_height > 0.0),
        "real page dimensions"
    );
}

/// §17.17.1 text-box content reaches markdown, exactly once, in reading order.
///
/// The fixture's two callouts are `<wp:inline>` `wps:wsp` shapes wrapped in an
/// `mc:AlternateContent` whose `mc:Fallback` is a VML `v:textbox` carrying
/// **the same text** — so the walk must commit to one branch or emit the box
/// twice. Before the text-box tap, the structure walk descended into neither
/// and 1,213 chars of instructional prose were absent from native markdown
/// while the LibreOffice path kept them.
#[cfg(feature = "docx-native")]
#[tokio::test]
#[serial]
async fn test_parse_docx_native_text_box_integration() {
    let path = "../../docx_files/enterprise/epa_swppp_template.docx";
    let lit = LiteParse::new(LiteParseConfig {
        output_format: liteparse::config::OutputFormat::Markdown,
        quiet: true,
        ..Default::default()
    });
    let md = lit.parse(path).await.expect("native parse succeeds").text;

    // The `mc:Choice`/`mc:Fallback` pair holds one copy of the text between
    // them; two hits would mean both branches were walked.
    assert_eq!(
        md.matches("equipment/vehicle washing practices").count(),
        1,
        "text box emitted exactly once (one MC branch)"
    );
    assert_eq!(
        md.matches("types of building products, materials, and wastes")
            .count(),
        1,
    );

    // Sibling blocks, not fused into the host paragraph: the box's own header
    // and its bullets keep their structure, and land after the section heading
    // the box follows in the document.
    let heading = md
        .find("5.4 Washing of Equipment and Vehicles")
        .expect("host heading present");
    let header = md
        .find("**Instructions (see CGP Parts 2.3.2 and 7.2.6):**")
        .expect("box header emitted as its own bold block");
    let bullet = md
        .find("- Describe equipment/vehicle washing practices")
        .expect("box body emitted as list items");
    assert!(
        heading < header && header < bullet,
        "box content reads after its host heading, header before bullets"
    );
}

/// Native image extraction: original embedded bytes surface on
/// `ParseResult.images` with platform naming, markdown carries matching
/// figure refs for body images, and repeated media (the per-page header
/// logo case) dedups to one canonical entry.
#[cfg(feature = "docx-native")]
#[tokio::test]
#[serial]
async fn test_parse_docx_native_images_integration() {
    let lit = LiteParse::new(LiteParseConfig {
        output_format: liteparse::config::OutputFormat::Markdown,
        extract_images: true,
        quiet: true,
        ..Default::default()
    });

    // Body images: every extracted entry has bytes and a matching markdown
    // figure reference.
    let parsed = lit
        .parse("../../docx_files/financial/fdic_srs_user_guide.docx")
        .await
        .expect("native parse succeeds");
    assert!(
        !parsed.images.is_empty(),
        "drawing commands surface as extracted images"
    );
    for img in &parsed.images {
        assert!(!img.bytes.is_empty(), "{}: bytes present", img.id);
        assert!(
            img.width > 0 && img.height > 0,
            "{}: real pixel dims",
            img.id
        );
        assert!(
            parsed
                .text
                .contains(&format!("![](img_{}.{})", img.id, img.format)),
            "{}: figure ref in markdown",
            img.id
        );
    }

    // Header-logo dedup: one canonical entry, every other placement points
    // at it and shares its bytes.
    let parsed = lit
        .parse("../../docx_files/enterprise/nitaac_sow_template.docx")
        .await
        .expect("native parse succeeds");
    let canonical: Vec<_> = parsed
        .images
        .iter()
        .filter(|i| i.duplicate_of.is_none())
        .collect();
    let dups: Vec<_> = parsed
        .images
        .iter()
        .filter(|i| i.duplicate_of.is_some())
        .collect();
    assert_eq!(canonical.len(), 1, "one canonical logo");
    assert!(!dups.is_empty(), "repeated placements dedup");
    for d in &dups {
        assert_eq!(d.duplicate_of.as_deref(), Some(canonical[0].id.as_str()));
        assert!(
            std::sync::Arc::ptr_eq(&d.bytes, &canonical[0].bytes),
            "duplicates share the canonical buffer"
        );
    }
}

/// Native annotation extraction: per-word hyperlink rects merge to one
/// `link` annotation per hyperlink instance (uri set), and internal links
/// (TOC/cross-refs) surface as uri-less `link` annotations — the GoTo shape
/// the LibreOffice-converted path produces for the same documents.
#[cfg(feature = "docx-native")]
#[tokio::test]
#[serial]
async fn test_parse_docx_native_annotations_integration() {
    let lit = LiteParse::new(LiteParseConfig {
        extract_annotations: true,
        quiet: true,
        ..Default::default()
    });

    // External hyperlinks.
    let parsed = lit
        .parse("../../docx_files/legal/uk_parl_media_bill_ia.docx")
        .await
        .expect("native parse succeeds");
    let anns: Vec<_> = parsed
        .pages
        .iter()
        .flat_map(|p| p.annotations.as_deref().expect("enabled flag → Some"))
        .collect();
    assert!(!anns.is_empty(), "hyperlinks surface as annotations");
    assert!(anns.iter().all(|a| a.subtype == "link"));
    assert!(
        anns.iter().all(|a| a.uri.is_some()),
        "this doc has only external links"
    );
    for a in &anns {
        let r = a.rect.as_ref().expect("every link has a rect");
        assert!(r.width > 0.0 && r.height > 0.0);
    }
    // Word-grain rects must have been merged: fewer annotations than linked
    // words (this doc has 83 per-word link rects).
    assert!(
        anns.len() < 40,
        "per-word rects merged into per-hyperlink annotations, got {}",
        anns.len()
    );

    // Internal links: uri-less `link` annotations.
    let parsed = lit
        .parse("../../docx_files/legal/courts_8th_civil_jury.docx")
        .await
        .expect("native parse succeeds");
    let internal = parsed
        .pages
        .iter()
        .flat_map(|p| p.annotations.as_deref().unwrap_or_default())
        .filter(|a| a.uri.is_none())
        .count();
    assert!(
        internal > 0,
        "internal links surface as uri-less annotations"
    );

    // Flag off: `annotations` stays None on every page.
    let lit_off = LiteParse::new(LiteParseConfig {
        quiet: true,
        ..Default::default()
    });
    let parsed = lit_off
        .parse("../../docx_files/legal/uk_parl_media_bill_ia.docx")
        .await
        .expect("native parse succeeds");
    assert!(parsed.pages.iter().all(|p| p.annotations.is_none()));
}

/// Native complexity: layout signals come from source facts — the
/// section-declared column count and the page's actual table blocks — not
/// from geometric detection.
#[cfg(feature = "docx-native")]
#[tokio::test]
#[serial]
async fn test_parse_docx_native_complexity_integration() {
    let lit = LiteParse::new(LiteParseConfig {
        include_complexity: true,
        quiet: true,
        ..Default::default()
    });

    // This doc's first page declares 2- and 3-column sections (§17.6.4).
    let parsed = lit
        .parse("../../docx_files/financial/cfpb_credit_dispute_letter.docx")
        .await
        .expect("native parse succeeds");
    let stats: Vec<_> = parsed
        .pages
        .iter()
        .map(|p| {
            p.complexity
                .as_ref()
                .expect("flag on → stats on every page")
        })
        .collect();
    let layouts: Vec<_> = stats
        .iter()
        .map(|c| c.layout.as_ref().expect("native stats carry layout"))
        .collect();
    assert!(
        layouts.iter().any(|l| l.column_count >= 2),
        "section-declared multi-column surfaces"
    );
    // Native text is never vector-outline text: the area is a known zero
    // (or None when a cheaper predicate fired), never a positive value.
    assert!(
        stats
            .iter()
            .all(|c| c.uncovered_vector_area.unwrap_or(0.0) == 0.0)
    );

    // Table pages report TableLikely from actual table blocks.
    let parsed = lit
        .parse("../../docx_files/enterprise/nitaac_sow_template.docx")
        .await
        .expect("native parse succeeds");
    assert!(
        parsed.pages.iter().any(|p| {
            p.complexity
                .as_ref()
                .and_then(|c| c.layout.as_ref())
                .is_some_and(|l| l.ruled_table_count > 0)
        }),
        "table blocks drive the table-likely signal"
    );

    // Flag off: no complexity on any page.
    let lit_off = LiteParse::new(LiteParseConfig {
        quiet: true,
        ..Default::default()
    });
    let parsed = lit_off
        .parse("../../docx_files/financial/cfpb_credit_dispute_letter.docx")
        .await
        .expect("native parse succeeds");
    assert!(parsed.pages.iter().all(|p| p.complexity.is_none()));
}

/// Native PPTX path end-to-end.
///
/// The load-bearing assertion is the byte-identity between `parsed.text` and
/// the emitter's own output: `office/pptx.rs` is what the corpus content-recall
/// number (0.9256 vs anydoc's 0.9250) was measured on, and the wiring must not
/// perturb it. If this fails, the recorded score no longer describes what the
/// CLI produces.
///
/// The rest pins the contract the geometry pass added: one page per slide, real
/// boxes on every page, and an outline built from slide titles.
#[cfg(feature = "pptx-native")]
#[tokio::test]
#[serial]
async fn test_parse_pptx_native_integration() {
    let path = "../../bench/pptx_corpus/staging/cloudviper_workshop__19687752.pptx";
    let lit = LiteParse::new(LiteParseConfig {
        output_format: liteparse::config::OutputFormat::Markdown,
        quiet: true,
        ..Default::default()
    });
    let parsed = lit.parse(path).await.expect("native parse succeeds");

    let data = std::fs::read(path).expect("fixture readable");
    let blocks = liteparse::office::pptx::pptx_to_blocks(
        &data,
        liteparse::office::pptx::EmitOptions {
            // Mirrors the defaults the parse above ran with: `extract_links`
            // is on by default, the native path always emits notes, and
            // `image_mode` defaults to `placeholder` so figure refs are on
            // while the bytes behind them are not requested.
            links: true,
            notes: true,
            figures: true,
            images: false,
        },
    )
    .expect("blocks");
    let expected = liteparse::markdown_layout::render_blocks(&blocks);
    assert_eq!(
        parsed.text, expected,
        "native pipeline markdown must match the emitter byte-for-byte"
    );

    // A slide is a page — the counts cannot drift, because a `BlockSource`
    // indexes pages directly.
    assert_eq!(parsed.pages.len(), 25, "one page per slide");
    assert!(
        parsed
            .pages
            .iter()
            .all(|p| p.page_width > 0.0 && p.page_height > 0.0),
        "real slide dimensions"
    );

    let items: usize = parsed.pages.iter().map(|p| p.text_items.len()).sum();
    assert!(items > 500, "per-run geometry present: {items} items");
    assert!(
        parsed
            .pages
            .iter()
            .flat_map(|p| &p.text_items)
            .all(|i| i.width > 0.0 || i.text.trim().is_empty()),
        "no degenerate boxes on visible text"
    );
    assert!(
        !parsed.outline.is_empty(),
        "outline built from slide titles"
    );
    assert!(
        parsed.outline.iter().all(|o| o.level == 1),
        "PPTX has no heading hierarchy, so every outline entry is level 1"
    );
}

/// Native PPTX screenshots: the slide is rastered from the same draw commands
/// the geometry pass laid out, so the PNG and the parse's `TextItem`s share one
/// coordinate space. That parity is the assertion below — a screenshot from the
/// conversion path never had it, which is what blocked highlight-on-screenshot.
///
/// `--target-pages` is exercised too, because a slide number is a page number
/// and nothing downstream re-maps it.
#[cfg(feature = "pptx-native")]
#[tokio::test]
#[serial]
async fn test_screenshot_pptx_native_integration() {
    let path = "../../bench/pptx_corpus/staging/cloudviper_workshop__19687752.pptx";
    let lit = LiteParse::new(LiteParseConfig {
        quiet: true,
        ..Default::default()
    });

    let shots = lit.screenshot(path, None).await.expect("native raster");
    assert_eq!(shots.len(), 25, "one screenshot per slide");
    assert!(
        shots.iter().all(|s| !s.image_bytes.is_empty()),
        "every slide encodes to PNG"
    );
    assert!(
        shots.iter().any(|s| !s.is_solid_fill),
        "a 25-slide deck is not 25 blank pages"
    );

    // Same coordinate space as the parse: the raster is the page box scaled by
    // dpi/72, so a `TextItem` box maps onto the PNG by that one factor.
    let parsed = lit.parse(path).await.expect("native parse succeeds");
    let scale = lit.config().dpi / 72.0;
    for (shot, page) in shots.iter().zip(&parsed.pages) {
        let expect_w = (page.page_width * scale).round() as u32;
        let expect_h = (page.page_height * scale).round() as u32;
        assert!(
            shot.width.abs_diff(expect_w) <= 1 && shot.height.abs_diff(expect_h) <= 1,
            "slide {}: raster {}x{} vs page {}x{} at {scale}x",
            shot.page_num,
            shot.width,
            shot.height,
            expect_w,
            expect_h
        );
    }

    let picked = lit
        .screenshot(path, Some(vec![2, 5]))
        .await
        .expect("native raster");
    assert_eq!(
        picked.iter().map(|s| s.page_num).collect::<Vec<_>>(),
        vec![2, 5],
        "a slide number is a page number"
    );

    let err = lit
        .screenshot(path, Some(vec![999]))
        .await
        .expect_err("out-of-range slide is the caller's error, not a fallback");
    assert!(
        err.to_string().contains("out of range"),
        "engine reports the range rather than degrading to LibreOffice: {err}"
    );
}

/// A config the native PPTX path cannot honor is a hard error, not a silent
/// swap to LibreOffice — the user picks the engine, the engine never picks for
/// them. `extract_annotations` is the PPTX-specific entry: the geometry adapter
/// builds fragments with no `LinkTarget`, so there are no link rects to merge.
///
/// This used to test `extract_images`, which is now supported — see
/// [`test_pptx_native_extracts_images`].
#[cfg(feature = "pptx-native")]
#[tokio::test]
#[serial]
async fn test_pptx_native_unsupported_config_is_hard_error() {
    let lit = LiteParse::new(LiteParseConfig {
        extract_annotations: true,
        quiet: true,
        ..Default::default()
    });
    let msg = match lit
        .parse("../../bench/pptx_corpus/staging/cloudviper_workshop__19687752.pptx")
        .await
    {
        Err(e) => e.to_string(),
        Ok(_) => panic!("unsupported option must not silently fall back"),
    };
    assert!(
        msg.contains("extract_annotations") && msg.contains("native PPTX"),
        "error names the option and the engine: {msg}"
    );
}

/// `extract_images` on the native PPTX path: bytes come back, duplicates
/// collapse onto one canonical entry, and every ref in the markdown names a
/// file the extraction actually produced.
///
/// The dedup assertion is the load-bearing one. A deck places its master's logo
/// on every slide — this fixture is 25 slides — so an extractor without the
/// contract would hand back 25 copies of one PNG.
#[cfg(feature = "pptx-native")]
#[tokio::test]
#[serial]
async fn test_pptx_native_extracts_images() {
    let lit = LiteParse::new(LiteParseConfig {
        output_format: liteparse::config::OutputFormat::Markdown,
        extract_images: true,
        quiet: true,
        ..Default::default()
    });
    let parsed = lit
        .parse("../../bench/pptx_corpus/staging/cloudviper_workshop__19687752.pptx")
        .await
        .expect("native parse succeeds with extract_images");

    assert!(
        !parsed.images.is_empty(),
        "a deck full of pictures extracts some"
    );
    assert!(
        parsed.images.iter().any(|i| i.duplicate_of.is_some()),
        "a repeated master picture collapses onto a canonical entry"
    );
    // Bytes, dimensions and placement are all real — a zero on any of them is
    // an entry that resolved structurally and carries nothing.
    for img in &parsed.images {
        assert!(!img.bytes.is_empty(), "{} has bytes", img.id);
        assert!(img.width > 0 && img.height > 0, "{} has dimensions", img.id);
        assert!(
            img.bbox.width > 0.0 && img.bbox.height > 0.0,
            "{} is placed",
            img.id
        );
        assert!(img.page >= 1, "{} is on a 1-based page", img.id);
    }

    // Every `![](img_…)` in the markdown names an extracted image. After
    // `rewrite_duplicate_image_refs` those are canonical names only, which is
    // what makes a written-out directory self-consistent.
    let canonical: std::collections::HashSet<&str> = parsed
        .images
        .iter()
        .filter(|i| i.duplicate_of.is_none())
        .map(|i| i.name.as_str())
        .collect();
    let mut refs = 0;
    for (at, marker) in parsed.text.match_indices("![](") {
        let name = parsed.text[at + marker.len()..]
            .split(')')
            .next()
            .expect("a closed markdown image ref");
        assert!(
            canonical.contains(name),
            "markdown ref {name} names a canonical extracted image"
        );
        refs += 1;
    }
    assert!(refs > 0, "figure refs reach the markdown");
}

/// Native XLSX screenshots: the sheet is rastered from the grid painter's own
/// draw commands, over the pages the geometry pass already numbered.
///
/// The coordinate-space parity assertion is the load-bearing one, and it means
/// more here than it did for the deck: LibreOffice paginates a sheet by its own
/// print rules, so the converted PDF's page 2 was never this pass's page 2. A
/// native raster and a native `TextItem` now share one page *and* one box.
#[cfg(feature = "xlsx-native")]
#[tokio::test]
#[serial]
async fn test_screenshot_xlsx_native_integration() {
    let path = "../../bench/xlsx_corpus/nfs/IC-Accounting-Business-General-Ledger-11334.xlsx";
    let lit = LiteParse::new(LiteParseConfig {
        quiet: true,
        ..Default::default()
    });

    let shots = lit.screenshot(path, None).await.expect("native raster");
    let parsed = lit.parse(path).await.expect("native parse succeeds");
    assert_eq!(
        shots.len(),
        parsed.pages.len(),
        "the raster paginates exactly like the parse"
    );
    assert!(
        shots.iter().all(|s| !s.image_bytes.is_empty()),
        "every page encodes to PNG"
    );
    assert!(
        shots.iter().any(|s| !s.is_solid_fill),
        "a painted ledger is not a blank page"
    );

    // Same coordinate space as the parse: the raster is the page box scaled by
    // dpi/72, modulo the 32 MP clamp a wide sheet can hit.
    for (shot, page) in shots.iter().zip(&parsed.pages) {
        let scale = liteparse_ooxml::render::raster::effective_scale(
            page.page_width,
            page.page_height,
            lit.config().dpi / 72.0,
        );
        let expect_w = (page.page_width * scale).round() as u32;
        let expect_h = (page.page_height * scale).round() as u32;
        assert!(
            shot.width.abs_diff(expect_w) <= 1 && shot.height.abs_diff(expect_h) <= 1,
            "page {}: raster {}x{} vs page {}x{} at {scale}x",
            shot.page_num,
            shot.width,
            shot.height,
            expect_w,
            expect_h
        );
    }

    let picked = lit
        .screenshot(path, Some(vec![2, 3]))
        .await
        .expect("native raster");
    assert_eq!(
        picked.iter().map(|s| s.page_num).collect::<Vec<_>>(),
        vec![2, 3],
        "a sheet page number is a page number"
    );

    let err = lit
        .screenshot(path, Some(vec![999]))
        .await
        .expect_err("out-of-range page is the caller's error, not a fallback");
    assert!(
        err.to_string().contains("out of range"),
        "engine reports the range rather than degrading to LibreOffice: {err}"
    );
}
