//! DOCX → [`Block`] using only `liteparse-docx`'s `parse` + `resolve` stages.
//!
//! Deliberately avoids the vendored crate's `render::layout` stage: structure
//! alone — headings, lists, emphasis, merged cells, notes — is recoverable
//! without a layout engine, and layout is where the font/measurement
//! dependencies live. Geometry is a separate concern (see `NATIVE_OFFICE_PLAN.md`
//! stage 2); this module produces no bounding boxes.
//!
//! What `resolve()` hands us is *not* fully cascaded: it walks `basedOn` chains
//! and flattens numbering (including `lvlOverride`/`startOverride`), but leaves
//! the per-run merge to the caller. That merge is [`Emitter::effective_fmt`].

use std::collections::HashMap;

use liteparse_docx::model::{
    Block as DocxBlock, Inline, NoteId, NumId, NumberFormat, OutlineLevel, Paragraph,
    ParagraphProperties, RunElement, RunProperties, StrikeStyle, StyleId, Table, VerticalMerge,
};
use liteparse_docx::render::resolve::ResolvedDocument;

use crate::error::LiteParseError;
use crate::markdown_layout::{Block, Cell, escape_inline};

/// Parse a DOCX and emit the shared block model.
///
/// Errors only when the archive itself is unreadable — the vendored parser is
/// fail-open on unknown elements, unknown attribute values and malformed
/// scalars, so a document with unmodeled markup still yields blocks.
pub fn docx_to_blocks(data: &[u8]) -> Result<Vec<Block>, LiteParseError> {
    let parsed = liteparse_docx::docx::parse(data)
        .map_err(|e| LiteParseError::Conversion(format!("docx parse failed: {e}")))?;
    let resolved = liteparse_docx::render::resolve::resolve(parsed);
    Ok(emit(&resolved))
}

/// Emit blocks for an already-resolved document.
pub fn emit(doc: &ResolvedDocument) -> Vec<Block> {
    emit_with_sources(doc).into_iter().map(|(b, _)| b).collect()
}

/// Where an emitted [`Block`] came from, for the layout block→page join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSource {
    /// Index into the flattened concatenation of every section's blocks
    /// (section breaks included) — the same flattening whose indices the
    /// layout engine records in `LayoutedPage::block_starts`.
    Body(usize),
    /// Footnote/endnote bodies, and the `HorizontalRule` that precedes them.
    Note,
}

/// [`emit`], with each block tagged by the source body block that produced it.
///
/// One source block maps to zero or one emitted blocks today (empty paragraphs
/// and section breaks emit nothing; nothing merges across source blocks), but
/// the tagging tolerates zero-or-more either way.
pub fn emit_with_sources(doc: &ResolvedDocument) -> Vec<(Block, BlockSource)> {
    let mut em = Emitter::new(doc);
    let mut out: Vec<(Block, BlockSource)> = Vec::new();
    // One Emitter across all blocks: list counters must persist so an ordered
    // list interrupted by prose continues its count.
    let mut scratch = Vec::new();
    let mut flat_idx = 0usize;
    for section in &doc.sections {
        for block in &section.blocks {
            em.blocks(std::slice::from_ref(block), &mut scratch);
            for b in scratch.drain(..) {
                out.push((b, BlockSource::Body(flat_idx)));
            }
            flat_idx += 1;
        }
    }

    // Note bodies are appended after the document, behind a rule. Emit *all* of
    // them, not just those whose reference we walked past: notes are frequently
    // referenced from headers, footers and textboxes, and content-completeness
    // is what matters here (spike 4 measured `present` 0.568 → 0.921 from this
    // one choice). Note ids 0 and 1 are the separator and continuationSeparator
    // — chrome, never real content.
    let mut notes = Vec::new();
    for map in [&doc.footnotes, &doc.endnotes] {
        let mut ids: Vec<NoteId> = map.keys().copied().filter(|i| i.value() > 1).collect();
        ids.sort_by_key(|i| i.value());
        for id in ids {
            em.blocks(&map[&id], &mut notes);
        }
    }
    if !notes.is_empty() {
        out.push((Block::HorizontalRule, BlockSource::Note));
        out.extend(notes.into_iter().map(|b| (b, BlockSource::Note)));
    }
    out
}

/// Effective character formatting after the style cascade is applied.
#[derive(Default, Clone, Copy, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
}

impl Fmt {
    fn is_plain(self) -> bool {
        !self.bold && !self.italic && !self.strike
    }
}

