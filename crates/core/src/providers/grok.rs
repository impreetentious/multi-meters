use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::{json, Value};

use super::{spend_widgets, widget, widget_labeled, Provider};
use crate::http::Http;
use crate::json::{as_date, jwt_exp, num_at, str_at};
use crate::models::*;
use crate::paths;
use crate::spend;

pub struct GrokProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl GrokProvider {
    pub fn new() -> Self {
        let info = ProviderInfo {
            id: "grok".into(),
            display_name: "Grok".into(),
            icon: "grok".into(),
            links: vec![ProviderLink {
                label: "Usage".into(),
                url: "https://grok.com/?_s=usage".into(),
            }],
        };
        let mut widgets = vec![
            widget_labeled(
                "grok.weekly",
                "grok",
                "Weekly",
                "Weekly limit",
                true,
                false,
                false,
            ),
            widget(
                "grok.payAsYouGo",
                "grok",
                "Pay as you go",
                true,
                true,
                false,
            ),
        ];
        widgets.extend(spend_widgets("grok"));
        Self { info, widgets }
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_entries()
            .map(|(_, entries)| !entries.is_empty())
            .unwrap_or(false)
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        // One working copy for the whole sweep: a later entry's write must carry forward the
        // rotated credentials an earlier entry already persisted, not revert them.
        let (mut working_file, entries) = match load_entries() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(&self.info, "Grok not logged in. Run `grok login`.")
            }
        };
        let mut saw_expired = false;

        for (key, mut entry) in entries {
            let mut token = str_at(&entry, "key").unwrap_or("").trim().to_string();
            if token.is_empty() {
                continue;
            }
            if needs_refresh(&entry, &token) {
                if let Some(fresh) = refresh_entry(http, &mut working_file, &key, &mut entry).await
                {
                    token = fresh;
                } else if is_expired(&entry, &token) {
                    saw_expired = true;
                    continue;
                }
            }

            let mut credits = match fetch_credits(http, &token).await {
                Ok(response) => response,
                Err(_) => {
                    return ProviderSnapshot::err(
                        &self.info,
                        "Grok billing request failed. Check your connection.",
                    )
                }
            };

            // Match the CLI's auth retry: even a token that did not appear near expiry can be
            // revoked server-side, so refresh once on an auth response before asking the user to
            // sign in again.
            if is_auth_error(credits.status) {
                if let Some(fresh) = refresh_entry(http, &mut working_file, &key, &mut entry).await
                {
                    token = fresh;
                    credits = match fetch_credits(http, &token).await {
                        Ok(response) => response,
                        Err(_) => {
                            return ProviderSnapshot::err(
                                &self.info,
                                "Grok billing request failed. Check your connection.",
                            )
                        }
                    };
                }
            }
            if is_auth_error(credits.status) {
                saw_expired = true;
                continue;
            }
            if !credits.ok() {
                return ProviderSnapshot::err(
                    &self.info,
                    &format!("Grok billing request failed (HTTP {}).", credits.status),
                );
            }
            let body = match credits.json() {
                Some(value) => value,
                None => return ProviderSnapshot::err(&self.info, "Grok billing response changed."),
            };
            let settings = fetch_settings(http, &token).await.ok();
            let plan = settings
                .and_then(|response| response.json())
                .and_then(|value| str_at(&value, "subscription_tier_display").map(str::to_string));

            let mut lines = map_credits(&body);
            lines.extend(spend::grok_lines());
            return ProviderSnapshot::ok(&self.info, plan, lines);
        }

        ProviderSnapshot::err(
            &self.info,
            if saw_expired {
                "Grok auth expired. Run `grok login` again."
            } else {
                "Grok auth invalid. Run `grok login` again."
            },
        )
    }
}

