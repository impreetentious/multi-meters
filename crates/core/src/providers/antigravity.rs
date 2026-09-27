use async_trait::async_trait;
use base64::Engine as _;
use chrono::{Duration, Utc};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{widget, Provider};
use crate::http::Http;
use crate::json::{as_date, bool_at, num_at, str_at};
use crate::models::*;
use crate::paths;

pub struct AntigravityProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl AntigravityProvider {
    pub fn new() -> Self {
        Self {
            info: ProviderInfo {
                id: "antigravity".into(),
                display_name: "Antigravity".into(),
                icon: "antigravity".into(),
                links: vec![],
            },
            widgets: vec![
                widget("antigravity.geminiPro", "antigravity", "Session", true),
                widget("antigravity.geminiWeekly", "antigravity", "Weekly", true),
                widget("antigravity.claude", "antigravity", "Claude", true),
                widget(
                    "antigravity.claudeWeekly",
                    "antigravity",
                    "Claude Weekly",
                    true,
                ),
            ],
        }
    }
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        match load_auth() {
            Ok(auth) => auth.is_some(),
            Err(error) => {
                // Detection must keep an indeterminate credential enabled so refresh can explain
                // how to repair a locked or damaged credential-manager entry.
                tracing::warn!(%error, "Antigravity credential status is indeterminate");
                true
            }
        }
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let mut auth = match load_auth() {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Start Antigravity or run `agy` and try again.",
                )
            }
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        if !auth.access_usable() {
            if auth.refresh.is_none() {
                return expired(&self.info);
            }
            match refresh_auth(http, &mut auth).await {
                TokenRefresh::Refreshed => {}
                TokenRefresh::AuthFailed => return expired(&self.info),
                TokenRefresh::Unavailable => return unavailable(&self.info),
            }
        }

        match fetch_usage(http, auth.access.as_deref().unwrap_or_default()).await {
            UsageProbe::Success(result) => {
                ProviderSnapshot::ok(&self.info, result.plan, result.lines)
            }
            UsageProbe::Unavailable => unavailable(&self.info),
            UsageProbe::AuthFailed if auth.refresh.is_none() => expired(&self.info),
            UsageProbe::AuthFailed => match refresh_auth(http, &mut auth).await {
                TokenRefresh::AuthFailed => expired(&self.info),
                TokenRefresh::Unavailable => unavailable(&self.info),
                TokenRefresh::Refreshed => {
                    match fetch_usage(http, auth.access.as_deref().unwrap_or_default()).await {
                        UsageProbe::Success(result) => {
                            ProviderSnapshot::ok(&self.info, result.plan, result.lines)
                        }
                        UsageProbe::AuthFailed => expired(&self.info),
                        UsageProbe::Unavailable => unavailable(&self.info),
                    }
                }
            },
        }
    }
}

fn expired(info: &ProviderInfo) -> ProviderSnapshot {
    ProviderSnapshot::err(
        info,
        "Antigravity sign-in expired. Open Antigravity or run `agy` to refresh.",
    )
}

fn unavailable(info: &ProviderInfo) -> ProviderSnapshot {
    ProviderSnapshot::err(
        info,
        "Antigravity usage is temporarily unavailable. Try again shortly.",
    )
}

#[derive(Default)]
struct AntigravityAuth {
    access: Option<String>,
    refresh: Option<String>,
    expiry: Option<chrono::DateTime<Utc>>,
}

impl AntigravityAuth {
    fn access_usable(&self) -> bool {
        self.access.as_ref().is_some_and(|token| !token.is_empty())
            && self
                .expiry
                .is_none_or(|expiry| expiry - Utc::now() > Duration::minutes(1))
    }
}

fn load_auth() -> Result<Option<AntigravityAuth>, String> {
    let raw = paths::cred_read_checked("gemini", Some("antigravity")).map_err(|error| {
        format!("Couldn't read Antigravity credentials from Windows Credential Manager: {error}")
    })?;
    let Some(raw) = raw else {
        discard_cached_access();
        return Ok(None);
    };
    let mut auth = extract_auth(&raw).ok_or_else(|| {
        "Antigravity credentials are invalid. Open Antigravity or run `agy` to sign in again."
            .to_string()
    })?;
    if !auth.access_usable() {
        if let Some(cached) = load_cached_access(auth.refresh.as_deref()) {
            auth.access = Some(cached.0);
            auth.expiry = Some(cached.1);
        }
    }
    Ok(Some(auth))
}

