//! §17.3.2.20 `w:lang` reduced to the distinctions layout actually makes.

use crate::model::RunProperties;

/// The language a piece of the document is written in, as far as this engine's
/// layout is concerned.
///
/// Deliberately **not** a BCP-47 parser. The tag space is open and unbounded,
/// while the engine asks a language exactly two questions:
///
/// * §17.18.85 — which character a `decimal` tab aligns its zone on;
/// * §17.9.27 — whether `ordinal` / `cardinalText` / `ordinalText` can be
///   rendered as words.
///
/// So this names the groups those two questions have distinct answers for, and
/// stops. When a third question arrives, or when a language's number words are
/// implemented, the enum gains a variant and the compiler finds every site that
/// has to answer for it — which is the whole reason this is an enum and not a
/// `&str` compared afresh at each call site.
///
/// Only `w:lang/@w:val` is read. `@w:eastAsia` and `@w:bidi` name the languages
/// of *other script runs* in the same document, and neither of the two
/// questions above is asked of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Locale {
    /// English, in any region — the one language whose number words this engine
    /// spells. Also the answer for a document that declares no language at all,
    /// which is what every document was assumed to be before `w:lang` was read.
    #[default]
    English,
    /// A recognised language that writes a decimal **comma**: German, French,
    /// Spanish, Italian, Portuguese, Russian, Polish, Dutch, the Nordics, and
    /// most of Central and Eastern Europe.
    CommaDecimal,
    /// A recognised language that is not English and writes a decimal **point**:
    /// Japanese, Chinese, Korean, Hebrew, Thai, and most of South and
    /// South-East Asia.
    PointDecimal,
    /// A tag whose primary subtag is not in the table below.
    ///
    /// It answers every question exactly as [`Locale::English`] does, on
    /// purpose: before `w:lang` was read, *every* document got a decimal point
    /// and English number words, so an unfamiliar tag has to keep rendering as
    /// it does today rather than silently losing content to a degrade. It is a
    /// separate variant because it is a different *fact* — it logs, so an
    /// unhandled language is visible rather than silently assumed English.
    Unrecognised,
}

impl Locale {
    /// §17.18.85: the character a `decimal` tab stop aligns its zone on.
    pub fn decimal_separator(self) -> char {
        match self {
            Locale::CommaDecimal => ',',
            Locale::English | Locale::PointDecimal | Locale::Unrecognised => '.',
        }
    }

    /// §17.9.27: whether this engine can render a number as words in this
    /// language — which `ordinal`, `cardinalText` and `ordinalText` all need.
    ///
    /// A language that answers `false` gets digits. That is not a degrade for
    /// its own sake: writing `1st` onto a German list is not an approximation
    /// of German, it is English text in a German document, and the digits Word
    /// itself falls back to are closer than another language's words.
    pub fn spells_numbers(self) -> bool {
        match self {
            Locale::English | Locale::Unrecognised => true,
            Locale::CommaDecimal | Locale::PointDecimal => false,
        }
    }

