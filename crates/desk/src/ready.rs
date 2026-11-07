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

    /// Returns the cleared items of a kind.
    pub async fn dismissed(&self, kind: &str) -> Result<HashSet<String>, DeskError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT item_id FROM ready_dismissals WHERE tenant_id = $1 AND kind = $2",
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
    }
}
