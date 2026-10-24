# Vox port spec — Integrations: browser bridge, browser extension, Explorer verb, launch at login, file text extraction

Source of truth: `C:/Users/Kwaku/Desktop/Desktop Widget` (Electron + TypeScript). Every behavior below is cited as
`file:line`. Paths are relative to that root.

| Piece | Old files | Section |
|---|---|---|
| Local HTTP bridge (server the extension talks to) | `src/main/integrations/local-server.ts`, glue in `src/main/index.ts:213-289` | §1 |
| Browser extension (Chrome/Edge MV3, **ships unchanged**) | `browser-extension/manifest.json`, `bridge.js`, `background.js`, `popup.js`, `popup.html`, `README.md` | §2 |
| Explorer "Read with Vox" verb | `src/main/integrations/explorer-menu.ts`, glue `src/main/index.ts:185-211`, `resources/installer.nsh` | §3 |
| Launch at login | `src/main/integrations/autostart.ts`, glue `src/main/index.ts:176-183` | §4 |
| File → plain text for the Explorer verb / `--read-file` | `src/main/integrations/text-extract.ts`, caller `src/main/reader.ts:454-478` | §5 |

Conventions used throughout:

- **Lengths are UTF-16 code units** (JavaScript string length) unless stated otherwise, like the other port specs.
  Use `s.encode_utf16().count()` (and cut on UTF-16 indices, never splitting a surrogate pair) where parity matters.
- "JS whitespace" (`\s`, `trim()`) = U+0009–U+000D, U+0020, U+00A0, U+1680, U+2000–U+200A, U+2028, U+2029, U+202F,
  U+205F, U+3000, **U+FEFF**. Rust's `char::is_whitespace` / `str::trim` lack U+FEFF (and include U+0085). Where a rule
  below says `\s` or `trim`, use the JS set (a small `is_js_ws(c)` helper).
- All copy strings are exact, including typographic characters (— U+2014, – U+2013, → U+2192, ’ U+2019).
- Wire-level behavior of §1 was verified by running the original server (transpiled) against raw TCP requests; the
  responses quoted in §1.10 are captured output, not paraphrase. A second review re-ran the server (Node 24) and
  the text extractors against extra edge cases; everything marked "verified" is captured behavior, and the
  launch-at-login parsing claims in §4.2 were checked against Electron 44.7.0 itself (read-only calls).

---

## 1. Local HTTP bridge

Purpose: the browser extension (§2) cannot reach the desktop app any other way, so the app runs a tiny JSON HTTP
server on loopback. Web pages must never be able to drive it (local-server.ts:1-9).

### 1.1 Constants

| Name | Value | Source |
|---|---|---|
| Bind address | `127.0.0.1` (IPv4 loopback only; `::1` is **not** bound — a connect to `[::1]:port` is refused) | local-server.ts:52, :159 |
| Default port | `47823` (`LOCAL_SERVER_DEFAULT_PORT`) | shared/constants.ts:12, shared/defaults.ts:79 |
| Port setting | `settings.server.port`, integer, sanitized to 1024..65535 (rounded, clamped) | settings-store.ts:168 |
| Client header name | `x-vox-client` (header names are case-insensitive) | shared/constants.ts:13 |
| Client header value | `browser-extension` (exact, **case-sensitive**) | shared/constants.ts:14 |
| Max request body | 2 MiB = `2 * 1024 * 1024` = 2 097 152 bytes | local-server.ts:48 |
| Body / handler deadline (`timeoutMs`) | 15 000 ms | local-server.ts:49 |
| Whole-request hard cap (Node `requestTimeout`) | `timeoutMs + 1000` = 16 000 ms | local-server.ts:120 |
| Header deadline (Node `headersTimeout`) | `min(10 000, timeoutMs)` = 10 000 ms | local-server.ts:121 |
| Timeout check granularity | 1 000 ms (Node `connectionsCheckingInterval`; default 30 s would make the timeouts coarse) | local-server.ts:122-123 |
| Keep-alive idle timeout | 5 000 ms | local-server.ts:132 |
| Max simultaneous connections | 64 (extra connections are closed immediately) | local-server.ts:133 |
| Socket inactivity timeout | `timeoutMs + 5000` = 20 000 ms → socket destroyed | local-server.ts:136 |
| Max header block | 16 KiB (Node default `maxHeaderSize`, not overridden) → raw 431 | local-server.ts:144 |
| `url` max length | 2048 | local-server.ts:50 |
| `title` max length | 300 (after trim) | local-server.ts:51 |
| App name / version in `/health` | `"Vox"` / app version (`package.json` `"0.1.0"`) | index.ts:255-259 |

### 1.2 Listener lifecycle (local-server.ts:85-186)

State: `server` (listening or none), `boundPort` (actual port while running, else 0), `requestedPort` (last port
asked for), a set of open sockets.

- `running` = a listener exists. `port` = `boundPort` while running, else `requestedPort` (local-server.ts:85-92).
- **All `start`/`stop` calls are serialized** through one promise queue; a failing op never blocks later ones
  (local-server.ts:70-71, 104-108). Reason: rapid settings changes (toggle + port edit) must never race two
  listeners. In Rust: one `tokio::sync::Mutex` (or a command channel to a single owner task).
- `start(port)` never fails/throws; it returns `{ ok, port, error? }`:
  1. If `port` is not an integer in `0..=65535` → `{ ok:false, port, error: "Invalid port <port> (use 1024–65535)" }`
     (note: the check allows 0..65535 though the message says 1024; port 0 = OS-chosen ephemeral port, used by tests).
     `<port>` is the JS `String(port)`, so `1.5` → `Invalid port 1.5 (use 1024–65535)` (verified). An invalid port
     does **not** update `requestedPort` and does not touch a running listener.
  2. `requestedPort = port`.
  3. Already running, `port != 0` and `port == boundPort` → `{ ok:true, port }` (no restart). Port 0 always restarts.
  4. Running on another port → stop first (below), then bind anew. **If the new bind then fails, the bridge stays
     stopped** (the old listener is already gone; `running` = false, `port` = the failed port).
  5. Bind `127.0.0.1:port`. Node's `exclusive: true` only disables *cluster handle sharing* between Node worker
     processes; it is **not** a socket option (libuv deliberately does not set `SO_REUSEADDR` for TCP on Windows,
     where it would allow port stealing). On bind error → close, log warn `bridge: <error>`, return
     `{ ok:false, port, error }` with error text mapped from the OS error:

     | OS error | WinSock | `error` string |
     |---|---|---|
     | `EADDRINUSE` | 10048 | `Port <port> is already in use by another program` |
     | `EACCES` | 10013 (e.g. Hyper-V/WSL excluded port ranges) | `Port <port> is reserved or blocked by Windows — choose another port` |
     | `EADDRNOTAVAIL` | 10049 | `The loopback address 127.0.0.1 is not available` |
     | anything else | | the OS error message |

     (local-server.ts:399-410)
  6. On success: errors raised later by the listener are only logged (`bridge: <message>`), never fatal
     (local-server.ts:167-168); `boundPort` = actual bound port; log info `bridge: listening on http://127.0.0.1:<port>`;
     return `{ ok:true, port: boundPort }`.
- `stop()`: if running → forget the listener, `boundPort = 0`, close the listening socket, **forcibly close every open
  connection** (in-flight requests included), resolve when closed (local-server.ts:175-186). Never fails.

Rust binding advice: bind with `socket2` and set `SO_EXCLUSIVEADDRUSE` (Windows) so no other process can bind the
same port on top of ours (a hardening beyond parity); never set `SO_REUSEADDR`. Map
`WSAEADDRINUSE`/`WSAEACCES`/`WSAEADDRNOTAVAIL` to the strings above. Note `std::net::TcpListener::bind` sets
nothing special on Windows either, but use `socket2` so the option is set **before** `bind`.

### 1.3 App-level glue (index.ts:226-289)

- Wanted state: `settings.triggers.browserExtension && !screenshotMode` (default `browserExtension = true`). Port:
  `settings.server.port`. `syncServer` runs at startup (index.ts:501) and whenever `triggers.browserExtension` or
  `server.port` changes (index.ts:305-307). Calls are serialized by a second queue (index.ts:266-289):
  not wanted → `stop()` if running; wanted → create the server lazily; if running on the same port → nothing; if
  running on another port → `stop()`; then `start(port)`.
  - Success → log `bridge listening on 127.0.0.1:<port>`.
  - Failure → log warn `bridge failed on port <port>: <error or "unknown error">` and, unless demo mode, error toast
    **`Browser bridge couldn't use port <port>. Pick another port in Settings`** (no trailing period) with action
    button `Open settings` → route `{ page: "settings", section: "integrations" }` (index.ts:123-130, 280-283). All
    main-process toasts wait for the widget window to be ready first (≤ 5 s, see below) and are dropped if the reader
    does not exist yet.
  - Any exception inside a sync step → log error `bridge sync failed` (never propagates).
  - "Demo mode" = env `VOX_DEMO` set **or** screenshot mode (log.ts:27); screenshot mode never runs the bridge at all.
- Shutdown stops the server as one of the parallel shutdown tasks (index.ts:414).
- `AppInfo.serverPort` = the server's `port` (bound port while running, else the last requested port) when a server
  object exists and that value is non-zero, else `settings.server.port`; `AppInfo.serverRunning` = server running
  (ipc.ts:288-289). Settings shows a badge `Listening on <port>` /
  `Not running` / `Off` / `Checking…`, and under the port field `Port <port> may be in use by another app. Try another
  one.` when enabled but not running (IntegrationsSection.tsx:39-51, 174).
- Handlers given to the server:
  - `getStatus()` → `{ appName: "Vox", version: <app version>, playing: playerState.status == "playing" }`
    (`loading` and `paused` report `false`) (index.ts:255-259).
  - `onRead(req)` → `bridgeRead` (index.ts:226-245):
    1. Reader not constructed yet → resolve `{ ok:false, code:"unknown", error:"Vox is still starting" }`.
    2. Foreground window info (process exe name lower-cased + friendly app name, see selection spec); any error → none.
       Keep it only if the process is one of
       `chrome.exe msedge.exe brave.exe opera.exe vivaldi.exe arc.exe chromium.exe yandex.exe firefox.exe`
       (reason: the request arrives while the browser is normally in front; anything else — our own window, a
       terminal — would mislabel the read).
    3. Start a read: `source = "browser-extension"`, `sourceApp = fg.appName || "Browser"`,
       `sourceProcess = fg.processName` (absent if not a browser), `sourceTitle = req.title` (the reader trims and
       cuts it to 200), `sourceUrl = req.url` only if it is an `http:`/`https:` URL of ≤ 4096 chars that parses
       (`isHttpUrl`, ipc.ts:102-110) — so a `file://` URL accepted by the server is dropped here.
    4. Return the reader's `ReadResult` (see §1.6.2). The reader resolves once the TTS session has **started**
       (after waiting for the player window to be ready — `whenWidgetReady(timeoutMs = 5000)`, a race that gives up
       after 5 s and continues anyway, windows.ts:650-653), not when synthesis finishes (reader.ts:305-387). So a
       normal `/read` answers within ~5 s, well inside the 15 s handler deadline.
    - The reader is constructed in bootstrap *before* `syncServer` first runs (index.ts:445, 501), so the
      "still starting" branch is effectively unreachable, and `getStatus().playing` is always a real boolean (in JS a
      missing reader would make `playing` `undefined` and drop the key from the JSON; the Rust port must always send
      a bool).
  - `onCommand(cmd)` → re-validated by the IPC validator `ipc.ts:118-145` (same rules except it silently drops an
    invalid `amount` instead of erroring; irrelevant because the server already normalized it) → `reader.command(cmd)`
    if valid (index.ts:251-254). Synchronous; the HTTP answer does not wait for the player.

### 1.4 Request pipeline — exact order of checks (local-server.ts:188-227)

Every request goes through these steps in this order; the first failing step answers. "JSON error" means the body
`{"ok":false,"error":"<message>"}` plus `,"code":"<code>"` only when a code is listed.

