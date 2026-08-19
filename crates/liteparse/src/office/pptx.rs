//! PPTX → [`Block`], reading the package directly instead of rendering it.
//!
//! Unlike [`super::docx`] this reader has real geometry available — every shape
//! declares `<a:off>`/`<a:ext>` in EMU, and `pptx::apply_slide_geometry` has
//! already composed group coordinate spaces into `Shape::slide_rect`. That
//! changes the shape of the problem: DOCX has no coordinates so its emitter
//! walks the document in source order, while here **source order is z-order and
//! disagrees with reading order on 48.9% of slides** (measured by
//! `pptx_emit_census`). Ordering is therefore a decision this module makes, not
//! one the file makes for it.
//!
//! The design below follows that census throughout; where a rule looks
//! arbitrary it is usually a corpus measurement, and the comment says which.

use liteparse_ooxml::model::{Inline, RunElement, RunProperties, StrikeStyle};
use liteparse_ooxml::pptx::{
    self, AutoNumberScheme, Background, BackgroundSource, Bullet, DeckTextDefaults,
    GraphicFramePayload, ListStyle, MatchRule, Placeholder, PlaceholderGeometry, PlaceholderKind,
    PlaceholderTextStyles, PresentationPackage, ResolvedTextStyle, Shape, ShapeKind, Table,
    TextBody, TextCascade, TextParagraph, TextStyles,
};
use std::collections::HashMap;

use crate::error::LiteParseError;
use crate::markdown_layout::{Block, Cell};
use crate::office::inline::{Chunk, Fmt, render_chunks};

/// Two shapes whose tops differ by less than this sit on the same visual row
/// and are ordered left-to-right. 0.25 in, generous for a line height, so the
/// sort never reorders two shapes a reader would call level with each other.
const ROW_BAND_EMU: i64 = 228_600;

/// Slide titles become `#`. Everything else that is prose becomes a paragraph
/// or a list item — PPTX has no other heading signal, since `outlineLvl` is a
/// DOCX concept and body placeholders carry indent levels, not ranks.
const TITLE_HEADING_LEVEL: u8 = 1;

/// What the emitter should produce beyond the always-on structure.
#[derive(Default, Clone, Copy)]
pub struct EmitOptions {
    /// Render external hyperlinks as `[text](url)`. Mirrors
    /// `LiteParseConfig::extract_links`, which the PDF path honors too.
    pub links: bool,
    /// Emit speaker notes after each slide's body. The census measured 222k
    /// characters of notes against 149k for all slide body text, and
    /// LibreOffice renders none of it, so this is the single largest content
    /// difference between the native and converted paths.
    pub notes: bool,
}

/// Where an emitted [`Block`] came from.
///
/// Simpler than the DOCX equivalent, which has to index into a flattened body
/// sequence to rejoin blocks to layout-assigned pages. A slide *is* a page, so
/// the index is intrinsic rather than recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSource {
    /// Body content of slide `n`, zero-based.
    Slide(usize),
    /// SmartArt text on slide `n`, zero-based.
    ///
    /// Distinguished from [`BlockSource::Slide`] because the text is not in the
    /// slide part at all — the `p:graphicFrame` holds only `dgm:relIds` and the
    /// content lives in `ppt/diagrams/data*.xml`, laid out by the diagram's own
    /// algorithm. The frame has a rectangle; the runs inside it do not, so a
    /// consumer that needs per-run geometry has to know these blocks cannot
    /// supply it.
    Diagram(usize),
    /// Speaker notes attached to slide `n`, zero-based.
    Notes(usize),
}

/// Parse a PPTX and emit the shared block model.
///
/// Errors only when the package itself is unreadable. Individual parts are
/// fail-open — a slide whose shape tree will not parse is skipped and the rest
/// of the deck still emits, matching the tolerance the DOCX vendor was
/// retrofitted with across four separate fail-closed classes.
pub fn pptx_to_blocks(data: &[u8], opts: EmitOptions) -> Result<Vec<Block>, LiteParseError> {
    Ok(emit_with_sources(data, opts)?
        .into_iter()
        .map(|(b, _)| b)
        .collect())
}