struct Emitter<'a> {
    doc: &'a ResolvedDocument,
    /// `(numId, level)` → next counter, so ordered lists number correctly and
    /// a list interrupted by prose continues its count.
    counters: HashMap<(i64, u8), u32>,
    /// True while emitting the content of a table cell. Cell text must stay
    /// *unescaped*: `render_blocks` escapes it per dialect on the way out (`|`
    /// for pipe tables, `&<>` for HTML), and markdown-escaping it here would
    /// either double the backslashes (`\\*`) or surface them literally inside
    /// an HTML block, where markdown escapes mean nothing.
    in_cell: bool,
}

impl<'a> Emitter<'a> {
    fn new(doc: &'a ResolvedDocument) -> Self {
        Self {
            doc,
            counters: HashMap::new(),
            in_cell: false,
        }
    }

    fn style_run(&self, id: Option<&StyleId>) -> Option<&RunProperties> {
        let id = id.or(self.doc.default_paragraph_style_id.as_ref())?;
        self.doc.styles.get(id).map(|s| &s.run)
    }

    fn style_para(&self, id: Option<&StyleId>) -> Option<&ParagraphProperties> {
        let id = id.or(self.doc.default_paragraph_style_id.as_ref())?;
        self.doc.styles.get(id).map(|s| &s.paragraph)
    }

    /// The cascade `resolve()` intentionally leaves to the caller:
    /// doc defaults → paragraph style → run's character style → direct.
    ///
    /// `Option<bool>` distinguishes "inherit" (`None`) from "explicitly off"
    /// (`Some(false)`), so a `NotBold` character style correctly overrides a
    /// bold paragraph style rather than being ignored.
    fn effective_fmt(
        &self,
        para: &Paragraph,
        run_style: Option<&StyleId>,
        direct: &RunProperties,
    ) -> Fmt {
        let mut bold = self.doc.doc_defaults_run.bold;
        let mut italic = self.doc.doc_defaults_run.italic;
        let mut strike = self.doc.doc_defaults_run.strike;

        let mut apply = |p: &RunProperties| {
            if p.bold.is_some() {
                bold = p.bold;
            }
            if p.italic.is_some() {
                italic = p.italic;
            }
            if p.strike.is_some() {
                strike = p.strike;
            }
        };

        if let Some(r) = self.style_run(para.style_id.as_ref()) {
            apply(r);
        }
        if let Some(s) = run_style.and_then(|id| self.doc.styles.get(id)) {
            apply(&s.run);
        }
        apply(direct);

        Fmt {
            bold: bold.unwrap_or(false),
            italic: italic.unwrap_or(false),
            strike: !matches!(strike.unwrap_or(StrikeStyle::None), StrikeStyle::None),
        }
    }

    /// Escape markdown specials unless we are inside a table cell.
    fn escape(&self, s: &str) -> String {
        if self.in_cell {
            s.to_string()
        } else {
            escape_inline(s)
        }
    }

    /// Cascade the two paragraph-level facts this emitter reads. Direct
    /// formatting wins; the paragraph style fills in what it leaves unset.
    fn para_props(&self, para: &Paragraph) -> ParagraphProperties {
        let mut out = para.properties.clone();
        if let Some(sp) = self.style_para(para.style_id.as_ref()) {
            if out.outline_level.is_none() {
                out.outline_level = sp.outline_level;
            }
            if out.numbering.is_none() {
                out.numbering = sp.numbering;
            }
        }
        out
    }

    /// Flatten a paragraph's inlines into formatting-tagged text chunks, in
    /// document order.
    fn chunks(&self, para: &Paragraph, content: &[Inline]) -> Vec<(Fmt, String)> {
        let mut out = Vec::new();
        self.collect(para, content, &mut out);
        out
    }

    fn collect(&self, para: &Paragraph, content: &[Inline], out: &mut Vec<(Fmt, String)>) {
        for inline in content {
            match inline {
                Inline::TextRun(run) => {
                    let fmt = self.effective_fmt(para, run.style_id.as_ref(), &run.properties);
                    let mut text = String::new();
                    for el in &run.content {
                        match el {
                            RunElement::Text(t) => text.push_str(t),
                            // A tab or a break inside a paragraph is a
                            // *within-line* separator once the paragraph
                            // becomes one markdown line; a space is the only
                            // faithful rendering.
                            RunElement::Tab | RunElement::LineBreak(_) => text.push(' '),
                            _ => {}
                        }
                    }
                    if !text.is_empty() {
                        out.push((fmt, text));
                    }
                }
                Inline::Hyperlink(h) => self.collect(para, &h.content, out),
                Inline::Field(f) => self.collect(para, &f.content, out),
                _ => {}
            }
        }
    }

