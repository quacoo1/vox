//! The selection gesture state machine (selection.md §5). Pure: no Windows calls; time advances only through event
//! timestamps and [`GestureMachine::tick`]; queries go through [`GestureDeps`]. Coordinates are physical pixels.
//! Key codes are Win32 virtual keys plus the extended flag (selection.md §4.3 gives the VK equivalents of the
//! original libuiohook sets).

#![warn(dead_code, unused)]

use vox_core::app::ChipMode;
use vox_core::constants::{
    CONTEXT_CHIP_MS, DRAG_THRESHOLD_PX, HINT_TTL_MS, MULTI_CLICK_MS, MULTI_CLICK_PX, SELECTION_CHIP_MS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureConfig {
    /// `triggers.rightClickChip`
    pub context_chip: bool,
    /// `triggers.selectionPopup`
    pub selection_chip: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    Drag,
    MultiClick,
    ShiftClick,
    Keyboard,
    SelectAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintClearReason {
    Click,
    Key,
    Expired,
    Blocked,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipHideReason {
    MouseDown,
    Key,
    Wheel,
    Timeout,
    Disabled,
    Dismissed,
    Activated,
    Reset,
}

/// What the service must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    HintSet(HintKind),
    HintCleared(HintClearReason),
    ShowContextChip { x: i32, y: i32 },
    ShowSelectionChip { x: i32, y: i32 },
    HideChip(ChipHideReason),
    ActivateChip(ChipMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureEventKind {
    MouseDown(MouseButton),
    MouseUp(MouseButton),
    MouseMove,
    Wheel,
    /// Key press (modifier keydowns included; keyups are not fed to the machine).
    KeyDown {
        vk: u32,
        extended: bool,
    },
}

/// One normalised input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureEvent {
    pub kind: GestureEventKind,
    pub x: i32,
    pub y: i32,
    /// libuiohook-style multi-click counter (selection.md §4.1); 0 for keys.
    pub clicks: u32,
    /// Monotonic milliseconds.
    pub time_ms: u64,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// A window identity for hint matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureTarget {
    /// Root HWND as a decimal string.
    pub id: String,
    /// Vox itself, an excluded app or a password manager.
    pub blocked: bool,
}

/// Queries the machine makes (selection.md §5.4). Unknown answers get the benefit of the doubt.
pub trait GestureDeps {
    fn is_inside_chip(&self, x: i32, y: i32) -> bool;
    fn target_at(&self, x: i32, y: i32) -> Option<GestureTarget>;
    fn focused_target(&self) -> Option<GestureTarget>;
    fn own_overlay_at(&self, x: i32, y: i32) -> bool;
    /// `Some(true)` I-beam/caret, `Some(false)` definitely not text, `None` can't judge.
    fn text_cursor(&self) -> Option<bool>;
    fn chips_suppressed(&self) -> bool;
}

/// `VK_A`: Ctrl+A selects all (layout-dependent VK, like the apps' own shortcut).
const VK_A: u32 = 0x41;
/// `VK_C`: Ctrl+C keeps the selection.
const VK_C: u32 = 0x43;
/// `VK_INSERT`: Ctrl+Insert keeps the selection, but only the extended (dedicated) Insert key — libuiohook maps the
/// numpad Insert (NumLock off, no extended flag) to a different code (selection.md §4.3, §13 #3).
const VK_INSERT: u32 = 0x2D;

/// Something was probably selected (selection.md §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hint {
    kind: HintKind,
    at: u64,
    /// The window it was selected in (`None` = unknown).
    target: Option<String>,
}

/// The left button is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeftPress {
    x: i32,
    y: i32,
    shift: bool,
    multi: bool,
    dragged: bool,
    /// The press landed on the chip; its release is neither a gesture nor a plain click.
    on_chip: bool,
    /// The press followed a right-click: it most likely picked or dismissed a native context-menu item.
    after_menu: bool,
    /// The press landed on Vox's own non-activating window (widget, chip): not a gesture in the user's app.
    overlay: bool,
    /// Text cursor at the press (`None` = unknown).
    text: Option<bool>,
}

impl LeftPress {
    fn special(x: i32, y: i32, on_chip: bool, overlay: bool) -> Self {
        Self { x, y, shift: false, multi: false, dragged: false, on_chip, after_menu: false, overlay, text: None }
    }
}

/// The previous left press, for multi-click detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LastDown {
    x: i32,
    y: i32,
    at: u64,
}

/// The machine's view of the visible chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chip {
    mode: ChipMode,
    at: u64,
    ttl: u64,
}

/// Euclidean distance in physical pixels (no overflow for any `i32` input).
fn distance(ax: i32, ay: i32, bx: i32, by: i32) -> f64 {
    let dx = (i64::from(ax) - i64::from(bx)) as f64;
    let dy = (i64::from(ay) - i64::from(by)) as f64;
    dx.hypot(dy)
}

/// The machine.
#[derive(Debug, Clone)]
pub struct GestureMachine {
    cfg: GestureConfig,
    hint: Option<Hint>,
    left: Option<LeftPress>,
    last_left_down: Option<LastDown>,
    chip: Option<Chip>,
    /// A right-click happened: the next click or key most likely goes to the native context menu.
    menu_open: bool,
}

impl GestureMachine {
    pub fn new(cfg: GestureConfig) -> Self {
        Self { cfg, hint: None, left: None, last_left_down: None, chip: None, menu_open: false }
    }

    /// `configure`: hides a visible chip whose mode is now disabled.
    pub fn configure(&mut self, cfg: GestureConfig) -> Vec<Decision> {
        self.cfg = cfg;
        let mut out = Vec::new();
        if let Some(chip) = self.chip {
            if !self.mode_enabled(chip.mode) {
                self.chip = None;
                out.push(Decision::HideChip(ChipHideReason::Disabled));
            }
        }
        out
    }

    /// `handle(ev)`: `tick(ev.time)` first, then dispatch (§5.5.2–§5.5.8).
    pub fn handle(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps) -> Vec<Decision> {
        let mut out = self.tick(ev.time_ms);
        match ev.kind {
            GestureEventKind::MouseDown(MouseButton::Left) => self.on_left_down(ev, deps, &mut out),
            GestureEventKind::MouseDown(MouseButton::Right) => self.on_right_down(ev, deps, &mut out),
            GestureEventKind::MouseDown(_) => self.on_other_down(ev, deps, &mut out),
            GestureEventKind::MouseUp(MouseButton::Left) => self.on_left_up(ev, deps, &mut out),
            // The right release opens most context menus: it must not hide the chip. Other releases mean nothing.
            GestureEventKind::MouseUp(_) => {}
            GestureEventKind::MouseMove => self.on_move(ev),
            GestureEventKind::Wheel => {
                if self.chip.take().is_some() {
                    out.push(Decision::HideChip(ChipHideReason::Wheel));
                }
            }
            GestureEventKind::KeyDown { vk, extended } => self.on_key_down(ev, vk, extended, deps, &mut out),
        }
        out
    }

