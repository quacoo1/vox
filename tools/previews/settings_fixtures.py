"""Settings page preview fixtures and renders.

Writes ui/previews/settings-<state>.json (one fixture per state the spec defines) and renders each one in the dark and
light themes with target/release/vox-shot.exe into shots/ui/settings/<state>-{dark,light}.png.

    python tools/previews/settings_fixtures.py            # write the fixtures
    python tools/previews/settings_fixtures.py render     # write the fixtures, then render all of them
    python tools/previews/settings_fixtures.py render voice library   # only states whose name contains a filter

Run from the repository root. Page fixtures use ui/previews/settings.slint (SettingsPage at a given page size, deep
linked with `scroll-to`); `section-*` fixtures use ui/previews/settings-sections.slint (one whole section, no
scrolling, at a given window width; the height follows the content).

Page sizes: the content area of the main window (MainWindow: panel inset 14 on each side, header 64 + divider,
sidebar 260 or the 72-px rail below 1100 px, + divider):
    1180 x 780 window  -> 892 x 700 page   (the default)
    1100 x 640 window  -> 811 x 548 page   (narrowest page: full sidebar; 514-px settings column)
     940 x 640 window  -> 839 x 548 page   (minimum window: rail sidebar)
"""
import copy
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PREVIEWS = os.path.join(ROOT, "ui", "previews")
SHOTS = os.path.join(ROOT, "shots", "ui", "settings")
VOX_SHOT = os.path.join(ROOT, "target", "release", "vox-shot.exe")

PAGE = (892, 700)
MIN_1100 = (811, 548)
MIN_940 = (839, 548)
RAIL = (720, 640)
SECTION_W = 675      # 595-px column (the column in a 1180-wide window)
SECTION_MIN_W = 594  # 514-px column (the column in an 1100-wide window)


def img(p):
    return {"$image": p}


def enum(e):
    return {"$enum": e}


def choice(value, label, acc=""):
    return {"value": value, "label": label, "accessible-label": acc or label}


def badge(text, tone="neutral"):
    return {"text": text, "tone": enum(f"BadgeTone.{tone}")}


HINT = "Needs Text to Speech access. Add \u201cUser: read\u201d to see your credits here."
LEAD = "Vox speaks with your own ElevenLabs account. Paste an API key to start reading."

CREDITS_READY = {
    "state": enum("CreditsState.ready"),
    "title": "",
    "tier": "Creator",
    "percent": "39%",
    "percent-accessible": "39 percent of credits left",
    "fraction": 0.39,
    "low": False,
    "facts": "38.8K of 100K left",
    "reset": "Resets in 12 days",
    "detail": "38,766 of 100,000 characters left. Resets in 12 days",
    "message": "",
    "action-label": "",
    "refreshing": False,
}

SECTIONS = [
    choice("account", "Account"),
    choice("voice", "Voice & model"),
    choice("playback", "Playback"),
    choice("triggers", "Triggers"),
    choice("widget", "Widget"),
    choice("appearance", "Appearance"),
    choice("library", "Library & privacy"),
    choice("integrations", "Integrations"),
    choice("about", "About"),
]


def hotkey(action, label, desc, keys, spoken, **kw):
    row = {
        "action": action,
        "label": label,
        "description": desc,
        "keys": keys,
        "spoken": spoken,
        "recording": False,
        "preview": [],
        "hint": "",
        "warning": "",
    }
    row.update(kw)
    return row


