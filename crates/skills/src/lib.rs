//! Loading of SKILL.md files in the open Agent Skills format.
//!
//! The model first sees only name and description. The full text is loaded on
//! request through a built-in tool.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Errors while loading skills.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// A file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// File path.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A file is malformed.
    #[error("invalid skill {path}: {detail}")]
    Invalid {
        /// File path.
        path: String,
        /// What is wrong.
        detail: String,
    },
}

/// One skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Skill name.
    pub name: String,
    /// When to use the skill.
    pub description: String,
    /// Full instructions.
    pub body: String,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

/// All available skills.
#[derive(Clone, Debug, Default)]
pub struct SkillLibrary {
    skills: BTreeMap<String, Skill>,
}

fn parse(text: &str, path: &str) -> Result<Skill, SkillError> {
    let invalid = |detail: String| SkillError::Invalid {
        path: path.to_owned(),
        detail,
    };
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| invalid("no frontmatter".to_owned()))?;
    let (frontmatter, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| invalid("unterminated frontmatter".to_owned()))?;
    let parsed: Frontmatter =
        serde_yaml_ng::from_str(frontmatter).map_err(|error| invalid(error.to_string()))?;
    Ok(Skill {
        name: parsed.name,
        description: parsed.description,
        body: body.trim().to_owned(),
    })
}

impl SkillLibrary {
    /// Loads every `<dir>/<skill>/SKILL.md`. A missing directory yields an empty library.
    pub fn load(dir: &Path) -> Result<Self, SkillError> {
        let mut skills = BTreeMap::new();
        let io = |path: &Path, source| SkillError::Io {
            path: path.display().to_string(),
            source,
        };
        if !dir.exists() {
            return Ok(Self::default());
        }
        for entry in std::fs::read_dir(dir).map_err(|error| io(dir, error))? {
            let path = entry
                .map_err(|error| io(dir, error))?
                .path()
                .join("SKILL.md");
            if path.is_file() {
                let text = std::fs::read_to_string(&path).map_err(|error| io(&path, error))?;
                let skill = parse(&text, &path.display().to_string())?;
                skills.insert(skill.name.clone(), skill);
            }
        }
        Ok(Self { skills })
    }

    /// Returns a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn loads_skills_and_ignores_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "brief",
            "---\nname: brief\ndescription: Writes briefs\nlicense: MIT\n---\n\nBe short.\n",
        );
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let library = SkillLibrary::load(dir.path()).unwrap();
        let skill = library.get("brief").unwrap();
        assert_eq!(skill.description, "Writes briefs");
        assert_eq!(skill.body, "Be short.");
        assert!(library.get("empty").is_none());
    }

    #[test]
    fn missing_directory_is_empty() {
        assert!(
            SkillLibrary::load(Path::new("/nonexistent"))
                .unwrap()
                .get("x")
                .is_none()
        );
    }

    #[test]
    fn rejects_malformed_skills() {
        for text in [
            "no frontmatter",
            "---\nname: x",
            "---\nname: only\n---\nbody",
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "bad", text);
            assert!(
                matches!(
                    SkillLibrary::load(dir.path()),
                    Err(SkillError::Invalid { .. })
                ),
                "{text}"
            );
        }
    }
}
