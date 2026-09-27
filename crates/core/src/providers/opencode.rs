use async_trait::async_trait;
use chrono::{Datelike, TimeZone, Timelike, Utc};
use serde_json::Value;

use super::{spend_widgets, widget, Provider};
use crate::http::Http;
use crate::models::*;
use crate::paths;
use crate::spend;

pub struct OpenCodeProvider {
    info: ProviderInfo,
    widgets: Vec<WidgetDescriptor>,
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        let info = ProviderInfo {
            id: "opencode".into(),
            display_name: "OpenCode".into(),
            icon: "opencode".into(),
            links: vec![ProviderLink {
                label: "Dashboard".into(),
                url: "https://opencode.ai/auth".into(),
            }],
        };
        let mut widgets = vec![
            widget("opencode.session", "opencode", "Session", true),
            widget("opencode.weekly", "opencode", "Weekly", true),
            widget("opencode.monthly", "opencode", "Monthly", true),
        ];
        widgets.extend(spend_widgets("opencode"));
        Self { info, widgets }
    }
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn widgets(&self) -> &[WidgetDescriptor] {
        &self.widgets
    }

    async fn has_local_credentials(&self) -> bool {
        match has_go_key() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "OpenCode credentials are present but unreadable");
                return true;
            }
        }
        match spend::opencode_scan() {
            Ok(Some(scan)) => scan.has_hosted_rows,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "OpenCode footprint is present but unreadable");
                true
            }
        }
    }

    async fn refresh(&self, _http: &Http) -> ProviderSnapshot {
        let mut lines = Vec::new();
        let go = match has_go_key() {
            Ok(value) => value,
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        let scan = match spend::opencode_scan() {
            Ok(scan) => scan,
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        if !go && scan.is_none() {
            return ProviderSnapshot::err(
                &self.info,
                "OpenCode not detected. Log in with OpenCode Go or use OpenCode locally first.",
            );
        }
        let windows = match go_windows(go) {
            Ok(lines) => lines,
            Err(error) => return ProviderSnapshot::err(&self.info, &error),
        };
        let showing_go = !windows.is_empty();
        lines.extend(windows);
        if let Some(scan) = scan {
            lines.extend(scan.into_lines());
        }
        let plan = if showing_go { Some("Go".into()) } else { None };
        ProviderSnapshot::ok(&self.info, plan, lines)
    }
}

fn has_go_key() -> Result<bool, String> {
    let path = paths::opencode_data_dir().join("auth.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Couldn't read OpenCode credentials at {}: {error}",
                path.display()
            ))
        }
    };
    let v = serde_json::from_str::<Value>(&text).map_err(|error| {
        format!(
            "OpenCode credentials at {} are invalid JSON: {error}",
            path.display()
        )
    })?;
    Ok(v.get("opencode-go").is_some()
        || v.get("opencode-go.key").is_some()
        || v.as_object()
            .map(|o| o.keys().any(|k| k.contains("opencode-go")))
            .unwrap_or(false))
}

fn go_windows(has_key: bool) -> Result<Vec<MetricLine>, String> {
    let (costs, anchor_ms) = scan_go_costs()?;
    if !has_key && costs.is_empty() {
        return Ok(vec![]);
    }
    let now = Utc::now();
    let now_ms = now.timestamp_millis();
    let session_start = now_ms - SESSION_MS;
    let session: f64 = costs
        .iter()
        .filter(|(t, _)| *t >= session_start && *t < now_ms)
        .map(|(_, c)| c)
        .sum();
    let oldest_session = costs
        .iter()
        .filter(|(t, _)| *t >= session_start && *t < now_ms)
        .map(|(t, _)| *t)
        .min();
    let session_reset = oldest_session.unwrap_or(now_ms) + SESSION_MS;

    let week_start = start_of_utc_week(now).timestamp_millis();
    let week_end = week_start + WEEK_MS;
    let weekly: f64 = costs
        .iter()
        .filter(|(t, _)| *t >= week_start && *t < week_end)
        .map(|(_, c)| c)
        .sum();

    let (month_start, month_end) = anchored_month_bounds(now_ms, anchor_ms);
    let monthly: f64 = costs
        .iter()
        .filter(|(t, _)| *t >= month_start && *t < month_end)
        .map(|(_, c)| c)
        .sum();
    let month_period = (month_end - month_start).max(1);

    Ok(vec![
        MetricLine::dollars_progress(
            "Session",
            snap_cost(session),
            12.0,
            Utc.timestamp_millis_opt(session_reset).single(),
            SESSION_MS,
        ),
        MetricLine::dollars_progress(
            "Weekly",
            snap_cost(weekly),
            30.0,
            Utc.timestamp_millis_opt(week_end).single(),
            WEEK_MS,
        ),
        MetricLine::dollars_progress(
            "Monthly",
            snap_cost(monthly),
            60.0,
            Utc.timestamp_millis_opt(month_end).single(),
            month_period,
        ),
    ])
}

