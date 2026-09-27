use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use super::{widget_labeled, Provider};
use crate::http::Http;
use crate::json::{num_at, str_at};
use crate::models::*;
use crate::paths;

pub struct DevinProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl DevinProvider {
    pub fn new() -> Self {
        Self {
            info: ProviderInfo {
                id: "devin".into(),
                display_name: "Devin".into(),
                icon: "devin".into(),
                links: vec![ProviderLink {
                    label: "Dashboard".into(),
                    url: "https://app.devin.ai/settings/plans".into(),
                }],
            },
            widgets: vec![
                widget_labeled("devin.weekly", "devin", "Weekly", "Weekly quota", true),
                widget_labeled("devin.daily", "devin", "Daily", "Daily quota", true),
                widget_labeled(
                    "devin.extra",
                    "devin",
                    "Extra Balance",
                    "Extra usage balance",
                    true,
                ),
            ],
        }
    }
}

#[async_trait]
impl Provider for DevinProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_auth().is_some()
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let auth = match load_auth() {
            Some(a) => a,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Run `devin auth login` or sign in to Devin and try again.",
                )
            }
        };
        let url = format!(
            "{}/exa.seat_management_pb.SeatManagementService/GetUserStatus",
            auth.1
        );
        let body = json!({
            "metadata": {
                "apiKey": auth.0,
                "ideName": "devin",
                "ideVersion": "1.108.2",
                "extensionName": "devin",
                "extensionVersion": "1.108.2",
                "locale": "en"
            }
        });
        let res = match http
            .post_json(&url, &[("Connect-Protocol-Version", "1")], &body)
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach Devin. Check your connection.",
                )
            }
        };
        if res.status == 401 || res.status == 403 {
            return ProviderSnapshot::err(
                &self.info,
                "Run `devin auth login` or sign in to Devin and try again.",
            );
        }
        if !res.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Devin request failed (HTTP {}).", res.status),
            );
        }
        let v = match res.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Devin usage data unavailable. Try again later.",
                )
            }
        };
        ProviderSnapshot::ok(&self.info, plan_name(&v), map_status(&v))
    }
}

fn load_auth() -> Option<(String, String)> {
    if let Some(text) = paths::read_text(&paths::devin_credentials()) {
        if let Some(key) = toml_string(&text, "windsurf_api_key") {
            let url = toml_string(&text, "api_server_url")
                .filter(|u| u.starts_with("https://"))
                .unwrap_or_else(|| "https://server.codeium.com".into());
            return Some((key, url.trim_end_matches('/').to_string()));
        }
    }
    let db = paths::devin_state_db();
    if let Some(raw) = paths::sqlite_value(
        &db,
        "SELECT value FROM ItemTable WHERE key = 'windsurfAuthStatus' LIMIT 1",
    ) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(key) = str_at(&v, "apiKey").filter(|s| !s.is_empty()) {
                return Some((key.to_string(), "https://server.codeium.com".into()));
            }
        }
    }
    None
}

fn toml_string(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix(&format!("{key}="))
            .or_else(|| line.strip_prefix(&format!("{key} =")))
        {
            let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn plan_name(v: &Value) -> Option<String> {
    v.pointer("/userStatus/planStatus/planInfo/planName")
        .and_then(|x| x.as_str())
        .or_else(|| str_at(v, "planName"))
        .or_else(|| str_at(v, "plan_name"))
        .or_else(|| v.pointer("/userStatus/planName").and_then(|x| x.as_str()))
        .map(crate::json::title_case)
}

fn map_status(v: &Value) -> Vec<MetricLine> {
    let mut lines = Vec::new();
    let status = v.get("userStatus").unwrap_or(v);
    let plan_status = status.get("planStatus").unwrap_or(status);
    let plan_info = plan_status.get("planInfo").cloned().unwrap_or(Value::Null);
    let hide_daily = plan_info.get("hideDailyQuota").and_then(|x| x.as_bool()) == Some(true);

    let daily_remaining = num_at(plan_status, "dailyQuotaRemainingPercent");
    let weekly_remaining = num_at(plan_status, "weeklyQuotaRemainingPercent");
    let daily_reset = if hide_daily {
        None
    } else {
        unix_seconds(plan_status.get("dailyQuotaResetAtUnix"))
    };
    let weekly_reset = unix_seconds(plan_status.get("weeklyQuotaResetAtUnix"));

    if !hide_daily {
        if let Some(remaining) = daily_remaining {
            lines.push(MetricLine::percent(
                "Daily quota",
                crate::json::clamp_percent(100.0 - remaining),
                daily_reset,
                24 * 60 * 60 * 1000,
            ));
        }
    }
    if let Some(remaining) = weekly_remaining {
        lines.push(MetricLine::percent(
            "Weekly quota",
            crate::json::clamp_percent(100.0 - remaining),
            weekly_reset,
            WEEK_MS,
        ));
    } else if hide_daily {
        if let Some(remaining) = daily_remaining {
            lines.push(MetricLine::percent(
                "Weekly quota",
                crate::json::clamp_percent(100.0 - remaining),
                weekly_reset,
                WEEK_MS,
            ));
        }
    }
    if let Some(micros) = num_at(plan_status, "overageBalanceMicros") {
        lines.push(MetricLine::values_dollars(
            "Extra usage balance",
            (micros / 1_000_000.0).max(0.0),
        ));
    }
    lines
}

fn unix_seconds(v: Option<&Value>) -> Option<chrono::DateTime<Utc>> {
    let n = v.and_then(crate::json::num)?;
    Utc.timestamp_opt(n as i64, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remaining_percent_flips_to_used() {
        let lines = map_status(&json!({
            "userStatus": {
                "planStatus": {
                    "planInfo": { "planName": "pro", "hideDailyQuota": false },
                    "dailyQuotaRemainingPercent": 40.0,
                    "weeklyQuotaRemainingPercent": 70.0,
                    "overageBalanceMicros": 2_500_000
                }
            }
        }));
        let daily = lines.iter().find(|l| l.label() == "Daily quota").unwrap();
        match daily {
            MetricLine::Progress { used, .. } => assert!((*used - 60.0).abs() < 0.001),
            other => panic!("{other:?}"),
        }
        match lines.iter().find(|l| l.label() == "Extra usage balance") {
            Some(MetricLine::Values { values, .. }) => {
                assert!((values[0].number - 2.5).abs() < 0.001)
            }
            other => panic!("{other:?}"),
        }
    }
}
