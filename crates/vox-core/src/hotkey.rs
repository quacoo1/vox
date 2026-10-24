//! Global-hotkey logic that is pure (shell.md §8, ui-settings-widget.md §3, contracts.md §2.3): accelerator
//! parsing, the canonical comparison key, reserved chords, conflicts, the registration plan (reserved / duplicate /
//! AltGr checks before the OS call), failure-toast copy, keycap labels and the recorder's key → accelerator mapping.
//!
//! Decisions (BRIEF.md Product rulings "Hotkeys"; ARCHITECTURE.md §8 D-HK-1..4): one split/canonical function for
//! core and UI (parts are trimmed); the Electron accelerator grammar is kept, including the implicit Shift for
//! shifted US symbols (`Ctrl++` / `Ctrl+Plus` = Ctrl+Shift+`=`); an `AltGr` token means **Ctrl+Alt** (it expands to
//! both modifiers for comparison and registration; keycaps still show `AltGr`); `RegisterHotKey` is called **with**
//! `MOD_NOREPEAT` (vox-win), so holding a chord no longer auto-repeats.

use crate::settings::{HotkeySettings, TriggerSettings};

/// The four hotkey actions, in registration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    ReadSelection,
    ReadClipboard,
    PlayPause,
    Stop,
}

impl HotkeyAction {
    pub const ALL: [HotkeyAction; 4] = [Self::ReadSelection, Self::ReadClipboard, Self::PlayPause, Self::Stop];
    /// Settings key (`triggers.hotkeys.<id>`).
    pub fn id(self) -> &'static str {
        match self {
            Self::ReadSelection => "readSelection",
            Self::ReadClipboard => "readClipboard",
            Self::PlayPause => "playPause",
            Self::Stop => "stop",
        }
    }
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadSelection => "Read selection",
            Self::ReadClipboard => "Read clipboard",
            Self::PlayPause => "Play / pause",
            Self::Stop => "Stop",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::ReadSelection => "Copies the selected text and reads it",
            Self::ReadClipboard => "Reads whatever text is on the clipboard",
            Self::PlayPause => "Toggles the current reading",
            Self::Stop => "Stops and clears the current reading",
        }
    }
    /// Dotted settings path.
    pub fn settings_path(self) -> String {
        format!("triggers.hotkeys.{}", self.id())
    }
}

/// Chords a hotkey must never take (compared canonically).
pub const RESERVED_ACCELERATORS: [&str; 12] = [
    "Ctrl+C",
    "Ctrl+Insert",
    "Ctrl+V",
    "Ctrl+X",
    "Ctrl+Z",
    "Ctrl+Y",
    "Ctrl+A",
    "Ctrl+S",
    "Alt+F4",
    "Alt+Tab",
    "Ctrl+Escape",
    "Ctrl+Shift+Escape",
];

const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "super"];

/// Splits an accelerator on `+`, handling a literal `+` key (`"Ctrl++"` → `["Ctrl", "+"]`): a `+` at a part start
/// is the key itself and a following `+` is skipped. The whole string and each part are trimmed.
pub fn split_parts(accel: &str) -> Vec<String> {
    let s = accel.trim();
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '+' && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        } else if ch == '+' {
            parts.push("+".to_string());
            if chars.get(i + 1) == Some(&'+') {
                i += 1;
            }
        } else {
            cur.push(ch);
        }
        i += 1;
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts.into_iter().map(|p| p.trim().to_string()).collect()
}

fn alias(p: &str) -> String {
    let low = p.trim().to_lowercase();
    match low.as_str() {
        "control" | "cmdorctrl" | "commandorcontrol" => "ctrl".into(),
        "option" => "alt".into(),
        "cmd" | "command" | "meta" | "win" => "super".into(),
        "esc" => "escape".into(),
        "return" => "enter".into(),
        "plus" => "+".into(),
        _ => low,
    }
}

/// Lower-cased, alias-folded parts; `AltGr` expands to `ctrl` + `alt` (Windows reports AltGr as Ctrl+Alt).
pub fn canonical_parts(accel: &str) -> Vec<String> {
    let mut out = Vec::new();
    for p in split_parts(accel) {
        let a = alias(&p);
        if a == "altgr" {
            out.push("ctrl".to_string());
            out.push("alt".to_string());
        } else {
            out.push(a);
        }
    }
    out
}