fn load_entries() -> Option<(Value, Vec<(String, Value)>)> {
    let text = paths::read_text(&paths::grok_home().join("auth.json"))?;
    let file: Value = serde_json::from_str(&text).ok()?;
    let obj = file.as_object()?;
    let entries = obj
        .iter()
        .filter(|(_, value)| {
            str_at(value, "key")
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .is_some()
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    Some((file, entries))
}

fn needs_refresh(entry: &Value, token: &str) -> bool {
    entry_expiry(entry)
        .map(|expiry| expiry - Utc::now() <= Duration::minutes(5))
        .unwrap_or(false)
        || jwt_exp(token)
            .map(|expiry| expiry - Utc::now() <= Duration::minutes(5))
            .unwrap_or(false)
}

fn is_expired(entry: &Value, token: &str) -> bool {
    jwt_exp(token)
        .or_else(|| entry_expiry(entry))
        .map(|expiry| Utc::now() >= expiry)
        .unwrap_or(false)
}

fn entry_expiry(entry: &Value) -> Option<chrono::DateTime<Utc>> {
    str_at(entry, "expires_at")
        .or_else(|| str_at(entry, "expires"))
        .and_then(crate::json::parse_iso)
}

fn client_id<'a>(entry_key: &'a str, entry: &'a Value) -> &'a str {
    str_at(entry, "oidc_client_id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            entry_key
                .rsplit("::")
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("b1a00492-073a-47ea-816f-4c329264a828")
}

async fn refresh_entry(
    http: &Http,
    file: &mut Value,
    key: &str,
    entry: &mut Value,
) -> Option<String> {
    let refresh = str_at(entry, "refresh_token")
        .or_else(|| str_at(entry, "refresh"))?
        .trim();
    if refresh.is_empty() {
        return None;
    }
    let fresh = match refresh_token(http, refresh, client_id(key, entry)).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Grok token refresh failed");
            return None;
        }
    };
    let token = str_at(&fresh, "access_token")?.trim().to_string();
    if token.is_empty() {
        tracing::warn!("Grok token refresh returned an empty access token");
        return None;
    }

    entry["key"] = json!(token);
    if let Some(value) = str_at(&fresh, "refresh_token")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        entry["refresh_token"] = json!(value);
    }
    if let Some(value) = str_at(&fresh, "id_token")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        entry["id_token"] = json!(value);
    }
    let expiry = num_at(&fresh, "expires_in")
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| Utc::now() + Duration::seconds(seconds as i64))
        .or_else(|| jwt_exp(&token))
        .unwrap_or_else(|| Utc::now() + Duration::hours(1));
    entry["expires_at"] = json!(expiry.to_rfc3339());
    file[key] = entry.clone();

    match serde_json::to_string_pretty(file) {
        Ok(text) => {
            if let Err(error) =
                paths::write_secret_text(&paths::grok_home().join("auth.json"), &text)
            {
                tracing::warn!(%error, "could not persist refreshed Grok credentials");
            }
        }
        Err(error) => tracing::warn!(%error, "could not encode refreshed Grok credentials"),
    }
    Some(token)
}

async fn fetch_credits(http: &Http, token: &str) -> anyhow::Result<crate::http::HttpResponse> {
    let authorization = format!("Bearer {token}");
    http.send(
        Method::GET,
        "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
        &[
            ("Authorization", authorization.as_str()),
            ("X-XAI-Token-Auth", "xai-grok-cli"),
            ("Accept", "application/json"),
        ],
        None,
        10,
    )
    .await
}

async fn fetch_settings(http: &Http, token: &str) -> anyhow::Result<crate::http::HttpResponse> {
    let authorization = format!("Bearer {token}");
    http.send(
        Method::GET,
        "https://cli-chat-proxy.grok.com/v1/settings",
        &[
            ("Authorization", authorization.as_str()),
            ("X-XAI-Token-Auth", "xai-grok-cli"),
            ("Accept", "application/json"),
        ],
        None,
        10,
    )
    .await
}

fn is_auth_error(status: u16) -> bool {
    status == 401 || status == 403
}

