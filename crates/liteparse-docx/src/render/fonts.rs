//! Font resolution with per-render ownership (`FontRegistry`) — fontdb + skrifa.
//!
//! Upstream this module is a 1,300-line registry built on Skia's `FontMgr`.
//! This is the C′ rewrite: same public surface as far as the layout engine
//! consumes it, with resolution answered by `fontdb` and metrics/advances by
//! `skrifa` (see `layout::measurer`). It is the one production seam between
//! dxpdf's layout engine and Skia — everything else in `render/layout/` is
//! vendored verbatim.
//!
//! The resolution rules and their ordering are not invented here: they were
//! derived and measured in `bench/docx_layout_spike` (spikes 6–8, recorded in
//! `NATIVE_OFFICE_PLAN.md`). Where both engines have the requested face,
//! agreement with Skia is 99.8% on widths and bit-exact on line metrics; every
//! divergence beyond that was a *resolution* difference, which is why this
//! file is mostly resolution policy and hardly any arithmetic.
//!
//! ## Host-dependence (decision, 2026-08-13)
//!
//! Geometry from this registry is deliberately **host-dependent** for
//! documents whose fonts the host lacks: no fonts are bundled and no checksum
//! pinning is done. Spike 8 proved a family name does not pin a font (two
//! "Caladea"s differ by ~7% in advances), so determinism would require
//! shipping files — a cost the project declined. What this file guarantees
//! instead is *visibility*: every resolution records which [`ResolveRule`]
//! answered, and only `Embedded`, `Exact`, alias/suffix hits and
//! `Substitution(Metric)` claim to be reproducible across hosts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::model::{EmbeddedFont, EmbeddedFontVariant};

// ─── Public types ───────────────────────────────────────────────────────────

/// Weight + slant, the two axes OOXML runs actually vary. Stands in for
/// `skia_safe::FontStyle` in the vendored surface; deliberately minimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontStyle {
    /// CSS-scale weight (400 normal, 700 bold). `FontCache`-style call sites
    /// only ever produce those two, but the alias index can land in between.
    pub weight: u16,
    pub italic: bool,
}

impl FontStyle {
    pub fn normal() -> Self {
        Self {
            weight: 400,
            italic: false,
        }
    }
    pub fn bold() -> Self {
        Self {
            weight: 700,
            italic: false,
        }
    }
    pub fn italic() -> Self {
        Self {
            weight: 400,
            italic: true,
        }
    }
    pub fn bold_italic() -> Self {
        Self {
            weight: 700,
            italic: true,
        }
    }
    pub fn from_flags(bold: bool, italic: bool) -> Self {
        Self {
            weight: if bold { 700 } else { 400 },
            italic,
        }
    }
}

/// Stable id for an embedded font registered in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EmbeddedFontId(u32);

impl EmbeddedFontId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Identity for a resolved face — wraps the `fontdb` id. Used as a cache key
/// by the measurer's cluster-advance memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypefaceId(pub fontdb::ID);

/// Single source of truth for "where did this typeface come from?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypefaceOrigin {
    /// Resolved from a font embedded in the DOCX (`word/fonts/*.odttf`).
    Embedded { id: EmbeddedFontId },
    /// Resolved from the host font database — exact match, alias,
    /// substitution, or fallback. See [`ResolveRule`] for which.
    System { typeface_id: TypefaceId },
}

/// A resolved face handle. Carried by value in fragments and draw commands,
/// so it stays small: the bytes live in the registry's `fontdb::Database` and
/// are borrowed at measure time.
#[derive(Clone, Debug)]
pub struct TypefaceEntry {
    pub id: fontdb::ID,
    pub origin: TypefaceOrigin,
}

/// Cache key for resolved typefaces — case-insensitive family + weight + slant.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct TypefaceKey {
    pub family_lc: String,
    pub weight: u16,
    pub italic: bool,
}

