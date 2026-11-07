//! Automation files: Markdown with YAML frontmatter, like agent files.
//!
//! An automation has one trigger and ordered steps. The body below the
//! frontmatter describes it for people. Parsing is strict, like for agents.

use crate::agent::{AgentError, Delivery, parse_cron, slugify};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Most steps an automation may have.
pub const MAX_STEPS: usize = 20;
/// Prefix of the run agent ID of an automation, as in `automation:<id>`.
pub const RUN_PREFIX: &str = "automation:";

fn field(field: &'static str, detail: impl Into<String>) -> AgentError {
    AgentError::Field {
        field,
        detail: detail.into(),
    }
}

/// When an automation runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutomationTrigger {
    /// On a cron schedule.
    Cron(String),
    /// When an event happens, such as `run.failed` or `mail.received`.
    Event {
        /// Event name.
        name: String,
        /// Fields the payload must contain, compared without case as substrings.
        filter: BTreeMap<String, String>,
    },
    /// When its webhook is called.
    Webhook {
        /// Fields the payload must contain.
        filter: BTreeMap<String, String>,
    },
    /// Only on request.
    Manual,
}

/// A check on the previous step. The automation stops when it fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// The previous output contains this text, ignoring case.
    Contains(String),
    /// The previous output does not contain this text, ignoring case.
    NotContains(String),
    /// The previous step ended with this status: `succeeded` or `failed`.
    Status(String),
}

/// One step.
#[derive(Clone, Debug, PartialEq)]
pub enum AutomationStep {
    /// Runs an agent with the event and previous output as context.
    Agent(String),
    /// Calls a tool. A tool with external effect waits for the owner's approval.
    Tool {
        /// Qualified tool name.
        name: String,
        /// Arguments. Strings may hold `{{...}}` placeholders.
        input: Value,
    },
    /// Sends a message.
    Notify {
        /// Target.
        to: Delivery,
        /// Subject. Defaults to the automation name.
        subject: Option<String>,
        /// Text in Markdown. May hold placeholders.
        message: String,
    },
    /// Stops the automation unless the condition holds.
    If(Condition),
    /// Runs `then` if the condition holds and `otherwise` if not, then goes on.
    Branch {
        /// The check.
        condition: Condition,
        /// Steps if it holds.
        then: Vec<AutomationStep>,
        /// Steps if it does not.
        otherwise: Vec<AutomationStep>,
    },
}

/// One instruction of an automation, with branches turned into jumps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op<'a> {
    /// Runs a step. `If` steps stop the run when they fail.
    Step(&'a AutomationStep),
    /// Goes to the instruction at the index unless the condition holds.
    JumpUnless(&'a Condition, usize),
    /// Goes to the instruction at the index.
    Jump(usize),
}

fn compile<'a>(steps: &'a [AutomationStep], ops: &mut Vec<Op<'a>>) {
    for step in steps {
        if let AutomationStep::Branch {
            condition,
            then,
            otherwise,
        } = step
        {
            let test = ops.len();
            ops.push(Op::Jump(0));
            compile(then, ops);
            let skip = ops.len();
            ops.push(Op::Jump(0));
            let else_start = ops.len();
            compile(otherwise, ops);
            let end = ops.len();
            ops[test] = Op::JumpUnless(condition, else_start);
            ops[skip] = Op::Jump(end);
        } else {
            ops.push(Op::Step(step));
        }
    }
}

/// Turns the steps into a flat program. The same file always gives the same
/// program, so a paused run resumes at the right instruction.
pub fn program(steps: &[AutomationStep]) -> Vec<Op<'_>> {
    let mut ops = Vec::new();
    compile(steps, &mut ops);
    ops
}

fn count(steps: &[AutomationStep]) -> usize {
    steps
        .iter()
        .map(|step| match step {
            AutomationStep::Branch {
                then, otherwise, ..
            } => 1 + count(then) + count(otherwise),
            _ => 1,
        })
        .sum()
}

