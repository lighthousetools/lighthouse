use super::*;
use lighthouse_mcp::{ToolHostFactory, ToolOutput};
use lighthouse_store::testing::fresh_store;
use serde_json::{Value, json};
use std::sync::Mutex;

/// Deterministic vectors: one dimension per known word.
struct FakeEmbedder {
    model: String,
    fail: Mutex<bool>,
}

impl FakeEmbedder {
    fn new(model: &str) -> Arc<Self> {
        Arc::new(Self {
            model: model.to_owned(),
            fail: Mutex::new(false),
        })
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MediaError> {
        if *self.fail.lock().unwrap() {
            return Err(MediaError::Failed("down".to_owned()));
        }
        Ok(texts
            .iter()
            .map(|text| {
                let lower = text.to_lowercase();
                ["invoice|bill", "holiday|vacation", "sourdough"]
                    .iter()
                    .map(|group| {
                        let hit = group.split('|').any(|word| lower.contains(word));
                        if hit { 1.0 } else { 0.01 }
                    })
                    .collect()
            })
            .collect())
    }
}

struct FakeMedia;

#[async_trait]
impl MediaReader for FakeMedia {
    async fn describe_image(&self, content_type: &str, bytes: &[u8]) -> Result<String, MediaError> {
        Ok(format!("A whiteboard {content_type} {}", bytes.len()))
    }

    async fn transcribe(
        &self,
        name: &str,
        _content_type: &str,
        _bytes: &[u8],
    ) -> Result<String, MediaError> {
        Ok(format!("Transcript of {name}"))
    }
}

struct Fixture {
    brain: Brain,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let brain = Brain::new(
        fresh_store().await,
        TenantId::DEFAULT,
        dir.path().join("brain"),
    )
    .unwrap();
    Fixture { brain, _dir: dir }
}

fn source() -> Source {
    Source {
        kind: "app".to_owned(),
        reference: "test".to_owned(),
    }
}

fn note(title: &str, body: &str) -> NewEntry {
    NewEntry {
        kind: "note".to_owned(),
        title: title.to_owned(),
        body: body.to_owned(),
        sources: vec![source()],
        links: Vec::new(),
    }
}

#[test]
fn fusion_prefers_items_in_both_rankings() {
    let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let merged = fuse(&[vec![a, b], vec![c, b]]);
    assert_eq!(merged[0].0, b);
    assert_eq!(merged.len(), 3);
}

#[test]
fn file_names_cannot_leave_their_directory() {
    assert_eq!(safe_file_name("../../etc/passwd").unwrap(), "passwd");
    assert_eq!(safe_file_name("..\\x.pdf").unwrap(), "x.pdf");
    assert_eq!(safe_file_name(".hidden").unwrap(), "hidden");
    assert!(safe_file_name("../").is_err());
}

#[tokio::test]
async fn resolving_rejects_paths_outside_the_root() {
    let fixture = fixture().await;
    assert!(fixture.brain.resolve("../secret.md").is_err());
    assert!(fixture.brain.resolve("/etc/passwd").is_err());
    assert!(fixture.brain.resolve("notes/a.md").is_ok());
}

#[tokio::test]
async fn an_entry_is_a_readable_file_and_needs_a_source() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let mut sourceless = note("x", "y");
    sourceless.sources.clear();
    assert!(matches!(
        brain.remember(sourceless).await,
        Err(BrainError::Invalid(_))
    ));
    let mut unknown = note("x", "y");
    unknown.kind = "secret".to_owned();
    assert!(brain.remember(unknown).await.is_err());
    assert!(brain.remember(note(" ", "y")).await.is_err());

    let entry = brain
        .remember(note("Tax logic", "Due in January"))
        .await
        .unwrap();
    let relative = brain.stored_path(entry.id).await.unwrap().unwrap();
    assert!(relative.starts_with("notes/tax-logic-"), "{relative}");
    let text = std::fs::read_to_string(brain.root.join(relative)).unwrap();
    assert!(text.contains("title: Tax logic"));
    assert!(text.ends_with("Due in January\n"));
    assert_eq!(brain.get(entry.id).await.unwrap().unwrap(), entry);
    assert_eq!(brain.get(Uuid::now_v7()).await.unwrap(), None);
}