| # | Check | Failure status | Error message | code | Extra headers |
|---|---|---|---|---|---|
| 0 | HTTP parse (malformed request line/headers, unknown or lower-case method) | raw **400** | *(no body)* | | see §1.7 |
| 0 | Header block > 16 KiB | raw **431** | *(no body)* | | see §1.7 |
| 1 | Method is `OPTIONS` (any path, any headers) | **403** | `Forbidden` | | — (deliberately **no** CORS headers) |
| 2 | `Host` (trim, lower-case) is exactly `127.0.0.1:<boundPort>` or `localhost:<boundPort>` | **403** | `Forbidden host` | | |
| 3 | `Origin` (trim, lower-case), if present and non-empty, is `null` or starts with `http:`, `https:` or `file:` | **403** | `Forbidden origin` | | |
| 4 | Request-target parses to a path | **400** | `Bad URL` | | |
| 5 | Path `/health`: method must be `GET` or `HEAD` | **405** | `Method not allowed` | | `Allow: GET` |
| 5 | Path `/health` → **200** (§1.6.1) | | | | |
| 6 | Path is `/read` or `/command`, else | **404** | `Not found` | | |
| 7 | Method must be `POST` | **405** | `Method not allowed` | | `Allow: POST` |
| 8 | Header `x-vox-client` equals `browser-extension` exactly | **403** | `Forbidden` | | |
| 9 | `Content-Type` (lower-cased) matches `^application/json\s*(;\|$)` | **415** | `Content-Type must be application/json` | | |
| 10 | Declared `Content-Length` > 2 097 152 | **413** | `Request body too large` | `too-long` | `Connection: close` |
| 11 | Body received bytes exceed 2 097 152 (also chunked bodies) | **413** | `Request body too large` | `too-long` | `Connection: close` |
| 11 | Body not complete within 15 s of starting to read it | **408** | `Request timed out` | | `Connection: close` |
| 11 | Body stream error | **400** | `Request stream error` | | |
| 11 | Client aborted mid-body | **400** | `Request aborted` | | (client is gone) |
| 12 | Body (UTF-8, lossy; one leading U+FEFF stripped) is valid JSON | **400** | `Invalid JSON` (an empty body is invalid JSON) | | |
| 13 | JSON value is an object (not array/null/string/number/bool) | **400** | `Expected a JSON object` | | |
| 14 | Route-specific validation (§1.6.2 / §1.6.3) | **400** | per route | per route | |
| 15 | `/read` handler answers within 15 s | **504** | `Timed out waiting for the app` | `unknown` | |
| 16 | Handler throws / rejects (`/read` promise rejects, or `/command`'s synchronous `onCommand` throws) | **500** | the thrown error's message | `unknown` | |
| 17 | Anything escaping the handler entirely (logged error `bridge: unhandled <message>`) | **500** | `Internal error` | `unknown` | |

Notes on individual steps:

- **Step 1** comes before the Host/Origin checks: every preflight is refused, so a browser never sends the real
  cross-origin request (the custom `x-vox-client` header guarantees a preflight for any web page).
- **Step 2** compares against the *bound* port (matters for port 0). `Host: 127.0.0.1` without a port fails; a
  missing Host (HTTP/1.0) fails (verified: HTTP/1.0 request without Host → 403 `Forbidden host`); whitespace around
  the value is ignored; `LOCALHOST:<port>` passes (lower-cased). Plain string compare, so `localhost.:<port>`,
  `[::1]:<port>` and `127.0.0.1:0<port>` all fail (verified for the first two).
- **Duplicate headers** (Node semantics, verified): for `Host` and `Content-Type` (and `Content-Length`) Node keeps
  the **first** value and drops later duplicates (`Host: 127.0.0.1:$P` + `Host: evil:$P` → 200; `Content-Type:
  application/json` + `Content-Type: text/plain` → accepted). Other headers — `Origin`, `x-vox-client` — are joined
  with `", "` in arrival order before the checks run. Rust (`http::HeaderMap` keeps all values): use the first value
  for Host/Content-Type; for Origin/x-vox-client join all values with `", "`.
- **Step 3**: a `chrome-extension://…` (or any non-web scheme, e.g. `moz-extension://`) Origin passes. No Origin
  and an empty/whitespace-only Origin pass. Test = trimmed, lower-cased value equals `null` or matches
  `^(https?|file):` (so ` HTTPS://X ` fails, verified). Duplicate Origins are joined first, so only the **first**
  one decides: `chrome-extension://a` + `http://x` → 200; `http://x` + `chrome-extension://a` → 403 (verified).
- **Step 4** (`parsePath`, local-server.ts:356-363): the request-target is resolved as a WHATWG URL against
  `http://127.0.0.1`; only its *pathname* is used (query string ignored; absolute-form `http://evil/health` routes
  as `/health`; dot segments are normalized, `/x/../health` → `/health`; percent-escapes are **not** decoded,
  `/%68ealth` → 404, `/health%2F` → 404). If the pathname is longer than 1 char, all trailing `/` are stripped
  (`/health/` → `/health`; `/health//` → `/health`). Routing is case-sensitive (`/HEALTH` → 404). More verified
  WHATWG-URL consequences: a `#fragment` is dropped (`/health#x` → 200); `\` counts as `/` (`/health\` → 200);
  a target starting with `//` is parsed as an *authority* — `//evil/health` → path `/health` → 200, while `//` and
  `///` (empty host) make the URL parser throw → **400 `Bad URL`**. These are the only practical ways to reach
  `Bad URL`. For a Rust port it is enough to: if the target starts with `//`, take the part after the next `/` as the
  path (empty host → 400 `Bad URL`); drop `?query` and `#fragment`; map `\` to `/`; strip trailing slashes; compare
  exactly. Dot-segment handling is optional (the extension only ever sends `/health`, `/read`, `/command`).
- **Step 5**: `/health` does not need the client header, does not check Content-Type and never reads a body.
  `HEAD /health` returns the same status and headers (including the JSON's `Content-Length`) with no body.
  `HEAD` on any other path gets that path's error status with no body.
- **Step 8**: duplicate `x-vox-client` headers are joined by Node as `browser-extension, browser-extension`, which
  fails (verified 403). Value `Browser-Extension` fails.
- **Step 9** examples: `application/json` ✓, `Application/JSON; charset=utf-8` ✓, `application/json ;x` ✓,
  `application/json<TAB>;x` ✓ (JS `\s`), missing ✗, `text/plain` ✗, `application/jsonx` ✗,
  `application/vnd.api+json` ✗ (verified). The charset parameter is ignored (body is always decoded as UTF-8).
- **Order of 403/415/413 for POST**: client header (403) is checked before Content-Type (415), which is checked before
  the declared length (413); e.g. `POST /read` with `text/plain` and no `x-vox-client` → 403 (verified). Any method
  other than POST on `/read`/`/command` (`PUT`, `PATCH`, `DELETE`, `GET`, `HEAD`) → 405 `Allow: POST`; on `/health`
  anything but GET/HEAD → 405 `Allow: GET`.
- **Steps 8–10 fail without reading the body.** Node drains/discards it; a Rust server must either drain it or close
  the connection after responding (simplest: add `Connection: close` to any error sent before the body was read).
- **Step 11**: after a 413/408/400 raised while reading, the remainder of the upload is discarded (`req.resume()`) so
  the client can read the response (local-server.ts:257-259). `Transfer-Encoding: chunked` bodies are accepted
  (verified). The size check counts raw received bytes (before decoding). The 15 s body timer starts when the body
  read starts, i.e. right after the header checks — **but** Node's whole-request timer (16 s from the first byte of
  the request) also runs: if the header block itself took more than ~1 s to arrive and the body then stalls, the
  16 s timer fires first and the client gets the **raw 400** of §1.7, not the JSON 408 (verified with scaled-down
  timeouts: headers dribbled over 1.6 s + stalled body → raw 400 at `timeoutMs + 1000`). Parity is only required for
  the common case (headers in one packet → JSON 408 at 15 s).
- **`Expect: 100-continue`**: Node answers `HTTP/1.1 100 Continue` automatically as soon as the headers arrive, before
  any check, then the real response (verified). hyper sends 100 only when the body is first polled, so error
  responses produced before reading the body go out without a 100 — harmless (the extension never sends `Expect`).
- **Step 12**: decoding is lossy UTF-8 (invalid bytes → U+FFFD, then parsed; verified `{"text":"a<0xFF>"}` → text
  `"a\uFFFD"`). Rust: `String::from_utf8_lossy`, strip one leading `'\u{FEFF}'`, then `serde_json::from_str` into a
  `serde_json::Value` (do the field validation by hand so messages match). JSON.parse details that differ from
  `serde_json` and **must** be handled:
  - **Lone surrogate escapes are accepted.** `{"text":"a\ud83d"}` and `{"title":"\udc00x"}` → 200, the handler gets a
    string containing the lone UTF-16 unit (verified). The extension really sends these: `bridge.js` cuts `text` to
    200 000 and `title` to 300 **UTF-16 units**, which can split an emoji, and `JSON.stringify` then emits
    `"\ud83d"`. `serde_json` rejects lone surrogates in `\u` escapes when building a `String` ("unexpected end of hex
    escape" / "lone leading surrogate in hex escape"), so a
    naive Rust server would answer 400 `Invalid JSON` and the read would fail. Fix: before parsing, rewrite every
    `\uD800`–`\uDBFF` escape not immediately followed by a `\uDC00`–`\uDFFF` escape, and every unpaired
    `\uDC00`–`\uDFFF` escape, to `\uFFFD` (scan escapes properly: in `\\ud83d` the `\\` is an escaped backslash and
    `ud83d` is plain text), or use a lenient JSON parser. Getting U+FFFD instead of the lone unit is fine.
  - Duplicate keys: last one wins (`{"type":"jump","type":"stop"}` → `stop`, verified); `serde_json::Value` behaves
    the same.
  - Numbers are IEEE doubles: integers beyond i64/u64 parse as floats (`positionSec: 1.2e29` accepted, verified);
    `1.0` equals `1` (`direction: 1.0` accepted, verified). Compare via `as_f64()`. `-0` → `max(0,-0)` → sent as `0`.
  - Overflowing literals (`1e400`) become `Infinity` in JS, so the route check answers 400
    `"rate" must be a finite number` (verified); `serde_json` instead fails the whole parse ("number out of range")
    → `Invalid JSON`. Accept the difference or pre-handle; the extension never sends such numbers.
  - JSON whitespace (space, TAB, LF, CR) around the value is allowed (` \n {…} \n` → 200, verified).
- **Step 15**: the 15 s handler timer starts after the body was parsed (independent of the body timer). The read
  itself is **not** cancelled on 504; it may still start later.
- Logging (local-server.ts:219-225): `HttpError`s with status ≥ 500 (in practice only 504) are logged as warn
  `bridge: <status> <message>`; 4xx are not logged. Non-`HttpError` handler failures (step 16) are logged as error
  `bridge: handler failed: <message>`.

### 1.5 Response format (local-server.ts:370-381)

Every response produced by the router (all rows above except the raw 400/431) has:

```
HTTP/1.1 <status> <reason>
Content-Type: application/json; charset=utf-8
Content-Length: <UTF-8 byte length of the JSON>
Cache-Control: no-store
X-Content-Type-Options: nosniff
[Allow: GET | Allow: POST]          (405 only)
[Connection: close]                 (408/413, or when the client asked for close)

<compact JSON, UTF-8, non-ASCII not escaped>
```

- **Never** any `Access-Control-*` header. The extension does not need them (host permission bypasses CORS, §2.1)
  and their absence is what keeps web pages out.
- A response is only written once; if headers were already sent or the socket is gone, later attempts are dropped.
- Node also adds `Date`, and `Connection: keep-alive` + `Keep-Alive: timeout=5` on persistent connections; these
  are not relied on by the extension.
- Error body: `{"ok":false,"error":"<message>"}` or `{"ok":false,"error":"<message>","code":"<ReadErrorCode>"}`.
  Key order `ok, error, code` (irrelevant to clients, but nice for golden tests).

### 1.6 Routes

#### 1.6.1 `GET /health` (and `HEAD /health`)

No header, no body required. Response 200:

```json
{"ok":true,"app":"Vox","version":"0.1.0","playing":false}
```

| Field | Type | Meaning |
|---|---|---|
| `ok` | `true` | always true on 200 |
| `app` | string | product name (`APP_NAME` = `"Vox"`) |
| `version` | string | app version string |
| `playing` | bool | `true` only while the player status is `playing` |

The extension popup polls this every 2.5 s with a 1.5 s timeout and shows `"<app> <version>"`, a "Now playing"
label and the pause/resume icon from `playing` (§2.4). SPEC.md omitted `playing`; the code sends it and the popup
uses it — keep it.

#### 1.6.2 `POST /read`

Headers required: `x-vox-client: browser-extension`, `Content-Type: application/json`.

Request body (any extra fields are ignored):

```ts
{ text: string,        // required; must contain a non-whitespace char (JS trim)
  url?: string,        // optional; kept only if valid (below), otherwise silently dropped
  title?: string }     // optional; kept trimmed, ≤300, only if non-empty after trim
```

Validation (local-server.ts:309-317), in order:

1. `text` not a string → 400 `"text" must be a string`, code `empty-text`. (Message includes the double quotes:
   JSON-escaped as `"\"text\" must be a string"`.)
2. `text.trim()` empty (JS trim, so `"\uFEFF\u00A0"` counts as empty — verified) → 400
   `Nothing to read — the text is empty`, code `empty-text`.
3. `url` is kept iff it is a string, length ≤ 2048 UTF-16 units, and matches `/^(https?|file):\/\//i`
   (case-insensitive; so `FILE:///c:/a.txt` and `HTTPS://X` are kept verbatim, `https:example.com` and
   `javascript:…` are dropped). It is not otherwise parsed or normalized. A non-string `url`/`title` (e.g. `5`) is
   silently dropped, never an error (verified).
4. `title` is kept iff it is a string whose trim is non-empty; stored as `title.trim()` cut to its first 300 UTF-16
   units (a cut can split a surrogate pair in the old code; in Rust cut on a char boundary at ≤ 300 units).
5. The text itself is passed through untouched (no length limit beyond the 2 MiB body; the reader pre-cuts to
   `max(maxChars × 4, 200 000)` and truncates to `settings.text.maxChars`, toasting
   `Reading the first <maxChars formatted en-US> characters`).

Then the handler runs with a 15 s deadline (step 15). Its `ReadResult` is returned **with HTTP 200 whatever `ok`
is** — application-level refusals are 200 + `ok:false`:

| Situation | Body (HTTP 200) |
|---|---|
| Read started | `{"ok":true,"readId":"<uuid v4>"}` |
| App still starting | `{"ok":false,"code":"unknown","error":"Vox is still starting"}` |
| No ElevenLabs key (also toasts `Add your ElevenLabs API key to start reading` with `Add key` → settings/account) | `{"ok":false,"code":"no-api-key","error":"No ElevenLabs API key"}` |
| Nothing left after text cleaning (also toasts `Nothing to read in that selection`) | `{"ok":false,"code":"empty-text","error":"Nothing to read"}` |
| TTS engine failed to start | `{"ok":false,"readId":"<uuid>","code":"unknown","error":"<engine message>"}` |

(reader.ts:305-387, index.ts:226-227). The extension treats any `ok !== true` as a failure and shows `error`
(§2.2), so field names `ok` and `error` are load-bearing; `readId`/`code` are informational. Key order as produced
by the reader: `ok, readId` / `ok, code, error` / `ok, readId, code, error`. The API-key check comes first (before
any text cleaning) and is skipped in mock-TTS mode (`VOX_MOCK_TTS=1` or demo).

#### 1.6.3 `POST /command`

Headers as for `/read`. Body is a `PlayerCommand`; validated and normalized to a copy containing only known fields
(local-server.ts:320-352). Response on success: 200 `{"ok":true}` (even if nothing is playing).

| `type` | Required fields | Normalization | Error (400, no code) |
|---|---|---|---|
| `toggle`, `play`, `pause`, `stop` | — | other fields dropped | — |
| `seek` | `positionSec` finite number | `max(0, positionSec)` | `"positionSec" must be a finite number` |
| `seekFraction` | `fraction` finite number | clamp 0..1 | `"fraction" must be a finite number` |
| `skip` | `direction` ∈ {1, −1} (JSON numbers), `unit` ∈ {`"sentence"`, `"seconds"`}; optional `amount` | `amount` (if present, i.e. not `undefined`) must be a finite number, clamped 0..3600 | checked in this order: `"direction" must be 1 or -1`, `"unit" must be sentence\|seconds`, `"amount" must be a finite number` (note `"amount": null` → error) |
| `rate` | `rate` finite number | clamp 0.5..3 | `"rate" must be a finite number` |
| `volume` | `volume` finite number | clamp 0..1 | `"volume" must be a finite number` |
| missing / anything else | | | `Unknown command type` |

"Finite number" = a JSON number (never a numeric string: `"rate":"2"` → error, verified). `type` must be one of the
exact strings (`{"type":1}`, `{"type":"Stop"}` → `Unknown command type`). Only the first failing check is reported
(`{"type":"skip","direction":-1,"unit":"x","amount":"y"}` → the `unit` message, verified). Verified examples:
`{"type":"rate","rate":10}` → 200 and
the command applied is `{type:"rate",rate:3}`; `{"type":"seek","positionSec":-5}` → `positionSec:0`;
`{"type":"skip","direction":-1,"unit":"seconds","amount":99999}` → `amount:3600`.
The extension only ever sends `{"type":"toggle"}` and `{"type":"stop"}` (§2.4); the rest exists for parity with the
in-app command channel.

### 1.7 Connection-level behavior (local-server.ts:118-146; verified)

| Situation | Behavior |
|---|---|
| Header block not complete within 10 s (or a connection that sends nothing) | raw `HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`, close (Node raises `ERR_HTTP_REQUEST_TIMEOUT` into the custom client-error handler, which maps everything except header overflow to 400) |
| Malformed request / non-standard method / lower-case `post` | same raw 400 |
| Header block > 16 KiB | raw `HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`, close |
| Client reset (`ECONNRESET`) or socket no longer writable during a parse error | destroy silently |
| Entire request (headers + body) not received within 16 s of its first byte | same raw 400 + close (same `ERR_HTTP_REQUEST_TIMEOUT` path; verified). The 15 s JSON 408 wins only when the headers arrived within ~1 s (see §1.4 step 11). Once the request is fully received this timer no longer applies, so a slow handler is bounded only by the 15 s handler deadline |
| Idle keep-alive connection | closed after 5 s (observed 5–6 s: timers are checked once per second) |
| Any socket with no I/O for 20 s | destroyed without a response |
| More than 64 concurrent connections (idle keep-alive ones included) | new ones closed immediately after accept |
| `stop()` | all sockets destroyed immediately |
| Timer granularity | header/request timeouts are evaluated every 1 s, so the 10 s header timeout fires between 10 and 11 s |
| `Expect: 100-continue` | `HTTP/1.1 100 Continue` sent before any check (Node default, verified) |
| `CONNECT` method | no `connect` listener → Node closes the connection without a response |

Rust (hyper 1.x / tokio) mapping: `http1::Builder::new().keep_alive(true).header_read_timeout(10s)`
(+ `timer(TokioTimer)`), `max_buf_size(16 * 1024)` for the header cap, a `Semaphore(64)` around accepted sockets,
`tokio::time::timeout(15s)` around body collection (`http_body_util::Limited::new(body, 2 MiB)`) and around the read
handler, and an outer 20 s idle guard. Exact raw-400/431 bytes are nice-to-have; the extension never triggers them.
Two hyper differences to be aware of (both unreachable from the extension): hyper accepts any token as a method
(`get`, `FOO`), where Node's parser rejects anything outside its fixed upper-case method list with the raw 400 — to
mimic, answer `get`-style methods with that raw 400 before routing, or simply let them reach the router (they would
get 405/404); and hyper treats `CONNECT` as an upgrade request.

### 1.8 Security model and its reasons (local-server.ts:1-9, SPEC.md:433-442)

| Threat | Defence | Why it works |
|---|---|---|
| A web page `fetch`es the bridge | non-GET routes need the custom `x-vox-client` header → the browser must preflight → every `OPTIONS` gets 403 without CORS headers → the real request is never sent | CORS: custom headers are not "simple" |
| A web page sends a "simple" request (form POST, `no-cors` fetch, `<img>`) | such requests carry an `Origin` of `http(s)://…`, `file://…` or `null` → 403; they also cannot add `x-vox-client` → 403; `text/plain` bodies fail the JSON content-type check → 415 | layered |
| DNS rebinding (attacker domain resolving to 127.0.0.1) | `Host` must be literally `127.0.0.1:<port>` or `localhost:<port>` | a rebinding page sends its own host name |
| Remote machines | bound to 127.0.0.1 only | |
| The extension itself | exempt from CORS through `host_permissions` for `http://127.0.0.1/*` and `http://localhost/*`; sends `Origin: chrome-extension://<id>`, which passes step 3 | |

Known, accepted gaps (keep behavior; document for reviewers): a `no-cors` GET from a page may reach `/health`
without an Origin header (response is opaque to the page; read-only); any other installed extension with loopback
host access, and any local process, can call every route (the header is not a secret). The bridge can be turned off
in Settings.

### 1.9 Port mismatch between app and extension; Settings UI for the bridge

The extension stores its own port (default 47823) and the app has `settings.server.port`; nothing syncs them. The
Settings port row says: `Change it only if another app already uses it, then enter the same port in the extension’s
popup.` Port input: digits only, max 5, must parse as 1024..65535 else `Use a port between 1024 and 65535.`
(IntegrationsSection.tsx:151-215, settings/logic.ts:160-165).

Settings → Integrations, "Browser extension" block (IntegrationsSection.tsx:21-104, 151-215) — behavior the Rust
settings page must reproduce (visual design is free):

| Element | Behavior / copy |
|---|---|
| Section intro | `Real “Read with Vox” menu entries where Windows allows them, and how Vox starts with your PC.` |
| Sub-head badge | `Off` (toggle off) → `Checking…` (no AppInfo yet, or within a re-check window) → `Listening on <port>` (running; port = `AppInfo.serverPort` when running, else `settings.server.port`) → `Not running` |
| Re-check window | after toggling the bridge: refresh AppInfo after 700 ms; after applying a port: 900 ms; after toggling the Explorer verb: 1200 ms. The `Checking…` state ends when new AppInfo arrives or after 4000 ms at the latest (so it never sticks) |
| Toggle | label `Chrome & Edge bridge`, description `Lets the bundled extension add “Read with Vox” to the browser’s own right-click menu. Only reachable from this PC.` → `triggers.browserExtension` |
| Port row | disabled while the bridge is off. Apply on blur or Enter; Escape reverts to the saved port; a draft equal to the saved port is a no-op; invalid → inline alert `Use a port between 1024 and 65535.` (cleared on the next keystroke). When enabled, AppInfo loaded, not re-checking and the server is not running: warning `Port <port> may be in use by another app. Try another one.` |
| `parsePort(text)` | trim; must match `^\d{1,5}$`; number in 1024..65535, else invalid |
| Install row | label `Install the extension`, description `One-time setup in Chrome, Edge, Brave or any Chromium browser.`; steps: `Open chrome://extensions or edge://extensions` (each with a copy button: tooltip `Copy <text>` → `Copied` for 1400 ms; failure toast `Couldn’t copy to the clipboard`), `Turn on Developer mode in the corner of the page`, `Click Load unpacked and choose the extension folder` + button `Show extension folder` (opens the folder in Explorer; errors ignored, failure only logged `openPath failed: <err>`) |

The same section also holds the Explorer toggle (§3.5), `Launch at login` / `Start minimized` (§4.3) and
`Close to tray` (`Closing the window keeps Vox running so shortcuts and chips keep working.`, shell spec).

### 1.10 Wire-compatibility test vectors (captured from the original server)

Use these as integration tests for the Rust server (`$P` = port). `curl` sets `Host: 127.0.0.1:$P` itself.

| Request | Expected status | Expected body |
|---|---|---|
| `GET /health` | 200 | `{"ok":true,"app":"Vox","version":"0.1.0","playing":false}` |
| `HEAD /health` | 200 | empty, `Content-Length: 57` |
| `GET /health/?x=1` | 200 | as above |
| `POST /health` | 405, `Allow: GET` | `{"ok":false,"error":"Method not allowed"}` |
| `OPTIONS /read` (with `Origin: https://evil.com`) | 403 | `{"ok":false,"error":"Forbidden"}` |
| `GET /health`, `Host: evil.com:$P` | 403 | `{"ok":false,"error":"Forbidden host"}` |
| `GET /health`, `Host: 127.0.0.1` | 403 | `{"ok":false,"error":"Forbidden host"}` |
| `GET /health`, `Host: LOCALHOST:$P` | 200 | health JSON |
| `GET /health`, `Origin: http://x` | 403 | `{"ok":false,"error":"Forbidden origin"}` |
| `GET /health`, `Origin: null` | 403 | `{"ok":false,"error":"Forbidden origin"}` |
| `GET /health`, `Origin: chrome-extension://abc` | 200 | health JSON |
| `GET /nope` | 404 | `{"ok":false,"error":"Not found"}` |
| `GET /read` | 405, `Allow: POST` | `{"ok":false,"error":"Method not allowed"}` |
| `POST /read` without `x-vox-client` | 403 | `{"ok":false,"error":"Forbidden"}` |
| `POST /read`, `Content-Type: text/plain` | 415 | `{"ok":false,"error":"Content-Type must be application/json"}` |
| `POST /read`, `Content-Length: 3000000` | 413, `Connection: close` | `{"ok":false,"error":"Request body too large","code":"too-long"}` |
| `POST /read`, body `{x` | 400 | `{"ok":false,"error":"Invalid JSON"}` |
| `POST /read`, empty body | 400 | `{"ok":false,"error":"Invalid JSON"}` |
| `POST /read`, body `[]` / `null` / `"x"` | 400 | `{"ok":false,"error":"Expected a JSON object"}` |
| `POST /read`, body `{"text":1}` | 400 | `{"ok":false,"error":"\"text\" must be a string","code":"empty-text"}` |
| `POST /read`, body `{"text":" \n\t "}` | 400 | `{"ok":false,"error":"Nothing to read — the text is empty","code":"empty-text"}` |
| `POST /read`, body = UTF-8 BOM + `{"text":"hi"}` | 200 | handler receives `{text:"hi"}` |
| `POST /read`, `{"text":"hi","url":"javascript:alert(1)","title":"  T  ","extra":1}` | 200 | handler receives `{text:"hi",title:"T"}` |
| `POST /read`, body arrives partially and stalls | 408 after 15 s, `Connection: close` | `{"ok":false,"error":"Request timed out"}` |
| `POST /read`, handler never answers | 504 after 15 s | `{"ok":false,"error":"Timed out waiting for the app","code":"unknown"}` |
| `POST /command` `{"type":"toggle","x":true}` | 200 | `{"ok":true}` (command `{type:"toggle"}`) |
| `POST /command` `{"type":"skip","direction":2,"unit":"words"}` | 400 | `{"ok":false,"error":"\"direction\" must be 1 or -1"}` |
| `POST /command` `{"type":"jump"}` | 400 | `{"ok":false,"error":"Unknown command type"}` |
| Garbage request line `HELLO` | raw 400, no body | — |
| 20 000-byte header | raw 431, no body | — |
| `GET //` (or `///`) | 400 | `{"ok":false,"error":"Bad URL"}` |
| `GET //evil/health`, `GET /health#x`, `GET /health\` | 200 | health JSON |
| `HEAD /nope` | 404 | empty body |
| `PATCH /health` | 405, `Allow: GET` | `{"ok":false,"error":"Method not allowed"}` |
| `GET /health`, `Origin: chrome-extension://a` then `Origin: http://x` | 200 | health JSON |
| `GET /health`, `Origin: http://x` then `Origin: chrome-extension://a` | 403 | `{"ok":false,"error":"Forbidden origin"}` |
| `GET /health`, `Host: localhost.:$P` / `Host: [::1]:$P` | 403 | `{"ok":false,"error":"Forbidden host"}` |
| `POST /read`, no `x-vox-client`, `Content-Type: text/plain` | 403 | `{"ok":false,"error":"Forbidden"}` |
| `POST /command`, `Content-Type: application/vnd.api+json` | 415 | `{"ok":false,"error":"Content-Type must be application/json"}` |
| `POST /read`, body `{"text":"a\ud83d"}` (lone surrogate escape) | 200 | handler receives the text (Rust: `"a"` + U+FFFD) |
| `POST /read`, `{"text":"a","title":"   ","url":"HTTPS://X"}` | 200 | handler receives `{text:"a",url:"HTTPS://X"}` |
| `POST /command` `{"type":"skip","direction":1,"unit":"seconds","amount":null}` | 400 | `{"ok":false,"error":"\"amount\" must be a finite number"}` |
| `POST /command` `{"type":"rate","rate":"2"}` | 400 | `{"ok":false,"error":"\"rate\" must be a finite number"}` |
| `POST /command` with `Expect: 100-continue` | 100 then 200 | `{"ok":true}` |
| keep-alive `GET /health` | 200 + `Connection: keep-alive`, `Keep-Alive: timeout=5`; server closes the idle socket after 5–6 s | health JSON |

```bash
curl -s http://127.0.0.1:47823/health
curl -s -X POST http://127.0.0.1:47823/read -H 'x-vox-client: browser-extension' \
     -H 'content-type: application/json' -d '{"text":"Hello from the bridge","url":"https://example.com","title":"Example"}'
curl -s -X POST http://127.0.0.1:47823/command -H 'x-vox-client: browser-extension' \
     -H 'content-type: application/json' -d '{"type":"toggle"}'
```

---

## 2. Browser extension (kept unchanged; the server must satisfy it)

The extension is plain JS with no build step and is shipped as a folder the user loads with "Load unpacked". The
Rust app must ship the folder **byte-for-byte** (including `popup.css`, `fonts/`, `icons/`) and keep the protocol of
§1 exactly. This section documents what it sends and expects.

### 2.1 Manifest (browser-extension/manifest.json)

| Key | Value |
|---|---|
| `manifest_version` | 3 |
| `name` / `short_name` | `Read with Vox` / `Vox` |
| `version` | `0.1.0` |
| `description` | `Right-click any text and hear it read aloud by the Vox desktop app. Text stays on your computer.` |
| `minimum_chrome_version` | `110` |
| `permissions` | `contextMenus`, `storage`, `activeTab`, `scripting` |
| `host_permissions` | `http://127.0.0.1/*`, `http://localhost/*` (any port; this is what exempts its requests from CORS and preflights) |
| `background.service_worker` | `background.js` |
| `action` | title `Read with Vox`, popup `popup.html`, icons 16/32/48/128 (`icons/icon-<n>.png`) |
| `icons` | the same four PNGs |
| `commands.read-selection` | suggested key `Alt+Shift+R` (`suggested_key.default`), description `Read the selected text with Vox` |

No Firefox-specific keys: it is a Chromium-only extension (Chrome, Edge, Brave, any Chromium browser), although the
app's foreground allowlist also lists `firefox.exe`. No `content_scripts`, no `web_accessible_resources`, and **no
`key`**: an unpacked extension's id is derived from its folder path, so the `Origin: chrome-extension://<id>` differs
per install/browser — the server must never allowlist a particular extension id (it only rejects web origins).

Packaging in the old app: copied to `<install>\resources\browser-extension` (electron-builder.yml:34-37);
`AppInfo.extensionPath` points there (dev: `<repo>\browser-extension`) (ipc.ts:239-243); Settings → Integrations has
a `Show extension folder` button that opens it (`app:revealPath('extension')`). The icons are generated by the old
build (`npm run icons`, `scripts/gen-icons.mjs`) and are committed PNGs — ship them as files.

### 2.2 Bridge client (browser-extension/bridge.js)

Constants (bridge.js:8-13; "keep in sync with src/shared/constants.ts"):

| Name | Value |
|---|---|
| `APP_NAME` | `Vox` |
| `DEFAULT_PORT` | `47823` |
| `CLIENT_HEADER` / `CLIENT_VALUE` | `x-vox-client` / `browser-extension` |
| `MAX_TEXT_CHARS` | 200 000 (text sliced before sending; the server accepts 2 MB anyway) |

- Port: `chrome.storage.sync` key `port` (default 47823); valid = integer 1024..65535, otherwise the default is used.
  `setPort` rejects invalid ports with `Port must be a number between 1024 and 65535.` (bridge.js:23-39).
- Every request goes to **`http://127.0.0.1:<port><path>`** (never `localhost`), so `Host` is always
  `127.0.0.1:<port>` (bridge.js:50).
- `GET` requests send **no** custom headers. Non-GET requests send `content-type: application/json` and
  `x-vox-client: browser-extension`; body `JSON.stringify(body)`. Fetch options: `cache: 'no-store'`,
  `credentials: 'omit'`, `signal: AbortSignal.timeout(timeoutMs)` (bridge.js:41-57).
- Timeouts: default 4000 ms; `/health` 1500 ms; `/read` 20 000 ms; `/command` 4000 ms. The server's 15 s handler
  deadline is deliberately below the extension's 20 s.
- Response handling (bridge.js:58-76) and the user-visible messages:

  | Outcome | Message (kind) |
  |---|---|
  | Fetch timed out / aborted | `Vox did not answer in time.` (`timeout`) |
  | Network error (nothing listening) | `Vox isn't running. Start the Vox app (port <p>).` (`offline`) |
  | HTTP status not 2xx | `Vox refused the request: <json.error if a string, else "HTTP <status>">` (`http`) |
  | 2xx but body is not JSON | `Port <p> is used by another program, not Vox.` (`http`) |
  | 2xx JSON | returned to caller |

- `health(port)` → `GET /health`. `read({text,url,title})` → `POST /read` with body
  `{ text: String(text||'').slice(0,200000), url?: url if /^(https?|file):/i, title?: title.trim().slice(0,300) if non-empty }`;
  if the JSON's `ok !== true` → error `<result.error>` or `Vox could not read that.` (`read`) (bridge.js:84-91).
  `command(cmd)` → `POST /command` (bridge.js:94-96).
- Both slices count **UTF-16 units** and can cut an emoji in half; `JSON.stringify` then sends a lone surrogate
  escape such as `"\ud83d"` — the Rust server must accept it (§1.4 step 12). The extension's URL test only needs
  `scheme:` (no `//`), so e.g. `https:foo` is sent and then dropped by the server's stricter `://` test.
- The body is parsed as JSON for **every** status before `res.ok` is checked; "not JSON" also covers JSON `null`,
  `false`, `0` and `""` (all falsy). So the server's error bodies must stay JSON with a string `error` for the
  extension to show the reason.

### 2.3 Service worker (browser-extension/background.js)

- Context menus (recreated on install and browser startup): id `vox-read-selection`, title `Read with Vox`,
  context `selection`; id `vox-read-page`, title `Read page with Vox`, context `page` (background.js:14-23).
- Keyboard command `read-selection` → read selection in the active tab (background.js:30-32).
- Popup messages `{type:'vox:read-selection'|'vox:read-page'}` from its own extension id → `{ok:true}` or
  `{ok:false,error}` (background.js:35-43).
- **Read selection** (background.js:52-77): run `selectionInPage` in the clicked frame (menu) or the top frame
  (shortcut). It returns `null` if the focused element is a password `<input>` (→ error
  `Password fields are never read aloud.`), the selected substring of a focused `<textarea>`/`<input>` if any, else
  `window.getSelection().toString()` (keeps line breaks, unlike the menu's `selectionText`). Longest result (by
  trimmed length; first one on ties) wins. Shortcut/popup with nothing found and no password field → retry in all
  frames (the retry does not re-check for password fields; their `null` results are just ignored). Scripting
  failures (PDF viewer, `chrome://`, Web Store) are swallowed; then, for the context menu only, an empty scripted
  result falls back to `info.selectionText`. Nothing → `Select some text first.` (menu) or
  `Select some text first. (On protected pages, right-click → Read with Vox.)` (shortcut/popup). Sends
  `{ text, url: tab.url, title: tab.title }`.
- **Read page** (background.js:79-90, 155-240): injected `extractPage(20000)`:
  root = the `<article>` with the most text if ≥ 200 chars (text length = textContent with whitespace runs collapsed,
  trimmed), else `main, [role="main"]` if ≥ 200 chars, else the best article, main, or `<body>`. Walk the DOM,
  skipping tags `script style noscript template svg math canvas iframe object embed video audio nav aside footer form
  button select input textarea dialog menu` (and `header` when the root is `<body>`), roles `navigation complementary
  contentinfo banner search menu menubar toolbar dialog`, `hidden`, `aria-hidden="true"`, and computed
  `display:none` / `visibility:hidden|collapse`. Text nodes collapse whitespace to one space; `<br>` = line break;
  block-level elements (computed display not `inline*`/`contents`) request a line break before and after, paragraph
  tags `p h1-h6 blockquote pre table ul ol dl section article figure` a blank line. Collection stops past 24 000 chars
  (1.2 × max). Then collapse `[ \t\u00A0]+`→space, trim spaces around newlines, ≥3 newlines → 2, trim. If < 200 chars,
  use `document.body.innerText` (trimmed) when longer (reason: shadow-DOM / custom-element pages defeat the walk).
  If > 20 000: `cut = lastIndexOf(' ', 20000)` (a space at or before index 20 000); slice to `cut` if it is beyond
  18 000, else hard cut at 20 000; trim end. A `visibility:hidden` parent skips its whole subtree, even visible
  children. Title = `document.title`, else `tab.title`. Errors: scripting refused →
  `The browser doesn't let extensions read this page. Select the text and use Read with Vox instead.`; empty →
  `No readable text found on this page.`; no tab → `No active tab.`.
- Error badge (background.js:102-134): on failure store `chrome.storage.local.lastError = { message, at: Date.now() }`,
  badge text `!` on `#D71921` with white text, action title `Read with Vox — <message>`. On success clear all three.

### 2.4 Popup (browser-extension/popup.html, popup.js)

- On open: read port, render the shortcut (from `chrome.commands.getAll`, keys split on `+` into `<kbd>`; empty →
  `a shortcut (not set)`; API failure → `Alt+Shift+R`), show the stored last error, check health, then poll
  `GET /health` every **2500 ms** (popup.js:7, 182-205). A sequence number drops stale health answers.
- Online (`info.ok`): status text `<info.app || "Vox"> <info.version>` (trimmed), dot white; subtitle
  `Right-click any text and choose <b>Read with Vox</b>.`; label `Now playing` (if `info.playing === true`) or
  `Player idle`; toggle button aria-label `Pause`/`Resume`; transport enabled. Offline: `Offline`,
  `Vox isn't running on port <port>. Open the app, then try again.`, `Player offline`, buttons disabled
  (popup.js:40-60). A 24-dot meter mirrors `offline/online/playing` (CSS only).
- Buttons: `Read selection` / `Read page` → message to the worker (ignored while `aria-busy`); success notice
  `Sent to Vox.` then re-check health after 600 ms; failure → error notice (`res.error`, else `Could not reach Vox.`)
  with relative time (`Just now` < 60 s, `<round(min)> min ago` < 1 h, `<round(h)> h ago` < 24 h, else locale date).
  Pause/resume → `POST /command {"type":"toggle"}`; stop → `{"type":"stop"}`; errors shown as notices; then
  re-check health. Dismissing a notice removes `lastError` and the badge text (not the action title).
- Port field: digits only, ≤ 5; applied on change/Enter if valid 1024..65535 (else `aria-invalid`), saved to
  `chrome.storage.sync`, status `Checking`, re-check. Hint: `Must match Settings → Triggers → Browser extension in the
  Vox app.` (stale: the app moved the bridge to **Settings → Integrations**; see open questions).
- `Change` link opens `edge://extensions/shortcuts` when the UA contains `Edg/`, else `chrome://extensions/shortcuts`.

---

## 3. Explorer "Read with Vox" verb

A classic static shell verb registered **per user** for nine extensions, so no admin rights are needed and it
applies whatever app owns the extension. Windows 11 shows such verbs only under **Show more options**
(Shift+F10); only packaged apps with an `IExplorerCommand` can appear in the compact top-level menu
(explorer-menu.ts:1-8).

### 3.1 Registry layout (explorer-menu.ts:14-30, 67-79)

Extensions (`EXPLORER_EXTENSIONS`, in this order):
`.txt .md .markdown .log .csv .json .rtf .html .htm`

For each `<ext>`, key `HKEY_CURRENT_USER\Software\Classes\SystemFileAssociations\<ext>\shell\VoxRead`:

| Key | Value name | Type | Data | Why |
|---|---|---|---|---|
| `…\shell\VoxRead` | (Default) | REG_SZ | `Read with Vox` (`READ_WITH_LABEL`) | menu text |
| `…\shell\VoxRead` | `MultiSelectModel` | REG_SZ | `Single` | reading several files at once would just cancel each other out, so the verb only appears for a single selected file |
| `…\shell\VoxRead` | `Icon` | REG_SZ | packaged: full path of `Vox.exe` (no `,index`, i.e. icon 0); dev: `<appPath>\resources\icon.ico` | icon next to the entry; when no icon is configured the value is deleted (removes a stale icon from an older install) |
| `…\shell\VoxRead\command` | (Default) | REG_SZ | command line (§3.2) | |

No `Extended`, `Position` or `CommandFlags` values. Values are REG_SZ, not REG_EXPAND_SZ. Verb key name `VoxRead`
(`EXPLORER_VERB`) is mirrored in the uninstaller. Explorer reads these keys on every right-click, so no
`SHChangeNotify` is needed (the old code sends none).

### 3.2 Command line (explorer-menu.ts:33-58; index.ts:185-187)

```
<quote(exePath)> [<quote(arg)>…] --read-file "%1"
```

- Packaged: `exePath` = the running executable, no extra args → e.g.
  `C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe --read-file "%1"`.
- Dev: `exePath` = `electron.exe`, extra arg = the app folder (Rust dev builds: just the dev exe).
- If a configured command contains no `%1`, `%L` or `%V` placeholder (case-insensitive regex `%[1LV]`), ` "%1"` is
  appended (explorer-menu.ts:69). The built command always contains `"%1"`.
- `quoteWindowsArg` — MSVCRT / `CommandLineToArgvW`-compatible, applied to the exe and each extra arg (not to
  `--read-file` or `"%1"`):

```text
quote(arg):
  if arg is non-empty and contains no whitespace (JS \s) and no '"': return arg unchanged   // note: unquoted!
  out = '"'; backslashes = 0
  for ch in arg:
    if ch == '\\': backslashes += 1; continue
    if ch == '"':  out += '\\' * (2*backslashes + 1) + '"'
    else:          out += '\\' * backslashes + ch
    backslashes = 0
  return out + '\\' * (2*backslashes) + '"'
```

  So a path without spaces is written **without** quotes (and `""` for an empty arg). Keep this exact algorithm: the
  startup state check (§3.4) compares the stored string with the freshly built one, and a different quoting style
  would make every existing install look "stale" (harmless — it just rewrites — but noisy).

### 3.3 Install / remove (explorer-menu.ts:117-156)

`setExplorerMenu(enabled, { command, label, iconPath? }) → { ok, error? }`, serialized through a queue so a quick
on/off toggle never interleaves registry writes:

- Not Windows → `{ ok:false, error:"The Explorer menu is only available on Windows" }`.
- `enabled == false` → remove all (below).
- Empty (after trim) `command` or `label` → `{ ok:false, error:"Missing command or label" }`.
- Install: for all nine extensions **in parallel**, each extension's steps sequentially:
  1. set (Default) = label; 2. set `MultiSelectModel` = `Single`; 3. set `Icon` = iconPath, or delete `Icon`
  (failure ignored); 4. create `…\command` and set its (Default) = command.
  The old code shells out to `%SystemRoot%\System32\reg.exe` (`add <key> /ve|/v <name> /t REG_SZ /d <data> /f`,
  `delete <key> /v Icon /f`), hidden window, 10 s timeout per call, argv-quoted without a shell (no `%VAR%`
  expansion). A Rust port should call the registry API directly (`RegCreateKeyExW` / `RegSetValueExW`), which removes
  the reg.exe OEM-code-page and quoting concerns.
- A failing step stops the remaining steps **of that extension only**; the other extensions carry on. If any
  non-optional step failed → **remove everything** ("never leave a half-installed menu behind"; the rollback's own
  errors are ignored) and return `{ ok:false, error: "Could not add the Explorer entry (<ext>: <reason>)" }` using the
  failure of the first extension **in `EXPLORER_EXTENSIONS` order** (not the first in time) (reason = reg.exe stderr
  decoded as Latin-1 and trimmed, or `reg.exe exited with <code>`; in Rust use the OS error message).
- In the app the `Icon` branch always writes (an icon path is always passed), and the command always contains
  `"%1"`, so the "delete Icon" and "append `"%1"`" branches only matter for other callers/tests.
- Remove one extension: delete the whole `…\shell\VoxRead` tree (`reg delete <key> /f`; Rust `RegDeleteTreeW` +
  `RegDeleteKeyW`, or `winreg::RegKey::delete_subkey_all`). A "not found" is success. reg.exe exits 1 for both
  "not found" and real failures, so on failure the old code queries the key and only reports an error if it still
  exists — in Rust, treat `ERROR_FILE_NOT_FOUND` as success.
- Remove all: all nine in parallel; any failure → `{ ok:false, error:"Could not remove the Explorer entry (<ext>: <reason>)" }`
  (first failure), else `{ ok:true }`.

### 3.4 State detection (explorer-menu.ts:158-223)

Old implementation: one `reg query HKCU\Software\Classes\SystemFileAssociations /s`, output decoded as Latin-1 and
parsed: a line starting with `HKEY_CURRENT_USER\` (case-insensitive) selects the current key; for the nine
`…\<ext>\shell\voxread\command` keys, the first line matching `^.*?\s{2,}REG_(?:EXPAND_)?SZ(?:\s{2,}(.*))?$` gives
the command (trimmed; missing data → `""`). The value name is not matched because reg.exe localizes "(Default)".

Rust equivalent (direct API): for each extension open `HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\VoxRead\command`;
if the key exists, read its default value if it is REG_SZ/REG_EXPAND_SZ (raw, unexpanded), else `""`.

- `isExplorerMenuInstalled()` = the `command` key exists for **all nine** extensions (value content irrelevant).
  Feeds `AppInfo.explorerMenuInstalled` (Settings shows an `Installed` badge on the "Windows" sub-head; no badge
  otherwise). Recomputed on every AppInfo request (ipc.ts:273-276). Non-Windows / query failure → false.
- `explorerMenuState(expected)`:
  - `absent`: no extension has the `command` key (also when the base key does not exist / query fails).
  - `stale`: some but not all extensions have it, **or** any extension's command ≠ `expected.trim()` compared
    case-insensitively.
  - `current`: all nine have a command equal (case-insensitive) to the expected one.
  - Old special case: if `expected` contains characters outside printable ASCII `[\x20-\x7e]`, any complete install
    counts as `current`, because reg.exe output (OEM code page decoded as Latin-1) cannot be compared reliably and
    the app would otherwise rewrite the registry on every start. With the direct UTF-16 registry API this exception
    is unnecessary; compare always.

### 3.5 When it is synced (index.ts:189-211, 308, 502)

- Setting `settings.triggers.explorerMenu` (default **false**). Skipped entirely in demo mode.
- Startup (`force = false`): compute the state with the current expected command; if it already matches the
  setting (`current` when enabled, `absent` when disabled) do nothing; otherwise log
  `explorer menu is <state>; installing|removing it` and install/remove. This repairs a moved install directory,
  a dev build's leftovers, or a partial install.
- Setting toggled (`force = true`): always install/remove.
- Options passed: `command` (§3.2), `label = "Read with Vox"`, `iconPath` = exe path (packaged) or
  `<appPath>\resources\icon.ico` (dev).
- Failure → log warn `explorer menu update failed` and error toast `Couldn't update the Explorer menu: <error>` (just
  `Couldn't update the Explorer menu` if the error is empty); no action button. Exceptions → log warn
  `explorer menu sync failed`, no toast. Startup order: `syncServer`, then this, then autostart (index.ts:501-503),
  all after the windows and tray exist and before the initial CLI action is handled.
