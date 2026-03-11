// ── HudWindow — Floating Chat Bar (Pluely-inspired) ──────

import { useEffect, useLayoutEffect, useRef, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import gsap from "gsap";
import { ElapsedTimer, ActivityFeed, stampStep, type TimestampedStep, type AgentStep } from "../HudWidgets";
import type {
  EnvStatus,
  HudUpdate,
  PermissionState,
} from "../types";
import {
  DEFAULT_HUD_MODEL,
  HUD_HEIGHT,
  HUD_WIDTH,
  MAIN_LABEL,
  MODEL_OPTIONS,
} from "../constants";
import {
  revealMainWindow,
} from "../lib/tauri";
import { runAgentLoop, summarizeRunCost } from "../lib/agentRunner";
import type { VisionAction, SavedRun } from "../types";

// ── Constants ─────────────────────────────────────
const COLLAPSED_SIZE = 30;
const BAR_H = 60;
const EXPANDED_H = 248;

export function HudWindow() {
  // ── Core state ─────────────────────────────────
  const [status, setStatus] = useState<HudUpdate>({
    estop: false,
    overlay: true,
    keyLoaded: false,
    permsReady: false,
    instruction: "Waiting for command",
  });
  const loopStopRef = useRef(false);
  const hudIsSourceRef = useRef(false);
  const [looping, setLooping] = useState(false);
  const loopingRef = useRef(false);

  // ── Panel state ────────────────────────────────
  const [hudCollapsed, setHudCollapsed] = useState(false);
  const [showActivity, setShowActivity] = useState(false);
  const [activityFeed, setActivityFeed] = useState<TimestampedStep[]>([]);
  const [hudInstruction, setHudInstruction] = useState("");
  const [paused, setPaused] = useState(false);
  const pausedRef = useRef(false);
  const [redirectInput, setRedirectInput] = useState("");
  const instructionRef = useRef("");
  const redirectInputRef = useRef<HTMLInputElement>(null);
  const lastRunStepsRef = useRef<VisionAction[]>([]);
  const [hasUnsavedRun, setHasUnsavedRun] = useState(false);
  const activityEndRef = useRef<HTMLDivElement>(null);
  const [windowBlurred, setWindowBlurred] = useState(false);

  // Detect window focus/blur to compensate for macOS dropping backdrop-filter
  // and to restore correct window dimensions on focus return (e.g. after Spotlight)
  useEffect(() => {
    const onBlur = () => setWindowBlurred(true);
    const onFocus = () => {
      setWindowBlurred(false);
      // Restore correct window size on focus return — macOS may have
      // disrupted the window when Spotlight or another system overlay opened.
      if (loopingRef.current || showActivity) {
        void setWindowSizeCentered(HUD_WIDTH, EXPANDED_H);
      } else if (!hudCollapsed) {
        void setWindowSizeCentered(HUD_WIDTH, BAR_H);
      }
    };
    window.addEventListener("blur", onBlur);
    window.addEventListener("focus", onFocus);
    return () => {
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("focus", onFocus);
    };
  }, [showActivity, hudCollapsed]);

  // ── Model selection ────────────────────────────
  const [hudModel, setHudModel] = useState(() => {
    const stored = localStorage.getItem("computer-use-default-model");
    const validIds = MODEL_OPTIONS.map((m) => m.id) as readonly string[];
    if (!stored || !validIds.includes(stored)) {
      localStorage.setItem("computer-use-default-model", DEFAULT_HUD_MODEL);
      return DEFAULT_HUD_MODEL;
    }
    return stored;
  });

  const updateHudModel = (m: string) => {
    setHudModel(m);
    localStorage.setItem("computer-use-default-model", m);
  };

  // ── Refs for GSAP ──────────────────────────────
  const pillRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // ── Activity push ──────────────────────────────
  const pushActivity = useCallback((step: AgentStep) => {
    setActivityFeed((f) => [...f.slice(-40), stampStep(step)]);
    // Only scroll if window is focused — avoid layout reflows during macOS app deactivation
    if (!document.hidden) {
      setTimeout(() => activityEndRef.current?.scrollIntoView({ behavior: "smooth" }), 50);
    }
  }, []);

  // ── Helper: set window size + center on monitor ─
  const setWindowSizeCentered = async (w: number, h: number) => {
    const win = getCurrentWindow();
    const monitor = await currentMonitor();
    const scale = monitor?.scaleFactor || 1;
    const monX = monitor ? monitor.position.x / scale : 0;
    const monW = monitor ? monitor.size.width / scale : 1400;
    const monY = monitor ? monitor.position.y / scale : 0;
    const x = monX + monW / 2 - w / 2;
    const y = monY + 18;

    const size = new LogicalSize(w, h);
    await win.setMinSize(size).catch(() => undefined);
    await win.setMaxSize(size).catch(() => undefined);
    await win.setSize(size).catch(() => undefined);
    await win.setPosition(new LogicalPosition(x, y)).catch(() => undefined);
  };

  // ── Boot ───────────────────────────────────────
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
      await win.show().catch(() => undefined);
      await win.setAlwaysOnTop(true).catch(() => undefined);
      await win.setDecorations(false).catch(() => undefined);
      await win.setShadow(false).catch(() => undefined);
      await win.setFocusable(true).catch(() => undefined);
      await win.setIgnoreCursorEvents(false).catch(() => undefined);
      await win.setBackgroundColor({ red: 0, green: 0, blue: 0, alpha: 0 }).catch(() => undefined);

      // Set initial size centered
      await setWindowSizeCentered(HUD_WIDTH, BAR_H);

      unlistenStatus = await listen<HudUpdate>("hud_update", ({ payload }) => {
        setStatus(payload);
      });

      try {
        const [perms, env] = await Promise.all([
          invoke<PermissionState>("check_permissions_cmd"),
          invoke<EnvStatus>("env_status_cmd"),
        ]);
        setStatus((prev) => ({
          ...prev,
          permsReady: Boolean(perms.screen_recording && perms.accessibility),
          keyLoaded: Boolean(env.mistral_api_key_loaded),
        }));
      } catch {
        // best-effort
      }

      // Entrance animation
      if (pillRef.current) {
        gsap.fromTo(
          pillRef.current,
          { opacity: 0, y: -20, scale: 0.9 },
          { opacity: 1, y: 0, scale: 1, duration: 0.5, ease: "back.out(1.7)" },
        );
      }
    })();

    return () => {
      if (unlistenStatus) unlistenStatus();
    };
  }, []);

  // ── Listen for agent_step events ──────────────
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<AgentStep>("agent_step", ({ payload }) => {
        if (cancelled) return;
        if (hudIsSourceRef.current) return;
        pushActivity(payload);
        setShowActivity(true);
      });
    })();
    return () => { cancelled = true; if (unlisten) unlisten(); };
  }, [pushActivity]);

  // ── Activity toggle with smooth GSAP ───────────
  const toggleActivity = async () => {
    if (showActivity) {
      // ── CLOSE: animate height + content in sync ──
      const proxy = { h: EXPANDED_H };
      await new Promise<void>((resolve) => {
        const tl = gsap.timeline({ onComplete: resolve });

        // Fade out dropdown content
        if (panelRef.current) {
          tl.to(panelRef.current, {
            opacity: 0, y: -6, duration: 0.15, ease: "power2.in",
          }, 0);
        }

        // Shrink window frame-by-frame via proxy
        tl.to(proxy, {
          h: BAR_H,
          duration: 0.3,
          ease: "power3.inOut",
          onUpdate: () => {
            void setWindowSizeCentered(HUD_WIDTH, Math.round(proxy.h));
          },
        }, 0);

        // Shrink pill visually in sync
        if (pillRef.current) {
          tl.to(pillRef.current, {
            height: BAR_H, duration: 0.3, ease: "power3.inOut",
          }, 0);
        }
      });

      setShowActivity(false);
      if (pillRef.current) pillRef.current.style.height = "";

    } else {
      // ── OPEN: animate height + content in sync ──
      setShowActivity(true);
      // Set pill to bar height so it starts small
      if (pillRef.current) pillRef.current.style.height = `${BAR_H}px`;

      const proxy = { h: BAR_H };
      await new Promise<void>((resolve) => {
        const tl = gsap.timeline({ onComplete: resolve });

        // Grow window frame-by-frame via proxy
        tl.to(proxy, {
          h: EXPANDED_H,
          duration: 0.3,
          ease: "power2.out",
          onUpdate: () => {
            void setWindowSizeCentered(HUD_WIDTH, Math.round(proxy.h));
          },
        }, 0);

        // Grow pill visually in sync
        if (pillRef.current) {
          tl.to(pillRef.current, {
            height: "100%", duration: 0.3, ease: "power2.out",
          }, 0);
        }

        // Fade in dropdown content (slightly delayed)
        if (panelRef.current) {
          tl.fromTo(panelRef.current,
            { opacity: 0, y: -6 },
            { opacity: 1, y: 0, duration: 0.2, ease: "power2.out" },
            0.12,
          );
        }
      });
    }
  };

  // ── Collapse / Expand with GSAP ────────────────
  const collapseHud = async () => {
    const win = getCurrentWindow();
    const monitor = await currentMonitor();
    const scale = monitor?.scaleFactor || 1;
    const monX = monitor ? monitor.position.x / scale : 0;
    const monW = monitor ? monitor.size.width / scale : 1400;
    const monY = monitor ? monitor.position.y / scale : 0;

    if (pillRef.current) {
      await new Promise<void>((resolve) => {
        gsap.to(pillRef.current, { scale: 0.5, opacity: 0, duration: 0.25, ease: "power3.in", onComplete: resolve });
      });
    }

    setHudCollapsed(true);
    setShowActivity(false);

    await win.setFocusable(false).catch(() => undefined);
    const size = new LogicalSize(COLLAPSED_SIZE, COLLAPSED_SIZE);
    await win.setMinSize(size).catch(() => undefined);
    await win.setMaxSize(size).catch(() => undefined);
    await win.setSize(size).catch(() => undefined);
    await win.setPosition(new LogicalPosition(monX + monW / 2 - COLLAPSED_SIZE / 2, monY + 18)).catch(() => undefined);

    requestAnimationFrame(() => {
      if (pillRef.current) {
        gsap.fromTo(pillRef.current, { scale: 0.3, opacity: 0 }, { scale: 1, opacity: 1, duration: 0.35, ease: "elastic.out(1, 0.5)" });
      }
    });
  };

  const expandHud = async () => {
    if (pillRef.current) {
      await new Promise<void>((resolve) => {
        gsap.to(pillRef.current, { scale: 0.3, opacity: 0, duration: 0.2, ease: "power2.in", onComplete: resolve });
      });
    }

    setHudCollapsed(false);
    await setWindowSizeCentered(HUD_WIDTH, BAR_H);
    const win = getCurrentWindow();
    await win.setFocusable(true).catch(() => undefined);
    await win.setIgnoreCursorEvents(false).catch(() => undefined);

    requestAnimationFrame(() => {
      if (pillRef.current) {
        gsap.fromTo(pillRef.current, { scale: 0.8, opacity: 0, y: -10 }, { scale: 1, opacity: 1, y: 0, duration: 0.4, ease: "back.out(1.7)" });
      }
    });
  };

  const toggleCollapse = async () => {
    if (hudCollapsed) await expandHud();
    else await collapseHud();
  };


  // ── Main window toggle ─────────────────────────
  const openMainFromHud = async () => {
    const main = await WebviewWindow.getByLabel(MAIN_LABEL);
    if (!main) {
      await emit("hud_action_error", { action: "open_main", message: "Main window not found" }).catch(() => undefined);
      return;
    }
    const visible = await main.isVisible().catch(() => false);
    if (visible) {
      await main.hide().catch(() => undefined);
    } else {
      await revealMainWindow();
    }
  };

  // ── Agent loop from HUD ────────────────────────
  const runAgentLoopFromHud = async () => {
    if (!hudInstruction.trim()) return;
    loopStopRef.current = false;
    pausedRef.current = false;
    setPaused(false);
    hudIsSourceRef.current = true;
    instructionRef.current = hudInstruction;
    setLooping(true);
    loopingRef.current = true;
    setShowActivity(true);
    setRedirectInput("");

    // Expand to show activity
    await setWindowSizeCentered(HUD_WIDTH, EXPANDED_H);

    lastRunStepsRef.current = [];
    setHasUnsavedRun(false);
    try {
      const steps = await runAgentLoop({
        instruction: hudInstruction,
        model: hudModel,
        shouldStop: () => loopStopRef.current,
        shouldPause: () => pausedRef.current,
        getInstruction: () => instructionRef.current,
        onStep: pushActivity,
        onVision: (v) => {
          if (v.action !== "none") lastRunStepsRef.current.push(v);
        },
      });
      const costSummary = summarizeRunCost(steps);
      if (costSummary) {
        pushActivity({ phase: "done", step: 0, max_steps: 0, message: `Run complete — ${costSummary}` });
      }
      setHasUnsavedRun(lastRunStepsRef.current.length > 0);
    } catch (err) {
      await emit("hud_action_error", { action: "agent_loop", message: String(err) }).catch(() => undefined);
    } finally {
      hudIsSourceRef.current = false;
      setLooping(false);
      loopingRef.current = false;
      setPaused(false);
      pausedRef.current = false;
    }
  };

  const togglePause = () => {
    const next = !pausedRef.current;
    pausedRef.current = next;
    setPaused(next);
  };

  const sendRedirect = () => {
    const msg = redirectInput.trim();
    if (!msg) return;
    // Append redirect context to the instruction
    instructionRef.current = `${instructionRef.current}\n\nUser redirect: ${msg}`;
    pushActivity({ phase: "capture" as AgentStep["phase"], step: 0, max_steps: 0, message: `Redirected: ${msg}` });
    setRedirectInput("");
    // If agent is paused, resume it
    if (pausedRef.current) {
      pausedRef.current = false;
      setPaused(false);
    }
    // If agent loop has finished, restart it with the updated instruction
    if (!loopingRef.current) {
      void runAgentLoopFromHud();
    }
  };

  const saveLastRun = async () => {
    const steps = lastRunStepsRef.current;
    if (steps.length === 0) return;
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
          instruction: instructionRef.current || hudInstruction,
          task_context: null,
          model: hudModel,
          steps: savedRunSteps,
          total_cost_usd: totalCost,
        },
      });
      pushActivity({ phase: "done", step: 0, max_steps: 0, message: `Run saved (${run.total_steps} steps)` });
      lastRunStepsRef.current = [];
      setHasUnsavedRun(false);
    } catch (err) {
      pushActivity({ phase: "error" as AgentStep["phase"], step: 0, max_steps: 0, message: `Save error: ${String(err)}` });
    }
  };

  // ── Export activity ────────────────────────────
  const exportActivity = async () => {
    const md = activityFeed
      .map((a) => {
        const ts = new Date(a.ts).toLocaleTimeString();
        const prefix = a.step > 0 ? `[${a.step}/${a.max_steps}] ` : "";
        return `- **${ts}** [${a.phase.toUpperCase()}] ${prefix}${a.message}`;
      })
      .join("\n");
    const content = `# HUD Activity\n\n${md}\n`;
    try {
      const path = await invoke<string>("export_markdown_cmd", { filename: "hud-activity.md", content });
      await navigator.clipboard.writeText(content).catch(() => undefined);
      pushActivity({ phase: "done", step: 0, max_steps: 0, message: `Exported → ${path}` });
    } catch {
      try {
        await navigator.clipboard.writeText(content);
        pushActivity({ phase: "done", step: 0, max_steps: 0, message: "Copied to clipboard" });
      } catch (err) {
        await emit("hud_action_error", { action: "export_activity", message: String(err) }).catch(() => undefined);
      }
    }
  };

  const copyActivity = () => {
    const text = activityFeed
      .map((a) => {
        const ts = new Date(a.ts).toLocaleTimeString();
        const prefix = a.step > 0 ? `[${a.step}/${a.max_steps}] ` : "";
        return `${ts} [${a.phase.toUpperCase()}] ${prefix}${a.message}`;
      })
      .join("\n");
    void navigator.clipboard.writeText(text).then(() => {
      pushActivity({ phase: "done", step: 0, max_steps: 0, message: "Copied to clipboard" });
    });
  };

  // ── Focus input ────────────────────────────────
  const focusInput = () => {
    const win = getCurrentWindow();
    void win.setFocus().catch(() => undefined);
    setTimeout(() => inputRef.current?.focus(), 50);
  };

  // ── Drag support (grip handle only) ─────────────
  const startDrag = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    void getCurrentWindow().startDragging();
  };

  // ── Reset position (double-click drag area) ────
  const resetPosition = () => {
    if (hudCollapsed) {
      void setWindowSizeCentered(COLLAPSED_SIZE, COLLAPSED_SIZE);
    } else if (showActivity) {
      void setWindowSizeCentered(HUD_WIDTH, EXPANDED_H);
    } else {
      void setWindowSizeCentered(HUD_WIDTH, BAR_H);
    }
  };

  // ── Render ─────────────────────────────────────

  return (
    <main className={`hud-root ${hudCollapsed ? "hud-collapsed" : ""}`}>
      <div
        ref={pillRef}
        className={`hud-pill ${showActivity ? "expanded" : ""} ${hudCollapsed ? "collapsed" : ""} ${windowBlurred ? "hud-blurred" : ""}`}
        title={hudCollapsed ? "Click to expand" : ""}
      >
        {hudCollapsed ? (
          <button
            className="hud-btn hud-btn-collapsed"
            onClick={() => void toggleCollapse()}
            title="Expand HUD"
            type="button"
          >
            <svg viewBox="0 0 24 24" className="hud-icon" aria-hidden="true">
              <path d="M12 5v14" />
              <path d="M5 12h14" />
            </svg>
          </button>
        ) : (
          <>
            {/* ── Inline Chat Bar ─── */}
            <div className="hud-main">
              <div className="hud-controls">
                <button className="hud-btn hud-btn-icon" onClick={() => void openMainFromHud()} title="Dashboard" type="button">
                  <svg viewBox="0 0 24 24" className="hud-icon"><path d="M5 7h14" /><path d="M5 12h14" /><path d="M5 17h14" /></svg>
                </button>
                {looping ? (
                  <button className={`hud-btn hud-btn-icon ${paused ? "active" : ""}`} onClick={togglePause} title={paused ? "Resume" : "Pause"} type="button">
                    {paused ? (
                      <svg viewBox="0 0 24 24" className="hud-icon"><polygon points="8,5 19,12 8,19" fill="currentColor" stroke="none" /></svg>
                    ) : (
                      <svg viewBox="0 0 24 24" className="hud-icon"><rect x="6" y="5" width="4" height="14" fill="currentColor" rx="1" /><rect x="14" y="5" width="4" height="14" fill="currentColor" rx="1" /></svg>
                    )}
                  </button>
                ) : (
                  <button
                    className="hud-btn hud-btn-icon"
                    disabled={!hudInstruction.trim()}
                    onClick={() => void runAgentLoopFromHud()}
                    title="Run agent"
                    type="button"
                  >
                    <svg viewBox="0 0 24 24" className="hud-icon"><polygon points="8,5 19,12 8,19" fill="currentColor" stroke="none" /></svg>
                  </button>
                )}
              </div>

              {looping ? (
                <div className="hud-task-summary" title={hudInstruction}>
                  {paused ? "⏸ " : ""}{hudInstruction.length > 50 ? `${hudInstruction.slice(0, 50)}…` : hudInstruction}
                </div>
              ) : (
                <input
                  ref={inputRef}
                  className="hud-chat-input"
                  placeholder="Type instruction…"
                  value={hudInstruction}
                  onClick={focusInput}
                  onChange={(e) => setHudInstruction(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey && !looping) {
                      e.preventDefault();
                      void runAgentLoopFromHud();
                    }
                  }}
                  autoComplete="off"
                  spellCheck={false}
                />
              )}

              <div className="hud-right">
                {looping && <ElapsedTimer active={looping} />}
                <button className={`hud-btn hud-btn-icon ${showActivity ? "active" : ""}`} onClick={() => void toggleActivity()} title="Activity" type="button">
                  <svg viewBox="0 0 24 24" className="hud-icon"><path d="M6 9l6 6 6-6" /></svg>
                </button>
                <button className="hud-btn hud-btn-icon" onClick={() => void toggleCollapse()} title="Collapse" type="button">
                  <svg viewBox="0 0 24 24" className="hud-icon"><path d="M18 6L6 18" /><path d="M6 6l12 12" /></svg>
                </button>
                {/* ── Drag grip (6 dots) ─── */}
                <div
                  className="hud-grip"
                  data-tauri-drag-region
                  onMouseDown={startDrag}
                  onDoubleClick={resetPosition}
                  title="Drag to move • Double-click to reset"
                >
                  <svg viewBox="0 0 8 14" width="8" height="14">
                    <circle cx="2" cy="2" r="1.1" fill="currentColor" />
                    <circle cx="6" cy="2" r="1.1" fill="currentColor" />
                    <circle cx="2" cy="7" r="1.1" fill="currentColor" />
                    <circle cx="6" cy="7" r="1.1" fill="currentColor" />
                    <circle cx="2" cy="12" r="1.1" fill="currentColor" />
                    <circle cx="6" cy="12" r="1.1" fill="currentColor" />
                  </svg>
                </div>
              </div>
            </div>

            {/* ── Activity Panel ─── */}
            {showActivity && (
              <div className="hud-dropdown" ref={panelRef}>
                <div className="hud-panel-toolbar">
                  <select value={hudModel} onChange={(e) => updateHudModel(e.target.value)} className="hud-model-select">
                    {MODEL_OPTIONS.map((m) => (<option key={m.id} value={m.id}>{m.label}</option>))}
                  </select>
                  <button className="hud-btn hud-btn-sm" onClick={() => void exportActivity()} title="Export .md">Export</button>
                  <button className="hud-btn hud-btn-sm" onClick={() => copyActivity()} title="Copy">Copy</button>
                  {activityFeed.length > 0 && (
                    <button className="hud-btn hud-btn-sm" onClick={() => setActivityFeed([])} title="Clear">Clear</button>
                  )}
                </div>
                <ActivityFeed items={activityFeed} endRef={activityEndRef} />
                {/* ── Redirect bar (pinned to bottom of dropdown) ─── */}
                <div className="hud-redirect-bar">
                  <div className="hud-redirect-input-wrap">
                    <input
                      ref={redirectInputRef}
                      className="hud-redirect-input"
                      placeholder={looping ? "Redirect agent…" : "Type instruction…"}
                      value={redirectInput}
                      onChange={(e) => setRedirectInput(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && !e.shiftKey) {
                          e.preventDefault();
                          sendRedirect();
                        }
                      }}
                      autoComplete="off"
                      spellCheck={false}
                    />
                    <button
                      className="hud-redirect-send"
                      disabled={!redirectInput.trim()}
                      onClick={sendRedirect}
                      title="Send"
                      type="button"
                    >
                      <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                        <line x1="22" y1="2" x2="11" y2="13" />
                        <polygon points="22 2 15 22 11 13 2 9 22 2" />
                      </svg>
                    </button>
                  </div>
                  {looping && (
                    <button
                      className="hud-btn hud-btn-sm hud-btn-stop-sm"
                      onClick={() => { loopStopRef.current = true; }}
                      title="Stop run"
                      type="button"
                    >
                      ■
                    </button>
                  )}
                  {hasUnsavedRun && !looping && (
                    <button
                      className="hud-btn hud-btn-sm"
                      onClick={() => void saveLastRun()}
                      title="Save this run"
                      type="button"
                      style={{ borderColor: "rgba(90, 176, 255, 0.4)", color: "#8ed6ff" }}
                    >
                      Save
                    </button>
                  )}
                </div>
              </div>
            )}
          </>
        )}
      </div>
    </main>
  );
}