/// The canonical comparison key: modifiers de-duplicated in `ctrl, alt, shift, super` order, then the other parts in
/// original order. `"Shift+Control+s"` → `"ctrl+shift+s"`.
pub fn canonical_key(accel: &str) -> String {
    let parts = canonical_parts(accel);
    let mut out: Vec<String> = MODIFIERS.iter().filter(|m| parts.iter().any(|p| p == *m)).map(|m| m.to_string()).collect();
    out.extend(parts.into_iter().filter(|p| !MODIFIERS.contains(&p.as_str())));
    out.join("+")
}

/// True for a non-blank accelerator equal (canonically) to a reserved chord.
pub fn is_reserved(accel: &str) -> bool {
    if accel.trim().is_empty() {
        return false;
    }
    let key = canonical_key(accel);
    RESERVED_ACCELERATORS.iter().any(|r| canonical_key(r) == key)
}

/// Actions whose non-blank shortcut is shared with another action.
pub fn conflicts(h: &HotkeySettings) -> Vec<HotkeyAction> {
    let keys: Vec<(HotkeyAction, String)> = HotkeyAction::ALL
        .iter()
        .filter(|a| !h.get(**a).trim().is_empty())
        .map(|a| (*a, canonical_key(h.get(*a))))
        .collect();
    keys.iter().filter(|(a, k)| keys.iter().any(|(b, k2)| a != b && k == k2)).map(|(a, _)| *a).collect()
}

/// Hotkey modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl Modifiers {
    /// Ctrl, Alt or Win (Shift alone is not enough for the recorder).
    pub fn strong(self) -> bool {
        self.ctrl || self.alt || self.win
    }
    /// Recorder order names: `Ctrl`, `Alt`, `Shift`, `Super`.
    pub fn names(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.ctrl {
            v.push("Ctrl");
        }
        if self.alt {
            v.push("Alt");
        }
        if self.shift {
            v.push("Shift");
        }
        if self.win {
            v.push("Super");
        }
        v
    }
}

/// An accelerator ready for `RegisterHotKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParsedAccelerator {
    pub modifiers: Modifiers,
    /// Windows virtual-key code.
    pub vk: u32,
}

/// Key name (lower-cased, aliases folded) → `(vk, implicit_shift)`, as Electron registers it (shell.md §8.2).
pub fn key_vk(key: &str) -> Option<(u32, bool)> {
    let k = key.to_lowercase();
    let b = k.as_bytes();
    if b.len() == 1 {
        let c = b[0];
        if c.is_ascii_lowercase() {
            return Some((c.to_ascii_uppercase() as u32, false));
        }
        if c.is_ascii_digit() {
            return Some((c as u32, false));
        }
        let unshifted = match c {
            b';' => Some(0xBA),
            b'=' => Some(0xBB),
            b',' => Some(0xBC),
            b'-' => Some(0xBD),
            b'.' => Some(0xBE),
            b'/' => Some(0xBF),
            b'`' => Some(0xC0),
            b'[' => Some(0xDB),
            b'\\' => Some(0xDC),
            b']' => Some(0xDD),
            b'\'' => Some(0xDE),
            _ => None,
        };
        if let Some(vk) = unshifted {
            return Some((vk, false));
        }
        let shifted = match c {
            b'+' => Some(0xBB),
            b':' => Some(0xBA),
            b'<' => Some(0xBC),
            b'_' => Some(0xBD),
            b'>' => Some(0xBE),
            b'?' => Some(0xBF),
            b'~' => Some(0xC0),
            b'{' => Some(0xDB),
            b'|' => Some(0xDC),
            b'}' => Some(0xDD),
            b'"' => Some(0xDE),
            b'!' => Some(b'1' as u32),
            b'@' => Some(b'2' as u32),
            b'#' => Some(b'3' as u32),
            b'$' => Some(b'4' as u32),
            b'%' => Some(b'5' as u32),
            b'^' => Some(b'6' as u32),
            b'&' => Some(b'7' as u32),
            b'*' => Some(b'8' as u32),
            b'(' => Some(b'9' as u32),
            b')' => Some(b'0' as u32),
            _ => None,
        };
        return shifted.map(|vk| (vk, true));
    }
    if let Some(n) = k.strip_prefix("num").and_then(|d| d.parse::<u32>().ok()).filter(|d| *d <= 9) {
        return Some((0x60 + n, false));
    }
    if let Some(n) = k.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()).filter(|d| (1..=24).contains(d)) {
        return Some((0x6F + n, false));
    }
    let vk = match k.as_str() {
        "plus" => return Some((0xBB, true)),
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "backspace" => 0x08,
        "delete" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "escape" | "esc" => 0x1B,
        "printscreen" => 0x2C,
        "numadd" => 0x6B,
        "numsub" => 0x6D,
        "nummult" => 0x6A,
        "numdiv" => 0x6F,
        "numdec" => 0x6E,
        "mediaplaypause" => 0xB3,
        "mediastop" => 0xB2,
        "medianexttrack" => 0xB0,
        "mediaprevioustrack" => 0xB1,
        "volumeup" => 0xAF,
        "volumedown" => 0xAE,
        "volumemute" => 0xAD,
        _ => return None,
    };
    Some((vk, false))
}

