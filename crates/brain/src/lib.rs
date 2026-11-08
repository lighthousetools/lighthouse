//! Second Brain: Markdown files with hybrid search.
//!
//! Files on disk are the truth, readable with any editor. Postgres and the
//! BM25 index are caches that [`Brain::sync`] rebuilds from the files. Search
//! merges BM25 and vector ranks with Reciprocal Rank Fusion. Without an
//! embedding model it falls back to BM25 alone.

mod entry;
mod extract;
mod lexical;
mod tools;

pub use entry::{Entry, KINDS, Source, chunks};
pub use extract::{MediaError, MediaReader};
pub use lexical::LexicalIndex;
pub use tools::BrainToolFactory;

use async_trait::async_trait;
use chrono::Utc;
use lighthouse_core::TenantId;
use lighthouse_core::agent::slugify;
use lighthouse_store::Store;
use lighthouse_store::sqlx::{self, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// Weight constant of Reciprocal Rank Fusion.
const RRF_K: f64 = 60.0;
const CANDIDATES: usize = 50;
const SNIPPET_CHARS: usize = 240;
const MAX_FILE_NAME: usize = 120;

/// Errors from the Brain.
#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    /// A file has an invalid format.
    #[error("invalid entry: {0}")]
    Format(String),
    /// The search index failed.
    #[error("search index error: {0}")]
    Index(String),
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A file operation failed.
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    /// The request is invalid.
    #[error("{0}")]
    Invalid(String),
    /// Reading a file failed.
    #[error(transparent)]
    Media(#[from] MediaError),
}

/// Turns text into vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Name of the model. Vectors of other models are not compared.
    fn model(&self) -> String;

    /// Embeds texts, returning one vector per text.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MediaError>;
}

/// A new entry.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct NewEntry {
    /// One of the entry kinds.
    #[serde(rename = "type")]
    pub kind: String,
    /// Title.
    pub title: String,
    /// Markdown text.
    pub body: String,
    /// Sources. An entry without a source is not stored.
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Links to other entries.
    #[serde(default)]
    pub links: Vec<String>,
}

/// A change to an entry.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct EntryPatch {
    /// New title.
    pub title: Option<String>,
    /// New text.
    pub body: Option<String>,
    /// `active`, `superseded` or `archived`.
    pub status: Option<String>,
    /// New links.
    pub links: Option<Vec<String>>,
}

/// An entry without its text, for lists.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct EntrySummary {
    /// Entry ID.
    pub id: Uuid,
    /// Kind of entry.
    #[serde(rename = "type")]
    pub kind: String,
    /// Title.
    pub title: String,
    /// Status.
    pub status: String,
    /// Start of the text.
    pub snippet: String,
    /// File name of the original, for uploaded files.
    pub original: Option<String>,
    /// When it last changed.
    pub updated_at: chrono::DateTime<Utc>,
}

/// One search result.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SearchHit {
    /// The entry.
    pub entry: EntrySummary,
    /// Fusion score. Only the order means anything.
    pub score: f64,
}

/// What a sync changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Entries read from new or changed files.
    pub indexed: usize,
    /// Entries whose file is gone.
    pub removed: usize,
    /// Files that could not be read. They stay untouched on disk.
    pub skipped: usize,
}

/// The Second Brain of one tenant.
#[derive(Clone)]
pub struct Brain {
    store: Store,
    tenant: TenantId,
    root: PathBuf,
    lexical: Arc<LexicalIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    media: Option<Arc<dyn MediaReader>>,
}

impl std::fmt::Debug for Brain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Brain")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn vector_literal(vector: &[f32]) -> String {
    let parts: Vec<String> = vector.iter().map(f32::to_string).collect();
    format!("[{}]", parts.join(","))
}

fn snippet(body: &str) -> String {
    body.chars().take(SNIPPET_CHARS).collect()
}

/// Merges rankings, best first. An item scores higher the earlier it appears.
fn fuse(rankings: &[Vec<Uuid>]) -> Vec<(Uuid, f64)> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut merged: Vec<_> = scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    merged
}

/// Keeps a safe file name, so that an upload cannot leave its directory.
fn safe_file_name(name: &str) -> Result<String, BrainError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .take(MAX_FILE_NAME)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        return Err(BrainError::Invalid("the file name is empty".to_owned()));
    }
    Ok(cleaned)
}

