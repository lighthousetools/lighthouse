//! Adds and removes hooks in the Claude Code settings.
//!
//! Other hooks and settings stay untouched. The hooks are recognized by their
//! URL, so removing them finds exactly what was installed.

use serde_json::{Value, json};

const EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "Stop",
    "SessionEnd",
    "PostToolUse",
];
/// Claude Code waits at most this long for an HTTP hook.
const PERMISSION_TIMEOUT: u64 = 600;
const EVENT_TIMEOUT: u64 = 5;

fn is_ours(group: &Value, url: &str) -> bool {
    group["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|hook| hook["url"] == url))
}

/// Removes our hooks. Returns whether anything changed.
pub fn remove(settings: &mut Value, url: &str) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        let before = groups.len();
        groups.retain(|group| !is_ours(group, url));
        changed |= groups.len() != before;
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|list| !list.is_empty()));
    if hooks.is_empty()
        && let Some(object) = settings.as_object_mut()
    {
        object.remove("hooks");
    }
    changed
}

/// Adds our hooks once, replacing an older installation.
pub fn install(settings: &mut Value, url: &str) {
    remove(settings, url);
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .expect("settings are an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    for event in EVENTS {
        let timeout = if event == "PermissionRequest" {
            PERMISSION_TIMEOUT
        } else {
            EVENT_TIMEOUT
        };
        let group = json!({"matcher": "*", "hooks": [
            {"type": "http", "url": url, "timeout": timeout}
        ]});
        let groups = hooks
            .as_object_mut()
            .expect("hooks are an object")
            .entry(event)
            .or_insert_with(|| json!([]));
        if let Some(list) = groups.as_array_mut() {
            list.push(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:47800/hook";

    #[test]
    fn installing_twice_adds_one_set_and_keeps_other_hooks() {
        let mine = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}]});
        let mut settings = json!({"model": "m", "hooks": {"Stop": [mine.clone()]}});
        install(&mut settings, URL);
        install(&mut settings, URL);
        assert_eq!(settings["model"], "m");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0], mine);
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            600
        );
        assert_eq!(settings["hooks"].as_object().unwrap().len(), EVENTS.len());
    }

    #[test]
    fn removing_restores_the_settings() {
        let original = json!({"model": "m", "hooks": {"Stop": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}]}
        ]}});
        let mut settings = original.clone();
        install(&mut settings, URL);
        assert!(remove(&mut settings, URL));
        assert_eq!(settings, original);
        assert!(!remove(&mut settings, URL));
    }

    #[test]
    fn empty_settings_stay_clean() {
        let mut settings = Value::Null;
        install(&mut settings, URL);
        assert!(remove(&mut settings, URL));
        assert_eq!(settings, json!({}));
    }
}