    /// §17.7.2: classify the first `w:lang/@w:val` a cascade sets.
    ///
    /// Layers come highest-priority first, exactly as §17.7.2 resolves any
    /// other run property. A cascade that sets none anywhere is
    /// [`Locale::English`] and logs nothing — an absent tag is the common case
    /// for a minimal document, not an unhandled language.
    pub fn from_cascade<'a>(layers: impl IntoIterator<Item = &'a RunProperties>) -> Self {
        layers
            .into_iter()
            // The emptiness test belongs *inside* the search: an empty
            // `@w:val` sets nothing, so it must not stop the walk and shadow a
            // lower layer that does.
            .find_map(|rp| {
                rp.lang
                    .as_ref()
                    .and_then(|l| l.val.as_deref())
                    .filter(|tag| !tag.is_empty())
            })
            .map_or(Locale::English, Locale::from_tag)
    }

    /// Classify one §17.3.2.20 tag by its **primary subtag**.
    ///
    /// BCP-47's primary subtag is everything before the first `-`, and it is
    /// all either question depends on for the languages below: `de-DE` and
    /// `de-AT` write the same decimal comma, `en-US` and `en-GB` the same
    /// point. Matched case-insensitively, because the attribute is a tag and
    /// tags are case-insensitive even though Word writes them `ll-CC`.
    ///
    /// **Known simplification.** CLDR has regional overrides that this ignores
    /// — `de-CH` and `it-CH` write a point where `de` and `it` write a comma,
    /// `en-ZA` a comma where `en` writes a point, and Latin-American Spanish
    /// splits both ways. Modelling those means carrying a region table, which
    /// is the BCP-47 parsing this type exists to avoid; a half-filled table
    /// would look complete and not be. Likewise Arabic and Persian are listed
    /// as point-decimal, which is right for their Latin-digit documents and
    /// wrong for the Arabic-Indic `٫` some regions use.
    pub fn from_tag(tag: &str) -> Self {
        let primary = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
        match primary.as_str() {
            "en" => Locale::English,

            // Writes a decimal comma.
            "af" | "sq" | "hy" | "az" | "be" | "bs" | "bg" | "ca" | "hr" | "cs" | "da" | "nl"
            | "et" | "eu" | "fi" | "fo" | "fr" | "gl" | "ka" | "de" | "el" | "hu" | "is" | "id"
            | "it" | "kk" | "lb" | "lv" | "lt" | "mk" | "mn" | "nb" | "nn" | "no" | "pl" | "pt"
            | "ro" | "ru" | "sr" | "sk" | "sl" | "es" | "sv" | "tr" | "uk" | "vi" => {
                Locale::CommaDecimal
            }

            // Writes a decimal point, but is not English.
            "am" | "ar" | "bn" | "cy" | "fa" | "fil" | "ga" | "gu" | "he" | "hi" | "iw" | "ja"
            | "km" | "kn" | "ko" | "lo" | "ml" | "mr" | "ms" | "mt" | "my" | "ne" | "pa" | "si"
            | "sw" | "ta" | "te" | "th" | "tl" | "ur" | "zh" => Locale::PointDecimal,

            _ => {
                log::warn!(
                    "w:lang: unhandled language tag {tag:?} (§17.3.2.20) — \
                     assuming a decimal point and English number words, which \
                     is what every document got before locale was read"
                );
                Locale::Unrecognised
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lang;

    fn rp(tag: Option<&str>) -> RunProperties {
        RunProperties {
            lang: tag.map(|t| Lang {
                val: Some(t.to_string()),
                east_asia: None,
                bidi: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn english_regions_are_all_english() {
        for tag in ["en", "en-US", "en-GB", "en-AU", "EN-us"] {
            assert_eq!(Locale::from_tag(tag), Locale::English, "{tag}");
        }
    }

    #[test]
    fn the_corpus_languages_classify_as_they_should() {
        // Every tag `test-files/` and `test-cases/` actually declares.
        for tag in ["de-AT", "de-DE", "pl-PL", "it-IT", "ca-ES", "fr-FR"] {
            assert_eq!(Locale::from_tag(tag), Locale::CommaDecimal, "{tag}");
        }
        for tag in ["en-US", "en-GB"] {
            assert_eq!(Locale::from_tag(tag), Locale::English, "{tag}");
        }
    }

    #[test]
    fn point_decimal_languages_are_not_english() {
        for tag in ["ja-JP", "zh-CN", "ko-KR", "he-IL", "th-TH", "hi-IN"] {
            assert_eq!(Locale::from_tag(tag), Locale::PointDecimal, "{tag}");
        }
    }

    /// The primary subtag alone decides, so a region this table has never seen
    /// still classifies rather than falling through to `Unrecognised`.
    #[test]
    fn an_unknown_region_still_resolves_by_its_primary_subtag() {
        assert_eq!(Locale::from_tag("de-LI"), Locale::CommaDecimal);
        assert_eq!(Locale::from_tag("en-ZZ"), Locale::English);
    }

    #[test]
    fn an_unknown_primary_subtag_is_unrecognised() {
        for tag in ["zz-ZZ", "x-klingon", "qqq"] {
            assert_eq!(Locale::from_tag(tag), Locale::Unrecognised, "{tag}");
        }
    }

    /// The reason `Unrecognised` is its own variant rather than `English`: it
    /// must answer identically, so an unfamiliar document renders unchanged.
    #[test]
    fn an_unrecognised_tag_answers_exactly_as_english_does() {
        assert_eq!(
            Locale::Unrecognised.decimal_separator(),
            Locale::English.decimal_separator(),
        );
        assert_eq!(
            Locale::Unrecognised.spells_numbers(),
            Locale::English.spells_numbers(),
        );
    }

    #[test]
    fn only_comma_languages_write_a_comma() {
        assert_eq!(Locale::CommaDecimal.decimal_separator(), ',');
        assert_eq!(Locale::English.decimal_separator(), '.');
        assert_eq!(Locale::PointDecimal.decimal_separator(), '.');
    }

    #[test]
    fn only_english_spells_numbers() {
        assert!(Locale::English.spells_numbers());
        assert!(!Locale::CommaDecimal.spells_numbers());
        assert!(!Locale::PointDecimal.spells_numbers());
    }

    #[test]
    fn the_cascade_takes_the_first_layer_that_sets_a_tag() {
        let layers = [rp(None), rp(Some("de-DE")), rp(Some("en-US"))];
        assert_eq!(Locale::from_cascade(layers.iter()), Locale::CommaDecimal);
    }

    /// An empty `@w:val` sets nothing — it must not shadow a lower layer that
    /// does, and it must not be classified as an unknown language either.
    #[test]
    fn an_empty_tag_falls_through_to_the_next_layer() {
        let layers = [rp(Some("")), rp(Some("de-DE"))];
        assert_eq!(Locale::from_cascade(layers.iter()), Locale::CommaDecimal);
    }

    /// A document that declares no language anywhere is English and silent —
    /// the overwhelmingly common minimal document, not an unhandled language.
    #[test]
    fn a_cascade_with_no_tag_at_all_is_english() {
        assert_eq!(Locale::from_cascade([rp(None)].iter()), Locale::English);
        assert_eq!(Locale::from_cascade(std::iter::empty()), Locale::English);
    }
}
