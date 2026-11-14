//! Offline voice add-ons in the core (docs/addons-design.md "Vox integration"): what was found on disk at startup, the
//! read gate's question, the startup sweep and the idle tick. Installing, removing and the Voices page section come
//! with the add-on manager.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vox_addons::{AddonHost, AddonsRoot};
use vox_core::app::RuntimeFlags;
use vox_core::constants::{ADDON_IDLE_TICK_SEC, ADDON_QUIT_GRACE_MS};
use vox_core::providers::Provider;

use super::timers::TimerId;
use super::{spawn_worker, Core};

/// The add-ons folder and the speech provider serving what is installed in it. Absent in demo and shots mode, which
/// never touch the real folder.
pub struct AddonServices {
    pub root: AddonsRoot,
    pub host: Arc<AddonHost>,
    /// Add-ons with a `current.json` that did not load at startup (id → why, already logged).
    pub failed: BTreeMap<String, String>,
}

impl AddonServices {
    /// What `app::run` registers: the real add-ons folder, never in demo or shots mode.
    pub fn for_app(flags: &RuntimeFlags) -> Option<AddonServices> {
        if flags.demo || flags.shots() {
            return None;
        }
        vox_store::paths::addons_root().map(AddonServices::open)
    }

    /// Loads everything installed under `root`. Reads small JSON files only and starts no helper.
    pub fn open(root: PathBuf) -> AddonServices {
        let root = AddonsRoot::open(root);
        let host = Arc::new(AddonHost::new(|id| tracing::warn!(target: "addons", "{id}: unusable until it is repaired or Vox restarts")));
        let mut failed = BTreeMap::new();
        for item in root.scan() {
            match item.result {
                Ok(addon) => drop(host.replace(addon.runtime)),
                Err(e) => drop(failed.insert(item.id, e)),
            }
        }
        tracing::info!(target: "addons", "{} add-on(s) loaded from {}", host.runtimes().len(), root.path().display());
        AddonServices { root, host, failed }
    }
}

impl Core {
    /// Why this add-on voice cannot read (the message for the user); `None` when it may.
    pub(super) fn addon_refusal(&self, voice_id: &str) -> Option<String> {
        let not_available = || vox_tts::not_connected(Provider::Addon).message;
        let Some(voice) = vox_core::addons::parse_addon_voice_id(voice_id) else { return Some(not_available()) };
        let Some(addons) = &self.services.addons else { return Some(not_available()) };
        if !addons.host.is_installed(voice.addon) && addons.failed.contains_key(voice.addon) {
            return Some(format!("{} couldn’t be loaded. Reinstall it on the Voices page.", voice.addon));
        }
        addons.host.unavailable(voice.addon)
    }

    /// Whether any add-on is installed.
    pub(super) fn has_addons(&self) -> bool {
        self.services.addons.as_ref().is_some_and(|a| !a.host.runtimes().is_empty())
    }

    /// Startup: tidy up after interrupted installs on a worker, and start the idle tick.
    pub(super) fn start_addons(&mut self) {
        let Some(addons) = &self.services.addons else { return };
        let root = addons.root.clone();
        spawn_worker("vox-addons-sweep", move || {
            let report = root.sweep();
            if report != Default::default() {
                tracing::info!(target: "addons", "sweep: {report:?}");
            }
        });
        self.arm_addon_idle();
    }

    /// Runs while an add-on is installed.
    fn arm_addon_idle(&mut self) {
        if self.has_addons() {
            self.timers.set(TimerId::AddonIdle, Instant::now() + Duration::from_secs(ADDON_IDLE_TICK_SEC));
        }
    }

    /// `TimerId::AddonIdle`: quits helpers nobody has used for a while. On a worker, since each may take the quit grace
    /// to exit.
    pub(super) fn on_addon_idle(&mut self) {
        if let Some(addons) = &self.services.addons {
            let host = addons.host.clone();
            spawn_worker("vox-addons-idle", move || host.reap_idle(Duration::from_millis(ADDON_QUIT_GRACE_MS)));
        }
        self.arm_addon_idle();
    }
}
