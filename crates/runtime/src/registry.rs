//! The agents of a running server.
//!
//! Agents come from two directories. Files you manage yourself are read-only
//! here. Agents created in the apps land in a second directory, and a change
//! takes effect without a restart.

use crate::load_agents;
use lighthouse_core::agent::{AgentDefinition, AgentError, slugify};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Errors when changing agents.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The file is invalid.
    #[error(transparent)]
    Invalid(#[from] AgentError),
    /// The ID cannot be a file name.
    #[error("the agent ID `{0}` must be lowercase letters, digits and dashes")]
    BadId(String),
    /// The agent comes from a file managed outside the apps.
    #[error("the agent `{0}` is managed as a file on the server")]
    ReadOnly(String),
    /// A file operation failed.
    #[error("cannot write the agent: {0}")]
    Io(#[from] std::io::Error),
}

/// An agent with its file.
#[derive(Clone, Debug)]
pub struct LoadedAgent {
    /// The definition.
    pub agent: Arc<AgentDefinition>,
    /// File content.
    pub source: String,
    /// Whether the apps can change it.
    pub editable: bool,
}

/// All agents.
#[derive(Debug)]
pub struct AgentRegistry {
    managed: PathBuf,
    created: PathBuf,
    agents: RwLock<BTreeMap<String, LoadedAgent>>,
}

impl AgentRegistry {
    /// Loads the agents of both directories.
    pub fn load(managed: PathBuf, created: PathBuf) -> std::io::Result<Self> {
        let registry = Self {
            managed,
            created,
            agents: RwLock::default(),
        };
        registry.reload()?;
        Ok(registry)
    }

    /// Reads both directories again. Invalid files are skipped and logged.
    pub fn reload(&self) -> std::io::Result<()> {
        let mut agents = BTreeMap::new();
        for (directory, editable) in [(&self.created, true), (&self.managed, false)] {
            for file in load_agents(directory)? {
                let path = file.path.display().to_string();
                match file.agent {
                    Ok(agent) => {
                        agents.insert(
                            agent.id.clone(),
                            LoadedAgent {
                                agent: Arc::new(agent),
                                source: std::fs::read_to_string(&file.path)?,
                                editable,
                            },
                        );
                    }
                    Err(error) => tracing::error!(%path, %error, "skipping invalid agent"),
                }
            }
        }
        *self.agents.write().expect("agents lock") = agents;
        Ok(())
    }

    /// Returns an agent.
    pub fn get(&self, id: &str) -> Option<LoadedAgent> {
        self.agents.read().expect("agents lock").get(id).cloned()
    }

    /// Returns every agent, sorted by ID.
    pub fn list(&self) -> Vec<LoadedAgent> {
        let agents = self.agents.read().expect("agents lock");
        agents.values().cloned().collect()
    }

    /// Validates a file and stores it. An existing agent of that ID is replaced.
    pub fn save(&self, source: &str) -> Result<Arc<AgentDefinition>, RegistryError> {
        let agent = AgentDefinition::parse(source)?;
        if slugify(&agent.id) != agent.id || agent.id.is_empty() {
            return Err(RegistryError::BadId(agent.id));
        }
        if self.get(&agent.id).is_some_and(|known| !known.editable) {
            return Err(RegistryError::ReadOnly(agent.id));
        }
        std::fs::create_dir_all(&self.created)?;
        let path = self.created.join(format!("{}.md", agent.id));
        let temporary = path.with_extension("md.tmp");
        std::fs::write(&temporary, source)?;
        std::fs::rename(&temporary, &path)?;
        self.reload()?;
        Ok(Arc::new(agent))
    }

    /// Deletes an agent. Returns whether it existed.
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
        format!("---\nname: {name}\nscope: org\ntrigger:\n  manual: true\n---\nGo.\n")
    }

    fn registry() -> (AgentRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("fixed.md"), file("Fixed")).unwrap();
        std::fs::write(managed.join("broken.md"), "broken").unwrap();
        let registry = AgentRegistry::load(managed, dir.path().join("created")).unwrap();
        (registry, dir)
    }

    #[test]
    fn created_agents_can_change_and_managed_ones_cannot() {
        let (registry, _dir) = registry();
        assert_eq!(registry.list().len(), 1);
        registry.save(&file("New one")).unwrap();
        let agent = registry.get("new-one").unwrap();
        assert!(agent.editable);
        assert!(agent.source.contains("Go."));
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
    }

    #[test]
    fn invalid_files_and_unsafe_ids_are_refused() {
        let (registry, _dir) = registry();
        assert!(matches!(
            registry.save("nonsense"),
            Err(RegistryError::Invalid(_))
        ));
        let unsafe_id =
            "---\nname: X\nid: ../evil\nscope: org\ntrigger:\n  manual: true\n---\nGo.\n";
        assert!(matches!(
            registry.save(unsafe_id),
            Err(RegistryError::BadId(_))
        ));
    }
}
