//! The Second Brain.

use crate::auth::Caller;
use crate::{ApiError, Instance, Scoped, internal, may_ask};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use lighthouse_brain::{BrainError, Entry, EntryPatch, EntrySummary, NewEntry, SearchHit, Source};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const DEFAULT_HITS: usize = 10;

fn brain_error(error: BrainError) -> ApiError {
    match error {
        BrainError::Invalid(_) | BrainError::Media(_) => {
            ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
        }
        other => internal(other),
    }
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "unknown entry".to_owned())
}

fn app_source() -> Source {
    Source {
        kind: "app".to_owned(),
        reference: "manual".to_owned(),
    }
}

/// Parameters of a search.
#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

/// Parameters of the entry list.
#[derive(Deserialize)]
pub(crate) struct EntriesQuery {
    #[serde(rename = "type")]
    kind: Option<String>,
    limit: Option<i64>,
}

/// Parameters of an upload.
#[derive(Deserialize)]
pub(crate) struct UploadQuery {
    name: String,
}

/// Parameters of a link lookup.
#[derive(Deserialize)]
pub(crate) struct ResolveQuery {
    title: String,
}

/// The entry behind a link.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ResolvedEntry {
    /// Entry ID.
    pub id: Uuid,
}

/// A question to the Brain.
#[derive(Deserialize, ToSchema)]
pub struct AskRequest {
    /// The question.
    pub question: String,
}

/// An answer from the Brain.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AskAnswer {
    /// Run that holds the costs of this answer.
    pub run_id: Uuid,
    /// The answer in Markdown.
    pub answer: String,
    /// The entries it is based on.
    pub sources: Vec<EntrySummary>,
}

/// Searches the Brain.
#[utoipa::path(
    get,
    operation_id = "search_brain",
    path = "/brain/search",
    params(
        ("q" = String, Query, description = "Search terms"),
        ("limit" = Option<usize>, Query, description = "Maximum number of hits")
    ),
    responses((status = 200, body = Vec<SearchHit>)),
    security(("bearer" = []))
)]
pub(crate) async fn search(
    Scoped(state): Scoped,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<SearchHit>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_HITS).clamp(1, 50);
    let hits = state
        .brain
        .search(&query.q, limit)
        .await
        .map_err(brain_error)?;
    Ok(Json(hits))
}

/// Asks a question and answers from the stored entries.
#[utoipa::path(
    post,
    operation_id = "ask_brain",
    path = "/brain/ask",
    request_body = AskRequest,
    responses(
        (status = 200, body = AskAnswer),
        (status = 409, description = "The entitlements refuse the question"),
        (status = 502, description = "The model could not answer")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn ask(
    State(instance): State<Instance>,
    caller: Caller,
    Scoped(state): Scoped,
    Json(request): Json<AskRequest>,
) -> Result<Json<AskAnswer>, ApiError> {
    if request.question.trim().is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the question is empty".to_owned(),
        ));
    }
    may_ask(&instance, &caller).await?;
    state
        .assistant
        .ask(&request.question)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!(%error, "question failed");
            ApiError(StatusCode::BAD_GATEWAY, error)
        })
}

/// Lists entries, latest first.
#[utoipa::path(
    get,
    operation_id = "list_entries",
    path = "/brain/entries",
    params(
        ("type" = Option<String>, Query, description = "Entry kind"),
        ("limit" = Option<i64>, Query, description = "Maximum number of entries")
    ),
    responses((status = 200, body = Vec<EntrySummary>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    Scoped(state): Scoped,
    Query(query): Query<EntriesQuery>,
) -> Result<Json<Vec<EntrySummary>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    let entries = state
        .brain
        .list(query.kind.as_deref(), limit)
        .await
        .map_err(brain_error)?;
    Ok(Json(entries))
}

/// Full entry with its text.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct EntryDetail {
    /// The entry.
    #[serde(flatten)]
    pub entry: Entry,
    /// Markdown text.
    pub body: String,
    /// Entries that link here.
    #[serde(default)]
    pub backlinks: Vec<EntrySummary>,
}

impl From<Entry> for EntryDetail {
    fn from(entry: Entry) -> Self {
        let body = entry.body.clone();
        Self {
            entry,
            body,
            backlinks: Vec::new(),
        }
    }
}

/// Stores an entry. Without sources it counts as entered in the app.
#[utoipa::path(
    post,
    operation_id = "create_entry",
    path = "/brain/entries",
    request_body = NewEntry,
    responses(
        (status = 201, body = EntryDetail),
        (status = 422, description = "Invalid entry")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    Scoped(state): Scoped,
    Json(mut new): Json<NewEntry>,
) -> Result<(StatusCode, Json<EntryDetail>), ApiError> {
    if new.sources.is_empty() {
        new.sources.push(app_source());
    }
    let entry = state.brain.remember(new).await.map_err(brain_error)?;
    Ok((StatusCode::CREATED, Json(entry.into())))
}

