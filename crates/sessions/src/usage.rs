//! Costs and tokens of sessions, from the OpenTelemetry metrics Claude Code exports.
//!
//! Claude Code sends `claude_code.cost.usage` (USD) and `claude_code.token.usage`
//! (by `type`: input, output, cacheRead, cacheCreation) with a `session.id`
//! attribute when OTLP export over HTTP with JSON is enabled.

use crate::{SessionError, Sessions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

const COST: &str = "claude_code.cost.usage";
const TOKENS: &str = "claude_code.token.usage";
/// OTLP marks cumulative sums with this temporality.
const CUMULATIVE: i64 = 2;

/// Costs and tokens of one session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct SessionUsage {
    /// Cost in USD, as Claude Code reports it.
    pub cost_usd: f64,
    /// Input tokens.
    pub input_tokens: f64,
    /// Output tokens.
    pub output_tokens: f64,
    /// Tokens read from the prompt cache.
    pub cache_read_tokens: f64,
    /// Tokens written to the prompt cache.
    pub cache_creation_tokens: f64,
    /// Last report.
    pub updated_at: Option<DateTime<Utc>>,
}

/// Usage of a session from one export, and whether it is a running total.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReport {
    /// Session ID.
    pub session_id: String,
    /// The usage.
    pub usage: SessionUsage,
    /// Whether the numbers are totals rather than increments.
    pub cumulative: bool,
}

fn attribute<'a>(attributes: &'a Value, key: &str) -> Option<&'a str> {
    attributes.as_array()?.iter().find_map(|attribute| {
        (attribute["key"] == key)
            .then(|| attribute["value"]["stringValue"].as_str())
            .flatten()
    })
}

fn number(point: &Value) -> f64 {
    point["asDouble"]
        .as_f64()
        .or_else(|| point["asInt"].as_str().and_then(|text| text.parse().ok()))
        .or_else(|| point["asInt"].as_f64())
        .unwrap_or_default()
}

/// Reads an OTLP JSON metrics export into usage per session.
pub fn parse_otlp(export: &Value) -> Vec<UsageReport> {
    let mut reports: BTreeMap<(String, bool), SessionUsage> = BTreeMap::new();
    for resource in export["resourceMetrics"].as_array().into_iter().flatten() {
        let resource_session = attribute(&resource["resource"]["attributes"], "session.id");
        for scope in resource["scopeMetrics"].as_array().into_iter().flatten() {
            for metric in scope["metrics"].as_array().into_iter().flatten() {
                let name = metric["name"].as_str().unwrap_or_default();
                if name != COST && name != TOKENS {
                    continue;
                }
                let sum = &metric["sum"];
                let cumulative = sum["aggregationTemporality"].as_i64() == Some(CUMULATIVE);
                for point in sum["dataPoints"].as_array().into_iter().flatten() {
                    let Some(session) =
                        attribute(&point["attributes"], "session.id").or(resource_session)
                    else {
                        continue;
                    };
                    let usage = reports.entry((session.to_owned(), cumulative)).or_default();
                    let value = number(point);
                    if name == COST {
                        usage.cost_usd += value;
                        continue;
                    }
                    match attribute(&point["attributes"], "type").unwrap_or_default() {
                        "input" => usage.input_tokens += value,
                        "output" => usage.output_tokens += value,
                        "cacheRead" => usage.cache_read_tokens += value,
                        "cacheCreation" => usage.cache_creation_tokens += value,
                        _ => {}
                    }
                }
            }
        }
    }
    reports
        .into_iter()
        .map(|((session_id, cumulative), usage)| UsageReport {
            session_id,
            usage,
            cumulative,
        })
        .collect()
}

