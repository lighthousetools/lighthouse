//! Agent files: Markdown with YAML frontmatter.
//!
//! Parsing is strict. Unknown fields are errors, so that a feature an instance
//! does not support yet is rejected instead of silently ignored.

use croner::Cron;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;

/// Errors in an agent file.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentError {
    /// The file does not start with a frontmatter block.
    #[error("agent file must start with a `---` frontmatter block")]
    MissingFrontmatter,
    /// The frontmatter is invalid.
    #[error("invalid frontmatter: {0}")]
    Frontmatter(String),
    /// A field has an invalid value.
    #[error("invalid {field}: {detail}")]
    Field {
        /// Field name.
        field: &'static str,
        /// What is wrong.
        detail: String,
    },
}

fn field(field: &'static str, detail: impl Into<String>) -> AgentError {
    AgentError::Field {
        field,
        detail: detail.into(),
    }
}

/// What a tool may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolLevel {
    /// Reads only.
    #[default]
    Read,
    /// Reversible, without external effect.
    Draft,
    /// External effect.
    Write,
}

/// A tool an agent may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolGrant {
    /// Qualified name, `connector.tool`.
    pub name: String,
    /// Declared level.
    pub level: ToolLevel,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawTool {
    Read(String),
    Level(std::collections::BTreeMap<String, ToolLevel>),
}

/// Who owns an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One person.
    Personal(String),
    /// One team.
    Team(String),
    /// The whole organization.
    Org,
}

impl FromStr for Scope {
    type Err = AgentError;

    fn from_str(value: &str) -> Result<Self, AgentError> {
        match value.split_once(':') {
            None if value == "org" => Ok(Self::Org),
            Some(("personal", name)) if !name.is_empty() => Ok(Self::Personal(name.to_owned())),
            Some(("team", name)) if !name.is_empty() => Ok(Self::Team(name.to_owned())),
            _ => Err(field(
                "scope",
                format!("`{value}` is not org, personal:<user> or team:<team>"),
            )),
        }
    }
}

/// When an agent runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// On a cron schedule. Manual runs stay possible.
    Cron(String),
    /// Only on request.
    Manual,
}

/// Where a report goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// An email address.
    Mail(String),
    /// A Slack alias from the configuration.
    Slack(String),
}

impl FromStr for Delivery {
    type Err = AgentError;

    fn from_str(value: &str) -> Result<Self, AgentError> {
        match value.split_once(':') {
            Some(("mail", address)) if address.contains('@') => Ok(Self::Mail(address.to_owned())),
            Some(("slack", alias)) if !alias.is_empty() => Ok(Self::Slack(alias.to_owned())),
            _ => Err(field(
                "deliver",
                format!("`{value}` is not mail:<address> or slack:<alias>"),
            )),
        }
    }
}

/// Provider preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Models {
    /// Provider for the text task.
    pub text: Option<String>,
    /// Fallback provider.
    pub fallback: Option<String>,
}

/// Limits of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    /// Maximum cost in EUR.
    pub max_cost_eur: f64,
    /// Maximum duration.
    pub timeout: Duration,
    /// Maximum number of model calls.
    pub max_steps: u32,
}

/// A parsed agent file.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDefinition {
    /// Stable ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Scope.
    pub scope: Scope,
    /// Trigger.
    pub trigger: Trigger,
    /// Granted tools.
    pub tools: Vec<ToolGrant>,
    /// Skills the agent may load.
    pub skills: Vec<String>,
    /// Provider preferences.
    pub models: Models,
    /// Limits.
    pub limits: Limits,
    /// Delivery targets.
    pub deliver: Vec<Delivery>,
    /// The agent's instructions.
    pub prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrigger {
    cron: Option<String>,
    manual: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_cost_eur: Option<f64>,
    timeout: Option<String>,
    max_steps: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    id: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    scope: String,
    trigger: RawTrigger,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    models: Models,
    limits: Option<RawLimits>,
    #[serde(default)]
    deliver: Vec<String>,
}

const DEFAULT_MAX_COST_EUR: f64 = 1.0;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_MAX_STEPS: u32 = 20;

/// Derives a stable ID from a name.
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase();
    let parts = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty());
    parts.collect::<Vec<_>>().join("-")
}