#[tokio::test]
async fn lexical_search_works_without_an_embedding_model() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let entry = brain
        .remember(note("Tax logic", "Due in January"))
        .await
        .unwrap();
    brain
        .remember(note("Other", "Nothing related"))
        .await
        .unwrap();
    let hits = brain.search("january", 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.id, entry.id);
    assert_eq!(hits[0].entry.snippet, "Due in January");
    assert!(brain.search("unrelated-term", 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn hybrid_search_finds_meaning_without_shared_words() {
    let fixture = fixture().await;
    let brain = fixture
        .brain
        .clone()
        .with_embedder(FakeEmbedder::new("fake-1"));
    let entry = brain
        .remember(note("Payment", "The bill is overdue"))
        .await
        .unwrap();
    brain
        .remember(note("Baking", "Sourdough needs time"))
        .await
        .unwrap();
    let hits = brain.search("invoice", 1).await.unwrap();
    assert_eq!(hits[0].entry.id, entry.id);
}

#[tokio::test]
async fn a_new_embedding_model_rebuilds_the_vectors() {
    let fixture = fixture().await;
    let first = fixture
        .brain
        .clone()
        .with_embedder(FakeEmbedder::new("fake-1"));
    first.remember(note("Payment", "The bill")).await.unwrap();
    assert_eq!(first.embed_missing().await.unwrap(), 0);
    let second = fixture
        .brain
        .clone()
        .with_embedder(FakeEmbedder::new("fake-2"));
    assert_eq!(second.embed_missing().await.unwrap(), 1);
    assert_eq!(second.embed_missing().await.unwrap(), 0);
}

#[tokio::test]
async fn a_failing_embedder_does_not_lose_the_entry() {
    let fixture = fixture().await;
    let embedder = FakeEmbedder::new("fake-1");
    *embedder.fail.lock().unwrap() = true;
    let brain = fixture.brain.clone().with_embedder(embedder.clone());
    brain
        .remember(note("Payment", "The invoice"))
        .await
        .unwrap();
    assert_eq!(brain.search("invoice", 5).await.unwrap().len(), 1);
    *embedder.fail.lock().unwrap() = false;
    assert_eq!(brain.embed_missing().await.unwrap(), 1);
}

#[tokio::test]
async fn superseded_entries_leave_the_search_but_stay_readable() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let entry = brain.remember(note("Old rule", "January")).await.unwrap();
    let patch = EntryPatch {
        status: Some("superseded".to_owned()),
        ..EntryPatch::default()
    };
    let updated = brain.update(entry.id, patch).await.unwrap().unwrap();
    assert_eq!(updated.status, "superseded");
    assert!(updated.updated >= entry.updated);
    assert!(brain.search("january", 5).await.unwrap().is_empty());
    assert_eq!(brain.list(Some("note"), 5).await.unwrap().len(), 1);
    assert!(brain.list(Some("file"), 5).await.unwrap().is_empty());
    let bad = EntryPatch {
        status: Some("gone".to_owned()),
        ..EntryPatch::default()
    };
    assert!(brain.update(entry.id, bad).await.is_err());
    assert!(
        brain
            .update(Uuid::now_v7(), EntryPatch::default())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_edit_is_searchable_and_links_do_not_repeat() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let entry = brain.remember(note("Rule", "January")).await.unwrap();
    let patch = EntryPatch {
        title: Some("New rule".to_owned()),
        body: Some("February".to_owned()),
        ..EntryPatch::default()
    };
    brain.update(entry.id, patch).await.unwrap();
    assert!(brain.search("january", 5).await.unwrap().is_empty());
    assert_eq!(brain.search("february", 5).await.unwrap().len(), 1);
    brain.link(entry.id, "project:x").await.unwrap();
    let linked = brain.link(entry.id, "project:x").await.unwrap().unwrap();
    assert_eq!(linked.links, ["project:x"]);
    assert!(brain.link(Uuid::now_v7(), "x").await.unwrap().is_none());
}

#[tokio::test]
async fn wiki_links_resolve_and_show_up_as_backlinks() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let target = brain.remember(note("Tax logic", "Rules")).await.unwrap();
    let source = brain
        .remember(note("Meeting", "See [[tax logic]] for details"))
        .await
        .unwrap();
    brain.remember(note("Other", "Nothing")).await.unwrap();
    let links = brain.backlinks(target.id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].id, source.id);
    assert_eq!(
        brain.find_by_title("TAX LOGIC").await.unwrap(),
        Some(target.id)
    );
    assert_eq!(brain.find_by_title("missing").await.unwrap(), None);
}

