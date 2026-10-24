# Vox rewrite: fixed decisions

This is the brief for the Rust rewrite of Vox (originally Electron + React + TypeScript in
`C:\Users\Kwaku\Desktop\Desktop Widget`). The behaviour to reproduce is written up per subsystem in `docs/port/*.md`.
The new look is in `docs/DESIGN.md`. This file lists the decisions that are already made.

## Goals

1. **Feature parity** with the Electron app, as described in `docs/port/*.md`, unless this brief says otherwise.
2. **The new visual design** from `docs/DESIGN.md`. Nothing from the old Nothing-OS look (dot-matrix fonts, Doto,
   the red accent) carries over. Behaviour, copy and information architecture do carry over.
3. **Native and light.** One `vox.exe`, no web view, no Node at runtime. Idle CPU near zero; memory well under
   Electron's.
4. **Profile compatibility.** The Rust app uses the same data folder (`%APPDATA%\Vox`, demo: `%APPDATA%\Vox-Demo`)
   and reads the existing settings, history and audio cache files as they are. It migrates the API key from the
   Electron `safeStorage` format on first run (see `docs/port/persistence.md`). Its own writes stay
   readable by the same schema, so going back to the Electron build keeps working where practical.
5. **The browser extension is unchanged.** `browser-extension/` is copied as-is. The Rust bridge must be
   wire-compatible with it (`docs/port/integrations.md`).

## Toolchain and crates

- Rust stable 1.98 (MSVC), edition 2021, Windows 10/11 x64 only. `#![windows_subsystem = "windows"]` on the binary
  in release builds.
- UI: **Slint `=1.18.1`** with `default-features = false` and features `std`, `backend-winit`, `renderer-skia`,
  `compat-1-2`, `accessibility`, `unstable-winit-030`, `image-default-formats`. `slint-build = "=1.18.1"` with
  style `fluent`. (Verified on this machine: Skia rendering, transparent frameless always-on-top windows,
  `take_snapshot`, `run_event_loop_until_quit` with every window hidden, and setting `WS_EX_NOACTIVATE` through
  `slint::winit_030::WinitWindowAccessor` once the event loop is running. Note `with_winit_window` returns `None`
  until the event loop has created the native window, so do that from a `slint::Timer` or after the first frame.)
- Tray: `tray-icon = "0.26"` (with its `muda` menus), created on the main thread; the Slint/winit loop pumps the Win32
  messages it needs. Events arrive on tray-icon's channels and are forwarded to the core.
- Win32: `windows = "0.62"` (pick only the features you need), `winreg` for the registry.
- Audio: `symphonia` (MP3 decode, pure Rust), `cpal` (WASAPI output). A pure-Rust **WSOLA time-stretch** for
  speed without pitch change (0.75×–2×), plus resampling to the device rate (`rubato` or a good-quality
  polyphase/cubic resampler).
- HTTP client: `ureq = "3"` (blocking, rustls) on worker threads. JSON: `serde`, `serde_json`.
- HTTP server for the browser bridge: `tiny_http`.
- Art: `tiny-skia` for the procedural silk thumbnails and the backdrop.
- Logging: `tracing` + `tracing-subscriber` writing `%APPDATA%\Vox\logs\main.log` with 1 MB rotation, API key masked,
  no read text in logs.
- Errors: `thiserror` in library crates, `anyhow` in the app crate.
- Time: `time` or `chrono` (pick one for the whole workspace) with local-time-zone support, because analytics
  attributes listening to local days.

## Workspace layout