- Settings copy: label `Explorer right-click`, description
  `Adds “Read with Vox” to text files (.txt .md .log .csv .json .rtf .html). On Windows 11 it’s under Show more options.`
  (the UI lists 7 of the 9 types: `.markdown` and `.htm` are omitted from the copy only) (IntegrationsSection.tsx:11, 116-128).

### 3.6 Uninstaller (resources/installer.nsh:9-27)

On uninstall, **unless it is an update** (an update runs the old uninstaller first; entries must survive upgrades):
delete key `HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\VoxRead` for all nine extensions, and the
launch-at-login values (§4.1) named after the product (`Vox`) in both `…\Run` and
`…\Explorer\StartupApproved\Run`. Application data in `%APPDATA%\Vox` is kept.

### 3.7 From click to read

1. Explorer runs `Vox.exe --read-file "<full path>"`.
2. CLI parsing (reader.ts:75-104; full rules in shell.md §2): `--read-file <path>` or `--read-file=<path>`; the next
   argv element is consumed whatever it looks like; empty → none.
3. If Vox is already running, the new process forwards its argv **and cwd** to the running instance and exits
   (single-instance lock, index.ts:538-560). Required for this feature; see shell.md §3.3.
4. `handleCli` (reader.ts:454-478) runs at most one action, in this precedence: `--read-text` > `--read-clipboard` >
   `--read-file`. For `--read-file`: resolve the path against the forwarded cwd (`path.resolve`); `name` = file name;
   `text = readTextFile(path, settings.text.maxChars + 1000)` (§5) — reading 1000 past the limit lets the reader
   notice the text is longer and toast `Reading the first <N> characters`; then start a read with
   `source = "explorer"`, `sourceApp = "Explorer"`, `sourceProcess = "explorer.exe"`, `sourceTitle = name`.
   Any error (open failure, not a regular file, binary) → log warn `read-file failed` and error toast
   **`Couldn't open <name>`**. An empty or whitespace-only file is *not* an error here: `readTextFile` returns `""`
   and the reader answers with its own info toast `Nothing to read in that selection`. A missing API key gives the
   usual `Add your ElevenLabs API key to start reading` toast.