BASE = {
    "loading": False,
    "saving": False,
    "status": "Saved automatically",
    "sections": SECTIONS,
    "revision": 1,
    "account": {
        "state": enum("AccountState.stored"),
        "badge": badge("Connected", "success"),
        "form-open": False,
        "replacing": False,
        "busy": False,
        "field-label": "New API key",
        "hint": HINT,
        "error": "",
        "submit-label": "Save new key",
        "lead": "",
        "limited-warning": "",
        "remove-busy": False,
        "credits": CREDITS_READY,
        "notice": "",
    },
    "voice": {
        "voice-name": "George",
        "voice-art": img("art/thumb-george.png"),
        "voice-tags": ["Male", "British", "Middle aged", "Warm", "Narration"],
        "tags-loading": False,
        "model-options": [
            choice("eleven_flash_v2_5", "Flash v2.5"),
            choice("eleven_turbo_v2_5", "Turbo v2.5"),
            choice("eleven_multilingual_v2", "Multilingual v2"),
            choice("eleven_v3", "v3"),
        ],
        "model-index": 0,
        "model-description": "Ultra low latency speech synthesis in 32 languages. Half the cost of the multilingual models.",
        "model-badges": [badge("0.5\u00d7 credits"), badge("40K chars / request"), badge("32 languages")],
        "model-error": "",
        "models-loading": False,
        "language-options": [choice("", "Auto-detect"), choice("en", "English"), choice("de", "German"),
                             choice("fr", "French"), choice("ja", "Japanese")],
        "language-index": 0,
        "quality-options": [choice("mp3_44100_128", "High \u00b7 128 kbps"),
                            choice("mp3_44100_64", "Standard \u00b7 64 kbps"),
                            choice("mp3_22050_32", "Data saver \u00b7 32 kbps")],
        "quality-index": 0,
        "stability": 0.5,
        "stability-text": "50%",
        "similarity": 0.75,
        "similarity-text": "75%",
        "style": 0.0,
        "style-text": "0%",
        "speed": 1.0,
        "speed-text": "1.00\u00d7",
        "speaker-boost": True,
        "reset-enabled": False,
        "test-busy": False,
    },
    "playback": {
        "rate-options": [choice("0.75", "0.75\u00d7", "0.75 times speed"), choice("1", "1\u00d7", "1 times speed"),
                         choice("1.25", "1.25\u00d7", "1.25 times speed"), choice("1.5", "1.5\u00d7", "1.5 times speed"),
                         choice("1.75", "1.75\u00d7", "1.75 times speed"), choice("2", "2\u00d7", "2 times speed")],
        "rate-index": 2,
        "volume": 0.8,
        "volume-text": "80%",
        "skip-urls": True,
        "skip-emails": False,
        "max-chars-options": [choice("2000", "2,000 characters"), choice("5000", "5,000 characters"),
                              choice("10000", "10,000 characters"), choice("20000", "20,000 characters"),
                              choice("50000", "50,000 characters"), choice("100000", "100,000 characters")],
        "max-chars-index": 3,
    },
    "triggers": {
        "hook-available": True,
        "hook-notice-title": "",
        "hook-notice-body": "",
        "right-click-chip": True,
        "selection-popup": False,
        "hotkeys-enabled": True,
        "hotkeys": [
            hotkey("readSelection", "Read selection", "Copies the selected text and reads it",
                   ["Ctrl", "Alt", "R"], "Ctrl plus Alt plus R"),
            hotkey("readClipboard", "Read clipboard", "Reads whatever text is on the clipboard",
                   ["Ctrl", "Alt", "V"], "Ctrl plus Alt plus V"),
            hotkey("playPause", "Play / pause", "Toggles the current reading",
                   ["Ctrl", "Alt", "P"], "Ctrl plus Alt plus P"),
            hotkey("stop", "Stop", "Stops and clears the current reading",
                   ["Ctrl", "Alt", "S"], "Ctrl plus Alt plus S"),
        ],
        "media-keys": True,
        "restore-clipboard": True,
        "excluded-apps": ["photoshop.exe", "mstsc.exe"],
        "exclude-error": "",
    },
    "widget": {
        "enabled": True,
        "position-options": [choice("bottom-right", "Bottom right"), choice("top-right", "Top right"),
                             choice("bottom-center", "Bottom center"),
                             choice("near-cursor", "Cursor", "Near the cursor"),
                             choice("remember", "Last spot", "Where I last dragged it")],
        "position-index": 0,
        "position-hint": "Above the clock, out of the way of most documents.",
        "compact": False,
        "highlight-words": True,
        "auto-hide": True,
    },
    "appearance": {
        "theme-options": [choice("system", "System"), choice("paper", "Paper"), choice("night", "Night")],
        "theme-index": 2,
        "intro": "The theme changes this window. The floating player and the chip always stay dark.",
        "theme-notes": ["Follows Windows dark mode", "Light window", "Dark window"],
    },
    "library": {
        "cache-badge": "187MB",
        "save-history": True,
        "store-text": True,
        "retention-options": [choice("0", "Keep forever"), choice("7", "7 days"), choice("30", "30 days"),
                              choice("90", "90 days"), choice("365", "1 year")],
        "retention-index": 0,
        "cache-audio": True,
        "cache-limit": 500,
        "cache-limit-max": 5000,
        "cache-limit-text": "500 MB",
        "usage-text": "187 MB of 500 MB \u00b7 214 reads cached",
        "usage-fraction": 0.374,
        "usage-high": False,
        "clear-cache-enabled": True,
        "clear-cache-busy": False,
        "export-busy": False,
        "clear-history-busy": False,
    },
    "integrations": {
        "bridge-enabled": True,
        "bridge-badge": badge("Listening on 47823", "success"),
        "port-error": "",
        "port-note": "",
        "explorer-enabled": True,
        "explorer-installed": True,
        "launch-at-login": True,
        "start-minimized": False,
        "close-to-tray": True,
        "copied": "",
    },
    "about": {
        "version": "1.0.0",
        "runtime": "Rust 1.98 \u00b7 Slint 1.18.1",
        "build": "a41c9e2",
        "data-folder": "C:\\Users\\Kwaku\\AppData\\Roaming\\Vox",
        "data-folder-ready": True,
        "demo": False,
    },
}


