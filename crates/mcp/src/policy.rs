use crate::ConnectorSource;
use lighthouse_config::ConnectorConfig;
use lighthouse_core::agent::{AgentDefinition, ToolLevel};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
/// counts as `write`, whatever the agent file says. A live policy reads the
/// connectors from a source on every question, so it follows changes and
/// answers for the user the current task acts for.
#[derive(Clone, Default)]
pub struct ToolPolicy {
    fixed: Arc<Lists>,
    live: Option<Arc<dyn ConnectorSource>>,
}

impl std::fmt::Debug for ToolPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolPolicy")
            .field("live", &self.live.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct Lists {
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
        Self {
            fixed: Arc::new(Lists::new(connectors.iter())),
            live: None,
        }
    }

    /// A policy that reads the connectors from a source on every question.
    pub fn live(source: Arc<dyn ConnectorSource>) -> Self {
        Self {
            fixed: Arc::default(),
            live: Some(source),
        }
    }

    fn lists(&self) -> Arc<Lists> {
        match &self.live {
            Some(source) => {
                let connections = source.connections();
                Arc::new(Lists::new(
                    connections
                        .iter()
                        .map(|(name, connection)| (name, &connection.config)),
                ))
            }
            None => self.fixed.clone(),
        }
    }

    /// Returns the effective level of a qualified tool.
    pub fn level(&self, qualified: &str) -> ToolLevel {
        self.lists().level(qualified)
    }

    /// Returns every tool whose real level exceeds what the agent file declares.
    ///
    /// The file has to admit that an agent acts, so a tool it declares as `read`
    /// cannot turn out to write.
    pub fn blocked(&self, agent: &AgentDefinition) -> Vec<BlockedTool> {
        let lists = self.lists();
        agent
            .tools
            .iter()
            .filter_map(|grant| {
                let level = lists.level(&grant.name);
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

impl Lists {
    fn new<'a>(
        connectors: impl Iterator<Item = (&'a String, &'a ConnectorConfig)> + Clone,
    ) -> Self {
        let mut read_tools: BTreeMap<String, BTreeSet<String>> = connectors
            .clone()
            .map(|(name, connector)| (name.clone(), connector.read_tools.iter().cloned().collect()))
            .collect();
        let mut draft_tools: BTreeMap<String, BTreeSet<String>> = connectors
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

    fn level(&self, qualified: &str) -> ToolLevel {
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

    struct Switching(std::sync::Mutex<bool>);

    impl ConnectorSource for Switching {
        fn connections(&self) -> Arc<BTreeMap<String, crate::Connection>> {
            let listed = *self.0.lock().unwrap();
            let connector = ConnectorConfig {
                transport: ConnectorTransport::Stdio {
                    command: "x".to_owned(),
                    args: Vec::new(),
                    secret_env: BTreeMap::new(),
                },
                read_tools: if listed {
                    vec!["list_threads".to_owned()]
                } else {
                    Vec::new()
                },
                draft_tools: Vec::new(),
            };
            Arc::new(BTreeMap::from([(
                "mail".to_owned(),
                crate::Connection {
                    config: connector,
                    vaults: Vec::new(),
                    guarded: false,
                },
            )]))
        }
    }

    #[test]
    fn a_live_policy_follows_its_source() {
        let source = Arc::new(Switching(std::sync::Mutex::new(true)));
        let policy = ToolPolicy::live(source.clone());
        assert_eq!(policy.level("mail.list_threads"), ToolLevel::Read);
        assert_eq!(policy.level("brain.search"), ToolLevel::Read);
        *source.0.lock().unwrap() = false;
        assert_eq!(policy.level("mail.list_threads"), ToolLevel::Write);
        assert!(format!("{policy:?}").contains("live: true"));
        assert!(!policy.blocked(&agent("  - mail.list_threads")).is_empty());
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
