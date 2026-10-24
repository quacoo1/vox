//! Clipboard access (selection.md §9; BRIEF.md Product rulings "Clipboard"). Writes open the clipboard with a
//! message-only owner window (see "Threads" below); the clipboard is opened, used and closed within one synchronous
//! block (never held across a sleep). Privacy markers (`ExcludeClipboardContentFromMonitorProcessing`,
//! `CanIncludeInClipboardHistory = 0`, `CanUploadToCloudClipboard = 0`) make a copy unreadable for Vox.
//!
//! Snapshot/restore covers every format the old app restored (selection.md §9.1: text formats, HTML Format, RTF,
//! PNG/bitmap data, custom registered formats within the size/time budget) **plus `CF_HDROP` and `CF_DIB`**
//! (ruling), as raw `HGLOBAL` bytes; synthesized formats are skipped only when their source is present
//! (`CF_TEXT`/`CF_OEMTEXT` when `CF_UNICODETEXT` exists; `CF_BITMAP`/the other DIB when `CF_DIB`/`CF_DIBV5` exists).
//! Every restore write (and the clear after a refused secret) also sets `CanIncludeInClipboardHistory = 0` (DWORD 0)
//! and `ExcludeClipboardContentFromMonitorProcessing` (empty), so Windows clipboard history and clipboard monitors
//! never record Vox's restores.
//!
//! Threads: **reads** (`read_clipboard_text`, `snapshot`, the privacy flag values) open the clipboard without an
//! owner window on the calling thread — `GetClipboardData` may block while a delayed-rendering owner answers
//! `WM_RENDERFORMAT`, so callers keep them off the UI and core threads. **Writes** (`restore`, `write_clipboard_text`,
//! the clear) need an owner window (`OpenClipboard(NULL)` + `EmptyClipboard` leaves no owner and `SetClipboardData`
//! fails), and the owner receives `WM_DESTROYCLIPBOARD` whenever another app empties the clipboard, so the owner window
//! lives on its own small thread that always pumps messages (`vox-clipboard-owner`, started on first use); a write is
//! one job run there while the caller waits for its result.
//!
//! Formats that are not `HGLOBAL`s are handled per selection.md §9.1: `CF_ENHMETAFILE` is kept as its metafile bits
//! (`GetEnhMetaFileBits` / `SetEnhMetaFileBits`), so `CF_METAFILEPICT` is re-synthesized from it; `CF_BITMAP`,
//! `CF_PALETTE` and the other DIB are re-synthesized from the kept DIB; `CF_OWNERDISPLAY`, the `CF_DSP*` formats and
//! the private / GDI ranges cannot be kept and count as skipped. `DataObject` / `Ole Private Data` (OLE plumbing) are
//! dropped silently before the time budget is checked.

#![warn(dead_code, unused)]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteEnhMetaFile, GetEnhMetaFileBits, SetEnhMetaFileBits, HENHMETAFILE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW, HWND_MESSAGE, MSG,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
};

/// Snapshot of the user's clipboard (all formats that could be read).
#[derive(Debug, Clone, Default)]
pub struct ClipboardSnapshot {
    /// `(format id, bytes)` in the order the clipboard owner placed them (its preference; restored in that order).
    pub formats: Vec<(u32, Vec<u8>)>,
    /// Some formats were skipped (too large / unreadable).
    pub incomplete: bool,
    /// The snapshot failed entirely.
    pub failed: bool,
}

/// Why a copy must not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    Secret,
    Private,
}

/// `CF_UNICODETEXT` as a string (`None` on any error or no text). Used by "Read clipboard". May block for as long as
/// a delayed-rendering owner (Excel, Word) takes to answer `WM_RENDERFORMAT`, so vox-app calls it on a core worker,
/// never on the core or UI thread (ARCHITECTURE.md §3.2 `ReadClipboard`).
pub fn read_clipboard_text() -> Option<String> {
    let text = read_text_raw()?;
    (!text.is_empty()).then_some(text)
}

/// Puts plain text on the clipboard (History "Copy text", Settings copy chips).
pub fn write_clipboard_text(text: &str) -> Result<(), crate::WinError> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    run_on_owner(move |owner| {
        let _open = open_clipboard(Some(owner))?;
        // SAFETY: the clipboard is open by this thread with an owner window.
        unsafe { EmptyClipboard() }?;
        set_raw(CF_UNICODETEXT, &bytes)
    })?
}

/// `GetClipboardSequenceNumber`. `0` means the caller has no window-station access; the capture pipeline then logs
/// `clipboard sequence unavailable` once and reports no selection (the spec's no-sequence fallback is not ported,
/// ARCHITECTURE.md §8 D-SEL-5).
pub fn sequence_number() -> u32 {
    // SAFETY: no arguments; never needs the clipboard lock.
    unsafe { GetClipboardSequenceNumber() }
}

/// `IsClipboardFormatAvailable(CF_UNICODETEXT) || (CF_TEXT)`.
pub fn has_text_format() -> bool {
    format_available(CF_UNICODETEXT) || format_available(CF_TEXT)
}

/// Every readable format of the current clipboard (selection.md §9.1), in the order the owner placed them (that
/// order is the source app's preference, so [`restore`] writes them back the same way).
///
/// When the clipboard still holds our own restore (D-SEL-8, see [`privacy`]), the "do not record" markers that the
/// restore itself added are left out, so they never turn the user's own data into a "secret" on the next restore:
/// the snapshot carries the markers the original data carried (a secret keeps its exclusion marker; private data
/// keeps `CanIncludeInClipboardHistory = 0`).
pub fn snapshot() -> ClipboardSnapshot {
    let Ok(_open) = open_clipboard(None) else {
        return ClipboardSnapshot { formats: Vec::new(), incomplete: false, failed: true };
    };
    // Read while the clipboard is open: nobody can change it now, so the sequence number matches what we list.
    let own = own_write_privacy(*LAST_OWN_WRITE.lock(), sequence_number());
    let mut source = OpenClipboardSource { started: Instant::now() };
    let mut snapshot = build_snapshot(&mut source, Limits::DEFAULT);
    if let Some(original) = own {
        strip_own_markers(&mut snapshot.formats, original, &privacy_ids());
    }
    snapshot
}

