use chrono::{Local, NaiveDate, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use walkdir::WalkDir;

use crate::json::{num, num_at, str_at};
use crate::models::{ChartPoint, MetricLine};
use crate::paths;

#[derive(Default, Clone)]
pub struct DaySpend {
    pub dollars: f64,
    pub tokens: f64,
    pub unknown_models: BTreeSet<String>,
}

pub fn claude_lines() -> Vec<MetricLine> {
    period_lines(scan_claude())
}

pub fn codex_lines() -> Vec<MetricLine> {
    period_lines(scan_codex())
}

pub fn grok_lines() -> Vec<MetricLine> {
    period_lines(scan_grok())
}

pub fn opencode_scan() -> Result<Option<OpenCodeScan>, String> {
    scan_opencode()
}

pub fn period_lines(days: BTreeMap<NaiveDate, DaySpend>) -> Vec<MetricLine> {
    period_lines_with(days, true, "Local logs · bundled model pricing")
}

fn period_lines_with(
    days: BTreeMap<NaiveDate, DaySpend>,
    estimated: bool,
    note: &str,
) -> Vec<MetricLine> {
    let today = Local::now().date_naive();
    let yesterday = today - chrono::Duration::days(1);
    let mut last30 = DaySpend::default();
    let mut points = Vec::new();
    for i in (0..30).rev() {
        let day = today - chrono::Duration::days(i);
        let spend = days.get(&day).cloned().unwrap_or_default();
        last30.dollars += spend.dollars;
        last30.tokens += spend.tokens;
        last30.unknown_models.extend(spend.unknown_models.clone());
        points.push(ChartPoint {
            value: spend.tokens,
            label: day.format("%b %d").to_string(),
            value_label: Some(format!(
                "{} tokens",
                crate::format::compact_number(spend.tokens)
            )),
        });
    }
    let t = days.get(&today).cloned().unwrap_or_default();
    let y = days.get(&yesterday).cloned().unwrap_or_default();
    vec![
        spend_or_none("Today", t, estimated),
        spend_or_none("Yesterday", y, estimated),
        spend_or_none("Last 30 Days", last30, estimated),
        MetricLine::Chart {
            label: "Usage Trend".into(),
            points,
            note: Some(note.into()),
        },
    ]
}

fn spend_or_none(label: &str, d: DaySpend, estimated: bool) -> MetricLine {
    if d.dollars == 0.0 && d.tokens == 0.0 && d.unknown_models.is_empty() {
        MetricLine::badge(label, "No data", Some("#A3A3A3"))
    } else {
        let mut line = MetricLine::spend(label, d.dollars, d.tokens, estimated);
        if let MetricLine::Values { unknown_models, .. } = &mut line {
            unknown_models.extend(d.unknown_models);
        }
        line
    }
}

fn scan_claude() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let mut files = Vec::new();
    for root in claude_project_roots() {
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if is_recent_file(path) {
                files.push(path.to_path_buf());
            }
        }
    }
    files.sort();
    let mut entries = Vec::new();
    for path in files {
        read_claude_jsonl(&path, &mut entries);
    }
    let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
    for entry in dedup_claude(entries) {
        if entry.day >= cutoff {
            merge_day(days.entry(entry.day).or_default(), entry.spend);
        }
    }
    merge_days(&mut days, scan_pi("claude"));
    days
}

fn claude_project_roots() -> Vec<std::path::PathBuf> {
    let configured = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(paths::expand_home)
                .map(|path| {
                    if path.file_name().and_then(|name| name.to_str()) == Some("projects") {
                        path
                    } else {
                        path.join("projects")
                    }
                })
                .collect::<Vec<_>>()
        });
    let roots = configured.unwrap_or_else(|| {
        let mut roots = Vec::new();
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                roots.push(paths::expand_home(&xdg).join("claude/projects"));
            }
        } else {
            roots.push(paths::home().join(".config/claude/projects"));
        }
        roots.push(paths::home().join(".claude/projects"));
        roots
    });
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|root| root.exists() && seen.insert(root.clone()))
        .collect()
}

