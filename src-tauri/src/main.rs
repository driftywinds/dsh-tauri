// Prevents an additional console window on Windows in all builds.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::webview::DownloadEvent;
use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const DSH_HOST: &str = "127.0.0.1";
const DSH_PORT: u16 = 3080;
const DSH_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Height, in CSS pixels, of the strip at the top of the window that's
/// draggable. Everything below this line behaves like normal page content
/// (text selection, buttons, scrolling, etc. all work normally) since there's
/// no OS titlebar (`decorations: false`) to grab otherwise.
const DRAG_REGION_HEIGHT_PX: u32 = 15;

/// Makes only the top `DRAG_REGION_HEIGHT_PX` pixels of the frameless window
/// draggable, mimicking a titlebar. Below that line, mousedown behaves like
/// normal page content — so selecting text, clicking buttons, etc. all work
/// everywhere else in the window. Within the drag strip itself, a mousedown
/// still won't start a drag if the target is a button/link/input/etc., or is
/// opted out via `class="no-drag"` or `-webkit-app-region: no-drag` (so you
/// can put real controls, e.g. a close button, inside the strip).
///
/// This is passed to `initialization_script`, which (unlike a one-off
/// `window.eval(...)` call) is re-injected before *every* page load —
/// including the initial "about:blank" placeholder and the later
/// navigation to `http://localhost:3080`. A plain `eval` only touches
/// whatever page happens to be loaded the instant it runs, so it would be
/// wiped out the moment the window navigates to the real dsh UI.
///
/// Requires `"withGlobalTauri": true` in `tauri.conf.json` (so
/// `window.__TAURI__` exists) and the `core:window:allow-start-dragging`
/// permission in `capabilities/default.json`.
fn drag_region_script() -> String {
    format!(
        r#"
    (function () {{
        var DRAG_REGION_HEIGHT = {height}; // px from the top of the window
        var SKIP_TAGS = {{ A: 1, BUTTON: 1, INPUT: 1, TEXTAREA: 1, SELECT: 1, OPTION: 1 }};

        document.addEventListener('mousedown', function (event) {{
            if (event.button !== 0) return; // left click only
            if (event.clientY > DRAG_REGION_HEIGHT) return; // only the top strip drags the window

            var target = event.target;
            if (!target || SKIP_TAGS[target.tagName]) return;

            var classList = target.className;
            if (typeof classList === 'string' && classList.split(' ').indexOf('no-drag') !== -1) {{
                return;
            }}
            var region = window.getComputedStyle(target).getPropertyValue('-webkit-app-region');
            if (region && region.trim() === 'no-drag') return;

            var tauriWindow = window.__TAURI__ && window.__TAURI__.window;
            if (tauriWindow && typeof tauriWindow.getCurrentWindow === 'function') {{
                tauriWindow.getCurrentWindow().startDragging();
            }}
        }});
    }})();
    "#,
        height = DRAG_REGION_HEIGHT_PX
    )
}

/// Holds the handle to the spawned `dsh web` child process so it can be
/// killed when the app shuts down.
struct DshProcess(Mutex<Option<Child>>);

