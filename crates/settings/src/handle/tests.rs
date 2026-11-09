use super::*;
use crate::{ConnectorInput, ModelInput, ProviderInput, RouteInput};
use lighthouse_config::{ProviderConfig, ProviderKind};
use lighthouse_store::testing::fresh_store;
use std::collections::BTreeMap;

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
    Defaults {
        providers: BTreeMap::from([("anthropic".to_owned(), provider)]),
        routing: BTreeMap::from([(
            "text".to_owned(),
            RouteConfig {
                provider: "anthropic".to_owned(),
                fallback: None,
            },
        )]),
        ..Defaults::default()
    }
}

async fn user(store: &Store, email: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, 'x')")
        .bind(id)
        .bind(email)
        .execute(store.pool())
        .await
        .unwrap();
    id
}

async fn tenant(store: &Store) -> TenantId {
    let tenant = TenantId::generate();
    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'other')")
        .bind(tenant.as_uuid())
        .execute(store.pool())
        .await
        .unwrap();
    tenant
}

fn options() -> Options {
    Options {
        allow_private_endpoints: true,
        limits: SettingsLimits::default(),
    }
}

async fn settings(store: &Store, tenant: TenantId, options: Options) -> TenantSettings {
    TenantSettings::open(store.clone(), tenant, Arc::new(defaults()), options)
        .await
        .unwrap()
}

fn provider(kind: &str, base_url: Option<&str>) -> ProviderInput {
    ProviderInput {
        kind: kind.to_owned(),
        base_url: base_url.map(str::to_owned),
    }
}

fn model(name: &str) -> ModelInput {
    ModelInput {
        model: name.to_owned(),
        input_eur_per_mtok: 1.0,
        output_eur_per_mtok: 2.0,
    }
}

fn connector(url: &str) -> ConnectorInput {
    ConnectorInput {
        url: url.to_owned(),
        auth: "bearer".to_owned(),
        client_id: None,
        scopes: Vec::new(),
        read_tools: vec!["list_threads".to_owned()],
        draft_tools: Vec::new(),
        enabled: true,
    }
}

#[tokio::test]
async fn without_entries_the_instance_applies() {
    let store = fresh_store().await;
    let settings = settings(&store, TenantId::DEFAULT, options()).await;
    let effective = settings.effective(None);
    assert_eq!(effective.routing["text"].source, Layer::Instance);
    assert_eq!(settings.snapshot().revision, 0);
    assert!(!settings.refresh().await.unwrap());
    assert!(format!("{settings:?}").contains("revision"));
}

#[tokio::test]
async fn tenant_and_personal_layers_override_and_reset() {
    let store = fresh_store().await;
    let alex = user(&store, "alex@example.com").await;
    let sam = user(&store, "sam@example.com").await;
    let settings = settings(&store, TenantId::DEFAULT, options()).await;
    settings
        .set_provider(
            None,
            Some(alex),
            "local",
            &provider("openai", Some("http://10.0.0.2:8000")),
        )
        .await
        .unwrap();
    settings
        .set_model(None, Some(alex), "local", "text", &model("local-model"))
        .await
        .unwrap();
    settings
        .set_route(
            None,
            Some(alex),
            "text",
            &RouteInput {
                provider: "local".to_owned(),
                fallback: Some("anthropic".to_owned()),
            },
        )
        .await
        .unwrap();
    settings
        .set_model(
            Some(sam),
            Some(sam),
            "anthropic",
            "text",
            &model("sams-model"),
        )
        .await
        .unwrap();
    settings
        .set_route(
            Some(sam),
            Some(sam),
            "text",
            &RouteInput {
                provider: "anthropic".to_owned(),
                fallback: None,
            },
        )
        .await
        .unwrap();
    let tenant = settings.effective(None);
    assert_eq!(tenant.routing["text"].route.provider, "local");
    assert_eq!(tenant.routing["text"].source, Layer::Tenant);
    assert_eq!(
        tenant.model("anthropic", "text").unwrap().model,
        "instance-model"
    );
    let for_alex = settings.effective(Some(alex));
    assert_eq!(for_alex.routing["text"].route.provider, "local");
    let for_sam = settings.effective(Some(sam));
    assert_eq!(for_sam.routing["text"].source, Layer::Personal);
    assert_eq!(
        for_sam.model("anthropic", "text").unwrap().model,
        "sams-model"
    );
    assert_eq!(for_sam.providers["anthropic"].defined_at, Layer::Instance);
    settings
        .delete_route(Some(sam), Some(sam), "text")
        .await
        .unwrap();
    assert_eq!(
        settings.effective(Some(sam)).routing["text"].source,
        Layer::Tenant
    );
    let error = settings
        .delete_provider(None, Some(alex), "local")
        .await
        .unwrap_err();
    assert!(matches!(error, SettingsError::Conflict(_)), "{error}");
    let error = settings
        .delete_model(None, Some(alex), "local", "text")
        .await
        .unwrap_err();
    assert!(matches!(error, SettingsError::Conflict(_)), "{error}");
    settings
        .delete_route(None, Some(alex), "text")
        .await
        .unwrap();
    settings
        .delete_model(None, Some(alex), "local", "text")
        .await
        .unwrap();
    settings
        .delete_provider(None, Some(alex), "local")
        .await
        .unwrap();
    assert!(matches!(
        settings.delete_provider(None, Some(alex), "local").await,
        Err(SettingsError::NotFound(_))
    ));
    let changes = settings.changes(None, 50).await.unwrap();
    assert_eq!(changes[0].area, "provider");
    assert_eq!(changes[0].action, "delete");
    assert_eq!(changes[0].actor, Some(alex));
    assert!(settings.changes(Some(sam), 50).await.unwrap().len() >= 2);
    assert!(settings.snapshot().revision > 5);
}