/// Parses an accelerator for registration: any number of modifiers and exactly one key. `None` = invalid
/// (reported as `in-use`, like Electron's throwing parser).
pub fn parse_accelerator(accel: &str) -> Option<ParsedAccelerator> {
    let parts = canonical_parts(accel);
    let mut m = Modifiers::default();
    let mut key: Option<(u32, bool)> = None;
    for p in &parts {
        match p.as_str() {
            "ctrl" => m.ctrl = true,
            "alt" => m.alt = true,
            "shift" => m.shift = true,
            "super" => m.win = true,
            "" => return None,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(key_vk(other)?);
            }
        }
    }
    let (vk, implicit_shift) = key?;
    if implicit_shift {
        m.shift = true;
    }
    Some(ParsedAccelerator { modifiers: m, vk })
}

/// The VK the AltGr guard checks for a character key: letters, digits and OEM punctuation (US positions).
pub fn character_key_vk(key: &str) -> Option<u32> {
    let k = key.to_lowercase();
    let b = k.as_bytes();
    if b.len() != 1 {
        return None;
    }
    let c = b[0];
    if c.is_ascii_lowercase() {
        return Some(c.to_ascii_uppercase() as u32);
    }
    if c.is_ascii_digit() {
        return Some(c as u32);
    }
    Some(match c {
        b';' => 0xBA,
        b'=' | b'+' => 0xBB,
        b',' => 0xBC,
        b'-' => 0xBD,
        b'.' => 0xBE,
        b'/' => 0xBF,
        b'`' => 0xC0,
        b'[' => 0xDB,
        b'\\' => 0xDC,
        b']' => 0xDD,
        b'\'' => 0xDE,
        _ => return None,
    })
}

/// When the AltGr guard applies (Ctrl and Alt, no Super, exactly one character key): `(vk, explicit_shift)`.
pub fn alt_gr_probe(accel: &str) -> Option<(u32, bool)> {
    let parts = canonical_parts(accel);
    let has = |m: &str| parts.iter().any(|p| p == m);
    if !(has("ctrl") && has("alt")) || has("super") {
        return None;
    }
    let keys: Vec<&String> = parts.iter().filter(|p| !MODIFIERS.contains(&p.as_str())).collect();
    if keys.len() != 1 {
        return None;
    }
    Some((character_key_vk(keys[0])?, has("shift")))
}

/// Why a hotkey was not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyFailureReason {
    /// Taken by another app, or not a valid accelerator.
    InUse,
    /// Two of our actions share it.
    Duplicate,
    Reserved,
    /// Ctrl+Alt(+Shift)+key types this character as AltGr on an installed layout (`""` = dead key).
    TypesCharacter(String),
}

impl HotkeyFailureReason {
    /// The reason id of the `hotkeys not registered: readClipboard=Ctrl+Alt+V (types-character), …` warn line
    /// (shell.md §8.5): `in-use` | `duplicate` | `reserved` | `types-character`.
    pub fn id(&self) -> &'static str {
        match self {
            Self::InUse => "in-use",
            Self::Duplicate => "duplicate",
            Self::Reserved => "reserved",
            Self::TypesCharacter(_) => "types-character",
        }
    }
}

