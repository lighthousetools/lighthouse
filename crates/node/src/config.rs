//! Local settings of the desktop agent.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// Port of the local hook server.
pub const DEFAULT_PORT: u16 = 47_800;

/// Settings written by `lighthouse-node pair`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Server URL.
    pub server: String,
    /// Device token.
    pub token: String,
    /// Machine name.
    pub name: String,
    /// Port of the local hook server.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Returns the default location, below the home directory.
pub fn default_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/lighthouse/node.toml"))
}

impl NodeConfig {
    /// Reads the settings.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// Writes the settings, readable only by this user, since they hold the token.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(toml::to_string(self)?.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn settings_round_trip_and_stay_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/node.toml");
        let config = NodeConfig {
            server: "https://lighthouse.example".to_owned(),
            token: "lht_x".to_owned(),
            name: "mac".to_owned(),
            port: DEFAULT_PORT,
        };
        config.save(&path).unwrap();
        assert_eq!(NodeConfig::load(&path).unwrap(), config);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
