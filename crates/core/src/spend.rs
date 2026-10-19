use chrono::{Local, NaiveDate, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

use crate::json::{num, num_at, str_at};
use crate::models::{ChartPoint, MetricLine, SpendCoverage};
use crate::paths;

#[derive(Default, Clone)]
pub struct DaySpend {
    pub dollars: f64,
    pub tokens: f64,
    pub priced_tokens: f64,
    pub unknown_models: BTreeSet<String>,
    pub incomplete: bool,
    pub stale: bool,
    repricing: Option<UsageCost>,
}

#[derive(Clone)]
struct UsageCost {
    model: Option<String>,
    breakdown: crate::pricing::TokenBreakdown,
    reported: Option<f64>,
    allow_estimate: bool,
}

impl DaySpend {
    fn reprice(&self) -> Self {
        let Some(raw) = &self.repricing else {
            return self.clone();
        };
        let mut value = self.clone();
        value.unknown_models.clear();
        let cost = raw
            .reported
            .or_else(|| {
                raw.model
                    .as_deref()
                    .filter(|_| raw.allow_estimate)
                    .and_then(|model| crate::pricing::estimate_cost(model, raw.breakdown))
            })
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        value.dollars = cost.unwrap_or(0.0);
        value.priced_tokens = if cost.is_some() { value.tokens } else { 0.0 };
        if cost.is_none() {
            value
                .unknown_models
                .insert(raw.model.clone().unwrap_or_else(|| "Unknown model".into()));
        }
        value
    }
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum LogAdapter {
    Claude,
    Codex,
    Grok,
    Pi,
}

#[derive(Clone)]
struct UsageRecord {
    timestamp: chrono::DateTime<Utc>,
    spend: DaySpend,
    claude: Option<ClaudeEntry>,
    event_key: Option<String>,
    card: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
struct FileStamp {
    identity: Option<(u64, u64)>,
    length: u64,
    modified: Option<std::time::SystemTime>,
}
fn file_stamp(metadata: &std::fs::Metadata) -> FileStamp {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let identity = None; // Uncertain identity deliberately selects the full-scan path.
    FileStamp {
        identity,
        length: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

#[derive(Clone, Default)]
struct FileUsage {
    stamp: Option<FileStamp>,
    digest: Vec<u8>,
    complete_offset: u64,
    records: Vec<UsageRecord>,
    codex: CodexState,
    grok_models: HashMap<i64, String>,
    malformed: bool,
    invalid_cards: BTreeSet<String>,
    incomplete_tail: bool,
    parses: usize,
    local_day: Option<NaiveDate>,
    local_offset: i32,
}
#[derive(Default)]
struct UsageCache {
    files: HashMap<(LogAdapter, PathBuf), FileUsage>,
    aliases: HashMap<(LogAdapter, PathBuf), PathBuf>,
}
#[cfg(test)]
thread_local! { static READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = std::cell::RefCell::new(None); }
static USAGE_CACHE: OnceLock<Mutex<UsageCache>> = OnceLock::new();

impl UsageCache {
    #[cfg(test)]
    fn read(
        &mut self,
        adapter: LogAdapter,
        path: &Path,
        full_scan: bool,
    ) -> (Vec<UsageRecord>, bool, bool) {
        self.read_policy(adapter, path, full_scan, true)
    }
    fn read_policy(
        &mut self,
        adapter: LogAdapter,
        path: &Path,
        full_scan: bool,
        confirmed_discovery: bool,
    ) -> (Vec<UsageRecord>, bool, bool) {
        let alias = (adapter, path.to_path_buf());
        let canonical = path.canonicalize().unwrap_or_else(|_| {
            self.aliases
                .get(&alias)
                .cloned()
                .unwrap_or_else(|| path.to_path_buf())
        });
        self.aliases.insert(alias, canonical.clone());
        let key = (adapter, canonical);
        let previous = self.files.get(&key).cloned().unwrap_or_default();
        if path
            .metadata()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            if confirmed_discovery {
                self.files.remove(&key);
                self.aliases
                    .retain(|(kind, _), canonical| *kind != adapter || *canonical != key.1);
                return (Vec::new(), true, false);
            }
            return (previous.records, false, true);
        }
        let today = Local::now().date_naive();
        let utc_offset = Local::now().offset().local_minus_utc();
        let full_scan = full_scan
            || previous.local_day.is_some_and(|day| today < day)
            || previous.local_offset != utc_offset;
        for _ in 0..2 {
            if let Ok(mut next) = read_stable_usage(adapter, path, &previous, full_scan) {
                let cutoff = Local::now().date_naive() - chrono::Duration::days(30);
                next.records
                    .retain(|record| record.timestamp.with_timezone(&Local).date_naive() >= cutoff);
                let records = next.records.clone();
                next.local_day = Some(today);
                next.local_offset = utc_offset;
                let complete = !next.malformed && !next.incomplete_tail;
                self.files.insert(key, next);
                return (records, complete, false);
            }
        }
        (previous.records, false, true)
    }
}

fn read_stable_usage(
    adapter: LogAdapter,
    path: &Path,
    previous: &FileUsage,
    full_scan: bool,
) -> std::io::Result<FileUsage> {
    let mut file = std::fs::File::open(path)?;
    let before = file_stamp(&file.metadata()?);
    let old_length = previous.stamp.as_ref().map_or(0, |stamp| stamp.length);
    let mut digest = Sha256::new();
    let mut prefix = None;
    let mut offset = 0;
    let mut buffer = [0u8; 64 * 1024];
    while offset < before.length {
        let mut count = buffer.len().min((before.length - offset) as usize);
        if offset < old_length {
            count = count.min((old_length - offset) as usize);
        }
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        offset += count as u64;
        if offset == old_length {
            prefix = Some(digest.clone().finalize().to_vec());
        }
    }
    let current_digest = digest.finalize().to_vec();
    let reusable = !full_scan
        && before.identity.is_some()
        && previous
            .stamp
            .as_ref()
            .is_some_and(|stamp| stamp.identity == before.identity)
        && before.length >= old_length
        && (before.length == old_length && current_digest == previous.digest
            || prefix.as_ref() == Some(&previous.digest));
    let mut next = if reusable {
        previous.clone()
    } else {
        FileUsage::default()
    };
    if !reusable || before.length != old_length {
        file.seek(SeekFrom::Start(next.complete_offset))?;
        let mut reader = BufReader::new((&mut file).take(before.length - next.complete_offset));
        let mut line = Vec::new();
        next.incomplete_tail = false;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                next.incomplete_tail = true;
                break;
            }
            next.complete_offset += line.len() as u64;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            next.parses += 1;
            match serde_json::from_slice::<Value>(&line) {
                Ok(value) => normalize_usage(adapter, &value, &mut next),
                Err(_) => next.malformed = true,
            }
        }
    }
    #[cfg(test)]
    READ_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().as_mut() {
            callback();
        }
    });
    if before != file_stamp(&file.metadata()?) || before != file_stamp(&path.metadata()?) {
        return Err(std::io::Error::other("Usage log changed during read"));
    }
    next.stamp = Some(before);
    next.digest = current_digest;
    Ok(next)
}

