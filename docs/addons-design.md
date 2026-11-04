# Vox local voice add-ons: final groundwork design (issue #1, §1)

## Summary

This phase builds the shared add-on pipeline and tests it end to end with a fake helper. It does not build the KittenTTS helpers. When nothing is installed, Vox behaves as it does today, apart from two fixes that are also useful on their own: the read gate now checks the voice actually being read, and an add-on error never shows an ElevenLabs title or an ElevenLabs settings button.

Shape:

- **Identity.** One new unit variant, `Provider::Addon`. Voice ids look like `addon:<addon-id>:<voice-key>`, so `addon:kittentts-lite:bella` and `addon:kittentts-2:bella` stay distinct. A new `Provider::ROUTED` list (EXTRA plus Addon) drives routing. `EXTRA` stays the credential-card list, so add-ons never get a card in Settings → Voice providers.
- **One new crate, `crates/vox-addons`.** It contains:
  - the `AddonHost` SpeechProvider, which picks the add-on from the voice id;
  - the helper client (one child process per add-on, JSON lines over stdio, timeouts that heartbeats keep alive, cancel, crash budget);
  - the install pipeline (signed catalogue, streamed SHA-256 download that can resume, safe tar or tar.gz extraction, version directories with one commit point);
  - the fake helper binary `vox-addon-fake` and its integration tests.
- **Process containment in `vox-win`.**
  - Windows: CREATE_NO_WINDOW plus a job object per helper with KILL_ON_JOB_CLOSE and DIE_ON_UNHANDLED_EXCEPTION.
  - Linux: a process group plus PR_SET_PDEATHSIG, set from one long-lived spawner thread.
  - On both, the helper also exits when its stdin closes, as a third backstop.
- **vox-app.** One `Command::Addons(AddonCmd)` variant, one `UiUpdate::Addons(Arc<AddonsView>)` snapshot, a startup scan, a shutdown step, an idle reaper and add-on notices. The Voices page gets an "Offline voices" section that shows in every page state.
- **Dependencies.** No new crates enter Cargo.lock: tar 0.4.46, flate2 1.1.10 and ring 0.17.14 are already there. One windows-crate feature is added (`Win32_System_JobObjects`). CI needs no change.

Both KittenTTS lines fit by data alone:

- **Lite:** a tar.gz of 25-80 MB, 8 voices listed statically in the manifest, short timeouts, espeak-ng kept inside its own helper archive.
- **KittenTTS 2:** a plain .tar of about 1 GB, 115 voices listed statically, model loads lazily, progress heartbeats, an absolute limit of up to 30 min, the "Powered by KittenML" credit and NOTICE files.

---

## Decisions

