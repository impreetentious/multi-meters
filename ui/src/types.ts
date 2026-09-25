export type MetricValue = {
  number: number;
  kind: "percent" | "dollars" | "count";
  label?: string;
  estimated?: boolean;
};

export type MetricLine =
  | {
      type: "progress";
      label: string;
      used: number;
      limit: number;
      format: { kind: "percent" | "dollars" | "count"; suffix?: string };
      resets_at?: string;
      period_duration_ms?: number;
      color_hex?: string;
    }
  | {
      type: "values";
      label: string;
      values: MetricValue[];
      color_hex?: string;
      expiries_at?: string[];
      unknown_models?: string[];
    }
  | { type: "badge"; label: string; text: string; color_hex?: string; subtitle?: string }
  | { type: "text"; label: string; value: string; color_hex?: string; subtitle?: string }
  | {
      type: "chart";
      label: string;
      points: { value: number; label: string; value_label?: string }[];
      note?: string;
    };

export type WidgetDescriptor = {
  id: string;
  provider_id: string;
  title: string;
  metric_label: string;
  pinnable: boolean;
  is_spend_tile: boolean;
  default_on: boolean;
  default_on_demand: boolean;
  default_pinned: boolean;
};

// Computed by the engine on every dashboard read. The interface renders this verdict rather
// than recomputing the thresholds, so there is exactly one implementation of pacing.
export type Pace = {
  status: "on_track" | "close" | "run_out" | "empty";
  color: string;
  projected: number;
  elapsed_fraction?: number;
};

export type RenderedWidget = {
  id: string;
  title: string;
  line: MetricLine | null;
  pinned: boolean;
  no_data: boolean;
  pace?: Pace;
};

export type DashboardProvider = {
  info: {
    id: string;
    display_name: string;
    icon: string;
    links: { label: string; url: string }[];
  };
  snapshot: {
    plan?: string;
    warning?: string;
    refreshed_at: string;
    stale: boolean;
  } | null;
  enabled: boolean;
  expanded: boolean;
  widgets: RenderedWidget[];
  on_demand: RenderedWidget[];
};

export type SpendSlice = {
  provider_id: string;
  display_name: string;
  dollars: number;
  tokens: number;
  color: string;
};

export type Dashboard = {
  providers: DashboardProvider[];
  pins: {
    provider_id: string;
    widget_id: string;
    title: string;
    text: string;
    used_ratio?: number;
    color?: string;
  }[];
  total_spend: {
    period: "today" | "yesterday" | "last30";
    metric: "cost" | "tokens" | "cost_per_million";
    value: number;
    dollars: number;
    tokens: number;
    slices: SpendSlice[];
  } | null;
  next_refresh_in_secs: number;
  refreshing: boolean;
  version: string;
};

export type Settings = {
  enabled: string[];
  order: string[];
  hidden_metrics: string[];
  on_demand: string[];
  pinned: Record<string, string[]>;
  expanded: string[];
  show_total_spend: boolean;
  launch_at_login: boolean;
  global_shortcut?: string | null;
  theme: "system" | "dark" | "light";
  density: "compact" | "comfortable";
  reduce_animations: boolean;
  time_format: "auto" | "12" | "24";
  show_usage_as: "left" | "used";
  reset_times: "countdown" | "exact";
  always_show_pacing: boolean;
  refresh_interval_minutes: 1 | 5 | 15 | 30 | 60;
  total_spend_period: "today" | "yesterday" | "last30";
  total_spend_metric: "cost" | "tokens" | "cost_per_million";
  notify_almost_out: boolean;
  notify_cutting_it_close: boolean;
  notify_will_run_out: boolean;
  api_key_configured: string[];
};

export type CustomizeProvider = {
  id: string;
  displayName: string;
  enabled: boolean;
  widgets: WidgetDescriptor[];
};

export type CustomizeData = {
  providers: CustomizeProvider[];
  settings: Settings;
};
