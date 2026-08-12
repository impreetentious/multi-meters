use async_trait::async_trait;
use std::sync::Arc;

use crate::http::Http;
use crate::models::{ProviderInfo, ProviderSnapshot, WidgetDescriptor};

mod claude;
mod codex;
mod copilot;
mod cursor;
mod devin;
mod grok;

#[async_trait]
pub trait Provider: Send + Sync {
    fn info(&self) -> &ProviderInfo;
    fn widgets(&self) -> &[WidgetDescriptor];
    async fn has_local_credentials(&self) -> bool;
    async fn refresh(&self, http: &Http) -> ProviderSnapshot;
}

pub fn catalog() -> Vec<Arc<dyn Provider>> {
    vec![
        Arc::new(claude::ClaudeProvider::new()),
        Arc::new(codex::CodexProvider::new()),
        Arc::new(cursor::CursorProvider::new()),
        Arc::new(copilot::CopilotProvider::new()),
        Arc::new(devin::DevinProvider::new()),
        Arc::new(grok::GrokProvider::new()),
    ]
}

pub fn widget(
    id: &str,
    provider_id: &str,
    title: &str,
    default_on: bool,
    on_demand: bool,
    pinned: bool,
) -> WidgetDescriptor {
    widget_labeled(id, provider_id, title, title, default_on, on_demand, pinned)
}

pub fn widget_labeled(
    id: &str,
    provider_id: &str,
    title: &str,
    metric_label: &str,
    default_on: bool,
    on_demand: bool,
    pinned: bool,
) -> WidgetDescriptor {
    WidgetDescriptor {
        id: id.to_string(),
        provider_id: provider_id.to_string(),
        title: title.to_string(),
        metric_label: metric_label.to_string(),
        pinnable: !id.ends_with(".trend"),
        is_spend_tile: false,
        default_on,
        default_on_demand: on_demand,
        default_pinned: pinned,
    }
}

pub fn spend_widgets(provider_id: &str) -> Vec<WidgetDescriptor> {
    let mut widgets = vec![
        widget(
            &format!("{provider_id}.today"),
            provider_id,
            "Today",
            true,
            true,
            false,
        ),
        widget(
            &format!("{provider_id}.yesterday"),
            provider_id,
            "Yesterday",
            true,
            true,
            false,
        ),
        widget(
            &format!("{provider_id}.last30"),
            provider_id,
            "Last 30 Days",
            true,
            true,
            false,
        ),
        widget(
            &format!("{provider_id}.trend"),
            provider_id,
            "Usage Trend",
            true,
            false,
            false,
        ),
    ];
    for widget in &mut widgets[..3] {
        widget.is_spend_tile = true;
    }
    widgets
}
