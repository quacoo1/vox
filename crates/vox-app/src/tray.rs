//! The tray (shell.md §7; ARCHITECTURE.md §8 D-SH-18): `vox_win::notify_icon::NotifyIcon` (version-4 notifications)
//! plus a `muda` menu, created on the main thread after the event loop exists.
//!
//! - Icon: a white rounded "V" wave mark on transparent (DESIGN.md "Tray"); the active variant (playing/loading) adds
//!   a small dot with a transparent ring cut around it. A faint dark halo keeps the white mark readable on a light
//!   taskbar. Rendered once per state at 16/20/24/32/40/48 px by the small anti-aliased rasterizer in [`mark`]
//!   (vox-app has no tiny-skia dependency; the drawing is a few round-capped strokes, which a signed-distance
//!   rasterizer with 4×4 supersampling renders exactly; pixels whose centre is clearly inside or outside skip the
//!   supersampling) and cached for the process ([`tray_images`]), so a play/pause flip only swaps bitmaps. The same drawing on a dark rounded tile produces the checked-in
//!   `crates/vox-app/assets/vox.ico` + `vox-256.png` through the `#[ignore]` generator test
//!   `tray::tests::generate_app_icon_assets` (D-SH-14, D-SH-20). `NotifyIcon` picks the size equal to
//!   `GetSystemMetricsForDpi(SM_CXSMICON)` of the primary monitor (nearest larger, else 32) and re-picks on
//!   `UiUpdate::DisplaysChanged` ([`Tray::refresh_dpi`]); images are swapped only when the state changes.
//! - Tooltip / menu from [`crate::ui::tray_view::TrayView`]; the menu is rebuilt only when `menu_key()` changes.
//!   Hints are **display text**, never muda accelerators: an item with a hint is labelled `format!("{label}\t{hint}")`
//!   (Win32 menus right-align the text after a tab), so the raw setting string is shown, never registered and never
//!   rejected by muda's accelerator parser (`num0`, `MediaPlayPause`, `F13`…). The warn-and-rebuild-without-hints path
//!   remains only for muda build errors.
//! - Activation (BRIEF.md Product rulings "Tray"): `NotifyIconEvent::Activate` (left click, double click, keyboard
//!   select) is handled **synchronously on the UI thread** in the event handler ([`crate::ui::tray_open`]:
//!   `windowing::main_window::show` — Slint show + restore + `SetForegroundWindow` while the tray click's foreground
//!   permission is still valid); the same for the menu's `Open Vox` / `History` / `Analytics` / `Settings` (navigate
//!   via `ui::shell` first). Only the resulting `WindowEvent::MainState` goes to the core.
//!   `NotifyIconEvent::ContextMenu{x, y}` → `muda::ContextMenu::show_context_menu_for_hwnd(icon.hwnd(), position)`
//!   (modal, `TPM_RETURNCMD`; muda calls `SetForegroundWindow` before `TrackPopupMenu`), then `WM_NULL` is posted to
//!   the tray's window (shell.md §7.4; muda does not); the selected item's `MenuEvent` is drained from muda's channel
//!   right after it returns.
//!   Other menu items → `Command::Tray(..)` (checkbox items send the negation of the value the menu was built with);
//!   `Quit Vox` commits a pending History undo batch first, then `Command::Quit` ([`crate::ui::tray_quit`]).
//! - Menu exactly as shell.md §7.3: `Read clipboard` [hint] · `Pause`/`Resume` [hint] · `Stop` [hint] · — ·
//!   `Open Vox` · `History` · `Analytics` · `Settings` · — · ☑ `Right-click chip` · ☑ `Floating widget` ·
//!   ☑ `Hotkeys` · — · `Quit Vox`. Labels built from `APP_NAME`; enabled states per shell.md §7.3 /
//!   reader-playback.md §8.10; hints only when `hotkey::tray_hint_ok`.
//! - `TaskbarCreated` (Explorer restart): `NotifyIcon` re-adds the icon.
//! - Manual test: Win+B, arrow to Vox, Enter → the main window opens (not the menu).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use muda::{CheckMenuItem, ContextMenu, Menu, MenuId, MenuItem, PredefinedMenuItem};
use vox_core::app::AppPage;
use vox_core::constants::APP_NAME;
use vox_win::notify_icon::{NotifyIcon, NotifyIconEvent, TrayIconImage};

