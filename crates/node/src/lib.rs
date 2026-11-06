//! Desktop agent that connects Claude Code sessions to Lighthouse.
//!
//! Claude Code calls local HTTP hooks. The agent keeps track of sessions,
//! reports them to the server over an outbound connection, and holds
//! permission requests open until a device decides. It opens no public port.

pub mod config;
pub mod hooks;

use axum::Json;
use axum::extract::State;
use axum::routing::{get, post};
use lighthouse_sessions::{NewPermission, NodeReport, NodeState, Permission, SessionReport};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Claude Code gives up on the hook after 600 seconds, so answer before that.
const HOLD_LIMIT: Duration = Duration::from_secs(590);
const LONG_POLL_SECONDS: u64 = 30;
/// Sessions that ended stay in reports for one more round.
const REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Errors when talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The request failed.
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server refused.
    #[error("the server answered {0}")]
    Status(StatusCode),
}

/// Client for the server.
#[derive(Clone, Debug)]
pub struct Server {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl Server {
    /// Creates the client.
    pub fn new(url: &str, token: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http: reqwest::Client::new(),
        }
    }

    async fn parse<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, NodeError> {
        let status = response.status();
        if !status.is_success() {
            return Err(NodeError::Status(status));
        }
        Ok(response.json().await?)
    }

    async fn report(&self, report: &NodeReport) -> Result<NodeState, NodeError> {
        let response = self
            .http
            .post(format!("{}/node/report", self.url))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await?;
        Self::parse(response).await
    }

    async fn ask(&self, new: &NewPermission) -> Result<Permission, NodeError> {
        let response = self
            .http
            .post(format!("{}/node/permissions", self.url))
            .bearer_auth(&self.token)
            .json(new)
            .send()
            .await?;
        Self::parse(response).await
    }

    async fn poll(&self, id: uuid::Uuid) -> Result<Permission, NodeError> {
        let response = self
            .http
            .get(format!(
                "{}/node/permissions/{id}?wait={LONG_POLL_SECONDS}",
                self.url
            ))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse(response).await
    }
}

/// Everything the local hook server shares.
#[derive(Debug)]
pub struct Agent {
    server: Server,
    name: String,
    sessions: Mutex<BTreeMap<String, SessionReport>>,
    paused: Mutex<bool>,
    hold_limit: Duration,
}

/// State shown by `lighthouse-node status`.
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct LocalStatus {
    /// Machine name.
    pub name: String,
    /// Whether remote actions are paused.
    pub paused: bool,
    /// Known sessions.
    pub sessions: Vec<SessionReport>,
}

fn project_of(cwd: Option<&str>) -> String {
    cwd.and_then(|path| Path::new(path).file_name())
        .map_or_else(
            || "unknown".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
}

/// Reads the current branch. Failures just leave it out.
async fn branch_of(cwd: Option<&str>) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(["-C", cwd?, "branch", "--show-current"])
        .output()
        .await
        .ok()?;
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !branch.is_empty()).then_some(branch)
}

/// The answer that lets Claude Code decide by itself.
fn no_decision() -> Value {
    json!({})
}

fn decision(allow: bool) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": {"behavior": if allow { "allow" } else { "deny" }},
    }})
}

impl Agent {
    /// Creates the agent.
    pub fn new(server: Server, name: String) -> Self {
        Self {
            server,
            name,
            sessions: Mutex::default(),
            paused: Mutex::new(false),
            hold_limit: HOLD_LIMIT,
        }
    }

    /// Changes how long a permission request stays open.
    #[must_use]
    pub fn with_hold_limit(mut self, limit: Duration) -> Self {
        self.hold_limit = limit;
        self
    }

    fn snapshot(&self) -> Vec<SessionReport> {
        let sessions = self.sessions.lock().expect("sessions lock");
        sessions.values().cloned().collect()
    }

