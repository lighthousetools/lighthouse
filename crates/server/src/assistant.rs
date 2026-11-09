//! The assistant that answers conversations when no Talk agent is chosen.
//!
//! It is not an agent file. Its tools come from the connectors the person has
//! connected, looked up for every message, so a new connection works in the
//! chat at once. The connector allowlists decide each tool's level as for any
//! agent: tools they do not list wait for approval.

use lighthouse_core::agent::{
    AgentDefinition, Limits, Models, Scope, ToolGrant, ToolLevel, Trigger,
};
use lighthouse_mcp::{ToolHostFactory, ToolPolicy};
use std::time::Duration;

/// The agent ID of the assistant's runs. The colon keeps it apart from agent files.
pub const ASSISTANT_ID: &str = "talk:assistant";
const MAX_COST_EUR: f64 = 1.0;
const MAX_STEPS: u32 = 12;
const TIMEOUT: Duration = Duration::from_secs(300);
/// A large server should not crowd out the ones that only read.
const MAX_TOOLS: usize = 96;
/// Notes and proposals stay inside the instance.
const OWN_TOOLS: [(&str, ToolLevel); 5] = [
    ("brain.search", ToolLevel::Read),
    ("brain.get", ToolLevel::Read),
    ("brain.remember", ToolLevel::Draft),
    ("memory.propose", ToolLevel::Draft),
    ("tasks.propose", ToolLevel::Draft),
];

/// Builds the assistant for one message, for the user the task acts for.
///
/// Returns `None` when no connector can be reached, so the caller can let the
/// Second Brain answer. A connector that fails is left out and named in the
/// prompt, so one broken connection does not take the chat down.
pub async fn assistant(
    tools: &dyn ToolHostFactory,
    policy: &ToolPolicy,
    connectors: &[String],
    message: &str,
) -> Option<AgentDefinition> {
    let mut grants = Vec::new();
    let mut reachable = Vec::new();
    let mut failed = Vec::new();
    for connector in connectors {
        match tools.offered(connector).await {
            Ok(offered) => {
                grants.extend(offered.iter().map(|tool| {
                    let name = format!("{connector}.{tool}");
                    let level = policy.level(&name);
                    ToolGrant { name, level }
                }));
                reachable.push(connector.as_str());
            }
            Err(error) => {
                tracing::warn!(%connector, %error, "the assistant leaves a connector out");
                failed.push(connector.as_str());
            }
        }
    }
    if reachable.is_empty() {
        return None;
    }
    grants.sort_by_key(|grant| grant.level);
    grants.truncate(MAX_TOOLS.saturating_sub(OWN_TOOLS.len()));
    grants.extend(OWN_TOOLS.iter().map(|(name, level)| ToolGrant {
        name: (*name).to_owned(),
        level: *level,
    }));
    Some(AgentDefinition {
        id: ASSISTANT_ID.to_owned(),
        name: "Assistant".to_owned(),
        scope: Scope::Org,
        trigger: Trigger::Manual,
        tools: grants,
        skills: Vec::new(),
        models: Models::default(),
        limits: Limits {
            max_cost_eur: MAX_COST_EUR,
            timeout: TIMEOUT,
            max_steps: MAX_STEPS,
        },
        deliver: Vec::new(),
        prompt: prompt(&reachable, &failed, message),
    })
}

fn prompt(reachable: &[&str], failed: &[&str], message: &str) -> String {
    let mut prompt = format!(
        "You are the owner's assistant in a conversation. Answer their latest message, in its \
         language, and keep the answer short. Look things up with your tools before you say you \
         do not know. Use brain.search for their notes and these connected services for live \
         data: {}. Say plainly when a tool fails or finds nothing. Store a note with \
         brain.remember only when they ask for it.\n",
        reachable.join(", ")
    );
    if !failed.is_empty() {
        prompt.push_str(&format!(
            "These services cannot be reached right now, so tell the owner if the answer needs \
             them: {}.\n",
            failed.join(", ")
        ));
    }
    prompt.push_str(&format!("\nThe owner's latest message:\n{message}"));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lighthouse_core::RunId;
    use lighthouse_mcp::{McpError, ToolHost};

    /// Offers a read tool and a write tool. The connector `down` cannot be reached.
    struct Offers;

    #[async_trait]
    impl ToolHostFactory for Offers {
        async fn open(&self, _: &[String], _: RunId) -> Result<Box<dyn ToolHost>, McpError> {
            unreachable!("building the assistant opens no run")
        }

        async fn offered(&self, connector: &str) -> Result<Vec<String>, McpError> {
            if connector == "down" {
                return Err(McpError::UnknownConnector(connector.to_owned()));
            }
            Ok(vec!["delete_issue".to_owned(), "list_issues".to_owned()])
        }
    }

    fn policy() -> ToolPolicy {
        let config: lighthouse_config::ConnectorConfig = toml::from_str(
            "transport = \"http\"\nurl = \"https://demo.test/mcp\"\nread_tools = [\"list_issues\"]",
        )
        .unwrap();
        ToolPolicy::new(&[("demo".to_owned(), config)].into_iter().collect())
    }

    #[tokio::test]
    async fn tools_keep_the_level_of_the_connector() {
        let agent = assistant(&Offers, &policy(), &["demo".to_owned()], "Hi")
            .await
            .unwrap();
        let level = |name: &str| {
            agent
                .tools
                .iter()
                .find(|grant| grant.name == name)
                .unwrap()
                .level
        };
        assert_eq!(level("demo.list_issues"), ToolLevel::Read);
        assert_eq!(
            level("demo.delete_issue"),
            ToolLevel::Write,
            "an unlisted tool asks first"
        );
        assert!(policy().blocked(&agent).is_empty(), "the run may start");
    }

    #[tokio::test]
    async fn a_broken_connector_is_left_out_and_named() {
        let connectors = ["demo".to_owned(), "down".to_owned()];
        let agent = assistant(&Offers, &policy(), &connectors, "Hi")
            .await
            .unwrap();
        assert!(
            !agent
                .tools
                .iter()
                .any(|grant| grant.name.starts_with("down."))
        );
        assert!(agent.prompt.contains(
            "cannot be reached right now, so tell the owner if the answer needs them: down"
        ));
    }

    #[tokio::test]
    async fn without_a_reachable_connector_there_is_no_assistant() {
        assert!(assistant(&Offers, &policy(), &[], "Hi").await.is_none());
        assert!(
            assistant(&Offers, &policy(), &["down".to_owned()], "Hi")
                .await
                .is_none()
        );
    }

    #[test]
    fn the_message_comes_last() {
        let prompt = prompt(&["demo"], &[], "Was ist offen?");
        assert!(prompt.ends_with("The owner's latest message:\nWas ist offen?"));
    }
}
