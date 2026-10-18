use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::Value;

use super::{spend_widgets, widget, Provider};
use crate::http::Http;
use crate::json::{as_date, jwt_exp, num, num_at, str_at};
use crate::models::*;
use crate::paths;
use crate::spend;

pub struct CodexProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl CodexProvider {
    pub fn new() -> Self {
        let info = ProviderInfo {
            id: "codex".into(),
            display_name: "Codex".into(),
            icon: "codex".into(),
            links: vec![
                ProviderLink {
                    label: "Status".into(),
                    url: "https://status.openai.com/".into(),
                },
                ProviderLink {
                    label: "Dashboard".into(),
                    url: "https://chatgpt.com/codex/settings/usage".into(),
                },
            ],
        };
        let mut widgets = vec![
            widget("codex.session", "codex", "Session", true),
            widget("codex.weekly", "codex", "Weekly", true),
            widget("codex.spark", "codex", "Spark", true),
            widget("codex.sparkWeekly", "codex", "Spark Weekly", true),
            widget("codex.rateLimitResets", "codex", "Rate Limit Resets", true),
            widget("codex.credits", "codex", "Credits", true),
        ];
        widgets.extend(spend_widgets("codex"));
        Self { info, widgets }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_auth().is_some()
    }

    fn credential_identity(&self) -> Option<Vec<u8>> {
        use sha2::{Digest, Sha256};
        let auth = load_auth()?;
        Some(Sha256::digest(serde_json::to_vec(&auth).ok()?).to_vec())
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        self.refresh_from(http, &load_auth, true).await
    }
}

impl CodexProvider {
    async fn refresh_from(
        &self,
        http: &Http,
        read_auth: &(dyn Fn() -> Option<Value> + Send + Sync),
        include_spend: bool,
    ) -> ProviderSnapshot {
        let auth = match read_auth() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Not logged in. Run `codex` to authenticate.",
                )
                .failed(FailureCategory::Authentication)
            }
        };
        if auth
            .pointer("/tokens/access_token")
            .and_then(|v| v.as_str())
            .is_none()
            && auth.get("OPENAI_API_KEY").is_some()
        {
            return ProviderSnapshot::err(&self.info, "Usage not available for API key.")
                .failed(FailureCategory::Configuration);
        }

        let token = match auth
            .pointer("/tokens/access_token")
            .and_then(|v| v.as_str())
        {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Not logged in. Run `codex` to authenticate.",
                )
                .failed(FailureCategory::Authentication)
            }
        };
        if jwt_exp(&token).is_some_and(|expires| expires <= Utc::now()) {
            return ProviderSnapshot::err(
                &self.info,
                "Session expired. Run `codex` to log in again.",
            )
            .failed(FailureCategory::Authentication);
        }
        let account = auth
            .pointer("/tokens/account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut headers = vec![
            ("Authorization", format!("Bearer {token}")),
            ("Accept", "application/json".into()),
            ("User-Agent", "MultiMeters".into()),
        ];
        if !account.is_empty() {
            headers.push(("ChatGPT-Account-Id", account.clone()));
        }
        let hdr_refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let res = match http
            .send(
                Method::GET,
                "https://chatgpt.com/backend-api/wham/usage",
                &hdr_refs,
                None,
                10,
            )
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach Codex. Check your connection.",
                )
            }
        };
        if read_auth().as_ref() != Some(&auth) {
            return ProviderSnapshot::err(
                &self.info,
                "Codex credentials changed. Refresh to use the current account.",
            )
            .failed(FailureCategory::Superseded);
        }
        if res.status == 401 || res.status == 403 {
            return ProviderSnapshot::err(
                &self.info,
                "Session expired. Run `codex` to log in again.",
            )
            .with_http_failure(&res);
        }
        if !res.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Codex request failed (HTTP {}).", res.status),
            )
            .with_http_failure(&res);
        }
        let header_percents = (
            res.header("x-codex-primary-used-percent")
                .and_then(|value| value.parse().ok()),
            res.header("x-codex-secondary-used-percent")
                .and_then(|value| value.parse().ok()),
        );
        let header_credits = res
            .header("x-codex-credits-balance")
            .and_then(|value| value.parse().ok());
        let body = match res.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Codex usage data unavailable. Try again later.",
                )
            }
        };

        let reset_res = http
            .send(
                Method::GET,
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits",
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("Accept", "application/json"),
                    ("User-Agent", "MultiMeters"),
                ],
                None,
                10,
            )
            .await
            .ok();

        if read_auth().as_ref() != Some(&auth) {
            return ProviderSnapshot::err(
                &self.info,
                "Codex credentials changed. Refresh to use the current account.",
            )
            .failed(FailureCategory::Superseded);
        }
        let reset_body = reset_res
            .as_ref()
            .filter(|response| response.ok())
            .and_then(|response| response.json());
        let mut lines = map_usage(&body, reset_body, header_percents, header_credits);
        if include_spend {
            lines.extend(spend::codex_lines());
        }
        ProviderSnapshot::ok(&self.info, plan_name(&body), lines)
    }
}

