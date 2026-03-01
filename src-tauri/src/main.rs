#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod recording;
mod shortcuts;


#[allow(unused_imports)]
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Code, Modifiers, ShortcutState};
use xcap::Monitor;

const MAX_ACTIONS_PER_RUN: u32 = 30;
const DEFAULT_MISTRAL_MODEL: &str = "mistralai/mistral-large-2512";
const DEFAULT_MISTRAL_BASE: &str = "https://openrouter.ai/api/v1";
const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.60;
const DEFAULT_INFER_MAX_DIM: u32 = 2048;

/// Global mutex to serialize all xcap screen captures.
/// macOS's CGWindowListCreateImage is NOT safe to call concurrently
/// from multiple threads — doing so crashes the process.
pub(crate) fn capture_mutex() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Default)]
struct RuntimeGuards {
    estop: AtomicBool,
    actions: AtomicU32,
}

#[derive(Default)]
struct DisplayState {
    primary_scale_factor: Arc<RwLock<f64>>,
}

#[derive(Default)]
struct RecordingState {
    active: Mutex<Option<ActiveRecording>>,
}

struct ActiveRecording {
    session_id: String,
    output_dir: PathBuf,
    fps: u32,
    started_unix_ms: u128,
    stop_flag: Arc<AtomicBool>,
    frame_ticks: Arc<AtomicU64>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Serialize)]
struct PermissionState {
    screen_recording: bool,
    accessibility: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CaptureFrame {
    monitor_id: u32,
    monitor_origin_x_pt: i32,
    monitor_origin_y_pt: i32,
    screenshot_w_px: u32,
    screenshot_h_px: u32,
    scale_factor: f64,
    png_path: String,
    capture_ms: u128,
}

#[derive(Debug, Clone, Deserialize)]
struct InferClickRequest {
    png_path: String,
    instruction: String,
    #[allow(dead_code)]
    model: Option<String>,
    /// Prior action history for multi-step loops
    #[serde(default)]
    step_context: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClickRequest {
    x_norm: f64,
    y_norm: f64,
    screenshot_w_px: u32,
    screenshot_h_px: u32,
    sent_w_px: u32,
    sent_h_px: u32,
    monitor_origin_x_pt: i32,
    monitor_origin_y_pt: i32,
    scale_factor: f64,
    confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
struct VisionAction {
    action: String,
    x_norm: f64,
    y_norm: f64,
    confidence: f64,
    reason: String,
    model_ms: u128,
    sent_w: u32,
    sent_h: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<Vec<KeyAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyAction {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RuntimeState {
    estop: bool,
    actions: u32,
    max_actions: u32,
}

#[derive(Debug, Clone, Serialize)]
struct EnvStatus {
    mistral_api_key_loaded: bool,
    mistral_api_base: String,
}

#[derive(Debug, Clone, Serialize)]
struct MistralAuthStatus {
    ok: bool,
    http_status: Option<u16>,
    message: String,
    mistral_api_base: String,
}

#[derive(Debug, Clone, Serialize)]
struct RecordingStatus {
    active: bool,
    session_id: Option<String>,
    output_dir: Option<String>,
    fps: Option<u32>,
    frame_ticks: u64,
    started_unix_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordingSummary {
    session_id: String,
    output_dir: String,
    fps: u32,
    frame_ticks: u64,
    duration_ms: u128,
    #[serde(default)]
    name: String,
    #[serde(default)]
    instruction: String,
    #[serde(default)]
    task_context: String,
    #[serde(default)]
    model: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentCursorEvent {
    x_pt: i32,
    y_pt: i32,
    monitor_origin_x_pt: i32,
    monitor_origin_y_pt: i32,
    phase: String,
    unix_ms: u128,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionReplayRequest {
    session_id: String,
    instruction: String,
    model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionReplayResult {
    session_id: String,
    monitor_id: u32,
    frame_path: String,
    action: VisionAction,
    clicked: bool,
}

#[derive(Debug, Deserialize)]
struct VisionActionRaw {
    action: String,
    #[serde(default)]
    x_norm: f64,
    #[serde(default)]
    y_norm: f64,
    confidence: f64,
    reason: String,
    #[serde(default)]
    keys: Option<Vec<KeyAction>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct KeyCombo {
    key: String,
    direction: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PressKeysRequest {
    keys: Vec<KeyCombo>,
    delay_ms: Option<u64>,
}

fn now_unix_ms() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())
        .map(|d| d.as_millis())
}

fn recordings_root_dir() -> PathBuf {
    std::env::temp_dir().join("agenticify-recordings")
}

fn monitor_by_id(monitor_id: u32) -> Result<Monitor, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    monitors
        .into_iter()
        .find(|m| m.id().ok() == Some(monitor_id))
        .ok_or_else(|| format!("Monitor {} is not currently available", monitor_id))
}

fn latest_frame_for_session(session_id: &str) -> Result<(PathBuf, u32), String> {
    let session_dir = recordings_root_dir().join(session_id);
    if !session_dir.exists() {
        return Err(format!("Session '{}' not found", session_id));
    }

    let mut best: Option<(String, PathBuf, u32)> = None;
    let monitor_dirs = fs::read_dir(&session_dir).map_err(|e| e.to_string())?;
    for monitor_entry in monitor_dirs {
        let monitor_path = monitor_entry.map_err(|e| e.to_string())?.path();
        if !monitor_path.is_dir() {
            continue;
        }

        let dir_name = monitor_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let Some(monitor_id_str) = dir_name.strip_prefix("monitor-") else {
            continue;
        };
        let Ok(monitor_id) = monitor_id_str.parse::<u32>() else {
            continue;
        };

        let frames = fs::read_dir(&monitor_path).map_err(|e| e.to_string())?;
        for frame_entry in frames {
            let frame_path = frame_entry.map_err(|e| e.to_string())?.path();
            if !frame_path.is_file() {
                continue;
            }
            if frame_path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }

            let frame_name = frame_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();

            let replace = match &best {
                Some((best_name, _, _)) => frame_name > *best_name,
                None => true,
            };
            if replace {
                best = Some((frame_name, frame_path.clone(), monitor_id));
            }
        }
    }

    best
        .map(|(_, path, monitor_id)| (path, monitor_id))
        .ok_or_else(|| format!("No frame images found for session '{}'", session_id))
}

fn confidence_threshold() -> f64 {
    std::env::var("AGENT_CONFIDENCE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(DEFAULT_CONFIDENCE_THRESHOLD)
}

fn clamp_pixel(v: f64, max: f64) -> f64 {
    v.max(0.0).min(max)
}

fn pixel_to_global_points(req: &ClickRequest) -> (i32, i32) {
    // x_norm/y_norm are pixel coordinates in the SENT (possibly downscaled) image.
    // Scale them back to original screenshot pixel coordinates, then to screen points.
    let scale_x = req.screenshot_w_px as f64 / req.sent_w_px.max(1) as f64;
    let scale_y = req.screenshot_h_px as f64 / req.sent_h_px.max(1) as f64;

    let x_px = clamp_pixel(req.x_norm * scale_x, (req.screenshot_w_px.saturating_sub(1)) as f64);
    let y_px = clamp_pixel(req.y_norm * scale_y, (req.screenshot_h_px.saturating_sub(1)) as f64);

    let x_pt = req.monitor_origin_x_pt as f64 + (x_px / req.scale_factor.max(1.0));
    let y_pt = req.monitor_origin_y_pt as f64 + (y_px / req.scale_factor.max(1.0));

    (x_pt.round() as i32, y_pt.round() as i32)
}

fn perform_real_click(
    app: Option<&tauri::AppHandle>,
    guards: &RuntimeGuards,
    req: &ClickRequest,
) -> Result<(), String> {
    if guards.estop.load(Ordering::SeqCst) {
        return Err("Emergency stop active".to_string());
    }

    if req.confidence < confidence_threshold() {
        return Err(format!(
            "Confidence {:.3} below threshold {:.3}",
            req.confidence,
            confidence_threshold()
        ));
    }

    let n = guards.actions.fetch_add(1, Ordering::SeqCst);
    if n >= MAX_ACTIONS_PER_RUN {
        guards.estop.store(true, Ordering::SeqCst);
        return Err("Max actions reached; E-STOP engaged".to_string());
    }

    let (x_pt, y_pt) = pixel_to_global_points(req);
    let started = Instant::now();

    if let Some(app_handle) = app {
        let _ = app_handle.emit(
            "agent_cursor_event",
            AgentCursorEvent {
                x_pt,
                y_pt,
                monitor_origin_x_pt: req.monitor_origin_x_pt,
                monitor_origin_y_pt: req.monitor_origin_y_pt,
                phase: "move".to_string(),
                unix_ms: now_unix_ms().unwrap_or(0),
            },
        );
    }

    // Use CGEvent to click without moving the user's physical cursor.
    // This posts synthetic mouse-down + mouse-up events at the target point,
    // so the agent can click through overlay windows and the user keeps control.
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, CGEventTapLocation, EventField};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        use core_graphics::geometry::CGPoint;

        let point = CGPoint::new(x_pt as f64, y_pt as f64);
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "Failed to create CGEventSource")?;

        let mouse_down = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::LeftMouseDown,
            point,
            CGMouseButton::Left,
        ).map_err(|_| "Failed to create mouse-down CGEvent")?;
        mouse_down.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);

        let mouse_up = CGEvent::new_mouse_event(
            source,
            CGEventType::LeftMouseUp,
            point,
            CGMouseButton::Left,
        ).map_err(|_| "Failed to create mouse-up CGEvent")?;
        mouse_up.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);

        mouse_down.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(30));
        mouse_up.post(CGEventTapLocation::HID);
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Fallback to enigo on non-macOS platforms
        let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        enigo.move_mouse(x_pt, y_pt, Coordinate::Abs).map_err(|e| e.to_string())?;
        enigo.button(Button::Left, Direction::Click).map_err(|e| e.to_string())?;
    }

