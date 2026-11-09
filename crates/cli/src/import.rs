//! Imports what Claude Code already knows about a person into Lighthouse.
//!
//! Claude Code keeps memories per project under `~/.claude/projects/<folder>/memory/*.md`, each with a type of `user`,
//! `feedback`, `project` or `reference`, and skills under `~/.claude/skills/<name>/SKILL.md`. The import maps them:
//!
//! - `user` memories become confirmed facts.
//! - `feedback` memories of the home folder become rules. Feedback of a single project stays with that project.
//! - Every project becomes one Brain entry of the type `project` with its state, references and conventions.
//! - Skills are listed, and copied into a skills folder when one is given.
//!
//! Anything that looks like a key, token or password is replaced by `[redacted]` before it leaves the machine.
//! Rules, facts and projects that already exist are skipped, so the import can run again.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::client::Client;

/// The longest rule or fact in characters.
const MAX_SENTENCE: usize = 500;
/// The longest Brain entry in characters.
const MAX_BODY: usize = 200_000;

/// What an import would create.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    /// Standing instructions for every agent.
    #[serde(default)]
    pub rules: Vec<PlannedRule>,
    /// Facts about the person.
    #[serde(default)]
    pub facts: Vec<PlannedFact>,
    /// Knowledge for the Second Brain.
    #[serde(default)]
    pub brain: Vec<PlannedEntry>,
    /// Skills found on the machine.
    #[serde(default)]
    pub skills: Vec<PlannedSkill>,
    /// What was redacted or left out, for the person to check.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A rule.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PlannedRule {
    /// The rule.
    pub text: String,
    /// A category such as `writing` or the project it came from.
    #[serde(default)]
    pub category: Option<String>,
}

/// A fact.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PlannedFact {
    /// The fact.
    pub text: String,
    /// A category such as `profile`.
    #[serde(default)]
    pub category: Option<String>,
}

/// A Brain entry.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PlannedEntry {
    /// Title. Entries are matched by it.
    pub title: String,
    /// Markdown.
    pub body: String,
    /// One of the Brain's entry types.
    #[serde(rename = "type", default = "project_kind")]
    pub kind: String,
    /// Titles of related entries.
    #[serde(default)]
    pub links: Vec<String>,
    /// Where it came from.
    #[serde(default)]
    pub source: String,
}

fn project_kind() -> String {
    "project".to_owned()
}

/// A skill.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PlannedSkill {
    /// Folder name.
    pub name: String,
    /// The folder.
    pub path: PathBuf,
    /// Whether it has more files than `SKILL.md`, which Lighthouse does not load.
    pub has_references: bool,
}

/// One memory file.
#[derive(Debug, PartialEq)]
struct Memory {
    name: String,
    kind: String,
    text: String,
}

/// Reads a memory file: frontmatter with `name`, `description` and `type` (or `metadata.type`), then the text.
fn parse_memory(content: &str) -> Option<Memory> {
    let rest = content.strip_prefix("---")?;
    let (front, body) = rest.split_once("\n---")?;
    let mut name = None;
    let mut kind = None;
    let mut description = None;
    for line in front.lines() {
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim().trim_matches('"').to_owned();
            match key.trim() {
                "name" if !value.is_empty() => name = Some(value),
                "type" if !value.is_empty() => kind = Some(value),
                "description" if !value.is_empty() => description = Some(value),
                _ => {}
            }
        }
    }
    let body = body.trim_start_matches(['-', '\n']).trim();
    let text = if body.is_empty() {
        description?
    } else {
        body.to_owned()
    };
    Some(Memory {
        name: name?,
        kind: kind.unwrap_or_else(|| "project".to_owned()),
        text,
    })
}

/// The statement of a memory, without its `**Why:**` and `**How to apply:**` parts.
fn statement(text: &str) -> String {
    let end = ["**Why:**", "**How to apply:**", "Why:"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len());
    let sentence = text[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&sentence, MAX_SENTENCE)
}

fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(limit).collect();
    short.push('…');
    short
}

