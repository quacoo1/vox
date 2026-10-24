// Service worker: context-menu entries, the keyboard command, and reads requested by the popup.
/* global chrome, importScripts, VoxBridge */
importScripts('bridge.js')

const { APP_NAME } = VoxBridge
const MENU_SELECTION = 'vox-read-selection'
const MENU_PAGE = 'vox-read-page'
const PAGE_MAX_CHARS = 20000
const ACCENT_RED = '#D71921'
const DEFAULT_TITLE = `Read with ${APP_NAME}`

// ───────────────────────────── menus & triggers ─────────────────────────────

function createMenus() {
  chrome.contextMenus.removeAll(() => {
    void chrome.runtime.lastError
    chrome.contextMenus.create({ id: MENU_SELECTION, title: `Read with ${APP_NAME}`, contexts: ['selection'] })
    chrome.contextMenus.create({ id: MENU_PAGE, title: `Read page with ${APP_NAME}`, contexts: ['page'] })
  })
}

chrome.runtime.onInstalled.addListener(createMenus)
chrome.runtime.onStartup.addListener(createMenus)

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === MENU_SELECTION) void run(() => readSelection(tab, info))
  else if (info.menuItemId === MENU_PAGE) void run(() => readPage(tab))
})

chrome.commands.onCommand.addListener((command, tab) => {
  if (command === 'read-selection') void run(async () => readSelection(tab || (await activeTab())))
})

// Requests from our own popup: { type: 'vox:read-selection' | 'vox:read-page' } → { ok, error? }
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id || !msg) return false
  if (msg.type !== 'vox:read-selection' && msg.type !== 'vox:read-page') return false
  run(async () => {
    const tab = await activeTab()
    return msg.type === 'vox:read-page' ? readPage(tab) : readSelection(tab)
  }).then(sendResponse)
  return true // keep the channel open for the async answer
})

// ───────────────────────────── reading ─────────────────────────────

async function activeTab() {
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true })
  return tab
}

async function readSelection(tab, info) {
  if (!tab || tab.id === undefined) throw new Error('No active tab.')
  let text = ''
  let secret = false
  // Prefer the page's own selection: it keeps line breaks (info.selectionText collapses them).
  try {
    const target = { tabId: tab.id }
    if (info && typeof info.frameId === 'number') target.frameIds = [info.frameId]
    const results = await chrome.scripting.executeScript({ target, func: selectionInPage })
    secret = results.some((r) => r && r.result === null)
    text = pickLongest(results)
    if (!text.trim() && !info && !secret) {
      // Keyboard shortcut: the selection may live in an iframe.
      const all = await chrome.scripting.executeScript({ target: { tabId: tab.id, allFrames: true }, func: selectionInPage })
      text = pickLongest(all)
    }
  } catch {
    // Restricted pages (PDF viewer, chrome://, the Web Store) cannot be scripted — use the menu's copy below.
  }
  if (secret) throw new Error('Password fields are never read aloud.')
  if (!text.trim() && info && info.selectionText) text = info.selectionText
  if (!text.trim()) {
    throw new Error(info ? 'Select some text first.' : `Select some text first. (On protected pages, right-click → ${DEFAULT_TITLE}.)`)
  }
  await VoxBridge.read({ text, url: tab.url, title: tab.title })
}

async function readPage(tab) {
  if (!tab || tab.id === undefined) throw new Error('No active tab.')
  let page = null
  try {
    const [res] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: extractPage, args: [PAGE_MAX_CHARS] })
    page = res && res.result
  } catch {
    throw new Error("The browser doesn't let extensions read this page. Select the text and use Read with Vox instead.")
  }
  if (!page || !page.text || !page.text.trim()) throw new Error('No readable text found on this page.')
  await VoxBridge.read({ text: page.text, url: tab.url, title: page.title || tab.title })
}

function pickLongest(results) {
  let best = ''
  for (const r of results || []) {
    if (r && typeof r.result === 'string' && r.result.trim().length > best.trim().length) best = r.result
  }
  return best
}

// ───────────────────────────── status badge ─────────────────────────────

async function run(task) {
  try {
    await task()
    await clearError()
    return { ok: true }
  } catch (err) {
    const message = (err && err.message) || String(err)
    await reportError(message)
    return { ok: false, error: message }
  }
}

