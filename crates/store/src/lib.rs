//! Postgres storage and migrations.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use sqlx;

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A migration failed.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// Handle to the database.
#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects to Postgres.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Applies all pending migrations.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!().run(&self.pool).await?;
        Ok(())
    }

    /// Returns the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Helpers for tests that need a real database.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::Store;
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    /// Creates a migrated, empty database for one test.
    ///
    /// Reads the server URL from `DATABASE_URL`.
    pub async fn fresh_store() -> Store {
        let base =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point to a test Postgres");
        let name = format!("lighthouse_test_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&base)
            .await
            .expect("connect to Postgres");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut admin)
            .await
            .expect("create test database");
        let (server, _) = base.rsplit_once('/').expect("DATABASE_URL has a path");
        let store = Store::connect(&format!("{server}/{name}"))
            .await
            .expect("connect to test database");
        store.migrate().await.expect("migrate");
        store
    }
}

#[cfg(test)]
mod tests {
    use super::testing::fresh_store;
    use lighthouse_core::TenantId;

    #[tokio::test]
    async fn migrations_create_the_single_tenant() {
        let store = fresh_store().await;
        let (id,): (uuid::Uuid,) = sqlx::query_as("SELECT id FROM tenants")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(id, TenantId::SINGLE.as_uuid());
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let store = fresh_store().await;
        store.migrate().await.unwrap();
    }
}