/// Prefixes of well-known keys and tokens.
const SECRET_PREFIXES: [&str; 14] = [
    "sk-",
    "sk_live_",
    "sk_test_",
    "pk_live_",
    "ghp_",
    "gho_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "lin_api_",
    "lht_",
    "AKIA",
    "AIza",
];

/// Whether a word looks like a secret: a known prefix, a private key, or a long random string.
fn is_secret(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    if word.len() < 16 {
        return false;
    }
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| word.starts_with(prefix))
    {
        return true;
    }
    let digits = word.chars().filter(char::is_ascii_digit).count();
    let upper = word.chars().filter(char::is_ascii_uppercase).count();
    let lower = word.chars().filter(char::is_ascii_lowercase).count();
    let plain = word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '='));
    plain && word.len() >= 32 && digits >= 4 && upper >= 2 && lower >= 2
}

/// Replaces secrets. Returns the text and whether anything was replaced.
fn redact(text: &str) -> (String, bool) {
    if text.contains("-----BEGIN") {
        return ("[redacted: contained a private key]".to_owned(), true);
    }
    let mut changed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let lower = line.to_lowercase();
            let labelled = [
                "password:",
                "password=",
                "passwort:",
                "secret:",
                "token:",
                "api key:",
                "api_key=",
            ]
            .iter()
            .any(|label| lower.contains(label));
            line.split(' ')
                .map(|word| {
                    let after_label = labelled
                        && word.len() >= 8
                        && !word.ends_with(':')
                        && word.chars().any(|c| c.is_ascii_digit());
                    if is_secret(word) || after_label {
                        changed = true;
                        "[redacted]".to_owned()
                    } else {
                        word.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    (lines.join("\n"), changed)
}

/// Turns a Claude Code project folder like `-Users-jane-code-my-app` back into a path, checking which hyphens were
/// slashes. Falls back to the folder name.
fn folder_path(folder: &str) -> PathBuf {
    let parts: Vec<&str> = folder.trim_start_matches('-').split('-').collect();
    let mut path = PathBuf::from("/");
    let mut pending = String::new();
    for part in parts {
        if pending.is_empty() {
            pending = part.to_owned();
        } else {
            pending = format!("{pending}-{part}");
        }
        let candidate = path.join(&pending);
        if candidate.is_dir() {
            path = candidate;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        path = path.join(pending);
    }
    path
}

/// The name of a project folder: the last part of its path.
fn project_name(folder: &str, home: &Path) -> Option<String> {
    let path = folder_path(folder);
    if path == home || path == Path::new("/") {
        return None;
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Reads `~/.claude` into a plan.
pub fn from_claude(claude: &Path, home: &Path) -> anyhow::Result<Plan> {
    let mut plan = Plan::default();
    let mut projects: BTreeMap<String, BTreeMap<&'static str, Vec<String>>> = BTreeMap::new();
    let projects_dir = claude.join("projects");
    let mut folders: Vec<PathBuf> = match std::fs::read_dir(&projects_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    folders.sort();
    for folder in folders {
        let memory = folder.join("memory");
        let Ok(files) = std::fs::read_dir(&memory) else {
            continue;
        };
        let folder_name = folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let project = project_name(&folder_name, home);
        let mut files: Vec<PathBuf> = files
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        files.sort();
        for file in files {
            let is_memory = file.extension().is_some_and(|ext| ext == "md")
                && file.file_name().is_some_and(|name| name != "MEMORY.md");
            if !is_memory {
                continue;
            }
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let Some(memory) = parse_memory(&content) else {
                plan.warnings
                    .push(format!("skipped {} (no frontmatter)", file.display()));
                continue;
            };
            let (text, redacted) = redact(&memory.text);
            if redacted {
                plan.warnings
                    .push(format!("redacted a secret in {}", file.display()));
            }
            match (memory.kind.as_str(), &project) {
                ("user", _) => plan.facts.push(PlannedFact {
                    text: statement(&text),
                    category: Some("profile".to_owned()),
                }),
                ("feedback", None) => plan.rules.push(PlannedRule {
                    text: statement(&text),
                    category: None,
                }),
                (kind, Some(project)) => {
                    let section = match kind {
                        "feedback" => "Conventions",
                        "reference" => "References",
                        _ => "State",
                    };
                    let item = format!(
                        "- **{}:** {}",
                        memory.name,
                        text.split_whitespace().collect::<Vec<_>>().join(" ")
                    );
                    projects
                        .entry(project.clone())
                        .or_default()
                        .entry(section)
                        .or_default()
                        .push(item);
                }
                (_, None) => {
                    let item = format!(
                        "- **{}:** {}",
                        memory.name,
                        text.split_whitespace().collect::<Vec<_>>().join(" ")
                    );
                    projects
                        .entry("General".to_owned())
                        .or_default()
                        .entry("Notes")
                        .or_default()
                        .push(item);
                }
            }
        }
    }
    for (project, sections) in projects {
        let mut body = String::new();
        for name in ["State", "Conventions", "References", "Notes"] {
            if let Some(items) = sections.get(name) {
                body.push_str(&format!("## {name}\n\n{}\n\n", items.join("\n")));
            }
        }
        plan.brain.push(PlannedEntry {
            title: project.clone(),
            body: cut(body.trim(), MAX_BODY),
            kind: "project".to_owned(),
            links: Vec::new(),
            source: format!("claude-code/{project}"),
        });
    }
    plan.rules.dedup_by(|a, b| a.text == b.text);
    plan.facts.dedup_by(|a, b| a.text == b.text);
    plan.skills = skills(&claude.join("skills"));
    Ok(plan)
}

fn skills(dir: &Path) -> Vec<PlannedSkill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PlannedSkill> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("SKILL.md").is_file())
        .map(|path| {
            let has_references = std::fs::read_dir(&path)
                .map(|files| files.count() > 1)
                .unwrap_or(false);
            PlannedSkill {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path,
                has_references,
            }
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Copies the skills into a folder Lighthouse reads, keeping existing ones. Returns how many were copied.
pub fn copy_skills(plan: &Plan, target: &Path) -> anyhow::Result<usize> {
    let mut copied = 0;
    for skill in &plan.skills {
        let destination = target.join(&skill.name);
        if destination.exists() {
            continue;
        }
        std::fs::create_dir_all(&destination)?;
        let (text, _) = redact(&std::fs::read_to_string(skill.path.join("SKILL.md"))?);
        std::fs::write(destination.join("SKILL.md"), text)?;
        copied += 1;
    }
    Ok(copied)
}

fn texts(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str().map(str::to_owned))
        .collect()
}

/// What an import did.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Created rules, facts and entries.
    pub created: [usize; 3],
    /// Skipped because they already exist.
    pub skipped: usize,
}

/// Sends a plan to a server. Existing rules and facts with the same text and entries with the same title are skipped.
pub async fn apply(client: &Client, plan: &Plan) -> anyhow::Result<Outcome> {
    let mut outcome = Outcome::default();
    let rules = texts(&client.get("/rules").await?);
    for rule in &plan.rules {
        if rules.contains(&rule.text) {
            outcome.skipped += 1;
            continue;
        }
        let mut body = json!({"text": rule.text, "enabled": true});
        if let Some(category) = &rule.category {
            body["category"] = json!(category);
        }
        client.post("/rules", &body).await?;
        outcome.created[0] += 1;
    }
    let facts = texts(&client.get("/memory").await?);
    for fact in &plan.facts {
        if facts.contains(&fact.text) {
            outcome.skipped += 1;
            continue;
        }
        client
            .post(
                "/memory",
                &json!({"text": fact.text, "category": fact.category}),
            )
            .await?;
        outcome.created[1] += 1;
    }
    for entry in &plan.brain {
        let path = format!("/brain/resolve?title={}", encode(&entry.title));
        if client.get(&path).await.is_ok() {
            outcome.skipped += 1;
            continue;
        }
        let body = json!({
            "type": entry.kind,
            "title": entry.title,
            "body": entry.body,
            "links": entry.links,
            "sources": [{"kind": "import", "ref": entry.source}],
        });
        client.post("/brain/entries", &body).await?;
        outcome.created[2] += 1;
    }
    Ok(outcome)
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn memories_are_parsed_with_either_type_field() {
        let nested = "---\nname: no-trailers\ndescription: d\nmetadata:\n  type: feedback\n---\n\nNo trailers.\n**Why:** asked.";
        let memory = parse_memory(nested).unwrap();
        assert_eq!(memory.kind, "feedback");
        assert_eq!(statement(&memory.text), "No trailers.");
        let flat = "---\nname: role\ndescription: The owner is a CTO\ntype: user\n---\n";
        assert_eq!(parse_memory(flat).unwrap().text, "The owner is a CTO");
        assert!(parse_memory("no frontmatter").is_none());
    }

    #[test]
    fn secrets_are_redacted() {
        let (text, changed) = redact("use lin_api_abcdefghijklmnop1234 here");
        assert!(changed);
        assert_eq!(text, "use [redacted] here");
        let (text, changed) = redact("password: Hunter2Hunter2");
        assert!(changed && !text.contains("Hunter2"));
        let (_, changed) = redact("Deploy with GitHub Actions to the server on Fridays.");
        assert!(!changed);
        assert!(redact("-----BEGIN OPENSSH PRIVATE KEY-----").1);
        assert!(is_secret("Ab3dEf9hIj2kLm5nOp8qRs1tUv4wXy7z"));
        assert!(!is_secret("crates/server/tests/end_to_end.rs"));
    }

    #[test]
    fn a_claude_folder_becomes_rules_facts_and_projects() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let code = home.join("code").join("my-app");
        std::fs::create_dir_all(&code).unwrap();
        let claude = root.path().join("claude");
        let encoded = |path: &Path| path.to_string_lossy().replace('/', "-");
        let global = claude.join("projects").join(encoded(&home)).join("memory");
        let project = claude.join("projects").join(encoded(&code)).join("memory");
        write(&global.join("MEMORY.md"), "- index");
        write(
            &global.join("a.md"),
            "---\nname: english\ntype: feedback\n---\nWrite commits in English.\n**Why:** public.",
        );
        write(
            &global.join("b.md"),
            "---\nname: role\ntype: user\n---\nThe owner runs two companies.",
        );
        write(
            &project.join("c.md"),
            "---\nname: deploy\ntype: project\n---\nDeploys on push to main.",
        );
        write(
            &project.join("d.md"),
            "---\nname: tests\ntype: feedback\n---\nRun the e2e suite first.",
        );
        write(
            &project.join("e.md"),
            "---\nname: key\ntype: reference\n---\nToken ghp_abcdefghijklmnopqrstuvwxyz0123",
        );
        write(
            &claude.join("skills").join("report").join("SKILL.md"),
            "---\nname: report\n---\nWrite it.",
        );
        write(
            &claude
                .join("skills")
                .join("report")
                .join("references")
                .join("x.md"),
            "x",
        );

        let plan = from_claude(&claude, &home).unwrap();
        assert_eq!(
            plan.rules,
            vec![PlannedRule {
                text: "Write commits in English.".to_owned(),
                category: None
            }]
        );
        assert_eq!(plan.facts[0].text, "The owner runs two companies.");
        assert_eq!(plan.brain.len(), 1);
        let entry = &plan.brain[0];
        assert_eq!(entry.title, "my-app");
        assert!(entry.body.contains("## State") && entry.body.contains("Deploys on push"));
        assert!(entry.body.contains("## Conventions") && entry.body.contains("e2e suite"));
        assert!(entry.body.contains("[redacted]") && !entry.body.contains("ghp_"));
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.skills.len(), 1);
        assert!(plan.skills[0].has_references);

        let target = root.path().join("skills");
        assert_eq!(copy_skills(&plan, &target).unwrap(), 1);
        assert_eq!(copy_skills(&plan, &target).unwrap(), 0);
    }

    #[test]
    fn a_curated_plan_reads_from_json() {
        let plan: Plan = serde_json::from_value(json!({
            "rules": [{"text": "Answer in German."}],
            "brain": [{"title": "Atlas", "body": "B"}]
        }))
        .unwrap();
        assert_eq!(plan.brain[0].kind, "project");
        assert!(plan.facts.is_empty());
    }
}
