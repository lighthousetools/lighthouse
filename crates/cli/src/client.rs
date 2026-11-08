//! Calls to a running server.
//!
//! Commands use a user's device token from `LHT_TOKEN` when one is set, and
//! the admin token otherwise. With the admin token, `--tenant` picks the
//! tenant through the `X-Lighthouse-Tenant` header.

use anyhow::{Context, bail};
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use uuid::Uuid;

/// Header with which the admin token picks a tenant.
const TENANT_HEADER: &str = "x-lighthouse-tenant";

/// A client of one server with one token.
pub struct Client {
    base: String,
    token: SecretString,
    tenant: Option<Uuid>,
    http: reqwest::Client,
}

async fn answer(response: reqwest::Response, path: &str) -> anyhow::Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "server answered {status} to {path}: {}",
            body["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(body)
}

impl Client {
    /// Creates a client. `tenant` only matters for the admin token.
    pub fn new(base: String, token: SecretString, tenant: Option<Uuid>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            tenant,
            http: reqwest::Client::new(),
        }
    }

    /// Sends a request and returns the JSON answer.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.token.expose_secret());
        if let Some(tenant) = self.tenant {
            request = request.header(TENANT_HEADER, tenant.to_string());
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.context("cannot reach the server")?;
        answer(response, path).await
    }

    /// Sends a GET request.
    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.send(Method::GET, path, None).await
    }

    /// Sends a POST request with a JSON body.
    pub async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        self.send(Method::POST, path, Some(body)).await
    }
}

/// Signs in with email and password and returns the issued token.
pub async fn login(
    base: &str,
    email: Option<&str>,
    password: &SecretString,
    device: &str,
    tenant: Option<Uuid>,
) -> anyhow::Result<Value> {
    let response = reqwest::Client::new()
        .post(format!("{}/auth/login", base.trim_end_matches('/')))
        .json(&json!({
            "email": email,
            "password": password.expose_secret(),
            "device_name": device,
            "device_kind": "api",
            "tenant_id": tenant,
        }))
        .send()
        .await
        .context("cannot reach the server")?;
    answer(response, "/auth/login").await
}

/// One line per tenant: ID, role and name, with the current one marked.
pub fn tenant_lines(tenants: &Value, current: Option<&str>) -> Vec<String> {
    tenants
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|tenant| {
            let id = tenant["tenant_id"].as_str().unwrap_or("?");
            let mark = if Some(id) == current { "*" } else { " " };
            format!(
                "{mark} {id}  {:<7} {}",
                tenant["role"].as_str().unwrap_or("?"),
                tenant["tenant_name"].as_str().unwrap_or("")
            )
        })
        .collect()
}

/// What to tell the inviting person about a new invitation.
pub fn invitation_message(created: &Value) -> String {
    let email = created["invitation"]["email"].as_str().unwrap_or("?");
    match created["accept_url"].as_str() {
        Some(url) if created["mailed"] != true => {
            format!("Invited {email}. No mail is configured, so pass on this link:\n{url}")
        }
        _ => format!("Invited {email} by mail."),
    }
}

/// What to tell after a sign-in or a switch: how to use the token.
pub fn token_message(issued: &Value) -> String {
    format!(
        "Signed in to tenant {}. Use the token with:\n\nexport LHT_TOKEN={}",
        issued["tenant_id"].as_str().unwrap_or("?"),
        issued["token"].as_str().unwrap_or("?")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn requests_carry_the_token_and_the_tenant() {
        let server = MockServer::start().await;
        let tenant = Uuid::now_v7();
        Mock::given(method("GET"))
            .and(path("/tenants"))
            .and(header("authorization", "Bearer admin"))
            .and(header(TENANT_HEADER, tenant.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tenant/invitations"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "not allowed"})))
            .mount(&server)
            .await;
        let client = Client::new(
            format!("{}/", server.uri()),
            SecretString::from("admin".to_owned()),
            Some(tenant),
        );
        assert_eq!(client.get("/tenants").await.unwrap(), json!([]));
        let error = client
            .post("/tenant/invitations", &json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("403") && error.contains("not allowed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn login_sends_email_and_password() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .and(body_partial_json(
                json!({"email": "a@example.com", "device_kind": "api"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": Uuid::nil(), "token": "lht_x", "tenant_id": "t1", "user_id": "u1"})))
            .mount(&server)
            .await;
        let issued = login(
            &server.uri(),
            Some("a@example.com"),
            &SecretString::from("password".to_owned()),
            "lht",
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            token_message(&issued),
            "Signed in to tenant t1. Use the token with:\n\nexport LHT_TOKEN=lht_x"
        );
    }

    #[test]
    fn tenants_and_invitations_read_well() {
        let tenants = json!([
            {"tenant_id": "a", "tenant_name": "Home", "role": "owner"},
            {"tenant_id": "b", "tenant_name": "Studio", "role": "member"}
        ]);
        assert_eq!(
            tenant_lines(&tenants, Some("b")),
            ["  a  owner   Home", "* b  member  Studio"]
        );
        assert!(tenant_lines(&Value::Null, None).is_empty());
        let mailed =
            json!({"invitation": {"email": "x@example.com"}, "mailed": true, "accept_url": null});
        assert_eq!(
            invitation_message(&mailed),
            "Invited x@example.com by mail."
        );
        let link = json!({"invitation": {"email": "x@example.com"}, "mailed": false,
                          "accept_url": "https://l.test/#/invite?token=t"});
        assert!(invitation_message(&link).ends_with("https://l.test/#/invite?token=t"));
    }
}