```
vox-2/
  Cargo.toml                 workspace
  crates/
    vox-core/                types and contracts shared by everything: settings schema + defaults, history
                             records, player snapshot, read requests/sources, events, constants, ledger helpers,
                             formatting helpers. No I/O, no Windows APIs. Everyone depends on it.
    vox-text/                text clean-up, chunking, sentence/word segmentation, alignment-to-word timing,
                             file text extraction (.txt .md .markdown .log .csv .json .rtf .html .htm)
    vox-store/               paths, atomic JSON files, settings store, secrets (DPAPI + Electron migration),
                             history store, audio cache, analytics (pure computations), demo data, log setup
    vox-tts/                 ElevenLabs client (TTS with timestamps, voices, models, user/subscription),
                             TtsEngine with bounded look-ahead and cancellation, mock synthesiser
    vox-audio/               decoder, time-stretch, resampler, output device, Player (chunk queue, position,
                             seek, pause, speed, volume, buffered progress), output warmer
    vox-win/                 global low-level hooks thread, selection gesture state machine, clipboard
                             round-trip, foreground app + sensitive/private/fullscreen detection, SendInput,
                             global hotkeys (RegisterHotKey thread, reserved + AltGr guards), media keys / SMTC,
                             Explorer verb + launch-at-login registry, single instance + argv forwarding (named
                             pipe), window helpers (no-activate, tool window, work areas, cursor, DWM colours)
    vox-bridge/              localhost HTTP bridge for the browser extension
    vox-art/                 procedural silk art (seeded thumbnails, backdrop) rendered with tiny-skia
    vox-app/                 the `vox.exe` binary: core thread (ReadController and orchestration), Slint UI
                             glue, tray, windows (main, widget, chip, popup), settings reactions, CLI
  ui/                        all .slint files (compiled by vox-app's build.rs; previewed by vox-shot)
    design/                  Theme tokens + components from DESIGN.md
    pages/                   Home, History, Analytics, Voices, Settings (+ sections), Onboarding
    windows/                 MainWindow, WidgetWindow, ChipWindow
    models.slint             the structs the Rust side fills (the UI data contract)
    icons/                   Lucide SVGs (tools/icons/extract-lucide.mjs adds more)
    previews/                *.json demo data for vox-shot, one per page/state
  tools/vox-shot/            renders any .slint component + JSON data to PNG (already built)
  browser-extension/         copied unchanged from the Electron project
  docs/                      BRIEF.md, DESIGN.md, ARCHITECTURE.md, port/*.md
```

Library crates must build and test on their own (`cargo test -p vox-text`) so they can be written in parallel.
Only `vox-app` depends on Slint.

## Threads and data flow

- **Main thread**: the Slint event loop, all Slint windows, tray-icon. It holds no business logic. It renders
  `vox-core` view-state pushed from the core and sends `Command`s to the core.
- **Core thread** (`vox-app/src/core`): owns the ReadController and the services (settings, history, cache, TTS
  engine, player handle). It receives `Command`s on a `crossbeam-channel` from every source (UI, tray, hotkeys,
  hook gestures, bridge, CLI/second instance) and publishes state to the UI through `slint::invoke_from_event_loop`
  (throttled for high-rate player updates, about 30 Hz while playing).
- **Hook thread** (vox-win): installs `WH_MOUSE_LL` / `WH_KEYBOARD_LL` and runs its own message loop. Hook
  callbacks do almost nothing: they push raw events to the gesture state machine thread so they never block input.
- **Hotkey thread** (vox-win): a message-only window with `RegisterHotKey`.
- **Bridge thread(s)** (vox-bridge), **TTS workers** (vox-tts), **audio callback** (cpal, lock-free reads of the
  current buffer).
- In the Electron app the widget window owned playback (`<audio>`). Here the **core owns playback**; the widget is
  only a view, so turning the widget off never affects audio.

## Windows

- **Main window**: normal decorated window (resizable, snaps), dark title bar via
  `DWMWA_USE_IMMERSIVE_DARK_MODE` and `DWMWA_CAPTION_COLOR` matching the backdrop. Close hides to tray when
  "close to tray" is on.
- **Widget window**: no frame, transparent background, always on top, `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`
  (never steals focus, no taskbar button), exactly the card's size, positioned by the placement rules in
  `docs/port/shell.md`, draggable by its non-control areas (winit `drag_window`), auto-hide rules from the spec.
- **Chip window** and **selection popup**: same treatment as the widget; placement rules from the spec.

## Quality bar

- `cargo clippy --workspace -- -D warnings` clean, `cargo test --workspace` green.
- Every pure algorithm (text chunking/clean-up, gestures, ledger/analytics, hotkey validation, time-stretch,
  bridge request checks, settings migration) has unit tests, including the edge cases the specs call out.
- No `unwrap()` on I/O or user input in non-test code. No panics across FFI callbacks (hook procs catch panics).
- Logs never contain the API key or the text being read.

## Product rulings on the specs' open questions

These answer the "open questions" and "decide deliberately" items in `docs/port/*.md`. Anything not listed here is
the architect's call, recorded in the decisions log of `docs/ARCHITECTURE.md`.

- **UI is native Slint, in-process.** IPC, sender checks, webview hardening, drag-region lag fixes, the 38-DIP
  minimum window height and the readiness latch disappear. The requirements behind them stay: no lost clicks, no
  focus stealing, the player gets `sessionStart` before chunks, toasts raised before the UI is up are not lost.
