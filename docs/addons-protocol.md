# Vox voice add-on helper protocol, version 1

This is the normative contract between Vox and an offline voice add-on. An add-on is a folder with an `addon.json`
manifest and a helper program; Vox starts the helper when one of its voices is first used and talks to it over the
helper's stdin and stdout. The design behind it is in [addons-design.md](addons-design.md); the Rust side is
`crates/vox-addons` (`protocol.rs` for the messages, `helper.rs` for the client), and `vox-addon-fake`
(`src/fake.rs`) is a complete reference helper.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

---

## 1. The add-on folder

```
<add-on id>/<version dir>/
  addon.json          the manifest (below)
  bin/helper.exe      the helper program (any path inside the folder)
  …                   models, data, notices
```

Vox runs the helper with the version folder as its working directory. Everything the helper needs at run time MUST be
inside that folder; anything it wants to keep between runs (caches) goes to `VOX_ADDON_DATA`.

### 1.1 addon.json (schema 1)

```json
{
  "schema": 1,
  "id": "kittentts-2",
  "version": "2.0.0",
  "name": "KittenTTS 2",
  "short_name": "KittenTTS 2",
  "description": "Expressive offline voices.",
  "protocol": { "min": 1, "max": 1 },
  "exe": "bin/kittentts2-helper.exe",
  "args": ["--models", "models"],
  "env": { "OMP_NUM_THREADS": "4" },
  "timeouts": { "start_ms": 20000, "load_ms": 300000, "idle_ms": 60000, "synth_max_ms": 1800000 },
  "idle_exit_sec": 300,
  "credit": "Powered by KittenML",
  "notices": ["NOTICE.txt"],
  "voices": [
    { "key": "bella", "name": "Bella", "gender": "female", "language": "en-US", "accent": "American",
      "use_case": "narration", "description": "Warm and clear." }
  ],
  "voice_aliases": {}
}
```

| Field | Required | Rules |
|---|---|---|
| `schema` | yes | `1` |
| `id` | yes | 1–32 of `[a-z0-9-]`, starting and ending with a letter or digit, no `--`, not a DOS device name (`con`, `aux`, `com1`…). **Permanent once published.** |
| `version` | yes | 1–4 dot-separated numbers (`2.0.0`), each at most 9 digits |
| `name` | yes | 1–60 characters, shown in Vox |
| `short_name` | no | 1–40 characters, the suffix of voice names (`Bella (KittenTTS 2)`); defaults to `name` |
| `description`, `credit` | no | up to 300 / 100 characters |
| `protocol` | yes | the protocol majors the helper speaks; must overlap Vox's (1–1) |
| `exe` | yes | a path inside the folder (rules below); on Windows it MUST end in `.exe` |
| `args` | no | at most 64 strings of at most 1024 bytes each, without NUL, passed as is |
| `env` | no | at most 64 variables; keys `[A-Z][A-Z0-9_]{0,63}`; `PATH`, `PATHEXT`, `LD_*`, `DYLD_*` and `VOX_*` are refused; values at most 4096 bytes, without NUL |
| `timeouts` | no | see §5; clamped into range |
| `idle_exit_sec` | no | quit the helper after this long unused: `0` (never), otherwise clamped into 30–3600; default 300 |
| `notices` | no | at most 32 paths inside the folder; each MUST exist |
| `voices` | no | at most 1000 entries; `key` (voice key grammar below) unique, `name` 1–60 characters; optional `gender` (≤ 30 characters), `language` (≤ 35), `accent` (≤ 40), `use_case` (≤ 40) and `description` (≤ 300), without control characters (a value that breaks this is left out, the voice is kept). Without the list Vox asks the helper (`voices`) once and remembers the answer. |
| `voice_aliases` | no | at most 1000 `{"old key": "current key"}` pairs, both in the voice key grammar, for a voice an update renamed |

Unknown fields are ignored. A manifest that breaks a rule is not loaded.

Paths (`exe`, `notices`) are `/`-separated and relative, at most 8 levels and 200 bytes; each part is 1–100 bytes of
printable characters other than `< > : " \ | ? *`, never `.` or `..`, does not end in a dot or a space, and is not a
DOS device name, with or without an extension.

### 1.2 Voice ids (stability rules)

Vox stores a voice as `addon:<add-on id>:<voice key>` in settings, history, analytics and voice art. Voice keys are
1–64 of `[A-Za-z0-9._-]`, start with a letter or digit and do not end with `.`.

- The id never contains a version, a platform or a model file name.
- An add-on id is permanent once published.
- Voice keys are a public contract: an update MAY add keys and MUST NOT rename or remove them. A rename that cannot be
  avoided goes into `voice_aliases`.

