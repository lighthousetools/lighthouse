//! The single-owner sign-in of earlier versions, kept while the server moves
//! to [`crate::Accounts`].

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Argon2, PasswordHash};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use lighthouse_core::TenantId;
use lighthouse_store::Store;
use lighthouse_store::sqlx::{self, Row};
use rand::Rng;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{AuthError, Device, DeviceKind, IssuedToken};

const TOKEN_PREFIX: &str = "lht_";
const TOKEN_BYTES: usize = 32;
const MIN_PASSWORD_LEN: usize = crate::MIN_PASSWORD_LEN;
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
}

/// Sign-in of the owner.
#[derive(Clone, Debug)]
pub struct Auth {
    store: Store,
    tenant: TenantId,
    attempts: Arc<Mutex<Attempts>>,
}

fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn new_token() -> SecretString {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    SecretString::from(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

impl Auth {
    /// Creates the sign-in for a tenant.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self {
            store,
            tenant,
            attempts: Arc::default(),
        }
    }

    /// Returns whether the owner has set a password.
    pub async fn is_set_up(&self) -> Result<bool, AuthError> {
        Ok(self.password_hash().await?.is_some())
    }

    async fn password_hash(&self) -> Result<Option<String>, AuthError> {
        let row = sqlx::query("SELECT password_hash FROM owner_accounts WHERE tenant_id = $1")
            .bind(self.tenant.as_uuid())
            .fetch_optional(self.store.pool())
            .await?;
        Ok(row.map(|row| row.get(0)))
    }

    /// Sets the password and signs out every device.
    pub async fn set_password(&self, password: &SecretString) -> Result<(), AuthError> {
        if password.expose_secret().chars().count() < MIN_PASSWORD_LEN {
            return Err(AuthError::WeakPassword);
        }
        let hash = Argon2::default()
            .hash_password(password.expose_secret().as_bytes())
            .map_err(|_| AuthError::Hash)?
            .to_string();
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO owner_accounts (tenant_id, password_hash) VALUES ($1, $2) \
             ON CONFLICT (tenant_id) DO UPDATE \
             SET password_hash = EXCLUDED.password_hash, updated_at = now()",
        )
        .bind(self.tenant.as_uuid())
        .bind(hash)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE device_tokens SET revoked_at = now() \
             WHERE tenant_id = $1 AND revoked_at IS NULL",
        )
        .bind(self.tenant.as_uuid())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    fn check_lockout(&self) -> Result<(), AuthError> {
        let mut attempts = self.attempts.lock().expect("attempts lock");
        match attempts.locked_until {
            Some(until) if until > Instant::now() => Err(AuthError::LockedOut),
            Some(_) => {
                *attempts = Attempts::default();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn record_attempt(&self, success: bool) {
        let mut attempts = self.attempts.lock().expect("attempts lock");
        if success {
            *attempts = Attempts::default();
            return;
        }
        attempts.failures += 1;
        if attempts.failures >= MAX_FAILURES {
            attempts.locked_until = Some(Instant::now() + LOCKOUT);
        }
    }

    /// Checks the password and issues a token for a new device.
    pub async fn login(
        &self,
        password: &SecretString,
        name: &str,
        kind: DeviceKind,
    ) -> Result<IssuedToken, AuthError> {
        self.check_lockout()?;
        let stored = self.password_hash().await?;
        let valid = stored.as_deref().is_some_and(|hash| {
            PasswordHash::new(hash).is_ok_and(|parsed| {
                Argon2::default()
                    .verify_password(password.expose_secret().as_bytes(), &parsed)
                    .is_ok()
            })
        });
        self.record_attempt(valid);
        if !valid {
            tracing::warn!("failed sign-in");
            return Err(AuthError::WrongPassword);
        }
        self.issue(name, kind).await
    }

    /// Issues a token without a password check, for callers already signed in.
    pub async fn issue(&self, name: &str, kind: DeviceKind) -> Result<IssuedToken, AuthError> {
        let id = Uuid::now_v7();
        let token = new_token();
        sqlx::query(
            "INSERT INTO device_tokens (id, tenant_id, name, kind, token_hash) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(self.tenant.as_uuid())
        .bind(name)
        .bind(kind.as_str())
        .bind(hash_token(token.expose_secret()))
        .execute(self.store.pool())
        .await?;
        Ok(IssuedToken {
            id,
            token,
            tenant: self.tenant,
        })
    }

    /// Returns the device behind a token, if it is valid.
    pub async fn verify(&self, token: &str) -> Result<Option<Uuid>, AuthError> {
        Ok(self.verify_device(token).await?.map(|(id, _)| id))
    }

    /// Returns the device behind a token and its kind name, if it is valid.
    pub async fn verify_device(&self, token: &str) -> Result<Option<(Uuid, String)>, AuthError> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT id, kind FROM device_tokens \
             WHERE token_hash = $1 AND tenant_id = $2 AND revoked_at IS NULL",
        )
        .bind(hash_token(token))
        .bind(self.tenant.as_uuid())
        .fetch_optional(self.store.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let id: Uuid = row.get(0);
        let kind: String = row.get(1);
        sqlx::query(
            "UPDATE device_tokens SET last_used_at = now() WHERE id = $1 \
             AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(id)
        .execute(self.store.pool())
        .await?;
        Ok(Some((id, kind)))
    }

    /// Lists devices that are signed in.
    pub async fn devices(&self) -> Result<Vec<Device>, AuthError> {
        let rows = sqlx::query(
            "SELECT id, name, kind, created_at, last_used_at FROM device_tokens \
             WHERE tenant_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
        )
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Device {
                id: row.get(0),
                name: row.get(1),
                kind: row.get(2),
                tenant_id: self.tenant.as_uuid(),
                created_at: row.get(3),
                last_used_at: row.get(4),
            })
            .collect())
    }

    /// Signs a device out. Returns whether it was signed in.
    pub async fn revoke(&self, id: Uuid) -> Result<bool, AuthError> {
        let result = sqlx::query(
            "UPDATE device_tokens SET revoked_at = now() \
             WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(self.tenant.as_uuid())
        .execute(self.store.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_store::testing::fresh_store;

    const PASSWORD: &str = "correct horse battery";

    fn secret(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    async fn auth() -> Auth {
        let auth = Auth::new(fresh_store().await, TenantId::DEFAULT);
        auth.set_password(&secret(PASSWORD)).await.unwrap();
        auth
    }

    #[tokio::test]
    async fn fresh_instances_need_setup_and_reject_every_login() {
        let auth = Auth::new(fresh_store().await, TenantId::DEFAULT);
        assert!(!auth.is_set_up().await.unwrap());
        let result = auth.login(&secret(PASSWORD), "x", DeviceKind::Web).await;
        assert!(matches!(result, Err(AuthError::WrongPassword)));
    }

    #[tokio::test]
    async fn short_passwords_are_rejected() {
        let auth = Auth::new(fresh_store().await, TenantId::DEFAULT);
        let result = auth.set_password(&secret("short")).await;
        assert!(matches!(result, Err(AuthError::WeakPassword)));
    }

    #[tokio::test]
    async fn login_issues_a_token_that_verifies_until_revoked() {
        let auth = auth().await;
        let issued = auth
            .login(&secret(PASSWORD), "phone", DeviceKind::Ios)
            .await
            .unwrap();
        let token = issued.token.expose_secret();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(auth.verify(token).await.unwrap(), Some(issued.id));

        let devices = auth.devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "phone");
        assert_eq!(devices[0].kind, "ios");
        assert!(devices[0].last_used_at.is_some());

        assert!(auth.revoke(issued.id).await.unwrap());
        assert!(!auth.revoke(issued.id).await.unwrap());
        assert_eq!(auth.verify(token).await.unwrap(), None);
        assert!(auth.devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_database_never_holds_the_token() {
        let auth = auth().await;
        let issued = auth.issue("cli", DeviceKind::Api).await.unwrap();
        let stored: (Vec<u8>,) = sqlx::query_as("SELECT token_hash FROM device_tokens")
            .fetch_one(auth.store.pool())
            .await
            .unwrap();
        assert_ne!(stored.0, issued.token.expose_secret().as_bytes());
        assert_eq!(stored.0.len(), 32);
    }

    #[tokio::test]
    async fn unknown_tokens_do_not_verify() {
        let auth = auth().await;
        assert_eq!(auth.verify("nonsense").await.unwrap(), None);
        assert_eq!(auth.verify("lht_unknown").await.unwrap(), None);
    }

    #[tokio::test]
    async fn repeated_failures_lock_the_login_even_for_the_right_password() {
        let auth = auth().await;
        for _ in 0..MAX_FAILURES {
            let result = auth.login(&secret("wrong"), "x", DeviceKind::Web).await;
            assert!(matches!(result, Err(AuthError::WrongPassword)));
        }
        let result = auth.login(&secret(PASSWORD), "x", DeviceKind::Web).await;
        assert!(matches!(result, Err(AuthError::LockedOut)));
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let auth = auth().await;
        for _ in 0..MAX_FAILURES - 1 {
            let _ = auth.login(&secret("wrong"), "x", DeviceKind::Web).await;
        }
        auth.login(&secret(PASSWORD), "x", DeviceKind::Web)
            .await
            .unwrap();
        let result = auth.login(&secret("wrong"), "x", DeviceKind::Web).await;
        assert!(matches!(result, Err(AuthError::WrongPassword)));
    }

    #[tokio::test]
    async fn changing_the_password_signs_out_every_device() {
        let auth = auth().await;
        let issued = auth.issue("old", DeviceKind::Web).await.unwrap();
        auth.set_password(&secret("another long password"))
            .await
            .unwrap();
        assert_eq!(
            auth.verify(issued.token.expose_secret()).await.unwrap(),
            None
        );
        let result = auth.login(&secret(PASSWORD), "x", DeviceKind::Web).await;
        assert!(matches!(result, Err(AuthError::WrongPassword)));
    }
}
