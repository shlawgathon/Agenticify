import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, emitTo, listen } from "@tauri-apps/api/event";
import {
  availableMonitors,
  currentMonitor,
  getCurrentWindow,
  LogicalPosition,
  LogicalSize,
} from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

type Tab = "run" | "sessions" | "dev";

type PermissionState = {
  screen_recording: boolean;
  accessibility: boolean;
};

type EnvStatus = {
  mistral_api_key_loaded: boolean;
  mistral_api_base: string;
};

type MistralAuthStatus = {
  ok: boolean;
  http_status: number | null;
  message: string;
  mistral_api_base: string;
};

type RuntimeState = {
  estop: boolean;
  actions: number;
  max_actions: number;
};

type CaptureFrame = {
  monitor_id: number;
  monitor_origin_x_pt: number;
  monitor_origin_y_pt: number;
  screenshot_w_px: number;
  screenshot_h_px: number;
  scale_factor: number;
  png_path: string;
  capture_ms: number;
};

type KeyAction = {
  key: string;
  direction?: "press" | "release" | "click";
};

type VisionAction = {
  action: "click" | "hotkey" | "type" | "none";
  x_norm: number;
  y_norm: number;
  confidence: number;
  reason: string;
  model_ms: number;
  keys?: KeyAction[];
  text?: string;
};

type RecordingStatus = {
  active: boolean;
  session_id: string | null;
  output_dir: string | null;
  fps: number | null;
  frame_ticks: number;
  started_unix_ms: number | null;
};

type RecordingSummary = {
  session_id: string;
  output_dir: string;
  fps: number;
  frame_ticks: number;
  duration_ms: number;
};

type SessionReplayResult = {
  session_id: string;
  monitor_id: number;
  frame_path: string;
  action: VisionAction;
  clicked: boolean;
};

type AgentCursorEvent = {
  x_pt: number;
  y_pt: number;
  monitor_origin_x_pt: number;
  monitor_origin_y_pt: number;
  phase: "move" | "click" | string;
  unix_ms: number;
};

type OverlayBounds = {
  x: number;
  y: number;
  width: number;
  height: number;
};

type HudUpdate = {
  estop: boolean;
  overlay: boolean;
  keyLoaded: boolean;
  permsReady: boolean;
  instruction: string;
};

type HudActionError = {
  action: string;
  message: string;
};

type AgentStep = {
  phase: "capture" | "thinking" | "click" | "hotkey" | "type" | "done" | "error";
  step: number;
  max_steps: number;
  message: string;
};

const OVERLAY_LABEL = "overlay";
const OVERLAY_QUERY_KEY = "overlay";
const HUD_LABEL = "hud";
const HUD_QUERY_KEY = "hud";
const MAIN_LABEL = "main";
const DEFAULT_HUD_MODEL = "mistralai/ministral-14b-2512";
const HUD_WIDTH = 460;
const HUD_HEIGHT = 48;

const FLOW = [
  "UI calls `capture_primary_cmd`; Rust captures the primary display and writes a PNG in temp storage.",
  "UI sends `png_path` + instruction to `infer_click_cmd` (instruction already includes your Task Context).",
  "Rust calls Mistral Vision with strict JSON schema and parses: `action`, `x_norm`, `y_norm`, `confidence`, `reason`.",
  "UI executes click only when `action=click`; Rust still enforces confidence threshold, E-STOP, and max-action cap before clicking.",
  "Rust converts normalized coordinates to macOS logical points, performs real click with enigo, then emits `agent_cursor_event` for overlay visuals.",
];

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = Math.floor(ms / 1000);
  const rem = ms % 1000;
  if (s < 60) return `${s}.${Math.floor(rem / 100)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${s % 60}s`;
}

async function computeOverlayBounds(): Promise<OverlayBounds> {
  const monitors = await availableMonitors();
  if (monitors.length === 0) {
    return { x: 0, y: 0, width: 1200, height: 800 };
  }

  let minX = Number.POSITIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  let maxX = Number.NEGATIVE_INFINITY;
  let maxY = Number.NEGATIVE_INFINITY;

  for (const m of monitors) {
    const scale = m.scaleFactor || 1;
    const x = m.position.x / scale;
    const y = m.position.y / scale;
    const w = m.size.width / scale;
    const h = m.size.height / scale;

    minX = Math.min(minX, x);
    minY = Math.min(minY, y);
    maxX = Math.max(maxX, x + w);
    maxY = Math.max(maxY, y + h);
  }

  return {
    x: minX,
    y: minY,
    width: Math.max(1, maxX - minX),
    height: Math.max(1, maxY - minY),
  };
}

async function ensureOverlayWindow(): Promise<WebviewWindow> {
  const bounds = await computeOverlayBounds();
  const overlayUrl = `/?${OVERLAY_QUERY_KEY}=1&ox=${encodeURIComponent(bounds.x)}&oy=${encodeURIComponent(bounds.y)}`;
  const configure = async (win: WebviewWindow) => {
    await win.setPosition(new LogicalPosition(bounds.x, bounds.y));
    await win.setSize(new LogicalSize(bounds.width, bounds.height));
    await win.setAlwaysOnTop(true);
    await win.setIgnoreCursorEvents(true);
    await win.setFocusable(false);
  };

  const existing = await WebviewWindow.getByLabel(OVERLAY_LABEL);
  if (existing) {
    await configure(existing);
    await emitTo(OVERLAY_LABEL, "overlay_bounds", {
      x: bounds.x,
      y: bounds.y,
    }).catch(() => undefined);
    return existing;
  }

  const overlay = new WebviewWindow(OVERLAY_LABEL, {
    url: overlayUrl,
    transparent: true,
    decorations: false,
    alwaysOnTop: true,
    visibleOnAllWorkspaces: true,
    skipTaskbar: true,
    visible: false,
    focus: false,
    resizable: false,
    shadow: false,
    x: bounds.x,
    y: bounds.y,
    width: bounds.width,
    height: bounds.height,
  });

  overlay.once("tauri://created", async () => {
    await configure(overlay).catch(() => undefined);
  });

  return overlay;
}

async function ensureHudWindow(): Promise<WebviewWindow> {
  const monitor = await currentMonitor();
  const scale = monitor?.scaleFactor || 1;
  const monX = monitor ? monitor.position.x / scale : 0;
  const monY = monitor ? monitor.position.y / scale : 0;
  const monW = monitor ? monitor.size.width / scale : 1400;
  const width = HUD_WIDTH;
  const height = HUD_HEIGHT;
  const x = monX + monW / 2 - width / 2;
  const y = monY + 20;

  const configure = async (win: WebviewWindow) => {
    await win.setPosition(new LogicalPosition(x, y));
    await win.setSize(new LogicalSize(width, height));
    await win.setMaxSize(new LogicalSize(width, height)).catch(() => undefined);
    await win.setMinSize(new LogicalSize(width, height)).catch(() => undefined);
    await win.show();
    await win.setAlwaysOnTop(true);
    await win.setFocusable(false);
    await win
      .setBackgroundColor({ red: 0, green: 0, blue: 0, alpha: 0 })
      .catch(() => undefined);
  };

  const existing = await WebviewWindow.getByLabel(HUD_LABEL);
  if (existing) {
    await configure(existing);
    return existing;
  }

  const hud = new WebviewWindow(HUD_LABEL, {
    url: `/?${HUD_QUERY_KEY}=1`,
    transparent: true,
    decorations: false,
    alwaysOnTop: true,
    visibleOnAllWorkspaces: true,
    skipTaskbar: true,
    focus: true,
    resizable: false,
    shadow: false,
    x,
    y,
    width,
    height,
  });

  hud.once("tauri://created", async () => {
    await configure(hud).catch(() => undefined);
  });

  return hud;
}

async function revealMainWindow(): Promise<boolean> {
  const main = await WebviewWindow.getByLabel(MAIN_LABEL);
  if (!main) return false;
  await main.show().catch(() => undefined);
  const minimized = await main.isMinimized().catch(() => false);
  if (minimized) {
    await main.unminimize().catch(() => undefined);
  }
  await main.setFocus().catch(() => undefined);
  return true;
}

