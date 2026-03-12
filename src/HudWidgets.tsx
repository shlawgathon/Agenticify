import { useState, useEffect, useRef } from "react";

// ── Types ──────────────────────────────────────────────

export type AgentStep = {
  phase: "capture" | "thinking" | "click" | "hotkey" | "type" | "shell" | "done" | "error";
  step: number;
  max_steps: number;
  message: string;
  cost_usd?: number;
  token_total?: number;
};

export type TimestampedStep = AgentStep & {
  /** epoch ms when the step was recorded */
  ts: number;
};

// ── Helpers ────────────────────────────────────────────

function pad2(n: number) {
  return n.toString().padStart(2, "0");
}

function formatTime(date: Date): string {
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

function formatElapsed(ms: number): string {
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}:${pad2(m)}:${pad2(s)}`;
  return `${m}:${pad2(s)}`;
}

// ── Elapsed Timer ──────────────────────────────────────
// Shows a running clock like 0:05, 1:23, etc.
// Starts counting from when `active` becomes true.

export function ElapsedTimer({
  active,
  label,
}: {
  active: boolean;
  label?: string;
}) {
  const startRef = useRef<number | null>(null);
  const [elapsed, setElapsed] = useState(0);

  useEffect(() => {
    if (active) {
      startRef.current = Date.now();
      setElapsed(0);
      const id = window.setInterval(() => {
        if (startRef.current) {
          setElapsed(Date.now() - startRef.current);
        }
      }, 250);
      return () => window.clearInterval(id);
    } else {
      startRef.current = null;
    }
  }, [active]);

  if (!active) return null;

  return (
    <span className="hud-elapsed">
      {label && <span className="hud-elapsed-label">{label}</span>}
      <span className="hud-elapsed-time">{formatElapsed(elapsed)}</span>
    </span>
  );
}

// ── Timestamped Activity Feed ──────────────────────────
// Use `stampStep` to add timestamps, then `ActivityFeed` to render.

export function stampStep(step: AgentStep): TimestampedStep {
  return { ...step, ts: Date.now() };
}

export function ActivityFeed({
  items,
  endRef,
}: {
  items: TimestampedStep[];
  endRef?: React.RefObject<HTMLDivElement | null>;
}) {
  return (
    <>
      {items.length === 0 ? (
        <div className="hud-activity-item" style={{ opacity: 0.5 }}>
          No activity yet
        </div>
      ) : (
        items.map((a, i) => (
          <div key={i} className="hud-activity-item">
            <span className="activity-ts">{formatTime(new Date(a.ts))}</span>
            <span className={`phase-tag ${a.phase}`}>{a.phase}</span>
            <span style={{ display: "inline-flex", alignItems: "baseline", gap: 4, flexWrap: "wrap" }}>
              {a.step > 0 && <span style={{ opacity: 0.5, fontSize: "0.48rem" }}>[{a.step}/{a.max_steps}]</span>}
              <ActionPreview phase={a.phase} message={a.message} />
            </span>
          </div>
        ))
      )}
      {endRef && <div ref={endRef} />}
    </>
  );
}

// ── ActionPreview — Rich formatting per action type ────

function ActionPreview({
  phase,
  message,
}: {
  phase: AgentStep["phase"];
  message: string;
}) {
  // Shell → mini code block
  if (phase === "shell") {
    return (
      <span style={{ display: "inline-flex", flexDirection: "column", gap: 2 }}>
        <code style={{
          display: "inline-block",
          fontSize: "0.5rem",
          background: "rgba(255,160,50,0.1)",
          border: "1px solid rgba(255,160,50,0.2)",
          borderRadius: 4,
          padding: "1px 5px",
          color: "#ffcc80",
          fontFamily: "monospace",
          maxWidth: 280,
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}>
          $ {message}
        </code>
      </span>
    );
  }

  // Hotkey → kbd badges
  if (phase === "hotkey") {
    const keys = message.split("+").map((k) => k.trim()).filter(Boolean);
    return (
      <span style={{ display: "inline-flex", alignItems: "center", gap: 2 }}>
        {keys.map((key, j) => (
          <span key={j}>
            <kbd style={{
              display: "inline-block",
              fontSize: "0.48rem",
              fontWeight: 700,
              background: "rgba(0,200,200,0.1)",
              border: "1px solid rgba(0,200,200,0.25)",
              borderRadius: 3,
              padding: "0 4px",
              color: "#80f0f0",
              fontFamily: "monospace",
              lineHeight: "1.6",
            }}>
              {key}
            </kbd>
            {j < keys.length - 1 && <span style={{ opacity: 0.3, margin: "0 1px" }}>+</span>}
          </span>
        ))}
      </span>
    );
  }

  // Click → coordinates badge
  if (phase === "click") {
    const coordMatch = message.match(/\((\d+),\s*(\d+)\)/);
    if (coordMatch) {
      return (
        <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
          <span style={{
            fontSize: "0.48rem",
            background: "rgba(90,176,255,0.12)",
            border: "1px solid rgba(90,176,255,0.2)",
            borderRadius: 4,
            padding: "0 4px",
            color: "#8ed6ff",
            fontFamily: "monospace",
          }}>
            ({coordMatch[1]}, {coordMatch[2]})
          </span>
          <span>{message.replace(coordMatch[0], "").trim()}</span>
        </span>
      );
    }
  }

  // Type → quoted text
  if (phase === "type") {
    return (
      <span style={{
        fontStyle: "italic",
        color: "#d8b4fe",
      }}>
        "{message}"
      </span>
    );
  }

  // Default → plain text
  return <span style={{ whiteSpace: "pre-wrap" }}>{message}</span>;
}
