//! Desktop agent that connects Claude Code sessions to Lighthouse.
//!
//! Claude Code calls local HTTP hooks. The agent keeps track of sessions,
//! reports them to the server over an outbound connection, and holds
//! permission requests open until a device decides. It opens no public port.

pub mod activity;
pub mod config;
pub mod hooks;
pub mod launcher;
pub mod local;
pub mod replies;

use axum::Json;
use axum::extract::State;
use axum::routing::{get, post};
use lighthouse_sessions::{
    NewPermission, NewSessionEvent, NodeReport, NodeState, Permission, SessionMessage,
    SessionReport,
};
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
/// Time for reporting an event or fetching messages without holding a hook.
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
/// The server waits at most this long for a message.
const MAX_STOP_HOLD: Duration = Duration::from_secs(60);
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

    /// Sends what the agent found on the machine to one of the `/node` endpoints.
    pub async fn send(&self, path: &str, body: &Value) -> Result<(), NodeError> {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(60))
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(NodeError::Status(response.status()));
        }
        Ok(())
    }

    async fn events(&self, events: &[NewSessionEvent]) -> Result<(), NodeError> {
        let response = self
            .http
            .post(format!("{}/node/events", self.url))
            .bearer_auth(&self.token)
            .timeout(EVENT_TIMEOUT)
            .json(&json!({"events": events}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(NodeError::Status(response.status()));
        }
        Ok(())
    }

    async fn messages(
        &self,
        session: &str,
        wait: Duration,
    ) -> Result<Vec<SessionMessage>, NodeError> {
        let response = self
            .http
            .get(format!(
                "{}/node/sessions/{session}/messages?wait={}",
                self.url,
                wait.as_secs()
            ))
            .bearer_auth(&self.token)
            .timeout(wait + EVENT_TIMEOUT)
            .send()
            .await?;
        Self::parse(response).await
    }

    /// What Lighthouse knows for a session that starts, resumes or was compacted.
    async fn context(
        &self,
        session: &str,
        cwd: Option<&str>,
        source: &str,
    ) -> Result<String, NodeError> {
        let mut url = url::Url::parse(&format!("{}/node/sessions/", self.url))
            .and_then(|base| base.join(&format!("{}/context", encode_segment(session))))
            .map_err(|_| NodeError::Status(StatusCode::BAD_REQUEST))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("source", source);
            if let Some(cwd) = cwd {
                pairs.append_pair("cwd", cwd);
            }
        }
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .timeout(EVENT_TIMEOUT)
            .send()
            .await?;
        let context: Value = Self::parse(response).await?;
        Ok(context["text"].as_str().unwrap_or_default().to_owned())
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
    stop_hold: Duration,
    replies: Option<replies::Replies>,
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

/// Keeps a session ID inside its path segment.
fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Hands Lighthouse's context to a starting session.
fn with_context(text: &str) -> Value {
    if text.trim().is_empty() {
        return no_decision();
    }
    json!({"hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": text,
    }})
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
            stop_hold: Duration::ZERO,
            replies: None,
        }
    }

    /// Keeps a stopped session waiting this long for a message from the owner.
    /// Zero, the default, only hands over messages that are already there.
    #[must_use]
    pub fn with_stop_hold(mut self, hold: Duration) -> Self {
        self.stop_hold = hold.min(MAX_STOP_HOLD);
        self
    }

    /// Sends the texts Claude wrote between tool calls along with the hook's own events.
    #[must_use]
    pub fn with_replies(mut self, on: bool) -> Self {
        self.replies = on.then(replies::Replies::default);
        self
    }

    /// New texts of the session since the last hook, as `say` events.
    fn said(&self, hook: &Value) -> Vec<NewSessionEvent> {
        let (Some(replies), Some(session), Some(path)) = (
            &self.replies,
            hook["session_id"].as_str(),
            hook["transcript_path"].as_str(),
        ) else {
            return Vec::new();
        };
        let path = std::path::Path::new(path);
        match hook["hook_event_name"].as_str().unwrap_or_default() {
            "SessionStart" => {
                replies.start(session, path);
                Vec::new()
            }
            "PostToolUse" | "Stop" | "PreCompact" => replies
                .read_new(session, path)
                .into_iter()
                .map(|text| activity::said(session, text))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn send_event(&self, hook: &Value) {
        let mut events = self.said(hook);
        if let Some(event) = activity::event_of(hook) {
            // The last text already is the answer at the stop; show it once.
            if event.kind == "stop"
                && events
                    .last()
                    .is_some_and(|said| said.summary == event.summary)
            {
                events.pop();
            }
            events.push(event);
        }
        if events.is_empty() {
            return;
        }
        let server = self.server.clone();
        tokio::spawn(async move {
            if let Err(error) = server.events(&events).await {
                tracing::debug!(%error, "event not reported");
            }
        });
    }

    /// Hands messages that wait for a working session over at once, without waiting for it to stop.
    async fn deliver_now(&self, hook: &Value) -> Value {
        let (Some(session), Some(event)) = (
            hook["session_id"].as_str(),
            hook["hook_event_name"].as_str(),
        ) else {
            return no_decision();
        };
        if *self.paused.lock().expect("paused lock") {
            return no_decision();
        }
        match self.server.messages(session, Duration::ZERO).await {
            Ok(messages) if !messages.is_empty() => {
                let texts: Vec<String> = messages.into_iter().map(|message| message.text).collect();
                activity::context_with(event, &texts)
            }
            Ok(_) => no_decision(),
            Err(error) => {
                tracing::debug!(%error, "cannot fetch messages");
                no_decision()
            }
        }
    }

    /// Hands messages from the owner over when a session stops.
    async fn stop(&self, hook: &Value) -> Value {
        let Some(session) = hook["session_id"].as_str() else {
            return no_decision();
        };
        if *self.paused.lock().expect("paused lock") {
            return no_decision();
        }
        match self.server.messages(session, self.stop_hold).await {
            Ok(messages) if !messages.is_empty() => {
                let texts: Vec<String> = messages.into_iter().map(|message| message.text).collect();
                self.touch(hook, "running").await;
                activity::continue_with(&texts)
            }
            Ok(_) => no_decision(),
            Err(error) => {
                tracing::debug!(%error, "cannot fetch messages");
                no_decision()
            }
        }
    }

    /// Fetches the context of a starting session. Any failure leaves the
    /// session as it is.
    async fn session_context(&self, hook: &Value) -> Value {
        let Some(session) = hook["session_id"].as_str() else {
            return no_decision();
        };
        let source = hook["source"].as_str().unwrap_or("startup");
        match self
            .server
            .context(session, hook["cwd"].as_str(), source)
            .await
        {
            Ok(text) => with_context(&text),
            Err(error) => {
                tracing::debug!(%error, "no context for the session");
                no_decision()
            }
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
        self.send_event(&hook);
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
            "SessionStart" => {
                self.touch(hook, "running").await;
                self.session_context(hook).await
            }
            "PostToolUse" | "UserPromptSubmit" => {
                self.touch(hook, "running").await;
                self.deliver_now(hook).await
            }
            "PreCompact" | "PreToolUse" => {
                self.touch(hook, "running").await;
                no_decision()
            }
            "Notification" => {
                self.touch(hook, "waiting").await;
                no_decision()
            }
            "Stop" => {
                self.touch(hook, "waiting").await;
                self.stop(hook).await
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

/// How often the agent looks for sessions, skills and memories.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Reports what Claude Code keeps on this machine, now and every [`SCAN_INTERVAL`], until the process ends.
pub async fn run_scans(server: Server, home: std::path::PathBuf, memory_sync: bool) {
    let scanner = Arc::new(Mutex::new(local::Scanner::new(home)));
    let mut ticker = tokio::time::interval(SCAN_INTERVAL);
    loop {
        ticker.tick().await;
        let scanning = scanner.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut scanner = scanning.lock().expect("scanner lock");
            let sessions = scanner.sessions();
            let skills = scanner.skills(&local::Scanner::project_folders(&sessions));
            let memories = if memory_sync {
                scanner.changed_memories()
            } else {
                Vec::new()
            };
            (sessions, skills, memories)
        })
        .await;
        let Ok((sessions, skills, memories)) = found else {
            continue;
        };
        if let Err(error) = server
            .send("/node/history", &json!({"sessions": sessions}))
            .await
        {
            tracing::warn!(%error, "past sessions not reported");
        }
        if let Err(error) = server
            .send("/node/skills", &json!({"skills": skills}))
            .await
        {
            tracing::warn!(%error, "skills not reported");
        }
        if memories.is_empty() {
            continue;
        }
        let body = json!({"projects": memories.iter().map(local::ProjectMemories::to_json).collect::<Vec<_>>()});
        if let Err(error) = server.send("/node/memories", &body).await {
            tracing::warn!(%error, "memories not reported, trying again with the next scan");
            scanner
                .lock()
                .expect("scanner lock")
                .forget_memories(&memories);
        }
    }
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
        let requests: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/node/report")
            .collect();
        let body: Value = requests.last().unwrap().body_json().unwrap();
        assert_eq!(body["sessions"][0]["status"], "done");
        assert_eq!(requests.len(), 3, "every change is reported at once");
    }

    #[tokio::test]
    async fn texts_between_tool_calls_reach_the_server_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/node/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"paused": false})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/node/events"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/node/sessions/s1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/-tmp-app");
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("s1.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let path = transcript.to_string_lossy().into_owned();
        let say = |text: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#
            ) + "\n"
        };
        let agent =
            Agent::new(Server::new(&server.uri(), "token"), "mac".to_owned()).with_replies(true);
        let hook = |name: &str| {
            json!({"hook_event_name": name, "session_id": "s1", "cwd": "/tmp/app",
            "transcript_path": path, "tool_name": "Bash", "tool_input": {"command": "ls"},
            "last_assistant_message": "All green."})
        };
        agent.handle(hook("SessionStart")).await;
        std::fs::write(&transcript, say("Running the tests now.")).unwrap();
        agent.handle(hook("PostToolUse")).await;
        let mut written = std::fs::read_to_string(&transcript).unwrap();
        written.push_str(&say("All green."));
        std::fs::write(&transcript, written).unwrap();
        agent.handle(hook("Stop")).await;
        let mut sent: Vec<Value> = Vec::new();
        for _ in 0..50 {
            sent = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/node/events")
                .flat_map(|request| {
                    request.body_json::<Value>().unwrap()["events"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            if sent.iter().any(|event| event["kind"] == "stop") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let said: Vec<&str> = sent
            .iter()
            .filter(|event| event["kind"] == "say")
            .filter_map(|event| event["summary"].as_str())
            .collect();
        assert_eq!(said, ["Running the tests now."]);
        let stop = sent.iter().find(|event| event["kind"] == "stop").unwrap();
        assert_eq!(stop["summary"], "All green.");
    }

    #[tokio::test]
    async fn a_stopped_session_gets_the_owners_messages_and_tools_are_reported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/node/report"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"paused": false})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/node/events"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let message = json!([{"id": ID, "session_id": "s1", "text": "Now run the tests",
            "status": "delivered", "created_at": "2026-11-06T10:00:00Z", "delivered_at": null}]);
        Mock::given(method("GET"))
            .and(path("/node/sessions/s1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/node/sessions/s1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let agent = Agent::new(Server::new(&server.uri(), "token"), "mac".to_owned())
            .with_stop_hold(Duration::from_secs(1));
        // A working session gets the message with its next tool call, not only when it stops.
        let after_tool = agent
            .handle(
                json!({"hook_event_name": "PostToolUse", "session_id": "s1", "cwd": "/tmp/app",
                           "tool_name": "Bash", "tool_input": {"command": "ls"}}),
            )
            .await;
        assert_eq!(
            after_tool["hookSpecificOutput"]["hookEventName"],
            "PostToolUse"
        );
        assert!(
            after_tool["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("Now run the tests")
        );
        let stop = json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": "/tmp/app"});
        assert_eq!(agent.status().sessions[0].status, "running");
        assert_eq!(agent.handle(stop).await, no_decision());
        for _ in 0..50 {
            let requests = server.received_requests().await.unwrap();
            if requests
                .iter()
                .any(|request| request.url.path() == "/node/events")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no event was reported");
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

    #[tokio::test]
    async fn a_starting_session_gets_the_context_of_lighthouse() {
        let (server, agent) = mount("pending", "allowed").await;
        Mock::given(method("GET"))
            .and(path("/node/sessions/s%201/context"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "# Lighthouse"})))
            .mount(&server)
            .await;
        let start = |session: &str| {
            json!({"hook_event_name": "SessionStart", "session_id": session, "cwd": "/tmp/app",
                   "source": "compact"})
        };
        let answer = agent.handle(start("s 1")).await;
        assert_eq!(
            answer["hookSpecificOutput"]["additionalContext"],
            "# Lighthouse"
        );
        assert_eq!(
            answer["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        let asked = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|request| request.url.path().ends_with("/context"))
            .unwrap();
        let query = asked.url.query().unwrap_or_default().to_owned();
        assert!(
            query.contains("source=compact") && query.contains("cwd=%2Ftmp%2Fapp"),
            "{query}"
        );
        assert_eq!(
            agent.handle(start("s2")).await,
            json!({}),
            "no context, no decision"
        );
        assert_eq!(with_context("  "), json!({}));
        let compact =
            json!({"hook_event_name": "PreCompact", "session_id": "s2", "trigger": "auto"});
        assert_eq!(agent.handle(compact).await, json!({}));
    }
}