| # | Decision | Alternative rejected | Why |
|---|---|---|---|
| D1 | One unit variant `Provider::Addon`. The add-on id goes inside the voice id. | One variant per add-on; `Provider::Addon(String)`; a string-keyed registry | Provider stays `Copy`/`Hash`/`Ord`, and the engine map `HashMap<Provider, Arc<dyn SpeechProvider>>` (engine.rs:225) keeps one slot. Per-add-on variants need a Vox release for every add-on. A payload variant or a registry spreads changes across about 25 call sites (identity map, option b'/c). |
| D2 | New `Provider::ROUTED = [Windows, Polly, Azure, OpenAi, Google, Addon]` for `provider_of`, `from_id` and `engine.providers()`. `EXTRA` is unchanged. | Put Addon in EXTRA and filter it out of `providers_data` (ui/settings.rs:899) | EXTRA also drives the credential cards and `provider_secrets::load_all` (vox-store/src/provider_secrets.rs:42-50). Leaving Addon out of EXTRA keeps it off every credential surface with no filter. `provider_rank` already ranks a provider outside EXTRA last (`unwrap_or(EXTRA.len())`, vox-core/src/voices.rs:155). |
| D3 | The `addon:` prefix is compiled in. AddonHost is registered whenever an add-ons root exists, even with nothing installed. | Register only when something is installed | `provider_of` falls back to ElevenLabs for unknown prefixes (vox-core/src/providers.rs:160). An always-present host answers "KittenTTS 2 isn't installed" by name. NoKey behaviour does not change either way: Windows voices are always registered, Linux included (app.rs:194), so `has_any_provider` (ui/voices.rs:576) is already always true. |
| D4 | Every add-on failure uses `ReadErrorCode::Unknown` with Vox-written copy. | `NoApiKey` (for not installed) or `TtsError::timeout()` (Network) | `NoApiKey` gives the card title "Add your ElevenLabs API key" (vox-core/src/format.rs:668) and adds a Settings→account action through `error_needs_settings` (vox-core/src/read.rs:125-131, reader.rs:396-399). `Network` gives "Can't reach ElevenLabs" (format.rs:673). `Unknown` gives "Something went wrong" with our message as the detail, and needs no change to the persisted error-code set (`ReadErrorCode::ALL`, read.rs:27). |
| D5 | The read gate checks the **effective** voice (`req.voice`, else `settings.voice_id`). | Keep gating `settings.voice_id` only | Today an override voice is not gated (reader.rs:155 vs :196-197). History replays of a removed add-on make that gap common. |
| D6 | New crate `vox-addons`, which depends on vox-core, vox-text, vox-tts, vox-store and vox-win. | Put it in vox-tts (helper) plus vox-store (install) | vox-tts depends on neither vox-win nor vox-store (crates/vox-tts/Cargo.toml). Containment needs vox-win, and tar, flate2, process and IO code does not belong in the speech crate. The cost is exporting one agent builder from vox-tts (see Vox integration). |
| D7 | Wire framing: one UTF-8 JSON object per line, with WAV audio base64-encoded inside the JSON. | A JSON header plus a raw binary payload | C stdio and Python stdout default to text mode on Windows, and text mode silently turns `\n` into `\r\n` inside a raw WAV. With base64, a stray `\r` only lands at line ends, and Vox strips it. The cost is 33 % more bytes, about 4 MB for a 900-unit chunk, which is negligible over a pipe. `base64` is already a vox-tts dependency. |
| D8 | Containment in vox-win: a job object per helper on Windows, PDEATHSIG from a dedicated spawner thread on Linux. Stdin EOF is a third backstop. | Stdin EOF alone | A KittenTTS 2 helper busy in llama.cpp does not read stdin and would hold about 1 GB after Vox exits, including the `process::exit` paths (lifecycle.rs:95, ui/mod.rs:1418). DIE_ON_UNHANDLED_EXCEPTION also suppresses WER "stopped working" dialogs. |
| D9 | Version directories are named `<ver>-<rand8>`, and `current.json` is the only commit point. | `<id>/<version>` | Reinstalling or repairing the *current* version would collide with a directory that a running helper holds open on Windows. That reinstall is the very recovery the crash message tells users to do. |
| D10 | The smoke test sends `quit`, **waits for the helper to exit** (kill after 2 s), then renames the staging dir with busy retries. | Rename right after `quit` | Windows cannot rename a directory whose exe is still running. Without the wait, installs fail intermittently. |
| D11 | Voices are listed statically in `addon.json`, or in `voices.json`, which Vox writes at install time from a probe. `list_voices` and `summary` never start a helper in the normal path. | Ask the helper every time | Listing 115 voices must not load 1 GB of weights. `summary()` runs on the core thread (core/mod.rs:509-513). |
| D12 | A pending request's deadline is an **idle** deadline that any frame resets (progress heartbeat), plus an absolute `synth_max`. | One fixed synth timeout | KittenTTS 2 on CPU is legitimately slow. A real hang is caught in about 60 s, not 30 min. |
| D13 | Downloads use one streamed GET. On failure, `Range` resume continues from a kept `.part`. | Fixed 4 MiB Range segments | A segmented 1 GB download re-resolves GitHub's signed redirect about 250 times. Resume covers the real need, a dropped connection. Stall detection stays a known limit (see Security / risks). |
| D14 | Ed25519-signed catalogue with compiled-in rotatable keys and an anti-rollback `sequence`. While no key is configured, the feature is inert. | Unsigned HTTPS plus SHA-256 | Helpers are executables that run as the user. Without a signature, anyone with write access to the release can ship code. ring is already in Cargo.lock. |
| D15 | The catalogue lives at a **fixed, never-latest** release tag. | `releases/latest/download/...` | `latest` moves to whichever release is published last, and the catalogue goes 404 the first time an add-on archive release is published. |
| D16 | Install, Update and Cancel are fire-and-forget, with state carried in one snapshot. Remove is request-shaped (`Done{req}`, with an `owed_answer` arm). | Every command request-shaped, or every command fire-and-forget | Long installs are not request-shaped. The Remove confirm dialog needs a busy state that always resolves (D-ARCH-3, command.rs:7-10). |
| D17 | No lock files. | `.lock` / `.inuse` | The single-instance key guards each root (runtime.rs:58-65). Demo mode never touches the add-ons root. |
| D18 | No host-side sub-chunking. The helper must accept 900 UTF-16 units and split internally. | Splitting, PCM concatenation and cue shifting in Vox | That would add a large bug surface for no groundwork value, and the helper knows its model's limits. |
| D19 | No speed, language or option argument in v1. Speed stays with WSOLA (`playback_rate`). The protocol reserves an `options` object. | Extend `SpeechProvider` now | The trait has no option slot (vox-tts/src/providers.rs:24). The UI already sends non-ElevenLabs users to Playback for speed (ui/settings.rs:108-120). An additive default trait method can come later. |
| D20 | `UiUpdate::Addons` stays in VoicesController (added to `reaches_every_page`, ui/mod.rs:1481). | Shared state in UiCtx | It avoids changing the `cx!` macro (ui/mod.rs:436-455) and the test World (analytics.rs:1269-1285). It can move later if onboarding needs it. |

---

## Identity and routing

### Voice id format

```
voice_id  := "addon:" addon_id ":" voice_key
addon_id  := [a-z0-9] ( [a-z0-9-]{0,30} [a-z0-9] )?       1-32 chars, no "--"
             not a DOS reserved name (con prn aux nul com1-9 lpt1-9)
voice_key := [A-Za-z0-9] [A-Za-z0-9._-]{0,63}            1-64 chars, no ':', no whitespace, no trailing '.'
```

- Examples: `addon:kittentts-lite:bella`, `addon:kittentts-2:bella`, `addon:kokoro:af_heart`, and in tests `addon:fake-a:alpha`.
- The maximum length is 6 + 32 + 1 + 64 = 103 UTF-16 units. That is under the 200-unit `settings.voiceId` truncation (vox-core/src/settings.rs:469-481), so an id is never corrupted.
- **Stability rules (normative, in docs/addons-protocol.md):**
  - The id never contains a version, platform or model file name.
  - An add-on id is permanent once published.
  - Voice keys are a public contract of the add-on: updates may add keys and must not rename them. An optional manifest map `voice_aliases: {"old":"new"}` covers a rename. That keeps settings.voiceId, history, analytics grouping and voice art (all keyed on the exact string) stable across updates.

### vox-core/src/providers.rs changes

- `enum Provider` gains `Addon`.
- `display_name` → "Offline voices", `id` → "addon", `prefix` → "addon:" (private fn at :131).
- `needs_credentials` → false (`!matches!(self, Windows | Addon)`). `connect_provider` and `disconnect_provider` therefore return early (catalog.rs:304-366).
- `spec()` gets an arm with an empty form and `exact_timing: false`. It is never rendered.
- `pub const ROUTED: [Provider; 6] = [Windows, Polly, Azure, OpenAi, Google, Addon];`
- `provider_of` (:160) and `from_id` (:56) iterate `ROUTED`. `EXTRA` (:30) is unchanged.
- Module doc table: `| Offline voice add-on | addon:kittentts-lite:bella | <addon-id>:<voice-key> |`.
- Exhaustive matches that need a new arm: `display_name`, `id`, `spec`, `prefix`, plus `vox_tts::providers::connect` (vox-tts/src/providers.rs:45), where the arm is `Provider::Addon => None` because vox-app builds the host.

### New in vox-core/src/addons.rs

Modelled on `parse_polly_rest` (providers.rs:193):

```rust
pub struct AddonVoiceRef<'a> { pub addon: &'a str, pub voice: &'a str }
pub fn is_valid_addon_id(s: &str) -> bool;
pub fn is_valid_voice_key(s: &str) -> bool;
pub fn addon_voice_id(addon: &str, voice: &str) -> Option<String>;   // None if either part is invalid
pub fn parse_addon_rest(rest: &str) -> Option<AddonVoiceRef<'_>>;   // split_once(':') + validate
pub fn parse_addon_voice_id(id: &str) -> Option<AddonVoiceRef<'_>>; // via split_voice_id
pub fn addon_voice_name(voice: &str, short: &str) -> String;
// "Bella" + "KittenTTS 2" -> "Bella (KittenTTS 2)"; "Anna (de-DE)" -> "Anna (de-DE, KittenTTS 2)"
```

### How the engine finds an add-on's client

1. `synthesize_chunk` (engine.rs:391) calls `provider_of(id)`, which gives `Addon`. The engine looks up the map slot that holds the single `Arc<AddonHost>` and passes `rest = "<addon-id>:<voice-key>"`. **No engine change is needed**, except that `providers()` (engine.rs:711) iterates `ROUTED`.
2. `AddonHost::synthesize` runs `parse_addon_rest`, then resolves `voice_aliases`, then looks up `runtimes[addon_id]` (an `Arc<AddonRuntime>`), then calls `runtime.helper.synthesize(...)`.
3. Missing add-on id: `TtsError::new(Unknown, "KittenTTS 2 isn't installed. Install it on the Voices page.")`, not retryable. The name comes from installed manifests or the cached catalogue, and falls back to the raw id.

### Display

- AddonHost builds each `Voice` by hand, as polly.rs:283-290 does:
  - `voice_id = addon_voice_id(..)`;
  - `name = addon_voice_name(v.name, manifest.short_name)`;
  - `category = Some(manifest.name)`, so the type filter shows "KittenTTS Lite" and "KittenTTS 2" as separate facets and the row subtitle fallback names the add-on;
  - labels gender, accent, language and use_case come from the manifest;
  - `description` comes from the voice entry, else the add-on description.
- `catalog.rs:287` strips everything from the first " (", so the preview sentence still says "Hi, I'm Bella".
- Sort (vox-core/src/voices.rs `sort_voices_for_page`, about :159-171): add `.then_with(|| addon_group(a).cmp(&addon_group(b)))` after `provider_rank`. `addon_group` returns the lower-cased category for `Provider::Addon` and "" otherwise. Add-ons sort last as a block, each add-on is grouped, and other providers are unchanged. The order test at voices.rs:367-377 gains two add-on lines that share voice names.
- `queries.rs:88-94 provider_statuses` filters out `Provider::Addon`. Add-on state reaches the UI through `UiUpdate::Addons`, and AppInfo keeps meaning "connected services".
- `ui/settings.rs:108 other_provider_notice` gets an Addon arm. Title "Offline voice". Body "This offline voice speaks with its own settings. Set the reading speed in Playback."

### Compatibility

- `Provider` has no serde, so only id strings reach disk.
- Electron reads `addon:` ids as ElevenLabs ids, exactly as it already does for `windows:` and `polly:`.
- Audio cache manifests carry no voice id (vox-store/src/audio_cache.rs:59-107), so the cache is unchanged. WAV chunk files are already supported (audio_cache.rs:212-216).
- Add-on install state never goes into settings.json; it lives only under the add-ons root.
- History records an add-on voice name with its suffix, so analytics Top voices and history search keep the two KittenTTS lines apart.

---

## Catalogue and manifest

### Location and trust (vox-core/src/constants.rs)

```rust
pub const ADDON_CATALOGUE_URL: &str =
    "https://github.com/quacoo1/vox/releases/download/addons-index/catalogue.json";
pub const ADDON_CATALOGUE_SIG_URL: &str =
    "https://github.com/quacoo1/vox/releases/download/addons-index/catalogue.json.sig";
pub const ADDON_CATALOGUE_KEYS: &[[u8; 32]] = &[]; // Ed25519 public keys; empty = feature inert (see Open questions)
pub const ADDON_DOWNLOAD_HOSTS: &[&str] =
    &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
pub const ADDON_CATALOGUE_MAX_BYTES: u64 = 1 << 20;
pub const ADDON_CATALOGUE_TTL_MS: u64 = 6 * 3600 * 1000;
pub const ADDON_PROTOCOL_MIN: u32 = 1;
pub const ADDON_PROTOCOL_MAX: u32 = 1;
```

- `addons-index` is a dedicated tag. Its assets are replaced on each publish, and it is never marked "latest".
- `catalogue.json.sig` holds the standard base64 (with padding) of a 64-byte Ed25519 signature over the **exact bytes** of catalogue.json. Verification: `ring::signature::UnparsedPublicKey::new(&ED25519, key).verify(body, sig)` against each key in the list.
- A missing or bad signature rejects the catalogue as a whole.
- `sequence` lower than the last accepted value is rejected as stale.
- The catalogue source is injected (`CatalogueSource { url, sig_url, keys, policy }`), so tests use a ring key pair from a fixed seed and a loopback policy.
- In release builds there is no environment override of the catalogue URL. `cfg(debug_assertions)` builds accept `VOX_ADDON_CATALOGUE_URL`, for https or for http on 127.0.0.1 only.

### catalogue.json (schema 1)

Unknown fields are ignored everywhere. An invalid entry is dropped with a log line and never fails the whole catalogue.

```json
{
  "schema": 1,
  "sequence": 7,
  "generated": "2026-11-01T12:00:00Z",
  "addons": [
    {
      "id": "kittentts-lite",
      "name": "KittenTTS Lite",
      "short_name": "KittenTTS Lite",
      "publisher": "KittenML",
      "description": "8 natural English voices. Small and fast on any PC.",
      "homepage": "https://github.com/KittenML/KittenTTS",
      "licence": "Apache-2.0 models; helper includes espeak-ng (GPL-3.0), source offer in NOTICE",
      "credit": "",
      "releases": [
        {
          "version": "0.8.1",
          "protocol": 1,
          "min_vox": "0.2.0",
          "voices": 8,
          "languages": ["en"],
          "artifacts": [
            {
              "platform": "windows-x86_64",
              "format": "tar.gz",
              "url": "https://github.com/quacoo1/vox/releases/download/kittentts-lite-0.8.1/kittentts-lite-0.8.1-windows-x86_64.tar.gz",
              "size": 52428800,
              "sha256": "3b4c…(64 lowercase hex)",
              "unpacked_size": 61000000,
              "max_entries": 200
            },
            { "platform": "linux-x86_64", "format": "tar.gz", "url": "https://github.com/…linux-x86_64.tar.gz",
              "size": 50331648, "sha256": "…", "unpacked_size": 59000000, "max_entries": 200 }
          ]
        }
      ]
    },
    {
      "id": "kittentts-2",
      "name": "KittenTTS 2",
      "short_name": "KittenTTS 2",
      "publisher": "KittenML",
      "description": "115 voices in 16+ languages. Large; slower on CPU.",
      "licence": "KittenML Community Licence",
      "credit": "Powered by KittenML",
      "releases": [ { "version": "2.0.0", "protocol": 1, "min_vox": "0.2.0", "voices": 115,
                      "languages": ["en","de","fr","es"],
                      "artifacts": [ { "platform": "windows-x86_64", "format": "tar", "size": 930000000, "…": "…" } ] } ]
    }
  ]
}
```

Field rules (`vox_core::addons::parse_catalogue(&[u8]) -> Result<Catalogue, CatalogueError>`):

| Field | Rule |
|---|---|
| `schema` | Must be 1. A value above 1 gives `Unsupported`, shown as "Update Vox to see more offline voices". |
| `id` | Must pass `is_valid_addon_id` and be unique. |
| `name` | 1-40 chars. |
| `short_name` | 1-24 chars; defaults to `name`. |
| `description` | At most 300 chars. |
| `credit` | At most 80 chars. |
| `releases` | Newest first. |
| `version`, `min_vox` | `vox_core::addons::Version`: dotted numeric with 1-4 parts, an optional `-pre`, and `Ord`. No semver crate. |
| `protocol` | The helper protocol major the release speaks. |
| `platform` | `format!("{}-{}", std::env::consts::OS, ARCH)`: windows-x86_64, windows-aarch64, linux-x86_64 or linux-aarch64. |
| `format` | `"tar"` or `"tar.gz"`. `"tar.zst"` is reserved. |
| `url` | Must be https. |
| `size` | 1..=4 GiB. |
| `sha256` | Exactly 64 hex chars. |
| `unpacked_size` | size/4 ..= 8 GiB. |
| `max_entries` | At most 10 000. |

`pick_release(addon, vox_version, ADDON_PROTOCOL_MIN..=MAX, platform)` returns the first release with a compatible protocol, `min_vox <= CARGO_PKG_VERSION` and an artifact for this platform. If releases exist but none fits, the row shows **Incompatible** ("Needs a newer Vox"). With no artifact for this platform, the add-on is hidden.

### addon.json (at the root of every archive, schema 1)

```json
{
  "schema": 1,
  "id": "kittentts-2",
  "version": "2.0.0",
  "name": "KittenTTS 2",
  "short_name": "KittenTTS 2",
  "protocol": { "min": 1, "max": 1 },
  "exe": "bin/kittentts2-helper.exe",
  "args": ["--models", "models"],
  "env": { "OMP_NUM_THREADS": "4" },
  "timeouts": { "start_ms": 20000, "load_ms": 300000, "idle_ms": 60000, "synth_max_ms": 1800000 },
  "idle_exit_sec": 300,
  "credit": "Powered by KittenML",
  "notices": ["NOTICE.txt", "licenses/KittenML-Community-Licence.txt"],
  "voices": [
    { "key": "bella", "name": "Bella", "gender": "female", "language": "en-US", "accent": "American",
      "description": "Warm and clear." }
  ],
  "voice_aliases": {}
}
```

Validation (`parse_manifest`), at install time and again at every scan:

- **Identity:** id and version equal the catalogue release (skipped for `source: "local"` installs), and protocol overlaps `ADDON_PROTOCOL_MIN..=MAX`.
- **Executable:** `exe` passes the extractor path rules, is a regular file inside the version dir and ends in `.exe` on Windows. Linux sets mode 0755 on this file only.
- **Environment:** `env` keys match `[A-Z][A-Z0-9_]{0,63}`. `PATH`, `LD_*`, `DYLD_*` and `VOX_*` are rejected.
- **Timeouts and idle exit** are clamped:

  | Field | Range | Default |
  |---|---|---|
  | `start_ms` | 5-180 s | 20 s |
  | `load_ms` | 0-600 s | 60 s |
  | `idle_ms` | 5-600 s | 60 s |
  | `synth_max_ms` | 10 s-30 min | 120 s |
  | `idle_exit_sec` | 0 (never) or 30-3600 | 300 |

- **Voices:** `voices` is optional. There are at most 1000 entries, keys must be valid and unique, and names are 1-60 chars.
- **Notices:** each `notices` entry must resolve to a file inside the dir.

### Files Vox writes

- `<version dir>/voices.json` = `{"schema":1,"voices":[…]}`. It is written from the install probe only when the manifest has no `voices`.
- `<root>/<id>/current.json`, written with `vox_store::fsutil::write_file_atomic(path, bytes, true)` (fsutil.rs:142):

```json
{ "schema": 1, "version": "2.0.0", "dir": "2.0.0-k3j9az1q", "sha256": "…", "size": 930000000,
  "installed_at": "2026-11-04T10:00:00Z", "source": "catalogue" }
```

- `source` is `"catalogue"` or `"local"`. A local install is placed by hand by a helper author. It is loaded but never offered updates.

---

## Install, update, remove

### Directories

New functions in vox-store/src/paths.rs, next to the roaming helper (:23-57). They are **not** Profile accessors, because Profile is Roaming (paths.rs:69-75) and GB-sized models must not roam.

- `pub fn local_data_dir() -> Option<PathBuf>`
  - Windows: `SHGetKnownFolderPath(FOLDERID_LocalAppData)`, with `%LOCALAPPDATA%` as fallback. The `Win32_UI_Shell` feature is already in vox-store/Cargo.toml.
  - Linux: `$XDG_DATA_HOME`, else `~/.local/share`.
- `pub fn addons_root() -> Option<PathBuf>`
  - Windows: `%LOCALAPPDATA%\Vox\addons`. This does not collide with the installer's `{localappdata}\Programs\Vox`.
  - Linux: `$XDG_DATA_HOME/vox/addons`, else `~/.local/share/vox/addons`. Lowercase `vox` matches master.key (secrets.rs:283-287).
  - Override: `VOX_ADDONS_DIR`, used by tests, QA and helper authors.
- **Demo mode never touches the root.** It shows a fixture `AddonsView` and registers no AddonHost; mock TTS bypasses providers anyway (engine.rs:393).

Layout:

```
addons/
  catalogue.json  catalogue.json.sig  catalogue-state.json   {"last_sequence":7,"fetched_at":…}
  .staging/   <id>-<ver>-<rand8>.part  .part.json  <id>-<ver>-<rand8>/  trash-<id>-<rand8>/
  <id>/current.json                    the ONLY thing that makes an add-on installed
  <id>/<ver>-<rand8>/                  addon.json, voices.json?, bin/…, models/…, NOTICE*
  <id>/data/                           helper-writable (VOX_ADDON_DATA), kept across updates
```

### Install or update

There is one code path for install, update, resume and repair: `vox_addons::install::run(ctx, job, &mut dyn FnMut(InstallStage)) -> Result<InstalledAddon, AddonError>`. It runs on `spawn_worker("vox-addon-install")` (core/mod.rs:888). Stages reach the core as `Command::Addons(AddonCmd::Event(..))`, the catalog.rs:301-330 pattern; a failed spawn is published on the core thread.

1. **Core.** The core rejects the request if a job for that id is running or the release cannot be picked. It calls `maybe_recheck_proxy(false)` (core/mod.rs:545) and creates `Job { gen, cancel: CancelToken, stage }`.
2. **Preflight.** `vox_win::disk::free_bytes(root)` must be at least `size + unpacked_size + 64 MiB`, minus the bytes of a resumable `.part`. Windows uses `GetDiskFreeSpaceExW` (`Win32_Storage_FileSystem` is already in vox-win); Linux uses `statvfs`. Otherwise the job fails with `DiskFull{needed}`.
3. **Download** (`download.rs`).
   - Agent: `vox_tts::providers::https_agent(ua, proxy, HttpsAgentOptions { https_only: true, max_redirects: 0 })`.
   - Redirects: Vox follows `Location` itself, at most 5 hops. Each hop must be https and its host in `ADDON_DOWNLOAD_HOSTS`, else `UntrustedRedirect`.
   - Deadlines: no global timeout, `timeout_recv_body = size / 32 KiB/s + 120 s`, connect timeout 10 s.
   - Body: read with `body_mut().with_config().limit(size + 1).reader()` in 64 KiB reads and appended to `.staging/<id>-<ver>-<rand>.part`, feeding `sha2::Sha256` as it goes.
   - Limits and cancel: the download aborts the moment the total exceeds `size`, and the CancelToken is checked between reads.
   - Progress: reported at most every 250 ms, throttled at the source.
   - Sidecar: `.part.json {url, sha256, size, done}` is written every 8 MiB.
   - Resume: a later Install with a resumable part re-hashes the existing prefix and sends `Range: bytes=N-`. A 200 instead of 206 restarts from 0, and a 416 deletes the part and restarts.
   - Errors: `StorageFull`, `QuotaExceeded` and raw OS errors 28 or 112 become `DiskFull`. Network errors become `Failed{resumable: true}` with the part kept.
4. **Verify.** `len == size`, then `hex(sha256) == artifact.sha256`. A mismatch deletes the part and fails with "The download was damaged. Try again." This failure is not resumable.
5. **Extract** (`extract.rs`) into a fresh `.staging/<id>-<ver>-<rand>/`, created with `create_dir`.
   - Decoding: `flate2::read::GzDecoder` for tar.gz (miniz_oxide backend), the raw file for tar.
   - `tar::Archive` with `set_preserve_permissions(false)`, `set_unpack_xattrs(false)`, `set_overwrite(false)` and `set_preserve_mtime(false)`.
   - Vox iterates `entries()` and **never calls `unpack`**. Only Regular and Directory entries are accepted, plus the pax and GNU long-name header types. Symlink, Link, Char, Block, Fifo, GNU sparse and anything else give `UnsafeArchive`.
   - Paths are checked on `entry.path()`, after pax and long-name overrides:
     - valid UTF-8; Normal components only (a leading CurDir is skipped; RootDir, Prefix and ParentDir are rejected);
     - each component 1-100 chars with none of `: * ? " < > | \` or control characters, and no trailing `.` or space;
     - not a reserved DOS stem, case-insensitive;
     - depth at most 8 and at most 200 chars in total;
     - no duplicate after lowercasing (NTFS case collision).
   - Caps: entries at most `max_entries`, and running bytes at most `unpacked_size`, which also catches gzip bombs.
   - Each file is written by Vox with `OpenOptions::create_new` and a counting `io::copy`, then `sync_all`.
   - tar's own `validate_inside_dst` is only a second safety net. The `.part` is deleted after a successful extraction.
6. **Validate** with `parse_manifest`. On Linux, set mode 0755 on `manifest.exe`.
7. **Smoke test.**
   - Spawn the helper from the staged dir through the vox-win spawner, send `hello` (within `start_ms`) and then `voices` (if the manifest has `voices`, the helper's keys must be a superset of them).
   - Write `voices.json` with `write_file_atomic` if the manifest has no voice list. Then send `quit`, **wait for exit**, and kill after 2 s.
   - Spawn errors map to clear copy (see Helper protocol). Nothing is committed if this step fails.
8. **Commit.**
   - Create `<root>/<id>/` if needed.
   - `fs::rename(staged, <root>/<id>/<ver>-<rand>)`, wrapped in `fsutil::retry_busy` (fsutil.rs:56), because Defender can hold a just-run exe. This is atomic on the same volume; on Linux the parent dir is fsynced.
   - Then `write_file_atomic(<id>/current.json, …, true)`. **This is the commit point.**
9. **Swap** on the core thread (`Installed{id, gen, Ok(addon)}`). Events with a stale `gen` are ignored.
   - `host.replace(id, AddonRuntime::new(addon))`.
   - `publish_addons()`, then `publish_app_info()` and `fetch_voices(true)`. This mirrors `on_provider_validated` (catalog.rs:332-350).
   - The old runtime goes into `retired: Vec<(Weak<AddonRuntime>, PathBuf)>`.

**Rollback:** any failure before step 8 deletes the staging entries and leaves `current.json`, the old version dir and the running helper untouched. A crash during step 8 is also safe: before `current.json` is written, the new dir is an orphan that the sweep deletes; after it, the new version is complete.

**Cancel** (Downloading only): the core sets the job's CancelToken, bumps `gen`, clears the job and publishes the row as Available at once. The worker notices at its next read, or at the `recv_body` deadline for a stalled socket, deletes its own part and exits; its late events are ignored. A new Install creates a new `<rand>` part name, so it never shares a file with a lingering worker. Resume is offered only for a part whose worker reported a resumable failure.

### Remove

`AddonCmd::Remove{req, id}`, after a confirm dialog:

1. On the core thread, `host.remove(id)` returns the runtime and marks it `removed`. Any in-flight request fails with "KittenTTS 2 was removed." (not retryable).
2. Delete `<id>/current.json`. The add-on is now uninstalled, even if the file deletion below fails.
3. `publish_addons()` with the row in Removing, then `publish_app_info()` and `fetch_voices(true)`.
4. On a worker: `runtime.stop_and_wait(2 s)` (quit, then the job close or killpg), then `fs::rename(<id>, .staging/trash-<id>-<rand>)`, then `remove_dir_all_retry` (fsutil.rs:242). `data/` goes too.
5. `Removed{id, req, result}` arrives. The core answers `UiUpdate::Done{req, result}` and republishes. A failed delete is logged, and the startup sweep finishes it.

If the selected voice belongs to the removed add-on, it stays in settings, and the read gate explains it (see Vox integration).

### Startup

- **Scan.** In app.rs, right after the Windows registration (:194-196): `AddonsRoot::open(root).scan()`.
  - It reads only small JSON files: current.json, addon.json and voices.json.
  - Invalid entries are logged and shown as "Failed: reinstall it". Their current.json is kept, so Repair is possible.
  - Then `engine.set_provider(Provider::Addon, Some(host.clone()))`. **No helper is spawned.**
- **Sweep.** After the core starts, a worker (`vox-addons-sweep`) deletes:
  - everything in `.staging/` except resumable parts younger than 7 days that have a matching `.part.json`;
  - every `<id>/<dir>` not named by current.json (except `data/`);
  - every `<id>/` that has no current.json.

### A read in progress during update or remove

- **Update:** chunks already synthesized keep playing from memory and the cache. Each chunk's `synthesize` looks up the current runtime, so later chunks come from the new version and the read continues. The voice id has no version, so nothing in history changes.
  - The old runtime finishes its in-flight request. When its last `Arc` drops, `Drop` sends quit and closes its job, which kills the process.
  - The `AddonIdle` timer tick sees the `Weak` reference gone and spawns `remove_dir_all_retry(old dir)`.
  - If the new version dropped a voice key without an alias, the next chunk fails with "This voice isn't in KittenTTS Lite 0.9.0." and the read is cut short through `cut_short_locked` (engine.rs:316-354).
- **Remove:** the next chunk, or the one in flight, fails with "KittenTTS 2 was removed." Chunk 0 failing means Failed; a later chunk cuts the read short. The confirm dialog warns about this.

### Uninstalling Vox

- `installer/vox.iss:23-27`:
  - change `taskkill /IM vox.exe` to `taskkill /F /T /IM vox.exe`; `/T` kills helpers too, and job close would anyway;
  - add `[UninstallDelete] Type: filesandordirs; Name: "{localappdata}\Vox\addons"`;
  - add `Type: dirifempty; Name: "{localappdata}\Vox"`.
- `scripts/install-linux.sh --uninstall`: also `rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/vox/addons"` unless `--keep-addons` is given, and print what was removed.

---

## Helper protocol (v1)

The normative spec goes in `docs/addons-protocol.md`; the Rust types live in `vox-addons/src/protocol.rs` (pure, no IO).

### Process model

- **One process per installed add-on**, at most, started lazily on the first `synthesize` (preview or read). It is never started at Vox startup, by `summary()`, or by `list_voices` when voices are known. The one exception is a side-loaded add-on with no voice list: `list_voices` then asks once, on the `vox-voices` worker, and memoises the answer.
- **Spawn** goes through `vox_win::process::Spawner`:
  - It is one long-lived thread, `vox-addons-spawn`, fed by a channel, because PR_SET_PDEATHSIG fires when the *spawning thread* exits. No other thread may spawn helpers.
  - `program` is the canonicalised absolute path of `<version dir>/<exe>`, checked to be inside the dir. PATH is never searched.
  - `current_dir` is the version dir, and `args` come from the manifest.
  - The inherited environment loses every `VOX_*` variable and gains `VOX_ADDON_PROTOCOL=1`, `VOX_ADDON_ID`, `VOX_ADDON_DIR` and `VOX_ADDON_DATA` (`<root>/<id>/data`, created by Vox), plus the manifest `env`.
  - stdin, stdout and stderr are all piped.
- **Windows containment:**
  - `creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)`, because Vox is a GUI-subsystem process (main.rs:16).
  - Immediately after spawn, `AssignProcessToJobObject` puts the child in its own job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`.
  - `HelperGuard` owns the job handle. Dropping it or calling `kill_tree()` (TerminateJobObject) kills the whole tree, and so does any Vox exit, `process::exit` included.
- **Linux containment:**
  - `process_group(0)`.
  - `pre_exec`: `prctl(PR_SET_PDEATHSIG, SIGKILL)`, then `_exit(1)` if `getppid()` is not the pid captured before the fork. Only async-signal-safe calls are made there.
  - `kill_tree()` is `killpg(pgid, SIGKILL)` followed by a reap. libc is already a vox-win dependency.
- **Helper rules (normative):**
  1. Write only protocol lines to stdout; logs go to stderr.
  2. Write nothing before `hello` arrives.
  3. Exit within 1 s of `quit` or stdin EOF.
  4. Create no child processes before replying to `hello`. This closes the window before job assignment.
  5. Handle requests one at a time, in order (concurrency 1).
  6. Answer `hello` and `voices` in under 2 s without loading weights; load the model on the first `synthesize`.
  7. Accept any `synthesize` text up to 4096 UTF-8 bytes and split internally if the model needs it. Vox sends at most 900 UTF-16 units (vox-core constants CHUNK_MAX).
  8. On Windows, call `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX)`. The job already covers WER.
- **Vox threads per helper:**
  - `vox-addon-<id>-out` reads stdout lines (`take(cap).read_until(b'\n')`) into a crossbeam channel.
  - `vox-addon-<id>-in` writes queued requests, so a helper that has stopped reading never blocks a Vox thread.
  - `vox-addon-<id>-err` drains stderr (see Stderr logging).

### Framing

- One compact UTF-8 JSON object per line, ending in `\n`. A trailing `\r` is stripped.
- Vox → helper lines are at most 64 KiB. Helper → Vox lines are at most 96 MiB (`ADDON_MAX_LINE`).
- Every message has `"type"`. Requests carry `"id": u64`, increasing per process, and replies echo it.
- Both sides ignore unknown fields.
- Vox discards replies to non-pending ids, such as stale replies after a cancel or timeout. Unknown reply types for the pending id are logged and ignored, but they still count as liveness.
- Unknown request types get the error `unsupported`.
- These are protocol violations: non-JSON or a non-object line on stdout, a line over the cap, a missing `type`, or EOF mid-line. Vox kills the helper, the event counts toward the crash budget, and the request fails.

### Messages

Vox → helper:

```json
{"id":1,"type":"hello","protocol":{"min":1,"max":1},"vox":"0.2.0","os":"windows","arch":"x86_64"}
{"id":2,"type":"voices"}
{"id":3,"type":"synthesize","voice":"bella","text":"Hello there, wörld 👋.","word_timing":true,"options":{}}
{"id":4,"type":"cancel","target":3}
{"id":5,"type":"quit"}
```

`options` is reserved for later fields such as speed and language. v1 Vox always sends `{}`. `cancel` and `quit` get no reply.

Helper → Vox:

```json
{"id":1,"type":"hello","protocol":1,"name":"kittentts2-helper","version":"2.0.0","offsets":"utf8","word_timing":"exact"}
{"id":2,"type":"voices","voices":[{"key":"bella","name":"Bella","gender":"female","language":"en-US","accent":"American","description":"Warm and clear."}]}
{"id":3,"type":"progress","stage":"loading","fraction":0.4}
{"id":3,"type":"audio","format":"wav","wav":"UklGRiQAAABXQVZF…","offsets":"utf8","words":[{"from":0,"to":5,"start":0.0},{"from":6,"to":12,"start":0.41},{"from":13,"to":20,"start":0.83}]}
{"id":3,"type":"error","code":"unknown_voice","message":"no voice 'x'"}
```

- **`progress`** is optional and repeatable. It resets the pending request's idle deadline. KittenTTS 2 should send one at least every `idle_ms / 2` during model load and long synthesis.
- **`wav`** is base64 (standard alphabet) of ONE complete RIFF/WAVE file with exact header sizes.
  - Encoding: PCM s16le or IEEE f32le, `WAVE_FORMAT_EXTENSIBLE` allowed. Symphonia is built with wav and pcm and decodes these (Cargo.toml:79).
  - 1-2 channels, 8 000-192 000 Hz (KittenTTS: 24 000), non-empty data.
  - The feeder resamples per chunk (vox-audio feeder.rs:151-158).
- **`words`** is optional. It is omitted when `word_timing` was false or the helper has no timing.
  - `from`/`to` are half-open offsets into `text` exactly as sent, in the unit `offsets`: `"utf8"` (byte offsets, the default), `"codepoint"` or `"utf16"`. The `offsets` value on `audio` overrides the one on `hello`.
  - `start` is in seconds from the start of the audio, finite, >= 0 and ascending.
- **Error codes:** `bad_request`, `unknown_voice`, `text_too_long`, `cancelled`, `model_load_failed`, `out_of_memory`, `unsupported`, `unsupported_protocol`, `internal`. `message` is logged only and never shown, because it is an untrusted string.

### Version handshake

- Vox offers `{min:1, max:1}`. The helper replies with one `protocol` in that range, or with `{"type":"error","code":"unsupported_protocol","supported":{"min":2,"max":2}}` and exits. The add-on is then marked Incompatible: "KittenTTS 2 needs an update."
- The catalogue's `release.protocol` and the manifest's `protocol` range let Vox hide incompatible builds before downloading, and mark an installed one Incompatible without spawning it.
- Changes within v1 are additive only. A breaking change bumps the major, and Vox supports both majors for at least one release.

### Result building (AddonHost::synthesize)

1. Base64-decode `wav`, then validate with `vox_tts::wav::read_wav_info` (wav.rs:61):
   - PCM16 or float32, 1-2 channels, 8-192 kHz;
   - `data_bytes > 0` and the data inside the file; the streaming size 0xFFFFFFFF is rejected;
   - duration at most 600 s and at most 0.5 s × len16(text) + 10 s.
   Otherwise: "KittenTTS Lite sent audio Vox couldn't use." (not retryable; the helper is kept, since the protocol was fine).
2. The duration comes from `WavInfo.duration_sec`. The helper's own numbers are never trusted.
3. Words are converted to half-open UTF-16 offsets into the **chunk** text with `vox_core::text16` (text16.rs:11-143):
   - utf8: `is_char_boundary`, then `utf16_offset`;
   - codepoint: an indexed char walk;
   - utf16: used as is.
   A cue is dropped when it is out of range, off a boundary, has `from >= to`, or has a non-finite time.
4. `WordCue { start_sec: start.clamp(0, dur), from, to }` (windows_voices.rs:32) goes through `alignment_from_cues(text, &cues, dur)` (windows_voices.rs:40, pub and compiled on both OSes). It sorts, clamps and fills gaps. With no valid cues or with `word_timing=false`, `vox_tts::mock::even_alignment(text, dur)` (mock.rs:60) is used. The result has one entry per UTF-16 unit; an astral character takes 2 entries, the second being "".
5. `SynthesisResult { audio, mime: MIME_WAV, alignment, request_id: None, character_cost: Some(0.0) }`. `character_cost` is **never None**, because None bills the chunk's length (engine.rs:486-488).

### Timeouts (the Vox side; the engine applies none)

| Phase | Deadline |
|---|---|
| spawn → `hello` reply | `start_ms` |
| `voices` | 10 s |
| `synthesize`, idle | no frame for `idle_ms`. Any frame for the pending id resets it. The first request after a spawn adds `load_ms` to its first idle window. |
| `synthesize`, absolute | `synth_max_ms` (+ `load_ms` on the first request) |
| cancel grace | 2 s |
| quit grace | 400 ms on shutdown, 2 s on remove or smoke test |

Waiting is a loop:

- The per-add-on slot lock is taken with `parking_lot::Mutex::try_lock_for(50 ms)`, checking `cancel.is_cancelled()` between tries. A superseded session or preview never sits behind a slow chunk without a way out.
- Then `rx.recv_timeout(50 ms)` checks cancel and the deadlines each tick. `CancelToken` has no callback (cancel.rs:1-5).
- **On cancel:** send `cancel{target}` and wait up to the grace for the target's terminal frame, discarding it. If none arrives, `kill_tree()`; the next call respawns. Return `TtsError::cancelled()`.
- **On timeout:** `kill_tree()` and return `TtsError::new(Unknown, "KittenTTS 2 took too long to respond.")`, not retryable.

### Crash and restart policy

- **Crash:** EOF on stdout, child exit, a broken pipe before the reply, or a protocol violation. The result is `TtsError::new(Unknown, "KittenTTS Lite stopped unexpectedly.")` with `retryable = true`. Nothing is billed, so `with_retries` (engine.rs:422, 1/2/4 s) respawns lazily on the next attempt.
- **Crash budget:** 3 crashes, violations or timeouts within 10 minutes mark the runtime **Broken**. Further calls fail immediately, not retryable, with "KittenTTS Lite keeps stopping. Repair it on the Voices page." The host notifies the core, which republishes the row. Repair (an Install of the same version), a reinstall or a restart clears it.
- **Spawn errors** (`vox_win::process::classify_spawn_error`), all not retryable:
  - NotFound: "KittenTTS Lite is incomplete. Repair it on the Voices page." (Broken)
  - Windows raw OS error 5, 225, 1260 or 4551: "Windows blocked KittenTTS Lite from starting."
  - Linux EACCES or EPERM: "Your system blocked KittenTTS Lite from starting. Is the add-ons folder on a noexec drive?"
- **Idle reaper:** the core timer `TimerId::AddonIdle` (every 60 s) calls `host.reap_idle(now)`. It uses `try_lock` and quits helpers idle longer than `idle_exit_sec` to free model RAM.
- **Shutdown:** `("addons", Box::new(move || host.shutdown(Duration::from_millis(400))))` joins the `run_bounded` list (lifecycle.rs:151-164). It quits every helper, waits, then calls `kill_tree`. The job, PDEATHSIG and stdin EOF cover the hard-exit paths.

### Stderr logging

- Lines are capped at 1 KiB and logged to tracing target `"addons"` with the add-on id.
- Rate: the first 200 lines per process, then at most 1 per second, so the pipe never fills.
- The last 64 lines are kept in a ring buffer and logged at warn level after a crash or kill, for diagnosis.

### Mapping to TtsError

| Helper outcome | ReadErrorCode | Retryable | Message (shown) |
|---|---|---|---|
| crash or protocol violation (first ones) | Unknown | yes | "<name> stopped unexpectedly." |
| Broken | Unknown | no | "<name> keeps stopping. Repair it on the Voices page." |
| timeout | Unknown | no | "<name> took too long to respond." |
| cancel | (cancelled()) | — | aborted |
| unknown_voice | Unknown | no | "This voice isn't in <name> <version>." |
| out_of_memory | Unknown | no | "Not enough memory for <name>." |
| model_load_failed | Unknown | no | "<name> couldn't load its voice model. Try repairing it." |
| other error codes | Unknown | no | "<name> couldn't read this text." |
| not installed / removed / incompatible | Unknown | no | "<name> isn't installed. Install it on the Voices page." / "…was removed." / "<name> needs an update." |

---

## Vox integration

### vox-core

- **src/providers.rs:** the Addon variant, its arms, `ROUTED`, and `provider_of`/`from_id` over ROUTED. Tests: ROUTED round-trip; `provider_of("addon:x:y") == Addon`; `split_voice_id("addon:")` is None.
- **NEW src/addons.rs** (`pub mod addons;` in lib.rs):
  - id and voice helpers (see Identity);
  - `Version`;
  - `Catalogue`, `CatalogueAddon`, `Release`, `Artifact`, `ArchiveFormat`, `parse_catalogue`, `pick_release`, `platform_key()`;
  - `AddonManifest`, `ManifestVoice`, `Timeouts` (with `clamp()`), `parse_manifest`, `InstalledPointer`;
  - the view snapshot (Debug, Clone, PartialEq, Default; no serde):
    ```rust
    pub struct AddonsView { pub catalogue: CatalogueState /* Idle|Loading|Ready|Offline(String)|Unavailable */, pub items: Vec<AddonState> }
    pub struct AddonState { pub id: String, pub name: String, pub description: String, pub credit: String,
        pub installed: Option<InstalledInfo /* version, voices */>, pub offered: Option<Offer /* version, size, voices */>,
        pub job: Option<AddonJob>, pub error: Option<String>, pub resumable: bool, pub broken: Option<String>, pub incompatible: bool }
    pub enum AddonJob { Downloading { done: u64, total: u64 }, Verifying, Installing, Removing }
    ```
- **src/constants.rs:** the `ADDON_*` constants (see Catalogue), plus `ADDON_PROGRESS_MS = 250`, `ADDON_CRASH_LIMIT = 3`, `ADDON_CRASH_WINDOW_SEC = 600`, `ADDON_CANCEL_GRACE_MS = 2000`, `ADDON_QUIT_GRACE_MS = 400` and `ADDON_MAX_LINE = 96 << 20`.
- **src/voices.rs:** the `addon_group` tie-break and its test.

### vox-tts

- **src/providers.rs:**
  - `connect` (:45) gets the arm `Addon => None`.
  - `not_connected` (:58) gets an Addon arm, `TtsError::new(Unknown, "This offline voice isn't installed. Install it on the Voices page.")`. It is a fallback only, since the host is normally registered.
  - New `pub fn https_agent(ua, proxy, HttpsAgentOptions { https_only, max_redirects }) -> ureq::Agent`. The existing `pub(crate) fn build_agent` (:66) calls it with the defaults (false, 10), so its behaviour is unchanged.
- **src/engine.rs:** `providers()` (:711) iterates `Provider::ROUTED`.
- **src/engine_tests.rs:** an in-process `FakeProvider` routing test with `mock = false`. It is the first test of provider routing at all (the map notes no engine test reaches a `SpeechProvider`).

### vox-store

- **src/paths.rs:** `local_data_dir()`, `addons_root()`.
- No other change. vox-addons uses the existing pub `fsutil::{write_file_atomic, retry_busy, remove_dir_all_retry}` (fsutil.rs:56/142/242).

### vox-win

- **NEW src/process.rs** (Windows) and **src/linux/process.rs**, re-exported from lib.rs (:31-67) in the existing twin pattern:
  - `pub struct Spawner` (`Spawner::global()`, which starts the `vox-addons-spawn` thread on first use);
  - `pub fn spawn(&self, cmd: Command) -> io::Result<(Child, HelperGuard)>`;
  - `impl HelperGuard { pub fn kill_tree(&self) }`, with Drop doing the same;
  - `pub fn classify_spawn_error(e: &io::Error) -> SpawnBlock { NotFound, Blocked, NoExec, Other }`.
- **NEW src/disk.rs** and **src/linux/disk.rs:** `pub fn free_bytes(path: &Path) -> Option<u64>`.
- **Cargo.toml:** add the windows feature `"Win32_System_JobObjects"`.

### NEW crate crates/vox-addons

- Workspace member and `[workspace.dependencies]` entry; licence Proprietary.
- Dependencies: vox-core, vox-text, vox-tts, vox-store, vox-win, serde, serde_json, ureq (platform-verifier), sha2, hex, base64, parking_lot, crossbeam-channel, tracing, thiserror, plus:
  - `tar = { version = "0.4.46", default-features = false }`
  - `flate2 = { version = "1.1.10", default-features = false, features = ["rust_backend"] }`
  - `ring = "0.17.14"`
  All three are already in Cargo.lock and licensed MIT/Apache, Apache-2.0 AND ISC, and MIT/Zlib/Apache.
- Dev-dependencies: tiny_http, tempfile.

| File | Contents |
|---|---|
| src/lib.rs | public re-exports |
| src/error.rs | `AddonError { DiskFull{needed}, Offline(String), UntrustedRedirect(String), Http(u16), Oversize, Corrupt, UnsafeArchive(String), BadManifest(String), HelperCheckFailed(String), Incompatible, Cancelled, Io(String) }`, `user_message(name) -> String`, `resumable() -> bool` |
| src/protocol.rs | `HostMsg`, `HelperMsg` (serde, `#[serde(tag="type", rename_all="snake_case")]`, `#[serde(other)] Unknown`), `LineReader` with caps, `encode(&HostMsg) -> Vec<u8>` |
| src/helper.rs | `HelperClient { spec, slot: Mutex<Slot>, crashes: Mutex<VecDeque<Instant>>, broken: RwLock<Option<String>>, last_used: AtomicU64 }`, with `hello`, `voices`, `synthesize(voice, text, word_timing, &CancelToken) -> Result<HelperAudio, HelperError>`, `stop_and_wait(grace)`, `idle_for()`. `HelperError::into_tts(name)` |
| src/host.rs | `AddonHost { runtimes: RwLock<BTreeMap<String, Arc<AddonRuntime>>>, names: RwLock<HashMap<String,String>>, on_health: Box<dyn Fn(&str) + Send + Sync> }`, with `replace`, `remove`, `is_installed`, `status(id) -> RuntimeStatus`, `set_known_names`, `reap_idle`, `shutdown`, and `impl SpeechProvider`. `summary()` returns a cached "2 add-ons · 123 voices" and never touches a helper. `region()` returns "". |
| src/runtime.rs | `AddonRuntime { manifest, pointer, dir, voices: Arc<Vec<ManifestVoice>>, helper: HelperClient, removed: AtomicBool }`, with Drop stopping the helper |
| src/root.rs | `AddonsRoot { path }`: `open`, `scan() -> Vec<ScanItem>`, `sweep()`, `staging()`, `load_cached_catalogue()` |
| src/net.rs | `NetPolicy { hosts, allow_loopback_http }`, `get_following(agent, url, policy)`, the catalogue fetch with the 1 MiB body limit |
| src/catalogue.rs | `fetch(source, agent) -> Result<VerifiedCatalogue, AddonError>`, signature and sequence checks, cache write |
| src/download.rs | `download(agent, &Artifact, part, &CancelToken, &mut dyn FnMut(u64,u64)) -> Result<(), AddonError>` with resume |
| src/extract.rs | `extract(archive, format, dest, ExtractCaps) -> Result<ExtractStats, AddonError>` |
| src/install.rs | `run(ctx, job, progress)`, `remove(root, runtime)`, the smoke-test probe |
| src/fake.rs + src/bin/vox-addon-fake.rs | the fake helper (`#[doc(hidden)] pub fn run(args, stdin, stdout, stderr) -> i32`; the bin has `test = false`) |

