//! How the owner presents themselves: headline, bio, audience, voice and topics.

use crate::{Desk, DeskError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// The owner's profile for writing posts and decks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
#[serde(default)]
pub struct BrandProfile {
    /// One line on who the owner is.
    pub headline: Option<String>,
    /// A few sentences about the owner.
    pub bio: Option<String>,
    /// Who the owner writes for.
    pub audience: Option<String>,
    /// Notes on tone and style, in the owner's words.
    pub voice: Option<String>,
    /// Topics the owner stands for.
    pub topics: Vec<String>,
    /// Words, claims or topics to avoid.
    pub avoid: Vec<String>,
    /// Last change.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Desk {
    /// Returns the profile. An empty profile if none was saved.
    pub async fn brand_profile(&self) -> Result<BrandProfile, DeskError> {
        let profile: Option<BrandProfile> = sqlx::query_as(
            "SELECT headline, bio, audience, voice, topics, avoid, updated_at FROM brand_profile \
             WHERE tenant_id = $1",
        )
        .bind(self.tenant())
        .fetch_optional(self.pool())
        .await?;
        Ok(profile.unwrap_or_default())
    }

    /// Replaces the profile.
    pub async fn save_brand_profile(
        &self,
        profile: &BrandProfile,
    ) -> Result<BrandProfile, DeskError> {
        let trim = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Ok(sqlx::query_as(
            "INSERT INTO brand_profile (tenant_id, headline, bio, audience, voice, topics, avoid) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (tenant_id) DO UPDATE SET \
             headline = $2, bio = $3, audience = $4, voice = $5, topics = $6, avoid = $7, \
             updated_at = now() RETURNING headline, bio, audience, voice, topics, avoid, updated_at",
        )
        .bind(self.tenant())
        .bind(trim(&profile.headline))
        .bind(trim(&profile.bio))
        .bind(trim(&profile.audience))
        .bind(trim(&profile.voice))
        .bind(&profile.topics)
        .bind(&profile.avoid)
        .fetch_one(self.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    #[tokio::test]
    async fn the_profile_starts_empty_and_is_replaced() {
        let desk = Desk::new(fresh_store().await, TenantId::DEFAULT);
        assert_eq!(desk.brand_profile().await.unwrap(), BrandProfile::default());
        let saved = desk
            .save_brand_profile(&BrandProfile {
                headline: Some(" Builds tools for small teams ".to_owned()),
                voice: Some(" ".to_owned()),
                topics: vec!["Automation".to_owned()],
                ..BrandProfile::default()
            })
            .await
            .unwrap();
        assert_eq!(
            saved.headline.as_deref(),
            Some("Builds tools for small teams")
        );
        assert_eq!(saved.voice, None);
        assert_eq!(desk.brand_profile().await.unwrap().topics, ["Automation"]);
    }
}