impl AutomationStep {
    /// Returns the kind name: `agent`, `tool`, `notify` or `if`.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agent",
            Self::Tool { .. } => "tool",
            Self::Notify { .. } => "notify",
            Self::If(_) => "if",
            Self::Branch { .. } => "branch",
        }
    }
}

/// A parsed automation file.
#[derive(Clone, Debug, PartialEq)]
pub struct Automation {
    /// Stable ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether it runs on its trigger. The apps can override this.
    pub enabled: bool,
    /// Trigger.
    pub trigger: AutomationTrigger,
    /// Steps in order.
    pub steps: Vec<AutomationStep>,
    /// Description for people.
    pub description: String,
}

impl Automation {
    /// Returns the agent ID its runs have in the log.
    pub fn run_agent_id(&self) -> String {
        format!("{RUN_PREFIX}{}", self.id)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrigger {
    cron: Option<String>,
    event: Option<String>,
    webhook: Option<bool>,
    manual: Option<bool>,
    #[serde(rename = "match", default)]
    filter: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    name: String,
    #[serde(default)]
    input: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    #[serde(default = "owner")]
    to: String,
    subject: Option<String>,
    message: String,
}

fn owner() -> String {
    "owner".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCondition {
    contains: Option<String>,
    not_contains: Option<String>,
    status: Option<String>,
    then: Option<Vec<RawStep>>,
    #[serde(rename = "else")]
    otherwise: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum RawStep {
    Agent(String),
    Tool(RawTool),
    Notify(RawNotify),
    If(RawCondition),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAutomation {
    name: String,
    id: Option<String>,
    #[serde(default = "enabled")]
    enabled: bool,
    trigger: RawTrigger,
    steps: Vec<RawStep>,
}

fn enabled() -> bool {
    true
}

/// Checks an event name such as `run.failed` or `mail.received`.
pub fn valid_event_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    parts.clone().count() >= 2 && parts.all(valid)
}

fn parse_trigger(raw: RawTrigger) -> Result<AutomationTrigger, AgentError> {
    let filter = raw.filter;
    let trigger = match (raw.cron, raw.event, raw.webhook, raw.manual) {
        (Some(cron), None, None, None) => {
            parse_cron(&cron)?;
            if !filter.is_empty() {
                return Err(field("trigger.match", "only events and webhooks can match"));
            }
            AutomationTrigger::Cron(cron)
        }
        (None, Some(name), None, None) => {
            if !valid_event_name(&name) {
                return Err(field(
                    "trigger.event",
                    format!("`{name}` is not an event name like run.failed"),
                ));
            }
            AutomationTrigger::Event { name, filter }
        }
        (None, None, Some(true), None) => AutomationTrigger::Webhook { filter },
        (None, None, None, Some(true)) => AutomationTrigger::Manual,
        _ => {
            return Err(field(
                "trigger",
                "needs exactly one of cron, event, webhook: true or manual: true",
            ));
        }
    };
    Ok(trigger)
}

fn parse_condition(
    contains: Option<String>,
    not_contains: Option<String>,
    status: Option<String>,
) -> Result<Condition, AgentError> {
    match (contains, not_contains, status) {
        (Some(text), None, None) => Ok(Condition::Contains(text)),
        (None, Some(text), None) => Ok(Condition::NotContains(text)),
        (None, None, Some(status)) if status == "succeeded" || status == "failed" => {
            Ok(Condition::Status(status))
        }
        _ => Err(field(
            "steps.if",
            "needs exactly one of contains, not_contains or status: succeeded|failed",
        )),
    }
}

fn parse_step(raw: RawStep) -> Result<AutomationStep, AgentError> {
    Ok(match raw {
        RawStep::Agent(id) => {
            if slugify(&id) != id || id.is_empty() {
                return Err(field("steps.agent", format!("`{id}` is not an agent ID")));
            }
            AutomationStep::Agent(id)
        }
        RawStep::Tool(tool) => {
            if !matches!(tool.name.split_once('.'), Some((c, t)) if !c.is_empty() && !t.is_empty())
            {
                return Err(field(
                    "steps.tool",
                    format!("`{}` is not connector.tool", tool.name),
                ));
            }
            AutomationStep::Tool {
                name: tool.name,
                input: tool
                    .input
                    .unwrap_or_else(|| Value::Object(Default::default())),
            }
        }
        RawStep::Notify(notify) => {
            if notify.message.trim().is_empty() {
                return Err(field("steps.notify", "needs a message"));
            }
            AutomationStep::Notify {
                to: notify.to.parse()?,
                subject: notify.subject,
                message: notify.message,
            }
        }
        RawStep::If(raw) => {
            let condition = parse_condition(raw.contains, raw.not_contains, raw.status)?;
            match (raw.then, raw.otherwise) {
                (None, None) => AutomationStep::If(condition),
                (then, otherwise) => AutomationStep::Branch {
                    condition,
                    then: then
                        .unwrap_or_default()
                        .into_iter()
                        .map(parse_step)
                        .collect::<Result<_, _>>()?,
                    otherwise: otherwise
                        .unwrap_or_default()
                        .into_iter()
                        .map(parse_step)
                        .collect::<Result<_, _>>()?,
                },
            }
        }
    })
}

impl Automation {
    /// Parses an automation file.
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
        let raw: RawAutomation = serde_saphyr::from_str(frontmatter)
            .map_err(|error| AgentError::Frontmatter(error.to_string()))?;
        let id = raw.id.unwrap_or_else(|| slugify(&raw.name));
        if id.is_empty() || slugify(&id) != id {
            return Err(field("id", "must be lowercase letters, digits and dashes"));
        }
        if raw.steps.is_empty() {
            return Err(field("steps", format!("needs 1 to {MAX_STEPS} steps")));
        }
        Ok(Self {
            id,
            name: raw.name,
            enabled: raw.enabled,
            trigger: parse_trigger(raw.trigger)?,
            steps: raw
                .steps
                .into_iter()
                .map(parse_step)
                .collect::<Result<_, _>>()?,
            description: body.trim().to_owned(),
        })
        .and_then(|automation| {
            if count(&automation.steps) > MAX_STEPS {
                return Err(field(
                    "steps",
                    format!("needs 1 to {MAX_STEPS} steps, branches included"),
                ));
            }
            Ok(automation)
        })
    }
}

/// Reads a field of a JSON payload by a dotted path, such as `from` or `run.agent_id`.
pub fn payload_field<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(payload, |value, key| value.get(key))
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Checks a payload against a filter: every field must contain its text,
/// ignoring case.
pub fn matches(filter: &BTreeMap<String, String>, payload: &Value) -> bool {
    filter.iter().all(|(path, expected)| {
        payload_field(payload, path).is_some_and(|value| {
            as_text(value)
                .to_lowercase()
                .contains(&expected.to_lowercase())
        })
    })
}

/// Fills `{{output}}`, `{{status}}`, `{{automation}}`, `{{event}}` and
/// `{{event.<field>}}` placeholders. Unknown placeholders become empty.
pub fn render(template: &str, values: &BTreeMap<&str, String>, event: &Value) -> String {
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
        let value = if key == "event" {
            as_text(event)
        } else if let Some(path) = key.strip_prefix("event.") {
            payload_field(event, path).map(as_text).unwrap_or_default()
        } else {
            values.get(key).cloned().unwrap_or_default()
        };
        result.push_str(&value);
        rest = &after[end + 2..];
    }
    result.push_str(rest);
    result
}

/// Fills placeholders in every string of a JSON value.
pub fn render_value(value: &Value, values: &BTreeMap<&str, String>, event: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(render(text, values, event)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_value(item, values, event))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), render_value(item, values, event)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FULL: &str = r#"---
name: Triage failed runs
trigger:
  event: run.failed
  match:
    agent_id: brief
steps:
  - agent: triage
  - if:
      contains: urgent
  - tool:
      name: linear.create_issue
      input:
        title: "Check {{event.agent_id}}"
  - notify:
      to: owner
      message: "Triage: {{output}}"
---
Looks into failed briefs.
"#;

    fn with(frontmatter: &str) -> Result<Automation, AgentError> {
        Automation::parse(&format!("---\n{frontmatter}\n---\n"))
    }

    #[test]
    fn branches_compile_into_jumps() {
        let automation = with(
            "name: A\ntrigger:\n  manual: true\nsteps:\n  - agent: triage\n  - if:\n      contains: urgent\n      then:\n        - agent: escalate\n      else:\n        - notify:\n            message: calm\n        - agent: archive\n  - agent: wrap",
        )
        .unwrap();
        assert_eq!(automation.steps[1].kind(), "branch");
        let ops = program(&automation.steps);
        let shape: Vec<String> = ops
            .iter()
            .map(|op| match op {
                Op::Step(step) => step.kind().to_owned(),
                Op::JumpUnless(_, target) => format!("unless->{target}"),
                Op::Jump(target) => format!("jump->{target}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "agent",
                "unless->4",
                "agent",
                "jump->6",
                "notify",
                "agent",
                "agent"
            ]
        );
        let only_then = with(
            "name: A\ntrigger:\n  manual: true\nsteps:\n  - if:\n      status: failed\n      then:\n        - agent: retry",
        )
        .unwrap();
        assert_eq!(program(&only_then.steps).len(), 3);
        let nested = format!(
            "name: A\ntrigger:\n  manual: true\nsteps:\n  - if:\n      status: failed\n      then:\n{}",
            "        - agent: a\n".repeat(MAX_STEPS)
        );
        assert!(
            with(nested.trim_end()).is_err(),
            "branches count toward the limit"
        );
    }

    #[test]
    fn the_examples_parse() {
        for example in [
            include_str!("../../../automations/examples/triage-failed-briefs.md"),
            include_str!("../../../automations/examples/reply-to-offer-requests.md"),
        ] {
            let automation = Automation::parse(example).unwrap();
            assert!(!automation.description.is_empty());
        }
    }

    #[test]
    fn parses_a_full_automation() {
        let automation = Automation::parse(FULL).unwrap();
        assert_eq!(automation.id, "triage-failed-runs");
        assert!(automation.enabled);
        assert_eq!(automation.run_agent_id(), "automation:triage-failed-runs");
        assert_eq!(
            automation.trigger,
            AutomationTrigger::Event {
                name: "run.failed".to_owned(),
                filter: BTreeMap::from([("agent_id".to_owned(), "brief".to_owned())]),
            }
        );
        let kinds: Vec<_> = automation.steps.iter().map(AutomationStep::kind).collect();
        assert_eq!(kinds, ["agent", "if", "tool", "notify"]);
        assert_eq!(
            automation.steps[1],
            AutomationStep::If(Condition::Contains("urgent".to_owned()))
        );
        assert_eq!(automation.description, "Looks into failed briefs.");
    }

    #[test]
    fn parses_every_trigger_and_condition() {
        let steps = "steps:\n  - agent: a";
        let cases = [
            (
                "trigger:\n  cron: \"0 7 * * *\"",
                AutomationTrigger::Cron("0 7 * * *".to_owned()),
            ),
            (
                "trigger:\n  webhook: true",
                AutomationTrigger::Webhook {
                    filter: BTreeMap::new(),
                },
            ),
            ("trigger:\n  manual: true", AutomationTrigger::Manual),
        ];
        for (trigger, expected) in cases {
            let automation = with(&format!("name: A\nenabled: false\n{trigger}\n{steps}")).unwrap();
            assert_eq!(automation.trigger, expected);
            assert!(!automation.enabled);
        }
        let conditions = with(
            "name: A\ntrigger:\n  manual: true\nsteps:\n  - if:\n      not_contains: ok\n  - if:\n      status: failed\n  - tool:\n      name: brain.search",
        )
        .unwrap();
        assert_eq!(
            conditions.steps[0],
            AutomationStep::If(Condition::NotContains("ok".to_owned()))
        );
        assert_eq!(
            conditions.steps[1],
            AutomationStep::If(Condition::Status("failed".to_owned()))
        );
        assert_eq!(
            conditions.steps[2],
            AutomationStep::Tool {
                name: "brain.search".to_owned(),
                input: json!({})
            }
        );
    }

    #[test]
    fn rejects_invalid_files() {
        let manual = "trigger:\n  manual: true";
        for frontmatter in [
            format!("name: A\n{manual}\nsteps: []"),
            "name: A\ntrigger:\n  cron: \"0 7 * * *\"\n  manual: true\nsteps:\n  - agent: a"
                .to_owned(),
            "name: A\ntrigger:\n  cron: nonsense\nsteps:\n  - agent: a".to_owned(),
            "name: A\ntrigger:\n  cron: \"0 7 * * *\"\n  match:\n    a: b\nsteps:\n  - agent: a"
                .to_owned(),
            "name: A\ntrigger:\n  event: Failed\nsteps:\n  - agent: a".to_owned(),
            format!("name: A\n{manual}\nsteps:\n  - agent: Bad Agent"),
            format!("name: A\n{manual}\nsteps:\n  - tool:\n      name: nodot"),
            format!("name: A\n{manual}\nsteps:\n  - notify:\n      message: \" \""),
            format!("name: A\n{manual}\nsteps:\n  - notify:\n      to: nowhere\n      message: hi"),
            format!("name: A\n{manual}\nsteps:\n  - if:\n      status: maybe"),
            format!("name: A\n{manual}\nsteps:\n  - if:\n      contains: a\n      status: failed"),
            format!("name: A\n{manual}\nsteps:\n  - unknown: x"),
            format!("name: A\nid: ../x\n{manual}\nsteps:\n  - agent: a"),
        ] {
            assert!(with(&frontmatter).is_err(), "{frontmatter}");
        }
        assert!(Automation::parse("no frontmatter").is_err());
        let long = format!(
            "name: A\n{manual}\nsteps:\n{}",
            "  - agent: a\n".repeat(MAX_STEPS + 1)
        );
        assert!(with(long.trim_end()).is_err());
    }

    #[test]
    fn filters_and_placeholders_read_the_payload() {
        let event =
            json!({"from": "Alex <alex@example.com>", "run": {"agent_id": "brief"}, "n": 3});
        let filter = BTreeMap::from([("from".to_owned(), "ALEX@example".to_owned())]);
        assert!(matches(&filter, &event));
        let other = BTreeMap::from([("run.agent_id".to_owned(), "inbox".to_owned())]);
        assert!(!matches(&other, &event));
        let missing = BTreeMap::from([("subject".to_owned(), "x".to_owned())]);
        assert!(!matches(&missing, &event));

        let values = BTreeMap::from([("output", "done".to_owned())]);
        assert_eq!(
            render(
                "{{ output }} for {{event.run.agent_id}} ({{event.n}}){{unknown}}",
                &values,
                &event
            ),
            "done for brief (3)"
        );
        assert!(render("{{event}}", &values, &event).contains("alex@example.com"));
        assert_eq!(
            render("broken {{output", &values, &event),
            "broken {{output"
        );
        let input = json!({"title": "{{output}}", "tags": ["{{event.n}}"], "count": 1});
        assert_eq!(
            render_value(&input, &values, &event),
            json!({"title": "done", "tags": ["3"], "count": 1})
        );
        assert!(valid_event_name("mail.received"));
        assert!(!valid_event_name("mail"));
        assert!(!valid_event_name("mail..x"));
    }
}
