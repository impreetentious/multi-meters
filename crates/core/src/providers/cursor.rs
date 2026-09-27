use async_trait::async_trait;
use chrono::{Duration, Local, TimeZone, Utc};
use reqwest::Method;
use serde_json::{json, Value};

use super::{spend_widgets, widget, widget_labeled, Provider};
use crate::http::{Http, HttpResponse};
use crate::json::{jwt_exp, jwt_payload, num, num_at, str_at};
use crate::models::*;
use crate::paths;

pub struct CursorProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl CursorProvider {
    pub fn new() -> Self {
        let info = ProviderInfo {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            icon: "cursor".into(),
            links: vec![
                ProviderLink {
                    label: "Status".into(),
                    url: "https://status.cursor.com/".into(),
                },
                ProviderLink {
                    label: "Dashboard".into(),
                    url: "https://www.cursor.com/dashboard".into(),
                },
            ],
        };
        let mut widgets = vec![
            widget("cursor.credits", "cursor", "Credits", false),
            widget_labeled("cursor.usage", "cursor", "Total Usage", "Total usage", true),
            widget("cursor.requests", "cursor", "Requests", false),
            widget_labeled("cursor.auto", "cursor", "Auto Usage", "Auto usage", true),
            widget_labeled("cursor.api", "cursor", "API Usage", "API usage", true),
            widget_labeled(
                "cursor.onDemand",
                "cursor",
                "Extra Usage",
                "On-demand",
                true,
            ),
        ];
        widgets.extend(spend_widgets("cursor"));
        Self { info, widgets }
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_tokens().is_some()
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let mut tokens = match load_tokens() {
            Some(t) => t,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Not logged in. Sign in via Cursor app or run `agent login`.",
                )
            }
        };
        if needs_refresh(&tokens.0) {
            if let Some(rt) = tokens.1.as_deref() {
                if let Ok(fresh) = refresh_token(http, rt).await {
                    tokens.0 = fresh.clone();
                    if let Err(error) = save_access_token(&fresh) {
                        tracing::warn!(%error, "could not persist refreshed Cursor credentials");
                    }
                }
            }
        }
        let access = tokens.0.clone();
        let usage_res = match connect(
            http,
            "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            &access,
        )
        .await
        {
            Ok(r) if r.status == 401 || r.status == 403 => {
                if let Some(rt) = tokens.1.as_deref() {
                    if let Ok(fresh) = refresh_token(http, rt).await {
                        if let Err(error) = save_access_token(&fresh) {
                            tracing::warn!(%error, "could not persist refreshed Cursor credentials");
                        }
                        connect(http, "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage", &fresh).await
                    } else {
                        Ok(r)
                    }
                } else {
                    Ok(r)
                }
            }
            other => other,
        };
        let usage_res = match usage_res {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach Cursor. Check your connection.",
                )
            }
        };
        if usage_res.status == 401 || usage_res.status == 403 {
            return ProviderSnapshot::err(
                &self.info,
                "Session expired. Sign in via Cursor app or run `agent login`.",
            );
        }
        if !usage_res.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Cursor request failed (HTTP {}).", usage_res.status),
            );
        }
        let usage = match usage_res.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Cursor usage data unavailable. Try again later.",
                )
            }
        };

        let plan_res = connect(
            http,
            "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo",
            &access,
        )
        .await
        .ok();
        let credits_res = connect(
            http,
            "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance",
            &access,
        )
        .await
        .ok();
        let stripe = cookie_get(http, "https://cursor.com/api/auth/stripe", &access)
            .await
            .ok();
        let summary = cookie_get(http, "https://cursor.com/api/usage-summary", &access)
            .await
            .ok();

        let plan = plan_res.and_then(|r| r.json()).and_then(|v| {
            v.pointer("/planInfo/planName")
                .and_then(|x| x.as_str())
                .or_else(|| str_at(&v, "planName"))
                .or_else(|| str_at(&v, "membershipType"))
                .map(|s| s.to_string())
        });
        let credits = credits_res.and_then(|r| r.json());
        let stripe_cents = stripe
            .and_then(|r| r.json())
            .map(stripe_prepaid_cents)
            .unwrap_or(0.0);

        let mut lines = match map_usage(&usage, plan.as_deref(), credits.as_ref(), stripe_cents) {
            Ok(l) => l,
            Err(msg) => return ProviderSnapshot::err(&self.info, &msg),
        };

        if let Some(summary) = summary.and_then(|r| r.json()) {
            merge_summary(&mut lines, &summary);
        }

        lines.extend(cursor_spend(http, &access).await);
        ProviderSnapshot::ok(&self.info, plan, lines)
    }
}