/// Parses a duration such as `90s`, `10m` or `1h`.
pub fn parse_duration(value: &str) -> Result<Duration, AgentError> {
    let invalid = || {
        field(
            "timeout",
            format!("`{value}` is not a duration like 90s, 10m or 1h"),
        )
    };
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let seconds = match unit {
        "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        _ => return Err(invalid()),
    };
    Ok(Duration::from_secs(seconds))
}

/// Parses a cron expression.
pub fn parse_cron(value: &str) -> Result<Cron, AgentError> {
    Cron::from_str(value).map_err(|error| field("trigger.cron", format!("`{value}`: {error}")))
}

fn parse_tool(raw: RawTool) -> Result<ToolGrant, AgentError> {
    let (name, level) = match raw {
        RawTool::Read(name) => (name, ToolLevel::Read),
        RawTool::Level(map) => {
            let mut entries = map.into_iter();
            match (entries.next(), entries.next()) {
                (Some(entry), None) => entry,
                _ => return Err(field("tools", "an entry must name exactly one tool")),
            }
        }
    };
    match name.split_once('.') {
        Some((connector, tool)) if !connector.is_empty() && !tool.is_empty() => {
            Ok(ToolGrant { name, level })
        }
        _ => Err(field("tools", format!("`{name}` is not connector.tool"))),
    }
}

impl AgentDefinition {
    /// Parses an agent file.
    pub fn parse(text: &str) -> Result<Self, AgentError> {
        let rest = text
            .strip_prefix("---\n")
            .ok_or(AgentError::MissingFrontmatter)?;
        let (frontmatter, body) = match rest.split_once("\n---\n") {
            Some(parts) => parts,
            None => rest
                .strip_suffix("\n---")
                .map(|yaml| (yaml, ""))
                .ok_or(AgentError::MissingFrontmatter)?,
        };
        let raw: RawAgent = serde_yaml_ng::from_str(frontmatter)
            .map_err(|error| AgentError::Frontmatter(error.to_string()))?;
        let trigger = match (raw.trigger.cron, raw.trigger.manual) {
            (Some(cron), _) => {
                parse_cron(&cron)?;
                Trigger::Cron(cron)
            }
            (None, Some(true)) => Trigger::Manual,
            _ => return Err(field("trigger", "needs cron or manual: true")),
        };
        let limits = raw.limits.unwrap_or(RawLimits {
            max_cost_eur: None,
            timeout: None,
            max_steps: None,
        });
        let limits = Limits {
            max_cost_eur: limits.max_cost_eur.unwrap_or(DEFAULT_MAX_COST_EUR),
            timeout: limits
                .timeout
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(DEFAULT_TIMEOUT),
            max_steps: limits.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
        };
        if limits.max_cost_eur <= 0.0 || limits.max_steps == 0 {
            return Err(field("limits", "must be positive"));
        }
        let id = raw.id.unwrap_or_else(|| slugify(&raw.name));
        if id.is_empty() {
            return Err(field("id", "must not be empty"));
        }
        let prompt = body.trim().to_owned();
        if prompt.is_empty() {
            return Err(field(
                "prompt",
                "the body below the frontmatter must not be empty",
            ));
        }
        let _ = raw.owner;
        Ok(Self {
            id,
            name: raw.name,
            scope: raw.scope.parse()?,
            trigger,
            tools: raw
                .tools
                .into_iter()
                .map(parse_tool)
                .collect::<Result<_, _>>()?,
            skills: raw.skills,
            models: raw.models,
            limits,
            deliver: raw
                .deliver
                .iter()
                .map(|value| value.parse())
                .collect::<Result<_, _>>()?,
            prompt,
        })
    }
}

/// Converts a qualified tool name into a name every model API accepts.
pub fn wire_name(qualified: &str) -> String {
    qualified.replacen('.', "__", 1)
}

