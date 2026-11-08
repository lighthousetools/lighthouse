//! Command line for Lighthouse.

mod demo;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use lighthouse_audit::{AuditLog, RunDetail};
use lighthouse_config::Config;
use lighthouse_core::TenantId;
use lighthouse_mcp::{McpConnector, ToolPolicy};
use lighthouse_runtime::load_agents;
use lighthouse_store::Store;
use lighthouse_vault::{MasterKey, Vault};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

/// Command line for Lighthouse.
#[derive(Parser)]
#[command(name = "lht", version, about)]
struct Cli {
    /// Path of the configuration file.
    #[arg(
        long,
        env = "LIGHTHOUSE_CONFIG",
        default_value = "lighthouse.toml",
        global = true
    )]
    config: PathBuf,
    /// Server URL. Defaults to the configured bind address.
    #[arg(long, env = "LHT_SERVER", global = true)]
    server: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manages encrypted keys.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Inspects agent files.
    #[command(subcommand)]
    Agents(AgentsCommand),
    /// Starts an agent on the running server.
    Run {
        /// Agent ID.
        id: String,
    },
    /// Inspects past runs.
    #[command(subcommand)]
    Runs(RunsCommand),
    /// Starts the OAuth flow of a connector.
    Connect {
        /// Connector name.
        connector: String,
    },
    /// Inspects connectors.
    #[command(subcommand)]
    Connectors(ConnectorsCommand),
    /// Checks and refreshes connected sources.
    #[command(subcommand)]
    Sources(SourcesCommand),
    /// Prepares the local demo.
    #[command(subcommand)]
    Demo(DemoCommand),
    /// Shows the unified inbox of the running server.
    Inbox {
        /// Maximum number of items.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum DemoCommand {
    /// Adds fictional demo data to the database, once.
    Seed,
    /// Runs the demo agents whose results are missing: `ready` while Ready has no
    /// proposals, and the Morning Brief and weekly report while there is no brief or recap.
    Warm,
}