    /// Expiry: hint after 60 s, chip after its ttl (4 s selection / 8 s context).
    pub fn tick(&mut self, now_ms: u64) -> Vec<Decision> {
        let mut out = Vec::new();
        if let Some(hint) = &self.hint {
            if now_ms.saturating_sub(hint.at) >= HINT_TTL_MS {
                self.hint = None;
                out.push(Decision::HintCleared(HintClearReason::Expired));
            }
        }
        if let Some(chip) = self.chip {
            if now_ms.saturating_sub(chip.at) >= chip.ttl {
                self.chip = None;
                out.push(Decision::HideChip(ChipHideReason::Timeout));
            }
        }
        out
    }

    /// The chip was hidden from outside (the chip UI dismissed it, or an activation).
    pub fn hide_chip(&mut self, reason: ChipHideReason) -> Vec<Decision> {
        match self.chip.take() {
            Some(_) => vec![Decision::HideChip(reason)],
            None => Vec::new(),
        }
    }

    /// Forgets everything (the hook stopped): hide-chip first, then hint-cleared.
    pub fn reset(&mut self) -> Vec<Decision> {
        let mut out = Vec::new();
        if self.chip.is_some() {
            out.push(Decision::HideChip(ChipHideReason::Reset));
        }
        if self.hint.is_some() {
            out.push(Decision::HintCleared(HintClearReason::Reset));
        }
        self.chip = None;
        self.hint = None;
        self.left = None;
        self.last_left_down = None;
        self.menu_open = false;
        out
    }

    /// The hook only forwards moves while this is true (hot-path filter).
    pub fn wants_mouse_move(&self) -> bool {
        self.left.is_some_and(|p| !p.dragged && !p.on_chip && !p.overlay)
    }