/// Puts failures in registration order (`HotkeyAction::ALL` index; each action fails at most once), as shell.md
/// §8.5–§8.6 lists them. `vox_win::hotkeys::HotkeyService::register` merges the plan's failures with the OS refusals
/// and returns them sorted with this, so `describe_failures` names `Ctrl+Alt+R, Ctrl+Alt+P` in that order even when
/// the OS refused the first one. Stable.
pub fn sort_failures(failures: &mut [HotkeyFailure]) {
    let index = |a: HotkeyAction| HotkeyAction::ALL.iter().position(|x| *x == a).unwrap_or(usize::MAX);
    failures.sort_by_key(|f| index(f.action));
}

/// The warn line's list: `readClipboard=Ctrl+Alt+V (types-character), stop=Ctrl+Alt+S (in-use)`.
pub fn failures_log_line(failures: &[HotkeyFailure]) -> String {
    failures.iter().map(|f| format!("{}={} ({})", f.action.id(), f.accelerator, f.reason.id())).collect::<Vec<_>>().join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyFailure {
    pub action: HotkeyAction,
    /// Trimmed, as written in settings.
    pub accelerator: String,
    pub reason: HotkeyFailureReason,
}

/// What to register after the pure checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegistrationPlan {
    pub register: Vec<(HotkeyAction, String, ParsedAccelerator)>,
    pub failures: Vec<HotkeyFailure>,
}

/// The registration algorithm minus the OS call (shell.md §8.5): disabled → nothing; for each action in order:
/// trim; empty → skip; reserved → failure (not "seen"); seen → duplicate; mark seen; AltGr conflict (via `alt_gr`,
/// which returns the typed character, `""` for a dead key, or `None`) → failure (still seen); unparseable → in-use.
/// The OS layer appends `InUse` for every refused registration.
pub fn plan_registration(triggers: &TriggerSettings, alt_gr: impl Fn(u32, bool) -> Option<String>) -> RegistrationPlan {
    let mut plan = RegistrationPlan::default();
    if !triggers.hotkeys_enabled {
        return plan;
    }
    let mut seen: Vec<String> = Vec::new();
    for action in HotkeyAction::ALL {
        let accel = triggers.hotkeys.get(action).trim().to_string();
        if accel.is_empty() {
            continue;
        }
        let fail = |reason| HotkeyFailure { action, accelerator: accel.clone(), reason };
        let key = canonical_key(&accel);
        if is_reserved(&accel) {
            plan.failures.push(fail(HotkeyFailureReason::Reserved));
            continue;
        }
        if seen.contains(&key) {
            plan.failures.push(fail(HotkeyFailureReason::Duplicate));
            continue;
        }
        seen.push(key);
        if let Some((vk, shift)) = alt_gr_probe(&accel) {
            if let Some(ch) = alt_gr(vk, shift) {
                plan.failures.push(fail(HotkeyFailureReason::TypesCharacter(ch)));
                continue;
            }
        }
        match parse_accelerator(&accel) {
            Some(p) => plan.register.push((action, accel.clone(), p)),
            None => plan.failures.push(fail(HotkeyFailureReason::InUse)),
        }
    }
    plan
}

/// The failure toast line (shell.md §8.6), or `None` when nothing failed.
pub fn describe_failures(failures: &[HotkeyFailure]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let pick = |f: &dyn Fn(&HotkeyFailureReason) -> bool| -> Vec<&HotkeyFailure> { failures.iter().filter(|x| f(&x.reason)).collect() };
    let typing = pick(&|r| matches!(r, HotkeyFailureReason::TypesCharacter(_)));
    let reserved = pick(&|r| matches!(r, HotkeyFailureReason::Reserved));
    let in_use = pick(&|r| matches!(r, HotkeyFailureReason::InUse | HotkeyFailureReason::Duplicate));
    let list = |v: &[&HotkeyFailure]| v.iter().map(|f| f.accelerator.as_str()).collect::<Vec<_>>().join(", ");
    let mut parts: Vec<String> = Vec::new();
    match typing.len() {
        0 => {}
        1 => {
            let f = typing[0];
            let ch = match &f.reason {
                HotkeyFailureReason::TypesCharacter(c) => c.as_str(),
                _ => "",
            };
            if ch.is_empty() {
                parts.push(format!("{} types a character on your keyboard, so it isn’t used as a shortcut", f.accelerator));
            } else {
                parts.push(format!("{} types \u{201C}{ch}\u{201D} on your keyboard, so it isn’t used as a shortcut", f.accelerator));
            }
        }
        _ => parts.push(format!("{} type characters on your keyboard, so they aren’t used as shortcuts", list(&typing))),
    }
    if !reserved.is_empty() {
        let verb = if reserved.len() == 1 { "is" } else { "are" };
        parts.push(format!("{} {verb} reserved for copying, pasting or Windows", list(&reserved)));
    }
    if !in_use.is_empty() {
        let verb = if in_use.len() == 1 { "is" } else { "are" };
        parts.push(format!("{} {verb} already in use", list(&in_use)));
    }
    let tail = if failures.len() > 1 { "Pick other shortcuts in Settings" } else { "Pick another shortcut in Settings" };
    Some(format!("{}. {tail}", parts.join(". ")))
}

