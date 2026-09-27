use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Mutex;

use super::{spend_widgets, widget, widget_labeled, Provider};
use crate::http::Http;
use crate::json::{date_at, jwt_exp, num_at, obj, str_at};
use crate::models::*;
use crate::paths;
use crate::spend;

pub struct ClaudeProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
    rate_limited_until: Mutex<Option<chrono::DateTime<Utc>>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        let info = ProviderInfo {
            id: "claude".into(),
            display_name: "Claude".into(),
            icon: "claude".into(),
            links: vec![
                ProviderLink {
                    label: "Status".into(),
                    url: "https://status.anthropic.com/".into(),
                },
                ProviderLink {
                    label: "Dashboard".into(),
                    url: "https://claude.ai/settings/usage".into(),
                },
            ],
        };
        let mut widgets = vec![
            widget("claude.session", "claude", "Session", true),
            widget("claude.weekly", "claude", "Weekly", true),
            widget("claude.sonnet", "claude", "Sonnet", false),
            widget("claude.fable", "claude", "Fable", false),
            widget_labeled(
                "claude.extra",
                "claude",
                "Extra Usage",
                "Extra usage spent",
                true,
            ),
        ];
        widgets.extend(spend_widgets("claude"));
        Self {
            info,
            widgets,
            rate_limited_until: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_oauth().is_some() || std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok()
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let mut oauth = match load_oauth() {
            Some(v) => v,
            None => {
                if let Ok(_token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
                    let mut snap = ProviderSnapshot::ok(&self.info, None, spend::claude_lines());
                    snap.warning = Some(
                        "Re-login for live usage. Run `claude` and sign in again to restore session and weekly limits."
                            .into(),
                    );
                    return snap;
                }
                return ProviderSnapshot::err(
                    &self.info,
                    "Not logged in. Run `claude` to authenticate.",
                );
            }
        };

        if scopes_missing_profile(&oauth) {
            let mut snapshot =
                ProviderSnapshot::ok(&self.info, plan_name(&oauth), spend::claude_lines());
            snapshot.warning = Some(
                "Re-login for live usage. Run `claude` and sign in again to restore session and weekly limits."
                    .into(),
            );
            return snapshot;
        }

        if needs_refresh(&oauth) {
            if let Some(rt) = oauth.get("refresh_token").and_then(|v| v.as_str()) {
                if let Ok(fresh) = refresh_token(http, rt).await {
                    oauth = merge_oauth(oauth, fresh);
                    if let Err(error) = save_oauth(&oauth) {
                        tracing::warn!(%error, "could not persist refreshed Claude credentials");
                    }
                }
            }
        }

        let token = match oauth.get("access_token").and_then(|v| v.as_str()) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Not logged in. Run `claude` to authenticate.",
                )
            }
        };

        if let Some(until) = self
            .rate_limited_until
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .filter(|until| *until > Utc::now())
        {
            return rate_limited_snapshot(&self.info, &oauth, Some(until - Utc::now()));
        }

        let res = match http
            .send(
                Method::GET,
                "https://api.anthropic.com/api/oauth/usage",
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("Accept", "application/json"),
                    ("anthropic-beta", "oauth-2025-04-20"),
                    ("User-Agent", "claude-code/2.1.69"),
                ],
                None,
                10,
            )
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach Anthropic. Check your connection.",
                )
            }
        };

        if res.status == 429 {
            let retry = parse_retry_after(&res).unwrap_or_else(|| Duration::minutes(5));
            if let Ok(mut until) = self.rate_limited_until.lock() {
                *until = Some(Utc::now() + retry);
            }
            return rate_limited_snapshot(&self.info, &oauth, Some(retry));
        }
        if res.status == 401 || res.status == 403 {
            return ProviderSnapshot::err(
                &self.info,
                "Session expired. Run `claude` to log in again.",
            );
        }
        if !res.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Claude request failed (HTTP {}).", res.status),
            );
        }

        let body = match res.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Claude usage data unavailable. Try again later.",
                )
            }
        };

        if let Ok(mut until) = self.rate_limited_until.lock() {
            *until = None;
        }

        let mut lines = map_usage(&body);
        lines.extend(spend::claude_lines());
        ProviderSnapshot::ok(&self.info, plan_name(&oauth), lines)
    }
}

fn scopes_missing_profile(oauth: &Value) -> bool {
    let Some(scopes) = oauth.get("scopes") else {
        return false;
    };
    match scopes {
        Value::Array(values) => !values
            .iter()
            .any(|scope| scope.as_str() == Some("user:profile")),
        Value::String(scopes) => !scopes
            .split_whitespace()
            .any(|scope| scope == "user:profile"),
        _ => false,
    }
}