---

## 2. The process

- Vox starts at most one helper per add-on, the first time one of its voices is used (a read or a preview). It is never
  started just to list voices when the manifest lists them.
- The program is started directly (no shell, PATH is not searched) with `args`, the version folder as its working
  directory, and stdin, stdout and stderr all piped.
- Environment: the user's environment without any `VOX_*` variable, plus `VOX_ADDON_PROTOCOL=1`, `VOX_ADDON_ID`,
  `VOX_ADDON_DIR` (the version folder), `VOX_ADDON_DATA` (a per-add-on data folder Vox creates; it survives updates)
  and the manifest's `env`.
- Containment: on Windows the helper runs with no console window in a job object that is killed with Vox; on Linux it
  leads its own process group and is killed when Vox exits. The helper MUST NOT try to escape either.

### 2.1 Helper rules

1. Write only protocol lines to stdout. Logs go to stderr (Vox logs at most 1 KiB per line, the first 200 lines and
   then one per second, and keeps the last 64 for its crash log).
2. Write nothing to stdout before `hello` arrives.
3. Exit within 1 s of `quit` or of stdin reaching end of file.
4. Start no child processes before replying to `hello`.
5. Handle requests one at a time, in order. Vox sends the next request only after the previous one's last frame, apart
   from `cancel` (which can arrive while a request runs) and `quit`.
6. Answer `hello` and `voices` in under 2 s without loading model weights; load the model on the first `synthesize`.
7. Accept any `synthesize` text up to 4096 UTF-8 bytes and split it internally if the model needs smaller pieces. Vox
   sends at most 900 UTF-16 code units per request.
8. On Windows, call `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX)` at start.

---

## 3. Framing

- One compact UTF-8 JSON object per line, ending in `\n`. A trailing `\r` is ignored, so a helper whose stdout is in
  Windows text mode still works.
- Vox → helper lines are at most 64 KiB. Helper → Vox lines are at most 96 MiB.
- Every message has a `"type"`. Requests carry `"id"` (an unsigned integer that increases for the life of the
  process) and every reply carries the `id` of the request it answers.
- Both sides ignore fields they do not know.
- Vox drops every frame whose `id` is not the pending request's (late answers after a cancel or a timeout, and
  anything written while no request is pending) without keeping it. A reply of an unknown `type` for the pending
  request is ignored but counts as a sign of life.
- Fields of an unexpected shape (a list where a string belongs, say) are ignored rather than refused; `words` keeps at
  most 4096 cues and `voices` at most 1001 entries.
- A helper MUST reply `{"type":"error","code":"unsupported"}` to a request `type` it does not know.
- **Protocol violations**, after which Vox kills the helper: a stdout line that is not a JSON object or has no
  `type`, a line over the cap, or end of file in the middle of a line.

---

## 4. Messages

### 4.1 Vox → helper

```json
{"type":"hello","id":1,"protocol":{"min":1,"max":1},"vox":"0.2.0","os":"windows","arch":"x86_64"}
{"type":"voices","id":2}
{"type":"synthesize","id":3,"voice":"bella","text":"Hello there, wörld 👋.","word_timing":true,"options":{}}
{"type":"cancel","id":4,"target":3}
{"type":"quit","id":5}
```

- `hello` is always the first request. `os` and `arch` are Rust's `std::env::consts` values.
- `synthesize.voice` is a key from the add-on's voice list (after `voice_aliases`). `word_timing` is false when
  nothing will highlight words (the helper MAY then skip timing). `options` is reserved: v1 always sends `{}`, and a
  helper MUST ignore keys it does not know.
- `cancel` asks the helper to stop request `target` as soon as it can. `cancel` and `quit` get no reply.

### 4.2 Helper → Vox

```json
{"id":1,"type":"hello","protocol":1,"name":"kittentts2-helper","version":"2.0.0","offsets":"utf8","word_timing":"exact"}
{"id":2,"type":"voices","voices":[{"key":"bella","name":"Bella","gender":"female","language":"en-US","accent":"American","description":"Warm and clear."}]}
{"id":3,"type":"progress","stage":"loading","fraction":0.4}
{"id":3,"type":"audio","format":"wav","wav":"UklGRiQAAABXQVZF…","offsets":"utf8","words":[{"from":0,"to":5,"start":0.0},{"from":6,"to":12,"start":0.41}]}
{"id":3,"type":"error","code":"unknown_voice","message":"no voice 'x'"}
```

