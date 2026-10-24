# Port spec: Selection subsystem (global hook, selection gestures, clipboard capture, foreground app)

Source of truth: `src/main/selection/*` in the Electron app (9 files, all read in full). Citations are
`file:line` relative to `src/main/selection/` unless a longer path is given. Code outside this folder is cited only where
it shows how this subsystem is driven or consumed.

What this subsystem does: it watches global mouse and keyboard input for gestures that probably selected text
somewhere ("hints"). It shows a small chip (a "Read with Vox" pill next to the native right-click menu, or an optional
PopClip-style "Read" pill right after a selection). When the user activates a chip or presses the read-selection
hotkey, it copies the selection out of the foreground app through a clipboard round-trip (synthetic copy chord, poll,
restore the user's clipboard) without reading passwords or private text, and reports which app the text came from.

---

## 0. Files

| File | Role |
|---|---|
| `service.ts` | Orchestrator. Owns the hook lifecycle, feeds the gesture machine, applies its decisions (chip show/hide/activate), runs the clipboard round-trip, schedules clipboard restores and late-copy watches, emits events. |
| `gestures.ts` | Pure state machine with no I/O. Time advances only through event timestamps and `tick(now)`. |
| `keycodes.ts` | Hook key codes (libuiohook "VC_*" codes) and the modifier / navigation / neutral key sets. |
| `keys.ts` | Picks the copy method per app (Ctrl+C, Ctrl+Insert, none). Sends synthetic chords. Detects and waits on held modifiers. |
| `clipboard.ts` | Clipboard snapshot and restore, sequence numbers, text polling, privacy-marker detection. |
| `sensitive.ts` | Password-manager deny list, privacy marker format names, flag decoding. Pure data. |
| `foreground.ts` | Maps a window (the foreground one or the one under a point) to process, friendly app name, title and class. Also: text-cursor evidence, the own-overlay test, the fullscreen test. |
| `win32.ts` | FFI bindings to user32, kernel32 and shell32 (the optional ones). Every Win32 call lives here. |
| `hook.ts` | Lazy loader for the global input-hook library (uiohook-napi = libuiohook). |

---

## 1. Data flow

```
 WH_MOUSE_LL / WH_KEYBOARD_LL (hook thread)
        │  mousedown/up/move/wheel, keydown/keyup  (physical px, VC keycodes, modifier flags)
        ▼
 SelectionService.feed()  ──► ModifierTracker (keydown/keyup)
        │  (mousemove only while machine.wantsMouseMove; keydown skipped while a capture runs)
        ▼
 GestureMachine.handle(ev) ──queries──► targetAt / focusedTarget / ownOverlayAt / textCursor /
        │                               isInsideChip / chipsSuppressed  (Win32, see §11-12)
        ▼ decisions[]
 SelectionService.apply()
   show-context-chip / show-selection-chip ──► emit 'chip-show' {x,y (DIP), mode}
   hide-chip                                ──► emit 'chip-hide'
   activate-chip                            ──► activate(mode) ──► readSelection(source)
   (then re-arm the single deadline timer for hint expiry / chip auto-hide)

 readSelection(source) ─► capture():
   foreground window → password manager? refuse
   copy method (Ctrl+C | Ctrl+Insert | read clipboard as-is)
   wait modifiers released (≤700 ms) → settle 40/120 ms → snapshot clipboard → seq number
   send chord → poll clipboard (15 ms, ≤450 ms, +≤1500 ms settle) [→ Escape + retry once for context chip]
   privacy markers? refuse
   settleClipboard(): schedule restore (150 ms / now) or watch for a late copy (1.5 s)
 ─► emit 'read' | 'no-selection' | 'notice'
```

---

## 2. External interface

### 2.1 Settings consumed

All under `settings.triggers`, re-applied live through `applySettings` (`service.ts:215-221`). Defaults are from
`src/shared/defaults.ts:30-45`.

| Field | Type | Default | Effect here |
|---|---|---|---|
| `rightClickChip` | bool | `true` | Enables the context chip (machine `contextChip`). Also one of the two reasons the hook runs. |
| `selectionPopup` | bool | `false` | Enables the selection chip (machine `selectionChip`). Also one of the two reasons the hook runs. |
| `excludedApps` | string[] | `[]` | Executable names where no hints or chips happen. Normalized, see §7.2. |
| `restoreClipboard` | bool | `true` | Puts the user's previous clipboard back after a successful text capture. A missing value is treated as `true` (`?? true`). |

The shell calls `applySettings` on **every** settings change, whatever field changed (`src/main/index.ts:295-304`,
wrapped in try/catch). Reconfiguring the machine with unchanged chip flags has no effect. `restoreClipboard` is read
**live** at the moment a restore decision is made (`service.ts:453`, `service.ts:558`), not when the capture starts, so
toggling it mid-capture affects that capture. Before the first `start()` the settings are `null`, which counts as
"no chip trigger enabled" and `restoreClipboard = true`.

### 2.2 Commands (inputs)

| Command | Behavior | Source |
|---|---|---|
| `start(settings)` | `started=true`, `stopped=false`, applies settings, starts native loading once (`initNative`). The shell never calls it in its screenshot mode, because "the global hook would let the user's own clicks disturb the screenshot run" (`src/main/index.ts:358-359`). | `service.ts:208-213` |
| `applySettings(settings)` | Rebuilds the excluded set, reconfigures the machine (which may emit hide-chip `disabled`), then starts or stops the hook to match. | `service.ts:215-221` |
| `stop()` | `started=false`, `stopped=true`. Flags a running capture to abort (no more keys, no more polling). Stops the hook, resets the machine, clears the deadline timer, starts flushing the pending clipboard job (not awaited). | `service.ts:224-232` |
| `flush()` | Awaits the in-flight capture (errors ignored), then `flushRestore()`. The shell calls `stop()`, then `flush()` inside a 3 s shutdown budget (`src/main/index.ts:398-417`). | `service.ts:241-245` |
| `whenReady()` | Resolves after the native modules load (or fail). Returns `hookAvailable`. If `start()` has not run yet, it triggers the module loads itself. | `service.ts:235-238` |
| `hookAvailable` | `hook module loaded && no hook start error`. Independent of whether the hook is running right now. | `service.ts:247-249` |
| `busy` | A capture is in progress. | `service.ts:252-254` |
| `setChipBounds(rect \| null)` | The chip window's actual bounds in DIP while visible, `null` when hidden or hiding. Used for the hook-side click test. | `service.ts:259-261` |
| `chipActivated()` | The chip UI got a left `pointerdown`. Calls `activate(machine.chipMode ?? lastChipMode)` (deduped, §6.3). `lastChipMode` is the mode of the last `chip-show` (initially `context`). It is needed because the hook-side activation usually cleared `machine.chip` already. | `service.ts:264-266` |
| `chipDismissed()` | The chip UI got a non-left `pointerdown`. Calls `machine.hideChip('dismissed')`, re-arms the deadline, emits `chip-hide`. | `service.ts:268-272` |
| `readSelection(source, {viaContextMenu?})` | Full capture, then emits events (§8). Ignored with no event while a capture already runs (debug log `capture already running, ignoring trigger from <source>`). `viaContextMenu` defaults to `source === 'context-chip'`. | `service.ts:280-311` |
| `captureSelection({viaContextMenu})` | Same capture, but returns `{text, foreground}` or `null` and emits nothing. No late-copy handler, so a late copy is only undone. Not used by the app today. | `service.ts:318-321` |

Callers in the shell: the global "read selection" hotkey (default `Ctrl+Alt+R`) calls `readSelection('hotkey')`
(`src/main/index.ts:158-160`). The chip renderer's left `pointerdown` hides the chip window and calls `chipActivated()`.
Any other button hides it and calls `chipDismissed()` (`src/main/ipc.ts:438-455`, `src/renderer/src/chip/Chip.tsx:72-73`).
The chip UI reacts on press, not click, because some context menus swallow the click that dismisses them. It ignores
presses entirely while it plays its exit animation ("inert"), and it suppresses its own context menu
(`Chip.tsx:65-82`). A press anywhere in the chip window counts as a press on the chip.

Other consumers of this folder: `getForegroundInfo()` labels browser-extension reads (the shell keeps it only when the
foreground process is a known browser, `src/main/index.ts:213-245`), and `altGrCharacter()` serves the hotkey
registration (§12.1).

### 2.3 Events (outputs)

| Event | Payload | When | Shell's reaction (outside this subsystem) |
|---|---|---|---|
| `read` | `{ text: string, source: ReadSource, foreground: ForegroundInfo \| null }` | Text was captured (trimmed, non-empty). Also fires for a late copy after a `no-selection` for the same trigger: typically within about 2 s, at worst 1.5 s (watch) + 0.45 s (poll) + 1.5 s (settle) ≈ 3.45 s after it. | Starts TTS with `sourceApp=appName`, `sourceProcess=processName`, `sourceTitle=title`. |
| `no-selection` | `source` | The capture found no text. It can be followed immediately by a `notice` (loss notice) when the copy replaced the clipboard with non-text data. | Toast `info`: "Select some text first". |
| `notice` | `{ kind: 'info' \| 'error', message }` | A refusal (password manager, secret, private text), or the previous clipboard could not be fully restored. | Toast. |
| `chip-show` | `{ x: int, y: int, mode: 'context' \| 'selection' }`, where x,y are the cursor in DIP, rounded | The machine decided to show a chip. | Places and shows the chip window (context label "Read with Vox", selection label "Read"). |
| `chip-hide` | none | Any hide decision, any activation, any dismissal. | Sets the chip bounds to null at once, makes the window click-through, lets the renderer play its 120 ms exit animation, then hides the window after `CHIP_EXIT_MS = 140` (`src/main/windows.ts:767-804`). If the window is already hidden or a hide is already pending, it only nulls the bounds and tells the renderer. |
| `hook-status` | `bool` | Once after native init, and again whenever `hookAvailable` changes. Never before `start()`. In practice at most twice (`true`, then `false` if a later start fails, since the failure is sticky). It reports `true` even when the hook is not running because both chip triggers are off. | Logs only: `native input hook available`, or `native input hook unavailable: chips are disabled, hotkeys/extension/tray still work` (`src/main/index.ts:354-357`). Settings reads `whenReady()` raced against a 2 s timeout that falls back to `hookAvailable` (`src/main/ipc.ts:272-278`). |

`ReadSource` values produced here: `'hotkey'`, `'context-chip'`, `'selection-popup'` (`src/shared/types.ts:14-23`).
Activation of the context chip maps to `'context-chip'` and the selection chip to `'selection-popup'`
(`service.ts:794`).

Every emit is wrapped so that a throwing listener is logged and never propagates (`service.ts:857-865`).

### 2.4 Notice copy (exact strings, `APP_NAME = "Vox"`)

All have `kind: 'info'` (`service.ts:103-109`). This subsystem never produces `kind: 'error'`, although the type
allows it.

| Key | Message |
|---|---|
| `secret` | `That looks like a password, so Vox won't read it` |
| `privateText` | `That text is marked private, so Vox won't read it` |
| `passwordManager` | `Vox doesn't read from password managers` |
| `clipboardTooLarge` | `Couldn't restore your previous clipboard (it was too large)` |
| `clipboardUnreadable` | `Couldn't restore your previous clipboard` |

`refusal(privacy)` picks `secret` for `'secret'` and `privateText` otherwise (`service.ts:143-145`).
`lossNotice(snapshot)` (`service.ts:148-153`):
- If the snapshot failed: `clipboardTooLarge` when some formats were skipped, else `clipboardUnreadable`.
- If it did not fail: `clipboardTooLarge` when some formats were skipped, else no notice.

### 2.5 Data shapes

```ts
ForegroundInfo {            // foreground.ts:13-21
  processName: string       // lower-case exe basename, e.g. "chrome.exe"; '' when the process can't be queried
  appName: string           // friendly name, e.g. "Chrome"
  title: string             // window title, whitespace-collapsed, trimmed, max 300 chars
  pid: number
}
WindowInfo extends ForegroundInfo {   // foreground.ts:87-92 (internal)
  windowId: string          // top-level HWND as a decimal string; stable while the window lives
  className: string         // window class, e.g. "ConsoleWindowClass" ('' when unknown)
}
GestureTarget { id: string /* windowId */, blocked: boolean }   // gestures.ts:53-58
```

---

## 3. Native dependencies, availability and degradation

The original loads two native modules lazily. Neither load ever throws; a failure is recorded.

- **Input hook** (uiohook-napi 1.5.5, a wrapper over libuiohook) (`hook.ts:13-30`). It provides global mouse and keyboard
  events and a fallback key injector. Load errors are kept in `hookLoadError()`.
- **Win32 bindings** (koffi 3.3.2) (`win32.ts:240-266`). Only on `win32`; elsewhere it is null with "unsupported
  platform". `win32()` is a synchronous accessor that starts loading on first use and returns null until loaded.
  `shell32!SHQueryUserNotificationState` is optional: if it can't bind, fullscreen detection is off
  (`win32.ts:124-130`).

Native init (`service.ts:640-646`): load both, log what's missing (`global hook unavailable, chips disabled: <error>`,
`koffi unavailable, foreground app info disabled`), `syncHook()`, `reportStatus()`.

