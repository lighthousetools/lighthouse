//! Contract templates in the Brain, and drafts composed from them.

use crate::{ActionError, ActionResult, ApiError, ApiState, internal, not_found};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lighthouse_brain::{BrainError, Entry, EntrySummary, NewEntry, Source};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

const KIND: &str = "template";

/// A new template.
#[derive(Deserialize, ToSchema)]
pub struct NewTemplate {
    /// Title, such as "Service agreement".
    pub title: String,
    /// Markdown with `{{field}}` placeholders, such as `{{customer}}`.
    pub body: String,
}

/// A draft to compose.
#[derive(Deserialize, ToSchema)]
pub struct ComposeRequest {
    /// The template.
    pub template_id: Uuid,
    /// Values for its placeholders.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Title of the draft. Defaults to the template's title.
    #[serde(default)]
    pub title: Option<String>,
    /// Contract source whose `draft` action receives `title` and `body`. Without one, the text only comes back.
    #[serde(default)]
    pub source: Option<String>,
}

/// A composed draft.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Composed {
    /// Title.
    pub title: String,
    /// The text with the values filled in.
    pub text: String,
    /// Placeholders without a value, left as `[TODO: field]`.
    pub missing: Vec<String>,
    /// The draft action, if a source was named.
    pub action: Option<ActionResult>,
}

/// Fills `{{field}}` placeholders and returns the text with the fields that had no value.
pub fn fill(template: &str, values: &BTreeMap<String, String>) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut missing = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        text.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            text.push_str(&rest[start..]);
            return (text, missing);
        };
        let key = after[..end].trim();
        match values.get(key) {
            Some(value) => text.push_str(value),
            None => {
                text.push_str(&format!("[TODO: {key}]"));
                if !missing.iter().any(|known| known == key) {
                    missing.push(key.to_owned());
                }
            }
        }
        rest = &after[end + 2..];
    }
    text.push_str(rest);
    (text, missing)
}

/// Lists the contract templates.
#[utoipa::path(
    get,
    operation_id = "list_templates",
    path = "/contracts/templates",
    responses((status = 200, body = Vec<EntrySummary>)),
    security(("bearer" = []))
)]
pub(crate) async fn templates(
    State(state): State<ApiState>,
) -> Result<Json<Vec<EntrySummary>>, ApiError> {
    Ok(Json(
        state.brain.list(Some(KIND), 200).await.map_err(internal)?,
    ))
}

/// Stores a template in the Brain. Change it with `PATCH /brain/entries/{id}`.
#[utoipa::path(
    post,
    operation_id = "create_template",
    path = "/contracts/templates",
    request_body = NewTemplate,
    responses((status = 201, body = Entry), (status = 400, description = "Invalid template")),
    security(("bearer" = []))
)]
pub(crate) async fn create_template(
    State(state): State<ApiState>,
    Json(new): Json<NewTemplate>,
) -> Result<(StatusCode, Json<Entry>), ApiError> {
    let entry = state
        .brain
        .remember(NewEntry {
            kind: KIND.to_owned(),
            title: new.title,
            body: new.body,
            sources: vec![Source {
                kind: "app".to_owned(),
                reference: "contracts".to_owned(),
            }],
            links: Vec::new(),
        })
        .await
        .map_err(|error| match error {
            BrainError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
            other => internal(other),
        })?;
    Ok((StatusCode::CREATED, Json(entry)))
}

/// Composes a draft from a template. With a `source`, the draft goes to that
/// source's `draft` action; a writing tool waits for approval, and nothing is
/// ever signed or sent by itself.
#[utoipa::path(
    post,
    operation_id = "compose_contract",
    path = "/contracts/compose",
    request_body = ComposeRequest,
    responses(
        (status = 200, body = Composed),
        (status = 404, description = "Unknown template"),
        (status = 422, description = "The source has no draft action")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn compose(
    State(state): State<ApiState>,
    Json(request): Json<ComposeRequest>,
) -> Result<Json<Composed>, ApiError> {
    let template = state
        .brain
        .get(request.template_id)
        .await
        .map_err(internal)?
        .filter(|entry| entry.kind == KIND)
        .ok_or_else(|| not_found("template"))?;
    let (text, missing) = fill(&template.body, &request.values);
    let title = request.title.unwrap_or(template.title);
    let action = match &request.source {
        Some(source) => Some(
            state
                .connected
                .act(
                    source,
                    "draft",
                    None,
                    serde_json::json!({"title": title, "body": text}),
                )
                .await
                .map_err(|error| match error {
                    ActionError::Rejected(reason) => {
                        ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason)
                    }
                    ActionError::Failed(reason) => internal(reason),
                })?,
        ),
        None => None,
    };
    Ok(Json(Composed {
        title,
        text,
        missing,
        action,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use serde_json::{Value, json};

    #[test]
    fn placeholders_are_filled_or_marked() {
        let values = BTreeMap::from([("customer".to_owned(), "Example Ltd".to_owned())]);
        let (text, missing) = fill(
            "For {{ customer }}, from {{start}} to {{start}}. {{broken",
            &values,
        );
        assert_eq!(
            text,
            "For Example Ltd, from [TODO: start] to [TODO: start]. {{broken"
        );
        assert_eq!(missing, ["start"]);
    }

    #[tokio::test]
    async fn templates_are_stored_and_composed_into_drafts() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let new = json!({"title": "Service agreement", "body": "Between us and {{customer}}."});
        let (status, body) =
            send(app, "POST", "/contracts/templates", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED);
        let template: Value = serde_json::from_str(&body).unwrap();
        assert!(
            call(app, "GET", "/contracts/templates", Some(TOKEN))
                .await
                .1
                .contains("Service agreement")
        );
        let request = json!({"template_id": template["id"], "values": {"customer": "Example Ltd"}, "source": "docs"});
        let (status, body) = send(
            app,
            "POST",
            "/contracts/compose",
            Some(TOKEN),
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let composed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(composed["text"], "Between us and Example Ltd.");
        assert_eq!(composed["action"]["status"], "done");
        let only_text = json!({"template_id": template["id"]});
        let (_, body) = send(
            app,
            "POST",
            "/contracts/compose",
            Some(TOKEN),
            Some(only_text),
        )
        .await;
        let composed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(composed["missing"][0], "customer");
        assert!(composed["action"].is_null());
        let unknown = json!({"template_id": Uuid::nil()});
        assert_eq!(
            send(
                app,
                "POST",
                "/contracts/compose",
                Some(TOKEN),
                Some(unknown)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let wrong = json!({"template_id": template["id"], "source": "nope"});
        assert_eq!(
            send(app, "POST", "/contracts/compose", Some(TOKEN), Some(wrong))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            send(
                app,
                "POST",
                "/contracts/templates",
                Some(TOKEN),
                Some(json!({"title": " ", "body": "x"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}
