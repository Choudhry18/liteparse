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

impl ThemeColorScheme {
    /// Resolve a theme color index to an RGB value.
    pub fn resolve(&self, idx: ThemeColorIndex) -> u32 {
        match idx {
            ThemeColorIndex::Dark1 => self.dark1,
            ThemeColorIndex::Light1 => self.light1,
            ThemeColorIndex::Dark2 => self.dark2,
            ThemeColorIndex::Light2 => self.light2,
            ThemeColorIndex::Accent1 => self.accent1,
            ThemeColorIndex::Accent2 => self.accent2,
            ThemeColorIndex::Accent3 => self.accent3,
            ThemeColorIndex::Accent4 => self.accent4,
            ThemeColorIndex::Accent5 => self.accent5,
            ThemeColorIndex::Accent6 => self.accent6,
            ThemeColorIndex::Hyperlink => self.hyperlink,
            ThemeColorIndex::FollowedHyperlink => self.followed_hyperlink,
        }
    }
}

/// Index into the theme color scheme (ST_ThemeColor).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeColorIndex {
    Dark1,
    Light1,
    Dark2,
    Light2,
    Accent1,
    Accent2,
    Accent3,
    Accent4,
    Accent5,
    Accent6,
    Hyperlink,
    FollowedHyperlink,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_maps_each_index_to_its_slot() {
        // Distinct value per slot catches an accidental field swap in `resolve`.
        let cs = ThemeColorScheme {
            dark1: 1,
            light1: 2,
            dark2: 3,
            light2: 4,
            accent1: 5,
            accent2: 6,
            accent3: 7,
            accent4: 8,
            accent5: 9,
            accent6: 10,
            hyperlink: 11,
            followed_hyperlink: 12,
        };
        assert_eq!(cs.resolve(ThemeColorIndex::Dark1), 1);
        assert_eq!(cs.resolve(ThemeColorIndex::Light1), 2);
        assert_eq!(cs.resolve(ThemeColorIndex::Dark2), 3);
        assert_eq!(cs.resolve(ThemeColorIndex::Light2), 4);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent1), 5);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent2), 6);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent3), 7);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent4), 8);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent5), 9);
        assert_eq!(cs.resolve(ThemeColorIndex::Accent6), 10);
        assert_eq!(cs.resolve(ThemeColorIndex::Hyperlink), 11);
        assert_eq!(cs.resolve(ThemeColorIndex::FollowedHyperlink), 12);
    }
}