Starting the hook is **synchronous**: the wrapper creates the hook thread and blocks until that thread has either
installed both LL hooks or failed. On Windows the only start failure is `SetWindowsHookEx` returning NULL, which surfaces
as the exception message `Failed to register low level windows hook.` (`node_modules/uiohook-napi/src/lib/addon.c:180-240`).

**When the hook runs** (`service.ts:648-653`): `started && (rightClickChip || selectionPopup || win32 == null)`. Without
Win32 bindings the hook is also the only source of held-modifier state. A native port always has Win32, so in
practice the hook runs only while at least one chip trigger is enabled.

**syncHook** (`service.ts:655-682`):
- If the hook module isn't loaded, do nothing.
- `want = wantHook() && hookStartError == null`.
- If `want` and not running: attach the six listeners (mousedown, mouseup, mousemove, wheel, keydown, keyup) and start.
  On an exception: detach, store `hookStartError` (sticky, never retried while the process lives), log
  `global hook failed to start: <message>`. Then `reportStatus()` (in both the success and the failure case).
- If not `want` and running: stop (errors logged as `global hook failed to stop`), detach, `running=false`, clear the
  ModifierTracker, apply `machine.reset()` (hides the chip and clears the hint).
- Otherwise nothing (no status report).

**reportStatus** (`service.ts:684-690`): only after `start()` has run. Emits `hook-status(hookAvailable)` only when the
value differs from the last one reported.

Degradation rules (from `docs/SPEC.md:180-181` and the code):
- No hook: there are no chips, but the hotkey capture still works.
- No Win32: foreground info is `null`, there is no sequence number (the fallback clipboard path in §8.3, steps 11-13 applies), keys
  are sent through the hook library, and modifier state comes from the hook.

---

## 4. Global input hook (what the original receives)

libuiohook on Windows installs `SetWindowsHookEx(WH_KEYBOARD_LL)` and `SetWindowsHookEx(WH_MOUSE_LL)` on its own
thread. That thread runs at `THREAD_PRIORITY_TIME_CRITICAL` (`uiohook-napi/src/lib/uiohook_worker.c:84-90`). The hook
never consumes an event (`CallNextHookEx` always). Events reach JS through an unbounded, non-blocking thread-safe
queue (`napi_tsfn_nonblocking`, `addon.c:11-27`), so the hook procedure never waits on app logic. Injected input is
**not** filtered: Vox's own synthetic Ctrl+C and Escape come back through the hook (see the keydown rule in §4.1 for
how that is handled).

The same thread also runs a **UIPI watch** that only affects the modifier flags (`libuiohook/src/windows/input_hook.c:92-131,
612-662`): `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` and `SetWinEventHook(EVENT_SYSTEM_MINIMIZEEND)` (both
`WINEVENT_OUTOFCONTEXT`), plus an 83 ms `SetTimer` that polls `GetForegroundWindow`. On every foreground change it posts
a registered message (`RegisterWindowMessage("UIOHOOK_UIPI_TEST")`) to the new foreground window. If `PostMessage`
fails with `ERROR_ACCESS_DENIED` (an elevated window while Vox is not elevated), it marks the state "blocked by UIPI".
The next hook event then re-seeds the whole modifier mask from `GetAsyncKeyState`. The reason (libuiohook's
assumption, consistent with UIPI): a non-elevated LL hook misses key transitions made while the elevated window is in
front, so the tracked mask would otherwise go stale (a stuck Shift would turn every later click into a shift+click).
Expect the same blindness for gestures made inside elevated apps (no hints, no chips there). A capture from an elevated
app fails regardless, because `SendInput` / `keybd_event` into a higher-integrity window is silently dropped (neither
the return value nor `GetLastError` reports it), so the user just gets "Select some text first".

### 4.1 Event fields used

`feed()` (`service.ts:727-761`) normalizes every event into a `GestureEvent` (`gestures.ts:9-26`):

| Field | Value |
|---|---|
| `type` | `mousedown`, `mouseup`, `mousemove`, `wheel`, `keydown`. `keyup` is never fed to the machine; it only updates the ModifierTracker. `keydown` comes from both `WM_KEYDOWN` and `WM_SYSKEYDOWN` (Alt combinations and F10 arrive as SYS), `keyup` from `WM_KEYUP` / `WM_SYSKEYUP`. `wheel` comes from both `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL` (tilt / horizontal), and is dropped by libuiohook in the rare case that `SystemParametersInfo(SPI_GETWHEELSCROLLLINES/CHARS)` fails. |
| `x`, `y` | `MSLLHOOKSTRUCT.pt`, in **physical** per-monitor-aware screen pixels. libuiohook casts to int16; a native port should use i32. Key events use 0. |
| `button` | 1 left, 2 right, 3 middle, 4 XButton1, 5 XButton2. 0 for wheel and keys. |
| `clicks` | libuiohook's multi-click counter (below). 0 for keys. |
| `time` | **Wall clock at processing time** (`Date.now()`), not the hook's timestamp. All timers use the same clock. A port should use a monotonic millisecond clock. |
| `shift`, `ctrl`, `alt`, `meta` | libuiohook's modifier mask, tracked from LL keyboard events by the side-specific VKs only (`VK_L/RSHIFT`, `VK_L/RCONTROL`, `VK_L/RMENU`, `VK_L/RWIN`); each flag is "left OR right". The mask is updated **before** the event is dispatched, so a modifier's own keydown already carries its flag. It is seeded from `GetAsyncKeyState` when the hook starts and re-seeded after a UIPI-blocked foreground (above). It is zeroed right after a keydown of Delete (VC 0x0E53, the extended Delete key only) while Ctrl and Alt are set: Ctrl+Alt+Del switches to the secure desktop, and the hook presumably never sees those key-ups (`input_hook.c:197-236`). AltGr arrives as LCtrl+RAlt, so it sets both `ctrl` and `alt`. |
| `keycode` | libuiohook VC code (§4.3). 0 for mouse events. |

**libuiohook click counting** (`node_modules/uiohook-napi/libuiohook/src/windows/input_hook.c:304-470`):
- On a button press: if `button == previous press button && (t - previous press time) <= GetDoubleClickTime()`, then
  `count++`. Otherwise `count = 1` and the remembered button becomes this button. The press time is remembered on
  every press. Position is ignored.
- A wheel event sets `count = 1` and the remembered button to "none".
- A mouse move that actually moved, more than the double-click time after the last press, sets `count = 0`.
- A release more than the double-click time after its press sets `count = 0`.
- The last two rules cannot change the next press's count: that press is necessarily more than the double-click time
  after the remembered one, so it restarts at 1 anyway. Only the wheel rule matters (§14).

libuiohook dispatches a mouse move only when the point differs from the **last press point** (`input_hook.c:407`). A
move back onto the exact press pixel is never reported, which is harmless for drag detection.

**Moves**: libuiohook's "dragged" events (moved with a button held) are re-labelled as plain `mousemove` by the
wrapper. The service only forwards `mousemove` to the machine while `machine.wantsMouseMove` is true (`service.ts:712-714`,
`gestures.ts:153-155`). That is the hot-path filter, and a mousemove with no resulting decision is not applied either
(`service.ts:756`).

**keydown** (`service.ts:717-721`): it always updates the ModifierTracker. It is fed to the machine only when **no
capture is running** (`!capturing`), so Vox's own synthetic Ctrl+C or Escape can never clear the selection hint or hide a
chip. The user's own keydowns during a capture (up to about 4 s, §8.10) are ignored by the machine too. **keyup**
(`service.ts:723-725`) only updates the ModifierTracker.

Event construction (`service.ts:727-730`): mouse events pass `x`, `y`, `button` (`Number(e.button) || 0`; wheel events
have no button field, so 0) and `clicks`. Key events pass `x = y = button = clicks = 0` and the keycode.

Exceptions in gesture handling are caught and logged at most once per 10 s (`warnThrottled`, `service.ts:867-872`).
Nothing may propagate into the hook callback.

### 4.2 Hook event timing note
Auto-repeat of a held key produces repeated `keydown` events. The machine ignores modifier keydowns entirely and reports
`hint-set` only on transitions (§5.5.8).

### 4.3 Key codes (`keycodes.ts`) and their Win32 VK equivalents

libuiohook maps the LL hook's `vkCode` (plus `LLKHF_EXTENDED`) to VC codes through a table
(`libuiohook/src/windows/input_helper.c:293-333`). Specifically, for VK_PRIOR, NEXT, END, HOME, LEFT, UP, RIGHT, DOWN,
INSERT and DELETE **without** the extended flag (the numpad versions with NumLock off), it ORs `0xEE00` into the code.
A native port can classify on VK codes directly. The equivalences below reproduce the original sets.

`Key` constants (`keycodes.ts:4-35`):

