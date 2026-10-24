//! Native treatment of the three Slint windows (shell.md §6; BRIEF.md "Windows"; ARCHITECTURE.md §3.4). Runs on the
//! UI thread.
//!
//! - **Before any window exists**, `app::run` installs the backend with [`install_backend`]
//!   (`slint::BackendSelector::new().backend_name("winit").with_winit_window_attributes_hook(..)`). The hook
//!   ([`window_attributes`]) matches the window by `attributes.title`: `Vox Player` and `Vox Chip` get
//!   `.with_active(false)` (winit then shows them with `SW_SHOWNOACTIVATE`, also on every later Slint `show()`),
//!   `.with_skip_taskbar(true)` (`WindowAttributesExtWindows`), `.with_window_level(AlwaysOnTop)`,
//!   `.with_resizable(false)`, `.with_enabled_buttons(empty)` and an off-screen `.with_position`; the main window gets
//!   `.with_theme(Some(Dark | Light))`, `.with_position` / `.with_inner_size` from the saved placement and
//!   `.with_maximized(..)`, so its first show already has the right title bar and geometry.
//!
//!   **Implementation note (Slint 1.18.1):** the hook runs inside `create_window_adapter`, i.e. while the component's
//!   `new()` runs, and at that moment `attributes.title` is still Slint's placeholder (the program name) — the
//!   component's own `title` reaches the window only later. So the hook cannot tell the windows apart by title alone:
//!   `AppUi::create` creates each window inside [`creating`] (a thread-local "which window is being created"), and the
//!   hook uses that, falling back to the title (`Vox` / `Vox Player` / `Vox Chip`) when it is ever set in time.
//!   `with_maximized` is **not** applied through the hook: winit maximizes a hidden window with `ShowWindow(
//!   SW_MAXIMIZE)` (which shows and activates it for a moment); the saved maximized state is applied by
//!   `main_window::show` on the first show instead. The saved outer rectangle is applied exactly with
//!   `SetWindowPlacement` once the HWND exists (`main_window::on_created`).
//! - **Overlays are styled before anyone can see them**: `AppUi::create` pre-creates both overlays (show at
//!   -32000,-32000 then hide, from a `Timer::single_shot(0)`), reads each HWND ([`hwnd_of`]) and installs
//!   `vox_win::window::install_overlay_subclass` (which enforces `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`
//!   against winit's style rewrites, swallows the system menu and turns `WM_CLOSE` into
//!   `WindowEvent::CloseRequested(kind)`); only then `AppUi::overlays_ready = true`. Visibility always goes through
//!   Slint (`show()` / `hide()`); after a show `vox_win::window::assert_topmost(hwnd, raise)` re-asserts the z-order.
//!   Chip click-through: winit `Window::set_cursor_hittest(false/true)` via `WinitWindowAccessor`.
//! - Close requests: `window.on_close_requested` on all three windows always returns `KeepWindowShown`
//!   (`ui` module docs).
//! - Geometry: `vox_core::geometry` in DIP of the target monitor; [`dpi`] converts with that monitor's scale. A placement
//!   onto a monitor whose scale differs from the window's current one first moves the hidden window to the target work
//!   area's top-left (`SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER`), lets winit's `ScaleFactorChanged` settle, then
//!   computes and applies the final bounds once. `ScaleFactorChanged` on a visible widget/chip (seen by the window's
//!   event dispatcher, registered by `AppUi::style_native_windows`) is treated like `WidgetIntent::Reposition` for the
//!   widget; a visible chip is hidden (the next selection places it again).
//! - **First frame** ([`reveal`]): a hidden widget is shown parked off-screen at its final size and moved into place
//!   once Slint presented a frame at the new data and size, so the stale surface of the previous show never reaches
//!   the screen (cloaking does not work: DXGI then reports the window occluded and Slint renders nothing).
//! - **Per-pixel alpha (open issue)**: the Skia renderer draws through wgpu's DirectX 12 backend, whose swap chain for
//!   an HWND only offers opaque composition, so the overlays' transparent pixels (rounded corners, the card's enter
//!   fade) reach the screen black although `take_snapshot` shows them transparent. Presenting through a
//!   DirectComposition visual (`WGPU_DX12_PRESENTATION_SYSTEM=Visual`, with and without a redirection bitmap) was
//!   tried and measured with on-screen captures: the corners stayed black, so it is not used. Mitigation: both
//!   overlays are clipped to their rounded shape with a window region (`native::set_round_region`; aliased edge), so
//!   the corners show the desktop; translucency (the card's 0.96 panel alpha, the enter fade) still composes over
//!   black.
//! - **Outside `WM_CLOSE`**: `taskkill` without `/F` and installers post `WM_CLOSE` to the process's *visible*
//!   top-level windows, which is winit's zero-size "Winit Thread Event Target" window (the hidden system-events window
//!   never sees it, and winit would destroy its event target, cutting the UI off from the core). `AppUi` subclasses
//!   that window with the overlay subclass ([`native::thread_windows_of_class`]): `WM_CLOSE` is swallowed and becomes
//!   `Command::System(SystemEvent::CloseRequested)` (save, then quit).
//! - **Clocks**: UI deadlines (toast timeouts and exits, widget auto-hide and toast slot, drag and own-move grace, page
//!   wake marks) use [`mono_ms`] (monotonic since process start), so a wall-clock change never stalls or fires them;
//!   [`now_ms`] (the wall clock, `UiCtx::now_ms`) is only for what is shown or compared with stored times (relative
//!   dates; the Settings page's own chips and the hotkey recorder's hints, which it compares with `UiCtx::now_ms`).
//!
//! | Module | Spec |
//! |---|---|
//! | [`main_window`] | shell.md §6.2: creation with saved geometry, show/raise/focus, close → tray or quit, DWM caption colours (backdrop colour), the native event dispatcher (recorder, geometry, X1/X2), wake (focus/visible) |
//! | [`widget_window`] | shell.md §6.3: placement per `widget.position` from the monitor under the cursor, anchored resize in one `set_bounds`, drag via `drag_window` + end detection → `WindowEvent::WidgetDragged`, hover poll every 100 ms while visible |
//! | [`chip_window`] | shell.md §6.5: placement, raise above menus, click-through during the 140 ms exit, real bounds to the selection service |
//! | [`dpi`] | DIP ↔ physical per monitor |