/// Reads an entry.
#[utoipa::path(
    get,
    operation_id = "get_entry",
    path = "/brain/entries/{id}",
    params(("id" = Uuid, Path, description = "Entry ID")),
    responses((status = 200, body = EntryDetail), (status = 404, description = "Unknown entry")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<EntryDetail>, ApiError> {
    let entry = state.brain.get(id).await.map_err(brain_error)?;
    let mut detail = EntryDetail::from(entry.ok_or_else(not_found)?);
    detail.backlinks = state.brain.backlinks(id).await.map_err(brain_error)?;
    Ok(Json(detail))
}

/// Finds the entry behind a `[[Title]]` link.
#[utoipa::path(
    get,
    operation_id = "resolve_entry",
    path = "/brain/resolve",
    params(("title" = String, Query, description = "Entry title")),
    responses((status = 200, body = ResolvedEntry), (status = 404, description = "Unknown title")),
    security(("bearer" = []))
)]
pub(crate) async fn resolve(
    Scoped(state): Scoped,
    Query(query): Query<ResolveQuery>,
) -> Result<Json<ResolvedEntry>, ApiError> {
    let id = state
        .brain
        .find_by_title(&query.title)
        .await
        .map_err(brain_error)?;
    Ok(Json(ResolvedEntry {
        id: id.ok_or_else(not_found)?,
    }))
}

/// Answers a question as a stream of server-sent events.
///
/// `delta` events carry text, and a final `done` or `failed` event ends it.
#[utoipa::path(
    post,
    operation_id = "ask_brain_stream",
    path = "/brain/ask/stream",
    request_body = AskRequest,
    responses((status = 200, description = "Server-sent events", content_type = "text/event-stream"),
        (status = 409, description = "The entitlements refuse the question")),
    security(("bearer" = []))
)]
pub(crate) async fn ask_stream(
    State(instance): State<Instance>,
    caller: Caller,
    Scoped(state): Scoped,
    Json(request): Json<AskRequest>,
) -> Result<impl IntoResponse, ApiError> {
    may_ask(&instance, &caller).await?;
    let (sender, receiver) = mpsc::channel::<Result<Event, Infallible>>(256);
    tokio::spawn(async move {
        let (deltas, mut incoming) = mpsc::unbounded_channel();
        let relay = sender.clone();
        let forward = tokio::spawn(async move {
            while let Some(text) = incoming.recv().await {
                let event = Event::default()
                    .event("delta")
                    .data(json!({"text": text}).to_string());
                if relay.send(Ok(event)).await.is_err() {
                    break;
                }
            }
        });
        let result = state.assistant.ask_stream(&request.question, deltas).await;
        let _ = forward.await;
        let event = match result {
            Ok(answer) => Event::default()
                .event("done")
                .data(serde_json::to_string(&answer).unwrap_or_default()),
            Err(error) => Event::default()
                .event("failed")
                .data(json!({"error": error}).to_string()),
        };
        let _ = sender.send(Ok(event)).await;
    });
    Ok(Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default()))
}

/// Changes an entry.
#[utoipa::path(
    patch,
    operation_id = "update_entry",
    path = "/brain/entries/{id}",
    params(("id" = Uuid, Path, description = "Entry ID")),
    request_body = EntryPatch,
    responses((status = 200, body = EntryDetail), (status = 404, description = "Unknown entry")),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(patch): Json<EntryPatch>,
) -> Result<Json<EntryDetail>, ApiError> {
    let entry = state.brain.update(id, patch).await.map_err(brain_error)?;
    Ok(Json(entry.ok_or_else(not_found)?.into()))
}

/// Deletes an entry for real, with its original and all index data.
#[utoipa::path(
    delete,
    operation_id = "delete_entry",
    path = "/brain/entries/{id}",
    params(("id" = Uuid, Path, description = "Entry ID")),
    responses((status = 204), (status = 404, description = "Unknown entry")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if state.brain.delete(id).await.map_err(brain_error)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

/// Uploads a file. The body is the file, `Content-Type` its type.
#[utoipa::path(
    post,
    operation_id = "upload_file",
    path = "/brain/files",
    params(("name" = String, Query, description = "File name")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, body = EntryDetail),
        (status = 422, description = "Unsupported file, or no model for it")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn upload(
    Scoped(state): Scoped,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<EntryDetail>), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream");
    let entry = state
        .brain
        .ingest_file(&query.name, content_type, body.to_vec(), app_source())
        .await
        .map_err(brain_error)?;
    Ok((StatusCode::CREATED, Json(entry.into())))
}

/// Downloads the original of a file.
#[utoipa::path(
    get,
    operation_id = "download_file",
    path = "/brain/files/{id}",
    params(("id" = Uuid, Path, description = "Entry ID")),
    responses((status = 200, description = "The file"), (status = 404, description = "No file")),
    security(("bearer" = []))
)]
pub(crate) async fn download(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let (name, bytes) = state
        .brain
        .original(id)
        .await
        .map_err(brain_error)?
        .ok_or_else(not_found)?;
    let disposition = format!(
        "attachment; filename=\"{}\"",
        name.replace(['"', '\\'], "_")
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}
