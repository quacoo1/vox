# Vox shell: bootstrap, windows, tray, hotkeys, installer

Porting spec for the "shell" subsystem of Vox (select text anywhere, ElevenLabs reads it aloud, a floating player
shows playback). It covers process lifecycle, the three windows, the tray, global hotkeys, reactions to settings
changes, the screenshot harness and the Windows installer.

Source of truth (TypeScript, Electron 44): `C:/Users/Kwaku/Desktop/Desktop Widget`. Citations are `path:line`
relative to that root. Files in scope: `src/main/index.ts`, `src/main/windows.ts`, `src/main/tray.ts`,
`src/main/shortcuts.ts`, `src/main/shots.ts`, `electron-builder.yml`, `resources/installer.nsh`. Behaviour of
neighbouring subsystems (reader, selection service, stores, bridge, explorer verb, autostart) is described only as
far as the shell calls it. Behaviour that comes from the platform rather than this repo is cited from the installed
electron-builder 26.15.3 NSIS templates (`node_modules/app-builder-lib/templates/nsis/...`) or, where marked
"Electron-internal", from Electron's own C++ (not in the repo; verify if exact parity matters).

Conventions in this document:

- **DIP** = device-independent pixels (1 DIP = 1 px at 96 DPI). Every geometry constant below is in DIP, as in the
  original. A native port must convert with the target monitor's scale (`GetDpiForMonitor / 96`) and be
  Per-Monitor-V2 DPI aware.
- **Work area** = a monitor's rectangle minus the taskbar and app bars (`MONITORINFO.rcWork`).
- **"Main"** = the core process/thread that owns windows, tray and hotkeys. **"UI"** = whatever renders a window's
  content (React in the original).
- `APP_NAME = "Vox"`, `APP_ID = "com.vox.reader"`, `READ_WITH_LABEL = "Read with Vox"`
  (`src/shared/constants.ts:2-4`). Every user-facing string that says "Vox" is built from `APP_NAME`.

---

## 1. Runtime flags, environment variables, paths

### 1.1 Environment variables (`src/main/log.ts:10-34`)

Evaluated once, before anything touches the user-data folder (the logger module is deliberately the very first
import of the entry point, `src/main/index.ts:1-3`, because demo mode moves userData and must do so before any other
module or Chromium reads it).

A flag is "on" when the variable's value is exactly `"1"` or `"true"` (`log.ts:20-23`).

| Variable | Effect |
|---|---|
| `VOX_SHOTS_DIR=<dir>` | Screenshot harness mode (section 9). Any **non-empty** value enables it (not the `"1"`/`"true"` rule); `shotsDir = path.resolve(value)` (relative to the process cwd). Implies demo. |
| `VOX_DEMO` | Demo mode: separate profile folder `%APPDATA%\Vox-Demo` (`<appData>\<APP_NAME>-Demo`), seeded history, onboarding marked done. Implies mock TTS. |
| `VOX_MOCK_TTS` | No network TTS (quiet test tone). `mockTts = demo OR VOX_MOCK_TTS`. |
| `VOX_DEBUG` | Debug-level log lines are written (otherwise dropped). |
| `ELEVENLABS_API_KEY` | Fallback API key when no stored key decrypts (settings store, `settings-store.ts:241-242`). |
| `ELECTRON_RENDERER_URL` | Dev only (unpackaged): load UI pages from a dev server instead of files. |

Derived runtime record:

```ts
interface Runtime {
  demo: boolean        // VOX_DEMO || shotsDir !== null
  mockTts: boolean     // demo || VOX_MOCK_TTS
  shotsDir: string | null
  debug: boolean       // VOX_DEBUG
}
```

What each mode switches off (collected from `index.ts`):

| Behaviour | demo | shots |
|---|---|---|
| userData = `%APPDATA%\Vox-Demo` | yes | yes |
| Seed demo history when history is empty; set `onboarded = true` | yes (`index.ts:432-435`) | yes |
| Launch-at-login registry writes | skipped (`index.ts:177`) | skipped |
| Explorer verb sync | skipped (`index.ts:190`) | skipped |
| Bridge "port in use" toast | suppressed (`index.ts:282`) | n/a (bridge off) |
| Browser bridge server | runs | **off** (`index.ts:268`) |
| Global hotkeys | registered | **never** (`index.ts:155`) |
| Selection service (global input hook, chips) | runs | **not started** (`index.ts:298, 359`) |
| Main window shown at start | normal rule | **never** (`index.ts:467`) |
| Chromium window-occlusion tracking | normal | disabled (`index.ts:521`) |

### 1.2 Paths

| What | Path |
|---|---|
| userData (normal) | `%APPDATA%\Vox` (Roaming; Electron default = appData + product name) |
| userData (demo/shots) | `%APPDATA%\Vox-Demo` |
| Chromium session data | `<userData>\Session` (`index.ts:533-536`). Reason: Chromium's HTTP cache defaults to `<userData>\Cache`, which on case-insensitive Windows is the same folder name space as the app's own caches; the audio cache's init deletes anything in its folder that is not a read. (The audio cache now lives in `<userData>\audio-cache` for the same reason, `audio-cache.ts:55-57`.) A WebView2-based port must likewise give the web engine its own subfolder and never let it share `audio-cache`. |
| Settings | `<userData>\settings.json` (+ `.bak`), API key `<userData>\secrets.bin` (DPAPI) |
| History | `<userData>\history.json` |
| Logs | `<userData>\logs\main.log`, rotated to `main.1.log` at 1 MiB |
| Icons at runtime | packaged: `<installDir>\resources\resources\{icon.ico,tray.ico,tray-active.ico,...}`; dev: `<appPath>\resources\...` (`windows.ts:368-372`) |
| Unpacked browser extension | packaged: `<installDir>\resources\browser-extension`; dev: `<appPath>\browser-extension` (`ipc.ts:239-243`) |

### 1.3 Logging (`src/main/log.ts:36-128`)

- Line format: `${ISO-8601 UTC timestamp} ${LEVEL padded to 5} [${scope}] ${text}\n`, e.g.
  `2026-10-17T09:12:03.456Z INFO  [app] Vox 0.1.0 starting (...)`. Also echoed to stdout/stderr.
- Args are joined with spaces: Error → stack (or `name: message`); string as-is; `undefined` → `"undefined"`;
  anything else `JSON.stringify`, cut to 2000 chars + `…`; unserialisable → `String(x)`.
- Redaction applied to the whole line (last line of defence, never log text being read or keys):
  ```
  /\bsk_[A-Za-z0-9]{16,}\b/g                    -> "sk_***"
  /(xi-api-key["':\s=]+)[^\s"',}]+/gi           -> "$1***"
  ```
- Rotation: before appending, if `currentSize + lineBytes > 1_048_576`, delete `main.1.log`, rename `main.log` to
  `main.1.log` (failures ignored: another process may hold the file; keep appending), reset size to 0.
- Logging must never throw; if the log directory cannot be created, file logging is disabled for the session.
- The log path is resolved lazily at the **first write** (so the demo-mode userData redirect has already happened),
  and the running size counter starts from the existing `main.log` size (0 if absent). Console echo: `error` →
  stderr, `warn` → stderr, `info`/`debug` → stdout (the line without its trailing newline). Debug lines are dropped
  before both echo and file when `VOX_DEBUG` is off.
- Scopes used by the shell: `app`, `bridge`, `windows`, `tray`, `shortcuts`, `shots` (the bridge server's own
  messages are forwarded into the `bridge` scope at their level).

---

## 2. Command line (`src/main/reader.ts:67-104, 454-478`; `src/main/index.ts:47-51, 332-336`)

### 2.1 Switches

| Switch | Meaning |
|---|---|
| `--hidden` | Start in the tray (main window not shown). Written by launch-at-login when "start minimized" is on. |
| `--read-clipboard` | Read the clipboard's text aloud. |
| `--read-text <text>` / `--read-text=<text>` | Read `<text>` aloud (source `cli`). |
| `--read-file <path>` / `--read-file=<path>` | Read a text file (source `explorer`). Used by the Explorer verb: `<exe> --read-file "%1"`. |

Parsing algorithm (`parseCliArgs`), over the **full** argv (exe path, dev app path and Chromium-added switches
included; they are ignored):

```
out = { hidden: false, readFile: null, readClipboard: false, readText: null }
for i in 0..argv.len:
  arg = argv[i]
  if !arg.starts_with("--"): continue
  eq = arg.find('=')                         // only counts when eq > 0
  name   = eq > 0 ? arg[..eq] : arg
  inline = eq > 0 ? arg[eq+1..] : None
  value() = inline if Some, else (if i+1 < len { i += 1; argv[i] } else None)
  match name:
    "--hidden"         -> hidden = true
    "--read-clipboard" -> readClipboard = true
    "--read-file"      -> readFile = value() or None if empty
    "--read-text"      -> readText = value() or None if empty
    _                  -> ignore (unknown switches never consume a value)
```

Edge cases worth preserving: names are case-sensitive; `--read-text --hidden` makes `"--hidden"` the text (the next
element is consumed whatever it looks like); `--read-text=` gives `null` (an inline empty value does **not** consume
the next element); a trailing `--read-file` gives `null`; `--hidden=x` / `--read-clipboard=x` still count (the name
is cut at `=`). Unknown switches are ignored, including `--updated`, which the installer's "Run Vox" finish-page
checkbox passes after an upgrade (section 10), and positional arguments (the exe path, the dev app path).

### 2.2 Acting on the arguments (`handleCli`, `handleArgs`)

Only one action runs per invocation, in priority order `readText` > `readClipboard` > `readFile`:

1. `readText` → `reader.read({ text, source: "cli" })`. Returns "acted".
2. `readClipboard` → `reader.readClipboard()` (empty clipboard → info toast `Copy some text first`). "acted".
3. `readFile` → resolve against the **invoking** process's working directory (`path.resolve(cwd, readFile)`),
   extract text reading at most `settings.text.maxChars + 1000` characters (a little past the limit so the reader can
   say it truncated), then `reader.read({ text, source: "explorer", sourceApp: "Explorer", sourceProcess:
   "explorer.exe", sourceTitle: <file name> })`. Any failure (open, decode, or the read itself throwing) → warn
   `read-file failed` and error toast `Couldn't open <file name>` (`<file name>` = base name of the resolved path).
   "acted" either way.
4. Otherwise "not acted".

`handleArgs(args, cwd, fromSecondInstance)` (`index.ts:332-336`): no-op while the reader does not exist (bootstrap
failed or not finished); run `handleCli`; if nothing was acted on **and** the call came from a second instance **and**
`--hidden` was not given → show the main window (default route). So launching Vox again from the Start menu brings
the existing window forward; a second `Vox.exe --hidden` does nothing at all. In shots mode the initial arguments
are never handled (bootstrap returns before `handleArgs`, 3.2 step 16).

---

## 3. Process lifecycle

### 3.1 Before the UI toolkit is ready (`main()`, `index.ts:532-585`)

Strict order:

1. (Module load) Detect runtime flags (1.1); if demo, redirect userData. Parse `initialArgs` from the process argv
   (`index.ts:47`).
2. Set the web engine's session-data dir to `<userData>\Session`.
3. Apply engine switches that can only be set before startup (`applyChromiumSwitches`, `index.ts:516-530`):
   - `disable-lcd-text`: grayscale text antialiasing everywhere. Reason: ClearType's coloured subpixel fringes break
     up the "Doto" dot-matrix font's glyphs. **Native port: render all text with grayscale AA**
     (DirectWrite/Direct2D `D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE`).
   - In shots mode: disable feature `CalculateNativeWinOcclusion` (windows behind others must still paint).
   - Read `settings.json` synchronously (falls back to `settings.json.bak` only when the main file exists but is
     unreadable/corrupt; missing file → no settings; BOM stripped; sanitized against defaults). If
     `triggers.mediaKeys === false`, disable features `HardwareMediaKeyHandling` and `MediaSessionService`
     (otherwise Chromium lets hardware media keys and the Windows media overlay play/pause the widget's audio even
     without handlers, and lists the app in the overlay). Existing `disable-features` values are merged and
     de-duplicated.
4. **Single-instance lock** with additional data `{ argv: process.argv, cwd: process.cwd() }`. If the lock is not
   obtained, quit immediately (this process is a second instance; its argv is delivered to the first one, 3.3). None
   of the later handlers (before-quit etc.) are registered in a second instance, so it exits without any shutdown
   work. The lock is keyed on the userData directory, so a demo instance (`Vox-Demo`) and a normal instance can run
   side by side. (Steps 2-3 run before the lock, so a second instance also reads `settings.json`; harmless, but a
   native port should take the lock first.)
5. Set the process AppUserModelID to `com.vox.reader` (taskbar grouping, toast notifications; the installer's
   shortcuts carry the same ID).
6. Remove the application menu (no default accelerators such as reload/devtools/zoom in any window).
7. Install process handlers (`index.ts:55-60`): uncaught exceptions and unhandled promise rejections are **logged,
   not fatal**; `SIGINT`/`SIGTERM` (Ctrl+C in a dev terminal) → normal quit sequence so stores are flushed.