use crate::command::{Command, CoreHandle, TrayAction};
use crate::ui::tray_view::{with_hint, TrayView};

/// The sizes rendered for the notification area (`NotifyIcon` picks the one matching `SM_CXSMICON`).
pub const TRAY_SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];
/// The sizes of `assets/vox.ico` (D-SH-20; written by the generator test).
#[cfg(test)]
pub const APP_ICON_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// The built menu and what each item does.
struct BuiltMenu {
    menu: Menu,
    key: String,
    actions: Vec<(MenuId, TrayAction)>,
}

/// State shared with the icon's event handler (the handler runs on the UI thread, outside any `AppUi` borrow).
#[derive(Default)]
struct Shared {
    menu: RefCell<Option<BuiltMenu>>,
    hwnd: Cell<isize>,
}

/// The tray icon and its menu.
pub struct Tray {
    shown: Option<TrayView>,
    icon: Option<NotifyIcon>,
    shared: Rc<Shared>,
    /// The icon state currently shown (`None` before the first update).
    active: Option<bool>,
}

impl Tray {
    pub fn create(core: CoreHandle) -> anyhow::Result<Tray> {
        let shared = Rc::new(Shared::default());
        let handler_shared = shared.clone();
        let handler_core = core;
        let icon = NotifyIcon::new(
            APP_NAME,
            tray_images(false),
            Box::new(move |event| {
                crate::ui::guard("tray event", || on_icon_event(&handler_shared, &handler_core, event));
            }),
        )
        .map_err(|e| anyhow::anyhow!("could not create the tray icon: {e}"))?;
        shared.hwnd.set(icon.hwnd());
        // The active variant off the UI thread, so the first read's flip only swaps bitmaps.
        if let Err(e) = std::thread::Builder::new().name("vox-tray-icons".into()).spawn(|| drop(tray_images(true))) {
            tracing::debug!(target: "tray", "active tray icons are rendered on first use: {e}");
        }
        Ok(Tray { shown: None, icon: Some(icon), shared, active: Some(false) })
    }

    /// Applies a desired state (cheap when unchanged).
    pub fn update(&mut self, view: TrayView) {
        let Some(icon) = self.icon.as_ref() else { return };
        if self.shown.as_ref() == Some(&view) {
            return;
        }
        if self.active != Some(view.active) {
            icon.set_images(tray_images(view.active));
            self.active = Some(view.active);
        }
        icon.set_tooltip(&view.tooltip);
        let key = view.menu_key();
        let stale = self.shared.menu.borrow().as_ref().map(|m| m.key != key).unwrap_or(true);
        if stale {
            match build_menu(&view, true) {
                Ok(built) => *self.shared.menu.borrow_mut() = Some(built),
                Err(e) => {
                    tracing::warn!(target: "tray", "tray menu with accelerator hints failed, rebuilding without: {e}");
                    match build_menu(&view, false) {
                        Ok(built) => *self.shared.menu.borrow_mut() = Some(built),
                        Err(e) => tracing::warn!(target: "tray", "tray menu could not be built: {e}"),
                    }
                }
            }
        }
        self.shown = Some(view);
    }

    /// Displays / DPI changed: re-pick the icon size.
    pub fn refresh_dpi(&mut self) {
        if let Some(icon) = self.icon.as_ref() {
            icon.refresh_dpi();
        }
    }