#[tokio::test]
async fn lower_providers_keep_their_endpoint_and_routes_need_models() {
    let store = fresh_store().await;
    let settings = settings(&store, TenantId::DEFAULT, options()).await;
    let error = settings
        .set_provider(None, None, "anthropic", &provider("openai", None))
        .await
        .unwrap_err();
    assert!(matches!(error, SettingsError::Conflict(_)), "{error}");
    settings
        .set_provider(None, None, "anthropic", &provider("anthropic", None))
        .await
        .unwrap();
    for (name, input) in [
        ("Bad", provider("openai", None)),
        ("other", provider("mock", None)),
        ("smtp", provider("openai", None)),
        ("other", provider("openai", Some("ftp://x"))),
    ] {
        assert!(
            matches!(
                settings.set_provider(None, None, name, &input).await,
                Err(SettingsError::Invalid(_))
            ),
            "{name}"
        );
    }
    let route = |provider: &str, fallback: Option<&str>| RouteInput {
        provider: provider.to_owned(),
        fallback: fallback.map(str::to_owned),
    };
    for input in [route("nobody", None), route("anthropic", Some("anthropic"))] {
        assert!(matches!(
            settings.set_route(None, None, "text", &input).await,
            Err(SettingsError::Invalid(_))
        ));
    }
    assert!(matches!(
        settings
            .set_route(None, None, "image", &route("anthropic", None))
            .await,
        Err(SettingsError::Invalid(_))
    ));
    let mut bad = model("x");
    bad.input_eur_per_mtok = -1.0;
    assert!(
        settings
            .set_model(None, None, "anthropic", "text", &bad)
            .await
            .is_err()
    );
    assert!(matches!(
        settings
            .set_model(None, None, "nobody", "text", &model("x"))
            .await,
        Err(SettingsError::NotFound(_))
    ));
}