/// Converts a wire name back into a qualified tool name.
pub fn qualified_name(wire: &str) -> Option<String> {
    wire.split_once("__")
        .map(|(connector, tool)| format!("{connector}.{tool}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r##"---
name: Weekly report
id: weekly
scope: org
owner: alex
trigger:
  cron: "0 8 * * FRI"
tools:
  - linear.list_issues
  - github.list_releases
  - slack.post: write
skills: [weekly-report]
models:
  text: anthropic
  fallback: openai
limits:
  max_cost_eur: 0.5
  timeout: 10m
  max_steps: 5
deliver: ["slack:#weekly", "mail:team@example.com"]
---
Write the report.
"##;

    fn with(frontmatter: &str) -> Result<AgentDefinition, AgentError> {
        AgentDefinition::parse(&format!("---\n{frontmatter}\n---\nDo it.\n"))
    }

    const MINIMAL: &str = "name: Morning Brief\nscope: org\ntrigger:\n  manual: true";

    #[test]
    fn parses_a_full_agent() {
        let agent = AgentDefinition::parse(FULL).unwrap();
        assert_eq!(agent.id, "weekly");
        assert_eq!(agent.scope, Scope::Org);
        assert_eq!(agent.trigger, Trigger::Cron("0 8 * * FRI".to_owned()));
        assert_eq!(
            agent.tools[0],
            ToolGrant {
                name: "linear.list_issues".to_owned(),
                level: ToolLevel::Read
            }
        );
        assert_eq!(agent.tools[2].level, ToolLevel::Write);
        assert_eq!(agent.limits.timeout, Duration::from_secs(600));
        assert_eq!(agent.limits.max_steps, 5);
        assert_eq!(agent.deliver[0], Delivery::Slack("#weekly".to_owned()));
        assert_eq!(
            agent.deliver[1],
            Delivery::Mail("team@example.com".to_owned())
        );
        assert_eq!(agent.prompt, "Write the report.");
    }

    #[test]
    fn derives_id_and_defaults() {
        let agent = with(MINIMAL).unwrap();
        assert_eq!(agent.id, "morning-brief");
        assert_eq!(agent.trigger, Trigger::Manual);
        assert_eq!(agent.limits.max_steps, DEFAULT_MAX_STEPS);
    }

    #[test]
    fn rejects_unknown_fields_such_as_memory() {
        let error = with(&format!("{MINIMAL}\nmemory:\n  read: true"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `memory`"), "{error}");
    }

    #[test]
    fn rejects_missing_frontmatter_and_empty_prompt() {
        assert_eq!(
            AgentDefinition::parse("just text"),
            Err(AgentError::MissingFrontmatter)
        );
        assert_eq!(
            AgentDefinition::parse("---\nname: x"),
            Err(AgentError::MissingFrontmatter)
        );
        let error = AgentDefinition::parse(&format!("---\n{MINIMAL}\n---")).unwrap_err();
        assert!(error.to_string().contains("prompt"), "{error}");
    }

    #[test]
    fn rejects_bad_values() {
        let cases = [
            ("name: x\nscope: nowhere\ntrigger:\n  manual: true", "scope"),
            (
                "name: x\nscope: org\ntrigger:\n  cron: \"nope\"",
                "trigger.cron",
            ),
            ("name: x\nscope: org\ntrigger:\n  manual: false", "trigger"),
            (
                "name: x\nscope: org\ntrigger:\n  manual: true\ntools: [linear]",
                "tools",
            ),
            (
                "name: x\nscope: org\ntrigger:\n  manual: true\ndeliver: [\"fax:1\"]",
                "deliver",
            ),
            (
                "name: x\nscope: org\ntrigger:\n  manual: true\nlimits:\n  timeout: soon",
                "timeout",
            ),
            (
                "name: x\nscope: org\ntrigger:\n  manual: true\nlimits:\n  max_steps: 0",
                "limits",
            ),
            ("name: \"!!\"\nscope: org\ntrigger:\n  manual: true", "id"),
        ];
        for (frontmatter, expected) in cases {
            let error = with(frontmatter).unwrap_err().to_string();
            assert!(error.contains(expected), "{frontmatter}: {error}");
        }
    }

    #[test]
    fn parses_scopes() {
        assert_eq!(
            "personal:alex".parse(),
            Ok(Scope::Personal("alex".to_owned()))
        );
        assert_eq!("team:core".parse(), Ok(Scope::Team("core".to_owned())));
        assert!("team:".parse::<Scope>().is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5d").is_err());
    }

    #[test]
    fn wire_names_round_trip() {
        assert_eq!(wire_name("linear.list_issues"), "linear__list_issues");
        assert_eq!(
            qualified_name("linear__list_issues").unwrap(),
            "linear.list_issues"
        );
        assert_eq!(qualified_name("load_skill"), None);
    }

    #[test]
    fn tool_entry_must_name_one_tool() {
        let frontmatter = format!("{MINIMAL}\ntools:\n  - a.b: read\n    c.d: read");
        assert!(
            with(&frontmatter)
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }
}
