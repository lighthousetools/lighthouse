//! The automations of a running server.
//!
//! Like agents, automations come from a directory you manage and a second one
//! for automations created in the apps. Changes take effect without a restart.

use crate::RegistryError;
use lighthouse_core::agent::slugify;
use lighthouse_core::automation::Automation;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// An automation with its file.
#[derive(Clone, Debug)]
pub struct LoadedAutomation {
    /// The definition.
    pub automation: Arc<Automation>,
    /// File content.
    pub source: String,
    /// Whether the apps can change it.
    pub editable: bool,
}

/// All automations.
#[derive(Debug)]
pub struct AutomationRegistry {
    managed: PathBuf,
    created: PathBuf,
    automations: RwLock<BTreeMap<String, LoadedAutomation>>,
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
        };
        registry.reload()?;
        Ok(registry)
    }

    /// Reads both directories again. Invalid files are skipped and logged.
    pub fn reload(&self) -> std::io::Result<()> {
        let mut automations = BTreeMap::new();
        for (directory, editable) in [(&self.created, true), (&self.managed, false)] {
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

    /// Returns an automation.
    pub fn get(&self, id: &str) -> Option<LoadedAutomation> {
        let automations = self.automations.read().expect("automations lock");
        automations.get(id).cloned()
    }

    /// Returns every automation, sorted by ID.
    pub fn list(&self) -> Vec<LoadedAutomation> {
        let automations = self.automations.read().expect("automations lock");
        automations.values().cloned().collect()
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