    /// The next expiry deadline (ms), for the service's single timer.
    pub fn next_deadline(&self) -> Option<u64> {
        let hint = self.hint.as_ref().map(|h| h.at.saturating_add(HINT_TTL_MS));
        let chip = self.chip.map(|c| c.at.saturating_add(c.ttl));
        match (hint, chip) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Mode of the visible chip.
    pub fn chip_mode(&self) -> Option<ChipMode> {
        self.chip.map(|c| c.mode)
    }

    // ───────────── internals ─────────────

    fn mode_enabled(&self, mode: ChipMode) -> bool {
        match mode {
            ChipMode::Context => self.cfg.context_chip,
            ChipMode::Selection => self.cfg.selection_chip,
        }
    }

    fn inside_chip(&self, ev: &GestureEvent, deps: &dyn GestureDeps) -> bool {
        self.chip.is_some() && deps.is_inside_chip(ev.x, ev.y)
    }

    fn live_hint(&self, now: u64) -> bool {
        self.hint.as_ref().is_some_and(|h| now.saturating_sub(h.at) < HINT_TTL_MS)
    }

    /// A plain drag counts as a text selection only with a text cursor at its press or its release (unknown = yes).
    /// The release is asked only when the press answer was a definite "no" (short-circuit, selection.md §5.4).
    fn dragged_over_text(p: &LeftPress, deps: &dyn GestureDeps) -> bool {
        p.text != Some(false) || deps.text_cursor() != Some(false)
    }

    fn clear_hint(&mut self, reason: HintClearReason, out: &mut Vec<Decision>) {
        if self.hint.take().is_some() {
            out.push(Decision::HintCleared(reason));
        }
    }

    fn on_left_down(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps, out: &mut Vec<Decision>) {
        if let Some(chip) = self.chip {
            if deps.is_inside_chip(ev.x, ev.y) {
                // A press on the visible chip: activate it (the hook sees presses some context menus swallow).
                self.chip = None;
                self.left = Some(LeftPress::special(ev.x, ev.y, true, false));
                self.last_left_down = None;
                self.menu_open = false;
                out.push(Decision::ActivateChip(chip.mode));
                return;
            }
            self.chip = None;
            out.push(Decision::HideChip(ChipHideReason::MouseDown));
        }
        if deps.own_overlay_at(ev.x, ev.y) {
            // Pause / skip on the widget, or dragging it aside: the selection in the user's app is untouched.
            self.left = Some(LeftPress::special(ev.x, ev.y, false, true));
            self.last_left_down = None;
            self.menu_open = false;
            return;
        }
        // The hook's click counter ignores position, so a second press must also land near the first one.
        let multi = self.last_left_down.is_some_and(|prev| {
            distance(prev.x, prev.y, ev.x, ev.y) <= MULTI_CLICK_PX
                && (ev.clicks >= 2 || ev.time_ms.saturating_sub(prev.at) <= MULTI_CLICK_MS)
        });
        self.left = Some(LeftPress {
            x: ev.x,
            y: ev.y,
            shift: ev.shift,
            multi,
            dragged: false,
            on_chip: false,
            after_menu: self.menu_open,
            overlay: false,
            text: deps.text_cursor(),
        });
        self.last_left_down = Some(LastDown { x: ev.x, y: ev.y, at: ev.time_ms });
        self.menu_open = false;
    }

    fn on_move(&mut self, ev: &GestureEvent) {
        let Some(p) = self.left.as_mut() else { return };
        if p.dragged || p.on_chip || p.overlay {
            return;
        }
        if distance(p.x, p.y, ev.x, ev.y) > DRAG_THRESHOLD_PX {
            p.dragged = true;
            self.last_left_down = None;
        }
    }

    fn on_left_up(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps, out: &mut Vec<Decision>) {
        let Some(p) = self.left.take() else { return };
        if p.on_chip || p.overlay {
            return;
        }
        // Moves can be coalesced by the hook: re-check the distance at release.
        let dragged = p.dragged || distance(p.x, p.y, ev.x, ev.y) > DRAG_THRESHOLD_PX;
        if dragged {
            self.last_left_down = None;
        }
        if dragged || p.multi || p.shift {
            // Judge the window at the PRESS point: a selection drag may overshoot onto another window.
            let target = deps.target_at(p.x, p.y);
            if target.as_ref().is_some_and(|t| t.blocked) {
                self.clear_hint(HintClearReason::Blocked, out);
                return;
            }
            // No text cursor at either end: a game's box-select, a brush stroke, a window or scrollbar drag. Not a
            // new selection, and not a reason to forget an existing one either.
            if dragged && !p.multi && !p.shift && !Self::dragged_over_text(&p, deps) {
                return;
            }
            let kind = if dragged {
                HintKind::Drag
            } else if p.multi {
                HintKind::MultiClick
            } else {
                HintKind::ShiftClick
            };
            self.hint = Some(Hint { kind, at: ev.time_ms, target: target.map(|t| t.id) });
            out.push(Decision::HintSet(kind));
            if self.cfg.selection_chip && !deps.chips_suppressed() {
                self.chip = Some(Chip { mode: ChipMode::Selection, at: ev.time_ms, ttl: SELECTION_CHIP_MS });
                // The RELEASE point.
                out.push(Decision::ShowSelectionChip { x: ev.x, y: ev.y });
            }
            return;
        }
        // A click right after a right-click picks or dismisses the native menu; the selection usually survives it.
        if p.after_menu {
            return;
        }
        self.clear_hint(HintClearReason::Click, out);
    }

    /// A right-click on the window the live hint belongs to (unknown identities get the benefit of the doubt).
    fn armed_for(&self, ev: &GestureEvent, deps: &dyn GestureDeps) -> bool {
        if !self.cfg.context_chip || !self.live_hint(ev.time_ms) {
            return false;
        }
        let Some(hint) = self.hint.as_ref() else { return false };
        if deps.chips_suppressed() {
            return false;
        }
        let target = deps.target_at(ev.x, ev.y);
        match target {
            Some(t) if t.blocked => false,
            Some(t) => hint.target.as_ref().is_none_or(|id| *id == t.id),
            None => true,
        }
    }

    fn on_right_down(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps, out: &mut Vec<Decision>) {
        if self.inside_chip(ev, deps) {
            // Nothing changes: the chip UI dismisses itself.
            return;
        }
        self.menu_open = true;
        self.last_left_down = None;
        if self.armed_for(ev, deps) {
            // Replaces a visible selection chip without a hide decision. The right-PRESS point.
            self.chip = Some(Chip { mode: ChipMode::Context, at: ev.time_ms, ttl: CONTEXT_CHIP_MS });
            out.push(Decision::ShowContextChip { x: ev.x, y: ev.y });
            return;
        }
        if self.chip.take().is_some() {
            out.push(Decision::HideChip(ChipHideReason::MouseDown));
        }
    }

    fn on_other_down(&mut self, ev: &GestureEvent, deps: &dyn GestureDeps, out: &mut Vec<Decision>) {
        self.last_left_down = None;
        self.menu_open = false;
        if self.chip.is_some() && !deps.is_inside_chip(ev.x, ev.y) {
            self.chip = None;
            out.push(Decision::HideChip(ChipHideReason::MouseDown));
        }
    }

    fn on_key_down(
        &mut self,
        ev: &GestureEvent,
        vk: u32,
        extended: bool,
        deps: &dyn GestureDeps,
        out: &mut Vec<Decision>,
    ) {
        // Held modifiers auto-repeat keydown: they must neither hide the chip nor touch the hint.
        if is_modifier_vk(vk) {
            return;
        }
        if self.chip.take().is_some() {
            out.push(Decision::HideChip(ChipHideReason::Key));
        }
        if is_navigation_vk(vk) {
            // Shift+arrows extend a selection (unless they navigate an open context menu). Plain navigation keys keep
            // the hint and `menu_open` (menu navigation).
            if ev.shift && !self.menu_open {
                self.set_keyboard_hint(HintKind::Keyboard, ev.time_ms, deps, out);
            }
            return;
        }
        // Shift allowed; AltGr arrives as Ctrl+Alt, so AltGr+A is an ordinary key.
        if vk == VK_A && ev.ctrl && !ev.alt && !ev.meta {
            self.menu_open = false;
            self.set_keyboard_hint(HintKind::SelectAll, ev.time_ms, deps, out);
            return;
        }
        if self.menu_open {
            // Escape / Enter / a mnemonic went to the native menu and closed it; the selection is untouched.
            self.menu_open = false;
            return;
        }
        if is_neutral_vk(vk) {
            return;
        }
        // Copy shortcuts keep the selection (Shift and Win allowed; AltGr+C is not a copy).
        if ev.ctrl && !ev.alt && (vk == VK_C || (vk == VK_INSERT && extended)) {
            return;
        }
        self.clear_hint(HintClearReason::Key, out);
    }

    fn set_keyboard_hint(&mut self, kind: HintKind, at: u64, deps: &dyn GestureDeps, out: &mut Vec<Decision>) {
        let target = deps.focused_target();
        // Selecting inside Vox (or an excluded app) leaves any hint for another window alone.
        if target.as_ref().is_some_and(|t| t.blocked) {
            return;
        }
        let hint = Hint { kind, at, target: target.map(|t| t.id) };
        // The timestamp is always refreshed; auto-repeat of Shift+Arrow reports only transitions.
        let changed = self.hint.as_ref().is_none_or(|prev| prev.kind != hint.kind || prev.target != hint.target);
        self.hint = Some(hint);
        if changed {
            out.push(Decision::HintSet(kind));
        }
    }
}

/// The 8 side-specific modifier VKs (plus the generic Shift/Ctrl/Alt and Win keys).
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// Arrows, Home, End, PageUp, PageDown (regardless of the extended flag).
pub fn is_navigation_vk(vk: u32) -> bool {
    matches!(vk, 0x21..=0x28)
}

/// Keys that never change a selection (selection.md §4.3 table, as libuiohook produced them on Windows).
pub fn is_neutral_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x1B | 0x14
            | 0x90
            | 0x91
            | 0x2C
            | 0x5D
            | 0xAD
            | 0xAE
            | 0xAF
            | 0xB0
            | 0xB1
            | 0xB2
            | 0xB3
            | 0xB5
            | 0x5F
            | 0xB6
            | 0xB7
            | 0xE5
            | 0xE6
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    use ChipHideReason as H;
    use Decision as D;
    use HintClearReason as C;

    /// Scriptable dependencies. `targets` maps an x coordinate range start to a target (`x < 1000` → `app_a`,
    /// otherwise `app_b` by default).
    struct Fake {
        inside_chip: Cell<bool>,
        targets: RefCell<Vec<(i32, Option<GestureTarget>)>>,
        focused: RefCell<Option<GestureTarget>>,
        overlay: Cell<bool>,
        /// Answers for successive `text_cursor()` calls; the last one repeats.
        text: RefCell<Vec<Option<bool>>>,
        text_calls: Cell<u32>,
        suppressed: Cell<bool>,
        target_calls: RefCell<Vec<(i32, i32)>>,
    }