def variant(**patches):
    """BASE with `section__field=value` patches (double underscore = one level down)."""
    d = copy.deepcopy(BASE)
    for path, value in patches.items():
        keys = path.split("__")
        cur = d
        for k in keys[:-1]:
            cur = cur[k]
        cur[keys[-1]] = value
    return d


def hk(i, base=None, **kw):
    rows = copy.deepcopy((base or BASE)["triggers"]["hotkeys"])
    rows[i].update(kw)
    return rows


# name -> {"data": ..., other root properties..., "_preview": "page" | "section", "_size": (w, h) | None}
fixtures = {}


def page(name, data, scroll_to="", size=PAGE, **props):
    fx = {"data": data}
    if scroll_to:
        fx["scroll-to"] = scroll_to
    fx.update(props)
    fixtures[name] = (fx, "page", size)


def section(name, data, which, width=SECTION_W, **props):
    fx = {"data": data, "section": which, "page-width": width}
    fx.update(props)
    fixtures[name] = (fx, "section", None)


# ── Page: populated, loading, saving ──
page("populated", BASE)
loading = copy.deepcopy(BASE)
loading["loading"] = True
loading["status"] = ""
loading["sections"] = []
page("loading", loading)
page("saving", variant(saving=True, status="Saving"), "playback")

# ── Account: no key (first-time form), invalid key, migration notice, demo, replace form, limited key, remove busy,
#    credits loading / error / low ──
NO_KEY_CREDITS = dict(CREDITS_READY, state=enum("CreditsState.no-key"), tier="", percent="", fraction=0.0,
                      facts="", reset="", detail="", message="Add a key to see your monthly credits.")
nokey_account = dict(BASE["account"], state=enum("AccountState.no-key"), badge=badge("No key", "attention"),
                     **{"form-open": True, "field-label": "API key", "submit-label": "Save key", "lead": LEAD,
                        "credits": NO_KEY_CREDITS})
page("key-none", variant(account=nokey_account), "account")
page("key-invalid", variant(account=dict(nokey_account, error="ElevenLabs rejected that key.")), "account",
     **{"key-draft": "sk_7c1e0a9b3f2d4e6a8c0b1d3f5a7e9c2b"})
migration_account = dict(nokey_account, notice="Vox couldn\u2019t bring over the API key saved by the previous "
                                                 "version. Paste it again to keep reading.")
page("key-migration", variant(account=migration_account), "account")
# Demo mode (contracts.md §7.3): `demo_account` = Creator tier, 61,240 of 100,000 used, resets in 12 days (the ready
# credits above, title-cased tier as ui-settings-widget.md §2.2); the demo catalog's George (labels in spec order:
# accent, age, gender, use case; no descriptive tags) and Flash v2.5 (English only, so no language-count badge).
demo_account = dict(BASE["account"], state=enum("AccountState.demo"), badge=badge("Demo account"),
                    **{"field-label": "API key", "submit-label": "Save key", "lead": LEAD,
                       "credits": dict(CREDITS_READY, state=enum("CreditsState.demo"))})
page("key-demo", variant(account=demo_account, about__demo=True,
                         **{"voice__voice-tags": ["British", "Middle aged", "Male", "Narration"],
                            "voice__model-description": "Ultra low latency, 32 languages",
                            "voice__model-badges": [badge("0.5× credits"), badge("40K chars / request")],
                            "voice__language-options": [choice("", "Auto-detect"), choice("en", "English")]}),
     "account")
