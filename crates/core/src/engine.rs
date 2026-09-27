use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinSet;

use crate::format::{format_line, pace, used_ratio};
use crate::http::Http;
use crate::legacy;
use crate::limits::{self, ProviderLimits};
use crate::models::*;
use crate::paths;
use crate::providers::{catalog, Provider};
use crate::settings::AppSettings;
use crate::spend::spend_from_lines;

pub struct AppEngine {
    http: Http,
    providers: Vec<Arc<dyn Provider>>,
    inner: RwLock<Inner>,
    refresh_guard: Mutex<()>,
}

struct Inner {
    settings: AppSettings,
    snapshots: HashMap<String, ProviderSnapshot>,
    errors: HashMap<String, String>,
    last_attempt: Option<Instant>,
    refreshing: bool,
}

impl AppEngine {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let http = Http::new()?;
        let providers = catalog();
        let mut settings = AppSettings::load();
        let repaired = repair_loaded_settings(&mut settings);
        sanitize_settings(&mut settings, &providers);
        settings.validate_values().map_err(anyhow::Error::msg)?;
        if repaired {
            if let Err(error) = settings.save() {
                tracing::warn!(%error, "could not persist repaired settings");
            }
        }
        let snapshots = load_cache();
        Ok(Arc::new(Self {
            http,
            providers,
            inner: RwLock::new(Inner {
                settings,
                snapshots,
                errors: HashMap::new(),
                last_attempt: None,
                refreshing: false,
            }),
            refresh_guard: Mutex::new(()),
        }))
    }

    pub async fn seed_if_needed(&self) {
        let provider_ids: BTreeSet<String> = self
            .providers
            .iter()
            .map(|provider| provider.info().id.clone())
            .collect();
        let (seeded, known) = {
            let inner = self.inner.read().await;
            (
                inner.settings.seeded,
                inner.settings.known_providers.clone(),
            )
        };

        // Older settings files predate provider history. Treat the current catalog
        // as known so an upgrade never re-enables providers the user turned off.
        if seeded && known.is_empty() {
            let mut inner = self.inner.write().await;
            if inner.settings.known_providers.is_empty() {
                inner.settings.known_providers = provider_ids;
                if let Err(error) = inner.settings.save() {
                    tracing::error!(%error, "could not save provider catalog migration");
                }
            }
            return;
        }

        let candidates: BTreeSet<String> = if seeded {
            provider_ids.difference(&known).cloned().collect()
        } else {
            provider_ids.clone()
        };
        if candidates.is_empty() {
            return;
        }

        let found = self.detect_credentials(Some(&candidates)).await;
        let mut inner = self.inner.write().await;
        if !inner.settings.seeded && !found.is_empty() {
            inner.settings.enabled = found.into_iter().collect();
        } else {
            inner.settings.enabled.extend(found);
        }
        inner.settings.seeded = true;
        inner.settings.known_providers.extend(candidates);
        if let Err(error) = inner.settings.save() {
            tracing::error!(%error, "could not save provider detection");
        }
    }

    async fn detect_credentials(&self, only: Option<&BTreeSet<String>>) -> Vec<String> {
        let mut tasks = JoinSet::new();
        for provider in &self.providers {
            if only.is_some_and(|ids| !ids.contains(&provider.info().id)) {
                continue;
            }
            let provider = Arc::clone(provider);
            tasks.spawn(async move {
                let id = provider.info().id.clone();
                (id, provider.has_local_credentials().await)
            });
        }
        let mut found = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok((id, true)) => found.push(id),
                Ok((_, false)) => {}
                Err(error) => tracing::warn!(%error, "provider credential probe failed"),
            }
        }
        found
    }

    pub async fn settings(&self) -> AppSettings {
        self.inner.read().await.settings.clone()
    }

    pub async fn patch_settings(&self, patch: serde_json::Value) -> anyhow::Result<AppSettings> {
        validate_patch_keys(&patch)?;
        let mut inner = self.inner.write().await;
        let mut value = serde_json::to_value(&inner.settings)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("settings serialization is not an object"))?;
        for (key, value) in patch
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("settings patch must be an object"))?
        {
            object.insert(key.clone(), value.clone());
        }
        let mut settings: AppSettings = serde_json::from_value(value)?;
        settings.legacy_api_keys = inner.settings.legacy_api_keys.clone();
        settings.api_key_configured = inner.settings.api_key_configured.clone();
        settings.seeded = inner.settings.seeded;
        settings.known_providers = inner.settings.known_providers.clone();
        settings.validate_values().map_err(anyhow::Error::msg)?;
        sanitize_settings(&mut settings, &self.providers);
        settings.save()?;
        inner.settings = settings.clone();
        Ok(settings)
    }

    pub async fn toggle_pin(
        &self,
        provider_id: &str,
        widget_id: &str,
    ) -> Result<AppSettings, String> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.info().id == provider_id)
            .ok_or_else(|| "Unknown provider".to_string())?;
        let widget = provider
            .widgets()
            .iter()
            .find(|widget| widget.id == widget_id)
            .ok_or_else(|| "Unknown metric".to_string())?;
        if !widget.pinnable {
            return Err("This metric cannot be pinned".into());
        }
        let mut inner = self.inner.write().await;
        let mut settings = inner.settings.clone();
        settings.toggle_pin(provider_id, widget_id)?;
        if settings.is_pinned(widget_id) {
            settings.hidden_metrics.remove(widget_id);
        }
        settings.save().map_err(|error| error.to_string())?;
        inner.settings = settings.clone();
        Ok(settings)
    }

    pub async fn set_api_key(&self, provider_id: &str, value: &str) -> anyhow::Result<AppSettings> {
        if !matches!(provider_id, "openrouter" | "zai") {
            anyhow::bail!("API keys are not supported for this provider");
        }
        let previous_key = paths::app_api_key_checked(provider_id)?;
        paths::set_app_api_key(provider_id, value)?;
        let mut inner = self.inner.write().await;
        let mut settings = inner.settings.clone();
        settings.legacy_api_keys.remove(provider_id);
        if value.trim().is_empty() {
            settings.api_key_configured.remove(provider_id);
        } else {
            settings.api_key_configured.insert(provider_id.to_string());
        }
        if let Err(error) = settings.save() {
            let rollback =
                paths::set_app_api_key(provider_id, previous_key.as_deref().unwrap_or_default());
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback) => anyhow::anyhow!(
                    "{error}; the previous credential also could not be restored: {rollback}"
                ),
            });
        }
        inner.settings = settings.clone();
        Ok(settings)
    }

    pub async fn reset_all_settings(&self) -> anyhow::Result<AppSettings> {
        let found = self.detect_credentials(None).await;
        let mut inner = self.inner.write().await;
        let mut settings = AppSettings::default();
        settings.seeded = true;
        settings.known_providers = self
            .providers
            .iter()
            .map(|provider| provider.info().id.clone())
            .collect();
        settings.api_key_configured = inner.settings.api_key_configured.clone();
        settings.legacy_api_keys = inner.settings.legacy_api_keys.clone();
        if !found.is_empty() {
            settings.enabled = found.into_iter().collect();
        }
        sanitize_settings(&mut settings, &self.providers);
        settings.save()?;
        inner.settings = settings.clone();
        Ok(settings)
    }

    /// Seconds until the background loop should fetch again. Zero once a refresh is due.
    pub async fn secs_until_refresh(&self) -> i64 {
        secs_until_refresh(&*self.inner.read().await)
    }

    /// Refresh every enabled provider. Returns whether snapshots were actually fetched, so a
    /// caller can skip the work that only matters when the data changed.
    pub async fn refresh_all(&self, force: bool) -> bool {
        let _refresh_guard = self.refresh_guard.lock().await;
        let enabled = {
            let mut inner = self.inner.write().await;
            if inner.refreshing {
                return false;
            }
            if !force && refresh_is_fresh(&inner) {
                return false;
            }
            inner.refreshing = true;
            inner.settings.enabled.clone()
        };

        let mut tasks = JoinSet::new();
        for provider in &self.providers {
            if !enabled.contains(&provider.info().id) {
                continue;
            }
            let provider = Arc::clone(provider);
            let http = self.http.clone();
            tasks.spawn(async move {
                let id = provider.info().id.clone();
                let snapshot = provider.refresh(&http).await;
                (id, snapshot)
            });
        }

        let mut updates = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(update) => updates.push(update),
                Err(error) => tracing::error!(%error, "provider refresh task failed"),
            }
        }

        let snapshots = {
            let mut inner = self.inner.write().await;
            for (id, snapshot) in updates {
                apply_refresh(&mut inner, id, snapshot);
            }
            inner.last_attempt = Some(Instant::now());
            inner.refreshing = false;
            inner.snapshots.clone()
        };
        if let Err(error) = save_cache(&snapshots) {
            tracing::error!(%error, "could not save usage cache");
        }
        true
    }

    pub async fn refresh_one(&self, id: &str, force: bool) -> Result<(), String> {
        let _refresh_guard = self.refresh_guard.lock().await;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.info().id == id)
            .cloned()
            .ok_or_else(|| "Unknown provider".to_string())?;
        if !force {
            let inner = self.inner.read().await;
            if inner.snapshots.get(id).is_some_and(snapshot_is_fresh) {
                return Ok(());
            }
        }
        let snapshot = provider.refresh(&self.http).await;
        let snapshots = {
            let mut inner = self.inner.write().await;
            apply_refresh(&mut inner, id.to_string(), snapshot);
            inner.snapshots.clone()
        };
        save_cache(&snapshots).map_err(|error| error.to_string())
    }

    pub async fn dashboard(&self) -> Dashboard {
        let inner = self.inner.read().await;
        let used_mode = inner.settings.show_usage_as == "used";
        let mut dashboard_providers = Vec::new();
        let mut pins = Vec::new();
        let mut spend_slices = Vec::new();
        let spend_label = period_label(&inner.settings.total_spend_period);
        let mut has_spend_provider = false;

        for provider_id in &inner.settings.order {
            let Some(provider) = self
                .providers
                .iter()
                .find(|provider| provider.info().id == *provider_id)
            else {
                continue;
            };
            let info = provider.info().clone();
            if !inner.settings.is_enabled(&info.id) {
                continue;
            }
            let snapshot = inner
                .snapshots
                .get(&info.id)
                .cloned()
                .map(snapshot_for_dashboard);
            let mut visible = Vec::new();
            let mut on_demand = Vec::new();
            for widget in provider.widgets() {
                if inner.settings.metric_hidden(&widget.id) {
                    continue;
                }
                let line = snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .lines
                        .iter()
                        .find(|line| {
                            line.label().eq_ignore_ascii_case(&widget.metric_label)
                                || line.label().eq_ignore_ascii_case(&widget.title)
                        })
                        .cloned()
                });
                let no_data = line.as_ref().is_none_or(|line| {
                    line.is_error()
                        || matches!(line, MetricLine::Badge { text, .. } if text == "No data" || text == "No usage data")
                });
                let rendered = RenderedWidget {
                    id: widget.id.clone(),
                    title: widget.title.clone(),
                    no_data,
                    pinned: inner.settings.is_pinned(&widget.id),
                    pace: line.as_ref().and_then(widget_pace),
                    line,
                };
                if rendered.pinned {
                    if let Some(line) = &rendered.line {
                        if !line.is_error() {
                            pins.push(Pin {
                                provider_id: info.id.clone(),
                                widget_id: widget.id.clone(),
                                title: format!("{} {}", info.display_name, widget.title),
                                text: format_line(line, used_mode),
                                used_ratio: used_ratio(line),
                                color: rendered.pace.as_ref().map(|pace| pace.color.clone()),
                            });
                        }
                    }
                }
                if inner.settings.metric_on_demand(&widget.id) {
                    on_demand.push(rendered);
                } else {
                    visible.push(rendered);
                }
            }

            let supports_spend = provider
                .widgets()
                .iter()
                .any(|widget| widget.is_spend_tile && widget.metric_label == spend_label);
            has_spend_provider |= supports_spend;
            if supports_spend {
                if let Some((dollars, tokens)) = snapshot
                    .as_ref()
                    .and_then(|snapshot| spend_from_lines(&snapshot.lines, spend_label))
                {
                    spend_slices.push(SpendSlice {
                        provider_id: info.id.clone(),
                        display_name: info.display_name.clone(),
                        dollars,
                        tokens,
                        color: provider_color(&info.id).to_string(),
                    });
                }
            }

            dashboard_providers.push(DashboardProvider {
                info,
                snapshot,
                enabled: true,
                expanded: inner.settings.expanded.contains(provider_id),
                widgets: visible,
                on_demand,
            });
        }

        let total_spend = if inner.settings.show_total_spend && has_spend_provider {
            let dollars = spend_slices.iter().map(|slice| slice.dollars).sum::<f64>();
            let tokens = spend_slices.iter().map(|slice| slice.tokens).sum::<f64>();
            let metric = inner.settings.total_spend_metric.clone();
            spend_slices.retain(|slice| match metric.as_str() {
                "tokens" => slice.tokens > 0.0,
                "cost_per_million" => slice.tokens > 0.0 && slice.dollars > 0.0,
                _ => slice.dollars > 0.0,
            });
            spend_slices.sort_by(|left, right| {
                slice_value(right, &metric).total_cmp(&slice_value(left, &metric))
            });
            Some(TotalSpend {
                period: inner.settings.total_spend_period.clone(),
                metric: metric.clone(),
                value: match metric.as_str() {
                    "tokens" => tokens,
                    "cost_per_million" if tokens > 0.0 => dollars / tokens * 1_000_000.0,
                    "cost_per_million" => 0.0,
                    _ => dollars,
                },
                dollars,
                tokens,
                slices: spend_slices,
            })
        } else {
            None
        };

        Dashboard {
            providers: dashboard_providers,
            pins,
            total_spend,
            next_refresh_in_secs: secs_until_refresh(&inner),
            refreshing: inner.refreshing,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    pub async fn notification_candidates(&self) -> Vec<UsageNotification> {
        let inner = self.inner.read().await;
        let mut notifications = Vec::new();
        for provider in &self.providers {
            if !inner.settings.is_enabled(&provider.info().id) {
                continue;
            }
            let Some(snapshot) = inner.snapshots.get(&provider.info().id) else {
                continue;
            };
            if snapshot.is_error()
                || chrono::Utc::now()
                    .signed_duration_since(snapshot.refreshed_at)
                    .num_seconds()
                    >= CACHE_TTL_SECS * 2
            {
                continue;
            }
            for widget in provider.widgets() {
                // A metric the user removed from the dashboard should not still interrupt them.
                if inner.settings.metric_hidden(&widget.id) {
                    continue;
                }
                let Some(MetricLine::Progress {
                    used,
                    limit,
                    resets_at,
                    period_duration_ms,
                    ..
                }) = snapshot.line(&widget.metric_label)
                else {
                    continue;
                };
                let Some(kind) = notification_kind(
                    *used,
                    *limit,
                    *resets_at,
                    *period_duration_ms,
                    &inner.settings,
                ) else {
                    continue;
                };
                let reset_key = resets_at
                    .map(|reset| reset.timestamp().to_string())
                    .unwrap_or_else(|| snapshot.refreshed_at.date_naive().to_string());
                let remaining = if *limit > 0.0 {
                    ((1.0 - used / limit).clamp(0.0, 1.0) * 100.0).round()
                } else {
                    0.0
                };
                let (title, body) = match kind {
                    "will_run_out" => (
                        format!("{} May Run Out", provider.info().display_name),
                        format!("{} is projected to run out before it resets.", widget.title),
                    ),
                    "cutting_close" => (
                        format!("{} Is Cutting It Close", provider.info().display_name),
                        format!(
                            "{} is projected to finish with little room left.",
                            widget.title
                        ),
                    ),
                    _ => (
                        format!("{} Is Almost Out", provider.info().display_name),
                        format!("{} has about {remaining:.0}% remaining.", widget.title),
                    ),
                };
                notifications.push(UsageNotification {
                    id: format!("{}:{}:{kind}", provider.info().id, widget.id),
                    window: reset_key,
                    title,
                    body,
                });
            }
        }
        notifications
    }

    pub async fn customize(&self) -> serde_json::Value {
        let inner = self.inner.read().await;
        let providers: Vec<_> = inner
            .settings
            .order
            .iter()
            .filter_map(|id| {
                self.providers
                    .iter()
                    .find(|provider| provider.info().id == *id)
            })
            .map(|provider| {
                json!({
                    "id": provider.info().id,
                    "displayName": provider.info().display_name,
                    "icon": provider.info().icon,
                    "enabled": inner.settings.is_enabled(&provider.info().id),
                    "widgets": provider.widgets(),
                })
            })
            .collect();
        json!({ "providers": providers, "settings": inner.settings })
    }

    pub async fn limits_json(&self, filter: Option<&str>) -> Option<serde_json::Value> {
        let inner = self.inner.read().await;
        if filter.is_some_and(|id| !self.provider_ids().iter().any(|known| known == id)) {
            return None;
        }
        let providers: Vec<_> = self
            .providers
            .iter()
            .filter(|provider| {
                filter.map_or_else(
                    || inner.settings.is_enabled(&provider.info().id),
                    |id| provider.info().id == id,
                )
            })
            .map(|provider| ProviderLimits {
                info: provider.info(),
                widgets: provider.widgets(),
                snapshot: inner.snapshots.get(&provider.info().id),
            })
            .collect();
        Some(limits::envelope(&providers, &inner.errors))
    }

    pub async fn usage_json(&self, filter: Option<&str>) -> Option<serde_json::Value> {
        let inner = self.inner.read().await;
        if filter.is_some_and(|id| !self.provider_ids().iter().any(|known| known == id)) {
            return None;
        }
        let snapshots: Vec<_> = self
            .providers
            .iter()
            .filter(|provider| {
                filter.map_or_else(
                    || inner.settings.is_enabled(&provider.info().id),
                    |id| provider.info().id == id,
                )
            })
            .filter_map(|provider| inner.snapshots.get(&provider.info().id))
            .cloned()
            .map(ProviderSnapshot::with_current_staleness)
            .map(|snapshot| legacy::snapshot(&snapshot))
            .collect();
        Some(json!(snapshots))
    }

    /// Build an engine over fixed providers, settings and snapshots. Test-only: it reads and
    /// writes nothing under the user's real configuration directory.
    #[cfg(test)]
    pub(crate) fn for_test(
        providers: Vec<Arc<dyn Provider>>,
        settings: AppSettings,
        snapshots: HashMap<String, ProviderSnapshot>,
    ) -> Self {
        Self {
            http: Http::with_proxy(None).expect("test HTTP client"),
            providers,
            inner: RwLock::new(Inner {
                settings,
                snapshots,
                errors: HashMap::new(),
                last_attempt: None,
                refreshing: false,
            }),
            refresh_guard: Mutex::new(()),
        }
    }

    /// One-line-per-pin summary for the tray tooltip, so the numbers are readable on hover
    /// without opening the flyout. Capped because the Windows shell truncates a long tooltip.
    pub async fn tray_summary(&self) -> String {
        const MAX_TOOLTIP_CHARS: usize = 120;
        let dashboard = self.dashboard().await;
        let mut summary = String::from("MultiMeters");
        for pin in &dashboard.pins {
            let line = format!("\n{} — {}", pin.title, pin.text);
            if summary.chars().count() + line.chars().count() > MAX_TOOLTIP_CHARS {
                break;
            }
            summary.push_str(&line);
        }
        summary
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|provider| provider.info().id.clone())
            .collect()
    }
}

