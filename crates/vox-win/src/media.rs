//! System Media Transport Controls (reader-playback.md §9): hardware media keys and the Windows media flyout. Runs
//! on its own STA thread with a hidden window (`ISystemMediaTransportControlsInterop::GetForWindow`). Enabled live
//! from `triggers.mediaKeys` (decision: no restart needed, the restart toast is dropped).
//!
//! What the flyout shows is decided by the pure `MediaModel` (tested): the session is listed only while media keys
//! are on **and** [`MediaState::active`]; metadata is pushed when the session becomes active or its title/voice
//! change; the playback status is `Playing` for `playing`, `Paused` otherwise; the timeline (when the duration is
//! known) is refreshed when the status, the rate, `floor(position)` or `round(duration)` change, so the position is
//! sent at most once per second during playback (the OS extrapolates in between). Inactive → status `Closed`,
//! display cleared, controls disabled (Windows drops the entry).
//!
//! Buttons (called on a WinRT thread, under `catch_unwind`, only while active): Play, Pause, Stop, Next → next
//! sentence, Previous → previous sentence, FastForward / Rewind → `SeekBy(±10)`, a timeline drag →
//! `SeekTo(seconds)`. The handler only enqueues (`Command::Media`).

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{JoinHandle, ThreadId};

use crossbeam_channel::{Receiver, Sender};
use windows::core::{w, HSTRING};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, PostMessageW, PostQuitMessage, WM_APP, WM_CLOSE, WM_DESTROY,
};

use crate::window::{create_hidden_window, hwnd_from, hwnd_raw, pump_messages, register_class, ComGuard, Hwnd};

/// A media-key / flyout action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaAction {
    Play,
    Pause,
    /// = the widget's Close (stops playback and synthesis, hides the widget).
    Stop,
    /// Previous sentence.
    Previous,
    /// Next sentence.
    Next,
    SeekTo(f64),
    /// Seconds (negative = back); default 10.
    SeekBy(f64),
}

/// What the flyout shows.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaState {
    /// mediaKeys on, a session exists and status ≠ error.
    pub active: bool,
    pub playing: bool,
    /// `session.title || "Vox"`.
    pub title: String,
    /// Voice name.
    pub artist: String,
    /// `APP_NAME`.
    pub album: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub rate: f64,
}

pub type MediaHandler = Arc<dyn Fn(MediaAction) + Send + Sync>;

/// Posted to the media window whenever a request was queued.
const WM_APP_REQUEST: u32 = WM_APP + 0x4D;
/// The seek step of FastForward / Rewind (reader-playback.md §9 `seekOffset || 10`).
const SEEK_STEP_SEC: f64 = 10.0;

pub struct MediaSession {
    requests: Sender<Request>,
    hwnd: Hwnd,
    thread: parking_lot::Mutex<Option<JoinHandle<()>>>,
    thread_id: ThreadId,
}

enum Request {
    Enabled(bool),
    Update(MediaState),
    Stop,
}

