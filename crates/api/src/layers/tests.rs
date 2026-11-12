use crate::SignupMode;
use crate::tests::{Harness, TOKEN, build_harness_tweaked, call, send, signed_in};
use axum::http::StatusCode;
use lighthouse_config::{
    ConnectorAuth, ConnectorConfig, ConnectorTransport, ModelConfig, ProviderConfig, ProviderKind,
    RouteConfig,
};
use lighthouse_core::TenantId;
use lighthouse_settings::{Defaults, Options, TenantSettings};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const PASSWORD: &str = "a long enough password";

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn defaults() -> Defaults {
    let provider = ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: None,
        tasks: BTreeMap::from([(
            "text".to_owned(),
            ModelConfig {
                model: "instance-model".to_owned(),
                input_eur_per_mtok: 3.0,
                output_eur_per_mtok: 15.0,
            },
        )]),
    };
    let connector = ConnectorConfig {
        transport: ConnectorTransport::Http {
            url: "https://mcp.example.com/mcp".to_owned(),
            auth: ConnectorAuth::Bearer,
            client_id: None,
            scopes: Vec::new(),
            allow_private_addresses: false,
            headers: Default::default(),
        },
        read_tools: vec!["list_issues".to_owned()],
        draft_tools: Vec::new(),
        app_secret: None,
    };
    Defaults {
        providers: BTreeMap::from([("anthropic".to_owned(), provider)]),
        routing: BTreeMap::from([(
            "text".to_owned(),
            RouteConfig {
                provider: "anthropic".to_owned(),
                fallback: None,
            },
        )]),
        connectors: BTreeMap::from([("issues".to_owned(), connector)]),
        values: BTreeMap::new(),
        sources: BTreeMap::new(),
        oauth_apps: Vec::new(),
    }
}

/// A harness whose default tenant has instance defaults and the given options.
async fn harness(allow_private: bool, refuse: bool) -> Harness {
    let harness = build_harness_tweaked(Some(TOKEN), None, SignupMode::Invite, None, |instance| {
        if refuse {
            instance.entitlements = Arc::new(Refuse);
        }
    })
    .await;
    let store = harness.accounts.store().clone();
    let settings = TenantSettings::open(
        store,
        TenantId::DEFAULT,
        Arc::new(defaults()),
        Options {
            allow_private_endpoints: allow_private,
            ..Options::default()
        },
    )
    .await
    .unwrap();
    harness
        .tenants
        .replace(TenantId::DEFAULT, |state| state.settings = settings);
    harness
}

struct Refuse;

#[async_trait::async_trait]
impl crate::hooks::Entitlements for Refuse {
    async fn check(&self, _: TenantId, action: &crate::hooks::Action) -> Result<(), String> {
        match action {
            crate::hooks::Action::ChangeSettings { area, .. } if area == "connector" => {
                Err("connectors need a plan".to_owned())
            }
            _ => Ok(()),
        }
    }
}

