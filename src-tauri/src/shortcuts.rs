use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

// ── App Shortcuts Cache (LLM-Powered) ──────────────────
//
// Dynamically fetches keyboard shortcuts for the currently active
// macOS application using the same OpenRouter/Mistral API the agent
// already uses. Results are cached per app name for the session
// lifetime so we only pay the LLM cost once per unique app.

/// Tauri-managed state (registered in main but not strictly needed for
/// the global helpers below — kept for potential future Tauri-state usage).
#[derive(Default)]
#[allow(dead_code)]
pub struct ShortcutsCache {
    pub cache: Mutex<HashMap<String, String>>,
}

/// Global static cache so async `infer_click_cmd` can use it without
/// needing Tauri `State<>`.
fn global_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Apps that are system-level or have no meaningful shortcuts.
const SKIP_APPS: &[&str] = &[
    "loginwindow",
    "systemprefspane",
    "systemuiserver",
    "universalcontrol",
    "dock",
    "screencaptureui",
    "agenticify",
    "controlcenter",
    "notificationcenter",
    "wallpaper",
    "windowmanager",
    "tauri",
];

/// Returns `true` if this app should be skipped (system-level, no shortcuts).
pub fn should_skip(app_name: &str) -> bool {
    let lower = app_name.trim().to_lowercase();
    if lower.is_empty() {
        return true;
    }
    SKIP_APPS.iter().any(|s| lower == *s)
}

/// Check the global cache for existing shortcuts.
pub fn get_cached_global(app_name: &str) -> Option<String> {
    let key = app_name.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    let lock = global_cache().lock().ok()?;
    lock.get(&key).cloned()
}

/// Store shortcuts in the global cache.
fn set_cached_global(app_name: &str, shortcuts: String) {
    let key = app_name.trim().to_lowercase();
    if key.is_empty() {
        return;
    }
    if let Ok(mut lock) = global_cache().lock() {
        lock.insert(key, shortcuts);
    }
}

/// Clear the entire global shortcuts cache.
pub fn clear_global_cache() {
    if let Ok(mut lock) = global_cache().lock() {
        lock.clear();
    }
}

/// Fetch shortcuts for `app_name` via an OpenRouter chat completion.
async fn fetch_via_llm(app_name: &str, api_key: &str, api_base: &str) -> Result<String, String> {
    use openrouter_rs::{
        OpenRouterClient,
        api::chat::{ChatCompletionRequest, Message},
        types::Role,
    };

    let prompt = format!(
        "List the 20 most useful keyboard shortcuts for \"{}\" on macOS.\n\
         Format each as: Shortcut - Description\n\
         One per line. No intro, no outro, no numbering. Just the shortcut lines.\n\
         Include media keys, navigation, and editing shortcuts if applicable.\n\
         Use Cmd/Opt/Ctrl/Shift notation.",
        app_name
    );

    let client = if api_base.contains("openrouter.ai") {
        OpenRouterClient::builder()
            .api_key(api_key)
            .http_referer("https://agenticify.local")
            .x_title("Agenticify-Shortcuts")
            .build()
            .map_err(|e| format!("Shortcuts client error: {}", e))?
    } else {
        OpenRouterClient::builder()
            .api_key(api_key)
            .base_url(api_base)
            .http_referer("https://agenticify.local")
            .x_title("Agenticify-Shortcuts")
            .build()
            .map_err(|e| format!("Shortcuts client error: {}", e))?
    };

    let request = ChatCompletionRequest::builder()
        .model(super::DEFAULT_MISTRAL_MODEL)
        .messages(vec![
            Message::new(Role::System, "You are a keyboard shortcut reference. Respond ONLY with shortcut lines, nothing else."),
            Message::new(Role::User, prompt.as_str()),
        ])
        .temperature(0.0)
        .max_tokens(600_u32)
        .build()
        .map_err(|e| format!("Shortcuts request build error: {}", e))?;

    let response = client
        .send_chat_completion(&request)
        .await
        .map_err(|e| format!("Shortcuts API error: {}", e))?;

    let content = response.choices.first()
        .and_then(|c| c.content())
        .ok_or_else(|| "No content in shortcuts response".to_string())?
        .to_string();

    Ok(content.trim().to_string())
}

/// High-level helper using the global static cache: get shortcuts for
/// `app_name`, using cache first, then LLM fetch. Returns the shortcuts
/// text or an empty string on failure.
pub async fn get_or_fetch_global(
    app_name: &str,
    api_key: &str,
    api_base: &str,
) -> String {
    if should_skip(app_name) {
        return String::new();
    }

    // Check cache first
    if let Some(cached) = get_cached_global(app_name) {
        println!("[shortcuts] cache hit for '{}'", app_name);
        return cached;
    }

    // Fetch from LLM
    println!("[shortcuts] fetching shortcuts for '{}' via LLM...", app_name);
    match fetch_via_llm(app_name, api_key, api_base).await {
        Ok(shortcuts) => {
            println!("[shortcuts] fetched {} chars for '{}'", shortcuts.len(), app_name);
            set_cached_global(app_name, shortcuts.clone());
            shortcuts
        }
        Err(err) => {
            println!("[shortcuts] fetch failed for '{}': {}", app_name, err);
            // Cache the failure so we don't keep retrying
            set_cached_global(app_name, String::new());
            String::new()
        }
    }
}