fn event_timestamp(value: &Value) -> Option<chrono::DateTime<Utc>> {
    for key in ["timestamp", "time", "ts", "created_at"] {
        if let Some(text) = str_at(value, key) {
            return crate::json::parse_iso(text).map(|date| date.with_timezone(&Utc));
        }
    }
    if let Some(text) = value.pointer("/message/timestamp").and_then(Value::as_str) {
        return crate::json::parse_iso(text).map(|date| date.with_timezone(&Utc));
    }
    let number = num_at(value, "timestamp").or_else(|| num_at(value, "ts"))?;
    let milliseconds = if number > 1_000_000_000_000.0 {
        number as i64
    } else {
        (number * 1000.0) as i64
    };
    Utc.timestamp_millis_opt(milliseconds).single()
}

fn normalize_usage(adapter: LogAdapter, value: &Value, file: &mut FileUsage) {
    // Only normalized metering and deduplication fields survive this function.
    // The parsed JSON, including any prompt or credential fields, is discarded.
    let pi_card = if adapter == LogAdapter::Pi {
        match value.pointer("/message/provider").and_then(Value::as_str) {
            Some("anthropic" | "claude-agent-sdk") => Some("claude"),
            Some("openai-codex") => Some("codex"),
            Some("cursor") => Some("cursor"),
            Some("zai" | "zhipu") => Some("zai"),
            Some("google-antigravity") => Some("antigravity"),
            Some("github-copilot") => Some("copilot"),
            _ => return,
        }
    } else {
        None
    };
    let metering = match adapter {
        LogAdapter::Claude => value
            .pointer("/message/usage")
            .or_else(|| value.get("usage")),
        LogAdapter::Codex => value
            .pointer("/payload/info/last_token_usage")
            .or_else(|| value.pointer("/payload/info/total_token_usage"))
            .or_else(|| value.pointer("/payload/usage"))
            .or_else(|| value.get("usage"))
            .or_else(|| value.pointer("/event/usage")),
        LogAdapter::Grok => (str_at(value, "msg") == Some("shell.turn.inference_done"))
            .then(|| value.get("ctx").unwrap_or(value)),
        LogAdapter::Pi => (str_at(value, "type") == Some("message")
            && value.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
        .then(|| value.pointer("/message/usage"))
        .flatten(),
    };
    if let Some(usage) = metering {
        let missing_required = match adapter {
            LogAdapter::Claude => {
                num_at(usage, "input_tokens").is_none() || num_at(usage, "output_tokens").is_none()
            }
            LogAdapter::Grok => num_at(usage, "prompt_tokens")
                .or_else(|| num_at(usage, "input_tokens"))
                .is_none(),
            LogAdapter::Codex => ![
                "total_tokens",
                "input_tokens",
                "prompt_tokens",
                "input",
                "output_tokens",
                "completion_tokens",
                "output",
            ]
            .iter()
            .any(|key| usage.get(key).is_some()),
            LogAdapter::Pi => !["totalTokens", "input", "output"]
                .iter()
                .any(|key| usage.get(key).is_some()),
        };
        if event_timestamp(value).is_none() || missing_required {
            if let Some(card) = pi_card {
                file.invalid_cards.insert(card.into());
            } else {
                file.malformed = true;
            }
            return;
        }
    }
    if invalid_metering(value) {
        if let Some(card) = pi_card {
            file.invalid_cards.insert(card.into());
        } else {
            file.malformed = true;
        }
        return;
    }
    let mut record = None;
    match adapter {
        LogAdapter::Claude => {
            if let Some((day, spend)) = parse_claude_event(value) {
                let message = value.get("message").unwrap_or(value);
                let entry = ClaudeEntry {
                    message_id: str_at(message, "id")
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(ToOwned::to_owned),
                    request_id: str_at(value, "requestId")
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(ToOwned::to_owned),
                    is_sidechain: crate::json::bool_at(value, "isSidechain").unwrap_or(false),
                    has_speed: message
                        .get("usage")
                        .or_else(|| value.get("usage"))
                        .is_some_and(|usage| usage.get("speed").is_some()),
                    reported_tokens: claude_reported_tokens(value),
                    day,
                    spend: spend.clone(),
                };
                record = Some((spend, Some(entry), None, None));
            }
        }
        LogAdapter::Codex => {
            if let Some((_, spend)) = parse_codex_event(value, &mut file.codex) {
                record = Some((spend, None, Some(codex_event_key(value, &file.codex)), None));
            }
        }
        LogAdapter::Grok => {
            let pid = num_at(value, "pid").map(|pid| pid as i64);
            if let Some(model) = grok_model_event(value) {
                if let Some(pid) = pid {
                    file.grok_models.insert(pid, model.to_owned());
                }
            } else if let Some((_, spend)) = parse_grok_event(
                value,
                pid.and_then(|pid| file.grok_models.get(&pid).map(String::as_str)),
            ) {
                record = Some((spend, None, None, None));
            }
        }
        LogAdapter::Pi => {
            for card in ["claude", "codex", "cursor", "zai", "antigravity", "copilot"] {
                if let Some((id, _, spend)) = parse_pi_event(value, card) {
                    record = Some((spend, None, id, Some(card.to_owned())));
                    break;
                }
            }
        }
    }
    if let (Some(timestamp), Some((spend, claude, event_key, card))) =
        (event_timestamp(value), record)
    {
        file.records.push(UsageRecord {
            timestamp,
            spend,
            claude,
            event_key,
            card,
        });
    }
}

fn cached_file(
    adapter: LogAdapter,
    path: &Path,
    confirmed_discovery: bool,
    card: Option<&str>,
) -> (Vec<UsageRecord>, bool, bool) {
    let mut cache = USAGE_CACHE
        .get_or_init(|| Mutex::new(UsageCache::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (records, mut complete, stale) =
        cache.read_policy(adapter, path, false, confirmed_discovery);
    if let Some(card) = card {
        if let Some(entry) = cache.files.get(&(
            adapter,
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        )) {
            complete &= !entry.invalid_cards.contains(card);
        }
    }
    (records, complete, stale)
}

fn discovery_files(adapter: LogAdapter, roots: &[PathBuf]) -> (Vec<PathBuf>, bool) {
    let mut files = BTreeSet::new();
    let mut complete = true;
    for root in roots {
        match root.metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                complete = false;
                continue;
            }
            Ok(_) => {}
        }
        for entry in WalkDir::new(root) {
            match entry {
                Ok(entry)
                    if entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                        && is_recent_file(entry.path()) =>
                {
                    files.insert(
                        entry
                            .path()
                            .canonicalize()
                            .unwrap_or_else(|_| entry.path().to_path_buf()),
                    );
                }
                Ok(_) => {}
                Err(_) => complete = false,
            }
        }
    }
    let mut cache = USAGE_CACHE
        .get_or_init(|| Mutex::new(UsageCache::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if complete {
        cache
            .files
            .retain(|(kind, path), _| *kind != adapter || files.contains(path));
        cache
            .aliases
            .retain(|(kind, _), canonical| *kind != adapter || files.contains(canonical));
    } else {
        files.extend(
            cache
                .files
                .keys()
                .filter(|(kind, _)| *kind == adapter)
                .map(|(_, path)| path.clone()),
        );
    }
    (files.into_iter().collect(), complete)
}

fn invalid_metering(value: &Value) -> bool {
    let containers = [
        "/usage",
        "/message/usage",
        "/payload/usage",
        "/event/usage",
        "/payload/info/last_token_usage",
        "/payload/info/total_token_usage",
        "/ctx",
    ];
    let fields = [
        "input_tokens",
        "prompt_tokens",
        "input",
        "output_tokens",
        "completion_tokens",
        "output",
        "total_tokens",
        "totalTokens",
        "cached_input_tokens",
        "cache_read_input_tokens",
        "cached_tokens",
        "cached_prompt_tokens",
        "cache_creation_input_tokens",
        "reasoning_output_tokens",
        "reasoning_tokens",
        "cacheWrite",
        "cacheWrite1h",
        "cacheRead",
        "ephemeral_5m_input_tokens",
        "ephemeral_1h_input_tokens",
    ];
    containers
        .iter()
        .filter_map(|path| value.pointer(path))
        .chain(std::iter::once(value))
        .any(|usage| {
            fields
                .iter()
                .filter_map(|field| usage.get(field))
                .any(|count| num(count).is_none_or(|count| !count.is_finite() || count < 0.0))
                || usage.get("cache_creation").is_some_and(|cache| {
                    fields
                        .iter()
                        .filter_map(|field| cache.get(field))
                        .any(|count| {
                            num(count).is_none_or(|count| !count.is_finite() || count < 0.0)
                        })
                })
        })
}

fn mark_scan(days: &mut BTreeMap<NaiveDate, DaySpend>, complete: bool, stale: bool) {
    if !complete || stale {
        for i in 0..30 {
            let spend = days
                .entry(Local::now().date_naive() - chrono::Duration::days(i))
                .or_default();
            spend.incomplete |= !complete;
            spend.stale |= stale;
        }
    }
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
    period_lines_at(days, estimated, note, Local::now().date_naive())
}

fn period_lines_at(
    days: BTreeMap<NaiveDate, DaySpend>,
    estimated: bool,
    note: &str,
    today: NaiveDate,
) -> Vec<MetricLine> {
    let yesterday = today - chrono::Duration::days(1);
    let mut last30 = DaySpend::default();
    let mut points = Vec::new();
    for i in (0..30).rev() {
        let day = today - chrono::Duration::days(i);
        let spend = days.get(&day).cloned().unwrap_or_default();
        last30.dollars += spend.dollars;
        last30.tokens += spend.tokens;
        last30.priced_tokens += spend.priced_tokens;
        last30.incomplete |= spend.incomplete;
        last30.stale |= spend.stale;
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
    if d.incomplete && d.tokens == 0.0 && d.dollars == 0.0 && d.unknown_models.is_empty() {
        return MetricLine::badge(label, "Unavailable", Some("#A3A3A3"));
    }
    let valid = d.dollars.is_finite()
        && d.dollars >= 0.0
        && d.tokens.is_finite()
        && d.tokens >= 0.0
        && d.priced_tokens.is_finite()
        && d.priced_tokens >= 0.0;
    if !valid {
        return MetricLine::badge(label, "Unavailable", Some("#A3A3A3"));
    }
    let mut line = MetricLine::spend(label, d.dollars, d.tokens, estimated);
    if let MetricLine::Values {
        unknown_models,
        spend_coverage,
        ..
    } = &mut line
    {
        let has_cost = d.priced_tokens > 0.0 || d.unknown_models.is_empty();
        *spend_coverage = Some(SpendCoverage {
            observed_tokens: d.tokens,
            priced_tokens: has_cost.then_some(d.priced_tokens),
            dollars: has_cost.then_some(d.dollars),
            complete: !d.incomplete,
            stale: d.stale,
            estimated,
        });
        unknown_models.extend(d.unknown_models);
    }
    line
}

fn scan_claude() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let (files, mut complete) = discovery_files(LogAdapter::Claude, &claude_project_roots());
    let discovery_complete = complete;
    let mut stale = false;
    let mut entries = Vec::new();
    for path in files {
        let (records, success, retained) =
            cached_file(LogAdapter::Claude, &path, discovery_complete, None);
        complete &= success;
        stale |= retained;
        for record in records {
            if let Some(mut entry) = record.claude {
                entry.day = record.timestamp.with_timezone(&Local).date_naive();
                entry.spend = record.spend.reprice();
                entries.push(entry);
            }
        }
    }
    for entry in dedup_claude(entries) {
        merge_day(days.entry(entry.day).or_default(), entry.spend);
    }
    merge_days(&mut days, scan_pi("claude"));
    mark_scan(&mut days, complete, stale);
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
        .filter(|root| seen.insert(root.clone()))
        .collect()
}

fn scan_pi(card_id: &str) -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let (files, mut complete) = discovery_files(LogAdapter::Pi, &[paths::pi_sessions()]);
    let discovery_complete = complete;
    let mut stale = false;
    let mut seen = HashSet::new();
    for path in files {
        let (records, success, retained) =
            cached_file(LogAdapter::Pi, &path, discovery_complete, Some(card_id));
        complete &= success;
        stale |= retained;
        for record in records {
            if record.card.as_deref() == Some(card_id)
                && !record
                    .event_key
                    .as_ref()
                    .is_some_and(|key| !seen.insert(key.clone()))
            {
                merge_day(
                    days.entry(record.timestamp.with_timezone(&Local).date_naive())
                        .or_default(),
                    record.spend.reprice(),
                );
            }
        }
    }
    mark_scan(&mut days, complete, stale);
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
    target.priced_tokens += source.priced_tokens;
    target.incomplete |= source.incomplete;
    target.stale |= source.stale;
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
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    let mut spend = DaySpend {
        tokens,
        repricing: Some(UsageCost {
            model: model.map(ToOwned::to_owned),
            breakdown,
            reported: carried_cost,
            allow_estimate: true,
        }),
        ..DaySpend::default()
    };
    if let Some(cost) = carried_cost
        .or_else(|| model.and_then(|model| crate::pricing::estimate_cost(model, breakdown)))
    {
        spend.dollars = cost;
        spend.priced_tokens = tokens;
    } else {
        spend
            .unknown_models
            .insert(model.unwrap_or("Unknown model").to_string());
    }
    Some((str_at(v, "id").map(ToOwned::to_owned), day, spend))
}

fn scan_codex() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let home = paths::codex_home();
    let (files, mut complete) = discovery_files(
        LogAdapter::Codex,
        &[home.join("sessions"), home.join("archived_sessions")],
    );
    let discovery_complete = complete;
    let mut stale = false;
    let mut seen = HashSet::new();
    for path in files {
        let (records, success, retained) =
            cached_file(LogAdapter::Codex, &path, discovery_complete, None);
        complete &= success;
        stale |= retained;
        for record in records {
            if !record
                .event_key
                .as_ref()
                .is_some_and(|key| !seen.insert(key.clone()))
            {
                merge_day(
                    days.entry(record.timestamp.with_timezone(&Local).date_naive())
                        .or_default(),
                    record.spend.reprice(),
                );
            }
        }
    }
    merge_days(&mut days, scan_pi("codex"));
    mark_scan(&mut days, complete, stale);
    days
}

fn scan_grok() -> BTreeMap<NaiveDate, DaySpend> {
    let mut days = BTreeMap::new();
    let path = paths::grok_home().join("logs/unified.jsonl");
    let (records, complete, stale) = cached_file(LogAdapter::Grok, &path, true, None);
    for record in records {
        merge_day(
            days.entry(record.timestamp.with_timezone(&Local).date_naive())
                .or_default(),
            record.spend.reprice(),
        );
    }
    mark_scan(&mut days, complete, stale);
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
    mark_scan(&mut days, readable == databases.len(), false);
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
        if !cost.is_finite() || cost < 0.0 || !tokens.is_finite() || tokens < 0.0 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        found = true;
        let spend = days.entry(day).or_default();
        spend.dollars += cost.max(0.0);
        spend.tokens += tokens.max(0.0);
        spend.priced_tokens += tokens.max(0.0);
    }
    Ok(found)
}