    /// Reports to the server and drops finished sessions afterwards.
    pub async fn report(&self) -> Result<(), NodeError> {
        let sessions = self.snapshot();
        let state = self
            .server
            .report(&NodeReport {
                name: self.name.clone(),
                sessions: sessions.clone(),
            })
            .await?;
        *self.paused.lock().expect("paused lock") = state.paused;
        let mut known = self.sessions.lock().expect("sessions lock");
        for sent in sessions.iter().filter(|session| session.status == "done") {
            let unchanged = known
                .get(&sent.id)
                .is_some_and(|session| session.status == "done");
            if unchanged {
                known.remove(&sent.id);
            }
        }
        Ok(())
    }

    /// Reports until the process ends.
    pub async fn run_reports(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(REPORT_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = self.report().await {
                tracing::warn!(%error, "cannot reach the server");
            }
        }
    }

    async fn touch(&self, hook: &Value, status: &str) {
        let Some(id) = hook["session_id"].as_str() else {
            return;
        };
        let cwd = hook["cwd"].as_str();
        let branch = branch_of(cwd).await;
        let summary = hook["last_assistant_message"].as_str().map(str::to_owned);
        let mut sessions = self.sessions.lock().expect("sessions lock");
        let session = sessions
            .entry(id.to_owned())
            .or_insert_with(|| SessionReport {
                id: id.to_owned(),
                project: project_of(cwd),
                branch: None,
                status: status.to_owned(),
                summary: None,
                cost_eur: 0.0,
            });
        status.clone_into(&mut session.status);
        if branch.is_some() {
            session.branch = branch;
        }
        if summary.is_some() {
            session.summary = summary;
        }
    }

    async fn ask(&self, hook: &Value) -> Result<Value, NodeError> {
        let new = NewPermission {
            session_id: hook["session_id"].as_str().unwrap_or("unknown").to_owned(),
            project: project_of(hook["cwd"].as_str()),
            tool: hook["tool_name"].as_str().unwrap_or("unknown").to_owned(),
            input: hook["tool_input"].clone(),
        };
        let mut permission = self.server.ask(&new).await?;
        let deadline = tokio::time::Instant::now() + self.hold_limit;
        while permission.status == "pending" && tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            permission =
                match tokio::time::timeout(remaining, self.server.poll(permission.id)).await {
                    Ok(result) => result?,
                    Err(_) => break,
                };
        }
        Ok(match permission.status.as_str() {
            "allowed" => decision(true),
            "denied" => decision(false),
            _ => no_decision(),
        })
    }

    /// Handles a hook call, reports the change and returns the answer for Claude Code.
    pub async fn handle(&self, hook: Value) -> Value {
        let event = hook["hook_event_name"].as_str().unwrap_or_default();
        if event == "PermissionRequest" {
            self.touch(&hook, "waiting").await;
            self.report_quietly().await;
        }
        let answer = self.apply(&hook).await;
        if !matches!(event, "PreToolUse" | "PostToolUse") {
            self.report_quietly().await;
        }
        answer
    }

    async fn report_quietly(&self) {
        if let Err(error) = self.report().await {
            tracing::debug!(%error, "report failed, the next round retries");
        }
    }

    async fn apply(&self, hook: &Value) -> Value {
        match hook["hook_event_name"].as_str().unwrap_or_default() {
            "PermissionRequest" => {
                let answer = self.ask(hook).await.unwrap_or_else(|error| {
                    tracing::warn!(%error, "permission request stays local");
                    no_decision()
                });
                self.touch(hook, "running").await;
                answer
            }
            "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => {
                self.touch(hook, "running").await;
                no_decision()
            }
            "Notification" => {
                self.touch(hook, "waiting").await;
                no_decision()
            }
            "Stop" => {
                self.touch(hook, "waiting").await;
                no_decision()
            }
            "SessionEnd" => {
                self.touch(hook, "done").await;
                no_decision()
            }
            _ => no_decision(),
        }
    }

    /// Returns the local state.
    pub fn status(&self) -> LocalStatus {
        LocalStatus {
            name: self.name.clone(),
            paused: *self.paused.lock().expect("paused lock"),
            sessions: self.snapshot(),
        }
    }
}

async fn hook(State(agent): State<Arc<Agent>>, Json(body): Json<Value>) -> Json<Value> {
    Json(agent.handle(body).await)
}

async fn status(State(agent): State<Arc<Agent>>) -> Json<LocalStatus> {
    Json(agent.status())
}

