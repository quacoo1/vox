# Vox port spec: main window screens (shell, Home, History, Analytics)

Scope: the app ("main") window's chrome and its first three pages, plus the shared view-model code they use. The UI is
being **redesigned**, so this document deliberately ignores visual styling (colours, fonts, spacing, animation). What
it pins down is the information architecture and behaviour: what each region shows, where every number comes from, how
it is formatted, every control and its effect, every empty / loading / error state, all copy, keyboard handling, and the
chart data models. Layout facts are included only where they change *content* (e.g. a narrow window hides a label).

Neighbouring specs in this folder (read them for the other side of each call):

| Topic | Spec |
|---|---|
| Types (`Settings`, `ReadRecord`, `PlayerState`, `AnalyticsSummary`, `AccountInfo`, `Toast`...), IPC surface, toast catalog, mock API | `contracts.md` |
| History store, search, export, analytics computation (`computeAnalytics`), listening ledger, demo data | `persistence.md` |
| Reader / player state machine, replay semantics, widget | `reader-playback.md` |
| Windows (main window size, title-bar overlay, close-to-tray), tray, theme plumbing in the shell | `shell.md` |
| ElevenLabs client, caches (account 60 s, models 1 h, voices 10 min) | `tts.md` |

Source root for citations: `C:/Users/Kwaku/Desktop/Desktop Widget/src/renderer/src/` unless a path starts with
`shared/` or `main/` (those are under `src/`).

Files covered (all read in full): `app/App.tsx`, `app/main.tsx`, `app/store.ts`, `app/components/*` (Sidebar, TopBar,
NowReadingMini, CreditsSummary, ErrorBoundary, format, read-actions, useFreshness, useMediaQuery), `app/components/home/*`,
`app/components/onboarding/*`, `app/pages/Home.tsx`, `app/pages/History.tsx`, `HistoryData.ts`, `HistoryModel.ts`,
`HistoryRow.tsx`, `app/pages/Analytics.tsx`, `app/charts/*`, `lib/api.ts`, `lib/format.ts`, `lib/hooks.ts`,
`lib/mock-vox.ts`. Behaviour that lives in shared design components and that these screens depend on was checked in
`design/toast-store.ts`, `Toast.tsx`, `DotMeter.tsx`, `DotSpark.tsx` (`sparkLevels`), `DotProgress.tsx` (seek bar),
`Segmented.tsx`, `Input.tsx` (`SearchField`), `Dialog.tsx`, `Button.tsx`, `util.ts` (`dotLayout`), `dotfont.ts`
(monogram) and `theme.ts`.

---

## 0. Conventions

* **Copy strings are verbatim.** They include typographic characters: curly quotes `“ ”`, the ellipsis `…` (one
  character), `×` (U+00D7), `·` (U+00B7 middle dot), `–` (U+2013 en dash), `—` (U+2014 em dash), `→`, `≈`. The original
  is *inconsistent* about apostrophes: History and Analytics use `’` (U+2019, e.g. `Couldn’t`), Home / shell /
  onboarding mostly use a straight `'` (e.g. `Couldn't`). Each string below is reproduced exactly; normalising is an
  open question (§12).
* `APP_NAME = "Vox"`, `READ_WITH_LABEL = "Read with Vox"` (`shared/constants.ts:2,4`). Wherever a string below says
  "Vox" or "Read with Vox" it is built from these constants.
* **Locale.** Every number / date formatter in the original uses the *system default locale* (`Intl.*(undefined, …)`,
  which in Electron is the OS UI language). Examples in this spec are en-US. Copy text itself is English-only.
  Some date words are **hard-coded English** and do not follow the locale: chart axis labels (`JAN`…`DEC`,
  `SUN`…`SAT`), heatmap row labels (`MON`…`SUN`), the heatmap's weekday names (`Monday`… in tooltips, summaries and
  the `Peak Sat 21:00` header), and the Week card's initials and weekday names (`S M T W T F S`, `Monday`…). Everything
  produced by `Intl` (Home date line, day-group labels, `bucketTitle`, relative times, full date-times, time of day)
  follows the locale.
* **Durations** are seconds (floating point). **Timestamps** (`createdAt`) are epoch milliseconds. **Day keys** are
  local-calendar `YYYY-MM-DD` strings. **All calendar maths is local time** and must be DST-safe (use calendar-date
  arithmetic, never `+86_400_000`, except where noted).
* "Toast" = the app window's transient notification stack (§1.7). "Navigate" = change route (§2.1).
* Text lengths in the original (`truncate`, etc.) are UTF-16 code units. A Rust port should count `char`s (or grapheme
  clusters) and accept the tiny difference, or count UTF-16 units for exact parity. (The original can cut a surrogate
  pair in half at the truncation point; a `char`-based port cannot.)
* "Whitespace" in `truncate` / `countWords` is JavaScript `\s`: Unicode White_Space **plus U+FEFF**. Rust's
  `char::is_whitespace` lacks U+FEFF; add it if exact parity matters.
* **Icon-only buttons** use their accessible label as their native tooltip (`design/Button.tsx:100`, `title ?? label`)
  unless this spec names a different tooltip. History's row actions suppress the native tooltip and show a custom one
  instead (§4.7).
* **Buttons with a spinner are disabled while it spins** (`design/Button.tsx:42`: `disabled || loading`), so a second
  click on a busy button does nothing.

---

## 1. State, data sources and cross-cutting machinery

### 1.1 Backend calls used by these screens

All go through one façade (`lib/api.ts:4`, the preload bridge `window.vox`; see `contracts.md` §5).

