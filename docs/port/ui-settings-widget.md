# Vox port spec: Settings, Voices, Onboarding, floating widget UI, chip, design behaviors

Source root: `C:/Users/Kwaku/Desktop/Desktop Widget`. All paths below are relative to `src/renderer/src/` unless they
start with `src/` (then they are relative to the source root). Line numbers refer to the files as read for this spec.

This document covers **behavior, data and copy** of the user-facing UI. Visual styling is deliberately omitted: the UI
is being redesigned. Where a styling rule has behavioral weight (which parts of the widget drag the window, minimum hit
targets, which window is always dark) it is kept.

Sibling specs (same folder) own the neighbouring details; this spec restates only what the UI needs:

| Topic | Owner |
|---|---|
| Settings schema, defaults, sanitize/clamp rules, IPC channel list | `contracts.md` |
| Player state machine (`AudioQueuePlayer`), auto-hide controller, media session, read lifecycle | `reader-playback.md` |
| Window creation/placement/hover polling, tray, global hotkey registration, settings-change side effects in the core | `shell.md` |
| History/cache stores, retention, export format | `persistence.md` |
| Selection capture, chip triggering (hook) | `selection.md` |

## 0. Conventions

- `APP_NAME = 'Vox'`, `READ_WITH_LABEL = 'Read with Vox'` (`src/shared/constants.ts:2-4`). Every copy string below that
  contains "Vox" is built from `APP_NAME`; keep it a single constant.
- Copy is quoted exactly. Typographic characters are significant: `’` (U+2019), `“ ”` (U+201C/D), `—` (em dash),
  `…` (U+2026), `×` (U+00D7), `−` (U+2212 minus), `≈` (U+2248), and `\u00a0` (no-break space) where noted. Some
  strings in the source use a straight `'` instead of `’`; those are reproduced as-is (the inconsistency is listed in
  §11).
- "Toast id" = the dedupe id passed to the toast system (§1.5): a new toast with the same id replaces the old one.
- "Core" = the former Electron main process (settings store, reader, windows). "UI" = renderer code.
- All settings writes go through the core's `settings.update(patch)`: a deep-merge patch (objects merge; arrays, `null`
  and primitives replace), then the core sanitizes/clamps the full object (`src/main/services/settings-store.ts:85-182`)
  and broadcasts the new settings to every window. The clamp ranges are repeated in the binding table (§2.12).

---

## 1. Shared UI infrastructure

### 1.1 Optimistic settings model (`app/settings/hooks.ts:21-75`)

Used by the Settings page and the Voices page (one instance per page, `update` passed down to sections).

State:
- `server`: last settings received from the core (initial `get()`, then every `settings:changed` broadcast, and the
  return value of each `update`).
- `overlay`: settings with in-flight patches applied locally (null when nothing is in flight).
- `accepted`: settings adopted from `setApiKey` / `clearApiKey` results (null after the next fresh `server` value).
- `pending`: count of in-flight updates. `saving = pending > 0`.

What the UI shows: `overlay ?? accepted ?? server` (null until the first load).

`update(patch)`:
1. If nothing is loaded yet, ignore.
2. `optimistic = mergeSettings(latestShown, patch)` (same merge rule as the core: objects merge, arrays/null/primitives
   replace; never mutates; `logic.ts:49-62`). Show it immediately; `pending += 1`; `saving = true`.
3. Send the patch to the core.
4. On failure: error toast, title `Couldn’t save that setting`, message = cleaned error text (§1.4), toast id
   `settings-save-error`.
5. When `pending` returns to 0 (success or failure): drop the overlay (the server copy now reflects every patch that
   succeeded; a failed one therefore visually reverts) and set `saving = false`.

Note: successive updates chain on the *shown* value (`latest`), so rapid toggles accumulate correctly even before the
core answers. The `server` copy is replaced by the core's return value of each successful `update` as well as by
broadcasts, so by the time `pending` reaches 0 it already reflects every patch that succeeded.

`accept(next)`: adopt a full settings object returned by another call (API key set/clear); it also becomes the base
the next `update` chains on immediately (before any re-render). Cleared as soon as a newer server snapshot arrives.

If the page goes away with writes in flight, the overlay/`saving` reset is skipped (nothing left to render); the writes
themselves still complete in the core.

### 1.2 Loadable fetch helper (`app/settings/hooks.ts:113-162`)

States: `idle` | `loading {data?}` | `ready {data}` | `error {error, data?}`. Previous data is kept through
`loading`/`error` so lists never blank during a refresh.

- Runs the fetcher whenever its **key** changes; a `null` key means "don't fetch" (state `idle`). Switching to a `null`
  key **drops the previous data** (it is the only transition that does) and invalidates any in-flight response.
- Every run takes a token; a response whose token is no longer current (key changed, newer run, unmounted) is dropped.
- `reload(refresh = true)` re-runs with `refresh=true`, which tells the core to bypass its ElevenLabs list caches.
- First run for a key uses `refresh=false`.
- Error text is passed through `cleanError` (§1.4).

### 1.3 App info (`app/settings/hooks.ts:77-111`)

`AppInfo` (`src/shared/types.ts:399-415`): `name, version, electronVersion, platform, isPackaged, userDataPath,
extensionPath, serverPort, serverRunning, nativeHookAvailable, explorerMenuInstalled, demoMode`. Loaded once when the
Settings page mounts; `refresh(delayMs)` re-reads it after `delayMs` (debounced: a new refresh cancels a pending one;
`delayMs ≤ 0` loads immediately). Load errors are swallowed (the previous value stays). `null` until the first load:
every consumer treats unknown fields as "not known yet" (e.g. hook availability defaults to available, §2.5).

### 1.4 Error text cleaning (`app/settings/logic.ts:326-331`)

```
raw = err.message | err (if string) | 'Something went wrong'
msg = raw.replace(/^Error invoking remote method '[^']*':\s*/, '')   // Electron IPC wrapper
         .replace(/^(\w*Error):\s*/, '')                              // "TypeError: " etc.
return msg.trim() || 'Something went wrong'
```
In a Rust port without IPC wrappers, keep only the second rule (strip a leading `SomethingError: `) and the fallback.

### 1.5 Toasts in the app window (`design/toast-store.ts`, `design/Toast.tsx`)

Data: `{ id, kind: 'info'|'success'|'error', message, title?, action?: {label, onClick}, duration (ms, 0 = sticky),
createdAt }`.

| Rule | Value |
|---|---|
| Default duration | info 4000 ms, success 3000 ms, error 6500 ms (`toast-store.ts:32`) |
| With an action and no explicit duration | `max(kind default, 6000)` (`toast-store.ts:53`) |
| Max on screen | 4; the oldest are dropped (`MAX_TOASTS`, `toast-store.ts:33, 57`) |
| Same id pushed again | the old one is removed and the new one is appended as newest; its timer restarts (the card is keyed by `id:createdAt`) |
| Generated id | `'t' + base36(now) + base36(seq++)` |
| Pause | pointer over the toast, or focus inside it, pauses the countdown. At the moment of pausing the remaining time becomes `max(800, remaining − elapsed)`; on resume a fresh timer runs for that remaining time (`Toast.tsx:38-46`). Focus moving between elements *inside* the toast does not resume it; only focus leaving the toast does (`Toast.tsx:56-59`) |
| Exit | dismissal by timer, close or action plays a 180 ms exit, then removes the toast (`Toast.tsx:10, 29-34`). A toast removed by the 4-toast cap, by a same-id replacement or by a programmatic `dismiss(id)` disappears at once (no exit) |
| Title | optional bold first line above the message |
| Action button | runs `onClick`, then dismisses |
| Close button | always present, accessible name `Dismiss notification` |
| Semantics | `error` → assertive (role alert); others → polite (role status). Region name `Notifications`, polite live region |
| Glyph per kind | info → info, success → check, error → alert |
| Placement | bottom-right in the normal shell; bottom-center during onboarding (`app/App.tsx:109, 125`) |

Core-originated toasts (`app:toast` event, payload `{id, kind, message, action?: {label, route?}}`) are pushed into
this store with the core's id; their action navigates the app window to `route` (`app/store.ts:283-293`). The same
event also reaches the widget (§6.9).

### 1.6 Confirmation dialog (`design/Dialog.tsx`)

Modal with an inert background. Props used in scope: glyph (always `trash`), tone `danger`, size `sm` (sizes: sm 400,
md 520, lg 680 px wide), title, description, footer buttons.

- `dismissable` (default true; every dialog in scope uses the default): only then do Esc / backdrop close requests reach
  the owner and is the header close button (`Close`) rendered.
- Close requests (Esc, backdrop click, the header close button `Close`) call the owner's `onClose`; the owner decides
  (several owners ignore it while an operation is running). The dialog never closes itself: Esc's default action is
  cancelled and the owner flips `open`.
- A backdrop click closes only if the press **started and ended** on the backdrop (a text selection dragged outside must
  not close it).