// ───────────────────────────── display ─────────────────────────────

/// Keycap labels for an accelerator (ui-settings-widget.md §3.2); empty for `""`.
pub fn display_labels(accel: &str) -> Vec<String> {
    if accel.trim().is_empty() {
        return Vec::new();
    }
    split_parts(accel)
        .iter()
        .map(|p| {
            let low = p.to_lowercase();
            let fixed = match low.as_str() {
                "commandorcontrol" | "cmdorctrl" | "control" | "ctrl" => "Ctrl",
                "command" | "cmd" => "Cmd",
                "alt" | "option" => "Alt",
                "altgr" => "AltGr",
                "shift" => "Shift",
                "super" | "meta" | "win" => "Win",
                "plus" => "+",
                "space" => "Space",
                "return" | "enter" => "Enter",
                "escape" | "esc" => "Esc",
                "up" => "\u{2191}",
                "down" => "\u{2193}",
                "left" => "\u{2190}",
                "right" => "\u{2192}",
                "pageup" => "PgUp",
                "pagedown" => "PgDn",
                "delete" => "Del",
                "insert" => "Ins",
                "backspace" => "Backspace",
                "printscreen" => "PrtSc",
                "numadd" => "Num +",
                "numsub" => "Num \u{2212}",
                "nummult" => "Num \u{00D7}",
                "numdiv" => "Num \u{00F7}",
                "numdec" => "Num .",
                "mediaplaypause" => "Play/Pause",
                "medianexttrack" => "Next track",
                "mediaprevioustrack" => "Prev track",
                "mediastop" => "Media stop",
                "volumeup" => "Vol +",
                "volumedown" => "Vol \u{2212}",
                "volumemute" => "Mute",
                _ => "",
            };
            if !fixed.is_empty() {
                return fixed.to_string();
            }
            if let Some(d) = low.strip_prefix("num").filter(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit()) {
                return format!("Num {d}");
            }
            if p.chars().count() == 1 {
                p.to_uppercase()
            } else {
                p.clone()
            }
        })
        .collect()
}

/// Labels joined with `+` (`Ctrl+Insert` → `Ctrl+Ins`), used in the reserved-chord warnings.
pub fn chord_label(accel: &str) -> String {
    display_labels(accel).join("+")
}

/// Screen-reader form: `Ctrl plus Alt plus R`; a `+` key reads as `Plus`; empty → `Not set`.
pub fn spoken(accel: &str) -> String {
    let labels = display_labels(accel);
    if labels.is_empty() {
        return "Not set".into();
    }
    labels.into_iter().map(|l| if l == "+" { "Plus".to_string() } else { l }).collect::<Vec<_>>().join(" plus ")
}

/// Tray menus show an accelerator hint only for `^[A-Za-z0-9]+(\+[A-Za-z0-9]+)*$`.
pub fn tray_hint_ok(accel: &str) -> bool {
    !accel.is_empty() && accel.split('+').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Settings-row warning for a stored reserved chord, or a shared shortcut (persistent warnings, §2.5).
pub fn row_warning(h: &HotkeySettings, action: HotkeyAction) -> Option<String> {
    let accel = h.get(action);
    if is_reserved(accel) {
        return Some(format!("{} is used by Windows and other apps (copying, pasting\u{2026}). Pick another shortcut.", chord_label(accel)));
    }
    if conflicts(h).contains(&action) {
        return Some("Same shortcut as another action \u{2014} only one of them will fire.".into());
    }
    None
}

/// The transient message when the recorder produced a reserved chord (not saved).
pub fn reserved_rejection(accel: &str) -> String {
    format!("{} is used by Windows and other apps, so it can\u{2019}t be a global shortcut. Pick another.", chord_label(accel))
}

// ───────────────────────────── recorder ─────────────────────────────

/// Outcome of one key press while recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Only modifiers held: live preview of these names (`Ctrl`, `Alt`, `Shift`, `Super`).
    Partial(Vec<String>),
    /// A key without Ctrl/Alt/Win that is not standalone: hint `Add Ctrl, Alt or Win`.
    NeedsModifier(String),
    /// Hint `That key can’t be used`.
    Unsupported,
    /// Hint `On this keyboard that types “{ch}”`.
    TypesCharacter { accelerator: String, ch: String },
    /// Save this accelerator.
    Ok(String),
}