impl Brain {
    /// Opens the Brain in a directory.
    pub fn new(store: Store, tenant: TenantId, root: PathBuf) -> Result<Self, BrainError> {
        Ok(Self {
            store,
            tenant,
            root,
            lexical: Arc::new(LexicalIndex::new()?),
            embedder: None,
            media: None,
        })
    }

    /// Adds an embedding model.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Adds a reader for images and audio.
    #[must_use]
    pub fn with_media(mut self, media: Arc<dyn MediaReader>) -> Self {
        self.media = Some(media);
        self
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, BrainError> {
        let path = Path::new(relative);
        let safe = path
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
        if !safe {
            return Err(BrainError::Invalid(format!("unsafe path `{relative}`")));
        }
        Ok(self.root.join(path))
    }

    async fn write_entry(&self, relative: &str, entry: &Entry) -> Result<(), BrainError> {
        let path = self.resolve(relative)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = entry.render()?;
        let temporary = path.with_extension("md.tmp");
        tokio::fs::write(&temporary, &text).await?;
        tokio::fs::rename(&temporary, &path).await?;
        self.index_entry(relative, entry, &text).await
    }

    async fn index_entry(
        &self,
        relative: &str,
        entry: &Entry,
        text: &str,
    ) -> Result<(), BrainError> {
        let parts = chunks(&format!("{}\n\n{}", entry.title, entry.body));
        let sources = serde_json::to_value(&entry.sources)
            .map_err(|error| BrainError::Format(error.to_string()))?;
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO brain_entries (id, tenant_id, path, kind, title, scope, status, body, \
             hash, links, sources, original, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
             ON CONFLICT (id) DO UPDATE SET path = $3, kind = $4, title = $5, scope = $6, \
             status = $7, body = $8, hash = $9, links = $10, sources = $11, original = $12, \
             updated_at = $14",
        )
        .bind(entry.id)
        .bind(self.tenant.as_uuid())
        .bind(relative)
        .bind(&entry.kind)
        .bind(&entry.title)
        .bind(&entry.scope)
        .bind(&entry.status)
        .bind(&entry.body)
        .bind(hash(text))
        .bind(&entry.links)
        .bind(sources)
        .bind(&entry.original)
        .bind(entry.created)
        .bind(entry.updated)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM brain_chunks WHERE entry_id = $1 AND tenant_id = $2")
            .bind(entry.id)
            .bind(self.tenant.as_uuid())
            .execute(&mut *transaction)
            .await?;
        for (seq, part) in parts.iter().enumerate() {
            sqlx::query(
                "INSERT INTO brain_chunks (id, tenant_id, entry_id, seq, text) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(entry.id)
            .bind(i32::try_from(seq).unwrap_or(i32::MAX))
            .bind(part)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.lexical.replace(entry.id, &parts)?;
        if let Err(error) = self.embed_missing().await {
            tracing::warn!(%error, "embedding failed, search stays lexical for now");
        }
        Ok(())
    }

    /// Embeds every chunk without a vector of the current model.
    ///
    /// This is also how a change of model rebuilds the vector index.
    pub async fn embed_missing(&self) -> Result<usize, BrainError> {
        let Some(embedder) = &self.embedder else {
            return Ok(0);
        };
        let model = embedder.model();
        let mut done = 0;
        loop {
            let rows = sqlx::query(
                "SELECT id, text FROM brain_chunks WHERE tenant_id = $1 \
                 AND (embedding_model IS DISTINCT FROM $2) LIMIT 64",
            )
            .bind(self.tenant.as_uuid())
            .bind(&model)
            .fetch_all(self.store.pool())
            .await?;
            if rows.is_empty() {
                return Ok(done);
            }
            let texts: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
            let vectors = embedder.embed(&texts).await?;
            if vectors.len() != rows.len() {
                return Err(MediaError::Failed("wrong number of vectors".to_owned()).into());
            }
            for (row, vector) in rows.iter().zip(&vectors) {
                let id: Uuid = row.get(0);
                sqlx::query(
                    "UPDATE brain_chunks SET embedding = $1::text::vector, embedding_model = $2 \
                     WHERE id = $3 AND tenant_id = $4",
                )
                .bind(vector_literal(vector))
                .bind(&model)
                .bind(id)
                .bind(self.tenant.as_uuid())
                .execute(self.store.pool())
                .await?;
            }
            done += rows.len();
        }
    }

    /// Stores a new entry and returns it.
    pub async fn remember(&self, new: NewEntry) -> Result<Entry, BrainError> {
        self.store_entry(new, None).await
    }

    async fn store_entry(
        &self,
        new: NewEntry,
        original: Option<String>,
    ) -> Result<Entry, BrainError> {
        if !KINDS.contains(&new.kind.as_str()) {
            return Err(BrainError::Invalid(format!("unknown type `{}`", new.kind)));
        }
        if new.title.trim().is_empty() {
            return Err(BrainError::Invalid("the title is empty".to_owned()));
        }
        if new.sources.is_empty() {
            return Err(BrainError::Invalid("every entry needs a source".to_owned()));
        }
        let now = Utc::now();
        let id = Uuid::now_v7();
        let entry = Entry {
            id,
            kind: new.kind,
            title: new.title,
            scope: "personal".to_owned(),
            status: "active".to_owned(),
            sources: new.sources,
            links: new.links,
            original,
            created: now,
            updated: now,
            body: new.body,
        };
        let name = match &entry.original {
            Some(original) => format!("{}/{original}.md", self.files_dir(id)),
            None => format!(
                "{}s/{}-{}.md",
                entry.kind,
                slugify(&entry.title),
                &id.simple().to_string()[24..]
            ),
        };
        self.write_entry(&name, &entry).await?;
        Ok(entry)
    }

    fn files_dir(&self, id: Uuid) -> String {
        format!("files/{}", id.simple())
    }

    /// Stores an uploaded file with its extracted text.
    pub async fn ingest_file(
        &self,
        name: &str,
        content_type: &str,
        bytes: Vec<u8>,
        source: Source,
    ) -> Result<Entry, BrainError> {
        let name = safe_file_name(name)?;
        let text =
            extract::extract_text(content_type, &name, &bytes, self.media.as_deref()).await?;
        let new = NewEntry {
            kind: "file".to_owned(),
            title: name.clone(),
            body: text,
            sources: vec![source],
            links: Vec::new(),
        };
        let entry = self.store_entry(new, Some(name.clone())).await?;
        let path = self.resolve(&format!("{}/{name}", self.files_dir(entry.id)))?;
        tokio::fs::write(path, bytes).await?;
        Ok(entry)
    }

    async fn stored_path(&self, id: Uuid) -> Result<Option<String>, BrainError> {
        let row = sqlx::query("SELECT path FROM brain_entries WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(self.tenant.as_uuid())
            .fetch_optional(self.store.pool())
            .await?;
        Ok(row.map(|row| row.get(0)))
    }

    /// Reads an entry from its file.
    pub async fn get(&self, id: Uuid) -> Result<Option<Entry>, BrainError> {
        let Some(relative) = self.stored_path(id).await? else {
            return Ok(None);
        };
        let text = tokio::fs::read_to_string(self.resolve(&relative)?).await?;
        Ok(Some(Entry::parse(&text)?))
    }

    /// Reads the original of an uploaded file, with its name.
    pub async fn original(&self, id: Uuid) -> Result<Option<(String, Vec<u8>)>, BrainError> {
        let Some(entry) = self.get(id).await? else {
            return Ok(None);
        };
        let Some(name) = entry.original else {
            return Ok(None);
        };
        let path = self.resolve(&format!("{}/{name}", self.files_dir(id)))?;
        Ok(Some((name, tokio::fs::read(path).await?)))
    }

    /// Changes an entry. Returns `None` if it does not exist.
    pub async fn update(&self, id: Uuid, patch: EntryPatch) -> Result<Option<Entry>, BrainError> {
        let Some(mut entry) = self.get(id).await? else {
            return Ok(None);
        };
        if let Some(status) = patch.status {
            if !["active", "superseded", "archived"].contains(&status.as_str()) {
                return Err(BrainError::Invalid(format!("unknown status `{status}`")));
            }
            entry.status = status;
        }
        if let Some(title) = patch.title {
            entry.title = title;
        }
        if let Some(body) = patch.body {
            entry.body = body;
        }
        if let Some(links) = patch.links {
            entry.links = links;
        }
        entry.updated = Utc::now();
        let relative = self.stored_path(id).await?.unwrap_or_default();
        self.write_entry(&relative, &entry).await?;
        Ok(Some(entry))
    }

    /// Adds a link to an entry.
    pub async fn link(&self, id: Uuid, link: &str) -> Result<Option<Entry>, BrainError> {
        let Some(entry) = self.get(id).await? else {
            return Ok(None);
        };
        let mut links = entry.links;
        if !links.iter().any(|existing| existing == link) {
            links.push(link.to_owned());
        }
        let patch = EntryPatch {
            links: Some(links),
            ..EntryPatch::default()
        };
        self.update(id, patch).await
    }

    /// Deletes an entry for real: file, original and every index row.
    ///
    /// Only the fact that a deletion happened stays. Returns whether the
    /// entry existed.
    pub async fn delete(&self, id: Uuid) -> Result<bool, BrainError> {
        let Some(relative) = self.stored_path(id).await? else {
            return Ok(false);
        };
        let path = self.resolve(&relative)?;
        let original = format!("{}", self.root.join(self.files_dir(id)).display());
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if relative.starts_with("files/") {
            tokio::fs::remove_dir_all(&original).await?;
        }
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query("DELETE FROM brain_entries WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(self.tenant.as_uuid())
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO brain_deletions (entry_id, tenant_id) VALUES ($1, $2)")
            .bind(id)
            .bind(self.tenant.as_uuid())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        self.lexical.replace(id, &[])?;
        Ok(true)
    }

    /// Lists entries that link here with `[[Title]]`.
    pub async fn backlinks(&self, id: Uuid) -> Result<Vec<EntrySummary>, BrainError> {
        let rows = sqlx::query(
            "SELECT e.id, e.kind, e.title, e.status, e.body, e.original, e.updated_at \
             FROM brain_entries e JOIN brain_entries target ON target.id = $1 \
             WHERE e.tenant_id = $2 AND e.id <> $1 \
             AND position('[[' || lower(target.title) || ']]' IN lower(e.body)) > 0 \
             ORDER BY e.updated_at DESC",
        )
        .bind(id)
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows.iter().map(summary).collect())
    }

    /// Finds an entry by its title, for following `[[Title]]`.
    pub async fn find_by_title(&self, title: &str) -> Result<Option<Uuid>, BrainError> {
        let row = sqlx::query(
            "SELECT id FROM brain_entries WHERE tenant_id = $1 AND lower(title) = lower($2) \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(self.tenant.as_uuid())
        .bind(title)
        .fetch_optional(self.store.pool())
        .await?;
        Ok(row.map(|row| row.get(0)))
    }

    /// Lists entries, latest first.
    pub async fn list(
        &self,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EntrySummary>, BrainError> {
        let rows = sqlx::query(
            "SELECT id, kind, title, status, body, original, updated_at FROM brain_entries \
             WHERE tenant_id = $1 AND ($2::text IS NULL OR kind = $2) \
             ORDER BY updated_at DESC LIMIT $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(kind)
        .bind(limit)
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows.iter().map(summary).collect())
    }

    /// Lists entries that carry a link, such as `holding:depot:ACME`, latest first.
    pub async fn linked(&self, link: &str, limit: i64) -> Result<Vec<EntrySummary>, BrainError> {
        let rows = sqlx::query(
            "SELECT id, kind, title, status, body, original, updated_at FROM brain_entries \
             WHERE tenant_id = $1 AND $2 = ANY(links) ORDER BY updated_at DESC LIMIT $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(link)
        .bind(limit)
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows.iter().map(summary).collect())
    }

    async fn vector_ranks(&self, query: &str) -> Vec<Uuid> {
        let Some(embedder) = &self.embedder else {
            return Vec::new();
        };
        let vectors = match embedder.embed(&[query.to_owned()]).await {
            Ok(vectors) => vectors,
            Err(error) => {
                tracing::warn!(%error, "vector search skipped");
                return Vec::new();
            }
        };
        let Some(vector) = vectors.first() else {
            return Vec::new();
        };
        let rows = sqlx::query(
            "SELECT entry_id FROM brain_chunks WHERE tenant_id = $1 AND embedding_model = $2 \
             ORDER BY embedding <=> $3::text::vector LIMIT $4",
        )
        .bind(self.tenant.as_uuid())
        .bind(embedder.model())
        .bind(vector_literal(vector))
        .bind(CANDIDATES as i64)
        .fetch_all(self.store.pool())
        .await;
        match rows {
            Ok(rows) => {
                let mut seen = HashSet::new();
                rows.iter()
                    .map(|row| row.get::<Uuid, _>(0))
                    .filter(|id| seen.insert(*id))
                    .collect()
            }
            Err(error) => {
                tracing::warn!(%error, "vector search skipped");
                Vec::new()
            }
        }
    }

    /// Searches with BM25 and vectors. Superseded and archived entries stay out.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, BrainError> {
        let lexical = self.lexical.search(query, CANDIDATES)?;
        let vector = self.vector_ranks(query).await;
        let merged = fuse(&[lexical, vector]);
        let ids: Vec<Uuid> = merged.iter().map(|(id, _)| *id).collect();
        let rows = sqlx::query(
            "SELECT id, kind, title, status, body, original, updated_at FROM brain_entries \
             WHERE tenant_id = $1 AND id = ANY($2) AND status = 'active'",
        )
        .bind(self.tenant.as_uuid())
        .bind(&ids)
        .fetch_all(self.store.pool())
        .await?;
        let mut found: HashMap<Uuid, EntrySummary> = rows
            .iter()
            .map(summary)
            .map(|summary| (summary.id, summary))
            .collect();
        Ok(merged
            .into_iter()
            .filter_map(|(id, score)| {
                Some(SearchHit {
                    entry: found.remove(&id)?,
                    score,
                })
            })
            .take(limit)
            .collect())
    }

    /// Brings the caches in line with the files and rebuilds the BM25 index.
    ///
    /// A file that cannot be read stays untouched and keeps its old index row.
    pub async fn sync(&self) -> Result<SyncReport, BrainError> {
        let mut report = SyncReport::default();
        tokio::fs::create_dir_all(&self.root).await?;
        let known: HashMap<String, String> =
            sqlx::query("SELECT path, hash FROM brain_entries WHERE tenant_id = $1")
                .bind(self.tenant.as_uuid())
                .fetch_all(self.store.pool())
                .await?
                .iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect();
        let mut present = HashSet::new();
        for path in markdown_files(&self.root).await? {
            let relative = path
                .strip_prefix(&self.root)
                .map_err(|error| BrainError::Invalid(error.to_string()))?
                .to_string_lossy()
                .into_owned();
            present.insert(relative.clone());
            let text = tokio::fs::read_to_string(&path).await?;
            if known.get(&relative) == Some(&hash(&text)) {
                continue;
            }
            match Entry::parse(&text) {
                Ok(entry) => {
                    self.index_entry(&relative, &entry, &text).await?;
                    report.indexed += 1;
                }
                Err(error) => {
                    tracing::warn!(file = %relative, %error, "skipping file");
                    report.skipped += 1;
                }
            }
        }
        for relative in known.keys().filter(|path| !present.contains(*path)) {
            sqlx::query("DELETE FROM brain_entries WHERE tenant_id = $1 AND path = $2")
                .bind(self.tenant.as_uuid())
                .bind(relative)
                .execute(self.store.pool())
                .await?;
            report.removed += 1;
        }
        let rows = sqlx::query(
            "SELECT entry_id, text FROM brain_chunks WHERE tenant_id = $1 ORDER BY entry_id, seq",
        )
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        let mut grouped: HashMap<Uuid, Vec<String>> = HashMap::new();
        for row in rows {
            grouped.entry(row.get(0)).or_default().push(row.get(1));
        }
        for (id, parts) in grouped {
            self.lexical.replace(id, &parts)?;
        }
        Ok(report)
    }
}

fn summary(row: &sqlx::postgres::PgRow) -> EntrySummary {
    let body: String = row.get(4);
    EntrySummary {
        id: row.get(0),
        kind: row.get(1),
        title: row.get(2),
        status: row.get(3),
        snippet: snippet(&body),
        original: row.get(5),
        updated_at: row.get(6),
    }
}

async fn markdown_files(root: &Path) -> Result<Vec<PathBuf>, BrainError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(item) = entries.next_entry().await? {
            let path = item.path();
            if item.file_type().await?.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests;