#[tokio::test]
async fn deletion_removes_everything_but_the_fact() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    let entry = brain
        .ingest_file("notes.txt", "text/plain", b"secret plan".to_vec(), source())
        .await
        .unwrap();
    let directory = brain.root.join(brain.files_dir(entry.id));
    assert!(directory.join("notes.txt").exists());
    assert!(brain.delete(entry.id).await.unwrap());
    assert!(!directory.exists());
    assert!(!brain.delete(entry.id).await.unwrap());
    assert!(brain.search("secret", 5).await.unwrap().is_empty());
    let (chunks,): (i64,) = sqlx::query_as("SELECT count(*) FROM brain_chunks")
        .fetch_one(brain.store.pool())
        .await
        .unwrap();
    assert_eq!(chunks, 0);
    let (deletions,): (i64,) = sqlx::query_as("SELECT count(*) FROM brain_deletions")
        .fetch_one(brain.store.pool())
        .await
        .unwrap();
    assert_eq!(deletions, 1);
}

#[tokio::test]
async fn files_keep_their_original_and_become_searchable() {
    let fixture = fixture().await;
    let brain = fixture.brain.clone().with_media(Arc::new(FakeMedia));
    let image = brain
        .ingest_file("board.jpg", "image/jpeg", vec![1, 2, 3], source())
        .await
        .unwrap();
    assert_eq!(image.body, "A whiteboard image/jpeg 3");
    let (name, bytes) = brain.original(image.id).await.unwrap().unwrap();
    assert_eq!((name.as_str(), bytes), ("board.jpg", vec![1, 2, 3]));
    assert_eq!(brain.search("whiteboard", 5).await.unwrap().len(), 1);

    let audio = brain
        .ingest_file("memo.m4a", "audio/mp4", vec![1], source())
        .await
        .unwrap();
    assert_eq!(audio.body, "Transcript of memo.m4a");

    let note = brain.remember(note("x", "y")).await.unwrap();
    assert!(brain.original(note.id).await.unwrap().is_none());
}

/// A one-page PDF with the text "Lighthouse".
fn pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_owned(),
        {
            let stream = "BT /F1 18 Tf 20 100 Td (Lighthouse) Tj ET";
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            )
        },
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let start = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
        objects.len() + 1
    ));
    out.into_bytes()
}

#[tokio::test]
async fn text_inside_a_pdf_becomes_searchable() {
    let fixture = fixture().await;
    let entry = fixture
        .brain
        .ingest_file("report.pdf", "application/pdf", pdf(), source())
        .await
        .unwrap();
    assert!(entry.body.contains("Lighthouse"), "{:?}", entry.body);
}

#[tokio::test]
async fn media_without_a_model_is_refused_with_a_clear_reason() {
    let fixture = fixture().await;
    let result = fixture
        .brain
        .ingest_file("a.png", "image/png", vec![1], source())
        .await;
    assert!(result.unwrap_err().to_string().contains("no model"));
}

