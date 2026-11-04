//! Free disk space, for the add-on install preflight (docs/addons-design.md, "Install pipeline").

use std::path::Path;

/// The bytes this user may still write on the volume that holds `path`, or `None` when that cannot be read. A `path`
/// that does not exist yet (the add-ons folder before the first install) is measured at its nearest existing parent.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let existing =
        path.ancestors().map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p }).find(|p| p.exists())?;
    available(existing)
}

#[cfg(windows)]
fn available(path: &Path) -> Option<u64> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut free = 0u64;
    // SAFETY: `free` outlives the call; the other outputs are not asked for. The caller's figure honours disk quotas.
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(path), Some(&mut free), None, None) }.ok()?;
    Some(free)
}

#[cfg(not(windows))]
fn available(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero statvfs is valid; statvfs fills it in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers outlive the call.
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // f_bavail: the blocks an unprivileged user may use (not the root reserve).
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_folder_has_free_space() {
        let free = free_bytes(&std::env::temp_dir()).expect("free space of the temp folder");
        assert!(free > 0);
    }

    #[test]
    fn a_missing_folder_is_measured_at_its_parent() {
        let temp = std::env::temp_dir();
        let missing = temp.join(format!("vox-disk-missing-{}", std::process::id())).join("addons");
        assert!(!missing.exists());
        assert!(free_bytes(&missing).is_some());
    }

    #[test]
    fn a_relative_path_is_measured_from_the_working_folder() {
        assert!(free_bytes(Path::new("vox-disk-relative-missing")).is_some());
    }
}
