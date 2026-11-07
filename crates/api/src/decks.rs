//! Decks: presentations as Markdown in the Brain, with an HTML export.
//!
//! A deck is a Brain entry of the type `deck`. A line with only `---` starts a
//! new slide. The export renders every slide as a page, ready to show in a
//! browser or to print as PDF.

use crate::{ApiError, ApiState, internal, not_found};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use lighthouse_brain::{BrainError, Entry, EntrySummary, NewEntry, Source};
use pulldown_cmark::{Event, Parser, html};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

const KIND: &str = "deck";
const LIST_LIMIT: i64 = 200;
const STYLE: &str = "body{margin:0;font-family:system-ui,sans-serif;background:#1d1d1f}\
.slide{box-sizing:border-box;width:100vw;min-height:100vh;padding:8vh 8vw;background:#fff;color:#1d1d1f;\
display:flex;flex-direction:column;justify-content:center;border-bottom:1px solid #ddd}\
.slide h1{font-size:3.2em;margin:0 0 .4em}.slide h2{font-size:2.2em;margin:0 0 .5em}\
.slide li,.slide p{font-size:1.4em;line-height:1.5}.slide img{max-width:100%;max-height:60vh}\
@media print{body{background:#fff}.slide{width:auto;height:100vh;page-break-after:always;border:0}}";

/// A new deck.
#[derive(Deserialize, ToSchema)]
pub struct NewDeck {
    /// Title.
    pub title: String,
    /// Slides in Markdown, separated by lines with only `---`.
    pub markdown: String,
    /// Links to other entries, such as `customer:example`.
    #[serde(default)]
    pub links: Vec<String>,
}

/// A brief for a new deck.
#[derive(Deserialize, ToSchema)]
pub struct DeckBrief {
    /// Title.
    pub title: String,
    /// What the deck is for, who sees it and what it should say.
    pub brief: String,
}

/// Where to attach a deck.
#[derive(Deserialize, ToSchema)]
pub struct DeckAttachment {
    /// The room, as a connected item.
    pub room_id: Uuid,
}

/// Format of the export.
#[derive(Deserialize)]
pub(crate) struct ExportQuery {
    format: Option<String>,
}

/// Splits a deck into slides.
pub fn slides(markdown: &str) -> Vec<String> {
    let mut slides = Vec::new();
    let mut current = String::new();
    for line in markdown.lines() {
        if line.trim() == "---" {
            slides.push(std::mem::take(&mut current));
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    slides.push(current);
    slides
        .into_iter()
        .map(|slide| slide.trim().to_owned())
        .filter(|slide| !slide.is_empty())
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Renders a deck as one HTML page. Raw HTML in the Markdown is shown as text.
pub fn render_html(title: &str, markdown: &str) -> String {
    let mut body = String::new();
    for slide in slides(markdown) {
        let events = Parser::new(&slide).map(|event| match event {
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            other => other,
        });
        let mut rendered = String::new();
        html::push_html(&mut rendered, events);
        body.push_str("<section class=\"slide\">");
        body.push_str(&rendered);
        body.push_str("</section>\n");
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body>\n{body}</body></html>\n",
        escape(title)
    )
}

async fn deck(state: &ApiState, id: Uuid) -> Result<Entry, ApiError> {
    state
        .brain
        .get(id)
        .await
        .map_err(internal)?
        .filter(|entry| entry.kind == KIND)
        .ok_or_else(|| not_found("deck"))
}

/// Lists the decks, latest first.
#[utoipa::path(
    get,
    operation_id = "list_decks",
    path = "/decks",
    responses((status = 200, body = Vec<EntrySummary>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<EntrySummary>>, ApiError> {
    Ok(Json(
        state
            .brain
            .list(Some(KIND), LIST_LIMIT)
            .await
            .map_err(internal)?,
    ))
}

/// Stores a new deck in the Brain. Change it with `PATCH /brain/entries/{id}`.
#[utoipa::path(
    post,
    operation_id = "create_deck",
    path = "/decks",
    request_body = NewDeck,
    responses((status = 201, body = Entry), (status = 400, description = "Invalid deck")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    State(state): State<ApiState>,
    Json(new): Json<NewDeck>,
) -> Result<(StatusCode, Json<Entry>), ApiError> {
    let entry = state
        .brain
        .remember(NewEntry {
            kind: KIND.to_owned(),
            title: new.title,
            body: new.markdown,
            sources: vec![Source {
                kind: "app".to_owned(),
                reference: "decks".to_owned(),
            }],
            links: new.links,
        })
        .await
        .map_err(|error| match error {
            BrainError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
            other => internal(other),
        })?;
    state
        .events
        .emit("decks.changed", serde_json::json!({"id": entry.id}));
    Ok((StatusCode::CREATED, Json(entry)))
}

/// Writes a deck from a brief, in the owner's voice, and stores it in the Brain
/// as a draft to edit. Nothing is shared.
#[utoipa::path(
    post,
    operation_id = "generate_deck",
    path = "/decks/generate",
    request_body = DeckBrief,
    responses((status = 201, body = Entry), (status = 502, description = "The model failed")),
    security(("bearer" = []))
)]
pub(crate) async fn generate(
    State(state): State<ApiState>,
    Json(brief): Json<DeckBrief>,
) -> Result<(StatusCode, Json<Entry>), ApiError> {
    let instructions = "Write a presentation in Markdown. Start every slide after the first with a line that \
        holds only ---. Each slide has a heading and at most five short points. Use only facts from the brief \
        and mark anything the owner must fill in as [TODO].";
    let written = state
        .writer
        .write(instructions, &brief.brief)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))?;
    create(
        State(state),
        Json(NewDeck {
            title: brief.title,
            markdown: written.text,
            links: Vec::new(),
        }),
    )
    .await
}

/// Attaches a deck to a deal room through the room source's `attach` action,
/// with `title`, `markdown` and `html` as input. A writing tool waits for approval.
#[utoipa::path(
    post,
    operation_id = "attach_deck",
    path = "/decks/{id}/attach",
    params(("id" = Uuid, Path, description = "Deck ID")),
    request_body = DeckAttachment,
    responses(
        (status = 202, body = crate::ActionResult),
        (status = 404, description = "Unknown deck or room"),
        (status = 422, description = "Not a room, or its source has no attach action")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn attach(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(attachment): Json<DeckAttachment>,
) -> Result<(StatusCode, Json<crate::ActionResult>), ApiError> {
    let entry = deck(&state, id).await?;
    let room = state
        .sync
        .item(attachment.room_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found("room"))?;
    let input = serde_json::json!({
        "title": entry.title,
        "markdown": entry.body,
        "html": render_html(&entry.title, &entry.body),
    });
    let result = state
        .connected
        .act(&room.source, "attach", Some(&room), input)
        .await
        .map_err(|error| match error {
            crate::ActionError::Rejected(reason) => {
                ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason)
            }
            crate::ActionError::Failed(reason) => internal(reason),
        })?;
    let status = if result.status == "proposed" {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(result)))
}

