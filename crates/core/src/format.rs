use chrono::{DateTime, Local, Utc};

use crate::models::{MetricKind, MetricLine, MetricValue, ProgressFormat};

pub fn compact_number(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000_000.0 {
        format!("{:.1}B", n / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if abs >= 10_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else if abs >= 100.0 {
        format!("{:.0}", n)
    } else if abs >= 10.0 {
        format!("{:.1}", n)
    } else {
        format!("{:.2}", n)
    }
}

pub fn dollars(n: f64) -> String {
    if n.abs() >= 1000.0 {
        format!("${}", compact_number(n))
    } else {
        format!("${:.2}", n)
    }
}

pub fn percent(n: f64) -> String {
    format!("{:.0}%", n.round())
}

pub fn format_value(v: &MetricValue) -> String {
    match v.kind {
        MetricKind::Dollars => dollars(v.number),
        MetricKind::Percent => percent(v.number),
        MetricKind::Count => {
            let n = compact_number(v.number);
            match &v.label {
                Some(label) => format!("{n} {label}"),
                None => n,
            }
        }
    }
}

pub fn format_values(values: &[MetricValue]) -> String {
    values
        .iter()
        .map(format_value)
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn format_line(line: &MetricLine, used_mode: bool) -> String {
    match line {
        MetricLine::Progress {
            used,
            limit,
            format,
            ..
        } => match format {
            ProgressFormat::Percent => {
                let shown = if used_mode {
                    *used
                } else {
                    (100.0 - used).clamp(0.0, 100.0)
                };
                let word = if used_mode { "used" } else { "left" };
                format!("{} {word}", percent(shown))
            }
            ProgressFormat::Dollars => {
                if used_mode {
                    format!("{} of {}", dollars(*used), dollars(*limit))
                } else {
                    format!("{} left", dollars((limit - used).max(0.0)))
                }
            }
            ProgressFormat::Count { suffix } => {
                if used_mode {
                    format!("{:.0} / {:.0} {suffix}", used, limit)
                } else {
                    format!("{:.0} {suffix} left", (limit - used).max(0.0))
                }
            }
        },
        MetricLine::Values { values, .. } => format_values(values),
        MetricLine::Badge { text, .. } => text.clone(),
        MetricLine::Text { value, .. } => value.clone(),
        MetricLine::Chart { .. } => "trend".into(),
    }
}

pub fn used_ratio(line: &MetricLine) -> Option<f64> {
    match line {
        MetricLine::Progress { used, limit, .. } if *limit > 0.0 => {
            Some((*used / *limit).clamp(0.0, 1.0))
        }
        _ => None,
    }
}

pub fn format_reset(at: DateTime<Utc>, countdown: bool) -> String {
    let now = Utc::now();
    if countdown {
        let secs = (at - now).num_seconds();
        if secs <= 0 {
            return "Resets soon".into();
        }
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if h >= 48 {
            format!("Resets in {}d {}h", h / 24, h % 24)
        } else if h >= 1 {
            format!("Resets in {h}h {m}m")
        } else {
            format!("Resets in {m}m")
        }
    } else {
        let local = at.with_timezone(&Local);
        let today = Local::now().date_naive();
        if local.date_naive() == today {
            format!("Resets today at {}", local.format("%I:%M %p"))
        } else {
            format!("Resets {}", local.format("%b %d at %I:%M %p"))
        }
    }
}

/// Pace color: blue on track, yellow cutting it close, red will run out.
pub fn pace_color(
    used: f64,
    limit: f64,
    resets_at: Option<DateTime<Utc>>,
    period_ms: Option<i64>,
) -> &'static str {
    let ratio = if limit > 0.0 { used / limit } else { 0.0 };
    let remaining = (1.0 - ratio).clamp(0.0, 1.0);
    if remaining <= 0.005 {
        return "#EF4444";
    }
    if let (Some(reset), Some(period)) = (resets_at, period_ms) {
        let now = Utc::now();
        let left_ms = (reset - now).num_milliseconds().max(0) as f64;
        let elapsed = (period as f64 - left_ms).max(1.0);
        if elapsed / period as f64 > 0.08 {
            let projected = ratio / (elapsed / period as f64);
            if projected >= 1.0 {
                return "#EF4444";
            }
            if projected >= 0.90 {
                return "#F59E0B";
            }
            return "#3B82F6";
        }
    }
    if remaining <= 0.10 {
        "#EF4444"
    } else if ratio >= 0.80 {
        "#F59E0B"
    } else {
        "#3B82F6"
    }
}