fn extract_auth(raw: &str) -> Option<AntigravityAuth> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return auth_from_value(&v);
    }
    if text.starts_with('{') || text.starts_with('[') {
        return None;
    }
    if let Some(rest) = text.strip_prefix("Bearer ") {
        let t = rest.trim();
        if !t.is_empty() {
            return Some(AntigravityAuth {
                access: Some(t.to_string()),
                ..AntigravityAuth::default()
            });
        }
    }
    Some(AntigravityAuth {
        access: Some(text.to_string()),
        ..AntigravityAuth::default()
    })
}

fn auth_from_value(v: &Value) -> Option<AntigravityAuth> {
    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
        return Some(AntigravityAuth {
            access: Some(s.to_string()),
            ..AntigravityAuth::default()
        });
    }
    let source = v.get("token").unwrap_or(v);
    let access = [
        "access_token",
        "accessToken",
        "token",
        "id_token",
        "idToken",
        "bearerToken",
        "auth_token",
        "authToken",
    ]
    .into_iter()
    .find_map(|key| str_at(source, key).filter(|value| !value.is_empty()))
    .map(ToOwned::to_owned);
    let refresh = str_at(source, "refresh_token")
        .or_else(|| str_at(source, "refreshToken"))
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let expiry = ["expiry", "expires_at", "expiresAt"]
        .into_iter()
        .find_map(|key| source.get(key).and_then(crate::json::as_date));
    if access.is_some() || refresh.is_some() {
        return Some(AntigravityAuth {
            access,
            refresh,
            expiry,
        });
    }
    for key in ["tokens", "oauth", "oauth2", "credentials", "auth"] {
        if let Some(nested) = v.get(key) {
            if let Some(auth) = auth_from_value(nested) {
                return Some(auth);
            }
        }
    }
    None
}

enum CloudOutcome {
    Ok(Value),
    AuthFailed,
    Unavailable,
}

enum TokenRefresh {
    Refreshed,
    AuthFailed,
    Unavailable,
}

struct UsageResult {
    plan: Option<String>,
    lines: Vec<MetricLine>,
}

enum UsageProbe {
    Success(UsageResult),
    AuthFailed,
    Unavailable,
}

async fn cloud_code(
    http: &Http,
    path: &str,
    token: &str,
    user_agent: &str,
    body: &Value,
) -> CloudOutcome {
    let authorization = format!("Bearer {token}");
    let payload = match serde_json::to_vec(body) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(%error, %path, "could not encode Antigravity request");
            return CloudOutcome::Unavailable;
        }
    };
    for base in [
        "https://daily-cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com",
    ] {
        let response = match http
            .send(
                Method::POST,
                &format!("{base}{path}"),
                &[
                    ("Authorization", authorization.as_str()),
                    ("Content-Type", "application/json"),
                    ("Accept", "application/json"),
                    ("User-Agent", user_agent),
                ],
                Some(payload.clone()),
                15,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, %path, %base, "Antigravity endpoint was unreachable");
                continue;
            }
        };
        if matches!(response.status, 401 | 403) {
            return CloudOutcome::AuthFailed;
        }
        if response.ok() {
            if let Some(value) = response.json() {
                return CloudOutcome::Ok(value);
            }
            tracing::warn!(%path, %base, "Antigravity endpoint returned invalid JSON");
        }
    }
    CloudOutcome::Unavailable
}

async fn refresh_auth(http: &Http, auth: &mut AntigravityAuth) -> TokenRefresh {
    let Some(refresh) = auth.refresh.clone() else {
        return TokenRefresh::AuthFailed;
    };
    match refresh_google_token(http, &refresh).await {
        GoogleRefresh::Refreshed(access, expires_in) => {
            auth.access = Some(access);
            auth.expiry = Some(Utc::now() + Duration::seconds(expires_in));
            cache_access_token(auth);
            TokenRefresh::Refreshed
        }
        GoogleRefresh::AuthFailed => TokenRefresh::AuthFailed,
        GoogleRefresh::Unavailable => TokenRefresh::Unavailable,
    }
}

enum GoogleRefresh {
    Refreshed(String, i64),
    AuthFailed,
    Unavailable,
}

async fn refresh_google_token(http: &Http, refresh: &str) -> GoogleRefresh {
    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        urlencoding::encode(
            "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
        ),
        urlencoding::encode("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"),
        urlencoding::encode(refresh),
    );
    let response = match http
        .send(
            Method::POST,
            "https://oauth2.googleapis.com/token",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            Some(body.into_bytes()),
            15,
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "Google OAuth refresh was unreachable");
            return GoogleRefresh::Unavailable;
        }
    };
    if !response.ok() {
        return if (400..500).contains(&response.status) && !matches!(response.status, 408 | 429) {
            GoogleRefresh::AuthFailed
        } else {
            GoogleRefresh::Unavailable
        };
    }
    let Some(body) = response.json() else {
        return GoogleRefresh::Unavailable;
    };
    let Some(access) = str_at(&body, "access_token")
        .map(str::trim)
        .filter(|access| !access.is_empty())
    else {
        return GoogleRefresh::Unavailable;
    };
    let expires_in = num_at(&body, "expires_in")
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .unwrap_or(3_600.0) as i64;
    GoogleRefresh::Refreshed(access.to_string(), expires_in)
}

