//! Items the owner cleared from "Ready for you".

use crate::{Desk, DeskError};
use std::collections::HashSet;

impl Desk {
    /// Clears an item. Returns whether it was not cleared before.
    pub async fn dismiss(&self, kind: &str, item: &str) -> Result<bool, DeskError> {
        let result = sqlx::query(
            "INSERT INTO ready_dismissals (tenant_id, kind, item_id) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(self.tenant())
        .bind(kind)
        .bind(item)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Hides an item until a time. A later snooze replaces an earlier one.
    pub async fn snooze(
        &self,
        kind: &str,
        item: &str,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), DeskError> {
        sqlx::query(
            "INSERT INTO ready_dismissals (tenant_id, kind, item_id, until) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, kind, item_id) DO UPDATE SET until = $4, dismissed_at = now()",
        )
        .bind(self.tenant())
        .bind(kind)
        .bind(item)
        .bind(until)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Shows a cleared or snoozed item again. Returns whether it was hidden.
    pub async fn unsnooze(&self, kind: &str, item: &str) -> Result<bool, DeskError> {
        let result = sqlx::query(
            "DELETE FROM ready_dismissals WHERE tenant_id = $1 AND kind = $2 AND item_id = $3",
        )
        .bind(self.tenant())
        .bind(kind)
        .bind(item)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns the items of a kind that are hidden now: cleared, or snoozed until later.
    pub async fn dismissed(&self, kind: &str) -> Result<HashSet<String>, DeskError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT item_id FROM ready_dismissals WHERE tenant_id = $1 AND kind = $2 \
             AND (until IS NULL OR until > now())",
        )
        .bind(self.tenant())
        .bind(kind)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(item,)| item).collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::Desk;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    #[tokio::test]
    async fn dismissals_are_remembered_once() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        assert!(desk.dismiss("draft", "a").await.unwrap());
        assert!(!desk.dismiss("draft", "a").await.unwrap());
        assert!(desk.dismissed("draft").await.unwrap().contains("a"));
        assert!(desk.dismissed("other").await.unwrap().is_empty());
        let later = chrono::Utc::now() + chrono::Duration::hours(2);
        desk.snooze("task", "t", later).await.unwrap();
        assert!(desk.dismissed("task").await.unwrap().contains("t"));
        desk.snooze("task", "t", chrono::Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap();
        assert!(
            desk.dismissed("task").await.unwrap().is_empty(),
            "the snooze is over"
        );
        assert!(desk.unsnooze("task", "t").await.unwrap());
        assert!(!desk.unsnooze("task", "t").await.unwrap());
    }
}
