use async_trait::async_trait;
use serde_json::Value;

use super::{widget, Provider};
use crate::http::Http;
use crate::json::{as_date, num_at, str_at};
use crate::models::*;
use crate::paths;
use crate::settings::AppSettings;

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

pub struct ZaiProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl ZaiProvider {
    pub fn new() -> Self {
        Self {
            info: ProviderInfo {
                id: "zai".into(),
                display_name: "Z.ai".into(),
                icon: "zai".into(),
                links: vec![
                    ProviderLink {
                        label: "Dashboard".into(),
                        url: "https://z.ai/manage-apikey/coding-plan/personal/my-plan".into(),
                    },
                    ProviderLink {
                        label: "API Keys".into(),
                        url: "https://z.ai/manage-apikey/apikey-list".into(),
                    },
                ],
            },
            widgets: vec![
                widget("zai.session", "zai", "Session", true, false, true),
                widget("zai.weekly", "zai", "Weekly", true, false, true),
                widget("zai.webSearches", "zai", "Web Searches", true, true, false),
            ],
        }
    }
}

#[async_trait]
impl Provider for ZaiProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        match load_key() {
            Ok(key) => key.is_some(),
            Err(error) => {
                tracing::warn!(%error, "Z.ai credential status is indeterminate");
                true
            }
        }
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let key = match load_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                return ProviderSnapshot::err(&self.info, "Add a Z.ai API key in Settings.")
            }
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        let auth = [("Authorization", format!("Bearer {key}"))];
        let hdr: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let quota = match http
            .get("https://api.z.ai/api/monitor/usage/quota/limit", &hdr)
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach Z.ai. Check your connection.",
                )
            }
        };
        if quota.status == 401 || quota.status == 403 {
            return ProviderSnapshot::err(&self.info, "Z.ai API key is invalid.");
        }
        if !quota.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Z.ai request failed (HTTP {}).", quota.status),
            );
        }
        let body = match quota.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Z.ai usage data unavailable. Try again later.",
                )
            }
        };
        if body.get("success").and_then(|v| v.as_bool()) == Some(false)
            && str_at(&body, "msg")
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains("coding plan")
        {
            return ProviderSnapshot::err(
                &self.info,
                "No active GLM Coding Plan. Subscribe at z.ai/subscribe to see usage.",
            );
        }
        let sub = http
            .get("https://api.z.ai/api/biz/subscription/list", &hdr)
            .await
            .ok();
        let plan = sub.and_then(|r| r.json()).and_then(plan_name);
        match map_quota(&body) {
            Ok(lines) => ProviderSnapshot::ok(&self.info, plan, lines),
            Err(msg) => ProviderSnapshot::err(&self.info, &msg),
        }
    }
}

fn load_key() -> Result<Option<String>, String> {
    let credential_error = match paths::app_api_key_checked("zai") {
        Ok(Some(key)) => return Ok(Some(key)),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let settings = AppSettings::load();
    if let Some(k) = settings
        .legacy_api_keys
        .get("zai")
        .cloned()
        .filter(|s| !s.is_empty())
    {
        return Ok(Some(k));
    }
    for path in [
        paths::app_config_dir().join("zai.json"),
        paths::home().join(".config/zai/key.json"),
        paths::home().join(".config/openusage/zai.json"),
    ] {
        if let Some(text) = paths::read_text(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(k) = v
                    .get("apiKey")
                    .or_else(|| v.get("api_key"))
                    .and_then(|x| x.as_str())
                {
                    if !k.is_empty() {
                        return Ok(Some(k.to_string()));
                    }
                }
            }
        }
    }
    let environment = std::env::var("ZAI_API_KEY")
        .ok()
        .or_else(|| std::env::var("GLM_API_KEY").ok())
        .filter(|s| !s.is_empty());
    if environment.is_some() {
        return Ok(environment);
    }
    match credential_error {
        Some(error) => Err(format!(
            "Couldn't read the saved Z.ai key from Windows Credential Manager: {error}"
        )),
        None => Ok(None),
    }
}

fn plan_name(v: Value) -> Option<String> {
    let list = v.get("data").or(Some(&v))?;
    let arr = list
        .as_array()
        .or_else(|| list.get("list").and_then(|x| x.as_array()))?;
    arr.iter().find_map(|item| {
        str_at(item, "productName")
            .or_else(|| str_at(item, "name"))
            .or_else(|| str_at(item, "planName"))
            .filter(|name| !name.trim().is_empty())
            .map(|s| s.to_string())
    })
}

fn map_quota(root: &Value) -> Result<Vec<MetricLine>, String> {
    let container = root.get("data").unwrap_or(root);
    let limits = container
        .get("limits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Z.ai usage data unavailable. Try again later.".to_string())?;
    if limits.is_empty() {
        return Ok(vec![MetricLine::no_data()]);
    }
    let mut lines = Vec::new();
    let mut saw_recognized = false;
    for entry in limits {
        let ty = str_at(entry, "type")
            .or_else(|| str_at(entry, "name"))
            .unwrap_or("");
        if ty == "TOKENS_LIMIT" {
            if let Some(line) = token_line(entry)? {
                saw_recognized = true;
                lines.push(line);
            }
        } else if ty == "TIME_LIMIT" && !saw_web_search(&lines) {
            saw_recognized = true;
            lines.push(web_search_line(entry)?);
        }
    }
    if lines.is_empty() {
        if saw_recognized {
            Err("Z.ai returned an invalid quota response.".into())
        } else {
            Ok(vec![MetricLine::no_data()])
        }
    } else {
        Ok(lines)
    }
}

