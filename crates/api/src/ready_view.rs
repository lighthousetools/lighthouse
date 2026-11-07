//! Turns proposed tool calls into something the owner can review: who it is
//! for, which service it goes to, why the agent wants it, the text and the
//! item it answers.

use crate::connected::SourceInfo;
use chrono::{DateTime, Utc};
use lighthouse_core::proposal;
use lighthouse_sync::{SyncStore, SyncedItem};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

/// Argument names that hold the text to review, in order of preference.
const TEXT_KEYS: [&str; 8] = [
    "body",
    "text",
    "message",
    "comment",
    "content",
    "description",
    "note",
    "reply",
];
/// Argument names that make a good one-line title.
const TITLE_KEYS: [&str; 4] = ["subject", "title", "summary", "name"];
/// Argument names that name the person something goes to.
const PERSON_KEYS: [&str; 5] = ["to", "recipient", "attendee", "assignee", "reviewer"];
const MAX_TITLE: usize = 90;

/// The person something is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Counterpart {
    /// Display name.
    pub name: String,
    /// Mail address or handle, if known.
    pub address: Option<String>,
    /// Their organization, if known.
    pub org: Option<String>,
}

/// One argument, readable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PreviewField {
    /// Argument name.
    pub key: String,
    /// Readable label, such as `To` or `Subject`.
    pub label: String,
    /// The value as text.
    pub value: String,
}

/// The arguments of a call, readable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Preview {
    /// The arguments apart from the text and internal references.
    pub fields: Vec<PreviewField>,
    /// The text to review, such as the body of a mail.
    pub text: Option<String>,
    /// The argument that holds the text, for edits.
    pub text_key: Option<String>,
}

/// The item a call answers, to quote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Quote {
    /// Who wrote it.
    pub author: Option<String>,
    /// Its title, such as the subject.
    pub title: Option<String>,
    /// Its text or snippet.
    pub text: Option<String>,
    /// When it happened.
    pub occurred_at: Option<DateTime<Utc>>,
    /// Link to it.
    pub url: Option<String>,
}

/// What the owner sees of a proposed call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Review {
    /// One line for the list.
    pub summary: Option<String>,
    /// Why the agent proposes it.
    pub reason: Option<String>,
    /// The service it goes to, such as `gmail`, `linear` or `linkedin`.
    pub service: Option<String>,
    /// The person it is for.
    pub counterpart: Option<Counterpart>,
    /// The connected item it answers.
    pub context_item_id: Option<Uuid>,
    /// The item it answers, quoted.
    pub quote: Option<Quote>,
    /// The arguments, readable.
    pub preview: Option<Preview>,
}

/// Turns a key such as `pull_number` or `issueId` into `Pull number` or `Issue id`.
pub fn label(key: &str) -> String {
    let mut words = String::new();
    for (index, character) in key.chars().enumerate() {
        if character == '_' || character == '-' {
            words.push(' ');
        } else if character.is_uppercase() && index > 0 {
            words.push(' ');
            words.extend(character.to_lowercase());
        } else {
            words.push(character);
        }
    }
    let mut characters = words.trim().chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()).filter(|text| !text.trim().is_empty()),
        Value::Array(values) => {
            let parts: Vec<String> = values.iter().filter_map(as_text).collect();
            Some(parts.join(", ")).filter(|text| !text.is_empty())
        }
        other => Some(other.to_string()),
    }
}

/// The input name a template fills an argument from, such as `body` for `{{input.body}}`.
fn template_input(template: Option<&Value>, key: &str) -> Option<String> {
    let text = template?.get(key)?.as_str()?.trim();
    let name = text.strip_prefix("{{input.")?.strip_suffix("}}")?;
    Some(name.trim().to_owned())
}

fn refers_to_item(template: Option<&Value>, key: &str) -> bool {
    template
        .and_then(|template| template.get(key))
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("{{item."))
}

/// The argument that holds the text to review.
pub fn text_key(input: &Value, template: Option<&Value>) -> Option<String> {
    let object = input.as_object()?;
    let named = |key: &str| {
        let name = template_input(template, key).unwrap_or_else(|| key.to_owned());
        TEXT_KEYS.contains(&name.as_str())
    };
    TEXT_KEYS
        .iter()
        .find(|key| object.get(**key).is_some_and(Value::is_string))
        .map(|key| (*key).to_owned())
        .or_else(|| {
            object
                .iter()
                .find(|(key, value)| value.is_string() && named(key))
                .map(|(key, _)| key.clone())
        })
}