fn apply_refresh(inner: &mut Inner, id: String, snapshot: ProviderSnapshot) {
    if let Some(message) = snapshot.error_message().map(str::to_owned) {
        inner.errors.insert(id.clone(), message.clone());
        if let Some(previous) = inner
            .snapshots
            .get_mut(&id)
            .filter(|snapshot| !snapshot.is_error())
        {
            previous.warning = Some(message);
            previous.stale = true;
        } else {
            inner.snapshots.insert(id, snapshot);
        }
    } else {
        inner.errors.remove(&id);
        inner.snapshots.insert(id, snapshot);
    }
}

fn snapshot_for_dashboard(mut snapshot: ProviderSnapshot) -> ProviderSnapshot {
    if snapshot.warning.is_none() {
        snapshot.warning = snapshot.error_message().map(ToOwned::to_owned);
    }
    snapshot.with_current_staleness()
}

fn snapshot_is_fresh(snapshot: &ProviderSnapshot) -> bool {
    !snapshot.is_error()
        && !snapshot.stale
        && chrono::Utc::now()
            .signed_duration_since(snapshot.refreshed_at)
            .num_seconds()
            < CACHE_TTL_SECS
}

/// Pace a progress meter. A line that carries its own colour keeps it — a provider-supplied
/// colour is a deliberate override, not something to project over.
fn widget_pace(line: &MetricLine) -> Option<Pace> {
    let MetricLine::Progress {
        used,
        limit,
        resets_at,
        period_duration_ms,
        color_hex,
        ..
    } = line
    else {
        return None;
    };
    let mut verdict = pace(*used, *limit, *resets_at, *period_duration_ms);
    if let Some(color) = color_hex {
        verdict.color = color.clone();
    }
    Some(verdict)
}