fn load_auth() -> Option<Value> {
    let path = paths::codex_home().join("auth.json");
    let text = paths::read_text(&path)?;
    serde_json::from_str(&text).ok()
}

fn plan_name(body: &Value) -> Option<String> {
    let raw = str_at(body, "plan_type")?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(match raw.to_ascii_lowercase().as_str() {
        "prolite" => "Pro 5x".into(),
        "pro" => "Pro 20x".into(),
        _ => crate::json::title_case(raw),
    })
}

fn map_usage(
    body: &Value,
    reset_body: Option<Value>,
    header_percents: (Option<f64>, Option<f64>),
    header_credits: Option<f64>,
) -> Vec<MetricLine> {
    let mut lines = Vec::new();
    let rate = body.get("rate_limit");
    lines.extend(window_lines(rate, "Session", "Weekly", header_percents));
    if let Some(arr) = body
        .get("additional_rate_limits")
        .and_then(|v| v.as_array())
    {
        if let Some(spark) = arr.iter().find(is_spark) {
            lines.extend(window_lines(
                spark.get("rate_limit"),
                "Spark",
                "Spark Weekly",
                (None, None),
            ));
        }
    }
    if let Some(resets) = read_resets(body, reset_body.as_ref()) {
        lines.push(MetricLine::Values {
            label: "Rate Limit Resets".into(),
            values: vec![MetricValue {
                number: resets.0 as f64,
                kind: MetricKind::Count,
                label: Some("available".into()),
                estimated: false,
            }],
            color_hex: None,
            expiries_at: resets.1,
            unknown_models: vec![],
        });
    }
    if let Some(remaining) = read_credits(body, header_credits) {
        let credits = remaining.max(0.0).floor();
        let dollars = credits * 0.04;
        lines.push(MetricLine::Values {
            label: "Credits".into(),
            values: vec![
                MetricValue {
                    number: dollars,
                    kind: MetricKind::Dollars,
                    label: None,
                    estimated: false,
                },
                MetricValue {
                    number: credits,
                    kind: MetricKind::Count,
                    label: Some("credits".into()),
                    estimated: false,
                },
            ],
            color_hex: None,
            expiries_at: vec![],
            unknown_models: vec![],
        });
    }
    lines
}