fn scan_pi(card_id: &str) -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let root = paths::pi_sessions();
    if !root.exists() {
        return days;
    }
    let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
    let mut seen = HashSet::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            || !is_recent_file(path)
        {
            continue;
        }
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let Some((id, day, spend)) = parse_pi_event(&value, card_id) else {
                continue;
            };
            if day < cutoff || id.as_ref().is_some_and(|id| !seen.insert(id.clone())) {
                continue;
            }
            merge_day(days.entry(day).or_default(), spend);
        }
    }
    days
}

fn merge_days(target: &mut BTreeMap<NaiveDate, DaySpend>, source: BTreeMap<NaiveDate, DaySpend>) {
    for (day, spend) in source {
        merge_day(target.entry(day).or_default(), spend);
    }
}

fn merge_day(target: &mut DaySpend, source: DaySpend) {
    target.dollars += source.dollars;
    target.tokens += source.tokens;
    target.unknown_models.extend(source.unknown_models);
}

fn parse_pi_event(v: &Value, card_id: &str) -> Option<(Option<String>, NaiveDate, DaySpend)> {
    if str_at(v, "type") != Some("message") {
        return None;
    }
    let day = event_day(v)?;
    let message = v.get("message")?;
    if str_at(message, "role") != Some("assistant") {
        return None;
    }
    let mapped_card = match str_at(message, "provider")? {
        "anthropic" | "claude-agent-sdk" => "claude",
        "openai-codex" => "codex",
        "cursor" => "cursor",
        "zai" | "zhipu" => "zai",
        "google-antigravity" => "antigravity",
        "github-copilot" => "copilot",
        _ => return None,
    };
    if mapped_card != card_id {
        return None;
    }
    let usage = message.get("usage")?;
    let cache_write = num_at(usage, "cacheWrite").unwrap_or(0.0).max(0.0);
    let cache_write_1h = num_at(usage, "cacheWrite1h")
        .unwrap_or(0.0)
        .clamp(0.0, cache_write);
    let breakdown = crate::pricing::TokenBreakdown {
        input: num_at(usage, "input").unwrap_or(0.0).max(0.0),
        cache_write_5m: (cache_write - cache_write_1h).max(0.0),
        cache_write_1h,
        cache_read: num_at(usage, "cacheRead").unwrap_or(0.0).max(0.0),
        output: num_at(usage, "output").unwrap_or(0.0).max(0.0),
        fast: false,
    };
    let tokens = num_at(usage, "totalTokens")
        .filter(|tokens| *tokens > 0.0)
        .unwrap_or_else(|| breakdown.total());
    if tokens <= 0.0 {
        return None;
    }
    let model = str_at(message, "model")
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let carried_cost = usage
        .pointer("/cost/total")
        .and_then(num)
        .filter(|cost| *cost > 0.0);
    let mut spend = DaySpend::default();
    if let Some(cost) = carried_cost
        .or_else(|| model.and_then(|model| crate::pricing::estimate_cost(model, breakdown)))
    {
        spend.dollars = cost;
        spend.tokens = tokens;
    } else if let Some(model) = model {
        spend.unknown_models.insert(model.to_string());
    }
    Some((str_at(v, "id").map(ToOwned::to_owned), day, spend))
}

fn scan_codex() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let mut seen_events = HashSet::new();
    let home = paths::codex_home();
    for dir in ["sessions", "archived_sessions"] {
        let root = home.join(dir);
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if is_recent_file(path) {
                ingest_codex_jsonl(path, &mut days, &mut seen_events);
            }
        }
    }
    merge_days(&mut days, scan_pi("codex"));
    days
}

fn scan_grok() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let path = paths::grok_home().join("logs/unified.jsonl");
    let Ok(file) = std::fs::File::open(path) else {
        return days;
    };
    let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
    let mut model_by_pid = HashMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("inference_done") && !line.contains("model") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let pid = num_at(&value, "pid").map(|pid| pid as i64);
        if let Some(model) = grok_model_event(&value) {
            if let Some(pid) = pid {
                model_by_pid.insert(pid, model.to_string());
            }
            continue;
        }
        let model = pid.and_then(|pid| model_by_pid.get(&pid).map(String::as_str));
        if let Some((day, spend)) = parse_grok_event(&value, model) {
            if day >= cutoff {
                merge_day(days.entry(day).or_default(), spend);
            }
        }
    }
    days
}

