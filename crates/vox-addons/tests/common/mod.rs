//! Shared by the vox-addons integration tests: fake add-ons in temp folders, short timeouts, process checks.
//!
//! Rules (docs/addons-design.md "Tests"): temp folders only, short injected timeouts with generous upper bounds, so a
//! slow runner or an antivirus scanning a fresh exe does not flake.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;
use vox_addons::{load_spec, AddonRuntime, HelperSpec, HelperTimeouts};

/// The fake helper binary Cargo built for these tests.
pub const FAKE: &str = env!("CARGO_BIN_EXE_vox-addon-fake");

/// The timeouts every test uses: hello 2 s, no load allowance, idle 1 s, synth at most 10 s, cancel grace 300 ms.
pub fn quick() -> HelperTimeouts {
    HelperTimeouts {
        start: Duration::from_secs(2),
        load: Duration::ZERO,
        idle: Duration::from_secs(1),
        synth_max: Duration::from_secs(10),
        voices: Duration::from_secs(2),
        cancel_grace: Duration::from_millis(300),
    }
}

/// A temp folder for a test, with the fake's count and log files in it.
pub struct Scratch {
    pub tmp: TempDir,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch { tmp: tempfile::tempdir().expect("temp dir") }
    }

    pub fn path(&self) -> &Path {
        self.tmp.path()
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }
}

/// A spec that runs the fake straight from Cargo's target folder (no add-on folder), with [`quick`] timeouts, its
/// spawns counted in `<scratch>/count.txt` and its requests logged in `<scratch>/log.txt`.
pub fn fake_spec(scratch: &Scratch, args: &[&str]) -> HelperSpec {
    let mut spec = HelperSpec::new("fake-a", "Fake A", "1.0.0", PathBuf::from(FAKE), scratch.path().to_path_buf());
    spec.args = base_args(scratch, "count.txt", "log.txt");
    spec.args.extend(args.iter().map(|a| a.to_string()));
    spec.timeouts = quick();
    spec
}

fn base_args(scratch: &Scratch, count: &str, log: &str) -> Vec<String> {
    vec![
        "--count-file".into(),
        scratch.file(count).to_string_lossy().into_owned(),
        "--log-file".into(),
        scratch.file(log).to_string_lossy().into_owned(),
    ]
}

/// What a fake add-on folder looks like.
pub struct FakeAddon {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Extra fake flags.
    pub args: Vec<String>,
    /// `Some`: list these voices in addon.json; `None`: list none.
    pub voices: Option<Value>,
    pub extra: Value,
}

impl FakeAddon {
    pub fn new(id: &str, name: &str) -> FakeAddon {
        FakeAddon {
            id: id.into(),
            name: name.into(),
            version: "1.0.0".into(),
            args: Vec::new(),
            voices: Some(json!([
                {"key": "alpha", "name": "Alpha", "gender": "female", "language": "en-US", "accent": "American"},
                {"key": "beta", "name": "Beta", "gender": "male", "language": "en-GB", "accent": "British"}
            ])),
            extra: json!({}),
        }
    }

    /// Writes `<scratch>/<id>/<version>/` with the fake copied in as `bin/vox-addon-fake[.exe]` and an addon.json
    /// that logs to `<scratch>/<id>-count.txt` and `<scratch>/<id>-log.txt`. Returns the version folder.
    pub fn write(&self, scratch: &Scratch) -> PathBuf {
        let dir = scratch.path().join(&self.id).join(&self.version);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let exe = if cfg!(windows) { "bin/vox-addon-fake.exe" } else { "bin/vox-addon-fake" };
        std::fs::copy(FAKE, dir.join(exe)).unwrap();
        let mut args = base_args(scratch, &format!("{}-count.txt", self.id), &format!("{}-log.txt", self.id));
        args.extend(self.args.iter().cloned());
        let mut manifest = json!({
            "schema": 1, "id": self.id, "version": self.version, "name": self.name,
            "description": format!("{} test voices.", self.name),
            "protocol": {"min": 1, "max": 1}, "exe": exe, "args": args,
        });
        if let Some(v) = &self.voices {
            manifest["voices"] = v.clone();
        }
        for (k, v) in self.extra.as_object().unwrap() {
            manifest[k] = v.clone();
        }
        std::fs::write(dir.join("addon.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        dir
    }

    /// [`FakeAddon::write`], then the runtime with [`quick`] timeouts.
    pub fn runtime(&self, scratch: &Scratch) -> AddonRuntime {
        let dir = self.write(scratch);
        let (manifest, mut spec) = load_spec(&dir, None).expect("the fake add-on loads");
        spec.timeouts = quick();
        AddonRuntime::new(manifest, spec)
    }
}

/// Lines of a fake's count or log file (empty when it does not exist yet).
pub fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}

/// How many times the fake was spawned (its count file).
pub fn spawns(scratch: &Scratch, count_file: &str) -> usize {
    lines(&scratch.file(count_file)).len()
}

/// The pids the fake spawned, from its count file.
pub fn spawned_pids(scratch: &Scratch, count_file: &str) -> Vec<u32> {
    lines(&scratch.file(count_file)).iter().filter_map(|l| l.strip_prefix("spawn ")?.parse().ok()).collect()
}

/// Waits up to `timeout` for `f`.
pub fn eventually(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether process `pid` is running (a zombie counts as gone).
#[cfg(not(windows))]
pub fn is_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next());
    !matches!(state, Some('Z' | 'X') | None)
}

#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))).unwrap_or(false)
}
