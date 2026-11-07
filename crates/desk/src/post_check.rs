//! Checks a post before it goes out: length, hook and hashtags. Performs no IO.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Most characters a post may have.
pub const MAX_LENGTH: usize = 3000;
/// Posts shorter than this read like a status update.
pub const SHORT_LENGTH: usize = 300;
/// Longest hook that still fits before the fold.
pub const MAX_HOOK: usize = 150;
/// Most hashtags before a post looks like spam.
pub const MAX_HASHTAGS: usize = 5;

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CheckIssue {
    /// Stable code, such as `too_long` or `hook_too_long`.
    pub code: String,
    /// `error`, `warning` or `hint`.
    pub severity: String,
    /// What to change.
    pub message: String,
}

/// The result of the check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PostCheck {
    /// Characters.
    pub characters: usize,
    /// Words.
    pub words: usize,
    /// The first line, which readers see first.
    pub hook: String,
    /// Hashtags in order of appearance.
    pub hashtags: Vec<String>,
    /// Findings, errors first.
    pub issues: Vec<CheckIssue>,
    /// Whether nothing blocks publishing.
    pub ok: bool,
}

fn issue(code: &str, severity: &str, message: String) -> CheckIssue {
    CheckIssue {
        code: code.to_owned(),
        severity: severity.to_owned(),
        message,
    }
}

/// Finds hashtags such as `#rust` or `#SmallBusiness`.
pub fn hashtags(text: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        let starts_word = index == 0 || chars[index - 1].is_whitespace();
        if *c != '#' || !starts_word {
            continue;
        }
        let tag: String = chars[index + 1..]
            .iter()
            .take_while(|c| c.is_alphanumeric() || **c == '_')
            .collect();
        if !tag.is_empty() && !tag.chars().all(|c| c.is_ascii_digit()) {
            tags.push(format!("#{tag}"));
        }
    }
    tags
}

/// Checks the text of a post.
pub fn check_post(text: &str) -> PostCheck {
    let characters = text.chars().count();
    let words = text.split_whitespace().count();
    let hook = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned();
    let tags = hashtags(text);
    let mut issues = Vec::new();
    if text.trim().is_empty() {
        issues.push(issue("empty", "error", "The post has no text.".to_owned()));
    }
    if characters > MAX_LENGTH {
        issues.push(issue(
            "too_long",
            "error",
            format!("{characters} characters, at most {MAX_LENGTH} fit."),
        ));
    } else if !text.trim().is_empty() && characters < SHORT_LENGTH {
        issues.push(issue(
            "short",
            "hint",
            format!(
                "{characters} characters. Longer posts with one clear idea tend to carry further."
            ),
        ));
    }
    let hook_length = hook.chars().count();
    if hook_length > MAX_HOOK {
        issues.push(issue(
            "hook_too_long",
            "warning",
            format!(
                "The first line has {hook_length} characters. Keep it under {MAX_HOOK}, so it ends before \"see more\"."
            ),
        ));
    }
    if tags.len() > MAX_HASHTAGS {
        issues.push(issue(
            "too_many_hashtags",
            "warning",
            format!("{} hashtags. Three to five are enough.", tags.len()),
        ));
    } else if tags.is_empty() && !text.trim().is_empty() {
        issues.push(issue(
            "no_hashtags",
            "hint",
            "No hashtags. One to three help people find the post.".to_owned(),
        ));
    }
    if hook.contains('#') {
        issues.push(issue(
            "hashtag_in_hook",
            "hint",
            "Hashtags in the first line distract from the hook.".to_owned(),
        ));
    }
    if text.contains("http://") || text.contains("https://") {
        issues.push(issue(
            "link_in_post",
            "hint",
            "Links in a post often lower its reach. Consider the first comment.".to_owned(),
        ));
    }
    let rank = |severity: &str| match severity {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    };
    issues.sort_by_key(|issue| rank(&issue.severity));
    PostCheck {
        characters,
        words,
        hook,
        hashtags: tags,
        ok: !issues.iter().any(|issue| issue.severity == "error"),
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(check: &PostCheck) -> Vec<&str> {
        check
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect()
    }

    #[test]
    fn a_good_post_passes() {
        let body = "Most invoices are late for one reason.\n\n".to_owned()
            + &"We changed how we send reminders, and here is what happened. ".repeat(6)
            + "\n\n#smallbusiness #finance";
        let check = check_post(&body);
        assert!(check.ok);
        assert!(check.issues.is_empty(), "{:?}", check.issues);
        assert_eq!(check.hook, "Most invoices are late for one reason.");
        assert_eq!(check.hashtags, ["#smallbusiness", "#finance"]);
    }

    #[test]
    fn problems_are_found_and_sorted() {
        assert_eq!(codes(&check_post("  ")), ["empty"]);
        let long = format!("{}\n{}", "Hook", "x".repeat(MAX_LENGTH));
        assert_eq!(codes(&check_post(&long))[0], "too_long");
        assert!(!check_post(&long).ok);
        let hook = format!("{} #tag\nrest", "word ".repeat(40));
        let checked = check_post(&hook);
        let found = codes(&checked);
        assert!(found.contains(&"hook_too_long") && found.contains(&"hashtag_in_hook"));
        let tags = format!("Hook\n\n{} #a #b #c #d #e #f", "text ".repeat(70));
        assert_eq!(codes(&check_post(&tags)), ["too_many_hashtags"]);
        let short = check_post("Short note https://example.com");
        assert_eq!(codes(&short), ["short", "no_hashtags", "link_in_post"]);
        let unbroken = format!("{} {}", "Hook.", "more ".repeat(60));
        assert!(codes(&check_post(&unbroken)).contains(&"hook_too_long"));
    }

    #[test]
    fn hashtags_need_a_word_start_and_letters() {
        assert_eq!(
            hashtags("#one two#three #4 #a_b, #über"),
            ["#one", "#a_b", "#über"]
        );
    }
}