/// Emit blocks tagged with the slide they came from.
pub fn emit_with_sources(
    data: &[u8],
    opts: EmitOptions,
) -> Result<Vec<(Block, BlockSource)>, LiteParseError> {
    let pkg = pptx::walk(data)
        .map_err(|e| LiteParseError::Conversion(format!("pptx parse failed: {e}")))?;
    Ok(Deck::new(&pkg).emit(&pkg, opts))
}

/// Per-deck caches for the two cascades.
///
/// Both rungs are keyed by part *path* and rebuilt once per path, never once
/// per slide. Every previous PPTX probe records this discipline because a deck
/// with 60 slides sharing 3 layouts otherwise reparses those layouts 60 times.
#[doc(hidden)]
pub struct Deck {
    master_geo: HashMap<String, PlaceholderGeometry>,
    layout_geo: HashMap<String, PlaceholderGeometry>,
    master_text: HashMap<String, (PlaceholderTextStyles, DeckTextDefaults)>,
    layout_text: HashMap<String, PlaceholderTextStyles>,
    /// Each part's **own** `<p:bg>`, uninherited — the two upper rungs of the
    /// background cascade, cached beside the geometry and text rungs and for
    /// the same reason: a deck with 60 slides over 3 layouts must not reparse
    /// them 60 times. `None` in the map means "this part declares none", which
    /// is not the same as "no background": see [`pptx::resolve_background`].
    master_bg: HashMap<String, Option<Background>>,
    layout_bg: HashMap<String, Option<Background>>,
    default_text_style: ListStyle,
    /// The rung-7-only defaults a slide falls back to when its master has no
    /// entry. Stored on the deck rather than built as a local in the walk so
    /// that [`PreparedSlide`] can borrow it and every walk builds the *same*
    /// `TextCascade` — see [`Deck::prepare`].
    fallback_defaults: DeckTextDefaults,
}

/// One slide's shapes with their geometry composed and their text cascade
/// assembled — everything a walk needs before it visits a shape.
///
/// This exists so that the markdown walk and the geometry walk cannot drift.
/// They must agree on the composed rectangles and on the cascade, because a
/// `TextItem` is only a faithful box for the markdown a reader sees if both
/// came out of the same resolution. Two copies of this setup would compile,
/// run, and disagree silently.
#[doc(hidden)]
pub struct PreparedSlide<'d> {
    pub shapes: Vec<Shape>,
    /// The background in force on this slide, already resolved down the
    /// slide → layout → master chain, with the rung it came from.
    ///
    /// Owned rather than borrowed because the winner may be the slide's own,
    /// which is parsed here and outlives nothing. Cloning one `p:bg` per slide
    /// is a fill descriptor, not a shape tree.
    ///
    /// Still **uncoloured**: turning it into a fill needs the theme of this
    /// slide's own master, which the emitter does not load and the geometry
    /// pass does. `None` means no part in the chain declared one, which does
    /// not happen on the corpus (0 of 1,278) and is therefore a signal.
    pub background: Option<(BackgroundSource, Background)>,
    layout_text: Option<&'d PlaceholderTextStyles>,
    master_text: Option<&'d PlaceholderTextStyles>,
    deck_defaults: &'d DeckTextDefaults,
}

impl<'d> PreparedSlide<'d> {
    /// The slide-level text cascade (rungs 4-7). Rung 2, the shape's own
    /// `a:lstStyle`, is layered on per text body by the caller.
    pub fn cascade(&self) -> TextCascade<'d> {
        TextCascade {
            shape: None,
            layout: self.layout_text,
            master: self.master_text,
            deck: Some(self.deck_defaults),
        }
    }
}

impl Deck {
    pub fn new(pkg: &PresentationPackage) -> Self {
        // Rung 7. A deck whose presentation.xml will not parse still
        // resolves through the earlier rungs, so this degrades to empty
        // rather than failing the deck.
        let default_text_style =
            pptx::parse_default_text_style(&pkg.presentation.xml).unwrap_or_default();
        Self {
            master_geo: HashMap::new(),
            layout_geo: HashMap::new(),
            master_text: HashMap::new(),
            layout_text: HashMap::new(),
            master_bg: HashMap::new(),
            layout_bg: HashMap::new(),
            fallback_defaults: DeckTextDefaults {
                master_styles: TextStyles::default(),
                default_text_style: default_text_style.clone(),
            },
            default_text_style,
        }
    }

