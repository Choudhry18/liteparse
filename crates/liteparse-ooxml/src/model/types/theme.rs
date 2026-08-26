//! Theme types — color schemes, font schemes, and script tags.

use super::drawing::{DrawingFill, EffectList, Outline};

/// Resolved theme data from `theme1.xml`.
#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub color_scheme: ThemeColorScheme,
    pub major_font: ThemeFontScheme,
    pub minor_font: ThemeFontScheme,
    /// §20.1.4.1.13 fillStyleLst — theme fill styles referenced via
    /// `<a:fillRef idx="N">`. 0-based in storage — `fillRef idx="1"` is
    /// `fill_styles[0]`. `phClr` inside the fill is substituted by the ref's
    /// color at resolve time.
    pub fill_styles: Vec<DrawingFill>,
    /// §20.1.4.1.7 bgFillStyleLst — the *background* fill matrix, a list
    /// distinct from [`Self::fill_styles`] and reached only through a
    /// §19.3.1.3 `<p:bgRef>` whose `idx` is 1001 or greater (`idx - 1000`,
    /// then 0-based here). A shape's `<a:fillRef>` never reaches it, so a
    /// `bgRef` resolved against `fill_styles` instead would silently render
    /// with no background.
    pub bg_fill_styles: Vec<DrawingFill>,
    /// §20.1.4.1.21 lnStyleLst — theme line styles referenced via
    /// `<a:lnRef idx="N">`. 0-based in storage — `lnRef idx="1"` is
    /// `line_styles[0]`.
    pub line_styles: Vec<Outline>,
    /// §20.1.4.1.12 effectStyleLst — theme effect styles referenced via
    /// `<a:effectRef idx="N">`. Spec requires exactly 3, so the typical
    /// contents are `[subtle, moderate, intense]`. 0-based in storage —
    /// `effectRef idx="1"` is `effect_styles[0]`.
    pub effect_styles: Vec<EffectList>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThemeColorScheme {
    pub dark1: u32,
    pub light1: u32,
    pub dark2: u32,
    pub light2: u32,
    pub accent1: u32,
    pub accent2: u32,
    pub accent3: u32,
    pub accent4: u32,
    pub accent5: u32,
    pub accent6: u32,
    pub hyperlink: u32,
    pub followed_hyperlink: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ThemeFontScheme {
    pub latin: String,
    pub east_asian: String,
    pub complex_script: String,
    /// §20.1.4.1.16: per-script font overrides.
    pub script_fonts: Vec<ThemeScriptFont>,
}

/// §20.1.4.1.16: a per-script font mapping in a theme font scheme.
#[derive(Clone, Debug)]
pub struct ThemeScriptFont {
    /// ISO 15924 script code.
    pub script: ScriptTag,
    /// Typeface name for this script.
    pub typeface: String,
}

/// ISO 15924 script codes used in OOXML theme font schemes (§20.1.4.1.16).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScriptTag {
    Arab,
    Armn,
    Beng,
    Bopo,
    Bugi,
    Cans,
    Cher,
    Deva,
    Ethi,
    Geor,
    Gujr,
    Guru,
    Hang,
    Hans,
    Hant,
    Hebr,
    Java,
    Jpan,
    Khmr,
    Knda,
    Laoo,
    Lisu,
    Mlym,
    Mong,
    Mymr,
    Nkoo,
    Olck,
    Orya,
    Osma,
    Phag,
    Sinh,
    Sora,
    Syre,
    Syrj,
    Syrn,
    Syrc,
    Tale,
    Talu,
    Taml,
    Telu,
    Tfng,
    Thaa,
    Thai,
    Tibt,
    Uigh,
    Viet,
    Yiii,
    /// Unrecognized script code — preserved as-is.
    Other(Box<str>),
}