### vox-app

- **Cargo.toml:** depend on vox-addons.
- **src/command.rs**, next to `Catalog(CatalogResult)` at :257:
  ```rust
  Addons(AddonCmd),
  pub enum AddonCmd {
      FetchCatalogue { refresh: bool },
      Install { id: String },              // install, update, resume, repair
      Cancel { id: String },
      Remove { req: RequestId, id: String }, // answered by UiUpdate::Done{req}
      Event(AddonEvent),                    // internal, from workers and the host
  }
  pub enum AddonEvent {
      Catalogue(Result<Arc<VerifiedCatalogue>, String>),
      Stage { id: String, gen: u64, stage: InstallStage },
      Installed { id: String, gen: u64, result: Result<Box<InstalledAddon>, String>, resumable: bool },
      Removed { id: String, req: RequestId, result: Result<(), String> },
      Health { id: String },
      Swept,
  }
  ```
- **src/core/mod.rs:**
  - `handle_command` (:342-440): `Command::Addons(c) => self.addons_command(c)`.
  - `command_name` (:909): `"addons"`.
  - `owed_answer` (:1028): `Command::Addons(AddonCmd::Remove { req, .. }) => Some(OwedAnswer::Done(*req))`.
- **NEW src/core/addons.rs:**
  - `AddonManager { root: Option<AddonsRoot>, host: Option<Arc<AddonHost>>, catalogue: CatalogueState, catalogue_at: u64, jobs: HashMap<String, Job>, errors: HashMap<String, String>, resumable: HashSet<String>, retired: Vec<(Weak<AddonRuntime>, PathBuf)>, next_gen: u64 }`, owned by Core.
  - Methods: `addons_command`, `publish_addons()` (builds `AddonsView` and sends `UiUpdate::Addons`), `on_idle_tick()` (`reap_idle` plus retired-dir cleanup), `notices()`.
  - The catalogue fetch runs on `spawn_worker("vox-addon-catalogue")` after `maybe_recheck_proxy(false)`. The TTL is honoured unless `refresh`.
