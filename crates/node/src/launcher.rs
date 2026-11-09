//! Starts and resumes Claude Code sessions the server asks for.
//!
//! Only with `launch = true` in `node.toml`, and only in directories below one
//! of `launch_roots`. Each launch runs `claude -p` in headless mode as a child
//! process. Its hooks report it like every other session, so permission
//! requests still go through Lighthouse.

use crate::{NodeError, Server};
use lighthouse_sessions::{Launch, LaunchQueue, LaunchReport};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Semaphore, oneshot};
use uuid::Uuid;

/// Launches that run at the same time.
pub const MAX_RUNNING: usize = 3;
const POLL_SECONDS: u64 = 30;
const RETRY: Duration = Duration::from_secs(5);
const MAX_RESULT: usize = 20_000;

impl Server {
    async fn take_launches(&self, wait: u64) -> Result<LaunchQueue, NodeError> {
        let response = self
            .http
            .get(format!("{}/node/launches?wait={wait}", self.url))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(wait + 10))
            .send()
            .await?;
        Self::parse(response).await
    }

    async fn report_launch(&self, id: Uuid, report: &LaunchReport) -> Result<(), NodeError> {
        let response = self
            .http
            .post(format!("{}/node/launches/{id}", self.url))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(NodeError::Status(response.status()))
        }
    }
}

/// Runs launches for this machine.
#[derive(Debug)]
pub struct Launcher {
    server: Server,
    roots: Vec<PathBuf>,
    program: String,
    permits: Arc<Semaphore>,
    running: Mutex<HashMap<Uuid, oneshot::Sender<()>>>,
}

/// Expands a leading `~` to the home directory.
fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_RESULT).collect()
}

/// What Claude Code printed with `--output-format json`.
fn outcome(stdout: &str, exited_well: bool, stderr: &str) -> LaunchReport {
    let parsed: Option<Value> = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line.trim()).ok())
        .or_else(|| serde_json::from_str(stdout.trim()).ok());
    let Some(result) = parsed else {
        let text = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        return LaunchReport {
            status: "failed".to_owned(),
            session_id: None,
            result: Some(clip(text.trim())),
            cost_usd: None,
        };
    };
    let failed = !exited_well || result["is_error"].as_bool().unwrap_or(false);
    LaunchReport {
        status: if failed { "failed" } else { "succeeded" }.to_owned(),
        session_id: result["session_id"].as_str().map(str::to_owned),
        result: result["result"].as_str().map(clip),
        cost_usd: result["total_cost_usd"].as_f64(),
    }
}

impl Launcher {
    /// Creates the launcher. `program` is the Claude Code binary.
    pub fn new(server: Server, roots: &[String], program: &str) -> Self {
        Self {
            server,
            roots: roots
                .iter()
                .map(|root| expand(root))
                .filter_map(|root| root.canonicalize().ok())
                .collect(),
            program: program.to_owned(),
            permits: Arc::new(Semaphore::new(MAX_RUNNING)),
            running: Mutex::default(),
        }
    }

    /// The working directory, if it lies below one of the roots.
    fn allowed(&self, cwd: &str) -> Option<PathBuf> {
        let path = expand(cwd).canonicalize().ok()?;
        self.roots
            .iter()
            .any(|root| path.starts_with(root))
            .then_some(path)
    }

    async fn report(&self, id: Uuid, report: LaunchReport) {
        if let Err(error) = self.server.report_launch(id, &report).await {
            tracing::warn!(%error, %id, "launch not reported");
        }
    }

    /// Runs one launch to its end.
    pub async fn run_one(&self, launch: Launch) {
        let Some(cwd) = self.allowed(&launch.cwd) else {
            let report = LaunchReport {
                status: "failed".to_owned(),
                session_id: None,
                result: Some(format!(
                    "{} is not below one of the launch_roots of this machine",
                    launch.cwd
                )),
                cost_usd: None,
            };
            return self.report(launch.id, report).await;
        };
        let Ok(_permit) = self.permits.clone().acquire_owned().await else {
            return;
        };
        let (cancel, cancelled) = oneshot::channel();
        self.running
            .lock()
            .expect("running lock")
            .insert(launch.id, cancel);
        let report = self.execute(&launch, &cwd, cancelled).await;
        self.running
            .lock()
            .expect("running lock")
            .remove(&launch.id);
        self.report(launch.id, report).await;
    }

    async fn execute(
        &self,
        launch: &Launch,
        cwd: &Path,
        cancelled: oneshot::Receiver<()>,
    ) -> LaunchReport {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg("-p")
            .arg(&launch.prompt)
            .args(["--output-format", "json"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(session) = &launch.resume {
            command.args(["--resume", session]);
        }
        if let Some(model) = &launch.model {
            command.args(["--model", model]);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return LaunchReport {
                    status: "failed".to_owned(),
                    session_id: None,
                    result: Some(format!("cannot start {}: {error}", self.program)),
                    cost_usd: None,
                };
            }
        };
        self.report(
            launch.id,
            LaunchReport {
                status: "running".to_owned(),
                session_id: launch.resume.clone(),
                result: None,
                cost_usd: None,
            },
        )
        .await;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let read_out = tokio::spawn(async move {
            let mut text = String::new();
            if let Some(out) = stdout.as_mut() {
                let _ = out.read_to_string(&mut text).await;
            }
            text
        });
        let read_err = tokio::spawn(async move {
            let mut text = String::new();
            if let Some(err) = stderr.as_mut() {
                let _ = err.read_to_string(&mut text).await;
            }
            text
        });
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            _ = cancelled => {
                let _ = child.kill().await;
                return LaunchReport {
                    status: "cancelled".to_owned(),
                    session_id: launch.resume.clone(),
                    result: Some("stopped from Lighthouse".to_owned()),
                    cost_usd: None,
                };
            }
        };
        let stdout = read_out.await.unwrap_or_default();
        let stderr = read_err.await.unwrap_or_default();
        outcome(
            &stdout,
            status.is_some_and(|status| status.success()),
            &stderr,
        )
    }