async fn refresh_token(http: &Http, refresh: &str, client_id: &str) -> anyhow::Result<Value> {
    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        urlencoding::encode(client_id),
        urlencoding::encode(refresh)
    );
    let res = http
        .send(
            Method::POST,
            "https://auth.x.ai/oauth2/token",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            Some(body.into_bytes()),
            15,
        )
        .await?;
    if !res.ok() {
        anyhow::bail!("grok refresh {}", res.status);
    }
    res.json().ok_or_else(|| anyhow::anyhow!("bad body"))
}

fn map_credits(body: &Value) -> Vec<MetricLine> {
    let mut lines = Vec::new();
    let config = body.get("config").unwrap_or(body);
    let period = config
        .pointer("/currentPeriod/type")
        .and_then(|v| v.as_str())
        .or_else(|| str_at(config, "period_type"))
        .or_else(|| str_at(config, "periodType"))
        .unwrap_or("");
    let used = num_at(config, "creditUsagePercent")
        .or_else(|| num_at(config, "used_percent"))
        .or_else(|| num_at(config, "usedPercent"))
        .unwrap_or(0.0);
    if period.eq_ignore_ascii_case("weekly")
        || period.contains("WEEK")
        || period == "USAGE_PERIOD_TYPE_WEEKLY"
    {
        let period_obj = config.get("currentPeriod").cloned().unwrap_or(Value::Null);
        let resets = as_date(period_obj.get("end").unwrap_or(&Value::Null))
            .or_else(|| as_date(config.get("period_end").unwrap_or(&Value::Null)))
            .or_else(|| as_date(config.get("periodEnd").unwrap_or(&Value::Null)));
        let start = as_date(period_obj.get("start").unwrap_or(&Value::Null));
        let ms = match (start, resets) {
            (Some(s), Some(e)) if e > s => (e - s).num_milliseconds().max(1),
            _ => WEEK_MS,
        };
        lines.push(MetricLine::percent("Weekly limit", used, resets, ms));
    }
    let cap = config
        .pointer("/onDemandCap/val")
        .and_then(crate::json::num)
        .or_else(|| num_at(config, "on_demand_cap"))
        .or_else(|| {
            config.get("onDemandCap").and_then(|v| match v {
                Value::Number(_) | Value::String(_) => crate::json::num(v),
                _ => None,
            })
        })
        .unwrap_or(0.0);
    let pay = if cap > 0.0 {
        format!("{:.0} cap", cap)
    } else {
        "Disabled".into()
    };
    lines.push(MetricLine::badge(
        "Pay as you go",
        &pay,
        Some(if cap > 0.0 { "#22c55e" } else { "#a3a3a3" }),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_nested_config_weekly_and_cap() {
        let lines = map_credits(&json!({
            "config": {
                "creditUsagePercent": 99.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-03T04:01:09.238389+00:00",
                    "end": "2026-07-10T04:01:09.238389+00:00"
                },
                "onDemandCap": { "val": 2500 }
            }
        }));
        assert!(lines.iter().any(|l| l.label() == "Weekly limit"));
        match lines.iter().find(|l| l.label() == "Pay as you go") {
            Some(MetricLine::Badge { text, .. }) => assert_eq!(text, "2500 cap"),
            other => panic!("expected badge, got {other:?}"),
        }
    }

    #[test]
    fn derives_refresh_client_id_from_auth_entry_key() {
        let entry = json!({ "key": "token" });
        assert_eq!(
            client_id("user@example.com::desktop-client", &entry),
            "desktop-client"
        );
        assert_eq!(
            client_id("user@example.com::", &entry),
            "b1a00492-073a-47ea-816f-4c329264a828"
        );
    }

    #[test]
    fn explicit_refresh_client_id_wins() {
        let entry = json!({
            "key": "token",
            "oidc_client_id": " explicit-client "
        });
        assert_eq!(
            client_id("user::fallback-client", &entry),
            "explicit-client"
        );
    }
}
