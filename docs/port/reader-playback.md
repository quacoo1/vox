# Vox port spec: read lifecycle, player state machine, widget playback

Everything a Rust rebuild needs to reproduce **how a read starts, plays, stops, replays and is accounted for**. This
covers the main-process `ReadController`, the playback engine that the floating widget owns (`AudioQueuePlayer`), its
timing helpers, the audio-output warmer, the widget controller (auto-hide, settings), media-session / media keys, the
UI presentation rules derived from player state (status labels, `≈` time-left, speed cycle, word highlighting), and
the listening ledger used for "Heard %" and per-day analytics.

Source files (paths relative to the original repo root):

| File | Role |
|---|---|
| `src/main/reader.ts` | `ReadController`: every read entry point, history/cache bookkeeping, state fold-back |
| `src/renderer/src/widget/player.ts` | `AudioQueuePlayer`: chunk queue, status machine, timeline maths, listening clock |
| `src/renderer/src/widget/timing.ts` | Pure helpers: alignment lookups, sentence/word boundaries |
| `src/renderer/src/widget/controller.ts` | Wires player to IPC: session events, commands, settings, auto-hide |
| `src/renderer/src/widget/output-warmer.ts` | Keeps the audio device awake so the first words are not clipped |
| `src/renderer/src/widget/useMediaSession.ts` | Windows media overlay / hardware media keys |
| `src/renderer/src/widget/usePlayerSnapshot.ts` | UI refresh policy |
| `src/shared/listen-log.ts` | Hour-bucketed listening ledger |

Context files consulted (not specified here, only their contracts): `src/main/tts/engine.ts` (TtsEngine),
`src/main/services/audio-cache.ts`, `src/main/services/history-store.ts`, `src/main/services/analytics.ts`,
`src/main/windows.ts`, `src/main/ipc.ts`, `src/main/index.ts`, `src/main/tray.ts`, `src/shared/types.ts`,
`src/shared/constants.ts`, `src/shared/defaults.ts`, `src/renderer/src/widget/Widget.tsx`,
`src/renderer/src/widget/WidgetView.tsx`, `src/renderer/src/widget/SentenceText.tsx`,
`src/renderer/src/widget/main.tsx`, `src/renderer/src/widget/Widget.css`, `src/renderer/src/lib/format.ts`,
`src/main/tts/mock.ts`, `src/main/integrations/local-server.ts`, `src/renderer/src/app/pages/HistoryModel.ts`.

Process wiring (`widget/main.tsx`): the player and controller are created once per widget page load, outside the UI
tree, with `report = state → send player:state to main`; the controller is disposed on page unload. A native port has
the same shape: one player instance for the app's life, created before any read.

---

## 0. Conventions that matter for a port

- **All text offsets are UTF-16 code-unit indices** (JavaScript string indices): `textStart`, `textEnd`, `charIndex`,
  `totalChars`, sentence starts, word spans, `text.maxChars`, `PREVIEW_CHARS`, `TITLE_CHARS`, `ReadRecord.charCount`.
  The audio-cache manifests on disk store these UTF-16 offsets. A Rust port must either keep text as UTF-16
  (`Vec<u16>`) for offset maths, or convert consistently at every boundary (and migrate caches if it reads the
  Electron app's `userData`). "Never split a surrogate pair" rules below exist because of this.
- Times: player/audio maths in **seconds (f64)**; timers and wall clock in **milliseconds**. "Audio seconds" (position
  in the synthesized audio, independent of playback rate) and "wall seconds" (real time spent listening) are different
  quantities: `positionSec`, `durationSec`, `maxPositionSec`, `audioDurationSec` are audio seconds; `listenedSec` is
  wall seconds.
- `round1(n) = Math.round(n * 10) / 10` (values are non-negative, so Rust `(n * 10.0).round() / 10.0` matches).
- `clampNum(n, min, max)`: **NaN returns `min`**, otherwise `min(max, max(min, n))` (`timing.ts:4-7`).
- JS `\s` / `\S` (used in several regexes) and JS `trim()`/`trimEnd()` use the same set:
  `[\t\n\v\f\r \u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF]`.
  Rust `char::is_whitespace`, `str::trim` and the `regex` crate's `\s` use Unicode `White_Space`, which **includes
  U+0085 and excludes U+FEFF**. Write a `js_ws(u16) -> bool` helper and use it everywhere this spec says "whitespace".
- **Lone surrogates.** `truncateText` and `preCut` never split a surrogate pair, but several other cuts do not guard:
  the 160-unit record preview (`text.slice(0, 160)`, `reader.ts:342`), `previewTitle`'s 64-unit cut
  (`reader.ts:132`), the player's title fallback `text.slice(0, 120)` (`player.ts:203`) and the tray tooltip. JS
  strings tolerate a trailing lone high surrogate; V8's `JSON.stringify` writes it as the escape `"\ud83d"` into
  `history.json`. A Rust `String` cannot hold one, and `serde_json` rejects an unpaired `\uD800`-`\uDBFF` escape
  when parsing, so a Rust reader of old `history.json` files must tolerate (or repair) it, and Rust cuts should drop
  a trailing high surrogate the way `truncateText` does.
- `ESTIMATED_CHARS_PER_SECOND = 15` (`src/shared/constants.ts:17`) is the speech-rate guess used everywhere before
  real audio durations are known.
- `APP_NAME = 'Vox'`, `APP_ID = 'com.vox.reader'` (AppUserModelID, set at startup), `READ_WITH_LABEL = 'Read with Vox'`
  (`src/shared/constants.ts:2-4`).
- Non-ASCII characters in copy strings below are exact code points: `…` U+2026, `≈` U+2248, `−` U+2212 (minus),
  `×` U+00D7, `·` U+00B7, `“` `”` U+201C/U+201D, `’` U+2019, `—` U+2014, `–` U+2013, `→` (docs only).

---

## 1. Architecture and message flow

```
 triggers (hotkeys, chips, tray, CLI, browser bridge, app UI)
            │
            ▼
   ReadController (main)  ──start/playCached/stop/progress──▶  TtsEngine (main)
     │  ▲   │                                                       │
     │  │   └─ history store / audio cache                         │ sink: sessionStart, chunk, done, error
     │  │                                                           ▼
     │  └──── player:state (PlayerState) ─────────────────  AudioQueuePlayer (widget process, owns audio)
     │                                                              ▲
     └──── player:command (PlayerCommand) ─────────────────────────┘
```

- **The widget owns playback.** Main never touches audio. Main sends session events and commands; the player reports
  `PlayerState` back. Main folds those reports into history, feeds the playhead to the engine (bounded look-ahead),
  relays them to the app window and the tray.
- The widget window exists for the whole app lifetime, created hidden; audio plays whether or not it is visible
  (`backgroundThrottling: false`, `autoplayPolicy: 'no-user-gesture-required'`, `windows.ts:600-622`). A native port
  should put the player in its own always-alive component (thread/actor), independent of any window.
- Widget-local UI actions (its buttons, media keys) call the player **directly**; main only learns about them via
  state reports. The single exception is close/stop, which also tells main to stop synthesis.
- Main's "is the widget listening yet" gate: `whenWidgetReady(timeoutMs = 5000)` resolves on the first `player:state`
  report or first size report from the widget, or 1500 ms after the widget page finished loading, or after the
  timeout (`windows.ts:636-653`, `ipc.ts:356-364`). Every read/replay awaits it before starting the engine so the
  session-start event is not lost. In a single-process Rust port the player can exist before any read, making this
  unnecessary (but keep the ordering guarantee: the player must receive `sessionStart` before chunks).
  Once resolved, the gate stays resolved for the rest of the app's life (`markWidgetReady` nulls the resolver,
  `windows.ts:644-653`): a later renderer reload does **not** re-arm it. The widget controller's startup report
  (§7) is what normally resolves it.
- Main → widget messages are fire-and-forget (`windows.ts:823-834`): if the widget renderer is gone or reloading, the
  session start, chunks, done, errors and commands sent meanwhile are silently lost.
- Widget renderer crash recovery (`windows.ts:396-415`): on `render-process-gone` (unless quitting, the reason is
  `clean-exit`, or the window is destroyed) the page is reloaded, at most `MAX_CRASH_RELOADS = 3` times within a
  sliding `CRASH_WINDOW_MS = 60 000` ms; beyond that it stays dead (logged "renderer keeps crashing"). The reloaded
  player starts `idle` and reports that once (§7), but main's engine session is **not** stopped: synthesis continues
  until the look-ahead stalls (no more position reports), and `engine.currentSessionId` stays set, so the play/pause
  hotkey sends `toggle` to an idle player (a dead key) until something stops the engine.
- The widget window is app-lifetime and never recreated. A close request to it (system menu, an outside `WM_CLOSE`)
  quits the whole app instead of destroying it (`windows.ts:430-444`, `index.ts:458-461`: the synchronous
  session-end save of §2.12 first, then the normal quit), because reads would otherwise keep being synthesized and
  billed into a window that no longer exists.
- Main keeps the last reported `PlayerState` (`IDLE_PLAYER_STATE` until the first report); the app window reads it via
  `player:get-state` on load and then follows the relayed reports.

### 1.1 Channel payloads (contracts)

```ts
// main → player
TtsSessionStart {
  sessionId: string            // UUID per session (not per read)
  readId: string               // history record id
  text: string                 // full cleaned text being read (session text)
  title: string                // previewTitle(text) — one-line preview
  voiceName: string
  sourceApp?: string           // e.g. "Chrome"
  estimatedDurationSec: number // see §3; player falls back to text.length/15 when not finite or <= 0
  chunkCount: number           // informational; player does not use it
  rate: number                 // settings.playbackRate at start
  volume: number               // settings.volume at start
  highlightWords: boolean      // settings.widget.highlightWords at start
  fromCache: boolean           // cached replay
}
TtsChunk {
  sessionId: string
  index: number                // 0,1,2… delivered in order (player still reorders defensively)
  textStart: number            // offset into TtsSessionStart.text (UTF-16)
  textEnd: number              // exclusive
  audio: bytes                 // complete encoded file: MP3 (ElevenLabs) or WAV (mock)
  mime: string                 // 'audio/mpeg' | 'audio/wav'
  alignment: Alignment | null  // per-character timing relative to the chunk's audio start
  durationSec: number          // best known duration (engine estimate, see §3; player refines it from the decoder)
}
Alignment { chars: string[]; starts: number[]; ends: number[] }   // seconds
// Playback reads ONLY alignment.starts (charAtTime/timeAtChar, §5). `chars` and `ends` are never read by the player;
// `ends` matters only to the engine's duration rule (§3) and to clipAlignment (§2.7). For records whose text is
// not stored, cached `chars` are all '' (same length as starts).
TtsSessionDone  { sessionId: string; totalChunks: number }
TtsSessionError { sessionId: string; message: string; code: ReadErrorCode; fatal: boolean }

// player → main (also relayed main → app window)
PlayerState {
  status: 'idle' | 'loading' | 'playing' | 'paused' | 'ended' | 'error'
  sessionId: string | null
  readId: string | null
  title: string
  voiceName: string
  sourceApp?: string
  positionSec: number          // audio seconds
  durationSec: number          // audio seconds, estimate until synthesis is done
  durationIsEstimate: boolean
  bufferedFraction: number     // 0..1 synthesized share of the (estimated) total
  rate: number
  volume: number
  charIndex: number            // current char offset into session text
  totalChars: number           // session text length
  listenedSec: number          // wall seconds actually listened in THIS session
  error?: string
  errorCode?: ReadErrorCode
}
IDLE_PLAYER_STATE = { status:'idle', sessionId:null, readId:null, title:'', voiceName:'', positionSec:0,
  durationSec:0, durationIsEstimate:true, bufferedFraction:0, rate:1, volume:1, charIndex:0, totalChars:0,
  listenedSec:0 }

// anyone → main → player
PlayerCommand =
  | { type: 'toggle' } | { type: 'play' } | { type: 'pause' } | { type: 'stop' }
  | { type: 'seek'; positionSec: number }
  | { type: 'seekFraction'; fraction: number }
  | { type: 'skip'; direction: 1 | -1; unit: 'sentence' | 'seconds'; amount?: number }
  | { type: 'rate'; rate: number }
  | { type: 'volume'; volume: number }

ReadErrorCode = 'no-api-key' | 'unauthorized' | 'quota' | 'rate-limit' | 'network' | 'empty-text'
              | 'no-selection' | 'too-long' | 'not-cached' | 'unknown'
ReadResult = { ok: boolean; readId?: string; error?: string; code?: ReadErrorCode }
Toast = { id: string /*UUID*/; kind: 'info' | 'success' | 'error'; message: string;
          action?: { label: string; route?: { page: AppPage; section?: string } } }
```

Validation main applies to inbound commands and states (`ipc.ts:118-172`; the browser bridge has a near-identical
command parser that answers HTTP 400 instead of ignoring, `integrations/local-server.ts:318-350`, whose result is then
passed through the IPC parser again). One difference: over IPC a non-finite `skip.amount` is silently dropped (the
command still runs with the 10 s default); over the bridge an `amount` that is present but not a finite number is a
400 (`"amount" must be a finite number`). An unknown command type is ignored over IPC and a 400
`Unknown command type` over the bridge.

| Field | Rule |
|---|---|
| `seek.positionSec` | finite, then `max(0, v)` |
| `seekFraction.fraction` | finite, clamp 0..1 |
| `skip.direction` | exactly 1 or -1; `unit` exactly `'sentence'`/`'seconds'`; `amount` optional, finite, clamp 0..3600 (an `amount` of 0 reaches the player, which treats any non-positive amount as the 10 s default) |
| `rate.rate` | finite, clamp 0.5..3 |
| `volume.volume` | finite, clamp 0..1 |
| PlayerState | `status` must be valid else whole report dropped; non-finite numbers fall back to `IDLE_PLAYER_STATE`; `positionSec`,`durationSec`,`charIndex`,`totalChars`,`listenedSec` ≥ 0; `bufferedFraction` clamp 0..1; `rate`/`volume` only finite-checked (not clamped); `sessionId`/`readId` any string else null; `title` ≤ 300 chars, `voiceName`/`sourceApp` ≤ 120, `error` ≤ 500 (empty → absent; strings are cut, not trimmed); `durationIsEstimate` non-boolean → true; `errorCode` kept only if a known code. Accepted only from the widget sender; every accepted-or-not report from the widget also resolves the widget-ready gate. |

---

## 2. ReadController (main) — `src/main/reader.ts`

### 2.1 Constants