replace_account = dict(BASE["account"], **{"form-open": True, "replacing": True, "busy": True})
page("key-replace", variant(account=replace_account), "account",
     **{"key-draft": "sk_2b9d4f6a8c0e1a3c5e7a9b1d3f5a7c9e"})
limited_account = dict(BASE["account"], **{
    "limited-warning": "This key can\u2019t read your account usage, so credits won\u2019t be shown. Reading works "
                       "as long as it has Text to Speech access.",
    "credits": dict(NO_KEY_CREDITS, state=enum("CreditsState.restricted"),
                    message="This key can\u2019t read account info, so credits aren\u2019t shown. Reading still works.")})
page("key-limited", variant(account=limited_account), "account")
page("key-remove-busy", variant(**{"account__remove-busy": True}), "account")
credits_error_account = dict(BASE["account"], credits=dict(
    NO_KEY_CREDITS, state=enum("CreditsState.error"), tier="",
    message="Couldn\u2019t load your account: Can\u2019t reach ElevenLabs. Check your connection.",
    **{"action-label": "Try again"}))
page("credits-error", variant(account=credits_error_account), "account")
page("credits-loading", variant(account=dict(BASE["account"], credits=dict(
    NO_KEY_CREDITS, state=enum("CreditsState.loading"), message=""))), "account")
page("credits-low", variant(account=dict(BASE["account"], credits=dict(
    CREDITS_READY, percent="8%", fraction=0.08, low=True, facts="2.4K of 30K left", reset="Resets tomorrow",
    tier="Starter", **{"percent-accessible": "8 percent of credits left"}))), "account")

# ── Voice & model ──
VOICE_EDITED = {"voice__reset-enabled": True, "voice__stability": 0.35, "voice__stability-text": "35%",
                "voice__style": 0.2, "voice__style-text": "20%", "voice__speed": 1.08,
                "voice__speed-text": "1.08\u00d7", "voice__test-busy": True}
page("voice", variant(**VOICE_EDITED), "voice")
page("voice-loading", variant(**{"voice__tags-loading": True, "voice__voice-tags": [],
                                 "voice__models-loading": True, "voice__model-badges": [],
                                 "voice__model-description": "Which ElevenLabs model synthesizes speech.",
                                 "voice__voice-art": img("art/small-george.png")}), "voice")
page("voice-error", variant(**{
    "voice__model-error": "Couldn\u2019t load models: Can\u2019t reach ElevenLabs. Check your connection.",
    "voice__model-badges": [], "voice__voice-tags": ["ElevenLabs voice"],
    "voice__model-options": [choice("eleven_flash_v2_5", "Flash v2.5")],
    "voice__model-description": "Which ElevenLabs model synthesizes speech."}), "voice")

# ── Playback (a rate set elsewhere joins the presets) ──
page("playback", variant(**{
    "playback__rate-options": BASE["playback"]["rate-options"][:3] + [choice("1.4", "1.4\u00d7", "1.4 times speed")]
    + BASE["playback"]["rate-options"][3:], "playback__rate-index": 3}), "playback")

# ── Triggers: recorder recording (modifiers held + hint), empty recording, conflict / reserved warnings, hook
#    unavailable, long chords ──
page("triggers", BASE, "triggers")
page("hotkey-recording", variant(**{"triggers__hotkeys": hk(
    0, recording=True, preview=["Ctrl", "Shift"], hint="Add Ctrl, Alt or Win")}), "triggers")
page("hotkey-recording-empty", variant(**{"triggers__hotkeys": hk(1, recording=True)}), "triggers")
# Four held modifiers with short stored chords (the recorders at their 248-px minimum).
page("hotkey-recording-modifiers", variant(**{"triggers__hotkeys": hk(
    2, recording=True, preview=["Ctrl", "Alt", "Shift", "Win"])}), "triggers")
conflict = copy.deepcopy(BASE["triggers"]["hotkeys"])
conflict[2].update(keys=["Ctrl", "Alt", "R"], spoken="Ctrl plus Alt plus R",
                   warning="Same shortcut as another action \u2014 only one of them will fire.")