/// True for modifier virtual keys (Shift/Ctrl/Alt/Win, either side).
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// Accelerator key name for a Windows virtual key (the layout-following VK is what `RegisterHotKey` uses).
pub fn key_name_for_vk(vk: u32) -> Option<String> {
    let s: &str = match vk {
        0x41..=0x5A | 0x30..=0x39 => return Some(char::from_u32(vk)?.to_string()),
        0x60..=0x69 => return Some(format!("num{}", vk - 0x60)),
        0x70..=0x87 => return Some(format!("F{}", vk - 0x6F)),
        0x20 => "Space",
        0x0D => "Enter",
        0x09 => "Tab",
        0x08 => "Backspace",
        0x2E => "Delete",
        0x2D => "Insert",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x26 => "Up",
        0x28 => "Down",
        0x25 => "Left",
        0x27 => "Right",
        0x1B => "Escape",
        0x2C => "PrintScreen",
        0xBD => "-",
        0xBB => "=",
        0xDB => "[",
        0xDD => "]",
        0xDC | 0xE2 => "\\",
        0xBA => ";",
        0xDE => "'",
        0xC0 => "`",
        0xBC => ",",
        0xBE => ".",
        0xBF => "/",
        0x6B => "numadd",
        0x6D => "numsub",
        0x6A => "nummult",
        0x6F => "numdiv",
        0x6E => "numdec",
        0xB3 => "MediaPlayPause",
        0xB2 => "MediaStop",
        0xB0 => "MediaNextTrack",
        0xB1 => "MediaPreviousTrack",
        0xAF => "VolumeUp",
        0xAE => "VolumeDown",
        0xAD => "VolumeMute",
        _ => return None,
    };
    Some(s.to_string())
}

/// Keys allowed without Ctrl/Alt/Win: `^(F([1-9]|1[0-9]|2[0-4])|Media.*|Volume.*|PrintScreen)$`.
pub fn is_standalone_key(name: &str) -> bool {
    if let Some(n) = name.strip_prefix('F').and_then(|d| d.parse::<u32>().ok()) {
        let digits = &name[1..];
        return (1..=24).contains(&n) && !digits.starts_with('0');
    }
    name.starts_with("Media") || name.starts_with("Volume") || name == "PrintScreen"
}

