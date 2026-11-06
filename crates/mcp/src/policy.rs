use lighthouse_config::ConnectorConfig;
use lighthouse_core::agent::{AgentDefinition, ToolLevel};
use std::collections::{BTreeMap, BTreeSet};

/// Classifies tools by the connector allowlists.
///
/// A tool is `read` only if its connector lists it. Everything else counts as
/// `write`, whatever the agent file says.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
    read_tools: BTreeMap<String, BTreeSet<String>>,
}

/// A tool that stops an agent from running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedTool {
    /// Qualified tool name.
    pub tool: String,
    /// Why it is blocked.
    pub reason: String,
}

impl ToolPolicy {
    /// Builds the policy from the connector configuration.
    pub fn new(connectors: &BTreeMap<String, ConnectorConfig>) -> Self {
        let read_tools = connectors
            .iter()
            .map(|(name, connector)| (name.clone(), connector.read_tools.iter().cloned().collect()))
            .collect();
        Self { read_tools }
    }

    /// Returns the effective level of a qualified tool.
    pub fn level(&self, qualified: &str) -> ToolLevel {
        let allowed = qualified
            .split_once('.')
            .and_then(|(connector, tool)| Some(self.read_tools.get(connector)?.contains(tool)));
        if allowed == Some(true) {
            ToolLevel::Read
        } else {
            ToolLevel::Write
        }
    }

    /// Returns every tool of an agent that is above `read`.
    pub fn blocked(&self, agent: &AgentDefinition) -> Vec<BlockedTool> {
        agent
            .tools
            .iter()
            .filter_map(|grant| {
                let reason = if grant.level != ToolLevel::Read {
                    format!("the agent file declares it as {:?}", grant.level).to_lowercase()
                } else if self.level(&grant.name) != ToolLevel::Read {
                    "the connector does not list it as read-only".to_owned()
                } else {
                    return None;
                };
                Some(BlockedTool {
                    tool: grant.name.clone(),
                    reason,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_config::ConnectorTransport;

    fn policy() -> ToolPolicy {
        let connector = ConnectorConfig {
            transport: ConnectorTransport::Stdio {
                command: "x".to_owned(),
                args: Vec::new(),
                secret_env: BTreeMap::new(),
            },
            read_tools: vec!["list_issues".to_owned()],
        };
        ToolPolicy::new(&BTreeMap::from([("linear".to_owned(), connector)]))
    }

    fn agent(tools: &str) -> AgentDefinition {
        AgentDefinition::parse(&format!(
            "---\nname: x\nscope: org\ntrigger:\n  manual: true\ntools:\n{tools}\n---\nGo.\n"
        ))
        .unwrap()
    }

    #[test]
    fn only_allowlisted_tools_are_read() {
        let policy = policy();
        assert_eq!(policy.level("linear.list_issues"), ToolLevel::Read);
        assert_eq!(policy.level("linear.delete_issue"), ToolLevel::Write);
        assert_eq!(policy.level("github.list_issues"), ToolLevel::Write);
        assert_eq!(policy.level("nonsense"), ToolLevel::Write);
    }

    #[test]
    fn allowlisted_read_agent_is_not_blocked() {
        assert!(
            policy()
                .blocked(&agent("  - linear.list_issues"))
                .is_empty()
        );
    }

    #[test]
    fn unsuffixed_unclassified_tool_is_blocked() {
        let blocked = policy().blocked(&agent("  - linear.delete_issue"));
        assert_eq!(blocked[0].tool, "linear.delete_issue");
        assert!(blocked[0].reason.contains("does not list it"));
    }

    #[test]
    fn declared_write_and_draft_are_blocked_even_on_read_tools() {
        let blocked = policy().blocked(&agent(
            "  - linear.list_issues: write\n  - linear.list_issues: draft",
        ));
        assert_eq!(blocked.len(), 2);
        assert!(blocked[0].reason.contains("write"));
        assert!(blocked[1].reason.contains("draft"));
    }
}