fn is_spark(entry: &&Value) -> bool {
    let name = str_at(entry, "limit_name")
        .or_else(|| str_at(entry, "metered_feature"))
        .unwrap_or("")
        .to_ascii_lowercase();
    name.contains("spark")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowKind {
    Session,
    Weekly,
}

fn window_lines(
    rate: Option<&Value>,
    session: &str,
    weekly: &str,
    header_percents: (Option<f64>, Option<f64>),
) -> Vec<MetricLine> {
    let candidates = [
        (
            rate.and_then(|rate| rate.get("primary_window")),
            header_percents.0,
            WindowKind::Session,
        ),
        (
            rate.and_then(|rate| rate.get("secondary_window")),
            header_percents.1,
            WindowKind::Weekly,
        ),
    ];
    [
        (WindowKind::Session, session, SESSION_MS),
        (WindowKind::Weekly, weekly, WEEK_MS),
    ]
    .into_iter()
    .filter_map(|(kind, label, fallback_period)| {
        let exact = candidates.iter().find(|(window, _, _)| {
            window.and_then(read_period_ms).and_then(window_kind) == Some(kind)
        });
        let fallback = candidates.iter().find(|(window, _, fallback_kind)| {
            *fallback_kind == kind
                && window
                    .and_then(read_period_ms)
                    .and_then(window_kind)
                    .is_none()
        });
        let (window, header, _) = exact.or(fallback)?;
        let used = window
            .and_then(|window| num_at(window, "used_percent"))
            .or(*header)?;
        let period = window.and_then(read_period_ms).unwrap_or(fallback_period);
        Some(MetricLine::percent(
            label,
            used,
            window.and_then(reset_date),
            period,
        ))
    })
    .collect()
}

fn window_kind(period_ms: i64) -> Option<WindowKind> {
    match period_ms {
        SESSION_MS => Some(WindowKind::Session),
        WEEK_MS => Some(WindowKind::Weekly),
        _ => None,
    }
}

fn read_period_ms(win: &Value) -> Option<i64> {
    num_at(win, "limit_window_seconds")
        .or_else(|| num_at(win, "window_seconds"))
        .map(|s| (s * 1000.0) as i64)
}

fn reset_date(win: &Value) -> Option<chrono::DateTime<Utc>> {
    win.get("reset_at").and_then(as_date).or_else(|| {
        num_at(win, "reset_after_seconds")
            .map(|seconds| Utc::now() + Duration::seconds(seconds as i64))
    })
}

fn read_resets(body: &Value, extra: Option<&Value>) -> Option<(i64, Vec<chrono::DateTime<Utc>>)> {
    let source = extra
        .filter(|value| num_at(value, "available_count").is_some())
        .or_else(|| body.get("rate_limit_reset_credits"))?;
    let count = num_at(source, "available_count").filter(|count| *count >= 0.0)?;
    let mut expiries: Vec<_> = source
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|credit| str_at(credit, "status").is_none_or(|status| status == "available"))
        .filter_map(|credit| credit.get("expires_at").and_then(as_date))
        .collect();
    expiries.sort();
    Some((count.floor() as i64, expiries))
}

