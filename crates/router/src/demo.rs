//! A provider for the local demo. It answers without a model: it calls the
//! read tools an agent has once, then writes a plausible answer that fits the
//! request, such as a brief with a structured document, a rewrite or a deck.

use crate::{Block, ChatRequest, ChatResponse, Provider, RouterError, Usage};
use async_trait::async_trait;
use serde_json::json;

/// The demo provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct DemoProvider;

fn usage() -> Usage {
    Usage {
        input_tokens: 800,
        output_tokens: 200,
    }
}

fn untrusted(text: &str) -> &str {
    let start = text.find('>').map_or(0, |index| index + 1);
    let end = text.rfind("</untrusted_content").unwrap_or(text.len());
    text.get(start..end).unwrap_or(text).trim()
}

fn latest_user_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn tool_results(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter(|block| matches!(block, Block::ToolResult { .. }))
        .count()
}

const BRIEF: &str = "# Morning Brief\n\n**Needs you**\n- Review the pull request \"Speed up the invoice export\" (#42)\n- Globex asks for an updated offer\n\n**Overnight**\n- The nightly build passed\n- Two invoices of Acme were paid\n\n**Your day**\n- 10:00 Weekly planning with Jane Doe\n\n```lighthouse-document\n{\"kind\": \"morning_brief\", \"title\": \"Morning Brief\", \"summary\": \"One review and one offer wait for you; the night was calm.\", \"sections\": [{\"key\": \"needs_you\", \"title\": \"Needs you\", \"items\": [{\"text\": \"Review the pull request Speed up the invoice export\", \"source\": {\"service\": \"github\", \"reference\": \"#42\"}}, {\"text\": \"Globex asks for an updated offer\", \"source\": {\"service\": \"mail\"}}]}, {\"key\": \"overnight\", \"title\": \"Overnight\", \"items\": [{\"text\": \"The nightly build passed\", \"source\": {\"service\": \"github\"}}, {\"text\": \"Two invoices of Acme were paid\", \"source\": {\"service\": \"bank\"}, \"numbers\": [{\"label\": \"Paid\", \"value\": 2}]}]}, {\"key\": \"your_day\", \"title\": \"Your day\", \"items\": [{\"text\": \"10:00 Weekly planning with Jane Doe\", \"source\": {\"service\": \"calendar\"}}]}], \"unavailable\": []}\n```";

const RECAP: &str = "# Weekly report\n\n**At a glance:** A steady week. Acme moved to the proposal stage, and the export got faster.\n\n**Numbers:** 9 issues done (7 the week before), 4 pull requests merged.\n\n```lighthouse-document\n{\"kind\": \"weekly_recap\", \"title\": \"Weekly report\", \"summary\": \"A steady week with more done than the week before.\", \"sections\": [{\"key\": \"numbers\", \"title\": \"Numbers\", \"items\": [{\"text\": \"Issues done\", \"source\": {\"service\": \"issues\"}, \"numbers\": [{\"label\": \"Done\", \"value\": 9, \"previous\": 7}]}, {\"text\": \"Pull requests merged\", \"source\": {\"service\": \"github\"}, \"numbers\": [{\"label\": \"Merged\", \"value\": 4, \"previous\": 3}]}]}, {\"key\": \"shipped\", \"title\": \"Shipped\", \"items\": [{\"text\": \"Faster invoice export\", \"source\": {\"service\": \"github\", \"reference\": \"#42\"}}]}, {\"key\": \"risks\", \"title\": \"Risks\", \"items\": [{\"text\": \"The Globex renewal has had no activity for 30 days\", \"source\": {\"service\": \"crm\"}}]}], \"unavailable\": []}\n```";

const DECK: &str = "# Proposal for Globex\n\nPrepared by Jane Doe\n\n---\n\n## The problem\n\n- Invoices go out late\n- Nobody sees the pipeline in one place\n\n---\n\n## Our approach\n\n- One weekly report\n- Reminders before due dates\n\n---\n\n## Next steps\n\n- Pilot with one team [TODO]\n- Decision by the end of the month";

const AUTOMATION: &str = "---\nname: Tell me about failed briefs\ntrigger:\n  event: run.failed\n  match:\n    agent_id: morning-brief\nsteps:\n  - notify:\n      to: owner\n      message: \"The Morning Brief failed: {{event.reason}}\"\n---\nTells you when the Morning Brief fails.\n";

const AGENT: &str = "---\nname: Inbox helper\nscope: org\ntrigger:\n  manual: true\ntools:\n  - demo.list_threads\n---\nSummarize the new mail and propose replies.\n";

