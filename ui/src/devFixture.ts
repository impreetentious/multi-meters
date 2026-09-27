// Fixture backend for `npm run dev` in a plain browser, where no Tauri IPC exists. It answers
// the same commands as src-tauri with plausible data so the flyout can be built and reviewed on
// any OS. `bridge.ts` only reaches for it under `import.meta.env.DEV`, so it is tree-shaken out
// of production bundles.
import type { CustomizeData, Dashboard, MetricLine, Pace, Settings, SpendSlice, WidgetDescriptor } from "./types";

const HOUR = 3_600_000;
const iso = (msFromNow: number) => new Date(Date.now() + msFromNow).toISOString();

type Widget = { id: string; title: string; line: MetricLine | null };
type Provider = {
  id: string;
  displayName: string;
  icon: string;
  plan?: string;
  warning?: string;
  stale?: boolean;
  links: { label: string; url: string }[];
  widgets: Widget[];
};

function trend(values: number[]): MetricLine {
  const start = Date.now() - (values.length - 1) * 24 * HOUR;
  return {
    type: "chart",
    label: "Usage Trend",
    points: values.map((value, index) => {
      const day = new Date(start + index * 24 * HOUR);
      return {
        value,
        label: day.toLocaleDateString([], { month: "short", day: "numeric" }),
        value_label: `$${value.toFixed(2)}`,
      };
    }),
    note: "Local logs · bundled model pricing",
  };
}

function spend(label: string, dollars: number, tokens: number): MetricLine {
  return {
    type: "values",
    label,
    values: [
      { number: dollars, kind: "dollars", estimated: true },
      { number: tokens, kind: "count", label: "tokens" },
    ],
  };
}

