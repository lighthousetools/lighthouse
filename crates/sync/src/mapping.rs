//! Turns tool results into items. Performs no IO.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use lighthouse_config::{CollectionConfig, FieldMap, FieldRule};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Most bytes of an item kept as raw data.
const MAX_RAW: usize = 64 * 1024;
/// States that count as closed or done.
pub const CLOSED_STATES: [&str; 10] = [
    "closed",
    "merged",
    "done",
    "completed",
    "canceled",
    "cancelled",
    "archived",
    "declined",
    "duplicate",
    "resolved",
];
/// States of a check that count as failed.
pub const FAILED_STATES: [&str; 5] = ["failure", "failed", "error", "timed_out", "action_required"];
/// Flags the cache sets from the fields of an item.
pub const DERIVED_FLAGS: [&str; 6] = [
    "important",
    "unread",
    "mine",
    "from_me",
    "mention",
    "failed",
];

/// Errors while reading a tool result.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MappingError {
    /// The result is not JSON.
    #[error("the result of `{0}` is not JSON")]
    NotJson(String),
    /// The configured path holds no list.
    #[error("`{path}` in the result of `{tool}` is not a list")]
    NotAList {
        /// Tool name.
        tool: String,
        /// Configured path.
        path: String,
    },
}

/// An item as the cache stores it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MappedItem {
    /// ID in the service.
    pub external_id: String,
    /// ID of the parent item in the service.
    pub parent_id: Option<String>,
    /// Title or subject.
    pub title: Option<String>,
    /// Snippet, description or body.
    pub summary: Option<String>,
    /// Author or sender.
    pub author: Option<String>,
    /// Assignee.
    pub assignee: Option<String>,
    /// State in the service.
    pub state: Option<String>,
    /// Address to open it.
    pub url: Option<String>,
    /// Flags, sorted.
    pub flags: Vec<String>,
    /// 0 to 100.
    pub importance: i16,
    /// When it last changed or arrived.
    pub occurred_at: Option<DateTime<Utc>>,
    /// Start, for events.
    pub starts_at: Option<DateTime<Utc>>,
    /// End, for events.
    pub ends_at: Option<DateTime<Utc>>,
    /// Every mapped field.
    pub data: Value,
    /// The item as the tool returned it, if it is small enough.
    pub raw: Option<Value>,
}

/// Who the owner is in a service, to set `mine`, `from_me` and `mention`.
#[derive(Clone, Debug, Default)]
pub struct Owner {
    /// Account names, such as a login or an email address.
    pub accounts: Vec<String>,
}

impl Owner {
    fn is(&self, value: &str) -> bool {
        let value = value.to_lowercase();
        self.accounts
            .iter()
            .filter(|account| !account.is_empty())
            .any(|account| value.contains(&account.to_lowercase()))
    }

    fn mentioned(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.accounts
            .iter()
            .filter(|account| !account.is_empty() && !account.contains('@'))
            .any(|account| text.contains(&format!("@{}", account.to_lowercase())))
    }
}

/// Reads a value by a dotted path. Numbers index lists.
pub fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.')
        .try_fold(value, |current, key| match current {
            Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
            other => other.get(key),
        })
}

/// Turns a value into text. Lists become comma-separated text.
pub fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(text_of).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        Value::Object(map) => ["name", "email", "login", "displayName", "title"]
            .iter()
            .find_map(|key| map.get(*key).and_then(text_of)),
        other => Some(other.to_string()),
    }
}

fn apply_rule(rule: &FieldRule, value: Option<&Value>) -> Option<Value> {
    if let Some(needle) = &rule.contains {
        let needle = needle.to_lowercase();
        let found = match value {
            Some(Value::Array(items)) => items.iter().any(|item| {
                text_of(item).is_some_and(|text| text.to_lowercase().contains(&needle))
            }),
            Some(value) => text_of(value).is_some_and(|text| text.to_lowercase().contains(&needle)),
            None => false,
        };
        return Some(Value::Bool(found));
    }
    if let Some(expected) = &rule.equals {
        let equal = value
            .and_then(text_of)
            .is_some_and(|text| text.eq_ignore_ascii_case(expected));
        return Some(Value::Bool(equal));
    }
    let mut text = text_of(value?)?;
    if let Some(marker) = &rule.after {
        text = text.split_once(marker.as_str())?.1.to_owned();
    }
    if let Some(marker) = &rule.after_last {
        text = text.rsplit_once(marker.as_str())?.1.to_owned();
    }
    if let Some(marker) = &rule.before {
        text = text
            .split_once(marker.as_str())
            .map_or(text.clone(), |(head, _)| head.to_owned());
    }
    Some(Value::String(text))
}

