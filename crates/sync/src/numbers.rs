//! Key numbers computed from the cache: pipeline, cash, burn, runway, open and
//! overdue invoices, revenue per month and the value of holdings, and a cash
//! forecast from them.
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
    /// Revenue of the months asked for.
    pub revenue_total: f64,
    /// Revenue of the same number of months before them, for comparison.
    pub revenue_previous_total: f64,
    /// Change against the months before, in percent, if there was revenue then.
    pub revenue_change_percent: Option<f64>,
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

/// Start of the month that lies `months - 1` months before the month of `now`.
pub fn period_start(now: DateTime<Utc>, months: i32) -> DateTime<Utc> {
    use chrono::{Datelike, Months, TimeZone};
    let first = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    first
        .checked_sub_months(Months::new(u32::try_from(months - 1).unwrap_or(0)))
        .unwrap_or(first)
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

        let totals: Vec<(String, f64, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), \
             coalesce(sum(amount) FILTER (WHERE coalesce(starts_at, occurred_at) >= $6), 0), \
             coalesce(sum(amount) FILTER (WHERE coalesce(starts_at, occurred_at) < $6), 0) \
             FROM synced_items WHERE tenant_id = $1 AND kind = 'invoice' AND {ORG} \
             AND NOT 'payable' = ANY(flags) AND coalesce(lower(state), '') NOT IN ('void', 'canceled', 'cancelled', 'draft') \
             AND $3::text[] IS NOT NULL AND $4::timestamptz IS NOT NULL AND $5::int IS NOT NULL \
             AND coalesce(starts_at, occurred_at) >= $7 GROUP BY 1"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .bind(now)
        .bind(months.max(1))
        .bind(period_start(now, months.max(1)))
        .bind(period_start(now, 2 * months.max(1)))
        .fetch_all(self.pool())
        .await?;
        for (currency, current, previous) in totals {
            let numbers = entry(&mut by_currency, currency);
            numbers.revenue_total = current;
            numbers.revenue_previous_total = previous;
            numbers.revenue_change_percent =
                (previous > 0.0).then(|| (current - previous) / previous * 100.0);
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

/// One month of the cash forecast.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForecastMonth {
    /// `YYYY-MM`.
    pub period: String,
    /// Cash at the start of the month.
    pub opening: f64,
    /// Open issued invoices due this month; overdue ones count in the first month.
    pub invoices_in: f64,
    /// Open received invoices due this month; overdue ones count in the first month.
    pub payables_out: f64,
    /// Open deals expected to close this month, times their probability. Deals
    /// without a close date (`ends_at`) are spread evenly over the months.
    pub pipeline_weighted: f64,
    /// The usual monthly net outflow, from the last 90 days.
    pub burn: f64,
    /// Cash at the end of the month.
    pub closing: f64,
}

/// The cash forecast of one currency.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CurrencyForecast {
    /// Currency code.
    pub currency: String,
    /// The months ahead, this month first.
    pub months: Vec<ForecastMonth>,
}

/// Amounts per currency and month bucket, where `None` has no date.
type Buckets = BTreeMap<String, BTreeMap<Option<String>, f64>>;

/// Rolls cash forward: opening, plus invoices and weighted pipeline, minus
/// payables and burn. `periods` are the months ahead, this month first.
pub fn roll_forward(
    numbers: &CurrencyNumbers,
    periods: &[String],
    receivables: &BTreeMap<Option<String>, f64>,
    payables: &BTreeMap<Option<String>, f64>,
    pipeline: &BTreeMap<Option<String>, f64>,
) -> Vec<ForecastMonth> {
    let first = periods.first().cloned().unwrap_or_default();
    let due = |buckets: &BTreeMap<Option<String>, f64>, period: &str| -> f64 {
        buckets
            .iter()
            .filter(|(month, _)| match month {
                Some(month) if period == first => month.as_str() <= period,
                Some(month) => month == period,
                None => period == first,
            })
            .map(|(_, amount)| amount)
            .sum()
    };
    let undated = pipeline.get(&None).copied().unwrap_or_default() / periods.len().max(1) as f64;
    let mut cash = numbers.cash;
    periods
        .iter()
        .map(|period| {
            let dated: f64 = pipeline
                .iter()
                .filter(|(month, _)| {
                    month.as_deref() == Some(period.as_str())
                        || (period == &first
                            && month
                                .as_deref()
                                .is_some_and(|month| month < period.as_str()))
                })
                .map(|(_, amount)| amount)
                .sum();
            let month = ForecastMonth {
                period: period.clone(),
                opening: cash,
                invoices_in: due(receivables, period),
                payables_out: due(payables, period),
                pipeline_weighted: dated + undated,
                burn: numbers.monthly_burn,
                closing: 0.0,
            };
            cash = month.opening + month.invoices_in + month.pipeline_weighted
                - month.payables_out
                - month.burn;
            ForecastMonth {
                closing: cash,
                ..month
            }
        })
        .collect()
}

