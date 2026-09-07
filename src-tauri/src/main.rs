// Prevents an additional console window on Windows in all builds.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const DSH_HOST: &str = "127.0.0.1";
const DSH_PORT: u16 = 3080;
const DSH_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Makes the frameless window draggable from anywhere in the webview
/// content. On mousedown we start a window drag unless the target is a
/// button, link, input, textarea, select, or already opted out via
/// `class="no-drag"` or `-webkit-app-region: no-drag`.
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
const DRAG_REGION_SCRIPT: &str = r#"
    (function () {
        var SKIP_TAGS = { A: 1, BUTTON: 1, INPUT: 1, TEXTAREA: 1, SELECT: 1, OPTION: 1 };

        document.addEventListener('mousedown', function (event) {
            if (event.button !== 0) return; // left click only

            var target = event.target;
            if (!target || SKIP_TAGS[target.tagName]) return;

            var classList = target.className;
            if (typeof classList === 'string' && classList.split(' ').indexOf('no-drag') !== -1) {
                return;
            }
            var region = window.getComputedStyle(target).getPropertyValue('-webkit-app-region');
            if (region && region.trim() === 'no-drag') return;

            var tauriWindow = window.__TAURI__ && window.__TAURI__.window;
            if (tauriWindow && typeof tauriWindow.getCurrentWindow === 'function') {
                tauriWindow.getCurrentWindow().startDragging();
            }
        });
    })();
"#;

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
            // `DRAG_REGION_SCRIPT` as a real initialization script (see its
            // doc comment for why a one-off `.eval()` isn't good enough).
            let window_config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .expect("`main` window not found in tauri.conf.json")
                .clone();

            // `.drag_and_drop(false)` disables Tauri's *native* OS-level
            // drag-drop handler (used for dropping files onto the window).
            // That handler is ON by default and, on Windows/WebView2,
            // intercepts drag-related mouse events before they ever reach
            // our JS — silently eating the exact mousedown→drag sequence
            // `startDragging()` needs, no matter how correct the JS is.
            // Setting `dragDropEnabled: false` in tauri.conf.json alone
            // does NOT work here: there's a known Tauri bug where that
            // config field is ignored for windows built via
            // `WebviewWindowBuilder::from_config` (which is why it's also
            // forced explicitly here rather than left to the config).
            let window = tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?
                .initialization_script(DRAG_REGION_SCRIPT)
                .drag_and_drop(false)
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