/// The arguments of a call, readable. `template` is the argument template of
/// the source action behind the tool, if there is one.
pub fn preview(input: &Value, template: Option<&Value>) -> Preview {
    let Some(object) = input.as_object() else {
        return Preview {
            text: as_text(input),
            ..Preview::default()
        };
    };
    let text_key = text_key(input, template);
    let fields = object
        .iter()
        .filter(|(key, _)| !key.starts_with('_'))
        .filter(|(key, _)| Some(key.as_str()) != text_key.as_deref())
        .filter(|(key, _)| !refers_to_item(template, key))
        .filter_map(|(key, value)| {
            let name = template_input(template, key).unwrap_or_else(|| key.clone());
            as_text(value).map(|value| PreviewField {
                key: key.clone(),
                label: label(&name),
                value,
            })
        })
        .collect();
    Preview {
        fields,
        text: text_key.as_ref().and_then(|key| as_text(&object[key])),
        text_key,
    }
}

/// Splits `Jane Doe <jane@acme.example>` into a name and an address.
pub fn person(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let (Some(open), Some(close)) = (text.find('<'), text.rfind('>'))
        && open < close
    {
        let address = text[open + 1..close].trim().to_owned();
        let name = text[..open].trim().trim_matches('"').trim();
        let name = if name.is_empty() { &address } else { name };
        return Some((name.to_owned(), Some(address)));
    }
    if text.contains('@') && !text.contains(' ') {
        return Some((text.to_owned(), Some(text.to_owned())));
    }
    Some((text.to_owned(), None))
}

/// Guesses an organization from a mail address, such as `Globex` for
/// `john@globex.example`.
pub fn org_of(address: &str) -> Option<String> {
    let domain = address.rsplit_once('@')?.1;
    let first = domain.split('.').next()?;
    let mut characters = first.chars();
    characters
        .next()
        .map(|initial| initial.to_uppercase().chain(characters).collect())
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    if line.chars().count() > MAX_TITLE {
        let cut: String = line.chars().take(MAX_TITLE - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_owned()
    }
}

/// Builds reviews, with the sources and organizations loaded once.
pub struct Reviewer<'a> {
    sync: &'a SyncStore,
    sources: Vec<SourceInfo>,
    orgs: HashMap<String, String>,
    post_service: Option<String>,
}

impl<'a> Reviewer<'a> {
    /// A reviewer over the given sources. `orgs` maps a source to the name of its organization.
    pub fn new(
        sync: &'a SyncStore,
        sources: Vec<SourceInfo>,
        orgs: HashMap<String, String>,
        post_service: Option<String>,
    ) -> Self {
        Self {
            sync,
            sources,
            orgs,
            post_service,
        }
    }

    /// The source and action template behind a tool, if a source maps it.
    fn action(&self, tool: &str) -> Option<(&SourceInfo, Option<&Value>)> {
        self.sources.iter().find_map(|source| {
            source
                .action_tools
                .iter()
                .find(|action| action.tool == tool)
                .map(|action| (source, action.input.as_ref()))
        })
    }