fn field_value(item: &Value, map: &FieldMap) -> Option<Value> {
    match map {
        FieldMap::Path(path) => lookup(item, path).filter(|value| !value.is_null()).cloned(),
        FieldMap::Rule(rule) => apply_rule(rule, lookup(item, &rule.path)),
    }
}

/// Reads a time from RFC 3339 text, a date, or Unix seconds or milliseconds.
pub fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .map(|time| time.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
                Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
            }),
        Value::Number(number) => {
            let value = number.as_i64()?;
            if value > 100_000_000_000 {
                Utc.timestamp_millis_opt(value).single()
            } else {
                Utc.timestamp_opt(value, 0).single()
            }
        }
        Value::Object(map) => map
            .get("dateTime")
            .or_else(|| map.get("date"))
            .and_then(parse_time),
        _ => None,
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(text.to_lowercase().as_str(), "true" | "yes" | "1"),
        Some(Value::Number(number)) => number.as_i64().is_some_and(|value| value != 0),
        _ => false,
    }
}

/// Parses a tool result and returns its list of items.
pub fn items_of(tool: &str, text: &str, path: &str) -> Result<Vec<Value>, MappingError> {
    let parsed: Value =
        serde_json::from_str(text.trim()).map_err(|_| MappingError::NotJson(tool.to_owned()))?;
    match lookup(&parsed, path) {
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::Object(_)) if path.is_empty() => Ok(vec![parsed]),
        _ => Err(MappingError::NotAList {
            tool: tool.to_owned(),
            path: path.to_owned(),
        }),
    }
}

/// Scores how much an item asks for the owner's attention, from 0 to 100.
pub fn importance(
    flags: &BTreeSet<String>,
    occurred_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> i16 {
    let has = |flag: &str| flags.contains(flag);
    let base = if has("review_requested") {
        80
    } else if has("failed") && has("mine") {
        75
    } else if has("assigned") || has("important") {
        70
    } else if has("mention") {
        60
    } else if has("unread") {
        40
    } else {
        10
    };
    let recent = occurred_at.is_some_and(|time| now - time < chrono::Duration::hours(24));
    base + if recent { 10 } else { 0 }
}

/// Maps one item of a collection. Returns `None` without an external ID.
pub fn map_item(
    collection: &CollectionConfig,
    item: &Value,
    owner: &Owner,
    now: DateTime<Utc>,
) -> Option<MappedItem> {
    let mut data = Map::new();
    for (target, map) in &collection.fields {
        if let Some(value) = field_value(item, map) {
            data.insert(target.clone(), value);
        }
    }
    let text = |name: &str| data.get(name).and_then(text_of);
    let external_id = text("external_id")?;
    let state = text("state");
    let mut flags: BTreeSet<String> = collection.flags.iter().cloned().collect();
    for flag in ["important", "unread"] {
        if truthy(data.get(flag)) {
            flags.insert(flag.to_owned());
        }
    }
    let author = text("author");
    if let Some(author) = &author
        && owner.is(author)
    {
        flags.insert("mine".to_owned());
        if matches!(collection.kind.as_str(), "thread" | "message") {
            flags.insert("from_me".to_owned());
        }
    }
    let words = format!(
        "{} {}",
        text("title").unwrap_or_default(),
        text("summary").unwrap_or_default()
    );
    if owner.mentioned(&words) {
        flags.insert("mention".to_owned());
    }
    if collection.kind == "check"
        && state
            .as_deref()
            .is_some_and(|state| FAILED_STATES.contains(&state.to_lowercase().as_str()))
    {
        flags.insert("failed".to_owned());
    }
    let occurred_at = data.get("occurred_at").and_then(parse_time);
    let raw = serde_json::to_string(item)
        .ok()
        .filter(|encoded| encoded.len() <= MAX_RAW)
        .map(|_| item.clone());
    Some(MappedItem {
        external_id,
        parent_id: text("parent_id"),
        title: text("title"),
        summary: text("summary"),
        author,
        assignee: text("assignee"),
        state,
        url: text("url"),
        importance: importance(&flags, occurred_at, now),
        flags: flags.into_iter().collect(),
        occurred_at,
        starts_at: data.get("starts_at").and_then(parse_time),
        ends_at: data.get("ends_at").and_then(parse_time),
        data: Value::Object(data),
        raw,
    })
}

/// Fills `{{path}}` placeholders from a context object, such as `{{since}}`,
/// `{{parent.external_id}}` or `{{input.body}}`. Unknown paths become empty.
pub fn render(template: &str, context: &Value) -> String {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            result.push_str(&rest[start..]);
            return result;
        };
        let key = after[..end].trim();
        result.push_str(&lookup(context, key).and_then(text_of).unwrap_or_default());
        rest = &after[end + 2..];
    }
    result.push_str(rest);
    result
}