| Name | VC | VK equivalent |
|---|---|---|
| Escape | 0x0001 | VK_ESCAPE 0x1B |
| Tab / Enter / Space / Backspace | 0x000F / 0x001C / 0x0039 / 0x000E | (defined, unused by the logic) |
| A | 0x001E | 'A' 0x41 (VK, so layout-dependent like apps' own Ctrl+A) |
| C | 0x002E | 'C' 0x43 |
| CapsLock / NumLock / ScrollLock | 0x003A / 0x0045 / 0x0046 | VK_CAPITAL 0x14 / VK_NUMLOCK 0x90 / VK_SCROLL 0x91 |
| PrintScreen | 0x0E37 | VK_SNAPSHOT 0x2C |
| Insert | 0x0E52 | VK_INSERT 0x2D **with** extended flag (numpad Ins = 0xEE52, which does *not* match) |
| Delete | 0x0E53 | (unused) |
| Home/End/PageUp/PageDown | 0x0E47/0x0E4F/0x0E49/0x0E51 | VK_HOME 0x24 / VK_END 0x23 / VK_PRIOR 0x21 / VK_NEXT 0x22 (extended) |
| ArrowUp/Down/Left/Right | 0xE048/0xE050/0xE04B/0xE04D | VK_UP 0x26 / VK_DOWN 0x28 / VK_LEFT 0x25 / VK_RIGHT 0x27 (extended) |
| ContextMenu | 0x0E5D | VK_APPS 0x5D |
| Ctrl / CtrlRight | 0x001D / 0x0E1D | VK_LCONTROL 0xA2 / VK_RCONTROL 0xA3 |
| Alt / AltRight | 0x0038 / 0x0E38 | VK_LMENU 0xA4 / VK_RMENU 0xA5 |
| Shift / ShiftRight | 0x002A / 0x0036 | VK_LSHIFT 0xA0 / VK_RSHIFT 0xA1 |
| Meta / MetaRight | 0x0E5B / 0x0E5C | VK_LWIN 0x5B / VK_RWIN 0x5C |

**MODIFIER_KEYS** (`keycodes.ts:37-46`): the 8 side-specific modifiers above. libuiohook's table also maps the
**generic** VKs `VK_SHIFT 0x10`, `VK_CONTROL 0x11` and `VK_MENU 0x12` to the left-side codes (`input_helper.c` table
rows 16-18), so if a generic modifier VK ever reaches the hook (some injectors send them) it is still a modifier key for
the machine. A VK-based port must include 0x10-0x12 in its modifier set. (They do not set the `shift/ctrl/alt` flags,
which track only the side-specific VKs.)

**NAVIGATION_KEYS** (`keycodes.ts:49-66`): arrows, Home, End, PageUp, PageDown, plus their numpad (non-extended)
variants `0xEE48, 0xEE50, 0xEE4B, 0xEE4D, 0xEE47, 0xEE4F, 0xEE49, 0xEE51`. In VK terms: VK_UP, VK_DOWN, VK_LEFT,
VK_RIGHT, VK_HOME, VK_END, VK_PRIOR, VK_NEXT, **regardless of the extended flag**. With NumLock on, the numpad sends
VK_NUMPAD0-9, which are not navigation keys.

**NEUTRAL_KEYS** (`keycodes.ts:72-95`) are keys that never change a text selection: Escape, CapsLock, NumLock,
ScrollLock, PrintScreen, ContextMenu, VolumeMute 0xE020, VolumeDown 0xE02E, VolumeUp 0xE030, MediaPlay 0xE022,
MediaStop 0xE024, MediaPrevious 0xE010, MediaNext 0xE019, MediaSelect 0xE06D, MediaEject 0xE02C, Power 0xE05E,
Sleep 0xE05F, Wake 0xE063, AppMail 0xE06C, AppCalculator 0xE021, AppMusic 0xE03C, AppPictures 0xE064. Browser keys
(back, refresh, and so on) are deliberately **not** neutral, because they replace the page and with it the selection.

The neutral set as libuiohook actually produces it from VKs on Windows:

| VK | → VC (neutral?) |
|---|---|
| VK_ESCAPE 0x1B, VK_CAPITAL 0x14, VK_NUMLOCK 0x90, VK_SCROLL 0x91, VK_SNAPSHOT 0x2C, VK_APPS 0x5D | yes |
| VK_VOLUME_MUTE 0xAD, VK_VOLUME_DOWN 0xAE, VK_VOLUME_UP 0xAF | yes |
| VK_MEDIA_NEXT_TRACK 0xB0, VK_MEDIA_PREV_TRACK 0xB1, VK_MEDIA_STOP 0xB2, VK_MEDIA_PLAY_PAUSE 0xB3, VK_LAUNCH_MEDIA_SELECT 0xB5 | yes |
| VK_SLEEP 0x5F | yes |
| VK_LAUNCH_APP1 0xB6 → AppMail, VK_LAUNCH_APP2 0xB7 → AppCalculator | yes |
| **VK_PROCESSKEY 0xE5** (IME composition) → AppPictures; OEM 0xE6 → AppMusic | yes (an accident of libuiohook's table: IME keystrokes do not clear the hint) |
| VK_LAUNCH_MAIL 0xB4 → VC_UNDEFINED (0) | **no** (it clears the hint, despite the intent) |
| MediaEject, Power, Wake | have no VK mapping, so they are never produced on Windows |

---

## 5. Gesture state machine (`gestures.ts`)

Pure logic: no Electron or native imports. The service feeds it normalized events in physical pixels with millisecond
timestamps and applies the decisions it returns. Time only advances through `ev.time` and `tick(now)`
(`gestures.ts:1-3`). Port it as a pure, unit-testable Rust struct.

### 5.1 Constants (`GESTURE_TIMING`, `gestures.ts:89-96`)

| Constant | Value | Meaning |
|---|---|---|
| `dragThresholdPx` | 6 | Euclidean distance (physical px) from the press point. Strictly `>` 6 counts as a drag. |
| `multiClickMs` | 500 | Two left presses at most this far apart (inclusive) can be a multi-click. |
| `multiClickPx` | 4 | ...and at most this far apart in space (Euclidean, inclusive). |
| `hintTtlMs` | 60 000 | A selection hint is live for 60 s. |
| `selectionChipMs` | 4 000 | Selection chip auto-hide. |
| `contextChipMs` | 8 000 | Context chip auto-hide. |

### 5.2 Configuration and types

- `GestureConfig { contextChip: bool /* rightClickChip */, selectionChip: bool /* selectionPopup */ }`. The service
  starts with both false and configures from settings.
- `HintKind = drag | multi-click | shift-click | keyboard | select-all`
- `HintClearReason = click | key | expired | blocked | reset`
- `ChipHideReason = mousedown | key | wheel | timeout | disabled | dismissed | activated | reset`
- Decisions (`gestures.ts:36-43`):
  `hint-set{kind}`, `hint-cleared{reason}`, `show-context-chip{x,y}`, `show-selection-chip{x,y}`,
  `hide-chip{reason}`, `activate-chip{mode}`. Coordinates are physical pixels.

### 5.3 State (`gestures.ts:129-134`, `98-118`)

| Field | Shape | Meaning |
|---|---|---|
| `hint` | `{kind, at, target: windowId \| null} \| null` | Something was probably selected. `target` is the window it was selected in (null means unknown). |
| `left` | `LeftPress \| null` | The left button is currently held: `{x, y, shift, multi, dragged, onChip, afterMenu, overlay, text: bool \| null}`. |
| `lastLeftDown` | `{x, y, at} \| null` | The previous left press, for multi-click detection. |
| `chip` | `{mode, at, ttl} \| null` | The machine's view of the visible chip. |
| `menuOpen` | bool | A right-click happened, so the next click or key most likely goes to the native context menu. |

Derived values:
- `wantsMouseMove = left && !left.dragged && !left.onChip && !left.overlay` (`gestures.ts:153-155`).
- `nextDeadline() = min(hint.at + 60000, chip.at + chip.ttl)` over whichever exist, or null (`gestures.ts:158-166`).
- `liveHint(now) = hint && now - hint.at < 60000` (`gestures.ts:258-260`).

### 5.4 Dependencies (queries the machine makes)

Each one is optional. A missing one, or an unknown answer, gives the benefit of the doubt as noted. The
implementations are in the service (`service.ts:191-202`).

| Dep | Implementation | Asked when | Unknown → |
|---|---|---|---|
| `isInsideChip(x,y)` | §6.4 | Only while `chip` is set: left, right or other press | false |
| `targetAt(x,y)` | `classify(windowInfoAtPoint(x,y) ?? foregroundWindowInfo())` | Left release of a gesture (at the **press** point); right press when armed | null (treated as matching any hint) |
| `focusedTarget()` | `classify(foregroundWindowInfo())` | Keyboard hints: every Shift+navigation keydown (auto-repeat included, unless a menu is open) and every Ctrl+A. It is not cached, so each call runs a process query | null |
| `ownOverlayAt(x,y)` | `isOwnOverlayAt(x,y) === true` (§11.4) | Every left press (must be cheap) | false |
| `textCursor()` | `textCursorShown()` (§11.5) | Every left press that is not on the chip or an overlay (plain clicks included). Again at the release of a plain drag, but **only** when the press answer was a definite `false` (short-circuit in `draggedOverText`) | null = "maybe text" |
| `chipsSuppressed()` | `fullscreenAppRunning()` (§11.6) | Only right before showing a chip | false |

### 5.5 Event handling

`handle(ev)` first runs `tick(ev.time)` (expiry decisions are prepended), then dispatches (`gestures.ts:214-242`).

#### 5.5.1 `tick(now)` (`gestures.ts:181-192`)
- If `hint` and `now - hint.at >= 60000`: clear it → `hint-cleared{expired}`.
- If `chip` and `now - chip.at >= chip.ttl`: clear it → `hide-chip{timeout}`.

#### 5.5.2 Left press (`onLeftDown`, `gestures.ts:277-317`)
```
if chip && isInsideChip(x,y):                         // press on the visible chip
    mode = chip.mode; chip = null
    left = {x,y, onChip:true, overlay:false, text:null, shift:false, multi:false, dragged:false, afterMenu:false}
    lastLeftDown = null; menuOpen = false
    emit activate-chip{mode}; return
if chip: chip = null; emit hide-chip{mousedown}
if ownOverlayAt(x,y):                                 // Vox's own non-activating window (widget / chip)
    left = {..., overlay:true, text:null}; lastLeftDown = null; menuOpen = false; return
prev = lastLeftDown
multi = prev != null && dist(prev, ev) <= 4 && (ev.clicks >= 2 || ev.time - prev.at <= 500)
left = {x,y, shift: ev.shift, multi, dragged:false, onChip:false, afterMenu: menuOpen, overlay:false,
        text: textCursor()}                           // null when unknown
lastLeftDown = {x, y, at: ev.time}
menuOpen = false
```
"The hook's click counter ignores position, so a second press must also land near the first one."
Overlay reason: "Pause / skip on the widget, or dragging it aside: the selection in the user's app is untouched."

#### 5.5.3 Move (`onMove`, `gestures.ts:319-326`)
If `left` is set, none of `dragged`, `onChip` and `overlay` is true, and `dist(left, ev) > 6`: set
`left.dragged = true` and `lastLeftDown = null`.

#### 5.5.4 Left release (`onLeftUp`, `gestures.ts:328-357`)
```
p = left; left = null
if !p || p.onChip || p.overlay: return
dragged = p.dragged || dist(p, ev) > 6               // moves may be coalesced: re-check at release
if dragged: lastLeftDown = null
if dragged || p.multi || p.shift:
    target = targetAt(p.x, p.y)                      // the PRESS point: a drag may overshoot onto another window
    if target?.blocked: clearHint(blocked); return
    if dragged && !p.multi && !p.shift && !draggedOverText(p): return   // hint untouched
    kind = dragged ? drag : (p.multi ? multi-click : shift-click)
    hint = {kind, at: ev.time, target: target?.id ?? null}; emit hint-set{kind}
    if cfg.selectionChip && !chipsSuppressed():
        chip = {mode: selection, at: ev.time, ttl: 4000}
        emit show-selection-chip{x: ev.x, y: ev.y}   // the RELEASE point
    return
if p.afterMenu: return                               // click picked/dismissed the native menu; selection survives
clearHint(click)
```
`draggedOverText(p) = p.text != false || textCursor() /*at release*/ != false`
(`gestures.ts:267-269`). A plain drag is rejected only when **both** ends definitively showed no text cursor. The
reason: "No text cursor at either end: a game's box-select, a brush stroke, a window or scrollbar drag. Not a new
selection, and not a reason to forget an existing one either." Multi-click and shift-click hints have no
text-cursor check, so a double-click on a desktop icon or a title bar also sets a hint.

Precedence of the kinds: `dragged` wins. A double-click-and-drag (word-wise selection) or a shift+drag therefore sets
kind `drag`, but it **skips** the text-cursor check, because that check applies only when `!multi && !shift`.

Note that the first click of every double-click is itself a plain click, so its release clears any existing hint
(`click`) unless it follows a right-click; the second press then sets a fresh `multi-click` hint.

#### 5.5.5 Right press (`onRightDown`, `gestures.ts:369-382`)
```
if chip && isInsideChip(x,y): return                 // nothing changes (the chip UI dismisses itself)
menuOpen = true; lastLeftDown = null
if armedFor(ev):
    chip = {mode: context, at: ev.time, ttl: 8000}
    emit show-context-chip{x: ev.x, y: ev.y}        // replaces a visible selection chip without a hide decision
    return
if chip: chip = null; emit hide-chip{mousedown}
```
`armedFor(ev)` (`gestures.ts:360-367`):
`cfg.contextChip && liveHint(ev.time) && !chipsSuppressed() && !(t = targetAt(ev.x, ev.y))?.blocked &&
(hint.target == null || t == null || hint.target == t.id)`.
Unknown identities get the benefit of the doubt.

The **right-button release is ignored**, because releasing the right button opens most context menus and must not
hide the chip (`gestures.ts:224`).

#### 5.5.6 Other button press (middle, X1, X2) (`onOtherDown`, `gestures.ts:384-391`)
Set `lastLeftDown = null` and `menuOpen = false`. If a chip is visible and the press is not inside it: hide it with
`hide-chip{mousedown}`.

#### 5.5.7 Wheel
If a chip is visible: hide it with `hide-chip{wheel}`. The hint is untouched (`gestures.ts:229-234`).

#### 5.5.8 Key down (`onKeyDown`, `gestures.ts:393-420`). Order matters.
```
if isModifier(code): return                          // held modifiers auto-repeat: no effect at all
if chip: chip = null; emit hide-chip{key}            // ANY other key hides the chip (Escape included)
if isNavigation(code):
    if ev.shift && !menuOpen: setKeyboardHint(keyboard)
    return                                           // plain arrows: no clear, menuOpen kept (menu navigation)
if code == A && ev.ctrl && !ev.alt && !ev.meta:      // Shift allowed
    menuOpen = false; setKeyboardHint(select-all); return
if menuOpen: menuOpen = false; return                // Escape/Enter/mnemonic went to the menu; hint survives
if isNeutral(code): return
if ev.ctrl && !ev.alt && (code == C || code == Insert): return   // copy keeps the selection (Shift and Win allowed)
clearHint(key)
```
Because AltGr arrives as LCtrl+RAlt (`ctrl` and `alt` both set), AltGr+A and AltGr+C are **not** treated as Ctrl+A or
Ctrl+C. They are ordinary keys that clear the hint.
`setKeyboardHint(kind, at)` (`gestures.ts:422-430`):
```
target = focusedTarget()
if target?.blocked: return                           // selecting inside Vox / an excluded app leaves other hints alone
prev = hint
hint = {kind, at, target: target?.id ?? null}        // timestamp always refreshed
if !prev || prev.kind != kind || prev.target != hint.target: emit hint-set{kind}   // no flood on auto-repeat
```

#### 5.5.9 External calls
- `configure(cfg)` (`gestures.ts:170-178`): stores the config. If the visible chip's mode is now disabled, it clears the
  chip with `hide-chip{disabled}`.
- `hideChip(reason='dismissed')` (`gestures.ts:195-199`): clears the chip with `hide-chip{reason}` if one is set.
- `reset()` (`gestures.ts:202-212`): emits `hide-chip{reset}` if a chip was set and `hint-cleared{reset}` if a hint
  was set. Clears chip, hint, left, lastLeftDown, menuOpen.

### 5.6 How the service applies decisions (`service.ts:763-785`)

| Decision | Action |
|---|---|
| `show-context-chip` / `show-selection-chip` | `showChip(mode, x, y)`: convert physical → DIP, remember `lastChipMode = mode`, emit `chip-show{x: round, y: round, mode}`. |
| `hide-chip` | Emit `chip-hide`. |
| `activate-chip{mode}` | `activate(mode)` (§6.3). |
| `hint-set` / `hint-cleared` | Debug log only. |

After every apply, including an empty one for non-move events, it calls `scheduleDeadline()`.

**Deadline timer** (`service.ts:835-855`). There is a single timer.
- `next = started ? machine.nextDeadline() : null`.
- If `next` equals the armed deadline, keep it. Otherwise clear it, and if `next` is set, arm a one-shot at
  `max(0, next - now) + 5 ms`. When it fires it runs `apply(machine.tick(now))`.
- `stop()` clears it.

### 5.7 When chips appear and disappear (summary)

**Context chip** ("Read with Vox"):
- **Appears**, at the right-press point, only when all of these hold:
  - the right-click chip is enabled;
  - a hint is live (made less than 60 s ago);
  - no D3D-fullscreen game or presentation is running;
  - the window under the cursor is not Vox, an excluded app or a password manager;
  - it is the same top-level window as the hint's (or either identity is unknown).
- **Disappears**:
  - after 8 s;
  - on any left press outside it;
  - on a right press with no live hint (or in another window);
  - on a middle or X press outside it;
  - on the wheel;
  - on any non-modifier keydown;
  - on activation or dismissal;
  - when the right-click chip setting is turned off;
  - when the hook stops.

**Selection chip** ("Read"):
- **Appears**, at the left-release point, when a mouse gesture sets a hint (drag over text, double or triple click, or
  shift+click), the selection popup is enabled and no D3D-fullscreen game or presentation is running.
- Keyboard selections (Shift+arrows, Ctrl+A) **never** show it; they only arm the context chip.
- **Disappears** after 4 s, and on the same triggers as the context chip, except that the setting that hides it is the
  selection popup being turned off (turning off the right-click chip does not hide it). A right-click that arms the
  context chip replaces it without a separate hide.

**Hint**:
- **Set by**:
  - a left drag of more than 6 px whose press or release showed a text cursor (or either is unknown, or the app is a
    document reader); a double-click-drag or shift+drag counts without that check;
  - a double or triple click within 4 px and 500 ms (or within the system double-click time), set at each release;
  - shift+click;
  - Shift + a navigation key (when no context menu is open);
  - Ctrl+A.
- **Cleared by**:
  - a plain left click that does not follow a right-click;
  - a non-neutral, non-copy keydown (when no menu is open);
  - a mouse gesture in a blocked window;
  - 60 s expiry;
  - a reset.
- **Unaffected by**:
  - plain navigation keys;
  - modifiers;
  - the wheel;
  - Ctrl+C or Ctrl+Insert;
  - the first key or click after a right-click;
  - a non-text drag;
  - keyboard selections in blocked windows.

Hints are tracked, and the context chip armed, even while chips are suppressed. Suppression only blocks showing a chip.

### 5.8 Scenario checklist (use as acceptance tests)

1. Drag-select in Chrome (I-beam) and right-click on the text: the context chip shows at the cursor. Click it to read.
2. Double-click a word, right-click: chip. Triple-click: the hint is set again at the release of each multi press
   (`hint-set{multi-click}` on the 2nd and 3rd release). The 1st click's release clears any older hint.
3. Ctrl+A in Notepad, right-click in Notepad: chip. Right-click in another app's window: no chip, and any visible chip
   hides.
4. Right-click with no prior selection gesture: no chip.
5. A box-select drag in a game (arrow cursor at both ends, no caret): no hint, and an existing hint is kept.
6. Select, then plain left-click elsewhere: the hint is cleared and a later right-click shows no chip.
7. Select, right-click (menu opens), click "Copy" in the native menu: the hint survives, so the next right-click shows
   the chip again.
8. Select, right-click, press Escape (closes the menu): the hint survives, the chip hides.
9. Select, right-click, press ↓ ↓ Enter in the menu: no keyboard hint from the arrows, Enter only resets `menuOpen`, the
   hint survives.
10. Selection popup on, drag-select: "Read" appears above-right of the release point and hides after 4 s, on any key, on
    the wheel, or on a press elsewhere.
11. A drag inside Vox's own main window: target blocked, so any existing hint is cleared. Clicking or dragging the
    floating widget (non-activating) is ignored entirely.
12. A PowerPoint slideshow or a D3D-exclusive game running: no chips at all.
13. Hold Shift and drag across text in a paint app (no I-beam): a `drag` hint is still set (shift skips the cursor
    check).
14. Select text, then AltGr+C (typing a character on, for example, a Polish layout): the hint is cleared.

---

## 6. Chip plumbing in the service

### 6.1 Coordinates
The hook delivers physical pixels. The original converts with Electron's `screen.screenToDipPoint` (per-display scale
factor) before emitting `chip-show` and before testing against chip bounds, which are in DIP (`service.ts:806-812`). If
the conversion throws, it uses the raw point. A native port that is per-monitor-v2 DPI-aware can keep **everything in
physical pixels** (hook points, window rects from `GetWindowRect`, `SetWindowPos`) and drop this conversion.
`chip-show` rounds with JavaScript `Math.round` (halves round toward +∞, so −1.5 → −1). Rust's `f64::round` rounds
halves away from zero; use `(v + 0.5).floor()` if the DIP path is kept and exact parity matters.

### 6.2 Chip bounds contract
The window layer reports the chip window's real bounds after placing it, and `null` as soon as a hide starts (the chip
is inert during its 140 ms exit animation). The bounds are the whole window, as Windows sized it: it may round the
width up a pixel, and it keeps transparent windows at least about 38 DIP tall (`src/main/windows.ts:758-800`).