    async fn context_item(
        &self,
        input: &Value,
        source: Option<&SourceInfo>,
        template: Option<&Value>,
    ) -> Option<SyncedItem> {
        let mut references: Vec<String> = proposal::context(input).into_iter().collect();
        if let (Some(template), Some(object)) = (template, input.as_object()) {
            for (key, value) in object {
                let refers = template
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("{{item.external_id}}"));
                if refers && let Some(text) = as_text(value) {
                    references.push(text);
                }
            }
        }
        for reference in references {
            let found = match Uuid::parse_str(&reference) {
                Ok(id) => self.sync.item(id).await,
                Err(_) => {
                    let name = source.map(|source| source.name.as_str());
                    match self.sync.find_external(&reference, name).await {
                        Ok(None) if name.is_some() => {
                            self.sync.find_external(&reference, None).await
                        }
                        other => other,
                    }
                }
            };
            match found {
                Ok(Some(item)) => return Some(item),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "cannot look up the item a call answers"),
            }
        }
        None
    }

    /// Reviews a proposed or prepared call.
    pub async fn call(&self, tool: &str, input: &Value, said: Option<&str>) -> Review {
        let (source, template) = match self.action(tool) {
            Some((source, template)) => (Some(source), template),
            None => (None, None),
        };
        let item = self.context_item(input, source, template).await;
        let preview = preview(input, template);
        let service = source
            .map(|source| source.service.clone())
            .or_else(|| {
                item.as_ref().and_then(|item| {
                    self.sources
                        .iter()
                        .find(|source| source.name == item.source)
                        .map(|source| source.service.clone())
                })
            })
            .or_else(|| tool.split('.').next().map(str::to_owned));
        let org = item
            .as_ref()
            .and_then(|item| self.orgs.get(&item.source).cloned());
        let named = PERSON_KEYS
            .iter()
            .find_map(|key| input.get(*key).and_then(as_text));
        let counterpart = item
            .as_ref()
            .and_then(|item| item.author.clone())
            .or(named)
            .and_then(|text| person(&text))
            .map(|(name, address)| Counterpart {
                org: org.clone().or_else(|| address.as_deref().and_then(org_of)),
                name,
                address,
            });
        let summary = TITLE_KEYS
            .iter()
            .find_map(|key| input.get(*key).and_then(as_text))
            .or_else(|| {
                item.as_ref()
                    .and_then(|item| item.title.as_ref())
                    .map(|title| format!("Re: {title}"))
            })
            .or_else(|| preview.text.as_deref().map(first_line))
            .map(|text| first_line(&text));
        Review {
            summary,
            reason: proposal::reason(input).or_else(|| said.map(first_line)),
            service,
            counterpart,
            context_item_id: item.as_ref().map(|item| item.id),
            quote: item.map(|item| Quote {
                author: item.author,
                title: item.title,
                text: item.summary,
                occurred_at: item.occurred_at,
                url: item.url,
            }),
            preview: Some(preview),
        }
    }

    /// The service posts go to.
    pub fn post_service(&self, channel: &str) -> Option<String> {
        Some(channel.to_owned())
            .filter(|channel| !channel.is_empty())
            .or_else(|| self.post_service.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_read_like_words() {
        assert_eq!(label("pull_number"), "Pull number");
        assert_eq!(label("issueId"), "Issue id");
        assert_eq!(label("to"), "To");
        assert_eq!(label(""), "");
    }

    #[test]
    fn previews_name_the_fields_and_lift_the_text() {
        let template = json!({"thread_id": "{{item.external_id}}", "message": "{{input.body}}"});
        let input = json!({"thread_id": "t1", "message": "Hello", "to": ["a@x.example", "b@x.example"],
                           "_reason": "asked", "cc": null, "count": 2});
        let preview = preview(&input, Some(&template));
        assert_eq!(preview.text.as_deref(), Some("Hello"));
        assert_eq!(preview.text_key.as_deref(), Some("message"));
        let labels: Vec<_> = preview
            .fields
            .iter()
            .map(|field| (field.label.as_str(), field.value.as_str()))
            .collect();
        assert_eq!(labels, [("Count", "2"), ("To", "a@x.example, b@x.example")]);
        let bare = super::preview(&json!("just text"), None);
        assert_eq!(bare.text.as_deref(), Some("just text"));
    }

    #[test]
    fn people_and_orgs_come_from_addresses() {
        assert_eq!(
            person("\"Jane Doe\" <jane@acme.example>"),
            Some(("Jane Doe".to_owned(), Some("jane@acme.example".to_owned())))
        );
        assert_eq!(
            person("<jane@acme.example>").unwrap().0,
            "jane@acme.example"
        );
        assert_eq!(
            person("john@globex.example").unwrap().1.as_deref(),
            Some("john@globex.example")
        );
        assert_eq!(person("jane-doe"), Some(("jane-doe".to_owned(), None)));
        assert_eq!(person("  "), None);
        assert_eq!(org_of("john@globex.example").as_deref(), Some("Globex"));
        assert_eq!(org_of("nobody"), None);
    }

    #[test]
    fn long_lines_are_cut() {
        let line = first_line(&format!("\n{}", "word ".repeat(40)));
        assert!(line.ends_with('…') && line.chars().count() <= MAX_TITLE);
    }
}