5. A launch that carries a read action does not show the main window (index.ts:466-471, 335). For a forwarded
   second-instance launch, the main window is shown only when no action was started **and** `--hidden` is absent
   (index.ts:332-336); an action counts as started even if the read then fails.

---

## 4. Launch at login

### 4.1 Registry (autostart.ts; Electron `app.setLoginItemSettings` on Windows)

| Item | Value |
|---|---|
| Key | `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` |
| Value name | `Vox` for installed builds; `Vox (dev)` for dev builds (a distinct name keeps dev and installed builds from overwriting each other's entry) (autostart.ts:13-18, 29) |
| Type | REG_SZ |
| Data | `"<exe path>"` followed by ` --hidden` when `settings.app.startMinimized` is true. Dev: `"<electron.exe>" <quoted appPath> [--hidden]`. Per the autostart.ts:16 comment ("Electron 44 quotes each arg itself (AddQuoteForArg), so pass it raw"), Electron joins exe + args with spaces and quotes each argument with Chromium's command-line rule (quote only if it contains a space, `\` or `"`; double the backslashes that precede a `"` or the closing quote; `\"` for quotes) — so the dev app path is always quoted and `--hidden` never is. The exact bytes the old build wrote were **not** re-verified here (it would need writing to the registry); check `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v Vox` on a machine with the old build. The Rust port should write `"<exe>"` / `"<exe>" --hidden` and recognize either quoting when reading (parse with `CommandLineToArgvW`). |
| Task Manager state | `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run`, REG_BINARY value of the same name (12 bytes: first byte `0x02`/`0x06` = enabled, `0x03`/`0x07` = disabled, then a FILETIME). Written by Windows when the user toggles the entry in Task Manager / Settings → Apps → Startup. |

`HIDDEN_ARG = "--hidden"` (autostart.ts:5).

### 4.2 Algorithm (autostart.ts:24-48)

`applyLaunchAtLogin(enabled, startHidden)`; no-op on non-Windows; skipped in demo mode; exceptions only logged
(`[autostart] could not update the login item: <message>`; caller logs `applyLaunchAtLogin failed`).

- **Enable** (intended behavior, autostart.ts:20-23, 36-47): if a per-user (`HKCU`) Run value with our name already
  exists whose command parses to the same exe path (**case-insensitive**) and exactly the same args (count and each
  string, case-sensitive) → do **nothing**. Reason: calling this on every startup must not re-enable an entry the
  user switched off in Task Manager. Otherwise write the Run value and **delete** the `StartupApproved\Run` value of
  the same name (this is what Electron does with its default `enabled: true`; deleting it means "enabled").
  Consequence to keep: changing `startMinimized` while enabled rewrites the value and re-enables a
  Task-Manager-disabled entry.
- **What the old build actually does** (verified with Electron 44.7.0's `app.getLoginItemSettings` against existing
  Run entries on this machine): Electron builds `launchItems` with Chromium's `base::CommandLine` parser, which
  (a) puts `--switch` style arguments into its switch map, so `launchItems[].args` **omits them** — Spotify's
  `"…\Spotify.exe" --autostart --minimized` comes back with `args: []`; and (b) parses the lookup `path` (passed
  unquoted, `process.execPath`) as a command line, so an exe path **containing a space** is cut at the space and
  matches nothing (`C:\Program Files\Mozilla Firefox\firefox.exe` → no launch items, although Firefox has a Run
  entry). Consequences in the old app: with `startMinimized` on (target args `["--hidden"]` vs parsed `[]`), or with
  a space anywhere in the exe path (e.g. a user name with a space, a custom install folder), the "already registered"
  check never matches, so **every startup rewrites the value and deletes the StartupApproved value**, silently
  re-enabling an entry the user disabled in Task Manager. The idempotent case only works for the default install
  with `startMinimized` off. The Rust port should implement the intended behavior (parse with
  `CommandLineToArgvW`, compare all args including switches); see open questions.
- **Disable**: delete the Run value and the `StartupApproved\Run` value (missing values are fine). Because it runs at
  every startup, a disabled setting deletes both values on every launch (harmless; also cleans up after a crash).
- Called at startup (index.ts:503) and whenever `app.launchAtLogin` or `app.startMinimized` changes
  (index.ts:309-311). Defaults: `launchAtLogin = false`, `startMinimized = false`. Synchronous; the setting change
  is never rolled back on failure and there is no toast.

Why the old code compares via "launch items" rather than Electron's `openAtLogin` flag: `openAtLogin` only looks at
the value named after the AppUserModelID (`com.vox.reader`), not our custom name (autostart.ts:37-38). A Rust port
just reads its own value.

Migration from the Electron build: if the Rust exe is installed at the same path, the existing `Vox` value
(`"…\Vox.exe"` / `"…\Vox.exe" --hidden`, quoted or not) is recognized as identical by the `CommandLineToArgvW`
comparison and left alone (Task Manager state preserved); a different path is simply rewritten. Electron's
`getLoginItemSettings` also reports `HKLM` Run entries (`scope: "machine"`), but the old check only accepted
`scope == "user"`; the Rust port only reads HKCU.

### 4.3 What `--hidden` does at launch (index.ts:466-471, 332-336)

The main window is shown at startup only if: not screenshot mode, no `--hidden`, no read action in argv, and
(`startMinimized` is false or onboarding is not finished). "Read action" = any of `--read-text <t>`,
`--read-clipboard`, `--read-file <path>` (`hasReadAction`; CLI grammar in shell.md). A second instance launched with `--hidden` and no read
action does not bring up the main window. Settings copy: `Launch at login` — `Start Vox quietly in the tray when you
sign in to Windows.`; `Start minimized` — `Open straight to the tray instead of showing this window.`
(IntegrationsSection.tsx:129-140). Note the mismatch: with `startMinimized` off, a login launch has no `--hidden` and
shows the main window.

---

## 5. File text extraction (`readTextFile`)

Turns a text-ish file into plain prose worth reading aloud. Pure, no UI; errors are surfaced by the caller (§3.7).

### 5.1 Entry point (text-extract.ts:6-38)

```text
readTextFile(path, maxChars) -> Result<String>:
  open path read-only (Node opens with FILE_SHARE_READ|WRITE|DELETE; Rust std's default share mode is the same)
  if not a regular file: error "Not a regular file"
  size      = min(file_size, MAX_FILE_BYTES)          // MAX_FILE_BYTES = 5 * 1024 * 1024 = 5 242 880
  truncated = file_size > MAX_FILE_BYTES               // "first 5 MB are far more than any read's character limit"
  read up to `size` bytes from offset 0 (loop until full or EOF; keep what was read)
  text = decodeTextBuffer(bytes, truncated)            // §5.2
  if looksBinary(text): error "This file does not look like text"   // §5.3
  ext = lower-case extension of the path (last '.' in the file name, not a leading dot; ".bashrc" -> "")
  text = match ext:
     ".html" | ".htm" | ".xhtml"           -> htmlToText(text)                 // §5.4
     ".rtf" if text matches /^\s*\{\\rtf/   -> rtfToText(text)                  // §5.6  (else: plain)
     ".md" | ".markdown"                   -> markdownToText(text)             // §5.7
     otherwise (.txt .log .csv .json, any other) -> plain(text)
  return truncateText(text, maxChars)                  // §5.9
plain(t) = normalizeNewlines(t), remove every U+0000, JS-trim     // no whitespace collapsing; JSON/CSV verbatim
```

Caller passes `maxChars = settings.text.maxChars + 1000` (`maxChars` ∈ 100..100 000, default 20 000 → 21 000).
`.xhtml` is handled although it is not registered for the Explorer verb (reachable via `--read-file`). HTML files
are not sniffed for `<meta charset>`; everything goes through §5.2.

Per-extension summary:

| Extension | Pipeline |
|---|---|
| `.txt`, `.log`, `.csv`, `.json` (and unknown) | decode → binary check → newline normalize, drop NULs, trim → truncate |
| `.md`, `.markdown` | decode → binary check → Markdown cleanup (+ tidy) → truncate |
| `.rtf` | decode → binary check → RTF parse (+ tidy) if it starts with `{\rtf` after optional whitespace, else plain → truncate |
| `.html`, `.htm`, `.xhtml` | decode → binary check → HTML → text (+ entities, tidy) → truncate |

### 5.2 Decoding (text-extract.ts:42-135)

```text
decodeTextBuffer(buf, truncated):
  if buf starts EF BB BF  -> decodeUtf8(buf[3..], truncated)
  if buf starts FF FE     -> decodeUtf16(buf[2..], LE)
  if buf starts FE FF     -> decodeUtf16(buf[2..], BE)
  if sniffUtf16(buf) = LE|BE -> decodeUtf16(buf, that order)
  else                    -> decodeUtf8(buf, truncated)
```

- `decodeUtf8(data, truncated)`: if `truncated`, first `trimIncompleteUtf8`. Strict UTF-8 decode; on success return
  it. The JS `TextDecoder('utf-8')` (both the strict and the lossy one) **removes one leading U+FEFF** of `data`, so
  a file starting with two BOMs decodes to the text without either (verified `EF BB BF EF BB BF 41` → `A`); Rust's
  `from_utf8`/`from_utf8_lossy` do not, so strip one leading `'\u{FEFF}'` after both passes (not after the cp1252
  fallback, which keeps `ï»¿`: verified `EF BB BF EF BB BF 41 E9 E9` → `ï»¿Aéé`). On failure: `loose` = lossy decode (each maximal
  invalid subsequence → one U+FFFD; Rust `String::from_utf8_lossy` has the same rule); `replacements` = count of
  U+FFFD chars in `loose`; `highBytes` = count of bytes ≥ 0x80 in `data`; return `loose` if
  `replacements * 4 < highBytes`, else **Windows-1252** decode of `data`. Reason: a legacy ANSI file makes nearly
  every high byte invalid; a UTF-8 file with a stray byte only a few.
- `trimIncompleteUtf8(buf)`: `i = len-1`; walk back while `buf[i] & 0xC0 == 0x80` and fewer than 3 continuation
  bytes seen; if `i < 0` return buf; `lead = buf[i]`; `need = lead ≥ 0xF0 ? 3 : lead ≥ 0xE0 ? 2 : lead ≥ 0xC0 ? 1 : 0`;
  if `need > continuationCount` return `buf[..i]` (drop the cut sequence) else buf. Applies only when the 5 MB cap
  cut the file.
- `decodeUtf16(buf, order)`: drop a trailing odd byte; for BE swap each byte pair; decode UTF-16LE; if the last code
  unit is a high surrogate (D800–DBFF) drop it. No BOM removal here: `FF FE FF FE 41 00` → U+FEFF + `A` (verified;
  the later JS `trim` removes it on the plain-text path). JS keeps other lone surrogates as-is (verified
  `FF FE 41 00 00 DC 42 00` → `A\uDC00B`); in Rust use `char::decode_utf16` mapping lone surrogates to U+FFFD (tiny
  difference: they then count toward `looksBinary`).
- An empty file decodes to `""` (not binary, plain path → `""`). Three bytes or fewer never sniff as UTF-16
  (`41 00 42` → `A`, U+0000, `B` via UTF-8).
- `sniffUtf16(buf)`: `n = min(len, 4096)` rounded down to even; `n < 4` → none. Over byte pairs `k = 0, 2, …, n-2`:
  `evenZeros` += `buf[k] == 0`, `oddZeros` += `buf[k+1] == 0`; `pairs = n/2`.
  LE if `oddZeros > 0.4·pairs && evenZeros < 0.05·pairs`; BE if `evenZeros > 0.4·pairs && oddZeros < 0.05·pairs`.
- Windows-1252: bytes map to the same code point except 0x80–0x9F:
  `80→20AC 82→201A 83→0192 84→201E 85→2026 86→2020 87→2021 88→02C6 89→2030 8A→0160 8B→2039 8C→0152 8E→017D
  91→2018 92→2019 93→201C 94→201D 95→2022 96→2013 97→2014 98→02DC 99→2122 9A→0161 9B→203A 9C→0153 9E→017E 9F→0178`;
  `81 8D 8F 90 9D` map to U+0081 etc. This is exactly WHATWG windows-1252 → `encoding_rs::WINDOWS_1252`.
- `decodeCodepage(bytes, cp)` (RTF only): `cp` 1252 or 0 → Windows-1252 above; 65001 → UTF-8 (lossy, U+FFFD; a
  leading BOM in that byte run is removed, as above); otherwise the WHATWG encoding with label `windows-<cp>`
  (Rust: `Encoding::for_label(b"windows-<cp>")`, decode with `decode_without_bom_handling` — `TextDecoder` only
  sniffs BOMs for UTF-8/UTF-16, and invalid sequences become U+FFFD); if the label is unknown → Windows-1252.
  Verified: `\ansicpg1251` → Cyrillic, `\ansicpg949 \'b0\'a1` → `가`, `\ansicpg0 \'e9` → `é`,
  `\ansicpg65001 \'c3\'a9` → `é`. Known labels: `windows-874`, `windows-1250` … `windows-1258`, `windows-949`
  (→ EUC-KR). **Not** known: `windows-932`, `windows-936`, `windows-950` (Japanese/Chinese RTF falls back to 1252 and
  comes out as mojibake — see open questions).

### 5.3 Binary check (text-extract.ts:137-146)

`sample` = first 8192 UTF-16 units of the decoded text; empty → not binary. Count units that are U+FFFD or
`< 0x20` other than TAB (09), LF (0A), FF (0C), CR (0D). Binary iff `count / sample_len > 0.1` → error
`This file does not look like text`.

### 5.4 HTML → text (text-extract.ts:150-262)

Linear-time, regex-light scanner; entities are decoded **after** tags are removed (so `&lt;b&gt;` stays literal
text). Constants:

```text
HTML_SKIP      = script style noscript template svg math iframe object canvas title select head
HTML_BLOCK     = address article aside blockquote body caption dd details dialog div dl dt fieldset figcaption
                 figure footer form h1 h2 h3 h4 h5 h6 header hr html li main nav ol p pre section summary table
                 tbody td tfoot th thead tr ul
HTML_PARAGRAPH = p h1 h2 h3 h4 h5 h6 blockquote pre table ul ol section article
```

Algorithm (indices are into the string):

```text
out = [] ; pending = 0      // pending line break: 0 none, 1 line, 2 paragraph (3+ possible via <br>)
pushText(s): if s empty return
             if pending > 0 and s contains a non-JS-whitespace char: out.push("\n" * pending); pending = 0
             out.push(s)
breakLine(level): pending = max(pending, level)

i = 0
loop:
  lt = index of '<' at or after i;  none -> pushText(html[i..]); break
  if lt > i: pushText(html[i..lt])
  if html[lt..] starts with "<!--": i = index of "-->" after lt+4, plus 3; or end of input; continue
  match /<(\/?)([a-zA-Z][a-zA-Z0-9:-]*)/ anchored at lt:
    no match:
      if html[lt+1] is '!' or '?': i = (index of '>' from lt) + 1, or end   // <!DOCTYPE>, <![CDATA[, <?xml?>
      else: pushText("<"); i = lt + 1                                      // literal '<' in text ("3 < 5")
      continue
  closing = group1 == "/" ; name = lower(group2)
  // find the tag's closing '>' honoring quoted attribute values (they may contain '>')
  j = lt + len(match); quote = none; prev = ''
  while j < n:
    c = html[j]
    if quote: if c == quote: quote = none
    elif c in {'"', '\''} and prev == '=': quote = c
    elif c == '>': break
    if c not in {' ', '\t', '\n', '\r'}: prev = c
    j += 1
  selfClosing = html[j-1] == '/'
  i = j + 1                                  // past '>' (or past the end if none)
  if !closing and !selfClosing and name in HTML_SKIP:
    end = findClosingTag(name, from i)       // index just past the first /<\/name\b[^>]*>/i at or after i, or none
    if name == "head": if end found: i = end  // <head> may legally lack its end tag: only skip when one exists
    else: i = end found ? end : n             // unclosed <script> etc. drops the rest of the document
    continue
  if name == "br":            (opening or closing)
    if pending > 0: breakLine(pending + 1) else out.push("\n")
  elif name in {td, th}:      if closing: out.push("\t")          // cell separator; td/th never break lines
  elif name in HTML_PARAGRAPH: breakLine(2)                        // opening and closing tags alike
  elif name in HTML_BLOCK:     breakLine(1)
  elif name == "img" and !closing:
    alt = first match of /\balt\s*=\s*("([^"]*)"|'([^']*)')/i in html[lt..j]   // quoted alt only
    if alt and alt.trim() non-empty: pushText(" " + alt.trim() + " ")
  // every other tag (inline: span, b, a, …) is removed with NO space inserted ("foo<span>bar</span>" -> "foobar")
result = tidyWhitespace(decodeHtmlEntities(out.join("")))
```

`findClosingTag` is case-insensitive, `\b` after the name (so `</scripts>` does not close `<script>`); nested
same-name skip elements end at the first closing tag. A stray closing skip tag (`</script>`) is simply dropped.

Regex/word-boundary notes for Rust (all verified against the original):

- JS `\b` (no `u` flag) is ASCII-only: word chars are `[A-Za-z0-9_]`. In the Rust `regex` crate write `(?-u:\b)`
  (its default `\b` is Unicode-aware). Consequences to keep: `</script-x>` **does** close `<script>` (`-` is a
  boundary; `<script>a</script-x>b` → `b`), and the `alt` pattern also matches `data-alt=…`
  (`<img data-alt="x" src=1>y` → `x y`).
- `<script/>` / `<svg/>` (self-closing) do not start a skipped region (`<script/>visible` → `visible`).
- The `>`-scanner only enters a quoted value when the quote follows `=` (whitespace in between allowed:
  `<a title= "x>y">z</a>` → `z`); an unquoted `>` ends the tag (`<a href=a>b>c</a>` → `b>c`).
- A tag without a closing `>` swallows the rest of the input (`<div class="a"` → empty).
- `<!-->` does not end the comment (search for `-->` starts after `<!--`): `a<!-->b-->c` → `ac`. `<![CDATA[x]]>` and
  `<?xml …?>` are dropped up to the first `>` (`a<![CDATA[x]]>b` → `ab`).
- `<pre>` content is collapsed like everything else by `tidyWhitespace` (`<pre>  a\n    b</pre>` → `a\nb`).
- `</br>` breaks like `<br>`. A closing `</td>` pushes TAB **directly** (not via `pushText`), so it does not flush a
  pending break: `<p>x</p><td>a</td>b` → `x\n\na b`.
- `alt` text and all other text go through entity decoding afterwards (`alt="A &amp; B"` → `A & B`).

### 5.5 HTML entities (text-extract.ts:264-295)

Regex (global): `&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31})(;?)`

- Numeric (`#…`, decimal or hex): `cp`; if `0x80 ≤ cp ≤ 0x9F` map through the Windows-1252 table (HTML treats these
  C1 controls as cp1252; the five unmapped ones `81 8D 8F 90 9D` stay C1 controls, `&#x81;` → U+0081); if
  `cp == 0`, `> 0x10FFFF`, or a surrogate (D800–DFFF) → U+FFFD; else that char. The `;` is optional (`&#65` → `A`).
  The digit counts are hard limits that include leading zeros: `&#00000065;` matches only `#0000006` → U+0006
  followed by literal `5;` (verified).
- Named bodies need at least **2** characters (`&a;` is never matched), and the name match is greedy over
  `[a-zA-Z0-9]` up to 32 chars (`&lt&gt` → `<>`; `&ampx` → body `ampx`).
- Named: look up the body (case-sensitive). Unknown → leave the whole match unchanged. Known but no `;` → only
  decoded if the name is one of `amp lt gt quot nbsp copy reg` (legacy entities browsers accept without `;`);
  otherwise left unchanged (`&hellip` stays literal, `&copy` → ©, `&ampx` stays literal since the name `ampx` is
  unknown).
- Named table (every entry; values are exact):

```text
amp &   lt <   gt >   quot "   apos '
nbsp, ensp, emsp, thinsp -> " " (U+0020, plain space)
ndash –  mdash —  hellip …  lsquo ‘  rsquo ’  sbquo ‚  ldquo “  rdquo ”  bdquo „
laquo «  raquo »  lsaquo ‹  rsaquo ›  bull •  middot ·  copy ©  reg ®  trade ™
deg °  plusmn ±  times ×  divide ÷  minus −  frac12 ½  frac14 ¼  frac34 ¾
euro €  pound £  yen ¥  cent ¢  sect §  para ¶  dagger †  Dagger ‡  prime ′  Prime ″
larr ←  rarr →  uarr ↑  darr ↓  harr ↔
shy, zwj, zwnj, lrm, rlm -> "" (removed)
iexcl ¡  iquest ¿  micro µ  alpha α  beta β  gamma γ  delta δ  pi π  mu μ  sigma σ
le ≤  ge ≥  ne ≠  asymp ≈  infin ∞  check ✓  star ☆
aacute á  eacute é  iacute í  oacute ó  uacute ú  Aacute Á  Eacute É  Iacute Í  Oacute Ó  Uacute Ú
agrave à  egrave è  igrave ì  ograve ò  ugrave ù  Agrave À  Egrave È
acirc â  ecirc ê  icirc î  ocirc ô  ucirc û
auml ä  euml ë  iuml ï  ouml ö  uuml ü  yuml ÿ  Auml Ä  Ouml Ö  Uuml Ü  szlig ß
ccedil ç  Ccedil Ç  ntilde ñ  Ntilde Ñ  atilde ã  otilde õ  aring å  Aring Å  aelig æ  AElig Æ
oslash ø  Oslash Ø  oelig œ  OElig Œ
```

(`star` is U+2606 WHITE STAR; `check` U+2713; `minus` U+2212; `prime`/`Prime` U+2032/U+2033.)

### 5.6 RTF → text (text-extract.ts:297-436)

Rough parser: drops control words and metadata destinations, keeps paragraphs, `\uN` and `\'hh` characters. Runs
over the decoded text (RTF is 7-bit, so decoding is normally a no-op).

```text
RTF_SKIP (destination words whose group is metadata) =
  fonttbl colortbl stylesheet info pict object objdata header headerl headerr headerf footer footerl footerr footerf
  footnote annotation fldinst xmlnstbl listtable listoverridetable rsidtbl generator themedata colorschememapping
  datastore latentstyles pgdsctbl filetbl revtbl bkmkstart bkmkend nonshppict shpinst pntext pntxta pntxtb private
  userprops docvar xe tc template ftnsep ftnsepc ftncn aftnsep aftnsepc aftncn mmathPr wgrffmtfilter listpicture
  protusertbl defchp defpap
RTF_SYMBOLS = par:"\n" line:"\n" row:"\n" sect:"\n\n" page:"\n\n" tab:"\t" cell:"\t"
              emdash:"—" endash:"–" bullet:"•" lquote:"‘" rquote:"’" ldblquote:"“" rdblquote:"”"
              emspace:" " enspace:" " qmspace:" " (plain U+0020)   zwj:"" zwnj:"" zwbo:"" zwnbo:""
```

State: `out`, a stack of groups, current `group = {skip:false, uc:1}`, `pendingBytes = []`, `ucSkip = 0`,
`codepage = 1252` (document-global). `emit(s)` appends unless `group.skip`. `flushBytes()` emits
`decodeCodepage(pendingBytes, codepage)` (subject to the *current* group's skip) and clears.

```text
while i < n:
  c = rtf[i]
  '{'  : flushBytes; push group; group = copy(group); ucSkip = 0; i += 1
  '}'  : flushBytes; group = pop() or {skip:false, uc:1}; ucSkip = 0; i += 1
  '\\' :
    next = rtf[i+1]; if none: break
    if next == '\'':                               // \'hh : one byte in the ANSI code page
      byte = JS parseInt(rtf[i+2..i+4], 16)        // leading hex digits of those ≤2 chars; NaN if the first isn't hex
                                                   // (JS quirks: leading whitespace and a sign are accepted, so
                                                   //  "\' f" -> 15 -> U+000F, "\'-1" -> -1; never seen in real RTF)
      i += 4
      if ucSkip > 0: ucSkip -= 1                   // it is the ANSI fallback of a preceding \uN
      elif byte is a number: pendingBytes.push(byte)   // consecutive \'hh accumulate (DBCS pairs decode together)
      continue
    flushBytes
    if next is an ASCII letter:                    // control word
      j = i+1; while j < n and j - i <= 32 and rtf[j] is ASCII letter: j += 1     // word ≤ 32 letters
      word = rtf[i+1..j]
      k = j; if rtf[k] == '-' and rtf[k+1] is a digit: k += 1
      if rtf[k] is a digit: while k < n and k - j < 11 and rtf[k] is a digit: k += 1; param = int(rtf[j..k])
      else: k = j; param = none
      if rtf[k] == ' ': k += 1                     // the delimiter space belongs to the control word
      i = k; ucSkip = 0
      if word == "bin":            i += max(0, param or 0)        // skip raw binary payload
      elif word in RTF_SKIP:       group.skip = true
      elif word == "ansicpg" and param is not none: codepage = param      // \ansicpg0 -> 0 -> treated as 1252
      elif word == "uc":           group.uc = max(0, param ?? 1)          // \uc with no number -> 1
      elif word == "u" and param is not none:                             // \u0 counts! (emits U+0000, later removed)
                                   emit(UTF-16 unit (param < 0 ? param + 65536 : param) mod 65536); ucSkip = group.uc
      elif word in RTF_SYMBOLS:    emit(RTF_SYMBOLS[word])
      // all other control words: ignored
      continue
    i += 2                                          // control symbol
    literal = next in {'\\','{','}'} ? next : next == '~' ? " " : next == '_' ? "-" : none
    if literal: if ucSkip > 0: ucSkip -= 1 else emit(literal)
    elif next == '*': group.skip = true             // ignorable destination we do not understand
    elif next is '\n' or '\r': emit("\n")           // backslash-newline = \par
    // others (\- optional hyphen, \: etc.) ignored
  '\r' or '\n': i += 1                              // raw line breaks are not content in RTF
  else:                                             // text run
    j = i; advance j until '\\', '{', '}', '\r', '\n' or end
    flushBytes
    run = rtf[i..j]; i = j
    if ucSkip > 0: skip = min(ucSkip, len(run)); run = run[skip..]; ucSkip -= skip
    if run: emit(run)
flushBytes
return tidyWhitespace(out.join(""))
```

Rust specifics: `\uN` emits a single UTF-16 code unit; astral characters arrive as two `\u` escapes (high then low
surrogate, e.g. `\u-10179?\u-8704?` = 😀) and JS concatenation forms the pair. Build the output as a `Vec<u16>` and
convert at the end with `String::from_utf16_lossy` (or buffer a pending high surrogate). Only look up `RTF_SYMBOLS`
by exact key (the JS `in` operator also matches `Object.prototype` names: `a\constructor b` really outputs
`afunction Object() { [native code] }b` in the old code — a bug, do not copy).

`ucSkip` rules (all verified): it is **reset to 0** by every control word (before the word is handled, so `\uN`
then sets it to `group.uc`) and by `{` / `}`; it is **consumed** one at a time by text characters (counted in
UTF-16 units), by `\'hh`, and by the **five** literal control symbols `\\ \{ \} \~ \_` (`x舒\~y` → `x—y`); it is
**untouched** by other control symbols (`\-`, `\:`, `\*`, backslash-newline): `x舒\-?y` → `x—y`.

More verified edge cases: `a\u0?b` → `ab` (U+0000 emitted, `?` skipped as its fallback, NUL removed by
`tidyWhitespace`); `\uc0 舒 x` → `—x` (no fallback to skip; the space after `舒` is its delimiter);
`舒{}?` → `—?` (the brace reset the skip); a 40-letter control word is cut after 32 letters and the rest is text
(`\` + 40×`a` + ` x` → `aaaaaaaa x`); `\bin-5` skips nothing; `{\*\foo hidden\par}shown` → `shown`. Parameter digits
are limited to 11 characters including the `-` (the remaining digits become text). The `\'hh` byte list is decoded
as raw bytes; with the parseInt quirks above a value can fall outside 0..255 — a Rust port may simply require two
hex digits (or one hex digit followed by a non-hex char) and ignore anything else.

### 5.7 Markdown → text (text-extract.ts:438-476)

Light cleanup so syntax characters are not read aloud; prose untouched. Per line of `normalizeNewlines(md)`
split on `\n`:

```text
inFence = false
for raw in lines:
  if raw matches /^\s*(```|~~~)/ : inFence = !inFence; drop the line; continue   // ``` and ~~~ close each other
  if inFence: keep raw verbatim; continue
  if raw matches /^\s*([-*_])(\s*\1){2,}\s*$/ : keep "" ; continue               // thematic break
  line = raw with, in order:
    1  /^\s{0,3}#{1,6}\s+/                          -> ""     ATX heading marker
    2  /\s+#+\s*$/                                  -> ""     closing hashes, on ANY line (also eats a lone trailing
                                                              " #": "C #" -> "C"; but "C #tag" stays)
    3  /^\s*>\s?/                                   -> ""     one blockquote level
    4  /^(\s*)[-*+]\s+\[[ xX]\]\s+/                 -> "$1"   task-list item
    5  /^(\s*)[-*+]\s+/                             -> "$1"   bullet (numbered "1." items are kept)
    6  /!\[([^\]]*)\]\([^)]*\)/g                    -> "$1"   image -> alt text
    7  /\[([^\]]+)\]\([^)]*\)/g                     -> "$1"   inline link -> text
    8  /\[([^\]]+)\]\[[^\]]*\]/g                    -> "$1"   reference link -> text
    9  /`([^`]+)`/g                                 -> "$1"   inline code
    10 /(\*\*|__)(?=\S)(.+?)(?<=\S)\1/g             -> "$2"   bold
    11 /(^|[\s(])[*_](?=\S)([^*_\n]+?)(?<=\S)[*_](?=[\s).,!?:;]|$)/g -> "$1$2"   italic (open/close need not match)
    12 /~~(?=\S)(.+?)(?<=\S)~~/g                    -> "$1"   strikethrough
  if line matches /^\s*\[[^\]]+\]:\s*\S+/ : drop it        // link reference definition (tested after rewriting)
  keep line
return tidyWhitespace(kept.join("\n"))
```