    /// Removes the icon (end of the quit sequence). Idempotent; later updates do nothing.
    pub fn destroy(&mut self) {
        if let Some(mut icon) = self.icon.take() {
            icon.destroy();
        }
        self.shared.menu.borrow_mut().take();
        self.shared.hwnd.set(0);
        self.shown = None;
        self.active = None;
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// The icon's events (UI thread, no borrow of `AppUi` held).
fn on_icon_event(shared: &Rc<Shared>, core: &CoreHandle, event: NotifyIconEvent) {
    match event {
        NotifyIconEvent::Activate => crate::ui::tray_open(None),
        NotifyIconEvent::ContextMenu { x, y } => {
            // Take what we need out of the cell first: an update may rebuild the menu while it is open (the menu loop
            // is modal and keeps dispatching messages); the clone keeps this one alive meanwhile.
            let (menu, actions) = match shared.menu.borrow().as_ref() {
                Some(m) => (m.menu.clone(), m.actions.clone()),
                None => return,
            };
            let hwnd = shared.hwnd.get();
            if hwnd == 0 {
                return;
            }
            // Drop anything stale from muda's channel so only this menu's selection is handled.
            while muda::MenuEvent::receiver().try_recv().is_ok() {}
            // The tray's hidden window is a zero-sized popup at the screen origin, so client = screen coordinates.
            let position = muda::dpi::Position::Physical(muda::dpi::PhysicalPosition::new(x, y));
            // SAFETY: `hwnd` is the live hidden window of our `NotifyIcon` (cleared on destroy).
            let selected = unsafe { menu.show_context_menu_for_hwnd(hwnd, Some(position)) };
            crate::windowing::native::post_null(hwnd);
            if !selected {
                return;
            }
            let mut chosen = None;
            while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
                if let Some((_, action)) = actions.iter().find(|(id, _)| id == event.id()) {
                    chosen = Some(*action);
                }
            }
            if let Some(action) = chosen {
                run_action(core, action);
            }
        }
    }
}

/// What a menu item does (Open / OpenPage / Quit stay on the UI thread).
fn run_action(core: &CoreHandle, action: TrayAction) {
    match action {
        TrayAction::Open => crate::ui::tray_open(None),
        TrayAction::OpenPage(page) => crate::ui::tray_open(Some(vox_core::app::AppRoute::page(page))),
        TrayAction::Quit => crate::ui::tray_quit(),
        other => core.send(Command::Tray(other)),
    }
}

/// Builds the menu of shell.md §7.3 for a view (pure apart from muda).
fn build_menu(view: &TrayView, hints: bool) -> muda::Result<BuiltMenu> {
    let hint = |h: &Option<String>| if hints { h.clone() } else { None };
    let menu = Menu::new();
    let mut actions = Vec::new();
    let mut item = |label: String, enabled: bool, action: TrayAction| -> muda::Result<()> {
        let it = MenuItem::new(label, enabled, None);
        actions.push((it.id().clone(), action));
        menu.append(&it)
    };
    item(with_hint("Read clipboard", hint(&view.hint_read_clipboard).as_deref()), true, TrayAction::ReadClipboard)?;
    item(with_hint(view.pause_label(), hint(&view.hint_play_pause).as_deref()), view.pause_enabled(), TrayAction::TogglePause)?;
    item(with_hint("Stop", hint(&view.hint_stop).as_deref()), view.stop_enabled(), TrayAction::Stop)?;
    menu.append(&PredefinedMenuItem::separator())?;
    item(format!("Open {APP_NAME}"), true, TrayAction::Open)?;
    item("History".into(), true, TrayAction::OpenPage(AppPage::History))?;
    item("Analytics".into(), true, TrayAction::OpenPage(AppPage::Analytics))?;
    item("Settings".into(), true, TrayAction::OpenPage(AppPage::Settings))?;
    menu.append(&PredefinedMenuItem::separator())?;
    let checks = [
        ("Right-click chip", view.right_click_chip, TrayAction::SetRightClickChip(!view.right_click_chip)),
        ("Floating widget", view.widget_enabled, TrayAction::SetWidgetEnabled(!view.widget_enabled)),
        ("Hotkeys", view.hotkeys_enabled, TrayAction::SetHotkeysEnabled(!view.hotkeys_enabled)),
    ];
    for (label, checked, action) in checks {
        let it = CheckMenuItem::new(label, true, checked, None);
        actions.push((it.id().clone(), action));
        menu.append(&it)?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let quit = MenuItem::new(format!("Quit {APP_NAME}"), true, None);
    actions.push((quit.id().clone(), TrayAction::Quit));
    menu.append(&quit)?;
    Ok(BuiltMenu { menu, key: view.menu_key(), actions })
}

/// The notification-area bitmaps for a state (rendered once per state and process, then cloned).
pub fn tray_images(active: bool) -> Vec<TrayIconImage> {
    static IDLE: OnceLock<Vec<TrayIconImage>> = OnceLock::new();
    static ACTIVE: OnceLock<Vec<TrayIconImage>> = OnceLock::new();
    let cell = if active { &ACTIVE } else { &IDLE };
    cell.get_or_init(|| render_tray_images(active)).clone()
}

fn render_tray_images(active: bool) -> Vec<TrayIconImage> {
    TRAY_SIZES.iter().map(|&size| TrayIconImage { size, rgba: mark::render(size, mark::Style::Tray { active }) }).collect()
}

/// The application icon (the mark on its dark tile) as straight RGBA8, `size × size` (window icon, assets).
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    mark::render(size, mark::Style::Tile)
}

/// The "V" wave mark (DESIGN.md "Tray"), drawn with signed distances and 4×4 supersampling.
pub mod mark {
    /// What to draw.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Style {
        /// White mark on transparent with a faint dark halo; `active` adds the dot.
        Tray { active: bool },
        /// The app icon: the white mark on a dark rounded tile (DESIGN.md panel colour and hairline).
        Tile,
    }