use chrono::TimeZone;

#[derive(Clone)]
struct ClaudeEntry {
    message_id: Option<String>,
    request_id: Option<String>,
    is_sidechain: bool,
    has_speed: bool,
    reported_tokens: f64,
    day: NaiveDate,
    spend: DaySpend,
}

#[cfg(test)]
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
    let supported_speed = !speed.is_some_and(|speed| !matches!(speed, "fast" | "standard"));
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
    let mut spend = DaySpend {
        tokens,
        repricing: Some(UsageCost {
            model: model.map(|name| {
                if supported_speed {
                    name.to_owned()
                } else {
                    format!("{name} [unsupported service tier]")
                }
            }),
            breakdown,
            reported: carried_cost,
            allow_estimate: supported_speed,
        }),
        ..DaySpend::default()
    };
    if let Some(cost) = carried_cost.or_else(|| {
        model
            .filter(|_| supported_speed)
            .and_then(|model| crate::pricing::estimate_cost(model, breakdown))
    }) {
        spend.dollars = cost.max(0.0);
        spend.priced_tokens = tokens;
    } else {
        spend
            .unknown_models
            .insert(model.unwrap_or("Unknown model").to_string());
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

#[derive(Clone, Default)]
struct CodexState {
    previous: Option<CodexUsage>,
    model: Option<String>,
    fast: bool,
}

#[cfg(test)]
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
            merge_day(total, spend);
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
    let identity = format!(
        "{timestamp}|{}|{}|{usage}",
        state.model.as_deref().unwrap_or_default(),
        state.fast
    );
    format!("{:x}", Sha256::digest(identity.as_bytes()))
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
        .unwrap_or_else(|| "Unknown model".into());
    state.model = Some(model.clone());
    let cached = usage.cached.min(usage.input);
    let breakdown = crate::pricing::TokenBreakdown {
        input: (usage.input - cached).max(0.0),
        cache_read: cached,
        output: usage.output,
        fast: state.fast,
        ..crate::pricing::TokenBreakdown::default()
    };
    let carried_cost = num_at(v, "cost_usd").filter(|cost| *cost >= 0.0);
    let mut spend = DaySpend {
        tokens,
        repricing: Some(UsageCost {
            model: Some(model.clone()),
            breakdown,
            reported: carried_cost,
            allow_estimate: true,
        }),
        ..DaySpend::default()
    };
    if let Some(cost) = carried_cost.or_else(|| crate::pricing::estimate_cost(&model, breakdown)) {
        spend.dollars = cost.max(0.0);
        spend.priced_tokens = tokens;
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
    let carried_cost = num_at(v, "cost")
        .or_else(|| num_at(usage, "cost"))
        .filter(|cost| *cost >= 0.0);
    let mut spend = DaySpend {
        tokens,
        repricing: Some(UsageCost {
            model: model.map(ToOwned::to_owned),
            breakdown,
            reported: carried_cost,
            allow_estimate: true,
        }),
        ..DaySpend::default()
    };
    if let Some(cost) = num_at(v, "cost")
        .or_else(|| num_at(usage, "cost"))
        .or_else(|| model.and_then(|model| crate::pricing::estimate_cost(model, breakdown)))
    {
        spend.dollars = cost.max(0.0);
        spend.priced_tokens = tokens;
    } else {
        spend
            .unknown_models
            .insert(model.unwrap_or("Unknown model").to_string());
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

    fn log_fixture() -> (PathBuf, PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "multi-meters-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        (root.clone(), root.join("usage.jsonl"))
    }
    fn metered_row(id: &str, cost: f64) -> String {
        format!(
            "{}\n",
            json!({"timestamp": Utc::now().to_rfc3339(), "requestId": id, "costUSD": cost, "message": {"id": id, "model": "unknown-fixture-model", "usage": {"input_tokens": 100, "output_tokens": 50}, "content": "PRIVATE PROMPT MUST NEVER BE CACHED"}})
        )
    }
    fn fixture_total(records: &[UsageRecord]) -> (f64, f64) {
        records.iter().fold((0.0, 0.0), |(cost, tokens), record| {
            let spend = record.spend.reprice();
            (cost + spend.dollars, tokens + spend.tokens)
        })
    }
    #[test]
    fn usage_cache_hashes_without_reparsing_and_appends_only_complete_rows() {
        let (root, path) = log_fixture();
        let first = metered_row("one", 2.0);
        std::fs::write(&path, &first).unwrap();
        let mut cache = UsageCache::default();
        let (records, complete, stale) = cache.read(LogAdapter::Claude, &path, false);
        assert!(complete && !stale);
        assert_eq!(fixture_total(&records), (2.0, 150.0));
        let parses = cache.files.values().next().unwrap().parses;
        cache.read(LogAdapter::Claude, &path, false);
        assert_eq!(cache.files.values().next().unwrap().parses, parses);
        let second = metered_row("two", 3.0);
        std::fs::write(&path, format!("{}{}", first, second.trim_end())).unwrap();
        let (records, complete, _) = cache.read(LogAdapter::Claude, &path, false);
        assert!(!complete);
        assert_eq!(fixture_total(&records), (2.0, 150.0));
        std::fs::write(&path, format!("{first}{second}")).unwrap();
        let (records, complete, _) = cache.read(LogAdapter::Claude, &path, false);
        assert!(complete);
        assert_eq!(fixture_total(&records), (5.0, 300.0));
        assert_eq!(cache.files.values().next().unwrap().parses, parses + 1);
        let mut full = Vec::new();
        read_claude_jsonl(&path, &mut full);
        assert_eq!(
            full.iter().map(|entry| entry.spend.dollars).sum::<f64>(),
            5.0
        );
        assert_eq!(
            fixture_total(&cache.read(LogAdapter::Claude, &path, true).0),
            (5.0, 300.0)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn usage_cache_replacement_truncation_rotation_and_deletion_match_full_reader() {
        let (root, path) = log_fixture();
        std::fs::write(&path, metered_row("one", 2.0)).unwrap();
        let mut cache = UsageCache::default();
        let original = cache.read(LogAdapter::Claude, &path, false).0;
        let rotated = root.join("rotated.jsonl");
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, metered_row("one", 2.0)).unwrap();
        let mut entries = Vec::new();
        for record in original
            .into_iter()
            .chain(cache.read(LogAdapter::Claude, &rotated, false).0)
            .chain(cache.read(LogAdapter::Claude, &path, false).0)
        {
            entries.push(record.claude.unwrap());
        }
        assert_eq!(
            dedup_claude(entries)
                .iter()
                .map(|entry| entry.spend.dollars)
                .sum::<f64>(),
            2.0
        );
        std::fs::write(&path, metered_row("changed", 7.0)).unwrap();
        assert_eq!(
            fixture_total(&cache.read(LogAdapter::Claude, &path, false).0),
            (7.0, 150.0)
        );
        std::fs::write(&path, "").unwrap();
        let (records, complete, stale) = cache.read(LogAdapter::Claude, &path, false);
        assert!(records.is_empty() && complete && !stale);
        std::fs::remove_file(&path).unwrap();
        assert!(cache.read(LogAdapter::Claude, &path, false).0.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn moving_usage_file_retries_once_then_retains_prior_normalized_snapshot() {
        let (root, path) = log_fixture();
        std::fs::write(&path, metered_row("one", 2.0)).unwrap();
        let mut cache = UsageCache::default();
        cache.read(LogAdapter::Claude, &path, false);
        let changing = path.clone();
        let mut writes = 0;
        READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                writes += 1;
                use std::io::Write;
                writeln!(
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&changing)
                        .unwrap(),
                    "{}",
                    metered_row(&format!("late-{writes}"), 1.0).trim()
                )
                .unwrap();
            }))
        });
        let (records, complete, stale) = cache.read(LogAdapter::Claude, &path, false);
        READ_HOOK.with(|hook| *hook.borrow_mut() = None);
        assert!(!complete && stale);
        assert_eq!(fixture_total(&records), (2.0, 150.0));
        let (records, complete, stale) = cache.read(LogAdapter::Claude, &path, false);
        assert!(complete && !stale);
        assert_eq!(fixture_total(&records), (4.0, 450.0));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn codex_cached_cumulative_deltas_match_full_reader_and_keep_unpriced_tokens() {
        let (root, path) = log_fixture();
        let time = Utc::now().to_rfc3339();
        let first = json!({"timestamp": time, "payload": {"model": "unknown-fixture-model", "info": {"total_token_usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}}}});
        let second = json!({"timestamp": time, "payload": {"model": "unknown-fixture-model", "info": {"total_token_usage": {"input_tokens": 160, "output_tokens": 35, "total_tokens": 195}}}});
        std::fs::write(&path, format!("{first}\n{first}\n")).unwrap();
        let mut cache = UsageCache::default();
        assert_eq!(
            fixture_total(&cache.read(LogAdapter::Codex, &path, false).0),
            (0.0, 120.0)
        );
        std::fs::write(&path, format!("{first}\n{first}\n{second}\n")).unwrap();
        let records = cache.read(LogAdapter::Codex, &path, false).0;
        assert_eq!(fixture_total(&records), (0.0, 195.0));
        assert!(records
            .iter()
            .all(|record| record.spend.priced_tokens == 0.0));
        let mut full = BTreeMap::new();
        ingest_codex_jsonl(&path, &mut full, &mut HashSet::new());
        assert_eq!(full.values().map(|spend| spend.tokens).sum::<f64>(), 195.0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_cache_cost_equivalence_and_invalid_metering_are_explicit() {
        let (root, path) = log_fixture();
        let time = Utc::now().to_rfc3339();
        let first = json!({"timestamp": time, "cost_usd": 2.0, "payload": {"model": "fixture", "info": {"total_token_usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}}}});
        let second = json!({"timestamp": time, "cost_usd": 1.0, "payload": {"model": "fixture", "info": {"total_token_usage": {"input_tokens": 160, "output_tokens": 35, "total_tokens": 195}}}});
        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let mut cache = UsageCache::default();
        let records = cache.read(LogAdapter::Codex, &path, false).0;
        assert_eq!(fixture_total(&records), (3.0, 195.0));
        let mut full = BTreeMap::new();
        ingest_codex_jsonl(&path, &mut full, &mut HashSet::new());
        assert_eq!(full.values().map(|spend| spend.dollars).sum::<f64>(), 3.0);
        let bad = json!({"timestamp": time, "message": {"model": "fixture", "usage": {"input_tokens": -100, "output_tokens": 50}}});
        std::fs::write(&path, format!("{bad}\n")).unwrap();
        let (records, complete, _) = cache.read(LogAdapter::Claude, &path, false);
        assert!(records.is_empty() && !complete);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metering_errors_never_become_confirmed_zero_and_missing_discovery_preserves_data() {
        let (root, path) = log_fixture();
        let mut cache = UsageCache::default();
        let invalid = json!({"timestamp": "bad", "message": {"model": "fixture", "usage": {"input_tokens": 100, "output_tokens": 50}}});
        std::fs::write(&path, format!("{invalid}\n")).unwrap();
        let (records, complete, _) = cache.read(LogAdapter::Claude, &path, false);
        assert!(records.is_empty() && !complete);
        let unsupported = json!({"timestamp": Utc::now().to_rfc3339(), "message": {"model": "claude-sonnet-4-5-20250929", "usage": {"input_tokens": 100, "output_tokens": 50, "speed": "unsupported"}}});
        std::fs::write(&path, format!("{unsupported}\n")).unwrap();
        let (records, complete, _) = cache.read(LogAdapter::Claude, &path, false);
        assert!(complete);
        assert_eq!(fixture_total(&records), (0.0, 150.0));
        assert_eq!(records[0].spend.reprice().priced_tokens, 0.0);
        std::fs::remove_file(&path).unwrap();
        let (records, complete, stale) = cache.read_policy(LogAdapter::Claude, &path, false, false);
        assert!(!complete && stale);
        assert_eq!(fixture_total(&records), (0.0, 150.0));
        assert!(cache
            .read_policy(LogAdapter::Claude, &path, false, true)
            .0
            .is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coverage_uses_observed_tokens_and_exact_priced_subset_and_rolls_day_buckets() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let mut spend = DaySpend {
            dollars: 2.0,
            tokens: 1000.0,
            priced_tokens: 1000.0,
            ..DaySpend::default()
        };
        merge_day(
            &mut spend,
            DaySpend {
                tokens: 1000.0,
                unknown_models: BTreeSet::from(["unknown".into()]),
                ..DaySpend::default()
            },
        );
        let lines = period_lines_at(
            BTreeMap::from([(today, spend.clone())]),
            true,
            "fixture",
            today,
        );
        let MetricLine::Values {
            spend_coverage: Some(coverage),
            ..
        } = &lines[0]
        else {
            panic!("known subtotal")
        };
        assert_eq!(coverage.observed_tokens, 2000.0);
        assert_eq!(coverage.priced_tokens, Some(1000.0));
        assert_eq!(coverage.dollars, Some(2.0));
        let next = period_lines_at(
            BTreeMap::from([(today, spend)]),
            true,
            "fixture",
            today + chrono::Duration::days(1),
        );
        let MetricLine::Values {
            spend_coverage: Some(coverage),
            ..
        } = &next[0]
        else {
            panic!("confirmed zero")
        };
        assert_eq!(coverage.dollars, Some(0.0));
        let MetricLine::Values {
            spend_coverage: Some(coverage),
            ..
        } = &next[1]
        else {
            panic!("yesterday")
        };
        assert_eq!(coverage.observed_tokens, 2000.0);
    }

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
    fn claude_preserves_unknown_priced_model_tokens() {
        let event = json!({
            "timestamp": "2026-08-13T10:00:00Z",
            "message": {
                "model": "future-model-with-no-price",
                "usage": { "input_tokens": 100, "output_tokens": 50 }
            }
        });
        let spend = parse_claude_event(&event).expect("Claude usage").1;
        assert_eq!(spend.tokens, 150.0);
        assert_eq!(spend.priced_tokens, 0.0);
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