/// Writes the final answer for a request.
pub fn answer(request: &ChatRequest) -> String {
    let system = &request.system;
    let user = latest_user_text(request);
    if system.contains("You write automation files") {
        return AUTOMATION.to_owned();
    }
    if system.contains("Available skills") && system.contains("Available tools") {
        return AGENT.to_owned();
    }
    if system.contains("Rewrite this post") {
        let text = untrusted(&user);
        let first = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(text);
        return format!(
            "{}\n\nThat is the whole point. What would you change first?",
            first.trim()
        );
    }
    if system.contains("Write a presentation") {
        return DECK.to_owned();
    }
    if system.contains("Second Brain") && request.tools.is_empty() {
        let question = user.lines().next().unwrap_or_default();
        return format!(
            "From your notes: the answer to \"{question}\" is in the entries listed below. This is the demo, so the \
             text is made up, but the sources are real entries of your Brain."
        );
    }
    let everything = format!("{system}\n{user}").to_lowercase();
    if everything.contains("morning-brief") || everything.contains("morning brief") {
        return BRIEF.to_owned();
    }
    if everything.contains("weekly-report") || everything.contains("weekly report") {
        return RECAP.to_owned();
    }
    if everything.contains("conversation") {
        return format!(
            "I looked at {} sources. Acme is in the proposal stage with an offer of 24,000 EUR, and Globex waits for \
             your answer since Tuesday. Shall I draft that reply?",
            tool_results(request)
        );
    }
    format!(
        "Done. I looked at {} sources and found nothing that needs you.",
        tool_results(request)
    )
}

#[async_trait]
impl Provider for DemoProvider {
    async fn complete(
        &self,
        _model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError> {
        let callable: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .filter(|name| *name != "load_skill" && !name.contains("propose"))
            .collect();
        if !callable.is_empty() && tool_results(request) == 0 {
            let content = callable
                .iter()
                .enumerate()
                .map(|(index, name)| Block::ToolUse {
                    id: format!("demo-{index}"),
                    name: (*name).to_owned(),
                    input: json!({}),
                })
                .collect();
            return Ok(ChatResponse {
                content,
                usage: usage(),
            });
        }
        Ok(ChatResponse::text(answer(request), usage()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolSpec};

    fn request(system: &str, user: &str, tools: &[&str]) -> ChatRequest {
        ChatRequest {
            system: system.to_owned(),
            messages: vec![Message::user(user)],
            tools: tools
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_owned(),
                    description: String::new(),
                    input_schema: json!({}),
                })
                .collect(),
            ..ChatRequest::default()
        }
    }

    #[tokio::test]
    async fn agents_first_read_then_answer() {
        let first = DemoProvider
            .complete(
                "demo",
                &request(
                    "agent",
                    "Write the Morning Brief.",
                    &["demo__list_threads", "load_skill", "memory__propose"],
                ),
            )
            .await
            .unwrap();
        let calls: Vec<_> = first
            .tool_uses()
            .map(|(_, name, _)| name.to_owned())
            .collect();
        assert_eq!(calls, ["demo__list_threads"]);
        let mut second = request("agent", "Write the Morning Brief.", &["demo__list_threads"]);
        second.messages.push(Message {
            role: crate::Role::User,
            content: vec![Block::ToolResult {
                id: "demo-0".to_owned(),
                content: "[]".to_owned(),
                is_error: false,
            }],
        });
        let report = DemoProvider
            .complete("demo", &second)
            .await
            .unwrap()
            .text_content();
        assert!(report.contains("```lighthouse-document"));
    }

    #[test]
    fn answers_fit_the_request() {
        assert!(answer(&request("You write automation files", "x", &[])).starts_with("---\nname:"));
        assert!(
            answer(&request("Available skills: Available tools:", "x", &[])).contains("trigger:")
        );
        let rewrite = answer(&request(
            "Rewrite this post",
            "<untrusted_content source=\"owner\">\nFirst line.\nSecond line.\n</untrusted_content>",
            &[],
        ));
        assert!(rewrite.starts_with("First line."));
        assert!(answer(&request("Write a presentation", "brief", &[])).contains("\n---\n"));
        assert!(
            answer(&request(
                "You answer questions from the owner's Second Brain.",
                "Who is Jane?",
                &[]
            ))
            .contains("Who is Jane?")
        );
        assert!(
            answer(&request("x", "Write this week's weekly report", &[])).contains("weekly_recap")
        );
        assert!(
            answer(&request(
                "x",
                "This run answers the owner in a conversation.",
                &[]
            ))
            .contains("Acme")
        );
        assert!(answer(&request("x", "anything", &[])).starts_with("Done."));
    }
}
