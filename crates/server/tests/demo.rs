//! Checks the files of the local demo without starting it.

use lighthouse_config::Config;
use lighthouse_core::agent::ToolLevel;
use lighthouse_core::automation::Automation;
use lighthouse_mcp::ToolPolicy;
use lighthouse_router::demo::answer;
use lighthouse_router::{ChatRequest, Message};
use lighthouse_runtime::load_agents;
use std::path::PathBuf;

fn demo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../demo")
}

#[test]
fn the_demo_configuration_loads_and_its_sources_only_read() {
    let config = Config::load(&demo().join("lighthouse.demo.toml")).unwrap();
    let policy = ToolPolicy::new(&config.connectors);
    assert!(config.sources.len() >= 12);
    for (name, source) in &config.sources {
        let resolved =
            lighthouse_sync::resolve(name, source, vec!["alex-example".to_owned()]).unwrap();
        for (collection, settings) in &resolved.collections {
            let tool = resolved.tool(&settings.tool);
            assert_eq!(policy.level(&tool), ToolLevel::Read, "{name}.{collection}");
            assert!(
                lighthouse_mcp::demo::sample(&settings.tool, &serde_json::json!({})).is_some(),
                "the demo server has {tool}"
            );
        }
    }
    let compose = std::fs::read_to_string(demo().join("compose.yml")).unwrap();
    assert!(compose.contains(":/srv/lighthouse-web:ro"));
    assert!(
        !compose.contains("dist}:"),
        "mount the parent of dist, not dist itself"
    );
    assert_eq!(
        config.paths.web.as_deref(),
        Some(std::path::Path::new("/srv/lighthouse-web/dist"))
    );
    let publishing = config.publishing.unwrap();
    assert_eq!(
        policy.level(&format!("{}.{}", publishing.connector, publishing.tool)),
        ToolLevel::Write
    );
}

#[test]
fn the_demo_agents_and_automations_are_valid() {
    let config = Config::load(&demo().join("lighthouse.demo.toml")).unwrap();
    let policy = ToolPolicy::new(&config.connectors);
    let agents = load_agents(&demo().join("agents")).unwrap();
    assert_eq!(agents.len(), 5);
    for file in agents {
        let agent = file.agent.unwrap();
        assert!(policy.blocked(&agent).is_empty(), "{}", agent.id);
    }
    for entry in std::fs::read_dir(demo().join("automations")).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        Automation::parse(&text).unwrap();
    }
}

#[test]
fn the_demo_briefs_hold_valid_documents() {
    for prompt in [
        "Write the Morning Brief.",
        "Write this week's weekly report.",
    ] {
        let request = ChatRequest {
            system: "agent".to_owned(),
            messages: vec![Message::user(prompt)],
            ..ChatRequest::default()
        };
        let (_, document) = lighthouse_desk::extract_document(&answer(&request));
        document.unwrap().unwrap();
    }
}
