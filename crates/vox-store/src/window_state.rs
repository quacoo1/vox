//! `window-state.json` (new, Rust only; BRIEF.md Product rulings "Main window remembers its size, position and
//! maximized state"; ARCHITECTURE.md §6.5).
//!
//! Pretty JSON, written atomically without `.bak`:
//! ```json
//! { "version": 1, "x": 312, "y": 140, "width": 1770, "height": 1170, "scale": 1.5, "maximized": false }
//! ```
//! `x, y, width, height` = the main window's **restored** (non-maximized, non-minimized) outer rectangle in physical
//! virtual-screen pixels (Per-Monitor-V2), as `GetWindowPlacement().rcNormalPosition` converted to screen
//! coordinates; `scale` = DPI/96 of the monitor it was on when saved. The Electron build ignores the file.
//!
//! Read rules ([`parse_window_state`]): any read/parse problem, a non-object, a missing or non-finite number, a width
//! or height ≤ 0 or `scale` ≤ 0 → `None` (the app then uses the default 1180×780 DIP centred on the primary
//! monitor). Placement on screen (re-scaling across DPI changes, re-clamping into a work area) is vox-app's job
//! (`windowing::main_window`), not this module's. Fractional pixel values are rounded (`Math.round`); values outside
//! the `i32` range are invalid. `maximized` must be a JSON boolean to count; anything else reads as `false`. The
//! `version` field is written but not checked.

use serde::Serialize;
use serde_json::Value;
use vox_core::js::js_round;
use vox_core::json::{js_f64, to_js_pretty};

use crate::fsutil::{read_json_file, write_file_atomic, JsonRead};
use crate::paths::Profile;

/// The persisted main-window placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// DPI scale (1.0 = 96 DPI) of the monitor the window was on when saved.
    pub scale: f64,
    pub maximized: bool,
}

#[derive(Serialize)]
struct WindowStateJson {
    version: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    #[serde(serialize_with = "js_f64")]
    scale: f64,
    maximized: bool,
}

impl WindowState {
    /// Pretty JSON in the documented field order (`version` first).
    pub fn to_json(&self) -> String {
        to_js_pretty(&WindowStateJson {
            version: 1,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            scale: self.scale,
            maximized: self.maximized,
        })
    }
}

fn pixel(v: Option<&Value>) -> Option<i32> {
    let x = v?.as_f64().filter(|x| x.is_finite())?;
    let r = js_round(x);
    (r >= i32::MIN as f64 && r <= i32::MAX as f64).then_some(r as i32)
}

/// Validates a parsed `window-state.json` (see module docs). Pure.
pub fn parse_window_state(v: &Value) -> Option<WindowState> {
    let o = v.as_object()?;
    let x = pixel(o.get("x"))?;
    let y = pixel(o.get("y"))?;
    let width = pixel(o.get("width")).filter(|w| *w > 0)?;
    let height = pixel(o.get("height")).filter(|h| *h > 0)?;
    let scale = o.get("scale")?.as_f64().filter(|s| s.is_finite() && *s > 0.0)?;
    let maximized = o.get("maximized").and_then(Value::as_bool).unwrap_or(false);
    Some(WindowState { x, y, width, height, scale, maximized })
}

/// Loads `window-state.json` (missing / unreadable / invalid → `None`; never fails, logs at debug level).
pub fn load_window_state(profile: &Profile) -> Option<WindowState> {
    match read_json_file(&profile.window_state_file()) {
        JsonRead::Ok(v) => {
            let state = parse_window_state(&v);
            if state.is_none() {
                tracing::debug!(target: "data", "ignoring an invalid window-state.json");
            }
            state
        }
        JsonRead::Missing => None,
        JsonRead::Corrupt(e) => {
            tracing::debug!(target: "data", "ignoring an unreadable window-state.json: {e}");
            None
        }
    }
}

/// Saves atomically (`fsutil::write_file_atomic`, no `.bak`); errors are logged (scope `data`) and swallowed.
pub fn save_window_state(profile: &Profile, state: &WindowState) {
    let path = profile.window_state_file();
    if let Err(e) = write_file_atomic(&path, state.to_json().as_bytes(), false) {
        tracing::warn!(target: "data", "could not save {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn sample() -> WindowState {
        WindowState { x: 312, y: -140, width: 1770, height: 1170, scale: 1.5, maximized: false }
    }

    #[test]
    fn json_shape() {
        assert_eq!(
            sample().to_json(),
            "{\n  \"version\": 1,\n  \"x\": 312,\n  \"y\": -140,\n  \"width\": 1770,\n  \"height\": 1170,\n  \"scale\": 1.5,\n  \"maximized\": false\n}"
        );
        let whole = WindowState { scale: 1.0, maximized: true, ..sample() };
        assert!(whole.to_json().contains("\"scale\": 1,"));
    }

    #[test]
    fn parse_rules() {
        let ok = json!({"version":1,"x":312,"y":-140,"width":1770,"height":1170,"scale":1.5,"maximized":false});
        assert_eq!(parse_window_state(&ok), Some(sample()));
        let mut v = ok.clone();
        v["maximized"] = json!(true);
        assert!(parse_window_state(&v).unwrap().maximized);
        v.as_object_mut().unwrap().remove("maximized");
        assert!(!parse_window_state(&v).unwrap().maximized);
        v["maximized"] = json!("yes");
        assert!(!parse_window_state(&v).unwrap().maximized);
        let mut v = ok.clone();
        v["x"] = json!(311.5);
        assert_eq!(parse_window_state(&v).unwrap().x, 312);
        v.as_object_mut().unwrap().remove("version");
        assert!(parse_window_state(&v).is_some(), "version is not checked");
        for (field, bad) in [
            ("x", json!(null)),
            ("y", json!("1")),
            ("width", json!(0)),
            ("height", json!(-5)),
            ("scale", json!(0)),
            ("scale", json!(-1.5)),
            ("x", json!(1e12)),
            ("scale", json!("Infinity")),
        ] {
            let mut v = ok.clone();
            v[field] = bad.clone();
            assert_eq!(parse_window_state(&v), None, "{field} = {bad}");
        }
        let mut v = ok.clone();
        v.as_object_mut().unwrap().remove("height");
        assert_eq!(parse_window_state(&v), None);
        assert_eq!(parse_window_state(&json!([1, 2])), None);
        assert_eq!(parse_window_state(&json!(null)), None);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        assert_eq!(load_window_state(&p), None);
        save_window_state(&p, &sample());
        assert_eq!(load_window_state(&p), Some(sample()));
        assert!(!crate::fsutil::with_suffix(&p.window_state_file(), ".bak").exists());
        fs::write(p.window_state_file(), "{ torn").unwrap();
        assert_eq!(load_window_state(&p), None);
        fs::write(p.window_state_file(), r#"{"x":1,"y":2,"width":0,"height":3,"scale":1}"#).unwrap();
        assert_eq!(load_window_state(&p), None);
    }

    #[test]
    fn save_errors_are_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("blocker"), b"x").unwrap();
        let p = Profile::at(dir.path().join("blocker"), false);
        save_window_state(&p, &sample());
        assert_eq!(load_window_state(&p), None);
    }
}
