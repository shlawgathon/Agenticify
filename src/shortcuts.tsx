import { invoke } from "@tauri-apps/api/core";

// ── Types ──────────────────────────────────────────────

export type AppShortcuts = {
  app_name: string;
  shortcuts: string;
  from_cache: boolean;
};

// ── Commands ───────────────────────────────────────────

/**
 * Fetch keyboard shortcuts for a given app name.
 * Uses the Rust backend's LLM-powered cache — first call for a new app
 * hits the API, subsequent calls return cached results instantly.
 */
export async function fetchAppShortcuts(appName: string): Promise<AppShortcuts> {
  return invoke<AppShortcuts>("get_app_shortcuts_cmd", { appName });
}

/**
 * Clear the shortcuts cache (e.g. when resetting the session or
 * if the user wants fresh shortcuts data).
 */
export async function clearShortcutsCache(): Promise<void> {
  return invoke("clear_shortcuts_cache_cmd");
}