pub mod chip_window;
pub mod dpi;
pub mod main_window;
pub mod native;
pub mod reveal;
pub mod widget_window;

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
use slint::winit_030::winit::window::{Theme, WindowAttributes, WindowButtons, WindowLevel};
use vox_win::window::PhysRect;

use crate::command::WindowKind;

/// Where overlays are created and parked while hidden (far off every monitor).
pub const OFFSCREEN: i32 = -32000;

/// What the attributes hook needs (captured by `app::run` before the backend is installed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttributesInit {
    /// Resolved main-window theme.
    pub dark: bool,
    /// The main window's normal rect and maximized flag (physical px).
    pub main_placement: (PhysRect, bool),
}

thread_local! {
    /// The window whose component is being created right now (module docs, "Implementation note").
    static CREATING: Cell<Option<WindowKind>> = const { Cell::new(None) };
}

/// Runs `f` (a component's `new()`) with `kind` announced to the attributes hook.
pub fn creating<R>(kind: WindowKind, f: impl FnOnce() -> R) -> R {
    let previous = CREATING.with(|c| c.replace(Some(kind)));
    let result = f();
    CREATING.with(|c| c.set(previous));
    result
}

/// Which window the attributes describe: the announced kind, else the title.
fn kind_of(attributes: &WindowAttributes) -> Option<WindowKind> {
    if let Some(kind) = CREATING.with(Cell::get) {
        return Some(kind);
    }
    match attributes.title.as_str() {
        "Vox Player" => Some(WindowKind::Widget),
        "Vox Chip" => Some(WindowKind::Chip),
        "Vox" => Some(WindowKind::Main),
        _ => None,
    }
}

