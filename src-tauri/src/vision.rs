use crate::{
    models::{InferClickRequest, InferenceUsage, KeyAction, VisionAction, VisionActionRaw},
    os_context::gather_os_context_snapshot,
    shortcuts, DEFAULT_INFER_MAX_DIM, DEFAULT_MODEL,
};
use openrouter_rs::types::{
    completion::{ResponseUsage, ToolCall},
    Tool,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Cursor,
    sync::{Mutex, OnceLock},
    time::Instant,
};

#[derive(Clone, Debug)]
struct ModelPricing {
    prompt: f64,
    completion: f64,
    image: f64,
    request: f64,
}

fn pricing_cache() -> &'static Mutex<HashMap<String, Option<ModelPricing>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<ModelPricing>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Debug)]
struct ShortcutToolSpec {
    name: String,
    description: String,
    shortcut: String,
    keys: Vec<KeyAction>,
}

#[derive(Debug, Deserialize)]
struct ShortcutToolArgs {
    confidence: f64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ClickToolArgs {
    x_norm: f64,
    y_norm: f64,
    confidence: f64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct TypeToolArgs {
    text: String,
    confidence: f64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ShellToolArgs {
    command: String,
    confidence: f64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct DoneToolArgs {
    reason: String,
}

fn key_action(key: &str, direction: Option<&str>) -> KeyAction {
    KeyAction {
        key: key.to_string(),
        direction: direction.map(str::to_string),
    }
}

fn shortcut_tool(
    name: &str,
    description: &str,
    shortcut: &str,
    keys: Vec<KeyAction>,
) -> ShortcutToolSpec {
    ShortcutToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        shortcut: shortcut.to_string(),
        keys,
    }
}

fn modifier_press_release(
    modifiers: &[&str],
    primary_key: &str,
    primary_direction: Option<&str>,
) -> Vec<KeyAction> {
    let mut keys = Vec::with_capacity(modifiers.len() * 2 + 1);
    for modifier in modifiers {
        keys.push(key_action(modifier, Some("press")));
    }
    keys.push(key_action(primary_key, primary_direction.or(Some("click"))));
    for modifier in modifiers.iter().rev() {
        keys.push(key_action(modifier, Some("release")));
    }
    keys
}

fn built_in_shortcut_tools() -> Vec<ShortcutToolSpec> {
    vec![
        shortcut_tool(
            "browser_new_tab",
            "Open a new browser tab.",
            "Cmd+T",
            modifier_press_release(&["Meta"], "t", Some("click")),
        ),
        shortcut_tool(
            "browser_close_tab",
            "Close the current browser tab.",
            "Cmd+W",
            modifier_press_release(&["Meta"], "w", Some("click")),
        ),
        shortcut_tool(
            "browser_reload",
            "Reload the current browser tab.",
            "Cmd+R",
            modifier_press_release(&["Meta"], "r", Some("click")),
        ),
        shortcut_tool(
            "browser_back",
            "Go back in browser history.",
            "Cmd+[",
            modifier_press_release(&["Meta"], "[", Some("click")),
        ),
        shortcut_tool(
            "browser_forward",
            "Go forward in browser history.",
            "Cmd+]",
            modifier_press_release(&["Meta"], "]", Some("click")),
        ),
        shortcut_tool(
            "system_open_spotlight",
            "Open Spotlight search to switch apps or launch something.",
            "Cmd+Space",
            modifier_press_release(&["Meta"], "Space", Some("click")),
        ),
        shortcut_tool(
            "system_quit_app",
            "Quit the frontmost app.",
            "Cmd+Q",
            modifier_press_release(&["Meta"], "q", Some("click")),
        ),
        shortcut_tool(
            "edit_select_all",
            "Select all content in the focused control.",
            "Cmd+A",
            modifier_press_release(&["Meta"], "a", Some("click")),
        ),
        shortcut_tool(
            "edit_copy",
            "Copy the current selection.",
            "Cmd+C",
            modifier_press_release(&["Meta"], "c", Some("click")),
        ),
        shortcut_tool(
            "edit_paste",
            "Paste clipboard contents.",
            "Cmd+V",
            modifier_press_release(&["Meta"], "v", Some("click")),
        ),
        shortcut_tool(
            "file_save",
            "Save the current document or form.",
            "Cmd+S",
            modifier_press_release(&["Meta"], "s", Some("click")),
        ),
        shortcut_tool(
            "press_return",
            "Press Return or Enter.",
            "Return",
            vec![key_action("Return", Some("click"))],
        ),
        shortcut_tool(
            "press_escape",
            "Press Escape.",
            "Escape",
            vec![key_action("Escape", Some("click"))],
        ),
        shortcut_tool(
            "press_tab",
            "Press Tab.",
            "Tab",
            vec![key_action("Tab", Some("click"))],
        ),
        shortcut_tool(
            "press_shift_tab",
            "Press Shift+Tab to move backward through focusable elements.",
            "Shift+Tab",
            modifier_press_release(&["Shift"], "Tab", Some("click")),
        ),
        shortcut_tool(
            "press_space",
            "Press Space.",
            "Space",
            vec![key_action("Space", Some("click"))],
        ),
        shortcut_tool(
            "press_backspace",
            "Press Backspace.",
            "Backspace",
            vec![key_action("Backspace", Some("click"))],
        ),
        shortcut_tool(
            "press_delete",
            "Press Delete.",
            "Delete",
            vec![key_action("Delete", Some("click"))],
        ),
        shortcut_tool(
            "move_up",
            "Press the Up arrow key.",
            "Up",
            vec![key_action("Up", Some("click"))],
        ),
        shortcut_tool(
            "move_down",
            "Press the Down arrow key.",
            "Down",
            vec![key_action("Down", Some("click"))],
        ),
        shortcut_tool(
            "move_left",
            "Press the Left arrow key.",
            "Left",
            vec![key_action("Left", Some("click"))],
        ),
        shortcut_tool(
            "move_right",
            "Press the Right arrow key.",
            "Right",
            vec![key_action("Right", Some("click"))],
        ),
        shortcut_tool(
            "press_home",
            "Press Home.",
            "Home",
            vec![key_action("Home", Some("click"))],
        ),
        shortcut_tool(
            "press_end",
            "Press End.",
            "End",
            vec![key_action("End", Some("click"))],
        ),
        shortcut_tool(
            "press_page_up",
            "Press Page Up.",
            "PageUp",
            vec![key_action("PageUp", Some("click"))],
        ),
        shortcut_tool(
            "press_page_down",
            "Press Page Down.",
            "PageDown",
            vec![key_action("PageDown", Some("click"))],
        ),
    ]
}

fn split_shortcut_line(line: &str) -> Option<(&str, &str)> {
    [" - ", " — ", " – ", ": "]
        .iter()
        .find_map(|delimiter| line.split_once(delimiter))
        .map(|(shortcut, description)| (shortcut.trim(), description.trim()))
}

fn normalize_shortcut_token(token: &str) -> Option<String> {
    let normalized = token.trim().trim_matches('(').trim_matches(')').trim();
    if normalized.is_empty() {
        return None;
    }

    let lower = normalized.to_lowercase();
    let mapped = match lower.as_str() {
        "cmd" | "command" | "⌘" => "Meta",
        "shift" | "⇧" => "Shift",
        "ctrl" | "control" | "⌃" => "Control",
        "opt" | "option" | "alt" | "⌥" => "Alt",
        "enter" | "return" | "↩" | "⏎" => "Return",
        "esc" | "escape" | "⎋" => "Escape",
        "tab" | "⇥" => "Tab",
        "space" | "spacebar" => "Space",
        "up" | "uparrow" | "↑" => "Up",
        "down" | "downarrow" | "↓" => "Down",
        "left" | "leftarrow" | "←" => "Left",
        "right" | "rightarrow" | "→" => "Right",
        "backspace" | "⌫" => "Backspace",
        "delete" | "del" | "⌦" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page up" => "PageUp",
        "pagedown" | "page down" => "PageDown",
        _ => normalized,
    };

    let cleaned = mapped.trim();
    if cleaned.chars().count() == 1 {
        let ch = cleaned.chars().next().unwrap();
        let normalized = if ch.is_ascii_alphabetic() {
            ch.to_ascii_lowercase().to_string()
        } else {
            ch.to_string()
        };
        return Some(normalized);
    }

    match cleaned {
        "Meta" | "Shift" | "Control" | "Alt" | "Return" | "Escape" | "Tab" | "Space" | "Up"
        | "Down" | "Left" | "Right" | "Backspace" | "Delete" | "Home" | "End" | "PageUp"
        | "PageDown" => Some(cleaned.to_string()),
        _ if cleaned.len() == 2
            && cleaned.starts_with('F')
            && cleaned[1..].parse::<u8>().is_ok() =>
        {
            Some(cleaned.to_string())
        }
        _ => None,
    }
}

fn parse_shortcut_combo(shortcut: &str) -> Option<Vec<KeyAction>> {
    let shortcut = shortcut
        .split('/')
        .next()
        .unwrap_or(shortcut)
        .split(" or ")
        .next()
        .unwrap_or(shortcut)
        .trim();

    if shortcut.is_empty() || shortcut.contains(',') || shortcut.to_lowercase().contains(" then ") {
        return None;
    }

    let mut modifiers = Vec::new();
    let mut primary = None::<String>;

    for token in shortcut.split('+') {
        let key = normalize_shortcut_token(token)?;
        match key.as_str() {
            "Meta" | "Shift" | "Control" | "Alt" => modifiers.push(key),
            _ => {
                if primary.is_some() {
                    return None;
                }
                primary = Some(key);
            }
        }
    }

    let primary = primary?;
    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
    Some(modifier_press_release(
        &modifier_refs,
        &primary,
        Some("click"),
    ))
}

fn slugify_tool_name(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_sep = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }

    let trimmed = out.trim_matches('_');
    let mut slug = if trimmed.is_empty() {
        "shortcut".to_string()
    } else {
        trimmed.to_string()
    };
    if slug
        .chars()
        .next()
        .map(|ch| ch.is_ascii_digit())
        .unwrap_or(false)
    {
        slug.insert_str(0, "shortcut_");
    }
    if slug.len() > 48 {
        slug.truncate(48);
        slug = slug.trim_matches('_').to_string();
    }
    slug
}

fn parse_app_shortcut_tools(shortcuts_text: &str) -> Vec<ShortcutToolSpec> {
    let mut tools = Vec::new();
    let mut names = HashSet::new();

    for line in shortcuts_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((shortcut, description)) = split_shortcut_line(line) else {
            continue;
        };
        let Some(keys) = parse_shortcut_combo(shortcut) else {
            continue;
        };

        let base_name = format!("app_{}", slugify_tool_name(description));
        let mut tool_name = base_name.clone();
        let mut suffix = 2;
        while !names.insert(tool_name.clone()) {
            tool_name = format!("{}_{}", base_name, suffix);
            suffix += 1;
        }

        tools.push(shortcut_tool(
            &tool_name,
            &format!("App-specific shortcut: {}.", description),
            shortcut,
            keys,
        ));
    }

    tools
}

fn tool_reason_schema(description: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "confidence": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0,
                "description": "Confidence that this is the best next action."
            },
            "reason": {
                "type": "string",
                "description": description
            }
        },
        "required": ["confidence", "reason"]
    })
}