/// Puts the snapshot back (one `OpenClipboard`/`EmptyClipboard`/`SetClipboardData…`/`CloseClipboard` block) with the
/// two "do not record" markers added (module docs). An empty snapshot clears the clipboard (with the markers).
/// Records `last_own_write` (see [`privacy`]) on success.
pub fn restore(snapshot: &ClipboardSnapshot) -> Result<(), crate::WinError> {
    restore_shared(Arc::new(snapshot.clone()))
}

/// Privacy markers on the current clipboard content.
///
/// Our own writes are exempt (selection.md §8.11, §13 #17): every restore marks the data with
/// `ExcludeClipboardContentFromMonitorProcessing`, which this function would otherwise read as `Secret`, so after
/// any capture the user's own restored clipboard would look like a password and copy-on-select would refuse it. The
/// module therefore records `last_own_write: Option<(seq, original: Option<Privacy>)>` right after each of its own
/// restores or clears (the sequence number after the write and the privacy of the snapshot that was put back);
/// while `sequence_number()` still equals that `seq`, `privacy()` returns `original` instead of scanning markers.
/// Unit-tested with the fake `CaptureEnv`.
pub fn privacy() -> Option<Privacy> {
    if let Some(original) = own_write_privacy(*LAST_OWN_WRITE.lock(), sequence_number()) {
        return original;
    }
    scan_privacy()
}

// ───────────────────────────── crate-internal entry points ─────────────────────────────

/// [`restore`] without copying the snapshot (the selection pipeline keeps snapshots in an `Arc`).
pub(crate) fn restore_shared(snapshot: Arc<ClipboardSnapshot>) -> Result<(), crate::WinError> {
    if snapshot.failed {
        return Err(crate::WinError("the clipboard snapshot failed; nothing to restore".into()));
    }
    run_on_owner(move |owner| {
        let ids = privacy_ids();
        let open = open_clipboard(Some(owner))?;
        // SAFETY: the clipboard is open by this thread with an owner window.
        unsafe { EmptyClipboard() }?;
        let mut written = 0usize;
        let mut failures = 0usize;
        for (id, bytes) in &snapshot.formats {
            if *id == ids.history_flag {
                // Replaced by our own "keep out of history" value below.
                continue;
            }
            let result = if *id == CF_ENHMETAFILE { set_emf(bytes) } else { set_raw(*id, bytes) };
            match result {
                Ok(()) => written += 1,
                Err(e) => {
                    failures += 1;
                    tracing::debug!(target: "selection", "clipboard restore skipped format {id}: {e}");
                }
            }
        }
        set_do_not_record_markers(&snapshot, &ids);
        drop(open);
        *LAST_OWN_WRITE.lock() = Some((sequence_number(), snapshot_privacy(&snapshot, &ids)));
        if written == 0 && failures > 0 {
            return Err(crate::WinError(format!("none of the {failures} clipboard formats could be restored")));
        }
        Ok(())
    })?
}

/// Empties the clipboard (with the "do not record" markers): the restore of a refused secret whose snapshot failed.
pub(crate) fn clear_clipboard() -> Result<(), crate::WinError> {
    run_on_owner(|owner| {
        let ids = privacy_ids();
        let open = open_clipboard(Some(owner))?;
        // SAFETY: the clipboard is open by this thread with an owner window.
        unsafe { EmptyClipboard() }?;
        set_do_not_record_markers(&ClipboardSnapshot::default(), &ids);
        drop(open);
        *LAST_OWN_WRITE.lock() = Some((sequence_number(), None));
        Ok(())
    })?
}

/// `CF_UNICODETEXT` up to the first NUL, untrimmed (`None` when the clipboard could not be opened or holds no text;
/// Windows synthesizes it from `CF_TEXT` / `CF_OEMTEXT`, so ANSI copies are covered).
pub(crate) fn read_text_raw() -> Option<String> {
    let _open = open_clipboard(None).ok()?;
    // SAFETY: the clipboard is open; the handle stays valid until it is closed.
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let global = HGLOBAL(handle.0);
    with_locked(global, |bytes| {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    })
}

/// `isPrivateFlagValue`: `None` (unreadable), fewer than 4 bytes, or a little-endian DWORD 0 → private.
pub(crate) fn is_private_flag_value(bytes: Option<&[u8]>) -> bool {
    match bytes {
        Some(b) if b.len() >= 4 => (b[0] | b[1] | b[2] | b[3]) == 0,
        _ => true,
    }
}

/// The D-SEL-8 exemption: `Some(original)` while the sequence number still equals the one recorded right after our
/// own restore or clear; `None` (scan the markers) otherwise. A sequence number of 0 never matches.
pub(crate) fn own_write_privacy(last: Option<(u32, Option<Privacy>)>, seq: u32) -> Option<Option<Privacy>> {
    match last {
        Some((recorded, original)) if seq != 0 && recorded == seq => Some(original),
        _ => None,
    }
}

// ───────────────────────────── format ids and rules ─────────────────────────────

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
const CF_OWNERDISPLAY: u32 = 0x80;
const CF_DSPBITMAP: u32 = 0x82;
const CF_DSPMETAFILEPICT: u32 = 0x83;
const CF_DSPENHMETAFILE: u32 = 0x8E;
/// `CF_PRIVATEFIRST..=CF_GDIOBJLAST`: handles of any kind, never copyable.
const PRIVATE_AND_GDI: std::ops::RangeInclusive<u32> = 0x0200..=0x03FF;
/// Registered formats start here.
const FIRST_REGISTERED: u32 = 0xC000;

/// Registered formats whose mere presence marks the content as a secret (selection.md §9.4).
const MARKER_FORMATS: [&str; 2] = ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore"];
/// Registered DWORD formats; 0 means "keep out of history / the cloud" (private).
const FLAG_FORMATS: [&str; 2] = ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"];

/// What to do with one listed format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatPlan {
    /// Copy the `HGLOBAL` bytes.
    Raw,
    /// Keep the enhanced metafile as its bits.
    Emf,
    /// Windows re-synthesizes it from a kept format, or it is OLE plumbing: drop silently.
    Silent,
    /// Cannot be kept: counts as skipped.
    Unsupported,
}