impl TypefaceKey {
    pub fn new(family: &str, style: FontStyle) -> Self {
        Self {
            family_lc: family.to_lowercase(),
            weight: style.weight,
            italic: style.italic,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RegisterError {
    #[error("invalid embedded font data for '{family}' ({variant:?})")]
    InvalidFontData {
        family: String,
        variant: EmbeddedFontVariant,
    },
}

// ─── Resolution rules (spike 6/8 artifacts) ─────────────────────────────────

/// Whether a substitute reproduces the original's *metrics* or merely its look.
///
/// This distinction is the whole point of the table, not a nicety. Spike 7
/// measured the cost of getting it wrong: Helvetica standing in for Arial is
/// metric-compatible in **advances** (so a width test sees nothing) and off by
/// **+14.99% in line metrics** (1.0000 em vs 1.1499 em) — a one-directional
/// bias that compounds down every page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubstKind {
    /// Designed as a drop-in: same upem, same advances, same line metrics.
    /// Geometry is reproducible across hosts wherever the substitute exists.
    Metric,
    /// Similar design, different metrics. Better than a generic sans, but it
    /// does **not** buy cross-host determinism, and the report says so.
    Visual,
}

/// Which rule produced a resolution. Recorded per request so a consumer can
/// say *why* geometry might differ on another host, not just that it might.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolveRule {
    /// The DOCX embedded the font; the bytes travel with the document.
    Embedded,
    /// fontdb matched the requested family outright.
    Exact,
    /// The face-alias index answered — a PostScript name
    /// (`AvenirNext-Regular`) or a weight-qualified face name
    /// (`Calibri Light`) resolved to its family.
    Alias,
    /// A trailing style word was stripped from the family name
    /// (`Times New Roman Bold` → `Times New Roman`).
    StyleSuffix,
    /// The substitution table fired.
    Substitution(SubstKind),
    /// Nothing matched; the host's generic sans-serif answered. **This is the
    /// non-deterministic case** — the answer is a property of the machine.
    Generic,
    /// Even the generic missed — see `LAST_RESORT`. Also host-specific, but
    /// distinguished from `Generic` because reaching it means the host's
    /// `sans-serif` alias points at a font that is not installed, which is a
    /// fact about the *image* worth surfacing on its own.
    LastResort,
    /// No face at all (an empty font database).
    None,
}

impl ResolveRule {
    pub fn name(self) -> &'static str {
        match self {
            ResolveRule::Embedded => "embedded",
            ResolveRule::Exact => "exact",
            ResolveRule::Alias => "alias",
            ResolveRule::StyleSuffix => "style_suffix",
            ResolveRule::Substitution(SubstKind::Metric) => "substitution_metric",
            ResolveRule::Substitution(SubstKind::Visual) => "substitution_visual",
            ResolveRule::Generic => "generic",
            ResolveRule::LastResort => "last_resort",
            ResolveRule::None => "none",
        }
    }

    /// Is the geometry this produced expected to be identical on another host
    /// (that has the same document and the named fonts)? Only the rules that
    /// pin an identity qualify; a visual substitute and a generic fallback
    /// both give geometry that is a property of the machine.
    pub fn deterministic(self) -> bool {
        matches!(
            self,
            ResolveRule::Embedded
                | ResolveRule::Exact
                | ResolveRule::Alias
                | ResolveRule::StyleSuffix
                | ResolveRule::Substitution(SubstKind::Metric)
        )
    }
}

/// Trailing style words stripped by [`strip_style_suffix`]. Family names carry
/// a baked style suffix often enough to matter ("Times New Roman Bold" as a
/// *family*). Skia's `FontMgr` absorbs these; `fontdb`'s family match does
/// not, and would silently substitute instead.
///
/// Deliberately excludes `roman` and `book`: "Times New Roman" and "Century
/// Schoolbook" are families whose last word *is* a style word, and stripping
/// it sends the query somewhere that does not exist. This cost spike 6 a run —
/// the fix looked inert because it had quietly broken its own best case.
/// `black` and `light` stay in only because stripping is attempted after an
/// exact family match has already failed, so "Arial Black" is never reached by
/// this path on a host that has it.
const STYLE_SUFFIXES: &[&str] = &[
    "bold", "italic", "oblique", "regular", "light", "medium", "semibold", "demibold", "black",
    "heavy", "thin",
];