    type P = (f32, f32);

    /// The mark's centre line in unit coordinates: two mirrored cubic arms meeting in a rounded bottom.
    fn centre_line() -> Vec<P> {
        let left = [(0.17, 0.24), (0.27, 0.24), (0.42, 0.80), (0.50, 0.80)];
        let mut pts = Vec::new();
        const N: usize = 40;
        for i in 0..=N {
            let t = i as f32 / N as f32;
            pts.push(cubic(left, t));
        }
        for i in (0..N).rev() {
            let (x, y) = pts[i];
            pts.push((1.0 - x, y));
        }
        pts
    }

    fn cubic(p: [P; 4], t: f32) -> P {
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        (a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0, a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1)
    }

    fn seg_dist(p: P, a: P, b: P) -> f32 {
        let (abx, aby) = (b.0 - a.0, b.1 - a.1);
        let (apx, apy) = (p.0 - a.0, p.1 - a.1);
        let len2 = abx * abx + aby * aby;
        let t = if len2 > 0.0 { ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0) } else { 0.0 };
        let (dx, dy) = (apx - t * abx, apy - t * aby);
        (dx * dx + dy * dy).sqrt()
    }

    fn polyline_dist(p: P, pts: &[P]) -> f32 {
        pts.windows(2).map(|w| seg_dist(p, w[0], w[1])).fold(f32::INFINITY, f32::min)
    }

    /// Signed distance of a rounded box centred at `c` with half extents `h` and radius `r`.
    fn round_box(p: P, c: P, h: P, r: f32) -> f32 {
        let qx = (p.0 - c.0).abs() - (h.0 - r);
        let qy = (p.1 - c.1).abs() - (h.1 - r);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - r
    }

    /// Premultiplied float canvas.
    struct Canvas {
        size: u32,
        px: Vec<[f32; 4]>,
        /// Skip the supersampling of pixels clearly inside or outside ([`Canvas::coverage`]); off only in the test that
        /// proves the shortcut changes nothing.
        shortcut: bool,
    }

    impl Canvas {
        fn new(size: u32, shortcut: bool) -> Self {
            Canvas { size, px: vec![[0.0; 4]; (size * size) as usize], shortcut }
        }
        /// Coverage of `inside(p) <= 0` per pixel with 4×4 samples (pixel units). Every `sdf` here is a true distance
        /// (1-Lipschitz), so a pixel whose centre is at least `EXACT_MARGIN` from the edge has all 16 samples fully
        /// outside (or inside): one evaluation gives the 0 (or 1) the samples would sum to.
        fn coverage(&self, sdf: &dyn Fn(P) -> f32) -> Vec<f32> {
            const SS: usize = 4;
            /// The farthest sample from the pixel centre (≈ 0.53 px) plus the half-sample soft edge (0.125 px).
            const EXACT_MARGIN: f32 = 0.7;
            let n = self.size as usize;
            let mut out = vec![0.0; n * n];
            for y in 0..n {
                for x in 0..n {
                    if self.shortcut {
                        let centre = sdf((x as f32 + 0.5, y as f32 + 0.5));
                        if centre >= EXACT_MARGIN {
                            continue;
                        }
                        if centre <= -EXACT_MARGIN {
                            out[y * n + x] = 1.0;
                            continue;
                        }
                    }
                    let mut hits = 0.0;
                    for sy in 0..SS {
                        for sx in 0..SS {
                            let p = (x as f32 + (sx as f32 + 0.5) / SS as f32, y as f32 + (sy as f32 + 0.5) / SS as f32);
                            let d = sdf(p);
                            // A sample half a sub-pixel away from the edge counts half: smoother small sizes.
                            hits += (0.5 - d * SS as f32).clamp(0.0, 1.0);
                        }
                    }
                    out[y * n + x] = hits / (SS * SS) as f32;
                }
            }
            out
        }
        /// Source-over with a straight colour and per-pixel coverage.
        fn fill(&mut self, cov: &[f32], rgb: [f32; 3], alpha: f32) {
            for (d, &c) in self.px.iter_mut().zip(cov) {
                let a = (c * alpha).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                for (v, c) in d.iter_mut().zip(rgb) {
                    *v = c * a + *v * (1.0 - a);
                }
                d[3] = a + d[3] * (1.0 - a);
            }
        }
        /// Clears where `cov` is set (destination-out).
        fn erase(&mut self, cov: &[f32]) {
            for (d, &c) in self.px.iter_mut().zip(cov) {
                let keep = (1.0 - c).clamp(0.0, 1.0);
                for v in d.iter_mut() {
                    *v *= keep;
                }
            }
        }
        /// Straight RGBA8.
        fn into_rgba(self) -> Vec<u8> {
            let mut out = Vec::with_capacity(self.px.len() * 4);
            for p in self.px {
                let a = p[3].clamp(0.0, 1.0);
                let un = |v: f32| if a > 0.0 { (v / a).clamp(0.0, 1.0) } else { 0.0 };
                let q = |v: f32| (v * 255.0).round() as u8;
                out.extend_from_slice(&[q(un(p[0])), q(un(p[1])), q(un(p[2])), q(a)]);
            }
            out
        }
    }

    fn hex(rgb: u32) -> [f32; 3] {
        [((rgb >> 16) & 0xff) as f32 / 255.0, ((rgb >> 8) & 0xff) as f32 / 255.0, (rgb & 0xff) as f32 / 255.0]
    }

    /// Renders the mark at `size` px (straight RGBA8, `size × size × 4` bytes).
    pub fn render(size: u32, style: Style) -> Vec<u8> {
        render_with(size, style, true)
    }

    /// [`render`] with the coverage shortcut on or off (the test compares both).
    pub(super) fn render_with(size: u32, style: Style, shortcut: bool) -> Vec<u8> {
        let size = size.max(1);
        let s = size as f32;
        let mut canvas = Canvas::new(size, shortcut);
        let line = centre_line();
        match style {
            Style::Tray { active } => {
                let to_px: Vec<P> = line.iter().map(|&(x, y)| (x * s, y * s)).collect();
                let half = (0.078 * s).max(1.2);
                let halo = canvas.coverage(&|p| polyline_dist(p, &to_px) - (half + 1.0));
                let body = canvas.coverage(&|p| polyline_dist(p, &to_px) - half);
                canvas.fill(&halo, [0.0; 3], 0.30);
                canvas.fill(&body, [1.0; 3], 1.0);
                if active {
                    let c = (0.80 * s, 0.78 * s);
                    let r = (0.13 * s).max(2.0);
                    let gap = (0.07 * s).max(1.0);
                    let dist = |p: P| ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt();
                    let ring = canvas.coverage(&|p| dist(p) - (r + gap));
                    canvas.erase(&ring);
                    let dot_halo = canvas.coverage(&|p| dist(p) - (r + 1.0));
                    let dot = canvas.coverage(&|p| dist(p) - r);
                    canvas.fill(&dot_halo, [0.0; 3], 0.30);
                    canvas.fill(&dot, [1.0; 3], 1.0);
                }
            }
            Style::Tile => {
                let inset = 0.15;
                let scale = 1.0 - 2.0 * inset;
                let to_px: Vec<P> = line.iter().map(|&(x, y)| ((inset + x * scale) * s, (inset + y * scale + 0.01) * s)).collect();
                let hair = (s / 128.0).max(1.0);
                let c = (s / 2.0, s / 2.0);
                let radius = 0.22 * s;
                let outer = canvas.coverage(&|p| round_box(p, c, (s / 2.0, s / 2.0), radius));
                let inner = canvas.coverage(&|p| round_box(p, c, (s / 2.0 - hair, s / 2.0 - hair), (radius - hair).max(0.0)));
                canvas.fill(&outer, hex(0x262a2d), 1.0);
                canvas.fill(&inner, hex(0x1a1a1a), 1.0);
                let half = (0.066 * s).max(1.0);
                let body = canvas.coverage(&|p| polyline_dist(p, &to_px) - half);
                canvas.fill(&body, hex(0xf2f2f2), 1.0);
            }
        }
        canvas.into_rgba()
    }
}

