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

use liteparse_ooxml::model::{ColorMap, Inline, RunElement};
use liteparse_ooxml::pptx::{
    self, AutoNumberScheme, Background, BackgroundSource, Bullet, DeckTextDefaults,
    GraphicFramePayload, ListStyle, MatchRule, Placeholder, PlaceholderGeometry, PlaceholderKind,
    PlaceholderTextStyles, PresentationPackage, ResolvedTextStyle, Shape, ShapeKind, Table,
    TextBody, TextCascade, TextParagraph, TextStyles,
};
use liteparse_ooxml::render::layout::draw_command::ResolvedFill;
use liteparse_ooxml::render::resolve::images::PartMedia;
use liteparse_ooxml::render::resolve::shape_visuals::resolve_blip_fill;
use std::collections::HashMap;

use crate::error::LiteParseError;
use crate::markdown_layout::{Block, Cell};
use crate::office::figures::FigureSink;
use crate::office::inline::{Chunk, fmt_of, render_chunks};
use crate::types::{ExtractedImage, Rect};

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
    /// Emit `Block::Figure` refs for the deck's pictures. Mirrors the PDF and
    /// DOCX gate, `image_mode != Off`.
    pub figures: bool,
    /// Keep the bytes behind those pictures in [`NativeDeck::images`]. Mirrors
    /// `effective_extract_images`.
    ///
    /// Separate from `figures` because the two config gates are separate: an
    /// `image_mode = Off` parse with `extract_images` on wants the bytes and
    /// no markdown refs, and the PDF and DOCX paths both honour that pair.
    /// **Ids do not depend on either flag** — the picture walk assigns them
    /// whenever it runs at all, so `img_p3_2` names the same picture however
    /// the caller asked for it.
    pub images: bool,
}

/// What one native-parsed deck yields: the block stream, and the pictures
/// behind whatever `Block::Figure`s are in it.
///
/// One walk produces both, which is the structural difference from the DOCX
/// path. There, ids come from the layout walk and figures from the structure
/// walk, so `docx_layout::NativeImages` has to carry a media-pointer → id FIFO
/// to rejoin them. A slide's shapes already carry composed geometry *and* their
/// reading order, so the id can be assigned where the figure is emitted and the
/// join never has to exist.
pub struct NativeDeck {
    pub blocks: Vec<(Block, BlockSource)>,
    /// Empty unless [`EmitOptions::images`] asked for the bytes. Placement
    /// order, deduplicated: repeats carry `duplicate_of` and share the
    /// canonical entry's `Arc`, matching the PDF and DOCX contract.
    pub images: Vec<ExtractedImage>,
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
        .blocks
        .into_iter()
        .map(|(b, _)| b)
        .collect())
}

