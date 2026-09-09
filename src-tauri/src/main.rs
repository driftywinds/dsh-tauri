// Prevents an additional console window on Windows in all builds.
//
// ## Debug output visibility
// `eprintln!` calls below go to stderr. In a `tauri dev` session the main
// Tauri process's stderr is forwarded to the terminal you ran `pnpm tauri dev`
// from, so *all* debug logging is visible there. In a released app
// (`windows_subsystem = "windows"` suppresses the console) they are silently
// discarded unless a debugger is attached — the release build is intentionally
// quiet.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::webview::DownloadEvent;
use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use url::Url;

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
    eprintln!("[dsh-tauri] Waiting for {host}:{port} to be reachable (timeout: {}s)…", timeout.as_secs());
    let deadline = Instant::now() + timeout;
    let mut attempts = 0u32;
    while Instant::now() < deadline {
        if TcpStream::connect((host, port)).is_ok() {
            eprintln!("[dsh-tauri] Port {port} is reachable (after {attempts} poll(s)).");
            return true;
        }
        attempts += 1;
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("[dsh-tauri] Timed out waiting for {host}:{port} after {}s.", timeout.as_secs());
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
fn spawn_dsh() -> std::io::Result<(Child, ChildStdout, ChildStderr)> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        let mut child = Command::new("cmd")
            .args(["/C", "dsh", "web", "--no-open"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            // Pipe stderr too so the debug reader thread can relay it.
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?;

        // Bind the whole cmd.exe -> dsh -> node tree to a kill-on-close Job
        // Object so it dies with this process even if this process crashes or
        // is killed externally (the graceful path separately uses
        // taskkill /T /F — see kill_dsh).
        assign_kill_on_close_job(&child);

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Ok((child, stdout, stderr))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut child = Command::new("dsh")
            .args(["web", "--no-open"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Ok((child, stdout, stderr))
    }
}

/// Assigns the spawned `dsh web` child to a Windows Job Object whose only
/// limit is `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The job handle is
/// deliberately never closed: the kernel kills every process in the job when
/// its last handle disappears, i.e. when this process terminates — for any
/// reason. Graceful close, panic, Task Manager kill, or crash: the whole
/// `cmd.exe` -> `dsh` -> `node` tree dies with it, so the server can never
/// outlive the app and squat port 3080 (which made the next launch's
/// `dsh web` fail to bind and left the app on a blank window).
///
/// Best-effort by design: on any failure only a warning is logged and the
/// previous behavior remains (graceful shutdown still runs `taskkill /T /F`
/// in `kill_dsh`). Nested jobs are supported since Windows 8, so assignment
/// also succeeds when this app itself is already inside a job.
#[cfg(target_os = "windows")]
fn assign_kill_on_close_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            eprintln!(
                "[dsh-tauri] WARN: CreateJobObjectW failed — the dsh process tree is NOT bound to this process's lifetime; it may survive a crash/force-kill."
            );
            return;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            eprintln!(
                "[dsh-tauri] WARN: SetInformationJobObject failed — the dsh process tree is NOT bound to this process's lifetime; it may survive a crash/force-kill."
            );
            return;
        }

        if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
            eprintln!(
                "[dsh-tauri] WARN: AssignProcessToJobObject failed — the dsh process tree is NOT bound to this process's lifetime; it may survive a crash/force-kill."
            );
            return;
        }

        eprintln!("[dsh-tauri] dsh web process tree bound to kill-on-close job object.");
        // `job` is intentionally leaked (no CloseHandle): keeping the handle
        // alive is what keeps the kill-on-close contract armed.
    }
}

