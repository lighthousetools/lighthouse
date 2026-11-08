//! The companies the owner runs.

use crate::{ApiError, ApiState, Scoped, internal, not_found};
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use lighthouse_sync::{Organization, OrganizationError, OrganizationInput};
use serde_json::json;

fn changed(state: &ApiState, id: &str) {
    state
        .events
        .emit("organizations.changed", json!({"id": id}));
}

/// Lists the organizations.
#[utoipa::path(
    get,
    operation_id = "list_organizations",
    path = "/organizations",
    responses((status = 200, body = Vec<Organization>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(Scoped(state): Scoped) -> Result<Json<Vec<Organization>>, ApiError> {
    Ok(Json(state.sync.organizations().await.map_err(internal)?))
}

/// Creates or replaces an organization and the sources that belong to it.
#[utoipa::path(
    put,
    operation_id = "save_organization",
    path = "/organizations/{id}",
    params(("id" = String, Path, description = "Organization ID")),
    request_body = OrganizationInput,
    responses((status = 200, body = Organization), (status = 400, description = "Invalid organization")),
    security(("bearer" = []))
)]
pub(crate) async fn save(
    Scoped(state): Scoped,
    Path(id): Path<String>,
    Json(input): Json<OrganizationInput>,
) -> Result<Json<Organization>, ApiError> {
    let organization =
        state
            .sync
            .save_organization(&id, input)
            .await
            .map_err(|error| match error {
                OrganizationError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
                other => internal(other),
            })?;
    changed(&state, &id);
    Ok(Json(organization))
}

/// Deletes an organization. Its items stay.
#[utoipa::path(
    delete,
    operation_id = "delete_organization",
    path = "/organizations/{id}",
    params(("id" = String, Path, description = "Organization ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown organization")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !state
        .sync
        .delete_organization(&id)
        .await
        .map_err(internal)?
    {
        return Err(not_found("organization"));
    }
    changed(&state, &id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::tests::{TOKEN, call, harness, send};
    use axum::http::StatusCode;
    use lighthouse_sync::MappedItem;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn organizations_narrow_the_connected_lists() {
        let harness = harness(Some(TOKEN)).await;
        let item = |id: &str| MappedItem {
            external_id: id.to_owned(),
            title: Some(id.to_owned()),
            data: json!({}),
            ..MappedItem::default()
        };
        harness
            .sync
            .upsert("mail-a", "mail", "thread", &[item("a")], &[])
            .await
            .unwrap();
        harness
            .sync
            .upsert("mail-b", "mail", "thread", &[item("b")], &[])
            .await
            .unwrap();
        let app = &harness.app;
        let body = json!({"name": "Acme", "color": "#336699", "sources": ["mail-a"]});
        let (status, _) = send(app, "PUT", "/organizations/acme", Some(TOKEN), Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        let bad = json!({"name": ""});
        assert_eq!(
            send(app, "PUT", "/organizations/acme", Some(TOKEN), Some(bad))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let (_, body) = call(app, "GET", "/organizations", Some(TOKEN)).await;
        assert!(body.contains("Acme"));
        let (_, body) = call(app, "GET", "/mail/threads?org=acme", Some(TOKEN)).await;
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["org"], "acme");
        let (_, body) = call(app, "GET", "/inbox?org=acme", Some(TOKEN)).await;
        assert!(body.contains("\"total\""));
        assert_eq!(
            call(app, "DELETE", "/organizations/acme", Some(TOKEN))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", "/organizations/acme", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