fn secs_until_refresh(inner: &Inner) -> i64 {
    let interval = inner.settings.refresh_interval_secs() as i64;
    let elapsed = inner
        .last_attempt
        .map(|attempt| attempt.elapsed().as_secs() as i64)
        .unwrap_or(interval);
    (interval - elapsed).max(0)
}

fn refresh_is_fresh(inner: &Inner) -> bool {
    if let Some(last_attempt) = inner.last_attempt {
        return last_attempt.elapsed()
            < Duration::from_secs(inner.settings.refresh_interval_secs());
    }
    !inner.settings.enabled.is_empty()
        && inner
            .settings
            .enabled
            .iter()
            .all(|id| inner.snapshots.get(id).is_some_and(snapshot_is_fresh))
}

fn period_label(period: &str) -> &'static str {
    match period {
        "yesterday" => "Yesterday",
        "last30" => "Last 30 Days",
        _ => "Today",
    }
}

fn slice_value(slice: &SpendSlice, metric: &str) -> f64 {
    match metric {
        "tokens" => slice.tokens,
        "cost_per_million" if slice.tokens > 0.0 => slice.dollars / slice.tokens * 1_000_000.0,
        "cost_per_million" => 0.0,
        _ => slice.dollars,
    }
}

fn notification_kind(
    used: f64,
    limit: f64,
    resets_at: Option<chrono::DateTime<chrono::Utc>>,
    period_duration_ms: Option<i64>,
    settings: &AppSettings,
) -> Option<&'static str> {
    if limit <= 0.0 || !used.is_finite() || !limit.is_finite() {
        return None;
    }
    let remaining = (1.0 - used / limit).clamp(0.0, 1.0);
    let status = pace(used, limit, resets_at, period_duration_ms).status;
    if settings.notify_will_run_out
        && matches!(status, PaceStatus::RunOut | PaceStatus::Empty)
        && resets_at.is_some()
    {
        Some("will_run_out")
    } else if settings.notify_almost_out && remaining <= 0.1 {
        Some("almost_out")
    } else if settings.notify_cutting_it_close && status == PaceStatus::Close {
        Some("cutting_close")
    } else {
        None
    }
}

