//! Switches for automations, which otherwise live in files.

use crate::{Desk, DeskError};
use std::collections::HashMap;

impl Desk {
    /// Switches an automation on or off, overriding its file.
    pub async fn set_automation_enabled(&self, id: &str, enabled: bool) -> Result<(), DeskError> {
        sqlx::query(
            "INSERT INTO automation_settings (tenant_id, automation_id, enabled) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant_id, automation_id) DO UPDATE SET enabled = $3, updated_at = now()",
        )
        .bind(self.tenant())
        .bind(id)
        .bind(enabled)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Returns the switches the owner set, by automation ID.
    pub async fn automation_switches(&self) -> Result<HashMap<String, bool>, DeskError> {
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT automation_id, enabled FROM automation_settings WHERE tenant_id = $1",
        )
        .bind(self.tenant())
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::Desk;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    #[tokio::test]
    async fn switches_override_and_change() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        assert!(desk.automation_switches().await.unwrap().is_empty());
        desk.set_automation_enabled("follow-up", false)
            .await
            .unwrap();
        assert!(!desk.automation_switches().await.unwrap()["follow-up"]);
        desk.set_automation_enabled("follow-up", true)
            .await
            .unwrap();
        assert!(desk.automation_switches().await.unwrap()["follow-up"]);
    }
}
