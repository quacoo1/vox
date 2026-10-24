# Vox port spec: TTS subsystem (ElevenLabs client, text processing, TTS engine)

Source of truth: `C:/Users/Kwaku/Desktop/Desktop Widget/src/main/tts/` (5 files, all read in full):

| File | Lines | Role |
|---|---|---|
| `src/main/tts/elevenlabs.ts` | 666 | ElevenLabs REST client: transport, error classification, voices/models/account, synthesis with timestamps, key validation, TTL caches |
| `src/main/tts/text.ts` | 411 | Pure text utilities: `cleanText` (normalize selected text for speech), sentence splitting, `chunkText` |
| `src/main/tts/engine.ts` | 436 | `TtsEngine`: turns cleaned text into an ordered stream of synthesized chunks, with bounded look-ahead (credit protection), retries, cancellation, cached replay |
| `src/main/tts/mock.ts` | 49 | Offline synthesizer (quiet tone WAV and an evenly spaced fake alignment) |
| `src/main/tts/wav.ts` | 69 | 16-bit PCM mono WAV encoder and RIFF header reader |

Types and constants come from `src/shared/types.ts`, `src/shared/constants.ts` and `src/shared/defaults.ts` (summarized in §1). Callers are cited where they define the contract (`src/main/reader.ts`, `src/main/ipc.ts`, `src/main/log.ts`, `src/main/services/audio-cache.ts`, `src/renderer/src/widget/timing.ts`).

Every "Verified" table and example in this document was produced by running the original TypeScript (bundled with esbuild, Node 24.20, the ICU that ships with it) and can be used as a golden test suite. A second reader re-ran all of them and added the edge cases marked as such.

Importers of this subsystem (everything else is internal): `index.ts:444` builds `new ElevenLabsClient(() => settingsStore.getApiKey())`; `reader.ts` uses `TtsEngine`, `cleanText` and `MESSAGES`; `ipc.ts` uses `validateApiKey` and the client's `listVoices` / `listModels` / `getAccount`. Nothing else imports `text.ts`, `mock.ts` or `wav.ts`.

> **Units warning (read first).** All string lengths, offsets and limits in the original are **UTF-16 code units** (JavaScript `string.length` / indices). That applies to the chunk limits (220/900), the context windows (300), the synthesis timeout (50 ms per char), the billing fallback, duration estimates (15 chars/s), `textStart`/`textEnd` offsets sent to the player, and the mock alignment (one entry per UTF-16 unit). ElevenLabs itself counts **code points** in its alignment. The Rust port must pick one unit and use it everywhere: player, cache, history and alignment mapping. See Open Questions in the return summary; the recommended default is Unicode scalar values (`char` indices), which makes ElevenLabs alignment 1:1 with the text more often. Section 8 lists every place where JS semantics differ from Rust defaults.

---

## 1. Shared data model used by this subsystem

### 1.1 Constants (`src/shared/constants.ts:6-9,17`)

| Name | Value | Notes |
|---|---|---|
| `ELEVENLABS_BASE_URL` | `https://api.elevenlabs.io` | All paths are resolved against this |
| `ELEVENLABS_DEFAULT_VOICE_ID` | `JBFqnCBsd6RMkjVDRZzb` | "George", a premade voice |
| `ELEVENLABS_DEFAULT_VOICE_NAME` | `George` | |
| `ELEVENLABS_DEFAULT_MODEL_ID` | `eleven_flash_v2_5` | |
| `ESTIMATED_CHARS_PER_SECOND` | `15` | Rough speech rate for durations before audio exists |

### 1.2 Settings consumed (`src/shared/types.ts:11,25-31,41-123`; defaults `src/shared/defaults.ts:14-28,72-76`)

| Field | Type | Default | Used by |
|---|---|---|---|
| `modelId` | string | `eleven_flash_v2_5` | request `model_id` |
| `outputFormat` | `'mp3_44100_128' \| 'mp3_44100_64' \| 'mp3_22050_32'` | `mp3_44100_128` | request query `output_format`, MIME, MP3 duration |
| `voice.stability` | 0..1 | 0.5 | `voice_settings.stability` |
| `voice.similarityBoost` | 0..1 | 0.75 | `voice_settings.similarity_boost` |
| `voice.style` | 0..1 | 0 | `voice_settings.style` |
| `voice.useSpeakerBoost` | bool | true | `voice_settings.use_speaker_boost` |
| `voice.speed` | 0.7..1.2 (server-side speed) | 1 | `voice_settings.speed` (clamped to 0.7..1.2 in the request), duration estimates and mock length (these use the **unclamped** value, only replaced by 1 when it is not finite or ≤ 0, so an out-of-range hand-edited 2.0 makes estimates assume 2× while the server speaks at 1.2×) |
| `languageCode` | ISO 639-1, `''` = auto-detect | `''` | `language_code` (omitted when empty) |
| `playbackRate` | 0.5..3 (client-side) | 1 | only forwarded to the player in session start |
| `volume` | 0..1 | 1 | only forwarded |
| `widget.highlightWords` | bool | true | only forwarded |
| `text.skipUrls` | bool | true | `cleanText` |
| `text.skipEmails` | bool | false | `cleanText` |
| `text.maxChars` | int | 20000 | caller-side truncation (not used inside `cleanText`) |

