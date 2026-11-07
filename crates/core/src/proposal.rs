//! What an agent may add to a call that proposes something to the owner.
//!
//! A write or draft call can carry two extra arguments: `_reason`, one sentence
//! on why the owner should want it, and `_context`, the item it answers, such
//! as the external ID of a mail thread. Lighthouse keeps both for the owner's
//! review and removes them before the tool sees the arguments.

use serde_json::{Map, Value, json};

/// Why the owner should want the call.
pub const REASON: &str = "_reason";
/// The item the call answers.
pub const CONTEXT: &str = "_context";

/// The extra arguments as JSON Schema properties.
pub fn schema_properties() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        REASON.to_owned(),
        json!({"type": "string", "description": "One sentence for the owner: why this is worth approving."}),
    );
    properties.insert(
        CONTEXT.to_owned(),
        json!({"type": "string", "description": "ID of the item this answers, such as a mail thread or an issue."}),
    );
    properties
}

/// Adds the extra arguments to a tool's input schema.
pub fn extend_schema(schema: &mut Value) {
    if let Some(object) = schema.as_object_mut() {
        let properties = object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(properties) = properties.as_object_mut() {
            for (key, value) in schema_properties() {
                properties.entry(key).or_insert(value);
            }
        }
    }
}

/// The reason of a call, if it has one.
pub fn reason(input: &Value) -> Option<String> {
    text(input, REASON)
}

/// The item a call answers, if it names one.
pub fn context(input: &Value) -> Option<String> {
    text(input, CONTEXT)
}

fn text(input: &Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Arguments that name what a call acts on, in order of preference.
const TARGET_KEYS: [&str; 12] = [
    "thread_id",
    "threadId",
    "issue_id",
    "issueId",
    "issue_number",
    "pullNumber",
    "pull_number",
    "event_id",
    "eventId",
    "post_id",
    "id",
    "to",
];

/// What a call acts on, such as a mail thread or an issue: its `_context`, else
/// the first argument that names a target. Two open proposals of the same tool
/// for the same target are equivalent.
pub fn target(input: &Value) -> Option<String> {
    context(input).or_else(|| {
        TARGET_KEYS.iter().find_map(|key| match &input[*key] {
            Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    })
}

/// The arguments without the extras, as the tool gets them.
pub fn strip(mut input: Value) -> Value {
    if let Some(object) = input.as_object_mut() {
        object.remove(REASON);
        object.remove(CONTEXT);
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_are_read_and_removed() {
        let input = json!({"body": "Hi", "_reason": " John asked ", "_context": "t1"});
        assert_eq!(reason(&input).as_deref(), Some("John asked"));
        assert_eq!(context(&input).as_deref(), Some("t1"));
        assert_eq!(strip(input), json!({"body": "Hi"}));
        assert_eq!(reason(&json!({"_reason": ""})), None);
        assert_eq!(strip(json!("text")), json!("text"));
    }

    #[test]
    fn targets_come_from_the_context_or_a_named_argument() {
        assert_eq!(
            target(&json!({"_context": "t1", "thread_id": "t2"})).as_deref(),
            Some("t1")
        );
        assert_eq!(target(&json!({"thread_id": " t2 "})).as_deref(), Some("t2"));
        assert_eq!(target(&json!({"pullNumber": 42})).as_deref(), Some("42"));
        assert_eq!(target(&json!({"body": "Hi", "id": ""})), None);
    }

    #[test]
    fn schemas_get_the_extras() {
        let mut schema = json!({"type": "object", "properties": {"body": {"type": "string"}}});
        extend_schema(&mut schema);
        assert!(schema["properties"]["_reason"].is_object());
        assert!(schema["properties"]["body"].is_object());
        let mut bare = json!({"type": "object"});
        extend_schema(&mut bare);
        assert!(bare["properties"]["_context"].is_object());
    }
}
