// ── ShortcutsTab — Per-App Cached Keyboard Shortcuts + Live Discovery ───

import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";

interface CachedShortcutEntry {
  app_name: string;
  shortcuts: string;
}

interface DiscoveredMenuItem {
  menu: string;
  item: string;
  shortcut: string | null;
  enabled: boolean;
  source: string;
}

interface MenuDiscoveryResult {
  app_name: string;
  items: DiscoveredMenuItem[];
  error: string | null;
}

interface FrontmostApp {
  app_name: string;
  window_title: string;
}

export function ShortcutsTab() {
  const [entries, setEntries] = useState<CachedShortcutEntry[]>([]);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);

  // Live discovery state
  const [discovering, setDiscovering] = useState(false);
  const [discoveredItems, setDiscoveredItems] = useState<DiscoveredMenuItem[]>([]);
  const [discoveredApp, setDiscoveredApp] = useState("");
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [autoDiscover, setAutoDiscover] = useState(() => {
    return localStorage.getItem("computer-use-auto-discover") === "true";
  });

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await invoke<CachedShortcutEntry[]>("list_all_cached_shortcuts_cmd");
      setEntries(data);
    } catch (err) {
      console.error("Failed to load shortcuts:", err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void refresh(); }, [refresh]);

  const toggle = (app: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(app)) next.delete(app); else next.add(app);
      return next;
    });
  };

  const deleteApp = async (app: string) => {
    try {
      await invoke("delete_cached_shortcuts_cmd", { appName: app });
      void refresh();
    } catch (err) {
      console.error("Failed to delete:", err);
    }
  };

  const exportAll = async () => {
    try {
      const md = await invoke<string>("export_shortcuts_cmd");
      await navigator.clipboard.writeText(md);
      alert("Shortcuts copied to clipboard as Markdown!");
    } catch (err) {
      console.error("Export failed:", err);
    }
  };

  const clearAll = async () => {
    if (!confirm("Clear all cached shortcuts?")) return;
    try {
      await invoke("clear_shortcuts_cache_cmd");
      void refresh();
    } catch (err) {
      console.error("Clear failed:", err);
    }
  };

  // ── Live Menu Discovery ──────────────────────
  const discoverFrontmost = async () => {
    setDiscovering(true);
    setDiscoveryError(null);
    try {
      const app = await invoke<FrontmostApp>("get_frontmost_app_cmd");
      if (!app.app_name) {
        setDiscoveryError("No frontmost app detected");
        return;
      }
      setDiscoveredApp(app.app_name);
      const result = await invoke<MenuDiscoveryResult>("discover_menu_items_cmd", { appName: app.app_name });
      if (result.error) {
        setDiscoveryError(result.error);
        setDiscoveredItems([]);
      } else {
        setDiscoveredItems(result.items);
      }
    } catch (err) {
      setDiscoveryError(String(err));
    } finally {
      setDiscovering(false);
    }
  };

  const toggleAutoDiscover = () => {
    const next = !autoDiscover;
    setAutoDiscover(next);
    localStorage.setItem("computer-use-auto-discover", String(next));
  };

  const shortcutItems = discoveredItems.filter((i) => i.shortcut);
  const noShortcutItems = discoveredItems.filter((i) => !i.shortcut);

  return (
    <>
      {/* ── Live Menu Discovery ──── */}
      <section className="card">
        <div className="card-head">
          <h2>🔍 Live Menu Discovery</h2>
          <div className="row">
            <button onClick={() => void discoverFrontmost()} disabled={discovering}>
              {discovering ? "Scanning…" : "Discover Frontmost App"}
            </button>
            <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: "0.78rem", color: "var(--muted)" }}>
              <input
                type="checkbox"
                checked={autoDiscover}
                onChange={toggleAutoDiscover}
                style={{ width: 14, height: 14 }}
              />
              Auto-discover on agent start
            </label>
          </div>
        </div>

        <p className="muted" style={{ fontSize: "0.78rem", marginBottom: 8 }}>
          Scans the frontmost app's real menu bar to discover all commands and keyboard shortcuts.
          <br /> Source: <code style={{ background: "rgba(90,176,255,0.12)", color: "#8ed6ff", border: "none" }}>live</code> = real system introspection,
          <code style={{ background: "rgba(255,190,60,0.12)", color: "#ffd080", border: "none" }}>llm</code> = model-generated
        </p>

        {discoveryError && (
          <p style={{ fontSize: "0.78rem", color: "var(--bad)" }}>Error: {discoveryError}</p>
        )}

        {discoveredApp && discoveredItems.length > 0 && (
          <div style={{ marginTop: 8 }}>
            <strong style={{ fontSize: "0.82rem" }}>
              {discoveredApp} — {shortcutItems.length} shortcuts, {noShortcutItems.length} other items
              <span style={{
                marginLeft: 8,
                fontSize: "0.6rem",
                padding: "1px 5px",
                borderRadius: 4,
                background: "rgba(90,176,255,0.12)",
                color: "#8ed6ff",
              }}>
                live
              </span>
            </strong>
            <div style={{ marginTop: 6, display: "grid", gap: 3, maxHeight: 300, overflowY: "auto" }}>
              {shortcutItems.map((item, i) => (
                <div
                  key={`${item.menu}-${item.item}-${i}`}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    fontSize: "0.76rem",
                    padding: "3px 8px",
                    borderRadius: 6,
                    background: "rgba(255,255,255,0.02)",
                    opacity: item.enabled ? 1 : 0.4,
                  }}
                >
                  <span style={{ color: "var(--muted)", minWidth: 80 }}>{item.menu}</span>
                  <span style={{ flex: 1 }}>{item.item}</span>
                  {item.shortcut && (
                    <code style={{
                      background: "rgba(90,176,255,0.12)",
                      color: "#8ed6ff",
                      border: "none",
                      fontSize: "0.72rem",
                      padding: "1px 5px",
                    }}>
                      {item.shortcut}
                    </code>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}
      </section>

      {/* ── Cached (LLM) Shortcuts ──── */}
      <section className="card">
        <div className="card-head">
          <h2>Cached Shortcuts</h2>
          <div className="row">
            <button onClick={() => void refresh()} disabled={loading}>
              {loading ? "Loading…" : "Refresh"}
            </button>
            <button onClick={() => void exportAll()} disabled={entries.length === 0}>
              Export
            </button>
            <button onClick={() => void clearAll()} disabled={entries.length === 0}>
              Clear All
            </button>
          </div>
        </div>

        <p className="muted" style={{ fontSize: "0.78rem", marginBottom: 8 }}>
          Auto-fetched per app during agent runs.
          Source: <code style={{ background: "rgba(255,190,60,0.12)", color: "#ffd080", border: "none" }}>llm</code> (model: <code>llama-3.3-70b-instruct:free</code>)
        </p>

        {entries.length === 0 ? (
          <p className="muted" style={{ fontSize: "0.8rem" }}>
            No shortcuts cached yet. Run the agent to auto-populate, or switch apps while running.
          </p>
        ) : (
          <div style={{ display: "grid", gap: 6 }}>
            {entries.map((entry) => (
              <div
                key={entry.app_name}
                style={{
                  border: "1px solid var(--card-border)",
                  borderRadius: 8,
                  overflow: "hidden",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    padding: "6px 10px",
                    cursor: "pointer",
                    background: expanded.has(entry.app_name) ? "rgba(255,255,255,0.04)" : "transparent",
                  }}
                  onClick={() => toggle(entry.app_name)}
                >
                  <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
                    <strong style={{ fontSize: "0.82rem", textTransform: "capitalize" }}>
                      {expanded.has(entry.app_name) ? "▾" : "▸"} {entry.app_name}
                    </strong>
                    <span style={{
                      fontSize: "0.56rem",
                      padding: "1px 5px",
                      borderRadius: 4,
                      background: "rgba(255,190,60,0.12)",
                      color: "#ffd080",
                    }}>
                      llm
                    </span>
                  </span>
                  <button
                    onClick={(e) => { e.stopPropagation(); void deleteApp(entry.app_name); }}
                    style={{ fontSize: "0.7rem", padding: "1px 6px" }}
                  >
                    ✕
                  </button>
                </div>
                {expanded.has(entry.app_name) && (
                  <pre style={{
                    padding: "6px 12px 10px",
                    margin: 0,
                    fontSize: "0.72rem",
                    lineHeight: 1.5,
                    whiteSpace: "pre-wrap",
                    color: "rgba(200, 200, 200, 0.85)",
                    borderTop: "1px solid var(--card-border)",
                    background: "rgba(0, 0, 0, 0.15)",
                  }}>
                    {entry.shortcuts}
                  </pre>
                )}
              </div>
            ))}
          </div>
        )}
      </section>
    </>
  );
}