### 6.3 Activation (`service.ts:787-796`)
```
activate(mode):
    if now - lastActivationAt < 300: return          // de-dupe: renderer pointerdown AND hook mouse-down both fire
    lastActivationAt = now
    machine.hideChip('activated')                    // decisions ignored; chip-hide emitted directly:
    scheduleDeadline(); emit chip-hide
    source = mode == context ? 'context-chip' : 'selection-popup'
    readSelection(source, {viaContextMenu: mode == context})   // async, not awaited
```
Why there are two activation paths: some context menus swallow the click that dismisses them, so the chip UI never
sees it. The hook sees every press (`docs/SPEC.md:135-137`). `lastActivationAt` starts at −∞. The dedupe is global,
not per chip: any second activation within 300 ms of the first is dropped, whichever path it came from.

Either path can win the race. If the hook wins, `activate-chip` clears `machine.chip` and the renderer's later
`chipActivated()` falls back to `lastChipMode` and is deduped. If the renderer wins, the hook's press then finds no
machine chip. If the chip window is still hit-testable, the press counts as an own-overlay press and is ignored. If the
window has already turned click-through, it counts as a click in the app below (it clears the hint unless it follows a
right-click). The capture never depends on the hint, so the read happens either way.

### 6.4 Hook-side chip hit test (`isInsideChip`, `service.ts:814-823`)
- No bounds: false.
- Convert the point to DIP. It must lie in the half-open rect `[x, x+w) × [y, y+h)`.
- Then `return isOwnOverlayAt(x, y) ?? true`. The reason: "A context menu, submenu or flyout opened later sits above the
  chip (topmost windows are not ordered by level on Windows): a click on it must not also start a read.
  WindowFromPoint ignores mouse capture and menu loops, so a menu that swallows the click on the chip itself still
  counts as a chip click. Unknown: bounds decide."

The chip window must be topmost at the "pop-up-menu" level (above native context menus), non-activating
(`WS_EX_NOACTIVATE`), and shown without activation, so the target app keeps focus and its selection.

---

## 7. Target classification and app rules

### 7.1 `classify(info)` (`service.ts:826-831`)
```
null info -> null (unknown)
blocked = info.pid == own process id
       || (info.processName != '' && (excluded.contains(processName) || isPasswordManager(processName)))
-> { id: info.windowId, blocked }
```
Blocked windows produce no hints and no chips: Vox's own windows (widget, chip, main app), excluded apps and password
managers. A process that can't be queried (`processName == ''`) is never blocked unless it is Vox.

### 7.2 Excluded apps normalization (`normalizeExe`, `service.ts:155-159`)
`trim → lowercase → last path segment (split on \ or /) → append ".exe" if not already ending in ".exe"`. Empty
results are dropped. For example `"C:\Tools\Foo"` becomes `"foo.exe"`. Note that `soffice.bin` would become
`soffice.bin.exe` and so could never match.

### 7.3 Password managers (`sensitive.ts:16-39`)
The match is case-insensitive on the exe name: `keepass.exe, keepassxc.exe, keeweb.exe, 1password.exe, bitwarden.exe,
enpass.exe, roboform.exe, robotaskbaricon.exe, dashlane.exe, keeperpasswordmanager.exe, nordpass.exe,
proton pass.exe, lastpass.exe, passwordsafe.exe, pwsafe.exe, sticky password.exe, stpass.exe`.

Effects:
- No hints or chips (classify).
- **No key is sent** when a capture starts with one in front: the result is refused with the `passwordManager` notice.
  The reason: in KeePass, Ctrl+C in the entry list copies the selected entry's password.

The primary defence against secrets is the clipboard privacy markers (§9.4). The deny list is secondary. Why secrets
matter so much: "Secrets must never be read aloud: they would be sent to the TTS API and stored in history"
(`sensitive.ts:1`). For the same reason the captured text itself is never logged.

`isPasswordManager(name)` is `name` non-empty and `lower(name)` in the set (`sensitive.ts:37-39`). An unknown
foreground process (`''` or no window) is never a password manager.

---

## 8. Capture pipeline

### 8.1 Constants (`CAPTURE_TIMING`, `service.ts:86-101`)

| Constant | ms | Use |
|---|---|---|
| `modifierWaitMs` | 700 | Max wait for physically held modifiers to be released before force-releasing them. |
| (modifier poll) | 10 | Poll interval of that wait (`keys.ts:257`). |
| `settleMs` | 40 | Pause before the snapshot and chord (hotkey, selection chip). |
| `contextMenuSettleMs` | 120 | The same pause for context-chip captures, so the native context menu can close first. |
| `pollIntervalMs` | 15 | Clipboard poll interval. |
| `pollTimeoutMs` | 450 | Clipboard poll window per chord. |
| `textSettleMs` | 1 500 | Extra wait once the clipboard changed and lists a text format but its text is not readable yet (RDP / VM redirection, huge selections). |
| `escapeSettleMs` | 80 | Pause after Escape before the retry chord. |
| `restoreDelayMs` | 150 | Delay before restoring the user's clipboard after a successful capture. |
| `lateCopyGraceMs` | 1 500 | After a capture found nothing: how long a slow app's late copy is still watched for (and undone). |
| `latePollMs` | 25 | Sequence-number poll interval of the late-copy and echo watches. |
| `activationDedupeMs` | 300 | Chip activation de-dupe. |

### 8.2 Entry: `readSelection(source, opts)` (`service.ts:280-311`)
1. If a capture is running: log and return, with no event.
2. `onLateText(text, fg)` emits `read{text, source, fg}` unless stopped.
3. `outcome = capture(opts.viaContextMenu ?? source == 'context-chip', onLateText)`. An exception counts as `empty`.
4. If stopped (shutting down): emit nothing.
5. Emit by outcome:
   - `text`: `read`, then the optional notice.
   - `empty`: `no-selection(source)`, then the optional notice.
   - `refused`: the notice.
   - `aborted`: nothing.

`capture()` (`service.ts:323-336`) is re-entrancy guarded. A second call returns `empty` without doing anything. It
sets `capturing=true` and `abortCapture=false` and stores the promise as `inflight`. In `finally` it resets
`capturing` and `abortCapture` and clears `inflight` if it is still the current one.

### 8.3 `runCapture(viaContextMenu, onLateText)` (`service.ts:338-383`)

```
1  await flushRestore()           // ends a previous capture's pending restore / late-copy watch, so its snapshot is
                                  // back on the clipboard before ours is taken
2  ensure native modules loaded
3  win = foregroundWindowInfo()   // BEFORE any key is sent: the app the selection lives in
   foreground = win ? {processName, appName, title, pid} : null
4  if isPasswordManager(win.processName): return refused(passwordManager)      // nothing sent
5  method = copyMethodFor(win.processName, win.className)
   if method == read-clipboard: return readCopyOnSelect(foreground)            // §8.11
6  forced = waitForModifiersReleased(tracker, 700, poll 10, aborted)            // §10.3 (log forced ones)
7  sleep(viaContextMenu ? 120 : 40)
8  if aborted: return aborted
9  snapshot = snapshotClipboard()                                              // §9.1
10 if aborted: return aborted                                                  // nothing sent yet, nothing to undo
11 since   = clipboardSequenceNumber()        // null only without Win32
12 cleared = since == null && isSnapshotComplete(snapshot) && clearClipboard()
13 stale   = (since == null && !cleared) ? readClipboardText() : undefined
14 ctx = {snapshot, since, cleared, stale, foreground, chords: 0, lastChordAt: 0}
15 raw = ''; privacy = null
   try:
       raw = copyAndRead(method.chord, viaContextMenu && method.escapeRetry, ctx)
       if raw.trim() != '': privacy = clipboardPrivacy()     // while the copied data is still on the clipboard
   finally:                                                  // on abort and on errors too
       notice = settleClipboard(ctx, raw, privacy != null, onLateText)
16 if privacy: return refused(refusal(privacy))
17 if aborted: return aborted
18 text = raw.trim(); return text ? {kind: text, text, foreground, notice} : {kind: empty, notice}
```
The text itself is never logged.