    /// Stops a running launch. Returns whether it was running here.
    pub fn cancel(&self, id: Uuid) -> bool {
        self.running
            .lock()
            .expect("running lock")
            .remove(&id)
            .is_some_and(|cancel| cancel.send(()).is_ok())
    }

    /// Takes launches from the server until the process ends.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.server.take_launches(POLL_SECONDS).await {
                Ok(queue) => {
                    for id in queue.cancel {
                        self.cancel(id);
                    }
                    for launch in queue.launches {
                        let launcher = self.clone();
                        tokio::spawn(async move { launcher.run_one(launch).await });
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot fetch launches");
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::os::unix::fs::PermissionsExt as _;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn launch(cwd: &Path, resume: Option<&str>) -> Launch {
        Launch {
            id: Uuid::now_v7(),
            node_id: Uuid::now_v7(),
            cwd: cwd.to_string_lossy().into_owned(),
            prompt: "Run the tests".to_owned(),
            resume: resume.map(str::to_owned),
            model: None,
            status: "started".to_owned(),
            cancel_requested: false,
            session_id: None,
            result: None,
            cost_usd: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            finished_at: None,
        }
    }

    /// A stand-in for Claude Code that prints its arguments as the result.
    fn fake_claude(dir: &Path, body: &str) -> String {
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.to_string_lossy().into_owned()
    }

    async fn reports(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    async fn mock() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex("^/node/launches/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_launch_runs_claude_and_reports_its_result() {
        let server = mock().await;
        let root = tempfile::tempdir().unwrap();
        let program = fake_claude(
            root.path(),
            r#"echo "{\"type\":\"result\",\"is_error\":false,\"result\":\"$*\",\"session_id\":\"s9\",\"total_cost_usd\":0.25}""#,
        );
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let launcher = Launcher::new(Server::new(&server.uri(), "t"), &roots, &program);
        launcher.run_one(launch(root.path(), Some("s8"))).await;
        let sent = reports(&server).await;
        assert_eq!(sent[0]["status"], "running");
        assert_eq!(sent[0]["session_id"], "s8");
        assert_eq!(sent[1]["status"], "succeeded");
        assert_eq!(sent[1]["session_id"], "s9");
        assert_eq!(sent[1]["cost_usd"], 0.25);
        let result = sent[1]["result"].as_str().unwrap();
        assert!(
            result.contains("-p Run the tests --output-format json --resume s8"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn a_directory_outside_the_roots_is_refused() {
        let server = mock().await;
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let launcher = Launcher::new(Server::new(&server.uri(), "t"), &roots, "claude");
        launcher.run_one(launch(elsewhere.path(), None)).await;
        let sent = reports(&server).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["status"], "failed");
        let escape = root.path().join("..");
        launcher.run_one(launch(&escape, None)).await;
        assert_eq!(reports(&server).await[1]["status"], "failed");
    }

    #[tokio::test]
    async fn a_failing_or_silent_claude_ends_as_failed_and_a_cancel_stops_it() {
        let server = mock().await;
        let root = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let broken = fake_claude(root.path(), "echo 'not logged in' >&2; exit 1");
        let launcher = Launcher::new(Server::new(&server.uri(), "t"), &roots, &broken);
        launcher.run_one(launch(root.path(), None)).await;
        let sent = reports(&server).await;
        assert_eq!(sent[1]["status"], "failed");
        assert_eq!(sent[1]["result"], "not logged in");

        let slow = fake_claude(root.path(), "sleep 30");
        let launcher = Arc::new(Launcher::new(
            Server::new(&server.uri(), "t"),
            &roots,
            &slow,
        ));
        let job = launch(root.path(), None);
        let id = job.id;
        let running = launcher.clone();
        let task = tokio::spawn(async move { running.run_one(job).await });
        for _ in 0..100 {
            if launcher.cancel(id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.await.unwrap();
        let sent = reports(&server).await;
        assert_eq!(sent.last().unwrap()["status"], "cancelled");
        assert!(!launcher.cancel(id));
        let missing = Launcher::new(
            Server::new(&server.uri(), "t"),
            &roots,
            "/nonexistent/claude",
        );
        missing.run_one(launch(root.path(), None)).await;
        assert_eq!(reports(&server).await.last().unwrap()["status"], "failed");
    }

    #[test]
    fn output_is_read_from_the_last_json_line() {
        let report = outcome("noise\n{\"is_error\":true,\"result\":\"r\"}\n", true, "");
        assert_eq!(report.status, "failed");
        assert_eq!(report.result.as_deref(), Some("r"));
        assert_eq!(
            expand("~/x"),
            PathBuf::from(std::env::var("HOME").unwrap()).join("x")
        );
    }
}
