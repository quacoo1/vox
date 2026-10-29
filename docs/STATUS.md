# Vox (Rust + Slint) — status

The Electron app in `C:\Users\Kwaku\Desktop\Desktop Widget` has been rebuilt here as a native Rust app with a Slint
UI, redesigned after `docs/reference-redesign.png`. Decisions: `docs/BRIEF.md`. Visual language: `docs/DESIGN.md`.
Architecture, contracts and the decisions log: `docs/ARCHITECTURE.md`. The behaviour it reproduces: `docs/port/*.md`.

## Build and run

```powershell
cargo build -p vox-app                 # target\debug\vox.exe (+ browser-extension\ next to it)
cargo build -p vox-app --release       # target\release\vox.exe, the portable build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

| Mode | How | Profile |
|---|---|---|
| Normal | `vox.exe` (`--hidden` starts in the tray) | `%APPDATA%\Vox` — the same folder, settings, history and audio cache as the Electron build |
| Demo | `$env:VOX_DEMO = '1'; vox.exe` — seeded history, mock speech, no API key needed | `%APPDATA%\Vox-Demo` |
| Mock speech only | `$env:VOX_MOCK_TTS = '1'` | normal profile |
| Screenshots | `powershell -ExecutionPolicy Bypass -File scripts\shots.ps1` → every page in both themes, widget and chip states, `shots\app\report.json` | demo |
| Static UI previews | `target\release\vox-shot.exe ui\previews\<file>.slint --data ui\previews\<file>.json --out x.png` (copy in `tools\bin`) | — |

Command line, as before: `--read-text "…"`, `--read-clipboard`, `--read-file <path>`, `--hidden`. A second launch hands
its arguments to the running copy.

The Rust app refuses to start while the old Electron Vox is running on the same profile (it shows a message box), so
quit the Electron app before trying the Rust build on your real profile. Demo mode uses its own profile and is not
affected. On first run with your real profile the API key is migrated from the Electron `secrets.bin` into
`secrets-dpapi.bin`; `secrets.bin` is left untouched, so the Electron build keeps working if you go back.

## What is done

| Area | State |
|---|---|
| Libraries (`vox-core`, `vox-text`, `vox-store`, `vox-tts`, `vox-audio`, `vox-win`, `vox-bridge`, `vox-art`) | Implemented from the specs, each reviewed independently for spec conformance and bugs (plus real-time audio and Win32 lenses) and fixed |
| UI (`ui/`) | Component library, every page (Home, History, Analytics, Voices, Settings with all sections, onboarding), the floating player (full and compact) and the Read with Vox chip, in Night and Paper themes, each critiqued and polished against the reference |
| App (`crates/vox-app`) | Core thread, page controllers, native windows, tray, screenshot harness; audited against the architecture and the running app, 46 findings fixed or answered |
| Browser extension | `browser-extension\` copied unchanged; the Rust bridge is wire-compatible |

## Verified here

| Check | Result |
|---|---|
| Clean `cargo build --workspace` | builds |
| `cargo test --workspace` | 1,112 passed, 0 failed; 31 ignored (desktop or live-API tests that need a person, a real key or the live clipboard) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| Screenshot harness (demo, mock speech) | 18 shots, 0 failures, 0 console errors |
| Second instance `--read-text` | forwarded in 128 ms; the read started, played to the end on the mock voice, saved as `source: cli`, completed |
| Browser bridge | `GET /health` → `{"ok":true,"app":"Vox","version":"0.1.0","playing":false}`; `POST /read` with the extension's header → read started, saved as `source: browser-extension` with the page URL; a web-page `Origin` and a missing client header → 403 |
| Startup in demo mode | global hotkeys registered, selection hook installed, bridge listening on `127.0.0.1:47823`, audio output opened |
| Outside close request (`taskkill` without `/F`) | quits cleanly (shutting down → core stopped → Vox stopped) |

## Needs a person at the machine

These cannot be checked headlessly. ARCHITECTURE.md §9 has the full manual checklist.

1. A real read with your ElevenLabs key (quit the Electron app first): speech, word highlighting, speed 0.75–2× with
   natural pitch, credits on Home and Analytics, the key migration from the Electron profile.
2. The right-click chip over real apps (Notepad, Chrome, Word): appears beside the context menu, never steals focus,
   the clipboard is restored afterwards (text, images, files) without clipboard-history entries.
3. Selection popup, global hotkeys (including AltGr keyboard layouts), media keys and the Windows media flyout.
4. The floating player on your desktop: no black corners, dragging, remembered position, auto-hide, compact pill,
   multiple monitors with different scaling.
5. Tray menu, close to tray, launch at login, the Explorer "Read with Vox" entry, the Chrome/Edge extension end to end.
6. An app running as administrator: Vox should say it cannot read from it.

## Known gaps and compromises

- The floating player and chip are clipped to a rounded window region (the GPU surface is opaque on Windows), so their
  rounded corners are not anti-aliased. A smooth edge needs per-pixel alpha in Slint's Windows renderer.
- On mixed-DPI setups the widget can briefly show a stale frame when it appears on a monitor whose scale differs from
  the off-screen parking spot.
- No installer yet (out of scope for this milestone): the portable layout is `vox.exe` + `browser-extension\`.
- `docs/ARCHITECTURE.md` lags the code in a few places (list stabilisation in `ui/stable.rs`, window regions and the
  first-frame parking, the per-session instance pipe name).

## Disk

The C: drive was nearly full during this work. `Cargo.toml` keeps debug info small and turns off incremental builds
for `vox-app` (its incremental cache grew to several GB). If space runs low, deleting `target\debug\incremental`
and `target\debug\deps\*.rcgu.o` is safe while nothing is building.