fn build_action_tools(shortcut_tools: &[ShortcutToolSpec]) -> Result<Vec<Tool>, String> {
    let click_tool = Tool::builder()
        .name("click_target")
        .description("Click a target at the provided pixel coordinates in the screenshot.")
        .parameters(json!({
            "type": "object",
            "properties": {
                "x_norm": {
                    "type": "number",
                    "description": "Pixel X coordinate in the screenshot."
                },
                "y_norm": {
                    "type": "number",
                    "description": "Pixel Y coordinate in the screenshot."
                },
                "confidence": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "description": "Confidence that the click target is correct."
                },
                "reason": {
                    "type": "string",
                    "description": "Why this click is the best next action."
                }
            },
            "required": ["x_norm", "y_norm", "confidence", "reason"]
        }))
        .build()
        .map_err(|e| format!("Failed to build click tool: {}", e))?;

    let type_tool = Tool::builder()
        .name("type_text")
        .description("Type text into the currently focused control.")
        .parameters(json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Exact text to type."
                },
                "confidence": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "description": "Confidence that typing this text is correct."
                },
                "reason": {
                    "type": "string",
                    "description": "Why typing this text is the best next action."
                }
            },
            "required": ["text", "confidence", "reason"]
        }))
        .build()
        .map_err(|e| format!("Failed to build type tool: {}", e))?;

    let shell_tool = Tool::builder()
        .name("run_shell")
        .description("Run a shell command. Use this only when the user explicitly asks for CLI or terminal work.")
        .parameters(json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute."
                },
                "confidence": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "description": "Confidence that shell is the right interface for this step."
                },
                "reason": {
                    "type": "string",
                    "description": "Why shell is required for this task."
                }
            },
            "required": ["command", "confidence", "reason"]
        }))
        .build()
        .map_err(|e| format!("Failed to build shell tool: {}", e))?;

    let done_tool = Tool::builder()
        .name("task_done")
        .description(
            "Mark the task as complete when the goal is already achieved and visually confirmed.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "What visible evidence confirms the task is done."
                }
            },
            "required": ["reason"]
        }))
        .build()
        .map_err(|e| format!("Failed to build done tool: {}", e))?;

    let mut tools = vec![click_tool, type_tool, shell_tool, done_tool];
    for spec in shortcut_tools {
        tools.push(
            Tool::builder()
                .name(&spec.name)
                .description(&format!(
                    "{} Shortcut: {}.",
                    spec.description, spec.shortcut
                ))
                .parameters(tool_reason_schema(
                    "Why this shortcut is the best next action.",
                ))
                .build()
                .map_err(|e| format!("Failed to build shortcut tool '{}': {}", spec.name, e))?,
        );
    }

    Ok(tools)
}