    /// Prime the cascades for this slide's layout/master, parse its shape
    /// tree, and compose its geometry.
    ///
    /// `None` when the shape tree will not parse — fail-open, the same slide
    /// is skipped by every walk.
    pub fn prepare(&mut self, slide: &pptx::SlideParts) -> Option<PreparedSlide<'_>> {
        self.prime(slide);

        let part = pptx::parse_slide_part(&slide.slide.xml).ok()?;
        let mut shapes = part.shapes;
        let background = pptx::resolve_background(
            part.background.as_ref(),
            slide
                .layout
                .as_ref()
                .and_then(|l| self.layout_bg.get(&l.path))
                .and_then(Option::as_ref),
            slide
                .master
                .as_ref()
                .and_then(|m| self.master_bg.get(&m.path))
                .and_then(Option::as_ref),
        )
        .map(|(src, bg)| (src, bg.clone()));
        let layout_geo = slide
            .layout
            .as_ref()
            .and_then(|l| self.layout_geo.get(&l.path));
        // Cascade before composition: a placeholder has no rectangle to
        // compose until it has inherited one.
        pptx::apply_inherited_geometry(&mut shapes, layout_geo, MatchRule::Idx);
        pptx::apply_slide_geometry(&mut shapes);

        let master = slide
            .master
            .as_ref()
            .and_then(|m| self.master_text.get(&m.path));
        Some(PreparedSlide {
            shapes,
            background,
            layout_text: slide
                .layout
                .as_ref()
                .and_then(|l| self.layout_text.get(&l.path)),
            master_text: master.map(|(p, _)| p),
            deck_defaults: master.map_or(&self.fallback_defaults, |(_, d)| d),
        })
    }

    fn emit(&mut self, pkg: &PresentationPackage, opts: EmitOptions) -> Vec<(Block, BlockSource)> {
        let mut out = Vec::new();

        for (idx, slide) in pkg.slides.iter().enumerate() {
            let Some(prepared) = self.prepare(slide) else {
                continue;
            };
            let shapes = &prepared.shapes;

            let mut ctx = SlideCtx {
                cascade: prepared.cascade(),
                part: &slide.slide,
                package: &pkg.package,
                opts,
            };
            for shape in reading_order(shapes) {
                emit_shape(shape, &mut ctx, &mut out, BlockSource::Slide(idx));
            }

            if opts.notes
                && let Some(notes) = &slide.notes
                && let Ok(nshapes) = pptx::parse_shape_tree(&notes.xml)
            {
                emit_notes(&nshapes, &mut ctx, &mut out, idx);
            }
        }
        out
    }

    /// Build whichever cascade rungs this slide's layout and master have not
    /// contributed yet.
    fn prime(&mut self, slide: &pptx::SlideParts) {
        if let Some(master) = &slide.master
            && !self.master_geo.contains_key(&master.path)
        {
            // One deserialization for both payloads: the background lives
            // beside the shape tree in the same `p:cSld`, so parsing the part
            // twice would double the cost of priming for a one-element field.
            let part = pptx::parse_slide_part(&master.xml).unwrap_or_default();
            let shapes = part.shapes;
            self.master_bg.insert(master.path.clone(), part.background);
            self.master_geo.insert(
                master.path.clone(),
                PlaceholderGeometry::from_master(&shapes),
            );
            self.master_text.insert(
                master.path.clone(),
                (
                    PlaceholderTextStyles::from_part(&shapes),
                    DeckTextDefaults {
                        master_styles: pptx::parse_text_styles(&master.xml).unwrap_or_default(),
                        default_text_style: self.default_text_style.clone(),
                    },
                ),
            );
        }
        let master_geo = slide
            .master
            .as_ref()
            .and_then(|m| self.master_geo.get(&m.path));

        if let Some(layout) = &slide.layout
            && !self.layout_geo.contains_key(&layout.path)
        {
            let part = pptx::parse_slide_part(&layout.xml).unwrap_or_default();
            let mut shapes = part.shapes;
            self.layout_bg.insert(layout.path.clone(), part.background);
            pptx::apply_inherited_geometry(&mut shapes, master_geo, MatchRule::CollapsedKind);
            self.layout_geo.insert(
                layout.path.clone(),
                PlaceholderGeometry::from_layout(&shapes, master_geo),
            );
            self.layout_text.insert(
                layout.path.clone(),
                PlaceholderTextStyles::from_part(&shapes),
            );
        }
    }
}

