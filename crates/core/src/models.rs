use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SESSION_MS: i64 = 5 * 60 * 60 * 1000;
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const MONTH_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const CACHE_TTL_SECS: i64 = 5 * 60;
pub const MAX_PINS_PER_PROVIDER: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Percent,
    Dollars,
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressFormat {
    Percent,
    Dollars,
    Count { suffix: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub number: f64,
    pub kind: MetricKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartPoint {
    pub value: f64,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetricLine {
    Text {
        label: String,
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        color_hex: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
    },
    Values {
        label: String,
        values: Vec<MetricValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        color_hex: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        expiries_at: Vec<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unknown_models: Vec<String>,
    },
    Progress {
        label: String,
        used: f64,
        limit: f64,
        format: ProgressFormat,
        #[serde(skip_serializing_if = "Option::is_none")]
        resets_at: Option<DateTime<Utc>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        period_duration_ms: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        color_hex: Option<String>,
    },
    Badge {
        label: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        color_hex: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
    },
    Chart {
        label: String,
        points: Vec<ChartPoint>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl MetricLine {
    pub fn label(&self) -> &str {
        match self {
            Self::Text { label, .. }
            | Self::Values { label, .. }
            | Self::Progress { label, .. }
            | Self::Badge { label, .. }
            | Self::Chart { label, .. } => label,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Badge { label, .. } if label == "Error")
    }

    pub fn percent(
        label: &str,
        used: f64,
        resets_at: Option<DateTime<Utc>>,
        period_ms: i64,
    ) -> Self {
        Self::Progress {
            label: label.to_string(),
            used: used.clamp(0.0, 100.0),
            limit: 100.0,
            format: ProgressFormat::Percent,
            resets_at,
            period_duration_ms: Some(period_ms),
            color_hex: None,
        }
    }

    pub fn dollars_progress(
        label: &str,
        used: f64,
        limit: f64,
        resets_at: Option<DateTime<Utc>>,
        period_ms: i64,
    ) -> Self {
        Self::Progress {
            label: label.to_string(),
            used,
            limit,
            format: ProgressFormat::Dollars,
            resets_at,
            period_duration_ms: Some(period_ms),
            color_hex: None,
        }
    }

    pub fn values_dollars(label: &str, amount: f64) -> Self {
        Self::Values {
            label: label.to_string(),
            values: vec![MetricValue {
                number: amount,
                kind: MetricKind::Dollars,
                label: None,
                estimated: false,
            }],
            color_hex: None,
            expiries_at: vec![],
            unknown_models: vec![],
        }
    }

    pub fn spend(label: &str, dollars: f64, tokens: f64, estimated: bool) -> Self {
        Self::Values {
            label: label.to_string(),
            values: vec![
                MetricValue {
                    number: dollars,
                    kind: MetricKind::Dollars,
                    label: None,
                    estimated,
                },
                MetricValue {
                    number: tokens,
                    kind: MetricKind::Count,
                    label: Some("tokens".into()),
                    estimated: false,
                },
            ],
            color_hex: None,
            expiries_at: vec![],
            unknown_models: vec![],
        }
    }

    pub fn badge(label: &str, text: &str, color: Option<&str>) -> Self {
        Self::Badge {
            label: label.to_string(),
            text: text.to_string(),
            color_hex: color.map(|s| s.to_string()),
            subtitle: None,
        }
    }

    pub fn error(message: &str) -> Self {
        Self::badge("Error", message, Some("#EF4444"))
    }

    pub fn no_data() -> Self {
        Self::badge("Status", "No usage data", Some("#A3A3A3"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub links: Vec<ProviderLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetDescriptor {
    pub id: String,
    pub provider_id: String,
    pub title: String,
    pub metric_label: String,
    pub pinnable: bool,
    pub is_spend_tile: bool,
    pub default_on: bool,
    pub default_on_demand: bool,
    pub default_pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub provider_id: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub lines: Vec<MetricLine>,
    pub refreshed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub stale: bool,
}

impl ProviderSnapshot {
    pub fn ok(info: &ProviderInfo, plan: Option<String>, mut lines: Vec<MetricLine>) -> Self {
        if lines.is_empty() {
            lines.push(MetricLine::no_data());
        }
        Self {
            provider_id: info.id.clone(),
            display_name: info.display_name.clone(),
            plan,
            lines,
            refreshed_at: Utc::now(),
            warning: None,
            stale: false,
        }
    }

    pub fn err(info: &ProviderInfo, message: &str) -> Self {
        Self {
            provider_id: info.id.clone(),
            display_name: info.display_name.clone(),
            plan: None,
            lines: vec![MetricLine::error(message)],
            refreshed_at: Utc::now(),
            warning: None,
            stale: false,
        }
    }

    pub fn line(&self, label: &str) -> Option<&MetricLine> {
        self.lines
            .iter()
            .find(|l| l.label().eq_ignore_ascii_case(label))
    }

    pub fn error_message(&self) -> Option<&str> {
        self.lines.iter().find_map(|line| match line {
            MetricLine::Badge { label, text, .. } if label == "Error" => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn is_error(&self) -> bool {
        self.error_message().is_some()
    }

    pub fn with_current_staleness(mut self) -> Self {
        self.stale |= Utc::now()
            .signed_duration_since(self.refreshed_at)
            .num_seconds()
            >= CACHE_TTL_SECS * 2;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardProvider {
    pub info: ProviderInfo,
    pub snapshot: Option<ProviderSnapshot>,
    pub enabled: bool,
    pub expanded: bool,
    pub widgets: Vec<RenderedWidget>,
    pub on_demand: Vec<RenderedWidget>,
}

/// How a meter is tracking against its reset. Named states rather than colours, so callers
/// branch on meaning and the palette stays in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaceStatus {
    /// Projected to finish the window with room to spare.
    OnTrack,
    /// Projected to finish the window with very little left.
    Close,
    /// Projected to run out before the window resets.
    RunOut,
    /// Already spent, or as good as.
    Empty,
}

impl PaceStatus {
    pub fn color(self) -> &'static str {
        match self {
            Self::OnTrack => "#3B82F6",
            Self::Close => "#F59E0B",
            Self::RunOut | Self::Empty => "#EF4444",
        }
    }

    /// Whether the projection is worth showing unprompted.
    pub fn is_warning(self) -> bool {
        matches!(self, Self::Close | Self::RunOut)
    }
}

/// A meter's pace verdict, recomputed whenever the dashboard is assembled so the projection
/// tracks the clock. This is the only implementation — the interface renders what it is given
/// rather than recomputing the same thresholds in another language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pace {
    pub status: PaceStatus,
    pub color: String,
    /// Usage projected to the end of the window, as a fraction of the limit.
    pub projected: f64,
    /// How far through the window the clock is, 0..1. Absent when the window is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_fraction: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedWidget {
    pub id: String,
    pub title: String,
    pub line: Option<MetricLine>,
    pub pinned: bool,
    pub no_data: bool,
    /// Present only for progress meters that can be paced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace: Option<Pace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub provider_id: String,
    pub widget_id: String,
    pub title: String,
    pub text: String,
    pub used_ratio: Option<f64>,
    /// Pace colour for the pin's mini track. Pins are the glanceable surface, so they have to
    /// carry the same urgency signal the full meter shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotalSpend {
    pub period: String,
    pub metric: String,
    pub value: f64,
    pub dollars: f64,
    pub tokens: f64,
    pub slices: Vec<SpendSlice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendSlice {
    pub provider_id: String,
    pub display_name: String,
    pub dollars: f64,
    pub tokens: f64,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub providers: Vec<DashboardProvider>,
    pub pins: Vec<Pin>,
    pub total_spend: Option<TotalSpend>,
    pub next_refresh_in_secs: i64,
    pub refreshing: bool,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageNotification {
    /// Identifies the alert itself — provider, metric and severity — independent of when it
    /// fires.
    pub id: String,
    /// The reset window the reading belongs to. One notification per alert per window.
    pub window: String,
    pub title: String,
    pub body: String,
}

/// Remembers which alerts have already been shown. Keyed by alert rather than by
/// alert-and-window, so it stays the size of the alert catalogue instead of growing by one
/// entry every time a quota window rolls over.
#[derive(Debug, Default)]
pub struct AlertLog(std::collections::HashMap<String, String>);

impl AlertLog {
    /// Records the notification and reports whether it is new for its window.
    pub fn should_deliver(&mut self, notification: &UsageNotification) -> bool {
        self.0
            .insert(notification.id.clone(), notification.window.clone())
            .as_deref()
            != Some(notification.window.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn provider_color(id: &str) -> &'static str {
    match id {
        "claude" => "#D97757",
        "codex" => "#10A37F",
        "cursor" => "#F54E00",
        "copilot" => "#7C3AED",
        "grok" => "#1F1F1F",
        "opencode" => "#FF5C00",
        "openrouter" => "#6566F1",
        "zai" => "#000000",
        "devin" => "#3B82F6",
        "antigravity" => "#4285F4",
        _ => "#737373",
    }
}
