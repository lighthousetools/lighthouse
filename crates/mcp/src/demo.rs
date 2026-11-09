//! A small MCP server with fictional sample data, for demos and tests.
//!
//! Every name, company and amount here is made up: Acme Ltd and Globex Inc
//! are the two demo companies, and Jane Doe and John Roe are their people.
//! Dates are relative to now, so the demo always looks current.

use chrono::{DateTime, Duration, Utc};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use serde_json::{Value, json};
use std::sync::Arc;

/// Serves sample data for every domain. `list_issues`, `echo` and
/// `delete_issue` stay as they were for the tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct DemoServer;

/// Tools and what they do.
pub const TOOLS: [(&str, &str); 29] = [
    ("list_issues", "Lists open issues."),
    ("echo", "Returns its arguments."),
    ("delete_issue", "Deletes an issue."),
    ("list_threads", "Lists mail threads."),
    ("list_events", "Lists calendar events."),
    ("list_tickets", "Lists issues with state and assignee."),
    ("search_pull_requests", "Searches pull requests."),
    (
        "get_pull_request_status",
        "Returns the checks of a pull request.",
    ),
    (
        "get_pull_request_files",
        "Returns the changed files of a pull request.",
    ),
    ("list_deals", "Lists the deals of a company."),
    ("list_accounts", "Lists the bank accounts of a company."),
    (
        "list_transactions",
        "Lists the bank transactions of a company.",
    ),
    ("list_invoices", "Lists the invoices of a company."),
    ("list_contracts", "Lists contracts."),
    ("list_rooms", "Lists deal rooms."),
    ("list_room_steps", "Lists the next steps of a room."),
    ("list_room_milestones", "Lists the milestones of a room."),
    ("list_holdings", "Lists the holdings of the portfolio."),
    ("list_quotes", "Lists market quotes."),
    ("create_draft", "Saves a mail draft."),
    ("send_reply", "Sends a reply."),
    ("create_event", "Creates a calendar event."),
    ("move_deal", "Moves a deal to another stage."),
    ("create_invoice_draft", "Creates an invoice draft."),
    ("add_next_step", "Adds a next step to a room."),
    ("create_post", "Publishes a post."),
    ("add_comment", "Comments on an issue."),
    ("create_pull_request_review", "Reviews a pull request."),
    ("move_event", "Moves a calendar event."),
];

fn tool(name: &'static str, description: &'static str) -> Tool {
    let schema = json!({"type": "object", "properties": {}});
    let schema = schema.as_object().cloned().unwrap_or_default();
    let tool = Tool::new(name, description, Arc::new(schema));
    // Like real servers, the demo says which of its tools only read.
    let reads = ["list_", "get_", "search_"]
        .iter()
        .any(|prefix| name.starts_with(prefix));
    let hints = rmcp::model::ToolAnnotations::new().read_only(reads);
    tool.annotate(if name.starts_with("delete_") {
        hints.destructive(true)
    } else {
        hints
    })
}

/// The fixed point of all demo times: the start of the current day in UTC, so
/// the data stays the same on every refresh of a day.
fn anchor() -> DateTime<Utc> {
    Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map_or_else(Utc::now, |start| start.and_utc())
}

fn ago(days: i64) -> String {
    (anchor() - Duration::days(days)).to_rfc3339()
}

/// Hours after the start of today.
fn ahead(hours: i64) -> String {
    (anchor() + Duration::hours(hours)).to_rfc3339()
}

fn company(arguments: &Value) -> &str {
    match arguments["company"].as_str() {
        Some("globex") => "globex",
        _ => "acme",
    }
}

fn pull(number: u64, title: &str, author: &str) -> Value {
    json!({"number": number, "title": title, "user": {"login": author}, "state": "open",
           "body": format!("{title}. Please review."), "updated_at": ago(1),
           "html_url": format!("https://github.com/example/demo/pull/{number}"),
           "repository_url": "https://api.github.com/repos/example/demo"})
}

