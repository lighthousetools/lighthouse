//! What Claude Code keeps on a machine, read the same way by the importer, the desktop agent and the server.
//!
//! Memories live under `~/.claude/projects/<folder>/memory/*.md` with a frontmatter `type` of `user`, `feedback`,
//! `project` or `reference`. Project folders encode a path, such as `-Users-jane-code-my-app`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The longest rule or fact in characters.
pub const MAX_SENTENCE: usize = 500;
/// The longest Brain entry in characters.
pub const MAX_BODY: usize = 200_000;

/// One memory file.
#[derive(Debug, PartialEq)]
pub struct Memory {
    /// Its `name`.
    pub name: String,
    /// `user`, `feedback`, `project` or `reference`.
    pub kind: String,
    /// The text, or the description when the text is empty.
    pub text: String,
}

/// Reads a memory file: frontmatter with `name`, `description` and `type` (or `metadata.type`), then the text.
pub fn parse_memory(content: &str) -> Option<Memory> {
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
pub fn statement(text: &str) -> String {
    let end = ["**Why:**", "**How to apply:**", "Why:"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len());
    let sentence = text[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&sentence, MAX_SENTENCE)
}

/// Cuts a text to a number of characters and marks the cut.
pub fn cut(text: &str, limit: usize) -> String {
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
pub fn is_secret(word: &str) -> bool {
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
pub fn redact(text: &str) -> (String, bool) {
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
pub fn folder_path(folder: &str) -> PathBuf {
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
pub fn project_name(folder: &str, home: &Path) -> Option<String> {
    let path = folder_path(folder);
    if path == home || path == Path::new("/") {
        return None;
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// The section of a project entry a memory belongs in.
pub fn section_of(kind: &str) -> &'static str {
    match kind {
        "feedback" => "Conventions",
        "reference" => "References",
        _ => "State",
    }
}

/// One line of a project entry: the memory's name and its text on one line.
pub fn item(memory: &Memory, text: &str) -> String {
    format!(
        "- **{}:** {}",
        memory.name,
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// The Markdown body of a project entry from its sections.
pub fn project_body(sections: &BTreeMap<&str, Vec<String>>) -> String {
    let mut body = String::new();
    for name in ["State", "Conventions", "References", "Notes"] {
        if let Some(items) = sections.get(name) {
            body.push_str(&format!("## {name}\n\n{}\n\n", items.join("\n")));
        }
    }
    cut(body.trim(), MAX_BODY)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_body_has_its_sections_in_order() {
        let memory = Memory {
            name: "deploy".to_owned(),
            kind: "project".to_owned(),
            text: "On push.".to_owned(),
        };
        let mut sections = BTreeMap::new();
        sections
            .entry(section_of("feedback"))
            .or_insert_with(Vec::new)
            .push(item(&memory, "Run e2e."));
        sections
            .entry(section_of("project"))
            .or_insert_with(Vec::new)
            .push(item(&memory, "On\n push."));
        let body = project_body(&sections);
        assert!(
            body.starts_with("## State\n\n- **deploy:** On push."),
            "{body}"
        );
        assert!(body.contains("## Conventions"));
    }
}