pub struct OpenCodeScan {
    pub days: BTreeMap<NaiveDate, DaySpend>,
    pub has_hosted_rows: bool,
}

impl OpenCodeScan {
    pub fn into_lines(self) -> Vec<MetricLine> {
        period_lines_with(self.days, false, "Local OpenCode logs · measured cost")
    }
}

fn scan_opencode() -> Result<Option<OpenCodeScan>, String> {
    let mut days = BTreeMap::new();
    let dir = paths::opencode_data_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return if dir.exists() {
            Err("Couldn't read OpenCode's local data directory.".into())
        } else {
            Ok(None)
        };
    };
    let mut databases = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("opencode") && name.ends_with(".db") {
            databases.push(entry.path());
        }
    }
    databases.sort();
    if databases.is_empty() {
        return Ok(None);
    }
    let cutoff_ms = (Utc::now() - chrono::Duration::days(33)).timestamp_millis();
    let mut readable = 0;
    let mut has_hosted_rows = false;
    for database in &databases {
        match scan_opencode_database(database, cutoff_ms, &mut days) {
            Ok(found) => {
                readable += 1;
                has_hosted_rows |= found;
            }
            Err(error) => {
                tracing::warn!(path = %database.display(), %error, "could not scan OpenCode usage database");
            }
        }
    }
    if readable == 0 {
        return Err("Couldn't read OpenCode's local database. Quit OpenCode and refresh, or check the data directory's permissions.".into());
    }
    Ok(Some(OpenCodeScan {
        days,
        has_hosted_rows,
    }))
}

fn scan_opencode_database(
    path: &std::path::Path,
    cutoff_ms: i64,
    days: &mut BTreeMap<NaiveDate, DaySpend>,
) -> rusqlite::Result<bool> {
    let connection = rusqlite::Connection::open(path)?;
    let mut statement = connection.prepare(
        "SELECT time_created,
                json_extract(data,'$.cost'),
                COALESCE(json_extract(data,'$.tokens.total'),0)
         FROM message
         WHERE time_created >= ?1
           AND json_valid(data)
           AND json_extract(data,'$.role') = 'assistant'
           AND json_extract(data,'$.providerID') IN ('opencode-go','opencode')
           AND json_type(data,'$.cost') IN ('integer','real')",
    )?;
    let rows = statement.query_map([cutoff_ms], |row| {
        let timestamp = row
            .get::<_, i64>(0)
            .or_else(|_| row.get::<_, f64>(0).map(|value| value as i64))?;
        let cost = row
            .get::<_, f64>(1)
            .or_else(|_| row.get::<_, i64>(1).map(|value| value as f64))?;
        let tokens = row
            .get::<_, f64>(2)
            .or_else(|_| row.get::<_, i64>(2).map(|value| value as f64))?;
        Ok((timestamp, cost, tokens))
    })?;
    let mut found = false;
    for row in rows {
        let (timestamp, cost, tokens) = row?;
        let timestamp = if timestamp > 1_000_000_000_000 {
            timestamp
        } else {
            timestamp * 1_000
        };
        let Some(day) = Utc
            .timestamp_millis_opt(timestamp)
            .single()
            .map(|date| date.with_timezone(&Local).date_naive())
        else {
            continue;
        };
        found = true;
        let spend = days.entry(day).or_default();
        spend.dollars += cost.max(0.0);
        spend.tokens += tokens.max(0.0);
    }
    Ok(found)
}

use chrono::TimeZone;

struct ClaudeEntry {
    message_id: Option<String>,
    request_id: Option<String>,
    is_sidechain: bool,
    has_speed: bool,
    reported_tokens: f64,
    day: NaiveDate,
    spend: DaySpend,
}

