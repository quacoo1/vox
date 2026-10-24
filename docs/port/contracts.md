# Vox port spec — Shared contracts, data model, settings schema, IPC surface

This document is the backbone for the Rust rebuild of **Vox** (Windows tray utility: select text anywhere, ElevenLabs
reads it aloud, a floating player shows playback). It describes the current Electron + React + TypeScript app
(v0.1.0) precisely enough to rebuild it without opening the TypeScript.

Citations are `path:line` relative to the old source root `C:/Users/Kwaku/Desktop/Desktop Widget`.

**In scope (read completely):** `docs/SPEC.md`, `src/shared/constants.ts`, `src/shared/defaults.ts`,
`src/shared/types.ts`, `src/shared/ipc.ts`, `src/shared/listen-log.ts`, `src/preload/index.ts`, `src/main/ipc.ts`,
`src/renderer/src/lib/mock-vox.ts`.
**Consulted to resolve behavior behind the contracts:** `src/main/services/{settings-store,history-store,json-file,audio-cache,analytics,paths}.ts`,
`src/main/{reader,windows,index,shortcuts,tray,log}.ts`, `src/main/tts/{elevenlabs,engine}.ts`,
`src/main/integrations/local-server.ts`, `src/main/selection/service.ts` (notices, timing constants),
`src/renderer/src/widget/{controller,player}.ts`, `src/renderer/src/app/settings/{logic,PlaybackSection}.ts(x)`.

Sibling port specs go deeper on their own subsystems: `persistence.md`, `reader-playback.md`, `selection.md`,
`shell.md` (bootstrap, windows, tray, hotkeys, screenshot harness), `tts.md`. Where this file and a sibling disagree,
re-check the cited source line.

Terminology: **core** = the old Electron *main* process (owns state, network, OS hooks, persistence). **UI surfaces**
= the three old renderer windows: **app** (main window), **widget** (floating player), **chip** (the "Read with Vox"
pill). In Rust these may live in one process; the contracts below stay the same, only the transport changes.

---

## 1. Architecture (from `docs/SPEC.md`)

### 1.1 Product shape

- Tray app. Main window pages: **Home, History, Analytics, Voices, Settings** (+ Onboarding). SPEC.md:1-4.
- Product name lives in one constant (`APP_NAME = 'Vox'`, `READ_WITH_LABEL = 'Read with Vox'`). UI strings must
  never hard-code "Vox"; always derive from the constant. SPEC.md:9-10, constants.ts:2-4.
- Original visual reference: Nothing-OS widgets (black rounded cards, dot-matrix numerals, dotted progress bars,
  single red accent). The user intends a redesign from a new reference image; §1.11 records the old design language
  as information only.

### 1.2 Components and data flow (SPEC.md:26-47)

```
                       ┌──────────────────── core ─────────────────────┐
 global mouse/kbd hook │ SelectionService ── 'read' ─┐                  │
 global hotkeys        │ shortcuts ──────────────────┤                  │
 LocalServer (:47823)  │ ── browser extension ───────┤                  │
 argv / 2nd instance   │ ── --read-file / --read-clipboard / --read-text┤
 tray menu             │ ── read clipboard ──────────┤                  │
                       │                    ReadController (reader)     │
                       │   history-store ◄──┤  │  ├─► audio-cache       │
                       │                    │  ▼                        │
                       │             TtsEngine ──► ElevenLabsClient     │
                       └───────────────┬────────────────────────────────┘
                     tts:* events      │        player:state (widget → core)
                                       ▼        ▲
                        Widget (owns audio playback + Windows media session)
                                       │ player:state relayed by core
                                       ▼
                        App window (Home/History/Analytics/Voices/Settings)
                        Chip window ("Read with Vox" pill near the cursor)
```

Key ownership rule: **the widget owns audio playback** (even while hidden). The core never plays audio; it streams
synthesized chunks to the widget and learns the playhead back from the widget's `player:state` reports. The app
window only mirrors the widget's state (relayed by core) and sends commands through core.

### 1.3 Windows and their platform-level behavior

| Window | Old Electron config | What it means natively (Win32) |
|---|---|---|
| **Widget** (windows.ts:600-642) | frameless, `transparent`, bg `#00000000`, `alwaysOnTop` level `'floating'`, `skipTaskbar`, `focusable:false`, `resizable/maximizable/minimizable/fullscreenable:false`, `hasShadow:false`, title `"Vox Player"`, `backgroundThrottling:false`, autoplay without user gesture | Popup window, per-pixel alpha only at rounded corners (layered/DirectComposition). `HWND_TOPMOST`. `WS_EX_TOOLWINDOW` (no taskbar / Alt+Tab). `WS_EX_NOACTIVATE`: clicking it must **never** take focus from the document being read. Timers/audio must run at full rate while hidden. Default size 380×168 DIP until the UI reports its size (windows.ts:38). |
| **Chip** (windows.ts:725-756) | same as widget, title `"Vox Chip"`, level `'pop-up-menu'` | Topmost no-activate tool window. Default 153×38 (context mode) / 82×38 (selection mode) DIP (windows.ts:40-43). |
| **App** (windows.ts:502-547) | 1180×780, min 940×640, title `"Vox"`, icon `resources/icon.ico`, `titleBarStyle:'hidden'` + `titleBarOverlay` (native min/max/close, height 44, initial colors from `THEME_COLORS` of the resolved theme), bg per theme, menu removed, `show:false` until first paint | Normal top-level window with custom-drawn title bar area and native caption buttons whose colors the UI sets (`app:set-titlebar-theme`). **Always created at startup** (kept hidden when it should not show; see "App" below) with initial route `onboarded ? #/home : #/onboarding` (index.ts:466-471). |

Show/hide semantics:

- **Widget** is created hidden at startup and lives for the whole app lifetime. `widget.enabled = false` means it is
  never *shown*, but it still plays audio. It is shown with *show-without-activate* (`showInactive()` ≙
  `ShowWindow(SW_SHOWNOACTIVATE)` / `SetWindowPos(..., SWP_NOACTIVATE|SWP_SHOWWINDOW)`), re-asserting topmost each
  time. It is repositioned only when it was hidden (or on placement-setting change, or a display change). SPEC.md:49-56,
  windows.ts:678-685.
- **Widget readiness** (windows.ts:316-319, 635-653): core keeps a one-shot "widget ready" latch. It is set by the first
  `player:state` (even an invalid one), the first valid `widget:set-size`, or 1500 ms after the widget page finished
  loading (fallback). `whenWidgetReady(timeout = 5000 ms)` resolves immediately once the latch is set, else at the
  latch or after 5 s, whichever is first. Reads, cached replays and the shell's toasts (hotkey / Explorer menu / bridge
  failures) await it so the first `tts:session-start` / `app:toast` is not sent before the widget listens (other toasts
  are sent at once). The latch is **never re-armed**: after a widget UI crash/reload
  nothing waits again (the reloaded widget reports its state once on load instead, §5.4).
- The widget window **is exactly the card** (no transparent shadow margin; a flat card with a hairline border). The UI
  measures itself and reports `{width, height, hit}` (`widget:set-size`); core resizes the window to that content size,
  keeping the anchored edges fixed (a window in the lower half grows upward, right half grows leftward). SPEC.md:52-56,
  windows.ts:59-67, 116-123, 696-712.
- **Chip**: shown with show-without-activate at a position core computes beside the native context menu. Windows keeps a
  transparent Electron window at least ~38 DIP tall (and may round width up 1 px), so the chip root has
  `min-height: 38px` with the pill centred, a press anywhere in the window counts as a chip press, and core gives the
  input hook the window's *real* bounds after placing it. On hide, core first sends `chip:hide` so the UI plays a
  120 ms exit animation; during it the window ignores the mouse (click-through, the only time it does); after
  `CHIP_EXIT_MS = 140` ms the window is hidden (only if no new show happened meanwhile). Showing again cancels the
  pending hide and clears click-through. `chip:hide` is sent on **every** hide request, even when the chip is already
  hidden; the hook's chip bounds are cleared at once (the chip is inert for the hook during the exit animation). Enter/exit
  animations only scale down, never translate. SPEC.md:83-93, windows.ts:767-804. Show order: clear pending hide → set
  mode → round the cursor point → send `chip:show` → place (setBounds) → give the hook the window's real bounds → clear
  click-through → show-without-activate → re-assert topmost → bring to top.
- **App**: close → hide to tray when `app.closeToTray`, else quit (once quitting, close is allowed). Shown on start
  unless `--hidden`, a CLI read action, screenshot mode, or (`app.startMinimized` **and** onboarded). `showMain`
  (windows.ts:564-575): no window → create it shown with the given route (default `onboarded ? home : onboarding`);
  else navigate (if a route), restore if minimized, show, bring to top, focus. Hash routes:
  `#/home`, `#/history`, `#/analytics`, `#/voices`, `#/settings`, `#/settings/<section>`, `#/onboarding`.
  SPEC.md:97-103, index.ts:466-471.

### 1.4 Window rules (hard-won; keep the reasons)

1. **Drag handles, never a draggable card with holes.** Only text/labels are drag handles (source label row,
   sentence/title body when it holds no control, voice name, time, status label, toast text; on the compact pill the
   full-height time column, the middle when it holds neither a control nor the stopped-early note, and an invisible
   grip filling the rest of the row). Never make the whole card a drag region with no-drag holes for controls, and never
   put a control inside a handle. Reason: Chromium sends new drag regions one frame after a layout change and none while
   hidden, so the first frame after a view switch or show still had the previous view's regions and a press on a control
   became a window move (click lost). For the same reason the top bar **reserves the Speed pill's slot** on the idle and
   error cards. SPEC.md:56-65.
   *Native port:* compute handle rectangles from the **current** layout inside `WM_NCHITTEST` (return `HTCAPTION` only
   for handle rects, `HTCLIENT` elsewhere) and the lag disappears — but keep the rule "a control never lies inside a
   handle". Alternative: keep everything `HTCLIENT` and start a move on mouse-down over a handle with
   `ReleaseCapture(); SendMessage(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0)`; then the page keeps hover events and the
   cursor poll below becomes unnecessary.
2. **A handle's region is its own box only.** In CSS the region property inherits and every transformed descendant added
   its rectangle (a long sentence scrolled hidden lines upward via `translateY`, and those hidden lines became a title
   bar over Speed and Close). Fix was: every element resets to "no region", only handles opt in. *Native:* hit-test only
   against the handle's own visible rect, clipped. SPEC.md:66-70.
3. **Handles get no mouse moves**, so they cannot show tooltips; text whose tooltip matters (the stopped-early reason)
   must not be a handle. SPEC.md:71-72.
4. Buttons drawn smaller than 32 px get an invisible hit area ≥ 32×32. SPEC.md:74.
5. **User drag end → remember position.** The OS "moved" notification at the end of a move loop
   (≙ `WM_EXITSIZEMOVE` after a caption drag) stores `widget.customX/customY` (window top-left, DIP) and switches
   `widget.position` to `'remember'`. Ignore such notifications within **500 ms** (strictly: handled only when
   `now − lastOwnMove > 500`) of core's own move/resize, and ignore them while the widget is hidden. The anchor is
   recomputed by quadrant from the new bounds. Because that settings write changes the placement keys while the widget
   is visible, core immediately re-places it through the `remember` rule — which **clamps it back inside the work
   area** of the display matching the saved rect (a widget dragged partly off-screen snaps back in).
   SPEC.md:74-76, windows.ts:626-629, 714-721, index.ts:313-314, 453-455.
6. **No click-through toggling while shown.** The old app used to toggle "ignore mouse events" from a 40 ms cursor poll
   around a shadow margin; that raced fast clicks onto the app below, and `WindowFromPoint` skips a window while it
   ignores the mouse. Now windows always take the mouse (chip exception: exit animation). SPEC.md:77-82,
   windows.ts:446-454.
7. **Widget hover is decided by core.** Windows treats drag handles as caption, where the page gets no pointer events,
   so while the widget is visible core polls the cursor every **100 ms** (`HOVER_POLL_MS`) and sends `widget:hover`
   (`true`/`false`) **only on change**; the poll stops when the widget is hidden (state becomes "unknown"; a `false` is
   sent if it was `true`). Hover = cursor inside the reported `hit` rect (or the whole window if no valid hit), tested
   half-open: `0 ≤ cursor.x − win.x − hit.x < hit.width` and likewise for y (cursor and bounds in screen DIP). A failed
   cursor query keeps the previous value. The poll (re)starts on every show and on every visible resize. The
   widget's auto-hide and toast dismissal wait while hovered. windows.ts:456-498.
8. **Chip topmost caveat.** On Windows, topmost windows are not ordered by "level": a menu opened later can sit above
   the chip. So the hook's click test also checks that the window under the cursor is this app's own non-activating
   window. SPEC.md:87-90.
9. **Close requests.** Widget and chip cannot be closed by the user. A `WM_CLOSE` from outside (taskkill without /f, an
   installer) is treated as **quit** (after a synchronous save). Right-click on a caption/handle would open the system
   menu whose Close destroys the window, so the system menu is suppressed on these windows
   (≙ swallow `WM_NCRBUTTONUP`/`WM_CONTEXTMENU` on caption / `WM_SYSCOMMAND SC_KEYMENU`). Disabling "closable" is not
   an option because it also blocked the app's own quit. SPEC.md:94-96, windows.ts:430-444, index.ts:457-461.
10. **Quit sequence.** On quit (index.ts:398-417): mark "quitting" (windows may now close), clear the retention and
    hotkey-suspension timers, unregister hotkeys, stop the hook, persist playback (§6.4), stop synthesis (the widget
    is *not* sent a stop), then in parallel flush settings, flush history, flush any pending clipboard restore and stop
    the bridge — waiting at most **3 s** (`SHUTDOWN_TIMEOUT_MS`) — then destroy the tray icon and quit. The hard exit
    (`FORCE_EXIT_MS` = **4 s**) is armed only *after* cleanup finished, so the worst case is ≈ 7 s after the quit request.
    A second quit request while flushing just waits for the first. Once cleanup is done, "all windows closed" re-issues
    the quit (a close racing the quit could cancel it); before that, closing every window never quits (tray app).
    SIGINT/SIGTERM also go through this sequence. index.ts:36-37, 55-60, 398-417, 562-583.
11. **Log-off/shutdown.** On `WM_QUERYENDSESSION`/`WM_ENDSESSION` (any window; every window gets them, the first save
    wins, later ones find nothing dirty) core folds the latest playback into history (`persistPlayback`, §6.4), then
    writes pending settings and history **synchronously** (`flushSync`, §1.7) — the normal async quit never runs there
    and the process may be killed as soon as the handler returns. An outside `WM_CLOSE` on widget/chip runs the same
    synchronous save before starting the quit (a graceful kill may be followed by a forced one within a second or two).
    index.ts:388-396, 457-461, windows.ts:417-428.
12. **UI crash recovery.** A crashed UI surface is reloaded, at most 3 times per sliding 60 s window (a 4th crash
    within 60 s → left alone and logged `<name> renderer keeps crashing; not reloading again`). No reload while quitting
    or for a clean exit. windows.ts:396-415.
13. Coordinates: the global hook reports **physical pixels**; convert to DIP (per-monitor DPI) before comparing with
    window bounds; keep the chip inside the target display's work area. SPEC.md:177-178.