fn wait_for_port(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Spawns `dsh web` without showing a console window and without opening
/// a browser window.
///
/// On Windows, `dsh` is very often a `.cmd`/`.bat`/`.ps1` shim rather than a
/// raw `.exe` (common for CLIs installed via npm/cargo wrapper scripts).
/// `cmd.exe` knows how to resolve those via PATHEXT — which is why `dsh`
/// works fine when you type it in a terminal — but `Command::new("dsh")`
/// calls `CreateProcess` directly and only auto-appends `.exe`, so it fails
/// with "not found" even though the same name works in your shell. Routing
/// through `cmd /C` restores that PATHEXT resolution.
/// `CREATE_NO_WINDOW` suppresses the CMD console window.
fn spawn_dsh() -> std::io::Result<Child> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        Command::new("cmd")
            .args(["/C", "dsh", "web", "--no-open"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("dsh")
            .args(["web", "--no-open"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
    }
}

fn kill_dsh(state: &DshProcess) {
    if let Some(mut child) = state.0.lock().unwrap().take() {
        // On Windows, `child` is the `cmd.exe` wrapper — killing it alone
        // leaves the actual `dsh` process (its child) running, since Windows
        // doesn't cascade-kill descendants the way Unix process groups do.
        // `taskkill /T` kills the whole process tree instead.
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .output();
        }
        // Best-effort: dsh may have already exited on its own.
        // Do NOT call child.wait() here — that can block for seconds.
        let _ = child.kill();
    }
}

/// Very small percent-decoder for the last path segment of a download URL.
/// Only unescapes `%XX` triplets; anything else (including invalid UTF-8) is
/// passed through as-is rather than erroring, since this is only used to
/// build a nicer on-disk filename (e.g. `session%20log.txt` -> `session
/// log.txt`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turns a download URL's last path segment into a safe, non-empty filename:
/// strips path separators / control characters a server could sneak in
/// (defense against path traversal via a crafted filename), and falls back
/// to a timestamped default if there's nothing usable — e.g. the URL ends in
/// `/`, is a bare `blob:` URL with no path, or decodes to `.`/`..`.
fn sanitize_download_filename(raw: Option<&str>) -> String {
    let decoded = raw.map(percent_decode).unwrap_or_default();

    let cleaned: String = decoded
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim();

    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("download-{stamp}")
    } else {
        cleaned.to_string()
    }
}

/// If `filename` already exists in `dir`, appends " (1)", " (2)", ... before
/// the extension so a repeat download never silently clobbers a previous
/// one (mirrors what browsers do).
fn unique_download_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("download");
    let ext = path.extension().and_then(|s| s.to_str());

    for n in 1..10_000 {
        let name = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Astronomically unlikely fallback (10,000 collisions).
    dir.join(filename)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(DshProcess(Mutex::new(None)))
        .setup(|app| {
            // --- Spawn dsh web and navigate to it once ready ---
            let child = spawn_dsh().expect(
                "failed to launch `dsh web` — is the `dsh` binary installed and on PATH?",
            );

            {
                let state = app.state::<DshProcess>();
                *state.0.lock().unwrap() = Some(child);
            }

            // The "main" window has `"create": false` in tauri.conf.json, so
            // Tauri does *not* auto-create it at startup — we build it here
            // ourselves from that same config, which lets us attach
            // `drag_region_script()` as a real initialization script (see
            // its doc comment for why a one-off `.eval()` isn't good enough)
            // and a download handler.
            let window_config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .expect("`main` window not found in tauri.conf.json")
                .clone();

            // Resolve the OS "Downloads" folder once, up front, for the
            // download handler below. Falls back to the temp dir on the
            // (very unlikely) chance it can't be resolved.
            let downloads_dir = app
                .path()
                .download_dir()
                .unwrap_or_else(|_| std::env::temp_dir());

            // `.disable_drag_drop_handler()` disables Tauri's *native*
            // OS-level drag-drop handler (used for dropping files onto the
            // window). That handler is ON by default and, on
            // Windows/WebView2, intercepts drag-related mouse events before
            // they ever reach our JS — silently eating the exact
            // mousedown→drag sequence `startDragging()` needs, no matter
            // how correct the JS is. Setting `dragDropEnabled: false` in
            // tauri.conf.json alone does NOT work here: there's a known
            // Tauri bug where that config field is ignored for windows
            // built via `WebviewWindowBuilder::from_config` (which is why
            // it's also forced explicitly here rather than left to the
            // config).
            let window = tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?
                .initialization_script(drag_region_script())
                .disable_drag_drop_handler()
                // Gives the page (and any "download session log" style
                // button in dsh's own UI) real file-download capability.
                // Recent Tauri/wry versions do allow downloads to proceed
                // by default, but without this handler there's no control
                // over *where* a file lands or what it's named, and a
                // frameless window has no native chrome (save-as dialog,
                // downloads bar) to fall back on — so we pick a sensible,
                // collision-safe destination in the user's Downloads folder
                // ourselves.
                .on_download(move |_webview, event| {
                    match event {
                        DownloadEvent::Requested { url, destination } => {
                            let filename = sanitize_download_filename(
                                url.path_segments().and_then(|mut s| s.next_back()),
                            );
                            *destination = unique_download_path(&downloads_dir, &filename);
                            eprintln!("Downloading {url} -> {}", destination.display());
                        }
                        DownloadEvent::Finished { url, path, success } => {
                            if success {
                                eprintln!("Download finished: {url} -> {path:?}");
                            } else {
                                eprintln!("Download failed: {url}");
                            }
                        }
                        _ => {}
                    }
                    // Always let the download proceed.
                    true
                })
                .build()
                .expect("failed to build main window");

            std::thread::spawn(move || {
                if wait_for_port(DSH_HOST, DSH_PORT, DSH_READY_TIMEOUT) {
                    let url = format!("http://{DSH_HOST}:{DSH_PORT}");
                    let _ = window.eval(&format!("window.location.replace('{url}')"));
                    let _ = window.set_focus();
                } else {
                    eprintln!(
                        "Timed out after {}s waiting for dsh web on {DSH_HOST}:{DSH_PORT}",
                        DSH_READY_TIMEOUT.as_secs()
                    );
                }
            });

            // --- Register Ctrl+Q global shortcut to close the window ---
            #[cfg(desktop)]
            {
                let ctrl_q = Shortcut::new(Some(Modifiers::CONTROL), Code::KeyQ);
                app.handle().plugin(
                    tauri_plugin_global_shortcut::Builder::new()
                        .with_handler(move |_app, shortcut, event| {
                            if shortcut == &ctrl_q
                                && event.state() == ShortcutState::Pressed
                            {
                                if let Some(window) = _app.get_webview_window("main") {
                                    let _ = window.close();
                                }
                            }
                        })
                        .build(),
                )?;
                app.global_shortcut().register(ctrl_q)?;
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Belt-and-suspenders: make sure dsh dies even if the app exits
            // through a path other than the window close event above
            // (e.g. Cmd+Q on macOS, or the process being killed externally).
            if let tauri::RunEvent::ExitRequested { .. } = event {
                kill_dsh(&app_handle.state::<DshProcess>());
            }

            // Kill any windows that aren't the main window (prevents
            // dsh web UI from spawning extra browser/webview windows).
            if let tauri::RunEvent::WindowEvent { label, .. } = event {
                if label != "main" {
                    if let Some(w) = app_handle.get_webview_window(&label) {
                        let _ = w.close();
                    }
                }
            }
        });
}