Patterns without `/g` replace only the first match. Patterns 10–12 need look-ahead/look-behind: use `fancy-regex`
(the `regex` crate has no look-around) or hand-code them; `\s`/`\S` must use the JS whitespace set. Fenced code is
kept but still goes through `tidyWhitespace` (indentation collapses). Pattern 10 is lazy with backtracking, so
`**a **b** c**` → `a **b c**` (the first `**` pairs with the `**` after `b`); fancy-regex backtracks the same way.

More verified outputs: `C #tag` → `C #tag`; `Title ##  ` → `Title`; `#hashtag` → unchanged (no space after `#`);
`####### x` → unchanged (7 hashes); `    # not heading` (4 spaces) → `# not heading` (not a heading; indent then
removed by `tidyWhitespace`); `> > a` → `> a` (one level per line); `a*b*c` → unchanged (italic needs a space or `(`
before the opener); `(*x_ y` → `(x y`; `[](http://x)` → unchanged (link text needs ≥ 1 char; image alt may be
empty); `  - [ ] todo` → `todo`; `  [x]: http://a` → dropped; ```` ``` ```` … `~~~` closes the fence.

### 5.8 Shared helpers (text-extract.ts:478-492)

```text
normalizeNewlines(t) = replace /\r\n?/g with "\n"
tidyWhitespace(t):
  t = normalizeNewlines(t)
  t = remove all U+0000
  t = replace /[ \t\f\v\u00A0]+/g with " "      // collapse runs of spaces, tabs, FF, VT, NBSP
  t = replace / *\n */g with "\n"              // trim spaces around line breaks
  t = replace /\n{3,}/g with "\n\n"            // at most one blank line
  return JS-trim(t)
