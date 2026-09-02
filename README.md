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
npm run build    # tauri build — produces a native installer/binary
```

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