use lighthouse_config::ConnectorConfig;
use lighthouse_core::agent::{AgentDefinition, ToolLevel};
use std::collections::{BTreeMap, BTreeSet};

const BRAIN: &str = "brain";
const BRAIN_READ_TOOLS: [&str; 2] = ["search", "get"];
/// Changes to the Brain are reversible and never leave the instance.
const BRAIN_DRAFT_TOOLS: [&str; 2] = ["remember", "link"];
/// Proposals to the owner, who accepts or rejects them. They never leave the instance.
const PROPOSAL_TOOLS: [(&str, &str); 3] = [
    ("tasks", "propose"),
    ("memory", "propose"),
    ("posts", "propose"),
];

/// Classifies tools by the connector allowlists.
///
/// A tool is `read` or `draft` only if its connector lists it. Everything else
/// counts as `write`, whatever the agent file says.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
    read_tools: BTreeMap<String, BTreeSet<String>>,
    draft_tools: BTreeMap<String, BTreeSet<String>>,
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
    /// Builds the policy from the connector configuration and the built-in tools.
    pub fn new(connectors: &BTreeMap<String, ConnectorConfig>) -> Self {
        let mut read_tools: BTreeMap<String, BTreeSet<String>> = connectors
            .iter()
            .map(|(name, connector)| (name.clone(), connector.read_tools.iter().cloned().collect()))
            .collect();
        let mut draft_tools: BTreeMap<String, BTreeSet<String>> = connectors
            .iter()
            .map(|(name, connector)| {
                (
                    name.clone(),
                    connector.draft_tools.iter().cloned().collect(),
                )
            })
            .collect();
        let builtin = |tools: &[&str]| tools.iter().map(|tool| (*tool).to_owned()).collect();
        read_tools.insert(BRAIN.to_owned(), builtin(&BRAIN_READ_TOOLS));
        draft_tools.insert(BRAIN.to_owned(), builtin(&BRAIN_DRAFT_TOOLS));
        for (connector, tool) in PROPOSAL_TOOLS {
            draft_tools.insert(connector.to_owned(), builtin(&[tool]));
        }
        Self {
            read_tools,
            draft_tools,
        }
    }

    /// Returns the effective level of a qualified tool.
    pub fn level(&self, qualified: &str) -> ToolLevel {
        let Some((connector, tool)) = qualified.split_once('.') else {
            return ToolLevel::Write;
        };
        let lists = |tools: &BTreeMap<String, BTreeSet<String>>| {
            tools.get(connector).is_some_and(|set| set.contains(tool))
        };
        if lists(&self.read_tools) {
            ToolLevel::Read
        } else if lists(&self.draft_tools) {
            ToolLevel::Draft
        } else {
            ToolLevel::Write
        }
    }

    /// Returns every tool whose real level exceeds what the agent file declares.
    ///
    /// The file has to admit that an agent acts, so a tool it declares as `read`
    /// cannot turn out to write.
    pub fn blocked(&self, agent: &AgentDefinition) -> Vec<BlockedTool> {
        agent
            .tools
            .iter()
            .filter_map(|grant| {
                let level = self.level(&grant.name);
                (level > grant.level).then(|| BlockedTool {
                    tool: grant.name.clone(),
                    reason: if level == ToolLevel::Write {
                        "the connector does not list it as read-only".to_owned()
                    } else {
                        "the agent file declares it below its level".to_owned()
                    },
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
            draft_tools: vec!["save_draft".to_owned()],
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
    fn brain_tools_are_built_in() {
        let policy = policy();
        assert_eq!(policy.level("brain.search"), ToolLevel::Read);
        assert_eq!(policy.level("brain.remember"), ToolLevel::Draft);
        assert_eq!(policy.level("brain.delete"), ToolLevel::Write);
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
    fn a_tool_declared_at_its_level_or_above_is_not_blocked() {
        let policy = policy();
        assert_eq!(policy.level("linear.save_draft"), ToolLevel::Draft);
        let agent = agent(
            "  - linear.list_issues: write\n  - linear.save_draft: draft\n  - linear.delete_issue: write",
        );
        assert!(policy.blocked(&agent).is_empty());
    }

    #[test]
    fn a_tool_declared_below_its_level_is_blocked() {
        let policy = policy();
        let blocked = policy.blocked(&agent("  - linear.save_draft"));
        assert!(blocked[0].reason.contains("below its level"));
        let blocked = policy.blocked(&agent("  - linear.delete_issue: draft"));
        assert!(blocked[0].reason.contains("does not list it"));
    }

    #[test]
    fn proposals_to_the_owner_are_drafts() {
        let policy = policy();
        assert_eq!(policy.level("tasks.propose"), ToolLevel::Draft);
        assert_eq!(policy.level("memory.propose"), ToolLevel::Draft);
        assert_eq!(policy.level("posts.propose"), ToolLevel::Draft);
        assert_eq!(policy.level("tasks.delete"), ToolLevel::Write);
    }
}
