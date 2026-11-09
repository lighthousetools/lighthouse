//! Turns hook payloads into session events: tool calls, changed files, todo
//! lists and plans. Performs no IO.

use lighthouse_sessions::NewSessionEvent;
use serde_json::{Value, json};

const MAX_SUMMARY: usize = 300;
const MAX_DIFF: usize = 20_000;
const MAX_PLAN: usize = 20_000;
const FILE_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

fn lines(prefix: char, text: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}\n"))
        .collect()
}

/// Builds a diff from the input of a file tool.
pub fn diff_of(tool: &str, input: &Value) -> Option<String> {
    let text = |name: &str| input[name].as_str().unwrap_or_default();
    let diff = match tool {
        "Edit" => format!(
            "{}{}",
            lines('-', text("old_string")),
            lines('+', text("new_string"))
        ),
        "MultiEdit" => input["edits"]
            .as_array()?
            .iter()
            .map(|edit| {
                format!(
                    "@@\n{}{}",
                    lines('-', edit["old_string"].as_str().unwrap_or_default()),
                    lines('+', edit["new_string"].as_str().unwrap_or_default())
                )
            })
            .collect(),
        "Write" => lines('+', text("content")),
        "NotebookEdit" => lines('+', text("new_source")),
        _ => return None,
    };
    Some(clip(&diff, MAX_DIFF))
}

fn summary_of(tool: &str, input: &Value) -> Option<String> {
    let field = match tool {
        "Bash" => "command",
        "Read" | "Edit" | "MultiEdit" | "Write" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" => "description",
        _ => return None,
    };
    input[field].as_str().map(|text| clip(text, MAX_SUMMARY))
}

/// Builds the event for a hook call, if it has one.
pub fn event_of(hook: &Value) -> Option<NewSessionEvent> {
    let session_id = hook["session_id"].as_str()?.to_owned();
    let event =
        |kind: &str, tool: Option<&str>, summary: Option<String>, detail: Value| NewSessionEvent {
            session_id: session_id.clone(),
            kind: kind.to_owned(),
            tool: tool.map(str::to_owned),
            summary,
            file_path: None,
            detail,
            at: None,
        };
    match hook["hook_event_name"].as_str()? {
        "UserPromptSubmit" => {
            let prompt = hook["prompt"].as_str().map(|text| clip(text, MAX_SUMMARY));
            Some(event("prompt", None, prompt, json!({})))
        }
        "PreCompact" => {
            let trigger = hook["trigger"]
                .as_str()
                .map(|text| format!("compacted ({text})"));
            Some(event("compact", None, trigger, json!({})))
        }
        "Stop" => {
            let summary = hook["last_assistant_message"]
                .as_str()
                .map(|text| clip(text, MAX_SUMMARY));
            Some(event("stop", None, summary, json!({})))
        }
        "PostToolUse" => {
            let tool = hook["tool_name"].as_str()?;
            let input = &hook["tool_input"];
            let failed = hook["tool_response"]["is_error"].as_bool().unwrap_or(false)
                || hook["tool_response"]["success"].as_bool() == Some(false);
            let status = json!({"failed": failed});
            Some(match tool {
                "TodoWrite" => event(
                    "todos",
                    Some(tool),
                    None,
                    json!({"todos": input["todos"].clone()}),
                ),
                "ExitPlanMode" => event(
                    "plan",
                    Some(tool),
                    None,
                    json!({"plan": clip(input["plan"].as_str().unwrap_or_default(), MAX_PLAN)}),
                ),
                tool if FILE_TOOLS.contains(&tool) => {
                    let path = input["file_path"]
                        .as_str()
                        .or_else(|| input["notebook_path"].as_str())
                        .map(str::to_owned);
                    let mut file = event(
                        "file",
                        Some(tool),
                        summary_of(tool, input),
                        json!({"diff": diff_of(tool, input), "failed": failed}),
                    );
                    file.file_path = path;
                    file
                }
                tool => event("tool", Some(tool), summary_of(tool, input), status),
            })
        }
        _ => None,
    }
}

/// The answer to a `Stop` hook that hands messages over as the next instruction.
pub fn continue_with(messages: &[String]) -> Value {
    json!({
        "decision": "block",
        "reason": format!("The owner sent you this through Lighthouse:\n\n{}", messages.join("\n\n")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, input: Value) -> Value {
        json!({"session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": name,
               "tool_input": input, "tool_response": {}})
    }

    #[test]
    fn tools_become_timeline_entries_files_todos_and_plans() {
        let bash = event_of(&tool("Bash", json!({"command": "cargo test"}))).unwrap();
        assert_eq!(
            (bash.kind.as_str(), bash.summary.as_deref()),
            ("tool", Some("cargo test"))
        );
        let edit = event_of(&tool(
            "Edit",
            json!({"file_path": "src/a.rs", "old_string": "a\nb", "new_string": "c"}),
        ))
        .unwrap();
        assert_eq!(edit.kind, "file");
        assert_eq!(edit.file_path.as_deref(), Some("src/a.rs"));
        assert_eq!(edit.detail["diff"], "-a\n-b\n+c\n");
        let multi = diff_of(
            "MultiEdit",
            &json!({"edits": [{"old_string": "x", "new_string": "y"}]}),
        )
        .unwrap();
        assert_eq!(multi, "@@\n-x\n+y\n");
        assert_eq!(
            diff_of("Write", &json!({"content": "new"})).unwrap(),
            "+new\n"
        );
        assert_eq!(
            diff_of("NotebookEdit", &json!({"new_source": "n"})).unwrap(),
            "+n\n"
        );
        assert!(diff_of("Bash", &json!({})).is_none());
        let todos = event_of(&tool("TodoWrite", json!({"todos": [{"content": "Test"}]}))).unwrap();
        assert_eq!(todos.detail["todos"][0]["content"], "Test");
        let plan = event_of(&tool("ExitPlanMode", json!({"plan": "1. Do it"}))).unwrap();
        assert_eq!(plan.detail["plan"], "1. Do it");
        let other = event_of(&tool("mcp__x__y", json!({}))).unwrap();
        assert_eq!((other.kind.as_str(), other.summary), ("tool", None));
    }

    #[test]
    fn prompts_and_stops_are_recorded_and_others_are_not() {
        let long = "x".repeat(400);
        let prompt = event_of(
            &json!({"session_id": "s1", "hook_event_name": "UserPromptSubmit",
                                      "prompt": long}),
        )
        .unwrap();
        assert_eq!(prompt.summary.unwrap().chars().count(), MAX_SUMMARY + 1);
        let stop = event_of(&json!({"session_id": "s1", "hook_event_name": "Stop",
                                    "last_assistant_message": "Done"}))
        .unwrap();
        assert_eq!(stop.kind, "stop");
        assert!(
            event_of(&json!({"session_id": "s1", "hook_event_name": "SessionStart"})).is_none()
        );
        assert!(event_of(&json!({"hook_event_name": "Stop"})).is_none());
        let answer = continue_with(&["Run the tests".to_owned()]);
        assert_eq!(answer["decision"], "block");
        assert!(answer["reason"].as_str().unwrap().contains("Run the tests"));
    }
}
