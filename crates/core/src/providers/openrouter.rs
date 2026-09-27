use async_trait::async_trait;
use serde_json::Value;

use super::{widget, Provider};
use crate::http::Http;
use crate::json::{bool_at, num_at};
use crate::models::*;
use crate::paths;
use crate::settings::AppSettings;

pub struct OpenRouterProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self {
            info: ProviderInfo {
                id: "openrouter".into(),
                display_name: "OpenRouter".into(),
                icon: "openrouter".into(),
                links: vec![
                    ProviderLink {
                        label: "Activity".into(),
                        url: "https://openrouter.ai/activity".into(),
                    },
                    ProviderLink {
                        label: "Credits".into(),
                        url: "https://openrouter.ai/settings/credits".into(),
                    },
                ],
            },
            widgets: vec![
                widget("openrouter.credits", "openrouter", "Credits", true),
                widget("openrouter.balance", "openrouter", "Balance", true),
                widget("openrouter.today", "openrouter", "Today", true),
                widget("openrouter.week", "openrouter", "This Week", true),
                widget("openrouter.month", "openrouter", "This Month", true),
                widget("openrouter.keyLimit", "openrouter", "Key Limit", true),
            ],
        }
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
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
                tracing::warn!(%error, "OpenRouter credential status is indeterminate");
                true
            }
        }
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let key = match load_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                return ProviderSnapshot::err(&self.info, "Add an OpenRouter API key in Settings.")
            }
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        let auth = [("Authorization", format!("Bearer {key}"))];
        let hdr: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let credits = http.get("https://openrouter.ai/api/v1/credits", &hdr).await;
        let key_meta = http.get("https://openrouter.ai/api/v1/key", &hdr).await;
        let mut lines = Vec::new();
        let mut plan = None;
        match credits {
            Ok(res) if res.ok() => {
                if let Some(data) = res.json().and_then(|v| v.get("data").cloned()) {
                    lines.extend(credits_lines(&data));
                }
            }
            Ok(res) if res.status == 401 || res.status == 403 => {
                return ProviderSnapshot::err(&self.info, "OpenRouter API key is invalid.");
            }
            Ok(res) => {
                return ProviderSnapshot::err(
                    &self.info,
                    &format!("OpenRouter request failed (HTTP {}).", res.status),
                );
            }
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach OpenRouter. Check your connection.",
                )
            }
        }
        if let Ok(res) = key_meta {
            if res.ok() {
                if let Some(data) = res.json().and_then(|v| v.get("data").cloned()) {
                    let (p, extra) = key_metrics(&data);
                    plan = p;
                    lines.extend(extra);
                }
            }
        }
        ProviderSnapshot::ok(&self.info, plan, lines)
    }
}

fn load_key() -> Result<Option<String>, String> {
    let credential_error = match paths::app_api_key_checked("openrouter") {
        Ok(Some(key)) => return Ok(Some(key)),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    let settings = AppSettings::load();
    if let Some(k) = settings
        .legacy_api_keys
        .get("openrouter")
        .cloned()
        .filter(|s| !s.is_empty())
    {
        return Ok(Some(k));
    }
    for path in [
        paths::app_config_dir().join("openrouter.json"),
        paths::home().join(".config/multimeters/openrouter.json"),
        paths::home().join(".config/openusage/openrouter.json"),
        paths::home().join(".config/openrouter/key.json"),
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
            } else if !text.trim().is_empty() && !text.trim().starts_with('{') {
                return Ok(Some(text.trim().to_string()));
            }
        }
    }
    let environment = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .or_else(|| std::env::var("OPENROUTER_KEY").ok())
        .filter(|s| !s.is_empty());
    if environment.is_some() {
        return Ok(environment);
    }
    match credential_error {
        Some(error) => Err(format!(
            "Couldn't read the saved OpenRouter key from Windows Credential Manager: {error}"
        )),
        None => Ok(None),
    }
}

fn credits_lines(data: &Value) -> Vec<MetricLine> {
    let Some(total_usage) = num_at(data, "total_usage") else {
        return vec![];
    };
    let used = total_usage.max(0.0);
    let total_credits = num_at(data, "total_credits").unwrap_or(0.0).max(0.0);
    let mut lines = Vec::new();
    if total_credits > 0.0 {
        lines.push(MetricLine::dollars_progress(
            "Credits",
            used,
            total_credits,
            None,
            0,
        ));
    }
    lines.push(MetricLine::Values {
        label: "Balance".into(),
        values: vec![MetricValue {
            number: (total_credits - used).max(0.0),
            kind: MetricKind::Dollars,
            label: None,
            estimated: false,
        }],
        color_hex: None,
        expiries_at: vec![],
        unknown_models: vec![],
    });
    lines
}

fn key_metrics(data: &Value) -> (Option<String>, Vec<MetricLine>) {
    let mut lines = Vec::new();
    if let Some(n) = num_at(data, "usage_daily") {
        lines.push(MetricLine::values_dollars("Today", n.max(0.0)));
    }
    if let Some(n) = num_at(data, "usage_weekly") {
        lines.push(MetricLine::values_dollars("This Week", n.max(0.0)));
    }
    if let Some(n) = num_at(data, "usage_monthly") {
        lines.push(MetricLine::values_dollars("This Month", n.max(0.0)));
    }
    if let Some(limit) = num_at(data, "limit").filter(|l| *l > 0.0) {
        lines.push(MetricLine::dollars_progress(
            "Key Limit",
            num_at(data, "usage").unwrap_or(0.0).max(0.0),
            limit,
            None,
            0,
        ));
    }
    let plan = bool_at(data, "is_free_tier").map(|free| {
        if free {
            "Free tier".into()
        } else {
            "Pay as you go".into()
        }
    });
    (plan, lines)
}
