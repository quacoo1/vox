//! The add-ons folder: what the startup scan loads and what the sweep may delete (docs/addons-design.md "Startup").

// `TtsError` is returned by value across the speech provider API (see vox-tts lib.rs).
#![allow(clippy::result_large_err)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::*;
use serde_json::json;
use vox_addons::{AddonHost, AddonsRoot, BrokenReason, HelperHealth, ScanItem, SweepReport};
use vox_core::addons::InstallSource;
use vox_core::tts::MIME_WAV;
use vox_tts::{CancelToken, SpeechProvider};

const SHA: &str = "abababababababababababababababababababababababababababababababab";

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

/// A hand-placed add-on: `<root>/<id>/<version>/` plus a `current.json` naming it.
fn side_load(scratch: &Scratch, addon: &FakeAddon) -> PathBuf {
    let dir = addon.write(scratch);
    write_json(&dir.parent().unwrap().join("current.json"), json!({"schema": 1, "version": addon.version, "dir": addon.version, "source": "local"}));
    dir
}

/// An add-on as the installer leaves it: version folder `<version>-<rand>`, `current.json` from the catalogue.
fn catalogue_install(scratch: &Scratch, addon: &FakeAddon, rand: &str) -> PathBuf {
    let plain = addon.write(scratch);
    let dir = plain.with_file_name(format!("{}-{rand}", addon.version));
    fs::rename(&plain, &dir).unwrap();
    point_at(scratch, &addon.id, &addon.version, dir.file_name().unwrap().to_str().unwrap());
    dir
}

fn point_at(scratch: &Scratch, id: &str, version: &str, dir: &str) {
    write_json(
        &scratch.path().join(id).join("current.json"),
        json!({"schema": 1, "version": version, "dir": dir, "sha256": SHA, "size": 1, "installed_at": "2026-11-04T10:00:00Z", "source": "catalogue"}),
    );
}

fn scan(scratch: &Scratch) -> Vec<ScanItem> {
    AddonsRoot::open(scratch.path()).scan()
}

fn the_error(items: Vec<ScanItem>) -> String {
    assert_eq!(items.len(), 1);
    match items.into_iter().next().unwrap().result {
        Err(e) => e,
        Ok(_) => panic!("loaded"),
    }
}

fn sweep(scratch: &Scratch) -> SweepReport {
    AddonsRoot::open(scratch.path()).sweep()
}

#[test]
fn a_root_that_does_not_exist_is_just_empty() {
    let scratch = Scratch::new();
    let root = AddonsRoot::open(scratch.file("never-made"));
    assert!(root.scan().is_empty());
    assert_eq!(root.sweep(), SweepReport::default());
    assert!(!scratch.file("never-made").exists(), "opening, scanning and sweeping create nothing");
}

#[test]
fn a_side_loaded_add_on_lists_voices_without_starting_and_reads_when_used() {
    let scratch = Scratch::new();
    side_load(&scratch, &FakeAddon::new("fake-a", "Fake A"));
    let items = scan(&scratch);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "fake-a");
    let host = AddonHost::new(|_| {});
    for item in items {
        let addon = item.result.expect("loads");
        assert_eq!(addon.pointer.source, InstallSource::Local);
        assert_eq!(addon.runtime.helper.spec().data_dir.as_deref(), Some(scratch.path().join("fake-a").join("data").as_path()));
        host.replace(addon.runtime);
    }
    let voices = host.list_voices().unwrap();
    assert_eq!(voices.iter().map(|v| v.voice_id.as_str()).collect::<Vec<_>>(), ["addon:fake-a:alpha", "addon:fake-a:beta"]);
    assert_eq!(spawns(&scratch, "fake-a-count.txt"), 0, "scanning and listing start no helper");

    // The only test here that runs a helper: see tests/host.rs `live` for why copied programs and forks do not mix.
    let out = host.synthesize("Hello there.", "fake-a:alpha", false, &CancelToken::new()).expect("reads");
    assert_eq!(out.mime, MIME_WAV);
    assert_eq!(spawns(&scratch, "fake-a-count.txt"), 1);
    assert!(scratch.path().join("fake-a").join("data").is_dir(), "the data folder appears when the helper starts");
    host.shutdown(Duration::from_millis(300));
}

