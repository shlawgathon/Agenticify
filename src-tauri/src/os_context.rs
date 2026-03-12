use crate::models::{FrontmostApp, OsContextSnapshot};
use std::{
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .stdout
                    .take()
                    .and_then(|mut out| {
                        use std::io::Read;
                        let mut buf = String::new();
                        out.read_to_string(&mut buf).ok()?;
                        Some(buf.trim().to_string())
                    })
                    .unwrap_or_default();
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return String::new();
                }
                std::thread::sleep(Duration::from_millis(50));
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
        "Spotify" => {
            r#"
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
        "#
        }
        "Music" => {
            r#"
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
        "#
        }
        "VLC" => {
            r#"
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
        "#
        }
        "Safari" => {
            r#"
            tell application "Safari"
                if it is running then
                    set tabURL to URL of current tab of front window
                    set tabTitle to name of current tab of front window
                    set tabCount to count of tabs of front window
                    return tabTitle & " | " & tabURL & " | tabs:" & tabCount
                end if
            end tell
        "#
        }
        "Google Chrome" => {
            r#"
            tell application "Google Chrome"
                if it is running then
                    set tabURL to URL of active tab of front window
                    set tabTitle to title of active tab of front window
                    set tabCount to count of tabs of front window
                    return tabTitle & " | " & tabURL & " | tabs:" & tabCount
                end if
            end tell
        "#
        }
        "Arc" => {
            r#"
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
        "#
        }
        "Firefox" => {
            r#"
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
        "#
        }
        "Preview" => {
            r#"
            tell application "Preview"
                if it is running then
                    set docName to name of front document
                    return docName
                end if
            end tell
        "#
        }
        "TextEdit" => {
            r#"
            tell application "TextEdit"
                if it is running then
                    set docName to name of front document
                    return docName
                end if
            end tell
        "#
        }
        "Notes" => {
            r#"
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
        "#
        }
        "Reminders" | "Calendar" | "Mail" => {
            return String::new();
        }
        "Messages" => {
            r#"
            tell application "System Events"
                tell process "Messages"
                    try
                        return name of front window
                    on error
                        return ""
                    end try
                end tell
            end tell
        "#
        }
        "Slack" | "Discord" => {
            r#"
            tell application "System Events"
                tell process "{APP}"
                    try
                        return name of front window
                    on error
                        return ""
                    end try
                end tell
            end tell
        "#
        }
        "Finder" => {
            r#"
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
        "#
        }
        "Terminal" => {
            r#"
            tell application "Terminal"
                if it is running then
                    set tabProcs to processes of front tab of front window
                    set AppleScript's text item delimiters to ", "
                    return tabProcs as text
                end if
            end tell
        "#
        }
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

    let final_script = script.replace("{APP}", app_name);
    run_osascript_timeout(&final_script, Duration::from_secs(2))
}

/// Gather rich OS context to inject into the agent prompt and return the
/// frontmost app name for shortcut lookup in one pass.
pub(crate) fn gather_os_context_snapshot() -> OsContextSnapshot {
    let mut parts: Vec<String> = Vec::new();
    let mut frontmost_app_name = String::new();
    let mut frontmost_window_title = String::new();

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
    if let Ok(output) = Command::new("osascript")
        .arg("-e")
        .arg(frontmost_script)
        .output()
    {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let segs: Vec<&str> = raw.splitn(2, "|||").collect();
        frontmost_app_name = segs.first().unwrap_or(&"").to_string();
        frontmost_window_title = segs.get(1).unwrap_or(&"").to_string();
        parts.push(format!(
            "Frontmost app: {}\nWindow title: {}",
            frontmost_app_name, frontmost_window_title
        ));
    }

    let running_apps_script = r#"
        tell application "System Events"
            set appList to name of every application process whose background only is false
            set AppleScript's text item delimiters to ", "
            return appList as text
        end tell
    "#;
    if let Ok(output) = Command::new("osascript")
        .arg("-e")
        .arg(running_apps_script)
        .output()
    {
        let apps = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !apps.is_empty() {
            parts.push(format!("Running GUI apps: {}", apps));
        }
    }

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
    if let Ok(output) = Command::new("osascript")
        .arg("-e")
        .arg(window_count_script)
        .output()
    {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let segs: Vec<&str> = raw.splitn(2, "|||").collect();
        let count = segs.first().unwrap_or(&"0");
        let names = segs.get(1).unwrap_or(&"").to_string();
        if !names.is_empty() {
            parts.push(format!(
                "Open windows in frontmost app ({}): {}",
                count, names
            ));
        }
    }

    if !frontmost_app_name.is_empty() {
        let app_state = query_app_state(&frontmost_app_name);
        if !app_state.is_empty() {
            parts.push(format!("App state ({}): {}", frontmost_app_name, app_state));
        }

        // Inject real menu bar shortcuts for the frontmost app
        match crate::menu_discovery::discover_menu_items_cmd(frontmost_app_name.clone()) {
            crate::menu_discovery::MenuDiscoveryResult { items, error: None, .. } if !items.is_empty() => {
                let menu_ctx = crate::menu_discovery::format_menu_context(&items);
                if !menu_ctx.is_empty() {
                    parts.push(format!("Available shortcuts ({}):\n{}", frontmost_app_name, menu_ctx));
                }
            }
            _ => {} // Silently skip if discovery fails or returns empty
        }
    }

    let human_date = Command::new("date")
        .arg("+%A, %B %d, %Y %I:%M %p")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_else(|| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("unix {}", now)
        });
    parts.push(format!("Current date/time: {}", human_date));

    let context_block = if parts.is_empty() {
        String::new()
    } else {
        format!("\n\n═══ CURRENT OS STATE ═══\n{}", parts.join("\n"))
    };

    if frontmost_app_name.is_empty() && !frontmost_window_title.is_empty() {
        frontmost_app_name = frontmost_window_title;
    }

    OsContextSnapshot {
        frontmost_app_name,
        context_block,
    }
}

#[tauri::command]
pub fn get_frontmost_app_cmd() -> Result<FrontmostApp, String> {
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