- **src/update.rs:** `UiUpdate::Addons(Arc<AddonsView>)`. Extend `collapse_player_updates` (:263) so consecutive `Addons` updates in one batch collapse to the newest.
- **src/ui/mod.rs:** add `UiUpdate::Addons { .. }` to `reaches_every_page` (:1481).
- **src/app.rs** (:194-198): after the Windows registration, outside demo mode:
  ```rust
  let root = vox_store::paths::addons_root().map(AddonsRoot::open);
  let host = root.as_ref().map(|r| AddonHost::new(r.scan(), on_health));
  if let Some(h) = &host { engine.set_provider(Provider::Addon, Some(h.clone())); }
  ```
  Pass `root` and `host` into Core. `on_health` is wired to `core.send(Command::Addons(AddonCmd::Event(AddonEvent::Health{id})))` once the CoreHandle exists, through a `OnceLock` set after spawn.
- **src/core/timers.rs:** `TimerId::AddonIdle`, armed every 60 s while the host has runtimes or retired entries.
- **src/core/lifecycle.rs** (:151-164): the `"addons"` shutdown task.
- **src/core/reader.rs** (:151-171):
  - Compute the **effective voice id** first (the expression at :196, moved up) and gate on it.
  - New arm before the generic one: `Provider::Addon =>` if `parse_addon_voice_id` fails, or the host reports not installed, Broken or Incompatible, then:
    - `self.toast(Toast::error(msg).with_action("Open Voices", Some(AppRoute::page(AppPage::Voices))))`;
    - `return ReadResult::failed(ReadErrorCode::Unknown, &msg)`.
  - Regression tests: ElevenLabs, Windows and Polly override voices still pass the gate when connected and are blocked when not.