impl SyncStore {
    /// Forecasts cash for `months` months, this month first, per currency.
    pub async fn forecast(
        &self,
        org: Option<&str>,
        months: i32,
        now: DateTime<Utc>,
    ) -> Result<Vec<CurrencyForecast>, sqlx::Error> {
        use chrono::Months;
        let months = months.clamp(1, 24);
        let start = period_start(now, 1);
        let periods: Vec<String> = (0..months)
            .filter_map(|offset| start.checked_add_months(Months::new(u32::try_from(offset).ok()?)))
            .map(|month| month.format("%Y-%m").to_string())
            .collect();
        let numbers = self.numbers(org, 1, now).await?;
        let tenant = self.tenant_id();
        let open = "(state IS NULL OR lower(state) <> ALL($3))";
        let rows: Vec<(String, String, Option<String>, f64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(currency, ''), \
             CASE WHEN kind = 'deal' THEN 'deal' WHEN 'payable' = ANY(flags) THEN 'payable' ELSE 'receivable' END, \
             to_char(date_trunc('month', ends_at), 'YYYY-MM'), \
             coalesce(sum(CASE WHEN kind = 'deal' AND data->>'probability' ~ '^[0-9]+(\\.[0-9]+)?$' \
                 THEN amount * (data->>'probability')::float8 / 100 ELSE amount END), 0) \
             FROM synced_items WHERE tenant_id = $1 AND kind IN ('invoice', 'deal') AND {ORG} AND {open} \
             GROUP BY 1, 2, 3"
        )))
        .bind(tenant)
        .bind(org)
        .bind(closed())
        .fetch_all(self.pool())
        .await?;
        let mut buckets: BTreeMap<&str, Buckets> = BTreeMap::new();
        for (currency, kind, month, amount) in rows {
            let currency = if currency.is_empty() {
                NO_CURRENCY.to_owned()
            } else {
                currency
            };
            let kind = match kind.as_str() {
                "deal" => "deal",
                "payable" => "payable",
                _ => "receivable",
            };
            *buckets
                .entry(kind)
                .or_default()
                .entry(currency)
                .or_default()
                .entry(month)
                .or_default() += amount;
        }
        let empty = BTreeMap::new();
        let of = |kind: &str, currency: &str| {
            buckets
                .get(kind)
                .and_then(|by_currency| by_currency.get(currency))
                .unwrap_or(&empty)
        };
        Ok(numbers
            .currencies
            .iter()
            .map(|numbers| CurrencyForecast {
                currency: numbers.currency.clone(),
                months: roll_forward(
                    numbers,
                    &periods,
                    of("receivable", &numbers.currency),
                    of("payable", &numbers.currency),
                    of("deal", &numbers.currency),
                ),
            })
            .collect())
    }
}

#[cfg(test)]
mod forecast_tests {
    use super::*;

    #[test]
    fn cash_rolls_forward_with_dues_pipeline_and_burn() {
        let numbers = CurrencyNumbers {
            currency: "EUR".to_owned(),
            cash: 1000.0,
            monthly_burn: 100.0,
            ..CurrencyNumbers::default()
        };
        let periods = ["2026-11".to_owned(), "2026-12".to_owned()];
        let receivables = BTreeMap::from([
            (Some("2026-10".to_owned()), 50.0),
            (Some("2026-12".to_owned()), 200.0),
        ]);
        let payables = BTreeMap::from([(None, 30.0)]);
        let pipeline = BTreeMap::from([(None, 400.0), (Some("2026-12".to_owned()), 60.0)]);
        let months = roll_forward(&numbers, &periods, &receivables, &payables, &pipeline);
        assert_eq!(months[0].invoices_in, 50.0, "overdue counts now");
        assert_eq!(months[0].payables_out, 30.0);
        assert_eq!(months[0].pipeline_weighted, 200.0, "undated deals spread");
        assert_eq!(months[0].closing, 1000.0 + 50.0 + 200.0 - 30.0 - 100.0);
        assert_eq!(months[1].opening, months[0].closing);
        assert_eq!(months[1].pipeline_weighted, 260.0);
        assert_eq!(months[1].invoices_in, 200.0);
    }
}