/// Strip trailing style words, e.g. `Times New Roman Bold` → `Times New Roman`.
/// Never strips the whole name (`Roman` alone stays `Roman`) — a family that
/// *is* a style word is a real family, not a suffix.
fn strip_style_suffix(family: &str) -> Option<String> {
    let mut words: Vec<&str> = family.split_whitespace().collect();
    let mut stripped = false;
    while words.len() > 1
        && STYLE_SUFFIXES.contains(&words[words.len() - 1].to_lowercase().as_str())
    {
        words.pop();
        stripped = true;
    }
    stripped.then(|| words.join(" "))
}

/// Requested family → ordered substitute candidates.
///
/// Consulted only after embedded / exact / alias / style-suffix have all
/// failed — so a host that really has the font is never diverted here.
/// Ordering within a row is deliberate: metric clones first, visual
/// approximations last, so a host with the real clone installed never falls
/// through to an approximation.
///
/// Deliberately **not** listed: Verdana, Tahoma, Consolas, Segoe UI (beyond
/// Selawik), Wingdings, Bahnschrift, and the CJK families. No metric-
/// compatible free clone exists, and inventing a `Visual` row for them would
/// convert a visible "this host fell back to a generic" into an invisible
/// wrong answer. Absent families must stay loud.
///
/// The URW / TeX Gyre rows are all `Visual`, and that was **measured, not
/// assumed** (spike 8): the whole set is advance-compatible with the
/// PostScript base 35 and line-metric-incompatible (every face reports
/// `lh = 1.200 em` against 1.000 em for the originals). Same trap as
/// Helvetica-for-Arial, one layer down.
///
/// Trap encoded in the table itself: `Helvetica` and `Arial` must not point at
/// the same clones the same way round. Liberation Sans clones **Arial** (so it
/// is `Metric` there) and merely resembles Helvetica (so it is `Visual` here).
/// Getting these rows interchanged is the easiest mistake in this table and
/// would be invisible on a host missing both.
const SUBSTITUTIONS: &[(&str, &[(&str, SubstKind)])] = &[
    // Microsoft ClearType set → Google's Chrome OS metric clones.
    ("Calibri", &[("Carlito", SubstKind::Metric)]),
    // Carlito has no Light cut; the regular is metric-compatible with Calibri
    // proper, not with Calibri Light, so this is honestly a Visual match.
    ("Calibri Light", &[("Carlito", SubstKind::Visual)]),
    ("Cambria", &[("Caladea", SubstKind::Metric)]),
    // Microsoft's own metric-compatible stand-in for Segoe UI.
    ("Segoe UI", &[("Selawik", SubstKind::Metric)]),
    (
        "Arial",
        &[
            ("Liberation Sans", SubstKind::Metric),
            ("Arimo", SubstKind::Metric),
            ("Helvetica", SubstKind::Visual),
        ],
    ),
    (
        "Times New Roman",
        &[
            ("Liberation Serif", SubstKind::Metric),
            ("Tinos", SubstKind::Metric),
        ],
    ),
    (
        "Courier New",
        &[
            ("Liberation Mono", SubstKind::Metric),
            ("Cousine", SubstKind::Metric),
        ],
    ),
    (
        "Georgia",
        &[("Gelasio", SubstKind::Metric), ("Tinos", SubstKind::Visual)],
    ),
    // The PostScript base 35 and their URW / TeX Gyre clones. "Times" and
    // "Helvetica" are the PostScript originals, *not* the Microsoft ones.
    (
        "Helvetica",
        &[
            ("Nimbus Sans", SubstKind::Visual),
            ("TeX Gyre Heros", SubstKind::Visual),
            ("Arial", SubstKind::Visual),
            ("Liberation Sans", SubstKind::Visual),
        ],
    ),
    (
        "Times",
        &[
            ("Nimbus Roman", SubstKind::Visual),
            ("TeX Gyre Termes", SubstKind::Visual),
            ("Liberation Serif", SubstKind::Visual),
        ],
    ),
    (
        "Courier",
        &[
            ("Nimbus Mono PS", SubstKind::Visual),
            ("TeX Gyre Cursor", SubstKind::Visual),
            ("Liberation Mono", SubstKind::Visual),
        ],
    ),
    (
        "Palatino",
        &[
            ("P052", SubstKind::Visual),
            ("TeX Gyre Pagella", SubstKind::Visual),
        ],
    ),
    (
        "Palatino Linotype",
        &[
            ("P052", SubstKind::Visual),
            ("TeX Gyre Pagella", SubstKind::Visual),
        ],
    ),
    (
        "Book Antiqua",
        &[
            ("P052", SubstKind::Visual),
            ("TeX Gyre Pagella", SubstKind::Visual),
        ],
    ),
    (
        "Century Schoolbook",
        &[
            ("C059", SubstKind::Visual),
            ("TeX Gyre Schola", SubstKind::Visual),
        ],
    ),
    (
        "New Century Schoolbook",
        &[
            ("C059", SubstKind::Visual),
            ("TeX Gyre Schola", SubstKind::Visual),
        ],
    ),
    (
        "Bookman Old Style",
        &[
            ("URW Bookman", SubstKind::Visual),
            ("TeX Gyre Bonum", SubstKind::Visual),
        ],
    ),
    // Century Gothic imitates ITC Avant Garde Gothic; URW Gothic clones Avant
    // Garde, so it is a clone of the *ancestor*, not of Century Gothic.
    (
        "Century Gothic",
        &[
            ("URW Gothic", SubstKind::Visual),
            ("TeX Gyre Adventor", SubstKind::Visual),
        ],
    ),
    (
        "AvantGarde",
        &[
            ("URW Gothic", SubstKind::Visual),
            ("TeX Gyre Adventor", SubstKind::Visual),
        ],
    ),
    ("ZapfChancery", &[("Z003", SubstKind::Visual)]),
    ("Monotype Corsiva", &[("Z003", SubstKind::Visual)]),
    ("Symbol", &[("Standard Symbols PS", SubstKind::Visual)]),
    ("ZapfDingbats", &[("D050000L", SubstKind::Visual)]),
];