/// `acceleratorFromEvent` for a Win32 key press: `vk` from `WM_KEYDOWN/WM_SYSKEYDOWN`, held modifiers, and
/// `alt_gr_char` = what Ctrl+Alt(+Shift)+key types on the active layout (`None` when nothing / not checked).
pub fn record_key(vk: u32, mods: Modifiers, alt_gr_char: Option<String>) -> RecordOutcome {
    if is_modifier_vk(vk) {
        return RecordOutcome::Partial(mods.names().into_iter().map(String::from).collect());
    }
    let Some(key) = key_name_for_vk(vk) else { return RecordOutcome::Unsupported };
    if !mods.strong() && !is_standalone_key(&key) {
        return RecordOutcome::NeedsModifier(key);
    }
    let mut parts: Vec<String> = mods.names().into_iter().map(String::from).collect();
    parts.push(key);
    let accelerator = parts.join("+");
    if mods.ctrl && mods.alt && !mods.win {
        if let Some(ch) = alt_gr_char.filter(|c| !c.is_empty()) {
            return RecordOutcome::TypesCharacter { accelerator, ch };
        }
    }
    RecordOutcome::Ok(accelerator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_listed_in_registration_order() {
        // The OS refused readSelection after the plan already failed playPause as a duplicate (shell.md §8.5–§8.6).
        let mut all = vec![
            HotkeyFailure { action: HotkeyAction::PlayPause, accelerator: "Ctrl+Alt+P".into(), reason: HotkeyFailureReason::Duplicate },
            HotkeyFailure { action: HotkeyAction::ReadSelection, accelerator: "Ctrl+Alt+R".into(), reason: HotkeyFailureReason::InUse },
        ];
        sort_failures(&mut all);
        assert_eq!(describe_failures(&all).unwrap(), "Ctrl+Alt+R, Ctrl+Alt+P are already in use. Pick other shortcuts in Settings");
        assert_eq!(failures_log_line(&all), "readSelection=Ctrl+Alt+R (in-use), playPause=Ctrl+Alt+P (duplicate)");
        assert_eq!(HotkeyFailureReason::TypesCharacter("@".into()).id(), "types-character");
        assert_eq!(HotkeyFailureReason::Reserved.id(), "reserved");
    }

    #[test]
    fn splitting_and_canonical() {
        // ui-settings-widget.md §3.1 vectors.
        assert_eq!(split_parts("Ctrl+++"), vec!["Ctrl", "+"]);
        assert_eq!(split_parts("+"), vec!["+"]);
        assert_eq!(split_parts("Ctrl+"), vec!["Ctrl"]);
        assert_eq!(split_parts("Ctrl+Plus"), vec!["Ctrl", "Plus"]);
        assert_eq!(split_parts(""), Vec::<String>::new());
        assert_eq!(split_parts("Ctrl++"), vec!["Ctrl", "+"]);
        assert_eq!(split_parts(" Ctrl + R "), vec!["Ctrl", "R"]);
        assert_eq!(canonical_key("Shift+Control+s"), "ctrl+shift+s");
        assert_eq!(canonical_key("CmdOrCtrl+Plus"), "ctrl++");
        assert_eq!(canonical_key("Ctrl+Ctrl+A"), "ctrl+a");
        assert!(is_reserved("control+insert"));
        assert!(is_reserved("Shift+Ctrl+Esc"));
        assert!(!is_reserved(""));
        assert!(!is_reserved("Ctrl+Alt+R"));
    }

    #[test]
    fn parse_for_registration() {
        let p = parse_accelerator("Ctrl+Alt+R").unwrap();
        assert_eq!(p.vk, 0x52);
        assert!(p.modifiers.ctrl && p.modifiers.alt && !p.modifiers.shift);
        let p = parse_accelerator("Ctrl++").unwrap();
        assert_eq!((p.vk, p.modifiers.shift), (0xBB, true));
        assert_eq!(parse_accelerator("Ctrl+F13").unwrap().vk, 0x7C);
        assert_eq!(parse_accelerator("Super+num7").unwrap().vk, 0x67);
        assert_eq!(parse_accelerator("MediaPlayPause").unwrap().vk, 0xB3);
        assert!(parse_accelerator("Ctrl+A+B").is_none());
        assert!(parse_accelerator("Ctrl").is_none());
        assert!(parse_accelerator("Ctrl+Nope").is_none());
        assert_eq!(alt_gr_probe("Ctrl+Alt+V"), Some((0x56, false)));
        assert_eq!(alt_gr_probe("Ctrl+Alt+Shift+2"), Some((0x32, true)));
        assert_eq!(alt_gr_probe("Ctrl+Alt+F1"), None);
        assert_eq!(alt_gr_probe("Ctrl+Alt+Super+V"), None);
    }

    #[test]
    fn registration_plan_and_copy() {
        let mut t = TriggerSettings::default();
        t.hotkeys.read_clipboard = "Ctrl+Alt+V".into();
        t.hotkeys.play_pause = "ctrl+c".into();
        t.hotkeys.stop = " Ctrl+Alt+R ".into();
        let plan = plan_registration(&t, |vk, _| (vk == 0x56).then(|| "@".to_string()));
        assert_eq!(plan.register.len(), 1);
        assert_eq!(plan.register[0].0, HotkeyAction::ReadSelection);
        let reasons: Vec<_> = plan.failures.iter().map(|f| (f.action, f.reason.clone())).collect();
        assert_eq!(
            reasons,
            vec![
                (HotkeyAction::ReadClipboard, HotkeyFailureReason::TypesCharacter("@".into())),
                (HotkeyAction::PlayPause, HotkeyFailureReason::Reserved),
                (HotkeyAction::Stop, HotkeyFailureReason::Duplicate),
            ]
        );
        assert_eq!(
            describe_failures(&plan.failures).unwrap(),
            "Ctrl+Alt+V types \u{201C}@\u{201D} on your keyboard, so it isn’t used as a shortcut. ctrl+c is reserved for copying, pasting or Windows. Ctrl+Alt+R is already in use. Pick other shortcuts in Settings"
        );
        let one = [HotkeyFailure { action: HotkeyAction::Stop, accelerator: "Ctrl+Alt+S".into(), reason: HotkeyFailureReason::InUse }];
        assert_eq!(describe_failures(&one).unwrap(), "Ctrl+Alt+S is already in use. Pick another shortcut in Settings");
        t.hotkeys_enabled = false;
        assert_eq!(plan_registration(&t, |_, _| None), RegistrationPlan::default());
    }

    #[test]
    fn altgr_is_ctrl_alt() {
        assert_eq!(canonical_key("AltGr+R"), "ctrl+alt+r");
        assert_eq!(canonical_key("AltGr+R"), canonical_key("Ctrl+Alt+R"));
        let p = parse_accelerator("AltGr+Q").unwrap();
        assert!(p.modifiers.ctrl && p.modifiers.alt && !p.modifiers.shift);
        assert_eq!(p.vk, 0x51);
        assert_eq!(alt_gr_probe("AltGr+Q"), Some((0x51, false)));
        assert_eq!(display_labels("AltGr+Q"), vec!["AltGr", "Q"]);
    }

    #[test]
    fn labels() {
        assert_eq!(display_labels("Ctrl+Alt+R"), vec!["Ctrl", "Alt", "R"]);
        assert_eq!(chord_label("Ctrl+Insert"), "Ctrl+Ins");
        assert_eq!(display_labels("Super+numadd"), vec!["Win", "Num +"]);
        assert_eq!(spoken("Ctrl++"), "Ctrl plus Plus");
        assert_eq!(spoken(""), "Not set");
        assert!(tray_hint_ok("Ctrl+Alt+V"));
        assert!(!tray_hint_ok("Ctrl+Alt+;"));
        let h = HotkeySettings { stop: "Ctrl+Alt+R".into(), ..HotkeySettings::default() };
        assert_eq!(conflicts(&h), vec![HotkeyAction::ReadSelection, HotkeyAction::Stop]);
        assert!(row_warning(&h, HotkeyAction::Stop).unwrap().starts_with("Same shortcut"));
    }

    #[test]
    fn recorder() {
        let ca = Modifiers { ctrl: true, alt: true, ..Default::default() };
        assert_eq!(record_key(0x52, ca, None), RecordOutcome::Ok("Ctrl+Alt+R".into()));
        assert_eq!(
            record_key(0x56, ca, Some("@".into())),
            RecordOutcome::TypesCharacter { accelerator: "Ctrl+Alt+V".into(), ch: "@".into() }
        );
        assert_eq!(record_key(0x52, Modifiers::default(), None), RecordOutcome::NeedsModifier("R".into()));
        assert_eq!(record_key(0x52, Modifiers { shift: true, ..Default::default() }, None), RecordOutcome::NeedsModifier("R".into()));
        assert_eq!(record_key(0x7B, Modifiers::default(), None), RecordOutcome::Ok("F12".into()));
        assert_eq!(record_key(0xB3, Modifiers::default(), None), RecordOutcome::Ok("MediaPlayPause".into()));
        assert_eq!(record_key(0xA2, ca, None), RecordOutcome::Partial(vec!["Ctrl".into(), "Alt".into()]));
        assert_eq!(record_key(0xFF, ca, None), RecordOutcome::Unsupported);
        // Numpad keys arrive as VK_NUMPAD0..9 / VK_DECIMAL (the recorder maps winit's Numpad* key codes directly,
        // ARCHITECTURE.md §8 D-HK-5), never as the NumLock-off navigation VKs.
        assert_eq!(record_key(0x67, ca, None), RecordOutcome::Ok("Ctrl+Alt+num7".into()));
        assert_eq!(record_key(0x6E, ca, None), RecordOutcome::Ok("Ctrl+Alt+numdec".into()));
        assert_eq!(record_key(0x6B, ca, None), RecordOutcome::Ok("Ctrl+Alt+numadd".into()));
        assert!(is_standalone_key("F24") && !is_standalone_key("F25") && !is_standalone_key("F0"));
    }
}