Notes on the steps:
- Step 1 runs for **every** capture, including the copy-on-select path (§8.11) and the password-manager refusal.
  Flushing a pending late-copy watch puts the old snapshot back whenever the clipboard sequence moved, whatever the
  new content is (§8.8). See §13 for the consequence.
- With no foreground window (or no Win32), `win` is null: `foreground = null`, no password-manager check applies, and
  `copyMethodFor(undefined, undefined)` gives **Ctrl+C with the Escape retry allowed**.
- The modifier wait (step 6) cannot return `aborted` by itself. An abort during it skips the forced release, and the
  settle sleep still runs before step 8 returns `aborted`.
- Ordering of the returns: a privacy refusal (step 16) wins over an abort (step 17).

Why the sequence number is taken **once, before the first chord**: a late copy from attempt 1 still counts during
attempt 2. With sequence numbers the clipboard is **never cleared**. Stale text is told apart by the sequence number,
and when the copy writes nothing the user's clipboard (including formats a snapshot cannot hold) stays exactly as it
was.

Without sequence numbers (fallback only), emptying the clipboard first is the only way to recognise a copy of the very
text already on it. So it is done only when the snapshot can put everything back.

### 8.4 Copy method (`copyMethodFor`, `keys.ts:86-94`)

Process and class names are compared lower-cased.
```
if exe in COPY_ON_SELECT_PROCESSES or class in COPY_ON_SELECT_CLASSES:  -> read-clipboard (send nothing)
if exe in TERMINAL_PROCESSES or class in TERMINAL_CLASSES
   or exe in TERMINAL_EDITOR_PROCESSES or JETBRAINS_IDE matches exe:     -> chord Ctrl+Insert, escapeRetry=false
else                                                                     -> chord Ctrl+C,      escapeRetry=true
```

| Set | Members | Reason |
|---|---|---|
| `TERMINAL_PROCESSES` (`keys.ts:10-26`) | `windowsterminal.exe, cmd.exe, powershell.exe, pwsh.exe, conhost.exe, wt.exe, mintty.exe, alacritty.exe, wezterm-gui.exe, conemu.exe, conemu64.exe, tabby.exe, hyper.exe, warp.exe, fluentterminal.app.exe` | Ctrl+C means SIGINT in a console. |
| `TERMINAL_CLASSES` (`keys.ts:29-35`) | `consolewindowclass, cascadia_hosting_window_class, mintty, virtualconsoleclass, org.wezfurlong.wezterm` | Catches hosts whose exe isn't listed, or renamed / portable builds. |
| `COPY_ON_SELECT_PROCESSES` (`keys.ts:41-46`) | `putty.exe, kitty.exe, kitty_portable.exe, mobaxterm.exe` | The selection is already on the clipboard, and every chord (even Ctrl+Insert or Escape) would be forwarded to the remote shell. |
| `COPY_ON_SELECT_CLASSES` (`keys.ts:47`) | `putty, kitty` | |
| `TERMINAL_EDITOR_PROCESSES` (`keys.ts:54-63`) | `code.exe, code - insiders.exe, codium.exe, vscodium.exe, cursor.exe, windsurf.exe, zed.exe, devenv.exe` | Integrated terminal: Ctrl+C with nothing selected interrupts the running job. These apps (and Blink web views) map Ctrl+Insert to Copy. Ctrl+Insert is **not** a safe global default, because many other apps don't map it. |

JetBrains regex (`keys.ts:65-66`):
```
^(idea|pycharm|webstorm|phpstorm|clion|goland|rider|rubymine|datagrip|dataspell|rustrover|studio|aqua|writerside)(64)?\.exe$
```
`escapeRetry=false` for terminals and editors because an Escape would reach the shell.

### 8.5 Sending chords and the Escape retry (`copyAndRead`, `service.ts:394-423`)
```
attempts = escapeRetry ? 2 : 1            // escapeRetry = viaContextMenu && method.escapeRetry
for attempt in 0..attempts:
    if aborted: return ''
    if attempt > 0:
        if since != null && seq() != since: break      // the 1st copy DID arrive, just without text (image, files):
                                                       // asking again would only repeat it
        if !sendEscape(): break                        // close a context menu that still owns the keyboard
        sleep(80)
    if !sendChord(chord): log "no key backend"; return ''
    ctx.chords++; ctx.lastChordAt = now
    text = waitForClipboardText({timeout 450, interval 15, sinceSequence: since, staleText: stale,
                                 settle 1500, aborted})
    if text.trim(): return text                        // untrimmed text is returned
return ''
```
So only **context-chip captures with Ctrl+C** get the Escape retry. Hotkey and selection-chip captures make a single
attempt.

`sendChord` returns false both when there is no key backend and when a native call threw. Either way the log line is
`cannot send the copy shortcut: no key backend (uiohook-napi and koffi unavailable)` and the attempt loop ends. `chords`
is incremented only after a successful send, so a failed first send leaves `chords = 0`. Settle then finds the
clipboard unchanged and (with a sequence number) still arms the late-copy watch. A failed `sendEscape` ends the retry
silently.

### 8.6 Polling for the copied text (`waitForClipboardText`, `clipboard.ts:287-305`)
```
deadline = now + timeoutMs; settleDeadline = deadline + settleMs
loop:
    if aborted(): return ''
    moved = (since == null) || seq() != since          // a null seq() later also counts as moved
    if moved:
        text = readClipboardText()                     // CF_UNICODETEXT as a string; '' on any error
        if text != '' && (since != null || staleText undefined || text != staleText): return text
    if now >= deadline:
        settling = since != null && moved && now < settleDeadline && clipboardHasTextFormat()
        if !settling: return ''
    sleep(intervalMs)
```
`clipboardHasTextFormat()` is `IsClipboardFormatAvailable(CF_UNICODETEXT=13) || IsClipboardFormatAvailable(CF_TEXT=1)`
(`clipboard.ts:191-198`). It needs no clipboard lock and is true even when the owner hasn't rendered the data yet
(delayed rendering). The 1.5 s extension applies only after the sequence number moved and while a text format is
listed but unreadable.

Whitespace-only text ends the poll (it counts as an arrival), but the caller treats it as nothing.

### 8.7 Deciding what happens to the user's clipboard (`settleClipboard`, `service.ts:438-469`)

`echoOf = (ctx.chords > 1 && raw.trim()) ? raw : undefined`. The context chip's retry sent a second chord, and a slow
app may still answer it after the restore.

| Case | Action | Returned notice |
|---|---|---|
| `secret` (privacy marker found, `secret` or `private`) | `scheduleRestore(delay 0, secret=true, echoOf)`. This runs **regardless of `restoreClipboard`**: "Never leave a secret on the clipboard on our account." If the snapshot had failed, the clipboard is **emptied** instead. | none (the refusal notice comes from the outcome). No loss notice either, even when the clipboard was emptied. |
| Text captured, `restoreClipboard == false` | Nothing: the copied text stays on the clipboard. | none |
| Text captured, `restoreClipboard == true` | `scheduleRestore(delay 150, ifStill={text: raw, seq: seq() now}, echoOf)` | `lossNotice(snapshot)` |
| Nothing captured | If aborted and a chord was sent: `awaitInFlightCopy` (below). Then if `clipboardChanged(ctx)`: `scheduleRestore(delay 0)`. This covers a clipboard emptied by us or replaced by a non-text copy (files, an image), and runs regardless of `restoreClipboard`. | `lossNotice(snapshot)` |
| Nothing captured, unchanged | If `since != null && !aborted`: `watchLateCopy(ctx, onLateText)` | none |

`clipboardChanged(ctx)` (`service.ts:426-430`):
- With a sequence number: `seq() != since`.
- Otherwise, if cleared: `true`.
- Otherwise: `readClipboardText() != (stale ?? '')`.

`awaitInFlightCopy(ctx)` (`service.ts:475-480`): quitting mid-capture. While `seq() == since && now < lastChordAt + 450`,
sleep 15 ms. This gives the copy the rest of its normal window (well inside the shutdown budget), so the restore can
undo it instead of it landing after Vox is gone.

### 8.8 Restore jobs

There is **at most one pending clipboard job** (`pendingRestore`), plus a chain of running clipboard work
(`restoring`).

`scheduleRestore(snapshot, {delayMs, ifStill?, secret?, echoOf?})` (`service.ts:487-505`) creates a job whose
`finish()` does this:
```
if ifStill:
    untouched = ifStill.seq != null && seq() == ifStill.seq
    if !untouched && readClipboardText() != ifStill.text: return   // the user copied something else meanwhile
putBack(snapshot, clearIfUnrestorable = secret)
if echoOf != undefined: guardEcho(snapshot, echoOf, secret)
```
The job is armed with a timer of `delayMs` and made pending.

`putBack(snapshot, clearIfUnrestorable)` (`service.ts:507-513`): if the snapshot failed, clear the clipboard only when
`clearIfUnrestorable` (the secret case) and otherwise leave it. If it didn't fail, `restoreClipboard(snapshot)` (§9.2).

`guardEcho(snapshot, text, secret)` (`service.ts:565-573`): after a restore, a copy of the very same text is the
answer to the second chord, so undo it once. `from = seq()` (if null, do nothing). Watch the sequence: when it moves,
or on flush, `if seq() != from && readClipboardText() == text: putBack(snapshot, secret)`. A different text is the
user's own copy and is left alone.

`watchLateCopy(ctx, onLateText)` (`service.ts:520-530`) covers the case where the chord was delivered but nothing
arrived in time (Remote Desktop / Citrix / VM redirection, or Word after Ctrl+A on a long document). It watches the
sequence starting at `since`:
- **When it moves**: `finishLateCopy`.
- **When flushed early** (next capture, quitting): if `seq() != since`, `putBack(snapshot, false)`. A late copy that
  already landed is only undone, not read.
- **When 1.5 s pass with no move**: the job expires silently and the snapshot is dropped. Nothing is restored, because
  nothing changed.

Neither branch looks at *what* moved the sequence. Any clipboard write in the window counts as "the late copy",
including the user's own Ctrl+C or a clipboard manager re-writing the clipboard. It is then read aloud (if it is text)
and later replaced by the old snapshot, or undone outright on an early flush (§13).

`finishLateCopy(ctx, onLateText)` (`service.ts:532-562`):
```
superseded = () => capturing || stopped
raw = waitForClipboardText({timeout 450, interval 15, sinceSequence: since, settle 1500, aborted: superseded})
text = raw.trim()
if !text || !onLateText || superseded(): putBack(snapshot, false); return
echoOf = chords > 1 ? raw : undefined
privacy = clipboardPrivacy()
if privacy: scheduleRestore(delay 0, secret=true, echoOf); emit notice(refusal(privacy)); return
seq0 = seq()
onLateText(text, ctx.foreground)              // emits 'read' with the ORIGINAL trigger's source
if !restoreClipboard: return
scheduleRestore(delay 150, ifStill={text: raw, seq: seq0}, echoOf)
if lossNotice(snapshot): emit notice
```

`watchSequence(from, onMove, onFlush)` (`service.ts:579-596`):
```
deadline = now + 1500
job.finish = onFlush
tick every 25 ms (first tick at +25):
    if job is no longer pending: stop
    if seq() == from:
        if now < deadline: re-arm 25 ms   else: pendingRestore = null (expire silently, nothing runs)
        return
    pendingRestore = null; track(onMove())
setPending(job)
```

`setPending(job)` (`service.ts:598-602`): if a different job is pending, run its decision **now** (`runPending`), then
make this job pending.

`runPending(job)` (`service.ts:605-611`): if it is still the pending one, un-pend it, cancel its timer and
`track(job.finish())`.

`track(p)` (`service.ts:614-624`): chains `p` (errors logged as `clipboard restore failed`) onto `restoring`, so
`restoring` settles only after all earlier work **and** `p` are done. It clears `restoring` when that chain drains and
no newer work was tracked meanwhile.

`flushRestore()` (`service.ts:627-636`): up to **4 rounds**. In each round: run the pending job now, await
`restoring`, and stop when nothing is pending. The bound exists because a finish can queue the next job
(late copy → restore → echo guard).