- **src/core/queries.rs** (:88-94): filter out `Provider::Addon`.
- **src/core/notices.rs** (:20-36): append an "Offline voice add-ons" section with, per installed add-on, name, version, credit and the contents of each `notices` file (at most 256 KiB each).
- **src/ui/settings.rs** (:108): the `other_provider_notice` Addon arm.

---

## UI

### Models (ui/models.slint, after VoicesData)

Every string is built in Rust, following the models.slint rules (models.slint:1-8):

```slint
export struct AddonRow {
    id: string, name: string, description: string,
    status: string,           // "48 MB · 8 voices" | "Installed · 8 voices · v0.8.1" | "Downloading · 12 MB of 48 MB"
    credit: string,           // "Powered by KittenML" or ""
    badge: string,            // "Installed" | "Needs a newer Vox" | ""
    badge-ok: bool,           // success dot vs muted
    primary-label: string,    // "Install · 48 MB" | "Update · 52 MB" | "Cancel" | "Resume" | "Try again" | "Repair" | "" (hidden)
    primary-a11y: string,     // "Install KittenTTS Lite, 48 megabytes"
    primary-secondary: bool,  // secondary kind (Cancel)
    busy: bool,               // primary shows loading (Verifying/Installing)
    progress: float,          // < 0 hides the meter
    progress-text: string,    // "12 MB of 48 MB"
    can-remove: bool, removing: bool, remove-a11y: string,
    error: string,            // AlertLine when non-empty
}
```