async fn member(app: &crate::Router, owner: &str, email: &str) -> String {
    let (status, body) = send(
        app,
        "POST",
        "/tenant/invitations",
        Some(owner),
        Some(json!({"email": email, "role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = json_of(&body)["accept_url"].as_str().unwrap().to_owned();
    let token = url.split("token=").nth(1).unwrap().to_owned();
    let (status, body) = send(
        app,
        "POST",
        "/auth/accept",
        None,
        Some(
            json!({"token": token, "name": "Robin", "password": PASSWORD,
                    "device_name": "web", "device_kind": "web"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    json_of(&body)["token"].as_str().unwrap().to_owned()
}

async fn put(app: &crate::Router, token: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, body) = send(app, "PUT", uri, Some(token), Some(body)).await;
    (status, json_of(&body))
}

async fn get(app: &crate::Router, token: &str, uri: &str) -> Value {
    let (status, body) = call(app, "GET", uri, Some(token)).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    json_of(&body)
}

fn find<'a>(list: &'a Value, key: &str, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|entry| entry[key] == name)
        .unwrap_or_else(|| panic!("no {name} in {list}"))
}

#[tokio::test]
async fn members_read_managers_change_and_everyone_has_their_own_layer() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let robin = member(app, &owner, "robin@example.com").await;
    let view = get(app, &robin, "/settings/effective").await;
    assert_eq!(view["can_manage"], false);
    assert_eq!(view["tasks"], json!(["text"]));
    let route = find(&view["routing"], "task", "text");
    assert_eq!(
        (route["source"].as_str(), route["model"].as_str()),
        (Some("instance"), Some("instance-model"))
    );
    let anthropic = find(&view["providers"], "name", "anthropic");
    assert_eq!(anthropic["key_name"], "anthropic");
    assert!(anthropic["key"].is_null());
    let provider = json!({"kind": "openai", "base_url": "http://10.0.0.2:8000/v1"});
    let (status, _) = put(
        app,
        &robin,
        "/settings/tenant/providers/local",
        provider.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = put(app, &owner, "/settings/tenant/providers/local", provider).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let model =
        json!({"model": "local-model", "input_eur_per_mtok": 0.0, "output_eur_per_mtok": 0.0});
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/providers/local/models/text",
        model,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/routing/text",
        json!({"provider": "local", "fallback": "anthropic"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let model =
        json!({"model": "robins-model", "input_eur_per_mtok": 1.0, "output_eur_per_mtok": 2.0});
    let (status, body) = put(
        app,
        &robin,
        "/settings/me/providers/anthropic/models/text",
        model,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = put(
        app,
        &robin,
        "/settings/me/routing/text",
        json!({"provider": "anthropic"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let for_owner = get(app, &owner, "/settings/effective").await;
    let route = find(&for_owner["routing"], "task", "text");
    assert_eq!(
        (route["provider"].as_str(), route["source"].as_str()),
        (Some("local"), Some("tenant"))
    );
    assert_eq!(route["fallback_model"], "instance-model");
    let local = find(&for_owner["providers"], "name", "local");
    assert_eq!(
        (local["key_name"].as_str(), local["defined_at"].as_str()),
        (Some("provider:local"), Some("tenant"))
    );
    let anthropic = find(&for_owner["providers"], "name", "anthropic");
    assert_eq!(anthropic["models"][0]["model"], "instance-model");
    let for_robin = get(app, &robin, "/settings/effective").await;
    let route = find(&for_robin["routing"], "task", "text");
    assert_eq!(
        (route["provider"].as_str(), route["source"].as_str()),
        (Some("anthropic"), Some("personal"))
    );
    let anthropic = find(&for_robin["providers"], "name", "anthropic");
    assert_eq!(anthropic["models"][0]["model"], "robins-model");
    assert_eq!(anthropic["models"][0]["source"], "personal");

    let tenant = get(app, &robin, "/settings/tenant/entries").await;
    assert_eq!(tenant["providers"][0]["name"], "local");
    assert_eq!(tenant["routing"][0]["provider"], "local");
    let mine = get(app, &owner, "/settings/me/entries").await;
    assert!(mine["providers"].as_array().unwrap().is_empty(), "{mine}");
    let robins = get(app, &robin, "/settings/me/entries").await;
    assert_eq!(robins["layer"], "personal");
    assert_eq!(robins["providers"][0]["name"], "anthropic");

    let (status, _) = call(app, "DELETE", "/settings/me/routing/text", Some(&robin)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let for_robin = get(app, &robin, "/settings/effective").await;
    assert_eq!(
        find(&for_robin["routing"], "task", "text")["source"],
        "tenant"
    );
    let (status, body) = call(
        app,
        "DELETE",
        "/settings/tenant/providers/local",
        Some(&owner),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = call(app, "DELETE", "/settings/tenant/routing/text", Some(&owner)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        "DELETE",
        "/settings/tenant/providers/local/models/text",
        Some(&owner),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        "DELETE",
        "/settings/tenant/providers/local",
        Some(&owner),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        "DELETE",
        "/settings/tenant/providers/local",
        Some(&owner),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let changes = get(app, &owner, "/settings/tenant/changes").await;
    assert_eq!(changes[0]["action"], "delete");
    assert_eq!(changes[0]["layer"], "tenant");
    let (status, _) = call(app, "GET", "/settings/tenant/changes", Some(&robin)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mine = get(app, &robin, "/settings/me/changes?limit=5").await;
    assert!(!mine.as_array().unwrap().is_empty());

    let (status, _) = call(app, "GET", "/settings/me/entries", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(app, "GET", "/settings/others/entries", Some(&owner)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let presets = get(app, &robin, "/settings/presets").await;
    assert!(presets["providers"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn input_is_validated() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    for (uri, body, expected) in [
        (
            "/settings/tenant/providers/Bad",
            json!({"kind": "openai"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/providers/smtp",
            json!({"kind": "openai"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/providers/local",
            json!({"kind": "mock"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/providers/anthropic",
            json!({"kind": "openai"}),
            StatusCode::CONFLICT,
        ),
        (
            "/settings/tenant/providers/anthropic/models/text",
            json!({"model": "x", "input_eur_per_mtok": -1.0, "output_eur_per_mtok": 1.0}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/providers/anthropic/models/vision",
            json!({"model": "x", "input_eur_per_mtok": 1.0, "output_eur_per_mtok": 1.0}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/providers/nobody/models/text",
            json!({"model": "x", "input_eur_per_mtok": 1.0, "output_eur_per_mtok": 1.0}),
            StatusCode::NOT_FOUND,
        ),
        (
            "/settings/tenant/routing/text",
            json!({"provider": "nobody"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/routing/image",
            json!({"provider": "anthropic"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/connectors/issues",
            json!({"url": "https://other.example.com/mcp"}),
            StatusCode::CONFLICT,
        ),
        (
            "/settings/tenant/connectors/mail",
            json!({"url": "https://mail.example.com/mcp", "auth": "basic"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/connectors/mail",
            json!({"url": "https://mail.example.com/mcp", "read_tools": ["a b"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/settings/tenant/connectors/mail",
            json!({"transport": "stdio", "command": "rm"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, body) = put(app, &owner, uri, body).await;
        assert_eq!(status, expected, "{uri}: {body}");
    }
}

#[tokio::test]
async fn private_and_metadata_endpoints_are_refused() {
    let harness = harness(false, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let robin = member(app, &owner, "robin@example.com").await;
    for url in [
        "http://mcp.example.com/mcp",
        "https://127.0.0.1:8080/mcp",
        "https://169.254.169.254/latest/meta-data",
        "https://metadata.google.internal/",
        "https://10.1.2.3/mcp",
        "https://[::1]/mcp",
        "https://localhost/mcp",
        "https://user:secret@mcp.example.com/mcp",
    ] {
        for (token, layer) in [(&owner, "tenant"), (&robin, "me")] {
            let (status, body) = put(
                app,
                token,
                &format!("/settings/{layer}/connectors/mail"),
                json!({"url": url}),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
            let (status, body) = put(
                app,
                token,
                &format!("/settings/{layer}/providers/local"),
                json!({"kind": "openai", "base_url": url}),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        }
    }
    let view = get(app, &owner, "/settings/effective").await;
    assert_eq!(view["allow_private_endpoints"], false);
    assert_eq!(view["connectors"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn connectors_are_switched_and_connected_per_person() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let robin = member(app, &owner, "robin@example.com").await;
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/connectors/issues/enabled",
        json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let view = get(app, &robin, "/settings/effective").await;
    let issues = find(&view["connectors"], "name", "issues");
    assert_eq!(
        (issues["enabled"].as_bool(), issues["disabled_by"].as_str()),
        (Some(false), Some("tenant"))
    );
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/connectors/issues/enabled",
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = put(
        app,
        &robin,
        "/settings/tenant/connectors/mail",
        json!({"url": "http://10.0.0.9/mcp"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = put(
        app,
        &robin,
        "/settings/me/connectors/mail",
        json!({"url": "http://127.0.0.1:9/mcp", "auth": "oauth", "read_tools": ["list_threads"]}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = put(
        app,
        &robin,
        "/settings/me/connectors/issues/enabled",
        json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let view = get(app, &robin, "/settings/effective").await;
    let mail = find(&view["connectors"], "name", "mail");
    assert_eq!(
        (mail["source"].as_str(), mail["auth"].as_str()),
        (Some("personal"), Some("oauth"))
    );
    assert!(mail["connected"].is_null());
    assert_eq!(
        find(&view["connectors"], "name", "issues")["disabled_by"],
        "personal"
    );
    let for_owner = get(app, &owner, "/settings/effective").await;
    assert!(
        for_owner["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "mail")
    );
    assert_eq!(
        find(&for_owner["connectors"], "name", "issues")["enabled"],
        true
    );

    // Bearer tokens: the tenant's for everyone, a member's own for themselves.
    let issues = find(&for_owner["connectors"], "name", "issues");
    assert_eq!(issues["token_name"], "mcp:issues");
    let (status, _) = put(
        app,
        &owner,
        "/credentials/mcp:issues",
        json!({"value": "shared"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = put(
        app,
        &owner,
        "/me/credentials/mcp:issues",
        json!({"value": "mine"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let for_owner = get(app, &owner, "/settings/effective").await;
    assert_eq!(
        find(&for_owner["connectors"], "name", "issues")["connected"],
        "personal"
    );
    let (status, _) = put(
        app,
        &robin,
        "/settings/me/connectors/issues/enabled",
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let view = get(app, &robin, "/settings/effective").await;
    assert_eq!(
        find(&view["connectors"], "name", "issues")["connected"],
        "tenant"
    );
    assert_eq!(
        get(app, &owner, "/me/credentials").await,
        json!(["mcp:issues"])
    );
    assert_eq!(get(app, &robin, "/me/credentials").await, json!([]));
    for (name, expected) in [
        ("oauth:mail", StatusCode::UNPROCESSABLE_ENTITY),
        ("Bad%20Name", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let (status, _) = put(
            app,
            &robin,
            &format!("/me/credentials/{name}"),
            json!({"value": "x"}),
        )
        .await;
        assert_eq!(status, expected, "{name}");
    }
    let (status, _) = put(
        app,
        &robin,
        "/me/credentials/provider:x",
        json!({"value": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(app, "DELETE", "/me/credentials/mcp:issues", Some(&robin)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, "DELETE", "/me/credentials/mcp:issues", Some(&owner)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "GET", "/me/credentials", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // OAuth: members connect for themselves, never for the tenant.
    let (status, _) = call(app, "POST", "/connectors/issues/connect", Some(&robin)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(app, "POST", "/connectors/mail/connect", Some(&robin)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the flow starts personally: {body}"
    );
    let (status, body) = call(
        app,
        "POST",
        "/connectors/issues/connect?scope=personal",
        Some(&robin),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = call(
        app,
        "POST",
        "/connectors/issues/connect?scope=all",
        Some(&robin),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        app,
        "POST",
        "/connectors/issues/connect?scope=personal",
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = call(app, "DELETE", "/settings/me/connectors/mail", Some(&robin)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "DELETE", "/settings/me/connectors/mail", Some(&robin)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn entitlements_can_refuse_changes() {
    let harness = harness(true, true).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let (status, body) = put(
        app,
        &owner,
        "/settings/tenant/connectors/mail",
        json!({"url": "http://10.0.0.9/mcp"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "connectors need a plan");
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/providers/local",
        json!({"kind": "openai"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn single_settings_resolve_per_person() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let robin = member(app, &owner, "robin@example.com").await;
    let (status, _) = put(
        app,
        &robin,
        "/settings/tenant/values/timezone",
        json!("Asia/Tokyo"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = put(
        app,
        &owner,
        "/settings/tenant/values/timezone",
        json!("Asia/Tokyo"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = put(
        app,
        &robin,
        "/settings/me/values/timezone",
        json!("America/Lima"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = put(app, &robin, "/settings/me/values/language", json!("de")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let for_robin = get(app, &robin, "/settings/values").await;
    let zone = find(&for_robin, "key", "timezone");
    assert_eq!(
        (zone["value"].as_str(), zone["source"].as_str()),
        (Some("America/Lima"), Some("personal"))
    );
    assert_eq!(find(&for_robin, "key", "language")["value"], "de");
    let for_owner = get(app, &owner, "/settings/values").await;
    assert_eq!(find(&for_owner, "key", "timezone")["source"], "tenant");
    assert!(
        for_owner
            .as_array()
            .unwrap()
            .iter()
            .all(|value| value["key"] != "language")
    );
    assert_eq!(get(app, &robin, "/me").await["timezone"], "America/Lima");
    assert_eq!(get(app, &owner, "/me").await["timezone"], "Asia/Tokyo");
    let (status, _) = call(app, "DELETE", "/settings/me/values/timezone", Some(&robin)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(get(app, &robin, "/me").await["timezone"], "Asia/Tokyo");
    let (status, _) = call(app, "DELETE", "/settings/me/values/timezone", Some(&robin)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for (key, value, expected) in [
        (
            "timezone",
            json!("Mars/Base"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("language", json!("fr"), StatusCode::UNPROCESSABLE_ENTITY),
        ("server.bind", json!("0.0.0.0"), StatusCode::NOT_FOUND),
        (
            "notifications",
            json!({"deliver": ["mail:someone@example.com"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "weather",
            json!({"latitude": 1.0, "longitude": 1.0, "base_url": "https://x.example.com"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, body) = put(app, &robin, &format!("/settings/me/values/{key}"), value).await;
        assert_eq!(status, expected, "{key}: {body}");
    }
}

#[tokio::test]
async fn the_talk_agent_is_each_persons_choice() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let (status, body) = send(
        app,
        "POST",
        "/talk/threads",
        Some(&owner),
        Some(json!({"title": "Hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let thread = json_of(&body)["id"].as_str().unwrap().to_owned();
    let uri = format!("/talk/threads/{thread}/messages");
    // Without a Talk agent, the Second Brain answers at once.
    let (status, body) = send(
        app,
        "POST",
        &uri,
        Some(&owner),
        Some(json!({"text": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = put(
        app,
        &owner,
        "/settings/me/values/talk.agent",
        json!("known"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // With one, a run answers later.
    let (status, body) = send(
        app,
        "POST",
        &uri,
        Some(&owner),
        Some(json!({"text": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn rest_api_connectors_round_trip() {
    let harness = harness(true, false).await;
    let app = &harness.app;
    let owner = signed_in(app).await;
    let (status, body) = put(
        app,
        &owner,
        "/settings/tenant/connectors/crm",
        json!({
            "url": "http://10.0.0.9/openapi.yaml",
            "auth": "bearer",
            "api": {"key": "header", "key_name": "x-api-token"},
            "read_tools": ["get_deals"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let view = get(app, &owner, "/settings/effective").await;
    let crm = find(&view["connectors"], "name", "crm");
    assert_eq!(crm["transport"], "openapi");
    assert_eq!(crm["url"], "http://10.0.0.9/openapi.yaml");
    assert_eq!(crm["auth"], "bearer");
    assert_eq!(crm["token_name"], "mcp:crm");
    assert_eq!(
        crm["api"],
        json!({"key": "header", "key_name": "x-api-token"})
    );
    let (status, _) = put(
        app,
        &owner,
        "/settings/tenant/connectors/crm",
        json!({"url": "http://10.0.0.9/openapi.yaml", "auth": "oauth", "api": {"key": "bearer"}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn connectors_with_an_instance_app_need_no_client_id() {
    let harness = build_harness_tweaked(Some(TOKEN), None, SignupMode::Invite, None, |_| {}).await;
    let mut defaults = defaults();
    defaults.connectors.insert(
        "code".to_owned(),
        ConnectorConfig {
            transport: ConnectorTransport::Http {
                url: "https://api.githubcopilot.com/mcp/".to_owned(),
                auth: ConnectorAuth::Oauth,
                client_id: None,
                scopes: Vec::new(),
                allow_private_addresses: false,
                headers: Default::default(),
            },
            read_tools: Vec::new(),
            draft_tools: Vec::new(),
            app_secret: None,
        },
    );
    defaults.oauth_apps = vec![lighthouse_settings::InstanceApp {
        url: "https://api.githubcopilot.com/mcp/".to_owned(),
        client_id: "shared-app".to_owned(),
        client_secret: Some(secrecy::SecretString::from("s3")),
    }];
    let settings = TenantSettings::open(
        harness.accounts.store().clone(),
        TenantId::DEFAULT,
        Arc::new(defaults),
        Options::default(),
    )
    .await
    .unwrap();
    harness
        .tenants
        .replace(TenantId::DEFAULT, |state| state.settings = settings);
    let app = &harness.app;
    let owner = signed_in(app).await;

    let view = get(app, &owner, "/settings/effective").await;
    let code = find(&view["connectors"], "name", "code");
    assert_eq!(code["instance_app"], true);
    assert_eq!(
        code["client_id"],
        Value::Null,
        "the app's client ID is not a setting"
    );
    assert!(
        !view.to_string().contains("s3"),
        "the secret never leaves the server"
    );
    let issues = find(&view["connectors"], "name", "issues");
    assert_eq!(issues["instance_app"], false);

    let presets = get(app, &owner, "/settings/presets").await;
    let github = find(&presets["connectors"], "id", "github");
    assert_eq!(github["instance_app"], true);
    let linear = find(&presets["connectors"], "id", "linear");
    assert_eq!(linear["instance_app"], false);
}
