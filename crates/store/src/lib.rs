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
    /// Drops the test database when the last clone goes away.
    #[cfg(any(test, feature = "testing"))]
    test_database: Option<std::sync::Arc<testing::TestDatabase>>,
}

impl Store {
    /// Connects to Postgres.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await?;
        Ok(Self {
            pool,
            #[cfg(any(test, feature = "testing"))]
            test_database: None,
        })
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
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const PREFIX: &str = "lighthouse_test_";
    /// Test databases older than this are leftovers of runs that did not clean up.
    const LEFTOVER_AGE: Duration = Duration::from_secs(2 * 3600);

    /// A database that one test created. Dropping it drops the database.
    #[derive(Debug)]
    pub struct TestDatabase {
        admin_url: String,
        name: String,
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let admin_url = self.admin_url.clone();
            let name = self.name.clone();
            let dropped = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(async move {
                    let mut admin = PgConnection::connect(&admin_url).await.ok()?;
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
                    )))
                    .execute(&mut admin)
                    .await
                    .ok()
                })
            });
            let _ = dropped.join();
        }
    }

    fn now_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// Returns whether a test database is a leftover: older than two hours, or
    /// named in the old way without a creation time.
    pub fn is_leftover(name: &str, now: u64) -> bool {
        let Some(rest) = name.strip_prefix(PREFIX) else {
            return false;
        };
        match rest.split_once('_') {
            Some((created, _)) => created
                .parse::<u64>()
                .is_ok_and(|created| now.saturating_sub(created) > LEFTOVER_AGE.as_secs()),
            None => true,
        }
    }

    async fn drop_leftovers(admin: &mut PgConnection) {
        let names: Vec<(String,)> = sqlx::query_as(
            "SELECT datname FROM pg_database WHERE datname LIKE 'lighthouse\\_test\\_%'",
        )
        .fetch_all(&mut *admin)
        .await
        .unwrap_or_default();
        let now = now_seconds();
        for (name,) in names.into_iter().filter(|(name,)| is_leftover(name, now)) {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
            )))
            .execute(&mut *admin)
            .await;
        }
    }

    /// Creates a migrated, empty database for one test. It is dropped when the
    /// last clone of the store goes away. Leftovers of earlier runs are dropped
    /// once per process.
    ///
    /// Reads the server URL from `DATABASE_URL`.
    pub async fn fresh_store() -> Store {
        static CLEANED: OnceLock<()> = OnceLock::new();
        let base =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point to a test Postgres");
        let name = format!(
            "{PREFIX}{}_{}",
            now_seconds(),
            uuid::Uuid::new_v4().simple()
        );
        let mut admin = PgConnection::connect(&base)
            .await
            .expect("connect to Postgres");
        if CLEANED.set(()).is_ok() {
            drop_leftovers(&mut admin).await;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut admin)
            .await
            .expect("create test database");
        let (server, _) = base.rsplit_once('/').expect("DATABASE_URL has a path");
        let mut store = Store::connect(&format!("{server}/{name}"))
            .await
            .expect("connect to test database");
        store.test_database = Some(Arc::new(TestDatabase {
            admin_url: base.clone(),
            name,
        }));
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

    #[test]
    fn only_old_and_unnamed_test_databases_are_leftovers() {
        use super::testing::is_leftover;
        assert!(is_leftover("lighthouse_test_0123456789abcdef", 10_000));
        assert!(is_leftover("lighthouse_test_100_abc", 100 + 3 * 3600));
        assert!(!is_leftover("lighthouse_test_100_abc", 200));
        assert!(!is_leftover("lighthouse", 10_000));
        assert!(!is_leftover("lighthouse_test_soon_abc", 10_000));
    }

    #[tokio::test]
    async fn a_test_database_is_dropped_after_the_test() {
        let store = fresh_store().await;
        let (name,): (String,) = sqlx::query_as("SELECT current_database()::text")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let copy = store.clone();
        drop(store);
        drop(copy);
        let base = std::env::var("DATABASE_URL").unwrap();
        let mut admin = <sqlx::postgres::PgConnection as sqlx::Connection>::connect(&base)
            .await
            .unwrap();
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM pg_database WHERE datname = $1")
                .bind(&name)
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let store = fresh_store().await;
        store.migrate().await.unwrap();
    }
}