`VoicesData` (models.slint:506-523) gains three fields:

- `addons: [AddonRow]`
- `addons-note: string`
- `list-label: string`, which replaces the hard-coded "ElevenLabs library" at voices.slint:920 and :970 with "Voices".

### Row states (`fn addon_rows(&AddonsView) -> (Vec<AddonRowView>, String)` in ui/voices.rs, pure and unit-tested)

| State | status | Trailing controls |
|---|---|---|
| Available | "48 MB · 8 voices" | primary "Install · 48 MB" (Icons.download) |
| Downloading | "Downloading · 12 MB of 48 MB" | Meter + secondary "Cancel" |
| Verifying / Installing | "Checking the download…" / "Installing…" | primary with loading |
| Installed | "Installed · 8 voices · v0.8.1" | Badge "Installed" (success dot) + danger "Remove" (Icons.trash-2) |
| Update available | "Installed · v0.8.1 · v0.8.2 available" | primary "Update · 52 MB" + Remove |
| Removing | unchanged | Remove with loading |
| Failed (resumable) | "Download stopped at 31 MB of 930 MB" | AlertLine (error) + "Resume" |
| Failed | unchanged | AlertLine + "Try again" |
| Broken | "Installed · v0.8.1" | AlertLine "KittenTTS Lite keeps stopping." + "Repair" + Remove |
| Incompatible | "Installed · v0.8.1" or the catalogue text | muted Badge "Needs a newer Vox" (+ Remove if installed) |

Copy:

- Failure messages:
  - "The download was damaged. Try again."
  - "Not enough disk space. KittenTTS 2 needs 1.2 GB free."
  - "Couldn't download KittenTTS 2. Check your connection and try again."
  - "Windows blocked KittenTTS Lite from starting."
- `addons-note`:
  - "Loading offline voices…" while loading with nothing installed;
  - "Couldn't load the list of offline voices. Refresh to try again." when the fetch fails and there is no cache;
  - "Offline voices aren't available yet." when no key is configured;
  - otherwise "Offline voices run on this PC. Nothing you read is sent online."
- Credit line: "Powered by KittenML", shown under the description when it is non-empty.
- Empty and NoKey copy (ui/voices.rs page_view, :239): a keyless Linux user sees **Empty**, not NoKey, because Windows voices are always registered. Both bodies gain "…or install an offline voice below." Empty becomes "No voices yet. Install an offline voice below, or connect a voice service in Settings."
- Remove dialog: `DialogSpec { id: "addon-remove:<id>", owner: AppPage::Voices, title: "Remove KittenTTS 2?", body: "Frees 1.1 GB. Its 115 voices will disappear from the list. You can install it again any time.", confirm_label: "Remove", cancel_label: "Cancel", danger: true, busy }`. When the current read uses that add-on, the body adds " The current read stops." The pending `remove_req: Option<(RequestId, String)>` resolves on `Done{req}`.

### Slint