#[tokio::test]
async fn connectors_are_http_unique_and_switchable() {
    let store = fresh_store().await;
    let sam = user(&store, "sam@example.com").await;
    let mut instance = defaults();
    instance.connectors.insert(
        "linear".to_owned(),
        lighthouse_config::ConnectorConfig {
            transport: lighthouse_config::ConnectorTransport::Http {
                url: "https://mcp.linear.app/mcp".to_owned(),
                auth: lighthouse_config::ConnectorAuth::Oauth,
                client_id: None,
                scopes: Vec::new(),
                allow_private_addresses: false,
            },
            read_tools: Vec::new(),
            draft_tools: Vec::new(),
        },
    );
    let settings = TenantSettings::open(
        store.clone(),
        TenantId::DEFAULT,
        Arc::new(instance),
        options(),
    )
    .await
    .unwrap();
    settings
        .set_connector(None, None, "mail", &connector("http://10.0.0.3/mcp"))
        .await
        .unwrap();
    let error = settings
        .set_connector(
            Some(sam),
            Some(sam),
            "mail",
            &connector("http://10.0.0.4/mcp"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, SettingsError::Conflict(_)), "{error}");
    assert!(matches!(
        settings
            .set_connector(None, None, "linear", &connector("http://10.0.0.4/mcp"))
            .await,
        Err(SettingsError::Conflict(_))
    ));
    settings
        .set_connector(
            Some(sam),
            Some(sam),
            "calendar",
            &connector("http://10.0.0.5/mcp"),
        )
        .await
        .unwrap();
    settings
        .switch_connector(None, None, "linear", false)
        .await
        .unwrap();
    settings
        .switch_connector(Some(sam), Some(sam), "mail", false)
        .await
        .unwrap();
    settings
        .switch_connector(Some(sam), Some(sam), "calendar", false)
        .await
        .unwrap();
    let tenant = settings.effective(None);
    assert!(!tenant.connectors["linear"].enabled);
    assert!(tenant.connectors["mail"].enabled);
    assert!(!tenant.connectors.contains_key("calendar"));
    let for_sam = settings.effective(Some(sam));
    assert!(!for_sam.connectors["mail"].enabled);
    assert_eq!(for_sam.connectors["calendar"].source, Layer::Personal);
    assert!(!for_sam.connectors["calendar"].enabled);
    settings
        .switch_connector(None, None, "linear", true)
        .await
        .unwrap();
    assert!(settings.effective(None).connectors["linear"].enabled);
    assert!(matches!(
        settings
            .switch_connector(None, None, "nothing", false)
            .await,
        Err(SettingsError::NotFound(_))
    ));
    let mut both = connector("http://10.0.0.6/mcp");
    both.draft_tools = both.read_tools.clone();
    assert!(
        settings
            .set_connector(None, None, "both", &both)
            .await
            .is_err()
    );
    let mut auth = connector("http://10.0.0.6/mcp");
    auth.auth = "basic".to_owned();
    assert!(
        settings
            .set_connector(None, None, "auth", &auth)
            .await
            .is_err()
    );
    settings.delete_connector(None, None, "mail").await.unwrap();
    assert!(settings.delete_connector(None, None, "mail").await.is_err());
}

#[tokio::test]
async fn private_endpoints_are_refused_by_default() {
    let store = fresh_store().await;
    let settings = settings(
        &store,
        TenantId::DEFAULT,
        Options {
            allow_private_endpoints: false,
            ..options()
        },
    )
    .await;
    for url in [
        "http://mcp.example.com/mcp",
        "https://127.0.0.1/mcp",
        "https://169.254.169.254/latest",
        "https://localhost/mcp",
        "https://[::1]/mcp",
    ] {
        let error = settings
            .set_connector(None, None, "mail", &connector(url))
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Invalid(_)), "{url}: {error}");
        let error = settings
            .set_provider(None, None, "local", &provider("openai", Some(url)))
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Invalid(_)), "{url}: {error}");
    }
}

