use serde::{Deserialize, Serialize};
use std::{process::Command, time::Duration};

/// A single discovered menu item from a live macOS application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredMenuItem {
    pub menu: String,
    pub item: String,
    pub shortcut: Option<String>,
    pub enabled: bool,
    pub source: String, // "live"
}

/// Result from a menu discovery scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuDiscoveryResult {
    pub app_name: String,
    pub items: Vec<DiscoveredMenuItem>,
    pub error: Option<String>,
}

/// Enumerate all menu bar items with keyboard shortcuts from the frontmost app
/// using macOS System Events AppleScript.
fn discover_menu_items_applescript(app_name: &str) -> Result<Vec<DiscoveredMenuItem>, String> {
    // This script walks the full menu bar hierarchy of the given process
    // and returns each item with its shortcut (if any) as a pipe-delimited string.
    let script = format!(
        r#"
        tell application "System Events"
            tell process "{}"
                set menuData to {{}}
                try
                    repeat with menuBarItem in menu bar items of menu bar 1
                        set menuName to name of menuBarItem
                        try
                            repeat with menuItem in menu items of menu 1 of menuBarItem
                                try
                                    set itemName to name of menuItem
                                    if itemName is not missing value and itemName is not "" then
                                        set isEnabled to enabled of menuItem
                                        try
                                            set shortcutKey to value of attribute "AXMenuItemCmdChar" of menuItem
                                            set shortcutMods to value of attribute "AXMenuItemCmdModifiers" of menuItem
                                            set modStr to ""
                                            if shortcutMods is not missing value then
                                                if shortcutMods is 0 then
                                                    set modStr to "⌘"
                                                else if shortcutMods is 1 then
                                                    set modStr to "⇧⌘"
                                                else if shortcutMods is 2 then
                                                    set modStr to "⌥⌘"
                                                else if shortcutMods is 3 then
                                                    set modStr to "⇧⌥⌘"
                                                else if shortcutMods is 4 then
                                                    set modStr to "⌃⌘"
                                                else if shortcutMods is 5 then
                                                    set modStr to "⇧⌃⌘"
                                                end if
                                            end if
                                            if shortcutKey is not missing value and shortcutKey is not "" then
                                                copy (menuName & "|" & itemName & "|" & modStr & shortcutKey & "|" & (isEnabled as text)) to end of menuData
                                            else
                                                copy (menuName & "|" & itemName & "||" & (isEnabled as text)) to end of menuData
                                            end if
                                        on error
                                            copy (menuName & "|" & itemName & "||" & (isEnabled as text)) to end of menuData
                                        end try
                                    end if
                                end try
                            end repeat
                        end try
                    end repeat
                end try
                set AppleScript's text item delimiters to "|||"
                return menuData as text
            end tell
        end tell
        "#,
        app_name
    );

    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn osascript: {}", e))?;

    // Timeout after 5 seconds (menu walk can be slow for complex apps)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Menu discovery timed out (5s)".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Wait error: {}", e)),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Output read error: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("AppleScript failed: {}", stderr.trim()));
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    for entry in raw.split("|||") {
        let parts: Vec<&str> = entry.splitn(4, '|').collect();
        if parts.len() >= 2 {
            let menu = parts[0].trim().to_string();
            let item = parts[1].trim().to_string();
            let shortcut = if parts.len() > 2 && !parts[2].trim().is_empty() {
                Some(parts[2].trim().to_string())
            } else {
                None
            };
            let enabled = parts
                .get(3)
                .map(|s| s.trim().to_lowercase() == "true")
                .unwrap_or(true);

            if !item.is_empty() {
                items.push(DiscoveredMenuItem {
                    menu,
                    item,
                    shortcut,
                    enabled,
                    source: "live".to_string(),
                });
            }
        }
    }

    println!(
        "[menu_discovery] found {} items for '{}'",
        items.len(),
        app_name
    );
    Ok(items)
}

/// Tauri command: discover menu items from the frontmost app.
#[tauri::command]
pub fn discover_menu_items_cmd(app_name: String) -> MenuDiscoveryResult {
    let app = app_name.trim().to_string();
    if app.is_empty() {
        return MenuDiscoveryResult {
            app_name: app,
            items: Vec::new(),
            error: Some("No app name provided".to_string()),
        };
    }

    match discover_menu_items_applescript(&app) {
        Ok(items) => MenuDiscoveryResult {
            app_name: app,
            items,
            error: None,
        },
        Err(err) => {
            println!("[menu_discovery] error for '{}': {}", app, err);
            MenuDiscoveryResult {
                app_name: app,
                items: Vec::new(),
                error: Some(err),
            }
        }
    }
}

/// Format discovered menu items as a compact string block for the system prompt.
/// Only includes items WITH keyboard shortcuts, sorted by menu.
pub fn format_menu_context(items: &[DiscoveredMenuItem]) -> String {
    let with_shortcuts: Vec<&DiscoveredMenuItem> =
        items.iter().filter(|i| i.shortcut.is_some()).collect();

    if with_shortcuts.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current_menu = String::new();
    for item in &with_shortcuts {
        if item.menu != current_menu {
            current_menu = item.menu.clone();
            lines.push(format!("  {}:", current_menu));
        }
        if let Some(ref shortcut) = item.shortcut {
            lines.push(format!("    {} → {}", item.item, shortcut));
        }
    }

    if lines.is_empty() {
        return String::new();
    }

    format!("Menu shortcuts:\n{}", lines.join("\n"))
}
