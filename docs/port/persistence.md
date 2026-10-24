# Vox port spec: persistence, history, audio cache, analytics, demo data, logging

Scope: everything Vox keeps on disk and everything computed from it. Source of truth (TypeScript, Electron):

| Area | Source |
|---|---|
| Profile path helper | `src/main/services/paths.ts` |
| Durable JSON engine (atomic writes, `.bak`, debounce) | `src/main/services/json-file.ts` |
| Settings + API key | `src/main/services/settings-store.ts`, `src/shared/defaults.ts`, `src/shared/types.ts` |
| History | `src/main/services/history-store.ts`, `src/shared/listen-log.ts` |
| Audio cache | `src/main/services/audio-cache.ts` |
| Analytics | `src/main/services/analytics.ts` (+ display maths in `src/renderer/src/app/charts/model.ts`) |
| Demo data | `src/main/services/demo-data.ts` |
| Logging + runtime flags | `src/main/log.ts` |
| Callers that define policy (retention timers, cache pruning, preview-only mode, session end) | `src/main/index.ts`, `src/main/reader.ts`, `src/main/ipc.ts`, `src/main/windows.ts` |

All line references are `file:line` in the original repo (`C:\Users\Kwaku\Desktop\Desktop Widget`). Facts marked
**[observed]** were checked against a real profile on the developer machine (structure only, no contents).

The Rust app should read and write the **same files in the same folder with the same formats**. Then "migration" is
zero-copy: the only thing that needs real work is decrypting the API key (section 4).

---

## 1. Profile location, runtime modes, on-disk layout

### 1.1 Where the profile lives

- `userData` = `%APPDATA%\Vox` (Known Folder `FOLDERID_RoamingAppData` + the product name `Vox`). Electron derives
  it from `package.json` `productName: "Vox"`. Windows paths are case-insensitive, so `%APPDATA%\vox` is the same
  folder. **[observed]**
- Every service resolves paths through `userDataFile(...parts)` = `join(userData, ...parts)`, re-evaluated on every
  call so a redirect made before the stores initialise (demo mode) is honoured (`paths.ts:4-10`). The stores fix
  their file path when they initialise (settings/history at `init()`, the log at its first line); the cache and
  `secrets.bin` resolve it on each operation. The profile folder is created implicitly by the first write
  (`mkdir -p` in every writer).
- **Demo / screenshot mode** redirects `userData` to `%APPDATA%\Vox-Demo` (`log.ts:25-31`). This happens before
  anything touches the profile (log.ts is the first import of index.ts for this reason, `index.ts:1-3`). Demo data
  must never touch the real profile.
- Chromium's own session data (HTTP cache, Local Storage, **`Local State`** which holds the API-key wrapping key) is
  redirected to `userData\Session` (`index.ts:533-536`). Reason: Chromium's HTTP cache folder is `Cache`; on a
  case-insensitive file system a `cache` folder of ours would be the same directory, and our cache `init()` deletes
  anything that is not a read id.

### 1.2 Runtime flags (environment variables) — `log.ts:10-34`

A flag is on when the variable is exactly `1` or `true` (case-sensitive).

| Variable | Effect |
|---|---|
| `VOX_DEMO` | `demo = true`: profile becomes `%APPDATA%\Vox-Demo`, history seeded (section 8), `mockTts` implied. |
| `VOX_SHOTS_DIR=<path>` | Screenshot harness output dir (any non-empty value; resolved to absolute against the process working directory). Implies `demo` and `mockTts`. |
| `VOX_MOCK_TTS` | No network TTS (generated WAV). Implied by demo. |
| `VOX_DEBUG` | `debug`-level log lines are written (otherwise dropped). |
| `ELEVENLABS_API_KEY` | Fallback API key when `secrets.bin` is missing / undecryptable (section 4.4). Never persisted. |

In demo mode the app also skips launch-at-login and Explorer-menu registry writes (`index.ts:176-190`), forces
`onboarded = true` (`index.ts:432-435`), and serves canned voices/models/account when no key is set (`ipc.ts:200-235`,
`302`) — those belong to other subsystems but are listed so the demo profile behaves the same.

### 1.3 Complete layout of `%APPDATA%\Vox`

```
%APPDATA%\Vox\
  settings.json            pretty JSON (2-space, LF, no trailing newline). Settings minus hasApiKey.  §3
  settings.json.bak        byte-identical mirror of the last committed settings.json
  settings.json.tmp        transient: async atomic write in progress
  settings.json.tmp-sync   transient: synchronous (log-off) atomic write in progress
  settings.json.corrupt    only after an unreadable settings.json was found (kept for manual recovery)
  secrets.bin              encrypted API key (Chromium OSCrypt "v10" blob). No .bak.          §4
  secrets.bin.tmp          transient
  history.json             compact JSON {"version":1,"records":[...]} newest first           §5
  history.json.bak / .tmp / .tmp-sync / .corrupt   same roles as for settings.json
  audio-cache\                                                                                §6
    <readId>\              one folder per cached read (readId = lowercase UUID v4)
      manifest.json        compact JSON, written last (commit marker)
      manifest.json.tmp    transient
      0.mp3, 1.mp3 ...     one file per synthesized chunk: <index>.<mp3|wav|ogg|bin>
  logs\
    main.log               current log, rotated at 1 MiB                                      §9
    main.1.log             previous log (single generation)
  Session\                 Chromium session data (not ours). Contains "Local State" (§4).
    Local State            JSON; os_crypt.encrypted_key = DPAPI-wrapped AES-256 key
    Cache\, Local Storage\, Network\, GPUCache\ ...  (renderer state; nothing the port needs except Local State)
```

**[observed]** on the dev machine: `settings.json` 1,344 B, `history.json` 286 KB / 206 records, 199 cache entries
(199 MB), `secrets.bin` 82 B, `Session\Local State` 490 B, no `Local State` at the profile root.

Other files written outside the profile: history export (user-chosen path, default
`%USERPROFILE%\Documents\Vox-history-YYYY-MM-DD.json`, §5.9). The export goes through the atomic writer, so a
transient `<chosen path>.tmp` also appears in the export folder.

Leftover temp files (`*.tmp`, `*.tmp-sync`, `manifest.json.tmp`) after a crash are **never read and never cleaned up
at startup**; the next write to the same target simply overwrites them. Inside an audio-cache entry, a stray
`manifest.json.tmp` next to a valid `manifest.json` is ignored; without `manifest.json` the whole entry is deleted by
the startup scan (§6.5).

There is **no schema-migration framework**. Every file is `version: 1`; "migration" means *normalise on load
(drop unknown keys, clamp, fill defaults) and rewrite if the normalised form differs* (§3.4, §5.3, §6.3).

---

## 2. Durable JSON engine (`json-file.ts`)

Used by `settings.json` and `history.json` (via `JsonFile`), and by `secrets.bin`, cache manifests and the history
export (via `writeFileAtomic` directly).

### 2.1 Windows file-busy retry — `json-file.ts:19-33`

Antivirus and the search indexer briefly hold files open, surfacing as `EPERM` / `EBUSY` / `EACCES`
(Win32: `ERROR_ACCESS_DENIED` 5, `ERROR_SHARING_VIOLATION` 32, `ERROR_LOCK_VIOLATION` 33). Such failures are retried
with delays **15, 40, 90, 180, 350 ms** (5 retries, 6 attempts total); any other error, or the 6th failure, is
thrown. Applied to: opening the temp file, rename, fallback copy, `.bak` write, and every JSON read (`readJsonFile`,
which also covers cache manifests).

The test is on Node's error *code string* (`EPERM`, `EBUSY`, `EACCES`). Node/libuv map Win32 errors to those as:
`ERROR_ACCESS_DENIED` (5) and `ERROR_PRIVILEGE_NOT_HELD` (1314) → `EPERM`; `ERROR_SHARING_VIOLATION` (32) and
`ERROR_LOCK_VIOLATION` (33) → `EBUSY`; `ERROR_NOACCESS` (998), `ERROR_CANT_ACCESS_FILE` (1920) and
`ERROR_ELEVATION_REQUIRED` (740) → `EACCES`. 5, 32 and 33 are the ones that occur in practice.

**Not retried** (single attempt; failure handling as described where each is used): `mkdir -p` of the parent, the
`write`/`fsync` of the temp file's content, deleting the temp file, everything in the synchronous variant (§2.3),
reading `secrets.bin`, deleting `secrets.bin`, copying to `.corrupt`, deleting `.corrupt`, the early synchronous
settings read (§3.5), audio chunk file writes and reads (§6.6, §6.7), and log writes (§9).

### 2.2 Atomic write — `writeFileAtomic(file, data, {backup})` — `json-file.ts:35-88`

Per-target-path FIFO queue: two writers to the same path never run concurrently (they would share `<file>.tmp`).
A failed write does not block the next one in the queue. The queue key is the exact path string (no
normalisation); every caller builds paths the same way, so that is enough. The queue entry is dropped once it is
the last one and has settled.

```
mkdir -p dirname(file)                                   [no retry; failure -> throw]
open "<file>.tmp" for write (create/truncate)            [retry]
write all bytes; fsync (FlushFileBuffers); close         [no retry; failure -> close, throw, .tmp left behind]
rename "<file>.tmp" -> file (replace existing)           [retry]
  on ANY failure after the retries (some scanners open files without FILE_SHARE_DELETE, which blocks replacing):
    warn "atomic rename failed for <file>, copying in place"
    copy "<file>.tmp" over file (NOT atomic)               [retry; failure -> throw, .tmp left, no .bak write]
    delete "<file>.tmp" (ignore errors)
if backup:
    write "<file>.bak" = same bytes (plain overwrite, not atomic)   [retry]; failure only warns
                                                                    "could not update <file>.bak"
```

Node specifics that the native port should reproduce: opening with `'w'` = `CREATE_ALWAYS`, `GENERIC_WRITE`, share
mode `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`; `rename` = `MoveFileExW(tmp, file,
MOVEFILE_REPLACE_EXISTING)`; `copyFile` = `CopyFileW` (overwrites in place). `data` may be a string (written as
UTF-8, no BOM) or raw bytes (`secrets.bin`).

Invariant (and the reason for the order): `.bak` is rewritten *after* the main file commits, so it always mirrors
the **last committed** state — never an older one that still contains records the user deleted. A torn `.bak` is
harmless because the main file was just committed; a torn main file (fallback copy interrupted) is recovered from
`.bak`, which still holds the previous commit. No directory fsync (not available on Windows).

### 2.3 Synchronous variant — `writeFileAtomicSync` — `json-file.ts:90-123`

Same algorithm, synchronous, no retries, temp name **`<file>.tmp-sync`** so it never collides with an async write
still in flight on `<file>.tmp`. Throws on failure (except `.bak` failure, which warns; a failed unlink of the
`.tmp-sync` after a fallback copy is silently left for the next sync write to overwrite). Used only at Windows
log-off / shutdown (§10.3), when the process can be killed as soon as the handler returns.

It does **not** go through the per-path queue of §2.2: it runs immediately, even while an async write of the same
file is half-way through. Correctness relies on the separate temp name plus the `rewriteAfterInFlight` rule of §2.5.
In a multi-threaded port this concurrency is real (not just interleaved I/O as in Node), so the two writers must
still never share a temp file, and the `.bak` writes of both may race (harmless: whichever lands last is a
complete, committed state).

### 2.4 Reading — `readJsonFile` — `json-file.ts:125-142`

Outcome is one of `ok(value)`, `missing`, `corrupt(error)`:
- `ENOENT` (`ERROR_FILE_NOT_FOUND`/`PATH_NOT_FOUND`) → `missing`. Any other read error (after retries) → `corrupt`.
- Decode UTF-8 **lossily**: invalid byte sequences become U+FFFD instead of failing (Node `readFile(f, 'utf8')`;
  Rust: `String::from_utf8_lossy`). Strip one leading BOM U+FEFF ("BOM added by Notepad").
- Empty or whitespace-only (JS `trim()`: all Unicode white space, line terminators and U+FEFF) → `corrupt` with
  error "file is empty". JSON parse error → `corrupt`.
- No schema validation here: **any** syntactically valid JSON is `ok`, including `null`, `[]`, `42` or `"text"`.
  Such a file never falls back to `.bak` and is never copied to `.corrupt`; the store-level sanitiser turns it into
  defaults / zero records (§3.4, §5.1).
- `JSON.parse` semantics the port must match (serde_json differs on the first and third):
  - a number outside the f64 range (`1e400`) parses as ±Infinity, which the sanitisers then treat as "not finite"
    → field fallback. serde_json rejects the **whole file** ("number out of range").
  - duplicate keys: last one wins (serde_json `Value` behaves the same).
  - an escaped lone surrogate (`"\ud83d"`) is accepted (§11.2). serde_json rejects the whole file.

### 2.5 `JsonFile<T>` — debounced durable document — `json-file.ts:144-303`

Options: `snapshot()` (called at write time so the newest state is saved), `debounceMs` (default 250),
`maxWaitMs` (default `debounceMs * 8`, and never less than `debounceMs`), `pretty` (2-space indent).

| Instance | debounce | maxWait | pretty |
|---|---|---|---|
| `settings.json` (`settings-store.ts:278-282`) | 250 ms | 2000 ms (default ×8) | yes |
| `history.json` (`history-store.ts:215-221`) | 2500 ms | 10 000 ms | no |

History uses the long debounce because the reader patches `listenedSec` every ~2 s while playing; a debounce longer
than that coalesces playback into one full-file write per `maxWait` instead of rewriting a possibly multi-MB file
every 2 s (`history-store.ts:217-218`).

State per instance: `timer` (debounce **or** retry timer; one slot shared by both), `firstScheduledAt`, `dirty`,
`failures`, the write chain, `inFlight` (an async write has taken a snapshot that is not on disk yet),
`rewriteAfterInFlight`. Read-only `pending` = `dirty`.