```

### 5.9 Truncation (text-extract.ts:494-504)

Cuts to `maxChars` UTF-16 units, preferring a whitespace boundary near the end, never splitting a surrogate pair.
(Different from the reader's own `truncateText`; this one runs first.)

```text
truncateText(text, maxChars):
  if !(maxChars > 0) or len16(text) <= maxChars: return text
  end = floor(maxChars)
  if unit(end-1) is a high surrogate: end -= 1
  window = min(200, floor(end / 10))
  w = text[end-window .. end]
  space = index in w of the LAST JS-whitespace char such that only non-whitespace follows it to the end of w
          (= JS w.search(/\s\S*$/); -1 if none)
  if space > 0: end = end - window + space          // space == 0 does not move the cut
  return trimEnd(text[0..end])
```

Notes (verified): `NaN`, `0` and negative limits return the text unchanged; a fractional limit is floored
(`abcdef`@2.5 → `ab`); for `end < 10` the window is 0, so the cut is hard (`aaaa\tbbbb`@6 → `aaaa\tb`); the
whitespace class here is full JS `\s` (TAB, LF, NBSP, U+FEFF … all count), and `trimEnd` is JS-whitespace trimEnd.
Note the plain-text path (`.txt .log .csv .json`) runs only newline normalization, NUL removal and `trim` before
this — runs of spaces and blank lines are kept.

### 5.10 Golden vectors (captured from the original functions)

HTML (`htmlToText`):

| Input | Output |
|---|---|
| `<p>Hello <b>world</b></p><p>Second</p>` | `Hello world\n\nSecond` |
| `<ul><li>One</li><li>Two</li></ul>after` | `One\nTwo\n\nafter` |
| `a<br>b<br><br>c` | `a\nb\n\nc` |
| `<div>x</div><br><div>y</div>` | `x\n\ny` |
| `<table><tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr></table>` | `A B\nC D` |
| `<script>var a="<p>";</script>Visible<style>p{}</style>` | `Visible` |
| `<head><title>T</title></head><body>Body</body>` | `Body` |
| `<head><meta charset=utf-8><p>No end head` | `No end head` |
| `<!-- c --><!DOCTYPE html>x &lt;b&gt; &amp;amp; &nbsp;y &copy &hellip &#8217; &#x1F600; &#150; &#0;` | `x <b> &amp; y © &hellip ’ 😀 – �` |
| `<img alt="A cat" src=x>and <img alt=dog> <img alt='Bird'/>` | `A cat and Bird` |
| `<a title="x>y" href="/">link</a> 3 < 5 and 5>3` | `link 3 < 5 and 5>3` |
| `foo<span>bar</span> baz` | `foobar baz` |
| `<h1>Title</h1>Text<h2>Sub</h2>` | `Title\n\nText\n\nSub` |
| `<svg><text>no</text></svg>yes<svg/>end` | `yesend` |
| `<script>never closed` | `` (empty) |
| `<img alt="A &amp; B">` | `A & B` |
| `<img data-alt="x" src=1>y` | `x y` |
| `<script>a</script-x>b` | `b` |
| `<script/>visible` | `visible` |
| `<a href=a>b>c</a>` | `b>c` |
| `<p>x</p><td>a</td>b` | `x\n\na b` |
| `a</br>b` | `a\nb` |
| `<dl><dt>T</dt><dd>D</dd></dl>` | `T\nD` |
| `a<!-->b-->c` | `ac` |
| `a<![CDATA[x]]>b` / `a<?xml x?>b` | `ab` |
| `<ul><li>a<ul><li>b</li></ul></li></ul>` | `a\n\nb` |
| `<div class="a"` | `` (empty) |