#[tokio::test]
async fn limits_hold_per_layer_and_per_tenant() {
    let store = fresh_store().await;
    let limits = SettingsLimits {
        providers: 1,
        models: 1,
        connectors: 1,
        sources: 1,
        files: 1,
        tenant_entries: 4,
    };
    let settings = settings(
        &store,
        TenantId::DEFAULT,
        Options {
            allow_private_endpoints: true,
            limits,
        },
    )
    .await;
    settings
        .set_provider(None, None, "one", &provider("openai", None))
        .await
        .unwrap();
    assert!(matches!(
        settings
            .set_provider(None, None, "two", &provider("openai", None))
            .await,
        Err(SettingsError::Limit(_))
    ));
    settings
        .set_model(None, None, "one", "text", &model("a"))
        .await
        .unwrap();
    settings
        .set_connector(None, None, "mail", &connector("http://10.0.0.3/mcp"))
        .await
        .unwrap();
    assert!(matches!(
        settings
            .set_connector(None, None, "other", &connector("http://10.0.0.3/mcp"))
            .await,
        Err(SettingsError::Limit(_))
    ));
    settings
        .set_value(None, None, "talk.agent", &serde_json::json!("x"))
        .await
        .unwrap();
    assert!(matches!(
        settings
            .set_value(None, None, "weather", &serde_json::json!({}))
            .await,
        Err(SettingsError::Limit(_))
    ));
    settings
        .delete_value(None, None, "talk.agent")
        .await
        .unwrap();
    assert!(
        settings
            .delete_value(None, None, "talk.agent")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tenants_never_see_each_others_entries() {
    let store = fresh_store().await;
    let other = tenant(&store).await;
    let ours = settings(&store, TenantId::DEFAULT, options()).await;
    let theirs = settings(&store, other, options()).await;
    ours.set_provider(None, None, "local", &provider("openai", None))
        .await
        .unwrap();
    ours.set_connector(None, None, "mail", &connector("http://10.0.0.3/mcp"))
        .await
        .unwrap();
    theirs.reload().await.unwrap();
    assert!(theirs.snapshot().tenant.providers.is_empty());
    assert!(theirs.snapshot().tenant.connectors.is_empty());
    assert!(matches!(
        theirs.delete_provider(None, None, "local").await,
        Err(SettingsError::NotFound(_))
    ));
    assert!(matches!(
        theirs.delete_connector(None, None, "mail").await,
        Err(SettingsError::NotFound(_))
    ));
    assert!(theirs.changes(None, 10).await.unwrap().is_empty());
    assert_eq!(ours.layer(None).providers.len(), 1);
}

#[tokio::test]
async fn another_process_sees_changes_on_refresh() {
    let store = fresh_store().await;
    let first = settings(&store, TenantId::DEFAULT, options()).await;
    let second = settings(&store, TenantId::DEFAULT, options()).await;
    first
        .set_provider(None, None, "local", &provider("openai", None))
        .await
        .unwrap();
    let before = second.snapshot();
    assert!(before.tenant.providers.is_empty());
    assert!(second.refresh().await.unwrap());
    assert!(second.snapshot().tenant.providers.contains_key("local"));
    assert!(
        before.tenant.providers.is_empty(),
        "an old snapshot stays as it was"
    );
}

#[tokio::test]
async fn members_tokens_never_reach_a_url_they_did_not_connect_to() {
    let store = fresh_store().await;
    let sam = user(&store, "sam@example.com").await;
    let robin = user(&store, "robin@example.com").await;
    let settings = settings(&store, TenantId::DEFAULT, options()).await;
    let token = |owner: Option<Uuid>, name: &'static str| {
        let store = store.clone();
        async move {
            sqlx::query(
                "INSERT INTO credentials (tenant_id, user_id, name, nonce, ciphertext) \
                 VALUES ($1, $2, $3, '\\x00', '\\x00')",
            )
            .bind(TenantId::DEFAULT.as_uuid())
            .bind(owner)
            .bind(name)
            .execute(store.pool())
            .await
            .unwrap();
        }
    };
    let count = |owner: Option<Uuid>| {
        let store = store.clone();
        async move {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM credentials WHERE user_id IS NOT DISTINCT FROM $1",
            )
            .bind(owner)
            .fetch_one(store.pool())
            .await
            .unwrap();
            count
        }
    };
    settings
        .set_connector(None, None, "mail", &connector("http://10.0.0.3/mcp"))
        .await
        .unwrap();
    token(Some(sam), "mcp:mail").await;
    token(Some(robin), "oauth:mail").await;
    token(None, "mcp:mail").await;
    settings
        .set_connector(None, None, "mail", &connector("http://10.0.0.3/mcp"))
        .await
        .unwrap();
    assert_eq!(count(Some(sam)).await, 1, "the same URL keeps them");
    settings
        .set_connector(None, None, "mail", &connector("http://10.0.0.4/mcp"))
        .await
        .unwrap();
    assert_eq!((count(Some(sam)).await, count(Some(robin)).await), (0, 0));
    assert_eq!(count(None).await, 1, "the tenant's own token stays");

    settings
        .set_connector(
            Some(sam),
            Some(sam),
            "own",
            &connector("http://10.0.0.5/mcp"),
        )
        .await
        .unwrap();
    token(Some(sam), "mcp:own").await;
    token(Some(robin), "mcp:own").await;
    settings
        .delete_connector(Some(sam), Some(sam), "own")
        .await
        .unwrap();
    assert_eq!((count(Some(sam)).await, count(Some(robin)).await), (0, 1));
    // A new tenant connector takes over a name only without old tokens.
    settings
        .set_connector(None, None, "own", &connector("http://10.0.0.6/mcp"))
        .await
        .unwrap();
    assert_eq!(count(Some(robin)).await, 0);
    token(Some(sam), "mcp:own").await;
    settings.delete_connector(None, None, "own").await.unwrap();
    assert_eq!(count(Some(sam)).await, 0);
}