impl Sessions {
    /// Stores usage reports of a node. Increments add up, totals replace.
    pub async fn record_usage(
        &self,
        node: Uuid,
        reports: &[UsageReport],
    ) -> Result<(), SessionError> {
        for report in reports {
            let usage = &report.usage;
            sqlx::query(
                "INSERT INTO session_usage (tenant_id, node_id, session_id, cost_usd, input_tokens, \
                 output_tokens, cache_read_tokens, cache_creation_tokens) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (node_id, session_id) DO UPDATE SET \
                 cost_usd = CASE WHEN $9 THEN GREATEST(session_usage.cost_usd, $4) ELSE session_usage.cost_usd + $4 END, \
                 input_tokens = CASE WHEN $9 THEN GREATEST(session_usage.input_tokens, $5) ELSE session_usage.input_tokens + $5 END, \
                 output_tokens = CASE WHEN $9 THEN GREATEST(session_usage.output_tokens, $6) ELSE session_usage.output_tokens + $6 END, \
                 cache_read_tokens = CASE WHEN $9 THEN GREATEST(session_usage.cache_read_tokens, $7) ELSE session_usage.cache_read_tokens + $7 END, \
                 cache_creation_tokens = CASE WHEN $9 THEN GREATEST(session_usage.cache_creation_tokens, $8) ELSE session_usage.cache_creation_tokens + $8 END, \
                 updated_at = now()",
            )
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(&report.session_id)
            .bind(usage.cost_usd)
            .bind(usage.input_tokens)
            .bind(usage.output_tokens)
            .bind(usage.cache_read_tokens)
            .bind(usage.cache_creation_tokens)
            .bind(report.cumulative)
            .execute(self.store.pool())
            .await?;
        }
        Ok(())
    }

    /// Returns the usage of a session, if Claude Code reported any.
    pub async fn usage(
        &self,
        node: Uuid,
        session: &str,
    ) -> Result<Option<SessionUsage>, SessionError> {
        Ok(sqlx::query_as(
            "SELECT cost_usd, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
             updated_at FROM session_usage WHERE tenant_id = $1 AND node_id = $2 AND session_id = $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(node)
        .bind(session)
        .fetch_optional(self.store.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeReport;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    fn point(session: &str, kind: Option<&str>, value: f64) -> Value {
        let mut attributes = vec![json!({"key": "session.id", "value": {"stringValue": session}})];
        if let Some(kind) = kind {
            attributes.push(json!({"key": "type", "value": {"stringValue": kind}}));
        }
        json!({"attributes": attributes, "asDouble": value})
    }

    fn export(temporality: i64, cost: f64, input: f64) -> Value {
        json!({"resourceMetrics": [{"resource": {"attributes": []}, "scopeMetrics": [{"metrics": [
            {"name": COST, "sum": {"aggregationTemporality": temporality, "dataPoints": [point("s1", None, cost)]}},
            {"name": TOKENS, "sum": {"aggregationTemporality": temporality, "dataPoints": [
                point("s1", Some("input"), input), point("s1", Some("output"), 10.0),
                point("s1", Some("cacheRead"), 5.0), point("s1", Some("cacheCreation"), 2.0)]}},
            {"name": "claude_code.session.count", "sum": {"dataPoints": [point("s1", None, 1.0)]}}
        ]}]}]})
    }

    #[test]
    fn exports_turn_into_usage_per_session() {
        let reports = parse_otlp(&export(1, 0.25, 100.0));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].usage.cost_usd, 0.25);
        assert_eq!(reports[0].usage.input_tokens, 100.0);
        assert_eq!(reports[0].usage.cache_creation_tokens, 2.0);
        assert!(!reports[0].cumulative);
        let resource = json!({"resourceMetrics": [{"resource": {"attributes": [
            {"key": "session.id", "value": {"stringValue": "s2"}}]},
            "scopeMetrics": [{"metrics": [{"name": COST, "sum": {"dataPoints": [{"asInt": "3"}]}}]}]}]});
        assert_eq!(parse_otlp(&resource)[0].session_id, "s2");
        assert!(parse_otlp(&json!({})).is_empty());
    }

    #[tokio::test]
    async fn increments_add_up_and_totals_replace() {
        let sessions = Sessions::new(fresh_store().await, TenantId::DEFAULT);
        let node = Uuid::now_v7();
        sessions
            .report(
                node,
                &NodeReport {
                    name: "mac".to_owned(),
                    sessions: Vec::new(),
                },
            )
            .await
            .unwrap();
        sessions
            .record_usage(node, &parse_otlp(&export(1, 0.25, 100.0)))
            .await
            .unwrap();
        sessions
            .record_usage(node, &parse_otlp(&export(1, 0.25, 100.0)))
            .await
            .unwrap();
        let usage = sessions.usage(node, "s1").await.unwrap().unwrap();
        assert_eq!((usage.cost_usd, usage.input_tokens), (0.5, 200.0));
        sessions
            .record_usage(node, &parse_otlp(&export(2, 0.75, 150.0)))
            .await
            .unwrap();
        let usage = sessions.usage(node, "s1").await.unwrap().unwrap();
        assert_eq!((usage.cost_usd, usage.input_tokens), (0.75, 200.0));
        assert!(sessions.usage(node, "nope").await.unwrap().is_none());
    }
}
