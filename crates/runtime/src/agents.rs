use lighthouse_core::agent::{AgentDefinition, AgentError};
use std::path::{Path, PathBuf};

/// One agent file and its parse result.
#[derive(Debug)]
pub struct AgentFile {
    /// File path.
    pub path: PathBuf,
    /// Parsed definition or the reason it is invalid.
    pub agent: Result<AgentDefinition, AgentError>,
}

/// Reads every `*.md` file in a directory. A missing directory yields nothing.
pub fn load_agents(dir: &Path) -> std::io::Result<Vec<AgentFile>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            let text = std::fs::read_to_string(&path)?;
            files.push(AgentFile {
                agent: AgentDefinition::parse(&text),
                path,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_markdown_files_and_reports_errors_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let valid = "---\nname: A\nscope: org\ntrigger:\n  manual: true\n---\nGo.\n";
        std::fs::write(dir.path().join("a.md"), valid).unwrap();
        std::fs::write(dir.path().join("b.md"), "broken").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let files = load_agents(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].agent.is_ok());
        assert!(files[1].agent.is_err());
    }

    #[test]
    fn missing_directory_is_empty() {
        assert!(load_agents(Path::new("/nonexistent")).unwrap().is_empty());
    }
}