fn load_cache() -> HashMap<String, ProviderSnapshot> {
    match paths::read_json_with_backup(&paths::cache_path()) {
        Ok(Some(cache)) => cache,
        Ok(None) => HashMap::new(),
        Err(error) => {
            tracing::error!(%error, "could not load usage cache");
            HashMap::new()
        }
    }
}

fn save_cache(snapshots: &HashMap<String, ProviderSnapshot>) -> anyhow::Result<()> {
    paths::write_json_atomic(&paths::cache_path(), snapshots)
}

fn sanitize_settings(settings: &mut AppSettings, providers: &[Arc<dyn Provider>]) {
    let known_providers: HashSet<String> = providers
        .iter()
        .map(|provider| provider.info().id.clone())
        .collect();
    let widget_owner: HashMap<String, (&str, bool)> = providers
        .iter()
        .flat_map(|provider| {
            provider.widgets().iter().map(move |widget| {
                (
                    widget.id.clone(),
                    (provider.info().id.as_str(), widget.pinnable),
                )
            })
        })
        .collect();

    settings.enabled.retain(|id| known_providers.contains(id));
    let mut seen = HashSet::new();
    settings
        .order
        .retain(|id| known_providers.contains(id) && seen.insert(id.clone()));
    for provider in providers {
        if seen.insert(provider.info().id.clone()) {
            settings.order.push(provider.info().id.clone());
        }
    }
    settings
        .hidden_metrics
        .retain(|id| widget_owner.contains_key(id));
    settings
        .on_demand
        .retain(|id| widget_owner.contains_key(id));
    settings.expanded.retain(|id| known_providers.contains(id));

    if !settings.seeded && settings.hidden_metrics.is_empty() {
        for provider in providers {
            for widget in provider
                .widgets()
                .iter()
                .filter(|widget| !widget.default_on)
            {
                settings.hidden_metrics.insert(widget.id.clone());
            }
        }
    }

    settings.pinned.retain(|provider_id, widgets| {
        if !known_providers.contains(provider_id) {
            return false;
        }
        let mut seen_widgets = HashSet::new();
        widgets.retain(|widget_id| {
            widget_owner
                .get(widget_id)
                .is_some_and(|(owner, pinnable)| {
                    *owner == provider_id && *pinnable && seen_widgets.insert(widget_id.clone())
                })
        });
        widgets.truncate(MAX_PINS_PER_PROVIDER);
        !widgets.is_empty()
    });

    for provider in providers {
        let visible: Vec<_> = provider
            .widgets()
            .iter()
            .filter(|widget| !settings.hidden_metrics.contains(&widget.id))
            .collect();
        if !visible.is_empty()
            && visible
                .iter()
                .all(|widget| settings.on_demand.contains(&widget.id))
        {
            settings.on_demand.remove(&visible[0].id);
        }
    }
}