function OverlayWindow() {
  const qs = new URLSearchParams(window.location.search);
  const ox = Number(qs.get("ox") ?? "0");
  const oy = Number(qs.get("oy") ?? "0");
  const originRef = useRef<{ x: number; y: number }>({
    x: Number.isFinite(ox) ? ox : 0,
    y: Number.isFinite(oy) ? oy : 0,
  });

  const [cursor, setCursor] = useState<{
    visible: boolean;
    x: number;
    y: number;
    phase: string;
  }>({ visible: false, x: 0, y: 0, phase: "move" });

  useLayoutEffect(() => {
    document.documentElement.classList.add("overlay-window");
    document.body.classList.add("overlay-window");
    return () => {
      document.documentElement.classList.remove("overlay-window");
      document.body.classList.remove("overlay-window");
    };
  }, []);

  useEffect(() => {
    let hideTimer: number | undefined;
    let unlistenCursor: (() => void) | undefined;
    let unlistenBounds: (() => void) | undefined;

    void (async () => {
      const win = getCurrentWindow();
      await win.setAlwaysOnTop(true).catch(() => undefined);
      await win.setIgnoreCursorEvents(true).catch(() => undefined);
      await win.setFocusable(false).catch(() => undefined);
      await win.setDecorations(false).catch(() => undefined);
      await win.setShadow(false).catch(() => undefined);
      await win.hide().catch(() => undefined);

      unlistenCursor = await listen<AgentCursorEvent>(
        "agent_cursor_event",
        ({ payload }) => {
          void (async () => {
            const w = getCurrentWindow();
            await w.show().catch(() => undefined);
            await w.setAlwaysOnTop(true).catch(() => undefined);
            await w.setIgnoreCursorEvents(true).catch(() => undefined);
            await w.setFocusable(false).catch(() => undefined);
          })();

          const localX = payload.x_pt - originRef.current.x;
          const localY = payload.y_pt - originRef.current.y;
          setCursor({
            visible: true,
            x: localX,
            y: localY,
            phase: payload.phase,
          });

          if (hideTimer) {
            window.clearTimeout(hideTimer);
          }
          hideTimer = window.setTimeout(() => {
            setCursor((prev) => ({ ...prev, visible: false }));
            void getCurrentWindow()
              .hide()
              .catch(() => undefined);
          }, 1200);
        },
      );

      unlistenBounds = await listen<{ x: number; y: number }>(
        "overlay_bounds",
        ({ payload }) => {
          originRef.current = { x: payload.x, y: payload.y };
        },
      );
    })();

    return () => {
      if (hideTimer) window.clearTimeout(hideTimer);
      if (unlistenCursor) unlistenCursor();
      if (unlistenBounds) unlistenBounds();
    };
  }, []);

  return (
    <main className="overlay-root">
      {cursor.visible ? (
        <div
          className={`agent-cursor ${cursor.phase === "click" ? "click" : "move"}`}
          style={{ left: `${cursor.x}px`, top: `${cursor.y}px` }}
        >
          <span className="agent-cursor-ring" />
          <span className="agent-cursor-dot" />
        </div>
      ) : null}
    </main>
  );
}