const PROVIDERS: Provider[] = [
  {
    id: "claude",
    displayName: "Claude",
    icon: "claude",
    plan: "Max 20x",
    links: [{ label: "Usage", url: "https://claude.ai/settings/usage" }],
    widgets: [
      {
        id: "claude.session",
        title: "Session",
        line: {
          type: "progress",
          label: "Session",
          used: 71,
          limit: 100,
          format: { kind: "percent" },
          resets_at: iso(1.6 * HOUR),
          period_duration_ms: 5 * HOUR,
        },
      },
      {
        id: "claude.weekly",
        title: "Weekly",
        line: {
          type: "progress",
          label: "Weekly",
          used: 44,
          limit: 100,
          format: { kind: "percent" },
          resets_at: iso(93 * HOUR),
          period_duration_ms: 7 * 24 * HOUR,
        },
      },
      { id: "claude.today", title: "Today", line: spend("Today", 4.82, 1_284_000) },
      { id: "claude.yesterday", title: "Yesterday", line: spend("Yesterday", 11.4, 3_910_000) },
      { id: "claude.last30", title: "Last 30 Days", line: spend("Last 30 Days", 214.7, 71_400_000) },
      { id: "claude.trend", title: "Usage Trend", line: trend([3, 8, 5, 12, 9, 14, 11, 4.8]) },
    ],
  },
  {
    id: "codex",
    displayName: "Codex",
    icon: "codex",
    plan: "Pro",
    links: [{ label: "Billing", url: "https://platform.openai.com/usage" }],
    widgets: [
      {
        id: "codex.session",
        title: "Session",
        line: {
          type: "progress",
          label: "Session",
          used: 93,
          limit: 100,
          format: { kind: "percent" },
          resets_at: iso(0.7 * HOUR),
          period_duration_ms: 5 * HOUR,
        },
      },
      {
        id: "codex.weekly",
        title: "Weekly",
        line: {
          type: "progress",
          label: "Weekly",
          used: 61,
          limit: 100,
          format: { kind: "percent" },
          resets_at: iso(52 * HOUR),
          period_duration_ms: 7 * 24 * HOUR,
        },
      },
      {
        id: "codex.credits",
        title: "Credits",
        line: {
          type: "values",
          label: "Credits",
          values: [{ number: 1_450, kind: "count", label: "credits" }],
          expiries_at: [iso(15 * 24 * HOUR)],
        },
      },
      { id: "codex.today", title: "Today", line: spend("Today", 1.94, 512_000) },
      { id: "codex.yesterday", title: "Yesterday", line: spend("Yesterday", 3.1, 880_000) },
      { id: "codex.last30", title: "Last 30 Days", line: spend("Last 30 Days", 62.3, 18_900_000) },
      { id: "codex.trend", title: "Usage Trend", line: trend([1, 2, 1.5, 4, 2.2, 3.1, 2.7, 1.9]) },
    ],
  },
  {
    id: "cursor",
    displayName: "Cursor",
    icon: "cursor",
    plan: "Ultra",
    warning: "Cursor's usage endpoint returned 503; showing the last good reading.",
    stale: true,
    links: [{ label: "Dashboard", url: "https://cursor.com/dashboard" }],
    widgets: [
      {
        id: "cursor.auto",
        title: "Auto",
        line: {
          type: "progress",
          label: "Auto",
          used: 18.4,
          limit: 20,
          format: { kind: "dollars" },
          resets_at: iso(210 * HOUR),
          period_duration_ms: 30 * 24 * HOUR,
        },
      },
      {
        id: "cursor.api",
        title: "API",
        line: {
          type: "progress",
          label: "API",
          used: 112,
          limit: 400,
          format: { kind: "dollars" },
          resets_at: iso(210 * HOUR),
          period_duration_ms: 30 * 24 * HOUR,
        },
      },
      {
        id: "cursor.requests",
        title: "Requests",
        line: {
          type: "progress",
          label: "Requests",
          used: 342,
          limit: 500,
          format: { kind: "count", suffix: "requests" },
          resets_at: iso(210 * HOUR),
          period_duration_ms: 30 * 24 * HOUR,
        },
      },
      { id: "cursor.today", title: "Today", line: spend("Today", 0.91, 240_000) },
      { id: "cursor.yesterday", title: "Yesterday", line: spend("Yesterday", 2.4, 700_000) },
      { id: "cursor.last30", title: "Last 30 Days", line: spend("Last 30 Days", 41.6, 12_100_000) },
      { id: "cursor.trend", title: "Usage Trend", line: trend([0.4, 1.9, 2.2, 0.8, 3.4, 2.1, 2.4, 0.9]) },
    ],
  },
  {
    id: "copilot",
    displayName: "Copilot",
    icon: "copilot",
    links: [],
    widgets: [
      {
        id: "copilot.premium",
        title: "Premium Requests",
        line: {
          type: "badge",
          label: "Premium Requests",
          text: "Sign in to GitHub Copilot",
          color_hex: "#EF4444",
        },
      },
    ],
  },
];

const PROVIDER_COLORS: Record<string, string> = {
  claude: "#D97757",
  codex: "#10A37F",
  cursor: "#F54E00",
  copilot: "#7C3AED",
};

const SPEND_LABELS: Record<Settings["total_spend_period"], string> = {
  today: "Today",
  yesterday: "Yesterday",
  last30: "Last 30 Days",
};

let settings: Settings = {
  enabled: ["claude", "codex", "cursor", "copilot"],
  order: ["claude", "codex", "cursor", "copilot"],
  hidden_metrics: [],
  on_demand: PROVIDERS.flatMap((provider) =>
    provider.widgets.filter((w) => /\.(today|yesterday|last30|credits|requests)$/.test(w.id)).map((w) => w.id),
  ),
  pinned: { claude: ["claude.session", "claude.weekly"], codex: ["codex.session"] },
  expanded: [],
  show_total_spend: true,
  hide_on_blur: true,
  launch_at_login: false,
  global_shortcut: "Ctrl+Shift+M",
  theme: "system",
  density: "compact",
  reduce_animations: false,
  time_format: "auto",
  show_usage_as: "left",
  reset_times: "countdown",
  always_show_pacing: false,
  refresh_interval_minutes: 5,
  total_spend_period: "today",
  total_spend_metric: "cost",
  notify_almost_out: false,
  notify_cutting_it_close: false,
  notify_will_run_out: false,
  api_key_configured: [],
};

let lastRefresh = Date.now();