fn credential_fingerprint(refresh: Option<&str>) -> Option<String> {
    let refresh = refresh.map(str::trim).filter(|value| !value.is_empty())?;
    let digest = Sha256::digest(refresh.as_bytes());
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest))
}

fn load_cached_access(refresh: Option<&str>) -> Option<(String, chrono::DateTime<Utc>)> {
    let Some(expected) = credential_fingerprint(refresh) else {
        discard_cached_access();
        return None;
    };
    let path = paths::app_data().join("antigravity/auth.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(%error, "could not read Antigravity token cache");
            return None;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Antigravity token cache is malformed; discarding it");
            discard_cached_access();
            return None;
        }
    };
    if str_at(&value, "credentialFingerprint") != Some(expected.as_str()) {
        discard_cached_access();
        return None;
    }
    let Some(access) = str_at(&value, "accessToken")
        .map(str::trim)
        .filter(|access| !access.is_empty())
        .map(ToOwned::to_owned)
    else {
        discard_cached_access();
        return None;
    };
    let Some(expiry) = value.get("expiresAtMs").and_then(crate::json::as_date) else {
        discard_cached_access();
        return None;
    };
    if expiry - Utc::now() <= Duration::minutes(1) {
        discard_cached_access();
        return None;
    }
    Some((access, expiry))
}

fn cache_access_token(auth: &AntigravityAuth) {
    let (Some(access), Some(expiry), Some(fingerprint)) = (
        &auth.access,
        auth.expiry,
        credential_fingerprint(auth.refresh.as_deref()),
    ) else {
        return;
    };
    let value = json!({
        "accessToken": access,
        "expiresAtMs": expiry.timestamp_millis(),
        "credentialFingerprint": fingerprint,
    });
    if let Err(error) =
        paths::write_secret_json(&paths::app_data().join("antigravity/auth.json"), &value)
    {
        tracing::warn!(%error, "could not cache refreshed Antigravity token");
    }
}

fn discard_cached_access() {
    let path = paths::app_data().join("antigravity/auth.json");
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not remove stale Antigravity token cache"),
    }
}

async fn fetch_usage(http: &Http, token: &str) -> UsageProbe {
    match cloud_code(
        http,
        "/v1internal:retrieveUserQuotaSummary",
        token,
        "antigravity",
        &json!({}),
    )
    .await
    {
        CloudOutcome::AuthFailed => return UsageProbe::AuthFailed,
        CloudOutcome::Ok(body) => {
            if let Some(lines) = map_summary(&body) {
                return UsageProbe::Success(UsageResult {
                    plan: load_plan_project(http, token).await.0,
                    lines,
                });
            }
        }
        CloudOutcome::Unavailable => {}
    }

    match cloud_code(
        http,
        "/v1internal:fetchAvailableModels",
        token,
        "antigravity",
        &json!({}),
    )
    .await
    {
        CloudOutcome::AuthFailed => return UsageProbe::AuthFailed,
        CloudOutcome::Ok(body) => {
            let lines = map_legacy_models(&body);
            if !lines.is_empty() {
                return UsageProbe::Success(UsageResult {
                    plan: load_plan_project(http, token).await.0,
                    lines,
                });
            }
        }
        CloudOutcome::Unavailable => {}
    }

    let (plan, project) = load_plan_project(http, token).await;
    let mut request = project
        .as_ref()
        .map_or_else(|| json!({}), |project| json!({ "project": project }));
    let mut quota = cloud_code(
        http,
        "/v1internal:retrieveUserQuota",
        token,
        "agy",
        &request,
    )
    .await;
    if matches!(quota, CloudOutcome::Unavailable) && project.is_some() {
        request = json!({});
        quota = cloud_code(
            http,
            "/v1internal:retrieveUserQuota",
            token,
            "agy",
            &request,
        )
        .await;
    }
    match quota {
        CloudOutcome::AuthFailed => UsageProbe::AuthFailed,
        CloudOutcome::Ok(body) => {
            let lines = map_quota_buckets(&body);
            if lines.is_empty() {
                UsageProbe::Unavailable
            } else {
                UsageProbe::Success(UsageResult { plan, lines })
            }
        }
        CloudOutcome::Unavailable => UsageProbe::Unavailable,
    }
}

