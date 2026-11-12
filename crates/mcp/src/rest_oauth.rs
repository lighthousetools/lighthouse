//! OAuth for REST API connectors.
//!
//! REST APIs name their authorization server in the configuration, so there is no discovery. The flow is the
//! authorization code flow with PKCE. Tokens are kept in the vault under `oauth:<connector>` and refresh before they
//! expire, into the vault they came from.

use crate::McpError;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use lighthouse_config::OpenApiOAuth;
use lighthouse_vault::Vault;
use rand::Rng;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Seconds before expiry at which a token counts as expired.
const EARLY: i64 = 60;

/// Tokens of a REST API connector as stored in the vault.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RestToken {
    /// Marks the entry as one of a REST API connector.
    pub rest: bool,
    /// The access token.
    pub access_token: String,
    /// The refresh token, if the server issued one.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token expires, if the server said.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl RestToken {
    fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at - EARLY <= now)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// A started flow: what the callback needs to finish it.
#[derive(Debug)]
pub struct Started {
    /// The URL the user opens.
    pub url: String,
    /// The `state` parameter of that URL.
    pub csrf: String,
    /// The PKCE verifier.
    pub verifier: String,
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn oauth_error(error: impl std::fmt::Display) -> McpError {
    McpError::OAuth(error.to_string())
}

/// Builds the authorization URL of a new flow.
pub fn start(oauth: &OpenApiOAuth, redirect_uri: &str) -> Result<Started, McpError> {
    let verifier = random();
    let csrf = random();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(&oauth.authorize_url).map_err(oauth_error)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &oauth.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &csrf)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if !oauth.scopes.is_empty() {
        url.query_pairs_mut()
            .append_pair("scope", &oauth.scopes.join(" "));
    }
    Ok(Started {
        url: url.into(),
        csrf,
        verifier,
    })
}

async fn token_request(
    http: &reqwest::Client,
    oauth: &OpenApiOAuth,
    secret: Option<&SecretString>,
    mut form: Vec<(&str, String)>,
    previous_refresh: Option<String>,
    now: i64,
) -> Result<RestToken, McpError> {
    form.push(("client_id", oauth.client_id.clone()));
    if let Some(secret) = secret {
        form.push(("client_secret", secret.expose_secret().to_owned()));
    }
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let response = http
        .post(&oauth.token_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| oauth_error(error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        // The body may echo request details, so only the status is kept.
        return Err(McpError::OAuth(format!(
            "the token endpoint answered {status}"
        )));
    }
    let token: TokenResponse = response.json().await.map_err(oauth_error)?;
    Ok(RestToken {
        rest: true,
        access_token: token.access_token,
        refresh_token: token.refresh_token.or(previous_refresh),
        expires_at: token.expires_in.map(|seconds| now + seconds),
    })
}

/// Exchanges the code of the callback for tokens and stores them in `vault`. `app_secret` is the secret of the
/// instance's app when the connector signs in with it; otherwise the secret is the vault's.
#[allow(clippy::too_many_arguments)]
pub async fn finish(
    http: &reqwest::Client,
    oauth: &OpenApiOAuth,
    connector: &str,
    vault: &Vault,
    app_secret: Option<&SecretString>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    now: i64,
) -> Result<(), McpError> {
    let secret = match app_secret {
        Some(secret) => Some(secret.clone()),
        None => {
            vault
                .get(&crate::oauth::client_secret_entry(connector))
                .await?
        }
    };
    let form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", redirect_uri.to_owned()),
        ("code_verifier", verifier.to_owned()),
    ];
    let token = token_request(http, oauth, secret.as_ref(), form, None, now).await?;
    save(vault, connector, &token).await
}

async fn save(vault: &Vault, connector: &str, token: &RestToken) -> Result<(), McpError> {
    let json = serde_json::to_string(token).map_err(oauth_error)?;
    vault
        .put(&format!("oauth:{connector}"), &SecretString::from(json))
        .await?;
    Ok(())
}

/// The access token of a connector from the first vault that has one, refreshed when it expired. `app_secret` is as
/// for [`finish`].
pub async fn access_token(
    http: &reqwest::Client,
    oauth: &OpenApiOAuth,
    connector: &str,
    vaults: &[Vault],
    app_secret: Option<&SecretString>,
    now: i64,
) -> Result<SecretString, McpError> {
    let entry = format!("oauth:{connector}");
    let not_connected = || McpError::NotAuthorized {
        connector: connector.to_owned(),
        detail: format!("run `lht connect {connector}` first"),
    };
    let mut found = None;
    for vault in vaults {
        if let Some(value) = vault.get(&entry).await? {
            found = Some((vault.clone(), value));
            break;
        }
    }
    let (vault, value) = found.ok_or_else(not_connected)?;
    let token: RestToken =
        serde_json::from_str(value.expose_secret()).map_err(|_| not_connected())?;
    if !token.expired(now) {
        return Ok(SecretString::from(token.access_token));
    }
    let refresh = token.refresh_token.clone().ok_or_else(not_connected)?;
    let mut secret = app_secret.cloned();
    for vault in vaults.iter().filter(|_| app_secret.is_none()) {
        if let Some(value) = vault
            .get(&crate::oauth::client_secret_entry(connector))
            .await?
        {
            secret = Some(value);
            break;
        }
    }
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh.clone()),
    ];
    if !oauth.scopes.is_empty() {
        form.push(("scope", oauth.scopes.join(" ")));
    }
    let fresh = token_request(http, oauth, secret.as_ref(), form, Some(refresh), now).await?;
    save(&vault, connector, &fresh).await?;
    Ok(SecretString::from(fresh.access_token))
}