function formatLine(line: MetricLine, usedMode: boolean): string {
  if (line.type !== "progress") return "";
  if (line.format.kind === "percent") {
    const value = usedMode ? line.used : Math.max(0, line.limit - line.used);
    return `${Math.round(value)}% ${usedMode ? "used" : "left"}`;
  }
  if (line.format.kind === "dollars") {
    return usedMode
      ? `$${line.used.toFixed(2)} of $${line.limit.toFixed(2)}`
      : `$${Math.max(0, line.limit - line.used).toFixed(2)} left`;
  }
  const suffix = line.format.suffix ? ` ${line.format.suffix}` : "";
  return usedMode
    ? `${line.used} / ${line.limit}${suffix}`
    : `${Math.max(0, line.limit - line.used)}${suffix} left`;
}

const PACE_COLORS: Record<Pace["status"], string> = {
  on_track: "#3B82F6",
  close: "#F59E0B",
  run_out: "#EF4444",
  empty: "#EF4444",
};

// Stands in for `format::pace` in the core crate. This is the fixture's job — mimicking the
// backend — and it never ships: the interface itself has no pacing logic of its own.
function paceVerdict(line: Extract<MetricLine, { type: "progress" }>): Pace {
  const ratio = line.limit > 0 ? line.used / line.limit : 0;
  const remaining = Math.min(1, Math.max(0, 1 - ratio));
  const elapsed =
    line.resets_at && line.period_duration_ms && line.period_duration_ms > 0
      ? Math.min(
          1,
          Math.max(
            0,
            (line.period_duration_ms - Math.max(0, new Date(line.resets_at).getTime() - Date.now())) /
              line.period_duration_ms,
          ),
        )
      : undefined;
  const verdict = (status: Pace["status"], projected: number): Pace => ({
    status,
    color: line.color_hex ?? PACE_COLORS[status],
    projected,
    elapsed_fraction: elapsed,
  });

  if (remaining <= 0.005) return verdict("empty", 1);
  if (elapsed !== undefined && elapsed > 0.08) {
    const projected = ratio / elapsed;
    if (projected >= 1) return verdict("run_out", projected);
    if (projected >= 0.9) return verdict("close", projected);
    return verdict("on_track", projected);
  }
  if (remaining <= 0.1) return verdict("empty", ratio);
  return verdict(ratio >= 0.8 ? "close" : "on_track", ratio);
}