    if let Some(app_handle) = app {
        let _ = app_handle.emit(
            "agent_cursor_event",
            AgentCursorEvent {
                x_pt,
                y_pt,
                monitor_origin_x_pt: req.monitor_origin_x_pt,
                monitor_origin_y_pt: req.monitor_origin_y_pt,
                phase: "click".to_string(),
                unix_ms: now_unix_ms().unwrap_or(0),
            },
        );
    }

    let click_ms = started.elapsed().as_millis();
    println!(
        "[telemetry] click_ms={} point=({}, {}) norm=({:.1}, {:.1}) screenshot={}x{} scale={:.2} monitor_origin=({}, {}) action_count={}",
        click_ms,
        x_pt,
        y_pt,
        req.x_norm,
        req.y_norm,
        req.screenshot_w_px,
        req.screenshot_h_px,
        req.scale_factor,
        req.monitor_origin_x_pt,
        req.monitor_origin_y_pt,
        n + 1
    );

    Ok(())
}

fn primary_monitor() -> Result<Monitor, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .ok_or_else(|| "No primary monitor found".to_string())
}

#[cfg(target_os = "macos")]
fn primary_backing_scale_factor() -> Option<f64> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let mtm = MainThreadMarker::new()?;
    NSScreen::mainScreen(mtm).map(|s| s.backingScaleFactor() as f64)
}

#[cfg(not(target_os = "macos"))]
fn primary_backing_scale_factor() -> Option<f64> {
    Some(1.0)
}

#[cfg(target_os = "macos")]
fn check_permissions() -> PermissionState {
    use core_graphics::access::ScreenCaptureAccess;
    use macos_accessibility_client::accessibility::application_is_trusted;

    PermissionState {
        screen_recording: ScreenCaptureAccess::default().preflight(),
        accessibility: application_is_trusted(),
    }
}

#[cfg(not(target_os = "macos"))]
fn check_permissions() -> PermissionState {
    PermissionState {
        screen_recording: true,
        accessibility: true,
    }
}

#[cfg(target_os = "macos")]
fn request_permissions() -> PermissionState {
    use core_graphics::access::ScreenCaptureAccess;
    use macos_accessibility_client::accessibility::application_is_trusted_with_prompt;

    let _ = ScreenCaptureAccess::default().request();
    let _ = application_is_trusted_with_prompt();

    let state = check_permissions();

    if !state.screen_recording {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .status();
    }

    if !state.accessibility {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
    }

    state
}

