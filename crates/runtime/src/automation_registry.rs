//! The automations of a running server.
//!
//! Like agents, automations come from a directory you manage. Those created in
//! the apps belong to a tenant or a person and are stored with the settings;
//! changes take effect without a restart.

use crate::RegistryError;
use crate::layered::{LayeredFile, Layers};
use lighthouse_core::agent::slugify;
use lighthouse_core::automation::Automation;
use lighthouse_settings::{FileKind, Layer, TenantSettings};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// An automation with its file.
#[derive(Clone, Debug)]
pub struct LoadedAutomation {
    /// The definition.
    pub automation: Arc<Automation>,
    /// File content.
    pub source: String,
    /// Whether the apps can change it.
    pub editable: bool,
    /// The layer it comes from.
    pub layer: Layer,
    /// Whether a layer leaves it in use for this user.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
}

impl LayeredFile for LoadedAutomation {
    fn read(source: &str, layer: Layer) -> Option<Self> {
        let automation = Automation::parse(source).ok()?;
        Some(Self {
            automation: Arc::new(automation),
            source: source.to_owned(),
            editable: true,
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

/// All automations.
#[derive(Debug)]
pub struct AutomationRegistry {
    managed: PathBuf,
    created: PathBuf,
    automations: RwLock<BTreeMap<String, LoadedAutomation>>,
    layers: Option<Layers<LoadedAutomation>>,
}

fn markdown_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

impl AutomationRegistry {
    /// Loads the automations of both directories.
    pub fn load(managed: PathBuf, created: PathBuf) -> std::io::Result<Self> {
        let registry = Self {
            managed,
            created,
            automations: RwLock::default(),
            layers: None,
        };
        registry.reload()?;
        Ok(registry)
    }

    /// Loads the automations you manage in a directory, with the ones of a
    /// tenant and its members from the settings.
    pub fn layered(managed: PathBuf, settings: TenantSettings) -> std::io::Result<Self> {
        let registry = Self {
            created: PathBuf::new(),
            managed,
            automations: RwLock::default(),
            layers: Some(Layers::new(settings, FileKind::Automation)),
        };
        registry.reload()?;
        Ok(registry)
    }

    /// The automations as a user has them, or as the tenant has them for `None`.
    pub fn for_user(&self, user: Option<Uuid>) -> Arc<BTreeMap<String, LoadedAutomation>> {
        let built_in = self.automations.read().expect("automations lock");
        match &self.layers {
            Some(layers) => layers.resolve(user, &built_in),
            None => Arc::new(built_in.clone()),
        }
    }

    /// Returns an automation as a user has it.
    pub fn get_for(&self, user: Option<Uuid>, id: &str) -> Option<LoadedAutomation> {
        self.for_user(user).get(id).cloned()
    }

    /// The settings the automations of tenants and people are stored with.
    pub fn settings(&self) -> Option<&TenantSettings> {
        self.layers.as_ref().map(Layers::settings)
    }

    /// Reads both directories again. Invalid files are skipped and logged.
    pub fn reload(&self) -> std::io::Result<()> {
        let mut automations = BTreeMap::new();
        for (directory, editable) in [(&self.created, true), (&self.managed, false)] {
            if directory.as_os_str().is_empty() {
                continue;
            }
            for path in markdown_files(directory)? {
                let source = std::fs::read_to_string(&path)?;
                match Automation::parse(&source) {
                    Ok(automation) => {
                        automations.insert(
                            automation.id.clone(),
                            LoadedAutomation {
                                automation: Arc::new(automation),
                                source,
                                editable,
                                layer: if editable {
                                    Layer::Tenant
                                } else {
                                    Layer::Instance
                                },
                                enabled: true,
                                disabled_by: None,
                            },
                        );
                    }
                    Err(error) => {
                        let path = path.display().to_string();
                        tracing::error!(%path, %error, "skipping invalid automation");
                    }
                }
            }
        }
        *self.automations.write().expect("automations lock") = automations;
        Ok(())
    }

    /// Returns an automation, as the user the current task acts for has it.
    pub fn get(&self, id: &str) -> Option<LoadedAutomation> {
        self.get_for(lighthouse_audit::acting_user::current(), id)
    }

    /// Returns every automation, sorted by ID, as the user the current task
    /// acts for has them.
    pub fn list(&self) -> Vec<LoadedAutomation> {
        self.for_user(lighthouse_audit::acting_user::current())
            .values()
            .cloned()
            .collect()
    }

    /// Validates a file and stores it. An existing automation of that ID is replaced.
    pub fn save(&self, source: &str) -> Result<Arc<Automation>, RegistryError> {
        let automation = Automation::parse(source)?;
        if slugify(&automation.id) != automation.id {
            return Err(RegistryError::BadId(automation.id));
        }
        if self
            .get(&automation.id)
            .is_some_and(|known| !known.editable)
        {
            return Err(RegistryError::ReadOnly(automation.id));
        }
        std::fs::create_dir_all(&self.created)?;
        let path = self.created.join(format!("{}.md", automation.id));
        let temporary = path.with_extension("md.tmp");
        std::fs::write(&temporary, source)?;
        std::fs::rename(&temporary, &path)?;
        self.reload()?;
        Ok(Arc::new(automation))
    }

    /// Deletes an automation. Returns whether it existed.
    pub fn delete(&self, id: &str) -> Result<bool, RegistryError> {
        let Some(known) = self.get(id) else {
            return Ok(false);
        };
        if !known.editable {
            return Err(RegistryError::ReadOnly(id.to_owned()));
        }
        std::fs::remove_file(self.created.join(format!("{id}.md")))?;
        self.reload()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> String {
        format!(
            "---\nname: {name}\ntrigger:\n  manual: true\nsteps:\n  - agent: brief\n---\nHelps.\n"
        )
    }

    #[test]
    fn created_automations_can_change_and_managed_ones_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("fixed.md"), file("Fixed")).unwrap();
        std::fs::write(managed.join("broken.md"), "broken").unwrap();
        std::fs::write(managed.join("notes.txt"), "ignored").unwrap();
        let registry = AutomationRegistry::load(managed, dir.path().join("created")).unwrap();
        assert_eq!(registry.list().len(), 1);

        registry.save(&file("New one")).unwrap();
        let saved = registry.get("new-one").unwrap();
        assert!(saved.editable);
        assert!(saved.source.contains("Helps."));
        assert!(registry.delete("new-one").unwrap());
        assert!(!registry.delete("new-one").unwrap());
        assert!(matches!(
            registry.save(&file("Fixed")),
            Err(RegistryError::ReadOnly(_))
        ));
        assert!(matches!(
            registry.delete("fixed"),
            Err(RegistryError::ReadOnly(_))
        ));
        assert!(matches!(
            registry.save("nonsense"),
            Err(RegistryError::Invalid(_))
        ));
    }
}