fn substitutes_for(family: &str) -> &'static [(&'static str, SubstKind)] {
    let lookup = |name: &str| {
        SUBSTITUTIONS
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, subs)| *subs)
    };
    // A face-qualified name ("Calibri Light Italic") should still reach its
    // family's row when the suffix-stripped query itself found nothing.
    lookup(family)
        .or_else(|| lookup(&strip_style_suffix(family)?))
        .unwrap_or(&[])
}

/// Last-resort families, tried in order before taking any face at all.
///
/// `fontdb::Family::SansSerif` is **not** a reliable floor. `load_system_fonts`
/// parses fontconfig and *overwrites* fontdb's built-in default with whatever
/// the host's `sans-serif` alias names first — so in a container without
/// DejaVu installed, the generic query returns `None`. Spike 8's first Linux
/// run hit exactly this: 7 requests resolved to nothing and silently left the
/// statistics. Measuring with a wrong font is a visible, attributable error;
/// measuring nothing is silent.
const LAST_RESORT: &[&str] = &[
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "Noto Sans",
    "FreeSans",
];

// ─── Face-alias index ───────────────────────────────────────────────────────

/// Names beyond the family name under which a host face is reachable:
/// PostScript names (`AvenirNext-Regular` — Word writes these into `w:rFonts`
/// often enough to matter) and weight-qualified face names (`Calibri Light`).
/// Skia's `FontMgr` matches both; `fontdb::Query` matches family names only,
/// so without this index those requests would silently substitute.
///
/// Mirrors upstream's `FaceAliasIndex`, including the ambiguity rule: a name
/// claimed by two *different* (family, weight) targets resolves to nothing
/// rather than to whichever face enumerated first.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FaceAlias {
    family: String,
    weight: u16,
}

#[derive(Clone, Debug)]
enum AliasEntry {
    Unique(FaceAlias),
    Ambiguous,
}

#[derive(Default)]
struct FaceAliasIndex {
    aliases: HashMap<String, AliasEntry>,
}

impl FaceAliasIndex {
    fn build(db: &fontdb::Database) -> Self {
        let mut index = Self::default();
        for face in db.faces() {
            // Weight-bearing aliases only, as upstream: width and slant
            // aliases would need a fuller OpenType identity model.
            if face.style != fontdb::Style::Normal || face.stretch != fontdb::Stretch::Normal {
                // PostScript names are still unique per face; index them even
                // for italic faces so `SomeFont-BoldItalic` resolves. The
                // weight carried is the face's own; the caller re-queries by
                // family so slant comes from the request.
                index.insert_ps_only(face);
                continue;
            }
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            let alias = FaceAlias {
                family: family.clone(),
                weight: face.weight.0,
            };
            if !face.post_script_name.trim().is_empty() {
                index.insert_alias(&face.post_script_name, alias.clone());
            }
            for weight_name in canonical_weight_names(face.weight.0) {
                index.insert_alias(&format!("{family} {weight_name}"), alias.clone());
            }
        }
        index
    }