struct Tokens(String, Option<String>);

fn load_tokens() -> Option<Tokens> {
    let dbs = [
        Some(paths::cursor_state_db()),
        paths::cursor_state_db_alternate(),
    ];
    for db in dbs.into_iter().flatten() {
        if !db.exists() {
            continue;
        }
        let access = paths::sqlite_value(
            &db,
            "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken' LIMIT 1",
        );
        let refresh = paths::sqlite_value(
            &db,
            "SELECT value FROM ItemTable WHERE key = 'cursorAuth/refreshToken' LIMIT 1",
        );
        if let Some(a) = access.filter(|s| !s.trim().is_empty()) {
            return Some(Tokens(a, refresh));
        }
    }
    let access = paths::cred_read("cursor-access-token", None)?;
    let refresh = paths::cred_read("cursor-refresh-token", None);
    Some(Tokens(access, refresh))
}

fn save_access_token(token: &str) -> anyhow::Result<()> {
    let db = [
        Some(paths::cursor_state_db()),
        paths::cursor_state_db_alternate(),
    ]
    .into_iter()
    .flatten()
    .find(|path| path.exists());
    if let Some(db) = db {
        let conn = rusqlite::Connection::open(&db)?;
        conn.execute(
            "INSERT OR REPLACE INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', ?1)",
            [token],
        )?;
    } else {
        paths::cred_write("cursor-access-token", None, token)?;
    }
    Ok(())
}

fn needs_refresh(token: &str) -> bool {
    match jwt_exp(token) {
        Some(exp) => exp - Utc::now() <= Duration::minutes(5),
        None => true,
    }
}

async fn refresh_token(http: &Http, refresh: &str) -> anyhow::Result<String> {
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB",
        "refresh_token": refresh
    });
    let res = http
        .post_json("https://api2.cursor.sh/oauth/token", &[], &body)
        .await?;
    if !res.ok() {
        anyhow::bail!("cursor refresh {}", res.status);
    }
    let v = res.json().ok_or_else(|| anyhow::anyhow!("bad body"))?;
    str_at(&v, "access_token")
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no token"))
}

async fn connect(http: &Http, url: &str, token: &str) -> anyhow::Result<HttpResponse> {
    http.send(
        Method::POST,
        url,
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Connect-Protocol-Version", "1"),
        ],
        Some(b"{}".to_vec()),
        10,
    )
    .await
}

fn session_cookie(token: &str) -> Option<(String, String)> {
    let payload = jwt_payload(token)?;
    let sub = str_at(&payload, "sub")?.to_string();
    let user_id = sub.split('|').nth(1).unwrap_or(&sub);
    if user_id.is_empty() {
        return None;
    }
    Some((user_id.to_string(), format!("{user_id}%3A%3A{token}")))
}

async fn cookie_get(http: &Http, url: &str, token: &str) -> anyhow::Result<HttpResponse> {
    let (_, session) = session_cookie(token).ok_or_else(|| anyhow::anyhow!("no session"))?;
    http.send(
        Method::GET,
        url,
        &[("Cookie", &format!("WorkosCursorSessionToken={session}"))],
        None,
        10,
    )
    .await
}

