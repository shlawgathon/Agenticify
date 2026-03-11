// ── Shared Constants ───────────────────────────────────

export const OVERLAY_LABEL = "overlay";
export const OVERLAY_QUERY_KEY = "overlay";
export const HUD_LABEL = "hud";
export const HUD_QUERY_KEY = "hud";
export const MAIN_LABEL = "main";
export const DEFAULT_HUD_MODEL = "openai/gpt-5.4";

export const MODEL_OPTIONS = [
  { id: "mistralai/mistral-large-2512", label: "Mistral Large 3" },
  { id: "qwen/qwen3.5-35b-a3b", label: "Qwen 3.5 35B A3B" },
  { id: "openai/gpt-5.4", label: "GPT-5.4" },
  { id: "anthropic/claude-sonnet-4.6", label: "Claude Sonnet 4.6" },
  { id: "google/gemini-3.1-pro-preview-customtools", label: "Gemini 3.1 Pro Preview (Custom Tools)" },
] as const;

export const HUD_WIDTH = 460;
export const HUD_HEIGHT = 42;

export const FLOW = [
  "UI calls `capture_primary_cmd`; Rust captures the primary display and writes a PNG in temp storage.",
  "UI sends `png_path` + instruction to `infer_click_cmd` (instruction already includes your Task Context).",
  "Rust calls the selected OpenRouter model with strict JSON output parsing and records `action`, coordinates, confidence, reason, model, and token usage.",
  "UI executes click only when `action=click`; Rust still enforces confidence threshold, E-STOP, and max-action cap before clicking.",
  "Rust converts normalized coordinates to macOS logical points, performs real click with enigo, then emits `agent_cursor_event` for overlay visuals.",
];