/// `.ico` writer for the generator test: 32-bit DIB entries below 256 px, PNG for 256 px.
#[cfg(test)]
fn encode_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    fn dib(size: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let w = size as i32;
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&(w * 2).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        let mask_row = size.div_ceil(32) * 4;
        let image_bytes = size * size * 4 + mask_row * size;
        out.extend_from_slice(&image_bytes.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        for y in (0..size).rev() {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        // AND mask: all zero (the alpha channel decides).
        out.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
        out
    }
    let blobs: Vec<Vec<u8>> = images
        .iter()
        .map(|(size, rgba)| {
            if *size >= 256 {
                vox_art::Art { width: *size, height: *size, rgba: rgba.clone() }.encode_png().expect("png encoding")
            } else {
                dib(*size, rgba)
            }
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for ((size, _), blob) in images.iter().zip(&blobs) {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += blob.len() as u32;
    }
    for blob in blobs {
        out.extend_from_slice(&blob);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::player::{PlayerState, PlayerStatus};
    use vox_core::settings::Settings;

    fn alpha_at(rgba: &[u8], size: u32, x: u32, y: u32) -> u8 {
        rgba[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_mark_is_a_white_symmetric_v() {
        for size in TRAY_SIZES {
            let img = mark::render(size, mark::Style::Tray { active: false });
            assert_eq!(img.len(), (size * size * 4) as usize);
            // Something is drawn, the corners stay transparent.
            assert!(img.chunks(4).any(|p| p[3] == 255 && p[0] == 255 && p[1] == 255 && p[2] == 255), "{size}");
            assert_eq!(alpha_at(&img, size, 0, size - 1), 0);
            assert_eq!(alpha_at(&img, size, size - 1, size - 1), 0);
            // Mirror symmetry of the coverage (the mark is drawn from mirrored arms).
            for y in 0..size {
                for x in 0..size / 2 {
                    let a = alpha_at(&img, size, x, y) as i32;
                    let b = alpha_at(&img, size, size - 1 - x, y) as i32;
                    assert!((a - b).abs() <= 8, "size {size} at ({x},{y}): {a} vs {b}");
                }
            }
        }
    }

    #[test]
    fn tray_images_are_cached_per_state() {
        let a = tray_images(true);
        assert_eq!(a, tray_images(true));
        assert_eq!(a, render_tray_images(true), "the cache holds exactly the rendered bitmaps");
        assert_ne!(tray_images(false), a);
    }

    #[test]
    fn the_coverage_shortcut_changes_no_pixel() {
        for size in TRAY_SIZES {
            for style in [mark::Style::Tray { active: false }, mark::Style::Tray { active: true }, mark::Style::Tile] {
                assert_eq!(mark::render_with(size, style, true), mark::render_with(size, style, false), "{size} {style:?}");
            }
        }
        assert_eq!(mark::render_with(64, mark::Style::Tile, true), mark::render_with(64, mark::Style::Tile, false));
    }

    #[test]
    fn the_active_variant_adds_the_dot() {
        let size = 32;
        let idle = mark::render(size, mark::Style::Tray { active: false });
        let active = mark::render(size, mark::Style::Tray { active: true });
        assert_ne!(idle, active);
        // The dot's centre is solid white only in the active variant.
        let (cx, cy) = ((0.80 * size as f32) as u32, (0.78 * size as f32) as u32);
        assert_eq!(alpha_at(&active, size, cx, cy), 255);
        assert!(alpha_at(&idle, size, cx, cy) < 255);
        let images = tray_images(true);
        assert_eq!(images.iter().map(|i| i.size).collect::<Vec<_>>(), TRAY_SIZES.to_vec());
        assert!(images.iter().all(|i| i.rgba.len() == (i.size * i.size * 4) as usize));
    }

    #[test]
    fn the_app_tile_is_opaque_inside_with_rounded_corners() {
        let size = 64;
        let img = app_icon_rgba(size);
        assert_eq!(alpha_at(&img, size, 0, 0), 0, "rounded corner");
        assert_eq!(alpha_at(&img, size, size / 2, size / 8), 255);
        let centre = ((size / 2 * size + size / 2) * 4) as usize;
        assert!(img[centre + 3] == 255);
    }

    #[test]
    fn ico_layout() {
        let images: Vec<(u32, Vec<u8>)> = [16u32, 256].iter().map(|&s| (s, app_icon_rgba(s))).collect();
        let ico = encode_ico(&images);
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 2, 0]);
        // First entry: 16×16, 32 bpp DIB of 40 + 16·16·4 + 16·4 bytes at offset 6 + 32.
        assert_eq!(&ico[6..10], &[16, 16, 0, 0]);
        let len16 = u32::from_le_bytes(ico[14..18].try_into().unwrap());
        assert_eq!(len16, 40 + 16 * 16 * 4 + 16 * 4);
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 38);
        // Second entry: 256 px is stored as PNG (width/height byte 0).
        assert_eq!(&ico[22..24], &[0, 0]);
        let off = u32::from_le_bytes(ico[34..38].try_into().unwrap()) as usize;
        assert_eq!(&ico[off..off + 8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn menu_items_follow_the_view() {
        let mut s = Settings::default();
        s.triggers.hotkeys.read_clipboard = "Ctrl+Alt+V".into();
        let state = PlayerState { status: PlayerStatus::Paused, ..PlayerState::idle() };
        let view = TrayView::build(&s, &state);
        let built = build_menu(&view, true).expect("menu");
        let actions: Vec<TrayAction> = built.actions.iter().map(|(_, a)| *a).collect();
        assert_eq!(
            actions,
            vec![
                TrayAction::ReadClipboard,
                TrayAction::TogglePause,
                TrayAction::Stop,
                TrayAction::Open,
                TrayAction::OpenPage(AppPage::History),
                TrayAction::OpenPage(AppPage::Analytics),
                TrayAction::OpenPage(AppPage::Settings),
                TrayAction::SetRightClickChip(!s.triggers.right_click_chip),
                TrayAction::SetWidgetEnabled(!s.widget.enabled),
                TrayAction::SetHotkeysEnabled(!s.triggers.hotkeys_enabled),
                TrayAction::Quit,
            ]
        );
        assert_eq!(built.key, view.menu_key());
        let items = built.menu.items();
        let label = |i: usize| items[i].as_menuitem().map(|m| m.text()).unwrap_or_default();
        // muda reports the label before the tab; the Win32 item text carries the `\t` hint after it.
        assert_eq!(label(0), "Read clipboard");
        assert_eq!(label(1), "Resume");
        assert_eq!(label(4), "Open Vox");
        assert_eq!(label(13), "Quit Vox");
        assert_eq!(items.len(), 14);
        assert!(items[3].as_predefined_menuitem().is_some());
        let no_hints = build_menu(&view, false).expect("menu");
        assert_eq!(no_hints.menu.items().len(), 14);
    }

    /// Renders the tray icons (both states) and the app tile at 8× nearest-neighbour into the temp folder for a
    /// visual review (`cargo test -p vox-app -- --ignored preview_icons`).
    #[test]
    #[ignore]
    fn preview_icons() {
        let dir = std::env::temp_dir().join("vox-icon-preview");
        std::fs::create_dir_all(&dir).unwrap();
        let mut list = vec![("tile-32".to_string(), 32, app_icon_rgba(32))];
        for size in TRAY_SIZES {
            for active in [false, true] {
                list.push((format!("tray-{size}-{}", if active { "active" } else { "idle" }), size, mark::render(size, mark::Style::Tray { active })));
            }
        }
        for (name, size, rgba) in list {
            let big = size * 8;
            let mut out = vec![0u8; (big * big * 4) as usize];
            for y in 0..big {
                for x in 0..big {
                    let src = (((y / 8) * size + x / 8) * 4) as usize;
                    // On a mid-grey background, so the halo and the transparent parts are visible.
                    let a = rgba[src + 3] as u32;
                    let dst = ((y * big + x) * 4) as usize;
                    for c in 0..3 {
                        out[dst + c] = ((rgba[src + c] as u32 * a + 0x80 * (255 - a)) / 255) as u8;
                    }
                    out[dst + 3] = 255;
                }
            }
            vox_art::Art { width: big, height: big, rgba: out }.save_png(&dir.join(format!("{name}.png"))).unwrap();
        }
    }

    /// Writes `assets/vox.ico` and `assets/vox-256.png` (run once with `cargo test -p vox-app -- --ignored
    /// generate_app_icon_assets`; the files are checked in and embedded by build.rs).
    #[test]
    #[ignore]
    fn generate_app_icon_assets() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        std::fs::create_dir_all(&dir).unwrap();
        let images: Vec<(u32, Vec<u8>)> = APP_ICON_SIZES.iter().map(|&s| (s, app_icon_rgba(s))).collect();
        std::fs::write(dir.join("vox.ico"), encode_ico(&images)).unwrap();
        let big = images.iter().find(|(s, _)| *s == 256).unwrap();
        vox_art::Art { width: 256, height: 256, rgba: big.1.clone() }.save_png(&dir.join("vox-256.png")).unwrap();
    }
}
