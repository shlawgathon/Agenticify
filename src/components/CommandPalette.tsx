// ── CommandPalette — ⌘K overlay for Dashboard ─────────

import { useEffect, useRef, useState } from "react";
import { COMMANDS, filterCommands, type Command, type CommandContext } from "../lib/commandSystem";

interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  ctx: CommandContext;
}

export function CommandPalette({ open, onClose, ctx }: CommandPaletteProps) {
  const [query, setQuery] = useState("");
  const [selectedIdx, setSelectedIdx] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const filtered = filterCommands(query);

  useEffect(() => {
    if (open) {
      setQuery("");
      setSelectedIdx(0);
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [open]);

  // Clamp selection
  useEffect(() => {
    if (selectedIdx >= filtered.length) setSelectedIdx(Math.max(0, filtered.length - 1));
  }, [filtered.length, selectedIdx]);

  const runCommand = (cmd: Command) => {
    const args = query.replace(/^\/?\s*\S+\s*/, ""); // extract args after command name
    cmd.execute(ctx, args);
    onClose();
  };

  const handleKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelectedIdx((i) => Math.min(i + 1, filtered.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelectedIdx((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (filtered[selectedIdx]) runCommand(filtered[selectedIdx]);
    }
  };

  if (!open) return null;

  return (
    <div className="cmd-palette-backdrop" onClick={onClose}>
      <div className="cmd-palette" onClick={(e) => e.stopPropagation()}>
        <div className="cmd-palette-input-wrap">
          <svg viewBox="0 0 24 24" className="cmd-palette-search-icon" aria-hidden="true">
            <circle cx="11" cy="11" r="8" />
            <line x1="21" y1="21" x2="16.65" y2="16.65" />
          </svg>
          <input
            ref={inputRef}
            className="cmd-palette-input"
            placeholder="Type a command…"
            value={query}
            onChange={(e) => { setQuery(e.target.value); setSelectedIdx(0); }}
            onKeyDown={handleKey}
            autoComplete="off"
            spellCheck={false}
          />
        </div>
        <div className="cmd-palette-list">
          {filtered.length === 0 ? (
            <div className="cmd-palette-empty">No matching commands</div>
          ) : (
            filtered.map((cmd, i) => (
              <button
                key={cmd.id}
                className={`cmd-palette-item ${i === selectedIdx ? "selected" : ""}`}
                onClick={() => runCommand(cmd)}
                onMouseEnter={() => setSelectedIdx(i)}
                type="button"
              >
                <span className="cmd-palette-item-label">/{cmd.id}</span>
                <span className="cmd-palette-item-desc">{cmd.description}</span>
              </button>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