#[test]
fn only_current_json_makes_an_add_on_installed() {
    let scratch = Scratch::new();
    FakeAddon::new("fake-a", "Fake A").write(&scratch);
    assert!(scan(&scratch).is_empty());
}

#[test]
fn things_that_are_not_add_ons_are_ignored() {
    let scratch = Scratch::new();
    fs::write(scratch.file("catalogue.json"), "{}").unwrap();
    fs::create_dir_all(scratch.path().join(".staging")).unwrap();
    fs::create_dir_all(scratch.path().join("Not An Id")).unwrap();
    write_json(&scratch.path().join("Not An Id").join("current.json"), json!({"schema": 1}));
    assert!(scan(&scratch).is_empty());
}

#[test]
fn a_bad_pointer_is_reported_kept_and_blocks_the_sweep() {
    let scratch = Scratch::new();
    let dir = catalogue_install(&scratch, &FakeAddon::new("fake-a", "Fake A"), "abcd1234");
    let other = dir.with_file_name("0.9.0-zzzz9999");
    fs::create_dir_all(&other).unwrap();
    let pointer = scratch.path().join("fake-a").join("current.json");
    fs::write(&pointer, "{ not json").unwrap();
    assert!(the_error(scan(&scratch)).contains("current.json"));
    assert_eq!(sweep(&scratch), SweepReport::default());
    assert!(pointer.exists() && dir.exists() && other.exists(), "nothing goes when Vox cannot tell what is current");
}

#[test]
fn a_pointer_cannot_reach_outside_its_add_on() {
    let scratch = Scratch::new();
    let id_dir = scratch.path().join("fake-a");
    fs::create_dir_all(&id_dir).unwrap();
    FakeAddon::new("fake-b", "Fake B").write(&scratch);
    write_json(&id_dir.join("current.json"), json!({"schema": 1, "version": "1.0.0", "dir": "../fake-b/1.0.0", "source": "local"}));
    assert!(the_error(scan(&scratch)).contains("bad dir"));
}

#[test]
fn an_add_on_cannot_claim_another_id() {
    let scratch = Scratch::new();
    let addon = FakeAddon::new("fake-a", "Fake A");
    let dir = side_load(&scratch, &addon);
    let moved = scratch.path().join("fake-z");
    fs::rename(dir.parent().unwrap(), &moved).unwrap();
    assert!(the_error(scan(&scratch)).contains("fake-a"));
}

#[test]
fn a_catalogue_install_must_be_the_version_it_says_but_a_local_one_need_not() {
    let scratch = Scratch::new();
    let addon = FakeAddon::new("fake-a", "Fake A");
    let dir = catalogue_install(&scratch, &addon, "abcd1234");
    point_at(&scratch, "fake-a", "2.0.0", dir.file_name().unwrap().to_str().unwrap());
    assert!(the_error(scan(&scratch)).contains("2.0.0"));

    let scratch = Scratch::new();
    let dir = side_load(&scratch, &addon);
    write_json(&dir.parent().unwrap().join("current.json"), json!({"schema": 1, "version": "9.9.9", "dir": "1.0.0", "source": "local"}));
    assert!(scan(&scratch).remove(0).result.is_ok());
}

#[test]
fn a_pointer_at_a_missing_folder_is_not_installed_and_leaves_the_rest() {
    let scratch = Scratch::new();
    let dir = catalogue_install(&scratch, &FakeAddon::new("fake-a", "Fake A"), "abcd1234");
    point_at(&scratch, "fake-a", "1.0.0", "1.0.0-gone0000");
    assert!(the_error(scan(&scratch)).contains("missing"));
    assert_eq!(sweep(&scratch), SweepReport::default());
    assert!(dir.exists());
}

#[test]
fn a_broken_manifest_is_reported() {
    let scratch = Scratch::new();
    let dir = side_load(&scratch, &FakeAddon::new("fake-a", "Fake A"));
    fs::write(dir.join("addon.json"), "[]").unwrap();
    assert!(!the_error(scan(&scratch)).is_empty());
}