### 8.9 Abort and shutdown
`stop()` sets `abortCapture` when a capture runs. The capture checks it at every step and in every poll loop. Even when
aborted, `settleClipboard` always runs once a chord might have been sent ("the restore decision must never be
skipped"). Captures that finish after `stop()` emit nothing (`stopped`). The shell then awaits `flush()` so a pending
restore lands before exit.

### 8.10 Timeline (typical hotkey capture, everything fast)
```
t0        hotkey (Ctrl+Alt+R) fires; user still holds Ctrl+Alt
t0..≤700  poll GetAsyncKeyState every 10 ms until no modifier is down (else synthesize key-ups at 700)
+40       settle
+~5       snapshot clipboard (≤500 ms budget)
          seq0 = GetClipboardSequenceNumber()
          keybd_event: LCtrl↓ C↓ C↑ LCtrl↑
+15..450  poll every 15 ms until seq != seq0 and CF_UNICODETEXT readable (+≤1500 if a text format is listed but empty)
          check privacy markers → emit 'read' {trimmed text, 'hotkey', foreground}
+150      restore snapshot if the clipboard still holds our copy (seq unchanged, or text equal)
```
Worst case before giving up on a context chip: 700 (modifiers) + 120 (settle) + ~540 (snapshot: up to 2 × 20 ms
retries plus the 500 ms budget, plus however long the last format read blocks) + 450 (attempt 1, nothing moved) + 80
(Escape) + 450 + 1500 (attempt 2 moved, text listed but unreadable) ≈ **3.8 s**, plus any flushed restore at step 1.
Both attempts cannot use the 1.5 s extension: the extension needs the sequence to have moved, and a moved sequence
stops the retry. A late copy can then still produce a `read` up to 1.5 s + 0.45 s (+1.5 s settle) later.

### 8.11 Copy-on-select path (`readCopyOnSelect`, `service.ts:386-392`)
Used for PuTTY and similar. No modifier wait, no snapshot, no keys, no restore, no abort checks, no late-copy watch.
The step-1 `flushRestore()` of §8.3 has already run, though.
- `text = readClipboardText().trim()`. If empty: return `empty` with no notice.
- If the clipboard carries privacy markers: return `refused`.
- Otherwise return `text`.

---

## 9. Clipboard (`clipboard.ts`)

### 9.1 Snapshot (`snapshotClipboard`, `clipboard.ts:68-115`)

Options and defaults (`clipboard.ts:35-37`): `maxFormatBytes = 64 MiB`, `maxTotalBytes = 192 MiB`, `budgetMs = 500`.
The memory is held only while a capture runs; a 4K screenshot as a raw DIB alone is 33 MB.

```
ClipboardSnapshot { items: [ {formatKey -> payload} ], skipped: [formatKey], bytes, failed: bool }

started = now
listing = read clipboard; up to 3 attempts, 20 ms apart     // another app (often a clipboard manager) may hold it open
if all attempts failed: failed = true; return
for each item (on Windows: one):
    formats sorted by PRIORITY (stable): text/plain, text/html, text/rtf, image/png, bookmark, then the rest
    for each format:
        if it is a raw OS format named "DataObject" or "Ole Private Data" (case-insensitive): skip silently
            // OLE plumbing that only means something while the original owner holds the clipboard
        if now - started > budgetMs: skipped += format; continue        // delayed-render sources can be slow
        value = read(format)          // on error: skipped += format; continue
        size  = bytes (strings counted as UTF-16: len*2)
        if size > maxFormatBytes or bytes + size > maxTotalBytes: skipped += format; continue
        keep; bytes += size
    keep the item if it has any format
if skipped nonempty: drop "Object Descriptor" and "Link Source Descriptor" from kept items (bytes adjusted),
                     drop items left empty      // OLE descriptors only describe the payload next to them
if no items kept and skipped nonempty: failed = true   // restoring "empty" would only wipe the captured text too
```
`isSnapshotComplete = !failed && skipped.is_empty()` (`clipboard.ts:135-137`).

Details that matter for parity:
- The `DataObject` / `Ole Private Data` skip happens **before** the budget check. Those formats never appear in
  `skipped` and can never make the snapshot partial or failed.
- The budget is checked **before** each format read, not during it. A single slow delayed-render read can overrun it.
  Once over budget, every remaining format is recorded as skipped (and so triggers `clipboardTooLarge`).
- An empty clipboard gives `items = []`, `skipped = []` and `failed = false`. Restoring it empties the clipboard.
- Byte sizes in the original were Electron's view: strings count `len × 2` (UTF-16 code units), blobs their byte size,
  and a bookmark `(title.len + url.len) × 2`. Natively, use `GlobalSize`.
- A read error and an over-limit size are both recorded as "skipped". There is no separate "unreadable" list, so
  `lossNotice` says "(it was too large)" even when the real cause was an unreadable format or the time budget.

**What Electron actually saw.** The original used Electron 44's async clipboard. It lists standard MIME types
(`text/plain` = CF_UNICODETEXT, `text/html` = "HTML Format", `text/rtf` = "Rich Text Format", `image/png`, and a bookmark
= URL+title) plus **registered** OS formats by name. Standard Win32 formats Electron does not map, such as **CF_HDROP**
from an Explorer file copy, are never in a snapshot. That is one reason the capture avoids touching the clipboard at
all when it can (the sequence-number design) (`clipboard.ts:1-9`).

**Native equivalent (recommended).** Use `OpenClipboard(hwnd)`, then `EnumClipboardFormats` in order. For each format,
get its name (`GetClipboardFormatNameW` for ids ≥ 0xC000) and use `GetClipboardData`. For HGLOBAL formats, copy
`GlobalSize` bytes under `GlobalLock`. Apply the same priority order, the same skip rules (by name), the same budgets,
the descriptor drop and the `failed` / `skipped` semantics. Then:
- Skip formats that are not HGLOBAL: `CF_BITMAP(2)`, `CF_METAFILEPICT(3)`, `CF_PALETTE(9)`, `CF_ENHMETAFILE(14)`,
  `CF_DSPBITMAP(0x82)`, `CF_DSPMETAFILEPICT(0x83)`, `CF_DSPENHMETAFILE(0x8E)`, `CF_OWNERDISPLAY(0x80)`, the private
  range `0x0200–0x02FF` and the GDI range `0x0300–0x03FF`. Alternatively, convert CF_ENHMETAFILE with
  `GetEnhMetaFileBits` / `SetEnhMetaFileBits`. Count a skipped format as "skipped" only if it is not one Windows will
  re-synthesize.
- Don't store synthesized duplicates. If CF_UNICODETEXT is present, CF_TEXT and CF_OEMTEXT are synthesized from it;
  keep CF_LOCALE. If CF_DIB or CF_DIBV5 is present, CF_BITMAP and the other DIB format are synthesized. Reading a
  synthesized format forces a conversion and wastes budget.
- `GetClipboardData` on a delayed-render format sends `WM_RENDERFORMAT` to the owner synchronously. That is why the
  500 ms budget is checked between formats. Run the snapshot off the UI thread.
- Restoring CF_HDROP raw (it is an HGLOBAL `DROPFILES`) is an improvement over the original; see the open questions.

### 9.2 Restore (`restoreClipboard`, `clipboard.ts:140-162`)
```
if failed: return false
if no items: EmptyClipboard; return true
try: write ALL kept formats in ONE clipboard transaction (atomic); return true
catch: // a raw format the OS refused: keep what matters for pasting
try: write only PRIORITY formats (text/plain, text/html, text/rtf, image/png, bookmark); none -> false
catch: false
```
Native: `OpenClipboard(own message-only hwnd)`, `EmptyClipboard`, then `GlobalAlloc(GMEM_MOVEABLE)` + copy +
`SetClipboardData` per format, then `CloseClipboard`. **You must open the clipboard with a real window handle.** With
`OpenClipboard(NULL)`, `EmptyClipboard` sets the owner to NULL and `SetClipboardData` then fails. Retry `OpenClipboard`
briefly (3 attempts, 20 ms apart, as in the snapshot), because other apps hold it open momentarily. Restore the privacy
markers too, since they are just registered formats in the snapshot. That keeps a previously copied password excluded
from clipboard history. The service ignores the return value.

### 9.3 Small helpers
- `clearClipboard()`: empty it; false on error (`clipboard.ts:164-171`).
- `readClipboardText()`: Unicode text, or `''` on any error (`clipboard.ts:173-179`). Natively this is
  `OpenClipboard` (a short retry as in the snapshot is fine; return `''` if it stays locked) →
  `GetClipboardData(CF_UNICODETEXT)` → `GlobalLock` → UTF-16 up to
  the first NUL (bounded by `GlobalSize / 2`). Windows synthesizes CF_UNICODETEXT from CF_TEXT / CF_OEMTEXT, so ANSI
  copies are covered. The value is not trimmed here; callers trim.
- `clipboardSequence()`: `GetClipboardSequenceNumber()`, or null without Win32 or on error (`clipboard.ts:182-188`).
  Note: Windows returns 0 when the caller has no window-station access; the original does not special-case that.

### 9.4 Privacy markers (`clipboardPrivacy`, `clipboard.ts:228-265`; names in `sensitive.ts:8-14`)

| Format (registered name) | Rule | Result |
|---|---|---|
| `ExcludeClipboardContentFromMonitorProcessing` | presence | `secret` (set by password managers such as KeePass, KeePassXC, Bitwarden and Chromium's password manager) |
| `Clipboard Viewer Ignore` | presence | `secret` |
| `CanIncludeInClipboardHistory` | DWORD value 0 | `private` (browsers set it for InPrivate / Incognito windows) |
| `CanUploadToCloudClipboard` | DWORD value 0 | `private` |

Algorithm (with Win32):
1. If any marker format is available (`IsClipboardFormatAvailable(RegisterClipboardFormatW(name))`, ids cached per
   name), return `secret`.
2. `flags` = the privacy flag formats that are available. If any of the availability checks in steps 1-2 throws,
   treat it as no flags (so a throwing marker check yields `null`, not `secret`). If there are no flags, return
   `null`.
3. Read each present flag's payload bytes. Return `private` if any present flag `isPrivateFlagValue(bytes)`. A flag that
   is listed but whose bytes can't be obtained counts as private. If reading throws, return `private` ("a flag is there
   but unreadable: assume the restrictive value"). Flag names are matched case-insensitively against the raw
   format names. In the original the bytes came from Electron's listing, with a string payload UTF-8 encoded.
   Natively, read the HGLOBAL bytes.

Both `secret` and `private` lead to the same handling (refusal plus an immediate restore, §8.7). Only the notice text
differs.

`isPrivateFlagValue(bytes)` (`sensitive.ts:42-45`): `bytes == null || len < 4 → private`. Otherwise it is private iff
`bytes[0]|bytes[1]|bytes[2]|bytes[3] == 0` (a little-endian DWORD 0).

Fallback without Win32: scan the listed raw formats. A marker gives `secret` (returned immediately); a flag with a
private value gives `private`; an unreadable clipboard gives `null`.

Native: `GetClipboardData(id)` → `GlobalLock` → read 4 bytes. The check runs only when non-empty text was captured
(and in the copy-on-select and late-copy paths), while the copied data is still on the clipboard. There is no other
"private window" detection: private-window text is recognised **only** by these flags on the copied data.

---

## 10. Synthetic keys and held modifiers (`keys.ts`)

### 10.1 Key backends (`keys.ts:116-137`)
1. **Preferred (Win32)**: `keybd_event(vk, scan, flags, 0)` with `scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC=0) & 0xFF`
   and `flags = (up ? KEYEVENTF_KEYUP=2 : 0) | (extended ? KEYEVENTF_EXTENDEDKEY=1 : 0)` (`win32.ts:180-184`). The
   reason: it sends the virtual key **and** its scan code (plus the extended flag), like a real keyboard.
2. **Fallback (hook library)**: `SendInput` with the VK only and scan code 0. That leaves `KeyboardEvent.code` empty in
   Chromium apps and breaks apps that dispatch shortcuts by scan code.

VK table for injected keys (`keys.ts:97-109`):

| Key | VK | Extended |
|---|---|---|
| C | 0x43 | no |
| Insert | 0x2D | **yes** |
| Escape | 0x1B | no |
| LShift / RShift | 0xA0 / 0xA1 | no / no |
| LCtrl / RCtrl | 0xA2 / 0xA3 | no / **yes** |
| LAlt / RAlt | 0xA4 / 0xA5 | no / **yes** |
| LWin / RWin | 0x5B / 0x5C | yes / yes |

### 10.2 Chords (`keys.ts:151-192`)
- `sendChord({modifier, key})` sends `modifier↓, key↓, key↑, modifier↑`, back-to-back with no delays, as four
  separate `keybd_event` calls (not one atomic `SendInput`, so physical input can interleave). Every release is
  in a `finally`, so a native failure can never leave Ctrl stuck. If `modifier↓` throws, `modifier↑` is still sent.
  If `key↓` throws, `key↑` and then `modifier↑` are still sent. It returns false if there is no backend or a call
  threw. The modifier is always LCtrl (`Ctrl+C` = 0xA2 + 0x43; `Ctrl+Insert` = 0xA2 + 0x2D extended). Note that LCtrl is
  injected even when the user is (after a forced release) still physically holding a modifier.
- `sendEscape()` sends `Esc↓ Esc↑`.
- `releaseKeys(codes)` sends a key-up for each code and ignores failures. "The next physical press of the key resyncs
  its state."

### 10.3 Held modifiers (`keys.ts:197-272`)
- `heldModifiers()`: with Win32 it is authoritative, `GetAsyncKeyState(vk) & 0x8000` for the 8 side-specific VKs
  (0xA0–0xA5, 0x5B, 0x5C). The hook's own view goes stale if it misses a key-up, for example across the secure desktop.
  Without Win32 it uses the ModifierTracker: a set fed by hook keydown (modifiers only) and keyup (any code removes
  itself). The tracker is cleared when the hook stops.
- `waitForModifiersReleased(tracker, 700, 10, aborted)`: while any modifier is held and the deadline hasn't passed,
  sleep 10 ms and re-check. `aborted` is checked only right after each sleep (not before the first check), and on
  abort it returns `[]` without releasing anything. If nothing is held at the start it returns `[]` at once.
  If `GetAsyncKeyState` throws, the check falls back to the tracker. If modifiers are still held at the
  deadline, synthesize their key-ups, forget them in the tracker, and return them (logged as "force-released").
  Purpose: a Ctrl+C sent while the user still holds Shift from `Ctrl+Alt+Shift+…` would become Ctrl+Shift+C (DevTools in
  browsers).

---

## 11. Foreground and window info (`foreground.ts`)

Every function is synchronous, takes well under a millisecond and never throws. `null` means unknown: no Win32, no
window, or a protected process.

### 11.1 `describe(hwnd) → WindowInfo | null` (`foreground.ts:98-114`)
```
if hwnd null: null
pid = GetWindowThreadProcessId(hwnd)           // 0 (window gone) -> null
processName = lower(basename(QueryFullProcessImageNameW(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, pid))))
              ('' when OpenProcess fails: protected / exited process)
title = cleanTitle(GetWindowTextW(hwnd))
if title == '':                                 // owned popups (a browser's context menu, a tool window) are captionless
    owner = GetAncestor(hwnd, GA_ROOTOWNER) ?? hwnd
    if owner != hwnd: title = cleanTitle(GetWindowTextW(owner))
appName = friendlyAppName(processName)
if processName == 'applicationframehost.exe' && title:     // UWP host: caption ends with the real app ("Inbox - Mail")
    appName = last(title.split(' - ')).trim() || appName
windowId = decimal(hwnd); className = GetClassNameW(hwnd)
```
`cleanTitle(s)`: replace every run of whitespace with a single space, trim, then truncate to **300** chars
(`foreground.ts:23,94-96`). `basename` splits on the last `\` or `/`. The truncation is by UTF-16 code units, so it can
split a surrogate pair. A Rust port should truncate at 300 UTF-16 units and drop a dangling high surrogate, or truncate
at 300 `char`s.

`processName` is also `''` when `QueryFullProcessImageNameW` fails, including for an image path of 1024 or more WCHARs
(the buffer size). `className` is read from the **same** `hwnd` the info describes (not the owner).

`windowId` is the decimal value of the `hwnd` passed in. For `foregroundWindowInfo()` that is the raw
`GetForegroundWindow()` handle, without `GA_ROOT`. For point lookups it is the `GA_ROOT` ancestor. Both are top-level
windows. An owned top-level window (a dialog, a tool window) has its own id, distinct from its owner's.

- `foregroundWindowInfo()` is `describe(GetForegroundWindow())` (`foreground.ts:117-124`).
- `windowInfoAtPoint(x,y)` is `describe(rootWindow(WindowFromPoint({trunc x, trunc y})))`, with
  `rootWindow(h) = GetAncestor(h, GA_ROOT) ?? h` (`foreground.ts:139-146`, `win32.ts:152-154`). Physical coordinates.
  `WindowFromPoint` skips hidden, disabled and click-through windows.
- `getForegroundInfo()` / `toForegroundInfo()` drop `windowId` and `className`.

### 11.2 Friendly names (`friendlyAppName`, `foreground.ts:25-79`)
The name is lower-cased and trimmed first; an empty name gives `''`. Then a table lookup:

| exe | name | exe | name |
|---|---|---|---|
| chrome.exe | Chrome | msedge.exe | Edge |
| winword.exe | Word | acrord32.exe, acrobat.exe | Acrobat |
| notepad.exe | Notepad | code.exe | VS Code |
| firefox.exe | Firefox | outlook.exe, olk.exe | Outlook |
| onenote.exe | OneNote | slack.exe | Slack |
| discord.exe | Discord | obsidian.exe | Obsidian |
| notion.exe | Notion | teams.exe, ms-teams.exe | Teams |
| explorer.exe | Explorer | brave.exe | Brave |
| opera.exe | Opera | vivaldi.exe | Vivaldi |
| sumatrapdf.exe | SumatraPDF | kindle.exe | Kindle |
| excel.exe | Excel | powerpnt.exe | PowerPoint |
| windowsterminal.exe, wt.exe | Terminal | cmd.exe | Command Prompt |
| powershell.exe, pwsh.exe | PowerShell | notepad++.exe | Notepad++ |
| sublime_text.exe | Sublime Text | cursor.exe | Cursor |
| thunderbird.exe | Thunderbird | whatsapp.exe | WhatsApp |
| telegram.exe | Telegram | foxitpdfreader.exe | Foxit Reader |
| msedgewebview2.exe | Edge WebView | | |

Otherwise: strip a trailing `.exe`, split on the regex `/[\s._-]+/`, drop empty parts, upper-case the first char of
each part, and join with a space. Example: `my_tool.exe` → `My Tool`.

### 11.3 Window identity
`windowId` is the root HWND as a decimal string. A hint made by mouse is tied to the root window **under the press
point**. A keyboard hint is tied to the **foreground** window. A right-click compares against the root window under
the right-press point.

### 11.4 Own overlay test (`isOwnOverlayAt`, `foreground.ts:152-162`)
```
no Win32 -> null
h = rootWindow(WindowFromPoint(pt)); h null -> false
-> GetWindowThreadProcessId(h) == own pid && (GetWindowLongW(h, GWL_EXSTYLE) & WS_EX_NOACTIVATE 0x08000000) != 0
```
It is cheap (no process query) and runs on every left press. Vox's floating widget and chip are non-activating windows
(`WS_EX_NOACTIVATE`; in the original, Electron's `focusable: false` is what sets that style, `win32.ts:49-50`), so the
port's overlay windows **must** carry that style for this test to work. It returns `false` when `WindowFromPoint` finds
nothing, and `null` (unknown) only without Win32 or on an exception. The main app
window is activating, so presses there count as gestures in a blocked target, not as overlay presses.

### 11.5 Text-cursor evidence (`textCursorShown`, `foreground.ts:207-218`)
```
no Win32 -> null
shape = cursorShape()
if shape == ibeam || foregroundHasCaret(): true
if shape == suppressed || documentAppInFront(): null        // can't judge
false
```
- `cursorShape()` (`win32.ts:191-197`): `GetCursorInfo` with `cbSize` set. A failure gives `other`. Then
  `flags & CURSOR_SUPPRESSED(2)` gives `suppressed` (touch or pen input, no cursor drawn), `!(flags & CURSOR_SHOWING(1))`
  gives `hidden` (games often draw their own), `hCursor == LoadCursorW(NULL, IDC_IBEAM=32513)` gives `ibeam`, and
  anything else is `other`. System cursors are shared, so every app showing the standard I-beam shows that exact handle.
  `hidden` falls through to `false` unless a caret or a document app says otherwise.
- `foregroundHasCaret()` (`win32.ts:198-201`): `GetGUIThreadInfo(0 /* foreground thread */, &gti)` succeeds and
  `gti.hwndCaret != NULL`.
- `documentAppInFront()` (`foreground.ts:168-200`): the foreground window's process is in `DOCUMENT_APPS`:
  `winword.exe, acrord32.exe, acrobat.exe, foxitpdfreader.exe, foxitpdfeditor.exe, foxitreader.exe, pdfxedit.exe,
  sumatrapdf.exe, kindle.exe, ebook-viewer.exe, onenote.exe, outlook.exe, olk.exe, soffice.bin, wps.exe`. The result is
  cached by `(hwnd, pid)`, so the process query runs only when the foreground window changes. These readers may draw
  their own text cursor, so a drag there gets the benefit of the doubt.

### 11.6 Fullscreen / presentation (`fullscreenAppRunning`, `foreground.ts:224-231`)
`SHQueryUserNotificationState(&state)`. If it returns S_OK and the state is `QUNS_RUNNING_D3D_FULL_SCREEN (3)` or
`QUNS_PRESENTATION_MODE (4)`, it returns true. Any failure or a missing function gives false. It is **deliberately not
`QUNS_BUSY (2)`**: that also covers F11 browsers and fullscreen PDF / ebook readers, which are reading surfaces.

---

## 12. Win32 API catalogue (everything the subsystem calls)

Bound in `win32.ts:84-237` unless noted ("via hook lib" / "via Electron" means the original reached it indirectly; a
native port calls it directly).

| API | DLL | Purpose | Notes |
|---|---|---|---|
| `GetForegroundWindow` | user32 | Capture target; keyboard-hint target; document-app check | |
| `WindowFromPoint(POINT)` | user32 | Window under a physical point (gesture target, overlay test, chip hit test) | POINT passed by value; coordinates truncated to int |
| `GetAncestor(h, GA_ROOT=2 / GA_ROOTOWNER=3)` | user32 | Top-level window; owner root for captionless popups | Null result → use the input hwnd |
| `GetWindowTextW` | user32 | Title | Buffer 512 WCHAR; result length capped at 511 |
| `GetWindowThreadProcessId` | user32 | Window → pid | Thread id 0 → pid 0 → "gone" |
| `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION=0x1000, FALSE, pid)` | kernel32 | Handle for the image path query | Null → `''` |
| `QueryFullProcessImageNameW(h, 0, buf, &size)` | kernel32 | Full exe path → basename → lower-case | Buffer 1024 WCHAR; flags 0 = Win32 path format |
| `CloseHandle` | kernel32 | Close the process handle | Always, in `finally` |
| `GetClassNameW` | user32 | Window class (terminal / copy-on-select detection) | Buffer 256 WCHAR |
| `GetWindowLongW(h, GWL_EXSTYLE=-20)` | user32 | `WS_EX_NOACTIVATE` test | Treated as u32 |
| `GetCursorInfo(&CURSORINFO)` | user32 | Cursor showing / suppressed / handle | `cbSize` = 4+4+ptr+8 (24 on x64). Flags at offset 4, `hCursor` at offset 8 |
| `LoadCursorW(NULL, IDC_IBEAM=32513)` | user32 | The shared system I-beam handle to compare with | Loaded once |
| `GetGUIThreadInfo(0, &GUITHREADINFO)` | user32 | Caret in the foreground thread | `cbSize` = 8 + 6·ptr + 16 (72 on x64). `hwndCaret` at offset 8 + 5·ptr |
| `SHQueryUserNotificationState(&state)` | shell32 (optional) | Fullscreen D3D / presentation mode | HRESULT 0 = success |
| `GetAsyncKeyState(vk)` | user32 | Physically held modifiers (bit 0x8000) | Also used by libuiohook to seed its modifier mask |
| `GetClipboardSequenceNumber` | user32 | Detects that the copy landed / the clipboard changed | Never needs a clipboard lock |
| `IsClipboardFormatAvailable(fmt)` | user32 | Text format listed (delayed render); privacy markers present | No lock needed |
| `RegisterClipboardFormatW(name)` | user32 | Name → format id for markers and flags | Cache per name; 0 → "not available" |
| `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC=0)` | user32 | Scan code for injected keys | `& 0xFF` |
| `keybd_event(vk, scan, flags, 0)` | user32 | Inject chord / Escape / key-ups | Port: `SendInput` with `KEYBDINPUT{wVk, wScan, dwFlags}` is equivalent |
| `GetKeyboardLayoutList`, `MapVirtualKeyExW`, `ToUnicodeEx` | user32 | `altGrCharacter` (used by the hotkey subsystem, §12.1) | |
| `SetWindowsHookExW(WH_KEYBOARD_LL=13 / WH_MOUSE_LL=14)`, `CallNextHookEx`, `UnhookWindowsHookEx`, `GetMessageW` | user32 (via hook lib) | Global input | The hook thread needs a message loop |
| `GetDoubleClickTime` | user32 (via hook lib) | Multi-click counter window | Read on every press, so a change in Control Panel applies at once |
| `SetWinEventHook(EVENT_SYSTEM_FOREGROUND / EVENT_SYSTEM_MINIMIZEEND, WINEVENT_OUTOFCONTEXT)`, `SetTimer(83 ms)`, `RegisterWindowMessageW("UIOHOOK_UIPI_TEST")`, `PostMessageW` + `GetLastError == ERROR_ACCESS_DENIED` | user32 (via hook lib) | UIPI detection for the foreground window, to re-seed the modifier mask (§4) | Optional in a port that reads modifier state another way |
| `SystemParametersInfoW(SPI_GETWHEELSCROLLLINES / SPI_GETWHEELSCROLLCHARS)` | user32 (via hook lib) | Wheel amount (unused here); a failure drops the wheel event | Not needed in a port |
| `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)` | kernel32 (via hook lib) | Hook thread priority | |
| `SendInput` | user32 (via hook lib) | Fallback key injection (VK only) | |
| `OpenClipboard`, `EnumClipboardFormats`, `GetClipboardFormatNameW`, `GetClipboardData`, `EmptyClipboard`, `SetClipboardData`, `CloseClipboard`, `GlobalAlloc/Lock/Unlock/Size` | user32 / kernel32 (via Electron) | Snapshot / restore / read text / read flag DWORDs | See §9 |
| (Electron `screen.screenToDipPoint`) | — | Physical → DIP | Native: stay physical, or `MonitorFromPoint` + `GetDpiForMonitor` |

Constants (`win32.ts:50-76`): `WS_EX_NOACTIVATE=0x08000000`, `QUNS_RUNNING_D3D_FULL_SCREEN=3`,
`QUNS_PRESENTATION_MODE=4`, `CF_TEXT=1`, `CF_UNICODETEXT=13`, `PROCESS_QUERY_LIMITED_INFORMATION=0x1000`, `GA_ROOT=2`,
`GA_ROOTOWNER=3`, `KEYEVENTF_EXTENDEDKEY=1`, `KEYEVENTF_KEYUP=2`, `MAPVK_VK_TO_VSC=0`, `VK_SHIFT=0x10`,
`VK_CONTROL=0x11`, `VK_MENU=0x12`, `VK_LSHIFT=0xA0`, `VK_LCONTROL=0xA2`, `VK_RMENU=0xA5`,
`TOUNICODE_NO_STATE_CHANGE=0x4`, `GWL_EXSTYLE=-20`, `IDC_IBEAM=32513`, `CURSOR_SHOWING=1`, `CURSOR_SUPPRESSED=2`.

### 12.1 `altGrCharacter(vk, shift) → Option<String>` (`win32.ts:215-235`)
This lives in this file but serves the hotkey subsystem. Windows delivers AltGr as Ctrl+Alt, so a hotkey on a Ctrl+Alt
combination steals that character. Such hotkeys are skipped.
```
n = GetKeyboardLayoutList(0, NULL); if n <= 0: return None
hkls = GetKeyboardLayoutList(n, buf)  (use min(n, returned))
state[256] = 0; state[VK_CONTROL]=state[VK_MENU]=state[VK_LCONTROL]=state[VK_RMENU]=0x80
if shift: state[VK_SHIFT]=state[VK_LSHIFT]=0x80
for hkl in hkls:
    scan = MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, hkl)
    r = ToUnicodeEx(vk, scan, state, out[8 WCHAR], 8, TOUNICODE_NO_STATE_CHANGE /*Win10 1607+: don't touch dead-key state*/, hkl)
    if r < 0: return Some("")            // AltGr dead key
    if r == 0: continue
    text = out[..min(r,8)]
    if any code point >= 0x20 && != 0x7F: return Some(text)   // control chars (Ctrl+letter) are not typed characters
