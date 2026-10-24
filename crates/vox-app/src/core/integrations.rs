//! Explorer verb and launch-at-login glue (shell.md §4.3–§4.4, integrations.md §3–§4). Skipped in demo mode.
//!
//! - `sync_explorer_menu(enabled, force)`: expected command = `vox_win::registry::build_verb_command(exe, [])`, icon
//!   `"<exe>",0` (packaged and dev, D-INT-7); not forced (startup): compare `explorer_menu_state(expected)` with the
//!   setting and rewrite only when stale; forced (setting toggled): always. Failure → error toast
//!   `copy::explorer_menu_failed(err)` (no action). Always followed by `UiUpdate::AppInfo` (`explorer_menu_installed`
//!   freshly computed), also when skipped in demo mode.
//! - `apply_launch_at_login(settings)`: value name `Vox` (packaged) / `Vox (dev)`, args `--hidden` when
//!   `startMinimized`; errors only logged; followed by `UiUpdate::AppInfo`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use vox_core::app::Toast;
use vox_win::registry::{ExplorerMenuSpec, MenuState};

use super::Core;

/// Decides whether the startup sync must write (pure).
pub fn explorer_needs_write(enabled: bool, state: vox_win::registry::MenuState) -> bool {
    use vox_win::registry::MenuState::*;
    !matches!((enabled, state), (true, Current) | (false, Absent))
}

/// The verb's install parameters for this executable (D-INT-7: the exe's own first icon, packaged and dev).
pub fn explorer_spec(exe: &Path) -> ExplorerMenuSpec {
    let exe = exe.to_string_lossy();
    ExplorerMenuSpec {
        command: vox_win::registry::build_verb_command(&exe, &[]),
        label: vox_core::constants::READ_WITH_LABEL.to_string(),
        icon_path: Some(format!("\"{exe}\",0")),
    }
}

/// The `Run` value name: `Vox` for packaged builds, `Vox (dev)` otherwise (dev and installed builds never overwrite
/// each other's entry).
pub fn run_value_name(is_packaged: bool) -> String {
    if is_packaged {
        vox_core::constants::APP_NAME.to_string()
    } else {
        format!("{} (dev)", vox_core::constants::APP_NAME)
    }
}

fn state_name(state: MenuState) -> &'static str {
    match state {
        MenuState::Absent => "absent",
        MenuState::Stale => "stale",
        MenuState::Current => "current",
    }
}

impl Core {
    /// `syncExplorerMenu(enabled, force)` (module docs); `AppInfo` follows in every case.
    pub(super) fn sync_explorer_menu(&mut self, force: bool) {
        if !self.demo() {
            let enabled = self.settings().triggers.explorer_menu;
            let spec = explorer_spec(&self.services.runtime.exe_path);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if !force {
                    let state = vox_win::registry::explorer_menu_state(&spec.command);
                    if !explorer_needs_write(enabled, state) {
                        return Ok(false);
                    }
                    tracing::info!(
                        target: "integrations",
                        "explorer menu is {}; {} it",
                        state_name(state),
                        if enabled { "installing" } else { "removing" }
                    );
                }
                vox_win::registry::set_explorer_menu(enabled, &spec).map(|()| true)
            }));
            match outcome {
                Ok(Ok(true)) => tracing::info!(target: "integrations", "explorer menu {}", if enabled { "installed" } else { "removed" }),
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(target: "integrations", "explorer menu update failed: {e}");
                    self.toast(Toast::error(vox_core::copy::explorer_menu_failed(&e)));
                }
                Err(_) => tracing::warn!(target: "integrations", "explorer menu sync failed"),
            }
        }
        self.publish_app_info();
    }

    /// `applyLaunchAtLogin` (module docs); errors are only logged; `AppInfo` follows.
    pub(super) fn apply_launch_at_login(&mut self) {
        if !self.demo() {
            let app = self.settings().app;
            let runtime = &self.services.runtime;
            let name = run_value_name(runtime.is_packaged);
            let exe = runtime.exe_path.clone();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                vox_win::registry::apply_launch_at_login(app.launch_at_login, app.start_minimized, &name, &exe)
            }));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(target: "integrations", "could not update the login item: {e}"),
                Err(_) => tracing::warn!(target: "integrations", "applyLaunchAtLogin failed"),
            }
        }
        self.publish_app_info();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_sync_writes_only_when_stale() {
        assert!(!explorer_needs_write(true, MenuState::Current));
        assert!(explorer_needs_write(true, MenuState::Stale));
        assert!(explorer_needs_write(true, MenuState::Absent));
        assert!(!explorer_needs_write(false, MenuState::Absent));
        assert!(explorer_needs_write(false, MenuState::Stale));
        assert!(explorer_needs_write(false, MenuState::Current));
    }

    #[test]
    fn verb_spec_and_run_value_name() {
        let spec = explorer_spec(Path::new(r"C:\Program Files\Vox\vox.exe"));
        assert_eq!(spec.command, r#""C:\Program Files\Vox\vox.exe" --read-file "%1""#);
        assert_eq!(spec.label, "Read with Vox");
        assert_eq!(spec.icon_path.as_deref(), Some(r#""C:\Program Files\Vox\vox.exe",0"#));
        let spec = explorer_spec(Path::new(r"C:\Users\K\Vox\vox.exe"));
        assert_eq!(spec.command, r#"C:\Users\K\Vox\vox.exe --read-file "%1""#, "unquoted without whitespace");
        assert_eq!(run_value_name(true), "Vox");
        assert_eq!(run_value_name(false), "Vox (dev)");
    }
}