- **Meter:** promote it from ui/pages/settings/parts.slint:520 to `ui/design/meter.slint` and export it from ui/design/index.slint; parts.slint re-exports it, so library.slint is unchanged. Its role is `progress-indicator`. ProgressLine is not used, because its role is slider (progress-line.slint:54). ui/pages/history.slint:453 has a private `component Meter`: rename it to `HistoryMeter` if history.slint ever imports from the design index.
- **ui/pages/voices.slint:**
  - New `AddonItem`, modelled on VoiceItem (:372-496) and ProviderCard: a 40 px tile with Icons.puzzle; name, description, status, credit; the Meter when `progress >= 0`; AlertLine when `error != ""`; sm Buttons with `accessible-text` from Rust.
  - Callbacks `addon-primary(string)`, `addon-remove(string)` and `addons-retry()`.
  - **Body restructure (:904-985):** in every non-loading state, one ScrollArea holds either a compact StatePanel (no-key, error, empty or no-match; minimum height, not full height) or the voice list. It is followed by `SectionLabel { text: "Offline voices"; trailing-text: <installed count> }`, the AddonItem rows and the note. The loading skeleton stays as it is.
- **ui/windows/main-window.slint** (:237-245, :653-667): `voices-addon-primary(string)`, `voices-addon-remove(string)` and `voices-addons-retry()`, forwarded from VoicesPage.
- **ui/mod.rs** (:1790-1799): `on!(m, on_voices_addon_primary, voices_cb, |c, cx, id| c.addon_primary(cx, &id));` and the same for remove and retry.
- **VoicesController:**
  - On activation, send `FetchCatalogue { refresh: false }`; Refresh sends `true`.
  - `addon_primary` maps the row state to Install or Cancel and sets `busy` optimistically.
  - Store `UiUpdate::Addons` and rebuild.
  - Errors show inline only. No core toasts, because they also reach the widget and OS notifications.
- **Stability (ui/stable.rs):** `keyed!(AddonRow, |r| RowKey::Str(r.id.clone()))` next to :100-103, and `addons` added to `composite!(VoicesData { models: [...] })` at :117. Rows are patched in place at progress-tick rate (at most 4 per second), so hover, focus and a pending Cancel click survive.

### Accessibility

- Meter: `accessible-role: progress-indicator`, `accessible-value` from progress, and an accessible label "Downloading KittenTTS 2, 25 percent".
- Buttons carry Rust-built `accessible-text`: "Install KittenTTS Lite, 48 megabytes", "Cancel download of KittenTTS 2", "Remove KittenTTS Lite".
- The section label is a real heading. Error lines use AlertLine (role alert).
- Rows are keyed, so screen-reader focus is not lost on progress ticks.
- The dialog uses the existing focus-trapped mechanism (ui/mod.rs:193-221).

### Previews

- New hand-written fixtures in ui/previews/: `voices-addons-available.json`, `-downloading`, `-verifying`, `-installed`, `-update`, `-failed`, `-resumable`, `-broken`, `-incompatible`, and `voices-empty-addons.json`. List them in the voices.slint header (:6-9).
- Add `"addons": []`, `"addons-note": ""` and `"list-label": "Voices"` to the 17 existing fixtures if vox-shot rejects missing fields.
- Render with `target/release/vox-shot` (`vox-shot.exe` on Windows) at 892x700, 720x700 and 812x548, dark and with `--set Theme.dark=false`.
- The demo `AddonsView` fixture makes the section appear in the in-app shots (shots.rs:72).

---

## Test helper and tests

### Fake helper

`crates/vox-addons/src/bin/vox-addon-fake.rs` calls `vox_addons::fake::run`. Cargo builds it for vox-addons integration tests and exposes `env!("CARGO_BIN_EXE_vox-addon-fake")`, with `.exe` on Windows. It is never packaged; vox.iss copies only vox.exe.

Normal behaviour:

- It implements protocol 1 exactly.
- `voices`: `alpha` "Alpha" (female, en-US) and `beta` "Beta" (male, en-GB).
- `synthesize`: a 24 kHz s16 mono sine via `vox_tts::wav::encode_wav` (wav.rs:7), 40 ms per char and capped at 20 s, at 440 Hz for alpha and 220 Hz for beta. Word cues are split at whitespace in the declared offset unit.
- It honours `cancel` and exits on stdin EOF.

Startup flags, set through the manifest `args`:

- `--offsets utf8|codepoint|utf16`
- `--heartbeat-ms N`
- `--tag v2`, echoed in the hello `version` so a test can tell which version answered
- `--count-file PATH`, which logs each spawn
- `--log-file PATH`, which logs each request header
- `--mode` one of:
  - `normal`
  - `exit-at-start`
  - `no-hello`
  - `slow-hello:MS`
  - `protocol:2`
  - `banner` (writes text before hello)
  - `no-eof-exit`
  - `spawn-grandchild` (reports `grandchild_pid` in hello)
  - `stderr-flood` (10 MB)

Per-request triggers at the start of the text:

- `@crash` (half a line, then exit 3)
- `@crash-after:N`
- `@hang` (no reply)
- `@slow:MS` (heartbeats; honours cancel)
- `@ignore-cancel`
- `@garbage` (non-JSON line)
- `@huge` (line over the cap)
- `@bad-wav` (wrong RIFF, 0xFFFFFFFF size, 8-bit PCM)
- `@long-wav`
- `@bad-offsets` (out of range, reversed, mid-codepoint)
- `@no-words`
- `@err:CODE`
- `@stale` (an old-id reply first)

### Tests

All run under the existing `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` matrix on windows-latest and ubuntu-latest with xvfb (.github/workflows/ci.yml:15-49). **No workflow change.**

Rules for every test:

- Tempdir roots only, never the real LocalAppData or ~/.local/share.
- Network on 127.0.0.1 only.
- Injected short timeouts (hello 2 s, idle 1 s, cancel grace 300 ms) with generous upper bounds (under 10 s), so a slow runner or Defender scanning a fresh exe does not flake.

Locally: `~/.cargo/bin/cargo test --workspace` and `~/.cargo/bin/cargo clippy --workspace --all-targets -- -D warnings`.

| Suite | Covers |
|---|---|
| vox-core unit | id and key grammar (DOS names, 32/64 limits, `--`, ':'); `addon_voice_id`/parse round-trip; `addon_voice_name`; `provider_of("addon:x:y") == Addon` with nothing registered; ROUTED/from_id round-trip; EXTRA unchanged; Version ordering; `parse_catalogue` dropping bad entries (http URL, bad hash, bad id, duplicates) and handling schema 2; `pick_release` (protocol, min_vox, platform); `parse_manifest` clamps and rejections (env keys, exe outside the dir); sort grouping of two add-ons that share names |
| vox-tts unit | `engine_tests.rs` FakeProvider routing with mock = false (WAV chunk, alignment, Complete with billed 0); the `https_agent` defaults equal the old build_agent |
| vox-win unit | the test binary re-execs itself as a child and grandchild: `kill_tree` kills both; spawning from a short-lived thread through the Spawner leaves the child alive after that thread exits (the PDEATHSIG trap); classify_spawn_error |
| vox-addons `tests/protocol.rs` + unit | line reader caps, CRLF, truncated lines; a 10k-iteration deterministic pseudo-random input loop never panics or allocates beyond the caps |
| vox-addons `tests/helper.rs` | handshake (normal, `protocol:2` gives Incompatible, `no-hello`/`slow-hello` time out, `banner` is a violation, `exit-at-start` crashes); synthesize gives a valid WAV, MIME_WAV, `Some(0.0)`, and a consistent alignment of length == len16 on "héllo 👋 wörld" in all three offset units; `@bad-offsets`/`@no-words` fall back to even_alignment; `@crash` is retryable, then a respawn (count-file); 3 crashes give Broken with no further spawn; `@hang` times out at the idle deadline and the process is gone; `@slow` with heartbeats outlives idle_ms and succeeds; cancel during `@slow` returns cancelled within the grace and the next call works; `@ignore-cancel` is killed and respawned; `@garbage`/`@huge` violate the protocol and kill; `@bad-wav` gives an audio error and the helper is kept; `@stale` replies are discarded; `stderr-flood` does not deadlock; 3 threads calling synthesize are serialised in FIFO order (log-file); stdin EOF ends the process within 2 s; `spawn-grandchild` then `kill_tree` leaves no grandchild |
| vox-addons `tests/host.rs` | two add-ons `fake-a`/`fake-b` with identical voice keys: 4 voices, distinct ids `addon:fake-a:alpha` and `addon:fake-b:alpha`, categories, and names "Alpha (Fake A)"; routing reaches the right process (tone or log); list_voices and summary never spawn (count-file empty); voices.json fallback; unknown id gives the "isn't installed" copy with code Unknown; `voice_aliases` resolve; reap_idle; shutdown; `replace` while a `@slow` request runs (the old one finishes, then the old process exits) |
| vox-addons `tests/engine.rs` | a real `TtsEngine` (mock = false) with `set_provider(Addon, host)` and a 3-chunk session on `addon:fake-a:alpha`: WAV chunks, usable alignment, Complete, billed 0; `@crash-after:1` is retried and delivered; a crash on chunk 0 gives Failed after the budget; a later chunk failing cuts the read short; **update while playing** (`--tag v1` then `replace` with v2 after chunk 1, so later chunks come from v2, the session completes and the old dir becomes deletable); remove mid-read cuts short with "was removed"; `addon:missing:alpha` fails with Unknown and **zero** hits on the tiny_http ElevenLabs mock |
| vox-addons `tests/install.rs` | tiny_http on 127.0.0.1, loopback NetPolicy, a test Ed25519 key from a fixed seed. Catalogue: good, bad and missing signature, lower sequence, over 1 MiB, cache re-verified. Download: success; a drop at 40 % then Range resume with the hash OK; a server ignoring Range; 416; more than size aborts early; wrong hash (Corrupt, part deleted); redirect to a disallowed host, to http, or more than 5 hops; cancel (part deleted). Disk full via an injected writer and free_bytes. Hostile archives built with `tar::Builder` plus raw header edits: `../evil`, `/abs`, `C:x`, `a:b`, pax path `..`, symlink, hardlink, char device, fifo, sparse, `CON.txt`, `aux`, trailing dot or space, case collision, depth 9, a 201-char path, too many entries, a gzip bomb, a truncated gzip, missing addon.json, id or version mismatch, exe outside the dir. Each expects nothing written outside staging (the test walks the tmp parent) and staging removed. Commit and recovery: a crash simulated between rename and current.json keeps the old version current and the sweep removes the orphan; current.json pointing at a missing dir means not installed; a smoke-test failure (`exit-at-start`) commits nothing; reinstalling the **same version while its helper runs** succeeds (rand dir); remove while running. End to end: install the fake from a served tar.gz, then synthesize, update, remove, and the dir is gone. Linux: the exe is 0755 and a non-executable exe gives the blocked copy. |
| vox-app | `addon_rows` for every state; `page_view` shows the section in NoKey, Empty, Error and NoMatch; the read gate (an override voice of an uninstalled add-on is blocked with code Unknown, the Voices route and no settings action; regressions for ElevenLabs, Windows and Polly overrides); `Remove` answered even when the handler panics (owed_answer); a startup scan of a pre-built fixture dir registers the host (no spawn: voices come from the manifest); `provider_statuses` excludes Addon; the Remove dialog flow |
| Preview fixtures | rendering every `voices-addons-*.json` with vox-shot (exit 2 on a schema mismatch); manual, plus STATUS.md instructions for both OS paths |

