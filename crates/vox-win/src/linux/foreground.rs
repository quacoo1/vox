//! The foreground app is not observable from an X11 client under Wayland (and most apps are Wayland clients), so reads
//! carry no app label on Linux.

use vox_core::read::ForegroundInfo;

pub fn foreground_info() -> Option<ForegroundInfo> {
    None
}