fn validate_confidence(confidence: f64) -> Result<f64, String> {
    if (0.0..=1.0).contains(&confidence) {
        Ok(confidence)
    } else {
        Err("confidence must be in [0,1]".to_string())
    }
}

fn extract_json_payload(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return Ok(trimmed.to_string());
    }

    if let Some(start) = trimmed.find("```") {
        let rest = &trimmed[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            return Ok(rest[..end].trim().to_string());
        }
    }

    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return Ok(trimmed[start..=end].to_string());
            }
        }
    }

    Err("Could not extract JSON from model response".to_string())
}

fn build_vision_action(
    action: &str,
    x_norm: f64,
    y_norm: f64,
    confidence: f64,
    reason: String,
    model_ms: u128,
    sent_w: u32,
    sent_h: u32,
    model: String,
    provider: Option<String>,
    tool_name: Option<String>,
    shortcut: Option<String>,
    usage: InferenceUsage,
    keys: Option<Vec<KeyAction>>,
    text: Option<String>,
    command: Option<String>,
) -> VisionAction {
    VisionAction {
        action: action.to_string(),
        x_norm,
        y_norm,
        confidence,
        reason,
        model_ms,
        sent_w,
        sent_h,
        model,
        provider,
        tool_name,
        shortcut,
        usage,
        keys,
        text,
        command,
        shell_output: None,
    }
}