struct SlideCtx<'a> {
    cascade: TextCascade<'a>,
    /// The slide part, for resolving relationships. Hyperlinks need the raw
    /// external target and SmartArt needs a resolved part path, so both the
    /// relationship table and the part's own directory are required.
    part: &'a pptx::Part,
    package: &'a liteparse_ooxml::docx::zip::PackageContents,
    opts: EmitOptions,
}

// ── reading order ───────────────────────────────────────────────────────────

/// Order a slide's top-level shapes for reading: **title first, then
/// geometric** (top-to-bottom in 0.25 in bands, left-to-right within a band).
///
/// Both halves are load-bearing and the census measured each. Source order
/// alone puts the title somewhere other than first on 95 slides; geometric
/// order alone still does on 41, because authors routinely place a kicker, a
/// caption or a logo strip above the title. Together: **0**.
///
/// Chrome is dropped rather than sorted — a slide number in a corner otherwise
/// perturbs the order without a reader ever caring.
///
/// A group is one unit. Its children are emitted in their own reading order
/// within it, so a two-column group does not interleave with unrelated shapes
/// elsewhere on the slide.
///
/// Public — and `doc(hidden)` — so that a probe or census can measure *this*
/// order rather than a copy of it. A duplicate that drifted by one tie-break
/// would report the production traversal as correct while measuring a
/// different one.
#[doc(hidden)]
pub fn reading_order(shapes: &[Shape]) -> Vec<&Shape> {
    let mut v: Vec<&Shape> = shapes
        .iter()
        .filter(|s| !is_chrome(s.placeholder.as_ref()))
        .collect();
    v.sort_by_key(|s| {
        let (band, x) = match s.slide_rect {
            Some(r) => {
                let b = r.bounding_box();
                (b.origin.y.raw().div_euclid(ROW_BAND_EMU), b.origin.x.raw())
            }
            // A shape with no rectangle cannot be placed. Sorting it last
            // keeps it in the output — dropping content to keep a sort total
            // is never the right trade — while leaving positioned shapes in
            // their proper order. The census measured 0 of these.
            None => (i64::MAX, i64::MAX),
        };
        (!is_title(s.placeholder.as_ref()), band, x)
    });
    v
}

#[doc(hidden)]
pub fn is_title(ph: Option<&Placeholder>) -> bool {
    matches!(
        ph.map(|p| p.kind),
        Some(PlaceholderKind::Title | PlaceholderKind::CtrTitle)
    )
}

/// The date / footer / slide-number family.
///
/// Note this drops ~7k characters the census found in `dt` and `ftr`
/// placeholders, which average 50 and 56 characters — far too long to be a
/// date or a page number, so some authors are reusing them for real text.
/// Dropping them wholesale is the conservative choice for now because a
/// length test would also admit genuine repeated footer chrome on every
/// slide of a 60-slide deck; revisit with a measurement, not a guess.
#[doc(hidden)]
pub fn is_chrome(ph: Option<&Placeholder>) -> bool {
    matches!(
        ph.map(|p| p.kind),
        Some(
            PlaceholderKind::Dt
                | PlaceholderKind::Ftr
                | PlaceholderKind::SldNum
                | PlaceholderKind::Hdr
                | PlaceholderKind::SldImg
        )
    )
}

// ── shapes ──────────────────────────────────────────────────────────────────

fn emit_shape(
    shape: &Shape,
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    src: BlockSource,
) {
    match &shape.kind {
        ShapeKind::AutoShape(sp) => {
            if let Some(body) = &sp.text {
                emit_text_body(body, shape.placeholder.as_ref(), ctx, out, src);
            }
        }
        ShapeKind::Group(group) => {
            for child in reading_order(&group.children) {
                emit_shape(child, ctx, out, src);
            }
        }
        ShapeKind::GraphicFrame(frame) => {
            match &frame.payload {
                GraphicFramePayload::Table(table) => emit_table(table, ctx, out, src),
                GraphicFramePayload::Diagram { data_rel } => emit_diagram(data_rel, ctx, out, src),
                // OLE and charts, which we genuinely do not read.
                GraphicFramePayload::Unsupported { .. } => {}
            }
        }
        // Pictures and connectors carry no text. Figures need image extraction
        // wired first, which on the DOCX path takes its ids from the layout
        // stage; PPTX has no equivalent yet.
        ShapeKind::Picture(_) | ShapeKind::Connector(_) => {}
    }
}