function dashboard(): Dashboard {
  const usedMode = settings.show_usage_as === "used";
  const spendLabel = SPEND_LABELS[settings.total_spend_period];
  const ordered = settings.order
    .map((id) => PROVIDERS.find((provider) => provider.id === id))
    .filter((provider): provider is Provider => !!provider && settings.enabled.includes(provider.id));

  const pins: Dashboard["pins"] = [];
  const slices: SpendSlice[] = [];
  let hasSpendProvider = false;

  const providers = ordered.map((provider) => {
    const visible: Dashboard["providers"][number]["widgets"] = [];
    const onDemand: Dashboard["providers"][number]["widgets"] = [];
    for (const widget of provider.widgets) {
      if (settings.hidden_metrics.includes(widget.id)) continue;
      const pinned = Object.values(settings.pinned).some((ids) => ids.includes(widget.id));
      const rendered = {
        id: widget.id,
        title: widget.title,
        line: widget.line,
        pinned,
        no_data: !widget.line,
        pace: widget.line?.type === "progress" ? paceVerdict(widget.line) : undefined,
      };
      if (pinned && widget.line && widget.line.type === "progress") {
        pins.push({
          provider_id: provider.id,
          widget_id: widget.id,
          title: `${provider.displayName} ${widget.title}`,
          text: formatLine(widget.line, usedMode),
          used_ratio: widget.line.limit > 0 ? widget.line.used / widget.line.limit : undefined,
          color: rendered.pace?.color,
        });
      }
      if (settings.on_demand.includes(widget.id)) onDemand.push(rendered);
      else visible.push(rendered);
    }

    const spendWidget = provider.widgets.find((widget) => widget.title === spendLabel);
    if (spendWidget) hasSpendProvider = true;
    if (spendWidget?.line?.type === "values") {
      slices.push({
        provider_id: provider.id,
        display_name: provider.displayName,
        dollars: spendWidget.line.values[0]?.number ?? 0,
        tokens: spendWidget.line.values[1]?.number ?? 0,
        color: PROVIDER_COLORS[provider.id] ?? "#737373",
      });
    }

    return {
      info: { id: provider.id, display_name: provider.displayName, icon: provider.icon, links: provider.links },
      snapshot: {
        plan: provider.plan,
        warning: provider.warning,
        refreshed_at: new Date(lastRefresh).toISOString(),
        stale: !!provider.stale,
      },
      enabled: true,
      expanded: settings.expanded.includes(provider.id),
      widgets: visible,
      on_demand: onDemand,
    };
  });

  const dollars = slices.reduce((sum, slice) => sum + slice.dollars, 0);
  const tokens = slices.reduce((sum, slice) => sum + slice.tokens, 0);
  const metric = settings.total_spend_metric;
  const kept = slices.filter((slice) =>
    metric === "tokens" ? slice.tokens > 0 : metric === "cost_per_million" ? slice.tokens > 0 && slice.dollars > 0 : slice.dollars > 0,
  );

  const elapsed = Math.floor((Date.now() - lastRefresh) / 1_000);
  return {
    providers,
    pins,
    total_spend:
      settings.show_total_spend && hasSpendProvider
        ? {
            period: settings.total_spend_period,
            metric,
            value:
              metric === "tokens"
                ? tokens
                : metric === "cost_per_million"
                  ? tokens > 0
                    ? (dollars / tokens) * 1e6
                    : 0
                  : dollars,
            dollars,
            tokens,
            slices: kept,
          }
        : null,
    next_refresh_in_secs: Math.max(0, settings.refresh_interval_minutes * 60 - elapsed),
    refreshing: false,
    version: "dev-fixture",
  };
}

function customize(): CustomizeData {
  return {
    providers: settings.order
      .map((id) => PROVIDERS.find((provider) => provider.id === id))
      .filter((provider): provider is Provider => !!provider)
      .map((provider) => ({
        id: provider.id,
        displayName: provider.displayName,
        icon: provider.icon,
        enabled: settings.enabled.includes(provider.id),
        widgets: provider.widgets.map<WidgetDescriptor>((widget) => ({
          id: widget.id,
          provider_id: provider.id,
          title: widget.title,
          metric_label: widget.title,
          pinnable: !widget.id.endsWith(".trend"),
          is_spend_tile: /\.(today|yesterday|last30)$/.test(widget.id),
          default_on: true,
        })),
      })),
    settings,
  };
}

export function devInvoke(command: string, args?: Record<string, unknown>): unknown {
  switch (command) {
    case "get_dashboard":
      return dashboard();
    case "get_settings":
      return settings;
    case "get_customize":
      return customize();
    case "refresh_all":
    case "refresh_one":
      lastRefresh = Date.now();
      return dashboard();
    case "patch_settings":
      settings = { ...settings, ...(args?.patch as Partial<Settings>) };
      return settings;
    case "toggle_pin": {
      const providerId = String(args?.providerId);
      const widgetId = String(args?.widgetId);
      const current = settings.pinned[providerId] ?? [];
      const next = current.includes(widgetId)
        ? current.filter((id) => id !== widgetId)
        : current.length >= 2
          ? (() => {
              throw new Error("Up to 2 pins per provider");
            })()
          : [...current, widgetId];
      settings = { ...settings, pinned: { ...settings.pinned, [providerId]: next } };
      return settings;
    }
    case "set_api_key": {
      const provider = String(args?.providerId);
      const value = String(args?.value ?? "").trim();
      const configured = new Set(settings.api_key_configured);
      if (value) configured.add(provider);
      else configured.delete(provider);
      settings = { ...settings, api_key_configured: [...configured] };
      return settings;
    }
    case "reset_all_settings":
      settings = { ...settings, hidden_metrics: [], expanded: [], theme: "system", density: "compact" };
      return settings;
    case "reveal_log":
    case "hide_flyout":
      return null;
    default:
      throw new Error(`Unknown command: ${command}`);
  }
}