fn parse_tool_action(
    tool_call: &ToolCall,
    shortcut_tools: &HashMap<String, ShortcutToolSpec>,
    model_ms: u128,
    sent_w: u32,
    sent_h: u32,
    model: String,
    provider: Option<String>,
    usage: InferenceUsage,
) -> Result<VisionAction, String> {
    let tool_name = tool_call.function.name.as_str();
    let args = tool_call.function.arguments.trim();

    match tool_name {
        "click_target" => {
            let args: ClickToolArgs = serde_json::from_str(args).map_err(|e| e.to_string())?;
            validate_confidence(args.confidence)?;
            if args.x_norm < 0.0
                || args.x_norm > sent_w as f64
                || args.y_norm < 0.0
                || args.y_norm > sent_h as f64
            {
                return Err(format!(
                    "x_norm and y_norm must be pixel coordinates within the image (0-{}, 0-{})",
                    sent_w, sent_h
                ));
            }

            Ok(build_vision_action(
                "click",
                args.x_norm,
                args.y_norm,
                args.confidence,
                args.reason,
                model_ms,
                sent_w,
                sent_h,
                model,
                provider,
                Some("click_target".to_string()),
                None,
                usage,
                None,
                None,
                None,
            ))
        }
        "type_text" => {
            let args: TypeToolArgs = serde_json::from_str(args).map_err(|e| e.to_string())?;
            if args.text.is_empty() {
                return Err("type_text requires a non-empty 'text' field".to_string());
            }
            validate_confidence(args.confidence)?;

            Ok(build_vision_action(
                "type",
                0.0,
                0.0,
                args.confidence,
                args.reason,
                model_ms,
                sent_w,
                sent_h,
                model,
                provider,
                Some("type_text".to_string()),
                None,
                usage,
                None,
                Some(args.text),
                None,
            ))
        }
        "run_shell" => {
            let args: ShellToolArgs = serde_json::from_str(args).map_err(|e| e.to_string())?;
            if args.command.is_empty() {
                return Err("run_shell requires a non-empty 'command' field".to_string());
            }
            validate_confidence(args.confidence)?;

            Ok(build_vision_action(
                "shell",
                0.0,
                0.0,
                args.confidence,
                args.reason,
                model_ms,
                sent_w,
                sent_h,
                model,
                provider,
                Some("run_shell".to_string()),
                None,
                usage,
                None,
                None,
                Some(args.command),
            ))
        }
        "task_done" => {
            let args: DoneToolArgs = serde_json::from_str(args).map_err(|e| e.to_string())?;
            Ok(build_vision_action(
                "none",
                0.0,
                0.0,
                0.0,
                args.reason,
                model_ms,
                sent_w,
                sent_h,
                model,
                provider,
                Some("task_done".to_string()),
                None,
                usage,
                None,
                None,
                None,
            ))
        }
        _ => {
            let args: ShortcutToolArgs = serde_json::from_str(args).map_err(|e| e.to_string())?;
            validate_confidence(args.confidence)?;
            let shortcut = shortcut_tools
                .get(tool_name)
                .ok_or_else(|| format!("Unknown tool returned by model: '{}'", tool_name))?;

            for key in &shortcut.keys {
                crate::parse_key_name(&key.key)?;
            }

            Ok(build_vision_action(
                "hotkey",
                0.0,
                0.0,
                args.confidence,
                args.reason,
                model_ms,
                sent_w,
                sent_h,
                model,
                provider,
                Some(tool_name.to_string()),
                Some(shortcut.shortcut.clone()),
                usage,
                Some(shortcut.keys.clone()),
                None,
                None,
            ))
        }
    }
}