/// Exports a deck as one HTML page with a slide per page, or as Markdown.
///
/// For a PDF, open the HTML export and print it; the page breaks after every slide.
#[utoipa::path(
    get,
    operation_id = "export_deck",
    path = "/decks/{id}/export",
    params(
        ("id" = Uuid, Path, description = "Deck ID"),
        ("format" = Option<String>, Query, description = "html (default) or markdown")
    ),
    responses(
        (status = 200, description = "The deck", content_type = "text/html"),
        (status = 404, description = "Unknown deck"),
        (status = 415, description = "Unknown format")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn export(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let entry = deck(&state, id).await?;
    let (content_type, body) = match query.format.as_deref().unwrap_or("html") {
        "html" => (
            "text/html; charset=utf-8",
            render_html(&entry.title, &entry.body),
        ),
        "markdown" | "md" => ("text/markdown; charset=utf-8", entry.body),
        other => {
            return Err(ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("`{other}` is not html or markdown; print the HTML export for a PDF"),
            ));
        }
    };
    Ok(([(header::CONTENT_TYPE, content_type)], body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use serde_json::{Value, json};

    #[test]
    fn slides_split_and_render_without_raw_html() {
        assert_eq!(
            slides("# One\n---\n\n---\n## Two\n- a"),
            ["# One", "## Two\n- a"]
        );
        let html = render_html(
            "Plan <1>",
            "# Hello\n<script>alert(1)</script>\n---\n- item",
        );
        assert!(html.contains("<title>Plan &lt;1&gt;</title>"));
        assert_eq!(html.matches("<section class=\"slide\">").count(), 2);
        assert!(!html.contains("<script>"));
        assert!(html.contains("<li>item</li>"));
    }

    #[tokio::test]
    async fn decks_are_stored_listed_and_exported() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let new = json!({"title": "Quarterly plan", "markdown": "# Plan\n---\n## Goals\n- Ship"});
        let (status, body) = send(app, "POST", "/decks", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let entry: Value = serde_json::from_str(&body).unwrap();
        let id = entry["id"].as_str().unwrap();
        let (_, body) = call(app, "GET", "/decks", Some(TOKEN)).await;
        assert!(body.contains("Quarterly plan"));
        let (status, body) = call(app, "GET", &format!("/decks/{id}/export"), Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<h2>Goals</h2>"));
        let (_, body) = call(
            app,
            "GET",
            &format!("/decks/{id}/export?format=md"),
            Some(TOKEN),
        )
        .await;
        assert!(body.starts_with("# Plan"));
        assert_eq!(
            call(
                app,
                "GET",
                &format!("/decks/{id}/export?format=pdf"),
                Some(TOKEN)
            )
            .await
            .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let unknown = format!("/decks/{}/export", Uuid::nil());
        assert_eq!(
            call(app, "GET", &unknown, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = send(
            app,
            "POST",
            "/decks/generate",
            Some(TOKEN),
            Some(json!({"title": "Pitch", "brief": "Three slides"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let generated: Value = serde_json::from_str(&body).unwrap();
        let export = format!("/decks/{}/export?format=md", generated["id"].as_str().unwrap());
        assert_eq!(call(app, "GET", &export, Some(TOKEN)).await.1, "rewritten: Three slides");
        assert_eq!(
            send(
                app,
                "POST",
                "/decks/generate",
                Some(TOKEN),
                Some(json!({"title": "x", "brief": "fail"}))
            )
            .await
            .0,
            StatusCode::BAD_GATEWAY
        );
        let room = harness
            .sync
            .upsert(
                "rooms",
                "rooms",
                "room",
                &[lighthouse_sync::MappedItem {
                    external_id: "r1".to_owned(),
                    data: json!({}),
                    ..lighthouse_sync::MappedItem::default()
                }],
                &[],
            )
            .await
            .unwrap();
        let attach = format!("/decks/{id}/attach");
        let (status, body) = send(
            app,
            "POST",
            &attach,
            Some(TOKEN),
            Some(json!({"room_id": room[0].id})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(body.contains("rooms.attach"));
        assert_eq!(
            send(
                app,
                "POST",
                &attach,
                Some(TOKEN),
                Some(json!({"room_id": Uuid::nil()}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let empty = json!({"title": " ", "markdown": "x"});
        assert_eq!(
            send(app, "POST", "/decks", Some(TOKEN), Some(empty))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}