    fn insert_ps_only(&mut self, face: &fontdb::FaceInfo) {
        let Some((family, _)) = face.families.first() else {
            return;
        };
        if face.post_script_name.trim().is_empty() {
            return;
        }
        let alias = FaceAlias {
            family: family.clone(),
            weight: face.weight.0,
        };
        self.insert_alias(&face.post_script_name, alias);
    }

    fn insert_alias(&mut self, name: &str, alias: FaceAlias) {
        use std::collections::hash_map::Entry;
        match self.aliases.entry(face_name_key(name)) {
            Entry::Vacant(entry) => {
                entry.insert(AliasEntry::Unique(alias));
            }
            Entry::Occupied(mut entry) => match entry.get() {
                AliasEntry::Unique(existing) if existing == &alias => {}
                AliasEntry::Unique(_) => {
                    entry.insert(AliasEntry::Ambiguous);
                }
                AliasEntry::Ambiguous => {}
            },
        }
    }

    fn resolve(&self, name: &str) -> Option<&FaceAlias> {
        match self.aliases.get(&face_name_key(name))? {
            AliasEntry::Unique(alias) => Some(alias),
            AliasEntry::Ambiguous => None,
        }
    }
}

fn face_name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn canonical_weight_names(weight: u16) -> &'static [&'static str] {
    match weight {
        100 => &["Thin", "Hairline"],
        200 => &["ExtraLight", "Extra Light", "UltraLight", "Ultra Light"],
        300 => &["Light"],
        400 => &["Regular", "Normal"],
        500 => &["Medium"],
        600 => &["Semibold", "SemiBold", "Semi Bold", "DemiBold", "Demi Bold"],
        700 => &["Bold"],
        800 => &["ExtraBold", "Extra Bold", "UltraBold", "Ultra Bold"],
        900 => &["Black", "Heavy"],
        _ => &[],
    }
}

/// Combine the weight a face *name* carries with the weight the run requested.
///
/// The requested weight is not really a weight — it is built from a
/// `bold: bool`, so `NORMAL` carries no information and must not participate:
/// taking `max` unconditionally would promote `"Calibri Light"` (342) back to
/// plain Calibri at 400, discarding exactly the information the alias index
/// exists to honour. A bold request still raises a light face.
fn merged_alias_weight(alias_weight: u16, requested_weight: u16) -> u16 {
    if requested_weight > 400 {
        alias_weight.max(requested_weight)
    } else {
        alias_weight
    }
}

// ─── FontRegistry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct EmbeddedRecord {
    family: String,
    variant: EmbeddedFontVariant,
    face: Option<fontdb::ID>,
}

pub struct FontRegistry {
    db: fontdb::Database,
    embedded: Vec<EmbeddedRecord>,
    embedded_index: HashMap<(String, EmbeddedFontVariant), EmbeddedFontId>,
    aliases: FaceAliasIndex,
    typefaces: RefCell<HashMap<TypefaceKey, TypefaceEntry>>,
    /// Which rule answered, per cache key. Kept beside the memo rather than
    /// derived later: by the time you hold a `fontdb::ID` the reason is gone.
    rules: RefCell<HashMap<TypefaceKey, ResolveRule>>,
}