fn map_usage(
    usage: &Value,
    plan_name: Option<&str>,
    credit_grants: Option<&Value>,
    stripe_cents: f64,
) -> Result<Vec<MetricLine>, String> {
    if usage.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return Err("No active Cursor subscription.".into());
    }
    let plan_usage = usage.get("planUsage").cloned().unwrap_or(json!({}));
    let limit = num_at(&plan_usage, "limit");
    let total_percent = num_at(&plan_usage, "totalPercentUsed");
    if limit.is_none() && total_percent.is_none() {
        return Err("Total usage limit missing from API response.".into());
    }

    let mut lines = Vec::new();
    append_credits(credit_grants, stripe_cents, &mut lines);

    let plan_used_cents = num_at(&plan_usage, "totalSpend").unwrap_or_else(|| {
        (limit.unwrap_or(0.0) - num_at(&plan_usage, "remaining").unwrap_or(0.0)).max(0.0)
    });
    let computed = limit
        .filter(|l| *l > 0.0)
        .map(|l| plan_used_cents / l * 100.0)
        .unwrap_or(0.0);
    let total_usage_percent = total_percent.unwrap_or(computed);
    let cycle = billing_cycle(usage);
    let spend_limit = usage.get("spendLimitUsage").cloned().unwrap_or(json!({}));
    let is_team = plan_name.unwrap_or("").eq_ignore_ascii_case("team")
        || str_at(&spend_limit, "limitType")
            .map(|s| s.eq_ignore_ascii_case("team"))
            .unwrap_or(false)
        || num_at(&spend_limit, "pooledLimit").unwrap_or(0.0) > 0.0;

    if is_team {
        if let Some(limit_cents) = limit {
            lines.push(MetricLine::dollars_progress(
                "Total usage",
                crate::json::cents_to_dollars(plan_used_cents),
                crate::json::cents_to_dollars(limit_cents),
                cycle.0,
                cycle.1,
            ));
        }
    } else {
        lines.push(MetricLine::percent(
            "Total usage",
            total_usage_percent,
            cycle.0,
            cycle.1,
        ));
    }
    if let Some(auto) = num_at(&plan_usage, "autoPercentUsed") {
        lines.push(MetricLine::percent("Auto usage", auto, cycle.0, cycle.1));
    }
    if let Some(api) = num_at(&plan_usage, "apiPercentUsed") {
        lines.push(MetricLine::percent("API usage", api, cycle.0, cycle.1));
    }
    if spend_limit
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false)
    {
        let od_limit = num_at(&spend_limit, "individualLimit")
            .or_else(|| num_at(&spend_limit, "pooledLimit"))
            .unwrap_or(0.0);
        let remaining = num_at(&spend_limit, "individualRemaining")
            .or_else(|| num_at(&spend_limit, "pooledRemaining"))
            .unwrap_or(0.0);
        let spent = [
            num_at(&spend_limit, "individualUsed"),
            num_at(&spend_limit, "pooledUsed"),
            num_at(&spend_limit, "totalSpend"),
        ]
        .into_iter()
        .flatten()
        .find(|n| *n > 0.0)
        .unwrap_or_else(|| (od_limit - remaining).max(0.0));
        if od_limit > 0.0 {
            lines.push(MetricLine::dollars_progress(
                "On-demand",
                crate::json::cents_to_dollars(spent),
                crate::json::cents_to_dollars(od_limit),
                cycle.0,
                cycle.1,
            ));
        } else if spent > 0.0 {
            lines.push(MetricLine::values_dollars(
                "On-demand",
                crate::json::cents_to_dollars(spent),
            ));
        }
    }
    Ok(lines)
}

fn stripe_prepaid_cents(body: Value) -> f64 {
    num_at(&body, "customerBalance")
        .filter(|n| *n < 0.0)
        .map(|n| n.abs())
        .unwrap_or(0.0)
}

fn append_credits(grants: Option<&Value>, stripe_cents: f64, lines: &mut Vec<MetricLine>) {
    let has_grants =
        grants.and_then(|g| g.get("hasCreditGrants").and_then(|v| v.as_bool())) == Some(true);
    let grant_total = if has_grants {
        grants.and_then(|g| num_at(g, "totalCents")).unwrap_or(0.0)
    } else {
        0.0
    };
    let grant_used = if has_grants && grant_total > 0.0 {
        grants.and_then(|g| num_at(g, "usedCents")).unwrap_or(0.0)
    } else {
        0.0
    };
    let combined_total = if grant_total > 0.0 { grant_total } else { 0.0 } + stripe_cents;
    if combined_total <= 0.0 {
        return;
    }
    let remaining = (combined_total - grant_used).max(0.0);
    lines.push(MetricLine::values_dollars(
        "Credits",
        crate::json::cents_to_dollars(remaining),
    ));
}