fn read_claude_jsonl(path: &std::path::Path, entries: &mut Vec<ClaudeEntry>) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some((day, spend)) = parse_claude_event(&v) {
            let message = v.get("message").unwrap_or(&v);
            entries.push(ClaudeEntry {
                message_id: str_at(message, "id")
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned),
                request_id: str_at(&v, "requestId")
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned),
                is_sidechain: crate::json::bool_at(&v, "isSidechain").unwrap_or(false),
                has_speed: message
                    .get("usage")
                    .or_else(|| v.get("usage"))
                    .is_some_and(|usage| usage.get("speed").is_some()),
                reported_tokens: claude_reported_tokens(&v),
                day,
                spend,
            });
        }
    }
}

fn dedup_claude(entries: Vec<ClaudeEntry>) -> Vec<ClaudeEntry> {
    let mut deduped: Vec<ClaudeEntry> = Vec::new();
    let mut exact: HashMap<(String, Option<String>), usize> = HashMap::new();
    let mut by_message: HashMap<String, Vec<usize>> = HashMap::new();
    for entry in entries {
        let Some(message_id) = entry.message_id.clone() else {
            deduped.push(entry);
            continue;
        };
        let key = (message_id.clone(), entry.request_id.clone());
        let collision = exact.get(&key).copied().or_else(|| {
            by_message.get(&message_id).and_then(|indices| {
                indices
                    .iter()
                    .copied()
                    .find(|index| entry.is_sidechain || deduped[*index].is_sidechain)
            })
        });
        if let Some(index) = collision {
            if claude_entry_should_replace(&entry, &deduped[index]) {
                if let Some(old_id) = &deduped[index].message_id {
                    exact.remove(&(old_id.clone(), deduped[index].request_id.clone()));
                }
                deduped[index] = entry;
                exact.insert(key, index);
            }
            continue;
        }
        let index = deduped.len();
        deduped.push(entry);
        exact.insert(key, index);
        by_message.entry(message_id).or_default().push(index);
    }
    deduped
}

fn claude_entry_should_replace(candidate: &ClaudeEntry, existing: &ClaudeEntry) -> bool {
    if candidate.is_sidechain != existing.is_sidechain {
        return existing.is_sidechain;
    }
    if candidate.reported_tokens != existing.reported_tokens {
        return candidate.reported_tokens > existing.reported_tokens;
    }
    candidate.has_speed && !existing.has_speed
}

fn claude_reported_tokens(value: &Value) -> f64 {
    let message = value.get("message").unwrap_or(value);
    let Some(usage) = message.get("usage").or_else(|| value.get("usage")) else {
        return 0.0;
    };
    let creation = usage.get("cache_creation");
    num_at(usage, "input_tokens").unwrap_or(0.0).max(0.0)
        + num_at(usage, "output_tokens").unwrap_or(0.0).max(0.0)
        + creation.map_or_else(
            || {
                num_at(usage, "cache_creation_input_tokens")
                    .unwrap_or(0.0)
                    .max(0.0)
            },
            |cache| {
                num_at(cache, "ephemeral_5m_input_tokens")
                    .unwrap_or(0.0)
                    .max(0.0)
                    + num_at(cache, "ephemeral_1h_input_tokens")
                        .unwrap_or(0.0)
                        .max(0.0)
            },
        )
        + num_at(usage, "cache_read_input_tokens")
            .unwrap_or(0.0)
            .max(0.0)
}

fn is_recent_file(path: &std::path::Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .is_none_or(|age| age <= std::time::Duration::from_secs(35 * 24 * 60 * 60))
}

fn event_day(v: &Value) -> Option<NaiveDate> {
    let ts = str_at(v, "timestamp")
        .or_else(|| str_at(v, "time"))
        .or_else(|| str_at(v, "ts"))
        .or_else(|| str_at(v, "created_at"))
        .or_else(|| v.pointer("/message/timestamp").and_then(|x| x.as_str()));
    if let Some(ts) = ts {
        return crate::json::parse_iso(ts).map(|d| d.with_timezone(&Local).date_naive());
    }
    if let Some(n) = num_at(v, "timestamp").or_else(|| num_at(v, "ts")) {
        let ms = if n > 1_000_000_000_000.0 {
            n as i64
        } else {
            (n * 1000.0) as i64
        };
        return Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(|d| d.with_timezone(&Local).date_naive());
    }
    None
}