fn parse_retry_after(response: &crate::http::HttpResponse) -> Option<Duration> {
    let raw = response.header("retry-after")?.trim();
    if let Ok(seconds) = raw.parse::<i64>() {
        return Some(Duration::seconds(seconds.max(0)));
    }
    chrono::DateTime::parse_from_rfc2822(raw)
        .ok()
        .map(|date| (date.with_timezone(&Utc) - Utc::now()).max(Duration::zero()))
}

fn rate_limited_snapshot(
    info: &ProviderInfo,
    oauth: &Value,
    retry: Option<Duration>,
) -> ProviderSnapshot {
    let wait = retry
        .map(|duration| ((duration.num_seconds().max(0) + 59) / 60).max(1))
        .map(|minutes| format!(" Retrying in about {minutes} minute(s)."))
        .unwrap_or_default();
    let message = format!(
        "Updates blocked by Anthropic. Be patient — manual refreshes will make it worse.{wait}"
    );
    let mut snapshot = ProviderSnapshot::err(info, &message);
    snapshot.plan = plan_name(oauth);
    snapshot.lines.extend(spend::claude_lines());
    snapshot.warning = Some(message);
    snapshot
}

fn load_oauth() -> Option<Value> {
    let file = paths::claude_home().join(".credentials.json");
    if let Some(text) = paths::read_text(&file) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(oauth) = v.get("claudeAiOauth").cloned() {
                if oauth
                    .get("accessToken")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .is_some()
                    || oauth
                        .get("access_token")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .is_some()
                {
                    return Some(normalize_oauth(oauth));
                }
            }
            if v.get("accessToken").is_some() || v.get("access_token").is_some() {
                return Some(normalize_oauth(v));
            }
        }
    }
    if let Some(raw) = paths::cred_read("Claude Code-credentials", None)
        .or_else(|| paths::cred_read("Claude Code-credentials", Some(&whoami())))
    {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            return Some(normalize_oauth(
                v.get("claudeAiOauth").cloned().unwrap_or(v),
            ));
        }
    }
    None
}

fn normalize_oauth(v: Value) -> Value {
    let mut out = json!({});
    if let Some(obj) = v.as_object() {
        let access = obj
            .get("access_token")
            .or_else(|| obj.get("accessToken"))
            .cloned();
        let refresh = obj
            .get("refresh_token")
            .or_else(|| obj.get("refreshToken"))
            .cloned();
        let expires = obj
            .get("expires_at")
            .or_else(|| obj.get("expiresAt"))
            .cloned();
        if let Some(a) = access {
            out["access_token"] = a;
        }
        if let Some(r) = refresh {
            out["refresh_token"] = r;
        }
        if let Some(e) = expires {
            out["expires_at"] = e;
        }
        if let Some(s) = obj
            .get("subscriptionType")
            .or_else(|| obj.get("subscription_type"))
        {
            out["subscription_type"] = s.clone();
        }
        if let Some(t) = obj
            .get("rateLimitTier")
            .or_else(|| obj.get("rate_limit_tier"))
        {
            out["rate_limit_tier"] = t.clone();
        }
        if let Some(s) = obj.get("scopes") {
            out["scopes"] = s.clone();
        }
    }
    out
}

fn save_oauth(oauth: &Value) -> anyhow::Result<()> {
    let file = paths::claude_home().join(".credentials.json");
    let existing = paths::read_text(&file)
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    let mut root = existing;
    let mapped = json!({
        "accessToken": oauth.get("access_token"),
        "refreshToken": oauth.get("refresh_token"),
        "expiresAt": oauth.get("expires_at"),
        "subscriptionType": oauth.get("subscription_type"),
        "rateLimitTier": oauth.get("rate_limit_tier"),
        "scopes": oauth.get("scopes"),
    });
    root["claudeAiOauth"] = mapped;
    paths::write_secret_text(&file, &serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

fn needs_refresh(oauth: &Value) -> bool {
    if let Some(exp) = num_at(oauth, "expires_at") {
        let ms = if exp > 1_000_000_000_000.0 {
            exp
        } else {
            exp * 1000.0
        };
        return ms <= (Utc::now() + Duration::minutes(5)).timestamp_millis() as f64;
    }
    if let Some(token) = str_at(oauth, "access_token") {
        if let Some(exp) = jwt_exp(token) {
            return exp - Utc::now() <= Duration::minutes(5);
        }
    }
    true
}

async fn refresh_token(http: &Http, refresh: &str) -> anyhow::Result<Value> {
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        "scope": "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
    });
    let res = http
        .post_json("https://platform.claude.com/v1/oauth/token", &[], &body)
        .await?;
    if !res.ok() {
        anyhow::bail!("refresh failed {}", res.status);
    }
    res.json()
        .ok_or_else(|| anyhow::anyhow!("bad refresh body"))
}

