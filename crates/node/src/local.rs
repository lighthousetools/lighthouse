//! Finds what Claude Code keeps on this machine: sessions that can be resumed, skills, and memories.
//!
//! Sessions are the transcripts directly in `~/.claude/projects/<folder>/`; subagent transcripts in deeper folders and
//! the folders of temporary directories are skipped. Of a session only metadata leaves the machine: its project,
//! branch, first prompt, message count and last activity. Skills are `~/.claude/skills/*/SKILL.md` and the
//! `.claude/skills` of every project a session worked in. Memories are the files under `memory/` of each project
//! folder; only projects whose files changed are sent again, and every secret is redacted here before it leaves.

use lighthouse_sessions::claude;
use lighthouse_sessions::history::MAX_PROMPT;
use lighthouse_sessions::{LocalSkill, PastSession};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How much of a new transcript is read to find its project, branch and first prompt.
const HEAD_BYTES: u64 = 2 * 1024 * 1024;
/// How much is read at once when counting messages.
const CHUNK: usize = 8 * 1024 * 1024;

/// What is known about one transcript.
#[derive(Clone, Debug, Default)]
struct Transcript {
    size: u64,
    modified: Option<SystemTime>,
    counted: u64,
    messages: i32,
    cwd: Option<String>,
    branch: Option<String>,
    first_prompt: Option<String>,
}

/// Memory files of one project, ready to send.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectMemories {
    /// Project name, or none for the home folder.
    pub project: Option<String>,
    /// File names and their redacted content.
    pub memories: Vec<(String, String)>,
}

impl ProjectMemories {
    /// The shape the server takes.
    pub fn to_json(&self) -> Value {
        json!({
            "project": self.project,
            "memories": self.memories.iter().map(|(file, content)| json!({"file": file, "content": content})).collect::<Vec<_>>(),
        })
    }
}

/// Scans the Claude Code folder and remembers what it saw, so later scans only read what changed.
#[derive(Debug)]
pub struct Scanner {
    home: PathBuf,
    transcripts: HashMap<PathBuf, Transcript>,
    memory_prints: HashMap<String, u64>,
}