Entities: `&amp`→`&`, `&ampx`→`&ampx`, `&notanentity;` unchanged, `&Hellip;` unchanged, `&#x110000;`→`�`,
`&#xD800;`→`�`, `&#128;`→`€`, `&#x9F;`→`Ÿ`, `&#65`→`A`, `&lt&gt`→`<>`.

Markdown (`markdownToText`):

| Input | Output |
|---|---|
| `` # Title #\n\nSome **bold** and *italic* and _under_ and `code`. `` | `Title\n\nSome bold and italic and under and code.` |
| `- item one\n* item two\n+ item three\n- [x] done task\n1. numbered` | `item one\nitem two\nitem three\ndone task\n1. numbered` |
| `> quote\n>> nested` | `quote\n> nested` |
| `[link](http://x) ![alt](img.png) [ref][1]\n\n[1]: http://example.com` | `link alt ref` |
| `` ```js\nconst a = **1**;\n```\nafter `` | `const a = **1**;\nafter` |
| `---\n***\n- - -` | `` (empty) |
| `snake_case_name and 2*3*4 and C #` | `snake_case_name and 2*3*4 and C` |
| `~~strike~~ and __bold__` | `strike and bold` |
| `Line   with    spaces\n\n\n\nnext` | `Line with spaces\n\nnext` |