async function reportError(message) {
  try {
    await chrome.storage.local.set({ lastError: { message, at: Date.now() } })
    await chrome.action.setBadgeBackgroundColor({ color: ACCENT_RED })
    if (chrome.action.setBadgeTextColor) await chrome.action.setBadgeTextColor({ color: '#FFFFFF' })
    await chrome.action.setBadgeText({ text: '!' })
    await chrome.action.setTitle({ title: `${DEFAULT_TITLE} — ${message}` })
  } catch {
    // badge/storage failures must never mask the original error
  }
}

async function clearError() {
  try {
    await chrome.storage.local.remove('lastError')
    await chrome.action.setBadgeText({ text: '' })
    await chrome.action.setTitle({ title: DEFAULT_TITLE })
  } catch {
    // ignore
  }
}

// ───────────────────────────── injected into the page ─────────────────────────────
// These run in the page via chrome.scripting.executeScript, so they must be self-contained.

function selectionInPage() {
  const el = document.activeElement
  if (el && el.tagName === 'INPUT' && el.type === 'password') return null // never read secrets aloud
  if (el && (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT')) {
    try {
      const start = el.selectionStart
      const end = el.selectionEnd
      if (typeof start === 'number' && typeof end === 'number' && end > start) return el.value.slice(start, end)
    } catch {
      // input types without selection support (email, number) throw
    }
  }
  const sel = window.getSelection()
  return sel ? sel.toString() : ''
}

function extractPage(maxChars) {
  const textLength = (el) => (el.textContent || '').replace(/\s+/g, ' ').trim().length
  const pickRoot = () => {
    let best = null
    let bestLen = 0
    for (const a of document.querySelectorAll('article')) {
      const len = textLength(a)
      if (len > bestLen) {
        best = a
        bestLen = len
      }
    }
    if (best && bestLen >= 200) return best
    const main = document.querySelector('main, [role="main"]')
    if (main && textLength(main) >= 200) return main
    return best || main || document.body
  }

  const SKIP_TAGS = new Set([
    'script', 'style', 'noscript', 'template', 'svg', 'math', 'canvas', 'iframe', 'object', 'embed', 'video', 'audio',
    'nav', 'aside', 'footer', 'form', 'button', 'select', 'input', 'textarea', 'dialog', 'menu'
  ])
  const SKIP_ROLES = new Set(['navigation', 'complementary', 'contentinfo', 'banner', 'search', 'menu', 'menubar', 'toolbar', 'dialog'])
  const PARAGRAPH = /^(p|h[1-6]|blockquote|pre|table|ul|ol|dl|section|article|figure)$/

  const root = pickRoot()
  if (!root) return { text: '', title: document.title }
  const skipHeaders = root === document.body
  const limit = maxChars * 1.2
  const parts = []
  let length = 0
  let pendingBreak = 0

  const push = (s) => {
    if (!s) return
    if (pendingBreak && /\S/.test(s)) {
      parts.push(pendingBreak > 1 ? '\n\n' : '\n')
      pendingBreak = 0
    }
    parts.push(s)
    length += s.length
  }

  const walk = (node) => {
    for (let child = node.firstChild; child && length < limit; child = child.nextSibling) {
      if (child.nodeType === Node.TEXT_NODE) {
        push(child.nodeValue.replace(/\s+/g, ' '))
        continue
      }
      if (child.nodeType !== Node.ELEMENT_NODE) continue
      const tag = child.localName
      if (SKIP_TAGS.has(tag) || (skipHeaders && tag === 'header')) continue
      const role = child.getAttribute('role')
      if ((role && SKIP_ROLES.has(role)) || child.hidden || child.getAttribute('aria-hidden') === 'true') continue
      if (tag === 'br') {
        pendingBreak = Math.max(pendingBreak, 1)
        continue
      }
      const style = getComputedStyle(child)
      if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') continue
      const block = !style.display.startsWith('inline') && style.display !== 'contents'
      const level = block ? (PARAGRAPH.test(tag) ? 2 : 1) : 0
      pendingBreak = Math.max(pendingBreak, level)
      walk(child)
      pendingBreak = Math.max(pendingBreak, level)
    }
  }
  walk(root)

  let text = parts
    .join('')
    .replace(/[ \t ]+/g, ' ')
    .replace(/ *\n */g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .trim()
  // Pages built from shadow DOM / custom elements can defeat the walk: fall back to rendered text.
  if (text.length < 200) {
    const rendered = (document.body && document.body.innerText ? document.body.innerText : '').trim()
    if (rendered.length > text.length) text = rendered
  }
  if (text.length > maxChars) {
    const cut = text.lastIndexOf(' ', maxChars)
    text = text.slice(0, cut > maxChars * 0.9 ? cut : maxChars).trimEnd()
  }
  return { text, title: document.title }
}