| Reply | To | Fields |
|---|---|---|
| `hello` | `hello` | `protocol`: the one major chosen from Vox's range; `name`, `version`: for the log; `offsets`: the default offset unit of word cues (`utf8`, `codepoint` or `utf16`; default `utf8`); `word_timing`: informative (`exact`, `estimated`, `none`) |
| `voices` | `voices` | `voices`: the same entry format as `addon.json` |
| `progress` | `synthesize` | optional and repeatable; `stage`, `fraction` are informative. Every frame for the pending request resets its idle deadline (§5), so a slow helper SHOULD send one at least every `idle_ms / 2`. |
| `audio` | `synthesize` | the result (below) |
| `error` | any request | `code` (below); `message` is logged and never shown |

#### Audio

- `format` is `"wav"`, and `wav` is the standard base64 of **one complete RIFF/WAVE file** with exact header sizes:
  PCM signed 16-bit or IEEE float 32-bit little-endian (`WAVE_FORMAT_EXTENSIBLE` allowed), 1–2 channels,
  8 000–192 000 Hz, a non-empty `data` chunk whose size is real (no streaming size `0xFFFFFFFF`).
- The audio MUST NOT be longer than 600 s, nor than 0.5 s per UTF-16 unit of the text plus 10 s.
- `words` is optional (leave it out when `word_timing` was false or there is no timing): one cue per word, `from` and
  `to` a half-open range into `text` **exactly as sent**, in the unit of `offsets` (the reply's, else `hello`'s), and
  `start` the word's start in seconds from the start of the audio (finite, ≥ 0, ascending).
- Vox measures the duration from the WAV itself and checks every cue: a cue out of range, not on a character
  boundary, with `from >= to` or a bad time is dropped; with no usable cue, highlighting is spread evenly over the
  audio.
- Audio that breaks these rules fails the request ("<name> sent audio Vox couldn’t use.") but the helper is kept.

#### Error codes

| Code | Meaning | What Vox shows |
|---|---|---|
| `bad_request` | the request was malformed | "<name> couldn’t read this text." |
| `unknown_voice` | no such voice key | "This voice isn’t in <name> <version>." |
| `text_too_long` | the text is over the helper's limit | "<name> couldn’t read this text." |
| `cancelled` | the request was cancelled | nothing (the read was stopped) |
| `model_load_failed` | the model could not be loaded | "<name> couldn’t load its voice model. Try repairing it." |
| `out_of_memory` | not enough memory | "Not enough memory for <name>." |
| `unsupported` | unknown request type | — |
| `unsupported_protocol` | reply to `hello`: no common major; add `"supported":{"min":…,"max":…}` and exit | "<name> needs an update." |
| `internal` | anything else | "<name> couldn’t read this text." |

### 4.3 Version handshake

Vox offers `{"min":1,"max":1}`. The helper replies `hello` with one `protocol` in that range, or with the
`unsupported_protocol` error and exits; Vox then marks the add-on as needing an update and does not start it again
until it is updated. Changes within v1 are additive only (new optional fields, new message types that may be
ignored); a breaking change bumps the major, and Vox supports both majors for at least one release.

---

## 5. Timeouts

The manifest's `timeouts` set how long Vox waits; values outside the range are clamped.

| Field | Range | Default | Deadline |
|---|---|---|---|
| `start_ms` | 5–180 s | 20 s | spawn → the `hello` reply |
| `load_ms` | 0–600 s | 60 s | added to the first idle window and to the absolute limit of every `synthesize` until the helper has returned audio once since it started (model loading) |
| `idle_ms` | 5–600 s | 60 s | a pending `synthesize` fails when no frame for it arrives for this long |
| `synth_max_ms` | 10 s–30 min | 120 s | absolute limit of one `synthesize` |

Vox's own: `voices` 10 s; after `cancel`, 2 s for the request's last frame; after `quit`, 400 ms (when Vox exits) or
2 s (when the add-on is removed) to exit. A helper that misses a deadline is killed.

---

## 6. Failures and restarts

- A crash (stdout end of file, exit, a broken pipe) or a protocol violation fails the request as retryable: Vox
  restarts the helper on the next attempt.
- Three crashes, violations or timeouts within 10 minutes mark the add-on as broken ("<name> keeps stopping. Repair it
  on the Voices page."): Vox does not start it again until it is repaired, reinstalled or Vox restarts.
- A helper program that is missing marks the add-on as incomplete. One the system refuses to start gives "Windows
  blocked <name> from starting." (or "Your system blocked…" on Linux).
- An idle helper is quit after `idle_exit_sec`; the next use starts it again.