fn merge_oauth(mut old: Value, fresh: Value) -> Value {
    if let Some(t) = str_at(&fresh, "access_token") {
        old["access_token"] = json!(t);
    }
    if let Some(t) = str_at(&fresh, "refresh_token") {
        old["refresh_token"] = json!(t);
    }
    if let Some(s) = num_at(&fresh, "expires_in") {
        old["expires_at"] = json!((Utc::now() + Duration::seconds(s as i64)).timestamp_millis());
    }
    old
}

fn plan_name(oauth: &Value) -> Option<String> {
    let raw = str_at(oauth, "subscription_type")?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut name = crate::json::title_case(raw);
    if let Some(tier) = str_at(oauth, "rate_limit_tier") {
        if let Some(m) = tier
            .split(|c: char| !c.is_ascii_alphanumeric())
            .find(|s| s.ends_with('x'))
        {
            name = format!("{name} {m}");
        }
    }
    Some(name)
}

fn map_usage(body: &Value) -> Vec<MetricLine> {
    let mut lines = Vec::new();
    append_window(body.get("five_hour"), "Session", SESSION_MS, &mut lines);
    append_window(body.get("seven_day"), "Weekly", WEEK_MS, &mut lines);
    append_window(body.get("seven_day_sonnet"), "Sonnet", WEEK_MS, &mut lines);
    if let Some(limits) = body.get("limits").and_then(|v| v.as_array()) {
        for entry in limits {
            if str_at(entry, "kind") != Some("weekly_scoped") {
                continue;
            }
            let name = entry
                .pointer("/scope/model/display_name")
                .and_then(|v| v.as_str());
            if name == Some("Fable") {
                if let Some(used) = num_at(entry, "percent") {
                    lines.push(MetricLine::percent(
                        "Fable",
                        used,
                        date_at(entry, "resets_at"),
                        WEEK_MS,
                    ));
                }
            }
        }
    }
    if let Some(extra) = obj(body.get("extra_usage").unwrap_or(&Value::Null)) {
        if extra.get("is_enabled").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(used_cents) = extra.get("used_credits").and_then(crate::json::num) {
                let used = crate::json::cents_to_dollars(used_cents);
                let limit_cents = extra
                    .get("monthly_limit")
                    .and_then(crate::json::num)
                    .unwrap_or(0.0);
                if limit_cents > 0.0 {
                    lines.push(MetricLine::dollars_progress(
                        "Extra usage spent",
                        used,
                        crate::json::cents_to_dollars(limit_cents),
                        None,
                        MONTH_MS,
                    ));
                } else if used > 0.0 {
                    lines.push(MetricLine::values_dollars("Extra usage spent", used));
                }
            }
        }
    }
    lines
}

fn append_window(value: Option<&Value>, label: &str, period: i64, lines: &mut Vec<MetricLine>) {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return;
    };
    let Some(used) = obj.get("utilization").and_then(crate::json::num) else {
        return;
    };
    lines.push(MetricLine::percent(
        label,
        used,
        obj.get("resets_at").and_then(crate::json::as_date),
        period,
    ));
}

fn whoami() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extra_usage_uses_credits_cents() {
        let lines = map_usage(&json!({
            "extra_usage": { "is_enabled": true, "used_credits": 250, "monthly_limit": 1000 }
        }));
        let extra = lines
            .iter()
            .find(|line| line.label() == "Extra usage spent")
            .expect("extra");
        match extra {
            MetricLine::Progress { used, limit, .. } => {
                assert!((*used - 2.5).abs() < 0.001);
                assert!((*limit - 10.0).abs() < 0.001);
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn detects_inference_only_scopes() {
        assert!(scopes_missing_profile(
            &json!({"scopes":["user:inference"]})
        ));
        assert!(!scopes_missing_profile(
            &json!({"scopes":"user:profile user:inference"})
        ));
        assert!(!scopes_missing_profile(&json!({})));
    }

    #[test]
    fn parses_numeric_retry_after() {
        let response = crate::http::HttpResponse {
            status: 429,
            body: vec![],
            headers: std::collections::HashMap::from([("retry-after".into(), "120".into())]),
        };
        assert_eq!(parse_retry_after(&response), Some(Duration::seconds(120)));
    }
}