| Name | Value | Meaning (source) |
|---|---|---|
| `PREVIEW_CHARS` | 160 | Characters of text kept on the record when `library.storeText` is off (`reader.ts:40`) |
| `HISTORY_WRITE_INTERVAL_MS` | 2000 | Min interval between history writes of playback progress when status did not change (`:41`) |
| `TITLE_CHARS` | 64 | Max length of `previewTitle` (`:42`) |
| `RAW_CUT_FACTOR` / `RAW_CUT_MIN` | 4 / 200 000 | Raw input pre-cut to `max(maxChars × 4, 200 000)` before cleaning (`:47-48`) |
| `PARTIAL_READ_ERROR` | `'Only part of the text could be synthesized'` | Record error when a later chunk failed (`:50`) |
| `CACHE_MB` | 1 000 000 | Cache limit unit: decimal MB (`:169`) |
| `HIDDEN_MARK` | `'…'` (U+2026, 1 UTF-16 unit) | Stand-in for text not stored (`:179`) |
| `FINAL_STATUSES` | `ended`, `idle`, `error` | Only used for a debug log (`:251`) |
| `MAX_TRACKED_SESSIONS` | 16 | Tracked sessions kept, oldest evicted first (`:252`) |
| `MAX_READ_TEXT` (ipc) | 2 000 000 | App-sent text is sliced to this before `read()` (`ipc.ts:100`) |

Relevant settings (defaults from `src/shared/defaults.ts`; clamps from `settings-store.ts`):

| Setting | Default | Clamp | Used for |
|---|---|---|---|
| `text.maxChars` | 20 000 | 100..100 000 | truncation |
| `library.saveHistory` | true | | create a record at all |
| `library.storeText` | true | | full text vs 160-char preview |
| `library.cacheAudio` | true | | cache synthesized audio on complete |
| `library.cacheLimitMB` | 500 | 50..10 000 | LRU prune limit |
| `widget.enabled` | true | | show widget on read; toast routing |
| `widget.autoHide` | true | | hide 4 s after `ended` |
| `widget.highlightWords` | true | | word highlighting |
| `widget.compact` | false | | compact pill layout |
| `playbackRate` | 1 | 0.5..3 | client-side rate |
| `volume` | 1 | 0..1 | |
| `triggers.mediaKeys` | true | | media session / media keys |
| `triggers.hotkeysEnabled`, `triggers.hotkeys.readSelection` | true, `'Ctrl+Alt+R'` | | hint in "Nothing is playing" toast |
| `voice.speed` | 1 | 0.7..1.2 | ElevenLabs server speed; divides duration estimates |

### 2.2 Entry points (every way a read starts or playback is driven)

| Trigger | Call | `source` | Extra metadata |
|---|---|---|---|
| Global hotkey "read selection" (default Ctrl+Alt+R) | selection service captures text → `read()` | `'hotkey'` | `sourceApp` = foreground app friendly name, `sourceProcess` = lower-case exe, `sourceTitle` = foreground window title (`index.ts:338-348`) |
| Right-click "Read with Vox" chip | same path | `'context-chip'` | same |
| PopClip-style selection chip | same path | `'selection-popup'` | same |
| Selection capture found nothing | toast info `'Select some text first'` | | (`index.ts:349`) |
| Selection refused (secrets, password managers…) | toast with the notice's kind/message | | (`index.ts:351`) |
| Hotkey "read clipboard" (default Ctrl+Alt+V), tray "Read clipboard", app button, CLI `--read-clipboard` | `readClipboard()` | `'clipboard'` | none |
| Hotkey play/pause (default Ctrl+Alt+P) | `togglePlayback()` | | |
| Hotkey stop (default Ctrl+Alt+S), tray "Stop" | `command({type:'stop'})` | | |
| Tray "Pause"/"Resume" | `command({type:'toggle'})` | | |
| Browser extension `POST /read {text, url?, title?}` | `read()` | `'browser-extension'` | `sourceApp` = foreground app name **only if** the foreground process is a known browser (`chrome.exe, msedge.exe, brave.exe, opera.exe, vivaldi.exe, arc.exe, chromium.exe, yandex.exe, firefox.exe`), else `'Browser'`; `sourceProcess` likewise; `sourceTitle` = page title; `sourceUrl` only if http(s) (`index.ts:213-245`) |
| Browser extension `POST /command` | `command(parsed)` | | |
| Browser extension `GET /health` | reads `playerState.status === 'playing'` | | |
| CLI `--read-text <t>` | `read()` | `'cli'` | none |
| CLI `--read-file <path>` (also the Explorer context-menu verb) | `readTextFile` → `read()` | `'explorer'` | `sourceApp:'Explorer'`, `sourceProcess:'explorer.exe'`, `sourceTitle:` file basename |
| App window "read text" (IPC `reader:read-text`) | `read()` | `opts.source` if a valid `ReadSource`, else `'manual'` | `sourceApp` ≤120, `sourceTitle` ≤300, `sourceUrl` if http(s) (`ipc.ts:336-346`) |
| App window replay from history (IPC `reader:replay`, id ≤128 chars) | `replay(readId)` | (`'replay'` only if re-synthesized) | |
| App window transport (Now Playing card, mini player, Settings → Playback rate/volume) | IPC `player:command` → `command()` | | |
| Widget "Try again" on the error card | `replay(readId)`; if that returns `ok:false` with code `unknown` or `not-cached`, falls back to `readText(session.text, {sourceApp})` (source then defaults to `'manual'`) (`Widget.tsx:179-189`). Failed reads are never cached, so with history on this re-synthesizes as a **new** record with `source:'replay'` (the failed record keeps its `error`); with history off `replay` answers `unknown` and the session text is read again as `'manual'`; for a preview-only record `not-cached` triggers the same fallback with the full session text. Any other failure (e.g. `no-api-key`) is left to `read()`'s own toast. | | |
| Widget buttons / media keys | act on the player directly (§5) | | |
| Widget close button / media "stop" | `player.stop()` + `command({type:'stop'})` to main + hide window | | |
| App quit, Windows log-off/shutdown | `persistPlayback()`, then `shutdown()` on quit | | |
| Cache limit lowered in Settings | `pruneCache()` | | (`index.ts:316-323`) |

`ReadRequest`: `{ text, source, sourceApp?, sourceProcess?, sourceTitle?, sourceUrl?, voice?: {id, name} }`. `voice`
overrides the configured voice (used only by replays that must re-synthesize).

### 2.3 `read(req)` — `reader.ts:305-387`

1. `settings = current settings`.
2. **API key gate** (skipped in mock-TTS mode): no stored key →
   toast `{kind:'error', message:'Add your ElevenLabs API key to start reading', action:{label:'Add key', route:{page:'settings', section:'account'}}}`;
   return `{ok:false, code:'no-api-key', error:'No ElevenLabs API key'}`. No record is created.
3. `raw = preCut(req.text ?? '', max(settings.text.maxChars * 4, 200000))` (§2.4).
4. `cleaned = cleanText(raw.text, settings.text)` (text-cleaning subsystem: strips invisible chars, normalises
   whitespace, de-hyphenates wraps, removes URLs/emails per settings). If `cleanText` throws, log and use
   `raw.text.trim()`.
5. Empty `cleaned` → toast `{kind:'info', message:'Nothing to read in that selection'}`; return
   `{ok:false, code:'empty-text', error:'Nothing to read'}`.
6. `cut = truncateText(cleaned, settings.text.maxChars)`; `text = cut.text`. If `cut.truncated || raw.cut` → toast
   info ``Reading the first ${maxChars formatted en-US} characters`` (e.g. `Reading the first 20,000 characters`).
   This toast is sent *before* the session starts (the widget shows it as a banner once the player card is up).
7. `readId = random UUID v4`. `voiceId = req.voice?.id || settings.voiceId`; `voiceName = req.voice?.name || settings.voiceName`.
8. Build the record:

   | Field | Value |
   |---|---|
   | `id` | `readId` |
   | `createdAt` | now (epoch ms) |
   | `text` | `storeText ? text : text.slice(0, 160)` |
   | `textTruncated` | `!storeText && text.length > 160` |
   | `charCount` | `text.length` (full text, UTF-16 units) |
   | `wordCount` | `countWords(text)` (full text) |
   | `voiceId`, `voiceName` | from step 7 |
   | `modelId` | `settings.modelId` |
   | `source` | `req.source` |
   | `sourceApp`, `sourceProcess`, `sourceUrl` | request value, empty string → absent |
   | `sourceTitle` | `req.sourceTitle.trim().slice(0, 200)`, absent if not given |
   | `audioDurationSec`, `listenedSec`, `charactersBilled` | 0 |
   | `completed`, `cached`, `favorite` | false |
   | `maxPositionSec`, `listenLog`, `error` | absent (omitted from JSON) |

   The history store normalises every record it stores (`history-store.ts:52-97`, other subsystem): the optional
   strings `sourceApp`, `sourceProcess`, `sourceTitle`, `sourceUrl`, `error` are trimmed and dropped when empty
   (so a whitespace-only title ends up absent), `sourceProcess` is lower-cased, numbers are clamped to ≥ 0 and
   `charCount`/`wordCount`/`charactersBilled` rounded, `maxPositionSec` of 0 is omitted, `listenLog` goes through
   `sanitizeListenLog` (§11). An update whose normalised result equals the stored record is a no-op (no write, no
   change notification).

9. `recordSaved = settings.library.saveHistory`; if true, add the record to history. The record exists **before**
   the engine starts: if `engine.start` then throws, the record stays, carrying the error (step 11).