function HudWindow() {
  const [status, setStatus] = useState<HudUpdate>({
    estop: false,
    overlay: true,
    keyLoaded: false,
    permsReady: false,
    instruction: "Waiting for command",
  });
  const [recordingActive, setRecordingActive] = useState(false);
  const [recordingTicks, setRecordingTicks] = useState(0);
  const [busy, setBusy] = useState({
    start: false,
    stop: false,
    run: false,
  });
  const loopStopRef = useRef(false);
  const [looping, setLooping] = useState(false);

  const refreshRecordingState = async () => {
    try {
      const status = await invoke<RecordingStatus>("recording_status_cmd");
      setRecordingActive(status.active);
      setRecordingTicks(status.frame_ticks);
    } catch {
      // best-effort for HUD
    }
  };

  useLayoutEffect(() => {
    document.documentElement.classList.add("hud-window");
    document.body.classList.add("hud-window");
    return () => {
      document.documentElement.classList.remove("hud-window");
      document.body.classList.remove("hud-window");
    };
  }, []);

  useEffect(() => {
    let unlistenStatus: (() => void) | undefined;

    void (async () => {
      const win = getCurrentWindow();
      await win
        .setSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT))
        .catch(() => undefined);
      await win
        .setMaxSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT))
        .catch(() => undefined);
      await win
        .setMinSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT))
        .catch(() => undefined);
      await win.show().catch(() => undefined);
      await win.setAlwaysOnTop(true).catch(() => undefined);
      await win.setDecorations(false).catch(() => undefined);
      await win.setShadow(false).catch(() => undefined);
      await win.setFocusable(true).catch(() => undefined);
      await win
        .setBackgroundColor({ red: 0, green: 0, blue: 0, alpha: 0 })
        .catch(() => undefined);

      unlistenStatus = await listen<HudUpdate>("hud_update", ({ payload }) => {
        setStatus(payload);
      });
      await refreshRecordingState();
    })();

    const id = window.setInterval(() => {
      void refreshRecordingState();
    }, 1000);

    return () => {
      window.clearInterval(id);
      if (unlistenStatus) unlistenStatus();
    };
  }, []);

  const startRecordingFromHud = async () => {
    setBusy((b) => ({ ...b, start: true }));
    try {
      await invoke("start_recording_session_cmd", { fps: 2 });
      await refreshRecordingState();
    } catch (err) {
      await emit("hud_action_error", {
        action: "record_start",
        message: String(err),
      } satisfies HudActionError).catch(() => undefined);
    } finally {
      setBusy((b) => ({ ...b, start: false }));
    }
  };

  const stopRecordingFromHud = async () => {
    setBusy((b) => ({ ...b, stop: true }));
    try {
      await invoke<RecordingSummary>("stop_recording_session_cmd");
      await refreshRecordingState();
    } catch (err) {
      await emit("hud_action_error", {
        action: "record_stop",
        message: String(err),
      } satisfies HudActionError).catch(() => undefined);
    } finally {
      setBusy((b) => ({ ...b, stop: false }));
    }
  };

  const runOneShotFromHud = async () => {
    setBusy((b) => ({ ...b, run: true }));
    try {
      const captured = await invoke<CaptureFrame>("capture_primary_cmd");
      const inferred = await invoke<VisionAction>("infer_click_cmd", {
        req: {
          png_path: captured.png_path,
          instruction: status.instruction || "Click the target button",
          model: DEFAULT_HUD_MODEL,
        },
      });

      if (inferred.action === "click") {
        await invoke("execute_real_click_cmd", {
          req: {
            x_norm: inferred.x_norm,
            y_norm: inferred.y_norm,
            screenshot_w_px: captured.screenshot_w_px,
            screenshot_h_px: captured.screenshot_h_px,
            monitor_origin_x_pt: captured.monitor_origin_x_pt,
            monitor_origin_y_pt: captured.monitor_origin_y_pt,
            scale_factor: captured.scale_factor,
            confidence: inferred.confidence,
          },
        });
      } else if (inferred.action === "hotkey" && inferred.keys?.length) {
        await invoke("press_keys_cmd", {
          req: { keys: inferred.keys, delay_ms: 30 },
        });
      } else if (inferred.action === "type" && inferred.text) {
        await invoke("type_text_cmd", { text: inferred.text });
      } else {
        return;
      }
    } catch (err) {
      await emit("hud_action_error", {
        action: "run_once",
        message: String(err),
      } satisfies HudActionError).catch(() => undefined);
    } finally {
      setBusy((b) => ({ ...b, run: false }));
    }
  };

  const runAgentLoopFromHud = async () => {
    loopStopRef.current = false;
    setLooping(true);
    setBusy((b) => ({ ...b, run: true }));
    const MAX_STEPS = 30;
    try {
      for (let step = 1; step <= MAX_STEPS; step++) {
        if (loopStopRef.current) break;

        const captured = await invoke<CaptureFrame>("capture_primary_cmd");
        const inferred = await invoke<VisionAction>("infer_click_cmd", {
          req: {
            png_path: captured.png_path,
            instruction: status.instruction || "Click the target button",
            model: DEFAULT_HUD_MODEL,
          },
        });

        if (inferred.action === "none" || loopStopRef.current) break;

        if (inferred.action === "click") {
          await invoke("execute_real_click_cmd", {
            req: {
              x_norm: inferred.x_norm,
              y_norm: inferred.y_norm,
              screenshot_w_px: captured.screenshot_w_px,
              screenshot_h_px: captured.screenshot_h_px,
              monitor_origin_x_pt: captured.monitor_origin_x_pt,
              monitor_origin_y_pt: captured.monitor_origin_y_pt,
              scale_factor: captured.scale_factor,
              confidence: inferred.confidence,
            },
          });
        } else if (inferred.action === "hotkey" && inferred.keys?.length) {
          await invoke("press_keys_cmd", {
            req: { keys: inferred.keys, delay_ms: 30 },
          });
        }

        // Brief pause between steps to let the UI settle
        await new Promise((r) => setTimeout(r, 800));
      }
    } catch (err) {
      await emit("hud_action_error", {
        action: "agent_loop",
        message: String(err),
      } satisfies HudActionError).catch(() => undefined);
    } finally {
      setLooping(false);
      setBusy((b) => ({ ...b, run: false }));
    }
  };

  const toggleOverlayFromHud = async () => {
    try {
      if (status.overlay) {
        const overlay = await WebviewWindow.getByLabel(OVERLAY_LABEL);
        if (overlay) {
          await overlay.hide().catch(() => undefined);
        }
        setStatus((s) => ({ ...s, overlay: false }));
        await emit("hud_overlay_state_changed", { enabled: false }).catch(
          () => undefined,
        );
      } else {
        await ensureOverlayWindow();
        setStatus((s) => ({ ...s, overlay: true }));
        await emit("hud_overlay_state_changed", { enabled: true }).catch(
          () => undefined,
        );
      }
    } catch (err) {
      await emit("hud_action_error", {
        action: "toggle_overlay",
        message: String(err),
      } satisfies HudActionError).catch(() => undefined);
    }
  };

  const openMainFromHud = async () => {
    const main = await WebviewWindow.getByLabel(MAIN_LABEL);
    if (!main) {
      await emit("hud_action_error", {
        action: "open_main",
        message: "Main window not found (label=main)",
      } satisfies HudActionError).catch(() => undefined);
      return;
    }
    const visible = await main.isVisible().catch(() => false);
    if (visible) {
      await main.hide().catch(() => undefined);
    } else {
      await revealMainWindow();
    }
  };

  const suppressDashboard = () => {
    void (async () => {
      const main = await WebviewWindow.getByLabel(MAIN_LABEL);
      if (main) await main.hide().catch(() => undefined);
    })();
  };

  const [hudHoverOnly, setHudHoverOnly] = useState(false);
  const [hudCollapsed, setHudCollapsed] = useState(false);

  const [hudPanel, setHudPanel] = useState<"none" | "activity" | "command">("none");
  const [activityFeed, setActivityFeed] = useState<AgentStep[]>([]);
  const [hudInstruction, setHudInstruction] = useState("");
  const [hudContext, setHudContext] = useState("");
  const activityEndRef = useRef<HTMLDivElement>(null);

  const pushActivity = (step: AgentStep) => {
    setActivityFeed((f) => [...f.slice(-30), step]);
    setTimeout(() => activityEndRef.current?.scrollIntoView({ behavior: "smooth" }), 50);
  };

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<AgentStep>("agent_step", ({ payload }) => {
        pushActivity(payload);
      });
    })();
    return () => { if (unlisten) unlisten(); };
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<boolean>("hud_hover_mode", ({ payload }) => {
        setHudHoverOnly(payload);
      });
    })();
    return () => { if (unlisten) unlisten(); };
  }, []);

  const togglePanel = async (panel: "activity" | "command") => {
    const win = getCurrentWindow();
    if (hudPanel === panel) {
      setHudPanel("none");
      await win.setFocusable(false).catch(() => undefined);
      await win.setSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
      await win.setMaxSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
      await win.setMinSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
    } else {
      setHudPanel(panel);
      const h = panel === "activity" ? 180 : 200;
      await win.setFocusable(panel === "command").catch(() => undefined);
      await win.setMinSize(new LogicalSize(HUD_WIDTH, h)).catch(() => undefined);
      await win.setMaxSize(new LogicalSize(HUD_WIDTH, h)).catch(() => undefined);
      await win.setSize(new LogicalSize(HUD_WIDTH, h)).catch(() => undefined);
    }
  };

  const runAgentLoopFromHudWithTracking = async () => {
    loopStopRef.current = false;
    setLooping(true);
    setBusy((b) => ({ ...b, run: true }));
    const MAX_STEPS = 30;
    const inst = hudInstruction || status.instruction || "Click the target button";
    const stepHistory: string[] = [];
    try {
      for (let step = 1; step <= MAX_STEPS; step++) {
        if (loopStopRef.current) break;

        const captureStep: AgentStep = { phase: "capture", step, max_steps: MAX_STEPS, message: "Capturing screen..." };
        await emit("agent_step", captureStep).catch(() => undefined);

        const captured = await invoke<CaptureFrame>("capture_primary_cmd");

        const thinkStep: AgentStep = { phase: "thinking", step, max_steps: MAX_STEPS, message: "Model is thinking..." };
        await emit("agent_step", thinkStep).catch(() => undefined);

        const inferred = await invoke<VisionAction>("infer_click_cmd", {
          req: {
            png_path: captured.png_path,
            instruction: inst,
            model: DEFAULT_HUD_MODEL,
            step_context: stepHistory.length > 0 ? stepHistory.join("\n") : undefined,
          },
        });

        if (inferred.action === "none" || loopStopRef.current) {
          const doneStep: AgentStep = { phase: "done", step, max_steps: MAX_STEPS, message: inferred.reason };
          await emit("agent_step", doneStep).catch(() => undefined);
          break;
        }

        if (inferred.action === "click") {
          await invoke("execute_real_click_cmd", {
            req: {
              x_norm: inferred.x_norm, y_norm: inferred.y_norm,
              screenshot_w_px: captured.screenshot_w_px, screenshot_h_px: captured.screenshot_h_px,
              monitor_origin_x_pt: captured.monitor_origin_x_pt, monitor_origin_y_pt: captured.monitor_origin_y_pt,
              scale_factor: captured.scale_factor, confidence: inferred.confidence,
            },
          });
          stepHistory.push(`Step ${step}: clicked at (${inferred.x_norm}, ${inferred.y_norm}) — ${inferred.reason}`);
          const s: AgentStep = { phase: "click", step, max_steps: MAX_STEPS, message: inferred.reason };
          await emit("agent_step", s).catch(() => undefined);
        } else if (inferred.action === "hotkey" && inferred.keys?.length) {
          await invoke("press_keys_cmd", { req: { keys: inferred.keys, delay_ms: 30 } });
          const keyDesc = inferred.keys.map((k) => k.key).join("+");
          stepHistory.push(`Step ${step}: hotkey ${keyDesc} — ${inferred.reason}`);
          const s: AgentStep = { phase: "hotkey", step, max_steps: MAX_STEPS, message: `${keyDesc} — ${inferred.reason}` };
          await emit("agent_step", s).catch(() => undefined);
        } else if (inferred.action === "type" && inferred.text) {
          await invoke("type_text_cmd", { text: inferred.text });
          stepHistory.push(`Step ${step}: typed "${inferred.text}" — ${inferred.reason}`);
          const s: AgentStep = { phase: "type", step, max_steps: MAX_STEPS, message: `"${inferred.text}" — ${inferred.reason}` };
          await emit("agent_step", s).catch(() => undefined);
        }

        await new Promise((r) => setTimeout(r, 800));
      }
    } catch (err) {
      const errStep: AgentStep = { phase: "error", step: 0, max_steps: MAX_STEPS, message: String(err) };
      await emit("agent_step", errStep).catch(() => undefined);
    } finally {
      setLooping(false);
      setBusy((b) => ({ ...b, run: false }));
    }
  };

  const toggleCollapse = async () => {
    const win = getCurrentWindow();
    const monitor = await currentMonitor();
    const scale = monitor?.scaleFactor || 1;
    const monX = monitor ? monitor.position.x / scale : 0;
    const monW = monitor ? monitor.size.width / scale : 1400;
    const monY = monitor ? monitor.position.y / scale : 0;
    const next = !hudCollapsed;
    setHudCollapsed(next);
    if (next) {
      setHudPanel("none");
      const w = 48, h = 48;
      await win.setMinSize(new LogicalSize(w, h)).catch(() => undefined);
      await win.setMaxSize(new LogicalSize(w, h)).catch(() => undefined);
      await win.setSize(new LogicalSize(w, h)).catch(() => undefined);
      await win.setPosition(new LogicalPosition(monX + monW / 2 - w / 2, monY + 20)).catch(() => undefined);
    } else {
      await win.setMinSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
      await win.setMaxSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
      await win.setSize(new LogicalSize(HUD_WIDTH, HUD_HEIGHT)).catch(() => undefined);
      await win.setPosition(new LogicalPosition(monX + monW / 2 - HUD_WIDTH / 2, monY + 20)).catch(() => undefined);
    }
  };

  return (
    <main
      className={`hud-root ${hudPanel !== "none" ? "hud-expanded" : ""} ${hudHoverOnly ? "hud-hover-only" : ""} ${hudCollapsed ? "hud-collapsed" : ""}`}
      onClick={suppressDashboard}
    >
      <section
        className={`hud-pill ${hudPanel !== "none" ? "expanded" : ""} ${hudCollapsed ? "collapsed" : ""}`}
        onMouseDown={(e) => { if (!hudPanel || hudPanel !== "command") e.preventDefault(); }}
        title={hudCollapsed ? "Click to expand" : "Agenticify HUD"}
      >
        {hudCollapsed ? (
          <button
            className="hud-btn hud-btn-icon"
            onClick={() => void toggleCollapse()}
            title="Expand HUD"
            type="button"
          >
            <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
              <path d="M9 18l6-6-6-6" />
            </svg>
          </button>
        ) : (
          <>
        <div className="hud-main">
          <div className="hud-controls">
            <button
              className="hud-btn hud-btn-icon"
              onClick={(e) => { e.stopPropagation(); void openMainFromHud(); }}
              title="Show main menu"
              aria-label="Show main menu"
              type="button"
            >
              <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
                <path d="M5 7h14" />
                <path d="M5 12h14" />
                <path d="M5 17h14" />
              </svg>
            </button>
            <button
              className={`hud-btn hud-btn-icon ${hudPanel === "command" ? "active" : ""}`}
              onClick={(e) => { e.stopPropagation(); void togglePanel("command"); }}
              title="Set instruction & run agent loop"
              aria-label="Set instruction & run agent loop"
              type="button"
            >
              <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
                <path d="M12 20h9" />
                <path d="M16.5 3.5a2.121 2.121 0 1 1 3 3L7 19l-4 1 1-4Z" />
              </svg>
            </button>
            <button
              className={`hud-btn hud-btn-icon ${hudPanel === "activity" ? "active" : ""}`}
              onClick={(e) => { e.stopPropagation(); void togglePanel("activity"); }}
              title="Toggle model activity feed"
              aria-label="Toggle model activity feed"
              type="button"
            >
              <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
                <path d="M6 9l6 6 6-6" />
              </svg>
            </button>
            <button
              className={`hud-btn hud-btn-icon ${status.overlay ? "" : "overlay-off"}`}
              onClick={(e) => { e.stopPropagation(); void toggleOverlayFromHud(); }}
              title="Toggle visual overlay cursor layer"
              aria-label="Toggle visual overlay cursor layer"
              type="button"
            >
              <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
                <circle cx="12" cy="12" r="9" />
                <circle cx="12" cy="12" r="4.5" />
                <circle cx="12" cy="12" r="1.2" fill="currentColor" stroke="none" />
                <path d="M12 2v4" />
                <path d="M12 18v4" />
                <path d="M2 12h4" />
                <path d="M18 12h4" />
              </svg>
            </button>
          </div>
          <div className="hud-right">
            {looping && (
              <button
                className="hud-btn"
                onClick={(e) => { e.stopPropagation(); loopStopRef.current = true; }}
                title="Stop agent loop"
                style={{ fontSize: "0.5rem", padding: "2px 5px" }}
              >
                STOP
              </button>
            )}
            <span
              className={`hud-state ${recordingActive ? "ok" : "bad"}`}
              title={recordingActive ? `Recording (${recordingTicks} ticks)` : "Recording idle"}
            >
              {recordingActive ? "REC" : "IDLE"}
            </span>
            <span
              className={`hud-state ${status.permsReady && status.keyLoaded ? "ok" : "bad"}`}
              title={status.permsReady && status.keyLoaded ? "Ready" : "Setup needed"}
            >
              {status.permsReady && status.keyLoaded ? "READY" : "SETUP"}
            </span>

            <button
              className="hud-btn hud-btn-icon"
              onClick={(e) => { e.stopPropagation(); void toggleCollapse(); }}
              title="Collapse HUD"
              type="button"
            >
              <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
                <path d="M15 18l-6-6 6-6" />
              </svg>
            </button>
          </div>
        </div>

        {hudPanel === "activity" && (
          <div className="hud-dropdown">
            {activityFeed.length === 0 ? (
              <div className="hud-activity-item" style={{ opacity: 0.5 }}>No activity yet</div>
            ) : (
              activityFeed.map((a, i) => (
                <div key={i} className="hud-activity-item">
                  <span className={`phase-tag ${a.phase}`}>{a.phase}</span>
                  <span>{a.step > 0 ? `[${a.step}/${a.max_steps}] ` : ""}{a.message}</span>
                </div>
              ))
            )}
            <div ref={activityEndRef} />
          </div>
        )}

        {hudPanel === "command" && (
          <div className="hud-input-panel">
            <input
              placeholder="Instruction (e.g. Open Chrome and go to google.com)"
              value={hudInstruction}
              onChange={(e) => setHudInstruction(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void runAgentLoopFromHudWithTracking();
                }
              }}
            />
            <textarea
              placeholder="Task context (optional)"
              rows={2}
              value={hudContext}
              onChange={(e) => setHudContext(e.target.value)}
            />
            <div className="hud-input-actions">
              <button
                className="hud-btn"
                disabled={busy.run || !hudInstruction.trim()}
                onClick={() => void runAgentLoopFromHudWithTracking()}
              >
                {looping ? "Running..." : "▶ Run Loop"}
              </button>
            </div>
          </div>
        )}
        </>
        )}
      </section>
    </main>
  );
}

