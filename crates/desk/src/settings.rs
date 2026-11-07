//! Settings the owner changes in the apps, stored as JSON per key.

use crate::{Desk, DeskError};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

impl Desk {
    /// Reads a setting, or `None` when it was never set or no longer fits.
    pub async fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, DeskError> {
        let row: Option<(Value,)> =
            sqlx::query_as("SELECT value FROM owner_settings WHERE tenant_id = $1 AND key = $2")
                .bind(self.tenant())
                .bind(key)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.and_then(|(value,)| serde_json::from_value(value).ok()))
    }

    /// Stores a setting.
    pub async fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<(), DeskError> {
        let value =
            serde_json::to_value(value).map_err(|error| crate::invalid(error.to_string()))?;
        sqlx::query(
            "INSERT INTO owner_settings (tenant_id, key, value) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant_id, key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(self.tenant())
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    #[tokio::test]
    async fn settings_are_stored_and_replaced() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        assert_eq!(desk.setting::<Value>("budget").await.unwrap(), None);
        desk.set_setting("budget", &json!({"monthly_eur": 20}))
            .await
            .unwrap();
        desk.set_setting("budget", &json!({"monthly_eur": 30}))
            .await
            .unwrap();
        let budget: Value = desk.setting("budget").await.unwrap().unwrap();
        assert_eq!(budget["monthly_eur"], 30);
        assert_eq!(
            desk.setting::<u32>("budget").await.unwrap(),
            None,
            "a value that no longer fits"
        );
    }
}
