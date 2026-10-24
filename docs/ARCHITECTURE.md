# Vox (Rust) — Architecture

This is the build contract for the Rust/Slint rewrite of Vox. It turns `docs/BRIEF.md` (fixed decisions and the
**Product rulings**), `docs/DESIGN.md` (look) and the nine port specs in `docs/port/` (behaviour) into crates, threads,
interfaces and work packages that many engineers can implement **in parallel without talking to each other**.

| | |
|---|---|
| Status | Skeleton complete and revised after the nine spec reviews (revision 2): every crate compiles (`cargo check --workspace`), every public signature is final, bodies are `todo!()` except **vox-core** (implemented, 87 tests), **vox-art** and **ui/design** (done by other agents), and a few pure helpers in vox-app (`core::failure_update`, `ui::chip::press_gate`, `core::retention::reconcile_plan`, …). The UI contract (`ui/models.slint`, window/page interfaces, the controller interface in `vox_app::ui`) is final; page visuals are placeholders. What revision 2 changed and why: §8.15. |
| Precedence | BRIEF.md Product rulings › this document (§8 decisions) › `docs/port/*.md` › the old TypeScript. |
| Authoritative API | The source doc comments plus Appendix A (generated from them). A cross-crate signature change needs an architect change to this file. Private internals are each owner's choice. |
| Section references | `contracts.md §5.3` etc. refer to `docs/port/`. "R-…" = a Product ruling, "D-…" = a decision in §8. |

Reading guide: everyone reads §1, §3 and §8; then your crate in §2 and your package in §10. §4 (flows) shows how the
pieces meet; §5 (audio), §6 (files) and §7 (UI contract) are the deep dives for their owners.

---

## 1. Crate map and dependency graph

```text
                                ┌────────────────────────── vox-core ──────────────────────────┐
                                │  pure types + rules: settings, history, player/read/TTS      │
                                │  types, formatting, locale, hotkeys, geometry, ledger, JSON   │
                                └──────────────────────────────────────────────────────────────┘
                                   ▲        ▲          ▲          ▲          ▲          ▲
                                   │        │          │          │          │          │
                              vox-text  vox-store  vox-audio   vox-win   vox-bridge     │
                                   ▲                                                    │
                                   │                                                    │
                                vox-tts (vox-core + vox-text)          vox-art (no workspace deps)
                                   ▲
   all eight library crates ───────┴──────────────────────────────►  vox-app  (vox.exe; + slint, muda)
```

| Crate | Kind | Responsibility | Workspace deps | Main external deps |
|---|---|---|---|---|
| `vox-core` | lib | Shared contracts. Settings schema/defaults/sanitize, `ReadRecord` + sanitize, listen-log ledger, player/read/TTS stream types, user-facing copy, display formatting (locale-aware), hotkey grammar/plan, window geometry, CLI parsing, JSON compatibility, UTF-16 helpers, chart models. **No I/O, no Windows APIs, no threads.** | — | serde, serde_json (`preserve_order`), chrono, regex, url, uuid |
| `vox-text` | lib | Text clean-up, sentence segmentation, chunking, reader cuts, preview titles, context windows, word highlighting view, file text extraction. | vox-core | regex, fancy-regex, icu_segmenter, encoding_rs |
| `vox-store` | lib | Profile paths, durable JSON documents, settings store, API-key storage + Electron migration, history store, audio cache, analytics computation, demo data, logging, `ui-state.json`, `window-state.json`. | vox-core | serde_json, chrono, parking_lot, crossbeam-channel, tracing(-subscriber), regex, base64, aes-gcm, windows (DPAPI, known folders) |
| `vox-tts` | lib | ElevenLabs client (system proxy, Windows cert store), error model, mapping, memo caches, key validation, preview download; `TtsEngine` (sessions, look-ahead, retries, billing, cached playback); mock synthesiser; WAV helper. | vox-core, vox-text | ureq (rustls + `platform-verifier`, gzip), icu_collator, sha2, hex, base64, url |
| `vox-audio` | lib | The `Player`: decode (MP3/WAV), WSOLA time-stretch, resample, WASAPI output, output warmer, timeline/position/listening maths, gapless chunk joins, failure recovery. | vox-core | symphonia, cpal, rubato, rtrb |
| `vox-win` | lib | Everything Win32 except audio/DPAPI/Slint windows: input hook, gesture machine, selection service + clipboard round trip, foreground info, UIPI probe, hotkeys, SMTC media keys, the tray icon (`notify_icon`, version 4), registry (Explorer verb, autostart, theme, proxy), single instance + Electron detection, system events, locale, window/monitor helpers (overlay subclass), shell helpers (notifications + AUMID registration). | vox-core | windows 0.62, winreg |
| `vox-bridge` | lib | Localhost HTTP bridge for the unchanged browser extension: request pipeline (pure) + its own listener (accept loop, per-connection threads; D-INT-16). | vox-core | httparse, socket2, windows (WinSock) |
| `vox-art` | lib (**done**) | Procedural silk thumbnails/backdrop: `Art {width, height, rgba}`, `thumbnail(seed,w,h)`, `backdrop(w,h,dark)`, `accent(seed)`, `palette_for(seed)`. | — | tiny-skia |
| `vox-app` | bin `vox.exe` | Bootstrap, the core thread (read controller + orchestration), Slint UI glue and page controllers, native window treatment, tray, screenshot harness. | all | slint =1.18.1, muda 0.21 (tray menu; the icon is `vox_win::notify_icon`, D-SH-18), anyhow |
| `tools/vox-shot` | bin (**done**) | Renders any `.slint` component + JSON to PNG (page previews). | — | slint-interpreter |

Deviations from BRIEF's crate list (each with its reason in §8): the bridge does not use `tiny_http` (D-INT-16: no access to accepted sockets, no header cap, `Connection` dropped; tiny_http stays as the vox-tts test server); the tray icon is not `tray-icon` (D-SH-18: no `NOTIFYICON_VERSION_4`, so keyboard selection cannot open the window), its `muda` menus stay.

Rules:
- Library crates never depend on each other except `vox-tts → vox-text`, and everything → `vox-core`. Only vox-app
  depends on Slint. Every library crate builds and tests alone: `cargo test -p vox-text` etc.
- Errors: `thiserror` enums in libraries (`StoreError`, `SecretError`, `ExtractError`, `TtsError`, `DecodeError`,
  `OutputError`, `WinError`), `anyhow` in vox-app. vox-core has no error type except `json::LenientParseError`;
  its validators return `Option`/`Result<_, String>` carrying the exact user-facing message.
- No `unwrap()` on I/O or user input outside tests; no panic may cross an FFI callback (hook procs, WndProcs, the
  cpal callback, COM/WinRT handlers catch and log). Logs never contain the API key or the text being read.
- `cargo clippy --workspace -- -D warnings` clean once implemented. The skeleton crates carry
  `#![allow(dead_code, unused)]` while bodies are `todo!()`; each owner removes it when done.

---

## 2. The crates

Each subsection lists modules, the contract points other crates rely on, errors, threading, and the spec sections
implemented. Exact signatures: Appendix A and the source.

### 2.1 vox-core (implemented)

Rules (`crates/vox-core/src/lib.rs`): pure; **text offsets crossing a crate boundary or persisted are UTF-16 code
units (`u32`)** (§5.6); JSON written with JS number formatting and the Electron field order; local-time maths generic
over `chrono::TimeZone` (production passes `chrono::Local`, tests fixed offsets); display conventions from
`locale::current()` (D-FMT-1).

| Module | What | Spec |
|---|---|---|
| `analytics` | `AnalyticsRange`, `AnalyticsSummary` and parts (`Totals`, `TodayStats`, `Streak`, `DailyEntry`, `TopApp`, `TopVoice`, `SourceCount`) | contracts.md §3.7, persistence.md §7.4, D-AN-2 |
| `app` | `AppPage`, `AppRoute` (hash parse/format, validation), `Toast`/`ToastKind`/`ToastAction` (durations), `AppInfo` (+`build`, `api_key_notice`), `native_hook_available(Option<bool>)`, `ChipMode`, `RuntimeFlags` | contracts.md §1.8, §3.8; ui-main.md §2.1; shell.md §1.1 |
| `cache` | `CacheStats` | contracts.md §3.7 |
| `charts` | daily bucketing (day/week/month), ticks, axis labels, tooltips, bar levels, heat cells, top items | ui-main.md §5.8–§5.11, §11.3 |
| `cli` | `parse_cli_args`, `CliArgs`, `CliAction`, `cli_outcome` (`handleArgs`: action + show-main rule), `resolve_read_file` | contracts.md §1.8, shell.md §2, reader-playback.md §2.8 |
| `constants` | every cross-cutting constant (timings, limits, sizes, ports, names; revision 2 adds `MAX_ALL_DAYS`, `SHUTDOWN_WATCHDOG_MS`, `SERVICE_SHUTDOWN_JOIN_MS`, `SESSION_END_WAIT_MS`/`_FALLBACK_MS`, `PRELOAD_OUTPUT_DELAY_MS`, `HOTKEY_SUSPEND_REARM_MS`, `PROXY_RECHECK_MS`, `HOOK_SILENCE_REINSTALL_MS`; `SIDEBAR_RAIL_BELOW` = 1100) | contracts.md §2, shell.md §11, tts.md §9, selection.md §5.1/§8.1, integrations.md §1.1, persistence.md §7.1 |
| `copy` | exact core copy (toasts, notices, ruling copy: old-version guard, key migration, UIPI, hook notice, player failure, `SOMETHING_WENT_WRONG`, `open_link_failed`, Appearance intro + theme notes) — curly apostrophes (D-COPY-1) | contracts.md §5.6, selection.md §2.4, shell.md §5.4 |
| `format` | clocks, durations, numbers (locale), compact numbers, dates (locale order/clock), relative times, day labels, credits, tiers, **one model-name style** (`model_name`, D-UI-2), bytes, player labels (`progress_accessible`), error titles, tray tooltip | ui-main.md §11, ui-settings-widget.md §2/§6, reader-playback.md §8, shell.md §7.2 |
| `geometry` | `Point`/`Size`/`Rect`, widget placement per position + anchors, anchored resize, context-menu prediction + chip placement, size/hit sanitizing, hover test | contracts.md §4.2, shell.md §6.3.4, §6.4, §6.5.3 |
| `history` | `ReadSource`, `ReadRecord` (+ JSON sanitize `from_value`, typed `RecordPatch`, `apply_patch`, `same_as`), history file parse, insertion, `HistoryQuery`/`HistoryPage`/`list_records`, `count_expiring`, word counts, listened fraction, audio length, `can_replay` | contracts.md §3.6, §3.9; persistence.md §5 |
| `hotkey` | `HotkeyAction`, split/canonical key (AltGr = Ctrl+Alt, D-HK-2), reserved chords, conflicts, `parse_accelerator` → VK, AltGr probe, `plan_registration`, `sort_failures` (registration order), `HotkeyFailureReason::id`, `failures_log_line`, failure copy, keycap labels, recorder mapping | contracts.md §2.3, shell.md §8, ui-settings-widget.md §3 |
| `ids` | UUID v4, read-id validation | contracts.md §3.6 |
| `js` | `js_round`, `round1/3`, JS whitespace/trim, `to_fixed`, `number_to_string` | contracts.md §3 conventions, tts.md §8 |
| `json` | BOM strip, lone-surrogate repair, lenient parse (`parse_lenient`, and `parse_lenient_with(text, OutOfRange::{InfinityString, Null})` — the bridge maps `1e400` to `null`, D-INT-15), JS-compatible serializers, `lenient` field readers | persistence.md §2.4, §11; integrations.md §1.4 |
| `listen_log` | `ListenLog`, hour keys (strict, D-LL-1), sanitize/add/same/entries | contracts.md §6, persistence.md §5.5, reader-playback.md §11 |
| `locale` | `Locale` (decimal, group, 12/24 h, day/month order), process locale + thread override | R-Formatting, D-FMT-1 |
| `player` | `PlayerStatus`, `PlayerState` (+ idle), `SessionInfo`, `SessionWarning`, `PlayerSnapshot`, `PlayerCommand` + lenient and bridge-strict parsers, `sanitize_player_command` (the same clamps for typed in-process commands), `next_rate` | contracts.md §3.5, §5.1, §5.3; reader-playback.md §1.1, §4.10, §8.5 |
| `read` | `ReadErrorCode`, `eleven_messages`, `error_message`, `error_needs_settings`, `ReadResult` (canonical failures incl. `internal_error`), `ReadRequest`, `VoiceOverride`, `ReadTextOptions::into_request`, `ForegroundInfo`, `BridgeReadRequest::into_request`, `is_http_url` | contracts.md §3.1, §3.4, §5.3; tts.md §2.4.2 |
| `settings` | `Settings` and sub-structs, enums (`ThemeMode`, `ResolvedTheme`, `WidgetPosition`, `OutputFormat`), sanitize/merge/patch/stored form/`needs_rewrite`, `quantize` (Slint `f32` slider → stepped `f64`), option builders for Settings pages, exe-name and port validation, model options/badges | contracts.md §3.2, §4; persistence.md §3; ui-settings-widget.md §2 |
| `text16` | UTF-16 ↔ byte offsets, `slice16`, `prefix16` (never splits a pair), `Utf16Index` | §5.6 |
| `time` | local calendar helpers (DST-safe), day keys, `iso_utc_ms` (JS `toISOString`: export `exportedAt`, log lines) | persistence.md §7, §9, §11 #6; contracts.md §3.9 |
| `timing` | `ChunkTiming::{char_at_time, time_at_char}`, `sentence_at` | reader-playback.md §5, tts.md §7 |
| `tts` | `Alignment`, `TextSpan`, `TextChunk`, `TtsSessionStart` (+`voice_id`, `sentences`), `TtsChunk`, `TtsSessionDone`, `TtsSessionError`, `CachedChunk`, `ext_for_mime` | contracts.md §3.4, tts.md §1.3 |
| `voices` | `Voice`, `TtsModel`, `ModelLanguage`, `AccountInfo`, `ApiKeyResult`, credit summary, Voices-page sort/filter/facets/tags/captions | contracts.md §3.3, ui-settings-widget.md §4.3 |

Contract notes: `Settings::apply_patch` never fails (invalid values keep the base value); `ReadRecord::from_value`
returns `None` only without a usable `id`/`createdAt`; `PlayerCommand` parsing: `parse_player_command` (lenient, UI
and IPC semantics) vs `parse_bridge_command` (strict, exact HTTP-400 messages), and every **typed** command from any
in-process source goes through `sanitize_player_command` in the core (contracts.md §5.1). Every Slint `float` that
becomes a setting or a player command goes through `settings::quantize` first. `locale::current()` is thread-safe
(process value behind an `RwLock`, per-thread override for tests).

### 2.2 vox-text

All public offsets are UTF-16 units; inputs/outputs are `&str`/`String`; nothing panics on any input.

| Module | Functions | Spec |
|---|---|---|
| `clean` | `clean_text(text, CleanOptions) -> String` (11 steps + D-TXT-1..4) | tts.md §3.1–§3.6 |
| `segment` | `sentence_spans` (UAX #29 via `icu_segmenter`, `\n` breaks, abbreviation merge), `fallback_segments`, `sentence_starts` (player's; soft splits > 280), `soft_splits`, `word_spans` | tts.md §3.7, reader-playback.md §5 |
| `chunk` | `chunk_text(text, first_max, max)`, `chunk_text_default` (220/900), `split_span` | tts.md §3.8 |
| `reader` | `pre_cut`, `truncate_text`, `preview_title`, `context_before`, `context_after` | reader-playback.md §2.4, tts.md §4.5 |
| `highlight` | `sentence_view(text, &Utf16Index, sentences, char_index, started, window_units) -> SentenceView` (spoken/current/upcoming, windowed) | reader-playback.md §8.7, ui-settings-widget.md §6.10 |
| `extract` | `read_text_file(path, max_chars) -> Result<String, ExtractError>`, `decode_text_buffer`, `looks_binary`, `html_to_text`, `rtf_to_text`, `markdown_to_text`, `truncate_extracted` | integrations.md §5 |

Errors: `ExtractError::{NotAFile, Binary, Io}` — the reader turns any of them into the toast `Couldn’t open <name>`.
Data: `crates/vox-text/data/en-words.txt` (to be added by the owner: a permissively licensed English word list,
~50k entries, licence recorded in the file header) for D-TXT-2. Threading: pure functions, callable anywhere.

### 2.3 vox-store

| Module | Public surface | Spec |
|---|---|---|
| `paths` | `Profile::{resolve(demo), at, root, is_demo, settings_file, history_file, secrets_file, secrets_dpapi_file, window_state_file, ui_state_file, audio_cache_dir, logs_dir, log_file, local_state_candidates}` | persistence.md §1, §6.1 here |
| `fsutil` | `retry_busy`, `is_busy_error`, `write_file_atomic(path, data, backup)`, `write_file_atomic_sync` (no per-path lock), `read_json_file → JsonRead`, `load_with_backup → LoadOutcome`, `discard_corrupt_copy`, `remove_dir_all_retry` | persistence.md §2.1–§2.4 |
| `json_doc` | `JsonDoc::{new(path, JsonDocOptions, Snapshot), schedule, flush, flush_sync, discard_corrupt_copy}`, `JsonDocOptions{debounce, max_wait, backup, retry_base, max_retries}` | persistence.md §2.5 |
| `settings_store` | `SettingsStore::{open, get, update, subscribe, api_key, has_api_key, key_status, set_api_key, flush, flush_sync, read_settings_file_sync}`, `SettingsChange`, `KeyStatus`, `KeyStatusSource` | contracts.md §4.3–§4.5, persistence.md §3, §6.2 here |
| `secrets` | `load_api_key → KeyLoad{key, source, problem}`, `save_api_key`, `clear_api_key` (tombstone), `dpapi_protect/unprotect`, `wrapped_key_from_local_state`, `decrypt_v10` | persistence.md §4, R-API-key, §6.4 here |
| `history_store` | `HistoryStore::{open, add, update, get, newest, list, all, with_all, len, is_empty, remove, clear, toggle_favorite, apply_retention, count_expiring, set_cached, export_to, on_change, on_removed, flush, flush_sync}` | contracts.md §3.6, persistence.md §5 |
| `audio_cache` | `AudioCache::{open, save_async, load_async, save, load, has, has_text, redact_text, remove, clear, stats, prune, remove_orphans, ids, flush}`, `SaveDone`, `LoadDone`, `CacheManifest`, `ManifestChunk`, `parse_manifest`, `is_valid_chunk_file` | persistence.md §6 |
| `analytics` | `compute_analytics(records, range, now)`, `heard_fraction`, `app_name`, `streaks` | persistence.md §7, D-AN-2 |
| `demo` | `Mulberry32`, `generate_demo_records(now, count)` (whitespace word counts, D-P-14), `demo_voices`, `demo_models`, `demo_account` | persistence.md §8, contracts.md §7.3 |
| `log` | `init_logging(profile, debug) -> LogGuard`, `redact` (ASCII word boundaries), `format_line` | persistence.md §9 |
| `ui_state` | `UiState{analytics_range}`, `load_ui_state`, `save_ui_state` | ui-main.md §5.1 |
| `window_state` | `WindowState{x,y,width,height,scale,maximized}`, `parse_window_state`, `load_window_state`, `save_window_state` | R-Main-window, §6.5 here |
| `error` | `StoreError::{Io, Json, InvalidId, Secret, Other}` | — |

Threading: every handle is `Clone + Send + Sync` (state behind one `parking_lot::Mutex` per store; mutation and
snapshot under the same lock, "snapshot at write time" — persistence.md §11 #10). Listeners (settings changes,
`on_removed`, the history change notification) are invoked **after the lock is released**, over a cloned listener
list, so a listener may call any store method; a re-entrant settings `update()` nests. `JsonDoc` owns one writer
thread per document (retry schedule `retry_base × failures`, `max_retries`; two concurrent `flush_sync` calls are
serialised by its state lock). `HistoryStore` owns one notify-timer thread (100 ms / 400 ms debounce; listeners must
only enqueue). `AudioCache::open` scans with up to 8 scoped threads and starts the cache's **I/O worker**: one thread
with a FIFO job queue that performs every disk mutation (saves, the `lastAccess` touch of a load, redactions, folder
deletions), which gives the per-id ordering of persistence.md §6.4 without per-id locks. `remove`, `prune`, `clear`
and `remove_orphans` update the in-memory index synchronously (fast, safe on the core thread) and queue the deletions;
`save_async` / `load_async` report through a callback that only enqueues a `Command`; `flush(timeout)` drains the
queue at quit. Store methods are short and synchronous; the slow ones (`export_to`, the cache's disk work) never run
on the core or UI thread. Errors: most operations log (scope `data`) and continue (persistence.md); the few that
return `Result` say so.

### 2.4 vox-tts

| Module | Public surface | Spec |
|---|---|---|
| `error` | `TtsError{code, message, status, api_code, api_message, param, request_id, retryable, missing_permission, aborted, retry_after_sec}` + constructors (retry policy D-NET-2), `ErrorBody`, `parse_error_body`, `error_from_response`, `permission_message`, `parse_retry_after`, `int_header`, `INVALID_KEY_CODES` | tts.md §2.4 |
| `mapping` | `map_voice`, `sort_voices`, `map_model`, `map_account`, `map_alignment`, `mime_for_format`, `mp3_kbps`, `decode_base64_lenient` | tts.md §2.5, §2.6 |
| `client` | `ElevenLabsClient::{new(key_source, ClientConfig), list_voices, list_models, get_account, synthesize, set_proxy, proxy, clear_cache, validate_api_key, fetch_preview}`, `ClientConfig{base_url, user_agent, proxy, platform_roots}`, `ProxyConfig`, `user_agent()`, `SynthesisRequest`, `SynthesisResult`, `RejectedParams`, `RejectedParam`, `key_fingerprint`, `without_rejected_param`, `synthesis_body`, `KeySource` | tts.md §2.3, §2.6–§2.11; D-NET-1..3 |
| `engine` | `TtsEngine::{new(client, sink, events, mock), start, play_cached, progress, stop, current_session_id, current_read_id, is_mock}`, `TtsSink` trait, `EngineEvent::{Complete, Failed, Stopped}` (billing `f64`), `EngineEvents`, `StartSessionOptions`, `PlayCachedOptions` | tts.md §4; reader-playback.md §3; D-NET-4/5 |
| `cancel` | `CancelToken::{new, cancel, is_cancelled, sleep}` | tts.md §4.3 |
| `mock` | `mock_synthesize(text, speed)`, `even_alignment` | tts.md §5 |
| `wav` | `encode_wav`, `read_wav_info → WavInfo` | tts.md §6 |
| `duration` | `chunk_duration(result, chunk_len16, settings)` | tts.md §4.8 |
| `cached` | `preview_replay(preview, chunks)`, `clip_alignment`, `without_text` | reader-playback.md §2.7 |

Network policy (R-Network): Windows system proxy (`ClientConfig::proxy`, read by vox-win, re-read on resume and
before reads/catalog fetches when ≥ 30 s old, swapped with `set_proxy`; a request that fails before sending because of
the proxy itself is retried once directly — D-NET-1), Windows certificate store (`RootCerts::PlatformVerifier`),
`user-agent: Vox/<version> (Windows)`, non-streaming `with-timestamps`, 220-unit first chunk, **no retry of timeouts
or of anything possibly billed** (D-NET-2), rejected optional parameters remembered per model for the process lifetime
(D-NET-3). Bodies: `http_status_as_error(false)`, every body read as bytes under the request timeout, lossy UTF-8,
`vox_core::json::parse_lenient` (never stricter than Node; `client` module docs).

Engine contract (module docs are authoritative; the core relies on each point):
- One `Mutex<EngineState>` guards the current session and its `finished`/`billed`/`delivered_sec`/`position_sec`; the
  worker holds it across "re-check live → update → sink call → event", and `start`/`stop`/`play_cached` take it to
  swap sessions. After `stop()`/`start()` returns, nothing but the D-NET-4 trailing `Stopped` can be emitted for the
  old session.
- A later chunk that fails for good: `finished = true` → `sink.error(non-fatal)` → `events(Stopped{error: Some})` →
  `sink.done(delivered)` (D-NET-5) — so the core sees `Stopped` (partial) before the player can report `ended`.
- Stop / supersede emits `Stopped{billed_so_far, error: None}` **synchronously on the caller's thread** when the session
  had not finished and billed > 0; an in-flight request that still returns audio afterwards produces one more
  cumulative `Stopped` from the worker (D-NET-4). Billing is an unrounded `f64`; the reader rounds once.
- `play_cached` computes `sentences` with `vox_text::sentence_starts` over the (possibly rebuilt) session text.

Threading: one engine worker thread per session; catalog calls run on callers' worker threads; the memo caches are
shared behind a mutex with in-flight joining (condvar).

### 2.5 vox-audio

Only `player` is a cross-crate contract: `Player::{spawn(PlayerConfig, PlayerEvents), start_session, add_chunk,
finish_session, fail_session, command, snapshot, preload_output, shutdown}`, `PlayerConfig{report_interval,
warm_output, null_output, name}`, `PlayerEvent::{State(PlayerSnapshot), Failed{session_id, message}}`. Internal
modules (`timeline`, `clock`, `decode`, `stretch`, `resample`, `output`, `warmer`) are the owner's starting point
and may change. Spec: reader-playback.md §4–§6, §8.9, §12; design in §5 here.

Contract points added in revision 2 (player module docs): an explicit `Stop` emits `State(idle, session Some)` then a
sessionless `State(idle, session None)` (a supersede does not); every spec `notify()` point (e.g. the decoded-duration
refinement, also while paused) emits a `State`; `command(Rate|Volume)` and `start_session` write the target into the
shared snapshot synchronously; a stream (re)open failing with `NoDevice` switches to the simulated clock and never
counts toward the 3-in-10-s failure budget; the 10 ms tick runs only while a session, a warm-up or a held stream needs
it; position and the listening clock come from device-reported playback instants (§5.4).

Threading: one player thread per `Player` (the app has two: reading and preview); one cpal output stream per player
(its real-time callback never locks or allocates); a supervisor inside the handle restarts a dead player thread
(R-Audio). `snapshot()` reads shared state (a small `Mutex`/seqlock, never held by the callback) and is safe from any
thread at 30 Hz.

### 2.6 vox-win

| Module | Public surface (all in physical pixels) | Threads owned | Spec |
|---|---|---|---|
| `gestures` | pure `GestureMachine` (`new, configure, handle, tick, hide_chip, reset, wants_mouse_move, next_deadline, chip_mode`), `GestureDeps`, events/decisions, VK classifiers | — | selection.md §5 |
| `hook` | `InputHook::{start(Sender<RawInput>), set_wants_mouse_move, stop}`, `RawInput` | hook thread (`WH_MOUSE_LL`+`WH_KEYBOARD_LL`, `THREAD_PRIORITY_TIME_CRITICAL`, message loop) | selection.md §4, §14 |
| `selection` | `SelectionService::{new(events), start, apply_settings, stop, flush, hook_available, is_busy, read_selection, set_chip_bounds, chip_activated, chip_dismissed}` (all non-blocking except `flush`), `SelectionConfig`, `SelectionEvent`, `NoticeKind`; private `CaptureEnv` seam | selection consumer (machine, modifier flags, deadline timer, chip decisions, dedupe, hook watchdog) + **clipboard worker** (captures, restores, late-copy watch, serial) | selection.md §2, §4.1, §6–§8; D-SEL-1..5 |
| `clipboard` | `read_clipboard_text`, `write_clipboard_text`, `sequence_number`, `has_text_format`, `snapshot`, `restore`, `privacy` (our own restores exempt via `last_own_write`) | caller's (worker) thread | selection.md §9; R-Clipboard |
| `keys` | `CopyMethod`, `copy_method_for`, `send_chord`, `send_escape`, `wait_modifiers_released` | caller's | selection.md §8.4, §10 |
| `foreground` | `WindowInfo`, `foreground_info`, `foreground_window_info`, `window_info_at`, `friendly_app_name`, `is_password_manager`, `clean_title`, `is_own_overlay_at`, `text_cursor_shown`, `fullscreen_app_running`, `input_blocked_by_uipi(hwnd, pid)` | caller's | selection.md §7, §11; R-Clipboard (UIPI) |
| `hotkeys` | `HotkeyService::{spawn(handler), register(&plan) -> failures (registration order), unregister_all, shutdown}`, `alt_gr_character`, `alt_gr_character_active_layout`, `vk_from_scan_code` | hotkey thread (message-only window) | shell.md §8; R-Hotkeys |
| `media` | `MediaSession::{spawn(handler), set_enabled, update(MediaState), shutdown}`, `MediaAction`, `MediaState` | SMTC STA thread + hidden window | reader-playback.md §9; R-Media-keys |
| `notify_icon` | `NotifyIcon::{new(tooltip, images, handler), set_images, refresh_dpi, set_tooltip, hwnd, destroy}`, `NotifyIconEvent::{Activate, ContextMenu}`, `TrayIconImage` | none (hidden window on the UI thread) | shell.md §7.3–§7.4; R-Tray; D-SH-18 |
| `registry` | Explorer verb (`quote_windows_arg`, `build_verb_command`, `set_explorer_menu[_at]`, `explorer_menu_state[_at]`, `is_explorer_menu_installed[_at]`, `ExplorerMenuSpec`, `MenuState`), `apply_launch_at_login[_at]`, `run_command_matches`, `RegistryLocation` (test hive), `apps_use_light_theme`, `system_proxy`, `parse_proxy_server`, `SystemProxy` | caller's | integrations.md §3–§4, shell.md §5.6, D-NET-1 |
| `single_instance` | `acquire(key) -> InstanceRole` (per-user mutex + first pipe instance), `InstanceServer::listen`, `InstanceListener::shutdown`, `forward_to_primary` (server SID check), `ForwardedArgs`, `electron_instance_running(profile_root)` | pipe server thread | shell.md §3.3; R-Do-not-run-both |
| `system` | `SystemWatcher::{spawn(on_event, on_session_end), shutdown}`, `SystemEvent::{DisplaysChanged, ThemeChanged, LocaleChanged, ClockChanged, Resumed, CloseRequested, MotionChanged}`, `user_locale()` | watcher thread (hidden **top-level** window) | shell.md §3.6, §5.5, §5.6; D-FMT-1 |
| `window` | `Hwnd`, `PhysRect`, `MonitorInfo{bounds, work_area, scale, primary, id}`, `install_overlay_subclass(hwnd, on_close)`, `OverlayCloseHandler`, `assert_topmost`, `is_visible`, `set_bounds` (one call), `window_rect`, `set_caption_theme`, `bring_to_front`, `cursor_pos`, `left_button_down`, `monitor_from_point`, `monitor_from_rect`, `primary_monitor`, `monitors`, `normal_placement`, `set_normal_placement`, `reduce_motion` | — | shell.md §6.1 |
| `shell` | `open_external`, `reveal_path`, `show_notification` (silent ToastGeneric, `CreateToastNotifierWithId(APP_ID)`), `register_app_user_model`, `save_file_dialog(owner, …)`, `set_app_user_model_id`, `documents_folder`, `set_per_monitor_dpi_aware`, `message_box` | save dialog: dedicated STA thread | contracts.md §5.3 |
| `error` | `WinError(String)` | — | — |

Callbacks given to vox-win services (`SelectionEvents`, `HotkeyHandler`, `MediaHandler`, `ForwardHandler`,
`SystemHandler`, `SessionEndHandler`, `OverlayCloseHandler`, `NotifyIconHandler`) are called on vox-win threads (the
last two on the UI thread) and must not block (vox-app's implementations enqueue a `Command`, except
`SessionEndHandler`, which waits ≤ 5 s for the core's synchronous save and flushes settings + history itself after
2.5 s without a reply).

### 2.7 vox-bridge

| Module | Public surface | Spec |
|---|---|---|
| `pipeline` (pure, unit-tested without sockets) | `check_request(method, url, headers, bound_port) -> Result<Route, HttpResponse>`, `check_body_headers`, `parse_json_body` (`parse_lenient_with(.., OutOfRange::Null)`), `parse_read_body -> BridgeReadRequest`, `parse_command_body -> PlayerCommand`, `json_response`, `error_response`, `health_body`, `HttpResponse`, `Route`, `Headers` (first value for Host/Content-Type/Content-Length; joined for Origin/x-vox-client) | integrations.md §1.4–§1.6, §1.10 |
| `server` | `BridgeServer::{new(handler), with_limits(handler, BridgeLimits), start(port) -> StartResult, stop, running, port}`, `BridgeHandler` trait (`read(req, deadline) -> Option<ReadResult>`, `command`, `status`), `BridgeLimits`, `BridgeStatus`, `StartResult` | integrations.md §1.2, §1.3, §1.7 |

Listener (D-INT-16; `server` module docs): a `socket2` socket with `SO_EXCLUSIVEADDRUSE` (set through `windows`
`Win32_Networking_WinSock` before `bind`; never `SO_REUSEADDR`) on `127.0.0.1:<port>`; **its own accept loop** and a
connection registry capped at 64; one thread per connection that reads the head with a deadline (10 s first request,
5 s keep-alive idle), caps it at 16 KiB (raw 431), answers malformed input / odd methods / header timeouts with the
raw 400, closes `CONNECT`, answers `Expect: 100-continue`, parses with `httparse`, runs the pipeline, reads the body
(Content-Length or chunked, 2 MiB, 15 s → 408 + `Connection: close`), and writes `Connection: close` or `Keep-Alive:
timeout=5` itself. `/read` calls `BridgeHandler::read(req, now + 15 s)`; `None` → 504 `Timed out waiting for the app`
(code `unknown`). Every closed socket is closed abortively (linger 0), so a re-enable / port change back / quick
relaunch can bind the same port again. `stop()` closes the listener and joins the accept thread before returning,
hard-closes every registered connection, and never waits for in-flight `read`s (the core calls it).

### 2.8 vox-art (done)

`Art { width, height, rgba }` (straight RGBA8, `save_png`, `encode_png`, `pixel`), `thumbnail(seed, w, h)`,
`backdrop(w, h, dark)`, `accent(seed) -> [u8; 3]`, `palette_for(seed)`. Deterministic per seed; ~10 ms per 220×164
thumbnail, < 100 ms per backdrop. Used only by `vox-app::ui::art` (worker thread) — seeds: voice id (voice art),
app name (source art).

### 2.9 vox-app (`vox.exe`)

| Module | Owner area | Thread | Spec |
|---|---|---|---|
| `main` | entry; `windows_subsystem = "windows"` in release | main | BRIEF.md |
| `app` | bootstrap order (channel + pipe listener first, AUMID registration, `UiInit`), old-version guard, startup route, quit hand-off | main | shell.md §3; §4.17–§4.19 here |
| `runtime` | `Runtime` (flags, profile, exe/extension paths, packaged, runtime label, build hash, lossy argv/cwd) | — | shell.md §1 |
| `command` | `Command` (every input to the core), `RequestId`, `CoreHandle`, `TrayAction`, `WindowKind`, `WindowEvent`, `BridgeCall`, `CatalogResult` (key generation), `ReplayOrigin`, `RevealTarget` | — | §3.2 |
| `update` | `UiUpdate` (everything the core publishes), `UiSink` (batching, buffering until attach, chip events dropped while detached), `CreditsView`, `CreditsUpdate`, `WidgetIntent`, `PreviewUpdate`, `RefreshReason` | — | §3.3 |
| `core::mod` | `Core::spawn/run/dispatch` (startup work order), `Services`, `OwedAnswer` / `owed_answer` / `failure_update` (panicking requests are still answered, D-ARCH-3) | core | §3.1 |
| `core::reader` | read controller: `read`, `read_clipboard` + `on_clipboard_text`, `replay` + `on_replay_loaded`, `read_file`, `toggle_playback`, `command` (sanitize; stop-with-session → `Widget(Hide)`), engine events, `on_cache_saved`, player reports, `prune_cache`, `new_record` | core | reader-playback.md §2 |
| `core::sessions` | tracked sessions + listening fold-back (pure; `on_state(&PlayerSnapshot)`, warning → partial; `persist_all(now, current)`) | core | reader-playback.md §2.12, contracts.md §6.4 |
| `core::engine_sink` | `PlayerSink` (TtsSink → Player), engine/player/preview event adapters | engine worker / player threads | reader-playback.md §3 |
| `core::toasts` | toast routing (app / widget / OS notification), `ToastRouter` (`ui_ready` deferral) | core | reader-playback.md §2.11 |
| `core::reactions` | settings-change dispatcher (pure diff + effects; #12 also at startup) | core | shell.md §5.1 |
| `core::hotkeys` | registration plan, failure toasts, `HotkeyState` suspension machine (pure) | core | shell.md §8 |
| `core::selection` | selection events → reads/toasts; `hook_status` → `AppInfo` | core | shell.md §4.1 |
| `core::bridge` | `BridgeGlue` (BridgeHandler with a deadline), `sync_server` | bridge connection threads / core | shell.md §4.2, integrations.md §1.3 |
| `core::integrations` | Explorer verb sync, launch at login (each followed by `AppInfo`) | core | shell.md §4.3–§4.4 |
| `core::retention` | startup/6-hourly/60 s-after-change retention (`Schedule`, pure), cache reconciliation, startup orphan removal (core thread) | core | persistence.md §5.10, §6.10 |
| `core::catalog` | voices/models/account/key validation/previews (workers, key generation), demo fallbacks | core + workers | contracts.md §5.3, tts.md §2.7–§2.11, ui-settings-widget.md §4.6 |
| `core::credits` | credits state machine (refreshing flag) and `WakeThrottle` (wake + forced reset, pure) | core | ui-main.md §1.5 |
| `core::media` | SMTC state from snapshots (active needs status ≠ idle); media actions (sanitized seeks) | core | reader-playback.md §9 |
| `core::queries` | history list/export (owner HWND, ISO timestamp)/clear, analytics, cache stats/clear (core thread), app info | core + workers | contracts.md §5.3 |
| `core::lifecycle` | quit sequence (watchdog, engine drain, bounded flush, bounded service shutdown), session end, overlay close requests | core | shell.md §3.5–§3.7 |
| `core::timers` | deadline heap (`TimerId`, incl. `PreloadOutput`) | core | §3.7 |
| `ui::mod` | `AppUi` (windows, shared state, controllers, art, tray), `UiInit`, the controller interface (`UiCtx`, `PageController`, `ToastSink`, `NavSink`, `DialogSink`, `FieldWrites`, `NavIntent`, `UiToastAction`, `DialogSpec`), `create`, `apply` | main | §3.4, §7 |
| `ui::generated` | `slint::include_modules!()` | — | — |
| `ui::shell` | `ShellController` (routes + back/forward, page enter/leave, wake throttle, sidebar), `ToastStackState` (exits, keys, no removal on replace), `DialogSlot`, `effective_page` | main | ui-main.md §1.7, §2 |
| `ui::home`/`history`/`analytics`/`voices`/`settings`/`onboarding` | page controllers (`PageController` + callback methods) building the page structs; Settings' pure `Overlay`, `RetentionGuard`, `PortField`, `HotkeyRows` | main | ui-main.md §3–§5, ui-settings-widget.md §2, §4, §5 |
| `ui::widget` / `ui::chip` | widget controller (view selection, `ToastSlot`, `AutoHide`, `card-key`) / chip controller (`press_gate`) | main | ui-settings-widget.md §6–§7, reader-playback.md §7–§8 |
| `ui::art` | art cache + worker | main + art worker | DESIGN.md "Art" |
| `ui::convert` | small converters (choices, keycaps, `credits_data` per `CreditsSurface`, toasts) | main | — |
| `ui::hotkey_recorder` | Settings recorder fed by winit key events (numpad VKs direct) | main | ui-settings-widget.md §3.5 |
| `ui::tray_view` | tray icon state, tooltip, menu key (pure) | main | shell.md §7 |
| `windowing::{mod, main_window, widget_window, chip_window, dpi}` | backend install + winit attributes hook, overlay pre-creation, native event dispatcher, placement, drag, hover, DPI hops, geometry persistence | main | shell.md §6; R-DPI; R-Main-window |
| `tray` | `vox_win::notify_icon::NotifyIcon` + muda menu; Open/OpenPage handled on the UI thread | main | shell.md §7; R-Tray |
| `shots` | `VOX_SHOTS_DIR` harness (`start` async on the UI thread, watchdog, `looks_blank`) | main | shell.md §9 |

### 2.10 Spec ownership (every section of every spec)

Owner = where the behaviour is implemented (crate::module or `ui/` file). "Info" = informative only, nothing to build.
"Gone" = an Electron mechanism the native design removes (the requirement behind it is owned where stated). Package
ids (P-…) refer to §10.

**contracts.md**

| § | Owner |
|---|---|
| 1.1 Product shape | vox-app (`ui::shell` pages), `vox_core::constants::{APP_NAME, READ_WITH_LABEL}`, `Brand` global |
| 1.2 Components and data flow | §3 here (core owns playback: vox-audio `Player` owned by `core`, BRIEF) |
| 1.3 Windows and platform behaviour | `windowing::{main_window, widget_window, chip_window}`, `vox_win::window`; readiness latch Gone (player exists before reads; UiSink buffers) |
| 1.4 Window rules 1–15 | #1–#4 handles: `ui/windows/widget-window.slint` (`drag-start` only from handles) + `windowing::widget_window`; #5 drag end: `windowing::widget_window` + `core::reactions`; #6 no click-through except chip exit: `windowing::chip_window`; #7 hover: `windowing::widget_window` (100 ms poll) → `ui::widget`; #8 own-overlay test: `vox_win::foreground::is_own_overlay_at`; #9 close requests: overlays → `vox_win::window::install_overlay_subclass` (`WM_CLOSE` → `WindowEvent::CloseRequested(Widget\|Chip)`, no `WS_SYSMENU`) + `core::lifecycle` (log, session end, quit); main → `windowing::main_window` (close to tray / quit); the hidden system window → `vox_win::system` (`SystemEvent::CloseRequested`); #10 quit: `core::lifecycle`; #11 session end: `vox_win::system` + `core::lifecycle`; #12 crash recovery Gone (in-process; panics in UI callbacks are caught and logged by vox-app); #13 coordinates: physical px end-to-end + `windowing::dpi`; #14 display changes: `core` (System → `WidgetIntent::Reposition`, chip hide); #15 hardening Gone (`app:open-external` → `core` + `vox_win::shell::open_external` with `is_http_url`) |
| 1.5 Trigger layers | `vox_win::selection` (+gestures), `vox_win::hotkeys` + `core::hotkeys`, vox-bridge + `core::bridge`, `vox_win::registry` + `core::integrations`, `tray`, `ui::home` |
| 1.6 Reading pipeline summary | `core::reader` (+ vox-text, vox-tts, vox-store) |
| 1.7 Persistence layout | vox-store (§6 here) |
| 1.8 Runtime modes, env, CLI | `vox_core::{app::RuntimeFlags, cli}`, `runtime`, `app`, `core::reader::read_file` |
| 1.9 Media keys | `vox_win::media` + `core::media` (live, R-Media-keys) |
| 1.10 Local bridge protocol | vox-bridge (`pipeline`, `server`) + `core::bridge` |
| 1.11 Old design language | Info (replaced by DESIGN.md / `ui/design`) |
| 1.12 Core module contracts | §2 here (one module per contract) |
| 2.1 constants.ts | `vox_core::constants` (`ACCENT_RED` dropped: no red in the new design) |
| 2.2 defaults.ts | `vox_core::settings` (`Default`), `vox_core::constants::PLAYBACK_RATES` |
| 2.3 Accelerator canonical form | `vox_core::hotkey` |
| 2.4 Other constants | `vox_core::constants` (window/crash constants Gone where noted) |
| 3.1 Enumerations | `vox_core::{settings, history, read, player, analytics, app}` |
| 3.2 Settings | `vox_core::settings` |
| 3.3 ElevenLabs types | `vox_core::voices`; mapping `vox_tts::mapping` |
| 3.4 Reading/TTS stream types | `vox_core::{read, tts}`; `vox_text::reader::preview_title` |
| 3.5 Player types | `vox_core::player` |
| 3.6 History types | `vox_core::history`, `vox_store::history_store` |
| 3.7 Analytics types | `vox_core::{analytics, cache}`, `vox_store::analytics` |
| 3.8 App-shell types | `vox_core::app` (`native_hook_available` from the core's `hook_status`); `OverlaySize`/`TitleBarTheme` Gone (native sizes; DWM caption colours set by `windowing::main_window`) |
| 3.9 On-disk formats | vox-store (`history_store`, `audio_cache`), `vox_core::history::{HistoryFile, HistoryExport}` |
| 4.1 Every settings key | `vox_core::settings` (sanitize), `core::reactions` (effects) |
| 4.2 Widget placement | `vox_core::geometry::place_widget`, `windowing::widget_window` |
| 4.3 Update algorithm | `vox_core::settings::{deep_merge, Settings::apply_patch}`, `vox_store::settings_store::update` |
| 4.4 Load / persist | `vox_store::settings_store` |
| 4.5 API key storage | `vox_store::{secrets, settings_store}` (R-API-key) |
| 5.1 Transport semantics | Gone (in-process); validation kept in `core` handlers (§3.2) |
| 5.2 Channel table | `command::Command` + `update::UiUpdate` (mapping table in §3.2/§3.3) |
| 5.3 Per-channel behaviour | `core::{reader, queries, catalog, hotkeys, lifecycle}`, `windowing::*`, `ui::*` (row by row in §3.2) |
| 5.4 UI façade | `ui::*` controllers + `CoreHandle` |
| 5.5 Who emits each event | `core` publishers (§3.3) |
| 5.6 Toast catalog | `vox_core::copy`, `core::toasts` |
| 5.7 One read end to end | §4.5 here |
| 6.1–6.3 Ledger format/functions/consumers | `vox_core::listen_log`, `vox_store::analytics` |
| 6.4 Ledger writer | `core::sessions` + `core::reader` |
| 7.1–7.2, 7.4–7.6 Mock façade | Gone as code (D-UI-5): page previews use `ui/previews/*.json` + `tools/vox-shot`; app-level mock = demo mode |
| 7.3 Core demo fallbacks | `vox_store::demo` + `core::catalog` (tables pinned in §9) |
| 8 Inconsistencies 1–22 | §8 decisions D-C-1…22 |

**tts.md**

| § | Owner |
|---|---|
| 1.1–1.3 Shared model | `vox_core::{constants, settings, tts, voices, read}` |
| 2.1 API facts | `vox_tts::client` |
| 2.2 Constants | `vox_core::constants` |
| 2.3 Transport | `vox_tts::client` (D-NET-1) |
| 2.4.1–2.4.5 Error model | `vox_tts::error`; messages `vox_core::read::eleven_messages` |
| 2.5 Response mapping | `vox_tts::mapping` |
| 2.6 Synthesis | `vox_tts::client::synthesize`, `mapping::decode_base64_lenient` |
| 2.6.1 synthesize + fallback | `vox_tts::client` (`RejectedParams`, D-NET-3) |
| 2.7 Voices | `vox_tts::client::list_voices` |
| 2.8 Models | `vox_tts::client::list_models` |
| 2.9 Account | `vox_tts::client::get_account` |
| 2.10 Caches | `vox_tts::client` (memo) |
| 2.11 Key validation | `vox_tts::client::validate_api_key`, `core::catalog` |
| 3.1–3.6 cleanText | `vox_text::clean` (D-TXT-1..4) |
| 3.7 sentenceSpans | `vox_text::segment` |
| 3.8 chunkText | `vox_text::chunk` |
| 3.9 truncateAtBoundary | Gone (unused in the original) |
| 4.1–4.7, 4.9, 4.10 Engine | `vox_tts::engine` (D-NET-2/4/5; one engine lock) |
| 4.5 contexts | `vox_text::reader::{context_before, context_after}` |
| 4.8 chunkDuration | `vox_tts::duration` |
| 5 Mock | `vox_tts::mock` |
| 6 WAV | `vox_tts::wav` |
| 7 Alignment → timing | `vox_core::timing` (+ `vox_audio::timeline`) |
| 8 JS behaviours | `vox_core::js` + owners per row (regex dialect in vox-text, base64 in vox-tts, collation D-VO-1) |
| 9 Constants | `vox_core::constants` |

**reader-playback.md**

| § | Owner |
|---|---|
| 0 Conventions | `vox_core::{text16, js}`; §5.6 here |
| 1, 1.1 Architecture, payloads | §3 here; types in vox-core |
| 2.1 Constants | `vox_core::constants` |
| 2.2 Entry points | `core::{reader, selection, hotkeys, bridge, media}`, `tray`, `ui::*` (flows §4) |
| 2.3 read() | `core::reader::read` |
| 2.4 Text helpers | `vox_text::reader`; `vox_core::history::count_words` |
| 2.5 readClipboard | `core::reader::read_clipboard` (core worker) + `vox_win::clipboard::read_clipboard_text` → `Command::ClipboardText` |
| 2.6 replay | `core::reader::replay` + `on_replay_loaded` (`AudioCache::load_async`) |
| 2.7 previewReplay / clipAlignment | `vox_tts::cached` |
| 2.8 handleCli / parseCliArgs | `vox_core::cli`, `core::reader` (Cli handler) |
| 2.9 togglePlayback | `core::reader::toggle_playback` |
| 2.10 command/stop/shutdown | `core::reader::command`, `core::lifecycle` |
| 2.11 Toast routing | `core::toasts` |
| 2.12 Session tracking | `core::sessions` |
| 2.13 Engine events | `core::reader::on_engine_event` + `on_cache_saved` (`AudioCache::save_async` → `Command::CacheSaved`) |
| 2.14 pruneCache | `core::reader::prune_cache` + `vox_store::audio_cache::prune` (index-only, queued deletions) |
| 3 Engine contract | `vox_tts::engine` |
| 4.1–4.10 AudioQueuePlayer | `vox_audio::player` (+ timeline, clock, output, decode, stretch, resample) |
| 5 Timing helpers | `vox_core::timing`, `vox_text::segment::{sentence_starts, soft_splits, word_spans}` |
| 6 Output warmer | `vox_audio::warmer` (+ output keep-alive) |
| 7 Widget controller | `ui::widget` (auto-hide, holds, close), `core::reader` (rate/volume apply-on-change, `core::reactions` #12) |
| 8.1 Window visibility | `core::toasts` / `core::reader` (Show intents), `windowing::widget_window` |
| 8.2–8.8 Presentation rules | `vox_core::format` + `ui::widget`; highlighting `vox_text::highlight` |
| 8.9 UI refresh policy | `ui::mod` 30 Hz snapshot timer (§3.3) |
| 8.10 Tray | `ui::tray_view`, `tray` |
| 9 Media session | `vox_win::media`, `core::media` |
| 10 Persisted fields | `core::{reader, sessions}` |
| 11, 11.1 Ledger | `vox_core::listen_log`, `vox_store::analytics`, `vox_core::history::listened_fraction` |
| 12 Edge cases | split by owner in §9: vox-audio scenarios #2–#5, #7–#14, #21–#28; vox-app reader/sessions/stores #1, #6, #15–#20, #30; vox-tts `play_cached` |

**selection.md**

| § | Owner |
|---|---|
| 0 Files, 1 Data flow | `vox_win::{selection, gestures, hook, clipboard, keys, foreground}` |
| 2.1 Settings consumed | `vox_win::selection::SelectionConfig` |
| 2.2 Commands | `vox_win::selection::SelectionService` (`whenReady`/`captureSelection` Gone: unused) |
| 2.3 Events | `vox_win::selection::SelectionEvent` → `core::selection` / `UiSink` |
| 2.4 Notice copy | `vox_core::copy` |
| 2.5 Data shapes | `vox_core::read::ForegroundInfo`, `vox_win::foreground::WindowInfo`, `vox_win::gestures::GestureTarget` |
| 3 Native deps & degradation | `vox_win::selection` (hook failure retried, D-SEL-1) |
| 4, 4.1–4.3 Hook events, timing, key codes | `vox_win::hook` (priority, watchdog) + `vox_win::selection` consumer (libuiohook click counter, modifier flags from `GetAsyncKeyState` / tracked mask, Ctrl+Alt+Del zeroing), `gestures::{is_modifier_vk, is_navigation_vk, is_neutral_vk}` |
| 5, 5.1–5.8 (5.5.1–5.5.9) Gesture machine | `vox_win::gestures`; §5.6 application in `vox_win::selection` |
| 6.1–6.4 Chip plumbing | `vox_win::selection` (dedupe, bounds, hook-side hit test), `ui::chip` + `windowing::chip_window` |
| 7.1–7.3 Classification, excluded apps, password managers | `vox_win::{selection, foreground}`; normalisation `vox_core::settings::normalize_excluded_apps` |
| 8.1–8.11 Capture pipeline | `vox_win::selection` clipboard worker (+ `clipboard`, `keys`; `CaptureEnv` seam); order D-SEL-2 (copy-on-select before the UIPI probe); no-sequence fallback Gone (D-SEL-5) |
| 9.1–9.4 Clipboard | `vox_win::clipboard` (R-Clipboard) |
| 10.1–10.3 Synthetic keys | `vox_win::keys` |
| 11.1–11.6 Foreground info | `vox_win::foreground` |
| 12, 12.1 Win32 catalogue, AltGr | `vox_win::*`; `vox_win::hotkeys::alt_gr_character` |
| 13 Quirks 1–20 | §8 D-SEL-* |
| 14 Native notes | `vox_win::{hook, selection, clipboard, keys}` |

**shell.md**

| § | Owner |
|---|---|
| 1.1 Environment | `vox_core::app::RuntimeFlags`, `runtime` |
| 1.2 Paths | `runtime`, `vox_store::paths` |
| 1.3 Logging | `vox_store::log` |
| 2.1 Switches | `vox_core::cli` |
| 2.2 Acting on arguments | `core` (Cli handler) + `core::reader` |
| 3.1 Before the UI toolkit | `app::run` (steps 1–4; Chromium switches Gone, media keys live) |
| 3.2 Bootstrap | `app::run`, `core::Core` startup |
| 3.3 Second instance | `vox_win::single_instance` (per-user names, first pipe instance in `acquire`), `app` (own `Cli` enqueued, listener started right after `acquire`), `core` (deferred FIFO) |
| 3.4 Startup visibility | `app::should_show_main_at_start`, `app::startup_route` |
| 3.5 Quit | `core::lifecycle` (watchdog, bounded steps), `ui::mod` (`UiUpdate::Quit`: window-state save, hard exit), `app` |
| 3.6 Session end | `vox_win::system` (+ 2.5 s fallback flush in the handler), `core::lifecycle::session_end` |
| 3.7 Close requests | `windowing::main_window` (close → tray/quit), overlay subclass → `WindowEvent::CloseRequested` → `core::lifecycle`, `vox_win::system` (hidden window) |
| 3.8 Crash recovery & hardening | Gone (in-process native UI) |
| 4.1 Selection wiring | `app` (event routing), `core::selection` |
| 4.2 Bridge glue | `core::bridge` |
| 4.3 Explorer verb sync | `core::integrations`, `vox_win::registry` |
| 4.4 Launch at login | `core::integrations`, `vox_win::registry::apply_launch_at_login` |
| 5.0 Settings the shell reads | `vox_core::settings` |
| 5.1 Dispatcher | `core::reactions` (+ table §4.15 here) |
| 5.2 Retention debounce | `core::retention`, `core::timers` |
| 5.3 Toast helper | `core::toasts` (no readiness wait: UiSink buffers) |
| 5.4 Shell toast copy | `vox_core::copy`, `vox_core::hotkey::describe_failures` |
| 5.5 Display changes | `vox_win::system` → `core` → `WidgetIntent::Reposition`, chip hide |
| 5.6 Theme | `vox_core::settings::resolve_theme`, `vox_win::registry::apps_use_light_theme`, `core` (`UiUpdate::Theme`), `windowing::main_window` (DWM caption) |
| 6.1 Windows overview | `windowing::*` (attributes hook, pre-created overlays), `vox_win::window` (overlay subclass, `assert_topmost`) |
| 6.2 Main window | `windowing::main_window` (+ geometry persistence, R-Main-window) |
| 6.3.1 Widget role | `windowing::widget_window` |
| 6.3.2 Readiness | Gone |
| 6.3.3 Size reports | `windowing::widget_window::resize` (from `size-changed`) |
| 6.3.4 Placement | `vox_core::geometry` + `windowing::widget_window` + `windowing::dpi` |
| 6.3.5 Dragging | `windowing::widget_window::start_drag` (winit `drag_window`) |
| 6.3.6 Hover | `windowing::widget_window` → `ui::widget` |
| 6.3.7 Widget UI hover/auto-hide/toasts | `ui::widget` |
| 6.4 Pure geometry | `vox_core::geometry` |
| 6.5.1–6.5.3 Chip | `ui::chip`, `windowing::chip_window`, `vox_core::geometry::place_chip` |
| 6.6 UI ↔ shell messages | `ui::*` callbacks → `Command`s (§3.2) |
| 7.1–7.4 Tray | `tray` (`vox_win::notify_icon` + muda), `ui::tray_view` (icon redrawn for the new design, DESIGN.md "Tray"; D-SH-18) |
| 8.1–8.8 Hotkeys | `vox_core::hotkey`, `vox_win::hotkeys`, `core::hotkeys` |
| 9 Screenshot harness | `shots` (in-app), `scripts/shots.ps1` runner (P-APP-INT), `tools/vox-shot` for static pages |
| 10 Packaging/installer | Out of scope (R-Installer); portable layout D-INT-13; exe icon + VERSIONINFO + manifest embedded by `build.rs` (D-SH-20); AUMID registered at run time (D-SH-19) |
| 11 Constants | `vox_core::constants` |
| 12 Pitfalls 1–11 | §8 D-SH-* |

**persistence.md**

| § | Owner |
|---|---|
| 1.1–1.3 Profile, flags, layout | `vox_store::paths`, `vox_core::app::RuntimeFlags`; §6.1 here |
| 2.1–2.4 Durable JSON engine | `vox_store::fsutil` |
| 2.5 JsonFile | `vox_store::json_doc` |
| 3.1–3.5 settings.json | `vox_core::settings`, `vox_store::settings_store` |
| 4.1–4.5 API key | `vox_store::secrets` (+ R-API-key) |
| 5.1–5.4, 5.8–5.10 History | `vox_store::history_store`, `vox_core::history` |
| 5.5 Ledger | `vox_core::listen_log` |
| 5.6 Preview-only mode | `core::reader`, `vox_tts::cached`, `vox_store::audio_cache::redact_text` |
| 5.7 Who writes which field | `core::{reader, sessions}` |
| 6.1–6.9 Audio cache | `vox_store::audio_cache` |
| 6.10 Startup reconciliation | `core::retention::reconcile_*` |
| 7.1–7.5 Analytics | `vox_store::analytics` |
| 7.6 Display-side derivations | `vox_core::charts`, `ui::analytics` |
| 8.1–8.5, Appendix A Demo data | `vox_store::demo` |
| 9 Logging | `vox_store::log` |
| 10.1–10.3 Lifecycle | `app`, `core::lifecycle` |
| 11 Porting hazards 1–10 | `vox_core::{text16, json, js, time}`, vox-store (D-P-*) |
| 12 Test vectors | vox-store tests (§9) |

**integrations.md**

| § | Owner |
|---|---|
| 1.1 Constants | `vox_core::constants` |
| 1.2 Listener lifecycle | `vox_bridge::server` |
| 1.3 App-level glue | `core::bridge` |
| 1.4 Request pipeline | `vox_bridge::pipeline` |
| 1.5 Response format | `vox_bridge::pipeline::{json_response, error_response}` |
| 1.6.1–1.6.3 Routes | `vox_bridge::pipeline` + `core::bridge` |
| 1.7 Connection behaviour | `vox_bridge::server` (own accept loop, D-INT-16) |
| 1.8 Security model | `vox_bridge::pipeline` |
| 1.9 Port mismatch / Settings UI | `ui::settings` (Integrations), `ui/pages/settings/integrations.slint` |
| 1.10 Wire vectors | vox-bridge tests |
| 2.1–2.4 Browser extension | shipped unchanged (`browser-extension/`, copied next to `vox.exe`); `runtime::extension_dir` |
| 3.1–3.5, 3.7 Explorer verb | `vox_win::registry`, `core::integrations`, `core::reader::read_file` |
| 3.6 Uninstaller | Deferred with R-Installer: the future installer must delete the nine `…\shell\VoxRead` keys and the `Run`/`StartupApproved\Run` `Vox` values, skipped on update (D-INT-17) |
| 4.1–4.3 Launch at login | `vox_win::registry::apply_launch_at_login`, `core::integrations`, `app` (`--hidden`) |
| 5.1–5.10 File text extraction | `vox_text::extract` |
| 6 Rust porting notes | owners above (the bridge: its own accept loop + httparse instead of hyper, D-INT-16) |
| 7 Open questions 1–15 | §8 D-INT-* |

**ui-main.md**

| § | Owner |
|---|---|
| 0 Conventions | `vox_core::format`, `Brand`, D-COPY-1 |
| 1.1 Backend calls | `Command`s (§3.2) |
| 1.2 Data shapes | vox-core types |
| 1.3 App store | `ui::shell` (route), `UiUpdate::{Settings, Player, AppInfo, Navigate, Toast}`; failure UI D-UI-16 |
| 1.4 Freshness | `core::timers` (Midnight) + `UiUpdate::Refresh(Day)` + `ui::*` 60 s tick (`PageController::on_tick`), page wake via `ShellController::on_main_window_activity` (`on_wake`); credits wake in `core::credits` |
| 1.5 Credits state machine | `core::credits` (moved to the core so every page agrees) |
| 1.6 Read actions | `ui::*` controllers (`ReadStarted` handling, replay failure toasts) |
| 1.7 Toast system | `ui::shell::ToastStackState`, `ui/pages/shell/toasts.slint` |
| 1.8 Settings-derived helpers | `vox_core::hotkey::display_labels`, `ui::convert::keycaps` |
| 2.1 Routing | `ui::shell::RouteHistory`, `vox_core::app::AppRoute` |
| 2.2 Boot and theme | `ui::mod`, `core` (`UiUpdate::Theme`) |
| 2.3 Onboarding gate | `ui::shell::effective_page` |
| 2.4 Shell regions | `ui/windows/main-window.slint` (P-UI-SHELL); page enter/leave `ShellController` + `PageController` |
| 2.5 Top bar | `ui/windows/main-window.slint` header row: title, page actions from page data, Send to tray, theme toggle (D-UM-28); native caption buttons |
| 2.6 Sidebar | `ui::shell` + `ui/pages/shell/sidebar.slint` |
| 2.7 Mini player | `ui::shell` (`MiniPlayerData`) |
| 2.8 Global shortcuts | `ui/windows/main-window.slint` root FocusScope (Ctrl+1..5 → `navigate`, Ctrl+K, Ctrl+F) + `windowing::main_window` dispatcher (X1/X2 → `ShellController::back/forward`) |
| 2.9 Crash boundary | Gone (Rust controllers cannot crash a page; panics are caught at the callback boundary and logged) |
| 3.1–3.11 Home | `ui::home` + `ui/pages/home.slint` (+ `core::queries::QueryHome`) |
| 4.1–4.13 History | `ui::history` + `ui/pages/history.slint` (+ `core::queries`) |
| 5.1–5.14 Analytics | `ui::analytics` + `ui/pages/analytics.slint` (+ `vox_core::charts`) |
| 6.1 CreditsSummary | `ui::convert::credits_data` + `vox_core::format` |
| 6.2 Source labels/icons | `vox_core::history::ReadSource::{label, icon}` |
| 7 Reserved | — |
| 8 Dev-only pieces | Gone (gallery: `ui/previews/gallery.slint`) |
| 9 Platform behaviours | `ui::*`, `windowing::*` (keyboard focus, wheel, tooltips), reduce motion via the `Motion` global (`vox_win::window::reduce_motion`, `SystemEvent::MotionChanged`) |
| 10, 10.1–10.5 Onboarding steps | `ui::onboarding` + `ui/pages/onboarding*.slint` |
| 11.1–11.4 Format helpers | `vox_core::{format, charts, history}` (implemented, tested) |
| 12 Inconsistencies 1–27 | §8 D-UM-* |
| 13 Test vectors | vox-core tests (done) + vox-app UI controller tests |

**ui-settings-widget.md**

| § | Owner |
|---|---|
| 0 Conventions | `vox_core::format`, D-COPY-1 |
| 1.1 Optimistic settings | `ui::settings` |
| 1.2 Loadable fetch | `ui::{voices, settings}` (+ `core::catalog`) |
| 1.3 App info | `core::queries::QueryAppInfo` → `UiUpdate::AppInfo` |
| 1.4 Error text cleaning | `vox_core::format::clean_error` (used everywhere, D-USW-14) |
| 1.5 Toasts | `ui::shell` |
| 1.6 Confirmation dialog | `ui::shell` dialog slot + `ui/pages/shell/dialog.slint` |
| 1.7 Slider rows, 1.8 controls | `ui/design/*` (done) + page files |
| 1.9 Theme | `core` (`UiUpdate::Theme`), `Theme.dark` on the main window; widget and chip always dark (D-USW-20); first paint from `UiInit` |
| 2.1 Layout/nav/deep links | `ui::settings` (deep links), `ui/pages/settings.slint` (scroll spy, a documented Slint-side exception) |
| 2.2–2.10 Sections | `ui::settings` + `ui/pages/settings/*.slint` |
| 2.11 Section badges | `ui::settings` |
| 2.12 Binding table | `ui::settings` (`settings::patch_at`) |
| 3.1–3.4, 3.6 Hotkey strings/labels/AltGr/validation | `vox_core::hotkey`, `vox_win::hotkeys::alt_gr_character_active_layout` |
| 3.5 Recorder | `ui::hotkey_recorder`, `core::hotkeys` (suspension) |
| 4.1–4.5, 4.7 Voices page | `ui::voices` + `ui/pages/voices.slint`, `vox_core::voices` |
| 4.6 Sample preview | `core::catalog` + preview `Player` |
| 5.1–5.3 Onboarding | `ui::onboarding` + `ui/pages/onboarding*.slint` |
| 6.1–6.12, 6.14 Widget | `ui::widget` + `ui/windows/widget-window.slint` (no self-sizing; `card-key`), `windowing::widget_window` (one `set_bounds`) |
| 6.13 Media session | `core::media` |
| 7 Chip | `ui::chip` + `ui/windows/chip-window.slint`, `windowing::chip_window` |
| 8 Toast ids/copy | `ui::*` (ids as listed), `vox_core::copy` |
| 9 Platform notes | `vox_win::*`, `windowing::*` |
| 10 Edge cases | tests (§9) |
| 11 Decisions 1–19 | §8 D-USW-* |

**BRIEF.md / DESIGN.md**: BRIEF goals and rulings → §8 R-*; DESIGN.md → `ui/design` (done), pages (P-UI-*), `vox-art`
(done), `tray` icon drawing, `windowing::main_window` (backdrop-coloured caption).

---

## 3. Threads, channels and ownership

### 3.1 Thread inventory

| Thread | Created by | Lifetime | Does | Talks to |
|---|---|---|---|---|
| **main (UI)** | process | whole run | Slint/winit event loop, the three windows (+ their native event dispatchers), the tray icon (`NotifyIcon` hidden window) and muda menu, page controllers, widget/chip controllers, art cache, 30 Hz snapshot timer, native window treatment, the shots harness (async on the loop). Never blocks, never does I/O except tiny HWND calls and the quit-time `window-state.json` save. | core via `CoreHandle::send`; `Player::snapshot()` / the shots harness's direct `start_session`/`fail_session`; `SelectionService::{chip_activated, chip_dismissed, set_chip_bounds}` |
| **core** | `core::Core::spawn` | until the quit sequence ends | Owns `Services` (stores, client, engine, both players' handles, selection, hotkeys, media, bridge) and all reader/credits/session/toast-router state. `crossbeam_channel::select!` over the command receiver and the next `Timers` deadline. Never blocks on the network or on long disk work (cache I/O is the cache's worker; clipboard reads, file extraction, catalog and exports are core workers). | `UiSink` (→ main), every service |
| art worker | `ui::art` | app | Renders vox-art images at 2×; results posted with `slint::invoke_from_event_loop` | main |
| core workers | `core::{reader, catalog, queries}` (`std::thread::spawn`, short-lived) | per job | clipboard reads (`read_clipboard_text`, may block on delayed rendering), `--read-file` extraction, voices/models/account fetches, key validation, preview download, history export (save dialog + write), `CountExpiring` on large histories | core via `Command::{ClipboardText, ReadPrepared, Toast, Catalog}`; `UiUpdate` directly only for pure query answers (`Exported`, `ExpiringCount`) |
| cache I/O worker | `vox_store::AudioCache::open` | app | FIFO of every audio-cache disk mutation: saves, load + touch, redactions, folder deletions | core via `Command::{CacheSaved, ReplayLoaded}` (callbacks only enqueue) |
| TTS session worker | `vox_tts::TtsEngine::start/play_cached` | per session (+ wind-down after a stop) | sequential synthesis, retries, look-ahead waits (under the engine lock for every sink/event step) | sink → reading `Player` (enqueue); events → core |
| player thread ×2 | `vox_audio::Player::spawn` (reading, preview) | app (restarted by its supervisor on failure) | state machine, decode, stretch, resample, feed ring, warmer; ticks only while active | core (`PlayerEvent`), cpal stream |
| cpal callback | cpal/WASAPI | while a stream is open | copy blocks, volume, keep-alive, count frames, tag playback instants | lock-free ring + atomics |
| hook thread | `vox_win::hook::InputHook::start` | while a chip trigger is on | LL hooks (time-critical priority) + message loop; copies raw events to a channel | selection consumer |
| selection consumer | `vox_win::selection::SelectionService::new` | app | gesture machine, modifier flags, deadline timer, chip decisions, activation dedupe, hook watchdog; never touches the clipboard | `SelectionEvents` callback (→ `UiSink` for chip show/hide, → core otherwise), clipboard worker (posts `Capture` / `Flush`) |
| clipboard worker | `vox_win::selection::SelectionService::new` | app | strictly serial: captures, `flushRestore`, scheduled restores, the late-copy watch and `finishLateCopy`, the echo guard, `flush(timeout)`; never runs gesture logic | `SelectionEvents` callback (→ core) |
| hotkey thread | `vox_win::hotkeys::HotkeyService::spawn` | app | `RegisterHotKey`, `WM_HOTKEY` | core (`Command::Hotkey`) |
| SMTC thread | `vox_win::media::MediaSession::spawn` | app | SMTC (STA + hidden window) | core (`Command::Media`) |
| system watcher | `vox_win::system::SystemWatcher::spawn` | app | hidden top-level window: session end (+ 2.5 s fallback flush), display/theme/locale/clock/motion changes, resume, outside `WM_CLOSE` | core (`Command::System`, `Command::SessionEnd` with a reply); the settings and history stores (fallback `flush_sync`) |
| instance pipe | `vox_win::single_instance::InstanceServer::listen` (right after `acquire`, before the stores open) | app | receives forwarded argv/cwd | core channel (`Command::Cli`, buffered until the core runs) |
| bridge accept + connections | `vox_bridge::BridgeServer::start` | while enabled | own accept loop; one thread per connection (HTTP head/body deadlines, pipeline); `/read` waits on the core's reply until its deadline | core (`Command::Bridge`, `Command::Player`), atomics for `/health` |
| store threads | vox-store | app | settings writer, history writer, history notify timer | disk; history notify → core (`Command::HistoryChanged`) |
| quit watchdog | `core::lifecycle::shutdown` (first `Quit`) | detached | `std::process::exit(0)` after 7 s if the process is still alive | — |
| shots watchdog | `shots::start` | detached (shots mode) | writes `report.json` and exits 2 after 100 s | — |
| logging | caller threads | — | `tracing` subscriber appends synchronously under a mutex | `main.log` |

Ownership rules: the **core owns playback** (BRIEF) — the widget is a view; turning it off never touches audio. Only
the core thread mutates settings and history (stores are thread-safe, but single-writer keeps ordering simple; the
system watcher's fallback `flush_sync` only writes what is already in memory). The UI thread owns every Slint object.
Shared, lock-light state across threads: `Player` snapshots, the bridge's `playing: Arc<AtomicBool>`,
`SelectionService` handle, `SettingsStore::api_key` (read by TTS workers through the client's `KeySource`), the audio
cache index.

### 3.2 `Command` — everything sent to the core (`crates/vox-app/src/command.rs`)

One unbounded `crossbeam_channel` (`CoreHandle::send` never blocks; after the core exited sends are dropped). It is
created by `app::run` in step 1, so the instance listener and the primary's own `Cli` can enqueue before the core
exists. The core processes commands strictly in order. Requests the UI waits for carry a `RequestId`; the answer is a
`UiUpdate` with the same id (stale ids are dropped by the controllers) and is **always** sent — a handler that panics
is answered with its failure variant (`core::owed_answer` + `failure_update`, D-ARCH-3).

| Command | Sent by | Core handler → effect / reply |
|---|---|---|
| `ReadText{req, text, options}` | Home Quick read, onboarding "Read this", Settings "Test voice" (`copy::test_voice_sample`), widget retry fallback, shots harness | `ReadTextOptions::into_request` (2 000 000-unit cut, source default `manual`, caps) → `reader::read` → `UiUpdate::ReadStarted{req, result}` (if `req`) |
| `ReadClipboard{req}` | Home, tray (via `Tray`), hotkey (via `Hotkey`), CLI | `reader::read_clipboard`: a core worker runs `vox_win::clipboard::read_clipboard_text()` (Excel/Word delayed rendering can take seconds) → `ClipboardText{req, text}` |
| `ClipboardText{req, text}` | the clipboard worker | blank/`None` → info toast `Copy some text first` + `ReadStarted{req, clipboard_empty()}`; else `reader::read(ReadRequest::new(text, Clipboard))` (D-ARCH-5) → `ReadStarted{req}` |
| `Replay{req, read_id}` | Home Recent / Replay last / now-playing Try again, History, widget Try again | id validation (`Invalid id`) → `reader::replay(origin = Request)`: not in history / not cached → answered at once; `cache.has` → `cache.load_async` (I/O worker) |
| `ReplayLoaded{req, read_id, origin, chunks}` | the cache I/O worker | `reader::on_replay_loaded`: record re-read → play cached (≥ 1 chunk) or the miss path → `ReadStarted{req}`; `origin = TogglePlayback` turns a final failure into the `Nothing is playing…` toast |
| `ReadSelection(source)` | `Hotkey(ReadSelection)` (internal), chip activation is direct (§3.5) | `selection.read_selection(source)` (never blocks: queued on the clipboard worker) |
| `ReadPrepared(request)` | core workers (`--read-file` extraction) | `reader::read(request)` |
| `TogglePlayback` | `Hotkey(PlayPause)` | `reader::toggle_playback` |
| `Player(cmd)` | UI transport (Home, mini player, widget), tray, hotkeys, SMTC (via `Media`), bridge `/command` | `reader::command`: `sanitize_player_command` first (invalid → dropped); `Stop`: `had = player.snapshot().session.is_some() || engine.current_session_id().is_some()` (the engine's session covers a Stop before the player thread processed `Start`, D-ARCH-8), a pending deferred widget show is dropped → `player.command(Stop)` → `engine.stop()` → if `had`: `UiUpdate::Widget(Hide)` (every source); rate/volume are session-only; never shows the widget |
| `CycleRate` | widget speed pill | `next = next_rate(player.snapshot().state.rate)` (the player updates its snapshot synchronously) → `player.command(Rate(next))` + `settings.update({playbackRate: next})` |
| `UpdateSettings{req, patch}` | Settings, onboarding, tray checkboxes, widget drag (`WindowEvent`), Voices "Use voice", header theme toggle | must be a JSON object (else logged + `SettingsSaved{req, Err("Invalid settings patch")}`) → `settings.update` → reactions (§4.15) → **always** publishes `UiUpdate::Settings(current)` even when nothing changed (contracts.md §8 #9), then `SettingsSaved{req, Ok}` when `req` is set (a store write refused → `Err`) |
| `SetApiKey{req, key}` / `ClearApiKey{req}` | Settings Account, onboarding | worker validation → core stores → publishes `Settings` and `AppInfo` **before** `UiUpdate::ApiKey{req}` / `Done{req}` (§4.11, §4.19) |
| `SuspendHotkeys(bool)` | hotkey recorder start/stop (+ 20 s re-arm while recording) | `core::hotkeys::HotkeyState` (30 s safety timer → `UiUpdate::HotkeysResumed`); the recorder's `UpdateSettings` precedes `SuspendHotkeys(false)` on this FIFO channel, so the resume registers the new accelerator |
| `FetchVoices{refresh}`, `FetchModels{refresh}` | Voices, Settings, onboarding, Home (models) | worker (stamped with the key generation) → `Catalog` → `UiUpdate::{Voices, Models}{refresh, result}` (demo fallbacks without a key; stale generations dropped) |
| `RefreshCredits{force}` | credits Retry / Refresh buttons, Analytics mount | `core::credits::refresh` → `UiUpdate::Credits(CreditsUpdate{view, refreshing})`. Main-window wake is derived by the core from `WindowEvent::MainState` (never sent by the UI) |
| `PreviewVoice{voice_id}` / `StopPreview` | Voices, onboarding voice step | §4.10 → `UiUpdate::Preview` |
| `PreviewFailed{message}` | preview player | end the sample → `PreviewUpdate::Failed` |
| `QueryHistory{req, query, with_grand_total}` | History | `history.list` (+ unfiltered total) → `UiUpdate::HistoryPage` |
| `QueryHome{req}` | Home (immediate on enter / Retry / day / wake; 700 ms trailing debounce only for background changes) | `compute_analytics(all, 7d, Local::now())` + 5 newest → `UiUpdate::Home` |
| `QueryAnalytics{req, range}` | Analytics (immediate on mount, range change, Retry, day, wake; 700 ms trailing debounce only for `HistoryChanged`) | `compute_analytics` → `UiUpdate::Analytics` |
| `RemoveReads{req, ids}`, `ClearHistory{req}` | History delete-with-undo commit, clear dialogs (History, Settings Library) | `history.remove`/`clear` (`on_removed` → `AudioCache::remove`: index now, deletions queued) → `Done` |
| `ToggleFavorite{req, id}` | History | → `FavoriteToggled{req, Ok(record) / Err}` |
| `ExportHistory{req, owner}` | History, Settings Library (`owner` = the main window's HWND) | worker: save dialog owned by `owner` + `export_to(path, iso_utc_ms(now))` → `Exported` |
| `CountExpiring{req, days}` | Settings retention guard | `history.count_expiring` (worker for large histories) → `ExpiringCount{req, Ok(n) / Err}` |
| `QueryCacheStats{req}`, `ClearCache{req}` | Settings Library | `cache.stats()` / on the core thread `cache.clear()` (index-only) → `history.set_cached(<records with cached>, false)` → `CacheStats{req}` |
| `CopyText{req, text}` | History "Copy text", Settings copy chips | `vox_win::clipboard::write_clipboard_text` → `Done{req}`; the core never toasts for it (History toasts id `history-copy`) |
| `SaveAnalyticsRange(range)` | Analytics | `vox_store::ui_state::save_ui_state` |
| `SaveWindowState(state)` | main window (debounced 1 s, hide to tray) | `vox_store::window_state::save_window_state` (the quit-time save is the UI's own) |
| `ShowMain(route)` | toasts' actions, sidebar, second instance (tray Open is UI-local) | route validation (`AppRoute::validated`) → `UiUpdate::ShowMain` |
| `HideMain` | header "Send to tray" (D-UM-28) | `UiUpdate::HideMain` (the UI saves the placement first) |
| `OpenExternal{url, req}` | links (About, Get a key, History URL) | `is_http_url` else ignored (log) → `vox_win::shell::open_external`; with `req` → `Done{req, Err}` and the requester toasts (`copy::open_link_failed`, id `open-external`); without `req` failures are only logged (About) |
| `RevealPath(target)` | Settings (data folder, extension folder) | `vox_win::shell::reveal_path` (failure logged) |
| `QueryAppInfo` | Settings, onboarding | → `UiUpdate::AppInfo` |
| `Quit` | tray Quit, main close with close-to-tray off, shots end (the UI first commits a pending History undo batch) | `core::lifecycle::shutdown` (§4.17) |
| `Window(WindowEvent)` | UI thread | `MainState` → toast router (first one = `ui_ready`), credits wake (`WakeThrottle`); `WidgetVisible`; `WidgetDragged{x,y}` → `UpdateSettings({widget:{position:'remember', customX, customY}})`; `CloseRequested(Widget\|Chip)` → log + `session_end` + quit |
| `Tray(action)` | tray event handlers (UI thread) | `ReadClipboard`; `TogglePause` → `Player(Toggle)`; `Stop` → `Player(Stop)`; checkbox actions → `UpdateSettings`; `Quit`. `Open` / `OpenPage` never reach the core (handled on the UI thread) |
| `Hotkey(action)` | hotkey thread | readSelection → `selection.read_selection(Hotkey)`; readClipboard → `reader::read_clipboard`; playPause → `toggle_playback`; stop → `reader::command(Stop)` |
| `Selection(event)` | selection consumer / clipboard worker (non-chip events) | `core::selection` (`HookStatus` → `hook_status` → `AppInfo`) |
| `Media(action)` | SMTC thread | `core::media` (play/pause → Player; stop → `reader::command(Stop)`; previous/next → sentence skip; seek-to / seek-by → sanitized `Seek` / `Skip`) |
| `Bridge(Read{request, reply})` | bridge connection thread | `BridgeReadRequest::into_request(foreground_info())` → `reader::read` → `reply.send(result)` (the bridge stopped waiting after its deadline → 504; the read still runs) |
| `Cli{argv, cwd, from_second_instance}` | `app::run` (own argv, enqueued right after `acquire`; never in shots mode), instance pipe | deferred until core startup finished, then FIFO; `vox_core::cli::cli_outcome` → `reader` (`--read-text` > `--read-clipboard` > `--read-file`); nothing acted on + second instance + not `--hidden` → `ShowMain(None)` |
| `System(event)` | system watcher | DisplaysChanged → chip hide + `WidgetIntent::Reposition` + `UiUpdate::DisplaysChanged`; ThemeChanged → re-resolve theme; LocaleChanged → `set_process_locale(user_locale())` + `UiUpdate::Refresh(Locale)`; MotionChanged → `UiUpdate::Motion(reduce_motion())`; ClockChanged/Resumed → day check + credits reset check + `Refresh(Day)` (+ proxy re-read on Resumed); CloseRequested → `session_end` then `Quit` |
| `SessionEnd(reply)` | system watcher (blocks ≤ 5 s; flushes itself after 2.5 s) | `core::lifecycle::session_end` (sync) → `reply.send(())` |
| `Toast(toast)` | core workers (`--read-file` failure, …) | `core::toasts` routing exactly like a core-raised toast |
| `Engine(event)` | TTS session workers / the caller of `engine.stop()` | `reader::on_engine_event` |
| `CacheSaved{read_id, result}` | the cache I/O worker | `reader::on_cache_saved` (§4.5 step 9) |
| `PlayerReport(snapshot)` | reading player | `reader::on_player_report` (store, `engine.progress`, sessions fold, credits settle, SMTC, bridge atomic, `UiUpdate::Player`) |
| `PreviewReport(snapshot)` | preview player | `core::catalog` preview state → `UiUpdate::Preview` |
| `PlayerFailed{session_id, message}` | reading player supervisor | `engine.stop()` if it is the engine's session; log; `UiUpdate::Widget(Show)` if `widget.enabled` |
| `HistoryChanged` | history notify thread | 150 ms throttle (`TimerId::HistoryChangedThrottle`) → `UiUpdate::HistoryChanged` |
| `Catalog(result)` | core workers | `core::catalog` (results of an older key generation dropped) |

Validation stays in the core even though the UI is in-process (contracts.md §5.1): ids (`requireId`: non-empty, ≤ 128
units), routes (`AppRoute::validated`), URLs (`is_http_url`), patches (JSON object), player commands
(`sanitize_player_command` for every typed command — UI sliders, SMTC seeks, tray, hotkeys; the bridge path parses
with `parse_bridge_command` first). Slint `float`s that become settings go through `settings::quantize` in the UI
controllers.

### 3.3 `UiUpdate` — what the core publishes, and throttling (`crates/vox-app/src/update.rs`)

`UiSink::send` pushes onto a `Mutex<VecDeque>`; the first push of a batch schedules one
`slint::invoke_from_event_loop` that drains the queue and calls `AppUi::apply` for each update in order;
consecutive `UiUpdate::Player` entries in one batch collapse to the newest. Before `UiSink::attach()` (UI not created
yet) updates only queue, so nothing raised during startup is lost — except `ChipShow`/`ChipHide`, which are **dropped**
while detached (a replayed chip would appear at a stale cursor position).

| Update | Published when | Consumed by |
|---|---|---|
| `Settings(Arc<Settings>)` | every `UpdateSettings` (changed or not), key set/clear, startup | every controller, tray, widget (rate label), `Theme` follow-up |
| `SettingsSaved{req, result}` | right after the `Settings` an `UpdateSettings{req: Some}` produced | `ui::settings` overlay (pending set), onboarding finish, Voices "Use voice" |
| `Theme{dark}` | startup, `app.theme` change, OS theme change while `system` | `Theme.dark` on the **main window**, DWM caption colour, backdrop re-render (widget and chip stay dark, D-USW-20) |
| `Motion(bool)` | startup, `SystemEvent::MotionChanged` | `Motion.reduce` on all three windows |
| `Player(PlayerSnapshot)` | every reading-player report (status change, discrete change, every 500 ms while playing, the sessionless report after an explicit stop) | widget, mini player, Home now-playing, History live badges, tray |
| `Preview(PreviewUpdate)` | sample loading/playing/stopped/failed | Voices, onboarding voice step (`Failed` → their `voice-preview` toast) |
| `Credits(CreditsUpdate)` | a refresh starts (`refreshing: true`) and the latest one settles (`false`), even when the view is unchanged | sidebar, Home, Analytics (spinner), Settings Account, onboarding |
| `AppInfo(Arc<AppInfo>)` | startup, `QueryAppInfo`, hook status, the end of **every** bridge / Explorer / launch-at-login reaction (even when skipped), key set/clear | Settings (Triggers notice, Integrations badge + re-check window, About), onboarding key notice |
| `Toast(Toast)` | every core toast (§4), incl. `Command::Toast` from workers | app toast stack **and** widget toast slot |
| `Navigate(route)` / `ShowMain(route)` / `HideMain` | toast actions, second instance, startup route, header Send to tray | `ui::shell` + `windowing::main_window` |
| `Widget(WidgetIntent::{Show{reposition}, Hide, Reposition})` | Show: read/replay start (if enabled), toast routing, player failure; Hide: `widget.enabled` off, **every stop while a session existed**; Reposition: placement change, display change | `windowing::widget_window` + `ui::widget` (the UI never infers a hide from an `idle` snapshot) |
| `ChipShow{x,y,mode}` / `ChipHide` | **directly from the selection callback** (not via the core; dropped while detached) | `ui::chip` + `windowing::chip_window` (ignored until `overlays_ready`) |
| `DisplaysChanged` | `SystemEvent::DisplaysChanged` | tray (`refresh_dpi`), widget/chip re-placement |
| `HotkeysResumed` | the 30 s suspension safety timer fired | `ui::hotkey_recorder` (stop recording, clear preview) |
| `HistoryChanged` | coalesced (store 100/400 ms + core 150 ms) | Home, History, Analytics, Settings cache stats |
| `HistoryPage`, `Home`, `Analytics`, `Voices{refresh}`, `Models{refresh}`, `ReadStarted`, `ApiKey`, `Done`, `CacheStats`, `Exported`, `ExpiringCount{Result}`, `FavoriteToggled{Result}` | answers to requests (always sent, failure variant after a panic) | the requesting controller (also when its page is not active) |
| `Refresh(Locale \| Day)` | locale change; midnight (+1 s), clock change, resume | all controllers rebuild; Home/Analytics re-query |
| `Quit` | end of the bounded part of the quit sequence | UI: save `window-state.json` (sync), destroy the tray, hide windows, arm 4 s hard exit, `slint::quit_event_loop()` |

Ordering guarantees: key set/clear → `Settings`, `AppInfo`, then `ApiKey{req}` / `Done{req}`; `UpdateSettings{req}` →
`Settings`, then `SettingsSaved{req}`.

**High-rate playback (BRIEF "about 30 Hz while playing")**: the core does not push frames. While the last reading
snapshot's status is `playing` or `loading` **and** at least one player surface is visible (widget, main window on
Home or with the mini player), the UI runs a 33 ms `slint::Timer` that calls `player.snapshot()` and updates only
what changed, keyed exactly like reader-playback.md §8.9 (`status | session | wantsPlay | buffering | starting |
warning | partialText | charIndex | floor(position×10) | round(duration×10) | estimate | round(buffered×1000) | rate |
volume | error`). The timer stops when the status leaves playing/loading or no surface is visible. Discrete changes
arrive as `UiUpdate::Player`. The tray and SMTC are fed only from discrete reports (≤ 2/s).

### 3.4 The UI thread

`ui::AppUi` (thread-local) owns `MainWindow`, `WidgetWindow`, `ChipWindow`, the `CoreHandle`, the reading `Player`
handle, the `SelectionService` handle, the shared state (settings, last snapshot, credits, app info, models), the
art cache, the tray and every controller. `AppUi::apply` updates the shared state from an update first, then fans it
out to the `ShellController` and the **active** page controller through the `PageController` interface (§7.3), then
applies what the controller sinks collected (toasts, navigation with `on_leave`/`on_enter`, dialog changes, in-out
field writes) and pushes the rebuilt structs. Each page controller keeps page-local state (filters, expanded rows,
undo batches, pending request ids, debounces via `slint::Timer`) and rebuilds its Slint struct from domain data with
vox-core's format helpers; `.slint` files never format. Art images come from `ui::art::ArtCache` (placeholder accent
image on a miss, re-apply on completion). Every Slint callback body is wrapped so a panic is caught, logged (`ui
callback panicked`) and does not unwind into Slint.

Native windows (D-SH-2/D-SH-3; `windowing` module docs):
- **Before any window exists** `app::run` installs the winit backend with a window-attributes hook: the overlays (`Vox
  Player`, `Vox Chip`, matched by title) are created `with_active(false)` (every later Slint `show()` uses
  `SW_SHOWNOACTIVATE`), skip-taskbar, always-on-top, not resizable, off-screen; the main window gets its resolved theme,
  saved placement and maximized state up front.
- `AppUi::create(init: UiInit, …)` applies Theme/Brand/Motion, the onboarding gate and every controller **before any
  show**, pre-creates both overlays (show at -32000,-32000, then hide, from `Timer::single_shot(0)`), installs the
  overlay subclass on each HWND (`vox_win::window::install_overlay_subclass`: enforces `WS_EX_NOACTIVATE |
  WS_EX_TOOLWINDOW | WS_EX_TOPMOST` against winit's style rewrites, no `WS_SYSMENU`, `MA_NOACTIVATE`, `WM_CLOSE` →
  `CloseRequested`) and only then sets `overlays_ready` (chip shows are ignored until then). The main window's DWM
  caption colour is applied after its HWND exists and before it is shown (only when `init.show_main`). Then the first
  `WindowEvent::MainState` is sent and the `UiSink` attaches.
- Visibility always goes through Slint (`show()`/`hide()`); `assert_topmost` re-asserts the z-order after a show;
  chip click-through uses winit `set_cursor_hittest`. Position and size of an overlay are applied together in one
  `set_bounds` (the `.slint` windows do not bind their own size).
- One native event dispatcher per window (`on_winit_window_event`): hotkey recorder, geometry tracking, X1/X2.
- `on_close_requested` returns `KeepWindowShown` on all three windows (main: close to tray or quit; overlays: the core
  quits).

### 3.5 How every source feeds in

- **Tray**: `vox_win::notify_icon::NotifyIcon` + muda event handlers run on the UI thread. `Activate` (left click,
  double click, keyboard select) and the menu's Open/History/Analytics/Settings are handled **there** (navigate + show
  + `SetForegroundWindow` while the click's foreground permission is valid); other items → `Command::Tray`.
- **Hotkeys**: `HotkeyService` thread → `HotkeyHandler` → `Command::Hotkey(action)`.
- **Hook / gestures**: hook thread → channel → selection consumer (`GestureMachine`) → `SelectionEvents`:
  `ChipShow/ChipHide` → `UiSink::send(UiUpdate::ChipShow/ChipHide)` (low latency, no core hop; dropped before the UI
  attaches); `NoSelection`, `Notice`, `HookStatus` → `Command::Selection`. Captures run on the clipboard worker, which
  emits `Read` / `NoSelection` / `Notice` → `Command::Selection`.
- **Chip presses**: chip UI `pressed(left)` on the UI thread → `ui::chip` hides (exit animation) and calls
  `selection.chip_activated()` / `chip_dismissed()` directly (non-blocking enqueue). The hook-side press detection
  inside the reported chip bounds de-duplicates within 300 ms (selection.md §6.3).
- **Bridge**: connection thread → `BridgeGlue::read(request, deadline)` sends `Command::Bridge(Read{request, reply})`
  and waits with `recv_deadline` (`None` → 504); `command` → `Command::Player`; `status` reads `playing: AtomicBool`
  (set by the core on every report).
- **Second instance**: a later `vox.exe` → `forward_to_primary` (per-user named pipe, server SID checked, JSON `{argv,
  cwd}`) → instance pipe thread (listening since right after the primary's `acquire`) →
  `Command::Cli{from_second_instance: true}`; the core defers CLI commands until its startup is done and handles them
  FIFO.
- **CLI at startup**: `app::run` enqueues `Command::Cli{argv, cwd, false}` right after `acquire` (before the listener
  starts, so it is first), not in shots mode.
- **SMTC**: media thread → `Command::Media`.
- **System**: watcher thread → `Command::System`; session end → `Command::SessionEnd(reply)` and wait ≤ 5 s (own
  `flush_sync` after 2.5 s).
- **Overlay close**: the overlay subclass's `WM_CLOSE` → `Command::Window(WindowEvent::CloseRequested(kind))`.
- **Engine / players / stores / workers / cache I/O**: see the `Command` table.

### 3.6 Startup ordering guarantees (replacing Electron's readiness latch)

1. The reading `Player` exists before the core starts, so `session_start` always precedes chunks (sink calls are
   FIFO into the player's channel).
2. `reader::read` registers the tracked session immediately after `engine.start` returns, on the core thread, before
   it processes the next command — player reports are `Command`s queued behind it, so no report precedes tracking
   (reader-playback.md §2.3 step 12). A later-chunk failure's `Stopped{error}` is enqueued before the player gets
   `done`, so it precedes the `ended` report (D-NET-5).
3. `UiSink` buffers until the UI attaches; toasts raised at startup are shown when the windows exist. Their extra
   route (widget / OS notification) is decided only after the first `WindowEvent::MainState` (the router's
   `ui_ready`), so a startup failure toast does not pop the widget beside a main window that is opening focused.
4. Forwarded CLI args are never lost and never overtake the primary's own: the channel exists from step 1, the
   primary enqueues its own `Cli` before the instance listener starts (right after `acquire`, which already created the
   first pipe instance), and the core handles `Cli` commands FIFO after its startup work (hotkeys, selection, bridge,
   integrations).
5. The first frame is right: `AppUi::create` receives `UiInit` (settings, resolved theme, app info, route, show
   decision, placement, Analytics range, reduce motion) built from the stores by `app::run`, and applies all of it to
   the windows and controllers before any window is shown; the overlays are styled before they can be seen.

### 3.7 Timers

Core (`core::timers::TimerId`): `RetentionInterval` (6 h, skipped while `RetentionChange` pending),
`RetentionChange` (60 s after the last change), `HotkeySuspend` (30 s safety), `HistoryChangedThrottle` (150 ms),
`CreditsAfterRead` (1.5 s after a read settled), `CreditsReset` (`nextResetUnix` + 5 s; long waits re-armed),
`Midnight` (local midnight + 1 s → `Refresh(Day)`, re-armed; also re-checked on resume/clock change), `MediaTick`
(1 s SMTC position refresh while playing), `PreloadOutput` (1 s after core startup, whatever the window visibility;
not with `null_output`). UI (`slint::Timer`): page debounces (Home/Analytics 700 ms for background refreshes only,
History search 220 ms, settings retention 450 ms, cache re-measure 2.5 s), toast timers (+hover pause) and the 180 ms
toast exits, widget auto-hide 4 s / hold retry 1.5 s, widget toast 2.5/5 s, chip exit 140 ms, hover poll 100 ms,
drag-end poll 50 ms, 60 s "now" tick, the 30 Hz snapshot timer, main-window geometry save debounce 1 s, hotkey
recorder hint 1.8 s / rejection 6 s / suspension re-arm 20 s, copy chip 1.4 s, the Settings re-check window (≤ 4 s,
ended early by the next `AppInfo`). Selection (vox-win): its own single deadline timer + the hook watchdog (10 s);
the clipboard worker's restore timers. Engine: retry sleeps and look-ahead waits on `CancelToken`. Lifecycle: the 7 s
quit watchdog thread; the shots harness's 100 s watchdog.

---

## 4. End-to-end flows

Notation: `UI` = main thread, `core` = core thread, `→` = enqueue/call. Function names are the skeleton's.

### 4.1 Right-click chip read ("Read with Vox" beside the context menu)

1. **Hint.** The user drag-selects in Chrome. Hook thread: `WM_LBUTTONDOWN/MOUSEMOVE/LBUTTONUP` → `RawInput` →
   selection consumer (modifier flags from a `GetAsyncKeyState` snapshot for mouse presses) →
   `GestureMachine::handle` (drag > 6 px with text-cursor evidence, target not Vox / excluded / password manager) →
   `Decision::HintSet(Drag)` (60 s TTL, single deadline timer).
2. **Chip show.** `WM_RBUTTONDOWN` while the hint is live (not fullscreen/presentation) →
   `Decision::ShowContextChip{x,y}` → `SelectionEvent::ChipShow{x,y,Context}` → `UiSink` →
   `UiUpdate::ChipShow`. UI (`ui::chip`, only once `overlays_ready`): cancel a pending hide, `ChipData{kind: context,
   label: Brand.read-with-label, art: voice-art dot, hiding: false, show-key+1}`; `windowing::chip_window::show_at`:
   monitor under the point (`vox_win::window::monitor_from_point`; a DPI hop first moves the hidden window onto it)
   → DIP → `vox_core::geometry::place_chip(Context, cursor, size, work area)` (beside the predicted menu, never on it)
   → physical → one `set_bounds`, click-through off (`set_cursor_hittest(true)`), Slint `show()` (no activation:
   `with_active(false)` + the overlay subclass) → `assert_topmost(raise = true)` (above the menu) → real rect →
   `selection.set_chip_bounds(Some(rect))`.
3. **Activation.** Left press on the chip: the chip window's `pressed(true)` (UI) → `ui::chip`: start the exit
   (bounds `None`, `hiding = true`, click-through, hide after 140 ms) and `selection.chip_activated()`; and/or the hook
   sees the press inside the chip bounds with our own no-activate window under the cursor → `ActivateChip(Context)`.
   The service de-duplicates within 300 ms, emits `ChipHide`, and posts a capture (`ContextChip`, context-menu path) to
   the clipboard worker. A non-left press → `chip_dismissed()`; presses while `hiding` are ignored.
4. **Capture** (clipboard worker, selection.md §8; order D-SEL-2): `flushRestore` → `foreground_window_info` →
   password manager → `Notice` (refused, nothing sent) → `copy_method_for`: copy-on-select terminals read the
   clipboard with no UIPI check; chord methods only: `input_blocked_by_uipi(hwnd, pid) == Some(true)` →
   `Notice(elevated_app_refused)` (no `NoSelection`, clipboard untouched) → Ctrl+C with Escape retry;
   `wait_modifiers_released(700 ms)`; settle 120 ms (the native menu closes); clipboard snapshot (all formats incl.
   `CF_HDROP`/`CF_DIB`); sequence number; `send_chord`; poll every 15 ms ≤ 450 ms (+ ≤ 1.5 s once changed with a
   text format listed); nothing → one Escape + retry; privacy markers → refuse + restore now; else read text; restore
   the snapshot 150 ms later (when `restoreClipboard`, read live) with the "do not record" markers (our own restores
   are exempt from the privacy check, `last_own_write`); nothing at all → `NoSelection` + 1.5 s late-copy watch.
5. **Read.** `SelectionEvent::Read{text, ContextChip, foreground}` → `Command::Selection` → `core::selection::request_for`
   (sourceApp/process/title) → `core::reader::read` (§4.5 steps 3–9).

### 4.2 Selection popup read ("Read" pill after selecting)

Same as 4.1 with `triggers.selectionPopup` (selection.md §5.5.8, §5.7): `ShowSelectionChip{x,y}` at the left-release
point, **only for mouse gestures that set a hint** (drag over text or with unknown evidence, double/triple click,
shift+click, double-click-drag or shift+drag), when `selectionPopup` is on and no D3D-fullscreen or presentation app
is running. **Keyboard selections (Shift+navigation, Ctrl+A) never show it**; they only set the hint that arms the
context chip. Chip mode `Selection`, label `Read`, placed above-right of the cursor (`place_chip(Selection, …)`; below
when there is no room above). Hidden after 4 s, on any left/middle/X press outside it, on the wheel, on any
non-modifier keydown, when `selectionPopup` is turned off, or replaced without a hide by a context chip on a right
press. Activation → `read_selection(SelectionPopup)` with the plain path (settle 40 ms, no Escape retry).

### 4.3 Hotkey "read selection" (default Ctrl+Alt+R)

`WM_HOTKEY` (hotkey thread, `MOD_NOREPEAT`) → `Command::Hotkey(ReadSelection)` → core →
`selection.read_selection(Hotkey)` (never blocks the core: queued on the clipboard worker; ignored while a capture is
queued or running) → capture as 4.1 step 4 without the context-menu
settle/Escape retry → `Read{…, Hotkey, fg}` → `reader::read`. Works with chips off and with the hook unavailable. The
hotkey's own keydown clears any hint (quirk kept, D-SEL-Q1).

### 4.4 Read clipboard (hotkey Ctrl+Alt+V, tray, Home button, `--read-clipboard`)

`Command::{Hotkey(ReadClipboard) | Tray(ReadClipboard) | ReadClipboard{req} | Cli}` → `reader::read_clipboard(req)`:
a core worker runs `vox_win::clipboard::read_clipboard_text()` (failure counts as empty; it may block on Excel/Word
delayed rendering, so never on the core) → `Command::ClipboardText{req, text}` → blank → toast info `Copy some text
first`, `ReadResult::clipboard_empty()`; else `read(ReadRequest::new(text, Clipboard))` (no app/title).
`ReadStarted{req}` answers the Home button's request id (its spinner ends either way).

### 4.5 Quick read (Home), and one read end to end

1. UI: `home-quick-read(text)` (Read or Ctrl+Enter; enabled while `quick-read-can-read` = the trimmed text is not
   empty, computed in Rust; ignored while a read is already busy) → `read-busy = true` → `Command::ReadText{req, text,
   ReadTextOptions{source: Manual}}`.
2. core: `into_request` (2 000 000-unit cut, caps) → `reader::read(req)` (proxy re-read when the last check is ≥ 30 s
   old).
3. **Key gate**: no key and not mock → toast error `Add your ElevenLabs API key to start reading` + `Add key` →
   settings/account; `ReadResult::no_api_key()`; no record.
4. `pre_cut(text, max(maxChars × 4, 200 000))` → `clean_text` (panic → JS-trimmed raw, caught with `catch_unwind`)
   → empty → toast `Nothing to read in that selection` + `nothing_to_read()` → `truncate_text(maxChars)`; any cut →
   toast info `Reading the first 20,000 characters` (en-US number).
5. Record (`new_record`): UUID, `createdAt` now, text or 160-unit **char-safe** preview, `textTruncated`, `charCount`
   = UTF-16 length, `wordCount` = Unicode word rule, voice/model, source + metadata (title trimmed, ≤ 200 units).
   `history.add` when `saveHistory`.
6. `engine.start(StartSessionOptions{read_id, text, title: preview_title(text, 64), voice_id, voice_name,
   source_app, settings})` → (a superseded session's `Stopped{billed}` is emitted synchronously first) →
   synchronously `sink.session_start` → `PlayerSink` → `player.start_session` (`loading`, warmer prepares). Panic in
   `start` (programming error) → record error + toast + `failed_with_id`.
7. `sessions.track(session_id, read_id, saved, from_cache = false, 0, false)`; `widget.enabled` → (deferred to the
   player's first report of that session, `reader::show_widget_for`, so the card is never built from the previous
   session's snapshot; immediate when the player already has it; a Stop drops it)
   `UiUpdate::Widget(Show{reposition: false})` (positions only if hidden); return `ReadResult::started(read_id)` →
   `UiUpdate::ReadStarted{req}` → Home clears `read-busy` (a failure or a panic answer → the `read-failed` toast).
8. Engine worker: chunks 0 and 1 back to back, later chunks when `delivered − position < 120 s`; each `sink.chunk` →
   `player.add_chunk` (decode on the player thread). Player: warmer gate (≤ 3 s) → `playing`; reports →
   `Command::PlayerReport` → `reader::on_player_report`: store, `engine.progress(session, position)`,
   `sessions.on_state(&snapshot)` → `history.update` patches (listenedSec, completed, maxPositionSec, ledger at the
   current hour; first report and every status change write, else ≥ 2 s; a snapshot warning marks the session
   partial), credits settle detection, SMTC update, bridge `playing` atomic, `UiUpdate::Player`.
9. Engine `Complete{chunks, billed}` → the record still exists and is saved (not from cache) → patch
   `audioDurationSec = round1(Σ finite durations)` and `charactersBilled = max(0, js_round(billed))` at once; if
   `cacheAudio` and chunks exist → `cache.save_async(id, chunks | without_text(chunks) for preview-only)` on the cache's
   I/O worker → `Command::CacheSaved{read_id, result}` → `reader::on_cache_saved`: `Err` → log `audio cache save
   failed` (`cached` stays false); `Ok` but the record was deleted meanwhile → `cache.remove(id)` (no orphan); `Ok` →
   `prune_cache([read_id])` → `set_cached(evicted, false)` → patch `cached = true`. Record gone at `Complete` →
   nothing cached.
10. Player `ended` → `completed` written; `ui::widget` auto-hides after 4 s (holds: hover, toast) →
    `windowing::widget_window::hide` → `WindowEvent::WidgetVisible(false)`. Credits refresh 1.5 s after the read
    settled (forced).

### 4.6 Browser bridge: read selection, read page, commands, health

- **Read selection** (extension menu / Alt+Shift+R / popup) and **Read page** (the extension extracts the page text
  itself with `extractPage(20000)`) both arrive as `POST /read {text, url?, title?}`. `vox_bridge` worker:
  `check_request` (OPTIONS → 403; `Host` ∈ {`127.0.0.1:<port>`, `localhost:<port>`}; web/`null`/`file:` Origin →
  403; path; method; `x-vox-client: browser-extension`) → `check_body_headers` (JSON content type, ≤ 2 MiB) → body
  within 15 s → `parse_json_body` (`1e400` → `null`) → `parse_read_body` → `BridgeGlue::read(request, deadline)` →
  `Command::Bridge(Read{request, reply})`.
  core: `request.into_request(vox_win::foreground::foreground_info())` (browser name only for a known browser
  process, else `Browser`; `sourceUrl` only http(s) ≤ 4096; title → reader trims to 200) → `reader::read` → reply →
  200 `ReadResult` JSON (`ok:false` results are still 200). No reply within 15 s (or the core gone) → `read` returns
  `None` → 504 `Timed out waiting for the app` (the read is not cancelled).
- **Commands** (popup pause/resume/stop): `POST /command` → `parse_command_body` (strict, 400 with the exact message)
  → `BridgeGlue::command` → `Command::Player(cmd)` → 200 `{ok:true}` immediately.
- **Health**: `GET|HEAD /health` → `{ok:true, app:"Vox", version, playing}` (`playing` from the atomic the core sets
  on every report; `loading`/`paused` → false).
- **Lifecycle**: `core::bridge::sync_server` at startup and when `triggers.browserExtension` or `server.port`
  changes (not in shots mode): same port running → nothing; else (re)start; failure → log + toast `Browser bridge
  couldn’t use port <N>. Pick another port in Settings` (`Open settings` → settings/integrations, not in demo); a
  fresh `AppInfo` follows **every** sync, also when nothing changed (Integrations badge / port note; ends the Settings
  re-check window). Turning the bridge off closes the listener and every connection abortively, so turning it back on
  (or relaunching right after a quit) binds the same port again (D-INT-16).

### 4.7 Explorer "Read with Vox" (`--read-file`)

1. Explorer runs `vox.exe --read-file "C:\…\notes.md"`. If Vox runs: the new process is **Secondary**
   (`single_instance::acquire`) → `AllowSetForegroundWindow(primary)` → `forward_to_primary({argv, cwd})` → exit 0. If
   not: this process becomes the primary and handles its own argv after startup (main window not shown: a read
   action is present).
2. core `Cli`: `parse_cli_args`; priority `--read-text` > `--read-clipboard` > `--read-file`;
   `resolve_read_file(cwd, path)` (the second instance's cwd).
3. File extraction runs on a **core worker** (`vox_text::read_text_file(path, maxChars + 1000)`: ≤ 5 MiB, encoding
   sniffing, binary rejection, HTML/RTF/Markdown conversion) → `Command::ReadPrepared(ReadRequest{text, source:
   Explorer, source_app: "Explorer", source_process: "explorer.exe", source_title: file name})` → `reader::read`.
   Any error → the worker logs warn `read-file failed: <e>` (never the text) and sends
   `Command::Toast(Toast::error(copy::couldnt_open(name)))` (`name` = the resolved path's file name, lossy), which
   `core::toasts` routes like any core toast — an Explorer launch rarely has the main window focused, so the widget or
   an OS notification shows it. A path with an unpaired surrogate (argv is read lossily) fails here the same way.

### 4.8 Second instance argv forwarding

Later launch → `acquire(instance_key)` = Secondary (per-user mutex `Local\<key>-<SID>`) → connect to
`\\.\pipe\<key>-<SID>` (waits ≤ 5 s), check the server process's user SID, `AllowSetForegroundWindow(pid)`, forward
`{argv, cwd}` → exit. The primary created that pipe instance inside `acquire` and started its listener right after,
before opening any store, so a launch during a slow startup is buffered, not lost. Primary pipe thread →
`Command::Cli{argv, cwd, from_second_instance: true}` → deferred until core startup finished (after the primary's own
`Cli`) → handled like §4.7 step 2 (`cli_outcome`); if no read action was acted on and `--hidden` is absent →
`UiUpdate::ShowMain(None)` (restore, show, raise, focus — the secondary's `AllowSetForegroundWindow` lets the focus
change succeed). A forwarding failure (pipe busy/absent) is logged to stderr by the secondary, which still exits.
Another user's launches never reach this user's primary (D-SH-5).

### 4.9 Replay from history: free cached replay vs re-synthesis

`Command::Replay{req, read_id}` → `reader::replay(req, id, Request)`:
1. `history.get(id)` missing → `ReadResult::not_in_history()` (no readId; UI toasts `replay-<id>`).
2. **Cache hit** (`cache.has(id)` → `cache.load_async(id)` on the I/O worker — up to ~100 MB, never on the core
   thread; it touches `lastAccess` → `Command::ReplayLoaded{chunks}` → the record is re-read → ≥ 1 chunk): session text =
   `rec.text`, or for preview-only records `vox_tts::cached::preview_replay(rec.text, chunks)` (preview + `…`
   placeholders, clipped alignments) → `engine.play_cached(PlayCachedOptions{read_id, text, title:
   preview_title(rec.text), voice_id: rec.voice_id, voice_name, source_app: rec.source_app, settings (current rate,
   volume, highlight), chunks})` (sentences from `sentence_starts` over that session text) → `sessions.track(…,
   from_cache: true, base = rec.listened_sec/rec.completed)` →
   widget Show → `ReadResult::started(id)` (same id). No key needed, nothing billed; listening accumulates into the
   original record and the ledger credits the replay hour.
3. **Miss** (no entry, or the load returned `None`): `rec.cached` → `history.set_cached([id], false)`;
   `rec.text_truncated` → `ReadResult::not_cached(id)`
   (UI toast); else `read(ReadRequest{text: rec.text, source: Replay, source_app/process/title/url from the record,
   voice: VoiceOverride{rec.voice_id || settings voice, rec.voice_name}})` → a **new** record, billed, current model
   and other settings, every gate/toast of `read`.

### 4.10 Voice preview (Voices page, onboarding voice step)

`voices-preview(id)` → `Command::PreviewVoice{voice_id}` → `core::catalog`: same voice playing/loading → stop it
(`preview_player.command(Stop)`, `PreviewUpdate::Stopped`); else cancel the previous download (`CancelToken`),
`PreviewUpdate::Loading(id)`, pause the reading player if `playing`/`loading` (`reader::command(Pause)`; onboarding
too, D-USW-2), worker `client.fetch_preview(preview_url, cancel)` (no key header, 15 s) → `Catalog(Preview{voice_id,
result})` → still wanted → one-chunk session on the **preview player** (`TtsSessionStart{from_cache: true, rate: 1,
volume: settings.volume, text: ""…}`, chunk `audio/mpeg`, `finish_session`) → `PreviewReport`s → `Playing(id)`; ended/
stopped → `Stopped`. A reading-player transition **into** `playing` stops the sample. Failure → `Stopped` + toast id
`voice-preview` (`Couldn’t play this sample. Check your connection.` for network errors, else `Couldn’t play this
sample.`) raised by the Voices / onboarding controller from `PreviewUpdate::Failed` (never a core toast). A `volume`
change while a sample plays applies to it (reaction #12). Voices without `previewUrl` show no preview button.

### 4.11 Onboarding key validation (and Settings → Account)

1. Key step: `onboarding-connect-key(draft)` with the raw draft (Slint has no trim): Rust JS-trims it; blank →
   `Paste your API key first.` under the field, nothing sent; else `key.busy = true` → `Command::SetApiKey{req, key}`.
2. core: `> 256` units → `ApiKeyResult{ok:false, error: KEY_TOO_LONG, account: None}` at once; else a worker runs
   `client.validate_api_key(key)` (empty / whitespace checks, `GET /v1/user/subscription` 15 s, never retried) →
   `Catalog(ApiKey{req, key, result})`.
3. core: `result.ok` → `settings.set_api_key(Some(key))` (writes `secrets-dpapi.bin`; `key_changed`) → reactions:
   forced credits refresh (account from the validation is used as the first `Ready` value when present), voices/
   models refetch (key generation bumped: older catalog results are dropped), `AppInfo.api_key_notice` cleared →
   publishes `Settings` and `AppInfo` **then** `UiUpdate::ApiKey{req, result}`.
4. UI: ok → connected caption `{Tier} plan · {compact} characters left ({pct}%)` (or `Ready to read.`) and the
   warning (restricted key / out of credits) if any; draft cleared through `FieldWrites` (`onboarding-key-draft`);
   Continue → voice step. Error → `clean_error` message under the field (typing clears it). The Settings → Account
   form uses the same flow with `settings-key-draft` and closes on a state that already has `hasApiKey = true`.

### 4.12 Main window: show, hide, close, geometry

Created hidden at startup with its resolved theme, the placement from `window-state.json`
(`main_window::initial_placement`) and the maximized state set by the winit attributes hook, so the first show has
the right title bar and geometry; the DWM caption colour is applied after the HWND exists and before the first show.
`UiUpdate::ShowMain(route)` (and tray Activate/Open, handled on the UI thread) → navigate (if a route) →
`bring_to_front` (restore if minimized, show, `SetForegroundWindow`). Close (X, Alt+F4, outside `WM_CLOSE`; winit
`CloseRequested` → `on_close_requested` returns `KeepWindowShown`) → `closeToTray` → save placement
(`Command::SaveWindowState`) + hide (`WindowEvent::MainState{visible:false}`); else commit a pending History undo
batch and `Command::Quit`. Header "Send to tray" → `Command::HideMain` (the UI saves the placement first). Move/resize
→ save 1 s after the last change; on `UiUpdate::Quit` the UI saves synchronously. DWM caption = backdrop colour of the
resolved theme (`set_caption_theme`). Mouse X1/X2 = back/forward through the native event dispatcher (anywhere in the
window; ignored during onboarding; the stack skips `onboarding` routes, so the gate is never a back/forward target
once onboarding finished). Ctrl+1..5 / Ctrl+K / Ctrl+F in the root FocusScope (§7.4). The sidebar collapses to
a rail below 1100 DIP (`rail` property; D-UM-29).

### 4.13 Widget: show, hide, auto-hide, drag, position, DPI

- **Show** (`UiUpdate::Widget(Show{reposition})`): hidden or `reposition` → place: `monitor_from_point(cursor)` (for
  `remember` with saved coordinates: `windowing::dpi::electron_dip_to_phys(customX/Y)` → that monitor, else
  `nearest_monitor_dip`; a DPI hop first moves the hidden window onto the target monitor and lets
  `ScaleFactorChanged` settle) → `vox_core::geometry::place_widget` in that monitor's DIP (card size from
  `size-changed`, default 380×168) → physical → one `set_bounds` → record anchor + "moved by us" time → Slint `show()`
  (never activating: `with_active(false)` + the overlay subclass) → `assert_topmost(raise: false)` → start the 100 ms
  hover poll → `WindowEvent::WidgetVisible(true)`. Already visible → only re-assert topmost.
- **Resize** (`size-changed(w, h)` from the card; the `.slint` window does not bind its own size): sanitize 40..1200 ×
  24..900; hidden → keep top-left; visible → `resize_anchored(old, size, anchor, work area of the monitor of the old
  rect)`; position and size in **one** `set_bounds` (no frame grows past the work area); mark "moved by us" (500 ms
  grace).
- **Hide**: by the widget controller (Close — local hide first —, auto-hide 4 s after `ended` without warning — holds
  re-check every 1.5 s —, toast dismissal when `hides_on_dismiss`) or by the core (`widget.enabled` turned off;
  **every stop while a session existed**, from any source: UI, tray, hotkey, SMTC, bridge). The controller never
  infers a hide from an `idle` snapshot. Hiding stops the hover poll (`hovered` → false).
- **Drag**: pointer-down on a text handle → `drag-start` → `winit::Window::drag_window()`; a 50 ms poll waits for the
  left button to be released; if the window moved and > 500 ms since our own move →
  `phys_to_electron_dip(top-left)` → `WindowEvent::WidgetDragged{x, y}` → core `UpdateSettings({widget:{position:
  "remember", customX, customY}})` → reactions → `Widget(Reposition)` → the clamped `remember` placement (snaps back
  into the work area).
- **Placement change while visible** / **display change** (`UiUpdate::DisplaysChanged`, or winit `ScaleFactorChanged`
  on the visible widget) → re-place; while hidden → applies next show.
- **Close request** from outside (`WM_CLOSE` → subclass) → `WindowEvent::CloseRequested(Widget)` → the core logs
  `widget window got a close request; quitting`, runs `session_end` and quits. A graceful `taskkill /PID` (no `/F`)
  posts `WM_CLOSE` to the process's top-level windows; winit's event-target window is subclassed the same way
  (`install_overlay_subclass`), so it becomes `Command::System(CloseRequested)` and the app saves and quits.
- **First frame off-screen** (`windowing::reveal`): a hidden widget is shown parked off-screen at its final size and
  moved into place on the next turn after its first `RedrawRequested` at that size (250 ms fallback), so the previous
  card is never on screen. It parks only when the monitor nearest the parking point has the target monitor's scale
  (mixed-DPI setups otherwise show in place). Slint's rendering notifier never fires with this feature set (the Skia
  wgpu surface implements it only with `unstable-wgpu-30`), so `reveal` watches winit's `RedrawRequested` instead.
- **No per-pixel alpha**: Skia draws through wgpu on DirectX 12, whose window swap chain is opaque, so transparent
  pixels show black. The widget (radius 20 DIP, a pill when compact) and the chip (pill) are clipped to their shape
  with `SetWindowRgn` (`windowing::native`, aliased edge), and both windows draw an opaque panel under the card / pill
  (`opaque-backing`, set by Rust) so the enter / exit animations and the 0.96 panel never blend over black.
- **Snapshot**: `show_widget` reads the player's snapshot itself when the widget is hidden, and a read's show is
  published only after the player reported the new session (`reader::show_widget_for`), so the card is built for what
  plays.

### 4.14 Out of credits: stop-early and fatal

- **Later chunk** gets HTTP 401/402 `quota_exceeded` → `TtsError{code: quota, retryable: false}` → engine, under its
  lock: `finished = true` → `sink.error(non-fatal, quota, QUOTA)` (player `warning` → widget `Stops early · out of
  credits`, link → Settings → Account) → `EngineEvent::Stopped{billed, error}` → `sink.done(delivered)` (D-NET-5).
  Because `Stopped` is enqueued on the core channel before the player even receives `done`, and the player can report
  `ended` only after `done`, the core always processes `Stopped` first: session `partial = true`, patch
  `charactersBilled` + `error = Only part of the text could be synthesized`. (With Electron's order — done, then
  stopped — a player that had already drained the delivered audio after 1/2/4 s retries at 1.5–2× would report
  `ended` first and the read would be stored as completed.) Second guard: `Sessions::on_state` marks a non-cached
  session partial whenever its snapshot carries a `warning`. Playback reaches the end of the delivered audio →
  `ended` + warning → label `Stopped early`, **no auto-hide**, record not completed, nothing cached. Read settled →
  forced credits refresh after 1.5 s → low-credit state everywhere. Test (vox-app integration): mock server, chunk 1
  returns 503 four times while a `null_output` player has drained chunk 0 → `completed == false`, `error ==
  PARTIAL_READ_ERROR`.
- **First chunk** fails → `sink.error(fatal)` → player `error` → widget error card `Out of ElevenLabs credits` +
  `Open settings`; `EngineEvent::Failed` → record `error` + toast error with `Open settings` → settings/account
  (`error_needs_settings`).

### 4.15 Settings change → what reacts

Every `UpdateSettings`: `UiUpdate::Settings` (all surfaces, tray menu/tooltip refresh on the UI side), then
`SettingsSaved{req}` when the patch carried a request id. Then, for an effective change
(`core::reactions::diff(next, prev, key_changed, shots)`):

| Setting | Reaction |
|---|---|
| `onboarded` | UI onboarding gate; startup route next launch |
| `voiceId`, `voiceName`, `modelId`, `outputFormat`, `voice.*`, `languageCode` | next read (UI labels refresh) |
| `playbackRate` / `volume` | value changed → `player.command(Rate/Volume)` now (applies to the current read; an unrelated write never re-applies them) + next session start values; `volume` also to the preview player while a sample is loading/playing; also applied **once at startup** (reader-playback.md §7) |
| `triggers.rightClickChip`, `selectionPopup`, `excludedApps`, `restoreClipboard` | `selection.apply_settings` (not shots); chip flags changed → hook (re)start/stop, retried after a failure (D-SEL-1) |
| `triggers.hotkeysEnabled`, `hotkeys.*` | re-register unless suspended/shots → failure toast (`Open settings` → triggers) |
| `triggers.mediaKeys` | `media.set_enabled(..)` live; **no** restart toast (R-Media-keys) |
| `triggers.browserExtension`, `server.port` | `bridge::sync_server` (+ failure toast), then `AppInfo` always (also when the same port already runs) |
| `triggers.explorerMenu` | forced Explorer verb install/remove (not demo) → failure toast `Couldn’t update the Explorer menu: …`; then `AppInfo` always (also in demo) |
| `widget.enabled` | true→false → `Widget(Hide)`; false→true nothing (shows with the next read/toast) |
| `widget.position`, `customX`, `customY` | visible → `Widget(Reposition)` (skipped when the same change disabled the widget) |
| `widget.autoHide`, `widget.highlightWords`, `widget.compact` | UI widget controller (auto-hide sync, live highlight, layout) |
| `app.launchAtLogin`, `app.startMinimized` | `apply_launch_at_login` (not demo), then `AppInfo` |
| `app.closeToTray` | read at close time |
| `app.theme` | resolve with the OS → `UiUpdate::Theme` (main window, DWM caption, backdrop; the widget and chip stay dark) |
| `library.saveHistory`, `storeText`, `text.*` | next read |
| `library.cacheAudio` | next completed synthesis |
| `library.cacheLimitMB` decreased | `prune_cache([])` now (log `cache limit lowered: evicted N cached reads`) |
| `library.retentionDays` | arm `RetentionChange` (60 s after the last change, then-current value) |
| `hasApiKey` / key replaced | key generation bumped, forced credits refresh, voices + models refetch, `AppInfo` (key notice) |

### 4.16 Media keys and the transport hotkeys

`MediaSession` (SMTC) is enabled from `triggers.mediaKeys` at startup and live on change; it is fed from the core
(`core::media::media_state(snapshot, media_keys)` on every report and on the 1 s `MediaTick` while playing:
active = media keys on ∧ session ∧ status ≠ error ∧ status ≠ idle — after an explicit stop the player also sends a
sessionless `idle` report, so SMTC drops the stopped read; title = session title or `Vox`; artist = voice; album =
`Vox`). Actions (media thread → `Command::Media`): play/pause → `Player(Play|Pause)`; stop → `reader::command(Stop)`
(= widget Close: it hides the widget when a session existed, like every stop); previous/next → sentence skip; seek-to /
seek-by → `sanitize_player_command(Seek | Skip{seconds})` (OS-supplied `f64`s). They work with the widget hidden or
disabled (the core owns playback). Play/pause **hotkey** → `reader::toggle_playback` (status ≠ idle or engine session
→ `Toggle`; else replay the newest record if it is cached — loaded on the cache's I/O worker, origin
`TogglePlayback`; else toast `Nothing is playing. Select some text and press Ctrl+Alt+R`). Stop hotkey / tray Stop /
bridge stop / Home and mini-player Stop → `Player(Stop)` → `reader::command`: engine stopped and, when a session
existed, `UiUpdate::Widget(Hide)` from the core.

### 4.17 Startup and quit

Startup = `app::run` steps 1–10 (module docs of `app.rs`; the ordering guarantees are §3.6). Quit: before the UI
sends `Command::Quit` it commits a pending History undo batch (its `RemoveReads` precede `Quit`); `Command::Quit`
(first request; later ones ignored) → `core::lifecycle::shutdown`:
1. arm the detached **7 s watchdog** (`SHUTDOWN_WATCHDOG_MS`: logs `forced exit after shutdown timeout`, then
   `std::process::exit(0)`), so a hung service can never leave a live tray over a dead core;
2. log `shutting down`; cancel retention/suspension/preload timers; unregister hotkeys; `selection.stop()` (flags only);
3. persist playback: `sessions.persist_all(now, Some(&player.snapshot()))` (exact values, D-RP-6);
4. `engine.stop()` — it emits `Stopped{billed_so_far}` synchronously onto the channel — then drain the queued
   commands with `rx.try_iter()`, dispatching only `Command::Engine(..)` (billing known at stop time is persisted;
   everything else is dropped);
5. in parallel ≤ 3 s: `settings.flush()`, `history.flush()`, `selection.flush(3 s)`, `cache.flush(..)`,
   `bridge.stop()`;
6. publish `UiUpdate::Quit` → the UI saves `window-state.json` synchronously (`main_window::current_state`), destroys
   the tray, hides the windows, arms its 4 s hard exit, quits the loop;
7. `player.shutdown()` ×2 and `media.shutdown()`, each on a helper thread joined ≤ 500 ms; the core loop ends →
   `app::run` joins the core and returns.
Session end: `SystemEvent`/`SessionEnd` → `lifecycle::session_end` (persist playback with the current snapshot,
`settings.flush_sync()`, `history.flush_sync()`, never vetoes, idempotent); if the core has not replied within 2.5 s
the watcher's handler runs both `flush_sync`s itself. A pending History undo batch is dropped at log-off and on
core-originated quits (D-UM-32). Overlay close request → log, `session_end`, quit.

### 4.18 Electron-instance detection ("Do not run both builds at once")

Primary instance only, before logging and before any store opens: `vox_win::single_instance::electron_instance_running
(profile.root())` (Chromium's `Chrome_MessageWindow` titled with the profile path; `lockfile` fallback) → true →
`vox_win::shell::message_box(copy::old_version_running_title(), copy::old_version_running_body())` → exit 0. The
demo profile is checked against `Vox-Demo` (a normal Electron Vox does not block a demo Rust run). Not repeated later
(an Electron start while the Rust app runs is the user's choice; both would then share files — documented risk).

### 4.19 API key migration

`SettingsStore::open` → `secrets::load_api_key`: `secrets-dpapi.bin` present → use it (zero bytes = cleared,
unreadable → `KeyProblem::Unreadable`); absent and `secrets.bin` present → decrypt (`v10` via `Local State` AES key,
or raw DPAPI) → success: save to `secrets-dpapi.bin` (log `migrated the API key from the previous version`),
`KeySource::Migrated`; failure → `KeyProblem::MigrationFailed` (log reason, never the key). No key and
`ELEVENLABS_API_KEY` set → environment key. The problem becomes `AppInfo.api_key_notice`
(`copy::key_migration_failed()` / `key_unreadable()`), shown on Settings → Account and the onboarding key step; when
onboarded the main window opens on settings/account at startup (D-KEY-3). Reads without a key show the usual
`Add your ElevenLabs API key…` toast. Setting a key clears the problem. `secrets.bin` is never written.

### 4.20 Theme switching

`app.theme` ∈ System/Paper/Night. The first value comes from `UiInit` (resolved by `app::run` before any window is
shown, so the wrong theme never flashes). Later: the core resolves `resolve_theme(mode, os_dark)`
(`apps_use_light_theme()`, re-read on `SystemEvent::ThemeChanged` while `system`) → `UiUpdate::Theme{dark}` → UI sets
`Theme.dark` on the **main window**, re-renders the backdrop (`ArtCache::backdrop(w, h, dark)`), and
`set_caption_theme(hwnd, dark, backdrop rgb)`. The widget and the chip are always dark (`Theme.dark = true` set once;
ui-settings-widget.md §1.9, D-USW-20). Appearance page: `set-choice("app.theme", value)` → `UpdateSettings`; header
theme toggle → `UpdateSettings({app:{theme: dark ? "paper" : "night"}})` (D-UM-28).

---

## 5. Audio design (vox-audio)

Behaviour reference: reader-playback.md §4 (player), §5 (timing), §6 (warmer), §12 (edge cases). This section fixes the
native mechanics that the old `<audio>`-per-chunk design got from Chromium.

### 5.1 Pipeline

```text
TtsChunk.audio (MP3 44.1/22.05 kHz mono, or mock WAV 22.05 kHz)
   │ player thread, ahead of time (chunk n+1 while n plays)
   ▼ decode::decode  — symphonia (MP3 with gapless trimming: FormatOptions{enable_gapless: true}), WAV via symphonia
   │                    → DecodedAudio{sample_rate, channels, f32 samples}; decoded duration replaces the engine's
   │                    when they differ by > 0.01 s (onChunkMeta)
   ▼ stretch::Wsola   — tempo = current rate (0.5–3, pitch preserved; rate 1 = bit-exact pass-through); state carried
   │                    across chunk boundaries (one continuous stream), reset only on seek / session change
   ▼ resample         — rubato (sinc, good quality) source rate → device rate; mono → device channels by duplication
   ▼ blocks ≈10 ms, each tagged BlockTag{generation, chunk, source_frame_end}
   ▼ rtrb SPSC ring (≈200 ms capacity; the player keeps ≥ 100 ms queued)
   ▼ cpal callback (WASAPI shared, f32): pop blocks of the current generation, × volume (atomic f32), keep-alive
     square wave (±1/32768, 10 ms half-period) when the ring is empty and the output is held, count rendered frames,
     record each popped block's device playback instant (`OutputCallbackInfo::timestamp().playback`), publish the
     tags (atomics)
```

The player thread loop: `crossbeam_channel::select!` over its message channel (start/chunk/done/fail/command/
preload/shutdown). A 10 ms tick that tops up the ring, polls the warmer, advances chunk boundaries from the played
tags, runs the listening clock and emits reports (status change, discrete change, 500 ms while playing) runs **only
while a session is active** (loading / playing / paused with queued audio), a warm-up is pending or the stream is
held; otherwise the thread blocks on its channel with the next warmer-release or device-poll deadline as the timeout
(BRIEF Goal 3: two idle players must not wake 200 times a second).

### 5.2 Player state and commands

Exactly reader-playback.md §4.3–§4.6 (state fields, `startSession` / `addChunk` / `finishSession` / `failSession`,
status machine, `play`/`pause`/`toggle`/`stop`/`seek`/`seekFraction`/`skipSeconds`/`skipSentence`/`setRate`/
`setVolume`). Native equivalents of element events:

| Browser event | Native source |
|---|---|
| `playing` (audible) | the first block of the current generation is reported played by the callback |
| `waiting` (stall) | ring under-run while `playing` and the current chunk still has frames to feed (should not happen; logged) |
| `ended` (chunk) | the played tag passes the last source frame of the chunk; with gapless joins the next chunk's blocks are already queued, so `advance` only moves `cur` and banks listening |
| `pause`/`play` by the OS | none: SMTC actions come through the core as commands (no external element control) |
| `error` (decode) | `decode` fails → chunk `failed` (keeps its timeline slot, skipped) |
| `NotAllowedError` | none (no autoplay policy natively) |

`pause` flushes the ring (new generation) and remembers the exact source position of the last played block, so
resume restarts from what was actually heard. `seek` flushes, resets WSOLA and resampler, re-decodes if needed and
refills from the target. `setRate` banks listening, changes WSOLA tempo for blocks produced from now on, and flushes the
ring so the new rate is heard within ~10 ms (re-queued from the last played position). `setVolume` only writes the
atomic. `setRate`/`setVolume` (and `start_session` from its payload) also write the clamped target into the shared
snapshot **synchronously**, before enqueueing, so `snapshot()` reflects them at once (the core's `CycleRate` reads
`snapshot().state.rate`; reader-playback.md §8.5). An explicit `stop` flushes, reports `idle` with the final
`listenedSec` **and the session still attached** (the core's last fold), then a second `idle` report **without a
session** (rate and volume kept), so SMTC and the widget's view selection see "no session"; the implicit stop inside
`start_session` (a supersede) reports `[idle + old session, loading + new session]` with nothing in between. Every
spec `notify()` / `changed(report = false)` point (e.g. the `onChunkMeta` decoded-duration refinement while paused)
also emits a report. The output is released 30 s after a stop.

### 5.3 Gapless chunk joins (R-Audio)

Chunk *n+1* is decoded and its blocks are produced as soon as chunk *n*'s last blocks are queued, using the same WSOLA
and resampler state, so the device sees one continuous signal. MP3 encoder delay/padding are trimmed by symphonia's
gapless mode (ElevenLabs MP3s carry LAME/Xing info); without it the first decoder-delay frames of each chunk are
dropped by a fixed 1 105-sample offset at 44.1 kHz (529 at 22.05 kHz) — the owner verifies against a real ElevenLabs
file and records the result in a test. Chunk boundaries are tracked through `BlockTag::chunk` so positions and
"current chunk" remain exact even though there is no audible gap.

### 5.4 Position, duration, buffered and heard accounting

- `positionSec` = Σ `dur(chunks[0..cur])` + `t`, where `t` = (source frame of the **played** block of `cur`) /
  source rate. Each popped block carries its device playback instant (cpal `OutputCallbackInfo::timestamp().playback`,
  or `IAudioClock::GetPosition`); the played tag is the last block whose playback instant ≤ now, so position, the
  highlighted word and chunk `ended` follow what is heard (Bluetooth latency included; Chromium accounted for the sink
  delay too). `snapshot()` extrapolates by `(now − tag_seen_at) × rate` while playing (capped at the chunk duration) —
  a **display-only** interpolation. `dur(c)` per §4.8 (decoded or engine duration, else character estimate ≥ 0.1 s).
- `durationSec`, `durationIsEstimate`, `bufferedFraction`, `charIndex`, `totalChars` exactly per reader-playback.md
  §4.8/§4.10 (`vox_audio::timeline`, using `vox_core::timing::ChunkTiming` for in-chunk lookups).
- `listenedSec` = the listening clock (reader-playback.md §4.9): counts wall time only while `playing` and audible,
  capped by media progress / rate; banked before rate changes and seeks; per session. Media time = source seconds of
  the current chunk taken **only** from the device-reported played tag on the player thread, never from `snapshot()`'s
  extrapolation, so a system sleep or a stall credits nothing (reader-playback.md §4.9).
- **Heard** (`maxPositionSec`, `completed`, ledger) is folded by the core (`core::sessions`), never by the player.

### 5.5 Word and sentence timing

`TtsChunk.alignment` (`starts` only) → `ChunkTiming::char_at_time / time_at_char` (proportional mapping when
ElevenLabs' code-point count differs from the chunk's UTF-16 length; linear fallback without alignment).
`TtsSessionStart.sentences` (UTF-16 sentence starts from `vox_text::sentence_starts`, computed by the engine) drive
`skipSentence` (1.2 wall-second grace) and the widget's sentence. The UI builds the highlighted view with
`vox_text::highlight::sentence_view` over the session text (`Utf16Index` cached per session).

### 5.6 Text offset policy (R-Text-offsets) and where conversions happen

Every offset or length that is persisted, crosses a crate boundary, or is compared with a spec limit is **UTF-16
code units** (`u32`/`usize`): chunk ranges, `TtsChunk.text_start/end`, `PlayerState.char_index/total_chars`,
sentence starts, word spans, `charCount`, the 160-unit preview, the 64-unit title, 220/900 chunk limits, 300-unit
contexts, `maxChars`, cache manifest `textStart/textEnd`. Rust strings stay UTF-8. Conversions happen **only** through
`vox_core::text16` (`utf16_len`, `byte_index`, `utf16_offset`, `slice16`, `prefix16`, `Utf16Index`):

| Where | Conversion |
|---|---|
| vox-text (`clean` excepted, which is offset-free) | inputs `&str`; internal scanning may use bytes/chars; outputs converted to UTF-16 with `Utf16Index` before returning |
| vox-tts engine | chunk spans from vox-text; context windows cut with `prefix16`/`slice16`; billing fallback = `utf16_len(chunk)`; duration estimate = `utf16_len(text) / 15 / speed` |
| vox-tts mock | one alignment entry per UTF-16 unit (astral: char then `""`) |
| vox-audio | never slices text; uses offsets numerically |
| core reader | `charCount = utf16_len`, preview = `prefix16(text, 160)` (never splits a pair — D-C-17), truncation by vox-text |
| vox-store | persists offsets as given; cache manifests unchanged from Electron (no conversion) |
| UI | `Utf16Index::slice` before showing a highlighted range; `truncate` helpers count UTF-16 |
| JSON readers | lone surrogate escapes repaired to U+FFFD (`vox_core::json::repair_lone_surrogates`) |

Existing Electron caches therefore replay unchanged: their offsets are UTF-16 into the record's full text, exactly
what the player expects.

### 5.7 Output warmer

`vox_audio::warmer::OutputWarmer` implements reader-playback.md §6 with the audio clock = frames rendered since the
stream last started (`AudioOutput::rendered_frames`): `prepare` (cancel release; wake the stream; ready when ≥ 1 s
rendered, or given up after 3 s, or impossible), `release(30 s | 60 s)`, `open_failed` (retry creation after 60 s),
`preload` (vox-app calls `player.preload_output()` 1 s after core startup — `TimerId::PreloadOutput`, whatever the
window visibility, so a `--hidden` / launch-at-login start also gets it; skipped with `null_output` — so the first read
does not pay the stream construction). The keep-alive signal plays under speech for as long as the output is held. Calls per player
moment as in reader-playback.md §6 table.

### 5.8 Device changes and missing devices

The output follows the Windows default render device: the player polls `cpal::default_output_device()` every 2 s while
a stream exists (and handles the stream error callback). On change/loss: drop the stream, open the new default, refill
from the last played source position (no status change, log `audio device changed`). No device at all → simulated
clock (§2.5 notes); a device appearing later is picked up by the poll. This includes **mid-read**: a (re)open that
fails with `OutputError::NoDevice` (the only output was unplugged) switches the session to the simulated clock, keeping
position and status (reader-playback.md §6 "creation failure never waits", §12 #28).

### 5.9 Player failure recovery (R-Audio)

`Player` is a supervisor handle: the player thread runs under `catch_unwind`; a panic, or 3 stream (re)opens failing
with `OutputError::Stream` within 10 s (`NoDevice` never counts, §5.8), ends that thread. The supervisor starts a new thread, recreates the failed session's `SessionInfo`
(kept in the handle) in status `error` (`copy::PLAYBACK_FAILED` / `NO_AUDIO_OUTPUT`, code `unknown`), emits
`PlayerEvent::State` then `PlayerEvent::Failed`. The core stops the engine if that session is the engine's and shows
the widget's error card ("Try again" → replay or re-read).

### 5.10 The preview player

A second `Player` (config `name: "preview"`, own stream) plays one-chunk sample sessions; it never reports to the
reader, the engine or SMTC. The core pauses the reading player when a sample starts and stops the sample when reading
starts playing (§4.10).

---

## 6. Persistence compatibility (vox-store)

### 6.1 Files read and written

| File (under `%APPDATA%\Vox`, demo `Vox-Demo`) | Read | Written | Format / rules | Electron compatibility |
|---|---|---|---|---|
| `settings.json` (+`.bak`, `.tmp`, `.tmp-sync`, `.corrupt`) | startup | debounced 250 ms / 2 s; quit flush; session-end sync | pretty JSON, 2-space indent, LF, no trailing newline, Electron key order, JS numbers | same schema; the old app reads our file unchanged |
| `history.json` (+ same) | startup | debounced 2.5 s / 10 s; flush/sync | compact `{"version":1,"records":[…]}`, newest first, Electron field order | same |
| `secrets.bin` | startup, **only if `secrets-dpapi.bin` is absent** | never | Chromium `v10` (or raw DPAPI) | untouched (Electron keeps working with its key) |
| `secrets-dpapi.bin` (+`.tmp`) | startup | key set (raw DPAPI blob) / cleared (0 bytes) | new | ignored by Electron |
| `Session\Local State`, `Local State` | migration only | never | JSON `os_crypt.encrypted_key` | untouched |
| `audio-cache\<id>\manifest.json` + `<index>.<ext>` | startup scan, replay (I/O worker) | after a completed synthesis, touch on load, redact — all on the cache's I/O worker | compact manifest written last; UTF-16 offsets | identical layout both ways |
| `window-state.json` | startup (`app::run` → `UiInit`) | 1 s after move/resize, hide (core); quit (the UI, synchronously) | §6.5 | ignored by Electron |
| `ui-state.json` | startup (`app::run` → `UiInit::analytics_range`) | range change | `{"analyticsRange":"30d"}` pretty | ignored by Electron |
| `vox-icon.png` | — | (re)written at startup when missing or different, from the embedded `assets/vox-256.png` (not shots) | the app icon for the AUMID `IconUri` (D-SH-19) | ignored by Electron |
| `logs\main.log`, `main.1.log` | — | append | `<ISO-UTC> <LEVEL> [scope] text`, 1 MiB rotation, key masking | same format |
| export file (user path) | — | History export | pretty `{"app":"Vox","version":1,"exportedAt":…,"records":[…]}` via `<path>.tmp` | same |

Never create `cache\`. Temp files left by a crash are never read and simply overwritten later.

### 6.2 JSON naming, numbers and fallbacks

- Field names camelCase exactly as the Electron files (`serde(rename_all = "camelCase")` + explicit renames), field
  order = struct order (persistence.md §3.1, contracts.md §3.6), optional fields **omitted** (never `null`) except
  `widget.customX/customY`, which are `null` when unset.
- Numbers: integral values written without `.0`, others shortest round-trip, `-0` → `0`, non-finite → `null`
  (`vox_core::json::js_f64`, `to_js_pretty/compact`). Byte-identical output is not required (R-Profile) but this keeps
  the textual `needs_rewrite` comparison stable.
- Reading: lossy UTF-8, BOM stripped, lone `\uD8xx` escapes repaired, empty/whitespace = corrupt, numbers out of f64
  range treated as non-finite for that field only (`parse_lenient` pre-pass) — never fail a whole file for one
  field.
- **Invalid-field fallback**: settings → the base value (defaults on load, current settings on update; an invalid
  patch value silently keeps the current value — contracts.md §4.1); records → the per-field rules of contracts.md
  §3.6 (missing `id`/`createdAt` drops the record with a warning); manifests → persistence.md §6.3; unknown keys are
  dropped everywhere; a `settings.json` of the wrong shape (`null`, `[]`) loads as defaults and is rewritten on the
  next change (spec behaviour). A `history.json` of the wrong shape (neither an array nor an object whose `records`
  is an array: `{}`, `{"records":5}`, `null`, `42`) is treated as **corrupt** — `.corrupt` copy, `.bak` fallback,
  immediate rewrite — instead of silently loading zero records and overwriting `.bak` on the next mutation (D-P-16).
  Manifests normalise odd numbers / alignment elements instead of being deleted (D-P-15).
- **Corrupt main + good `.bak`** (R-Profile): copy main → `.corrupt`, load `.bak`, and **rewrite the main file
  immediately** (log `restored <file> from backup; rewriting it`). Missing main file = deliberate reset (no `.bak`
  fallback).
- `settings.json` is rewritten once when its re-serialised parsed form differs from the canonical form
  (`Settings::needs_rewrite`) — the migration mechanism (clamps, dropped keys, stray `hasApiKey`, key order).

### 6.3 History specifics

Records: `vox_core::history::ReadRecord::from_value` (sanitize) on load, `apply_patch` for typed updates, the
equality short-circuit (no write, no event), insertion order (newest first; ties: newer insertion first). Previews of
new preview-only records are cut char-safe (`prefix16`), so the Rust app never writes a lone surrogate; old files that
contain one load through the repair. Word count rule: the reader's Unicode rule everywhere (D-C-1) except the demo
generator, which keeps the whitespace rule its vectors depend on (D-P-14). Export: UTC date in the default file name
(D-C-3); `exportedAt` from `vox_core::time::iso_utc_ms`.

### 6.4 The API key: migration and the new `secrets-dpapi.bin` (R-API-key)

Algorithm in `vox_store::secrets` module docs and §4.19. Key points: `secrets.bin` is never modified; the migrated
or entered key lives in `secrets-dpapi.bin` as a plain DPAPI blob (current user, no entropy); clearing writes a
0-byte tombstone so `secrets.bin` is not re-migrated; DPAPI failures produce a visible notice, never a crash; the key
is never logged (the log masker is the last line of defence). Consequence (documented): a key entered in the Rust
app is not seen by the Electron build, which keeps its own `secrets.bin` key.

### 6.5 `window-state.json` (R-Main-window)

`{"version":1,"x":…,"y":…,"width":…,"height":…,"scale":…,"maximized":…}`: the **restored** rect in physical screen
pixels + the monitor scale at save time. Restore (`windowing::main_window::initial_placement`): rect must overlap a
current monitor's work area by ≥ 64×64 px, else default (1180×780 DIP centred on the primary work area); size
re-scaled by `monitor.scale / saved.scale` when the DPI changed, clamped ≥ 940×640 DIP and ≤ the work area;
maximized restored via `WPF_RESTORETOMAXIMIZED`. Saved on hide-to-tray, quit, and 1 s after the last move/resize.
Invalid file → default (no error UI).

### 6.6 `ui-state.json`

`{"analyticsRange":"7d|30d|90d|365d|all"}` (default 30d) — replaces the old `localStorage`. Read once at startup by
`app::run` (`UiInit::analytics_range`; no I/O on the UI thread); the Analytics controller keeps the range in memory
and saves every change with `Command::SaveAnalyticsRange`. Errors ignored. The Electron `vox.analytics.range`
localStorage value is **not** migrated: the first Rust run shows 30D.

### 6.7 Audio cache compatibility

Same folder names (`^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$`), chunk files (`^\d{1,6}\.(mp3|wav|ogg|bin)$`), manifest
fields and order, LRU by `lastAccess`, `bytes` accounting, validation and repair (persistence.md §6). Offsets are UTF-16
into the record's full text both ways, so Electron caches replay in Rust and vice versa. Startup reconciliation as
persistence.md §6.10: stale flags and redactions in `app::run` step 5; orphan removal in the core's startup work on
the core thread (index-only; the candidate set is the index at call time, so a read saved by a cold-start
`--read-file` is never deleted), before any queued `Cli`, bridge or selection command.

### 6.8 Logging

`vox_store::log`: `tracing` subscriber writing the Electron line format (`2026-10-09T00:20:30.735Z INFO  [scope] text`,
timestamp `vox_core::time::iso_utc_ms`; scope = tracing target), rotation at 1 MiB to `main.1.log`, masking (`sk_…` →
`sk_***` with ASCII word boundaries like JS `\b`, `xi-api-key…` → `***`),
argument cut at 2000 units, debug lines only with `VOX_DEBUG`, stderr echo in debug builds. Store warnings use scope
`data`. Never log read text or keys (reviewed in every package's definition of done).

---

## 7. UI contract (`ui/`)

### 7.1 Principles

- `ui/app.slint` (compiled by `crates/vox-app/build.rs`, style `fluent`) exports `Theme`, `Brand`, `Motion`, the three
  windows and every struct/enum of `ui/models.slint`; Rust uses the generated types from `vox_app::ui::generated`.
- **Stable list identity** (`ui::stable`): Slint's repeater drops every row instance when a list property gets a model
  of another identity, and the controllers build fresh `ModelRc`s on every `build`. Every push (`push_sidebar`,
  `push_toasts`, `push_page`) goes through `stable::stabilize`: the lists of the shown struct are updated **in place**
  (`set_row_data` for changed rows; keyed rows — toasts, Recent rows, choice items, History / Voices rows — inserted or
  removed by key; never `set_vec`), and an unchanged struct is not set again. Rows keep their instances (clicks, focus
  and hover survive the 30 Hz poll); lists a controller keeps itself are left alone. An empty `slint::Image` never
  compares equal in Slint 1.18.1, so a struct holding one is set again (its rows are still kept).
- **Rust formats, Slint presents.** Every dynamic string arrives ready to show (vox-core `format`/`copy`, locale-aware),
  every number that is displayed arrives as text (plus a 0..1 `float` where something is drawn proportionally), every
  image (voice/app art, backdrop) arrives as an `image` rendered by vox-art at 2×. `.slint` files contain no business
  logic, no formatting and no colours (only `Theme` tokens). Static copy may live in the pages, but the product name
  always comes from `Brand.app-name` / `Brand.read-with-label`, and apostrophes are `’`. Spoken / accessible strings
  are Rust-made too (`*-accessible`, `accessible`, `*-summary`, `*-spoken` fields; D-UM-30). Documented exceptions:
  the Settings scroll spy (`SettingsPage.active-section`, geometry only Slint knows), the root keyboard shortcuts
  (§7.4) and the slider labels while dragging (computed by Rust through the pure callback `settings-format-number`).
- **Callbacks carry intents, not state changes**: ids, option values, dotted settings keys, raw text. The controller
  validates, updates its state, sends `Command`s, and pushes a new struct. Slider values arrive as `float` (`f32`) and
  are snapped with `vox_core::settings::quantize` before they become a patch or a player command.
- **Stateful controls are never driven only by a one-way binding.** The design `Toggle`, `Segmented`, `Select` and
  `Slider` change their own value on interaction, which breaks a one-way binding; Rust's later values (an optimistic
  revert, Reset, the retention guard's cancel, a tray checkbox, a widget drag to "Last spot", a CycleRate, "Clear
  filters") would never reach the control. Every page/section therefore re-asserts its controls from `data` in a
  `changed data => { toggle.checked = data.x; seg.current-index = data.y-index; slider.value = data.z; }` handler,
  and the page structs carry `revision: int` (`SettingsData`, `VoicesData`, `HistoryData`), bumped by Rust on every
  rebuild that must override what the user did, so the handler fires even when the values look unchanged.
- **Text fields keep their own text.** A field's text is an `in-out` property of `MainWindow` (`onboarding-key-draft`,
  `settings-key-draft`, `settings-exclude-draft`, `settings-port-draft`, `voices-search-text`, `history-search-text`,
  `sidebar-search-text`) passed down with `<=>` to the page/section and the design field's `text`; Rust writes it only
  to reset or normalise it (`ui::FieldWrites`: strip the port draft, clear after an add/submit, "Clear filters", the
  stored port changed, Escape).
- **Interfaces are final; visuals are placeholders.** Page owners replace the bodies with `ui/design` components
  (`import { … } from "../design/index.slint"`) but keep every property and callback. Adding a field is allowed only
  through the architect (this section + the Rust filler).
- The main window forwards page callbacks with a page prefix (`home-…`, `history-…`, `settings-…`) so Rust binds
  every callback on the one `MainWindow` component.
- `Motion.reduce` (OS "show animations" off) turns every page/window animation into 0 ms (1 ms where an animation must
  still run): the widget's sentence fade, scroll transitions, the card enter animation, toast banners and exits, the
  chip's enter/exit, Settings smooth scroll. (The finished `ui/design` components keep their own short transitions.)
- Previews: each page owner adds `ui/previews/<page>.json` (+ a small preview `.slint` re-exporting `Theme`) and
  renders with `target/release/vox-shot ui/pages/<page>.slint --data ui/previews/<page>.json --out shots/<page>.png`
  in both themes.

### 7.2 Globals

| Global | Properties | Set by |
|---|---|---|
| `Theme` (`ui/design/theme.slint`) | `dark: bool` (+ read-only tokens) | `ui::mod`: the main window from `UiInit::dark` and `UiUpdate::Theme`; the widget and chip windows `true` once (always dark, D-USW-20) |
| `Brand` (`ui/models.slint`) | `app-name`, `read-with-label` | `ui::mod::AppUi::create` from `vox_core::constants::{APP_NAME, READ_WITH_LABEL}` on all three windows |
| `Motion` (`ui/models.slint`) | `reduce: bool` | `ui::mod` on all three windows from `UiInit::reduce_motion` and `UiUpdate::Motion` |

### 7.3 Windows and who fills them

| Window / component | Properties → Rust filler | Callbacks → handler |
|---|---|---|
| `MainWindow` (`ui/windows/main-window.slint`) | `page`, `onboarding`, `page-title`, `theme-dark`, `sidebar`, `toasts`, `dialog`, `settings-scroll-to/-nonce`, `backdrop` → `ui::shell` (+ `ui::art`); `home` → `ui::home`; `history` → `ui::history`; `analytics` → `ui::analytics`; `voices` → `ui::voices`; `settings` → `ui::settings`; `onboarding-data` → `ui::onboarding`; in-out text fields (`*-draft`, `*-search-text`) ↔ the owning controller through `FieldWrites`; `sidebar-focus-nonce`, `history-focus-nonce` (Slint-local); `rail` (out) read by `ui::shell` | shell callbacks (`navigate`, `sidebar-search`, `mini-*`, `credits-open`, `header-send-to-tray`, `header-toggle-theme`, `toast-*`, `dialog-*`) → `ui::shell`; `home-*` → `ui::home`; `history-*` → `ui::history` (incl. the header's `history-export` / `history-clear-all`); `analytics-*` → `ui::analytics` (incl. the header's `analytics-set-range` and `analytics-daily-width`); `voices-*` → `ui::voices`; `settings-*` → `ui::settings` (+ `ui::hotkey_recorder`; `settings-format-number` is a pure callback); `onboarding-*` → `ui::onboarding` |
| `WidgetWindow` (`ui/windows/widget-window.slint`) | `data: WidgetData` → `ui::widget` (from `PlayerSnapshot` + 30 Hz polling + toast slot); `card-width/-height` (out) → `windowing::widget_window::resize` via `size-changed` (the window never binds its own size) | `action(PlayerAction)` → `ui::widget` (toggle/previous/next → `Command::Player`; `cycle-rate` → `Command::CycleRate`; `close` → local hide + `Command::Player(Stop)`); `seek(f)` → `Player(SeekFraction)`; `error-action` → Open settings / Try again; `warning-clicked` → settings/account; `toast-action`; `drag-start` → `windowing::widget_window::start_drag`; `size-changed` |
| `ChipWindow` (`ui/windows/chip-window.slint`) | `data: ChipData` → `ui::chip`; `pill-width/-height` (out) | `pressed(left)` → `ui::chip` (`press_gate`) → `SelectionService::chip_activated/dismissed`; `size-changed` → `ui::chip` (sizes per mode) |

Page/section components and their fillers:

| Component (file) | Data property (struct) | Filled by | Callbacks handled by |
|---|---|---|---|
| `Sidebar` (`pages/shell/sidebar.slint`) | `data: SidebarData`, `page`, `rail`, `search-text` (in-out), `focus-search-nonce` | `ui::shell` | `ui::shell` (navigate, search → History with `NavIntent::Search`, mini transport → `Command::Player`, credits → settings/account) |
| `ToastStack` (`pages/shell/toasts.slint`) | `toasts: [ToastData]` (`leaving`, `key`), `centered` | `ui::shell::ToastStackState` | `ui::shell` (action → route or `UiToastAction::Page` → the page's `on_toast_action`, dismiss, hover pause) |
| `ConfirmDialog` (`pages/shell/dialog.slint`) | `data: DialogData` | `ui::shell` (one slot; ids: `history-clear` → History; `remove-key`, `retention`, `library-clear-history` → Settings) | `ui::shell` → owning controller's `on_dialog` |
| `HomePage` (`pages/home.slint`) | `data: HomeData` | `ui::home` | `ui::home` |
| `HistoryPage` (`pages/history.slint`) | `data: HistoryData`, `search-text` (in-out), `focus-search-nonce` | `ui::history` | `ui::history` |
| `AnalyticsPage` (`pages/analytics.slint`) | `data: AnalyticsData` | `ui::analytics` | `ui::analytics` (`daily-width-changed` → `set_plot_width`) |
| `VoicesPage` (`pages/voices.slint`) | `data: VoicesData`, `search-text` (in-out) | `ui::voices` | `ui::voices` |
| `SettingsPage` (`pages/settings.slint`) + sections `AccountSection`, `VoiceModelSection`, `PlaybackSection`, `TriggersSection`, `WidgetSection`, `AppearanceSection`, `LibrarySection`, `IntegrationsSection`, `AboutSection` (`pages/settings/*.slint`) | `data: SettingsData` (`account`, `voice`, `playback`, `triggers`, `widget`, `appearance`, `library`, `integrations`, `about`, `revision`), `scroll-to`, `scroll-nonce`, `active-section` (in-out, Slint scroll spy), `key-draft` / `exclude-draft` / `port-draft` (in-out) | `ui::settings` | `ui::settings` (generic `set-bool/number/choice(key, value)` with dotted keys → `vox_core::settings::patch_at`; `format-number` pure callback) |
| `OnboardingPage` (`pages/onboarding.slint`) + `WelcomeStep`, `ApiKeyStep`, `VoiceStep`, `TryStep`, `DoneStep` (`pages/onboarding/steps.slint`) | `data: OnboardingData`, `backdrop`, `key-draft` (in-out) | `ui::onboarding` | `ui::onboarding` |

Struct → filler (every struct of `ui/models.slint`): `ChoiceItem`, `KeyGroup`, `Fact`, `BadgeData` → `ui::convert`
helpers used by every page; `ToastData` → `ui::convert::toast_data` (app stack and widget slot); `DialogData` →
`ui::shell`; `CreditsData` → `ui::convert::credits_data(.., CreditsSurface, ..)` (sidebar line, Home, Analytics,
Settings Account, onboarding — per-surface copy); `BarData` → `ui::home` (Last 7 days) and `ui::analytics` (daily
chart); `NavItemData`, `MiniPlayerData`, `SidebarData` → `ui::shell`; `NowPlayingData`, `StatBlockData`, `WeekData`,
`RecentRow`, `WayRow`, `HomeData` → `ui::home`; `HistoryRow`, `HistoryData` → `ui::history`; `KpiData`,
`HeatCellData`, `ShareRow`, `AnalyticsData` → `ui::analytics`; `VoiceRow` → `ui::voices` (also reused by
`ui::onboarding` for the voice step); `VoicesData` → `ui::voices`; `AccountData`, `VoiceModelData`, `PlaybackData`,
`HotkeyRowData` (with `ui::hotkey_recorder`), `TriggersData`, `WidgetSettingsData`, `AppearanceData`, `LibraryData`,
`IntegrationsData`, `AboutData`, `SettingsData` → `ui::settings`; `KeyStepData`, `VoiceStepData`, `TryStepData`,
`OnboardingData` → `ui::onboarding`; `WidgetData` → `ui::widget`; `ChipData` → `ui::chip`.

**Controller interface** (final; `crates/vox-app/src/ui/mod.rs`, `shell.rs`):

```rust
pub struct UiCtx<'a> {
    pub core: &'a CoreHandle, pub settings: &'a Arc<Settings>, pub player: &'a PlayerSnapshot,
    pub credits: &'a CreditsUpdate, pub app_info: &'a Arc<AppInfo>,
    pub models: &'a Arc<Vec<TtsModel>>,            // the last Ok list, kept when the key goes away
    pub now_ms: f64, pub demo: bool, pub reduce_motion: bool,
    pub art: &'a mut ArtCache, pub toasts: &'a mut ToastSink, pub nav: &'a mut NavSink,
    pub dialogs: &'a mut DialogSink, pub fields: &'a mut FieldWrites,
}
pub trait PageController {
    type Data;
    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {}
    fn on_leave(&mut self, cx: &mut UiCtx) {}
    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool { false }   // dirty?
    fn on_tick(&mut self, cx: &mut UiCtx) {}                                       // 60 s
    fn on_wake(&mut self, cx: &mut UiCtx) {}                                       // ≤ once / 30 s per page
    fn on_toast_removed(&mut self, cx: &mut UiCtx, id: &str, why: ToastRemoval) {}
    fn on_toast_action(&mut self, cx: &mut UiCtx, id: &str) -> bool { false }
    fn on_dialog(&mut self, cx: &mut UiCtx, id: &str, confirmed: bool) -> bool { false }
    fn build(&self, cx: &mut UiCtx) -> Self::Data;
}
pub enum NavIntent { None, Search(String) }
pub enum UiToastAction { Page(AppPage) }
// ToastSink::{push(Toast, Option<UiToastAction>), dismiss(id)}; NavSink::{navigate, navigate_with};
// DialogSink::{open(DialogSpec), set_busy, close}; FieldWrites { settings_port_draft, …, sidebar_search_text }
// ShellController::{navigate(route, intent, onboarded) -> Navigation{left, entered, route, intent},
//                   back, forward, on_main_window_activity(focused, visible, now) -> Option<AppPage>, build_sidebar}
```

Each page controller implements `PageController` and exposes one method per callback of its page with `(&mut self, cx:
&mut UiCtx, …)` — e.g. `HomeController::{quick_read, quick_read_edited, read_clipboard, player_action, seek, replay,
replay_last, pause_live, resume_live, retry, credits_retry, open_page, open_settings}`, `HistoryController::{…,
flush}`, `AnalyticsController::{set_range, retry, refresh_credits, add_key, set_plot_width}`,
`SettingsController::{…, format_number(&self, key, value) -> String}` (signatures in Appendix A). `AppUi::apply`
updates the shared state, then calls `on_update` on the shell and the **active** page (inactive pages get only
`Refresh` and the answers to their own requests), applies the sinks (toasts → `ShellController::toasts`, navigation
→ `ShellController::navigate` then `on_leave` / `on_enter`, dialog changes, field writes) and pushes the rebuilt
structs. The 60 s tick and the wake (`ShellController::on_main_window_activity`) go to the active page.

### 7.4 Page ownership notes

- Shell (P-UI-SHELL, `main-window.slint` + `ui::shell`): the header row is drawn by `MainWindow` — title, the current
  page's actions rendered from the page data (History: `history.count-label` eyebrow, Export with
  `history.can-export` / `history.exporting`, Clear all; Analytics: `analytics.range-label` eyebrow and the range
  segmented control from `analytics.ranges` / `analytics.range`), then "Send to tray" and the theme toggle (label and
  icon from `theme-dark`), both hidden during onboarding (D-UM-28). A root FocusScope (`capture-key-pressed`; inert
  while `dialog.open` or `onboarding`): Ctrl+1..5 → `navigate` unless `TextInputInterface.text-input-focused`
  (D-UM-23); Ctrl+K → `sidebar-focus-nonce += 1`; Ctrl+F (no Alt/Shift/Meta) on History → `history-focus-nonce += 1`
  even while typing elsewhere; `/` on History when no text input is focused → the same. The sidebar search submits
  with Enter → `sidebar-search(text)` → History with `NavIntent::Search` (applied at once; the sidebar field is cleared
  through `FieldWrites`). Mouse X1/X2 are native (windowing). Toast stack: same-id push replaces in place (no removal,
  new `key`); exits animate 180 ms (`leaving`); a pushed-out or programmatically dismissed toast is absent at once. The
  dialog slot closes a dialog owned by the page being left; while a dialog is open the window is inert to page
  navigation.
- Home: `state` loading/ready/error (first-load error with Retry and `retrying`, R-Home); `week.title` = "Last 7
  days"; `RecentRow.resumable` for the paused live read (Resume → `home-resume-live` → `Command::Player(Play)`); the
  main transport button always sends `toggle` (Rust maps it); `home-replay-last` owns `replay-last-busy`; seek keys per
  D-UM-31; `quick-read-can-read` from Rust (Slint has no trim).
- History: delete-with-undo (toast id `history-remove` with `UiToastAction::Page(History)`; after every stack change
  commit when no `history-remove` toast is present — a leaving toast counts as present, a same-id replacement is never
  a removal; also on page leave, before Export / Clear all, before the UI sends `Quit`), clear dialog
  (`history-clear`), load more, expanded details (full text, notes, facts, URL), the nothing-yet sentence (`state-body`
  + keycaps + `state-body-after`), keyboard (Ctrl+F, `/` through `focus-search-nonce`), favourite results
  (`FavoriteToggled` `Result`, failure toast id `history-favorite`), copy (`Done` → toast `history-copy`).
- Analytics: range from `UiInit` (kept in memory, saved on change), immediate queries on mount / range / Retry / day /
  wake and a 700 ms debounce only for `HistoryChanged`, the plot width from `daily-width-changed` (bucketing in Rust;
  nothing drawn while unknown), all 24 hour labels with Slint-side thinning, empty banner when no reads **and** no
  listening, `retrying`, the accessible strings and live text of the keyboard-selected mark (D-UM-30).
- Settings: optimistic overlay keyed by `UpdateSettings{req}` / `SettingsSaved{req}` (unrelated `Settings` broadcasts
  never settle it), stateful controls re-asserted from `data` (`revision`), sliders quantized, live slider labels via
  `settings-format-number`, retention guard dialog (`retention`), Library clear-history dialog
  (`library-clear-history`), remove-key dialog (`remove-key`), port field (in-out draft, `PortField`), the re-check
  window (ended by the next `AppInfo`), hotkey recorder (winit key events while `HotkeyRowData.recording`; stops on
  blur / main window unfocused, hidden or minimized / `HotkeysResumed`), hook notice (only after a failed install),
  key notice, build hash, Appearance copy from data (`intro`, `theme-notes`). Scroll spy: `SettingsPage.active-section`
  is computed in Slint (the `settings.slint` header has the rules); Rust only sets `scroll-to` / `scroll-nonce`.
- Voices: in-out `voices-search-text`, filter selects re-asserted from `data` (`revision`), `PreviewUpdate::Failed` →
  toast `voice-preview`, "Use voice" toast on its `SettingsSaved`.
- Onboarding: steps welcome → key → voice → try → done; key notice on the key step; previews pause reading; `connect`
  sends the raw draft (Rust trims; blank → `Paste your API key first.`); finish waits for its `SettingsSaved`.
- Widget: views toast/error/idle/compact/full (ui-settings-widget.md §6.2), compact pill per D-USW-21, sentence
  highlighting from `vox_text::sentence_view`, `card-key` replays the enter animation, `progress-accessible` and
  `warning-dot-tooltip`, always dark, drag handles = text/labels only (never a control inside a handle; contracts.md
  §1.4).
- Chip: 34-px pill, art dot + label; press anywhere = chip press; exit animation 120 ms (scale only); always dark.

### 7.5 Struct reference (generated from `ui/models.slint`)

- **enum `PageId`** = `home | history | analytics | voices | settings | onboarding`
- **enum `PlayerPhase`** = `idle | loading | playing | paused | ended | error` — Player status as the UI sees it.
- **enum `MainAction`** = `play | pause | play-again | try-again` — What the main transport button does / shows.
- **enum `PlayerAction`** = `toggle | play | pause | stop | previous | next | cycle-rate | close` — Transport actions sent from any player surface.
- **enum `ToastTone`** = `info | success | error`
- **enum `BadgeTone`** = `neutral | success | attention`
- **enum `CreditsState`** = `loading | no-key | ready | restricted | error | demo` — ElevenLabs credits state machine (ui-main.md §1.5) plus demo.
- **enum `LoadState`** = `loading | ready | error | empty` — Generic async content state.
- **enum `WidgetView`** = `toast | error | idle | compact | full` — Which card the widget shows (ui-settings-widget.md §6.2).
- **enum `WidgetBody`** = `sentence | title | banner` — What fills the widget body.
- **enum `ChipKind`** = `context | selection`
- **enum `PreviewState`** = `none | ready | loading | playing`
- **enum `HistoryRowKind`** = `header | read`
- **enum `HistoryState`** = `loading | error | nothing-yet | no-matches | list` — History body states (ui-main.md §4.5).
- **enum `VoicesState`** = `loading | no-key | error | empty | no-match | list` — Voices content states (ui-settings-widget.md §4.4).
- **enum `AccountState`** = `no-key | demo | stored`

**global `Brand`** — Product name constants, set once by Rust from vox_core::constants (APP_NAME, READ_WITH_LABEL) on every window.

| Field | Type | Meaning |
|---|---|---|
| `app-name` | `string` |  |
| `read-with-label` | `string` |  |

**global `Motion`** — The OS "reduce motion" preference (vox_win::window::reduce_motion), set by Rust on all three windows at startup and on UiUpdate::Motion. When true, pages and windows use 0 ms (1 ms) for their own animations: the widget's sentence fade, scroll transitions, the card enter animation, toast banners, the chip's enter/exit and Settings smooth scroll.

| Field | Type | Meaning |
|---|---|---|
| `reduce` | `bool` |  |

**struct `ChoiceItem`** — An option of a select / segmented control.

| Field | Type | Meaning |
|---|---|---|
| `value` | `string` |  |
| `label` | `string` |  |
| `accessible-label` | `string` |  |

**struct `KeyGroup`** — A label followed by keycaps ("Selection [Ctrl][Alt][R]").

| Field | Type | Meaning |
|---|---|---|
| `label` | `string` |  |
| `keys` | `[string]` |  |

**struct `Fact`**

| Field | Type | Meaning |
|---|---|---|
| `label` | `string` |  |
| `value` | `string` |  |
| `tooltip` | `string` |  |

**struct `BadgeData`**

| Field | Type | Meaning |
|---|---|---|
| `text` | `string` |  |
| `tone` | `BadgeTone` |  |

**struct `ToastData`**

| Field | Type | Meaning |
|---|---|---|
| `id` | `string` |  |
| `tone` | `ToastTone` |  |
| `title` | `string` |  |
| `message` | `string` |  |
| `action-label` | `string` | Empty = no action button. |
| `leaving` | `bool` | In its 180 ms exit animation (still in the stack; clicks are ignored). |
| `key` | `int` | Unique per push: a same-id replacement gets a new key (the enter animation replays). |

**struct `DialogData`** — The single modal slot of the main window. Ids: "history-clear" (History), "remove-key", "retention", "library-clear-history" (Settings).

| Field | Type | Meaning |
|---|---|---|
| `open` | `bool` |  |
| `id` | `string` | Identifies the dialog in confirm/cancel callbacks. |
| `title` | `string` |  |
| `body` | `string` |  |
| `confirm-label` | `string` |  |
| `cancel-label` | `string` |  |
| `danger` | `bool` |  |
| `busy` | `bool` |  |

**struct `CreditsData`** — Built per surface by ui::convert::credits_data (Home / Analytics / Sidebar / Settings Account / Onboarding copy differs; `demo` only on Settings Account).

| Field | Type | Meaning |
|---|---|---|
| `state` | `CreditsState` |  |
| `title` | `string` | Home no-key / restricted / error title ("No API key", "Usage hidden", "Credits unavailable"); empty elsewhere. |
| `tier` | `string` | Badge, e.g. "Creator" (empty = none). |
| `percent` | `string` | "39%" |
| `percent-accessible` | `string` | "39 percent of credits left" |
| `fraction` | `float` | 0..1 left |
| `low` | `bool` |  |
| `facts` | `string` | "38.8K of 100K left" |
| `reset` | `string` | "Resets in 12 days" (empty = unknown) |
| `detail` | `string` | "38,766 of 100,000 characters left. Resets in 12 days" (tooltip / accessible value) |
| `message` | `string` | Body text for no-key / restricted / error states. |
| `action-label` | `string` | Button for no-key / restricted / error states ("Add key", "Manage key", "Retry"); empty = none. |
| `refreshing` | `bool` | A forced refresh is in flight (Analytics "Refresh balance" spins and is disabled while refreshing \|\| loading). |

**struct `BarData`** — One bar of a chart.

| Field | Type | Meaning |
|---|---|---|
| `level` | `float` | 0..1 of the tallest bar. |
| `label` | `string` | Axis label ("" = unlabelled). |
| `today` | `bool` |  |
| `zero` | `bool` |  |
| `tooltip-title` | `string` |  |
| `tooltip-value` | `string` |  |
| `tooltip-sub` | `string` |  |
| `accessible` | `string` | Spoken value ("Tue, Sep 30: 1h 5m, 3 reads"); also the polite live text while selected by keyboard. |

**struct `NavItemData`**

| Field | Type | Meaning |
|---|---|---|
| `page` | `PageId` |  |
| `label` | `string` |  |
| `icon` | `string` | Lucide icon name (ui/icons). |

**struct `MiniPlayerData`** — The sidebar's compact "now playing" row (DESIGN.md; mini player of ui-main.md §2.7).

| Field | Type | Meaning |
|---|---|---|
| `visible` | `bool` |  |
| `art` | `image` |  |
| `status` | `string` | "Now reading", "Paused", "Preparing", "Couldn't read" |
| `title` | `string` |  |
| `phase` | `PlayerPhase` |  |
| `playing` | `bool` | Shows pause (else play). |
| `can-toggle` | `bool` |  |
| `progress` | `float` |  |
| `buffered` | `float` |  |
| `time` | `string` | "-3:12" / "~-3:12" (empty when unknown) |
| `time-accessible` | `string` | "3:12 left" / "about 3:12 left" (formatClock, ui-main.md §2.7 / §11.1) |
| `error` | `string` |  |

**struct `SidebarData`**

| Field | Type | Meaning |
|---|---|---|
| `nav` | `[NavItemData]` |  |
| `now` | `MiniPlayerData` |  |
| `credits-visible` | `bool` |  |
| `credits-line` | `string` | "39% credits left" |
| `credits-fraction` | `float` |  |
| `credits-low` | `bool` |  |
| `add-key-visible` | `bool` | "Add your ElevenLabs key" warning button instead of credits. |
| `hint-label` | `string` | "Read selection" + keycaps, or "Right-click → Read with Vox" when no hotkey. |
| `hint-keys` | `[string]` |  |
| `hint-spoken` | `string` | The hint read aloud ("Read selection: Ctrl plus Alt plus R"). |

**struct `NowPlayingData`**

| Field | Type | Meaning |
|---|---|---|
| `idle` | `bool` | Idle body (nothing playing / ended without title). |
| `art` | `image` |  |
| `title` | `string` |  |
| `meta` | `string` |  |
| `meta-is-error` | `bool` |  |
| `badges` | `[string]` | "Chrome", "Live", "Paused", "Finished" |
| `phase` | `PlayerPhase` |  |
| `time` | `string` |  |
| `time-estimate` | `bool` |  |
| `time-accessible` | `string` |  |
| `progress` | `float` |  |
| `buffered` | `float` |  |
| `seekable` | `bool` |  |
| `can-stop` | `bool` |  |
| `can-previous` | `bool` |  |
| `can-next` | `bool` |  |
| `main-action` | `MainAction` |  |
| `main-enabled` | `bool` |  |
| `title-tooltip` | `string` | The raw title (tooltip on the truncated one). |
| `idle-title` | `string` | Idle body copy. |
| `idle-sub` | `string` |  |
| `idle-keys` | `[string]` |  |
| `idle-keys-spoken` | `string` | The idle keycaps read aloud ("Ctrl plus Alt plus R"). |
| `replay-last-visible` | `bool` | "Replay last" |
| `replay-last-tooltip` | `string` |  |
| `replay-last-busy` | `bool` |  |

**struct `StatBlockData`**

| Field | Type | Meaning |
|---|---|---|
| `loading` | `bool` |  |
| `label` | `string` |  |
| `badge` | `string` |  |
| `value` | `string` | Big Display number ("4h 20m", "39%") |
| `value-accessible` | `string` | The big number read aloud ("4 hours 20 minutes listened today"). |
| `caption` | `string` |  |
| `sub` | `string` |  |
| `fraction` | `float` | 0..1 line meter |
| `accessible-value` | `string` | The meter's accessible value ("39% of the daily goal" / "Goal reached: 20 minutes listened"). |
| `done` | `bool` |  |

**struct `WeekData`**

| Field | Type | Meaning |
|---|---|---|
| `loading` | `bool` |  |
| `title` | `string` | Card label: "Last 7 days" (BRIEF.md: "This week" is a rolling 7 days, labelled as such). |
| `total` | `string` |  |
| `bars` | `[BarData]` |  |
| `streak` | `string` |  |
| `best` | `string` |  |
| `streak-active` | `bool` |  |
| `accessible-description` | `string` | "Listening per day. Monday: 5 minutes, …" |

**struct `RecentRow`**

| Field | Type | Meaning |
|---|---|---|
| `id` | `string` |  |
| `art` | `image` |  |
| `title` | `string` |  |
| `meta` | `string` | "Outlook · George · 10 h ago" |
| `duration` | `string` |  |
| `failed` | `bool` |  |
| `failed-tooltip` | `string` |  |
| `cached` | `bool` |  |
| `favorite` | `bool` |  |
| `live` | `bool` | In the player and playing: the button pauses. |
| `resumable` | `bool` | In the player and paused: the button resumes (BRIEF.md "A paused read on Recent offers Resume"). |
| `busy` | `bool` |  |
| `accessible-label` | `string` |  |

**struct `WayRow`**

| Field | Type | Meaning |
|---|---|---|
| `title` | `string` |  |
| `body` | `string` |  |
| `key-groups` | `[KeyGroup]` |  |
| `icon` | `string` |  |
| `section` | `string` |  |
| `off` | `bool` |  |

**struct `HomeData`**

| Field | Type | Meaning |
|---|---|---|
| `state` | `LoadState` | First load failed → inline error with Retry (BRIEF.md "Home"); later failures keep the last data (ready). |
| `error-title` | `string` |  |
| `error-body` | `string` |  |
| `retrying` | `bool` | Retry in flight (the error card's button spins). |
| `greeting` | `string` |  |
| `date-line` | `string` |  |
| `now` | `NowPlayingData` |  |
| `today` | `StatBlockData` |  |
| `credits` | `CreditsData` |  |
| `week` | `WeekData` |  |
| `quick-read-words` | `string` |  |
| `quick-read-can-read` | `bool` | The trimmed Quick read text is not empty (Read button and Ctrl+Enter). |
| `read-busy` | `bool` |  |
| `clipboard-busy` | `bool` |  |
| `recent-loading` | `bool` |  |
| `recent` | `[RecentRow]` |  |
| `ways` | `[WayRow]` |  |
| `voice-name` | `string` |  |
| `voice-meta` | `string` |  |
| `voice-art` | `image` |  |
| `voice-accessible` | `string` | "Voice: George. Flash v2.5, speed 1 times. Change voice" |

**struct `HistoryRow`** — A flat list row: a day header or a read.

| Field | Type | Meaning |
|---|---|---|
| `kind` | `HistoryRowKind` |  |
| `id` | `string` |  |
| `header-label` | `string` | Header: "Today" / "4 reads · 12m" |
| `header-meta` | `string` |  |
| `art` | `image` | Read: collapsed |
| `source-icon` | `string` |  |
| `source-label` | `string` |  |
| `title` | `string` |  |
| `meta` | `string` |  |
| `time-tooltip` | `string` |  |
| `favorite` | `bool` |  |
| `badges` | `[string]` | "Playing", "Paused", "Failed", "Free replay" |
| `progress` | `float` |  |
| `percent` | `string` |  |
| `complete` | `bool` |  |
| `can-replay` | `bool` |  |
| `replay-label` | `string` |  |
| `replay-tooltip` | `string` |  |
| `expanded` | `bool` | Read: expanded details |
| `full-text` | `string` |  |
| `notes` | `[string]` |  |
| `facts` | `[Fact]` |  |
| `details` | `[Fact]` |  |
| `url-label` | `string` |  |

**struct `HistoryData`**

| Field | Type | Meaning |
|---|---|---|
| `state` | `HistoryState` |  |
| `dimmed` | `bool` |  |
| `count-label` | `string` |  |
| `actions-visible` | `bool` |  |
| `exporting` | `bool` |  |
| `clearing` | `bool` |  |
| `can-export` | `bool` |  |
| `search` | `string` | Filters as applied. The field's text is MainWindow.history-search-text (in-out; Rust writes it only to reset it). |
| `favorites-only` | `bool` |  |
| `source` | `string` |  |
| `source-options` | `[ChoiceItem]` |  |
| `state-icon` | `string` | State copy (error / nothing yet / no matches) |
| `state-title` | `string` |  |
| `state-body` | `string` |  |
| `state-keys` | `[string]` |  |
| `state-body-after` | `string` | Text after the keycaps (". Every read lands here."); body, keycaps and body-after render inline. |
| `state-button` | `string` |  |
| `rows` | `[HistoryRow]` |  |
| `footer` | `string` |  |
| `can-load-more` | `bool` |  |
| `loading-more` | `bool` |  |
| `end-of-history` | `bool` |  |
| `revision` | `int` | Bumped when Rust overrides what the user did to a stateful control (Clear filters…): re-assert from data. |

**struct `KpiData`**

| Field | Type | Meaning |
|---|---|---|
| `icon` | `string` |  |
| `label` | `string` |  |
| `value` | `string` |  |
| `value-accessible` | `string` | The value read aloud. |
| `spark-label` | `string` | The sparkline's accessible label. |
| `unit` | `string` |  |
| `sub` | `string` |  |
| `spark` | `[float]` |  |
| `dots` | `[bool]` |  |

**struct `HeatCellData`**

| Field | Type | Meaning |
|---|---|---|
| `day` | `int` |  |
| `hour` | `int` |  |
| `level` | `float` | 0..1 (sqrt scale) |
| `peak` | `bool` |  |
| `now` | `bool` |  |
| `tooltip-title` | `string` |  |
| `tooltip-value` | `string` |  |
| `accessible` | `string` | "Monday 21:00–22:00: 12m"; also the polite live text while selected by keyboard. |

**struct `ShareRow`**

| Field | Type | Meaning |
|---|---|---|
| `key` | `string` |  |
| `rank` | `string` |  |
| `icon` | `string` |  |
| `art` | `image` |  |
| `label` | `string` |  |
| `sub` | `string` |  |
| `value` | `string` |  |
| `percent` | `string` |  |
| `fraction` | `float` |  |
| `first` | `bool` |  |
| `accessible` | `string` | The row read aloud. |

**struct `AnalyticsData`**

| Field | Type | Meaning |
|---|---|---|
| `state` | `LoadState` |  |
| `dimmed` | `bool` |  |
| `retrying` | `bool` | Retry after a first-load failure in flight (the error card's button spins). |
| `range` | `string` |  |
| `ranges` | `[ChoiceItem]` |  |
| `range-label` | `string` |  |
| `refresh-failed` | `bool` |  |
| `error` | `string` |  |
| `empty` | `bool` |  |
| `empty-title` | `string` |  |
| `empty-body` | `string` |  |
| `listened` | `string` | Hero |
| `listened-accessible` | `string` |  |
| `today` | `string` |  |
| `today-accessible` | `string` |  |
| `days-active` | `string` |  |
| `days-active-accessible` | `string` | "24 of 30 days" (D-UM-30) |
| `days-active-fraction` | `float` |  |
| `audio-generated` | `string` |  |
| `kpis` | `[KpiData]` |  |
| `daily` | `[BarData]` | Daily listening |
| `daily-avg` | `string` |  |
| `daily-avg-accessible` | `string` |  |
| `daily-peak` | `string` |  |
| `daily-peak-accessible` | `string` |  |
| `daily-empty` | `string` |  |
| `daily-summary` | `string` |  |
| `heat` | `[HeatCellData]` | When you listen |
| `heat-peak` | `string` |  |
| `heat-day-labels` | `[string]` |  |
| `heat-hour-labels` | `[string]` |  |
| `heat-summary` | `string` |  |
| `sources` | `[ShareRow]` | How you start reads / top lists |
| `sources-empty` | `string` |  |
| `sources-summary` | `string` |  |
| `top-apps` | `[ShareRow]` |  |
| `top-apps-summary` | `string` |  |
| `top-voices` | `[ShareRow]` |  |
| `top-voices-summary` | `string` |  |
| `credits` | `CreditsData` |  |
| `billed` | `string` |  |

**struct `VoiceRow`**

| Field | Type | Meaning |
|---|---|---|
| `id` | `string` |  |
| `name` | `string` |  |
| `art` | `image` |  |
| `subtitle` | `string` | "Male · American · Narration" |
| `description` | `string` |  |
| `tags` | `[string]` |  |
| `caption` | `string` |  |
| `in-use` | `bool` |  |
| `preview` | `PreviewState` |  |

**struct `VoicesData`** — The search field's text is MainWindow.voices-search-text (in-out).

| Field | Type | Meaning |
|---|---|---|
| `state` | `VoicesState` |  |
| `in-use` | `string` |  |
| `count` | `string` |  |
| `gender` | `string` |  |
| `accent` | `string` |  |
| `category` | `string` |  |
| `gender-options` | `[ChoiceItem]` |  |
| `accent-options` | `[ChoiceItem]` |  |
| `category-options` | `[ChoiceItem]` |  |
| `refreshing` | `bool` |  |
| `state-title` | `string` |  |
| `state-body` | `string` |  |
| `state-button` | `string` |  |
| `voices` | `[VoiceRow]` |  |
| `revision` | `int` | Bumped when Rust overrides the filter selects (Clear filters): re-assert from data. |

**struct `AccountData`**

| Field | Type | Meaning |
|---|---|---|
| `state` | `AccountState` |  |
| `badge` | `BadgeData` |  |
| `form-open` | `bool` |  |
| `replacing` | `bool` |  |
| `busy` | `bool` |  |
| `field-label` | `string` |  |
| `hint` | `string` |  |
| `error` | `string` |  |
| `submit-label` | `string` |  |
| `lead` | `string` |  |
| `limited-warning` | `string` |  |
| `remove-busy` | `bool` |  |
| `credits` | `CreditsData` |  |
| `notice` | `string` | Stored key could not be migrated/read (AppInfo.api_key_notice); empty = none. |

**struct `VoiceModelData`**

| Field | Type | Meaning |
|---|---|---|
| `voice-name` | `string` |  |
| `voice-art` | `image` |  |
| `voice-tags` | `[string]` |  |
| `tags-loading` | `bool` |  |
| `model-options` | `[ChoiceItem]` |  |
| `model-index` | `int` |  |
| `model-description` | `string` |  |
| `model-badges` | `[BadgeData]` |  |
| `model-error` | `string` |  |
| `models-loading` | `bool` |  |
| `language-options` | `[ChoiceItem]` |  |
| `language-index` | `int` |  |
| `quality-options` | `[ChoiceItem]` |  |
| `quality-index` | `int` |  |
| `stability` | `float` |  |
| `stability-text` | `string` |  |
| `similarity` | `float` |  |
| `similarity-text` | `string` |  |
| `style` | `float` |  |
| `style-text` | `string` |  |
| `speed` | `float` |  |
| `speed-text` | `string` |  |
| `speaker-boost` | `bool` |  |
| `reset-enabled` | `bool` |  |
| `test-busy` | `bool` |  |

**struct `PlaybackData`**

| Field | Type | Meaning |
|---|---|---|
| `rate-options` | `[ChoiceItem]` |  |
| `rate-index` | `int` |  |
| `volume` | `float` |  |
| `volume-text` | `string` |  |
| `skip-urls` | `bool` |  |
| `skip-emails` | `bool` |  |
| `max-chars-options` | `[ChoiceItem]` |  |
| `max-chars-index` | `int` |  |

**struct `HotkeyRowData`**

| Field | Type | Meaning |
|---|---|---|
| `action` | `string` | "readSelection" \| "readClipboard" \| "playPause" \| "stop" |
| `label` | `string` |  |
| `description` | `string` |  |
| `keys` | `[string]` |  |
| `spoken` | `string` |  |
| `recording` | `bool` |  |
| `preview` | `[string]` | Held modifiers while recording. |
| `hint` | `string` | Flash hint while recording ("Add Ctrl, Alt or Win"). |
| `warning` | `string` | Persistent / transient warning under the row. |

**struct `TriggersData`**

| Field | Type | Meaning |
|---|---|---|
| `hook-available` | `bool` |  |
| `hook-notice-title` | `string` | Notice while the hook is unavailable (vox_core::copy::HOOK_UNAVAILABLE_*); empty when available. |
| `hook-notice-body` | `string` |  |
| `right-click-chip` | `bool` |  |
| `selection-popup` | `bool` |  |
| `hotkeys-enabled` | `bool` |  |
| `hotkeys` | `[HotkeyRowData]` |  |
| `media-keys` | `bool` |  |
| `restore-clipboard` | `bool` |  |
| `excluded-apps` | `[string]` |  |
| `exclude-error` | `string` |  |

**struct `WidgetSettingsData`**

| Field | Type | Meaning |
|---|---|---|
| `enabled` | `bool` |  |
| `position-options` | `[ChoiceItem]` |  |
| `position-index` | `int` |  |
| `position-hint` | `string` |  |
| `compact` | `bool` |  |
| `highlight-words` | `bool` |  |
| `auto-hide` | `bool` |  |

**struct `AppearanceData`**

| Field | Type | Meaning |
|---|---|---|
| `theme-options` | `[ChoiceItem]` |  |
| `theme-index` | `int` |  |
| `intro` | `string` | vox_core::copy::APPEARANCE_INTRO (the floating player and the chip always stay dark). |
| `theme-notes` | `[string]` | One note per theme option, same order (vox_core::copy::THEME_NOTES). |

**struct `LibraryData`**

| Field | Type | Meaning |
|---|---|---|
| `cache-badge` | `string` |  |
| `save-history` | `bool` |  |
| `store-text` | `bool` |  |
| `retention-options` | `[ChoiceItem]` |  |
| `retention-index` | `int` |  |
| `cache-audio` | `bool` |  |
| `cache-limit` | `float` | MB, slider 50..cache-limit-max step 50 |
| `cache-limit-max` | `float` |  |
| `cache-limit-text` | `string` |  |
| `usage-text` | `string` |  |
| `usage-fraction` | `float` |  |
| `usage-high` | `bool` |  |
| `clear-cache-enabled` | `bool` |  |
| `clear-cache-busy` | `bool` |  |
| `export-busy` | `bool` |  |
| `clear-history-busy` | `bool` |  |

**struct `IntegrationsData`**

| Field | Type | Meaning |
|---|---|---|
| `bridge-enabled` | `bool` |  |
| `bridge-badge` | `BadgeData` |  |
| `port-error` | `string` |  |
| `port-note` | `string` |  |
| `explorer-enabled` | `bool` |  |
| `explorer-installed` | `bool` |  |
| `launch-at-login` | `bool` |  |
| `start-minimized` | `bool` |  |
| `close-to-tray` | `bool` |  |
| `copied` | `string` | The copy chip text that was just copied (shows "Copied" for 1.4 s). |

**struct `AboutData`**

| Field | Type | Meaning |
|---|---|---|
| `version` | `string` |  |
| `runtime` | `string` |  |
| `build` | `string` | Short build hash ("dev" for local builds). |
| `data-folder` | `string` |  |
| `data-folder-ready` | `bool` |  |
| `demo` | `bool` |  |

**struct `SettingsData`**

| Field | Type | Meaning |
|---|---|---|
| `loading` | `bool` |  |
| `saving` | `bool` |  |
| `status` | `string` | "Saving" / "Saved automatically" |
| `sections` | `[ChoiceItem]` |  |
| `revision` | `int` | Bumped on every rebuild that must override what the user did (revert, Reset, retention cancel, a change made elsewhere): sections re-assert toggles / selects / sliders from data in `changed data` handlers. |
| `account` | `AccountData` |  |
| `voice` | `VoiceModelData` |  |
| `playback` | `PlaybackData` |  |
| `triggers` | `TriggersData` |  |
| `widget` | `WidgetSettingsData` |  |
| `appearance` | `AppearanceData` |  |
| `library` | `LibraryData` |  |
| `integrations` | `IntegrationsData` |  |
| `about` | `AboutData` |  |

**struct `KeyStepData`**

| Field | Type | Meaning |
|---|---|---|
| `has-key` | `bool` |  |
| `form-visible` | `bool` |  |
| `replacing` | `bool` |  |
| `busy` | `bool` |  |
| `error` | `string` |  |
| `hint` | `string` |  |
| `connected-caption` | `string` |  |
| `warning` | `string` |  |
| `notice` | `string` | Stored key could not be migrated/read (AppInfo.api_key_notice); empty = none. |

**struct `VoiceStepData`**

| Field | Type | Meaning |
|---|---|---|
| `state` | `LoadState` |  |
| `title` | `string` |  |
| `body` | `string` |  |
| `items` | `[VoiceRow]` |  |

**struct `TryStepData`**

| Field | Type | Meaning |
|---|---|---|
| `can-read` | `bool` |  |
| `caption` | `string` |  |
| `busy` | `bool` |  |
| `sample` | `string` |  |
| `keys` | `[string]` |  |

**struct `OnboardingData`**

| Field | Type | Meaning |
|---|---|---|
| `step` | `int` | 0 welcome, 1 key, 2 voice, 3 try, 4 done |
| `furthest` | `int` |  |
| `step-labels` | `[string]` |  |
| `crumb` | `string` |  |
| `next-label` | `string` |  |
| `next-secondary` | `bool` |  |
| `show-back` | `bool` |  |
| `show-skip` | `bool` |  |
| `finishing` | `bool` |  |
| `art` | `image` |  |
| `key` | `KeyStepData` |  |
| `voices` | `VoiceStepData` |  |
| `try-it` | `TryStepData` |  |
| `tips` | `[KeyGroup]` |  |

**struct `WidgetData`**

| Field | Type | Meaning |
|---|---|---|
| `view` | `WidgetView` |  |
| `body` | `WidgetBody` |  |
| `art` | `image` |  |
| `source` | `string` | "Chrome" |
| `voice` | `string` |  |
| `phase` | `PlayerPhase` |  |
| `status` | `string` | "Reading", "Synthesizing", … |
| `status-loading` | `bool` |  |
| `rate` | `string` |  |
| `show-rate` | `bool` |  |
| `title` | `string` |  |
| `spoken` | `string` |  |
| `current` | `string` |  |
| `upcoming` | `string` |  |
| `sentence-key` | `int` | Bumped when the sentence changes (fade-in). |
| `card-key` | `int` | Bumped on a new session, a full↔compact switch, and each appearance of the error / standalone toast card (replays the card enter animation; status changes do not bump it). |
| `time` | `string` |  |
| `time-estimate` | `bool` |  |
| `time-accessible` | `string` |  |
| `progress` | `float` |  |
| `buffered` | `float` |  |
| `progress-accessible` | `string` | The progress bar's accessible value: "1:03 of about 4:23" (vox_core::format::progress_accessible). |
| `seekable` | `bool` |  |
| `previous-enabled` | `bool` |  |
| `next-enabled` | `bool` |  |
| `previous-label` | `string` |  |
| `next-label` | `string` |  |
| `main-action` | `MainAction` |  |
| `compact-loading` | `bool` |  |
| `warning-visible` | `bool` |  |
| `warning-lead` | `string` | "Stops early" / "Stopped early" |
| `warning-reason` | `string` |  |
| `warning-link` | `bool` |  |
| `warning-tooltip` | `string` |  |
| `warning-dot-tooltip` | `string` | The compact pill's red dot: "Stops early: {message}". |
| `error-title` | `string` |  |
| `error-detail` | `string` |  |
| `error-action` | `string` | "Open settings" / "Try again" / "" |
| `toast` | `ToastData` |  |
| `toast-visible` | `bool` |  |

**struct `ChipData`**

| Field | Type | Meaning |
|---|---|---|
| `kind` | `ChipKind` |  |
| `label` | `string` |  |
| `art` | `image` |  |
| `hiding` | `bool` | Exit animation running (presses ignored). |
| `show-key` | `int` | Bumped on every show (replays the enter animation). |

### 7.6 Component interfaces (generated from `ui/windows/*.slint` and `ui/pages/**/*.slint`)

**`AnalyticsPage`** (`ui/pages/analytics.slint`, inherits `Rectangle`) — Analytics (ui-main.md §5; DESIGN.md "Screens → Analytics"). Placeholder visuals; keep the interface. The range segmented control and the eyebrow are rendered by MainWindow's header row (`analytics.ranges`, `analytics.range`, `analytics.range-label` → `analytics-set-range(value)`, ARCHITECTURE.md §7.4). Charts: Rust buckets the daily series for the plot width reported through `daily-width-changed` (nothing is drawn while `data.daily` is empty); the heatmap receives all 24 hour labels and shows every 3rd or 6th one from its own pitch (`3 × pitch ≥ 34px` → every 3rd). The keyboard-selected bar / cell exposes its `accessible` text in a hidden Text with `accessible-live-region: AccessibleLiveness.polite` (as ui/design/toast.slint does; D-UM-30).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `AnalyticsData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `daily-width-changed` | `(float)` | The daily plot's width in logical px, on creation and whenever it changes. |
| `retry` | `()` |  |
| `refresh-credits` | `()` | Credits block: "Refresh balance" / "Retry" (forced refresh). |
| `add-key` | `()` | Credits block: "Add API key" → Settings → Account. |

**`HistoryPage`** (`ui/pages/history.slint`, inherits `Rectangle`) — History (ui-main.md §4; DESIGN.md "Screens → History"). Placeholder visuals; keep the interface. The header actions (count eyebrow, Export, Clear all) are rendered by MainWindow's header row from `data` (ARCHITECTURE.md §7.4). The search field's text is `search-text` (in-out; Rust writes it only to reset it); `focus-search-nonce` changes → focus the search field and select its text (Ctrl+F / `/`). The nothing-yet body is `state-body` + `state-keys` (keycaps) + `state-body-after`, inline. Stateful controls (favorites segmented, source select) re-assert from `data` in a `changed data` handler (`data.revision`).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `HistoryData` |  |
| `search-text` | in-out | `string` |  |
| `focus-search-nonce` | in | `int` |  |

| Callback | Args | Meaning |
|---|---|---|
| `search-edited` | `(string)` | Raw search text on every edit (Rust debounces 220 ms; empty applies at once). Escape clears. |
| `set-favorites-only` | `(bool)` | Segmented "All \| Favorites". |
| `set-source` | `(string)` | Source filter value ("" = all sources). |
| `clear-filters` | `()` |  |
| `toggle-row` | `(string)` | Row toggle (expand/collapse) and row actions, by read id. |
| `replay` | `(string)` |  |
| `copy-text` | `(string)` |  |
| `toggle-favorite` | `(string)` |  |
| `delete` | `(string)` |  |
| `open-url` | `(string)` |  |
| `load-more` | `()` |  |
| `retry` | `()` | Error state "Try again". |
| `paste-and-read` | `()` | Nothing-yet "Paste & read" → Home. |

**`HomePage`** (`ui/pages/home.slint`, inherits `Rectangle`) — Home (ui-main.md §3; DESIGN.md "Screens → Home"). Placeholder visuals: the page owner replaces the body but keeps this interface (docs/ARCHITECTURE.md §7.4).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `HomeData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `quick-read` | `(string)` | Quick read: "Read" / Ctrl+Enter with the untrimmed text, enabled while `data.quick-read-can-read`; live word count on every edit (`quick-read-edited`). |
| `quick-read-edited` | `(string)` |  |
| `read-clipboard` | `()` |  |
| `player-action` | `(PlayerAction)` | Now playing transport. The main button always sends `toggle` (Rust maps it: playing/paused → toggle; ended or error with a read → replay); stop / previous / next as labelled. |
| `seek` | `(float)` | Seek committed at a 0..1 fraction (only while playing or paused). Keyboard (D-UM-31): wrap the design ProgressLine in a FocusScope whose `capture-key-pressed` handles Right/Up +0.02, Left/Down −0.02, PageUp +0.1, PageDown −0.1, Home 0, End 1, each sent at once with `seek(clamp(value ± step, 0, 1))`; `value-text` = `data.now.time-accessible`. |
| `replay` | `(string)` | Replay a Recent row's read. |
| `replay-last` | `()` | "Replay last" (its own spinner: `data.now.replay-last-busy`). |
| `pause-live` | `()` | Recent row of the live read: pause. |
| `open-page` | `(PageId)` | "All history" / voice block → Voices. |
| `open-settings` | `(string)` | Ways-to-read rows and credits buttons → a settings section. |
| `credits-retry` | `()` | Credits "Retry". |
| `retry` | `()` | Inline load error "Retry" (data.state == error). |
| `resume-live` | `()` | Recent row of the paused read: resume. |

**`OnboardingPage`** (`ui/pages/onboarding.slint`, inherits `Rectangle`) — Onboarding (ui-settings-widget.md §5; DESIGN.md "Screens → Onboarding"): full backdrop, a centred 560-px dialog-shaped panel with step dots, art per step and a primary "Continue" pill. Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `OnboardingData` |  |
| `backdrop` | in | `image` |  |
| `key-draft` | in-out | `string` | The API-key draft survives step changes. |

| Callback | Args | Meaning |
|---|---|---|
| `go-to` | `(int)` | Frame navigation. |
| `next` | `()` |  |
| `back` | `()` |  |
| `skip` | `()` |  |
| `connect-key` | `(string)` | Key step |
| `key-edited` | `()` |  |
| `get-key` | `()` |  |
| `replace-key` | `()` |  |
| `cancel-replace` | `()` |  |
| `choose-voice` | `(string)` | Voice step |
| `preview-voice` | `(string)` |  |
| `retry-voices` | `()` |  |
| `read-sample` | `()` | Try step |

**`WelcomeStep`** (`ui/pages/onboarding/steps.slint`, inherits `Rectangle`) — Onboarding steps (ui-main.md §10, ui-settings-widget.md §5.3). Placeholder visuals; keep each interface. Step 1: logo, `VOX`, tagline, body.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `art` | in | `image` |  |

**`ApiKeyStep`** (`ui/pages/onboarding/steps.slint`, inherits `Rectangle`) — Step 2: connect the ElevenLabs key. The draft text lives in the frame (OnboardingPage) so Back/Continue keep it.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `KeyStepData` |  |
| `draft` | in-out | `string` |  |

| Callback | Args | Meaning |
|---|---|---|
| `connect` | `(string)` | "Connect" with the raw draft: Rust trims it and answers `Paste your API key first.` when it is blank (Slint has no trim). |
| `edited` | `()` | Typing clears the error. |
| `get-key` | `()` |  |
| `replace-key` | `()` |  |
| `cancel-replace` | `()` |  |

**`VoiceStep`** (`ui/pages/onboarding/steps.slint`, inherits `Rectangle`) — Step 3: pick a voice (API order) with previews.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `VoiceStepData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `choose` | `(string)` |  |
| `preview` | `(string)` |  |
| `retry` | `()` |  |

**`TryStep`** (`ui/pages/onboarding/steps.slint`, inherits `Rectangle`) — Step 4: how to read from anywhere + "Read this".

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `TryStepData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `read-sample` | `()` |  |
| `add-key` | `()` | "Add a key" → the key step. |

**`DoneStep`** (`ui/pages/onboarding/steps.slint`, inherits `Rectangle`) — Step 5: done + tips.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `tips` | in | `[KeyGroup]` |  |

**`SettingsPage`** (`ui/pages/settings.slint`, inherits `Rectangle`) — Settings (ui-settings-widget.md §2; DESIGN.md "Screens → Settings"): a second-level section list on the left and a scrolling column of sections on the right. Placeholder visuals; keep the interface. Section components live in ui/pages/settings/*.slint; this page forwards their callbacks.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `SettingsData` |  |
| `scroll-to` | in | `string` | Deep link: scroll to this section id (instantly on the first scroll after mount, smoothly after). |
| `scroll-nonce` | in | `int` | Bumped by Rust to re-announce the same section (a toast action clicked twice). |
| `active-section` | in-out | `string` | The scroll-spy section (computed here; see the header). (default `"account"`) |
| `key-draft` | in-out | `string` | Text-field drafts (in-out; MainWindow forwards them to Rust). |
| `exclude-draft` | in-out | `string` |  |
| `port-draft` | in-out | `string` |  |

| Callback | Args | Meaning |
|---|---|---|
| `format-number` | `pure (string, float) -> string` | A slider's live label: (dotted key, value) → text ("80%", "1.15×", "500 MB"). |
| `section-selected` | `(string)` | Section nav click (the page scrolls itself; Rust only records it). |
| `set-bool` | `(string, bool)` | Generic setting writes (dotted settings key path). |
| `set-number` | `(string, float)` |  |
| `set-choice` | `(string, string)` |  |
| `save-key` | `(string)` | Account |
| `key-edited` | `()` |  |
| `open-key-form` | `()` |  |
| `cancel-key-form` | `()` |  |
| `remove-key` | `()` |  |
| `get-key` | `()` |  |
| `credits-retry` | `()` |  |
| `test-voice` | `()` | Voice & model |
| `browse-voices` | `()` |  |
| `reset-voice-settings` | `()` |  |
| `hotkey-start` | `(string)` | Triggers |
| `hotkey-stop` | `()` |  |
| `hotkey-clear` | `(string)` |  |
| `add-excluded` | `(string)` |  |
| `remove-excluded` | `(string)` |  |
| `excluded-edited` | `()` |  |
| `clear-cache` | `()` | Library & privacy |
| `export-history` | `()` |  |
| `clear-history` | `()` |  |
| `port-edited` | `(string)` | Integrations |
| `commit-port` | `(string)` |  |
| `revert-port` | `()` |  |
| `copy-text` | `(string)` |  |
| `show-extension-folder` | `()` |  |
| `open-data-folder` | `()` | About |
| `open-link` | `(string)` |  |

**`AboutSection`** (`ui/pages/settings/about.slint`, inherits `Rectangle`) — Settings → About (ui-settings-widget.md §2.10). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `AboutData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `open-data-folder` | `()` |  |
| `open-link` | `(string)` | "dashboard" \| "billing" \| "docs" |

**`AccountSection`** (`ui/pages/settings/account.slint`, inherits `Rectangle`) — Settings → Account (ui-settings-widget.md §2.2). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `AccountData` |  |
| `key-draft` | in-out | `string` | The key field's text (in-out; Rust clears it after a successful save / when the form closes). |

| Callback | Args | Meaning |
|---|---|---|
| `save-key` | `(string)` | Submit the key field (Enter or the button); Rust trims, validates and answers through `data`. |
| `key-edited` | `()` | Typing clears the error. |
| `open-form` | `()` | "Replace" / "Add key" open the form; "Cancel" closes it. |
| `cancel-form` | `()` |  |
| `remove-key` | `()` | "Remove" → confirmation dialog (main window dialog slot). |
| `get-key` | `()` | "Get a key" / "Manage keys" open elevenlabs.io. |
| `credits-retry` | `()` | Credits card "Try again". |

**`AppearanceSection`** (`ui/pages/settings/appearance.slint`, inherits `Rectangle`) — Settings → Appearance (ui-settings-widget.md §2.7). Placeholder visuals; keep the interface. Copy from data: `intro` and one `theme-notes` entry per tile (D-USW-20: the floating player and the chip always stay dark).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `AppearanceData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `set-choice` | `(string, string)` | key = "app.theme"; value = "system" \| "paper" \| "night". |

**`IntegrationsSection`** (`ui/pages/settings/integrations.slint`, inherits `Rectangle`) — Settings → Integrations (ui-settings-widget.md §2.9; integrations.md). Placeholder visuals; keep the interface. Static copy (D-INT-3; the product name from Brand.app-name): "Launch at login" — "Start " + Brand.app-name + " when you sign in to Windows. Turn on Start minimized to keep it in the tray." "Start minimized" — "Open straight to the tray instead of showing this window." (governs every launch)

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `IntegrationsData` |  |
| `port-draft` | in-out | `string` | The port field's text (in-out): Rust writes the stripped draft after every edit, resets it on Escape and when the stored port changes. |

| Callback | Args | Meaning |
|---|---|---|
| `set-bool` | `(string, bool)` | key = "triggers.browserExtension" \| "triggers.explorerMenu" \| "app.launchAtLogin" \| "app.startMinimized" \| "app.closeToTray". |
| `port-edited` | `(string)` | Port field: every edit (Rust strips non-digits, ≤ 5), commit on blur/Enter, revert on Escape. |
| `commit-port` | `(string)` |  |
| `revert-port` | `()` |  |
| `copy-text` | `(string)` | Copy chips ("chrome://extensions", "edge://extensions"). |
| `show-extension-folder` | `()` |  |

**`LibrarySection`** (`ui/pages/settings/library.slint`, inherits `Rectangle`) — Settings → Library & privacy (ui-settings-widget.md §2.8). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `LibraryData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `set-bool` | `(string, bool)` | key = "library.saveHistory" \| "library.storeText" \| "library.cacheAudio". |
| `set-choice` | `(string, string)` | key = "library.retentionDays" (Rust runs the retention guard: 450 ms settle + confirmation dialog). |
| `set-number` | `(string, float)` | key = "library.cacheLimitMB" (slider commit, MB; Rust quantizes to 50). Label: `format-number(key, value)`. |
| `format-number` | `pure (string, float) -> string` |  |
| `clear-cache` | `()` |  |
| `export-history` | `()` |  |
| `clear-history` | `()` | Opens the `library-clear-history` dialog ("Clear history?"). |

**`PlaybackSection`** (`ui/pages/settings/playback.slint`, inherits `Rectangle`) — Settings → Playback (ui-settings-widget.md §2.4). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `PlaybackData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `set-choice` | `(string, string)` | key = "playbackRate" (also applied live) \| "text.maxChars"; value = option value. |
| `set-number` | `(string, float)` | key = "volume" (commit; also applied live; Rust quantizes to 0.01). Label: `format-number("volume", value)`. |
| `format-number` | `pure (string, float) -> string` |  |
| `set-bool` | `(string, bool)` | key = "text.skipUrls" \| "text.skipEmails". |

**`TriggersSection`** (`ui/pages/settings/triggers.slint`, inherits `Rectangle`) — Settings → Triggers (ui-settings-widget.md §2.5, §3). Placeholder visuals; keep the interface. The hotkey recorder captures keys in Rust (winit key events) while `HotkeyRowData.recording` is true.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `TriggersData` |  |
| `exclude-draft` | in-out | `string` | The excluded-app input's text (in-out; Rust clears it after a successful add). |

| Callback | Args | Meaning |
|---|---|---|
| `set-bool` | `(string, bool)` | key = "triggers.rightClickChip" \| "triggers.selectionPopup" \| "triggers.hotkeysEnabled" \| "triggers.mediaKeys" \| "triggers.restoreClipboard". |
| `hotkey-start` | `(string)` | Recorder: focus/click/Enter starts recording the action ("readSelection"…); blur/Tab/Escape stops. |
| `hotkey-stop` | `()` |  |
| `hotkey-clear` | `(string)` | Idle Backspace/Delete clears the action's shortcut. |
| `add-excluded` | `(string)` | Excluded apps: add (input text) / remove (exe name); typing clears the error. |
| `remove-excluded` | `(string)` |  |
| `excluded-edited` | `()` |  |

**`VoiceModelSection`** (`ui/pages/settings/voice-model.slint`, inherits `Rectangle`) — Settings → Voice & model (ui-settings-widget.md §2.3). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `VoiceModelData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `set-choice` | `(string, string)` | Selects: key = "modelId" \| "languageCode" \| "outputFormat", value = option value. |
| `set-number` | `(string, float)` | Sliders commit on release: key = "voice.stability" \| "voice.similarityBoost" \| "voice.style" \| "voice.speed". Rust quantizes (step 0.01). The label beside a slider is `format-number(key, slider.value)` (live while dragging). |
| `format-number` | `pure (string, float) -> string` |  |
| `set-bool` | `(string, bool)` | Toggle: key = "voice.useSpeakerBoost". |
| `test-voice` | `()` |  |
| `browse-voices` | `()` |  |
| `reset-voice-settings` | `()` |  |

**`WidgetSection`** (`ui/pages/settings/widget.slint`, inherits `Rectangle`) — Settings → Widget (ui-settings-widget.md §2.6). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `WidgetSettingsData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `set-bool` | `(string, bool)` | key = "widget.enabled" \| "widget.compact" \| "widget.highlightWords" \| "widget.autoHide". |
| `set-choice` | `(string, string)` | key = "widget.position"; value = "bottom-right" \| "top-right" \| "bottom-center" \| "near-cursor" \| "remember". |

**`ConfirmDialog`** (`ui/pages/shell/dialog.slint`, inherits `Rectangle`) — The confirmation dialog (ui-settings-widget.md §1.6; DESIGN.md `Dialog`): scrim, 64-px header with title and close glyph, body, right-aligned Cancel (initial focus) + confirm pill (danger tone). Esc, a press+release on the scrim and the close glyph call `cancel` (Rust ignores it while `busy`). Ids: history-clear, remove-key, retention, library-clear-history. Placeholder visuals.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `DialogData` |  |

| Callback | Args | Meaning |
|---|---|---|
| `confirm` | `(string)` |  |
| `cancel` | `(string)` |  |

**`Sidebar`** (`ui/pages/shell/sidebar.slint`, inherits `Rectangle`) — The main panel's sidebar (DESIGN.md "Screens"; ui-main.md §2.6–§2.7): search field, primary nav, Settings, the compact now-playing row and the credits line. Collapses to icons below 1100 px (`rail`; labels move into tooltips, the footer is not rendered, the rail mini-player variant). Placeholder visuals.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `SidebarData` |  |
| `page` | in | `PageId` |  |
| `rail` | in | `bool` |  |
| `search-text` | in-out | `string` | The search field's text (in-out; cleared by Rust after a submit). |
| `focus-search-nonce` | in | `int` | Changes → focus the search field (Ctrl+K). |

| Callback | Args | Meaning |
|---|---|---|
| `navigate` | `(PageId)` |  |
| `search` | `(string)` | "Search history" submitted (Enter) → History with that search, applied at once. |
| `mini-action` | `(PlayerAction)` | Now-playing row: toggle / stop. |
| `mini-open` | `()` | Now-playing row title → Home. |
| `credits-open` | `()` | Credits line / "Add your ElevenLabs key" → Settings → Account. |

**`ToastStack`** (`ui/pages/shell/toasts.slint`, inherits `Rectangle`) — The app window's toast stack (ui-main.md §1.7, ui-settings-widget.md §1.5): bottom-right (bottom-centre during onboarding), max 4, newest last. Timing lives in Rust; the stack reports hover so Rust can pause the countdown. Rows whose `leaving` is true play the 180 ms exit (fade + slide; instant under Motion.reduce) and ignore clicks; a new `key` replays the enter animation.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `toasts` | in | `[ToastData]` |  |
| `centered` | in | `bool` |  |

| Callback | Args | Meaning |
|---|---|---|
| `action` | `(string)` |  |
| `dismiss` | `(string)` |  |
| `hover` | `(string, bool)` | Pointer or keyboard focus inside a toast (pauses its timer). |

**`VoicesPage`** (`ui/pages/voices.slint`, inherits `Rectangle`) — Voices (ui-settings-widget.md §4; DESIGN.md "Screens → Voices"). Placeholder visuals; keep the interface. The search field's text is `search-text` (in-out; Rust clears it on "Clear filters" and when the page is left). The filter selects re-assert from `data` in a `changed data` handler (`data.revision`).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `VoicesData` |  |
| `search-text` | in-out | `string` |  |

| Callback | Args | Meaning |
|---|---|---|
| `search-edited` | `(string)` | Filters (no debounce). Values are option values ("" = all). |
| `set-gender` | `(string)` |  |
| `set-accent` | `(string)` |  |
| `set-category` | `(string)` |  |
| `clear-filters` | `()` |  |
| `refresh` | `()` | "Refresh voices" / error "Try again" (refresh = true). |
| `preview` | `(string)` | Sample play/stop for a voice id (one at a time; pauses a reading). |
| `use-voice` | `(string)` | "Use voice". |
| `add-key` | `()` | No-key empty state "Add API key". |

**`ChipWindow`** (`ui/windows/chip-window.slint`, inherits `Window`) — The "Read with Vox" chip and the selection popup (DESIGN.md "Read with Vox chip"; ui-settings-widget.md §7; shell.md §6.5). A 34-px pill window; a press anywhere is a chip press: left → activate, other → dismiss, on press (not click); presses are ignored while `data.hiding`. Enter/exit animations only scale (exit 120 ms; 1 ms under Motion.reduce). The Window does **not** bind its own width/height: the pill lays out at the top-left and reports its size (`pill-width`/`pill-height` + `size-changed`); Rust applies position and size in one SetWindowPos. Always dark (Theme.dark = true, D-USW-20). Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `ChipData` |  |
| `opaque-backing` | in | `bool` | An opaque panel pill under the animated pill; Rust sets it (no per-pixel alpha with Skia on DirectX 12: the enter/exit would blend over black). Previews leave it off. |
| `pill-width` | out | `length` | (default `153px`) |
| `pill-height` | out | `length` | (default `34px`) |

| Callback | Args | Meaning |
|---|---|---|
| `pressed` | `(bool)` | Pointer-down anywhere in the window: `true` = left button. |
| `size-changed` | `(length, length)` | The pill's size changed (per label/mode). |

**`MainWindow`** (`ui/windows/main-window.slint`, inherits `Window`) — The main window (DESIGN.md "Screens"; ui-main.md §2; shell.md §6.2): backdrop art with the panel inset 14 px; header row (64 px) with the page title and the page's header actions; sidebar; the current page; toasts; one modal dialog slot; onboarding replaces the shell. Native decorated window (title bar coloured with DWM by Rust). Placeholder visuals: the window owner (P-UI-SHELL) replaces the layout but keeps every property and callback; page callbacks are forwarded with a page prefix so Rust binds them on this one component (docs/ARCHITECTURE.md §7.3).

| Property | Dir | Type | Notes |
|---|---|---|---|
| `page` | in | `PageId` | ── shell state ── (default `PageId.home`) |
| `onboarding` | in | `bool` | Show onboarding instead of the shell (not onboarded, or the onboarding route). |
| `page-title` | in | `string` | Header title of the current page ("History"). |
| `theme-dark` | in | `bool` | The resolved main-window theme (drives the header theme toggle's label and icon). (default `true`) |
| `backdrop` | in | `image` |  |
| `sidebar` | in | `SidebarData` |  |
| `toasts` | in | `[ToastData]` |  |
| `dialog` | in | `DialogData` |  |
| `settings-scroll-to` | in | `string` | Settings deep link (section id) and its re-announce nonce. |
| `settings-scroll-nonce` | in | `int` |  |
| `home` | in | `HomeData` | ── page data ── |
| `history` | in | `HistoryData` |  |
| `analytics` | in | `AnalyticsData` |  |
| `voices` | in | `VoicesData` |  |
| `settings` | in | `SettingsData` |  |
| `onboarding-data` | in | `OnboardingData` |  |
| `onboarding-key-draft` | in-out | `string` | ── text fields (in-out: the field's own text; Rust writes them only to reset) ── The onboarding key draft (kept across steps; cleared by Rust after a successful connect). |
| `settings-key-draft` | in-out | `string` | Settings → Account key field. |
| `settings-exclude-draft` | in-out | `string` | Settings → Triggers excluded-app input (cleared after a successful add). |
| `settings-port-draft` | in-out | `string` | Settings → Integrations port field (digits only, ≤ 5; reset on Escape and when the stored port changes). |
| `voices-search-text` | in-out | `string` | Voices search field (cleared by "Clear filters" and when the page is left). |
| `history-search-text` | in-out | `string` | History search field (reset by "Clear filters" and by a sidebar search). |
| `sidebar-search-text` | in-out | `string` | Sidebar "Search history" field (cleared after submit). |
| `sidebar-focus-nonce` | in-out | `int` | Focus requests (Slint-local; bumped by the root FocusScope). |
| `history-focus-nonce` | in-out | `int` |  |
| `rail` | out | `bool` | Sidebar collapses to icons below 1100 px (ui-main.md §2.6 RAIL_QUERY; D-UM-29). (default `root.width < 1100px`) |

| Callback | Args | Meaning |
|---|---|---|
| `navigate` | `(PageId)` | ── shell callbacks ── |
| `sidebar-search` | `(string)` | Sidebar "Search history" submitted (Enter) → History with that search (applied at once). |
| `mini-action` | `(PlayerAction)` |  |
| `mini-open` | `()` |  |
| `credits-open` | `()` |  |
| `header-send-to-tray` | `()` | Header: send the window to the tray (Command::HideMain) / switch between paper and night. |
| `header-toggle-theme` | `()` |  |
| `toast-action` | `(string)` |  |
| `toast-dismiss` | `(string)` |  |
| `toast-hover` | `(string, bool)` |  |
| `dialog-confirm` | `(string)` |  |
| `dialog-cancel` | `(string)` |  |
| `home-quick-read` | `(string)` | ── Home ── |
| `home-quick-read-edited` | `(string)` |  |
| `home-read-clipboard` | `()` |  |
| `home-player-action` | `(PlayerAction)` |  |
| `home-seek` | `(float)` |  |
| `home-replay` | `(string)` |  |
| `home-replay-last` | `()` |  |
| `home-pause-live` | `()` |  |
| `home-open-page` | `(PageId)` |  |
| `home-open-settings` | `(string)` |  |
| `home-credits-retry` | `()` |  |
| `home-retry` | `()` |  |
| `home-resume-live` | `()` |  |
| `history-search-edited` | `(string)` | ── History ── |
| `history-set-favorites-only` | `(bool)` |  |
| `history-set-source` | `(string)` |  |
| `history-clear-filters` | `()` |  |
| `history-toggle-row` | `(string)` |  |
| `history-replay` | `(string)` |  |
| `history-copy-text` | `(string)` |  |
| `history-toggle-favorite` | `(string)` |  |
| `history-delete` | `(string)` |  |
| `history-open-url` | `(string)` |  |
| `history-load-more` | `()` |  |
| `history-export` | `()` |  |
| `history-clear-all` | `()` |  |
| `history-retry` | `()` |  |
| `history-paste-and-read` | `()` |  |
| `analytics-set-range` | `(string)` | ── Analytics ── |
| `analytics-retry` | `()` |  |
| `analytics-refresh-credits` | `()` |  |
| `analytics-add-key` | `()` |  |
| `analytics-daily-width` | `(float)` | The daily plot's width in logical px (on creation and on every change). |
| `voices-search-edited` | `(string)` | ── Voices ── |
| `voices-set-gender` | `(string)` |  |
| `voices-set-accent` | `(string)` |  |
| `voices-set-category` | `(string)` |  |
| `voices-clear-filters` | `()` |  |
| `voices-refresh` | `()` |  |
| `voices-preview` | `(string)` |  |
| `voices-use-voice` | `(string)` |  |
| `voices-add-key` | `()` |  |
| `settings-section-selected` | `(string)` | ── Settings ── |
| `settings-set-bool` | `(string, bool)` |  |
| `settings-set-number` | `(string, float)` |  |
| `settings-set-choice` | `(string, string)` |  |
| `settings-format-number` | `pure (string, float) -> string` | A slider's live label while dragging (dotted key, value) → formatted text. |
| `settings-save-key` | `(string)` |  |
| `settings-key-edited` | `()` |  |
| `settings-open-key-form` | `()` |  |
| `settings-cancel-key-form` | `()` |  |
| `settings-remove-key` | `()` |  |
| `settings-get-key` | `()` |  |
| `settings-credits-retry` | `()` |  |
| `settings-test-voice` | `()` |  |
| `settings-browse-voices` | `()` |  |
| `settings-reset-voice-settings` | `()` |  |
| `settings-hotkey-start` | `(string)` |  |
| `settings-hotkey-stop` | `()` |  |
| `settings-hotkey-clear` | `(string)` |  |
| `settings-add-excluded` | `(string)` |  |
| `settings-remove-excluded` | `(string)` |  |
| `settings-excluded-edited` | `()` |  |
| `settings-clear-cache` | `()` |  |
| `settings-export-history` | `()` |  |
| `settings-clear-history` | `()` |  |
| `settings-port-edited` | `(string)` |  |
| `settings-commit-port` | `(string)` |  |
| `settings-revert-port` | `()` |  |
| `settings-copy-text` | `(string)` |  |
| `settings-show-extension-folder` | `()` |  |
| `settings-open-data-folder` | `()` |  |
| `settings-open-link` | `(string)` |  |
| `onboarding-go-to` | `(int)` | ── Onboarding ── |
| `onboarding-next` | `()` |  |
| `onboarding-back` | `()` |  |
| `onboarding-skip` | `()` |  |
| `onboarding-connect-key` | `(string)` |  |
| `onboarding-key-edited` | `()` |  |
| `onboarding-get-key` | `()` |  |
| `onboarding-replace-key` | `()` |  |
| `onboarding-cancel-replace` | `()` |  |
| `onboarding-choose-voice` | `(string)` |  |
| `onboarding-preview-voice` | `(string)` |  |
| `onboarding-retry-voices` | `()` |  |
| `onboarding-read-sample` | `()` |  |

**`WidgetWindow`** (`ui/windows/widget-window.slint`, inherits `Window`) — The floating player (DESIGN.md "Floating widget"; ui-settings-widget.md §6; shell.md §6.3). A frameless, transparent, always-on-top, never-activating window that is exactly the card (380 w full card, 48-h compact pill; radius 20). The Window does **not** bind its own width/height: the card lays out at the window's top-left at its preferred size and reports it through `card-width`/`card-height` + `size-changed`; Rust then applies position and size in one SetWindowPos (keeping the anchored edges fixed, shell.md §6.3.3) and Slint follows WM_SIZE — so a bottom-anchored card never grows downward for a frame. Rust starts a native move from `drag-start` (pointer-down on a text/label handle — never on a control). Always dark (Theme.dark = true, D-USW-20). The card replays its enter animation when `data.card-key` changes (0 ms under Motion.reduce). Compact pill (D-USW-21): art dot, play/pause, then the compact toast / ended-early note / "Synthesizing" loader / small progress bar, time, close. Placeholder visuals; keep the interface.

| Property | Dir | Type | Notes |
|---|---|---|---|
| `data` | in | `WidgetData` |  |
| `opaque-backing` | in | `bool` | An opaque panel under the card (same shape); Rust sets it (no per-pixel alpha with Skia on DirectX 12: the enter fade and the 0.96 panel would blend over black; SetWindowRgn cuts the corners). Previews leave it off. |
| `card-width` | out | `length` | The card's preferred size (Rust sizes the native window to it). (default `380px`) |
| `card-height` | out | `length` | (default `168px`) |

| Callback | Args | Meaning |
|---|---|---|
| `action` | `(PlayerAction)` | Transport and chrome: toggle, previous, next, cycle-rate, close. |
| `seek` | `(float)` | Progress released at a 0..1 fraction (only when `seekable`). |
| `error-action` | `()` | Error card button ("Open settings" / "Try again"). |
| `warning-clicked` | `()` | Warning note link (settings-fixable codes) → Settings → Account. |
| `toast-action` | `()` | Toast action button / dismissal by click. |
| `drag-start` | `()` | Pointer-down on a drag handle. |
| `size-changed` | `(length, length)` | The card's size changed (also called once after the first layout). |

---

## 8. Decisions log

### 8.1 Product rulings (BRIEF.md) and where they are implemented

| Id | Ruling (short) | Implemented by |
|---|---|---|
| R-UI | Native Slint, in-process: IPC, sender checks, webview hardening, drag-region lag fixes, 38-DIP minimum, readiness latch disappear; keep: no lost clicks, no focus stealing, `sessionStart` before chunks, early toasts not lost | §3.6; `windowing::*` (attributes hook `with_active(false)`, overlays pre-created and subclassed before they can be seen, HTCLIENT + `drag_window` from text handles only); `UiSink` buffering; `UiInit` before the first frame |
| R-Text-offsets | Internal char/byte indices allowed; persisted / old-file offsets in UTF-16 | §5.6 (UTF-16 at every crate boundary, stronger than required) |
| R-Profile | `%APPDATA%\Vox` in place; format-compatible files; byte-identical numbers not required; corrupt main + good `.bak` → rewrite at once and log | vox-store (§6) |
| R-API-key | Migrate Electron `v10`; on failure ask again with a clear message; store as plain DPAPI in `secrets-dpapi.bin`; leave `secrets.bin` untouched | `vox_store::secrets`, `settings_store::key_status`, `AppInfo.api_key_notice`, §4.19 |
| R-Do-not-run-both | Electron instance running → native message, exit | `vox_win::single_instance::electron_instance_running`, `shell::message_box`, §4.18 |
| R-Media-keys | SMTC live; no restart toast/copy; works with the widget off | `vox_win::media`, `core::media`, `core::reactions` |
| R-Hotkeys | `MOD_NOREPEAT`; keep Electron meaning (`Plus` = Shift+`=`); `AltGr` = Ctrl+Alt | `vox_core::hotkey` (D-HK-*), `vox_win::hotkeys` |
| R-Network | System proxy, Windows cert store, UA `Vox/<version> (Windows)`, non-streaming `with-timestamps`, 220-char first chunk, no timeout retries, remember rejected optional params, bill chunks that returned audio even after a stop | `vox_tts::{client, error, engine}` (`set_proxy`, direct retry when the proxy itself fails), `vox_win::registry::system_proxy` (D-NET-1..7) |
| R-Text-cleanup | Old rules + fixes: HTML entities, `well-\nknown`, `mailto:` case-insensitive, autolink brackets | `vox_text::clean` (D-TXT-1..4) |
| R-Audio | Gapless chunks; pitch-preserving speed; player failure → restart player + stop engine + error card | `vox_audio::player` (§5.3, §5.9), `core` (`PlayerFailed`) |
| R-Clipboard | Restore all old formats + `CF_HDROP` + `CF_DIB`; restore writes marked not-for-history / excluded from monitors; detect UIPI → "Vox can’t read from apps running as administrator" | `vox_win::{clipboard, selection, foreground::input_blocked_by_uipi}` (D-SEL-2, D-SEL-8), `copy::elevated_app_refused` |
| R-Hook | Hook install failure → Triggers notice; retry when trigger settings change | `vox_win::selection` (D-SEL-1; + silent-removal watchdog), core `hook_status` → `vox_core::app::native_hook_available` → `AppInfo.native_hook_available`, `TriggersData.hook-notice-*` |
| R-DPI | Per-monitor-v2, physical px placement; Electron DIP `customX/Y` converted through the containing monitor, else re-clamp to the nearest work area | `windowing::dpi` (D-DPI-1), `vox_win::window::monitors` |
| R-Main-window | Remember size, position, maximized (`window-state.json`) | `vox_store::window_state`, `windowing::main_window` (§6.5) |
| R-Widget-z-order | Widget topmost above normal windows; chip topmost | `vox_win::window::install_overlay_subclass` (enforces `WS_EX_TOPMOST`), `assert_topmost(raise)` |
| R-Tray | Left click and keyboard select open the main window; double click = left click | `tray` + `vox_win::notify_icon` (`NOTIFYICON_VERSION_4`: `NIN_SELECT`/`NIN_KEYSELECT`; D-SH-18) |
| R-Copy | Rewrite old-look copy; curly apostrophes; one model-name style; About shows version + build hash; System/Paper/Night stays | D-COPY-1, `vox_core::format::model_name`, `AboutData.build`, build.rs |
| R-Analytics | 700 ms debounce; "All time" one window; empty banner checks listening; "This week" = rolling, labelled "Last 7 days" | `ui::analytics` (debounce only for background refreshes, D-UM-5), D-AN-2 (capped at `MAX_ALL_DAYS`), `WeekData.title` |
| R-Home | Failed first load → inline error + Retry; paused read on Recent offers Resume | `HomeData.state`, `home-retry`, `RecentRow.resumable`, `home-resume-live` |
| R-Formatting | Windows user locale for dates and numbers where `Intl` defaults were used | `vox_core::locale` + `vox_win::system::user_locale` (D-FMT-1) |
| R-Installer | Out of scope; portable `vox.exe` + `browser-extension\` next to it | `runtime::extension_dir` (D-INT-13) |
| R-Demo | `VOX_DEMO=1` (Vox-Demo, seeded history) and `VOX_MOCK_TTS=1` stay; same shape/distribution | `vox_core::app::RuntimeFlags`, `vox_store::demo`, `vox_tts::mock` |

### 8.2 Cross-cutting architect decisions

| Id | Decision | Reason |
|---|---|---|
| D-FMT-1 | Locale = conventions only (decimal/group separators, 12/24 h, day-first vs month-first); weekday/month names and words stay English; `GetLocaleInfoEx` read at startup and on `WM_SETTINGCHANGE("intl")`; the truncation toast stays en-US (`format_number_en`) because the old app pinned it | English copy with foreign names would be inconsistent; conventions are what users notice |
| D-COPY-1 | Curly `’` in **all** user-facing copy, core copy included; logs keep ASCII | R-Copy; identity comparisons use the same constants |
| D-UI-2 | One model name: catalogue name minus leading `Eleven ` (kept before a version), else `pretty_model_id`, else title-cased id (`format::model_name`) | R-Copy ("Flash v2.5" everywhere) |
| D-UI-5 | No in-app mock façade (contracts.md §7): page previews are `ui/previews/*.json` + vox-shot; the running app's offline mode is demo/mock TTS | Static JSON previews cover component work; demo mode covers runtime |
| D-UI-6 | `Brand` global for the product name in static copy | contracts.md §1.1 "never hard-code Vox" |
| D-ARCH-1 | Core thread + `Command` channel + `UiSink`; UI polls `Player::snapshot()` at 30 Hz while active instead of the core pushing frames | Fewer cross-thread messages; BRIEF's 30 Hz satisfied on the UI side |
| D-ARCH-2 | Only the core thread mutates settings/history; listeners only enqueue (store listeners run after the store lock is released, D-P-18). The system watcher's 2.5 s fallback `flush_sync` writes in-memory state only (D-P-20) | Ordering and reentrancy safety |
| D-ARCH-3 | Panics: every UI callback, core dispatch and service callback runs under `catch_unwind`; a panicking command is logged (`panic` scope); **if it carried a `RequestId` it is still answered with its failure variant** (`core::owed_answer` taken before dispatch → `failure_update`: `ReadStarted{internal_error()}`, `Home/Analytics/HistoryPage/…{Err("Something went wrong")}`); FFI callbacks never unwind | Robustness; a dropped request would leave skeletons or spinners forever (R-Home, `read-busy`) |
| D-ARCH-4 | `UpdateSettings` always answers with `UiUpdate::Settings` (even unchanged), then `SettingsSaved{req}` when it carried a request id | contracts.md §8 #9 + the optimistic overlay needs to know when **its** patch settled (ui-settings-widget.md §1.1) |
| D-ARCH-5 | `ReadPrepared` / `ClipboardText` commands for worker-prepared reads (file extraction and clipboard reads off the core thread); worker failures come back as `Command::Toast` | Network-share files and delayed clipboard rendering must not stall the core; toasts need the core's router |
| D-ARCH-6 | The UI controller interface is fixed (`UiCtx`, `PageController`, sinks, `NavIntent`, `UiToastAction`, `ShellController`; §7.3) | Several packages implement the two sides in parallel |
| D-ARCH-7 | Typed player commands from every source go through `vox_core::player::sanitize_player_command` in the core; Slint `float`s go through `settings::quantize` before they become settings | contracts.md §5.1 "keep these validators in Rust even in-process"; `f32` noise must not reach settings.json or ElevenLabs |
| D-ARCH-8 | "Stop hides the widget" is a **core intent**: `reader::command(Stop)` publishes `Widget(Hide)` when a session existed before the stop, for every source; the UI never infers a hide from `idle` | In Electron every stop reached the widget; here the widget sees only snapshots, and a supersede also reports `idle` |
| D-ARCH-9 | `UiInit` built by `app::run` from the stores is applied before any window is shown (settings, theme, app info, route, placement, Analytics range, reduce motion) | The first frame must not show the wrong theme, an empty gate or a jumping window (ui-settings-widget.md §1.9; §3.6 #5) |
| D-KEY-3 | Key problem + onboarded → main window opens on settings/account at startup; notice also on the onboarding key step | R-API-key "ask for the key again … with a clear message" |
| D-DPI-1 | Electron DIP space model: monitor DIP origin = physical origin / own scale; load maps through the first containing monitor, else nearest monitor + `remember` clamp; save maps back and `js_round`s | Exact on single-DPI setups (files interchangeable), safe on mixed DPI |
| D-AN-2 | `all` analytics daily series covers the whole window (no 365 cap) but starts no earlier than `today − (MAX_ALL_DAYS − 1)` (3 653 days); older records fall outside `inRange` everywhere | R-Analytics; one malformed `createdAt` (0, −1e14) must not build a 20 000-entry series on every 700 ms refresh |
| D-LL-1 | Hour keys validated strictly (impossible dates and years < 100 rejected; DST gaps resolve forward) | contracts.md §8 #5/#15 recommendation; only hand-edited files differ |

### 8.3 contracts.md §8 (inconsistencies 1–22)

| Id | Decision |
|---|---|
| D-C-1 | Unicode word rule everywhere, including the sanitize fallback (`history::count_words`) — except the demo generator, which keeps the whitespace rule (D-P-14) |
| D-C-2 | Core clamp stays 50..10 000 MB; the Library slider's max is `max(5000, stored value)` so a hand-edited value is shown truthfully (also D-USW-15) |
| D-C-3 | Export file name keeps the UTC date (parity; harmless) |
| D-C-4 | One clamp: `HistoryQuery::effective_limit` 1..500 (default 50); search cut to 500 units |
| D-C-5/15 | = D-LL-1 |
| D-C-6 | Keep values that round to 0.0 in the ledger (parity; `listening_entries` ignores them) |
| D-C-7 | `parsePlayerState` gone: player states are produced in-process by `vox_audio` and are valid by construction |
| D-C-8 | `/health` keeps `playing` (the popup uses it) |
| D-C-9 | = D-ARCH-4 |
| D-C-10 | Media keys live (R-Media-keys) |
| D-C-11 | Chromium workarounds dropped: drag-region lag (native HTCLIENT + `drag_window`), 38-DIP minimum, `Session/` collision (we never create `cache`), `Error invoking remote method` prefix (`clean_error` still strips `XxxError:`), `disable-lcd-text` (no dot fonts). Requirements kept: no lost clicks, no focus steal, chip clickable anywhere |
| D-C-12 | Both parsers kept: lenient for in-app commands (typed anyway), strict for the bridge (HTTP 400) |
| D-C-13 | Bridge guards follow the code (every request incl. `/health`; `null`/`file:` origins refused) |
| D-C-14 | Readiness latch gone (D-RP-1) |
| D-C-16 | `js_round` wherever the spec says rounded (vox-core `js`) |
| D-C-17 | All cuts are surrogate-safe (`prefix16`); a one-unit difference from the old app in rare cases is accepted |
| D-C-18 | Title caps: bridge/UI 300, record 200 (only 200 is stored) |
| D-C-19 | UI validates exe names with `normalize_exe_name` (stricter), the store re-normalises with `normalize_excluded_apps`; one accelerator canonical function (parts trimmed) |
| D-C-20 | `AppInfo.server_port` semantics kept (bound port, else last requested, else settings) |
| D-C-21 | A drag snaps back into the work area (kept, intended) |
| D-C-22 | Read requests no longer wait for widget readiness (answers in < 100 ms) |

### 8.4 tts.md

| Id | Decision |
|---|---|
| D-T-1 | Units: UTF-16 code units (not `char`s as tts.md's warning suggests) — required by R-Text-offsets and existing caches; mock alignment stays one entry per UTF-16 unit |
| D-T-2 | HTTP: blocking `ureq` on threads (BRIEF), not reqwest/tokio |
| D-NET-1 | Proxy: env override, then WinINET registry (`ProxyEnable`/`ProxyServer` incl. per-protocol syntax/`ProxyOverride` incl. `<local>`); PAC/WPAD not supported (logged once when `AutoConfigURL` is set). Read at startup **and** re-read on `SystemEvent::Resumed` and before a read / catalog fetch when the last check is ≥ 30 s old; a changed value is applied with `ElevenLabsClient::set_proxy` (atomic agent swap; in-flight requests keep the old agent). A request that fails **before sending** because of the proxy itself (`ConnectProxyFailed`, refused/timed-out connect to the proxy host) is retried once directly and `proxy <host:port> unreachable; using a direct connection` is logged once — safe because nothing was sent (a stale proxy must not break what worked in Electron, which ignored proxies) |
| D-NET-2 | Retry only certainly-unprocessed failures (429/5xx/unsent); never timeouts, post-send network errors, 2xx-bad-body or no-audio (see `vox_tts::error`) |
| D-NET-3 | A rejected `language_code` (per model + code) or context (per model) is remembered for the process lifetime; requests are reduced up front |
| D-NET-4 | Billing counts every request that returned audio, including one finishing after a stop. `stop()` / an implicit stop emits `Stopped{billed_so_far}` **synchronously on the caller's thread** (tts.md §4.7; keeps the §4.4 supersede order and lets a quit persist known billing); the worker emits one more, cumulative `Stopped` only if an in-flight request still returned audio — it may follow events of a newer session (consumers key by `read_id`; the reader writes the absolute value) |
| D-NET-5 | A later-chunk failure is delivered as `sink.error(non-fatal)` → `events(Stopped{error})` → `sink.done`, under the engine lock (the original emitted `stopped` after `done`, which was safe only because Electron handled it synchronously before any widget IPC; with threads the player's `ended` could overtake it and store a partial read as completed). Second guard: `Sessions::on_state` treats a snapshot warning as partial |
| D-NET-6 | Billing is accumulated as an unrounded `f64` per session (`EngineEvent::{Complete, Stopped}.characters_billed: f64`); the reader writes `max(0, js_round(billed))` once (1.5 + 1.5 → 3) |
| D-NET-7 | Response bodies: `http_status_as_error(false)`; bytes under the request timeout (≤ 32 MiB) → lossy UTF-8 → `vox_core::json::parse_lenient` for 2xx and error bodies alike (never stricter than Node, since D-NET-2 makes a 2xx bad body final) |
| D-T-7 | One `Mutex<EngineState>` serialises every "re-check live → update → sink → event" step of the worker with `start`/`stop`/`play_cached` (a stale `Failed` toast for a superseded read is impossible by construction) |
| D-T-3 | Sentence segmentation with `icu_segmenter` (compiled data), validated by tts.md §3.7's table; `fallback_segments` kept for tests |
| D-VO-1 | `sort_voices` uses `icu_collator` (primary strength, numeric; already a vox-tts dependency); the Voices page keeps vox-core's simple lower-case ordering (UI-only) |
| D-T-4 | Lenient base64 exactly as tts.md §2.6 |
| D-T-5 | `truncateAtBoundary` not ported (unused) |
| D-T-6 | Listener and sink panics caught and logged everywhere (tts.md §4.10 note) |

### 8.5 reader-playback.md

| Id | Decision |
|---|---|
| D-RP-1 | The core owns one long-lived `Player`; no widget readiness, no renderer crash recovery (§12 #29 cannot happen) |
| D-RP-2 | Gapless joins (R-Audio, §5.3) — the "small gap between chunks" is gone |
| D-RP-3 | No "external" element pause/play: SMTC goes through `Command::Media` → normal commands |
| D-RP-4 | `NotAllowedError`/autoplay paths are gone |
| D-RP-5 | A start failure wins over the optimistic `playing` (sync-throw quirk fixed) |
| D-RP-6 | `persistPlayback` folds `player.snapshot()` (exact values at quit/log-off), not the last ≤ 500 ms-old report |
| D-RP-7 | Play/pause hotkey quirks kept (no-op on the error card; replays an auto-hidden ended read with the window hidden) |
| D-RP-8 | Long-sentence highlighting uses a character window (`vox_text::highlight`) instead of DOM line scrolling |
| D-RP-9 | No output device → simulated clock (as Chromium), also mid-read (a reopen failing with `NoDevice` never counts toward the failure budget); only `Stream` failures and panics → R-Audio recovery |
| D-RP-10 | "Audible" = the device has played blocks of the current generation; stalls cannot occur except on under-run (logged) |
| D-RP-11 | An explicit Stop yields `[State(idle, session), State(idle, no session)]`; a supersede yields `[State(idle, old), State(loading, new)]`; every spec `notify()` point emits a `State` (reader-playback.md §4.6, §4.10, §9 Lifetime) |
| D-RP-12 | Position, highlight and chunk `ended` use the device playback instant of each block; the listening clock uses only that device tag (never the display extrapolation); the player ticks only while needed |
| D-RP-13 | The output is preloaded 1 s after core startup whatever the window visibility; `playbackRate`/`volume` are applied to the reading player once at startup (reader-playback.md §6, §7) |
| D-RP-14 | Cache saves and replay loads run on the audio cache's I/O worker (`Command::{CacheSaved, ReplayLoaded}`); `Complete` patches duration + billing at once and `cached` after the save (chosen over one patch after the save, reader-playback.md §2.13's literal order: billing and duration must survive a quit or log-off while a 100 MB save runs; the only visible difference is a short window in which the record is not yet `cached`); a record deleted during the save has its entry removed (§12 #17) |

### 8.6 selection.md §13 quirks

| Id | Decision |
|---|---|
| D-SEL-Q1 hotkey clears the hint | keep |
| D-SEL-Q2 navigation keys keep the hint | keep |
| D-SEL-Q3 numpad Insert / IME / Launch Mail | keep the VK equivalents of §4.3 |
| D-SEL-Q4 blocked-window asymmetry | keep |
| D-SEL-Q5 non-text drag | keep |
| D-SEL-Q6 sticky hook failure | **changed**: retried on trigger changes (D-SEL-1, R-Hook) |
| D-SEL-Q7 `.exe` forced | keep |
| D-SEL-Q8 exception in copy → `no-selection` | keep (logged) |
| D-SEL-Q9 `restoreClipboard = false` semantics | keep |
| D-SEL-Q10 late copy after `no-selection` | keep |
| D-SEL-Q11 display change hides the chip directly | keep |
| D-SEL-Q12 capture ignores the hint | keep |
| D-SEL-Q13 mouse events during capture | keep |
| D-SEL-Q14 busy capture ignores triggers silently | keep |
| D-SEL-Q15 chip anchor points | keep |
| D-SEL-Q16 late-copy window can read the user's own copy | keep (parity; documented) |
| D-SEL-Q17 copy-on-select after a failed capture | keep |
| D-SEL-Q18 elevated foreground | **changed**: UIPI refusal notice before sending keys, detected with a direct `PostMessageW` probe (D-SEL-2, R-Clipboard) |
| D-SEL-Q19 keydowns ignored during capture | keep; additionally our `SendInput` events carry a `dwExtraInfo` tag the hook drops (D-SEL-3) |
| D-SEL-Q20 loss-notice wording | keep |
| §9 `CF_HDROP` raw restore | adopted (R-Clipboard), plus `CF_DIB` and the "do not record" markers |
| §6.1 coordinates | physical px throughout (Per-Monitor-V2); DIP only inside `vox_core::geometry` (D-SEL-4) |

### 8.7 shell.md §12 and inline questions

| Id | Decision |
|---|---|
| D-SH-1 | Hidden top-level window in `vox_win::system` (session end, outside `WM_CLOSE`); `TaskbarCreated` is handled by `vox_win::notify_icon` (icon re-added) |
| D-SH-2 | Floating windows: all `HTCLIENT`; moves start on pointer-down on a text handle via winit `drag_window` (= `ReleaseCapture` + `WM_NCLBUTTONDOWN(HTCAPTION)`), so hover/clicks are never lost. Styles are **enforced**, not applied once: winit 0.30's `WindowFlags::apply_diff` rewrites `GWL_STYLE`/`GWL_EXSTYLE` (adding `WS_SYSMENU`, dropping our extended bits) on every visibility/topmost/hit-test change, so `vox_win::window::install_overlay_subclass` handles `WM_STYLECHANGING` (re-adds `WS_EX_NOACTIVATE \| WS_EX_TOOLWINDOW \| WS_EX_TOPMOST`, strips `WS_SYSMENU`), makes a newly layered window render (`SetLayeredWindowAttributes`), answers `WM_MOUSEACTIVATE → MA_NOACTIVATE`, swallows the system menu and turns `WM_CLOSE` into `WindowEvent::CloseRequested`. Chip click-through = winit `set_cursor_hittest` (survives diffs) |
| D-SH-3 | Overlays are created through the winit attributes hook with `with_active(false)` (every Slint `show()` then uses `SW_SHOWNOACTIVATE`), skip-taskbar, always-on-top; pre-created off-screen and subclassed before anyone can see them (`overlays_ready`). Visibility always goes through Slint; `assert_topmost(hwnd, raise)` re-asserts the z-order after each show (chip raised to the top of the topmost band) |
| D-SH-5 | Single instance: per-user named mutex (`Local\<key>-<SID>`) + per-user named pipe (`\\.\pipe\<key>-<SID>`, DACL = user + SYSTEM, `PIPE_REJECT_REMOTE_CLIENTS`, first instance created inside `acquire`) carrying JSON `{argv, cwd}` (not `WM_COPYDATA`); the secondary verifies the server's user SID before `AllowSetForegroundWindow` and writing; demo and normal profiles have different keys; argv read lossily (`args_os`) |
| D-SH-7 | `MOD_NOREPEAT` on; `Plus`/`+` keep Electron's implicit Shift (R-Hotkeys) |
| D-SH-8 | A drag ends as "user moved" only if the window rect changed and > 500 ms after our own move |
| D-SH-9 | Widget = `HWND_TOPMOST` (above normal windows, R-Widget-z-order); the Electron "just under the taskbar" nuance is dropped |
| D-SH-11 | Main window geometry persisted (R-Main-window) |
| D-SH-12 | Crash reload, devtools, navigation hardening: gone |
| D-SH-13 | Grayscale text AA not required (no dot-matrix fonts) |
| D-SH-14 | Tray icon redrawn for DESIGN.md (white "V" wave mark; active dot), rendered at 16/20/24/32 px at startup with tiny-skia |
| D-SH-15 | Ctrl+C in a dev console is not trapped (atomic writes keep files consistent); release builds have no console |
| D-SH-16 | `--hidden` and launch-at-login unchanged; the Run value is compared with full argv (D-INT-9) |
| D-SH-17 | Screenshot harness: Slint `take_snapshot()` per window; `report.json` keeps its shape with `app.runtime` instead of `app.electron`; the sequence runs as `slint::spawn_local` on the UI thread with Timer-based sleeps (`shots::start` returns a cell with the exit code), a 100 s watchdog thread exits 2, the mock read is a `ReadText{req}` whose `ReadStarted` is forwarded to the harness, the loading/error states are driven on the reading player directly, a tracing layer collects console entries, `displays[].id` = `HMONITOR`, the initial CLI is not handled; the runner is `scripts/shots.ps1` (`tools/` is frozen) |
| D-SH-18 | Tray icon = `vox_win::notify_icon::NotifyIcon` (`NIM_SETVERSION(NOTIFYICON_VERSION_4)`, `NIN_SELECT`/`NIN_KEYSELECT`/left up/double click → Activate, `WM_CONTEXTMENU` → muda menu via `show_context_menu_for_hwnd`), replacing `tray-icon` (0.26.1 never sets the version, so keyboard selection arrives as `WM_RBUTTONDOWN/UP` and opens the menu — R-Tray could not be met). Icon size = `SM_CXSMICON` at the primary monitor's DPI, re-picked on `DisplaysChanged`. Tray Open/OpenPage handled synchronously on the UI thread (the click's foreground permission is short-lived). Menu hints are display text after a tab, never muda accelerators |
| D-SH-19 | Toasts of the unpackaged exe: `vox_win::shell::register_app_user_model(APP_ID, APP_NAME, <profile>\vox-icon.png)` (HKCU `AppUserModelId`) at startup (not in shots mode), then `CreateToastNotifierWithId(APP_ID)` with a silent `ToastGeneric`; the first failure per session is logged (scope `notify`). Without it Windows drops every core notification (widget off + main hidden) |
| D-SH-20 | `vox.exe` embeds ICON 1 (`crates/vox-app/assets/vox.ico`, 16–256 px, generated once by an `#[ignore]` test from the tray's tiny-skia drawing), VERSIONINFO (`Vox`, `CARGO_PKG_VERSION`) and a manifest (PerMonitorV2, Common-Controls v6) from `build.rs` (`winresource` or `embed-resource`, added by P-APP-WIN with the assets); `vox-256.png` is the `MainWindow` icon and the AUMID icon source. Explorer's `Icon="<exe>",0`, the taskbar, Alt+Tab and Startup apps then show the Vox name and icon |
| D-SH-21 | Close requests: `on_close_requested` returns `KeepWindowShown` on all three windows; main → close to tray or quit on the UI thread; widget/chip (`WM_CLOSE` via the subclass) → `WindowEvent::CloseRequested(kind)` → the core logs `<kind> window got a close request; quitting`, runs `session_end`, quits |
| D-SH-22 | Quit is bounded: a 7 s watchdog thread armed by the first `Quit`; `UiUpdate::Quit` right after the bounded flush; players and SMTC shut down afterwards on helper threads joined ≤ 500 ms; the UI saves `window-state.json` itself on `Quit` |
| D-SH-23 | The instance listener starts right after `acquire` (the channel exists from step 1, the primary's own `Cli` is enqueued first), so a launch during a slow startup is buffered, never lost, and never overtakes the primary's own arguments |

### 8.8 persistence.md

| Id | Decision |
|---|---|
| D-P-1 | UTF-16 lengths/offsets (§5.6) |
| D-P-2 | Lone surrogate escapes repaired on read, never written (char-safe cuts) |
| D-P-3 | JS number formatting on write (`js_f64`), `-0` → `0` |
| D-P-4 | Lenient reads: out-of-range numbers become a non-number for that field only (`json::quote_out_of_range_numbers`) |
| D-P-5 | `preserve_order` maps; struct field order = Electron order; `listenLog` ascending |
| D-P-6 | DST: gaps forward, overlaps earlier (`vox_core::time`) |
| D-P-7 | Never create `cache\` |
| D-P-8 | Busy retries also on chunk reads, manifest reads and secret reads (harmless, recommended) |
| D-P-9 | One mutex per store; snapshot at write time |
| D-P-10 | Secrets: `secrets-dpapi.bin` (R-API-key) supersedes persistence.md §4.5's "rewrite `secrets.bin`" |
| D-P-11 | Corrupt main + good `.bak` → immediate rewrite (R-Profile) |
| D-P-12 | Secret writes are synchronous in `set_api_key` (tiny file), so a log-off right after a key change keeps it |
| D-P-13 | Store warnings go to `main.log` (scope `data`) |
| D-P-14 | `generate_demo_records` sets `word_count = count_whitespace_words(text)` (persistence.md §8.4), not the reader's `count_words`: four demo snippets count differently (Council 81 vs 82, Tenancy 123 vs 127, Urban 168 vs 169, Weekly 176 vs 175) and the §12 vectors (totals.words, avgWordsPerRead, avgListenWpm, readingTimeEquivalentSec) need the whitespace rule |
| D-P-15 | `parse_manifest` normalises what §6.3 accepts "as is" but typed fields cannot hold: `index`/`textStart`/`textEnd` → finite ? `js_round(max(0, x))` saturated to `u32` : 0; `bytes` likewise to `u64`; `durationSec` → finite ? x : 0; alignment kept only with three equal-length arrays, a non-string char → `""`, a non-number / non-finite start/end → `NaN` (rejected for timing, written `null`). Touch/redact rewrite the normalised values (R-Profile) |
| D-P-16 | A `history.json` of the wrong shape (not an array, not an object with an array `records`) is **corrupt** (`.corrupt` + `.bak` + immediate rewrite), not "zero records" — the spec flagged the old behaviour as undesirable and it silently wipes history and `.bak` on the next mutation; `settings.json` keeps the spec behaviour |
| D-P-17 | The audio cache owns one I/O worker (FIFO) for every disk mutation; `remove`/`prune`/`clear`/`remove_orphans` update the index synchronously and queue deletions; saves/loads are `save_async`/`load_async` reporting through `Command`s; `remove_orphans` snapshots its candidates at call time and runs on the core thread during core startup. Why: hundreds of recursive deletes (≈ 600 ms of busy retries each) or a 100 MB load on the core thread would starve player reports, `/read` and `SessionEnd`; FIFO gives persistence.md §6.4's per-id order without per-id locks (a mutex is not FIFO). The proposed `Command::CacheCleared` is unnecessary: `ClearCache` is index-only on the core thread |
| D-P-18 | Store listeners run after the store lock is released, over a cloned list; a re-entrant settings `update()` nests (persistence.md §3.3) |
| D-P-19 | `set_api_key` with `SecretError::Dpapi`: warn `secure storage is unavailable; the API key is kept in memory for this session only` and best-effort write the tombstone, so the next start asks again instead of loading an older key; plain I/O failures log only (parity) |
| D-P-20 | Session end: the watcher waits ≤ 5 s for the core's `SessionEnd` reply and calls `settings.flush_sync()` + `history.flush_sync()` itself after 2.5 s without one (both idempotent; two concurrent `flush_sync`s are serialised by the document's lock; `write_file_atomic_sync` takes no per-path lock) |
| D-P-21 | `JsonDocOptions` gains `retry_base` / `max_retries` (spec values 1 s / 5) so the 6-attempt schedule is testable with ms durations |

### 8.9 integrations.md §7

| Id | Decision |
|---|---|
| D-INT-1 | Guards follow the code (= D-C-13) |
| D-INT-2 | Bridge settings stay under Settings → Integrations; the extension's stale hint stays (extension unchanged) |
| D-INT-3 | Launch-at-login copy is rewritten to match behaviour (static page copy in `ui/pages/settings/integrations.slint`, product name from `Brand`): `Launch at login` — `Start Vox when you sign in to Windows. Turn on Start minimized to keep it in the tray.`; `Start minimized` — unchanged: `Open straight to the tray instead of showing this window.` (it governs every launch, not only sign-in) |
| D-INT-4 | RTF code pages 932/936/950 → Shift_JIS/GBK/Big5 (improvement; golden row updated) |
| D-INT-5 | Windows 11 top-level menu: out of scope |
| D-INT-6 | Keep `"%1"` |
| D-INT-7 | Packaged = `!cfg!(debug_assertions)`; dev Run value `Vox (dev)`; the verb `Icon` is `"<exe>",0` for packaged **and** dev builds (the exe's embedded icon, D-SH-20) |
| D-INT-8 | Firefox stays in the browser allowlist |
| D-INT-9 | Launch-at-login idempotency compares the full argv incl. `--hidden` (fix) |
| D-INT-10 | Parse the Run value with `CommandLineToArgvW` |
| D-INT-11 | Lone surrogates in bridge JSON → U+FFFD (accepted) |
| D-INT-12 | `file://` page URLs dropped (parity) |
| D-INT-13 | Extension folder = `<exe dir>\browser-extension` (portable, R-Installer); `crates/vox-app/build.rs` mirrors the repo's `browser-extension\` next to every built `vox.exe` (`target\<profile>\browser-extension\`); when that folder is missing, the repo folder (dev) |
| D-INT-14 | Superseded by D-INT-16 (tiny_http's accept loop and sockets are private, so the planned socket timeouts could not be built) |
| D-INT-15 | Out-of-range literals in bridge bodies become `null` (`vox_core::json::parse_lenient_with(.., OutOfRange::Null)`), which reproduces every old outcome: `"text":1e400` → `"text" must be a string`; `title`/`url` dropped; numeric fields → `must be a finite number`; `direction` → `must be 1 or -1`; a top-level `1e400` → `Expected a JSON object` (the files' `"Infinity"` string mapping would have read "Infinity" aloud) |
| D-INT-16 | The bridge has its own accept loop (+ `httparse`) instead of `tiny_http` — a deliberate deviation from BRIEF's crate list: tiny_http 0.12 exposes no accepted sockets (no read timeouts, no connection count, no close), drops `Connection` response headers, reads headers without a cap and leaves keep-alive connections feeding a dead queue after a stop, so the §1.10 408/431/keep-alive rows, the 64-connection cap and a clean re-enable were impossible. `SO_EXCLUSIVEADDRUSE` via `windows` (socket2 has no setter); every server-closed socket is closed abortively (linger 0) so the same port can be bound again at once; `stop()` closes the listener before returning and never joins threads blocked in `read`. tiny_http remains the vox-tts test server |
| D-INT-17 | Uninstall (integrations.md §3.6) is deferred with R-Installer. Portable builds: deleting the folder leaves the nine verbs and a Run value pointing at a missing exe — the Settings toggles are the removal path. Because the Rust build reuses the Electron names (`Vox`, `VoxRead`), uninstalling the old Electron build deletes the Rust build's verb and launch-at-login entry; the next start of `vox.exe` repairs both (startup sync) |
| D-INT-18 | `BridgeHandler::read(req, deadline) -> Option<ReadResult>`: `None` (timeout or the core gone) → 504 `Timed out waiting for the app`, code `unknown`; the read is not cancelled |
| D-INT-19 | Every bridge / Explorer / launch-at-login reaction ends with `UiUpdate::AppInfo` (also when it skipped work); the Settings re-check window ends on the first `AppInfo` after it started or after 4 s — replacing the spec's delayed 700/900/1200 ms re-queries |

### 8.10 ui-main.md §12

| Id | Decision |
|---|---|
| D-UM-1 two `creditsLeftFraction` | one (`format::credits_left_fraction`, 0 for unknown limit) |
| D-UM-2 two model labels | one (D-UI-2) |
| D-UM-3 two heard fractions | keep both semantics where they are used (History list caps at 0.99; analytics `heard_fraction`) |
| D-UM-4 mixed apostrophes | `’` everywhere (D-COPY-1) |
| D-UM-5 Analytics refetch storm | 700 ms trailing debounce for `HistoryChanged` only; mount, range change, Retry, day and wake query at once (R-Analytics; Home/History debounce only background triggers too) |
| D-UM-6 `all` window mix | D-AN-2 |
| D-UM-7 empty banner vs listening | banner only when no reads and no listening (R-Analytics) |
| D-UM-8 Home `failed` never shown | inline error + Retry (R-Home) |
| D-UM-9 `Resets tomorrow` < 24 h | keep (parity; harmless) |
| D-UM-10 paused Recent read restarts | Resume (R-Home) |
| D-UM-11 "This week" rolling | label "Last 7 days" (R-Analytics) |
| D-UM-12 "nothing yet" during undo | keep header/toolbar visible while an undo batch is pending |
| D-UM-13 `X of Y reads` always plural | singular when Y == 1 (`count_label`) |
| D-UM-14 theme toggle replaces `system` | keep the three-way choice (R-Copy); the header toggle itself stays (D-UM-28) and writes `paper`/`night` from the resolved theme |
| D-UM-15 rounding | `js_round` (D-C-16) |
| D-UM-16 no startup error state | the in-process shell always has settings: `UiInit` is built from the stores and applied before the first show (§3.6 #5) — not reachable |
| D-UM-17 `60 minutes ago` | keep (parity; vectors in ui-main.md §13) |
| D-UM-18 visible vs spoken time | keep (parity) |
| D-UM-19 two "active day" definitions | `Days active` = reads or listening; chart summary = listening (kept, labelled distinctly) |
| D-UM-20 Refresh balance feedback | show a spinner on the button while a refresh is in flight: `UiUpdate::Credits(CreditsUpdate{view, refreshing})` is published when a refresh starts and when the latest one settles, even with unchanged numbers → `CreditsData.refreshing` |
| D-UM-21 unreachable onboarding message / lost warning | keep the warning while the key step is revisited in the same onboarding run |
| D-UM-22 search clear race | the History controller cancels the debounce on clear (fixed) |
| D-UM-23 Ctrl+1..5 while typing | ignored while a text field has focus (fixed) |
| D-UM-24 Recent `Failed` vs History `Failed` | keep (parity) |
| D-UM-25 `Plan` badge differences | kept per surface: `ui::convert::credits_data(.., CreditsSurface, ..)` with `tier_label` on Analytics, `title_case_words` on Home, `voices::title_case` on Settings Account, and the per-surface copy table (convert.rs docs); `CreditsData.title` for Home's titles; `demo` state only on Settings Account |
| D-UM-26 favourite failures evict Undo | favourite failure toasts use id `history-favorite` (dedupe); `FavoriteToggled{req, Result<Option<ReadRecord>, String>}` carries the failure so the controller can revert |
| D-UM-27 Analytics retry progress | show the spinner on retry (`AnalyticsData.retrying`; Home's inline error the same with `HomeData.retrying`) |
| D-UM-28 top bar (§2.5, §12 #14) | **parity**: the header keeps "Send to tray" (`header-send-to-tray` → `Command::HideMain`, the placement is saved first — with close-to-tray off it is the only way to the tray) and the theme toggle (`header-toggle-theme` → `UpdateSettings({app:{theme: dark ? "paper" : "night"}})`, label/icon from `theme-dark`), both hidden during onboarding |
| D-UM-29 rail threshold | the sidebar rail starts below **1100** DIP (spec `RAIL_QUERY max-width: 1099px`); DESIGN.md's 900 px assumed a smaller minimum window — with the 940 DIP minimum a 900 px threshold could never trigger |
| D-UM-30 accessibility | every spoken string of ui-main.md (§3.5, §3.8, §3.9, §5.6–§5.11, §5.14, §6.1, §1.8, §2.7, §3.4) has a Rust-filled field (`*-accessible`, `accessible`, `*-summary`, `*-spoken`, `title-tooltip`); the keyboard-selected chart mark is announced through a hidden Text with `accessible-live-region: polite` bound to its `accessible` (as `ui/design/toast.slint` does). The hidden data tables are dropped: each chart's summary plus per-mark labels expose the same data |
| D-UM-31 Home seek keyboard | parity: `home.slint` wraps the design ProgressLine in a FocusScope whose `capture-key-pressed` handles Right/Up +0.02, Left/Down −0.02, PageUp +0.1, PageDown −0.1, Home 0, End 1 (each sent at once), before the ProgressLine's own ±5 % |
| D-UM-32 undo batch at shutdown | the UI commits a pending batch before every `Command::Quit` it sends (tray Quit, main close with close-to-tray off, shots end), so its `RemoveReads` precede `Quit`. Log-off and core-originated quits (system close, overlay close) drop it — the reads reappear on the next start (rare; accepted rather than adding a UI round trip inside `session_end`) |
| D-UM-33 replay buttons | `home-replay-last` is separate from the Recent rows' `replay(id)` (own spinners); the main transport button always sends `toggle` and Rust maps ended/error-with-read to `Command::Replay` |
| D-UM-34 sidebar search / Ctrl+K / Ctrl+F | Ctrl+K focuses the sidebar search; Enter navigates to History with `NavIntent::Search` (applied at once) and clears the sidebar field; Ctrl+F / `/` focus the History field — all Slint-local focus nonces in `MainWindow` (no Rust round trip; the gap's `history-focus-search()` callback was not needed) |
| D-UM-35 header actions | `MainWindow` renders the page actions (History count/Export/Clear all; Analytics eyebrow/range) from the page data, so the header layout has one owner (P-UI-SHELL); the page packages own the data fields and handle the forwarded callbacks |
| D-UM-36 dialogs and navigation | a page change closes a dialog owned by the leaving page (a busy request still completes and its controller still handles the answer); while a dialog is open, Ctrl+1..5, Ctrl+K and the sidebar nav are ignored |
| D-UM-37 Analytics plot width | `analytics-daily-width(float)` reports the plot width; Rust buckets with `max_columns = max(1, floor(w / 6))` and re-buckets from the cached summary on change; Slint thins the 24 heatmap hour labels from its pitch |

### 8.11 ui-settings-widget.md §11

| Id | Decision |
|---|---|
| D-USW-1 two API-key forms | keep both flows (different contexts) but share validation, error cleaning and copy where equal |
| D-USW-2 two voice pickers | onboarding keeps API order and compact rows; both use `vox_core::voices` tags and the same preview engine (pauses reading) |
| D-USW-3 demo counts as "can read" | demo counts in both Try-it and Voice steps |
| D-USW-4 apostrophes | `’` everywhere (D-COPY-1) |
| D-USW-5 About → Electron row | replaced by runtime + build hash rows (R-Copy) |
| D-USW-6 media keys restart copy | removed (R-Media-keys) |
| D-USW-7 hook-unavailable notice | kept, driven by `SetWindowsHookExW` failures, retried (R-Hook) |
| D-USW-8 idle card | kept as a fallback (shown only if the widget is shown with no session and no toast) |
| D-USW-9 caption colours | native DWM caption = backdrop colour (no overlay buttons) |
| D-USW-10 "Saving" badge | kept: "Saving" while any of the controller's own `UpdateSettings{req}` is pending, "Saved automatically" when every one has its `SettingsSaved{req}` (unrelated `Settings` broadcasts never settle the overlay) |
| D-USW-11 toast on both surfaces when app visible but unfocused | kept (the widget is the fallback surface) |
| D-USW-12 retention count by binary search | direct `CountExpiring` |
| D-USW-13 recorder hint while modifiers held | shown (fixed) |
| D-USW-14 error text cleaning | `clean_error` everywhere |
| D-USW-15 cache slider above 5000 | D-C-2 |
| D-USW-16 shared library busy flag | separate busy flags per action (`clear-cache-busy`, `export-busy`, `clear-history-busy`) |
| D-USW-17 recorder auto-repeat | ignore auto-repeated keydowns while idle (fixed) |
| D-USW-18 30 s safety net while recording | re-arm the suspension every 20 s while the recorder records **and the main window is focused and visible**; recording stops when the window loses focus, is hidden or minimized, and on `UiUpdate::HotkeysResumed` (the safety timer fired) |
| D-USW-19 onboarding Get a key | toast failures (`OpenExternal{req}` → `Done{req, Err}`) and disable Cancel while connecting (consistent with Settings) |
| D-USW-20 widget/chip theme | the floating windows are **always Night** (`Theme.dark = true`, spec §1.9); `UiUpdate::Theme` applies to the main window only. Appearance copy rewritten (R-Copy): intro `The theme changes this window. The floating player and the chip always stay dark.`, tile notes `Follows Windows dark mode` / `Light window` / `Dark window` (`vox_core::copy::{APPEARANCE_INTRO, THEME_NOTES}`, `AppearanceData.{intro, theme-notes}`) |
| D-USW-21 compact pill content | spec §6.5: art dot (DESIGN.md) on the left, play/pause, then the compact toast / ended-early note / `Synthesizing` loader / small progress bar, time, close — no sentence excerpt (DESIGN.md's excerpt would hide the progress and the notes the spec puts there) |
| D-USW-22 accessible labels of Segmented/Select | **rejected** as a design change: `ui/design` is frozen and its `Segmented`/`Select` take `[string]` models only, so the visible labels serve as the accessible names (`1.25×` reads as "1.25 times" with most screen readers); `ChoiceItem.accessible-label` is still filled for components that can use it |
| D-USW-23 catalog keyed fetch | the core's key generation stamps every catalog job and drops older results; `UiUpdate::{Voices, Models}{refresh, result}` |
| D-USW-24 sample-preview and link failures | `PreviewUpdate::Failed` → UI-local toast `voice-preview` (never a core toast that would pop the widget); `OpenExternal{url, req}` answered with `Done{req}` — Account and onboarding toast `open-external`, About ignores failures (spec §8 "no toast") |
| D-USW-25 slider labels | `settings-format-number(key, value)` (pure callback) formats a slider's label live while dragging |

### 8.12 Text clean-up fixes (R-Text-cleanup)

| Id | Fix (details in `vox_text::clean` docs) | Golden row now |
|---|---|---|
| D-TXT-1 | Decode HTML entities before step 2 — only `;`-terminated references (`&name;` from the integrations.md §5.5 table, `&#N;`, `&#xH;`); semicolon-less legacy forms stay literal (they mostly occur in pasted URL query strings: decoding `&lt=2` would end `URL_RE` early) | `"foo&#160;bar"` → `"foo bar"`; `"see https://x.com/?a=1&lt=2 ok"` → `"see ok"`; `"&#65 B"` unchanged |
| D-TXT-2 | Keep the wrap hyphen when both halves are dictionary words and the joined word is not | `"well-\nknown fact"` → `"well-known fact"` |
| D-TXT-3 | `mailto:` case-insensitive | `"MAILTO:foo@bar.com"` → `""` |
| D-TXT-4 | Autolink `<…>` brackets around removed URLs/e-mails collapse | `"See <https://x.com> now"` → `"See now"` |

### 8.13 Hotkeys

| Id | Decision |
|---|---|
| D-HK-1 | One split/canonical function (parts trimmed) for core, UI and registration |
| D-HK-2 | `AltGr` token = Ctrl+Alt (canonical key, registration, AltGr guard); keycap shows `AltGr` |
| D-HK-3 | `MOD_NOREPEAT` |
| D-HK-4 | Recorder reads VKs from winit scan codes (`vk_from_scan_code`) and probes AltGr on the active layout; registration probes every installed layout |
| D-HK-5 | Numpad keys are mapped from winit `KeyCode::Numpad0..9 / NumpadDecimal / NumpadAdd…` to `VK_NUMPAD0..9 / VK_DECIMAL / VK_ADD…` directly (scan-code mapping ignores NumLock and would record Insert/End…); failures come back from `HotkeyService::register` sorted in registration order (`vox_core::hotkey::sort_failures`) and are logged with `failures_log_line` |

### 8.14 Selection

| Id | Decision |
|---|---|
| D-SEL-1 | Hook failures retried on trigger-setting changes; `HookStatus` drives the notice |
| D-SEL-2 | UIPI refusal before any key is sent, detected by `foreground::input_blocked_by_uipi(hwnd, pid)`: High/System self → no; libuiohook's `PostMessageW` probe (`ERROR_ACCESS_DENIED` → blocked); only if the post fails otherwise, compare token integrity (an `OpenProcessToken` access-denied counts as blocked; an `OpenProcess` failure never does — `PROCESS_QUERY_LIMITED_INFORMATION` succeeds on an elevated process of the same user, and protected or other-user windows are not UIPI-blocked). Capture order: `flushRestore` → foreground → password manager → copy method → **copy-on-select terminals skip the probe** (they inject nothing) → probe for chord methods only |
| D-SEL-3 | Injected keys tagged with Vox's `dwExtraInfo` and dropped by the hook (plus the parity rule of ignoring keydowns during capture) |
| D-SEL-4 | Physical coordinates end to end |
| D-SEL-5 | The spec's "no clipboard sequence number" fallback (§8.3 steps 12–13, §8.6 `staleText`, the cleared/stale branches of §8.7) is **not ported**: native Win32 always has sequence numbers; `0` (no window-station access) is logged once as `clipboard sequence unavailable` and the capture reports no selection |
| D-SEL-6 | Two selection threads: the consumer (machine, flags, timer, chip decisions, dedupe, hook watchdog) and one serial clipboard worker (captures, restores, late-copy watch, flush); every service method only enqueues except `flush` |
| D-SEL-7 | Modifier flags: mouse presses use a `GetAsyncKeyState` snapshot (immune to lost key-ups); key events a mask tracked from side-specific VKs, zeroed after Ctrl+Alt+Del and re-seeded on foreground change, after every capture and at hook start (no `SetWinEventHook`) |
| D-SEL-8 | Our own restores are exempt from the privacy scan (`last_own_write`: while the sequence number is unchanged, `privacy()` returns the restored snapshot's original privacy), so copy-on-select after a capture does not refuse the user's own clipboard as a "password" |
| D-SEL-9 | The hook thread runs at `THREAD_PRIORITY_TIME_CRITICAL`; the consumer re-installs a hook that Windows removed silently (no event for 10 s while `GetLastInputInfo` advances) |

### 8.15 Revision 2: review gaps and their resolution

Nine reviews (one per spec) checked revision 1 of this document and the skeleton. Every gap was applied unless the
row says otherwise; where two reviews proposed different fixes for the same problem, the chosen one is named.

| Spec | Gap (short) | Resolution |
|---|---|---|
| contracts, tts, reader-playback | `Stopped{error}` raced the player's `ended` (partial read stored as completed) | D-NET-5 order `error → Stopped → done` under the engine lock (chosen over `Stopped → error → done`, which would show the player's `done` before its warning); + warning ⇒ partial in `Sessions::on_state(&PlayerSnapshot)`; tests §9 |
| contracts, reader-playback | tray/hotkey/bridge/Home Stop left the widget showing | D-ARCH-8 (core publishes `Widget(Hide)` when a session existed; `Media(Stop)`'s separate hide removed) |
| contracts | typed commands unvalidated | D-ARCH-7 `sanitize_player_command` (implemented + table tests) |
| contracts, persistence | cache deletes / save / load / clear on the core thread; no command for the save result; ClearCache mutating history from a worker | D-P-17 (I/O worker, `save_async`/`load_async`, `Command::{CacheSaved, ReplayLoaded}`, ClearCache on the core thread); `Command::CacheCleared` **not added** (unnecessary with the index-only clear) |
| contracts, persistence | orphan removal could delete a fresh cold-start read | `remove_orphans` candidate snapshot at call time, run on the core thread before queued commands (D-P-17) |
| contracts, reader-playback, shell | OS notifications silently dropped (no registered AUMID) | D-SH-19 `register_app_user_model` + silent ToastGeneric, log once |
| contracts, shell, integrations | no exe icon / VERSIONINFO / manifest | D-SH-20 (assets generated by an `#[ignore]` vox-app test, not a `tools/` script — `tools/` is frozen); D-INT-7 icon `"<exe>",0` |
| contracts | hook notice shown when nothing failed | `hook_status: Option<bool>` + `vox_core::app::native_hook_available` (implemented + test) |
| contracts | `f32` slider noise | `settings::quantize` (implemented + tests), D-ARCH-7 |
| contracts | no `toISOString` helper | `time::iso_utc_ms` (implemented + tests) |
| contracts | demo tables not pinned | §9 vox-store row |
| tts | stop delayed all known billing to the worker's exit; quit lost it | D-NET-4 synchronous `Stopped` + shutdown drain of `Engine` commands |
| tts | check-then-emit race (stale `Failed`) | D-T-7 engine lock |
| tts | ureq defaults stricter than Node | D-NET-7 + `json::parse_lenient_with` (test vector in vox-core) |
| tts | proxy read once; dead proxy breaks everything | D-NET-1 `set_proxy`, re-reads, one direct retry |
| tts | billing float vs `u64` | D-NET-6 (`f64` in the events) |
| tts | D-TXT-1 broke URLs with `&lt=` | D-TXT-1 `;`-terminated only |
| tts | missing verified vectors | §9 vox-tts / vox-text rows |
| reader-playback | stale session after an explicit stop (SMTC, widget view) | D-RP-11 sessionless `State`; `media_state` needs status ≠ idle |
| reader-playback | `--read-file` failure had no way to toast | `Command::Toast` (chosen over `ReadPrepared(Result)`: reusable by any worker) |
| reader-playback | test plan put reader/core items under vox-audio | §9 split |
| reader-playback | CycleRate double click lost a step | synchronous snapshot targets in the player (§5.2) |
| reader-playback, ui-settings-widget | no card-enter key | `WidgetData.card-key` |
| reader-playback, ui-settings-widget | reduce motion unreachable from Slint | `Motion` global + `SystemEvent::MotionChanged` + `UiUpdate::Motion` |
| reader-playback, ui-settings-widget | progress bar / red dot accessible strings | `WidgetData.{progress-accessible, warning-dot-tooltip}`, `format::progress_accessible` (implemented) |
| reader-playback | NoDevice vs failure budget contradiction | D-RP-9 |
| reader-playback | preload only after the UI is shown | D-RP-13 `TimerId::PreloadOutput` |
| reader-playback | startup rate/volume never applied | D-RP-13 (reaction #12 at startup) |
| reader-playback | replay `load` on the core thread | D-RP-14 / D-P-17 |
| reader-playback | listening clock vs extrapolation; latency | D-RP-12 |
| reader-playback | 10 ms tick while idle | D-RP-12 / §5.1 |
| reader-playback | `persist_all` had no snapshot | `persist_all(now, current)` |
| reader-playback | `play_cached` sentences unspecified | engine docs |
| selection | restore jobs on the gesture thread | D-SEL-6 clipboard worker |
| selection | UIPI detection missed the ruling's case / refused other-user windows | D-SEL-2 `input_blocked_by_uipi(hwnd, pid)` |
| selection | §4.2 showed the selection chip for keyboard selections | §4.2 rewritten |
| selection | modifier flags unowned / stuck Shift | D-SEL-7 |
| selection | first chip shown before overlay styles existed | D-SH-3 (attributes hook + pre-created overlays + `overlays_ready`) |
| selection | capture pipeline untestable | private `CaptureEnv` seam + §9 vectors + P-WIN-SEL DoD |
| selection | copy-on-select refused under UIPI; `flushRestore` missing | D-SEL-2 order |
| selection | own restores looked like secrets | D-SEL-8 |
| selection | no-sequence fallback unreachable | D-SEL-5 (option a: Gone) |
| selection | hook thread priority / silent removal | D-SEL-9 |
| selection | stale chip replayed after attach | `UiSink` drops chip events while detached |
| selection | `read_selection` blocking unspecified | non-blocking docs on every service method |
| selection, shell | layered click-through invisible / reset by winit | D-SH-2 (`set_cursor_hittest` + subclass `SetLayeredWindowAttributes`; fallback: hide at once) |
| selection | `ReadClipboard` on the core thread | `ClipboardText` worker (D-ARCH-5) |
| persistence | demo word counts | D-P-14 (implemented test vectors 81/123) |
| persistence | manifest numbers | D-P-15 |
| persistence | log-off flush behind a busy core | D-P-20 |
| persistence | wrong-shape history wipes data | D-P-16 |
| persistence | listener reentrancy | D-P-18 |
| persistence, ui-main, ui-settings-widget, shell | no startup data path to the UI (range, placement, theme, settings) | D-ARCH-9 `UiInit` (one struct for the three proposals `UiStartup`/`UiInit`/`InitialUi`) |
| persistence | sync write and the per-path lock | fsutil docs |
| persistence | retry schedule untestable | D-P-21 |
| persistence | Unicode `\b` in `redact` | ASCII boundaries (log docs) |
| persistence | `all` window unbounded | D-AN-2 + `MAX_ALL_DAYS` |
| persistence | DPAPI failure resurrects an old key | D-P-19 |
| persistence | §12 vectors / timer rules missing | §9 |
| shell | winit undoes overlay styles; first show activates | D-SH-2, D-SH-3 |
| shell | first frames with wrong theme / jumping window | D-ARCH-9, attributes hook for the main window |
| shell | close requests contradictory | D-SH-21 |
| shell | forwarded launches lost during a slow start | D-SH-23 |
| shell | pipe crosses users | D-SH-5 |
| shell | unbounded shutdown steps | D-SH-22 |
| shell | tray keyboard select impossible with tray-icon | D-SH-18 (`NotifyIcon` + muda; deviation from BRIEF's crate list) |
| shell | widget/chip resized top-left first | windows no longer bind their size; one `set_bounds` |
| shell | shots harness could not run | D-SH-17 (`shots::start`, runner `scripts/shots.ps1` because `tools/` is frozen) |
| shell, integrations | startup toasts popped the widget | `ToastRouter.ui_ready` deferral (chosen over passing `initial_surfaces` to `Core::spawn`: robust to any startup timing) |
| shell | tray Open through the core lost the foreground permission | D-SH-18 (UI-local) |
| shell | muda accelerator hints | D-SH-18 (tab text) |
| shell | tray icon size per DPI | D-SH-18 + `UiUpdate::DisplaysChanged` |
| shell | hidden window placed across DPI | `windowing` DPI hop + `ScaleFactorChanged` handling |
| shell | window state saved after the core stopped reading | D-SH-22 (the UI saves on `Quit`) |
| shell, integrations | `env::args()` panics on lone surrogates | D-SH-5 / `Runtime::argv` docs (`args_os` lossy) |
| shell | failure order / reason ids | D-HK-5 (`sort_failures`, `HotkeyFailureReason::id`, implemented + test) |
| shell | pure functions for suspension, retention, CLI, blank test, geometry | `HotkeyState`, `retention::Schedule`, `vox_core::cli::cli_outcome` (implemented + test), `shots::looks_blank`, geometry tests (implemented) |
| integrations | tiny_http cannot implement §1.7 | D-INT-16 |
| integrations | `SO_EXCLUSIVEADDRUSE` impossible / rebind failures | D-INT-16 (windows WinSock feature, abortive closes) |
| integrations | duplicate-header rules | `pipeline::Headers` docs + §9 |
| integrations | 504 not expressible | D-INT-18 |
| integrations | `1e400` read aloud as "Infinity" | D-INT-15 (`OutOfRange::Null`, implemented + test) |
| integrations | re-check window never ended | D-INT-19 |
| integrations | registry tests would hit the real keys | `RegistryLocation` + `*_at` + `run_command_matches` |
| integrations | test plan holes, ReadResult key order | §9 (`/read` bodies compared as parsed JSON) |
| integrations | uninstaller | D-INT-17 |
| integrations | launch-at-login copy | D-INT-3 exact strings |
| ui-main | no controller interface (blocker) | D-ARCH-6 (§7.3) |
| ui-main | `ToastRemoval::Replaced` would commit Undo | removed; same-id replacement is not a removal |
| ui-main | toast UI-local actions | `UiToastAction::Page` |
| ui-main | chart width unknown to Rust | D-UM-37 |
| ui-main, ui-settings-widget | one credits copy for every surface | D-UM-25 `CreditsSurface`, `CreditsData.title` |
| ui-main | Refresh balance spinner not expressible | D-UM-20 `CreditsUpdate.refreshing` |
| ui-main | top bar actions homeless | D-UM-28 (option a, parity) |
| ui-main | panicking request never answered | D-ARCH-3 (`failure_update`, implemented + test) |
| ui-main | rail could never occur | D-UM-29 |
| ui-main | spoken strings missing | D-UM-30 |
| ui-main | retry spinners | D-UM-27 `retrying` |
| ui-main | Analytics debounced range clicks | D-UM-5 |
| ui-main | Analytics range never delivered | `UiInit::analytics_range`; not migrated from localStorage (§6.6) |
| ui-main | favourite failures inexpressible | D-UM-26 |
| ui-main | copy / link failure toasts | `CopyText` → `Done` only; `copy::open_link_failed` (implemented) |
| ui-main | replay-last vs Recent; main button mapping | D-UM-33 |
| ui-main | seek keyboard | D-UM-31 (option b, parity via `capture-key-pressed`) |
| ui-main, ui-settings-widget | toast exit animation; replay of the enter animation | `ToastData.{leaving, key}` (a replacement is not a removal — the reviews' "Replaced removes at once" is superseded by the ui-main rule) |
| ui-main | X1/X2 over controls | native dispatcher; `go-back`/`go-forward` removed |
| ui-main | sidebar search / Ctrl+K / Ctrl+F | D-UM-34 |
| ui-main | header actions split | D-UM-35 |
| ui-main | nothing-yet sentence | `HistoryData.state-body-after` |
| ui-main | trim in Slint | `HomeData.quick-read-can-read`; onboarding sends the raw draft |
| ui-main | undo batch at shutdown | D-UM-32 (partially adopted: log-off / core-originated quits drop it) |
| ui-main | dialog stays open over another page | D-UM-36 |
| ui-main | export dialog had no owner | `ExportHistory{owner}` |
| ui-main | credits wake ambiguity | core-only wake (`WakeThrottle`); `RefreshCredits` sender list |
| ui-main | §13 vectors missing | added to vox-core tests (implemented) + §9 |
| ui-settings-widget | text fields unreachable from Rust (blocker) | §7.1 text-field rule, in-out drafts on `MainWindow`; `IntegrationsData.port-text` and `VoicesData.search` removed |
| ui-settings-widget | stateful controls ignore Rust | §7.1 rule + `revision` fields |
| ui-settings-widget | overlay could not match its own patches | `UpdateSettings{req}` + `SettingsSaved{req}` (D-ARCH-4, D-USW-10) |
| ui-settings-widget | scroll spy unowned | Slint-side exception (`settings.slint` header) |
| ui-settings-widget | widget/chip themed with Paper | D-USW-20 |
| ui-settings-widget | first paint with the wrong theme | D-ARCH-9 |
| ui-settings-widget | recorder kept hotkeys suspended after blur | D-USW-18 |
| ui-settings-widget | safety-timer stop had no trigger | `UiUpdate::HotkeysResumed` |
| ui-settings-widget | numpad keys unrecordable | D-HK-5 (vectors implemented) |
| ui-settings-widget | slider labels while dragging | D-USW-25 |
| ui-settings-widget | Library clear-history dialog id | `library-clear-history` |
| ui-settings-widget | stale catalog results across key changes | D-USW-23 |
| ui-settings-widget | sample / link failures as core toasts | D-USW-24 |
| ui-settings-widget | retention count failure inexpressible | `ExpiringCount{Result}` |
| ui-settings-widget | sample volume not live | reaction #12 extended |
| ui-settings-widget | Settings/AppInfo vs ApiKey order | ordering guarantee (§3.3) |
| ui-settings-widget | one winit event filter per window | single dispatcher in `windowing::main_window::on_created` |
| ui-settings-widget | controller vectors | pure `Overlay`, `RetentionGuard`, `PortField`, `HotkeyRows`, `ToastSlot`, `AutoHide`, `press_gate`; split vectors implemented |
| ui-settings-widget | compact pill content conflict | D-USW-21 |
| ui-settings-widget | accessible labels for Segmented/Select | **rejected** (D-USW-22): `ui/design` is frozen; visible labels serve |

---

## 9. Test plan

General: every crate's pure logic is unit-tested in-crate (`#[cfg(test)]`); spec "verified" tables become table-driven
tests verbatim (inputs and expected outputs copied from the spec, with the D-* changes applied); Windows-only
integration tests that need an interactive desktop are `#[ignore]` and run manually (`cargo test -- --ignored`).
CI: `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, the shots harness in demo mode.

| Crate | Unit tests (spec vectors to carry over) | Integration / manual |
|---|---|---|
| vox-core (done, 87 tests) | settings sanitize/merge/stored form/`needs_rewrite`, `quantize` (0.8f32/0.01 → 0.8, 0.65f32/0.05 → 0.65, cacheLimitMB step 50); record sanitize & patches; word counts incl. the demo snippets (Council 81 whitespace / 82 Unicode, Tenancy 123 / 127); ledger (contracts.md §6.2 worked example); hotkeys (canonical, split vectors `Ctrl+++` / `+` / `Ctrl+`, reserved, plan, `sort_failures` with an OS refusal ahead of a duplicate, reason ids, failure copy, AltGr, labels, recorder incl. numpad VK_NUMPAD7 / VK_DECIMAL); geometry (incl. context-chip passes 2 and 3, selection flip, centre-anchored resize, `anchor_for` quadrants); formatting incl. ui-main.md §13 vectors (`60 minutes ago`, `24 hours ago`, `in 2 minutes`, compact `1M`/`123.5K`/`2.5B`/`999.6`, `-3:20`, `3:19`, `1.33×`, `1×`, `3`, `1M`, `1 minute`, `Yesterday`) and `progress_accessible`; a day-first/24 h locale; JSON (BOM, lone surrogates, out-of-range numbers in both `OutOfRange` modes, an ElevenLabs body with `"\ud800"` and `1e400`, JS numbers); routes (`a%2Fb`, `?x=1`, `#/History` → home); player command parsers and `sanitize_player_command` (same vectors as the lenient parser); `cli_outcome`; `native_hook_available` (None / true / false); `iso_utc_ms` (§3.9 and the log-line example); copy (`open_link_failed`) | Keep adding vectors when a consumer finds a gap (owner of vox-core = architect). Obsolete under DESIGN.md, not ported: ui-main.md §13 `columnGeometry`, heat geometry, `dotLayout`, DotMeter cells and monogram vectors |
| vox-text | tts.md §3.6 both tables (with D-TXT-1..4 rows changed, plus `"see https://x.com/?a=1&lt=2 ok"` → `"see ok"`, `"&#65 B"` unchanged, `"foo&#160;bar"` → `"foo bar"`) and every §3.2/§3.3/§3.4/§3.5 example; §3.7 sentence table; §3.8 chunk examples + property: `text[start..end] == chunk.text` (UTF-16) for random inputs, first ≤ 220, others ≤ 900, never split a pair; reader-playback.md §2.4 `preCut`/`truncateText`/`previewTitle` cases; tts.md §4.5 contexts (`"a"×250+" "+"b"×100` → previous_text `"a"×199+" "+"b"×100`; contextAfter(`"c"×100+" "+"d"×250`) → its first 300 units; ≤ 300 units pass through; surrogate-safe cuts); §5 `sentenceStarts`/`softSplits`/`wordSpans`; `sentence_view` (spoken/current/upcoming, windowing, not started → all upcoming); integrations.md §5.10 golden vectors with D-INT-4: `{\rtf1\ansi\ansicpg932 \'82\'a0 kana}` ⇒ `あ kana`, `{\rtf1\ansi\ansicpg936 \'c4\'e3}` ⇒ `你`, `{\rtf1\ansi\ansicpg950 \'a7\'41}` ⇒ `你`; truncation `@0` unchanged and `@2` → `ab` (the fractional, NaN and negative vectors are N/A: the caller passes `maxChars + 1000` as a `usize`); `looks_binary`; a no-panic fuzz loop over random UTF-8/UTF-16-edge strings | — |
| vox-store | persistence.md §12 verbatim: mulberry32; `generate_demo_records` — the five special records (ids, createdAt, source, sourceApp, error, listened/audio/billed), records [0] `7c134be0-…` (createdAt 1792229746902), [1], [2] and the oldest `94b96364-…`, counts 120 reads / 3 replays / 2 errors / 14 favorites / 99 completed; `compute_analytics` 30d/all/7d totals (words 9171/15235/2036), avgWordsPerRead 129/127, avgListenWpm 166, readingTimeEquivalentSec 2117/3491, `daily.length` 30/60/7, `daily[0]` 2026-09-18, first day 2026-08-19, the 7d per-day triples, heatmap row sums, top lists, sources, streaks — computed with `Utc`; `all` capped at `MAX_ALL_DAYS` (records at createdAt 0 and −1e14); demo tables (contracts.md §7.3): `demo_voices()` = the 8 rows in order (voiceId incl. Liam `TX3LPaxmHKxFdv7VOQHJ`, name, category `premade`, description, labels {accent, age, gender, use_case}), `demo_models()` = the 4 rows (modelId, name, description, maxChars 40000/40000/10000/5000, costFactor 0.5/0.5/1/1, languages `[{id:"en", name:"English"}]`), `demo_account(now)` = {tier `creator`, characterCount 61240, characterLimit 100000, nextResetUnix = now_s + 12×86400, status `active`}; fsutil with `tempfile` (atomic write + `.bak` mirror, copy fallback, busy retry via an injected op, `.corrupt` + `.bak` load, missing → defaults without `.bak`, BOM, empty file, `read_json_file` on `null`/`[]`/`42` → `Ok` without `.corrupt`); `JsonDoc` debounce/max-wait/flush/flush_sync ordering and the retry schedule with ms `retry_base` against an unwritable target (exactly 6 attempts; a later `schedule()` makes exactly one more); settings store (migration rewrite, corrupt+bak immediate rewrite, no-op update, listener order, a listener calling `get()` and `update()`, key status, DPAPI-failure tombstone); secrets (`wrapped_key_from_local_state`, `decrypt_v10` with a test vector built from `aes-gcm`, tombstone, migration precedence; DPAPI round trip `#[cfg(windows)]`); history store (update equality no-op, insertion ties, re-insert on `createdAt` change, retention vs favourites, `on_removed` synchronous and re-entrant (`with_all` inside), `.corrupt` deletion, wrong shape `{}` + good `.bak` → restored and `history.json.corrupt` exists, list/search semantics, `count_expiring`, export JSON); audio cache (`parse_manifest` rules incl. D-P-15 normalisation of `1.5`, `-3`, a `null` start, a numeric char, and rejection of a mismatched `readId`, empty `chunks`, a bad `file`, a non-string `mime`; save/load/touch through the I/O worker; FIFO per id (remove after a queued save leaves no entry); redact; prune LRU + protect; `remove_orphans` snapshot (an id saved after the call survives); startup repair of bad folders/files; `flush`); log (`redact`: `é`+`sk_`+20 alphanumerics masked, `xsk_…` and `sk_…_x` not masked; `format_line`; rotation with a tiny limit); window/ui state parse | Read a copy of a real Electron profile (settings, history, cache, `secrets.bin`) — manual, never committed |
| vox-tts | tts.md §2.4.4 verified table (all rows), `parse_error_body` cases, `permission_message` examples and quirks, `parse_retry_after` / `int_header` (`"0x10"` → 16, dates → none, `" 1e3 "`, `"Infinity"`, `"3, 3"`, `"0"`), §2.5 mapping (`map_voice` premade split, `sort_voices` incl. `Émile`/`emile`, `map_model` languages, `map_account` non-coercion, `map_alignment`: unequal or zero lengths, non-finite times, `1` → `"1"`, `null` → `"null"`; `mime_for_format`; `mp3_kbps` (`mp3_22050_32` → 32, `mp3_x_0` → None)), lenient base64 examples, `synthesis_body` verified request, `without_rejected_param` + `RejectedParams` (remembered, applied up front), `key_fingerprint`, retry policy flags per constructor (D-NET-2), mock vectors (`"Hi."` → 26 504 bytes / 13 230 frames; `"x"×30`, speed 2 → 1.0 s, 44 144 bytes), `encode_wav`/`read_wav_info` (mock file, odd chunks, truncated data, `encode_wav(-2⁻¹⁶)` → 0), `chunk_duration` rules, `preview_replay`/`clip_alignment` | Against a local `tiny_http` mock server (`ClientConfig.base_url`, dev-dependency): (1) the §2.6 request — method, path `/v1/text-to-speech/a%20b%2Fc/with-timestamps?output_format=mp3_44100_128`, `xi-api-key: KEY` from a stored `"  KEY  "`, accept, content-type, `user-agent: Vox/<v> (Windows)`, body; (2) the 422-language → 400-'stitching' → 200 sequence (3 POSTs), then the next chunk in 1 POST without `language_code` or context (D-NET-3); (3) `/v2/voices` paging (page 1 without token; `"a+b/c= d"` round-trips; an error on page 2 discards page 1; at 25 pages `has_more` is ignored; v1 fallback only for 4xx not in {401, 402, 403, 429} and never with missingPermission; anonymous 401 needs_authorization → no-api-key; anonymous timeout rethrown); (4) models: non-array → bad_response, no key → no_key; (5) account: missing permission → `Ok(None)` cached 60 s; (6) memo: concurrent callers share one request, refresh replaces pending, an older generation never overwrites, failures not cached, TTL from completion, fingerprint change misses; (7) every §2.11 validation row (NBSP / U+FEFF inside the key, a non-object 200, restricted-key and quota warnings, both unauthorized message rules, 5xx → its message); (8) a 200 body with `"\ud800"` and `1e400` still returns the audio; (9) proxy: a dead proxy → one direct retry, logged once. Engine: `session_start` before `start` returns; look-ahead (chunk 2 immediately at 15 chars/s, chunk 3 waits for `progress`); retry-after 3 → waits 3 s, 30 → 10 s, timeout → no retry; retries only for 429/5xx; **`Stopped{error}` observed before the sink's `done`**; stop with billed > 0 emits `Stopped` before `stop()`/`start()` returns; audio arriving after the stop → a second, cumulative `Stopped`; start/stop in a tight loop never yields `Failed`/`Complete` for a read after its `stop()` returned; supersede ordering; billing 1.5 + 1.5 → 3; key cleared mid-read → non-fatal `no-api-key`, `Stopped{error}`, `done`; worker panic → fatal `ENGINE_CRASHED` + `Failed`; sink panic → `Complete` still emitted; fatal chunk-0; empty text; `play_cached` renumbering and `play_cached` with zero chunks → fatal `not-cached` + `Failed`. Live ElevenLabs smoke test `#[ignore]` with `ELEVENLABS_API_KEY` |
| vox-audio | `timeline` (§4.8 formulas incl. estimate refinement, `locate` clamps, `time_at_char` gaps/∞), `clock` (cap by media progress, rate banking, rebase on seek, device-tag media time only), `warmer` state machine (ready/wait/give-up/release/retry-after-failure), WSOLA (rate 1 exact pass-through; output length ≈ input / rate ± 1 %; a 220 Hz tone keeps its frequency ± 1 % at 0.5×/2×), resampler length ratio, decode (mock WAV; a short MP3 fixture with its licence in `tests/fixtures/`) | Player state machine with `null_output` (or an injected clock / output factory): reader-playback.md §12 #2–#5, #7–#14, #21–#28 as scenario tests; Stop → `[State(idle, session), State(idle, None)]`; supersede → `[State(idle, old), State(loading, new)]`; `command(Rate)` visible in `snapshot()` at once; a mid-read `NoDevice` reopen → simulated clock, no failure; gapless join (no silent gap > 1 ms between two chunks in the rendered stream); failure recovery (inject a panic → `State(error)` then `Failed`; 3 `Stream` reopen failures in 10 s → the same); the idle player thread does not tick; manual listening test on speakers + Bluetooth (warm-up, device switch, highlight in sync) |
| vox-win | gestures (fake `GestureDeps`): selection.md §5.8 scenario checklist plus the boundary vectors — drag at exactly 6 px is not a drag and 6.01 px is; multi-click at 4 px / 500 ms inclusive and via `clicks ≥ 2`; hint expiry `>=` vs live-hint `<` at 60 000 ms; `next_deadline`; `wants_mouse_move`; `configure` hides only the disabled mode's chip; a right press inside the chip changes nothing; a context chip replaces a selection chip with no `HideChip`; `reset` emits hide then clear; a blocked mouse gesture clears the hint while a blocked keyboard hint leaves it; extended Ctrl+Insert keeps the hint and non-extended Ctrl+numpad-Ins clears it; AltGr+A and AltGr+C clear it; Ctrl+Shift+A sets select-all; VK 0xB4 is not neutral and 0xE5 is; no hint-set flood on auto-repeat. Click counter with `GetDoubleClickTime` injected incl. the wheel reset. Modifier flags with a fake key-state provider (a stuck Shift is cleared by the next press; Ctrl+Alt+Del zeroes the mask). Activation: 300 ms global dedupe falling back to the last chip mode. Chip hit test: half-open rect, then `own_overlay ?? true`. `classify`: own pid; `''` never blocked unless our own pid; excluded apps; password managers. Capture pipeline over the fake `CaptureEnv`: privacy wins over abort; whitespace-only text ends the poll; the 1.5 s extension only when the sequence moved and a text format is listed; the Escape retry only for the context chip with Ctrl+C and skipped when the sequence moved; a secret is restored regardless of `restoreClipboard` and the clipboard cleared when the snapshot failed; the four lossNotice combinations; `restoreClipboard` read live; a late copy emits `Read` with the original source; `flushRestore` stops after 4 rounds; the echo guard; awaitInFlightCopy; copy-on-select sends no keys and skips the UIPI probe; our own restore is not reported as a secret (D-SEL-8). Snapshot rules: DataObject and Ole Private Data skipped before the budget check; descriptors dropped when something was skipped; failed when nothing was kept and something was skipped. `isPrivateFlagValue`: None, length < 4 and DWORD 0 are private. A partial `SendInput` sends the missing key-ups. `clean_title` trims to 300 units and drops a dangling high surrogate; the ApplicationFrameHost appName comes from the text after the last ` - ` of the title; QUNS 3 and 4 → true, 2 → false. `copy_method_for` table; `friendly_app_name` table, `is_password_manager`; `quote_windows_arg` / `build_verb_command` (integrations.md §3.2 vectors); `run_command_matches` (`"C:\A B\vox.exe"` vs `c:\a b\VOX.EXE` → true; unquoted `C:\A B\vox.exe` → false; `"C:\x\vox.exe" --hidden` vs no args → false, vs `--hidden` → true; `C:\x\vox.exe --hidden` vs `--hidden` → true; `""` → false); `parse_proxy_server`; key classifiers | `#[ignore]` desktop tests: hook start/stop and priority; clipboard snapshot/restore round trip incl. `CF_HDROP` + markers; UIPI against an elevated Notepad (`Some(true)`) and a "Run as different user" Notepad (`Some(false)`); hotkey register/conflict (`ERROR_HOTKEY_ALREADY_REGISTERED`); single-instance forwarding between two processes of one user, and a second user's launch not reaching the first user's primary; Electron detection against a fake `Chrome_MessageWindow`; SMTC flyout; registry install/remove/state under `RegistryLocation` `Software\VoxTest\<uuid>\…` (0 of 9 → Absent; 9 equal ignoring case → Current; 8 of 9 or one different → Stale; `REG_EXPAND_SZ` compared raw), cleaned up afterwards; `NotifyIcon`: Win+B, arrows, Enter → Activate (not the menu); a silent notification appears with the widget off and the main window hidden |
| vox-bridge | pipeline: integrations.md §1.10 wire vectors (every row: OPTIONS, Host, Origin, paths, methods, client header, content type, sizes, JSON errors, `/read` body rules, `/command` messages, response headers) plus every verified example inline in §1.4 notes and §1.6.2/§1.6.3: `/%68ealth` → 404, `/health%2F` → 404, `/HEALTH` → 404, `/health//` → 200, the Content-Type variants, lossy `a\xFF`, `\uFEFF\u00A0` text → empty-text, `FILE:///c:/a.txt` kept, non-string url/title dropped, last duplicate key wins, `direction: 1.0`, `positionSec: 1.2e29`, HTTP/1.0 without Host → 403; duplicate headers (two Hosts → 200 with the first value; `application/json` then `text/plain` → accepted; duplicate `x-vox-client` → 403; `Browser-Extension` → 403; both Origin orders); `1e400` vectors per D-INT-15. `/read` bodies are compared as **parsed JSON** (ReadResult key order follows vox-core) | Socket tests with `BridgeServer::with_limits` (scaled-down timeouts) and a fake `BridgeHandler`: `/health` GET/HEAD; `/read` reply and 504 (handler returns `None`); `/command`; `HELLO` → raw 400; a 20 000-byte header → 431; the 408 stall with `Connection: close`; the keep-alive close after 5 s idle; 64-connection cap; start on a busy port → exact error strings; restart on another port; open a keep-alive connection, `stop()`, `start(same port)` → ok; serve a request, drop the server, bind again within 100 ms → ok |
| vox-app | pure modules: `sessions` (first report writes, status-change writes, 2 s throttle, ledger delta to the current hour, `maxPositionSec` monotonic, partial never completes, an `ended` snapshot with a warning never completes, sessionless reports ignored, 16-session eviction, `persist_all(now, current)`), `reactions::diff` (every row of §4.15), `toasts::route` + `ToastRouter` (deferral until the first `MainState`), `tray_view::build` + `menu_key`, `retention::{reconcile_plan, Schedule}` (interval skipped while a change is pending; the change run re-arms and fires once 60 s after the last change with the then-current value; both cancelled by shutdown), `hotkeys::HotkeyState` (true twice re-arms; false while not suspended does nothing; the timer resumes and re-registers from current settings and publishes `HotkeysResumed`), `integrations::explorer_needs_write`, `media::media_state` (inactive when idle), `credits` (state machine incl. stale-sequence drop, refreshing true → false even with unchanged numbers, reset logic, `WakeThrottle` throttle / passed-reset bypass / shared `forcedReset`), `queries::export_file_name`, `core::{owed_answer, failure_update}` (done), `ui::shell::{RouteHistory, ToastStackState (same-id push → no removal; a 5th push → `PushedOut`; exits keep the toast present), effective_page}`, `ui::settings::{Overlay (failed first update reverts), RetentionGuard (7→30→7 within 450 ms gives one count; back to the stored value writes nothing; a stricter value with count 0 saves silently; a count failure reverts + toast), PortField (`00080` is an error; Escape restores; blur with the stored value writes nothing), HotkeyRows (reserved chord keeps the old one with the 6 s message; a stored reserved chord shows the persistent message)}`, a model switch resets the language in the same write, `ui::widget::{ToastSlot (the revealed chain; a same-id toast restarts its timer), AutoHide (stopped-early never auto-hidden), hides_on_dismiss, view selection}`, `ui::chip::press_gate` (right press dismisses; presses while hiding ignored), `ui::home` Today caption (600 s → `5m to your daily goal`, 899.5 s → `1m to your daily goal`), `ui::history` row and detail audio strings (`~0:20`, `about 0:20`), `count_label` singular (D-UM-13), `ui::convert::credits_data` for every surface × state incl. the §6.1 example (`39%`, `38.8K of 100K left`, `Resets in 12 days`), `windowing::dpi` (done), `main_window::initial_placement`, `app::{should_show_main_at_start, startup_route}`, `shots::looks_blank`; page controllers build structs from fixtures (one test per state: loading/error/empty/list). Reader tests behind a fake engine + `null_output` players + temp stores (reader-playback.md §12 #1, #6, #15–#20, #30): `new_record` against §2.3's table (160-unit char-safe preview; `textTruncated` only when > 160; charCount/wordCount of the full text; empty strings → absent; sourceTitle trimmed to ≤ 200); `toggle_playback` (toggle when status ≠ idle or an engine session exists; replay the newest only if cached && `has`; a load failure re-synthesizes; then the `Nothing is playing` toast); replay (not in history without readId; a hit needs no key and keeps base values; preview-only text rebuilt; stale `cached` cleared; `not-cached` for preview-only); `Complete` (record gone → nothing cached; save error → cached false; prune protects keep + engine read + last readId; a record deleted during the save → entry removed); `Stopped{error}` → partial, `PARTIAL_READ_ERROR`, never completed; billing on supersede (#1); `Failed` toast carries `Open settings` only per `error_needs_settings`; history off still tracks and writes nothing; #19 persist on quit and session end; #30 via `reactions::diff`; Stop with a session → `Widget(Hide)`, Stop while idle → nothing; `sanitize_player_command` applied to SMTC seeks; a panicking request handler is still answered | Integration (mock ElevenLabs server + `null_output` players + temp profile): chunk 1 returns 503 four times while the player has drained chunk 0 → `completed == false`, `error == PARTIAL_READ_ERROR`; quit mid-synthesis persists `charactersBilled`; a fake service whose `stop()` never returns → the quit path still publishes `UiUpdate::Quit` (watchdog armed). Demo-mode smoke: `VOX_DEMO=1 VOX_MOCK_TTS=1` read → play → end → history/analytics updated; `scripts/shots.ps1` (`VOX_SHOTS_DIR` harness) produces every page × theme + widget/chip states with the blank-image check and `report.json`. Manual checklist: no focus steal by widget/chip (incl. the **first chip after a cold start**), chip above context menus, the chip exit animation renders while click-through, no visible jump when the bottom-anchored card grows, drag/remember/DPI moves across a 100 %/150 % monitor pair, close-to-tray, Send to tray, second instance (incl. during a slow start), Explorer verb (Vox icon shown; Startup apps lists "Vox" with the icon), uninstall the Electron build → start vox.exe → verb and Run value restored, quit and relaunch right after an extension `/read` → the bridge listens, tray keyboard select, a core toast with the widget off and the main window hidden appears in Action Center, browser extension against the bridge |

---

## 10. Work breakdown (parallel packages)

Everything below can start now: vox-core, vox-art, `ui/design`, `ui/icons`, `tools/vox-shot` are done, and all
cross-crate signatures, the UI contract and the UI controller interface (§7.3) are final. Rules for every package:
implement only inside your files; do not change a public signature or a `ui/models.slint` struct (ask the architect);
remove `#![allow(dead_code, unused)]` from your crate when done; `cargo test -p <crate>` and `cargo clippy -p <crate> --
-D warnings` green; no `todo!()` left in your files; no key or read text in logs; `tools/` and `ui/design` are frozen.

| Package | Files | Inputs (specs) | Definition of done |
|---|---|---|---|
| **P-TEXT** vox-text | `crates/vox-text/src/*`, `crates/vox-text/data/en-words.txt` | tts.md §3, §4.5, §8; reader-playback.md §2.4, §5, §8.7; integrations.md §5; §5.6 + D-TXT-1..4, D-INT-4 here | All §9 vox-text tests (incl. the `;`-only entity rows and the §4.5 context vectors); no panics in the fuzz loop; offsets UTF-16 |
| **P-STORE** vox-store | `crates/vox-store/src/*` | persistence.md (all), contracts.md §3.6/§3.9/§4/§6/§7.3, §6 + D-P-* + R-API-key + R-Profile here | All §9 vox-store tests incl. persistence.md §12 vectors and the demo tables; the cache I/O worker (FIFO, async save/load, index-only removals, `flush`); listeners after lock release; DPAPI round trip on Windows; real-profile manual read |
| **P-TTS** vox-tts | `crates/vox-tts/src/*` (dev-deps `tiny_http`, `tempfile`) | tts.md §2, §4–§9; reader-playback.md §2.7, §3; D-NET-1..7, D-T-*, D-VO-1 here | §9 vox-tts tests incl. every mock-server test (the §2.6 request, the 3-POST fallback, paging, memo, §2.11 rows, proxy fallback, engine ordering/billing/lock); mock mode works without network. Engine tests that need real chunking run once P-TEXT lands (until then use texts ≤ 220 units) |
| **P-AUDIO** vox-audio | `crates/vox-audio/src/*`, `tests/fixtures/*` | reader-playback.md §4–§6, §8.9, §12; §5 here; R-Audio; D-RP-9..12 | §9 vox-audio tests (incl. the sessionless stop report, synchronous rate/volume targets, NoDevice mid-read, idle thread without ticks); manual listening checklist (gapless, speed 0.75–2× natural pitch, Bluetooth warm-up and highlight sync, device switch) |
| **P-WIN-SEL** vox-win selection side | `vox-win/src/{gestures, hook, selection, clipboard, keys, foreground}.rs` | selection.md (all); contracts.md §1.4 #8, §1.5; R-Clipboard, R-Hook; D-SEL-1..9 | Gesture scenario and boundary vectors; the capture pipeline over the fake `CaptureEnv` (every §9 vox-win pipeline vector); modifier-flag tests; desktop checklist: chip appears beside the menu in Notepad/Chrome/Word, never steals focus; captures restore the clipboard (text, image, files) without history entries; UIPI notice with an elevated Notepad, none with a "Run as different user" Notepad; copy-on-select works with an elevated PuTTY |
| **P-WIN-SYS** vox-win system side | `vox-win/src/{hotkeys, media, notify_icon, registry, single_instance, system, window, shell, error, lib}.rs` | shell.md §3.3, §3.6, §5.5–§5.6, §6.1, §7.3–§7.4, §8; integrations.md §3–§4; reader-playback.md §9; contracts.md §5.3; R-Hotkeys, R-Media-keys, R-Do-not-run-both, R-Tray, D-NET-1, D-FMT-1, D-SH-2/3/5/18/19 | Unit tests (quote, proxy parser, `run_command_matches`); desktop checklist: hotkeys (+AltGr layouts, failure order), SMTC flyout + hardware keys, Explorer verb install/remove/idempotent startup sync under a test `RegistryLocation`, Run value idempotency, per-user second-instance forwarding (incl. a second user), Electron detection, session-end save, overlay subclass keeps no-activate/tool-window/topmost across Slint show/hide, `NotifyIcon` keyboard select, AUMID registration + a silent notification |
| **P-BRIDGE** vox-bridge | `crates/vox-bridge/src/*` | integrations.md §1, §2; contracts.md §1.10; D-INT-1, -11, -15, -16, -18 | All wire vectors (incl. §1.4/§1.6 inline examples and duplicate headers); every socket test of §9 (408/431/400, keep-alive close, 64-cap, immediate rebind after stop and after drop); the unchanged extension works end to end against `vox.exe` (manual) |
| **P-APP-CORE** vox-app core | `vox-app/src/{app.rs, runtime.rs, command.rs, update.rs, core/*}` | reader-playback.md §2, §7 (core side), §9 (core side); shell.md §2–§5, §8.5–§8.8; contracts.md §5–§6; ui-main.md §1.5; persistence.md §5.10, §6.10, §10; §3, §4 here | Pure-module and reader tests of §9; the integration tests of §9 (partial read never completed, billing persisted on quit, bounded quit with a hung service); demo-mode smoke test passes headless (core + players with `null_output`, no UI) |
| **P-APP-WIN** vox-app native shell | `vox-app/src/{windowing/*, tray.rs, ui/mod.rs, ui/art.rs, ui/convert.rs, ui/tray_view.rs, shots.rs, main.rs}`, `build.rs`, `crates/vox-app/assets/*` | shell.md §6, §7, §9; contracts.md §1.3–§1.4; ui-settings-widget.md §6.12, §9; R-DPI, R-Main-window, R-Tray, R-Widget-z-order; D-SH-*, D-DPI-1, D-ARCH-6/9 | `AppUi` per §7.3 (shared state, fan-out, sinks, `UiInit` applied before the first show); backend + attributes hook; overlays pre-created and subclassed before use; windows behave per §4.12/§4.13 on mixed-DPI monitors (100 %/150 % pair, no jump when the bottom-anchored card grows); tray per R-Tray; `vox.ico`/`vox-256.png` generated and embedded with VERSIONINFO + manifest; shots harness writes every page × theme + widget/chip states; 30 Hz snapshot timer with change keys; `credits_data` tests for every surface |
| **P-UI-SHELL** | `ui/windows/main-window.slint` (layout, header row with page actions, root FocusScope), `ui/pages/shell/*.slint`, `vox-app/src/ui/shell.rs` | ui-main.md §1.7, §2; ui-settings-widget.md §1.5–§1.6, §1.9; DESIGN.md "Screens"; D-UM-28/29/34/35/36 | Panel on backdrop, header (title, page actions, Send to tray, theme toggle), sidebar (rail < 1100 px, search with Ctrl+K), toasts (timers, hover pause, 180 ms exits, same-id replacement without removal, UI-local actions), dialog (closes with its page), shortcuts; vox-shot previews (dark/light, 1000 px rail) |
| **P-UI-HOME** | `ui/pages/home.slint`, `vox-app/src/ui/home.rs`, `ui/previews/home*.json` | ui-main.md §1.4–§1.6, §3, §6.1; R-Home; DESIGN.md Home; D-UM-30/31/33 | All `HomeData` states incl. first-load error + Retry (`retrying`) and Resume; seek keys; accessible strings; previews both themes |
| **P-UI-HISTORY** | `ui/pages/history.slint`, `vox-app/src/ui/history.rs`, previews | ui-main.md §4, §11.2–§11.3; ui-settings-widget.md §8 ids; D-UM-26/32/34 | Load/reconcile/load more, delete with undo (commit rule), clear dialog, expanded rows, keyboard (focus nonce), in-out search field, favourite results; previews |
| **P-UI-ANALYTICS** | `ui/pages/analytics.slint`, `vox-app/src/ui/analytics.rs`, previews | ui-main.md §5, §6.1, §11.3; R-Analytics; D-AN-2, D-UM-30/37 | Range from `UiInit`, immediate vs debounced queries, plot-width bucketing, charts from `vox_core::charts`, heatmap label thinning, empty banner rule, accessible live text; previews |
| **P-UI-VOICES** | `ui/pages/voices.slint`, `vox-app/src/ui/voices.rs`, previews | ui-settings-widget.md §4; DESIGN.md Voices; D-USW-23/24 | States, filters (re-asserted), in-out search, preview states and failures, Use voice; previews |
| **P-UI-SETTINGS** | `ui/pages/settings.slint`, `ui/pages/settings/*.slint`, `vox-app/src/ui/{settings, hotkey_recorder}.rs`, previews | ui-settings-widget.md §1–§3, §8; integrations.md §1.9; R-Copy, R-Media-keys, R-Hook, R-API-key; D-USW-*, D-INT-3/19 | Every section and binding (§2.12 table) with stateful controls re-asserted from `data` (flip a toggle, push a reverted struct → the toggle shows the reverted value); optimistic overlay keyed by request; quantized sliders and live labels; scroll spy; retention guard; port field; Library clear-history dialog; recorder (suspension, re-arm only while focused, stop on blur/minimize/`HotkeysResumed`, AltGr, reserved, numpad); key notice; hook notice; About build hash; Appearance copy; previews |
| **P-UI-ONBOARDING** | `ui/pages/onboarding.slint`, `ui/pages/onboarding/steps.slint`, `vox-app/src/ui/onboarding.rs`, previews | ui-settings-widget.md §5; ui-main.md §10; R-API-key | Five steps, key validation flow (§4.11; raw draft trimmed in Rust), voice step with previews, try step, finish on `SettingsSaved`; previews |
| **P-UI-WIDGET-CHIP** | `ui/windows/widget-window.slint`, `ui/windows/chip-window.slint` (layout), `vox-app/src/ui/{widget, chip}.rs`, previews | ui-settings-widget.md §6–§7; reader-playback.md §7–§8; DESIGN.md "Floating widget", "Read with Vox chip"; D-USW-20/21 | All widget views/states (§6.8 matrix) incl. compact pill per D-USW-21, toasts (standalone/banner, hides-on-dismiss), auto-hide holds, highlighting, `card-key` enter animation, reduce motion; chip enter/exit; windows never bind their own size; handles never contain controls; always dark; previews |
| **P-APP-INT** integration | cross-cutting wiring review, `browser-extension/` packaging next to the exe, `scripts/shots.ps1`, CI scripts | BRIEF.md quality bar; §9 smoke and manual checklists | `cargo test --workspace` + clippy clean; demo smoke + shots in CI (`scripts/shots.ps1`: deletes `report.json`, sets `VOX_SHOTS_DIR`/`VOX_DEMO`/`VOX_MOCK_TTS`, kills after 120 s, prints the summary, exits 0/1/2); the manual checklist signed off; idle CPU ≈ 0 and memory well below the Electron build (measured) |

Hand-off order is flexible; the only real sequencing is: P-TTS engine tests want P-TEXT's chunker, P-APP-CORE's
end-to-end smoke wants the libraries, and P-APP-INT comes last. Page packages build and preview their pages with
vox-shot and fixture JSON before the core exists, and unit-test their controllers against vox-core types through the
`PageController` interface with a hand-built `UiCtx`.

---

## Appendix A. Public API reference (generated from the skeleton)

Generated from the source by extracting every `pub` item with its signature and the first sentence of its doc comment (struct fields and enum variants inline; trait members inline). The source doc comments are the full contract; this listing exists so engineers see every cross-crate surface in one place. `impl` blocks list their public methods; trait impls show only their header. vox-art lists only its crate root (internal modules are private). Regenerate after a signature change.


### A.1 `vox-core`

#### `vox_core::analytics`

```rust
pub enum AnalyticsRange { D7, D30, D90, D365, All }
    // Analytics range.
impl AnalyticsRange {
    pub const ALL: [AnalyticsRange; 5] = [Self::D7, Self::D30, Self::D90, Self::D365, Self::All]
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
        // Invalid → `None` (callers default to 30d).
    pub fn days(self) -> Option<u32>
        // Days in the range; `None` for `all`.
    pub fn button(self) -> &'static str
        // Segmented-control button text.
    pub fn label(self) -> &'static str
        // `RANGE_LABELS`.
}
pub struct Totals { listened_sec: f64, reads: u64, words: u64, chars: u64, audio_sec: f64, completed_reads: u64, characters_billed: u64 }
pub struct TodayStats { listened_sec: f64, reads: u64, words: u64 }
pub struct Streak { current: u32, longest: u32 }
pub struct DailyEntry { date: String, listened_sec: f64, reads: u64, words: u64 }
    // One local calendar day.
pub struct TopApp { name: String, reads: u64, listened_sec: f64 }
pub struct TopVoice { voice_id: String, name: String, reads: u64, listened_sec: f64 }
pub struct SourceCount { source: ReadSource, reads: u64 }
pub struct AnalyticsSummary { range: AnalyticsRange, generated_at: f64, totals: Totals, today: TodayStats, streak: Streak, daily: Vec<DailyEntry>, heatmap: [[f64; 24]; 7], top_apps: Vec<TopApp>, top_voices: Vec<TopVoice>, sources: Vec<SourceCount>, avg_words_per_read: u64, avg_listen_wpm: u64, reading_time_equivalent_sec: u64 }
    // The analytics summary for one range (all calendar maths in local time).
impl AnalyticsSummary {
    pub fn empty(range: AnalyticsRange, now_ms: f64) -> Self
        // An all-zero summary (used before the first computation).
}
```

#### `vox_core::app`

```rust
pub enum AppPage { Home, History, Analytics, Voices, Settings, Onboarding }
    // Main-window pages.
impl AppPage {
    pub const NAV: [AppPage; 5] = [AppPage::Home, AppPage::History, AppPage::Analytics, AppPage::Voices, AppPage::Settings]
        // Sidebar order (Ctrl+1..5).
    pub const ALL: [AppPage; 6] = [AppPage::Home, AppPage::History, AppPage::Analytics, AppPage::Voices, AppPage::Settings, AppPage…
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn title(self) -> &'static str
        // Header title / nav label.
}
pub struct AppRoute { page: AppPage, section: Option<String> }
    // A route: page + optional anchor (settings section id).
impl AppRoute {
    pub fn page(page: AppPage) -> Self
    pub fn settings(section: &str) -> Self
        // `settings/<section>`.
    pub fn validated(page: AppPage, section: Option<&str>) -> Self
        // `parseRoute` (main/ipc.ts): section kept only if it matches `^[A-Za-z0-9_-]{1,64}$`.
    pub fn to_hash(&self) -> String
        // `#/<page>` or `#/<page>/<section>` (section percent-encoded like `encodeURIComponent`).
    pub fn parse_hash(hash: &str) -> Self
        // `parseHash` (ui-main.md §2.1): unknown page → home (section dropped); section = remaining non-empty segments joined with `/`, URI-decoded (a malformed escape keeps the raw text).
    pub fn settings_section(&self) -> Option<&str>
        // The settings section to scroll to, if this is a valid settings deep link.
}
pub fn is_valid_section(s: &str) -> bool
    // `^[A-Za-z0-9_-]{1,64}$`.
pub enum ToastKind { Info, Success, Error }
    // Toast kinds.
impl ToastKind {
    pub fn duration_ms(self) -> u64
        // App-window auto-dismiss duration (ms), before the action rule.
}
pub struct ToastAction { label: String, route: Option<AppRoute> }
    // A toast action button.
pub struct Toast { id: String, kind: ToastKind, title: Option<String>, message: String, action: Option<ToastAction> }
    // A toast.
impl Toast {
    pub fn new(kind: ToastKind, message: impl Into<String>) -> Self
        // A toast with a fresh UUID id.
    pub fn info(message: impl Into<String>) -> Self
    pub fn error(message: impl Into<String>) -> Self
    pub fn success(message: impl Into<String>) -> Self
    pub fn with_id(mut self, id: impl Into<String>) -> Self
        // Sets a fixed dedupe id.
    pub fn with_title(mut self, title: impl Into<String>) -> Self
    pub fn with_action(mut self, label: impl Into<String>, route: Option<AppRoute>) -> Self
    pub fn with_settings_action(self, section: &str) -> Self
        // `Open settings` → `settings/<section>`.
    pub fn app_duration_ms(&self) -> u64
        // App-window duration: kind default, at least 6000 ms with an action.
    pub fn widget_duration_ms(&self) -> u64
        // Widget duration: 2500 ms, 5000 ms with an action.
}
pub struct AppInfo { name: String, version: String, runtime: String, build: String, api_key_notice: Option<String>, is_packaged: bool, user_data_path: PathBuf, extension_path: PathBuf, server_port: u16, server_running: bool, native_hook_available: bool, explorer_menu_installed: bool, demo_mode: bool }
    // App facts for Settings → About / Triggers / Integrations / Account (contracts.md §3.8; `electronVersion` replaced by `runtime` + `build`, BRIEF.md "About shows the app version and build hash").
pub fn native_hook_available(hook_status: Option<bool>) -> bool
    // `AppInfo.native_hook_available` from the core's hook status (contracts.md §3.8, §1.5; ARCHITECTURE.md §3.2 `Selection(HookStatus)`): `None` (no install attempt reported yet) and `Some(true)` → available; only `Some(false)` (an attempt fail…
pub enum ChipMode { Context, Selection }
    // Chip modes.
impl ChipMode {
    pub fn label(self) -> &'static str
    pub fn read_source(self) -> crate::history::ReadSource
        // The read source an activation produces.
    pub fn default_size(self) -> (f64, f64)
        // Default window size before the UI reports one (DIP).
}
pub struct RuntimeFlags { demo: bool, mock_tts: bool, shots_dir: Option<PathBuf>, debug: bool }
    // Runtime flags from the environment (contracts.md §1.8, shell.md §1.1).
impl RuntimeFlags {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self
        // Reads the flags through `get` (`std::env::var` in production).
    pub fn shots(&self) -> bool
        // Screenshot-harness mode.
}
```

#### `vox_core::cache`

```rust
pub struct CacheStats { reads: u64, files: u64, bytes: u64 }
    // Totals from the in-memory cache index (no disk scan).
```

#### `vox_core::charts`

```rust
pub enum BucketUnit { Day, Week, Month }
    // Column unit after bucketing.
pub struct ColumnBucket { key: String, start: NaiveDate, end: NaiveDate, days: u32, listened_sec: f64, reads: u64, words: u64, is_today: bool }
    // One chart column.
pub fn bucket_daily(daily: &[DailyEntry], max_columns: usize, today_key: &str) -> (BucketUnit, Vec<ColumnBucket>)
    // Groups daily points into at most `max_columns` columns: days if they fit; else 7-day weeks counted back from the newest day (the newest column always ends on the newest day; the oldest may be partial); else calendar months.
pub fn pick_ticks(n: usize, max_labels: usize) -> Vec<usize>
    // Indices to label on an axis of `n` columns, anchored at the newest column.
pub fn axis_label(b: &ColumnBucket, unit: BucketUnit, total: usize) -> String
    // Axis label: month `JAN` (January shows the 2-digit year: `JAN 26`); day with ≤ 7 columns: `SUN`…; else `SEP 14`.
pub fn bucket_title(b: &ColumnBucket, unit: BucketUnit) -> String
    // Tooltip/table title: `Tue, Sep 30` (day), `Sep 8 – Sep 14` (week; single date for 1 day), `September 2026`.
pub fn bucket_tooltip(b: &ColumnBucket, unit: BucketUnit) -> (String, String, String)
    // Bucket tooltip: `(title, value, sub)` = (`bucket_title`, `42H18M`-style hours, `3 reads · 1,204 words`).
pub fn levels(values: &[f64], steps: u32) -> Vec<u32>
    // `dotLevels`: integer levels 0..=steps; any positive value lights at least 1; the max lights all.
pub fn bar_fractions(values: &[f64]) -> Vec<f64>
    // Continuous 0..1 bar heights relative to the max (0 for non-positive values).
pub fn bar_dots(value: f64, max: f64, count: u32) -> u32
    // `barDots(value, max, count)`.
pub fn active_days(daily: &[DailyEntry]) -> (usize, usize)
    // Days with any activity / all days.
pub fn recent_activity(daily: &[DailyEntry], n: usize) -> Vec<bool>
    // Last `n` days' activity flags, oldest → newest.
pub struct HeatCell { day: usize, hour: usize, value: f64, level: f64 }
    // One heatmap cell (`day` 0 = Monday).
pub fn heat_cells(heatmap: &[[f64; 24]; 7]) -> (Vec<HeatCell>, f64, Option<HeatCell>, f64)
    // Normalised heatmap: `(cells Mon→Sun × 0→23, max, peak (first strictly largest positive), total)`.
pub struct TopItem { key: String, name: String, value: f64, display: String, sub: String }
    // A ranked row for Top apps / Top voices.
pub fn top_items(rows: &[(String, String, u64, f64)]) -> Vec<TopItem>
    // `topItems`: by listening when any row has listening (rows with 0 listening drop out), else by reads; first 6.
```

#### `vox_core::cli`

```rust
pub struct CliArgs { hidden: bool, read_clipboard: bool, read_file: Option<String>, read_text: Option<String> }
    // Parsed switches.
pub enum CliAction { ReadText(String), ReadClipboard, ReadFile(String) }
    // The one action an invocation performs (priority `--read-text` > `--read-clipboard` > `--read-file`).
impl CliArgs {
    pub fn action(&self) -> Option<CliAction>
    pub fn has_read_action(&self) -> bool
}
pub fn cli_outcome(args: &CliArgs, from_second_instance: bool) -> (Option<CliAction>, bool)
    // `handleArgs` (shell.md §2.2): the one action to run, and whether to show the main window — only when nothing is acted on **and** the call came from a second instance **and** `--hidden` was not given (launching Vox again brings the window f…
pub fn parse_cli_args<S: AsRef<str>>(argv: &[S]) -> CliArgs
    // `parseCliArgs` over the full argv (exe path included; non-`--` tokens and unknown switches are ignored).
pub fn resolve_read_file(cwd: &Path, path: &str) -> PathBuf
    // Resolves a `--read-file` path against the invoking process's working directory (`path.resolve(cwd, p)`).
```

#### `vox_core::constants`

```rust
pub const APP_NAME: &str = "Vox"
    // The single source of the product name (contracts.md §2.1).
pub const APP_ID: &str = "com.vox.reader"
    // App User Model ID (toast attribution, taskbar grouping, installer shortcuts).
pub const READ_WITH_LABEL: &str = "Read with Vox"
    // Context chip label and Explorer verb label: `Read with Vox`.
pub const READ_LABEL: &str = "Read"
    // The selection-popup chip label.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION")
    // App version shown in About, `/health` and logs (workspace version).
pub const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io"
pub const ELEVENLABS_DEFAULT_VOICE_ID: &str = "JBFqnCBsd6RMkjVDRZzb"
    // "George", a premade voice.
pub const ELEVENLABS_DEFAULT_VOICE_NAME: &str = "George"
pub const ELEVENLABS_DEFAULT_MODEL_ID: &str = "eleven_flash_v2_5"
pub const ELEVENLABS_API_KEYS_URL: &str = "https://elevenlabs.io/app/settings/api-keys"
    // Where "Get a key" / "Manage keys" lead.
pub const ELEVENLABS_DASHBOARD_URL: &str = "https://elevenlabs.io/app"
pub const ELEVENLABS_BILLING_URL: &str = "https://elevenlabs.io/app/subscription"
pub const ELEVENLABS_DOCS_URL: &str = "https://elevenlabs.io/docs"
pub const SYNTH_TIMEOUT_MS: u64 = 30_000
    // Synthesis timeout = max(SYNTH_TIMEOUT_MS, chars × SYNTH_MS_PER_CHAR) per HTTP request.
pub const SYNTH_MS_PER_CHAR: u64 = 50
pub const LIST_TIMEOUT_MS: u64 = 15_000
    // Voices / models / subscription / key validation timeout (per page).
pub const VOICES_TTL_MS: u64 = 600_000
pub const MODELS_TTL_MS: u64 = 3_600_000
pub const ACCOUNT_TTL_MS: u64 = 60_000
pub const MAX_VOICE_PAGES: usize = 25
pub const VOICE_PAGE_SIZE: usize = 100
pub const ERROR_MESSAGE_CAP: usize = 300
    // Error message cap for non-JSON error bodies and "other HTTP" messages.
pub const MAX_API_KEY_LEN: usize = 256
    // Keys longer than this are rejected before validation (`That doesn't look like an API key`).
pub const FIRST_CHUNK_MAX: usize = 220
pub const CHUNK_MAX: usize = 900
pub const RETRY_DELAYS_MS: [u64; 3] = [1_000, 2_000, 4_000]
pub const MAX_RETRY_AFTER_MS: u64 = 10_000
pub const MOCK_LATENCY_FIRST_MS: u64 = 350
pub const MOCK_LATENCY_NEXT_MS: u64 = 120
pub const CONTEXT_CHARS: usize = 300
pub const LOOKAHEAD_SEC: f64 = 120.0
    // Max seconds of delivered-but-unplayed audio before synthesis waits for the listener.
pub const LOOKAHEAD_FREE_CHUNKS: usize = 2
    // Chunks `[0, LOOKAHEAD_FREE_CHUNKS)` are requested without waiting.
pub const ESTIMATED_CHARS_PER_SECOND: f64 = 15.0
    // Speech-rate guess used before real durations exist.
pub const MOCK_SAMPLE_RATE: u32 = 22_050
pub const MAX_READ_TEXT: usize = 2_000_000
    // Text sliced from UI/bridge input before the reader sees it.
pub const PREVIEW_CHARS: usize = 160
    // Characters kept on a record when `library.storeText` is off.
pub const HISTORY_WRITE_INTERVAL_MS: u64 = 2_000
pub const TITLE_CHARS: usize = 64
    // Max length of `previewTitle` (player title, tray tooltip, media overlay).
pub const RAW_CUT_FACTOR: usize = 4
pub const RAW_CUT_MIN: usize = 200_000
pub const PARTIAL_READ_ERROR: &str = "Only part of the text could be synthesized"
pub const MAX_TRACKED_SESSIONS: usize = 16
pub const CACHE_MB: u64 = 1_000_000
    // Cache limit unit: decimal MB.
pub const HIDDEN_MARK: &str = "\u{2026}"
    // Stand-in for text that was not stored (preview-only cached replays).
pub const SOURCE_TITLE_MAX: usize = 200
    // `sourceTitle` stored on a record is trimmed and cut to this many units.
pub const READ_FILE_EXTRA_CHARS: usize = 1_000
    // `--read-file` reads this many characters past `text.maxChars` so truncation is detectable.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024
    // Max file bytes read by the file text extractor.
pub const PLAYBACK_RATES: [f64; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0]
    // Client-side rates offered in the UI (defaults.ts:84).
pub const RATE_MIN: f64 = 0.5
pub const RATE_MAX: f64 = 3.0
pub const END_GUARD_SEC: f64 = 0.05
    // Seeks never land closer than this to the end of available audio / of a chunk.
pub const BACK_GRACE_SEC: f64 = 1.2
    // "Previous sentence" restarts the current sentence when more than this many wall seconds into it.
pub const PARTIAL_SKIP_SEC: f64 = 10.0
    // Prev/next step when sentence positions are unknown (partial text).
pub const DEFAULT_SKIP_SEC: f64 = 10.0
    // Default `skip` amount with `unit: seconds`.
pub const IDLE_OUTPUT_HOLD_MS: u64 = 30_000
pub const PAUSED_OUTPUT_HOLD_MS: u64 = 60_000
pub const PLAYER_REPORT_MS: u64 = 500
    // Player state report cadence while playing (core-facing; the UI is refreshed at UI_FRAME_MS).
pub const MIN_CHUNK_DURATION_SEC: f64 = 0.1
    // Floor of an estimated chunk duration.
pub const WARM_MS: u64 = 1_000
    // Output warmer: audio the device must have rendered before speech starts.
pub const WARM_MAX_WAIT_MS: u64 = 3_000
    // Output warmer: a warm-up never holds playback longer than this.
pub const WARM_POLL_MS: u64 = 25
pub const WARM_RETRY_MS: u64 = 60_000
    // Output warmer: after the stream could not be created, don't retry for this long.
pub const LONG_SENTENCE: usize = 280
    // Sentences longer than this are soft-split at clause punctuation (sentence skipping).
pub const SOFT_TARGET: usize = 200
pub const AUTO_HIDE_MS: u64 = 4_000
pub const HOLD_RETRY_MS: u64 = 1_500
pub const WIDGET_TOAST_MS: u64 = 2_500
pub const WIDGET_TOAST_ACTION_MS: u64 = 5_000
pub const WIDGET_TOAST_HOVER_RECHECK_MS: u64 = 1_000
pub const TOAST_INFO_MS: u64 = 4_000
    // App-window toast durations (ui-main.md §1.7).
pub const TOAST_SUCCESS_MS: u64 = 3_000
pub const TOAST_ERROR_MS: u64 = 6_500
pub const TOAST_ACTION_MIN_MS: u64 = 6_000
pub const MAX_TOASTS: usize = 4
pub const TOAST_EXIT_MS: u64 = 180
pub const UI_FRAME_MS: u64 = 33
    // UI refresh cadence for the playing state (≈30 Hz).
pub const PAGE_REFRESH_DEBOUNCE_MS: u64 = 700
    // Home / History trailing refresh debounce.
pub const HISTORY_SEARCH_DEBOUNCE_MS: u64 = 220
pub const HISTORY_PAGE_SIZE: usize = 50
pub const CREDITS_AFTER_READ_MS: u64 = 1_500
    // Credits: forced refresh after a read settles; wake throttle.
pub const CREDITS_WAKE_THROTTLE_MS: u64 = 30_000
pub const CACHE_REMEASURE_MS: u64 = 2_500
    // Settings library: re-measure cache this long after the last history change.
pub const RETENTION_SETTLE_MS: u64 = 450
pub const HOTKEY_REJECT_MESSAGE_MS: u64 = 6_000
pub const HOTKEY_HINT_MS: u64 = 1_800
pub const COPY_CONFIRM_MS: u64 = 1_400
pub const BRIDGE_RECHECK_MAX_MS: u64 = 4_000
pub const WIDGET_MARGIN: f64 = 24.0
pub const DEFAULT_WIDGET_SIZE: (f64, f64) = (380.0, 168.0)
pub const WIDGET_SIZE_MIN: (f64, f64) = (40.0, 24.0)
pub const WIDGET_SIZE_MAX: (f64, f64) = (1200.0, 900.0)
pub const HIT_RECT_MIN: f64 = 8.0
pub const DEFAULT_CHIP_SIZE_CONTEXT: (f64, f64) = (153.0, 38.0)
pub const DEFAULT_CHIP_SIZE_SELECTION: (f64, f64) = (82.0, 38.0)
pub const CHIP_SIZE_MIN: (f64, f64) = (32.0, 24.0)
pub const CHIP_SIZE_MAX: (f64, f64) = (480.0, 160.0)
pub const OWN_MOVE_GRACE_MS: u64 = 500
    // User-drag notifications are ignored for this long after our own move/resize.
pub const HOVER_POLL_MS: u64 = 100
pub const CHIP_EXIT_MS: u64 = 140
    // Chip exit: 120 ms UI animation + a frame.
pub const CONTEXT_MENU_SMALL: (f64, f64) = (220.0, 240.0)
pub const CONTEXT_MENU_GUESS: (f64, f64) = (320.0, 360.0)
pub const CONTEXT_MENU_LARGE: (f64, f64) = (360.0, 480.0)
pub const CHIP_GAP: f64 = 18.0
pub const CHIP_GAP_V: f64 = 20.0
pub const SELECTION_CHIP_GAP: f64 = 20.0
pub const NEAR_CURSOR_DX: f64 = 16.0
pub const NEAR_CURSOR_DY: f64 = 20.0
pub const NEAR_CURSOR_FLIP: f64 = 16.0
pub const MAIN_WINDOW_SIZE: (f64, f64) = (1180.0, 780.0)
pub const MAIN_WINDOW_MIN: (f64, f64) = (940.0, 640.0)
pub const SIDEBAR_RAIL_BELOW: f64 = 1100.0
    // Below this main-window width (logical px = DIP) the sidebar collapses to icons: ui-main.md §2.6 `RAIL_QUERY` `max-width: 1099px`.
pub const CUSTOM_COORD_LIMIT: f64 = 100_000.0
pub const TOOLTIP_MAX: usize = 127
    // Tray tooltip limit (UTF-16 units).
pub const SHUTDOWN_TIMEOUT_MS: u64 = 3_000
pub const FORCE_EXIT_MS: u64 = 4_000
pub const RETENTION_INTERVAL_MS: u64 = 6 * 3_600_000
pub const RETENTION_CHANGE_DELAY_MS: u64 = 60_000
pub const SHORTCUT_SUSPEND_MAX_MS: u64 = 30_000
pub const HOTKEY_SUSPEND_REARM_MS: u64 = 20_000
    // The Settings recorder re-arms the hotkey suspension this often while it records with the main window focused and visible (ARCHITECTURE.md §8 D-USW-18).
pub const SHUTDOWN_WATCHDOG_MS: u64 = SHUTDOWN_TIMEOUT_MS + FORCE_EXIT_MS
    // Detached watchdog armed by the first quit request: `std::process::exit(0)` if the process is still alive after the bounded flush budget plus the UI's hard exit (shell.md §3.5 worst case ≈ 7 s; ARCHITECTURE.md §4.17).
pub const SERVICE_SHUTDOWN_JOIN_MS: u64 = 500
    // Each of `player.shutdown()` ×2 and `media.shutdown()` runs on a helper thread joined for at most this long.
pub const SESSION_END_WAIT_MS: u64 = 5_000
    // The system watcher waits this long for the core's `SessionEnd` reply …
pub const SESSION_END_FALLBACK_MS: u64 = 2_500
    // … and flushes settings + history itself (`flush_sync`) when no reply came within this long.
pub const PRELOAD_OUTPUT_DELAY_MS: u64 = 1_000
    // The reading player builds its output stream this long after core startup, whether or not a window is shown (reader-playback.md §6 preload; skipped with `null_output`).
pub const DAY_MS: f64 = 86_400_000.0
pub const SETTINGS_DEBOUNCE_MS: u64 = 250
pub const SETTINGS_MAX_WAIT_MS: u64 = 2_000
pub const HISTORY_DEBOUNCE_MS: u64 = 2_500
pub const HISTORY_MAX_WAIT_MS: u64 = 10_000
pub const HISTORY_NOTIFY_DEBOUNCE_MS: u64 = 100
    // History change notification: store debounce / max wait, then the UI throttle.
pub const HISTORY_NOTIFY_MAX_WAIT_MS: u64 = 400
pub const HISTORY_CHANGED_THROTTLE_MS: u64 = 150
pub const FILE_RETRY_DELAYS_MS: [u64; 5] = [15, 40, 90, 180, 350]
    // Windows sharing-violation retry delays (5 retries, 6 attempts).
pub const MAX_WRITE_RETRIES: u32 = 5
pub const LISTEN_LOG_MAX_KEYS: usize = 400
pub const LOG_ROTATE_BYTES: u64 = 1_048_576
pub const LOG_ARG_MAX_CHARS: usize = 2_000
pub const SILENT_READING_WPM: f64 = 238.0
pub const TOP_N: usize = 6
pub const MAX_DAILY_DAYS: usize = 365
    // Largest finite analytics range in days.
pub const MAX_ALL_DAYS: usize = 3_653
    // `all` analytics window cap: `startDay = max(local day of the oldest valid record, today − (MAX_ALL_DAYS − 1))`, so one malformed `createdAt` (0, −1e14) cannot produce a 20 000-entry daily series; older records fall outside the window every…
pub const DAILY_GOAL_SEC: f64 = 900.0
    // Daily listening goal on Home (15 minutes).
pub const LOW_CREDITS_SHARE: f64 = 0.15
    // Credits below this share are "low".
pub const LOCAL_SERVER_DEFAULT_PORT: u16 = 47_823
pub const PROXY_RECHECK_MS: u64 = 30_000
    // The ElevenLabs client re-reads the Windows proxy before a read / catalog fetch when the last check is older than this (ARCHITECTURE.md §8 D-NET-1).
pub const LOCAL_SERVER_CLIENT_HEADER: &str = "x-vox-client"
pub const LOCAL_SERVER_CLIENT_VALUE: &str = "browser-extension"
pub const BRIDGE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024
pub const BRIDGE_TIMEOUT_MS: u64 = 15_000
pub const BRIDGE_HEADERS_TIMEOUT_MS: u64 = 10_000
pub const BRIDGE_KEEP_ALIVE_MS: u64 = 5_000
pub const BRIDGE_MAX_CONNECTIONS: usize = 64
pub const BRIDGE_IDLE_SOCKET_MS: u64 = 20_000
pub const BRIDGE_MAX_HEADER_BYTES: usize = 16 * 1024
pub const BRIDGE_URL_MAX: usize = 2_048
pub const BRIDGE_TITLE_MAX: usize = 300
pub const HTTP_URL_MAX: usize = 4_096
    // http(s) URLs kept on a record are at most this long.
pub const MODIFIER_WAIT_MS: u64 = 700
pub const MODIFIER_POLL_MS: u64 = 10
pub const CAPTURE_SETTLE_MS: u64 = 40
pub const CONTEXT_MENU_SETTLE_MS: u64 = 120
pub const CLIPBOARD_POLL_MS: u64 = 15
pub const CLIPBOARD_POLL_TIMEOUT_MS: u64 = 450
pub const CLIPBOARD_TEXT_SETTLE_MS: u64 = 1_500
pub const ESCAPE_SETTLE_MS: u64 = 80
pub const CLIPBOARD_RESTORE_DELAY_MS: u64 = 150
pub const LATE_COPY_GRACE_MS: u64 = 1_500
pub const LATE_POLL_MS: u64 = 25
pub const CHIP_ACTIVATION_DEDUPE_MS: u64 = 300
pub const HOOK_SILENCE_REINSTALL_MS: u64 = 10_000
    // The selection consumer re-installs the hook when it saw no hook event for this long while `GetLastInputInfo` reports newer input (Windows silently removes slow LL hooks; selection.md §14).
pub const DRAG_THRESHOLD_PX: f64 = 6.0
pub const MULTI_CLICK_MS: u64 = 500
pub const MULTI_CLICK_PX: f64 = 4.0
pub const HINT_TTL_MS: u64 = 60_000
pub const SELECTION_CHIP_MS: u64 = 4_000
pub const CONTEXT_CHIP_MS: u64 = 8_000
pub const WINDOW_TITLE_MAX: usize = 300
    // Window titles are whitespace-collapsed and cut to this many units.
pub const EXCLUDED_APPS_MAX: usize = 200
    // Max excluded apps kept by the settings sanitizer.
pub const EXPLORER_EXTENSIONS: [&str; 9] = [".txt", ".md", ".markdown", ".log", ".csv", ".json", ".rtf", ".html", ".htm"]
    // Explorer verb file types, in registry order.
pub const EXPLORER_VERB: &str = "VoxRead"
    // Explorer verb key name (mirrored by the uninstaller).
pub const RUN_VALUE_NAME: &str = "Vox"
    // Launch-at-login value name (dev builds use `Vox (dev)`).
pub const RUN_VALUE_NAME_DEV: &str = "Vox (dev)"
pub const HIDDEN_ARG: &str = "--hidden"
pub const SINGLE_INSTANCE_KEY: &str = "com.vox.reader"
    // Named-pipe / mutex key of the single-instance lock (suffixed with `-Demo` in demo mode).
```

#### `vox_core::copy`

```rust
pub const ADD_KEY_TO_READ: &str = "Add your ElevenLabs API key to start reading"
pub const ADD_KEY_ACTION: &str = "Add key"
pub const OPEN_SETTINGS_ACTION: &str = "Open settings"
pub const NOTHING_TO_READ: &str = "Nothing to read in that selection"
pub const COPY_SOME_TEXT: &str = "Copy some text first"
pub const SELECT_SOME_TEXT: &str = "Select some text first"
pub const CLIPBOARD_TOO_LARGE: &str = "Couldn’t restore your previous clipboard (it was too large)"
pub const CLIPBOARD_UNREADABLE: &str = "Couldn’t restore your previous clipboard"
pub const EXPLORER_MENU_FAILED: &str = "Couldn’t update the Explorer menu"
pub fn elevated_app_refused() -> String
    // Selection notice when the foreground app runs elevated and Vox does not (UIPI blocks the copy chord).
pub fn old_version_running_title() -> String
    // Native message box when an Electron build of Vox is running (BRIEF.md "Do not run both builds at once").
pub fn old_version_running_body() -> String
pub fn key_migration_failed() -> String
    // Shown on Settings → Account and the onboarding key step when the Electron key could not be migrated (persistence.md §4; BRIEF.md "API key").
pub fn key_unreadable() -> String
    // Same place, when `secrets-dpapi.bin` exists but cannot be decrypted (profile copied from another account/PC).
pub const HOOK_UNAVAILABLE_TITLE: &str = "Mouse and keyboard hook unavailable"
    // Settings → Triggers notice while the global input hook could not be installed (ui-settings-widget.md §2.5).
pub const HOOK_UNAVAILABLE_BODY: &str = "The right-click chip and the selection popup need it, so they\u{2019}re paused. Shortcuts, the b…
pub const PLAYBACK_FAILED: &str = "Audio playback stopped unexpectedly. Check your speakers or headphones and try again."
    // Error-card message after the audio player failed and was restarted (BRIEF.md "Audio").
pub const NO_AUDIO_OUTPUT: &str = "Couldn\u{2019}t open your speakers or headphones"
    // Error-card message when no audio output device can be opened.
pub const SOMETHING_WENT_WRONG: &str = "Something went wrong"
    // The generic failure text (also what `format::clean_error` returns for a blank message).
pub const APPEARANCE_INTRO: &str = "The theme changes this window. The floating player and the chip always stay dark."
    // Settings → Appearance intro and per-theme tile notes (BRIEF.md Product rulings "Copy": the old "Cards stay black…" / "Black cards on …" copy described the old look; ARCHITECTURE.md §8 D-USW-20: the floating player and the chip are always d…
pub const THEME_NOTES: [&str; 3] = ["Follows Windows dark mode", "Light window", "Dark window"]
pub fn open_link_failed(err: &str) -> String
    // `Couldn't open the link: <cleaned error>` (History page links, Settings → Account / onboarding "Get a key").
pub fn reading_first(max_chars: u32) -> String
    // `Reading the first 20,000 characters`.
pub fn couldnt_open(file_name: &str) -> String
    // `Couldn't open <file name>`.
pub fn nothing_is_playing(read_selection_hotkey: Option<&str>) -> String
    // `Nothing is playing. Select some text and press <hotkey>` / `… to read it`.
pub fn secret_refused() -> String
    // `That looks like a password, so Vox won't read it`.
pub fn private_refused() -> String
    // `That text is marked private, so Vox won't read it`.
pub fn password_manager_refused() -> String
    // `Vox doesn't read from password managers`.
pub fn explorer_menu_failed(err: &str) -> String
    // `Couldn't update the Explorer menu[: <err>]`.
pub fn bridge_port_failed(port: u16) -> String
    // `Browser bridge couldn't use port <N>. Pick another port in Settings`.
pub fn port_in_use(port: u16) -> String
    // Bridge listen errors (integrations.md §1.2).
pub fn port_blocked(port: u16) -> String
pub const LOOPBACK_UNAVAILABLE: &str = "The loopback address 127.0.0.1 is not available"
pub fn invalid_port(port: impl std::fmt::Display) -> String
pub fn port_may_be_in_use(port: u16) -> String
    // Settings → Integrations note under the port field.
pub fn reading_with(name: &str) -> String
    // `Reading with George from now on`.
pub fn test_voice_sample(voice_name: &str) -> String
    // Voice & model "Test voice" sample.
pub fn onboarding_sample(voice_name: &str) -> String
    // Onboarding "Try it" sample.
```

#### `vox_core::format`

```rust
pub const WEEKDAYS_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"]
pub const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
pub const WEEKDAY_INITIALS: [&str; 7] = ["S", "M", "T", "W", "T", "F", "S"]
pub const WEEKDAYS_MON_FIRST: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
    // Monday-first full names (heatmap rows).
pub const MONTHS_LONG: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October"…
pub const MONTHS_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
pub fn format_clock(sec: f64) -> String
    // `263 → "4:23"`, `3723 → "1:02:03"` (floored).
pub fn remaining_clock(position_sec: f64, duration_sec: f64, estimate: bool) -> String
    // `'-3:20'`, `'~-3:20'` while estimated (Home/sidebar; rounded).
pub fn widget_time_label(state: &PlayerState) -> (String, bool)
    // The widget's time label (reader-playback.md §8.3): ended → total length at the current rate; otherwise the remaining wall time prefixed `≈` (estimate) or `−` (U+2212).
pub fn progress_accessible(state: &PlayerState) -> String
    // The widget progress bar's accessible value (reader-playback.md §8.3; ui-settings-widget.md §6.3): `{clock(pos)} of {about }{clock(dur)}` in media seconds (no rate division), `about ` while the duration is an estimate — `"1:03 of about 4:23…
pub fn dot_duration(sec: f64) -> String
    // `15600 → "4H20M"`, `3900 → "1H05M"`, `42 → "42S"` (floored; the old dot-matrix style).
pub fn format_hm_dot(sec: f64) -> String
    // `152280 → "42H18M"`, `1080 → "18M"`, `45 → "45S"`, `0 → "0M"`, `360000 → "100H"` (rounded).
pub fn duration_speech(sec: f64) -> String
    // `'4 hours 20 minutes'` (floored).
pub fn speak_duration(sec: f64) -> String
    // Like [`duration_speech`] but rounded.
pub fn format_duration(sec: f64) -> String
    // `15600 → "4h 20m"`, `300 → "5m"`, `20 → "20s"`, `3600 → "1h"` (rounded).
pub fn hour_range(hour: u32) -> String
    // `"21:00–22:00"` (en dash; 24 wraps to 00).
pub fn format_number(n: f64) -> String
    // `round(n)` with the user locale's grouping: `38766 → "38,766"` (en-US) / `"38.766"` (de-DE).
pub fn format_number_en(n: f64) -> String
    // [`format_number`] pinned to en-US (`toLocaleString('en-US')` in the old app: the truncation toast).
pub fn format_compact(n: f64) -> String
    // Compact notation with at most one fraction digit (English suffixes, locale decimal separator): `12400 → "12.4K"`, `100000 → "100K"`, `999 → "999"`, `1500000 → "1.5M"`; de-DE `"12,4K"`.
pub fn count_label(n: u64, singular: &str, plural: &str) -> String
    // `"{n} {singular|plural}"` with locale grouping (`"1 read"`, `"1,204 reads"`).
pub fn rate_label(rate: f64) -> String
    // Playback rate label: `1 → "1×"`, `1.25 → "1.25×"` (`Number(r.toFixed(2))`).
pub fn percent(fraction: f64) -> String
    // Percentage text `"39%"` of a 0..1 fraction.
pub fn truncate(text: &str, max: usize) -> String
    // Whitespace collapsed; longer than `max` units → first `max − 1` units, trimmed, plus `…`.
pub fn display_url(url: &str) -> String
    // `https://www.example.com/a/b?q=1 → "example.com/a/b"`; unparsable → the raw string.
pub fn clean_error(raw: &str) -> String
    // Strips a leading `SomethingError: ` from an error message; empty → `Something went wrong`.
pub fn time_of_day<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
    // `"9:05 PM"` (12-hour locales) / `"21:05"` (24-hour locales).
pub fn date_line<Tz: TimeZone>(now: &DateTime<Tz>) -> String
    // `"Friday, October 17"` (month-first) / `"Friday 17 October"` (day-first).
pub fn full_date_time<Tz: TimeZone>(tz: &Tz, ms: f64) -> String
    // `"Friday, October 17, 2026 at 9:05 PM"` / `"Friday 17 October 2026 at 21:05"`.
pub fn short_date_with_year<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
    // `"Oct 8, 2026"` / `"8 Oct 2026"`.
pub fn weekday_month_day<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
    // `"Tue, Sep 30"` / `"Tue 30 Sep"`.
pub fn greeting<Tz: TimeZone>(now: &DateTime<Tz>) -> &'static str
    // Greeting by local hour: 05–11 morning, 12–17 afternoon, else evening.
pub fn format_relative<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String
    // `Intl.RelativeTimeFormat('en', {numeric: 'auto'})` style: `just now` (< 45 s), minutes, hours, then calendar days (`yesterday`, `3 days ago`) within a week, then `Oct 8, 2026`.
pub fn day_label<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String
    // History day-group label: `Today` (also future), `Yesterday`, weekday within the last week, `Mon, Sep 14` this year, else `Mon, Sep 14, 2025` (day-first locales: `Mon 14 Sep`, `Mon 14 Sep 2025`).
pub fn row_time<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String
    // History row time: relative within 6 days, else the time of day.
pub fn weekday_initial(day_key: &str) -> &'static str
    // `S M T W T F S` initial of a day key; `·` when unparsable.
pub fn weekday_name(day_key: &str) -> String
    // `Monday` for a day key; the key itself when unparsable.
pub fn credits_left_fraction(count: f64, limit: f64) -> f64
    // `limit ≤ 0` → 0; else `clamp((limit − count) / limit, 0, 1)`.
pub fn credits_low(fraction_left: f64) -> bool
    // True below 15 %.
pub fn reset_label(next_reset_unix: Option<f64>, now_ms: f64) -> Option<String>
    // `Resets in 12 days` / `Resets tomorrow` / `Resets today` (any reset within 24 h is "tomorrow", parity quirk).
pub fn credits_facts(remaining: f64, limit: f64) -> String
    // `38.8K of 100K left` (locale decimal separator).
pub fn credits_detail(remaining: f64, limit: f64, reset: Option<&str>) -> String
    // `38,766 of 100,000 characters left. Resets in 12 days` (locale grouping).
pub fn tier_label(tier: &str) -> String
    // Analytics tier badge: `[_-]+` → space, capitalise each word; `""` → `Plan`.
pub fn title_case_words(tier: &str) -> String
    // Home tier badge: split on `[_\s-]+`, capitalise each word; empty stays empty (no badge).
pub fn pretty_model_id(id: &str) -> String
    // Home's model label for ids the list lacks: `eleven_flash_v2_5 → "Flash v2.5"`, `eleven_v3 → "Eleven v3"`.
pub fn model_label(id: &str, models: &[TtsModel]) -> String
    // Home's short model name (= [`model_name`]).
pub fn pretty_model_id_settings(id: &str) -> String
    // Title-cases an arbitrary model id: `eleven_turbo_v2_5 → "Eleven Turbo v2.5"`, `eleven_v3 → "Eleven v3"`, `my_model → "My Model"` (the last fallback of [`model_name`]).
pub fn model_name(id: &str, models: &[TtsModel]) -> String
    // **The** model display name (BRIEF.md Product rulings: one style everywhere, e.g. `Flash v2.5`): the catalogue name without a leading `Eleven ` (kept before a version), else [`pretty_model_id`], else the title-cased id without `Eleven `.
pub fn model_label_history(id: &str) -> String
    // History details model label: [`model_name`] without a catalogue; `""` → `Unknown model`.
pub fn format_bytes(bytes: u64) -> String
    // Decimal units: `187_400_000 → "187 MB"`, `1_600_000_000 → "1.6 GB"`, `0 → "0 MB"` (locale decimal separator).
pub fn format_megabytes(mb: u32) -> String
    // `500 → "500 MB"`, `2000 → "2 GB"`, `1500 → "1.5 GB"`.
pub fn cache_badge(bytes: u64) -> String
    // Header badge form without the space: `"187MB"`.
pub fn status_label(s: &PlayerSnapshot) -> &'static str
    // Widget status label (sentence case per DESIGN.md): `Stopped early`, `Starting`, `Ready`, `Synthesizing`, `Reading`, `Paused`, `Finished`, `Error`.
pub fn mini_status_label(status: PlayerStatus) -> &'static str
    // Sidebar mini-player label.
pub fn widget_source_label(s: &PlayerSnapshot) -> String
    // The widget's source label: `sourceApp`, else `Replay` for cached replays, else `Vox` (not upper-cased in the new design).
pub fn warning_lead(ended: bool) -> &'static str
    // `Stopped early` (ended) / `Stops early`.
pub fn warning_reason(code: ReadErrorCode) -> &'static str
    // Short reason for a non-fatal error.
pub fn error_title(code: ReadErrorCode) -> &'static str
    // Error-card title by code.
pub fn error_detail(state: &PlayerState) -> String
    // Error-card detail: the error if present and different from the title, else the generic line.
pub fn tray_tooltip(state: &PlayerState) -> String
    // Tray tooltip (shell.md §7.2), cut to 127 UTF-16 units keeping a closing `”`.
pub fn is_low_share(v: f64) -> bool
    // A 0..1 meter tone: low credits use the warning tone.
```

#### `vox_core::geometry`

```rust
pub struct Point { x: f64, y: f64 }
pub struct Size { width: f64, height: f64 }
pub struct Rect { x: f64, y: f64, width: f64, height: f64 }
impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self
    pub fn right(&self) -> f64
    pub fn bottom(&self) -> f64
    pub fn size(&self) -> Size
    pub fn contains(&self, p: Point) -> bool
        // Half-open containment `[x, x+w) × [y, y+h)`.
    pub fn intersects(&self, o: &Rect) -> bool
        // Overlap test where touching edges do not count.
}
pub enum HAnchor { Left, Center, Right }
    // Horizontal anchor (edge kept fixed when the widget resizes).
pub enum VAnchor { Top, Bottom }
pub struct Anchor { h: HAnchor, v: VAnchor }
impl Default for Anchor {
}
pub fn clamp_to_area(r: Rect, area: Rect) -> Rect
    // Moves `r` inside `area` without resizing (an oversized rect pins to the area's top-left), then rounds every field with `Math.round`.
pub fn anchor_for(r: Rect, area: Rect) -> Anchor
    // Quadrant anchor: centre strictly right of the area's mid-x → right; strictly below mid-y → bottom.
pub fn resize_anchored(old: Rect, size: Size, anchor: Anchor, area: Rect) -> Rect
    // New bounds for a size change keeping the anchored edges fixed, clamped into `area`.
pub struct WidgetPlacementInput { position: WidgetPosition, size: Size, area: Rect, cursor: Point, custom: Option<Point> }
    // Inputs for [`place_widget`].
pub fn place_widget(i: WidgetPlacementInput) -> (Rect, Anchor)
    // Widget bounds and anchor per `widget.position` (shell.md §6.3.4).
pub enum MenuH { Right, Left, Unknown }
    // Which way a native context menu opens.
pub enum MenuV { Down, Up, Unknown }
pub fn predict_menu(cursor: Point, area: Rect, small: Size, large: Size) -> (MenuH, MenuV)
    // Predicts the menu's flip from the room around the cursor.
pub fn menu_rects(cursor: Point, pred: (MenuH, MenuV), menu: Size) -> Vec<Rect>
    // Every rect the menu may occupy.
pub fn context_candidates(cursor: Point, size: Size, h: MenuH, menu: Size) -> Vec<Rect>
    // Candidate chip positions, best first.
pub fn place_chip(mode: ChipMode, cursor: Point, size: Size, area: Rect) -> Rect
    // Chip bounds: context mode beside the predicted menu (never on it), selection mode above-right of the cursor.
pub fn sanitize_size(width: f64, height: f64, min: (f64, f64), max: (f64, f64)) -> Option<Size>
    // `sanitizeSize`: finite width/height clamped to `[min, max]` then rounded; `None` when not finite.
pub fn sanitize_hit(hit: Rect, size: Size) -> Option<Rect>
    // `sanitizeHit`: the card rectangle inside the window, floored/ceiled and clipped; smaller than 8×8 → `None` (the whole window counts).
pub fn point_on_card(p: Point, window: Rect, hit: Option<Rect>) -> bool
    // `pointOnCard`: cursor within the card (window-relative `hit`, or the whole window).
```

#### `vox_core::history`

```rust
pub enum ReadSource { Hotkey, ContextChip, SelectionPopup, BrowserExtension, Explorer, Clipboard, Manual, Replay, Cli }
    // How a read was started.
impl ReadSource {
    pub const ALL: [ReadSource; 9] = [ ReadSource::Hotkey, ReadSource::ContextChip, ReadSource::SelectionPopup, ReadSource::BrowserExt…
        // Validation list, in this order (main/ipc.ts:73-83).
    pub const FILTER_ORDER: [ReadSource; 9] = [ ReadSource::ContextChip, ReadSource::Hotkey, ReadSource::SelectionPopup, ReadSource::BrowserExt…
        // History "Filter by source" order (charts/labels.ts:31-41).
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn label(self) -> &'static str
        // UI label (ui-main.md §6.2).
    pub fn icon(self) -> &'static str
        // Lucide icon name (ui/icons) for this source.
    pub fn fallback_app_name(self) -> &'static str
        // Analytics app-name fallback when a record has no `sourceApp` (persistence.md §7.3).
}
pub struct ReadRecord { id: String, created_at: f64, text: String, text_truncated: bool, char_count: u64, word_count: u64, voice_id: String, voice_name: String, model_id: String, source: ReadSource, source_app: Option<String>, source_process: Option<String>, source_title: Option<String>, source_url: Option<String>, audio_duration_sec: f64, listened_sec: f64, max_position_sec: Option<f64>, listen_log: Option<ListenLog>, completed: bool, cached: bool, characters_billed: u64, favorite: bool, error: Option<String> }
    // One read, as persisted (field order = serialized order; absent optionals are omitted, never `null`).
impl ReadRecord {
    pub fn new(id: String, created_at: f64, source: ReadSource) -> Self
        // A fresh record with zero/false bookkeeping fields (the reader fills creation fields).
}
pub fn count_whitespace_words(text: &str) -> u64
    // Whitespace word count: `\s+`-separated tokens of the JS-trimmed text (persistence.md §8.4 `countWords`).
pub fn count_words(text: &str) -> u64
    // The reader's word count: matches of `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu`.
pub fn count_tokens(text: &str) -> u64
    // `\S+` runs (the UI's live word counter in Quick read).
impl ReadRecord {
    pub fn from_value(v: &Value, base: Option<&ReadRecord>) -> Option<ReadRecord>
        // Normalises an untrusted record (history-store.ts:48-98).
    pub fn normalized(mut self) -> ReadRecord
        // Applies the store's normalisation to a typed record (trims optional strings, lower-cases the process, clamps numbers, drops a zero `maxPositionSec`, sanitizes the ledger, cuts the id to 128 units).
    pub fn apply_patch(&self, p: &RecordPatch) -> ReadRecord
        // `update(id, patch)` merge: patch fields over `self`, re-normalised with `self` as the base.
    pub fn same_as(&self, other: &ReadRecord) -> bool
        // Field-by-field equality with ledgers compared by value (absent ≡ empty).
    pub fn to_json(&self) -> String
        // Compact JSON of this record.
}
pub struct RecordPatch { created_at: Option<f64>, text: Option<String>, text_truncated: Option<bool>, char_count: Option<f64>, word_count: Option<f64>, voice_id: Option<String>, voice_name: Option<String>, model_id: Option<String>, source: Option<ReadSource>, source_app: Option<Option<String>>, source_process: Option<Option<String>>, source_title: Option<Option<String>>, source_url: Option<Option<String>>, audio_duration_sec: Option<f64>, listened_sec: Option<f64>, max_position_sec: Option<f64>, listen_log: Option<ListenLog>, completed: Option<bool>, cached: Option<bool>, characters_billed: Option<f64>, favorite: Option<bool>, error: Option<Option<String>> }
    // A typed history patch.
pub struct HistoryFile<'a> { version: u32, records: &'a [ReadRecord] }
    // `history.json` file wrapper: `{"version":1,"records":[…]}`.
pub struct HistoryExport<'a> { app: &'a str, version: u32, exported_at: String, records: &'a [ReadRecord] }
    // History export (pretty JSON): `{"app":"Vox","version":1,"exportedAt":ISO,"records":[…]}`.
pub fn parse_history_file(v: &Value) -> (Vec<ReadRecord>, usize)
    // Parses a loaded `history.json` value (`{records:[…]}` or a bare array): sanitizes every record, counts rejects, sorts by `createdAt` descending (stable) and drops later duplicates of an id.
pub fn insertion_index(records: &[ReadRecord], created_at: f64) -> usize
    // Index where a record with `created_at` belongs in a newest-first list: after every strictly newer record and **before** existing records with an equal timestamp.
pub struct HistoryQuery { search: Option<String>, limit: Option<f64>, offset: Option<f64>, favorites_only: bool, source: Option<ReadSource> }
    // `history:list` query.
impl HistoryQuery {
    pub fn effective_limit(&self) -> usize
        // Effective `limit`.
    pub fn effective_offset(&self) -> usize
        // Effective `offset`.
    pub fn terms(&self) -> Vec<String>
        // Lower-cased literal search terms: the first 500 units, trimmed, split on whitespace, at most 16.
    pub fn matches(&self, r: &ReadRecord, terms: &[String]) -> bool
        // True if `r` matches the filters and every term occurs (case-insensitively) in one of text, sourceApp, sourceTitle, voiceName, sourceUrl.
}
pub struct HistoryPage { items: Vec<ReadRecord>, total: usize }
    // One page of matches.
pub fn list_records(records: &[ReadRecord], q: &HistoryQuery) -> HistoryPage
    // Runs a query over a newest-first record list.
pub fn count_expiring(records: &[ReadRecord], days: u32, now_ms: f64) -> usize
    // Number of non-favorite records `applyRetention(days)` would delete now (`createdAt < now − days × 86 400 000`).
pub fn listened_fraction(r: &ReadRecord) -> f64
    // "Heard" share for the History row: completed → 1; unknown audio length → 0; else reached / audio, capped at 0.99.
pub fn audio_length(r: &ReadRecord) -> (f64, bool)
    // Audio length `(seconds, is_estimate)`: the synthesized length, else `round(charCount / 15)` estimated.
pub fn can_replay(r: &ReadRecord) -> bool
    // Replay is possible: cached audio, or the full text to re-synthesize.
```

#### `vox_core::hotkey`

```rust
pub enum HotkeyAction { ReadSelection, ReadClipboard, PlayPause, Stop }
    // The four hotkey actions, in registration order.
impl HotkeyAction {
    pub const ALL: [HotkeyAction; 4] = [Self::ReadSelection, Self::ReadClipboard, Self::PlayPause, Self::Stop]
    pub fn id(self) -> &'static str
        // Settings key (`triggers.hotkeys.<id>`).
    pub fn parse(id: &str) -> Option<Self>
    pub fn label(self) -> &'static str
    pub fn description(self) -> &'static str
    pub fn settings_path(self) -> String
        // Dotted settings path.
}
pub const RESERVED_ACCELERATORS: [&str; 12] = [ "Ctrl+C", "Ctrl+Insert", "Ctrl+V", "Ctrl+X", "Ctrl+Z", "Ctrl+Y", "Ctrl+A", "Ctrl+S", "Alt+F4",…
    // Chords a hotkey must never take (compared canonically).
pub fn split_parts(accel: &str) -> Vec<String>
    // Splits an accelerator on `+`, handling a literal `+` key (`"Ctrl++"` → `["Ctrl", "+"]`): a `+` at a part start is the key itself and a following `+` is skipped.
pub fn canonical_parts(accel: &str) -> Vec<String>
    // Lower-cased, alias-folded parts; `AltGr` expands to `ctrl` + `alt` (Windows reports AltGr as Ctrl+Alt).
pub fn canonical_key(accel: &str) -> String
    // The canonical comparison key: modifiers de-duplicated in `ctrl, alt, shift, super` order, then the other parts in original order.
pub fn is_reserved(accel: &str) -> bool
    // True for a non-blank accelerator equal (canonically) to a reserved chord.
pub fn conflicts(h: &HotkeySettings) -> Vec<HotkeyAction>
    // Actions whose non-blank shortcut is shared with another action.
pub struct Modifiers { ctrl: bool, alt: bool, shift: bool, win: bool }
    // Hotkey modifiers.
impl Modifiers {
    pub fn strong(self) -> bool
        // Ctrl, Alt or Win (Shift alone is not enough for the recorder).
    pub fn names(self) -> Vec<&'static str>
        // Recorder order names: `Ctrl`, `Alt`, `Shift`, `Super`.
}
pub struct ParsedAccelerator { modifiers: Modifiers, vk: u32 }
    // An accelerator ready for `RegisterHotKey`.
pub fn key_vk(key: &str) -> Option<(u32, bool)>
    // Key name (lower-cased, aliases folded) → `(vk, implicit_shift)`, as Electron registers it (shell.md §8.2).
pub fn parse_accelerator(accel: &str) -> Option<ParsedAccelerator>
    // Parses an accelerator for registration: any number of modifiers and exactly one key.
pub fn character_key_vk(key: &str) -> Option<u32>
    // The VK the AltGr guard checks for a character key: letters, digits and OEM punctuation (US positions).
pub fn alt_gr_probe(accel: &str) -> Option<(u32, bool)>
    // When the AltGr guard applies (Ctrl and Alt, no Super, exactly one character key): `(vk, explicit_shift)`.
pub enum HotkeyFailureReason { InUse, Duplicate, Reserved, TypesCharacter(String) }
    // Why a hotkey was not registered.
impl HotkeyFailureReason {
    pub fn id(&self) -> &'static str
        // The reason id of the `hotkeys not registered: readClipboard=Ctrl+Alt+V (types-character), …` warn line (shell.md §8.5): `in-use` | `duplicate` | `reserved` | `types-character`.
}
pub fn sort_failures(failures: &mut [HotkeyFailure])
    // Puts failures in registration order (`HotkeyAction::ALL` index; each action fails at most once), as shell.md §8.5–§8.6 lists them.
pub fn failures_log_line(failures: &[HotkeyFailure]) -> String
    // The warn line's list: `readClipboard=Ctrl+Alt+V (types-character), stop=Ctrl+Alt+S (in-use)`.
pub struct HotkeyFailure { action: HotkeyAction, accelerator: String, reason: HotkeyFailureReason }
pub struct RegistrationPlan { register: Vec<(HotkeyAction, String, ParsedAccelerator)>, failures: Vec<HotkeyFailure> }
    // What to register after the pure checks.
pub fn plan_registration(triggers: &TriggerSettings, alt_gr: impl Fn(u32, bool) -> Option<String>) -> RegistrationPlan
    // The registration algorithm minus the OS call (shell.md §8.5): disabled → nothing; for each action in order: trim; empty → skip; reserved → failure (not "seen"); seen → duplicate; mark seen; AltGr conflict (via `alt_gr`, which returns the t…
pub fn describe_failures(failures: &[HotkeyFailure]) -> Option<String>
    // The failure toast line (shell.md §8.6), or `None` when nothing failed.
pub fn display_labels(accel: &str) -> Vec<String>
    // Keycap labels for an accelerator (ui-settings-widget.md §3.2); empty for `""`.
pub fn chord_label(accel: &str) -> String
    // Labels joined with `+` (`Ctrl+Insert` → `Ctrl+Ins`), used in the reserved-chord warnings.
pub fn spoken(accel: &str) -> String
    // Screen-reader form: `Ctrl plus Alt plus R`; a `+` key reads as `Plus`; empty → `Not set`.
pub fn tray_hint_ok(accel: &str) -> bool
    // Tray menus show an accelerator hint only for `^[A-Za-z0-9]+(\+[A-Za-z0-9]+)*$`.
pub fn row_warning(h: &HotkeySettings, action: HotkeyAction) -> Option<String>
    // Settings-row warning for a stored reserved chord, or a shared shortcut (persistent warnings, §2.5).
pub fn reserved_rejection(accel: &str) -> String
    // The transient message when the recorder produced a reserved chord (not saved).
pub enum RecordOutcome { Partial(Vec<String>), NeedsModifier(String), Unsupported, TypesCharacter { accelerator: String, ch: String }, Ok(String) }
    // Outcome of one key press while recording.
pub fn is_modifier_vk(vk: u32) -> bool
    // True for modifier virtual keys (Shift/Ctrl/Alt/Win, either side).
pub fn key_name_for_vk(vk: u32) -> Option<String>
    // Accelerator key name for a Windows virtual key (the layout-following VK is what `RegisterHotKey` uses).
pub fn is_standalone_key(name: &str) -> bool
    // Keys allowed without Ctrl/Alt/Win: `^(F([1-9]|1[0-9]|2[0-4])|Media.*|Volume.*|PrintScreen)$`.
pub fn record_key(vk: u32, mods: Modifiers, alt_gr_char: Option<String>) -> RecordOutcome
    // `acceleratorFromEvent` for a Win32 key press: `vk` from `WM_KEYDOWN/WM_SYSKEYDOWN`, held modifiers, and `alt_gr_char` = what Ctrl+Alt(+Shift)+key types on the active layout (`None` when nothing / not checked).
```

#### `vox_core::ids`

```rust
pub fn new_uuid() -> String
    // A new random UUID v4 in lower-case hyphenated form (`crypto.randomUUID()`), used for read ids, session ids and toast ids.
pub fn is_valid_read_id(id: &str) -> bool
    // True if `id` may name an audio-cache folder: `^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$` (persistence.md §6.1).
```

#### `vox_core::js`

```rust
pub fn js_round(x: f64) -> f64
    // `Math.round(x)`: rounds half toward +∞ (`-2.5 → -2`, `2.5 → 3`).
pub fn round1(x: f64) -> f64
    // `Math.round(x * 10) / 10`.
pub fn round3(x: f64) -> f64
    // `Math.round(x * 1000) / 1000`.
pub fn clamp_num(x: f64, min: f64, max: f64) -> f64
    // `Math.min(max, Math.max(min, x))` with NaN → `min` (reader-playback.md §0 `clampNum`).
pub fn is_js_whitespace(c: char) -> bool
    // True for the ECMAScript whitespace + line-terminator set matched by `\s` and stripped by `trim()`.
pub fn js_trim(s: &str) -> &str
    // `String.prototype.trim`.
pub fn js_trim_end(s: &str) -> &str
    // `String.prototype.trimEnd`.
pub fn js_trim_start(s: &str) -> &str
    // `String.prototype.trimStart`.
pub fn collapse_whitespace(s: &str) -> String
    // `s.replace(/\s+/g, ' ').trim()`: every whitespace run becomes one space, ends trimmed.
pub fn has_js_whitespace(s: &str) -> bool
    // True if the string contains any JS whitespace (`/\s/.test(s)`).
pub fn to_fixed(x: f64, digits: usize) -> String
    // `Number.prototype.toFixed(digits)` for finite `x` (digits ≤ 20).
pub fn number_to_string(x: f64) -> String
    // `String(x)` for a finite number: shortest round-trip form, integral values without a fraction, `-0` → `"0"`.
pub fn fixed_trimmed(x: f64, digits: usize) -> String
    // `Number(x.toFixed(digits))` rendered with [`number_to_string`]: `1.25 → "1.25"`, `1.0 → "1"`.
pub fn capitalize_first(s: &str) -> String
    // Upper-case the first character, leave the rest (`'abc' → 'Abc'`).
```

#### `vox_core::json`

```rust
pub fn strip_bom(s: &str) -> &str
    // Strips one leading U+FEFF (Notepad's BOM).
pub fn repair_lone_surrogates(s: &str) -> Cow<'_, str>
    // Replaces `\uD800`–`\uDFFF` escapes that are not part of a valid high+low pair with `�`.
pub enum LenientParseError { Empty, Invalid(serde_json::Error) }
    // Error from [`parse_lenient`].
impl std::fmt::Display for LenientParseError {
}
impl std::error::Error for LenientParseError {
}
pub enum OutOfRange { InfinityString, Null }
    // How [`parse_lenient_with`] treats number literals outside the f64 range (`1e400`).
pub fn parse_lenient(text: &str) -> Result<Value, LenientParseError>
    // Parses JSON text the way the old `readJsonFile` / bridge body parser did: BOM stripped, empty → error, lone surrogate escapes repaired, and number literals outside the f64 range (`1e400`) replaced by the strings `"Infinity"` / `"-Infinity"…
pub fn parse_lenient_with(text: &str, mode: OutOfRange) -> Result<Value, LenientParseError>
    // [`parse_lenient`] with an explicit [`OutOfRange`] policy.
pub fn quote_out_of_range_numbers(s: &str) -> Cow<'_, str>
    // Replaces number literals (outside strings) whose value overflows `f64` with `"Infinity"` / `"-Infinity"`.
pub fn replace_out_of_range_numbers(s: &str, mode: OutOfRange) -> Cow<'_, str>
    // Replaces number literals (outside strings) whose value overflows `f64` according to `mode`.
pub fn to_js_pretty<T: Serialize + ?Sized>(v: &T) -> String
    // `JSON.stringify(v, null, 2)`: 2-space indent, `"key": value`, no trailing newline.
pub fn to_js_compact<T: Serialize + ?Sized>(v: &T) -> String
    // `JSON.stringify(v)`: compact.
pub fn serialize_js_number<S: serde::Serializer>(v: f64, s: S) -> Result<S::Ok, S::Error>
    // Serializes an `f64` like `JSON.stringify`: integral values (|x| < 2^53) as integers, non-finite as `null`.
pub fn js_f64<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error>
    // `#[serde(serialize_with = "vox_core::json::js_f64")]`
pub fn js_opt_f64<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error>
    // `#[serde(serialize_with = "vox_core::json::js_opt_f64")]` (use with `skip_serializing_if = "Option::is_none"`).
pub fn js_f64_vec<S: serde::Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error>
    // `#[serde(serialize_with = "vox_core::json::js_f64_vec")]`
pub struct JsNumber(pub f64)
    // Wrapper that serializes an `f64` with [`serialize_js_number`].
impl Serialize for JsNumber {
}
pub mod lenient {
    // Lenient accessors over untrusted JSON (contracts.md §3: "finite" = a JSON number; booleans must be JSON booleans).
    pub fn obj(v: &Value) -> Option<&Map<String, Value>>
        // `v` as an object (`isObj`: not null, not an array).
    pub fn child<'a>(v: &'a Value, key: &str) -> Cow<'a>
        // The child object at `key`, or an empty map when absent / not an object (sanitize's `rec()`).
    pub enum Cow<'a> { Borrowed(&'a Map<String, Value>), Empty }
        // Borrowed-or-empty object view.
    impl<'a> Cow<'a> {
        pub fn get(&self, key: &str) -> Option<&'a Value>
    }
    pub fn number(v: Option<&Value>) -> Option<f64>
        // A JSON number (always finite in JSON).
    pub fn boolean(v: Option<&Value>) -> Option<bool>
        // A JSON boolean.
    pub fn string(v: Option<&Value>) -> Option<&str>
        // A JSON string.
}
```

#### `vox_core::listen_log`

```rust
pub struct ListenLog(pub BTreeMap<String, f64>)
    // `{"YYYY-MM-DDTHH": seconds}`; a `BTreeMap` so keys serialize ascending (lexicographic = chronological).
impl ListenLog {
    pub fn new() -> Self
    pub fn is_empty(&self) -> bool
    pub fn len(&self) -> usize
    pub fn get(&self, key: &str) -> Option<f64>
    pub fn iter(&self) -> impl Iterator<Item = (&String, &f64)>
    pub fn total(&self) -> f64
        // Sum of all entries.
}
impl Serialize for ListenLog {
}
pub fn hour_key<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
    // `"{year}-{MM}-{DD}T{HH}"` of a local date-time.
pub fn hour_key_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> String
    // Hour key of epoch milliseconds in `tz`.
pub fn parse_hour_key(key: &str) -> Option<NaiveDateTime>
    // Parses `^(\d{4})-(\d{2})-(\d{2})T(\d{2})$` (ASCII digits) into a local wall-clock hour.
pub fn hour_key_date<Tz: TimeZone>(key: &str, tz: &Tz) -> Option<DateTime<Tz>>
    // The local start of an hour key, or `None` for a malformed key.
pub fn sanitize_listen_log(v: &Value) -> Option<ListenLog>
    // Normalises an untrusted ledger: must be a JSON object; keeps entries whose value is a finite number > 0 and whose key parses; values rounded to 0.1; at most [`LISTEN_LOG_MAX_KEYS`] (newest kept).
pub fn sanitize_log(log: &ListenLog) -> Option<ListenLog>
    // Same rules for an in-memory ledger.
pub fn add_to_listen_log(log: Option<&ListenLog>, key: &str, sec: f64) -> ListenLog
    // A copy of `log` with `sec` more seconds at `key` (a plain copy for `sec <= 0`); never `None` (empty map instead).
pub fn same_listen_log(a: Option<&ListenLog>, b: Option<&ListenLog>) -> bool
    // Same key count and every value equal; absent and empty compare equal.
pub fn listening_entries<Tz: TimeZone>(tz: &Tz, created_at_ms: f64, listened_sec: f64, log: Option<&ListenLog>) -> Vec<(DateTime<Tz>, f64)>
    // When a record's listening happened: one entry per ledger hour (scaled down if the ledger says more than `listened_sec`), plus the uncovered remainder (> 0.05 s) at `created_at_ms`.
```

#### `vox_core::locale`

```rust
pub enum DateOrder { MonthFirst, DayFirst }
    // Day/month order in dates with names (`October 17` vs `17 October`).
pub struct Locale { decimal: String, group: String, hour12: bool, date_order: DateOrder }
    // Number and date conventions.
impl Locale {
    pub const EN_US: Locale = Locale { decimal: String::new(), group: String::new(), hour12: true, date_order: DateOrder::Month…
        // The default and the test locale.
    pub fn decimal_sep(&self) -> &str
        // `"."`/`","` with en-US defaults for empty fields (`EN_US` is `const`, so its separators are stored empty and resolved here).
    pub fn group_sep(&self) -> &str
        // The grouping separator; en-US `,` when unset.
    pub const NO_GROUPING: &'static str = "\u{0}"
        // Marker for "no thousands separator" in [`Locale::group`].
    pub fn new(decimal: &str, group: &str, hour12: bool, date_order: DateOrder) -> Locale
        // A locale from raw conventions (what `vox_win::system::user_locale` returns).
    pub fn group_digits(&self, digits: &str) -> String
        // Integer digits with grouping: `1234567 → "1,234,567"` (en-US) / `"1.234.567"` (de-DE).
    pub fn localize_decimal(&self, ascii: &str) -> String
        // Replaces the `.` of an ASCII decimal string (`"38.8"`) with this locale's decimal separator.
}
impl Default for Locale {
}
pub fn set_process_locale(locale: Locale)
    // Installs the user's conventions for the whole process (vox-app at startup and on locale change).
pub fn current() -> Locale
    // The conventions in effect on this thread: a [`with_locale`] override, else the process locale, else en-US.
pub fn with_locale<R>(locale: Locale, f: impl FnOnce() -> R) -> R
    // Runs `f` with `locale` as this thread's conventions (tests; deterministic regardless of other threads).
```

#### `vox_core::player`

```rust
pub enum PlayerStatus { Idle, Loading, Playing, Paused, Ended, Error }
    // Player status.
impl PlayerStatus {
    pub fn as_str(self) -> &'static str
    pub fn is_active(self) -> bool
        // loading / playing / paused: a read is in progress (tray "Stop" enabled, credits refresh on settle).
    pub fn is_settled(self) -> bool
        // idle / ended / error.
}
pub struct PlayerState { status: PlayerStatus, session_id: Option<String>, read_id: Option<String>, title: String, voice_name: String, position_sec: f64, duration_sec: f64, duration_is_estimate: bool, buffered_fraction: f64, rate: f64, volume: f64, char_index: u32, total_chars: u32, listened_sec: f64, source_app: Option<String>, error: Option<String>, error_code: Option<ReadErrorCode> }
    // The player's state (serialized order of `main/ipc.ts:150-171`).
impl PlayerState {
    pub fn idle() -> Self
        // `IDLE_PLAYER_STATE`.
}
impl Default for PlayerState {
}
pub struct SessionInfo { id: String, read_id: String, text: Arc<str>, title: String, voice_name: String, voice_id: String, source_app: Option<String>, from_cache: bool, highlight_words: bool, sentences: Arc<[u32]> }
    // Session info that is stable for the session (cheap to clone through `Arc`).
pub struct SessionWarning { message: String, code: ReadErrorCode }
    // A non-fatal session error: the read will stop before the end.
pub struct PlayerSnapshot { state: PlayerState, session: Option<Arc<SessionInfo>>, wants_play: bool, buffering: bool, starting: bool, warning: Option<SessionWarning>, partial_text: bool }
    // Everything the UI renders about playback.
pub enum SkipDirection { Back, Forward }
    // `skip.direction`.
impl SkipDirection {
    pub fn sign(self) -> f64
}
pub enum SkipUnit { Sentence, Seconds }
    // `skip.unit`.
pub enum PlayerCommand { Toggle, Play, Pause, Stop, Seek { position_sec: f64 }, SeekFraction { fraction: f64 }, Skip { direction: SkipDirection, unit: SkipUnit, amount: Option<f64> }, Rate { rate: f64 }, Volume { volume: f64 } }
    // A transport command (from the UI, tray, hotkeys, media keys or the bridge).
impl PlayerCommand {
    pub fn next_sentence() -> Self
    pub fn previous_sentence() -> Self
}
pub fn parse_player_command(v: &Value) -> Option<PlayerCommand>
    // `parsePlayerCommand` (main/ipc.ts:118-144): lenient — anything invalid is dropped (`None`); a bad `amount` is ignored.
pub fn parse_bridge_command(v: &Value) -> Result<PlayerCommand, String>
    // The bridge's stricter parser (local-server.ts:320-352): an invalid command is an error with the exact HTTP-400 message; an `amount` that is present but not a finite number (including `null`) is an error.
pub fn sanitize_player_command(cmd: PlayerCommand) -> Option<PlayerCommand>
    // Validates an already-typed command from any in-process source (contracts.md §5.1: "keep these validators in Rust even in-process"; ARCHITECTURE.md §3.2).
pub fn next_rate(rate: f64) -> f64
    // The widget's speed cycle: first preset `> rate + 0.001`, else wraps to 0.75.
```

#### `vox_core::read`

```rust
pub enum ReadErrorCode { NoApiKey, Unauthorized, Quota, RateLimit, Network, EmptyText, NoSelection, TooLong, NotCached, Unknown }
    // Why a read failed.
impl ReadErrorCode {
    pub const ALL: [ReadErrorCode; 10] = [ Self::NoApiKey, Self::Unauthorized, Self::Quota, Self::RateLimit, Self::Network, Self::EmptyTex…
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn default_message(self) -> Option<&'static str>
        // The generic per-code fallback (reader.ts `ERROR_MESSAGES`), used only when an error has no message.
    pub fn is_settings_error(self) -> bool
        // Codes the user fixes in Settings → Account.
    pub fn is_retry_error(self) -> bool
        // Codes the widget offers "Try again" for.
}
pub mod eleven_messages {
    // Exact user-facing ElevenLabs messages (tts.md §2.4.2).
    pub const NO_KEY: &str = "Add your ElevenLabs API key in Settings to start reading"
    pub const BAD_KEY: &str = "Your ElevenLabs key was rejected \u{2014} update it in Settings"
    pub const QUOTA: &str = "You’re out of ElevenLabs credits \u{2014} top up your plan or wait for the monthly reset"
    pub const RATE_LIMIT: &str = "ElevenLabs is busy right now \u{2014} try again in a moment"
    pub const CONCURRENCY: &str = "Too many requests at once for your ElevenLabs plan \u{2014} try again in a moment"
    pub const NETWORK: &str = "Couldn’t reach ElevenLabs \u{2014} check your internet connection"
    pub const TIMEOUT: &str = "ElevenLabs took too long to respond \u{2014} try again"
    pub const SERVER: &str = "ElevenLabs is having trouble right now \u{2014} try again in a moment"
    pub const VOICE_MISSING: &str = "That voice is no longer available \u{2014} pick another one in Voices"
    pub const VOICE_DENIED: &str = "That voice isn’t available on your ElevenLabs plan \u{2014} pick another one in Voices"
    pub const MODEL: &str = "The selected model isn’t available for your key \u{2014} choose another in Settings"
    pub const TOO_LONG: &str = "This passage is too long for the selected model"
    pub const BAD_RESPONSE: &str = "ElevenLabs sent a response that could not be read"
    pub const NO_AUDIO: &str = "ElevenLabs returned no audio"
    pub const CANCELLED: &str = "Cancelled"
    pub const PASTE_KEY_FIRST: &str = "Paste your ElevenLabs API key first"
        // Key validation copy (tts.md §2.11).
    pub const KEY_HAS_SPACES: &str = "That doesn’t look like an API key \u{2014} it contains spaces"
    pub const KEY_TOO_LONG: &str = "That doesn’t look like an API key"
    pub const KEY_NOT_ACCEPTED: &str = "ElevenLabs didn’t accept this key \u{2014} check that you copied all of it"
    pub const KEY_RESTRICTED_WARNING: &str = "This key can’t read your account usage, so credits won’t be shown. Reading works as long as it h…
    pub const NO_TEXT: &str = "There is no text to read"
        // Engine-level messages.
    pub const CACHED_AUDIO_MISSING: &str = "The saved audio for this read is missing"
    pub const ENGINE_CRASHED: &str = "Something went wrong while preparing the audio"
    pub const PART_FAILED: &str = "Part of the text could not be synthesized"
    pub fn permission(what: &str) -> String
        // `Your ElevenLabs key isn't allowed to use {what} — enable it for the key on elevenlabs.io, or use another key`.
}
pub fn error_message(code: ReadErrorCode, fallback: Option<&str>) -> String
    // `errorMessage(code, fallback)`: the error's own message trimmed if non-empty, else the per-code default, else `Something went wrong while reading`.
pub fn error_needs_settings(code: ReadErrorCode, message: &str, missing_permission: bool) -> bool
    // `errorNeedsSettings`: `no-api-key` and `quota`; `unauthorized` only when not a permission problem and the message is exactly the bad-key message.
pub struct ReadResult { ok: bool, read_id: Option<String>, error: Option<String>, code: Option<ReadErrorCode> }
    // Result of starting a read.
impl ReadResult {
    pub fn started(read_id: impl Into<String>) -> Self
    pub fn failed(code: ReadErrorCode, error: impl Into<String>) -> Self
    pub fn failed_with_id(read_id: impl Into<String>, code: ReadErrorCode, error: impl Into<String>) -> Self
    pub fn no_api_key() -> Self
        // The canonical failure results (contracts.md §5.3).
    pub fn nothing_to_read() -> Self
    pub fn clipboard_empty() -> Self
    pub fn not_in_history() -> Self
    pub fn not_cached(read_id: impl Into<String>) -> Self
    pub fn still_starting() -> Self
    pub fn internal_error() -> Self
        // The answer to a read request whose core handler panicked (ARCHITECTURE.md §8 D-ARCH-3): `unknown`, `Something went wrong`.
}
pub struct VoiceOverride { id: String, name: String }
    // Overrides the configured voice (replays that re-synthesize).
pub struct ReadRequest { text: String, source: ReadSource, source_app: Option<String>, source_process: Option<String>, source_title: Option<String>, source_url: Option<String>, voice: Option<VoiceOverride> }
    // What the reader is asked to read.
impl ReadRequest {
    pub fn new(text: impl Into<String>, source: ReadSource) -> Self
}
pub struct ReadTextOptions { source: Option<ReadSource>, source_app: Option<String>, source_title: Option<String>, source_url: Option<String> }
    // Options for a read typed or pasted in the app (`reader:read-text`).
impl ReadTextOptions {
    pub fn into_request(self, text: &str) -> ReadRequest
        // Builds the reader request with the IPC-edge rules: text cut to 2 000 000 units; default source `manual`; `sourceApp` cut to 120 (not trimmed); `sourceTitle` cut to 300; `sourceUrl` kept only if [`is_http_url`].
}
pub struct ForegroundInfo { process_name: String, app_name: String, title: String, pid: u32 }
    // The app a selection came from.
pub const KNOWN_BROWSER_PROCESSES: [&str; 9] = ["chrome.exe", "msedge.exe", "brave.exe", "opera.exe", "vivaldi.exe", "arc.exe", "chromium.exe",…
    // Processes whose foreground window labels a bridge read with the browser's own name.
pub struct BridgeReadRequest { text: String, url: Option<String>, title: Option<String> }
    // A validated `POST /read` body (integrations.md §1.6.2): `url` already matched `^(https?|file)://` and ≤ 2048, `title` trimmed, ≤ 300, `None` if blank.
impl BridgeReadRequest {
    pub fn into_request(self, foreground: Option<&ForegroundInfo>) -> ReadRequest
        // The reader request (index.ts:226-245): source `browser-extension`; `sourceApp` = the foreground app only if it is a known browser, else `"Browser"`; `sourceUrl` only for http(s) URLs ≤ 4096.
}
pub fn is_http_url(s: &str) -> bool
    // `isHttpUrl`: a string ≤ 4096 units that parses as an absolute URL with scheme `http` or `https`.
```

#### `vox_core::settings`

```rust
pub enum ThemeMode { System, Paper, Night }
    // `app.theme`.
impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Paper, ThemeMode::Night]
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn label(self) -> &'static str
        // Settings → Appearance label.
}
pub enum ResolvedTheme { Paper, Night }
    // The theme actually shown.
impl ResolvedTheme {
    pub fn is_dark(self) -> bool
}
pub fn resolve_theme(mode: ThemeMode, os_dark: bool) -> ResolvedTheme
    // `system` follows the OS app theme (dark → night).
pub enum WidgetPosition { BottomRight, TopRight, BottomCenter, NearCursor, Remember }
    // `widget.position`.
impl WidgetPosition {
    pub const ALL: [WidgetPosition; 5] = [ WidgetPosition::BottomRight, WidgetPosition::TopRight, WidgetPosition::BottomCenter, WidgetPosi…
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn label(self) -> &'static str
        // Segmented-control label (ui-settings-widget.md §2.6).
    pub fn accessible_label(self) -> &'static str
        // Accessible label.
    pub fn hint(self) -> &'static str
        // Row description shown for the current position.
}
pub enum OutputFormat { Mp3_44100_128, Mp3_44100_64, Mp3_22050_32 }
    // `outputFormat` (ElevenLabs `output_format`).
impl OutputFormat {
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Mp3_44100_128, OutputFormat::Mp3_44100_64, OutputFormat::Mp3_22050_32]
    pub fn as_str(self) -> &'static str
    pub fn parse(s: &str) -> Option<Self>
    pub fn label(self) -> &'static str
        // "Audio quality" option label.
    pub fn kbps(self) -> u32
        // Constant MP3 bitrate (used for byte-size durations, tts.md §4.8).
    pub fn mime(self) -> &'static str
        // MIME of the audio ElevenLabs returns for this format.
}
pub struct VoiceSettings { stability: f64, similarity_boost: f64, style: f64, use_speaker_boost: bool, speed: f64 }
    // ElevenLabs server-side voice settings.
impl Default for VoiceSettings {
}
impl VoiceSettings {
    pub fn is_default(&self) -> bool
        // All five values equal the defaults (the "Reset" button is disabled).
}
pub struct HotkeySettings { read_selection: String, read_clipboard: String, play_pause: String, stop: String }
    // `triggers.hotkeys`: Electron accelerator strings, `""` = disabled.
impl Default for HotkeySettings {
}
impl HotkeySettings {
    pub fn get(&self, action: crate::hotkey::HotkeyAction) -> &str
        // The accelerator for an action (raw, as stored).
}
pub struct TriggerSettings { right_click_chip: bool, selection_popup: bool, hotkeys_enabled: bool, hotkeys: HotkeySettings, media_keys: bool, browser_extension: bool, explorer_menu: bool, excluded_apps: Vec<String>, restore_clipboard: bool }
impl Default for TriggerSettings {
}
pub struct WidgetSettings { enabled: bool, position: WidgetPosition, custom_x: Option<i32>, custom_y: Option<i32>, auto_hide: bool, highlight_words: bool, compact: bool }
impl Default for WidgetSettings {
}
pub struct AppSettings { launch_at_login: bool, start_minimized: bool, close_to_tray: bool, theme: ThemeMode }
impl Default for AppSettings {
}
pub struct LibrarySettings { save_history: bool, store_text: bool, cache_audio: bool, cache_limit_mb: u32, retention_days: u32 }
impl Default for LibrarySettings {
}
impl LibrarySettings {
    pub fn cache_limit_bytes(&self) -> u64
        // Cache limit in bytes (`cacheLimitMB × 1 000 000`).
}
pub struct TextSettings { skip_urls: bool, skip_emails: bool, max_chars: u32 }
impl Default for TextSettings {
}
pub struct ServerSettings { port: u16 }
impl Default for ServerSettings {
}
pub struct Settings { version: u32, has_api_key: bool, onboarded: bool, voice_id: String, voice_name: String, model_id: String, output_format: OutputFormat, voice: VoiceSettings, language_code: String, playback_rate: f64, volume: f64, triggers: TriggerSettings, widget: WidgetSettings, app: AppSettings, library: LibrarySettings, text: TextSettings, server: ServerSettings }
    // The whole settings object.
impl Default for Settings {
}
pub fn sanitize_language_code(v: Option<&Value>, fb: &str) -> String
    // `languageCode`: trimmed + lower-cased; `""` allowed; else must match `^[a-z]{2,3}([-_][a-z0-9]{2,8})?$`.
pub fn normalize_excluded_apps(v: Option<&Value>, fb: &[String]) -> Vec<String>
    // `normalizeExcludedApps`: not an array → fallback.
impl Settings {
    pub fn sanitize(input: &Value, base: &Settings, has_api_key: bool) -> Settings
        // Rebuilds a complete, valid `Settings` from untrusted JSON.
    pub fn from_stored(raw: &Value, has_api_key: bool) -> Settings
        // Settings loaded from disk: sanitized against the defaults.
    pub fn to_value(&self) -> Value
        // The stored form as a JSON value (no `hasApiKey`).
    pub fn stored_json(&self) -> String
        // `settings.json` text: `JSON.stringify(stored, null, 2)`.
    pub fn needs_rewrite(&self, raw: &Value) -> bool
        // True if the file a value was loaded from differs textually (compact JSON) from what we would write, so a rewrite should be scheduled (persistence.md §3.4: migration = normalise + rewrite once).
    pub fn apply_patch(&self, patch: &Value) -> Settings
        // `update(patch)`: `sanitize(deepMerge(current, patch), base = current)`.
}
pub fn deep_merge(base: &Value, patch: &Value) -> Value
    // `deepMerge(base, patch)`: plain objects merge recursively; arrays, `null` and primitives replace; keys `__proto__`, `constructor`, `prototype` are ignored.
pub fn patch_at(path: &str, value: Value) -> Value
    // Builds a nested JSON patch from a dotted key path: `patch_at("triggers.hotkeys.stop", json!(""))` → `{"triggers":{"hotkeys":{"stop":""}}}`.
pub fn quantize(value: f32, step: f64) -> f64
    // Snaps a Slint slider value (`float` = `f32`) to its step before it becomes a settings patch or a player command (contracts.md §3.2/§4.1 slider steps; ARCHITECTURE.md §7.1): `js_round(value / step) × step`, then rounded to the step's number…
pub fn hotkeys_changed(a: &Settings, b: &Settings) -> bool
    // Whether hotkey registration must re-run: `hotkeysEnabled` or any of the four **raw** strings changed.
pub const SETTINGS_SECTIONS: [(&str, &str); 9] = [ ("account", "Account"), ("voice", "Voice & model"), ("playback", "Playback"), ("triggers", "Tri…
    // Settings section ids, in nav order, with their labels (ui-settings-widget.md §2.1).
pub fn is_settings_section(id: &str) -> bool
    // True for a known settings section id.
pub fn playback_rate_options(current: f64) -> Vec<f64>
    // Playback-speed options: the presets plus the current rate when it is finite and not within 0.001 of a preset; ascending.
pub fn rate_option_index(options: &[f64], current: f64) -> Option<usize>
    // Index of the option within 0.001 of `current`, if any.
pub fn rate_accessible_label(rate: f64) -> String
    // Accessible label `"1.25 times speed"`.
pub const MAX_CHAR_PRESETS: [u32; 6] = [2_000, 5_000, 10_000, 20_000, 50_000, 100_000]
pub fn max_chars_options(current: u32) -> Vec<u32>
    // "Longest selection" options: presets plus the current value if different, ascending.
pub fn max_chars_label(n: u32) -> String
    // `20,000 characters`.
pub const RETENTION_PRESETS: [(u32, &str); 5] = [(0, "Keep forever"), (7, "7 days"), (30, "30 days"), (90, "90 days"), (365, "1 year")]
    // Retention presets: (days, label).
pub fn retention_options(current: u32) -> Vec<(u32, String)>
    // Retention options plus the current custom value as `"{n} days"`.
pub fn retention_label(days: u32) -> String
    // `'1\u{a0}year'`, `'7\u{a0}days'`, `'1\u{a0}day'` (no-break space).
pub fn is_stricter_retention(next: u32, current: u32) -> bool
    // Switching retention from `current` to `next` can delete reads right away.
pub fn normalize_exe_name(input: &str) -> Option<String>
    // The Settings page's strict exe-name check (ui-settings-widget.md §2.5): `None` = invalid.
pub fn add_excluded_app(list: &[String], input: &str) -> Result<Vec<String>, String>
    // Adds a program to the excluded list; `Err` carries the UI error text.
pub fn parse_port(text: &str) -> Option<u16>
    // A user-typed port: trimmed `^\d{1,5}$` within 1024..65535.
pub fn cost_label(factor: Option<f64>) -> Option<String>
    // `{n}× credits` for a cost factor (`None` when unknown).
pub fn model_options(models: &[TtsModel], current: &str) -> Vec<(String, String)>
    // Model select options `(value, label)`; the current id is prepended (prettified) when the list lacks it.
pub fn language_options(models: &[TtsModel], model_id: &str, current: &str) -> Vec<(String, String)>
    // Language options: `('', "Auto-detect")`, then the model's languages sorted by name; an unknown current code is appended upper-cased.
pub fn model_change_patch(models: &[TtsModel], new_model: &str, current_language: &str) -> Value
    // The patch written when the model changes: also resets a pinned language the new (known) model lacks.
pub fn model_badges(model: &TtsModel) -> Vec<(String, bool)>
    // Meta badges under the model row: cost (flagged when > 1), max chars per request, language count.
```

#### `vox_core::text16`

```rust
pub fn utf16_len(s: &str) -> usize
    // Number of UTF-16 code units in `s` (JS `s.length`).
pub fn byte_index(s: &str, off16: usize) -> usize
    // Byte index in `s` of UTF-16 offset `off16`, clamped to the end; an offset inside a surrogate pair maps to the start of that character.
pub fn utf16_offset(s: &str, b: usize) -> usize
    // UTF-16 offset of byte index `b` (which must be a char boundary; clamped to the end).
pub fn slice16(s: &str, start: usize, end: usize) -> &str
    // `s.slice(start, end)` in UTF-16 units, both ends floored to character starts.
pub fn prefix16(s: &str, max: usize) -> &str
    // The first `max` UTF-16 units of `s`, never splitting a surrogate pair (JS `slice(0, max)` minus a dangling high surrogate).
pub fn splits_pair(s: &str, off16: usize) -> bool
    // True if UTF-16 offset `off16` falls between the two halves of a surrogate pair in `s`.
pub struct Utf16Index
    // Precomputed UTF-16 ↔ byte mapping for one long text (a session text is converted many times per second when the widget highlights words).
impl Utf16Index {
    pub fn new(s: &str) -> Self
        // Builds the index for `s` in one pass.
    pub fn len16(&self) -> usize
        // UTF-16 length of the indexed text.
    pub fn byte_of(&self, off16: usize) -> usize
        // Byte index of UTF-16 offset `off16` (floored to a char start, clamped to the end).
    pub fn utf16_of(&self, b: usize) -> usize
        // UTF-16 offset of byte index `b` (floored to a char start).
    pub fn slice<'a>(&self, text: &'a str, start16: usize, end16: usize) -> &'a str
        // `text[start16..end16]` using this index (`text` must be the string the index was built from).
}
```

#### `vox_core::time`

```rust
pub fn from_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> DateTime<Tz>
    // Epoch milliseconds → zoned date-time.
pub fn iso_utc_ms(ms: f64) -> String
    // JS `new Date(ms).toISOString()`: UTC, millisecond precision, `Z` — `2026-10-17T09:44:00.000Z` (contracts.md §3.9 `exportedAt`; persistence.md §9 log lines).
pub fn to_ms<Tz: TimeZone>(dt: &DateTime<Tz>) -> f64
    // Zoned date-time → epoch milliseconds.
pub fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> DateTime<Tz>
    // A local wall-clock time as an instant: gaps move forward (by 1 h, else 30 min, else 2 h), ambiguity takes the earlier instant.
pub fn local_midnight<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> DateTime<Tz>
    // Local midnight (start of day) of `date`.
pub fn day_key_of_date(date: NaiveDate) -> String
    // `YYYY-MM-DD`.
pub fn day_key<Tz: TimeZone>(dt: &DateTime<Tz>) -> String
    // Local `YYYY-MM-DD` of `dt`.
pub fn day_key_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> String
    // Local `YYYY-MM-DD` of epoch milliseconds.
pub fn parse_day_key(key: &str) -> Option<NaiveDate>
    // Parses `^\d{4}-\d{2}-\d{2}$` into a calendar date (invalid dates → `None`).
pub fn add_days(date: NaiveDate, n: i64) -> NaiveDate
    // `date + n` calendar days.
pub fn calendar_days_between(earlier: NaiveDate, later: NaiveDate) -> i64
    // Whole calendar days from `earlier` to `later` (negative when `later` is before).
pub fn start_of_day_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> f64
    // Epoch ms of the local midnight that starts the day containing `ms`.
pub fn local_hour<Tz: TimeZone>(dt: &DateTime<Tz>) -> u32
    // Local hour (0–23) of `dt`.
pub fn monday_index<Tz: TimeZone>(dt: &DateTime<Tz>) -> usize
    // Monday-first weekday index (0 = Monday … 6 = Sunday), JS `(getDay() + 6) % 7`.
```

#### `vox_core::timing`

```rust
pub fn last_index_at_or_before<T: PartialOrd + Copy>(arr: &[T], x: T) -> Option<usize>
    // Index of the last element `<= x` in an ascending slice, or `None`.
pub struct ChunkTiming<'a> { text_start: u32, text_end: u32, duration_sec: f64, starts: Option<&'a [f64]> }
    // Timing view of one chunk.
impl<'a> ChunkTiming<'a> {
    pub fn char_at_time(&self, t: f64) -> u32
        // Absolute character spoken at `t` seconds into the chunk.
    pub fn time_at_char(&self, ch: u32) -> f64
        // Inverse: the start time of character `ch` (guarantees `char_at_time(time_at_char(c)) >= c`).
}
pub fn sentence_at(sentences: &[u32], text_len: u32, char_index: u32) -> (usize, u32, u32)
    // The sentence containing `char_index`: `(index, start, end)` with `end = next start or text_len`.
```

#### `vox_core::tts`

```rust
pub type AudioBytes = Arc<[u8]>
    // Encoded audio bytes (a complete MP3 or WAV file for one chunk), shared between the sink and the cache.
pub const MIME_MPEG: &str = "audio/mpeg"
pub const MIME_WAV: &str = "audio/wav"
pub struct Alignment { chars: Vec<String>, starts: Vec<f64>, ends: Vec<f64> }
    // Character timing for one chunk; seconds relative to the chunk's audio start.
impl Alignment {
    pub fn is_consistent(&self) -> bool
        // All three arrays have equal length.
    pub fn last_end(&self) -> f64
        // `max(0, max(ends))`.
    pub fn redacted(&self) -> Alignment
        // Same timing with every char replaced by `""` (preview-only records keep no text next to the audio).
    pub fn has_text(&self) -> bool
        // Some char is non-empty.
    pub fn usable_for_timing(&self) -> bool
        // The player's validity rule: non-empty starts, all finite.
}
pub struct TextSpan { start: u32, end: u32 }
    // A `[start, end)` range in UTF-16 units.
impl TextSpan {
    pub fn new(start: u32, end: u32) -> Self
    pub fn len(&self) -> u32
    pub fn is_empty(&self) -> bool
}
pub struct TextChunk { text: String, start: u32, end: u32 }
    // A synthesis chunk: `text == input[start..end)` in UTF-16 units.
pub struct TtsSessionStart { session_id: String, read_id: String, text: Arc<str>, title: String, voice_name: String, voice_id: String, source_app: Option<String>, estimated_duration_sec: f64, chunk_count: u32, rate: f64, volume: f64, highlight_words: bool, from_cache: bool, sentences: Arc<[u32]> }
    // A new session for the player; any previous session is superseded.
pub struct TtsChunk { session_id: String, index: u32, text_start: u32, text_end: u32, audio: AudioBytes, mime: String, alignment: Option<Arc<Alignment>>, duration_sec: f64 }
    // One synthesized chunk, delivered in index order 0, 1, 2…
pub struct TtsSessionDone { session_id: String, total_chunks: u32 }
    // Every chunk delivered (also after a non-fatal failure, with the number actually delivered).
pub struct TtsSessionError { session_id: String, message: String, code: ReadErrorCode, fatal: bool }
    // `fatal`: session aborted before any audio.
pub struct CachedChunk { index: u32, text_start: u32, text_end: u32, mime: String, alignment: Option<Alignment>, duration_sec: f64, audio: AudioBytes }
    // A chunk as kept in the audio cache and replayed for free.
pub fn ext_for_mime(mime: &str) -> &'static str
    // Chunk file extension from a MIME (case-insensitive substring): mpeg|mp3 → mp3, wav|wave → wav, ogg|opus → ogg, else bin.
```

#### `vox_core::voices`

```rust
pub struct Voice { voice_id: String, name: String, labels: Vec<(String, String)>, category: Option<String>, description: Option<String>, preview_url: Option<String> }
    // A voice.
impl Voice {
    pub fn label(&self, key: &str) -> Option<&str>
        // A label value (raw, untrimmed), `None` when absent.
    pub fn label_trimmed(&self, key: &str) -> &str
        // `(labels[key] ?? '').trim()`.
}
pub struct ModelLanguage { id: String, name: String }
    // One language of a model.
pub struct TtsModel { model_id: String, name: String, description: Option<String>, languages: Vec<ModelLanguage>, max_chars: Option<f64>, cost_factor: Option<f64> }
    // A TTS-capable model.
pub struct AccountInfo { tier: String, character_count: f64, character_limit: f64, next_reset_unix: Option<f64>, status: Option<String> }
    // `GET /v1/user/subscription` summary.
pub struct ApiKeyResult { ok: bool, error: Option<String>, warning: Option<String>, account: Option<AccountInfo> }
    // Result of validating and storing an API key (`settings:set-api-key`, contracts.md §5.3).
pub struct CreditSummary { used: f64, limit: f64, remaining: f64, fraction_left: f64, reset_in_days: Option<i64> }
    // `creditSummary()` (ui-settings-widget.md §2.2).
pub fn credit_summary(a: &AccountInfo, now_ms: f64) -> CreditSummary
    // Derives the numbers every credits view shows.
pub fn title_case(s: &str) -> String
    // `'creator' → 'Creator'`, `'starter_plus' → 'Starter plus'`: `[_-]+` → space, trimmed, first char upper-cased.
pub fn sort_voices_for_page(voices: &[Voice]) -> Vec<Voice>
    // Voices-page order: own voices first (cloned, professional, generated, other, premade), then by name (case-insensitive; decision: plain lower-case compare instead of ICU `localeCompare`).
pub struct VoiceFilter { query: String, gender: String, accent: String, category: String }
    // Voices-page filter (`''` = any).
impl VoiceFilter {
    pub fn is_active(&self) -> bool
}
pub fn filter_voices(voices: &[Voice], f: &VoiceFilter) -> Vec<Voice>
    // Filters by gender/accent/category (normalised equality) and search terms (every term a substring of the name, description, category and all label values).
pub struct VoiceFacets { genders: Vec<String>, accents: Vec<String>, categories: Vec<String> }
    // Distinct filter values (first-seen spelling), sorted case-insensitively.
pub fn voice_facets(voices: &[Voice]) -> VoiceFacets
pub const CAPTION_LABEL_KEYS: [&str; 2] = ["use_case", "use case"]
    // Label keys the caption already shows.
pub fn voice_tags(v: &Voice, max: usize, skip: &[&str]) -> Vec<String>
    // Short descriptive tags, most useful first, de-duplicated case-insensitively, at most `max`.
pub fn voice_kind(v: &Voice) -> String
    // `'Cloned'`, `'Designed'` (generated), `'Professional'`, or `''` for stock voices.
pub fn voice_caption(v: &Voice) -> String
    // `"Cloned · Narration"`: kind and use case, empty parts dropped.
pub fn voice_subtitle(v: &Voice) -> String
    // New-design row subtitle (DESIGN.md Voices: "Male · American · Narration"): gender, accent, use case title-cased; falls back to the kind, the category, then `"Voice"`.
```


### A.2 `vox-text`

#### `vox_text::chunk`

```rust
pub fn chunk_text(text: &str, first_max: usize, max: usize) -> Vec<TextChunk>
    // Chunks `text`; `first_max` is raised to ≥ 20 and `max` to ≥ `first_max`.
pub fn chunk_text_default(text: &str) -> Vec<TextChunk>
    // `chunk_text(text, 220, 900)`: what the engine always uses.
pub fn split_span(text: &str, span: TextSpan, limit: usize) -> (TextSpan, Option<TextSpan>)
    // `splitSpan(text, span, limit)`: `(head, rest)` for a span longer than `limit` (tts.md §3.8).
```

#### `vox_text::clean`

```rust
pub struct CleanOptions { skip_urls: bool, skip_emails: bool }
    // The `settings.text` switches `cleanText` reads.
impl From<&TextSettings> for CleanOptions {
}
pub fn clean_text(text: &str, opts: CleanOptions) -> String
    // Cleans `text` for speech (tts.md §3.1 steps 1–11).
```

#### `vox_text::extract`

```rust
pub enum ExtractError { NotAFile, Binary, Io(std::io::Error) }
    // Why a file could not be read.
pub fn read_text_file(path: &Path, max_chars: usize) -> Result<String, ExtractError>
    // `readTextFile(path, maxChars)`.
pub fn decode_text_buffer(bytes: &[u8], truncated: bool) -> String
    // `decodeTextBuffer(bytes, truncated)` (integrations.md §5.2).
pub fn looks_binary(text: &str) -> bool
    // `looksBinary(text)` (integrations.md §5.3).
pub fn html_to_text(html: &str) -> String
    // `htmlToText` including entities and tidy (integrations.md §5.4–§5.5).
pub fn rtf_to_text(rtf: &str) -> String
    // `rtfToText` (integrations.md §5.6).
pub fn markdown_to_text(md: &str) -> String
    // `markdownToText` (integrations.md §5.7).
pub fn truncate_extracted(text: &str, max_chars: usize) -> String
    // The extractor's own truncation (integrations.md §5.9; differs from the reader's `truncate_text`).
```

#### `vox_text::highlight`

```rust
pub enum WordState { Spoken, Current, Upcoming }
    // Look of one word.
pub struct SentenceView { spoken: String, current: String, upcoming: String, sentence_index: usize, clipped_start: bool }
    // What the widget body shows.
pub fn sentence_view(text: &str, index: &Utf16Index, sentences: &[u32], char_index: u32, started: bool, window_units: usize) -> SentenceView
    // Builds the view.
```

#### `vox_text::reader`

```rust
pub fn pre_cut(text: &str, max: usize) -> (String, bool)
    // `preCut(text, max)`: cut unbounded raw input before cleaning — at `max`, backed up to the last `\n` if it lies in the final tenth; never ends with a lone high surrogate; not trimmed.
pub fn truncate_text(text: &str, max: usize) -> (String, bool)
    // `truncateText(text, max)`: final cut to `maxChars` — back up to the last whitespace only if it lies in the final 10 % (`> max × 0.9`), never split a surrogate pair, `trimEnd`.
pub fn preview_title(text: &str, max: usize) -> String
    // `previewTitle(text, max = 64)`: whitespace collapsed; longer than `max` → cut at the last space if it lies beyond `max × 0.6`, strip trailing `[\s,.;:\-–—]`, append `…`.
pub fn context_before(prev: Option<&str>) -> Option<String>
    // `contextBefore(prevChunkText)`: up to ~300 units from the end of the previous chunk, starting at a word boundary (the first whitespace within the first 60 units of the 300-unit tail), trimmed; `None` if empty.
pub fn context_after(next: Option<&str>) -> Option<String>
    // `contextAfter(nextChunkText)`: up to ~300 units from the start of the next chunk, ending at a word boundary (the last whitespace after unit 240 of the 300-unit head), trimmed; `None` if empty.
```

#### `vox_text::segment`

```rust
pub fn sentence_spans(text: &str) -> Vec<TextSpan>
    // Trimmed sentence spans with abbreviations merged (UTF-16 offsets into `text`).
pub fn fallback_segments(text: &str) -> Vec<TextSpan>
    // The fallback splitter used when no UAX #29 segmenter is available (tts.md §3.7 `fallbackSegments`).
pub fn sentence_starts(text: &str) -> Vec<u32>
    // Sentence start offsets for skip/highlight (ascending, never empty).
pub fn soft_splits(text: &str, start: u32, end: u32) -> Vec<u32>
    // Soft split points inside `[start, end)` for a sentence longer than 280 units (reader-playback.md §5).
pub fn word_spans(text: &str, start: u32, end: u32) -> Vec<TextSpan>
    // Every non-whitespace run inside `[start, end)` as absolute UTF-16 spans.
```


### A.3 `vox-store`

#### `vox_store::analytics`

```rust
pub fn compute_analytics<Tz: TimeZone>(records: &[ReadRecord], range: AnalyticsRange, now: DateTime<Tz>) -> AnalyticsSummary
    // The summary for `range` at `now` (records dated after today are ignored everywhere).
pub fn heard_fraction(r: &ReadRecord) -> f64
    // `heardFraction(r)` (persistence.md §7.3).
pub fn app_name(r: &ReadRecord) -> String
    // `appName(r)`: trimmed `sourceApp`, else the per-source fallback (`ReadSource::fallback_app_name`).
pub fn streaks<Tz: TimeZone>(records: &[ReadRecord], now: &DateTime<Tz>) -> Streak
    // `streaks()` over all valid records (persistence.md §7.5).
```

#### `vox_store::audio_cache`

```rust
pub struct ManifestChunk { index: u32, text_start: u32, text_end: u32, mime: String, alignment: Option<Alignment>, duration_sec: f64, file: String, bytes: u64 }
    // One chunk entry of a manifest.
pub struct CacheManifest { read_id: String, created_at: f64, last_access: f64, chunks: Vec<ManifestChunk> }
    // A parsed `manifest.json`.
impl CacheManifest {
    pub fn to_json(&self) -> String
        // Compact JSON in the Electron field order.
}
pub fn parse_manifest(v: &Value, folder: &str, now_ms: f64) -> Option<CacheManifest>
    // `parseManifest` (persistence.md §6.3): `None` unless an object whose `readId` equals `folder` and whose `chunks` is a non-empty array of objects with a valid `file` and a string `mime`; `createdAt` falls back to `now_ms`, `lastAccess` to `…
pub fn is_valid_chunk_file(name: &str) -> bool
    // `^\d{1,6}\.(mp3|wav|ogg|bin)$`.
pub type SaveDone = Box<dyn FnOnce(Result<(), StoreError>) + Send>
    // Completion callback of [`AudioCache::save_async`] (runs on the cache's I/O worker; must only enqueue).
pub type LoadDone = Box<dyn FnOnce(Option<Vec<CachedChunk>>) + Send>
    // Completion callback of [`AudioCache::load_async`] (`None` = missing or broken entry, which was deleted).
pub struct AudioCache
    // Thread-safe audio cache handle.
impl AudioCache {
    pub fn open(profile: &Profile) -> AudioCache
        // Scans and repairs `audio-cache\` (creates it if missing; on failure the cache stays empty for the session) and starts the I/O worker.
    pub fn save_async(&self, read_id: String, chunks: Vec<CachedChunk>, done: SaveDone)
        // Queues a save of a completed read's chunks (valid index, non-empty audio; de-duplicated by index, last wins; sorted); replaces any existing entry.
    pub fn load_async(&self, read_id: String, done: LoadDone)
        // Queues a load (sorted by stored index) that also touches `lastAccess`; a broken entry is deleted → `None`.
    pub fn save(&self, read_id: &str, chunks: &[CachedChunk]) -> Result<(), StoreError>
        // Blocking [`AudioCache::save_async`] (tests and tools; never on the core or UI thread).
    pub fn load(&self, read_id: &str) -> Option<Vec<CachedChunk>>
        // Blocking [`AudioCache::load_async`] (tests and tools; never on the core or UI thread).
    pub fn has(&self, read_id: &str) -> bool
        // The index has an entry (index-only, fast).
    pub fn has_text(&self, read_id: &str) -> bool
        // Some alignment char of the entry is non-empty (index-only).
    pub fn redact_text(&self, read_id: &str)
        // Queues blanking every alignment char of an entry (preview-only records); the index's `hasText` is cleared at once.
    pub fn remove(&self, read_id: &str)
        // Drops the index entry now and queues the folder deletion (after any queued save of the same id).
    pub fn clear(&self) -> CacheStats
        // Empties the index now and queues deleting every entry and anything else inside `audio-cache\`; returns the stats afterwards (zero).
    pub fn stats(&self) -> CacheStats
        // Totals from the index.
    pub fn prune(&self, max_bytes: u64, protect: &[String]) -> Vec<String>
        // Evicts least-recently-used entries from the index until the total is ≤ `max_bytes`, never touching `protect`, and queues their deletion; returns the evicted ids (the caller clears their records' `cached` flag).
    pub fn remove_orphans(&self, keep: &HashSet<String>) -> Vec<String>
        // Removes entries whose id is not in `keep`; returns them.
    pub fn ids(&self) -> Vec<String>
        // Every cached read id.
    pub fn flush(&self, timeout: Duration) -> bool
        // Waits up to `timeout` for the I/O queue to drain; `false` when it did not (quit sequence).
}
```

#### `vox_store::demo`

```rust
pub struct Mulberry32
    // mulberry32 (seed `0x5EED70C5` for the demo history).
impl Mulberry32 {
    pub fn new(seed: u32) -> Self
    pub fn next_f64(&mut self) -> f64
        // Next value in `[0, 1)`.
}
pub fn generate_demo_records<Tz: TimeZone>(now: DateTime<Tz>, count: usize) -> Vec<ReadRecord>
    // `generateDemoRecords(now, count = 120)` (persistence.md §8.5), newest first.
pub fn demo_voices() -> Vec<Voice>
    // The 8 demo voices (contracts.md §7.3), all premade, in the table's order (Liam = `TX3LPaxmHKxFdv7VOQHJ` …): `voiceId`, `name`, category `premade`, `description`, labels `{accent, age, gender, use_case}` exactly as listed.
pub fn demo_models() -> Vec<TtsModel>
    // The 4 demo models (contracts.md §7.3), in order: `modelId`, `name`, `description`, `maxChars` 40000/40000/10000/5000, `costFactor` 0.5/0.5/1/1, `languages = [{id: "en", name: "English"}]`.
pub fn demo_account(now_ms: f64) -> AccountInfo
    // `{tier:'creator', characterCount:61240, characterLimit:100000, nextResetUnix: now_s + 12 × 86 400, status:'active'}`.
```

#### `vox_store::error`

```rust
pub enum StoreError { Io(std::io::Error), Json(String), InvalidId(String), Secret(crate::secrets::SecretError), Other(String) }
    // Errors surfaced by vox-store (most store operations log and continue instead of failing).
```

#### `vox_store::fsutil`

```rust
pub enum JsonRead { Ok(Value), Missing, Corrupt(String) }
    // Outcome of reading one JSON file.
pub struct LoadOutcome { value: Option<Value>, from_backup: bool, main_was_corrupt: bool }
    // Outcome of loading a document with `.bak` / `.corrupt` handling.
pub fn retry_busy<T>(op: impl FnMut() -> io::Result<T>) -> io::Result<T>
    // Runs `op`, retrying sharing violations with the spec's delays.
pub fn is_busy_error(e: &io::Error) -> bool
    // True for the Windows "file busy" errors that are retried.
pub fn write_file_atomic(path: &Path, data: &[u8], backup: bool) -> io::Result<()>
    // `writeFileAtomic(path, data, {backup})`.
pub fn write_file_atomic_sync(path: &Path, data: &[u8], backup: bool) -> io::Result<()>
    // `writeFileAtomicSync` (session end): `<file>.tmp-sync`, no retries, no per-path lock (module docs); a `.bak` failure only warns.
pub fn read_json_file(path: &Path) -> JsonRead
    // `readJsonFile`.
pub fn load_with_backup(path: &Path) -> LoadOutcome
    // `JsonFile.load()`: main ok → it; missing → defaults (**no** `.bak` fallback: deleting is a reset); corrupt → copy to `<file>.corrupt` (best effort), then try `<file>.bak`.
pub fn discard_corrupt_copy(path: &Path)
    // Deletes `<file>.corrupt` (ignores not-found).
pub fn remove_dir_all_retry(path: &Path) -> bool
    // Recursive delete with up to 4 retries (60 ms × attempt); returns false (and logs) on final failure.
```

#### `vox_store::history_store`

```rust
pub struct HistoryStore
    // Thread-safe history store handle.
impl HistoryStore {
    pub fn open(profile: &Profile) -> HistoryStore
        // Loads `history.json` (missing/corrupt per [`crate::fsutil::load_with_backup`]).
    pub fn add(&self, record: ReadRecord)
        // Adds (or replaces by id) a record; it is normalised first.
    pub fn update(&self, id: &str, patch: &RecordPatch) -> Option<ReadRecord>
        // Patches a record.
    pub fn get(&self, id: &str) -> Option<ReadRecord>
    pub fn newest(&self) -> Option<ReadRecord>
        // The newest record (by `createdAt`).
    pub fn list(&self, query: &HistoryQuery) -> HistoryPage
        // `history:list` (vox_core::history::list_records semantics).
    pub fn all(&self) -> Vec<ReadRecord>
        // A copy of every record, newest first.
    pub fn with_all<R>(&self, f: impl FnOnce(&[ReadRecord]) -> R) -> R
        // Runs `f` over the records without copying them (analytics, reconciliation).
    pub fn len(&self) -> usize
    pub fn is_empty(&self) -> bool
    pub fn remove(&self, id: &str) -> bool
        // Removes one record; `false` for an unknown id (no write, no event).
    pub fn clear(&self) -> usize
        // Removes every record; returns how many (0 = no-op).
    pub fn toggle_favorite(&self, id: &str) -> Option<ReadRecord>
        // Flips `favorite`; returns the updated record.
    pub fn apply_retention(&self, days: u32, now_ms: f64) -> usize
        // Deletes non-favorite records with `createdAt < now − days × 86 400 000`; `days == 0` → 0.
    pub fn count_expiring(&self, days: u32, now_ms: f64) -> usize
        // How many reads `apply_retention(days, now)` would delete (the Settings retention guard).
    pub fn set_cached(&self, ids: &[String], cached: bool)
        // Sets `cached` on many records in one mutation (cache clear, eviction, reconciliation).
    pub fn export_to(&self, path: &Path, exported_at_iso: &str) -> std::io::Result<()>
        // Writes the export JSON (`HistoryExport`, pretty, atomic, no `.bak`) to `path`.
    pub fn on_change(&self, listener: Box<dyn Fn() + Send + Sync>)
        // Debounced change notification (no payload).
    pub fn on_removed(&self, listener: Box<dyn Fn(&[String]) + Send + Sync>)
        // Synchronous removal notification with the removed ids (called on the mutating thread — the core — after the lock is released).
    pub fn flush(&self)
    pub fn flush_sync(&self)
}
```

#### `vox_store::json_doc`

```rust
pub type Snapshot = Arc<dyn Fn() -> Option<String> + Send + Sync>
    // Produces the document text at write time (`None` = nothing to write / serialization failed).
pub struct JsonDocOptions { debounce: Duration, max_wait: Duration, backup: bool, retry_base: Duration, max_retries: u32 }
impl JsonDocOptions {
    pub fn settings() -> Self
    pub fn history() -> Self
}
pub struct JsonDoc
    // Handle to one debounced document (cheap to clone; one writer thread per document).
impl JsonDoc {
    pub fn new(path: PathBuf, opts: JsonDocOptions, snapshot: Snapshot) -> JsonDoc
        // Spawns the writer thread for `path`.
    pub fn path(&self) -> &Path
    pub fn schedule(&self)
        // Marks dirty and arms the debounce timer.
    pub fn flush(&self)
        // Cancels the timer, writes now if dirty, waits for any in-flight write.
    pub fn flush_sync(&self)
        // Session-end write (see module docs).
    pub fn discard_corrupt_copy(&self)
        // Deletes `<file>.corrupt`.
}
```

#### `vox_store::log`

```rust
pub struct LogGuard
    // Keeps the file writer alive; dropping it flushes.
pub fn init_logging(profile: &Profile, debug: bool) -> LogGuard
    // Installs the global subscriber.
pub fn redact(line: &str) -> Cow<'_, str>
    // Last line of defence: `(?-u:\b)sk_[A-Za-z0-9]{16,}(?-u:\b)` → `sk_***` and `(?i)(xi-api-key["':\s=]+)[^\s"',}]+` → `$1***`.
pub fn format_line(timestamp_ms: i64, level: tracing::Level, scope: &str, text: &str) -> String
    // One formatted line (without the trailing newline): `2026-10-09T00:20:30.735Z INFO [app] text` (timestamp from `vox_core::time::iso_utc_ms`).
```

#### `vox_store::paths`

```rust
pub struct Profile
    // A resolved profile folder.
impl Profile {
    pub fn resolve(demo: bool) -> Result<Profile, StoreError>
        // `%APPDATA%\Vox` (`FOLDERID_RoamingAppData` via `SHGetKnownFolderPath` + `APP_NAME`), or `%APPDATA%\Vox-Demo` when `demo`.
    pub fn at(root: impl Into<PathBuf>, demo: bool) -> Profile
        // A profile at an explicit folder (tests, tools).
    pub fn root(&self) -> &Path
    pub fn is_demo(&self) -> bool
    pub fn settings_file(&self) -> PathBuf
    pub fn history_file(&self) -> PathBuf
    pub fn secrets_file(&self) -> PathBuf
        // Electron's `secrets.bin` (read-only for the Rust app).
    pub fn secrets_dpapi_file(&self) -> PathBuf
        // The Rust app's `secrets-dpapi.bin`.
    pub fn window_state_file(&self) -> PathBuf
    pub fn ui_state_file(&self) -> PathBuf
    pub fn audio_cache_dir(&self) -> PathBuf
    pub fn logs_dir(&self) -> PathBuf
    pub fn log_file(&self) -> PathBuf
    pub fn local_state_candidates(&self) -> [PathBuf; 2]
        // Where Electron kept `Local State` (with the OSCrypt key): `Session\Local State` first, then the root.
}
```

#### `vox_store::secrets`

```rust
pub enum SecretError { Io(std::io::Error), Dpapi(String), Format(String), Decrypt }
    // Secret storage failures (never contain the key).
pub enum KeySource { None, Dpapi, Migrated }
    // Where the loaded key came from.
pub enum KeyProblem { MigrationFailed, Unreadable }
    // Why a stored key could not be used; drives `AppInfo::api_key_notice` (`copy::key_migration_failed()` / `copy::key_unreadable()`), shown on Settings → Account and the onboarding key step until a key is set.
pub struct KeyLoad { key: Option<String>, source: KeySource, problem: Option<KeyProblem> }
    // Result of [`load_api_key`].
pub fn load_api_key(profile: &Profile) -> KeyLoad
    // Loads the key (see the module docs for the order).
pub fn save_api_key(profile: &Profile, key: &str) -> Result<(), SecretError>
    // Saves `key` (already trimmed, non-empty) to `secrets-dpapi.bin` as a raw DPAPI blob through [`crate::fsutil::write_file_atomic`] (no `.bak`).
pub fn clear_api_key(profile: &Profile) -> Result<(), SecretError>
    // Clears the key: writes a **zero-byte** `secrets-dpapi.bin` (atomic) so the next start neither finds a key nor re-migrates `secrets.bin`.
pub fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, SecretError>
    // `CryptProtectData` (current user, no entropy, UI forbidden).
pub fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, SecretError>
    // `CryptUnprotectData` (no entropy, UI forbidden).
pub fn wrapped_key_from_local_state(local_state_json: &str) -> Result<Vec<u8>, SecretError>
    // Extracts the DPAPI-wrapped AES key from `Local State` JSON text: base64-decode `os_crypt.encrypted_key` and strip the `"DPAPI"` prefix.
pub fn decrypt_v10(blob: &[u8], aes_key: &[u8]) -> Result<Vec<u8>, SecretError>
    // Decrypts a `v10` blob with the 32-byte AES-GCM key.
```

#### `vox_store::settings_store`

```rust
pub struct SettingsChange<'a> { next: &'a Settings, prev: &'a Settings, key_changed: bool }
    // One effective change.
pub type SettingsListener = Box<dyn Fn(&SettingsChange<'_>) + Send + Sync>
    // Settings change listener.
pub struct KeyStatus { source: KeyStatusSource, problem: Option<KeyProblem> }
    // Where the key in memory came from, and whether a stored key could not be used.
pub enum KeyStatusSource { None, Stored(KeySource), Environment, Entered }
    // [`KeySource`] plus the environment fallback and keys set during this session.
pub struct SettingsStore
    // Thread-safe settings store handle.
impl SettingsStore {
    pub fn open(profile: &Profile, env_api_key: Option<String>) -> SettingsStore
        // Loads settings and the API key (with Electron migration).
    pub fn get(&self) -> Settings
        // The current settings (with the derived `has_api_key`).
    pub fn update(&self, patch: &Value) -> Settings
        // Applies a JSON merge patch (contracts.md §4.3).
    pub fn subscribe(&self, listener: SettingsListener)
        // Registers a change listener (called after every effective change, including API-key set/clear).
    pub fn api_key(&self) -> Option<String>
        // The API key (trimmed), if any.
    pub fn has_api_key(&self) -> bool
    pub fn key_status(&self) -> KeyStatus
        // Where the key came from / why a stored key is unusable (drives `AppInfo::api_key_notice`).
    pub fn set_api_key(&self, key: Option<&str>) -> Settings
        // Stores (`Some`) or clears (`None`/blank) the key; returns the resulting settings.
    pub fn flush(&self)
        // Writes pending settings and waits for pending secret writes (quit).
    pub fn flush_sync(&self)
        // Synchronous session-end write of `settings.json` (`.tmp-sync`).
    pub fn read_settings_file_sync(profile: &Profile) -> Option<Settings>
        // The early synchronous read used before the stores open (persistence.md §3.5): missing → `None` (no `.bak`); unreadable main → try `.bak`; never writes a `.corrupt` copy; sanitized with `has_api_key = false`.
}
```

#### `vox_store::ui_state`

```rust
pub struct UiState { analytics_range: AnalyticsRange }
pub fn load_ui_state(profile: &Profile) -> UiState
    // Loads `ui-state.json` (missing/invalid → defaults).
pub fn save_ui_state(profile: &Profile, state: &UiState)
    // Saves `ui-state.json` atomically (errors ignored).
```

#### `vox_store::window_state`

```rust
pub struct WindowState { x: i32, y: i32, width: i32, height: i32, scale: f64, maximized: bool }
    // The persisted main-window placement.
impl WindowState {
    pub fn to_json(&self) -> String
        // Pretty JSON in the documented field order (`version` first).
}
pub fn parse_window_state(v: &Value) -> Option<WindowState>
    // Validates a parsed `window-state.json` (see module docs).
pub fn load_window_state(profile: &Profile) -> Option<WindowState>
    // Loads `window-state.json` (missing / unreadable / invalid → `None`; never fails, logs at debug level).
pub fn save_window_state(profile: &Profile, state: &WindowState)
    // Saves atomically (`fsutil::write_file_atomic`, no `.bak`); errors are logged (scope `data`) and swallowed.
```


### A.4 `vox-tts`

#### `vox_tts::cached`

```rust
pub fn preview_replay(preview: &str, chunks: Vec<CachedChunk>) -> (String, Vec<CachedChunk>)
    // `previewReplay(preview, cached)`: the session text = preview + one `…` per chunk past it (a straddling chunk gets `textEnd = len + 1` over an appended `…` and a clipped alignment; a chunk entirely past it gets `"\n…"` and `alignment: None`…
pub fn clip_alignment(chunk: &CachedChunk, len: u32, keep: u32) -> Option<Alignment>
    // `clipAlignment(c, len, keep)`: timing for the first `keep` chars plus one entry for the rest (chars all `""`).
pub fn without_text(chunks: &[CachedChunk]) -> Vec<CachedChunk>
    // `withoutText(chunks)`: same chunks with every alignment char replaced by `""`.
```

#### `vox_tts::cancel`

```rust
pub struct CancelToken
    // A cancellation token.
impl CancelToken {
    pub fn new() -> Self
    pub fn cancel(&self)
        // Cancels and wakes every waiter.
    pub fn is_cancelled(&self) -> bool
    pub fn sleep(&self, d: Duration) -> bool
        // Sleeps up to `d`; returns `false` if cancelled (early).
}
```

#### `vox_tts::client`

```rust
pub type KeySource = Arc<dyn Fn() -> Option<String> + Send + Sync>
    // Reads the current API key on every call (`SettingsStore::api_key`).
pub struct ProxyConfig { url: String, bypass: Vec<String>, bypass_local: bool }
    // A forward proxy for every request.
pub struct ClientConfig { base_url: String, user_agent: String, proxy: Option<ProxyConfig>, platform_roots: bool }
    // Client construction options.
impl Default for ClientConfig {
}
pub fn user_agent() -> String
    // `Vox/<version> (Windows)`.
pub struct SynthesisRequest { text: String, voice_id: String, model_id: String, output_format: String, voice_settings: VoiceSettings, previous_text: Option<String>, next_text: Option<String>, language_code: Option<String> }
    // One synthesis request (tts.md §2.6).
pub struct SynthesisResult { audio: AudioBytes, mime: String, alignment: Option<Alignment>, request_id: Option<String>, character_cost: Option<f64> }
    // A synthesized chunk.
pub struct RejectedParams
    // Optional parameters a model rejected during this process (the Product ruling "remember a rejected optional parameter for the rest of the session").
impl RejectedParams {
    pub fn language_rejected(&self, model_id: &str, code: &str) -> bool
        // `language_code = code` was rejected for `model_id`.
    pub fn context_rejected(&self, model_id: &str) -> bool
        // `previous_text`/`next_text` were rejected for `model_id`.
    pub fn reject_language(&self, model_id: &str, code: &str)
    pub fn reject_context(&self, model_id: &str)
    pub fn apply(&self, req: &SynthesisRequest) -> SynthesisRequest
        // Removes remembered rejections from a request before it is sent (and `eleven_v3*` context, tts.md §2.6.1).
}
pub struct ElevenLabsClient
    // Cheap to clone; all clones share the agent, the caches and [`RejectedParams`].
impl ElevenLabsClient {
    pub fn new(key_source: KeySource, config: ClientConfig) -> Self
        // Builds the agent from `config`.
    pub fn list_voices(&self, refresh: bool) -> Result<Vec<Voice>, TtsError>
        // All voices for the key (v2 paging, v1 fallback, anonymous v1 without a key), sorted (tts.md §2.7).
    pub fn list_models(&self, refresh: bool) -> Result<Vec<TtsModel>, TtsError>
        // TTS-capable models; no key → `no_key` (tts.md §2.8).
    pub fn get_account(&self, refresh: bool) -> Result<Option<AccountInfo>, TtsError>
        // Subscription; `Ok(None)` without a key or when the key lacks permission (cached as `None`) (tts.md §2.9).
    pub fn synthesize(&self, req: &SynthesisRequest, cancel: &CancelToken) -> Result<SynthesisResult, TtsError>
        // One chunk with timestamps: [`RejectedParams::apply`], then up to 2 parameter-fallback resends (each remembered), no other retries (the engine owns those).
    pub fn set_proxy(&self, proxy: Option<ProxyConfig>)
        // Replaces the proxy for every later request (module docs "Proxy at run time"): builds a new agent and swaps it atomically; requests in flight keep the agent they started with; memos and rejected parameters are kept.
    pub fn proxy(&self) -> Option<ProxyConfig>
        // The proxy currently in use (`None` = direct).
    pub fn clear_cache(&self)
        // Drops every memo (never called by the app; key changes rely on the fingerprint).
    pub fn validate_api_key(&self, key: &str) -> ApiKeyResult
        // `validateApiKey(key)` (tts.md §2.11): uncached, uses the candidate key.
    pub fn fetch_preview(&self, url: &str, cancel: &CancelToken) -> Result<AudioBytes, TtsError>
        // Downloads a voice's public sample (`Voice::preview_url`, no API key header) for the Voices/onboarding preview (ui-settings-widget.md §4.6).
}
pub fn key_fingerprint(key: Option<&str>) -> String
    // `hex(sha256(utf8(key)))[0..16]`, or `"anon"`.
pub fn without_rejected_param(req: &SynthesisRequest, err: &TtsError) -> Option<(SynthesisRequest, RejectedParam)>
    // `withoutRejectedParam(req, err)` (tts.md §2.6.1): `(reduced request, which parameter)`; the caller remembers it.
pub enum RejectedParam { Language(String), Context }
    // Which optional parameter a fallback dropped.
pub fn synthesis_body(req: &SynthesisRequest) -> serde_json::Value
    // The JSON body of a synthesis request (clamped voice settings; optional fields omitted when empty).
```

#### `vox_tts::duration`

```rust
pub fn chunk_duration(result: &SynthesisResult, chunk_len16: usize, settings: &Settings) -> f64
    // Best known duration of one synthesized chunk.
```

#### `vox_tts::engine`

```rust
pub trait TtsSink: Send + Sync { fn session_start(&self, start: TtsSessionStart), fn chunk(&self, chunk: TtsChunk), fn done(&self, done: TtsSessionDone), fn error(&self, error: TtsSessionError) }
    // Delivery to the player (vox-app forwards to `vox_audio::Player`).
pub enum EngineEvent { Complete { read_id: String, chunks: Vec<CachedChunk>, characters_billed: f64 }, Failed { read_id: String, error: TtsError }, Stopped { read_id: String, characters_billed: f64, error: Option<TtsError> } }
    // Events for the read controller.
pub type EngineEvents = Arc<dyn Fn(EngineEvent) + Send + Sync>
    // Event listener (vox-app enqueues `Command::Engine`).
pub struct StartSessionOptions { read_id: String, text: String, title: String, voice_id: String, voice_name: String, source_app: Option<String>, settings: Settings }
    // `StartSessionOptions`.
pub struct PlayCachedOptions { read_id: String, text: String, title: String, voice_id: String, voice_name: String, source_app: Option<String>, settings: Settings, chunks: Vec<CachedChunk> }
    // `PlayCachedOptions`.
pub struct TtsEngine
    // The engine handle (one per app).
impl TtsEngine {
    pub fn new(client: ElevenLabsClient, sink: Arc<dyn TtsSink>, events: EngineEvents, mock: bool) -> Self
    pub fn start(&self, opts: StartSessionOptions) -> String
        // Starts a fresh synthesis session; returns its id.
    pub fn play_cached(&self, opts: PlayCachedOptions) -> String
        // Plays cached chunks as a session; returns its id.
    pub fn progress(&self, session_id: &str, position_sec: f64)
        // The player's playhead (audio seconds) for look-ahead; ignored for other sessions or non-finite values.
    pub fn stop(&self)
        // Stops the current session (idempotent): under the engine lock cancels its token and forgets it (`current_*` become `None`); then emits `Stopped { billed_so_far, error: None }` synchronously on the caller's thread when the session had not f…
    pub fn current_session_id(&self) -> Option<String>
        // The latest session id, kept after synthesis finishes (audio may still play) until `stop()`.
    pub fn current_read_id(&self) -> Option<String>
    pub fn is_mock(&self) -> bool
}
```

#### `vox_tts::error`

```rust
pub struct TtsError { code: ReadErrorCode, message: String, status: Option<u16>, api_code: Option<String>, api_message: Option<String>, param: Option<String>, request_id: Option<String>, retryable: bool, missing_permission: bool, aborted: bool, retry_after_sec: Option<f64> }
    // `ElevenLabsError`.
impl TtsError {
    pub fn new(code: ReadErrorCode, message: impl Into<String>) -> Self
        // A plain error with a code and message (all flags false).
    pub fn no_key() -> Self
    pub fn bad_response() -> Self
        // 2xx with an unparsable body (not retryable: a 2xx synthesis was billed).
    pub fn no_audio() -> Self
        // 2xx synthesis without audio (not retryable: billed).
    pub fn cancelled() -> Self
    pub fn timeout() -> Self
        // The request timeout fired (not retryable: possibly billed).
    pub fn network() -> Self
        // Transport failure after the request was (possibly) sent (not retryable).
    pub fn network_unsent() -> Self
        // Transport failure before any request byte was sent: DNS, connection refused/reset during connect, TLS handshake, proxy CONNECT refused (retryable).
    pub fn empty_text() -> Self
        // Engine: the text yields no chunks.
    pub fn cached_audio_missing() -> Self
        // Engine: a cached session with zero chunks.
    pub fn crashed() -> Self
        // Engine: unexpected internal failure.
}
pub struct ErrorBody { kind: Option<String>, code: Option<String>, status: Option<String>, message: Option<String>, request_id: Option<String>, param: Option<String> }
    // `parseErrorBody(raw)` result (tts.md §2.4.3).
pub fn parse_error_body(raw: &str) -> ErrorBody
    // `parseErrorBody` (tts.md §2.4.3): JSON `detail` object / string / FastAPI array, or the first 300 chars of a non-HTML text body.
pub fn error_from_response(status: u16, raw: &str, header: &dyn Fn(&str) -> Option<String>) -> TtsError
    // `errorFromResponse(status, raw, headers)` (tts.md §2.4.4): the 10-row first-match classification.
pub fn permission_message(api_message: Option<&str>) -> String
    // `permissionMessage(apiMessage)` (tts.md §2.4.4).
pub fn parse_retry_after(v: Option<&str>) -> Option<f64>
    // `retry-after` → seconds when finite and > 0 (JS `Number()` rules: trims, accepts `0x` hex, rejects dates).
pub fn int_header(v: Option<&str>) -> Option<f64>
    // `intHeader` (tts.md §2.5): absent/blank → none; else finite and ≥ 0 (fractions allowed).
pub const INVALID_KEY_CODES: [&str; 5] = ["invalid_api_key", "missing_api_key", "needs_authorization", "invalid_authorization_header", "un…
    // Key-validation `INVALID_KEY_CODES`.
```

#### `vox_tts::mapping`

```rust
pub fn map_voice(raw: &Value) -> Option<Voice>
    // `mapVoice(raw)`: `None` without a non-empty `voice_id`; premade names `"George - Warm…"` split into name + `labels.tagline`.
pub fn sort_voices(voices: Vec<Voice>) -> Vec<Voice>
    // `sortVoices`: de-duplicate by id (first wins), then own voices (cloned/generated/professional) rank 0, other non-premade 1, premade 2; within a rank by name with `icu_collator` (root locale, `Strength::Primary`, numeric ordering on — `loca…
pub fn map_model(raw: &Value) -> Option<TtsModel>
    // `mapModel(raw)`: only `can_do_text_to_speech === true` with a non-empty `model_id`.
pub fn map_account(raw: &Value) -> Result<AccountInfo, TtsError>
    // `mapAccount(raw)`: a non-object → `bad_response`.
pub fn map_alignment(raw: &Value) -> Option<Alignment>
    // `mapAlignment(raw)`: requires the three arrays, equal non-zero length, finite times; chars stringified JS-style.
pub fn mime_for_format(format: &str) -> &'static str
    // `mimeForFormat`: mp3 → audio/mpeg, wav → audio/wav, opus → audio/ogg, pcm → audio/pcm, ulaw/alaw → audio/basic, else application/octet-stream.
pub fn mp3_kbps(format: &str) -> Option<u32>
    // `mp3Kbps`: `^mp3_\d+_(\d+)$` → the bitrate (0 → `None`).
pub fn decode_base64_lenient(s: &str) -> Vec<u8>
    // Node-compatible lenient base64 (tts.md §2.6 step 2): both alphabets, skip invalid chars, stop at the first `=`, drop a dangling sextet.
```

#### `vox_tts::mock`

```rust
pub fn mock_synthesize(text: &str, speed: f64) -> SynthesisResult
    // `mockSynthesize(text, speed)`.
pub fn even_alignment(text: &str, duration_sec: f64) -> Option<Alignment>
    // `evenAlignment(text, durationSec)`: one entry per **UTF-16 unit** (so it maps 1:1 onto the UTF-16 offsets); for an astral character the first unit's entry holds the character and the second `""`.
```

#### `vox_tts::wav`

```rust
pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8>
    // `encodeWav(samples, sampleRate)`: 44-byte header, little-endian; `v = clamp(s, -1, 1)`, `round(v < 0 ? v × 32768 : v × 32767)` with JS rounding.
pub struct WavInfo { format: u16, channels: u16, sample_rate: u32, bits_per_sample: u16, data_offset: usize, data_bytes: usize, duration_sec: f64 }
    // Parsed `fmt ` + `data` facts.
pub fn read_wav_info(bytes: &[u8]) -> Option<WavInfo>
    // `readWavInfo(bytes)` (walks RIFF chunks; 64-bit arithmetic; a later `fmt ` overrides; `data` before `fmt ` is skipped).
```


### A.5 `vox-audio`

#### `vox_audio::clock`

```rust
pub type NowMs = fn() -> f64
    // Monotonic milliseconds supplier (injected for tests).
pub struct ListeningClock
impl ListeningClock {
    pub fn sync(&mut self, running: bool, now_ms: f64, media_sec: f64, rate: f64)
        // `syncClock`: start or stop counting (`running = playing && audible`), with the current media time.
    pub fn credit(&mut self, now_ms: f64, media_sec: f64, rate: f64)
        // `creditListening`: bank and restart the baseline (before rate changes and seeks).
    pub fn rebase(&mut self, now_ms: f64, media_sec: f64)
        // After a seek: restart the baseline at the new media time without counting the jump.
    pub fn listened_sec(&self, now_ms: f64, media_sec: f64, rate: f64) -> f64
        // `listenedSec()`.
    pub fn reset(&mut self)
}
```

#### `vox_audio::decode`

```rust
pub struct DecodedAudio { sample_rate: u32, channels: u16, samples: Vec<f32> }
    // Interleaved f32 PCM.
impl DecodedAudio {
    pub fn frames(&self) -> usize
    pub fn duration_sec(&self) -> f64
        // Decoded duration (adopted when it differs from the engine's by > 0.01 s, §4.7 `onChunkMeta`).
}
pub enum DecodeError { Unsupported(String), Corrupt(String) }
pub fn decode(bytes: &[u8], mime: &str) -> Result<DecodedAudio, DecodeError>
    // Decodes one complete file.
```

#### `vox_audio::output`

```rust
pub struct BlockTag { generation: u64, chunk: u32, source_frame_end: f64 }
    // A block's position tag: which chunk and source frame it ends at.
pub enum OutputError { NoDevice, Stream(String) }
pub struct AudioOutput
    // The open output (lives on the player thread).
impl AudioOutput {
    pub fn open() -> Result<AudioOutput, OutputError>
        // Opens the default output device (stream starts suspended).
    pub fn sample_rate(&self) -> u32
    pub fn channels(&self) -> u16
    pub fn free_frames(&self) -> usize
        // Free space in the ring, in frames.
    pub fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool
        // Queues one interleaved block (device rate and channel count).
    pub fn played_tag(&self) -> BlockTag
        // Tag of the last block whose device playback instant is ≤ now (see [`BlockTag`]).
    pub fn rendered_frames(&self) -> u64
        // Total frames the device consumed since the stream last started (warmer clock).
    pub fn set_volume(&self, volume: f32)
        // Linear 0..1.
    pub fn flush(&mut self) -> u64
        // Starts a new generation: queued blocks of older generations are dropped by the callback.
    pub fn set_keep_alive(&self, on: bool)
        // Keep-alive signal on/off when nothing is queued.
    pub fn suspend(&mut self)
    pub fn resume(&mut self)
    pub fn is_lost(&self) -> bool
        // The stream reported an error (device removed).
}
```

#### `vox_audio::player`

```rust
pub struct PlayerConfig { report_interval: Duration, warm_output: bool, null_output: bool, name: &'static str }
    // Player settings.
impl Default for PlayerConfig {
}
pub enum PlayerEvent { State(PlayerSnapshot), Failed { session_id: Option<String>, message: String } }
    // What the player tells the core.
pub type PlayerEvents = Arc<dyn Fn(PlayerEvent) + Send + Sync>
    // Listener for [`PlayerEvent`]s (vox-app enqueues `Command::PlayerReport`).
pub struct Player
    // Handle to the player thread.
impl Player {
    pub fn spawn(config: PlayerConfig, events: PlayerEvents) -> Player
        // Spawns the player thread.
    pub fn start_session(&self, start: TtsSessionStart)
        // `startSession` (§4.4).
    pub fn add_chunk(&self, chunk: TtsChunk)
        // `addChunk` (§4.4).
    pub fn finish_session(&self, done: TtsSessionDone)
        // `finishSession` (§4.4).
    pub fn fail_session(&self, error: TtsSessionError)
        // `failSession` (§4.4).
    pub fn command(&self, cmd: PlayerCommand)
        // Transport command (§4.6).
    pub fn snapshot(&self) -> PlayerSnapshot
        // The current snapshot computed for "now" (position includes the time played since the last report).
    pub fn preload_output(&self)
        // Builds the output stream while idle and leaves it suspended (warmer `preload`, §6).
    pub fn shutdown(&self)
        // Stops the session (without reporting) and closes the output; the thread exits.
}
```

#### `vox_audio::resample`

```rust
pub struct Resampler
    // Streaming resampler.
impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16) -> Self
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>)
        // Converts `input` (interleaved) and appends to `out`.
    pub fn flush(&mut self, out: &mut Vec<f32>)
        // Emits the tail.
    pub fn reset(&mut self)
    pub fn ratio(&self) -> f64
}
```

#### `vox_audio::stretch`

```rust
pub struct Wsola
    // Streaming WSOLA over one PCM buffer.
impl Wsola {
    pub fn new(sample_rate: u32, channels: u16) -> Self
    pub fn set_rate(&mut self, rate: f64)
        // Clamped to 0.5..3.
    pub fn rate(&self) -> f64
    pub fn reset(&mut self)
        // Drops internal overlap state (after a seek or chunk change).
    pub fn render(&mut self, src: &[f32], pos: &mut f64, out: &mut [f32]) -> usize
        // Renders up to `out.len() / channels` frames from `src` (interleaved) starting at source frame `*pos`, advancing `*pos` by the source frames consumed.
}
```

#### `vox_audio::timeline`

```rust
pub struct TimelineChunk { text_start: u32, text_end: u32, duration_sec: f64, alignment_starts: Option<Vec<f64>>, failed: bool }
    // What the timeline needs per chunk.
pub struct Timeline { text_len: u32, estimated_duration_sec: f64, chunks: Vec<TimelineChunk>, done: bool }
    // Session-level inputs.
impl Timeline {
    pub fn dur(&self, i: usize) -> f64
        // `dur(c)`.
    pub fn available_sec(&self) -> f64
        // Σ `dur` over delivered chunks.
    pub fn chunk_start(&self, i: usize) -> f64
        // Start time of chunk `i`.
    pub fn position_sec(&self, cur: usize, t: f64, ended: bool) -> f64
        // Position for current chunk `cur` at in-chunk time `t` (`ended` → available).
    pub fn duration_sec(&self) -> f64
        // Duration: exact when done, else available + remaining chars × observed seconds per char.
    pub fn char_index(&self, cur: usize, t: f64, ended: bool) -> u32
        // The char index for current chunk `cur` at in-chunk time `t`.
    pub fn time_at_char(&self, ch: u32) -> f64
        // Session seconds of char `ch`; `f64::INFINITY` when its audio has not arrived.
    pub fn locate(&self, sec: f64) -> (usize, f64)
        // Chunk index and offset for a session time (seek), clamped to available − 0.05 s.
}
```

#### `vox_audio::warmer`

```rust
pub enum WarmState { Ready, Waiting }
    // Warmer decisions.
pub struct OutputWarmer
impl OutputWarmer {
    pub fn prepare(&mut self, now_ms: f64, rendered_frames: u64, sample_rate: u32, running: bool) -> WarmState
        // `prepare()`: cancels a pending release; returns `Ready` when warm, given up or impossible.
    pub fn poll(&mut self, now_ms: f64, rendered_frames: u64, sample_rate: u32, running: bool) -> WarmState
        // Polled while `Waiting`.
    pub fn release(&mut self, now_ms: f64, after_ms: u64)
        // `release(afterMs)`.
    pub fn should_suspend(&mut self, now_ms: f64) -> bool
        // True when the scheduled release time has passed (the player suspends the stream).
    pub fn open_failed(&mut self, now_ms: f64)
        // The stream could not be created.
    pub fn may_open(&self, now_ms: f64) -> bool
        // May the player try to open the stream now (60 s after a failure)?
}
pub fn keep_alive_sample(i: u64, sample_rate: u32) -> f32
    // The keep-alive sample at frame `i` for `sample_rate` (`±1/32768`, period 20 ms).
```


### A.6 `vox-win`

#### `vox_win::clipboard`

```rust
pub struct ClipboardSnapshot { formats: Vec<(u32, Vec<u8>)>, incomplete: bool, failed: bool }
    // Snapshot of the user's clipboard (all formats that could be read).
pub enum Privacy { Secret, Private }
    // Why a copy must not be read.
pub fn read_clipboard_text() -> Option<String>
    // `CF_UNICODETEXT` as a string (`None` on any error or no text).
pub fn write_clipboard_text(text: &str) -> Result<(), crate::WinError>
    // Puts plain text on the clipboard (History "Copy text", Settings copy chips).
pub fn sequence_number() -> u32
    // `GetClipboardSequenceNumber`.
pub fn has_text_format() -> bool
    // `IsClipboardFormatAvailable(CF_UNICODETEXT) || (CF_TEXT)`.
pub fn snapshot() -> ClipboardSnapshot
pub fn restore(snapshot: &ClipboardSnapshot) -> Result<(), crate::WinError>
    // Puts the snapshot back (one `OpenClipboard`/`EmptyClipboard`/`SetClipboardData…`/`CloseClipboard` block) with the two "do not record" markers added (module docs).
pub fn privacy() -> Option<Privacy>
    // Privacy markers on the current clipboard content.
```

#### `vox_win::error`

```rust
pub struct WinError(pub String)
    // A Win32/WinRT failure (message for logs and toasts).
impl From<windows::core::Error> for WinError {
}
impl From<std::io::Error> for WinError {
}
```

#### `vox_win::foreground`

```rust
pub struct WindowInfo { hwnd: isize, info: ForegroundInfo, class_name: String }
    // A described top-level window.
impl WindowInfo {
    pub fn window_id(&self) -> String
        // The gesture identity: root HWND as decimal.
}
pub fn foreground_info() -> Option<ForegroundInfo>
    // `describe(GetForegroundWindow())` without the window id/class.
pub fn foreground_window_info() -> Option<WindowInfo>
pub fn window_info_at(x: i32, y: i32) -> Option<WindowInfo>
    // The root window under a physical point.
pub fn friendly_app_name(process_name: &str) -> String
    // `friendlyAppName(exe)` (table + title-case fallback; pure).
pub fn is_password_manager(process_name: &str) -> bool
    // The password-manager deny list (pure, case-insensitive).
pub fn clean_title(title: &str) -> String
    // `cleanTitle`: whitespace collapsed, trimmed, ≤ 300 units (pure).
pub fn is_own_overlay_at(x: i32, y: i32) -> Option<bool>
    // Our own non-activating window (widget/chip) is under the point.
pub fn text_cursor_shown() -> Option<bool>
    // Text-cursor evidence (I-beam, caret, or a document app in front → benefit of the doubt).
pub fn fullscreen_app_running() -> bool
    // D3D fullscreen or presentation mode (`SHQueryUserNotificationState` 3 or 4).
pub fn input_blocked_by_uipi(hwnd: isize, pid: u32) -> Option<bool>
    // UIPI check (BRIEF.md Product rulings "Clipboard": the target app runs elevated and Vox does not, so injected input would be dropped).
```

#### `vox_win::gestures`

```rust
pub struct GestureConfig { context_chip: bool, selection_chip: bool }
pub enum HintKind { Drag, MultiClick, ShiftClick, Keyboard, SelectAll }
pub enum HintClearReason { Click, Key, Expired, Blocked, Reset }
pub enum ChipHideReason { MouseDown, Key, Wheel, Timeout, Disabled, Dismissed, Activated, Reset }
pub enum Decision { HintSet(HintKind), HintCleared(HintClearReason), ShowContextChip { x: i32, y: i32 }, ShowSelectionChip { x: i32, y: i32 }, HideChip(ChipHideReason), ActivateChip(ChipMode) }
    // What the service must do.
pub enum MouseButton { Left, Right, Middle, X1, X2 }
pub enum GestureEventKind { MouseDown(MouseButton), MouseUp(MouseButton), MouseMove, Wheel, KeyDown { vk: u32, extended: bool } }
pub struct GestureEvent { kind: GestureEventKind, x: i32, y: i32, clicks: u32, time_ms: u64, shift: bool, ctrl: bool, alt: bool, meta: bool }
    // One normalised input event.
pub struct GestureTarget { id: String, blocked: bool }
    // A window identity for hint matching.
pub trait GestureDeps { fn is_inside_chip(&self, x: i32, y: i32) -> bool, fn target_at(&self, x: i32, y: i32) -> Option<GestureTarget>, fn focused_target(&self) -> Option<GestureTarget>, fn own_overlay_at(&self, x: i32, y: i32) -> bool, fn text_cursor(&self) -> Option<bool>, fn chips_suppressed(&self) -> bool }
    // Queries the machine makes (selection.md §5.4).
pub struct GestureMachine
    // The machine.
impl GestureMachine {
    pub fn new(cfg: GestureConfig) -> Self
    pub fn configure(&mut self, cfg: GestureConfig) -> Vec<Decision>
        // `configure`: hides a visible chip whose mode is now disabled.
    pub fn handle(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps) -> Vec<Decision>
        // `handle(ev)`: `tick(ev.time)` first, then dispatch (§5.5.2–§5.5.8).
    pub fn tick(&mut self, now_ms: u64) -> Vec<Decision>
        // Expiry: hint after 60 s, chip after its ttl (4 s selection / 8 s context).
    pub fn hide_chip(&mut self, reason: ChipHideReason) -> Vec<Decision>
    pub fn reset(&mut self) -> Vec<Decision>
    pub fn wants_mouse_move(&self) -> bool
        // The hook only forwards moves while this is true (hot-path filter).
    pub fn next_deadline(&self) -> Option<u64>
        // The next expiry deadline (ms), for the service's single timer.
    pub fn chip_mode(&self) -> Option<ChipMode>
        // Mode of the visible chip.
}
pub fn is_modifier_vk(vk: u32) -> bool
    // The 8 side-specific modifier VKs (plus the generic Shift/Ctrl/Alt and Win keys).
pub fn is_navigation_vk(vk: u32) -> bool
    // Arrows, Home, End, PageUp, PageDown (regardless of the extended flag).
pub fn is_neutral_vk(vk: u32) -> bool
    // Keys that never change a selection (selection.md §4.3 table, as libuiohook produced them on Windows).
```

#### `vox_win::hook`

```rust
pub enum RawInput { Mouse { msg: u32, x: i32, y: i32, mouse_data: u32, time: u32 }, Key { down: bool, vk: u32, scan: u32, extended: bool, injected: bool, time: u32 } }
    // A raw hook event.
pub struct InputHook
    // The running hook.
impl InputHook {
    pub fn start(events: Sender<RawInput>) -> Result<InputHook, crate::WinError>
        // Starts the hook thread; fails if `SetWindowsHookExW` fails.
    pub fn set_wants_mouse_move(&self, wants: bool)
    pub fn stop(self)
        // Unhooks and ends the thread.
}
```

#### `vox_win::hotkeys`

```rust
pub type HotkeyHandler = Arc<dyn Fn(HotkeyAction) + Send + Sync>
    // Hotkey callback (called on the hotkey thread; vox-app enqueues `Command::Hotkey`).
pub struct HotkeyService
    // The hotkey thread.
impl HotkeyService {
    pub fn spawn(on_hotkey: HotkeyHandler) -> Result<HotkeyService, crate::WinError>
    pub fn register(&self, plan: &RegistrationPlan) -> Vec<HotkeyFailure>
        // Unregisters ours, then registers the plan; returns the plan's failures plus `InUse` for every OS refusal, sorted into registration order with `vox_core::hotkey::sort_failures` (each action fails at most once), so `describe_failures` lists…
    pub fn unregister_all(&self)
    pub fn shutdown(&self)
}
pub fn alt_gr_character(vk: u32, shift: bool) -> Option<String>
    // What Ctrl+Alt(+Shift)+`vk` types on any installed layout: `Some("")` dead key, `Some(ch)`, or `None`.
pub fn alt_gr_character_active_layout(vk: u32, shift: bool) -> Option<String>
    // The same probe on the **active** layout only (the Settings recorder).
pub fn vk_from_scan_code(scan: u32, extended: bool) -> Option<u32>
    // Scan code (+ extended flag) → virtual key on the active layout (`MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)`), for the recorder fed by winit key events.
```

#### `vox_win::keys`

```rust
pub enum CopyMethod { CtrlC { escape_retry: bool }, CtrlInsert, ReadClipboard }
    // How to copy the selection from an app.
pub fn copy_method_for(process_name: &str, class_name: &str) -> CopyMethod
    // `copyMethodFor(exe, class)` (pure; lower-cased comparisons; sets from selection.md §8.4).
pub fn send_chord(method: CopyMethod) -> bool
    // Sends a chord with `SendInput` (tagged with Vox's `dwExtraInfo`); false when it could not be sent.
pub fn send_escape() -> bool
    // Sends a single Escape.
pub fn wait_modifiers_released(max_ms: u64, aborted: &dyn Fn() -> bool) -> Vec<u32>
    // Waits up to `max_ms` (polling 10 ms with `GetAsyncKeyState`) until no modifier is physically held; then releases any still held with synthetic key-ups.
```

#### `vox_win::media`

```rust
pub enum MediaAction { Play, Pause, Stop, Previous, Next, SeekTo(f64), SeekBy(f64) }
    // A media-key / flyout action.
pub struct MediaState { active: bool, playing: bool, title: String, artist: String, album: String, position_sec: f64, duration_sec: f64, rate: f64 }
    // What the flyout shows.
pub type MediaHandler = Arc<dyn Fn(MediaAction) + Send + Sync>
pub struct MediaSession
impl MediaSession {
    pub fn spawn(on_action: MediaHandler) -> Result<MediaSession, crate::WinError>
    pub fn set_enabled(&self, enabled: bool)
    pub fn update(&self, state: MediaState)
        // Pushes metadata/state; position is refreshed at most once per second unless status/rate changed.
    pub fn shutdown(&self)
}
```

#### `vox_win::notify_icon`

```rust
pub struct TrayIconImage { size: u32, rgba: Vec<u8> }
    // One rendered icon bitmap (straight RGBA8, `size × size`).
pub enum NotifyIconEvent { Activate, ContextMenu { x: i32, y: i32 } }
    // What the icon reports.
pub type NotifyIconHandler = Box<dyn Fn(NotifyIconEvent)>
    // Event callback, called on the UI thread (the thread that created the icon).
pub struct NotifyIcon
    // The icon and its hidden window.
impl NotifyIcon {
    pub fn new(tooltip: &str, images: Vec<TrayIconImage>, on_event: NotifyIconHandler) -> Result<NotifyIcon, crate::WinError>
        // Creates the hidden window and adds the icon (version 4).
    pub fn set_images(&self, images: Vec<TrayIconImage>)
        // Replaces the bitmaps (normal / active state) and re-picks the size for the current DPI.
    pub fn refresh_dpi(&self)
        // Re-picks the bitmap size after a display / DPI change.
    pub fn set_tooltip(&self, tooltip: &str)
    pub fn hwnd(&self) -> Hwnd
        // The hidden window (owner for the muda context menu; `SetForegroundWindow` on it before `TrackPopupMenu`).
    pub fn destroy(&mut self)
        // Removes the icon (`NIM_DELETE`) and destroys the window.
}
```

#### `vox_win::registry`

```rust
pub struct ExplorerMenuSpec { command: String, label: String, icon_path: Option<String> }
    // Explorer verb install parameters.
pub struct RegistryLocation { classes_base: String, run_key: String, startup_approved_key: String }
    // Where the integrations live (all under `HKCU`).
impl Default for RegistryLocation {
}
pub fn run_command_matches(stored: &str, exe: &Path, args: &[&str]) -> bool
    // The launch-at-login idempotency test (integrations.md §4.2; D-INT-9, D-INT-10).
pub enum MenuState { Absent, Stale, Current }
pub fn quote_windows_arg(arg: &str) -> String
    // `quoteWindowsArg` (MSVCRT / `CommandLineToArgvW` rules; unquoted when no whitespace and no `"`).
pub fn build_verb_command(exe: &str, extra_args: &[&str]) -> String
    // `<quote(exe)> [quote(arg)…] --read-file "%1"`.
pub fn set_explorer_menu(enabled: bool, spec: &ExplorerMenuSpec) -> Result<(), String>
    // Installs (all 9 extensions; any failure rolls everything back) or removes the verb.
pub fn set_explorer_menu_at(loc: &RegistryLocation, enabled: bool, spec: &ExplorerMenuSpec) -> Result<(), String>
    // [`set_explorer_menu`] under `loc` (tests).
pub fn explorer_menu_state(expected_command: &str) -> MenuState
    // `explorerMenuState(expected)` (case-insensitive command comparison): 0 of 9 `command` keys → `Absent`; all 9 equal ignoring case → `Current`; anything else (8 of 9, one different command) → `Stale`.
pub fn explorer_menu_state_at(loc: &RegistryLocation, expected_command: &str) -> MenuState
    // [`explorer_menu_state`] under `loc` (tests).
pub fn is_explorer_menu_installed() -> bool
    // The `command` key exists for all nine extensions.
pub fn is_explorer_menu_installed_at(loc: &RegistryLocation) -> bool
    // [`is_explorer_menu_installed`] under `loc` (tests).
pub fn apply_launch_at_login(enabled: bool, start_hidden: bool, value_name: &str, exe: &Path) -> Result<(), String>
    // `applyLaunchAtLogin(enabled, startHidden)` under `HKCU\…\Run` value `value_name` (`Vox` / `Vox (dev)`): enabling is idempotent when [`run_command_matches`] (same exe case-insensitively, same args exactly), otherwise writes `"<exe>"[ --hidd…
pub fn apply_launch_at_login_at(loc: &RegistryLocation, enabled: bool, start_hidden: bool, value_name: &str, exe: &Path) -> Result<(), String>
    // [`apply_launch_at_login`] under `loc` (tests).
pub fn apps_use_light_theme() -> Option<bool>
    // `AppsUseLightTheme` (`Some(false)` = dark apps); `None` when unreadable.
pub struct SystemProxy { url: String, bypass: Vec<String>, bypass_local: bool }
    // The Windows (WinINET) proxy for the current user, for the ElevenLabs client (BRIEF.md "honour the Windows system proxy"; ARCHITECTURE.md §8 D-NET-1).
pub fn system_proxy() -> Option<SystemProxy>
    // Reads, in order: the `HTTPS_PROXY`/`https_proxy`/`ALL_PROXY` environment variables (a developer override), then `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`: `ProxyEnable == 1` and `ProxyServer` — either `host:port` o…
pub fn parse_proxy_server(value: &str) -> Option<String>
    // Pure parser for a `ProxyServer` value (tested): `"proxy:8080"` → `http://proxy:8080`; `"http=a:1;https=b:2"` → `http://b:2`; `""` → `None`.
```

#### `vox_win::selection`

```rust
pub struct SelectionConfig { right_click_chip: bool, selection_popup: bool, excluded_apps: Vec<String>, restore_clipboard: bool }
    // The settings the service consumes.
impl From<&Settings> for SelectionConfig {
}
pub enum NoticeKind { Info, Error }
pub enum SelectionEvent { Read { text: String, source: ReadSource, foreground: Option<ForegroundInfo> }, NoSelection(ReadSource), Notice { kind: NoticeKind, message: String }, ChipShow { x: i32, y: i32, mode: ChipMode }, ChipHide, HookStatus(bool) }
    // What the service reports.
pub type SelectionEvents = Arc<dyn Fn(SelectionEvent) + Send + Sync>
    // Event listener.
pub struct SelectionService
    // Thread-safe handle.
impl SelectionService {
    pub fn new(events: SelectionEvents) -> Self
        // Creates the service and its two long-lived threads (the selection consumer and the clipboard worker, module docs).
    pub fn start(&self, cfg: SelectionConfig)
        // Starts with the given settings: installs the hook only if a chip trigger is on (each install attempt reports `HookStatus`).
    pub fn apply_settings(&self, cfg: SelectionConfig)
        // Re-applies settings (may hide a chip; starts/stops the hook; retries a failed hook, D-SEL-1).
    pub fn stop(&self)
        // Stops the hook, aborts a running capture, starts flushing a pending clipboard restore.
    pub fn flush(&self, timeout: Duration)
        // Waits (≤ `timeout`) for an in-flight capture and pending restore to finish: posts `Flush{reply}` to the clipboard worker and waits on the reply.
    pub fn hook_available(&self) -> bool
        // `false` only after an install attempt failed; `true` before any attempt (chips off at launch, shots mode) and after a successful one.
    pub fn is_busy(&self) -> bool
        // A capture is queued or running on the clipboard worker (an `AtomicBool` the worker sets).
    pub fn read_selection(&self, source: ReadSource)
        // **Never blocks**: enqueues a capture on the clipboard worker and returns; ignored (debug log only, no event) while a capture is queued or running.
    pub fn set_chip_bounds(&self, bounds: Option<PhysRect>)
        // The chip window's real bounds while visible (physical px), `None` as soon as it starts hiding.
    pub fn chip_activated(&self)
        // The chip UI got a left press (non-blocking enqueue; deduped with the hook's own detection within 300 ms).
    pub fn chip_dismissed(&self)
        // The chip UI got a non-left press.
}
```

#### `vox_win::shell`

```rust
pub fn open_external(url: &str) -> Result<(), crate::WinError>
    // Opens an http(s) URL in the default browser (`ShellExecuteW open`).
pub fn reveal_path(path: &Path) -> Result<(), crate::WinError>
    // Opens a folder in Explorer.
pub fn show_notification(title: &str, body: &str) -> Result<(), crate::WinError>
    // A silent OS notification (title `Vox`, body): `ToastNotificationManager::CreateToastNotifierWithId(APP_ID)` with a `ToastGeneric` template (two text lines) and `<audio silent="true"/>`.
pub fn register_app_user_model(id: &str, display_name: &str, icon_png: &Path) -> Result<(), crate::WinError>
    // Registers the App User Model ID for toast notifications of an unpackaged app (R-Installer: no Start-menu shortcut carries the AUMID): writes `HKCU\Software\Classes\AppUserModelId\<id>` with `DisplayName = display_name` and `IconUri = icon_…
pub fn save_file_dialog(owner: Option<isize>, title: &str, default_path: &Path, filter_name: &str, extension: &str) -> Option<PathBuf>
    // `IFileSaveDialog` on a dedicated STA thread (blocking the caller): title, default path, one filter `(name, ext)`.
pub fn set_app_user_model_id(id: &str)
    // `SetCurrentProcessExplicitAppUserModelID`.
pub fn documents_folder() -> Option<PathBuf>
    // `FOLDERID_Documents`.
pub fn set_per_monitor_dpi_aware()
    // Declares Per-Monitor-V2 DPI awareness for the process (before any window exists).
pub fn message_box(title: &str, body: &str)
    // A blocking native message box (`MessageBoxW`, `MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND`, no owner).
```

#### `vox_win::single_instance`

```rust
pub struct ForwardedArgs { argv: Vec<String>, cwd: PathBuf }
    // What a later launch forwarded.
pub enum InstanceRole { Primary(InstanceServer), Secondary }
    // The lock outcome.
pub struct InstanceServer
    // The primary's pipe server: owns the per-user mutex and the first pipe instance (not reading yet).
pub type ForwardHandler = Arc<dyn Fn(ForwardedArgs) + Send + Sync>
    // Listener callback (called on the pipe thread).
impl InstanceServer {
    pub fn listen(self, on_args: ForwardHandler) -> Result<InstanceListener, crate::WinError>
        // Starts the pipe thread.
}
pub struct InstanceListener
    // Keeps the pipe thread alive.
impl InstanceListener {
    pub fn shutdown(self)
}
pub fn acquire(key: &str) -> Result<InstanceRole, crate::WinError>
    // Takes the per-user lock for `key`; as primary also creates the first pipe instance (module docs).
pub fn forward_to_primary(key: &str, args: &ForwardedArgs) -> Result<(), crate::WinError>
    // Sends argv + cwd to the primary of the **same user** (waits ≤ 5 s for the pipe; verifies the server process's user SID before `AllowSetForegroundWindow` and writing).
pub fn electron_instance_running(profile_root: &Path) -> bool
    // Is an **Electron** build of Vox running on this profile? (BRIEF.md "Do not run both builds at once"; ARCHITECTURE.md §4.18.) Chromium's process singleton (which Electron's `requestSingleInstanceLock` uses) keeps a message-only window of cl…
```

#### `vox_win::system`

```rust
pub enum SystemEvent { DisplaysChanged, ThemeChanged, LocaleChanged, ClockChanged, Resumed, CloseRequested, MotionChanged }
pub type SystemHandler = Arc<dyn Fn(SystemEvent) + Send + Sync>
pub type SessionEndHandler = Arc<dyn Fn() + Send + Sync>
    // Must return only after the synchronous save.
pub struct SystemWatcher
impl SystemWatcher {
    pub fn spawn(on_event: SystemHandler, on_session_end: SessionEndHandler) -> Result<SystemWatcher, crate::WinError>
    pub fn shutdown(&self)
}
pub fn user_locale() -> vox_core::locale::Locale
    // The user's number/date conventions (`GetLocaleInfoEx(LOCALE_NAME_USER_DEFAULT, …)`): `LOCALE_SDECIMAL`, `LOCALE_STHOUSAND` (empty when `LOCALE_SGROUPING` is "0"), 12-hour when `LOCALE_STIMEFORMAT` contains `h` but no `H`, day-first when `L…
```

#### `vox_win::window`

```rust
pub type Hwnd = isize
    // A raw window handle (`HWND` as `isize`, as winit's `raw-window-handle` exposes it).
pub struct PhysRect { x: i32, y: i32, width: i32, height: i32 }
    // A rectangle in physical pixels.
impl PhysRect {
    pub fn contains(&self, x: i32, y: i32) -> bool
}
pub struct MonitorInfo { bounds: PhysRect, work_area: PhysRect, scale: f64, primary: bool, id: isize }
    // One monitor.
pub type OverlayCloseHandler = std::sync::Arc<dyn Fn() + Send + Sync>
    // Called when an overlay receives `WM_CLOSE` (Alt+F4 is impossible without focus; this is taskkill without `/f`, an installer, …).
pub fn install_overlay_subclass(hwnd: Hwnd, on_close: OverlayCloseHandler) -> Result<(), crate::WinError>
    // Installs the overlay subclass (`SetWindowSubclass`, `Win32_UI_Shell`) on a widget/chip HWND, once, right after the window was first created (vox-app pre-creates both overlays off-screen at startup, before any can be seen), and applies the…
pub fn assert_topmost(hwnd: Hwnd, raise: bool)
    // Re-asserts the z-order of a window Slint just showed: `SetWindowPos(HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)`, then with `raise` (the chip) `HWND_TOP` within the topmost band so it sits above an open context menu.
pub fn is_visible(hwnd: Hwnd) -> bool
pub fn set_bounds(hwnd: Hwnd, r: PhysRect)
    // Moves and resizes in **one** `SetWindowPos(.., SWP_NOACTIVATE | SWP_NOZORDER)` (the widget's anchored resize and placement apply position and size together; Slint follows `WM_SIZE`).
pub fn window_rect(hwnd: Hwnd) -> Option<PhysRect>
pub fn set_caption_theme(hwnd: Hwnd, dark: bool, caption_rgb: u32)
    // Dark title bar + caption colour (`DWMWA_USE_IMMERSIVE_DARK_MODE`, `DWMWA_CAPTION_COLOR` as 0x00BBGGRR).
pub fn bring_to_front(hwnd: Hwnd)
    // Brings a normal window forward (restore if minimized, show, `SetForegroundWindow`).
pub fn cursor_pos() -> Option<(i32, i32)>
pub fn left_button_down() -> bool
    // The left button is physically down (`GetAsyncKeyState(VK_LBUTTON)`), used to detect the end of a widget drag.
pub fn monitor_from_point(x: i32, y: i32) -> Option<MonitorInfo>
    // Monitor nearest a point.
pub fn monitor_from_rect(r: PhysRect) -> Option<MonitorInfo>
    // Monitor best matching a rect.
pub fn primary_monitor() -> Option<MonitorInfo>
    // The primary monitor.
pub fn monitors() -> Vec<MonitorInfo>
    // Every monitor (`EnumDisplayMonitors`), primary first.
pub fn normal_placement(hwnd: Hwnd) -> Option<(PhysRect, bool)>
    // The restored (normal) rectangle of a top-level window in screen pixels and whether it is maximized (`GetWindowPlacement`, `rcNormalPosition` converted from workspace to screen coordinates).
pub fn set_normal_placement(hwnd: Hwnd, normal: PhysRect, maximized: bool, show: bool)
    // Sets the restored rectangle and the maximized flag (`SetWindowPlacement`).
pub fn reduce_motion() -> bool
    // OS "reduce motion" (`SPI_GETCLIENTAREAANIMATION` off).
```


### A.7 `vox-bridge`

#### `vox_bridge::pipeline`

```rust
pub struct HttpResponse { status: u16, headers: Vec<(String, String)>, body: Vec<u8>, close: bool }
    // A response to write.
pub enum Route { Health { head: bool }, Read, Command }
    // A routed request.
pub type Headers<'a> = &'a [(String, String)]
    // The request headers in arrival order (names as received).
pub fn check_request(method: &str, url: &str, headers: Headers<'_>, bound_port: u16) -> Result<Route, HttpResponse>
    // Steps 1–7 (method, Host, Origin, path, method per route, client header).
pub fn check_body_headers(headers: Headers<'_>) -> Result<(), HttpResponse>
    // Step 8 pre-checks before reading the body: content type, declared length.
pub fn parse_json_body(body: &[u8]) -> Result<serde_json::Map<String, Value>, HttpResponse>
    // Body → JSON object: lossy UTF-8, then `vox_core::json::parse_lenient_with(text, OutOfRange::Null)` (BOM stripped, lone surrogate escapes → U+FFFD, an out-of-range literal like `1e400` → `null`, which reproduces every old outcome: `"text":1…
pub fn parse_read_body(body: &serde_json::Map<String, Value>) -> Result<BridgeReadRequest, HttpResponse>
    // `POST /read` body (integrations.md §1.6.2): `text` must be a non-blank string (400 `empty-text`); `url` kept if a string ≤ 2048 matching `^(https?|file)://`; `title` trimmed, ≤ 300, dropped if blank.
pub fn parse_command_body(body: &serde_json::Map<String, Value>) -> Result<PlayerCommand, HttpResponse>
    // `POST /command` body via `vox_core::player::parse_bridge_command` (400 with its message).
pub fn json_response(status: u16, body: &Value, head: bool) -> HttpResponse
    // A JSON response with the four standard headers.
pub fn error_response(status: u16, message: &str, code: Option<ReadErrorCode>) -> HttpResponse
    // `{ "ok": false, "error": msg, "code"?: code }`.
pub fn health_body(status: &crate::server::BridgeStatus) -> Value
    // `{ "ok": true, "app": "Vox", "version": "<x.y.z>", "playing": bool }`.
```

#### `vox_bridge::server`

```rust
pub struct BridgeStatus { app_name: String, version: String, playing: bool }
    // What `/health` reports.
pub trait BridgeHandler: Send + Sync { fn read(&self, req: BridgeReadRequest, deadline: Instant) -> Option<ReadResult>, fn command(&self, cmd: PlayerCommand), fn status(&self) -> BridgeStatus }
    // Implemented by vox-app (core glue).
pub struct StartResult { ok: bool, port: u16, error: Option<String> }
    // `start(port)` result.
pub struct BridgeLimits { headers: Duration, keep_alive: Duration, body: Duration, handler: Duration, idle: Duration, max_connections: usize, max_header_bytes: usize }
    // Connection limits and timeouts (integrations.md §1.7).
impl Default for BridgeLimits {
}
pub struct BridgeServer
    // The bridge server (one per app).
impl BridgeServer {
    pub fn new(handler: Arc<dyn BridgeHandler>) -> Self
    pub fn with_limits(handler: Arc<dyn BridgeHandler>, limits: BridgeLimits) -> Self
        // A server with custom limits (socket tests).
    pub fn start(&self, port: u16) -> StartResult
        // Starts (or restarts on a different port; same port while running → no-op `ok`).
    pub fn stop(&self)
        // Stops listening and hard-closes every connection (module docs).
    pub fn running(&self) -> bool
    pub fn port(&self) -> u16
        // Bound port while running, else the last requested port (0 if never started).
}
```


### A.8 `vox-art`

#### `vox_art`

```rust
pub struct Art { width: u32, height: u32, rgba: Vec<u8> }
    // A rendered image: straight (not premultiplied) RGBA8, row-major, `width * height * 4` bytes.
impl Art {
    pub fn save_png(&self, path: &Path) -> std::io::Result<()>
        // Writes the image as an 8-bit RGBA PNG.
    pub fn encode_png(&self) -> std::io::Result<Vec<u8>>
        // Encodes the image as an 8-bit RGBA PNG in memory.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4]
        // Reads back pixel (x, y) as [r, g, b, a].
}
pub fn palette_for(seed: &str) -> Palette
    // The palette a seed maps to.
pub fn thumbnail(seed: &str, width: u32, height: u32) -> Art
    // Renders the silk thumbnail for `seed` at any size and aspect ratio.
pub fn backdrop(width: u32, height: u32, dark: bool) -> Art
    // Renders the window backdrop: very dark navy with soft silver-blue silk (`dark`), or the light "Paper" variant.
pub fn accent(seed: &str) -> [u8; 3]
    // A representative colour for tiny dots (sRGB).
pub mod debug {
    pub const STYLES: [&str; 5] = ["drape", "ribbons", "crease", "arcs", "bloom"]
    pub fn style_name(seed: &str) -> &'static str
        // The composition style a seed uses.
    pub fn thumbnail_with(seed: &str, palette: crate::Palette, style: usize, w: u32, h: u32) -> crate::Art
        // Renders `seed` with a forced palette and style (index into `STYLES`).
    pub fn variant_scores(seed: &str, palette: crate::Palette, style: usize) -> (Vec<f32>, u32)
        // The structure score of each layout variant of `seed` (see `pick_variant`), and the one chosen.
}
```


### A.9 `vox-app`

#### `vox_app::app`

```rust
pub fn run() -> anyhow::Result<ExitCode>
    // The whole program.
pub fn should_show_main_at_start(runtime: &Runtime, args: &vox_core::cli::CliArgs, settings: &vox_core::settings::Settings) -> bool
    // Startup visibility (shell.md §3.4): not shots mode, no `--hidden`, no read action, and (`!startMinimized` or not onboarded).
pub fn startup_route(settings: &vox_core::settings::Settings, key_problem: bool) -> vox_core::app::AppRoute
    // The route the main window opens on (D-KEY-3): onboarding when not onboarded; `settings/account` when onboarded but a stored key could not be migrated/read and no key is available; else home.
```

#### `vox_app::command`

```rust
pub struct RequestId(pub u64)
    // Correlates a UI request with its `UiUpdate` answer.
pub enum RevealTarget { UserData, Extension }
    // Which folder to open in Explorer.
pub enum TrayAction { Open, ReadClipboard, TogglePause, Stop, OpenPage(AppPage), SetRightClickChip(bool), SetWidgetEnabled(bool), SetHotkeysEnabled(bool), Quit }
    // Tray menu actions (shell.md §7.3).
pub enum WindowKind { Main, Widget, Chip }
    // Which native window an event is about.
pub enum WindowEvent { MainState { visible: bool, focused: bool }, WidgetVisible(bool), WidgetDragged { x: i32, y: i32 }, CloseRequested(WindowKind) }
    // Facts the UI thread reports about the native windows.
pub enum BridgeCall { Read { request: BridgeReadRequest, reply: Sender<ReadResult> } }
    // A call from a bridge connection thread.
pub enum CatalogResult { Voices { generation: u64, refresh: bool, result: Result<Arc<Vec<Voice>>, String> }, Models { generation: u64, result: Result<Arc<Vec<TtsModel>>, String> }, Account { generation: u64, forced: bool, result: Result<Option<AccountInfo>, String> }, ApiKey { req: RequestId, key: String, result: ApiKeyResult }, Preview { voice_id: String, result: Result<AudioBytes, String> } }
    // Results of catalog / account work done on worker threads.
pub enum ReplayOrigin { Request, TogglePlayback }
    // Who asked for a replay (decides the failure feedback when the cached audio turns out to be unusable).
pub enum Command { ReadText { req: Option<RequestId>, text: String, options: ReadTextOptions }, ReadClipboard { req: Option<RequestId> }, ClipboardText { req: Option<RequestId>, text: Option<String> }, Replay { req: Option<RequestId>, read_id: String }, ReplayLoaded { req: Option<RequestId>, read_id: String, origin: ReplayOrigin, chunks: Option<Vec<CachedChunk>> }, ReadSelection(vox_core::history::ReadSource), ReadPrepared(vox_core::read::ReadRequest), TogglePlayback, Player(PlayerCommand), CycleRate, UpdateSettings { req: Option<RequestId>, patch: serde_json::Value }, SetApiKey { req: RequestId, key: String }, ClearApiKey { req: RequestId }, SuspendHotkeys(bool), FetchVoices { refresh: bool }, FetchModels { refresh: bool }, RefreshCredits { force: bool }, PreviewVoice { voice_id: String }, StopPreview, QueryHistory { req: RequestId, query: HistoryQuery, with_grand_total: bool }, QueryHome { req: RequestId }, QueryAnalytics { req: RequestId, range: AnalyticsRange }, RemoveReads { req: RequestId, ids: Vec<String> }, ClearHistory { req: RequestId }, ToggleFavorite { req: RequestId, id: String }, ExportHistory { req: RequestId, owner: Option<isize> }, CountExpiring { req: RequestId, days: u32 }, QueryCacheStats { req: RequestId }, ClearCache { req: RequestId }, CopyText { req: RequestId, text: String }, SaveAnalyticsRange(AnalyticsRange), SaveWindowState(vox_store::window_state::WindowState), ShowMain(Option<AppRoute>), HideMain, OpenExternal { url: String, req: Option<RequestId> }, RevealPath(RevealTarget), QueryAppInfo, Quit, Window(WindowEvent), Tray(TrayAction), Hotkey(HotkeyAction), Selection(SelectionEvent), Media(MediaAction), Bridge(BridgeCall), Cli { argv: Vec<String>, cwd: PathBuf, from_second_instance: bool }, System(SystemEvent), SessionEnd(Sender<()>), Toast(Toast), Engine(EngineEvent), CacheSaved { read_id: String, result: Result<(), String> }, PlayerReport(PlayerSnapshot), PreviewReport(PlayerSnapshot), PreviewFailed { message: String }, PlayerFailed { session_id: Option<String>, message: String }, HistoryChanged, Catalog(CatalogResult) }
    // Everything the core thread handles.
pub struct CoreHandle
    // Cloneable sender used by every source.
impl CoreHandle {
    pub fn new(tx: Sender<Command>) -> Self
    pub fn send(&self, cmd: Command)
        // Enqueues a command (dropped silently after the core exited).
    pub fn request_id(&self) -> RequestId
        // A fresh request id.
}
```

#### `vox_app::core::bridge`

```rust
pub struct BridgeGlue { core: CoreHandle, playing: Arc<AtomicBool> }
    // The handler given to `BridgeServer::new`.
impl BridgeHandler for BridgeGlue {
}
```

#### `vox_app::core::catalog`

```rust
pub fn preview_url<'a>(voices: &'a [Voice], voice_id: &str) -> Option<&'a str>
    // The `previewUrl` of a voice id, if known.
```

#### `vox_app::core::credits`

```rust
pub fn read_settled(prev: PlayerStatus, next: PlayerStatus) -> bool
    // A read settled: previous status loading/playing/paused and next idle/ended/error.
pub enum CreditsRefresh { Normal, Forced }
    // What a wake / reset check asks for.
pub struct WakeThrottle { last_wake_ms: f64, forced_reset: Option<f64> }
    // The §1.5 wake throttle and the shared `forcedReset` memory (pure; monotonic ms and epoch seconds injected).
impl WakeThrottle {
    pub fn on_wake(&mut self, now_ms: f64, now_unix: f64, next_reset_unix: Option<f64>) -> Option<CreditsRefresh>
        // A `MainState` transition counted as a wake at `now_ms`; `next_reset_unix` from the current account.
    pub fn on_reset_timer(&mut self, next_reset_unix: f64) -> Option<CreditsRefresh>
        // The `CreditsReset` timer fired for `next_reset_unix`.
}
```

#### `vox_app::core::engine_sink`

```rust
pub struct PlayerSink { player: Player }
    // `TtsSink` → reading player.
impl TtsSink for PlayerSink {
}
pub fn engine_events(core: CoreHandle) -> Arc<dyn Fn(EngineEvent) + Send + Sync>
    // Engine events → the core.
pub fn player_events(core: CoreHandle) -> Arc<dyn Fn(PlayerEvent) + Send + Sync>
    // Reading-player events → the core.
pub fn preview_events(core: CoreHandle) -> Arc<dyn Fn(PlayerEvent) + Send + Sync>
    // Preview-player events → the core (a preview-player failure ends the sample with `PreviewUpdate::Failed`, which the Voices / onboarding controller turns into its `voice-preview` toast).
```

#### `vox_app::core::hotkeys`

```rust
pub struct HotkeyState { suspended: bool }
    // Suspension state.
pub struct HotkeyEffects { unregister: bool, register: bool, arm_timer: bool, cancel_timer: bool, notify_resumed: bool }
    // What the core must do after a suspension transition.
impl HotkeyState {
    pub fn suspend(&mut self) -> HotkeyEffects
        // `SuspendHotkeys(true)`.
    pub fn resume(&mut self) -> HotkeyEffects
        // `SuspendHotkeys(false)`.
    pub fn on_timer(&mut self) -> HotkeyEffects
        // The 30 s safety timer fired.
}
```

#### `vox_app::core::integrations`

```rust
pub fn explorer_needs_write(enabled: bool, state: vox_win::registry::MenuState) -> bool
    // Decides whether the startup sync must write (pure).
```

#### `vox_app::core::lifecycle`

```rust
pub const SHUTDOWN_BUDGET: Duration = Duration::from_millis(vox_core::constants::SHUTDOWN_TIMEOUT_MS)
    // Flush budget.
pub const FORCE_EXIT_AFTER: Duration = Duration::from_millis(vox_core::constants::FORCE_EXIT_MS)
    // Hard exit after cleanup (UI side, armed on `UiUpdate::Quit`).
pub const WATCHDOG_AFTER: Duration = Duration::from_millis(vox_core::constants::SHUTDOWN_WATCHDOG_MS)
    // The core-side watchdog armed by the first quit request.
pub const SERVICE_JOIN: Duration = Duration::from_millis(vox_core::constants::SERVICE_SHUTDOWN_JOIN_MS)
    // Join limit for each of the players' and SMTC's shutdown helpers.
```

#### `vox_app::core::media`

```rust
pub fn media_state(snapshot: &PlayerSnapshot, media_keys: bool) -> MediaState
    // Pure mapping from a snapshot (tested).
```

#### `vox_app::core`

```rust
pub struct Services { runtime: Runtime, settings: SettingsStore, history: HistoryStore, cache: AudioCache, client: ElevenLabsClient, engine: TtsEngine, player: Player, preview_player: Player, selection: SelectionService, hotkeys: Option<HotkeyService>, media: Option<MediaSession>, bridge: Arc<BridgeServer> }
    // Every service handle the core owns (built by `app::run`, moved into the core thread).
pub struct Core
    // The core's state.
impl Core {
    pub fn spawn(services: Services, rx: Receiver<Command>, handle: CoreHandle, ui: UiSink) -> JoinHandle<()>
        // Spawns the core thread; returns its join handle (joined after the event loop ends).
}
pub enum OwedAnswer { ReadStarted(RequestId), Home(RequestId), Analytics(RequestId), HistoryPage(RequestId), SettingsSaved(RequestId), ApiKey(RequestId), Done(RequestId), Exported(RequestId), CacheStats(RequestId), ExpiringCount(RequestId), FavoriteToggled(RequestId) }
    // The answer a request command is owed (D-ARCH-3).
pub fn owed_answer(cmd: &Command) -> Option<OwedAnswer>
    // Which answer `cmd` owes its requester (`None` for commands without a request id).
pub fn failure_update(owed: OwedAnswer) -> UiUpdate
    // The failure answer for a command whose handler panicked (`Something went wrong`; reads answer `ReadResult::internal_error()`).
```

#### `vox_app::core::queries`

```rust
pub fn export_file_name(now: DateTime<Utc>) -> String
    // `Vox-history-YYYY-MM-DD.json` with the **UTC** date (parity, contracts.md §8 #3).
```

#### `vox_app::core::reactions`

```rust
pub struct Reactions { hotkeys: bool, selection: bool, bridge: bool, explorer_menu: bool, launch_at_login: bool, theme: bool, hide_widget: bool, reposition_widget: bool, retention: bool, prune_cache: bool, media_keys: bool, player_rate: bool, player_volume: bool, key_changed: bool }
    // Which reactions apply (pure; tested).
pub fn diff(next: &Settings, prev: &Settings, key_replaced: bool, shots: bool) -> Reactions
    // Compares `next` with `prev`.
```

#### `vox_app::core::reader`

```rust
pub struct ReaderState { sessions: Sessions, last_snapshot: PlayerSnapshot }
    // Reader state kept by the core.
pub trait Reader { fn read(&mut self, req: ReadRequest) -> ReadResult, fn read_clipboard(&mut self, req: Option<RequestId>), fn on_clipboard_text(&mut self, req: Option<RequestId>, text: Option<String>), fn replay(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin), fn on_replay_loaded(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin, chunks: Option<Vec<CachedChunk>>), fn read_file(&mut self, path: &str, cwd: &std::path::Path), fn toggle_playback(&mut self), fn command(&mut self, cmd: PlayerCommand), fn on_engine_event(&mut self, ev: EngineEvent), fn on_cache_saved(&mut self, read_id: &str, result: Result<(), String>), fn on_player_report(&mut self, snapshot: PlayerSnapshot), fn prune_cache(&mut self, keep: &[String]) -> Vec<String> }
    // The reader's entry points.
pub fn new_record(read_id: &str, now_ms: f64, text: &str, store_text: bool, req: &ReadRequest, voice_id: &str, voice_name: &str, model_id: &str) -> ReadRecord
    // The record a fresh read creates (reader-playback.md §2.3 step 8).
```

#### `vox_app::core::retention`

```rust
pub fn reconcile_plan(records: &[ReadRecord], cached_ids: &HashSet<String>, ids_with_text: &HashSet<String>) -> (Vec<String>, Vec<String>)
    // The `cached = false` fixes and the ids to redact (pure).
pub enum RetentionRun { Interval, AfterChange }
    // A retention run the core must perform now.
pub struct Schedule { next_interval_ms: Option<f64>, change_due_ms: Option<f64> }
    // The retention timers as a pure schedule over monotonic milliseconds.
impl Schedule {
    pub fn on_change(&mut self, now_ms: f64)
        // `retentionDays` changed at `now_ms`: (re)arm the 60 s change run.
    pub fn due(&mut self, now_ms: f64) -> Option<RetentionRun>
        // The run due at `now_ms`, if any (re-arms the interval).
    pub fn cancel(&mut self)
        // Shutdown: cancel both.
}
```

#### `vox_app::core::selection`

```rust
pub fn request_for(text: String, source: ReadSource, foreground: Option<&ForegroundInfo>) -> ReadRequest
    // The read request for captured text (pure).
```

#### `vox_app::core::sessions`

```rust
pub struct TrackedSession { session_id: String, read_id: String, record_saved: bool, from_cache: bool, base_listened_sec: f64, base_completed: bool, listened_sec: f64, completed: bool, max_position_sec: f64, partial: bool, last_status: Option<PlayerStatus>, last_write_ms: f64, written: Option<Written> }
    // One engine session the reader started.
pub struct Written { listened_sec: f64, completed: bool, max_position_sec: f64 }
    // The values last written for a session.
pub struct Sessions
    // Up to 16 sessions in insertion order.
impl Sessions {
    pub fn track(&mut self, session_id: String, read_id: String, record_saved: bool, from_cache: bool, base_listened_sec: f64, base_completed: bool)
        // `track(...)` right after the engine accepted a session (before any report can be processed).
    pub fn latest_for(&mut self, read_id: &str) -> Option<&mut TrackedSession>
        // `latestSessionFor(readId)`.
    pub fn on_state(&mut self, snap: &PlayerSnapshot, now_ms: f64) -> Option<PendingWrite>
        // Folds one player report (`snap.state` for the session `snap.state.session_id`; `snap.warning` → `partial` for a non-cached session, module docs); returns a [`PendingWrite`] when a history write is due.
    pub fn persist_all(&mut self, now_ms: f64, current: Option<&PlayerSnapshot>) -> Vec<PendingWrite>
        // `persistPlayback()` (quit / log-off, D-RP-6): first folds `current` (the player's exact snapshot at that moment) into the session matching `current.state.session_id` — same rules as [`Sessions::on_state`] but without the 2 s throttle — the…
}
pub struct PendingWrite { session_id: String, read_id: String, next: Written, previous_listened_sec: f64 }
    // A due write for one session.
impl PendingWrite {
    pub fn patch(&self, record: &ReadRecord, hour_key: &str) -> RecordPatch
        // `writeSession` patch for the current record: `{listenedSec, completed}` + ledger delta at `hour_key` (when > 0) + `maxPositionSec` when it exceeds the record's.
}
impl Sessions {
    pub fn mark_written(&mut self, session_id: &str, written: Written, now_ms: f64)
}
```

#### `vox_app::core::timers`

```rust
pub enum TimerId { RetentionInterval, RetentionChange, HotkeySuspend, HistoryChangedThrottle, CreditsAfterRead, CreditsReset, Midnight, MediaTick, PreloadOutput }
    // Every core timer.
pub struct Timers
    // The heap.
impl Timers {
    pub fn set(&mut self, id: TimerId, at: Instant)
        // Arms (or re-arms) `id` at `at`.
    pub fn cancel(&mut self, id: TimerId)
    pub fn is_set(&self, id: TimerId) -> bool
    pub fn next_deadline(&self) -> Option<Instant>
        // The earliest deadline.
    pub fn take_due(&mut self, now: Instant) -> Vec<TimerId>
        // Removes and returns every timer due at `now`.
}
```

#### `vox_app::core::toasts`

```rust
pub struct SurfaceState { main_visible: bool, main_focused: bool, widget_visible: bool }
    // What the router needs to know about the windows (kept up to date from `WindowEvent`s).
pub enum ToastRoute { AppOnly, ShowWidget, OsNotification, Nowhere }
    // Where a toast goes besides the two UI surfaces.
pub fn route(toast: &Toast, surfaces: SurfaceState, widget_enabled: bool) -> ToastRoute
    // Pure routing decision.
pub struct ToastRouter { surfaces: SurfaceState, ui_ready: bool, notify_failure_logged: bool }
    // The router's state (pure; tested: routing before the UI is ready is deferred and resolved by the first `MainState`).
impl ToastRouter {
    pub fn on_toast(&mut self, toast: &Toast, widget_enabled: bool) -> Option<ToastRoute>
        // A new core toast: returns its extra route now (`None` = deferred until the UI is ready).
    pub fn on_main_state(&mut self, visible: bool, focused: bool, widget_enabled: bool) -> Vec<(Toast, ToastRoute)>
        // A `MainState` report: updates the surfaces; on the first one, returns the routes of the deferred toasts.
}
```

#### `vox_app::runtime`

```rust
pub struct Runtime { flags: RuntimeFlags, profile: Profile, exe_path: PathBuf, extension_dir: PathBuf, is_packaged: bool, runtime_label: String, build_hash: String, argv: Vec<String>, cwd: PathBuf }
    // Everything the rest of the app needs to know about how it was started.
impl Runtime {
    pub fn detect() -> anyhow::Result<Runtime>
        // Reads `VOX_*` from the environment (`RuntimeFlags::from_lookup(std::env::var)`; `VOX_SHOTS_DIR` made absolute against the cwd), resolves the profile (`%APPDATA%\Vox` or `Vox-Demo`), the exe and extension paths.
    pub fn instance_key(&self) -> String
        // The single-instance key: `com.vox.reader`, or `com.vox.reader-Demo` for the demo profile.
    pub fn compiled_build_hash() -> &'static str
        // The build hash compiled in by build.rs.
}
```

#### `vox_app::shots`

```rust
pub const DEMO_TEXT: &str = "The quiet revolution in personal computing was never about faster chips. It was about the moment…
    // The fixed demo paragraph the harness reads (old `shots.ts` `DEMO_TEXT`).
pub const LOADING_TEXT: &str = "Section 4.2: The licensee shall not sublicense, assign, or otherwise transfer any rights granted…
    // The licence clause of the synthetic loading session (old `shots.ts` `LOADING_TEXT`).
pub fn start(out_dir: PathBuf, core: CoreHandle) -> Rc<Cell<Option<u8>>>
    // Starts the harness on the UI thread (module docs).
pub fn looks_blank(rgba: &[u8], width: u32, height: u32) -> bool
    // The blank test (shell.md §9 step 3): zero size → blank; else sample every `max(1, floor(w·h / 4000))`-th pixel of the straight RGBA8 buffer, quantise each channel to its top 4 bits, and count distinct values; more than 3 distinct → not bla…
```

#### `vox_app::tray`

```rust
pub struct Tray
    // The tray icon and its menu.
impl Tray {
    pub fn create(core: CoreHandle) -> anyhow::Result<Tray>
    pub fn update(&mut self, view: TrayView)
        // Applies a desired state (cheap when unchanged).
    pub fn refresh_dpi(&mut self)
        // Displays / DPI changed: re-pick the icon size.
    pub fn destroy(&mut self)
        // Removes the icon (end of the quit sequence).
}
```

#### `vox_app::ui::analytics`

```rust
pub struct AnalyticsController
impl AnalyticsController {
    pub fn new(range: vox_core::analytics::AnalyticsRange) -> Self
        // Seeded from `UiInit::analytics_range`.
    pub fn set_range(&mut self, cx: &mut UiCtx, value: &str)
    pub fn retry(&mut self, cx: &mut UiCtx)
    pub fn refresh_credits(&mut self, cx: &mut UiCtx)
    pub fn add_key(&mut self, cx: &mut UiCtx)
    pub fn set_plot_width(&mut self, cx: &mut UiCtx, width: f32)
        // The daily plot's width in logical px (module docs).
}
impl PageController for AnalyticsController {
}
```

#### `vox_app::ui::art`

```rust
pub enum ArtSize { Large, Compact, Dot }
    // Art slots.
impl ArtSize {
    pub fn pixels(self) -> (u32, u32)
        // Render size in pixels (2× the logical slot).
}
pub struct ArtCache
    // UI-thread cache.
impl ArtCache {
    pub fn image(&mut self, seed: &str, size: ArtSize) -> slint::Image
        // The image for `seed` at `size` (placeholder on a miss; the render is queued).
    pub fn backdrop(&mut self, width: u32, height: u32, dark: bool) -> slint::Image
        // The backdrop for a window of `width`×`height` physical pixels.
}
pub fn to_image(art: &vox_art::Art) -> slint::Image
    // Converts rendered art to a Slint image (UI thread).
```

#### `vox_app::ui::chip`

```rust
pub enum PressGate { Activate, Dismiss, Ignore }
    // What a chip press does (pure).
pub fn press_gate(left: bool, hiding: bool) -> PressGate
    // `pressed(left)` while `hiding` → `Ignore`; left → `Activate`; other → `Dismiss`.
pub struct ChipController { mode: ChipMode, hiding: bool, cursor: Option<(i32, i32)>, sizes: [(f64, f64); 2] }
```

#### `vox_app::ui::convert`

```rust
pub enum CreditsSurface { Sidebar, Home, Analytics, SettingsAccount, Onboarding }
    // Which surface a `CreditsData` is for (the copy differs per surface; module docs).
pub fn choices(items: &[(String, String)]) -> ModelRc<ChoiceItem>
    // `[(value, label)]` → `[ChoiceItem]` (accessible label = label).
pub fn keycaps(accel: &str) -> ModelRc<SharedString>
    // Keycap labels for an accelerator (`vox_core::hotkey::display_labels`).
pub fn strings(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString>
    // A string list model.
pub fn credits_data(credits: &CreditsUpdate, surface: CreditsSurface, demo: bool, now_ms: f64) -> CreditsData
    // The credits block for one surface (module docs table).
pub fn toast_data(toast: &vox_core::app::Toast, leaving: bool, key: u64) -> ToastData
    // A core or UI toast for the app stack (`leaving`, `key` from the stack entry) or the widget slot (`false`, its own key).
```

#### `vox_app::ui::history`

```rust
pub struct HistoryController
    // History page state.
impl HistoryController {
    pub fn search_edited(&mut self, cx: &mut UiCtx, text: &str)
    pub fn set_favorites_only(&mut self, cx: &mut UiCtx, on: bool)
    pub fn set_source(&mut self, cx: &mut UiCtx, value: &str)
    pub fn clear_filters(&mut self, cx: &mut UiCtx)
    pub fn toggle_row(&mut self, cx: &mut UiCtx, id: &str)
    pub fn replay(&mut self, cx: &mut UiCtx, id: &str)
    pub fn copy_text(&mut self, cx: &mut UiCtx, id: &str)
    pub fn toggle_favorite(&mut self, cx: &mut UiCtx, id: &str)
    pub fn delete(&mut self, cx: &mut UiCtx, id: &str)
    pub fn open_url(&mut self, cx: &mut UiCtx, id: &str)
    pub fn load_more(&mut self, cx: &mut UiCtx)
    pub fn export(&mut self, cx: &mut UiCtx, owner: Option<isize>)
    pub fn clear_all(&mut self, cx: &mut UiCtx)
    pub fn retry(&mut self, cx: &mut UiCtx)
    pub fn paste_and_read(&mut self, cx: &mut UiCtx)
    pub fn flush(&mut self, cx: &mut UiCtx)
        // Commits a pending undo batch now and dismisses its toast (before `Command::Quit`).
}
impl PageController for HistoryController {
}
```

#### `vox_app::ui::home`

```rust
pub struct HomeController
    // Home page state.
impl HomeController {
    pub fn quick_read(&mut self, cx: &mut UiCtx, text: &str)
    pub fn quick_read_edited(&mut self, cx: &mut UiCtx, text: &str)
    pub fn read_clipboard(&mut self, cx: &mut UiCtx)
    pub fn player_action(&mut self, cx: &mut UiCtx, action: PlayerAction)
    pub fn seek(&mut self, cx: &mut UiCtx, fraction: f32)
        // Seek bar committed at a 0..1 fraction (sent as `Command::Player(SeekFraction)`; the core sanitizes).
    pub fn replay(&mut self, cx: &mut UiCtx, read_id: &str)
        // A Recent row's replay.
    pub fn replay_last(&mut self, cx: &mut UiCtx)
        // "Replay last" (the newest read).
    pub fn pause_live(&mut self, cx: &mut UiCtx)
    pub fn resume_live(&mut self, cx: &mut UiCtx)
    pub fn retry(&mut self, cx: &mut UiCtx)
    pub fn credits_retry(&mut self, cx: &mut UiCtx)
    pub fn open_page(&mut self, cx: &mut UiCtx, page: PageId)
    pub fn open_settings(&mut self, cx: &mut UiCtx, section: &str)
}
impl PageController for HomeController {
}
```

#### `vox_app::ui::hotkey_recorder`

```rust
pub struct Recorder { recording: Option<HotkeyAction>, hint: Option<String>, preview: Vec<String> }
    // Recorder state.
pub fn on_winit_event(recorder: &mut Recorder, event: &slint::winit_030::winit::event::WindowEvent) -> bool
    // The main window's key events while recording: returns true when the event was consumed (the dispatcher then answers `EventResult::PreventDefault`).
```

#### `vox_app::ui`

```rust
pub struct UiInit { settings: Arc<Settings>, dark: bool, reduce_motion: bool, app_info: Arc<AppInfo>, route: AppRoute, show_main: bool, main_placement: (PhysRect, bool), analytics_range: AnalyticsRange, profile: Profile, demo: bool }
    // Everything the UI needs before the first frame (built by `app::run` from the stores; ARCHITECTURE.md §3.6 #5).
pub enum UiToastAction { Page(AppPage) }
    // A UI-local toast action: what the shell does when a toast without a route is actioned.
pub struct ToastSink { pushes: Vec<(Toast, Option<UiToastAction>)>, dismisses: Vec<String> }
    // Toasts raised by controllers during one call; the shell pushes them afterwards.
impl ToastSink {
    pub fn push(&mut self, toast: Toast, local: Option<UiToastAction>)
        // Push (or replace by id) a toast in the app stack; `local` = a UI-local action (Undo, Retry).
    pub fn dismiss(&mut self, id: &str)
        // Programmatic dismiss: the toast is absent at once (no exit animation; ui-main.md §1.7).
}
pub enum NavIntent { None, Search(String) }
    // How a navigation was asked for.
pub struct NavSink { target: Option<(AppRoute, NavIntent)> }
    // A navigation requested by a controller during one call.
impl NavSink {
    pub fn navigate(&mut self, route: AppRoute)
    pub fn navigate_with(&mut self, route: AppRoute, intent: NavIntent)
}
pub struct DialogSpec { id: String, owner: AppPage, title: String, body: String, confirm_label: String, cancel_label: String, danger: bool, busy: bool }
    // The one modal dialog (ids: `history-clear` → History; `remove-key`, `retention`, `library-clear-history` → Settings).
pub struct DialogSink { open: Option<DialogSpec>, busy: Option<(String, bool)>, close: Option<String> }
    // Dialog changes requested by a controller during one call.
impl DialogSink {
    pub fn open(&mut self, spec: DialogSpec)
    pub fn set_busy(&mut self, id: &str, busy: bool)
    pub fn close(&mut self, id: &str)
}
pub struct FieldWrites { settings_port_draft: Option<String>, settings_exclude_draft: Option<String>, voices_search_text: Option<String>, history_search_text: Option<String>, sidebar_search_text: Option<String>, onboarding_key_draft: Option<String>, settings_key_draft: Option<String> }
    // Writes to the main window's in-out text fields (the field keeps its own text while the user types; Rust writes it only through these: ARCHITECTURE.md §7.1 "Text fields").
pub struct UiCtx<'a> { core: &'a CoreHandle, settings: &'a Arc<Settings>, player: &'a PlayerSnapshot, credits: &'a CreditsUpdate, app_info: &'a Arc<AppInfo>, models: &'a Arc<Vec<TtsModel>>, now_ms: f64, demo: bool, reduce_motion: bool, art: &'a mut art::ArtCache, toasts: &'a mut ToastSink, nav: &'a mut NavSink, dialogs: &'a mut DialogSink, fields: &'a mut FieldWrites }
    // The shared state and sinks every controller call receives.
pub trait PageController { type Data, fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent), fn on_leave(&mut self, cx: &mut UiCtx), fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool, fn on_tick(&mut self, cx: &mut UiCtx), fn on_wake(&mut self, cx: &mut UiCtx), fn on_toast_removed(&mut self, cx: &mut UiCtx, id: &str, why: shell::ToastRemoval), fn on_toast_action(&mut self, cx: &mut UiCtx, id: &str) -> bool, fn on_dialog(&mut self, cx: &mut UiCtx, id: &str, confirmed: bool) -> bool, fn build(&self, cx: &mut UiCtx) -> Self::Data }
    // The page controller contract (ARCHITECTURE.md §7.3).
pub struct AppUi { main: MainWindow, widget: WidgetWindow, chip: ChipWindow, core: CoreHandle, player: vox_audio::Player, selection: vox_win::selection::SelectionService, init: UiInit, settings: Arc<Settings>, snapshot: PlayerSnapshot, credits: CreditsUpdate, app_info: Arc<AppInfo>, models: Arc<Vec<TtsModel>>, dark: bool, reduce_motion: bool, art: art::ArtCache, overlays_ready: bool, shell: shell::ShellController, home: home::HomeController, history: history::HistoryController, analytics: analytics::AnalyticsController, voices: voices::VoicesController, settings_page: settings::SettingsController, onboarding: onboarding::OnboardingController, widget_ctl: widget::WidgetController, chip_ctl: chip::ChipController, recorder: hotkey_recorder::Recorder, widget_window: crate::windowing::widget_window::WidgetWindowState, tray: Option<crate::tray::Tray> }
    // Everything on the UI thread (kept in a thread-local; `UiSink` drains into [`AppUi::apply`]).
impl AppUi {
    pub fn create(init: UiInit, core: CoreHandle, player: vox_audio::Player, selection: vox_win::selection::SelectionService, sink: &crate::update::UiSink) -> anyhow::Result<()>
        // Creates the three windows (hidden), applies `init` (Brand, `Theme.dark` on the main window and `true` on the widget and chip, `Motion.reduce` on all three, the onboarding gate, every controller seeded from `init.settings`), wires every cal…
    pub fn apply(update: UiUpdate)
        // Applies one update (called on the UI thread by the `UiSink` drain; module docs "Controller interface").
}
```

#### `vox_app::ui::onboarding`

```rust
pub struct OnboardingController
impl OnboardingController {
    pub fn go_to(&mut self, cx: &mut UiCtx, step: i32)
    pub fn next(&mut self, cx: &mut UiCtx)
    pub fn back(&mut self, cx: &mut UiCtx)
    pub fn skip(&mut self, cx: &mut UiCtx)
    pub fn connect_key(&mut self, cx: &mut UiCtx, draft: &str)
    pub fn key_edited(&mut self, cx: &mut UiCtx)
    pub fn get_key(&mut self, cx: &mut UiCtx)
    pub fn replace_key(&mut self, cx: &mut UiCtx)
    pub fn cancel_replace(&mut self, cx: &mut UiCtx)
    pub fn choose_voice(&mut self, cx: &mut UiCtx, voice_id: &str)
    pub fn preview_voice(&mut self, cx: &mut UiCtx, voice_id: &str)
    pub fn retry_voices(&mut self, cx: &mut UiCtx)
    pub fn read_sample(&mut self, cx: &mut UiCtx)
}
impl PageController for OnboardingController {
}
```

#### `vox_app::ui::settings`

```rust
pub struct Overlay
    // The optimistic settings overlay.
impl Overlay {
    pub fn push(&mut self, req: RequestId, patch: serde_json::Value)
        // Records a patch sent with `req` (the view applies it at once).
    pub fn view(&self, stored: &vox_core::settings::Settings) -> vox_core::settings::Settings
        // The settings to show: `stored` with every pending patch merged in order.
    pub fn on_saved(&mut self, req: RequestId, result: &Result<(), String>) -> Option<String>
        // `SettingsSaved{req, result}`: drops that patch; `Some(message)` = the error toast to show.
    pub fn saving(&self) -> bool
        // "Saving" badge.
}
pub enum RetentionStep { Nothing, Count { days: u32 }, Save { days: u32 }, Confirm { days: u32, count: usize }, Revert { error: Option<String> } }
    // What the retention guard asks the controller to do.
pub struct RetentionGuard
    // The retention guard (ui-settings-widget.md §2.8), pure with injected milliseconds.
impl RetentionGuard {
    pub fn on_choice(&mut self, days: u32, stored: u32, now_ms: f64) -> RetentionStep
        // The user picked `days` at `now_ms` (stored value `stored`).
    pub fn on_settle(&mut self, now_ms: f64) -> RetentionStep
        // The settle timer fired.
    pub fn on_count(&mut self, result: &Result<usize, String>) -> RetentionStep
        // `ExpiringCount` arrived.
    pub fn on_dialog(&mut self, confirmed: bool) -> RetentionStep
        // The dialog was confirmed / cancelled.
}
pub enum PortCommit { Nothing, Save(u16), Invalid }
    // What a port-field event produces.
pub struct PortField { draft: String, error: bool }
    // The port field (ui-settings-widget.md §2.9), pure.
impl PortField {
    pub fn edited(&mut self, text: &str) -> String
        // Every edit: strip non-digits, cut to 5; returns the draft to write back to `settings-port-draft`.
    pub fn commit(&mut self, text: &str, stored: u16) -> PortCommit
        // Blur / Enter.
    pub fn revert(&mut self, stored: u16) -> String
        // Escape: back to the stored port (returns the draft).
    pub fn on_stored_change(&mut self, stored: u16) -> String
        // The stored port changed (even mid-edit): reset the draft (returns it).
}
pub struct HotkeyRows
    // The hotkey rows' messages (6 s rejection, 1.8 s hint, persistent warnings), pure with injected time.
impl HotkeyRows {
    pub fn warning(&self, settings: &vox_core::settings::Settings, action: vox_core::hotkey::HotkeyAction, now_ms: f64) -> Option<String>
        // The warning to show under `action`'s row at `now_ms` (rejection first, else `hotkey::row_warning`).
    pub fn reject(&mut self, action: vox_core::hotkey::HotkeyAction, accel: &str, now_ms: f64)
        // A reserved chord was recorded: keep the old shortcut, show `reserved_rejection` for 6 s.
}
pub struct SettingsController { overlay: Overlay, retention: RetentionGuard, port: PortField, hotkeys: HotkeyRows }
impl SettingsController {
    pub fn section_selected(&mut self, cx: &mut UiCtx, section: &str)
    pub fn set_bool(&mut self, cx: &mut UiCtx, key: &str, value: bool)
    pub fn set_number(&mut self, cx: &mut UiCtx, key: &str, value: f32)
        // A slider commit (`f32` → `quantize`).
    pub fn set_choice(&mut self, cx: &mut UiCtx, key: &str, value: &str)
    pub fn format_number(&self, key: &str, value: f32) -> String
        // The `settings-format-number` pure callback: a slider's live label.
    pub fn save_key(&mut self, cx: &mut UiCtx, key: &str)
    pub fn key_edited(&mut self, cx: &mut UiCtx)
    pub fn open_key_form(&mut self, cx: &mut UiCtx)
    pub fn cancel_key_form(&mut self, cx: &mut UiCtx)
    pub fn remove_key(&mut self, cx: &mut UiCtx)
    pub fn get_key(&mut self, cx: &mut UiCtx)
    pub fn credits_retry(&mut self, cx: &mut UiCtx)
    pub fn test_voice(&mut self, cx: &mut UiCtx)
    pub fn browse_voices(&mut self, cx: &mut UiCtx)
    pub fn reset_voice_settings(&mut self, cx: &mut UiCtx)
    pub fn add_excluded(&mut self, cx: &mut UiCtx, name: &str)
    pub fn remove_excluded(&mut self, cx: &mut UiCtx, name: &str)
    pub fn excluded_edited(&mut self, cx: &mut UiCtx)
    pub fn clear_cache(&mut self, cx: &mut UiCtx)
    pub fn export_history(&mut self, cx: &mut UiCtx, owner: Option<isize>)
    pub fn clear_history(&mut self, cx: &mut UiCtx)
    pub fn port_edited(&mut self, cx: &mut UiCtx, text: &str)
    pub fn commit_port(&mut self, cx: &mut UiCtx, text: &str)
    pub fn revert_port(&mut self, cx: &mut UiCtx)
    pub fn copy_text(&mut self, cx: &mut UiCtx, text: &str)
    pub fn show_extension_folder(&mut self, cx: &mut UiCtx)
    pub fn open_data_folder(&mut self, cx: &mut UiCtx)
    pub fn open_link(&mut self, cx: &mut UiCtx, which: &str)
}
impl PageController for SettingsController {
}
```

#### `vox_app::ui::shell`

```rust
pub struct RouteHistory
    // Back/forward route history.
impl RouteHistory {
    pub fn navigate(&mut self, route: AppRoute) -> bool
        // Pushes a route (drops forward entries); an identical route with a section re-announces it.
    pub fn back(&mut self) -> Option<&AppRoute>
    pub fn forward(&mut self) -> Option<&AppRoute>
    pub fn current(&self) -> Option<&AppRoute>
}
pub enum ToastRemoval { TimedOut, Dismissed, Action, PushedOut }
    // Why a toast left the stack.
pub struct StackEntry { toast: Toast, local: Option<UiToastAction>, leaving: bool, remaining_ms: u64, paused: bool, key: u64 }
    // One toast in the app stack.
pub struct ToastStackState
    // The app window's toast stack state (pure; tested: a same-id push reports no removal; a 5th push yields `PushedOut` for the oldest; an exit keeps the toast present until `finish_exit`).
impl ToastStackState {
    pub fn push(&mut self, toast: Toast, local: Option<UiToastAction>) -> Vec<(String, ToastRemoval)>
        // Pushes, or replaces the same id in place (timer reset, exit cancelled, moved to newest, new key); returns the removals to report (only `PushedOut`).
    pub fn begin_exit(&mut self, id: &str, why: ToastRemoval) -> bool
        // Starts the 180 ms exit (timer, close button, action).
    pub fn finish_exit(&mut self, id: &str) -> Option<(String, ToastRemoval)>
        // The exit finished: removes the row and returns the removal to report.
    pub fn dismiss_now(&mut self, id: &str) -> Option<(String, ToastRemoval)>
        // Programmatic dismiss: removed at once (reported as `Dismissed`).
    pub fn contains(&self, id: &str) -> bool
        // Present (a leaving toast counts as present).
    pub fn entries(&self) -> &[StackEntry]
}
pub struct DialogSlot { open: Option<super::DialogSpec> }
    // The open dialog (one slot).
pub struct Navigation { left: Option<AppPage>, entered: Option<AppPage>, route: AppRoute, intent: NavIntent }
    // What a navigation changed: the page left (its controller gets `on_leave`) and the page entered (`on_enter` with the route and intent).
pub struct ShellController { routes: RouteHistory, toasts: ToastStackState, dialog: DialogSlot }
    // The shell controller (owned by `AppUi`).
impl ShellController {
    pub fn navigate(&mut self, route: AppRoute, intent: NavIntent, onboarded: bool) -> Navigation
        // Navigates (effective page via [`effective_page`]); closes a dialog owned by the leaving page.
    pub fn back(&mut self, onboarded: bool) -> Option<Navigation>
        // Mouse X1 / Alt+Left equivalents (ignored during onboarding).
    pub fn forward(&mut self, onboarded: bool) -> Option<Navigation>
    pub fn on_main_window_activity(&mut self, focused: bool, visible: bool, now_ms: f64) -> Option<AppPage>
        // Main window focus / visibility changed: the active page to wake (≤ once per 30 s per page since its entry).
    pub fn build_sidebar(&self, cx: &mut UiCtx) -> SidebarData
        // The sidebar struct (nav, mini player, credits line via `convert::credits_data(.., Sidebar, ..)`, hint).
}
pub fn effective_page(route: &AppRoute, onboarded: bool) -> AppPage
    // The page shown for a route given the onboarding state.
```

#### `vox_app::ui::tray_view`

```rust
pub struct TrayView { active: bool, tooltip: String, status_bucket: &'static str, right_click_chip: bool, widget_enabled: bool, hotkeys_enabled: bool, hint_read_clipboard: Option<String>, hint_play_pause: Option<String>, hint_stop: Option<String> }
    // The tray's full desired state.
impl TrayView {
    pub fn build(settings: &Settings, state: &PlayerState) -> TrayView
        // Pure mapping (tested).
    pub fn menu_key(&self) -> String
        // The menu rebuild key.
}
```

#### `vox_app::ui::voices`

```rust
pub struct VoicesController
impl VoicesController {
    pub fn search_edited(&mut self, cx: &mut UiCtx, text: &str)
    pub fn set_gender(&mut self, cx: &mut UiCtx, value: &str)
    pub fn set_accent(&mut self, cx: &mut UiCtx, value: &str)
    pub fn set_category(&mut self, cx: &mut UiCtx, value: &str)
    pub fn clear_filters(&mut self, cx: &mut UiCtx)
    pub fn refresh(&mut self, cx: &mut UiCtx)
    pub fn preview(&mut self, cx: &mut UiCtx, voice_id: &str)
    pub fn use_voice(&mut self, cx: &mut UiCtx, voice_id: &str)
    pub fn add_key(&mut self, cx: &mut UiCtx)
}
impl PageController for VoicesController {
}
```

#### `vox_app::ui::widget`

```rust
pub struct ShownToast { toast: Toast, revealed: bool, session_id: Option<String> }
    // The toast currently in the widget's slot.
pub fn hides_on_dismiss(shown: &ShownToast, snapshot: &PlayerSnapshot) -> bool
    // `hidesOnDismiss`.
pub struct ToastSlot { shown: Option<ShownToast>, deadline_ms: Option<f64> }
    // The widget's toast slot (pure; injected milliseconds; tested: the `revealed` chain, a same-id toast restarts its timer, dismissal hides only per `hides_on_dismiss`).
impl ToastSlot {
    pub fn push(&mut self, toast: Toast, hidden: bool, snapshot: &PlayerSnapshot, now_ms: f64)
        // A toast arrived (`hidden` = the window was hidden).
    pub fn on_timer(&mut self, hovered: bool, snapshot: &PlayerSnapshot, now_ms: f64) -> Option<bool>
        // The timer fired: `Some(hide_window)` when the toast is dismissed now, `None` when it stays (hovered → re-check in 1 s).
}
pub struct AutoHide { deadline_ms: Option<f64> }
    // The auto-hide machine (pure; tested: stopped-early reads are never auto-hidden; holds retry every 1.5 s).
impl AutoHide {
    pub fn on_snapshot(&mut self, snapshot: &PlayerSnapshot, auto_hide: bool, now_ms: f64)
        // A new snapshot: arm / cancel the 4 s timer.
    pub fn on_timer(&mut self, hovered: bool, toast_shown: bool, now_ms: f64) -> bool
        // The timer fired: true = hide now; false = a hold is active (re-armed for 1.5 s).
}
pub struct WidgetController { slot: ToastSlot, auto_hide: AutoHide, hidden: bool, hovered: bool, card_key: i32 }
    // Widget state.
impl WidgetController {
    pub fn build(&self, snapshot: &PlayerSnapshot, settings: &vox_core::settings::Settings, art: &mut super::art::ArtCache) -> WidgetData
}
```

#### `vox_app::update`

```rust
pub enum CreditsView { Loading, NoKey, Ready(AccountInfo), Restricted, Error(String) }
    // The credits state machine's output (ui-main.md §1.5).
pub struct CreditsUpdate { view: CreditsView, refreshing: bool }
    // What `core::credits` publishes: the view plus whether a refresh is in flight (D-UM-20: the Analytics `Refresh balance` button spins while `refreshing`).
pub enum WidgetIntent { Show { reposition: bool }, Hide, Reposition }
    // What the widget window must do (core decides showing; the UI's widget controller decides hiding, except for the core-side stop rule below).
pub enum PreviewUpdate { Loading(String), Playing(String), Stopped, Failed { voice_id: String, message: String } }
    // Sample-preview state for a voice id (one at a time).
pub enum RefreshReason { Locale, Day }
    // Why every page model must be rebuilt.
pub enum UiUpdate { Settings(Arc<Settings>), SettingsSaved { req: RequestId, result: Result<(), String> }, Theme { dark: bool }, Motion(bool), Player(PlayerSnapshot), Preview(PreviewUpdate), Credits(CreditsUpdate), AppInfo(Arc<AppInfo>), Toast(Toast), Navigate(AppRoute), ShowMain(Option<AppRoute>), HideMain, Widget(WidgetIntent), ChipShow { x: i32, y: i32, mode: ChipMode }, ChipHide, DisplaysChanged, HotkeysResumed, HistoryChanged, HistoryPage { req: RequestId, result: Result<(HistoryPage, usize), String> }, Home { req: RequestId, result: Result<(Arc<AnalyticsSummary>, Vec<ReadRecord>), String> }, Analytics { req: RequestId, result: Result<Arc<AnalyticsSummary>, String> }, Voices { refresh: bool, result: Result<Arc<Vec<Voice>>, String> }, Models { result: Result<Arc<Vec<TtsModel>>, String> }, ReadStarted { req: RequestId, result: ReadResult }, ApiKey { req: RequestId, result: ApiKeyResult }, Done { req: RequestId, result: Result<(), String> }, CacheStats { req: RequestId, result: Result<CacheStats, String> }, Exported { req: RequestId, result: Result<Option<PathBuf>, String> }, ExpiringCount { req: RequestId, result: Result<usize, String> }, FavoriteToggled { req: RequestId, result: Result<Option<ReadRecord>, String> }, Refresh(RefreshReason), Quit }
    // Every update.
pub struct UiSink
    // Publishes updates to the Slint event loop.
impl UiSink {
    pub fn new(apply: fn(UiUpdate)) -> Self
        // `apply` is called on the UI thread for every drained update (in order), once attached.
    pub fn attach(&self)
        // Called once on the UI thread at the end of `AppUi::create`: from now on queued updates are drained through `slint::invoke_from_event_loop`.
    pub fn send(&self, update: UiUpdate)
}
```

#### `vox_app::windowing::chip_window`

```rust
pub fn show_at(window: &ChipWindow, mode: ChipMode, cursor_phys: (i32, i32), size_dip: (f64, f64)) -> Option<vox_win::window::PhysRect>
pub fn begin_hide(window: &ChipWindow)
pub fn finish_hide(window: &ChipWindow)
```

#### `vox_app::windowing::dpi`

```rust
pub fn to_dip(r: PhysRect, scale: f64) -> Rect
    // Physical → DIP with `scale`.
pub fn to_phys(r: Rect, scale: f64) -> PhysRect
    // DIP → physical (rounded half up like `Math.round`).
pub fn work_area_dip(m: &MonitorInfo) -> Rect
    // A monitor's work area in its own DIP.
pub fn monitor_dip_rect(m: &MonitorInfo) -> Rect
    // The monitor's bounds in the Electron/Chromium DIP screen space (origin = physical origin / own scale).
pub fn electron_dip_to_phys(p: Point, monitors: &[MonitorInfo]) -> Option<(i32, i32, MonitorInfo)>
    // Saved Electron DIP point → physical point via the first monitor whose DIP rect contains it; `None` if none does.
pub fn phys_to_electron_dip(x: i32, y: i32, monitors: &[MonitorInfo]) -> Option<Point>
    // Physical point → Electron DIP point via the monitor containing it (else the first/primary monitor), rounded with `js_round` (the value written to `widget.customX/customY`).
pub fn nearest_monitor_dip(p: Point, monitors: &[MonitorInfo]) -> Option<MonitorInfo>
    // The monitor nearest a DIP point (fallback when no monitor contains a saved point): minimal distance from the point to each monitor's DIP rect.
```

#### `vox_app::windowing::main_window`

```rust
pub fn initial_placement(saved: Option<&WindowState>, monitors: &[MonitorInfo]) -> (PhysRect, bool)
    // Where the window opens (pure; tested): the saved state if its rect overlaps some monitor's work area by at least 64×64 physical px — rescaled by `monitor.scale / saved.scale` around its top-left when the monitor's scale changed, size clamp…
pub fn on_created(window: &MainWindow, dark: bool)
    // Once the HWND exists (before the first show): caption colours, close handling, the native event dispatcher.
pub fn show(window: &MainWindow)
    // Shows and focuses the window (from the tray, a second instance, `ShowMain`).
pub fn hide(window: &MainWindow)
    // Hides to the tray (after saving the placement).
pub fn current_state(window: &MainWindow) -> Option<WindowState>
    // The current placement to persist (`None` before the HWND exists).
```

#### `vox_app::windowing`

```rust
pub struct AttributesInit { dark: bool, main_placement: (PhysRect, bool) }
    // What the attributes hook needs (captured by `app::run` before the backend is installed).
pub fn window_attributes(attributes: WindowAttributes, init: &AttributesInit) -> WindowAttributes
    // The attributes hook body (module docs): matches by `attributes.title` (`Vox`, `Vox Player`, `Vox Chip`).
pub fn install_backend(init: AttributesInit) -> anyhow::Result<()>
    // Installs the winit backend with the attributes hook (must run before any Slint window is created).
pub fn hwnd_of(window: &slint::Window) -> Option<isize>
    // The window's HWND once the native window exists.
```

#### `vox_app::windowing::widget_window`

```rust
pub struct WidgetWindowState { anchor: Anchor, size: Option<Size>, moved_by_us_ms: f64, visible: bool, hovered: bool }
    // Native-side widget state.
pub fn show(window: &WidgetWindow, state: &mut WidgetWindowState, settings: &vox_core::settings::Settings, reposition: bool)
pub fn hide(window: &WidgetWindow, state: &mut WidgetWindowState)
pub fn resize(window: &WidgetWindow, state: &mut WidgetWindowState, width: f64, height: f64)
pub fn start_drag(window: &WidgetWindow, state: &mut WidgetWindowState)
```