fn kill_dsh(state: &DshProcess) {
    if let Some(mut child) = state.0.lock().unwrap().take() {
        let pid = child.id();
        eprintln!("[dsh-tauri] Killing dsh web process (PID {pid})…");
        // On Windows, `child` is the `cmd.exe` wrapper — killing it alone
        // leaves the actual `dsh` process (its child) running, since Windows
        // doesn't cascade-kill descendants the way Unix process groups do.
        // `taskkill /T` kills the whole process tree instead.
        #[cfg(target_os = "windows")]
        {
            let result = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
            eprintln!("[dsh-tauri] taskkill /T /F (PID {pid}): {result:?}");
        }
        // Best-effort: dsh may have already exited on its own.
        // Do NOT call child.wait() here — that can block for seconds.
        let _ = child.kill();
        eprintln!("[dsh-tauri] dsh web process killed.");
    } else {
        eprintln!("[dsh-tauri] kill_dsh: no child process to kill.");
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

/// Extract the authenticated dsh web URL from one line of `dsh web` stdout.
/// Expected line format:  `dsh web: http://127.0.0.1:3080/?token=abc123`
/// (optionally followed by ` (LAN: http://...?token=...)` which is ignored).
fn extract_dsh_web_url_line(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = line.strip_prefix("dsh web: ")?;
    // Take the first whitespace-delimited token — the loopback URL itself.
    let url_str = rest.split_whitespace().next()?;
    if url_str.starts_with("http://") || url_str.starts_with("https://") {
        Some(url_str.to_string())
    } else {
        None
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
            eprintln!("[dsh-tauri] Spawning `dsh web --no-open`…");
            let (child, stdout, stderr) = spawn_dsh().expect(
                "failed to launch `dsh web` — is the `dsh` binary installed and on PATH?",
            );
            eprintln!("[dsh-tauri] dsh web process spawned (PID {})", child.id());

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
            let app_handle = app.handle().clone();
            let _window = tauri::WebviewWindowBuilder::from_config(&app_handle, &window_config)?
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
                            eprintln!("[dsh-tauri] Downloading {url} -> {}", destination.display());
                        }
                        DownloadEvent::Finished { url, path, success } => {
                            if success {
                                eprintln!("[dsh-tauri] Download finished: {url} -> {path:?}");
                            } else {
                                eprintln!("[dsh-tauri] Download failed: {url}");
                            }
                        }
                        _ => {}
                    }
                    // Always let the download proceed.
                    true
                })
                .build()
                .expect("failed to build main window");

            // Spawn a reader thread that logs every line from dsh web's stderr
            // (useful when troubleshooting the DSH server itself — .stderr is
            // piped so the subprocess won't block).
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => eprintln!("[dsh stderr] {l}"),
                        Err(e) => eprintln!("[dsh stderr] (read error: {e})"),
                    }
                }
                eprintln!("[dsh stderr] (stream ended)");
            });

            // Clone the AppHandle for the stdout reader thread so it can
            // dispatch navigation back to the main (event-loop) thread later.
            let navigate_handle = app_handle.clone();
            std::thread::spawn(move || {
                // Read dsh web's stdout line by line until we find the
                // authenticated URL (which carries the per-process `?token=...`).
                let authenticated_url = {
                    let reader = BufReader::new(stdout);
                    let deadline = Instant::now() + DSH_READY_TIMEOUT;
                    let mut found_url: Option<String> = None;

                    for line_result in reader.lines() {
                        if Instant::now() >= deadline {
                            eprintln!(
                                "[dsh-tauri] Timed out after {}s waiting for dsh web URL on stdout",
                                DSH_READY_TIMEOUT.as_secs()
                            );
                            return;
                        }
                        let line = match line_result {
                            Ok(l) => l,
                            Err(e) => {
                                eprintln!("[dsh-tauri] Error reading dsh web stdout: {e}");
                                return;
                            }
                        };
                        eprintln!("[dsh stdout] {line}");
                        if let Some(url) = extract_dsh_web_url_line(&line) {
                            eprintln!("[dsh-tauri] Extracted authenticated URL: {url}");
                            found_url = Some(url);
                            break;
                        }
                    }

                    match found_url {
                        Some(url) => url,
                        None => {
                            eprintln!("[dsh-tauri] dsh web closed stdout before printing the URL");
                            return;
                        }
                    }
                };

                // The URL is printed once the server is listening (or very
                // shortly after), but wait for the port to be reachable so we
                // don't race the TCP listen socket.
                if !wait_for_port(DSH_HOST, DSH_PORT, DSH_READY_TIMEOUT) {
                    eprintln!(
                        "[dsh-tauri] Port {DSH_PORT} never became reachable. Giving up."
                    );
                    return;
                }

                // Navigate the window on the MAIN event-loop thread so the
                // WebView2 is guaranteed to be fully created and ready by the
                // time the closure runs.  Calling navigate() from a raw thread
                // races window creation (which Tauri queues on the event loop
                // during from_config+build), and a lost race leaves you staring
                // at about:blank with a silent failure.
                let url_for_main = authenticated_url.clone();
                let closure_handle = navigate_handle.clone();
                let _ = navigate_handle.run_on_main_thread(move || {
                    let main_window = match closure_handle.get_webview_window("main") {
                        Some(w) => w,
                        None => {
                            eprintln!("[dsh-tauri] ERROR: main window not found on event loop — navigation aborted.");
                            return;
                        }
                    };

                    eprintln!("[dsh-tauri] Navigating WebView to authenticated URL…");
                    match Url::parse(&url_for_main) {
                        Ok(url) => {
                            if let Err(e) = main_window.navigate(url) {
                                eprintln!("[dsh-tauri] ERROR: window.navigate() failed: {e}");
                                // Fallback: try eval as a last resort.
                                eprintln!("[dsh-tauri] Falling back to window.eval()…");
                                let _ = main_window.eval(&format!(
                                    "window.location.replace('{url_for_main}')"
                                ));
                            } else {
                                eprintln!("[dsh-tauri] window.navigate() succeeded.");
                            }
                        }
                        Err(e) => {
                            eprintln!("[dsh-tauri] ERROR: Url::parse() failed for '{url_for_main}': {e}");
                            // Fallback: try eval anyway.
                            eprintln!("[dsh-tauri] Falling back to window.eval()…");
                            let _ = main_window.eval(&format!(
                                "window.location.replace('{url_for_main}')"
                            ));
                        }
                    }

                    // Bring the window to the foreground.
                    if let Err(e) = main_window.set_focus() {
                        eprintln!("[dsh-tauri] window.set_focus() failed: {e}");
                    } else {
                        eprintln!("[dsh-tauri] window.set_focus() succeeded.");
                    }
                });
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