| Call | Returns | Used by |
|---|---|---|
| `settings.get()` | `Settings` | store bootstrap (§1.3), History (`useSettings`) |
| `settings.update(patch)` | `Settings` | theme toggle, onboarding (voice choice, `onboarded`) |
| `settings.setApiKey(key)` | `ApiKeyResult {ok, error?, warning?, account?, settings}` | onboarding key step |
| `settings.onChange(cb)` | event `Settings` | store |
| `eleven.account({refresh?})` | `AccountInfo \| null` (`null` = key can't read account) | credits (§1.5) |
| `eleven.models()` | `TtsModel[]` | Home voice card (model display name) |
| `eleven.voices({refresh?})` | `Voice[]` | onboarding voice step |
| `reader.readText(text, {source:'manual'})` | `ReadResult` | Quick read, onboarding "Read this" |
| `reader.readClipboard()` | `ReadResult` | Quick read "Read clipboard" |
| `reader.replay(readId)` | `ReadResult` | Now playing, Recent, History |
| `player.getState()` / `player.onState(cb)` | `PlayerState` | store, History |
| `player.command(cmd)` | fire-and-forget | transport controls |
| `history.list({search?, favoritesOnly?, source?, offset?, limit?})` | `HistoryPage {items, total}` | Home recent, History |
| `history.toggleFavorite(id)` | `ReadRecord \| null` | History |
| `history.remove(id)` / `history.clear()` | void | History |
| `history.exportJson()` | saved path or `null` (user cancelled the save dialog) | History |
| `history.onChange(cb)` | event (no payload) | Home, History, Analytics ("history version") |
| `analytics.summary(range)` | `AnalyticsSummary` | Home (`'7d'`), Analytics |
| `app.info()` | `AppInfo` (only `demoMode` used here) | store |
| `app.openExternal(url)` | void (http/https only) | History page links, onboarding "Get a key" |
| `app.hideToTray()` | fire-and-forget | top bar |
| `app.setTitleBarTheme({color, symbolColor})` | fire-and-forget | shell theme |
| `app.onNavigate(cb)` | event `AppRoute` | store |
| `app.onToast(cb)` | event `Toast` | store |

`history.list` semantics (main, `persistence.md` §5.8): newest first; `limit` default 50, clamped 1..500; `search` is
case-insensitive, split on whitespace (max 16 terms), and **every** term must appear (substring) in at least one of
`text`, `sourceApp`, `sourceTitle`, `voiceName`, `sourceUrl`. `history:changed` is emitted debounced (100 ms trailing,
max 400 ms wait); while a read plays it fires roughly every 2 s because listening time is persisted.

### 1.2 Data shapes consumed (fields actually read by this subsystem)

```text
PlayerState   { status: 'idle'|'loading'|'playing'|'paused'|'ended'|'error', readId: string|null, title, voiceName,
                sourceApp?, positionSec, durationSec, durationIsEstimate, bufferedFraction (0..1), error? }
ReadRecord    { id, createdAt(ms), text, textTruncated, charCount, wordCount, voiceId, voiceName, modelId, source,
                sourceApp?, sourceTitle?, sourceUrl?, audioDurationSec (0 until known), listenedSec, maxPositionSec?,
                listenLog?, completed, cached, charactersBilled, favorite, error? }
AccountInfo   { tier: string, characterCount, characterLimit, nextResetUnix? (seconds), status? }
AnalyticsSummary {
  range, generatedAt,
  totals { listenedSec, reads, words, chars, audioSec, completedReads, charactersBilled },
  today  { listenedSec, reads, words },
  streak { current, longest },
  daily  [{ date:'YYYY-MM-DD', listenedSec, reads, words }]   // oldest → newest, one per day; 'all' capped at 365
  heatmap number[7][24]                                       // [0=Mon..6=Sun][hour] → listened seconds
  topApps [{ name, reads, listenedSec }]                       // ≤6, sorted by listening desc
  topVoices [{ voiceId, name, reads, listenedSec }]            // ≤6
  sources [{ source, reads }]                                  // all sources with reads, desc
  avgWordsPerRead, avgListenWpm, readingTimeEquivalentSec }
Settings (subset) { hasApiKey, onboarded, voiceId, voiceName, modelId, playbackRate,
  triggers { rightClickChip, hotkeysEnabled, hotkeys { readSelection, readClipboard, playPause, stop }, browserExtension },
  app { theme: 'system'|'paper'|'night' } }
```

Semantics the screens rely on (computed by main, `persistence.md` §7): reads/words/chars count on the day the read was
*created*; listening counts *when it happened* (hour ledger), so replaying an old cached read today adds listening to
today. `today.reads/words` = records created today; `today.listenedSec` = ledger seconds today. Streak = consecutive days
(ending today, or yesterday if today has nothing yet) with ≥1 read created or ≥1 s listened; `longest ≥ current`.
`avgListenWpm` = heard words / listened minutes; `readingTimeEquivalentSec` = heard words at 238 wpm.

### 1.3 App store (`app/store.ts`)

One global state object for the window:

| Field | Type | Initial | Meaning |
|---|---|---|---|
| `route` | `{page, section?}` | parsed from the current URL hash | current screen (§2.1) |
| `settings` | `Settings \| null` | `null` | `null` until first load; the shell renders nothing but a draggable strip until then |
| `player` | `PlayerState` | `IDLE_PLAYER_STATE` | live mirror of the widget's player |
| `credits` | `{status, account}` | `{status:'loading', account:null}` | ElevenLabs balance (§1.5) |
| `demoMode` | bool | false | main serves a sample account without a key (screenshots/demo) |

`connectAppStore()` (`store.ts:167-300`) wires everything once at startup and returns a disposer:

1. **Route**: listen for hash changes; on each, parse and replace `route` if different (`store.ts:173-179`).
2. **Settings**: subscribe to `settings:changed`; then `settings.get()`. *Race rule*: if a live event arrived before the
   initial `get()` resolves, the `get()` result is discarded (the event is newer) (`store.ts:181-194`).
   `applySettings(next)` (`store.ts:141-145`): store it; then if this is the first settings ever, or `hasApiKey` flipped,
   call `refreshCredits(force = (there were previous settings))` (first load is non-forced; a key change is forced).
3. **Player**: same pattern (`onState` first, `getState()` dropped if a live event already came) (`store.ts:196-218`).
   `applyPlayer(p)`: store it; if the transition *finished a read* — previous status ∈ {loading, playing, paused} and new
   status ∈ {idle, ended, error} (`readSettled`, `store.ts:153-161`) — schedule a **forced** credits refresh
   `CREDITS_AFTER_READ_MS = 1500` ms later, debounced (a new settle restarts the timer). Reason: credits are billed when
   text is synthesized, which runs ahead of playback, so a read stopped early or failed has usually billed most of its
   text; waiting 1.5 s lets main record it.
4. **Demo mode**: `app.info()`; if `demoMode` is true set it and call `refreshCredits()` (`store.ts:220-227`).
   Ordering consequence: if settings arrive first with `hasApiKey:false`, credits go to `no-key` and then flip to
   `loading` → `ready` once `app.info()` reports demo mode (a brief "no key" flash in demo mode). If `app.info()`
   resolves before settings, its `refreshCredits()` is a no-op (step 1 of §1.5) and the first settings load does the
   refresh instead.
5. **Main-driven navigation**: `app:navigate` event → `navigate(route)` (`store.ts:229`).
6. **Credits on wake** and **reset timer** (§1.5).
7. **Toasts from main**: `app:toast` event `{id, kind, message, action?: {label, route?}}` → show a toast with the same
   id (replaces an existing one with that id) and the default duration for its kind (§1.7; main sends no duration).
   Its action button navigates to `action.route` **if present**; an action without a route only dismisses the toast
   (`store.ts:283-293`).

Failure handling at startup: a rejected `settings.get()`, `player.getState()` or `app.info()` is only logged
(`[store] settings.get failed`, `[store] player.getState failed`, `[store] app.info failed`). There is **no error UI**:
if `settings.get()` fails and no `settings:changed` event ever arrives, the window stays on the empty draggable boot strip
forever (open question §12). A failed `getState()` leaves the player at `IDLE_PLAYER_STATE` until the next live event.

`updateSettings(patch)` = `settings.update(patch)` then `applySettings(result)`; errors propagate to the caller.

The disposer returned by `connectAppStore()` removes every listener, clears the credits-after-read and reset timers,
and marks the connection dead so late `get()` results are ignored (`store.ts:295-299`).

### 1.4 Freshness: keeping time-based numbers honest (`components/useFreshness.ts`, `lib/hooks.ts`)

The window lives in the tray for days, so "today" must move at midnight and coming back must show current numbers.

| Primitive | Behaviour |
|---|---|
| `now` tick (`useNow(ms)`) | a wall-clock value re-read every `ms`; screens here use 60 000 ms (relative times, Home date/greeting, heatmap "now" cell) |
| day key (`useDayKey`) | local `YYYY-MM-DD` of `now` with a 60 s tick, so it changes within a minute after midnight |
| wake count (`useWakeCount(30_000)`) | increments when the window gains focus or becomes visible again (e.g. reopened from tray), at most once per 30 s; ignored while hidden |
| freshness key | `"{dayKey}#{wakeCount}"` — any change means "refetch" |
| history version | increments on every `history:changed` event |

Details that matter for parity (`lib/hooks.ts:56-63`, `useFreshness.ts:16-33`):
* The `now` tick is a plain interval started when the screen mounts; it is **not aligned to minute boundaries**, so
  minute-granular text (relative times, the greeting, the day key) can lag the wall clock by up to 60 s.
* The wake counter's "last wake" starts at mount time, so a focus/visibility event within 30 s of a page opening does not
  count. Both window `focus` and document `visibilitychange` (to visible) are listened to; the throttle collapses the
  pair that fires when the window is restored. Events while the document is hidden are ignored.
* Each page (Home, Analytics) owns its own wake counter, which restarts when the page is re-created (§2.4). That is
  harmless because a newly created page fetches anyway.
* Port: drive all three from one "clock + wake" service (minute tick, day change, wake event) rather than per-page
  timers; also treat `WM_TIMECHANGE` (user changed the clock) as a day re-check.

### 1.5 ElevenLabs credits state machine (`store.ts:59-134, 231-281`)

States: `loading` · `no-key` (no API key stored) · `ready` (account holds the latest known numbers) · `restricted` (the
key works but may not read account info) · `error` (never loaded: network etc.).

`refreshCredits(force = false)`:

1. If settings are not loaded yet: return.
2. `seq = ++creditsSeq` (latest call wins; stale responses are dropped).
3. If `!settings.hasApiKey && !demoMode` → `no-key`, done.
4. If current status is `no-key` or `error` → set `loading` (from `ready`/`restricted` the old numbers stay visible
   while refreshing — stale-while-revalidate).
5. `account = eleven.account(force ? {refresh:true} : none)` (main caches 60 s; `force` bypasses).
6. If `seq` is stale → ignore. Else `account ? ready(account) : restricted`.
7. On exception (if not stale): log; **keep the last good numbers** — only switch to `error` when the current status is
   neither `ready` nor `restricted`.

Refresh triggers (complete list):

| Trigger | Forced? | Source |
|---|---|---|
| First settings load | no | `store.ts:144` |
| `hasApiKey` changed | yes | `store.ts:144` |
| A read settled (1.5 s debounce) | yes | `store.ts:199-204` |
| Demo mode detected | no | `store.ts:225` |
| Window focus / became visible | no, throttled to once per `CREDITS_WAKE_THROTTLE_MS = 30 000` ms | `store.ts:233-252` |
| …but the account's `nextResetUnix` has passed and no forced refresh was done for that reset value yet | yes, bypasses the throttle; remembered per reset value | `store.ts:235-246` |
| Reset timer fires (window stayed open across the reset moment) | yes, once per reset value | `store.ts:254-281` |
| Analytics credits card mounted | no | `charts/CreditsCard.tsx:39-41` |
| Analytics "Refresh balance" / "Retry" | yes | `charts/CreditsCard.tsx:55,77` |
| Home credits "Retry" | yes | `home/CreditsCard.tsx:57` |
| Onboarding key connected | yes | `onboarding/StepApiKey.tsx:49` |

Reset timer: whenever `credits` changes, re-arm for the account's `nextResetUnix` (if it differs from the armed value):
wait until `nextResetUnix*1000 + 5000` ms. Long waits are re-armed in steps no longer than `MAX_TIMEOUT_MS =
2_147_483_647` ms (JS `setTimeout` ceiling); at fire time, if the reset moment has passed and it was not already forced,
force a refresh. (In Rust, also re-check on resume from sleep — a timer can sleep through the moment.)

Exact algorithm (`store.ts:233-281`):

```text
state: lastWake = <connect time>, forcedReset = 0, armedFor = 0, resetTimer = none

resetPassed() = r = credits.account?.nextResetUnix ?? 0
                r > 0 && r*1000 <= now && r != forcedReset ? r : 0

onWake (window focus, or document became visible; ignored while hidden):
  r = resetPassed()
  if r == 0 && now - lastWake < 30_000: return          // throttled
  lastWake = now
  if r: forcedReset = r
  refreshCredits(force = r != 0)

armReset(account)                                       // called at connect and on every credits change
  r = account?.nextResetUnix ?? 0
  if r == armedFor: return
  armedFor = r; cancel resetTimer
  if r == 0: return                                     // account null (loading/restricted/error/no-key) disarms
  tick():
    wait = r*1000 - now
    if wait > 0: resetTimer = after(min(wait + 5000, MAX_TIMEOUT_MS), tick); return
    if r == forcedReset: return
    forcedReset = r; refreshCredits(true)
  tick()                                                // runs at once: an already-past reset forces immediately
```

`forcedReset` is shared by the wake path and the timer path, so one reset value is force-refreshed at most once in
total. Because `armReset` runs `tick()` immediately, an account whose `nextResetUnix` is already in the past (main's
cache still holding last period's numbers) triggers one forced refresh as soon as it is stored.

### 1.6 Read actions (`components/read-actions.ts`)

Main already toasts almost every read failure (missing key, empty text, engine errors). These wrappers only add toasts
for the cases main stays silent about, plus transport failures:

| Action | Call | Extra failure handling |
|---|---|---|
| `readText(text)` | `reader.readText(text, {source:'manual'})` | call throws → error toast `{message or "Something went wrong"}`, id `read-failed`; returns `{ok:false, code:'unknown', error: message}` (`message` = the error's message or its string form, possibly empty) |
| `readClipboard()` | `reader.readClipboard()` | same |
| `replayRead(readId)` | `reader.replay(readId)` | same; additionally when `!ok` and (`code == 'not-cached'` **or** (`code == 'unknown'` and no `readId` in the result)) → error toast `res.error ?? "That read can't be replayed"`, id `replay-{readId}`. (Those are "nothing left to replay" and "the record vanished"; a failed re-synthesis carries its new `readId` and main toasts it.) |

Main's replay rules (for context, `reader-playback.md` §2.6): cached audio → free replay; otherwise, if only a preview
was stored, fails with `not-cached` ("The audio is no longer cached and the full text was not saved"); otherwise
re-synthesizes (bills credits) as a new read with `source:'replay'`. Missing record → `unknown`, "That read is no longer
in your history".

### 1.7 Toast system (needed for History's undo semantics) (`design/toast-store.ts`, `design/Toast.tsx`)

| Rule | Value |
|---|---|
| Kinds | `info`, `success`, `error` (default `info` when no kind is given); glyph info / check / alert |
| Default duration | info 4000 ms, success 3000 ms, error 6500 ms (`TOAST_DURATION`, `toast-store.ts:32`) |
| With an action button | `max(kind duration, 6000)` ms |
| Explicit `duration: 0` (or negative) | sticky |
| Stack | max `MAX_TOASTS = 4`; newest last; pushing a 5th drops the oldest **immediately** (no exit animation) |
| Same `id` pushed again | atomically replaces the old toast in one store update (the id is never absent from the stack), moved to newest, fresh timer; if the old one was mid-exit, its exit is cancelled |
| No id given | a unique id `t{Date.now() base36}{counter base36}` |
| Hover or keyboard focus inside a toast | pauses its timer; on resume the remaining time is `max(800, remaining)` ms. Pause is one flag set by pointer-enter or focus and cleared by pointer-leave or focus leaving the toast, so moving the pointer away resumes even while focus is still inside |
| Leaving (timer, close button, action) | 180 ms exit animation (`EXIT_MS`), *then* removed from the stack |
| Programmatic `dismiss(id)` | removes at once, no exit animation |
| Action click | runs the action, then the toast leaves |
| Close button | label "Dismiss notification" |
| Roles | error toasts are alerts (assertive); others are status messages; the stack is a polite live region named `Notifications` |
| Placement | shell: bottom-right; onboarding: bottom-centre |
| Optional bold title line | used by "History exported" |

"Removed from the stack" is the event History's undo logic waits for (§4.10). The removal reasons are therefore:
timed out (+180 ms), close button (+180 ms), action button (+180 ms), evicted by the cap (immediate), programmatic
dismiss (immediate).

### 1.8 Settings-derived helpers used on several screens

* **Read-selection hotkey shown in hints**: `settings.triggers.hotkeysEnabled ? settings.triggers.hotkeys.readSelection : ""`.
  Empty string = "no hotkey" (the UI falls back to right-click wording).
* **Key caps**: an accelerator string like `Ctrl+Alt+R` is shown as separate key caps (`Ctrl` `Alt` `R`); screen-reader
  text "Ctrl plus Alt plus R"; an empty accelerator shows `Not set` (design `KeyCaps`, details in the settings/design spec).

---

## 2. Shell (`app/App.tsx`, `app/store.ts`, `components/Sidebar.tsx`, `TopBar.tsx`, `NowReadingMini.tsx`, `ErrorBoundary.tsx`)

### 2.1 Routing

Routes are `{page, section?}`. Pages: `home`, `history`, `analytics`, `voices`, `settings`, `onboarding`, plus the
dev-only `gallery`. The original stores the route in the URL hash, e.g. `#/settings/account`.

```text
NAV_PAGES (sidebar order, Ctrl+1..5) = [home, history, analytics, voices, settings]
KNOWN_PAGES = NAV_PAGES + [onboarding, gallery]
```

`parseHash(hash)` (`store.ts:31-44`):

1. `path = hash` with a leading `#` and any leading `/`s removed (`/^#?\/*/`), then cut at the first `?` or `#`.
2. Split on `/`: `head` = first segment.
3. `page = head` if `head ∈ KNOWN_PAGES`, else `home`.
4. Section = the remaining **non-empty** segments joined with `/` — only if `page == head` (an unknown page drops its
   section). Empty → no section.
5. Section is URI-decoded; a malformed escape keeps the raw text.

Examples: `""` → home; `#/history` → history; `#/settings/account` → settings/account; `#/nope/x` → home (no section);
`#//analytics//` → analytics; `#/settings/a%20b` → settings/"a b"; `#/settings/%E0` → settings/"%E0";
`#/settings/account?x=1` → settings/account; `#/History` → home (page names are case-sensitive).

`routeHash(route)` = `#/{page}` + (`/` + `encodeURIComponent(section)` if any) — the **whole** section is one encoded
component, so a section `a/b` becomes `#/settings/a%2Fb` (which parses back to `a/b`). It must match main's own
`routeHash()` in `main/windows.ts`, which builds the URL when main opens the window on a route. `sameRoute` compares page
and section (`undefined` ≡ `""`). A route given to `navigate` with an empty section is normalised to no section
(`toRoute`, `store.ts:55-57`).

`navigate(to)` (`store.ts:94-104`; `to` is a page name or a route):
1. Update `route` if different.
2. If the URL hash differs, set it — this **pushes a history entry** (so mouse back/forward work); the resulting
   hash-change sees the same route and does nothing.
3. If the hash is already identical **and** the route has a section, re-announce a hash-change anyway, so the page
   scrolls back to the section (e.g. clicking a toast action "Open settings" twice).

Port: keep an explicit back/forward stack of routes and a "route re-announced" event for the same-section case. Main can
also navigate the window (`app:navigate`), which goes through the same `navigate`.

Settings sections that this subsystem links to: `account`, `triggers`, `integrations` (full list: account, voice,
playback, triggers, widget, appearance, library, integrations, about).

### 2.2 Boot and theme (`App.tsx:60-100`)

* Until `settings` load, render only an empty 44 px **draggable** strip (window background visible; main already painted
  the window in the saved theme, `shell.md` §5.6). Settings arrive "within a frame or two". If they never arrive
  (`settings.get()` rejected, §1.3) the strip stays forever — no error, no retry. The `#/gallery` route is the only
  thing rendered before settings load.
* Theme: `resolved = settings.app.theme` if `paper`/`night`; for `system`, follow the OS dark-mode preference live
  (dark → `night`, light → `paper`) (`design/theme.ts:13-38`). Apply the palette *before the first paint with real
  settings* so the wrong theme never flashes. Whenever `resolved` changes, send `app.setTitleBarTheme(...)` so the
  native caption buttons sit on the canvas colour: paper `{color:'#ececec', symbolColor:'#0b0b0b'}`, night
  `{color:'#000000', symbolColor:'#f2f2f2'}` (`design/theme.ts:7-10,46-49`).

### 2.3 Onboarding gate (`App.tsx:78, 102-112`)

`onboarding = settings loaded && page != gallery && (page == onboarding || !settings.onboarded)`.

* Onboarding layout: top bar in **minimal** mode (no brand), the onboarding flow (§10), toasts bottom-centre. **No
  sidebar, no shell shortcuts** (neither Ctrl+1..5 nor mouse back/forward). While not onboarded, *any* route shows
  onboarding (deep links included). The flow has its own crash boundary (§2.9) with the fixed reset key `onboarding`.
* Otherwise the full shell: top bar, sidebar, page area, toasts bottom-right.
* `gallery` (`#/gallery`) is a dev-only design reference page, lazy-loaded, rendered bare (no shell). Not needed in the
  port unless you want a component gallery.

### 2.4 Shell regions

```text
┌──────────── top bar (44 px drag strip; native caption buttons at the right) ────────────┐
│ sidebar (nav, spacer, mini player, footer)  │  page area (scrolls; one page at a time)  │
└──────────────────────────────────────────────────────────────────────────────────────────┘
toasts (bottom-right)
```

Page area behaviour:
* Scrolls to the top whenever `route.page` changes (`App.tsx:81-83`). A section change on the same page does not.
* The page is **re-created** when `route.page` changes (all page-local state — History filters, expanded rows — resets;
  Analytics re-reads its stored range) (`App.tsx:120`).
* Each page is wrapped in a crash boundary (§2.9) whose reset key is the page id.

### 2.5 Top bar (`components/TopBar.tsx`)

* The whole bar is a window-drag region; interactive content must stop before the native caption buttons (the
  original uses the title-bar-overlay safe width). Window: default 1180×780, min 940×640, frameless with caption-button
  overlay height 44 (`main/windows.ts:502-518`).
* Brand (hidden in minimal/onboarding mode): the app logo tile + the app name in upper case (`VOX`; accessible name
  `Vox`).
* Actions (right):
  1. **Theme toggle**: label `Switch to night theme` when the resolved theme is paper (icon: moon), `Switch to paper
     theme` when night (icon: sun). Click → `updateSettings({app:{theme: resolved == 'night' ? 'paper' : 'night'}})`.
     Note this replaces `system` with an explicit theme. Failure → error toast
     `Couldn't change the theme: {message}` (`App.tsx:85-89`).
  2. **Send to tray**: label `Send to tray` (icon: tray) → `app.hideToTray()` (hides the window; app keeps running).
  Both are icon-only buttons whose tooltip is their label (§0).

### 2.6 Sidebar (`components/Sidebar.tsx`)

**Nav list** (accessible name `Vox sections`), in `NAV_PAGES` order:

| # | Page | Label | Icon (semantic) | Shortcut |
|---|---|---|---|---|
| 1 | home | Home | home (the widget dashboard) | Ctrl+1 |
| 2 | history | History | history | Ctrl+2 |
| 3 | analytics | Analytics | chart | Ctrl+3 |
| 4 | voices | Voices | voice | Ctrl+4 |
| 5 | settings | Settings | gear | Ctrl+5 |

* The item for the current page is marked current (`aria-current=page`). Click → `navigate({page})` (drops any section).
* Each item shows its digit (1–5) as a shortcut hint, revealed on hover/keyboard focus (hidden in rail mode); exposes
  `aria-keyshortcuts="Control+N"`.
* Keyboard inside the list: `ArrowDown`/`ArrowUp` move focus to the next/previous item **with wrap-around**; `Home`/`End`
  to first/last; `Enter`/`Space` activate; `Tab` leaves the list (`Sidebar.tsx:30-44`). Arrow keys only move focus
  (they do not navigate); they act only while focus is on a nav item.

**Rail mode** (window narrower than 1100 DIP: `RAIL_QUERY = (max-width: 1099px)`, `useMediaQuery.ts:18`):
* Sidebar collapses to an icon column (72 px vs 232 px). Labels become screen-reader-only and move into a tooltip on the
  right of each icon; digit hints hidden.
* The **footer is not rendered** (no hotkey hint, no credits).
* The mini player switches to its rail variant (§2.7).

**Mini player**: rendered between a spacer and the footer **on every page except Home** (Home has the full player card;
"a second title, progress bar and pause button there would be noise") (`Sidebar.tsx:58-59`).

**Footer** (not in rail mode; `Sidebar.tsx:102-140`):
1. Hint row: label `Read selection`, then the read-selection hotkey as key caps; if there is no hotkey (§1.8):
   `Right-click → Read with Vox`.
2. Credits:
   * credits `ready` → a button (→ `settings/account`) showing label `Credits`, the percent left `{round(left*100)}%`
     (screen reader: `{n} percent left`), and a one-row dot meter of `left` (accessible name
     `ElevenLabs credits left`; turns to the warning tone below 15 %). `left = creditsLeftFraction(count, limit)`
     (§11.1).
   * else if `!settings.hasApiKey` → a warning button (alert icon) `Add your ElevenLabs key` → `settings/account`.
     (In demo mode without a key this also shows while credits are still loading or failed, because only `ready`
     suppresses it.)
   * else (loading / restricted / error with a key) → nothing.
   * The percentage text itself does not change tone; only the meter does (`tone = auto`, warning below 0.15).

### 2.7 Mini player ("Now reading", `components/NowReadingMini.tsx`)

Visible only while `player.status ∈ {loading, playing, paused, error}`; hidden on Home and for idle/ended.

Status labels: loading `Preparing` · playing `Now reading` · paused `Paused` · error `Couldn't read`.
Title: `player.title || "Untitled selection"`.

**Full variant** (accessible region name `Now reading`):
* Top line: live dot (pulsing when playing) + status label + activity indicator (a loader while loading, otherwise a
  small level animation that moves only while playing).
* Title button (2 lines max; tooltip `Open the player`) → navigate to Home.
* Error status: the text `player.error || "Playback stopped."` instead of the progress bar. Otherwise a non-interactive
  progress bar: value `positionSec/durationSec` (0 if duration ≤ 0), buffered `bufferedFraction`, name `Reading
  progress`.
* Foot: if `durationSec > 0`, the time left `remainingClock(position, duration, durationIsEstimate)` e.g. `-3:12` or
  `~-3:12` (screen reader: `{formatClock(max(0, duration-position))} left`); otherwise empty.
  Controls: Play/Pause (label `Pause` while playing else `Play`; enabled only when playing/paused; sends
  `{type:'toggle'}`) and Stop (label `Stop`; always enabled; sends `{type:'stop'}`).

Status → controls summary (full variant): loading → Play disabled, Stop enabled; playing → Pause enabled; paused →
Play enabled; error → Play disabled, Stop enabled (Stop is never disabled). The level animation is a 4-bar × 3-row dot
wave that animates only while playing.

**Rail variant**: a single icon button with a tooltip `{Status label}: {title}` (shown to the right of the rail).
* playing/paused: label `Pause`/`Play`, click toggles; icon pause/play.
* loading: icon = loader; error: icon = alert. Label `Open player`; click → navigate Home.
* A small attention dot when playing or error.

### 2.8 Global shell shortcuts (`App.tsx:36-58`)

Active only when settings are loaded and not in onboarding.

| Input | Effect |
|---|---|
| `Ctrl+1` … `Ctrl+5` | navigate to `NAV_PAGES[n-1]`. Ignored if Alt, Shift or Meta is also held, or if the key event is an auto-repeat. Default action suppressed. |
| Mouse button 3 ("back", XBUTTON1) released | history back |
| Mouse button 4 ("forward", XBUTTON2) released | history forward |

Matching details (`App.tsx:40-50`):
* The digit is read from the produced key *character* (`Number(e.key)` must be an integer 1..5), so both the top-row
  digits and the numpad digits (with NumLock on) work; `Ctrl+0`, `Ctrl+6`… are not handled here (no default suppressed).
  On a native port, accept `VK_1..VK_5` and `VK_NUMPAD1..VK_NUMPAD5`.
* The listener is window-wide and runs **even while focus is in a text field** (Quick read box, History search): Ctrl+2
  in the search box still switches pages.
* Back/forward fire on button *release* anywhere in the window and walk the route history stack (§2.1).

(Main adds `F12` / `Ctrl+Shift+I` dev-tools in unpackaged builds — not needed in the port.)

### 2.9 Page crash boundary (`components/ErrorBoundary.tsx`)

If drawing a page fails, only that page is replaced by a centred panel: alert glyph, title `This page hit a snag`, body
= the error message or `Something went wrong while drawing it.`, primary button `Try again` (clears the error and
re-renders). Navigating to another page also clears it (the boundary's reset key is the page id, so a section change
on the same page does **not** clear it). The error is logged (`[app] page crashed`, with the component stack). The
boundary catches drawing errors only; failed async calls are handled by each screen as described below.

---

## 3. Home (`pages/Home.tsx`, `components/home/*`)

### 3.1 Data (`components/home/useHomeData.ts`)

`HomeData = { summary: AnalyticsSummary | null (range '7d'), recent: ReadRecord[] | null (5 newest), failed: bool }`.

* Fetch = `analytics.summary('7d')` and `history.list({limit:5})` in parallel; success → replace both, `failed=false`;
  failure → keep previous data, `failed=true` (logged `[home] refresh failed`). **`failed` is not displayed anywhere**:
  if the very first fetch fails, the Today / Week / Recent cards stay in their loading state until the next trigger
  (open question §12).
* Refetch triggers: history version, `player.status` change (any status change, including playing ↔ paused),
  `player.readId` change, freshness key (day rollover or window wake, §1.4).
* Refreshes are **trailing-debounced `REFRESH_DEBOUNCE_MS = 700` ms** (each trigger restarts the timer) — but only
  once a fetch has **succeeded** (`loaded` flag, set on success only). Until then every trigger fetches after a 0 ms
  delay (triggers in the same instant still coalesce). Reason for the debounce: history changes arrive every ~2 s while
  a read plays.
* The sequence number is taken when the timer fires (when the request starts), and only the latest request may apply
  its result; a failure only sets `failed` if it is still the latest. Results after leaving the page are ignored
  (leaving bumps the sequence) (`useHomeData.ts:33-52`).

`useModels(enabled)`: `eleven.models()` when `enabled` (= `settings.hasApiKey`) becomes true; failure → `[]` silently
(the id-based model label is used instead). When the key goes away the last list is kept, not cleared. Main caches
models for an hour (`useHomeData.ts:58-72`).

### 3.2 Header

* Date line: today as `weekday long, month long, day numeric` in the system locale, e.g. `Friday, October 17`.
* Title: greeting by local hour (`greeting`, `components/format.ts:72-77`): 05:00–11:59 `Good morning`, 12:00–17:59
  `Good afternoon`, otherwise `Good evening`.
* Both re-evaluated on a 60 s tick.

### 3.3 Card inventory and order

Reading order (the original is a 12-column bento; prominence in brackets): Now playing [large] · Today · Credits ·
Quick read [large] · This week · Voice · Recent [large, tall] · Ways to read [tall].

Responsive content change: when the page content area is narrower than 900 px the grid becomes 6 columns and Now
playing, Quick read, Recent and Ways to read span the full width (no content changes, only order/stacking).

### 3.4 Now playing card (`home/NowPlayingCard.tsx`) — the in-app twin of the floating player

Accessible name `Now playing`. Mode: **idle** when `status == 'idle'` or (`status == 'ended'` and `title` is empty);
otherwise **player**.

**Idle body**
* Cursor icon.
* Title: `Select text, then right-click`.
* Sub line: `Choose “Read with Vox”` + (if a read-selection hotkey exists: ` or press` followed by its key caps).
* An empty, non-interactive progress bar (name `Nothing playing`).
* Footer: label `Vox Player`; if a latest read exists (`recent[0]`): button `Replay last` (tooltip =
  `truncate(lastRead.text, 120)`; shows a spinner while the call is in flight) → `replayRead(lastRead.id)`.

**Player body**
* Top-left: loader (`Preparing audio`) while loading, otherwise the level animation (active only while playing;
  accessible label `Playing` while playing).
* Badges: `sourceApp` (if any); `Live` (pulsing, while playing); `Paused`; `Finished` (ended).
* Title: `player.title || "Untitled selection"` (tooltip: the raw `player.title`, so no tooltip when it is empty).
* Meta line:
  * error → `player.error || "Playback stopped."` (announced as an alert);
  * loading → `Preparing audio…`;
  * else `{voiceName} · {sourceApp}` (empty parts dropped, joined with ` · `).
* Time (only if `durationSec > 0`): ended → `formatClock(duration)` (screen reader `{clock} total`); else
  `remainingClock(position, duration, durationIsEstimate)` (screen reader `{formatClock(max(0,duration-position))} left`).
* Seek bar: value = 1 if ended, else `position/duration` (0 if no duration); buffered = `bufferedFraction`; **interactive
  only while playing or paused** (otherwise disabled and not focusable). Seeking sends `{type:'seekFraction', fraction}`
  on pointer release (dragging previews the position without sending); keyboard: Right/Up +0.02, Left/Down −0.02,
  PageUp +0.1, PageDown −0.1, Home 0, End 1 (each sends immediately). Accessible name `Seek`, value text
  `{formatClock(position)} of {formatClock(duration)}`; value now = `round(fraction*100)` of 0..100.
  Seek mechanics (`design/DotProgress.tsx:74-131`):
  * Only the primary mouse button starts a drag; the pointer is captured so dragging outside the bar keeps working.
  * Pointer → fraction: `clamp((x − trackLeft − inset − dot/2) / (trackWidth − dot − 2·inset), 0, 1)`, where `dot` is
    the track dot diameter and `inset` keeps the playhead from overhanging the ends (0 when there is no knob). Any
    geometry works as long as the ends map to 0 and 1.
  * Releasing commits; a cancelled pointer (capture lost, touch cancel) ends the drag **without** seeking.
  * Keyboard steps are relative to the *current* value and clamped to 0..1.
* Footer: label `Vox Player`; transport buttons:

| Button | Label | Enabled when | Action |
|---|---|---|---|
| Stop | `Stop` | playing, paused or loading | `{type:'stop'}` |
| Previous | `Previous sentence` | playing or paused | `{type:'skip', direction:-1, unit:'sentence'}` |
| Main | `Pause` (playing) / `Play again` (ended) / `Try again` (error) / `Play` (otherwise) | not loading | playing/paused → `{type:'toggle'}`; ended or error **with** a `readId` → `replayRead(readId)`; otherwise nothing |
| Next | `Next sentence` | playing or paused | `{type:'skip', direction:1, unit:'sentence'}` |

Main button icon: pause while playing; a replay icon for ended/error; play otherwise.

### 3.5 Today card (`home/TodayCard.tsx`)

Constant `DAILY_GOAL_SEC = 15 * 60` (`shared/constants.ts:44`) → `goalMin = 15`.

* Header: headphones glyph, label `Today`, badge `Goal 15m`.
* Loading (`summary == null`): busy state with placeholders for meter, numeral and caption.
* `listened = summary.today.listenedSec`, `reads = summary.today.reads`, `words = summary.today.words`.
* `share = min(1, listened / 900)`, `done = listened >= 900`.
* Meter (2 dot rows) filled by `share`; when `done`, the last lit dot is drawn in the accent ("goal reached" signal).
  Accessible name `Daily goal of 15 minutes`; value text `Goal reached: {durationSpeech(listened)} listened` when done,
  else `{round(share*100)}% of the daily goal`.
* Big numeral: `dotDuration(listened)` e.g. `4H20M` (screen reader `{durationSpeech} listened today`).
* Caption line 1: `{formatNumber(reads)} read|reads · {formatCompact(words)} word|words` (singular when exactly 1; the
  word count has tooltip `{formatNumber(words)} words`).
* Caption line 2: `Daily goal reached` when done, else `{max(1, ceil((900 - listened)/60))}m to your daily goal`.

### 3.6 Credits card (Home) (`home/CreditsCard.tsx`)

Accessible name `ElevenLabs credits`. Driven by the store's credits (§1.5); it does **not** trigger a refresh by itself.

| Credits status | Content |
|---|---|
| `ready` | Header: bolt glyph, `Credits`, badge = title-cased `tier` (split on runs of `_`, whitespace or `-`, drop empty parts, upper-case the first character of each word and keep the rest unchanged, join with one space: `creator`→`Creator`, `pro_plus`→`Pro Plus`, `PRO`→`PRO`; no badge when tier is empty). Body: **CreditsSummary** (§6.1) with the large numeral. |
| `loading` | Header `Credits`; busy placeholders |
| `no-key` | alert glyph; title `No API key`; body `Add your ElevenLabs key to start reading.`; primary button `Add key` → `settings/account` |
| `restricted` | info glyph; `Usage hidden`; `This key can't read account usage. Reading still works.`; secondary button `Manage key` → `settings/account` |
| `error` | info glyph; `Credits unavailable`; `Couldn't reach ElevenLabs.`; button `Retry` → `refreshCredits(true)` |

### 3.7 Quick read card (`home/QuickReadCard.tsx`)

* Header: text glyph, label `Quick read`, right: live word count `{formatNumber(n)} word|words` when `n > 0`, else
  empty (announced politely). `n = countWords(text)` (§11.2).
* Multi-line text box, placeholder `Paste or type text…`, 4 rows, not resizable, spell-check off.
* Buttons:
  * `Read` (primary; disabled when `text.trim()` is empty; spinner while running) → `readText(text)` — the **untrimmed**
    text, source `manual`.
  * `Read clipboard` (spinner while running) → `readClipboard()`.
  * Only one of the two may run at a time (a second click while busy is ignored).
* `Ctrl+Enter` (or Meta+Enter) in the box, when non-empty → same as `Read`. Hint: key caps `Ctrl` `Enter` + `to read`.
* The text is **not** cleared after reading. It is page-local, so it is lost when leaving Home (§2.4).
* The card is labelled by its header text `Quick read`; the text box has the same accessible name.

### 3.8 This week card (`home/WeekCard.tsx`)

* Header: chart glyph, label `This week`, badge (only when loaded) = `dotDuration(sum of listenedSec over the 7 days)`.
  Note: it is the rolling last 7 days (today last), not a calendar week.
* Loading: one placeholder block.
* Chart: 7 columns × `ROWS = 6` dots, `days = summary.daily.slice(-7)`; lit dots per column =
  `sparkLevels(values, 6)` (`design/DotSpark.tsx:29-33`): `top = max(0, …values)`; if `top ≤ 0` all 0; else `0` for
  `v ≤ 0`, otherwise `max(1, round(min(v, top) / top * 6))` (any positive value lights ≥1 dot; the max day lights all
  6). Dots light bottom-up. Today's (last) column's lit dots are highlighted. If `daily` has fewer than 7 entries, fewer
  columns are drawn (the last is still "today").
* Day initials under the columns: `S M T W T F S` by weekday (`weekdayInitial`; `·` for an unparsable key); today's
  initial emphasised; tooltip per day `{weekday name} · {formatDuration(listenedSec)}` e.g. `Monday · 1h 5m`.
* Accessible description: `Listening per day. Monday: 5 minutes, Tuesday: 0 seconds, …` (`weekdayName` +
  `durationSpeech`, joined by `, `).
* Footer: bolt icon (highlighted when `streak.current > 0`), the number `streak.current` (plain integer, not
  locale-formatted), `day streak` (never singularised), and `best {streak.longest}`.

### 3.9 Voice card (`home/VoiceCard.tsx`)

The whole card is one button → navigate to **Voices**.
* Top line: `{modelLabel(settings.modelId, models)} · {rateLabel(settings.playbackRate)}`, e.g. `Flash v2.5 · 1×`
  (models list from §3.1; `modelLabel(id, models)` and `rateLabel` are in §11.1 — not the History `modelLabel` of
  §11.3).
* Centre: the voice monogram, decorative (`design/dotfont.ts:52-66`): Unicode-NFD-normalise the name, strip
  combining marks (`\p{M}`), take the **first ASCII letter or digit** (`[A-Za-z0-9]`), upper-case it, and draw it on a
  7×7 dot matrix; a name with no such character gets a small waveform glyph instead. `No voice` → `N`, `Élodie` → `E`,
  `李` → waveform.
* Bottom: `settings.voiceName || "No voice"` (tooltip: same).
* `rate = settings.playbackRate ?? 1`; before settings load the top line is empty.
* Accessible name: `Voice: {name}. {model}, speed {round(rate*100)/100} times. Change voice` (before settings load:
  `Change voice`). Numbers print in JavaScript's shortest form (`1`, `1.25`, `0.9`), which Rust's `{}` for `f64`
  reproduces.

### 3.10 Recent card (`home/RecentCard.tsx`)

* Card accessible name `Recent reads`. Header: history glyph, `Recent`, ghost button `All history` (with right arrow) →
  navigate History.
* `recent == null`: 4 placeholder rows.
* `recent == []`: empty state, book glyph, title `No reads yet`, body
  `Select text anywhere, right-click and choose “Read with Vox”.`
* Otherwise one row per read (max 5, newest first):
  * Play button. "Live" = this read is in the player **and** status is `playing`.
    * live → label `Pause`, shows a level animation, click sends `{type:'pause'}`;
    * not live → label `Replay: {truncate(text, 60)}`, play icon, click → `replayRead(id)`; while that call is pending
      the button shows a loader and is disabled (per row; a second row's replay supersedes the pending marker).
    * Note: a **paused** read is not "live", so its button replays (restarts) instead of resuming.
  * Title: `truncate(text, 140)`.
  * Meta: `{sourceApp} · {voiceName} · {formatRelative(createdAt, now)}` (missing parts dropped); `now` ticks every
    60 s.
  * Right side, in order: a filled star if favourite (name `Favorite`); `Cached` (tooltip
    `Audio cached: replays are free`) if `cached`; then either `Failed` with alert icon (tooltip = the error) when
    `error` is set **and** both `audioDurationSec` and `listenedSec` are 0, or the duration
    `formatClock(audioDurationSec || listenedSec)` (so `0:00` for a read with neither and no error). Note this
    deliberately differs from History, whose `Failed` badge shows for *any* error (§4.7).

### 3.11 Ways to read card (`home/WaysCard.tsx`)

Card accessible name `Ways to read`. Header: cursor glyph, `Ways to read`. Three rows; each row is a button (with a
trailing chevron) → `settings/{section}`; a row shows an `Off` tag (and dimmed state) when its feature is disabled. The
row's glyph is the row's icon (cursor, keyboard, globe).

| Title | Body | Section | Off when |
|---|---|---|---|
| `Right-click chip` | `Select text, right-click, then pick “Read with Vox” beside the menu.` | triggers | `!triggers.rightClickChip` (not off before settings load) |
| `Hotkeys` | if hotkeys enabled: `Selection` [readSelection key caps] `Clipboard` [readClipboard key caps] (empty accelerator → `Not set`); else `Global shortcuts are turned off.` | triggers | `!triggers.hotkeysEnabled` (also "off" before settings load) |
| `Browser extension` | `Adds “Read with Vox” to the Chrome and Edge right-click menu.` | integrations | `!triggers.browserExtension` |

---

## 4. History (`pages/History.tsx`, `HistoryData.ts`, `HistoryModel.ts`, `HistoryRow.tsx`)

### 4.1 Page state

| State | Initial | Notes |
|---|---|---|
| `search` (raw field text) | `""` | |
| `view` | `all` | `all` \| `favorites` |
| `source` | `""` (all) | a `ReadSource` |
| `expanded` | ∅ | set of record ids whose details are open |
| `confirmClear`, `clearing`, `exporting` | false | |

Effective filters: `search = debounced(search.trim(), 220 ms)` **except** that an empty trimmed field applies
immediately (so clearing is instant); `favoritesOnly = view == 'favorites'`; `source`. `filtered = search != "" ||
favoritesOnly || source != ""`. All state resets when leaving the page (§2.4).

Debounce exactly as implemented (`History.tsx:69-72`, `HistoryData.ts:276-283`): `settled` = the trimmed text once it
has stayed unchanged for 220 ms; `effective = trimmed == "" ? "" : settled`. Quirk: after clearing, `settled` still holds
the old query for 220 ms, so typing a new query within that window briefly re-applies the *old* query until the new one
settles. A port may instead reset the debounced value to `""` on clear.

History keeps its **own** subscriptions to settings and player state (`lib/hooks.ts` `useSettings`,
`usePlayerState`) rather than the app store's; the player starts as `IDLE_PLAYER_STATE` until `getState()` answers.
Behaviourally identical; a port can use the shared store.

### 4.2 List loading (`useHistoryList`, `HistoryData.ts:68-202`)

Constants: `PAGE_SIZE = 50`; `REFRESH_DEBOUNCE_MS = 700`; `MAX_LIST_PAGE = 500` (main's clamp).

State: `items`, `total` (records matching filters), `grandTotal` (all records, ignoring filters), `status`
(`loading`/`ready`/`error`; `loading` also when filters changed while old items are still shown), `error`, `loadingMore`.
Internal: `wanted` (how many rows to keep loaded, starts at 50), request id counter, `refreshing` flag, and two id sets:
`pending` (locally removed, still in main during the undo window) and `deleted` (main deleted them, but they may still
appear in a response already in flight).

**Fetch cycle** (runs on filter change, history version bump, or forced reload):
1. Filters changed → `wanted = 50`; `status = loading` (keep current items; they are shown dimmed). Fetch immediately.
2. Forced reload (`reload()` or after an undo) → fetch immediately.
3. History version bumped → if `status == ready`, wait 700 ms (trailing debounce; a newer trigger restarts it);
   otherwise fetch immediately.
4. Fetch `fetchWindow(query, max(50, wanted))`: pages of ≤500 from offset 0 until `wanted` rows or a short page
   (`HistoryModel.ts:115-130`); dedupe by id; `total` = last page's total. In parallel, **if filtered**, fetch
   `history.list({limit:1})` to learn the unfiltered total.
5. Drop stale responses (request id). Remove rows in `pending` or `deleted`.
   `total = max(0, page.total − (number of rows removed from this window))`;
   `grandTotal = filtered ? max(0, all.total − |pending|) : total`.
6. Reconcile: unchanged records keep their previous objects (field-by-field equality; `listenLog` compared by value),
   and if nothing changed at all the old list is kept, so rows don't redraw (`reconcileRecords`). If items, totals and
   flags are all unchanged, the state isn't touched.
7. `status = ready`, `error = null`, `loadingMore = false`.
8. Failure → `status = error`, `error = message`, `loadingMore = false`, items kept.

Request bookkeeping (`HistoryData.ts:87-160`):
* One request-id counter is shared by refreshes and load-more. A refresh that starts while a load-more is in flight
  **supersedes it**: the load-more response is dropped, and the refresh fetches the enlarged window because `wanted` was
  already raised to `offset + 50`. The refresh's completion clears `loadingMore`.
* `refreshing` is set when a refresh request starts and cleared only when the **latest** refresh settles (success or
  failure); load-more is refused while it is set.
* The "status was ready?" test for the 700 ms debounce uses the status at the moment of the trigger; filter changes and
  forced reloads are always immediate.
* The page's own actions (delete commit, favourite toggle, clear) make main emit `history:changed`, so each also causes a
  debounced refetch.
* Leaving the page bumps the request id so in-flight responses are ignored.

**Load more** (`loadMore`): ignored while a refresh is in flight, while already loading more, unless `ready`, or when
`items.length ≥ total`. Request `{...filters, offset: items.length, limit: 50}`; set `wanted = offset + 50`. On
success: drop pending/deleted rows, append only unseen ids (`appendUnique`), `total = max(items.length, page.total −
|pending|)`. On failure: `loadingMore = false`, `error = message` (status stays `ready`). A successful load-more
does **not** clear an earlier `error`; only the next successful refresh does. Reason for
`offset = items.length`: locally removed rows may still hold slots in main's list, so a few loaded rows may be re-read
(dropped as duplicates) but none is ever skipped.

**Local edits**:
* `patchItem(id, patch)` — optimistic field update (favourite).
* `removeItem(id)` — add to `pending` (always); if the row is loaded, remove it from `items` and do `total−1`,
  `grandTotal−1` (both floored at 0); if it is not loaded, nothing else changes.
* `restoreItems(records)` — for each record still in `pending`: remove from `pending`, re-insert at its date position
  (before the first item with an older `createdAt`, `insertByDate`), `total+n`, `grandTotal+n`; then force a refetch
  (a restored row may no longer match filters changed meanwhile).
* `confirmRemoved(ids)` — move ids from `pending` to `deleted`.

### 4.3 Header

* Eyebrow / count label (announced politely):
  * `Loading` — while `status == loading` and `grandTotal == 0`;
  * filtered → `{formatNumber(total)} of {formatNumber(grandTotal)} reads` (always plural);
  * else `{formatNumber(grandTotal)} read` / `reads`.
* Title `History`.
* Actions (hidden entirely in the "nothing yet" state, §4.5):
  * `Export` (download icon; disabled when `grandTotal == 0`; spinner while exporting): first **flush pending
    deletes** (§4.10) so just-removed reads are not in the file; then `history.exportJson()` (main shows a save dialog).
    Path returned → success toast, title `History exported`, message = the path. `null` (cancelled) → nothing. Error →
    `Export failed: {message}`.
  * `Clear all` (trash icon; ghost; disabled when `grandTotal == 0`; danger colour only on hover) → opens the confirm
    dialog (§4.11).

### 4.4 Toolbar (hidden in the "nothing yet" state)

Search region, name `Filter history`:
* Search field: name `Search history`, placeholder `Search text, apps, voices…`. A clear button (`Clear search`)
  appears when non-empty; clicking it clears the field and puts focus back in it. `Escape` in the field clears it if
  non-empty (and is consumed so it doesn't close anything else); on an empty field Escape is not consumed.
* Segmented control `Show` (a radio group): `All` | `Favorites` (star glyph). `ArrowRight`/`ArrowDown` select the next
  option, `ArrowLeft`/`ArrowUp` the previous, **wrapping around**; `Home`/`End` the first/last; selection moves focus
  with it, and only the selected option is in the Tab order (`design/Segmented.tsx:60-83`).
* Select `Filter by source`: `All sources`, then in this order: `Right-click chip`, `Hotkey`, `Selection popup`,
  `Browser extension`, `Clipboard`, `Typed in app`, `File Explorer`, `Replay`, `Command line` (`SOURCE_ORDER`,
  `charts/labels.ts:31-41`).

### 4.5 Body states (decision order, `History.tsx:232-353`)

| # | Condition | Body |
|---|---|---|
| 1 | `status == loading` and no items | 4 placeholder rows |
| 2 | `status == error` and no items | alert glyph, `Couldn’t load your history`, body = error message, primary `Try again` → reload |
| 3 | "nothing yet": `status == ready`, `grandTotal == 0`, no items | history glyph, title `Nothing read yet`, body `Select text in any app, right-click and choose “Read with Vox”` + (if a read-selection hotkey: ` — or press ` [key caps]) + `. Every read lands here.`; primary button `Paste & read` → navigate Home (it sets the route to `#/home` directly; Quick read is not focused). Header actions and toolbar hidden. |
| 4 | `status == ready`, no items (filters exclude everything) | see below; button `Clear filters` resets search (to "", applied at once), view `all`, source all |
| 5 | otherwise | the grouped list (dimmed and busy while `status == loading`) + footer |

State 4 copy, by priority:

| Active filter | Glyph | Title | Body |
|---|---|---|---|
| search text | search | `No matches` | `Nothing matches “{search}”` + (` with these filters` if favourites or a source is also active) + `.` |
| favourites | star | `No favorites yet` | `Star a read to keep it here.` |
| otherwise (source) | cursor | `No reads here` | `No reads came from {sourceLabel(source)} yet.` (if no source is set, which can happen transiently: `No reads came from this source yet.`) |

**Background errors**: when an error occurs while rows are on screen (refresh or load-more failed), show an error toast
`Couldn’t load history: {error}` (id `history-load`, 6500 ms) with action `Retry` → reload. It is (re)shown whenever
the `error` value changes to a non-null value while rows exist, or when rows appear while an error is still set; the
same error text arriving again does not re-toast (`History.tsx:109-113`). In state 5 with `status == error`, the old
rows stay on screen (not dimmed) and the toast is the only signal.

### 4.6 Day groups (`groupByDay`, `HistoryModel.ts:44-57`)

Records (newest first) are split into runs of the same local calendar day, order preserved. Each group:
* `label = dayLabel(createdAt, now)` (§11.2): `Today`, `Yesterday`, weekday name (`Monday`) within the last 7 calendar
  days, then `Mon, Sep 14` (this year) or `Mon, Sep 14, 2025` (other years). Future dates count as `Today`.
* Header meta: `{n} read|reads` + (` · {shortDuration(Σ max(0, listenedSec))}` when > 0), e.g. `4 reads · 12m`. `n` is
  a plain integer (not locale-formatted). (Wall-clock listening, not audio length.) Only *loaded* rows count, so the
  oldest group can grow after `Load more`.
* Each group is a labelled region named by its label.
* `now` ticks every 60 s, so labels move at midnight.

### 4.7 Row, collapsed (`HistoryRow.tsx:58-175`)

Main toggle button (expands/collapses; `aria-expanded`):
* Source icon (tooltip `sourceLabel(source)`; icons per source in §6.2).
* Text: `truncate(text, 320)` or `Empty read`; shown 2 lines max (1 line while expanded, since the full text is below).
* Meta line (items separated by ` · `):
  1. app = `sourceApp || sourceLabel(source)`;
  2. `voiceName || "Unknown voice"`;
  3. audio length `formatClock(len)`, prefixed `~` when estimated (`audioLength`: `audioDurationSec` if > 0, else
     `round(charCount / 15)` estimated — `ESTIMATED_CHARS_PER_SECOND = 15`);
  4. time: `formatRelative(createdAt, now)` if `now − createdAt < 6 × 86 400 000` ms, else time of day (`9:05 PM`);
     tooltip = full date-time (`Friday, October 17, 2026 at 9:05 PM`);
  5. filled star (name `Favorite`) if favourite (hidden in the narrow layout);
  6. badges: `Playing` (this read is in the player and status playing **or loading**), `Paused` (in the player and
     paused), `Failed` (if `error` is set — any error, even a partially played read), `Free replay` (if `cached` and
     the read is not in the player).
* Progress: a small bar of `listenedFraction(r)` and the text `{pct}%` (emphasised at 100 %), accessible
  `Listened` / `{pct}% listened`.
  `listenedFraction` (`HistoryModel.ts:64-71`): completed → 1; `audioDurationSec ≤ 0` → 0; else
  `min(0.99, max(0, reached / audioDurationSec))` with `reached = maxPositionSec` if present and > 0, else
  `listenedSec`. Capped at 99 % so 100 % means "played to the end".

Secondary actions (revealed on row hover / keyboard focus inside the row / while expanded, taking the place of the
progress column; always visible on touch-only devices and in the narrow layout):

| Action | Label / tooltip | Effect |
|---|---|---|
| Delete | label `Delete from history`, tooltip `Delete` | §4.10 |
| Copy | `Copy text` | copy `r.text` to the clipboard → success toast `Copied the saved preview` if `textTruncated` else `Text copied` (id `history-copy`); failure → `Couldn’t copy to the clipboard` |
| Favourite | label `Favorite` (toggle, pressed when favourite); tooltip `Remove from favorites` / `Add to favorites` | optimistic flip → `history.toggleFavorite(id)` → apply the server's `favorite` if a record comes back (a `null` result leaves the optimistic value); on failure revert and toast `Couldn’t update favorites: {message}` (no toast id, so repeated failures stack) |

Replay (always visible, last): label `Replay` or `Replay (free)` when cached. `canReplay = cached || !textTruncated`
(without cached audio main would re-synthesize the stored text; a ~160-char preview is not worth paying for, so main
refuses). Tooltip: cached → `Free replay from saved audio`; else if `canReplay` → `Replay · uses credits`; else
`Audio no longer cached and full text wasn’t saved`. When `!canReplay` the button stays focusable (so keyboard users get
the reason) but does nothing. Click → `replayRead(id)` (main toasts its own failures; §1.6 adds the rest).

Row "in the player" rule (`History.tsx:226-230`): `player.readId == r.id` and status `playing`/`loading` → playing;
`paused` → paused; otherwise none.

### 4.8 Row, expanded details (`HistoryRow.tsx:177-228`, region `Read details`)

* Full text in a scrollable area (max ~260 px tall; name `Full text`).
* Note if `textTruncated`: `Only a short preview was saved for this read (full text storage is off).`
* Error note (if `error`): the error text.
* Facts: `Chars` = `formatNumber(charCount)`; `Words` = `formatNumber(wordCount)`; `Credits` =
  `formatNumber(charactersBilled)`; `Heard` = `{pct}%` (same `pct` as the collapsed row).
* Info list:
  * `Source`: source icon + `sourceLabel(source)` + (` · {sourceApp}` if any).
  * `Window`: `sourceTitle` (single line, ellipsised; tooltip full) — only if present.
  * `Page`: only if `sourceUrl` matches `^https?://` (case-insensitive): a link showing `displayUrl(url)` (hostname
    without leading `www.` + path, path omitted when `/`; query/fragment dropped; raw string if unparsable) with an
    external-link icon; tooltip full URL; click → `app.openExternal(url)`, failure toast
    `Couldn’t open the link: {message}`.
  * `Voice`: `{voiceName || "Unknown"} · {modelLabel(modelId)}` (the *History* model label, §11.3, e.g.
    `Eleven Flash v2.5`).
  * `Audio`: (`about ` if estimated) + `formatClock(len)` + (` · saved for free replay` if cached).
  * `Read`: full date-time of `createdAt`.

### 4.9 Footer (`History.tsx:337-350`)

* If `items.length < total`: `Showing {formatNumber(items.length)} of {formatNumber(total)}` and a `Load more` button
  (spinner while loading more; disabled unless `status == ready`).
* Else if more than 8 items are shown: a dotted divider labelled `End of history`.
* Else nothing.

### 4.10 Delete with undo (`useUndoableDelete`, `HistoryData.ts:204-273`)

Toast id `UNDO_TOAST_ID = "history-remove"`.

1. **Delete click**: if keyboard focus is currently inside this row (true after a mouse click too, since clicking a
   button focuses it), first move focus to the next row's toggle (or the previous one if it was last; nowhere if it was
   the only row) so focus stays in the list; collapse the row if expanded; add the record to the undo batch (a map keyed
   by id); `removeItem` (row disappears, counts drop).
2. Show/replace the success toast (id `history-remove`): `Removed from history` when the batch has 1 record, else
   `{n} reads removed from history`; action `Undo`. Duration 6000 ms (action rule, §1.7); each new delete replaces the
   toast and restarts its timer, so the undo window extends and the count grows.
3. **Commit** happens when the undo toast is gone from the stack for any reason — timed out (plus its 180 ms exit),
   closed, pushed out by 4 newer toasts — or when the page is left (unmount), or on `flush()`. Mechanically: on *every*
   toast-stack change, if the batch is non-empty and no toast with id `history-remove` is in the stack, commit
   (`HistoryData.ts:242-252`). Because a same-id push replaces atomically (§1.7), deleting another row while the toast
   shows never commits early. Commit takes the whole batch, clears it, and calls `history.remove(id)` for each record
   **in parallel** (main also deletes cached audio). Each success → `confirmRemoved([id])`. Each failure →
   `restoreItems([record])` and error toast `Couldn’t delete: {message}` (id `history-remove-error`, so several failures
   collapse into one toast).
4. **Undo** (toast action): take the batch, clear it, `restoreItems(batch)`; the toast then leaves (and since the batch
   is empty, nothing is committed).
5. `flush()` = commit now + dismiss the undo toast (immediate removal); used before Export and Clear all. Leaving the
   page does the same (commit, then dismiss).

Port note: model "toast removed (id, reason)" as an explicit event; the commit logic subscribes to it.

### 4.11 Clear all dialog

Modal, danger tone, trash glyph, small:
* Title `Clear all history?`
* Description `All {formatNumber(grandTotal)} read|reads will be removed from this computer. This can’t be undone.`
* Buttons: `Cancel` (initial focus; disabled while clearing) and `Clear history` (danger; spinner while clearing).
* `Escape`, backdrop click (press and release both on the backdrop, so a text selection dragged outside does not close
  it) and the dialog's close button (label `Close`) call close, which is ignored while clearing.
* Modal behaviour (`design/Dialog.tsx`): the rest of the window is inert; `Tab`/`Shift+Tab` wrap within the dialog;
  closing plays a 180 ms exit before the dialog disappears (and it still counts as "open" for §4.12 during that time).
* Clear: flush pending deletes; `history.clear()`; success → close, collapse all rows, toast `History cleared`, reload;
  failure → toast `Couldn’t clear history: {message}` (dialog stays open).

### 4.12 Keyboard (History)

| Input | Effect |
|---|---|
| `Ctrl+F` (no Alt/Shift/Meta; `f` or `F`) | focus the search field and select its text — works even while typing in another field; not while a modal dialog is open |
| `/` (no Ctrl/Alt/Meta) while focus is not in a text input, text area, select or editable text | same (the `/` is not typed) |
| `ArrowDown` / `ArrowUp` on a row toggle | focus next / previous row toggle (clamped, no wrap) |
| `Home` / `End` on a row toggle | first / last row toggle |
| `Enter` / `Space` on a row toggle | expand / collapse |
| `Escape` in search | clear search |

### 4.13 Responsive (History)

Content area narrower than 760 px: the progress bar moves under the text; the secondary actions are always visible
inline; the favourite star in the meta line is hidden; details stack in one column. On touch-only devices
(`hover: none`) the secondary actions permanently replace the progress column.

---

## 5. Analytics (`pages/Analytics.tsx`, `charts/*`)

### 5.1 Range selector and persistence

| Value | Button | Accessible label | `RANGE_LABELS` |
|---|---|---|---|
| `7d` | `7D` | `Last 7 days` | `Last 7 days` |
| `30d` | `30D` | `Last 30 days` | `Last 30 days` |
| `90d` | `90D` | `Last 90 days` | `Last 90 days` |
| `365d` | `1Y` | `Last 12 months` | `Last 12 months` |
| `all` | `ALL` | `All time` | `All time` |

Segmented control named `Time range`. The choice is persisted per user in browser local storage key
`vox.analytics.range` (any read/write failure ignored); an absent/invalid value → default `30d`. Port: a small UI-state
file (not the settings schema).

### 5.2 Loading (`useToday`, `useAnalytics`)

* `today` = local day key; a timer fires at the next local midnight + 1000 ms, and it is also re-checked on window
  focus / visibility (a timer can sleep through midnight with the PC). These re-checks are **not** throttled (unlike the
  30 s wake counter). The timer is re-armed only when `today` actually changes; if it fires and the day key is the same
  (clock changed backwards), it is not re-armed until the next focus re-check finds a new day (`Analytics.tsx:78-94`).
  Port: always re-arm after firing.
* Fetch `analytics.summary(range)` whenever range, history version, a retry nonce, `today`, or the freshness key change.
  **No debounce** (unlike Home/History) — every `history:changed` refetches, so during playback the dashboard dims and
  refetches about every 2 s (see §12).
* Initial state `{summary:null, loading:true, error:null}`. Each fetch sets `loading=true` first (the error is *not*
  cleared). While loading, the previous summary stays and the grid is dimmed/busy. Only the latest request may apply
  (request id; leaving the page invalidates in-flight requests). Success → `{summary, loading:false, error:null}`;
  failure → keep summary, `loading=false`, `error=message`. Consequence: after a first-load failure, pressing
  `Try again` leaves the error card on screen (no spinner) until the retry answers.

### 5.3 Header and page states

* Eyebrow: the range label; plus ` · Couldn’t refresh` (warning tone) when a summary is shown, not loading, and the last
  fetch failed. Title `Analytics`. Range selector on the right.
* No summary and an error → card: alert glyph, `Couldn’t load analytics`, body = error, primary `Try again` (retry).
* No summary yet → skeleton (hero, 4 KPI tiles, two chart blocks).
* Otherwise the dashboard (§5.4–5.11), in this order: [empty banner] · Listening time (hero) · Words read · Reads ·
  Streak · Pace · Daily listening (wide) · Credits (narrow) · When you listen (wide) · How you start reads (narrow) · Top
  apps · Top voices.

Responsive: under 860 px content width the hero and every chart card take the full width and KPI tiles go two per row.

### 5.4 Derived values (`Analytics.tsx:242-250`)

```text
t          = summary.totals
days       = activeDays(summary.daily)     // active = #days with reads>0 OR listenedSec>0; total = daily.length
finishedPct= t.reads>0 ? round(t.completedReads / t.reads * 100) : 0
lastWeek   = summary.daily.slice(-7)
avgActive  = days.active>0 ? t.listenedSec / days.active : 0
peakDay    = first day with the strictly largest listenedSec (> 0); none if all are 0
peak cell  = heatCells(heatmap).peak     // §5.9
empty      = t.reads == 0
```

### 5.5 Empty banner (only when `t.reads == 0`)

Chart glyph; title `Nothing listened yet` for `all`, otherwise `Nothing listened in the {range label lower-cased}`
(e.g. `Nothing listened in the last 30 days`); body
`Select text anywhere, right-click and choose “Read with Vox”. Your stats fill in as you listen.` The rest of the
dashboard still renders (zeros, chart empty notes).

### 5.6 Listening time (hero)

* Header: headphones glyph, `Listening time`; right: `Today` + `formatHMDot(summary.today.listenedSec)` (screen reader:
  `speakDuration`).
* Meter (3 dot rows) of `days.active / days.total` (0 if no days): name `Days with listening`, value text
  `{active} of {total} days`; caption `Days active` and `{active}/{total}`.
* Big value `formatHMDot(t.listenedSec)` (e.g. `42H18M`; screen reader `speakDuration`).
* Aside: `{shortDuration(t.audioSec)} of audio generated`.

### 5.7 KPI tiles

Each tile: icon + label, optional "aside" mini-visual at the top right, a big value (optional unit word after it) and a
sub line.

| Label (icon) | Value | Unit | Sub line | Aside |
|---|---|---|---|---|
| `Words read` (quote) | `formatCompact(t.words)` (screen reader `{formatNumber} words`) | — | reads>0: `{formatNumber(avgWordsPerRead)} per read on average`; else `Across every read` | spark of `lastWeek.words`, name `Words read, last 7 days` |
| `Reads` (play) | `formatNumber(t.reads)` | — | reads>0: `{finishedPct}% listened to the end`; else `No reads yet` | spark of `lastWeek.reads`, name `Reads, last 7 days` |
| `Streak` (bolt) | `streak.current` | `day` / `days` | `Longest {longest} day` / `days` | 7 dots = `recentActivity(daily, 7)` (active = reads>0 or listenedSec>0), oldest → newest, last = today |
| `Pace` (gauge) | `round(avgListenWpm)` (0 if not finite) | `wpm` | `readingTimeEquivalentSec>0`: `≈ {shortDuration(readingTimeEquivalentSec)} of silent reading`; else `Words per minute heard` | — |

"Words read" counts every word of every read in the range, heard or not (code comment). Spark: 4 rows of dots per
column, lit = `sparkLevels(values, 4)` (§3.8 rule), last column highlighted, other lit columns in a dimmer tone; one
column per entry of `lastWeek`. The streak is all-time (main), while the dots come from the range's `daily` (fewer than
7 dots if `all` covers fewer than 7 days). The streak dots are decorative (hidden from screen readers); the Pace tile
has no aside. The `Reads` value has no separate spoken label (the visible number is read).

### 5.8 Daily listening chart (`charts/DailyColumns.tsx`, `charts/model.ts`)

Card header: chart glyph, `Daily listening`; right: `Avg {formatHMDot(avgActive)}` and, if a peak day exists,
`Peak {formatHMDot(peakDay.listenedSec)}` (both with spoken equivalents).

**Bucketing** — `bucketDaily(daily, maxColumns, now)` with `maxColumns = floor(plotWidth / MIN_COL_PITCH)`,
`MIN_COL_PITCH = 6` px (before the width is known: `daily.length`):

1. `daily.length ≤ max` → one column per **day** (`unit = day`).
2. else if `ceil(len/7) ≤ max` → **weeks**: walk *backwards* from the newest day in chunks of 7, so the newest column is
   always a full 7-day week ending today; the oldest column may be partial (`days < 7`).
3. else → calendar **months** (`YYYY-MM` of each day; start = first day present, end = last day present).

Each bucket: `key`, `start`, `end` (local midnights, inclusive), `days`, sums of `listenedSec`, `reads`, `words`, and
`isToday` (contains today's key). Example: 365 days at 640 px → max 106 → 53 weekly columns (oldest has 1 day); at 300 px
→ max 50 → months.

**Geometry** — `columnGeometry(n, width, height = 168, maxDotPitch = 11)`:

```text
colPitch  = n>0 && width>0 ? width/n : 0
dotPitch  = clamp(min(11, colPitch), 3, 11)
dot       = round(dotPitch*0.72, 1 decimal)                 // dot diameter
perColumn = colPitch>0 ? clamp(floor(colPitch*0.62/dotPitch), 1, 6) : 1   // wide columns become dot bars
rows      = max(1, floor((height - dot)/dotPitch) + 1)
column centre x(i) = i*colPitch + colPitch/2 ; dot row y(r) = height - dot/2 - r*dotPitch (r=0 is the bottom)
```

Plot height 168 px + 22 px axis band.

**Levels** — `dotLevels(values, rows, max?)`: `top = max ?? max(0, …values)`; if `top ≤ 0` (or NaN) or `rows ≤ 0`
all 0; else each `v > 0` → `clamp(round(min(v, top)/top*rows), 1, rows)`, `v ≤ 0` → 0. Example `[0,100,50,1]`, rows 15
→ `[0,15,8,1]`. Lit dots fill bottom-up (row `r` is lit when `r < level`). Each column draws `perColumn` dot
sub-columns at x offsets `(j − (perColumn−1)/2) · dotPitch` from the column centre, all with the same level. The today
column's lit dots use the accent colour; in the today column every **unlit bottom-row** dot is drawn as a "now" marker
(so an empty today still shows where today is).

`isToday` is computed from the clock at bucketing time; the buckets are recomputed when `today` changes (§5.2), which
moves the highlight at midnight. Before the plot width is known, nothing is drawn (but the empty note can show).

**Axis labels** — `pickTicks(n, cap)`, `cap = width / (unit==day && n≤7 ? 40 : 64)`: `step = max(1, ceil(n / max(1,
floor(cap))))`; indices `n−1, n−1−step, …` (anchored at the newest column). Label text `axisLabel(bucket, unit, n)`:
* month: `JAN`…`DEC`; January shows the 2-digit year, `JAN 26`;
* day with `n ≤ 7`: weekday `SUN`…`SAT`;
* otherwise `{MON} {day}` of the bucket start, e.g. `SEP 14`.
A label within 24 px of the left/right edge is anchored to that edge (text starts at x = 0 / ends at x = width), else
centred on the column; baseline at `plotHeight + 17`. Today's label is emphasised. Day numbers are not zero-padded
(`SEP 4`).

**Interaction**
* Pointer move → active column = `clamp(floor((x − left)/colPitch), 0, n−1)` (`left` = the plot's left edge); a
  highlight band is drawn behind it: rect `x = i·colPitch + 1`, `y = −4`, `w = max(2, colPitch − 2)`,
  `h = plotHeight + 8`, corner radius `min(8, colPitch/2)`. Pointer leave clears it unless the chart is in
  keyboard mode.
* Keyboard (chart is focusable): on *keyboard* focus (not mouse click) enter keyboard mode and select the active column
  or the newest. Keys move from the active column, or from the newest if none: `ArrowLeft` −1, `ArrowRight` +1,
  `PageUp` **−7** (older), `PageDown` **+7** (newer), `Home` first, `End` newest, all clamped. Any of these keys also
  enters keyboard mode, even after a mouse-click focus. `Escape` clears the selection but stays in keyboard mode; blur
  clears both (`DailyColumns.tsx:64-76,146-155`).
* Tooltip (§5.13, side placement, anchored at the column centre and the **top** of the plot (`y = 0`), offset =
  `barHalf + 10` with `barHalf = ((perColumn − 1)·dotPitch + dot)/2`): title `bucketTitle` (day `Tue, Sep 30`;
  week `Sep 8 – Sep 14`, or a single date if the bucket has 1 day; month `September 2026`); value
  `formatHMDot(listenedSec)`; sub `{formatNumber(reads)} read|reads · {formatNumber(words)} words` (words always
  plural).
* Screen-reader live text while in keyboard mode: `{bucketTitle}: {shortDuration}, {reads} reads` (`reads` is the raw
  integer, always "reads").
* If every bucket is ≤ 0: overlay note `No listening in this range yet`.
* Accessible summary (`dailySummary`, computed from the raw `daily` series, not the buckets): none active →
  `Daily listening, {range lower}: nothing listened yet.`; else
  `Daily listening, {range lower}: {speakDuration(total)} over {k} active day|days. Busiest day {Tue, Sep 30} with
  {speakDuration(peak)}.` (busiest = first maximum). Here an "active day" means `listenedSec > 0` only — unlike
  `activeDays` (§5.4), which also counts days with reads but no listening.
* Data table twin: caption `Listening per {day|week|month}, {range lower}`, columns `Day|Week|Month`, `Listened`,
  `Reads`, `Words`; rows `[bucketTitle, shortDuration, reads, words]` (counts as raw integers). The first cell of each
  row is its row header.

### 5.9 When you listen (heatmap) (`charts/Heatmap.tsx`)

Header: clock glyph, `When you listen`; right (only if a peak exists): `Peak {Mon} {HH:00}` — first 3 letters of the
Monday-first weekday name and the start of the hour range (e.g. `Peak Sat 21:00`).

Data: `heatmap[day 0=Mon..6=Sun][hour 0..23]` seconds. `heatCells` normalises missing/negative/NaN cells to 0, computes
`max`, `total`, per-cell `level = sqrt(value/max)` (area-true), and `peak` = the first cell (Mon→Sun, 0→23) with the
strictly largest positive value.

Geometry (`Heatmap.tsx:30-39`):
```text
LABEL_W = 40, AXIS_H = 22, MAX_PITCH = 30
pitch  = clamp((width - 40)/24, 8, 30); gridW = 24*pitch; x0 = 40 + max(0, (width - 40 - gridW)/2)   // grid centred
height = 22 + 7*pitch;  maxR = 0.42*pitch;  minR = max(1.2, 0.09*pitch)
cell centre: x = x0 + hour*pitch + pitch/2 ; y = 22 + day*pitch + pitch/2
radius = value>0 ? minR + (maxR-minR)*level : minR ; opacity (value>0) = 0.28 + 0.72*level ; empty cells drawn "off"
hour labels every 3 h if 3*pitch ≥ 34 else every 6 h, text "00","03",… centred on the hour's column, baseline y = 13
day labels MON…SUN left-aligned at x = 0, baseline at the row centre + 4 (today's weekday emphasised)
hit test: hour = floor((px - x0)/pitch), day = floor((py - 22)/pitch); outside 0..23 / 0..6 → no cell
```
* A **now ring** (accent) circles the current weekday/hour cell, radius `maxR + 2.5` (refreshed every 60 s).
* Hover → active cell under the pointer (outside the grid clears unless in keyboard mode); a ring of radius
  `maxR + 2.5` marks it.
* Keyboard: on keyboard focus (not a mouse click) enter keyboard mode and select the active cell, else the peak cell,
  else the now cell. Keys move from the active cell, or **from the now cell** when nothing is active (e.g. after
  `Escape`): `ArrowLeft`/`ArrowRight` hour −1/+1, `ArrowUp`/`ArrowDown` day −1/+1 (up = towards Monday), `Home`/`End`
  hour 0/23 (same day), all clamped; any of these keys enters keyboard mode. `Escape` clears the selection; blur clears
  selection and keyboard mode (`Heatmap.tsx:55-110`).
* Tooltip: title `{Mon} · {HH:00–HH:00}` (`hourRange`, en dash, `24` wraps to `00`); value `formatHMDot(value)`; sub
  `Busiest hour` when it is the peak cell. Placement **below** the cell for Mon–Wed (day < 3), anchored at
  `cellY + maxR + 8`; **above** otherwise, anchored at `cellY − maxR − 8`; x = cell centre.
* Legend (decorative): `Less`, five sample dots at levels 0, .25, .5, .75, 1 (same radius/opacity formulas; level 0
  drawn as an empty cell), `More`; and a ring sample labelled `Now`. It is inset so it lines up with the first and last
  grid columns.
* Screen-reader: summary (`heatSummary`): no peak → `Listening by weekday and hour: no listening yet.`; else
  `Listening by weekday and hour. Busiest time: {Monday}s, {21:00–22:00}, {speakDuration}.`; live text
  `{Monday} {21:00–22:00}: {shortDuration}`; table caption `Listening by weekday and hour`, columns `Weekday`,
  `00:00`…`23:00`, rows of `shortDuration`.

### 5.10 How you start reads (`charts/SourcesMeter.tsx`)

Header: cursor glyph, `How you start reads`.
* Rows = sources with `reads > 0`, sorted by reads desc (stable: ties keep main's order), top **5**. `total` = sum of
  `max(0, reads)` over **all** sources (so shares of the shown rows may sum to < 100 %).
  `pct = total>0 ? round(reads/total*100) : 0`.
* Each row: source icon, `sourceLabel`, `formatNumber(reads)`, `{pct}%` (the first row emphasised), and a 2-row dot meter
  filled by `reads/total` (meter accessible name = the source label).
* Empty: `No reads in this range yet`.
* Accessible summary `Reads by source: Right-click chip 70 (28%), Hotkey 59 (24%), ….` (raw read counts); table caption
  `Reads by source`, columns `Source`, `Reads`, `Share`. Unknown source values show the raw value and the info icon
  (§6.2).

### 5.11 Top apps / Top voices (`charts/TopList.tsx`, `Analytics.tsx:176-186`)

Two cards: globe glyph `Top apps` (title `Top apps by listening time`, empty `No apps in this range yet`, key = name) and
voice glyph `Top voices` (title `Top voices by listening time`, empty `No voices in this range yet`, key = voiceId).

`topItems(rows)`: if **no** row has `listenedSec > 0`, rank by reads (value = reads, display = `String(reads)` — not
locale-formatted, text `{reads} reads` — always plural); otherwise by listening (value = listenedSec, display
`formatHMDot`, text `shortDuration`). Name = `name || "Unknown"`; sub = `{formatNumber(reads)} read|reads`. The order is
main's (already sorted by listening desc); the UI does not re-sort, even in reads mode.

`TopList`: keep rows with `value > 0` (so in listening mode, apps with reads but no listening drop out), first **6**;
`max = max(value)`. Each row: rank `01`…`06`, name (tooltip full) with the sub line, a horizontal dot bar with
`barDots(value, max, count)` lit (`0` if `max ≤ 0`, `value ≤ 0` or `count ≤ 0`, else
`clamp(round(value/max*count), 1, count)`; the first row's bar emphasised), and the display value. Summary
`{title}: {name} {text}, ….` / `{title}: no data yet.`; table caption = title, columns `Name`, `Value`, `Detail`
(`Value` = text, `Detail` = sub).

Bar dot count (`dotLayout(width, dot = 7, minGap = 3)`, `design/util.ts:27-31`): if `width ≤ 7` → `count = width > 0 ?
1 : 0`; else `count = max(2, floor((width + 3) / 10))` and `pitch = (width − 7)/(count − 1)` so the first and last dots
touch both ends of the bar.

App names come from main (`sourceApp`, or fallbacks: `Vox` for manual/replay, `Clipboard`, `Explorer`, `Command line`,
`Browser` for the extension, `Other`).

### 5.12 Credits card (Analytics variant) (`charts/CreditsCard.tsx`)

Narrow card next to Daily listening. On mount: `refreshCredits(false)` (main's 60 s cache answers most).

* Header: bolt glyph, `Credits`; right (only when a key exists, i.e. status ≠ `no-key`): tier badge when `ready`
  (`tierLabel`: `_`/`-` runs → space, capitalise each word, empty → `Plan`) and an icon button `Refresh balance`
  (→ `refreshCredits(true)`, disabled while `loading`).
* Body:

| Status | Content |
|---|---|
| `no-key` | `Add your ElevenLabs API key to see your credit balance.` + primary `Add API key` → `settings/account` (sets the route directly) |
| `loading` | placeholders |
| `error` | `Couldn’t reach ElevenLabs.` + `Retry` (forced refresh) |
| `restricted` (or ready without account) | `This API key can’t read account details, so the balance isn’t available.` |
| `ready` | CreditsSummary (§6.1), normal numeral size |

* Footer (always): the range label and `{formatCompact(t.charactersBilled)} billed`.
* Feedback gap: `Refresh balance` only disables while status is `loading`, and a forced refresh from `ready` or
  `restricted` keeps the old numbers without entering `loading` (§1.5). So in the common case the click shows no busy
  state at all; the numbers just change when the answer arrives.
* Tier badge: `ready` with an empty tier shows `Plan` here, whereas Home shows no badge (§3.6). `tierLabel` keeps
  whitespace as is and upper-cases any letter that starts a word (`\b\w`).

### 5.13 Chart tooltip placement (`charts/ChartTooltip.tsx`)

Anchor `(x, y)` relative to the chart; bubble width `w` measured; chart width `bounds`.
* `above`/`below`: centred on x, clamped: `left = min(max(4, x − w/2), max(4, bounds − w − 4))`; drawn above / below y.
* `side`: top-aligned at y; `left = x + offset` if it fits (`x + offset + w ≤ bounds`), else `x − offset − w`; never
  below 0. It never covers the mark.
* Invisible until measured (avoids a one-frame jump): before the width is known, `left = x` and the bubble is hidden;
  it is re-measured on every update. Content: title, a large value, optional sub. Decorative for screen readers (values
  are in the data tables).

### 5.14 Accessibility twins

Every chart exposes (a) a one-sentence summary as its accessible name, (b) a polite live text for the
keyboard-selected mark, and (c) a hidden data table with every value. In a native port, expose (a) as the element's
UIA Name, (c) as a UIA Table/Grid or a "View as table" toggle, and raise UIA notifications for (b).

---

## 6. Shared view components

### 6.1 CreditsSummary (`components/CreditsSummary.tsx`) — used on Home, Analytics (and Settings)

Input: `characterCount`, `characterLimit`, `nextResetUnix?`; numeral size (Home large, Analytics normal).

```text
left      = creditsLeftFraction(count, limit)      // 0 when limit ≤ 0 (!)
pct       = round(left*100)
remaining = max(0, limit - count)
reset     = resetLabel(nextResetUnix)               // or null
detail    = "{formatNumber(remaining)} of {formatNumber(limit)} characters left" + (reset ? ". {reset}" : "")
```

* Dot meter (2 rows) of `left`; name `Credits left this period`, value text = `detail`; warning tone when
  `left < 0.15`.
* Numeral `{pct}%` (screen reader `{pct} percent of credits left`); accent/warning tone when `left < LOW_SHARE = 0.15`
  ("low credits is one of the few uses of the accent").
* Facts line (tooltip = `detail`): `{formatCompact(remaining)} of {formatCompact(limit)} left`, and the reset label.
  Example: count 61 234, limit 100 000, reset in 12 days → `39%`, `38.8K of 100K left`, `Resets in 12 days`; detail
  `38,766 of 100,000 characters left. Resets in 12 days`.

### 6.2 Source labels and icons (`charts/labels.ts`)

| Source | Label | Icon (semantic) |
|---|---|---|
| `context-chip` | Right-click chip | cursor |
| `hotkey` | Hotkey | keyboard |
| `selection-popup` | Selection popup | text-cursor |
| `browser-extension` | Browser extension | globe |
| `explorer` | File Explorer | folder |
| `clipboard` | Clipboard | clipboard |
| `manual` | Typed in app | text |
| `replay` | Replay | replay |
| `cli` | Command line | terminal |
| unknown value | the raw value | info |

---

## 7. (reserved) — Voices and Settings pages are out of scope; see their own spec.

---

## 8. Dev-only pieces

* `#/gallery`: design-system reference page (§2.3). Optional.
* `lib/mock-vox.ts`: an in-memory fake of the whole backend API for UI previews without the main process (deterministic
  LCG PRNG `s = s*1664525 + 1013904223 (mod 2^32)`, 64 sample records, a sample account `creator` 61 234 / 100 000
  resetting in 12 days, a "playing" player state, sample voices/models, `mockSummary(range)`). Fully specified in
  `contracts.md` §7. Recommended for the port as a "preview data provider" so every screen state can be rendered in
  isolation.

---

## 9. Platform behaviours Chromium provided implicitly (reproduce natively)

| Original mechanism | What it did | Native Windows equivalent |
|---|---|---|
| URL hash + session history | route storage, back/forward stack | explicit `Vec<Route>` + index; `WM_XBUTTONUP` (XBUTTON1/2) or `WM_APPCOMMAND` `APPCOMMAND_BROWSER_BACKWARD/FORWARD` |
| `matchMedia('(max-width: 1099px)')` | rail mode | window client width in DIPs (`GetClientRect` / `GetDpiForWindow`), re-evaluated on `WM_SIZE` |
| container queries (900/860/760 px) | per-page reflow | page content width at layout time |
| `prefers-color-scheme` | `system` theme | `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`, refresh on `WM_SETTINGCHANGE` "ImmersiveColorSet" |
| title-bar overlay | native caption buttons on a custom 44 px bar | `WM_NCCALCSIZE` + `WM_NCHITTEST` (HTCAPTION for the strip; HTMAXBUTTON for Win11 snap layouts) + DWM caption colour / dark-mode attributes |
| window `focus` / `visibilitychange` | "wake" (§1.4, §1.5) | `WM_ACTIVATE` (WA_ACTIVE/WA_CLICKACTIVE), `WM_SHOWWINDOW` (shown from tray), restore from minimised; plus `WM_POWERBROADCAST` resume for midnight/reset timers |
| `KeyboardEvent.repeat` | ignore auto-repeat for Ctrl+1..5 | `WM_KEYDOWN` lParam bit 30 |
| `:focus-visible` | only keyboard focus pins chart tooltips | track last input modality (keyboard vs pointer) |
| `navigator.clipboard.writeText` | Copy text | `CF_UNICODETEXT` via `OpenClipboard`/`SetClipboardData` |
| `localStorage` | analytics range | small per-user UI state file |
| `Intl.*` with default locale | numbers, compact numbers, dates, relative times | ICU4X or Win32 NLS (`GetNumberFormatEx`, `GetDateFormatEx`, `GetTimeFormatEx`) with `GetUserDefaultLocaleName` |
| `<dialog>.showModal()` | modal, focus trap, Esc → cancel, initial focus on `data-autofocus` | modal overlay with focus scope |
| `title` attributes | tooltips | native tooltips |
| `aria-live` | announcements | `UiaRaiseNotificationEvent` |
| ResizeObserver | chart widths | layout pass |
| `setTimeout` max 2^31−1 ms | reset timer stepping | not needed with 64-bit timers, but keep "re-check on wake" |
| `setPointerCapture` on the seek bar | drag continues outside the bar; capture loss cancels | `SetCapture` / `ReleaseCapture`, treat `WM_CAPTURECHANGED` as cancel (no seek) |
| `<audio>` (onboarding voice previews) | stream an MP3 preview URL, pause/stop, ended event | any audio sink (e.g. the app's own player, `rodio`/WASAPI) with an HTTP fetch of `previewUrl` |
| `Intl.RelativeTimeFormat({numeric:'auto'})` | `yesterday` / `tomorrow` / `today`, `in N …` / `N … ago` | ICU4X relative time, or hand-written English strings if the port fixes en-US |
| clock changes (`Date.now()` jumps) | day key only re-checked on the next tick/focus | also handle `WM_TIMECHANGE` |

---

## 10. Onboarding step components (`components/onboarding/*`)

The steps live in this subsystem's files; the flow that hosts them is `pages/Onboarding.tsx` (summarised here for
context). Steps in order: `welcome` (`Welcome`), `key` (`API key`), `voice` (`Voice`), `try` (`Try it`), `done`
(`Done`). Each step receives: a heading handle (focused when the step appears so screen readers announce it — not on
first paint), `goTo(step)`, and the API-key draft (kept by the host so Back/Continue don't wipe it).

Host summary: crumb `Setup {i}/5` (hidden on welcome); `Skip setup` (every step but the last) → finish; `Back` (not on
first/last); progress dots `Step {i}: {label}` (clickable up to the furthest visited step); next button: welcome
`Get started`, key step without a key `Skip for now` (secondary style), last `Open Vox`, else `Continue`. Finish =
`updateSettings({onboarded:true})` then navigate Home; failure → toast `Couldn't save your setup: {message}`. While
onboarding, a navigation to `settings/account` (e.g. a toast's "Add key") jumps to the key step instead.

### 10.1 Welcome
Animated logo (accessible name `Vox logo`); heading = app name upper-cased (`VOX`, read as `Vox`); tagline
`Select any text. Hear it read aloud.`; body
`Vox turns anything on your screen into natural speech with ElevenLabs voices. Setup takes about a minute.`

### 10.2 API key (`StepApiKey.tsx`)
* Eyebrow `ElevenLabs`; title `Connect your account`; body
  `Vox speaks with your own ElevenLabs API key. It is stored encrypted on this PC and only ever sent to ElevenLabs.`
* Form (shown when there is no key, or while replacing):
  * Field `API key`; hint `Find it under Settings → API keys on elevenlabs.io.` (hidden while an error shows); masked
    monospace input, placeholder `sk_…`, no autocomplete/spell-check, disabled while connecting; trailing toggle
    `Show key` / `Hide key`.
  * `Connect` (submit; disabled when the trimmed draft is empty; spinner). Empty on submit → error
    `Paste your API key first.` Otherwise `settings.setApiKey(trimmed)`: `ok` → remember `{warning, account}`, clear the
    draft, re-mask, leave replace mode, `refreshCredits(true)`; not ok → error `res.error || "ElevenLabs rejected that
    key."`; exception → its message. Typing clears the error. Pressing Enter in the field submits the form.
    Note: because `Connect` is disabled while the trimmed draft is empty, and Enter does not submit a form whose
    submit button is disabled, `Paste your API key first.` is effectively unreachable in the original; a port that
    lets Enter submit an empty field should show it.
  * `Get a key` (external-link icon) → open `https://elevenlabs.io/app/settings/api-keys` (`API_KEYS_URL`; a failure
    to open is ignored silently).
  * `Cancel` (only in replace mode) → leave replace mode (the draft is kept).
* Connected state: check mark, `Key connected`, caption = account (from the connect result, else the store's `ready`
  account) ? `{Tier} plan · {formatCompact(remaining)} characters left ({pct}%)` (tier with only the first letter
  upper-cased; the `{Tier} plan · ` part omitted if tier is empty) : `Ready to read.`; the warning from the connect
  result if any (e.g. restricted key: "This key can't read your account usage, so credits won't be shown. Reading works
  as long as it has Text to Speech access."); button `Replace key`. The connect result (account + warning) is held by
  the step itself, so it is forgotten when the user moves to another step and comes back: the caption then falls back
  to the store's `ready` account (or `Ready to read.`) and the warning is no longer shown. `{pct}` uses
  `creditsLeftFraction` from §11.1 (0 for an unknown limit). The connected panel is a polite status region.

### 10.3 Voice (`StepVoice.tsx`)
* Eyebrow `Voice`; title `Pick a voice`; body
  `Preview a few and choose the one you like. You can switch any time on the Voices page.`
* No key → empty state `Connect a key to browse voices` / `{settings.voiceName ?? "The default voice"} will read for you
  until then.`
* Loading → 6 placeholder tiles. Load = `eleven.voices()` (only while a key exists; it re-runs when the key appears);
  retries use `{refresh:true}`. Choosing a voice that is already selected does nothing.
* Error → `Couldn't load voices` / message / `Try again`.
* Zero voices → `No voices on this account` / `Add voices on elevenlabs.io, then come back.`
* List (scrollable, ~268 px): per voice a select button (pressed when it is the current voice) showing the name and
  tags = `[labels.accent, labels.gender, labels.use_case ?? labels["use case"], labels.age]` non-empty, first 3, joined
  ` · ` — or `category`, or `Voice`. Choosing a different voice → `updateSettings({voiceId, voiceName})`, failure toast
  `Couldn't select {name}: {message}`.
* Preview button (only with a `previewUrl`): `Preview {name}` / `Stop {name} preview` (pressed while playing). One shared
  audio player: clicking the playing voice pauses it; another voice stops the current one and plays the new URL. A
  rejected play is ignored if it was superseded (AbortError); otherwise clear the state and toast
  `Couldn't play the {name} preview` (id `voice-preview`). The playing marker is cleared on pause/ended **only if the
  player is really stopped** (the previous preview's pause event arrives after the next one started). The player does
  not preload (`preload = none`). Leaving the step stops and unloads the audio (pause, drop the source).

### 10.4 Try it (`StepTry.tsx`)
* Eyebrow `Try it`; title `Read from anywhere`.
* Illustration (decorative): a paragraph `The quiet revolution in personal computing was never about faster chips.`
  with `It was about software fitting around people.` highlighted, the chip `Read with Vox`, a fake context menu
  (`Copy`, `Search the web`, `Print…`) and a pointer.
* Steps: `1 Select text in any app`, `2 Right-click the selection`, `3 Click Read with Vox beside the menu` (label in
  bold), and, if a read-selection hotkey exists, `or press [keys]`.
* Sample (quoted): `Hi, I'm {voiceName || "Vox"}. Select any text on your screen, right-click it, and choose Read with
  Vox. I'll read emails, articles and documents aloud while you keep working.`
* `canRead = hasApiKey || demoMode` (demo mode reads with mock audio, so needs no key). If so: caption
  `Playing in the floating player…` while the player's `readId` equals the sample's read id and status is
  playing/loading, else `Or hear it right here:`; button `Read this` (spinner) → `readText(sample)` (source `manual`),
  remembering the returned `readId` only when `ok` and a `readId` came back. Otherwise:
  `Connect your ElevenLabs key to hear it.` + `Add a key` → key step.

### 10.5 Done (`StepDone.tsx`)
Check glyph; `You’re all set`; body `Vox lives in the system tray. Close this window whenever you like: it keeps
watching for your selections.`; tips: `Click the tray icon to open Vox` (tray icon); if hotkeys are enabled, each of
these only when its accelerator is non-empty: `Read selection` [keys] (keyboard icon); `Play / pause` [keys] (pause
icon).

---

## 11. Format helpers (exact algorithms and test vectors)

### 11.1 `components/format.ts`

| Function | Algorithm | Examples |
|---|---|---|
| `dotDuration(sec)` | `s = max(0, floor(sec or 0))`; `s<60` → `{s}S`; else `h=floor(s/3600)`, `m=floor(s%3600/60)`; `h>0` → `{h}H{mm}M` else `{m}M` | 15600→`4H20M`, 3900→`1H05M`, 2280→`38M`, 42→`42S`, 0→`0S`, 60→`1M` |
| `durationSpeech(sec)` | floor; `<60` → `{s} second(s)`; else `{h} hour(s)` (if h) + `{m} minute(s)` (if m, or if h==0), space-joined | 15600→`4 hours 20 minutes`, 3600→`1 hour`, 61→`1 minute`, 1→`1 second` |
| `prettyModelId(id)` | regex `^eleven_(?:(.+?)_)?v(\d+)(?:_(\d+))?$` (lazy name group); version `v{2}` + (`.{3}`); no name group → `Eleven {version}`; else name split on `_`, empty parts dropped, first letter of each word upper-cased (rest unchanged), joined with spaces, + ` {version}`; no match → id | `eleven_flash_v2_5`→`Flash v2.5`, `eleven_v3`→`Eleven v3`, `eleven_multilingual_v2`→`Multilingual v2`, `eleven_english_sts_v2`→`English Sts v2`, `foo`→`foo` |
| `modelLabel(id, models?)` | model in list with a name → name with a leading `Eleven ` removed unless followed by `v`+digit (`/^Eleven\s+(?!v\d)/i`); else `prettyModelId` | `Eleven Flash v2.5`→`Flash v2.5`; `Eleven v3`→`Eleven v3` |
| `dayKeyOf(date)` / `parseDayKey(key)` | local `YYYY-MM-DD` ↔ local midnight; parse requires `^\d{4}-\d{2}-\d{2}$`, else null | |
| `weekdayInitial(key)` | `S M T W T F S`[weekday]; invalid → `·` | |
| `weekdayName(key)` | `Sunday`…`Saturday`; invalid → key | |
| `greeting(now)` | 5≤h<12 morning, 12≤h<18 afternoon, else evening | 04:59→`Good evening` |
| `remainingClock(pos, dur, est)` | `left = max(0, round((dur or 0) − (pos or 0)))` (JS round, half up); `m=floor(left/60)`, `s=left%60`, `h=floor(m/60)`; clock `h:mm:ss` (minutes `m%60` padded) if h>0 else `m:ss`; prefix `~` if estimate, then `-` | (63,263,false)→`-3:20`; est→`~-3:20`; 3700 left→`-1:01:40`; (63.4,263)→`-3:20` |
| `creditsLeftFraction(count, limit)` | `limit ≤ 0` (or NaN) → 0; else `clamp((limit − count)/limit, 0, 1)` | (61234,100000)→0.38766 |
| `resetLabel(unix, now)` | `unix` missing or 0 → null; `days = ceil((unix*1000 − now)/86 400 000)`; `≤0` `Resets today`; `1` `Resets tomorrow`; else `Resets in {days} days` | 3 h ahead → `Resets tomorrow` (quirk: any reset within 24 h); 2 h ago → `Resets today` |
| `resetDateLabel(unix)` | locale `month short, day numeric` | `Oct 20` (unused in scope) |
| `rateLabel(rate)` | `round((rate or 1)*100)/100` + `×` (0 or NaN → 1); the number printed in shortest form | 1→`1×`, 1.25→`1.25×`, 1.5→`1.5×`, 1.333→`1.33×`, 0→`1×` |

Mismatch to know about: the visible `remainingClock` **rounds** the remaining seconds, while every spoken "… left"
label uses `formatClock`, which **floors** (`formatClock(max(0, dur − pos))`). At position 63.4 of 263 the screen shows
`-3:20` and a screen reader says `3:19 left`.

### 11.2 `lib/format.ts` and `HistoryModel.ts`

| Function | Algorithm | Examples |
|---|---|---|
| `formatClock(sec)` | floor, clamp ≥0; `h:mm:ss` if h>0 else `m:ss` | 263→`4:23`, 3723→`1:02:03`, 0→`0:00` |
| `splitHM(sec)` | `mins=floor(max(0,sec)/60)` → `{h: floor(mins/60), m: mins%60}` | |
| `formatDuration(sec)` | `s=round`; `<60` `{s}s`; h==0 `{m}m`; m==0 `{h}h`; else `{h}h {m}m` | 15600→`4h 20m`, 300→`5m`, 20→`20s`, 3600→`1h` |
| `formatHours(sec)` | hours ≥100 → integer, else one decimal | 5400→`1.5` (unused in scope) |
| `formatNumber(n)` | `round(n or 0)` (JS round, half up; NaN → 0) with locale grouping | 38766→`38,766`, 2.5→`3` |
| `formatCompact(n)` | locale compact *short* notation, max 1 fraction digit, trailing zeros dropped, rounding half away from zero; the input is **not** rounded first (NaN → 0); if rounding reaches the next unit, the next unit is used | 12400→`12.4K`, 100000→`100K`, 999→`999`, 1000→`1K`, 1234→`1.2K`, 12345→`12.3K`, 123456→`123.5K`, 999950→`1M`, 1500000→`1.5M`, 2.5e9→`2.5B`, 999.6→`999.6` |
| `formatRelative(ms, now)` | `diff=(ms−now)/1000`, `abs`; `<45` → `just now`; `<3600` → relative minutes `round(diff/60)`; `<86400` → relative hours `round(diff/3600)`; else calendar days `round((startOfDay(ms) − startOfDay(now))/86 400 000)`: `abs(days) < 7` → relative days; else locale date `month short, day numeric, year numeric`. Relative words use "auto" numeric style (−1 day → `yesterday`, +1 → `tomorrow`, 0 → `today`); future times read `in 5 minutes`, `in 2 hours`, `in 3 days` | 30 s ago `just now`; 5 min `5 minutes ago`; 90 s `1 minute ago` (JS rounds −1.5 → −1); 90 s ahead `in 2 minutes`; 59 min 45 s ago `60 minutes ago` (round(−59.75) = −60, quirk); 23 h 50 min ago `24 hours ago` (quirk); 2 h `2 hours ago`; 23:00 yesterday seen at 01:00 → `2 hours ago`; 30 h ago crossing one midnight → `yesterday`; 24.5 h apart within one 25-hour DST day → `today`; 3 days → `3 days ago`; 9 days → `Oct 8, 2026` |
| `truncate(text, max)` | collapse whitespace runs (JS `\s`, §0) to one space, trim; if `len ≤ max` return; else `slice(0, max−1)`, trim end, + `…` (result length ≤ max) | `truncate("a  b", 10)`→`a b`; `truncate("hello world", 6)`→`hello…` |
| `countWords(text)` | number of `\S+` runs in the trimmed text | `"  a  b\nc "`→3 |
| `dayLabel(ms, now)` | `diff = round((midnight(now) − midnight(ms))/86 400 000)` (DST-safe); `≤0` `Today`; `1` `Yesterday`; `<7` weekday long; same year → `{Mon}, {Sep} {14}`; else `{Mon}, {Sep} {14}, {2025}` (locale `weekday short, month short, day numeric[, year numeric]`) | |
| `rowTime(ms, now)` | `now − ms < 6×86 400 000` (elapsed ms, not calendar days) → `formatRelative`; else locale time `hour numeric, minute 2-digit` (`9:05 PM`). Rows 6–7 days old therefore show a time of day under a weekday header | |
| full date-time (row tooltip, `Read` fact) | locale `dateStyle: full, timeStyle: short` | `Friday, October 17, 2026 at 9:05 PM` |
| `displayUrl(url)` | parse; `hostname` minus leading `www.` + `pathname` (omit when `/`); parse error → raw | `https://www.example.com/a/b?q=1`→`example.com/a/b` |
| `isWebUrl(url)` | non-empty and `^https?://` (case-insensitive) | |
| `audioLength(r)` | `audioDurationSec>0` → exact; else `round(charCount/15)`, estimate | |

`JS Math.round` rounds half toward +∞ (`round(-1.5) = -1`, `round(2.5) = 3`); Rust's `f64::round` rounds half away
from zero. Use `(x + 0.5).floor()` where parity matters (relative times, percentages, dot levels).

### 11.3 Chart / History helpers (`charts/model.ts`, `charts/labels.ts`)

| Function | Algorithm | Examples |
|---|---|---|
| `formatHMDot(sec)` | `s=max(0, round(sec or 0))`; `0<s<60` → `{s}S`; `mins=floor(s/60)`, `h`, `m`; h==0 → `{m}M`; h≥100 → `{h}H`; else `{h}H{mm}M` | 152280→`42H18M`, 1080→`18M`, 45→`45S`, 0→`0M`, 59.6→`1M`, 360000→`100H` |
| `speakDuration(sec)` | like `durationSpeech` but rounds instead of flooring | 59.6→`1 minute`, 0→`0 seconds` |
| `shortDuration(sec)` | identical output to `formatDuration` | 4320→`1h 12m` |
| `hourRange(h)` | `{HH}:00–{HH+1 mod 24}:00` (en dash) | 21→`21:00–22:00`, 23→`23:00–00:00` |
| `mondayIndex(date)` | `(getDay()+6)%7` | Sunday→6 |
| `bucketTitle(b, unit)` | day `{Tue}, {Sep} {30}`; month `{September} {2026}`; week `{Sep} {8} – {Sep} {14}` (single date if 1 day) | |
| `recentActivity(daily, n)` | last n days → `reads>0 or listenedSec>0` | |
| `activeDays(daily)` | `{active: count(reads>0 or listenedSec>0), total: len}` | |
| `modelLabel(id)` (History) | `""` → `Unknown model`; replace `_v(\d+)_(\d+)` → ` v$1.$2`; split `[_\s]+`; capitalise words except those starting `v`+digit | `eleven_flash_v2_5`→`Eleven Flash v2.5`, `eleven_v3`→`Eleven v3` |
| `tierLabel(tier)` (Analytics) | `""` → `Plan`; else replace runs of `[_-]` with one space, then upper-case every word-initial `[A-Za-z0-9_]` (`\b\w`); rest unchanged | `pro_plus`→`Pro Plus`, `creator`→`Creator` |
| `titleCase(tier)` (Home) | split `[_\s-]+`, drop empties, upper-case first char of each (rest unchanged), join `' '`; Home hides an empty tier | `pro_plus`→`Pro Plus` |
| `sourceLabel(s)` / `sourceGlyph(s)` | table §6.2; unknown → raw value / `info` | |

### 11.4 Dot meter fill rule (design `DotMeter`)

Cells filled = `round(clamp(value,0,1) × rows × columns)`, filled **column by column, top→bottom** (battery style):
cell index `c·rows + r` (r = 0 at the top) is lit when `< filled`. Columns = as many as fit the width:
`max(1, floor((width + gap) / (dot + gap)))`, with the horizontal pitch stretched to `(width − dot)/(columns − 1)` so
the meter spans the width edge to edge. Warning tone when the meter is `auto` and value < `lowAt` (default 0.15); with
`accentLast` the last lit cell (index `filled − 1`) is drawn in the accent. Accessibility: a meter with value
`round(clamp(value)·100)` of 0..100, a name and optional value text (`design/DotMeter.tsx:30-97`).

Meters used here: sidebar credits (1 row, auto tone), CreditsSummary (2 rows, auto tone), Today goal (2 rows,
`accentLast` when done), Analytics days-active (3 rows), per-source meters (2 rows).

---

## 12. Inconsistencies and porting hazards (decide deliberately)

1. **Two `creditsLeftFraction`s**: `components/format.ts` returns **0** for an unknown/zero limit (UI shows `0%` in the
   warning colour); `charts/CreditsCard.tsx:19-22` exports one returning **1** (unused by the card). Unlimited plans
   would look empty.
2. **Two model labels**: Home `Flash v2.5` vs History details `Eleven Flash v2.5` for the same id.
3. **Two "heard" fractions**: History caps at 0.99 unless completed and uses 0 when audio length is unknown; main's
   analytics `heardFraction` uses the char-based estimate and returns 1 when no length can be derived.
4. **Mixed apostrophes** (`’` vs `'`) in copy (§0).
5. Analytics refetches on **every** `history:changed` (≈ every 2 s during playback) with no debounce; Home and History
   debounce 700 ms.
6. `all` range: totals are all-time, but `daily` is capped at 365 days, so `Avg` (= total ÷ active days in the last 365)
   and `Days active x/365` mix windows for histories older than a year.
7. The Analytics empty banner keys on `reads == 0`; listening from a replay of an older read can be > 0 in the same range,
   so the banner can contradict the hero.
8. Home's `failed` flag is never shown; a failed first load leaves skeletons until the next trigger.
9. `resetLabel` says `Resets tomorrow` for any reset less than 24 h away, even later today.
10. Recent card: a paused read's button replays from the start rather than resuming.
11. "This week" is the rolling last 7 days.
12. History's "nothing yet" state can appear while an undo is pending if the last record was just deleted (the toolbar
    and header actions vanish until Undo).
13. History's count label `X of Y reads` is always plural.
14. The theme toggle replaces `system` with an explicit theme.
15. Rounding: JS `Math.round` vs Rust `round` (§11.2).
16. **No error state for the shell's own startup**: a failed `settings.get()` leaves the window on an empty strip forever
    (§1.3, §2.2).
17. `formatRelative` can print `60 minutes ago` (59.5–60 min) and `24 hours ago` (23.5–24 h) because it rounds before
    switching units (§11.2).
18. The visible remaining time rounds, the spoken one floors (§11.1), so they can differ by a second.
19. Two definitions of "active day" on the Analytics page: `Days active` counts reads **or** listening (§5.4), the
    daily chart's spoken summary counts listening only (§5.8).
20. Analytics `Refresh balance` gives no busy feedback from `ready`/`restricted` (§5.12).
21. Onboarding's `Paste your API key first.` can never show (§10.2), and the connect warning is lost after leaving the
    key step.
22. History search: clearing then quickly typing a new query briefly re-applies the old query (§4.1).
23. Ctrl+1..5 switch pages even while typing in a text field (§2.8); History's `Ctrl+F` likewise works while typing.
24. Home's Recent card shows `Failed` only for reads that never produced audio or listening; History shows `Failed` for
    any read with an error (§3.10 vs §4.7).
25. Analytics shows a `Plan` badge for an empty tier, Home shows none (§5.12 vs §3.6), and the two tier helpers differ
    slightly (§11.3).
26. History favourite failures have no toast id, so repeated failures stack up to the 4-toast cap and can evict the
    Undo toast — which commits pending deletes (§1.7, §4.10).
27. Analytics's first-load error card does not show progress while retrying (§5.2).

---

## 13. Test vectors (pure functions)

```text
parseHash("#/settings/account")      = {settings, "account"}
parseHash("#/foo/bar")               = {home}
parseHash("#///history//x//y")       = {history, "x/y"}
bucketDaily(365 days, 106) unit      = week, 53 buckets, first.days = 1, last ends today
bucketDaily(90 days, 106) unit       = day, 90 buckets
bucketDaily(365 days, 50) unit       = month (12 or 13 buckets)
columnGeometry(30, 640, 168)         = {colPitch 21.333, dotPitch 11, dot 7.9, perColumn 1, rows 15}
columnGeometry(7, 640, 168)          = {colPitch 91.43, dotPitch 11, dot 7.9, perColumn 5, rows 15}
pickTicks(30, 10)                    = [2,5,8,11,14,17,20,23,26,29]
dotLevels([0,100,50,1], 15)          = [0,15,8,1]
sparkLevels([0,3,6], 6)              = [0,3,6]
heat geometry width 640              = pitch 25, x0 40, height 197, maxR 10.5, minR 2.25, hourStep 3
barDots(1, 100, 20)                  = 1 ; barDots(50,100,20) = 10
listenedFraction({completed:false, audioDurationSec:100, maxPositionSec:100}) = 0.99
listenedFraction({completed:false, audioDurationSec:0, listenedSec:50})        = 0
listenedFraction({completed:false, audioDurationSec:100, listenedSec:40})      = 0.4   // no maxPositionSec
audioLength({audioDurationSec:0, charCount:300})  = {sec 20, estimate true}  → row "~0:20", details "about 0:20"
TodayCard caption at listened 600 s  = "5m to your daily goal" ; at 899.5 s = "1m to your daily goal"
routeHash({settings, "a/b"})         = "#/settings/a%2Fb"
parseHash("#/settings/account?x=1")  = {settings, "account"}
parseHash("#/History")               = {home}
formatRelative(now-3585s, now)       = "60 minutes ago"
formatRelative(now-85800s, now)      = "24 hours ago"
formatRelative(now+90s, now)         = "in 2 minutes"
formatCompact(999950)                = "1M" ; formatCompact(123456) = "123.5K"
remainingClock(63.4, 263, false)     = "-3:20" ; spoken formatClock(199.6) = "3:19"
resetLabel(now/1000 - 7200, now)     = "Resets today"
dotLayout(100, 7, 3)                 = {count 10, pitch 10.333}
dotLayout(5, 7, 3)                   = {count 1, pitch 0}
DotMeter cells(value 0.38766, rows 2, cols 20) = round(15.5064) = 16 lit
monogram("Élodie") = "E" ; monogram("No voice") = "N" ; monogram("李") = waveform fallback
prettyModelId("eleven_turbo_v2_5")   = "Turbo v2.5" ; History modelLabel("") = "Unknown model"
tierLabel("") = "Plan" ; titleCase("pro_plus") = "Pro Plus"
truncate("hello world", 6)           = "hello…"
dayLabel(yesterday 23:59, today 00:01) = "Yesterday"
```