/// Folders of temporary directories, which hold throwaway sessions.
fn is_temporary(folder: &str) -> bool {
    folder.contains("-var-folders-")
        || folder.starts_with("-private-tmp")
        || folder.starts_with("-tmp")
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// The text a user typed, unless it is a command, a caveat or other noise Claude Code adds.
fn typed_prompt(line: &Value) -> Option<String> {
    if line["type"] != "user"
        || line["isMeta"].as_bool().unwrap_or(false)
        || line["isSidechain"].as_bool().unwrap_or(false)
    {
        return None;
    }
    let content = &line["message"]["content"];
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let noise = text.is_empty()
        || text.starts_with('<')
        || text.starts_with("Caveat:")
        || text.starts_with("[Request interrupted");
    (!noise).then(|| text.chars().take(MAX_PROMPT).collect())
}

/// Reads a frontmatter field such as `name` or `description`.
fn front(content: &str, field: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let (head, _) = rest.split_once("\n---")?;
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == field)
            .then(|| value.trim().trim_matches('"').to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn sorted_dir(path: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries
}

impl Scanner {
    /// A scanner for the Claude Code folder below `home`.
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            transcripts: HashMap::new(),
            memory_prints: HashMap::new(),
        }
    }

    fn projects(&self) -> PathBuf {
        self.home.join(".claude").join("projects")
    }

    /// Finds the project, branch and first prompt in the head of a new transcript.
    fn read_head(path: &Path, transcript: &mut Transcript) {
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        for line in BufReader::new(file.take(HEAD_BYTES)).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if transcript.cwd.is_none() {
                transcript.cwd = value["cwd"].as_str().map(str::to_owned);
            }
            if transcript.branch.is_none() {
                transcript.branch = value["gitBranch"]
                    .as_str()
                    .filter(|branch| !branch.is_empty())
                    .map(str::to_owned);
            }
            if transcript.first_prompt.is_none() {
                transcript.first_prompt = typed_prompt(&value);
            }
            if transcript.cwd.is_some()
                && transcript.branch.is_some()
                && transcript.first_prompt.is_some()
            {
                break;
            }
        }
    }

    /// Counts the messages of the complete lines added since the last scan.
    fn count_new(path: &Path, transcript: &mut Transcript) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(transcript.counted)).is_err() {
            return;
        }
        let mut buffer = vec![0u8; CHUNK];
        let mut carry: Vec<u8> = Vec::new();
        let mut offset = transcript.counted;
        while let Ok(read) = file.read(&mut buffer) {
            if read == 0 {
                break;
            }
            carry.extend_from_slice(&buffer[..read]);
            let Some(last) = carry.iter().rposition(|byte| *byte == b'\n') else {
                continue;
            };
            for line in carry[..last].split(|byte| *byte == b'\n') {
                let message = |kind: &[u8]| line.windows(kind.len()).any(|window| window == kind);
                if message(br#""type":"user""#) || message(br#""type":"assistant""#) {
                    transcript.messages = transcript.messages.saturating_add(1);
                }
            }
            offset += last as u64 + 1;
            carry.drain(..=last);
        }
        transcript.counted = offset;
    }

    /// The sessions on this machine. Only transcripts that changed since the last scan are read again.
    pub fn sessions(&mut self) -> Vec<PastSession> {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        for folder in sorted_dir(&self.projects()) {
            let Some(name) = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !folder.is_dir() || is_temporary(&name) {
                continue;
            }
            for path in sorted_dir(&folder) {
                if path.extension().is_none_or(|ext| ext != "jsonl") || !path.is_file() {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                seen.insert(path.clone());
                let entry = self.transcripts.entry(path.clone()).or_default();
                if entry.size > meta.len() {
                    *entry = Transcript::default();
                }
                if entry.size != meta.len() || entry.modified != meta.modified().ok() {
                    if entry.size == 0 {
                        Self::read_head(&path, entry);
                    }
                    Self::count_new(&path, entry);
                    entry.size = meta.len();
                    entry.modified = meta.modified().ok();
                }
                let Some(id) = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                else {
                    continue;
                };
                let project = entry
                    .cwd
                    .as_deref()
                    .and_then(|cwd| Path::new(cwd).file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .or_else(|| claude::project_name(&name, &self.home))
                    .unwrap_or_else(|| "home".to_owned());
                let Some(last) = entry.modified else { continue };
                found.push(PastSession {
                    id,
                    project,
                    cwd: entry.cwd.clone(),
                    branch: entry.branch.clone(),
                    first_prompt: entry.first_prompt.clone(),
                    messages: entry.messages,
                    last_active_at: last.into(),
                });
            }
        }
        self.transcripts.retain(|path, _| seen.contains(path));
        found
    }

    /// The skills of the user and of the given project folders.
    pub fn skills(&self, projects: &BTreeSet<String>) -> Vec<LocalSkill> {
        let mut found = Vec::new();
        let mut read = |dir: PathBuf, scope: &str, project: Option<&str>| {
            for skill in sorted_dir(&dir) {
                let file = skill.join("SKILL.md");
                let Ok(content) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let folder = skill
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                found.push(LocalSkill {
                    name: front(&content, "name").unwrap_or(folder),
                    description: front(&content, "description"),
                    path: skill.to_string_lossy().into_owned(),
                    scope: scope.to_owned(),
                    project: project.map(str::to_owned),
                });
            }
        };
        read(self.home.join(".claude").join("skills"), "user", None);
        for project in projects {
            if Path::new(project) == self.home {
                continue;
            }
            read(
                Path::new(project).join(".claude").join("skills"),
                "project",
                Some(project),
            );
        }
        found
    }

    /// The project folders the sessions worked in.
    pub fn project_folders(sessions: &[PastSession]) -> BTreeSet<String> {
        sessions
            .iter()
            .filter_map(|session| session.cwd.clone())
            .collect()
    }

    /// The memories of every project whose files changed since the last scan, redacted.
    pub fn changed_memories(&mut self) -> Vec<ProjectMemories> {
        let mut changed = Vec::new();
        for folder in sorted_dir(&self.projects()) {
            let Some(name) = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if is_temporary(&name) {
                continue;
            }
            let files: Vec<PathBuf> = sorted_dir(&folder.join("memory"))
                .into_iter()
                .filter(|file| {
                    file.extension().is_some_and(|ext| ext == "md")
                        && file.file_name().is_some_and(|file| file != "MEMORY.md")
                })
                .collect();
            if files.is_empty() {
                continue;
            }
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for file in &files {
                file.hash(&mut hasher);
                std::fs::metadata(file)
                    .map(|meta| meta.len())
                    .unwrap_or(0)
                    .hash(&mut hasher);
                modified(file).hash(&mut hasher);
            }
            let print = hasher.finish();
            if self.memory_prints.get(&name) == Some(&print) {
                continue;
            }
            let memories: Vec<(String, String)> = files
                .iter()
                .filter_map(|file| {
                    let content = std::fs::read_to_string(file).ok()?;
                    let (content, _) = claude::redact(&content);
                    Some((file.file_name()?.to_string_lossy().into_owned(), content))
                })
                .collect();
            self.memory_prints.insert(name.clone(), print);
            changed.push(ProjectMemories {
                project: claude::project_name(&name, &self.home),
                memories,
            });
        }
        changed
    }

    /// Forgets which memories were sent, so they go out again, for example after a failed report.
    pub fn forget_memories(&mut self, projects: &[ProjectMemories]) {
        let names: BTreeMap<Option<String>, ()> = projects
            .iter()
            .map(|project| (project.project.clone(), ()))
            .collect();
        let home = self.home.clone();
        self.memory_prints
            .retain(|folder, _| !names.contains_key(&claude::project_name(folder, &home)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(value: Value) -> String {
        value.to_string() + "\n"
    }

    #[test]
    fn sessions_carry_metadata_only_and_are_read_again_when_they_grow() {
        let home = tempfile::tempdir().unwrap();
        let app = home.path().join("code").join("app");
        std::fs::create_dir_all(&app).unwrap();
        let folder = home.path().join(".claude/projects/-code-app");
        std::fs::create_dir_all(folder.join("s1/subagents")).unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects/-private-tmp-x")).unwrap();
        std::fs::write(
            home.path().join(".claude/projects/-private-tmp-x/t.jsonl"),
            "{}\n",
        )
        .unwrap();
        std::fs::write(folder.join("s1/subagents/a.jsonl"), "{}\n").unwrap();
        let cwd = app.to_string_lossy().into_owned();
        let mut transcript = std::fs::File::create(folder.join("s1.jsonl")).unwrap();
        transcript
            .write_all(
                (line(json!({"type": "user", "cwd": cwd, "gitBranch": "main", "message": {"content": "<command-name>/clear</command-name>"}}))
                    + &line(json!({"type": "user", "cwd": cwd, "message": {"content": [{"type": "text", "text": "Fix   the export\nplease"}]}}))
                    + &line(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "secret answer"}]}})))
                .as_bytes(),
            )
            .unwrap();
        let mut scanner = Scanner::new(home.path().to_path_buf());
        let sessions = scanner.sessions();
        assert_eq!(sessions.len(), 1, "temp folders and subagents are skipped");
        let session = &sessions[0];
        assert_eq!(session.id, "s1");
        assert_eq!(session.project, "app");
        assert_eq!(session.branch.as_deref(), Some("main"));
        assert_eq!(
            session.first_prompt.as_deref(),
            Some("Fix the export please")
        );
        assert_eq!(session.messages, 3);
        assert!(
            !serde_json::to_string(&sessions)
                .unwrap()
                .contains("secret answer")
        );
        transcript
            .write_all(
                line(json!({"type": "assistant", "message": {"content": "more"}})).as_bytes(),
            )
            .unwrap();
        assert_eq!(scanner.sessions()[0].messages, 4);
        let skills_dir = app.join(".claude/skills/jumpstart");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: jumpstart\ndescription: The daily briefing\n---\nSteps",
        )
        .unwrap();
        let own = home.path().join(".claude/skills/notes");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("SKILL.md"), "no frontmatter").unwrap();
        let skills = scanner.skills(&Scanner::project_folders(&sessions));
        assert_eq!(skills.len(), 2);
        assert_eq!(
            (skills[0].name.as_str(), skills[0].scope.as_str()),
            ("notes", "user")
        );
        assert_eq!(skills[1].description.as_deref(), Some("The daily briefing"));
        assert_eq!(skills[1].project.as_deref(), Some(cwd.as_str()));
    }

    #[test]
    fn memories_go_out_when_they_change_and_without_secrets() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("code/app")).unwrap();
        let memory = home.path().join(".claude/projects/-code-app/memory");
        std::fs::create_dir_all(&memory).unwrap();
        // The folder name encodes the path below `/`, which the test cannot create, so it names the project itself.
        std::fs::write(memory.join("MEMORY.md"), "- index").unwrap();
        std::fs::write(
            memory.join("deploy.md"),
            "---\nname: deploy\ntype: project\n---\nToken ghp_abcdefghijklmnopqrstuvwxyz0123",
        )
        .unwrap();
        let mut scanner = Scanner::new(home.path().to_path_buf());
        let changed = scanner.changed_memories();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].memories.len(), 1, "the index is not a memory");
        assert!(changed[0].memories[0].1.contains("[redacted]"));
        assert!(scanner.changed_memories().is_empty(), "nothing changed");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            memory.join("tests.md"),
            "---\nname: tests\ntype: feedback\n---\nRun e2e first.",
        )
        .unwrap();
        let changed = scanner.changed_memories();
        assert_eq!(
            changed[0].memories.len(),
            2,
            "all files of a changed project go out"
        );
        scanner.forget_memories(&changed);
        assert_eq!(
            scanner.changed_memories().len(),
            1,
            "a forgotten project goes out again"
        );
        assert_eq!(changed[0].to_json()["memories"][0]["file"], "deploy.md");
    }
}
