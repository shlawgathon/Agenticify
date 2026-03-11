// ── MainApp — Dashboard Orchestrator ───────────────────

import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, emitTo, listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { AgentStep } from "../HudWidgets";
import type {
  AgentCursorEvent,
  CaptureFrame,
  EnvStatus,
  HudActionError,
  HudUpdate,
  MistralAuthStatus,
  PermissionState,
  RuntimeState,
  SavedRun,
  SessionManifest,
  SessionStatus,
  Tab,
  VisionAction,
} from "../types";
import {
  DEFAULT_HUD_MODEL,
  HUD_LABEL,
  MODEL_OPTIONS,
  OVERLAY_LABEL,
} from "../constants";
import {
  ensureHudWindow,
  ensureOverlayWindow,
  enforceOverlayPassThrough,
  formatDuration,
  getWindowContext,
} from "../lib/tauri";
import {
  executeInferredAction,
  formatVisionCost,
  formatVisionUsage,
  replayRun,
  runAgentLoop,
  summarizeRunCost,
} from "../lib/agentRunner";
import { RunTab } from "./RunTab";
import { SavedRunsTab } from "./SavedRunsTab";
import { ShortcutsTab } from "./ShortcutsTab";
import { MemoryTab } from "./MemoryTab";
import { DevTab } from "./DevTab";
import { ModelActivityPanel, type ModelActivityPanelHandle } from "./ModelActivityPanel";
import { ActivityLogPanel } from "./ActivityLogPanel";

