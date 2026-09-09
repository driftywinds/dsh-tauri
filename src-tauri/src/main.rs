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

/// Makes Ctrl+Q close the window — window-local, unlike a global shortcut:
/// the keydown listener lives in the page and only fires while this window
/// has keyboard focus, so Ctrl+Q keeps its normal meaning in every other
/// application, and a hotkey collision can never abort startup.
///
/// `getCurrentWindow().close()` is an IPC call, so it requires
/// `core:window:allow-close` in capabilities/default.json. Like the drag
/// region script, this is re-injected before *every* page load (including
/// about:blank and the dsh UI), so it also works on the startup error page.
fn close_shortcut_script() -> String {
    r#"
    (function () {
        document.addEventListener('keydown', function (event) {
            if (event.defaultPrevented) return; // the page already handled it
            if (!event.ctrlKey || event.altKey || event.metaKey) return;
            if (event.code !== 'KeyQ') return; // physical key: layout- and CapsLock-independent
            var tauriWindow = window.__TAURI__ && window.__TAURI__.window;
            if (tauriWindow && typeof tauriWindow.getCurrentWindow === 'function') {
                event.preventDefault();
                tauriWindow.getCurrentWindow().close();
            }
        });
    })();
    "#
    .to_string()
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

/// Renders a fatal-error page into the main window (which sits on
/// about:blank at startup), replacing the silent white rectangle with the
/// reason and a hint. The text passes through two encodings, so it is
/// escaped for both: HTML entities (it is assigned via innerHTML) and
/// JavaScript string escapes (it rides inside an eval'd single-quoted
/// string) — error text can neither inject markup nor break the script.
fn show_error_page(window: &tauri::WebviewWindow, heading: &str, details: &str) {
    let escape = |s: &str| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\\', "\\\\")
            .replace('`', "\\`")
            .replace("${", "\\${")
            .replace('\'', "\\'")
            .replace('\r', "")
            .replace('\n', "\\n")
    };
    let script = format!(
        r#"(function () {{
            document.body.style.cssText = 'margin:0;background:#16171a;color:#d7d7db;font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;';
            document.body.innerHTML = '<div style="max-width:660px;padding:0 32px;">'
                + '<h1 style="font-size:19px;font-weight:600;color:#ff8080;margin:0 0 14px;">{heading}</h1>'
                + '<pre style="white-space:pre-wrap;font-size:13px;line-height:1.6;margin:0;color:#c7c7cc;">{details}</pre>'
                + '<p style="font-size:12px;color:#77777d;margin-top:22px;">You can close this window.</p>'
                + '</div>';
        }})();"#,
        heading = escape(heading),
        details = escape(details),
    );
    if let Err(e) = window.eval(&script) {
        eprintln!("[dsh-tauri] ERROR: could not render the startup error page: {e}");
    }
}

/// Surfaces a fatal startup failure in the UI; callable from any thread.
/// The injection is queued onto the main event-loop thread. When called
/// during `setup` (before the loop runs) it executes as soon as the loop
/// starts, by which time the webview exists — a direct `eval` call would
/// race window creation (see the navigation comment in `main`).
fn report_startup_failure(handle: &tauri::AppHandle, heading: &str, details: &str) {
    let heading = heading.to_string();
    let details = details.to_string();
    let closure_handle = handle.clone();
    let _ = handle.run_on_main_thread(move || match closure_handle.get_webview_window("main") {
        Some(w) => show_error_page(&w, &heading, &details),
        None => eprintln!(
            "[dsh-tauri] ERROR: main window not found; startup error could not be shown: {heading}: {details}"
        ),
    });
}