impl MediaSession {
    pub fn spawn(on_action: MediaHandler) -> Result<MediaSession, crate::WinError> {
        let (requests, rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<Hwnd, crate::WinError>>(1);
        let thread = std::thread::Builder::new()
            .name("vox-media-session".into())
            .spawn(move || media_thread(on_action, rx, ready_tx))
            .map_err(|e| crate::WinError(format!("cannot start the media session thread: {e}")))?;
        let thread_id = thread.thread().id();
        match ready_rx.recv() {
            Ok(Ok(hwnd)) => {
                Ok(MediaSession { requests, hwnd, thread: parking_lot::Mutex::new(Some(thread)), thread_id })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(crate::WinError("the media session thread ended before starting".into()))
            }
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.send(Request::Enabled(enabled));
    }

    /// Pushes metadata/state; position is refreshed at most once per second unless status/rate changed.
    pub fn update(&self, state: MediaState) {
        self.send(Request::Update(state));
    }

    /// Removes the session from the flyout and ends the thread. Idempotent.
    pub fn shutdown(&self) {
        let Some(thread) = self.thread.lock().take() else { return };
        if !self.send(Request::Stop) {
            return;
        }
        if std::thread::current().id() == self.thread_id {
            return;
        }
        if thread.join().is_err() {
            tracing::warn!(target: "media", "media session thread panicked while stopping");
        }
    }

    /// Never blocks: queues and wakes the media thread.
    fn send(&self, request: Request) -> bool {
        if self.requests.send(request).is_err() {
            return false;
        }
        // SAFETY: posting to our own window; fails only once it is destroyed.
        unsafe { PostMessageW(Some(hwnd_from(self.hwnd)), WM_APP_REQUEST, WPARAM(0), LPARAM(0)) }.is_ok()
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One change to apply to the SMTC.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    /// List the session (controls enabled).
    Activate,
    /// Drop the session: status `Closed`, display cleared, controls disabled.
    Deactivate,
    Metadata {
        title: String,
        artist: String,
        album: String,
    },
    Status {
        playing: bool,
    },
    Timeline {
        position: f64,
        duration: f64,
        rate: f64,
    },
    ClearTimeline,
}

/// What was last pushed to the SMTC.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    metadata: Option<(String, String, String)>,
    playing: Option<bool>,
    timeline: Option<TimelineKey>,
}

/// The timeline refresh key (reader-playback.md §9 "Position state").
#[derive(Debug, Clone, Copy, PartialEq)]
enum TimelineKey {
    Cleared,
    Known { position_floor: i64, duration_round: i64, rate_bits: u64, playing: bool },
}

/// The pure "what to push" machine (module docs).
#[derive(Debug, Default)]
struct MediaModel {
    enabled: bool,
    last: Option<MediaState>,
    shown: Option<Shown>,
}

impl MediaModel {
    fn set_enabled(&mut self, enabled: bool) -> Vec<Op> {
        self.enabled = enabled;
        self.reconcile()
    }

    fn update(&mut self, state: MediaState) -> Vec<Op> {
        self.last = Some(state);
        self.reconcile()
    }

    fn is_active(&self) -> bool {
        self.shown.is_some()
    }

    fn reconcile(&mut self) -> Vec<Op> {
        let state = match &self.last {
            Some(s) if self.enabled && s.active => s.clone(),
            _ => {
                return if self.shown.take().is_some() { vec![Op::Deactivate] } else { Vec::new() };
            }
        };
        let mut ops = Vec::new();
        let shown = self.shown.get_or_insert_with(|| {
            ops.push(Op::Activate);
            Shown { metadata: None, playing: None, timeline: None }
        });
        let metadata = (state.title.clone(), state.artist.clone(), state.album.clone());
        if shown.metadata.as_ref() != Some(&metadata) {
            ops.push(Op::Metadata { title: metadata.0.clone(), artist: metadata.1.clone(), album: metadata.2.clone() });
            shown.metadata = Some(metadata);
        }
        if shown.playing != Some(state.playing) {
            ops.push(Op::Status { playing: state.playing });
            shown.playing = Some(state.playing);
        }
        let rate = if state.rate.is_finite() && state.rate > 0.0 { state.rate } else { 1.0 };
        let duration = state.duration_sec;
        let known = duration.is_finite() && duration > 0.0;
        let position =
            if known && state.position_sec.is_finite() { state.position_sec.clamp(0.0, duration) } else { 0.0 };
        let key = if known {
            TimelineKey::Known {
                position_floor: position.floor() as i64,
                duration_round: duration.round() as i64,
                rate_bits: rate.to_bits(),
                playing: state.playing,
            }
        } else {
            TimelineKey::Cleared
        };
        if shown.timeline != Some(key) {
            ops.push(if known { Op::Timeline { position, duration, rate } } else { Op::ClearTimeline });
            shown.timeline = Some(key);
        }
        ops
    }
}

/// SMTC button → action.
fn action_for_button(button: SystemMediaTransportControlsButton) -> Option<MediaAction> {
    Some(match button {
        SystemMediaTransportControlsButton::Play => MediaAction::Play,
        SystemMediaTransportControlsButton::Pause => MediaAction::Pause,
        SystemMediaTransportControlsButton::Stop => MediaAction::Stop,
        SystemMediaTransportControlsButton::Next => MediaAction::Next,
        SystemMediaTransportControlsButton::Previous => MediaAction::Previous,
        SystemMediaTransportControlsButton::FastForward => MediaAction::SeekBy(SEEK_STEP_SEC),
        SystemMediaTransportControlsButton::Rewind => MediaAction::SeekBy(-SEEK_STEP_SEC),
        _ => return None,
    })
}

/// Seconds → `TimeSpan` (100 ns ticks).
fn time_span(seconds: f64) -> TimeSpan {
    let ticks = if seconds.is_finite() { (seconds.max(0.0) * 10_000_000.0).round() } else { 0.0 };
    TimeSpan { Duration: ticks.min(i64::MAX as f64) as i64 }
}

/// `TimeSpan` → seconds (`None` for a negative request).
fn seconds_of(span: TimeSpan) -> Option<f64> {
    (span.Duration >= 0).then(|| span.Duration as f64 / 10_000_000.0)
}

struct ThreadState {
    hwnd: HWND,
    smtc: SystemMediaTransportControls,
    requests: Receiver<Request>,
    model: MediaModel,
    active: Arc<AtomicBool>,
    tokens: (i64, i64),
    warned: bool,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

fn media_thread(on_action: MediaHandler, requests: Receiver<Request>, ready: Sender<Result<Hwnd, crate::WinError>>) {
    crate::window::ensure_mta();
    let _com = ComGuard::new(COINIT_APARTMENTTHREADED);
    let class = w!("VoxMediaSessionWindow");
    let hwnd = match register_class(class, Some(media_proc)).and_then(|_| create_hidden_window(class, "Vox", None)) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let active = Arc::new(AtomicBool::new(false));
    let setup = (|| -> windows::core::Result<(SystemMediaTransportControls, (i64, i64))> {
        let interop = windows::core::factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
        // SAFETY: a live top-level window of this thread.
        let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd)? };
        smtc.SetIsEnabled(false)?;
        smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;
        smtc.SetIsPlayEnabled(true)?;
        smtc.SetIsPauseEnabled(true)?;
        smtc.SetIsStopEnabled(true)?;
        smtc.SetIsNextEnabled(true)?;
        smtc.SetIsPreviousEnabled(true)?;
        smtc.SetIsFastForwardEnabled(true)?;
        smtc.SetIsRewindEnabled(true)?;
        let (handler, live) = (on_action.clone(), active.clone());
        let buttons = smtc.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                if let Some(action) = args.Button().ok().and_then(action_for_button) {
                    dispatch(&handler, &live, action);
                }
            }
            Ok(())
        }))?;
        let (handler, live) = (on_action.clone(), active.clone());
        let seeks = smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<
            SystemMediaTransportControls,
            PlaybackPositionChangeRequestedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                if let Some(seconds) = args.RequestedPlaybackPosition().ok().and_then(seconds_of) {
                    dispatch(&handler, &live, MediaAction::SeekTo(seconds));
                }
            }
            Ok(())
        }))?;
        Ok((smtc, (buttons, seeks)))
    })();
    let (smtc, tokens) = match setup {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: our own window on this thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = ready.send(Err(crate::WinError(format!("media session unavailable: {}", e.message()))));
            return;
        }
    };
    let _ = STATE.try_with(|s| {
        *s.borrow_mut() =
            Some(ThreadState { hwnd, smtc, requests, model: MediaModel::default(), active, tokens, warned: false })
    });
    let _ = ready.send(Ok(hwnd_raw(hwnd)));
    pump_messages();
    let _ = STATE.try_with(|s| {
        if let Some(state) = s.borrow_mut().take() {
            let _ = state.smtc.RemoveButtonPressed(state.tokens.0);
            let _ = state.smtc.RemovePlaybackPositionChangeRequested(state.tokens.1);
        }
    });
}