#[test]
fn the_sweep_removes_old_versions_but_never_the_current_one_or_the_data() {
    let scratch = Scratch::new();
    let dir = catalogue_install(&scratch, &FakeAddon::new("fake-a", "Fake A"), "abcd1234");
    let base = dir.parent().unwrap();
    let orphan = base.join("0.9.0-zzzz9999");
    fs::create_dir_all(orphan.join("models")).unwrap();
    fs::create_dir_all(base.join("data")).unwrap();
    fs::write(base.join("data").join("user.bin"), "keep").unwrap();
    // Not something the installer makes.
    fs::create_dir_all(base.join("notes")).unwrap();
    assert_eq!(sweep(&scratch), SweepReport { removed: 1, failed: 0 });
    assert!(!orphan.exists());
    assert!(dir.join("addon.json").exists());
    assert!(base.join("data").join("user.bin").exists());
    assert!(base.join("notes").exists());
    assert!(base.join("current.json").exists());
}

#[test]
fn the_sweep_finishes_an_interrupted_remove() {
    let scratch = Scratch::new();
    let dir = catalogue_install(&scratch, &FakeAddon::new("fake-a", "Fake A"), "abcd1234");
    let base = dir.parent().unwrap().to_path_buf();
    fs::create_dir_all(base.join("data")).unwrap();
    fs::remove_file(base.join("current.json")).unwrap();
    // The version folder, data/, then the empty add-on folder.
    assert_eq!(sweep(&scratch), SweepReport { removed: 3, failed: 0 });
    assert!(!base.exists());
}

#[test]
fn the_sweep_keeps_a_folder_without_an_installed_version() {
    let scratch = Scratch::new();
    // Someone's data in a folder named like an add-on id, and an empty one: nothing proves either is Vox's.
    fs::create_dir_all(scratch.path().join("corpus").join("data")).unwrap();
    fs::write(scratch.path().join("corpus").join("data").join("dataset.bin"), "precious").unwrap();
    fs::create_dir_all(scratch.path().join("scratch")).unwrap();
    assert_eq!(sweep(&scratch), SweepReport::default());
    assert!(scratch.path().join("corpus").join("data").join("dataset.bin").exists());
    assert!(scratch.path().join("scratch").exists());
}

#[test]
fn the_sweep_never_deletes_a_folder_vox_did_not_make() {
    let scratch = Scratch::new();
    // A helper author's folder without a current.json yet: a plain version folder is not an installer's.
    let work = FakeAddon::new("fake-a", "Fake A").write(&scratch);
    // What a folder of projects looks like with VOX_ADDONS_DIR pointed at it.
    fs::create_dir_all(scratch.path().join("src")).unwrap();
    fs::write(scratch.path().join("src").join("main.rs"), "fn main() {}").unwrap();
    fs::create_dir_all(scratch.path().join("target").join("1.0.0-abcd1234")).unwrap();
    fs::write(scratch.path().join("target").join("readme"), "x").unwrap();
    assert_eq!(sweep(&scratch), SweepReport::default());
    assert!(work.join("addon.json").exists());
    assert!(scratch.path().join("src").join("main.rs").exists());
    assert!(scratch.path().join("target").join("1.0.0-abcd1234").exists());
}

#[test]
fn the_sweep_leaves_a_hand_placed_add_on_alone() {
    let scratch = Scratch::new();
    let dir = side_load(&scratch, &FakeAddon::new("fake-a", "Fake A"));
    let older = dir.with_file_name("0.9.0-abcd1234");
    fs::create_dir_all(&older).unwrap();
    assert_eq!(sweep(&scratch), SweepReport::default());
    assert!(older.exists());
}