- **Text offsets**: internal text handling may use `char`/byte indices, but every offset that is persisted
  (history `charCount`, audio-cache manifests `textStart`/`textEnd`) or that crosses into old files stays in
  **UTF-16 code units**, converted at the store boundary. Existing caches must keep replaying.
- **Profile**: use `%APPDATA%\Vox` in place (no copy). Keep `settings.json` / `history.json` / cache manifests
  format-compatible (field names, value types, omitted optionals). Byte-identical number formatting is not
  required. Corrupt main file + good `.bak`: rewrite the main file right away and log it.
- **API key**: migrate the Electron safeStorage secret on first run if it can be decrypted (`Local State`
  `os_crypt.encrypted_key` → DPAPI → AES-256-GCM `v10` blob). If migration fails, ask for the key again (onboarding
  / Settings → Account) with a clear message. Store the key afterwards as a plain DPAPI blob in the new file
  `secrets-dpapi.bin` and leave `secrets.bin` untouched, so the Electron build still works if the user goes back.
- **Do not run both builds at once.** If an Electron Vox instance is running, the Rust app shows a native
  message ("Close the old version of Vox first") and exits.
- **Media keys** apply live through SMTC; drop every "restart Vox" toast and "takes effect after restart" copy.
  Media keys keep working while the widget is turned off.
- **Hotkeys** register with `MOD_NOREPEAT`. Keep Electron's meaning of existing accelerator strings in
  `settings.json` (`Plus` = Shift+`=`). Treat an `AltGr` token as Ctrl+Alt.
- **Network**: honour the Windows system proxy and use the Windows certificate store (rustls platform verifier).
  User-Agent `Vox/<version> (Windows)`. Keep the non-streaming `with-timestamps` request and the 220-char first
  chunk. **Do not retry timeouts** (they may already be billed); retry only what is safe. Remember a rejected
  optional parameter (`language_code`, context) for the rest of the session. Count billed characters for
  chunks that were requested and returned audio, including ones that arrive after a stop.
- **Text cleanup**: keep the old rules for parity, but fix clear bugs: decode HTML entities, keep the hyphen in
  `well-\nknown` when both halves are words, strip `mailto:` case-insensitively, drop autolink brackets.
- **Audio**: gapless playback between chunks. Speed changes preserve pitch. A player failure restarts the player
  and stops the engine with an error card.
- **Clipboard**: snapshot and restore every format the old app restored, plus `CF_HDROP` and `CF_DIB`. Mark restore
  writes with `CanIncludeInClipboardHistory = 0` and `ExcludeClipboardContentFromMonitorProcessing`. Detect UIPI
  (target app elevated, Vox not) and show "Vox can't read from apps running as administrator".
- **Hook**: if installing the hook fails, show the Triggers notice and retry when trigger settings change.
- **DPI**: the app is per-monitor-v2 DPI aware and uses physical pixels for window placement. Saved
  `widget.customX/Y` (Electron DIPs) are converted on load by mapping through the monitor containing the point;
  if that fails, re-clamp to the nearest work area.
- **Main window** remembers its size, position and maximized state (new file `window-state.json`).
- **Widget z-order**: topmost, above normal windows; the chip is topmost too.
- **Tray**: left click (and keyboard select) opens the main window. Double click is the same as left click.
- **Copy**: the redesign rewrites copy that describes the old look (Appearance notes, dot-matrix references).
  Apostrophes are curly everywhere (’). Model names use one style everywhere ("Flash v2.5"). About shows the app
  version and build hash instead of "Electron". The three-way theme choice (System / Paper / Night) stays.
- **Analytics**: debounce refreshes (700 ms) like Home and History; "All time" uses one consistent window; the
  empty banner checks listening time as well as reads. "This week" stays a rolling 7 days, labelled
  "Last 7 days".
- **Home**: a failed first load shows an inline error with Retry instead of endless skeletons. A paused read on
  Recent offers Resume.
- **Formatting** follows the Windows user locale for dates and numbers where the old app used `Intl` defaults.
- **Installer**: out of scope for the first milestone; produce a portable `vox.exe` and keep the extension folder
  next to it (`browser-extension\`). An installer comes later.
- **Demo mode** stays (`VOX_DEMO=1`, `%APPDATA%\Vox-Demo`, seeded history) and so does `VOX_MOCK_TTS=1`. They
  drive the screenshot QA of the real app. Demo data needs the same shape and distribution, not identical values.