#[derive(Subcommand)]
enum SourcesCommand {
    /// Resolves every source with its preset and checks that its collections only read.
    Check,
    /// Refreshes one source, or all, on the running server.
    Refresh {
        /// Source name. Leave out for all.
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectorsCommand {
    /// Lists the tools a connector offers and how they are classified.
    Tools {
        /// Connector name.
        connector: String,
    },
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Creates the master key file.
    Init,
    /// Stores a key, read from the terminal or from standard input.
    Set {
        /// Entry name, for example a provider, `smtp` or `mcp:<connector>`.
        name: String,
    },
    /// Lists entry names, never values.
    List,
    /// Deletes an entry.
    Delete {
        /// Entry name.
        name: String,
    },
}

#[derive(Subcommand)]
enum AgentsCommand {
    /// Validates all agent files. Fails if one is invalid or blocked.
    Check,
    /// Lists valid agents.
    List,
}

#[derive(Subcommand)]
enum RunsCommand {
    /// Lists the latest runs.
    List {
        /// Maximum number of runs.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Shows a run with all tool calls, tokens and costs.
    Show {
        /// Run ID.
        id: uuid::Uuid,
    },
}

async fn store(config: &Config) -> anyhow::Result<Store> {
    let store = Store::connect(config.database.url.expose_secret())
        .await
        .context("cannot connect to Postgres")?;
    store
        .migrate()
        .await
        .context("cannot migrate the database")?;
    Ok(store)
}

async fn vault(config: &Config) -> anyhow::Result<Vault> {
    let key = match (&config.vault.master_key, &config.vault.master_key_file) {
        (Some(key), _) => MasterKey::from_base64(key.expose_secret())?,
        (None, Some(path)) => MasterKey::from_file(path)?,
        (None, None) => bail!("configure vault.master_key_file or vault.master_key"),
    };
    Ok(Vault::new(store(config).await?, TenantId::DEFAULT, &key))
}

fn read_secret(name: &str) -> anyhow::Result<SecretString> {
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Value for {name}: "))?
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end().to_owned()
    };
    if value.is_empty() {
        bail!("the value must not be empty");
    }
    Ok(SecretString::from(value))
}

async fn keys(config: &Config, command: KeysCommand) -> anyhow::Result<()> {
    match command {
        KeysCommand::Init => {
            let path = config
                .vault
                .master_key_file
                .as_ref()
                .context("configure vault.master_key_file")?;
            MasterKey::create_file(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            println!(
                "created {}. Back it up: without it the vault cannot be read.",
                path.display()
            );
        }
        KeysCommand::Set { name } => {
            let value = read_secret(&name)?;
            vault(config).await?.put(&name, &value).await?;
            println!("stored {name}");
        }
        KeysCommand::List => {
            for name in vault(config).await?.list().await? {
                println!("{name}");
            }
        }
        KeysCommand::Delete { name } => {
            if !vault(config).await?.delete(&name).await? {
                bail!("no entry named {name}");
            }
            println!("deleted {name}");
        }
    }
    Ok(())
}

fn check_agents(config: &Config) -> anyhow::Result<()> {
    let policy = ToolPolicy::new(&config.connectors);
    let mut problems = 0;
    for file in load_agents(&config.paths.agents)? {
        let path = file.path.display();
        match file.agent {
            Err(error) => {
                println!("{path}: {error}");
                problems += 1;
            }
            Ok(agent) => {
                let blocked = policy.blocked(&agent);
                for tool in &blocked {
                    println!("{path}: blocked tool {}: {}", tool.tool, tool.reason);
                }
                if blocked.is_empty() {
                    println!("{path}: ok ({})", agent.id);
                }
                problems += blocked.len();
            }
        }
    }
    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    Ok(())
}

fn list_agents(path: &Path) -> anyhow::Result<()> {
    for file in load_agents(path)? {
        if let Ok(agent) = file.agent {
            println!("{}\t{}\t{:?}", agent.id, agent.name, agent.trigger);
        }
    }
    Ok(())
}

async fn connector_tools(config: &Config, connector: &str) -> anyhow::Result<()> {
    let policy = ToolPolicy::new(&config.connectors);
    let mcp = McpConnector::new(config.connectors.clone(), vault(config).await?);
    let session = mcp.session(&[format!("{connector}.any")]).await?;
    for tool in session.all_tools(connector).await? {
        let level = policy.level(&format!("{connector}.{}", tool.name));
        let summary = tool.description.lines().next().unwrap_or_default();
        println!(
            "{:<5}  {:<40}  {summary}",
            format!("{level:?}").to_lowercase(),
            tool.name
        );
    }
    Ok(())
}

fn check_sources(config: &Config) -> anyhow::Result<()> {
    let policy = ToolPolicy::new(&config.connectors);
    let mut problems = 0;
    for (name, source) in &config.sources {
        let mut accounts: Vec<String> = config
            .owner
            .accounts
            .get(&source.connector)
            .cloned()
            .into_iter()
            .collect();
        accounts.push(config.owner.email.clone());
        let resolved = match lighthouse_sync::resolve(name, source, accounts) {
            Ok(resolved) => resolved,
            Err(error) => {
                println!("{error}");
                problems += 1;
                continue;
            }
        };
        println!("{name}: {} via {}", resolved.domain, resolved.connector);
        for (collection, settings) in &resolved.collections {
            let tool = resolved.tool(&settings.tool);
            let level = policy.level(&tool);
            let ok = level == lighthouse_core::agent::ToolLevel::Read;
            println!(
                "  collection {collection:<16} {tool:<40} {}",
                if ok { "ok" } else { "not in read_tools" }
            );
            problems += usize::from(!ok);
        }
        for (action, settings) in &resolved.actions {
            let tool = resolved.tool(&settings.tool);
            let level = format!("{:?}", policy.level(&tool)).to_lowercase();
            let note = if level == "write" {
                "asks for approval"
            } else {
                "runs at once"
            };
            println!("  action     {action:<16} {tool:<40} {note}");
        }
    }
    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    Ok(())
}

async fn get(config: &Config, server: Option<String>, path: &str) -> anyhow::Result<Value> {
    let token = config
        .server
        .admin_token
        .as_ref()
        .context("set LIGHTHOUSE_SERVER__ADMIN_TOKEN")?;
    let response = reqwest::Client::new()
        .get(format!("{}{path}", server_url(config, server)))
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .context("cannot reach the server")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "server answered {status}: {}",
            body["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(body)
}

fn server_url(config: &Config, flag: Option<String>) -> String {
    flag.unwrap_or_else(|| {
        format!(
            "http://{}",
            config.server.bind.replace("0.0.0.0", "127.0.0.1")
        )
    })
}

/// Posts JSON with a token, the admin token when `token` is `None`.
async fn post_json(
    config: &Config,
    server: Option<String>,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> anyhow::Result<Value> {
    let admin = config
        .server
        .admin_token
        .as_ref()
        .context("set LIGHTHOUSE_SERVER__ADMIN_TOKEN")?;
    let response = reqwest::Client::new()
        .post(format!("{}{path}", server_url(config, server)))
        .bearer_auth(token.unwrap_or_else(|| admin.expose_secret()))
        .json(body)
        .send()
        .await
        .context("cannot reach the server")?;
    let status = response.status();
    let answer: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "server answered {status} to {path}: {}",
            answer["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(answer)
}

/// Feeds the demo machine through the node endpoints, as `lighthouse-node` would.
async fn demo_node(config: &Config, server: Option<String>) -> anyhow::Result<()> {
    let issued = post_json(
        config,
        server.clone(),
        "/auth/tokens",
        None,
        &demo::node_token_request(),
    )
    .await?;
    let token = issued["token"]
        .as_str()
        .context("the server issued no token")?;
    let now = chrono::Utc::now();
    for (path, body) in [
        ("/node/report", demo::node_report()),
        ("/node/events", demo::node_events(now)),
        ("/node/otel/v1/metrics", demo::node_metrics()),
        ("/node/permissions", demo::node_permission()),
    ] {
        post_json(config, server.clone(), path, Some(token), &body).await?;
    }
    println!("started the demo machine with two Claude Code sessions");
    Ok(())
}

async fn post(config: &Config, server: Option<String>, path: &str) -> anyhow::Result<Value> {
    let token = config
        .server
        .admin_token
        .as_ref()
        .context("set LIGHTHOUSE_SERVER__ADMIN_TOKEN")?;
    let response = reqwest::Client::new()
        .post(format!("{}{path}", server_url(config, server)))
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .context("cannot reach the server")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "server answered {status}: {}",
            body["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(body)
}

fn print_detail(detail: &RunDetail) {
    let run = &detail.run;
    println!("run      {}", run.id);
    println!("agent    {} ({})", run.agent_id, run.trigger);
    println!("status   {}", run.status);
    if let Some(reason) = &run.reason {
        println!("reason   {reason}");
    }
    println!(
        "tokens   {} in, {} out",
        run.input_tokens, run.output_tokens
    );
    println!("cost     {:.4} EUR\n", run.cost_eur);
    for step in &detail.steps {
        println!(
            "{:>3} {:<9} {:<9} {:<36} {:>6}/{:<6} {:.4} EUR {:>6} ms",
            step.seq,
            step.kind,
            step.status,
            step.name,
            step.input_tokens,
            step.output_tokens,
            step.cost_eur,
            step.duration_ms
        );
        if let Some(detail) = &step.detail {
            println!("    {detail}");
        }
    }
    if let Some(report) = &run.report {
        println!("\n{report}");
    }
}

async fn runs(config: &Config, command: RunsCommand) -> anyhow::Result<()> {
    let audit = AuditLog::new(store(config).await?, TenantId::DEFAULT);
    match command {
        RunsCommand::List { limit } => {
            for run in audit.list_runs(limit).await? {
                println!(
                    "{}  {}  {:<15} {:<20} {:.4} EUR",
                    run.id,
                    run.started_at.format("%Y-%m-%d %H:%M"),
                    run.status,
                    run.agent_id,
                    run.cost_eur
                );
            }
        }
        RunsCommand::Show { id } => match audit.run_detail(id).await? {
            Some(detail) => print_detail(&detail),
            None => bail!("no run with ID {id}"),
        },
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)
        .with_context(|| format!("cannot load {}", cli.config.display()))?;
    match cli.command {
        Command::Keys(command) => keys(&config, command).await,
        Command::Agents(AgentsCommand::Check) => check_agents(&config),
        Command::Agents(AgentsCommand::List) => list_agents(&config.paths.agents),
        Command::Run { id } => {
            let body = post(&config, cli.server, &format!("/agents/{id}/run")).await?;
            println!("started run {}", body["id"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Runs(command) => runs(&config, command).await,
        Command::Connectors(ConnectorsCommand::Tools { connector }) => {
            connector_tools(&config, &connector).await
        }
        Command::Sources(SourcesCommand::Check) => check_sources(&config),
        Command::Demo(DemoCommand::Warm) => {
            let feed = get(&config, cli.server.clone(), "/ready").await?;
            let brief = get(&config, cli.server.clone(), "/briefs/latest")
                .await
                .is_ok();
            let recaps = get(&config, cli.server.clone(), "/recaps")
                .await
                .unwrap_or_default();
            let agents = demo::warmup_agents(&feed, brief, &recaps);
            let sessions = get(&config, cli.server.clone(), "/sessions")
                .await
                .unwrap_or_default();
            let node = demo::needs_node(&sessions);
            if agents.is_empty() && !node {
                println!("The demo is warm already.");
            }
            if node {
                demo_node(&config, cli.server.clone()).await?;
            }
            for agent in agents {
                let body =
                    post(&config, cli.server.clone(), &format!("/agents/{agent}/run")).await?;
                println!(
                    "started {agent}: run {}",
                    body["id"].as_str().unwrap_or("?")
                );
            }
            Ok(())
        }
        Command::Demo(DemoCommand::Seed) => {
            let store = store(&config).await?;
            match demo::seed(&store, &config.brain.path, &config.owner).await? {
                demo::Seeded::Added => println!(
                    "Demo data added. Sign in as {} with the password `{}`.",
                    config.owner.email,
                    demo::DEMO_PASSWORD
                ),
                demo::Seeded::AlreadyThere => println!("Demo data is there already."),
            }
            Ok(())
        }
        Command::Sources(SourcesCommand::Refresh { name }) => {
            let path = match &name {
                Some(name) => format!("/connected/sources/{name}/refresh"),
                None => "/connected/refresh".to_owned(),
            };
            let body = post(&config, cli.server, &path).await?;
            let results = if body.is_array() {
                body
            } else {
                Value::Array(vec![body])
            };
            for result in results.as_array().into_iter().flatten() {
                println!(
                    "{}: {} items, {} events",
                    result["source"].as_str().unwrap_or("?"),
                    result["items"],
                    result["events"]
                );
                for error in result["errors"].as_array().into_iter().flatten() {
                    println!("  error: {}", error.as_str().unwrap_or_default());
                }
            }
            Ok(())
        }
        Command::Inbox { limit } => {
            let body = get(&config, cli.server, &format!("/inbox?limit={limit}")).await?;
            for item in body["items"].as_array().into_iter().flatten() {
                println!(
                    "{:>3}  {:<8} {:<12} {}",
                    item["importance"],
                    item["source"].as_str().unwrap_or("?"),
                    item["kind"].as_str().unwrap_or("?"),
                    item["title"].as_str().unwrap_or("(no title)")
                );
            }
            Ok(())
        }
        Command::Connect { connector } => {
            let body = post(
                &config,
                cli.server,
                &format!("/connectors/{connector}/connect"),
            )
            .await?;
            println!(
                "Open this URL to authorize {connector}:\n{}",
                body["authorize_url"].as_str().unwrap_or("?")
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
