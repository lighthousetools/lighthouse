//! Key numbers computed from the cache: pipeline, cash, burn, runway, open and
//! overdue invoices, revenue per month and the value of holdings.
//!
//! Numbers are grouped by currency and never converted. `XXX` stands for items
//! without a currency.

use crate::mapping::CLOSED_STATES;
use crate::store::SyncStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Days of transactions the burn rate looks at.
const BURN_DAYS: i64 = 90;
const NO_CURRENCY: &str = "XXX";
const ORG: &str = "($2::text IS NULL OR source IN (SELECT unnest(o.sources) FROM organizations o \
                   WHERE o.tenant_id = $1 AND o.id = $2))";

/// Open deals of one stage.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StageTotal {
    /// Stage, as the state of the deals.
    pub stage: String,
    /// Number of deals.
    pub count: i64,
    /// Their value.
    pub amount: f64,
}

/// Revenue of one month.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PeriodAmount {
    /// Month as `YYYY-MM`.
    pub period: String,
    /// Sum of issued invoices dated in it.
    pub amount: f64,
}

/// The numbers of one currency.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CurrencyNumbers {
    /// Currency code.
    pub currency: String,
    /// Value of open deals.
    pub pipeline_open: f64,
    /// Value of open deals times their `probability` field in percent, where set.
    pub pipeline_weighted: f64,
    /// Open deals.
    pub deals_open: i64,
    /// Open deals without activity for 30 days.
    pub deals_cold: i64,
    /// Open deals per stage.
    pub stages: Vec<StageTotal>,
    /// Sum of account balances.
    pub cash: f64,
    /// Net outflow per month over the last 90 days. Zero when more came in.
    pub monthly_burn: f64,
    /// Cash divided by the monthly burn, if there is a burn.
    pub runway_months: Option<f64>,
    /// Open issued invoices.
    pub receivables_open: f64,
    /// Open issued invoices past their due date.
    pub receivables_overdue: f64,
    /// Number of overdue issued invoices.
    pub invoices_overdue: i64,
    /// Open received invoices.
    pub payables_open: f64,
    /// Issued invoices per month, newest first.
    pub revenue: Vec<PeriodAmount>,
    /// Value of holdings.
    pub holdings: f64,
}

/// Key numbers of one organization, or of all.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Numbers {
    /// The organization, if the numbers are for one.
    pub org: Option<String>,
    /// When they were computed.
    pub computed_at: DateTime<Utc>,
    /// Numbers per currency.
    pub currencies: Vec<CurrencyNumbers>,
}

fn entry(map: &mut BTreeMap<String, CurrencyNumbers>, currency: String) -> &mut CurrencyNumbers {
    let currency = if currency.is_empty() {
        NO_CURRENCY.to_owned()
    } else {
        currency
    };
    map.entry(currency.clone())
        .or_insert_with(|| CurrencyNumbers {
            currency,
            ..CurrencyNumbers::default()
        })
}

fn closed() -> Vec<String> {
    CLOSED_STATES
        .iter()
        .map(|state| (*state).to_owned())
        .collect()
}