- Initial focus (applied each time the dialog opens): explicit target, else the element marked autofocus, else the first
  focusable element that is not the close button, else the first focusable (in every dialog in scope this is the
  footer's **Cancel**). "Focusable" = links with href, enabled buttons/inputs/selects/textareas, and anything with a
  tab index other than −1.
- Tab / Shift+Tab cycle within the dialog (focus trap): Tab on the last *visible* focusable goes to the first, Shift+Tab
  on the first goes to the last.
- Accessible name = title; description (when present) is the accessible description.
- Close plays a 180 ms exit before unmounting. If the system closes the modal on a repeated Esc despite cancellation,
  state is resynchronised (owner's `onClose`), or the modal is reopened when not dismissable.

### 1.7 Slider and slider rows (`design/Slider.tsx`, `app/settings/parts.tsx:135-191`)

Slider:
- Value snapped to `step` from `min` without float noise: `min + round((v − min)/step)·step`, rounded to
  `min(10, decimals(step)+2)` decimal places (`decimals` = digits after the point in the step's decimal text), then
  clamped to `[min, max]`; a non-positive step only clamps (`design/util.ts:16-21`).
- Pointer → value: `f = clamp((pointerX − trackLeft − t/2) / (trackWidth − t), 0, 1)`, `value = snap(min + f·(max−min))`,
  where `t` is the track thickness (sm 8, md 12, lg 16 px; settings rows use sm) — the knob's half-width is excluded at
  both ends (`Slider.tsx:75-83`). If `max == min` the span is treated as 1.
- Pointer: press on the track (left button only) captures the pointer, focuses the slider, sets the value at the
  pointer (live only, no commit); moves update live; release commits the value **at the release position**. Pointer
  cancel / lost capture ends the drag without commit.
- Keys: Right/Up `+step`, Left/Down `−step`, PageUp/PageDown `±max(step, (max−min)/10)`, Home `min`, End `max`; every
  key press both changes and **commits** (a commit fires even if the value did not change — SliderRow filters that).
- `onChange` fires live (only when the snapped value differs); `onCommit` fires on release / key press.
- Disabled: not focusable, ignores pointer and keys. Accessible: role slider, name = row label, value text = the row's
  formatted value (`50%`, `1.00×`, `500 MB`).

SliderRow (settings rows):
- Keeps a local **draft** while dragging; the row shows `format(draft ?? value)` beside the slider.
- Commit: clear the draft; call the binding's `onCommit(v)` **only if `v !== value`** (so a click that lands on the
  current value writes nothing).
- If focus leaves the row while a draft exists (drag interrupted without a release), the draft is committed.

### 1.8 Other control semantics

| Control | Behavior |
|---|---|
| Toggle row (`parts.tsx:89-100`) | A switch; the whole label + description area toggles it. Disabled rows don't toggle. Optional illustration next to it (decorative only). |
| Row (`parts.tsx:59-75`) | Label, optional description, control on the right (or below when "stacked"), optional extra content under it (warnings). `disabled` only dims; the control itself is disabled separately. |
| Sub-heading (`parts.tsx:102-111`) | Small caption grouping rows; may carry a right-side badge/button. |
| Notice (`parts.tsx:113-133`) | Inline callout, tone info (info glyph) or warning (alert glyph), optional title and action. |
| Segmented (`design/Segmented.tsx:60-83, 112`) | Single-choice group (radio group named by its label); arrow keys move focus **and select** (Right/Down next, Left/Up previous, **wrapping around** at the ends; Home/End first/last) — each key press fires a change. Clicking the already-selected option fires a change again with the same value (so e.g. Playback speed re-writes the rate and re-sends the live `rate` command; the store must treat an identical value as a no-op). |
| Select | Native dropdown; each change fires immediately (arrow keys on a closed select fire one change per option passed — see the retention debounce, §2.8). |
| Search field (`design/Input.tsx:112-161`) | Esc clears a non-empty value (and the event stops there); a clear button `Clear search` appears when non-empty and refocuses the field. |
| Form field (`design/Input.tsx:169-209`) | Label, control, and one note line: the error text **replaces** the hint while present. |

### 1.9 Theme (`design/theme.ts`, `app/App.tsx:67-89`)

- Modes (`ThemeMode`): `system` | `paper` | `night`. Resolved theme: `paper` | `night`.
- `resolve(mode, prefersDark) = mode == 'paper'|'night' ? mode : (prefersDark ? 'night' : 'paper')`. While the mode is
  `system`, the UI follows the OS dark-mode preference **live** (re-resolves on change).
- Canvas colors (also the title-bar caption colors): paper `{background '#ececec', foreground '#0b0b0b'}`, night
  `{background '#000000', foreground '#f2f2f2'}` (`theme.ts:7-10`). The UI pushes these to the window's caption buttons
  whenever the resolved theme changes (`app:set-titlebar-theme`). (The core uses slightly different symbol colors when
  first creating the window — `#111111` / `#ffffff`, `src/main/windows.ts:45-48` — see §11.)
- Nothing is themed until settings have loaded; the theme is applied before the first paint with real settings so the
  wrong theme never flashes.
- The **widget and chip windows are always `night`**, regardless of the app theme (`widget/main.tsx:9-10`,
  `chip/main.tsx:6-7`).
- Quick toggle in the app's top bar: switches to `paper` if the resolved theme is `night`, else to `night` (it never
  selects `system`); accessible name `Switch to night theme` / `Switch to paper theme`; failure toast
  `Couldn't change the theme: {message}` (`app/App.tsx:85-89`, `app/components/TopBar.tsx:35`).

---

## 2. Settings page (`app/pages/Settings.tsx`, `app/settings/*`)

### 2.1 Layout, navigation, deep links

Header: kicker `Vox · Preferences`, title `Settings`. Status (polite live region, right side):
- while `saving` (§1.1): `Saving` with a small loader;
- otherwise, once settings are loaded: `Saved automatically`.

Section index (left nav, `Settings sections`), in order (`logic.ts:20-30`):

| id | Label | Glyph |
|---|---|---|
| `account` | Account | user |
| `voice` | Voice & model | voice |
| `playback` | Playback | play |
| `triggers` | Triggers | cursor |
| `widget` | Widget | widget |
| `appearance` | Appearance | sun-moon |
| `library` | Library & privacy | library |
| `integrations` | Integrations | puzzle |
| `about` | About | info |

- Route: `#/settings/<id>`. Parser: `/^#?\/?settings\/([\w-]+)/` and the id must be one of the table
  (`logic.ts:38-42`). Each section's anchor id is `settings-<id>`.
- Clicking an index item scrolls the section into view (smooth unless the OS "reduce motion" preference is on), focuses
  the section (without scrolling) and does **not** change the route.
- Deep link: when the page's `section` changes (and settings are loaded) it scrolls to that section — **instantly** on
  the first scroll after mount, smoothly afterwards. Re-navigating to the same `#/settings/<id>` (route unchanged) is
  also honored by listening to hash changes directly (`Settings.tsx:55-70`); the shell re-announces a repeated anchor
  (e.g. a toast action) so this works on the second click too (`app/store.ts:94-104`).
- Initial active item: the `section` prop if valid, else `account`.

- Every programmatic scroll (`goTo`) only happens — and only pins/highlights — if the section's element exists.

Scroll spy (`Settings.tsx:72-117`), active only once settings are loaded:
- `SPY_LINE = 0.28`: the current section is the **last** section (in index order) whose top edge, measured in window
  coordinates, is at or above `0.28 × window client height` (the whole window's height, not the scroll container's);
  if none, the first section.
- If the scroll container is within 4 px of its bottom and has scrolled at all, the **last** section (`about`) is
  current (short final sections could never reach the line).
- Only scroll events from the element that scrolls this page count (not text areas, the nav, etc.); computation is
  coalesced to one per animation frame.
- **Pinning:** after a click or deep link, the target stays highlighted (spy suspended) until the user scrolls by hand:
  mouse wheel, touch move, any pointer press (covers scrollbar drags), or keys ArrowUp/ArrowDown/PageUp/PageDown/Home/
  End/Space. Reason: a smooth scroll to a short section near the bottom would otherwise highlight a different section.

Loading: until settings arrive, three skeleton cards (`Loading settings`).

Data fetched by the page: voices and models, only when `settings.hasApiKey || appInfo.demoMode`; keyed by
`hasApiKey`, so they refetch when a key is added/removed (`Settings.tsx:44-46`).

### 2.2 Account (`app/settings/AccountSection.tsx`)

Header badge: `Connected` (success, when a key is stored) | `Demo account` (demo mode and no key) | `No key`
(attention, pulsing).

Three bodies:

**A. No key and not demo, or "Replace"/"Add key" pressed → API key form** (`ApiKeyForm`, lines 188-285)

"Replacing a stored key" below means the form was opened by **Replace** while a key is stored
(`replacing && hasKey`, line 103). The demo body's **Add key** opens the same form but, since no key is stored, it
behaves like the first-time form (lead shown, `API key`, `Save key`) — except that it is auto-focused and has `Cancel`.

| Opened by | Lead paragraph | Field label | Submit | Cancel | Auto-focus |
|---|---|---|---|---|---|
| no key, not demo (always shown) | yes | `API key` | `Save key` | no | no |
| demo → `Add key` | yes | `API key` | `Save key` | yes | yes |
| key stored → `Replace` | no | `New API key` | `Save new key` | yes | yes |

- Lead paragraph: `Vox speaks with your own ElevenLabs account. Paste an API key to start reading.`
- Hint: `Needs Text to Speech access. Add “User: read” to see your credits here.` (an error replaces it, §1.8).
- Input: password-masked, monospace, no autocomplete, no spellcheck, placeholder `sk_…`. A trailing toggle
  `Show key` / `Hide key` (pressed state) reveals the text. Typing clears the error. The input stays **editable while
  saving** (only the buttons reflect busy).
- Buttons: submit (shows busy state while saving), `Cancel` (disabled while busy; closes the form, keeps the stored key),
  and `Get a key` (opens `https://elevenlabs.io/app/settings/api-keys` in the default browser; failure → error toast
  `Couldn’t open the browser`, id `open-external`).
- Submit (Enter or button):
  1. `key = value.trim()`; empty → error `Paste your ElevenLabs API key first.` (no request).
  2. Call the core's `setApiKey(key)` → `ApiKeyResult {ok, error?, warning?, account?, settings}`. The core rejects
     keys > 256 chars (`That doesn't look like an API key`), keys containing whitespace, and validates against
     ElevenLabs `GET /v1/user/subscription`. A key that works but cannot read account info is **ok** with a warning
     (`src/main/tts/elevenlabs.ts:636-666`, copy in `contracts.md`).
  3. `ok` → clear the input, adopt `res.settings` (§1.1 `accept`), close the form, remember `res.warning` (shown as a
     notice, below), bump an internal key version (forces the credits card to re-fetch with refresh), force a credits
     refresh for the rest of the shell (Home, sidebar, Analytics — they would not re-read on their own because
     `hasApiKey` stays true when replacing), success toast `API key saved` (id `api-key`).
  4. not ok → field error `res.error ?? 'ElevenLabs rejected that key.'` (fallback only when `error` is absent; an empty
     string would show empty — onboarding uses `||`, §5.3).
  5. thrown → field error = cleaned error (§1.4).
  The warning state is overwritten on every successful save (`res.warning ?? null`), so a later save of a full-access
  key clears the notice.

**B. Demo mode without a key** (lines 108-129): title `Demo account`, caption `Sample data. No key stored.`,
description `Add your own ElevenLabs key to read real text.`; buttons `Add key` (opens form A) and `Get a key`.

**C. Key stored** (lines 130-158): title `ElevenLabs API key`, a row of 24 masked dots (accessible name
`API key stored, hidden` — the key itself is never sent back to the UI), description
`Encrypted with Windows DPAPI. It never leaves this PC except to call ElevenLabs.`; buttons `Replace` (form A),
`Remove` (danger → confirmation), `Manage keys` (opens the API keys page).

Limited-key notice: when a warning was returned by the last save **and** a key is stored: info notice titled
`Limited key` with the warning text. Cleared on remove.

Remove confirmation dialog: title `Remove API key?`, description
`Vox won’t be able to read anything until you add a key again. History and cached audio stay.`; buttons `Cancel`
(disabled while removing) and `Remove key` (danger, busy). Close requests are ignored while removing. Confirm →
core `clearApiKey()` → adopt returned settings, force a credits refresh, clear warning, close form and dialog, info
toast `API key removed` (id `api-key`). Failure → error toast titled `Couldn’t remove the key` with the cleaned error
(id `api-key`), dialog stays open.

**Credits card** (aside, lines 287-351). Header `Credits`, right badge = account tier title-cased (`creator` →
`Creator`, `starter_plus` → `Starter plus`; `logic.ts:320-324`) when known. Fetch: account info when
`hasKey || demoMode`, keyed by `hasKey` and the key version; `refresh=true` when the key version > 0 or on retry.
Bodies, first match (`AccountSection.tsx:296-338`):
1. No key and not demo: empty meter (value 0, accessible name `Credits left`, not animated) +
   `Add a key to see your monthly credits.`
2. Loading with no data yet, or idle: skeletons.
3. Error with no data: `Couldn’t load your account: {error}` + `Try again` (reload with refresh). An error *with* older
   data keeps showing the older data.
4. Data is `null` or `characterLimit == 0` (restricted key; the core returns `null` when the key lacks `User: read`):
   empty meter (as in 1) + `This key can’t read account info, so credits aren’t shown. Reading still works.`
5. Otherwise the shared credits summary (`app/components/CreditsSummary.tsx`):
   - `left = clamp((limit − count)/limit, 0, 1)` (0 if limit ≤ 0 or not a number); `pct = round(left·100)`; meter of
     `left` (accessible name `Credits left this period`, value text = the detail text below).
   - Big numeral `{pct}%` (accessible `{pct} percent of credits left`), shown in the accent color when `left < 0.15`
     (`LOW_SHARE`).
   - Facts line: `{compact(remaining)} of {compact(limit)} left` and, if a reset time is known, the reset label.
   - Reset label (`app/components/format.ts`): `days = ceil((nextResetUnix·1000 − now)/86 400 000)`; `≤ 0` →
     `Resets today`, `1` → `Resets tomorrow`, else `Resets in {days} days`.
   - Detail text (tooltip / accessible value): `{grouped(remaining)} of {grouped(limit)} characters left[. {reset}]`.
   - `compact` = locale compact notation, max 1 fraction digit (`12400 → "12.4K"`); `grouped` = locale grouping of the
     rounded number (`lib/format.ts:31-41`).

`creditSummary()` (`logic.ts:301-318`) is the pure equivalent used elsewhere: `limit = max(0, characterLimit)`,
`used = max(0, characterCount)`, `remaining = max(0, limit − used)`, `fractionLeft = limit > 0 ? remaining/limit : 0`,
`resetInDays = nextResetUnix ? max(0, ceil((nextResetUnix·1000 − now)/86 400 000)) : null`.

### 2.3 Voice & model (`app/settings/VoiceSection.tsx`)

Header action: `Test voice` (primary, play icon, busy while the request is in flight). It asks the core to read
```
Hi, I’m {settings.voiceName}. This is how Vox sounds when it reads to you — select any text and I’ll take it from there.
```
with `source = 'manual'`, `sourceApp = 'Vox'` (so the widget's source label reads `VOX`). The core toasts every read
failure itself (missing key, empty text, engine errors); only a transport failure is toasted here (cleaned error, id
`voice-test`). Busy ends when the read *starts* (the request resolves), not when it finishes.

**Current voice summary**: avatar mark derived from the voice name, kicker `Current voice`, name =
`settings.voiceName`, tags:
- if the current voice (`settings.voiceId`) is in the loaded list: `[voiceKind(v), ...voiceTags(v, 4)]`, empty entries
  dropped — so up to **5** badges, and unlike the Voices-page cards the use case is **not** skipped here (algorithms
  §4.3);
- else, while the voice list is loading: a placeholder;
- else (list loaded without it, failed, or not fetched because there is no key): `ElevenLabs voice`.
Button `Browse voices` → navigates to `#/voices`.

**Model** (select). Description = the selected model's description (when the model is known and has one), else
`Which ElevenLabs model synthesizes speech.` While models are loading and none are known, a placeholder replaces the
select entirely (no select is shown). Models are only fetched when a key is stored or in demo mode (§2.1); without
them the select offers just the current id (prettified). Options (`logic.ts:285-290`): every model `{modelId, name}`;
if the current id is not in the list it is prepended with a prettified label (so the select never shows blank).
`prettyModelId` (`logic.ts:268-283`): split on `_`/whitespace; a part `v<digits>` followed by an all-digit part becomes
`v<major>.<minor>`; other parts get an uppercase first letter. `eleven_turbo_v2_5 → Eleven Turbo v2.5`,
`eleven_v3 → Eleven v3`.

Meta badges under the row (when the model is known or loading failed):
- cost: `{Number(costFactor.toFixed(2))}× credits` when `costFactor` is finite and > 0; highlighted (accent) only when
  `> 1` ("all facts are neutral; only a model that costs more than the base rate is worth flagging");
- `{round(maxChars/1000)}K chars / request` when `maxChars` is set;
- `{n} languages` when the model has more than one language;
- on load error: warning text `Couldn’t load models: {error}`.

Changing the model (`setModel`, lines 57-62): if a language is pinned and the new model is known, has a non-empty
language list and does **not** contain the pinned language, the patch also resets the language:
`{modelId, languageCode: ''}`; otherwise `{modelId}`.

**Language** (select). Description `Auto-detect works for most text. Pin a language for short or mixed snippets.`
Options (`logic.ts:292-299`): `'' → Auto-detect`, then the selected model's languages sorted by name
(locale compare), value = language id; a current code not in the list is appended with its code uppercased as label.
Binds `languageCode` (core accepts `''` or `/^[a-z]{2,3}([-_][a-z0-9]{2,8})?$/` after lower-casing).

**Audio quality** (select) → `outputFormat`. Description `Lower bitrates download faster and take less cache space.`

| value | label |
|---|---|
| `mp3_44100_128` | High · 128 kbps |
| `mp3_44100_64` | Standard · 64 kbps |
| `mp3_22050_32` | Data saver · 32 kbps |

**Voice settings** sub-heading with `Reset` (ghost button, reset icon): disabled when all five values equal the
defaults (`stability 0.5, similarityBoost 0.75, style 0, speed 1, useSpeakerBoost true`); otherwise writes the whole
default `voice` object.

| Row | Description | Range / step | Display | Binds |
|---|---|---|---|---|
| Stability | `Higher is steadier and more even; lower is more expressive.` | 0–1 / 0.01 | `round(v·100)%` | `voice.stability` |
| Similarity | `How closely the output sticks to the original voice.` | 0–1 / 0.01 | `%` | `voice.similarityBoost` |
| Style | `Exaggerates the speaker’s style. Adds latency above 0.` | 0–1 / 0.01 | `%` | `voice.style` |
| Speaking speed | `Generated by ElevenLabs, so pitch stays natural.` | 0.7–1.2 / 0.01 | `v.toFixed(2)×` | `voice.speed` |
| Speaker boost (toggle) | `Sharper likeness to the voice, slightly slower to generate.` | — | — | `voice.useSpeakerBoost` |

Sliders commit on release (§1.7). Patches are partial (`{voice: {stability: x}}`).

### 2.4 Playback (`app/settings/PlaybackSection.tsx`)

**Playback speed** (segmented, full width). Description `Applied on your PC on top of the voice’s speaking speed.`
- Options (`logic.ts:146-155`): presets `PLAYBACK_RATES = [0.75, 1, 1.25, 1.5, 1.75, 2]`, plus the current rate if it
  is finite and not within 0.001 of a preset (it was set elsewhere — the widget, the browser bridge); sorted ascending.
  Label `{Number(r.toFixed(2))}×` (`1×`, `1.25×`), accessible label `{n} times speed`. Selected = the option within
  0.001 of the current rate.
- On change: persist `playbackRate` **and** immediately send the live player command `{type:'rate', rate}` so a read in
  progress changes speed now.

**Volume** (slider with a speaker icon). Description `Reading volume, independent of the system mixer.` 0–1, step
0.01, display `round(v·100)%`. On commit: persist `volume` and send `{type:'volume', volume}`.

Sub-heading `Text cleanup`:
- `Skip links` — `Don’t read web addresses out loud.` → `text.skipUrls`.
- `Skip email addresses` — `Leave out anything that looks like name@example.com.` → `text.skipEmails`.
- `Longest selection` (select) — `Longer selections are trimmed, so a stray Ctrl+A doesn’t spend your credits.` →
  `text.maxChars`. Options: presets `[2000, 5000, 10000, 20000, 50000, 100000]` plus the current value if different,
  sorted; label `{n with en-US grouping} characters` (`20,000 characters`). Core clamp 100–100 000.

### 2.5 Triggers (`app/settings/TriggersSection.tsx`)

Intro: `Windows can’t add items to other apps’ menus, so Vox offers a few ways to start reading. Mix and match.`

If `appInfo.nativeHookAvailable === false` (unknown counts as available): warning notice titled
`Mouse and keyboard hook unavailable`, body
`The right-click chip and the selection popup need it, so they’re paused. Shortcuts, the browser extension and the tray menu still work.`
and the two chip toggles below are disabled.

| Toggle | Description | Binds | Notes |
|---|---|---|---|
| `“Read with Vox” chip` | `Select text, then right-click it in any app. The chip appears right beside the menu — click it to listen.` | `triggers.rightClickChip` | Illustration: a document with a selection, the chip left of the cursor, a native-looking menu (`Copy`, `Search the web`, `Print`) opening to the lower right; dimmed when off or hook missing (`art.tsx:22-49`). |
| `Selection popup` | `A small Read button pops up right after you select text, without right-clicking.` | `triggers.selectionPopup` | Illustration: a selected line with a small `Read` pill above its end (`art.tsx:51-65`). |

Sub-heading `Keyboard`:
- `Global shortcuts` — `Work in every app, even when this window is closed.` → `triggers.hotkeysEnabled`.
- Four shortcut rows (`logic.ts:137-142`), each disabled (row and recorder) when global shortcuts are off:

| Action id | Label | Description | Default |
|---|---|---|---|
| `readSelection` | Read selection | Copies the selected text and reads it | `Ctrl+Alt+R` |
| `readClipboard` | Read clipboard | Reads whatever text is on the clipboard | `Ctrl+Alt+V` |
| `playPause` | Play / pause | Toggles the current reading | `Ctrl+Alt+P` |
| `stop` | Stop | Stops and clears the current reading | `Ctrl+Alt+S` |

  The control is the hotkey recorder (§3.5), accessible name `{label} shortcut`. While it records, the UI tells the
  core to **suspend all global hotkeys** (otherwise the OS hands the combination being recorded to the registered
  action); the core re-enables them when recording ends, and after **30 s** at most as a safety net
  (`src/main/index.ts:132-152`).

  Suspension details (`src/main/index.ts:132-152`): each "suspend" re-arms the 30 s safety timer (a second suspend
  while already suspended only restarts the timer); "resume" re-registers from the **current** settings. While
  suspended, settings changes do not re-register anything. Ordering matters: the recorder sends the new accelerator
  (settings write) **before** it reports "recording ended", so the resume re-registers with the new value — keep that
  order (or have resume read the store after the write lands). Known gap: a recording left open for more than 30 s gets
  the hotkeys back while still recording, so the OS can again swallow a registered combination.

  `setHotkey(action, accel)` (lines 27-36):
  - Every call first cancels the pending rejection-clear timer.
  - If `accel` is a reserved chord (§3.6): **not saved** (the old shortcut stays); show under that row, for **6000 ms**
    (role alert): `{chord} is used by Windows and other apps, so it can’t be a global shortcut. Pick another.` Only one
    rejection exists at a time: a rejection on another row replaces it.
  - Else save `{triggers: {hotkeys: {[action]: accel}}}` (`''` = cleared/disabled) and clear any rejection message
    (on any row) immediately.

  Persistent warnings under a row (first match):
  1. the transient rejection above (for this row);
  2. the stored value is reserved (e.g. hand-edited file):
     `{chord} is used by Windows and other apps (copying, pasting…). Pick another shortcut.`;
  3. the row's action shares its canonical shortcut with another action:
     `Same shortcut as another action — only one of them will fire.`
  `{chord}` = the display labels joined with `+` (`Ctrl+Insert` → `Ctrl+Ins`).

- `Media keys` — `Play / pause and skip with your keyboard’s media keys and the Windows media flyout. Takes effect after Vox restarts.`
  → `triggers.mediaKeys`. Core side effect: info toast `Restart Vox to finish turning media keys on|off`.

Sub-heading `Capture`:
- `Restore clipboard` — `Vox copies the selection to read it. This puts your previous clipboard back afterwards.` →
  `triggers.restoreClipboard`.
- `Excluded apps` (stacked row) — `Chips never appear in these programs — handy for games, design tools or remote desktops.`
  → `triggers.excludedApps` (array of lower-case exe names).
  - Current entries as removable chips in a list named `Excluded apps` (each with a `Remove {name}` button, same text
    as tooltip; removal writes the filtered array). No list is rendered when empty.
  - Add form: input (placeholder `photoshop`, trailing fixed suffix `.exe`, no spellcheck/autocomplete) + `Add`
    button (disabled while the trimmed input is empty). Typing clears the error. Enter submits.
  - Submit: empty after trim → ignored. Else `addExcludedApp` (`logic.ts:66-83`):
    ```
    normalizeExeName(input):
      name = input.trim(); strip leading/trailing '"' runs; trim
      name = last path segment after splitting on / or \ ; trim; lower-case
      if empty → invalid
      if !endsWith('.exe') → name += '.exe'
      invalid if /[<>:"|?*\u0000-\u001f]/ matches, or name == '.exe', or /[. ]\.exe$/ matches (trailing dot/space)
      invalid if length > 120
    ```
    invalid → error `Enter a program name like notepad.exe`; duplicate → error `{name} is already excluded`; else
    append, clear input. Error shown under the form (role alert).
  - Core re-normalizes (lower-case, last path segment, `.exe` appended, unique, max 200 entries).
  - `C:\Apps\Photoshop.EXE → photoshop.exe`, `slack → slack.exe`.

### 2.6 Widget (`app/settings/WidgetSection.tsx`)

Intro: `The floating player that shows what’s being read. Audio keeps playing even when it’s hidden.`

- `Show floating player` — `Turn off to listen without anything on screen — use shortcuts or media keys to control it.`
  → `widget.enabled`. Core: switching off hides the widget immediately; reads keep playing (the widget window still
  owns playback, just hidden); toasts then fall back to OS notifications for non-success kinds (`reader-playback.md`).
- `Position` (stacked row; **never disabled**). Description = the hint of the current position. A small screen
  diagram shows where the player sits (decorative). Segmented control `Widget position`:

| value | Label | Accessible label | Hint |
|---|---|---|---|
| `bottom-right` | Bottom right | — | `Above the clock, out of the way of most documents.` |
| `top-right` | Top right | — | `Top corner, clear of the taskbar and notifications.` |
| `bottom-center` | Bottom center | — | `Centered above the taskbar, like a media flyout.` |
| `near-cursor` | Cursor | Near the cursor | `Next to where you selected the text.` |
| `remember` | Last spot | Where I last dragged it | `Wherever you last dropped it. Drag the widget to move it.` |

  Core side effects: a change repositions a visible widget. **Dragging the widget by hand writes
  `{position: 'remember', customX, customY}`**, so the control jumps to "Last spot" after a drag
  (`src/main/index.ts:453-455`). Placement geometry: `shell.md` §6.3.4 (24 DIP margin from the work-area edges, etc.).
- `Compact pill` — `A slim one-line player instead of the full card.` → `widget.compact` (disabled when the player is
  off).
- `Highlight words` — `Follow along as each word is spoken.` → `widget.highlightWords` (disabled when off).
- `Hide when finished` — `Tucks the player away a few seconds after the reading ends.` → `widget.autoHide` (disabled
  when off). The delay is 4000 ms (§6.11).

### 2.7 Appearance (`app/settings/AppearanceSection.tsx`)

Intro: `Cards stay black in every theme; the theme changes the canvas around them.` (design-bound copy; revise with the
redesign).

Three preview tiles (a mouse shortcut only: not focusable and hidden from assistive tech) and a segmented control
`Theme` (the accessible control), both writing `app.theme`:

| value | Label | Glyph | Tile note |
|---|---|---|---|
| `system` | System | globe | `Follows Windows dark mode` |
| `paper` | Paper | sun | `Black cards on light grey` |
| `night` | Night | moon | `Black cards on pure black` |

The `system` tile previews paper and night split diagonally. Core side effect: sets the native theme source and window
background (`shell.md` §5.6).

### 2.8 Library & privacy (`app/settings/LibrarySection.tsx`)

Intro: `Everything stays on this PC. Text is only sent to ElevenLabs to be spoken.` Header badge: cache size
`formatBytes(bytes)` with the space removed (`187MB`), once measured.

Cache stats: measured on mount; re-measured **2500 ms after the last history-change event** (history changes are a cheap
proxy for "something was cached"; they also fire every couple of seconds while listening, and measuring walks the disk,
so the timer is restarted on every event — a trailing debounce). Stats `{reads, files, bytes}`; failure → `statsError`.
A failed re-measure keeps the previously measured stats on screen (the error text only appears if nothing was ever
measured); a later success clears the error.

| Row | Copy | Binds / action |
|---|---|---|
| Save history (toggle) | `Keep a list of what you’ve listened to, for replays and analytics.` | `library.saveHistory` |
| Store full text (toggle, disabled when history is off) | `Off keeps only the first 160 characters of each read.` | `library.storeText` |
| Keep history for (select, disabled when history is off) | `Older reads are deleted automatically. Favorites are always kept.` | `library.retentionDays` via the retention guard |

Retention options (`logic.ts:186-199`): `0 Keep forever`, `7 7 days`, `30 30 days`, `90 90 days`, `365 1 year`, plus
the current value if it is not a preset (`{n} days`). Core clamp 0–3650.

**Retention guard** (lines 241-319). A shorter retention deletes history the moment it is saved, so:
- `isStricter(next, current) = next > 0 && (current <= 0 || next < current)`, always against the **stored** value (a
  pending choice is never the baseline).
- Every choice first cancels any pending check, closes a confirmation and clears the pending value.
- Not stricter → save immediately (if changed; choosing the stored value just cancels whatever was pending).
- Stricter → the select shows the pending value; wait **450 ms** after the last change (`RETENTION_SETTLE_MS`; arrow
  keys on a closed select fire one change per option passed), then count the reads that would be deleted:
  non-favorites with `createdAt < now − days × 86 400 000` (same rule as the core's retention).
  - count 0 → save, clear pending.
  - count > 0 → confirmation dialog:
    - title `Delete {N} read{s} older than {retentionLabel(days)}?` — `N` locale-grouped; `retentionLabel`:
      `365 → '1\u00a0year'`, else `'{d}\u00a0day'`/`'{d}\u00a0days'` (no-break space so it never wraps);
    - description `Their saved audio goes too. Favorites are always kept. This can’t be undone.`;
    - buttons `Cancel` (initial focus) and `Delete {N} read{s}` (danger). Confirm → save; Cancel/close → revert the
      select to the stored value. The dialog keeps its last text while animating out.
  - count failure → revert, error toast titled `Couldn’t check your history` (cleaned error, id `library-retention`).
  - Any new choice cancels a pending check/confirmation (token).
- Counting algorithm in the source (`logic.ts:216-244`): binary search over the newest-first history listing with
  one-record pages, once for all reads and once for favorites only; `count = max(0, (all.total − all.firstOlderIndex) −
  (fav.total − fav.firstOlderIndex))`; `days ≤ 0` or non-finite → 0. A record missing at a probed offset (the list
  shrank meanwhile) is treated as "older". It exists only because the UI could page the history over IPC; a native port
  should ask the store for the count directly (`favorite = false AND created_at < now − days·86 400 000`).

Sub-heading `Audio cache`:
- `Cache audio` (toggle) — `Replays from history play instantly and cost no credits.` → `library.cacheAudio`.
- `Cache size limit` (slider) — `Oldest audio is removed first when the cache is full.` Range `min 50`,
  `max max(5000, current)` (a hand-edited value above 5000 extends the slider; core allows up to 10 000), step 50;
  shown value `min(5000, current)`; display `formatMegabytes`. → `library.cacheLimitMB`. Core: lowering it prunes the
  cache immediately (least recently used first, never the playing read).
  Quirk (`LibrarySection.tsx:157-166`): with a hand-edited value above 5000 (say 6000) the track runs to 6000 but the
  knob and label sit at 5000 (`5 GB`) while the `In use` row says `of 6 GB`; and because SliderRow only commits values
  different from the *shown* 5000, releasing exactly on 5000 writes nothing. Decide whether the redesign keeps this
  (§11).
- `In use` row:
  - description: `{formatBytes(bytes)} of {formatMegabytes(limitMB)} · {reads} read{s} cached` | `Couldn’t read the cache size.` | `Measuring…`;
  - meter: `usage = min(1, bytes / max(1, limitMB × 1 000 000))`, accent tone when `usage > 0.9`, accessible name
    `Audio cache used`, value `{round(usage·100)} percent of the limit`;
  - button `Clear cache` (busy while clearing; disabled until measured or when `bytes == 0`) → core `cache.clear()`
    returns new stats → success toast `Audio cache cleared` (id `library-cache`).

```
formatBytes(b):  b = max(0, finite(b) ? b : 0)            // decimal units
  b < 1e6  → b == 0 ? '0 MB' : `${max(1, round(b/1000))} KB`
  b < 1e9  → `${b < 1e7 ? (b/1e6).toFixed(1) : round(b/1e6)} MB`
  else     → `${(b/1e9).toFixed(1)} GB`
formatMegabytes(mb): mb < 1000 → `${round(mb)} MB`; else gb = mb/1000 → integer ? `${gb} GB` : `${gb.toFixed(1)} GB`
```
(`187_400_000 → '187 MB'`, `1_600_000_000 → '1.6 GB'`, `500 → '500 MB'`, `1500 → '1.5 GB'`.)

Sub-heading `Your data`:
- `Export history` — `Save every read, with text and stats, as a JSON file.` Button `Export…` (busy) → core shows a
  save dialog and writes the file; returns the path or null (cancelled). Path → success toast titled
  `History exported` whose message is the path (id `library-export`). Null → nothing.
- `Clear history` — `Deletes every read and the analytics built from them.` Button `Clear history` (danger) →
  confirmation: title `Clear history?`, description
  `Every read, favorite and listening stat will be deleted. This can’t be undone.`, buttons `Cancel` (disabled while
  clearing) and `Clear history` (danger, busy); close ignored while clearing. Confirm → core `history.clear()` → close
  dialog, success toast `History cleared` (id `library-history`). On failure the dialog **stays open** (buttons
  re-enabled) behind the error toast.
- Any failure of these three actions → error toast with the cleaned error, id `library-{cache|export|history}`. The UI
  keeps a single busy slot (`busy ∈ {cache, export, history, none}`) and the buttons do not disable each other: starting
  a second action while one runs moves the busy look to it, and whichever finishes first clears the slot. Give each
  action its own busy flag in the port.

### 2.9 Integrations (`app/settings/IntegrationsSection.tsx`)

Intro: `Real “Read with Vox” menu entries where Windows allows them, and how Vox starts with your PC.`

Sub-heading `Browser extension` with a status badge (first match):
1. bridge setting off → `Off`;
2. app info not loaded yet, or a change is being re-checked → `Checking…`;
3. `appInfo.serverRunning` → `Listening on {port}` (success) where `port = appInfo.serverPort`;
4. else `Not running` (attention).

"Re-check": after a change that affects the status, show `Checking…` and refresh app info after a delay; `Checking…`
ends when new app info arrives or after **4000 ms** (app info can fail to load; never leave the badge stuck).

| Control | Copy | Binds | Re-check delay |
|---|---|---|---|
| `Chrome & Edge bridge` (toggle) | `Lets the bundled extension add “Read with Vox” to the browser’s own right-click menu. Only reachable from this PC.` | `triggers.browserExtension` | 700 ms |
| `Port` (text input) | `Change it only if another app already uses it, then enter the same port in the extension’s popup.` | `server.port` | 900 ms |
| `Explorer right-click` (toggle) | `Adds “Read with Vox” to text files (.txt .md .log .csv .json .rtf .html). On Windows 11 it’s under ` + emphasized `Show more options` + `.` | `triggers.explorerMenu` | 1200 ms |
| `Launch at login` (toggle) | `Start Vox quietly in the tray when you sign in to Windows.` | `app.launchAtLogin` | — |
| `Start minimized` (toggle) | `Open straight to the tray instead of showing this window.` | `app.startMinimized` | — |
| `Close to tray` (toggle) | `Closing the window keeps Vox running so shortcuts and chips keep working.` | `app.closeToTray` | — |

Port input (lines 151-215), disabled when the bridge is off:
- Monospace, numeric keyboard hint. Every keystroke strips non-digits and truncates to 5 characters; typing clears the
  error. Whenever the stored port changes (another window, a reset, its own save) the draft is reset to it — even
  mid-edit. An invalid draft is **not** reverted on blur; it stays with the error until edited or Escape.
- Apply on blur or Enter: if the trimmed draft equals the stored port → clear error, nothing else. Else
  `parsePort`: `/^\d{1,5}$/` and `1024 ≤ n ≤ 65535` (unprivileged ports only) — invalid → error
  `Use a port between 1024 and 65535.` (role alert); valid → save and re-check.
- Escape: revert to the stored port, clear error.
- Note under the field (error has priority): when app info is loaded, not re-checking, bridge on and the server is not
  running → `Port {stored port} may be in use by another app. Try another one.` The core also toasts
  `Browser bridge couldn't use port {port}. Pick another port in Settings` with an `Open settings` action to
  `#/settings/integrations`.

`Install the extension` (stacked row) — `One-time setup in Chrome, Edge, Brave or any Chromium browser.` Numbered steps:
1. `Open ` [`chrome://extensions` copy chip] ` or ` [`edge://extensions` copy chip]
2. `Turn on ` **Developer mode** ` in the corner of the page`
3. `Click ` **Load unpacked** ` and choose the extension folder` + button `Show extension folder` (core
   `revealPath('extension')`: opens the bundled extension folder in Explorer; errors ignored).

Copy chip: the text as a keycap + a button `Copy {text}` → writes the text to the clipboard; the button shows a check
and the label `Copied` for **1400 ms**; failure → error toast `Couldn’t copy to the clipboard` (id `copy`).

Sub-heading `Windows` with badge `Installed` (success) when `appInfo.explorerMenuInstalled`.

Core side effects (`shell.md` §4-5): bridge on/off/port → start/stop/restart the localhost server; Explorer toggle →
install/remove the registry verb (failure toast `Couldn't update the Explorer menu[: error]`); launch-at-login /
start-minimized → autostart registration.

### 2.10 About (`app/settings/AboutSection.tsx`)

Header badge `Demo mode` when demo. Hero: logo, `VOX` (app name uppercased), tagline
`Select text anywhere. Hear it in a voice you like.`, version badge `V{version}` (placeholder until info loads).

| Row | Content |
|---|---|
| Version | `{version}` + ` · dev` when not packaged; `…` until loaded |
| Electron | runtime version; `…` until loaded (replace in the port, §11) |
| Data folder | description = full user-data path (tooltip = same), or `Settings, history and cached audio` before load; button `Open` (disabled until loaded) → core `revealPath('userData')`, errors ignored |
| Demo data (demo only) | `You’re looking at sample history in a separate data folder. Your real library is untouched.` |
| Links | ghost buttons opening the browser (errors ignored): `ElevenLabs dashboard` → `https://elevenlabs.io/app`; `Usage & billing` → `https://elevenlabs.io/app/subscription`; `API docs` → `https://elevenlabs.io/docs` |

Fine print: `Vox is not affiliated with ElevenLabs. Speech is generated with your own API key and billed to your ElevenLabs account.`

### 2.11 Section header badges and other per-section state at a glance

| Section | Right of header |
|---|---|
| Account | Connected / Demo account / No key |
| Voice & model | `Test voice` button |
| Library & privacy | cache size badge (`187MB`) |
| About | `Demo mode` badge (demo only) |

### 2.12 Settings binding table

UI range = what the control offers; core clamp = what the store enforces (`src/main/services/settings-store.ts:98-170`).

| Key | Default | Control | UI range / options | Core clamp | Live side effects |
|---|---|---|---|---|---|
| `voiceId`, `voiceName` | `JBFqnCBsd6RMkjVDRZzb`, `George` | Voices page "Use voice", onboarding | — | non-empty, ≤200 chars | next read |
| `modelId` | `eleven_flash_v2_5` | Model select | models list + current | non-empty | may also reset `languageCode` |
| `languageCode` | `''` | Language select | `''` + model languages | `''` or `/^[a-z]{2,3}([-_][a-z0-9]{2,8})?$/` | next read |
| `outputFormat` | `mp3_44100_128` | Audio quality | 3 formats | one of 3 | next read |
| `voice.stability` | 0.5 | slider | 0–1 / 0.01 | 0–1 | next read |
| `voice.similarityBoost` | 0.75 | slider | 0–1 / 0.01 | 0–1 | next read |
| `voice.style` | 0 | slider | 0–1 / 0.01 | 0–1 | next read |
| `voice.speed` | 1 | slider | 0.7–1.2 / 0.01 | 0.7–1.2 | next read |
| `voice.useSpeakerBoost` | true | toggle | — | bool | next read |
| `playbackRate` | 1 | segmented; widget rate button | presets + current | 0.5–3 | live `rate` command |
| `volume` | 1 | slider | 0–1 / 0.01 | 0–1 | live `volume` command |
| `text.skipUrls` | true | toggle | — | bool | next read |
| `text.skipEmails` | false | toggle | — | bool | next read |
| `text.maxChars` | 20000 | select | 6 presets + current | 100–100 000 int | next read |
| `triggers.rightClickChip` | true | toggle | disabled w/o hook | bool | selection service |
| `triggers.selectionPopup` | false | toggle | disabled w/o hook | bool | selection service |
| `triggers.hotkeysEnabled` | true | toggle | — | bool | re-register hotkeys |
| `triggers.hotkeys.*` | see §2.5 | recorder | reserved refused | trimmed, ≤64 chars | re-register hotkeys |
| `triggers.mediaKeys` | true | toggle | — | bool | restart-needed toast; widget media session |
| `triggers.restoreClipboard` | true | toggle | — | bool | capture |
| `triggers.excludedApps` | `[]` | chip list | normalized exe names | normalized, unique, ≤200 | selection service |
| `triggers.browserExtension` | true | toggle | — | bool | start/stop bridge |
| `triggers.explorerMenu` | false | toggle | — | bool | registry verb |
| `server.port` | 47823 | text | 1024–65535 | 1024–65535 int | restart bridge |
| `widget.enabled` | true | toggle | — | bool | hide widget when off |
| `widget.position` | `bottom-right` | segmented | 5 values | one of 5 | reposition if visible |
| `widget.customX/Y` | null | (drag only) | — | int ±100 000 or null | — |
| `widget.compact` | false | toggle | disabled when widget off | bool | widget layout |
| `widget.highlightWords` | true | toggle | disabled when widget off | bool | widget text |
| `widget.autoHide` | true | toggle | disabled when widget off | bool | widget auto-hide |
| `app.theme` | `system` | tiles + segmented; top-bar toggle | 3 modes | one of 3 | native theme, window bg |
| `app.launchAtLogin` | false | toggle | — | bool | autostart |
| `app.startMinimized` | false | toggle | — | bool | autostart args |
| `app.closeToTray` | true | toggle | — | bool | close behavior |
| `library.saveHistory` | true | toggle | — | bool | — |
| `library.storeText` | true | toggle | disabled when history off | bool | — |
| `library.retentionDays` | 0 | select + guard | 0/7/30/90/365 + current | 0–3650 int | retention run |
| `library.cacheAudio` | true | toggle | — | bool | — |
| `library.cacheLimitMB` | 500 | slider | 50–max(5000,cur) / 50 | 50–10 000 int | prune when lowered |
| `onboarded` | false | onboarding finish/skip | — | bool | shell leaves onboarding |

---

## 3. Hotkeys: recording, formatting, validation (`design/hotkey.ts`, `design/HotkeyRecorder.tsx`, `app/settings/logic.ts:85-135`)

### 3.1 Accelerator strings

Format: modifiers then one key, joined with `+`, e.g. `Ctrl+Alt+R`. Empty string = not set / disabled. Modifiers
produced by the recorder, in this order: `Ctrl`, `Alt`, `Shift`, `Super` (the Windows key). Key names produced:

- Letters `A`–`Z` (uppercase), digits `0`–`9`, numpad digits `num0`–`num9`, `F1`–`F24`.
- Named keys (by physical key code → name), `hotkey.ts:39-81`:

| Physical key | Name | Physical key | Name |
|---|---|---|---|
| Space | `Space` | Minus | `-` |
| Enter, NumpadEnter | `Enter` | Equal | `=` |
| Tab | `Tab` | BracketLeft / Right | `[` / `]` |
| Backspace | `Backspace` | Backslash, IntlBackslash | `\` |
| Delete | `Delete` | Semicolon | `;` |
| Insert | `Insert` | Quote | `'` |
| Home / End | `Home` / `End` | Backquote | `` ` `` |
| PageUp / PageDown | `PageUp` / `PageDown` | Comma / Period / Slash | `,` `.` `/` |
| Arrow keys | `Up` `Down` `Left` `Right` | NumpadAdd/Subtract/Multiply/Divide/Decimal | `numadd` `numsub` `nummult` `numdiv` `numdec` |
| Escape | `Escape` | MediaPlayPause / MediaStop | `MediaPlayPause` / `MediaStop` |
| PrintScreen | `PrintScreen` | MediaTrackNext / Previous | `MediaNextTrack` / `MediaPreviousTrack` |
| | | AudioVolumeUp / Down / Mute | `VolumeUp` / `VolumeDown` / `VolumeMute` |

- Keys that may be used **without** Ctrl/Alt/Win (`STANDALONE`):
  ```
  /^(F([1-9]|1[0-9]|2[0-4])|Media.*|Volume.*|PrintScreen)$/
  ```
- Splitting (`splitAccelerator`, `hotkey.ts:247-268`) handles a literal plus key. Exact algorithm:
  ```
  s = trim(accelerator); if s == '' → []
  parts = []; cur = ''
  for i in 0..len(s):
    ch = s[i]
    if ch == '+' and cur != '':  parts.push(cur); cur = ''          // separator
    elif ch == '+' and cur == '': parts.push('+')                     // '+' at start / right after a separator = the plus key
                                  if s[i+1] == '+': i += 1            // swallow the separator that follows it
    else: cur += ch
  if cur != '': parts.push(cur)                                       // a trailing separator is dropped
  ```
  `Ctrl++` → `['Ctrl','+']`, `Ctrl+++` → `['Ctrl','+']`, `+` → `['+']`, `Ctrl+` → `['Ctrl']`, `Ctrl+Plus` →
  `['Ctrl','Plus']` (aliases are folded later, §3.6). Parts keep their spelling and spaces inside a part are kept.

### 3.2 Display labels (`acceleratorLabels`, `hotkey.ts:204-279`)

Each part, compared lower-case:

| Part(s) | Label | Part(s) | Label |
|---|---|---|---|
| commandorcontrol, cmdorctrl, control, ctrl | `Ctrl` | pageup / pagedown | `PgUp` / `PgDn` |
| command, cmd | `Cmd` | delete / insert | `Del` / `Ins` |
| alt, option | `Alt` | backspace | `Backspace` |
| altgr | `AltGr` | printscreen | `PrtSc` |
| shift | `Shift` | numadd / numsub / nummult / numdiv / numdec | `Num +` / `Num −` / `Num ×` / `Num ÷` / `Num .` |
| super, meta | `Win` | num0–num9 | `Num 0`–`Num 9` |
| plus | `+` | mediaplaypause | `Play/Pause` |
| space | `Space` | medianexttrack / mediaprevioustrack | `Next track` / `Prev track` |
| return, enter | `Enter` | mediastop | `Media stop` |
| escape, esc | `Esc` | volumeup / volumedown / volumemute | `Vol +` / `Vol −` / `Mute` |
| up / down / left / right | `↑` `↓` `←` `→` | other single character | uppercased |
| | | other | as written |

Lookup order per part: the table above (lower-cased), then `/^num([0-9])$/` → `Num {d}`, then a one-character part is
uppercased, else the part is shown exactly as written (`F5`, `Tab`, `Home`, `End`). Keycaps render one cap per label;
empty → `Not set`. Spoken form: labels joined with ` plus ` and a `+` label read as `Plus` (`Ctrl plus Alt plus R`).
The `{chord}` text in Settings warnings is the labels joined with `+` (`Ctrl+Ins`, `Ctrl+Shift+Esc`).

### 3.3 From a key press to an accelerator (`acceleratorFromEvent`, `hotkey.ts:180-202`)

Inputs per key event: physical key code (`code`), produced character/key name (`key`), modifier states, and the active
layout's base character per physical key (optional).

```
mods = [Ctrl if ctrl][Alt if alt][Shift if shift][Super if meta/win]
if code is a modifier key (Control/Alt/Shift/Meta/OS left|right) or key ∈ {Control, Alt, Shift, Meta, OS, AltGraph}:
    → partial(mods)                              // live preview of held modifiers
key = keyFromEvent(code, key, layout); none → unsupported
strong = ctrl || alt || meta
if requireModifier (default true) && !strong && key !~ STANDALONE → needs-modifier(key)   // Shift alone is not enough
accelerator = mods + key joined '+'
if altGrCharacter(event, layout) → types-character(accelerator, char)
→ ok(accelerator)
```

`keyFromEvent` (`hotkey.ts:148-169`): the global-hotkey API registers **virtual-key codes**, and Windows assigns letter
VKs by the layout's labels (on QWERTZ the key labelled Z — physical `KeyY` — is `VK_Z`). So for letter keys and
punctuation keys (which carry letters on some layouts, e.g. AZERTY's `M` where US has `;`):
1. if the produced character is an ASCII letter → that letter uppercased;
2. else if the layout's base character for that physical key is an ASCII letter → it (Ctrl+Alt acts as AltGr and turns
   `q` into `@`, so the produced character is useless);
3. else fall back to the physical (US-position) name: `KeyX → X`, digits by position (AZERTY's digit row still sends
   `VK_1..VK_0` although it types `&`, `é`…), non-Latin layouts (Russian, Greek) keep the US-position letter, which is
   what Windows uses for their VKs.
Non-letter keys always use the physical-key table.

**Native shortcut:** in a Win32 port the window procedure already receives the VK (`wParam` of
`WM_KEYDOWN`/`WM_SYSKEYDOWN`), which *is* the layout-following value this logic reconstructs. Use the VK for letters and
digits directly; use the scan code (lParam bits 16–23 + extended bit 24) where the physical position matters (numpad vs
main row, NumpadEnter vs Enter).

### 3.4 AltGr guard (`altGrCharacter`, `hotkey.ts:112-131`)

On Windows Ctrl+Alt **is** AltGr. If on the active layout the combination types a character (AltGr+V is `@` on
Hungarian, AltGr+S is `ś` on Polish), a global shortcut on it would stop the user typing that character anywhere, so the
recorder refuses it. Rule:
```
only when ctrl && alt && !meta and the produced key is "printable":
    exactly one Unicode code point, not whitespace, not in general category C* (Cc control, Cf format, Cs surrogate,
    Co private use, Cn unassigned) — so named keys like "Enter" (more than one code point) never count
not one if the produced character is an ASCII letter or digit (/^[a-z0-9]$/i)
if shift && the physical key is not a letter key (code !~ /^Key[A-Z]$/) → not one (can't tell)
if the layout map has a base char for the key: it is one iff lower(produced) != lower(base)   (layouts without an
     AltGr layer, e.g. Russian, report the base character)
else (no layout map, or the lookup threw): only letter keys whose produced character is not a Unicode letter (\p{L})
     count — e.g. '@' on KeyV counts, 'ś' on KeyS does not without the map
→ the result is the produced character, shown in the hint
```
The check runs after the `needs-modifier` check and only for keys that produced an accelerator.
Native: call `ToUnicodeEx(vk, scan, keystate with VK_CONTROL|VK_MENU (and VK_SHIFT) down, …, hkl)` for the active
`GetKeyboardLayout(0)`; a printable result that differs from the unmodified result means "types a character". The core
applies the same guard again at registration (`shell.md` §8.4).

### 3.5 Recorder control behavior (`HotkeyRecorder.tsx`)

Props: `value` (accelerator or `''`), `onChange(accel | '')`, accessible `label`, `requireModifier` (default true),
`onRecordingChange(bool)`, `disabled`, placeholder (default `Not set`).

States: **idle** and **recording**.

- Recording **starts** when the control gains focus (including via Tab) or on mouse press (only if not already
  recording); also on Enter/Space when it is focused but idle (e.g. after Escape). A disabled recorder never starts
  (and cannot be focused). Each start re-reads the keyboard layout map (the user may have switched layouts) —
  asynchronously, so until the read completes the previous recording's map (or none) is used — and clears the preview
  and hint.
- Recording **stops** on blur, on success, on Escape (cancel) and on clear. Stopping clears the modifier preview but
  **not** the hint: a hint flashed just before keeps being announced until its 1.8 s timer ends.
- `onRecordingChange` fires on every change of the recording state (not on mount); if the control disappears while
  recording, it fires `false` (so the core's hotkeys come back).

Key handling while idle (focused): Enter/Space (any modifiers) → start; bare (no Ctrl/Alt/Shift/Win) Backspace/Delete
with a value → `onChange('')` (clear). Side effect of not checking modifiers: holding a just-recorded chord that ends in
Enter or Space auto-repeats into the idle handler and restarts recording.

Key handling while recording (`onKeyDown`, lines 96-127):
1. Tab (without Ctrl/Alt/Win; Shift+Tab included) is let through so focus can leave (blur stops recording).
2. Everything else is swallowed (no default action, no propagation).
3. Bare Escape → stop, nothing changes. With a modifier, Escape is just a key (`Ctrl+Escape` is produced, then refused
   by Settings as reserved).
4. Bare Backspace/Delete → `onChange('')` (even when already empty), stop. With modifiers they are ordinary keys
   (`Ctrl+Backspace` is a valid shortcut; `Shift+Backspace` → needs-modifier).
5. Otherwise `acceleratorFromEvent` with the layout map:
   - `partial` → preview the held modifiers;
   - `needs-modifier` → flash hint `Add Ctrl, Alt or Win`;
   - `unsupported` → flash hint `That key can’t be used`;
   - `types-character` → flash hint `On this keyboard that types “{char}”`;
   - `ok` → `onChange(accel)`, stop.
6. Key-up while recording recomputes `partial` (without the layout map) so releasing a modifier updates the preview.

Hints flash for **1800 ms**. Display while recording: the held modifiers as keycaps (`Super` shown as `Win`) followed by
`+ …`; with no modifiers held, the hint or `Press keys…`. Display while idle: the value's keycaps or the placeholder.
Accessible name `{label}: {spoken value | placeholder}`; polite status text: the hint if any, else
`Recording. Press a key combination, Escape to cancel, Backspace to clear.` /
`Press Enter to change, Backspace to clear.`

Note: a hint flashed while modifiers are still held is only announced, not drawn (the preview wins). Keep or fix in the
redesign.

### 3.6 Canonical comparison, conflicts, reserved chords (`logic.ts:85-135`, `src/shared/constants.ts:22-41`)

```
ALIASES = { control:'ctrl', cmdorctrl:'ctrl', commandorcontrol:'ctrl', cmd:'super', command:'super', meta:'super',
            win:'super', option:'alt', altgr:'alt', plus:'+', return:'enter', esc:'escape' }
MODIFIER_ORDER = ['ctrl', 'alt', 'shift', 'super']
acceleratorKey(a) = parts(a) → lower-case → ALIASES → modifiers sorted by MODIFIER_ORDER and de-duplicated,
                    then the non-modifier parts in original order; joined '+'
```
`parts(a)` is `splitAccelerator` (§3.1). Consequences of the aliases: `Win`, `Meta`, `Cmd`, `Command` and `Super` are
the same modifier; `AltGr` compares equal to `Alt`; `Plus` equals `+`; `Return` equals `Enter`; `Esc` equals `Escape`.
Key parts are compared lower-cased, so `ctrl+alt+r` equals `Ctrl+Alt+R`. Modifier order is ignored (`Alt+Ctrl+R` =
`Ctrl+Alt+R`); duplicate modifiers collapse; non-modifier parts keep their order.

- **Conflicts:** actions whose non-blank (after trim) shortcuts have equal canonical keys are all flagged; blank
  shortcuts never conflict. Conflicts are allowed (warned), not refused.
- **Reserved:** a non-blank accelerator whose canonical key equals that of any entry below is refused by Settings (and
  skipped by the core if it appears in a hand-edited file):
  ```
  Ctrl+C, Ctrl+Insert, Ctrl+V, Ctrl+X, Ctrl+Z, Ctrl+Y, Ctrl+A, Ctrl+S, Alt+F4, Alt+Tab, Ctrl+Escape, Ctrl+Shift+Escape
  ```
  Reason: registering one hijacks it in every app while Vox runs, and Ctrl+C / Ctrl+Insert are also the copy chords Vox
  sends to capture a selection (a hotkey on them swallows the copy).

---

## 4. Voices page (`app/pages/Voices.tsx`, `app/settings/VoiceCard.tsx`, `app/settings/useVoicePreview.ts`)

### 4.1 Data

- Voice: `{voiceId, name, category?, description?, previewUrl?, labels: map<string,string>}` (labels: `accent`, `age`,
  `gender`, `use_case` / `use case`, `descriptive`, `description`, …).
- Fetched from the core (`eleven.voices({refresh})`) as soon as settings are loaded (always tried: demo mode serves
  voices without a key), keyed by `hasApiKey` (refetch when a key is added/removed). Refresh button → reload with
  `refresh=true`.
- A failed **refresh** while a list is already shown keeps the old list and shows an error toast titled
  `Couldn’t refresh voices` with the error (id `voices-refresh`).

### 4.2 Header and toolbar

- Kicker `ElevenLabs library`, title `Voices`.
- Badges: `In use: {name}` (once settings are loaded; name from the list if found, else `settings.voiceName`); count:
  `{shown} of {all}` when any filter field is non-empty (a whitespace-only query counts as "filtered" although it
  matches everything, giving `N of N`), else `{all} voices` (only when the list is non-empty).
- Toolbar (a search landmark):
  - Search field, accessible name `Search voices`, placeholder `Search name, accent, style…` (Esc clears, §1.8). Filters
    as you type (no debounce).
  - Gender segmented `Gender` (only if any voice has a gender label): `All` (`''`) + each gender facet.
  - Accent select `Accent` (only if any accent): `All accents` + each accent facet.
  - Type select `Voice type` (only if **more than one** category exists): `All types` + each category facet.
  - For all three: option value = facet lower-cased, label = `titleCase(facet)` (`american` → `American`,
    `middle_aged` → `Middle aged`). Comparison is case-insensitive anyway (`norm`).
  - Refresh icon button `Refresh voices` (disabled while loading or before settings load; spins while loading).
- Filters are page state only (not persisted); they reset when the page is left.

### 4.3 Sorting, filtering, facets, tags (`logic.ts:333-419`)

```
norm(s) = s.trim().toLowerCase();  label(v,k) = (v.labels[k] ?? '').trim()

sortVoices: rank = {cloned:0, professional:1, generated:2, premade:3}[norm(category)] ?? 2.5
            order by rank, then name (locale compare)          // the user's own voices first

filterVoices(v, f):
  gender set   → norm(label(v,'gender')) == norm(f.gender)
  accent set   → norm(label(v,'accent')) == norm(f.accent)
  category set → norm(category) == norm(f.category)
  terms = norm(query) split on whitespace (non-empty)
  every term must be a substring of norm(join(' ', [name, description ?? '', category ?? '', ...all label values]))

voiceFacets: for gender labels, accent labels, categories: distinct by norm(), keeping the first-seen spelling,
             sorted (locale compare)

titleCase(s) = s with [_-]+ → ' ', trimmed, first character uppercased (rest unchanged)

voiceTags(v, max=4, skip=[]):
  order = ['accent','age','gender','use_case','use case','descriptive','description'] minus skip
  push titleCase(trim(value)) for each key in order whose value is non-empty, then for every other label key (map
  order) not in order and not skipped; drop empty results and duplicates (case-insensitive); return the first `max`

voiceKind(v) = c = norm(category); '' if c empty or 'premade'; 'Designed' if 'generated'; else titleCase(c)
              // e.g. 'cloned' → 'Cloned', 'professional' → 'Professional'
voiceCaption(v) = join(' · ', non-empty [voiceKind(v), titleCase(label 'use_case' || label 'use case')])
CAPTION_LABEL_KEYS = ['use_case', 'use case']     // tags skip these on cards because the caption shows them
```

### 4.4 Content states (first match)

| Condition | Content |
|---|---|
| settings not loaded, or loading/idle with an empty list | 8 skeleton cards (`Loading voices`) |
| failed with an empty list and **no key** | empty state, glyph voice: `Add your ElevenLabs key to browse voices` / `Voices come straight from your ElevenLabs account — the premade library plus any voices you’ve cloned or designed.` / primary button `Add API key` → `#/settings/account` |
| failed with an empty list | empty state, glyph alert: `Couldn’t load your voices` / {error} / `Try again` (reload with refresh) |
| list empty | glyph mic: `No voices yet` / `Add or clone a voice in ElevenLabs, then refresh this page.` |
| filters match nothing | glyph search: `No voices match` / `Try a different search or clear the filters.` / `Clear filters` (resets all four filters) |
| otherwise | grid of voice cards (`Voices`) in sorted order |

### 4.5 Voice card (`VoiceCard.tsx`)

- Avatar mark from the name.
- Preview button, **only if `previewUrl`** (some clones have no sample): idle → play icon, `Play {name} sample`;
  loading → loader, `Stop {name} sample`; playing → animated wave, pressed, `Stop {name} sample`.
- Name; up to **3** tags (`voiceTags(v, 3, CAPTION_LABEL_KEYS)`); description trimmed or `No description.`
- Footer: caption (`voiceCaption`), then either an `In use` badge with a check (current voice) or a `Use voice` button
  (accessible `Use {name}`).
- The card is marked "current" when in use and "playing" while its sample plays. Accessible: each card is an article
  named by the voice name, inside a list named `Voices`.

### 4.6 Sample preview engine (`useVoicePreview.ts`)

One-at-a-time playback of ElevenLabs sample URLs through a single player; **samples and readings never overlap**.

- `toggle(voiceId, url)`: if this voice is the current sample → stop. Else stop any sample; if the reader's status is
  `playing` or `loading`, send the player command `pause`; create a fresh player with `preload=auto`, volume =
  `clamp(settings.volume, 0, 1)`; mark this voice **loading**; start streaming `url`.
  - When audio actually starts → loading cleared, voice **playing**.
  - Ended → stop.
  - Load/decode error → stop + error toast `Couldn’t play this sample. Check your connection.` (id `voice-preview`).
  - Play request rejected (other than "aborted because we stopped it ourselves") → stop + error toast
    `Couldn’t play this sample.` (id `voice-preview`).
  - Events from a superseded player are ignored.
- `stop()`: pause, drop the source and **abort the download**; clear playing/loading.
- Reader tracking: the tracked status starts as `idle`; the initial reader status is fetched once (ignored if a live
  state event already arrived first); on each state event, **only a transition into `playing`** (from anything else)
  stops a sample (whether it is still loading or already playing) — state reports already in flight when we paused the
  reading still say `playing` and must not cut the sample just started. The pause-on-start decision in `toggle` uses
  this tracked status.
- Volume changes apply live to the sample in progress.
- Leaving the page stops the sample.

### 4.7 Use voice

`update({voiceId, voiceName: name})` (optimistic, §1.1) and immediately a success toast
`Reading with {name} from now on` (id `voice-use`). The toast is shown before the save is confirmed; a failed save
shows the settings error toast and reverts.

---

## 5. Onboarding (`app/pages/Onboarding.tsx`, `app/components/onboarding/*`)

### 5.1 When it shows (`app/App.tsx:78, 102-112`)

Once settings are loaded: shown when the route page is `onboarding` **or** `settings.onboarded == false` (the design
gallery route excepted). It replaces the whole shell (top bar in minimal form, no sidebar); toasts at bottom-center;
the shell's page shortcuts are off. The core opens the main window on `onboarding` when not onboarded.

### 5.2 Frame

Steps (`types.ts:3, 16-22`): `welcome` (Welcome) → `key` (API key) → `voice` (Voice) → `try` (Try it) → `done` (Done).
Optional initial step (default `welcome`; an unknown step falls back to index 0); the furthest-visited index starts at
the initial step. The card is a region named `Vox setup`. Settings writes here go straight to the core (no optimistic
overlay, no Saving badge).

- Header: breadcrumb (logo, `Setup`, `{index+1}/5`) — hidden on the welcome step; `Skip setup` (ghost) on every step
  except the last, disabled while finishing → **finish**.
- Footer start: `Back` (ghost, left arrow) on every step except the first and the last.
- Footer center: progress dots, one per step (`Setup progress`), accessible `Step {n}: {label}`, the current one marked
  current; a dot is clickable only if it is not the current step and its index ≤ the furthest step visited.
- Footer end, primary action:
  - label: welcome → `Get started`; key step without a key → `Skip for now` (secondary style); last step → `Open Vox`;
    else `Continue`; right arrow on all but the last;
  - last step → **finish** (busy while finishing); else next step.
- Navigation clamps to the range; records direction (forward/back, for the transition) and the furthest visited index.
- Focus moves to the new step's heading on every step change (not on first paint) so screen readers announce it.
- The API key draft lives in the frame, so Back/Continue never wipes what was typed.
- **Hash redirect:** while onboarding, any navigation to `#/settings/account` (e.g. a core toast's `Add key` action)
  jumps to the key step instead (the shell keeps showing onboarding whatever the route says, so it would otherwise do
  nothing visible).
- **finish:** ignore if already finishing; write `{onboarded: true}`; navigate to `home`. Failure → re-enable, error
  toast `Couldn't save your setup: {message}` (no toast id; `message` is the raw error text, not cleaned — §1.4).

### 5.3 Steps

**Welcome** (`StepWelcome.tsx`): animated logo, heading `VOX` (accessible `Vox`), tagline
`Select any text. Hear it read aloud.`, body
`Vox turns anything on your screen into natural speech with ElevenLabs voices. Setup takes about a minute.`

**API key** (`StepApiKey.tsx`): eyebrow `ElevenLabs`, heading `Connect your account`, body
`Vox speaks with your own ElevenLabs API key. It is stored encrypted on this PC and only ever sent to ElevenLabs.`
- Form shown when no key is stored or "Replace key" was pressed:
  - Field `API key`, hint `Find it under Settings → API keys on elevenlabs.io.` (hidden while an error shows). Input:
    masked, monospace, large, no autocomplete/spellcheck, placeholder `sk_…`, **disabled while connecting**; reveal
    toggle `Show key`/`Hide key`. Typing clears the error.
  - Buttons: `Connect` (primary, busy; **disabled while the trimmed draft is empty**), `Get a key` (opens
    `https://elevenlabs.io/app/settings/api-keys`; unlike Settings, a failure to open the browser is **silently
    ignored** — no toast), `Cancel` (only when replacing; **not** disabled while connecting).
  - Connect: empty → `Paste your API key first.`; core `setApiKey(trimmed)`; ok → remember `{warning, account}`,
    clear the draft, hide the reveal, leave replace mode, force a credits refresh; not ok →
    `res.error || 'ElevenLabs rejected that key.'`; thrown → the raw error message (not cleaned). No success toast.
  - The remembered `{warning, account}` lives only while the step is on screen: after Back/Continue and return, the
    connected state shows the shell's credits (if loaded) and no warning.
  - The form/connected switch follows `settings.hasApiKey` from the core broadcast, not the connect result: until the
    broadcast arrives the (now empty) form stays visible.
- Connected state (status region): check mark, title `Key connected`, caption:
  - if account info is known (from this connect, else the shell's credits when ready):
    `{Tier} plan · {compact(remaining)} characters left ({pct}%)` — tier with only its first letter uppercased (no
    `_` → space, unlike Settings' `titleCase`: `starter_plus` → `Starter_plus plan`), omitted with its ` plan · ` when
    empty; `remaining = max(0, limit − count)`; `pct = round(clamp((limit−count)/limit,0,1)·100)` (0 when limit ≤ 0 or
    not a number). A restricted key (account `null`) shows `Ready to read.`;
  - else `Ready to read.`;
  - plus the warning (info icon) when the connect returned one;
  - button `Replace key` (ghost) → form.

**Voice** (`StepVoice.tsx`): eyebrow `Voice`, heading `Pick a voice`, body
`Preview a few and choose the one you like. You can switch any time on the Voices page.`
- No key (demo does **not** count here): empty state `Connect a key to browse voices` /
  `{settings.voiceName ?? 'The default voice'} will read for you until then.`
- Fetched only while a key is stored (re-fetched when `hasApiKey` turns true); first fetch without refresh, every
  `Try again` with `refresh: true`.
- Loading → 6 placeholders. Error → `Couldn't load voices` / {raw error message} / `Try again` (refetch with refresh).
  Empty → `No voices on this account` / `Add voices on elevenlabs.io, then come back.`
- List (scroll area, max height 268 px), **API order (not sorted)**. Each item: a select button (pressed when it is the
  current voice) with the name and a tag line, plus a preview button if `previewUrl`.
  - Tag line: `[labels.accent, labels.gender, labels.use_case ?? labels['use case'], labels.age]` (the `use case`
    fallback applies only when `use_case` is absent, not when it is empty), non-empty ones, first 3, raw values joined
    ` · ` (`british · middle aged · narration`); if that is empty → `category`, then `Voice`.
  - Choose: no-op if already current; else write `{voiceId, voiceName}` (no success toast); failure toast
    `Couldn't select {name}: {message}` (no id, raw message). The select button is a toggle button pressed when current.
  - Preview (simpler than §4.6; does **not** pause a reading): one shared player (preload none); clicking the playing
    voice pauses; another voice replaces the source and plays. "Previewing" clears only when the player is really
    paused/ended (a pause event from the previous sample arrives after the next one started). Play failure other than
    abort → `Couldn't play the {name} preview` (id `voice-preview`). Button: `Preview {name}` / `Stop {name} preview`
    (pressed), wave icon while playing. Stopped and released when the step goes away.

**Try it** (`StepTry.tsx`): eyebrow `Try it`, heading `Read from anywhere`.
- Illustration: a short paragraph with a highlighted sentence, the chip (`Read with Vox`), a native-looking menu
  (`Copy`, `Search the web`, `Print…`) and a pointer.
- Steps: `1 Select text in any app`, `2 Right-click the selection`, `3 Click Read with Vox beside the menu`, and when
  global shortcuts are on and `readSelection` is set: `or press {keycaps}`.
- Sample (quoted):
  `Hi, I'm {voiceName || 'Vox'}. Select any text on your screen, right-click it, and choose Read with Vox. I'll read emails, articles and documents aloud while you keep working.`
- Footer when a key is stored **or demo mode**: caption `Playing in the floating player…` while the read started here is
  `playing` or `loading` (matched by its read id: the `readId` of an ok result is remembered; a later read started
  elsewhere does not match), else `Or hear it right here:`; button `Read this` (primary, busy until the request
  resolves) → core `readText(sample, {source: 'manual'})` with **no** `sourceApp` (the widget shows `VOX` anyway);
  read failures are toasted by the core; a transport failure toasts `{raw message || 'Something went wrong'}` (id
  `read-failed`, `app/components/read-actions.ts`).
- Otherwise: `Connect your ElevenLabs key to hear it.` + `Add a key` → key step.

**Done** (`StepDone.tsx`): heading `You’re all set`, body
`Vox lives in the system tray. Close this window whenever you like: it keeps watching for your selections.` Tips:
`Click the tray icon to open Vox`; if shortcuts are on: `Read selection {keycaps}` and `Play / pause {keycaps}` (each
only when set).

---

## 6. Floating widget (`widget/Widget.tsx`, `widget/WidgetView.tsx`, `widget/SentenceText.tsx`)

The widget is a separate always-on-top, non-activating window that **owns audio playback** (see `reader-playback.md`).
This section defines exactly what it shows.

### 6.1 Inputs

From the player snapshot (`widget/player.ts:82-118`):
- `state`: `status` (`idle|loading|playing|paused|ended|error`), `title`, `voiceName`, `sourceApp?`, `positionSec`,
  `durationSec`, `durationIsEstimate`, `bufferedFraction` (0–1 synthesized), `rate`, `charIndex`, `error?`,
  `errorCode?`.
- `session` (null when none): `id`, `readId`, `text`, `title`, `voiceName`, `sourceApp?`, `fromCache`,
  `highlightWords`, `sentences` (sentence start offsets).
- `wantsPlay` (user intent: playback runs or resumes as soon as audio arrives; false when ended/error),
  `starting` (status `loading` with audio ready, waiting for the audio output to wake), `warning`
  (`{message, code}` of a non-fatal chunk failure: the read stops before the end), `partialText` (the text doesn't
  cover the audio — a cached replay of a read saved as a 160-char preview: word positions unknown).
- A session persists through `ended` and `error`; it disappears only on stop/close (status `idle`).

From settings (the widget reads them with a plain, non-optimistic settings subscription: initial get + broadcasts):
`widget.compact` (default false until loaded), `widget.highlightWords` (until loaded: the session's flag, then true),
`triggers.mediaKeys` (default **true** until loaded).

From the toast slot (§6.9): the current toast and whether it is *standalone*.

### 6.2 View selection (`WidgetView.tsx:109-124`), first match

1. a toast exists and (it is standalone **or** there is no session) → **Toast card**;
2. `status == 'error'` → **Error card**;
3. no session → **Idle card**;
4. compact setting on → **Compact pill**;
5. → **Full player card**.

Enter animation (scale 0.985 → 1 with fade; **scale only, never translate**, because the window ends at the card's
edge and a moved card would be clipped; disabled under "reduce motion"): the full and compact cards play it when
(re)created — they are keyed by session id, so a new session or a full↔compact switch re-creates them, while status
changes within a session do not; the error card and the standalone toast card play it every time they appear; the
idle card never does (`Widget.css:90-104`, `WidgetView.tsx:119-123`).

### 6.3 Shared pieces

**Top bar** (full card and error card):
- Left: status glyph + source label `(session.sourceApp || (session.fromCache ? 'Replay' : 'Vox')).toUpperCase()`
  (e.g. `CHROME`, `REPLAY`, `VOX`).
- Status glyph: `error` → red indicator light; `playing`/`paused`/`loading` → small dot-wave equalizer (animated only
  while `playing`; a distinct "loading" look while `loading`); `ended` → alert icon if stopped early (`warning` set),
  else check; anything else → logo mark.
- Right: rate button (only with a session and status ≠ `error`): text `formatRate(rate)` = `{Number(rate.toFixed(2))}×`,
  tooltip `Playback speed`, accessible `Playback speed {formatRate(rate)}` (e.g. `Playback speed 1.25×`); then Close
  (accessible name and tooltip `Close`).

**Time label** (`timeLabel`, lines 91-97):
```
rate = state.rate > 0 ? state.rate : 1
ended  → formatClock(durationSec / rate)                              // total, no prefix
else   → left = formatClock(max(0, durationSec − positionSec) / rate)
         durationIsEstimate ? '≈' + left : '−' + left                 // U+2248 / U+2212
formatClock(s): s = max(0, floor(s)); h>0 ? `h:mm:ss` : `m:ss`        // 263 → 4:23, 3723 → 1:02:03
```
Estimated times are drawn de-emphasized; accessible `about {time} left`.

**Progress / seek bar:**
- value `ended ? 1 : (durationSec > 0 ? positionSec/durationSec : 0)`; buffered overlay `bufferedFraction`;
- knob and seeking only when `bufferedFraction > 0`; otherwise inert;
- left-button press/drag shows a local preview; the seek is sent **on release** (at the release position) as a fraction
  (`seekFraction(f)`, clamped by the player to available audio); pointer cancel ends the drag **without** seeking; keys
  (only reachable if the bar can take focus — the widget window never takes focus, so effectively unused) ±0.02,
  PageUp/PageDown ±0.1, Home 0, End 1 (`design/DotProgress.tsx:74-131`);
- accessible name `Seek`, value `{clock(pos)} of {'about ' if estimate}{clock(dur)}`.

**Transport** (full card only):
- `hasAudio = bufferedFraction > 0`.
- Previous: disabled without audio. Next: disabled without audio or when `ended`. Labels/tooltips:
  `partialText` → `Back 10 seconds` / `Forward 10 seconds`, else `Previous sentence` / `Next sentence`.
  (Sentence-skip semantics incl. the 1.2 s "restart current sentence" grace: `reader-playback.md` §4.6.)
- Main button: `ended` → replay icon, `Play again`; `wantsPlay` → pause icon, `Pause`; else play icon, `Play`. Action:
  player toggle (on `ended` this replays from 0).

**Warning note** (`WarningNote`, lines 228-270) — shown when `warning` is set:
- lead `Stopped early` if `ended` else `Stops early`; reason by code:
  `no-api-key → no API key`, `unauthorized → API key rejected`, `quota → out of credits`,
  `rate-limit → ElevenLabs is busy`, `network → connection lost`, `too-long → text too long`, any other code
  (`empty-text`, `no-selection`, `not-cached`, `unknown`) → `synthesis failed`.
- Full layout: one line `{lead} · {reason}` preceded by a red dot. Compact layout: lead and reason on two lines, no ` · `.
- Codes the user fixes in Settings (`SETTINGS_ERRORS = {no-api-key, unauthorized, quota}`) make it a **link** (with a
  chevron) that opens Settings → Account; tooltip `{message}` + newline + `Open settings`; accessible
  `{lead}: {reason}. Open settings`. Other codes: plain text with tooltip `{message}`.

**Status label** (full card footer, polite):
```
ended && warning → 'STOPPED EARLY'
starting         → 'STARTING'
else by status:  idle 'READY' · loading 'SYNTHESIZING' · playing 'READING' · paused 'PAUSED' · ended 'FINISHED' · error 'ERROR'
```
plus a small animated loader whenever `status == 'loading'` (both SYNTHESIZING and STARTING).

### 6.4 Full player card (`PlayerCard`, lines 293-340)

Region named `Vox player` (the compact pill uses the same name; the error card is named `Vox error`). Rows, top to
bottom:
1. Top bar (§6.3).
2. Body (two lines tall), first match:
   - a toast (non-standalone) → **toast banner** (§6.9);
   - `highlightWords && session && !ended && !partialText` → **sentence text** (§6.10);
   - else the title (`state.title`), clamped to two lines.
3. Meta line: warning note if `warning`, else the voice name; time label on the right.
4. Progress bar (medium).
5. Footer: status label (left), transport (right).

### 6.5 Compact pill (`CompactCard`, lines 342-394)

One row: [main play/pause/replay button (same 3 states and labels as §6.3)] [middle] [right] [close (`Close`, small,
with an enlarged invisible hit area)].

Middle, first match:
- toast → **compact toast** (§6.9);
- `ended && warning` (ended early) → compact warning note (the reason takes the bar's place; a full bar would claim the
  read completed);
- `status == 'loading' && bufferedFraction <= 0` → a loader labelled `Synthesizing`;
- else a small progress bar.

Right:
- when the middle holds a note (toast or ended-early): no time; an invisible **drag grip** fills the rest (a note may be
  a link, the pill must keep a drag handle);
- otherwise: if a warning exists (read still going) a small red dot (accessible `Stops early`, tooltip
  `Stops early: {message}`), then the time label.

No source label, rate button, previous/next, text or voice name in the pill.

### 6.6 Error card (`ErrorCard`, lines 396-428) — fatal error

- Top bar (rate button hidden because status is `error`; status glyph = red light).
- Body: a toast banner if a (non-standalone) toast is up; otherwise:
  - title by code: `no-api-key → Add your ElevenLabs API key`, `unauthorized → ElevenLabs refused the request`
    (deliberately neutral: a 401 can also mean the key lacks Text to Speech permission or the account was flagged),
    `quota → Out of ElevenLabs credits`, `rate-limit → ElevenLabs is busy right now`, `network → Can't reach ElevenLabs`,
    `empty-text → Nothing to read there`, `too-long → That selection is too long`, any other code (`no-selection`,
    `not-cached`, `unknown`) → `Something went wrong`; missing code = `unknown`. Full code set:
    `no-api-key | unauthorized | quota | rate-limit | network | empty-text | no-selection | too-long | not-cached | unknown`
    (`src/shared/types.ts:171-181`);
  - detail: `state.error` if present and different from the title, else `The text could not be read aloud.`
- Footer: `ERROR` label and one action:
  - `SETTINGS_ERRORS` → `Open settings` (opens the app on Settings → Account);
  - `RETRY_ERRORS = {network, rate-limit, unknown}` with a session → `Try again`: ask the core to replay the session's
    `readId`; if that returns not-ok with code `unknown` or `not-cached` (not in history, e.g. history off), read the
    session text again directly with the same `sourceApp` (and no `source`); any other not-ok result does nothing more;
    all transport errors swallowed (the core toasts read failures). The button has no busy state and can be pressed
    repeatedly;
  - other codes (`empty-text`, `too-long`, `no-selection`, `not-cached`) → no button.
- Role alert (announced). Close in the top bar stops and hides.

### 6.7 Idle card (`IdleCard`, lines 430-456) — no session, no toast

Top bar with logo + `VOX` and Close; title `Nothing playing` (de-emphasized); meta `Select text, right-click, Read with Vox`;
an empty, non-interactive progress bar (`Progress`); footer `READY`. Normally the window is hidden when idle, so this
card is rarely seen (see §11).

### 6.8 State matrix (full card unless noted)

| User-visible state | Condition | Glyph | Body | Meta | Progress | Status label | Transport |
|---|---|---|---|---|---|---|---|
| Idle | no session | logo | `Nothing playing` | hint copy | empty | READY | none |
| Synthesizing (waiting for first audio, or caught up mid-read) | `loading`, `!starting` | wave (loading) | sentence (all words "upcoming") or title | voice, `≈m:ss` | no knob until audio | SYNTHESIZING + loader | prev/next disabled until audio; main shows Pause (wantsPlay) |
| Starting | `loading`, `starting` (audio ready, output waking) | wave (loading) | as above | voice, time | knob | STARTING + loader | enabled |
| Reading | `playing` | wave (animated) | sentence with highlighting | voice or warning, `−`/`≈` time | live | READING | Pause |
| Paused | `paused` | wave (still) | sentence | as above | live | PAUSED | Play |
| Finished | `ended`, no warning | check | title | voice, total time | full | FINISHED | prev enabled, next disabled, Play again |
| Stopped early | `ended` + warning | alert | title | `Stopped early · {reason}` (link if fixable) | full (compact: reason replaces bar) | STOPPED EARLY | as Finished |
| Will stop early | warning while still reading | wave | sentence | `Stops early · {reason}` | live | READING/PAUSED… | normal (compact: red dot + time) |
| Error | `error` | red light | error title + detail | — | — | ERROR + action button | none |
| Toast standalone | §6.9 | kind glyph | message | — | — | — | action button only |
| Toast banner | §6.9 | (card's) | banner replaces body | (card's) | (card's) | (card's) | (card's) |

Compact: see §6.5 for which elements each state replaces. Auto-hide never hides a stopped-early card (§6.11).

### 6.9 Toasts in the widget (`Widget.tsx:53-131`, `WidgetView.tsx:188-226, 458-470`)

The core sends **every** toast to the widget as well as to the app window, and shows the widget for it when the app
window is not focused (if the widget is enabled; otherwise an OS notification for non-success kinds). So the widget must
show each one whatever is playing. One slot: a new toast replaces the current one.

On arrival (payloads without a string `message` are ignored) record `{toast, revealed, sessionId}`:
- `sessionId` = current player session id (or null);
- `revealed = controller.hidden || (previous.revealed && previous.sessionId == sessionId)` — "the window had hidden
  itself when the toast arrived, so the core showed it just for the toast"; a toast replacing such a toast inherits it.
- `controller.hidden` is the **widget's own** hidden flag (`widget/controller.ts:39-48, 86-90`), not the real window
  visibility: it starts `true` (the window is created hidden), becomes `true` on every widget-side hide (auto-hide,
  Close, a `stop` command with a session, a toast dismissal that hides) and becomes `false` **only when a new read
  session starts** (the core shows the window for every new read). The core showing the window for a toast does *not*
  clear it — which is exactly what makes a second toast after a revealing one also count as revealed. Reproduce this
  flag, not `IsWindowVisible`.

Rules:
```
hidesOnDismiss(shown, status, sessionId) =
    status == 'idle' || (status == 'ended' && shown.revealed && shown.sessionId == sessionId)
standalone = sessionId == null || hidesOnDismiss(shown, status, sessionId)
```
Dismissal hides the window again only when nothing else would be on screen (idle), or for a finished read whose window
had already hidden itself before the toast. Never while a read is active, never for a finished card the user kept
(auto-hide off) or that auto-hide has not reached yet.

Timing: auto-dismiss after **2500 ms** (`TOAST_MS`), or **5000 ms** (`TOAST_ACTION_MS`) if it has an action button;
the timer restarts on every arrival, including a toast with the same id. If the pointer is over the card when the
timer fires, re-check every **1000 ms**. While a toast is up, auto-hide after `ended` waits (hold reason `toast`). The
hide-on-dismiss rule is evaluated with the player status and session **at dismissal time**, not at arrival.

Presentations:
- **Toast card** (standalone): kind glyph (error → alert, success → check, info → info), message, and the action
  button (primary) if any. Role alert for errors, status otherwise. Glyph and message are drag handles.
- **Banner** (on the full player card or error card): replaces the body: glyph, message (a drag handle — so no tooltip,
  a handle never gets mouse moves), action button.
- **Compact toast** (pill middle): two-line message; with an action the whole message is a link button with a chevron
  (tooltip = action label, accessible `{message}. {action label}`); without one, plain text (drag handle).

Action click: open the app window at `action.route` (or just show it if no route), then dismiss (which may hide the
widget per the rule above).

Toast copy produced by the core (examples; full catalog in `contracts.md` §5.6): `Add your ElevenLabs API key to start reading`
(action `Add key` → Settings/Account), `Nothing to read in that selection`, `Reading the first {maxChars} characters`,
`Select some text first`, `Copy some text first`, `Nothing is playing. Select some text and press {hotkey}`, engine
error messages with `Open settings`.

### 6.10 Sentence text with word highlighting (`SentenceText.tsx`, `widget/timing.ts`)

- Show only **the sentence containing `charIndex`**: `index = max(0, lastIndexAtOrBefore(sentences, charIndex))`,
  range `[sentences[index], sentences[index+1] ?? text.length)`.
- Words = maximal non-whitespace runs in that range, rendered separated by single spaces (original whitespace is not
  reproduced).
- `started = positionSec > 0 || status == 'playing'`; current word = `started ? last word whose start ≤ charIndex : −1`
  (before playback starts every word is "upcoming").
- Three looks: spoken (before current), current, upcoming (after).
- Viewport exactly two lines tall. Long sentences scroll line by line: offset = `min(currentWord.top,
  max(0, contentHeight − viewportHeight))` (never past the last full lines); with no current word the offset is 0.
  Recomputed when the current word, the sentence or its words change. A new sentence enters with a short animation.
- Sentence starts come from the session text (`widget/timing.ts:76-125`):
  ```
  raw = UAX #29 sentence-break offsets (Intl.Segmenter, default locale)
        // fallback without a segmenter: [0] + end of every match of /[.!?…]+["'”’)\]]*\s+/g
  raw.push(len(text))
  for each [a, b) between consecutive raw offsets:
      advance a past whitespace (/\s/); if a >= b skip (empty sentence)
      out.push(a, ...softSplits(text, a, b))
  return out or [0]

  softSplits(text, start, end):            // LONG_SENTENCE = 280, SOFT_TARGET = 200
    from = start
    while end − from > 280:
      cut = −1
      for p from min(end − 1, from + 280) down to from + 41:
        if text[p−1] ∈ { , ; : — – } and text[p] is whitespace:
          cut = p
          if p − from <= 200: break        // first candidate at or under 200 wins
      // no candidate ≤ 200 → cut is the lowest candidate found in (200, 280]
      if cut < 0: break
      advance cut past whitespace; if cut >= end: break
      push cut; from = cut
  ```
  (Same function feeds sentence skipping: `reader-playback.md` §5.)

### 6.11 Auto-hide and hover (summary; owner `reader-playback.md` §7, `shell.md` §6.3.6)

- After `ended`, if `widget.autoHide` and **no warning** (a read that stopped early keeps its card up so the user sees
  why), hide after **4000 ms**. While any hold is active (`hover`, `toast`) re-check every **1500 ms**.
- Hover: the card's text labels are drag handles (window caption), where the UI gets no pointer events, so the core polls
  the cursor every 100 ms and tells the widget when the pointer enters/leaves the card. Hover holds auto-hide and toast
  dismissal. In a native port, track `WM_NCMOUSEMOVE`/`WM_MOUSEMOVE` + `TrackMouseEvent(TME_LEAVE | TME_NONCLIENT)`
  instead of polling.
- Close button: stop playback locally, tell the core to stop synthesis (`stop` command), hide the window. A `stop`
  command arriving from elsewhere while a session exists also hides it.

### 6.12 Window-level behaviors the UI depends on

- **Size reporting** (`Widget.tsx:23-51`): the window is exactly the card (no shadow margin, transparent only at the
  rounded corners) so every click on it lands without click-through toggling. The UI reports its content size (rounded
  up) and the card's rectangle inside it (layout box, ignoring the entry animation's transform); duplicates and zero
  sizes are not sent; re-observed whenever the card element is replaced (view key = status, session id, toast id,
  compact). The core clamps to 40×24 … 1200×900 and keeps the anchored edges fixed when resizing (a widget in the lower
  half grows upward, in the right half leftward).
- **Drag handles** (`widget/Widget.css:33-69`): only these drag the window: source label area, the body when it holds no
  button/progress bar, voice name, time, status label, the compact "Synthesizing" loader, the compact middle when it
  holds no button/progress/warning, the compact grip, the banner message, the toast card's glyph and message.
  Buttons, the progress bar and anything marked "no-drag" never drag. Reason: making the whole card draggable with
  holes for controls fails when the window still has the previous view's regions (they update a frame late and not at
  all while hidden): a press on a button became a window move and the click was lost. Also: a handle's region must be
  exactly its own box (a scrolled sentence's hidden lines once turned Speed and Close into title bar).
- Right-clicking a drag handle must **not** open the system menu (its Close would destroy the window); the core
  suppresses it, and treats an external close request (WM_CLOSE) as "quit".
- Minimum hit target (32 px): buttons drawn smaller get an invisible larger hit area that is never a drag handle —
  the rate button extends 4 px above/below and 3 px left/right, the compact Close 2 px on every side
  (`Widget.css:71-87`).
- The compact pill's middle is a drag handle only when it holds no button, no progress bar and no warning note (the
  stopped-early reason needs mouse moves for its tooltip, which a handle never gets).
- The widget is non-activating (never steals focus), always on top, not in the taskbar; it keeps running timers and
  audio at full rate while hidden (Chromium's `backgroundThrottling: false` and autoplay without a user gesture).
- Dragging writes `widget.position = 'remember'` with the new top-left (§2.6).

### 6.13 Media session (summary; owner `reader-playback.md` §9)

Active only while `triggers.mediaKeys`, a session exists and status ≠ `error`. Metadata: title = `session.title || 'Vox'`,
artist = voice name, album = `Vox`. Actions: play, pause, stop (= Close), previous/next track (= sentence skip),
seek-to, seek backward/forward (default 10 s). Playback state `playing` when playing, else `paused`; `none` when
inactive. Position state refreshed on status/rate changes and once per second. Native: Windows
`SystemMediaTransportControls` (via `ISystemMediaTransportControlsInterop::GetForWindow` on the widget HWND).

### 6.14 Widget actions (`Widget.tsx:166-196`)

| Action | Effect |
|---|---|
| toggle | player toggle (pause if playing/loading, else play; on ended → replay from 0) |
| previous / next | sentence skip −1 / +1 (10 s steps when `partialText`) |
| seek(f) | `seekFraction(f)` |
| cycleRate | `next = first r in [0.75,1,1.25,1.5,1.75,2] with r > rate + 0.001, else 0.75` (wraps; any rate ≥ 2 → 0.75); apply to the player now and persist `playbackRate` (errors ignored) |
| close | §6.11 |
| openSettings | open the app on Settings → Account |
| retry | §6.6 |
| toastAction | §6.9 |

---

## 7. Chip and selection popup (`chip/Chip.tsx`, `chip/main.tsx`; window logic `shell.md` §6.5)

A separate tiny window showing one pill. Always the dark look.

| Mode | When | Default label | Glyph size |
|---|---|---|---|
| `context` | user right-clicked selected text; shown beside the native context menu | `Read with Vox` | 12 |
| `selection` | right after a selection (popup trigger) | `Read` | 11 |

Content: a round disc with a play icon + the label. Accessible name = the label. Not focusable. Pill height: 36 DIP in
context mode, 32 DIP in selection mode (`Chip.css:16-60`). Hover (and the static "hover" preview) inverts the pill's
colors; a press scales it to 0.96 — the only interactive feedback.

Initial state (before any show): mode `context`, label `Read with Vox`, hidden (exit look).

Behavior:
- **Show** (core → chip, payload `{mode, label}`): mode `selection` if the payload says so, anything else (including a
  missing payload) → `context`; label = payload label if non-empty, else the mode default; play the enter animation —
  replayed on **every** show, even when already shown: 200 ms, pill fades in from scale 0.82, the disc grows from
  scale 0.6.
- **Hide** (core → chip): ignored if not currently shown; else play the exit animation (120 ms ease-in: fade out to
  scale 0.9); the core hides the window 140 ms later and makes it click-through meanwhile (inert during exit).
- Reduce-motion: both animations shrink to 1 ms.
- **Press anywhere in the window = press on the chip** (the window is only as large as Windows allows around the pill):
  activation happens on **press, not click** (some context menus swallow the click that dismisses them). The press's
  default action is cancelled (no focus, no text selection). Left button → `activate` (core reads the selection); any
  other button → `dismiss`. Presses while hidden / during the exit animation are ignored. The page's own context menu
  is suppressed.
- **Size reporting:** reports `{width, height}` of its content (rounded up) whenever mode, label or rendered size
  changes, de-duplicated by `mode:WxH` and never with a zero dimension; Windows keeps a transparent window at least
  38 DIP tall (48 px at 125 %), so the content is at least 38 tall with the pill vertically centred — "the size the
  core places by is the window's real size". Defaults before a report: context 153×38, selection 82×38; core clamps
  32×24 … 480×160.
- Placement (core): context mode beside the predicted context-menu rectangle, never on it; selection mode 20 DIP
  above-right of the cursor (below if no room) — `shell.md` §6.5.3.
- Window: non-activating, always on top **above context menus** (z-level "pop-up-menu"), not in the taskbar. Showing
  must never take focus (that would close the context menu). Native: `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW |
  WS_EX_TOPMOST`, `SW_SHOWNOACTIVATE`, `WM_MOUSEACTIVATE → MA_NOACTIVATE`, re-assert `HWND_TOPMOST` after showing; the
  low-level mouse hook also tests clicks against the chip's bounds because a menu may capture the mouse.

---

## 8. Toast ids and copy used by this subsystem (dedupe ids)

| id | kind | Copy |
|---|---|---|
| `settings-save-error` | error | title `Couldn’t save that setting`, message = error |
| `open-external` | error | `Couldn’t open the browser` |
| `api-key` | success / info / error | `API key saved` / `API key removed` / title `Couldn’t remove the key` |
| `voice-test` | error | error text |
| `library-retention` | error | title `Couldn’t check your history` |
| `library-cache` | success | `Audio cache cleared` |
| `library-export` | success | title `History exported`, message = saved path |
| `library-history` | success | `History cleared` |
| `library-{cache,export,history}` | error | error text |
| `copy` | error | `Couldn’t copy to the clipboard` |
| `voices-refresh` | error | title `Couldn’t refresh voices`, message = error |
| `voice-use` | success | `Reading with {name} from now on` |
| `voice-preview` | error | `Couldn’t play this sample. Check your connection.` / `Couldn’t play this sample.` / `Couldn't play the {name} preview` |
| `read-failed` | error | raw error text, or `Something went wrong` when empty (onboarding "Read this" transport failure) |
| (no toast) | — | the widget's rate-button save failure, the onboarding `Get a key` open failure, About/Integrations reveal and link failures are all silently ignored |
| (none) | error | `Couldn't save your setup: {message}`; `Couldn't select {name}: {message}`; `Couldn't change the theme: {message}` |

---

## 9. Platform notes for the native port

| Electron/Chromium did | Native Windows equivalent |
|---|---|
| `safeStorage` ("Encrypted with Windows DPAPI") | `CryptProtectData` / `CryptUnprotectData` (user scope) on the key bytes; keep the copy truthful |
| `showInactive`, `focusable:false`, `alwaysOnTop('floating'|'pop-up-menu')`, `skipTaskbar` | `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, `ShowWindow(SW_SHOWNOACTIVATE)`, `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE|SWP_NOMOVE|SWP_NOSIZE)`; handle `WM_MOUSEACTIVATE` → `MA_NOACTIVATE` |
| Transparent frameless window with rounded card | Layered window (`WS_EX_LAYERED` + per-pixel alpha via `UpdateLayeredWindow`) or DWM rounded corners (`DWMWA_WINDOW_CORNER_PREFERENCE`) on an opaque window; the 38-DIP minimum height was an Electron quirk and can go |
| `-webkit-app-region: drag` | `WM_NCHITTEST` → `HTCAPTION` over drag handles, `HTCLIENT` over controls; suppress the system menu on right-click of the caption (`WM_NCRBUTTONUP`/`WM_CONTEXTMENU`) |
| `setIgnoreMouseEvents(true)` during chip exit | toggle `WS_EX_TRANSPARENT` (with `WS_EX_LAYERED`) |
| Cursor polling for hover | `TrackMouseEvent(TME_LEAVE | TME_NONCLIENT)` |
| `navigator.mediaSession` | `SystemMediaTransportControls` |
| `prefers-color-scheme` | registry `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`; re-read on `WM_SETTINGCHANGE` with `"ImmersiveColorSet"` |
| `titleBarOverlay` colors | custom-drawn caption or `DwmSetWindowAttribute(DWMWA_CAPTION_COLOR / DWMWA_TEXT_COLOR / DWMWA_USE_IMMERSIVE_DARK_MODE)` |
| `KeyboardEvent.code/key`, `navigator.keyboard.getLayoutMap()` | `WM_KEYDOWN/WM_SYSKEYDOWN` (Alt combos arrive as SYS*; F10 too), VK + scan code from `lParam`; `MapVirtualKeyExW`, `ToUnicodeEx` with a synthetic key state for AltGr probing; `GetKeyboardLayout(0)`. Note Windows injects a fake `LCtrl` with `RAlt` on AltGr layouts |
| `globalShortcut` suspend while recording | `UnregisterHotKey` all, re-register on stop or after 30 s |
| `<audio>` sample preview with abortable download | HTTP stream (cancellable) + MP3 decode + output; volume per stream |
| `Intl.NumberFormat` compact/grouping, `Intl.Segmenter` sentences, `localeCompare` | ICU4X (`icu_decimal`, `icu_segmenter`, `icu_collator`) or Windows `GetNumberFormatEx` + a UAX #29 sentence splitter |
| `shell.openExternal` (http/https only), reveal folder | `ShellExecuteW(L"open", url)`; `explorer.exe` / `SHOpenFolderAndSelectItems` |
| Save dialog for export | `IFileSaveDialog` |
| `navigator.clipboard.writeText` | Win32 clipboard (`CF_UNICODETEXT`) |
| Native `<dialog>` modal | owned modal window or in-app overlay with focus trap; Esc/backdrop semantics §1.6 |
| OS "reduce motion" | `SystemParametersInfo(SPI_GETCLIENTAREAANIMATION)` |

---

## 10. Edge cases checklist

- Optimistic settings: two quick changes then a failure of the first → the overlay holds until both settle, then shows
  the core's truth (second applied, first reverted).
- Retention: pick 7 → 30 → 7 within 450 ms on a "Keep forever" library → only one count, for the last value.
- Retention to a stricter value that deletes nothing saves silently.
- Hotkey recorder: tabbing into the control starts recording (focus starts it); Tab out cancels. Recording suspends
  global hotkeys; a crash mid-recording is covered by the core's 30 s auto-resume.
- Reserved chord recorded → old shortcut kept, message for 6 s; reserved chord already in the file → persistent message.
- Excluded apps: `"C:\Program Files\Foo\Bar.EXE"` (with quotes) → `bar.exe`; `con .exe` → invalid (space before
  `.exe`); `a?b` → invalid.
- Port: `00080` → 5 digits but 80 < 1024 → error; Escape restores; blur with the stored value → no write.
- Model switch to one lacking the pinned language resets the language in the same write.
- Unknown model id / language code / rate / maxChars / retention stay selectable (appended options).
- Voices: a voice with no `previewUrl` has no play button; starting a sample pauses a reading; a reading that starts
  stops the sample.
- Widget: toast during an active read → banner, never hides on dismiss; toast while hidden and idle → standalone card,
  hides on dismiss; toast while a finished card is still up (auto-hide pending) → banner, does not hide.
- Widget: stopped-early reads are never auto-hidden.
- Widget: cached replay of a preview-only read (`partialText`) → title instead of words, ±10 s skips.
- Chip: right-button press on the chip → dismiss, not activate; presses during exit → ignored.
- Widget toast while the widget had hidden itself, then a second toast before dismissal → both count as revealed; the
  second's dismissal hides the window (if idle, or ended with the same session).
- Widget toast with the same id twice → the dismiss timer restarts.
- Hotkey recording held open > 30 s → global hotkeys come back mid-recording (core safety net).
- Cache limit hand-edited above 5000 MB → slider shows 5 GB, `In use` shows the real limit (§2.8 quirk).
- Retention: switching from a pending stricter value back to the stored one cancels the check and writes nothing.
- Onboarding key step: leaving and returning drops the connect warning.

---

## 11. Inconsistencies and decisions for the port

1. **Two API-key forms** with different copy and rules: Settings (`API key`/`New API key`, empty → message, input
   stays enabled) vs onboarding (`API key`, Connect disabled when empty, input disabled while busy, different hint and
   error strings). Decide whether to unify.
2. **Two voice pickers** with different tag formats, sorting (onboarding keeps API order) and preview engines
   (onboarding's does not pause a reading). Probably unify on the Voices-page rules.
3. **Demo mode** counts as "can read" in onboarding Try-it but not in onboarding Voice (which requires a key).
4. Mixed apostrophes: most copy uses `’`, but `Couldn't save your setup`, `Couldn't select …`, `Couldn't play the …
   preview`, `Couldn't load voices`, `Can't reach ElevenLabs`, `Couldn't change the theme`, the onboarding sample
   (`I'm`, `I'll`) and several core messages use `'`. Normalize to `’` in the port unless the strings are matched
   elsewhere.
5. **About → Electron** row has no meaning in a native app; replace (e.g. build/commit, or remove).
6. **Media keys "Takes effect after Vox restarts"** is a Chromium limitation (media-key handling is a startup switch);
   with SMTC in a native app this can apply live — drop the copy and the restart toast if so.
7. **Hook-unavailable notice** exists because the native hook module could fail to load; in Rust the hook is part of
   the binary, but `SetWindowsHookEx` can still fail — keep the notice wired to that.
8. **Idle card**: reachable only if the window is shown with no session and no toast; the current core never does that
   intentionally. Keep as a fallback or drop.
9. Title-bar symbol colors differ between the UI (`#0b0b0b`/`#f2f2f2`) and the core's window creation
   (`#111111`/`#ffffff`).
10. The Settings "Saving" badge assumes asynchronous writes; if settings writes become synchronous in-process, keep the
    badge semantics (show "Saved automatically" after the write) or simplify.
11. Toasts reach both the app window and the widget; when the app is visible but **not focused**, both show the same
    toast. Intentional in the source (the widget is the fallback surface), but worth a decision in the redesign.
12. Retention count uses a binary search over paged history because of IPC; use a direct count query in Rust.
13. Hotkey recorder hint is not drawn while modifiers are held (only announced) — fix in the redesign.
14. **Error text cleaning differs:** Settings and Voices pass errors through `cleanError` (§1.4); onboarding (finish,
    key connect, voice list, voice select) and the Try-it read toast show the raw error message, which in Electron
    still carried the `Error invoking remote method '…': Error:` prefix. Use one cleaner everywhere.
15. **Cache-limit slider above 5000 MB** (hand-edited file): knob/label clamp to 5 GB while the stored and `In use`
    value is higher, and releasing on 5000 writes nothing (§2.8). Either cap the stored value at 5000 or show the real
    value.
16. **Library actions share one busy flag** (§2.8): two actions started together show the wrong busy state.
17. **Hotkey recorder auto-repeat:** Enter/Space in idle state restart recording regardless of modifiers, so holding a
    just-recorded chord ending in Enter/Space re-opens recording (§3.5). Check modifiers (or ignore auto-repeat) in the
    port.
18. **30 s suspension safety net** re-enables global hotkeys while the recorder may still be open (§2.5). Consider
    re-arming the timer while the recorder is focused, or stopping recording when it fires.
19. **Onboarding `Get a key`** ignores failures to open the browser and its `Cancel` stays enabled while connecting;
    Settings toasts the failure and disables Cancel.