10. Await widget readiness (§1).
11. `sessionId = engine.start({readId, text, title: previewTitle(text), voiceId, voiceName, sourceApp: req.sourceApp, settings})`.
    `engine.start` cancels any previous session (that one's player gets superseded by the new `sessionStart`).
    If `start` throws: `message = error message`; if `recordSaved` set `record.error = message`; toast
    `{kind:'error', message: errorMessage('unknown', message)}` (i.e. the thrown message, or
    `Something went wrong while reading` if it is blank); return `{ok:false, readId, code:'unknown', error: message}`.
    The widget is not shown in this case.
12. `track(sessionId, {readId, recordSaved, fromCache:false, baseListenedSec:0, baseCompleted:false})` (§2.12).
    Tracking happens even when `recordSaved` is false (so the engine's `stopped`/partial bookkeeping still finds
    the session; every history write is then skipped).
    **Ordering requirement:** the tracked session must exist before any state report for it can be processed. In
    Electron this holds because `start()` and `track()` run synchronously in one turn; in a threaded Rust port register
    the session before releasing the session-start event, or buffer early reports.
13. If `settings.widget.enabled` → show the widget (§8.1).
14. Return `{ok:true, readId}`.

### 2.4 Text helpers — `reader.ts:107-139`

**`preCut(text, max)`** — cut unbounded raw input before cleaning:
```
if len(text) <= max: return (text, cut=false)
cut = text[0:max]
nl = last index of '\n' in cut
if nl > max * 0.9: cut = cut[0:nl]          // at the last line break in the final tenth
if last unit of cut is a high surrogate (0xD800..0xDBFF): drop it
return (cut, cut=true)                       // no trimming
```

**`truncateText(text, max)`** — final cut to `maxChars`:
```
if len(text) <= max: return (text, truncated=false)
cut = text[0:max(0,max)]
lastSpace = index of match of /\s\S*$/ in cut   // the last whitespace char
if lastSpace > max * 0.9: cut = cut[0:lastSpace]
if last unit is a high surrogate: drop it
return (trimEnd(cut), truncated=true)
```

**`previewTitle(text, max = 64)`** — one-line title for the player, tray tooltip, media overlay:
```
flat = text.replace(/\s+/g, ' ').trim()
if len(flat) <= max: return flat
cut = flat[0:max]
space = last index of ' ' in cut
base = space > max * 0.6 ? cut[0:space] : cut       // 0.6 × 64 = 38.4
return base.replace(/[\s,.;:\-–—]+$/, '') + '…'
```
The 64-unit cut is not surrogate-safe (see §0). A text that is all punctuation past the cut yields just `…`.

**`countWords(text)`** = number of matches of `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu` (a word starts with a letter or
digit and continues with letters, digits, `'`, `’`, `-`).

### 2.5 `readClipboard()` — `reader.ts:389-401`

Read clipboard text (errors → empty string). If `text.trim()` is empty → toast info `'Copy some text first'`; return
`{ok:false, code:'empty-text', error:'The clipboard has no text'}`. Otherwise `read({text, source:'clipboard'})` (no
source app/title).

### 2.6 `replay(readId)` — `reader.ts:403-452`

1. `rec = history.get(readId)`; missing → return `{ok:false, code:'unknown', error:'That read is no longer in your history'}`
   (no toast; note: no `readId` in the result — the app uses that to tell "record vanished" apart from a failed re-read).
2. **Cache hit** (`audioCache.has(readId)`, an in-memory index lookup): load the chunks (I/O errors are logged and
   treated as a miss; a load that returns **zero** chunks is also a miss and falls through to step 3, including its
   paid re-synthesis). If at least one chunk:
   - `session = rec.textTruncated ? previewReplay(rec.text, chunks) : {text: rec.text, chunks}` (§2.7).
   - Await widget readiness; `sessionId = engine.playCached({readId, text: session.text, title: previewTitle(rec.text), voiceName: rec.voiceName, sourceApp: rec.sourceApp, settings, chunks: session.chunks})`.
   - `track(sessionId, {readId, recordSaved:true, fromCache:true, baseListenedSec: rec.listenedSec, baseCompleted: rec.completed})`.
     The replay **accumulates into the original record** (no new record, no API key needed, no credits; the API-key
     gate of `read()` is not consulted on this path).
   - The title comes from `rec.text` (for a preview-only record: the 160-unit preview, not the rebuilt text). The
     record's `sourceApp` is passed on, so the widget's source label shows the original app (e.g. `CHROME`), and
     `REPLAY` only when the record has no `sourceApp` (§8.2).
   - Show the widget if enabled; return `{ok:true, readId}`. No truncation toast (cached text is not re-cut).
3. **Cache miss**: if `rec.cached` is true, set it false on the record (the flag was stale).
   - If `rec.textTruncated` (only a preview was stored) → return
     `{ok:false, readId, code:'not-cached', error:'The audio is no longer cached and the full text was not saved'}` (no toast; the app shows it).
   - Else re-synthesize: `read({text: rec.text, source:'replay', sourceApp, sourceProcess, sourceTitle, sourceUrl, voice:{id: rec.voiceId, name: rec.voiceName}})`.
     This creates a **new record** (new id, `source:'replay'`), bills credits, uses the original voice but the
     *current* model and other settings. The text goes through clean/truncate again (idempotent in practice), and
     through every gate and toast of `read()` (no API key → `no-api-key` + its toast; a lowered `maxChars` →
     truncation toast). The original record is left as it was (apart from `cached = false`).

### 2.7 Cached replay of a preview-only record — `previewReplay` / `clipAlignment` (`reader.ts:187-226`)

When `storeText` was off, the record keeps only the first 160 chars, but the cached chunks index the full text. The
session text is rebuilt so every offset lies inside it and the widget can still highlight and skip:

```
previewReplay(preview, cached):
  L = len(preview); text = preview
  for c in cached sorted by index:
    start = max(0, floor(c.textStart) or 0)
    end   = max(start, floor(c.textEnd) or 0)
    if end <= L:            keep c unchanged
    elif start < L:         // straddles the end of the preview (only ever the first chunk past L)
      text += '…'           // appended directly, no newline, at index L
      c' = c with textStart=start, textEnd=L+1, alignment=clipAlignment(c, end-start, L-start)
    else:                   // entirely past the preview
      text += '\n…'
      c' = c with textStart=len(text)-1, textEnd=len(text), alignment=null
  return (text, chunks')
```
Chunks always begin on sentence boundaries (chunking rule of the engine), so each `\n…` line becomes its own sentence
(a UAX #29 sentence break follows a line feed) that "next sentence" can skip to.

Details: `floor(x) or 0` is JS `Math.floor(x) || 0` (NaN → 0). "Keep c unchanged" returns the original chunk object,
offsets not floored. Every rebuilt chunk keeps its own audio, mime and `durationSec`; the input array is not mutated
(sorted copy).

```
clipAlignment(c, len, keep):            // timing for the first `keep` chars + one entry for the rest
  a = c.alignment
  n = (a && len(a.starts) > 0 && len(a.ends) == len(a.starts)) ? len(a.starts) : 0
  lastEnd = n > 0 ? max(a.ends) : 0
  total = c.durationSec > 0 ? c.durationSec : lastEnd
  if !(total > 0): return null
  entry(k) = (n == len || len <= 1) ? min(k, n-1) : round(k*(n-1)/(len-1))   // proportional map
  for k in 0..=keep:
    starts[k] = n > 0 ? a.starts[entry(k)] : total*k/len
    ends[k]   = k == keep ? max(total, lastEnd) : (n > 0 ? a.ends[entry(k)] : total*(k+1)/len)
  return { chars: [''; keep+1], starts, ends }
```
So the trailing `…` starts at the time of the first hidden character and lasts to the end of the chunk audio. The
result has exactly `keep + 1` entries for a `keep + 1`-unit span, so the player maps it 1:1. `round` is JS
`Math.round` (half up; inputs are non-negative). If any `ends` value is NaN, `max(a.ends)` is NaN: with
`durationSec <= 0` the function returns null; otherwise the final end is NaN (harmless, the player never reads `ends`).

`withoutText(chunks)`: same chunks with `alignment.chars` replaced by `''` entries (same length, timing kept). Used
when caching audio for a preview-only record so the full text never lands on disk next to the audio.

### 2.8 `handleCli(args, cwd)` and `parseCliArgs(argv)` — `reader.ts:67-104, 455-478`

`parseCliArgs` scans the whole argv; only tokens starting with `--` are considered; `--name=value` uses the inline
value, otherwise the **next argv token is consumed as the value even if it starts with `--`**; unknown switches and
positionals are ignored (unknown switches do not consume a value).

| Switch | Effect |
|---|---|
| `--hidden` | `hidden = true` (do not show main window) |
| `--read-clipboard` | `readClipboard = true` |
| `--read-file <path>` | `readFile = value || null` |
| `--read-text <text>` | `readText = value || null` |

`--hidden=anything` and `--read-clipboard=anything` still just set the flag (the inline value is ignored and does
not consume the next token). An empty value (`--read-text=`) or a value-taking switch as the last token gives null.
A repeated switch: the last one wins.

`handleCli` (first match wins, returns true when it acted):
1. `readText` → `read({text, source:'cli'})`.
2. `readClipboard` → `readClipboard()`.
3. `readFile` → `file = resolve(cwd || process cwd, path)`, `name = basename`; `text = readTextFile(file, maxChars + 1000)`
   (reads a little past the limit so the truncation toast still fires; the file reader decodes encodings, rejects
   binary, converts HTML/RTF, reads at most 5 MB); `read({text, source:'explorer', sourceApp:'Explorer', sourceProcess:'explorer.exe', sourceTitle:name})`.
   Any error → toast `{kind:'error', message:`Couldn't open ${name}`}`. Returns true either way.

Called with the initial argv at startup and with each second instance's argv (single-instance lock). If a second
instance did nothing and was not `--hidden`, the main window is shown (`index.ts:332-336`).

### 2.9 `togglePlayback()` (play/pause hotkey) — `reader.ts:484-498`

```
if lastReportedState.status != 'idle' OR engine.currentSessionId != null:
    command({type:'toggle'}); return
latest = newest history record (by createdAt)
if latest?.cached and audioCache.has(latest.id):
    if replay(latest.id).ok: return
hotkey = hotkeysEnabled ? hotkeys.readSelection.trim() : ''
toast info: 'Nothing is playing. ' + (hotkey ? `Select some text and press ${hotkey}` : 'Select some text to read it')
```
`engine.currentSessionId` stays set after synthesis finishes, until `engine.stop()`. So after a read *ends* (status
`ended`), the hotkey toggles, which replays it from the start (§4.6 `play()` on `ended`). After the widget is closed
(stop → engine stopped, status `idle`), the hotkey replays the newest read only if that one is cached; it never
searches older records.

"Newest" = `historyStore.list({limit: 1})`: records sorted newest-first by `createdAt` (a new record goes before an
existing one with the same `createdAt`), no filters. More consequences, all reproduced by the code as written:
- While the **error** card is up the status is `error` (≠ idle), so the hotkey sends `toggle`, which the player
  ignores in `error`: the key does nothing until the card is closed.
- After an `ended` read was **auto-hidden** (§7), the hotkey's `toggle` replays it from 0 while the widget stays
  hidden: neither main nor the widget shows the window for a command. Media keys behave the same (§9).
- If `audioCache.has()` is true but loading fails or yields no chunks, `replay()` falls into its miss path and
  **re-synthesizes** (credits) a new `replay` record; if that fails too (e.g. `no-api-key`), both `read()`'s toast and
  the "Nothing is playing…" toast are shown.
- The `replay` and the toast are awaited sequentially; nothing debounces repeated presses.

### 2.10 `command(cmd)`, `stop()`, `shutdown()` — `reader.ts:501-540`

- `command(cmd)`: forward to the player (`player:command`); if `cmd.type == 'stop'` also `engine.stop()` (aborts
  in-flight synthesis; billed-so-far is reported via the engine's `stopped` event). Order: the command is sent to
  the player first, then the engine is stopped. Main never shows the widget for a command. In the widget, a `stop`
  that arrives while a session exists also hides the window (§7); the echo of the widget's own close (§7 `close()`)
  finds no session and does nothing.
- `stop()` = `command({type:'stop'})`.
- `shutdown()`: `engine.stop()` only (quit sequence; does not touch the widget). Errors logged.

### 2.11 Toast routing — `reader.ts:542-557`

```
toast = { id: UUID, ...input }
send toast to app window AND widget
if main window is visible AND focused: return            // the app shows it
if settings.widget.enabled:
    if widget not visible: show widget (no activation)   // widget displays it, hides itself after if idle (§8.4)
    return
if input.kind != 'success' and native notifications supported:
    show OS notification { title: 'Vox', body: message, silent: true }
```
`showWidget` positions the window first when it was hidden (§8.1). With the widget disabled and the app window not
focused, a `success` toast is not surfaced anywhere outside the app window. The toast's `action` is dropped from the
native notification (it has no buttons).

### 2.12 Session tracking and state fold-back — `reader.ts:230-252, 559-636`

`TrackedSession` (one per engine session, keyed by `sessionId`, max 16, insertion-order eviction):

| Field | Meaning |
|---|---|
| `readId` | record id |
| `recordSaved` | a history record exists to write into |
| `fromCache` | cached replay |
| `baseListenedSec`, `baseCompleted` | record values before this session (0/false for fresh reads) — replays add on top |
| `listenedSec` | max `listenedSec` reported for this session (wall seconds) |
| `completed` | reached `ended` without being partial |
| `maxPositionSec` | furthest audio position reached in this session |
| `partial` | synthesis ended early after a chunk failed; reaching the end does not complete the read |
| `lastStatus` | last reported status (null until the first report) |
| `lastWriteAt` | ms of last history write |
| `written` | `{listenedSec, completed, maxPositionSec}` last written, or null |

`latestSessionFor(readId)` = the most recently tracked session with that `readId`.

**`handlePlayerState(state)`** (every report from the player):
```
this.state = state
relay state to app window; emit 'state' (tray refresh)
if state.sessionId: engine.progress(state.sessionId, state.positionSec)    // releases bounded look-ahead
sess = sessions[state.sessionId]; if !sess or !sess.recordSaved: return
if finite(state.listenedSec): sess.listenedSec = max(sess.listenedSec, state.listenedSec)
if status in {playing, paused, ended}:
    pos = (status == 'ended') ? state.durationSec : state.positionSec
    if finite(pos): sess.maxPositionSec = max(sess.maxPositionSec, pos)
if status == 'ended' and !sess.partial: sess.completed = true
statusChanged = status != sess.lastStatus; sess.lastStatus = status
if !statusChanged and now - sess.lastWriteAt < 2000: return
writeSession(sess, now)
```
`this.state` is replaced by every report, including the final `idle` report of a superseded session and idle reports
that carry no session. A report for an untracked or evicted session still updates `this.state`, the app window, the
tray and the engine's playhead (the engine ignores other sessions); only the history part is skipped. The first
report of a session always writes (`lastStatus` starts null and `lastWriteAt` at 0).

Notes: `idle`/`loading`/`error` reports do not move `maxPositionSec`. An `ended` report credits the full duration —
so skipping "next sentence" past the last sentence (which ends the read, §4.6 `skipSentence`) counts as heard 100% and completed.

**`writeSession(sess, now)`** → returns the written `listenedSec` or null:
```
next = { listenedSec: round1(sess.baseListenedSec + sess.listenedSec),
         completed: sess.baseCompleted || sess.completed,
         maxPositionSec: round1(sess.maxPositionSec) }
if sess.written equals next (all three fields): return null
rec = history.get(sess.readId); if !rec: return null
patch = { listenedSec: next.listenedSec, completed: next.completed }
delta = next.listenedSec - (sess.written?.listenedSec ?? sess.baseListenedSec)
if delta > 0: patch.listenLog = addToListenLog(rec.listenLog, hourKey(local now), delta)    // §11
if next.maxPositionSec > (rec.maxPositionSec ?? 0): patch.maxPositionSec = next.maxPositionSec  // never decreases
sess.lastWriteAt = now; sess.written = next
history.update(sess.readId, patch)
```
The whole listening delta since the previous write is credited to the local hour **at write time** (writes happen at
most ~2 s apart while playing, so at most ~2 s can land in the neighbouring hour).

`writeSession` touches neither `lastWriteAt` nor `written` when it returns null (nothing changed, or the record is
gone), so while a deleted record's session keeps playing every report re-attempts the (cheap) lookup. Because
`listenedSec` values are `round1`ed before subtracting, `delta` is a multiple of 0.1 s plus float noise; the ledger
rounds again on insert. `patch` always carries `listenedSec` and `completed` even when unchanged; the store's
equality check suppresses no-op writes.

History persistence is itself debounced (2500 ms debounce, 10 s max wait for the file write; change notifications to
the app 100 ms debounce/400 ms max), so playback writes coalesce on disk.

**`persistPlayback()`** (`reader.ts:514-519`): for every tracked session with `recordSaved` and `lastStatus != null`,
`writeSession(sess, now)` immediately (errors are caught and logged by the caller). Main only knows the last reported
state (≤ 500 ms old while playing); a native port can query the player directly for exact values at that moment.
Two callers (`index.ts:380-416, 569-583`):

| Path | Sequence |
|---|---|
| App quit (`before-quit`, first request only; later requests wait for it) | stop timers, unregister hotkeys, stop the selection service → `persistPlayback()` → `reader.shutdown()` (engine stop) → **async** flush of settings, history and the selection service's clipboard restore (+ bridge server stop), raced against `SHUTDOWN_TIMEOUT_MS = 3000` → destroy tray → quit; a `FORCE_EXIT_MS = 4000` timer then force-exits if a window refuses to close |
| Windows log-off / shutdown (`query-session-end` and `session-end` on the app, widget and chip windows, i.e. `WM_QUERYENDSESSION`/`WM_ENDSESSION`; no `before-quit` happens) | `persistPlayback()` → settings `flushSync()` → history `flushSync()`, all synchronous, because the process may be killed as soon as the window procedure returns. It may run several times (each window, each message); repeats are cheap no-ops. The player is not stopped. |

### 2.13 Engine events — `reader.ts:269-293, 638-677`

**`complete(readId, chunks, billed)`** — every chunk of a fresh read was synthesized (never for cached replays, never
for partial synthesis):
```
sess = latestSessionFor(readId); if !sess or sess.fromCache or !sess.recordSaved: return
rec = history.get(readId); if !rec: return          // deleted / history cleared during synthesis: nothing to cache
duration = sum(c.durationSec for c in chunks if finite)
cached = false
if settings.library.cacheAudio and len(chunks) > 0:
    try:
        audioCache.save(readId, rec.textTruncated ? withoutText(chunks) : chunks)
        cached = true
        pruneCache(keep=[readId])
    except: log warn                                # save failed → cached false; save ok but prune failed → cached true
history.update(readId, { audioDurationSec: round1(duration), cached, charactersBilled: max(0, round(billed || 0)) })
```
The handler is asynchronous (awaits the save and the prune); the record update happens after them, so a replay
requested in between still sees `cached: false`. Its errors are logged (`complete handler failed`). `billed || 0`
maps NaN/undefined to 0 (same in `stopped`). `settings` is read at completion time, not at read start.
"Complete" means *synthesized*, not *listened*: with the 120 s look-ahead it fires once the listener is within ~2 min
of the end (or immediately for short texts). A read stopped mid-synthesis is never cached; replaying it later
re-synthesizes the full text as a new `replay` record.

**`failed(readId, err)`** — the session failed before any audio (first chunk failed, empty text, crash):
```
code = err.code ?? 'unknown'
message = errorMessage(code, err.message)
if latestSessionFor(readId)?.recordSaved: history.update(readId, { error: message })
needsKey = errorNeedsSettings({code, message: err.message ?? '', missingPermission: !!err.missingPermission})
toast { kind:'error', message, action: needsKey ? {label:'Open settings', route:{page:'settings', section:'account'}} : none }
```
The player independently receives the fatal `TtsSessionError` and shows its error card. Engine-side fatal causes and
their messages: first-chunk synthesis failure (ElevenLabs message), no chunks after chunking → `empty-text`
`There is no text to read`, an internal crash → `unknown` `Something went wrong while preparing the audio`, a cached
replay with zero chunks → `not-cached` `The saved audio for this read is missing` (unreachable: `replay()` only calls
`playCached` with ≥ 1 chunk; it would also write that error onto the original record). The record's `error` is never
cleared by a later success.

**`stopped(readId, billed, error?)`** — synthesis ended before the whole text after some chunks were billed (stopped,
superseded by a new read, or a later chunk failed for good — the latter carries `error`):
```
sess = latestSessionFor(readId); if !sess or sess.fromCache: return
if error: sess.partial = true          // must be set before the player's 'ended' report is processed
if !sess.recordSaved: return
patch = { charactersBilled: max(0, round(billed)) }
if error: patch.error = 'Only part of the text could be synthesized'
history.update(readId, patch)
```
(For stop/supersede the engine emits `stopped` asynchronously — after `stop()`/`start()` return — and only when some
characters were already billed and synthesis had not finished. For a later-chunk failure it is emitted right after
the session's `done` event, i.e. long before the player can play to the end, so `partial` is set in time.)

**Error messages** (`reader.ts:143-166`):
```
ERROR_MESSAGES = {
  unauthorized: 'ElevenLabs rejected your API key',
  quota:        'Your ElevenLabs character quota is used up',
  'rate-limit': 'ElevenLabs is busy right now. Try again in a moment',
  network:      "Couldn't reach ElevenLabs. Check your connection",
  'too-long':   'That selection is too long for the current model',
}
errorMessage(code, fallback) = fallback?.trim() || ERROR_MESSAGES[code] || 'Something went wrong while reading'
```
The engine's own message wins because ElevenLabs errors are already user-facing and more precise (missing TTS
permission, account flagged, timeout).

`errorNeedsSettings(err)`: true for `no-api-key` and `quota`; for `unauthorized` only when `!missingPermission` AND the
message is exactly the engine's bad-key message `'Your ElevenLabs key was rejected — update it in Settings'` (a key
lacking TTS permission is fixed on elevenlabs.io; other 401s such as "unusual activity" are not about the key).

### 2.14 `pruneCache(keep = [])` — `reader.ts:525-531`

`limit = settings.library.cacheLimitMB * 1_000_000` bytes. Protected ids: `keep` + engine's current read id + the
last reported state's `readId`. `audioCache.prune(limit, protected)` evicts least-recently-used entries (by last
save/load time) until total bytes ≤ limit; each evicted record gets `cached = false`. Returns evicted ids. Triggered
after each cache save and when the user lowers the limit.

---

## 3. TtsEngine contract the player relies on (summary)

Specified fully elsewhere (`src/main/tts/engine.ts`); behaviour this subsystem depends on:

- `start(opts)` → new `sessionId`; cancels the previous session first; emits `sessionStart` **synchronously** with
  `estimatedDurationSec = text.length / 15 / voice.speed` (`speed` non-positive/non-finite → 1), `fromCache:false`,
  `rate = settings.playbackRate`, `volume = settings.volume`, `highlightWords = settings.widget.highlightWords`. Chunks,
  done, errors and events arrive asynchronously afterwards.
- Chunking: first chunk ≤ 220 chars, later ≤ 900, on sentence boundaries (fallback clause/space).
- Sequential synthesis; **bounded look-ahead**: chunks 0 and 1 requested immediately; every later chunk waits until
  `deliveredSec - positionSec < 120` (`LOOKAHEAD_SEC`), where `positionSec` comes from `progress(sessionId, pos)`
  (called on every player report). A paused or silent player therefore holds synthesis (and billing).
- Chunk `durationSec`: the max alignment `end`; a "measured" duration (CBR MP3: byte size `bytes*8/(kbps*1000)`, kbps
  from the output format; WAV: the header's duration) is used instead when it lies within `[alignEnd, alignEnd + 2]` s
  (trailing silence after the last character); without alignment the measured duration if > 0; else
  `chars / 15 / speed`.
- Retries (per chunk, retryable errors only): delays `[1000, 2000, 4000]` ms, or the server's `Retry-After` hint if
  longer (capped at 10 000 ms); after the third retry the error stands. Mock mode waits 350 ms for the first chunk
  and 120 ms for each later one.
- Errors: no chunks after chunking → fatal `empty-text` `There is no text to read`; an internal crash → fatal
  `unknown` `Something went wrong while preparing the audio` (only if the session is still live and unfinished).
- First chunk failure → fatal `TtsSessionError` + `failed` event. Later chunk failure (after retries) → non-fatal
  `TtsSessionError`, then `done` with the chunks delivered so far, then `stopped(readId, billed, error)`.
- `playCached(opts)`: `sessionStart` with `fromCache:true` and `estimatedDurationSec = Σ chunk.durationSec`; then every
  chunk (renumbered 0..n-1, sorted by original index) and `done`, all immediately; zero chunks → fatal
  `not-cached` "The saved audio for this read is missing". No `complete` event.
- `currentSessionId` / `currentReadId`: the latest session until `stop()` (kept after synthesis finishes).
- Mock mode (no network; `VOX_MOCK_TTS=1`, demo and screenshot modes): 16-bit mono WAV 22 050 Hz, a 220 Hz sine at
  amplitude 0.015 with 30 ms linear fade in/out, lasting `max(0.6, chars/15/speed)` s (rounded to whole frames);
  alignment 1:1 per UTF-16 unit, evenly spaced, times rounded to 1 ms, last end = the exact duration; billed
  characters = text length. The API-key gate of `read()` is skipped in mock mode.
- Every sink call is wrapped: if delivery to the widget throws (window destroyed), it is logged and synthesis and
  caching continue.

---

## 4. AudioQueuePlayer — `src/renderer/src/widget/player.ts`

Framework-agnostic playback of one session delivered as ordered audio chunks. One audio element per chunk, played back
to back; at most the current and the next chunk hold a live element (decoder) at any time (two exceptions: a fatal
error keeps whatever elements exist until `stop()`, and a seek that lands inside a failed chunk creates an element
for it that is only released by a later seek, `finish()` or `stop()`, see §4.6). There is a small gap between chunks:
the next element's `play()` is only called from the previous one's `ended` event.

### 4.1 Constants

| Name | Value | Meaning |
|---|---|---|
| `RATE_MIN`, `RATE_MAX` | 0.5, 3 | playback rate clamp (`player.ts:153-154`) |
| `END_GUARD_SEC` | 0.05 | seeking never lands closer than this to the end of available audio / of a chunk (`:156`) |
| `BACK_GRACE_SEC` | 1.2 | "previous sentence" restarts the current sentence when more than this many **wall-clock** seconds into it (`:158`) |
| `PARTIAL_SKIP_SEC` | 10 | prev/next step when sentence positions are unknown (`:160`) |
| `IDLE_OUTPUT_HOLD_MS` | 30 000 | keep audio output awake after a read ends / stops / fails (`:162`) |
| `PAUSED_OUTPUT_HOLD_MS` | 60 000 | keep it awake while paused; longer would keep an audio stream open that can stop the PC from sleeping (`:164`) |
| report interval | 500 ms | state report cadence while `playing` (`reportIntervalMs ?? 500`, `:899`) |
| default skip amount | 10 s | `skip` with `unit:'seconds'` and no positive `amount` (`:303`) |
| default mime | `'audio/mpeg'` | when a chunk has no mime (`:597`) |
| default chunk min duration | 0.1 s | floor of estimated chunk duration (`:613`) |

### 4.2 Platform abstraction and what the browser did implicitly

The player is written against an injected environment (`player.ts:20-74`):

| Abstraction | Browser implementation | Native equivalent to build |
|---|---|---|
| `createAudio()` → element with `src, preload, playbackRate, defaultPlaybackRate, preservesPitch, volume, currentTime, duration, paused, ended, play(), pause(), load()` and events `ended, error, loadedmetadata, durationchange, pause, play, playing, waiting` | `new Audio()` | a per-chunk "voice" object: decoded PCM (MP3 via ElevenLabs, WAV in mock) + read cursor, fed into a mixer |
| `createUrl(bytes, mime)` / `revokeUrl` | Blob URL | just hold the bytes; free on session end |
| `now()` | `performance.now()` (monotonic ms) | `Instant` |
| `setInterval/clearInterval` | window timers | timer thread / tokio interval |
| `prepareOutput()` / `releaseOutput(ms)` | output warmer (§6) | same algorithm on the WASAPI stream |

Element properties set on creation (`ensureAudio`, `player.ts:668-674`), each with a reason:
- `preload = 'auto'` — decode ahead so the next chunk starts immediately.
- `preservesPitch = true` — **rate changes tempo but not pitch** (Chromium applies a WSOLA time-stretch). A native port
  needs a speech-quality time-stretcher for 0.5×–3× (see Rust notes).
- `volume = player volume` — linear amplitude 0..1.
- `defaultPlaybackRate = rate` and `playbackRate = rate` — `load()` resets `playbackRate` to `defaultPlaybackRate`, so
  both are set; `playbackRate` is set again before every `play()`.
- `currentTime = offset` if `offset > 0` (a seek that targeted this chunk before its element existed).

Event semantics the logic depends on:
- `playing` fires when sound actually starts; `waiting` when it stalls. These drive "audible" (listening clock).
- When an element finishes it fires `pause` (with `ended == true`) and then `ended`.
- `pause`/`play` also fire when **the OS** (Windows media overlay / hardware media keys via Chromium's default media
  session actions) pauses or plays the element directly — handled as "external" pause/play.
- `play()` returns a promise that rejects with `AbortError` when interrupted by `pause()`/src change (not a failure),
  or `NotAllowedError` under an autoplay policy.
- `loadedmetadata`/`durationchange` expose the decoded `duration`, which is more exact than the engine's estimate.

### 4.3 Internal state

| Field | Initial | Meaning |
|---|---|---|
| `session` | null | current session (below) |
| `status` | `'idle'` | `PlayerStatus` |
| `cur` | 0 | index of the current chunk (may equal `chunks.length` = past the last available chunk) |
| `wantPlay` | true | **user intent**: playback runs, or resumes as soon as audio arrives |
| `waiting` | false | caught up with synthesis, waiting for the next chunk ("buffering") |
| `rate`, `volume` | 1, 1 | persist across sessions in the player, but every `startSession` overrides them from the payload |
| `listenedAcc` | 0 | banked wall seconds of listening for this session |
| `mark` | null | `{wall: ms, media: s, audio: element}` while listening is being counted |
| `audible` | false | current element is producing sound (`playing` fired, no `pause`/`waiting` since) |
| `error` | null | `{message, code}` of a fatal session error |
| `startToken` | 0 | bumped by every `playCurrent()`; a start waiting for the output proceeds only if not superseded |
| `gatedToken` | -1 | `startToken` of the last start that had to wait for the output |
| `timer` | null | 500 ms report interval, running only while `playing` |

`stop()` resets `session`, `error`, `cur`, `listenedAcc`, `mark`, `audible`, `waiting` but **not** `wantPlay`, `rate`,
`volume`, `startToken` or `gatedToken` (the next `startSession` sets `wantPlay = true`; the tokens only ever grow).

Session:

| Field | Meaning |
|---|---|
| `info` | `SessionInfo {id, readId, text, title, voiceName, sourceApp?, fromCache, highlightWords, sentences: number[]}` — stable for the session |
| `estimatedDurationSec` | from `sessionStart` if finite and > 0, else `text.length / 15` (no voice-speed factor here) |
| `chunks: ChunkEntry[]` | contiguous, in order |
| `pending: Map<index, TtsChunk>` | out-of-order arrivals |
| `done` | all chunks delivered |
| `warning` | `{message, code}` of a non-fatal session error (read will stop early) |
| `partialText` | some chunk's raw text range lay beyond the session text |

ChunkEntry: `index, textStart, textEnd, url, alignment | null, durationSec (0 = unknown), audio | null,
detach | null, offset (s to apply when the element is (re)created), failed`.

### 4.4 Session events (main → player)

**`startSession(s)`** (`player.ts:194-229`):
1. If `s.sessionId` equals the current session id → ignore (duplicate delivery).
2. `stop()` — ends the previous session and **reports a final `idle` state for it** (so main writes its final listening).
3. `text = s.text if string else ''`. `info = {id, readId, text, title: s.title || text.slice(0,120), voiceName: s.voiceName || '', sourceApp: s.sourceApp || none, fromCache: !!s.fromCache, highlightWords: s.highlightWords !== false, sentences: sentenceStarts(text)}`.
4. `estimatedDurationSec` as above; `chunks = []`, `pending = {}`, `done = false`, `warning = null`, `partialText = false`.
5. `cur = 0`, `wantPlay = true`, `waiting = true`, `listenedAcc = 0`, `mark = null`, `audible = false`, `error = null`.
6. If `s.rate` finite → `rate = clamp(s.rate, 0.5, 3)`; if `s.volume` finite → `volume = clamp(s.volume, 0, 1)`.
7. `prepareOutput()` (start waking the audio device now, so it is up by the time the first chunk arrives; the returned
   promise is ignored here).
8. `setStatus('loading', force=true)`.

**`addChunk(c)`** (`:231-257`):
```
if no session or c.sessionId != session.id or c.index not an integer >= 0: ignore
if c.index < len(chunks) or c.index in pending: ignore (duplicate)
pending[c.index] = c
while pending has len(chunks): pushChunk(pending.remove(len(chunks)))
if status == 'error': changed(); return       // chunks are still stored, nothing plays
caughtUp = waiting or status == 'ended'
if caughtUp and cur < len(chunks):          // playback had consumed everything available
    if status == 'ended': wantPlay = true
    waiting = false
    if wantPlay: playCurrent()
    else: ensureAudio(cur); setStatus('paused')
elif cur + 1 < len(chunks):
    ensureAudio(cur + 1)                     // preload the next chunk
changed()
```

**`pushChunk`** (`:584-605`):
```
len = len(session.text)
rawStart = floor(c.textStart) or 0; rawEnd = floor(c.textEnd) or 0
if rawStart > len or rawEnd > len: session.partialText = true
textStart = clamp(rawStart, 0, len); textEnd = clamp(rawEnd, textStart, len)
append ChunkEntry{ index: len(chunks), textStart, textEnd, url: createUrl(c.audio, c.mime || 'audio/mpeg'),
  alignment: validAlignment(c.alignment), durationSec: (finite & > 0) ? c.durationSec : 0,
  audio:null, detach:null, offset:0, failed:false }
validAlignment(a) = null if a missing, a.starts not an array, empty, or any start non-finite; else a
```
`validAlignment` does not check that `starts` is ascending or that `ends`/`chars` have the same length; the binary
searches in §5 assume ascending starts (ElevenLabs and the mock produce them).
`partialText` is a defensive path: the controller's `previewReplay` normally guarantees offsets inside the text, so in
current builds it only triggers for mismatched data. When set, word positions past the text are unknown: the UI shows
the title instead of highlighted words, and prev/next step ±10 s.

**`finishSession(d)`** (`:259-265`): ignore other sessions; `done = true`; if `status != 'error' && waiting && cur >= len(chunks)` → `finish()` (all audio already played). `changed()`. `totalChunks` is not used. This also
applies when the user paused while buffering at the end of the available audio (`paused` + `waiting`): the read goes
straight to `ended`/FINISHED. `done` while the current chunk is still playing only sets the flag; the end is reached
through `advance` (§4.7).

**`failSession(e)`** (`:267-280`):
- Non-fatal: `warning = {message: e.message || 'Part of the text could not be synthesized', code: e.code ?? 'unknown'}`; `changed()`. Playback continues with what arrived; the session ends after the last good chunk (done follows). A later warning replaces an earlier one; it is never cleared within the session.
- Fatal: pause current element; `error = {message: e.message || 'Something went wrong', code: e.code ?? 'unknown'}`; `waiting = false`; `wantPlay = false`; `releaseOutput(30 000)`; `setStatus('error', force=true)` (the forced report carries `error`/`errorCode`). Chunk elements and their buffers are kept until `stop()` or the next session. Applies whatever the status was (even `ended`/`paused`).

### 4.5 Status state machine

`PlayerStatus`: `idle` (no session), `loading` (waiting for audio: synthesis, or output warm-up), `playing`, `paused`,
`ended` (everything played), `error` (fatal).

| From | Event | To | Notes |
|---|---|---|---|
| any | `startSession` | `loading` (forced report) | previous session reported `idle` first |
| `loading` (waiting) | first/next chunk arrives, `wantPlay` | `loading`(starting) → `playing`, or `playing` directly | output gate §4.7 |
| `paused` (waiting) | chunk arrives, `!wantPlay` | `paused` | element created, ready |
| `loading` (starting) | output warm or 3 s cap | `playing` | |
| `playing` | chunk ends, next chunk available | `playing` | via `advance` |
| `playing` | chunk ends, none available, not done | `loading` (waiting) if `wantPlay` | "SYNTHESIZING" |
| `playing` / `loading` | chunk ends / skip-next, none left, `done` | `ended` | `finish()` |
| `playing` / `loading` | `pause()` | `paused` | release output in 60 s |
| `paused` | `play()` | `playing` / `loading` | `loading` if waiting for chunks |
| `ended` | `play()` / `seek()` / skip-back | `playing`/`loading` | `play()` on ended = seek(0) = replay |
| `playing` | element paused by the OS | `paused` | external pause |
| `paused` | element played by the OS | `playing` | external play |
| `playing` | `play()` rejected `NotAllowedError` | `paused` | autoplay policy |
| `playing` / `loading` | `play()` rejected otherwise, or element `error` | `playing`/`loading`/`ended` | chunk marked failed and skipped (`advance`) |
| `loading` (starting) | `pause()` | `paused` | the pending warm-up start is abandoned |
| `paused` | `seek` / skip | `paused` | new offset applied, element ready; nothing plays |
| `paused` (waiting at end of audio) | `done` | `ended` | §4.4 `finishSession` |
| `paused` | skip-next past the last sentence, `done` | `ended` | `finish()` |
| `ended` | new chunk (defensive; `ended` implies `done`) | `playing`/`loading` | `wantPlay` set |
| any with session | fatal session error | `error` | |
| any | `stop()` | `idle` (forced report) | session forgotten |
| `error` | `play`/`seek`/`skip`/`toggle` | (no-op) | only stop or a new session leaves `error`; `pause` only clears `wantPlay`, `setRate`/`setVolume` still apply |

`setStatus(next, force=false)` (`:840-848`): no-op if unchanged and not forced; else set, `syncClock()`, start the 500 ms
report timer if `playing` else stop it, `report()`, `notify()`.

### 4.6 Transport commands

`command(cmd)` dispatch (`:284-311`): `toggle`, `play`, `pause`, `stop`; `seek` (if `positionSec` finite);
`seekFraction` (if `fraction` finite); `skip`: `dir = direction < 0 ? -1 : 1`; `unit == 'sentence'` → `skipSentence(dir)`,
else `skipSeconds(dir * (amount finite && amount > 0 ? amount : 10))`; `rate` → `setRate`; `volume` → `setVolume`.

**`play()`** (`:313-327`):
```
if no session or status == 'error': return
wantPlay = true
if status == 'ended': return seek(0)                 // "Play again"
if waiting or chunks[cur] is missing:
    waiting = true; prepareOutput(); setStatus('loading'); changed(); return
playCurrent(); changed()
```

**`pause()`** (`:329-338`): no session → return. `wantPlay = false`. If `playing` or `loading`: pause current element,
`setStatus('paused')`, `releaseOutput(60 000)` (in that order). `changed()`. (Pausing while buffering keeps
`waiting = true`; when the chunk arrives the player stays `paused` with audio ready.) In any other status it only
clears `wantPlay` and reports. Pausing during STARTING (gated start) makes the pending start bail out when the
warm-up resolves.

**`toggle()`**: `playing`/`loading` → `pause()`, otherwise `play()`.

**`stop()`** (`:346-369`):
```
if no session: if status != 'idle': setStatus('idle', force); return
pause current element; waiting = false
releaseOutput(30 000)        // a new session calls stop() then prepareOutput(), which cancels this release
setStatus('idle', force)     // reported WITH the session still attached → final listenedSec for that session
release every chunk's element; revoke every URL; pending.clear()
session = null; error = null; cur = 0; listenedAcc = 0; mark = null; audible = false
notify()
```

**`seek(sec)`** — seek to `sec` of session audio, clamped to what has been synthesized (`:372-404`):
```
if no session or status == 'error' or len(chunks) == 0: return
target = clamp(sec, 0, max(0, availableSec() - 0.05))
i = 0; start = 0
for i in 0 .. len(chunks)-2:                 // the last chunk takes everything beyond
    d = dur(chunks[i]); if target < start + d: break; start += d
if status == 'ended': wantPlay = true        // seeking after the end resumes playback
if i != cur:
    if chunks[cur] exists: releaseAudio(chunks[cur])
    audible = false; syncClock(); cur = i
release every chunk element except i and i+1
chunks[i].offset = clamp(target - start, 0, max(0, dur(chunks[i]) - 0.05))
waiting = false
a = ensureAudio(i); if a: setMediaTime(a, chunks[i].offset)   // a jump never counts as listening
if wantPlay: playCurrent() else setStatus('paused')
changed()                                    // reports → main → engine.progress (wakes look-ahead)
```
Notes on `seek`:
- The loop variable survives the loop: if no `break` happens, `i = n - 1` (the last chunk absorbs any remainder).
- Seeking while `paused` stays `paused` (offset applied, element created and positioned). Seeking while buffering
  (`loading`, `waiting`) back into available audio resumes playback (`waiting = false`, `wantPlay` is still true).
- Seeking within the current chunk while it plays: `setMediaTime` moves the playhead (banking listening first), then
  `playCurrent()` finds the element not paused and calls `startElement` directly (no warm-up gate, no status change).
- When the current chunk changes, the old chunk's element is released first (its listening banked), then every other
  element except `i` and `i+1` is released. An existing element for `i` (e.g. the preloaded next chunk) is reused.
- Seeking into a **failed** chunk: `ensureAudio` creates an element for it (the `failed` flag is not cleared), then
  `playCurrent()` sees `failed` and `advance`s to the start of the next playable chunk; the failed chunk's element
  is not released by that `advance` (only by a later seek, `finish()` or `stop()`). While paused, the playhead stays
  inside the failed chunk until play.

**`seekFraction(f)`** = `seek(clamp(f,0,1) * durationSec())` — fraction of the *estimated* total, so dragging past the
synthesized part lands at the end of available audio.

**`skipSeconds(delta)`** = `seek(positionSec() + delta)` (if a session exists).

**`skipSentence(dir)`** (`:420-443`):
```
if no session or status == 'error' or len(chunks) == 0: return
if dir > 0 and status == 'ended': return
if session.partialText: return skipSeconds(dir * 10)
pos = positionSec()
times = [timeAtChar(ch) for ch in session.info.sentences]      // Infinity where audio has not arrived
if dir > 0:
    t = first t in times with t > pos + 0.05
    if t exists and finite: return seek(t)
    if session.done: finish(); changed(); return              // next past the last sentence ends the read
    return seek(availableSec())                               // jump to end of available audio; then buffering
grace = 1.2 * rate                                            // 1.2 wall seconds expressed in audio seconds
target = 0
for t in times:
    if !finite(t) or t > pos - grace: break
    target = t
seek(target)
```
Effect of "previous": more than 1.2 wall-clock seconds into the current sentence → restart it; otherwise go to the
previous sentence's start (or 0). "Next" also works while `paused` (the seek keeps it paused), and "next" past the
last sentence when `done` ends the read even from `paused`. Sentence times are recomputed on every press from the
current chunk durations (which change as decoded durations replace estimates). `times` is ascending because
sentences and chunks are.

**`setRate(rate)`** (`:445-457`): ignore non-finite; `r = clamp(rate, 0.5, 3)`; equal → return; `creditListening()`
(listening so far is banked at the old rate); `rate = r`; set `defaultPlaybackRate` and `playbackRate` on every live
element; `changed()`. Works with no session too (the stored rate changes and an idle state is reported): this is how
the settings value applied at widget startup (§7) becomes the player's rate before any read.

**`setVolume(v)`**: ignore non-finite; clamp 0..1; equal → return; set on every live element; `changed()`.

**`dispose()`**: `stop()`, stop the report timer, drop listeners (on window unload; the `stop()` reports the final
`idle` state).

### 4.7 Chunk element lifecycle

**`ensureAudio(i)`** (`:643-676`): returns the existing element or creates one, registering handlers that only fire
while still relevant (`session.id` unchanged and the chunk still owns this element):

| Event | Handler |
|---|---|
| `ended` | `onChunkEnded(i)` |
| `error` | `onChunkError(i)` |
| `loadedmetadata`, `durationchange` | `onChunkMeta(c)` |
| `pause` | `onExternalPause(c, a)` |
| `play` | `onExternalPlay(c)` |
| `playing` | `setAudible(c, true)` |
| `waiting` | `setAudible(c, false)` |

**`releaseAudio(c)`** (`:678-692`): if `c` is current → `setAudible(c, false)` **first** (banks listening while the
element still reports its media time); detach handlers; `c.audio = null`; pause, remove `src`, `load()` (frees the
decoder); ignore errors.

**`playCurrent()`** — start the current chunk, gated on the output warmer (`:694-720`):
```
c = chunks[cur]; if none: return
if c.failed: return advance(cur + 1)
a = ensureAudio(cur)
token = ++startToken
ready = prepareOutput()            // null = output ready / no warmer / warm-up already timed out
if ready and a.paused:
    gatedToken = token
    setStatus('loading')           // UI: "STARTING"
    when ready resolves:
        if token != startToken or session changed or c.audio != a: return       // superseded
        if !wantPlay or waiting or status != 'loading': return                 // paused/stopped meanwhile
        startElement(c, a)         // start even if the output never got ready: waiting again would hold forever
    return
startElement(c, a)                 // an element already playing (seek/skip while playing) has the output awake
```
Why: a sleeping output device clips the first moments of sound. Usually warming already finished while the first
chunk was being synthesized; the warmer bounds the wait to 3 s. Pause, seek, stop or a new start during the wait
supersede it. Every `playCurrent()` calls `prepareOutput()`, which also cancels any scheduled output release. Several
gated starts share the warmer's single pending promise; only the newest token proceeds. If the status was already
`loading` (e.g. right after the chunk arrived while SYNTHESIZING), `setStatus('loading')` is a no-op: the switch from
SYNTHESIZING to STARTING is not a status change, it is only visible through `snapshot.starting` (§4.10); the caller's
`changed()` (e.g. in `addChunk`) still reports and notifies.

**`startElement(c, a)`** (`:722-735`): no session → return; `a.playbackRate = rate`; call `a.play()` (sync throw or
async rejection → `onPlayRejected`); `setStatus('playing')` immediately (listening is only counted once the element
reports `playing`); `ensureAudio(cur + 1)` (preload next).
Ordering quirk: a **synchronous** throw from `play()` is handled before `setStatus('playing')`, so a sync
`NotAllowedError` would set `paused` and then be overwritten by `playing` (and a sync generic error would `advance`
and then be overwritten too). Browsers always reject asynchronously, so only the async path is real. A native port
should make a start failure win over the optimistic `playing`.

**`onPlayRejected(err)`**: stale → ignore; `AbortError` → ignore (pause/seek interrupted it); `NotAllowedError` →
`wantPlay = false`, `releaseOutput(60 000)`, `setStatus('paused')`, `changed()`; anything else → `onChunkError(c.index)`.

**`onChunkMeta(c)`** (`:750-760`): if the decoded duration is finite, > 0 and differs from `c.durationSec` by
> 0.01 s → adopt it and `changed(report=false)` (UI refresh without a state report). If `c.offset > 0` and the element
is current and `currentTime < 0.01` and `|currentTime - offset| > 0.05` → re-apply the offset with `setMediaTime`
(the seek offset was lost when media loaded).

**`onChunkEnded(i)`** (`:762-771`): only for the current chunk: adopt the decoded duration if valid; `offset = 0`;
`releaseAudio(c)`; `advance(i + 1)`.

**`onChunkError(i)`** (`:773-781`): mark `failed`, release it; if it is current and (`playing` or `wantPlay`) →
`advance(i + 1)`, else `changed()`. A failed chunk keeps its slot on the timeline (its duration still counts), playback
skips over it. A failure is never retried within the session, and a read whose chunks failed still reaches `ended`,
which main counts as completed and heard to 100% (§2.12): only synthesis failures make a read "partial".

**`onExternalPause(c, a)`** (`:784-791`): `setAudible(c, false)`; then only if `c` is current, `!a.ended`, and status is
`playing`: `wantPlay = false`, `releaseOutput(60 000)`, `setStatus('paused')`, `changed()`. (Our own `pause()`/`stop()`
set the status before the async event arrives, so they are no-ops here; a natural end has `ended == true`.)

**`onExternalPlay(c)`** (`:793-800`): only if `c` is current and status is `paused`: `prepareOutput()` (keep the output
awake), `wantPlay = true`, `setStatus('playing')`, `changed()`.

**`advance(next)`** (`:802-826`):
```
audible = false; syncClock()
cur = next; while chunks[cur]?.failed: cur++
if cur < len(chunks):
    waiting = false; chunks[cur].offset = 0
    a = ensureAudio(cur); if a.currentTime > 0: a.currentTime = 0
    if wantPlay: playCurrent() else setStatus('paused')
    changed(); return
if session.done: finish(); changed(); return
waiting = true
if wantPlay: setStatus('loading')
changed()
```
Why the rewind: `offset` is only reset by `advance`/`onChunkEnded`, so a preloaded next chunk may have been created
with a stale offset from an earlier seek into it; its element would start mid-chunk without the reset. The rewind
uses a plain `currentTime = 0` (not `setMediaTime`): the listening mark was already cleared by `syncClock` above.
With `!wantPlay` (practically unreachable: every caller runs while playing intent is on) `advance` leaves the next
chunk ready and `paused`, or, past the available audio, keeps the status and only sets `waiting`.

**`finish()`** (`:829-838`): release all elements; `cur = len(chunks)`; `waiting = false`; `wantPlay = false`;
`releaseOutput(30 000)`; `setStatus('ended')`.

### 4.8 Timeline maths

```
dur(c)          = c.durationSec > 0 ? c.durationSec
                  : max(0.1, (c.textEnd - c.textStart) * perChar)
                  where perChar = textLen > 0 ? estimatedDurationSec / textLen : 1/15
availableSec()  = Σ dur(c) over delivered chunks
positionSec()   = 0 if no session
                  availableSec() if status == 'ended'
                  else Σ dur(chunks[0..min(cur,n)]) + clamp(t, 0, dur(chunks[cur]))  (if chunks[cur] exists)
                  where t = element.currentTime if the chunk has an element, else chunk.offset (non-finite → 0)
durationSec()   = 0 if no session
                  availableSec() if done
                  else avail + remainingChars * secPerChar
                    delivered      = chunks.last.textEnd (0 if none)
                    remainingChars = max(0, textLen - delivered)
                    secPerChar     = (delivered > 0 && avail > 0) ? avail / delivered       // observed rate
                                     : textLen > 0 ? estimatedDurationSec / textLen : 1/15
```
So the total starts as the engine's `chars / 15 / voice.speed` estimate and is refined to the measured
seconds-per-character of the audio already delivered as soon as the first chunk arrives; it becomes exact when `done`.

```
charIndex()     = 0 if no session; textLen if 'ended'
                  if chunks[cur] missing: cur > 0 ? chunks[cur-1].textEnd : 0
                  else charAtTime({...chunk, durationSec: dur(chunk)}, t)       // t as above
timeAtChar(ch)  (player level, session seconds; Infinity = audio not arrived yet)
                  if no chunks: Infinity
                  i = lastIndexAtOrBefore([c.textStart for c in chunks], ch); if i < 0: 0
                  if i is the last chunk and ch >= last.textEnd: done ? availableSec() : Infinity
                  start = Σ dur(chunks[0..i])
                  if ch >= chunks[i].textEnd: start + dur(chunks[i])            // in a gap between chunks
                  else start + timing.timeAtChar({...chunks[i], durationSec: dur(chunks[i])}, ch)
```

### 4.9 Listening clock (`listenedSec`)

Listening is counted **only while `status == 'playing'` and the current element is audible** (`playing` event seen,
no `pause`/`waiting` since) (`:850-895`):

```
syncClock():
  running = status == 'playing' && audible
  if running && mark == null && current element exists:
      mark = { wall: now(), media: mediaTime(el), audio: el }
  elif !running && mark != null:
      listenedAcc += creditSince(mark); mark = null

creditSince(m):                           // wall time, capped by actual media progress at the current rate
  wall  = (now() - m.wall) / 1000
  media = mediaTime(m.audio) - m.media
  if !(wall > 0) or !(media > 0): return 0
  return min(wall, media / rate)

creditListening():                        // bank and restart the baseline (before rate changes and seeks)
  if mark: listenedAcc += creditSince(mark); mark = {wall: now(), media: mediaTime(mark.audio), audio: mark.audio}

setMediaTime(el, t):                      // move a playhead without counting the jump
  counting = mark?.audio == el
  if counting: creditListening()
  el.currentTime = t
  if counting: mark = {wall: now(), media: mediaTime(el), audio: el}

listenedSec() = listenedAcc + creditSince(mark)
mediaTime(el) = el.currentTime if finite else 0
```
Why the cap: device spin-up after `playing`, stalls the element does not report, and system sleep (the monotonic clock
keeps counting through suspend on Windows) must add nothing. `syncClock` runs on every status change, audible change,
chunk advance and seek.

Further rules:
- `creditSince` divides by the rate **at the time of the call** for the whole interval, which is why `setRate` banks
  first (`creditListening`) and restarts the baseline.
- The mark belongs to one element. Moving to another chunk always goes through `audible = false` + `syncClock()`
  (`advance`, cross-chunk `seek`, `releaseAudio` of the current chunk), so the old element's time is banked and the
  new element only starts counting at its own `playing` event.
- Within the same element, `setMediaTime` banks and rebases, so a seek inside the chunk never counts as listened.
  The `currentTime = 0` rewind in `advance` and the `currentTime = offset` in `ensureAudio` happen while no mark exists.
- `listenedAcc` and `mark` are per session (reset by `startSession` and `stop`); `listenedSec` in a report is this
  session's total only. Main adds it on top of the record's previous total (§2.12).
- If the status is `playing` and `audible` is true but the current element is missing, nothing is counted.

### 4.10 Reporting, state and snapshot

- `report()` sends `getState()` to main (failures swallowed: reporting must never break playback). It runs on every
  status change, on every discrete change (`changed()`: chunk arrival, done, warning, seek, rate, volume, pause/play),
  and every 500 ms while `playing`. `changed(false)` (decoded-duration refinement) only notifies UI listeners.
- `notify()` calls UI subscribers (exceptions isolated per listener).
- On widget startup the controller reports the (idle) state once: main may have restarted the widget renderer
  mid-session, so main learns that nothing is playing. (This direct report also resolves main's widget-ready gate.)
- Order inside `setStatus`: status set → `syncClock()` → timer start/stop → `report()` → `notify()`. Inside
  `changed()`: `report()` then `notify()`.
- Nothing is reported while `paused`, `loading`, `ended` or `error` unless something changes; in particular a paused
  player sends no position updates, which is what holds the engine's look-ahead (§3).

**`getState()`** (`:492-518`):
```
no session → { ...IDLE_PLAYER_STATE, status, rate, volume }
else:
  duration = durationSec()
  { status, sessionId, readId, title, voiceName,
    positionSec: min(positionSec(), duration),
    durationSec: duration,
    durationIsEstimate: !done,
    bufferedFraction: done ? (chunks non-empty ? 1 : 0) : (duration > 0 ? clamp(availableSec()/duration, 0, 1) : 0),
    rate, volume, charIndex: charIndex(), totalChars: len(text), listenedSec: listenedSec(),
    sourceApp? (if set), error?/errorCode? (if fatal error) }
```
`bufferedFraction` is the synthesized share of the (estimated) total; because of the look-ahead it runs ≤ ~120 s of
audio (plus one chunk) ahead of the playhead after the first two chunks. Without a session the status is always
`idle` (only `stop()` clears the session, and it sets `idle` first), so the no-session state is `IDLE_PLAYER_STATE`
with the player's current `rate` and `volume`. `positionSec` is capped at `duration` because the estimate can drop
below the position for a moment while the measured rate refines it.

**`PlayerSnapshot`** (what the UI renders; `:102-118, 520-530`):

| Field | Definition |
|---|---|
| `state` | `getState()` |
| `session` | `SessionInfo` or null |
| `wantsPlay` | `wantPlay && session != null && status ∉ {ended, error}` — drives the play/pause icon |
| `buffering` | `waiting` |
| `starting` | `status == 'loading' && !waiting && gatedToken == startToken` — audio ready, waiting for the output to wake |
| `warning` | session's non-fatal error or null |
| `partialText` | session's `partialText` or false |

---

## 5. Timing helpers — `src/renderer/src/widget/timing.ts`

**`lastIndexAtOrBefore(arr, x)`**: binary search over an ascending array; index of the last element ≤ x, or -1.

**`charAtTime(span, t)`** — absolute char spoken at `t` s into a chunk (`:36-50`):
```
len = span.textEnd - span.textStart; if len <= 0: return span.textStart
starts = usableStarts(span, len)
if starts:
    n = len(starts); i = max(0, lastIndexAtOrBefore(starts, t))
    local = (n == len) ? i : floor(i * (len-1) / (n-1))         // proportional, both ends aligned
else:
    local = span.durationSec > 0 ? floor((t / span.durationSec) * len + 1e-9) : 0
    // epsilon: timeAtChar's float division must round-trip to the same character
return span.textStart + clamp(local, 0, len-1)

usableStarts(span, len) = alignment.starts if non-empty and (len(starts) == len or (len(starts) > 1 and len > 1)) else null
```
Why proportional: ElevenLabs counts alignment characters its own way (code points, its own whitespace handling), so
the alignment length can differ from the chunk's UTF-16 range; entry `n-1` maps to char `len-1`.

**`timeAtChar(span, char)`** — inverse (`:63-74`), guarantees `charAtTime(span, timeAtChar(span, c)) >= c`:
```
len = textEnd - textStart; if len <= 0: return 0
local = clamp(char - textStart, 0, len-1)
if starts usable: i = (n == len) ? local : min(n-1, ceil(local*(n-1)/(len-1))); return starts[i]
return span.durationSec * (local / len)
```

**`sentenceStarts(text)`** — offsets of the first non-space char of each sentence, ascending; never empty (`:103-126`):
```
raw = start index of every segment from a UAX #29 sentence segmenter (Intl.Segmenter granularity 'sentence',
      default locale); fallback when unavailable: [0] + end of every match of /[.!?…]+["'”’)\]]*\s+/g
raw.push(len(text))
out = []
for each consecutive pair (a, b) in raw:
    while a < b and text[a] is whitespace: a++
    if a >= b: continue
    out.push(a, ...softSplits(text, a, b))
return out if non-empty else [0]
```
Segmenter details: `new Intl.Segmenter(undefined, {granularity: 'sentence'})` uses the renderer's default locale and
plain ICU UAX #29 rules **without** abbreviation suppressions, so "Mr. Smith" breaks after "Mr. " (UAX #29 rule SB8
does not hold before an uppercase letter), and a break follows every line feed / paragraph separator (SB4). The first
segment index is always 0. Electron always has `Intl.Segmenter`, so the regex fallback is effectively dead code; keep
it only if the chosen segmenter can be unavailable. Empty text yields `[0]`. "Whitespace" here is JS `\s` (§0).
Computed once per session, at `startSession`.

**`softSplits(text, start, end)`** — sentences longer than 280 chars are split at clause punctuation so sentence
skipping stays useful (`LONG_SENTENCE = 280`, `SOFT_TARGET = 200`):
```
out = []; from = start
while end - from > 280:
    cut = -1
    hi = min(end - 1, from + 280)
    for p from hi down to from + 41:                        // p > from + 40
        if text[p-1] in {',', ';', ':', '—', '–'} and text[p] is whitespace:
            cut = p
            if p - from <= 200: break
    // result: the highest clause break at ≤ 200 chars from `from`; if none, the lowest one in (200, 280]
    if cut < 0: break
    while cut < end and text[cut] is whitespace: cut++
    if cut >= end: break
    out.push(cut); from = cut
return out
```

**`wordSpans(text, start, end)`**: every `/\S+/` run inside `[start, end)` as `{start, end}` absolute offsets.

**`sentenceAt(sentences, textLength, charIndex)`**: `index = max(0, lastIndexAtOrBefore(sentences, charIndex))`;
`start = sentences[index] ?? 0`; `end = sentences[index+1] ?? textLength`.

---

## 6. Output warmer — `src/renderer/src/widget/output-warmer.ts`

**Why:** many laptop codecs (Realtek power saving) and Bluetooth headsets power down after a few seconds of silence and
clip the start of the next sound while waking — i.e. the first words of a read. So while a read is active (and for a
while after it pauses or ends) an inaudible signal keeps the device running, and speech starts only once the output
has actually rendered 1 s of that signal.

**Why measure on the audio clock:** "rendered" is measured with the audio context's own clock (`currentTime`), which
only advances while the device consumes audio. The context state is unreliable: Chromium reports a new context
`running` ~0.7 s before the first frame reaches the device on a first start.

| Constant | Value | Meaning |
|---|---|---|
| `WARM_MS` | 1000 | audio the output must have rendered since it last started before speech starts |
| `MAX_WAIT_MS` | 3000 | one warm-up never holds playback longer than this from its first request |
| `POLL_MS` | 25 | warm-up poll interval |
| `RETRY_MS` | 60 000 | after the context could not be created, don't retry for this long (each try can block) |
| hold after end/stop/error | 30 000 ms | (player `IDLE_OUTPUT_HOLD_MS`) |
| hold after pause | 60 000 ms | (player `PAUSED_OUTPUT_HOLD_MS`) |

**The keep-alive signal** (`:64-75`): context with `latencyHint: 'playback'`; a 1-second mono buffer at the device
sample rate, looped forever, connected to the output:
```
half = max(1, round(sampleRate / 100))               // 10 ms
sample[i] = ((floor(i / half) % 2 == 0) ? +1 : -1) / 32768
```
A ±1 LSB (16-bit) square wave flipping every 10 ms ≈ −90 dBFS: far below hearing but **not digital silence**, which
some drivers (and Chromium's silent-output suspender) treat as "nothing playing" and power down.

**State:** `ctx`, `startedAt` (audio-clock time when the output was first *seen* running since it last started; null
while not running), `suspended` (our own suspend is in effect or in flight — the state may still read `running` until
it completes), `pending` (shared warm-up promise), `gaveUp` (this warm-up timed out), `failedAt`, `releaseTimer`,
`disposed`.

```
context():                          // lazy creation
  if ctx or disposed: return ctx
  if failedAt and now - failedAt < 60 000: return null
  try create context + looping signal; on state change to anything but 'running': startedAt = null
      ctx = c; failedAt = null
  except: failedAt = now
  return ctx

rendered():                         // seconds rendered since the output last started
  if !ctx or suspended or ctx.state != 'running': startedAt = null; return 0
  startedAt ??= ctx.currentTime
  return ctx.currentTime - startedAt
isWarm = rendered() >= 1.0

prepare():                          // returns null (start now) or a never-rejecting promise
  cancel pending release
  c = context(); if !c: return null                         // warming impossible → don't wait
  if suspended or c.state != 'running': suspended = false; startedAt = null; c.resume()
      // after our own suspend() the browser queues resume behind it, so it ends up running either way
  if gaveUp or isWarm: return null
  if !pending:
      deadline = now + 3000
      pending = poll every 25 ms: if disposed or isWarm or now >= deadline:
                    if !disposed and !warm: gaveUp = true
                    pending = null; resolve
  return pending

preload():                          // build the context while idle, leave it suspended
  if ctx or disposed: return
  c = context(); if !c: return
  suspended = true; startedAt = null; c.suspend()           // nothing plays yet; the device may sleep

release(afterMs):
  cancel pending release; if !ctx or disposed: return
  after max(0, afterMs): suspended = true; startedAt = null; gaveUp = false; ctx.suspend()
      // stopping the stream lets the device (and the system) idle; the next start warms afresh

dispose(): disposed = true; cancel release; close ctx; ctx = null; startedAt = null
```
`preload()` is scheduled when the widget starts, on an idle callback (timeout 5000 ms) or after 1000 ms: the first
context construction can block the page for most of a second, which must not happen when a read starts.

After a timeout (`gaveUp`), nothing waits for the output again until it has been released (suspended) and restarted.

Further details:
- `startedAt` is set lazily by the first `rendered()` call that sees the context running (normally the first 25 ms
  poll, which runs immediately when the wait is created), so "1 s rendered" is measured from the first observation
  after a (re)start. Measuring from the stream's real start (frames consumed since `Start()`) is equivalent or better.
- A context that exists but never reaches `running` (broken device) costs one 3 s wait, then `gaveUp` makes every
  later `prepare()` return null at once until a release timer fires (30 s after end/stop/error, 60 s after pause),
  which clears `gaveUp`. Creation failure (no device at all) never waits; creation is retried at most every 60 s.
- `release()` replaces any earlier scheduled release (last call wins) and does not affect an in-flight warm-up wait.
- The keep-alive keeps playing underneath speech for as long as the output is held; it is never paused separately.
- The `isWarm` getter and `dispose()` exist but nothing in production calls them (the player environment exposes only
  `prepare`/`release`, and the warmer lives as long as the widget page).
- The browser's audio context follows the system default output device. A native stream must do the same (re-open on
  default-device change) or speech and keep-alive can end up on different devices.

**When the player calls it:**

| Player moment | Call |
|---|---|
| `startSession` | `prepareOutput()` — warming overlaps synthesis of the first chunk |
| `play()` while waiting for chunks | `prepareOutput()` |
| `playCurrent()` | `prepareOutput()`; gate only if the element is paused |
| external play | `prepareOutput()` |
| `pause()`, external pause, `NotAllowedError` | `releaseOutput(60 000)` |
| `stop()`, fatal error, `finish()` | `releaseOutput(30 000)` |

Consequences: a read started within 30 s of the previous one ending, or resumed within 60 s of a pause, starts
instantly (the stream never stopped). A new session's `stop()` schedules a release that its own `prepareOutput()`
cancels immediately.

---

## 7. Widget controller — `src/renderer/src/widget/controller.ts`

`connectWidget(player, api, {autoHideMs = 4000, holdRetryMs = 1500})`.

State: `autoHide = true`, `holds: Set<'hover' | 'toast'>`, `hideTimer`, `hidden = true` (the window is created
hidden), `appliedRate = null`, `appliedVolume = null`, `disposed`.

**Settings application** (`:71-83`), on startup (`settings.get()`) and on every settings change:
- If `settings.playbackRate != appliedRate` → `appliedRate = it`; `player.setRate(it)`. Same for `volume`.
  Re-applied **only when the setting value changes**, so an unrelated settings write (theme, history…) never overrides
  a rate/volume command that came from the app or the browser bridge (those commands do not persist). Each new
  session still starts at `settings.playbackRate`/`volume` from the `sessionStart` payload. A changed value applies to
  the read that is playing **immediately** (e.g. changing Playback rate in Settings mid-read), and with no session
  it just sets the player's stored rate/volume. The first application (startup) always calls `setRate`/`setVolume`
  (no-ops when equal).
- `autoHide = settings.widget.autoHide !== false`; `syncAutoHide()`.
- Null settings, and anything after `dispose()`, are ignored; a failed startup `settings.get()` is ignored.

**Events wired:**
- `sessionStart` → clear the hide timer; `hidden = false` (main shows the window for every new read when the widget is
  enabled; the flag is set even when the widget is disabled and the window stays hidden); `player.startSession(s)`.
- `chunk` / `sessionDone` / `sessionError` → player.
- `command` → `had = player.hasSession`; `player.command(cmd)`; if `cmd.type == 'stop'` and `had` → `hide()`.
- player subscription → `syncAutoHide()`.

**Auto-hide rules** (`:55-69`):
```
shouldAutoHide = status == 'ended' && autoHide && !warning     // a read that stopped early keeps its card up
syncAutoHide(): if shouldAutoHide: (if no timer) scheduleHide(4000)  else clear timer
scheduleHide(ms): after ms: if !shouldAutoHide: return
                            if holds non-empty: scheduleHide(1500)   // retry while hovered / toast shown
                            else hide()
hide(): hidden = true; ask main to hide the window
```
Only `ended` auto-hides. `error` cards and `paused`/idle-after-toast states never auto-hide (an `idle` widget is hidden
by `stop`/close instead, or by toast dismissal §8.4).

`scheduleHide` always clears an existing timer first. `syncAutoHide` runs on every player notification and every
settings change, but only *starts* a timer when none is pending, so repeated notifications do not postpone the hide;
anything that makes `shouldAutoHide` false (replay, new read, a warning, `autoHide` switched off) cancels it, and
switching `autoHide` on while a finished card is showing starts a fresh 4 s. Auto-hide only hides the **window**: the
player keeps its `ended` session, so the media overlay still lists the read (§9) and play/pause (hotkey or media key)
replays it while the window stays hidden (§2.9).

**Holds:** `'hover'` while main reports the cursor over the card (main polls the cursor every 100 ms while the widget
is visible and sends `widget:hover` on change — the card's text labels are window-drag handles that Windows treats as
caption area, so the page gets no pointer events there); `'toast'` while a toast is on screen.
`holdAutoHide(hold, reason = 'hover')` adds/removes a reason. Hover is computed by main against the card rectangle the
widget reports (window-relative layout box of the card, falling back to the whole window), polling
`HOVER_POLL_MS = 100` only while the widget is visible; a change is sent only when the boolean flips; when the window
hides, polling stops and the next show re-evaluates (`windows.ts:456-498`). When the hover subscription is torn down
the hold is released.

**`close()`** (close button, media "stop"): `player.stop()`; send `{type:'stop'}` to main (stops synthesis; main echoes
the stop back, a no-op now); `hide()`. The close button hides the window even when nothing is playing (idle card,
standalone toast).

**`hide()`** only sets `hidden = true` and asks main to hide the window; it never touches playback.

**`dispose()`** (page unload): mark disposed, clear the hide timer, unsubscribe everything, `player.dispose()` (which
stops the session and reports a final `idle`).

---

## 8. Presentation rules derived from player state (widget UI)

Not the visual design (see the widget UI spec), but the logic that maps state to what the user sees.

### 8.1 Window visibility (main)

- `showWidget()`: if hidden, position it (per `widget.position` setting) and show **without activating** (Electron
  `showInactive`); re-assert always-on-top at the "floating" level; start hover tracking. The window is
  non-focusable, frameless, transparent, skip-taskbar. Native: `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`,
  `ShowWindow(SW_SHOWNOACTIVATE)` / `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)`.
- Shown on every `read()`/`replay()` start if `widget.enabled`; shown for toasts (§2.11). Hidden by the widget itself
  (close, stop, auto-hide, toast dismissal) and by main when `widget.enabled` is switched off. Calling `showWidget()`
  on an already visible widget does not move it (it only re-asserts topmost and refreshes hover tracking); a change of
  the placement settings while visible repositions it (`windows.ts:678-685`, `index.ts:313-314`). Commands (hotkeys,
  tray, bridge, media keys) never show it.
- Audio plays regardless of visibility; with the widget disabled the player still runs hidden.

### 8.2 Status phase labels (`WidgetView.tsx:49-60, 333`)

| Condition (first match) | Label |
|---|---|
| `status == 'ended' && warning` | `STOPPED EARLY` |
| `snapshot.starting` | `STARTING` |
| `idle` | `READY` |
| `loading` | `SYNTHESIZING` |
| `playing` | `READING` |
| `paused` | `PAUSED` |
| `ended` | `FINISHED` |
| `error` | `ERROR` |

So `loading` means SYNTHESIZING when waiting for audio (initial or mid-read catch-up) and STARTING when the audio is
ready but the output is still warming. The animated 3-dot loader follows the label **whenever `status == 'loading'`**,
i.e. for both SYNTHESIZING and STARTING (`WidgetView.tsx:334`). Status glyph: `error` → red LED; `playing`/`paused`/
`loading` → 4×4 dot-wave (animated only while playing, styled as loading while `loading`); `ended` → alert icon if
stopped early, else check; otherwise the logo. The error card always shows `ERROR` (no loader); the idle card `READY`.

Source label (top-left): `(sourceApp || (fromCache ? 'Replay' : 'Vox')).toUpperCase()`; the idle card always shows
`VOX`. Replays from cache usually carry the original `sourceApp` (§2.6), so `REPLAY` appears only for records
without one.

Which card is rendered (`WidgetView.tsx:119-123`, first match): a toast when `toastStandalone` or no session →
standalone toast card; `status == 'error'` → error card (a toast shows as a banner on it); no session → idle card;
`widget.compact` → compact pill; else the full player card. The card is re-created (enter animation) when the session
id changes.

### 8.3 Time left and the "≈" rule (`WidgetView.tsx:91-107`)

```
rate = state.rate > 0 ? state.rate : 1
if status == 'ended':  label = formatClock(durationSec / rate)                 // total length at current rate
else:
    left  = formatClock(max(0, durationSec - positionSec) / rate)            // remaining wall time at current rate
    label = durationIsEstimate ? '≈' + left : '−' + left                     // U+2248 / U+2212 minus sign
formatClock(s): s = max(0, floor(s || 0)); h = s/3600; m = (s%3600)/60; sec = s%60   // integer division
               h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`                      // 263 → "4:23", 3723 → "1:02:03"
               // NaN/undefined → "0:00" (`renderer/src/lib/format.ts:2-9`); minutes unpadded when h == 0
```
On `ended` the label has no prefix (neither `≈` nor `−`).
`≈` appears while synthesis has not finished (`durationIsEstimate = !done`) and is rendered dimmer (tertiary text
colour), accessible label `about {time} left`. Cached replays get `done` immediately, so they show `−` at once.
Progress bar a11y text: `{clock(pos)} of {about }{clock(dur)}`.

### 8.4 Toasts on the widget and their interaction with hiding (`Widget.tsx:53-131`)

- Toast durations: 2500 ms; 5000 ms if it has an action button. While hovered, dismissal is re-checked every 1000 ms.
- On arrival record `ShownToast {toast, revealed, sessionId}`: `revealed = controller.hidden || (previous toast revealed && previous.sessionId == current sessionId)`; `sessionId` = player session id at arrival.
- `hidesOnDismiss(shown, status, sessionId) = status == 'idle' || (status == 'ended' && shown.revealed && shown.sessionId == sessionId)` — dismissing hides the window again only when nothing else would be on screen, or for a finished read whose window had already hidden itself before the toast.
- `toastStandalone = (current sessionId == null) || hidesOnDismiss(...)`: standalone toast card; otherwise a banner
  replacing the text area of the player card (compact: replacing the progress bar).
- Only one toast is shown: a new toast replaces the current one and restarts its timer (it inherits `revealed` as
  above). A toast with a non-string `message` is ignored. The dismissal timer re-runs whenever the shown toast
  changes. Dismiss = clear it, then hide the window if `hidesOnDismiss` (evaluated with the status and session **at
  dismissal**). Clicking a toast's action opens the app window at `action.route` and dismisses the toast.
- While a toast is shown, the `'toast'` auto-hide hold is on (§7).
- The toast that `read()` sends before its session starts (truncation notice) arrives while no session exists, so it
  first shows standalone and turns into a banner as soon as `sessionStart` arrives.
- Glyph per kind: `error` → alert, `success` → check, `info` → info. Error toasts are announced as alerts, others as
  status.

### 8.5 Speed cycle and rate label

- Rates offered: `PLAYBACK_RATES = [0.75, 1, 1.25, 1.5, 1.75, 2]` (`defaults.ts:84`).
- `nextRate(rate) = first r in PLAYBACK_RATES with r > rate + 0.001, else 0.75` (wraps; any rate ≥ 2, e.g. 2.5 set from
  the bridge, cycles to 0.75).
- Clicking the rate button: `next = nextRate(player's current rate)`; `player.setRate(next)` immediately **and**
  persist `settings.playbackRate = next` (the resulting settings event re-applies the same value: no-op; a failed
  settings write is ignored). Unlike rate commands from the app or bridge, the widget's cycle **persists**, so the
  next read starts at that rate.
- Label `formatRate(rate) = Number(rate.toFixed(2)) + '×'` → `1×`, `1.25×`, `0.75×`, `2.5×` (two decimals, then
  trailing zeros and a trailing dot dropped). Shown when a session exists and status ≠ `error`; accessible label
  `Playback speed {label}`, tooltip `Playback speed`.
- Pitch is preserved at every rate (§4.2).

### 8.6 Transport and progress enabling (`WidgetView.tsx:160-186, 272-289`)

- `hasAudio = bufferedFraction > 0`. Previous disabled without audio; Next disabled without audio or when `ended`.
- Main button: `ended` → "Play again" (replay icon; action `toggle` → `play()` → `seek(0)`); `wantsPlay` → "Pause";
  else "Play".
- Labels: `partialText` → `Back 10 seconds` / `Forward 10 seconds`; else `Previous sentence` / `Next sentence`.
- Progress value `positionSec / durationSec` (0 if duration 0; 1 when `ended`), buffered overlay `bufferedFraction`,
  knob and seeking enabled only when `bufferedFraction > 0`; a seek sends the clicked fraction to `seekFraction`.
- Compact layout: while `loading` with `bufferedFraction <= 0` a "Synthesizing" loader replaces the progress bar; a
  stopped-early note replaces bar and time when `ended && warning`; a small red dot with tooltip
  `Stops early: {message}` while a warning exists and the read is still going.

### 8.7 Word highlighting (`SentenceText.tsx`, `Widget.tsx:213`, `WidgetView.tsx:309-320`)

- Enabled = live `settings.widget.highlightWords` (falls back to the session's flag, then true). Words are shown only
  if enabled, a session exists, status ≠ `ended`, and `!partialText`; otherwise the 2-line clamped title is shown.
- Only **the sentence containing `charIndex`** is displayed (`sentenceAt`). Words = `wordSpans` of that sentence, joined
  by single spaces (original whitespace is not reproduced).
- `started = positionSec > 0 || status == 'playing'`; current word index = `started ? lastIndexAtOrBefore(wordStarts, charIndex) : -1`.
- Styles: words before current = "spoken" (white 62% opacity), current = white 100%, after = "upcoming" (white 30%);
  colour transition 140 ms linear. Box is two lines tall (19 px font, 25 px line height, 50 px). Long sentences scroll
  so the current word's line is at the top: `translateY(-min(currentWord.offsetTop, scrollHeight - viewportHeight))`,
  260 ms ease (`cubic-bezier(0.2, 0.8, 0.2, 1)`). A new sentence fades in over 240 ms (from opacity 0, +4 px): the
  sentence block is keyed by its index, so each new sentence is a fresh block. Long words wrap anywhere. With the
  OS "reduce motion" preference, the fade, the scroll transition, the card enter animation and the toast banner
  animations are disabled (`Widget.css:749-756`).
- Before playback has started (`positionSec == 0` and not `playing`, e.g. SYNTHESIZING or STARTING) every word is
  "upcoming". The highlighted word follows `charIndex`, which only changes when the snapshot is re-read (per frame
  while playing, §8.9), so highlighting is as fine as the alignment and the frame rate.

### 8.8 Warning and error presentation (`WidgetView.tsx:62-85, 232-270, 396-428`)

Warning line (non-fatal error): lead `Stopped early` if `ended` else `Stops early`, then ` · ` and a reason:
```
WARNING_REASON = { 'no-api-key':'no API key', unauthorized:'API key rejected', quota:'out of credits',
                   'rate-limit':'ElevenLabs is busy', network:'connection lost', 'too-long':'text too long' }
default: 'synthesis failed'
```
Codes in `SETTINGS_ERRORS = {no-api-key, unauthorized, quota}` make the line a link to Settings → Account (tooltip
`{message}\nOpen settings`); otherwise tooltip = message.

Error card (fatal): title from
```
ERROR_TITLE = { 'no-api-key':'Add your ElevenLabs API key', unauthorized:'ElevenLabs refused the request',
  quota:'Out of ElevenLabs credits', 'rate-limit':'ElevenLabs is busy right now', network:"Can't reach ElevenLabs",
  'empty-text':'Nothing to read there', 'too-long':'That selection is too long' }
default: 'Something went wrong'
```
(`unauthorized` is deliberately neutral: a 401 can also mean missing TTS permission or a flagged account.) Detail =
`state.error` if present and different from the title, else `The text could not be read aloud.` Button:
`SETTINGS_ERRORS` → `Open settings` (Settings → Account); `RETRY_ERRORS = {network, rate-limit, unknown}` with a session
→ `Try again` (§2.2); any other code (`empty-text`, `too-long`, `not-cached`, `no-selection`) → no button. The error
card keeps the top bar (source label, close button; no rate button). `errorCode` missing → treated as `unknown`.
Idle card: `Nothing playing` / `Select text, right-click, Read with Vox` / `READY`, an empty progress bar, only a close
button.

### 8.9 UI refresh policy (`usePlayerSnapshot.ts`)

Discrete changes arrive via `player.subscribe`; while `playing`, a per-frame loop re-reads the snapshot. A redraw
happens only when this key changes:
```
status | session.id | wantsPlay | buffering | starting | warning.code:warning.message | partialText | charIndex |
floor(positionSec*10) | round(durationSec*10) | durationIsEstimate | round(bufferedFraction*1000) | rate | volume | error
```
Mechanics: every player notification "kicks": re-read the snapshot once and, if the status is `playing` and no frame
loop is running, start one. The frame loop re-reads every display frame and stops by itself as soon as the status is
not `playing`. The 500 ms state reports do not drive the UI. So the clock text, progress bar and highlighted word move
at frame rate while playing, and only on discrete changes otherwise.

### 8.10 Tray (consumer of `PlayerState`, `tray.ts`)

Icon "active" while `playing` or `loading`. Tooltip: `Vox · Reading “{title}”`, `Vox · Paused “{title}”` (without the
quoted part if no title), `Vox · Preparing audio…` while loading, else `Vox`; max 127 UTF-16 units, trimmed with `…`
(keeping the closing `”`). Menu "Pause"/"Resume" enabled when playing/paused; "Stop" enabled when
playing/paused/loading.

---

## 9. Media session / media keys — `src/renderer/src/widget/useMediaSession.ts`

Active only when the platform supports it, `settings.triggers.mediaKeys` is on (default true), a session exists, and
status ≠ `error`.

| Aspect | Behaviour |
|---|---|
| Metadata | `{title: session.title || 'Vox', artist: session.voiceName, album: 'Vox'}`; cleared when inactive |
| `play` | `player.play()` |
| `pause` | `player.pause()` |
| `stop` | `controller.close()` (stops playback and synthesis, hides the widget) |
| `previoustrack` / `nexttrack` | `skipSentence(-1)` / `skipSentence(+1)` |
| `seekto` | `player.seek(seekTime)` if a finite number |
| `seekbackward` / `seekforward` | `skipSeconds(-(seekOffset || 10))` / `skipSeconds(seekOffset || 10)` |
| Handlers | all registered while active, all removed when inactive (unsupported actions ignored) |
| Playback state | inactive → `none`; `playing` → `playing`; everything else (incl. `loading`, `ended`) → `paused` |
| Position state | when active and `durationSec > 0`: `{duration, position: clamp(positionSec, 0, duration), playbackRate: rate > 0 ? rate : 1}`; otherwise cleared. Refreshed when `active`, status, rate, `floor(positionSec)` (once per second, covers seeks) or `round(durationSec)` change — sub-second estimate refinements do not resend. Rejected values (e.g. position > duration mid-estimate) are ignored. The OS extrapolates the position from position + rate between updates. |
| Lifetime | Metadata is set when the session becomes active and is not refreshed during it (title/voice do not change). Handlers always call the latest actions. `ended` keeps the session **active** (state `paused`): the overlay keeps listing a finished read, also after auto-hide, and hardware Play replays it from the start (`play()` on `ended`), with the window hidden if it auto-hid. Close/stop (session gone) or an `error` makes it inactive (metadata cleared, state `none`, handlers removed). |

Platform detail: in Chromium, even with no handlers the OS could still play/pause the `<audio>` element (default
actions) and list the app in the Windows media overlay. When `mediaKeys` is off **at startup**, main disables the
Chromium features `HardwareMediaKeyHandling` and `MediaSessionService`; toggling the setting therefore shows the toast
`Restart Vox to finish turning media keys on|off` (`index.ts:324-328, 522-525`). The player's external pause/play
handlers (§4.7) exist for those direct OS actions. A native port using SMTC can apply the setting live.

---

## 10. Persisted fields this subsystem writes (`ReadRecord`)

| Field | Written by | Rule |
|---|---|---|
| all creation fields | `read()` | §2.3 |
| `listenedSec` | `writeSession` | `round1(base + session listened)`; wall seconds over all sessions incl. replays |
| `completed` | `writeSession` | sticky once true (`baseCompleted || completed`) |
| `maxPositionSec` | `writeSession` | furthest audio position, only increases; stored only when > 0 (0 is omitted from JSON) |
| `listenLog` | `writeSession` | hour ledger §11 |
| `audioDurationSec`, `cached`, `charactersBilled` | engine `complete` | §2.13 |
| `charactersBilled`, `error = PARTIAL_READ_ERROR` | engine `stopped` | §2.13 |
| `error` | engine `failed`, `engine.start` throw | user-facing message |
| `cached = false` | `replay` miss, `pruneCache` eviction | |

Note: `audioDurationSec` is the sum of the engine's chunk durations, while `maxPositionSec` on `ended` is the player's
`durationSec` (decoded durations), so the two can differ by a little; consumers clamp the ratio.

Consequences worth keeping in mind:
- A **partial** read (a later chunk failed) never gets `complete`, so its `audioDurationSec` stays 0 and it is never
  cached; its `maxPositionSec` can still reach the end of the audio that arrived. The History list therefore shows
  0% heard for it (`audioDurationSec <= 0 → 0`), while analytics estimates against `charCount / 15`.
- A read whose chunks failed to **decode/play** (not synthesis) still ends normally and is marked `completed`, heard
  100%.
- A read stopped mid-synthesis keeps `audioDurationSec = 0` and `cached = false`, but its `charactersBilled` is
  recorded by `stopped`.
- With `saveHistory` off nothing in this table is written; replays and the "Heard %" do not exist for that read.

---

## 11. Listening ledger — `src/shared/listen-log.ts`

Seconds listened per **local hour**, so analytics credit listening to when it happened (a replay today of an old read
counts today) rather than to `createdAt`.

```
LISTEN_LOG_MAX_KEYS = 400
KEY_RE = /^(\d{4})-(\d{2})-(\d{2})T(\d{2})$/
hourKey(date)    = `${YYYY}-${MM}-${DD}T${HH}` in local time, zero-padded       // e.g. "2026-10-17T09"
hourKeyDate(key) = local Date(y, mo-1, d, h) if KEY_RE matches and 1<=mo<=12, 1<=d<=31, h<=23 and the date is valid
                   else null
                   (JS Date rolls invalid days over, e.g. 02-31 → Mar 3; it is NOT rejected)
```

```
sanitizeListenLog(v):                          // untrusted ledger → clean ledger or undefined
  if not a plain object: undefined
  keep entries whose value is a finite number > 0 and whose key parses with hourKeyDate; value = round1(value)
  if none: undefined
  sort by key ascending (lexicographic == chronological); keep the LAST 400 (newest)
addToListenLog(log, key, sec):
  next = copy(log or {}); if finite(sec) && sec > 0: next[key] = round1((next[key] ?? 0) + sec)
  return sanitizeListenLog(next) ?? {}
sameListenLog(a, b): same key count and every a[k] === b[k] (both undefined → equal)
```

```
listeningEntries(r: {createdAt, listenedSec, listenLog?}) → [{at: Date, sec}]:
  total = (finite listenedSec > 0) ? listenedSec : 0
  out = entries for each ledger key with a valid date and finite sec > 0; logged = Σ sec
  if logged > total and logged > 0:            // ledger is written with listenedSec, so this only happens for bad files
      scale every entry by total/logged; logged = total
  rest = total - logged
  if rest > 0.05: out.push({at: createdAt, sec: rest})   // older versions / trimmed hours count at createdAt
  return out
```
Hours trimmed beyond 400 keys therefore fall back to "at `createdAt`".

Exact-behaviour notes (`listen-log.ts`):
- `hourKey` pads month, day and hour to 2 digits but does **not** pad the year (`getFullYear()`); a year below 1000
  would produce a key that `KEY_RE` rejects and the entry would be dropped. Irrelevant in practice.
- `hourKeyDate` uses JS `new Date(y, mo-1, d, h)`: years 0..99 are mapped to 1900..1999 by JS, and days past the end of
  a month roll over (`2026-02-31T05` → 2026-03-03 05:00) instead of being rejected. Time zone = the machine's local
  zone at the time of reading, so after a time-zone change old keys are interpreted in the new zone.
- Values are rounded with `round1` on every write and every sanitize; zero or negative values are dropped. A value
  that rounds to 0 is therefore dropped too.
- `sanitizeListenLog` rejects non-object input, arrays and null; non-number values (e.g. `"5"`) are skipped. The
  returned object's keys are in ascending order, which is also the key order written to `history.json`.
- `sameListenLog(undefined, {})` is true (both have zero keys); it is used by the history store to detect no-op
  updates.
- `listeningEntries` keeps entries in key order with unrounded scaled values; `at` for a ledger entry is the local
  start of that hour.

### 11.1 How the ledger and positions are consumed (other subsystems, for consistency)

- **Per-day / per-hour attribution** (`analytics.ts:181-204`): reads, words and characters count on the day of
  `createdAt`; listening seconds count at each `listeningEntries` timestamp (day bucket = local date of the hour,
  heatmap row = `(weekday + 6) % 7` so Monday = 0, column = hour). Listening before the range start or after today is
  ignored. Streaks count a day if a read was created that day **or** any ledger entry of ≥ 1 s falls on it.
- **Heard %** in analytics (`heardFraction`, `analytics.ts:36-44`):
  ```
  listened = max(0, listenedSec); if listened == 0: 0
  if completed: 1
  audio = audioDurationSec > 0 ? audioDurationSec : charCount / 15; if !(audio > 0): 1
  reached = maxPositionSec > 0 ? maxPositionSec : listened          // older records fall back to wall time
  return min(1, reached / audio)
  ```
  Note the order: a record with `listenedSec == 0` is 0 even when `completed`. Used for "heard words"
  (`wordCount × heardFraction`, summed over reads *created* in the range), average listening WPM
  (`round(heardWords / (Σ listenedSec of those reads / 60))`, 0 when no listening) and the silent-reading equivalent
  (`round(heardWords / 238 × 60)` s, `SILENT_READING_WPM = 238`).
- **Heard %** in the History list (`HistoryModel.ts:64-71`): `completed → 1`; `audioDurationSec <= 0 → 0`; else
  `min(0.99, max(0, (maxPositionSec > 0 ? maxPositionSec : listenedSec) / audioDurationSec))`, displayed as
  `round(fraction × 100)%` — an incomplete read never shows 100%.
- Because `maxPositionSec` is in audio seconds, "Heard %" is correct at any playback rate; `listenedSec` (wall time)
  is what the time totals use.

---

## 12. Edge cases and invariants (checklist for tests)

1. A new read while one plays: engine cancels the old session (old read gets `stopped` → `charactersBilled` if any
   were billed); player's `startSession` stops the old session, reporting it `idle` with its final `listenedSec`.
2. Duplicate `sessionStart` for the same session is ignored; chunks/done/errors for other sessions are ignored;
   out-of-order chunks are held until contiguous; duplicate chunk indexes ignored.
3. Pause holds synthesis beyond the look-ahead (no reports while paused, position constant).
4. Pause during SYNTHESIZING → `paused`; chunk arrives → stays `paused` with audio ready; `play()` starts it.
5. `done` while the player is waiting at the end of available audio → `ended` immediately.
6. Non-fatal error mid-read → warning shown ("Stops early · reason"); playback continues to the last good chunk →
   `ended` labelled STOPPED EARLY, card does **not** auto-hide, record not completed, `error` = partial message,
   nothing cached.
7. Fatal error → `error` card; play/seek/skip are no-ops; media session inactive; output released after 30 s.
8. `play()` on `ended` replays from 0 (and sets `wantPlay`); seeking or skipping back after `ended` resumes playback.
9. Next-sentence on the last sentence with synthesis done → `ended` (completes the read, heard 100%). Next when the
   next sentence's audio hasn't arrived → jump to end of available audio, then buffer.
10. Seek never goes past `available - 0.05` s; a chunk offset never past `dur - 0.05` s.
11. Rate change mid-play banks listening at the old rate first; seeks bank before jumping; listening never counts
    device spin-up, stalls or sleep.
12. A chunk that fails to decode/play is skipped; its duration still occupies the timeline.
13. Autoplay rejection (`NotAllowedError`) → `paused` with `wantPlay = false` (not an error).
14. Output warm-up: speech waits up to 3 s for 1 s of rendered keep-alive signal; if no output device / context
    cannot be created, no wait (retry creation after 60 s).
15. History off: no record, no caching, nothing written; replay impossible; widget retry falls back to re-reading
    the session text.
16. `storeText` off: record keeps 160 chars; cached audio stored with redacted alignment characters; cached replay
    rebuilds the text with `…` placeholders; cache miss → `not-cached` (no re-synthesis possible).
17. Record deleted during synthesis → `complete` does nothing (no orphan cache).
18. Replays of cached audio need no API key and accumulate `listenedSec` / `maxPositionSec` / `completed` into the
    original record, crediting the ledger at replay time.
19. Quit / log-off: `persistPlayback()` folds the last known progress into the records before the stores flush
    (asynchronously with a 3 s cap on quit; synchronously on `WM_QUERYENDSESSION`/`WM_ENDSESSION`).
20. Tracked sessions are capped at 16; late reports for evicted sessions are dropped harmlessly (history only; they
    still update main's last state, the tray and the app window).
21. Pause during STARTING (output warm-up) → `paused`; when the warm-up resolves the start is abandoned; `play()`
    later gates again only if the output is still not warm.
22. Seek or skip while `paused` stays `paused` at the new position; seek while buffering into available audio resumes.
23. Next-sentence on the last sentence while `paused` and `done` → `ended` (FINISHED, completed).
24. Paused while buffering at the end of available audio, then `done` arrives → `ended` without playing anything more.
25. A read whose chunks failed to decode reaches `ended` and is recorded as completed / heard 100%; only synthesis
    failures (non-fatal `TtsSessionError` + `stopped` with error) make it partial.
26. Error card: the play/pause hotkey sends `toggle`, which does nothing; only close (or a new read / "Try again")
    leaves it. The media session is inactive in `error`.
27. Auto-hidden `ended` read: play/pause hotkey or media Play replays it with the window still hidden; the media
    overlay keeps listing it until close/stop or a new read.
28. Output device absent (context cannot be created): reads start immediately, no warm-up wait; a context that exists
    but never runs costs one 3 s wait, then no waits until the next release.
29. Widget renderer crash mid-read: the reloaded player is `idle`, main's engine session is not stopped (look-ahead
    stalls), and the play/pause hotkey is dead until a stop or a new read.
30. A settings write that does not change `playbackRate`/`volume` never overrides a rate/volume set by a command;
    changing them in Settings applies to the current read immediately.