impl SyncStore {
    /// Computes the key numbers. `months` is the number of revenue months.
    pub async fn numbers(
        &self,
        org: Option<&str>,
        months: i32,
        now: DateTime<Utc>,
    ) -> Result<Numbers, sqlx::Error> {
        let mut by_currency: BTreeMap<String, CurrencyNumbers> = BTreeMap::new();
        let tenant = self.tenant_id();
        let open = "(state IS NULL OR lower(state) <> ALL($3))";

        let deals: Vec<(String, String, i64, f64, f64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), coalesce(state, ''), count(*), coalesce(sum(amount), 0), \
             coalesce(sum(amount * CASE WHEN data->>'probability' ~ '^[0-9]+(\\.[0-9]+)?$' \
                 THEN (data->>'probability')::float8 / 100 ELSE 1 END), 0), \
             count(*) FILTER (WHERE 'cold' = ANY(flags)) \
             FROM synced_items WHERE tenant_id = $1 AND kind = 'deal' AND {ORG} AND {open} \
             GROUP BY 1, 2 ORDER BY 2"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .fetch_all(self.pool())
        .await?;
        for (currency, stage, count, amount, weighted, cold) in deals {
            let numbers = entry(&mut by_currency, currency);
            numbers.pipeline_open += amount;
            numbers.pipeline_weighted += weighted;
            numbers.deals_open += count;
            numbers.deals_cold += cold;
            numbers.stages.push(StageTotal {
                stage,
                count,
                amount,
            });
        }

        let sums: Vec<(String, String, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), kind, coalesce(sum(amount), 0) FROM synced_items \
             WHERE tenant_id = $1 AND kind IN ('account', 'holding') AND {ORG} \
             AND $3::text[] IS NOT NULL GROUP BY 1, 2"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .fetch_all(self.pool())
        .await?;
        for (currency, kind, amount) in sums {
            let numbers = entry(&mut by_currency, currency);
            if kind == "account" {
                numbers.cash += amount;
            } else {
                numbers.holdings += amount;
            }
        }

        let flows: Vec<(String, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), coalesce(sum(amount), 0) FROM synced_items \
             WHERE tenant_id = $1 AND kind = 'transaction' AND {ORG} AND $3::text[] IS NOT NULL \
             AND occurred_at >= $4 AND occurred_at <= $5 GROUP BY 1"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .bind(now - chrono::Duration::days(BURN_DAYS))
        .bind(now)
        .fetch_all(self.pool())
        .await?;
        for (currency, net) in flows {
            let numbers = entry(&mut by_currency, currency);
            numbers.monthly_burn = (-net).max(0.0) / (BURN_DAYS as f64 / 30.0);
        }

        let invoices: Vec<(String, f64, f64, i64, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), \
             coalesce(sum(amount) FILTER (WHERE NOT 'payable' = ANY(flags)), 0), \
             coalesce(sum(amount) FILTER (WHERE NOT 'payable' = ANY(flags) AND ends_at < $4), 0), \
             count(*) FILTER (WHERE NOT 'payable' = ANY(flags) AND ends_at < $4), \
             coalesce(sum(amount) FILTER (WHERE 'payable' = ANY(flags)), 0) \
             FROM synced_items WHERE tenant_id = $1 AND kind = 'invoice' AND {ORG} AND {open} GROUP BY 1"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .bind(now)
        .fetch_all(self.pool())
        .await?;
        for (currency, open_amount, overdue, overdue_count, payable) in invoices {
            let numbers = entry(&mut by_currency, currency);
            numbers.receivables_open = open_amount;
            numbers.receivables_overdue = overdue;
            numbers.invoices_overdue = overdue_count;
            numbers.payables_open = payable;
        }

        let revenue: Vec<(String, String, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), to_char(date_trunc('month', coalesce(starts_at, occurred_at)), 'YYYY-MM'), \
             coalesce(sum(amount), 0) FROM synced_items WHERE tenant_id = $1 AND kind = 'invoice' AND {ORG} \
             AND NOT 'payable' = ANY(flags) AND coalesce(lower(state), '') NOT IN ('void', 'canceled', 'cancelled', 'draft') \
             AND $3::text[] IS NOT NULL AND coalesce(starts_at, occurred_at) >= date_trunc('month', $4::timestamptz) - make_interval(months => $5 - 1) \
             GROUP BY 1, 2 ORDER BY 2 DESC"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .bind(now)
        .bind(months.max(1))
        .fetch_all(self.pool())
        .await?;
        for (currency, period, amount) in revenue {
            entry(&mut by_currency, currency)
                .revenue
                .push(PeriodAmount { period, amount });
        }

        let mut currencies: Vec<CurrencyNumbers> = by_currency.into_values().collect();
        for numbers in &mut currencies {
            numbers.runway_months =
                (numbers.monthly_burn > 0.0).then(|| numbers.cash / numbers.monthly_burn);
        }
        Ok(Numbers {
            org: org.map(str::to_owned),
            computed_at: now,
            currencies,
        })
    }
}