/// Builds the local HTTP server. Bind it to the loopback address only.
pub fn router(agent: Arc<Agent>) -> axum::Router {
    axum::Router::new()
        .route("/hook", post(hook))
        .route("/status", get(status))
        .with_state(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ID: &str = "019a0000-0000-7000-8000-000000000000";

    fn permission(status: &str) -> Value {
        json!({
            "id": ID,
            "node_id": ID,
            "session_id": "s1",
            "project": "billing",
            "tool": "Bash",
            "input": {},
            "status": status,
            "created_at": "2026-11-06T10:00:00Z",
            "expires_at": "2026-11-06T10:10:00Z",
            "decided_at": null,
        })
    }

    async fn mount(status: &str, later: &str) -> (MockServer, Agent) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/node/permissions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(permission(status)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("^/node/permissions/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(permission(later)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/node/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"paused": true})))
            .mount(&server)
            .await;
        let agent = Agent::new(Server::new(&server.uri(), "token"), "mac".to_owned());
        (server, agent)
    }

    fn request() -> Value {
        json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "s1",
            "cwd": "/tmp/billing",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        })
    }

    #[tokio::test]
    async fn a_decision_reaches_claude_code() {
        let (_server, agent) = mount("pending", "allowed").await;
        let answer = agent.handle(request()).await;
        assert_eq!(
            answer["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        let (_server, agent) = mount("pending", "denied").await;
        let answer = agent.handle(request()).await;
        assert_eq!(answer["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn a_paused_node_gets_a_denial_at_once() {
        let (_server, agent) = mount("denied", "denied").await;
        let answer = agent.handle(request()).await;
        assert_eq!(answer["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn an_expired_request_leaves_the_decision_to_claude_code() {
        let (_server, agent) = mount("pending", "expired").await;
        assert_eq!(agent.handle(request()).await, no_decision());
    }

    #[tokio::test]
    async fn no_answer_within_the_limit_leaves_the_decision_to_claude_code() {
        let (_server, agent) = mount("pending", "pending").await;
        let agent = agent.with_hold_limit(Duration::from_millis(50));
        assert_eq!(agent.handle(request()).await, no_decision());
    }

    #[tokio::test]
    async fn an_unreachable_server_never_allows_anything() {
        let agent = Agent::new(Server::new("http://127.0.0.1:9", "token"), "mac".to_owned());
        assert_eq!(agent.handle(request()).await, no_decision());
        assert!(agent.report().await.is_err());
    }

    #[tokio::test]
    async fn hooks_follow_a_session_to_its_end() {
        let (server, agent) = mount("pending", "allowed").await;
        let event = |name: &str| {
            json!({"hook_event_name": name, "session_id": "s1", "cwd": "/tmp/billing",
                   "last_assistant_message": "Done"})
        };
        agent.handle(event("SessionStart")).await;
        assert_eq!(agent.status().sessions[0].project, "billing");
        assert_eq!(agent.status().sessions[0].status, "running");
        agent.handle(event("Stop")).await;
        let status = agent.status();
        assert_eq!(status.sessions[0].status, "waiting");
        assert_eq!(status.sessions[0].summary.as_deref(), Some("Done"));

        agent.handle(event("SessionEnd")).await;
        assert!(
            agent.status().sessions.is_empty(),
            "sent once, then dropped"
        );
        assert!(agent.status().paused, "the server state is adopted");
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests.last().unwrap().body_json().unwrap();
        assert_eq!(body["sessions"][0]["status"], "done");
        assert_eq!(requests.len(), 3, "every change is reported at once");
    }

    #[tokio::test]
    async fn unknown_events_are_ignored() {
        let (_server, agent) = mount("pending", "allowed").await;
        let answer = agent
            .handle(json!({"hook_event_name": "Something", "session_id": "s1"}))
            .await;
        assert_eq!(answer, no_decision());
        assert!(agent.status().sessions.is_empty());
    }

    #[test]
    fn project_names_come_from_the_directory() {
        assert_eq!(project_of(Some("/srv/billing")), "billing");
        assert_eq!(project_of(None), "unknown");
    }
}