fn parse_claude_event(v: &Value) -> Option<(NaiveDate, DaySpend)> {
    let day = event_day(v)?;
    let message = v.get("message").unwrap_or(v);
    let usage = message.get("usage").or_else(|| v.get("usage"))?;
    let input = num_at(usage, "input_tokens")?.max(0.0);
    let output = num_at(usage, "output_tokens")?.max(0.0);
    let speed = str_at(usage, "speed");
    if speed.is_some_and(|speed| !matches!(speed, "fast" | "standard")) {
        return None;
    }
    let (cache_write_5m, cache_write_1h) = usage
        .get("cache_creation")
        .map(|cache| {
            (
                num_at(cache, "ephemeral_5m_input_tokens")
                    .unwrap_or(0.0)
                    .max(0.0),
                num_at(cache, "ephemeral_1h_input_tokens")
                    .unwrap_or(0.0)
                    .max(0.0),
            )
        })
        .unwrap_or_else(|| {
            (
                num_at(usage, "cache_creation_input_tokens")
                    .unwrap_or(0.0)
                    .max(0.0),
                0.0,
            )
        });
    let breakdown = crate::pricing::TokenBreakdown {
        input,
        cache_write_5m,
        cache_write_1h,
        cache_read: num_at(usage, "cache_read_input_tokens")
            .unwrap_or(0.0)
            .max(0.0),
        output,
        fast: speed == Some("fast"),
    };
    let tokens = breakdown.total();
    if tokens <= 0.0 {
        return None;
    }
    let carried_cost = num_at(v, "costUSD")
        .or_else(|| num_at(v, "cost"))
        .or_else(|| message.get("costUSD").and_then(num));
    let model = str_at(message, "model")
        .map(str::trim)
        .filter(|model| !model.is_empty() && *model != "<synthetic>");
    let mut spend = DaySpend::default();
    if let Some(cost) = carried_cost
        .or_else(|| model.and_then(|model| crate::pricing::estimate_cost(model, breakdown)))
    {
        spend.dollars = cost.max(0.0);
        spend.tokens = tokens;
    } else if let Some(model) = model {
        spend.unknown_models.insert(model.to_string());
    }
    Some((day, spend))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CodexUsage {
    input: f64,
    cached: f64,
    output: f64,
    reasoning: f64,
    total: f64,
}

impl CodexUsage {
    fn from_json(value: &Value) -> Self {
        let input = num_at(value, "input_tokens")
            .or_else(|| num_at(value, "prompt_tokens"))
            .or_else(|| num_at(value, "input"))
            .unwrap_or(0.0)
            .max(0.0);
        let output = num_at(value, "output_tokens")
            .or_else(|| num_at(value, "completion_tokens"))
            .or_else(|| num_at(value, "output"))
            .unwrap_or(0.0)
            .max(0.0);
        let cached = num_at(value, "cached_input_tokens")
            .or_else(|| num_at(value, "cache_read_input_tokens"))
            .or_else(|| num_at(value, "cached_tokens"))
            .unwrap_or(0.0)
            .clamp(0.0, input);
        let reasoning = num_at(value, "reasoning_output_tokens")
            .or_else(|| num_at(value, "reasoning_tokens"))
            .unwrap_or(0.0)
            .max(0.0);
        let recomputed = input + output + reasoning;
        let total = num_at(value, "total_tokens")
            .filter(|total| *total > 0.0)
            .unwrap_or(recomputed)
            .max(0.0);
        Self {
            input,
            cached,
            output,
            reasoning,
            total,
        }
    }

    fn subtracting(self, previous: Option<Self>) -> Self {
        let previous = previous.unwrap_or_default();
        Self {
            input: (self.input - previous.input).max(0.0),
            cached: (self.cached - previous.cached).max(0.0),
            output: (self.output - previous.output).max(0.0),
            reasoning: (self.reasoning - previous.reasoning).max(0.0),
            total: (self.total - previous.total).max(0.0),
        }
    }
}

#[derive(Default)]
struct CodexState {
    previous: Option<CodexUsage>,
    model: Option<String>,
    fast: bool,
}

fn ingest_codex_jsonl(
    path: &std::path::Path,
    days: &mut BTreeMap<NaiveDate, DaySpend>,
    seen_events: &mut HashSet<String>,
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
    let mut state = CodexState::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some((day, spend)) = parse_codex_event(&value, &mut state) {
            if day < cutoff {
                continue;
            }
            if !seen_events.insert(codex_event_key(&value, &state)) {
                continue;
            }
            let total = days.entry(day).or_default();
            total.dollars += spend.dollars;
            total.tokens += spend.tokens;
            total.unknown_models.extend(spend.unknown_models);
        }
    }
}