fn repair_loaded_settings(settings: &mut AppSettings) -> bool {
    let defaults = AppSettings::default();
    let mut repaired = false;
    macro_rules! repair_value {
        ($field:ident, $valid:expr) => {
            if !$valid {
                tracing::warn!(
                    setting = stringify!($field),
                    "invalid saved setting was restored to its default"
                );
                settings.$field = defaults.$field.clone();
                repaired = true;
            }
        };
    }
    repair_value!(
        theme,
        matches!(settings.theme.as_str(), "system" | "dark" | "light")
    );
    repair_value!(
        density,
        matches!(settings.density.as_str(), "compact" | "comfortable")
    );
    repair_value!(
        time_format,
        matches!(settings.time_format.as_str(), "auto" | "12" | "24")
    );
    repair_value!(
        show_usage_as,
        matches!(settings.show_usage_as.as_str(), "left" | "used")
    );
    repair_value!(
        reset_times,
        matches!(settings.reset_times.as_str(), "countdown" | "exact")
    );
    repair_value!(
        refresh_interval_minutes,
        matches!(settings.refresh_interval_minutes, 1 | 5 | 15 | 30 | 60)
    );
    repair_value!(
        total_spend_period,
        matches!(
            settings.total_spend_period.as_str(),
            "today" | "yesterday" | "last30"
        )
    );
    repair_value!(
        total_spend_metric,
        matches!(
            settings.total_spend_metric.as_str(),
            "cost" | "tokens" | "cost_per_million"
        )
    );
    repair_value!(
        global_shortcut,
        settings
            .global_shortcut
            .as_ref()
            .is_none_or(|shortcut| shortcut.len() <= 80)
    );
    repaired
}

