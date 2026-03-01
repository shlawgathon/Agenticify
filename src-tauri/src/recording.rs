use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// ── CFRunLoop FFI (macOS) ──────────────────────────────
// Used to cleanly stop the rdev::listen run‐loop from another thread.

type RawCFRunLoopRef = *mut std::os::raw::c_void;

/// Wrapper so we can send the run‐loop ref across threads.
pub(crate) struct RunLoopHandle(RawCFRunLoopRef);
unsafe impl Send for RunLoopHandle {}
unsafe impl Sync for RunLoopHandle {}

extern "C" {
    fn CFRunLoopGetCurrent() -> RawCFRunLoopRef;
    fn CFRunLoopStop(rl: RawCFRunLoopRef);
}
use tauri::State;
use xcap::Monitor;

// ── Constants ──────────────────────────────────────────

fn recordings_root() -> PathBuf {
    std::env::temp_dir().join("agenticify-recordings")
}

fn now_ms() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .map_err(|e| e.to_string())
}

// ── Types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputEvent {
    pub unix_ms: u128,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub session_id: String,
    pub name: String,
    pub instruction: String,
    #[serde(default)]
    pub task_context: String,
    #[serde(default)]
    pub model: String,
    pub output_dir: String,
    pub fps: u32,
    pub frame_ticks: u64,
    pub duration_ms: u128,
    #[serde(default)]
    pub input_event_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub active: bool,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub elapsed_ms: Option<u128>,
    pub frame_ticks: u64,
}

pub struct ActiveSession {
    pub session_id: String,
    pub name: String,
    pub instruction: String,
    pub task_context: String,
    pub model: String,
    pub output_dir: PathBuf,
    pub fps: u32,
    pub started_unix_ms: u128,
    pub stop_flag: Arc<AtomicBool>,
    pub frame_ticks: Arc<AtomicU64>,
    pub input_events: Arc<Mutex<Vec<InputEvent>>>,
    pub frame_worker: Option<thread::JoinHandle<()>>,
    pub input_worker: Option<thread::JoinHandle<()>>,
    pub input_run_loop: Arc<OnceLock<RunLoopHandle>>,
}

#[derive(Default)]
pub struct SessionRecordingState {
    pub active: Mutex<Option<ActiveSession>>,
}

// ── Start Recording ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StartSessionRequest {
    pub name: Option<String>,
    pub instruction: Option<String>,
    pub task_context: Option<String>,
    pub model: Option<String>,
    pub fps: Option<u32>,
}