14. **Display changes** (display removed, or any display's metrics/work area changed): hide the chip and, if the widget
    is visible, re-place it from settings. OS theme change while `app.theme = 'system'`: re-apply the theme (window
    background). index.ts:367-378.
15. **UI-surface hardening (webview UIs only).** Links that try to open a new window or navigate away from the app's own
    pages are cancelled; if the URL is http(s) it is opened in the default browser instead. The only browser permission
    granted to UI pages is "sanitized clipboard write" (History's "copy text"); everything else is denied; embedded
    webviews are refused. A native UI just opens http(s) links via `app:open-external`. index.ts:62-82.

### 1.5 Trigger layers (SPEC.md:105-137)

Windows has no API to add items to other apps' text context menus, so reading is triggered in layers (all configurable
under Settings → Triggers):

| Layer | Setting | Behavior / constants |
|---|---|---|
| Context chip (default **on**) | `triggers.rightClickChip` | Hook watches *selection gestures*: left-drag > **6 px**, double/triple click, shift+click, Shift+arrows / Ctrl+A. A right-click (button-2 down) while the hint is live shows the chip **beside** (never on) the native context menu. Hint cleared by a plain left click (no drag), typing a non-modifier key, or after **60 s**. Not shown during exclusive fullscreen/presentations; a drag counts as text selection only if the cursor showed the I-beam/caret (apps drawing their own cursor get the benefit of the doubt). Never when the foreground process is Vox itself or listed in `triggers.excludedApps`. |
| Selection popup (default **off**) | `triggers.selectionPopup` | PopClip-style compact `Read` chip just above-right of the cursor right after a selection gesture; hides on next mouse-down elsewhere, any key, or after **4 s**. |
| Global hotkeys | `triggers.hotkeysEnabled`, `triggers.hotkeys.*` | `Ctrl+Alt+R` read selection, `Ctrl+Alt+V` read clipboard, `Ctrl+Alt+P` play/pause, `Ctrl+Alt+S` stop. Never registered: `RESERVED_ACCELERATORS` and Ctrl+Alt(+Shift) chords that type a character as AltGr on an installed layout (checked with `ToUnicodeEx` per layout; AltGr+V is '@' on Hungarian). Failures are reported in one toast. |
| Browser extension | `triggers.browserExtension`, `server.port` | Chrome/Edge extension adds "Read with Vox" to the browser menu and calls the localhost bridge (§1.10). |
| Explorer entry | `triggers.explorerMenu` | HKCU registry verb "Read with Vox" for `.txt .md .markdown .log .csv .json .rtf .html .htm` (no admin; uninstaller removes it). Launches the exe with `--read-file <path>`. |
| Tray / app | — | Tray "Read clipboard"; in-app "Paste & read" box (`reader:read-text`). |

Chip click: the chip UI calls `chip:activate` on pointer-down (left button; other buttons → `chip:dismiss`), **and** the
hook independently detects a left mouse-down inside the chip bounds (some context menus swallow the click that dismisses
them). Core de-duplicates within **300 ms**. Chip mode → read source: `context` → `'context-chip'`, `selection` →
`'selection-popup'`. SPEC.md:135-137, selection/service.ts:100, 794.

Selection capture (clipboard round-trip) summary, SPEC.md:139-175: read foreground window first (password managers
refused outright); copy chord per app (`Ctrl+C` default; `Ctrl+Insert` for terminals and editors with integrated
terminals; none for copy-on-select terminals PuTTY/KiTTY/MobaXterm); wait ≤ **700 ms** until no modifier keys are
physically held (+ ~**120 ms** for context-chip captures so the native menu can close); snapshot clipboard; detect the
copy by clipboard sequence number (no clearing); poll every **15 ms** up to **450 ms** (+ up to **1.5 s** once changed
but text not yet readable); context chip with Ctrl+C only: one `Escape` + retry; privacy markers
(`ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory`=0, `CanUploadToCloudClipboard`=0) →
never read, restore immediately, emit a notice; restore the snapshot ~**150 ms** after reading when
`triggers.restoreClipboard`; if nothing arrived, keep watching ~**1.5 s** for a late copy. Friendly app names map from
exe (chrome→Chrome, msedge→Edge, winword→Word, acrord32/acrobat→Acrobat, notepad→Notepad, code→VS Code,
firefox→Firefox, outlook/olk→Outlook, onenote→OneNote, slack→Slack, discord→Discord, obsidian→Obsidian,
notion→Notion, teams/ms-teams→Teams, explorer→Explorer, brave→Brave, opera→Opera, vivaldi→Vivaldi,
sumatrapdf→SumatraPDF, kindle→Kindle, else title-case of the exe stem). If the hook or the FFI fails to load, the app
keeps working (hotkeys, extension, tray) and `AppInfo.nativeHookAvailable` is `false`.

### 1.6 Reading pipeline summary (SPEC.md:183-225; details in the reader/TTS specs)

1. No API key (and not mock) → reject `no-api-key` + toast with action → `#/settings/account`. This check runs
   first, before the text is even looked at (reader.ts:305-314).
2. Pre-cut raw input to `max(text.maxChars × 4, 200 000)` chars (cut at the last `\n` if it lies in the final tenth;
   never leave a lone high surrogate at the end), clean (strip zero-width chars, collapse whitespace, de-hyphenate
   line-wrap hyphens, drop URLs/emails per settings; if cleaning throws, use the trimmed raw text), reject empty,
   truncate to `text.maxChars` (prefer the last whitespace if it lies in the final tenth; never split a surrogate pair;
   trim the end). Toast "Reading the first N characters" if either cut happened (reader.ts:106-126, 316-333).
3. Create `ReadRecord` (id = random UUID v4), add to history if `library.saveHistory`.
4. Await widget readiness (≤ 5 s, §1.3), start TTS with `title = previewTitle(text)` (§3.4); show the widget if
   `widget.enabled`.
5. On engine `complete`: cache audio if `library.cacheAudio`, prune to `cacheLimitMB`, update record
   (`audioDurationSec`, `cached`, `charactersBilled`). On `stopped`: record `charactersBilled` only; partial audio never
   cached; after a chunk failure the record gets `error = "Only part of the text could be synthesized"` and playing to
   the end does not mark it completed.
6. On widget `player:state`: feed the playhead to the engine (bounded look-ahead), update `listenedSec`, `completed`,
   `maxPositionSec`, and the hour ledger `listenLog` (throttled to 1 write / **2 s**, plus a write on every status
   change), relay to app window, refresh tray.
7. `replay(readId)`: cached → play cached (no credits), listening accumulates into the original record; else
   re-synthesize the stored text as a new read with source `replay`; `not-cached` only if the full text was not stored.
   A fresh replay uses the record's voice id/name (falling back to the current settings voice when empty) but the
   **current** model, output format, voice settings, language and text settings.

TTS: chunks ≤ 220 chars first, then ≤ 900, sentence boundaries; `POST /v1/text-to-speech/{voice_id}/with-timestamps`;
bounded look-ahead `LOOKAHEAD_SEC = 120` with the first 2 chunks free; retries 429/5xx ×3 (1 s, 2 s, 4 s); mock mode
(`VOX_MOCK_TTS=1`): 16-bit mono 22.05 kHz WAV, quiet 220 Hz tone, duration = chars/15 s.

Widget playback (SPEC.md:227-259): chunks played back-to-back; output warm-up (inaudible ±1 LSB loop keeps codecs /
Bluetooth awake; start only after 1 s of rendered signal measured on the audio clock; max wait 3 s; release 30 s after
end/stop/fail, 60 s after pause); position = finished chunk durations + current time; duration estimate for the
unsynthesized remainder = `remainingChars / 15 / speed`; playback rate with **pitch preservation** (Chromium
`preservesPitch=true` ≙ a time-stretch such as WSOLA/phase vocoder, not resampling); reports `player:state` on every
status change and every **500 ms** while playing; `listenedSec` accumulates wall-clock time only while audible *and*
media time advances (capped at media progress / rate); Windows media session (SMTC) metadata title = text preview,
artist = voice name, album = `APP_NAME`, handlers play/pause/stop/previous/next (= sentence skip) when
`triggers.mediaKeys`; auto-hide **4 s** after `ended` (retry every 1.5 s while hovered or a toast is up; never for a
read that stopped early).

### 1.7 Persistence layout (SPEC.md:261-280)

`userData` = `%APPDATA%\Vox` (Electron default from productName `Vox`); demo/screenshot mode uses
`%APPDATA%\Vox-Demo` and must never touch the real one.

| Path (under userData) | Content |
|---|---|
| `settings.json` (+ `.bak`, `.tmp`, `.tmp-sync`, `.corrupt`) | `Settings` minus `hasApiKey`, pretty JSON (2 spaces). §4.4 |
| `secrets.bin` (+ `secrets.bin.tmp` during a write; **no** `.bak`) | API key encrypted with Electron `safeStorage` (Windows: DPAPI-protected key). Never sent to UI. §4.5 |
| `history.json` (+ `.bak`, `.tmp`, `.tmp-sync`, `.corrupt`) | `{ "version": 1, "records": ReadRecord[] }`, newest first, compact JSON (no whitespace). |
| `audio-cache/<readId>/manifest.json` + `<index>.<ext>` (ext mp3 / wav / ogg / bin) | Per-chunk audio + alignment for free replays; LRU by manifest `lastAccess`. Never named `cache/` (collides case-insensitively with Chromium's `Cache`). |
| `Session/` | Chromium session data (irrelevant for a native port). |
| `logs/main.log` (rotated at 1 MiB = 1 048 576 bytes to `main.1.log`) | Log lines `ISO-time LEVEL [scope] text`; API keys redacted (regex `\bsk_[A-Za-z0-9]{16,}\b` → `sk_***`, and `(xi-api-key["':\s=]+)[^\s"',}]+` → `$1***`, case-insensitive); each argument cut to 2000 chars + `…`; never log text being read. log.ts:47-117. |

JSON store semantics (json-file.ts:19-302; full detail in `persistence.md`):

- **Atomic write** (`writeFileAtomic`): create the parent dir; open `<file>.tmp`, write, fsync (≙ `FlushFileBuffers`),
  close; rename over the target (≙ `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`). Opening the temp file, the rename, the
  copy fallback, the `.bak` write and every read are retried on `EPERM/EBUSY/EACCES` (≙ `ERROR_ACCESS_DENIED`,
  `ERROR_SHARING_VIOLATION`, `ERROR_LOCK_VIOLATION`) after 15, 40, 90, 180, 350 ms — at most 6 attempts — because
  antivirus/indexers briefly hold files open. If the rename still fails (scanners that open without
  `FILE_SHARE_DELETE`), copy the temp file over the target in place (not atomic, but `.bak` still holds the previous
  commit until the copy succeeded) and delete the temp file. With `backup` (settings, history): then rewrite
  `<file>.bak` with the same bytes, **non-atomically** (a torn `.bak` is harmless, the main file was just committed), so
  `.bak` always mirrors the last commit, never an older state holding deleted records. Writes to the same path are
  serialized (one at a time, in call order) so two writers never share `<file>.tmp`. Secrets, cache manifests and the
  history export use the same routine **without** `.bak`.
- **Load**: OK → use it; **missing → defaults (no `.bak` fallback: deleting the file is a deliberate reset)**; any
  other read error, an empty/whitespace-only file, or invalid JSON → copy the unreadable file to `<file>.corrupt`
  (best effort, overwrites an older `.corrupt`) and try `<file>.bak`; `.bak` missing or bad → defaults. A UTF-8 BOM
  (Notepad) is stripped before parsing.
- **Debounce** (`schedule()`): mark dirty; the first `schedule()` of a burst records its time `first`; every call
  re-arms one timer to `max(0, min(debounce, first + maxWait − now))`. Settings: debounce 250 ms, max wait 2 s
  (default max wait = 8 × debounce). History: 2500 ms / 10 s (a playing read patches the record every ~2 s; a shorter
  debounce would rewrite a multi-MB file every 2 s).
- **Write**: serialized; skipped when not dirty; the snapshot is taken at write time (latest state). A failed write
  marks dirty again and retries after `1 s × failures` (only if no timer is pending), at most 5 times; after that it
  waits for the next change.
- **`flush()`** (quit): cancel the timer, write now, wait for any in-flight write. Never rejects.
- **`flushSync()`** (log-off): cancel the timer; write when dirty **or while an async write is in flight** (its snapshot
  is not committed yet) using `<file>.tmp-sync` (separate name so it cannot collide with the async `.tmp`), fsync,
  rename (copy fallback, temp left behind if it cannot be deleted), `.bak`; no retries. If an async write was in flight,
  schedule one more async write after it settles, because the older in-flight snapshot may rename over the newer file.
  Never throws.
- **`discardCorruptCopy()`**: delete `<file>.corrupt` (history calls it after any removal, so deleted reads do not live
  on there).

### 1.8 Runtime modes, environment, CLI

| Input | Effect | Source |
|---|---|---|
| `VOX_DEMO=1` (or `true`) | Demo: separate userData `%APPDATA%\Vox-Demo`, seeded ~120 records over 60 days (only when history is empty), `onboarded` forced true (written before the settings listener exists, so no broadcast), mock TTS, demo voices/models/account when no key, no autostart/explorer-menu changes, no bridge-port toast. | log.ts:25-31, index.ts:84-89, 432-435 |
| `VOX_MOCK_TTS=1` (or `true`) | Mock synthesis (no network, no API key needed). Implied by demo. | log.ts:28 |
| `VOX_SHOTS_DIR=<dir>` | Screenshot harness (implies demo; path resolved to absolute). No hotkeys, no hook, no selection re-configuration, no bridge; the app window is not shown at start; Chromium occlusion detection off. Harness details in `shell.md`. | log.ts:26, index.ts:155, 268, 298, 359, 466-471, 521 |
| `VOX_DEBUG=1` (or `true`) | Debug-level logging. | log.ts:28 |
| `ELEVENLABS_API_KEY` | Fallback API key when `secrets.bin` is absent/undecryptable/empty (read once at init; never persisted). | settings-store.ts:241-242 |
| `--hidden` | Start without showing the app window. | reader.ts:89-91 |
| `--read-clipboard` | Read the clipboard. | reader.ts:92-94 |
| `--read-file <path>` / `--read-file=<path>` | Read a text file (Explorer verb). Source `explorer`, sourceApp `Explorer`, sourceProcess `explorer.exe`, sourceTitle = file name; reads `maxChars + 1000` chars so truncation is detectable; failure → toast `Couldn't open <name>`. | reader.ts:95-97, 464-476 |
| `--read-text <text>` / `--read-text=<text>` | Read literal text, source `cli`. | reader.ts:98-100 |

Boolean env flags are true only for the exact strings `1` or `true` (case-sensitive) (log.ts:20-23).

Argument parsing (reader.ts:74-104): only arguments starting with `--` are considered; unknown switches and
positional arguments are ignored. `--name=value` splits at the first `=`; otherwise a value-taking switch consumes the
**next argument whatever it is** (even another `--switch`); a missing or empty value → no value. If several read
actions are present only one runs, in priority `--read-text` > `--read-clipboard` > `--read-file` (reader.ts:455-478).

Single instance: a second launch forwards its `argv` and `cwd` to the running instance and exits; if the forwarded args
contain no read action and not `--hidden`, the running instance shows its main window. Relative `--read-file` paths are
resolved against the *second* instance's cwd. Forwarded args are handled only after the first instance finished
bootstrapping. index.ts:538-560, 332-336.

App user model id: `APP_ID = 'com.vox.reader'` (toast notifications attribution). index.ts:544.

### 1.9 Media keys

Media keys are **not** registered as global hotkeys (grabbing them breaks other players). The widget's media session
handles them. With `triggers.mediaKeys = false`, Chromium's own handling (`HardwareMediaKeyHandling`,
`MediaSessionService`) is disabled at **startup** only — read synchronously from `settings.json` before anything else —
so changing the setting shows the toast `Restart Vox to finish turning media keys on|off`. shortcuts.ts:1-3,
index.ts:515-530, 324-328. *Native:* register/unregister the SMTC session live; the restart caveat can disappear.

### 1.10 Local bridge protocol (SPEC.md:433-442, local-server.ts)

Security model (local-server.ts:3-9): web pages must never be able to drive the bridge. The custom header forces a
CORS preflight in browsers, every preflight is refused, web `Origin`s are refused, and the `Host` check defeats DNS
rebinding. The extension is exempt from CORS through its host permissions and sends a `chrome-extension://` Origin.

- Binds `127.0.0.1:<settings.server.port>` only (default **47823**). Never `0.0.0.0` or `::`.
- Request pipeline, in this order, for **every** request (local-server.ts:188-227):
  1. Method `OPTIONS` → **403** `Forbidden` (no CORS headers).
  2. `Host` header (trimmed, lower-cased) must equal `127.0.0.1:<boundPort>` or `localhost:<boundPort>` → else
     **403** `Forbidden host`. Applies to `/health` too.
  3. `Origin` header (trimmed, lower-cased): absent/empty → OK; the literal `null` or anything starting with `http:`,
     `https:` or `file:` → **403** `Forbidden origin`. Applies to `/health` too.
  4. Path = URL pathname with trailing slashes removed (`/health/` ≙ `/health`); unparsable → 400 `Bad URL`.
  5. `/health`: `GET` or `HEAD` only (else **405** `Method not allowed`, header `Allow: GET`) → **200**
     `{ "ok": true, "app": "Vox", "version": "<x.y.z>", "playing": <status === 'playing'> }` (HEAD: same headers, no
     body). No client header needed. (SPEC.md omits `playing`; the code sends it.)
  6. Any other path than `/read`, `/command` → **404** `Not found`; not `POST` → **405** (`Allow: POST`).
  7. Header `x-vox-client` must equal `browser-extension` exactly → else **403** `Forbidden`.
  8. Body: `Content-Type` must match `^application/json\s*(;|$)` (case-insensitive) → else **415**
     `Content-Type must be application/json`. Declared `Content-Length` or received bytes > **2 MiB** (2 097 152) →
     **413** `Request body too large` with `code: "too-long"` and `Connection: close`. Body not received within **15 s**
     → **408** `Request timed out` (`Connection: close`). Stream error → 400 `Request stream error`; client abort →
     400 `Request aborted`. UTF-8, BOM stripped; invalid JSON → 400 `Invalid JSON`; not a JSON object → 400
     `Expected a JSON object`.
- `POST /read` `{ text, url?, title? }` → **200** with the `ReadResult` (even when `ok:false`). `text` not a string →
  400 `"text" must be a string` (`code: "empty-text"`); blank → 400 `Nothing to read — the text is empty`
  (`empty-text`). `url` kept if a string ≤ 2048 chars matching `^(https?|file)://` (case-insensitive) — core then keeps
  it only if it is an http(s) URL ≤ 4096 chars; `title` trimmed, ≤ 300 chars, dropped if blank. The read must answer
  within 15 s → else **504** `Timed out waiting for the app` (`code: "unknown"`). Core side (index.ts:213-245): before
  the reader exists → `{ ok:false, code:'unknown', error:'Vox is still starting' }`; source `browser-extension`;
  `sourceApp` = the foreground app's friendly name and `sourceProcess` its exe only if the foreground process is a known
  browser (`chrome.exe msedge.exe brave.exe opera.exe vivaldi.exe arc.exe chromium.exe yandex.exe firefox.exe`), else
  `sourceApp = "Browser"` and no process; `sourceTitle` = title.
- `POST /command` `PlayerCommand` → **200** `{ "ok": true }`. Validation is **stricter than IPC**
  (local-server.ts:320-352): an invalid command is a **400**, not a silent drop — `"<field>" must be a finite number`,
  `"direction" must be 1 or -1`, `"unit" must be sentence|seconds`, `Unknown command type`; an `amount` that is
  present but not a finite number is a 400 (IPC just omits it). Clamps are the same as IPC (§5.3). Core then re-parses
  it with the IPC parser and forwards it like any other command.
- Every JSON response: `Content-Type: application/json; charset=utf-8`, `Content-Length`, `Cache-Control: no-store`,
  `X-Content-Type-Options: nosniff`. Errors are `{ "ok": false, "error": "<message>", "code"?: ReadErrorCode }`;
  unexpected exceptions → **500** `{ ok:false, error:<message>, code:'unknown' }`.
- Server limits: keep-alive 5 s, max 64 connections, headers timeout 10 s, request timeout 16 s, idle socket destroyed
  after 20 s; malformed HTTP → raw `400 Bad Request` (`431` for header overflow) then close.
- Listen failures (logged only): `Port <N> is already in use by another program` (EADDRINUSE ≙ `WSAEADDRINUSE`),
  `Port <N> is reserved or blocked by Windows — choose another port` (EACCES ≙ `WSAEACCES`, excluded port ranges),
  `The loopback address 127.0.0.1 is not available` (EADDRNOTAVAIL). A non-integer or out-of-range port →
  `Invalid port <p> (use 1024–65535)`.
- Bridge start failure → toast `Browser bridge couldn't use port <N>. Pick another port in Settings`
  (action → `settings/integrations`), suppressed in demo. Start/stop/restart calls are serialized (both in core and
  inside the server). Restart happens only when `triggers.browserExtension` or `server.port` changed; same port while
  running → nothing.
- Server state for `AppInfo`: `running` = currently listening; `port` = bound port while running, else the last
  requested port.

### 1.11 Old design language (informative; the user is redesigning)

SPEC.md:444-473. Cards black (`#0b0b0b` on paper canvas, `#111` on night), radius 26 px (small 20 px), 1 px hairline
`rgba(255,255,255,.07)`, padding 22–28 px. Themes: `paper` = `#ececec` canvas + black cards; `night` = `#000` canvas +
`#111` cards; `system` follows OS dark mode. Title-bar overlay colors: paper bg `#ececec` / symbols `#111111`, night bg
`#000000` / symbols `#ffffff` (windows.ts:45-48). Fonts: Doto (dot matrix; only ≥ 16 px, weight 800+ below 24 px),
Space Grotesk (card titles, 500), Inter (UI; labels 11 px/600/uppercase/tracking .09em), Cascadia Mono for keys/paths.
Dots: filled `#fff`, empty `#3a3a3a`, buffered `#6b6b6b`, larger white playhead dot. One accent red `#D71921`
(`ACCENT_RED`) used sparingly (live playback, today/now marker, errors, low credits, the N of the compass); never for
large fills, neutral badges or idle buttons (destructive buttons turn red only on hover and in their confirm dialog); no
second accent color (the extension's "connected" dot is white). Canvas text/sidebar use canvas tokens; content inside
cards always uses the dark card tokens (white / grays). Doto's own `%`, `:` and `.` were replaced by drawn dots; below
16 px Doto switches to Inter. Controls: pill toggles that invert (white track + black knob when on), vertical dot
sliders, segmented controls as black pills with a white active segment. Icons: below 20 px thin line icons (stroke
1.5); dot-matrix glyphs (5×5 / 7×7 grids) only at ≥ 20 px and for the logo (its 14 px variant is the bold 5×5 tray
"V"); solid transport icons everywhere. Charts are hand-drawn dot matrices (no chart library). Motion 160–240 ms
ease-out, dot stagger 12–20 ms, respect reduced motion. Focus ring 2 px (white / red-tinted) offset 2 px; min hit
target 32×32. Grayscale text antialiasing (ClearType fringes break dot glyphs) — index.ts:517-518, SPEC.md:444-473.

### 1.12 Core module contracts (SPEC.md:282-431, as implemented)

SPEC.md fixes the internal seams of the core. They are the natural Rust module boundaries; the "+" items are
extensions the implementation added beyond SPEC.md.

```text
settingsStore   init() · get() -> Settings (with derived hasApiKey) · update(patch) -> Settings · onChange(cb(next, prev))
                · getApiKey() -> key|null · setApiKey(key|null) · flush() · +flushSync() · +readSettingsFileSync() -> Settings|null
historyStore    init() · add(rec) · update(id, patch) -> rec|null · get(id) · list(HistoryQuery) -> HistoryPage
                · all() -> copy, newest first · remove(id) · clear() · toggleFavorite(id) · exportTo(path)
                · applyRetention(days, now?) -> removedCount · onChange(cb) (debounced) · +onRemoved(cb(ids)) (synchronous)
                · flush() · +flushSync()
audioCache      init() · save(readId, CachedChunk[]) · load(readId) -> CachedChunk[]|null · has(readId) · remove(readId)
                · clear() -> CacheStats · stats() -> CacheStats · prune(maxBytes, protect[]) -> evictedIds
                · +hasText(readId) · +redactText(readId) · +removeOrphans(keepIds) -> removedIds
analytics       computeAnalytics(records, range, now = now) -> AnalyticsSummary   (pure)
demo-data       generateDemoRecords(now, count?) -> ReadRecord[]
paths           userDataFile(...parts) -> <userData>\...   (resolved on every call: demo mode switches userData first)
ElevenLabsClient(getApiKey) listVoices({refresh?}) · listModels({refresh?}) · getAccount({refresh?}) · synthesize(req, cancel)
                · +clearCache();  validateApiKey(key)
TtsEngine({client, sink, mock}) start(StartSessionOptions) -> sessionId · playCached(opts + chunks) -> sessionId
                · progress(sessionId, positionSec) · stop() · currentSessionId · currentReadId
                events: complete(readId, CachedChunk[], charactersBilled)  — every chunk synthesized
                        failed(readId, ElevenLabsError)                     — before any audio
                        stopped(readId, charactersBilled, error?)           — stopped/superseded mid-synthesis, or ended
                                                                              early after a later chunk failed (error)
TtsSink         sessionStart(TtsSessionStart) · chunk(TtsChunk) · done(TtsSessionDone) · error(TtsSessionError)
                (the default sink forwards each to the widget as tts:* events)
SelectionService start(settings) · applySettings(settings) · stop() · hookAvailable · +whenReady() · readSelection(source)
                · captureSelection({viaContextMenu}) · setChipBounds(rect|null) · chipActivated() · chipDismissed() · +flush()
                events: read({text, source, foreground}) · no-selection(source) · chip-show({x, y, mode}) (cursor, DIP)
                        · chip-hide() · +notice({kind, message}) · +hook-status(available)
LocalServer(handlers{onRead, onCommand, getStatus}) start(port) -> {ok, port, error?} · stop() · running · port
explorer-menu   setExplorerMenu(enabled, {command, label, iconPath}) -> {ok, error?} · isExplorerMenuInstalled()
                · readTextFile(path, maxChars) (UTF-8/UTF-16 BOM aware; .html → tags stripped; .rtf → control words stripped)
autostart       applyLaunchAtLogin(enabled, startHidden)
```

Records passed between them:

```text
ForegroundInfo        { processName: lower-case exe ('' if unknown), appName: friendly name, title: trimmed (≤ 300), pid }
SelectionReadRequest  { text, source: ReadSource, foreground: ForegroundInfo | null }
BridgeReadRequest     { text, url?, title? }
ReadRequest (reader)  { text, source, sourceApp?, sourceProcess?, sourceTitle?, sourceUrl?, voice?: { id, name } }
CachedChunk           { index, textStart, textEnd, mime, alignment: Alignment|null, durationSec, audio: bytes }
StartSessionOptions   { readId, text, title, voiceId, voiceName, sourceApp?, settings: Settings }
TextChunk             { text, start, end }   // text == input[start..end)
SynthesisRequest      { text, voiceId, modelId, outputFormat, voiceSettings, previousText?, nextText?, languageCode? }
SynthesisResult       { audio: bytes, mime, alignment|null, requestId?, characterCost? }
ElevenLabsError       { code: ReadErrorCode, message (user-facing), status?, apiCode?, apiMessage?, param?, requestId?,
                        retryable, missingPermission, aborted, retryAfterSec? }
```

---

## 2. Constants

### 2.1 `src/shared/constants.ts` (whole file)

| Name | Value | Notes |
|---|---|---|
| `APP_NAME` | `'Vox'` | Single source of the product name. :2 |
| `APP_ID` | `'com.vox.reader'` | App User Model ID / installer appId. :3 |
| `READ_WITH_LABEL` | `` `Read with ${APP_NAME}` `` = `'Read with Vox'` | Context chip label, Explorer verb label. :4 |
| `ELEVENLABS_BASE_URL` | `'https://api.elevenlabs.io'` | :6 |
| `ELEVENLABS_DEFAULT_VOICE_ID` | `'JBFqnCBsd6RMkjVDRZzb'` | "George", a premade voice. :7 |
| `ELEVENLABS_DEFAULT_VOICE_NAME` | `'George'` | :8 |
| `ELEVENLABS_DEFAULT_MODEL_ID` | `'eleven_flash_v2_5'` | :9 |
| `LOCAL_SERVER_DEFAULT_PORT` | `47823` | Browser bridge. :12 |
| `LOCAL_SERVER_CLIENT_HEADER` | `'x-vox-client'` | :13 |
| `LOCAL_SERVER_CLIENT_VALUE` | `'browser-extension'` | :14 |
| `ESTIMATED_CHARS_PER_SECOND` | `15` | Duration estimate before audio exists. :17 |
| `ACCENT_RED` | `'#D71921'` | :20 |
| `RESERVED_ACCELERATORS` | `Ctrl+C, Ctrl+Insert, Ctrl+V, Ctrl+X, Ctrl+Z, Ctrl+Y, Ctrl+A, Ctrl+S, Alt+F4, Alt+Tab, Ctrl+Escape, Ctrl+Shift+Escape` | Chords a hotkey must never take: registering one hijacks it in every app, and Ctrl+C / Ctrl+Insert are also the copy chords sent to capture a selection (a hotkey on them swallows the copy). Compared **canonically** (§2.3). Settings refuses them; core skips them if a hand-edited settings file has one. :22-41 |
| `DAILY_GOAL_SEC` | `15 * 60` = 900 | Daily listening goal on Home. :44 |

### 2.2 `src/shared/defaults.ts`

- `DEFAULT_SETTINGS` — see §4 (defaults.ts:9-81).
- `PLAYBACK_RATES = [0.75, 1, 1.25, 1.5, 1.75, 2]` — client-side rates offered in the UI (defaults.ts:84). The UI also
  lists the current rate if it is finite and not within 0.001 of a preset (e.g. set elsewhere), sorted ascending,
  labelled `` `${Number(r.toFixed(2))}×` `` (e.g. `1.25×`, `1×`), accessible name `<r> times speed`
  (settings/logic.ts:146-155). Note the presets (0.75–2) are narrower than the allowed range (0.5–3).

### 2.3 Accelerator canonical form (used for reserved/duplicate checks)

shortcuts.ts:46-95 (core) and settings/logic.ts:85-135 (UI) implement the same comparison key:

1. Split on `+`, handling a literal `+` key (`"Ctrl++"` → `["ctrl", "+"]`; a `+` at a part start is the key itself and
   a following `+` is skipped).
2. Lower-case and trim each part, fold aliases:
   `control, cmdorctrl, commandorcontrol → ctrl`; `option, altgr → alt`; `cmd, command, meta, win → super`;
   `esc → escape`; `return → enter`; `plus → +`.
3. Modifiers `ctrl, alt, shift, super` de-duplicated and sorted in that order, then the non-modifier parts in original
   order, joined with `+`. Example: `"Shift+Control+s"` → `"ctrl+shift+s"`.

Accelerator strings use Electron syntax (`"Ctrl+Alt+R"`); empty string = disabled. Before splitting, the whole string
is trimmed; each part is trimmed and lower-cased; non-modifier parts are **not** de-duplicated. The UI's comparison key
(settings/logic.ts:102-111, design/hotkey.ts:248-268) is the same except that it does not trim individual parts
(`"Ctrl + R"` differs); a Rust port should share one function.

Character-key → virtual-key for the AltGr check (US layout, as Electron maps them), shortcuts.ts:97-118:

| Key part | VK |
|---|---|
| `a`–`z` | uppercase ASCII code (`0x41`–`0x5A`) |
| `0`–`9` | ASCII code (`0x30`–`0x39`) |
| `;` | `0xBA` |
| `=` and `+` | `0xBB` |
| `,` | `0xBC` |
| `-` | `0xBD` |
| `.` | `0xBE` |
| `/` | `0xBF` |
| `` ` `` | `0xC0` |
| `[` | `0xDB` |
| `\` | `0xDC` |
| `]` | `0xDD` |
| `'` | `0xDE` |

Only accelerators with both `ctrl` and `alt`, no `super`, and exactly one non-modifier part that maps to a VK are checked;
`shift` is passed along. The lookup returns the character, `''` for a dead key, or null (no conflict); a lookup
exception counts as no conflict (shortcuts.ts:124-137).

Registration (shortcuts.ts:143-185): always unregister ours first; `hotkeysEnabled = false` → stop (no failures). For
actions in the fixed order `readSelection, readClipboard, playPause, stop`: trim; empty → skip; canonical key reserved →
failure `reserved` (not added to "seen"); already seen → `duplicate`; add to seen; AltGr conflict →
`types-character` (still counts as seen, so a later identical chord reports `duplicate`); OS registration refused or
invalid syntax → `in-use`. Registration uses the trimmed raw string. Re-registration happens at startup (after waiting
≤ 1.5 s for the keyboard-layout FFI), when `hotkeysEnabled` or any accelerator string changed, and when a suspension
ends — and **each of those re-reports failures as a toast**. Not in screenshot mode, not while suspended.

Hotkey failure toast (one line, most specific first; shortcuts.ts:198-214):

- one types-character: `` `${accel} types “${ch}” on your keyboard, so it isn't used as a shortcut` `` (or
  `types a character` when the char is empty/dead key); several: `` `${list} type characters on your keyboard, so they aren't used as shortcuts` ``
- reserved: `` `${list} is|are reserved for copying, pasting or Windows` ``
- in-use or duplicate: `` `${list} is|are already in use` ``
- joined with `. `, then `. Pick another shortcut in Settings` (or `other shortcuts` when > 1 failure).
- Toast kind `error`, action `Open settings` → `{ page: 'settings', section: 'triggers' }`.

Hotkey suspension while the Settings recorder is recording: `SHORTCUT_SUSPEND_MAX_MS = 30 000` (index.ts:132-152).
Every `suspend(true)` restarts the 30 s safety timer; unregisters only on the first call. `suspend(false)` clears the
timer and, if suspended, re-registers from the **current** settings.

### 2.4 Other cross-cutting constants referenced by the contracts

| Constant | Value | Where |
|---|---|---|
| `MAX_READ_TEXT` | 2 000 000 chars (IPC read-text cut) | main/ipc.ts:100 |
| `READ_SOURCES` | `hotkey, context-chip, selection-popup, browser-extension, explorer, clipboard, manual, replay, cli` (validation list, this order) | main/ipc.ts:73-83, history-store.ts:12-22 |
| `PREVIEW_CHARS` | 160 (text kept when `storeText` off) | reader.ts:40 |
| `HISTORY_WRITE_INTERVAL_MS` | 2000 | reader.ts:41 |
| `TITLE_CHARS` | 64 (player title / tray tooltip preview) | reader.ts:42 |
| `RAW_CUT_FACTOR`, `RAW_CUT_MIN` | 4, 200 000 | reader.ts:47-48 |
| `PARTIAL_READ_ERROR` | `'Only part of the text could be synthesized'` | reader.ts:50 |
| `MAX_TRACKED_SESSIONS` | 16 | reader.ts:252 |
| `CACHE_MB` | 1 000 000 bytes (decimal MB) | reader.ts:169 |
| `WIDGET_MARGIN` | 24 DIP | windows.ts:36 |
| `TITLEBAR_HEIGHT` | 44 DIP | windows.ts:37 |
| `DEFAULT_WIDGET_SIZE` | 380 × 168 DIP (until the widget reports its size) | windows.ts:38 |
| `DEFAULT_CHIP_SIZES` | context 153 × 38, selection 82 × 38 DIP | windows.ts:40-43 |
| `THEME_COLORS` | paper `{ bg '#ececec', symbol '#111111' }`, night `{ bg '#000000', symbol '#ffffff' }` | windows.ts:45-48 |
| Widget size clamp | width 40..1200, height 24..900 DIP, rounded | windows.ts:697 |
| Chip size clamp | width 32..480, height 24..160 DIP, rounded | windows.ts:811 |
| Hit-rect minimum | 8 × 8 DIP (smaller → whole window) | windows.ts:248 |
| Own-move grace | 500 ms (user-drag notifications ignored after core moved/resized the widget) | windows.ts:628 |
| `HOVER_POLL_MS` | 100 | windows.ts:456 |
| `CHIP_EXIT_MS` | 140 (keep in sync with the chip's 120 ms exit animation + a frame) | windows.ts:768 |
| Chip placement | `CONTEXT_MENU_SIZE` small 220×240 / guess 320×360 / large 360×480; `CHIP_GAP` 18, `CHIP_GAP_V` 20, `SELECTION_CHIP_GAP` 20 (algorithm in `shell.md` §6.5.3) | windows.ts:130-139 |
| Widget-ready fallback | 1500 ms after page load; `whenWidgetReady` timeout 5000 ms | windows.ts:635-653 |
| `CRASH_WINDOW_MS`, `MAX_CRASH_RELOADS` | 60 000, 3 | windows.ts:396-397 |
| `SHUTDOWN_TIMEOUT_MS`, `FORCE_EXIT_MS` | 3000, 4000 | index.ts:36-37 |
| FFI wait before first hotkey registration | 1500 ms | index.ts:489 |
| `app:info` hook wait | 2000 ms | main/ipc.ts:275-278 |
| `RETENTION_INTERVAL_MS` | 6 h (the periodic run is skipped while a retention change is pending) | index.ts:30, 438-441 |
| `RETENTION_CHANGE_DELAY_MS` | 60 000 — a changed "Keep history for" applies 60 s after the last change (arrowing through a native select commits every option it passes, e.g. "7 days") | index.ts:31-35 |
| `DAY_MS` (retention) | 86 400 000 (retention cutoff = `now − days × DAY_MS`, not calendar days) | history-store.ts:24, 331-335 |
| history change debounce | `NOTIFY_DEBOUNCE_MS` 100 (max wait `NOTIFY_MAX_WAIT_MS` 400) in the store, then 150 ms throttle before `history:changed` | history-store.ts:25-26, main/ipc.ts:391-398 |
| JSON store retry delays | 15, 40, 90, 180, 350 ms; `MAX_WRITE_RETRIES` 5 (delay `1 s × failures`) | json-file.ts:20-21, 153 |
| Debounce / max wait | settings 250 ms / 2000 ms; history 2500 ms / 10 000 ms | settings-store.ts:278-282, history-store.ts:215-221 |
| `LISTEN_LOG_MAX_KEYS` | 400 | listen-log.ts:5 |
| `SILENT_READING_WPM` | 238 | analytics.ts:8 |
| analytics `TOP_N` | 6; `MAX_DAILY_DAYS` 365 | analytics.ts:7-9 |
| ElevenLabs timeouts | synth `max(30 000, chars × 50)` ms (`SYNTH_TIMEOUT_MS`, `SYNTH_MS_PER_CHAR`); lists and key validation 15 000 ms (`LIST_TIMEOUT_MS`) | elevenlabs.ts:20-24 |
| ElevenLabs caches | voices 10 min, models 60 min, account 60 s, keyed by SHA-256(key) hex[0..16] (`'anon'` without a key) | elevenlabs.ts:25-27, 516-518 |
| `MAX_VOICE_PAGES` | 25 (× `page_size` 100) | elevenlabs.ts:28 |
| `NO_CONTEXT_MODELS` | `/^eleven_v3/` (no previous/next text sent) | elevenlabs.ts:30 |
| Bridge limits | body 2 MiB (2 097 152 bytes), timeout 15 000 ms, url ≤ 2048, title ≤ 300 | local-server.ts:48-51 |
| `TOOLTIP_MAX` (tray) | 127 UTF-16 units | tray.ts:20-21 |
| Log | rotate at 1 MiB, argument cut 2000 chars | log.ts:47-48 |
| Capture timing | modifier wait 700, settle 40, context-menu settle 120, poll 15 / 450, text settle 1500, Escape settle 80, restore 150, late-copy grace 1500 (poll 25), chip activation de-dupe 300 (ms) | selection/service.ts:85-100 |
| Gestures | drag threshold 6 px, hint TTL 60 000 ms, selection chip 4000 ms | selection/gestures.ts:90-94 |

---

## 3. Data model (every shared type)

All JSON field names are exactly as shown (camelCase). Optional (`?`) fields are **omitted** from JSON when absent
(never `null`), except where a type says `| null`.

Conventions inherited from JavaScript that every Rust validator/serializer must reproduce deliberately:

- **String lengths and cuts are UTF-16 code units** (`.length`, `.slice(0, n)`): every "≤ N chars" cap in this file
  (ids 128, titles 300/200, voiceName 120, hotkeys 64, search 500, preview 160, read text 2 000 000, URL 4096 …). JS
  slicing can split a surrogate pair; only the reader's pre-cut and `maxChars` truncation guard against it.
- **Rounding**: `Math.round(x)` = `floor(x + 0.5)` (half toward +∞: `Math.round(-2.5) = -2`), unlike Rust
  `f64::round` (half away from zero). It matters for negative widget coordinates (`customX/customY`), window geometry
  and `round1`. Use a `js_round` helper.
- **Number output**: `JSON.stringify` writes integral doubles without a fraction (`1`, not `1.0`) and uses the
  shortest round-trip form otherwise. Reading either form is fine; byte-identical output needs a JS-style formatter.
- **Object key order** = insertion order. Where a type's serialized order differs from its table order, it is stated
  ("Serialized order: …"). Maps (`Voice.labels`, `ReadRecord.listenLog`) keep insertion order; `listenLog` is always
  written sorted ascending. No consumer depends on order; it only matters for byte-identical files.
- **Regex classes**: JS `\d` and `\w` are ASCII (`[0-9]`, `[A-Za-z0-9_]`); Rust's `regex` makes them Unicode by
  default — use explicit ASCII classes. JS `\s` is Unicode whitespace **plus U+FEFF**.
- `typeof x === 'number' && isFinite(x)` checks reject strings, `NaN`, `±Infinity`; JSON has no NaN/Infinity, so
  "finite" means "is a JSON number". Booleans must be real JSON booleans (no `0/1`, no `"true"`).

### 3.1 Enumerations

```text
ThemeMode     = 'system' | 'paper' | 'night'                         (types.ts:6)
ResolvedTheme = 'paper' | 'night'                                    (types.ts:7)
WidgetPosition= 'bottom-right' | 'top-right' | 'bottom-center' | 'near-cursor' | 'remember'   (types.ts:9)
OutputFormat  = 'mp3_44100_128' | 'mp3_44100_64' | 'mp3_22050_32'    (types.ts:11)
ReadSource    = 'hotkey' | 'context-chip' | 'selection-popup' | 'browser-extension' | 'explorer'
              | 'clipboard' | 'manual' | 'replay' | 'cli'            (types.ts:14-23)
ReadErrorCode = 'no-api-key' | 'unauthorized' | 'quota' | 'rate-limit' | 'network' | 'empty-text'
              | 'no-selection' | 'too-long' | 'not-cached' | 'unknown'   (types.ts:171-181)
PlayerStatus  = 'idle' | 'loading' | 'playing' | 'paused' | 'ended' | 'error'   (types.ts:241)
AnalyticsRange= '7d' | '30d' | '90d' | '365d' | 'all'                 (types.ts:346)
AppPage       = 'home' | 'history' | 'analytics' | 'voices' | 'settings' | 'onboarding'   (types.ts:384)
```

`ReadSource` meanings (types.ts:14-23): `hotkey` global read-selection shortcut; `context-chip` chip beside the native
right-click menu; `selection-popup` PopClip-style chip; `browser-extension` real browser context-menu entry;
`explorer` Explorer right-click on a text file (also `--read-file`); `clipboard` read-clipboard shortcut / tray /
`--read-clipboard`; `manual` typed or pasted in the app (default for `reader:read-text`); `replay` re-synthesized
from history; `cli` `--read-text`.

`OutputFormat` UI labels (settings/logic.ts:254-258): `mp3_44100_128` "High · 128 kbps", `mp3_44100_64`
"Standard · 64 kbps", `mp3_22050_32` "Data saver · 32 kbps". MIME for all three: `audio/mpeg` (mock: `audio/wav`).

`ReadErrorCode` producers:

| Code | Produced when | Default user message (reader.ts:143-158 `ERROR_MESSAGES`, used only if the error has no message) |
|---|---|---|
| `no-api-key` | read without key (and not mock); ElevenLabs call without key | — (toast `Add your ElevenLabs API key to start reading`) |
| `unauthorized` | HTTP 401 / `authentication_error`, or missing permission (scope) | `ElevenLabs rejected your API key` |
| `quota` | HTTP 402, `quota_exceeded`, `insufficient_credits`, `payment_required` | `Your ElevenLabs character quota is used up` |
| `rate-limit` | HTTP 429, `rate_limit_error`, concurrency codes, `system_busy` | `ElevenLabs is busy right now. Try again in a moment` |
| `network` | transport failure or timeout | `Couldn't reach ElevenLabs. Check your connection` |
| `empty-text` | nothing left after cleaning; empty clipboard; bridge `/read` with missing/blank text (HTTP 400); engine got zero chunks (fatal `tts:session-error` `There is no text to read`, engine.ts:238-241) | — |
| `no-selection` | declared; selection failures surface as an internal `no-selection` event + toast `Select some text first`, not as a `ReadResult` | — |
| `too-long` | `text_too_long` / `max_character_limit_exceeded`; bridge body > 2 MiB (HTTP 413) | `That selection is too long for the current model` |
| `not-cached` | replay of a preview-only record whose audio is gone (ReadResult); a cached session started with zero chunks (fatal `tts:session-error`, message `The saved audio for this read is missing` — engine.ts:316-322) | — |
| `unknown` | everything else (voice/model unavailable, 5xx, bad response, engine start failure, cancellation, an internal engine crash → fatal `Something went wrong while preparing the audio`, engine.ts:382-387) | `Something went wrong while reading` |

`errorMessage(code, fallback)` (reader.ts:156-158) = the error's own message trimmed if non-empty, else
`ERROR_MESSAGES[code]`, else `Something went wrong while reading`. Every ElevenLabs error already carries a precise
user-facing message, so the generic lines above appear only for errors without one.

HTTP error → code precedence (elevenlabs.ts:170-222; first match wins; "ids" = the error body's `detail.status` and
`detail.code`, lower-cased): (1) ids ∋ `quota_exceeded|insufficient_credits|payment_required`, or HTTP 402, or
`detail.type = payment_required` → `quota`; (2) ids ∋ `missing_permissions|insufficient_permissions`, or message
matches `/missing (the )?permission/i` → `unauthorized` + permission message + `missingPermission`; (3) HTTP 401 or
`type = authentication_error` → `unauthorized` (badKey message when ids ∋ an invalid-key code or no API message, else
`ElevenLabs: <message>`); (4) HTTP 429, `type = rate_limit_error`, or ids ∋
`too_many_concurrent_requests|concurrent_limit_exceeded|rate_limit_exceeded|system_busy` → `rate-limit` (concurrency
message for the first two ids), retryable; (5) `voice_not_found` → `unknown`/voiceMissing; (6) `voice_access_denied` →
voiceDenied; (7) `model_not_found|model_access_denied|unsupported_model` → model; (8)
`text_too_long|max_character_limit_exceeded` → `too-long`; (9) HTTP ≥ 500 → `unknown`/server, retryable; (10) else
`unknown` with `ElevenLabs: <message>` or `ElevenLabs returned an error (HTTP <status>)`, cut to 300 chars. Transport:
caller cancel → `unknown`/`Cancelled` (aborted, never shown); timeout → `network`/timeout; other → `network`/network.
Unparsable success body or no audio → `unknown`, retryable.

User-facing ElevenLabs messages (`MESSAGES`, elevenlabs.ts:32-48) that travel through IPC as `ReadResult.error`,
`TtsSessionError.message`, `ApiKeyResult.error/warning`, toasts, or rejected-call messages:

| Key | Text |
|---|---|
| noKey | `Add your ElevenLabs API key in Settings to start reading` |
| badKey | `Your ElevenLabs key was rejected — update it in Settings` |
| quota | `You're out of ElevenLabs credits — top up your plan or wait for the monthly reset` |
| rateLimit | `ElevenLabs is busy right now — try again in a moment` |
| concurrency | `Too many requests at once for your ElevenLabs plan — try again in a moment` |
| network | `Couldn't reach ElevenLabs — check your internet connection` |
| timeout | `ElevenLabs took too long to respond — try again` |
| server | `ElevenLabs is having trouble right now — try again in a moment` |
| voiceMissing | `That voice is no longer available — pick another one in Voices` |
| voiceDenied | `That voice isn't available on your ElevenLabs plan — pick another one in Voices` |
| model | `The selected model isn't available for your key — choose another in Settings` |
| tooLong | `This passage is too long for the selected model` |
| badResponse | `ElevenLabs sent a response that could not be read` |
| noAudio | `ElevenLabs returned no audio` |
| cancelled | `Cancelled` |
| (permission) | `` `Your ElevenLabs key isn't allowed to use ${what} — enable it for the key on elevenlabs.io, or use another key` `` where `what` ∈ `Text to Speech`, `Voices (read)`, `User (read)`, `Models (read)`, the scope with `_`→space, or `this feature` |
| (401 with own message) | `` `ElevenLabs: ${apiMessage}` `` |
| (other HTTP) | `` `ElevenLabs: ${apiMessage}` `` or `` `ElevenLabs returned an error (HTTP ${status})` `` (≤ 300 chars) |

### 3.2 `Settings` and sub-structures (types.ts:25-123)

```jsonc
// Settings (types.ts:41-123). Full defaults in §4.
{
  "version": 1,                 // literal 1
  "hasApiKey": false,           // DERIVED: true when an API key is stored. Read-only for UI; never persisted.
  "onboarded": false,           // onboarding finished or skipped
  "voiceId": "JBFqnCBsd6RMkjVDRZzb",
  "voiceName": "George",
  "modelId": "eleven_flash_v2_5",
  "outputFormat": "mp3_44100_128",
  "voice": {                    // VoiceSettings (types.ts:25-31) — ElevenLabs server-side voice settings
    "stability": 0.5,           // 0..1
    "similarityBoost": 0.75,    // 0..1
    "style": 0,                 // 0..1
    "useSpeakerBoost": true,
    "speed": 1                  // ElevenLabs server-side speed 0.7..1.2
  },
  "languageCode": "",           // ISO 639-1 passed to ElevenLabs; '' = auto-detect
  "playbackRate": 1,            // client-side rate 0.5..3 (pitch preserved)
  "volume": 1,                  // 0..1
  "triggers": {
    "rightClickChip": true,     // chip beside the native context menu
    "selectionPopup": false,    // PopClip-style chip after selecting
    "hotkeysEnabled": true,
    "hotkeys": {                // HotkeySettings (types.ts:33-39): accelerator strings, '' = disabled
      "readSelection": "Ctrl+Alt+R",
      "readClipboard": "Ctrl+Alt+V",
      "playPause": "Ctrl+Alt+P",
      "stop": "Ctrl+Alt+S"
    },
    "mediaKeys": true,          // hardware media keys / Windows media overlay control playback
    "browserExtension": true,   // run the localhost bridge
    "explorerMenu": false,      // Explorer "Read with Vox" for text files
    "excludedApps": [],         // lower-case exe names where chips never appear, e.g. "photoshop.exe"
    "restoreClipboard": true    // restore the user's clipboard after capturing a selection
  },
  "widget": {
    "enabled": true,            // show the floating player while reading (audio plays regardless)
    "position": "bottom-right",
    "customX": null,            // last dragged top-left (DIP) used when position === 'remember'
    "customY": null,
    "autoHide": true,           // hide a few seconds after reading finishes
    "highlightWords": true,     // highlight the word being spoken
    "compact": false            // compact pill instead of the full card
  },
  "app": { "launchAtLogin": false, "startMinimized": false, "closeToTray": true, "theme": "system" },
  "library": {
    "saveHistory": true,
    "storeText": true,          // store full text (else a 160-char preview)
    "cacheAudio": true,         // keep audio for free replays
    "cacheLimitMB": 500,        // decimal MB
    "retentionDays": 0          // delete history older than N days; 0 = forever
  },
  "text": { "skipUrls": true, "skipEmails": false, "maxChars": 20000 },
  "server": { "port": 47823 }
}
```

`DeepPartial<T>` (types.ts:125-127): every key optional, recursively for objects; **arrays are not partial** (an array
value replaces the whole array). `SettingsPatch = DeepPartial<Omit<Settings, 'version' | 'hasApiKey'>>`
(types.ts:129). Core ignores `version`/`hasApiKey` even if sent (sanitize rebuilds them). A `null` (or any
non-object) where a sub-object is expected (`{"triggers": null}`) is merged in as-is and then sanitizes back to the
current values, i.e. it is a no-op; `null` for a leaf is only meaningful for `widget.customX/customY`.

The block above is in serialized order (sanitize rebuilds the object in exactly this order). `settings.json` is the
same object without `hasApiKey`, pretty-printed with 2-space indentation, e.g. (defaults):

```json
{
  "version": 1,
  "onboarded": false,
  "voiceId": "JBFqnCBsd6RMkjVDRZzb",
  "voiceName": "George",
  "modelId": "eleven_flash_v2_5",
  "outputFormat": "mp3_44100_128",
  "voice": { "stability": 0.5, "similarityBoost": 0.75, "style": 0, "useSpeakerBoost": true, "speed": 1 },
  "languageCode": "",
  "playbackRate": 1,
  "volume": 1,
  "triggers": { "rightClickChip": true, "selectionPopup": false, "hotkeysEnabled": true,
    "hotkeys": { "readSelection": "Ctrl+Alt+R", "readClipboard": "Ctrl+Alt+V", "playPause": "Ctrl+Alt+P", "stop": "Ctrl+Alt+S" },
    "mediaKeys": true, "browserExtension": true, "explorerMenu": false, "excludedApps": [], "restoreClipboard": true },
  "widget": { "enabled": true, "position": "bottom-right", "customX": null, "customY": null, "autoHide": true,
    "highlightWords": true, "compact": false },
  "app": { "launchAtLogin": false, "startMinimized": false, "closeToTray": true, "theme": "system" },
  "library": { "saveHistory": true, "storeText": true, "cacheAudio": true, "cacheLimitMB": 500, "retentionDays": 0 },
  "text": { "skipUrls": true, "skipEmails": false, "maxChars": 20000 },
  "server": { "port": 47823 }
}
```

(The real file puts every key on its own line; the grouping above is only for brevity.)

### 3.3 ElevenLabs types

**`Voice`** (types.ts:133-140)

| Field | Type | Meaning / mapping from ElevenLabs JSON (elevenlabs.ts:296-323) |
|---|---|---|
| `voiceId` | string | `voice_id` (required; voices without it are dropped) |
| `name` | string | `name` trimmed, fallback `voiceId`. **Premade** voices named `"George - Warm, Captivating Storyteller"` are split at the first `" - "` (only if it is not at index 0): name = `"George"` (trimmed), rest trimmed → `labels.tagline` (unless empty or a `tagline` label already exists). |
| `category?` | string | `category` if a non-empty string (not trimmed): `premade`, `cloned`, `generated`, `professional`, … |
| `description?` | string | `description` trimmed, omitted if empty |
| `previewUrl?` | string | `preview_url` if a non-empty string |
| `labels` | map string→string | `labels`: string values trimmed (empty dropped); numbers/booleans stringified (`String(v)`); other types dropped. Common keys: `accent`, `age`, `gender`, `use_case`, `descriptive`, `tagline`. Always present (`{}` when none). |

Serialized order: `voiceId, name, labels, category?, description?, previewUrl?` (elevenlabs.ts:316-321; the demo
lists use `voiceId, name, category, description, labels`). Order is irrelevant to consumers.

List order from core (elevenlabs.ts:325-336): de-duplicated by `voiceId` (first wins), then own voices
(`cloned`/`generated`/`professional`) rank 0, other non-premade rank 1, `premade` rank 2; within a rank by name,
locale compare, base sensitivity, numeric. (The Voices page re-sorts with its own ranking: cloned 0, professional 1,
generated 2, unknown 2.5, premade 3, then name — settings/logic.ts:348-358.)

**`TtsModel`** (types.ts:142-150; mapping elevenlabs.ts:343-359) — only models with `can_do_text_to_speech === true`,
in ElevenLabs' order:

| Field | Type | Mapping |
|---|---|---|
| `modelId` | string | `model_id` (required, non-empty string) |
| `name` | string | `name` if a non-empty string (not trimmed), fallback `modelId` |
| `description?` | string | `description` trimmed, omitted if empty |
| `languages` | `{ id, name }[]` | `languages[].language_id` / `.name` (name falls back to id; entries without id dropped; `[]` when missing) |
| `maxChars?` | number | first finite of `maximum_text_length_per_request`, `max_characters_request_subscribed_user` |
| `costFactor?` | number | `model_rates.character_cost_multiplier` if finite (credits per char relative to base, e.g. 0.5 for Flash) |

The response must be a JSON array (else `badResponse`); entries that are not objects or lack
`can_do_text_to_speech === true` are skipped.

**`AccountInfo`** (types.ts:152-158; mapping elevenlabs.ts:361-373, from `GET /v1/user/subscription`)

| Field | Type | Mapping |
|---|---|---|
| `tier` | string | `tier` (non-empty string), fallback `'unknown'` |
| `characterCount` | number | `character_count` if finite, fallback 0 (credits used this period) |
| `characterLimit` | number | `character_limit` if finite, fallback 0 |
| `nextResetUnix?` | number | `next_character_count_reset_unix` if finite (**seconds** since epoch) |
| `status?` | string | `status` if a non-empty string (e.g. `active`) |

A non-object body → `badResponse` (`ElevenLabs sent a response that could not be read`). The UI derives
`remaining = max(0, limit − used)`, `fractionLeft = limit > 0 ? remaining / limit : 0`, and
`resetInDays = nextResetUnix ? max(0, ceil((nextResetUnix × 1000 − now) / 86 400 000)) : null`
(settings/logic.ts:311-318).

**`ApiKeyResult`** (types.ts:160-167): `{ ok: boolean, error?: string, warning?: string, account?: AccountInfo | null,
settings: Settings }`. `warning` = key works but cannot read account info (restricted key) or is out of quota.
`settings` is always the current settings after the attempt (so the UI sees `hasApiKey`). Core always fills
`account` (`null` when unknown or on failure) except on the length-guard failure, where it is absent.

### 3.4 Reading / TTS stream types

**`ReadResult`** (types.ts:183-188): `{ ok: boolean, readId?: string, error?: string, code?: ReadErrorCode }`.
`ok: true` means the read **started** (synthesis errors arrive later as `tts:session-error` + toast).

**`Alignment`** (types.ts:190-195): `{ chars: string[], starts: number[], ends: number[] }` — character-level timing
for one chunk; times in **seconds relative to the chunk's audio start**; all three arrays have equal length.
ElevenLabs counts **code points**, not UTF-16 units; the widget maps alignment entries onto the chunk's text range
proportionally when counts differ. Cached alignments of preview-only records have every `chars` entry `''` (timing
kept, text removed — audio-cache.ts:94-101).

**`TtsSessionStart`** (core → widget, types.ts:197-211). A new session; any previous one is cancelled.

| Field | Type | Value |
|---|---|---|
| `sessionId` | string | random UUID v4 per session |
| `readId` | string | the `ReadRecord.id` |
| `text` | string | full **cleaned** text being read (for preview-only cached replays: preview + `"…"` placeholders) |
| `title` | string | `previewTitle(text)` — see below (reader.ts:129-135); for replays computed from the record's stored text |
| `voiceName` | string | |
| `sourceApp?` | string | friendly app name, e.g. `"Chrome"`; omitted when empty |
| `estimatedDurationSec` | number | fresh read: `text.length / 15 / speed` with `speed = voice.speed` if finite and > 0, else 1; cached: sum of chunk durations |
| `chunkCount` | number | number of chunks the session will deliver |
| `rate` | number | `settings.playbackRate` |
| `volume` | number | `settings.volume` |
| `highlightWords` | boolean | `settings.widget.highlightWords` |
| `fromCache` | boolean | true for a free cached replay |

Serialized order: `sessionId, readId, text, title, voiceName, rate, volume, highlightWords, sourceApp?,
estimatedDurationSec, chunkCount, fromCache` (engine.ts:141-162, 221-233).

`previewTitle(text, max = 64)` (also the tray tooltip's title source):

```text
flat = text with every whitespace run replaced by one space, trimmed
if len(flat) <= max: return flat
cut   = flat[0..max)
space = last index of ' ' in cut
base  = (space > max * 0.6) ? cut[0..space) : cut          // 0.6 × 64 = 38.4
return base with trailing chars of the class [\s,.;:\-–—] removed  +  "…"   (U+2026)
```

**`TtsChunk`** (core → widget, types.ts:213-223), delivered in order 0,1,2…:

| Field | Type | Meaning |
|---|---|---|
| `sessionId` | string | |
| `index` | number | 0-based chunk index (cached replays renumber to 0..n−1 in index order) |
| `textStart` | number | offset into `TtsSessionStart.text` (UTF-16 code units in the old app) |
| `textEnd` | number | exclusive end offset |
| `audio` | bytes | complete encoded audio file for this chunk (MP3 or WAV) — binary, not base64, over IPC |
| `mime` | string | `'audio/mpeg'` or `'audio/wav'` |
| `alignment` | `Alignment \| null` | |
| `durationSec` | number | best known duration (from alignment / decoded) |

**`TtsSessionDone`** (types.ts:226-229): `{ sessionId, totalChunks }` — all chunks delivered (also sent after a
non-fatal failure, with the number actually delivered).

**`TtsSessionError`** (types.ts:231-237): `{ sessionId, message, code: ReadErrorCode, fatal: boolean }`. `fatal: true`
= session aborted (before any audio); `false` = a later chunk failed, playback of what arrived may continue (the widget
shows it as a warning; default text `Part of the text could not be synthesized`). A fatal error also reaches the
reader as the engine's `failed` event → record `error` + error toast (§5.6). A cancelled/superseded session emits
nothing to the widget.

### 3.5 Player types

**`PlayerState`** (types.ts:243-265) — reported by the widget, stored by core, mirrored to the app window.

| Field | Type | Meaning |
|---|---|---|
| `status` | PlayerStatus | |
| `sessionId` | string \| null | |
| `readId` | string \| null | |
| `title` | string | text preview |
| `voiceName` | string | |
| `sourceApp?` | string | |
| `positionSec` | number | playhead in **audio** seconds (sum of finished chunk durations + current chunk time) |
| `durationSec` | number | known chunk durations + estimate for the unsynthesized rest |
| `durationIsEstimate` | boolean | true until every chunk is known |
| `bufferedFraction` | number | 0..1 share of the session already synthesized ("buffered" dots) |
| `rate` | number | current client playback rate |
| `volume` | number | 0..1 |
| `charIndex` | number | current character offset into the session text (word highlight) |
| `totalChars` | number | session text length |
| `listenedSec` | number | **wall-clock** seconds actually spent audible in this session |
| `error?` | string | |
| `errorCode?` | ReadErrorCode | |

`IDLE_PLAYER_STATE` (types.ts:278-293): `status 'idle'`, `sessionId null`, `readId null`, `title ''`,
`voiceName ''`, `positionSec 0`, `durationSec 0`, `durationIsEstimate true`, `bufferedFraction 0`, `rate 1`,
`volume 1`, `charIndex 0`, `totalChars 0`, `listenedSec 0` (no optional fields). Core's state starts as a copy of it.

The state core stores and relays is the **parsed** one (§5.3), serialized in the order `status, sessionId, readId,
title, voiceName, positionSec, durationSec, durationIsEstimate, bufferedFraction, rate, volume, charIndex, totalChars,
listenedSec, sourceApp?, error?, errorCode?` (main/ipc.ts:150-171).

**`PlayerCommand`** (types.ts:267-276) — tagged union on `type`:

```jsonc
{ "type": "toggle" }
{ "type": "play" }
{ "type": "pause" }
{ "type": "stop" }
{ "type": "seek", "positionSec": 12.5 }          // audio seconds
{ "type": "seekFraction", "fraction": 0.4 }      // 0..1 of available (buffered) audio
{ "type": "skip", "direction": 1, "unit": "sentence" }            // direction is the NUMBER 1 or -1
{ "type": "skip", "direction": -1, "unit": "seconds", "amount": 10 }   // amount optional; widget default 10 s
{ "type": "rate", "rate": 1.5 }                  // 0.5..3, session only (does NOT persist settings)
{ "type": "volume", "volume": 0.8 }              // 0..1, session only
```

Widget semantics (widget/player.ts:284-311): `toggle` play/pause; `play` on an `ended` session seeks to 0 and plays;
`play` is ignored in `error`; `skip` with `unit: 'sentence'` jumps to the previous/next sentence start; `seconds` uses
`amount` if finite and > 0, else 10. `stop` with an active session also hides the widget (controller.ts:95-99). `rate`
and `volume` commands do not write settings — the Settings page calls `settings:update` **and** sends the command so a
read in progress changes immediately (PlaybackSection.tsx:16-20, 39-42). The widget re-applies `settings.playbackRate`
/ `settings.volume` only when **those values change**, so an unrelated settings write never overrides a rate/volume
command from the bridge or app (controller.ts:41-44, 71-83).

### 3.6 History types

**`ReadRecord`** (types.ts:297-331) — one read, persisted in `history.json`.

| Field | Type | Meaning / rules |
|---|---|---|
| `id` | string | UUID v4 (≤ 128 chars after sanitize; must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$` to have cached audio) |
| `createdAt` | number | epoch **ms** |
| `text` | string | full cleaned text, or the first 160 chars when `library.storeText` was off |
| `textTruncated` | boolean | true only when `storeText` was off **and** the text was longer than 160 |
| `charCount` | number | length of the full cleaned text (UTF-16 units) |
| `wordCount` | number | count of `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu` matches in the full text (reader.ts:137-139) |
| `voiceId`, `voiceName`, `modelId` | string | as used for synthesis (replays re-synthesize with the original voice) |
| `source` | ReadSource | |
| `sourceApp?` | string | friendly name, e.g. `"Chrome"` |
| `sourceProcess?` | string | lower-case exe, e.g. `"chrome.exe"` (lower-cased by sanitize) |
| `sourceTitle?` | string | window or page title, trimmed, ≤ 200 chars (the 200 cut is applied by the reader when the record is created, not by sanitize) |
| `sourceUrl?` | string | http(s) URL (browser extension) |
| `audioDurationSec` | number | total synthesized audio length; 0 until synthesis completes (rounded to 0.1) |
| `listenedSec` | number | wall-clock seconds listened, all sessions incl. replays (rounded to 0.1) |
| `maxPositionSec?` | number | furthest audio position reached (max over every session). "Heard %" = `maxPositionSec / audioDurationSec`, correct at any playback rate. Absent on older records; 0 is stored as absent. |
| `listenLog?` | map `'YYYY-MM-DDTHH'` → seconds | hour ledger, §6 |
| `completed` | boolean | played to the end (never set by a partial read) |
| `cached` | boolean | audio available for free replay |
| `charactersBilled` | number | characters sent to ElevenLabs (0 for cached replays), integer |
| `favorite` | boolean | favourites are exempt from retention |
| `error?` | string | failure message, or `PARTIAL_READ_ERROR` |

Record sanitize rules (history-store.ts:52-98), applied to every record loaded from disk and every add/update. Each
field falls back to `base` (the existing record on update; on load/add there is no base, so the "default" applies):

| Field | Rule |
|---|---|
| `id` | non-blank string → trimmed, cut to 128; else base id. **Required** on load/add (record dropped) |
| `createdAt` | finite number (any sign; not rounded); else base. **Required** on load/add |
| `text` | any string (not trimmed); else base, default `''` |
| `textTruncated`, `completed`, `cached`, `favorite` | JSON boolean; else base, default `false` |
| `charCount`, `wordCount`, `charactersBilled` | finite → `max(0, v)` → rounded (`js_round`); else base; defaults `text.length`, whitespace word count (`text.trim()` empty → 0, else number of `/\s+/`-separated parts — **a different word rule from the reader's**), `0` |
| `voiceId`, `voiceName`, `modelId` | any string (not trimmed); else base, default `''` |
| `source` | valid ReadSource string; else base, default `'manual'` |
| `sourceApp`, `sourceTitle`, `sourceUrl`, `error` | `undefined` → base; non-string → base; string → trimmed, `''` → **absent** |
| `sourceProcess` | same, then lower-cased |
| `audioDurationSec`, `listenedSec` | finite → `max(0, v)` (not rounded here); else base, default 0 |
| `maxPositionSec` | finite → `max(0, v)`; else base, default 0; then **0 → absent** |
| `listenLog` | key absent/`undefined` → base; else `sanitizeListenLog(v)` (§6), which may clear it (e.g. `{}` or `null`) |

Absent optional fields are deleted from the object (never serialized as `null`). Serialized field order: `id,
createdAt, text, textTruncated, charCount, wordCount, voiceId, voiceName, modelId, source, sourceApp?, sourceProcess?,
sourceTitle?, sourceUrl?, audioDurationSec, listenedSec, maxPositionSec?, listenLog?, completed, cached,
charactersBilled, favorite, error?`.

`update(id, patch)` (history-store.ts:249-265): unknown id → `null`. `merged = { ...existing, ...patch, id:
existing.id }` (the id can never change); for `sourceApp, sourceProcess, sourceTitle, sourceUrl, error`, a key present
in the patch with value `undefined` is turned into `''` so it clears; any other `undefined` in a patch means "keep".
`sanitize(merged, existing)`; if the result equals the existing record (every key `===`, ledgers via
`sameListenLog`) → return the existing record, no write, no notification. If `createdAt` is unchanged the record is
replaced in place, else re-inserted at its sorted position. Records stay sorted newest first by `createdAt` (ties: the
newer insertion goes first). `add(rec)` replaces any record with the same id. `remove(id)` of an unknown id and
`clear()` of an empty history are no-ops (no write, no event).

`list(q)` (history-store.ts:275-299): `limit` finite → `floor`, clamp 1..500, default 50; `offset` finite → `floor`,
clamp ≥ 0, default 0; search terms = `search.trim()` split on `/\s+/`, empty dropped, first 16, each a
case-insensitive **literal** substring (regex-escaped); a record matches when every term occurs in at least one of
`text, sourceApp, sourceTitle, voiceName, sourceUrl`; plus `favoritesOnly`, `source` filters; result
`{ items: matches[offset .. offset+limit), total: matches.length }`.

`applyRetention(days, now)`: `days` not finite or ≤ 0 → 0; else remove every **non-favorite** record with
`createdAt < now − days × 86 400 000`; returns the count. Every removal path (remove, clear, retention) notifies the
synchronous `onRemoved(ids)` listeners (core deletes their cached audio) and deletes `history.json.corrupt`.

**`HistoryQuery`** (types.ts:333-339): `{ search?: string, limit?: number /* default 50 */, offset?: number,
favoritesOnly?: boolean, source?: ReadSource }`.

**`HistoryPage`** (types.ts:341-344): `{ items: ReadRecord[], total: number }` — `total` = number of matches before
paging.

### 3.7 Analytics types

**`AnalyticsSummary`** (types.ts:348-374; computed by analytics.ts:99-234, all calendar math in **local time**,
records dated after today ignored):

| Field | Meaning |
|---|---|
| `range` | requested range (invalid → `'30d'`) |
| `generatedAt` | epoch ms (`now`) |
| `totals.reads/words/chars/audioSec/completedReads/charactersBilled` | over records **created** in range (range start = today − (N−1) days at local 00:00; `'all'` = local day of the oldest valid record, or today when there are none). Each summand is `nonNeg` (finite and > 0, else 0). Only `audioSec` is rounded (0.1) |
| `totals.listenedSec` | listening that **happened** in `[rangeStart, tomorrow 00:00)` (ledger entries of **all** valid records, §6), rounded 0.1 |
| `today` | `{ listenedSec (happened in [today 00:00, tomorrow 00:00), rounded 0.1), reads, words (created today) }` |
| `streak` | `{ current, longest }` over **all** valid records (not just the range): a day counts if a read was created that day or a ledger entry of ≥ 1 s falls on it (before tomorrow). `current` counts back from today, or from yesterday if today is empty; `longest` = longest run of consecutive calendar days (`max(longest, current)`) |
| `daily[]` | one `{ date: 'YYYY-MM-DD', listenedSec, reads, words }` per local calendar day from `max(rangeStart, today − 364 days)` to today, oldest → newest (≤ 365 entries; `'all'` capped to the last 365 days). `reads`/`words` from records created that day (in range); `listenedSec` from ledger entries that day, rounded 0.1 |
| `heatmap` | 7×24 `[weekday 0=Mon..6=Sun][hour 0..23]` listened seconds (index `(jsDay + 6) % 7`, local hour), each cell rounded 0.1 |
| `topApps[]` | `{ name, reads, listenedSec }`. `reads` from records created in range; `listenedSec` from in-range ledger entries of all records, so an old read replayed now appears with `reads: 0`. Groups with neither are dropped. Sorted by listenedSec (rounded) desc, then reads desc, then name (locale compare); top 6. name = trimmed `sourceApp` or by source: manual/replay → `Vox` (`APP_NAME`), clipboard → `Clipboard`, explorer → `Explorer`, cli → `Command line`, browser-extension → `Browser`, else `Other` |
| `topVoices[]` | `{ voiceId, name, reads, listenedSec }`, keyed by `voiceId || voiceName || 'unknown'`; name = voiceName of the newest in-range record of that key (for listening-only groups: the first record seen), fallback the key; same sort; top 6 |
| `sources[]` | `{ source, reads }` for records created in range, sorted by reads desc, then source (locale compare) |
| `avgWordsPerRead` | `round(words / reads)` or 0 |
| `avgListenWpm` | `heardListenedSec > 0 ? round(heardWords / (heardListenedSec/60)) : 0` where heardWords = Σ wordCount × heardFraction, heardListenedSec = Σ record listenedSec (both over records created in range) |
| `readingTimeEquivalentSec` | `round(heardWords / 238 × 60)` |

Valid records: `createdAt` finite and `< tomorrow 00:00` local (records dated in the future — clock changes — are
ignored everywhere). Day arithmetic is calendar-based (`new Date(y, m, d + n)`), never `± 86 400 000`, so DST days
are neither skipped nor repeated (analytics.ts:20-27).

`heardFraction(r)` (analytics.ts:36-44): 0 if nothing listened; 1 if completed; else
`min(1, (maxPositionSec>0 ? maxPositionSec : listenedSec) / (audioDurationSec || charCount/15))`; 1 if the audio
length is unknown (≤ 0). Seconds values rounded to 0.1.

**`CacheStats`** (types.ts:376-380): `{ reads: number /* cached entries */, files: number /* chunk files */,
bytes: number /* audio + manifest bytes */ }`. Computed from the in-memory index (no disk scan). The Settings UI shows
bytes in decimal units: `< 1 000 000` → `0 MB` if 0 else `max(1, round(b/1000)) KB`; `< 10^9` → `(b/10^6).toFixed(1)`
below 10 MB else `round(b/10^6)` + ` MB`; else `(b/10^9).toFixed(1) GB` (settings/logic.ts:170-175).

### 3.8 App-shell types

| Type | Shape | Notes |
|---|---|---|
| `AppRoute` (types.ts:386-390) | `{ page: AppPage, section?: string }` | `section` is an anchor inside the page, e.g. settings section id. Hash form: `/<page>` or `/<page>/<encodeURIComponent(section)>` (windows.ts:270-272). Settings section ids: `account, voice, playback, triggers, widget, appearance, library, integrations, about` (settings/logic.ts:20-30). |
| `Toast` (types.ts:392-397) | `{ id: string, kind: 'info'\|'success'\|'error', message: string, action?: { label: string, route?: AppRoute } }` | `id` = UUID v4 assigned by core (serialized first: `id, kind, message, action?`). Core itself never emits `success` (UI-local toasts only); every core action carries a route. |
| `AppInfo` (types.ts:399-415) | `{ name, version, electronVersion, platform, isPackaged, userDataPath, extensionPath, serverPort, serverRunning, nativeHookAvailable, explorerMenuInstalled, demoMode }` | `name` = `APP_NAME`; `version` = app version (`0.1.0`); `extensionPath` = folder of the unpacked browser extension (packaged: `<resources>/browser-extension`; dev: `<app>/browser-extension`). `serverPort` = `server.port || settings.server.port` where the server's port is the bound port while running, else the last requested port (so after a failed start it shows the requested port with `serverRunning: false`); `serverRunning` = listening now (false if the bridge was never created). `userDataPath` = the effective userData (the `-Demo` folder in demo mode). `nativeHookAvailable` = waits for the hook's first load result, at most 2 s, then the current flag; `explorerMenuInstalled` = registry check (false on error). `platform` = `'win32'`. `demoMode` = runtime demo flag. In Rust replace `electronVersion` with a runtime/build string (UI shows it on About). main/ipc.ts:239-294 |
| `ChipShowPayload` (types.ts:417-420) | `{ mode: 'context'\|'selection', label: string }` | label = `READ_WITH_LABEL` for context, `'Read'` for selection (windows.ts:778). |
| `TitleBarTheme` (types.ts:422-425) | `{ color: string, symbolColor: string }` | CSS colors for caption-button overlay background / glyphs. Accepted forms: `#rgb`…`#rrggbbaa` (3–8 hex digits), `rgb(...)`/`rgba(...)` with digits, whitespace, `.`, `,`, `%`, or `transparent` (§5.3). |
| `OverlaySize` (ipc.ts:103-111) | `{ width, height, hit?: { x, y, width, height } }` | Content size of a floating window in DIP; `hit` (widget only) = the card's rectangle inside the window (window-relative), where core tracks hover. |
| `ResolvedTheme` (types.ts:7) | `'paper' \| 'night'` | `system` resolves to `night` when the OS is in dark mode, else `paper` (windows.ts:274-277). |
| `ReadTextOptions` (ipc.ts:117-122) | `{ source?: ReadSource, sourceApp?: string, sourceTitle?: string, sourceUrl?: string }` | |
| `Unsubscribe` (ipc.ts:115) | `() => void` | |

### 3.9 On-disk formats tied to the model

```jsonc
// history.json (history-store.ts:7-10, 215-221) — compact JSON
{ "version": 1, "records": [ /* ReadRecord, newest first */ ] }
// A bare array of records is also accepted on load (history-store.ts:198). The "version" value is not checked.

// History export (history-store.ts:321-325) — pretty, 2-space indent, written atomically (<path>.tmp), no .bak
{ "app": "Vox", "version": 1, "exportedAt": "2026-10-17T09:44:00.000Z", "records": [ /* every ReadRecord, newest first */ ] }

// audio-cache/<readId>/manifest.json (audio-cache.ts:18-35) — compact JSON, written atomically LAST (no .bak),
// so a missing manifest = interrupted save. createdAt / lastAccess are epoch ms.
{ "version": 1, "readId": "<id>", "createdAt": 0, "lastAccess": 0,
  "chunks": [ { "index": 0, "textStart": 0, "textEnd": 212, "mime": "audio/mpeg",
                "alignment": { "chars": [], "starts": [], "ends": [] } | null,
                "durationSec": 14.2, "file": "0.mp3", "bytes": 227000 } ] }
// chunk file = "<index>.<ext>", ext from mime: contains mpeg|mp3 → mp3, wav|wave → wav, ogg|opus → ogg, else bin.
// chunk file names must match ^\d{1,6}\.(mp3|wav|ogg|bin)$ ; readId dirs must match ^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$
```

History load (history-store.ts:197-236): records that fail sanitize are dropped (warning `ignored N malformed history
record(s)`); the rest are sorted by `createdAt` descending (stable: equal timestamps keep file order) and de-duplicated
by `id` (first kept). Records added before the first load (an early read) are merged in afterwards and trigger a save.

Audio cache rules (audio-cache.ts): a manifest is valid only if it is an object whose `readId` equals the folder name
and whose `chunks` is a non-empty array of objects each with a valid `file` name and a string `mime`; other numbers
fall back to 0, `createdAt` to now, `lastAccess` to `createdAt`; an alignment whose three arrays are not equal-length
arrays becomes `null`. At startup every **directory** in `audio-cache/` with an invalid name is deleted, and every
entry whose manifest is missing/corrupt or whose chunk files are missing or 0 bytes is deleted (plain files at the
root are left alone at startup; `cache:clear` deletes them). Bytes per entry = manifest size + chunk file sizes.
Loading a cached read rewrites `lastAccess` (LRU = last save or load). `prune(maxBytes, protect)` evicts by ascending
`lastAccess` until the total fits, never touching protected ids (the read just saved, the engine's current read, the
widget's current read); prunes are serialized. Operations on the same readId are serialized. On startup core also
clears `cached` on records without an entry, redacts manifests that still hold text for preview-only records, and
deletes entries whose read is no longer in history (index.ts:107-121).

---

## 4. Settings schema

### 4.1 Every key: default, allowed values, sanitize rule, effects

Sanitize helpers (settings-store.ts:25-79): **bool** = must be boolean else fallback; **num(min,max[,int])** = must be a
finite number else fallback, then (if int) rounded, then clamped; **oneOf** = must be one of the listed strings else
fallback; **nonEmpty** = string, trimmed, cut to 200 chars, empty → fallback. *Fallback* = the value in `base` (the
current settings for an update, `DEFAULT_SETTINGS` when loading from disk) — **an invalid value in a patch silently
keeps the current value**, it does not reset to the default and does not raise an error.

| Key | Type | Default | Allowed / sanitize | Effect when changed (core: index.ts:295-330 unless noted) |
|---|---|---|---|---|
| `version` | `1` | 1 | always forced to 1 | — |
| `hasApiKey` | bool | false | derived from key presence; never stored | — |
| `onboarded` | bool | false | bool | initial route home vs onboarding; `startMinimized` is ignored until onboarded |
| `voiceId` | string | `JBFqnCBsd6RMkjVDRZzb` | nonEmpty ≤ 200 | next read |
| `voiceName` | string | `George` | nonEmpty ≤ 200 | next read |
| `modelId` | string | `eleven_flash_v2_5` | nonEmpty ≤ 200 | next read; models matching `^eleven_v3` never get previous/next text context |
| `outputFormat` | OutputFormat | `mp3_44100_128` | oneOf 3 values | next read |
| `voice.stability` | number | 0.5 | 0..1 | next read |
| `voice.similarityBoost` | number | 0.75 | 0..1 | next read |
| `voice.style` | number | 0 | 0..1 | next read |
| `voice.useSpeakerBoost` | bool | true | bool | next read |
| `voice.speed` | number | 1 | 0.7..1.2 | next read; also divides the duration estimate |
| `languageCode` | string | `''` | trimmed + lower-cased; `''` allowed; else must match `^[a-z]{2,3}([-_][a-z0-9]{2,8})?$`, otherwise fallback | next read (dropped and retried once if the API rejects it) |
| `playbackRate` | number | 1 | 0.5..3 (not integer). UI presets 0.75/1/1.25/1.5/1.75/2 | widget applies when the value changes |
| `volume` | number | 1 | 0..1. UI slider step 0.01 | widget applies when the value changes |
| `triggers.rightClickChip` | bool | true | bool | selection service re-configured (on every change) |
| `triggers.selectionPopup` | bool | false | bool | selection service |
| `triggers.hotkeysEnabled` | bool | true | bool | hotkeys re-registered |
| `triggers.hotkeys.readSelection` | string | `Ctrl+Alt+R` | any string, trimmed, ≤ 64 chars (`''` = disabled); non-string → fallback. Reserved/duplicate/AltGr chords are **stored** but skipped at registration | hotkeys re-registered; failures toast |
| `triggers.hotkeys.readClipboard` | string | `Ctrl+Alt+V` | same | same |
| `triggers.hotkeys.playPause` | string | `Ctrl+Alt+P` | same | same |
| `triggers.hotkeys.stop` | string | `Ctrl+Alt+S` | same | same |
| `triggers.mediaKeys` | bool | true | bool | toast `Restart Vox to finish turning media keys on\|off` (info); applied at next start |
| `triggers.browserExtension` | bool | true | bool | bridge started/stopped |
| `triggers.explorerMenu` | bool | false | bool | Explorer verb installed/removed (forced rewrite; not in demo); failure toast `Couldn't update the Explorer menu[: <err>]`. At startup the verb is only rewritten when stale (partial or pointing at another exe) |
| `triggers.excludedApps` | string[] | `[]` | must be an array else fallback; per item: non-strings skipped; trimmed, lower-cased, keep the part after the last `\` or `/`, trimmed; empty skipped; `.exe` appended if missing; de-duplicated (first kept); stop at 200 entries. The Settings UI is stricter before it sends (settings/logic.ts:67-83): also strips surrounding `"`, rejects `< > : " \| ? *` and control chars, a bare `.exe`, a dot/space before `.exe`, and names > 120 chars — errors `Enter a program name like notepad.exe`, `<name> is already excluded` | selection service |
| `triggers.restoreClipboard` | bool | true | bool | selection service |
| `widget.enabled` | bool | true | bool | true→false hides the widget now; false→true does nothing until the next read/toast |
| `widget.position` | WidgetPosition | `bottom-right` | oneOf 5 values | repositions if visible (skipped when the same change disabled the widget) |
| `widget.customX` | number \| null | null | `null` allowed; finite → clamped ±100 000 then rounded (`js_round`; negative values are legal: monitors left of / above the primary); else fallback | repositions if visible |
| `widget.customY` | number \| null | null | same | repositions if visible |
| `widget.autoHide` | bool | true | bool | widget auto-hide |
| `widget.highlightWords` | bool | true | bool | next session (`TtsSessionStart.highlightWords`) |
| `widget.compact` | bool | false | bool | widget layout |
| `app.launchAtLogin` | bool | false | bool | autostart entry updated (not in demo) |
| `app.startMinimized` | bool | false | bool | autostart entry updated (start hidden) |
| `app.closeToTray` | bool | true | bool | app window close behavior |
| `app.theme` | ThemeMode | `system` | oneOf 3 values | native theme mode (paper → light, night → dark, system → follow OS) + app window background (`#ececec` paper / `#000000` night); caption-button colors are set separately by the UI via `app:set-titlebar-theme` |
| `library.saveHistory` | bool | true | bool | next read |
| `library.storeText` | bool | true | bool | next read |
| `library.cacheAudio` | bool | true | bool | next completed synthesis |
| `library.cacheLimitMB` | integer | 500 | 50..10 000 (rounded). **UI slider is 50..5000 step 50** | lowering it prunes the cache now |
| `library.retentionDays` | integer | 0 | 0..3650 (rounded); UI options 0 "Keep forever", 7 "7 days", 30 "30 days", 90 "90 days", 365 "1 year" (+ current custom value as "N days") | applied 60 s after the last change (each change restarts the 60 s). The UI first confirms how many reads will be deleted when the new value is stricter (`next > 0 && (current ≤ 0 \|\| next < current)`), counting non-favorites older than `now − days × 86 400 000` by binary search over `history:list` pages of 1 (settings/logic.ts:207-244) |
| `text.skipUrls` | bool | true | bool | next read |
| `text.skipEmails` | bool | false | bool | next read |
| `text.maxChars` | integer | 20 000 | 100..100 000 (rounded); UI presets 2000, 5000, 10 000, 20 000, 50 000, 100 000 (+ current), labelled `20,000 characters` | next read |
| `server.port` | integer | 47823 | 1024..65 535 (rounded); UI accepts only trimmed `^\d{1,5}$` within that range | bridge restarted on the new port |

Every effective change additionally: broadcasts `settings:changed` to **all** UI surfaces, re-configures the
selection service (except screenshot mode), and refreshes the tray menu. The tray menu itself writes
`triggers.rightClickChip`, `widget.enabled`, `triggers.hotkeysEnabled` (checkbox items "Right-click chip",
"Floating widget", "Hotkeys") (tray.ts:98-115). A user drag of the widget writes
`{ widget: { position: 'remember', customX, customY } }` (index.ts:453-455). Demo mode writes `onboarded: true`.

Unknown keys anywhere are **dropped**.

### 4.2 Widget placement per `widget.position` (windows.ts:69-113)

Area = the work area of the display nearest the cursor (for `remember` with coordinates: the display matching the saved
rect). `m = 24`. All results are then clamped inside the area (an oversized rect pins to the area's top-left).

| Position | Top-left | Anchor (edges kept fixed on resize) |
|---|---|---|
| `bottom-right` (and fallback) | `(area.right − w − m, area.bottom − h − m)` | right, bottom |
| `top-right` | `(area.right − w − m, area.top + m)` | right, top |
| `bottom-center` | `(area.x + (area.width − w)/2, area.bottom − h − m)` | center, bottom |
| `near-cursor` | `(cursor.x + 16, cursor.y + 20)`; if it overflows right (beyond `area.right − m`): `x = cursor.x − w − 16`, anchor right; if it overflows bottom: `y = cursor.y − h − 16`, anchor bottom | left/right, top/bottom |
| `remember` | `(customX, customY)` if both non-null (clamped first), anchor by quadrant of the clamped rect (center strictly right of the area's mid-x → right, else left; strictly below mid-y → bottom, else top); else as `bottom-right` | quadrant |

`clampToArea(r, area)`: `x = max(area.x, min(r.x, area.x + area.width − r.width))`, same for y; then x, y, width,
height are each `js_round`ed (an oversized rect pins to the area's top-left). `resizeAnchored(old, size, anchor,
area)`: `x = old.x` (left), `old.x + old.width − size.width` (right), `old.x + (old.width − size.width)/2` (center);
`y = old.y` (top) or `old.y + old.height − size.height` (bottom); then `clampToArea`. Placement happens only when the
widget is shown from hidden, on a placement-setting change while visible, or on a display change while visible
(windows.ts:52-123, 655-675).

### 4.3 Update algorithm (settings-store.ts:173-182, 265-272, 295-299)

```text
update(patch):
  merged  = deepMerge(current, patch)
  next    = sanitizeSettings(merged, base = current, hasApiKey = (apiKey != null))
  if JSON(next) == JSON(current): return current           // no write, no event
  current = next; schedule debounced write (250 ms); notify listeners(next, prev); return next

deepMerge(base, patch):
  if patch is not a plain object: return (patch === undefined ? base : patch)   // arrays, null, primitives replace
  out = shallow copy of base (or {} if base is not a plain object)
  for (key, value) in patch:
     skip if value === undefined or key ∈ {"__proto__", "constructor", "prototype"}
     out[key] = (value and out[key] both plain objects) ? deepMerge(out[key], value) : value
```

`sanitizeSettings(input, base, hasApiKey)` rebuilds a complete `Settings` field-by-field per the table in §4.1 using
`base` for fallbacks (settings-store.ts:85-171); a missing or non-object `input` or sub-object counts as `{}` (all
fallbacks). The no-op comparison is on the full compact JSON string (including `hasApiKey`); since sanitize always
emits the same key order this equals a structural comparison. Listeners get `(next, prev)`; a throwing listener is
logged and does not stop the others. `update()` before `init()` only logs a warning (the change may be overwritten
by the load).

### 4.4 Load / persist (settings-store.ts:274-289, 334-350)

- `init()`: flush pending writes, load `settings.json` (with `.bak`/`.corrupt` behavior from §1.7), load the API key,
  `current = sanitizeSettings(raw, DEFAULT_SETTINGS, hasKey)`. If a file was loaded (main or `.bak`) and its compact
  JSON string differs from the sanitized stored form, schedule a rewrite (migration/clamping lands on disk). The
  comparison is textual, so a file with keys in another order, unknown keys, a stray `hasApiKey`, or numbers written
  differently (`1.0`) is also rewritten — harmless, but a Rust port writing a different key order would cause the old
  app to rewrite the file once. Before `init()` the in-memory settings are a copy of the defaults.
- Stored form = `Settings` without `hasApiKey`, pretty-printed, field order as in the struct.
- `readSettingsFileSync()` (used before UI/runtime init for the media-keys switch): reads `settings.json` (BOM
  stripped) and sanitizes against defaults (with `hasApiKey: false`); a **missing** main file → none (no `.bak`); an
  unreadable/invalid main file → try `settings.json.bak` (missing or invalid → none). No retries, no `.corrupt` copy.
  Never throws.

### 4.5 API key storage (settings-store.ts:202-263, 309-322)

- Kept only in core memory; **never sent to any UI surface** (UI only sees `hasApiKey`).
- `secrets.bin` = `safeStorage.encryptString(key)`. On Windows Electron's safeStorage is Chromium OSCrypt: an AES-256-GCM
  key stored DPAPI-protected in `userData/Local State`, blob prefixed `v10` + 12-byte nonce + ciphertext + 16-byte tag
  (verify before relying on it for migration — see open questions). A Rust port should use DPAPI directly
  (`CryptProtectData`/`CryptUnprotectData`, current-user scope).
- Load: read `secrets.bin` (read errors other than "not found" → warn); a non-empty blob: if encryption is
  unavailable → warn `secure storage is unavailable; the stored API key cannot be decrypted`; else decrypt and trim
  (failure → warn); a non-empty result is the key. Otherwise fallback to the trimmed `ELEVENLABS_API_KEY` env var
  (never written to `secrets.bin`); else no key. Must run after the platform is ready (safeStorage needs it).
- `setApiKey(key|null)`: trims; `''` → null; no-op if equal to the current key. Persists (serialized queue, each step
  runs even if the previous failed): null → delete `secrets.bin` ("not found" ignored); encryption unavailable → keep
  in memory for this session only (warn `... the API key is kept in memory for this session only`) and delete
  `secrets.bin`; else atomic write of the encrypted blob (no `.bak`). Then sets `hasApiKey` and **emits a change even
  if `hasApiKey` did not change** (key replaced) so every surface refetches account/voices. The emit goes through the
  same listener path → `settings:changed` broadcast. It does **not** schedule a `settings.json` write (`hasApiKey` is
  not stored). `flush()` waits for both the settings file and the secrets queue.
- The ElevenLabs client caches are keyed by a SHA-256 fingerprint of the key, so a new key never sees old cached
  voices/models/account.

---

## 5. IPC surface

### 5.1 Transport semantics

Three kinds (ipc.ts:29-34):

| Kind | Direction | Semantics |
|---|---|---|
| **invoke** | UI → core | request/response; returns a value or **rejects**. A rejection's message arrived in the UI wrapped as `Error invoking remote method '<channel>': Error: <msg>`; the UI strips that with `^Error invoking remote method '[^']*':\s*` then `^(\w*Error):\s*` (settings/logic.ts:326-331). *Rust:* return `Result<T, String>` with the plain message. |
| **send** | UI → core | fire-and-forget; no reply, invalid payloads silently dropped. |
| **event** | core → UI | push to one or more surfaces. |

Sender trust (main/ipc.ts:245-267, windows.ts:344-353): every invoke handler first checks the caller is one of the three
own windows (alive), else rejects with `Untrusted sender`. Send handlers have an origin class: `any` (any own window),
`widget` (only the widget), `chip` (only the chip); others are dropped silently. Send handler exceptions are caught and
logged (`<channel> handler failed`); invoke handler exceptions become the rejection message. Missing arguments arrive
as `undefined`, extra ones are ignored. Registering a handler replaces any previous one. **UI surfaces are treated as
untrusted input: every argument is validated in core.** Keep these validators in Rust even in-process: the same
`PlayerCommand` shape also arrives from the HTTP bridge.

"Object" in the validators below means `isObj(v)`: not null, `typeof 'object'`, not an array (i.e. a JSON object).

Event delivery (windows.ts:821-840): `sendToApp` / `sendToWidget` / `sendToChip` deliver to one surface if it exists
and is alive (silently dropped otherwise). Events are **not** queued: one sent before the surface's UI has subscribed
(page still loading) is lost — which is why reads wait for widget readiness and the widget reports its state on load;
`broadcast` delivers to every window. A payload of `undefined` means "no
payload". Events to one surface arrive in send order.

### 5.2 Channel table (shared/ipc.ts:35-101)

| Constant | Channel | Kind | From → To | Payload → Result |
|---|---|---|---|---|
| `settingsGet` | `settings:get` | invoke | any → core | `()` → `Settings` |
| `settingsUpdate` | `settings:update` | invoke | any → core | `(patch: SettingsPatch)` → `Settings` |
| `settingsSetApiKey` | `settings:set-api-key` | invoke | any → core | `(key: string)` → `ApiKeyResult` |
| `settingsClearApiKey` | `settings:clear-api-key` | invoke | any → core | `()` → `Settings` |
| `settingsChanged` | `settings:changed` | event | core → **all** surfaces | `Settings` |
| `elevenVoices` | `eleven:voices` | invoke | any → core | `(opts?: { refresh?: boolean })` → `Voice[]` |
| `elevenModels` | `eleven:models` | invoke | any → core | `(opts?)` → `TtsModel[]` |
| `elevenAccount` | `eleven:account` | invoke | any → core | `(opts?)` → `AccountInfo \| null` |
| `readText` | `reader:read-text` | invoke | any → core | `(text: string, opts?: ReadTextOptions)` → `ReadResult` |
| `readClipboard` | `reader:read-clipboard` | invoke | any → core | `()` → `ReadResult` |
| `readReplay` | `reader:replay` | invoke | any → core | `(readId: string)` → `ReadResult` |
| `playerGetState` | `player:get-state` | invoke | any → core | `()` → `PlayerState` |
| `playerCommand` | `player:command` | send **and** event | any → core; core → widget | `PlayerCommand` |
| `playerState` | `player:state` | send **and** event | widget → core; core → app | `PlayerState` |
| `historyList` | `history:list` | invoke | any → core | `(HistoryQuery)` → `HistoryPage` |
| `historyGet` | `history:get` | invoke | any → core | `(id)` → `ReadRecord \| null` |
| `historyRemove` | `history:remove` | invoke | any → core | `(id)` → `void` |
| `historyClear` | `history:clear` | invoke | any → core | `()` → `void` |
| `historyToggleFavorite` | `history:toggle-favorite` | invoke | any → core | `(id)` → `ReadRecord \| null` |
| `historyExport` | `history:export` | invoke | any → core | `()` → `string \| null` (saved path) |
| `historyChanged` | `history:changed` | event | core → app | none |
| `analyticsSummary` | `analytics:summary` | invoke | any → core | `(range: AnalyticsRange)` → `AnalyticsSummary` |
| `cacheStats` | `cache:stats` | invoke | any → core | `()` → `CacheStats` |
| `cacheClear` | `cache:clear` | invoke | any → core | `()` → `CacheStats` |
| `appInfo` | `app:info` | invoke | any → core | `()` → `AppInfo` |
| `appOpenExternal` | `app:open-external` | invoke | any → core | `(url)` → `void` (http/https only) |
| `appRevealPath` | `app:reveal-path` | invoke | any → core | `('userData' \| 'extension')` → `void` |
| `appShowMain` | `app:show-main` | send | any → core | `(route?: AppRoute)` |
| `appHideToTray` | `app:hide-to-tray` | send | any → core | `()` |
| `appQuit` | `app:quit` | send | any → core | `()` |
| `appSetTitleBarTheme` | `app:set-titlebar-theme` | send | any → core | `(TitleBarTheme)` |
| `appSuspendShortcuts` | `app:suspend-shortcuts` | send | any → core | `(suspend: boolean)` |
| `appNavigate` | `app:navigate` | event | core → app | `AppRoute` |
| `appToast` | `app:toast` | event | core → app **and** widget | `Toast` |
| `ttsSessionStart` | `tts:session-start` | event | core → widget | `TtsSessionStart` |
| `ttsChunk` | `tts:chunk` | event | core → widget | `TtsChunk` |
| `ttsSessionDone` | `tts:session-done` | event | core → widget | `TtsSessionDone` |
| `ttsSessionError` | `tts:session-error` | event | core → widget | `TtsSessionError` |
| `widgetSetSize` | `widget:set-size` | send | **widget only** → core | `OverlaySize` (DIP) |
| `widgetHide` | `widget:hide` | send | **widget only** → core | `()` |
| `widgetHover` | `widget:hover` | event | core → widget | `boolean` |
| `chipShow` | `chip:show` | event | core → chip | `ChipShowPayload` |
| `chipHide` | `chip:hide` | event | core → chip | none |
| `chipActivate` | `chip:activate` | send | **chip only** → core | `()` |
| `chipDismiss` | `chip:dismiss` | send | **chip only** → core | `()` |
| `chipSetSize` | `chip:set-size` | send | **chip only** → core | `OverlaySize` (DIP) |

### 5.3 Per-channel behavior and validation (main/ipc.ts:300-455)

**Settings**

- `settings:get` → `settingsStore.get()` (includes derived `hasApiKey`).
- `settings:update(patch)` → rejects `Invalid settings patch` unless `patch` is a plain object (not array/null);
  otherwise §4.3. Returns the resulting settings (callers should adopt the returned object; an invalid field silently
  kept its old value).
- `settings:set-api-key(key)`:
  1. `k = typeof key === 'string' ? key.trim() : ''`.
  2. `k.length > 256` → `{ ok:false, error:"That doesn't look like an API key", settings }` (no network call).
  3. `validateApiKey(k)` (elevenlabs.ts:640-665) — uncached, does not touch the account cache:
     - empty → `{ ok:false, error:'Paste your ElevenLabs API key first' }`
     - contains whitespace (`/\s/`, after trimming the ends) → `{ ok:false, error:"That doesn't look like an API key — it contains spaces" }`
     - `GET https://api.elevenlabs.io/v1/user/subscription` with header `xi-api-key: <k>`, `accept: application/json`,
       15 s timeout:
       - success → `{ ok:true, account }`
       - missing permission → `{ ok:true, account:null, warning:"This key can't read your account usage, so credits won't be shown. Reading works as long as it has Text to Speech access." }`
       - quota → `{ ok:true, account:null, warning: MESSAGES.quota }`
       - unauthorized → `{ ok:false, error: generic ? "ElevenLabs didn't accept this key — check that you copied all of it" : e.message }`
         (generic when no API message or the API code is one of `invalid_api_key, missing_api_key, needs_authorization,
         invalid_authorization_header, unauthorized`)
       - anything else (network, timeout, 5xx…) → `{ ok:false, error: e.message }`
  4. If `ok`, store the key (§4.5; emits a settings change).
  5. Return `{ ok, error, warning, account: res.account ?? null, settings: <current> }`.
- `settings:clear-api-key` → `setApiKey(null)` (deletes `secrets.bin`, emits), returns settings. The env-var fallback is
  only read at init, so a cleared key stays cleared for the session.

**ElevenLabs** — `refresh` = `opts` is an object with `refresh === true`. Demo fallback when demo mode **and** no key:
fixed lists (§7.3). Otherwise the client's memo cache (elevenlabs.ts:482-514): one entry per list, tagged with the key
fingerprint; a call without `refresh` returns the entry if it is for the same key and younger than the TTL, else
joins an in-flight request for the same key; `refresh: true` always starts a new request. Failures are not cached; a
slower, older response never overwrites a newer one. The memo holds only the latest key's data, so a key change never
shows the previous key's voices/account. Calls:

- `eleven:voices` → all voices for the key (`GET /v2/voices?page_size=100&include_total_count=false[&next_page_token=…]`,
  up to 25 pages, until `has_more !== true` or no `next_page_token`; a body without a `voices` array → badResponse;
  falls back to `GET /v1/voices` on a 4xx other than 401/402/403/429 unless missing permission). **Without a key**:
  public `GET /v1/voices` (no `xi-api-key` header; premade only) so Voices works before onboarding; a network/timeout
  failure there rejects with that error, any other failure with the noKey message. Result sorted/de-duplicated (§3.3).
  Cached 10 min. Every request sends `accept: application/json`; `xi-api-key: <key>` when a key exists.
- `eleven:models` → `GET /v1/models` (TTS-capable only). Without a key: rejects with the noKey message. Cached 1 h.
- `eleven:account` → `GET /v1/user/subscription`; `null` when no key or the key lacks permission. Cached 60 s.
- Errors reject with the user-facing message (§3.1).

**Reading**

- `reader:read-text(text, opts)`: rejects `Text must be a string` if `text` is not a string. Text cut to **2 000 000**
  chars. `opts` must be an object else `{}`. `source` must be a valid ReadSource else `'manual'` (the UI may pass any
  source, e.g. `replay` fallbacks); `sourceApp` non-empty string cut to 120 (not trimmed here); `sourceTitle` non-empty
  string cut to 300 (the reader then trims and cuts it to 200); `sourceUrl` kept only if `isHttpUrl` (string ≤ 4096
  chars that parses as an absolute URL — WHATWG URL rules — with protocol `http:`/`https:`). No `sourceProcess` from the
  UI. A read that passes the key/text checks resolves only after widget readiness (≤ 5 s on the first read after
  launch). Then `reader.read(…)`:

  | Outcome | ReadResult | Side effect |
  |---|---|---|
  | no key, not mock | `{ ok:false, code:'no-api-key', error:'No ElevenLabs API key' }` | toast error `Add your ElevenLabs API key to start reading`, action `Add key` → `settings/account` |
  | empty after cleaning | `{ ok:false, code:'empty-text', error:'Nothing to read' }` | toast info `Nothing to read in that selection` |
  | truncated (pre-cut or maxChars) | (continues) | toast info `` `Reading the first ${maxChars.toLocaleString('en-US')} characters` `` (e.g. `20,000`) |
  | engine failed to start | `{ ok:false, readId, code:'unknown', error:<msg> }` | record `error` set; toast error |
  | started | `{ ok:true, readId }` | record added (if saveHistory); widget shown (if enabled) |

  Core waits for the widget to be ready (≤ 5 s) before starting the engine, so the first `tts:session-start` is never
  lost.
- `reader:read-clipboard` → reads clipboard text (a read failure counts as empty); empty/whitespace → toast info
  `Copy some text first` and `{ ok:false, code:'empty-text', error:'The clipboard has no text' }`; else
  `read({ text, source:'clipboard' })` (no source app/process/title).
- `reader:replay(readId)` → `requireId`: must be a non-empty string ≤ 128 chars (not trimmed) else rejects `Invalid id`.
  - not in history → `{ ok:false, code:'unknown', error:'That read is no longer in your history' }` (no `readId`)
  - the audio cache has an entry for the id (independent of the record's `cached` flag) and it loads with ≥ 1 chunk →
    await widget readiness, free cached session (`fromCache: true`, title from the record text, the record's voiceName
    and sourceApp); `{ ok:true, readId }` (same id). Preview-only records rebuild the session text as preview + one
    `"…"` per chunk past it (reader.ts:181-226): a chunk ending inside the preview is unchanged; a chunk straddling the
    preview end gets `textEnd = preview.length + 1` over an appended `"…"` and an alignment clipped to the preview
    characters plus one entry for the rest (chars all `''`); a chunk entirely past it gets `"\n…"` appended and
    `[len−1, len)` with `alignment: null`.
  - otherwise (no entry, or loading failed — a broken entry is deleted): clear the record's `cached` flag if set; then
    `textTruncated` → `{ ok:false, readId, code:'not-cached', error:'The audio is no longer cached and the full text was not saved' }`
  - else a fresh read of the stored text with source `replay`, the record's voice (§1.6 #7), and the record's
    `sourceApp/sourceProcess/sourceTitle/sourceUrl` → new `readId` (a new history record).
  UI fallback seen in the old app: on `not-cached`/`unknown` the widget/app may call `reader:read-text` with the
  text it has (Widget.tsx:186, read-actions.ts:35).

**Player**

- `player:get-state` → last `PlayerState` received from the widget (initially `IDLE_PLAYER_STATE`).
- `player:command` (send, any own surface — including the widget itself): `parsePlayerCommand` (main/ipc.ts:118-144):
  non-object or unknown `type` → dropped; `seek.positionSec` finite → `max(0, v)`; `seekFraction.fraction` finite →
  clamp 0..1; `skip.direction` must be exactly `1` or `-1`, `unit` `'sentence'|'seconds'`, `amount` if finite →
  clamp 0..3600; `rate` finite → clamp 0.5..3; `volume` finite → clamp 0..1; extra fields dropped. Then
  `reader.command(cmd)`: forward as event `player:command` to the widget; if `stop`, also stop synthesis (abort
  in-flight requests). The widget's own Close button sends `player:command {stop}` after stopping locally, so core
  aborts synthesis and echoes `stop` back (harmless: no session left).
- `player:state` (send, **widget only**): first marks the widget ready (even if the payload is then rejected);
  `parsePlayerState` (main/ipc.ts:146-172): rejected (dropped) unless an object with a valid `status`; numbers must be
  finite else the `IDLE_PLAYER_STATE` value; `positionSec, durationSec, charIndex, totalChars, listenedSec` →
  `max(0, ·)`; `bufferedFraction` clamp 0..1; `rate`, `volume` not clamped; `sessionId`/`readId` any string (even
  empty; no length cap) else null; `title` ≤ 300, `voiceName` ≤ 120 (non-strings and `''` → `''`; not trimmed),
  `sourceApp` ≤ 120 (omitted if empty), `error` ≤ 500 (omitted if empty); `durationIsEstimate` boolean else `true`;
  `errorCode` kept only if valid; every other field dropped. Then `reader.handlePlayerState(state)` (reader.ts:560-582):
  1. store as current state; relay as event `player:state` to the **app** window; notify internal listeners (tray
     icon/tooltip refresh);
  2. if `sessionId` → `engine.progress(sessionId, positionSec)` (releases bounded look-ahead);
  3. fold into the tracked session's record (§6.4).

**History**

- `history:list(q)` → `parseHistoryQuery` (main/ipc.ts:174-183): non-object → `{}`; `search` if a string with
  non-blank content → first 500 chars (of the untrimmed string); `limit` finite → `js_round(clamp 1..1000)` (the store
  then floors and clamps 1..500, default 50); `offset` finite → `js_round(max 0)`; `favoritesOnly` only if boolean;
  `source` only if a valid ReadSource. Store semantics: §3.6 `list(q)`.
- `history:get(id)` → record or `null` (`requireId` validation).
- `history:remove(id)` → removes (unknown id → no-op, resolves); removal also deletes that read's cached audio (store
  `onRemoved` hook) and any `history.json.corrupt` copy (so deleted data does not live on). A read still being
  synthesized whose record was removed is never cached.
- `history:clear()` → removes all records (same hooks).
- `history:toggle-favorite(id)` → flips `favorite`, returns updated record or `null`.
- `history:export()` → native Save dialog (owned by the app window if present, else unowned): title `Export history`,
  default path `<Documents>\Vox-history-<YYYY-MM-DD>.json` (date is the **UTC** date from `toISOString()`), one filter
  named `JSON` with extension `json`. Cancel or empty path → `null`. Else writes the export JSON (§3.9) atomically and
  returns the chosen path; a write failure rejects with the file-system error message.
- `history:changed` (event to app, no payload) — two-stage coalescing: the store notifies 100 ms after the last
  mutation (but no later than 400 ms after the first of the burst), then the IPC layer, on the first notification,
  arms a 150 ms timer and ignores further notifications until it fires and sends one event (so ≥ 150 ms between
  events, and every change is followed by an event). Every mutation counts, including core-internal ones (listened
  time every ~2 s while playing, cache flags).

**Analytics & cache**

- `analytics:summary(range)` → invalid range → `'30d'`; `computeAnalytics(allRecords, range)` (§3.7).
- `cache:stats` → current totals.
- `cache:clear` → deletes every cache entry and anything else inside `audio-cache/`, takes the stats (normally all
  zeros), then sets `cached = false` on every record that had it; returns those stats.

**App shell**

- `app:info` → `AppInfo`; may take up to **2 s**: `nativeHookAvailable` waits for the hook to finish loading (or 2 s,
  then reads the current flag); `explorerMenuInstalled` queries the registry (false on error).
- `app:open-external(url)` → rejects `Only http(s) links can be opened` unless `isHttpUrl`; opens in the default
  browser (≙ `ShellExecuteW(NULL, L"open", url, …)`).
- `app:reveal-path(which)` → `'userData'` → userData dir, `'extension'` → extension folder, else rejects
  `Unknown path`. Opens the folder in Explorer (≙ `ShellExecuteW` open); failure only logged.
- `app:show-main(route?)` → `parseRoute`: object with `page` ∈ AppPage, else ignored (shows without navigating);
  `section` kept only if it is a string matching `^[A-Za-z0-9_-]{1,64}$` (JS `^[\w-]{1,64}$`, ASCII). Shows the app
  window: create it if absent (route default `onboarded ? home : onboarding`), else navigate (if a route), restore if
  minimized, show, bring to top, focus.
- `app:hide-to-tray` → hide the app window.
- `app:quit` → quit sequence (§1.4 #10).
- `app:set-titlebar-theme(theme)` → both fields strings that, trimmed, match
  `^(#[0-9a-f]{3,8}|rgba?\([\d\s.,%]+\)|transparent)$` (case-insensitive; `\d` = ASCII digits, `\s` = Unicode
  whitespace), else dropped; applies the trimmed caption overlay colors (height 44). No app window → ignored; a
  platform failure is only logged.
- `app:suspend-shortcuts(suspend)` → coerced `=== true`. `true`: unregister all hotkeys, (re)start a **30 s** safety timer
  that auto-resumes (a UI that crashes mid-recording must not leave hotkeys off). `false`: if suspended, re-register
  from current settings. While suspended, settings changes do not re-register hotkeys.
- `app:navigate` (event to app) — if the app page is still loading, core instead sets its location hash on load
  (`#/<page>[/<section>]`).
- `app:toast` (event to app **and** widget) — `reader.toast(input)` (reader.ts:542-557): assigns `id`, sends to both
  (always, whatever happens next). Then: if the app window exists, is visible **and** focused → nothing else. Else if
  `widget.enabled` → show the widget (only if hidden; repositioned per §4.2 since it was hidden) — its UI displays the
  toast (as a banner on the player card during a read, or as a standalone toast card when idle, then hides itself
  again). Else, for kinds other than `success` and when the OS supports notifications, a **silent** OS notification
  (title `Vox`, body = message; no action). Toasts raised by the shell glue (hotkey failures, Explorer menu, bridge
  port) first wait for widget readiness (index.ts:123-130), at any time, not only at startup.

**TTS stream (core → widget)** — `tts:session-start`, `tts:chunk` (× n, in index order), `tts:session-done`, and
optionally `tts:session-error` (fatal before any audio; non-fatal after a later chunk failed, followed by
`tts:session-done`). A new session-start supersedes the previous session. Cached replays deliver start, all chunks,
done immediately.

**Widget window**

- `widget:set-size(OverlaySize)` (widget only): `sanitizeSize` — payload must be an object whose `width`/`height` are
  finite numbers, clamped to **40..1200 × 24..900** DIP and then rounded; invalid → ignored entirely (no readiness, no
  hit update). Marks the widget ready. `hit` sanitized
  (windows.ts:236-250): all four finite; `left = max(0, floor(x))`, `top = max(0, floor(y))`,
  `right = min(width, ceil(x + w))`, `bottom = min(height, ceil(y + h))`; a rect smaller than 8×8 → no hit (whole
  window counts). The hit is updated even if the size is unchanged. Unchanged size → done. Hidden → just resize
  (top-left kept; placement happens at the next show). Visible → new bounds via `resizeAnchored` (§4.2) against the
  work area of the display matching the **old** bounds, mark "own move" (500 ms grace), and refresh hover tracking.
- `widget:hide` (widget only) → hide if visible. The widget hides itself on Close, Stop, and auto-hide.
- `widget:hover` (event) → see §1.4 #7.

**Chip window**

- `chip:show` (event) → sent **before** the window is placed and shown, so the UI renders the right label/size.
- `chip:hide` (event) → start exit animation (see §1.3).
- `chip:activate` (chip only) → hide chip, `selectionService.chipActivated()` (de-duplicated 300 ms against the hook's
  own detection of the same press; capture selection — via the context-menu path, i.e. +120 ms settle and an
  Escape retry, for `context` mode — and emit a read with source per chip mode).
- `chip:dismiss` (chip only) → hide chip, `selectionService.chipDismissed()`.
- `chip:set-size(OverlaySize)` (chip only) → same object/finite validation, clamp **32..480 × 24..160**, rounded
  (`hit` ignored); remembered **per chip mode** — stored for the mode of the most recent `chip:show` (initially
  `context`); unchanged → nothing; if visible and placed → re-place at the cursor with the new size (and hand the hook
  the new real bounds); else just resize.

### 5.4 The UI-side façade (`window.vox`, preload/index.ts:15-95; type `VoxApi` ipc.ts:125-215)

The UI never touches IPC directly; it calls a typed façade. A Rust UI should expose the equivalent API to its views.

```text
platform: 'win32'
settings.get()                   → invoke settings:get
settings.update(patch)           → invoke settings:update
settings.setApiKey(key)          → invoke settings:set-api-key
settings.clearApiKey()           → invoke settings:clear-api-key
settings.onChange(cb)            ← settings:changed                (returns unsubscribe)
eleven.voices(opts?) / models(opts?) / account(opts?)  → invoke eleven:*
reader.readText(text, opts?)     → invoke reader:read-text (two args)
reader.readClipboard()           → invoke reader:read-clipboard
reader.replay(readId)            → invoke reader:replay
player.getState()                → invoke player:get-state
player.command(cmd)              → send   player:command
player.onState(cb)               ← player:state
history.list(query?)             → invoke history:list (query defaults to {})
history.get / remove / clear / toggleFavorite → invoke history:*
history.exportJson()             → invoke history:export
history.onChange(cb)             ← history:changed (cb called with no args)
analytics.summary(range)         → invoke analytics:summary
cache.stats() / cache.clear()    → invoke cache:*
app.info / openExternal(url) / revealPath(which) → invoke app:*
app.showMain(route?) / hideToTray() / quit() / setTitleBarTheme(theme) → send app:*
app.suspendShortcuts(suspend)    → send app:suspend-shortcuts (suspend === true)
app.onNavigate(cb)               ← app:navigate
app.onToast(cb)                  ← app:toast
widget.onSessionStart/onChunk/onSessionDone/onSessionError ← tts:*
widget.onCommand(cb)             ← player:command
widget.reportState(state)        → send player:state
widget.setSize(size)             → send widget:set-size
widget.hide()                    → send widget:hide
widget.onHover(cb)               ← widget:hover (coerced to boolean === true)
chip.onShow(cb) / chip.onHide(cb) ← chip:show / chip:hide (onHide cb gets no args)
chip.activate() / dismiss() / setSize(size) → send chip:*
```

Each `on*` subscription adds one listener (any number per channel), receives only the payload, and returns an
unsubscribe that removes exactly that listener (preload/index.ts:4-10).

Usage contracts: UI hooks load once via `get()`/`getState()` and then follow the change events (lib/hooks.ts:6-46);
the history UI refetches whenever `history:changed` fires; the widget reports its state once right after loading
(core may have restarted that UI mid-session) (controller.ts:104-105).

### 5.5 Who emits each event, and when

| Event | Emitted by | Trigger |
|---|---|---|
| `settings:changed` | settings store listener (index.ts:296) | every effective settings change, every API-key set/clear |
| `player:state` → app | reader.handlePlayerState | every widget report (status change, every 500 ms while playing, seeks) |
| `player:command` → widget | reader.command | app/tray/hotkey/bridge/widget command |
| `history:changed` | main/ipc.ts:391-398 | history mutation (coalesced) |
| `app:navigate` | windows.navigateMain | `app:show-main` with a route, tray menu items, toast actions |
| `app:toast` | reader.toast | see toast catalog §5.6 (always sent to both app and widget) |
| `tts:*` | TtsEngine via the widget sink (reader.ts:254-261) | synthesis progress (never for a superseded/stopped session) |
| `widget:hover` | windows.pollWidgetHover | hover changes while widget visible; a final `false` when it hides while hovered |
| `chip:show` / `chip:hide` | windows.showChip / hideChip | selection service decisions, chip activate/dismiss, display changes (hide) |

Fan-out summary: `settings:changed` → every window (app, widget, chip); `app:toast` → app + widget;
`player:state`, `history:changed`, `app:navigate` → app only; `tts:*`, `player:command`, `widget:hover` → widget
only; `chip:show`, `chip:hide` → chip only.

### 5.6 Toast catalog (core-originated)

| Kind | Message | Action | Source |
|---|---|---|---|
| error | `Add your ElevenLabs API key to start reading` | `Add key` → settings/account | reader.ts:308-312 |
| info | `Nothing to read in that selection` | — | reader.ts:325 |
| info | `Reading the first <N> characters` | — | reader.ts:332 |
| error | engine start failure message | — | reader.ts:380 |
| info | `Copy some text first` | — | reader.ts:397 |
| error | `Couldn't open <file name>` | — | reader.ts:473 |
| info | `Nothing is playing. Select some text and press <readSelection hotkey>` (hotkeys enabled and that accelerator non-empty, shown trimmed as stored) / `Nothing is playing. Select some text to read it` — the play/pause hotkey when the player is `idle` and no engine session exists; before toasting, core replays the newest history record if it is cached (free), and toasts only if that is impossible or fails | — | reader.ts:484-498 |
| error | synthesis failure message (§3.1) | `Open settings` → settings/account when code is `no-api-key`, `quota`, or `unauthorized` with exactly the badKey message and not a permission problem | reader.ts:664-677 |
| info | `Select some text first` | — | index.ts:349 |
| info | `That looks like a password, so Vox won't read it` (copy flagged secret) | — | selection/service.ts:104, index.ts:351 |
| info | `That text is marked private, so Vox won't read it` (copy flagged private / not for history or cloud) | — | selection/service.ts:105 |
| info | `Vox doesn't read from password managers` | — | selection/service.ts:106 |
| info | `Couldn't restore your previous clipboard (it was too large)` | — | selection/service.ts:107 |
| info | `Couldn't restore your previous clipboard` | — | selection/service.ts:108 |
| error | hotkey failure line (§2.3) | `Open settings` → settings/triggers | index.ts:172 |
| error | `Couldn't update the Explorer menu[: <err>]` | — | index.ts:206 |
| error | `Browser bridge couldn't use port <N>. Pick another port in Settings` | `Open settings` → settings/integrations | index.ts:282 |
| info | `Restart Vox to finish turning media keys on\|off` | — | index.ts:326-327 |

All strings use `APP_NAME` where they say "Vox". The selection notice type allows `error`, but every notice in use is
`info`. Core never emits `success`. `Reading the first N characters` formats N with `,` thousands separators
(`toLocaleString('en-US')`).

### 5.7 End-to-end sequence of one read

```
UI/hotkey/bridge ── read request ──► core: validate, clean, create ReadRecord, history add  (history:changed ↘ app)
core: wait widget ready (≤5 s) ─► tts:session-start ─► widget          core: show widget (showInactive)
core: synth chunk 0,1 (free) ─► tts:chunk ─► widget starts (after output warm-up)
widget ─► player:state {loading/playing, positionSec…} every 500 ms ─► core ─► player:state ─► app
core: engine.progress(pos) releases chunk k when delivered − position < 120 s ─► tts:chunk …
core: all delivered ─► tts:session-done ─► widget;  engine 'complete' ─► cache audio, update record
widget ─► player:state {ended} ─► core: completed = true, final ledger write
widget: auto-hide after 4 s ─► widget:hide ─► core hides window
```

---

## 6. Listen-log ledger (`src/shared/listen-log.ts`, whole file)

Purpose: credit listening to **when it happened** (local hour) rather than to the day the read was created, so a
cached replay of an old read today counts today in Analytics (daily, today, heatmap, streak). Pure functions shared by
core (writes) and analytics (reads). Seconds of `listenedSec` not covered by the ledger (older records, trimmed hours)
count at `createdAt`.

### 6.1 Format

- `ReadRecord.listenLog?: { [hourKey: string]: number }` — seconds listened in that local hour, > 0, rounded to 0.1.
- Hour key = **local** time `YYYY-MM-DDTHH` (zero-padded; `HH` 00–23). Regex: 

```regex
^(\d{4})-(\d{2})-(\d{2})T(\d{2})$
```

- `LISTEN_LOG_MAX_KEYS = 400` hours kept per record (newest kept); older hours fold back into "at createdAt".
- Lexicographic order of keys = chronological order (fixed-width format).

### 6.2 Functions

```text
hourKey(d: local datetime) -> string                               (listen-log.ts:11-14)
  format "{year}-{month:02}-{day:02}T{hour:02}" in LOCAL time (year not padded; always 4 digits in practice).

hourKeyDate(key) -> local datetime | null                          (listen-log.ts:16-24)
  m = KEY_RE.match(key) else null
  y, mo, d, h = ints
  null if mo < 1 or mo > 12 or d < 1 or d > 31 or h > 23
  return local datetime (y, mo, d, h:00:00)    // JS Date: day overflow ROLLS OVER (e.g. 2025-02-31T10 -> 2025-03-03 10:00);
                                               // a non-existent DST hour moves forward. Null only if not finite.
                                               // JS quirk: years 0000-0099 are read as 1900-1999 (new Date(y, …)).
                                               // Use [0-9] (ASCII) for \d in Rust.

round1(n) = round(n * 10) / 10                 // JS Math.round (half up); inputs are non-negative here

sanitizeListenLog(v: any) -> map | undefined                       (listen-log.ts:28-43)
  if v is not a plain object (null, array, primitive): return undefined
  entries = []
  for (key, sec) in v:
     skip unless sec is a finite number and sec > 0
     skip unless hourKeyDate(key) != null
     entries.push((key, round1(sec)))
  if entries empty: return undefined
  sort entries by key ascending (string compare)
  return map of the LAST 400 entries (newest), keys in ascending order (= serialized order)
  // note: an entry whose value rounds to 0.0 (e.g. 0.04) is KEPT with value 0 — the >0 check happens before rounding.
  // note: values must be JSON numbers; "12" (a string) is dropped.

addToListenLog(log | undefined, key, sec) -> map                   (listen-log.ts:45-50)
  next = copy of log (or {})
  if sec finite and > 0: next[key] = round1((next[key] or 0) + sec)
  return sanitizeListenLog(next) or {}          // never undefined; key is NOT validated before insertion,
                                                // an invalid key is dropped by the sanitize step

sameListenLog(a, b) -> bool                                        (listen-log.ts:52-58)
  if a and b are the same reference: true
  ka = keys(a) or [], kb = keys(b) or []
  if len differs: false
  return every k in ka: b[k] === a[k]           // undefined and {} compare EQUAL

listeningEntries(r: { createdAt, listenedSec, listenLog? }) -> [(at: datetime, sec)]   (listen-log.ts:60-89)
  total  = (listenedSec finite and > 0) ? listenedSec : 0
  out = []; logged = 0
  for (key, sec) in r.listenLog (if any):
     at = hourKeyDate(key); skip if null or sec not finite or sec <= 0
     out.push((at, sec)); logged += sec
  // The ledger is written together with listenedSec, so it never exceeds it; scale down if a file says otherwise:
  if logged > total and logged > 0:
     k = total / logged; every out.sec *= k; logged = total
  rest = total - logged
  if rest > 0.05: out.push((datetime(createdAt), rest))
  return out                                   // ledger entries in key order, then the createdAt remainder
```

Worked example (unit-test seed): record `createdAt` = 2026-10-10 09:15 local, `listenedSec` 100,
`listenLog {"2026-10-17T08": 30, "2026-10-17T09": 50}` → entries `(10-17 08:00, 30)`, `(10-17 09:00, 50)`,
`(10-10 09:15, 20)`. With `listenedSec` 40 instead → scaled by 0.5: `(08:00, 15)`, `(09:00, 25)`, no remainder.
`addToListenLog({"2026-10-17T09": 50}, "2026-10-17T09", 0.06)` → `{"2026-10-17T09": 50.1}` (round1(50.06)).

### 6.3 Consumers

- History store sanitizes `listenLog` on load/update (`listenLog: v.listenLog === undefined ? base.listenLog :
  sanitizeListenLog(v.listenLog)`) and compares ledgers with `sameListenLog` to detect no-op updates
  (history-store.ts:84, 100-108).
- Analytics: `listeningEntries` for totals.listenedSec, today, daily, heatmap (`[(weekday+6)%7][hour]`), app/voice
  listening and streak (an entry counts toward a streak day only if `sec ≥ 1`) (analytics.ts:76-97, 181-204).

### 6.4 Writer: how core fills the ledger (reader.ts:230-252, 559-630)

Per started session core keeps a `TrackedSession` (max 16, oldest evicted; late reports for evicted sessions are
dropped):

```text
readId, recordSaved, fromCache,
baseListenedSec   // record.listenedSec before this session (cached replay) or 0 (fresh read)
baseCompleted     // record.completed before this session or false
listenedSec = 0, completed = false, maxPositionSec = 0, partial = false,
lastStatus = null, lastWriteAt = 0, written = null
```

Tracking starts right after the engine accepted the session (fresh read: `base* = 0/false`; cached replay: the
record's current `listenedSec`/`completed`, `recordSaved = true`). Sessions are kept in insertion order; when there are
more than 16 the oldest is evicted. The engine's `stopped` event (latest session for that readId, non-cached only) sets
`partial = true` if it carries an error and, when the record is saved, patches `charactersBilled` (+
`error = PARTIAL_READ_ERROR`); `complete` caches the audio (if `library.cacheAudio`; preview-only records get their
alignment chars blanked), prunes, then patches `audioDurationSec = round1(Σ finite chunk durations)`, `cached`,
`charactersBilled` — skipped if the record was removed meanwhile (reader.ts:278-292, 638-662).

On each widget state for a tracked, saved session (untracked or unsaved → only steps "store/relay/progress" of §5.3):

1. `listenedSec = max(listenedSec, state.listenedSec)`.
2. If status ∈ {playing, paused, ended}: `pos = (ended ? durationSec : positionSec)`; `maxPositionSec = max(·, pos)`.
3. If `ended` and not `partial` (no chunk failed): `completed = true`.
4. Write when the status changed or ≥ 2000 ms since the last write:
   - `next = { listenedSec: round1(base + listenedSec), completed: baseCompleted || completed, maxPositionSec: round1(maxPositionSec) }`
   - skip if equal to the last written values;
   - record gone (removed) → nothing (and nothing remembered, so it is retried on the next report);
   - patch `{ listenedSec, completed }`; `delta = next.listenedSec − (written?.listenedSec ?? baseListenedSec)`; if
     `delta > 0` → `listenLog = addToListenLog(record.listenLog, hourKey(now), delta)` (the whole delta goes to the
     **current** hour, even if part of it was listened in the previous hour); `maxPositionSec` patched only if it
     exceeds the record's (absent counts as 0);
   - remember `written = next`, `lastWriteAt = now`; apply via `historyStore.update` (in memory; the disk write is the
     store's 2.5 s / 10 s debounce).
5. On quit/log-off, `persistPlayback()` writes every tracked saved session that has received at least one state.

Note the status-change rule means the first report of every session writes immediately, and so does every
`loading → playing → paused → ended` transition.

---

## 7. Preview/mock API (`src/renderer/src/lib/mock-vox.ts`)

An in-memory fake of the whole UI façade for component previews and UI development without core. Installed only when
the real bridge is absent; exposes `window.__voxMock.emit(channel, payload)` to drive events from a preview
(mock-vox.ts:291-298). Recommended to keep an equivalent "preview core" in Rust for UI work.

### 7.1 Event bus and PRNG

- `on(channel, cb)` adds to a per-channel listener set, returns an unsubscribe; `emit(channel, payload?)` calls every
  listener (mock-vox.ts:18-28).
- Deterministic LCG (mock-vox.ts:31-37): `s = seed >>> 0`; each call `s = (s * 1664525 + 1013904223) >>> 0` (mod 2³²);
  returns `s / 4294967296`. The double-precision product stays below 2⁵³ (2³² × 1664525 + 1013904223 ≈ 7.15e15), so
  this is exactly `s = s.wrapping_mul(1664525).wrapping_add(1013904223)` on a `u32`, result `s as f64 / 4294967296.0`.

### 7.2 Fixtures

- `SNIPPETS` (8 English paragraphs: personal computing, Q3 roadmap update, photosynthesis, a license clause, Prague,
  password reset steps, a lighthouse novel ending, a sleep study result) — mock-vox.ts:39-48.
- `APPS = ['Chrome','Edge','Word','Acrobat','Notion','Outlook','Kindle','Slack']`.
- `SOURCES = ['context-chip','hotkey','browser-extension','selection-popup','clipboard','manual']`.
- `VOICES` = 7 premade voices (George `JBFqnCBsd6RMkjVDRZzb`, Sarah `EXAVITQu4vr4xnSDxMaL`, Brian
  `nPczCjzI2devNBz1zQrb`, Charlotte `XB0fDUnXU5powFXDhCwa`, Daniel `onwK4e9ZLuTAKqWW03F9`, Lily
  `pFZP5JQG7iQjIQuC4Bku`, Jessica `cgSgspJ2msm6clMCkdW9`) with the same descriptions/labels as the core demo list
  (§7.3), plus `{ voiceId:'custom-voice-001', name:'My Narrator', category:'cloned', description:'Instant voice clone',
  labels:{ accent:'Ghanaian', gender:'male' } }`.
- `MODELS` = Flash v2.5, Multilingual v2, Turbo v2.5, v3 (same entries as §7.3, different order).

### 7.3 Core demo fallbacks (main/ipc.ts:200-235) — used in demo mode without a key

Voices (all `category: 'premade'`, labels `{accent, age, gender, use_case}`):

| voiceId | name | description | accent | age | gender | use_case |
|---|---|---|---|---|---|---|
| JBFqnCBsd6RMkjVDRZzb | George | Warm, captivating storyteller | British | middle aged | male | narration |
| EXAVITQu4vr4xnSDxMaL | Sarah | Mature, reassuring, confident | American | young | female | news |
| nPczCjzI2devNBz1zQrb | Brian | Deep, resonant and comforting | American | middle aged | male | narration |
| XB0fDUnXU5powFXDhCwa | Charlotte | Relaxed and soothing | Swedish | young | female | characters |
| onwK4e9ZLuTAKqWW03F9 | Daniel | Steady broadcaster | British | middle aged | male | news |
| pFZP5JQG7iQjIQuC4Bku | Lily | Velvety actress | British | middle aged | female | narration |
| cgSgspJ2msm6clMCkdW9 | Jessica | Playful, bright, warm | American | young | female | conversational |
| TX3LPaxmHKxFdv7VOQHJ | Liam | Energetic, social media creator | American | young | male | social media |

Models (all `languages: [{ id:'en', name:'English' }]`):

| modelId | name | description | maxChars | costFactor |
|---|---|---|---|---|
| eleven_flash_v2_5 | Eleven Flash v2.5 | Ultra low latency, 32 languages | 40000 | 0.5 |
| eleven_turbo_v2_5 | Eleven Turbo v2.5 | Balanced quality and speed | 40000 | 0.5 |
| eleven_multilingual_v2 | Eleven Multilingual v2 | Most life-like, rich emotion | 10000 | 1 |
| eleven_v3 | Eleven v3 | Most expressive | 5000 | 1 |

Account: `{ tier:'creator', characterCount:61240, characterLimit:100000, nextResetUnix: now_s + 12×86400, status:'active' }`.

### 7.4 Mock records generator `makeRecords(now, 64)` (mock-vox.ts:68-103)

`rand = prng(7)`; for `i` in `0..count`, **in this exact rand() order**:

1. `text = SNIPPETS[i % 8]`
2. `daysAgo = floor(rand()^1.6 × 45)`
3. `hour = rand() < 0.5 ? 8 + floor(rand() × 3) : 19 + floor(rand() × 4)` (morning 8–10 or evening 19–22)
4. `d = now − daysAgo × 86 400 000 ms` (plain ms, not calendar days), then set its local time to
   `hour : floor(rand() × 60) : 00.000`
5. `voice = VOICES[floor(rand() × 4)]`
6. `words = text.split(/\s+/).length`, `audio = round(len / 15)`, `completed = rand() > 0.2`
7. record: `id 'mock-<i>'`, `createdAt = min(d, now − i × 60 000)`, `textTruncated false`, `charCount len`,
   `wordCount words`, voice id/name, `modelId 'eleven_flash_v2_5'`, `source = SOURCES[floor(rand() × 6)]`,
   `sourceApp = APPS[floor(rand() × 8)]`, `sourceTitle 'Untitled document'`, `audioDurationSec audio`,
   `listenedSec = completed ? audio : round(audio × rand())` (**`rand()` is consumed only when not completed**),
   `completed`, `cached = rand() > 0.3`, `charactersBilled = len`, `favorite = rand() > 0.85`.

So each record consumes 10 `rand()` calls (11 when not completed): daysAgo, morning/evening, hour offset, minute,
voice, completed, source, sourceApp, [listened], cached, favorite. Sorted newest first (stable). No
`listenLog`/`maxPositionSec`.

### 7.5 Mock analytics `mockSummary(range)` (mock-vox.ts:110-146)

`days` = 7/30/90/365 (`'all'` → 365); `rand = prng(days)`. Daily, oldest → newest: `active = rand() > 0.25`;
`listenedSec = active ? round(300 + rand() × 3600) : 0`; `reads = active ? 1 + floor(rand() × 9) : 0`;
`words = round(listenedSec × 2.6)`; `date` = local `YYYY-MM-DD`. Heatmap 7×24: `peak = 1` for hours 7–10 and 19–23,
`0.35` for 11–18, `0.04` otherwise; value `round(rand() × peak × (weekday ≥ 5 ? 1300 : 900))`. Totals from daily sums;
`chars = charactersBilled = words × 6`, `audioSec = listenedSec × 1.08`, `completedReads = round(reads × 0.82)`;
`today = { listenedSec: 4h20m = 15600, reads: 9, words: 11820 }`; `streak = { current: 12, longest: 23 }`;
`topApps` = first 6 APPS with `reads 60 − 9i`, `listenedSec (6 − i) × 5400`; `topVoices` = first 4 voices with
`reads 80 − 17i`, `listenedSec (4 − i) × 7200`; `sources` = SOURCES with `reads 70 − 11i`;
`avgWordsPerRead = round(words / max(1, reads))`; `avgListenWpm 168`; `readingTimeEquivalentSec = words / 238 × 60`.
`audioSec` and `readingTimeEquivalentSec` are **not** rounded here (fractional values reach the UI); the daily values
draw `rand()` in the order active → listened (only if active) → reads (only if active), then 168 heatmap draws
(weekday-major). The `records` argument is ignored.

### 7.6 Mock behavior (mock-vox.ts:148-289)

- Settings start as `DEFAULT_SETTINGS` + `hasApiKey: true`, `onboarded: true`. `update` deep-merges **without**
  sanitizing (plain objects merge, everything else — including `undefined` — replaces), emits `settings:changed`.
  `setApiKey(key)`: ok iff `key.trim().length > 8`; on failure error `That key was rejected by ElevenLabs.` and
  `account: null`; returns `account` on success. `clearApiKey` sets `hasApiKey: false`. Neither key call emits
  `settings:changed` (unlike the real core).
- Account `{ tier:'creator', characterCount:61234, characterLimit:100000, nextResetUnix: now+12 d }`.
- `readText`/`readClipboard` → `{ ok:true, readId:'mock-new' }`; `replay` → `{ ok:true, readId:'mock-0' }`.
- Player starts `playing`, session `mock-session`, read `mock-0`, title
  `Blinding Lights — The quiet revolution in personal computing`, voice George, source Chrome, position 63 / duration
  263 s, not estimate, buffered 0.8, rate 1, volume 1, charIndex 46, totalChars = SNIPPETS[0].length, listened 63.
  `command`: `toggle` flips playing/paused, `stop` → idle; every command emits `player:state`.
  `widget.reportState(s)` replaces and emits.
- History: list filters by lower-case substring of `text` only, favorites, source; paging default 50;
  `toggleFavorite` mutates; `remove`/`clear` no-ops; `exportJson` → `C:\Users\you\Documents\vox-history.json`.
- Cache stats `{ reads:41, files:96, bytes:187 400 000 }`; clear → zeros.
- App info: `Vox`, `0.1.0`, electron `44.7.0`, `win32`, not packaged, userData `C:\Users\you\AppData\Roaming\Vox`,
  extension `C:\Program Files\Vox\resources\browser-extension`, port 47823, running, hook available, explorer menu not
  installed, demo true. All send-style app/widget/chip calls are no-ops (except `widget.reportState`); subscriptions
  work and are driven with `__voxMock.emit`. The mock's `widget.onHover` does **not** coerce to boolean.
- Mock records come from `makeRecords(Date.now(), 64)` (64 records), generated once per mock instance.

---

## 8. Inconsistencies and decisions for the port

1. **Two word-count rules.** Reader: Unicode regex `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu` (reader.ts:137-139). History
   sanitize fallback (only when `wordCount` is missing): whitespace split (history-store.ts:43-46). Use the Unicode rule
   everywhere.
2. **Cache limit range.** Core clamps `cacheLimitMB` to 50..10 000; the Settings slider offers 50..5000. A
   hand-edited 8000 is valid but unrepresentable in the UI.
3. **Export filename date** uses the UTC date while every other date in the app is local.
4. **`history:list` limits.** IPC clamps `limit` to 1..1000, the store to 1..500; `search` is cut to 500 chars before
   trimming.
5. **`hourKeyDate` accepts day 31 for every month** and rolls over (JS `Date`). A Rust implementation using strict
   calendar validation would reject keys JS accepted. Only reachable via hand-edited files — pick one behavior
   deliberately (recommend: reject invalid dates; for DST-gap hours, take the later valid instant).
6. **`sanitizeListenLog` keeps values that round to 0.0** (positivity is tested before rounding).
7. **`parsePlayerState` does not clamp `rate`/`volume`** and does not bound `sessionId`/`readId` length.
8. **`/health` includes `playing`** although SPEC.md documents `{ app, version, ok }` only.
9. **An invalid settings value in a patch is silently ignored** (keeps the current value); UI must adopt the returned
   settings rather than its own optimistic copy.
10. **Media-keys setting needs a restart** only because of a Chromium startup switch; a native SMTC integration can
    apply it live (then drop the restart toast).
11. Several workarounds exist purely because of Chromium/Electron (drag-region lag, ~38 DIP minimum height of
    transparent windows, `sessionData` folder collision with `Cache`, `Error invoking remote method` prefix,
    `disable-lcd-text`). Re-check each against the chosen Rust UI stack instead of porting it blindly; the
    *requirements* behind them (no lost clicks, no focus steal, chip clickable anywhere, grayscale text for dot fonts)
    still hold.
12. **Bridge vs IPC command validation differ.** IPC silently drops an invalid `PlayerCommand` and ignores a bad
    `amount`; the bridge answers HTTP 400 (with a message) for both. Keep both behaviors or unify deliberately.
13. **Bridge guards are broader than SPEC.md says.** `Host` and `Origin` checks apply to every request including
    `GET /health`, and the Origin guard also rejects `null` and `file:` origins (SPEC.md mentions only http/https and
    "non-GET").
14. **Widget readiness is one-shot.** After a widget UI crash/reload nothing waits for it again, so a read started
    while the reloaded widget is still loading can lose its `tts:session-start`. A native in-process widget removes the
    problem; a webview port should re-arm the latch on reload.
15. **`hourKeyDate` maps years 0–99 to 1900–1999** (JS `Date` constructor quirk). Unreachable except via hand-edited
    files; a Rust port can reject such keys.
16. **JS `Math.round` vs Rust `round`** differ for negative halves (`customX = -100.5` → JS −100, Rust −101). Use a
    `js_round` helper wherever this spec says "rounded".
17. **Unsafe UTF-16 cuts.** The 160-char preview (`text.slice(0,160)`), `previewTitle`'s 64-char cut, the 500-char
    search cut and the IPC string caps (`optStr`) can split a surrogate pair; only the reader's pre-cut and `maxChars`
    truncation are surrogate-safe. A Rust port that cuts on `char` boundaries will differ by one unit in those rare
    cases (harmless; recommended).
18. **`sourceTitle` caps stack**: IPC cuts to 300 and the bridge trims and cuts to 300, then the reader trims and cuts to
    200 for the record (the foreground window title is already ≤ 300). Only the 200 matters for stored data.
19. **The UI's exe-name validation is stricter than core's** (`normalizeExeName` vs `normalizeExcludedApps`), and the
    UI's accelerator key does not trim parts while core's does.
20. **`AppInfo.serverPort`** after a failed bridge start reports the requested port with `serverRunning: false`; when
    the bridge was never created it falls back to `settings.server.port`.
21. **A user drag snaps back into the work area**: the drag's settings write re-places the visible widget through the
    clamped `remember` rule (§1.4 #5). Probably intended (keeps the widget reachable), but it is implicit.
22. **Read invocations can take up to 5 s** on the first read after launch (they await widget readiness before
    returning `ReadResult`); the bridge's 15 s `/read` timeout covers it.