fn validate_patch_keys(patch: &serde_json::Value) -> anyhow::Result<()> {
    const MUTABLE_KEYS: &[&str] = &[
        "enabled",
        "order",
        "hidden_metrics",
        "on_demand",
        "pinned",
        "expanded",
        "show_total_spend",
        "hide_on_blur",
        "launch_at_login",
        "global_shortcut",
        "theme",
        "density",
        "reduce_animations",
        "time_format",
        "show_usage_as",
        "reset_times",
        "always_show_pacing",
        "refresh_interval_minutes",
        "total_spend_period",
        "total_spend_metric",
        "notify_almost_out",
        "notify_cutting_it_close",
        "notify_will_run_out",
    ];
    let object = patch
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("settings patch must be an object"))?;
    if let Some(key) = object
        .keys()
        .find(|key| !MUTABLE_KEYS.contains(&key.as_str()))
    {
        anyhow::bail!("setting `{key}` cannot be changed through this command");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};

    #[test]
    fn failed_refresh_preserves_last_good_snapshot() {
        let info = ProviderInfo {
            id: "claude".into(),
            display_name: "Claude".into(),
            icon: "claude".into(),
            links: vec![],
        };
        let good = ProviderSnapshot::ok(
            &info,
            Some("Pro".into()),
            vec![MetricLine::percent("Session", 40.0, None, SESSION_MS)],
        );
        let fetched_at = good.refreshed_at;
        let mut inner = Inner {
            settings: AppSettings::default(),
            snapshots: HashMap::from([("claude".into(), good)]),
            errors: HashMap::new(),
            last_attempt: None,
            refreshing: false,
        };
        apply_refresh(
            &mut inner,
            "claude".into(),
            ProviderSnapshot::err(&info, "offline"),
        );
        let preserved = &inner.snapshots["claude"];
        assert_eq!(preserved.refreshed_at, fetched_at);
        assert_eq!(preserved.plan.as_deref(), Some("Pro"));
        assert_eq!(preserved.warning.as_deref(), Some("offline"));
        assert!(snapshot_for_dashboard(preserved.clone()).stale);
        assert_eq!(inner.errors["claude"], "offline");
    }

    #[test]
    fn initial_provider_errors_are_visible_on_the_dashboard() {
        let info = ProviderInfo {
            id: "zai".into(),
            display_name: "Z.ai".into(),
            icon: "zai".into(),
            links: vec![],
        };
        let snapshot = snapshot_for_dashboard(ProviderSnapshot::err(&info, "Sign in again"));
        assert_eq!(snapshot.warning.as_deref(), Some("Sign in again"));
    }

    #[test]
    fn old_snapshots_are_not_cache_fresh() {
        let info = ProviderInfo {
            id: "codex".into(),
            display_name: "Codex".into(),
            icon: "codex".into(),
            links: vec![],
        };
        let mut snapshot = ProviderSnapshot::ok(&info, None, vec![MetricLine::no_data()]);
        snapshot.refreshed_at = Utc::now() - ChronoDuration::seconds(CACHE_TTL_SECS + 1);
        assert!(!snapshot_is_fresh(&snapshot));
        snapshot.refreshed_at = Utc::now();
        snapshot.stale = true;
        assert!(!snapshot_is_fresh(&snapshot));
    }

    #[test]
    fn rejects_privileged_settings_patch_fields() {
        assert!(validate_patch_keys(&json!({ "seeded": false })).is_err());
        assert!(validate_patch_keys(&json!({ "theme": "dark" })).is_ok());
    }

    #[test]
    fn repairs_invalid_saved_values_without_weakening_patch_validation() {
        let mut settings = AppSettings {
            theme: "ultraviolet".into(),
            refresh_interval_minutes: 2,
            total_spend_metric: "watts".into(),
            ..AppSettings::default()
        };
        assert!(settings.validate_values().is_err());
        assert!(repair_loaded_settings(&mut settings));
        assert_eq!(settings.theme, "system");
        assert_eq!(settings.refresh_interval_minutes, 5);
        assert_eq!(settings.total_spend_metric, "cost");
        assert!(settings.validate_values().is_ok());
    }

    #[test]
    fn notification_priority_avoids_duplicate_categories() {
        let settings = AppSettings {
            notify_almost_out: true,
            notify_will_run_out: true,
            ..AppSettings::default()
        };
        let reset = Utc::now() + ChronoDuration::hours(1);
        assert_eq!(
            notification_kind(95.0, 100.0, Some(reset), Some(SESSION_MS), &settings),
            Some("will_run_out")
        );
    }

    /// A provider whose snapshot is supplied up front, so engine assembly can be tested without
    /// reaching a network or the user's machine.
    struct StubProvider {
        info: ProviderInfo,
        widgets: Vec<WidgetDescriptor>,
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn info(&self) -> &ProviderInfo {
            &self.info
        }
        fn widgets(&self) -> &[WidgetDescriptor] {
            &self.widgets
        }
        async fn has_local_credentials(&self) -> bool {
            false
        }
        async fn refresh(&self, _http: &Http) -> ProviderSnapshot {
            ProviderSnapshot::ok(&self.info, None, vec![])
        }
    }

    fn stub_info() -> ProviderInfo {
        ProviderInfo {
            id: "claude".into(),
            display_name: "Claude".into(),
            icon: "claude".into(),
            links: vec![],
        }
    }

    fn stub_widget(id: &str, title: &str) -> WidgetDescriptor {
        WidgetDescriptor {
            id: id.into(),
            provider_id: "claude".into(),
            title: title.into(),
            metric_label: title.into(),
            pinnable: true,
            is_spend_tile: false,
            default_on: true,
        }
    }

    fn stub_engine(settings: AppSettings) -> AppEngine {
        let info = stub_info();
        let snapshot = ProviderSnapshot::ok(
            &info,
            Some("Max".into()),
            vec![
                MetricLine::percent("Session", 96.0, None, SESSION_MS),
                MetricLine::percent("Weekly", 20.0, None, WEEK_MS),
            ],
        );
        AppEngine::for_test(
            vec![Arc::new(StubProvider {
                info,
                widgets: vec![
                    stub_widget("claude.session", "Session"),
                    stub_widget("claude.weekly", "Weekly"),
                ],
            })],
            settings,
            HashMap::from([("claude".to_string(), snapshot)]),
        )
    }

    fn stub_settings() -> AppSettings {
        AppSettings {
            enabled: BTreeSet::from(["claude".to_string()]),
            order: vec!["claude".into()],
            hidden_metrics: BTreeSet::new(),
            on_demand: BTreeSet::new(),
            pinned: std::collections::BTreeMap::new(),
            notify_almost_out: true,
            ..AppSettings::default()
        }
    }

    #[tokio::test]
    async fn hidden_metrics_do_not_raise_notifications() {
        let mut settings = stub_settings();
        let engine = stub_engine(settings.clone());
        let ids: Vec<_> = engine
            .notification_candidates()
            .await
            .into_iter()
            .map(|notification| notification.id)
            .collect();
        assert!(
            ids.iter().any(|id| id.contains("claude.session")),
            "a visible metric past the threshold should notify, got {ids:?}"
        );

        settings.hidden_metrics = BTreeSet::from(["claude.session".to_string()]);
        assert!(
            stub_engine(settings)
                .notification_candidates()
                .await
                .is_empty(),
            "a metric the user removed from the dashboard must not notify"
        );
    }

    #[tokio::test]
    async fn hidden_metrics_stay_off_the_dashboard() {
        let mut settings = stub_settings();
        settings.hidden_metrics = BTreeSet::from(["claude.weekly".to_string()]);
        settings.on_demand = BTreeSet::from(["claude.session".to_string()]);
        let dashboard = stub_engine(settings).dashboard().await;
        let provider = &dashboard.providers[0];
        assert!(provider.widgets.is_empty());
        assert_eq!(provider.on_demand.len(), 1);
        assert_eq!(provider.on_demand[0].id, "claude.session");
        assert_eq!(
            provider.snapshot.as_ref().unwrap().plan.as_deref(),
            Some("Max")
        );
    }

    #[tokio::test]
    async fn the_tray_tooltip_lists_pins_and_stays_within_the_shell_limit() {
        let mut settings = stub_settings();
        settings.pinned = std::collections::BTreeMap::from([(
            "claude".to_string(),
            vec!["claude.session".to_string(), "claude.weekly".to_string()],
        )]);
        let summary = stub_engine(settings).tray_summary().await;
        assert_eq!(
            summary,
            "MultiMeters\nClaude Session — 4% left\nClaude Weekly — 80% left"
        );
        assert!(summary.chars().count() <= 120);
    }

    #[tokio::test]
    async fn the_tray_tooltip_falls_back_to_the_app_name_without_pins() {
        assert_eq!(
            stub_engine(stub_settings()).tray_summary().await,
            "MultiMeters"
        );
    }

    #[tokio::test]
    async fn pinned_widgets_surface_with_their_used_ratio() {
        let mut settings = stub_settings();
        settings.pinned = std::collections::BTreeMap::from([(
            "claude".to_string(),
            vec!["claude.weekly".to_string()],
        )]);
        let dashboard = stub_engine(settings).dashboard().await;
        assert_eq!(dashboard.pins.len(), 1);
        assert_eq!(dashboard.pins[0].title, "Claude Weekly");
        assert_eq!(dashboard.pins[0].text, "80% left");
        assert_eq!(dashboard.pins[0].used_ratio, Some(0.2));
    }

    #[test]
    fn the_alert_log_repeats_across_windows_but_not_within_one() {
        let alert = |window: &str| UsageNotification {
            id: "claude:claude.session:almost_out".into(),
            window: window.into(),
            title: "Claude Is Almost Out".into(),
            body: "Session has about 6% remaining.".into(),
        };
        let mut log = AlertLog::default();
        assert!(log.should_deliver(&alert("1")));
        assert!(
            !log.should_deliver(&alert("1")),
            "one alert per reset window"
        );
        assert!(
            log.should_deliver(&alert("2")),
            "the next window fires again"
        );
        assert_eq!(log.len(), 1, "rolling windows must not grow the log");
    }

    #[test]
    fn shortening_the_refresh_interval_brings_the_next_run_forward() {
        let mut inner = Inner {
            settings: AppSettings {
                refresh_interval_minutes: 60,
                ..AppSettings::default()
            },
            snapshots: HashMap::new(),
            errors: HashMap::new(),
            last_attempt: Some(Instant::now()),
            refreshing: false,
        };
        assert!(secs_until_refresh(&inner) > 3_000);
        inner.settings.refresh_interval_minutes = 1;
        let due_in = secs_until_refresh(&inner);
        assert!(
            (0..=60).contains(&due_in),
            "a shortened interval must shorten the wait, got {due_in}"
        );
    }
}
