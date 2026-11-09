//! Reads what a session says to its owner between tool calls.
//!
//! Hooks only carry the last answer at a stop. The texts Claude writes while it works sit in the session's
//! transcript, a JSON Lines file under `~/.claude/projects`. This reads the new lines since the last call and keeps
//! only the text blocks of the main session's assistant messages: no tool input or output, no thinking and nothing of
//! subagents. A session is read from the moment the agent first sees it, never its earlier history.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// The longest text sent for one message.
const MAX_TEXT: usize = 20_000;
/// The most read at once, so a huge jump cannot stall a hook.
const MAX_READ: u64 = 4 * 1024 * 1024;

/// Where each session's transcript was read up to.
#[derive(Debug, Default)]
pub struct Replies {
    offsets: Mutex<HashMap<String, u64>>,
}

/// Only transcripts of Claude Code sessions are read.
fn is_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
        && path
            .components()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair[0].as_os_str() == ".claude" && pair[1].as_os_str() == "projects")
}

/// The text blocks of one transcript line, if it is an assistant message of the main session.
fn texts_of(line: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if value["type"] != "assistant" || value["isSidechain"].as_bool().unwrap_or(false) {
        return Vec::new();
    }
    value["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(MAX_TEXT).collect())
        .collect()
}

impl Replies {
    /// Marks the current end of a transcript as read, so its history is not sent.
    pub fn start(&self, session: &str, path: &Path) {
        if !is_transcript(path) {
            return;
        }
        let end = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        self.offsets
            .lock()
            .expect("offsets lock")
            .insert(session.to_owned(), end);
    }

    /// Returns the texts written since the last call. A session seen for the first time starts at the current end.
    pub fn read_new(&self, session: &str, path: &Path) -> Vec<String> {
        if !is_transcript(path) {
            return Vec::new();
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let end = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        let mut offsets = self.offsets.lock().expect("offsets lock");
        let Some(offset) = offsets.get(session).copied() else {
            offsets.insert(session.to_owned(), end);
            return Vec::new();
        };
        // A rewritten or truncated file starts over at its end.
        if end < offset {
            offsets.insert(session.to_owned(), end);
            return Vec::new();
        }
        let mut chunk = Vec::new();
        if file.seek(SeekFrom::Start(offset)).is_err()
            || file.take(MAX_READ).read_to_end(&mut chunk).is_err()
        {
            return Vec::new();
        }
        // Only complete lines; a line still being written is read next time.
        let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        offsets.insert(session.to_owned(), offset + last as u64 + 1);
        drop(offsets);
        String::from_utf8_lossy(&chunk[..=last])
            .lines()
            .flat_map(texts_of)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(kind: &str, sidechain: bool, blocks: &str) -> String {
        format!(r#"{{"type":"{kind}","isSidechain":{sidechain},"message":{{"content":{blocks}}}}}"#)
            + "\n"
    }

    #[test]
    fn only_new_texts_of_the_main_session_are_read() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/-work-app");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            line(
                "assistant",
                false,
                r#"[{"type":"text","text":"Old history"}]"#,
            )
            .as_bytes(),
        )
        .unwrap();
        let replies = Replies::default();
        replies.start("s1", &path);
        file.write_all(
            (line("assistant", false, r#"[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Checking the tests now."},{"type":"tool_use","name":"Bash","input":{}}]"#)
                + &line("user", false, r#"[{"type":"tool_result","content":"secret output"}]"#)
                + &line("assistant", true, r#"[{"type":"text","text":"Subagent talk"}]"#)
                + "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"half")
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(replies.read_new("s1", &path), ["Checking the tests now."]);
        file.write_all(b" a line\"}]}}\n").unwrap();
        assert_eq!(replies.read_new("s1", &path), ["half a line"]);
        assert!(replies.read_new("s1", &path).is_empty());
    }

    #[test]
    fn unknown_sessions_start_at_the_end_and_other_files_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/x");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s2.jsonl");
        std::fs::write(
            &path,
            line("assistant", false, r#"[{"type":"text","text":"Before"}]"#),
        )
        .unwrap();
        let replies = Replies::default();
        assert!(replies.read_new("s2", &path).is_empty());
        let elsewhere = home.path().join("notes.jsonl");
        std::fs::write(
            &elsewhere,
            line("assistant", false, r#"[{"type":"text","text":"No"}]"#),
        )
        .unwrap();
        replies.start("s3", &elsewhere);
        assert!(replies.read_new("s3", &elsewhere).is_empty());
        std::fs::write(&path, "").unwrap();
        assert!(
            replies.read_new("s2", &path).is_empty(),
            "a truncated file starts over"
        );
    }
}