fn main() {
    tauri::Builder::default()
        // Must be the FIRST plugin (per its docs): a second app launch exits
        // during this registration — before setup() — so it can never spawn
        // its own doomed `dsh web` or fight over port 3080.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Runs in the FIRST instance when a second launch is blocked:
            // surface the existing window instead of opening a clone.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(DshProcess(Mutex::new(None)))
        .setup(|app| {
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
                .initialization_script(close_shortcut_script())
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

            // --- Spawn dsh web ---
            // Spawned *after* the window exists so a spawn failure (e.g. the
            // `dsh` CLI is missing) can be rendered in the UI. Panicking here
            // would be invisible: release builds have no console.
            eprintln!("[dsh-tauri] Spawning `dsh web --no-open`…");
            let (child, stdout, stderr) = match spawn_dsh() {
                Ok(handles) => handles,
                Err(e) => {
                    let detail = format!(
                        "Failed to launch the `dsh` CLI process.\n\nIs `dsh` installed and available on your PATH?\n\nUnderlying error: {e}"
                    );
                    eprintln!("[dsh-tauri] FATAL: {detail}");
                    report_startup_failure(&app_handle, "DeepSeek Harness could not start", &detail);
                    return Ok(());
                }
            };
            eprintln!("[dsh-tauri] dsh web process spawned (PID {})", child.id());

            {
                let state = app.state::<DshProcess>();
                *state.0.lock().unwrap() = Some(child);
            }

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
                // One global deadline covers both the URL wait and the port
                // wait. A helper thread forwards stdout lines through a
                // channel so the wait can time out for real: `reader.lines()`
                // blocks forever when dsh prints nothing, so a deadline
                // checked around that blocking read never actually fires.
                let deadline = Instant::now() + DSH_READY_TIMEOUT;
                let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
                std::thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(l) => {
                                eprintln!("[dsh stdout] {l}");
                                // A send error means the waiting side is gone
                                // (URL found or gave up): stop draining.
                                if line_tx.send(l).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                eprintln!("[dsh-tauri] Error reading dsh web stdout: {e}");
                                return;
                            }
                        }
                    }
                });

                // Consume forwarded lines until the authenticated URL (which
                // carries the per-process `?token=...`) shows up, the deadline
                // expires, or stdout closes because dsh exited.
                let authenticated_url = loop {
                    match line_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(line) => {
                            if let Some(url) = extract_dsh_web_url_line(&line) {
                                eprintln!("[dsh-tauri] Extracted authenticated URL: {url}");
                                break Some(url);
                            }
                            // Not the URL line — keep waiting.
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            eprintln!(
                                "[dsh-tauri] Timed out after {}s waiting for the dsh web URL on stdout",
                                DSH_READY_TIMEOUT.as_secs()
                            );
                            break None;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            eprintln!("[dsh-tauri] dsh web closed stdout before printing the URL");
                            break None;
                        }
                    }
                };

                let Some(authenticated_url) = authenticated_url else {
                    // Fatal: dsh never reported a URL. Kill the wedged (or
                    // already dead) process and show the reason instead of
                    // leaving a silent white window.
                    eprintln!(
                        "[dsh-tauri] FATAL: no dsh web URL within {}s",
                        DSH_READY_TIMEOUT.as_secs()
                    );
                    kill_dsh(&navigate_handle.state::<DshProcess>());
                    report_startup_failure(
                        &navigate_handle,
                        "DeepSeek Harness server did not start",
                        "The `dsh web` process never reported its web address.\n\nPossible causes:\n  - the `dsh` CLI crashed at startup\n  - port 3080 is already taken by another program\n  - the server needed longer than the 30 second startup budget\n\nRun `dsh web --no-open` in a terminal to see the underlying error.",
                    );
                    return;
                };

                // The URL is printed once the server is listening (or very
                // shortly after), but wait for the port to be reachable so we
                // don't race the TCP listen socket. What remains of the global
                // deadline bounds this wait too.
                if !wait_for_port(
                    DSH_HOST,
                    DSH_PORT,
                    deadline.saturating_duration_since(Instant::now()),
                ) {
                    eprintln!("[dsh-tauri] Port {DSH_PORT} never became reachable. Giving up.");
                    kill_dsh(&navigate_handle.state::<DshProcess>());
                    report_startup_failure(
                        &navigate_handle,
                        "DeepSeek Harness server did not start",
                        "The `dsh web` process reported its web address, but the port never became reachable.\n\nRun `dsh web --no-open` in a terminal to see the underlying error.",
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