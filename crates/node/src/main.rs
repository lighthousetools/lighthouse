//! Command line of the desktop agent.

use anyhow::Context;
use clap::{Parser, Subcommand};
use lighthouse_node::config::{DEFAULT_PORT, NodeConfig, default_path};
use lighthouse_node::launcher::Launcher;
use lighthouse_node::{Agent, LocalStatus, Server, hooks, router};
use serde_json::{Value, json};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "lighthouse-node",
    version,
    about = "Desktop agent of Lighthouse"
)]
struct Cli {
    /// Settings file.
    #[arg(long, env = "LIGHTHOUSE_NODE_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connects this machine to a server.
    Pair {
        /// Server URL.
        #[arg(long)]
        server: String,
        /// Device token with kind `node`, from the apps. Prefer the environment.
        #[arg(long, env = "LIGHTHOUSE_NODE_TOKEN", hide_env_values = true)]
        token: String,
        /// Machine name.
        #[arg(long)]
        name: String,
    },
    /// Adds the hooks to the Claude Code settings.
    Install,
    /// Removes the hooks again.
    Uninstall,
    /// Runs the agent.
    Run,
    /// Shows what the running agent knows.
    Status,
    /// Pauses this machine: open requests are denied and no new ones wait.
    Stop,
}

fn claude_settings() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn read_settings(path: &PathBuf) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).context("the Claude Code settings are invalid"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.into()),
    }
}

fn write_settings(path: &PathBuf, settings: &Value) -> anyhow::Result<()> {
    if path.exists() {
        std::fs::copy(path, path.with_extension("json.lighthouse-backup"))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

fn hook_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/hook")
}

async fn run(config: NodeConfig) -> anyhow::Result<()> {
    let server = Server::new(&config.server, &config.token);
    let agent = Arc::new(
        Agent::new(server.clone(), config.name.clone())
            .with_stop_hold(std::time::Duration::from_secs(config.stop_hold_seconds))
            .with_replies(config.replies),
    );
    tokio::spawn(agent.clone().run_reports());
    if config.launch {
        let launcher = Launcher::new(server.clone(), &config.launch_roots, &config.claude);
        tokio::spawn(Arc::new(launcher).run());
        tracing::info!(roots = ?config.launch_roots, "launches enabled");
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot bind {address}"))?;
    tracing::info!(%address, "lighthouse-node started");
    axum::serve(listener, router(agent)).await?;
    Ok(())
}

async fn local_status(port: u16) -> anyhow::Result<LocalStatus> {
    let url = format!("http://127.0.0.1:{port}/status");
    let response = reqwest::get(url)
        .await
        .context("lighthouse-node is not running")?;
    Ok(response.json().await?)
}

async fn pause(config: &NodeConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = config.server.trim_end_matches('/');
    let nodes: Vec<Value> = client
        .get(format!("{base}/nodes"))
        .bearer_auth(&config.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let node = nodes
        .iter()
        .find(|node| node["name"] == config.name.as_str())
        .context("the server does not know this machine yet")?;
    let id = node["id"].as_str().context("the server sent no ID")?;
    client
        .patch(format!("{base}/nodes/{id}"))
        .bearer_auth(&config.token)
        .json(&json!({"paused": true}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let path = match cli.config {
        Some(path) => path,
        None => default_path()?,
    };
    let config = || {
        NodeConfig::load(&path).with_context(|| {
            format!(
                "cannot read {}, run `lighthouse-node pair` first",
                path.display()
            )
        })
    };
    match cli.command {
        Command::Pair {
            server,
            token,
            name,
        } => {
            let paired = NodeConfig {
                server,
                token,
                name,
                port: DEFAULT_PORT,
                stop_hold_seconds: 0,
                launch: false,
                launch_roots: Vec::new(),
                claude: "claude".to_owned(),
                replies: true,
            };
            paired.save(&path)?;
            println!("Paired. Run `lighthouse-node install`, then `lighthouse-node run`.");
        }
        Command::Install => {
            let settings_path = claude_settings()?;
            let mut settings = read_settings(&settings_path)?;
            hooks::install(&mut settings, &hook_url(config()?.port));
            write_settings(&settings_path, &settings)?;
            println!("Hooks added to {}.", settings_path.display());
        }
        Command::Uninstall => {
            let settings_path = claude_settings()?;
            let mut settings = read_settings(&settings_path)?;
            if hooks::remove(&mut settings, &hook_url(config()?.port)) {
                write_settings(&settings_path, &settings)?;
            }
            println!("Hooks removed.");
        }
        Command::Run => run(config()?).await?,
        Command::Status => {
            let status = local_status(config()?.port).await?;
            println!("{} (paused: {})", status.name, status.paused);
            for session in status.sessions {
                println!("{}  {}  {}", session.id, session.project, session.status);
            }
        }
        Command::Stop => {
            pause(&config()?).await?;
            println!("Paused. Open requests are denied.");
        }
    }
    Ok(())
}