/// The attributes hook body (module docs): matches by `attributes.title` (`Vox`, `Vox Player`, `Vox Chip`).
pub fn window_attributes(attributes: WindowAttributes, init: &AttributesInit) -> WindowAttributes {
    use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
    match kind_of(&attributes) {
        Some(WindowKind::Widget | WindowKind::Chip) => attributes
            .with_active(false)
            .with_skip_taskbar(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_resizable(false)
            .with_enabled_buttons(WindowButtons::empty())
            .with_decorations(false)
            .with_transparent(true)
            .with_undecorated_shadow(false)
            .with_position(PhysicalPosition::new(OFFSCREEN, OFFSCREEN)),
        Some(WindowKind::Main) => {
            let (rect, _maximized) = init.main_placement;
            let mut a = attributes.with_theme(Some(if init.dark { Theme::Dark } else { Theme::Light }));
            if rect.width > 0 && rect.height > 0 {
                a = a
                    .with_position(PhysicalPosition::new(rect.x, rect.y))
                    .with_inner_size(PhysicalSize::new(rect.width as u32, rect.height as u32));
            }
            a
        }
        None => attributes,
    }
}

/// Installs the winit backend with the attributes hook (must run before any Slint window is created).
pub fn install_backend(init: AttributesInit) -> anyhow::Result<()> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(move |attributes| window_attributes(attributes, &init))
        .select()
        .map_err(|e| anyhow::anyhow!("could not start the winit backend: {e}"))
}

/// The window's HWND once the native window exists.
pub fn hwnd_of(window: &slint::Window) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|w| match w.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
            _ => None,
        })
        .flatten()
}

/// Runs `f` with the winit window, if it exists (logs nothing; callers treat `None` as "not yet").
pub fn with_winit<R>(window: &slint::Window, f: impl FnOnce(&slint::winit_030::winit::window::Window) -> R) -> Option<R> {
    use slint::winit_030::WinitWindowAccessor;
    window.with_winit_window(f)
}

/// Milliseconds since the Unix epoch (the wall clock: relative times, `UiCtx::now_ms`; module docs "Clocks").
pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

/// Monotonic milliseconds since the first call (the clock of every UI deadline; module docs "Clocks"). Starts at 1 s so
/// a zero "moved by us" time is always long past.
pub fn mono_ms() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    1000.0 + start.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> AttributesInit {
        AttributesInit { dark: true, main_placement: (PhysRect { x: 10, y: 20, width: 1200, height: 800 }, false) }
    }

    #[test]
    fn the_hook_styles_by_the_announced_kind() {
        let a = creating(WindowKind::Widget, || window_attributes(WindowAttributes::default().with_title("vox"), &init()));
        assert!(!a.active);
        assert_eq!(a.window_level, WindowLevel::AlwaysOnTop);
        assert!(!a.resizable);
        assert_eq!(a.enabled_buttons, WindowButtons::empty());
        let main = creating(WindowKind::Main, || window_attributes(WindowAttributes::default(), &init()));
        assert!(main.active);
        assert_eq!(main.preferred_theme, Some(Theme::Dark));
        assert_eq!(main.window_level, WindowLevel::Normal);
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let a = mono_ms();
        let b = mono_ms();
        assert!(a >= 1000.0 && b >= a);
    }

    #[test]
    fn the_hook_falls_back_to_the_title() {
        let chip = window_attributes(WindowAttributes::default().with_title("Vox Chip"), &init());
        assert!(!chip.active);
        let other = window_attributes(WindowAttributes::default().with_title("something"), &init());
        assert!(other.active);
        assert_eq!(other.window_level, WindowLevel::Normal);
    }
}