8. Harden web contents (3.8). In shots mode start the console collector before any window exists (section 9).
9. Start `bootstrap()` asynchronously. If it throws (e.g. a store's `init()` fails): log `bootstrap failed` and exit
   with code **1** immediately: a hard exit, no quit sequence, no flush, no before-quit. The "booted" promise still
   settles, so queued second-instance requests run `handleArgs`, which is a no-op without a reader.
10. Register `second-instance`, `window-all-closed`, `before-quit`, `will-quit` handlers (3.3, 3.5).

### 3.2 Bootstrap (`index.ts:421-513`)

Strict order:

1. Log `Vox <version> starting (electron <ver>, demo=<bool>, mockTts=<bool>)`.
2. Wait for the toolkit to be ready.
3. Harden the web session: every permission request/check is denied except `clipboard-sanitized-write`
   (async clipboard text writes, e.g. "copy text" in History) (`index.ts:77-82`).
4. `settingsStore.init()` (must be after ready: DPAPI/safeStorage), then `historyStore.init()` and
   `audioCache.init()` in parallel.
5. Subscribe: whenever history records are removed (manually, cleared, or by retention), remove each one's cached
   audio (errors logged as `cache remove failed`).
6. Demo only: if history is empty, add generated demo records (`seeded N demo records`); if not onboarded, set
   `onboarded: true`.
7. Retention and cache hygiene:
   - `applyRetention()` now: if `library.retentionDays > 0`, delete records older than that many days; log
     `retention removed N records` when N > 0.
   - `reconcileCache()` (`index.ts:107-121`): for every record, if `cached` but the cache has no audio for it →
     set `cached: false`; if the record is `textTruncated` and its cache manifest still holds text → redact that
     text (older versions kept the full text in the manifest even when only a preview was stored; fire-and-forget).
     Then, asynchronously, delete cache entries whose read id is not in history (`removed N orphaned audio cache
     entries` when N > 0; failure → warn `cache orphan cleanup failed`). Startup does not wait for this.
   - Every **6 h** (`RETENTION_INTERVAL_MS = 21_600_000`) run `applyRetention()`, unless a retention change is
     pending (5.2), so the periodic run never cuts the user's grace period short.
8. Create the TTS client (reads the API key lazily), the read controller (`mock = runtime.mockTts`), subscribe
   "player state changed → refresh tray", register all UI message handlers. Handlers exist before any window can
   send.
9. Snapshot `settings = settingsStore.get()`. Give the window module its dependencies (6.1). Apply the theme
   setting. Create the **widget** window, the **chip** window, then the **main** window with
   `show = startup visibility rule` (3.4) and route `onboarded ? home : onboarding`.
10. Create the tray (section 7).
11. Wait for the Win32 helper bindings (used for the AltGr check) for **at most 1500 ms**; a slow load must not hold
    up startup (`index.ts:488-489`). A load that loses the race keeps going in the background; once it finishes,
    every later hotkey registration uses it (8.4). A load failure resolves to "no helpers" (never throws). (In a
    native port `user32` is always there, so this wait disappears.)
12. Register global hotkeys from the snapshot (section 8). Wire and start the selection service (4.1).
13. Subscribe to settings changes (section 5). Note: settings changes before this point do not trigger the
    reactions.
14. Watch displays and OS theme (5.5, 5.6).
15. Start the browser bridge if enabled; sync the Explorer verb (non-forced); apply launch-at-login.
16. Shots mode: run the harness (errors logged `screenshot run failed`), then quit regardless of outcome; stop here
    (the initial CLI arguments are ignored).
17. `handleArgs(initialArgs, processCwd, fromSecondInstance = false)`. Bootstrap is "finished" only after this
    returns, so a second-instance request that arrives meanwhile waits until the first launch's own CLI action has
    been handled.

In step 15 the bridge start and the Explorer-verb sync are fire-and-forget (not awaited); launch-at-login is a
synchronous registry call. Their failure toasts go through the toast helper (5.3) and so wait for the widget to be
ready.

### 3.3 Second instance (`index.ts:555-560`)

When another launch fails the lock, the running instance receives `(argv, cwd, additionalData)`. Prefer
`additionalData.argv` (filtered to strings) and `additionalData.cwd` over the platform-supplied values (the raw
`process.argv` of the second process is passed through verbatim this way; the engine's own copy of the second
command line can be altered). `additionalData.argv` is used only when it is an array (non-strings dropped), else the
platform argv; `additionalData.cwd` only when it is a string, else the platform cwd. Wait until bootstrap has
finished, then `handleArgs(parseCliArgs(args), cwd, true)`. Several requests queued during startup start in arrival
order once bootstrap finishes, without waiting for each other.

Platform-level behaviour to reproduce: Chromium's process singleton uses a named mutex plus a hidden message window
found by the second process, sends the command line + cwd + additional data via `WM_COPYDATA`, and calls
`AllowSetForegroundWindow(<first pid>)` first so that the running instance may bring its window to the foreground
(otherwise Windows only flashes the taskbar button). The second process then exits.

### 3.4 Startup visibility rule (`index.ts:466-471`)

```
showMain = !shotsMode
        && !initialArgs.hidden
        && !(initialArgs.readText || initialArgs.readFile || initialArgs.readClipboard)
        && (!settings.app.startMinimized || !settings.onboarded)
route    = settings.onboarded ? { page: "home" } : { page: "onboarding" }
```

Onboarding always shows (even with "start minimized"), except under `--hidden`, a read switch, or shots mode. The
main window is created hidden in every case and shown on "ready to show" (first paint) only when `showMain`.

### 3.5 Quit sequence (`index.ts:398-417, 562-584`)

State: `isQuitting` (shared with the window module), `shuttingDown` (in-flight promise), `cleanupDone`.

On any quit request (tray Quit, UI `app:quit`, SIGINT/SIGTERM, main-window close with close-to-tray off, a close
request on the widget/chip, end of shots run):

```
before-quit:
  isQuitting = true                          // window close handlers stop intercepting
  if cleanupDone: allow the quit to proceed
  else:
    cancel this quit
    if shuttingDown: return                  // a second request just waits for the first
    shuttingDown = shutdown()                // errors logged as "shutdown failed"
      .finally:
        cleanupDone = true
        quit again                           // now proceeds
        after 4000 ms (FORCE_EXIT_MS), unref'd: hard exit(0)
will-quit: unregister every global hotkey
window-all-closed: if cleanupDone -> quit again; else ignore (tray app: closing windows never quits)
```

`shutdown()`:

1. Log `shutting down`.
2. Clear the retention interval, the pending retention-change timer and the hotkey-suspend timer.
3. Unregister our hotkeys, then unregister all global hotkeys.
4. Stop the selection service (errors logged `selection stop failed`).
5. Persist playback: fold every tracked session's latest listened time into history now (errors logged).
6. Stop synthesis (without touching the widget).
7. In parallel, bounded by **3000 ms** (`SHUTDOWN_TIMEOUT_MS`): flush settings, flush history, flush the selection
   service (completes a pending clipboard restore), stop the bridge server if running. Individual failures are
   ignored (all-settled). On timeout the flushes are **not** cancelled; they keep running until the process exits.
8. Destroy the tray icon.

The 4 s hard-exit timer is armed only after `shutdown()` has settled, so the worst case from the first quit request
to process exit is about 3 s + 4 s = 7 s.

Why `window-all-closed` re-quits only after cleanup: a close request racing the quit (`taskkill` or an installer
posting `WM_CLOSE` to every window) can make Electron treat its own quit as cancelled, and the app would linger
with no windows. The 4 s hard exit is the last resort: everything is saved by then, so a window that refuses to
close must not keep the process alive.

### 3.6 Session end, log-off, shutdown (`index.ts:388-396`, `windows.ts:417-428`)

Windows log-off/shutdown sends `WM_QUERYENDSESSION` then `WM_ENDSESSION` to every top-level window. There is no
graceful quit sequence and the process may be killed as soon as the window procedure returns. On **either** message
on **any** of the three windows: synchronously persist playback (its own errors logged `persisting playback
failed`, then continue), `settingsStore.flushSync()`, `historyStore.flushSync()` (any other error logged
`session-end save failed`). There is no de-duplication: the save runs for every message on every window (up to 6
times); the first call writes, repeats find nothing pending. Do not veto the session end, and do not start the quit
sequence from here.

Native port note: these messages are only sent to top-level windows. A message-only window (`HWND_MESSAGE`) never
receives them, nor broadcast messages such as `TaskbarCreated`. Keep at least one hidden top-level window.

### 3.7 Close requests per window (`windows.ts:430-444, 526-531`; `index.ts:456-461`)

| Window | On close request (user X, Alt+F4, external `WM_CLOSE`) while not quitting |
|---|---|
| Main (`windows.ts:526-531`) | Cancel the close. If `app.closeToTray` (read from the **current** settings at that moment; default `true`) → hide the window; else → quit the app. No save here: the quit sequence flushes. |
| Widget, chip (`windows.ts:436-444`) | Cancel the close, log `<widget or chip> window got a close request; quitting` (`widget` / `chip`), then synchronously save (as in 3.6) and quit (`index.ts:457-461`). Reason: the user cannot close these windows, so the request comes from outside (`taskkill` without `/f`, an installer). A graceful kill can be followed by a forced one within a second or two (electron-builder's installer waits only ~1.3 s, section 10), so save before the async quit sequence. |

While `isQuitting` is true every close proceeds.

Rust port (vox.exe): `taskkill /PID` without `/F` posts `WM_CLOSE` to every top-level window of the process, including
winit's hidden event-target window; vox-app subclasses that window like the overlays (`install_overlay_subclass`), so
the request becomes `Command::System(CloseRequested)` → save, then quit (ARCHITECTURE.md §4.13). The system-events
window (`VoxSystemEventsWindow`) does not receive it.

Widget and chip also suppress the **system context menu**: their drag handles count as a caption, so a right-click
there would open the system menu whose "Close" destroys the window (`windows.ts:437`). These are app-lifetime
windows that nothing recreates; if one were closed, reads would keep being synthesized (and billed) into a window
that no longer exists. ("Not closable" is not an option either: it would also block the app's own quit.) Native port:
give these windows no `WS_SYSMENU`, and swallow `WM_NCRBUTTONDOWN/UP` and `WM_CONTEXTMENU` on caption areas.

### 3.8 UI crash recovery and hardening (only if the UI runs in a separate web process)

- Crash recovery (`windows.ts:396-415`): when a window's renderer process dies, log
  `<app|widget|chip> renderer gone: <reason> (<exitCode>)` (logged even while quitting). Ignore if quitting, a clean
  exit, or the window is gone. Otherwise keep crash timestamps from the last **60 s** (`CRASH_WINDOW_MS`), add this
  one; if there are now more than **3** (`MAX_CRASH_RELOADS`) → log `<name> renderer keeps crashing; not reloading
  again` and stop (the 4th crash within 60 s is final); else reload the page. Reason: never loop on a page that
  crashes while loading. Preload errors are logged `<name> preload error in <path>`.
- Page loading (`windows.ts:357-384`): packaged → file `out/renderer/<app|widget|chip>.html` with the route as the
  URL hash; dev (unpackaged and `ELECTRON_RENDERER_URL` set) → `<devUrl>/<name>.html[#<hash>]`. A load failure is
  logged `failed to load <name>.html` (error). The page's own base URL (`file:///.../out/renderer/` or the dev
  server origin + `/`) is what navigation hardening compares against.
- Navigation hardening (`index.ts:62-75`): new-window requests are always denied (an http/https URL is opened in the
  default browser instead); in-page navigation to anything not starting with the app's own base URL (case-insensitive)
  is cancelled (http/https targets open externally); embedded webviews are refused.
- Web preferences for all windows: context isolation, sandbox, no Node, spellcheck off.
- Dev only (unpackaged): F12 or Ctrl+Shift+I toggles devtools in the main window (`windows.ts:537-542`).

A fully native UI needs none of this; a WebView2 UI should mirror it (deny `NewWindowRequested`, filter
`NavigationStarting`, disable browser accelerator keys, deny permissions).

---

## 4. Glue to neighbouring subsystems (in `index.ts`)

### 4.1 Selection service wiring (`index.ts:338-365`)

| Event from selection service | Shell reaction |
|---|---|
| `read {text, source, foreground}` | `reader.read({ text, source, sourceApp: fg.appName, sourceProcess: fg.processName, sourceTitle: fg.title })` |
| `no-selection` | info toast `Select some text first` |
| `notice {kind, message}` | toast with that kind/message (refused secrets, private text, password managers, clipboard restore warnings) |
| `chip-show {x, y, mode}` (cursor in DIP) | `showChip` (6.5) |
| `chip-hide` | `hideChip` |
| `hook-status available` | log `native input hook available` / warn `native input hook unavailable: chips are disabled, hotkeys/extension/tray still work` |

The service is started with the startup settings unless in shots mode (the global hook would let the user's own
clicks disturb the run). Start failure is logged, never fatal. The chip window reports its on-screen rectangle to the
service (`setChipBounds`) so the hook can treat a mouse-down inside it as a chip press.

### 4.2 Browser bridge glue (`index.ts:213-289`)

- Bridge read request `{ text?, title?, url? }` → if the reader is not created yet:
  `{ ok: false, code: "unknown", error: "Vox is still starting" }`. Otherwise take the foreground window's info
  (errors → none) and keep it only when its process is a known browser:
  `chrome.exe, msedge.exe, brave.exe, opera.exe, vivaldi.exe, arc.exe, chromium.exe, yandex.exe, firefox.exe`
  (anything else, e.g. our own window or a terminal, would mislabel the read; the process name is compared exactly as
  the foreground helper reports it, i.e. lower-case). Read with `text` = `req.text` if a string, else `""` (the reader
  then rejects it as empty), `source: "browser-extension"`, `sourceApp: fg.appName || "Browser"` (`"Browser"` when the
  foreground was discarded), `sourceProcess: fg.processName` (absent when discarded), `sourceTitle: title` if a
  string, `sourceUrl: url` if it parses as an http/https URL of at most 4096 chars. The bridge's HTTP reply is the
  reader's `ReadResult`.
- Bridge command → validated player command → `reader.command`.
- Bridge status → `{ appName: "Vox", version, playing: playerState.status === "playing" }`.
- Server lifecycle `syncServer(settings)` (`index.ts:266-289`), calls serialised through a queue:
  `want = triggers.browserExtension && !shotsMode`. Not wanted → stop if running. Wanted → create the server lazily;
  if already running on `server.port` → nothing; if running on another port → stop; start on `server.port`.
  Success → log `bridge listening on 127.0.0.1:<actual port>`. Failure → warn `bridge failed on port <port>:
  <error, or "unknown error">` and (not in demo) error toast `Browser bridge couldn't use port <port>. Pick another
  port in Settings` with button `Open settings` → settings/integrations. Any exception inside a queued step → error
  log `bridge sync failed`; the queue continues with the next call. Defaults: `triggers.browserExtension = true`,
  `server.port = 47823` (integer 1024-65535).

### 4.3 Explorer verb sync (`index.ts:185-211`)

`syncExplorerMenu(enabled, force)`, skipped in demo:

- Expected command line: `<exe> --read-file "%1"` (dev: `<electron.exe> <appPath> --read-file "%1"`), each part
  quoted with MSVCRT/`CommandLineToArgvW` rules **only when it contains whitespace or `"`**
  (`explorer-menu.ts:36-58`). So `C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe --read-file "%1"` has no quotes
  around the exe.
- Not forced (startup): read the installed state; if it already matches (`current` when enabling, `absent` when
  disabling) do nothing; otherwise log `explorer menu is <state>; installing|removing it` and rewrite. `stale` means
  partial, or pointing at another executable (moved install, dev build).
- Forced (setting toggled): always write/remove.
- Install writes, for each of `.txt .md .markdown .log .csv .json .rtf .html .htm`, key
  `HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\VoxRead` with `(Default) = "Read with Vox"`,
  `MultiSelectModel = "Single"`, `Icon = <packaged: exe path; dev: resources\icon.ico>`, and
  `...\VoxRead\command (Default) = <command>`. Any failure rolls back everything.
- Failure result → warn `explorer menu update failed` and error toast `Couldn't update the Explorer menu: <error>`
  (or without `: <error>` when the error is empty). No action button. An exception anywhere in the sync → warn
  `explorer menu sync failed`, no toast. Default `triggers.explorerMenu = false`, so a fresh install has no verb
  until the user turns it on (and the startup sync removes a stale one).

### 4.4 Launch at login (`index.ts:176-183`, `integrations/autostart.ts`)

Skipped in demo; applied at startup and whenever `app.launchAtLogin` or `app.startMinimized` changes.

- Value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, name `Vox` (dev builds: `Vox (dev)`, so dev and
  installed builds never overwrite each other's entry). Data: `"<exe>"` followed by `--hidden` when
  `startMinimized` (dev: `"<electron.exe>" <appPath> [--hidden]`).
- Enabling is idempotent: if a user-scope entry with the same name, same exe path (case-insensitive) and identical
  args already exists, nothing is written, so calling it on every start does not re-enable an entry the user switched
  off in Task Manager (`...\Explorer\StartupApproved\Run`). Otherwise Electron writes the Run value and marks it
  enabled in `StartupApproved\Run`.
- Disabling removes the Run value (and its StartupApproved state). Errors are never fatal: a failing registry call
  is caught inside the autostart module and only printed to the console as `[autostart] could not update the login
  item: <message>` (not to the log file); anything thrown past that is logged `applyLaunchAtLogin failed` (warn).
  No-op on non-Windows. Defaults: `app.launchAtLogin = false`, `app.startMinimized = false`.
- The idempotence check matches entries the OS reports for **this executable path** (user scope only), comparing
  value name, path (case-insensitive) and the argument list element by element (case-sensitive). Electron's own
  "is it enabled" flag only looks at a value named after the AppUserModelID, which is why the name is matched
  explicitly.

---

## 5. Reactions to settings changes

### 5.0 Settings the shell reads (`shared/defaults.ts:9-81`, sanitized in `services/settings-store.ts:85-170`)

Field names are the persisted `settings.json` keys; keep them unchanged for compatibility. Sanitization replaces an
invalid value with the default (enums, booleans) or clamps it (numbers).

| Key | Default | Allowed / sanitized | Used by |
|---|---|---|---|
| `onboarded` | `false` | bool | startup route, startup visibility (3.4) |
| `app.launchAtLogin` | `false` | bool | 4.4 |
| `app.startMinimized` | `false` | bool | 3.4, 4.4 (`--hidden` in the Run value) |
| `app.closeToTray` | `true` | bool | 3.7 |
| `app.theme` | `"system"` | `system \| paper \| night` | 5.6 |
| `widget.enabled` | `true` | bool | 5.1 #8, toasts (5.3), tray checkbox |
| `widget.position` | `"bottom-right"` | `bottom-right \| top-right \| bottom-center \| near-cursor \| remember` | 6.3.4 |
| `widget.customX`, `widget.customY` | `null` | `null` or number → rounded, clamped to ±100 000 | 6.3.4, 6.3.5 |
| `widget.autoHide`, `widget.compact` | `true`, `false` | bool | widget UI (6.3.7), shots |
| `triggers.rightClickChip` | `true` | bool | selection service, tray checkbox |
| `triggers.selectionPopup` | `false` | bool | selection service (chip `selection` mode) |
| `triggers.hotkeysEnabled` | `true` | bool | 8, tray hints/checkbox |
| `triggers.hotkeys.{readSelection,readClipboard,playPause,stop}` | `Ctrl+Alt+R`, `Ctrl+Alt+V`, `Ctrl+Alt+P`, `Ctrl+Alt+S` | string, trimmed, cut to 64 chars; `""` = off | 8 |
| `triggers.mediaKeys` | `true` | bool | 3.1 (startup switch), 5.1 #11 |
| `triggers.browserExtension` | `true` | bool | 4.2 |
| `triggers.explorerMenu` | `false` | bool | 4.3 |
| `server.port` | `47823` | integer 1024-65 535 | 4.2 |
| `library.retentionDays` | `0` (keep forever) | integer 0-3650 | 3.2 step 7, 5.2 |
| `library.cacheLimitMB` | `500` | integer 50-10 000 | 5.1 #10 |
| `library.saveHistory` | `true` | bool | shots (9) |
| `text.maxChars` | `20000` | (reader subsystem) | 2.2 `--read-file` |

### 5.1 The dispatcher (`onSettingsChanged`, `index.ts:295-330`)

Runs after every **effective** settings change (the store emits only when the sanitized result differs), with
`(next, prev)`. Exceptions are caught and logged `settings change handler failed`. In order:

| # | Condition | Reaction |
|---|---|---|
| 1 | always | Send the full new settings to every window (`settings:changed`). |
| 2 | `hotkeysEnabled` differs, or any of the 4 hotkey strings differ (`shortcuts.ts:188-191`) | Re-register hotkeys (8.5); may toast failures. |
| 3 | not shots mode | `selectionService.applySettings(next)` (errors logged). |
| 4 | `triggers.browserExtension` or `server.port` differs | `syncServer(next)` (4.2). |
| 5 | `triggers.explorerMenu` differs | `syncExplorerMenu(next.explorerMenu, force = true)` (4.3). |
| 6 | `app.launchAtLogin` or `app.startMinimized` differs | Apply launch-at-login (4.4). |
| 7 | `app.theme` differs | Apply theme (5.6). |
| 8 | `widget.enabled` went true→false; **else** `widget.position`, `customX` or `customY` differs and the widget is visible | Hide the widget; **else** re-position it (6.3.4). Turning the widget **on** does not show it (it appears with the next read or toast). A placement change while hidden does nothing now; it applies at the next show. |
| 9 | `library.retentionDays` differs | Schedule retention (5.2). |
| 10 | `library.cacheLimitMB` decreased (an increase does nothing) | Prune the audio cache to the new limit (LRU, never the playing read); log `cache limit lowered: evicted N cached reads` when N > 0; failure → warn `cache prune failed`. |
| 11 | `triggers.mediaKeys` differs | Info toast `Restart Vox to finish turning media keys on` / `Restart Vox to finish turning media keys off` (the engine switch is startup-only, 3.1). Sent straight to the reader's toast, not through the 5.3 helper (no wait for widget readiness). |
| 12 | always | Refresh the tray (cheap when nothing relevant changed). |

### 5.2 Retention debounce (`index.ts:30-35, 98-105`)

A changed "Keep history for" is applied **60 s** (`RETENTION_CHANGE_DELAY_MS = 60_000`) after the **last** change,
not at once. Reason: on a native `<select>` the arrow keys commit every option they pass, so arrowing from "Keep
forever" to "1 year" goes through "7 days" and would delete history. Each change restarts the 60 s timer. While it is
pending, the 6-hourly periodic run is skipped. When the timer fires it applies the **then-current**
`library.retentionDays`, not the value that started it. `applyRetention` does nothing unless `retentionDays > 0`
(0, the default, means keep forever).

### 5.3 Toast helper used by the shell (`index.ts:123-130`)

`toast(kind, message, route?)`: no-op until the reader exists; waits for the widget UI to be ready (6.3.2, at most
5 s, because at startup the widget may not be listening yet), then emits a toast `{ id, kind, message, action?:
{ label: "Open settings", route } }`. Used by the hotkey-failure, bridge-port and Explorer-verb toasts. The other
shell toasts (`Select some text first`, selection notices, the media-keys restart toast, and the reader's own
`Copy some text first` / `Couldn't open <file>` / "Nothing is playing" toasts) go straight to the reader's toast
without waiting for readiness. Delivery (reader subsystem): sent to the main window and the widget; if the main
window is visible and focused nothing else happens; else if `widget.enabled` the widget is shown (non-activating) and
displays it; else, for kinds other than `success`, a silent OS notification titled `Vox`.

### 5.4 Shell toast copy (exact strings)

| When | Kind | Message | Button |
|---|---|---|---|
| Hotkeys failed to register | error | see 8.6 | `Open settings` → `settings/triggers` |
| Bridge port unusable (not demo) | error | `Browser bridge couldn't use port <port>. Pick another port in Settings` | `Open settings` → `settings/integrations` |
| Explorer verb write failed | error | `Couldn't update the Explorer menu: <error>` | none |
| Media-keys setting toggled | info | `Restart Vox to finish turning media keys on` (or `off`) | none |
| Selection gesture found nothing | info | `Select some text first` | none |
| `--read-file` failed | error | `Couldn't open <file name>` | none |
| `--read-clipboard` / tray / hotkey with empty clipboard (whitespace-only counts as empty) | info | `Copy some text first` | none |
| Play/pause hotkey with nothing to play or replay | info | `Nothing is playing. Select some text and press <readSelection hotkey>` / `Nothing is playing. Select some text to read it` (8.8) | none |

### 5.5 Display changes (`index.ts:367-372`)

On "display removed" or "display metrics changed" (resolution, scale, work area/taskbar change): hide the chip; if
the widget is visible, re-position it per its placement mode. ("Display added" is not handled.) Native: react to
`WM_DISPLAYCHANGE`, `WM_DPICHANGED`, `WM_SETTINGCHANGE` with `SPI_SETWORKAREA`.

### 5.6 Theme (`windows.ts:45-48, 274-277, 581-596`; `index.ts:374-377`)

- Modes: `system | paper | night`. Resolved: `paper`/`night` are explicit; `system` → `night` when the OS app theme is
  dark, else `paper` (`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme == 0`).
- Native window colours:

  | Resolved | Main window background | Caption-button symbol colour (initial) |
  |---|---|---|
  | paper | `#ececec` | `#111111` |
  | night | `#000000` | `#ffffff` |

  The UI later overrides caption colours with its canvas palette: paper `#ececec` / `#0b0b0b`, night `#000000` /
  `#f2f2f2` (`design/theme.ts:7-10, 46-49`).
- Applying a mode: force the toolkit's light/dark source (`paper` → light, `night` → dark, `system` → follow OS; this
  drives the UI's colour scheme and native frame dark mode) and set the main window background colour to the
  resolved theme's background (if the main window exists; at startup it is applied before the windows are created,
  and the main window is then created with the resolved colours). It does **not** recolour the caption buttons; the UI
  re-sends `app:set-titlebar-theme` after it re-themes.
- When the OS theme changes and the mode is `system`, re-apply (native: `WM_SETTINGCHANGE` with
  `"ImmersiveColorSet"`).
- Caption theme from the UI (`app:set-titlebar-theme {color, symbolColor}`): both strings trimmed and validated with
  `/^(#[0-9a-f]{3,8}|rgba?\([\d\s.,%]+\)|transparent)$/i` (`ipc.ts:185-193`); invalid → ignored. Applied to the
  caption overlay with height 44; errors logged `setTitleBarOverlay failed`.

---

## 6. Windows

### 6.1 Overview (`windows.ts:502-518, 600-624, 725-745`)

Three windows, all created at startup and kept for the app's lifetime. Main holds module-level state for each and
treats a destroyed window as absent.

| | Main ("app") | Widget ("player") | Chip |
|---|---|---|---|
| Title | `Vox` | `Vox Player` | `Vox Chip` |
| Initial size | 1180 × 780, min 940 × 640 | 380 × 168 (until the UI reports) | 153 × 38 (context-mode default) |
| Initial position | centred by the toolkit (no x/y given); size, position and maximized state are **never persisted** across launches | placed by main before every show from hidden (6.3.4) | placed by main before every show (6.5.2) |
| Frame | hidden title bar + native caption-button overlay, height 44 | frameless, transparent, no shadow | frameless, transparent, no shadow |
| Taskbar | yes | no | no |
| Activates on show/click | yes | **never** | **never** |
| Topmost | no | yes ("floating": on Windows Electron also slots it just **below the taskbar**, see the next table) | yes ("pop-up-menu": top of the topmost band, above the taskbar) |
| Resizable | yes | no (main sizes it) | no (main sizes it) |
| Min/max/fullscreen | normal | none | none |
| Shown with | show + raise + focus | show-without-activating | show-without-activating + raise |
| Close | hide-to-tray or quit (3.7) | = quit (3.7) | = quit (3.7) |
| Background throttling | default | **off** (owns audio playback even while hidden: timers and media at full rate; audio autoplays without a user gesture) | off |
| Icon | `resources/icon.ico` | - | - |

Electron option → Windows behaviour to reproduce natively:

| Electron | Win32 meaning |
|---|---|
| `focusable: false` | `WS_EX_NOACTIVATE` (also implies not in the taskbar). Return `MA_NOACTIVATE` from `WM_MOUSEACTIVATE`. The selection service recognises our own floating windows by this bit plus our PID. |
| `skipTaskbar` | `WS_EX_TOOLWINDOW` (also hidden from Alt+Tab). |
| `alwaysOnTop` (any level) | `HWND_TOPMOST`. Windows has no real levels inside the topmost band: a context menu or flyout opened later can sit above the chip. |
| `setAlwaysOnTop(true, 'floating')` (widget) | Electron-internal, not visible in this repo (`native_window_views.cc`, `behind_task_bar_`; verify against Electron 44 if exact parity matters): for the levels `floating`, `torn-off-menu`, `modal-panel`, `main-menu`, `status` Electron additionally calls `SetWindowPos(hwnd, FindWindowW(L"Shell_TrayWnd", NULL), 0,0,0,0, SWP_NOMOVE\|SWP_NOSIZE\|SWP_NOACTIVATE)`, i.e. the widget stays topmost but **under the taskbar** (an auto-hide taskbar sliding up covers it). `pop-up-menu` (chip) does not do this. |
| `setAlwaysOnTop(true, ...)` after every show, `moveTop()` | `SetWindowPos(HWND_TOPMOST, ...)` then (chip only) `moveTop()` = `SetWindowPos(hwnd, HWND_TOP, ..., SWP_NOMOVE\|SWP_NOSIZE\|SWP_NOACTIVATE)` to come to the top of the topmost band (e.g. above an already open context menu). |
| `showInactive()` | `ShowWindow(SW_SHOWNOACTIVATE)` / `SWP_SHOWWINDOW` + `SWP_NOACTIVATE`. Never steals focus from the document being read. |
| `transparent: true`, `backgroundColor #00000000` | Per-pixel alpha window (DirectComposition or layered); only the card's rounded corners are transparent. |
| `hasShadow: false` | No DWM shadow. |
| `setIgnoreMouseEvents(true/false)` | Toggle click-through (`WS_EX_TRANSPARENT` + `WS_EX_LAYERED`). Used **only** on the chip during its exit animation. |
| CSS drag region (`-webkit-app-region: drag`) | `WM_NCHITTEST` returns `HTCAPTION` for handle rectangles → the system move loop. Pages get no pointer events there. |
| `'moved'` event | `WM_EXITSIZEMOVE` after a user move loop **in which the window actually moved** (Electron sets an "is moving" flag on `WM_MOVING` and only then emits `moved` on exit; a plain click on a drag handle enters and leaves the loop without `WM_MOVING` and emits nothing). Never fires for programmatic `SetWindowPos`. |
| `'system-context-menu'` | Right-click on a caption area (`WM_NCRBUTTONDOWN` with `HTCAPTION`). |
| `'query-session-end'`, `'session-end'` | `WM_QUERYENDSESSION`, `WM_ENDSESSION`. |
| `'close'` | `WM_CLOSE` (also posted to every top-level window by `taskkill` without `/f`; electron-builder's installer uses that only when PowerShell is unavailable, otherwise it terminates the process outright, section 10). |
| `titleBarStyle: 'hidden'` + `titleBarOverlay` | Client area extends into the frame; caption buttons (min/max/close, ~138 px wide) drawn in the top-right in the given colours over a 44 px strip; the UI keeps its own content out of that area. Keep Windows 11 Snap Layouts (`HTMAXBUTTON` hit test). |
| `ready-to-show` | First frame painted; showing then avoids a white flash. Background colour is the theme colour. |

Window-module dependencies supplied by bootstrap (`windows.ts:281-297`, `index.ts:450-462`):

```ts
interface WindowDeps {
  getSettings(): Settings
  setChipBounds(bounds: Rect | null): void   // DIP, null when hidden -> selection service
  onWidgetDragged(bounds: Rect): void        // -> settings.update({widget:{position:"remember", customX:x, customY:y}})
  onSessionEnd(): void                       // synchronous save (3.6)
  onCloseRequested(which: "widget" | "chip"): void  // synchronous save, then quit (3.7)
}
```

Messaging helpers (`windows.ts:821-840`): `sendTo(window, channel, payload?)` silently does nothing for a missing or
destroyed window; `broadcast` sends to every window.

### 6.2 Main window (`windows.ts:502-596`)

- Created hidden with the resolved theme's background and caption colours (`THEME_COLORS`, 5.6), 1180 × 780 DIP,
  centred, no menu bar (menu removed, auto-hide menu bar on so Alt does not reveal one), then shown (which activates
  it) on first paint if requested. Geometry is not saved or restored; every launch starts centred at the default
  size.
- Initial route is a hash: `#/<page>` or `#/<page>/<encodeURIComponent(section)>`. Pages:
  `home | history | analytics | voices | settings | onboarding`. A route received from the UI is validated: page must
  be one of these, `section` must match `/^[\w-]{1,64}$/` or is dropped (`ipc.ts:112-116`).
- `showMainWindow(route?)`: if the window does not exist (only possible if it was destroyed), create it shown with
  `route ?? (onboarded ? home : onboarding)`. Otherwise: navigate if a route was given; restore if minimized; show;
  raise; focus. Callers: tray click and tray items, second instance with no action, UI `app:show-main` (e.g. a toast's
  "Open settings", the widget's settings button).
- `navigateMain(route)`: if the page has finished loading, send `app:navigate route`; otherwise, once it finishes
  loading, set `location.hash` to the route.
- `hideMainWindow()`: from the UI (`app:hide-to-tray`).
- Native note: `SetForegroundWindow` only succeeds with foreground rights; call it directly from the tray click
  handler and rely on `AllowSetForegroundWindow` from the second instance (3.3).

### 6.3 Widget window (floating player)

#### 6.3.1 Role (`windows.ts:600-642, 677-694`)

Frameless, transparent, topmost, non-activating, no taskbar entry. Created hidden at startup and **never
destroyed**: it owns audio playback even while hidden and even when `widget.enabled` is false (then it is simply
never shown, but still plays). The window is exactly the card: no transparent shadow margin; only the rounded
corners are transparent.

Who shows it: a new read or cached replay when `widget.enabled` (reader); a toast while the main window is not
focused and `widget.enabled` (5.3); the shots harness. Who hides it: the UI (`widget:hide`: close button, stop of an
active session, auto-hide, dismissing a toast that revealed it), `widget.enabled` turned off (5.1 #8).

#### 6.3.2 Readiness (`windows.ts:316-319, 635-653`)

`whenWidgetReady(timeoutMs = 5000)` resolves when the widget UI is listening, or after the timeout so reads never
hang. Already ready → resolves immediately. The widget becomes ready on the first of: its first size report
(`widget:set-size`), its first player-state report (`player:state`), or **1500 ms** after its page finished loading
(fallback if it never reports a size). Readiness is one-shot (not reset by a renderer reload).

#### 6.3.3 Size reports (`windows.ts:696-712`, `shared/ipc.ts:103-111`)

The UI measures its root and sends:

```ts
interface OverlaySize {         // DIP
  width: number
  height: number
  hit?: { x: number; y: number; width: number; height: number }  // the card inside the window (window-relative)
}
```

`resizeWidget(raw)`:

1. `size = sanitizeSize(raw, min {40, 24}, max {1200, 900})`: `raw` must be an object whose `width` and `height`
   are finite numbers, else the report is ignored entirely (and does not mark readiness); each clamped, then rounded.
   Also ignored if the widget window does not exist.
2. Mark the widget ready.
3. `widgetHit = sanitizeHit(raw, size)` (always recomputed, even when the size is unchanged):
   ```
   hit must be an object with finite x, y, width, height, else null
   left   = max(0, floor(x));        top    = max(0, floor(y))
   right  = min(size.width,  ceil(x + width))
   bottom = min(size.height, ceil(y + height))
   if right-left < 8 or bottom-top < 8: null
   else {x: left, y: top, width: right-left, height: bottom-top}
   ```
4. If the size equals the current size → done.
5. Store the size. If the window is hidden → just resize it, keeping its top-left (placement is computed at the
   next show); hover tracking is not touched.
6. If visible → keep the **anchored edges** fixed: `resizeAnchored(currentBounds, size, widgetAnchor,
   workArea(display best matching currentBounds))`; mark "moved by us" now; set bounds; refresh hover tracking.

The UI reports a new size whenever the card changes (status, session, toast, compact layout); the enter animation
only scales (never translates) because the window ends at the card's edge.

#### 6.3.4 Placement (`windows.ts:655-675`, pure geometry in 6.4)

`positionWidget()`:

```
s      = settings.widget
cursor = current cursor position (DIP)
area   = if s.position == "remember" and customX != null and customY != null:
             workArea(display best matching {customX, customY, widgetSize})   // MonitorFromRect, nearest
         else:
             workArea(display nearest to cursor)                              // MonitorFromPoint, nearest
{bounds, anchor} = placeWidget(s.position, widgetSize, area, {cursor, customX, customY})
widgetAnchor = anchor; movedByUsAt = now; setBounds(bounds)
```

So fixed corners (`bottom-right`, `top-right`, `bottom-center`) follow **the monitor the mouse is on** at show time.

`showWidget({ reposition? })`: if hidden (or `reposition`), position first; if hidden, show without activating;
re-assert topmost every time; start/refresh hover tracking. A visible widget is never moved by a new read.

`hideWidget()`: hide if visible. `isWidgetVisible()`.

Placement modes (setting `widget.position`, default `bottom-right`):

| Mode | Placement (margin m = 24) | Anchor (edges fixed on resize) |
|---|---|---|
| `bottom-right` (default, and any unknown value) | `x = area.right - w - m`, `y = area.bottom - h - m` | right, bottom |
| `top-right` | `x = area.right - w - m`, `y = area.y + m` | right, top |
| `bottom-center` | `x = area.x + (area.width - w) / 2`, `y = area.bottom - h - m` | center, bottom |
| `near-cursor` | see below | computed |
| `remember` | stored `customX/customY` clamped into the area; falls back to `bottom-right` when either is null | by quadrant (`anchorFor`) |

`near-cursor`: `x = cursor.x + 16`, `y = cursor.y + 20`, anchor left/top. If `x + w > area.right - m` →
`x = cursor.x - w - 16`, anchor h = right. If `y + h > area.bottom - m` → `y = cursor.y - h - 16`, anchor v = bottom.

All results go through `clampToArea` (6.4). `customX/customY` are stored as integers clamped to ±100 000 (or null).

#### 6.3.5 Dragging (`windows.ts:626-629, 714-721`; UI side `widget/Widget.css:33-70`)

- The card is client area; only text and labels are drag handles (caption hit-test): source label row, the sentence
  or title body when it holds no control, voice name, time, status label, toast/banner text; on the compact pill the
  time column, the middle when it holds no control/warning, and an invisible grip. Buttons and the progress bar are
  never handles. Buttons drawn smaller than 32 px get an invisible larger hit area.
- Design rules (hard-won, keep them in a native port of the hit-testing): never make the whole card draggable with
  holes punched for controls, and never put a control inside a handle. Chromium sent new drag regions one frame
  after a layout change and none while hidden, so a press right after a view switch hit a stale region, became a
  window move, and the click was lost. A handle's region is its own box only (inherited regions from transformed,
  scrolled-away text once turned Speed/Close into a title bar). A handle gets no mouse moves, so it shows no tooltip;
  text whose tooltip matters is not a handle.
- End of a user move (`WM_EXITSIZEMOVE` after the window really moved, see the `'moved'` row in 6.1; a click on a
  handle that does not move the window must not switch the mode to `remember`): ignored unless **more than 500 ms**
  have passed since the last programmatic move/resize (`now - movedByUsAt > 500`; belt and braces, since programmatic
  moves never produce the event). Else, if the widget is visible: read its bounds, set
  `widgetAnchor = anchorFor(bounds, workArea(display matching bounds))`, and save
  `{ widget: { position: "remember", customX: bounds.x, customY: bounds.y } }` (integers, DIP). That settings change then
  triggers 5.1 #8, which re-positions the visible widget in `remember` mode: a widget dragged partly off its work
  area snaps back inside it.

#### 6.3.6 Hover tracking (`windows.ts:446-498`)

The widget never toggles click-through. (It used to, from a 40 ms cursor poll around a shadow margin; that dropped
fast clicks onto the app below, and `WindowFromPoint` skipped the window while it ignored the mouse.) Because the drag
handles are caption area, the UI gets no pointer events there and cannot tell it is hovered, so main polls:

- `HOVER_POLL_MS = 100`. State `widgetOnCard: true | false | null` (null = hidden/unknown).
- Each poll: if the widget is visible, `next = pointOnCard(cursor, windowBounds, card)` where `card = widgetHit ??
  {0, 0, widgetSize}` (on error keep the previous value); if hidden, `next = null`. If `(next == true) !=
  (prev == true)` send `widget:hover <next == true>` to the widget. Store `next`. If `next == null`, stop the timer.
- `pointOnCard(p, bounds, hit)`: `x = p.x - bounds.x - hit.x`, `y = p.y - bounds.y - hit.y`; true iff
  `0 <= x < hit.width && 0 <= y < hit.height`.
- Tracking (re)starts after every show and every visible resize: poll once immediately, then start the interval if it
  is not running and the widget is visible. Hiding is noticed by the next poll (which sends `false` if it was
  hovered, then stops).
- `isWidgetHovered()` gives the same answer synchronously.

A native port can replace polling with real `WM_MOUSEMOVE`/`WM_NCMOUSEMOVE` plus `TrackMouseEvent(TME_LEAVE |
TME_NONCLIENT)`, but must report hover over handles too.

#### 6.3.7 What the widget UI does with hover, auto-hide and toasts (context; `widget/controller.ts`, `widget/Widget.tsx`)

- Auto-hide: when the player reaches `ended`, `widget.autoHide` is on and the read did not stop early with a
  warning, hide after **4000 ms**; if a hold is active at that moment (hover, or a toast on screen) retry every
  **1500 ms**. Any new session cancels it.
- Toasts on the widget stay **2500 ms** (**5000 ms** if they have a button); while hovered the dismissal re-checks
  every **1000 ms**. Dismissing hides the window again when idle, or when the window had hidden itself before the toast
  revealed it for the same finished session.
- The close button stops playback and synthesis, then hides. A `stop` command while a session exists hides.

#### 6.3.8 Rust port rendering notes (vox.exe; ARCHITECTURE.md §4.13, §7.1)

- Skia renders through wgpu on DirectX 12, whose window swap chain is opaque: "transparent" pixels show black. The
  widget and chip windows are therefore clipped to their rounded shape with `SetWindowRgn` (aliased edge) and draw an
  opaque panel under the card / pill (`opaque-backing`), so the enter / exit animations and the 0.96 panel never blend
  over black. True per-pixel translucency would need another renderer path (e.g. Skia on OpenGL, or wgpu through
  DirectComposition), which this build does not use.
- A hidden widget is first shown off-screen at its final size and moved into place after its first frame at that size
  (`windowing::reveal`, 250 ms fallback), so the previous session's card is never on screen; a read's show waits for
  the player's first report of the new session. On mixed-DPI setups where the parking point's monitor has another
  scale, the widget shows in place.
- Slint lists keep their row instances across pushes (`ui::stable`): hover, focus and clicks on the card, the sidebar,
  Home and the toast stack survive the 30 Hz playback poll.

### 6.4 Pure geometry (`windows.ts:50-123, 236-268`)

All inputs/outputs DIP; `Rect {x, y, width, height}`; `area.right = area.x + area.width`, etc.

```
clampToArea(r, area):             // move inside without resizing; oversized -> pinned to area's top-left
  x = max(area.x, min(r.x, area.x + area.width  - r.width))
  y = max(area.y, min(r.y, area.y + area.height - r.height))
  return round(x), round(y), round(r.width), round(r.height)

anchorFor(r, area):               // quadrant: lower half grows upward, right half grows leftward
  h = (r.x + r.width/2  > area.x + area.width/2)  ? right  : left
  v = (r.y + r.height/2 > area.y + area.height/2) ? bottom : top

resizeAnchored(old, size, anchor, area):
  x = old.x; y = old.y
  if anchor.h == right:  x = old.x + old.width - size.width
  if anchor.h == center: x = old.x + (old.width - size.width) / 2
  if anchor.v == bottom: y = old.y + old.height - size.height
  return clampToArea({x, y, size}, area)

sanitizeSize(raw, min, max):      // raw.width/height must be finite numbers
  {round(clamp(width, min.w, max.w)), round(clamp(height, min.h, max.h))}
```

`Anchor = { h: left | center | right, v: top | bottom }`; initial `widgetAnchor = {right, bottom}`.

### 6.5 Chip window ("Read with Vox" pill and selection popup)

#### 6.5.1 Role and modes (`windows.ts:39-43, 770-786`; `main/ipc.ts:437-455`)

| Mode | Shown when | Label | Default size until reported |
|---|---|---|---|
| `context` | the user right-clicks while a text-selection hint is live; the pill sits **beside** the native context menu, never on it | `Read with Vox` | 153 × 38 |
| `selection` (the "popup", off by default) | right after a selection gesture ends | `Read` | 82 × 38 |

Payload sent to the chip UI: `{ mode: "context" | "selection", label }`. The window is the pill; Windows kept the
transparent Electron window at least ~38 DIP tall (and may round its width up a pixel), so the chip UI has
`min-height: 38px` with the pill vertically centred. A press **anywhere** in the window counts as a chip press:
left button → `chip:activate`, other buttons → `chip:dismiss`, both on pointer-down (not click: some context menus
swallow the click that dismisses them); the window's own context menu is suppressed. The chip UI ignores presses
while its exit animation runs. Both `chip:activate` and `chip:dismiss` hide the chip immediately, then notify the
selection service (`chipActivated`, de-duplicated within 300 ms against the hook's own detection; `chipDismissed`).

Topmost "pop-up-menu" level, re-asserted and raised on every show so it sits above the context menu that is already
open. (A menu or flyout opened **later** can still cover it; the selection service's click test therefore also checks
that the window under the cursor is this app's own non-activating window.)

#### 6.5.2 Show / hide / resize (`windows.ts:758-819`)

```
showChip({x, y, mode}):                         // x, y = cursor in DIP
  if a hide is pending: cancel it
  chipMode = mode; chipCursor = (round(x), round(y))
  send chip:show {mode, label: mode == context ? "Read with Vox" : "Read"}
  placeChipWindow()
  click-through OFF                             // only ever on during the exit animation
  show without activating; topmost; raise

placeChipWindow():
  if chipCursor is null: return
  area = workArea(display nearest chipCursor)
  setBounds(placeChip(chipMode, chipCursor, chipSizes[chipMode], area))
  setChipBounds(actual window rect as the OS sized it)   // the hook's click test uses the real rect

hideChip():
  chipCursor = null; setChipBounds(null)
  send chip:hide                                 // UI starts its 120 ms exit animation
  if the window is not visible or a hide is already pending: return
  click-through ON                               // inert while animating
  after CHIP_EXIT_MS = 140 ms (120 ms animation + a frame):
    if no new show happened (chipCursor still null): hide the window

resizeChip(raw):
  size = sanitizeSize(raw, min {32, 24}, max {480, 160})
  if unchanged for the current mode: return
  chipSizes[chipMode] = size
  if visible and chipCursor: placeChipWindow() else set the window size only
```

Sizes are remembered per mode (in memory only, reset each launch). The chip UI reports its size after each
mode/label change, so the first show of a mode uses the default size and is re-placed when the real size arrives.
A report is filed under the mode **last shown** (`chipMode`), not the mode the UI measured; the report carries no
mode. A port that knows the measured mode should key on it. Reports are ignored when the chip window does not exist
or `width`/`height` are not finite numbers. `CHIP_EXIT_MS` must stay in sync with the
UI's exit animation (120 ms ease-in; enter/exit only scale, never translate).

The chip is hidden on display changes (5.5) and whenever the selection service says so (next mouse-down elsewhere,
any key, the selection popup's own timeout, etc.).

#### 6.5.3 Placement algorithm (`windows.ts:125-234`)

Constants:

| Name | Value | Meaning |
|---|---|---|
| `CONTEXT_MENU_SIZE.small` | 220 × 240 | below this room a flip is certain |
| `CONTEXT_MENU_SIZE.guess` | 320 × 360 | one typical menu size for the fallback pass |
| `CONTEXT_MENU_SIZE.large` | 360 × 480 | at or above this room a flip is impossible |
| `CHIP_GAP` | 18 | horizontal gap between cursor (or menu edge) and pill |
| `CHIP_GAP_V` | 20 | vertical gap for above/below candidates |
| `SELECTION_CHIP_GAP` | 20 | selection popup offset from the cursor |

Native context menus (Win32 and Chromium alike) open to the lower right of the cursor and flip left/upward when
they do not fit the work area. Their size is unknown, so the prediction uses a range:

```
predictMenu(cursor, area, small = SMALL, large = LARGE):
  roomRight = area.x + area.width  - cursor.x
  roomBelow = area.y + area.height - cursor.y
  h = roomRight >= large.width  ? right : roomRight < small.width  ? left : unknown
  v = roomBelow >= large.height ? down  : roomBelow < small.height ? up   : unknown

menuRects(cursor, pred, menu):             // every rect the menu may occupy
  for h in (pred.h == unknown ? [right, left] : [pred.h]):
    for v in (pred.v == unknown ? [down, up] : [pred.v]):
      x = h == right ? cursor.x : cursor.x - menu.width
      y = v == down  ? cursor.y : cursor.y - menu.height
      yield {x, y, menu.width, menu.height}

contextCandidates(cursor, size{w, hh}, h, menu):   // best first
  cy = cursor.y - hh/2
  xv = h == left ? cursor.x - w : h == right ? cursor.x : cursor.x - w/2
  [ {cursor.x - CHIP_GAP - w,               cy},   // 1 left of cursor, vertically centred
    {cursor.x + CHIP_GAP,                   cy},   // 2 right of cursor
    {xv, cursor.y - CHIP_GAP_V - hh},              // 3 above, hanging off the menu's own edge
    {xv, cursor.y + CHIP_GAP_V},                   // 4 below
    {cursor.x - menu.width - CHIP_GAP - w,  cy},   // 5 past the menu's far left side (corners)
    {cursor.x + menu.width + CHIP_GAP,      cy} ]  // 6 past the menu's far right side

intersects(a, b) = a.x < b.x+b.w && b.x < a.x+a.w && a.y < b.y+b.h && b.y < a.y+a.h   // touching edges do not count

firstClear(candidates, area, menus):
  for c in candidates: r = clampToArea(c, area); if r intersects no menu rect: return r
  return none

placeChip(mode, cursor, size, area):
  if mode == context:
    // 1. Clear of the menu whichever way an undecided flip goes (menus up to the large size)
    p = predictMenu(cursor, area)
    r = firstClear(contextCandidates(cursor, size, p.h, LARGE), area, menuRects(cursor, p, LARGE))
    if r: return r
    // 2. One typical size decides the flips (never "unknown" here)
    g = predictMenu(cursor, area, GUESS, GUESS)
    r = firstClear(contextCandidates(cursor, size, g.h, GUESS), area, menuRects(cursor, g, GUESS))
    if r: return r
    // 3. The classic spot
    return clampToArea({cursor.x - CHIP_GAP - size.w, cursor.y - size.h/2, size}, area)
  else:  // selection
    x = cursor.x + SELECTION_CHIP_GAP
    y = cursor.y - SELECTION_CHIP_GAP - size.h
    if y < area.y: y = cursor.y + SELECTION_CHIP_GAP
    return clampToArea({x, y, size}, area)
```

Effect: the pill sits left of the cursor while the menu opens to the right; right of it when the menu flips left;
above/below when neither side is free; past the menu's far side in screen corners.

### 6.6 UI ↔ shell messages handled here (`shared/ipc.ts`, `main/ipc.ts:412-455`)

Senders are validated: a message must come from one of our own windows; widget messages only from the widget, chip
messages only from the chip. A native port turns these into internal events but should keep the origin rules.

| Channel | Direction | Payload | Handling |
|---|---|---|---|
| `settings:changed` | main → all windows | `Settings` | after every effective change (5.1) |
| `app:navigate` | main → main window | `AppRoute {page, section?}` | 6.2 |
| `app:toast` | main → main window + widget | `Toast {id, kind: info/success/error, message, action?: {label, route?}}` | 5.3 |
| `app:show-main` | any window → main | `AppRoute?` (validated, invalid → default) | `showMainWindow` |
| `app:hide-to-tray` | any → main | - | hide main window |
| `app:quit` | any → main | - | quit (3.5) |
| `app:set-titlebar-theme` | any → main | `{color, symbolColor}` | 5.6 |
| `app:suspend-shortcuts` | any → main | `boolean` (only literal `true` suspends) | 8.7 |
| `widget:set-size` | widget → main | `OverlaySize` | 6.3.3 |
| `widget:hide` | widget → main | - | `hideWidget` |
| `widget:hover` | main → widget | `boolean` | 6.3.6 |
| `player:state` | widget → main | `PlayerState` | also marks the widget ready (6.3.2); refreshes the tray via the reader |
| `chip:show` | main → chip | `{mode, label}` | 6.5.2 |
| `chip:hide` | main → chip | - | start exit animation |
| `chip:activate` / `chip:dismiss` | chip → main | - | hide chip, then notify selection service |
| `chip:set-size` | chip → main | `{width, height}` | 6.5.2 |

---

## 7. Tray (`src/main/tray.ts`)

### 7.1 Icons and states (`tray.ts:30-34, 139-143, 165-173`)

| State | Icon file | When |
|---|---|---|
| normal | `resources/tray.ico` (sizes 16, 20, 24, 32, 40, 48) | idle, paused, ended, error |
| active | `resources/tray-active.ico` (same sizes) | player status `playing` or `loading` |

Design (generated by `scripts/gen-icons.mjs`): a bold 5×5 dot-matrix "V" of white round dots on a near-black
(`#0b0b0b`) rounded square with a faint white hairline edge at ≥ 32 px; the active variant adds a red (`#D71921`)
"live" dot in the lower-right corner with a transparent ring cut around it. A missing/unreadable icon file is logged
`tray icon missing or unreadable: <file>` (an empty icon is used). The icon is swapped only when the wanted state
differs from the shown one.

### 7.2 Tooltip (`tray.ts:20-55`)

| Player status | Tooltip |
|---|---|
| `playing` | `Vox · Reading “<title>”` (no title: `Vox · Reading`) |
| `paused` | `Vox · Paused “<title>”` (no title: `Vox · Paused`) |
| `loading` | `Vox · Preparing audio…` |
| anything else | `Vox` |

Characters: middle dot U+00B7 with spaces, curly quotes U+201C/U+201D, ellipsis U+2026. Windows truncates tray
tooltips at 127 UTF-16 units (`TOOLTIP_MAX = 127`): if longer, and the text ends with `”`, keep the first 125 units
and append `…”`; otherwise keep the first 126 and append `…`. (The original can split a surrogate pair at the cut;
a port should back off one unit if the cut lands inside a pair.) `<title>` is the player's one-line preview of the text
(≤ 64 chars, whitespace collapsed, cut at a word and suffixed `…`). The tooltip is only re-set when it changed.

### 7.3 Menu (`tray.ts:63-120`)

Left click on the icon (fires on button release) → show the main window (default route: `home`, or `onboarding` if
not onboarded; an existing window keeps its current page). There is no double-click handler (the first click of a
double-click already opens the window). Right click (button release) → context menu at the cursor:

| # | Item | Type | Enabled | Accelerator hint shown | Action |
|---|---|---|---|---|---|
| 1 | `Read clipboard` | normal | always | `hotkeys.readClipboard` | `reader.readClipboard()` |
| 2 | `Resume` while paused, otherwise `Pause` (also when idle/loading, then disabled) | normal | playing or paused | `hotkeys.playPause` | player command `toggle` |
| 3 | `Stop` | normal | playing, paused or loading | `hotkeys.stop` | player command `stop` |
| - | separator | | | | |
| 4 | `Open Vox` | normal | | | show main (default route) |
| 5 | `History` | normal | | | show main `history` |
| 6 | `Analytics` | normal | | | show main `analytics` |
| 7 | `Settings` | normal | | | show main `settings` |
| - | separator | | | | |
| 8 | `Right-click chip` | checkbox = `triggers.rightClickChip` | | | toggle that setting |
| 9 | `Floating widget` | checkbox = `widget.enabled` | | | toggle that setting |
| 10 | `Hotkeys` | checkbox = `triggers.hotkeysEnabled` | | | toggle that setting |
| - | separator | | | | |
| 11 | `Quit Vox` | normal | | | quit (3.5) |

Accelerator hints are display-only (never registered by the menu). A hint is shown only when hotkeys are enabled,
the string is non-empty and matches `/^[A-Za-z0-9]+(\+[A-Za-z0-9]+)*$/` (so e.g. `Ctrl+Alt+;` shows no hint);
building the menu with hints can throw on an accelerator the toolkit rejects, in which case it is rebuilt without
any hints (warn `tray menu with accelerator hints failed, rebuilding without`). The tray "Pause/Resume" sends a plain
`toggle`; it does not have the play/pause hotkey's "replay the last read" behaviour (8.8). Hints are the raw setting
strings (e.g. `Ctrl+Alt+V`), not re-formatted.

The checkbox items write the **negation of the value seen when the menu was built** (`{triggers: {rightClickChip:
!old}}` etc.); that is safe because any change of those values changes the menu key (7.4) and rebuilds the menu. The
resulting settings change then runs the normal dispatcher (5.1), e.g. unchecking `Floating widget` hides the widget
and unchecking `Hotkeys` unregisters them.

### 7.4 Refresh (`tray.ts:122-181`)

`refreshTray()` is called on every player-state report and after every settings change. It is cheap: icon and tooltip
are compared with what is shown, and the menu is rebuilt only when this key changes:

```
key = JSON([statusBucket, triggers.rightClickChip, widget.enabled, triggers.hotkeysEnabled, triggers.hotkeys])
statusBucket = status in {playing, paused, loading} ? status : "idle"
```

`refreshTray()` is a no-op before the tray exists and after it was destroyed (state reports during the shutdown flush
do nothing). `createTray()` adds the icon with the `normal` image, registers the click handler and refreshes once
(tooltip, menu). `destroyTray()` (end of shutdown) removes the icon and resets the cached icon/tooltip/menu key.

Native notes: `Shell_NotifyIconW` with `NOTIFYICON_VERSION_4`, tooltip via `NIF_TIP | NIF_SHOWTIP`; re-add the icon
when Explorer restarts (registered message `TaskbarCreated`, broadcast to top-level windows only); load the icon at
`SM_CXSMICON` for the current DPI; call `SetForegroundWindow` on the owner before `TrackPopupMenuEx` and post
`WM_NULL` after, so the menu closes when clicking elsewhere.

---

## 8. Global hotkeys (`src/main/shortcuts.ts`, `index.ts:132-174`)

### 8.1 Settings and defaults (`shared/defaults.ts:30-39`, `settings-store.ts:56, 123-127`)

```ts
triggers.hotkeysEnabled: boolean   // default true
triggers.hotkeys: {                // Electron accelerator strings; "" = disabled; trimmed, max 64 chars
  readSelection: "Ctrl+Alt+R",
  readClipboard: "Ctrl+Alt+V",
  playPause:     "Ctrl+Alt+P",
  stop:          "Ctrl+Alt+S"
}
```

Registration order (matters for duplicate detection): `readSelection, readClipboard, playPause, stop`.

Media keys are deliberately **not** registered as global hotkeys by the app itself: the widget's media-session
handling covers them, and grabbing them globally would break other players. (A user can still record a media or
volume key as one of the four hotkeys, 8.2; it is then registered like any other accelerator.) With `triggers.mediaKeys` off, the engine's own media-key
handling is switched off at startup (3.1). Native port: use System Media Transport Controls for the widget's
playback and enable/disable them per the setting (this can apply live; the "restart" toast exists only because of
Chromium's startup-only switch).

### 8.2 Accelerator grammar (what the settings file contains)

Produced by the Settings recorder (`renderer/src/design/hotkey.ts`), and possibly hand-edited:

- Parts joined by `+`; a literal plus key is written as a `+` right after a separator or at the start (`Ctrl++`)
  or as `Plus`.
- Modifiers (case-insensitive) and aliases: `Ctrl` (`Control`, `CmdOrCtrl`, `CommandOrControl`), `Alt` (`Option`,
  `AltGr`), `Shift`, `Super` (`Cmd`, `Command`, `Meta`, `Win`). Recorder order: Ctrl, Alt, Shift, Super.
- Keys the recorder emits: `A`-`Z` (the letter the **active layout** gives the key; Windows assigns letter VKs by the
  layout's labels), `0`-`9` (physical digit row), `num0`-`num9`, `F1`-`F24`, `Space`, `Enter` (alias `Return`),
  `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`,
  `Escape` (alias `Esc`), `PrintScreen`, punctuation ``- = [ ] \ ; ' ` , . /``, `numadd`, `numsub`, `nummult`,
  `numdiv`, `numdec`, `MediaPlayPause`, `MediaStop`, `MediaNextTrack`, `MediaPreviousTrack`, `VolumeUp`,
  `VolumeDown`, `VolumeMute`.
- The recorder requires Ctrl, Alt or Super unless the key is F1-F24, a media/volume key or PrintScreen.

Key → virtual-key code, as Electron registers it (punctuation by **US-layout** position):

| Key | VK | Key | VK |
|---|---|---|---|
| `A`-`Z` | `0x41`-`0x5A` | `0`-`9` | `0x30`-`0x39` |
| `num0`-`num9` | `0x60`-`0x69` | `F1`-`F24` | `0x70`-`0x87` |
| `;` | `0xBA` | `=` / `+` / `Plus` | `0xBB` |
| `,` | `0xBC` | `-` | `0xBD` |
| `.` | `0xBE` | `/` | `0xBF` |
| `` ` `` | `0xC0` | `[` | `0xDB` |
| `\` | `0xDC` | `]` | `0xDD` |
| `'` | `0xDE` | `Space` | `0x20` |
| `Enter` | `0x0D` | `Tab` | `0x09` |
| `Backspace` | `0x08` | `Delete` | `0x2E` |
| `Insert` | `0x2D` | `Home`/`End` | `0x24`/`0x23` |
| `PageUp`/`PageDown` | `0x21`/`0x22` | `Left`/`Up`/`Right`/`Down` | `0x25`/`0x26`/`0x27`/`0x28` |
| `Escape` | `0x1B` | `PrintScreen` | `0x2C` |
| `numadd`/`numsub` | `0x6B`/`0x6D` | `nummult`/`numdiv`/`numdec` | `0x6A`/`0x6F`/`0x6E` |
| `MediaPlayPause`/`MediaStop` | `0xB3`/`0xB2` | `MediaNextTrack`/`MediaPreviousTrack` | `0xB0`/`0xB1` |
| `VolumeUp`/`VolumeDown`/`VolumeMute` | `0xAF`/`0xAE`/`0xAD` | | |

Modifiers map to `MOD_CONTROL`, `MOD_ALT`, `MOD_SHIFT`, `MOD_WIN`. An unparseable accelerator is a registration
failure (reason `in-use`, see 8.5).

Electron-internal parser details (`keyboard_util.cc`, not in this repo; verify against Electron 44 if exact parity
matters): a single-character key that is a **shifted** US symbol adds Shift implicitly. In particular `+` and `Plus`
register as **Shift + `VK_OEM_PLUS`** (`Ctrl++` = Ctrl+Shift+`=` key), and `:` `<` `_` `>` `?` `~` `{` `|` `}` `"`
`!`-`)` likewise map to their unshifted key plus Shift. The recorder never emits those shifted symbols except `+`.
Electron's Windows global-shortcut listener calls `RegisterHotKey` with only `MOD_SHIFT/MOD_CONTROL/MOD_ALT/MOD_WIN`
(no `MOD_NOREPEAT`), so holding a chord auto-repeats `WM_HOTKEY`.

### 8.3 Canonical form and reserved chords (`shortcuts.ts:46-95`, `shared/constants.ts:22-41`)

```
splitParts(accel):                      // handles a literal '+'
  s = trim(accel); parts = []; cur = ""
  for i, ch in s:
    if ch == '+' and cur != "": parts.push(cur); cur = ""
    elif ch == '+':             parts.push("+"); if s[i+1] == '+': skip it
    else:                       cur += ch
  if cur != "": parts.push(cur)
  return parts.map(p -> ALIASES[lower(trim(p))] ?? lower(trim(p)))

ALIASES = { control: ctrl, cmdorctrl: ctrl, commandorcontrol: ctrl, option: alt, altgr: alt,
            cmd: super, command: super, meta: super, win: super, esc: escape, return: enter, plus: "+" }
MODIFIERS = [ctrl, alt, shift, super]   // canonical order

normalize(accel):
  mods = unique(parts ∩ MODIFIERS) sorted by MODIFIERS order
  rest = parts not in MODIFIERS, original order (not de-duplicated)
  return join(mods ++ rest, "+")
```

Reserved (never registered; Settings refuses them, main skips them if a hand-edited file has one), compared in
canonical form:

```
Ctrl+C  Ctrl+Insert  Ctrl+V  Ctrl+X  Ctrl+Z  Ctrl+Y  Ctrl+A  Ctrl+S
Alt+F4  Alt+Tab  Ctrl+Escape  Ctrl+Shift+Escape
```

Reason: registering one hijacks it in every app while Vox runs, and Ctrl+C / Ctrl+Insert are also the copy chords
the selection capture sends (a hotkey on them would swallow the copy).

### 8.4 AltGr guard (`shortcuts.ts:97-137`, `selection/win32.ts:215-235`)

Windows delivers AltGr as Ctrl+Alt, so a hotkey on Ctrl+Alt(+Shift)+key fires whenever the user types the character
that AltGr+key produces (AltGr+V is `@` on Hungarian, AltGr+S is `ś` on Polish), and stops them typing it.

```
altGrConflict(accel, lookup):
  parts = splitParts(accel)
  if !(ctrl in parts && alt in parts) || super in parts: return null
  keys = parts not in MODIFIERS
  vk = keys.len == 1 ? characterKeyVk(keys[0]) : null
  if vk == null: return null           // only letters, digits, OEM punctuation are checked
  return lookup(vk, shift in parts)    // errors -> log "keyboard layout lookup failed", null

characterKeyVk(k): /^[a-z]$/ -> ASCII uppercase code; /^[0-9]$/ -> ASCII code;
                   OEM map: ; 0xBA, = 0xBB, + 0xBB, , 0xBC, - 0xBD, . 0xBE, / 0xBF, ` 0xC0, [ 0xDB, \ 0xDC, ] 0xDD, ' 0xDE
                   else null
```

`lookup(vk, shift)` = what that key types with Ctrl+Alt(+Shift) on **any installed keyboard layout**:

```
layouts = GetKeyboardLayoutList()            // none -> null
state = 256-byte array; state[VK_CONTROL=0x11] = state[VK_MENU=0x12] = state[VK_LCONTROL=0xA2] = state[VK_RMENU=0xA5] = 0x80
if shift: state[VK_SHIFT=0x10] = state[VK_LSHIFT=0xA0] = 0x80
for hkl in layouts:
  scan = MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC = 0, hkl)
  n = ToUnicodeEx(vk, scan, state, buf, 8, flags = 0x4 /* don't change keyboard state (Win10 1607+) */, hkl)
  if n < 0: return ""                        // AltGr dead key: a conflict with no printable character
  if n == 0: continue
  text = buf[..min(n, 8)] as UTF-16
  if any code point in text is >= 0x20 and != 0x7F: return text   // control chars (Ctrl+letter) are not typing
return null
```

If the Win32 helpers are not loaded at the moment of a registration, the lookup returns null and no AltGr guard
applies to that registration. Concretely: if the load missed the 1500 ms startup window, the startup registration
runs unguarded (a conflicting chord gets registered and stays until the next re-registration); every later
registration (settings change, end of suspension) is guarded once the load has finished. If the load failed, the
guard never applies. The lookup is consulted only when the accelerator has both Ctrl and Alt, no Super, and exactly
one non-modifier key that is a letter, digit or OEM punctuation (F-keys, numpad, navigation and media keys are never
checked). Note the Shift flag passed to the lookup is only the accelerator's explicit `Shift`, not the implicit
Shift Electron adds for `+`/`Plus` (8.2).

### 8.5 Registration (`registerShortcuts`, `shortcuts.ts:143-185`; `applyShortcuts`, `index.ts:154-174`)

`applyShortcuts(settings)` does nothing in shots mode, before the reader exists, or while suspended (8.7). Otherwise:

```
unregister everything we registered before
if !hotkeysEnabled: return []                 // no failures
seen = {}; failures = []
for action in [readSelection, readClipboard, playPause, stop]:
  accel = trim(hotkeys[action]); if accel == "": continue
  key = normalize(accel)
  if key in RESERVED:          failures += {action, accel, "reserved"};        continue   // not added to seen
  if key in seen:              failures += {action, accel, "duplicate"};       continue
  seen += key
  ch = altGrConflict(accel, lookup)
  if ch != null:               failures += {action, accel, "types-character", ch}; continue
  ok = OS register(accel)      // invalid accelerator (throws) also counts as not ok
  if ok: remember accel        else failures += {action, accel, "in-use"}
if failures: warn "hotkeys not registered: readClipboard=Ctrl+Alt+V (types-character), ..."
```

Failure reasons: `in-use` (taken by another app, or invalid), `duplicate` (two of our actions share it), `reserved`,
`types-character`. An accelerator the OS layer rejects as unparseable is additionally logged
`invalid accelerator for <action>: <accel>` (warn). Each hotkey callback is wrapped: an exception is logged
`hotkey <action> handler failed`. The **trimmed** string is what gets registered, remembered for unregistering, and
reported in failures. "Unregister ours" (`unregisterShortcuts`) unregisters each remembered accelerator, ignoring
errors, and clears the list.

Change detection (`hotkeysChanged`, `shortcuts.ts:188-191`) compares `hotkeysEnabled` and the four **raw** (untrimmed)
strings with plain equality, so changing only surrounding whitespace also re-registers (and re-toasts failures).

After registering: any failures → error toast (8.6) with `Open settings` → `settings/triggers`; else, when enabled,
log `global hotkeys registered`. Called at startup and whenever 5.1 #2 fires, so the failure toast repeats on every
hotkey change that still fails.

Native: `RegisterHotKey(hwnd, id, mods, vk)` on the thread that pumps `WM_HOTKEY`; failure with
`ERROR_HOTKEY_ALREADY_REGISTERED` (1409) = `in-use`. Electron does not pass `MOD_NOREPEAT` (8.2), so today holding
e.g. the stop or play/pause chord fires repeatedly; adding `MOD_NOREPEAT` is a deliberate behaviour change (open
question). F12 is reserved for debuggers and always fails. The 8.7 suspension and the shots-mode / no-reader guards
apply before any of this.

### 8.6 Failure toast copy (`describeFailures`, `shortcuts.ts:193-214`)

Sentences in this order, joined by `. `, followed by `. Pick another shortcut in Settings` (one failure) or
`. Pick other shortcuts in Settings` (more than one failure in total):

1. `types-character`, exactly one: `<accel> types “<char>” on your keyboard, so it isn't used as a shortcut`
   (dead key, i.e. empty char: `<accel> types a character on your keyboard, so it isn't used as a shortcut`).
   More than one: `<a>, <b> type characters on your keyboard, so they aren't used as shortcuts`.
2. `reserved`: `<a>[, <b>] is|are reserved for copying, pasting or Windows` (`is` for one, `are` for several).
3. `in-use` and `duplicate` together: `<a>[, <b>] is|are already in use`.

Lists are the accelerators as written in settings (trimmed, not normalized), joined by `, `, in registration order.
"One failure" vs "more than one" for the closing sentence counts **all** failures of every reason. When several
`types-character` failures exist their characters are not shown. Example:
`Ctrl+Alt+V types “@” on your keyboard, so it isn't used as a shortcut. Ctrl+C is reserved for copying, pasting or
Windows. Pick other shortcuts in Settings`.

### 8.7 Suspension while Settings records a shortcut (`index.ts:132-152`)

While a shortcut field in Settings is recording, our own registrations would swallow the combination the user is
pressing. The UI sends `app:suspend-shortcuts true/false`:

- `true`: (re)start a **30 s** safety timer (`SHORTCUT_SUSPEND_MAX_MS`) that resumes automatically (a UI that crashes or
  reloads mid-recording must not leave hotkeys off for good). If not already suspended: mark suspended, unregister
  ours, debug-log `hotkeys suspended while recording`.
- `false`: clear the timer; if suspended: unmark and re-apply hotkeys from the **current** settings (picks up a
  hotkey saved during recording; registration is skipped while suspended). The re-apply runs the full 8.5 flow, so
  a still-failing hotkey toasts again. `false` while not suspended does nothing else. Any payload other than the
  literal boolean `true` counts as `false`.

### 8.8 Hotkey actions (`index.ts:157-168`)

| Action | Effect |
|---|---|
| `readSelection` | selection service captures the selected text (clipboard round-trip) with source `hotkey`; errors logged `readSelection failed` |
| `readClipboard` | `reader.readClipboard()` (result ignored; empty clipboard → `Copy some text first`) |
| `playPause` | `reader.togglePlayback()`: if the player status is not `idle` **or** a synthesis session is current → `toggle`; else replay the newest history record (by the history list's default order, limit 1) if it is marked `cached` and the cache still has its audio (free); if there is none or the replay fails → info toast `Nothing is playing. Select some text and press <readSelection hotkey, trimmed>` (or `Nothing is playing. Select some text to read it` when hotkeys are off or that hotkey is empty). Errors logged `play/pause failed`. |
| `stop` | player command `stop`: sent to the widget (which owns playback) and synthesis is stopped |

---

## 9. Screenshot harness (`src/main/shots.ts`, `scripts/shots.mjs`)

Test tooling, run via `npm run shots` (build, then `scripts/shots.mjs`). The runner starts the **unpackaged** build
(`node_modules\electron\dist\electron.exe <projectRoot>`, cwd = project root, so `isPackaged = false`) with
`VOX_SHOTS_DIR=<root>\shots`, `VOX_DEMO=1`, `VOX_MOCK_TTS=1`, and `ELECTRON_RUN_AS_NODE` / `ELECTRON_RENDERER_URL`
removed (always the built pages). It deletes an old `report.json` first, kills the app after **120 s**, prints a
summary (displays, shots with `!` for blank ones, up to 40 failures / console errors / warnings), and exits **0** only
when the report says `ok` and there was no timeout, **1** otherwise (also when no report was written), **2** when the
build or the Electron binary is missing. Inside the app (shots mode, 1.1):

1. Collector, installed before any window: for every page, record console messages of level `error` or `warning` as
   `{ window, level, message (≤ 2000 chars), source, line }` where `window` is `app|widget|chip` from the URL
   (`/\/(app|widget|chip)\.html/`) else the URL or `webContents#<id>`. Also failures:
   `<window>: renderer gone (<reason>)`, `<window>: load failed <code> <desc> <url>`,
   `<window>: preload error in <path>: <message>`.
2. `runShots`: create the output dir; delete old `^(app|widget|chip)-.*\.png$` files and `report.json`; snapshot
   settings; start a **100 s** watchdog (on expiry: failure `watchdog: run exceeded 100 s`, write the report, hard
   exit with code **2**, no quit sequence). Wait for the widget, then the three steps below in order. A missing
   window adds `app window missing` / `widget window missing` / `chip window missing` and skips that step; an
   exception anywhere adds `harness error: <stack or message>` and skips the remaining steps.
   - **App**: content size 1280 × 820, positioned at the primary work area's bottom-right with 24 margin (not left/above
     the work area origin), shown without activation; wait for load and fonts, 800 ms. For theme in `paper, night`:
     set the theme, 500 ms; for page in `home, history, analytics, voices, settings, onboarding`: navigate, 900 ms;
     if `location.hash` does not start with `#/<page>`, set it and wait 700 ms; capture `app-<page>-<theme>.png`.
     Hide the window.
   - **Widget**: set `widget {enabled: true, compact: false, position: bottom-right, autoHide: false}` and
     `library.saveHistory: false` (the harness read must not enter the demo history). Mock-read a fixed demo
     paragraph (`source: manual, sourceApp: "Chrome", sourceTitle: "Essay"`; failure → `mock read failed: <code>
     <error>`); wait up to 12 s (100 ms steps) for `playing` with position ≥ 1.5 s (else
     `widget never reached playing (status=<s>)`); show; 300 ms; capture `widget-playing.png`. Compact on, 800 ms,
     `widget-compact.png`; compact off; stop; 600 ms. Loading/error are driven directly (the mock is too fast): send a
     synthetic session start `{ sessionId: "shots-loading", readId: "shots-loading", text: <licence clause>, title:
     preview, voiceName: settings.voiceName, sourceApp: "Word", estimatedDurationSec: round(len/15), chunkCount: 2,
     rate: 1, volume: 1, highlightWords: true, fromCache: false }`; show; 900 ms; `widget-loading.png`; then a session
     error `{ code: "quota", message: "Your ElevenLabs character quota is used up", fatal: true }`; 800 ms;
     `widget-error.png`; stop; 200 ms; hide. The fixed demo paragraph and licence clause are literal strings in
     `shots.ts:31-38`. A missed `playing` state is recorded but the run continues.
   - **Chip**: cursor = primary work area `(right - 320, bottom - 280)`; show `context`, 700 ms,
     `chip-context.png`; show `selection`, 700 ms, `chip-selection.png`; hide.
   - Always: clear the watchdog; restore theme, widget `enabled/compact/position/autoHide`, `library.saveHistory`;
     100 ms; write the report. Then the app quits normally.
3. Capture: image of the window's contents → PNG. Failures: `<file>: window missing`, `<file>: empty capture`,
   `<file>: capture looks blank`, `<file>: <error message>`. **Blank test**: zero size → blank; else sample every
   `step = max(1, floor(w*h/4000))`-th pixel, quantise each of the 4 channels to its top 4 bits, and count distinct
   values; more than 3 distinct → not blank. An empty capture is reported as `empty capture` (that check wins over
   `looks blank`). The PNG is written and listed in `shots` **even when** it is empty or blank. Success logs
   `captured <file> <w>x<h>`.
4. `report.json`:
   ```ts
   {
     ok: boolean,                 // no failures and no console errors
     generatedAt: string,         // ISO
     durationMs: number,
     app: { version: string, electron: string, packaged: boolean },
     displays: { id, scaleFactor, bounds: Rect, workArea: Rect, primary: boolean }[],
     windows: { main: Rect | null, widget: Rect | null, chip: Rect | null },
     shots: { file, width, height, bytes, windowBounds: Rect | null, blank: boolean }[],
     failures: string[],
     consoleErrors: ConsoleEntry[],
     consoleWarnings: ConsoleEntry[]
   }
   ```
   Log: `report written: <n> shots, <f> failures, <e> console errors`.

---

## 10. Packaging and installer (`electron-builder.yml`, `resources/installer.nsh`; electron-builder templates `templates/nsis/{multiUser,multiUserUi,assistedInstaller,uninstaller}.nsh`, `include/allowOnlyOneInstallerInstance.nsh`, `include/installUtil.nsh`)

| Item | Value |
|---|---|
| App id / AUMID | `com.vox.reader` |
| Product name / exe | `Vox` / `Vox.exe` |
| Copyright | `Copyright © Vox` |
| Version | from `package.json` (`0.1.0`) |
| Target | NSIS installer, x64 only |
| Installer file | `Vox-Setup-<version>.exe` in `dist\` |
| Install mode | Assisted wizard, not one-click (`oneClick: false`). With `perMachine: false` electron-builder does **not** force per-user: it shows an install-mode page ("Who should this application be installed for?"): "Only for me (`<user>`)" = per-user, no admin rights, preselected unless an existing per-machine install is found; "Anyone who uses this computer (all users)" = per-machine, triggers a UAC elevation (`allowElevation` default on), installs to `C:\Program Files\Vox`. The uninstaller shows the same page only when both kinds of install exist. |
| Install directory | Per-user default `<FOLDERID_UserProgramFiles>\Vox` (normally `%LOCALAPPDATA%\Programs\Vox`); a previous install's location (`InstallLocation` under `HKCU\Software\<APP_GUID>`, HKLM for per-machine) wins; `/D=<path>` overrides. A directory page follows (`allowToChangeInstallationDirectory: true`); if the chosen path does not contain `Vox`, `\Vox` is appended. The page is skipped on updates. |
| `APP_GUID` | `2d7b63f5-20ec-57ab-adfc-70d33e18b727` = UUIDv5(`com.vox.reader`, electron-builder namespace `50e065bc-3134-11e6-9bab-38c9862bdaf3`). Uninstall entry: `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\2d7b63f5-20ec-57ab-adfc-70d33e18b727` (HKLM for per-machine). Also the name of the "only one installer instance" mutex. A Rust installer that must upgrade an Electron install finds it here. |
| Finish page | "Run Vox" checkbox (launches the app as the non-elevated user; after an update it passes `--updated`, which the app ignores, 2.1) |
| Shortcuts | Desktop and Start menu, named `Vox` (carry AUMID `com.vox.reader`, which Windows toast notifications need); the uninstaller removes them and unregisters the AUMID unless run with `--keep-shortcuts` |
| Installer/uninstaller icon | `resources/icon.ico` |
| App data on uninstall | **kept** (`deleteAppDataOnUninstall: false`): `%APPDATA%\Vox` survives |
| Packaged app files | `out/**` (compiled main, preload, renderer) and `package.json` inside `app.asar`, plus production `node_modules`, minus `uiohook-napi` sources/libuiohook and its darwin/linux/win32-arm64 prebuilds (only win32-x64 ships) and koffi's C++ sources, vendor and docs |
| Bundled resources | `resources\*.ico`, `resources\*.png` → `<install>\resources\resources\`; `browser-extension\` → `<install>\resources\browser-extension\` (the user loads it via "Load unpacked") |
| Native modules | prebuilt only, never rebuilt (`npmRebuild: false`, `buildDependenciesFromSource: false`); unpacked from the archive (`asarUnpack`: `uiohook-napi`, `koffi`, `@koromix/**` = koffi's per-platform binary package) |

Uninstall cleanup (custom NSIS macro `customUnInstall`, run after the "app running" check and before the files are
removed; **skipped during updates**, i.e. when the uninstaller runs with `--updated`. Installing over any existing
install (upgrade, reinstall or downgrade) runs the old uninstaller with `--updated` first, and the entries must survive
that):

```
DeleteRegKey   HKCU "Software\Classes\SystemFileAssociations\<ext>\shell\VoxRead"
               for ext in .txt .md .markdown .log .csv .json .rtf .html .htm
DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vox"
DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "Vox"
```

Reason: these are written by the app itself to HKCU and would otherwise keep pointing at a deleted `Vox.exe`. The
extension list and verb name must stay in sync with the Explorer-verb code; the value name is the product name. The
dev-build value `Vox (dev)` is not removed. The cleanup touches the HKCU of whoever runs the uninstaller (for an
elevated per-machine uninstall, the elevating account).

**Closing a running Vox** (electron-builder 26 template `include/allowOnlyOneInstallerInstance.nsh`,
`_CHECK_APP_RUNNING`; runs in both installer and uninstaller):

1. If Vox is running (PowerShell available and not `Restricted`: any process whose image path starts with the install
   dir; otherwise `tasklist` for `Vox.exe` owned by the current user): on an update wait 300 ms + 1000 ms and continue;
   otherwise show `Vox is running.\nClick OK to close it.\nIf it doesn't close, try closing it manually.` (OK/Cancel;
   Cancel aborts setup). Status line `Closing running Vox...`.
2. Kill without force: with PowerShell this is `Stop-Process -Id <pid>` for each matching process, which is a **hard
   `TerminateProcess`** (no `WM_CLOSE`, no chance to save); only without PowerShell is it
   `taskkill /IM Vox.exe /FI "PID ne <installer pid>" /FI "USERNAME eq %USERNAME%"` (no `/F`), which posts `WM_CLOSE`
   to Vox's top-level windows; that is the case 3.7's widget/chip handling turns into save-and-quit.
3. Sleep 300 ms; if still running sleep 1000 ms, then force-kill (`Stop-Process -Force` / `taskkill /F`); if still
   running, `Waiting for "Vox" to close.`, sleep 2000 ms and retry; from the second round show
   `Vox cannot be closed. \nPlease close it manually and click Retry to continue.` (Retry/Cancel).

So on most machines an install or uninstall kills Vox outright. Everything Vox persists must survive
`TerminateProcess` at any instant (atomic writes), and a native installer that wants a clean save should ask the app
to quit (e.g. post `WM_CLOSE` to its hidden top-level window, or send a quit command over the single-instance pipe)
and wait at least ~1.3 s before killing it.

---

## 11. Constants

| Constant | Value | Source |
|---|---|---|
| `RETENTION_INTERVAL_MS` | 6 h | `index.ts:30` |
| `RETENTION_CHANGE_DELAY_MS` | 60 000 ms | `index.ts:35` |
| `SHUTDOWN_TIMEOUT_MS` | 3000 ms | `index.ts:36` |
| `FORCE_EXIT_MS` | 4000 ms | `index.ts:37` |
| `SHORTCUT_SUSPEND_MAX_MS` | 30 000 ms | `index.ts:132` |
| Win32 helper load wait at startup | 1500 ms | `index.ts:489` |
| `WIDGET_MARGIN` | 24 DIP | `windows.ts:36` |
| `TITLEBAR_HEIGHT` | 44 DIP | `windows.ts:37` |
| Default widget size | 380 × 168 | `windows.ts:38` |
| Widget size bounds | min 40 × 24, max 1200 × 900 | `windows.ts:697` |
| Widget card hit min | 8 × 8 | `windows.ts:248` |
| Near-cursor offsets | +16 / +20, flipped −16 | `windows.ts:90-99` |
| Programmatic-move guard | 500 ms | `windows.ts:628` |
| Widget ready fallback | 1500 ms after load; wait cap 5000 ms | `windows.ts:637, 650` |
| `HOVER_POLL_MS` | 100 ms | `windows.ts:456` |
| Default chip sizes | context 153 × 38, selection 82 × 38 | `windows.ts:40-43` |
| Chip size bounds | min 32 × 24, max 480 × 160 | `windows.ts:811` |
| Context menu sizes | small 220 × 240, guess 320 × 360, large 360 × 480 | `windows.ts:130-134` |
| `CHIP_GAP` / `CHIP_GAP_V` / `SELECTION_CHIP_GAP` | 18 / 20 / 20 | `windows.ts:136-139` |
| `CHIP_EXIT_MS` | 140 ms (UI animation 120 ms) | `windows.ts:768` |
| Main window | 1180 × 780, min 940 × 640 | `windows.ts:506-509` |
| `CRASH_WINDOW_MS` / `MAX_CRASH_RELOADS` | 60 s / 3 reloads (4th crash in 60 s is final) | `windows.ts:396-397` |
| `TOOLTIP_MAX` | 127 UTF-16 units | `tray.ts:21` |
| Worst case quit request → process exit | ≈ 7 s (3 s flush cap + 4 s hard exit armed afterwards) | `index.ts:415, 581` |
| `customX/customY` clamp | integer, ±100 000 | `settings-store.ts:59-63` |
| `server.port` default / range | 47823 / 1024-65 535 | `shared/constants.ts:12`, `settings-store.ts:168` |
| Installer graceful-close window before force kill | 300 ms + 1000 ms | electron-builder `allowOnlyOneInstallerInstance.nsh` |
| Shots runner timeout | 120 s | `scripts/shots.mjs:13` |
| Log rotation | 1 MiB, args ≤ 2000 chars | `log.ts:47-48` |
| Shots watchdog / app size | 100 s / 1280 × 820 | `shots.ts:28-29` |
| Widget auto-hide / hold retry | 4000 / 1500 ms | `renderer/src/widget/controller.ts:33-34` |
| Widget toast / with action / hover recheck | 2500 / 5000 / 1000 ms | `renderer/src/widget/Widget.tsx:14-16, 121` |

---

## 12. Implementation pitfalls for a native Windows port

1. **Keep a hidden top-level window.** `WM_QUERYENDSESSION`, `WM_ENDSESSION`, `TaskbarCreated` and the installer's
   `WM_CLOSE` reach top-level windows only (not `HWND_MESSAGE` windows). Treat `WM_CLOSE` on any non-main window as
   "save synchronously, then quit".
2. **Non-activating + draggable**: `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, `MA_NOACTIVATE`, and
   `HTCAPTION` only on handle rectangles. Never return `HTCAPTION` for a control's rectangle, even for one frame after
   a layout change (the bug the original's CSS rules exist to avoid).
3. **Show without activating, then re-assert topmost** (`SWP_NOACTIVATE`), every time. The chip must also be raised
   above an already open context menu.
4. **DPI**: all constants are DIP. Place windows using the target monitor's work area and scale. The original's
   `customX/customY` are Electron DIP screen coordinates (see open questions).
5. **Single instance**: named mutex + IPC carrying raw argv and cwd; the second process calls
   `AllowSetForegroundWindow` before notifying; the first only acts after its own bootstrap finished.
6. **Quit must be idempotent and bounded**: one shutdown in flight, 3 s cap on flushes, 4 s hard exit.
7. **Hotkeys**: check reserved chords and the AltGr conflict before `RegisterHotKey`; produce the exact toast copy.
   Decide deliberately on `+`/`Plus` (Electron adds Shift) and `MOD_NOREPEAT` (Electron does not set it), 8.2.
8. **A drag ends only if the window moved**: treat `WM_EXITSIZEMOVE` as "user dragged" only when a `WM_MOVING` was
   seen in that loop, or a click on a text handle silently switches the placement to `remember`.
9. **Widget z-order vs taskbar**: Electron's `floating` level puts the widget just under the taskbar inside the
   topmost band; the chip (`pop-up-menu`) goes above it. Reproduce or consciously drop this (6.1).
10. **Expect to be hard-killed**: electron-builder's installer usually `TerminateProcess`es the app (section 10);
    `WM_CLOSE` arrives only on machines without usable PowerShell. All stores need atomic, crash-safe writes.
11. **Main window geometry is not persisted**: every launch is 1180 × 780, centred. Adding persistence is a feature,
    not parity.