    fn paragraph(&mut self, para: &Paragraph, out: &mut Vec<Block>) {
        let props = self.para_props(para);
        let chunks = self.chunks(para, &para.content);
        if chunks.iter().all(|(_, t)| t.trim().is_empty()) {
            return;
        }

        // Headings win over list formatting: a numbered heading is a heading.
        if let Some(lvl) = props.outline_level {
            // Deliberately plain — no `**` inside the `#`. The `#` *is* the
            // emphasis, and `# **Heading**` is redundant markup.
            let text: String = chunks.iter().map(|(_, t)| t.as_str()).collect();
            out.push(Block::Heading {
                // Emitted as declared. A dense 1..N remap was measured on the
                // 48-doc corpus and lost 39 of 66 heading rules; see the
                // "heading levels" note in NATIVE_OFFICE_PLAN.md.
                level: OutlineLevel::value(lvl).clamp(1, 6) as u8,
                text: self.escape(text.trim()),
            });
            return;
        }

        let (text, bold, italic) = render_chunks(&chunks, !self.in_cell);

        if let Some(nref) = props.numbering {
            let level = self
                .doc
                .numbering
                .get(&NumId::new(nref.num_id))
                .and_then(|lv| lv.get(nref.level as usize));
            let ordered = !matches!(level.map(|l| l.format), Some(NumberFormat::Bullet) | None);
            let marker = if ordered {
                let start = level.map(|l| l.start).unwrap_or(1);
                let key = (nref.num_id, nref.level);
                let n = *self
                    .counters
                    .entry(key)
                    .and_modify(|c| *c += 1)
                    .or_insert(start);
                // Deeper levels restart once a shallower one advances.
                self.counters
                    .retain(|(id, l), _| !(*id == nref.num_id && *l > nref.level));
                format!("{n}.")
            } else {
                // `render_blocks` prints `- ` for unordered items and ignores
                // this, but keep it meaningful rather than empty.
                "-".to_string()
            };
            out.push(Block::ListItem {
                ordered,
                marker,
                level: nref.level,
                text,
                bold,
                italic,
            });
            return;
        }

        out.push(Block::Paragraph { text, bold, italic });
    }

    /// Emit a table as [`Block::MergedTable`], which is the whole point of the
    /// native path: `gridSpan` and `vMerge` are explicit in the source, so the
    /// merge structure is read rather than inferred from cell coordinates.
    fn table(&mut self, table: &Table, out: &mut Vec<Block>) {
        // Grid column of each cell, so a vMerge run can be matched down a
        // column even when rows have different cell counts.
        let mut cols: Vec<Vec<u32>> = Vec::new();
        for row in &table.rows {
            let mut c = row.properties.grid_before;
            let mut rc = Vec::new();
            for cell in &row.cells {
                rc.push(c);
                c += cell.properties.grid_span.unwrap_or(1);
            }
            cols.push(rc);
        }

        // Row extent of each vMerge=Restart cell: how many following rows carry
        // a Continue cell in the same grid column.
        let mut rowspans: Vec<Vec<u16>> =
            table.rows.iter().map(|r| vec![1; r.cells.len()]).collect();
        for r in 0..table.rows.len() {
            for c in 0..table.rows[r].cells.len() {
                if table.rows[r].cells[c].properties.vertical_merge != Some(VerticalMerge::Restart)
                {
                    continue;
                }
                let col = cols[r][c];
                let mut n: u16 = 1;
                for r2 in r + 1..table.rows.len() {
                    let continues = table.rows[r2]
                        .cells
                        .iter()
                        .zip(&cols[r2])
                        .any(|(cell, cc)| {
                            *cc == col
                                && cell.properties.vertical_merge == Some(VerticalMerge::Continue)
                        });
                    if !continues {
                        break;
                    }
                    n += 1;
                }
                rowspans[r][c] = n;
            }
        }

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for (r, row) in table.rows.iter().enumerate() {
            let mut cells = Vec::new();
            for (c, cell) in row.cells.iter().enumerate() {
                // Continuation cells are absorbed into the restart cell's
                // rowspan and are absent from the grid, matching HTML.
                if cell.properties.vertical_merge == Some(VerticalMerge::Continue) {
                    continue;
                }
                cells.push(Cell::spanning(
                    self.cell_text(&cell.content),
                    cell.properties.grid_span.unwrap_or(1) as u16,
                    rowspans[r][c],
                ));
            }
            rows.push(cells);
        }

        // `header_rows` is a count of *leading* header rows. A `tblHeader` flag
        // deeper in the table marks a repeat-on-page-break row, which is not a
        // header in any markup we emit, so stop at the first non-header row.
        let header_rows = table
            .rows
            .iter()
            .take_while(|r| r.properties.is_header.unwrap_or(false))
            .count();

        if rows.iter().any(|r| !r.is_empty()) {
            out.push(Block::MergedTable { rows, header_rows });
        }
    }

