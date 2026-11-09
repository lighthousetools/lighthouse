//! Encrypted storage of API keys and OAuth tokens.
//!
//! Values are sealed with AES-256-GCM. The tenant and entry name are bound as
//! associated data, so a ciphertext cannot be moved to another entry.

use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use lighthouse_store::sqlx::{self, Row};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use std::path::Path;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// Errors from the vault. They never contain secret values.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// The master key is malformed.
    #[error("master key must be {KEY_LEN} bytes in base64")]
    InvalidMasterKey,
    /// The master key file could not be used.
    #[error("cannot use master key file: {0}")]
    KeyFile(#[from] std::io::Error),
    /// Decryption failed, usually because of a wrong master key.
    #[error("cannot decrypt vault entry `{0}`")]
    Decrypt(String),
    /// Encryption failed.
    #[error("cannot encrypt vault entry `{0}`")]
    Encrypt(String),
    /// A database operation failed.
    #[error("vault storage error: {0}")]
    Storage(#[from] sqlx::Error),
}

/// The key that encrypts all vault entries.
pub struct MasterKey(SecretBox<[u8; KEY_LEN]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

impl MasterKey {
    /// Parses a base64-encoded key.
    pub fn from_base64(encoded: &str) -> Result<Self, VaultError> {
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|_| VaultError::InvalidMasterKey)?;
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| VaultError::InvalidMasterKey)?;
        Ok(Self(SecretBox::new(Box::new(bytes))))
    }

    /// Reads a base64-encoded key from a file.
    pub fn from_file(path: &Path) -> Result<Self, VaultError> {
        Self::from_base64(&std::fs::read_to_string(path)?)
    }

    /// Creates a new key file that only the current user can read.
    ///
    /// Fails if the file already exists, so a key is never overwritten.
    pub fn create_file(path: &Path) -> Result<(), VaultError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let key: Key<Aes256Gcm> = Key::<Aes256Gcm>::generate();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writeln!(file, "{}", STANDARD.encode(key.as_slice()))?;
        Ok(())
    }
}

/// Encrypted key-value storage for one tenant, or for one person in a tenant.
///
/// The tenant's vault holds what its members share, such as provider keys and
/// connector tokens that owners and admins set. [`Vault::personal`] opens the
/// part of one member: their own keys and tokens, which no other member's
/// vault can read.
#[derive(Clone)]
pub struct Vault {
    store: Store,
    tenant: TenantId,
    user: Option<uuid::Uuid>,
    cipher: Aes256Gcm,
}