conflict[0].update(warning="Same shortcut as another action \u2014 only one of them will fire.")
conflict[3].update(keys=[], spoken="",
                   warning="Ctrl+C is used by Windows and other apps, so it can\u2019t be a global shortcut. Pick another.")
page("hotkey-conflict", variant(**{"triggers__hotkeys": conflict}), "triggers")
page("hook-unavailable", variant(**{
    "triggers__hook-available": False,
    "triggers__hook-notice-title": "Mouse and keyboard hook unavailable",
    "triggers__hook-notice-body": "The right-click chip and the selection popup need it, so they\u2019re paused. "
                                  "Shortcuts, the browser extension and the tray menu still work.",
    "triggers__hotkeys-enabled": False,
    "triggers__excluded-apps": [],
    "triggers__exclude-error": "Enter a program name like notepad.exe"}), "triggers",
    **{"exclude-draft": "con .exe"})
# Long chords: a full five-key shortcut, four held modifiers while recording, a long named key.
long_chords = copy.deepcopy(BASE["triggers"]["hotkeys"])
long_chords[0].update(keys=["Ctrl", "Alt", "Shift", "Win", "Prev track"],
                      spoken="Ctrl plus Alt plus Shift plus Win plus Prev track")
long_chords[1].update(recording=True, preview=["Ctrl", "Alt", "Shift", "Win"])
long_chords[2].update(keys=["Ctrl", "Shift", "Play/Pause"], spoken="Ctrl plus Shift plus Play/Pause")
long_chords[3].update(keys=["Ctrl", "Alt", "Shift", "Win", "Backspace"],
                      spoken="Ctrl plus Alt plus Shift plus Win plus Backspace")
page("hotkey-long-chords", variant(**{"triggers__hotkeys": long_chords}), "triggers")

# ── Widget, Appearance ──
page("widget", variant(**{"widget__position-index": 3,
                          "widget__position-hint": "Next to where you selected the text."}), "widget")
page("widget-top-right", variant(**{"widget__position-index": 1,
                                    "widget__position-hint": "Top corner, clear of the taskbar and notifications."}),
     "widget")
page("widget-off", variant(**{"widget__enabled": False, "widget__position-index": 4,
                              "widget__position-hint": "Wherever you last dropped it. Drag the widget to move it."}),
     "widget")
page("appearance", variant(**{"appearance__theme-index": 0}), "appearance")

# ── Library & privacy: populated (export busy), full + history off, measuring, measure failed, empty cache, a
#    hand-edited limit above 5000 (D-C-2: the slider max follows the stored value) ──
page("library", variant(**{"library__export-busy": True}), "library")
LIB_FULL = {
    "library__save-history": False, "library__retention-index": 2, "library__cache-limit": 250,
    "library__cache-limit-text": "250 MB", "library__usage-text": "241 MB of 250 MB \u00b7 1,204 reads cached",
    "library__usage-fraction": 0.964, "library__usage-high": True, "library__cache-badge": "241MB",
    "library__clear-cache-busy": True, "library__clear-history-busy": True}
page("library-full", variant(**LIB_FULL), "library")
page("library-measuring", variant(**{
    "library__cache-badge": "", "library__usage-text": "Measuring\u2026", "library__usage-fraction": 0.0,
    "library__clear-cache-enabled": False}), "library")
page("library-cache-error", variant(**{
    "library__cache-badge": "", "library__usage-text": "Couldn\u2019t read the cache size.",
    "library__usage-fraction": 0.0, "library__clear-cache-enabled": False}), "library")
page("library-empty", variant(**{
    "library__cache-badge": "0MB", "library__usage-text": "0 MB of 500 MB \u00b7 0 reads cached",
    "library__usage-fraction": 0.0, "library__clear-cache-enabled": False}), "library")
page("library-over-limit", variant(**{
    "library__cache-limit": 6000, "library__cache-limit-max": 6000, "library__cache-limit-text": "6 GB",
    "library__cache-badge": "1.2GB", "library__usage-text": "1.2 GB of 6 GB \u00b7 3,410 reads cached",
    "library__usage-fraction": 0.2}), "library")

# ── Integrations: copied chip, port error, port note (bridge not running), checking, bridge off ──
page("integrations", variant(**{"integrations__copied": "chrome://extensions"}), "integrations",
     **{"port-draft": "47823"})