#[test]
fn the_sweep_empties_staging_except_recent_resumable_downloads() {
    let scratch = Scratch::new();
    let staging = scratch.path().join(".staging");
    let trash = staging.join("trash-fake-a-abcd1234");
    fs::create_dir_all(trash.join("bin")).unwrap();
    fs::create_dir_all(staging.join("fake-a-1.0.0-abcd1234")).unwrap();
    let touch = |name: &str| fs::write(staging.join(name), "x").unwrap();
    touch("keep-1.0.0-aaaa1111.part");
    touch("keep-1.0.0-aaaa1111.part.json");
    touch("noinfo-1.0.0-bbbb2222.part");
    touch("nopart-1.0.0-cccc3333.part.json");
    touch("old-1.0.0-dddd4444.part");
    touch("old-1.0.0-dddd4444.part.json");
    touch("stray.tmp");
    let week_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
    fs::File::options().write(true).open(staging.join("old-1.0.0-dddd4444.part")).unwrap().set_modified(week_ago).unwrap();

    assert_eq!(sweep(&scratch).failed, 0);
    let mut left: Vec<String> = fs::read_dir(&staging).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left, ["keep-1.0.0-aaaa1111.part", "keep-1.0.0-aaaa1111.part.json"]);
}

#[cfg(unix)]
#[test]
fn the_sweep_does_not_follow_links() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    let outside = scratch.file("outside");
    fs::create_dir_all(outside.join("1.0.0-abcd1234")).unwrap();
    let root = scratch.path().join("root");
    fs::create_dir_all(&root).unwrap();
    // A linked add-on folder, and a linked version folder inside an uninstalled add-on.
    symlink(&outside, root.join("linked")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    symlink(&outside, root.join("other").join("2.0.0-abcd1234")).unwrap();
    let report = AddonsRoot::open(&root).sweep();
    assert_eq!(report, SweepReport::default());
    assert!(outside.join("1.0.0-abcd1234").exists());
    assert!(root.join("other").exists());
}

#[cfg(unix)]
#[test]
fn a_pipe_or_device_instead_of_a_json_file_is_refused_without_blocking() {
    let scratch = Scratch::new();
    let dir = side_load(&scratch, &FakeAddon::new("fake-a", "Fake A"));
    // Opening a pipe with no writer would block the scan forever; /dev/zero would never end.
    let fifo = dir.join("addon.json");
    fs::remove_file(&fifo).unwrap();
    let made = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(made.success());
    assert!(the_error(scan(&scratch)).contains("not a file"));
    let pointer = dir.parent().unwrap().join("current.json");
    fs::remove_file(&pointer).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &pointer).unwrap();
    assert!(the_error(scan(&scratch)).contains("not a file"));
}

#[test]
fn the_sweep_removes_links_in_staging_without_following_them() {
    let scratch = Scratch::new();
    let outside = scratch.file("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "x").unwrap();
    let staging = scratch.path().join("root").join(".staging");
    fs::create_dir_all(&staging).unwrap();
    let link = staging.join("fake-a-1.0.0-abcd1234");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&outside, &link).is_err() {
        return; // creating links needs Developer Mode or elevation on Windows
    }
    let report = AddonsRoot::open(scratch.path().join("root")).sweep();
    assert_eq!(report, SweepReport { removed: 1, failed: 0 });
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(outside.join("keep.txt").exists());
}

#[test]
fn the_host_says_why_an_add_on_cannot_read() {
    let scratch = Scratch::new();
    let dir = side_load(&scratch, &FakeAddon::new("fake-a", "Fake A"));
    let host = AddonHost::new(|_| {});
    assert_eq!(host.unavailable("fake-a").unwrap(), "fake-a isn’t installed. Install it on the Voices page.");
    host.replace(scan(&scratch).remove(0).result.expect("loads").runtime);
    assert_eq!(host.unavailable("fake-a"), None);

    // The program disappears: starting it marks the add-on incomplete, and the host says so without trying again.
    fs::remove_file(dir.join(if cfg!(windows) { "bin/vox-addon-fake.exe" } else { "bin/vox-addon-fake" })).unwrap();
    let runtime = host.runtime("fake-a").unwrap();
    assert!(runtime.helper.start().is_err());
    assert_eq!(runtime.helper.health(), HelperHealth::Broken(BrokenReason::Incomplete));
    assert_eq!(host.unavailable("fake-a").unwrap(), "Fake A is incomplete. Repair it on the Voices page.");

    host.remove("fake-a", Duration::from_millis(100));
    assert_eq!(host.unavailable("fake-a").unwrap(), "Fake A was removed.");
}
