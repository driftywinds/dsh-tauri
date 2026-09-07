# DeepSeek Harness — Desktop App

A tiny Tauri v2 wrapper around the DeepSeek Harness web UI.

**What it does:**
1. On launch, spawns `dsh web` as a child process.
2. Polls `127.0.0.1:3080` until it's reachable (30s timeout).
3. Navigates the (initially hidden) app window to `http://localhost:3080` and shows it.
4. When the window is closed (or the app quits), it kills the `dsh` child process.

## Prerequisites

- **Rust + Cargo (1.77 or newer)** — install/update via [rustup](https://rustup.rs), `rustup update` if you already have it
- **Node.js** (for the Tauri CLI, via `npm`/`npx`)
- **Tauri system dependencies** for your OS — see the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/) (on Linux this means `webkit2gtk`, `libayatana-appindicator`, etc.)
- **`dsh` installed and available on your `PATH`** (the app just shells out to `dsh web`). This is required — the app will panic at startup if `dsh` isn't found.

## Project layout

```
deepseek-harness-app/
├── package.json              # just enough to run the Tauri CLI
├── dist/index.html           # placeholder — never actually shown; window
|                              # loads http://localhost:3080 once dsh is up
└── src-tauri/
    ├── Cargo.toml
    ├── build.rs
    ├── tauri.conf.json       # window starts hidden with url "about:blank"
    ├── capabilities/default.json
    ├── icons/                # placeholder icons (see note below)
    └── src/main.rs           # all the logic lives here
```

## Setup & running

```bash
npm install
npm run dev      # tauri dev — spawns dsh, opens the window once it's up
npm run build    # tauri build — produces the release binary
```

## Portable exe (Windows)

The app is built as a standalone `.exe` that does **not** require installation.
After `npm run build`, the portable executable is at:

```
src-tauri/target/release/deepseek-harness.exe
```

Copy that `.exe` to any folder on any Windows 10 (April 2018 update or later)
or Windows 11 machine and double-click it — no installer, no admin rights,
no registry changes. The frontend is embedded in the binary, so only the `.exe`
is needed.

**Prerequisites on the target machine:**
- Windows 10 (April 2018 update / build 1809 or later) or Windows 11 — these
  ship with the WebView2 runtime pre-installed.
- `dsh` must be installed and available on the PATH (the app shells out to
  `dsh web` at startup).

If you need to support older Windows versions or offline machines, set
`bundle.windows.webviewInstallMode.type` to `"fixedRuntime"` in
`tauri.conf.json` and place the WebView2 fixed-version runtime next to the
`.exe` (see the [Tauri Windows installer docs](https://v2.tauri.app/distribute/windows-installer/)).

To also produce installers (MSI/NSIS), set `bundle.active` back to `true`.
The portable `.exe` is always built regardless of that setting.

## Icons

The icons under `src-tauri/icons/` are placeholder PNGs (plus a generated
`.ico` for Windows) so `tauri build` doesn't immediately fail on missing
icon files. There's no `icon.icns` (macOS), since that format can't be
generated outside of macOS in this environment. Before shipping, regenerate
proper icons from a real source image with:

```bash
npx tauri icon path/to/your-source-icon.png
```

This will produce all required platform formats, including `icon.icns`.

## Notes / things to double check on your machine

- **Toolchain**: This project was written and reviewed against the Tauri v2
  API, but could not be compiled in the sandbox this was built in (only an
  old `rustc 1.75` from `apt` was available there, and current crates.io
  dependencies now require the `edition2024` Cargo feature, which needs
  Rust 1.77+). Run `cargo check` inside `src-tauri/` on your own machine
  once you have a modern toolchain — that's the real compile check.
- **`dsh` on PATH**: `main.rs` calls `Command::new("dsh").arg("web")` and
  will `panic!` with a clear message at startup if `dsh` isn't found. If
  `dsh` isn't on the PATH Tauri sees (e.g. GUI-launched apps on macOS can
  have a different PATH than your terminal), swap in an absolute path.
- **Timeout**: if `dsh web` takes longer than 30s to come up, the window
  stays hidden and an error is printed to stderr. Bump
  `DSH_READY_TIMEOUT` in `main.rs` if needed.
- **Permissions**: `capabilities/default.json` is intentionally minimal
  (just `core:default` plus window show/close). Add more capabilities
  only if you add features that need them.
- **Dragging the frameless window**: since `decorations` is `false`,
  there's no OS titlebar to grab, so `main.rs` injects a small script
  (`DRAG_REGION_SCRIPT`) that starts a window drag on left-mousedown
  anywhere in the page (except on links/buttons/inputs/etc., or elements
  marked `class="no-drag"` / `-webkit-app-region: no-drag`). Three things
  have to line up for this to actually work:
  - `"withGlobalTauri": true` in `tauri.conf.json`, so `window.__TAURI__`
    exists in the page's JS. Note this exposes the Tauri JS API to
    whatever `dsh web` serves at `localhost:3080` — fine here since `dsh`
    is your own trusted process, but worth knowing.
  - `"core:window:allow-start-dragging"` in `capabilities/default.json`,
    since Tauri blocks every command by default until it's explicitly
    permitted.
  - The script has to be a real *initialization script*
    (`initialization_script(...)`), not a one-off `window.eval(...)`.
    `eval` only affects whatever page happens to be loaded at that
    instant; since this app navigates from `about:blank` to
    `http://localhost:3080`, an eval'd listener on the first page is
    gone the moment that navigation happens. An initialization script is
    re-injected before every page load instead, so it survives the
    navigation.
  - Because the window needs a custom initialization script, it's built
    by hand in `setup()` via `WebviewWindowBuilder::from_config(...)`
    rather than left for Tauri to auto-create — that's what the
    `"create": false` on the window entry in `tauri.conf.json` is for.
  - **Windows/WebView2 specifically**: Tauri's native OS-level drag-drop
    handler (for dropping files onto the window) is *on* by default, and
    it intercepts drag-related mouse events before they reach the
    webview's JS — silently eating the mousedown→drag sequence
    `startDragging()` needs. `main.rs` calls `.drag_and_drop(false)` on
    the window builder to turn that off. `"dragDropEnabled": false` is
    also set in `tauri.conf.json` for documentation purposes, but note
    it has no effect by itself for windows built via
    `WebviewWindowBuilder::from_config` (a known Tauri bug) — the
    `.drag_and_drop(false)` builder call is what actually matters.
  - **The big one — remote origin ACL**: Tauri v2's permission system
    checks the *origin of the page currently loaded in the webview*, not
    just the window label. `about:blank` (the initial placeholder) counts
    as a trusted local origin, but once the window navigates to
    `http://127.0.0.1:3080`, that's a genuine remote origin as far as
    Tauri's ACL is concerned. Without explicitly allow-listing it, Tauri
    silently blocks *every* IPC call — including `start_dragging` — made
    from that page, regardless of what's in `permissions`. That's what
    the `"remote": { "urls": [...] }` block in
    `capabilities/default.json` is for: it extends this capability's
    permissions (drag included) to pages loaded from
    `http://127.0.0.1:3080`. **Note it's `127.0.0.1`, not `localhost`** —
    Tauri's ACL treats them as different origins even though they
    resolve to the same machine, and it has to match whatever `DSH_HOST`
    in `main.rs` actually is exactly. If you ever change `DSH_HOST` or
    `DSH_PORT` in `main.rs`, update this URL list to match, or dragging
    (and any other IPC call) will silently stop working again — check
    the WebView2 devtools console (right-click → Inspect) for a
    `not allowed on window ...` message if that happens.