page("integrations-port", variant(**{
    "integrations__bridge-badge": badge("Not running", "attention"),
    "integrations__port-error": "Use a port between 1024 and 65535.",
    "integrations__explorer-enabled": False, "integrations__explorer-installed": False}),
    "integrations", **{"port-draft": "00080"})
page("integrations-busy", variant(**{
    "integrations__bridge-badge": badge("Not running", "attention"),
    "integrations__port-note": "Port 47823 may be in use by another app. Try another one."}),
    "integrations", **{"port-draft": "47823"})
page("integrations-checking", variant(**{"integrations__bridge-badge": badge("Checking\u2026")}),
     "integrations", **{"port-draft": "51200"})
page("integrations-off", variant(**{
    "integrations__bridge-enabled": False, "integrations__bridge-badge": badge("Off")}),
    "integrations", **{"port-draft": "47823"})

# ── About: loaded, demo, before AppInfo loads ──
page("about", BASE, "about")
page("about-demo", variant(**{
    "about__demo": True, "about__version": "1.0.0 \u00b7 dev", "about__build": "dev",
    "about__data-folder": "C:\\Users\\Kwaku\\AppData\\Roaming\\Vox-Demo"}), "about")
page("about-loading", variant(**{
    "about__version": "\u2026", "about__runtime": "\u2026",
    "about__data-folder": "Settings, history and cached audio", "about__data-folder-ready": False}), "about")

# ── Long text: long voice name, many tags, long model copy, long app names, long paths, long errors ──
long = variant(**{
    "voice__voice-name": "Bartholomew \u2014 Deep Velvet Storyteller (Professional Voice Clone, Extended)",
    "voice__voice-art": img("art/thumb-callum.png"),
    "voice__voice-tags": ["Male", "Transatlantic", "Old", "Deep and resonant", "Audiobooks"],
    "voice__model-options": [choice("eleven_multilingual_sts_v2_experimental",
                                    "Multilingual STS v2 Experimental Preview Build")],
    "voice__model-description": "Our most lifelike, emotionally rich speech synthesis model, with support for 29 "
                                "languages, long-form narration, careful pacing and consistent character voices "
                                "across very long passages of text.",
    "voice__model-badges": [badge("2\u00d7 credits", "attention"), badge("10K chars / request"),
                            badge("29 languages")],
    "triggers__excluded-apps": ["photoshop.exe", "adobe_premiere_pro_creative_cloud_2026_beta_build.exe",
                                "mstsc.exe", "eldenring.exe"],
    "triggers__exclude-error": "adobe_premiere_pro_creative_cloud_2026_beta_build.exe is already excluded",
    "about__data-folder": "C:\\Users\\Konstantinos.Papadopoulos-Whitfield\\AppData\\Roaming\\Vox\\profiles\\"
                          "default-with-a-very-long-folder-name",
    "account__credits": dict(CREDITS_READY, tier="Scale (annual, enterprise seat)",
                             facts="1.24M of 2M left", reset="Resets in 27 days"),
})
page("long-text", long, "voice")
page("long-text-triggers", long, "triggers")
page("long-text-about", long, "about")

# ── The narrowest real pages: 811 x 548 (1100 x 640 window, full sidebar) and 839 x 548 (940 x 640, rail) ──
page("min-1100-account", BASE, "", MIN_1100)
page("min-1100-voice", long, "voice", MIN_1100)
page("min-1100-playback", BASE, "playback", MIN_1100)
page("min-1100-triggers", variant(**{"triggers__hotkeys": long_chords}), "triggers", MIN_1100)
page("min-1100-widget", variant(**{"widget__position-index": 2,
                                   "widget__position-hint": "Centered above the taskbar, like a media flyout."}),
     "widget", MIN_1100)
page("min-1100-library", variant(**LIB_FULL), "library", MIN_1100)
page("min-1100-integrations", variant(**{
    "integrations__bridge-badge": badge("Not running", "attention"),
    "integrations__port-note": "Port 47823 may be in use by another app. Try another one."}),
    "integrations", MIN_1100, **{"port-draft": "47823"})
page("min-1100-loading", loading, "", MIN_1100)
page("min-940-account", variant(account=nokey_account), "account", MIN_940)
page("min-940-triggers", variant(**{"triggers__hotkeys": conflict}), "triggers", MIN_940)
page("min-940-saving", variant(saving=True, status="Saving"), "appearance", MIN_940)
# Safety net only (no real window is this narrow): the section list collapses to an icon rail below 780 px.
page("rail-fallback", variant(saving=True, status="Saving"), "widget", RAIL)