/// Calls the handler (only while the session is listed), never letting a panic reach WinRT.
fn dispatch(handler: &MediaHandler, active: &AtomicBool, action: MediaAction) {
    if !active.load(Ordering::Relaxed) {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| handler(action))).is_err() {
        tracing::warn!(target: "media", "media action handler panicked ({action:?})");
    }
}

unsafe extern "system" fn media_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_REQUEST => {
            if catch_unwind(AssertUnwindSafe(serve_requests)).is_err() {
                tracing::warn!(target: "media", "media request handling panicked");
            }
            LRESULT(0)
        }
        // An outside close request (taskkill without /f) is handled by the system watcher, which quits the app; this
        // window must stay alive until `shutdown`.
        WM_CLOSE => LRESULT(0),
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn serve_requests() {
    let _ = STATE.try_with(|s| {
        let Ok(mut guard) = s.try_borrow_mut() else { return };
        let Some(state) = guard.as_mut() else { return };
        while let Ok(request) = state.requests.try_recv() {
            let ops = match request {
                Request::Enabled(enabled) => state.model.set_enabled(enabled),
                Request::Update(media) => state.model.update(media),
                Request::Stop => {
                    let mut ops = state.model.set_enabled(false);
                    ops.retain(|op| *op == Op::Deactivate);
                    apply_all(state, &ops);
                    state.active.store(false, Ordering::Relaxed);
                    // SAFETY: destroys our own window on its thread; WM_DESTROY ends the loop.
                    if unsafe { DestroyWindow(state.hwnd) }.is_err() {
                        // SAFETY: ends the loop anyway.
                        unsafe { PostQuitMessage(0) };
                    }
                    return;
                }
            };
            apply_all(state, &ops);
            state.active.store(state.model.is_active(), Ordering::Relaxed);
        }
    });
}