return None
```

---

## 13. Quirks and edge cases (decide deliberately whether to keep each)

1. **The hotkey clears the hint.** The read-selection hotkey's own keydown (e.g. `R` with Ctrl+Alt) passes through the
   hook as a non-neutral key, so it hides the chip and clears the hint. The capture does not depend on the hint, so this
   is harmless.
2. **Plain navigation keys don't clear the hint**, by design. Inside a context menu they navigate it. On non-editable
   pages they scroll without collapsing the selection.
3. **Numpad Insert** (NumLock off) is VC 0xEE52, not `Key.Insert`, so Ctrl+numpad-Ins clears the hint. **IME
   composition** keys (VK_PROCESSKEY) map to a neutral code and never clear the hint. **VK_LAUNCH_MAIL** maps to
   undefined (0) and does clear it. These are side effects of libuiohook's table (§4.3).
4. **Asymmetry with blocked windows.** A *mouse* gesture in a blocked window clears an existing hint (`blocked`). A
   *keyboard* selection in a blocked window leaves it alone. A *plain click* in a blocked window clears it (`click`), as
   anywhere else.
5. A **non-text drag** neither sets nor clears the hint, but it does reset multi-click tracking.
6. A **hook start failure is sticky**: there is no retry until restart, and `hookAvailable` stays false.
7. **`excludedApps` normalization** always forces a `.exe` suffix (`soffice.bin` can't be excluded).
8. An **exception inside `copyAndRead`** still runs `settleClipboard`, but its notice is lost and the outcome becomes
   `empty`, so `no-selection` is emitted.
9. **`restoreClipboard=false`** leaves the *captured text* on the clipboard. It does **not** stop restoring after a
   secret (restore, or clear if the snapshot failed), after we emptied the clipboard, or after a non-text copy replaced
   it.
10. **A late copy can produce `read` after `no-selection`** for the same trigger, up to about 2–3.5 s later. Its `source`
    is the original trigger's.
11. **Display changes**: the shell hides the chip window directly on a display being removed or its metrics changing
    (not on a display being added), without telling the machine (`src/main/index.ts:367-373`). The machine's `chip`
    lingers until its timeout or the next event emits a redundant `hide-chip`. That is harmless because the chip
    bounds are already null.
12. **The capture ignores the hint.** The hotkey copies whatever is selected in the foreground app; there is no
    "no hint → no capture" rule.
13. **Mouse events are still processed during a capture**; only keydowns are suppressed (to ignore Vox's own injected
    keys). libuiohook does not filter injected input.
14. **`readSelection` while busy** returns silently, so a second chip click during a capture gives no feedback.
15. **The selection chip shows at the release point; the context chip at the right-press point.** The window layer then
    offsets them: the context chip beside the predicted native menu, the selection chip above-right of the cursor
    (`src/main/windows.ts:135-235`).
16. **Any clipboard write in the 1.5 s after a failed capture is taken for the late copy** (§8.8). If the user copies
    something themselves in that window, Vox reads it aloud as the late copy, then (with `restoreClipboard`) puts the
    old snapshot back 150 ms later. The user's own copy is lost, because the `ifStill` check sees exactly that text.
    If instead a new capture starts or Vox quits in that window, the flush puts the old snapshot back whenever the
    sequence moved, without checking the content.
17. **Copy-on-select after a failed capture.** The capture's step-1 flush (§8.3) can put an old snapshot over a PuTTY
    selection made within 1.5 s of an earlier failed capture. The copy-on-select read then returns the old clipboard
    text.
18. **Elevated foreground apps** (UIPI, §4): synthetic keys are silently dropped, so the capture times out and the user
    sees "Select some text first". Gestures there are expected to go unseen as well.
19. **Keydowns during a capture** are ignored by the machine, both Vox's injected ones and the user's own (§4.1).
20. **`lossNotice` wording**: any skipped format (unreadable, out of time budget or too large) produces
    "Couldn't restore your previous clipboard (it was too large)" (§9.1).

---

## 14. Native implementation notes (Rust, Windows)

- **Hook thread**: a dedicated thread installs `WH_MOUSE_LL` and `WH_KEYBOARD_LL` and pumps `GetMessageW`. The hook
  procedure must return fast, so copy `(msg, pt, mouseData, time, vkCode, scanCode, flags)` into a channel and
  `CallNextHookEx`. Windows silently removes LL hooks that exceed `LowLevelHooksTimeout`, with no notification on
  Windows 7+. Don't do `OpenProcess`, `WindowFromPoint` or similar inside the procedure.
- **Move filtering**: keep an `AtomicBool wants_mouse_move` that the hook procedure checks before posting
  `WM_MOUSEMOVE`. Otherwise you flood the channel with every move.
- **Consumer thread**: runs the pure `GestureMachine`, the dependency queries (`GetCursorInfo` and `GetGUIThreadInfo(0)`
  are global and work from any thread) and the single deadline timer. Use `Instant` milliseconds for `time`.
- **Click counter**: reproduce libuiohook's (§4.1). Simplification: with `lastLeftDown` non-null, the previous press
  was a left press, so
  `multi = lastLeftDown && dist <= 4 && (t - lastLeftDown.at) <= max(500, GetDoubleClickTime())`.
  It differs only when a wheel event came between the clicks.
- **Modifier flags on events**: track the side-specific VKs from the keyboard hook (as libuiohook does, seeded with
  `GetAsyncKeyState`), or query `GetAsyncKeyState` on the consumer. For the pre-copy wait, use `GetAsyncKeyState` (it
  is authoritative). If you track them in the hook, also re-seed from `GetAsyncKeyState` after the foreground changes
  (`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` on the hook thread). libuiohook does this only when the new foreground is
  UIPI-blocked, but doing it on every change is simpler and safe (§4).
- **Key classification by VK**: modifiers are 0xA0-0xA5, 0x5B, 0x5C **and** the generic 0x10-0x12. Navigation is
  VK_PRIOR, NEXT, END, HOME, LEFT, UP, RIGHT and DOWN (0x21-0x28), with or without `LLKHF_EXTENDED`. "Insert" for the
  copy rule is VK_INSERT 0x2D **with** `LLKHF_EXTENDED`. A is 0x41 and C is 0x43. The neutral VK list is in §4.3.
  `keydown` covers `WM_KEYDOWN` and `WM_SYSKEYDOWN`; `wheel` covers `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL`.
- **Synthetic keys**: the original sends 4 separate `keybd_event` calls. One `SendInput` with 4 `KEYBDINPUT`s is
  equivalent and atomic: LCtrl 0xA2 down, then C 0x43 or Insert 0x2D (`KEYEVENTF_EXTENDEDKEY`), key up, LCtrl up.
  Set `wScan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) & 0xFF`. `SendInput` returns how many events it inserted. If that
  is fewer than 4, send the missing key-ups (key, then LCtrl) in a second call, to keep the "never leave Ctrl stuck"
  guarantee. UIPI blocking is **not** reported by `SendInput`. To detect an elevated foreground, use libuiohook's trick
  (`PostMessageW(fg, RegisterWindowMessageW(...), 0, 0)` failing with `ERROR_ACCESS_DENIED`), or compare token
  integrity levels.
- **Injected keys**: the faithful approach is to skip keydowns while `capturing`. Optionally also tag your
  `SendInput` with a magic `dwExtraInfo` and drop exactly those in the hook. Don't drop all `LLKHF_INJECTED` events,
  because on-screen keyboards and AutoHotkey users need them.
- **Clipboard**: own a message-only window (`HWND_MESSAGE`) for `OpenClipboard`. Do the snapshot, restore and polling
  on a worker thread; `GetClipboardData` can block on delayed rendering. Use raw HGLOBAL bytes for every format (§9.1)
  instead of Electron's MIME mapping. Skip a synthesized format **only when its source format is present**:
  CF_TEXT and CF_OEMTEXT when CF_UNICODETEXT is listed; CF_BITMAP and the other DIB format when CF_DIB or CF_DIBV5 is.
  An ANSI-only clipboard must keep CF_TEXT. Never hold the clipboard open across an `.await` or a sleep (other apps,
  including the one answering our copy, are blocked meanwhile). Open, read or write, and close within one synchronous
  block on the worker, and release the lock between poll iterations.
- **Overlay windows** (chip, widget): `WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`. Show them with
  `SW_SHOWNOACTIVATE` / `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)`. Re-assert topmost after showing so they sit above
  context menus (Electron used the "pop-up-menu" level and `moveTop`). During the hide animation, make the window
  click-through (`WS_EX_TRANSPARENT | WS_EX_LAYERED`) and report null bounds.
- **DPI**: declare per-monitor-v2 awareness. Hook points, `WindowFromPoint` and `GetWindowRect` are then all in
  physical pixels, and §6.1's DIP conversion is unnecessary.
- **windows crate features**: `Win32_Foundation`, `Win32_UI_WindowsAndMessaging`, `Win32_UI_Input_KeyboardAndMouse`
  (`GetDoubleClickTime` also lives here), `Win32_System_DataExchange`, `Win32_System_Memory`, `Win32_System_Ole`
  (CF_* constants), `Win32_System_Threading`, `Win32_UI_Shell`, `Win32_UI_Accessibility` (`SetWinEventHook`, if you
  re-seed modifiers on foreground changes), `Win32_Security` (only if you compare token integrity levels),
  `Win32_Graphics_Gdi`, `Win32_UI_HiDpi`, `Win32_UI_TextServices` (HKL). Check docs.rs for the exact module of each
  function.
- **Panics** must never cross the hook FFI boundary. Wrap the hook procedure body in `catch_unwind` and throttle error
  logs to once per 10 s, as the original does.