fn codex_event_key(value: &Value, state: &CodexState) -> String {
    let timestamp = value
        .get("timestamp")
        .or_else(|| value.get("time"))
        .or_else(|| value.get("ts"))
        .map(Value::to_string)
        .unwrap_or_default();
    let usage = value
        .pointer("/payload/info/last_token_usage")
        .or_else(|| value.pointer("/payload/info/total_token_usage"))
        .or_else(|| value.pointer("/payload/usage"))
        .or_else(|| value.get("usage"))
        .or_else(|| value.pointer("/event/usage"))
        .map(Value::to_string)
        .unwrap_or_default();
    format!(
        "{timestamp}|{}|{}|{usage}",
        state.model.as_deref().unwrap_or_default(),
        state.fast
    )
}

fn parse_codex_event(v: &Value, state: &mut CodexState) -> Option<(NaiveDate, DaySpend)> {
    let payload = v.get("payload").unwrap_or(v);
    if let Some(model) = model_name(payload) {
        state.model = Some(model.to_string());
    }
    if str_at(v, "type") == Some("thread_settings_applied")
        || str_at(payload, "type") == Some("thread_settings_applied")
    {
        let tier = payload
            .pointer("/thread_settings/service_tier")
            .and_then(Value::as_str)
            .or_else(|| str_at(payload, "service_tier"));
        state.fast = matches!(tier, Some("fast" | "priority"));
    }
    let day = event_day(v)?;
    let info = v.pointer("/payload/info");
    let totals = info
        .and_then(|value| value.get("total_token_usage"))
        .map(CodexUsage::from_json);
    if totals.is_some() && totals == state.previous {
        return None;
    }
    let usage = info
        .and_then(|value| value.get("last_token_usage"))
        .map(CodexUsage::from_json)
        .or_else(|| totals.map(|total| total.subtracting(state.previous)))
        .or_else(|| {
            v.pointer("/payload/usage")
                .or_else(|| v.get("usage"))
                .or_else(|| v.pointer("/event/usage"))
                .map(CodexUsage::from_json)
        })?;
    if let Some(totals) = totals {
        state.previous = Some(totals);
    }
    let tokens = usage.total;
    if tokens <= 0.0 {
        return None;
    }
    let model = model_name(payload)
        .or_else(|| info.and_then(model_name))
        .map(ToOwned::to_owned)
        .or_else(|| state.model.clone())
        .unwrap_or_else(|| "gpt-5".into());
    state.model = Some(model.clone());
    let cached = usage.cached.min(usage.input);
    let breakdown = crate::pricing::TokenBreakdown {
        input: (usage.input - cached).max(0.0),
        cache_read: cached,
        output: usage.output,
        fast: state.fast,
        ..crate::pricing::TokenBreakdown::default()
    };
    let mut spend = DaySpend::default();
    if let Some(cost) =
        num_at(v, "cost_usd").or_else(|| crate::pricing::estimate_cost(&model, breakdown))
    {
        spend.dollars = cost.max(0.0);
        spend.tokens = tokens;
    } else {
        spend.unknown_models.insert(model);
    }
    Some((day, spend))
}

fn model_name(value: &Value) -> Option<&str> {
    str_at(value, "model")
        .or_else(|| str_at(value, "model_name"))
        .or_else(|| value.pointer("/metadata/model").and_then(Value::as_str))
        .map(str::trim)
        .filter(|model| !model.is_empty())
}