impl FontRegistry {
    /// Registry over the host's fonts, without any embedded fonts.
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let aliases = FaceAliasIndex::build(&db);
        Self {
            db,
            embedded: Vec::new(),
            embedded_index: HashMap::new(),
            aliases,
            typefaces: RefCell::new(HashMap::new()),
            rules: RefCell::new(HashMap::new()),
        }
    }

    /// Build a registry, registering all embedded fonts and preloading the
    /// requested family/style combinations.
    ///
    /// Fails with [`crate::render::error::RenderError::NoFontsAvailable`] when
    /// no face exists at all. Checking here rather than at the point of use is
    /// what lets [`Self::resolve`] return a `TypefaceEntry` rather than an
    /// `Option`: the take-any-face arm of `resolve_uncached` is unreachable
    /// for any registry this constructor returns.
    pub fn build(
        embedded: &[EmbeddedFont],
        families: &[String],
    ) -> Result<Self, crate::render::error::RenderError> {
        let mut reg = Self::new();
        for ef in embedded {
            if let Err(err) = reg.register_embedded(&ef.family, ef.variant, ef.data.clone()) {
                log::warn!("{err}");
            }
        }
        if reg.db.faces().next().is_none() {
            return Err(crate::render::error::RenderError::NoFontsAvailable);
        }
        // Alias index rebuilt after embedded registration so embedded
        // PostScript names resolve too.
        reg.aliases = FaceAliasIndex::build(&reg.db);
        reg.preload(families);
        Ok(reg)
    }

    pub fn db(&self) -> &fontdb::Database {
        &self.db
    }

    pub fn embedded_font_count(&self) -> usize {
        self.embedded.len()
    }

    pub fn cached_typeface_count(&self) -> usize {
        self.typefaces.borrow().len()
    }

    /// Register an embedded font. Subsequent `resolve` calls for the same
    /// family + variant return this face in preference to system resolution —
    /// dxpdf registers embedded fonts ahead of system resolution, and spike 6
    /// found the one embedded-font document in the corpus (Georgia) agreeing
    /// only by luck without this ordering.
    pub fn register_embedded(
        &mut self,
        family: &str,
        variant: EmbeddedFontVariant,
        bytes: Vec<u8>,
    ) -> Result<EmbeddedFontId, RegisterError> {
        let ids = self
            .db
            .load_font_source(fontdb::Source::Binary(Arc::new(bytes)));
        let face = ids.first().copied();
        if face.is_none() {
            return Err(RegisterError::InvalidFontData {
                family: family.to_string(),
                variant,
            });
        }
        let id = EmbeddedFontId(self.embedded.len() as u32);
        self.embedded.push(EmbeddedRecord {
            family: family.to_string(),
            variant,
            face,
        });
        self.embedded_index
            .insert((family.to_lowercase(), variant), id);
        log::debug!("registered embedded font '{}' {:?}", family, variant);
        Ok(id)
    }

    /// Family + variant for a registered embedded font.
    pub fn embedded_meta(&self, id: EmbeddedFontId) -> (&str, EmbeddedFontVariant) {
        let r = &self.embedded[id.0 as usize];
        (&r.family, r.variant)
    }

    /// Resolve a typeface by family + style. Embedded fonts win over system.
    /// Cached after the first resolution; later calls are O(1).
    pub fn resolve(&self, family: &str, style: FontStyle) -> TypefaceEntry {
        let key = TypefaceKey::new(family, style);
        if let Some(entry) = self.typefaces.borrow().get(&key) {
            return entry.clone();
        }
        let (entry, rule) = self.resolve_uncached(family, style);
        self.typefaces
            .borrow_mut()
            .insert(key.clone(), entry.clone());
        self.rules.borrow_mut().insert(key, rule);
        entry
    }

    /// The rule that answered for this request, resolving first if needed.
    /// This is the host-dependence signal: anything other than a
    /// [`ResolveRule::deterministic`] rule means the resulting geometry is a
    /// property of this machine.
    pub fn resolve_rule(&self, family: &str, style: FontStyle) -> ResolveRule {
        let key = TypefaceKey::new(family, style);
        if !self.rules.borrow().contains_key(&key) {
            self.resolve(family, style);
        }
        self.rules
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(ResolveRule::None)
    }

    fn query(&self, families: &[fontdb::Family], style: FontStyle) -> Option<fontdb::ID> {
        self.db.query(&fontdb::Query {
            families,
            weight: fontdb::Weight(style.weight),
            stretch: fontdb::Stretch::Normal,
            style: if style.italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
        })
    }

    fn resolve_uncached(&self, family: &str, style: FontStyle) -> (TypefaceEntry, ResolveRule) {
        // 1. Embedded, by family + variant.
        let variant = variant_for_style(style);
        if let Some(&eid) = self.embedded_index.get(&(family.to_lowercase(), variant)) {
            if let Some(face) = self.embedded[eid.0 as usize].face {
                log::debug!("[font] '{}' {:?} → embedded #{}", family, style, eid.0);
                return (
                    TypefaceEntry {
                        id: face,
                        origin: TypefaceOrigin::Embedded { id: eid },
                    },
                    ResolveRule::Embedded,
                );
            }
        }

        // 2. Exact family match. fontdb declines a family it does not have
        // (unlike fontconfig-backed Skia, which substitutes), so a hit here is
        // genuinely the requested family — no exactness guard needed.
        if let Some(id) = self.query(&[fontdb::Family::Name(family)], style) {
            return (system_entry(id), ResolveRule::Exact);
        }

        // 3. Face alias: PostScript name or weight-qualified face name.
        // Resolve to a *family*, then re-query with the merged weight and the
        // requested slant — a PostScript name names one specific face, so
        // taking it directly would pin `-Regular` even when the run asked for
        // bold.
        if let Some(alias) = self.aliases.resolve(family) {
            let merged = FontStyle {
                weight: merged_alias_weight(alias.weight, style.weight),
                italic: style.italic,
            };
            if let Some(id) = self.query(&[fontdb::Family::Name(&alias.family)], merged) {
                log::debug!(
                    "[font] '{}' {:?} → face alias '{}' {:?}",
                    family,
                    style,
                    alias.family,
                    merged
                );
                return (system_entry(id), ResolveRule::Alias);
            }
        }

        // 4. Style suffix baked into the family name. The requested
        // bold/italic still applies, so "Times New Roman Bold" lands on the
        // real TNR bold face.
        if let Some(base) = strip_style_suffix(family) {
            if let Some(id) = self.query(&[fontdb::Family::Name(&base)], style) {
                log::debug!(
                    "[font] '{}' {:?} → suffix-stripped '{}'",
                    family,
                    style,
                    base
                );
                return (system_entry(id), ResolveRule::StyleSuffix);
            }
        }

        // 5. Substitution table. Candidates in table order, so a host carrying
        // the real clone never falls through to a visual approximation.
        for (candidate, kind) in substitutes_for(family) {
            if let Some(id) = self.query(&[fontdb::Family::Name(candidate)], style) {
                log::debug!(
                    "[font] '{}' {:?} → substitute '{}' ({:?})",
                    family,
                    style,
                    candidate,
                    kind
                );
                return (system_entry(id), ResolveRule::Substitution(*kind));
            }
        }

        // 6. Whatever this host calls sans-serif.
        if let Some(id) = self.query(&[fontdb::Family::SansSerif], style) {
            log::debug!("[font] '{}' {:?} → generic sans-serif", family, style);
            return (system_entry(id), ResolveRule::Generic);
        }

        // 7. Family::SansSerif can itself resolve to nothing (see
        // LAST_RESORT). Walk an explicit list, then take *any* face in the
        // database — unreachable from `build`, which rejects an empty
        // database up front.
        for name in LAST_RESORT {
            if let Some(id) = self.query(&[fontdb::Family::Name(name)], style) {
                return (system_entry(id), ResolveRule::LastResort);
            }
        }
        // Unreachable for a registry from `FontRegistry::build`, which rejects
        // an empty font database up front so this arm always has something to
        // return — the same contract upstream documents for its Skia
        // `legacy_make_typeface` arm.
        let f = self
            .db
            .faces()
            .next()
            .expect("FontRegistry::build guarantees a last-resort face");
        (system_entry(f.id), ResolveRule::LastResort)
    }

    /// Resolve by exact family + style match only — embedded first, then the
    /// host — or `None`. No substitutes, no default fallback: necessary for
    /// the emoji pipeline, where substituting a non-emoji typeface for a
    /// missing color emoji font is never correct.
    pub fn resolve_exact(&self, family: &str, style: FontStyle) -> Option<TypefaceEntry> {
        let variant = variant_for_style(style);
        if let Some(&eid) = self.embedded_index.get(&(family.to_lowercase(), variant)) {
            if let Some(face) = self.embedded[eid.0 as usize].face {
                return Some(TypefaceEntry {
                    id: face,
                    origin: TypefaceOrigin::Embedded { id: eid },
                });
            }
        }
        self.query(&[fontdb::Family::Name(family)], style)
            .map(system_entry)
    }

    /// Resolve from the host font system only — bypasses embedded fonts.
    /// Used by the color emoji pipeline: Word's font subsetter strips color
    /// glyph tables (sbix/CBDT/COLR/SVG) when embedding emoji fonts, so a
    /// docx-embedded "Segoe UI Emoji" carries the right family name but no
    /// color glyphs and must not satisfy emoji resolution.
    pub fn resolve_system_only(&self, family: &str, style: FontStyle) -> Option<TypefaceEntry> {
        let id = self.query(&[fontdb::Family::Name(family)], style)?;
        // An embedded face is a Binary source; reject it here.
        let info = self.db.face(id)?;
        if matches!(info.source, fontdb::Source::Binary(_)) {
            return None;
        }
        Some(system_entry(id))
    }

    /// Pre-resolve all four style variants for each family.
    pub fn preload(&self, families: &[String]) {
        let styles = [
            FontStyle::normal(),
            FontStyle::bold(),
            FontStyle::italic(),
            FontStyle::bold_italic(),
        ];
        for family in families {
            for &style in &styles {
                self.resolve(family, style);
            }
        }
    }

    /// Snapshot of all cached entries.
    pub fn cached_entries(&self) -> Vec<(TypefaceKey, TypefaceEntry)> {
        self.typefaces
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

impl Default for FontRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn variant_for_style(style: FontStyle) -> EmbeddedFontVariant {
    let bold = style.weight >= 600;
    match (bold, style.italic) {
        (true, true) => EmbeddedFontVariant::BoldItalic,
        (true, false) => EmbeddedFontVariant::Bold,
        (false, true) => EmbeddedFontVariant::Italic,
        (false, false) => EmbeddedFontVariant::Regular,
    }
}

fn system_entry(id: fontdb::ID) -> TypefaceEntry {
    TypefaceEntry {
        id,
        origin: TypefaceOrigin::System {
            typeface_id: TypefaceId(id),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_suffix_strips_trailing_style_words() {
        assert_eq!(
            strip_style_suffix("Times New Roman Bold").as_deref(),
            Some("Times New Roman")
        );
        assert_eq!(
            strip_style_suffix("Foo Bold Italic").as_deref(),
            Some("Foo")
        );
        // Families whose last word is a style word must survive.
        assert_eq!(strip_style_suffix("Times New Roman"), None);
        assert_eq!(strip_style_suffix("Century Schoolbook"), None);
        // Never strip down to nothing.
        assert_eq!(strip_style_suffix("Bold"), None);
    }

    #[test]
    fn substitution_lookup_reaches_family_through_face_name() {
        // "Calibri Light" has its own (Visual) row; "Calibri Light Italic"
        // does not, and must reach a row via suffix stripping.
        assert!(!substitutes_for("Calibri").is_empty());
        assert!(!substitutes_for("Calibri Light Italic").is_empty());
        assert!(substitutes_for("Comic Sans MS").is_empty());
    }

    #[test]
    fn merged_weight_ignores_content_free_normal() {
        // NORMAL carries no information — must not promote a Light face.
        assert_eq!(merged_alias_weight(300, 400), 300);
        // A bold request still raises a light face.
        assert_eq!(merged_alias_weight(300, 700), 700);
        assert_eq!(merged_alias_weight(900, 700), 900);
    }

    #[test]
    fn resolve_is_total_and_records_a_rule() {
        let reg = FontRegistry::new();
        if reg.db.faces().next().is_none() {
            return; // fontless CI host: nothing to assert against
        }
        let _ = reg.resolve("Definitely Not A Font 123", FontStyle::normal());
        let rule = reg.resolve_rule("Definitely Not A Font 123", FontStyle::normal());
        assert!(
            !rule.deterministic(),
            "an unknown family must be flagged host-dependent, got {rule:?}"
        );
        // And a real family (any first face) resolves Exact.
        let first_family = reg
            .db
            .faces()
            .next()
            .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
            .unwrap();
        let _ = reg.resolve(&first_family, FontStyle::normal());
        assert_eq!(
            reg.resolve_rule(&first_family, FontStyle::normal()).name(),
            "exact"
        );
    }
}
