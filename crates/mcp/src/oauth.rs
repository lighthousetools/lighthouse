//! The MCP OAuth flow.
//!
//! The SDK handles discovery, client registration, PKCE and token refresh.
//! Credentials are kept in the vault under `oauth:<connector>`. A pending flow
//! lives in memory, so it must finish on the server that started it.

use crate::McpError;
use async_trait::async_trait;
use lighthouse_config::{ConnectorAuth, ConnectorConfig, ConnectorTransport};
use lighthouse_vault::Vault;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};
use secrecy::{ExposeSecret, SecretString};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;

const CLIENT_NAME: &str = "Lighthouse";

/// Stores OAuth credentials of one connector in the vault.
#[derive(Clone, Debug)]
pub struct VaultCredentialStore {
    vault: Vault,
    entry: String,
}

impl VaultCredentialStore {
    /// Creates the store for a connector.
    pub fn new(vault: Vault, connector: &str) -> Self {
        Self {
            vault,
            entry: format!("oauth:{connector}"),
        }
    }
}

fn store_error(error: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(error.to_string())
}

#[async_trait]
impl CredentialStore for VaultCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match self.vault.get(&self.entry).await.map_err(store_error)? {
            Some(value) => serde_json::from_str(value.expose_secret())
                .map(Some)
                .map_err(store_error),
            None => Ok(None),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials).map_err(store_error)?;
        self.vault
            .put(&self.entry, &SecretString::from(json))
            .await
            .map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.vault
            .delete(&self.entry)
            .await
            .map(|_| ())
            .map_err(store_error)
    }
}

/// Runs OAuth flows for connectors.
pub struct OAuthFlows {
    connectors: BTreeMap<String, ConnectorConfig>,
    redirect_uri: String,
    pending: Mutex<HashMap<String, (String, OAuthState)>>,
}

impl std::fmt::Debug for OAuthFlows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthFlows")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

fn oauth_error(error: impl std::fmt::Display) -> McpError {
    McpError::OAuth(error.to_string())
}

impl OAuthFlows {
    /// Creates the flow manager. `public_url` is the server's base URL. One
    /// manager serves every tenant: each flow stores its tokens in the vault
    /// it was started with.
    pub fn new(connectors: BTreeMap<String, ConnectorConfig>, public_url: &str) -> Self {
        Self {
            connectors,
            redirect_uri: format!("{}/oauth/callback", public_url.trim_end_matches('/')),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Starts a flow for the tenant of `vault` and returns the URL the user
    /// must open.
    pub async fn start(&self, connector: &str, vault: &Vault) -> Result<String, McpError> {
        let config = self
            .connectors
            .get(connector)
            .ok_or_else(|| McpError::UnknownConnector(connector.to_owned()))?;
        self.start_with(connector, config, vault).await
    }

    /// Starts a flow for a connector with the given settings, such as one a
    /// tenant or a person added, and stores its tokens in `vault`.
    pub async fn start_with(
        &self,
        connector: &str,
        config: &ConnectorConfig,
        vault: &Vault,
    ) -> Result<String, McpError> {
        let ConnectorTransport::Http {
            url,
            auth: ConnectorAuth::Oauth,
            client_id,
            scopes,
            allow_private_addresses,
            ..
        } = &config.transport
        else {
            return Err(McpError::OAuth(format!(
                "connector `{connector}` does not use OAuth"
            )));
        };
        let mut manager = if *allow_private_addresses {
            AuthorizationManager::new_allowing_private_addresses(url.as_str()).await
        } else {
            AuthorizationManager::new(url.as_str()).await
        }
        .map_err(oauth_error)?;
        manager.set_credential_store(VaultCredentialStore::new(vault.clone(), connector));
        let mut state = OAuthState::Unauthorized(manager);
        let mut request = AuthorizationRequest::new(self.redirect_uri.clone())
            .with_client_name(CLIENT_NAME)
            .with_application_type("web")
            .with_initial_scope_selection();
        if !scopes.is_empty() {
            request = request.with_scopes(scopes.iter().cloned());
        }
        if let Some(client_id) = client_id {
            request = request.with_preregistered_client(client_id.clone());
            if let Some(secret) = vault.get(&client_secret_entry(connector)).await? {
                request = request.with_client_secret(secret.expose_secret().to_owned());
            }
        }
        state
            .start_authorization(request)
            .await
            .map_err(oauth_error)?;
        let authorize_url = state.get_authorization_url().await.map_err(oauth_error)?;
        let parsed = url::Url::parse(&authorize_url).map_err(oauth_error)?;
        let csrf = parsed
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| McpError::OAuth("authorization URL has no state".to_owned()))?;
        self.pending
            .lock()
            .await
            .insert(csrf, (connector.to_owned(), state));
        Ok(authorize_url)
    }

    /// Finishes a flow with the parameters of the redirect and returns the connector name.
    pub async fn finish(
        &self,
        code: &str,
        csrf: &str,
        issuer: Option<&str>,
    ) -> Result<String, McpError> {
        let (connector, mut state) =
            self.pending.lock().await.remove(csrf).ok_or_else(|| {
                McpError::OAuth("unknown or expired authorization state".to_owned())
            })?;
        match issuer {
            Some(issuer) => {
                state
                    .handle_callback_with_issuer(code, csrf, Some(issuer))
                    .await
            }
            None => state.handle_callback(code, csrf).await,
        }
        .map_err(oauth_error)?;
        Ok(connector)
    }
}

/// The vault entry of the OAuth client secret of a connector, for servers that only accept registered confidential
/// clients, such as GitHub.
pub fn client_secret_entry(connector: &str) -> String {
    format!("oauth-client:{connector}")
}