fn plan_format(id: u32, name: Option<&str>, listed: &[u32], first_dib: Option<u32>) -> FormatPlan {
    if let Some(name) = name {
        if name.eq_ignore_ascii_case("DataObject") || name.eq_ignore_ascii_case("Ole Private Data") {
            return FormatPlan::Silent;
        }
    }
    let has = |f: u32| listed.contains(&f);
    let has_dib = has(CF_DIB) || has(CF_DIBV5);
    match id {
        CF_TEXT | CF_OEMTEXT if has(CF_UNICODETEXT) => FormatPlan::Silent,
        CF_BITMAP | CF_PALETTE if has_dib => FormatPlan::Silent,
        CF_DIB | CF_DIBV5 if first_dib.is_some_and(|first| first != id) => FormatPlan::Silent,
        CF_METAFILEPICT if has(CF_ENHMETAFILE) => FormatPlan::Silent,
        CF_ENHMETAFILE => FormatPlan::Emf,
        CF_BITMAP | CF_PALETTE | CF_METAFILEPICT | CF_OWNERDISPLAY | CF_DSPBITMAP | CF_DSPMETAFILEPICT
        | CF_DSPENHMETAFILE => FormatPlan::Unsupported,
        _ if PRIVATE_AND_GDI.contains(&id) => FormatPlan::Unsupported,
        _ => FormatPlan::Raw,
    }
}

/// Read first, so a time-budget cut never loses the formats people actually paste.
fn rank(id: u32, name: Option<&str>) -> u8 {
    match (id, name) {
        (CF_UNICODETEXT, _) => 0,
        (_, Some(n)) if n.eq_ignore_ascii_case("HTML Format") => 1,
        (_, Some(n)) if n.eq_ignore_ascii_case("Rich Text Format") => 2,
        (_, Some(n)) if n.eq_ignore_ascii_case("PNG") => 3,
        (CF_DIB | CF_DIBV5, _) => 4,
        (CF_HDROP, _) => 5,
        _ => 6,
    }
}

fn is_ole_descriptor(name: Option<&str>) -> bool {
    name.is_some_and(|n| {
        n.eq_ignore_ascii_case("Object Descriptor") || n.eq_ignore_ascii_case("Link Source Descriptor")
    })
}

/// Snapshot budgets (selection.md §9.1).
#[derive(Debug, Clone, Copy)]
struct Limits {
    max_format: usize,
    max_total: usize,
    budget_ms: u64,
}

impl Limits {
    const DEFAULT: Limits = Limits { max_format: 64 << 20, max_total: 192 << 20, budget_ms: 500 };
}

/// One format read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fetch {
    Bytes(Vec<u8>),
    TooLarge,
    Unreadable,
}

/// The clipboard as the snapshot algorithm sees it (real: the open clipboard; tests: a fake).
trait FormatSource {
    /// Every listed format, in enumeration order.
    fn formats(&mut self) -> Vec<u32>;
    /// The registered name of a format id ≥ 0xC000.
    fn name(&mut self, id: u32) -> Option<String>;
    /// Reads one format, refusing anything larger than `max` bytes.
    fn fetch(&mut self, id: u32, plan: FormatPlan, max: usize) -> Fetch;
    /// Milliseconds since the snapshot started.
    fn elapsed_ms(&mut self) -> u64;
}

/// selection.md §9.1 over any [`FormatSource`]. Formats are **read** in priority order (so a time-budget cut keeps
/// the ones people paste) but **kept** in enumeration order: that is the order the owner placed them in, i.e. its
/// preference (most descriptive first), which `EnumClipboardFormats` and OLE's `IEnumFORMATETC` report to paste
/// targets, so the restore must write them back in that order.
fn build_snapshot(src: &mut dyn FormatSource, limits: Limits) -> ClipboardSnapshot {
    let listed = src.formats();
    let first_dib = listed.iter().copied().find(|&id| id == CF_DIB || id == CF_DIBV5);
    let mut ordered: Vec<(usize, u32, Option<String>)> = listed
        .iter()
        .enumerate()
        .map(|(index, &id)| (index, id, if id >= FIRST_REGISTERED { src.name(id) } else { None }))
        .collect();
    ordered.sort_by_key(|(_, id, name)| rank(*id, name.as_deref()));

    let mut kept: Vec<(usize, u32, Option<String>, Vec<u8>)> = Vec::new();
    let mut skipped = 0usize;
    let mut bytes = 0usize;
    for (index, id, name) in ordered {
        let plan = plan_format(id, name.as_deref(), &listed, first_dib);
        match plan {
            FormatPlan::Silent => continue,
            FormatPlan::Unsupported => {
                skipped += 1;
                continue;
            }
            FormatPlan::Raw | FormatPlan::Emf => {}
        }
        // Checked before each read, not during it: one slow delayed render can overrun it.
        if src.elapsed_ms() > limits.budget_ms {
            skipped += 1;
            continue;
        }
        let room = limits.max_format.min(limits.max_total.saturating_sub(bytes));
        match src.fetch(id, plan, room) {
            Fetch::Bytes(data) => {
                bytes += data.len();
                kept.push((index, id, name, data));
            }
            Fetch::TooLarge | Fetch::Unreadable => skipped += 1,
        }
    }
    if skipped > 0 {
        // OLE descriptors only describe the payload next to them; without it they are junk.
        kept.retain(|(_, _, name, _)| !is_ole_descriptor(name.as_deref()));
    }
    // Back to the owner's order for the restore.
    kept.sort_by_key(|(index, ..)| *index);
    let failed = kept.is_empty() && skipped > 0;
    ClipboardSnapshot {
        formats: kept.into_iter().map(|(_, id, _, data)| (id, data)).collect(),
        incomplete: skipped > 0,
        failed,
    }
}

/// Registered ids of the privacy formats (0 when registration failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrivacyIds {
    markers: [u32; 2],
    flags: [u32; 2],
    /// `CanIncludeInClipboardHistory`
    history_flag: u32,
    /// `ExcludeClipboardContentFromMonitorProcessing`
    exclude_marker: u32,
}

fn privacy_ids() -> PrivacyIds {
    static IDS: OnceLock<PrivacyIds> = OnceLock::new();
    *IDS.get_or_init(|| {
        let markers = MARKER_FORMATS.map(register_format);
        let flags = FLAG_FORMATS.map(register_format);
        PrivacyIds { markers, flags, history_flag: flags[0], exclude_marker: markers[0] }
    })
}