#[tokio::test]
async fn a_failed_step_leaves_nothing_and_limits_hold_across_processes() {
    let store = fresh_store().await;
    let limits = SettingsLimits {
        tenant_entries: 3,
        ..SettingsLimits::default()
    };
    let options = Options {
        allow_private_endpoints: true,
        limits,
    };
    let first = settings(&store, TenantId::DEFAULT, options).await;
    let second = settings(&store, TenantId::DEFAULT, options).await;
    first
        .set_provider(None, None, "one", &provider("openai", None))
        .await
        .unwrap();
    first
        .set_provider(None, None, "two", &provider("openai", None))
        .await
        .unwrap();
    first
        .set_model(None, None, "two", "text", &model("m"))
        .await
        .unwrap();
    // Above the limit: the provider entry for the lower layer's name is rolled back too.
    let revision = first.snapshot().revision;
    assert!(matches!(
        first
            .set_model(None, None, "anthropic", "text", &model("x"))
            .await,
        Err(SettingsError::Limit(_))
    ));
    first.reload().await.unwrap();
    assert!(!first.snapshot().tenant.providers.contains_key("anthropic"));
    assert_eq!(first.snapshot().revision, revision, "nothing happened");
    // Two processes that see old snapshots still count what the other wrote.
    first.delete_model(None, None, "two", "text").await.unwrap();
    let input = provider("openai", None);
    let (a, b) = tokio::join!(
        first.set_provider(None, None, "race-a", &input),
        second.set_provider(None, None, "race-b", &input),
    );
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1);
    second.reload().await.unwrap();
    assert_eq!(second.snapshot().entries(), 3);
}

#[tokio::test]
async fn files_layer_override_and_switch_off() {
    let store = fresh_store().await;
    let sam = user(&store, "sam@example.com").await;
    let settings = settings(&store, TenantId::DEFAULT, options()).await;
    let changes = settings.subscribe();
    settings
        .set_file(None, None, FileKind::Agent, "brief", "tenant")
        .await
        .unwrap();
    assert!(changes.has_changed().unwrap());
    settings
        .set_file(Some(sam), Some(sam), FileKind::Agent, "brief", "mine")
        .await
        .unwrap();
    settings
        .set_file(Some(sam), Some(sam), FileKind::Skill, "tone", "skill")
        .await
        .unwrap();
    let key = (FileKind::Agent, "brief".to_owned());
    assert_eq!(settings.effective(None).files[&key].source, "tenant");
    let mine = settings.effective(Some(sam));
    assert_eq!(
        (mine.files[&key].source.as_str(), mine.files[&key].layer),
        ("mine", Layer::Personal)
    );
    settings
        .delete_file(Some(sam), Some(sam), FileKind::Agent, "brief")
        .await
        .unwrap();
    assert!(
        settings
            .delete_file(Some(sam), Some(sam), FileKind::Agent, "brief")
            .await
            .is_err()
    );
    settings
        .switch_file(Some(sam), Some(sam), FileKind::Agent, "brief", false)
        .await
        .unwrap();
    let mine = settings.effective(Some(sam));
    assert_eq!(mine.files[&key].disabled_by, Some(Layer::Personal));
    assert!(settings.effective(None).files[&key].enabled);
    settings
        .switch_file(Some(sam), Some(sam), FileKind::Agent, "brief", true)
        .await
        .unwrap();
    assert!(settings.effective(Some(sam)).files[&key].enabled);
    settings
        .switch_file(None, None, FileKind::Agent, "brief", false)
        .await
        .unwrap();
    assert_eq!(
        settings.effective(Some(sam)).files[&key].disabled_by,
        Some(Layer::Tenant)
    );
    settings
        .switch_file(None, None, FileKind::Automation, "built-in", false)
        .await
        .unwrap();
    let off = (FileKind::Automation, "built-in".to_owned());
    assert_eq!(settings.effective(Some(sam)).files_off[&off], Layer::Tenant);
    assert!(
        settings
            .import_file(FileKind::Automation, "old", "x")
            .await
            .unwrap()
    );
    assert!(
        !settings
            .import_file(FileKind::Automation, "old", "y")
            .await
            .unwrap()
    );
    assert_eq!(
        settings.layer(None).files[&(FileKind::Automation, "old".to_owned())].source,
        "x"
    );
}