fn parse_grok_event(v: &Value, model: Option<&str>) -> Option<(NaiveDate, DaySpend)> {
    let day = event_day(v)?;
    let usage = v.get("ctx").unwrap_or(v);
    if str_at(v, "msg") != Some("shell.turn.inference_done") {
        return None;
    }
    let prompt = num_at(usage, "prompt_tokens")
        .or_else(|| num_at(usage, "input_tokens"))?
        .max(0.0);
    let cached = num_at(usage, "cached_prompt_tokens")
        .unwrap_or(0.0)
        .clamp(0.0, prompt);
    let output = num_at(usage, "completion_tokens")
        .or_else(|| num_at(usage, "output_tokens"))
        .unwrap_or(0.0)
        .max(0.0)
        + num_at(usage, "reasoning_tokens").unwrap_or(0.0).max(0.0);
    let tokens = prompt + output;
    if tokens <= 0.0 {
        return None;
    }
    let model = model
        .or_else(|| str_at(usage, "model"))
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let breakdown = crate::pricing::TokenBreakdown {
        input: prompt - cached,
        cache_read: cached,
        output,
        ..crate::pricing::TokenBreakdown::default()
    };
    let mut spend = DaySpend::default();
    if let Some(cost) = num_at(v, "cost")
        .or_else(|| num_at(usage, "cost"))
        .or_else(|| model.and_then(|model| crate::pricing::estimate_cost(model, breakdown)))
    {
        spend.dollars = cost.max(0.0);
        spend.tokens = tokens;
    } else if let Some(model) = model {
        spend.unknown_models.insert(model.to_string());
    }
    Some((day, spend))
}

fn grok_model_event(v: &Value) -> Option<&str> {
    let ctx = v.get("ctx")?;
    let model = match str_at(v, "msg")? {
        "model changed" => str_at(ctx, "model"),
        "model catalog: notifying clients" => str_at(ctx, "current_model_id"),
        "backend_search: model switch" => str_at(ctx, "model")
            .or_else(|| str_at(ctx, "current_model_id"))
            .or_else(|| str_at(ctx, "model_id")),
        "subagent model resolved" => str_at(ctx, "model_id").or_else(|| str_at(ctx, "model")),
        _ => None,
    }?;
    let model = model.trim();
    (!model.is_empty()).then_some(model)
}