fn snap_cost(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn start_of_utc_week(now: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    let weekday = now.weekday().num_days_from_monday() as i64;
    (now.date_naive() - chrono::Duration::days(weekday))
        .and_hms_opt(0, 0, 0)
        .map(|n| Utc.from_utc_datetime(&n))
        .unwrap_or(now)
}

fn anchored_month_bounds(now_ms: i64, anchor_ms: Option<i64>) -> (i64, i64) {
    let Some(anchor_ms) = anchor_ms else {
        let now = Utc
            .timestamp_millis_opt(now_ms)
            .single()
            .unwrap_or_else(Utc::now);
        let start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now);
        let end = next_month(start);
        return (start.timestamp_millis(), end.timestamp_millis());
    };
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(Utc::now);
    let anchor = Utc.timestamp_millis_opt(anchor_ms).single().unwrap_or(now);
    let mut year = now.year();
    let mut month = now.month();
    let mut start = anchored_month_start(year, month, anchor);
    if start.timestamp_millis() > now_ms {
        let (y, m) = shift_month(year, month, -1);
        year = y;
        month = m;
        start = anchored_month_start(year, month, anchor);
    }
    let (ny, nm) = shift_month(year, month, 1);
    let end = anchored_month_start(ny, nm, anchor);
    (start.timestamp_millis(), end.timestamp_millis())
}

fn anchored_month_start(
    year: i32,
    month: u32,
    anchor: chrono::DateTime<Utc>,
) -> chrono::DateTime<Utc> {
    let dim = days_in_month(year, month);
    let day = anchor.day().min(dim);
    Utc.with_ymd_and_hms(
        year,
        month,
        day,
        anchor.hour(),
        anchor.minute(),
        anchor.second(),
    )
    .single()
    .unwrap_or(anchor)
}

fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = year * 12 + (month as i32 - 1) + delta;
    let normalized = ((total % 12) + 12) % 12;
    (total.div_euclid(12), (normalized + 1) as u32)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = shift_month(year, month, 1);
    let start = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single();
    let end = Utc.with_ymd_and_hms(ny, nm, 1, 0, 0, 0).single();
    match (start, end) {
        (Some(s), Some(e)) => (e - s).num_days().max(28) as u32,
        _ => 28,
    }
}

fn next_month(start: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    let (y, m) = shift_month(start.year(), start.month(), 1);
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0)
        .single()
        .unwrap_or(start)
}

type TimedCost = (i64, f64);
type GoCostScan = (Vec<TimedCost>, Option<i64>);

fn scan_go_costs() -> Result<GoCostScan, String> {
    let mut costs = Vec::new();
    let mut anchor: Option<i64> = None;
    let dir = paths::opencode_data_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((costs, anchor)),
        Err(error) => {
            return Err(format!(
                "Couldn't read OpenCode's local data directory: {error}"
            ))
        }
    };
    let mut databases = 0;
    let mut readable = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("opencode") || !name.ends_with(".db") {
            continue;
        }
        databases += 1;
        match scan_go_database(&entry.path()) {
            Ok(rows) => {
                readable += 1;
                for row in rows {
                    let ms = if row.0 > 1_000_000_000_000 {
                        row.0
                    } else {
                        row.0 * 1000
                    };
                    costs.push((ms, row.1.max(0.0)));
                    anchor = Some(anchor.map_or(ms, |a| a.min(ms)));
                }
            }
            Err(error) => {
                tracing::warn!(path = %entry.path().display(), %error, "could not scan OpenCode Go quota usage");
            }
        }
    }
    if databases > 0 && readable == 0 {
        return Err("Couldn't read OpenCode's local database. Quit OpenCode and refresh, or check the data directory's permissions.".into());
    }
    Ok((costs, anchor))
}

fn scan_go_database(path: &std::path::Path) -> rusqlite::Result<Vec<TimedCost>> {
    let connection = rusqlite::Connection::open(path)?;
    let mut statement = connection.prepare(
        "SELECT time_created, json_extract(data,'$.cost') FROM message
         WHERE json_valid(data)
           AND json_extract(data,'$.role') = 'assistant'
           AND json_extract(data,'$.providerID') = 'opencode-go'
           AND json_type(data,'$.cost') IN ('integer','real')",
    )?;
    let rows = statement.query_map([], |row| {
        let timestamp = row
            .get::<_, i64>(0)
            .or_else(|_| row.get::<_, f64>(0).map(|value| value as i64))?;
        let cost = row
            .get::<_, f64>(1)
            .or_else(|_| row.get::<_, i64>(1).map(|value| value as f64))?;
        Ok((timestamp, cost))
    })?;
    rows.collect()
}
