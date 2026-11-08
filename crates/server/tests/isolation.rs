//! Rows of one tenant cannot be read, changed or deleted through the
//! repositories of another, even with their IDs.

use base64::Engine;
use lighthouse_audit::{AuditLog, NewApproval};
use lighthouse_brain::{Brain, EntryPatch, NewEntry, Source};
use lighthouse_core::{RunId, TenantId};
use lighthouse_desk::{
    Desk, FactPatch, NewAlert, NewFact, NewPost, NewRule, NewTask, PostPatch, RulePatch, Severity,
    TaskPatch,
};
use lighthouse_sessions::{NewPermission, NodeReport, SessionReport, Sessions};
use lighthouse_store::Store;
use lighthouse_store::testing::fresh_store;
use lighthouse_sync::{OrganizationInput, SyncStore};
use lighthouse_vault::{MasterKey, Vault};
use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;

async fn two_tenants() -> (Store, TenantId, TenantId) {
    let store = fresh_store().await;
    let other = TenantId::generate();
    lighthouse_store::sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'other')")
        .bind(other.as_uuid())
        .execute(store.pool())
        .await
        .unwrap();
    (store, TenantId::DEFAULT, other)
}

#[tokio::test]
async fn the_desk_keeps_tenants_apart() {
    let (store, mine, theirs) = two_tenants().await;
    let desk = Desk::new(store.clone(), mine);
    let other = Desk::new(store, theirs);

    let task = desk
        .create_task(
            NewTask {
                title: "Mine".to_owned(),
                ..NewTask::default()
            },
            None,
        )
        .await
        .unwrap();
    assert!(other.task(task.id).await.unwrap().is_none());
    assert!(
        other
            .update_task(task.id, TaskPatch::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(other.complete_task(task.id).await.unwrap().is_none());
    assert!(!other.delete_task(task.id).await.unwrap());
    assert_eq!(other.reorder_tasks(&[task.id]).await.unwrap(), 0);

    let rule = desk
        .create_rule(NewRule {
            text: "Write short".to_owned(),
            agent_id: None,
            category: None,
            enabled: true,
        })
        .await
        .unwrap();
    assert!(other.rule(rule.id).await.unwrap().is_none());
    assert!(
        other
            .update_rule(rule.id, RulePatch::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!other.delete_rule(rule.id).await.unwrap());
    assert!(other.rules().await.unwrap().is_empty());

    let fact = desk
        .create_fact(NewFact {
            text: "Prefers mornings".to_owned(),
            category: None,
        })
        .await
        .unwrap();
    assert!(other.fact(fact.id).await.unwrap().is_none());
    assert!(
        other
            .update_fact(fact.id, FactPatch::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(other.confirm_fact(fact.id).await.unwrap().is_none());
    assert!(!other.delete_fact(fact.id).await.unwrap());

    let alert = desk
        .raise_alert(NewAlert {
            kind: "connector".to_owned(),
            key: "connector:mail".to_owned(),
            severity: Severity::Warning,
            title: "Mail is down".to_owned(),
            detail: None,
            source: None,
            value: None,
            unit: None,
        })
        .await
        .unwrap()
        .alert;
    assert!(other.alert(alert.id).await.unwrap().is_none());
    assert!(other.acknowledge_alert(alert.id).await.unwrap().is_none());
    assert!(other.resolve_alert(alert.id).await.unwrap().is_none());
    assert!(
        other
            .resolve_alert_key("connector:mail")
            .await
            .unwrap()
            .is_empty()
    );

    let post = desk
        .create_post(NewPost {
            body: "Hello".to_owned(),
            ..NewPost::default()
        })
        .await
        .unwrap();
    assert!(other.post(post.id).await.unwrap().is_none());
    assert!(
        other
            .update_post(post.id, PostPatch::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!other.delete_post(post.id).await.unwrap());

    let thread = desk.create_thread("Mine").await.unwrap();
    assert!(other.thread(thread.id).await.unwrap().is_none());
    assert!(!other.delete_thread(thread.id).await.unwrap());
    assert!(other.threads(10).await.unwrap().is_empty());

    assert!(desk.task(task.id).await.unwrap().is_some(), "still there");
    assert!(
        desk.alert(alert.id)
            .await
            .unwrap()
            .unwrap()
            .resolved_at
            .is_none()
    );
}

#[tokio::test]
async fn runs_and_approvals_stay_in_their_tenant() {
    let (store, mine, theirs) = two_tenants().await;
    let log = AuditLog::new(store.clone(), mine);
    let other = AuditLog::new(store, theirs);
    let run = RunId::new();
    log.start_run(run, "brief", "manual").await.unwrap();
    let approval = NewApproval {
        id: Uuid::now_v7(),
        tool: "mail.send".to_owned(),
        input: json!({}),
        quotes_external: false,
        reason: None,
        consulted: Vec::new(),
    };
    log.pause_run(run, "brief", &json!({}), std::slice::from_ref(&approval))
        .await
        .unwrap();
    assert!(other.run_detail(run.as_uuid()).await.unwrap().is_none());
    assert!(other.list_runs(10).await.unwrap().is_empty());
    assert!(other.list_approvals(None, 10).await.unwrap().is_empty());
    assert!(
        other
            .decide_approval(approval.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        other
            .edit_approval(approval.id, &json!({}))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        other
            .take_checkpoint(run.as_uuid())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        log.list_approvals(Some("pending"), 10).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn desktop_agents_stay_in_their_tenant() {
    let (store, mine, theirs) = two_tenants().await;
    let sessions = Sessions::new(store.clone(), mine);
    let other = Sessions::new(store, theirs);
    let node = Uuid::now_v7();
    let report = NodeReport {
        name: "mac".to_owned(),
        sessions: vec![SessionReport {
            id: "s1".to_owned(),
            project: "app".to_owned(),
            branch: None,
            status: "running".to_owned(),
            summary: None,
            cost_eur: 0.0,
        }],
    };
    sessions.report(node, &report).await.unwrap();
    let request = sessions
        .request(
            node,
            &NewPermission {
                session_id: "s1".to_owned(),
                project: "app".to_owned(),
                tool: "Bash".to_owned(),
                input: json!({}),
            },
        )
        .await
        .unwrap();
    assert!(other.nodes().await.unwrap().is_empty());
    assert!(other.session(node, "s1").await.unwrap().is_none());
    assert!(!other.set_paused(node, true).await.unwrap());
    assert!(other.decide(request.id, true).await.unwrap().is_none());
    assert!(
        other
            .node_request(node, request.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        other
            .send_message(node, "s1", "hi")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        other
            .request(
                node,
                &NewPermission {
                    session_id: "s1".to_owned(),
                    project: "app".to_owned(),
                    tool: "Bash".to_owned(),
                    input: json!({}),
                }
            )
            .await
            .is_err(),
        "a node of another tenant cannot ask"
    );
    assert_eq!(
        sessions.requests(Some("pending"), 10).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn the_cache_vault_and_brain_stay_in_their_tenant() {
    let (store, mine, theirs) = two_tenants().await;
    let sync = SyncStore::new(store.clone(), mine);
    let other_sync = SyncStore::new(store.clone(), theirs);
    let stored = sync
        .upsert(
            "issues",
            "issues",
            "issue",
            &[lighthouse_sync::MappedItem {
                external_id: "1".to_owned(),
                title: Some("Bug".to_owned()),
                ..lighthouse_sync::MappedItem::default()
            }],
            &[],
        )
        .await
        .unwrap();
    let id = stored[0].id;
    assert!(other_sync.item(id).await.unwrap().is_none());
    assert!(other_sync.raw(id).await.unwrap().is_none());
    assert!(!other_sync.snooze(id, None).await.unwrap());
    assert!(other_sync.find_external("1", None).await.unwrap().is_none());
    sync.save_organization(
        "acme",
        OrganizationInput {
            name: "Acme".to_owned(),
            color: None,
            sources: vec!["issues".to_owned()],
        },
    )
    .await
    .unwrap();
    assert!(other_sync.organizations().await.unwrap().is_empty());
    assert!(!other_sync.delete_organization("acme").await.unwrap());

    let key = MasterKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([8u8; 32]))
        .unwrap();
    let vault = Vault::new(store.clone(), mine, &key);
    let other_vault = Vault::new(store.clone(), theirs, &key);
    vault
        .put("anthropic", &SecretString::from("key".to_owned()))
        .await
        .unwrap();
    assert!(other_vault.get("anthropic").await.unwrap().is_none());
    assert!(other_vault.list().await.unwrap().is_empty());
    assert!(!other_vault.delete("anthropic").await.unwrap());

    let roots = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let brain = Brain::new(store.clone(), mine, roots.0.path().to_path_buf()).unwrap();
    let other_brain = Brain::new(store, theirs, roots.1.path().to_path_buf()).unwrap();
    let entry = brain
        .remember(NewEntry {
            kind: "note".to_owned(),
            title: "Secret plan".to_owned(),
            body: "Only for us".to_owned(),
            sources: vec![Source {
                kind: "app".to_owned(),
                reference: "test".to_owned(),
            }],
            links: Vec::new(),
        })
        .await
        .unwrap();
    assert!(other_brain.get(entry.id).await.unwrap().is_none());
    assert!(
        other_brain
            .update(entry.id, EntryPatch::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!other_brain.delete(entry.id).await.unwrap());
    assert!(other_brain.search("Secret", 5).await.unwrap().is_empty());
    assert!(brain.get(entry.id).await.unwrap().is_some());
}