    /// Flatten a cell's block content to a single line of inline markdown.
    ///
    /// A cell is a `Vec<Block>` in DOCX (it can hold paragraphs, lists, even
    /// nested tables) but both of our table renderings are single-line per
    /// cell, so the content is collapsed. Text is left *unescaped*: the
    /// renderer escapes per dialect (`|` for pipes, `&<>` for HTML), and
    /// escaping here would double up.
    fn cell_text(&mut self, content: &[DocxBlock]) -> String {
        let outer = std::mem::replace(&mut self.in_cell, true);
        let mut inner = Vec::new();
        self.blocks(content, &mut inner);
        self.in_cell = outer;
        let mut parts: Vec<String> = Vec::new();
        for b in &inner {
            match b {
                Block::Heading { text, .. } | Block::Paragraph { text, .. } => {
                    parts.push(text.clone())
                }
                Block::ListItem { text, .. } => parts.push(text.clone()),
                _ => {}
            }
        }
        parts
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn blocks(&mut self, blocks: &[DocxBlock], out: &mut Vec<Block>) {
        for b in blocks {
            match b {
                DocxBlock::Paragraph(p) => self.paragraph(p, out),
                DocxBlock::Table(t) => self.table(t, out),
                DocxBlock::SectionBreak(_) => {}
            }
        }
    }
}

/// Render formatting-tagged chunks to the `(text, bold, italic)` shape
/// `Block::Paragraph` and `Block::ListItem` expect.
///
/// When every chunk shares one plain-or-emphasised style the block-level flags
/// carry it and the text stays clean; otherwise emphasis is baked inline and
/// the flags are cleared. This mirrors `paragraph_from_accum` on the PDF path,
/// so the same prose looks the same whichever pipeline produced it.
fn render_chunks(chunks: &[(Fmt, String)], escape: bool) -> (String, bool, bool) {
    // Coalesce adjacent runs that share formatting first. Word splits a run at
    // every property change, including ones that don't survive the cascade
    // (language, spell-check state, rsid), so a single bold phrase routinely
    // arrives as several identically-formatted chunks. Emitting each one
    // separately would produce `**a****b**`, which no markdown parser reads as
    // one bold span.
    let mut merged: Vec<(Fmt, String)> = Vec::new();
    for (fmt, text) in chunks {
        match merged.last_mut() {
            Some((prev, buf)) if prev == fmt => buf.push_str(text),
            _ => merged.push((*fmt, text.clone())),
        }
    }
    let chunks = &merged[..];

    let uniform = chunks.first().map(|(f, _)| *f).filter(|f| {
        // Strike has no block-level flag, so a uniformly-struck paragraph still
        // takes the inline path.
        !f.strike && chunks.iter().all(|(g, _)| g == f)
    });

    if let Some(fmt) = uniform {
        let raw: String = chunks.iter().map(|(_, t)| t.as_str()).collect();
        let raw = raw.trim();
        let text = if escape {
            escape_inline(raw)
        } else {
            raw.to_string()
        };
        return (text, fmt.bold, fmt.italic);
    }

    let mut out = String::new();
    for (fmt, text) in chunks {
        // Emphasis markers must hug non-space content to be valid markdown, so
        // surrounding whitespace is hoisted outside the markers.
        let lead: String = text.chars().take_while(|c| c.is_whitespace()).collect();
        let core = text.trim();
        let trail = if core.is_empty() {
            String::new()
        } else {
            text.chars()
                .rev()
                .take_while(|c| c.is_whitespace())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };

        out.push_str(&lead);
        if core.is_empty() {
            continue;
        }
        let escaped = if escape {
            escape_inline(core)
        } else {
            core.to_string()
        };
        if fmt.is_plain() {
            out.push_str(&escaped);
        } else {
            let (mut open, mut close) = (String::new(), String::new());
            if fmt.bold {
                open.push_str("**");
                close.insert_str(0, "**");
            }
            if fmt.italic {
                open.push('*');
                close.insert(0, '*');
            }
            if fmt.strike {
                open.push_str("~~");
                close.insert_str(0, "~~");
            }
            out.push_str(&open);
            out.push_str(&escaped);
            out.push_str(&close);
        }
        out.push_str(&trail);
    }
    (out.trim().to_string(), false, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(bold: bool, italic: bool, strike: bool) -> Fmt {
        Fmt {
            bold,
            italic,
            strike,
        }
    }

    fn chunks(v: &[(Fmt, &str)]) -> Vec<(Fmt, String)> {
        v.iter().map(|(f, t)| (*f, t.to_string())).collect()
    }

    #[test]
    fn uniform_emphasis_becomes_block_flags() {
        let (text, bold, italic) =
            render_chunks(&chunks(&[(fmt(true, false, false), "all bold")]), true);
        assert_eq!((text.as_str(), bold, italic), ("all bold", true, false));
    }

    #[test]
    fn mixed_emphasis_is_baked_inline_and_flags_cleared() {
        let (text, bold, italic) = render_chunks(
            &chunks(&[
                (fmt(false, false, false), "plain "),
                (fmt(true, false, false), "bold"),
            ]),
            true,
        );
        assert_eq!(
            (text.as_str(), bold, italic),
            ("plain **bold**", false, false)
        );
    }

    /// `Block` has no strike flag, so a uniformly-struck paragraph must take
    /// the inline path rather than silently losing the strikethrough.
    #[test]
    fn uniform_strike_falls_back_to_inline() {
        let (text, bold, italic) =
            render_chunks(&chunks(&[(fmt(false, false, true), "gone")]), true);
        assert_eq!((text.as_str(), bold, italic), ("~~gone~~", false, false));
    }

    /// Markers must hug non-space content or markdown won't parse them, so
    /// whitespace inside a run is hoisted outside the markers.
    #[test]
    fn markers_hug_content_across_run_whitespace() {
        let (text, ..) = render_chunks(
            &chunks(&[
                (fmt(true, false, false), "bold "),
                (fmt(false, false, false), "tail"),
            ]),
            true,
        );
        assert_eq!(text, "**bold** tail");
    }

    #[test]
    fn escaping_is_skipped_for_cell_content() {
        let c = chunks(&[(fmt(false, false, false), "a_b*c")]);
        assert_eq!(render_chunks(&c, true).0, "a\\_b\\*c");
        // Inside a cell the renderer escapes per dialect; escaping here would
        // double the backslash in a pipe table and surface it in HTML.
        assert_eq!(render_chunks(&c, false).0, "a_b*c");
    }

    #[test]
    fn adjacent_runs_sharing_format_do_not_split_markers() {
        let (text, ..) = render_chunks(
            &chunks(&[
                (fmt(true, false, false), "a"),
                (fmt(true, false, false), "b"),
                (fmt(false, false, false), "!"),
            ]),
            true,
        );
        assert_eq!(text, "**ab**!");
    }

    /// Provenance tagging must not change what is emitted; body indices must
    /// be non-decreasing (the block→page join walks them with a forward
    /// cursor); and note bodies must form a `Note`-tagged tail behind the
    /// rule, so the page split can send them to the last page.
    #[test]
    fn emit_with_sources_is_emit_plus_tags() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docx_files/legal/courts_3rd_circuit_ch4.docx"
        ))
        .expect("corpus fixture");
        let parsed = liteparse_docx::docx::parse(&data).expect("fixture parses");
        let doc = liteparse_docx::render::resolve::resolve(parsed);

        let tagged = emit_with_sources(&doc);
        let plain = emit(&doc);
        assert_eq!(
            format!("{plain:?}"),
            format!("{:?}", tagged.iter().map(|(b, _)| b).collect::<Vec<_>>()),
            "tagging must be a pure annotation"
        );

        let body: Vec<usize> = tagged
            .iter()
            .filter_map(|(_, s)| match s {
                BlockSource::Body(i) => Some(*i),
                BlockSource::Note => None,
            })
            .collect();
        assert!(!body.is_empty());
        assert!(
            body.windows(2).all(|w| w[0] <= w[1]),
            "body indices are non-decreasing"
        );

        let first_note = tagged
            .iter()
            .position(|(_, s)| *s == BlockSource::Note)
            .expect("fixture has footnotes");
        assert!(
            matches!(tagged[first_note].0, Block::HorizontalRule),
            "notes start with the separating rule"
        );
        assert!(
            tagged[first_note..]
                .iter()
                .all(|(_, s)| *s == BlockSource::Note),
            "notes are a contiguous tail"
        );
        assert!(
            tagged[first_note..].len() > 1,
            "note bodies follow the rule"
        );
    }
}