`voiceId` comes from the read request (a replay re-synthesizes with the original read's voice), not necessarily from settings.

### 1.3 Domain types

```text
VoiceSettings { stability: f64, similarityBoost: f64, style: f64, useSpeakerBoost: bool, speed: f64 }

Alignment {               // character timing for ONE chunk; seconds relative to that chunk's audio start
  chars:  Vec<String>     // one entry per character as ElevenLabs counts them (code points); mock: per UTF-16 unit
  starts: Vec<f64>        // same length as chars
  ends:   Vec<f64>        // same length as chars
}

Voice   { voiceId, name, category?: String, description?: String, previewUrl?: String, labels: Map<String,String> }
TtsModel{ modelId, name, description?, languages: Vec<{id, name}>, maxChars?: f64, costFactor?: f64 }
AccountInfo { tier: String, characterCount: f64, characterLimit: f64, nextResetUnix?: f64, status?: String }

ReadErrorCode = 'no-api-key' | 'unauthorized' | 'quota' | 'rate-limit' | 'network'
              | 'empty-text' | 'no-selection' | 'too-long' | 'not-cached' | 'unknown'
```

Engine → player messages (`src/shared/types.ts:197-237`):

```text
TtsSessionStart { sessionId, readId, text /* full cleaned text */, title, voiceName, sourceApp?,
                  estimatedDurationSec, chunkCount, rate, volume, highlightWords, fromCache }
TtsChunk        { sessionId, index /* 0,1,2… in order */, textStart, textEnd /* exclusive, offsets into session text */,
                  audio: bytes, mime /* 'audio/mpeg' | 'audio/wav' */, alignment: Alignment | null, durationSec }
TtsSessionDone  { sessionId, totalChunks }
TtsSessionError { sessionId, message, code: ReadErrorCode, fatal: bool /* true = session aborted;
                  false = a chunk failed but playback of what arrived may continue */ }
```

Cache record (`src/main/services/audio-cache.ts:8-16`): `CachedChunk { index, textStart, textEnd, mime, alignment, durationSec, audio }`.

---

## 2. ElevenLabs client (`elevenlabs.ts`)

### 2.1 API facts the client relies on (`elevenlabs.ts:1-7`)

Verified by the original author against the public API reference and live error responses:

- Auth header: `xi-api-key: <key>`.
- Error bodies: `{ "detail": { "type", "code", "message", "status", "request_id", "param" } }`, where `status` is the legacy specific code (for example HTTP 401 has `code: "unauthorized"`, `status: "invalid_api_key"`). Validation errors (HTTP 422) are FastAPI style: `{ "detail": [ { "loc": [...], "msg": "...", "type": "..." } ] }`.
- `POST /v1/text-to-speech/{voice_id}/with-timestamps` → `{ audio_base64, alignment, normalized_alignment }` (non-streaming JSON).
- `GET /v2/voices` (`page_size` ≤ 100, `next_page_token`, `has_more`); `GET /v1/voices` (legacy; also works **without** a key, returning premade voices).
- `GET /v1/models`, `GET /v1/user/subscription`.
- **The API key never appears in errors or logs.** Keep that invariant (mark the header value sensitive in Rust).

The **streaming** endpoints are NOT used. Fast first audio comes from a small first chunk (§3.10) instead.

### 2.2 Constants (`elevenlabs.ts:20-30`)

| Name | Value | Meaning |
|---|---|---|
| `SYNTH_TIMEOUT_MS` | 30 000 | Minimum synthesis request timeout |
| `SYNTH_MS_PER_CHAR` | 50 | Synthesis timeout = `max(30 000, text.len × 50)` ms, so 30 s up to 600 chars and 45 s for a 900-char chunk. Reason (code comment): long chunks on slow models (`eleven_multilingual_v2`, v3) can take more than 30 s, and **a timed-out request may still be billed**, so headroom is given instead of relying on retries |
| `LIST_TIMEOUT_MS` | 15 000 | Timeout for voices/models/subscription requests (per page) |
| `VOICES_TTL_MS` | 600 000 (10 min) | Voices cache |
| `MODELS_TTL_MS` | 3 600 000 (1 h) | Models cache |
| `ACCOUNT_TTL_MS` | 60 000 (60 s) | Subscription cache |
| `MAX_VOICE_PAGES` | 25 | Max `/v2/voices` pages (25 × 100 = 2 500 voices) |
| `NO_CONTEXT_MODELS` | regex `^eleven_v3` | Models for which `previous_text`/`next_text` are never sent (request stitching is documented as unavailable for `eleven_v3`) |

### 2.3 Transport: `apiRequest(key | null, path, opts)` (`elevenlabs.ts:232-276`)

Options: `method` (GET default, or POST), `query` map, `body` (JSON value), `timeoutMs`, optional caller cancellation signal.

1. If the caller signal is **already** aborted: fail immediately with the cancelled error (`code: unknown`, message `Cancelled`, `aborted: true`). No request is sent.
2. URL = `new URL(path, base)`. Every `path` starts with `/`, so it replaces the base path. For each query entry whose value is not `undefined`, set `name=String(value)`. Numbers become decimal strings and booleans become `true`/`false`. Use proper URL encoding (`URLSearchParams` semantics: `application/x-www-form-urlencoded`, space → `+`, and everything except `A-Za-z0-9*-._` percent-encoded). Verified: `next_page_token = "a+b/c= d"` → `next_page_token=a%2Bb%2Fc%3D+d`. (WHATWG URL parsing would also collapse `.`/`..` path segments, which matters only if a voice id were `.` or `..`; ElevenLabs voice ids are alphanumeric, e.g. the default `JBFqnCBsd6RMkjVDRZzb`.)
3. Headers:
   - always `accept: application/json`
   - `xi-api-key: <key>` only when a key is given (the anonymous `/v1/voices` call sends no key)
   - `content-type: application/json` only when there is a body
   - body = `JSON.stringify(body)`
   - No `user-agent` is set by the code. Captured on the wire from Node 24's `fetch`, the request also carries `user-agent: node`, `accept-language: *`, `sec-fetch-mode: cors`, `accept-encoding: gzip, deflate`, `connection: keep-alive` and `content-length`. The code does not depend on any of them. The port should send its own `user-agent` (for example `Vox/<version>`).
4. A timeout of `timeoutMs` is combined with the caller's signal. **The timeout bounds the whole exchange including reading the full response body** (`res.text()` runs under the same signal). Each call of `apiRequest` gets a fresh timeout, so every parameter-fallback attempt (§2.6.1) and every engine retry (§4.5) has its own full budget.
5. Any failure while sending or reading the body maps through `transportError` (§2.4.5). **The body is read before the status is checked**, so a 4xx/5xx whose body read fails or times out becomes a `network` error (timeout / network message), not an HTTP error.
6. Non-2xx status: build the error via `errorFromResponse(status, rawBodyText, headers)` (§2.4).
7. 2xx (200–299): `JSON.parse(raw)`; on parse failure (including an empty body), the `badResponse` error (`code: unknown`, message `ElevenLabs sent a response that could not be read`, **retryable: true**).
8. Returns `{ data: parsed JSON, headers }`.

Header reads (`headers.get(name)`) are case-insensitive and return `null` when absent. When a header occurs several times, fetch joins the values with `", "`, so a numeric parse of e.g. `"3, 3"` fails and the header counts as absent.

Implicit platform behavior to reproduce: Node's `fetch` (undici) follows redirects (up to 20), transparently decompresses gzip/deflate (and br if a server sends it unasked), keeps connections alive, uses Node's bundled Mozilla CA list (not the Windows store), and **ignores the Windows system proxy**.

### 2.4 Error model

#### 2.4.1 `ElevenLabsError` fields (`elevenlabs.ts:52-95`)

| Field | Type | Meaning |
|---|---|---|
| `code` | `ReadErrorCode` | Category used by callers and the UI |
| `message` | string | **Already user-facing.** Callers show it verbatim |
| `status` | HTTP status? | |
| `apiCode` | string? | `detail.status ?? detail.code` (original case), e.g. `invalid_api_key`, `quota_exceeded`, `voice_not_found` |
| `apiMessage` | string? | ElevenLabs' own message, unmodified |
| `param` | string? | Offending parameter (from `detail.param` or the 422 `loc` path) |
| `requestId` | string? | |
| `retryable` | bool (default false) | The engine retries only these |
| `missingPermission` | bool (default false) | The key is valid but lacks the scope for this endpoint |
| `aborted` | bool (default false) | Cancelled by the caller: never show it as an error |
| `retryAfterSec` | number? | From the `retry-after` header |
| `cause` | any | Underlying error (debug only) |

Who reads which field: the read controller (`reader.ts:155-166,664-676`) uses only `code`, `message`, `missingPermission` and `status` (logged as `read failed: <code> (HTTP <status>)`). The engine uses `retryable`, `aborted` and `retryAfterSec`. The client itself uses `status`, `param`, `apiMessage` and `apiCode` (parameter fallback §2.6.1, key validation §2.11). `requestId` is never read anywhere; it is debug metadata only.

`toElevenLabsError(err)` (`elevenlabs.ts:97-104`): an `ElevenLabsError` passes through. An error named `AbortError` (for example an aborted abortable sleep) becomes `code: unknown`, message `Cancelled`, `aborted: true`. Any other `Error` becomes `code: unknown`, message = its message, or `Something went wrong` when that is empty, not retryable. A thrown non-`Error` value uses its string form as the message.

Two prebuilt errors:
- `noKeyError` → `code: 'no-api-key'`, message `MESSAGES.noKey`.
- `badResponse` → `code: 'unknown'`, message `MESSAGES.badResponse`, `retryable: true`.

#### 2.4.2 User-facing messages (`elevenlabs.ts:32-48`) (exact strings; `—` is U+2014 EM DASH; apostrophes are ASCII `'`)

| Key | Text |
|---|---|
| `noKey` | `Add your ElevenLabs API key in Settings to start reading` |
| `badKey` | `Your ElevenLabs key was rejected — update it in Settings` |
| `quota` | `You're out of ElevenLabs credits — top up your plan or wait for the monthly reset` |
| `rateLimit` | `ElevenLabs is busy right now — try again in a moment` |
| `concurrency` | `Too many requests at once for your ElevenLabs plan — try again in a moment` |
| `network` | `Couldn't reach ElevenLabs — check your internet connection` |
| `timeout` | `ElevenLabs took too long to respond — try again` |
| `server` | `ElevenLabs is having trouble right now — try again in a moment` |
| `voiceMissing` | `That voice is no longer available — pick another one in Voices` |
| `voiceDenied` | `That voice isn't available on your ElevenLabs plan — pick another one in Voices` |
| `model` | `The selected model isn't available for your key — choose another in Settings` |
| `tooLong` | `This passage is too long for the selected model` |
| `badResponse` | `ElevenLabs sent a response that could not be read` |
| `noAudio` | `ElevenLabs returned no audio` |
| `cancelled` | `Cancelled` |

`reader.ts:156-166` shows the engine's message is preferred over generic per-code text. `MESSAGES.badKey` identity is used to decide whether a toast offers "open Settings" (only for `no-api-key`, `quota`, or `unauthorized && !missingPermission && message == badKey`). Keep these strings as constants that can be compared.

#### 2.4.3 `parseErrorBody(raw)` (`elevenlabs.ts:110-148`)

Output: `{ type?, code?, status?, message?, requestId?, param? }`. Helper `str(x)` = x if it is a **non-empty** string, else none.

1. Try to parse `raw` as JSON. On failure: `text = raw.trim()`. If `text` is non-empty and does **not** start with `<` (an HTML error page), return `{ message: first 300 chars of text }`. Otherwise `{}`. **This is the only place the raw message is capped**; JSON messages are taken at full length.
2. `detail` = `json.detail` if `json` is an object (not array, not null) that **has the key** `detail` (even when its value is `null`), else `json` itself. So a top-level object without `detail`, like `{"status":"voice_not_found","message":"no voice"}`, is read as the detail.
3. If `detail` is a string → `{ message: detail }`, untrimmed and uncapped. A JSON body that is just a string (`"just a string"`) lands here too. `{"detail":""}` gives `message: ""`. Every consumer treats `""` as "no message" (falsy), but `apiMessage` is then `""` rather than absent.
4. If `detail` is an array (FastAPI validation):
   - `items` = its elements that are objects.
   - `message` = the non-empty `msg` strings of `items` joined with `"; "`, or none if that is empty.
   - `param` = if `items[0].loc` is an array: its elements stringified with JS `String()` and joined with `"."` (e.g. `["body","language_code"]` → `body.language_code`; numbers → decimal; `null` → `"null"`; an empty `loc` → `""`, an empty-string param, not absent).
   - `type` = `"validation_error"`. No `code`/`status`, so `has(...)` never matches a validation error.
5. If `detail` is not an object (`null`, number, boolean) → `{}`.
6. Else: `type = str(detail.type)`, `code = str(detail.code)`, `status = str(detail.status)`, `message = str(detail.message)`, `requestId = str(detail.request_id)`, `param = str(detail.param)`.

#### 2.4.4 `errorFromResponse(status, raw, headers)` (`elevenlabs.ts:150-222`)

Setup:
- `ids` = `[body.status, body.code]`, the non-empty ones, **lower-cased**. `has(names…)` = any name is in `ids`.
- `message` = `body.message`.
- `retryAfterSec` = `Number(header "retry-after")` when finite and `> 0`, else none. An absent header, `""`, `"0"` or an HTTP-date all give none. Fractional values like `"1.5"` are accepted. JS `Number()` trims whitespace.
- `requestId` = `body.requestId` ?? header `request-id` ?? header `x-trace-id` (the body's `request_id` wins over the headers).
- `apiCode` = `body.status ?? body.code` (original case); `apiMessage` = `message`; `param` = `body.param`.

Case rules: the `ids` list is lower-cased, so `QUOTA_EXCEEDED` matches `quota_exceeded`. But `body.type` is compared **exactly** (`payment_required`, `authentication_error`, `rate_limit_error`, case-sensitive). The permission regex is case-insensitive.

Classification. **Order matters; the first match wins:**

| # | Condition | `code` | Message | Retryable | Extra |
|---|---|---|---|---|---|
| 1 | `has('quota_exceeded','insufficient_credits','payment_required')` OR `status == 402` OR `body.type == 'payment_required'` | `quota` | `MESSAGES.quota` | no | Checked before 401 on purpose: ElevenLabs reports `quota_exceeded` with **HTTP 401** |
| 2 | `has('missing_permissions','insufficient_permissions')` OR `message` matches `/missing (?:the )?permission/i` | `unauthorized` | `permissionMessage(message)` | no | `missingPermission: true` |
| 3 | `status == 401` OR `body.type == 'authentication_error'` | `unauthorized` | if `has(INVALID_KEY_CODES…)` or no message (absent or `""`) → `MESSAGES.badKey`; else `` `ElevenLabs: ${message}` `` (**not** truncated: a 400-char message gives a 412-char text) | no | e.g. a free-tier "Unusual activity detected…" 401 shows ElevenLabs' own text |
| 4 | `status == 429` OR `body.type == 'rate_limit_error'` OR `has('too_many_concurrent_requests','concurrent_limit_exceeded','rate_limit_exceeded','system_busy')` | `rate-limit` | `MESSAGES.concurrency` if `has('too_many_concurrent_requests','concurrent_limit_exceeded')`, else `MESSAGES.rateLimit` | **yes** | `system_busy` is treated as rate limit even on 5xx |
| 5 | `has('voice_not_found')` | `unknown` | `MESSAGES.voiceMissing` | no | |
| 6 | `has('voice_access_denied')` | `unknown` | `MESSAGES.voiceDenied` | no | |
| 7 | `has('model_not_found','model_access_denied','unsupported_model')` | `unknown` | `MESSAGES.model` | no | |
| 8 | `has('text_too_long','max_character_limit_exceeded')` | `too-long` | `MESSAGES.tooLong` | no | `max_character_limit_exceeded` means one request's text is over the model's or account's per-request limit, **not** quota |
| 9 | `status >= 500` | `unknown` | `MESSAGES.server` | **yes** | |
| 10 | otherwise | `unknown` | `` `ElevenLabs: ${message}` `` if message, else `` `ElevenLabs returned an error (HTTP ${status})` ``; **truncated to 300 chars** (UTF-16 units, so a surrogate pair can be split) | no | Includes **HTTP 403** without a recognized code: it is `unknown`, not `unauthorized` |

`INVALID_KEY_CODES` (`elevenlabs.ts:150-156`): `invalid_api_key`, `missing_api_key`, `needs_authorization`, `invalid_authorization_header`, `unauthorized`.

`permissionMessage(apiMessage)` (`elevenlabs.ts:158-168`):
```text
scope = first capture of /permission\s+["'`]?([a-z_]+)/i in apiMessage (or none)
names = { text_to_speech: "Text to Speech", voices_read: "Voices (read)", user_read: "User (read)", models_read: "Models (read)" }
what  = scope ? (names[scope] (case-sensitive lookup) ?? scope with "_" → " ") : "this feature"
→ "Your ElevenLabs key isn't allowed to use ${what} — enable it for the key on elevenlabs.io, or use another key"
```
Example: `"The API key you used is missing the permission text_to_speech to execute this operation."` → `Your ElevenLabs key isn't allowed to use Text to Speech — enable it for the key on elevenlabs.io, or use another key`.

Quirks of the scope regex (keep them for parity, or improve deliberately): the regex is case-insensitive but the name lookup is not, so `TEXT_TO_SPEECH` → `TEXT TO SPEECH`. It takes the first word after `permission` + whitespace, so `"missing permission to do x"` → `…allowed to use to — …`. `"permissions x"` does not match there, because `permission` must be followed directly by whitespace.

Verified outputs from the original code:

| HTTP | Body | → code / message / retryable |
|---|---|---|
| 401 | `{"detail":{"status":"quota_exceeded","message":"This request exceeds your quota"}}` | `quota` / quota text / no |
| 401 | `{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}` | `unauthorized` / `badKey` / no |
| 401 | `{"detail":{"status":"detected_unusual_activity","message":"Unusual activity detected. Free Tier usage disabled."}}` | `unauthorized` / `ElevenLabs: Unusual activity detected. Free Tier usage disabled.` |
| 429 + `retry-after: 3` | `{"detail":{"status":"too_many_concurrent_requests",…}}` | `rate-limit` / concurrency text / yes / `retryAfterSec=3` |
| 422 | `{"detail":[{"loc":["body","language_code"],"msg":"bad lang","type":"value_error"}]}` | `unknown` / `ElevenLabs: bad lang` / no / `param=body.language_code` |
| 503 | `<html>bad gateway</html>` | `unknown` / server text / yes |
| 418 | `plain teapot text` | `unknown` / `ElevenLabs: plain teapot text` |
| 403 | `{"detail":{"message":"Forbidden"}}` | `unknown` / `ElevenLabs: Forbidden` / no *(second reader)* |
| 403 | *(empty body)* | `unknown` / `ElevenLabs returned an error (HTTP 403)` / no |
| 400 | `"just a string"` | `unknown` / `ElevenLabs: just a string` |
| 400 | `{"detail":""}` | `unknown` / `ElevenLabs returned an error (HTTP 400)` / `apiMessage=""` |
| 400 | `{"detail":{"status":"QUOTA_EXCEEDED"}}` | `quota` / quota text / `apiCode=QUOTA_EXCEEDED` (original case) |
| 400 | `{"status":"voice_not_found","message":"no voice"}` (no `detail` wrapper) | `unknown` / `voiceMissing` text |
| 400 | `{"detail":{"status":"max_character_limit_exceeded"}}` | `too-long` / `tooLong` text / no |
| 401 | `{"detail":{"status":"needs_authorization"}}` | `unauthorized` / `badKey` |
| 401 | `{"detail":{"code":"unauthorized","status":"missing_permissions","message":"The API key you used is missing the permission user_read to execute this operation."}}` | `unauthorized` / `Your ElevenLabs key isn't allowed to use User (read) — …` / `missingPermission=true` (row 2 wins over row 3) |
| 500 + `retry-after: 1.5` | `{"detail":{"status":"system_busy"}}` | `rate-limit` / `rateLimit` text / yes / `retryAfterSec=1.5` |
| 429 + `retry-after: Wed, 21 Oct 2015 07:28:00 GMT` | `{}` | `rate-limit` / `rateLimit` text / yes / no `retryAfterSec` |
| 400 | `{"detail":[{"loc":[],"msg":""}]}` | `unknown` / `ElevenLabs returned an error (HTTP 400)` / `param=""` |
| 400 | 400 × `x` (not JSON) | `unknown` / `ElevenLabs: xxx…` capped at 300 chars total |
| 401 | `{"detail":{"request_id":"body-id"}}` + header `request-id: hdr` | `requestId=body-id` |
| 429 + `retry-after:  0x10 ` | `{}` | `retryAfterSec=16` (JS `Number()` trims and accepts hex) |

#### 2.4.5 Transport errors (`elevenlabs.ts:224-228`)

| Situation (checked in this order) | `code` | Message | Retryable | `aborted` |
|---|---|---|---|---|
| Caller's cancel signal fired | `unknown` | `Cancelled` | no | **yes** |
| The request timeout fired | `network` | `MESSAGES.timeout` | **yes** | no |
| Anything else (DNS, TLS, connection reset, body read error, including a body read error on a non-2xx response) | `network` | `MESSAGES.network` | **yes** | no |

These errors carry no `status`, so they never trigger the parameter fallback (§2.6.1) or the v2 → v1 voices fallback (§2.7).

### 2.5 Response mapping helpers (`elevenlabs.ts:280-406`)

Tolerant parsing: helpers accept any JSON and silently drop wrong-typed fields.
- `isRecord(x)`: a non-null object, not an array.
- `str(x)`: a non-empty string, else none.
- `num(x)`: a finite number, else none (strings are NOT coerced).
- `clamp(v, lo, hi, fallback)`: if `v` is finite → `min(hi, max(lo, v))`, else `fallback`.

#### `mapVoice(raw)` → `Voice | null` (`elevenlabs.ts:296-323`)
1. Not an object, or no non-empty string `voice_id` → `null` (dropped).
2. `category = str(raw.category)`.
3. `name = str(raw.name)?.trim()`; if that is empty or missing → `voiceId`.
4. `labels`: if `raw.labels` is an object, for each entry: a string with non-empty trim → trimmed value; a number or boolean → its JS string form (`3` → `"3"`, `1.5` → `"1.5"`, `true` → `"true"`); anything else (whitespace-only strings, null, arrays, objects) skipped.
5. **Premade name split:** only when `category == "premade"` (exact, case-sensitive): find the **first** `" - "` in the already-trimmed `name`. If it is at index > 0 (premade names look like `George - Warm, Captivating Storyteller`): `tagline = after " - "`, trimmed. If the tagline is non-empty and `labels.tagline` is not already set, set it. `name = before " - "`, trimmed. Because the name was trimmed first, `" - "` can never be its final three characters, so in practice the tagline is never empty. `"A - "` → trimmed `"A -"` → no split, and `"  - x"` → `"- x"` (dash at index 0) → no split. Non-premade names are never split (`"A - B"` with category `cloned` stays).
6. Output `{ voiceId, name, labels }`, plus `category` if present, `description` = trimmed non-empty `raw.description`, and `previewUrl` = non-empty `raw.preview_url` (untrimmed).

Example: `{voice_id:"v1", name:"George - Warm, Captivating Storyteller", category:"premade", labels:{accent:" british ", age:"", n:3}}` → `{"voiceId":"v1","name":"George","labels":{"accent":"british","n":"3","tagline":"Warm, Captivating Storyteller"},"category":"premade"}`.

#### `sortVoices(voices)` (`elevenlabs.ts:326-336`)
1. De-duplicate by `voiceId`, keeping the **first** occurrence.
2. Sort (stable) by rank and then name:
   - rank 0: category `cloned`, `generated` or `professional` (the user's own voices)
   - rank 1: any other category, including none (other library voices)
   - rank 2: `premade`
   - name: `a.name.localeCompare(b.name, <default locale>, { sensitivity: 'base', numeric: true })`, i.e. ICU collation at primary strength (case- and accent-insensitive) with numeric ordering of digit runs.

Example: `[zed(premade), "Voice 10"(none), "voice 2"(none), Mine(cloned), dup of zed]` → `Mine, voice 2, Voice 10, zed`. Verified with accents: `Émile`(generated) and `emile`(professional) compare equal at primary strength, so they keep their input order: `Émile, emile, Mine, voice 2, Voice 10, zed`.

Cross-reference: the Voices page re-sorts this list in the UI (`src/renderer/src/app/settings/logic.ts:349-358`) with a different order: `cloned` 0, `professional` 1, `generated` 2, any other or no category 2.5, `premade` 3 (category lower-cased and trimmed), then plain `localeCompare` with no options. So the main-process order is what other consumers see (for example the settings voice picker), not the Voices grid. The settings UI spec owns that second sort.

#### `mapModel(raw)` → `TtsModel | null` (`elevenlabs.ts:343-359`)
- Dropped unless `raw.can_do_text_to_speech === true` (strict boolean) and `model_id` is a non-empty string.
- `languages`: `raw.languages` if it is an array, else `[]`. Keep object elements only, map each to `{ id: str(language_id) ?? "", name: str(name) ?? str(language_id) ?? "" }`, and drop those with an empty `id`. Verified: `[{language_id:"en",name:"English"},{language_id:""},{name:"x"},"str",{language_id:"fr"}]` → `[{id:"en",name:"English"},{id:"fr",name:"fr"}]`.
- `name = str(raw.name) ?? modelId` (not trimmed). `description` = trimmed non-empty.
- `maxChars = num(maximum_text_length_per_request) ?? num(max_characters_request_subscribed_user)`. `0` is a valid finite number and is kept.
- `costFactor = num(model_rates.character_cost_multiplier)` (when `model_rates` is an object).
- The list keeps ElevenLabs' order (no sorting).

#### `mapAccount(raw)` → `AccountInfo` (`elevenlabs.ts:361-373`)
- Not an object → `badResponse`.
- `tier = str(raw.tier) ?? "unknown"`, `characterCount = num(character_count) ?? 0`, `characterLimit = num(character_limit) ?? 0`, optional `nextResetUnix = num(next_character_count_reset_unix)`, optional `status = str(status)`. Verified: `{tier:"", character_count:"5", character_limit:100, …}` → `tier:"unknown", characterCount:0` (a numeric string is not coerced).

#### `mapAlignment(raw)` → `Alignment | null` (`elevenlabs.ts:375-384`)
- Requires an object with arrays `characters`, `character_start_times_seconds` and `character_end_times_seconds`.
- `n = characters.length`. Requires `n > 0` and all three lengths equal.
- Every start and end must be a finite number. Otherwise → `null` (the chunk is still delivered, without alignment). Times are not checked for being ascending or non-negative.
- `chars = characters.map(String)` (any JSON value is stringified JS-style: `1` → `"1"`, `null` → `"null"`). `starts`/`ends` are passed through unchanged.

#### MIME and bitrate (`elevenlabs.ts:386-399`)
`mimeForFormat(fmt)` by prefix: `mp3` → `audio/mpeg`; `wav` → `audio/wav`; `opus` → `audio/ogg`; `pcm` → `audio/pcm`; `ulaw` or `alaw` → `audio/basic`; else `application/octet-stream`.
`mp3Kbps(fmt)`: `/^mp3_\d+_(\d+)$/` (ASCII digits) → the bitrate number (`mp3_44100_128` → 128, `mp3_22050_32` → 32), else null. A bitrate of `0` is treated as "unknown" by its caller (falsy). ElevenLabs MP3 is **constant bitrate**, which lets duration be computed from byte size (§4.8).

`intHeader(headers, name)` (`elevenlabs.ts:401-406`): absent or blank → none. Otherwise `Number(v)` if finite and `>= 0` (fractions allowed despite the name).

### 2.6 Synthesis: `POST /v1/text-to-speech/{voice_id}/with-timestamps` (`elevenlabs.ts:410-467`)

Request input (`SynthesisRequest`): `text, voiceId, modelId, outputFormat, voiceSettings, previousText?, nextText?, languageCode?`.

HTTP:
```http
POST https://api.elevenlabs.io/v1/text-to-speech/{urlencode(voiceId)}/with-timestamps?output_format={outputFormat}
accept: application/json
content-type: application/json
xi-api-key: {key}
```
`urlencode` = `encodeURIComponent` (percent-encodes everything except `A-Za-z0-9-_.!~*'()`).

Body:
```json
{
  "text": "<chunk text>",
  "model_id": "<modelId>",
  "voice_settings": {
    "stability":        clamp(stability, 0, 1, fallback 0.5),
    "similarity_boost": clamp(similarityBoost, 0, 1, fallback 0.75),
    "style":            clamp(style, 0, 1, fallback 0),
    "use_speaker_boost": useSpeakerBoost,
    "speed":            clamp(speed, 0.7, 1.2, fallback 1)
  },
  "previous_text": "<only if non-empty>",
  "next_text":     "<only if non-empty>",
  "language_code": "<only if non-empty>"
}
```
Key order is not significant (the original emits `text, model_id, voice_settings{stability, similarity_boost, style, use_speaker_boost, speed}, previous_text, next_text, language_code`). Not sent: `seed`, `apply_text_normalization`, `enable_logging`, `optimize_streaming_latency`, `pronunciation_dictionary_locators`. `use_speaker_boost` is passed through unvalidated (in JS an `undefined` value would drop the key; settings validation elsewhere guarantees a boolean). The optional fields are omitted when empty: `""` is falsy.

Verified request (second reader, fetch mocked): `voiceId = "a b/c"`, voice settings `{stability: NaN, similarityBoost: 2, style: -1, useSpeakerBoost: true, speed: 5}` →
```http
POST https://api.elevenlabs.io/v1/text-to-speech/a%20b%2Fc/with-timestamps?output_format=mp3_44100_128
accept: application/json
xi-api-key: KEY                       (the stored "  KEY  " is trimmed)
content-type: application/json

{"text":"Hello","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":1,"style":0,"use_speaker_boost":true,"speed":1.2},"previous_text":"prev","next_text":"next","language_code":"en"}
```
(In the path a space is `%20`; in the query string it would be `+`.)

Timeout: `max(30 000, text.length × 50)` ms, per HTTP request.

Response parsing:
1. JSON must be an object, else `badResponse` (retryable).
2. `audio_base64`: a non-empty string is decoded as base64. Node's decoder is **lenient**. Behavior observed on Node 24 (keep it, since ElevenLabs output is never re-validated):
   - whitespace and any character outside the base64 alphabets are skipped (`"QU JD\n"` → `ABC`, `"Q!U#JD"` → `ABC`);
   - both alphabets are accepted, even mixed in one string (`-` ≡ `+`, `_` ≡ `/`);
   - decoding **stops at the first `=`** (`"QUJD=QUJD"` → `ABC`);
   - missing padding is fine (`"QUJ"` → `AB`) and a dangling single sextet is dropped (`"QUJDQ"` → `ABC`); non-zero trailing bits are accepted.

   Missing, empty, or decoding to 0 bytes (`"!!!!"`) → error `code: unknown`, message `ElevenLabs returned no audio`, **retryable: true**.

   Rust recipe: map `-`→`+` and `_`→`/`, keep only `[A-Za-z0-9+/]`, cut at the first `=`, drop a final group of length 1, then decode with a `GeneralPurpose` engine configured with `DecodePaddingMode::Indifferent` and `decode_allow_trailing_bits(true)`.
3. `alignment = mapAlignment(data.alignment) ?? mapAlignment(data.normalized_alignment)`. Prefer the raw-text alignment; normalized (numbers spelled out, etc.) is only the fallback.
4. `mime = mimeForFormat(outputFormat)` (the requested format, not the response content type).
5. `requestId` = header `request-id` (only this header here). Not used by the engine or anything downstream.
6. `characterCost` = `intHeader('x-character-count') ?? intHeader('character-cost')`. The docs name this header both ways; accept either. It is the characters or credits charged. Verified: `x-character-count: 42` → `characterCost = 42`. The engine sums it into `charactersBilled` (§4.4).

Response JSON shape:
```json
{
  "audio_base64": "<base64 mp3>",
  "alignment": {
    "characters": ["H","e","l","l","o"," ", "…"],
    "character_start_times_seconds": [0.0, 0.07, "…"],
    "character_end_times_seconds":   [0.07, 0.12, "…"]
  },
  "normalized_alignment": { "…same shape…" }
}
```

#### 2.6.1 `client.synthesize(req, cancel)` (`elevenlabs.ts:601-621`)
1. No key (after trimming) → `noKeyError`.
2. If `modelId` matches `^eleven_v3`, drop `previousText` and `nextText` before the first attempt.
3. Loop: call `synthesizeOnce`. On error `e`: if `attempt < 2`, compute `reduced = withoutRejectedParam(current, e)`. If there is no reduced request, throw `e`; otherwise retry with it at once (no delay). So there are **at most 3 HTTP requests**: original, then without `language_code`, then without context (in whichever order the API rejects them). The doc comment on `synthesize` says "Retries once", but the code allows two reductions.
4. **No** rate-limit or server retries here. Those belong to the engine (§4.5).
5. **Nothing is remembered.** The reduced request lives only inside this call. Every engine retry and every later chunk starts again from the full request, so a model that rejects `language_code` costs one extra (rejected) HTTP request per chunk and per retry.

Verified sequence (second reader): 422 `{"detail":[{"loc":["body","language_code"],"msg":"unsupported"}]}` → resend without `language_code`; 400 `{"detail":{"status":"invalid_request","message":"Request stitching is not supported"}}` → resend without `previous_text`/`next_text` (`stitch` matched the message); 200 → success. Three POSTs in all.

`withoutRejectedParam(req, e)` (`elevenlabs.ts:470-478`):
```text
if e.status not in {400, 422}: none
hint = lowercase(`${e.param ?? ""} ${e.apiMessage ?? ""}`)
if req.languageCode set and hint contains "language": same request without languageCode
else if (req.previousText or req.nextText) and hint matches /previous_text|next_text|previous text|next text|stitch/:
     same request without previousText AND nextText
else none
```

### 2.7 Voices: `listVoices({refresh})` (`elevenlabs.ts:520-572`)

Result: all voices available to the key, sorted (§2.5). Cached 10 min per key fingerprint (§2.10).

**With a key:**
1. `fetchVoicesV2(key)`: up to 25 pages of
   `GET /v2/voices?page_size=100&include_total_count=false[&next_page_token=<token>]` (token omitted on the first page; parameter order as shown), each with a 15 s timeout. Every page must be an object with a `voices` array, else `badResponse`. Append the mapped voices. `token = str(data.next_page_token)`. Stop when `data.has_more !== true` (strict boolean) or there is no token.
   - An error on **any** page aborts the whole listing. Voices from earlier pages are discarded (no partial list), and the error goes to step 2.
   - If page 25 still says `has_more: true`, the 2 500 voices collected so far are returned silently, with no error.
2. If v2 fails with `e`:
   - `v2Unavailable` = `e.status` exists, is 4xx, and is **not** 401, 402, 403 or 429.
   - If `!v2Unavailable` or `e.missingPermission` → throw `e`.
   - Else fall back to `GET /v1/voices` with the key (15 s); response `{ voices: [...] }`.
3. `sortVoices(result)`.

**Without a key** (so the Voices page works before onboarding):
- `GET /v1/voices` with no auth header (only `accept`). On error: if `e.code == 'network'` rethrow it (includes timeouts); otherwise throw `noKeyError` (verified: a 401 `needs_authorization` from the anonymous call surfaces as `no-api-key` / `Add your ElevenLabs API key in Settings to start reading`). This result is cached under the fingerprint `"anon"` for 10 min.

Caller context (`ipc.ts:303,331-333`): the IPC layer passes `{refresh: true}` only when the renderer sends exactly `refresh === true`. In demo mode without a stored key it returns built-in demo voices, models and account instead of calling the client (owned by the IPC/demo spec).

### 2.8 Models: `listModels({refresh})` (`elevenlabs.ts:574-583`)
- No key → reject with `noKeyError` (nothing is cached).
- `GET /v1/models` (15 s). The response must be a JSON **array**, else `badResponse`. Map with `mapModel` and drop nulls. Cached 1 h.

### 2.9 Account: `getAccount({refresh})` (`elevenlabs.ts:585-599`)
- No key → `null` (no request, nothing cached).
- `GET /v1/user/subscription` (15 s) → `mapAccount`. If the error has `missingPermission` → `null` (**cached as null for 60 s**; restricted keys are common). Other errors are thrown. Cached 60 s.

### 2.10 Caches: `Memo<T>` and the key fingerprint (`elevenlabs.ts:482-518`)

Fingerprint: `hex(sha256(utf8(key)))[0..16]`, or `"anon"` when there is no key. The cache is keyed by fingerprint, so changing the key naturally misses the cache, and the raw key is never stored as a map key.

`Memo.get(fp, refresh, load)`:
1. If not `refresh`:
   - if the stored entry has the same `fp` and `now − entry.at < ttl` → return the cached value.
   - if a pending load exists for the same `fp` → return that same in-flight future (concurrent callers share one request).
2. Otherwise start a new load with `gen = ++counter`, and make it the pending load (a forced refresh also becomes what later non-refresh callers join).
3. On success: store `{fp, at: now (completion time), value, gen}` **only if** there is no entry or `entry.gen < gen`. A slow older load finishing after a newer one never clobbers it.
4. On settle (success or failure): clear `pending` if it is still this load. **Failures are not cached.**

`clear()` drops the entry and the pending load. A load already in flight still stores its value when it finishes (the entry is empty, so the `gen` check passes). `client.clearCache()` clears all three memos. **It is never called anywhere in the app**; key changes rely on the fingerprint.

Other consequences: a forced refresh for one key replaces `pending`, so later non-refresh callers for that key join the refresh. A load for key A that finishes after a newer load for key B stored its entry is dropped (B's `gen` is higher), even though the keys differ. The TTL is measured from the load's **completion** time.

`key()`: `getApiKey()` trimmed (JS `trim()`); empty → none. The key getter is called on every operation (it reads the current stored key; `index.ts:444` passes `() => settingsStore.getApiKey()`).

### 2.11 Key validation: `validateApiKey(key)` (`elevenlabs.ts:636-665`)

Not cached; uses the candidate key, not the stored one.

| Step / outcome | Result |
|---|---|
| `k = key.trim()` is empty | `{ ok:false, error:"Paste your ElevenLabs API key first" }` |
| `k` contains any whitespace (`/\s/`, the JS set: includes NBSP, U+FEFF, U+2028, etc.) | `{ ok:false, error:"That doesn't look like an API key — it contains spaces" }` |
| `GET /v1/user/subscription` with `k` (15 s) succeeds with a JSON object | `{ ok:true, account: mapAccount(data) }` |
| …succeeds but the JSON is not an object | `mapAccount` throws `badResponse` → falls to "any other error" → `{ ok:false, error:"ElevenLabs sent a response that could not be read" }` |
| error `missingPermission` | `{ ok:true, account:null, warning:"This key can't read your account usage, so credits won't be shown. Reading works as long as it has Text to Speech access." }` |
| error `code == 'quota'` | `{ ok:true, account:null, warning: MESSAGES.quota }` (the key is valid, just out of credits) |
| error `code == 'unauthorized'` | `{ ok:false, error }` where error = `"ElevenLabs didn't accept this key — check that you copied all of it"` if there is no `apiMessage` or `lower(apiCode)` is in `INVALID_KEY_CODES`, else `e.message` |
| any other error (network, timeout, 5xx, 429…) | `{ ok:false, error: e.message }`, e.g. `Couldn't reach ElevenLabs — check your internet connection` |

The checks run in that order (missing permission, then quota, then unauthorized). Validation is never retried and does not touch the client caches, so the account fetched here is not reused by `getAccount`.

Caller context (`ipc.ts:311-324`): a non-string becomes `""`. The key is JS-trimmed, and keys longer than 256 UTF-16 units are rejected before this call with `That doesn't look like an API key` (no em-dash suffix). On `ok`, the trimmed key is stored. The IPC result is `{ ok, error, warning, account: account ?? null, settings }`.

---

## 3. Text processing (`text.ts`)

Pure functions with no I/O. The reading pipeline in the caller (`reader.ts:316-333`, for context):
`preCut(raw, max(maxChars × 4, 200 000))` → `cleanText(text, settings.text)` (if it throws, `raw.trim()` is used) → empty → "Nothing to read" → `truncateText(cleaned, maxChars)` → `engine.start(text)`. `truncateText` and `preCut` belong to the reader spec; `text.ts`'s own `truncateAtBoundary` is **not used** (§3.11).

### 3.1 `cleanText(text, {skipUrls, skipEmails})` (`text.ts:71-97`)

Normalizes selected text for speech. Paragraph breaks are kept as `"\n\n"`; structural line breaks (lists, headings, short title lines) as `"\n"`. Steps, in order:

1. Empty input → `""`.
2. Remove `INVISIBLE` characters.
3. Replace `LINE_BREAKS` with `\n`.
4. Replace U+2029 (PARAGRAPH SEPARATOR) with `\n\n`.
5. Replace `ODD_SPACES` with a single ASCII space (one-for-one, so a tab becomes one space).
6. `joinLines` (§3.3).
7. `stripInlineMarkdown` (§3.4).
8. If `skipUrls`: replace each `URL_RE` match with `HOLE + urlTrailing(match)` (§3.5).
9. If `skipEmails`: replace each `EMAIL_RE` match with `HOLE`.
10. If the text contains `HOLE` (U+E000): `closeHoles` (§3.5). This also triggers if the user's text itself contains U+E000, which is then treated as a removed item (verified: `"a  b"` with both skip options off → `"a b"`).
11. Final collapse: `/[ ]{2,}/g` → `" "`; then `/ *\n */g` → `"\n"`; then `/\n{3,}/g` → `"\n\n"`; then trim (JS `trim()` whitespace set, §8).

`opts` is the whole `settings.text` object; only `skipUrls` and `skipEmails` are read (truncation to `maxChars` is the caller's job).

**Whitespace invariant (useful for the port).** After steps 2–5, the only characters left from the JS `\s` set are U+0020 SPACE and U+000A LF. All the others are removed (U+FEFF) or converted: `\t \v \f`, NBSP, U+1680, U+2000–U+200A, U+202F, U+205F and U+3000 become spaces; `\r`, U+2028, U+2029 and U+0085 become line feeds. So every later `\s`, `\S` or `trim()` in `cleanText`, and in `sentenceSpans`/`chunkText` (whose input is always cleaned text), only ever sees `' '` and `'\n'`. The JS-vs-Rust whitespace differences in §8 therefore matter only in steps 2–5 and in the few places that see raw text (`validateApiKey`, the reader's `raw.trim()` fallback).

**Markdown link URLs are dropped regardless of `skipUrls`**: step 7 turns `[docs](https://x.com)` into `docs` before URL removal is considered (verified with both options off: `"Go to [docs](https://x.com) now"` → `"Go to docs now"`). Bare URLs are kept when `skipUrls` is off.

### 3.2 Character classes (`text.ts:22-24,37-39,59-61`)

```text
INVISIBLE   = [\u00AD\u180E\u200B-\u200F\u202A-\u202E\u2060-\u2064\uFEFF]      (removed; note: "a\u200Bb" → "ab")
ODD_SPACES  = [\t\v\f\u00A0\u1680\u2000-\u200A\u202F\u205F\u3000]            (→ " ")
LINE_BREAKS = \r\n?|[\u2028\u0085]                                           (→ "\n")
CJK         = [\u2E80-\u2FFF\u3040-\u30FF\u3400-\u4DBF\u4E00-\u9FFF\uF900-\uFAFF\uFF00-\uFFEF]
ENDS_WITH_PUNCT = [.!?:;,…。！？、，；：)"”’'\]]$
LINE_HYPHEN = \p{L}[-‐¬]$        (letter followed by U+002D, U+2010 or U+00AC at end)
UNSPACED    = [\u2E80-\u9FFF\uF900-\uFAFF\uFF00-\uFFEF]   (scripts written without spaces)
HOLE        = U+E000 (private-use placeholder marking removed URLs/e-mails, so cleanup only touches those spots)
```

`CJK` and `UNSPACED` are BMP-only ranges and are tested against single UTF-16 units in the original. Astral ideographs (CJK Extension B+, U+20000\u2026) are therefore **not** CJK there, and a char-based Rust port gets the same answer as long as it keeps the same ranges. Both include U+FF00\u2013U+FFEF (full-width Latin and half-width katakana), so `"\uFF28\uFF45\uFF4C\uFF4C\uFF4F\n\uFF37\uFF4F\uFF52\uFF4C\uFF44"` \u2192 `"\uFF28\uFF45\uFF4C\uFF4C\uFF4F\uFF37\uFF4F\uFF52\uFF4C\uFF44"` (joined with no space; verified).

Exact code points of every non-ASCII character in `text.ts`' patterns (the source glyphs are easy to mistype):

| Pattern | Non-ASCII members |
|---|---|
| `BULLET` (`text.ts:34`) | U+2022 \u2022, U+25E6 \u25E6, U+25AA \u25AA, U+2023 \u2023, U+00B7 \u00B7, U+25CF \u25CF, U+25CB \u25CB, U+25A0 \u25A0, U+25A1 \u25A1, U+2013 \u2013 (plus ASCII `- * +`) |
| `ENDS_WITH_PUNCT` (`:38`) | U+2026 \u2026, U+3002 \u3002, U+FF01 \uFF01, U+FF1F \uFF1F, U+3001 \u3001, U+FF0C \uFF0C, U+FF1B \uFF1B, U+FF1A \uFF1A, U+201D \u201D, U+2019 \u2019 (plus ASCII `. ! ? : ; , ) " ' ]`) |
| `LINE_HYPHEN` (`:39`) | U+2010 \u2010, U+00AC \u00AC (plus ASCII `-`) |
| `URL_TRAILING` (`:57`) | U+201D \u201D, U+2019 \u2019, U+00BB \u00BB (plus ASCII `. , ; : ! ? ' " ) ] } >`) |
| `closeHoles` pass 1 (`:187`) | openers U+FF08 \uFF08, U+FF3B \uFF3B; separators U+3001 \u3001, U+FF0C \uFF0C; closers U+FF09 \uFF09, U+FF3D \uFF3D |
| `closeHoles` pass 2 (`:189`) | U+3001 \u3001, U+FF0C \uFF0C |
| `closeHoles` pass 3 `post` (`:191`) | U+3001 \u3001, U+3002 \u3002, U+FF0C \uFF0C, U+FF0E \uFF0E, U+FF01 \uFF01, U+FF1F \uFF1F, U+FF1B \uFF1B, U+FF1A \uFF1A, U+FF09 \uFF09, U+FF3D \uFF3D, U+300D \u300D, U+300F \u300F |
| `closeHoles` pass 3 "sentence-ending post" (`:194`) | U+3002 \u3002, U+FF0E \uFF0E, U+FF01 \uFF01, U+FF1F \uFF1F, U+FF09 \uFF09, U+FF3D \uFF3D, U+300D \u300D, U+300F \u300F |
| `TERMINAL` (`:286`) | U+3002 \u3002, U+FF01 \uFF01, U+FF1F \uFF1F, U+203C \u203C, U+2047 \u2047, U+2048 \u2048, U+2049 \u2049 |
| `CLOSERS` (`:287`) | U+201D \u201D, U+2019 \u2019, U+00BB \u00BB, U+300D \u300D, U+300F \u300F |
| fallback CJK terminal (`:303`) | U+3002 \u3002, U+FF01 \uFF01, U+FF1F \uFF1F |
| `CLAUSE_TIGHT` (`:345`) | U+FF0C \uFF0C, U+3001 \u3001, U+FF1B \uFF1B, U+FF1A \uFF1A, U+2014 \u2014, U+2013 \u2013 |

### 3.3 Line classification and joining (`text.ts:26-35,63-69,99-152`)

Line regexes (JS syntax; `\s` is the JS whitespace set, §8):

````text
FENCE   = ^\s*(?:`{3,}|~{3,})\s*(?:[^\s`~]+(?:\s+(?:[^\s`~={}]+=[^\s`~{]*|\{[^`~{}\n]*\}))*)?\s*$
RULE    = ^\s*([-*_])(?:\s*\1){2,}\s*$
SETEXT  = ^\s*=+\s*$
HEADING = ^\s{0,3}#{1,6}\s+(.*?)(?:\s+#+)?\s*$
QUOTE   = ^\s*(?:>\s?)+
BULLET  = ^\s*[-*+•◦▪‣·●○■□–]\s+(.*)$
ORDERED = ^\s*(?:\d{1,3}[.)]|[a-z]\))\s+\S
````

`FENCE` matches a code fence delimiter line: ```` ``` ```` or `~~~` plus at most an info string (`js`, `c++`, `{r}`, `js title="a.js"`). Lines with content after the fence (```` ```npm install``` then… ````, `~~~ Chapter One ~~~`) are text, not fences. The key=value and `{…}` alternatives start with different characters, so a failing line never backtracks exponentially. **Only the fence lines are dropped; the code inside a fenced block is read aloud as text.**

`classify(raw) → Line{text, kind: text|heading|item, indented} | null` (`text.ts:99-109`):
1. `indented = /^\s{2,}\S/` on the **raw** line (before quote stripping). Tabs were already turned into single spaces, so one tab does not count as indented.
2. `line = raw` with the `QUOTE` prefix removed (nested `> >` too). Every later test runs on the quote-stripped line, so `> # Heading` is a heading and a bare `>` line is blank: `"> # Quoted heading\n>\n> para"` → `"Quoted heading\n\npara"` (verified).
3. Return `null` (a "blank" line) when `line.trim()` is empty, or it matches `RULE`, `SETEXT` or `FENCE`. Setext underlines therefore become paragraph breaks: `"Title\n=====\nBody"` → `"Title\n\nBody"` (verified). A `---` underline is caught by `RULE`.
4. If it matches `HEADING`: capture 1 non-empty → `{text: capture.trim(), kind: heading}`; empty (e.g. `"# "`) → `null`. A `#` must be followed by whitespace, so `#hashtag` is plain text, and so is a bare `####` (verified: `"####\nbody"` → `"#### body"`). More than six `#` is not a heading. Closing `##` are stripped.
5. If it matches `BULLET` → `{text: capture.trim(), kind: item}`. The bullet marker is removed. `"- "` alone gives an item with empty text (verified: `"- \nnext"` → `"next"`: the empty item contributes `""`, then `"\n"`, and the final trim removes it).
6. Else `line = line.trim()`; kind = `item` if it matches `ORDERED` (1–3 ASCII digits then `.` or `)`, or a single lowercase ASCII letter then `)`, followed by whitespace and a non-space; **the marker is kept in the text**), else `text`. `2024. was…` (four digits) and `A)` are plain text.

`joinLines(t)` (`text.ts:111-139`). It builds a list of parts and decides each join from the previous line only. (The original avoided reading the growing string inside the loop because that was quadratic in V8. In Rust, just push to a `String`.)
```text
parts = []; prev = None; paragraphBreak = false
for raw in t.split('\n'):
    line = classify(raw)
    if line is None:
        if prev: paragraphBreak = true          # leading blank lines are ignored
        continue
    if prev:
        last = prev.text
        if paragraphBreak:                         parts.push("\n\n")
        elif keepBreak(prev, line):                parts.push("\n")
        elif LINE_HYPHEN matches last's final 3 UTF-16 units  and  line.text starts with \p{Ll}:
            replace the last part (== prev.text) with last minus its final char   # "infor-\nmation" → "information"
            (no separator)                     # final 3 units, so an astral letter + hyphen still fits
        elif not (CJK(last char of last) and CJK(first char of line.text)):
            parts.push(" ")                    # CJK lines join with NO space
    parts.push(line.text); prev = line; paragraphBreak = false
return concat(parts)                           # trailing blank lines add nothing
```

In char terms, the hyphen test is: the last char of `last` is U+002D, U+2010 or U+00AC, **and** the char before it is `\p{L}`. The "final 3 UTF-16 units" slice exists only so that an astral letter followed by a hyphen still fits. The next line's first char must be `\p{Ll}`. The removed char is the hyphen (always one BMP unit). "Last char" and "first char" in the CJK test are single UTF-16 units in the original; because the `CJK` ranges are BMP-only, testing the last/first `char` gives identical results.

`keepBreak(prev, cur)` (`text.ts:141-152`). Keep a `\n` when:
1. either line is a `heading`; or
2. `cur` is an `item`; or
3. `prev` is an `item` → keep unless `cur.indented` (an indented continuation line is joined into the item with a space); or
4. a short title-like line: `prev.text.length < 40` (UTF-16 units) and `prev.text` does not match `ENDS_WITH_PUNCT` and `prev.text` contains **no** CJK char anywhere, and `cur.text` starts with `[\p{Lu}\p{N}]`. Examples: `Abstract\nWe propose…`, `Chapter 1\nThe beginning`. Counter-examples (verified): `"Intro:\nThe body"` → `"Intro: The body"` (ends with punctuation); `"Short\nlowercase next"` → `"Short lowercase next"`.

Rule 4 is never reached when `prev` is an item (rule 3 decides). Otherwise the lines are joined (the PDF hard-wrap case). Verified: `"item:\n  - nested\n  - nested2"` → `"item:\nnested\nnested2"`; `"Some text\n    indented code line\nmore"` → `"Some text indented code line more"` (indentation only matters after an item).

### 3.4 Inline markdown stripping (`text.ts:154-170`)

Applied in this exact order, globally. `m` = multiline (`^`/`$` also match at `\n`):

| # | Pattern | Replace | Purpose |
|---|---|---|---|
| 1 | `!\[([^[\]\n]*)\]\([^)\s]*(?:\s+"[^"]*")?\)` | `$1` | image → alt text |
| 2 | `\[([^[\]\n]+)\]\((?:[^()\s]\|\([^()\s]*\))*(?:\s+"[^"]*")?\)` | `$1` | link → link text (URL may contain one level of balanced parens) |
| 3 | `(^\|[^A-Za-z0-9*])\*\*(?=\S)((?:(?!\*\*)[^\n])*?\S)\*\*(?![A-Za-z0-9*])` (m) | `$1$2` | `**bold**` |
| 4 | `(^\|[^A-Za-z0-9_])__(?=\S)((?:(?!__)[^\n])*?\S)__(?![A-Za-z0-9_])` (m) | `$1$2` | `__bold__` |
| 5 | `(^\|[\s(])\*(?=\S)([^*\n]*?\S)\*(?=$\|[\s).,;:!?])` (m) | `$1$2` | `*em*` |
| 6 | `(^\|[\s(])_(?=\S)([^_\n]*?\S)_(?=$\|[\s).,;:!?])` (m) | `$1$2` | `_em_` |
| 7 | `` `+ `` | `` | remove all backticks |
| 8 | `~{2,}` | `` | `~~strike~~` and decorative `~~~ Chapter One ~~~` |

(`\|` in the table is a literal regex alternation `|`.)

Porting trap: in the Rust `regex`/`regex-syntax` dialect, an unescaped `[` inside a character class opens a **nested class**, so `[^[\]\n]` (rows 1–2) must be written `[^\[\]\n]`. Rust classes also treat `&&`, `--` and `~~` as set operators. None of these patterns contains one, but escape them if you rewrite a class.

All replacements are global, leftmost-first and non-overlapping, like JS `String.replace(/…/g)`. Because the `(^|[…])` prefix group consumes the preceding char, `**a** **b**` works (the space is consumed as `$1` of the second match), but two delimiters sharing one separator char can't both match. A Rust `replace_all` with the same patterns reproduces this.

Design reasons from the code comments:
- Link/image text stops at the next `[`, so text with many unclosed `[` stays linear.
- Bold applies only when not glued to a letter or digit, so `2**10 and 3**4` and `f(**a) and g(**b)` survive. The content can't run past another delimiter, which keeps the scan linear on text full of `**`.
- Single `*em*` / `_em_` apply only when delimited by spaces or punctuation, so `2 * 3` and `snake_case` survive.

HTML entities are **not** decoded (`foo&#160;bar` stays as is).

### 3.5 URL and e-mail removal (`text.ts:41-61,172-202`)

```text
URL_RE (flags: global, case-insensitive, unicode):
(?:\bhttps?:\/\/|\bftp:\/\/|(?<![@\w.-])www\.)[^\s<>"'`\u3000-\u30FF\uFF00-\uFFEF\p{Script=Han}\p{Script=Hangul}\p{Script=Thai}\p{Script=Lao}\p{Script=Khmer}\p{Script=Myanmar}]+
```
- Starts at `http://`, `https://` or `ftp://` (preceded by an ASCII word boundary), or at `www.` not preceded by `@`, an ASCII word char, `.` or `-`. Case-insensitive (`HTTPS://X.COM/Y ok` → `ok`, verified).
- Runs to whitespace, a quote or angle bracket, or the first CJK, kana, Hangul, Thai-family or full-width character. Those scripts don't put spaces between words, so `詳細はhttps://x.com/docsを参照…` must keep `を参照…`. Latin, Cyrillic and Greek IRI paths such as `/wiki/Москва` are still removed whole.
- The body needs at least one character, so a URL whose host **starts** with an excluded character is not matched at all: `http://例え.jp/パス is` stays unchanged (verified; known limitation).
- `"` `'` `` ` `` `<` `>` end the URL, so an autolink keeps its brackets: `See <https://x.com> now` → `See < > now` (verified; known limitation).
- `\p{Script=…}` here means the Script property, not Script_Extensions. The Rust `regex` crate gives the same meaning to `\p{Script=Han}` and to a bare `\p{Han}` (checked in `regex-syntax` 0.8 source). JS nuance: with the `iu` flags, `\b`/`\w` also treat U+017F ſ and U+212A K (Kelvin) as word characters through case folding. This is negligible, but it is why `(?-u:\b)` is not a 100 % emulation.

```text
EMAIL_LETTERS = \p{Script=Latin}\p{Script=Cyrillic}\p{Script=Greek}
EMAIL_LOCAL   = [EMAIL_LETTERS\p{N}._%+-]
EMAIL_LABEL   = [EMAIL_LETTERS\p{N}-]+
EMAIL_RE (flags: global, unicode; case-SENSITIVE):
(?:mailto:)?(?<!EMAIL_LOCAL)EMAIL_LOCAL+@EMAIL_LABEL(?:\.EMAIL_LABEL)*\.[EMAIL_LETTERS]{2,}(?![EMAIL_LETTERS])
```
The local part, domain labels and TLD are limited to spaced alphabets, so an address glued to CJK or Thai text (`連絡先はfoo@example.comまで`) doesn't take the neighboring words with it. The lookbehind stops a match from starting inside a run of those characters; without it, long letter runs would be quadratic. Note that the lookbehind sits **after** the optional `mailto:`, so for `mailto:foo@…` it checks the `:`. `mailto:` is only stripped in lowercase (`MAILTO:foo@bar.com` → `MAILTO:`; `email:foo@bar.com` → `email:`). The TLD needs ≥ 2 letters and must not be followed by another letter; `foo@bar.co.uk.` → whole address removed (`"Email foo@bar.co.uk."` → `"Email."`).

Order matters: URLs are removed (step 8) before e-mails (step 9), so with both options on, `user@www.example.com` is not cut at `www.` (the `www.` lookbehind sees `@`) and is then removed whole as an e-mail. With `skipEmails` off (the default), e-mails stay: `"Mail foo@example.com see https://x.com."` → `"Mail foo@example.com see."` (verified).

`urlTrailing(match)` (`text.ts:172-181`) gives back trailing punctuation that belongs to the sentence:
```text
trail = match of /[.,;:!?'"”’»)\]}>]+$/ at the end of the URL match (or "")
if trail has no ")": return trail
body = match minus trail
if count("(" in body) > count(")" in body):   # a ")" closing a "(" inside the URL (Wikipedia links) is part of the URL
    return trail with its FIRST ")" removed     # only ONE ")" is ever given back to the URL, however unbalanced
return trail
```
The `trail` is the maximal run of those characters at the end of the match: the leftmost position from which `[...]+$` matches. Quirk (verified): a `)` that is given back can swallow a preceding `:`. In `"A: https://a.com/b) c"`, the `)` is not part of the URL (the body has no `(`), so `)` is returned; pass 3 of `closeHoles` then sees `pre=":"`, `post=")"` and keeps only `post`, giving `"A) c"`.

`closeHoles(t)` (`text.ts:183-202`), three passes in order:
1. `[([（［]\s*\uE000(?:[\s,;、，]*\uE000)*\s*[)\]）］]` → `HOLE`. Brackets holding only removed items, like `(https://…)`, `[a@b.com]` or `（https://…）`, collapse (bracket kinds need not match).
2. `\uE000(?:[ \t]*(?:[,;/|、，]|and|or)?[ \t]*\uE000)+` → `HOLE`. Several removed items in a row become one. `and`/`or` are case-sensitive with no word boundary.
3. `([,;:]?)[ \t]*\uE000[ \t]*([,.;:!?)\]、。，．！？；：）］」』]?)` → computed per match (`pre` = group 1, `post` = group 2):
   - `post` matches `[.!?)\]。．！？）］」』]` → `post` (`"see <url>."` → `"see."`, `"(see <url>)"` → `"(see)"`, `"詳細は<url>。"` → `"詳細は。"`)
   - else if `post` is non-empty → `pre` if non-empty, else `post` (`"at <email>, or"` → `"at, or"`)
   - else if `pre` is non-empty → `pre + " "`
   - else: if the character just **before** the match and the character just **after** it (in the string before this pass) are both `UNSPACED` → `""` (`"詳細は<url>を参照"` → `"詳細はを参照"`), else `" "`. A missing neighbor (start or end of text) counts as not UNSPACED. The match includes the surrounding spaces/tabs and `pre`/`post`, so "before" and "after" are the characters outside those.

### 3.6 Verified `cleanText` examples (skipUrls = skipEmails = true)

| Input (JSON) | Output (JSON) |
|---|---|
| `"Hello   world"` | `"Hello world"` |
| `"This is infor-\nmation that wraps\nacross lines."` | `"This is information that wraps across lines."` |
| `"Abstract\nWe propose a method."` | `"Abstract\nWe propose a method."` |
| ``"# Title\nSome **bold** and _em_ and `code`.\n\n- item one\n- item two\n  continued\nAfter list."`` | `"Title\nSome bold and em and code.\n\nitem one\nitem two continued\nAfter list."` |
| `"## Title ##\nbody text"` | `"Title\nbody text"` |
| `"1. First\n2. Second\ncontinued"` | `"1. First\n2. Second\ncontinued"` |
| `"Chapter 1\nThe beginning"` | `"Chapter 1\nThe beginning"` |
| `"Line one\nline two continues"` | `"Line one line two continues"` |
| `"well-\nknown fact"` | `"wellknown fact"` (known limitation) |
| `"中文第一行\n中文第二行"` | `"中文第一行中文第二行"` |
| `"> quoted line\n> more quote"` | `"quoted line more quote"` |
| ```` "```js\nconst x = 1\n```\nDone." ```` | `"const x = 1\n\nDone."` |
| `"***\n\nPara two"` | `"Para two"` |
| `"a  \n\n\n\n  b"` | `"a\n\nb"` |
| `"Text\u2029Para"` | `"Text\n\nPara"` |
| `"Hello\u200Bworld\u00A0there\r\nnext"` | `"Helloworld there next"` |
| `"[link text](https://x.com \"title\") and ![alt](img.png)"` | `"link text and alt"` |
| `"2**10 and 3**4 and snake_case_name and 2 * 3 * 4"` | unchanged |
| `"~~strike~~ text ~~~ Chapter One ~~~"` | `"strike text Chapter One"` |
| `"see https://example.com/docs."` | `"see."` |
| `"Visit https://x.com today"` | `"Visit today"` |
| `"(see https://x.com)"` | `"(see)"` |
| `"Read (https://en.wikipedia.org/wiki/Foo_(bar)). Next."` | `"Read. Next."` |
| `"Results (https://a.com, https://b.com) are in"` | `"Results are in"` |
| `"Links: https://a.com, https://b.com and https://c.com."` | `"Links."` |
| `"Look here: https://a.com/x?y=1;"` | `"Look here:"` |
| `"www.example.com/path is it"` | `"is it"` |
| `"https://x.com"` | `""` |
| `"Mail me at foo@example.com, or call."` | `"Mail me at, or call."` |
| `"Contact: foo@x.com"` | `"Contact:"` |
| `"user@www.example.com"` | `""` |
| `"MAILTO:foo@bar.com"` | `"MAILTO:"` |
| `"詳細はhttps://x.com/docsを参照してください。"` | `"詳細はを参照してください。"` |
| `"連絡先はfoo@example.comまで"` | `"連絡先はまで"` |
| `"foo&#160;bar"` | `"foo&#160;bar"` |

Additional verified cases (second reader; skipUrls = skipEmails = true unless noted):

| Input (JSON) | Output (JSON) | Shows |
|---|---|---|
| `"- \nnext"` | `"next"` | empty bullet item |
| `"####\nbody"` | `"#### body"` | `#` without a following space is text |
| `"Title\n=====\nBody"` | `"Title\n\nBody"` | setext underline → paragraph break |
| `"> # Quoted heading\n>\n> para"` | `"Quoted heading\n\npara"` | quote stripped first; bare `>` is blank |
| `"item:\n  - nested\n  - nested2"` | `"item:\nnested\nnested2"` | nested bullets |
| `"Some text\n    indented code line\nmore"` | `"Some text indented code line more"` | indentation only matters after an item |
| `"Short\nlowercase next"` | `"Short lowercase next"` | title rule needs an uppercase/digit start |
| `"Intro:\nThe body"` | `"Intro: The body"` | title rule needs no final punctuation |
| `"1) one\n2) two"` | `"1) one\n2) two"` | ordered markers kept |
| `"a) alpha\nb) beta"` | `"a) alpha\nb) beta"` | letter markers kept |
| `"• bullet\n• two"` | `"bullet\ntwo"` | Unicode bullets removed |
| `"· mid"` / `"– dash item"` | `"mid"` / `"dash item"` | U+00B7 and U+2013 are bullets |
| `"A\tB"` | `"A B"` | tab → one space |
| `"x\u0085y"` | `"x y"` | NEL → line break → joined |
| `"﻿start"` | `"start"` | BOM removed |
| `"a　b"` | `"a b"` | ideographic space |
| `"Ｈｅｌｌｏ\nＷｏｒｌｄ"` | `"ＨｅｌｌｏＷｏｒｌｄ"` | full-width forms count as CJK |
| ```` "```npm install``` then go" ```` | `"npm install then go"` | not a fence; backticks stripped |
| ```` "~~~\ncode\n~~~" ```` | `"code"` | tilde fence |
| ```` "```js title=\"a.js\"\nx\n```" ```` | `"x"` | fence with attributes |
| `"See [a](b) [c](d)"` | `"See a c"` | two links |
| `"Go to [docs](https://x.com) now"` (both options off) | `"Go to docs now"` | link URL dropped regardless |
| `"HTTPS://X.COM/Y ok"` | `"ok"` | case-insensitive scheme |
| `"ftp://files.example.com/a.zip done"` | `"done"` | ftp |
| `"see (https://x.com/a_(b)_(c)) ok"` | `"see ok"` | balanced parens inside URL |
| `"x https://a.com/(b)) c"` | `"x) c"` | one `)` returned to the sentence |
| `"A: https://a.com/b) c"` | `"A) c"` | quirk: `:` swallowed |
| `"See <https://x.com> now"` | `"See < > now"` | quirk: autolink brackets stay |
| `"http://例え.jp/パス is"` | unchanged | quirk: CJK host not matched |
| `"see http://x.com/a b"` | `"see b"` | NBSP became a space before URL removal |
| `"mailto:foo@bar.com"` | `""` | lowercase `mailto:` removed |
| `"email:foo@bar.com"` | `"email:"` | |
| `"Email foo@bar.co.uk."` | `"Email."` | multi-label domain |
| `"Mail foo@example.com see https://x.com."` (skipEmails off) | `"Mail foo@example.com see."` | default settings |
| `"a  b"` (both off) | `"a b"` | literal U+E000 treated as a hole |
| `"trailing\n\n\n"` | `"trailing"` | |

Use both tables as a golden test suite for the Rust port.

### 3.7 Sentence splitting: `sentenceSpans(text)` (`text.ts:222-340`)

Returns trimmed `[start, end)` spans (no leading or trailing whitespace) with abbreviations merged.

1. **Raw segmentation**: `Intl.Segmenter('en', { granularity: 'sentence' })`, which is ICU's UAX #29 sentence-break rules. Each segment covers its trailing whitespace, and **every line break (`\n`) ends a sentence** (UAX #29 SB4). That matters because cleaned text keeps `\n` for list items and headings. If the segmenter is unavailable, use `fallbackSegments` (below).
2. For each raw span: `trimSpan` (strip `isSpace` code units at both ends); skip it if empty.
3. If there is a previous output span and `shouldJoin(text, last, t)` → extend `last.end = t.end`; else push `t`.

`isSpace(code)` (`text.ts:270-284`): 32, 9..=13, 0xA0, 0x3000, 0x2028, 0x2029, 0x2000..=0x200A, 0x202F, 0x205F, 0x1680, 0xFEFF. (NEL 0x85 is not included.)

**Abbreviation merging** (`text.ts:237-260`). ICU (and so `Intl.Segmenter`) has no abbreviation suppressions, so `Dr. Smith` would otherwise be two sentences.
```text
HONORIFICS (case-sensitive): Mr Mrs Ms Mx Messrs Mme Mlle Dr Prof Rev Hon St Mt Ft Gen Col Lt Sgt Capt Cmdr Adm Gov Sen Rep Pres Fr Supt Insp
ALWAYS_JOIN (lower-cased compare): e.g  i.e  vs  v  cf  viz  approx  ca  resp  incl
BEFORE_NUMBER (lower-cased compare): no nos fig figs eq eqs vol vols ch sec pp p art para ref refs tab
                                      jan feb mar apr jun jul aug sep sept oct nov dec ver op ed
LAST_TOKEN = /(?:^|[^\p{L}.])(\p{L}+(?:\.\p{L}+)*)\.$/u
```
`shouldJoin(text, cur, next)`:
1. `window = text[max(cur.start, cur.end − 24) .. cur.end]` (the last ≤ 24 UTF-16 units of the previous span; `cur` is the output span so far, already extended by earlier joins). Apply `LAST_TOKEN`; no match → don't join. The span must end with `.` right after the token: `."` endings never join. `^` in `LAST_TOKEN` anchors at the **window** start, so a long word cut by the 24-unit window counts as a (partial) token.
2. `word` = capture; `nextChar` = `text[next.start]` (the first UTF-16 unit of the next span; an astral first char is a lone surrogate there and never matches `\p{Lu}`/`\p{N}`. A char-based port would match it, which is harmless).
3. `word` is a single uppercase letter (`^\p{Lu}$`) → join iff `nextChar` is `\p{Lu}`. These are initials: `J. R. R. Tolkien`. This decides the outcome; no other rule is checked.
4. `word` ∈ HONORIFICS → join.
5. `lower(word)` ∈ ALWAYS_JOIN → join.
6. `lower(word)` ∈ BEFORE_NUMBER → join iff `nextChar` is `\p{N}` (`Fig. 3`, `No. 5`, `p. 12`, `Jan. 5`).
7. Else don't join.

Verified:

| Text | Sentences |
|---|---|
| `Dr. Smith went to Washington. He arrived at 5 p.m. Then left.` | `Dr. Smith went to Washington.` / `He arrived at 5 p.m.` / `Then left.` |
| `See Fig. 3 for details. And No. 5 too.` | `See Fig. 3 for details.` / `And No. 5 too.` |
| `J. R. R. Tolkien wrote it. Yes.` | `J. R. R. Tolkien wrote it.` / `Yes.` |
| `e.g. This one. Next.` | `e.g. This one.` / `Next.` |
| `Hello world.\nNext line here` | `Hello world.` / `Next line here` |
| `Mr. and Mrs. Smith. Next.` | `Mr. and Mrs. Smith.` / `Next.` *(second reader)* |
| `He said "Stop." Then left.` | `He said "Stop."` / `Then left.` (ICU break after the closing quote; `."` never joins) |
| `Wait... What? Yes!` | `Wait...` / `What?` / `Yes!` |
| `中文句子。第二句。` | `中文句子。` / `第二句。` (no space needed after `。`) |
| `Version 2.0 is out. Next.` | `Version 2.0 is out.` / `Next.` |
| `Hello world.Next sentence.` | one sentence (ICU UAX #29 SB7: no break between letter `.` and an uppercase letter with no space) |
| `vs. them. ok` | one sentence (ICU never breaks before a lowercase word, SB8) |
| `Fig. A shows` | `Fig.` / `A shows` (`BEFORE_NUMBER` needs a digit) |
| `Plan A. Then we left.` | **one sentence**: known limitation, the single-capital "initial" rule joins any `X.` followed by a capital |
| `U.S. Army is here. Next.` | `U.S.` / `Army is here.` / `Next.`: known limitation, dotted acronyms are not in any set |

These rows mix ICU's raw boundaries with the merge rules. A Rust port using ICU4X or `unicode-segmentation` should pass all of them before the merge logic is considered done.

**`fallbackSegments(text)`** (`text.ts:286-315`), used only without a segmenter:
```text
TERMINAL = [.!?。！？‼⁇⁈⁉]      CLOSERS = ["'”’)\]»」』]
start = 0
for i over code units:
  if text[i] == '\n': push [start, i+1]; start = i+1; continue
  if text[i] not TERMINAL: continue
  j = i+1; while j < len and (TERMINAL(text[j]) or CLOSERS(text[j])): j++
  if j >= len or text[i] in [。！？] or isSpace(text[j]):
      while j < len and text[j] != '\n' and isSpace(text[j]): j++
      push [start, j]; start = j; i = j-1
  else: i = j-1
if start < len: push [start, len]
```

`sentenceStarts(text)` = the `start` of each span. It is exported but **unused**: the player has its own version (`src/renderer/src/widget/timing.ts:103-127`, widget spec).

### 3.8 Chunking: `chunkText(text, {firstMax = 220, max = 900})` (`text.ts:342-410`)

Splits text into synthesis chunks on sentence boundaries. The first chunk is ≤ `firstMax` (fast first audio); later chunks are ≤ `max`. Sentences over the limit are split at clause punctuation, then at spaces. Offsets index the input exactly (`text[start..end] == chunk.text`). **Whitespace between chunks belongs to neither chunk.** Chunks may contain internal `\n`/`\n\n`.

```text
FIRST_CHUNK_MAX = 220; CHUNK_MAX = 900
firstMax = max(20, floor(opts.firstMax ?? 220))
max      = max(firstMax, floor(opts.max ?? 900))
stack = reverse(sentenceSpans(text))     # pop() yields sentences in order
out = []; cur = None
while stack not empty:
    unit  = stack.pop()
    limit = (out is empty) ? firstMax : max      # the chunk being built is the first one until one is emitted
    if cur is None:
        if len(unit) <= limit: cur = copy(unit)
        else:
            (head, rest) = splitSpan(text, unit, limit)
            out.push(head)
            if rest: stack.push(rest)            # remainder is re-considered (and may merge with following sentences)
    elif unit.end - cur.start <= limit:          # merged length includes the whitespace between sentences
        cur.end = unit.end
    else:
        out.push(cur); cur = None; stack.push(unit)
if cur: out.push(cur)
return [ {text: text[s.start..s.end], start, end} for s in out ]
```

`splitSpan(text, span, limit)` (`text.ts:352-375`):
```text
CLAUSE_SPACED = [;:,)\]]      (a cut only when followed by whitespace)
CLAUSE_TIGHT  = [，、；：—–]   (full-width comma, ideographic comma, full-width semicolon/colon, em dash, en dash; no space needed)
s  = span.start
hi = min(span.end, s + limit)
lo = s + floor(limit * 0.35)
findCut(from, test): for p = hi down to from+1: if test(p) return p; return -1
followedBySpace(p) = p < len(text) and isSpace(text[p])
clause(p) = CLAUSE_TIGHT(text[p-1]) or (CLAUSE_SPACED(text[p-1]) and followedBySpace(p))
cut = findCut(lo, clause)                       # 1. latest clause boundary in the last 65 % of the window
if cut < 0: cut = findCut(lo, followedBySpace)  # 2. latest space in the last 65 %
if cut < 0: cut = findCut(s,  followedBySpace)  # 3. any space
if cut < 0: cut = safeCut(text, hi)             # 4. hard cut, never inside a surrogate pair
if cut <= s: cut = min(span.end, s + max(1, limit))
head = trimSpan(text, s, cut) ?? {s, cut}
rest = trimSpan(text, cut, span.end)            # may be None
safeCut(text, pos) = pos-1 if text[pos-1] is a high surrogate (0xD800..=0xDBFF) else pos
```

Verified:
- 40 sentences `Sentence number N has a few words in it.` for **N = 0..39**, joined with single spaces (1 669 chars) → chunks `[0,204) [205,1081) [1082,1669)` (lengths 204, 876, 587). With N = 1..40 (1 670 chars) the result is `[0,204) [205,1082) [1083,1670)` (204, 877, 587). *(Second reader: the first version did not say which N range was used.)*
- `"word " × 100 + "end, more words " + "x"×10` → `[0,219)` (cut at a space), `[220,526)`.
- `"alpha beta gamma, " × 30` (one 540-char sentence) → `[0,215)` ends at `gamma,` (clause cut), `[216,539)`.
- `"中" × 500` → `[0,220)` (hard cut), `[220,500)`.
- `"😀" × 300` (600 UTF-16 units) → `[0,220) [220,600)`: 110 emoji, then 190. With char offsets the same rules give `[0,220) [220,300)` (220 emoji, then 80). The chunking differs with the offset unit, which is one more reason to pick the unit once (Units warning).
- `""` and `"   "` → no chunks (the engine then fails with `empty-text`, §4.7).
- `chunkText("Hi.", {firstMax: 5, max: 3})` → `[0,3)` (`firstMax` is raised to 20).

The engine always calls `chunkText(text)` with the defaults (220/900).

### 3.9 `truncateAtBoundary(text, maxChars)` (`text.ts:204-220`): exported, unused

Cuts to at most `maxChars`, preferring a sentence end, then a word boundary, in the last 40 % of the window:
```text
if len <= maxChars: return text
min = floor(maxChars * 0.6); window = text[0 .. maxChars+1]
cut = the largest sentenceSpans(window).end with min <= end <= maxChars, else -1
if cut < 0: space = index of the last whitespace in window that is followed only by non-whitespace (/\s\S*$/)
            cut = space >= min ? space : safeCut(text, maxChars)
return trimEnd(text[0..cut])
```
Example: `("One two three. Four five six seven. Eight nine ten eleven twelve.", 40)` → `"One two three. Four five six seven."`.
The live app truncates with `reader.ts:118-126 truncateText` instead (see the reader spec): cut at `max`, back up to the last whitespace only if it lies in the final 10 % (`> max × 0.9`), never split a surrogate pair, then `trimEnd`. The `cleanText` doc comment still points at `truncateAtBoundary`, but nothing calls it. Port it only if the Rust reader wants sentence-boundary truncation.

---

## 4. TTS engine (`engine.ts`)

The engine turns cleaned text into a stream of synthesized chunks for the player (the floating widget owns playback). One engine instance; **at most one live session**.

### 4.1 Public surface (`engine.ts:25-59,105-194`)

```text
trait TtsSink {                      // delivery to the player; any call may fail (e.g. window destroyed)
  session_start(TtsSessionStart); chunk(TtsChunk); done(TtsSessionDone); error(TtsSessionError)
}
Engine::new(client, sink, mock: bool, logger)
start(StartSessionOptions{ readId, text /* already cleaned+truncated */, title, voiceId, voiceName, sourceApp?, settings }) -> sessionId
playCached(PlayCachedOptions{ readId, text, title, voiceName, sourceApp?, settings, chunks: Vec<CachedChunk> }) -> sessionId
progress(sessionId, positionSec)     // the player's playhead, in seconds of session audio
stop()
currentSessionId -> Option<String>   // latest session, kept after synthesis finishes (audio may still play) until stop()
currentReadId    -> Option<String>
isMock -> bool
Events (to the read controller):
  complete(readId, chunks: Vec<CachedChunk>, charactersBilled)   // EVERY chunk synthesized; safe to cache. Not for cached playback.
  failed(readId, ElevenLabsError)                                // session failed before any audio
  stopped(readId, charactersBilled, error?: ElevenLabsError)     // ended before the whole text was synthesized, after some
                                                                 // chunks were billed. No `error`: stopped/superseded.
                                                                 // With `error`: a later chunk failed for good (player plays
                                                                 // up to the last good chunk). Lets the caller record spent credits.
```
The mock flag comes from `VOX_MOCK_TTS=1|true`, implied by demo mode (`VOX_DEMO=1|true` or `VOX_SHOTS_DIR` set) (`log.ts:20-28`).

### 4.2 Constants (`engine.ts:61-75`)

| Name | Value | Meaning |
|---|---|---|
| `RETRY_DELAYS_MS` | `[1000, 2000, 4000]` | Backoff before retries 1, 2 and 3 of a retryable chunk error (max 4 attempts per chunk) |
| `MAX_RETRY_AFTER_MS` | 10 000 | Cap on a server `retry-after` hint |
| `MOCK_LATENCY_MS` | `{ first: 350, next: 120 }` | Simulated latency in mock mode |
| `CONTEXT_CHARS` | 300 | Max size of `previous_text` / `next_text` |
| `LOOKAHEAD_SEC` | 120 | Max seconds of delivered-but-unplayed audio before synthesis pauses |
| `LOOKAHEAD_FREE_CHUNKS` | 2 | Chunks `[0, 2)` are requested without waiting for the listener |

### 4.3 Session state (`engine.ts:77-90,198-216`)

```text
Session { id: UUIDv4, readId, cancel: CancellationToken, finished: bool,
          billed: chars billed so far, deliveredSec: audio seconds delivered so far,
          positionSec: latest playhead reported, wake: Option<waker> /* set while waiting for the listener */ }
isLive(s) = (engine.session is s) and not s.cancel.is_cancelled()
```
`begin(readId)`: `stop()` the current session, then create and install a new one.

### 4.4 `start(opts)` and the synthesis loop (`engine.ts:133-151,236-298`)

`start`:
1. `session = begin(readId)`.
2. `chunks = chunkText(opts.text)` (defaults 220/900).
3. `speed = positive(settings.voice.speed)` (finite and > 0, else 1).
4. **Synchronously** call `sink.session_start` (inside the sink guard, §4.10) with: `sessionId`, `readId`, `text`, `title`, `voiceName`, `rate = settings.playbackRate`, `volume = settings.volume`, `highlightWords = settings.widget.highlightWords`, `sourceApp` (only if non-empty), `estimatedDurationSec = text.length / 15 / speed`, `chunkCount = chunks.len()`, `fromCache = false`.
5. Spawn `run(session, chunks, opts)`. If it fails unexpectedly → `crashed`.
6. Return `session.id`.

**Ordering guarantee:** `session_start` is delivered before `start` returns. Chunks, done, errors and **all events** arrive asynchronously afterwards; `run` first yields once (`await Promise.resolve()`), so nothing is emitted before `start()` returns.

Verified order when a new read supersedes a running one (second reader, fake client): `sink.session_start(new)` → `start()` returns → event `stopped(oldReadId, oldBilled)` → `sink.chunk(new, 0)` → … → `sink.done(new)` → event `complete(new)`. The old session's `stopped` is queued by `stop()` inside `begin()` before `run`'s first yield, so it always precedes every chunk or event of the new session. The old session's aborted request then fails silently.

A caller that calls `stop()` right after `start()` (before the first yield) gets nothing more: `run` sees `!isLive` and returns, and with `billed == 0` no `stopped` is emitted.

`start()` itself does work synchronously (`chunkText`, `session_start`). It throws only on a programming error; the reader catches that (`reader.ts:376-382`, toast `errorMessage('unknown', message)`).

`run(s, chunks, opts)`:
```text
yield; if !isLive(s): return
if chunks.is_empty(): fail(s, ElevenLabsError('empty-text', "There is no text to read")); return
delivered = []; billed = 0; cutShort = None
for i in 0..chunks.len():
    c = chunks[i]
    if i >= LOOKAHEAD_FREE_CHUNKS:
        waitForListener(s); if !isLive(s): return
    match synthesizeChunk(s, chunks, i, opts):
      Err(err):
        if !isLive(s): return                                   # stopped/superseded: silent
        e = toElevenLabsError(err)
        if i == 0: fail(s, e); return                           # fatal, nothing played
        sink.error{ sessionId, message: e.message, code: e.code, fatal: false }
        cutShort = Some(e); break
      Ok(result):
        if !isLive(s): return
        durationSec = chunkDuration(result, c, settings)
        sink.chunk{ sessionId, index: i, textStart: c.start, textEnd: c.end,
                    audio: result.audio, mime: result.mime, alignment: result.alignment, durationSec }
        delivered.push(CachedChunk{ index: i, textStart: c.start, textEnd: c.end, mime, alignment, durationSec, audio })
        billed += result.characterCost ?? c.text.length          # header value if present (a header "0" counts as 0), else UTF-16 length
        s.billed = billed; s.deliveredSec += durationSec
s.finished = true          # BEFORE emitting, so a later stop() doesn't report this session again
sink.done{ sessionId, totalChunks: delivered.len() }            # after a cut-short, totalChunks < chunkCount
if cutShort: emit stopped(readId, billed, cutShort)
else:        emit complete(readId, delivered, billed)
```
**Partial audio is never reported as complete**: it would be cached, and every replay would stop early.

The original copies the audio bytes for the sink (`new Uint8Array(result.audio)`) because a Node Buffer may be a pooled slice. In Rust, share one immutable buffer (`Arc<[u8]>`/`bytes::Bytes`) between the sink message and the `delivered` list.

### 4.5 Per-chunk synthesis and retries (`engine.ts:341-373,401-417`)

Mock mode: sleep `350 ms` (chunk 0) or `120 ms` (others), cancellable (Node's abortable timer rejects with an `AbortError`, which `toElevenLabsError` maps to `aborted`); then `mockSynthesize(chunk.text, settings.voice.speed)` (§5). Mock mode never calls the client: no key is needed, `voiceId`/`modelId`/`outputFormat`/`languageCode` are ignored, there are no retries, and the result is always `audio/wav`. The look-ahead still applies.

Real mode request:
```text
SynthesisRequest {
  text: chunks[i].text, voiceId: opts.voiceId, modelId: settings.modelId,
  outputFormat: settings.outputFormat, voiceSettings: settings.voice,
  previousText: contextBefore(chunks[i-1]?.text),   # prosody continuity across chunk boundaries
  nextText:     contextAfter(chunks[i+1]?.text),
  languageCode: settings.languageCode if non-empty
}
```

Context windows:
```text
contextBefore(t):   # up to ~300 chars from the END of the previous chunk, starting at a word boundary
  if t empty/None: None
  if len(t) <= 300: t
  tail = last 300 units of t
  sp = index of the FIRST whitespace in tail
  r = (0 <= sp < 60) ? tail[sp+1..] : tail
  r = trim(r); None if empty

contextAfter(t):    # up to ~300 chars from the START of the next chunk, ending at a word boundary
  if t empty/None: None
  if len(t) <= 300: t
  head = first 300 units of t
  sp = index of the LAST whitespace in head that is followed only by non-whitespace to the end (/\s\S*$/)
  r = (sp > 240) ? head[..sp] : head
  r = trim(r); None if empty
```
(For `eleven_v3`, the client drops both, §2.6.1.)

`len`/`last 300 units` are UTF-16 in the original, so a slice can start or end in the middle of a surrogate pair. JSON-encoding then emits a lone `\udXXX` escape. A char-indexed port cannot produce this; with UTF-16 offsets, apply `safeCut`-style adjustment. "Whitespace" in both helpers is JS `\s` (only `' '` and `'\n'` occur in cleaned text). The previous/next chunk is the **whole** neighbor chunk's text, never the session text beyond it. The first chunk gets no `previous_text` and the last no `next_text`. Verified: a 351-char chunk `"a"×250 + " " + "b"×100` gives `previous_text = "a"×199 + " " + "b"×100`: the tail's first space is at index 199 ≥ 60, so the tail is kept whole. `contextAfter("c"×100 + " " + "d"×250)` → the first 300 units unchanged (the last space is at index 100, not > 240).

Retry loop:
```text
for attempt in 0..:
    match client.synthesize(req, s.cancel):
        Ok(r) => return r
        Err(e) => e = toElevenLabsError(e)
            if e.aborted or !e.retryable or attempt >= 3 or !isLive(s): return Err(e)
            hinted = min((e.retryAfterSec ?? 0) * 1000, 10_000)
            sleep(max(RETRY_DELAYS_MS[attempt], hinted)) cancellable by s.cancel   # cancel → AbortError → aborted
```
Retryable (§2.4): rate-limit (429, `rate_limit_error`, concurrency, `system_busy`), any 5xx, network errors **including timeouts**, unparseable JSON, and empty audio. Worst case per chunk: 4 attempts × (up to 3 HTTP requests for parameter fallback) × 30–45 s, plus waits of 1 + 2 + 4 s (or `retry-after` hints up to 10 s). The `retry-after` hint can only lengthen a wait, never shorten it below the backoff step. Liveness is checked before each sleep. Not retryable, so they end the chunk at once: `no-api-key` (key removed mid-read), `unauthorized`, `quota`, `too-long`, voice/model errors, and other 4xx.

Every engine attempt calls `client.synthesize` with the original full request, so the parameter fallback of §2.6.1 starts over each time (it is not remembered across attempts or chunks).

### 4.6 Bounded look-ahead (credit protection) (`engine.ts:66-75,170-181,300-314`)

Synthesis is 30–50× faster than real time, and **every character is billed when it is requested**, so synthesis must not run ahead of the listener:
- Chunks 0 and 1 are requested back to back, without waiting. This gives fast first audio and no gap after the short first chunk. Requests are always **sequential** (one in flight per session).
- Every later chunk `i >= 2` first waits in `waitForListener` until `deliveredSec − positionSec < 120`.
- `deliveredSec` sums the real `durationSec` of delivered chunks. `positionSec` is the player's playhead in **seconds of session audio** (sum of finished chunk durations + current position in the current chunk). It does not depend on the client playback rate: 120 s of audio is about 40 s of listening even at 3×, far more than one request takes.

```text
waitForListener(s):
  while isLive(s) and s.deliveredSec - s.positionSec >= 120:
      wait until: progress() wakes it, OR s.cancel fires
      (then loop and re-check)

progress(sessionId, positionSec):
  s = current session; ignore if none, if s.id != sessionId, or if positionSec is not finite
  s.positionSec = max(0, positionSec)        # may move backwards on a seek back
  take s.wake and call it (if set)
```
`wake` is a single slot: one waiter at most (requests are sequential). Each wake re-runs the loop check, and if there is still too much unplayed audio, a fresh waiter is installed. Progress reports also arrive after synthesis has finished, while the audio is still playing; they just update `positionSec`. `positionSec` may exceed `deliveredSec`.
The player reports the playhead every 500 ms while playing, and on seeks (via the read controller, `reader.ts:559-565`). **Pausing holds synthesis, and so does a player that stops reporting.** `stop()` or a new session ends the wait.

Worked example at 15 chars/s, speed 1: chunk 0 (≤ 220 chars ≈ 15 s) and chunk 1 (≤ 900 ≈ 60 s) give 75 s < 120, so chunk 2 is also requested immediately (≈ 135 s). Chunk 3 waits until the playhead passes ≈ 15 s. A 20 000-character read stopped after 10 s therefore bills about 3 chunks (~2 000 chars), not the whole text.

### 4.7 `stop()`, cancellation and failure semantics (`engine.ts:183-194,375-394`)

`stop()`:
```text
s = current session; if none: return
engine.session = None
s.cancel.cancel()                            # aborts the in-flight HTTP request, retry sleeps, mock sleep, and listener wait
if !s.finished and s.billed > 0:
    emit stopped(s.readId, s.billed) ASYNCHRONOUSLY (next tick)
```
It is asynchronous like every other event because `stop()` can run inside `start()` (a new read superseding this one). Anything that resolves later for the stopped session is ignored (every await point re-checks `isLive`).

**What `billed` covers:** only chunks that were successfully delivered. The characters of a request that was in flight when the session was stopped, or that failed after ElevenLabs may already have charged it (timeouts), are **not** counted. A stop while chunk 0 or 1 is still in flight therefore emits no `stopped` at all (`billed == 0`), even though ElevenLabs may bill those requests. Verified: a 1 669-char read superseded while chunk 2 was in flight → `stopped(r1, 1080)` = 204 + 876 (the header was absent, so UTF-16 lengths were used), and the in-flight 587 chars were not counted.

`stop()` is idempotent: a second call finds no session. After a session finishes (`complete`, `failed` or a cut-short `stopped`), `stop()` still clears `currentSessionId` but emits nothing.

| Situation | Player (sink) sees | Event emitted |
|---|---|---|
| Text yields 0 chunks | `session_start(chunkCount 0)`, then `error{fatal:true, code:'empty-text', "There is no text to read"}` | `failed` |
| Chunk 0 fails for good | `error{fatal:true}` with the error's message and code | `failed` |
| Chunk i > 0 fails for good | `error{fatal:false}`, then `done{totalChunks = chunks delivered}` | `stopped(readId, billed, error)` |
| All chunks delivered | `done{totalChunks}` | `complete(readId, chunks, billed)` |
| `stop()` / superseded mid-synthesis, billed > 0 | nothing more | `stopped(readId, billed)` (no error) |
| `stop()` / superseded before anything was billed | nothing more | none |
| Unexpected internal failure in `run`/`replay` | if still live and not finished: `error{fatal:true, code:'unknown', "Something went wrong while preparing the audio"}`; logs `[tts] session crashed:` **always** (even when the session is no longer live or already finished) | `failed` (only if live and not finished) |
| Sink call fails (e.g. player window gone) | — | none; logs `[tts] sink failed:` and **synthesis and caching continue** |
| Missing key at synthesis time (key cleared mid-read; the reader normally refuses to start without one, `reader.ts:307-314`) | chunk 0: `error{fatal:true, code:'no-api-key', MESSAGES.noKey}`; later chunk: `error{fatal:false}` + `done` | `failed` / `stopped(…, error)` |

`fail(s, e)`: `s.finished = true`; `sink.error{ sessionId, message: e.message, code: e.code, fatal: true }`; emit `failed(readId, e)`.

Cancelled errors (`aborted`) are never shown. The `isLive` checks make them silent.

### 4.8 Chunk duration: `chunkDuration(result, chunk, settings)` (`engine.ts:419-435`)

```text
alignEnd = max(0, max(result.alignment.ends))          # 0 if no alignment
measured = None
if result.mime == 'audio/mpeg' and (kbps = mp3Kbps(settings.outputFormat)):
    measured = audio.len_bytes * 8 / (kbps * 1000)      # CBR MP3
elif result.mime == 'audio/wav':
    measured = readWavInfo(audio)?.durationSec
if alignEnd > 0:
    return (measured is Some and alignEnd <= measured <= alignEnd + 2) ? measured : alignEnd
if measured is Some and measured > 0: return measured
return chunk.text.len / 15 / positive(settings.voice.speed)
```
The byte-size duration is preferred when it is slightly longer than the last alignment end (trailing silence after the last character), but only within +2 s, which guards against bad estimates.

### 4.9 Cached playback: `playCached(opts)` (`engine.ts:153-167,316-339`)

1. `session = begin(readId)`. Sort chunks by `index`.
2. Synchronously `sink.session_start` with `estimatedDurationSec = Σ durationSec`, `chunkCount = n`, `fromCache = true` (other fields as in §4.4).
3. Async: yield; if not live return. If there are no chunks → `fail('not-cached', "The saved audio for this read is missing")`, which sends `error{fatal:true, code:'not-cached'}` and **emits `failed(readId, error)`** (verified).
4. Else, for each chunk in order: `sink.chunk` with `index` **renumbered 0..n-1** (so the player always sees 0..n-1; gaps or duplicates in the stored indices disappear, and equal indices keep their input order because the sort is stable), original `textStart`/`textEnd`/`mime`/`alignment`/`durationSec`/`audio`. Then `finished = true` and `sink.done{totalChunks: n}`.
5. **No `complete` event, no billing, no look-ahead** (all chunks are pushed at once; it costs nothing). Step 4 runs synchronously in one go after the single yield, so `stop()` can never interleave with it. A Rust task that awaits between sends should either send everything without awaiting or re-check liveness before each send.

`playCached` takes the current `settings` (rate, volume, highlightWords), not the values from the original read. It does not need `voiceId`.

### 4.10 Sink guard (`engine.ts:388-394`)

Every sink call is wrapped: an error is logged as `[tts] sink failed:` and swallowed. Event-listener failures are **not** wrapped. In the original a throwing listener propagates into `run`, which then ends in `crashed` (after `finished = true`, so nothing extra is emitted). The exception is the `stopped` event emitted from `stop()`: it runs in a queued microtask, so a throwing listener there is an uncaught process-level exception. The Rust port should catch and log listener failures everywhere instead.

The production logger is `createLogger('tts')` (`reader.ts:276`); the default is the console. The app-wide logging rule (`log.ts:4`) is to never log the text being read or keys. `ElevenLabsError` messages never contain the key, but a crash's error object is logged as is.

---

## 5. Mock synthesizer (`mock.ts`)

An offline stand-in for ElevenLabs (mock mode, demo and screenshot modes). It lets the whole pipeline run without a key or network.

| Constant | Value |
|---|---|
| `MOCK_SAMPLE_RATE` | 22 050 Hz |
| `TONE_HZ` | 220 |
| `AMPLITUDE` | 0.015 (very quiet) |
| `FADE_SEC` | 0.03 |
| `MIN_DURATION_SEC` | 0.6 |

The mock's purpose (`mock.ts:16`): `characterCost` mimics the API's billing header so that demo analytics look realistic.

`mockSynthesize(text, speed)` (`mock.ts:13-36`):
```text
rate      = (speed finite and > 0) ? speed : 1
targetSec = max(0.6, text.len / 15 / rate)
frames    = round(targetSec * 22050)
fade      = max(1, min(round(0.03 * 22050) /* = 662; JS rounds 661.5 up */, floor(frames / 2)))
for i in 0..frames:
    envelope   = min(1, i / fade, (frames - 1 - i) / fade)
    samples[i] = 0.015 * envelope * sin(2π * 220 / 22050 * i)        # f32
durationSec = frames / 22050
return { audio: encodeWav(samples, 22050), mime: "audio/wav",
         alignment: evenAlignment(text, durationSec), characterCost: text.len, requestId: None }
```

`evenAlignment(text, durationSec)` (`mock.ts:38-48`): empty text → null. `chars` = the text split into **UTF-16 code units** (one entry per unit, so it maps 1:1 onto text offsets). `per = durationSec / n`; `starts[i] = round3(i * per)`; `ends[i] = round3((i+1) * per)`, except the last entry, which is `round3(durationSec)`. `round3(v) = round(v * 1000) / 1000`, JS `Math.round` (half rounds toward +∞).

Example: `mockSynthesize("Hi.", 1)` → WAV of 26 504 bytes (13 230 frames = 0.6 s), alignment `chars [H,i,.]`, `starts [0, 0.2, 0.4]`, `ends [0.2, 0.4, 0.6]`, `characterCost 3`. `mockSynthesize("x"×30, 2)` → 1.0 s, 44 144 bytes (verified).

Numeric details: since `frames ≥ 13 230`, `fade` is always exactly 662. Compute the sample in f64 (`0.015 × envelope × sin(step × i)` with `step = 2π·220/22050`), then store it as **f32** (the original writes into a `Float32Array`). `encodeWav` reads that f32 back. For positive values JS `Math.round` and Rust `f64::round` agree (both round .5 up), so `frames` and `fade` match directly.

In the Rust port, if offsets become `char` indices, split by `char` instead so the mock stays 1:1.

---

## 6. WAV helper (`wav.ts`)

### `encodeWav(samples: f32[], sampleRate) -> bytes` (`wav.ts:5-27`): 16-bit PCM mono, 44-byte header, little-endian

| Offset | Size | Value |
|---|---|---|
| 0 | 4 | `RIFF` |
| 4 | u32 | `36 + dataBytes` |
| 8 | 4 | `WAVE` |
| 12 | 4 | `fmt ` |
| 16 | u32 | 16 |
| 20 | u16 | 1 (PCM) |
| 22 | u16 | 1 (mono) |
| 24 | u32 | sampleRate |
| 28 | u32 | sampleRate × 2 (byte rate) |
| 32 | u16 | 2 (block align) |
| 34 | u16 | 16 (bits per sample) |
| 36 | 4 | `data` |
| 40 | u32 | dataBytes = samples × 2 |
| 44… | i16 each | `v = clamp(s, -1, 1)`; `round(v < 0 ? v * 32768 : v * 32767)` (JS `Math.round`: halves toward +∞) |

Rounding trap: for negative halves JS `Math.round(-0.5)` is `-0` → 0, while Rust's `(-0.5f64).round()` is −1. An f32 sample of exactly −2⁻¹⁶ hits this. Use `(x + 0.5).floor()` for parity. (A NaN sample would be written as 0 by Node; the mock never produces one.) The header uses `sampleRate × 2` and block align 2, so it is only correct for mono 16-bit, the only kind it writes.

### `readWavInfo(bytes) -> Option<WavInfo>` (`wav.ts:29-68`)

`WavInfo { format, channels, sampleRate, bitsPerSample, dataOffset, dataBytes, durationSec }`. It walks the RIFF chunks so files with extra chunks (`LIST`, `fact`, …) work.
```text
if len < 12 or bytes[0..4] != "RIFF" or bytes[8..12] != "WAVE": None
fmt = None; at = 12
while at + 8 <= len:
    id = bytes[at..at+4]; size = u32le(at+4); body = at + 8
    if id == "fmt " and body + 16 <= len:
        fmt = { format: u16le(body), channels: u16le(body+2), sampleRate: u32le(body+4), bitsPerSample: u16le(body+14) }
    elif id == "data" and fmt is Some:
        dataBytes  = min(size, len - body)          # tolerates 0xFFFFFFFF / truncated streaming headers
        frameBytes = channels * bitsPerSample / 8   # may be fractional
        durationSec = (frameBytes > 0 and sampleRate > 0) ? dataBytes / frameBytes / sampleRate : 0
        return { ...fmt, dataOffset: body, dataBytes, durationSec }
    at = body + size + (size % 2)                   # chunks are word aligned
None   # also when "data" comes before "fmt " (that data chunk is skipped)
```
Use 64-bit arithmetic for `at` (`body + size` can overflow u32). A later `fmt ` chunk overrides an earlier one. A `fmt ` chunk is parsed whenever 16 bytes after its header fit in the **buffer** (its declared size is not checked); otherwise it is skipped. The `format` field is not checked, so float or compressed WAVs get a duration computed as if they were PCM. Verified on the mock: `{format:1, channels:1, sampleRate:22050, bitsPerSample:16, dataOffset:44, dataBytes:26460, durationSec:0.6}`.

Used by `chunkDuration` for `audio/wav` (in practice, mock audio; the offered output formats are all MP3).

---

## 7. Consumer contract: alignment → character and word timing (cross-reference; owned by the widget spec)

From the engine header (`engine.ts:3-6`): ElevenLabs alignment is relative to each chunk's text but counts characters its own way (code points, after its own whitespace handling), so `alignment.chars.len()` may differ from `textEnd − textStart`. **Chunks are delivered as-is. The player maps alignment indices onto the chunk's text range proportionally when the counts differ.** Mock alignment always matches 1:1.

For completeness, the player's mapping (`src/renderer/src/widget/timing.ts:24-74`), with `len = textEnd − textStart`, `n = starts.len()`:
- Usable alignment: `n == len`, or `n > 1 && len > 1`. Otherwise use the linear fallback.
- `charAtTime(t)`: `i = max(0, last index with starts[i] <= t)` (binary search); `local = (n == len) ? i : floor(i*(len−1)/(n−1))`. Fallback: `floor(t / durationSec * len + 1e-9)`. Result `textStart + clamp(local, 0, len−1)`.
- `timeAtChar(c)`: `local = clamp(c − textStart, 0, len−1)`; `i = (n == len) ? local : min(n−1, ceil(local*(n−1)/(len−1)))`; return `starts[i]`. Fallback: `durationSec * local / len`.
- Words are non-whitespace runs (`\S+`) of the session text; the highlighted word is the one containing `charAtTime`.

This is why the engine must deliver `textStart`/`textEnd` in the **same unit** the player uses to index `text`.

---

## 8. JS/Chromium behaviors that were implicit (reproduce deliberately)

| Area | Original behavior | Rust note |
|---|---|---|
| String units | `length`, indices and `slice` are UTF-16 code units; `text.split('')` splits surrogate pairs | Choose one unit (see the warning at the top); every limit in §2–§5 is in that unit |
| Regex `\s` | ECMAScript whitespace: `[\t\n\v\f\r \u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF]` | Rust `\s` = Unicode `White_Space` (adds U+0085, drops U+FEFF). Use an explicit class for parity. Where it matters: `cleanText` steps 2\u20135, `validateApiKey`, and the key/raw-text trims. After step 5, only `' '`/`'\n'` remain (\u00A73.1 whitespace invariant) |
| Regex `\w`, `\d`, `\b` | ASCII-only even with the `u` flag (`[A-Za-z0-9_]`, `[0-9]`, ASCII word boundary) | Rust defaults are Unicode. Use `[A-Za-z0-9_]`, `[0-9]` and `(?-u:\b)` (or emulate the boundary with lookaround) |
| Lookaround and backreferences | Backreference in `RULE` (`\1`); lookahead in bold/em (`(?=`, `(?!`) and `EMAIL_RE` (`(?!`); lookbehind in `URL_RE` and `EMAIL_RE` (`(?<!`) | The `regex` crate lacks them. Use `fancy-regex` (backtracking like V8, so the linear-time pattern designs still apply) or hand-written scanners |
| `[` inside a class | Literal in JS (`[^[\]\n]`) | Opens a nested class in Rust `regex`/`regex-syntax`: escape it (`[^\[\]\n]`). `&&`, `--` and `~~` inside a class are set operators in Rust |
| `\b`/`\w` with flags `iu` (`URL_RE`) | Also treat U+017F and U+212A as word chars (case folding) | Negligible; `(?-u:\b)` is close enough |
| `\p{Script=…}`, `\p{L}`, `\p{Lu}`, `\p{Ll}`, `\p{N}` | Unicode properties | Supported by `regex`/`fancy-regex` (`\p{Han}`, `\p{sc=Han}`, `\p{Script=Han}`) |
| Case-insensitive `URL_RE` | `i` flag, so `HTTPS://`/`WWW.` match | `(?i)` |
| `$`/`^` with the `m` flag | Match at `\n` (and other JS line terminators, already normalized away) | `(?m)` |
| `String.prototype.trim` | Strips ECMAScript whitespace + line terminators (includes U+FEFF, excludes U+0085) | `str::trim` differs slightly; use a custom predicate for parity |
| `String.replace(str, str)` | Replaces the **first** occurrence only (`urlTrailing`) | `replacen(…, 1)` |
| Sentence segmentation | `Intl.Segmenter('en', {granularity:'sentence'})` = ICU4C UAX #29, no abbreviation suppression (plain `en`, not `en-u-ss-standard`); an empty string yields no segments | `icu_segmenter::SentenceSegmenter` (ICU4X, same rules; returns UTF-8 byte boundaries starting with 0; skip empty spans) or `unicode-segmentation`'s sentence bounds. Validate against the §3.7 table |
| Voice name sort | `localeCompare(…, undefined, {sensitivity:'base', numeric:true})` (ICU collator, default locale) | `icu_collator` with `Strength::Primary` and numeric ordering on, root or system locale |
| `Number(str)` for headers | Trims; `""` → 0; `"0x10"` → 16; `"1e3"` → 1000; `"Infinity"` → not finite; garbage → NaN | Parse a trimmed `f64` (Rust also accepts `inf`/`nan`, which the finiteness check rejects; hex is the only JS form Rust's parser lacks); accept finite values only (`> 0` for `retry-after`, `>= 0` for cost) |
| Base64 | `Buffer.from(s,'base64')` is lenient: skips whitespace and invalid chars, accepts both alphabets even mixed, stops at the first `=`, drops a dangling sextet (§2.6) | Normalize as in §2.6, then a `GeneralPurpose` engine with `DecodePaddingMode::Indifferent` + `decode_allow_trailing_bits(true)` |
| HTTP | Node `fetch` (undici): redirects, gzip/deflate, keep-alive, Mozilla CA bundle, **no system proxy**, default `user-agent: node` | `reqwest` + tokio (`gzip`/`deflate` features); per-request `.timeout()` (covers the body read); decide on proxy/CA policy and set a real user agent |
| `encodeURIComponent` / `URLSearchParams` | Path: space → `%20`, keeps `A-Za-z0-9-_.!~*'()`; query: space → `+`, keeps only `A-Za-z0-9*-._` | `percent-encoding` with matching sets, or `url::Url::query_pairs_mut()` (form-urlencoded, same as `URLSearchParams`) |
| `Math.round` | Halves toward +∞ (`-0.5` → `-0`) | `f64::round` rounds halves away from zero; use `(x + 0.5).floor()` where negatives occur (`encodeWav`) |
| Cancellation | `AbortController` / `AbortSignal.any([caller, timeout])`; abortable `setTimeout` | `tokio_util::sync::CancellationToken` + `tokio::select!`; dropping the reqwest future aborts the connection |
| Async ordering | Microtask yield before the first sink/event; `queueMicrotask` for `stopped` | Spawn the session task; send `session_start` on the (FIFO) sink channel before spawning; send events through a channel. Keep the §4.4 order: the old session's `stopped` before any chunk or event of the new one |
| Listener exceptions | A throwing event listener inside `run` causes `crashed`; inside the `stopped` microtask it is uncaught | Catch and log listener failures; never let them abort the engine task |
| `crypto.randomUUID()` | v4 UUID for session ids | `uuid::Uuid::new_v4()` |
| SHA-256 | `node:crypto` | `sha2` + `hex` |

---

## 9. Master constants table

| Constant | Value | Source |
|---|---|---|
| Base URL | `https://api.elevenlabs.io` | constants.ts:6 |
| Synthesis timeout | `max(30 000 ms, 50 ms × chars)` | elevenlabs.ts:20-24,452 |
| List/subscription timeout | 15 000 ms | elevenlabs.ts:21 |
| Voices / models / account TTL | 10 min / 1 h / 60 s | elevenlabs.ts:25-27 |
| Max voice pages × page size | 25 × 100 | elevenlabs.ts:28,525 |
| Param-fallback extra requests | ≤ 2 (3 total) | elevenlabs.ts:611-619 |
| Error message cap | 300 chars | elevenlabs.ts:125,221 |
| Voice-setting clamps (fallback) | stability 0..1 (0.5), similarity 0..1 (0.75), style 0..1 (0), speed 0.7..1.2 (1) | elevenlabs.ts:436-442 |
| First chunk / later chunk max | 220 / 900 chars (min first 20) | text.ts:17-18,383-384 |
| Split search floor | 35 % of limit | text.ts:356 |
| Abbreviation window | 24 chars | text.ts:250 |
| Title-line heuristic | < 40 chars | text.ts:147 |
| `truncateAtBoundary` floor | 60 % | text.ts:209 |
| Engine retries | 3, delays 1 s / 2 s / 4 s, `retry-after` cap 10 s | engine.ts:61-63 |
| Look-ahead | 120 s of unplayed audio; first 2 chunks free | engine.ts:74-75 |
| Context window | 300 chars, word-boundary trim window 60 | engine.ts:64,402-417 |
| Duration tolerance (bytes vs alignment) | +2 s | engine.ts:431 |
| Estimated speech rate | 15 chars/s | constants.ts:17 |
| Mock latency | 350 ms first, 120 ms next | engine.ts:62 |
| Mock audio | 22 050 Hz, 220 Hz, amp 0.015, fade 0.03 s (= 662 samples), min 0.6 s | mock.ts:7-11 |
| Parse-error raw message cap (non-JSON bodies) | 300 chars | elevenlabs.ts:125 |
| Parameter-fallback trigger statuses | 400, 422 | elevenlabs.ts:471 |
| v2 voices fallback: statuses that do **not** fall back | 401, 402, 403, 429 (and any non-4xx or status-less error) | elevenlabs.ts:566-568 |
| Context trim windows | before: first space within 60 units of the 300-unit tail; after: last space after unit 240 | engine.ts:402-417 |
| Clause-split floor / hard-cut guard | 35 % of limit; cut never ≤ span start | text.ts:356,370 |
| API key length cap (caller) | 256 chars | ipc.ts:314 |
| Raw pre-cut (caller) | `max(maxChars × 4, 200 000)` | reader.ts:47-48,316 |
