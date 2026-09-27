use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;

use super::{widget, Provider};
use crate::http::Http;
use crate::json::{bool_at, num, num_at, parse_iso, str_at};
use crate::models::*;
use crate::paths;

pub struct CopilotProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl CopilotProvider {
    pub fn new() -> Self {
        Self {
            info: ProviderInfo {
                id: "copilot".into(),
                display_name: "Copilot".into(),
                icon: "copilot".into(),
                links: vec![
                    ProviderLink {
                        label: "Status".into(),
                        url: "https://www.githubstatus.com/".into(),
                    },
                    ProviderLink {
                        label: "Dashboard".into(),
                        url: "https://github.com/settings/billing".into(),
                    },
                ],
            },
            widgets: vec![
                widget("copilot.premium", "copilot", "Credits", true),
                widget("copilot.extra", "copilot", "Extra Usage", true),
                widget("copilot.orgCredits", "copilot", "Org Credits", true),
                widget("copilot.orgSpend", "copilot", "Org Spend", true),
                widget("copilot.chat", "copilot", "Chat", true),
                widget("copilot.completions", "copilot", "Completions", true),
            ],
        }
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        load_token().is_some()
    }

    async fn refresh(&self, http: &Http) -> ProviderSnapshot {
        let token = match load_token() {
            Some(t) => t,
            None => return ProviderSnapshot::err(
                &self.info,
                "Sign in to GitHub Copilot in your editor, or run gh auth login, and try again.",
            ),
        };
        let res = match http
            .send(
                Method::GET,
                "https://api.github.com/copilot_internal/user",
                &[
                    ("Authorization", &format!("token {token}")),
                    ("Accept", "application/json"),
                    ("Editor-Version", "vscode/1.96.2"),
                    ("Editor-Plugin-Version", "copilot-chat/0.26.7"),
                    ("User-Agent", "GitHubCopilotChat/0.26.7"),
                    ("X-Github-Api-Version", "2025-04-01"),
                ],
                None,
                15,
            )
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Couldn't reach GitHub. Check your connection.",
                )
            }
        };
        if res.status == 401 || res.status == 403 {
            return ProviderSnapshot::err(
                &self.info,
                "GitHub token invalid or expired. Re-authenticate (gh auth login) and try again.",
            );
        }
        if !res.ok() {
            return ProviderSnapshot::err(
                &self.info,
                &format!("Copilot usage request failed (HTTP {}).", res.status),
            );
        }
        let body = match res.json() {
            Some(v) => v,
            None => {
                return ProviderSnapshot::err(
                    &self.info,
                    "Copilot usage response invalid. Try again later.",
                )
            }
        };
        match map_usage(&body) {
            Ok((plan, lines, org)) => {
                let mut lines = lines;
                if org {
                    if let Some(extra) = org_billing(http, &token).await {
                        lines.extend(extra);
                    }
                }
                ProviderSnapshot::ok(&self.info, plan, lines)
            }
            Err(msg) => ProviderSnapshot::err(&self.info, &msg),
        }
    }
}

fn load_token() -> Option<String> {
    let home = paths::home();
    let mut editor_paths = vec![
        home.join(".config/github-copilot/apps.json"),
        home.join(".config/github-copilot/hosts.json"),
    ];
    if let Some(config) = dirs::config_dir() {
        editor_paths.push(config.join("github-copilot/apps.json"));
        editor_paths.push(config.join("github-copilot/hosts.json"));
    }
    // On Windows the editor plugins write under %LOCALAPPDATA%, not the roaming config root
    // `config_dir` resolves to. Every other Windows-aware provider here checks both.
    if let Some(local) = dirs::data_local_dir() {
        editor_paths.push(local.join("github-copilot/apps.json"));
        editor_paths.push(local.join("github-copilot/hosts.json"));
    }
    for path in editor_paths {
        if let Some(text) = paths::read_text(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(t) = extract_oauth(&v) {
                    return Some(t);
                }
            }
        }
    }
    let mut gh_paths = vec![home.join(".config/gh/hosts.yml")];
    if let Some(config) = dirs::config_dir() {
        gh_paths.push(config.join("gh/hosts.yml"));
        gh_paths.push(config.join("GitHub CLI/hosts.yml"));
    }
    let mut gh_user = None;
    for path in gh_paths {
        if let Some(text) = paths::read_text(&path) {
            if let Some(token) = github_yaml_value(&text, "oauth_token") {
                return Some(token);
            }
            gh_user = gh_user.or_else(|| github_yaml_value(&text, "user"));
        }
    }
    gh_user
        .as_deref()
        .and_then(|user| paths::cred_read("gh:github.com", Some(user)))
        .or_else(|| paths::cred_read("gh:github.com", Some("github.com")))
        .or_else(|| paths::cred_read("gh:github.com", None))
}

