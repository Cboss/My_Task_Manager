# Overseer

A fast, keyboard-first Windows task manager. Rust + Tauri 2 backend, plain HTML/JS frontend (no Node required).

## Setup (one time)

1. **MSVC C++ build tools** (Rust needs the Windows linker):
   `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
2. **Rust**: `winget install Rustlang.Rustup`, then open a new terminal.
3. WebView2 is already part of Windows 11.

## Run

```
cd overseer/src-tauri
cargo run              # dev build (first build takes a few minutes)
cargo run --release    # optimised build
```

To produce an installer: `cargo install tauri-cli --version "^2"` then `cargo tauri build` (from `overseer/src-tauri`).

To tweak the UI without Rust, open `ui/index.html` through any static server: it falls back to mock data.

## Features

- Live CPU / memory / disk / network graphs with history, plus a per-core heat strip
- Process table with heat-mapped cells, sortable columns, instant filter by name or PID
- Three views: **List**, **Tree** (parent/child, collapsible, PID-reuse safe), **Grouped** (e.g. chrome.exe ×40 summed)
- Details panel: path, command line, working dir, parent (clickable), start time, priority
- End task, **end whole process tree**, end all in a group, set priority, open file location
- Admin badge with one-click "restart elevated"

## Keys

`/` or `Ctrl+F` search · `1` `2` `3` switch view · `↑` `↓` move selection · `Del` end task · `Shift+Del` end tree · `Space` pause · `Esc` clear

## Layout

- `src-tauri/src/main.rs`: sampling (sysinfo) and Tauri commands
- `src-tauri/src/win.rs`: Win32 calls (priority, elevation, Explorer)
- `ui/`: index.html, style.css, app.js

## Next ideas

Per-process network (ETW), GPU per process, startup apps and services tabs, alert rules ("kill X if RAM > 4 GB"), signature check, and a tray icon with a global hotkey.