**load()** (`:184-197`):
1. Main `ok` → return value.
2. Main `missing` → return *nothing* (defaults). **No fallback to `.bak`: deleting the file is a deliberate reset.**
3. Main `corrupt` → warn `<path> is unreadable, trying backup: <error>`, copy main to `<file>.corrupt` (plain copy,
   overwrites an older `.corrupt`, no retry, failure ignored), read `<file>.bak`; `ok` → warn
   `restored <path> from backup` and return it; `.bak` missing or corrupt → return nothing (silently).

Note: after a restore from `.bak` the corrupt main file stays on disk until the next save (settings only rewrite if
the normalised value differs, §3.4); each start re-copies it to `.corrupt`. The port may rewrite immediately.

**schedule()** (`:200-210`): mark dirty; if no timer is pending, record `firstScheduledAt = now` (if one is pending,
cancel it and keep the old `firstScheduledAt`); (re)arm the timer for
`max(0, min(debounceMs, firstScheduledAt + maxWaitMs - now))`; when it fires, clear the slot and `write()`.
Continuous changes are therefore written at least every `maxWaitMs`. Because the slot is shared, a `schedule()`
while a failure-retry timer is pending replaces that retry timer and inherits its stale `firstScheduledAt`, so it
usually writes almost at once (wait clamps to 0).

**write()** (`:267-302`), serialised on a chain so writes never overlap (each `write()` call appends one run to the
chain; the checks below happen when the run starts, not when it was queued):
1. Not dirty → return. Else clear dirty, serialise `snapshot()` (2-space indent if `pretty`, else compact).
   Serialisation error → warn `could not serialise <path>`, return — `dirty` stays cleared, so that change is not
   retried until the next `schedule()`.
2. `inFlight = true`; `writeFileAtomic(path, text, {backup: true})`.
3. Success → `failures = 0`. Failure → dirty again, `failures++`, warn `could not save <path> (attempt <failures>)`,
   and if `failures <= 5` and the timer slot is empty, arm a retry after `1000 * failures` ms. So a failing file is
   tried **6 times** in total (initial attempt + retries after 1, 2, 3, 4 and 5 s); after the 6th consecutive
   failure (`failures = 6`) no retry is armed. `failures` is only reset by a successful write (async or sync), not
   by `schedule()`, so from then on each later `schedule()` makes exactly one more attempt with no automatic retry,
   until one succeeds. (Each attempt already includes the per-operation busy retries of §2.1.)
4. Finally `inFlight = false`; if `rewriteAfterInFlight` was set (see flushSync), clear it and `schedule()`.

**flush()** (`:213-219`): cancel the timer (debounce or retry) and append a write run to the chain; resolves when
that run (and everything before it) has finished; never rejects. Nothing is written if not dirty.

**flushSync()** (`:225-248`): cancel the timer; if neither dirty nor an async write in flight → return. Serialise
(error → warn `could not serialise <path>`, return) and `writeFileAtomicSync(path, text, {backup:true})`. On
success: not dirty, `failures = 0`, and if an async write was in flight set `rewriteAfterInFlight` (that older
async write may still rename its older snapshot over the newer file, so the newest state is re-scheduled once it
finishes). On failure: stay dirty, warn `could not save <path> synchronously`. Never throws. It writes even when
not dirty if an async write is in flight, because that write's data is not committed yet.

**discardCorruptCopy()** (`:254-260`): delete `<file>.corrupt` (no retry; ignore `ENOENT`, otherwise warn
`could not delete <path>.corrupt`). Never fails. Called when history records are removed so deleted reads do not
survive in the corrupt copy.

Warnings from this module (and from the stores and the audio cache, which reuse its `warn`) go to the console only
(`console.warn`, i.e. stderr) as `[data] <message>` or `[data] <message>: <error.message>` (`:6-9`) — **not** to
`main.log`. The port should send them to the log (scope `data`).

---

## 3. `settings.json` (`settings-store.ts`)

### 3.1 Stored shape (key order as written) — `types.ts:41-123`, `defaults.ts:9-81`

`hasApiKey` exists in memory only (derived) and is stripped before writing (`settings-store.ts:184-189`).

```jsonc
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
  "triggers": {
    "rightClickChip": true,
    "selectionPopup": false,
    "hotkeysEnabled": true,
    "hotkeys": { "readSelection": "Ctrl+Alt+R", "readClipboard": "Ctrl+Alt+V", "playPause": "Ctrl+Alt+P", "stop": "Ctrl+Alt+S" },
    "mediaKeys": true,
    "browserExtension": true,
    "explorerMenu": false,
    "excludedApps": [],
    "restoreClipboard": true
  },
  "widget": { "enabled": true, "position": "bottom-right", "customX": null, "customY": null,
              "autoHide": true, "highlightWords": true, "compact": false },
  "app": { "launchAtLogin": false, "startMinimized": false, "closeToTray": true, "theme": "system" },
  "library": { "saveHistory": true, "storeText": true, "cacheAudio": true, "cacheLimitMB": 500, "retentionDays": 0 },
  "text": { "skipUrls": true, "skipEmails": false, "maxChars": 20000 },
  "server": { "port": 47823 }
}
```

Formatting: `JSON.stringify(value, null, 2)`: `"key": value`, 2-space indent, LF, empty array as `[]`, integral
numbers without a decimal point (`1`, not `1.0`), shortest round-trip decimals (`0.75`), no trailing newline.

### 3.2 Field rules — `sanitizeSettings` — `settings-store.ts:85-171`

`fallback` = `DEFAULT_SETTINGS` when loading from disk, the **current** settings when applying a patch. So an
invalid value in a patch is ignored (the field keeps its current value), and an invalid value on disk becomes the
default. Unknown keys are dropped. `version` is always forced to `1`; `hasApiKey` is always the derived value.

| Field | Type | Default | Validation |
|---|---|---|---|
| `onboarded` | bool | false | bool else fallback |
| `voiceId`, `voiceName`, `modelId` | string | `JBFqnCBsd6RMkjVDRZzb`, `George`, `eleven_flash_v2_5` | non-empty: string → trim → first 200 UTF-16 units (not re-trimmed); `""` after trim or non-string → fallback |
| `outputFormat` | enum | `mp3_44100_128` | one of `mp3_44100_128`, `mp3_44100_64`, `mp3_22050_32` |
| `voice.stability` | number | 0.5 | finite → clamp [0,1] (not rounded) |
| `voice.similarityBoost` | number | 0.75 | clamp [0,1] |
| `voice.style` | number | 0 | clamp [0,1] |
| `voice.useSpeakerBoost` | bool | true | |
| `voice.speed` | number | 1 | clamp [0.7, 1.2] (ElevenLabs server-side speed) |
| `languageCode` | string | `""` (auto) | trim + lowercase; `""` allowed; else must match `^[a-z]{2,3}([-_][a-z0-9]{2,8})?$` else fallback |
| `playbackRate` | number | 1 | clamp [0.5, 3] (client-side rate) |
| `volume` | number | 1 | clamp [0, 1] |
| `triggers.rightClickChip` | bool | true | |
| `triggers.selectionPopup` | bool | false | |
| `triggers.hotkeysEnabled` | bool | true | |
| `triggers.hotkeys.{readSelection,readClipboard,playPause,stop}` | string | `Ctrl+Alt+R`/`V`/`P`/`S` | string → trim → first 64 chars (`""` = disabled is valid); non-string → fallback |
| `triggers.mediaKeys` | bool | true | |
| `triggers.browserExtension` | bool | true | |
| `triggers.explorerMenu` | bool | false | |
| `triggers.excludedApps` | string[] | `[]` | see `normalizeExcludedApps` below |
| `triggers.restoreClipboard` | bool | true | |
| `widget.enabled` | bool | true | |
| `widget.position` | enum | `bottom-right` | one of `bottom-right`, `top-right`, `bottom-center`, `near-cursor`, `remember` |
| `widget.customX`, `customY` | number \| null | null | `null` → null; non-finite/non-number → fallback; else clamp [-100000, 100000] then round (DIP) |
| `widget.autoHide`, `highlightWords`, `compact` | bool | true, true, false | |
| `app.launchAtLogin`, `startMinimized`, `closeToTray` | bool | false, false, true | |
| `app.theme` | enum | `system` | one of `system`, `paper`, `night` |
| `library.saveHistory` | bool | true | |
| `library.storeText` | bool | true | |
| `library.cacheAudio` | bool | true | |
| `library.cacheLimitMB` | int | 500 | round, clamp [50, 10000]. Decimal MB (×1,000,000 bytes) |
| `library.retentionDays` | int | 0 (forever) | round, clamp [0, 3650] |
| `text.skipUrls`, `skipEmails` | bool | true, false | |
| `text.maxChars` | int | 20000 | round, clamp [100, 100000] |
| `server.port` | int | 47823 | round, clamp [1024, 65535] |

Number rule (`:32-36`): non-number or non-finite → fallback; for integer fields round first (JS `Math.round`: halves
round toward +∞), then clamp. Bool rule: only real JSON booleans count (`"true"` → fallback). Enum rule (`oneOf`):
exact, case-sensitive string match. Structure rule: the input and each nested group (`voice`, `triggers`,
`triggers.hotkeys`, `widget`, `app`, `library`, `text`, `server`) are read only if they are JSON objects (not
arrays/null); otherwise they are treated as `{}`, i.e. every field in them takes its fallback. Hotkey strings are not
syntax-checked here (reserved/invalid accelerators are handled by the shortcuts subsystem).

