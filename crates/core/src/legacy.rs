use serde_json::{json, Map, Value};

use crate::models::{MetricLine, ProviderSnapshot};

pub fn snapshot(snapshot: &ProviderSnapshot) -> Value {
    let mut output = Map::from_iter([
        ("providerId".into(), json!(snapshot.provider_id)),
        ("displayName".into(), json!(snapshot.display_name)),
        (
            "lines".into(),
            Value::Array(snapshot.lines.iter().map(metric_line).collect()),
        ),
        ("fetchedAt".into(), json!(snapshot.refreshed_at)),
        ("stale".into(), json!(snapshot.stale)),
    ]);
    if let Some(plan) = &snapshot.plan {
        output.insert("plan".into(), json!(plan));
    }
    if let Some(warning) = &snapshot.warning {
        output.insert("warning".into(), json!(warning));
    }
    Value::Object(output)
}

fn metric_line(line: &MetricLine) -> Value {
    match line {
        MetricLine::Text {
            label,
            value,
            color_hex,
            subtitle,
        } => optional_fields(
            json!({ "type": "text", "label": label, "value": value }),
            color_hex,
            subtitle,
        ),
        MetricLine::Values {
            label,
            values,
            color_hex,
            expiries_at,
            unknown_models,
        } => {
            let mut value = json!({
                "type": "values",
                "label": label,
                "values": values,
            });
            insert_optional(
                &mut value,
                "color",
                color_hex.as_ref().map(|value| json!(value)),
            );
            if !expiries_at.is_empty() {
                value["expiriesAt"] = json!(expiries_at);
            }
            if !unknown_models.is_empty() {
                value["unknownModels"] = json!(unknown_models);
            }
            value
        }
        MetricLine::Progress {
            label,
            used,
            limit,
            format,
            resets_at,
            period_duration_ms,
            color_hex,
        } => {
            let mut value = json!({
                "type": "progress",
                "label": label,
                "used": used,
                "limit": limit,
                "format": format,
            });
            insert_optional(&mut value, "resetsAt", resets_at.map(|value| json!(value)));
            insert_optional(
                &mut value,
                "periodDurationMs",
                period_duration_ms.map(|value| json!(value)),
            );
            insert_optional(
                &mut value,
                "color",
                color_hex.as_ref().map(|value| json!(value)),
            );
            value
        }
        MetricLine::Badge {
            label,
            text,
            color_hex,
            subtitle,
        } => optional_fields(
            json!({ "type": "badge", "label": label, "text": text }),
            color_hex,
            subtitle,
        ),
        MetricLine::Chart {
            label,
            points,
            note,
        } => {
            let points: Vec<_> = points
                .iter()
                .map(|point| {
                    let mut value = json!({ "label": point.label, "value": point.value });
                    insert_optional(
                        &mut value,
                        "valueLabel",
                        point.value_label.as_ref().map(|value| json!(value)),
                    );
                    value
                })
                .collect();
            let mut value = json!({ "type": "barChart", "label": label, "points": points });
            insert_optional(&mut value, "note", note.as_ref().map(|value| json!(value)));
            value
        }
    }
}

fn optional_fields(mut value: Value, color: &Option<String>, subtitle: &Option<String>) -> Value {
    insert_optional(
        &mut value,
        "color",
        color.as_ref().map(|value| json!(value)),
    );
    insert_optional(
        &mut value,
        "subtitle",
        subtitle.as_ref().map(|value| json!(value)),
    );
    value
}

fn insert_optional(target: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        target[key] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChartPoint, ProviderInfo};

    #[test]
    fn legacy_usage_contract_is_camel_case() {
        let info = ProviderInfo {
            id: "claude".into(),
            display_name: "Claude".into(),
            icon: "claude".into(),
            links: vec![],
        };
        let snapshot = ProviderSnapshot::ok(
            &info,
            None,
            vec![MetricLine::Chart {
                label: "Usage Trend".into(),
                points: vec![ChartPoint {
                    value: 10.0,
                    label: "Aug 13".into(),
                    value_label: Some("10 tokens".into()),
                }],
                note: None,
            }],
        );
        let value = super::snapshot(&snapshot);
        assert_eq!(value["providerId"], "claude");
        assert!(value.get("provider_id").is_none());
        assert_eq!(value["lines"][0]["type"], "barChart");
        assert_eq!(value["lines"][0]["points"][0]["valueLabel"], "10 tokens");
    }
}