#[tokio::test]
async fn unsupported_or_broken_files_are_refused() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    assert!(
        brain
            .ingest_file("a.bin", "application/octet-stream", vec![0], source())
            .await
            .is_err()
    );
    assert!(
        brain
            .ingest_file("a.txt", "text/plain", vec![0xff, 0xfe], source())
            .await
            .is_err()
    );
    assert!(
        brain
            .ingest_file("a.pdf", "application/pdf", b"no pdf".to_vec(), source())
            .await
            .is_err()
    );
    assert!(brain.list(None, 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn sync_follows_the_files() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    assert_eq!(brain.sync().await.unwrap(), SyncReport::default());
    let entry = brain.remember(note("Rule", "January")).await.unwrap();
    let doomed = brain.remember(note("Doomed", "March")).await.unwrap();
    let relative = brain.stored_path(entry.id).await.unwrap().unwrap();
    let path = brain.root.join(&relative);
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("January", "February");
    std::fs::write(&path, edited).unwrap();
    let doomed_path = brain.stored_path(doomed.id).await.unwrap().unwrap();
    std::fs::remove_file(brain.root.join(doomed_path)).unwrap();
    std::fs::write(brain.root.join("broken.md"), "no frontmatter").unwrap();

    let report = brain.sync().await.unwrap();
    assert_eq!(
        report,
        SyncReport {
            indexed: 1,
            removed: 1,
            skipped: 1
        }
    );
    assert_eq!(brain.search("february", 5).await.unwrap().len(), 1);
    assert!(brain.search("march", 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_restart_rebuilds_the_lexical_index() {
    let fixture = fixture().await;
    let brain = &fixture.brain;
    brain.remember(note("Rule", "January")).await.unwrap();
    let restarted = Brain::new(brain.store.clone(), TenantId::DEFAULT, brain.root.clone()).unwrap();
    assert!(restarted.search("january", 5).await.unwrap().is_empty());
    restarted.sync().await.unwrap();
    assert_eq!(restarted.search("january", 5).await.unwrap().len(), 1);
}

struct NoTools;

#[async_trait]
impl ToolHost for NoTools {
    async fn list_tools(&self, tools: &[String]) -> Result<Vec<ToolSpec>, McpError> {
        Ok(tools
            .iter()
            .map(|name| ToolSpec {
                name: name.clone(),
                description: "outer".to_owned(),
                input_schema: json!({}),
            })
            .collect())
    }

    async fn call_tool(&self, tool: &str, _arguments: Value) -> Result<ToolOutput, McpError> {
        Ok(ToolOutput {
            text: format!("outer {tool}"),
            is_error: false,
        })
    }
}

#[async_trait]
impl ToolHostFactory for NoTools {
    async fn open(
        &self,
        _tools: &[String],
        _run: lighthouse_core::RunId,
    ) -> Result<Box<dyn ToolHost>, McpError> {
        Ok(Box::new(NoTools))
    }
}

use lighthouse_mcp::{McpError, ToolHost};
use lighthouse_router::ToolSpec;

#[tokio::test]
async fn agents_use_the_brain_through_tools() {
    let fixture = fixture().await;
    let factory = BrainToolFactory::new(fixture.brain.clone(), Arc::new(NoTools));
    let names = ["brain.search", "brain.remember", "linear.list"].map(str::to_owned);
    let host = factory
        .open(&names, lighthouse_core::RunId::new())
        .await
        .unwrap();
    let specs = host.list_tools(&names).await.unwrap();
    assert_eq!(specs.len(), 3);

    let stored = host
        .call_tool(
            "brain.remember",
            json!({"type": "note", "title": "Fact", "body": "January"}),
        )
        .await
        .unwrap();
    assert!(!stored.is_error, "{}", stored.text);
    let found = host
        .call_tool("brain.search", json!({"query": "january"}))
        .await
        .unwrap();
    assert!(found.text.contains("Fact"));

    let id = stored.text.strip_prefix("Stored as ").unwrap();
    let read = host
        .call_tool("brain.get", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(read.text, "# Fact\n\nJanuary");
    let linked = host
        .call_tool("brain.link", json!({"id": id, "link": "project:x"}))
        .await
        .unwrap();
    assert!(!linked.is_error);
    let entry = fixture
        .brain
        .get(Uuid::parse_str(id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.sources[0].kind, "agent");

    let outer = host.call_tool("linear.list", json!({})).await.unwrap();
    assert_eq!(outer.text, "outer linear.list");
}

#[tokio::test]
async fn agents_cannot_store_every_kind_or_pass_bad_input() {
    let fixture = fixture().await;
    let factory = BrainToolFactory::new(fixture.brain.clone(), Arc::new(NoTools));
    let names = ["brain.get".to_owned()];
    let host = factory
        .open(&names, lighthouse_core::RunId::new())
        .await
        .unwrap();
    for (tool, input) in [
        (
            "brain.remember",
            json!({"type": "customer", "title": "x", "body": "y"}),
        ),
        ("brain.get", json!({"id": "nonsense"})),
        ("brain.get", json!({"id": Uuid::now_v7().to_string()})),
        ("brain.search", json!({})),
        ("brain.delete", json!({})),
    ] {
        let output = host.call_tool(tool, input).await.unwrap();
        assert!(output.is_error, "{tool}");
    }
    assert!(host.call_tool("linear.list", json!({})).await.is_err());
}
