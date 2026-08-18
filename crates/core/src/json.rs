use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

pub fn obj(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    v.as_object()
}

pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn num_at(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(num)
}

pub fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

pub fn bool_at(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}

pub fn clamp_percent(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    v.clamp(0.0, 100.0)
}

pub fn cents_to_dollars(cents: f64) -> f64 {
    cents / 100.0
}

pub fn parse_iso(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|n| Utc.from_utc_datetime(&n))
        })
}

pub fn date_at(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key).and_then(as_date)
}

pub fn as_date(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_iso(s);
    }
    if let Some(n) = num(v) {
        if n > 1_000_000_000_000.0 {
            return Utc.timestamp_millis_opt(n as i64).single();
        }
        if n > 1_000_000_000.0 {
            return Utc.timestamp_opt(n as i64, 0).single();
        }
    }
    None
}

pub fn jwt_payload(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    let padded = match part.len() % 4 {
        0 => part.to_string(),
        2 => format!("{part}=="),
        3 => format!("{part}="),
        _ => part.to_string(),
    };
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, part)
        .or_else(|_| base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, &padded))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    let payload = jwt_payload(token)?;
    let exp = num_at(&payload, "exp")?;
    Utc.timestamp_opt(exp as i64, 0).single()
}

pub fn title_case(raw: &str) -> String {
    raw.split(['_', '-', ' '])
        .filter(|s| !s.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;
    use crate::models::{MetricLine, ProgressFormat};
    use serde_json::json;

    #[test]
    fn parses_iso_and_epoch() {
        assert!(parse_iso("2026-08-13T10:00:00Z").is_some());
        assert!(parse_iso("2026-08-13").is_some());
    }

    #[test]
    fn json_numbers() {
        assert_eq!(num(&json!(12)), Some(12.0));
        assert_eq!(num(&json!("3.5")), Some(3.5));
        assert_eq!(clamp_percent(140.0), 100.0);
    }

    #[test]
    fn title_cases_plan_names() {
        assert_eq!(title_case("pro_plus"), "Pro Plus");
    }

    #[test]
    fn jwt_payload_roundtrip() {
        let token = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhYmMiLCJleHAiOjk5OTk5OTk5OTl9.x";
        let v = jwt_payload(token).expect("payload");
        assert_eq!(v["sub"], "abc");
    }

    #[test]
    fn progress_line_label() {
        let line = MetricLine::percent("Session", 42.0, None, crate::models::SESSION_MS);
        assert_eq!(line.label(), "Session");
        assert!(!line.is_error());
        assert!(MetricLine::error("nope").is_error());
    }

    #[test]
    fn format_used_left() {
        let line = MetricLine::Progress {
            label: "Weekly".into(),
            used: 40.0,
            limit: 100.0,
            format: ProgressFormat::Percent,
            resets_at: None,
            period_duration_ms: None,
            color_hex: None,
        };
        assert!(format::format_line(&line, true).contains("40%"));
        assert!(format::format_line(&line, false).contains("60%"));
    }
}