/// Emit blocks tagged with the slide they came from, and the pictures behind
/// their figures.
///
/// Returns the pair rather than the blocks alone so that a caller cannot ask
/// for figures and quietly drop the bytes those refs name — the refs would
/// point at files nothing ever wrote.
pub fn emit_with_sources(data: &[u8], opts: EmitOptions) -> Result<NativeDeck, LiteParseError> {
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
    /// §19.3.1.6 `p:clrMap` per master and §19.3.1.7 `p:clrMapOvr` per layout,
    /// cached beside the background for the same reason. `None` means the part
    /// states no map of its own.
    master_clr_map: HashMap<String, Option<ColorMap>>,
    layout_clr_map: HashMap<String, Option<ColorMap>>,
    /// The **non-placeholder** shapes each rung contributes to every slide
    /// under it, geometry already composed. See [`inherited_shapes`] for why
    /// the placeholders are dropped here rather than skipped at paint time.
    ///
    /// Cached beside the other rungs for the same reason, and it matters more
    /// here than anywhere else: this is a whole shape tree, not one element.
    master_shapes: HashMap<String, Vec<Shape>>,
    layout_shapes: HashMap<String, Vec<Shape>>,
    /// §19.3.1.39 `p:sldLayout/@showMasterSp` — whether slides under this
    /// layout draw the master's shapes. 25 of the corpus's 415 layouts say no.
    layout_shows_master: HashMap<String, bool>,
    /// Image relationships per part, over one shared pool of bytes. On the
    /// deck for the same reason the other rungs are: a layout's media table is
    /// rebuilt 60 times otherwise, and its `Arc`s must be the *same* `Arc`s
    /// each time or the rasterizer's bitmap cache misses on every slide.
    media: pptx::MediaCache,
    default_text_style: ListStyle,
    /// The rung-7-only defaults a slide falls back to when its master has no
    /// entry. Stored on the deck rather than built as a local in the walk so
    /// that [`PreparedSlide`] can borrow it and every walk builds the *same*
    /// `TextCascade` — see [`Deck::prepare`].
    fallback_defaults: DeckTextDefaults,
    /// How many slides of this deck an inherited string lands on, keyed by the
    /// string itself. Built once in [`Deck::index_inherited_text`].
    ///
    /// Deck-wide by necessity: whether a layout's text is furniture or content
    /// is not a property of the slide showing it, and cannot be decided while
    /// standing on one. See [`PreparedSlide::inherited_text`].
    inherited_text_counts: HashMap<String, usize>,
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
    /// §19.3.1.6 the colour map in force on this slide, resolved down the same
    /// slide → layout → master chain as `background`.
    ///
    /// `None` means no part in the chain stated one, in which case
    /// [`ColorMap::default`] — the spec's identity mapping — applies; it is
    /// carried as `Option` rather than defaulted here so that the probe can
    /// tell "stated" from "assumed", which is exactly the 30-of-70 masters the
    /// census found.
    pub color_map: Option<ColorMap>,
    /// The master's and then the layout's own shapes, in the order they paint:
    /// **under** everything in `shapes`, master first. Empty when the rung
    /// contributes none, or when a `@showMasterSp="0"` turns it off.
    ///
    /// Borrowed, not cloned: unlike `background` this is a whole shape tree per
    /// part, shared by every slide under it.
    ///
    /// Read by the paint walk only: it is the whole rung, placeholders already
    /// dropped, and most of it is furniture with no text at all. The text these
    /// shapes carry reaches a reader through `inherited_text`, which is a much
    /// smaller list and a different rule.
    pub inherited: [&'d [Shape]; 2],
    /// The inherited shapes whose text a reader should actually get:
    /// `[master, layout]`, parallel to `inherited`, groups already flattened so
    /// each entry is a `p:sp` that carries its own composed rectangle.
    ///
    /// **This is a filtered list, and the filter is the point.** A layout shape
    /// is drawn on every slide that uses the layout, so emitting its text
    /// per-slide reprints the deck's furniture once per page: the census found
    /// 371 occurrences behind just 43 distinct strings, one of them a speaker
    /// banner landing on 47 slides and another the literal `‹#›` of a slide
    /// number. [`Deck::keeps_inherited_text`] admits only the strings that land
    /// on exactly one slide of the deck — 29 of the 371 — which is what makes
    /// this a content fix rather than a letterhead regression.
    ///
    /// Both walks read it, and neither re-derives it: the markdown a reader
    /// gets and the boxes the geometry pass reports are the same shapes by
    /// construction.
    pub inherited_text: [Vec<&'d Shape>; 2],
    /// The inherited shapes that are **pictures**: `[master, layout]`, parallel
    /// to `inherited`, groups already flattened.
    ///
    /// Unfiltered, and that is the difference from `inherited_text` beside it.
    /// The furniture rule there exists because reprinting a layout's speaker
    /// banner on all 47 slides that use the layout is a worse markdown than
    /// omitting it. A repeated *picture* has an answer that repeated text does
    /// not: the dedup contract gives every placement its own `name`, points
    /// them all at one canonical `path`, and `rewrite_duplicate_image_refs`
    /// resolves the refs — so 3,602 corpus placements cost 1,322 files.
    ///
    /// The conversion path this replaces already emits them, which is what
    /// settles it: LibreOffice flattens the master into every page, so
    /// `--no-office-native` prints 53 refs from 4 files on `bud_cnos` where
    /// this path printed none. Emitting them is parity, not noise.
    pub inherited_figures: [Vec<&'d Shape>; 2],
    /// Whether each arm of `inherited` is empty because it was **declined**
    /// rather than because the part had nothing to give: `[master, layout]`,
    /// parallel to `inherited`.
    ///
    /// The distinction is the whole reason `@showMasterSp` is parsed. Both
    /// cases paint nothing, so without this a deck that correctly honoured 25
    /// opt-outs and a deck whose layout parse silently failed would report the
    /// same numbers.
    pub declined: [bool; 2],
    /// The slide's own `r:embed` table, and its master's and layout's in the
    /// same `[master, layout]` order as `inherited`.
    ///
    /// Three tables rather than one because an `r:id` is scoped to the part
    /// that writes it: a layout picture's `rId1` and the slide's `rId1` are
    /// unrelated relationships that regularly name different images, so a
    /// single merged table would paint real photographs in the wrong places.
    pub media: &'d PartMedia,
    pub inherited_media: [&'d PartMedia; 2],
    /// The table for the part the **background** came from, which is the
    /// master's on 1,125 corpus slides and the slide's own on 58. Carried
    /// separately rather than derived by the caller, because the rung is
    /// already known here (`BackgroundSource`) and re-deriving it at the paint
    /// site is how the two would drift.
    pub background_media: &'d PartMedia,
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
        let mut deck = Self {
            master_geo: HashMap::new(),
            layout_geo: HashMap::new(),
            master_text: HashMap::new(),
            layout_text: HashMap::new(),
            master_bg: HashMap::new(),
            layout_bg: HashMap::new(),
            master_clr_map: HashMap::new(),
            layout_clr_map: HashMap::new(),
            master_shapes: HashMap::new(),
            layout_shapes: HashMap::new(),
            layout_shows_master: HashMap::new(),
            media: pptx::MediaCache::new(),
            fallback_defaults: DeckTextDefaults {
                master_styles: TextStyles::default(),
                default_text_style: default_text_style.clone(),
            },
            default_text_style,
            inherited_text_counts: HashMap::new(),
        };
        deck.index_inherited_text(pkg);
        deck
    }

    /// Count, deck-wide, how many slides each inherited string lands on.
    ///
    /// Runs here rather than lazily because the answer is not knowable from one
    /// slide, and both walks need it on their first. It primes the rung caches
    /// as it goes, so the parts it reads are the ones the walks would have
    /// parsed anyway — the only work this adds is one attribute read per slide.
    ///
    /// That attribute is `@showMasterSp`, and it is read through
    /// [`pptx::shows_inherited_shapes`] rather than by deserializing the slide:
    /// a slide that declines a rung draws none of its text, so counting its
    /// occurrences would push a string off the "lands on one slide" rule and
    /// silently drop content. Getting it from a second `parse_slide_part` per
    /// slide measured at +7.5% on the corpus's markdown run, for one boolean.
    fn index_inherited_text(&mut self, pkg: &PresentationPackage) {
        for slide in &pkg.slides {
            self.prime(slide);
            let shows_layout = pptx::shows_inherited_shapes(&slide.slide.xml);
            let mut found = Vec::new();
            for shapes in self.inherited_rungs(slide, shows_layout).0 {
                collect_text_shapes(shapes, &mut found);
            }
            // The keys are owned before the counter is touched: `found` borrows
            // the rung caches, and the tally borrows `self` mutably.
            let keys: Vec<String> = found.into_iter().map(|(_, key)| key).collect();
            for key in keys {
                *self.inherited_text_counts.entry(key).or_default() += 1;
            }
        }
    }

    /// Whether an inherited string is content rather than furniture.
    ///
    /// **Lands on exactly one slide of the deck.** Measured against the
    /// alternative — "comes from a rung part that exactly one slide draws",
    /// which needs no string keying — on the 45-deck corpus: the two keep the
    /// same 29 occurrences, and the rung rule additionally emits 10 the text
    /// rule rejects. All 10 are furniture, and one of them is an authoring note
    /// to whoever edits the deck ("Glegoo is een niet-Windows-Font…"), which is
    /// as clear a statement as the corpus can make about which axis is right.
    fn keeps_inherited_text(&self, key: &str) -> bool {
        self.inherited_text_counts.get(key) == Some(&1)
    }

    /// The `[master, layout]` shape trees a slide draws, and whether each empty
    /// arm was **declined** rather than absent.
    ///
    /// Shared by [`Deck::prepare`] and [`Deck::index_inherited_text`] because
    /// the two must agree about what a slide shows: a tally that counted a rung
    /// the walk then declines would drop a string that is on one page only.
    ///
    /// §19.2.1.32: the slide's own `@showMasterSp` turns off *everything* the
    /// rungs above supply, so it gates the master arm as well as the layout's —
    /// a slide that declines its layout's design does not then take the
    /// master's, which the layout was itself drawing over. §19.3.1.39: the
    /// layout's gates only the master's.
    fn inherited_rungs(
        &self,
        slide: &pptx::SlideParts,
        shows_layout: bool,
    ) -> ([&[Shape]; 2], [bool; 2]) {
        let layout = slide.layout.as_ref();
        let shows_master = shows_layout
            && layout
                .and_then(|l| self.layout_shows_master.get(&l.path))
                .copied()
                .unwrap_or(true);
        (
            [
                if shows_master {
                    slide
                        .master
                        .as_ref()
                        .and_then(|m| self.master_shapes.get(&m.path))
                        .map_or(&[][..], Vec::as_slice)
                } else {
                    &[]
                },
                if shows_layout {
                    layout
                        .and_then(|l| self.layout_shapes.get(&l.path))
                        .map_or(&[][..], Vec::as_slice)
                } else {
                    &[]
                },
            ],
            [!shows_master, !shows_layout],
        )
    }

    /// Prime the cascades for this slide's layout/master, parse its shape
    /// tree, and compose its geometry.
    ///
    /// `None` when the shape tree will not parse — fail-open, the same slide
    /// is skipped by every walk.
    pub fn prepare(
        &mut self,
        pkg: &PresentationPackage,
        slide: &pptx::SlideParts,
    ) -> Option<PreparedSlide<'_>> {
        self.prime(slide);
        // Primed before anything borrows, because building a table needs
        // `&mut self` and the three that come out of it are held at once.
        self.media.part_media(pkg, &slide.slide);
        for part in [slide.layout.as_ref(), slide.master.as_ref()]
            .into_iter()
            .flatten()
        {
            self.media.part_media(pkg, part);
        }

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
        // Nearest rung wins, and a `p:clrMapOvr/a:masterClrMapping` parses to
        // `None`, which is what makes "inherit" fall through correctly here.
        let color_map = part
            .color_map
            .or_else(|| {
                slide
                    .layout
                    .as_ref()
                    .and_then(|l| self.layout_clr_map.get(&l.path))
                    .copied()
                    .flatten()
            })
            .or_else(|| {
                slide
                    .master
                    .as_ref()
                    .and_then(|m| self.master_clr_map.get(&m.path))
                    .copied()
                    .flatten()
            });
        let layout_geo = slide
            .layout
            .as_ref()
            .and_then(|l| self.layout_geo.get(&l.path));
        // Cascade before composition: a placeholder has no rectangle to
        // compose until it has inherited one.
        pptx::apply_inherited_geometry(&mut shapes, layout_geo, MatchRule::Idx);
        pptx::apply_slide_geometry(&mut shapes);

        let (inherited, declined) = self.inherited_rungs(slide, part.show_inherited_shapes);
        // Filtered here rather than at either walk's door, for the same reason
        // `inherited_shapes` drops placeholders at cache-build time: what comes
        // out of `prepare` should be exactly what the walks may show, so the
        // markdown and the geometry cannot apply the rule differently.
        let inherited_text = inherited.map(|shapes| {
            let mut found = Vec::new();
            collect_text_shapes(shapes, &mut found);
            found
                .into_iter()
                .filter(|(_, key)| self.keeps_inherited_text(key))
                .map(|(shape, _)| shape)
                .collect()
        });

        // Same placement as `inherited_text`, and for the same reason: what
        // comes out of `prepare` should be exactly what the walks may show.
        let inherited_figures = inherited.map(|shapes| {
            let mut found = Vec::new();
            collect_figure_shapes(shapes, &mut found);
            found
        });

        let master = slide
            .master
            .as_ref()
            .and_then(|m| self.master_text.get(&m.path));
        let layout_path = slide.layout.as_ref().map(|l| l.path.as_str());
        let master_path = slide.master.as_ref().map(|m| m.path.as_str());
        // The background's own rung, not the slide's. `resolve_background`
        // already decided which part won; asking it again here is the whole
        // point of carrying `BackgroundSource` on the result.
        let background_media = self
            .media
            .get(match background.as_ref().map(|(src, _)| src) {
                Some(BackgroundSource::Layout) => layout_path,
                Some(BackgroundSource::Master) => master_path,
                _ => Some(slide.slide.path.as_str()),
            });
        Some(PreparedSlide {
            shapes,
            background,
            color_map,
            inherited,
            inherited_text,
            inherited_figures,
            declined,
            media: self.media.get(Some(slide.slide.path.as_str())),
            inherited_media: [self.media.get(master_path), self.media.get(layout_path)],
            background_media,
            layout_text: slide
                .layout
                .as_ref()
                .and_then(|l| self.layout_text.get(&l.path)),
            master_text: master.map(|(p, _)| p),
            deck_defaults: master.map_or(&self.fallback_defaults, |(_, d)| d),
        })
    }

    fn emit(&mut self, pkg: &PresentationPackage, opts: EmitOptions) -> NativeDeck {
        let mut out = Vec::new();
        let mut figures = FigureSink::default();

        for (idx, slide) in pkg.slides.iter().enumerate() {
            let Some(prepared) = self.prepare(pkg, slide) else {
                continue;
            };
            figures.reset_ordinal();
            let mut ctx = SlideCtx {
                cascade: prepared.cascade(),
                part: &slide.slide,
                media: prepared.media,
                package: &pkg.package,
                opts,
                page: (idx + 1) as u32,
                figures: &mut figures,
            };
            for read in slide_reading_order(&prepared) {
                // The part an `r:id` resolves in — see [`Reading::rung`]. The
                // fallback cannot fire: a rung contributes shapes only when
                // `inherited_rungs` found its part, which is the same `Option`.
                //
                // `media` moves with `part` and for the same reason: a layout
                // logo's `rId2` and the slide's `rId2` are different pictures,
                // so a figure resolved against the wrong table would extract a
                // real image the author never put on that slide.
                (ctx.part, ctx.media) = match read.rung {
                    Some(0) => (
                        slide.master.as_ref().unwrap_or(&slide.slide),
                        prepared.inherited_media[0],
                    ),
                    Some(_) => (
                        slide.layout.as_ref().unwrap_or(&slide.slide),
                        prepared.inherited_media[1],
                    ),
                    None => (&slide.slide, prepared.media),
                };
                emit_shape(read.shape, &mut ctx, &mut out, BlockSource::Slide(idx));
            }
            // Put the slide's own part back before anything else reads `ctx`:
            // the notes walk below resolves its rels through this field, and
            // leaving it pointing at whichever rung happened to sort last would
            // be a bug that only shows on slides that inherit text.
            ctx.part = &slide.slide;
            ctx.media = prepared.media;

            if opts.notes
                && let Some(notes) = &slide.notes
                && let Ok(nshapes) = pptx::parse_shape_tree(&notes.xml)
            {
                emit_notes(&nshapes, &mut ctx, &mut out, idx);
            }
        }
        NativeDeck {
            blocks: out,
            images: if opts.images {
                figures.images
            } else {
                Vec::new()
            },
        }
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
            self.master_shapes
                .insert(master.path.clone(), inherited_shapes(&shapes));
            self.master_bg.insert(master.path.clone(), part.background);
            self.master_clr_map
                .insert(master.path.clone(), part.color_map);
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
            self.layout_clr_map
                .insert(layout.path.clone(), part.color_map);
            self.layout_shows_master
                .insert(layout.path.clone(), part.show_inherited_shapes);
            pptx::apply_inherited_geometry(&mut shapes, master_geo, MatchRule::CollapsedKind);
            // After the placeholder cascade, before `from_layout` indexes it:
            // both read `Shape::transform`, which neither this nor the
            // composition below touches, so the order between them is free —
            // but the cascade above must come first, since a placeholder with
            // no rectangle of its own has nothing to compose.
            self.layout_shapes
                .insert(layout.path.clone(), inherited_shapes(&shapes));
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

/// The shapes a layout or master part contributes to every slide beneath it:
/// its **non-placeholder** shapes, with their group transforms composed.
///
/// PowerPoint draws a layout's ordinary shapes — the full-bleed panel, the
/// rule, the logo strip — on every slide that uses the layout. It does *not*
/// draw the layout's placeholders: those are prototypes, and the slide's own
/// placeholder is the thing that renders. Painting them would put every
/// layout's "Click to edit Master title style" prompt on the deck, and would
/// double-paint the fill of every placeholder a slide does use.
///
/// So the filter is the correctness rule, not an optimisation, and it is
/// applied here rather than at paint time so that the cache holds exactly what
/// the painter may draw. Top-level only, matching
/// `PlaceholderGeometry::index`: a placeholder never appears inside a group.
///
/// Corpus: 1,567 of 4,432 layout shapes and 179 of 395 master shapes survive.
fn inherited_shapes(shapes: &[Shape]) -> Vec<Shape> {
    let mut kept: Vec<Shape> = shapes
        .iter()
        .filter(|s| s.placeholder.is_none())
        .cloned()
        .collect();
    // These shapes never reach `Deck::prepare`'s composition pass — they are
    // not the slide's — so they compose here, once per part rather than once
    // per slide using it.
    pptx::apply_slide_geometry(&mut kept);
    kept
}

// ── figures ─────────────────────────────────────────────────────────────────

/// The blip fill a shape carries as its *picture*, if it is one.
///
/// Two shapes qualify and the second is the one the census had to find:
///
/// * `p:pic` — §19.3.1.37, the picture frame, whose image is its own
///   `p:blipFill` sibling of `spPr` rather than an `spPr` fill.
/// * **a `p:sp` with a blip fill and no text** — a picture in all but name.
///   One corpus deck (`twinning_the_results_reviewed`) declares *zero* `p:pic`
///   and puts all 37 of its images on 56 such shapes, so a `p:pic`-only rule
///   takes that deck from the conversion path's 45 refs and 29 files to
///   nothing at all.
///
/// The text test is what separates the two populations, and it separates them
/// almost perfectly: 140 of 141 slide-level blip-filled `p:sp`s carry no text
/// (pictures), while 27 of 29 layout-level ones do (a template banner *behind*
/// a heading, which is a backdrop and not a figure).
fn picture_fill(shape: &Shape) -> Option<&liteparse_ooxml::model::BlipFill> {
    match &shape.kind {
        ShapeKind::Picture(pic) => Some(&pic.blip_fill),
        ShapeKind::AutoShape(sp) => {
            if shape_has_text(sp) {
                return None;
            }
            match sp.properties.as_ref()?.fill.as_ref()? {
                liteparse_ooxml::model::DrawingFill::Blip(fill) => Some(fill),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether a `p:sp` shows any text at all.
///
/// An absent `p:txBody` and one holding only empty runs are different things in
/// the model and the same thing to a reader, which is the distinction that
/// matters for [`picture_fill`].
fn shape_has_text(sp: &liteparse_ooxml::pptx::AutoShape) -> bool {
    sp.text.as_ref().is_some_and(|body| {
        body.paragraphs
            .iter()
            .any(|para| !para.text().trim().is_empty())
    })
}

struct SlideCtx<'a> {
    cascade: TextCascade<'a>,
    /// The slide part, for resolving relationships. Hyperlinks need the raw
    /// external target and SmartArt needs a resolved part path, so both the
    /// relationship table and the part's own directory are required.
    part: &'a pptx::Part,
    /// The `r:embed` table for the part `part` points at, retargeted with it.
    ///
    /// Two fields moving together rather than one derived from the other,
    /// because they come from different places: `part` is the package part and
    /// this is `Deck`'s per-part media cache. They must stay in step — an
    /// inherited picture resolved against the *slide's* table returns a real
    /// photograph from an unrelated relationship, the same trap `Reading::rung`
    /// exists to close for hyperlinks.
    media: &'a PartMedia,
    package: &'a liteparse_ooxml::docx::zip::PackageContents,
    opts: EmitOptions,
    /// 1-based page number of the slide being emitted, for figure ids.
    page: u32,
    figures: &'a mut FigureSink,
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
    v.sort_by_key(|s| reading_key(s));
    v
}

/// One shape's position in the reading order: title first, then top-to-bottom
/// in bands, then left-to-right.
///
/// Factored out of [`reading_order`] so that [`slide_reading_order`], which
/// sorts a list the former cannot build, is provably the same order rather than
/// a copy that agrees today.
fn reading_key(shape: &Shape) -> (bool, i64, i64) {
    let (band, x) = match shape.slide_rect {
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
    (!is_title(shape.placeholder.as_ref()), band, x)
}

/// A shape in a slide's reading order, and which tree it came out of.
#[doc(hidden)]
pub struct Reading<'a> {
    pub shape: &'a Shape,
    /// `None` for the slide's own `p:spTree`; `Some(0)` for the master's and
    /// `Some(1)` for the layout's — the same `[master, layout]` index as
    /// [`PreparedSlide::inherited`] and `inherited_media`.
    ///
    /// Carried rather than derived because an inherited shape's `r:id`s resolve
    /// in the part that wrote them: a layout's `rId3` and the slide's `rId3`
    /// are unrelated relationships, and a hyperlink read from the wrong table
    /// is a real URL pointing somewhere the author never wrote.
    pub rung: Option<usize>,
    /// Whether this shape is in the order as a **picture** — a `p:pic`, or a
    /// textless blip-filled `p:sp` — rather than as text.
    ///
    /// Carried rather than re-derived because the two populations arrive from
    /// different lists (`inherited_text` and `inherited_figures`) and a
    /// consumer asking "is this inherited?" is no longer asking the same
    /// question as "is this inherited text?". The geometry pass's
    /// `inherited_text_laid_out` counter is exactly that consumer: keyed on
    /// `rung` alone it would silently start counting logos.
    pub figure: bool,
}

/// Everything a slide shows, in one reading order: its own shapes and the
/// inherited text that survived the furniture rule.
///
/// **One sort, not two passes.** An inherited shape is a backdrop in *paint*
/// order — that is why the paint walk draws the rungs before the slide — but it
/// is not a backdrop in *reading* order: `waterun`'s inherited subtitle sits
/// under the slide's title and reads there. Appending the rungs and sorting
/// once puts each string where it appears on the page, which is the only
/// ordering claim this module ever makes.
///
/// The slide's own shapes are inserted first, so on a tie — same band, same x —
/// the slide's text reads before the template's. `sort_by_key` is stable, so
/// that is a property of the order rather than an accident of it.
///
/// Public for the same reason [`reading_order`] is: a probe must measure this
/// traversal, not a re-implementation of it.
#[doc(hidden)]
pub fn slide_reading_order<'a>(prepared: &'a PreparedSlide<'_>) -> Vec<Reading<'a>> {
    let mut v: Vec<Reading<'a>> = prepared
        .shapes
        .iter()
        .filter(|s| !is_chrome(s.placeholder.as_ref()))
        .map(|shape| Reading {
            shape,
            rung: None,
            figure: picture_fill(shape).is_some(),
        })
        .collect();
    for (figure, rung_shapes) in [
        (false, &prepared.inherited_text),
        (true, &prepared.inherited_figures),
    ] {
        for (rung, shapes) in rung_shapes.iter().enumerate() {
            // No chrome filter: `is_chrome` reads a `p:ph`, and an inherited
            // shape has none by construction — `inherited_shapes` dropped every
            // placeholder before these reached the cache.
            v.extend(shapes.iter().map(|&shape| Reading {
                shape,
                rung: Some(rung),
                figure,
            }));
        }
    }
    v.sort_by_key(|r| reading_key(r.shape));
    v
}

/// Every text-bearing `p:sp` in a rung's tree, with the string the furniture
/// rule counts, groups flattened.
///
/// Flattened rather than walked as a tree because the rule is per *string*: a
/// group holding a logo and a strapline is two decisions, not one, and the
/// shapes come out of `inherited_shapes` with their group transforms already
/// composed — so a leaf carries its own rectangle and needs no parent.
///
/// The key joins the paragraphs a reader would see, and is what
/// `bench/pptx_corpus/inherited_text_census.py` counts, so the census's numbers
/// and this module's are about the same strings.
fn collect_text_shapes<'a>(shapes: &'a [Shape], out: &mut Vec<(&'a Shape, String)>) {
    for shape in shapes {
        match &shape.kind {
            ShapeKind::AutoShape(sp) => {
                if let Some(body) = &sp.text {
                    let mut key = String::new();
                    for para in &body.paragraphs {
                        let line = para.text();
                        if line.trim().is_empty() {
                            continue;
                        }
                        if !key.is_empty() {
                            key.push('\n');
                        }
                        key.push_str(&line);
                    }
                    let key = key.trim();
                    if !key.is_empty() {
                        out.push((shape, key.to_string()));
                    }
                }
            }
            ShapeKind::Group(group) => collect_text_shapes(&group.children, out),
            // A `p:graphicFrame`'s table is a second gap of a different shape —
            // 2 of them in layout parts against 237 `p:sp`s — and folding the
            // two would make neither number act on.
            _ => {}
        }
    }
}

/// Every picture in a rung's tree, groups flattened.
///
/// Flattened for the same reason [`collect_text_shapes`] is: `inherited_shapes`
/// has already composed group transforms, so a leaf carries its own composed
/// rectangle and needs no parent to place it.
///
/// Uses the same [`picture_fill`] test as the slide's own shapes, so a layout's
/// blip-filled panel and a slide's are the same kind of thing to this module.
fn collect_figure_shapes<'a>(shapes: &'a [Shape], out: &mut Vec<&'a Shape>) {
    for shape in shapes {
        if picture_fill(shape).is_some() {
            out.push(shape);
        }
        if let ShapeKind::Group(group) = &shape.kind {
            collect_figure_shapes(&group.children, out);
        }
    }
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
    // A shape is a picture *or* it is text — never both. `picture_fill`
    // declines a blip-filled shape that carries text, because there the image
    // is a backdrop the text sits on and the text is the content.
    if let Some(fill) = picture_fill(shape) {
        emit_figure(shape, fill, ctx, out, src);
        return;
    }
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
        // A picture whose bytes we do not surface (EMF, or an SVG with no
        // raster fallback) reaches here, as does every connector. Neither
        // carries text.
        ShapeKind::Picture(_) | ShapeKind::Connector(_) => {}
    }
}

/// Emit one picture as a `Block::Figure`, and record its bytes.
///
/// Nothing is emitted when the caller asked for neither figures nor images,
/// when the shape has no composed rectangle, or when `resolve_blip_fill`
/// declines the image. That resolver is shared with the paint walk rather than
/// re-implemented, so the set of pictures a reader gets a ref for and the set
/// the rasterizer draws cannot drift apart — it is also what applies the
/// `a:tile`, `r:link` and unresolvable-`r:embed` rules, each of which is a
/// picture with no bytes to hand back.
fn emit_figure(
    shape: &Shape,
    fill: &liteparse_ooxml::model::BlipFill,
    ctx: &mut SlideCtx<'_>,
    out: &mut Vec<(Block, BlockSource)>,
    src: BlockSource,
) {
    if !ctx.opts.figures && !ctx.opts.images {
        return;
    }
    let ResolvedFill::Blip(blip) = resolve_blip_fill(fill, Some(ctx.media)) else {
        return;
    };
    // The *bounding* box, matching the paint walk: `pptx::geometry` composes
    // group transforms into `slide_rect`, so a grouped or rotated picture's
    // placement is the box that composition produced.
    let Some(rect) = shape.slide_rect else { return };
    let box_ = rect.bounding_box();
    let bbox = Rect {
        x: emu_to_pt(box_.origin.x.raw()),
        y: emu_to_pt(box_.origin.y.raw()),
        width: emu_to_pt(box_.size.width.raw()),
        height: emu_to_pt(box_.size.height.raw()),
    };
    let Some((id, format)) = ctx.figures.place(
        &blip.data,
        blip.format,
        &format!("p{}", ctx.page),
        ctx.page,
        bbox,
    ) else {
        return;
    };
    if ctx.opts.figures {
        out.push((Block::Figure { id, format }, src));
    }
}

/// EMU → points. 914,400 EMU to the inch, 72 points to the inch.
fn emu_to_pt(emu: i64) -> f32 {
    emu as f32 / 12_700.0
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

    // ── inherited text ──────────────────────────────────────────────────────

    /// A shape tree from a fragment, with geometry composed the way
    /// `inherited_shapes` composes a rung's before it caches it.
    fn tree(inner: &str) -> Vec<Shape> {
        let xml = format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                      xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
                 <p:cSld><p:spTree>
                   <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                   <p:grpSpPr/>
                   {inner}
                 </p:spTree></p:cSld>
               </p:sld>"#
        );
        let mut shapes = pptx::parse_shape_tree(xml.as_bytes()).expect("parses");
        pptx::apply_slide_geometry(&mut shapes);
        shapes
    }

    /// A `p:sp` at `(x, y)` EMU carrying one paragraph per line of `text`.
    fn text_shape(id: u32, x: i64, y: i64, text: &[&str]) -> String {
        let paras: String = text
            .iter()
            .map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>"))
            .collect();
        format!(
            r#"<p:sp>
                 <p:nvSpPr><p:cNvPr id="{id}" name="s{id}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                 <p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr>
                 <p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>
               </p:sp>"#
        )
    }

    /// The key is the string the census counts: paragraphs joined by newline,
    /// blank ones skipped. A key that drifted from the census's would make
    /// every number in the plan's write-up about a different population.
    #[test]
    fn the_key_joins_the_paragraphs_a_reader_would_see() {
        let shapes = tree(&text_shape(
            2,
            0,
            0,
            &["Thank you", "", "for your", "attention"],
        ));
        let mut found = Vec::new();
        collect_text_shapes(&shapes, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "Thank you\nfor your\nattention");
    }

    /// A rung's logo-plus-strapline group is two strings, not one: the rule is
    /// per string, and a group that kept its children together would let a
    /// repeated logo drag a one-off strapline out of the output with it.
    #[test]
    fn groups_are_flattened_into_one_decision_per_string() {
        let shapes = tree(&format!(
            r#"<p:grpSp>
                 <p:nvGrpSpPr><p:cNvPr id="9" name="g"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                 <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/>
                   <a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/></a:xfrm></p:grpSpPr>
                 {}{}
               </p:grpSp>"#,
            text_shape(10, 0, 0, &["ACME"]),
            text_shape(11, 0, 100, &["a one-off strapline"]),
        ));
        let mut found = Vec::new();
        collect_text_shapes(&shapes, &mut found);
        let keys: Vec<&str> = found.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(keys, ["ACME", "a one-off strapline"]);
    }

    /// A picture or a connector carries no `p:txBody`, and an empty one is not
    /// a string — neither may enter the tally, or the counts stop matching the
    /// census and a blank shape starts competing for a "lands on one slide".
    #[test]
    fn shapes_with_nothing_to_say_are_not_counted() {
        let shapes = tree(&format!(
            r#"{}<p:cxnSp><p:nvCxnSpPr><p:cNvPr id="4" name="c"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr>
                 <p:spPr/></p:cxnSp>"#,
            text_shape(3, 0, 0, &["", "  "]),
        ));
        let mut found = Vec::new();
        collect_text_shapes(&shapes, &mut found);
        assert!(found.is_empty(), "{found:?} should be empty");
    }

    /// A `PreparedSlide` holding hand-built trees, so the merged order can be
    /// tested without a package on disk.
    fn prepared<'a>(
        shapes: Vec<Shape>,
        inherited: &'a [Shape],
        defaults: &'a DeckTextDefaults,
        media: &'a PartMedia,
    ) -> PreparedSlide<'a> {
        PreparedSlide {
            shapes,
            background: None,
            color_map: None,
            inherited: [&[], &[]],
            inherited_text: [Vec::new(), inherited.iter().collect()],
            inherited_figures: [Vec::new(), Vec::new()],
            declined: [false, false],
            media,
            inherited_media: [media, media],
            background_media: media,
            layout_text: None,
            master_text: None,
            deck_defaults: defaults,
        }
    }

    /// Inherited text is a backdrop in *paint* order and not in *reading*
    /// order: `waterun`'s inherited strapline sits above the slide's own first
    /// line and reads there, which one sort over both trees gives for free.
    #[test]
    fn inherited_text_reads_where_it_sits_on_the_page() {
        let defaults = DeckTextDefaults::default();
        let slide = tree(&text_shape(2, 0, 3_000_000, &["the slide's own line"]));
        let rung = tree(&text_shape(3, 0, 0, &["an inherited strapline"]));
        let media = PartMedia::default();
        let p = prepared(slide, &rung, &defaults, &media);
        let order: Vec<Option<usize>> = slide_reading_order(&p).iter().map(|r| r.rung).collect();
        assert_eq!(order, [Some(1), None]);
    }

    /// Same band, same x. The slide's own text is inserted first and the sort
    /// is stable, so it reads first — a template's caption should not cut in
    /// front of the line it was authored behind.
    #[test]
    fn the_slide_wins_a_tie_with_its_template() {
        let defaults = DeckTextDefaults::default();
        let slide = tree(&text_shape(2, 0, 0, &["the slide's own line"]));
        let rung = tree(&text_shape(3, 0, 0, &["an inherited line"]));
        let media = PartMedia::default();
        let p = prepared(slide, &rung, &defaults, &media);
        let order: Vec<Option<usize>> = slide_reading_order(&p).iter().map(|r| r.rung).collect();
        assert_eq!(order, [None, Some(1)]);
    }

    /// Title-first survives the merge. An inherited banner across the top of
    /// every slide sits above the title geometrically, and sorting on position
    /// alone would put it in front of the `#` on every page that keeps one.
    #[test]
    fn a_title_still_reads_first_under_an_inherited_banner() {
        let defaults = DeckTextDefaults::default();
        let mut slide = tree(&text_shape(2, 0, 3_000_000, &["the title"]));
        slide[0].placeholder = Some(Placeholder {
            kind: PlaceholderKind::Title,
            idx: 0,
        });
        let rung = tree(&text_shape(3, 0, 0, &["an inherited banner"]));
        let media = PartMedia::default();
        let p = prepared(slide, &rung, &defaults, &media);
        let order: Vec<Option<usize>> = slide_reading_order(&p).iter().map(|r| r.rung).collect();
        assert_eq!(order, [None, Some(1)]);
    }

    // ── figures ─────────────────────────────────────────────────────────

    /// A `p:pic` at `(x, y)` EMU whose blip names `rel`.
    fn pic_shape(id: u32, x: i64, y: i64, rel: &str) -> String {
        format!(
            r#"<p:pic>
                 <p:nvPicPr><p:cNvPr id="{id}" name="p{id}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
                 <p:blipFill><a:blip r:embed="{rel}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
                 <p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                   <a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
               </p:pic>"#
        )
    }

    /// A `p:sp` whose `spPr` fill is a blip, with `text` as its body (empty
    /// slice = no `p:txBody` at all).
    fn blip_filled_sp(id: u32, rel: &str, text: &[&str]) -> String {
        let body = if text.is_empty() {
            String::new()
        } else {
            let paras: String = text
                .iter()
                .map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>"))
                .collect();
            format!("<p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>")
        };
        format!(
            r#"<p:sp>
                 <p:nvSpPr><p:cNvPr id="{id}" name="s{id}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                 <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                   <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                   <a:blipFill><a:blip r:embed="{rel}"/><a:stretch><a:fillRect/></a:stretch></a:blipFill>
                 </p:spPr>
                 {body}
               </p:sp>"#
        )
    }

    /// The census's rule, in the two directions it has to hold: a `p:pic` is a
    /// picture, and so is a blip-filled shape that says nothing.
    #[test]
    fn a_pic_and_a_textless_blip_filled_shape_are_both_pictures() {
        let shapes = tree(&format!(
            "{}{}",
            pic_shape(2, 0, 0, "rId1"),
            blip_filled_sp(3, "rId2", &[]),
        ));
        assert!(shapes.iter().all(|s| picture_fill(s).is_some()));
    }

    /// The other half of the same rule, and the half that keeps it honest: a
    /// blip behind text is a backdrop, and the text is the content. 27 of the
    /// corpus's 29 layout-level blip-filled shapes are exactly this.
    #[test]
    fn a_blip_behind_text_is_a_backdrop_not_a_figure() {
        let shapes = tree(&blip_filled_sp(2, "rId2", &["a heading over a photo"]));
        assert!(picture_fill(&shapes[0]).is_none());
        // ...and a body of only-empty runs is textless to a reader, so it is
        // a picture — the model's `Some(TextBody)` is not the question.
        let blank = tree(&blip_filled_sp(3, "rId2", &["", "   "]));
        assert!(picture_fill(&blank[0]).is_some());
    }

    /// A shape with no blip anywhere is not a picture, whatever else it is.
    #[test]
    fn an_ordinary_shape_is_not_a_picture() {
        let shapes = tree(&text_shape(2, 0, 0, &["just words"]));
        assert!(picture_fill(&shapes[0]).is_none());
    }

    /// A rung's pictures reach the reading order, and they arrive flagged as
    /// figures — `inherited_text_laid_out` keys on that flag, so a picture
    /// counted as inherited *text* would corrupt the metric that step's A/B
    /// rests on.
    #[test]
    fn inherited_pictures_read_as_figures() {
        let defaults = DeckTextDefaults::default();
        let slide = tree(&text_shape(2, 0, 3_000_000, &["the slide's own line"]));
        let rung = tree(&pic_shape(3, 0, 0, "rId1"));
        let media = PartMedia::default();
        let mut p = prepared(slide, &[], &defaults, &media);
        p.inherited_figures = [Vec::new(), rung.iter().collect()];
        let order: Vec<(Option<usize>, bool)> = slide_reading_order(&p)
            .iter()
            .map(|r| (r.rung, r.figure))
            .collect();
        assert_eq!(order, [(Some(1), true), (None, false)]);
    }
}