/// Fills placeholders in every string of a value. A string that is only one
/// placeholder keeps the type of its value, so numbers stay numbers.
pub fn render_value(value: &Value, context: &Value) -> Value {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if let Some(key) = trimmed
                .strip_prefix("{{")
                .and_then(|rest| rest.strip_suffix("}}"))
                .filter(|key| !key.contains("{{"))
                && let Some(found) = lookup(context, key.trim())
                && !found.is_string()
            {
                return found.clone();
            }
            Value::String(render(text, context))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_value(item, context))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), render_value(item, context)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether a template uses a placeholder, such as `since`.
pub fn uses(value: &Value, placeholder: &str) -> bool {
    match value {
        Value::String(text) => text.contains(&format!("{{{{{placeholder}")),
        Value::Array(items) => items.iter().any(|item| uses(item, placeholder)),
        Value::Object(map) => map.values().any(|item| uses(item, placeholder)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn rule(path: &str) -> FieldRule {
        FieldRule {
            path: path.to_owned(),
            ..FieldRule::default()
        }
    }

    fn collection(kind: &str, fields: &[(&str, FieldMap)]) -> CollectionConfig {
        CollectionConfig {
            tool: "list".to_owned(),
            kind: kind.to_owned(),
            fields: fields
                .iter()
                .map(|(name, map)| ((*name).to_owned(), map.clone()))
                .collect::<BTreeMap<_, _>>(),
            ..CollectionConfig::default()
        }
    }

    fn path(text: &str) -> FieldMap {
        FieldMap::Path(text.to_owned())
    }

    #[test]
    fn paths_rules_and_flags_build_an_item() {
        let now = Utc::now();
        let pull = collection(
            "pull_request",
            &[
                ("external_id", path("number")),
                ("title", path("title")),
                ("author", path("user.login")),
                ("summary", path("body")),
                ("occurred_at", path("updated_at")),
                (
                    "repo",
                    FieldMap::Rule(FieldRule {
                        after: Some("/repos/".to_owned()),
                        ..rule("repository_url")
                    }),
                ),
                (
                    "owner",
                    FieldMap::Rule(FieldRule {
                        after: Some("/repos/".to_owned()),
                        before: Some("/".to_owned()),
                        ..rule("repository_url")
                    }),
                ),
                (
                    "name",
                    FieldMap::Rule(FieldRule {
                        after_last: Some("/".to_owned()),
                        ..rule("repository_url")
                    }),
                ),
                ("label", path("labels.0.name")),
            ],
        );
        let mut pull = pull;
        pull.flags = vec!["review_requested".to_owned()];
        let owner = Owner {
            accounts: vec!["alex".to_owned()],
        };
        let item = json!({"number": 12, "title": "Fix login", "user": {"login": "sam"},
            "body": "cc @Alex", "updated_at": now.to_rfc3339(),
            "repository_url": "https://api.example.com/repos/acme/app",
            "labels": [{"name": "bug"}]});
        let mapped = map_item(&pull, &item, &owner, now).unwrap();
        assert_eq!(mapped.external_id, "12");
        assert_eq!(mapped.data["repo"], "acme/app");
        assert_eq!(mapped.data["owner"], "acme");
        assert_eq!(mapped.data["name"], "app");
        assert_eq!(mapped.data["label"], "bug");
        assert_eq!(mapped.flags, ["mention", "review_requested"]);
        assert_eq!(mapped.importance, 90);
        assert!(mapped.raw.is_some());
        assert!(map_item(&pull, &json!({"title": "no id"}), &owner, now).is_none());
    }

    #[test]
    fn mail_checks_and_owner_flags() {
        let now = Utc::now();
        let mail = collection(
            "thread",
            &[
                ("external_id", path("id")),
                ("author", path("from")),
                (
                    "important",
                    FieldMap::Rule(FieldRule {
                        contains: Some("important".to_owned()),
                        ..rule("labels")
                    }),
                ),
                (
                    "unread",
                    FieldMap::Rule(FieldRule {
                        equals: Some("UNREAD".to_owned()),
                        ..rule("status")
                    }),
                ),
                ("occurred_at", path("date")),
            ],
        );
        let owner = Owner {
            accounts: vec!["alex@example.com".to_owned()],
        };
        let item = json!({"id": "t1", "from": "Alex <alex@example.com>", "labels": ["INBOX", "IMPORTANT"],
                          "status": "unread", "date": "2020-01-01"});
        let mapped = map_item(&mail, &item, &owner, now).unwrap();
        assert_eq!(mapped.flags, ["from_me", "important", "mine", "unread"]);
        assert_eq!(mapped.importance, 70);
        let check = collection(
            "check",
            &[("external_id", path("name")), ("state", path("conclusion"))],
        );
        let failed = map_item(
            &check,
            &json!({"name": "ci", "conclusion": "FAILURE"}),
            &owner,
            now,
        )
        .unwrap();
        assert_eq!(failed.flags, ["failed"]);
    }

    #[test]
    fn times_lists_and_texts() {
        assert!(parse_time(&json!("2026-11-06T08:00:00+01:00")).is_some());
        assert!(parse_time(&json!("2026-11-06")).is_some());
        assert_eq!(parse_time(&json!(0)).unwrap().timestamp(), 0);
        assert_eq!(
            parse_time(&json!(1_700_000_000_000_i64))
                .unwrap()
                .timestamp(),
            1_700_000_000
        );
        assert!(parse_time(&json!({"dateTime": "2026-11-06T08:00:00Z"})).is_some());
        assert!(parse_time(&json!("soon")).is_none());
        assert!(parse_time(&json!(true)).is_none());
        assert_eq!(
            text_of(&json!([{"email": "a@x"}, "b"])).as_deref(),
            Some("a@x, b")
        );
        assert_eq!(text_of(&json!(null)), None);
        assert_eq!(text_of(&json!(3)).as_deref(), Some("3"));

        assert_eq!(items_of("t", "[1, 2]", "").unwrap().len(), 2);
        assert_eq!(
            items_of("t", r#"{"a": {"b": [1]}}"#, "a.b").unwrap().len(),
            1
        );
        assert_eq!(items_of("t", r#"{"id": 1}"#, "").unwrap().len(), 1);
        assert!(matches!(
            items_of("t", "nope", ""),
            Err(MappingError::NotJson(_))
        ));
        assert!(matches!(
            items_of("t", r#"{"a": 1}"#, "a"),
            Err(MappingError::NotAList { .. })
        ));
        assert!(truthy(Some(&json!("yes"))) && truthy(Some(&json!(1))) && !truthy(None));
    }

    #[test]
    fn templates_fill_placeholders_and_keep_types() {
        let context = json!({"since": "2026-01-01", "parent": {"number": 7, "repo": "acme/app"}});
        assert_eq!(
            render("after:{{since}} {{ parent.repo }}{{nope}}", &context),
            "after:2026-01-01 acme/app"
        );
        assert_eq!(render("broken {{since", &context), "broken {{since");
        let input = json!({"pullNumber": "{{parent.number}}", "q": "repo:{{parent.repo}}", "list": ["{{since}}"], "n": 1});
        assert_eq!(
            render_value(&input, &context),
            json!({"pullNumber": 7, "q": "repo:acme/app", "list": ["2026-01-01"], "n": 1})
        );
        assert!(uses(&input, "since"));
        assert!(!uses(&json!({"q": "x"}), "since"));
        assert_eq!(importance(&BTreeSet::new(), None, Utc::now()), 10);
    }
}