fn parse_vision_action_json(
    content: &str,
    model_ms: u128,
    sent_w: u32,
    sent_h: u32,
    model: String,
    provider: Option<String>,
    usage: InferenceUsage,
) -> Result<VisionAction, String> {
    let json_text = extract_json_payload(content)?;
    let raw: VisionActionRaw = serde_json::from_str(&json_text).map_err(|e| e.to_string())?;

    let action = raw.action.to_lowercase();
    if action != "click"
        && action != "none"
        && action != "hotkey"
        && action != "type"
        && action != "shell"
    {
        return Err("action must be 'click', 'hotkey', 'type', 'shell', or 'none'".to_string());
    }

    if action == "click"
        && (raw.x_norm < 0.0
            || raw.x_norm > sent_w as f64
            || raw.y_norm < 0.0
            || raw.y_norm > sent_h as f64)
    {
        return Err(format!(
            "x_norm and y_norm must be pixel coordinates within the image (0-{}, 0-{})",
            sent_w, sent_h
        ));
    }

    if action == "hotkey" {
        match &raw.keys {
            Some(keys) if !keys.is_empty() => {
                for k in keys {
                    crate::parse_key_name(&k.key)?;
                }
            }
            _ => return Err("hotkey action requires a non-empty 'keys' array".to_string()),
        }
    }

    if action == "type" {
        match &raw.text {
            Some(t) if !t.is_empty() => {}
            _ => return Err("type action requires a non-empty 'text' field".to_string()),
        }
    }

    if action == "shell" {
        match &raw.command {
            Some(c) if !c.is_empty() => {}
            _ => return Err("shell action requires a non-empty 'command' field".to_string()),
        }
    }

    if !(0.0..=1.0).contains(&raw.confidence) {
        return Err("confidence must be in [0,1]".to_string());
    }

    Ok(build_vision_action(
        &action,
        raw.x_norm,
        raw.y_norm,
        raw.confidence,
        raw.reason,
        model_ms,
        sent_w,
        sent_h,
        model,
        provider,
        None,
        None,
        usage,
        raw.keys,
        raw.text,
        raw.command,
    ))
}

fn resolve_model(requested: Option<&str>) -> String {
    requested
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_MODEL)
        .to_string()
}

fn estimate_text_tokens(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil().max(1.0) as u32
}

fn estimate_image_tokens(width: u32, height: u32) -> u32 {
    let megapixels = (width as f64 * height as f64) / 1_000_000.0;
    (85.0 + (megapixels * 170.0)).ceil().max(85.0) as u32
}

fn build_inference_usage(
    system_prompt: &str,
    user_prompt: &str,
    content: &str,
    sent_w: u32,
    sent_h: u32,
    usage: Option<&ResponseUsage>,
) -> InferenceUsage {
    let estimated_prompt_tokens = estimate_text_tokens(system_prompt)
        + estimate_text_tokens(user_prompt)
        + estimate_image_tokens(sent_w, sent_h);
    let estimated_completion_tokens = estimate_text_tokens(content);
    let estimated_total_tokens = estimated_prompt_tokens + estimated_completion_tokens;

    InferenceUsage {
        prompt_tokens: usage.map(|u| u.prompt_tokens),
        completion_tokens: usage.map(|u| u.completion_tokens),
        total_tokens: usage.map(|u| u.total_tokens),
        estimated_prompt_tokens,
        estimated_completion_tokens,
        estimated_total_tokens,
        estimated_cost_usd: None,
    }
}

fn parse_pricing_value(value: &str) -> Option<f64> {
    value.parse::<f64>().ok()
}

async fn fetch_model_pricing(
    api_key: &str,
    api_base: &str,
    model_id: &str,
) -> Result<Option<ModelPricing>, String> {
    if let Ok(cache) = pricing_cache().lock() {
        if let Some(cached) = cache.get(model_id) {
            return Ok(cached.clone());
        }
    }

    let client = openrouter_rs::OpenRouterClient::builder()
        .base_url(api_base)
        .api_key(api_key)
        .build()
        .map_err(|e| format!("OpenRouter pricing client error: {}", e))?;

    let models = client
        .list_models()
        .await
        .map_err(|e| format!("OpenRouter models API error: {}", e))?;

    let mut parsed_models = HashMap::new();
    for model in models {
        let pricing = match (
            parse_pricing_value(&model.pricing.prompt),
            parse_pricing_value(&model.pricing.completion),
        ) {
            (Some(prompt), Some(completion)) => Some(ModelPricing {
                prompt,
                completion,
                image: model
                    .pricing
                    .image
                    .as_deref()
                    .and_then(parse_pricing_value)
                    .unwrap_or(0.0),
                request: model
                    .pricing
                    .request
                    .as_deref()
                    .and_then(parse_pricing_value)
                    .unwrap_or(0.0),
            }),
            _ => None,
        };
        parsed_models.insert(model.id, pricing);
    }

    if let Ok(mut cache) = pricing_cache().lock() {
        cache.extend(parsed_models.clone());
    }

    Ok(parsed_models.get(model_id).cloned().unwrap_or(None))
}

fn estimate_inference_cost(
    usage: &InferenceUsage,
    pricing: &ModelPricing,
    image_inputs: u32,
) -> f64 {
    let prompt_tokens = usage.prompt_tokens.unwrap_or(usage.estimated_prompt_tokens) as f64;
    let completion_tokens = usage
        .completion_tokens
        .unwrap_or(usage.estimated_completion_tokens) as f64;

    prompt_tokens * pricing.prompt
        + completion_tokens * pricing.completion
        + image_inputs as f64 * pricing.image
        + pricing.request
}

