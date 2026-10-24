// Popup: connection status, port setting, read buttons and basic transport controls.
/* global chrome, VoxBridge */
'use strict'

const $ = (id) => document.getElementById(id)
const { APP_NAME } = VoxBridge
const POLL_MS = 2500
const METER_DOTS = 24

const state = {
  port: VoxBridge.DEFAULT_PORT,
  online: null, // null = unknown yet
  playing: false,
  checkSeq: 0
}

function buildMeter() {
  const meter = $('meter')
  for (let i = 0; i < METER_DOTS; i++) {
    const dot = document.createElement('i')
    dot.style.setProperty('--i', String(i))
    meter.appendChild(dot)
  }
}

// ───────────────────────────── status ─────────────────────────────

async function checkHealth() {
  const seq = ++state.checkSeq
  let info = null
  try {
    info = await VoxBridge.health(state.port)
  } catch {
    info = null
  }
  if (seq !== state.checkSeq) return // a newer check (e.g. after a port change) superseded this one
  render(info)
}

function render(info) {
  const online = Boolean(info && info.ok)
  state.online = online
  state.playing = online && info.playing === true

  const status = $('status')
  status.dataset.state = online ? 'online' : 'offline'
  $('status-text').textContent = online ? `${info.app || APP_NAME} ${info.version || ''}`.trim() : 'Offline'

  $('meter').dataset.state = online ? (state.playing ? 'playing' : 'online') : 'offline'
  $('sub').innerHTML = online
    ? `Right-click any text and choose <b>Read with ${APP_NAME}</b>.`
    : `${APP_NAME} isn't running on port ${state.port}. Open the app, then try again.`

  const toggle = $('toggle')
  toggle.disabled = !online
  $('stop').disabled = !online
  toggle.dataset.playing = String(state.playing)
  toggle.setAttribute('aria-label', state.playing ? 'Pause' : 'Resume')
  $('playback-label').textContent = !online ? 'Player offline' : state.playing ? 'Now playing' : 'Player idle'
}

// ───────────────────────────── notices ─────────────────────────────

function showNotice(message, kind = 'error', at = Date.now()) {
  const when = kind === 'error' ? `<small>${timeAgo(at)}</small>` : ''
  $('notice-text').innerHTML = `${escapeHtml(message)}${when}`
  $('notice').dataset.kind = kind
  $('notice').hidden = false
}

async function dismissNotice() {
  $('notice').hidden = true
  try {
    await chrome.storage.local.remove('lastError')
    await chrome.action.setBadgeText({ text: '' })
  } catch {
    // ignore
  }
}

async function showStoredError() {
  try {
    const { lastError } = await chrome.storage.local.get('lastError')
    if (lastError && typeof lastError.message === 'string') showNotice(lastError.message, 'error', lastError.at)
  } catch {
    // ignore
  }
}

function timeAgo(at) {
  const sec = Math.max(0, Math.round((Date.now() - (Number(at) || Date.now())) / 1000))
  if (sec < 60) return 'Just now'
  if (sec < 3600) return `${Math.round(sec / 60)} min ago`
  if (sec < 86400) return `${Math.round(sec / 3600)} h ago`
  return new Date(at).toLocaleDateString()
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c])
}

// ───────────────────────────── actions ─────────────────────────────

async function requestRead(button, type) {
  if (button.getAttribute('aria-busy') === 'true') return
  button.setAttribute('aria-busy', 'true')
  try {
    const res = await chrome.runtime.sendMessage({ type })
    if (res && res.ok) {
      showNotice(`Sent to ${APP_NAME}.`, 'ok')
      setTimeout(checkHealth, 600)
    } else {
      showNotice((res && res.error) || `Could not reach ${APP_NAME}.`)
    }
  } catch (err) {
    showNotice((err && err.message) || String(err))
  } finally {
    button.removeAttribute('aria-busy')
  }
}

async function sendCommand(cmd) {
  try {
    await VoxBridge.command(cmd)
  } catch (err) {
    showNotice((err && err.message) || String(err))
  }
  await checkHealth()
}

async function savePort() {
  const input = $('port')
  const value = Number(input.value.trim())
  if (!VoxBridge.validPort(value)) {
    input.setAttribute('aria-invalid', 'true')
    return
  }
  input.removeAttribute('aria-invalid')
  if (value === state.port) return
  state.port = value
  try {
    await VoxBridge.setPort(value)
  } catch (err) {
    showNotice((err && err.message) || String(err))
  }
  $('status').dataset.state = 'checking'
  $('status-text').textContent = 'Checking'
  await checkHealth()
}

async function renderShortcut() {
  const host = $('shortcut')
  let shortcut = ''
  try {
    const commands = await chrome.commands.getAll()
    const cmd = commands.find((c) => c.name === 'read-selection')
    shortcut = (cmd && cmd.shortcut) || ''
  } catch {
    shortcut = 'Alt+Shift+R'
  }
  host.textContent = ''
  if (!shortcut) {
    host.textContent = 'a shortcut (not set)'
    return
  }
  shortcut.split('+').forEach((key, i) => {
    if (i) host.append('+')
    const kbd = document.createElement('kbd')
    kbd.textContent = key
    host.append(kbd)
  })
}

function openShortcutSettings(event) {
  event.preventDefault()
  const edge = navigator.userAgent.includes('Edg/')
  chrome.tabs.create({ url: edge ? 'edge://extensions/shortcuts' : 'chrome://extensions/shortcuts' })
}

// ───────────────────────────── boot ─────────────────────────────

async function init() {
  buildMeter()
  state.port = await VoxBridge.getPort()
  $('port').value = String(state.port)

  $('read-selection').addEventListener('click', (e) => requestRead(e.currentTarget, 'vox:read-selection'))
  $('read-page').addEventListener('click', (e) => requestRead(e.currentTarget, 'vox:read-page'))
  $('toggle').addEventListener('click', () => sendCommand({ type: 'toggle' }))
  $('stop').addEventListener('click', () => sendCommand({ type: 'stop' }))
  $('notice-close').addEventListener('click', dismissNotice)
  $('shortcut-link').addEventListener('click', openShortcutSettings)
  $('port').addEventListener('change', savePort)
  $('port').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') savePort()
  })
  $('port').addEventListener('input', (e) => {
    e.currentTarget.value = e.currentTarget.value.replace(/\D/g, '').slice(0, 5)
  })

  await Promise.all([renderShortcut(), showStoredError(), checkHealth()])
  setInterval(checkHealth, POLL_MS)
}

init()
