use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub enabled: BTreeSet<String>,
    pub order: Vec<String>,
    pub hidden_metrics: BTreeSet<String>,
    pub on_demand: BTreeSet<String>,
    pub pinned: BTreeMap<String, Vec<String>>,
    pub expanded: BTreeSet<String>,
    pub show_total_spend: bool,
    pub launch_at_login: bool,
    pub global_shortcut: Option<String>,
    pub theme: String,
    pub density: String,
    pub reduce_animations: bool,
    pub time_format: String,
    pub show_usage_as: String,
    pub reset_times: String,
    pub always_show_pacing: bool,
    pub refresh_interval_minutes: u64,
    pub total_spend_period: String,
    pub total_spend_metric: String,
    pub notify_almost_out: bool,
    pub notify_cutting_it_close: bool,
    pub notify_will_run_out: bool,
    pub seeded: bool,
    pub known_providers: BTreeSet<String>,
    pub api_key_configured: BTreeSet<String>,
    #[serde(default, rename = "api_keys", skip_serializing)]
    pub(crate) legacy_api_keys: BTreeMap<String, String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            enabled: BTreeSet::from(["claude".into(), "codex".into(), "cursor".into()]),
            order: vec![
                "claude".into(),
                "codex".into(),
                "cursor".into(),
                "antigravity".into(),
                "copilot".into(),
                "devin".into(),
                "grok".into(),
                "opencode".into(),
                "openrouter".into(),
                "zai".into(),
            ],
            hidden_metrics: BTreeSet::new(),
            on_demand: default_on_demand(),
            pinned: default_pins(),
            expanded: BTreeSet::new(),
            show_total_spend: true,
            launch_at_login: false,
            global_shortcut: Some("Ctrl+Shift+M".into()),
            theme: "system".into(),
            density: "compact".into(),
            reduce_animations: false,
            time_format: "auto".into(),
            show_usage_as: "left".into(),
            reset_times: "countdown".into(),
            always_show_pacing: false,
            refresh_interval_minutes: 5,
            total_spend_period: "today".into(),
            total_spend_metric: "cost".into(),
            notify_almost_out: false,
            notify_cutting_it_close: false,
            notify_will_run_out: false,
            seeded: false,
            known_providers: BTreeSet::new(),
            api_key_configured: BTreeSet::new(),
            legacy_api_keys: BTreeMap::new(),
        }
    }
}