fn billing_cycle(usage: &Value) -> (Option<chrono::DateTime<Utc>>, i64) {
    let start = usage
        .get("billingCycleStart")
        .and_then(crate::json::as_date);
    let end = usage.get("billingCycleEnd").and_then(crate::json::as_date);
    let period = match (start, end) {
        (Some(s), Some(e)) => (e - s).num_milliseconds().max(MONTH_MS),
        _ => MONTH_MS,
    };
    (end, period)
}

fn merge_summary(lines: &mut Vec<MetricLine>, summary: &Value) {
    if let Some(used) = num_at(summary, "includedRequestsUsed").or_else(|| {
        summary
            .pointer("/individualUsage/includedRequestsUsed")
            .and_then(num)
    }) {
        if let Some(limit) = num_at(summary, "includedRequestsLimit").or_else(|| {
            summary
                .pointer("/individualUsage/includedRequestsLimit")
                .and_then(num)
        }) {
            if limit > 0.0 {
                lines.push(MetricLine::Progress {
                    label: "Requests".into(),
                    used,
                    limit,
                    format: ProgressFormat::Count {
                        suffix: "reqs".into(),
                    },
                    resets_at: None,
                    period_duration_ms: Some(MONTH_MS),
                    color_hex: None,
                });
            }
        }
    }
}

async fn cursor_spend(http: &Http, token: &str) -> Vec<MetricLine> {
    let end = Utc::now();
    let start = end - Duration::days(30);
    let url = format!(
        "https://cursor.com/api/dashboard/export-usage-events-csv?startDate={}&endDate={}&strategy=tokens",
        start.timestamp_millis(),
        end.timestamp_millis()
    );
    let Ok((_, session)) = session_cookie(token).ok_or(()) else {
        return vec![];
    };
    let Ok(res) = http
        .send(
            Method::GET,
            &url,
            &[
                ("Cookie", &format!("WorkosCursorSessionToken={session}")),
                ("Accept", "text/csv"),
            ],
            None,
            30,
        )
        .await
    else {
        return vec![];
    };
    if !res.ok() {
        return vec![];
    }
    parse_csv_spend(&res.text())
}

fn parse_csv_spend(csv: &str) -> Vec<MetricLine> {
    let mut days: std::collections::BTreeMap<chrono::NaiveDate, crate::spend::DaySpend> =
        Default::default();
    let mut lines = csv.lines();
    let header = match lines.next() {
        Some(h) => h,
        None => return vec![],
    };
    let cols = split_csv(header);
    let date_i = cols.iter().position(|c| {
        c.eq_ignore_ascii_case("date")
            || c.eq_ignore_ascii_case("timestamp")
            || c.eq_ignore_ascii_case("day")
    });
    let component_names = [
        "Input (w/ Cache Write)",
        "Input (w/o Cache Write)",
        "Cache Read",
        "Output Tokens",
    ];
    let component_indices: Vec<_> = component_names
        .iter()
        .filter_map(|name| {
            cols.iter()
                .position(|column| column.eq_ignore_ascii_case(name))
        })
        .collect();
    let has_components = component_indices.len() == component_names.len();
    let token_indices: Vec<_> = if has_components {
        component_indices.clone()
    } else if let Some(total) = cols
        .iter()
        .position(|column| column.eq_ignore_ascii_case("Total Tokens"))
    {
        vec![total]
    } else {
        cols.iter()
            .enumerate()
            .filter(|(_, column)| column.to_ascii_lowercase().contains("token"))
            .map(|(index, _)| index)
            .collect()
    };
    let cost_i = cols.iter().position(|c| {
        c.to_ascii_lowercase().contains("cost") || c.to_ascii_lowercase().contains("usd")
    });
    let model_i = cols
        .iter()
        .position(|column| column.eq_ignore_ascii_case("Model"));
    let Some(date_i) = date_i else { return vec![] };
    for row in lines {
        let parts = split_csv(row);
        if parts.len() <= date_i {
            continue;
        }
        let day = crate::json::parse_iso(&parts[date_i])
            .map(|d| d.with_timezone(&chrono::Local).date_naive())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(parts[date_i].trim(), "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .and_then(|date| Local.from_local_datetime(&date).single())
                    .map(|date| date.date_naive())
            })
            .or_else(|| chrono::NaiveDate::parse_from_str(parts[date_i].trim(), "%Y-%m-%d").ok());
        let Some(day) = day else { continue };
        let token_values: Option<Vec<f64>> = token_indices
            .iter()
            .map(|index| parts.get(*index).and_then(|value| parse_csv_number(value)))
            .collect();
        let Some(tokens) = token_values.map(|values| values.into_iter().sum::<f64>()) else {
            continue;
        };
        let model = model_i
            .and_then(|index| parts.get(index))
            .map(|model| model.trim())
            .filter(|model| !model.is_empty());
        let carried_cost = cost_i
            .and_then(|i| parts.get(i))
            .and_then(|value| parse_csv_number(value));
        let estimated_cost = if has_components {
            let values: Option<Vec<f64>> = component_indices
                .iter()
                .map(|index| parts.get(*index).and_then(|value| parse_csv_number(value)))
                .collect();
            values.and_then(|values| {
                model.and_then(|model| {
                    crate::pricing::estimate_aggregated_cost(
                        model,
                        crate::pricing::TokenBreakdown {
                            cache_write_5m: values[0],
                            input: values[1],
                            cache_read: values[2],
                            output: values[3],
                            ..crate::pricing::TokenBreakdown::default()
                        },
                    )
                })
            })
        } else {
            None
        };
        let e = days.entry(day).or_default();
        if let Some(dollars) = carried_cost.or(estimated_cost) {
            e.dollars += dollars;
            e.tokens += tokens;
        } else if let Some(model) = model {
            e.unknown_models.insert(model.to_string());
        }
    }
    crate::spend::period_lines(days)
}