async fn load_plan_project(http: &Http, token: &str) -> (Option<String>, Option<String>) {
    match cloud_code(http, "/v1internal:loadCodeAssist", token, "agy", &json!({})).await {
        CloudOutcome::Ok(body) => (
            plan_name(&body),
            str_at(&body, "cloudaicompanionProject").map(ToOwned::to_owned),
        ),
        CloudOutcome::AuthFailed | CloudOutcome::Unavailable => (None, None),
    }
}

fn plan_name(value: &Value) -> Option<String> {
    let raw = value
        .pointer("/paidTier/name")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/currentTier/name").and_then(Value::as_str))
        .or_else(|| str_at(value, "userTier"))
        .or_else(|| str_at(value, "plan"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(tier) = raw.strip_prefix("Google AI ") {
        return Some(crate::json::title_case(tier));
    }
    for tier in ["Ultra", "Pro", "Free"] {
        if raw
            .to_ascii_lowercase()
            .contains(&tier.to_ascii_lowercase())
        {
            return Some(tier.into());
        }
    }
    Some(crate::json::title_case(raw))
}

fn map_summary(body: &Value) -> Option<Vec<MetricLine>> {
    let groups = body
        .pointer("/response/groups")
        .or_else(|| body.get("groups"))
        .and_then(Value::as_array)?;
    let mut buckets = std::collections::HashMap::new();
    for group in groups {
        if let Some(arr) = group.get("buckets").and_then(|v| v.as_array()) {
            for bucket in arr {
                if let Some(id) = str_at(bucket, "bucketId") {
                    if !matches!(id, "gemini-5h" | "gemini-weekly" | "3p-5h" | "3p-weekly") {
                        // Antigravity ships buckets this app does not meter (image quotas, for one).
                        // Routine, so it must not compete with real warnings in the log.
                        tracing::debug!(bucket_id = %id, "ignoring an unmapped Antigravity quota bucket");
                        continue;
                    }
                    if !buckets.contains_key(id) {
                        let Some(fraction) = num_at(bucket, "remainingFraction") else {
                            tracing::warn!(bucket_id = %id, "Antigravity quota bucket has no usable remaining fraction");
                            continue;
                        };
                        buckets.insert(
                            id.to_string(),
                            (
                                fraction,
                                as_date(bucket.get("resetTime").unwrap_or(&Value::Null)),
                            ),
                        );
                    }
                }
            }
        }
    }
    let specs = [
        ("gemini-5h", "Session", SESSION_MS),
        ("gemini-weekly", "Weekly", WEEK_MS),
        ("3p-5h", "Claude", SESSION_MS),
        ("3p-weekly", "Claude Weekly", WEEK_MS),
    ];
    Some(
        specs
            .into_iter()
            .filter_map(|(id, label, period)| {
                let (frac, reset) = buckets.get(id)?;
                let used = crate::json::clamp_percent((1.0 - *frac) * 100.0).round();
                Some(MetricLine::percent(label, used, *reset, period))
            })
            .collect(),
    )
}

fn map_legacy_models(body: &Value) -> Vec<MetricLine> {
    let configs = body
        .get("models")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, model)| {
            if bool_at(model, "isInternal") == Some(true) {
                return None;
            }
            let label = str_at(model, "displayName")
                .or_else(|| str_at(model, "label"))?
                .trim();
            if label.is_empty() {
                return None;
            }
            let id = str_at(model, "model").unwrap_or(key);
            let quota = model.get("quotaInfo").unwrap_or(&Value::Null);
            Some((
                label.to_string(),
                id.to_string(),
                num_at(quota, "remainingFraction").unwrap_or(0.0),
                as_date(quota.get("resetTime").unwrap_or(&Value::Null)),
            ))
        });
    build_legacy_lines(configs)
}