---

## Security

- **Trust root.** An Ed25519 signature over the exact catalogue bytes, checked against compiled-in rotatable keys (`ADDON_CATALOGUE_KEYS`, ring), plus a monotonic `sequence` against rollback. The URL is pinned to a fixed tag. With no key configured, the feature is inert ("Offline voices aren't available yet").
- **Transport.**
  - `https_only(true)`, and Vox follows redirects itself (at most 5 hops) against a host allowlist.
  - Platform certificate verification. The CONNECT proxy is the same as the other providers. Known limit: no ProxyConfig.bypass and no PAC (providers.rs:66-80).
  - Body caps: the catalogue at 1 MiB, the archive at exactly `size`.
- **Integrity.** Streaming SHA-256 against the signed catalogue, an exact size match, and nothing extracted before verification. A resumed part is fully re-hashed.
- **Extraction.** Entry-by-entry with a strict whitelist, path normalisation, a reserved-name and ADS (`:`) ban, case-collision detection, count and byte caps, `create_new` writes, no permissions or xattrs, and chmod 0755 only on the declared exe. `unpack()` is never called.
- **Ids as paths.** `addon_id` is restricted to `[a-z0-9-]` and voice keys to `[A-Za-z0-9._-]`, both validated at every parse. The exe path is canonicalised and checked to be inside its version dir. PATH is never searched.
- **Process.**
  - The helper environment is scrubbed of `VOX_*`, and the manifest cannot set PATH, LD_* or DYLD_*.
  - No inherited console; job object or process group containment; no WER dialogs; helpers cannot outlive Vox.
  - Helpers run as the user and are **not sandboxed**. The signature is what authorises the code.
- **Untrusted output.** Helper strings (error messages, stderr) are logged and never displayed. Voice names and descriptions are length-capped and come from the signed release's manifest. Every helper line, WAV and word list is size-capped. A misbehaving helper can only cost a timeout and a kill.
- **Licensing.**
  - espeak-ng (GPL-3.0) ships only inside the KittenTTS Lite helper archive, with its COPYING and source offer listed in `notices`. Vox (licence Proprietary, Cargo.toml:21) never links it, and the stdio boundary keeps it a separate program.
  - KittenTTS 2's community licence text ships as a notice, and "Powered by KittenML" appears on the row (`credit`) and in third-party-notices.txt (notices.rs).
  - Have a lawyer review this before the real helpers ship.
- **Windows reputation.** The helpers and Vox are unsigned (README.md:83-84). Smart App Control, WDAC or Defender can block a fresh exe without Mark-of-the-Web. The smoke test catches this before commit, and errors 5, 225, 1260 and 4551 show "Windows blocked …". The real fix is code signing in a future release workflow.
- **Known limits.**
  - ureq has no read-idle timeout, so a stalled socket lingers until the `recv_body` deadline. Cancel is immediate in the UI.
  - The window between spawn and job assignment is covered only by the "no children before hello" rule.

---

## PR split

Each PR passes `~/.cargo/bin/cargo test --workspace` and `~/.cargo/bin/cargo clippy --workspace --all-targets -- -D warnings` on both CI OSes. Docs are updated in the PR that changes the behaviour.

1. **Add-on voice identity, error copy and the read-gate fix** (useful on its own).
   - vox-core: `Provider::Addon`, `ROUTED`, `provider_of`/`from_id` over ROUTED, the arms, `addons.rs` id helpers only, the sort tie-break, tests.
   - vox-tts: the `connect` and `not_connected` Addon arms, `providers()` over ROUTED, the `engine_tests.rs` FakeProvider routing test (the first test of provider routing).
   - vox-app: the reader gate on the **effective voice** (fixes the existing override gap at reader.rs:155 vs :196) with regression tests; the `provider_statuses` filter; the `other_provider_notice` arm.
   - Docs: ARCHITECTURE.md §4.21 (id grammar, ROUTED).
   - Verify: unit tests; manual check that a history replay with an unconnected Polly override voice is now gated.
2. **vox-win: contained helper processes and free space.**
   - `process.rs` and the Linux twin (Spawner thread, job object, PDEATHSIG and getppid, kill_tree, classify_spawn_error), `disk.rs` free_bytes, feature `Win32_System_JobObjects`.
   - Verify: re-exec child and grandchild tests on both OSes (kill_tree, the spawn-thread survival test).
3. **vox-addons: protocol, helper client, AddonHost, fake helper.**
   - The new crate with protocol, helper, host and runtime; the vox-core manifest types and Timeouts; the fake bin; tests helper.rs, host.rs and engine.rs (add-on dirs built directly in tempdirs; no installer yet).
   - The `https_agent` export is not needed yet.
   - Docs: NEW docs/addons-protocol.md.
   - Verify: CI integration tests, including update-while-playing and remove-mid-read.
4. **On-disk store, startup scan, side-loading.**
   - vox-store `local_data_dir`/`addons_root`; vox-addons `root.rs` (scan, sweep); vox-app startup registration, the shutdown task, `TimerId::AddonIdle`, the notices section.
   - Shippable: a helper author can drop a `source: "local"` add-on into `VOX_ADDONS_DIR` and preview and read with it from the Voices list.
   - Verify: a core test with a fixture dir; manually run Vox with `VOX_ADDONS_DIR` pointing at a fake add-on, preview a voice, then read.
5. **Install pipeline: catalogue, download, extraction, commit.**
   - vox-core catalogue types, `pick_release` and constants (keys empty, so inert); `vox_tts::providers::https_agent`; vox-addons net, catalogue, download, extract and install; workspace pins for tar, flate2 and ring (all three already in Cargo.lock; `cargo tree -d` shows no new crates).
   - Verify: tests/install.rs, the hostile-archive matrix and the end-to-end test.
6. **Core add-on manager wiring.**
   - `Command::Addons(AddonCmd)` with the `command_name` and `owed_answer` arms, `core/addons.rs`, `UiUpdate::Addons` with batch collapse and `reaches_every_page`, the demo fixture view.
   - Verify: core tests (remove answered on panic, generations ignore stale events, swap republishes AppInfo and voices).
7. **Voices page "Offline voices" section.**
   - The models.slint AddonRow and VoicesData fields; Meter promoted to ui/design; the AddonItem and body restructure; main-window forwarding; the `on!` wiring; stable.rs keyed rows and composite; controller, dialog and copy; preview fixtures.
   - Docs: ARCHITECTURE.md §7.3, §7.5, §7.6 and §4.22 "Voice add-ons"; STATUS.md.
   - Verify: `addon_rows` and `page_view` tests; vox-shot renders of every fixture at three sizes, dark and light; a manual install of the fake from a local catalogue served from 127.0.0.1 (debug build, `VOX_ADDON_CATALOGUE_URL`, test key).
8. **Packaging and uninstall cleanup.**
   - vox.iss `taskkill /F /T` and `[UninstallDelete]`; install-linux.sh `--uninstall` with `--keep-addons`; README notes on the add-ons location and Smart App Control.
   - Verify: a manual uninstall on Windows with a running fake helper; the Linux script in a temp HOME.

---

## Carried from the PR 1 review

The adversarial review of PR 1 raised two cases that cannot happen until a host is registered (PR 4). They are
requirements for that PR:

- **The gate must ask the host, not only check that one exists.** With the host always registered (D3), `host.is_none()`
  is false even when the voice's add-on is not installed, so the reader's Addon arm must also refuse when the host
  reports the add-on not installed, Broken or Incompatible (as the vox-app section says). Test: with a host registered
  and nothing installed, an `addon:` override voice is refused at the gate (code Unknown, Open Voices, no record).
- **Gate refusals are marked `ReadResult::toasted`.** PR 1 added the flag so the History and Home replay toasts, the
  play/pause replay's "Nothing is playing" and the widget's Try again fallback (which would otherwise re-read the text
  in the selected voice) all stand down when the core's gate already explained the failure. Every new gate arm must set
  it.

## Open questions for the user

1. **Catalogue signing key.** Should I generate an Ed25519 key pair now (you keep the private key offline, its public half goes into `ADDON_CATALOGUE_KEYS`)? Or should schema 1 ship unsigned? I recommend signing. Without a key, the feature stays inert in release builds.
2. **Hosting.** Should the catalogue (tag `addons-index`) and the add-on archives live on `quacoo1/vox` releases, or in a separate `quacoo1/vox-addons` repo? Either works with the host allowlist.
3. **Updating during a read.** Is switching versions mid-read acceptable (later chunks may sound slightly different)? The alternative is to defer the swap until the current read ends.
4. **Previews during a slow KittenTTS 2 read.** A preview waits behind the chunk being synthesized. Is that fine for now, or should a preview get its own short-lived process?
5. **Speed for KittenTTS Lite.** Is leaving speed to Playback (WSOLA) acceptable for the first release, or do you want the protocol `options.speed` wired to a per-voice setting?
6. **Removing the selected voice.** Keep it selected and let the read gate explain (proposed), or reset to a default voice on Remove?
7. **Uninstalling Vox.** Delete `%LOCALAPPDATA%\Vox\addons` (proposed), or keep the downloaded models for a reinstall?
8. **Platforms.** Ship only windows-x86_64 and linux-x86_64 packages at first, or also aarch64?
9. **Onboarding.** Onboarding still needs an ElevenLabs key or demo mode (onboarding.rs:162). Should an "Install an offline voice" path be added in a later phase?
10. **Credits on the About page.** Should installed add-on credits ("Powered by KittenML") also appear on the About page, or is the add-on row plus third-party-notices.txt enough?
11. **Helper SDK.** If the KittenTTS helpers will be written in Rust, should the protocol types be split out into a small MIT OR Apache-2.0 crate that a GPL helper can link? If the helpers are C++ or Python, docs/addons-protocol.md is enough.