function MainApp() {
  const [tab, setTab] = useState<Tab>("run");
  const [darkMode, setDarkMode] = useState<boolean>(
    () => window.matchMedia("(prefers-color-scheme: dark)").matches,
  );
  const [overlayEnabled, setOverlayEnabled] = useState(true);
  const [hudEnabled, setHudEnabled] = useState(true);

  const [permissions, setPermissions] = useState<PermissionState | null>(null);
  const [envStatus, setEnvStatus] = useState<EnvStatus | null>(null);
  const [apiAuth, setApiAuth] = useState<MistralAuthStatus | null>(null);
  const [runtime, setRuntime] = useState<RuntimeState | null>(null);
  const [capture, setCapture] = useState<CaptureFrame | null>(null);
  const [vision, setVision] = useState<VisionAction | null>(null);
  const [instruction, setInstruction] = useState("Click the Save button");
  const [taskContext, setTaskContext] = useState(
    "Goal: complete the task using clicks and keyboard shortcuts.\nUse Cmd+Tab to switch apps if the target app is not visible.\nReturn action=none only when the goal is fully achieved.",
  );
  const [model, setModel] = useState("mistralai/ministral-14b-2512");

  const [recordingStatus, setRecordingStatus] =
    useState<RecordingStatus | null>(null);
  const [recordingSummary, setRecordingSummary] =
    useState<RecordingSummary | null>(null);
  const [sessions, setSessions] = useState<RecordingSummary[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(
    null,
  );
  const [recordingsRoot, setRecordingsRoot] = useState("");
  const [replayResult, setReplayResult] = useState<SessionReplayResult | null>(
    null,
  );
  const [log, setLog] = useState<string[]>([]);
  const [modelActivity, setModelActivity] = useState<AgentStep[]>([]);
  const modelActivityRef = useRef<HTMLDivElement>(null);

  const pushModelActivity = (step: AgentStep) => {
    setModelActivity((a) => [...a.slice(-40), step]);
    setTimeout(() => modelActivityRef.current?.scrollTo({ top: modelActivityRef.current.scrollHeight, behavior: "smooth" }), 50);
  };

  const [busy, setBusy] = useState({
    capture: false,
    infer: false,
    click: false,
    recordStart: false,
    recordStop: false,
    replay: false,
  });
  const loopStopRef = useRef(false);
  const [looping, setLooping] = useState(false);

  const pushLog = (entry: string) => {
    const line = `${new Date().toLocaleTimeString()}  ${entry}`;
    setLog((prev) => [line, ...prev].slice(0, 100));
  };

  const maybeLogRateLimitHint = (err: unknown, scope: string) => {
    const raw = String(err);
    const msg = raw.toLowerCase();
    if (msg.includes("429") || msg.includes("rate limit")) {
      const match = raw.match(/wait ~?(\d+)s/i);
      const wait = match?.[1];
      pushLog(
        wait
          ? `Provider is rate-limited during ${scope}; wait ~${wait}s and retry.`
          : `Provider is rate-limited during ${scope}; wait and retry.`,
      );
    }
  };

  const enforceOverlayPassThrough = async () => {
    const overlay = await WebviewWindow.getByLabel(OVERLAY_LABEL);
    if (!overlay) return;
    await overlay.setIgnoreCursorEvents(true).catch(() => undefined);
    await overlay.setFocusable(false).catch(() => undefined);
    await overlay.setAlwaysOnTop(true).catch(() => undefined);
  };

  const effectiveInstruction = useMemo(() => {
    const base = instruction.trim();
    const ctx = taskContext.trim();
    return ctx ? `${base}\n\nTask Context:\n${ctx}` : base;
  }, [instruction, taskContext]);

  const health = useMemo(() => {
    const keyReady = apiAuth
      ? apiAuth.ok
      : Boolean(envStatus?.mistral_api_key_loaded);
    return {
      permsReady: Boolean(
        permissions?.screen_recording && permissions?.accessibility,
      ),
      keyReady,
      estopOn: Boolean(runtime?.estop),
    };
  }, [permissions, envStatus, apiAuth, runtime]);

  const keyHealthLabel = useMemo(() => {
    if (!envStatus?.mistral_api_key_loaded) return "Missing";
    if (!apiAuth) return "Loaded";
    return apiAuth.ok ? "Valid" : "Invalid";
  }, [envStatus?.mistral_api_key_loaded, apiAuth]);

  useEffect(() => {
    document.documentElement.setAttribute(
      "data-theme",
      darkMode ? "dark" : "light",
    );
  }, [darkMode]);

  useEffect(() => {
    void bootstrap();
  }, []);

  useEffect(() => {
    const id = window.setInterval(() => {
      void refreshRecordingStatus(true);
      void refreshRuntime(true);
    }, 1000);
    return () => window.clearInterval(id);
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<AgentCursorEvent>("agent_cursor_event", ({ payload }) => {
      pushLog(`cursor ${payload.phase} -> (${payload.x_pt}, ${payload.y_pt})`);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  useEffect(() => {
    let unlistenOverlay: (() => void) | undefined;
    let unlistenHudError: (() => void) | undefined;

    void listen<{ enabled: boolean }>(
      "hud_overlay_state_changed",
      ({ payload }) => {
        setOverlayEnabled(payload.enabled);
        pushLog(`overlay ${payload.enabled ? "enabled" : "hidden"} via HUD`);
        if (payload.enabled) {
          void enforceOverlayPassThrough();
        }
      },
    ).then((fn) => {
      unlistenOverlay = fn;
    });

    void listen<HudActionError>("hud_action_error", ({ payload }) => {
      pushLog(`HUD ${payload.action} error: ${payload.message}`);
    }).then((fn) => {
      unlistenHudError = fn;
    });

    return () => {
      if (unlistenOverlay) unlistenOverlay();
      if (unlistenHudError) unlistenHudError();
    };
  }, []);

  useEffect(() => {
    if (!overlayEnabled) return;
    void enforceOverlayPassThrough();
    const id = window.setInterval(() => {
      void enforceOverlayPassThrough();
    }, 1200);
    return () => window.clearInterval(id);
  }, [overlayEnabled]);

  const publishHudUpdate = async () => {
    await emitTo(HUD_LABEL, "hud_update", {
      estop: Boolean(runtime?.estop),
      overlay: overlayEnabled,
      keyLoaded: Boolean(envStatus?.mistral_api_key_loaded),
      permsReady: Boolean(
        permissions?.screen_recording && permissions?.accessibility,
      ),
      instruction: effectiveInstruction,
    } satisfies HudUpdate).catch(() => undefined);
  };

  useEffect(() => {
    void publishHudUpdate();
  }, [
    runtime?.estop,
    overlayEnabled,
    envStatus?.mistral_api_key_loaded,
    permissions?.screen_recording,
    permissions?.accessibility,
    effectiveInstruction,
  ]);

  useEffect(() => {
    if (!hudEnabled) {
      return;
    }
    const id = window.setInterval(() => {
      void publishHudUpdate();
    }, 1000);
    return () => window.clearInterval(id);
  }, [
    hudEnabled,
    runtime?.estop,
    overlayEnabled,
    envStatus?.mistral_api_key_loaded,
    permissions?.screen_recording,
    permissions?.accessibility,
    effectiveInstruction,
  ]);

  const bootstrap = async () => {
    await Promise.all([
      refreshPermissions(true),
      refreshEnv(true),
      refreshRuntime(true),
      refreshRecordingStatus(true),
      loadRecordingsRoot(true),
      loadSessions(true),
    ]);
    if (overlayEnabled) {
      const ok = await ensureOverlayWindow()
        .then(() => true)
        .catch((e) => {
          pushLog(`overlay init error: ${String(e)}`);
          return false;
        });
      if (!ok) {
        setOverlayEnabled(false);
      }
    }
    if (hudEnabled) {
      const hud = await ensureHudWindow().catch((e) => {
        pushLog(`hud init error: ${String(e)}`);
        return null;
      });
      if (hud) {
        const visible = await hud.isVisible().catch(() => false);
        pushLog(`hud visible=${visible}`);
      }
    }
  };

  const refreshPermissions = async (silent = false) => {
    const res = await invoke<PermissionState>("check_permissions_cmd");
    setPermissions(res);
    if (!silent) {
      pushLog(
        `permissions -> screen=${res.screen_recording} accessibility=${res.accessibility}`,
      );
    }
  };

  const requestPermissions = async () => {
    const res = await invoke<PermissionState>("request_permissions_cmd");
    setPermissions(res);
    pushLog(
      `request permissions -> screen=${res.screen_recording} accessibility=${res.accessibility}`,
    );
  };

  const refreshEnv = async (silent = false) => {
    const res = await invoke<EnvStatus>("env_status_cmd");
    setEnvStatus(res);
    if (!res.mistral_api_key_loaded) {
      setApiAuth(null);
    }
    if (!silent) {
      pushLog(
        `env -> MISTRAL_API_KEY ${res.mistral_api_key_loaded ? "loaded" : "missing"}`,
      );
    }
  };

  const validateApiKey = async () => {
    try {
      const status = await invoke<MistralAuthStatus>(
        "validate_mistral_api_key_cmd",
      );
      setApiAuth(status);
      if (status.ok) {
        pushLog(
          `api key valid (${status.http_status ?? "ok"}) @ ${status.mistral_api_base}`,
        );
      } else {
        pushLog(`api key invalid: ${status.message}`);
      }
    } catch (err) {
      pushLog(`api key validation error: ${String(err)}`);
    }
  };

  const refreshRuntime = async (silent = false) => {
    const res = await invoke<RuntimeState>("get_runtime_state_cmd");
    setRuntime(res);
    if (!silent) {
      pushLog(
        `runtime -> estop=${res.estop} actions=${res.actions}/${res.max_actions}`,
      );
    }
  };

  const setEstop = async (enabled: boolean) => {
    const res = await invoke<RuntimeState>("set_estop_cmd", { enabled });
    setRuntime(res);
    pushLog(`E-STOP ${enabled ? "enabled" : "cleared"}`);
  };

  const setOverlay = async (enabled: boolean) => {
    if (enabled) {
      const ok = await ensureOverlayWindow()
        .then(() => true)
        .catch((e) => {
          pushLog(`overlay error: ${String(e)}`);
          return false;
        });
      setOverlayEnabled(ok);
      if (ok) {
        await enforceOverlayPassThrough();
      }
      pushLog(ok ? "overlay enabled" : "overlay failed");
      return;
    }
    const overlay = await WebviewWindow.getByLabel(OVERLAY_LABEL);
    if (overlay) {
      await overlay.hide().catch(() => undefined);
    }
    setOverlayEnabled(false);
    pushLog("overlay hidden");
  };

  const setHud = async (enabled: boolean) => {
    if (enabled) {
      const ok = await ensureHudWindow()
        .then(() => true)
        .catch((e) => {
          pushLog(`hud error: ${String(e)}`);
          return false;
        });
      setHudEnabled(ok);
      pushLog(ok ? "top HUD enabled" : "top HUD failed");
      return;
    }
    const hud = await WebviewWindow.getByLabel(HUD_LABEL);
    if (hud) {
      await hud.hide().catch(() => undefined);
    }
    setHudEnabled(false);
    pushLog("top HUD hidden");
  };

  const previewOverlayCursor = async () => {
    if (overlayEnabled) {
      await ensureOverlayWindow().catch(() => undefined);
    }

    const x =
      capture != null
        ? Math.round(
            capture.monitor_origin_x_pt +
              capture.screenshot_w_px / capture.scale_factor / 2,
          )
        : 240;
    const y =
      capture != null
        ? Math.round(
            capture.monitor_origin_y_pt +
              capture.screenshot_h_px / capture.scale_factor / 2,
          )
        : 240;

    await emitTo(OVERLAY_LABEL, "agent_cursor_event", {
      x_pt: x,
      y_pt: y,
      monitor_origin_x_pt: capture?.monitor_origin_x_pt ?? 0,
      monitor_origin_y_pt: capture?.monitor_origin_y_pt ?? 0,
      phase: "click",
      unix_ms: Date.now(),
    } satisfies AgentCursorEvent).catch((e) =>
      pushLog(`overlay preview error: ${String(e)}`),
    );
    pushLog("overlay cursor preview emitted");
  };

  const capturePrimary = async () => {
    setBusy((b) => ({ ...b, capture: true }));
    try {
      const res = await invoke<CaptureFrame>("capture_primary_cmd");
      setCapture(res);
      pushLog(
        `capture -> ${res.screenshot_w_px}x${res.screenshot_h_px} (${res.capture_ms}ms)`,
      );
    } catch (err) {
      pushLog(`capture error: ${String(err)}`);
    } finally {
      setBusy((b) => ({ ...b, capture: false }));
    }
  };

  const inferClick = async () => {
    if (!capture) {
      pushLog("infer blocked: capture is missing");
      return;
    }
    setBusy((b) => ({ ...b, infer: true }));
    try {
      pushLog(`infer request -> ${capture.png_path}`);
      const res = await invoke<VisionAction>("infer_click_cmd", {
        req: {
          png_path: capture.png_path,
          instruction: effectiveInstruction,
          model,
        },
      });
      setVision(res);
      pushLog(
        `infer -> ${res.action} conf=${res.confidence.toFixed(2)} (${res.model_ms}ms)`,
      );
    } catch (err) {
      pushLog(`infer error: ${String(err)}`);
      maybeLogRateLimitHint(err, "infer");
    } finally {
      setBusy((b) => ({ ...b, infer: false }));
    }
  };

  const executeClick = async () => {
    if (!capture || !vision) {
      pushLog("action blocked: no vision result ready");
      return;
    }
    setBusy((b) => ({ ...b, click: true }));
    try {
      if (vision.action === "click") {
        await invoke("execute_real_click_cmd", {
          req: {
            x_norm: vision.x_norm,
            y_norm: vision.y_norm,
            screenshot_w_px: capture.screenshot_w_px,
            screenshot_h_px: capture.screenshot_h_px,
            monitor_origin_x_pt: capture.monitor_origin_x_pt,
            monitor_origin_y_pt: capture.monitor_origin_y_pt,
            scale_factor: capture.scale_factor,
            confidence: vision.confidence,
          },
        });
        pushLog("real click executed");
      } else if (vision.action === "hotkey" && vision.keys?.length) {
        await invoke("press_keys_cmd", {
          req: { keys: vision.keys, delay_ms: 30 },
        });
        pushLog(`hotkey executed: ${vision.keys.map((k) => k.key).join("+")}`);
      } else if (vision.action === "type" && vision.text) {
        await invoke("type_text_cmd", { text: vision.text });
        pushLog(`typed: "${vision.text}"`);
      } else {
        pushLog("action blocked: no actionable result");
        return;
      }
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`click error: ${String(err)}`);
    } finally {
      setBusy((b) => ({ ...b, click: false }));
    }
  };

  const runLiveOnce = async () => {
    setBusy((b) => ({ ...b, capture: true, infer: true, click: true }));
    try {
      const captured = await invoke<CaptureFrame>("capture_primary_cmd");
      setCapture(captured);
      pushLog(
        `one-shot capture -> ${captured.screenshot_w_px}x${captured.screenshot_h_px}`,
      );

      const inferred = await invoke<VisionAction>("infer_click_cmd", {
        req: {
          png_path: captured.png_path,
          instruction: effectiveInstruction,
          model,
        },
      });
      setVision(inferred);
      pushLog(
        `one-shot infer -> ${inferred.action} conf=${inferred.confidence.toFixed(2)}`,
      );

      if (inferred.action === "click") {
        await invoke("execute_real_click_cmd", {
          req: {
            x_norm: inferred.x_norm,
            y_norm: inferred.y_norm,
            screenshot_w_px: captured.screenshot_w_px,
            screenshot_h_px: captured.screenshot_h_px,
            monitor_origin_x_pt: captured.monitor_origin_x_pt,
            monitor_origin_y_pt: captured.monitor_origin_y_pt,
            scale_factor: captured.scale_factor,
            confidence: inferred.confidence,
          },
        });
        pushLog("one-shot click executed");
      } else if (inferred.action === "hotkey" && inferred.keys?.length) {
        await invoke("press_keys_cmd", {
          req: { keys: inferred.keys, delay_ms: 30 },
        });
        pushLog(`one-shot hotkey executed: ${inferred.keys.map((k) => k.key).join("+")}`);
      } else {
        pushLog("one-shot stopped: model returned no actionable result");
        return;
      }
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`one-shot error: ${String(err)}`);
      maybeLogRateLimitHint(err, "one-shot");
    } finally {
      setBusy((b) => ({ ...b, capture: false, infer: false, click: false }));
    }
  };

    const runAgentLoop = async () => {
    loopStopRef.current = false;
    setLooping(true);
    setBusy((b) => ({ ...b, capture: true, infer: true, click: true }));
    const MAX_STEPS = 30;
    const stepHistory: string[] = [];
    try {
      for (let step = 1; step <= MAX_STEPS; step++) {
        if (loopStopRef.current) {
          pushLog(`agent loop stopped by user at step ${step}`);
          break;
        }

        try {
          const captureStep: AgentStep = { phase: "capture", step, max_steps: MAX_STEPS, message: "Capturing screen..." };
          pushModelActivity(captureStep);
          await emit("agent_step", captureStep).catch(() => undefined);

          pushLog(`agent loop step ${step}/${MAX_STEPS}`);
          const captured = await invoke<CaptureFrame>("capture_primary_cmd");
          setCapture(captured);

          const thinkStep: AgentStep = { phase: "thinking", step, max_steps: MAX_STEPS, message: "Model is thinking..." };
          pushModelActivity(thinkStep);
          await emit("agent_step", thinkStep).catch(() => undefined);

          const inferred = await invoke<VisionAction>("infer_click_cmd", {
            req: {
              png_path: captured.png_path,
              instruction: effectiveInstruction,
              model,
              step_context: stepHistory.length > 0 ? stepHistory.join("\n") : undefined,
            },
          });
          setVision(inferred);
          pushLog(
            `  step ${step} infer -> ${inferred.action} conf=${inferred.confidence.toFixed(2)}`,
          );

          if (inferred.action === "none" || loopStopRef.current) {
            pushLog(
              inferred.action === "none"
                ? `agent loop finished: model returned none ("${inferred.reason}")`
                : `agent loop stopped by user at step ${step}`,
            );
            const doneStep: AgentStep = {
              phase: "done", step, max_steps: MAX_STEPS,
              message: inferred.action === "none" ? inferred.reason : "Stopped by user",
            };
            pushModelActivity(doneStep);
            await emit("agent_step", doneStep).catch(() => undefined);
            break;
          }

          if (inferred.action === "click") {
            await invoke("execute_real_click_cmd", {
              req: {
                x_norm: inferred.x_norm, y_norm: inferred.y_norm,
                screenshot_w_px: captured.screenshot_w_px, screenshot_h_px: captured.screenshot_h_px,
                monitor_origin_x_pt: captured.monitor_origin_x_pt, monitor_origin_y_pt: captured.monitor_origin_y_pt,
                scale_factor: captured.scale_factor, confidence: inferred.confidence,
              },
            });
            stepHistory.push(`Step ${step}: clicked at (${inferred.x_norm}, ${inferred.y_norm}) — ${inferred.reason}`);
            pushLog(`  step ${step} click executed`);
            const s: AgentStep = { phase: "click", step, max_steps: MAX_STEPS, message: inferred.reason };
            pushModelActivity(s); await emit("agent_step", s).catch(() => undefined);
          } else if (inferred.action === "hotkey" && inferred.keys?.length) {
            await invoke("press_keys_cmd", {
              req: { keys: inferred.keys, delay_ms: 30 },
            });
            const keyDesc = inferred.keys.map((k) => k.key).join("+");
            stepHistory.push(`Step ${step}: hotkey ${keyDesc} — ${inferred.reason}`);
            pushLog(`  step ${step} hotkey: ${keyDesc}`);
            const s: AgentStep = { phase: "hotkey", step, max_steps: MAX_STEPS, message: `${keyDesc} — ${inferred.reason}` };
            pushModelActivity(s); await emit("agent_step", s).catch(() => undefined);
          } else if (inferred.action === "type" && inferred.text) {
            await invoke("type_text_cmd", { text: inferred.text });
            stepHistory.push(`Step ${step}: typed "${inferred.text}" — ${inferred.reason}`);
            pushLog(`  step ${step} typed: "${inferred.text}"`);
            const s: AgentStep = { phase: "type", step, max_steps: MAX_STEPS, message: `"${inferred.text}" — ${inferred.reason}` };
            pushModelActivity(s); await emit("agent_step", s).catch(() => undefined);
          }

          await refreshRuntime(true);
        } catch (stepErr) {
          pushLog(`  step ${step} error (continuing): ${String(stepErr)}`);
          stepHistory.push(`Step ${step}: ERROR — ${String(stepErr)}`);
          const errStep: AgentStep = { phase: "error", step, max_steps: MAX_STEPS, message: String(stepErr) };
          pushModelActivity(errStep); await emit("agent_step", errStep).catch(() => undefined);
          maybeLogRateLimitHint(stepErr, `agent-loop-step-${step}`);
        }
        // Brief pause to let the UI settle before next capture
        await new Promise((r) => setTimeout(r, 800));
      }
      pushLog("agent loop complete");
    } catch (err) {
      pushLog(`agent loop error: ${String(err)}`);
      maybeLogRateLimitHint(err, "agent-loop");
    } finally {
      setLooping(false);
      setBusy((b) => ({ ...b, capture: false, infer: false, click: false }));
    }
  };

  const refreshRecordingStatus = async (silent = false) => {
    try {
      const status = await invoke<RecordingStatus>("recording_status_cmd");
      setRecordingStatus(status);
      if (!silent) {
        pushLog(
          `recording -> active=${status.active} ticks=${status.frame_ticks}`,
        );
      }
    } catch (err) {
      if (!silent) {
        pushLog(`recording status error: ${String(err)}`);
      }
    }
  };

  const startRecording = async () => {
    setBusy((b) => ({ ...b, recordStart: true }));
    try {
      const status = await invoke<RecordingStatus>(
        "start_recording_session_cmd",
        { fps: 2 },
      );
      setRecordingStatus(status);
      setRecordingSummary(null);
      setReplayResult(null);
      pushLog(
        `recording started -> ${status.session_id ?? "unknown"} @${status.fps ?? 2}fps`,
      );
    } catch (err) {
      pushLog(`recording start error: ${String(err)}`);
    } finally {
      setBusy((b) => ({ ...b, recordStart: false }));
    }
  };

  const stopRecording = async () => {
    setBusy((b) => ({ ...b, recordStop: true }));
    try {
      const summary = await invoke<RecordingSummary>(
        "stop_recording_session_cmd",
      );
      setRecordingSummary(summary);
      pushLog(
        `recording stopped -> ${summary.frame_ticks} ticks in ${formatDuration(summary.duration_ms)}`,
      );
      await refreshRecordingStatus(true);
      await loadSessions(true);
      setSelectedSessionId(summary.session_id);
    } catch (err) {
      pushLog(`recording stop error: ${String(err)}`);
    } finally {
      setBusy((b) => ({ ...b, recordStop: false }));
    }
  };

  const loadRecordingsRoot = async (silent = false) => {
    try {
      const root = await invoke<string>("recordings_root_cmd");
      setRecordingsRoot(root);
      if (!silent) {
        pushLog(`recordings root -> ${root}`);
      }
    } catch (err) {
      if (!silent) {
        pushLog(`recordings root error: ${String(err)}`);
      }
    }
  };

  const loadSessions = async (silent = false) => {
    try {
      const res = await invoke<RecordingSummary[]>(
        "list_recording_sessions_cmd",
      );
      setSessions(res);
      if (res.length > 0 && !selectedSessionId) {
        setSelectedSessionId(res[0].session_id);
      }
      if (!silent) {
        pushLog(`sessions loaded -> ${res.length} session(s)`);
      }
    } catch (err) {
      if (!silent) {
        pushLog(`sessions load error: ${String(err)}`);
      }
    }
  };

  const openPath = async (path: string) => {
    if (!path) return;
    await invoke("open_path_cmd", { path }).catch((e) =>
      pushLog(`open path error: ${String(e)}`),
    );
  };

  const replaySelectedSession = async () => {
    if (!selectedSessionId) {
      pushLog("replay blocked: select a session first");
      return;
    }
    setBusy((b) => ({ ...b, replay: true }));
    try {
      const res = await invoke<SessionReplayResult>(
        "replay_recording_session_cmd",
        {
          req: {
            session_id: selectedSessionId,
            instruction: effectiveInstruction,
            model,
          },
        },
      );
      setReplayResult(res);
      setVision(res.action);
      pushLog(
        `session replay -> ${res.session_id} action=${res.action.action} clicked=${res.clicked} monitor=${res.monitor_id}`,
      );
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`session replay error: ${String(err)}`);
      if (String(err).includes("401")) {
        pushLog(
          'Provider auth failed: click "Validate API Key" and check OPENROUTER_API_KEY / MISTRAL_API_KEY in .env',
        );
      }
      maybeLogRateLimitHint(err, "session replay");
    } finally {
      setBusy((b) => ({ ...b, replay: false }));
    }
  };

  return (
    <main className="app">
      <div className="bg bg-a" />
      <div className="bg bg-b" />

      <header className="top card">
        <div>
          <h1>Agenticify</h1>
          <p className="muted">OS-native vision automation with real clicks</p>
        </div>
        <div className="row">
          <button onClick={() => {
            const newVal = !hudEnabled;
            void setHud(newVal);
            if (!newVal) void emit("hud_hover_mode", true);
          }}>
            {hudEnabled ? "Hide Top HUD" : "Show Top HUD"}
          </button>
          <button onClick={() => {
            void emit("hud_hover_mode", !hudEnabled);
          }}>
            Hover Mode
          </button>
          <button onClick={() => setDarkMode((v) => !v)}>
            {darkMode ? "Light Mode" : "Dark Mode"}
          </button>
        </div>
      </header>

      <nav className="tabs card">
        <button
          className={tab === "run" ? "tab active" : "tab"}
          onClick={() => setTab("run")}
        >
          Run
        </button>
        <button
          className={tab === "sessions" ? "tab active" : "tab"}
          onClick={() => setTab("sessions")}
        >
          Sessions
        </button>
        <button
          className={tab === "dev" ? "tab active" : "tab"}
          onClick={() => setTab("dev")}
        >
          Dev Tools
        </button>
      </nav>

      <div className="main-grid">
        <section className="stack">
          {tab === "run" ? (
            <>
              <article className="card">
                <div className="card-head">
                  <h2>Run</h2>
                  <div className="row">
                    <button onClick={() => void refreshPermissions()}>Check Permissions</button>
                    <button onClick={() => void requestPermissions()}>Request</button>
                    <button onClick={() => void validateApiKey()}>Validate API Key</button>
                    <button onClick={() => void refreshRuntime()}>Refresh Runtime</button>
                  </div>
                </div>
                <div className="health-grid">
                  <div className={`health ${health.permsReady ? "ok" : "bad"}`}>
                    <span>Permissions</span>
                    <strong>{health.permsReady ? "Ready" : "Missing"}</strong>
                  </div>
                  <div className={`health ${health.keyReady ? "ok" : "bad"}`}>
                    <span>Provider Key</span>
                    <strong>{keyHealthLabel}</strong>
                  </div>
                  <div className={`health ${health.estopOn ? "bad" : "ok"}`}>
                    <span>E-STOP</span>
                    <strong>{health.estopOn ? "ON" : "OFF"}</strong>
                  </div>
                  <div className={`health ${overlayEnabled ? "ok" : "bad"}`}>
                    <span>Overlay</span>
                    <strong>{overlayEnabled ? "ON" : "OFF"}</strong>
                  </div>
                  <div className={`health ${hudEnabled ? "ok" : "bad"}`}>
                    <span>Top HUD</span>
                    <strong>{hudEnabled ? "Visible" : "Hidden"}</strong>
                  </div>
                  <div className="health">
                    <span>Action Counter</span>
                    <strong>{runtime ? `${runtime.actions}/${runtime.max_actions}` : "n/a"}</strong>
                  </div>
                </div>
              </article>

              <article className="card">
                <h3>Live Command</h3>
                <label>
                  Instruction
                  <input
                    value={instruction}
                    onChange={(e) => setInstruction(e.target.value)}
                  />
                </label>
                <label>
                  Task Context (sent with instruction)
                  <textarea
                    rows={3}
                    value={taskContext}
                    onChange={(e) => setTaskContext(e.target.value)}
                  />
                </label>
                <label>
                  Model
                  <input
                    value={model}
                    onChange={(e) => setModel(e.target.value)}
                  />
                </label>
                <div className="row">
                  <button
                    className="primary"
                    onClick={() => void runLiveOnce()}
                    disabled={busy.capture || busy.infer || busy.click}
                  >
                    Run One-Shot
                  </button>
                  <button
                    className="primary"
                    onClick={() => void runAgentLoop()}
                    disabled={looping || busy.capture || busy.infer || busy.click}
                  >
                    {looping ? "Looping..." : "Run Agent Loop"}
                  </button>
                  {looping && (
                    <button onClick={() => { loopStopRef.current = true; }}>
                      Stop Loop
                    </button>
                  )}
                  <button onClick={() => void setEstop(false)}>Clear E-STOP</button>
                  <button onClick={() => void setEstop(true)}>Force E-STOP</button>
                </div>
                <p className="muted">
                  Global kill switch: <code>Cmd+Shift+Esc</code>
                </p>
              </article>
            </>
          ) : null}

          {tab === "sessions" ? (
            <>
              <article className="card">
                <div className="card-head">
                  <h2>Sessions</h2>
                  <div className="row">
                    <button
                      className="primary"
                      onClick={() => void startRecording()}
                      disabled={
                        busy.recordStart || Boolean(recordingStatus?.active)
                      }
                    >
                      {busy.recordStart ? "Starting..." : "Start Recording"}
                    </button>
                    <button
                      onClick={() => void refreshRecordingStatus()}
                      disabled={busy.recordStart || busy.recordStop}
                    >
                      Refresh
                    </button>
                    <button
                      onClick={() => void stopRecording()}
                      disabled={busy.recordStop || !recordingStatus?.active}
                    >
                      {busy.recordStop ? "Stopping..." : "Stop & Save"}
                    </button>
                  </div>
                </div>
                <div className="json-grid">
                  <div>
                    <small>Recording Status</small>
                    <pre>{JSON.stringify(recordingStatus, null, 2)}</pre>
                  </div>
                  <div>
                    <small>Last Session Summary</small>
                    <pre>{JSON.stringify(recordingSummary, null, 2)}</pre>
                  </div>
                </div>
              </article>

              <article className="card">
                <div className="card-head">
                  <h3>Storage</h3>
                  <div className="row">
                    <button onClick={() => void loadRecordingsRoot()}>
                      Refresh Root
                    </button>
                    <button onClick={() => void loadSessions()}>
                      Refresh Sessions
                    </button>
                    <button
                      onClick={() => void openPath(recordingsRoot)}
                      disabled={!recordingsRoot}
                    >
                      Open Root Folder
                    </button>
                  </div>
                </div>
                <p className="path">
                  {recordingsRoot || "Loading root path..."}
                </p>
              </article>

              <article className="card">
                <h3>Saved Sessions</h3>
                {sessions.length === 0 ? (
                  <p className="muted">No saved sessions yet.</p>
                ) : (
                  <div className="session-list">
                    {sessions.map((s) => (
                      <div
                        key={s.session_id}
                        className={`session-item ${selectedSessionId === s.session_id ? "selected" : ""}`}
                        onClick={() => setSelectedSessionId(s.session_id)}
                        role="button"
                        tabIndex={0}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            setSelectedSessionId(s.session_id);
                          }
                        }}
                      >
                        <div>
                          <strong>{s.session_id}</strong>
                          <small>
                            {s.frame_ticks} frames •{" "}
                            {formatDuration(s.duration_ms)} • {s.fps}fps
                          </small>
                        </div>
                        <button
                          className="open-badge"
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation();
                            void openPath(s.output_dir);
                          }}
                        >
                          Open
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </article>

              <article className="card">
                <h3>Replay Selected Session</h3>
                <label>
                  Instruction
                  <input
                    value={instruction}
                    onChange={(e) => setInstruction(e.target.value)}
                  />
                </label>
                <label>
                  Task Context (sent with instruction)
                  <textarea
                    rows={4}
                    value={taskContext}
                    onChange={(e) => setTaskContext(e.target.value)}
                  />
                </label>
                <label>
                  Model
                  <input
                    value={model}
                    onChange={(e) => setModel(e.target.value)}
                  />
                </label>
                <div className="row">
                  <button
                    className="primary"
                    onClick={() => void replaySelectedSession()}
                    disabled={busy.replay || !selectedSessionId}
                  >
                    {busy.replay ? "Running Replay..." : "Replay Session"}
                  </button>
                  <button
                    onClick={() =>
                      void openPath(
                        sessions.find((s) => s.session_id === selectedSessionId)
                          ?.output_dir ?? "",
                      )
                    }
                    disabled={!selectedSessionId}
                  >
                    Open Session Folder
                  </button>
                </div>
                <pre>{JSON.stringify(replayResult, null, 2)}</pre>
              </article>
            </>
          ) : null}

          {tab === "dev" ? (
            <>
              <article className="card">
                <h3>Step-by-Step Controls</h3>
                <div className="row">
                  <button onClick={() => void capturePrimary()} disabled={busy.capture}>
                    {busy.capture ? "Capturing..." : "Capture"}
                  </button>
                  <button onClick={() => void inferClick()} disabled={busy.infer || !capture}>
                    {busy.infer ? "Inferring..." : "Infer"}
                  </button>
                  <button className="primary" onClick={() => void executeClick()} disabled={busy.click}>
                    {busy.click ? "Clicking..." : "Real Click"}
                  </button>
                </div>
              </article>

              <article className="card">
                <h3>Runtime Data</h3>
                <div className="json-grid">
                  <div>
                    <small>Capture</small>
                    <pre>{JSON.stringify(capture, null, 2)}</pre>
                  </div>
                  <div>
                    <small>Vision</small>
                    <pre>{JSON.stringify(vision, null, 2)}</pre>
                  </div>
                  <div>
                    <small>Permissions</small>
                    <pre>{JSON.stringify(permissions, null, 2)}</pre>
                  </div>
                  <div>
                    <small>Env</small>
                    <pre>{JSON.stringify(envStatus, null, 2)}</pre>
                  </div>
                  <div>
                    <small>Instruction Sent To Provider</small>
                    <pre>{effectiveInstruction}</pre>
                  </div>
                </div>
              </article>

              <article className="card">
                <h2>How AI Knows What To Do</h2>
                <ol className="plain-list">
                  {FLOW.map((line) => (
                    <li key={line}>{line}</li>
                  ))}
                </ol>
              </article>

              <article className="card">
                <h3>Where Data Is Stored</h3>
                <p className="path">{recordingsRoot || "Loading..."}</p>
                <pre>{`session-<unix-ms>/
  manifest.json
  monitor-<id>/
    frame-000001.png
    frame-000002.png
    ...`}</pre>
              </article>
            </>
          ) : null}
        </section>

        <aside className="card side">
          <h3>Model Activity</h3>
          <div className="model-activity" ref={modelActivityRef}>
            {modelActivity.length === 0 ? (
              <div className="model-activity-item" style={{ opacity: 0.5 }}>
                <span className="activity-text">No model activity yet. Run the Agent Loop to see live updates.</span>
              </div>
            ) : (
              modelActivity.map((a, i) => (
                <div key={i} className="model-activity-item">
                  <span className={`phase-badge ${a.phase}`}>{a.phase}</span>
                  <span className="activity-text">
                    {a.step > 0 && <strong>[{a.step}/{a.max_steps}] </strong>}
                    {a.message}
                  </span>
                </div>
              ))
            )}
          </div>
        </aside>

        <aside className="card side">
          <h3>Activity Log</h3>
          <ul className="log">
            {log.length === 0 ? (
              <li>No events yet</li>
            ) : (
              log.map((line, i) => <li key={`${i}-${line}`}>{line}</li>)
            )}
          </ul>
        </aside>
      </div>
    </main>
  );
}

export default function App() {
  const params = new URLSearchParams(window.location.search);
  const isOverlay = params.has(OVERLAY_QUERY_KEY);
  const isHud = params.has(HUD_QUERY_KEY);
  if (isOverlay) return <OverlayWindow />;
  if (isHud) return <HudWindow />;
  return <MainApp />;
}