fn map_quota_buckets(body: &Value) -> Vec<MetricLine> {
    let configs = body
        .get("buckets")
        .or_else(|| body.pointer("/response/buckets"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| {
            let id = str_at(bucket, "modelId")?.trim();
            if id.is_empty() {
                return None;
            }
            Some((
                id.to_string(),
                id.to_string(),
                num_at(bucket, "remainingFraction").unwrap_or(0.0),
                as_date(bucket.get("resetTime").unwrap_or(&Value::Null)),
            ))
        });
    build_legacy_lines(configs)
}

fn build_legacy_lines<I>(configs: I) -> Vec<MetricLine>
where
    I: IntoIterator<Item = (String, String, f64, Option<chrono::DateTime<Utc>>)>,
{
    const BLACKLIST: &[&str] = &[
        "MODEL_CHAT_20706",
        "MODEL_CHAT_23310",
        "MODEL_GOOGLE_GEMINI_2_5_FLASH",
        "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING",
        "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE",
        "MODEL_GOOGLE_GEMINI_2_5_PRO",
        "MODEL_PLACEHOLDER_M19",
        "MODEL_PLACEHOLDER_M9",
        "MODEL_PLACEHOLDER_M12",
    ];
    let mut pools: std::collections::HashMap<&'static str, (f64, Option<chrono::DateTime<Utc>>)> =
        std::collections::HashMap::new();
    for (label, model_id, fraction, reset) in configs {
        if BLACKLIST.contains(&model_id.as_str()) || !fraction.is_finite() {
            continue;
        }
        let normalized = label
            .rfind(" (")
            .filter(|_| label.ends_with(')'))
            .map_or(label.as_str(), |index| &label[..index])
            .trim();
        if normalized.is_empty() {
            continue;
        }
        let pool = if normalized.to_ascii_lowercase().contains("gemini") {
            "Session"
        } else {
            "Claude"
        };
        let candidate = (fraction, reset);
        match pools.get(pool) {
            Some((current, _)) if *current <= fraction => {}
            _ => {
                pools.insert(pool, candidate);
            }
        }
    }
    ["Session", "Claude"]
        .into_iter()
        .filter_map(|label| {
            let (remaining, reset) = pools.get(label)?;
            let used = crate::json::clamp_percent((1.0 - *remaining) * 100.0).round();
            Some(MetricLine::percent(label, used, *reset, SESSION_MS))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_agy_token() {
        let raw = r#"{"token":{"access_token":"ya29.abc","refresh_token":"1//xyz"}}"#;
        let auth = extract_auth(raw).expect("auth");
        assert_eq!(auth.access.as_deref(), Some("ya29.abc"));
        assert_eq!(auth.refresh.as_deref(), Some("1//xyz"));
    }

    #[test]
    fn parses_bearer_credentials_without_treating_them_as_json() {
        let auth = extract_auth("Bearer ya29.token").expect("auth");
        assert_eq!(auth.access.as_deref(), Some("ya29.token"));
    }

    #[test]
    fn summary_is_authoritative_and_maps_exact_buckets() {
        let lines = map_summary(&json!({ "groups": [{ "buckets": [
            { "bucketId": "gemini-5h", "remainingFraction": 0.25 },
            { "bucketId": "gemini-image-5h", "remainingFraction": 1.0 },
            { "bucketId": "3p-weekly", "remainingFraction": 0.8 }
        ]}]}))
        .expect("summary");
        assert_eq!(lines.len(), 2);
        assert!(
            matches!(&lines[0], MetricLine::Progress { label, used, .. } if label == "Session" && *used == 75.0)
        );
        assert!(
            matches!(&lines[1], MetricLine::Progress { label, used, period_duration_ms: Some(period), .. } if label == "Claude Weekly" && *used == 20.0 && *period == WEEK_MS)
        );
        assert!(map_summary(&json!({ "groups": [] })).is_some());
        assert!(map_summary(&json!({ "models": {} })).is_none());
    }

    #[test]
    fn legacy_models_pool_gemini_and_non_gemini_by_worst_quota() {
        let lines = map_legacy_models(&json!({ "models": {
            "gemini-pro": { "displayName": "Gemini 3 Pro (High)", "quotaInfo": { "remainingFraction": 0.7 } },
            "gemini-flash": { "displayName": "Gemini Flash", "quotaInfo": { "remainingFraction": 0.4 } },
            "claude": { "displayName": "Claude Sonnet", "quotaInfo": { "remainingFraction": 0.9 } },
            "internal": { "displayName": "Gemini Internal", "isInternal": true, "quotaInfo": { "remainingFraction": 0.0 } }
        }}));
        assert_eq!(lines.len(), 2);
        assert!(
            matches!(&lines[0], MetricLine::Progress { label, used, .. } if label == "Session" && *used == 60.0)
        );
        assert!(
            matches!(&lines[1], MetricLine::Progress { label, used, .. } if label == "Claude" && *used == 10.0)
        );
    }

    #[test]
    fn refreshed_token_cache_fingerprint_is_bound_to_refresh_credential() {
        assert_eq!(
            credential_fingerprint(Some(" refresh ")),
            credential_fingerprint(Some("refresh"))
        );
        assert_ne!(
            credential_fingerprint(Some("refresh-a")),
            credential_fingerprint(Some("refresh-b"))
        );
        assert!(credential_fingerprint(None).is_none());
    }
}