fn emit_text_body(
    body: &TextBody,
    placeholder: Option<&Placeholder>,
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    src: BlockSource,
) {
    let title = is_title(placeholder);
    // Rung 2 is the shape's own list style, so the cascade is per-shape.
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    // Ordered-list counters, per indent level, scoped to this text body.
    // PowerPoint restarts numbering per shape, and `render_blocks` prints the
    // marker verbatim rather than renumbering, so emitting a constant "1."
    // would put a literal `1.` on every item.
    let mut counters: HashMap<u8, u32> = HashMap::new();
    for para in &body.paragraphs {
        let resolved = cascade.resolve(&para.properties, placeholder);
        let chunks = chunks_of(para, &resolved, ctx);
        let (text, bold, italic) = render_chunks(&chunks, true);
        if text.is_empty() {
            // 1,114 empty paragraphs on the corpus. They are blank-line
            // separators inside a text box, and emitting them as empty blocks
            // would put stray blank paragraphs through `render_blocks`.
            continue;
        }

        let block = if title {
            Block::Heading {
                level: TITLE_HEADING_LEVEL,
                text,
            }
        } else if let Some((ordered, marker)) =
            list_marker(resolved.bullet.as_ref(), resolved.level, &mut counters)
        {
            Block::ListItem {
                ordered,
                marker,
                level: resolved.level,
                text,
                bold,
                italic,
            }
        } else {
            Block::Paragraph { text, bold, italic }
        };
        out.push((block, src));
    }
}

/// The bullet a paragraph resolves to, as `(ordered, marker)`, or `None` when
/// it is not a list item at all.
///
/// **68.6% of non-empty paragraphs are not list items** on the corpus — 44.7%
/// resolve to an explicit `buNone` and 23.9% to no bullet. The `buNone` share
/// is the reason this consults the resolved style rather than the paragraph:
/// `buNone` exists precisely to override an inherited bullet, so an emitter
/// reading only the master's list style would turn 3,890 plain paragraphs into
/// list items.
fn list_marker(
    bullet: Option<&Bullet>,
    level: u8,
    counters: &mut HashMap<u8, u32>,
) -> Option<(bool, String)> {
    match bullet? {
        Bullet::None => None,
        // The glyph is nearly always from a symbol font — `\u{f0b7}` in
        // Wingdings is the commonest bullet in the corpus — and is meaningless
        // as text. `render_blocks` prints the marker verbatim, so passing the
        // codepoint through would put a private-use character in the markdown.
        // Every bullet character therefore normalises to `-`.
        Bullet::Character { .. } => Some((false, "-".into())),
        Bullet::AutoNumber { scheme, start_at } => {
            let n = counters
                .entry(level)
                .and_modify(|n| *n += 1)
                .or_insert_with(|| start_at.unwrap_or(1));
            // A deeper level restarting means the shallower one keeps counting,
            // which is what PowerPoint does and what a reader expects.
            Some((true, format_autonumber(scheme, *n)))
        }
    }
}

/// Render an auto-number in its declared scheme.
///
/// Four schemes appear on the corpus (`ArabicPeriod` 102, `AlphaLcParenR` 8,
/// `AlphaLcPeriod` 5, `ArabicParenR` 2) but all nine are cheap to support, and
/// `Other` degrades to a plain number rather than guessing.
fn format_autonumber(scheme: &AutoNumberScheme, n: u32) -> String {
    use AutoNumberScheme as S;
    match scheme {
        S::ArabicPeriod => format!("{n}."),
        S::ArabicParenR => format!("{n})"),
        S::ArabicParenBoth => format!("({n})"),
        S::AlphaLcPeriod => format!("{}.", alpha(n, b'a')),
        S::AlphaUcPeriod => format!("{}.", alpha(n, b'A')),
        S::AlphaLcParenR => format!("{})", alpha(n, b'a')),
        S::AlphaUcParenR => format!("{})", alpha(n, b'A')),
        S::RomanLcPeriod => format!("{}.", roman(n).to_lowercase()),
        S::RomanUcPeriod => format!("{}.", roman(n)),
        S::Other(_) => format!("{n}."),
    }
}