/// Fallback pricing when OpenRouter API is unavailable.
/// Returns approximate per-token pricing for known models.
fn fallback_model_pricing(model_id: &str) -> ModelPricing {
    let lower = model_id.to_lowercase();
    // Pricing per token (not per million) — divide standard $/M rates by 1_000_000
    let (prompt, completion) = if lower.contains("gpt-4o") && !lower.contains("mini") {
        (2.5e-6, 10.0e-6)   // GPT-4o: $2.50/$10 per M
    } else if lower.contains("gpt-4o-mini") {
        (0.15e-6, 0.60e-6)  // GPT-4o-mini: $0.15/$0.60 per M
    } else if lower.contains("gpt-4.1") {
        (2.0e-6, 8.0e-6)    // GPT-4.1: ~$2/$8 per M
    } else if lower.contains("gpt-5") {
        (2.0e-6, 8.0e-6)    // GPT-5: ~$2/$8 per M (estimate)
    } else if lower.contains("claude-3.5-sonnet") || lower.contains("claude-3-5-sonnet") {
        (3.0e-6, 15.0e-6)   // Claude 3.5 Sonnet: $3/$15 per M
    } else if lower.contains("claude-sonnet-4") {
        (3.0e-6, 15.0e-6)   // Claude Sonnet 4: $3/$15 per M
    } else if lower.contains("gemini-2") || lower.contains("gemini-pro") {
        (1.25e-6, 5.0e-6)   // Gemini 2: ~$1.25/$5 per M
    } else if lower.contains("mistral-large") {
        (2.0e-6, 6.0e-6)    // Mistral Large: $2/$6 per M
    } else if lower.contains("llama") {
        (0.80e-6, 0.80e-6)  // Llama: ~$0.80/$0.80 per M
    } else {
        (3.0e-6, 15.0e-6)   // Default: ~$3/$15 per M (conservative)
    };

    ModelPricing {
        prompt,
        completion,
        image: 0.003,  // ~$0.003 per image
        request: 0.0,
    }
}

fn infer_max_dim() -> u32 {
    std::env::var("AGENT_INFER_MAX_DIM")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.clamp(640, 4096))
        .unwrap_or(DEFAULT_INFER_MAX_DIM)
}

fn load_infer_image_bytes(path: &str) -> Result<(Vec<u8>, u32, u32, u32, u32), String> {
    let raw = fs::read(path).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&raw).map_err(|e| e.to_string())?;
    let orig_w = img.width();
    let orig_h = img.height();
    let max_dim = infer_max_dim();

    if orig_w.max(orig_h) <= max_dim {
        return Ok((raw, orig_w, orig_h, orig_w, orig_h));
    }

    let scale = (max_dim as f64) / (orig_w.max(orig_h) as f64);
    let new_w = ((orig_w as f64 * scale).round() as u32).max(1);
    let new_h = ((orig_h as f64 * scale).round() as u32).max(1);

    let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);
    let mut cursor = Cursor::new(Vec::new());
    resized
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;

    Ok((cursor.into_inner(), orig_w, orig_h, new_w, new_h))
}

