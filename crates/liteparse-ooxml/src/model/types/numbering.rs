//! Numbering definitions — abstract numbering, instances, picture bullets.

use std::collections::HashMap;

use super::formatting::{Alignment, NumberFormat};
use super::identifiers::{AbstractNumId, NumId, NumPicBulletId};
use super::paragraph::Indentation;
use super::run_properties::RunProperties;
use super::vml::Pict;

/// Raw numbering definitions as parsed from `word/numbering.xml`.
#[derive(Clone, Debug, Default)]
pub struct NumberingDefinitions {
    /// Abstract numbering definitions keyed by abstract numbering ID.
    pub abstract_nums: HashMap<AbstractNumId, AbstractNumbering>,
    /// Numbering instances keyed by numbering ID.
    pub numbering_instances: HashMap<NumId, NumberingInstance>,
    /// §17.9.21: picture bullet definitions keyed by numPicBulletId.
    pub pic_bullets: HashMap<NumPicBulletId, NumPicBullet>,
}

/// §17.9.21: a picture bullet definition.
#[derive(Clone, Debug)]
pub struct NumPicBullet {
    /// §17.9.21 @numPicBulletId: unique identifier.
    pub id: NumPicBulletId,
    /// §17.3.3.19: VML picture content.
    pub pict: Option<Pict>,
}

/// An abstract numbering definition.
#[derive(Clone, Debug)]
pub struct AbstractNumbering {
    pub levels: Vec<NumberingLevelDefinition>,
}

/// §17.18.53 ST_LevelSuffix — the character between a list label and the
/// paragraph text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LevelSuffix {
    /// A tab (the spec default).
    #[default]
    Tab,
    /// A single space.
    Space,
    /// Nothing — the text follows the label directly.
    Nothing,
}

/// A single level within an abstract numbering definition.
#[derive(Clone, Debug)]
pub struct NumberingLevelDefinition {
    pub level: u8,
    pub format: Option<NumberFormat>,
    pub level_text: String,
    pub start: Option<u32>,
    /// §17.9.7: justification of the numbering symbol (uses ST_Jc).
    pub justification: Option<Alignment>,
    pub indentation: Option<Indentation>,
    pub run_properties: Option<RunProperties>,
    /// §17.9.10: reference to a picture bullet definition.
    pub lvl_pic_bullet_id: Option<NumPicBulletId>,
    /// §17.9.29: separator between the number and the paragraph text.
    pub suffix: LevelSuffix,
    /// §17.9.8: render all level numbers as decimal (legal numbering).
    pub is_legal: bool,
}

/// §17.9.9: a per-instance override of one abstract numbering level.
#[derive(Clone, Debug)]
pub struct LevelOverride {
    /// §17.9.9 @ilvl: the level this override applies to.
    pub level: u8,
    /// §17.9.28 startOverride: restart this level's counter at the given value.
    pub start_override: Option<u32>,
    /// §17.9.9: the replacement level definition, if the override supplies one.
    pub definition: Option<NumberingLevelDefinition>,
}

/// A numbering instance — maps to an abstract numbering, with optional level overrides.
#[derive(Clone, Debug)]
pub struct NumberingInstance {
    pub abstract_num_id: AbstractNumId,
    pub level_overrides: Vec<LevelOverride>,
}