/// The conflict target of a vault entry: its tenant, its person, its name.
const ENTRY: &str =
    "(tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), name)";

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("tenant", &self.tenant)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// Creates a vault for the given tenant.
    pub fn new(store: Store, tenant: TenantId, key: &MasterKey) -> Self {
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key.0.expose_secret()));
        Self {
            store,
            tenant,
            user: None,
            cipher,
        }
    }

    /// The personal vault of a member in the same tenant.
    #[must_use]
    pub fn personal(&self, user: uuid::Uuid) -> Self {
        Self {
            user: Some(user),
            ..self.clone()
        }
    }

    /// The vault the tenant shares, from a personal one.
    #[must_use]
    pub fn shared(&self) -> Self {
        Self {
            user: None,
            ..self.clone()
        }
    }

    /// The person of a personal vault, `None` for the tenant's.
    pub fn user(&self) -> Option<uuid::Uuid> {
        self.user
    }

    /// The tenant of the vault.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    fn aad(&self, name: &str) -> Vec<u8> {
        match self.user {
            None => format!("{}:{name}", self.tenant.as_uuid()).into_bytes(),
            Some(user) => format!("{}:{user}:{name}", self.tenant.as_uuid()).into_bytes(),
        }
    }

    /// Stores a value, replacing any previous value.
    pub async fn put(&self, name: &str, value: &SecretString) -> Result<(), VaultError> {
        let nonce: Nonce<_> = Nonce::generate();
        let aad = self.aad(name);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: value.expose_secret().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Encrypt(name.to_owned()))?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO credentials (tenant_id, name, nonce, ciphertext, user_id) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT {ENTRY} \
             DO UPDATE SET nonce = $3, ciphertext = $4, updated_at = now()"
        )))
        .bind(self.tenant.as_uuid())
        .bind(name)
        .bind(nonce.as_slice())
        .bind(ciphertext)
        .bind(self.user)
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Reads a value.
    pub async fn get(&self, name: &str) -> Result<Option<SecretString>, VaultError> {
        let row = sqlx::query(
            "SELECT nonce, ciphertext FROM credentials WHERE tenant_id = $1 AND name = $2 \
             AND user_id IS NOT DISTINCT FROM $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(name)
        .bind(self.user)
        .fetch_optional(self.store.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let nonce: Vec<u8> = row.try_get("nonce")?;
        let ciphertext: Vec<u8> = row.try_get("ciphertext")?;
        let nonce = <[u8; NONCE_LEN]>::try_from(nonce)
            .map(Nonce::from)
            .map_err(|_| VaultError::Decrypt(name.to_owned()))?;
        let aad = self.aad(name);
        let plain = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Decrypt(name.to_owned()))?;
        let text = String::from_utf8(plain).map_err(|_| VaultError::Decrypt(name.to_owned()))?;
        Ok(Some(SecretString::from(text)))
    }

    /// Deletes a value. Returns whether it existed.
    pub async fn delete(&self, name: &str) -> Result<bool, VaultError> {
        let result = sqlx::query(
            "DELETE FROM credentials WHERE tenant_id = $1 AND name = $2 \
             AND user_id IS NOT DISTINCT FROM $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(name)
        .bind(self.user)
        .execute(self.store.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Lists the names of all entries.
    pub async fn list(&self) -> Result<Vec<String>, VaultError> {
        let rows = sqlx::query(
            "SELECT name FROM credentials WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 ORDER BY name",
        )
        .bind(self.tenant.as_uuid())
        .bind(self.user)
        .fetch_all(self.store.pool())
        .await?;
        rows.iter()
            .map(|row| row.try_get("name").map_err(VaultError::from))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_store::testing::fresh_store;

    fn key(byte: u8) -> MasterKey {
        MasterKey::from_base64(&STANDARD.encode([byte; KEY_LEN])).unwrap()
    }

    fn secret(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    #[tokio::test]
    async fn stores_reads_replaces_and_deletes() {
        let vault = Vault::new(fresh_store().await, TenantId::DEFAULT, &key(1));
        assert!(vault.get("anthropic").await.unwrap().is_none());
        vault.put("anthropic", &secret("first")).await.unwrap();
        vault.put("anthropic", &secret("second")).await.unwrap();
        vault.put("openai", &secret("other")).await.unwrap();
        assert_eq!(
            vault
                .get("anthropic")
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "second"
        );
        assert_eq!(vault.list().await.unwrap(), ["anthropic", "openai"]);
        assert!(vault.delete("anthropic").await.unwrap());
        assert!(!vault.delete("anthropic").await.unwrap());
    }

    #[tokio::test]
    async fn values_are_not_stored_in_plain_text() {
        let store = fresh_store().await;
        let vault = Vault::new(store.clone(), TenantId::DEFAULT, &key(1));
        vault
            .put("anthropic", &secret("very-secret-value"))
            .await
            .unwrap();
        let (ciphertext,): (Vec<u8>,) = sqlx::query_as("SELECT ciphertext FROM credentials")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(!ciphertext.windows(6).any(|window| window == b"secret"));
    }

    #[tokio::test]
    async fn wrong_key_fails_without_leaking() {
        let store = fresh_store().await;
        Vault::new(store.clone(), TenantId::DEFAULT, &key(1))
            .put("anthropic", &secret("value"))
            .await
            .unwrap();
        let error = Vault::new(store, TenantId::DEFAULT, &key(2))
            .get("anthropic")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "cannot decrypt vault entry `anthropic`");
    }

    #[tokio::test]
    async fn ciphertext_is_bound_to_its_entry() {
        let store = fresh_store().await;
        let vault = Vault::new(store.clone(), TenantId::DEFAULT, &key(1));
        vault.put("anthropic", &secret("value")).await.unwrap();
        sqlx::query("UPDATE credentials SET name = 'openai'")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(matches!(
            vault.get("openai").await,
            Err(VaultError::Decrypt(_))
        ));
    }

    async fn user(store: &Store, email: &str) -> uuid::Uuid {
        let id = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, 'x')")
            .bind(id)
            .bind(email)
            .execute(store.pool())
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn personal_entries_belong_to_one_member() {
        let store = fresh_store().await;
        let shared = Vault::new(store.clone(), TenantId::DEFAULT, &key(1));
        let alex = shared.personal(user(&store, "alex@example.com").await);
        let sam = shared.personal(user(&store, "sam@example.com").await);
        shared.put("anthropic", &secret("tenant")).await.unwrap();
        alex.put("anthropic", &secret("alex")).await.unwrap();
        alex.put("mcp:mail", &secret("token")).await.unwrap();
        let read = |vault: Vault| async move {
            vault
                .get("anthropic")
                .await
                .unwrap()
                .map(|value| value.expose_secret().to_owned())
        };
        assert_eq!(read(shared.clone()).await.as_deref(), Some("tenant"));
        assert_eq!(read(alex.clone()).await.as_deref(), Some("alex"));
        assert_eq!(read(sam.clone()).await, None);
        assert_eq!(shared.list().await.unwrap(), ["anthropic"]);
        assert_eq!(alex.list().await.unwrap(), ["anthropic", "mcp:mail"]);
        assert!(sam.list().await.unwrap().is_empty());
        assert!(!sam.delete("mcp:mail").await.unwrap());
        assert!(alex.delete("anthropic").await.unwrap());
        assert_eq!(read(shared.clone()).await.as_deref(), Some("tenant"));
        assert_eq!(alex.shared().user(), None);
        assert_eq!(alex.tenant(), TenantId::DEFAULT);
        sqlx::query("UPDATE credentials SET user_id = NULL WHERE name = 'mcp:mail'")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(matches!(
            shared.get("mcp:mail").await,
            Err(VaultError::Decrypt(_))
        ));
    }

    #[test]
    fn master_key_rejects_bad_input() {
        assert!(matches!(
            MasterKey::from_base64("nope!"),
            Err(VaultError::InvalidMasterKey)
        ));
        assert!(matches!(
            MasterKey::from_base64("aGVsbG8="),
            Err(VaultError::InvalidMasterKey)
        ));
    }

    #[test]
    fn master_key_debug_is_redacted() {
        assert_eq!(format!("{:?}", key(1)), "MasterKey([REDACTED])");
    }

    #[test]
    fn key_file_round_trip_and_no_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        MasterKey::create_file(&path).unwrap();
        MasterKey::from_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(MasterKey::create_file(&path).is_err());
    }
}