/// Removes the markers our own restore added from a snapshot taken of that restore (D-SEL-8). `original` is the
/// privacy recorded for the restored data: `None` → both markers are ours (the data carried neither an exclusion
/// marker nor a private flag); `Private` → the exclusion marker is ours (data with one would have been `Secret`),
/// while `CanIncludeInClipboardHistory = 0` matches the original's private value; `Secret` → nothing is removed (the
/// exclusion marker may be the original's own, and the data stays a secret either way).
fn strip_own_markers(formats: &mut Vec<(u32, Vec<u8>)>, original: Option<Privacy>, ids: &PrivacyIds) {
    let ours: &[u32] = match original {
        Some(Privacy::Secret) => &[],
        Some(Privacy::Private) => &[ids.exclude_marker],
        None => &[ids.exclude_marker, ids.history_flag],
    };
    formats.retain(|(id, _)| *id == 0 || !ours.contains(id));
}

/// The privacy the snapshot's own data carried (what `privacy()` would have said before the capture).
fn snapshot_privacy(snapshot: &ClipboardSnapshot, ids: &PrivacyIds) -> Option<Privacy> {
    let present = |id: u32| id != 0 && snapshot.formats.iter().any(|(f, _)| *f == id);
    if ids.markers.iter().any(|&id| present(id)) {
        return Some(Privacy::Secret);
    }
    let private =
        ids.flags.iter().filter(|&&id| present(id)).any(|&id| {
            is_private_flag_value(snapshot.formats.iter().find(|(f, _)| *f == id).map(|(_, b)| b.as_slice()))
        });
    private.then_some(Privacy::Private)
}

// ───────────────────────────── Win32 plumbing ─────────────────────────────

/// `(seq after our write, privacy of what we put back)`, see [`privacy`].
static LAST_OWN_WRITE: Mutex<Option<(u32, Option<Privacy>)>> = Mutex::new(None);

fn register_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
}

fn format_available(id: u32) -> bool {
    // SAFETY: a plain value argument; needs no clipboard lock.
    id != 0 && unsafe { IsClipboardFormatAvailable(id) }.is_ok()
}

/// Closes the clipboard when dropped.
struct OpenGuard;

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard.
        let _ = unsafe { CloseClipboard() };
    }
}

/// `OpenClipboard` with up to 3 attempts 20 ms apart (another app, often a clipboard manager, may hold it open).
fn open_clipboard(owner: Option<HWND>) -> Result<OpenGuard, crate::WinError> {
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: `owner` is our own message-only window (or none, for reads).
        match unsafe { OpenClipboard(owner) } {
            Ok(()) => return Ok(OpenGuard),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(|| crate::WinError("cannot open the clipboard".into()), crate::WinError::from))
}

/// Runs `f` over the locked bytes of an `HGLOBAL` (`None` when it cannot be locked).
fn with_locked<T>(global: HGLOBAL, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
    // SAFETY: `global` comes from GetClipboardData while the clipboard is open; the lock is released below.
    unsafe {
        let size = GlobalSize(global);
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            return None;
        }
        let out = f(std::slice::from_raw_parts(ptr as *const u8, size));
        let _ = GlobalUnlock(global);
        Some(out)
    }
}

/// The real source: the clipboard opened (without owner) by this thread.
struct OpenClipboardSource {
    started: Instant,
}

impl FormatSource for OpenClipboardSource {
    fn formats(&mut self) -> Vec<u32> {
        let mut out = Vec::new();
        let mut id = 0u32;
        loop {
            // SAFETY: the clipboard is open by this thread.
            id = unsafe { EnumClipboardFormats(id) };
            if id == 0 || out.len() >= 1024 {
                break;
            }
            out.push(id);
        }
        out
    }