pub fn spend_from_lines(lines: &[MetricLine], label: &str) -> Option<(f64, f64)> {
    let line = lines.iter().find(|l| l.label() == label)?;
    match line {
        MetricLine::Values { values, .. } => {
            let dollars = values
                .iter()
                .find(|v| matches!(v.kind, crate::models::MetricKind::Dollars))
                .map(|v| v.number)
                .unwrap_or(0.0);
            let tokens = values
                .iter()
                .find(|v| matches!(v.kind, crate::models::MetricKind::Count))
                .map(|v| v.number)
                .unwrap_or(0.0);
            if dollars == 0.0 && tokens == 0.0 {
                None
            } else {
                Some((dollars, tokens))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_uses_turn_delta_and_skips_unchanged_totals() {
        let first = json!({
            "timestamp": "2026-08-13T10:00:00Z",
            "payload": { "info": {
                "total_token_usage": { "input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 20, "total_tokens": 120 },
                "last_token_usage": { "input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 20, "total_tokens": 120 }
            }}
        });
        let repeated = first.clone();
        let second = json!({
            "timestamp": "2026-08-13T10:01:00Z",
            "payload": { "info": {
                "total_token_usage": { "input_tokens": 160, "cached_input_tokens": 70, "output_tokens": 35, "total_tokens": 195 }
            }}
        });
        let mut state = CodexState::default();
        assert_eq!(
            parse_codex_event(&first, &mut state).unwrap().1.tokens,
            120.0
        );
        assert!(parse_codex_event(&repeated, &mut state).is_none());
        assert_eq!(
            parse_codex_event(&second, &mut state).unwrap().1.tokens,
            75.0
        );
    }

    #[test]
    fn grok_reads_inference_done_context() {
        let event = json!({
            "ts": "2026-08-13T10:00:00Z",
            "msg": "shell.turn.inference_done",
            "ctx": { "prompt_tokens": 100, "completion_tokens": 20, "reasoning_tokens": 5 }
        });
        assert_eq!(
            parse_grok_event(&event, Some("grok-build-0.1"))
                .unwrap()
                .1
                .tokens,
            125.0
        );
    }

    #[test]
    fn pi_usage_is_attributed_and_uses_carried_cost() {
        let event = json!({
            "id": "msg-1",
            "type": "message",
            "timestamp": "2026-08-13T10:00:00Z",
            "message": {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-4-5-20250929",
                "usage": {
                    "input": 100,
                    "cacheWrite": 20,
                    "cacheWrite1h": 5,
                    "cacheRead": 30,
                    "output": 40,
                    "totalTokens": 190,
                    "cost": { "total": 0.42 }
                }
            }
        });
        let (id, _, spend) = parse_pi_event(&event, "claude").expect("Pi usage");
        assert_eq!(id.as_deref(), Some("msg-1"));
        assert_eq!(spend.tokens, 190.0);
        assert_eq!(spend.dollars, 0.42);
        assert!(parse_pi_event(&event, "codex").is_none());
    }

    #[test]
    fn claude_uses_cache_aware_bundled_pricing() {
        let event = json!({
            "timestamp": "2026-08-13T10:00:00Z",
            "message": {
                "model": "claude-sonnet-4-5-20250929",
                "usage": {
                    "input_tokens": 100000,
                    "cache_creation_input_tokens": 100000,
                    "cache_read_input_tokens": 100000,
                    "output_tokens": 100000
                }
            }
        });
        let spend = parse_claude_event(&event).expect("Claude usage").1;
        assert_eq!(spend.tokens, 400_000.0);
        // Long-context rates apply because the prompt exceeds 200k tokens.
        assert!((spend.dollars - 3.66).abs() < 0.000_001);
    }

    #[test]
    fn claude_flags_unknown_models_without_mixing_unpriced_tokens_into_totals() {
        let event = json!({
            "timestamp": "2026-08-13T10:00:00Z",
            "message": {
                "model": "future-model-with-no-price",
                "usage": { "input_tokens": 100, "output_tokens": 50 }
            }
        });
        let spend = parse_claude_event(&event).expect("Claude usage").1;
        assert_eq!(spend.tokens, 0.0);
        assert_eq!(
            spend.unknown_models,
            BTreeSet::from(["future-model-with-no-price".to_string()])
        );
    }

    #[test]
    fn claude_dedup_prefers_parent_then_larger_and_richer_entries() {
        let entry =
            |request: &str, sidechain: bool, tokens: f64, speed: bool, dollars: f64| ClaudeEntry {
                message_id: Some("msg-1".into()),
                request_id: Some(request.into()),
                is_sidechain: sidechain,
                has_speed: speed,
                reported_tokens: tokens,
                day: Local::now().date_naive(),
                spend: DaySpend {
                    dollars,
                    tokens,
                    ..DaySpend::default()
                },
            };
        let deduped = dedup_claude(vec![
            entry("side", true, 500.0, true, 5.0),
            entry("parent", false, 100.0, false, 1.0),
            entry("parent", false, 200.0, false, 2.0),
            entry("parent", false, 200.0, true, 3.0),
        ]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].spend.dollars, 3.0);
    }

    #[test]
    fn opencode_scans_json_message_rows_as_measured_spend() {
        let path = std::env::temp_dir().join(format!(
            "multimeters-opencode-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE message (time_created INTEGER NOT NULL, data TEXT NOT NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2)",
                rusqlite::params![
                    Utc::now().timestamp_millis(),
                    r#"{"role":"assistant","providerID":"opencode-go","modelID":"model","cost":0.75,"tokens":{"total":150}}"#
                ],
            )
            .unwrap();
        drop(connection);

        let mut days = BTreeMap::new();
        assert!(scan_opencode_database(&path, 0, &mut days).unwrap());
        let today = days.get(&Local::now().date_naive()).unwrap();
        assert_eq!(today.tokens, 150.0);
        assert_eq!(today.dollars, 0.75);
        let lines = OpenCodeScan {
            days,
            has_hosted_rows: true,
        }
        .into_lines();
        let MetricLine::Values { values, .. } = &lines[0] else {
            panic!("expected measured spend")
        };
        assert!(!values[0].estimated);
        std::fs::remove_file(path).unwrap();
    }
}