fn apply_all(state: &mut ThreadState, ops: &[Op]) {
    for op in ops {
        if let Err(e) = apply(&state.smtc, op) {
            if !state.warned {
                state.warned = true;
                tracing::warn!(target: "media", "media session update failed: {}", e.message());
            }
        }
    }
}

fn apply(smtc: &SystemMediaTransportControls, op: &Op) -> windows::core::Result<()> {
    match op {
        Op::Activate => smtc.SetIsEnabled(true),
        Op::Deactivate => {
            smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed)?;
            let display = smtc.DisplayUpdater()?;
            display.ClearAll()?;
            display.Update()?;
            update_timeline(smtc, 0.0, 0.0)?;
            smtc.SetIsEnabled(false)
        }
        Op::Metadata { title, artist, album } => {
            let display = smtc.DisplayUpdater()?;
            display.SetType(MediaPlaybackType::Music)?;
            let music = display.MusicProperties()?;
            music.SetTitle(&HSTRING::from(title.as_str()))?;
            music.SetArtist(&HSTRING::from(artist.as_str()))?;
            music.SetAlbumTitle(&HSTRING::from(album.as_str()))?;
            display.Update()
        }
        Op::Status { playing } => {
            smtc.SetPlaybackStatus(if *playing { MediaPlaybackStatus::Playing } else { MediaPlaybackStatus::Paused })
        }
        Op::Timeline { position, duration, rate } => {
            update_timeline(smtc, *position, *duration)?;
            smtc.SetPlaybackRate(*rate)
        }
        Op::ClearTimeline => update_timeline(smtc, 0.0, 0.0),
    }
}

