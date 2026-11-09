//! The skills of a running server: built-in ones from a directory, and those of
//! a tenant or a person, stored with the settings.

use crate::layered::{LayeredFile, Layers};
use lighthouse_audit::acting_user;
use lighthouse_settings::{FileKind, Layer, TenantSettings};
use lighthouse_skills::{Skill, SkillLibrary};
use std::collections::BTreeMap;

/// A skill with where it comes from.
#[derive(Clone, Debug)]
pub struct LoadedSkill {
    /// The skill.
    pub skill: Skill,
    /// The file, for a skill created in the apps.
    pub source: Option<String>,
    /// The layer it comes from.
    pub layer: Layer,
    /// Whether it is in use for this user.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
}

impl LayeredFile for LoadedSkill {
    fn read(source: &str, layer: Layer) -> Option<Self> {
        let skill = lighthouse_skills::parse(source, "stored skill").ok()?;
        Some(Self {
            skill,
            source: Some(source.to_owned()),
            layer,
            enabled: true,
            disabled_by: None,
        })
    }

    fn set_state(&mut self, enabled: bool, disabled_by: Option<Layer>) {
        self.enabled = enabled;
        self.disabled_by = disabled_by;
    }
}

/// All skills.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    built_in: BTreeMap<String, LoadedSkill>,
    layers: Option<Layers<LoadedSkill>>,
}

impl From<SkillLibrary> for SkillRegistry {
    fn from(library: SkillLibrary) -> Self {
        Self {
            built_in: library
                .all()
                .map(|skill| {
                    let loaded = LoadedSkill {
                        skill: skill.clone(),
                        source: None,
                        layer: Layer::Instance,
                        enabled: true,
                        disabled_by: None,
                    };
                    (skill.name.clone(), loaded)
                })
                .collect(),
            layers: None,
        }
    }
}

impl SkillRegistry {
    /// Adds the skills of a tenant and its members from the settings.
    #[must_use]
    pub fn layered(mut self, settings: TenantSettings) -> Self {
        self.layers = Some(Layers::new(settings, FileKind::Skill));
        self
    }

    /// Every skill as the user the current task acts for has it, also the ones
    /// switched off.
    pub fn all(&self) -> Vec<LoadedSkill> {
        match &self.layers {
            Some(layers) => layers
                .resolve(acting_user::current(), &self.built_in)
                .values()
                .cloned()
                .collect(),
            None => self.built_in.values().cloned().collect(),
        }
    }

    /// The names of the skills in use.
    pub fn names(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|known| known.enabled)
            .map(|known| known.skill.name)
            .collect()
    }

    /// A skill in use by name.
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.all()
            .into_iter()
            .find(|known| known.enabled && known.skill.name == name)
            .map(|known| known.skill)
    }

    /// Whether a skill exists, in use or not.
    pub fn exists(&self, name: &str) -> bool {
        self.all().iter().any(|known| known.skill.name == name)
    }
}
