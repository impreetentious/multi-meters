import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import type {
  CustomizeData,
  Dashboard,
  DashboardProvider,
  MetricLine,
  RenderedWidget,
  Settings,
  SpendSlice,
} from "./types";

type Screen = "dash" | "customize" | "settings";

function errorText(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function fmtMoney(value: number) {
  if (Math.abs(value) >= 1_000_000) return `$${(value / 1_000_000).toFixed(1)}M`;
  if (Math.abs(value) >= 1_000) return `$${(value / 1_000).toFixed(1)}K`;
  return `$${value.toFixed(2)}`;
}

function fmtNum(value: number) {
  if (Math.abs(value) >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(1)}B`;
  if (Math.abs(value) >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (Math.abs(value) >= 10_000) return `${(value / 1_000).toFixed(1)}K`;
  return value.toFixed(Math.abs(value) >= 100 ? 0 : 2);
}

function formatMetricValue(value: { number: number; kind: string; label?: string; estimated?: boolean }) {
  const prefix = value.estimated ? "~" : "";
  if (value.kind === "dollars") return `${prefix}${fmtMoney(value.number)}`;
  if (value.kind === "percent") return `${prefix}${Math.round(value.number)}%`;
  return `${prefix}${fmtNum(value.number)}${value.label ? ` ${value.label}` : ""}`;
}

function usedLeft(line: Extract<MetricLine, { type: "progress" }>, usedMode: boolean) {
  const kind = line.format.kind;
  if (kind === "percent") {
    const value = usedMode ? line.used : Math.max(0, line.limit - line.used);
    return `${Math.round(value)}% ${usedMode ? "used" : "left"}`;
  }
  if (kind === "dollars") {
    return usedMode
      ? `${fmtMoney(line.used)} of ${fmtMoney(line.limit)}`
      : `${fmtMoney(Math.max(0, line.limit - line.used))} left`;
  }
  const suffix = line.format.suffix ? ` ${line.format.suffix}` : "";
  return usedMode
    ? `${fmtNum(line.used)} / ${fmtNum(line.limit)}${suffix}`
    : `${fmtNum(Math.max(0, line.limit - line.used))}${suffix} left`;
}

function resetLabel(iso: string | undefined, countdown: boolean, timeFormat: Settings["time_format"]) {
  if (!iso) return "";
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "Reset time unavailable";
  const seconds = Math.max(0, Math.floor((at.getTime() - Date.now()) / 1_000));
  if (countdown) {
    if (seconds <= 0) return "Resets soon";
    const hours = Math.floor(seconds / 3_600);
    const minutes = Math.floor((seconds % 3_600) / 60);
    if (hours >= 48) return `Resets in ${Math.floor(hours / 24)}d ${hours % 24}h`;
    if (hours >= 1) return `Resets in ${hours}h ${minutes}m`;
    return `Resets in ${Math.max(1, minutes)}m`;
  }
  const hour12 = timeFormat === "auto" ? undefined : timeFormat === "12";
  return `Resets ${at.toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    hour12,
  })}`;
}

// The stylesheet defines one light palette and one dark palette. Resolving the "system"
// preference to a concrete value here keeps a `prefers-color-scheme` copy of the palette from
// having to shadow it.
function useResolvedTheme(preference: Settings["theme"]): "light" | "dark" {
  const query = () => window.matchMedia?.("(prefers-color-scheme: dark)");
  const [systemDark, setSystemDark] = useState(() => query()?.matches ?? true);
  useEffect(() => {
    const media = query();
    if (!media) return;
    const update = (event: MediaQueryListEvent) => setSystemDark(event.matches);
    setSystemDark(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  if (preference === "system") return systemDark ? "dark" : "light";
  return preference;
}

function nextUpdateLabel(seconds: number, refreshing: boolean) {
  if (refreshing) return "Updating…";
  if (seconds <= 0) return "Update due";
  if (seconds < 60) return `Next update in ${seconds}s`;
  return `Next update in ${Math.ceil(seconds / 60)}m`;
}

// A missing or unreadable icon should leave a gap, not a broken-image glyph.
function ProviderIcon({ icon, size }: { icon: string; size?: number }) {
  const [failed, setFailed] = useState(false);
  if (failed) return <i className="icon-fallback" style={size ? { width: size, height: size } : undefined} aria-hidden="true" />;
  return (
    <img
      src={`/icons/${icon}.svg`}
      alt=""
      width={size}
      height={size}
      onError={() => setFailed(true)}
    />
  );
}

function LineView({
  widget,
  settings,
  onToggleUsed,
  onToggleReset,
}: {
  widget: RenderedWidget;
  settings: Settings;
  onToggleUsed: () => void;
  onToggleReset: () => void;
}) {
  const line = widget.line;
  if (!line) {
    return (
      <div className="row row-h muted">
        <span>{widget.title}</span>
        <span>No data</span>
      </div>
    );
  }
  if (line.type === "progress") {
    const ratio = Math.min(1, Math.max(0, line.limit > 0 ? line.used / line.limit : 0));
    const verdict = widget.pace;
    const showPace = !!verdict && (settings.always_show_pacing || verdict.status === "close" || verdict.status === "run_out");
    const projectedLeft = Math.max(0, 1 - (verdict?.projected ?? 0));
    return (
      <div className="row">
        <div className="row-h">
          <span className="title">
            {widget.title}
            {showPace && (
              <small>
                {verdict?.status === "run_out"
                  ? "On track to run out early"
                  : `~${Math.round(projectedLeft * 100)}% left at reset`}
              </small>
            )}
          </span>
          <button className="value-button" onClick={onToggleUsed}>
            {usedLeft(line, settings.show_usage_as === "used")}
          </button>
        </div>
        <div
          className="track"
          role="progressbar"
          aria-label={widget.title}
          aria-valuemin={0}
          aria-valuemax={line.limit}
          aria-valuenow={Math.min(line.used, line.limit)}
          aria-valuetext={usedLeft(line, settings.show_usage_as === "used")}
        >
          <div
            className="fill"
            style={{ width: `${ratio * 100}%`, background: verdict?.color ?? line.color_hex }}
          />
          {showPace && verdict.elapsed_fraction != null && (
            <i className="pace-mark" style={{ left: `${verdict.elapsed_fraction * 100}%` }} />
          )}
        </div>
        {line.resets_at && (
          <button className="reset" onClick={onToggleReset}>
            {resetLabel(line.resets_at, settings.reset_times !== "exact", settings.time_format)}
          </button>
        )}
      </div>
    );
  }
  if (line.type === "values") {
    const text = line.values.map(formatMetricValue).join(" · ");
    return (
      <div className="row">
        <div className="row-h">
          <span className="title">{widget.title}</span>
          <span className="value-text">{text || "No data"}</span>
        </div>
        {line.expiries_at && line.expiries_at.length > 0 && (
          <div className="source-note">
            Next expiry {resetLabel(line.expiries_at[0], false, settings.time_format).replace(/^Resets /, "")}
          </div>
        )}
        {line.unknown_models && line.unknown_models.length > 0 && (
          <div className="source-note" title={line.unknown_models.join(", ")}>
            Totals exclude {line.unknown_models.length} model{line.unknown_models.length === 1 ? "" : "s"} with unavailable pricing
          </div>
        )}
      </div>
    );
  }
  if (line.type === "badge") {
    return (
      <div className="row">
        <div className="row-h">
          <span className="title">{widget.title}</span>
          <span className="badge" style={{ color: line.color_hex }}>{line.text}</span>
        </div>
        {line.subtitle && <div className="source-note">{line.subtitle}</div>}
      </div>
    );
  }
  if (line.type === "chart") {
    const maximum = Math.max(1, ...line.points.map((point) => point.value));
    return (
      <div className="row">
        <div className="title">{widget.title}</div>
        <div
          className="chart"
          role="img"
          aria-label={`${widget.title}: ${line.points
            .map((point) => `${point.label} ${point.value_label ?? fmtNum(point.value)}`)
            .join(", ")}`}
        >
          {line.points.map((point, index) => (
            <i
              key={`${point.label}:${index}`}
              title={`${point.label}: ${point.value_label ?? fmtNum(point.value)}`}
              style={{ height: `${Math.max(4, (point.value / maximum) * 100)}%` }}
            />
          ))}
        </div>
        {line.note && <div className="source-note">{line.note}</div>}
      </div>
    );
  }
  return (
    <div className="row">
      <div className="row-h">
        <span className="title">{widget.title}</span>
        <span className="value-text">{line.value}</span>
      </div>
      {line.subtitle && <div className="source-note">{line.subtitle}</div>}
    </div>
  );
}

function ProviderCard({
  provider,
  settings,
  busy,
  onPatch,
  onRefresh,
  onError,
}: {
  provider: DashboardProvider;
  settings: Settings;
  busy: boolean;
  onPatch: (patch: Partial<Settings>) => Promise<void>;
  onRefresh: () => Promise<void>;
  onError: (error: unknown) => void;
}) {
  const hasDetails = provider.on_demand.length > 0 || provider.info.links.length > 0;
  const toggleExpanded = () => {
    const expanded = new Set(settings.expanded);
    if (provider.expanded) expanded.delete(provider.info.id);
    else expanded.add(provider.info.id);
    return onPatch({ expanded: [...expanded] });
  };
  return (
    <section className="card">
      <div className="card-h">
        <ProviderIcon icon={provider.info.icon} />
        <span className="name">{provider.info.display_name}</span>
        {provider.snapshot?.plan && <span className="plan">{provider.snapshot.plan}</span>}
        {provider.snapshot?.stale && <span className="stale">Outdated</span>}
        <span className="grow" />
        <button className="icon-btn small" disabled={busy} onClick={() => void onRefresh()} aria-label={`Refresh ${provider.info.display_name}`}>
          ↻
        </button>
        {hasDetails && (
          <button
            className="icon-btn small"
            aria-expanded={provider.expanded}
            onClick={() => void toggleExpanded()}
          >
            {provider.expanded ? "Less" : "More"}
          </button>
        )}
      </div>
      {provider.snapshot?.warning && <div className="warning-box">{provider.snapshot.warning}</div>}
      {provider.widgets.map((widget) => (
        <LineView
          key={widget.id}
          widget={widget}
          settings={settings}
          onToggleUsed={() => void onPatch({ show_usage_as: settings.show_usage_as === "used" ? "left" : "used" })}
          onToggleReset={() => void onPatch({ reset_times: settings.reset_times === "exact" ? "countdown" : "exact" })}
        />
      ))}
      {provider.expanded &&
        provider.on_demand.map((widget) => (
          <LineView
            key={widget.id}
            widget={widget}
            settings={settings}
            onToggleUsed={() => void onPatch({ show_usage_as: settings.show_usage_as === "used" ? "left" : "used" })}
            onToggleReset={() => void onPatch({ reset_times: settings.reset_times === "exact" ? "countdown" : "exact" })}
          />
        ))}
      {provider.expanded && provider.info.links.length > 0 && (
        <div className="links">
          {provider.info.links.map((link) => (
            <button key={link.url} onClick={() => void openUrl(link.url).catch(onError)}>
              {link.label}
            </button>
          ))}
        </div>
      )}
    </section>
  );
}

function sliceMetric(slice: SpendSlice, metric: Settings["total_spend_metric"]) {
  if (metric === "tokens") return slice.tokens;
  if (metric === "cost_per_million") return slice.tokens > 0 ? (slice.dollars / slice.tokens) * 1_000_000 : 0;
  return slice.dollars;
}

// What each arc of the ring is sized by. Cost and tokens are additive, so a share of the total
// is meaningful. A blended rate is not — rates do not sum — so under Cost / MTok the ring shows
// the token volume the rates are averaged over, and the legend carries each provider's rate.
function sliceWeight(slice: SpendSlice, metric: Settings["total_spend_metric"]) {
  return metric === "cost" ? slice.dollars : slice.tokens;
}

function TotalSpendCard({
  total,
  settings,
  onPatch,
}: {
  total: NonNullable<Dashboard["total_spend"]>;
  settings: Settings;
  onPatch: (patch: Partial<Settings>) => Promise<void>;
}) {
  const sum = total.slices.reduce((value, slice) => value + sliceWeight(slice, total.metric), 0);
  const center =
    total.metric === "tokens"
      ? `${fmtNum(total.value)} tokens`
      : total.metric === "cost_per_million"
        ? `${fmtMoney(total.value)} / MTok`
        : fmtMoney(total.value);
  const shareOf = total.metric === "cost" ? "cost" : "tokens";
  const ringLabel = `Share of ${shareOf} by provider: ${total.slices
    .map((slice) => `${slice.display_name} ${Math.round((sliceWeight(slice, total.metric) / (sum || 1)) * 100)}%`)
    .join(", ")}`;
  return (
    <section className="card total-card">
      <div className="card-h spend-controls">
        <select value={settings.total_spend_metric} onChange={(event) => void onPatch({ total_spend_metric: event.target.value as Settings["total_spend_metric"] })}>
          <option value="cost">Cost</option>
          <option value="cost_per_million">Cost / MTok</option>
          <option value="tokens">Tokens</option>
        </select>
        <span className="grow" />
        <div className="segmented">
          {(["today", "yesterday", "last30"] as const).map((period) => (
            <button key={period} className={settings.total_spend_period === period ? "active" : ""} onClick={() => void onPatch({ total_spend_period: period })}>
              {period === "today" ? "Today" : period === "yesterday" ? "Yesterday" : "30 Days"}
            </button>
          ))}
        </div>
      </div>
      {total.slices.length === 0 ? (
        <div className="empty compact">No usage for this period.</div>
      ) : (
        <div className="ring-wrap">
          <div className="ring-stack">
            <svg className="ring" viewBox="0 0 36 36" role="img" aria-label={ringLabel}>
              {(() => {
                let accumulated = 0;
                return total.slices.map((slice) => {
                  const fraction = sum > 0 ? sliceWeight(slice, total.metric) / sum : 0;
                  const dash = `${fraction * 100} ${100 - fraction * 100}`;
                  const rotation = accumulated * 360;
                  accumulated += fraction;
                  return (
                    <circle
                      key={slice.provider_id}
                      cx="18"
                      cy="18"
                      r="15.9"
                      fill="none"
                      stroke={slice.color}
                      strokeWidth="3.5"
                      strokeDasharray={dash}
                      transform={`rotate(${rotation - 90} 18 18)`}
                    />
                  );
                });
              })()}
            </svg>
          </div>
          <div className="spend-summary">
            <div className="total-value">{center}</div>
            {total.slices.map((slice) => (
              <div className="legend" key={slice.provider_id}>
                <i style={{ background: slice.color }} />
                <b>{slice.display_name}</b>
                <span>
                  {total.metric === "tokens"
                    ? fmtNum(slice.tokens)
                    : total.metric === "cost_per_million"
                      ? fmtMoney(sliceMetric(slice, total.metric))
                      : fmtMoney(slice.dollars)}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
      {total.metric === "cost_per_million" && total.slices.length > 0 && (
        <div className="source-note">Ring shows each provider's share of tokens; the rate is a blended average.</div>
      )}
    </section>
  );
}

function Toggle({ checked, onChange, label }: { checked: boolean; onChange: (checked: boolean) => void; label: string }) {
  return <input aria-label={label} type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} />;
}

// The flyout hides itself whenever it loses focus, so a native `window.confirm` would take the
// window down with it. Confirmations have to live inside the webview.
function ConfirmDialog({
  title,
  body,
  confirmLabel,
  onConfirm,
  onCancel,
}: {
  title: string;
  body: string;
  confirmLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const confirmRef = useRef<HTMLButtonElement>(null);
  useEffect(() => confirmRef.current?.focus(), []);
  return (
    <div className="modal-scrim" onClick={onCancel}>
      <div
        className="modal"
        role="alertdialog"
        aria-modal="true"
        aria-label={title}
        onClick={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            onCancel();
          }
        }}
      >
        <h4>{title}</h4>
        <p>{body}</p>
        <div className="modal-actions">
          <button onClick={onCancel}>Cancel</button>
          <button ref={confirmRef} className="danger-fill" onClick={onConfirm}>{confirmLabel}</button>
        </div>
      </div>
    </div>
  );
}

function SettingsScreen({
  settings,
  onPatch,
  onApiKey,
  onReset,
  onError,
}: {
  settings: Settings;
  onPatch: (patch: Partial<Settings>) => Promise<void>;
  onApiKey: (provider: string, value: string) => Promise<void>;
  onReset: () => Promise<void>;
  onError: (error: unknown) => void;
}) {
  const [shortcut, setShortcut] = useState(settings.global_shortcut ?? "");
  const [openRouterKey, setOpenRouterKey] = useState("");
  const [zaiKey, setZaiKey] = useState("");
  const [confirmingReset, setConfirmingReset] = useState(false);
  const saveKey = async (provider: string, value: string, clear: () => void) => {
    try {
      await onApiKey(provider, value);
      clear();
    } catch (error) {
      onError(error);
    }
  };
  return (
    <div className="settings-list">
      <h3>Dashboard</h3>
      <label className="setting"><span>Show Total Spend</span><Toggle label="Show Total Spend" checked={settings.show_total_spend} onChange={(checked) => void onPatch({ show_total_spend: checked })} /></label>
      <label className="setting"><span>Refresh Interval</span><select value={settings.refresh_interval_minutes} onChange={(event) => void onPatch({ refresh_interval_minutes: Number(event.target.value) as Settings["refresh_interval_minutes"] })}><option value={1}>1 minute</option><option value={5}>5 minutes</option><option value={15}>15 minutes</option><option value={30}>30 minutes</option><option value={60}>60 minutes</option></select></label>
      <label className="setting"><span>Close When Unfocused</span><Toggle label="Close When Unfocused" checked={settings.hide_on_blur} onChange={(checked) => void onPatch({ hide_on_blur: checked })} /></label>
      <label className="setting"><span>Launch at Login</span><Toggle label="Launch at Login" checked={settings.launch_at_login} onChange={(checked) => void onPatch({ launch_at_login: checked })} /></label>
      <div className="setting stacked"><span>Global Shortcut</span><div className="inline-input"><input value={shortcut} placeholder="Ctrl+Shift+M" onChange={(event) => setShortcut(event.target.value)} /><button onClick={() => void onPatch({ global_shortcut: shortcut.trim() || null })}>Save</button></div></div>

      <h3>Appearance</h3>
      <label className="setting"><span>Theme</span><select value={settings.theme} onChange={(event) => void onPatch({ theme: event.target.value as Settings["theme"] })}><option value="system">System</option><option value="dark">Dark</option><option value="light">Light</option></select></label>
      <label className="setting"><span>Density</span><select value={settings.density} onChange={(event) => void onPatch({ density: event.target.value as Settings["density"] })}><option value="compact">Compact</option><option value="comfortable">Comfortable</option></select></label>
      <label className="setting"><span>Reduce Animations</span><Toggle label="Reduce Animations" checked={settings.reduce_animations} onChange={(checked) => void onPatch({ reduce_animations: checked })} /></label>

      <h3>Usage</h3>
      <label className="setting"><span>Show Usage As</span><select value={settings.show_usage_as} onChange={(event) => void onPatch({ show_usage_as: event.target.value as Settings["show_usage_as"] })}><option value="left">Left</option><option value="used">Used</option></select></label>
      <label className="setting"><span>Reset Times</span><select value={settings.reset_times} onChange={(event) => void onPatch({ reset_times: event.target.value as Settings["reset_times"] })}><option value="countdown">Countdown</option><option value="exact">Exact Time</option></select></label>
      <label className="setting"><span>Clock</span><select value={settings.time_format} onChange={(event) => void onPatch({ time_format: event.target.value as Settings["time_format"] })}><option value="auto">System</option><option value="12">12-hour</option><option value="24">24-hour</option></select></label>
      <label className="setting"><span>Always Show Pacing</span><Toggle label="Always Show Pacing" checked={settings.always_show_pacing} onChange={(checked) => void onPatch({ always_show_pacing: checked })} /></label>

      <h3>Notifications</h3>
      <label className="setting"><span>Almost Out</span><Toggle label="Notify when almost out" checked={settings.notify_almost_out} onChange={(checked) => void onPatch({ notify_almost_out: checked })} /></label>
      <label className="setting"><span>Cutting It Close</span><Toggle label="Notify when cutting it close" checked={settings.notify_cutting_it_close} onChange={(checked) => void onPatch({ notify_cutting_it_close: checked })} /></label>
      <label className="setting"><span>Projected to Run Out</span><Toggle label="Notify when projected to run out" checked={settings.notify_will_run_out} onChange={(checked) => void onPatch({ notify_will_run_out: checked })} /></label>

      <h3>API Keys</h3>
      <div className="setting stacked"><span>OpenRouter {settings.api_key_configured.includes("openrouter") && <em>Configured</em>}</span><div className="inline-input"><input type="password" autoComplete="off" value={openRouterKey} placeholder={settings.api_key_configured.includes("openrouter") ? "Enter a replacement key" : "sk-or-…"} onChange={(event) => setOpenRouterKey(event.target.value)} /><button disabled={!openRouterKey.trim()} onClick={() => void saveKey("openrouter", openRouterKey, () => setOpenRouterKey(""))}>Save</button>{settings.api_key_configured.includes("openrouter") && <button className="danger-text" onClick={() => void saveKey("openrouter", "", () => setOpenRouterKey(""))}>Remove</button>}</div></div>
      <div className="setting stacked"><span>Z.ai {settings.api_key_configured.includes("zai") && <em>Configured</em>}</span><div className="inline-input"><input type="password" autoComplete="off" value={zaiKey} placeholder={settings.api_key_configured.includes("zai") ? "Enter a replacement key" : "API key"} onChange={(event) => setZaiKey(event.target.value)} /><button disabled={!zaiKey.trim()} onClick={() => void saveKey("zai", zaiKey, () => setZaiKey(""))}>Save</button>{settings.api_key_configured.includes("zai") && <button className="danger-text" onClick={() => void saveKey("zai", "", () => setZaiKey(""))}>Remove</button>}</div></div>

      <h3>Advanced</h3>
      <div className="setting"><span>Local API</span><code>127.0.0.1:6736</code></div>
      <div className="button-row"><button onClick={() => void invoke("reveal_log").catch(onError)}>Reveal Log</button><button className="danger-text" onClick={() => setConfirmingReset(true)}>Reset All Settings…</button></div>

      {confirmingReset && (
        <ConfirmDialog
          title="Reset all settings?"
          body="Providers, metrics, pins and appearance go back to their defaults. Saved API keys and cached usage are kept."
          confirmLabel="Reset"
          onCancel={() => setConfirmingReset(false)}
          onConfirm={() => {
            setConfirmingReset(false);
            void onReset();
          }}
        />
      )}
    </div>
  );
}

function Customize({
  onChanged,
  onError,
}: {
  onChanged: () => Promise<void>;
  onError: (error: unknown) => void;
}) {
  const [data, setData] = useState<CustomizeData | null>(null);
  const load = useCallback(async () => setData(await invoke<CustomizeData>("get_customize")), []);
  useEffect(() => {
    void load().catch(onError);
  }, [load, onError]);
  if (!data) return <div className="empty">Loading…</div>;
  const update = async (patch: Partial<Settings>) => {
    await invoke("patch_settings", { patch });
    await Promise.all([load(), onChanged()]);
  };
  const toggleProvider = async (id: string, enabled: boolean) => {
    const next = new Set(data.settings.enabled);
    if (enabled) next.add(id);
    else next.delete(id);
    await update({ enabled: [...next] });
    if (enabled) {
      await invoke("refresh_one", { id, force: true });
      await onChanged();
    }
  };
  const move = async (id: string, delta: number) => {
    const order = [...data.settings.order];
    const from = order.indexOf(id);
    const to = Math.max(0, Math.min(order.length - 1, from + delta));
    if (from === to || from < 0) return;
    [order[from], order[to]] = [order[to], order[from]];
    await update({ order });
  };
  const toggleVisible = async (id: string, visible: boolean) => {
    const hidden = new Set(data.settings.hidden_metrics);
    if (visible) hidden.delete(id);
    else hidden.add(id);
    await update({ hidden_metrics: [...hidden] });
  };
  const setPlacement = async (id: string, onDemand: boolean) => {
    const next = new Set(data.settings.on_demand);
    if (onDemand) next.add(id);
    else next.delete(id);
    await update({ on_demand: [...next] });
  };
  const pin = async (providerId: string, widgetId: string) => {
    await invoke("toggle_pin", { providerId, widgetId });
    await Promise.all([load(), onChanged()]);
  };
  return (
    <div className="customize-list">
      {data.providers.map((provider, index) => (
        <section className="card" key={provider.id}>
          <div className="provider-row">
            <ProviderIcon icon={provider.icon} />
            <b>{provider.displayName}</b>
            <span className="grow" />
            <button disabled={index === 0} onClick={() => void move(provider.id, -1).catch(onError)}>↑</button>
            <button disabled={index === data.providers.length - 1} onClick={() => void move(provider.id, 1).catch(onError)}>↓</button>
            <Toggle label={`Enable ${provider.displayName}`} checked={provider.enabled} onChange={(enabled) => void toggleProvider(provider.id, enabled).catch(onError)} />
          </div>
          {provider.enabled && provider.widgets.map((widget) => {
            const visible = !data.settings.hidden_metrics.includes(widget.id);
            const pinned = data.settings.pinned[provider.id]?.includes(widget.id) ?? false;
            return (
              <div className="metric-row" key={widget.id}>
                <Toggle label={`Show ${widget.title}`} checked={visible} onChange={(checked) => void toggleVisible(widget.id, checked).catch(onError)} />
                <span>{widget.title}</span>
                <span className="grow" />
                {visible && <select aria-label={`${widget.title} placement`} value={data.settings.on_demand.includes(widget.id) ? "demand" : "always"} onChange={(event) => void setPlacement(widget.id, event.target.value === "demand").catch(onError)}><option value="always">Always</option><option value="demand">On Demand</option></select>}
                {widget.pinnable ? <button className="star" aria-label={`${pinned ? "Unpin" : "Pin"} ${widget.title}`} onClick={() => void pin(provider.id, widget.id).catch(onError)}>{pinned ? "★" : "☆"}</button> : <span className="star-gap" aria-hidden="true" />}
              </div>
            );
          })}
        </section>
      ))}
    </div>
  );
}

export default function App() {
  const [dashboard, setDashboard] = useState<Dashboard | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [screen, setScreen] = useState<Screen>("dash");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const reportError = useCallback((value: unknown) => setError(errorText(value)), []);
  const load = useCallback(async () => {
    try {
      const [nextDashboard, nextSettings] = await Promise.all([
        invoke<Dashboard>("get_dashboard"),
        invoke<Settings>("get_settings"),
      ]);
      setDashboard(nextDashboard);
      setSettings(nextSettings);
      setError(null);
    } catch (loadError) {
      reportError(loadError);
    }
  }, [reportError]);

  useEffect(() => {
    void load();
    const interval = window.setInterval(() => void load(), 5_000);
    const unlistenDashboard = listen("dashboard-updated", () => void load());
    const unlistenNavigate = listen<string>("navigate", (event) => {
      if (["dash", "customize", "settings"].includes(event.payload)) setScreen(event.payload as Screen);
      void load();
    });
    return () => {
      window.clearInterval(interval);
      void unlistenDashboard.then((unlisten) => unlisten());
      void unlistenNavigate.then((unlisten) => unlisten());
    };
  }, [load]);

  const patch = async (value: Partial<Settings>) => {
    try {
      setSettings(await invoke<Settings>("patch_settings", { patch: value }));
      await load();
    } catch (patchError) {
      reportError(patchError);
    }
  };
  const refreshAll = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      setDashboard(await invoke<Dashboard>("refresh_all", { force: true }));
      setError(null);
    } catch (refreshError) {
      reportError(refreshError);
    } finally {
      setBusy(false);
    }
  }, [busy, reportError]);
  const refreshProvider = async (id: string) => {
    if (busy) return;
    setBusy(true);
    try {
      setDashboard(await invoke<Dashboard>("refresh_one", { id, force: true }));
      setError(null);
    } catch (refreshError) {
      reportError(refreshError);
    } finally {
      setBusy(false);
    }
  };
  const setApiKey = async (provider: string, value: string) => {
    setSettings(await invoke<Settings>("set_api_key", { providerId: provider, value }));
    await load();
  };
  const resetAll = async () => {
    try {
      setSettings(await invoke<Settings>("reset_all_settings"));
      await load();
    } catch (resetError) {
      reportError(resetError);
    }
  };

  useEffect(() => {
    const keydown = (event: KeyboardEvent) => {
      const editing = (event.target as HTMLElement | null)?.closest?.("input, textarea, select");
      if (event.key === "Escape") {
        // Leave a half-typed shortcut or API key recoverable: the first Escape drops focus, a
        // second one — with nothing focused — closes the flyout.
        if (editing instanceof HTMLElement) editing.blur();
        else void invoke("hide_flyout").catch(reportError);
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "r") {
        event.preventDefault();
        void refreshAll();
      }
    };
    window.addEventListener("keydown", keydown);
    return () => window.removeEventListener("keydown", keydown);
  }, [refreshAll, reportError]);

  const scrollRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
  }, [screen]);

  const theme = useResolvedTheme(settings?.theme ?? "system");
  const density = settings?.density ?? "compact";
  return (
    <div className={`shell ${settings?.reduce_animations ? "reduce-motion" : ""}`} data-theme={theme} data-density={density}>
      <header className="topbar">
        {screen !== "dash" && <button className="icon-btn" onClick={() => setScreen("dash")}>Back</button>}
        <div className="brand">{screen === "dash" ? "MultiMeters" : screen === "customize" ? "Customize" : "Settings"}</div>
        <button className="icon-btn" disabled={busy} onClick={() => void refreshAll()}>{busy || dashboard?.refreshing ? "Refreshing…" : "Refresh"}</button>
      </header>

      {error && <div className="error-banner" role="alert"><span>{error}</span><button aria-label="Dismiss error" onClick={() => setError(null)}>×</button></div>}

      {screen === "dash" && dashboard && dashboard.pins.length > 0 && (
        <div className="pins">
          {dashboard.pins.map((pin) => (
            <div className="pin" key={`${pin.provider_id}:${pin.widget_id}`}>
              <div className="pin-title">{pin.title}</div>
              <div className="pin-value">{pin.text}</div>
              {pin.used_ratio != null && <div className="pin-track"><i style={{ width: `${Math.min(100, Math.max(0, pin.used_ratio * 100))}%`, background: pin.color }} /></div>}
            </div>
          ))}
        </div>
      )}

      <main className="scroll" ref={scrollRef}>
        {screen === "dash" && !dashboard && <div className="empty">Loading…</div>}
        {screen === "dash" && dashboard?.providers.length === 0 ? (
          <div className="empty"><p>No providers are enabled.</p><button onClick={() => setScreen("customize")}>Open Customize</button></div>
        ) : (
          screen === "dash" &&
          settings &&
          dashboard && (
            <>
              {dashboard.total_spend && <TotalSpendCard total={dashboard.total_spend} settings={settings} onPatch={patch} />}
              {dashboard.providers.map((provider) => (
                <ProviderCard key={provider.info.id} provider={provider} settings={settings} busy={busy || dashboard.refreshing} onPatch={patch} onRefresh={() => refreshProvider(provider.info.id)} onError={reportError} />
              ))}
            </>
          )
        )}
        {screen === "settings" && settings && <SettingsScreen settings={settings} onPatch={patch} onApiKey={setApiKey} onReset={resetAll} onError={reportError} />}
        {screen === "customize" && <Customize onChanged={load} onError={reportError} />}
      </main>

      <footer className="footer">
        <span>{dashboard ? `v${dashboard.version}` : ""}</span>
        <button className="countdown grow" onClick={() => void refreshAll()}>{dashboard ? nextUpdateLabel(dashboard.next_refresh_in_secs, busy || dashboard.refreshing) : ""}</button>
        <button className={screen === "customize" ? "active" : ""} onClick={() => setScreen("customize")}>Customize</button>
        <button className={screen === "settings" ? "active" : ""} onClick={() => setScreen("settings")}>Settings</button>
        <button onClick={() => void invoke("hide_flyout").catch(reportError)}>Close</button>
      </footer>
    </div>
  );
}