fn default_on_demand() -> BTreeSet<String> {
    [
        "antigravity.claude",
        "antigravity.claudeWeekly",
        "claude.sonnet",
        "claude.fable",
        "claude.today",
        "claude.yesterday",
        "claude.last30",
        "codex.spark",
        "codex.sparkWeekly",
        "codex.credits",
        "codex.rateLimitResets",
        "codex.today",
        "codex.yesterday",
        "codex.last30",
        "cursor.onDemand",
        "cursor.requests",
        "cursor.credits",
        "cursor.today",
        "cursor.yesterday",
        "cursor.last30",
        "copilot.orgCredits",
        "copilot.orgSpend",
        "copilot.chat",
        "copilot.completions",
        "devin.extra",
        "grok.payAsYouGo",
        "grok.today",
        "grok.yesterday",
        "grok.last30",
        "opencode.today",
        "opencode.yesterday",
        "opencode.last30",
        "openrouter.today",
        "openrouter.week",
        "openrouter.month",
        "openrouter.keyLimit",
        "zai.webSearches",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_pins() -> BTreeMap<String, Vec<String>> {
    let mut m = BTreeMap::new();
    m.insert(
        "claude".into(),
        vec!["claude.session".into(), "claude.weekly".into()],
    );
    m.insert(
        "codex".into(),
        vec!["codex.session".into(), "codex.weekly".into()],
    );
    m.insert(
        "cursor".into(),
        vec!["cursor.auto".into(), "cursor.api".into()],
    );
    m.insert("copilot".into(), vec!["copilot.premium".into()]);
    m.insert("openrouter".into(), vec!["openrouter.credits".into()]);
    m.insert(
        "zai".into(),
        vec!["zai.session".into(), "zai.weekly".into()],
    );
    m.insert(
        "antigravity".into(),
        vec![
            "antigravity.geminiPro".into(),
            "antigravity.geminiWeekly".into(),
        ],
    );
    m
}

impl AppSettings {
    pub fn load() -> Self {
        let path = paths::settings_path();
        let mut settings = match paths::read_json_with_backup::<Self>(&path) {
            Ok(Some(settings)) => settings,
            Ok(None) => Self::default(),
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "could not load settings; using defaults");
                Self::default()
            }
        };
        let migrated_legacy_keys = settings.migrate_legacy_api_keys();
        for provider in ["openrouter", "zai"] {
            match paths::app_api_key_checked(provider) {
                Ok(Some(_)) => {
                    settings.api_key_configured.insert(provider.to_string());
                }
                Ok(None) if !settings.legacy_api_keys.contains_key(provider) => {
                    settings.api_key_configured.remove(provider);
                }
                Ok(None) => {}
                Err(error) => {
                    // Retain the last-known configured marker. A locked credential manager must not
                    // make the UI claim that a saved key disappeared.
                    tracing::warn!(%provider, %error, "could not verify the saved API key");
                }
            }
        }
        if migrated_legacy_keys {
            if let Err(error) = settings.save() {
                tracing::warn!(%error, "could not persist API-key credential-store migration");
            }
        }
        settings
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = paths::settings_path();
        let mut value = serde_json::to_value(self)?;
        if !self.legacy_api_keys.is_empty() {
            value["api_keys"] = serde_json::to_value(&self.legacy_api_keys)?;
        }
        paths::write_json_atomic(&path, &value)
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    pub fn metric_hidden(&self, id: &str) -> bool {
        self.hidden_metrics.contains(id)
    }

    pub fn metric_on_demand(&self, id: &str) -> bool {
        self.on_demand.contains(id)
    }

    pub fn is_pinned(&self, widget_id: &str) -> bool {
        self.pinned
            .values()
            .any(|v| v.iter().any(|x| x == widget_id))
    }

    pub fn toggle_pin(&mut self, provider_id: &str, widget_id: &str) -> Result<(), String> {
        let list = self.pinned.entry(provider_id.to_string()).or_default();
        if let Some(i) = list.iter().position(|x| x == widget_id) {
            list.remove(i);
            return Ok(());
        }
        if list.len() >= crate::models::MAX_PINS_PER_PROVIDER {
            return Err("Up to 2 pins per provider".into());
        }
        list.push(widget_id.to_string());
        Ok(())
    }

    pub fn refresh_interval_secs(&self) -> u64 {
        self.refresh_interval_minutes.clamp(1, 60) * 60
    }

    pub fn validate_values(&self) -> Result<(), String> {
        if !matches!(self.theme.as_str(), "system" | "dark" | "light") {
            return Err("Theme must be System, Dark, or Light".into());
        }
        if !matches!(self.density.as_str(), "compact" | "comfortable") {
            return Err("Density must be Compact or Comfortable".into());
        }
        if !matches!(self.time_format.as_str(), "auto" | "12" | "24") {
            return Err("Time format must be Auto, 12-hour, or 24-hour".into());
        }
        if !matches!(self.show_usage_as.as_str(), "left" | "used") {
            return Err("Usage display must be Left or Used".into());
        }
        if !matches!(self.reset_times.as_str(), "countdown" | "exact") {
            return Err("Reset display must be Countdown or Exact".into());
        }
        if !matches!(self.refresh_interval_minutes, 1 | 5 | 15 | 30 | 60) {
            return Err("Refresh interval must be 1, 5, 15, 30, or 60 minutes".into());
        }
        if !matches!(
            self.total_spend_period.as_str(),
            "today" | "yesterday" | "last30"
        ) {
            return Err("Total spend period is invalid".into());
        }
        if !matches!(
            self.total_spend_metric.as_str(),
            "cost" | "tokens" | "cost_per_million"
        ) {
            return Err("Total spend metric is invalid".into());
        }
        if self
            .global_shortcut
            .as_ref()
            .is_some_and(|shortcut| shortcut.len() > 80)
        {
            return Err("Global shortcut is too long".into());
        }
        Ok(())
    }

    fn migrate_legacy_api_keys(&mut self) -> bool {
        let mut changed = false;
        let keys = self.legacy_api_keys.clone();
        for (provider, key) in keys {
            if key.trim().is_empty() {
                self.legacy_api_keys.remove(&provider);
                changed = true;
                continue;
            }
            match paths::set_app_api_key(&provider, &key) {
                Ok(()) => {
                    self.api_key_configured.insert(provider.clone());
                    self.legacy_api_keys.remove(&provider);
                    changed = true;
                    tracing::info!(%provider, "migrated API key to the OS credential store");
                }
                Err(error) => {
                    tracing::warn!(%provider, %error, "could not migrate legacy API key to the OS credential store");
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_user_selectable_values() {
        let mut settings = AppSettings::default();
        assert!(settings.validate_values().is_ok());
        settings.refresh_interval_minutes = 2;
        assert!(settings.validate_values().is_err());
        settings.refresh_interval_minutes = 5;
        settings.total_spend_metric = "watts".into();
        assert!(settings.validate_values().is_err());
    }

    #[test]
    fn settings_files_from_older_releases_take_the_current_defaults() {
        // A file written before a field existed must adopt the field's intended default, not
        // `bool::default()`. Container-level `#[serde(default)]` is what makes that true.
        let older = r#"{"theme":"dark","refresh_interval_minutes":15}"#;
        let settings: AppSettings = serde_json::from_str(older).expect("older settings parse");
        assert_eq!(settings.theme, "dark", "saved values still win");
        assert_eq!(settings.refresh_interval_minutes, 15);
        assert!(settings.show_total_spend);
        assert_eq!(settings.global_shortcut.as_deref(), Some("Ctrl+Shift+M"));
        assert!(
            !settings.enabled.is_empty(),
            "providers fall back to the default set"
        );
        assert!(settings.validate_values().is_ok());
    }

    #[test]
    fn enforces_pin_limit_and_allows_unpinning() {
        let mut settings = AppSettings::default();
        settings.pinned.clear();
        settings.toggle_pin("claude", "one").unwrap();
        settings.toggle_pin("claude", "two").unwrap();
        assert!(settings.toggle_pin("claude", "three").is_err());
        settings.toggle_pin("claude", "one").unwrap();
        assert_eq!(settings.pinned["claude"], vec!["two"]);
    }
}