fn extract_oauth(v: &Value) -> Option<String> {
    let map = v.as_object()?;
    map.iter()
        .filter(|(host, _)| host.as_str() == "github.com" || host.starts_with("github.com:"))
        .find_map(|(_, entry)| {
            str_at(entry, "oauth_token")
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn github_yaml_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let mut in_github = false;
    for line in text.lines() {
        if line
            .chars()
            .next()
            .is_some_and(|character| !character.is_whitespace())
        {
            in_github = line.trim_start().starts_with("github.com:");
            continue;
        }
        if !in_github {
            continue;
        }
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix(&prefix) {
            let value = value
                .trim()
                .trim_matches(|character| matches!(character, '"' | '\''));
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

fn map_usage(body: &Value) -> Result<(Option<String>, Vec<MetricLine>, bool), String> {
    let plan = str_at(body, "copilot_plan").map(crate::json::title_case);
    let resets = str_at(body, "quota_reset_date")
        .or_else(|| str_at(body, "limited_user_reset_date"))
        .and_then(parse_iso);
    let mut lines = Vec::new();
    let snapshots = body.get("quota_snapshots").cloned().unwrap_or(Value::Null);
    let credits = snapshot_line("Credits", snapshots.get("premium_interactions"), resets);
    if let Some(l) = credits {
        lines.push(l);
        if let Some(extra) = overage_line(snapshots.get("premium_interactions")) {
            lines.push(extra);
        }
    }
    if let Some(l) = snapshot_line("Chat", snapshots.get("chat"), resets) {
        lines.push(l);
    }
    if let Some(l) = snapshot_line("Completions", snapshots.get("completions"), resets) {
        lines.push(l);
    }
    if lines.is_empty() {
        let limited = body
            .get("limited_user_quotas")
            .cloned()
            .unwrap_or(Value::Null);
        let monthly = body.get("monthly_quotas").cloned().unwrap_or(Value::Null);
        if let Some(l) = limited_line("Chat", limited.get("chat"), monthly.get("chat"), resets) {
            lines.push(l);
        }
        if let Some(l) = limited_line(
            "Completions",
            limited.get("completions"),
            monthly.get("completions"),
            resets,
        ) {
            lines.push(l);
        }
    }
    if lines.is_empty() {
        if bool_at(body, "token_based_billing") == Some(true) {
            return Ok((plan, vec![], true));
        }
        return Err("Copilot usage data is unavailable for this account.".into());
    }
    Ok((plan, lines, false))
}

fn snapshot_line(
    label: &str,
    raw: Option<&Value>,
    resets: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<MetricLine> {
    let snap = raw?;
    let entitlement = num_at(snap, "entitlement");
    let remaining = num_at(snap, "remaining");
    if bool_at(snap, "unlimited") == Some(true)
        || entitlement == Some(-1.0)
        || remaining == Some(-1.0)
    {
        return None;
    }
    if entitlement == Some(0.0) {
        return None;
    }
    let used = if let Some(pr) = num_at(snap, "percent_remaining") {
        crate::json::clamp_percent(100.0 - pr)
    } else if let (Some(ent), Some(rem)) = (entitlement, remaining) {
        if ent <= 0.0 {
            return None;
        }
        crate::json::clamp_percent(100.0 - (rem / ent) * 100.0)
    } else {
        return None;
    };
    Some(MetricLine::percent(label, used, resets, MONTH_MS))
}

fn overage_line(raw: Option<&Value>) -> Option<MetricLine> {
    let snap = raw?;
    if bool_at(snap, "overage_permitted") != Some(true) {
        return None;
    }
    let overage = num_at(snap, "overage_count").unwrap_or(0.0).max(0.0);
    Some(MetricLine::Values {
        label: "Extra Usage".into(),
        values: vec![MetricValue {
            number: overage,
            kind: MetricKind::Count,
            label: None,
            estimated: false,
        }],
        color_hex: None,
        expiries_at: vec![],
        unknown_models: vec![],
    })
}

fn limited_line(
    label: &str,
    remaining: Option<&Value>,
    total: Option<&Value>,
    resets: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<MetricLine> {
    let total = total.and_then(num).filter(|t| *t > 0.0)?;
    let remaining = remaining.and_then(num)?;
    let used = (total - remaining).max(0.0);
    Some(MetricLine::percent(
        label,
        crate::json::clamp_percent((used / total) * 100.0),
        resets,
        MONTH_MS,
    ))
}

async fn org_billing(http: &Http, token: &str) -> Option<Vec<MetricLine>> {
    let res = http
        .send(
            Method::GET,
            "https://api.github.com/user/orgs?per_page=100",
            &[
                ("Authorization", &format!("token {token}")),
                ("Accept", "application/vnd.github+json"),
                ("User-Agent", "MultiMeters"),
            ],
            None,
            15,
        )
        .await
        .ok()?;
    if !res.ok() {
        return None;
    }
    let orgs = res.json()?.as_array()?.to_vec();
    for org in orgs {
        let Some(login) = str_at(&org, "login") else {
            continue;
        };
        let url = format!(
            "https://api.github.com/orgs/{}/settings/billing/usage/summary",
            urlencoding::encode(login)
        );
        if let Ok(b) = http
            .send(
                Method::GET,
                &url,
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("Accept", "application/vnd.github+json"),
                    ("User-Agent", "MultiMeters"),
                ],
                None,
                15,
            )
            .await
        {
            if !b.ok() {
                continue;
            }
            if let Some(lines) = b.json().as_ref().and_then(map_org_billing) {
                return Some(lines);
            }
        }
    }
    None
}

fn map_org_billing(body: &Value) -> Option<Vec<MetricLine>> {
    let items = body.get("usageItems")?.as_array()?;
    let credit_items: Vec<_> = items
        .iter()
        .filter(|item| {
            str_at(item, "product")
                .is_some_and(|product| product.trim().eq_ignore_ascii_case("copilot"))
                && str_at(item, "unitType").is_some_and(|unit| {
                    matches!(
                        unit.trim().to_ascii_lowercase().as_str(),
                        "ai-units" | "ai-credits"
                    )
                })
        })
        .collect();
    if credit_items.is_empty() {
        return None;
    }
    let credits: f64 = credit_items
        .iter()
        .map(|item| num_at(item, "grossQuantity").unwrap_or(0.0).max(0.0))
        .sum();
    let spend: f64 = credit_items
        .iter()
        .map(|item| num_at(item, "netAmount").unwrap_or(0.0).max(0.0))
        .sum();
    Some(vec![
        MetricLine::Values {
            label: "Org Credits".into(),
            values: vec![MetricValue {
                number: credits,
                kind: MetricKind::Count,
                label: Some("credits".into()),
                estimated: false,
            }],
            color_hex: None,
            expiries_at: vec![],
            unknown_models: vec![],
        },
        MetricLine::values_dollars("Org Spend", spend),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn editor_and_gh_tokens_are_scoped_to_github_dot_com() {
        let editor = json!({
            "enterprise.example": {"oauth_token":"wrong"},
            "github.com:copilot": {"oauth_token":"right"}
        });
        assert_eq!(extract_oauth(&editor).as_deref(), Some("right"));
        let yaml = "enterprise.example:\n    oauth_token: wrong\ngithub.com:\n    user: octocat\n    oauth_token: right\n";
        assert_eq!(
            github_yaml_value(yaml, "oauth_token").as_deref(),
            Some("right")
        );
        assert_eq!(github_yaml_value(yaml, "user").as_deref(), Some("octocat"));
    }

    #[test]
    fn org_billing_only_counts_copilot_credit_units() {
        let lines = map_org_billing(&json!({"usageItems":[
            {"product":"Copilot", "unitType":"ai-credits", "grossQuantity":12, "netAmount":3.5},
            {"product":"Copilot", "unitType":"seats", "grossQuantity":99, "netAmount":99},
            {"product":"Actions", "unitType":"ai-credits", "grossQuantity":99, "netAmount":99}
        ]}))
        .expect("Copilot credit usage");
        assert!(matches!(
            &lines[0],
            MetricLine::Values { values, .. } if values[0].number == 12.0
        ));
        assert!(matches!(
            &lines[1],
            MetricLine::Values { values, .. } if values[0].number == 3.5
        ));
    }
}