export function MainApp() {
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
  const [instruction, setInstruction] = useState("Open Google Chrome, search for 'weather in San Francisco', and take a screenshot of the results");
  const [taskContext, setTaskContext] = useState(
    "Goal: complete the task using all available actions — clicking, typing text, keyboard shortcuts (hotkeys), and shell commands.\nAfter using Cmd+Space to open Spotlight, always TYPE the app name, then press Return.\nReturn action=none only when the goal is fully achieved.",
  );
  const [model, setModel] = useState(() => {
    const stored = localStorage.getItem("computer-use-default-model");
    const validIds = MODEL_OPTIONS.map((m) => m.id) as readonly string[];
    if (!stored || !validIds.includes(stored)) {
      localStorage.setItem("computer-use-default-model", DEFAULT_HUD_MODEL);
      return DEFAULT_HUD_MODEL;
    }
    return stored;
  });
  const [maxSteps, setMaxSteps] = useState(30);

  const updateModel = (m: string) => {
    setModel(m);
    localStorage.setItem("computer-use-default-model", m);
  };

  const [recordingStatus, setRecordingStatus] = useState<SessionStatus | null>(null);
  const [recordingSummary, setRecordingSummary] = useState<SessionManifest | null>(null);
  const [sessions, setSessions] = useState<SessionManifest[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const [recordingsRoot, setRecordingsRoot] = useState("");

  // ── Saved Runs State ──────────────────────────────
  const [savedRuns, setSavedRuns] = useState<SavedRun[]>([]);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  const lastRunStepsRef = useRef<VisionAction[]>([]);
  const lastRunInstructionRef = useRef("");
  const lastRunScreenshotsRef = useRef<string[]>([]);
  const [hasUnsavedRun, setHasUnsavedRun] = useState(false);

  const [log, setLog] = useState<string[]>([]);
  const [modelActivity, setModelActivity] = useState<AgentStep[]>([]);
  const modelActivityRef = useRef<HTMLDivElement>(null);
  const modelActivityPanelRef = useRef<ModelActivityPanelHandle>(null);

  const pushModelActivity = (step: AgentStep) => {
    setModelActivity((a) => [...a.slice(-40), step]);
    setTimeout(() => modelActivityRef.current?.scrollTo({ top: modelActivityRef.current.scrollHeight, behavior: "smooth" }), 50);
  };

  const focusModelActivity = () => {
    setTab("run");
    setTimeout(() => modelActivityPanelRef.current?.focusPanel(), 50);
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
  const replayStopRef = useRef(false);
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

  const effectiveInstruction = useMemo(() => {
    const base = instruction.trim();
    const ctx = taskContext.trim();
    return ctx ? `${base}\n\nTask Context:\n${ctx}` : base;
  }, [instruction, taskContext]);

  const health = useMemo(() => {
    const keyReady = apiAuth ? apiAuth.ok : Boolean(envStatus?.mistral_api_key_loaded);
    return {
      permsReady: Boolean(permissions?.screen_recording && permissions?.accessibility),
      keyReady,
      estopOn: Boolean(runtime?.estop),
    };
  }, [permissions, envStatus, apiAuth, runtime]);

  const keyHealthLabel = useMemo(() => {
    if (!envStatus?.mistral_api_key_loaded) return "Missing";
    if (!apiAuth) return "Loaded";
    return apiAuth.ok ? "Valid" : "Invalid";
  }, [envStatus?.mistral_api_key_loaded, apiAuth]);

  // ── Effects ──────────────────────────────────────

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", darkMode ? "dark" : "light");
  }, [darkMode]);

  useEffect(() => { void bootstrap(); }, []);

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
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, []);

  useEffect(() => {
    let unlistenOverlay: (() => void) | undefined;
    let unlistenHudError: (() => void) | undefined;

    void listen<{ enabled: boolean }>("hud_overlay_state_changed", ({ payload }) => {
      setOverlayEnabled(payload.enabled);
      pushLog(`overlay ${payload.enabled ? "enabled" : "hidden"} via HUD`);
      if (payload.enabled) void enforceOverlayPassThrough();
    }).then((fn) => { unlistenOverlay = fn; });

    void listen<HudActionError>("hud_action_error", ({ payload }) => {
      pushLog(`HUD ${payload.action} error: ${payload.message}`);
    }).then((fn) => { unlistenHudError = fn; });

    return () => {
      if (unlistenOverlay) unlistenOverlay();
      if (unlistenHudError) unlistenHudError();
    };
  }, []);

  useEffect(() => {
    if (!overlayEnabled) return;
    void enforceOverlayPassThrough();
    const id = window.setInterval(() => { void enforceOverlayPassThrough(); }, 1200);
    return () => window.clearInterval(id);
  }, [overlayEnabled]);

  const publishHudUpdate = async () => {
    await emitTo(HUD_LABEL, "hud_update", {
      estop: Boolean(runtime?.estop),
      overlay: overlayEnabled,
      keyLoaded: Boolean(envStatus?.mistral_api_key_loaded),
      permsReady: Boolean(permissions?.screen_recording && permissions?.accessibility),
      instruction: effectiveInstruction,
    } satisfies HudUpdate).catch(() => undefined);
  };

  useEffect(() => { void publishHudUpdate(); }, [
    runtime?.estop, overlayEnabled, envStatus?.mistral_api_key_loaded,
    permissions?.screen_recording, permissions?.accessibility, effectiveInstruction,
  ]);

  useEffect(() => {
    if (!hudEnabled) return;
    const id = window.setInterval(() => { void publishHudUpdate(); }, 1000);
    return () => window.clearInterval(id);
  }, [hudEnabled, runtime?.estop, overlayEnabled, envStatus?.mistral_api_key_loaded,
    permissions?.screen_recording, permissions?.accessibility, effectiveInstruction]);

  // ── Bootstrap & Refresh Functions ─────────────────

  const bootstrap = async () => {
    await Promise.all([
      refreshPermissions(true),
      refreshEnv(true),
      refreshRuntime(true),
      refreshRecordingStatus(true),
      loadRecordingsRoot(true),
      loadSavedRuns(true),
    ]);
    if (overlayEnabled) {
      const ok = await ensureOverlayWindow()
        .then(() => true)
        .catch((e) => { pushLog(`overlay init error: ${String(e)}`); return false; });
      if (!ok) setOverlayEnabled(false);
    }
    if (hudEnabled) {
      const hud = await ensureHudWindow().catch((e) => {
        pushLog(`hud init error: ${String(e)}`); return null;
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
    if (!silent) pushLog(`permissions -> screen=${res.screen_recording} accessibility=${res.accessibility}`);
  };

  const requestPermissions = async () => {
    const res = await invoke<PermissionState>("request_permissions_cmd");
    setPermissions(res);
    pushLog(`request permissions -> screen=${res.screen_recording} accessibility=${res.accessibility}`);
  };

  const refreshEnv = async (silent = false) => {
    const res = await invoke<EnvStatus>("env_status_cmd");
    setEnvStatus(res);
    if (!res.mistral_api_key_loaded) setApiAuth(null);
    if (!silent) pushLog(`env -> MISTRAL_API_KEY ${res.mistral_api_key_loaded ? "loaded" : "missing"}`);
  };

  const validateApiKey = async () => {
    try {
      const status = await invoke<MistralAuthStatus>("validate_mistral_api_key_cmd");
      setApiAuth(status);
      pushLog(status.ok
        ? `api key valid (${status.http_status ?? "ok"}) @ ${status.mistral_api_base}`
        : `api key invalid: ${status.message}`);
    } catch (err) { pushLog(`api key validation error: ${String(err)}`); }
  };

  const refreshRuntime = async (silent = false) => {
    const res = await invoke<RuntimeState>("get_runtime_state_cmd");
    setRuntime(res);
    if (!silent) pushLog(`runtime -> estop=${res.estop} actions=${res.actions}/${res.max_actions}`);
  };

  const setEstop = async (enabled: boolean) => {
    const res = await invoke<RuntimeState>("set_estop_cmd", { enabled });
    setRuntime(res);
    pushLog(`E-STOP ${enabled ? "enabled" : "cleared"}`);
  };

  const refreshRecordingStatus = async (silent = false) => {
    try {
      const status = await invoke<SessionStatus>("session_status_cmd");
      setRecordingStatus(status);
      if (!silent) pushLog(`recording -> active=${status.active} ticks=${status.frame_ticks}`);
    } catch (err) { if (!silent) pushLog(`recording status error: ${String(err)}`); }
  };

  const startRecording = async () => {
    setBusy((b) => ({ ...b, recordStart: true }));
    try {
      const status = await invoke<SessionStatus>("start_session_cmd", {
        req: {
          instruction: instruction.trim() || undefined,
          task_context: taskContext.trim() || undefined,
          model,
          fps: 2,
        },
      });
      setRecordingStatus(status);
      setRecordingSummary(null);
      pushLog(`recording started -> ${status.name ?? status.session_id ?? "unknown"}`);
    } catch (err) { pushLog(`recording start error: ${String(err)}`); }
    finally { setBusy((b) => ({ ...b, recordStart: false })); }
  };

  const stopRecording = async () => {
    setBusy((b) => ({ ...b, recordStop: true }));
    try {
      const summary = await invoke<SessionManifest>("stop_session_cmd");
      setRecordingSummary(summary);
      pushLog(`recording stopped -> ${summary.frame_ticks} ticks in ${formatDuration(summary.duration_ms)}`);
      await refreshRecordingStatus(true);
      await loadSessions(true);
      setSelectedSessionId(summary.session_id);
    } catch (err) { pushLog(`recording stop error: ${String(err)}`); }
    finally { setBusy((b) => ({ ...b, recordStop: false })); }
  };

  const loadRecordingsRoot = async (silent = false) => {
    try {
      const root = await invoke<string>("recordings_root_cmd");
      setRecordingsRoot(root);
      if (!silent) pushLog(`recordings root -> ${root}`);
    } catch (err) { if (!silent) pushLog(`recordings root error: ${String(err)}`); }
  };

  const loadSessions = async (silent = false) => {
    try {
      const res = await invoke<SessionManifest[]>("list_sessions_cmd");
      setSessions(res);
      if (res.length > 0 && !selectedSessionId) setSelectedSessionId(res[0].session_id);
      if (!silent) pushLog(`sessions loaded -> ${res.length} session(s)`);
    } catch (err) { if (!silent) pushLog(`sessions load error: ${String(err)}`); }
  };

  const openPath = async (path: string) => {
    if (!path) return;
    await invoke("open_path_cmd", { path }).catch((e) => pushLog(`open path error: ${String(e)}`));
  };

  // ── Action Functions ────────────────────────────

  const capturePrimary = async () => {
    setBusy((b) => ({ ...b, capture: true }));
    try {
      const res = await invoke<CaptureFrame>("capture_primary_cmd");
      setCapture(res);
      pushLog(`capture -> ${res.screenshot_w_px}x${res.screenshot_h_px} (${res.capture_ms}ms)`);
    } catch (err) { pushLog(`capture error: ${String(err)}`); }
    finally { setBusy((b) => ({ ...b, capture: false })); }
  };

  const inferClick = async () => {
    if (!capture) { pushLog("infer blocked: capture is missing"); return; }
    setBusy((b) => ({ ...b, infer: true }));
    try {
      pushLog(`infer request -> ${capture.png_path}`);
      const res = await invoke<VisionAction>("infer_click_cmd", {
        req: { png_path: capture.png_path, instruction: effectiveInstruction, model },
      });
      setVision(res);
      const cost = formatVisionCost(res);
      pushLog(
        `infer -> ${res.action} conf=${res.confidence.toFixed(2)} (${res.model_ms}ms, ${formatVisionUsage(res)}, ${res.model}${cost ? `, ${cost}` : ""})`,
      );
    } catch (err) {
      pushLog(`infer error: ${String(err)}`);
      maybeLogRateLimitHint(err, "infer");
    } finally { setBusy((b) => ({ ...b, infer: false })); }
  };

  const executeClick = async () => {
    if (!capture || !vision) { pushLog("action blocked: no vision result ready"); return; }
    setBusy((b) => ({ ...b, click: true }));
    try {
      await executeInferredAction(capture, vision);
      pushLog(`action executed: ${vision.action}`);
      await refreshRuntime(true);
    } catch (err) { pushLog(`click error: ${String(err)}`); }
    finally { setBusy((b) => ({ ...b, click: false })); }
  };

  const runLiveOnce = async () => {
    setBusy((b) => ({ ...b, capture: true, infer: true, click: true }));
    try {
      const captured = await invoke<CaptureFrame>("capture_primary_cmd");
      setCapture(captured);
      pushLog(`one-shot capture -> ${captured.screenshot_w_px}x${captured.screenshot_h_px}`);

      const inferred = await invoke<VisionAction>("infer_click_cmd", {
        req: { png_path: captured.png_path, instruction: effectiveInstruction, model },
      });
      setVision(inferred);
      const cost = formatVisionCost(inferred);
      pushLog(
        `one-shot infer -> ${inferred.action} conf=${inferred.confidence.toFixed(2)} (${formatVisionUsage(inferred)}, ${inferred.model}${cost ? `, ${cost}` : ""})`,
      );

      if (inferred.action !== "none") {
        await executeInferredAction(captured, inferred);
        pushLog(`one-shot action executed: ${inferred.action}`);
      } else {
        pushLog("one-shot stopped: model returned no actionable result");
      }
      if (cost) {
        pushLog(`one-shot total cost ${cost}`);
      }
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`one-shot error: ${String(err)}`);
      maybeLogRateLimitHint(err, "one-shot");
    } finally {
      setBusy((b) => ({ ...b, capture: false, infer: false, click: false }));
    }
  };

  const runLoop = async () => {
    loopStopRef.current = false;
    focusModelActivity();
    setLooping(true);
    setBusy((b) => ({ ...b, capture: true, infer: true, click: true }));
    try {
      const steps = await runAgentLoop({
        instruction: effectiveInstruction,
        model,
        maxSteps,
        shouldStop: () => loopStopRef.current,
        onStep: (step) => {
          pushModelActivity(step);
          if (step.phase === "error") pushLog(`agent loop error: ${step.message}`);
        },
        onCapture: (c) => {
          setCapture(c);
          lastRunScreenshotsRef.current.push(c.png_path);
        },
        onVision: (v) => {
          setVision(v);
          // Collect actions for Save Run
          if (v.action !== "none") lastRunStepsRef.current.push(v);
        },
      });
      lastRunInstructionRef.current = effectiveInstruction;
      setHasUnsavedRun(lastRunStepsRef.current.length > 0);
      const totalCost = summarizeRunCost(steps);
      pushLog(totalCost ? `agent loop complete (${totalCost})` : "agent loop complete");
      pushLog('Click "Save Run" in the Run tab to save this run for replay.');
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`agent loop error: ${String(err)}`);
      maybeLogRateLimitHint(err, "agent-loop");
    } finally {
      setLooping(false);
      setBusy((b) => ({ ...b, capture: false, infer: false, click: false }));
    }
  };

  const replaySelectedSession = async () => {
    if (!selectedSessionId) { pushLog("replay blocked: select a session first"); return; }
    replayStopRef.current = false;
    focusModelActivity();
    setBusy((b) => ({ ...b, replay: true }));
    try {
      pushLog(`session replay started → ${selectedSessionId}`);
      const steps = await runAgentLoop({
        instruction: effectiveInstruction,
        model,
        shouldStop: () => replayStopRef.current,
        onStep: (step) => {
          pushModelActivity(step);
          void emit("agent_step", step).catch(() => undefined);
        },
        onCapture: (c) => setCapture(c),
        onVision: (v) => setVision(v),
      });

      // Persist activity log
      if (selectedSessionId) {
        invoke("save_activity_log_cmd", {
          sessionId: selectedSessionId,
          activityLog: steps,
        }).catch(() => { /* best-effort */ });
      }
      const totalCost = summarizeRunCost(steps);
      pushLog(totalCost ? `session replay complete (${totalCost})` : "session replay complete");
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`session replay error: ${String(err)}`);
      pushModelActivity({ phase: "error", step: 0, max_steps: 30, message: String(err) });
      if (String(err).includes("401")) {
        pushLog('Provider auth failed: click "Validate API Key" and check OPENROUTER_API_KEY / MISTRAL_API_KEY in .env');
      }
      maybeLogRateLimitHint(err, "session replay");
    } finally {
      setBusy((b) => ({ ...b, replay: false }));
    }
  };

  // ── Saved Runs Functions ─────────────────────────

  const loadSavedRuns = async (silent = false) => {
    try {
      const runs = await invoke<SavedRun[]>("list_saved_runs_cmd");
      setSavedRuns(runs);
      if (!silent) pushLog(`saved runs loaded → ${runs.length} run(s)`);
    } catch (err) { if (!silent) pushLog(`saved runs load error: ${String(err)}`); }
  };

  const saveLastRun = async () => {
    const steps = lastRunStepsRef.current;
    if (steps.length === 0) { pushLog("nothing to save: run the agent loop first"); return; }
    try {
      const totalCost = steps.reduce((sum, v) => sum + (v.usage.estimated_cost_usd ?? 0), 0);
      const savedRunSteps = steps.map((v) => ({
        action: v.action,
        x_norm: v.x_norm,
        y_norm: v.y_norm,
        confidence: v.confidence,
        reason: v.reason,
        sent_w: v.sent_w,
        sent_h: v.sent_h,
        keys: v.keys ?? null,
        text: v.text ?? null,
        command: v.command ?? null,
        tool_name: v.tool_name ?? null,
        shortcut: v.shortcut ?? null,
      }));
      const run = await invoke<SavedRun>("save_run_cmd", {
        req: {
          instruction: lastRunInstructionRef.current || instruction,
          task_context: taskContext || null,
          model,
          steps: savedRunSteps,
          total_cost_usd: totalCost,
        },
      });
      pushLog(`saved run → ${run.run_id} (${run.total_steps} steps)`);
      lastRunStepsRef.current = [];
      setHasUnsavedRun(false);
      await loadSavedRuns(true);
      setSelectedRunId(run.run_id);
    } catch (err) { pushLog(`save run error: ${String(err)}`); }
  };

  const replaySavedRun = async () => {
    if (!selectedRunId) return;
    const run = savedRuns.find((r) => r.run_id === selectedRunId);
    if (!run) { pushLog("replay blocked: run not found"); return; }
    replayStopRef.current = false;
    focusModelActivity();
    setBusy((b) => ({ ...b, replay: true }));
    try {
      pushLog(`replay started → ${run.name} (${run.total_steps} steps)`);
      await replayRun({
        steps: run.steps,
        shouldStop: () => replayStopRef.current,
        onStep: (step) => {
          pushModelActivity(step);
          void emit("agent_step", step).catch(() => undefined);
        },
        onCapture: setCapture,
      });
      pushLog("replay complete");
      await refreshRuntime(true);
    } catch (err) {
      pushLog(`replay error: ${String(err)}`);
      pushModelActivity({ phase: "error", step: 0, max_steps: 30, message: String(err) });
    } finally {
      setBusy((b) => ({ ...b, replay: false }));
    }
  };

  const deleteSelectedRun = async () => {
    if (!selectedRunId) return;
    try {
      await invoke("delete_saved_run_cmd", { runId: selectedRunId });
      pushLog(`deleted run → ${selectedRunId}`);
      setSelectedRunId(null);
      await loadSavedRuns(true);
    } catch (err) { pushLog(`delete run error: ${String(err)}`); }
  };

  // ── Render ──────────────────────────────────────

  return (
    <div className="dashboard">
      {/* ── Sidebar ─── */}
      <aside className="dash-sidebar">
        <div className="dash-sidebar-header">
          <div className="dash-logo">
            <span className="dash-logo-icon">⌘</span>
            <div className="dash-logo-text">
              <span className="dash-logo-name">Computer Use</span>
              <span className="dash-logo-sub">v0.1.0</span>
            </div>
          </div>
        </div>

        <nav className="dash-nav">
          <button className={`dash-nav-item ${tab === "run" ? "active" : ""}`} onClick={() => setTab("run")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><polygon points="5,3 19,12 5,21" /></svg>
            Run
          </button>
          <button className={`dash-nav-item ${tab === "activity" ? "active" : ""}`} onClick={() => setTab("activity")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><polyline points="22,12 18,12 15,21 9,3 6,12 2,12" /></svg>
            Activity
          </button>
          <button className={`dash-nav-item ${tab === "saved-runs" ? "active" : ""}`} onClick={() => setTab("saved-runs")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2z" /><path d="M17 21v-8H7v8" /><path d="M7 3v5h8" /></svg>
            Saved Runs
          </button>
          <button className={`dash-nav-item ${tab === "shortcuts" ? "active" : ""}`} onClick={() => setTab("shortcuts")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><rect x="2" y="4" width="20" height="16" rx="2" /><line x1="6" y1="8" x2="6" y2="8" /><line x1="10" y1="8" x2="10" y2="8" /><line x1="14" y1="8" x2="14" y2="8" /><line x1="18" y1="8" x2="18" y2="8" /><line x1="8" y1="16" x2="16" y2="16" /></svg>
            Shortcuts
          </button>
          <button className={`dash-nav-item ${tab === "memory" ? "active" : ""}`} onClick={() => setTab("memory")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><path d="M12 2a7 7 0 0 1 7 7c0 2.38-1.19 4.47-3 5.74V17a2 2 0 0 1-2 2H10a2 2 0 0 1-2-2v-2.26C6.19 13.47 5 11.38 5 9a7 7 0 0 1 7-7z" /><line x1="10" y1="22" x2="14" y2="22" /></svg>
            Memory
          </button>
          <button className={`dash-nav-item ${tab === "dev" ? "active" : ""}`} onClick={() => setTab("dev")}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
            Settings
          </button>
        </nav>

        <div className="dash-sidebar-footer">
          <select
            value={model}
            onChange={(e) => updateModel(e.target.value)}
            className="dash-model-select"
          >
            {MODEL_OPTIONS.map((m) => (
              <option key={m.id} value={m.id}>{m.label}</option>
            ))}
          </select>
          <button className="dash-nav-item" onClick={() => setDarkMode((v) => !v)}>
            <svg viewBox="0 0 24 24" className="dash-nav-icon">
              {darkMode
                ? <><circle cx="12" cy="12" r="5" /><path d="M12 1v2" /><path d="M12 21v2" /><path d="M4.22 4.22l1.42 1.42" /><path d="M18.36 18.36l1.42 1.42" /><path d="M1 12h2" /><path d="M21 12h2" /><path d="M4.22 19.78l1.42-1.42" /><path d="M18.36 5.64l1.42-1.42" /></>
                : <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
              }
            </svg>
            {darkMode ? "Light" : "Dark"}
          </button>
        </div>
      </aside>

      {/* ── Content ─── */}
      <main className="dash-content">
        <div className="dash-content-inner">
          {tab === "run" && (
            <>
              <RunTab
                permsReady={health.permsReady}
                keyReady={health.keyReady}
                keyLabel={keyHealthLabel}
                estopOn={health.estopOn}
                overlayEnabled={overlayEnabled}
                hudEnabled={hudEnabled}
                modelActivity={modelActivity}
                runtime={runtime}
                instruction={instruction}
                setInstruction={setInstruction}
                taskContext={taskContext}
                setTaskContext={setTaskContext}
                model={model}
                updateModel={updateModel}
                refreshPermissions={() => void refreshPermissions()}
                requestPermissions={() => void requestPermissions()}
                validateApiKey={() => void validateApiKey()}
                refreshRuntime={() => void refreshRuntime()}
                runLiveOnce={() => void runLiveOnce()}
                runAgentLoop={() => { lastRunStepsRef.current = []; lastRunScreenshotsRef.current = []; setHasUnsavedRun(false); void runLoop(); }}
                stopLoop={() => { loopStopRef.current = true; }}
                setEstop={(v) => void setEstop(v)}
                looping={looping}
                busy={busy}
              />
              {hasUnsavedRun && !looping && (
                <button onClick={() => void saveLastRun()} style={{ alignSelf: "flex-start" }}>
                  💾 Save Run ({lastRunStepsRef.current.length} steps)
                </button>
              )}
            </>
          )}

          {tab === "activity" && (
            <>
              {hasUnsavedRun && !looping && (
                <div className="row" style={{ marginBottom: 8 }}>
                  <button onClick={() => void saveLastRun()}>
                    💾 Save Run ({lastRunStepsRef.current.length} steps)
                  </button>
                  {lastRunScreenshotsRef.current.length > 0 && (
                    <button onClick={async () => {
                      try {
                        const dir = await invoke<string>("save_screenshots_cmd", { pngPaths: lastRunScreenshotsRef.current });
                        pushLog(`Screenshots saved to ${dir}`);
                      } catch (err) {
                        pushLog(`Screenshot save failed: ${err}`);
                      }
                    }}>
                      📸 Save Screenshots ({lastRunScreenshotsRef.current.length})
                    </button>
                  )}
                </div>
              )}
              <ModelActivityPanel ref={modelActivityPanelRef} activity={modelActivity} pushLog={pushLog} />
              <ActivityLogPanel log={log} />
            </>
          )}

          {tab === "saved-runs" && (
            <SavedRunsTab
              savedRuns={savedRuns}
              selectedRunId={selectedRunId}
              setSelectedRunId={setSelectedRunId}
              loadSavedRuns={() => void loadSavedRuns()}
              replaySelectedRun={() => void replaySavedRun()}
              stopReplay={() => { replayStopRef.current = true; }}
              deleteSelectedRun={() => void deleteSelectedRun()}
              busy={busy}
            />
          )}

          {tab === "shortcuts" && <ShortcutsTab />}

          {tab === "memory" && <MemoryTab />}

          {tab === "dev" && (
            <DevTab
              permissions={permissions}
              envStatus={envStatus}
              runtime={runtime}
              recordingsRoot={recordingsRoot}
              maxSteps={maxSteps}
              setMaxSteps={setMaxSteps}
              refreshPermissions={() => void refreshPermissions()}
              requestPermissions={() => void requestPermissions()}
              validateApiKey={() => void validateApiKey()}
              refreshRuntime={() => void refreshRuntime()}
              setEstop={(v) => void setEstop(v)}
              openPath={(p) => void openPath(p)}
            />
          )}
        </div>
      </main>
    </div>
  );
}

