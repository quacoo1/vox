// Talks to the Vox desktop app's local bridge (http://127.0.0.1:<port>).
// Shared by the service worker (importScripts) and the popup (<script>). Plain JS, no build step.
/* global chrome */
;(function (root) {
  'use strict'

  // The extension cannot import the app's TypeScript constants; keep these in sync with src/shared/constants.ts.
  const APP_NAME = 'Vox'
  const DEFAULT_PORT = 47823
  const CLIENT_HEADER = 'x-vox-client'
  const CLIENT_VALUE = 'browser-extension'
  // The bridge accepts 2 MB bodies; the app trims to its own character limit anyway.
  const MAX_TEXT_CHARS = 200000

  class BridgeError extends Error {
    constructor(message, kind) {
      super(message)
      this.name = 'BridgeError'
      this.kind = kind // 'offline' | 'timeout' | 'http' | 'read'
    }
  }

  function validPort(port) {
    return Number.isInteger(port) && port >= 1024 && port <= 65535
  }

  async function getPort() {
    try {
      const { port } = await chrome.storage.sync.get({ port: DEFAULT_PORT })
      return validPort(port) ? port : DEFAULT_PORT
    } catch {
      return DEFAULT_PORT
    }
  }

  async function setPort(port) {
    if (!validPort(port)) throw new BridgeError('Port must be a number between 1024 and 65535.', 'http')
    await chrome.storage.sync.set({ port })
  }

  async function request(path, { method = 'GET', body, timeoutMs = 4000, port } = {}) {
    const p = port ?? (await getPort())
    const headers = {}
    if (method !== 'GET') {
      headers['content-type'] = 'application/json'
      headers[CLIENT_HEADER] = CLIENT_VALUE
    }
    let res
    try {
      res = await fetch(`http://127.0.0.1:${p}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(timeoutMs),
        cache: 'no-store',
        credentials: 'omit'
      })
    } catch (err) {
      if (err && (err.name === 'TimeoutError' || err.name === 'AbortError')) {
        throw new BridgeError(`${APP_NAME} did not answer in time.`, 'timeout')
      }
      throw new BridgeError(`${APP_NAME} isn't running. Start the ${APP_NAME} app (port ${p}).`, 'offline')
    }
    let data = null
    try {
      data = await res.json()
    } catch {
      // non-JSON answer: something else is listening on this port
    }
    if (!res.ok) {
      const detail = data && typeof data.error === 'string' ? data.error : `HTTP ${res.status}`
      throw new BridgeError(`${APP_NAME} refused the request: ${detail}`, 'http')
    }
    if (!data) throw new BridgeError(`Port ${p} is used by another program, not ${APP_NAME}.`, 'http')
    return data
  }

  /** GET /health → { ok, app, version, playing } */
  function health(port) {
    return request('/health', { port, timeoutMs: 1500 })
  }

  /** POST /read → ReadResult. Throws BridgeError when the app refuses or cannot start the read. */
  async function read({ text, url, title }) {
    const body = { text: String(text || '').slice(0, MAX_TEXT_CHARS) }
    if (typeof url === 'string' && /^(https?|file):/i.test(url)) body.url = url
    if (typeof title === 'string' && title.trim()) body.title = title.trim().slice(0, 300)
    const result = await request('/read', { method: 'POST', body, timeoutMs: 20000 })
    if (result.ok !== true) throw new BridgeError(result.error || `${APP_NAME} could not read that.`, 'read')
    return result
  }

  /** POST /command with a PlayerCommand such as { type: 'toggle' } or { type: 'stop' }. */
  function command(cmd) {
    return request('/command', { method: 'POST', body: cmd })
  }

  root.VoxBridge = Object.freeze({ APP_NAME, DEFAULT_PORT, validPort, getPort, setPort, health, read, command, BridgeError })
})(typeof self !== 'undefined' ? self : globalThis)