/// Returns the sample answer of a tool, or `None` for an unknown tool.
pub fn sample(name: &str, arguments: &Value) -> Option<Value> {
    let acme = company(arguments) == "acme";
    Some(match name {
        "list_issues" => json!([
            {"id": "DEMO-1", "title": "Fix the login timeout", "priority": "high"},
            {"id": "DEMO-2", "title": "Update the release notes", "priority": "low"},
        ]),
        "echo" => arguments.clone(),
        "list_threads" => json!({"threads": [
            {"id": "t1", "url": "https://mail.example/threads/t1", "subject": "Updated offer for Globex?", "from": "John Roe <john.roe@globex.example>",
             "snippet": "Could you send us the updated offer by Friday?", "labels": ["INBOX", "IMPORTANT", "UNREAD"],
             "date": ago(1)},
            {"id": "t2", "url": "https://mail.example/threads/t2", "subject": "Invoice 2026-114 paid", "from": "Acme Accounts <accounts@acme.example>",
             "snippet": "We paid invoice 2026-114 today.", "labels": ["INBOX"], "date": ago(2)},
            {"id": "t3", "url": "https://mail.example/threads/t3", "subject": "Weekly newsletter", "from": "News <news@example.com>",
             "snippet": "Ten things to read this week.", "labels": ["INBOX", "UNREAD"], "date": ago(3)},
        ]}),
        "list_events" => json!([
            {"id": "e1", "url": "https://calendar.example/events/e1", "summary": "Weekly planning with Jane Doe", "start": ahead(10), "end": ahead(11),
             "attendees": ["jane.doe@acme.example"], "status": "confirmed"},
            {"id": "e2", "url": "https://calendar.example/events/e2", "summary": "Globex offer call", "start": ahead(24 + 14), "end": ahead(24 + 15),
             "attendees": ["john.roe@globex.example"], "status": "confirmed"},
            {"id": "e3", "url": "https://calendar.example/events/e3", "summary": "Focus: write the proposal", "start": ahead(48 + 9), "end": ahead(48 + 11),
             "status": "confirmed"},
        ]),
        "list_tickets" => json!([
            {"id": "i1", "identifier": "DEMO-1", "title": "Fix the login timeout", "status": "In Progress",
             "assignee": "Alex", "project": "Web app", "updatedAt": ago(1),
             "url": "https://linear.example/DEMO-1"},
            {"id": "i2", "identifier": "DEMO-2", "title": "Update the release notes", "status": "Todo",
             "assignee": "Alex", "project": "Web app", "updatedAt": ago(4),
             "url": "https://linear.example/DEMO-2"},
            {"id": "i3", "identifier": "DEMO-3", "title": "Export invoices as CSV", "status": "Done",
             "assignee": "Jane Doe", "project": "Billing", "updatedAt": ago(6),
             "url": "https://linear.example/DEMO-3"},
        ]),
        "search_pull_requests" => {
            let query = arguments["query"].as_str().unwrap_or_default();
            if query.contains("review-requested") {
                json!({"items": [pull(42, "Speed up the invoice export", "jane-doe")]})
            } else {
                json!({"items": [pull(41, "Add the weekly report page", "alex-example")]})
            }
        }
        "get_pull_request_status" => {
            let failing = arguments["pullNumber"].as_u64() == Some(41);
            json!({"statuses": [
                {"context": "ci", "state": if failing { "failure" } else { "success" },
                 "description": if failing { "2 tests failed" } else { "All checks passed" }},
                {"context": "Preview deployment", "state": "success",
                 "target_url": "https://preview.example/demo"},
            ]})
        }
        "get_pull_request_files" => json!([
            {"filename": "src/export.rs", "status": "modified", "additions": 24, "deletions": 9,
             "patch": "@@ -1,3 +1,4 @@\n-let rows = load();\n+let rows = load_in_batches();"},
            {"filename": "docs/export.md", "status": "modified", "additions": 3, "deletions": 1, "patch": "@@"},
        ]),
        "list_deals" => {
            if acme {
                json!([
                    {"id": "d1", "url": "https://crm.example/deals/d1", "title": "Acme rollout", "stage": "proposal", "value": "24000", "currency": "EUR",
                     "probability": "60", "owner": "Alex", "last_activity": ago(3), "updated": ago(3),
                 "next_step": "Send the updated offer", "contact": "Jane Doe", "close": ahead(24 * 20)},
                    {"id": "d2", "url": "https://crm.example/deals/d2", "title": "Acme support plan", "stage": "won", "value": "12000", "currency": "EUR",
                     "owner": "Alex", "last_activity": ago(40), "updated": ago(40)},
                    {"id": "d3", "url": "https://crm.example/deals/d3", "title": "Acme training", "stage": "won", "value": "8000", "currency": "EUR",
                     "owner": "Alex", "last_activity": ago(90), "updated": ago(90)},
                ])
            } else {
                json!([
                    {"id": "d4", "url": "https://crm.example/deals/d4", "title": "Globex renewal", "stage": "negotiation", "value": "18000", "currency": "EUR",
                     "probability": "40", "owner": "Alex", "last_activity": ago(35), "updated": ago(35),
                 "next_step": "Call John about the renewal terms", "contact": "John Roe", "close": ahead(24 * 40)},
                    {"id": "d5", "url": "https://crm.example/deals/d5", "title": "Globex pilot", "stage": "qualified", "value": "6000", "currency": "EUR",
                     "probability": "20", "owner": "Alex", "last_activity": ago(5), "updated": ago(5),
                 "next_step": "Agree on the pilot team", "contact": "John Roe"},
                ])
            }
        }
        "list_accounts" => {
            if acme {
                json!([{"id": "a1", "name": "Acme main account", "balance": "86400.00", "currency": "EUR"}])
            } else {
                json!([{"id": "a2", "name": "Globex account", "balance": "21300.00", "currency": "EUR"}])
            }
        }
        "list_transactions" => {
            let (prefix, scale) = if acme { ("acme", 1.0) } else { ("globex", 0.4) };
            json!(
                (0..8)
                    .map(|week| {
                        let income = week % 3 == 0;
                        json!({"id": format!("{prefix}-tx{week}"),
                       "label": if income { "Customer payment" } else { "Salaries and rent" },
                       "amount": if income { 9000.0 * scale } else { -7500.0 * scale },
                       "currency": "EUR", "settled_at": ago(week * 9 + 1)})
                    })
                    .collect::<Vec<_>>()
            )
        }
        "list_invoices" => {
            if acme {
                json!([
                    {"id": "inv1", "number": "2026-114", "customer": "Acme Ltd", "status": "paid", "total": "9600",
                     "currency": "EUR", "issue_date": ago(20), "due_date": ago(6), "type": "issued",
                 "format": "ZUGFeRD"},
                    {"id": "inv2", "number": "2026-121", "customer": "Acme Ltd", "status": "sent", "total": "4800",
                     "currency": "EUR", "issue_date": ago(40), "due_date": ago(10), "type": "issued",
                 "blocked_by": "Waiting for the purchase order number", "format": "XRechnung"},
                    {"id": "inv3", "number": "B-77", "customer": "Office rent", "status": "open", "total": "2100",
                     "currency": "EUR", "issue_date": ago(5), "due_date": ahead(240), "type": "received"},
                ])
            } else {
                json!([
                    {"id": "inv4", "number": "2026-118", "customer": "Globex Inc", "status": "paid", "total": "3000",
                     "currency": "EUR", "issue_date": ago(70), "due_date": ago(40), "type": "issued"},
                ])
            }
        }
        "list_contracts" => json!([
            {"id": "c1", "title": "Acme service agreement", "status": "signed", "signer": "Jane Doe",
             "expires": ahead(24 * 20)},
            {"id": "c2", "title": "Globex pilot agreement", "status": "draft", "signer": "John Roe",
             "expires": ahead(24 * 200)},
        ]),
        "list_rooms" => json!([
            {"id": "r1", "name": "Acme rollout", "phase": "proposal", "updated": ago(2)},
            {"id": "r2", "name": "Globex renewal", "phase": "negotiation", "updated": ago(12)},
        ]),
        "list_room_steps" => json!([
            {"id": "s1", "title": "Send the updated offer", "status": "open", "due": ahead(48 + 17)},
            {"id": "s2", "title": "Book the kickoff", "status": "open", "due": ahead(24 * 9)},
        ]),
        "list_room_milestones" => json!([
            {"id": "m1", "title": "First call", "date": ago(20)},
            {"id": "m2", "title": "Proposal sent", "date": ago(4)},
        ]),
        "list_holdings" => json!([
            {"symbol": "EXA", "name": "Example Corp", "value": "15400", "currency": "EUR",
             "next_earnings": ahead(24 * 12)},
            {"symbol": "SMP", "name": "Sample Industries", "value": "8200", "currency": "EUR",
             "next_earnings": ahead(24 * 70)},
        ]),
        "list_quotes" => json!([
            {"symbol": "EXA", "price": "154.00", "currency": "EUR", "time": ago(0)},
            {"symbol": "SMP", "price": "41.00", "currency": "EUR", "time": ago(0)},
            {"symbol": "TRY", "price": "12.40", "currency": "EUR", "time": ago(0)},
        ]),
        "create_draft"
        | "send_reply"
        | "create_event"
        | "move_deal"
        | "create_invoice_draft"
        | "add_next_step"
        | "create_post"
        | "add_comment"
        | "create_pull_request_review"
        | "move_event" => json!({"ok": true, "tool": name, "id": format!("{name}-1")}),
        _ => return None,
    })
}

impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            TOOLS
                .iter()
                .map(|(name, description)| tool(name, description))
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let text = |value: String| vec![ContentBlock::text(value)];
        let name = request.name.as_ref();
        if name == "delete_issue" {
            let result = CallToolResult::error(text("demo data cannot be deleted".to_owned()));
            return Ok(result.into());
        }
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        match sample(name, &arguments) {
            Some(value) => {
                let body = if name == "echo" {
                    json!(request.arguments).to_string()
                } else {
                    value.to_string()
                };
                Ok(CallToolResult::success(text(body)).into())
            }
            None => Err(ErrorData::invalid_params(
                format!("unknown tool {name}"),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_but_delete_has_sample_data() {
        for (name, _) in TOOLS {
            if name == "delete_issue" {
                assert!(sample(name, &json!({})).is_none());
                continue;
            }
            assert!(sample(name, &json!({})).is_some(), "{name}");
        }
        assert!(sample("nope", &json!({})).is_none());
        let globex = sample("list_deals", &json!({"company": "globex"})).unwrap();
        assert!(globex.to_string().contains("Globex renewal"));
        let review = sample(
            "search_pull_requests",
            &json!({"query": "review-requested:alex"}),
        )
        .unwrap();
        assert_eq!(review["items"][0]["number"], 42);
        let failing = sample("get_pull_request_status", &json!({"pullNumber": 41})).unwrap();
        assert_eq!(failing["statuses"][0]["state"], "failure");
    }

    #[test]
    fn demo_times_do_not_drift_between_refreshes() {
        let first = sample("list_events", &json!({})).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = sample("list_events", &json!({})).unwrap();
        assert_eq!(first, second);
        let steps = sample("list_room_steps", &json!({})).unwrap();
        assert!(steps[0]["due"].as_str().unwrap().contains("T17:00:00"));
    }
}