# ── Whole sections (ui/previews/settings-sections.slint): every row of a section without scrolling ──
long_triggers = copy.deepcopy(long)
long_triggers["triggers"]["hotkeys"] = hk(1, recording=True, preview=["Ctrl", "Alt"],
                                          hint="On this keyboard that types \u201c@\u201d")
long_triggers["triggers"]["hotkeys"][3].update(
    warning="Ctrl+C is used by Windows and other apps, so it can\u2019t be a global shortcut. Pick another.")
section("section-account", BASE, "account")
section("section-voice", BASE, "voice")
section("section-playback", BASE, "playback")
section("section-triggers", long_triggers, "triggers", **{"exclude-draft": "C:\\Program Files\\Foo\\Bar.EXE"})
section("section-widget", BASE, "widget")
section("section-appearance", variant(**{"appearance__theme-index": 1}), "appearance")
section("section-library", BASE, "library")
section("section-library-busy", variant(**{"library__clear-cache-busy": True, "library__export-busy": True,
                                           "library__clear-history-busy": True}), "library")
section("section-integrations", BASE, "integrations", **{"port-draft": "47823"})
section("section-about", long, "about")
# The same sections in the narrowest column (514 px).
section("section-min-voice", long, "voice", SECTION_MIN_W)
section("section-min-triggers", variant(**{"triggers__hotkeys": long_chords}), "triggers", SECTION_MIN_W)
section("section-min-widget", BASE, "widget", SECTION_MIN_W)
section("section-min-library", variant(**LIB_FULL), "library", SECTION_MIN_W)
section("section-min-integrations", variant(**{
    "integrations__bridge-badge": badge("Not running", "attention"),
    "integrations__port-note": "Port 47823 may be in use by another app. Try another one."}),
    "integrations", SECTION_MIN_W, **{"port-draft": "47823"})


def write():
    os.makedirs(PREVIEWS, exist_ok=True)
    keep = set()
    for name, (fx, _, _) in fixtures.items():
        path = os.path.join(PREVIEWS, f"settings-{name}.json")
        keep.add(os.path.basename(path))
        with open(path, "w", encoding="utf-8") as f:
            json.dump(fx, f, ensure_ascii=False, indent=1)
    # Drop fixtures of states that no longer exist.
    for f in os.listdir(PREVIEWS):
        if f.startswith("settings-") and f.endswith(".json") and f not in keep:
            os.remove(os.path.join(PREVIEWS, f))
    print(len(fixtures), "fixtures")


def render_one(name, kind, size, theme):
    data = os.path.join(PREVIEWS, f"settings-{name}.json")
    out = os.path.join(SHOTS, f"{name}-{theme}.png")
    slint = os.path.join(PREVIEWS, "settings.slint" if kind == "page" else "settings-sections.slint")
    cmd = [VOX_SHOT, slint, "--data", data, "--out", out]
    if size:
        cmd += ["--size", f"{size[0]}x{size[1]}"]
    if theme == "light":
        cmd += ["--set", "Theme.dark=false"]
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    msg = (r.stdout + r.stderr).strip()
    errors = [line for line in msg.splitlines() if not line.startswith(("callback:", "saved "))]
    return name, theme, r.returncode, "\n".join(errors)


def render(filters):
    os.makedirs(SHOTS, exist_ok=True)
    jobs = [(n, k, s, t) for n, (_, k, s) in fixtures.items() for t in ("dark", "light")
            if not filters or any(f in n for f in filters)]
    if not filters:
        # Drop renders of states that no longer exist.
        names = {f"{n}-{t}.png" for n, _, _, t in jobs}
        for f in os.listdir(SHOTS):
            if f.endswith(".png") and f not in names:
                os.remove(os.path.join(SHOTS, f))
    failed = 0
    with ThreadPoolExecutor(max_workers=6) as pool:
        for name, theme, code, errors in pool.map(lambda j: render_one(*j), jobs):
            if code != 0 or errors:
                failed += code != 0
                print(f"{name}-{theme}: exit {code}\n{errors}")
    print(f"rendered {len(jobs)} images, {failed} failed")


if __name__ == "__main__":
    write()
    if len(sys.argv) > 1 and sys.argv[1] == "render":
        render(sys.argv[2:])
