// One place that decides where the flyout's commands go. Inside the Tauri shell they go over
// IPC; under `npm run dev` in a plain browser they go to the local fixture, so the interface can
// be built and reviewed without a Tauri toolchain.
//
// The fixture is reached through a dynamic import guarded by `import.meta.env.DEV`, which is the
// literal `false` in production builds. Rollup folds the branch away and the fixture never
// reaches the shipped bundle — a static import would not, because the fixture builds its sample
// data at module scope and so cannot be treated as side-effect free.
import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";
import { openUrl as tauriOpenUrl } from "@tauri-apps/plugin-opener";

export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const fixture = import.meta.env.DEV && !inTauri ? import("./devFixture") : null;

export function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (fixture) return fixture.then((module) => module.devInvoke(command, args) as T);
  return tauriInvoke<T>(command, args);
}

export function listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
  if (fixture) return Promise.resolve(() => {});
  return tauriListen<T>(event, handler);
}

export function openUrl(url: string): Promise<void> {
  if (fixture) {
    window.open(url, "_blank", "noopener,noreferrer");
    return Promise.resolve();
  }
  return tauriOpenUrl(url);
}
