use std::collections::{HashMap, HashSet};

use chrono::{Duration, Utc};
use serde_json::{json, Map, Value};

use crate::models::{
    MetricKind, MetricLine, ProgressFormat, ProviderInfo, ProviderSnapshot, WidgetDescriptor,
    CACHE_TTL_SECS,
};

pub struct ProviderLimits<'a> {
    pub info: &'a ProviderInfo,
    pub widgets: &'a [WidgetDescriptor],
    pub snapshot: Option<&'a ProviderSnapshot>,
}

pub fn envelope(providers: &[ProviderLimits<'_>], errors: &HashMap<String, String>) -> Value {
    let mut output = Map::new();
    let included: HashSet<&str> = providers
        .iter()
        .map(|provider| provider.info.id.as_str())
        .collect();

    for provider in providers {
        let Some(snapshot) = provider.snapshot else {
            continue;
        };
        if snapshot.is_error() && snapshot.lines.len() == 1 {
            continue;
        }

        let mut resources = Map::new();
        for widget in provider.widgets {
            if widget.is_spend_tile || widget.id.ends_with(".trend") {
                continue;
            }
            let Some(line) = snapshot.line(&widget.metric_label) else {
                continue;
            };
            append_resource(&mut resources, &widget.id, line);
        }

        let stale = snapshot.stale
            || Utc::now()
                .signed_duration_since(snapshot.refreshed_at)
                .num_seconds()
                >= CACHE_TTL_SECS * 2;
        output.insert(
            provider.info.id.clone(),
            json!({
                "displayName": snapshot.display_name,
                "plan": snapshot.plan,
                "fetchedAt": snapshot.refreshed_at,
                "expiresAt": snapshot.refreshed_at + Duration::seconds(CACHE_TTL_SECS),
                "stale": stale,
                "resources": resources,
            }),
        );
    }

    let mut reported_errors = HashSet::new();
    let mut error_list: Vec<_> = errors
        .iter()
        .filter(|(provider_id, _)| included.contains(provider_id.as_str()))
        .map(|(provider_id, message)| {
            reported_errors.insert(provider_id.as_str());
            json!({
                "providerId": provider_id,
                "message": message,
            })
        })
        .collect();
    for provider in providers {
        if reported_errors.contains(provider.info.id.as_str()) {
            continue;
        }
        let Some(snapshot) = provider.snapshot else {
            continue;
        };
        let message = snapshot.error_message().or_else(|| {
            snapshot
                .stale
                .then_some(snapshot.warning.as_deref())
                .flatten()
        });
        if let Some(message) = message {
            error_list.push(json!({
                "providerId": provider.info.id,
                "message": message,
            }));
        }
    }
    error_list.sort_by(|left, right| {
        left["providerId"]
            .as_str()
            .cmp(&right["providerId"].as_str())
    });

    json!({
        "schema": "multimeters.limits.v1",
        "generatedAt": Utc::now(),
        "providers": output,
        "errors": error_list,
    })
}

fn append_resource(resources: &mut Map<String, Value>, widget_id: &str, line: &MetricLine) {
    let key = resource_key(widget_id);
    match line {
        MetricLine::Progress {
            used,
            limit,
            format,
            resets_at,
            period_duration_ms,
            ..
        } if used.is_finite() && limit.is_finite() => {
            let unit = match format {
                ProgressFormat::Percent => "percent",
                ProgressFormat::Dollars => "usd",
                ProgressFormat::Count { suffix } => suffix.as_str(),
            };
            let remaining = (limit - used).max(0.0);
            let utilization = if *limit > 0.0 {
                Some((used / limit).clamp(0.0, 1.0))
            } else {
                None
            };
            let mut resource = Map::from_iter([
                ("kind".into(), json!("consumption")),
                ("unit".into(), json!(unit)),
                ("used".into(), json!(used)),
                ("limit".into(), json!(limit)),
                ("remaining".into(), json!(remaining)),
            ]);
            if let Some(utilization) = utilization {
                resource.insert("utilization".into(), json!(utilization));
            }
            if let Some(resets_at) = resets_at {
                resource.insert("resetsAt".into(), json!(resets_at));
            }
            if let Some(milliseconds) = period_duration_ms.filter(|duration| *duration > 0) {
                resource.insert("windowSeconds".into(), json!(milliseconds / 1_000));
            }
            resources.insert(key.to_string(), Value::Object(resource));
        }
        MetricLine::Values {
            values,
            expiries_at,
            ..
        } => {
            if widget_id == "codex.credits" {
                for value in values {
                    let (resource_key, unit) = match value.kind {
                        MetricKind::Count => ("credits", "credits"),
                        MetricKind::Dollars => ("creditValue", "usd"),
                        MetricKind::Percent => continue,
                    };
                    let mut resource = Map::from_iter([
                        ("kind".into(), json!("balance")),
                        ("unit".into(), json!(unit)),
                        ("available".into(), json!(value.number)),
                    ]);
                    if value.estimated {
                        resource.insert("estimated".into(), json!(true));
                    }
                    resources.insert(resource_key.into(), Value::Object(resource));
                }
                return;
            }
            let Some(value) = values.first().filter(|value| value.number.is_finite()) else {
                return;
            };
            let unit = match value.kind {
                MetricKind::Dollars => "usd",
                MetricKind::Percent => "percent",
                MetricKind::Count => value.label.as_deref().unwrap_or("count"),
            };
            let mut resource = Map::from_iter([
                ("kind".into(), json!("balance")),
                ("unit".into(), json!(unit)),
                ("available".into(), json!(value.number)),
            ]);
            if value.estimated {
                resource.insert("estimated".into(), json!(true));
            }
            if !expiries_at.is_empty() {
                resource.insert("expiresAt".into(), json!(expiries_at));
            }
            resources.insert(key.to_string(), Value::Object(resource));
        }
        _ => {}
    }
}

/// The public name a widget's reading is published under. Only widgets whose key differs
/// from their id's suffix need an arm; everything else falls through to that suffix.
fn resource_key(widget_id: &str) -> &str {
    match widget_id {
        "antigravity.geminiPro" => "geminiSession",
        "antigravity.claude" => "nonGeminiSession",
        "antigravity.claudeWeekly" => "nonGeminiWeekly",
        "claude.extra" => "extraUsage",
        "copilot.premium" => "premiumCredits",
        "copilot.extra" => "extraUsage",
        "cursor.usage" => "totalUsage",
        "cursor.auto" => "autoUsage",
        "cursor.api" => "apiUsage",
        "devin.extra" => "extraUsageBalance",
        _ => widget_id
            .split_once('.')
            .map_or(widget_id, |(_, suffix)| suffix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetricLine, ProviderInfo};

    #[test]
    fn emits_stable_camel_case_resource_contract() {
        let info = ProviderInfo {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            icon: "cursor".into(),
            links: vec![],
        };
        let widget = WidgetDescriptor {
            id: "cursor.usage".into(),
            provider_id: "cursor".into(),
            title: "Total Usage".into(),
            metric_label: "Total usage".into(),
            pinnable: true,
            is_spend_tile: false,
            default_on: true,
        };
        let snapshot = ProviderSnapshot::ok(
            &info,
            Some("Pro".into()),
            vec![MetricLine::percent("Total usage", 42.0, None, 100_000)],
        );
        let value = envelope(
            &[ProviderLimits {
                info: &info,
                widgets: &[widget],
                snapshot: Some(&snapshot),
            }],
            &HashMap::new(),
        );
        assert_eq!(value["schema"], "multimeters.limits.v1");
        assert_eq!(
            value["providers"]["cursor"]["resources"]["totalUsage"]["remaining"],
            58.0
        );
        assert!(value["providers"]["cursor"].get("fetchedAt").is_some());
        assert!(value["providers"]["cursor"]["resources"]["totalUsage"]
            .get("resetsAt")
            .is_none());
    }

    #[test]
    fn preserves_failed_refresh_staleness_and_cached_error_message() {
        let info = ProviderInfo {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            icon: "cursor".into(),
            links: vec![],
        };
        let widget = WidgetDescriptor {
            id: "cursor.usage".into(),
            provider_id: "cursor".into(),
            title: "Total Usage".into(),
            metric_label: "Total usage".into(),
            pinnable: true,
            is_spend_tile: false,
            default_on: true,
        };
        let mut snapshot = ProviderSnapshot::ok(
            &info,
            None,
            vec![MetricLine::percent("Total usage", 10.0, None, 100_000)],
        );
        snapshot.stale = true;
        snapshot.warning = Some("offline".into());
        let value = envelope(
            &[ProviderLimits {
                info: &info,
                widgets: &[widget],
                snapshot: Some(&snapshot),
            }],
            &HashMap::new(),
        );
        assert_eq!(value["providers"]["cursor"]["stale"], true);
        assert_eq!(value["errors"][0]["message"], "offline");
    }
}