fn parse_csv_number(raw: &str) -> Option<f64> {
    let normalized = raw.trim().replace(',', "");
    if normalized.is_empty() {
        return Some(0.0);
    }
    normalized.parse::<f64>().ok().filter(|value| *value >= 0.0)
}

fn split_csv(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in row.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_cookie_splits_sub_and_encodes() {
        let token = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhdXRoMHx1c2VyMTIzIn0.x";
        let (user, cookie) = session_cookie(token).expect("cookie");
        assert_eq!(user, "user123");
        assert_eq!(cookie, format!("user123%3A%3A{token}"));
    }

    #[test]
    fn on_demand_label_and_cents() {
        let usage = json!({
            "planUsage": { "totalPercentUsed": 10.0 },
            "spendLimitUsage": { "individualLimit": 2000.0, "individualUsed": 500.0 }
        });
        let lines = map_usage(&usage, None, None, 0.0).unwrap();
        let od = lines
            .iter()
            .find(|l| l.label() == "On-demand")
            .expect("on-demand");
        match od {
            MetricLine::Progress { used, limit, .. } => {
                assert!((*used - 5.0).abs() < 0.001);
                assert!((*limit - 20.0).abs() < 0.001);
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn cursor_csv_sums_all_token_components() {
        let csv = format!(
            "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens\n\
             {},composer-2,10,20,30,40\n",
            Local::now().format("%Y-%m-%d")
        );
        let lines = parse_csv_spend(&csv);
        let today = lines.iter().find(|line| line.label() == "Today").unwrap();
        let MetricLine::Values { values, .. } = today else {
            panic!("expected values")
        };
        let tokens = values
            .iter()
            .find(|value| value.kind == MetricKind::Count)
            .unwrap();
        assert_eq!(tokens.number, 100.0);
        let dollars = values
            .iter()
            .find(|value| value.kind == MetricKind::Dollars)
            .unwrap();
        assert!((dollars.number - 0.000121).abs() < 0.000_000_1);
    }

    #[test]
    fn cursor_csv_flags_unknown_models_without_mixing_unpriced_tokens_into_totals() {
        let csv = format!(
            "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens\n\
             {},future-model-with-no-price,10,20,30,40\n",
            Local::now().format("%Y-%m-%d")
        );
        let lines = parse_csv_spend(&csv);
        let today = lines.iter().find(|line| line.label() == "Today").unwrap();
        let MetricLine::Values {
            values,
            unknown_models,
            ..
        } = today
        else {
            panic!("expected values")
        };
        assert_eq!(unknown_models, &["future-model-with-no-price"]);
        assert!(values.iter().all(|value| value.number == 0.0));
    }
}