/// Spreadsheet-style lettering: a..z, then aa, ab. Matches how PowerPoint
/// continues past 26 rather than wrapping back to `a`.
fn alpha(n: u32, base: u8) -> String {
    let mut n = n.max(1);
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(base + rem as u8);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn roman(n: u32) -> String {
    const TABLE: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    // Beyond the table's range there is no roman numeral to render, so fall
    // back to the arabic number rather than emitting an empty marker.
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    let mut n = n;
    let mut out = String::new();
    for (v, s) in TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

// ── tables ──────────────────────────────────────────────────────────────────

/// Emit `<a:tbl>` as [`Block::MergedTable`].
///
/// The occupancy rule is the trap the schema doc calls out: a cell absorbed by
/// a neighbour's merge is **still present** in the XML with `hMerge`/`vMerge`
/// set, while `MergedTable` follows HTML and expects it to be *absent*. So
/// absorbed cells are dropped and the origin cell carries the span.
///
/// Merges are rare here — 8 colspan and 6 rowspan cells across the whole
/// corpus, against DOCX where they were the single biggest win. The variant is
/// reused because it costs nothing, not because PPTX needs it.
fn emit_table(
    table: &Table,
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    src: BlockSource,
) {
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    for row in &table.rows {
        let mut cells = Vec::new();
        for cell in &row.cells {
            if cell.is_absorbed() {
                continue;
            }
            cells.push(Cell::spanning(
                cell_text(cell.text.as_ref(), ctx),
                cell.grid_span.min(u16::MAX as u32) as u16,
                cell.row_span.min(u16::MAX as u32) as u16,
            ));
        }
        rows.push(cells);
    }
    if rows.iter().all(|r| r.is_empty()) {
        return;
    }
    // `firstRow` is PPTX's only header signal and is set on 15 of 36 corpus
    // tables. When absent we claim no header rather than promoting row 0:
    // `MergedTable` renders a headerless table as a pipe table with an empty
    // header, which is what the DOCX path does for the same case.
    let header_rows = usize::from(table.first_row);
    out.push((Block::MergedTable { rows, header_rows }, src));
}

/// Collapse a cell's paragraphs to one line.
///
/// Text is left **unescaped**: `render_blocks` escapes per dialect on the way
/// out (`|` for pipe tables, `&<>` for HTML), and escaping here would either
/// double the backslashes or surface them literally inside an HTML cell. Same
/// rule as the DOCX emitter's `in_cell`.
fn cell_text(body: Option<&TextBody>, ctx: &mut SlideCtx<'_>) -> String {
    let Some(body) = body else {
        return String::new();
    };
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    let mut parts = Vec::new();
    for para in &body.paragraphs {
        let resolved = cascade.resolve(&para.properties, None);
        let chunks = chunks_of(para, &resolved, ctx);
        let (text, ..) = render_chunks(&chunks, false);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(" ")
}

// ── SmartArt ────────────────────────────────────────────────────────────────

/// Emit a SmartArt diagram's text, in the graphic frame's place.
///
/// The largest single content gap a shape-tree walk has: 53 corpus parts, all
/// 53 carrying text, 12,102 characters that no walk of the slide can reach.
///
/// Emitted as paragraphs at the frame's position in reading order, which is
/// the part geometry buys us — a document-order reader can only append the
/// diagram wherever its frame happened to be serialised.
///
/// A diagram is a graph, not a document: `dgm:pt` order is the data model's,
/// and the visual arrangement lives in the layout part we do not read. So the
/// text is emitted flat, in data-model order, and nothing here pretends to
/// recover the diagram's shape.
fn emit_diagram(
    data_rel: &str,
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    src: BlockSource,
) {
    let Some(path) = ctx.part.resolve_rel(data_rel) else {
        return;
    };
    let Some(xml) = ctx.package.get_part(&path) else {
        return;
    };
    let Ok(bodies) = pptx::parse_diagram_text(xml) else {
        return;
    };
    let src = match src {
        BlockSource::Slide(n) | BlockSource::Diagram(n) => BlockSource::Diagram(n),
        BlockSource::Notes(n) => BlockSource::Notes(n),
    };
    for body in &bodies {
        emit_text_body(body, None, ctx, out, src);
    }
}

// ── notes ───────────────────────────────────────────────────────────────────

/// Emit a notes slide's body.
///
/// Only the `body` placeholder. The census measured 222,109 characters there
/// against 1,420 characters everywhere else on notes slides, almost all of it a
/// slide number — so this needs no heuristic, and the `sldImg` thumbnail
/// carries no text at all.
fn emit_notes(
    shapes: &[Shape],
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    idx: usize,
) {
    let before = out.len();
    pptx::visit_all(shapes, &mut |shape: &Shape| {
        if !matches!(
            shape.placeholder.as_ref().map(|p| p.kind),
            Some(PlaceholderKind::Body)
        ) {
            return;
        }
        let Some(body) = shape.text() else { return };
        emit_text_body(body, None, ctx, out, BlockSource::Notes(idx));
    });
    if out.len() > before {
        // A rule ahead of the notes marks the boundary, mirroring how the DOCX
        // emitter sets footnote bodies off from the body text.
        out.insert(before, (Block::HorizontalRule, BlockSource::Notes(idx)));
    }
}

// ── inline ──────────────────────────────────────────────────────────────────

/// Flatten a paragraph's inlines to formatting-tagged chunks.
fn chunks_of(para: &TextParagraph, resolved: &ResolvedTextStyle, ctx: &SlideCtx<'_>) -> Vec<Chunk> {
    let mut out = Vec::new();
    walk_inlines(&para.content, None, resolved, ctx, &mut out);
    out
}

fn walk_inlines(
    inlines: &[Inline],
    link: Option<&str>,
    resolved: &ResolvedTextStyle,
    ctx: &SlideCtx<'_>,
    out: &mut Vec<Chunk>,
) {
    for inline in inlines {
        match inline {
            Inline::TextRun(run) => {
                // The cascade fills only what the run leaves unspecified, so a
                // run's own `b="1"` still wins. `apply_to_run` mutates a copy
                // for exactly this reason.
                let mut props = run.properties.clone();
                resolved.apply_to_run(&mut props);
                let fmt = fmt_of(&props);
                let mut text = String::new();
                for el in &run.content {
                    match el {
                        RunElement::Text(t) => text.push_str(t),
                        RunElement::Tab => text.push('\t'),
                        // `a:br` is a hard break inside one text box. 250
                        // paragraphs on the corpus have one, and `Block`'s
                        // single-line `text` cannot hold it, so it becomes a
                        // space rather than being dropped or splitting the
                        // paragraph into two unrelated blocks.
                        RunElement::LineBreak(_) => text.push(' '),
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(Chunk {
                        fmt,
                        link: link.map(str::to_string),
                        text,
                    });
                }
            }
            Inline::Hyperlink(h) => {
                let url = ctx.opts.links.then(|| hyperlink_url(h, ctx)).flatten();
                walk_inlines(&h.content, url.as_deref(), resolved, ctx, out);
            }
            // A field (slide number, date) is already flattened to a plain run
            // by the text parser, so nothing reaches here. Images inside a
            // text body have no text to contribute.
            _ => {}
        }
    }
}

fn hyperlink_url(h: &liteparse_ooxml::model::Hyperlink, ctx: &SlideCtx<'_>) -> Option<String> {
    use liteparse_ooxml::model::HyperlinkTarget as T;
    match &h.target {
        T::ExternalUrl(u) => Some(u.clone()),
        // PPTX hyperlinks are not rewritten at parse time the way DOCX's are,
        // so the rel is resolved here. `Part::resolve_rel` deliberately returns
        // None for external targets — it resolves *part paths* — and a
        // hyperlink is exactly the external case, so the relationship is read
        // directly.
        T::ExternalRel(id) => ctx
            .part
            .rels
            .find_by_id(id.as_str())
            .map(|r| r.target.clone())
            .filter(|t| !t.is_empty()),
        // An internal jump targets a slide, which markdown has no anchor for.
        T::Internal { .. } => None,
    }
}

fn fmt_of(p: &RunProperties) -> Fmt {
    Fmt {
        bold: p.bold.unwrap_or(false),
        italic: p.italic.unwrap_or(false),
        strike: matches!(p.strike, Some(StrikeStyle::Single | StrikeStyle::Double)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse_ooxml::pptx::{Placeholder, PlaceholderKind};

    #[test]
    fn explicit_bu_none_is_not_a_list_item() {
        // 44.7% of corpus paragraphs land here. `buNone` exists to override an
        // inherited bullet, so treating "has a resolved bullet" as "is a list"
        // would turn 3,890 plain paragraphs into list items.
        let mut c = HashMap::new();
        assert!(list_marker(Some(&Bullet::None), 0, &mut c).is_none());
    }

    #[test]
    fn absent_bullet_is_not_a_list_item() {
        let mut c = HashMap::new();
        assert!(list_marker(None, 0, &mut c).is_none());
    }

    #[test]
    fn symbol_font_bullet_never_reaches_the_markdown() {
        // Wingdings' private-use codepoint is the commonest bullet in the
        // corpus and is meaningless as text; `render_blocks` would print it.
        let mut c = HashMap::new();
        let bullet = Bullet::Character {
            char: "\u{f0b7}".into(),
            font: Some("Wingdings".into()),
        };
        let (ordered, marker) =
            list_marker(Some(&bullet), 0, &mut c).expect("a character bullet is a list item");
        assert!(!ordered);
        assert_eq!(marker, "-");
    }

    #[test]
    fn autonumber_counts_up_per_level() {
        let mut c = HashMap::new();
        let b = Bullet::AutoNumber {
            scheme: AutoNumberScheme::ArabicPeriod,
            start_at: None,
        };
        let m = |c: &mut HashMap<u8, u32>, lvl| list_marker(Some(&b), lvl, c).unwrap().1;
        assert_eq!(m(&mut c, 0), "1.");
        assert_eq!(m(&mut c, 0), "2.");
        // A nested level counts independently, and the outer one resumes.
        assert_eq!(m(&mut c, 1), "1.");
        assert_eq!(m(&mut c, 0), "3.");
    }

    #[test]
    fn autonumber_honors_start_at() {
        let mut c = HashMap::new();
        let b = Bullet::AutoNumber {
            scheme: AutoNumberScheme::ArabicParenR,
            start_at: Some(4),
        };
        assert_eq!(list_marker(Some(&b), 0, &mut c).unwrap().1, "4)");
    }

    #[test]
    fn alpha_continues_past_z_instead_of_wrapping() {
        assert_eq!(alpha(1, b'a'), "a");
        assert_eq!(alpha(26, b'a'), "z");
        assert_eq!(alpha(27, b'a'), "aa");
        assert_eq!(alpha(28, b'A'), "AB");
    }

    #[test]
    fn roman_falls_back_rather_than_emitting_nothing() {
        assert_eq!(roman(4), "IV");
        assert_eq!(roman(1987), "MCMLXXXVII");
        // Out of range: an empty marker would be worse than an arabic one.
        assert_eq!(roman(0), "0");
        assert_eq!(roman(4000), "4000");
    }

    fn ph(kind: PlaceholderKind) -> Option<Placeholder> {
        Some(Placeholder { kind, idx: 0 })
    }

    #[test]
    fn chrome_roles_are_recognised_and_titles_are_not() {
        for k in [
            PlaceholderKind::Dt,
            PlaceholderKind::Ftr,
            PlaceholderKind::SldNum,
            PlaceholderKind::Hdr,
            PlaceholderKind::SldImg,
        ] {
            assert!(is_chrome(ph(k).as_ref()), "{k:?} should be chrome");
        }
        assert!(!is_chrome(ph(PlaceholderKind::Title).as_ref()));
        assert!(!is_chrome(ph(PlaceholderKind::Body).as_ref()));
        // 59% of corpus slide text sits on shapes with no placeholder, so the
        // absent case must never be chrome.
        assert!(!is_chrome(None));
    }

    #[test]
    fn both_title_kinds_count_as_titles() {
        assert!(is_title(ph(PlaceholderKind::Title).as_ref()));
        assert!(is_title(ph(PlaceholderKind::CtrTitle).as_ref()));
        assert!(!is_title(ph(PlaceholderKind::SubTitle).as_ref()));
        assert!(!is_title(None));
    }
}