#[cfg(not(target_os = "macos"))]
fn request_permissions() -> PermissionState {
    check_permissions()
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

fn parse_vision_action(content: &str, model_ms: u128, sent_w: u32, sent_h: u32) -> Result<VisionAction, String> {
    let json_text = extract_json_payload(content)?;
    let raw: VisionActionRaw = serde_json::from_str(&json_text).map_err(|e| e.to_string())?;

    let action = raw.action.to_lowercase();
    if action != "click" && action != "none" && action != "hotkey" && action != "type" && action != "shell" {
        return Err("action must be 'click', 'hotkey', 'type', 'shell', or 'none'".to_string());
    }

    if action == "click" {
        if raw.x_norm < 0.0 || raw.x_norm > sent_w as f64 || raw.y_norm < 0.0 || raw.y_norm > sent_h as f64 {
            return Err(format!("x_norm and y_norm must be pixel coordinates within the image (0-{}, 0-{})", sent_w, sent_h));
        }
    }

    if action == "hotkey" {
        match &raw.keys {
            Some(keys) if !keys.is_empty() => {
                for k in keys {
                    parse_key_name(&k.key)?;
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

    Ok(VisionAction {
        action,
        x_norm: raw.x_norm,
        y_norm: raw.y_norm,
        confidence: raw.confidence,
        reason: raw.reason,
        model_ms,
        sent_w,
        sent_h,
        keys: raw.keys,
        text: raw.text,
        command: raw.command,
        shell_output: None,
    })
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



fn resolve_primary_api_base() -> String {
    std::env::var("OPENROUTER_API_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("MISTRAL_API_BASE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_MISTRAL_BASE.to_string())
}

fn resolve_primary_api_key() -> String {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("MISTRAL_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
}

fn is_openrouter_base(base: &str) -> bool {
    base.to_ascii_lowercase().contains("openrouter.ai")
}

#[tauri::command]
fn check_permissions_cmd() -> PermissionState {
    check_permissions()
}

#[tauri::command]
fn request_permissions_cmd() -> PermissionState {
    request_permissions()
}

#[tauri::command]
fn env_status_cmd() -> EnvStatus {
    let key_loaded = !resolve_primary_api_key().trim().is_empty();
    let base = resolve_primary_api_base();

    EnvStatus {
        mistral_api_key_loaded: key_loaded,
        mistral_api_base: base,
    }
}

#[tauri::command]
async fn validate_mistral_api_key_cmd() -> Result<MistralAuthStatus, String> {
    let base = resolve_primary_api_base();
    let api_key = resolve_primary_api_key().trim().to_string();

    if api_key.is_empty() {
        return Ok(MistralAuthStatus {
            ok: false,
            http_status: None,
            message: "Primary API key is missing (set OPENROUTER_API_KEY or MISTRAL_API_KEY)"
                .to_string(),
            mistral_api_base: base,
        });
    }

    let url = format!("{}/models", base.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(r) => r,
        Err(err) => {
            return Ok(MistralAuthStatus {
                ok: false,
                http_status: None,
            message: format!("Network error while contacting API provider: {}", err),
            mistral_api_base: base,
        });
    }
    };

    let status = resp.status();
    let code = status.as_u16();
    if status.is_success() {
        return Ok(MistralAuthStatus {
            ok: true,
            http_status: Some(code),
            message: "API key validated against provider".to_string(),
            mistral_api_base: base,
        });
    }

    let body = resp.text().await.unwrap_or_else(|_| "<no body>".to_string());
    let compact_body = body.chars().take(220).collect::<String>();
    let message = if status == reqwest::StatusCode::UNAUTHORIZED
        && is_openrouter_base(&base)
    {
        "Unauthorized (401): OPENROUTER_API_KEY is invalid or revoked.".to_string()
    } else if status == reqwest::StatusCode::UNAUTHORIZED {
        "Unauthorized (401): MISTRAL_API_KEY is invalid or revoked.".to_string()
    } else {
        format!("API error {}: {}", code, compact_body)
    };

    Ok(MistralAuthStatus {
        ok: false,
        http_status: Some(code),
        message,
        mistral_api_base: base,
    })
}

#[tauri::command]
fn recordings_root_cmd() -> Result<String, String> {
    let root = recordings_root_dir();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    Ok(root.to_string_lossy().to_string())
}

#[tauri::command]
fn list_recording_sessions_cmd() -> Result<Vec<RecordingSummary>, String> {
    let root = recordings_root_dir();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        let text = fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
        let mut summary: RecordingSummary = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if summary.output_dir.is_empty() {
            summary.output_dir = path.to_string_lossy().to_string();
        }
        sessions.push(summary);
    }

    sessions.sort_by(|a, b| b.session_id.cmp(&a.session_id));
    Ok(sessions)
}

#[tauri::command]
fn open_path_cmd(path: String) -> Result<(), String> {
    let p = PathBuf::from(path);
    if !p.exists() {
        return Err("Path does not exist".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&p)
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(&p)
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(&p)
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err("Unsupported OS for open_path_cmd".to_string())
}

#[tauri::command]
fn export_markdown_cmd(filename: String, content: String) -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory".to_string())?;
    let desktop = PathBuf::from(&home).join("Desktop");
    if !desktop.exists() {
        fs::create_dir_all(&desktop).map_err(|e| e.to_string())?;
    }

    // Avoid overwriting: if file exists, append a counter
    let base = filename.trim_end_matches(".md");
    let mut target = desktop.join(&filename);
    let mut counter = 1u32;
    while target.exists() {
        target = desktop.join(format!("{}-{}.md", base, counter));
        counter += 1;
    }

    fs::write(&target, &content).map_err(|e| e.to_string())?;

    // Reveal in Finder
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(&target).status();
    }

    let path_str = target.to_string_lossy().to_string();
    println!("[export] wrote {} ({} bytes)", path_str, content.len());
    Ok(path_str)
}

#[tauri::command]
fn recording_status_cmd(recording: State<RecordingState>) -> RecordingStatus {
    match recording.active.lock() {
        Ok(guard) => {
            if let Some(active) = guard.as_ref() {
                RecordingStatus {
                    active: true,
                    session_id: Some(active.session_id.clone()),
                    output_dir: Some(active.output_dir.to_string_lossy().to_string()),
                    fps: Some(active.fps),
                    frame_ticks: active.frame_ticks.load(Ordering::SeqCst),
                    started_unix_ms: Some(active.started_unix_ms),
                }
            } else {
                RecordingStatus {
                    active: false,
                    session_id: None,
                    output_dir: None,
                    fps: None,
                    frame_ticks: 0,
                    started_unix_ms: None,
                }
            }
        }
        Err(_) => RecordingStatus {
            active: false,
            session_id: None,
            output_dir: None,
            fps: None,
            frame_ticks: 0,
            started_unix_ms: None,
        },
    }
}

#[tauri::command]
fn start_recording_session_cmd(
    recording: State<RecordingState>,
    fps: Option<u32>,
) -> Result<RecordingStatus, String> {
    let mut guard = recording
        .active
        .lock()
        .map_err(|_| "Recording lock poisoned".to_string())?;

    if guard.is_some() {
        return Err("A recording session is already active".to_string());
    }

    let session_fps = fps.unwrap_or(2).clamp(1, 8);
    let started_unix_ms = now_unix_ms()?;
    let session_id = format!("session-{}", started_unix_ms);
    let output_dir = std::env::temp_dir()
        .join("agenticify-recordings")
        .join(&session_id);
    fs::create_dir_all(&output_dir).map_err(|e| e.to_string())?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let frame_ticks = Arc::new(AtomicU64::new(0));

    let stop_flag_worker = Arc::clone(&stop_flag);
    let frame_ticks_worker = Arc::clone(&frame_ticks);
    let output_dir_worker = output_dir.clone();

    let worker = thread::spawn(move || {
        let frame_interval = Duration::from_millis((1000 / session_fps.max(1)) as u64);

        while !stop_flag_worker.load(Ordering::SeqCst) {
            let tick_started = Instant::now();
            let tick = frame_ticks_worker.fetch_add(1, Ordering::SeqCst) + 1;

            match Monitor::all() {
                Ok(monitors) => {
                    for monitor in monitors {
                        let monitor_id = match monitor.id() {
                            Ok(id) => id,
                            Err(err) => {
                                eprintln!("[recording] monitor id error: {}", err);
                                continue;
                            }
                        };

                        let image = match monitor.capture_image() {
                            Ok(img) => img,
                            Err(err) => {
                                eprintln!(
                                    "[recording] capture monitor {} error: {}",
                                    monitor_id, err
                                );
                                continue;
                            }
                        };

                        let monitor_dir = output_dir_worker.join(format!("monitor-{}", monitor_id));
                        if let Err(err) = fs::create_dir_all(&monitor_dir) {
                            eprintln!("[recording] create dir error: {}", err);
                            continue;
                        }

                        let frame_file = monitor_dir.join(format!("frame-{:06}.png", tick));
                        if let Err(err) = image.save(&frame_file) {
                            eprintln!("[recording] frame write error: {}", err);
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[recording] monitor discovery error: {}", err);
                }
            }

            let elapsed = tick_started.elapsed();
            if elapsed < frame_interval {
                thread::sleep(frame_interval - elapsed);
            }
        }
    });

    *guard = Some(ActiveRecording {
        session_id,
        output_dir,
        fps: session_fps,
        started_unix_ms,
        stop_flag,
        frame_ticks,
        worker: Some(worker),
    });

    if let Some(active) = guard.as_ref() {
        Ok(RecordingStatus {
            active: true,
            session_id: Some(active.session_id.clone()),
            output_dir: Some(active.output_dir.to_string_lossy().to_string()),
            fps: Some(active.fps),
            frame_ticks: active.frame_ticks.load(Ordering::SeqCst),
            started_unix_ms: Some(active.started_unix_ms),
        })
    } else {
        Err("Recording state unavailable after startup".to_string())
    }
}

#[tauri::command]
fn stop_recording_session_cmd(
    recording: State<RecordingState>,
) -> Result<RecordingSummary, String> {
    let mut guard = recording
        .active
        .lock()
        .map_err(|_| "Recording lock poisoned".to_string())?;

    let mut active = guard
        .take()
        .ok_or_else(|| "No active recording session".to_string())?;

    active.stop_flag.store(true, Ordering::SeqCst);
    if let Some(worker) = active.worker.take() {
        worker
            .join()
            .map_err(|_| "Recording worker thread join failed".to_string())?;
    }

    let finished_unix_ms = now_unix_ms()?;
    let summary = RecordingSummary {
        session_id: active.session_id.clone(),
        output_dir: active.output_dir.to_string_lossy().to_string(),
        fps: active.fps,
        frame_ticks: active.frame_ticks.load(Ordering::SeqCst),
        duration_ms: finished_unix_ms.saturating_sub(active.started_unix_ms),
        name: String::new(),
        instruction: String::new(),
        task_context: String::new(),
        model: String::new(),
    };

    let manifest = json!({
        "session_id": summary.session_id,
        "output_dir": summary.output_dir,
        "fps": summary.fps,
        "frame_ticks": summary.frame_ticks,
        "duration_ms": summary.duration_ms
    });

    let manifest_path = active.output_dir.join("manifest.json");
    fs::write(&manifest_path, manifest.to_string()).map_err(|e| e.to_string())?;

    Ok(summary)
}

#[tauri::command]
async fn replay_recording_session_cmd(
    app: tauri::AppHandle,
    guards: State<'_, RuntimeGuards>,
    req: SessionReplayRequest,
) -> Result<SessionReplayResult, String> {
    let (frame_path, monitor_id) = latest_frame_for_session(&req.session_id)?;

    let action = infer_click_cmd(InferClickRequest {
        png_path: frame_path.to_string_lossy().to_string(),
        instruction: req.instruction,
        model: req.model,
        step_context: None,
    })
    .await?;

    let mut clicked = false;
    if action.action == "click" {
        let monitor = monitor_by_id(monitor_id)?;
        let scale_factor = monitor.scale_factor().map_err(|e| e.to_string())? as f64;
        let width_px = ((monitor.width().map_err(|e| e.to_string())? as f64) * scale_factor).round() as u32;
        let height_px = ((monitor.height().map_err(|e| e.to_string())? as f64) * scale_factor).round() as u32;

        let req_click = ClickRequest {
                x_norm: action.x_norm,
                y_norm: action.y_norm,
                screenshot_w_px: width_px,
                screenshot_h_px: height_px,
                sent_w_px: action.sent_w,
                sent_h_px: action.sent_h,
                monitor_origin_x_pt: monitor.x().map_err(|e| e.to_string())?,
                monitor_origin_y_pt: monitor.y().map_err(|e| e.to_string())?,
                scale_factor,
                confidence: action.confidence,
            };
        perform_real_click(Some(&app), &guards, &req_click)?;
        clicked = true;
    }

    Ok(SessionReplayResult {
        session_id: req.session_id,
        monitor_id,
        frame_path: frame_path.to_string_lossy().to_string(),
        action,
        clicked,
    })
}

#[tauri::command]
fn get_runtime_state_cmd(guards: State<RuntimeGuards>) -> RuntimeState {
    RuntimeState {
        estop: guards.estop.load(Ordering::SeqCst),
        actions: guards.actions.load(Ordering::SeqCst),
        max_actions: MAX_ACTIONS_PER_RUN,
    }
}

#[tauri::command]
fn set_estop_cmd(guards: State<RuntimeGuards>, enabled: bool) -> RuntimeState {
    guards.estop.store(enabled, Ordering::SeqCst);
    if !enabled {
        guards.actions.store(0, Ordering::SeqCst);
    }

    get_runtime_state_cmd(guards)
}

#[tauri::command]
fn capture_primary_cmd(display_state: State<DisplayState>) -> Result<CaptureFrame, String> {
    // Acquire capture mutex with retry (sync commands run on main thread —
    // blocking .lock() would hang the entire UI).
    let mut guard = None;
    for _ in 0..40 {
        match capture_mutex().try_lock() {
            Ok(g) => { guard = Some(g); break; }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("Capture mutex poisoned".to_string());
            }
        }
    }
    let _capture_lock = guard.ok_or("Capture lock busy (recording in progress), try again")?;
    let started = Instant::now();
    let monitor = primary_monitor()?;

    let monitor_id = monitor.id().map_err(|e| e.to_string())?;
    let monitor_origin_x_pt = monitor.x().map_err(|e| e.to_string())?;
    let monitor_origin_y_pt = monitor.y().map_err(|e| e.to_string())?;
    let screenshot = monitor.capture_image().map_err(|e| e.to_string())?;

    let screenshot_w_px = screenshot.width();
    let screenshot_h_px = screenshot.height();

    let startup_scale = *display_state
        .primary_scale_factor
        .read()
        .map_err(|_| "Display scale lock poisoned".to_string())?;

    let xcap_scale = monitor.scale_factor().map_err(|e| e.to_string())? as f64;
    let scale_factor = if startup_scale > 0.0 {
        startup_scale
    } else if xcap_scale > 0.0 {
        xcap_scale
    } else {
        1.0
    };

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let file_name = format!("agenticify-primary-{}-{}.png", monitor_id, ts);
    let png_path: PathBuf = std::env::temp_dir().join(file_name);
    screenshot.save(&png_path).map_err(|e| e.to_string())?;

    let capture_ms = started.elapsed().as_millis();
    println!(
        "[telemetry] capture_ms={} monitor_id={} size={}x{} scale={}",
        capture_ms, monitor_id, screenshot_w_px, screenshot_h_px, scale_factor
    );

    Ok(CaptureFrame {
        monitor_id,
        monitor_origin_x_pt,
        monitor_origin_y_pt,
        screenshot_w_px,
        screenshot_h_px,
        scale_factor,
        png_path: png_path.to_string_lossy().to_string(),
        capture_ms,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
struct FrontmostApp {
    app_name: String,
    window_title: String,
}
/// Run an osascript with a timeout. Returns stdout trimmed, or empty on failure/timeout.
fn run_osascript_timeout(script: &str, timeout: Duration) -> String {
    let mut child = match Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.stdout.take()
                    .and_then(|mut out| {
                        use std::io::Read;
                        let mut buf = String::new();
                        out.read_to_string(&mut buf).ok()?;
                        Some(buf.trim().to_string())
                    })
                    .unwrap_or_default();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return String::new();
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return String::new(),
        }
    }
}

/// Query app-specific state via AppleScript for scriptable macOS apps.
/// Returns a human-readable string with the app's internal state, or empty if
/// the app isn't scriptable or the query fails.
fn query_app_state(app_name: &str) -> String {
    let script = match app_name {
        // ── Media Players ──────────────────────────────────
        "Spotify" => r#"
            tell application "Spotify"
                if it is running then
                    set pState to player state as string
                    set tName to name of current track
                    set tArtist to artist of current track
                    set tAlbum to album of current track
                    set pos to player position as integer
                    set dur to (duration of current track) / 1000 as integer
                    set vol to sound volume
                    set shuf to shuffling
                    set rep to repeating
                    return pState & " | " & tName & " by " & tArtist & " (" & tAlbum & ") | " & pos & "/" & dur & "s | vol:" & vol & " | shuffle:" & shuf & " | repeat:" & rep
                end if
            end tell
        "#,
        "Music" => r#"
            tell application "Music"
                if it is running then
                    set pState to player state as string
                    set tName to name of current track
                    set tArtist to artist of current track
                    set pos to player position as integer
                    set dur to duration of current track as integer
                    return pState & " | " & tName & " by " & tArtist & " | " & pos & "/" & dur & "s"
                end if
            end tell
        "#,
        "VLC" => r#"
            tell application "VLC"
                if it is running then
                    try
                        set pState to playing
                        set tName to name of current item
                        return "playing:" & pState & " | " & tName
                    on error
                        return "idle"
                    end try
                end if
            end tell
        "#,

        // ── Browsers ───────────────────────────────────────
        "Safari" => r#"
            tell application "Safari"
                if it is running then
                    set tabURL to URL of current tab of front window
                    set tabTitle to name of current tab of front window
                    set tabCount to count of tabs of front window
                    return tabTitle & " | " & tabURL & " | tabs:" & tabCount
                end if
            end tell
        "#,
        "Google Chrome" => r#"
            tell application "Google Chrome"
                if it is running then
                    set tabURL to URL of active tab of front window
                    set tabTitle to title of active tab of front window
                    set tabCount to count of tabs of front window
                    return tabTitle & " | " & tabURL & " | tabs:" & tabCount
                end if
            end tell
        "#,
        "Arc" => r#"
            tell application "Arc"
                if it is running then
                    try
                        set tabURL to URL of active tab of front window
                        set tabTitle to title of active tab of front window
                        return tabTitle & " | " & tabURL
                    on error
                        return ""
                    end try
                end if
            end tell
        "#,
        "Firefox" => r#"
            tell application "System Events"
                tell process "Firefox"
                    try
                        set winTitle to name of front window
                        return winTitle
                    on error
                        return ""
                    end try
                end tell
            end tell
        "#,

        // ── Productivity ───────────────────────────────────
        "Preview" => r#"
            tell application "Preview"
                if it is running then
                    set docName to name of front document
                    return docName
                end if
            end tell
        "#,
        "TextEdit" => r#"
            tell application "TextEdit"
                if it is running then
                    set docName to name of front document
                    return docName
                end if
            end tell
        "#,
        "Notes" => r#"
            tell application "Notes"
                if it is running then
                    try
                        set noteName to name of first note of default account
                        return "Latest note: " & noteName
                    on error
                        return ""
                    end try
                end if
            end tell
        "#,
        "Reminders" | "Calendar" | "Mail" => {
            // These apps are scriptable but state queries are slow/complex
            // Just report they're running — the window title is usually enough
            return String::new();
        },
        "Messages" => r#"
            tell application "System Events"
                tell process "Messages"
                    try
                        return name of front window
                    on error
                        return ""
                    end try
                end tell
            end tell
        "#,
        "Slack" | "Discord" => r#"
            tell application "System Events"
                tell process "{APP}"
                    try
                        return name of front window
                    on error
                        return ""
                    end try
                end tell
            end tell
        "#,

        // ── Finder ─────────────────────────────────────────
        "Finder" => r#"
            tell application "Finder"
                try
                    set folderPath to POSIX path of (target of front Finder window as alias)
                    set itemCount to count of items of front Finder window
                    set sel to count of (selection as alias list)
                    return folderPath & " | items:" & itemCount & " | selected:" & sel
                on error
                    return "Desktop"
                end try
            end tell
        "#,

        // ── Terminal ───────────────────────────────────────
        "Terminal" => r#"
            tell application "Terminal"
                if it is running then
                    set tabProcs to processes of front tab of front window
                    set AppleScript's text item delimiters to ", "
                    return tabProcs as text
                end if
            end tell
        "#,

        // ── Unknown apps — try generic focused element ────
        _ => {
            let generic_script = format!(
                r#"tell application "System Events"
                    tell process "{}"
                        try
                            set fe to focused UI element
                            set feRole to role of fe
                            set feVal to value of fe
                            return feRole & ": " & feVal
                        on error
                            return ""
                        end try
                    end tell
                end tell"#,
                app_name
            );
            return run_osascript_timeout(&generic_script, Duration::from_secs(2));
        }
    };

    // For Slack/Discord, substitute {APP} placeholder
    let final_script = script.replace("{APP}", app_name);

    run_osascript_timeout(&final_script, Duration::from_secs(2))
}

/// Gather rich OS context to inject into the agent's prompt.
/// Returns a formatted string with frontmost app, window title, and running GUI apps.
fn gather_os_context() -> String {
    let mut parts: Vec<String> = Vec::new();

    // 1. Frontmost application + window title
    let frontmost_script = r#"
        tell application "System Events"
            set frontApp to name of first application process whose frontmost is true
            try
                set winTitle to name of front window of (first application process whose frontmost is true)
            on error
                set winTitle to ""
            end try
            return frontApp & "|||" & winTitle
        end tell
    "#;
    if let Ok(output) = Command::new("osascript").arg("-e").arg(frontmost_script).output() {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let segs: Vec<&str> = raw.splitn(2, "|||").collect();
        let app_name = segs.first().unwrap_or(&"").to_string();
        let win_title = segs.get(1).unwrap_or(&"").to_string();
        parts.push(format!("Frontmost app: {}\nWindow title: {}", app_name, win_title));
    }

    // 2. List of running GUI applications (visible in Dock)
    let running_apps_script = r#"
        tell application "System Events"
            set appList to name of every application process whose background only is false
            set AppleScript's text item delimiters to ", "
            return appList as text
        end tell
    "#;
    if let Ok(output) = Command::new("osascript").arg("-e").arg(running_apps_script).output() {
        let apps = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !apps.is_empty() {
            parts.push(format!("Running GUI apps: {}", apps));
        }
    }

    // 3. Number of open windows in the frontmost app
    let window_count_script = r#"
        tell application "System Events"
            set frontProc to first application process whose frontmost is true
            set winCount to count of windows of frontProc
            set winNames to {}
            repeat with w in windows of frontProc
                try
                    copy name of w to end of winNames
                end try
            end repeat
            set AppleScript's text item delimiters to " | "
            return (winCount as text) & "|||" & (winNames as text)
        end tell
    "#;
    if let Ok(output) = Command::new("osascript").arg("-e").arg(window_count_script).output() {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let segs: Vec<&str> = raw.splitn(2, "|||").collect();
        let count = segs.first().unwrap_or(&"0");
        let names = segs.get(1).unwrap_or(&"").to_string();
        if !names.is_empty() {
            parts.push(format!("Open windows in frontmost app ({}): {}", count, names));
        }
    }

    // 4. App-specific state (scriptable apps)
    // Extract the frontmost app name from our earlier query
    let frontmost_app_name = parts.first()
        .and_then(|p| p.strip_prefix("Frontmost app: "))
        .and_then(|s| s.split('\n').next())
        .unwrap_or("")
        .to_string();

    if !frontmost_app_name.is_empty() {
        let app_state = query_app_state(&frontmost_app_name);
        if !app_state.is_empty() {
            parts.push(format!("App state ({}): {}", frontmost_app_name, app_state));
        }
    }

    // 5. Current date/time
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parts.push(format!("System time (unix): {}", now));

    if parts.is_empty() {
        String::new()
    } else {
        format!("\n\n═══ CURRENT OS STATE ═══\n{}", parts.join("\n"))
    }
}

/// Extract just the frontmost app name (for shortcuts lookup).
fn extract_frontmost_app_name() -> String {
    let script = r#"
        tell application "System Events"
            set frontApp to name of first application process whose frontmost is true
            return frontApp
        end tell
    "#;
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ── Shortcuts Tauri Commands ───────────────────────────

#[derive(Debug, Clone, Serialize)]
struct AppShortcuts {
    app_name: String,
    shortcuts: String,
    from_cache: bool,
}

#[tauri::command]
async fn get_app_shortcuts_cmd(app_name: String) -> Result<AppShortcuts, String> {
    let api_key = resolve_primary_api_key();
    let api_base = resolve_primary_api_base();

    let was_cached = shortcuts::get_cached_global(&app_name).is_some();
    let text = shortcuts::get_or_fetch_global(&app_name, api_key.trim(), &api_base).await;

    Ok(AppShortcuts {
        app_name,
        shortcuts: text,
        from_cache: was_cached,
    })
}

#[tauri::command]
fn clear_shortcuts_cache_cmd() {
    shortcuts::clear_global_cache();
    println!("[shortcuts] global cache cleared");
}

#[tauri::command]
fn get_frontmost_app_cmd() -> Result<FrontmostApp, String> {
    let script = r#"
        tell application "System Events"
            set frontApp to name of first application process whose frontmost is true
            try
                set winTitle to name of front window of (first application process whose frontmost is true)
            on error
                set winTitle to ""
            end try
            return frontApp & "|||" & winTitle
        end tell
    "#;

    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("osascript failed: {}", e))?;

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts: Vec<&str> = raw.splitn(2, "|||").collect();

    Ok(FrontmostApp {
        app_name: parts.first().unwrap_or(&"").to_string(),
        window_title: parts.get(1).unwrap_or(&"").to_string(),
    })
}

#[tauri::command]
async fn infer_click_cmd(req: InferClickRequest) -> Result<VisionAction, String> {
    use openrouter_rs::{
        OpenRouterClient,
        api::chat::{ChatCompletionRequest, Message, Content, ContentPart},
        types::Role,
    };

    let started = Instant::now();
    let (image_bytes, orig_w, orig_h, sent_w, sent_h) = load_infer_image_bytes(&req.png_path)?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
    let image_url = format!("data:image/png;base64,{}", b64);
    if (orig_w, orig_h) != (sent_w, sent_h) {
        println!(
            "[telemetry] infer_image downscaled {}x{} -> {}x{} (max_dim={})",
            orig_w, orig_h, sent_w, sent_h, infer_max_dim()
        );
    } else {
        println!(
            "[telemetry] infer_image using original {}x{} (max_dim={})",
            orig_w, orig_h, infer_max_dim()
        );
    }

    let system_prompt = "You are a desktop automation agent running on macOS. Each step you receive a fresh screenshot of the entire screen and choose ONE action. You have up to 30 steps — USE THEM ALL if needed. Do NOT stop early.\n\n\
═══ AGENTICIFY HUD OVERLAY (CRITICAL — READ FIRST) ═══\n\
There is a small transparent overlay bar at the TOP CENTER of the screen. It belongs to Agenticify (YOUR control software) and contains buttons like 'Run Loop', 'Stop', status indicators, and an activity feed showing messages like 'Capturing screen...', 'Model is thinking...', 'CLICK | ...', 'DONE | ...'.\n\
⚠ THIS OVERLAY IS NOT PART OF THE DESKTOP. IT IS YOUR OWN HUD.\n\
  - NEVER click on it (not the buttons, not the text, not anything in it)\n\
  - NEVER reference it as evidence of task progress or completion\n\
  - NEVER treat messages like 'Model is thinking...' or 'CLICK | ...' as indicators that the task is done\n\
  - If you see a blue-glowing border around the screen edges, that is also YOUR overlay — ignore it\n\
  - Pretend the HUD does not exist. Focus ONLY on the actual desktop content beneath it.\n\n\
═══ MANDATORY PRE-ACTION CHECKLIST (DO THIS EVERY SINGLE STEP) ═══\n\
You MUST follow this checklist IN ORDER before choosing ANY action:\n\n\
STEP 1 — READ APP STATE: Look at the CURRENT OS STATE section in the user message. It tells you:\n\
  - Frontmost app (which app is active right now)\n\
  - Window title\n\
  - App state (e.g. Spotify: playing or paused, browser: current URL)\n\
  This is GROUND TRUTH about what the system is doing. Trust it over the screenshot.\n\n\
STEP 2 — IS THE TASK ALREADY DONE? Based on the app state:\n\
  - If the goal was to pause Spotify and app state says paused → return DONE immediately\n\
  - If the goal was to open Chrome and frontmost app is Google Chrome → return DONE immediately\n\
  - If the app state confirms the goal is met → STOP. Do not take another action.\n\n\
STEP 3 — IS THE RIGHT APP IN FRONT? If the frontmost app is NOT the target app:\n\
  - Do NOT click, hotkey, or type — you will act on the WRONG app\n\
  - Your ONLY valid action is: open Spotlight (Cmd+Space) then type app name then press Return\n\
  - Spotlight sequence must span multiple steps: Cmd+Space, then type, then Return\n\
  ⚠ AFTER Cmd+Space, your VERY NEXT action MUST be type. Do NOT click ANYTHING.\n\
  Clicking anywhere dismisses Spotlight. Do NOT click Dock icons or results.\n\
  Do NOT use Cmd+Tab.\n\n\
STEP 4 — DID MY LAST ACTION WORK? Check what changed:\n\
  - Compare the current app state and screenshot to what you did last step\n\
  - If your last action had NO EFFECT, do NOT repeat it — try a DIFFERENT approach\n\
  - NEVER repeat the same action twice in a row\n\n\
STEP 5 — CHOOSE YOUR NEXT ACTION based on all of the above.\n\n\
═══ WHEN TO STOP (STRICT RULES) ═══\n\
Return action=none ONLY when ALL of these are true:\n\
  1. The TARGET APP (not Agenticify) is in the foreground\n\
  2. The expected result is visible in the TARGET APP's content area\n\
  3. You have verified the result by examining the actual app UI, NOT the Agenticify HUD overlay\n\
You are NOT done if:\n\
  - You see 'Model is thinking...' or 'Capturing screen...' — that is YOUR HUD, not task completion\n\
  - You just opened an app but haven't performed the actual task yet\n\
  - A page is still loading (spinner visible, content not rendered)\n\
  - You typed a URL but haven't pressed Return\n\
  - You opened Spotlight but haven't typed or pressed Return\n\
  - The wrong page/app is showing\n\
  - A dialog, modal, or error is blocking the view\n\
  - You completed step 1 of a multi-step task but not the remaining steps\n\
ASK YOURSELF: \"Ignoring the Agenticify HUD, if someone looked at the TARGET APP, would they say the task is done?\" If no, KEEP GOING.\n\
When uncertain, CONTINUE — stopping early is worse than taking extra steps.\n\n\
═══ ACTIONS (return exactly one as JSON) ═══\n\n\
1. CLICK: {\"action\":\"click\", \"x_norm\":<px_x>, \"y_norm\":<px_y>, \"confidence\":0-1, \"reason\":\"...\"}\n\
   Pixel coords in the screenshot. (0,0) = top-left. Aim for the exact CENTER of the target element.\n\n\
2. HOTKEY: {\"action\":\"hotkey\", \"keys\":[{\"key\":\"Meta\",\"direction\":\"press\"},{\"key\":\"Space\",\"direction\":\"click\"},{\"key\":\"Meta\",\"direction\":\"release\"}], \"confidence\":0-1, \"reason\":\"...\"}\n\n\
3. TYPE: {\"action\":\"type\", \"text\":\"Chrome\", \"confidence\":0-1, \"reason\":\"...\"}\n\
   Types into whatever field currently has focus. Do NOT click before typing if a field is already focused (e.g. Spotlight).\n\n\
4. SHELL: {\"action\":\"shell\", \"command\":\"ls -la ~/Desktop\", \"confidence\":0-1, \"reason\":\"...\"}\n\
   ⚠ Do NOT use shell/CLI unless the user's task EXPLICITLY requires it (e.g. 'run a command', 'check git status', 'install X'). For all other tasks, use GUI actions (click, hotkey, type).\n\n\
5. DONE: {\"action\":\"none\", \"x_norm\":0, \"y_norm\":0, \"confidence\":0, \"reason\":\"...\"}\n\
   Use ONLY when the task goal is fully achieved and visually confirmed in the target app.\n\n\
═══ CLICK ACCURACY ═══\n\
  - Aim for the exact CENTER of the target element\n\
  - If a click doesn't work, try a keyboard shortcut instead (preferred)\n\
  - PREFER arrow keys + Return for menus, lists, dropdowns, search results\n\
  - Clicking is the LAST RESORT — use keyboard shortcuts whenever possible\n\n\
═══ TEXT EDITING ═══\n\
  - Clear field: Cmd+A then type replacement (overwrites selection)\n\
  - Delete: Backspace key\n\
  - Old/wrong text: ALWAYS Cmd+A then retype\n\n\
═══ SHELL VS GUI ═══\n\
  Shell: ONLY when the user explicitly asks for CLI/terminal actions (e.g. files, git, packages, scripts)\n\
  GUI: EVERYTHING ELSE — app interactions, media control, web browsing, clicking buttons, navigating menus\n\
  ⚠ DEFAULT TO GUI. Do not use shell commands to control apps (e.g. osascript) unless the user specifically requests CLI.\n\n\
═══ GOAL VERIFICATION ═══\n\
  - The browser address bar is ONLY at the very top, next to back/forward/reload buttons\n\
  - A text field inside a page is NOT the address bar even if it shows a URL\n\
  - Before stopping: What app am I in? What content is visible? Does it match the goal?\n\
  - IGNORE the Agenticify overlay when verifying — look at the actual app underneath\n\n\
═══ HOTKEYS ═══\n\
Browser: Cmd+L address bar | Cmd+T new tab | Cmd+W close tab | Cmd+R reload | Cmd+[ back | Cmd+] forward\n\
macOS: Cmd+Space Spotlight | Cmd+Q quit | Cmd+A select all | Cmd+C copy | Cmd+V paste | Cmd+S save\n\
Keys: Meta/Cmd, Tab, Space, Return/Enter, Escape, Shift, Control/Ctrl, Alt/Option, Up, Down, Left, Right, Backspace, Delete, Home, End, PageUp, PageDown, F1-F12, or any character.\n\
Directions: press (hold), release (let go), click (tap, default).\n\
ALWAYS prefer: hotkeys > app shortcuts > clicking. Only use shell if explicitly asked.\n\
App-specific shortcuts (if available) will be provided in the user message below.\n\
Return ONLY valid JSON.";

    // Gather OS context (frontmost app, running apps, window info)
    let os_context = gather_os_context();
    let frontmost_app = extract_frontmost_app_name();
    println!("[telemetry] os_context: {}", os_context.replace('\n', " | "));

    // Fetch app-specific shortcuts (cached after first lookup)
    let shortcuts_text = if !frontmost_app.is_empty() {
        let api_key_for_shortcuts = resolve_primary_api_key();
        let api_base_for_shortcuts = resolve_primary_api_base();
        shortcuts::get_or_fetch_global(
            &frontmost_app,
            api_key_for_shortcuts.trim(),
            &api_base_for_shortcuts,
        ).await
    } else {
        String::new()
    };

    let mut user_prompt = format!(
        "Task: {}\nCoordinate system: PIXEL coordinates. The screenshot image is {}x{} pixels wide and tall. (0,0) is the top-left corner. ({},{}) is the bottom-right corner. Return x_norm as the pixel column (0 to {}) and y_norm as the pixel row (0 to {}).{}",
        req.instruction,
        sent_w, sent_h,
        sent_w.saturating_sub(1), sent_h.saturating_sub(1),
        sent_w.saturating_sub(1), sent_h.saturating_sub(1),
        os_context
    );

    // Inject app-specific shortcuts if available
    if !shortcuts_text.is_empty() {
        user_prompt.push_str(&format!(
            "\n\n═══ APP-SPECIFIC SHORTCUTS ({}) ═══\n{}\nUse these shortcuts FIRST. Only click if no shortcut exists for the action.",
            frontmost_app, shortcuts_text
        ));
    }

    user_prompt.push_str("\n\nLook at the screenshot carefully. What is the NEXT single action to make progress toward the goal?");

    if let Some(ctx) = &req.step_context {
        user_prompt.push_str(&format!("\n\nPrevious actions taken:\n{}", ctx));
    }

    let api_key = resolve_primary_api_key();
    if api_key.trim().is_empty() {
        return Err("API key is missing. Set OPENROUTER_API_KEY (or MISTRAL_API_KEY).".to_string());
    }

    let model = DEFAULT_MISTRAL_MODEL.to_string();

    let client = OpenRouterClient::builder()
        .api_key(api_key.trim())
        .http_referer("https://agenticify.local")
        .x_title("Agenticify")
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
        .temperature(0.0)
        .build()
        .map_err(|e| format!("Request build error: {}", e))?;

    let response = client
        .send_chat_completion(&request)
        .await
        .map_err(|e| format!("OpenRouter API error: {}", e))?;

    let content = response.choices.first()
        .and_then(|c| c.content())
        .ok_or_else(|| "No content in response choices".to_string())?
        .to_string();

    let model_ms = started.elapsed().as_millis();
    let parsed = parse_vision_action(&content, model_ms, sent_w, sent_h)?;

    println!(
        "[telemetry] model_ms={} action={} confidence={:.3} provider=openrouter",
        model_ms, parsed.action, parsed.confidence
    );

    Ok(parsed)
}

#[tauri::command]
fn execute_real_click_cmd(
    app: tauri::AppHandle,
    guards: State<RuntimeGuards>,
    req: ClickRequest,
) -> Result<(), String> {
    perform_real_click(Some(&app), &guards, &req)
}

fn parse_key_name(name: &str) -> Result<Key, String> {
    match name {
        "Meta" | "Command" | "Cmd" => Ok(Key::Meta),
        "Tab" => Ok(Key::Tab),
        "Space" => Ok(Key::Space),
        "Return" | "Enter" => Ok(Key::Return),
        "Escape" | "Esc" => Ok(Key::Escape),
        "Shift" => Ok(Key::Shift),
        "Control" | "Ctrl" => Ok(Key::Control),
        "Alt" | "Option" => Ok(Key::Alt),
        "UpArrow" | "Up" => Ok(Key::UpArrow),
        "DownArrow" | "Down" => Ok(Key::DownArrow),
        "LeftArrow" | "Left" => Ok(Key::LeftArrow),
        "RightArrow" | "Right" => Ok(Key::RightArrow),
        "Backspace" => Ok(Key::Backspace),
        "Delete" => Ok(Key::Delete),
        "Home" => Ok(Key::Home),
        "End" => Ok(Key::End),
        "PageUp" => Ok(Key::PageUp),
        "PageDown" => Ok(Key::PageDown),
        "CapsLock" => Ok(Key::CapsLock),
        "F1" => Ok(Key::F1),
        "F2" => Ok(Key::F2),
        "F3" => Ok(Key::F3),
        "F4" => Ok(Key::F4),
        "F5" => Ok(Key::F5),
        "F6" => Ok(Key::F6),
        "F7" => Ok(Key::F7),
        "F8" => Ok(Key::F8),
        "F9" => Ok(Key::F9),
        "F10" => Ok(Key::F10),
        "F11" => Ok(Key::F11),
        "F12" => Ok(Key::F12),
        s if s.chars().count() == 1 => Ok(Key::Unicode(s.chars().next().unwrap())),
        other => Err(format!("Unknown key name: '{}'", other)),
    }
}

fn parse_direction(dir: Option<&str>) -> Result<Direction, String> {
    match dir {
        None | Some("click") | Some("Click") => Ok(Direction::Click),
        Some("press") | Some("Press") => Ok(Direction::Press),
        Some("release") | Some("Release") => Ok(Direction::Release),
        Some(other) => Err(format!(
            "Unknown direction '{}'. Use 'press', 'release', or 'click'.",
            other
        )),
    }
}

#[tauri::command]
fn press_keys_cmd(guards: State<RuntimeGuards>, req: PressKeysRequest) -> Result<(), String> {
    if guards.estop.load(Ordering::SeqCst) {
        return Err("Emergency stop active".to_string());
    }

    let delay = Duration::from_millis(req.delay_ms.unwrap_or(30));
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;

    for (i, combo) in req.keys.iter().enumerate() {
        let key = parse_key_name(&combo.key)?;
        let direction = parse_direction(combo.direction.as_deref())?;

        enigo
            .key(key, direction)
            .map_err(|e| format!("key '{}' failed: {}", combo.key, e))?;

        if i + 1 < req.keys.len() {
            thread::sleep(delay);
        }
    }

    let key_desc: Vec<String> = req.keys.iter().map(|k| {
        let dir = k.direction.as_deref().unwrap_or("click");
        format!("{}:{}", k.key, dir)
    }).collect();
    println!(
        "[keyboard] executed {} key action(s): {}",
        req.keys.len(),
        key_desc.join(" → ")
    );
    Ok(())
}

#[tauri::command]
fn type_text_cmd(guards: State<RuntimeGuards>, text: String) -> Result<(), String> {
    if guards.estop.load(Ordering::SeqCst) {
        return Err("Emergency stop active".to_string());
    }

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    enigo.text(&text).map_err(|e| e.to_string())?;

    println!("[keyboard] typed {} char(s)", text.len());
    Ok(())
}

// ── WhiteCircle Guardrail ──────────────────────────────────────────────────────

const MAX_SHELL_OUTPUT_BYTES: usize = 4096;
const SHELL_TIMEOUT_SECS: u64 = 10;

/// Check a string (command or output) through WhiteCircle's guardrail API.
/// Returns Ok(true) if safe / no key configured, Ok(false) if blocked.
async fn whitecircle_guard(input: &str, guard_type: &str) -> Result<bool, String> {
    let api_key = std::env::var("WHITECIRCLE_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());

    let api_key = match api_key {
        Some(k) => k,
        None => {
            println!("[whitecircle] no API key configured, skipping {} guard", guard_type);
            return Ok(true); // pass-through when not configured
        }
    };

    let base = std::env::var("WHITECIRCLE_API_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://eu.whitecircle.ai/api/v1".to_string());

    let strict = std::env::var("WHITECIRCLE_STRICT")
        .ok()
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let url = format!("{}/guard", base.trim_end_matches('/'));

    let payload = json!({
        "type": guard_type,
        "content": input,
        "source": "agenticify-shell"
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("whitecircle client error: {}", e))?;

    match client
        .post(&url)
        .bearer_auth(api_key.trim())
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(json!({}));
                let allowed = body.get("allowed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true); // default to allowed if response format unexpected
                let reason = body.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no reason");
                println!(
                    "[whitecircle] {} guard: allowed={} reason={}",
                    guard_type, allowed, reason
                );
                Ok(allowed)
            } else {
                let body = resp.text().await.unwrap_or_default();
                println!(
                    "[whitecircle] {} guard returned HTTP {}: {}",
                    guard_type, status, body.chars().take(200).collect::<String>()
                );
                // Non-success HTTP: if strict, block; otherwise pass
                if strict {
                    Err(format!("WhiteCircle guard error (HTTP {})", status))
                } else {
                    Ok(true)
                }
            }
        }
        Err(err) => {
            println!("[whitecircle] {} guard network error: {}", guard_type, err);
            if strict {
                Err(format!("WhiteCircle guard unreachable: {}", err))
            } else {
                Ok(true) // graceful degradation
            }
        }
    }
}

#[tauri::command]
async fn run_shell_cmd(guards: State<'_, RuntimeGuards>, command: String) -> Result<String, String> {
    if guards.estop.load(Ordering::SeqCst) {
        return Err("Emergency stop active".to_string());
    }

    let n = guards.actions.fetch_add(1, Ordering::SeqCst);
    if n >= MAX_ACTIONS_PER_RUN {
        guards.estop.store(true, Ordering::SeqCst);
        return Err("Max actions reached; E-STOP engaged".to_string());
    }

    // ── WhiteCircle input guard ──
    let input_safe = whitecircle_guard(&command, "input").await?;
    if !input_safe {
        return Err(format!(
            "Command blocked by WhiteCircle guardrail: {}",
            command.chars().take(100).collect::<String>()
        ));
    }

    println!("[shell] executing: {}", command);
    let started = Instant::now();

    // Spawn with timeout
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn shell: {}", e))?;

    let output = tokio_timeout(child).await?;

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[stderr] ");
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    // Truncate to avoid blowing up model context
    if combined.len() > MAX_SHELL_OUTPUT_BYTES {
        combined.truncate(MAX_SHELL_OUTPUT_BYTES);
        combined.push_str("\n... (output truncated)");
    }

    let exit_code = output.status.code().unwrap_or(-1);
    let elapsed_ms = started.elapsed().as_millis();
    println!(
        "[shell] exit={} ms={} output_bytes={} action_count={}",
        exit_code, elapsed_ms, combined.len(), n + 1
    );

    // ── WhiteCircle output guard ──
    let output_safe = whitecircle_guard(&combined, "output").await?;
    if !output_safe {
        return Ok("[output redacted by WhiteCircle guardrail]".to_string());
    }

    Ok(combined)
}

/// Wait for a child process with a timeout; kill it if it exceeds SHELL_TIMEOUT_SECS.
fn tokio_timeout(mut child: std::process::Child) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::process::Output, String>> + Send>> {
    Box::pin(async move {
        let deadline = Instant::now() + Duration::from_secs(SHELL_TIMEOUT_SECS);
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    return child.wait_with_output().map_err(|e| format!("shell output error: {}", e));
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(format!("Shell command timed out after {}s", SHELL_TIMEOUT_SECS));
                    }
                    // Yield briefly
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("Error waiting for shell: {}", e)),
            }
        }
    })
}

fn init_display_scale(display_state: &DisplayState) {
    let from_nsscreen = primary_backing_scale_factor().unwrap_or(0.0);
    let from_xcap = primary_monitor()
        .ok()
        .and_then(|m| m.scale_factor().ok())
        .map(|v| v as f64)
        .unwrap_or(0.0);

    let resolved = if from_nsscreen > 0.0 {
        from_nsscreen
    } else if from_xcap > 0.0 {
        from_xcap
    } else {
        1.0
    };

    if let Ok(mut lock) = display_state.primary_scale_factor.write() {
        *lock = resolved;
    }

    println!(
        "[startup] primary scale factor resolved: nsscreen={} xcap={} final={}",
        from_nsscreen, from_xcap, resolved
    );
}

fn main() {
    // Try loading .env from multiple locations (first match wins):
    // 1. CWD/.env (works during `cargo run` / `tauri dev`)
    // 2. ~/.agenticify.env (works for production .app bundles)
    let home_env = dirs::home_dir().map(|h| h.join(".agenticify.env"));
    let loaded = dotenvy::dotenv().ok().map(|p| p.display().to_string())
        .or_else(|| {
            home_env.as_ref().and_then(|p| {
                dotenvy::from_path(p).ok().map(|_| p.display().to_string())
            })
        });
    match loaded {
        Some(path) => println!("[startup] loaded environment from {}", path),
        None => println!(
            "[startup] no .env found (checked CWD and {}). Set env vars directly or create ~/.agenticify.env",
            home_env.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        ),
    }

    tauri::Builder::default()
        .manage(RuntimeGuards::default())
        .manage(DisplayState::default())
        .manage(RecordingState::default())
        .manage(recording::SessionRecordingState::default())
        .manage(shortcuts::ShortcutsCache::default())
        .setup(|app| {
            let display_state = app.state::<DisplayState>();
            init_display_scale(&display_state);

            #[cfg(target_os = "macos")]
            {
                let state = check_permissions();
                if !state.screen_recording || !state.accessibility {
                    let _ = request_permissions();
                }
            }

            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_shortcuts(["cmd+shift+escape", "cmd+shift+enter"])?
                    .with_handler(|app_handle, shortcut, event| {
                        if event.state == ShortcutState::Pressed
                            && shortcut.matches(Modifiers::SUPER | Modifiers::SHIFT, Code::Escape)
                        {
                            let guards = app_handle.state::<RuntimeGuards>();
                            guards.estop.store(true, Ordering::SeqCst);
                            println!("[safety] global E-STOP activated via Cmd+Shift+Esc");
                            return;
                        }

                        if event.state == ShortcutState::Pressed
                            && shortcut.matches(Modifiers::SUPER | Modifiers::SHIFT, Code::Enter)
                        {
                            if let Some(main) = app_handle.get_webview_window("main") {
                                let _ = main.show();
                                let _ = main.unminimize();
                                let _ = main.set_focus();
                                println!("[window] restored main window via Cmd+Shift+Enter");
                            } else {
                                println!("[window] could not restore main window (label=main not found)");
                            }
                        }
                    })
                    .build(),
            )?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_permissions_cmd,
            request_permissions_cmd,
            env_status_cmd,
            validate_mistral_api_key_cmd,
            recordings_root_cmd,
            list_recording_sessions_cmd,
            open_path_cmd,
            recording_status_cmd,
            start_recording_session_cmd,
            stop_recording_session_cmd,
            replay_recording_session_cmd,
            get_runtime_state_cmd,
            set_estop_cmd,
            capture_primary_cmd,
            infer_click_cmd,
            execute_real_click_cmd,
            press_keys_cmd,
            type_text_cmd,
            run_shell_cmd,
            get_frontmost_app_cmd,
            recording::start_session_cmd,
            recording::stop_session_cmd,
            recording::session_status_cmd,
            recording::list_sessions_cmd,
            recording::load_session_cmd,
            recording::delete_session_cmd,
            recording::save_activity_log_cmd,
            recording::load_activity_log_cmd,
            get_app_shortcuts_cmd,
            clear_shortcuts_cache_cmd,
            export_markdown_cmd,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri app");
}

#[cfg(test)]
mod tests {
    use super::{normalized_to_global_points, ClickRequest};

    #[test]
    fn maps_normalized_to_points_with_scale() {
        let req = ClickRequest {
            x_norm: 500.0,
            y_norm: 500.0,
            screenshot_w_px: 3000,
            screenshot_h_px: 2000,
            monitor_origin_x_pt: 0,
            monitor_origin_y_pt: 0,
            scale_factor: 2.0,
            confidence: 1.0,
        };

        let (x, y) = normalized_to_global_points(&req);
        assert!((x - 750).abs() <= 1);
        assert!((y - 500).abs() <= 1);
    }
}