fn read_credits(body: &Value, header: Option<f64>) -> Option<f64> {
    body.pointer("/credits/balance")
        .and_then(num)
        .or_else(|| {
            (body
                .pointer("/credits/has_credits")
                .and_then(Value::as_bool)
                == Some(false))
            .then_some(0.0)
        })
        .or_else(|| num_at(body, "credits_remaining"))
        .or_else(|| body.pointer("/credits/remaining").and_then(num))
        .or_else(|| body.pointer("/rate_limit/credits_remaining").and_then(num))
        .or(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_a_weekly_only_primary_window_by_duration() {
        let rate = json!({
            "primary_window": {
                "used_percent": 7,
                "limit_window_seconds": 604800,
                "reset_at": 1_800_000_000
            }
        });
        let lines = window_lines(Some(&rate), "Session", "Weekly", (None, None));
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            &lines[0],
            MetricLine::Progress { label, used, period_duration_ms: Some(period), resets_at: Some(_), .. }
                if label == "Weekly" && *used == 7.0 && *period == WEEK_MS
        ));
    }

    #[test]
    fn response_headers_fill_missing_windows() {
        let lines = window_lines(None, "Session", "Weekly", (Some(25.0), Some(50.0)));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].label(), "Session");
        assert_eq!(lines[1].label(), "Weekly");
    }

    #[test]
    fn reset_credits_filter_consumed_entries_and_fall_back() {
        let body = json!({"rate_limit_reset_credits":{"available_count":1}});
        let dedicated = json!({
            "available_count": 2.9,
            "credits": [
                {"status":"available", "expires_at":"2026-08-14T10:00:00Z"},
                {"expires_at":"2026-08-13T10:00:00Z"},
                {"status":"consumed", "expires_at":"2026-08-12T10:00:00Z"}
            ]
        });
        let resets = read_resets(&body, Some(&dedicated)).expect("reset credits");
        assert_eq!(resets.0, 2);
        assert_eq!(resets.1.len(), 2);
        assert!(resets.1[0] < resets.1[1]);
        assert_eq!(
            read_resets(&body, Some(&json!({"available_count":null})))
                .unwrap()
                .0,
            1
        );
    }

    #[test]
    fn formats_plan_and_floors_credit_balance() {
        assert_eq!(
            plan_name(&json!({"plan_type":"prolite"})).as_deref(),
            Some("Pro 5x")
        );
        let lines = map_usage(
            &json!({"credits":{"balance":821.9}}),
            None,
            (None, None),
            None,
        );
        let MetricLine::Values { values, .. } = &lines[0] else {
            panic!("expected credits")
        };
        assert_eq!(values[0].number, 32.84);
        assert_eq!(values[1].number, 821.0);
    }

    #[tokio::test]
    async fn read_only_auth_handles_expiry_replacement_deletion_and_authorization() {
        use base64::Engine as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let provider = CodexProvider::new();
        let jwt = |expiry| {
            format!(
                "x.{}.x",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(format!("{{\"exp\":{expiry}}}"))
            )
        };
        let directory = std::env::temp_dir().join(format!(
            "ehr-codex-fixture-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("auth.json");
        let calls = Arc::new(AtomicUsize::new(0));
        let outcome = Arc::new(Mutex::new((200u16, false, false)));
        let fixture = json!({"tokens":{"access_token":jwt(Utc::now().timestamp()+60), "account_id":"fixture-old", "refresh_token":"never-post"}});
        let bytes = serde_json::to_vec(&fixture).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let read_path = path.clone();
        let read = move || {
            std::fs::read(&read_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        };
        let mock_path = path.clone();
        let count = calls.clone();
        let current = outcome.clone();
        let http = Http::mocked(move |method, _url| {
            assert_eq!(method, Method::GET, "OAuth refresh POST is forbidden");
            count.fetch_add(1, Ordering::SeqCst);
            let (status, replace, delete) = *current.lock().unwrap();
            if replace {
                std::fs::write(
                    &mock_path,
                    br#"{"tokens":{"access_token":"new","account_id":"new"}}"#,
                )
                .unwrap();
            }
            if delete {
                std::fs::remove_file(&mock_path).ok();
            }
            Ok(crate::http::HttpResponse { received_at: Utc::now(), status, body: br#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000}}}"#.to_vec(), headers: std::collections::HashMap::new() })
        });
        assert!(!provider.refresh_from(&http, &read, false).await.is_error());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        for status in [401, 403] {
            *outcome.lock().unwrap() = (status, false, false);
            let result = provider.refresh_from(&http, &read, false).await;
            assert_eq!(
                result.failure.unwrap().category,
                FailureCategory::Authentication
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        for (replace, delete) in [(true, false), (false, true)] {
            std::fs::write(&path, &bytes).unwrap();
            *outcome.lock().unwrap() = (200, replace, delete);
            assert_eq!(
                provider
                    .refresh_from(&http, &read, false)
                    .await
                    .failure
                    .unwrap()
                    .category,
                FailureCategory::Superseded
            );
        }
        *outcome.lock().unwrap() = (200, false, false);
        let expired = json!({"tokens":{"access_token":jwt(Utc::now().timestamp()-1),"refresh_token":"never-post"}});
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        let before = calls.load(Ordering::SeqCst);
        assert_eq!(
            provider
                .refresh_from(&http, &read, false)
                .await
                .failure
                .unwrap()
                .category,
            FailureCategory::Authentication
        );
        assert_eq!(calls.load(Ordering::SeqCst), before);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