#[tauri::command]
pub async fn infer_click_cmd(req: InferClickRequest) -> Result<VisionAction, String> {
    use openrouter_rs::{
        api::chat::{ChatCompletionRequest, Content, ContentPart, Message},
        types::Role,
        OpenRouterClient,
    };

    let started = Instant::now();
    let (image_bytes, orig_w, orig_h, sent_w, sent_h) = load_infer_image_bytes(&req.png_path)?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
    let image_url = format!("data:image/png;base64,{}", b64);
    if (orig_w, orig_h) != (sent_w, sent_h) {
        println!(
            "[telemetry] infer_image downscaled {}x{} -> {}x{} (max_dim={})",
            orig_w,
            orig_h,
            sent_w,
            sent_h,
            infer_max_dim()
        );
    } else {
        println!(
            "[telemetry] infer_image using original {}x{} (max_dim={})",
            orig_w,
            orig_h,
            infer_max_dim()
        );
    }

    let system_prompt = include_str!("system_prompt.txt");

    let os_context = gather_os_context_snapshot();
    let frontmost_app = os_context.frontmost_app_name.clone();
    println!(
        "[telemetry] os_context: {}",
        os_context.context_block.replace('\n', " | ")
    );

    let shortcuts_text = if !frontmost_app.is_empty() {
        let api_key_for_shortcuts = crate::resolve_primary_api_key();
        let api_base_for_shortcuts = crate::resolve_primary_api_base();
        shortcuts::get_or_fetch_global(
            &frontmost_app,
            api_key_for_shortcuts.trim(),
            &api_base_for_shortcuts,
        )
        .await
    } else {
        String::new()
    };
    let mut shortcut_tools = built_in_shortcut_tools();
    shortcut_tools.extend(parse_app_shortcut_tools(&shortcuts_text));
    let shortcut_tool_lookup: HashMap<String, ShortcutToolSpec> = shortcut_tools
        .iter()
        .cloned()
        .map(|spec| (spec.name.clone(), spec))
        .collect();
    let tools = build_action_tools(&shortcut_tools)?;

    let mut user_prompt = format!(
        "Task: {}\nCoordinate system: PIXEL coordinates. The screenshot image is {}x{} pixels wide and tall. (0,0) is the top-left corner. ({},{}) is the bottom-right corner. Return x_norm as the pixel column (0 to {}) and y_norm as the pixel row (0 to {}).{}",
        req.instruction,
        sent_w,
        sent_h,
        sent_w.saturating_sub(1),
        sent_h.saturating_sub(1),
        sent_w.saturating_sub(1),
        sent_h.saturating_sub(1),
        os_context.context_block
    );

    if !shortcuts_text.is_empty() {
        user_prompt.push_str(&format!(
            "\n\nApp-specific shortcuts for {} have been converted into named tools when they could be parsed. Prefer those app_* tools before clicking.",
            frontmost_app
        ));
    }

    user_prompt.push_str(
        "\n\nLook at the screenshot carefully. Call the NEXT single tool that makes progress toward the goal.",
    );

    if let Some(ctx) = &req.step_context {
        user_prompt.push_str(&format!("\n\nPrevious actions taken:\n{}", ctx));
    }

    let api_key = crate::resolve_primary_api_key();
    let api_base = crate::resolve_primary_api_base();
    if api_key.trim().is_empty() {
        return Err("API key is missing. Set OPENROUTER_API_KEY (or MISTRAL_API_KEY).".to_string());
    }

    let model = resolve_model(req.model.as_deref());

    let client = OpenRouterClient::builder()
        .base_url(&api_base)
        .api_key(api_key.trim())
        .http_referer("https://computer-use.local")
        .x_title("Computer Use")
        .build()
        .map_err(|e| format!("OpenRouter client error: {}", e))?;

    println!("[provider] using openrouter model={}", model);

    let request = ChatCompletionRequest::builder()
        .model(&model)
        .messages(vec![
            Message::new(Role::System, system_prompt),
            Message::new(
                Role::User,
                Content::Parts(vec![
                    ContentPart::text(&user_prompt),
                    ContentPart::image_url(&image_url),
                ]),
            ),
        ])
        .tools(tools)
        .tool_choice_required()
        .parallel_tool_calls(false)
        .temperature(0.0)
        .build()
        .map_err(|e| format!("Request build error: {}", e))?;

    let response = client
        .send_chat_completion(&request)
        .await
        .map_err(|e| format!("OpenRouter API error: {}", e))?;

    let choice = response
        .choices
        .first()
        .ok_or_else(|| "No response choices returned".to_string())?;
    let content = choice.content().unwrap_or("").to_string();
    let tool_calls = choice
        .tool_calls()
        .map(|calls| calls.to_vec())
        .unwrap_or_default();
    let usage_payload = if !tool_calls.is_empty() {
        serde_json::to_string(&tool_calls).unwrap_or_else(|_| content.clone())
    } else {
        content.clone()
    };

    let model_ms = started.elapsed().as_millis();
    let mut usage = build_inference_usage(
        system_prompt,
        &user_prompt,
        &usage_payload,
        sent_w,
        sent_h,
        response.usage.as_ref(),
    );
    if let Some(pricing) = fetch_model_pricing(api_key.trim(), &api_base, &response.model)
        .await
        .ok()
        .flatten()
    {
        usage.estimated_cost_usd = Some(estimate_inference_cost(&usage, &pricing, 1));
    } else {
        // Fallback: estimate cost from known model pricing (per token, not per M)
        let fallback = fallback_model_pricing(&response.model);
        usage.estimated_cost_usd = Some(estimate_inference_cost(&usage, &fallback, 1));
    }
    let prompt_tokens = usage.prompt_tokens.unwrap_or(usage.estimated_prompt_tokens);
    let completion_tokens = usage
        .completion_tokens
        .unwrap_or(usage.estimated_completion_tokens);
    let total_tokens = usage.total_tokens.unwrap_or(usage.estimated_total_tokens);
    let estimated_cost = usage.estimated_cost_usd.unwrap_or(0.0);
    let parsed = if tool_calls.len() == 1 {
        parse_tool_action(
            &tool_calls[0],
            &shortcut_tool_lookup,
            model_ms,
            sent_w,
            sent_h,
            response.model.clone(),
            response.provider.clone(),
            usage,
        )?
    } else if tool_calls.is_empty() {
        parse_vision_action_json(
            &content,
            model_ms,
            sent_w,
            sent_h,
            response.model.clone(),
            response.provider.clone(),
            usage,
        )?
    } else {
        return Err(format!(
            "Model returned {} tool calls, but exactly one action is required",
            tool_calls.len()
        ));
    };

    println!(
        "[telemetry] model_ms={} action={} confidence={:.3} model={} provider={} tokens={}/{}/{} est_cost=${:.6}",
        model_ms,
        parsed.action,
        parsed.confidence,
        parsed.model,
        parsed.provider.as_deref().unwrap_or("openrouter"),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        estimated_cost
    );

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::{
        build_inference_usage, estimate_image_tokens, estimate_text_tokens, infer_click_cmd,
        parse_app_shortcut_tools, parse_shortcut_combo,
    };
    use crate::models::InferClickRequest;
    use image::{ImageBuffer, Rgba};
    use std::{env, error::Error};

    #[test]
    fn usage_estimates_scale_with_prompt_and_image_size() {
        let short = build_inference_usage("sys", "user", "{}", 512, 512, None);
        let long = build_inference_usage(
            &"system ".repeat(200),
            &"context ".repeat(400),
            &"{\"action\":\"none\"}",
            2048,
            1536,
            None,
        );

        assert!(estimate_text_tokens("hello world") > 0);
        assert!(estimate_image_tokens(2048, 1536) > estimate_image_tokens(512, 512));
        assert!(long.estimated_prompt_tokens > short.estimated_prompt_tokens);
        assert!(long.estimated_total_tokens > short.estimated_total_tokens);
        assert!(long.estimated_cost_usd.is_none());
    }

    #[test]
    fn parses_common_shortcut_combos_into_key_sequences() {
        let cmd_t = parse_shortcut_combo("Cmd+T").expect("should parse Cmd+T");
        assert_eq!(cmd_t.len(), 3);
        assert_eq!(cmd_t[0].key, "Meta");
        assert_eq!(cmd_t[0].direction.as_deref(), Some("press"));
        assert_eq!(cmd_t[1].key, "t");
        assert_eq!(cmd_t[1].direction.as_deref(), Some("click"));
        assert_eq!(cmd_t[2].key, "Meta");
        assert_eq!(cmd_t[2].direction.as_deref(), Some("release"));

        let shift_tab = parse_shortcut_combo("Shift+Tab").expect("should parse Shift+Tab");
        assert_eq!(shift_tab[0].key, "Shift");
        assert_eq!(shift_tab[1].key, "Tab");
        assert_eq!(shift_tab[2].key, "Shift");
    }

    #[test]
    fn converts_app_shortcut_lines_into_tools() {
        let tools = parse_app_shortcut_tools(
            "Cmd+T - New tab\nCmd+Shift+[ - Move tab left\nCtrl+Alt+P - Toggle panel",
        );
        assert_eq!(tools.len(), 3);
        assert!(tools.iter().any(|tool| tool.name == "app_new_tab"));
        assert!(tools.iter().any(|tool| tool.name == "app_move_tab_left"));
        assert!(tools.iter().any(|tool| tool.name == "app_toggle_panel"));
    }

    #[test]
    #[ignore = "requires OPENROUTER_API_KEY and network access"]
    fn live_model_returns_valid_action_and_usage() -> Result<(), Box<dyn Error>> {
        tauri::async_runtime::block_on(async {
            let _ = dotenvy::dotenv();

            if crate::resolve_primary_api_key().trim().is_empty() {
                eprintln!("skipping live model test: no API key found in env or .env");
                return Ok(());
            }

            let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_pixel(1280, 800, Rgba([245, 247, 250, 255]));
            for y in 280..520 {
                for x in 360..920 {
                    img.put_pixel(x, y, Rgba([29, 185, 84, 255]));
                }
            }

            let path = env::temp_dir().join("computer-use-live-model-test.png");
            img.save(&path)?;

            let requested_model = env::var("OPENROUTER_TEST_MODEL")
                .unwrap_or_else(|_| crate::DEFAULT_MODEL.to_string());

            let result = infer_click_cmd(InferClickRequest {
                png_path: path.to_string_lossy().to_string(),
                instruction: "Click the large green rectangle centered in the image".to_string(),
                model: Some(requested_model.clone()),
                step_context: Some(
                    "Synthetic validation image. There is no HUD; click the green rectangle only."
                        .to_string(),
                ),
            })
            .await?;

            eprintln!(
                "live model output: action={} reason={} model={} provider={:?} usage={:?}/{:?}/{:?} est_total={}",
                result.action,
                result.reason,
                result.model,
                result.provider,
                result.usage.prompt_tokens,
                result.usage.completion_tokens,
                result.usage.total_tokens,
                result.usage.estimated_total_tokens
            );

            assert!(!result.model.is_empty());
            assert!(result.usage.estimated_total_tokens > 0);
            assert!(result.usage.estimated_cost_usd.unwrap_or(0.0) >= 0.0);
            assert!(
                result.action == "click" || result.action == "none" || result.action == "hotkey",
                "unexpected live action: {}",
                result.action
            );
            if result.action == "click" {
                assert!((360.0..=920.0).contains(&result.x_norm));
                assert!((280.0..=520.0).contains(&result.y_norm));
            }

            Ok(())
        })
    }
}