fn update_timeline(smtc: &SystemMediaTransportControls, position: f64, duration: f64) -> windows::core::Result<()> {
    let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
    timeline.SetStartTime(time_span(0.0))?;
    timeline.SetMinSeekTime(time_span(0.0))?;
    timeline.SetEndTime(time_span(duration))?;
    timeline.SetMaxSeekTime(time_span(duration))?;
    timeline.SetPosition(time_span(position))?;
    smtc.UpdateTimelineProperties(&timeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(active: bool, playing: bool, position: f64, duration: f64, rate: f64) -> MediaState {
        MediaState {
            active,
            playing,
            title: "Council minutes".into(),
            artist: "Liam".into(),
            album: "Vox".into(),
            position_sec: position,
            duration_sec: duration,
            rate,
        }
    }

    fn metadata() -> Op {
        Op::Metadata { title: "Council minutes".into(), artist: "Liam".into(), album: "Vox".into() }
    }

    #[test]
    fn nothing_is_listed_until_enabled_and_active() {
        let mut m = MediaModel::default();
        assert_eq!(m.update(state(true, true, 0.0, 10.0, 1.0)), vec![]);
        assert!(!m.is_active());
        let ops = m.set_enabled(true);
        assert_eq!(
            ops,
            vec![
                Op::Activate,
                metadata(),
                Op::Status { playing: true },
                Op::Timeline { position: 0.0, duration: 10.0, rate: 1.0 }
            ]
        );
        assert!(m.is_active());
        // Turning media keys off drops the session at once; on again re-lists it with the last state.
        assert_eq!(m.set_enabled(false), vec![Op::Deactivate]);
        assert_eq!(m.set_enabled(false), vec![]);
        assert_eq!(m.set_enabled(true).first(), Some(&Op::Activate));
    }

    #[test]
    fn position_refreshes_once_per_second_unless_status_or_rate_change() {
        let mut m = MediaModel::default();
        m.set_enabled(true);
        m.update(state(true, true, 1.2, 30.0, 1.0));
        assert_eq!(m.update(state(true, true, 1.7, 30.0, 1.0)), vec![]);
        assert_eq!(
            m.update(state(true, true, 2.05, 30.0, 1.0)),
            vec![Op::Timeline { position: 2.05, duration: 30.0, rate: 1.0 }]
        );
        // Rate change → timeline + rate.
        assert_eq!(
            m.update(state(true, true, 2.1, 30.0, 1.5)),
            vec![Op::Timeline { position: 2.1, duration: 30.0, rate: 1.5 }]
        );
        // Pause → status and timeline.
        assert_eq!(
            m.update(state(true, false, 2.2, 30.0, 1.5)),
            vec![Op::Status { playing: false }, Op::Timeline { position: 2.2, duration: 30.0, rate: 1.5 }]
        );
        // A sub-second duration refinement does not resend; a rounded change does.
        assert_eq!(m.update(state(true, false, 2.2, 30.2, 1.5)), vec![]);
        assert_eq!(
            m.update(state(true, false, 2.2, 31.0, 1.5)),
            vec![Op::Timeline { position: 2.2, duration: 31.0, rate: 1.5 }]
        );
    }

    #[test]
    fn unknown_duration_clears_the_timeline_and_positions_are_clamped() {
        let mut m = MediaModel::default();
        m.set_enabled(true);
        let ops = m.update(state(true, false, 0.0, 0.0, 0.0));
        assert_eq!(ops, vec![Op::Activate, metadata(), Op::Status { playing: false }, Op::ClearTimeline]);
        // Position past the estimate → clamped; rate ≤ 0 → 1.
        assert_eq!(
            m.update(state(true, false, 12.0, 10.0, -1.0)),
            vec![Op::Timeline { position: 10.0, duration: 10.0, rate: 1.0 }]
        );
        assert_eq!(
            m.update(state(true, false, f64::NAN, 10.0, 1.0)),
            vec![Op::Timeline { position: 0.0, duration: 10.0, rate: 1.0 }]
        );
    }

    #[test]
    fn a_new_title_is_pushed_and_inactive_drops_the_session() {
        let mut m = MediaModel::default();
        m.set_enabled(true);
        m.update(state(true, true, 0.0, 10.0, 1.0));
        let mut next = state(true, true, 0.0, 10.0, 1.0);
        next.title = "Tenancy".into();
        assert_eq!(
            m.update(next.clone()),
            vec![Op::Metadata { title: "Tenancy".into(), artist: "Liam".into(), album: "Vox".into() }]
        );
        assert_eq!(m.update(MediaState { active: false, ..next.clone() }), vec![Op::Deactivate]);
        assert_eq!(m.update(MediaState { active: false, ..next }), vec![]);
        assert!(!m.is_active());
    }

    #[test]
    fn buttons_map_to_actions() {
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Play), Some(MediaAction::Play));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Pause), Some(MediaAction::Pause));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Stop), Some(MediaAction::Stop));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Next), Some(MediaAction::Next));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Previous), Some(MediaAction::Previous));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::FastForward), Some(MediaAction::SeekBy(10.0)));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Rewind), Some(MediaAction::SeekBy(-10.0)));
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::Record), None);
        assert_eq!(action_for_button(SystemMediaTransportControlsButton::ChannelUp), None);
    }

    #[test]
    fn time_spans_are_100_ns_ticks() {
        assert_eq!(time_span(1.5).Duration, 15_000_000);
        assert_eq!(time_span(-3.0).Duration, 0);
        assert_eq!(time_span(f64::NAN).Duration, 0);
        assert_eq!(time_span(f64::INFINITY).Duration, 0);
        assert_eq!(seconds_of(TimeSpan { Duration: 25_000_000 }), Some(2.5));
        assert_eq!(seconds_of(TimeSpan { Duration: -1 }), None);
    }

    #[test]
    fn handlers_run_only_while_listed_and_never_unwind() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handler: MediaHandler = Arc::new(move |a| {
            let _ = tx.send(a);
        });
        let live = AtomicBool::new(false);
        dispatch(&handler, &live, MediaAction::Play);
        assert!(rx.try_recv().is_err());
        live.store(true, Ordering::Relaxed);
        dispatch(&handler, &live, MediaAction::SeekTo(3.0));
        assert_eq!(rx.try_recv(), Ok(MediaAction::SeekTo(3.0)));
        let panicking: MediaHandler = Arc::new(|_| panic!("boom"));
        dispatch(&panicking, &live, MediaAction::Stop);
    }

    /// `cargo test -p vox-win smtc_flyout_lists_the_session -- --ignored --nocapture`: lists a fake read in the Windows
    /// media flyout (Win+A / the volume flyout) for 10 s; press the hardware play/pause or next keys to see actions.
    #[test]
    #[ignore = "shows a session in the Windows media flyout"]
    fn smtc_flyout_lists_the_session() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let session = MediaSession::spawn(Arc::new(move |a| {
            let _ = tx.send(a);
        }))
        .expect("SMTC");
        session.set_enabled(true);
        session.update(state(true, true, 3.0, 120.0, 1.0));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while let Ok(a) = rx.recv_deadline(deadline) {
            println!("{a:?}");
        }
        session.shutdown();
        session.shutdown();
    }
}