fn token_line(entry: &Value) -> Result<Option<MetricLine>, String> {
    let unit = num_at(entry, "unit")
        .filter(|value| value.is_finite())
        .ok_or_else(|| "Z.ai returned an invalid quota response.".to_string())?;
    let number = num_at(entry, "number")
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "Z.ai returned an invalid quota response.".to_string())?;
    let unit_ms = match unit {
        3.0 => 60.0 * 60.0 * 1_000.0,
        4.0 => 24.0 * 60.0 * 60.0 * 1_000.0,
        5.0 => 30.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
        6.0 => 7.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
        _ => return Ok(None),
    };
    let period = unit_ms * number;
    if !period.is_finite() || period < 1.0 || period > i64::MAX as f64 {
        return Err("Z.ai returned an invalid quota response.".into());
    }
    let used = num_at(entry, "percentage")
        .filter(|value| value.is_finite())
        .ok_or_else(|| "Z.ai returned an invalid quota response.".to_string())?;
    let label = if period < DAY_MS as f64 {
        "Session"
    } else {
        "Weekly"
    };
    Ok(Some(MetricLine::percent(
        label,
        used,
        entry.get("nextResetTime").and_then(as_date),
        period as i64,
    )))
}

fn saw_web_search(lines: &[MetricLine]) -> bool {
    lines.iter().any(|line| line.label() == "Web Searches")
}

fn web_search_line(entry: &Value) -> Result<MetricLine, String> {
    let used = num_at(entry, "currentValue")
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| "Z.ai returned an invalid quota response.".to_string())?;
    let limit = num_at(entry, "usage")
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| "Z.ai returned an invalid quota response.".to_string())?;
    Ok(MetricLine::Progress {
        label: "Web Searches".into(),
        used,
        limit,
        format: ProgressFormat::Count {
            suffix: "searches".into(),
        },
        resets_at: entry.get("nextResetTime").and_then(as_date),
        period_duration_ms: Some(MONTH_MS),
        color_hex: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use serde_json::json;

    #[test]
    fn maps_live_quota_shape_and_plan_name() {
        let body = json!({"data": {"limits": [
            {"type":"TOKENS_LIMIT", "unit":3, "number":5, "percentage":15, "nextResetTime":1770648402389_i64},
            {"type":"TOKENS_LIMIT", "unit":6, "number":1, "percentage":40},
            {"type":"TIME_LIMIT", "unit":5, "number":1, "usage":4000, "currentValue":1828}
        ]}});
        let lines = map_quota(&body).expect("valid limits");
        assert_eq!(lines.len(), 3);
        assert!(matches!(
            &lines[0],
            MetricLine::Progress { label, used, period_duration_ms: Some(period), resets_at: Some(reset), .. }
                if label == "Session" && *used == 15.0 && *period == 5 * 60 * 60 * 1_000 && reset.year() == 2026
        ));
        assert!(matches!(
            &lines[1],
            MetricLine::Progress { label, period_duration_ms: Some(period), .. }
                if label == "Weekly" && *period == WEEK_MS
        ));
        assert!(matches!(
            &lines[2],
            MetricLine::Progress { label, used, limit, .. }
                if label == "Web Searches" && *used == 1828.0 && *limit == 4000.0
        ));
        assert_eq!(
            plan_name(json!({"data":[{"productName":"GLM Coding Max"}]})).as_deref(),
            Some("GLM Coding Max")
        );
    }

    #[test]
    fn token_windows_follow_the_payload() {
        let lines = map_quota(&json!({"limits": [
            {"type":"TOKENS_LIMIT", "unit":3, "number":3, "percentage":10},
            {"type":"TOKENS_LIMIT", "unit":4, "number":3, "percentage":20}
        ]}))
        .expect("valid limits");
        assert!(
            matches!(&lines[0], MetricLine::Progress { period_duration_ms: Some(period), .. } if *period == 3 * 60 * 60 * 1_000)
        );
        assert!(
            matches!(&lines[1], MetricLine::Progress { period_duration_ms: Some(period), .. } if *period == 3 * DAY_MS)
        );
    }

    #[test]
    fn rejects_boolean_percentage_and_clamps_real_percentage() {
        assert!(map_quota(&json!({"limits":[
            {"type":"TOKENS_LIMIT", "unit":3, "number":5, "percentage":true}
        ]}))
        .is_err());
        let lines = map_quota(&json!({"limits":[
            {"type":"TOKENS_LIMIT", "unit":3, "number":5, "percentage":150}
        ]}))
        .expect("valid limits");
        assert!(matches!(&lines[0], MetricLine::Progress { used, .. } if *used == 100.0));
    }

    #[test]
    fn unknown_limits_are_a_valid_no_data_state() {
        let lines = map_quota(&json!({"limits":[{"type":"FUTURE_LIMIT"}]}))
            .expect("unknown limits should be ignored");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].label(), "Status");
    }
}