#[tauri::command]
pub fn start_session_cmd(
    state: State<SessionRecordingState>,
    req: StartSessionRequest,
) -> Result<SessionStatus, String> {
    let mut guard = state
        .active
        .lock()
        .map_err(|_| "Session lock poisoned".to_string())?;

    if guard.is_some() {
        return Err("A recording session is already active".to_string());
    }

    let started = now_ms()?;
    let session_id = format!("session-{}", started);
    let session_fps = req.fps.unwrap_or(2).clamp(1, 8);
    let name = req.name.unwrap_or_else(|| session_id.clone());
    let instruction = req.instruction.unwrap_or_default();
    let task_context = req.task_context.unwrap_or_default();
    let model = req.model.unwrap_or_default();

    let output_dir = recordings_root().join(&session_id);
    fs::create_dir_all(&output_dir).map_err(|e| e.to_string())?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let frame_ticks = Arc::new(AtomicU64::new(0));
    let input_events: Arc<Mutex<Vec<InputEvent>>> = Arc::new(Mutex::new(Vec::new()));

    // ── Frame capture worker ───────────────────────────
    let frame_stop = Arc::clone(&stop_flag);
    let frame_counter = Arc::clone(&frame_ticks);
    let frame_dir = output_dir.clone();

    let frame_worker = thread::spawn(move || {
        let interval = Duration::from_millis((1000 / session_fps.max(1)) as u64);

        while !frame_stop.load(Ordering::SeqCst) {
            let tick_start = Instant::now();
            let tick = frame_counter.fetch_add(1, Ordering::SeqCst) + 1;

            // Acquire the global capture mutex so we don't race with
            // capture_primary_cmd (agent loop). If the lock is held,
            // try_lock returns Err and we skip this frame gracefully.
            // IMPORTANT: only hold the lock during capture, NOT during save.
            let captured: Vec<(u32, image::DynamicImage)> = if let Ok(_guard) = crate::capture_mutex().try_lock() {
                let mut frames = Vec::new();
                if let Ok(monitors) = Monitor::all() {
                    for monitor in monitors {
                        let monitor_id = match monitor.id() {
                            Ok(id) => id,
                            Err(_) => continue,
                        };
                        if let Ok(image) = monitor.capture_image() {
                            frames.push((monitor_id, image::DynamicImage::from(image)));
                        }
                    }
                }
                frames
                // _guard dropped here — lock released before disk I/O
            } else {
                Vec::new() // agent is capturing — skip this recording frame
            };

            // Save frames to disk OUTSIDE the lock
            for (monitor_id, image) in captured {
                let mon_dir = frame_dir.join(format!("monitor-{}", monitor_id));
                let _ = fs::create_dir_all(&mon_dir);
                let frame_file = mon_dir.join(format!("frame-{:06}.png", tick));
                let _ = image.save(&frame_file);
            }

            let elapsed = tick_start.elapsed();
            if elapsed < interval {
                thread::sleep(interval - elapsed);
            }
        }
    });

    // ── Input capture worker (rdev) ────────────────────
    let input_stop = Arc::clone(&stop_flag);
    let input_sink = Arc::clone(&input_events);
    let input_run_loop: Arc<OnceLock<RunLoopHandle>> = Arc::new(OnceLock::new());
    let input_run_loop_tx = Arc::clone(&input_run_loop);

    let input_worker = thread::spawn(move || {
        let stop = input_stop;
        let sink = input_sink;

        // Capture this thread's CFRunLoop *before* rdev::listen enters it,
        // so stop_session_cmd can call CFRunLoopStop to unblock it.
        let rl = unsafe { CFRunLoopGetCurrent() };
        let _ = input_run_loop_tx.set(RunLoopHandle(rl));

        // rdev::listen is blocking; we poll `stop_flag` inside the callback
        // and use a thread so we can set the flag from outside.
        let _ = rdev::listen(move |event: rdev::Event| {
            if stop.load(Ordering::SeqCst) {
                return;
            }

            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);

            let input_event = match event.event_type {
                rdev::EventType::MouseMove { x, y } => InputEvent {
                    unix_ms,
                    event_type: "mouse_move".into(),
                    x: Some(x),
                    y: Some(y),
                    button: None,
                    key: None,
                },
                rdev::EventType::ButtonPress(btn) => InputEvent {
                    unix_ms,
                    event_type: "mouse_click".into(),
                    x: None,
                    y: None,
                    button: Some(format!("{:?}", btn)),
                    key: None,
                },
                rdev::EventType::ButtonRelease(btn) => InputEvent {
                    unix_ms,
                    event_type: "mouse_release".into(),
                    x: None,
                    y: None,
                    button: Some(format!("{:?}", btn)),
                    key: None,
                },
                rdev::EventType::KeyPress(key) => InputEvent {
                    unix_ms,
                    event_type: "key_press".into(),
                    x: None,
                    y: None,
                    button: None,
                    key: Some(format!("{:?}", key)),
                },
                rdev::EventType::KeyRelease(key) => InputEvent {
                    unix_ms,
                    event_type: "key_release".into(),
                    x: None,
                    y: None,
                    button: None,
                    key: Some(format!("{:?}", key)),
                },
                rdev::EventType::Wheel { delta_x, delta_y } => InputEvent {
                    unix_ms,
                    event_type: "wheel".into(),
                    x: Some(delta_x as f64),
                    y: Some(delta_y as f64),
                    button: None,
                    key: None,
                },
            };

            // Throttle mouse_move to ~30fps max to avoid huge files
            if input_event.event_type == "mouse_move" {
                if let Ok(events) = sink.lock() {
                    if let Some(last) = events.last() {
                        if last.event_type == "mouse_move"
                            && unix_ms.saturating_sub(last.unix_ms) < 33
                        {
                            return;
                        }
                    }
                }
            }

            if let Ok(mut events) = sink.lock() {
                events.push(input_event);
            }
        });
    });

    let status = SessionStatus {
        active: true,
        session_id: Some(session_id.clone()),
        name: Some(name.clone()),
        elapsed_ms: Some(0),
        frame_ticks: 0,
    };

    *guard = Some(ActiveSession {
        session_id,
        name,
        instruction,
        task_context,
        model,
        output_dir,
        fps: session_fps,
        started_unix_ms: started,
        stop_flag,
        frame_ticks,
        input_events,
        frame_worker: Some(frame_worker),
        input_worker: Some(input_worker),
        input_run_loop,
    });

    Ok(status)
}

// ── Stop Recording ─────────────────────────────────────

