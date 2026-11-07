//! Entries as Markdown files with frontmatter.

use crate::BrainError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Kinds of entries.
pub const KINDS: [&str; 8] = [
    "decision", "customer", "project", "person", "meeting", "note", "file", "deck",
];

/// Where an entry comes from. Every entry needs at least one source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Source {
    /// Kind of source, for example `app`, `meeting` or `agent`.
    pub kind: String,
    /// Reference inside that source.
    #[serde(rename = "ref")]
    pub reference: String,
}

/// An entry of the Brain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Entry {
    /// Entry ID.
    pub id: Uuid,
    /// One of [`KINDS`].
    #[serde(rename = "type")]
    pub kind: String,
    /// Title.
    pub title: String,
    /// Who the entry belongs to.
    pub scope: String,
    /// `active`, `superseded` or `archived`.
    pub status: String,
    /// Sources.
    pub sources: Vec<Source>,
    /// Links to other entries, such as `project:release-2-14`.
    #[serde(default)]
    pub links: Vec<String>,
    /// File name of the original, for uploaded files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
    /// When it was created.
    pub created: DateTime<Utc>,
    /// When it last changed.
    pub updated: DateTime<Utc>,
    /// Markdown text.
    #[serde(skip)]
    pub body: String,
}

impl Entry {
    /// Renders the file content.
    pub fn render(&self) -> Result<String, BrainError> {
        let frontmatter =
            serde_saphyr::to_string(self).map_err(|error| BrainError::Format(error.to_string()))?;
        let frontmatter = frontmatter.trim_end();
        Ok(format!(
            "---\n{frontmatter}\n---\n{}\n",
            self.body.trim_end()
        ))
    }

    /// Parses the file content.
    pub fn parse(text: &str) -> Result<Self, BrainError> {
        let missing = || BrainError::Format("the frontmatter is missing".to_owned());
        let rest = text.strip_prefix("---\n").ok_or_else(missing)?;
        let (frontmatter, body) = rest.split_once("\n---\n").ok_or_else(missing)?;
        let mut entry: Self = serde_saphyr::from_str(frontmatter)
            .map_err(|error| BrainError::Format(error.to_string()))?;
        body.trim_end().clone_into(&mut entry.body);
        Ok(entry)
    }
}

const MAX_CHUNK: usize = 1200;

/// Splits text at paragraphs into chunks of limited size.
pub fn chunks(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if !current.is_empty() && current.len() + paragraph.len() > MAX_CHUNK {
            result.push(std::mem::take(&mut current));
        }
        if paragraph.len() > MAX_CHUNK {
            let mut part = String::new();
            for word in paragraph.split_whitespace() {
                if part.len() + word.len() > MAX_CHUNK {
                    result.push(std::mem::take(&mut part));
                }
                if !part.is_empty() {
                    part.push(' ');
                }
                part.push_str(word);
            }
            current = part;
            continue;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        let now = Utc::now();
        Entry {
            id: Uuid::now_v7(),
            kind: "decision".to_owned(),
            title: "Title: with \"quotes\"".to_owned(),
            scope: "personal".to_owned(),
            status: "active".to_owned(),
            sources: vec![Source {
                kind: "meeting".to_owned(),
                reference: "weekly".to_owned(),
            }],
            links: vec!["project:x".to_owned()],
            original: None,
            created: now,
            updated: now,
            body: "Decided.\n\nReason: law.".to_owned(),
        }
    }

    #[test]
    fn entries_survive_a_round_trip() {
        let entry = entry();
        let text = entry.render().unwrap();
        assert!(text.contains("type: decision"), "{text}");
        assert!(text.contains("ref: weekly"), "{text}");
        assert_eq!(Entry::parse(&text).unwrap(), entry);
    }

    #[test]
    fn a_body_with_a_separator_stays_intact() {
        let mut entry = entry();
        entry.body = "Before\n---\nAfter".to_owned();
        let parsed = Entry::parse(&entry.render().unwrap()).unwrap();
        assert_eq!(parsed.body, entry.body);
    }

    #[test]
    fn files_without_frontmatter_are_errors() {
        assert!(Entry::parse("just text").is_err());
        assert!(Entry::parse("---\nid: x").is_err());
    }

    #[test]
    fn chunks_split_at_paragraphs_and_inside_long_ones() {
        assert!(chunks("  \n\n ").is_empty());
        assert_eq!(chunks("a\n\nb"), ["a\n\nb"]);
        let long = "word ".repeat(600);
        let parts = chunks(&format!("{long}\n\nshort"));
        assert!(parts.len() >= 2);
        assert!(parts.iter().all(|part| part.len() <= MAX_CHUNK));
        assert!(parts.last().unwrap().ends_with("short"));
    }
}