    fn target(id: &str, blocked: bool) -> Option<GestureTarget> {
        Some(GestureTarget { id: id.into(), blocked })
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                inside_chip: Cell::new(false),
                targets: RefCell::new(vec![(0, target("1", false)), (1000, target("2", false))]),
                focused: RefCell::new(target("1", false)),
                overlay: Cell::new(false),
                text: RefCell::new(vec![Some(true)]),
                text_calls: Cell::new(0),
                suppressed: Cell::new(false),
                target_calls: RefCell::new(Vec::new()),
            }
        }
        fn set_text(&self, answers: &[Option<bool>]) {
            *self.text.borrow_mut() = answers.to_vec();
        }
        fn set_targets(&self, t: Vec<(i32, Option<GestureTarget>)>) {
            *self.targets.borrow_mut() = t;
        }
    }

    impl GestureDeps for Fake {
        fn is_inside_chip(&self, _x: i32, _y: i32) -> bool {
            self.inside_chip.get()
        }
        fn target_at(&self, x: i32, y: i32) -> Option<GestureTarget> {
            self.target_calls.borrow_mut().push((x, y));
            let targets = self.targets.borrow();
            targets.iter().rev().find(|(from, _)| x >= *from).and_then(|(_, t)| t.clone())
        }
        fn focused_target(&self) -> Option<GestureTarget> {
            self.focused.borrow().clone()
        }
        fn own_overlay_at(&self, _x: i32, _y: i32) -> bool {
            self.overlay.get()
        }
        fn text_cursor(&self) -> Option<bool> {
            let n = self.text_calls.get();
            self.text_calls.set(n + 1);
            let answers = self.text.borrow();
            answers.get(n as usize).or(answers.last()).copied().flatten()
        }
        fn chips_suppressed(&self) -> bool {
            self.suppressed.get()
        }
    }

    fn ev(kind: GestureEventKind, x: i32, y: i32, t: u64) -> GestureEvent {
        GestureEvent { kind, x, y, clicks: 1, time_ms: t, shift: false, ctrl: false, alt: false, meta: false }
    }
    fn down(b: MouseButton, x: i32, y: i32, t: u64) -> GestureEvent {
        ev(GestureEventKind::MouseDown(b), x, y, t)
    }
    fn up(b: MouseButton, x: i32, y: i32, t: u64) -> GestureEvent {
        ev(GestureEventKind::MouseUp(b), x, y, t)
    }
    fn mv(x: i32, y: i32, t: u64) -> GestureEvent {
        ev(GestureEventKind::MouseMove, x, y, t)
    }
    fn key(vk: u32, t: u64) -> GestureEvent {
        let mut e = ev(GestureEventKind::KeyDown { vk, extended: false }, 0, 0, t);
        e.clicks = 0;
        e
    }
    fn key_ext(vk: u32, t: u64) -> GestureEvent {
        let mut e = key(vk, t);
        e.kind = GestureEventKind::KeyDown { vk, extended: true };
        e
    }
    fn with(mut e: GestureEvent, shift: bool, ctrl: bool, alt: bool, meta: bool) -> GestureEvent {
        e.shift = shift;
        e.ctrl = ctrl;
        e.alt = alt;
        e.meta = meta;
        e
    }
    fn clicks(mut e: GestureEvent, n: u32) -> GestureEvent {
        e.clicks = n;
        e
    }

    const L: MouseButton = MouseButton::Left;
    const R: MouseButton = MouseButton::Right;
    const VK_RIGHT: u32 = 0x27;
    const VK_DOWN: u32 = 0x28;
    const VK_RETURN: u32 = 0x0D;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_X: u32 = 0x58;

    fn both() -> GestureConfig {
        GestureConfig { context_chip: true, selection_chip: true }
    }
    fn context_only() -> GestureConfig {
        GestureConfig { context_chip: true, selection_chip: false }
    }

    struct Rig {
        m: GestureMachine,
        f: Fake,
    }

    impl Rig {
        fn new(cfg: GestureConfig) -> Self {
            Rig { m: GestureMachine::new(cfg), f: Fake::new() }
        }
        fn h(&mut self, e: GestureEvent) -> Vec<Decision> {
            self.m.handle(&e, &self.f)
        }
        /// Left drag from (x, y) to (x + dx, y) between t and t + 100.
        fn drag(&mut self, x: i32, y: i32, dx: i32, t: u64) -> Vec<Decision> {
            let mut out = self.h(down(L, x, y, t));
            out.extend(self.h(mv(x + dx, y, t + 50)));
            out.extend(self.h(up(L, x + dx, y, t + 100)));
            out
        }
        fn click(&mut self, x: i32, y: i32, t: u64) -> Vec<Decision> {
            let mut out = self.h(down(L, x, y, t));
            out.extend(self.h(up(L, x, y, t + 30)));
            out
        }
        fn right(&mut self, x: i32, y: i32, t: u64) -> Vec<Decision> {
            let mut out = self.h(down(R, x, y, t));
            out.extend(self.h(up(R, x, y, t + 30)));
            out
        }
    }

    // ───────────── selection.md §5.8 scenario checklist ─────────────

    #[test]
    fn s1_drag_then_right_click_shows_the_context_chip_and_a_press_activates_it() {
        let mut r = Rig::new(context_only());
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag)]);
        assert_eq!(r.right(150, 120, 1000), vec![D::ShowContextChip { x: 150, y: 120 }]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Context));
        r.f.inside_chip.set(true);
        assert_eq!(r.h(down(L, 170, 130, 2000)), vec![D::ActivateChip(ChipMode::Context)]);
        assert_eq!(r.m.chip_mode(), None);
        // The release of the chip press is neither a gesture nor a click: the hint survives.
        assert_eq!(r.h(up(L, 170, 130, 2050)), vec![]);
        r.f.inside_chip.set(false);
        assert_eq!(r.right(150, 120, 3000), vec![D::ShowContextChip { x: 150, y: 120 }]);
    }

    #[test]
    fn s2_double_and_triple_click_set_multi_click_hints_and_the_first_release_clears_an_old_one() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        // 1st click of the double click: a plain click clears the old hint.
        assert_eq!(r.click(300, 300, 1000), vec![D::HintCleared(C::Click)]);
        // 2nd press within 500 ms and 4 px → multi.
        assert_eq!(r.click(301, 300, 1200), vec![D::HintSet(HintKind::MultiClick)]);
        // 3rd press: still multi (hint-set again).
        assert_eq!(r.click(301, 301, 1400), vec![D::HintSet(HintKind::MultiClick)]);
        assert_eq!(r.right(301, 301, 2000), vec![D::ShowContextChip { x: 301, y: 301 }]);
    }

    #[test]
    fn s3_ctrl_a_arms_the_chip_only_in_the_same_window() {
        let mut r = Rig::new(context_only());
        assert_eq!(r.h(with(key(VK_A, 0), false, true, false, false)), vec![D::HintSet(HintKind::SelectAll)]);
        // Right-click in Notepad (window 1): chip.
        assert_eq!(r.right(10, 10, 100), vec![D::ShowContextChip { x: 10, y: 10 }]);
        // Right-click in another app's window: no chip, and the visible chip hides.
        assert_eq!(r.right(2000, 10, 200), vec![D::HideChip(H::MouseDown)]);
        assert_eq!(r.right(2000, 10, 300), vec![]);
    }

    #[test]
    fn s4_right_click_without_a_selection_gesture_shows_nothing() {
        let mut r = Rig::new(both());
        assert_eq!(r.right(10, 10, 0), vec![]);
        assert_eq!(r.m.chip_mode(), None);
    }

    #[test]
    fn s5_a_non_text_drag_neither_sets_nor_clears_the_hint() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.f.set_text(&[Some(false)]);
        assert_eq!(r.drag(500, 500, 100, 1000), vec![]);
        // Both ends were asked.
        assert_eq!(r.f.text_calls.get(), 3);
        // The earlier hint survives: a right-click still shows the chip.
        assert_eq!(r.right(100, 100, 2000), vec![D::ShowContextChip { x: 100, y: 100 }]);
    }

    #[test]
    fn s6_a_plain_click_elsewhere_clears_the_hint() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.click(600, 600, 1000), vec![D::HintCleared(C::Click)]);
        assert_eq!(r.right(100, 100, 2000), vec![]);
    }

    #[test]
    fn s7_clicking_copy_in_the_native_menu_keeps_the_hint() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.right(150, 100, 1000), vec![D::ShowContextChip { x: 150, y: 100 }]);
        // Left press on the menu item (outside the chip): the chip hides, the click picks the menu item.
        assert_eq!(r.click(180, 140, 1500), vec![D::HideChip(H::MouseDown)]);
        assert_eq!(r.right(150, 100, 2000), vec![D::ShowContextChip { x: 150, y: 100 }]);
    }

    #[test]
    fn s8_escape_closing_the_menu_hides_the_chip_and_keeps_the_hint() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.right(150, 100, 1000);
        assert_eq!(r.h(key(VK_ESCAPE, 1500)), vec![D::HideChip(H::Key)]);
        assert_eq!(r.right(150, 100, 2000), vec![D::ShowContextChip { x: 150, y: 100 }]);
    }

    #[test]
    fn s9_menu_navigation_keys_set_no_hint_and_enter_only_closes_the_menu() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.right(150, 100, 1000);
        assert_eq!(r.h(key_ext(VK_DOWN, 1100)), vec![D::HideChip(H::Key)]);
        assert_eq!(r.h(with(key_ext(VK_DOWN, 1200), true, false, false, false)), vec![]);
        assert_eq!(r.h(key(VK_RETURN, 1300)), vec![]);
        // menu_open was reset by Enter: the next key is a real key and clears the hint.
        assert_eq!(r.h(key(VK_X, 1400)), vec![D::HintCleared(C::Key)]);
    }

    #[test]
    fn s9b_hint_survives_enter_in_the_menu() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.right(150, 100, 1000);
        r.h(key_ext(VK_DOWN, 1100));
        r.h(key(VK_RETURN, 1300));
        assert_eq!(r.right(150, 100, 2000), vec![D::ShowContextChip { x: 150, y: 100 }]);
    }

    #[test]
    fn s10_selection_popup_appears_at_the_release_point_and_hides_on_every_trigger() {
        let mut r = Rig::new(both());
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag), D::ShowSelectionChip { x: 140, y: 100 }]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Selection));
        assert_eq!(r.m.next_deadline(), Some(100 + SELECTION_CHIP_MS));
        // 4 s after the release.
        assert_eq!(r.m.tick(4099), vec![]);
        assert_eq!(r.m.tick(4100), vec![D::HideChip(H::Timeout)]);
        // Any key.
        r.drag(100, 100, 40, 10_000);
        assert_eq!(r.h(key(VK_X, 10_200)), vec![D::HideChip(H::Key), D::HintCleared(C::Key)]);
        // The wheel (hint untouched).
        r.drag(100, 100, 40, 20_000);
        assert_eq!(r.h(ev(GestureEventKind::Wheel, 0, 0, 20_200)), vec![D::HideChip(H::Wheel)]);
        assert_eq!(r.h(ev(GestureEventKind::Wheel, 0, 0, 20_300)), vec![]);
        // A press elsewhere.
        r.drag(100, 100, 40, 30_000);
        let out = r.h(down(L, 900, 900, 30_200));
        assert_eq!(out, vec![D::HideChip(H::MouseDown)]);
        // A middle press elsewhere.
        r.h(up(L, 900, 900, 30_250));
        r.drag(100, 100, 40, 40_000);
        assert_eq!(r.h(down(MouseButton::Middle, 900, 900, 40_200)), vec![D::HideChip(H::MouseDown)]);
    }

    #[test]
    fn s11_drag_in_vox_main_window_clears_the_hint_and_the_widget_is_ignored() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.f.set_targets(vec![(0, target("9", true))]);
        assert_eq!(r.drag(100, 100, 40, 1000), vec![D::HintCleared(C::Blocked)]);
        // Clicking or dragging the floating widget (our non-activating overlay) is ignored entirely.
        r.f.set_targets(vec![(0, target("1", false))]);
        r.drag(100, 100, 40, 2000);
        r.f.overlay.set(true);
        assert_eq!(r.h(down(L, 5, 5, 3000)), vec![]);
        assert!(!r.m.wants_mouse_move());
        assert_eq!(r.h(up(L, 200, 5, 3100)), vec![]);
        assert_eq!(r.click(5, 5, 3200), vec![]);
        r.f.overlay.set(false);
        assert_eq!(r.right(100, 100, 4000), vec![D::ShowContextChip { x: 100, y: 100 }]);
    }

    #[test]
    fn s12_fullscreen_or_presentation_suppresses_chips_but_not_hints() {
        let mut r = Rig::new(both());
        r.f.suppressed.set(true);
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag)]);
        assert_eq!(r.right(100, 100, 1000), vec![]);
        r.f.suppressed.set(false);
        // The hint was tracked meanwhile: the chip shows once suppression ends.
        assert_eq!(r.right(100, 100, 2000), vec![D::ShowContextChip { x: 100, y: 100 }]);
    }

    #[test]
    fn s13_shift_drag_without_an_ibeam_still_sets_a_drag_hint() {
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false)]);
        let mut out = r.h(with(down(L, 100, 100, 0), true, false, false, false));
        out.extend(r.h(mv(200, 100, 50)));
        out.extend(r.h(up(L, 200, 100, 100)));
        assert_eq!(out, vec![D::HintSet(HintKind::Drag)]);
        // The release was not asked (shift skips the check).
        assert_eq!(r.f.text_calls.get(), 1);
    }

    #[test]
    fn s14_altgr_c_clears_the_hint() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(with(key(VK_C, 1000), false, true, true, false)), vec![D::HintCleared(C::Key)]);
    }

    // ───────────── boundary vectors (ARCHITECTURE.md §9) ─────────────

    #[test]
    fn drag_threshold_is_strictly_more_than_six_pixels() {
        // Exactly 6 px (moves and release): a plain click, not a drag.
        let mut r = Rig::new(context_only());
        r.drag(0, 0, 0, 0); // hint? no: a plain click with no hint → nothing
        r.h(with(key(VK_A, 10), false, true, false, false));
        let mut out = r.h(down(L, 100, 100, 1000));
        out.extend(r.h(mv(106, 100, 1010)));
        assert!(r.m.wants_mouse_move(), "6 px is not a drag yet");
        out.extend(r.h(up(L, 106, 100, 1020)));
        assert_eq!(out, vec![D::HintCleared(C::Click)]);
        // √37 ≈ 6.08 px: a drag (detected on the move).
        let mut out = r.h(down(L, 100, 100, 2000));
        out.extend(r.h(mv(106, 101, 2010)));
        assert!(!r.m.wants_mouse_move(), "a detected drag stops wanting moves");
        out.extend(r.h(up(L, 106, 101, 2020)));
        assert_eq!(out, vec![D::HintSet(HintKind::Drag)]);
        // Coalesced moves: the release alone decides (0 moves, release 6.08 px away).
        let mut r = Rig::new(context_only());
        r.h(down(L, 0, 0, 0));
        assert_eq!(r.h(up(L, 1, 6, 10)), vec![D::HintSet(HintKind::Drag)]);
        // Diagonal 4,4 = 5.66 px: a click.
        let mut r = Rig::new(context_only());
        r.h(down(L, 0, 0, 0));
        assert_eq!(r.h(up(L, 4, 4, 10)), vec![]);
        assert_eq!(r.m.next_deadline(), None);
    }

    #[test]
    fn multi_click_is_four_pixels_and_500_ms_inclusive_or_the_hook_counter() {
        // 4 px and exactly 500 ms → multi.
        let mut r = Rig::new(context_only());
        r.click(100, 100, 0);
        r.h(down(L, 104, 100, 500));
        assert_eq!(r.h(up(L, 104, 100, 520)), vec![D::HintSet(HintKind::MultiClick)]);
        // 501 ms with clicks = 1 → not multi.
        let mut r = Rig::new(context_only());
        r.click(100, 100, 0);
        r.h(down(L, 100, 100, 501));
        assert_eq!(r.h(up(L, 100, 100, 520)), vec![]);
        // 900 ms but the hook's counter says 2 (long system double-click time) → multi.
        let mut r = Rig::new(context_only());
        r.click(100, 100, 0);
        r.h(clicks(down(L, 100, 100, 900), 2));
        assert_eq!(r.h(up(L, 100, 100, 920)), vec![D::HintSet(HintKind::MultiClick)]);
        // 4.12 px apart → never multi, even with clicks = 2.
        let mut r = Rig::new(context_only());
        r.click(100, 100, 0);
        r.h(clicks(down(L, 104, 101, 100), 2));
        assert_eq!(r.h(up(L, 104, 101, 120)), vec![]);
    }

    #[test]
    fn hint_expiry_uses_ge_and_liveness_uses_lt_at_60_seconds() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0); // hint at t = 100
        assert_eq!(r.m.next_deadline(), Some(100 + HINT_TTL_MS));
        assert_eq!(r.h(down(R, 100, 100, 100 + HINT_TTL_MS - 1)), vec![D::ShowContextChip { x: 100, y: 100 }]);
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(down(R, 100, 100, 100 + HINT_TTL_MS)), vec![D::HintCleared(C::Expired)]);
        // tick() directly: one ms early keeps it.
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.m.tick(100 + HINT_TTL_MS - 1), vec![]);
        assert_eq!(r.m.tick(100 + HINT_TTL_MS), vec![D::HintCleared(C::Expired)]);
        assert_eq!(r.m.next_deadline(), None);
    }

    #[test]
    fn next_deadline_is_the_earliest_of_hint_and_chip() {
        let mut r = Rig::new(both());
        assert_eq!(r.m.next_deadline(), None);
        r.drag(100, 100, 40, 0); // hint + selection chip at 100
        assert_eq!(r.m.next_deadline(), Some(100 + SELECTION_CHIP_MS));
        r.right(100, 100, 1000); // context chip at 1000 (hint still at 100)
        assert_eq!(r.m.next_deadline(), Some(1000 + CONTEXT_CHIP_MS));
        assert_eq!(r.m.tick(1000 + CONTEXT_CHIP_MS), vec![D::HideChip(H::Timeout)]);
        assert_eq!(r.m.next_deadline(), Some(100 + HINT_TTL_MS));
    }

    #[test]
    fn wants_mouse_move_only_while_a_gesture_press_is_undecided() {
        let mut r = Rig::new(both());
        assert!(!r.m.wants_mouse_move());
        r.h(down(L, 0, 0, 0));
        assert!(r.m.wants_mouse_move());
        r.h(mv(10, 0, 10));
        assert!(!r.m.wants_mouse_move());
        r.h(up(L, 10, 0, 20));
        assert!(!r.m.wants_mouse_move());
        // A press on the chip.
        r.f.inside_chip.set(true);
        r.h(down(L, 140, 0, 100));
        assert!(!r.m.wants_mouse_move());
    }

    #[test]
    fn configure_hides_only_the_chip_of_the_disabled_mode() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.m.configure(context_only()), vec![D::HideChip(H::Disabled)]);
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        // Turning off the right-click chip leaves the selection chip.
        assert_eq!(r.m.configure(GestureConfig { context_chip: false, selection_chip: true }), vec![]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Selection));
        // A context chip hides when the right-click chip is turned off.
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        r.right(100, 100, 1000);
        assert_eq!(r.m.configure(GestureConfig { context_chip: true, selection_chip: false }), vec![]);
        assert_eq!(
            r.m.configure(GestureConfig { context_chip: false, selection_chip: false }),
            vec![D::HideChip(H::Disabled)]
        );
        assert_eq!(r.m.configure(both()), vec![]);
    }

    #[test]
    fn a_right_press_inside_the_chip_changes_nothing() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.right(150, 100, 1000);
        r.f.inside_chip.set(true);
        assert_eq!(r.h(down(R, 160, 110, 1500)), vec![]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Context));
        // menu_open was already true from the first right-click; a plain click after it still counts as menu click.
        r.f.inside_chip.set(false);
        assert_eq!(r.click(500, 500, 1600), vec![D::HideChip(H::MouseDown)]);
        // An X press inside the chip also changes nothing.
        r.right(150, 100, 2000);
        r.f.inside_chip.set(true);
        assert_eq!(r.h(down(MouseButton::X1, 160, 110, 2100)), vec![]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Context));
    }

    #[test]
    fn a_context_chip_replaces_a_selection_chip_without_a_hide() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Selection));
        assert_eq!(r.h(down(R, 120, 100, 500)), vec![D::ShowContextChip { x: 120, y: 100 }]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Context));
        // The right release is ignored.
        assert_eq!(r.h(up(R, 120, 100, 600)), vec![]);
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Context));
    }

    #[test]
    fn reset_emits_hide_then_clear_and_forgets_everything() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.m.reset(), vec![D::HideChip(H::Reset), D::HintCleared(C::Reset)]);
        assert_eq!(r.m.reset(), vec![]);
        assert_eq!(r.m.next_deadline(), None);
        assert_eq!(r.right(100, 100, 1000), vec![]);
        // hide_chip with nothing visible is silent; with a chip it reports the reason.
        assert_eq!(r.m.hide_chip(H::Dismissed), vec![]);
        r.drag(100, 100, 40, 2000);
        assert_eq!(r.m.hide_chip(H::Dismissed), vec![D::HideChip(H::Dismissed)]);
    }

    #[test]
    fn blocked_mouse_gestures_clear_the_hint_but_blocked_keyboard_hints_leave_it() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        // Keyboard selection inside a blocked window: hint (for window 1) untouched.
        *r.f.focused.borrow_mut() = target("7", true);
        assert_eq!(r.h(with(key(VK_A, 500), false, true, false, false)), vec![]);
        assert_eq!(r.h(with(key_ext(VK_RIGHT, 600), true, false, false, false)), vec![]);
        assert_eq!(r.right(100, 100, 1000), vec![D::ShowContextChip { x: 100, y: 100 }]);
        // A mouse gesture (double click) in a blocked window clears it. (The first click of the double click is a
        // plain click and clears the drag hint; a keyboard hint made in between is the one the double click clears.)
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.f.set_targets(vec![(0, target("7", true))]);
        assert_eq!(r.click(500, 500, 1000), vec![D::HintCleared(C::Click)]);
        assert_eq!(r.h(with(key(VK_A, 1100), false, true, false, false)), vec![D::HintSet(HintKind::SelectAll)]);
        assert_eq!(r.click(500, 500, 1200), vec![D::HintCleared(C::Blocked)]);
        // A right press on a blocked window never arms the chip.
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.f.set_targets(vec![(0, target("7", true))]);
        assert_eq!(r.right(100, 100, 1000), vec![]);
    }

    #[test]
    fn the_gesture_target_is_judged_at_the_press_point() {
        let mut r = Rig::new(context_only());
        r.drag(900, 100, 400, 0); // overshoots into window 2 (x ≥ 1000)
        assert_eq!(r.f.target_calls.borrow().as_slice(), &[(900, 100)]);
        // The hint belongs to window 1: a right-click in window 2 shows nothing.
        assert_eq!(r.right(1200, 100, 1000), vec![]);
        assert_eq!(r.right(950, 100, 2000), vec![D::ShowContextChip { x: 950, y: 100 }]);
    }

    #[test]
    fn unknown_identities_get_the_benefit_of_the_doubt() {
        // Hint target unknown.
        let mut r = Rig::new(context_only());
        r.f.set_targets(vec![(0, None)]);
        r.drag(100, 100, 40, 0);
        r.f.set_targets(vec![(0, target("5", false))]);
        assert_eq!(r.right(100, 100, 1000), vec![D::ShowContextChip { x: 100, y: 100 }]);
        // Right-click target unknown.
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.f.set_targets(vec![(0, None)]);
        assert_eq!(r.right(100, 100, 1000), vec![D::ShowContextChip { x: 100, y: 100 }]);
    }

    #[test]
    fn ctrl_insert_extended_keeps_the_hint_but_numpad_insert_clears_it() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(with(key_ext(VK_INSERT, 500), false, true, false, false)), vec![]);
        assert_eq!(r.h(with(key(VK_C, 600), true, true, false, true)), vec![], "Ctrl+Shift+Win+C still copies");
        assert_eq!(r.h(with(key(VK_INSERT, 700), false, true, false, false)), vec![D::HintCleared(C::Key)]);
    }

    #[test]
    fn altgr_a_and_altgr_c_are_ordinary_keys() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(with(key(VK_A, 500), false, true, true, false)), vec![D::HintCleared(C::Key)]);
        r.drag(100, 100, 40, 1000);
        assert_eq!(r.h(with(key(VK_C, 1500), false, true, true, false)), vec![D::HintCleared(C::Key)]);
        // Ctrl+Win+A is not select-all either.
        r.drag(100, 100, 40, 2000);
        assert_eq!(r.h(with(key(VK_A, 2500), false, true, false, true)), vec![D::HintCleared(C::Key)]);
    }

    #[test]
    fn ctrl_shift_a_sets_select_all_and_closes_the_menu() {
        let mut r = Rig::new(context_only());
        r.right(100, 100, 0); // menu open, no hint
        assert_eq!(r.h(with(key(VK_A, 100), true, true, false, false)), vec![D::HintSet(HintKind::SelectAll)]);
        // menu_open was reset: Shift+Right now extends (same target, different kind → hint-set).
        assert_eq!(r.h(with(key_ext(VK_RIGHT, 200), true, false, false, false)), vec![D::HintSet(HintKind::Keyboard)]);
    }

    #[test]
    fn launch_mail_is_not_neutral_and_ime_processkey_is() {
        assert!(!is_neutral_vk(0xB4));
        assert!(is_neutral_vk(0xE5));
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(key(0xE5, 500)), vec![]);
        assert_eq!(r.h(key(0xB4, 600)), vec![D::HintCleared(C::Key)]);
    }

    #[test]
    fn keyboard_hints_do_not_flood_on_auto_repeat() {
        let mut r = Rig::new(context_only());
        let shift_right = |t| with(key_ext(VK_RIGHT, t), true, false, false, false);
        assert_eq!(r.h(shift_right(0)), vec![D::HintSet(HintKind::Keyboard)]);
        assert_eq!(r.h(shift_right(30)), vec![]);
        assert_eq!(r.h(shift_right(60)), vec![]);
        // The timestamp was refreshed by the repeats.
        assert_eq!(r.m.next_deadline(), Some(60 + HINT_TTL_MS));
        // Another focused window → a transition.
        *r.f.focused.borrow_mut() = target("2", false);
        assert_eq!(r.h(shift_right(90)), vec![D::HintSet(HintKind::Keyboard)]);
        // Plain arrows keep the hint and set nothing.
        assert_eq!(r.h(key_ext(VK_RIGHT, 120)), vec![]);
        assert_eq!(r.m.next_deadline(), Some(90 + HINT_TTL_MS));
    }

    #[test]
    fn modifier_keydowns_have_no_effect_at_all() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        for vk in [0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5] {
            assert!(is_modifier_vk(vk));
            assert_eq!(r.h(key(vk, 500)), vec![]);
        }
        assert_eq!(r.m.chip_mode(), Some(ChipMode::Selection));
    }

    #[test]
    fn key_classifiers_match_the_spec_sets() {
        for vk in 0x21..=0x28 {
            assert!(is_navigation_vk(vk));
        }
        assert!(!is_navigation_vk(0x20));
        assert!(!is_navigation_vk(0x2D));
        assert!(!is_navigation_vk(0x67), "numpad 7 with NumLock on is not navigation");
        for vk in [
            0x1B, 0x14, 0x90, 0x91, 0x2C, 0x5D, 0xAD, 0xAE, 0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xB5, 0x5F, 0xB6, 0xB7, 0xE5,
            0xE6,
        ] {
            assert!(is_neutral_vk(vk), "{vk:#x}");
        }
        for vk in [0x0D, 0x08, 0x20, 0x41, 0xA6, 0xA7, 0xA8, 0xB4] {
            assert!(!is_neutral_vk(vk), "{vk:#x} (browser keys are not neutral)");
        }
        assert!(!is_modifier_vk(0x14));
        assert!(!is_modifier_vk(0x5D));
    }

    #[test]
    fn neutral_keys_and_copy_keep_the_hint_while_typing_clears_it() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        assert_eq!(r.h(key(VK_ESCAPE, 100)), vec![]);
        assert_eq!(r.h(key(0xAF, 200)), vec![]);
        assert_eq!(r.h(with(key(VK_C, 300), false, true, false, false)), vec![]);
        assert_eq!(r.h(key(VK_X, 400)), vec![D::HintCleared(C::Key)]);
        // Clearing an absent hint emits nothing.
        assert_eq!(r.h(key(VK_X, 500)), vec![]);
    }

    #[test]
    fn double_click_drag_is_a_drag_and_skips_the_text_cursor_check() {
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false)]);
        r.click(100, 100, 0);
        let calls = r.f.text_calls.get();
        r.h(down(L, 100, 100, 200));
        r.h(mv(200, 100, 250));
        assert_eq!(r.h(up(L, 200, 100, 300)), vec![D::HintSet(HintKind::Drag)]);
        // Asked at the second press only; never at the release.
        assert_eq!(r.f.text_calls.get(), calls + 1);
    }

    #[test]
    fn text_cursor_at_either_end_makes_a_drag_count() {
        // Press: definite no, release: yes.
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false), Some(true)]);
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag)]);
        assert_eq!(r.f.text_calls.get(), 2);
        // Press: unknown → no question at the release.
        let mut r = Rig::new(context_only());
        r.f.set_text(&[None, Some(false)]);
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag)]);
        assert_eq!(r.f.text_calls.get(), 1);
        // Press: no, release: unknown → counts.
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false), None]);
        assert_eq!(r.drag(100, 100, 40, 0), vec![D::HintSet(HintKind::Drag)]);
    }

    #[test]
    fn a_non_text_drag_resets_multi_click_tracking() {
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false)]);
        r.drag(100, 100, 40, 0);
        // A press right after (same spot as the drag's press, 200 ms later) is not a multi-click.
        r.h(down(L, 100, 100, 200));
        assert_eq!(r.h(up(L, 100, 100, 220)), vec![]);
    }

    #[test]
    fn a_right_click_resets_multi_click_tracking_and_marks_the_next_click_as_a_menu_click() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.click(300, 300, 1000); // clears the hint
        r.h(with(key(VK_A, 1050), false, true, false, false)); // select-all hint
        r.right(300, 300, 1100);
        // Press within 500 ms and 4 px of the earlier click, but the right press reset the tracking: not multi (a
        // multi-click would set a hint at the release); and it follows the right-click, so it does not clear the hint.
        assert_eq!(r.h(down(L, 300, 300, 1200)), vec![D::HideChip(H::MouseDown)]);
        assert_eq!(r.h(up(L, 300, 300, 1220)), vec![]);
        assert_eq!(r.right(300, 300, 1300), vec![D::ShowContextChip { x: 300, y: 300 }]);
    }

    #[test]
    fn the_chip_press_clears_multi_click_tracking_and_the_menu_flag() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0); // selection chip at 140,100
        r.f.inside_chip.set(true);
        assert_eq!(r.h(down(L, 140, 100, 300)), vec![D::ActivateChip(ChipMode::Selection)]);
        r.f.inside_chip.set(false);
        assert_eq!(r.h(up(L, 140, 100, 350)), vec![]);
        // A quick press at the same spot is a first click (lastLeftDown was cleared): it clears the hint.
        r.h(down(L, 140, 100, 400));
        assert_eq!(r.h(up(L, 140, 100, 420)), vec![D::HintCleared(C::Click)]);
    }

    #[test]
    fn shift_click_sets_a_shift_click_hint() {
        let mut r = Rig::new(context_only());
        r.f.set_text(&[Some(false)]);
        r.h(with(down(L, 100, 100, 0), true, false, false, false));
        assert_eq!(r.h(up(L, 100, 100, 20)), vec![D::HintSet(HintKind::ShiftClick)]);
    }

    #[test]
    fn other_buttons_hide_the_chip_only_outside_it_and_never_touch_the_hint() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        r.f.inside_chip.set(true);
        assert_eq!(r.h(down(MouseButton::Middle, 140, 100, 200)), vec![]);
        r.f.inside_chip.set(false);
        assert_eq!(r.h(down(MouseButton::X2, 900, 900, 300)), vec![D::HideChip(H::MouseDown)]);
        assert_eq!(r.h(up(MouseButton::X2, 900, 900, 320)), vec![]);
        assert_eq!(r.right(100, 100, 400), vec![D::ShowContextChip { x: 100, y: 100 }]);
    }

    #[test]
    fn handle_prepends_expiry_decisions() {
        let mut r = Rig::new(both());
        r.drag(100, 100, 40, 0);
        let out = r.h(key(VK_X, 100 + HINT_TTL_MS));
        assert_eq!(out, vec![D::HintCleared(C::Expired), D::HideChip(H::Timeout)]);
    }

    #[test]
    fn the_context_chip_times_out_after_8_seconds() {
        let mut r = Rig::new(context_only());
        r.drag(100, 100, 40, 0);
        r.right(100, 100, 1000);
        assert_eq!(r.m.tick(1000 + CONTEXT_CHIP_MS - 1), vec![]);
        assert_eq!(r.m.tick(1000 + CONTEXT_CHIP_MS), vec![D::HideChip(H::Timeout)]);
    }

    #[test]
    fn a_release_without_a_press_is_ignored() {
        let mut r = Rig::new(both());
        assert_eq!(r.h(up(L, 0, 0, 0)), vec![]);
        assert_eq!(r.h(mv(50, 50, 10)), vec![]);
    }
}