`normalizeExcludedApps(v, fallback)` (`:65-79`): not an array → fallback. For each string item: trim, lowercase,
keep only the last path segment (split on `\` or `/`), trim again; skip empty; append `.exe` unless it already ends
with `.exe`; skip duplicates; stop at 200 entries. Non-string items are skipped. A valid array that normalises to
`[]` is kept as `[]` (not the fallback).

The settings UI offers cache limits 50–5000 MB in steps of 50 and retention presets 0/7/30/90/365 days, but the
store accepts the wider ranges above (hand-edited files).

### 3.3 Patch semantics — `deepMerge` — `settings-store.ts:173-182`

`update(patch)` = `sanitize(deepMerge(current, patch), base = current)`. Merge rule: plain objects merge recursively;
arrays, `null` and primitives replace; keys whose value is `undefined` are skipped; keys `__proto__`, `constructor`,
`prototype` are ignored. A non-object patch replaces the whole value (unless undefined).

`commit` (`:265-272`): if the sanitised result serialises identically to the current settings (compact JSON of the
whole object, `hasApiKey` included) → no-op (no write, no event, returns the unchanged object). Otherwise replace,
`schedule()` the file write, then call every change listener synchronously with `(next, prev)`, iterating over a
copy of the listener set; a throwing listener is caught and warned `settings listener failed`. `update()` before
`init()` warns `settingsStore.update() called before init(); the change may be overwritten` — in fact the change is
always lost: there is no file object yet (nothing is scheduled) and `init()` replaces the in-memory value from disk.
The IPC edge rejects a non-object patch with `Invalid settings patch` (`ipc.ts:307-310`).

### 3.4 Load — `init()` — `settings-store.ts:276-289`

0. Before `init()`, `get()` returns `DEFAULT_SETTINGS` with `hasApiKey = false` and `getApiKey()` returns null.
1. Flush any previous instance (a re-init must not let a pending write land in the new file).
2. `load()` settings.json per §2.5 (corrupt → `.corrupt` + `.bak`; missing → defaults).
3. Load the API key (§4.4).
4. `current = sanitize(raw, DEFAULT_SETTINGS, hasApiKey)`.
5. If a file was loaded and `compact_json(raw) != compact_json(stored(current))`, schedule a rewrite so disk matches
   memory (this is the whole "migration" mechanism: clamped values, dropped unknown keys, a stray `hasApiKey`, and
   different key order all trigger one rewrite). A missing file is not written until the first change.

   The comparison is between **re-serialised parsed values**, not the file bytes: whitespace, indentation, a BOM and
   number spelling (`1.0`, `1e3`, `0.50`) never trigger a rewrite; key order, missing keys, extra keys and changed
   values do. A Rust port should reproduce this as `js_stringify(parsed_file) != js_stringify(canonical)`, where
   `js_stringify` is one JS-compatible compact serialiser used for both sides: object keys in insertion order
   (serde_json feature `preserve_order`) and every number formatted the JS way (integral → no `.0`, `-0` → `0`).
   Do not compare `serde_json::Value`s with `==`: `IndexMap` equality ignores key order, and `Number` equality treats
   the integer `1` and the float `1.0` as different.

   Edge cases: a file with valid JSON of the wrong shape (`null`, `[]`, `"x"`) is `ok` → sanitised to all defaults →
   differs → the defaults are written over it (and over `.bak`) ~250 ms later, with no `.corrupt` copy. When both
   `settings.json` and `.bak` are unreadable, the app runs on defaults and writes nothing until the first change, which
   then overwrites the corrupt main file (its content survives only in `settings.json.corrupt`).

### 3.5 Early synchronous read — `readSettingsFileSync()` — `settings-store.ts:334-350`

Before the UI engine starts, the app needs `triggers.mediaKeys` (Chromium feature switches, `index.ts:515-530`). It
reads `settings.json` synchronously, strips the BOM, parses and sanitises against defaults (`hasApiKey = false`).
Missing main file (`ENOENT`) → `null` immediately (no `.bak`, same "deleting is a reset" rule). Any other failure on
the main file — unreadable (busy/denied; no retry here) **or** unparsable (incl. empty) — → try `settings.json.bak`
the same way; `.bak` missing or bad → `null`. Valid JSON of the wrong shape is not a failure: it yields all defaults.
Never throws. In the native port the equivalent is "read settings before creating windows / registering media-key
handling"; `init()` can simply be synchronous.

---

## 4. API key (`secrets.bin`)

### 4.1 Behaviour — `settings-store.ts:202-263`, `:309-322`

- The key is never in `settings.json` and never sent to the UI; the UI only sees `hasApiKey`.
- `setApiKey(key)`: `key?.trim()`; empty → `null` (clear). Same as current → no-op. Otherwise update memory, persist
  (below), set `hasApiKey`, and **emit a change event even if `hasApiKey` did not change** (key replaced). It does
  not write `settings.json`.
- Persist (serialised on its own queue so writes stay ordered; each step runs after the previous one settles,
  successful or not):
  - `null` → delete `secrets.bin` (no retry; `ENOENT` ignored).
  - encryption unavailable → warn "secure storage is unavailable; the API key is kept in memory for this session
    only", delete `secrets.bin` (ignore errors), keep the key in memory.
  - else `writeFileAtomic(secrets.bin, encrypt(key))` — **no `.bak`** (temp file `secrets.bin.tmp`).
  - any other error → warn `could not persist the API key: <message>` (silent for `ENOENT`); the in-memory key and
    `hasApiKey` are kept regardless. Consequence worth knowing: if deleting `secrets.bin` fails (e.g. a scanner holds
    it), the old key is still on disk and comes back at the next start.
- `flush()` waits for both the settings file and the secrets queue (as queued at the moment of the call).
  `flushSync()` (log-off) covers **only `settings.json`**: a key change whose write is still queued at log-off is
  lost.
- Env-var interplay: when the key came from `ELEVENLABS_API_KEY`, `setApiKey(sameValue)` is a no-op (never
  persisted); a different key is persisted normally; `setApiKey(null)` clears it for the session (deleting a
  possibly absent `secrets.bin`) but the env var supplies it again at the next start.
- Input bound at the IPC edge: keys longer than 256 chars are rejected with "That doesn't look like an API key"
  before validation (`ipc.ts:311-324`). Validation against ElevenLabs happens in the TTS subsystem; only a
  validated key is stored.

### 4.2 What `safeStorage.encryptString` actually wrote (Chromium OSCrypt, Windows)

**[observed]** `secrets.bin` = 82 bytes starting with ASCII `v10`; `Session\Local State` contains
`{"os_crypt":{"audit_enabled":true,"encrypted_key":"<base64>"}, "uninstall_metrics":{...}}`, whose base64 decodes to
283 bytes starting with ASCII `DPAPI`. 82 = 3 (`v10`) + 12 (nonce) + 51 (an `sk_` + 48-char ElevenLabs key) + 16 (tag).

```
secrets.bin   = "v10" (3 bytes) || nonce (12 bytes) || AES-256-GCM ciphertext (len = plaintext) || GCM tag (16 bytes)
                plaintext = UTF-8 API key; no associated data
Local State   = JSON file; os_crypt.encrypted_key = base64( "DPAPI" (5 ASCII bytes) || DPAPI blob )
DPAPI blob    = CryptProtectData(32-byte AES key), current-user scope, no optional entropy
                (Chromium passes CRYPTPROTECT_AUDIT when creating it; flags do not matter for decrypting)
```

The AES key is created once per profile by Chromium and lives in `Local State`, which is in the session-data folder:
`%APPDATA%\Vox\Session\Local State` (because sessionData is redirected, §1.1). A profile created by a build that
did not redirect sessionData would have it at `%APPDATA%\Vox\Local State` — check both, `Session\` first.

### 4.3 Decrypt algorithm for the Rust migrator

```
fn load_legacy_api_key(user_data) -> Option<String>:
    blob = read(user_data\secrets.bin)          // missing / empty -> None
    if blob starts with b"v10":
        ls = read_json(user_data\Session\Local State) or read_json(user_data\Local State)
        wrapped = base64_decode(ls.os_crypt.encrypted_key)
        require wrapped[0..5] == b"DPAPI"
        aes_key = CryptUnprotectData(wrapped[5..], entropy = none, flags = CRYPTPROTECT_UI_FORBIDDEN)  // 32 bytes
        nonce = blob[3..15]; ct_and_tag = blob[15..]          // require len(blob) >= 3+12+16
        plain = AES-256-GCM.decrypt(aes_key, nonce, ct_and_tag, aad = [])
    else if blob starts with b"v20" or b"v11":
        return None                                           // not produced by Electron on Windows; unsupported
    else:
        plain = CryptUnprotectData(blob, entropy = none)      // legacy OSCrypt path: raw DPAPI blob
    key = utf8(plain).trim()
    return key if non-empty
```

DPAPI is bound to the Windows user account (and its credentials); copying the profile to another user or machine
makes decryption fail. Any failure → warn ("could not decrypt the stored API key") and behave as "no key".

### 4.4 Load order — `loadApiKey()` — `settings-store.ts:222-243`

1. Read `secrets.bin` (single attempt, no busy retry; non-`ENOENT` errors warn `could not read secrets.bin`).
2. Non-empty and encryption available → decrypt (UTF-8), trim, use if non-empty. Encryption unavailable → warn
   "secure storage is unavailable; the stored API key cannot be decrypted". Decrypt failure → warn
   `could not decrypt the stored API key: <message>`. In every failure case `secrets.bin` is left untouched (it is
   neither deleted nor rewritten), so a key that cannot be decrypted today stays on disk.
3. Else `ELEVENLABS_API_KEY` env var (trimmed, non-empty). It makes `hasApiKey` true but is never written to disk.
4. Else no key.

On Windows, Electron's `isEncryptionAvailable()` is true once the app is ready (DPAPI is always present); the
"unavailable" branches are effectively dead on Windows but keep them as error paths.

### 4.5 Writing from Rust (recommendation, see open questions)

Chromium's OSCrypt decrypt on Windows treats any blob **not** starting with `v10` as a raw DPAPI blob. So if the Rust
app writes `secrets.bin` as a raw `CryptProtectData` blob (current user, no entropy), both the Rust app and the old
Electron build can read it, and the Rust app no longer depends on `Local State`. (A DPAPI blob starts with
`01 00 00 00 D0 8C 9D DF ...`, never `v10`.) Re-encrypt this way after the first successful legacy decrypt.
Caveat: the raw-DPAPI fallback is long-standing in Chromium's synchronous OSCrypt; whether the Electron 44 build's
`safeStorage` still routes through it was not verified. Test a round trip with the old build before relying on
"downgrade keeps the key"; the Rust side (reading `v10` and raw DPAPI) does not depend on it.

---

## 5. History (`history.json`)

### 5.1 File shape — `history-store.ts:7-10`, `:215-221`

```json
{"version":1,"records":[ <ReadRecord>, ... ]}
```
Compact JSON (no whitespace), records **newest first** (`createdAt` descending). On load the reader also accepts a
bare array `[ <ReadRecord>, ... ]`; anything else → no records (`parseFile`, `:197-208`). `version` is not checked.

Loss cases the port should know about (current behaviour, not necessarily desirable):
- Valid JSON of another shape (`{}`, `{"records": 5}`, `null`) reads as `ok` → zero records, **no** `.bak` fallback,
  **no** `.corrupt` copy; the first mutation (e.g. the next read) overwrites `history.json` and `.bak`.
- Main and `.bak` both corrupt → zero records; the old content survives only in `history.json.corrupt` — and that
  copy is deleted the first time any record is removed (by the user or by retention, §5.4).

### 5.2 `ReadRecord` — `types.ts:296-330`

Key order as written (absent optional keys are omitted entirely, never `null`):

| Field | Type | Meaning |
|---|---|---|
| `id` | string | UUID v4 (lowercase, from `randomUUID`), also the audio-cache folder name. ≤128 chars. |
| `createdAt` | number | epoch ms when the read started (integer in practice; any finite number accepted) |
| `text` | string | full cleaned text, or the first 160 UTF-16 units when `storeText` was off (§5.6) |
| `textTruncated` | bool | true when `text` is only a preview |
| `charCount` | int | length of the full read text in **UTF-16 code units** |
| `wordCount` | int | words in the full text (reader's regex, §5.7) |
| `voiceId`, `voiceName`, `modelId` | string | may be `""` |
| `source` | enum | `hotkey`, `context-chip`, `selection-popup`, `browser-extension`, `explorer`, `clipboard`, `manual`, `replay`, `cli` |
| `sourceApp`? | string | friendly app name, e.g. `Chrome` |
| `sourceProcess`? | string | lower-case exe, e.g. `chrome.exe` |
| `sourceTitle`? | string | window/page title, trimmed, ≤200 chars (reader) |
| `sourceUrl`? | string | http(s) URL, browser-extension reads only |
| `audioDurationSec` | number | total synthesized audio seconds (0 until synthesis completes), 1 decimal |
| `listenedSec` | number | wall-clock seconds listened, all sessions incl. cached replays, 1 decimal |
| `maxPositionSec`? | number | furthest audio position reached (seconds of audio), max over sessions; omitted when 0 / absent (older records) |
| `listenLog`? | object | hour ledger `{"YYYY-MM-DDTHH": seconds}` local time (§5.5); omitted when empty |
| `completed` | bool | played to the end at least once (and synthesis was not cut short) |
| `cached` | bool | audio is in the cache, free replay possible |
| `charactersBilled` | int | characters ElevenLabs charged (0 for cached replays) |
| `favorite` | bool | favourites survive retention |
| `error`? | string | user-facing error for a failed / partial read |

### 5.3 Normalisation — `sanitizeRecord(v, base?)` — `history-store.ts:48-98`

Applied to every record on load, on `add`, and to the merged result of every `update` (with `base` = existing
record). Without `base`, a record lacking a usable `id` or `createdAt` is rejected.

- not an object → reject.
- `id`: string, trimmed non-empty → trimmed, first 128 chars; else `base.id`. `createdAt`: finite number (kept as is,
  not rounded); else `base.createdAt`. Either missing → reject.
- `text`: any string as is (not trimmed, no length cap); else `base.text ?? ""`.
- `source`: valid enum; else `base.source ?? "manual"`.
- `textTruncated`, `completed`, `cached`, `favorite`: bool; else base or `false`.
- `charCount`: `round(max(0, v))` if finite; else `base.charCount ?? utf16_len(text)`.
- `wordCount`: `round(max(0, v))` if finite; else `base.wordCount ?? countWords(text)` where this fallback
  `countWords` = number of `\s+`-separated tokens of the trimmed text (0 if empty) (`:43-46`) — differs from the
  reader's counter (§5.7) and only matters for malformed files.
- `voiceId`, `voiceName`, `modelId`: any string as is (not trimmed, no cap); else base or `""`.
- `sourceApp`, `sourceTitle`, `sourceUrl`, `error` (optString, `:36-41`): `undefined`/non-string → base value;
  string → trimmed; `""` after trim → **absent** (this is how a field is cleared). `sourceProcess`: same, then
  lowercased (JS `toLowerCase`, Unicode-aware; also applied to the base value).
- `audioDurationSec`, `listenedSec`: `max(0, v)` if finite (not rounded); else base or 0.
- `maxPositionSec`: `max(0, v)` if finite else base or 0; then **0 → absent**. So an explicit `0` or a negative
  number *clears* it (even when the base had a value); a non-number (`undefined`, `null`, a string) keeps the base.
- `listenLog`: key absent / value `undefined` → base value; any other value → `sanitizeListenLog` (§5.5), and an
  empty or invalid result (`{}`, `null`, an array, a log with no valid entries) → **absent** (clears it).
- `charactersBilled`: `round(max(0, v))` if finite; else base or 0.
- Unknown keys are dropped. Optional keys whose value is absent are deleted from the object.

On load: sanitise each item, count rejects (warn `ignored N malformed history record(s)`), stable-sort by
`createdAt` descending, and drop later duplicates of an id (first after sorting wins). Normalised records are not
written back until the next mutation.

### 5.4 In-memory model and operations — `history-store.ts:117-361`

State: `records` (vector, newest first) + `byId` map.

- **Insertion** (`:128-145`): binary search; a new record goes after all records with a strictly greater
  `createdAt` and **before** existing records with an equal one. `insert` first removes any record with the same id.
- **mutated()** = schedule the file write + debounced change notification.
- **init()** (`:213-236`): flush old instance; load; records added before the first `init()` (early reads) are kept
  and re-inserted with `insert` (so an early record **replaces** a loaded record with the same id), then
  `mutated()`; a re-init reloads from disk only.
- **add(rec)** (`:239-247`): sanitise without base (reject → warn
  `historyStore.add() ignored a record without id/createdAt`); insert/replace by id; mutated — always, even if an
  identical record already existed. The stored copy is independent of the caller's object.
- **update(id, patch)** (`:249-265`): unknown id → `null`. `merged = {...existing, ...patch, id: existing.id}`; for
  each of `sourceApp, sourceProcess, sourceTitle, sourceUrl, error` that is a key of `patch` with value `undefined`,
  set it to `""` so sanitisation clears it. `next = sanitize(merged, existing)`. If `next` equals `existing`
  (field-by-field over the union of keys, `listenLog` compared as maps) → return existing, no write. Same
  `createdAt` → replace in place; else re-insert. mutated; return `next`. (`listenLog` / `maxPositionSec` are *not*
  cleared by patching `undefined` — that keeps the existing value — but are cleared by patching an empty/invalid
  ledger such as `{}` / `null`, or `maxPositionSec: 0`, see §5.3. The reader never does either.)
- **get(id)** → record or `null`. At the IPC edge, `get`/`remove`/`toggleFavorite` ids must be a non-empty string of
  ≤ 128 chars, else the call fails with `Invalid id` (`ipc.ts:195-198`).
- **all()**: copy of the newest-first list.
- **remove(id)**, **clear()** (`:306-313`) → `removeWhere`: filter out, then if anything was removed: mutated,
  fire `onRemoved(ids)` **synchronously** (ids in newest-first order; each listener in try/catch, warn
  `history remove listener failed`), and delete `history.json.corrupt` in the background (an old corrupt copy would
  still hold removed reads; the next save rewrites both `history.json` and `.bak`). Unknown id / empty history → no
  write, no events.
- **toggleFavorite(id)** = `update(id, {favorite: !existing.favorite})`.
- **onChange(cb)**: debounced **100 ms** with max wait **400 ms** (`:147-162`; same timer formula as §2.5
  `schedule()`); listeners take no arguments, each in try/catch (warn `history listener failed`). The IPC layer
  additionally throttles the UI event `history:changed` (no payload): the first notification arms a 150 ms timer,
  further ones are ignored until it fires (`ipc.ts:391-398`); the UI re-queries.
- **onRemoved(cb)**: ids of removed records; `index.ts:429-431` uses it to delete their cached audio.
- **flush()** / **flushSync()** per §2.5.

### 5.5 Listening ledger `listenLog` — `shared/listen-log.ts`

Purpose: analytics credit listening to *when it happened* (a cached replay of an old read today counts today), not
to `createdAt`.

- Key: local hour `YYYY-MM-DDTHH` (`hourKey`, `:12-14`). Valid key regex `^(\d{4})-(\d{2})-(\d{2})T(\d{2})$` plus
  month 1–12, day 1–31, hour ≤ 23 (`:7`, `:17-24`). The key's instant = local `Date(y, m-1, d, h)` (JS normalises
  overflow such as Feb 31 → Mar 3, and a non-existent DST hour moves forward; reproduce or clamp).
- `sanitizeListenLog(v)` (`:32-43`): must be a non-null, non-array object; keep entries with a valid key and a
  number value that is finite and `> 0`, then round it to 0.1 s (the `> 0` test is made **before** rounding, so a
  value in (0, 0.05) is kept and stored as `0`; `listeningEntries` later ignores it); sort by key ascending (string
  order = chronological); keep the **newest 400** keys (`LISTEN_LOG_MAX_KEYS`; the older hours are dropped, and
  their seconds then fall into the "rest at createdAt" bucket of `listeningEntries`); empty → absent. Written in
  ascending key order.
- `addToListenLog(log, key, sec)` (`:46-50`): copy, `log[key] = round1((log[key] ?? 0) + sec)` when `sec` is finite
  and `> 0`, then sanitise (`{}` if nothing is left). The key is not validated before insertion; sanitise drops an
  invalid one.
- `sameListenLog(a, b)` (`:52-58`): same number of keys and `b[k] === a[k]` for every key of `a` (order ignored;
  absent ≡ absent).
- `listeningEntries(r)` (`:64-89`) — the canonical "when was this listened" expansion used by analytics:
  ```
  total = listenedSec if finite and > 0 else 0
  entries = [(local start of hour key, sec) for each valid key with finite sec > 0]
  logged = sum(sec)
  if logged > total and logged > 0: scale every entry by total/logged; logged = total   // file says more than total
  rest = total - logged
  if rest > 0.05: entries.push((createdAt, rest))     // older records, trimmed hours
  ```
  Entries come out in the ledger's stored order (ascending keys), with the "rest" entry last; the scaled values are
  not rounded. Keep that order: analytics sums these floats in this order before rounding to 0.1.

### 5.6 Preview-only mode (`library.storeText = false`) — `reader.ts:39-40`, `:338-344`, `:403-452`, `:638-662`

- New records store `text = fullText[0..160]` (first **160 UTF-16 units**, a plain slice that *can split a
  surrogate pair* — see §11) and `textTruncated = (fullText.length > 160)`. `charCount` and `wordCount` still
  describe the full text. Turning the setting off does **not** retroactively shorten existing records.
- Cached audio of a truncated record is saved **without text**: every alignment `chars` entry replaced by `""`
  (same array length), starts/ends kept (`withoutText`, `reader.ts:175-177`; `redactAlignment`,
  `audio-cache.ts:98-101`). Startup reconciliation redacts manifests written by older versions (§6.8).
- Replay of a truncated record: cached → the session text is rebuilt as the preview plus one `…` per chunk lying past
  it (`previewReplay`, `reader.ts:187-226`, reader subsystem); not cached → fails with code `not-cached`,
  "The audio is no longer cached and the full text was not saved".
- `library.saveHistory = false`: no record is created, listening is not tracked, and **nothing is cached** (caching
  needs a saved record). The UI disables "Store full text" and retention when history is off.

### 5.7 Who writes which field, and when (reader subsystem, summarised) — `reader.ts`

| Moment | Fields |
|---|---|
| Read starts (`:335-362`), only if `saveHistory` | new record: `id`=UUID v4, `createdAt`=now, text/preview, `textTruncated`, `charCount` (UTF-16 length), `wordCount` = count of matches of `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu` (`:137-139`), voice/model from settings or replay override, `source`, `sourceApp/Process/Title(trim, ≤200)/Url`, zeros/false elsewhere |
| Engine start throws | `error` = message |
| Synthesis completes (`:638-662`) | `audioDurationSec` = round1(sum of chunk durations), `cached` (true only if saved to cache), `charactersBilled` = round(billed) |
| Synthesis stopped after some chunks (`:284-292`) | `charactersBilled`; plus `error = "Only part of the text could be synthesized"` if a chunk failed |
| Synthesis failed (`:664-677`) | `error` = user-facing message |
| Player state reports (`:560-608`) | every ≥2 s (`HISTORY_WRITE_INTERVAL_MS = 2000`) or on status change: `listenedSec = round1(base + session)`, `completed`, `maxPositionSec` (only if larger), `listenLog[hourKey(now)] += delta` |
| Cache evicted / missing / cleared | `cached = false` |
| User | `favorite` toggled; remove; clear |

A cached replay does **not** create a record: it accumulates into the original (`baseListenedSec`), bills nothing.
A replay whose audio is no longer cached re-synthesises as a **new** record with `source = "replay"` (billed).

### 5.8 Search / list — `historyStore.list(q)` — `history-store.ts:271-299`

Query (`HistoryQuery`, after IPC validation `ipc.ts:174-183`): `search` (kept only if it is a string that is
non-blank after trim; then the *untrimmed* string is cut to its first 500 UTF-16 units), `limit` (finite number →
`round(clamp(v, 1, 1000))`), `offset` (finite number → `round(max(0, v))`), `favoritesOnly` (only a real boolean),
`source` (only one of the 9 enum values). A non-object query → `{}`; invalid fields are dropped, not errors.

```
limit  = clamp(floor(q.limit),  1, 500), default 50     (IPC first rounds and clamps to 1..1000)
offset = clamp(floor(q.offset), 0, 2^53-1), default 0
terms  = q.search.trim().split(/\s+/) non-empty, first 16 terms, each a LITERAL case-insensitive substring
fields(r) = [text, sourceApp, sourceTitle, voiceName, sourceUrl]          (absent fields skipped)
match(r) = (!favoritesOnly || r.favorite) && (!source || r.source == source)
           && every term matches at least one field
result  = { items: matches[offset .. offset+limit], total: matches.count }   newest first
```
Not searched: `sourceProcess`, `modelId`, `voiceId`, `error`. `\s` is the JS class (ASCII white space plus all
Unicode `Zs` spaces, U+2028/2029 and U+FEFF). Case-insensitivity is JS regex `/i` without the `u` flag: each UTF-16
unit is canonicalised with `toUpperCase` when that yields a single unit, but a non-ASCII unit never maps to an ASCII
one (so `ſ`≠`s`, Kelvin `K`≠`k`, `ß`≠`SS`). A Unicode simple-case-folding substring match (e.g. the `regex` crate's
case-insensitive mode) is an acceptable equivalent; the differences are limited to those exotic letters.

The settings UI uses `list` with `limit: 1` and binary search over `offset` to count how many reads a stricter
retention would delete (`countExpiringReads`, renderer `settings/logic.ts:215-243`): non-favourites with
`createdAt < now - days*86400000`. Keep `list` cheap for that pattern (or expose a count API).

### 5.9 Export — `exportTo(path)` — `history-store.ts:321-325`, `ipc.ts:377-389`

Pretty JSON (2-space), atomic write (§2.2, no `.bak`; creates `<chosen>.tmp` and any missing parent folders):
```json
{ "app": "Vox", "version": 1, "exportedAt": "2026-10-17T09:35:46.902Z", "records": [ ...same records, newest first... ] }
```
`exportedAt` = UTC ISO-8601 with milliseconds. `records` are the in-memory records exactly as stored (preview-only
records export only their preview). Save dialog: title "Export history", default
`Documents\Vox-history-<YYYY-MM-DD>.json` where the date is the **UTC** date (`toISOString().slice(0,10)`), filter
`JSON (*.json)`, parented to the main window when it exists. Cancel or empty path → returns `null`, nothing written.
Returns the saved path. A write error propagates to the UI as a rejected call.

### 5.10 Retention — `history-store.ts:327-335`, `index.ts:30-35`, `:91-105`, `:436-441`, `:315`

- `applyRetention(days, now)`: `days` not finite or ≤ 0 → 0. `cutoff = now - days * 86_400_000` (fixed 24 h days,
  not calendar days). Remove every record with `createdAt < cutoff` **and not favourite** (one `removeWhere`, so one
  write, one `onRemoved` with all ids). Returns the count; the caller (`index.ts:91-96`) reads
  `settings.library.retentionDays` at run time, returns early unless `days > 0`, and logs
  `retention removed N records` (scope `app`) when N > 0. Removal triggers `onRemoved` → cached audio deleted.
- Runs: once at startup (after stores load and demo seeding, before cache reconciliation); every **6 h**
  (`RETENTION_INTERVAL_MS`, a fixed interval from startup), skipped while the change timer below is pending ("a
  pending change applies itself; don't let the periodic run cut its grace period short"); and **60 s after the last
  change** of `retentionDays` (`RETENTION_CHANGE_DELAY_MS`, debounced: each change restarts the 60 s) — not
  immediately, because arrowing through a native select commits every option it passes (Keep forever → 7 days → ...
  → 1 year would delete reads on the way). Both timers are cancelled at shutdown.
- UI guard (renderer): a stricter value is only saved after 450 ms of no changes and, if it would delete anything,
  after the user confirms the count; a looser value saves immediately.

---

## 6. Audio cache (`audio-cache\`)

### 6.1 Keying and layout — `audio-cache.ts:1`, `:45-68`

- One entry per **read id** (not per text/voice; re-reading the same text synthesises and bills again). Entry dir
  `audio-cache\<readId>\`. `readId` must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$` (ids come from the UI on replay;
  this stops path escapes). Invalid ids are ignored by every operation.
- Chunk file name `<index>.<ext>`; ext from MIME (case-insensitive substring): `mpeg` or `mp3` → `mp3`; `wav` or
  `wave` → `wav`; `ogg` or `opus` → `ogg`; else `bin`. Valid chunk file name: `^\d{1,6}\.(mp3|wav|ogg|bin)$`.
- Folder name is deliberately not `cache` (§1.1).

### 6.2 `manifest.json` (compact JSON) — `audio-cache.ts:18-35`

```jsonc
{
  "version": 1,
  "readId": "02d84816-59fd-4cf1-8f73-86cd58eff9d3",   // must equal the folder name
  "createdAt": 1791700000000,                           // ms, when saved
  "lastAccess": 1791700000000,                          // ms, last save or load (LRU key)
  "chunks": [
    {
      "index": 0,                // chunk number in the original session (0..n-1)
      "textStart": 0,            // UTF-16 offset into the read's full text (inclusive)
      "textEnd": 412,            // exclusive
      "mime": "audio/mpeg",      // or "audio/wav" (mock TTS)
      "alignment": { "chars": ["H","e",...], "starts": [0, 0.192, ...], "ends": [...] },  // or null
      "durationSec": 27.4,       // best known chunk duration
      "file": "0.mp3",
      "bytes": 231593            // audio file size
    }
  ]
}
```
`alignment` = ElevenLabs character timing for that chunk; times in seconds relative to the chunk's audio start;
`chars.length` may differ from `textEnd - textStart` (ElevenLabs counts its own way; the player maps proportionally).
For preview-only records every `chars` entry is `""`.

### 6.3 Manifest validation — `parseManifest` — `audio-cache.ts:107-147`

Invalid (→ entry treated as corrupt) if: not an object; `readId` ≠ folder name (exact, case-sensitive string
compare against the folder name as listed on disk); `chunks` not a non-empty array; any chunk not an object, or its
`file` not a string matching the chunk regex, or `mime` not a string. Numeric fields (`index`, `textStart`,
`textEnd`, `durationSec`, `bytes`): finite number → as is (not required to be integers), else 0. `alignment`: an
object whose `chars`, `starts`, `ends` are all arrays of equal length, else `null` (element types are not checked).
`createdAt`: finite else now; `lastAccess`: finite else `createdAt`. `version` is not checked. Not checked either:
that `file` matches `index`, that file names are unique, or that `bytes` equals the size on disk.

The parsed manifest is a normalised copy (`version: 1`, unknown keys dropped, bad numbers → 0, bad alignment →
`null`); `load()`'s touch and `redactText()` rewrite the manifest **from that copy**, so those rewrites also
normalise it.

### 6.4 Index (in memory) — `audio-cache.ts:37-43`, `:180-188`

`readId → { bytes, files, lastAccess, hasText }`, where `bytes` = manifest bytes + audio bytes, `files` = chunk count,
`hasText` = some alignment has a non-empty char. `stats()` = `{ reads: entries, files: Σfiles, bytes: Σbytes }`.

Per-readId operation lock (`withLock`, `:70-80`): operations on one id run strictly in order; a failed one does not
block the next.

### 6.5 `init()` — scan and repair — `audio-cache.ts:196-224`

1. Clear index; `mkdir -p audio-cache` (failure → warn `could not create the audio cache directory`, cache stays
   empty for the session).
2. List **directories** in the root (plain files and symlinks/junctions at the root are left alone; listing failure →
   warn `could not list the audio cache`, cache stays empty). Process with concurrency 8 (8 workers pulling names
   from a shared cursor).
3. Invalid id → delete the folder recursively. Valid id (under its lock) → `scanEntry`: manifest must parse and
   validate; every chunk file must exist, be a regular file and have size > 0; bytes = manifest file size + chunk
   sizes on disk. OK → index it; otherwise delete the whole folder (incomplete save, corruption). Files in the
   folder that the manifest does not list (stale chunks, `manifest.json.tmp`) are neither counted nor deleted.
   The scan does not consult history; orphans are removed later by reconciliation (§6.10).

Folder deletion (`removeDir`, `:82-90`) = recursive delete of a folder *or file*, where a missing path counts as
success (`force`), retried up to 4 times on `EBUSY`, `EMFILE`, `ENFILE`, `ENOTEMPTY` or `EPERM` with a linear
backoff of 60 ms × attempt (60, 120, 180, 240 ms; Node `rm` `maxRetries: 4, retryDelay: 60`); final failure → warn
`could not delete <dir>: <message>` and return false. It never throws.

### 6.6 `save(readId, chunks)` — `audio-cache.ts:226-278`

1. Invalid id → warn `audioCache.save() ignored an invalid readId: <id>`, return (success). Keep chunks with
   integer `index ≥ 0` and non-empty audio; de-duplicate by index (last wins); sort by index. None left → return
   without touching disk (and without removing an existing entry).
2. Under the id lock: drop the index entry; delete the folder (a failed delete only warns — the save carries on and
   overwrites; leftover files of the old entry stay unindexed); `mkdir -p`; write each chunk file **plainly** (not
   atomic, not fsynced, no retry) as `<index>.<ext>`; then write `manifest.json` atomically (§2.2, no `.bak`)
   **last**, so an interrupted save has no manifest and is cleaned up by the next `init()`.
   `createdAt = lastAccess = now`. Chunk fields are stored as given (`mime` not normalised, `alignment` may be
   `null`); a non-finite number would serialise as `null` and read back as 0.
3. Index entry: bytes = Σ audio bytes + UTF-8 length of the manifest JSON; `files` = chunk count; `hasText` from the
   alignments as saved.
4. Any I/O error → delete the folder, re-throw (caller logs "audio cache save failed" and leaves `cached = false`).

Caller policy (`reader.ts:638-662`): only when synthesis **completed** (partial audio is never cached — every replay
would stop early), the record still exists, `saveHistory` was on for that read, and `library.cacheAudio` is on.
Truncated (preview-only) records are saved with redacted alignment. After a successful save:
`pruneCache(keep = [readId])`. Then the record gets `audioDurationSec = round1(Σ finite chunk durations)`,
`cached` (true once the save succeeded, even if the prune afterwards failed) and
`charactersBilled = max(0, round(billed))`.

Turning `library.cacheAudio` off only stops new saves: existing entries are kept, still replay for free and are
still counted and pruned against the limit. Nothing deletes the cache except eviction, record removal, "Clear
cache" and the startup scan.

### 6.7 `load(readId)` — `audio-cache.ts:280-321`

Invalid id → none. Under the lock: read+validate the manifest; not OK (missing/corrupt) → delete entry (index +
folder), return none. Read every chunk file in manifest order (no retry; empty → error `empty chunk <file>`). Sort by
index. **Touch**: rewrite the manifest with `lastAccess = now` atomically (failure only warns "could not touch cache
manifest"). Refresh the index entry (bytes = Σ audio bytes read + new manifest length, `lastAccess = now` even if the
touch failed — so LRU order in memory and on disk can then differ until restart). Any other error → warn
`dropping unreadable cache entry <id>` (silent for not-found), delete the entry, return none. Returned chunk indices
are the stored ones; the playback engine renumbers them 0..n-1. `load()` also (re)indexes an entry that was not in
the index.

### 6.8 Other operations

- `has(id)`, `hasText(id)`: index lookups.
- `redactText(id)` (`:332-351`): only if indexed with `hasText`; under the lock re-read the manifest (not OK → do
  nothing; the entry is *not* deleted here), rewrite it atomically with every alignment char set to `""`, update
  index (`bytes` = Σ manifest `bytes` + new manifest length, `hasText = false`). Errors → warn
  `could not redact cache manifest <id>`; never rejects.
- `remove(id)`: under the lock drop index entry + delete folder. Never rejects.
- `clear()` (`:359-375`): every name in the root (files and folders; listing error other than not-found → warn
  `could not list the audio cache`) ∪ every indexed id, all in parallel: valid id → locked remove; anything else →
  recursive delete. The root folder itself stays. Returns stats afterwards — index entries are dropped before their
  folders are deleted, so this reports 0 even if some deletions failed. The IPC caller then sets `cached = false` on
  every record that had it (`ipc.ts:296-298`, `:406-410`).
- `stats()` → `{reads, files, bytes}` from the index (no disk access).
- `removeOrphans(keep)` (`:408-414`): delete indexed entries whose id is not in `keep` (in parallel); returns their
  ids. Unindexed folders are not touched.

### 6.9 Size limit and eviction — `prune(maxBytes, protect)` — `audio-cache.ts:381-406`, `reader.ts:168-169`, `:521-531`

- Limit = `library.cacheLimitMB × 1,000,000` bytes (decimal MB, the unit Settings shows). Default 500 MB, range
  50–10,000.
- Algorithm (all prunes run one after another):
  ```
  limit = max(0, maxBytes) (non-finite -> unlimited)
  total = Σ index bytes; if total <= limit: return []
  candidates = index entries not in protect, sorted by lastAccess ascending (stable)
  for each candidate while total > limit: locked remove; total -= entry.bytes; evicted.push(id)
  ```
  If protected entries alone exceed the limit, everything else is evicted and the cache stays over the limit.
- `protect` = caller's `keep` + the read currently being synthesised + the read currently loaded in the player.
- Each evicted id gets `cached = false` on its history record.
- Triggers: after every successful cache save; whenever `cacheLimitMB` is **lowered** (`index.ts:316-323`, logs
  `cache limit lowered: evicted N cached reads`). Raising the limit does nothing.
- LRU recency = last save or last replay (`lastAccess` in the manifest), so it survives restarts. Prune itself uses
  the in-memory index values (never re-reads manifests) and subtracts each victim's indexed `bytes`, whether or not
  its folder deletion succeeded. Equal `lastAccess` values keep index insertion order.
- The `cached` flag on records is maintained by the callers, never by the cache itself: prune victims and
  reconciliation (§6.10) set it false; a successful save sets it true.

### 6.10 Startup reconciliation — `index.ts:107-121`, `:426-437`

Order: settings init → (history init ∥ cache init) → register `onRemoved → cache.remove` (each failure warns
`cache remove failed`) → demo seeding → retention → reconcile (one synchronous pass over the records, newest first):
1. Record `cached` but no cache entry → `update(id, {cached: false})`.
2. Record `textTruncated` and its manifest still has text → `redactText` (fire and forget; entries from older
   versions).
3. `removeOrphans(all record ids)` (logs `removed N orphaned audio cache entries` when N > 0; failure warns
   `cache orphan cleanup failed`).

Note the asymmetry: a cache entry whose record says `cached: false` is kept (and still used for replay, which checks
the cache, not the flag).

---

## 7. Analytics (`analytics.ts`, pure)

`computeAnalytics(records, range, now = current time) -> AnalyticsSummary` (`:99-234`). Called on demand with
`historyStore.all()` (`ipc.ts:401-404`; the IPC maps any range outside the 5 values to `30d`); nothing is stored or
cached. **All calendar maths is local time**; day arithmetic is calendar-based (`new Date(y, m, d + n)`), never
`+86_400_000`, so DST never skips or repeats a day (`:20-22`). "Local midnight" is JS `new Date(y, m, d)`: if
midnight does not exist that day (zones whose DST starts at 00:00) it is the first existing instant after it.

Input order matters only for float summation order and the voice-name tie-break; the caller always passes records
newest first, and the port should iterate in that order to reproduce the test vectors bit-for-bit.

### 7.1 Constants

| Name | Value |
|---|---|
| `RANGE_DAYS` | `7d`: 7, `30d`: 30, `90d`: 90, `365d`: 365; plus `all` |
| invalid range | → `30d` (also the IPC default) |
| `MAX_DAILY_DAYS` | 365 (daily series cap) |
| `SILENT_READING_WPM` | 238 |
| `TOP_N` | 6 |
| `ESTIMATED_CHARS_PER_SECOND` | 15 (`shared/constants.ts:17`) |
| `DAILY_GOAL_SEC` | 900 (Home "Today" goal, `shared/constants.ts:43-44`) |

Helpers: `dayKey(d)` = local `YYYY-MM-DD`; `nonNeg(v)` = v if finite and > 0 else 0; `round1(v)` = `round(v*10)/10`
(JS `Math.round`).

### 7.2 Window

```
today      = local midnight of now;  todayMs; tomorrowMs = local midnight of the next calendar day
valid      = records with finite createdAt < tomorrowMs        // future-dated (clock changes) ignored everywhere
startDay   = range == all ? local midnight of min(todayMs, min createdAt over valid)
                          : today - (RANGE_DAYS - 1) calendar days      // 7d = today + 6 previous days
startMs    = startDay
inRange    = valid with createdAt >= startMs
daily      = one entry per local day from max(startDay, today - 364 days) to today inclusive (≤365 entries),
             {date: "YYYY-MM-DD", listenedSec: 0, reads: 0, words: 0}, oldest -> newest
```

### 7.3 Accumulation

Pass 1 — **per read, on its creation day** (records in `inRange`):
```
words = nonNeg(wordCount)
totals.reads += 1; totals.words += words; totals.chars += nonNeg(charCount)
totals.audioSec += nonNeg(audioDurationSec); totals.charactersBilled += nonNeg(charactersBilled)
if completed: totals.completedReads += 1
heardWords       += words * heardFraction(r)
heardListenedSec += nonNeg(listenedSec)               // record total, NOT ledger-windowed
daily[dayKey(createdAt)].reads += 1; .words += words  (if that day is in the series)
apps[appName(r)].reads += 1
voices[voiceId || voiceName || "unknown"].reads += 1; group name = voiceName of the newest record (createdAt >=)
                                                       with a non-empty voiceName
sources[r.source] += 1
```
Pass 2 — **listening, when it happened** (every record in `valid`, not just `inRange`):
```
if createdAt >= todayMs: today.reads += 1; today.words += nonNeg(wordCount)
for (at, sec) in listeningEntries(r):                 // §5.5
    if at < startMs or at >= tomorrowMs: skip
    totals.listenedSec += sec
    if at >= todayMs: today.listenedSec += sec
    daily[dayKey(at)].listenedSec += sec               (if in series)
    heatmap[(weekday(at) + 6) % 7][hour(at)] += sec    // row 0 = Monday ... 6 = Sunday; column = local hour 0..23
    apps[appName(r)].listenedSec += sec                // creates the group with reads 0 if needed
    voices[key].listenedSec += sec                     // new group: name = r.voiceName (nameAt = createdAt);
                                                       // an existing group's name is NOT updated in this pass
```
`key` is the same `voiceId || voiceName || "unknown"` as in pass 1; `appName`/`key` are recomputed per entry.
So a cached replay of an old read counts as listening today, while its read/word/credit counts stay on the
original creation day.

`heardFraction(r)` (`:32-44`) — share of the read actually heard:
```
listened = nonNeg(listenedSec); if listened == 0: return 0
if completed: return 1
audio = nonNeg(audioDurationSec) || nonNeg(charCount) / 15
if !(audio > 0): return 1
reached = nonNeg(maxPositionSec ?? 0)
return min(1, (reached > 0 ? reached : listened) / audio)     // listened-time fallback for older records
```

`appName(r)` (`:46-65`): trimmed `sourceApp` if non-empty; else by source: `manual`/`replay` → `Vox`,
`clipboard` → `Clipboard`, `explorer` → `Explorer`, `cli` → `Command line`, `browser-extension` → `Browser`,
anything else → `Other`.

### 7.4 Output (`types.ts:347-373`)

```
range, generatedAt = now (ms)
totals = { listenedSec: round1, reads, words, chars, audioSec: round1, completedReads, charactersBilled }
today  = { listenedSec: round1, reads, words }
streak = streaks(valid)                          // not range-limited
daily  = series with listenedSec round1
heatmap = 7×24, each cell round1
topApps   = groups -> {name, reads, listenedSec: round1}, drop groups with reads == 0 and listenedSec == 0
            (tested AFTER round1, so a reads-0 group whose listening rounds to 0.0 disappears),
            sort byListened, first 6
topVoices = groups -> {voiceId: key, name: name || key, reads, listenedSec: round1}, NO zero filter (a group with
            reads 0 and listenedSec rounding to 0 is kept), sort byListened, first 6
sources   = [{source, reads}] sorted reads desc, then source name ascending
avgWordsPerRead          = reads > 0 ? round(words / reads) : 0
avgListenWpm             = heardListenedSec > 0 ? round(heardWords / (heardListenedSec / 60)) : 0
readingTimeEquivalentSec = round(heardWords / 238 * 60)     // silent-reading time for the words heard
```
`byListened` = listenedSec desc (after round1), then reads desc, then name ascending with `localeCompare` (ICU
collation of the default locale: case-insensitive at the first level, so `apple` < `Banana`, unlike byte order; a
case-insensitive compare with a byte-order tie-break is a fine approximation). `sources` names are ASCII
lowercase/hyphen, where `localeCompare` equals byte order.

Rounding happens only at the very end (sums are accumulated unrounded); `totals.chars`, `words` and
`charactersBilled` are sums of stored integers and are not rounded. `round1` and `Math.round` are JS (halves toward
+∞).

### 7.5 Streaks — `streaks()` — `analytics.ts:76-97`

```
days = { dayKey(createdAt) for r in valid }                        // any read counts, even failed / unheard
     ∪ { dayKey(at) for (at, sec) in listeningEntries(r) if sec >= 1 and at < tomorrowMs }
current: cursor = today if today ∈ days else yesterday; count consecutive days back while ∈ days
longest: walk days sorted ascending ("YYYY-MM-DD" sorts chronologically); run += 1 when key == next calendar day
         of previous key else run = 1; longest = max over runs
return { current, longest: max(longest, current) }
```
A streak is not broken until today ends (yesterday-anchored when today is still empty). The `sec >= 1` test is
applied to the (possibly scaled) ledger entries and the "rest" entry of `listeningEntries`; there is no lower date
bound. "Next calendar day" is computed as local `Date(y, m-1, d+1)` → `dayKey`.

### 7.6 Display-side derivations (renderer, for parity) — `charts/model.ts`, `pages/Analytics.tsx:242-361`

| UI figure | Computation |
|---|---|
| Listening time (hero, "hours listened") | `totals.listenedSec`, formatted `formatHMDot` (`model.ts:168-177`): `s = max(0, round(sec))`; `0` → `0M`; `1..59` → `45S`; `<1 h` → `18M`; `<100 h` → `42H05M` (minutes zero-padded); else `123H` (floor minutes/hours) |
| Days active | `active = count(daily where reads > 0 or listenedSec > 0)`, shown `active/total` with `total = daily.length` (so `all` caps at 365) |
| Audio generated | `totals.audioSec` |
| Words read / per read | `totals.words`, `avgWordsPerRead` (all words of every read in range, heard or not) |
| Reads / completion rate | `totals.reads`; `finished% = round(completedReads / reads × 100)` |
| Streak | `streak.current`, "Longest N days"; last-7 dots = `daily[-7:]` active flags |
| Pace | `avgListenWpm` wpm; sub "≈ <shortDuration(readingTimeEquivalentSec)> of silent reading" when `readingTimeEquivalentSec > 0`, else "Words per minute heard". `shortDuration`: `45s`, `18m`, `1h`, `1h 12m` |
| Daily listening avg / peak | avg = `totals.listenedSec / activeDays` (0 when no active day); peak = first day with the strictly largest `listenedSec` (none when every day is 0) |
| Heatmap | cell level = `sqrt(value / max)`; peak = largest cell |
| Credits used in range | `totals.charactersBilled` ("billed"); balance comes from the ElevenLabs account API |
| Top apps / voices | ranked by listening; if every row has 0 listening, values shown as read counts |
| Home "Today" | `analytics('7d').today.listenedSec / DAILY_GOAL_SEC`, capped at 1 |

The UI re-requests the summary when history changes, when the day rolls over (timer at next local midnight + 1 s,
re-checked on focus/visibility), and when the window is shown again.

---

## 8. Demo data (`demo-data.ts`, `index.ts:84-89`)

### 8.1 When

Demo / screenshot mode only: after history init, if history is empty, add every record from
`generateDemoRecords(new Date())` one by one (through normal `add`, so they are sanitised and saved to
`%APPDATA%\Vox-Demo\history.json`) and log `seeded N demo records` (scope `app`). Independently of whether seeding
happened, `onboarded` is set to `true` if it is not already (`index.ts:432-435`). A non-empty demo history is never
reseeded, so the demo profile keeps the dates of its first run until `Vox-Demo\history.json` is deleted. Demo reads
never have cached audio (`cached = false`), and have no `listenLog` and no `maxPositionSec`, so analytics put all
their listening at `createdAt` and use the listened-time fallback of `heardFraction`. The screenshot harness turns
`saveHistory` off for its own sample read so it does not land in demo history.

### 8.2 Contract

`generateDemoRecords(now, count = 120)`: deterministic — output depends only on `now` (and the local time zone) and
`count`; `count` reads over the last 60 days (`count - 3` originals + 3 replays when `count >= 20`, fewer only if a
replay finds no anchor; `count < 20` → no replays and no errors), all strictly before `now`, newest first. The
comment in the source promises a current streak of 9 and a longest of 18 (true when today already has reads, i.e.
when the local time is past 00:30). All snippet text is original (Appendix A). The records are returned with fields
in the construction order below; `historyStore.add` re-sanitises them into the canonical order of §5.2.

### 8.3 Tables

`APPS` (`:170-180`) — name, process, weight, snippet kinds, is-browser, window-title template (`T` = snippet title):

| name | process | weight | kinds | browser | window title |
|---|---|---|---|---|---|
| Chrome | chrome.exe | 35 | news, web, research | yes | `T - Google Chrome` |
| Edge | msedge.exe | 11 | news, web, research | yes | `T - Microsoft Edge` |
| Word | winword.exe | 11 | doc | no | `T.docx - Word` |
| Acrobat | acrobat.exe | 9 | research, doc | no | `T.pdf - Adobe Acrobat` |
| Notion | notion.exe | 8 | doc, notes | no | `T` |
| Outlook | olk.exe | 9 | email | no | `T - Outlook` |
| Slack | slack.exe | 6 | chat | no | `Slack - T` |
| Kindle | kindle.exe | 6 | fiction | no | `Kindle - T` |
| Obsidian | obsidian.exe | 5 | notes, research | no | `T - Obsidian` |

`VOICES`: George `JBFqnCBsd6RMkjVDRZzb` 40, Sarah `EXAVITQu4vr4xnSDxMaL` 25, Brian `nPczCjzI2devNBz1zQrb` 20,
Charlotte `XB0fDUnXU5powFXDhCwa` 15. `MODELS`: `eleven_flash_v2_5` 70, `eleven_multilingual_v2` 20,
`eleven_turbo_v2_5` 10. `BROWSER_SOURCES`: context-chip 38, browser-extension 32, hotkey 20, selection-popup 7,
clipboard 3. `DESKTOP_SOURCES`: context-chip 50, hotkey 32, selection-popup 10, clipboard 8. `RATES`: 1 → 55,
1.25 → 25, 1.5 → 15, 2 → 5. `GAP_DAYS` (day offsets with no reads, 0 = today) = {9, 13, 14, 33, 41, 42, 49, 55}.
`SPAN_DAYS = 60`, `REPLAYS = 3`, `ERRORS = ["Rate limited by ElevenLabs. Try again in a moment.",
"Network error. Check your connection and try again."]`.

### 8.4 Primitives (exact)

```rust
// mulberry32, seed 0x5EED70C5. JS: Math.imul = wrapping_mul (low 32 bits), >>> = logical shift on u32.
struct Mulberry32 { a: u32 }
impl Mulberry32 {
    fn next(&mut self) -> f64 {
        self.a = self.a.wrapping_add(0x6D2B_79F5);
        let mut t = self.a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
}
```
- `weighted(items)`: `r = rand() * Σweight`; walk items subtracting weight; first with `r < 0`; fallback last.
- `pick(items)` = `items[floor(rand() * len)]`.
- `round1` as §7.1. `countWords(text)` = whitespace-split non-empty tokens.
- `uuid()`: 32 digits `floor(rand()*16)` as lowercase hex; digit 12 := `4`; digit 16 := `(d & 3) | 8`; format
  8-4-4-4-12.
- `allocate(count, weights)` (largest remainder): `exact_i = w_i / Σw × count`; `out = floor(exact)`;
  `left = count - Σout`; order indices by fractional part descending (stable, ties by index); hand out +1 cycling
  through that order while `left > 0`. Then for each i ascending with `w_i > 0` and `out_i == 0`: donor = first index
  of the maximum `out`; if `out[donor] <= 1` stop; move one read from donor to i. (`Σw <= 0` or `count <= 0` → zeros.)
- `timeOfDayMs()`: `r = rand()`; `r < 0.45` → start 7 h, span 3 h; `r < 0.85` → 19 h, span 4.9 h; else 11 h,
  span 7 h; result `(start + rand() × span) × 3,600,000`.
- `chooseOrigin()`: `roll = rand()`; `roll < 0.04` → `{pick(all snippets), app: none, source: manual}`. Else
  `app = weighted(APPS)`; `snippet = pick(snippets whose kind ∈ app.kinds)` (in table order);
  `source = weighted(browser ? BROWSER_SOURCES : DESKTOP_SOURCES)`; `browser-extension` with a snippet lacking a
  URL becomes `context-chip`.

### 8.5 Algorithm — `generateDemoRecords` — `demo-data.ts:304-421` (rand() call order matters)

```
total = max(0, floor(count)); 0 -> []
rand = Mulberry32(0x5EED70C5)
today = local midnight of now; msIntoToday = now - today
replayCount = total >= 20 ? 3 : 0;  errorCount = total >= 20 ? 2 : 0;  baseCount = total - replayCount

// 1. day weights, offset 0..59 (rand() consumed for EVERY day, gap days included)
for offset in 0..60:
    day = local date today - offset
    w = offset in GAP_DAYS ? 0 : 1
    if day is Saturday or Sunday: w *= 0.75
    w *= 0.7 + 0.6 * (1 - offset / 59)
    w *= 0.6 + rand() * 0.8
    if offset == 0: w = msIntoToday < 30 min ? 0 : w * min(1, msIntoToday / 16 h)
perDay = allocate(baseCount, weights)

// 2. reads, offset 0..59, n in 0..perDay[offset]
dayStart = local midnight(today - offset); dayEnd = local midnight(today - offset + 1)
createdAt = min(dayStart + timeOfDayMs(), dayEnd - 60 000)          // clamp: 23-hour DST days
if offset == 0 and createdAt >= now - 60 000:
    earliest = msIntoToday > 8 h ? 7 h : 0
    createdAt = dayStart + earliest + rand() * max(0, msIntoToday - 120 000 - earliest)
createdAt = floor(createdAt)
(snippet, app, source) = chooseOrigin()
voice = weighted(VOICES); rate = weighted(RATES)
audioDurationSec = round1(utf16_len(snippet.text) / 15 * (0.92 + rand() * 0.16))
completed = rand() < 0.8
listenedSec = completed ? round1(audio / rate) : round1(audio * (0.08 + rand() * 0.8) / rate)
id = uuid(); modelId = weighted(MODELS); favorite = rand() < 0.1          // in this order
record = { id, createdAt, text: snippet.text, textTruncated: false, charCount: utf16_len(text),
           wordCount: countWords(text), voiceId, voiceName, modelId, source, audioDurationSec, listenedSec,
           completed, cached: false, charactersBilled: utf16_len(text), favorite }
if app: sourceApp = app.name; sourceProcess = app.process;
        sourceTitle = source == browser-extension ? snippet.title : app.windowTitle(snippet.title)
        if source == browser-extension and snippet.url: sourceUrl = snippet.url
sort records by createdAt descending (stable)

// 3. failed reads (no rand); records keep completed = false, favorite = false, text etc.
partials = records (sorted, newest first) where !completed and !favorite, skipping the first 3
for i in 0..errorCount while i < partials.len:
    r = partials[i*3] if it exists else partials[i]
    r.error = ERRORS[i]; r.listenedSec = 0; r.audioDurationSec = 0; r.charactersBilled = 0

// 4. replays (copies placed beside an existing read so gap days stay empty)
originals = records where no error and createdAt < now - 3 * 86_400_000  // fixed ms, list fixed before the loop
for i in 0..replayCount while originals non-empty:
    orig = originals[floor(rand() * len)]
    anchors = records (current list, newest first, appended replays included but filtered out)
              where source != replay and createdAt > orig.createdAt + 86_400_000      // fixed ms
    if anchors empty: continue                       // this replay is skipped; only one rand() was consumed
    anchor = anchors[floor(rand() * len)].createdAt
    shift = floor((2 + rand() * 18) * 60 000)
    later = anchor + shift
    if later not on anchor's local date or later >= now - 60 000: later = anchor - shift
    if later not on anchor's local date: later = anchor - 1
    // "same local date" = JS toDateString() equality, i.e. same local Y-M-D
    audio = orig.audioDurationSec || round1(orig.charCount / 15)
    replay = copy(orig) with id = uuid(), createdAt = later, source = replay, audioDurationSec = audio,
             listenedSec = round1(audio), completed = true, charactersBilled = 0, favorite = false
    if orig has no sourceApp: replay.sourceApp = "Vox"
    push replay
return records sorted by createdAt descending
```
(Demo replay records are billed 0; in the real app a cached replay creates no record and a non-cached replay is
billed — cosmetic only.)

---

## 9. Logging (`log.ts`)

- File `<userData>\logs\main.log`, path resolved lazily on the first write (after the demo redirect) and then fixed
  for the process. `mkdir -p`; failure disables file logging for the process (console only, never retried).
  Initial size = current file size, 0 if it cannot be stat'ed (`:54-69`). The size counter only tracks this
  process's own appends.
- Line format (`:101-104`):
  ```
  <ISO-8601 UTC with ms, e.g. 2026-10-09T00:20:30.735Z> <LEVEL padded to 5> [<scope>] <text>\n
  ```
  Levels `DEBUG`, `INFO `, `WARN `, `ERROR` (padded with trailing spaces, then one space). Example:
  `2026-10-09T00:20:30.735Z INFO  [app] Vox 0.1.0 starting (...)`. **[observed]**
- `debug` lines are dropped entirely (file and console) unless `VOX_DEBUG`. Every other line is also echoed to the
  console without its trailing newline (`error` → stderr via `console.error`, `warn` → `console.warn`, others →
  stdout).
- Argument formatting (`:78-88`), joined with single spaces: a top-level `Error` → its stack trace (multi-line;
  written as is, so one entry can span several physical lines) or `Name: message` when there is no stack; string →
  as is; `undefined` → `undefined`; anything else → compact `JSON.stringify`, cut to its first **2000** UTF-16 units
  + `…` (U+2026) when longer; if stringify throws (cycles, BigInt) or returns nothing (functions, symbols) → `String(v)`.
  Errors nested inside objects serialise as `{}`.
- **Masking** (`:71-76`), applied to the whole joined text, a last line of defence (the rule is: never log text being
  read or keys):
  ```
  /\bsk_[A-Za-z0-9]{16,}\b/g                → "sk_***"
  /(xi-api-key["':\s=]+)[^\s"',}]+/gi       → "$1***"     (keeps the header name and separator)
  ```
  Applied in that order, all occurrences. `\b` is the JS ASCII word boundary (`[A-Za-z0-9_]`): `sk_` must not be
  preceded by a word character, and the alphanumeric run must be followed by a non-word character or the end, so
  `xsk_…` or an `sk_…` run that continues with `_` is **not** masked. The header name match is case-insensitive
  (`XI-API-KEY: …` is masked); `\s` is the JS class (§5.8).
- **Rotation** (`:90-117`): before appending, if `size + utf8_len(line) > 1,048,576` (1 MiB): delete `main.1.log`
  (missing is fine), rename `main.log` → `main.1.log` (the name is derived by replacing a trailing `.log` with
  `.1.log`), reset the counter to 0 — even if the delete or rename failed (e.g. another process holds the file; a
  failed delete skips the rename): then keep appending to the big file rather than lose lines. Exactly one old
  generation. A single line larger than 1 MiB is still appended whole. Synchronous append (`appendFileSync`, UTF-8,
  creates the file); the counter grows only after a successful append; all errors swallowed (logging must never
  crash the app).
- Masking applies to the message text only (not the timestamp/level/scope), and to the console echo too.
- Scopes in use (`createLogger(scope)`): `main` (the default `log` export), `app`, `bridge` (`index.ts`), `ipc`,
  `reader`, `tts` (passed to the TTS engine by `reader.ts`), `selection`, `shortcuts`, `shots`, `tray`, `windows`.
  The JSON engine and the stores of this document do **not** use the logger: their warnings go to the console only
  as `[data] ...` (§2.5); callers in `index.ts`/`reader.ts` log the outcomes listed in this document (`retention
  removed N records`, `cache limit lowered: evicted N cached reads`, `audio cache save failed`, ...).
- First line of every run (`index.ts:422`): `Vox <version> starting (electron <ver>, demo=<bool>, mockTts=<bool>)`.

---

## 10. Lifecycle

### 10.1 Startup order — `index.ts:421-441`

Runtime flags + profile redirect → sessionData redirect → early settings read (media keys) → single-instance lock →
app ready → `settings.init()` → `history.init()` ∥ `audioCache.init()` → `onRemoved` hook → demo seeding →
`applyRetention()` → `reconcileCache()` → 6-hourly retention timer → rest of the app.

### 10.2 Normal quit — `index.ts:398-417`, `:569-583`

Persist playback progress into history (reader), stop synthesis, then `settings.flush()`, `history.flush()` (and other
subsystems) in parallel, bounded by **3 s**; then quit; hard exit after a further **4 s** if something refuses to
close. Audio-cache saves in flight are not awaited (an interrupted save has no manifest and is cleaned at next start).

### 10.3 Windows log-off / shutdown — `index.ts:388-396`, `windows.ts:417-444`

No graceful quit happens there and the process may be killed as soon as the window procedure returns, so on
`WM_QUERYENDSESSION` and `WM_ENDSESSION` (every window gets them; the first save wins, later ones are no-ops unless new
changes arrived) do **synchronously**: persist playback → `settings.flushSync()` → `history.flushSync()`. The same
synchronous save runs when the widget or chip window receives an external `WM_CLOSE` (taskkill / installers), before
quitting, because a forced kill can follow within a second or two.

---

## 11. Porting hazards

1. **UTF-16 lengths and offsets.** `charCount`, the 160-char preview, `textStart`/`textEnd` in manifests and every
   offset the player uses are JavaScript string indices (UTF-16 code units). Use `encode_utf16().count()` and convert
   offsets; do not use byte or `char` counts.
2. **Lone surrogates.** `text.slice(0, 160)` can split a surrogate pair; JSON serialisation then writes an escaped lone
   surrogate (`"\ud83d"`). Strict parsers (serde_json into `String`) reject the whole file. The Rust reader must
   tolerate unpaired `\uD800–\uDFFF` escapes (e.g. pre-scan and replace with `\uFFFD`). The port should cut previews
   on a code-point boundary. **[observed]** the current profile has none.
3. **Numbers.** Store all numbers as `f64`; write integral values without `.0` (`1`, `500`, `1792229746902`) and
   others in shortest round-trip form, so files stay byte-compatible (serde_json writes `1.0`). Write `-0` as `0`
   (JS `JSON.stringify(-0)` is `"0"`; serde_json writes `-0.0`; `Math.round(-0.4)` and the coordinate clamp can
   produce `-0`). Write non-finite values as `null` (both JS and serde_json do). JS uses exponent form only below
   1e-6 or from 1e21 up; none of Vox's fields reach those. `Math.round` rounds halves toward +∞ (`-2.5 → -2`); Rust
   `f64::round` rounds away from zero — use `(x + 0.5).floor()` where it matters (round1 of non-negative values is
   unaffected).
4. **Lenient JSON reads.** Decode UTF-8 lossily; strip a UTF-8 BOM; treat empty/whitespace files as corrupt; ignore
   unknown fields; accept wrong types by falling back (never fail the whole file because one field is odd). Two
   serde_json behaviours break this and need a pre-pass or a custom number/string handling: numbers outside f64
   range (`1e400`, JS → Infinity → field fallback; serde_json → whole-file error) and lone surrogate escapes (item 2).
   Keep object key order when parsing (`preserve_order`) where the order is compared (§3.4).
5. **Key order.** Keep struct field order as documented; `listenLog` is ascending (use `BTreeMap`). Readers must not
   depend on order.
6. **Local time.** Day keys, hour keys, streaks, heatmap and demo dates use the OS time zone with DST. Non-existent
   local hours (spring forward) must resolve forward like JS `Date`; ambiguous ones (fall back) to the earlier
   instant.
7. **Case-insensitive paths.** Never create a folder named `cache` in the profile; treat `audio-cache` names
   case-sensitively only for the readId regex.
8. **Retry on sharing violations** everywhere a file is opened, renamed or deleted (§2.1, §6.5). Retrying more
   places than the original (e.g. chunk reads, `secrets.bin`) is harmless and recommended.
9. **`trim()`** in JS also strips U+FEFF and all Unicode spaces; Rust `str::trim` does not strip U+FEFF. The same
   goes for the JS `\s` class used by word counts and search splitting: it includes U+FEFF, Rust's `\s` does not.
10. **Concurrency model.** The TS app is single-threaded: every store mutation, sanitisation and snapshot happens on
    one thread, and only file I/O is concurrent. A threaded Rust port needs one lock per store (settings, history,
    cache index) around mutation + snapshot, and must keep "snapshot at write time" semantics (the debounced writer
    serialises whatever is current when it runs, not what was current when `schedule()` was called).

---

## 12. Test vectors

Generated by running the original TypeScript with `TZ=UTC`, `now = 2026-10-17T12:00:00.000Z`, `count = 120`.

```
mulberry32(0x5EED70C5) first 5: 0.9154872244689614, 0.0947299508843571, 0.13766842219047248,
                                0.9967876193113625, 0.2670450499281287
generateDemoRecords: 120 records; 3 replays, 2 errors, 14 favorites, 99 completed
  [0] id 7c134be0-e9c6-4e9e-a122-0a916f3827ff createdAt 1792229746902 (2026-10-17T09:35:46.902Z) context-chip Chrome
      Brian eleven_multilingual_v2 audio 69.3 listened 69.3 completed charCount 969 wordCount 153
  [1] id 1157c6bc-ea84-475a-b04b-d46941f67a8d createdAt 1792220998996 (2026-10-17T07:09:58.996Z) context-chip Chrome
      George eleven_flash_v2_5 audio 45.8 listened 45.8 completed charCount 687 wordCount 108
  [2] id 7c333d39-59c5-4346-8aae-d8447f5c5246 createdAt 1792155597089 (2026-10-16T12:59:57.089Z) hotkey Word
      Charlotte eleven_multilingual_v2 audio 87.2 listened 87.2 completed charCount 1286 wordCount 209
  oldest id 94b96364-54a4-43d4-941e-2799bee1e1be createdAt 1787126485687 context-chip
computeAnalytics(demo, "30d", now):
  totals {listenedSec 3032.4, reads 71, words 9171, chars 55107, audioSec 3570, completedReads 60, charactersBilled 52235}
  today {listenedSec 115.1, reads 2, words 261}; streak {current 9, longest 18}
  avgWordsPerRead 129, avgListenWpm 166, readingTimeEquivalentSec 2117, daily.length 30
  topApps[0..3] Chrome 31 reads 1495 s; Word 5 reads 296.6 s; Notion 10 reads 287.4 s
  sources context-chip 38, hotkey 13, browser-extension 10, clipboard 3, manual 3, replay 2, selection-popup 2
  topVoices George 34 reads 1491 s; Sarah 18 / 822.6; Brian 10 / 421; Charlotte 9 / 297.8; daily[0] 2026-09-18
computeAnalytics(demo, "all", now):
  totals {listenedSec 4992.3, reads 120, words 15235, chars 92267, audioSec 6036.4, completedReads 99,
          charactersBilled 88530}; streak {9, 18}; daily.length 60, first day 2026-08-19
  avgWordsPerRead 127, avgListenWpm 166, readingTimeEquivalentSec 3491
  topApps[0..3] Chrome 53 reads 2427.4 s; Word 10 reads 544.7 s; Notion 16 reads 498.3 s
  topVoices George 51 / 2175.5; Sarah 33 / 1483.9; Brian 20 / 781.6; Charlotte 16 / 551.3
  sources context-chip 64, hotkey 24, browser-extension 12, clipboard 8, selection-popup 6, manual 3, replay 3
computeAnalytics(demo, "7d", now):
  totals {listenedSec 729.9, reads 17, words 2036, chars 12357, audioSec 823.5, completedReads 15,
          charactersBilled 12357}
  daily 10-11 39.7 s/2 reads/136 words, 10-12 125.1/3/386, 10-13 96.2/2/284, 10-14 173.1/4/424,
        10-15 46.6/2/207, 10-16 134.1/2/338, 10-17 115.1/2/261
  heatmap row sums (Mon..Sun) 125.1, 96.2, 173.1, 46.6, 134.1, 115.1, 39.7
special demo records (newest first):
  ff637708-165e-4f12-a24c-e037a7893430 1790981659614 replay  sourceApp Notion  listened 32.9 audio 32.9 billed 0
  cb704166-b49a-4981-a85b-f09e29400b03 1790761719271 hotkey  error "Rate limited by ElevenLabs. Try again in a moment."
  085ab926-5432-4e12-a582-47ee08ac5690 1790587199843 replay  sourceApp Chrome  listened 63.2 audio 63.2 billed 0
  3a1c73cd-e159-48fb-9c8b-707c03dd4044 1789889475246 context-chip error "Network error. Check your connection and try again."
  d71e7ede-2665-462e-8e31-3f886692526c 1788775017457 replay  sourceApp Word    listened 59.9 audio 59.9 billed 0
```
(All values above were re-run against the TypeScript on 2026-10-17 and match; error records have listened/audio/
billed 0.)

---

## Appendix A: demo snippets (`demo-data.ts:14-159`, verbatim, in order)

`kind` ∈ news, web, research, email, doc, notes, fiction, chat. `url` is present only where shown. Text lengths are
UTF-16 lengths (all characters are in the BMP, so equal to Unicode scalar counts; not byte counts).

```json
[
  {"kind":"news","title":"Harlow Bay tests floating breakwaters as winter storms intensify","url":"https://coastalledger.net/2026/harlow-bay-floating-breakwaters","text":"Harlow Bay will spend the winter as a test site for a new kind of sea defence: a chain of anchored concrete pontoons that rise and fall with the tide instead of standing against it. Engineers say the floating breakwaters absorb wave energy before it reaches the harbour wall, which has been overtopped four times since January. The pilot covers a 300-metre stretch outside the fishing quay and costs a fraction of raising the existing wall. Local skippers are cautiously supportive. \"If it keeps the slipway open in February, I don't care what it looks like,\" said one, who has fished from the bay for thirty years. Sensors on each pontoon will report movement and strain every ten minutes, and the council has promised to publish the data monthly."},
  {"kind":"news","title":"Council approves overnight bus network after two-year trial","url":"https://northfieldgazette.net/local/overnight-buses-approved","text":"The city council voted on Tuesday to make its overnight bus network permanent after a two-year trial that carried more than 1.4 million passengers. Five routes will run every thirty minutes between midnight and five in the morning, linking the hospital district, the airport and the three largest residential neighbourhoods. Night-shift workers made up almost half of all trips during the trial, according to the transport office. Councillors who opposed the plan questioned the running costs, but the motion passed comfortably."},
  {"kind":"news","title":"Battery recycling plant opens on the site of a former steelworks","url":"https://industrydispatch.net/energy/battery-recycling-plant-opens","text":"A battery recycling plant opened this week on land that once held one of the region's largest steelworks, bringing around 220 jobs to a town that lost thousands when the furnaces closed. The facility shreds spent lithium-ion cells from cars and power tools into a powder known as black mass, then uses a water-based process to recover lithium, nickel and cobalt. The operator says recovered metals are close enough to mined material to go straight back into new cells. Capacity in the first year is modest, roughly the batteries from forty thousand cars, but the site has room for a second line. Former steelworkers make up a fifth of the new workforce, many retrained through a programme run with the local college. Environmental groups welcomed the plant while urging regulators to monitor water use closely, since the chemical stage requires large volumes that must be treated before release. The company expects to reach full output by the end of next year."},
  {"kind":"news","title":"Heatwave pushes regional grid to record evening demand","url":"https://coastalledger.net/2026/heatwave-grid-record-demand","text":"Electricity demand reached a record on Wednesday evening as temperatures stayed above thirty degrees well after sunset, keeping air conditioners running across the region. Grid operators said the peak arrived later than usual, around half past eight, after solar output had already faded. Large batteries installed over the past two years supplied almost a tenth of demand during the critical hour, and no rolling outages were needed. Residents have been asked to run dishwashers and washing machines after ten at night until the heat breaks."},
  {"kind":"news","title":"Small libraries are lending drills, sewing machines and telescopes","url":"https://northfieldgazette.net/features/library-of-things","text":"On a Saturday morning the busiest shelf at the Eastgate branch library is not fiction or travel but a row of labelled plastic boxes holding a pressure washer, two sewing machines, a tile cutter and a surprisingly popular telescope. The \"library of things\" started three years ago with a handful of donated tools and now lends more than four hundred items. Members borrow them for a week, just like a book, and late returns are rare. Staff say the idea grew out of a simple observation: most households own a drill that is used for a few minutes a year. Lending one out a hundred times saves money, cupboard space and a surprising amount of plastic packaging. The collection now includes a projector, camping stoves, a carpet cleaner and a set of jam-making pans that is booked solid every autumn. Volunteers run repair sessions on the first Sunday of each month, and the most common fix is a frayed power cable. Several neighbouring towns have asked for advice on starting their own collections."},
  {"kind":"web","title":"Better pour-over coffee in five small steps","url":"https://slowmorningnotes.com/pour-over-basics","text":"Good pour-over coffee is mostly about consistency. Start with fresh beans and grind them just before brewing, roughly as coarse as sea salt. Use about sixty grams of coffee for every litre of water, and weigh both rather than guessing with scoops. Heat the water to just off the boil, around ninety-four degrees. Rinse the paper filter first to remove any papery taste, then add the grounds and pour just enough water to wet them. Wait thirty seconds while the coffee blooms and releases gas. Pour the rest slowly in small circles, keeping the bed evenly submerged, and aim to finish draining at about three and a half minutes. If the cup tastes sour, grind finer; if it tastes bitter or dry, grind coarser. Change one thing at a time and take notes."},
  {"kind":"web","title":"Why your sourdough starter smells like nail polish","url":"https://flourandpatience.com/starter-smells","text":"If your starter suddenly smells sharp, like nail polish remover, it is hungry rather than ruined. As the yeast and bacteria run out of fresh flour they produce more acetic acid and other volatile compounds. Feed it more often, or use a larger proportion of fresh flour and water at each feed, and the smell usually fades within a couple of days. A layer of grey liquid on top means the same thing and can simply be poured off or stirred back in."},
  {"kind":"web","title":"Ten minutes at a time: the quiet case for micro-walks","url":"https://slowmorningnotes.com/micro-walks","text":"You do not need an hour at the gym to feel the benefit of moving. Short walks of around ten minutes, spread through the day, lower blood sugar after meals, loosen stiff hips and give your eyes a break from the screen. The trick is to attach them to things you already do: after lunch, between meetings, before opening email in the morning. Leave the headphones behind once in a while. Boredom is where a surprising number of good ideas come from."},
  {"kind":"research","title":"Urban tree canopy and land surface temperature in mid-sized cities","url":"https://openfieldjournal.org/articles/urban-canopy-surface-temperature","text":"Abstract. Urban heat is increasingly recognised as a public health risk, yet most studies of mitigation focus on large metropolitan areas. We examined the relationship between tree canopy cover and summer land surface temperature across twenty-three mid-sized cities using satellite thermal imagery from five consecutive summers and high-resolution canopy maps. At the neighbourhood scale, each ten percentage point increase in canopy cover was associated with a reduction of 1.2 °C in mean afternoon surface temperature, after controlling for building density, impervious surface and distance to water. The cooling effect was non-linear: gains were largest when canopy rose from very low levels and flattened above roughly forty percent cover. Neighbourhoods with lower median incomes had, on average, half the canopy of wealthier areas and were consistently the hottest. Street trees planted within the past decade contributed measurably less cooling than mature trees, highlighting the long lead time of planting programmes. We discuss implications for prioritising planting in the hottest, least-shaded districts and for protecting existing mature trees during development."},
  {"kind":"research","title":"Spaced retrieval practice and vocabulary retention in adult learners","text":"Abstract. We compared spaced retrieval practice with massed rereading in a twelve-week study of 184 adults learning Portuguese vocabulary online. Participants in the retrieval condition attempted to recall each word before seeing its translation, at intervals that expanded from one day to three weeks. The rereading group spent the same total time reviewing word lists. On a delayed test eight weeks after the final session, the retrieval group recalled 61 percent of items compared with 34 percent for rereading. The advantage held across age groups and prior language experience, although participants consistently rated rereading as the more effective strategy. These findings suggest that learners' intuitions about study methods are an unreliable guide and that tools should schedule retrieval rather than leave it to preference."},
  {"kind":"research","title":"Methods: low-cost soil moisture sensing in allotment gardens","text":"Methods. Capacitive soil moisture probes were installed at depths of 10 cm and 30 cm in forty-two raised beds across six allotment sites. Each probe was paired with a microcontroller that recorded readings every fifteen minutes and transmitted them over a low-power radio link to a gateway at each site. Before deployment, probes were calibrated against gravimetric samples taken from the same soil, giving a calibration curve for each site. Gardeners kept paper diaries of watering times and approximate volumes, which were later digitised and matched against the sensor record. Beds were excluded from analysis when more than twenty percent of readings were missing in any given month."},
  {"kind":"research","title":"Discussion: meeting load after the shift to hybrid work","text":"Discussion. Our results indicate that the move to hybrid work did not reduce meeting load so much as redistribute it. Scheduled meetings fell by eleven percent, but short unscheduled calls more than doubled, and the share of the working day with no calendar commitments fell from 38 to 29 percent. Employees who spent three or more days in the office reported the longest uninterrupted blocks of focused work, largely because colleagues could resolve small questions in person rather than booking a call. These effects were strongest for team leads, whose calendars absorbed much of the coordination that had previously happened informally. We caution against reading these findings as an argument for full office attendance: self-reported wellbeing was highest among people with two office days a week. Instead, the data point to the value of protected focus time agreed at team level, and to the hidden cost of replacing quick conversations with calendar invitations."},
  {"kind":"email","title":"Re: Venue options for the March offsite","text":"Hi Priya, thanks for pulling the shortlist together. I've had a look at all three. The converted mill is lovely but the main room only seats thirty in a horseshoe, which is tight if the design team joins us. The hotel option is easiest for travel, though the breakout rooms have no daylight and we know how that goes by mid-afternoon. My vote is the farmhouse: two decent rooms, good food and a train station fifteen minutes away. Could you check whether they can hold the second week of March and whether there is space for a few people to stay on site? Happy to call them myself if that's easier. Thanks again, Sam"},
  {"kind":"email","title":"Invoice 2291 — payment received","text":"Hello, this is a quick note to confirm we have received your payment for invoice 2291. Thank you. A receipt is attached for your records, and no further action is needed. If any details on the receipt need correcting, simply reply to this email and we will reissue it within two working days."},
  {"kind":"email","title":"Weekly product update — sync, search and a new export","text":"Hi all, here's what shipped this week and what is coming next. Sync: the new conflict resolver is now live for everyone. Edits made offline on two devices are merged paragraph by paragraph instead of keeping both copies, and early numbers show duplicate documents down by roughly eighty percent. Please keep forwarding odd cases to the support channel. Search: results now update as you type and include matches inside attachments. Indexing large workspaces takes a few minutes the first time, so you may see a progress note at the top of the panel. Export: by popular demand, you can export any folder as a single PDF with a table of contents. Page breaks follow your headings, and images keep their original resolution. Next week we're focusing on performance for workspaces with more than ten thousand documents and on the long-requested dark theme for the mobile apps. Thanks to everyone who joined the testing sessions on Thursday — your notes shaped most of the changes above. As always, reply here with questions. Have a good weekend, Lena"},
  {"kind":"email","title":"Quick favour — could you review the draft?","text":"Hey, quick favour if you have twenty minutes this week. I've attached the draft of the grant summary. Mostly I need fresh eyes on the budget section, which I've rewritten three times and can no longer see clearly. No need for line edits, just flag anything that doesn't make sense. Thank you so much!"},
  {"kind":"doc","title":"Onboarding checklist — Support team","text":"Welcome to the support team. Your first week is about learning the product and the people, not closing tickets. Day one: collect your laptop, set up your accounts and join the team channel. Your buddy will walk you through the help desk and the internal knowledge base. Day two and three: shadow two experienced colleagues for at least three hours each, and keep a list of every question you have, however small. Day four: answer your first five tickets in draft mode, with your buddy reviewing replies before they are sent. Day five: join the weekly review, where we look at the trickiest conversations from the past week and agree on how to handle similar cases. By the end of the month you should be comfortable with billing questions, account recovery and the most common integration issues. Please ask for help early; nobody expects you to know everything yet."},
  {"kind":"doc","title":"Q4 planning memo","text":"Purpose. This memo sets out the priorities for the fourth quarter and the trade-offs we agreed during last week's planning sessions. It is meant to be read before Monday's all-hands so that the meeting can focus on questions rather than presentation. Priorities. First, reliability. The two outages in September cost us more goodwill than any feature launch has earned this year, and we will dedicate a third of engineering time to the incident follow-ups, better alerting and a tested failover for the primary database. Second, onboarding. New accounts that do not complete setup within three days rarely return, and the current flow asks for too much too early. The growth team will ship a shorter setup with sensible defaults by mid-November. Third, the partner programme, which will move from pilot to general availability with a self-serve application form. What we are not doing. We are pausing work on the advanced reporting module until January. It remains important, but splitting attention across four large projects is how we ended up with none of them finished last quarter. Risks. Hiring for two senior roles is behind schedule, and the holiday period shortens the quarter by roughly two weeks. Owners for each priority will share a one-page plan with milestones by Friday."},
  {"kind":"doc","title":"Tenancy agreement — Section 7, Repairs and maintenance","text":"7.1 The landlord shall keep in repair the structure and exterior of the property, including drains, gutters and external pipes, and shall keep in proper working order the installations for the supply of water, gas, electricity, sanitation and heating. 7.2 The tenant shall notify the landlord in writing of any defect or disrepair within a reasonable time of becoming aware of it. 7.3 The landlord or their agent may enter the property to inspect its condition or carry out repairs, provided that at least twenty-four hours' written notice has been given, except in an emergency. 7.4 The tenant shall not be responsible for fair wear and tear, nor for damage caused by an insured risk unless that damage results from the tenant's negligence."},
  {"kind":"doc","title":"Design review notes — widget redesign","text":"Attendees agreed the new player card reads clearly at a glance, but the progress dots are too small on high-density screens and should scale with the card. The red accent on the live indicator works; using it for the stop button as well felt alarming and will be removed. Action items: test the compact layout with very long titles, add keyboard shortcuts to the tooltip copy, and check contrast for the grey \"buffered\" dots on the paper theme. Next review is on Thursday."},
  {"kind":"notes","title":"Reading notes — attention and deep work","text":"Main idea: attention behaves more like a budget than a muscle. Every switch between tasks leaves a residue that takes several minutes to clear, so a day of short gaps between meetings produces almost no deep work even when the gaps add up to hours. Practical takeaways: batch shallow tasks into two windows a day, protect at least one ninety-minute block each morning, and keep a written list of open loops so they stop competing for attention. Worth rereading: the chapter on deliberately scheduling boredom."},
  {"kind":"notes","title":"Trip plan — Lisbon long weekend","text":"Friday: land around noon, drop bags at the guesthouse in Graça, late lunch nearby, then walk down through Alfama towards the river before sunset. Saturday: tram early before the queues, the tile museum in the afternoon, dinner booked for eight. Sunday: day trip to Sintra by train, leave by nine and pack a rain jacket because the hills make their own weather. Monday: slow breakfast, the market hall, airport by three. Budget roughly what we spent in Porto last spring. Remember adapters and comfortable shoes; everything is uphill."},
  {"kind":"fiction","title":"The Lantern Keeper — Chapter 3","text":"The storm arrived an hour before it was due, as storms on that coast always did, and by the time Mara reached the top of the tower the glass of the lantern room was streaming with rain. She lit the wick with hands that would not quite keep still. Below her the sea had turned the colour of slate, and somewhere out in it, if the harbourmaster's message was right, a small boat was trying to find its way home. She wound the clockwork that turned the lens and listened to it catch, a sound her father used to call the lighthouse clearing its throat. The beam swung out across the water, white and steady, and for a moment she saw the whole bay laid bare: the black teeth of the reef, the foam over the sandbar, the long empty curve of the shore. Then the rain closed in again. She sat on the iron step with her back against the warm brass housing and began to count the turns. Thirty, forty, fifty. On the sixty-third, a second light answered from the dark, low and yellow and bobbing, far closer to the reef than it should have been. Mara was on her feet before she knew she had decided anything. The bell rope was at the bottom of the stairs, and there were one hundred and twelve of them. She had counted those too, every night for eleven years."},
  {"kind":"fiction","title":"Salt and Static — Chapter 11","text":"The radio had not worked properly since the summer, but Theo kept it on the windowsill out of habit, and on the night the power failed it was the only thing in the flat that still made a sound. He sat on the floor with his back to the cold radiator and turned the dial slowly through the hiss. Mostly there was nothing. Then, near the end of the band, a woman's voice came through, calm and unhurried, reading what sounded like a list of tides. High water at four minutes past two. Low water at twenty past eight. She read them for harbours he had never heard of, towns that might not exist, and he found himself writing them down on the back of an envelope by the light of his phone. When the signal faded he turned the dial back and forth for an hour, but she did not return. In the morning the power came back, and the radio, as if embarrassed, played nothing but static. He kept the envelope anyway."},
  {"kind":"fiction","title":"The Orchard at Low Tide","text":"Every autumn the tide went out so far that the old orchard came back. Nobody in the village could remember it on dry land; the sea had taken it long before any of them were born. But on the lowest tides of October you could walk out across the ribbed sand and find the stumps in their neat rows, black and smooth as river stones, and if you were lucky, a single crab apple lodged between the roots, bright red and impossibly fresh. Nell's grandmother said they were left there on purpose. Nell, who was eleven and believed in tides more than in ghosts, intended to find out by whom."},
  {"kind":"chat","title":"#design-team","text":"Morning all! Uploaded the new icon set to the shared folder. The 24px versions are final; the 16px ones still need a pass because the thinner strokes disappear on the dark theme. If anyone has a minute today, I'd love a second pair of eyes on the settings and history icons in particular."},
  {"kind":"chat","title":"#launch-room","text":"Quick status before standup: the release build passed all checks overnight and the installer is signed. Two things left before we go: the store listing screenshots are still the old theme, and the support macros need the new shortcut names. I can take the screenshots if someone from support owns the macros. Planning to flip the switch at 3pm unless anyone shouts."}
]
```