RTF (`rtfToText`):

| Input | Output |
|---|---|
| `{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}\f0\fs24 Hello \b world\b0 !\par Second line\par}` | `Hello world!\nSecond line` |
| `{\rtf1 Caf\'e9 \u8212? dash \uc2\u8364\'80\'80 euro\uc1  \u-10179?\u-8704? emoji}` | `Café — dash € euro 😀 emoji` |
| `{\rtf1{\*\generator Riched20;}{\info{\title T}}Body\line next\tab tab \emdash  \~nbsp\_hyphen \{brace\}}` | `Body\nnext tab — nbsp-hyphen {brace}` |
| `{\rtf1\ansi\ansicpg1251 \'cf\'f0\'e8\'e2\'e5\'f2}` | `Привет` |
| `{\rtf1\ansi\ansicpg932 \'82\'a0 kana}` | `‚ kana` (932 unsupported → cp1252 fallback) |
| `{\rtf1{\field{\*\fldinst HYPERLINK "http://x"}{\fldrslt link text}}}` | `link text` |
| `{\rtf1 a\` + newline + `b \bin3 XYZafter}` | `a\nb after` |
| `{\rtf1 a\u0?b}` | `ab` |
| `{\rtf1 x舒\~y}` / `{\rtf1 x舒\_y}` | `x—y` (the literal symbol was the `\u` fallback) |
| `{\rtf1 x舒\-?y}` | `x—y` (`\-` leaves `ucSkip`, `?` is skipped) |
| `{\rtf1\uc0 舒 x}` | `—x` |
| `{\rtf1 舒{}?}` | `—?` |
| `{\rtf1\ansicpg949 \'b0\'a1}` | `가` |
| `{\rtf1\ansicpg65001 \'c3\'a9}` / `{\rtf1\ansicpg0 \'e9}` | `é` |
| `{\rtf1 {\*\foo hidden\par}shown}` | `shown` |

Decoding (`decodeTextBuffer`, bytes in hex):

| Bytes | truncated | Output |
|---|---|---|
| `EF BB BF 68 69` | no | `hi` |
| `FF FE 68 00 69 00` | no | `hi` |
| `FE FF 00 68 00 69` | no | `hi` |
| `68 00 69 00 21 00 21 00` (sniffed LE) | no | `hi!!` |
| `43 61 66 E9 20 93 71 94` | no | `Café “q”` (cp1252) |
| UTF-8 `héllo wörld ünïcödé` + `FF` | no | `héllo wörld ünïcödé\uFFFD` (lossy kept: 1 replacement × 4 < 13 high bytes) |
| `61 E2 82` | yes | `a` |
| `61 E2 82` | no | `aâ‚` (strict fails; 1 replacement ×4 ≥ 2 high bytes → cp1252) |
| `FF FE 68 00 69` | no | `h` |
| `FF FE 68 00 3D D8` | no | `h` (trailing high surrogate dropped) |
| `EF BB BF EF BB BF 41` | no | `A` (decoder drops the second BOM too) |
| `EF BB BF EF BB BF 41 E9 E9` | no | `ï»¿Aéé` (cp1252 fallback keeps it) |
| `FF FE FF FE 41 00` | no | U+FEFF `A` (UTF-16 keeps a second BOM) |
| `41 00 42` | no | `A` U+0000 `B` (too short to sniff) |
| `81 8D` | no | U+0081 U+008D |
| (empty) | no | `` |

Truncation (`truncateText`): `"word " × 50 + "end"` at 100 → 20 × `word` joined by spaces (cut at the space at index
99, trimmed); `abcdef`@2 → `ab`; 50 × `x` @30 → 30 × `x`; `ab😀cd`@3 → `ab`; any text @0 → unchanged;
`abcdef`@2.5 → `ab`; `abcdef`@NaN or @−1 → unchanged; `aaaa\tbbbb`@6 → `aaaa\tb`; `aaaaa` + space + 14×`b` @19 →
`aaaaa` + space + 13×`b` (window 1: hard cut).

All vectors in §5.10 were re-run against the original functions (esbuild-transpiled `text-extract.ts`) during review.

---

## 6. Rust porting notes (summary)

| Concern | Recommendation |
|---|---|
| HTTP server | `tokio` + `hyper` 1.x (`server::conn::http1`), bound via `socket2` with `SO_EXCLUSIVEADDRUSE` (set before `bind`, never `SO_REUSEADDR`) on `127.0.0.1` only; map `WSAEADDRINUSE` 10048 / `WSAEACCES` 10013 / `WSAEADDRNOTAVAIL` 10049 to the three exact strings of §1.2. Router = a small ordered checker reproducing §1.4 (not a framework router, so the 403/404/405/415/413 precedence matches). Responses built by one `json_response(status, value, extra_headers)` helper that always adds `Content-Type: application/json; charset=utf-8`, `Content-Length`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, omits the body for HEAD, and never adds CORS headers. If answering before the body was read (403/415/413), drain it or add `Connection: close`. |
| Headers | Host/Content-Type: use the **first** value; Origin/x-vox-client: join all values with `", "` (Node semantics, §1.4). Path: handle `//authority/...` (empty host → 400 `Bad URL`), drop `?`/`#`, `\`→`/`, strip trailing `/`. |
| Body limits/timeouts | `http_body_util::Limited` (2 MiB) inside `tokio::time::timeout(15 s)`; also reject a declared `Content-Length` > 2 MiB before reading. Handler: wrap `/read` in `timeout(15 s)` → 504, but do not cancel the read itself (spawn it; the old app lets it start late). `header_read_timeout(10 s)` with `TokioTimer`, keep-alive on (5 s idle), `max_buf_size` ≈ 16 KiB for the header cap, `Semaphore(64)` around accepted sockets, 20 s idle guard. Raw 400/431 bytes for malformed/timed-out headers are optional (the extension never triggers them). |
| JSON | Decode with `String::from_utf8_lossy`, strip one leading U+FEFF, **rewrite lone-surrogate `\u` escapes to `\uFFFD`** (the extension sends them, §1.4 step 12), then `serde_json::Value`; validate by hand so error strings match byte for byte; compare numbers via `as_f64()`. |
| Start/stop | one owner task or async mutex serializing start/stop/restart; `stop` aborts all connection tasks (keep a `JoinSet` or a cancellation token). |
| Read result | the `/read` handler awaits the reader's "session started" result (`ReadResult`), never synthesis completion. |
| Registry (Explorer verb, autostart) | `winreg` or `windows`/`windows-sys` (`RegCreateKeyExW`, `RegSetValueExW` REG_SZ, `RegDeleteTreeW`/`RegDeleteKeyW`, `RegDeleteValueW`, `RegGetValueW` with `RRF_NOEXPAND`). No `reg.exe`, which removes the OEM-code-page decoding, the argv quoting and the "non-ASCII command always counts as current" exception. Treat `ERROR_FILE_NOT_FOUND` on delete as success. Run the nine extension installs (parallel or sequential, it does not matter with the direct API), roll everything back if any fails, report the first failure in extension order. Serialize install/remove (one mutex) so a quick on/off toggle cannot interleave writes. No `SHChangeNotify` needed. |
| Command line | implement `quoteWindowsArg` exactly (§3.2) so existing installs compare as `current`. |
| Autostart | read `HKCU\...\Run\Vox` and parse it with `CommandLineToArgvW` (`windows::Win32::UI::Shell::CommandLineToArgvW`); compare argv[0] case-insensitively and the remaining args exactly (**including** `--hidden` — fixing the old Electron-parser gap, §4.2); equal → do nothing; else write REG_SZ `"<exe>"` or `"<exe>" --hidden` and delete `HKCU\...\Explorer\StartupApproved\Run\Vox`. Disabling deletes both values. Never touch HKLM. |
| Single instance | required so Explorer's `--read-file` reaches the running app (shell.md §3.3): named mutex + named pipe (or `WM_COPYDATA`) forwarding argv and cwd. |
| Encodings | `encoding_rs` (`WINDOWS_1252`; `Encoding::for_label(b"windows-<cp>")` + `decode_without_bom_handling` for RTF `\ansicpg`), `std::str::from_utf8` (strict) / `String::from_utf8_lossy` (same maximal-subpart U+FFFD count as WHATWG) — then strip one leading U+FEFF, as `TextDecoder` does; `char::decode_utf16` for UTF-16 with lone surrogates → U+FFFD. |
| RTF | build the output as `Vec<u16>` (`\uN` emits UTF-16 units; astral chars arrive as surrogate pairs), convert with `String::from_utf16_lossy` at the end; look up `RTF_SYMBOLS`/`RTF_SKIP` by exact key only; treat `\u0` / `\ansicpg0` as present parameters. |
| Regex | `fancy-regex` for the Markdown look-around patterns, or hand-written matchers; JS whitespace set for `\s`/`\S`/`trim` (includes U+FEFF; excludes U+0085). Use ASCII word boundaries `(?-u:\b)` for the HTML `alt` and closing-tag patterns. |
| Lengths | UTF-16 units (`encode_utf16().count()`), never cutting inside a surrogate pair, for: `maxChars`, the 300-char title, 2048-char URL, 8192-unit binary sample, the 200-unit truncation window, RTF `ucSkip` counting. |
| File read | `std::fs::File::open` (default share mode READ\|WRITE\|DELETE like Node), `metadata().is_file()`, read ≤ 5 MiB (5 242 880 bytes) from offset 0, keep what was read on early EOF. |
| Extension | ship `browser-extension/` byte for byte (incl. `popup.css`, `fonts/`, committed `icons/`), e.g. keep `<install>\resources\browser-extension`; expose its path for "Show extension folder" (open with `ShellExecuteW`/`explorer.exe <dir>`). |
| Uninstaller | keep the NSIS-equivalent step removing the 9 `VoxRead` verb keys and the `Vox` values in `Run` and `StartupApproved\Run`, skipped on update; keep `%APPDATA%\Vox`. |

---

## 7. Discrepancies and open questions

1. **contracts.md §1.10 / SPEC.md wording**: they say the Host/Origin checks apply to "every non-GET request". In the
   code the `OPTIONS` refusal, the `Host` check and the `Origin` check apply to **every** request, `GET /health`
   included; only the `x-vox-client` header and the JSON content type are limited to `POST /read` and
   `POST /command` (local-server.ts:190-208). Also, `file:` and `null` Origins are refused, not just `http(s)`.
   This spec follows the code.
2. **Stale popup hint**: the extension popup says `Must match Settings → Triggers → Browser extension in the Vox app`,
   but the app shows the bridge under **Settings → Integrations → Browser extension** (the extension README says
   Integrations). Since the extension ships unchanged, decide whether the Rust Settings keeps the bridge under
   "Integrations" (accepting the stale hint) or the extension gets a one-line copy fix.
3. **"Launch at login" copy vs behavior**: the toggle promises to start "quietly in the tray", but `--hidden` is only
   added when `Start minimized` is also on; otherwise a login launch opens the main window (§4.3). Keep or fix?
4. **RTF code pages 932/936/950**: `windows-932/936/950` are not WHATWG labels, so Japanese/Chinese RTF decodes as
   cp1252 mojibake. Rust could map 932→Shift_JIS, 936→GBK, 950→Big5 (`encoding_rs`), a deliberate improvement over
   parity. Decide. (Also: `\fcharsetN` per font is ignored entirely — only the document-global `\ansicpg` is used.)
5. **Windows 11 top-level menu**: the classic verb only appears under "Show more options". Showing it in the compact
   menu needs an `IExplorerCommand` COM handler registered through a sparse MSIX package (package identity). Out of
   scope for parity; flag if the redesign wants it.
6. **`%1` vs `%L`**: the verb uses `"%1"`. `%L` guarantees a long file name; switching would make every existing
   install read as `stale` once (auto-rewritten at startup when enabled). Keep `%1` unless there is a reason.
7. **Dev vs installed detection** in Rust (for the `Vox (dev)` Run value name and the dev icon path): the old code
   used Electron's `app.isPackaged`. Choose an equivalent (e.g. `cfg!(debug_assertions)` or "exe not under the
   install directory").
8. **Firefox** is in the bridge's foreground-browser allowlist but the extension is Chromium-only (MV3 without
   Gecko keys). Harmless; keep for parity.
9. **Launch-at-login idempotency was broken in the old build** (§4.2, verified with Electron 44.7.0): Electron's
   `launchItems` drop `--switch` args and mis-parse an unquoted exe path containing a space, so with
   `Start minimized` on, or with a space in the install path, every startup rewrote the Run value and deleted the
   `StartupApproved` value — re-enabling an entry the user had disabled in Task Manager. Recommendation: implement
   the documented intent (exact comparison including `--hidden`). Confirm this deliberate fix.
10. **Old Run value bytes not re-verified**: whether Electron 44 wrote the exe quoted (`"…\Vox.exe"`) was not
    checked (doing so means writing to the registry). It does not matter if the Rust reader parses with
    `CommandLineToArgvW`, but a golden test would need one real sample from a machine with the old build.
11. **Lone surrogates in bridge JSON**: the old server passes them through; the Rust server will turn them into
    U+FFFD (Rust strings cannot hold them). Accept this tiny difference (it only affects the last character of a
    200 000-unit selection or a 300-unit title)?
12. **`file://` URLs**: the server keeps `file://` page URLs but the app only stores `http(s)` as `sourceUrl`, so they
    are silently dropped (§1.3). Keep for parity, or store file URLs too?
13. **Extension folder location**: keep `<install>\resources\browser-extension` (what the README and existing users
    know; Chrome remembers the unpacked path, so moving it breaks existing installs until reloaded) or move it?
14. **Raw 400/431 parity**: should the Rust server reproduce Node's exact `HTTP/1.1 400 Bad Request\r\nConnection:
    close\r\nContent-Length: 0\r\n\r\n` for malformed/timed-out headers (including the 16 s whole-request timeout),
    or is hyper's default handling acceptable? The extension never triggers these paths.
15. **Number edge cases**: `1e400` gives 400 `"rate" must be a finite number` in the old server but would give 400
    `Invalid JSON` with `serde_json`. Accept?