    fn name(&mut self, id: u32) -> Option<String> {
        let mut buf = [0u16; 256];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { GetClipboardFormatNameW(id, &mut buf) };
        let n = usize::try_from(n).ok().filter(|&n| n > 0)?.min(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    fn fetch(&mut self, id: u32, plan: FormatPlan, max: usize) -> Fetch {
        // SAFETY: the clipboard is open; may send WM_RENDERFORMAT to a delayed-rendering owner.
        let Ok(handle) = (unsafe { GetClipboardData(id) }) else { return Fetch::Unreadable };
        if handle.is_invalid() {
            return Fetch::Unreadable;
        }
        match plan {
            FormatPlan::Emf => fetch_emf(HENHMETAFILE(handle.0), max),
            _ => fetch_global(HGLOBAL(handle.0), max),
        }
    }

    fn elapsed_ms(&mut self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

fn fetch_global(global: HGLOBAL, max: usize) -> Fetch {
    // SAFETY: a handle from GetClipboardData; GlobalSize returns 0 for an invalid one.
    let size = unsafe { GlobalSize(global) };
    if size > max {
        return Fetch::TooLarge;
    }
    if size == 0 {
        // A zero-length payload (some apps set presence-only formats this way).
        return Fetch::Bytes(Vec::new());
    }
    match with_locked(global, |b| b.to_vec()) {
        Some(bytes) => Fetch::Bytes(bytes),
        None => Fetch::Unreadable,
    }
}

fn fetch_emf(emf: HENHMETAFILE, max: usize) -> Fetch {
    // SAFETY: a size query.
    let size = unsafe { GetEnhMetaFileBits(emf, None) } as usize;
    if size == 0 {
        return Fetch::Unreadable;
    }
    if size > max {
        return Fetch::TooLarge;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` holds `size` bytes.
    let n = unsafe { GetEnhMetaFileBits(emf, Some(&mut buf)) } as usize;
    if n == 0 {
        return Fetch::Unreadable;
    }
    buf.truncate(n);
    Fetch::Bytes(buf)
}

/// `GlobalAlloc` + copy + `SetClipboardData` (the system owns the memory afterwards).
fn set_raw(id: u32, bytes: &[u8]) -> Result<(), crate::WinError> {
    // SAFETY: a fresh moveable allocation of at least one byte, filled under its lock and handed to the clipboard;
    // freed here only when SetClipboardData refused it.
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(crate::WinError("GlobalLock failed".into()));
        }
        std::ptr::write_bytes(ptr as *mut u8, 0, bytes.len().max(1));
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(global);
        if let Err(e) = SetClipboardData(id, Some(HANDLE(global.0))) {
            let _ = GlobalFree(Some(global));
            return Err(e.into());
        }
    }
    Ok(())
}

fn set_emf(bits: &[u8]) -> Result<(), crate::WinError> {
    // SAFETY: the metafile is created from our bytes and handed to the clipboard; deleted only when refused.
    unsafe {
        let emf = SetEnhMetaFileBits(bits);
        if emf.is_invalid() {
            return Err(crate::WinError("SetEnhMetaFileBits failed".into()));
        }
        if let Err(e) = SetClipboardData(CF_ENHMETAFILE, Some(HANDLE(emf.0))) {
            let _ = DeleteEnhMetaFile(Some(emf));
            return Err(e.into());
        }
    }
    Ok(())
}

/// `CanIncludeInClipboardHistory = 0` and `ExcludeClipboardContentFromMonitorProcessing` on the open clipboard.
fn set_do_not_record_markers(snapshot: &ClipboardSnapshot, ids: &PrivacyIds) {
    if ids.history_flag != 0 {
        if let Err(e) = set_raw(ids.history_flag, &0u32.to_le_bytes()) {
            tracing::debug!(target: "selection", "CanIncludeInClipboardHistory not set: {e}");
        }
    }
    let already = snapshot.formats.iter().any(|(f, _)| *f == ids.exclude_marker);
    if ids.exclude_marker != 0 && !already {
        if let Err(e) = set_raw(ids.exclude_marker, &[0u8; 4]) {
            tracing::debug!(target: "selection", "ExcludeClipboardContentFromMonitorProcessing not set: {e}");
        }
    }
}

/// selection.md §9.4 with Win32.
fn scan_privacy() -> Option<Privacy> {
    let ids = privacy_ids();
    if ids.markers.iter().any(|&id| format_available(id)) {
        return Some(Privacy::Secret);
    }
    let flags: Vec<u32> = ids.flags.iter().copied().filter(|&id| format_available(id)).collect();
    if flags.is_empty() {
        return None;
    }
    // A flag is listed but the clipboard can't be read: assume the restrictive value.
    let Ok(_open) = open_clipboard(None) else { return Some(Privacy::Private) };
    let private = flags.iter().any(|&id| {
        // SAFETY: the clipboard is open by this thread.
        let bytes = unsafe { GetClipboardData(id) }
            .ok()
            .filter(|h| !h.is_invalid())
            .and_then(|h| with_locked(HGLOBAL(h.0), |b| b.to_vec()));
        is_private_flag_value(bytes.as_deref())
    });
    private.then_some(Privacy::Private)
}

// ───────────────────────────── the owner-window thread ─────────────────────────────

type Job = Box<dyn FnOnce(HWND) + Send>;

/// Posted to the owner window to run queued jobs.
const WM_RUN_JOBS: u32 = WM_APP + 0x56;

struct Owner {
    jobs: Sender<Job>,
    hwnd: isize,
}

fn owner() -> Option<&'static Owner> {
    static OWNER: OnceLock<Option<Owner>> = OnceLock::new();
    OWNER.get_or_init(spawn_owner).as_ref()
}

fn spawn_owner() -> Option<Owner> {
    let (jobs, rx) = crossbeam_channel::unbounded::<Job>();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Option<isize>>(1);
    let spawned =
        std::thread::Builder::new().name("vox-clipboard-owner".into()).spawn(move || owner_thread(rx, ready_tx));
    if let Err(e) = spawned {
        tracing::warn!(target: "selection", "cannot start the clipboard owner thread: {e}");
        return None;
    }
    match ready_rx.recv() {
        Ok(Some(hwnd)) => Some(Owner { jobs, hwnd }),
        _ => {
            tracing::warn!(target: "selection", "cannot create the clipboard owner window");
            None
        }
    }
}

unsafe extern "system" fn owner_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: default processing (WM_DESTROYCLIPBOARD and friends need nothing else).
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn owner_thread(jobs: Receiver<Job>, ready: Sender<Option<isize>>) {
    // SAFETY: registering a class with a static name and an `extern "system"` procedure, then creating a
    // message-only window of it on this thread.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).ok().map(|m| m.into()).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(owner_wndproc),
            hInstance: instance,
            lpszClassName: w!("VoxClipboardOwner"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("VoxClipboardOwner"),
            w!("Vox clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(target: "selection", "clipboard owner window: {e}");
            let _ = ready.send(None);
            return;
        }
    };
    let _ = ready.send(Some(hwnd.0 as isize));
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` outlives the call.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        if msg.message == WM_RUN_JOBS && msg.hwnd == hwnd {
            for job in jobs.try_iter() {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(hwnd))).is_err() {
                    tracing::warn!(target: "selection", "clipboard job panicked");
                }
            }
            continue;
        }
        // SAFETY: a message retrieved by GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }
}

/// Runs `f` on the owner thread with the owner window and waits (≤ 10 s) for its result.
fn run_on_owner<T: Send + 'static>(f: impl FnOnce(HWND) -> T + Send + 'static) -> Result<T, crate::WinError> {
    let owner = owner().ok_or_else(|| crate::WinError("the clipboard owner window is unavailable".into()))?;
    let (tx, rx) = crossbeam_channel::bounded::<T>(1);
    let job: Job = Box::new(move |hwnd| {
        let _ = tx.send(f(hwnd));
    });
    owner.jobs.send(job).map_err(|_| crate::WinError("the clipboard owner thread has ended".into()))?;
    // SAFETY: posting a private message to our own window.
    unsafe { PostMessageW(Some(HWND(owner.hwnd as *mut core::ffi::c_void)), WM_RUN_JOBS, WPARAM(0), LPARAM(0)) }?;
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| crate::WinError("the clipboard write did not finish".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted clipboard: `(id, name, size, unreadable)` plus a clock that advances per read.
    struct FakeSource {
        formats: Vec<(u32, Option<&'static str>, usize, bool)>,
        clock: u64,
        cost_per_read: u64,
        reads: Vec<u32>,
    }

    impl FakeSource {
        fn new(formats: Vec<(u32, Option<&'static str>, usize, bool)>) -> Self {
            FakeSource { formats, clock: 0, cost_per_read: 0, reads: Vec::new() }
        }
    }

    impl FormatSource for FakeSource {
        fn formats(&mut self) -> Vec<u32> {
            self.formats.iter().map(|f| f.0).collect()
        }
        fn name(&mut self, id: u32) -> Option<String> {
            self.formats.iter().find(|f| f.0 == id).and_then(|f| f.1).map(str::to_string)
        }
        fn fetch(&mut self, id: u32, _plan: FormatPlan, max: usize) -> Fetch {
            self.reads.push(id);
            self.clock += self.cost_per_read;
            let Some(&(_, _, size, unreadable)) = self.formats.iter().find(|f| f.0 == id) else {
                return Fetch::Unreadable;
            };
            if unreadable {
                Fetch::Unreadable
            } else if size > max {
                Fetch::TooLarge
            } else {
                Fetch::Bytes(vec![id as u8; size])
            }
        }
        fn elapsed_ms(&mut self) -> u64 {
            self.clock
        }
    }

    const HTML: u32 = 0xC0A0;
    const RTF: u32 = 0xC0A1;
    const PNG: u32 = 0xC0A2;
    const DATAOBJECT: u32 = 0xC001;
    const OLE_PRIVATE: u32 = 0xC002;
    const OBJ_DESC: u32 = 0xC003;
    const LINK_DESC: u32 = 0xC004;
    const CUSTOM: u32 = 0xC100;

    fn ids(s: &ClipboardSnapshot) -> Vec<u32> {
        s.formats.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn priority_formats_are_read_first_and_synthesized_text_is_dropped() {
        let mut src = FakeSource::new(vec![
            (CUSTOM, Some("Custom"), 10, false),
            (CF_TEXT, None, 5, false),
            (CF_OEMTEXT, None, 5, false),
            (CF_LOCALE_ID, None, 4, false),
            (RTF, Some("Rich Text Format"), 30, false),
            (CF_UNICODETEXT, None, 10, false),
            (HTML, Some("HTML Format"), 20, false),
        ]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(src.reads, vec![CF_UNICODETEXT, HTML, RTF, CUSTOM, CF_LOCALE_ID]);
        // Kept (and restored) in the owner's order, not the read order.
        assert_eq!(ids(&s), vec![CUSTOM, CF_LOCALE_ID, RTF, CF_UNICODETEXT, HTML]);
        assert!(!s.incomplete && !s.failed);
    }

    #[test]
    fn the_owners_format_order_survives_a_budget_cut() {
        // Word-like order: native format first, then rich text, HTML, plain text.
        let mut src = FakeSource::new(vec![
            (CUSTOM, Some("Native"), 4, false),
            (RTF, Some("Rich Text Format"), 4, false),
            (HTML, Some("HTML Format"), 4, false),
            (CF_UNICODETEXT, None, 4, false),
            (PNG, Some("PNG"), 4, false),
        ]);
        src.cost_per_read = 200; // the budget runs out after three reads
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(src.reads, vec![CF_UNICODETEXT, HTML, RTF], "paste-critical formats are read first");
        assert_eq!(ids(&s), vec![RTF, HTML, CF_UNICODETEXT], "kept in the owner's order");
        assert!(s.incomplete && !s.failed);
    }

    const CF_LOCALE_ID: u32 = 16;

    #[test]
    fn an_ansi_only_clipboard_keeps_cf_text() {
        let mut src = FakeSource::new(vec![(CF_TEXT, None, 5, false), (CF_OEMTEXT, None, 5, false)]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_TEXT, CF_OEMTEXT]);
    }

    #[test]
    fn images_keep_one_dib_and_drop_what_windows_resynthesizes() {
        // A CF_BITMAP-only copy lists the synthesized DIBs too: keep the first DIB.
        let mut src = FakeSource::new(vec![
            (CF_BITMAP, None, 0, false),
            (CF_DIB, None, 100, false),
            (CF_DIBV5, None, 120, false),
            (CF_PALETTE, None, 0, false),
        ]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_DIB]);
        assert!(!s.incomplete);
        // DIBV5 enumerated first wins.
        let mut src = FakeSource::new(vec![(CF_DIBV5, None, 120, false), (CF_DIB, None, 100, false)]);
        assert_eq!(ids(&build_snapshot(&mut src, Limits::DEFAULT)), vec![CF_DIBV5]);
        // A lone CF_BITMAP can't be kept.
        let mut src = FakeSource::new(vec![(CF_BITMAP, None, 0, false)]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert!(s.failed && s.incomplete && s.formats.is_empty());
    }

    #[test]
    fn metafiles_keep_the_enhanced_metafile_only() {
        let mut src = FakeSource::new(vec![(CF_ENHMETAFILE, None, 50, false), (CF_METAFILEPICT, None, 0, false)]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_ENHMETAFILE]);
        assert!(!s.incomplete);
        let mut src = FakeSource::new(vec![(CF_UNICODETEXT, None, 4, false), (CF_METAFILEPICT, None, 0, false)]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert!(s.incomplete && !s.failed);
    }

    #[test]
    fn files_are_kept_raw() {
        let mut src = FakeSource::new(vec![
            (CF_HDROP, None, 300, false),
            (0xC200, Some("Preferred DropEffect"), 4, false),
            (0xC201, Some("Shell IDList Array"), 90, false),
        ]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_HDROP, 0xC200, 0xC201]);
        assert!(!s.incomplete);
    }

    #[test]
    fn ole_plumbing_is_dropped_before_the_budget_check_and_never_counts() {
        let mut src = FakeSource::new(vec![
            (CF_UNICODETEXT, None, 4, false),
            (DATAOBJECT, Some("DataObject"), 4, false),
            (OLE_PRIVATE, Some("Ole Private Data"), 4, false),
        ]);
        src.cost_per_read = 1000; // over budget after the first read
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_UNICODETEXT]);
        assert!(!s.incomplete, "the plumbing formats are not 'skipped'");
        assert_eq!(src.reads, vec![CF_UNICODETEXT]);
        // Case-insensitive names.
        let mut src = FakeSource::new(vec![(DATAOBJECT, Some("dataobject"), 4, false)]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert!(s.formats.is_empty() && !s.incomplete && !s.failed);
    }

    #[test]
    fn the_time_budget_skips_every_remaining_format() {
        let mut src = FakeSource::new(vec![
            (CF_UNICODETEXT, None, 4, false),
            (HTML, Some("HTML Format"), 4, false),
            (CUSTOM, Some("Custom"), 4, false),
        ]);
        src.cost_per_read = 501;
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CF_UNICODETEXT]);
        assert!(s.incomplete && !s.failed);
        assert_eq!(src.reads, vec![CF_UNICODETEXT]);
        // Exactly at the budget still reads (strictly greater skips).
        let mut src = FakeSource::new(vec![(CF_UNICODETEXT, None, 4, false), (HTML, Some("HTML Format"), 4, false)]);
        src.cost_per_read = 500;
        assert_eq!(ids(&build_snapshot(&mut src, Limits::DEFAULT)), vec![CF_UNICODETEXT, HTML]);
    }

    #[test]
    fn size_limits_per_format_and_in_total() {
        let limits = Limits { max_format: 100, max_total: 150, budget_ms: 500 };
        let mut src = FakeSource::new(vec![
            (CF_UNICODETEXT, None, 80, false),
            (HTML, Some("HTML Format"), 101, false), // over the per-format limit
            (RTF, Some("Rich Text Format"), 71, false), // 80 + 71 > 150
            (PNG, Some("PNG"), 70, false),           // 80 + 70 = 150 fits
        ]);
        let s = build_snapshot(&mut src, limits);
        assert_eq!(ids(&s), vec![CF_UNICODETEXT, PNG]);
        assert!(s.incomplete && !s.failed);
    }

    #[test]
    fn descriptors_are_dropped_only_when_something_was_skipped() {
        let base = vec![
            (CUSTOM, Some("Embed Source"), 10, false),
            (OBJ_DESC, Some("Object Descriptor"), 10, false),
            (LINK_DESC, Some("Link Source Descriptor"), 10, false),
        ];
        let mut src = FakeSource::new(base.clone());
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert_eq!(ids(&s), vec![CUSTOM, OBJ_DESC, LINK_DESC]);
        let mut with_bad = base;
        with_bad[0].3 = true; // the payload itself is unreadable
        let mut src = FakeSource::new(with_bad);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert!(s.formats.is_empty());
        assert!(s.failed && s.incomplete, "nothing kept and something skipped → failed");
    }

    #[test]
    fn an_empty_clipboard_is_a_complete_snapshot() {
        let mut src = FakeSource::new(vec![]);
        let s = build_snapshot(&mut src, Limits::DEFAULT);
        assert!(s.formats.is_empty() && !s.incomplete && !s.failed);
    }

    #[test]
    fn unsupported_handle_formats_count_as_skipped() {
        for id in [CF_OWNERDISPLAY, CF_DSPBITMAP, CF_DSPMETAFILEPICT, CF_DSPENHMETAFILE, 0x0200, 0x02FF, 0x0300, 0x03FF]
        {
            let mut src = FakeSource::new(vec![(CF_UNICODETEXT, None, 2, false), (id, None, 2, false)]);
            let s = build_snapshot(&mut src, Limits::DEFAULT);
            assert_eq!(ids(&s), vec![CF_UNICODETEXT], "{id:#x}");
            assert!(s.incomplete, "{id:#x}");
            assert!(!src.reads.contains(&id), "{id:#x} is never read");
        }
        // CF_DSPTEXT (an HGLOBAL) and 0x0400 are ordinary raw formats.
        let mut src = FakeSource::new(vec![(0x81, None, 2, false), (0x0400, None, 2, false)]);
        assert_eq!(ids(&build_snapshot(&mut src, Limits::DEFAULT)), vec![0x81, 0x0400]);
    }

    #[test]
    fn private_flag_values() {
        assert!(is_private_flag_value(None));
        assert!(is_private_flag_value(Some(&[])));
        assert!(is_private_flag_value(Some(&[1, 0, 0])));
        assert!(is_private_flag_value(Some(&[0, 0, 0, 0])));
        assert!(is_private_flag_value(Some(&[0, 0, 0, 0, 9])));
        assert!(!is_private_flag_value(Some(&[1, 0, 0, 0])));
        assert!(!is_private_flag_value(Some(&[0, 0, 0, 1])));
    }

    #[test]
    fn own_writes_are_exempt_while_the_sequence_is_unchanged() {
        assert_eq!(own_write_privacy(None, 5), None);
        assert_eq!(own_write_privacy(Some((5, None)), 5), Some(None));
        assert_eq!(own_write_privacy(Some((5, Some(Privacy::Secret))), 5), Some(Some(Privacy::Secret)));
        assert_eq!(own_write_privacy(Some((5, None)), 6), None);
        assert_eq!(own_write_privacy(Some((0, None)), 0), None);
    }

    #[test]
    fn snapshot_privacy_reads_the_snapshot_markers() {
        let ids = PrivacyIds {
            markers: [0xC300, 0xC301],
            flags: [0xC302, 0xC303],
            history_flag: 0xC302,
            exclude_marker: 0xC300,
        };
        let snap = |formats: Vec<(u32, Vec<u8>)>| ClipboardSnapshot { formats, incomplete: false, failed: false };
        assert_eq!(snapshot_privacy(&snap(vec![(13, vec![65, 0, 0, 0])]), &ids), None);
        assert_eq!(snapshot_privacy(&snap(vec![(0xC301, vec![])]), &ids), Some(Privacy::Secret));
        assert_eq!(snapshot_privacy(&snap(vec![(0xC303, vec![0, 0, 0, 0])]), &ids), Some(Privacy::Private));
        assert_eq!(snapshot_privacy(&snap(vec![(0xC302, vec![1, 0, 0, 0])]), &ids), None);
        assert_eq!(snapshot_privacy(&snap(vec![(0xC302, vec![1])]), &ids), Some(Privacy::Private));
        // Unregistered ids (0) never match.
        let zero = PrivacyIds { markers: [0, 0], flags: [0, 0], history_flag: 0, exclude_marker: 0 };
        assert_eq!(snapshot_privacy(&snap(vec![(0, vec![])]), &zero), None);
    }

    /// D-SEL-8 across several captures: a snapshot of our own restore drops the markers that restore added, so the
    /// recorded original privacy never drifts to `Secret` (the second restore used to record `Secret` for the user's
    /// ordinary clipboard, and copy-on-select then refused it as a password).
    #[test]
    fn a_snapshot_of_our_own_restore_drops_the_markers_it_added() {
        let ids = PrivacyIds {
            markers: [0xC300, 0xC301],
            flags: [0xC302, 0xC303],
            history_flag: 0xC302,
            exclude_marker: 0xC300,
        };
        let text = (CF_UNICODETEXT, vec![65, 0, 0, 0]);
        let exclude = (0xC300, vec![0, 0, 0, 0]);
        let history_off = (0xC302, vec![0, 0, 0, 0]);
        let cloud_off = (0xC303, vec![0, 0, 0, 0]);
        let snap = |formats: Vec<(u32, Vec<u8>)>| ClipboardSnapshot { formats, incomplete: false, failed: false };

        // Ordinary data, restored once (+ both markers), snapshotted again: back to the data alone.
        let mut formats = vec![text.clone(), history_off.clone(), exclude.clone()];
        strip_own_markers(&mut formats, None, &ids);
        assert_eq!(formats, vec![text.clone()]);
        assert_eq!(snapshot_privacy(&snap(formats), &ids), None);
        // Without the strip the second restore would have recorded a secret.
        let unstripped = snap(vec![text.clone(), history_off.clone(), exclude.clone()]);
        assert_eq!(snapshot_privacy(&unstripped, &ids), Some(Privacy::Secret));

        // Private data (cloud upload off): our exclusion marker goes, the private flags stay → still Private.
        let mut formats = vec![text.clone(), cloud_off.clone(), history_off.clone(), exclude.clone()];
        strip_own_markers(&mut formats, Some(Privacy::Private), &ids);
        assert_eq!(formats, vec![text.clone(), cloud_off, history_off.clone()]);
        assert_eq!(snapshot_privacy(&snap(formats), &ids), Some(Privacy::Private));

        // A secret keeps everything (its exclusion marker may be the password manager's own).
        let mut formats = vec![text.clone(), exclude.clone(), history_off.clone()];
        strip_own_markers(&mut formats, Some(Privacy::Secret), &ids);
        assert_eq!(formats.len(), 3);
        assert_eq!(snapshot_privacy(&snap(formats), &ids), Some(Privacy::Secret));

        // Our clear after a refused secret (markers only): an empty, complete snapshot.
        let mut formats = vec![history_off, exclude];
        strip_own_markers(&mut formats, None, &ids);
        assert!(formats.is_empty());

        // Unregistered ids (0) are never mistaken for ours.
        let zero = PrivacyIds { markers: [0, 0], flags: [0, 0], history_flag: 0, exclude_marker: 0 };
        let mut formats = vec![(0, vec![]), text.clone()];
        strip_own_markers(&mut formats, None, &zero);
        assert_eq!(formats, vec![(0, vec![]), text]);
    }

    // ───────────── desktop tests: `cargo test -p vox-win clipboard -- --ignored --test-threads=1` ─────────────
    // They replace the clipboard content of the interactive session (it is restored at the end where possible).

    #[test]
    #[ignore = "uses the interactive session's clipboard"]
    fn text_round_trip_and_sequence_numbers() {
        let before = snapshot();
        let seq = sequence_number();
        write_clipboard_text("Vox clipboard test ✓").expect("write");
        assert_ne!(sequence_number(), seq);
        assert!(has_text_format());
        assert_eq!(read_clipboard_text().as_deref(), Some("Vox clipboard test ✓"));
        assert_eq!(privacy(), None);
        if !before.failed {
            restore(&before).expect("restore");
        }
    }

    #[test]
    #[ignore = "uses the interactive session's clipboard"]
    fn snapshot_restore_round_trip_with_files_and_markers() {
        let before = snapshot();
        // A CF_HDROP for C:\Windows\notepad.exe plus text, written through the owner window.
        let mut dropfiles = Vec::new();
        dropfiles.extend_from_slice(&20u32.to_le_bytes()); // pFiles
        dropfiles.extend_from_slice(&[0u8; 8]); // pt
        dropfiles.extend_from_slice(&0u32.to_le_bytes()); // fNC
        dropfiles.extend_from_slice(&1u32.to_le_bytes()); // fWide
        for u in "C:\\Windows\\notepad.exe".encode_utf16().chain([0, 0]) {
            dropfiles.extend_from_slice(&u.to_le_bytes());
        }
        let text: Vec<u8> = "files".encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let source = ClipboardSnapshot {
            formats: vec![(CF_UNICODETEXT, text), (CF_HDROP, dropfiles.clone())],
            incomplete: false,
            failed: false,
        };
        restore(&source).expect("restore files");
        let snap = snapshot();
        assert!(!snap.failed, "{snap:?}");
        assert!(snap.formats.iter().any(|(id, b)| *id == CF_HDROP && b.starts_with(&dropfiles)));
        // The restore carried the "do not record" markers, yet our own write is not a secret.
        let ids = privacy_ids();
        assert!(format_available(ids.exclude_marker));
        assert!(format_available(ids.history_flag));
        assert_eq!(privacy(), None);
        assert_eq!(scan_privacy(), Some(Privacy::Secret), "the raw scan sees the marker");
        // A snapshot of our own restore leaves our markers out, and restoring it again is still not a secret.
        let again = snapshot();
        let order: Vec<u32> = again.formats.iter().map(|(id, _)| *id).collect();
        assert!(!order.iter().any(|id| *id == ids.exclude_marker || *id == ids.history_flag), "{order:?}");
        let at = |f: u32| order.iter().position(|id| *id == f).unwrap_or(usize::MAX);
        assert!(at(CF_UNICODETEXT) < at(CF_HDROP) && at(CF_HDROP) != usize::MAX, "owner's order kept: {order:?}");
        restore(&again).expect("restore again");
        assert_eq!(privacy(), None);
        if !before.failed {
            restore(&before).expect("restore the user's clipboard");
        }
    }
}
