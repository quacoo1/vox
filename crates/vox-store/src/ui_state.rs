//! Small per-user UI state that is not part of the settings schema (ui-main.md §5.1: the Analytics range was kept in
//! browser `localStorage`). File: `<profile>\ui-state.json`, pretty JSON `{"analyticsRange":"30d"}`; any read/write
//! failure is ignored (defaults).

use serde_json::{json, Value};
use vox_core::analytics::AnalyticsRange;
use vox_core::json::to_js_pretty;

use crate::fsutil::{read_json_file, write_file_atomic, JsonRead};
use crate::paths::Profile;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiState {
    pub analytics_range: AnalyticsRange,
}

/// Parses a loaded `ui-state.json` value; unknown or invalid fields take their defaults.
fn parse_ui_state(v: &Value) -> UiState {
    let analytics_range = v.get("analyticsRange").and_then(Value::as_str).and_then(AnalyticsRange::parse).unwrap_or_default();
    UiState { analytics_range }
}

/// Loads `ui-state.json` (missing/invalid → defaults).
pub fn load_ui_state(profile: &Profile) -> UiState {
    match read_json_file(&profile.ui_state_file()) {
        JsonRead::Ok(v) => parse_ui_state(&v),
        JsonRead::Missing => UiState::default(),
        JsonRead::Corrupt(e) => {
            tracing::debug!(target: "data", "ignoring an unreadable ui-state.json: {e}");
            UiState::default()
        }
    }
}

/// Saves `ui-state.json` atomically (errors ignored).
pub fn save_ui_state(profile: &Profile, state: &UiState) {
    let text = to_js_pretty(&json!({ "analyticsRange": state.analytics_range.as_str() }));
    if let Err(e) = write_file_atomic(&profile.ui_state_file(), text.as_bytes(), false) {
        tracing::debug!(target: "data", "could not save ui-state.json: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        assert_eq!(load_ui_state(&p), UiState { analytics_range: AnalyticsRange::D30 });
        save_ui_state(&p, &UiState { analytics_range: AnalyticsRange::D365 });
        assert_eq!(fs::read_to_string(p.ui_state_file()).unwrap(), "{\n  \"analyticsRange\": \"365d\"\n}");
        assert_eq!(load_ui_state(&p).analytics_range, AnalyticsRange::D365);
        save_ui_state(&p, &UiState { analytics_range: AnalyticsRange::All });
        assert_eq!(load_ui_state(&p).analytics_range, AnalyticsRange::All);
        for bad in ["", "[]", "{\"analyticsRange\":\"1y\"}", "{\"analyticsRange\":7}", "nope"] {
            fs::write(p.ui_state_file(), bad).unwrap();
            assert_eq!(load_ui_state(&p), UiState::default(), "{bad}");
        }
        fs::write(p.ui_state_file(), "\u{FEFF}{\"analyticsRange\":\"7d\",\"other\":1}").unwrap();
        assert_eq!(load_ui_state(&p).analytics_range, AnalyticsRange::D7);
    }

    #[test]
    fn save_errors_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("blocker"), b"x").unwrap();
        let p = Profile::at(dir.path().join("blocker"), false);
        save_ui_state(&p, &UiState::default());
        assert_eq!(load_ui_state(&p), UiState::default());
    }
}