#[tauri::command]
pub fn stop_session_cmd(
    state: State<SessionRecordingState>,
) -> Result<SessionManifest, String> {
    let mut guard = state
        .active
        .lock()
        .map_err(|_| "Session lock poisoned".to_string())?;

    let mut active = guard
        .take()
        .ok_or_else(|| "No active recording session".to_string())?;

    // Signal workers to stop
    active.stop_flag.store(true, Ordering::SeqCst);

    // Join frame worker
    if let Some(w) = active.frame_worker.take() {
        let _ = w.join();
    }

    // Stop the CFRunLoop driving rdev::listen so the thread can exit cleanly
    if let Some(handle) = active.input_run_loop.get() {
        unsafe { CFRunLoopStop(handle.0) };
    }
    if let Some(w) = active.input_worker.take() {
        let _ = w.join();
    }

    let finished = now_ms()?;
    let ticks = active.frame_ticks.load(Ordering::SeqCst);

    // Write input events
    let events: Vec<InputEvent> = active
        .input_events
        .lock()
        .map_err(|_| "Input events lock poisoned".to_string())?
        .drain(..)
        .collect();

    let events_path = active.output_dir.join("input_events.json");
    let events_json = serde_json::to_string(&events).map_err(|e| e.to_string())?;
    fs::write(&events_path, events_json).map_err(|e| e.to_string())?;

    let manifest = SessionManifest {
        session_id: active.session_id.clone(),
        name: active.name.clone(),
        instruction: active.instruction.clone(),
        task_context: active.task_context.clone(),
        model: active.model.clone(),
        output_dir: active.output_dir.to_string_lossy().to_string(),
        fps: active.fps,
        frame_ticks: ticks,
        duration_ms: finished.saturating_sub(active.started_unix_ms),
        input_event_count: events.len(),
    };

    // Write manifest
    let manifest_json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    let manifest_path = active.output_dir.join("manifest.json");
    fs::write(&manifest_path, manifest_json).map_err(|e| e.to_string())?;

    println!(
        "[recording] session '{}' saved: {} frames, {} input events, {}ms",
        manifest.name, ticks, manifest.input_event_count, manifest.duration_ms
    );

    Ok(manifest)
}

// ── Session Status ─────────────────────────────────────

#[tauri::command]
pub fn session_status_cmd(
    state: State<SessionRecordingState>,
) -> SessionStatus {
    match state.active.lock() {
        Ok(guard) => {
            if let Some(active) = guard.as_ref() {
                let elapsed = now_ms().unwrap_or(0).saturating_sub(active.started_unix_ms);
                SessionStatus {
                    active: true,
                    session_id: Some(active.session_id.clone()),
                    name: Some(active.name.clone()),
                    elapsed_ms: Some(elapsed),
                    frame_ticks: active.frame_ticks.load(Ordering::SeqCst),
                }
            } else {
                SessionStatus {
                    active: false,
                    session_id: None,
                    name: None,
                    elapsed_ms: None,
                    frame_ticks: 0,
                }
            }
        }
        Err(_) => SessionStatus {
            active: false,
            session_id: None,
            name: None,
            elapsed_ms: None,
            frame_ticks: 0,
        },
    }
}

// ── List Sessions ──────────────────────────────────────

#[tauri::command]
pub fn list_sessions_cmd() -> Result<Vec<SessionManifest>, String> {
    let root = recordings_root();
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
        if let Ok(mut manifest) = serde_json::from_str::<SessionManifest>(&text) {
            if manifest.output_dir.is_empty() {
                manifest.output_dir = path.to_string_lossy().to_string();
            }
            sessions.push(manifest);
        }
    }

    sessions.sort_by(|a, b| b.session_id.cmp(&a.session_id));
    Ok(sessions)
}

// ── Load Session ───────────────────────────────────────

#[tauri::command]
pub fn load_session_cmd(session_id: String) -> Result<SessionManifest, String> {
    let manifest_path = recordings_root()
        .join(&session_id)
        .join("manifest.json");

    if !manifest_path.exists() {
        return Err(format!("Session '{}' not found", session_id));
    }

    let text = fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
    let manifest: SessionManifest =
        serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(manifest)
}

// ── Delete Session ─────────────────────────────────────

#[tauri::command]
pub fn delete_session_cmd(session_id: String) -> Result<(), String> {
    let dir = recordings_root().join(&session_id);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Activity Log Persistence ───────────────────────────

#[tauri::command]
pub fn save_activity_log_cmd(
    session_id: String,
    activity_log: Vec<serde_json::Value>,
) -> Result<(), String> {
    let dir = recordings_root().join(&session_id);
    if !dir.exists() {
        return Err(format!("Session '{}' not found", session_id));
    }

    let path = dir.join("activity_log.json");
    let json = serde_json::to_string_pretty(&activity_log).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())?;

    println!(
        "[recording] saved activity log for '{}': {} entries",
        session_id,
        activity_log.len()
    );
    Ok(())
}

#[tauri::command]
pub fn load_activity_log_cmd(
    session_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    let path = recordings_root().join(&session_id).join("activity_log.json");

    if !path.exists() {
        // Graceful fallback for old sessions without an activity log
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let log: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(log)
}
