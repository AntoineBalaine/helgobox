//! Records and replays a "save as" action macro for the preset crawler.
//!
//! Some plug-ins don't expose preset names to the host (e.g. Serum after a recent update),
//! so the crawler scrapes the name from the plug-in's own "Save Preset As" dialog. The old
//! approach used two fixed screen positions and a blind select-all+copy, which broke on
//! real plug-ins (menus, unfocused fields, in-GUI dialogs). Instead, the user demonstrates
//! the whole flow once — open the dialog, focus the name field, select-all + copy, cancel —
//! and we record it as an ordered macro of clicks and keystrokes, then replay it per preset.
//!
//! Design notes (validated by a standalone PoC):
//! - Clicks are stored **window-relative** (the xcap window under the cursor + offset), so
//!   replay tracks a moved window and, crucially, can **verify** the expected window is
//!   topmost before clicking — never clicking a random window.
//! - Keys are replayed by **physical position** (`enigo::Key::Raw`), not character.
//!   `device_query` reports physical key positions (US-QWERTY-named); replaying the same
//!   physical position reproduces the user's keystroke under any layout (e.g. bépo, where
//!   the QWERTY-H position is 'c', so Cmd+that = Cmd+C = copy). Character-based replay would
//!   send the literal 'h' and fire Cmd+H instead.
//! - Recording runs on a background thread polling `device_query`; it stops on Escape, an
//!   explicit stop, or a timeout.

use device_query::{DeviceQuery, DeviceState, Keycode};
use image::DynamicImage;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use xcap::{Monitor, Window};

const POLL_INTERVAL: Duration = Duration::from_millis(3);
const MAX_RECORD: Duration = Duration::from_secs(120);
/// Max gap + distance for consecutive clicks to count as one multi-click sequence.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DIST: i32 = 6;
/// Size (physical px) of the grayscale screen patch captured around each click for
/// image-verified replay. Big enough to be distinctive (catches a menu item's text/icon),
/// small enough to match fast and tolerate the click sitting near a patch edge.
const PATCH_W: u32 = 120;
const PATCH_H: u32 = 72;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickButton {
    Left,
    Right,
}

/// A small grayscale screen patch captured around a click point at record time, used to
/// re-locate that spot on screen at replay time (template matching). All fields are in the
/// capturing monitor's *physical* pixel space.
#[derive(Clone, Debug)]
pub struct ClickPatch {
    /// Row-major `Luma8` pixels, exactly `w * h` bytes.
    pub bytes: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// Where the actual click point sits inside the patch (normally near the centre, but
    /// clamped when the click was near a screen edge).
    pub click_x: u32,
    pub click_y: u32,
}

/// One recorded input event. Clicks carry both window-relative anchoring (for verified,
/// movement-robust replay) and the absolute fallback. Keys carry the platform raw keycode.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Click {
        button: ClickButton,
        /// xcap window id the click landed in, or 0 if none was resolved.
        win_id: u32,
        off_x: i32,
        off_y: i32,
        abs_x: i32,
        abs_y: i32,
        /// Multi-click count (1 = single, 2 = double, 3 = triple, ...), detected from
        /// timing+position at record time. On macOS this becomes the event's click-state so
        /// a recorded triple-click replays as a real triple-click (needed to select text in
        /// native dialogs); elsewhere replay timing reproduces it.
        clicks: u8,
        /// True if this click landed in a window that had just appeared (a dialog/menu that
        /// opened since the previous click). Drives event-driven replay: replay waits for
        /// that new window to actually appear rather than sleeping the recorded delay.
        wait_for_window: bool,
        /// Grayscale screen patch around the click, for image-verified replay (locate the
        /// patch on screen, then click it). Captured only for the first click of a group
        /// (`clicks == 1`); multi-click continuations reuse the first click's location.
        patch: Option<ClickPatch>,
        delay_ms: u64,
    },
    KeyDown {
        raw: u16,
        delay_ms: u64,
    },
    KeyUp {
        raw: u16,
        delay_ms: u64,
    },
}

/// A recorded save-as action sequence.
pub type RecordedMacro = Vec<InputEvent>;

struct Session {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<InputEvent>>>,
    handle: Option<JoinHandle<()>>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn lock_session() -> std::sync::MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts recording on a background thread. No-op if already recording.
pub fn start_recording() {
    let mut guard = lock_session();
    if guard.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    let events = Arc::new(Mutex::new(Vec::new()));
    let handle = {
        let stop = stop.clone();
        let running = running.clone();
        let events = events.clone();
        std::thread::spawn(move || record_loop(stop, running, events))
    };
    *guard = Some(Session {
        stop,
        running,
        events,
        handle: Some(handle),
    });
}

/// Whether a recording is currently in progress (false once the user pressed Escape or it
/// timed out, even before `stop_recording` is called).
pub fn is_recording() -> bool {
    lock_session()
        .as_ref()
        .map(|s| s.running.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Number of events captured so far (for UI feedback).
pub fn recorded_event_count() -> usize {
    lock_session()
        .as_ref()
        .map(|s| s.events.lock().map(|e| e.len()).unwrap_or(0))
        .unwrap_or(0)
}

/// Stops recording (if any) and returns the captured macro.
pub fn stop_recording() -> RecordedMacro {
    let session = lock_session().take();
    let Some(mut session) = session else {
        return Vec::new();
    };
    session.stop.store(true, Ordering::Relaxed);
    if let Some(handle) = session.handle.take() {
        let _ = handle.join();
    }
    let mut macro_events = session
        .events
        .lock()
        .map(|e| e.clone())
        .unwrap_or_default();
    // The user stops recording by clicking the UI "Stop" button; that click is itself
    // captured, so drop the trailing click.
    if matches!(macro_events.last(), Some(InputEvent::Click { .. })) {
        macro_events.pop();
    }
    macro_events
}

fn stamp(last: &mut Instant) -> u64 {
    let now = Instant::now();
    let d = now.duration_since(*last).as_millis() as u64;
    *last = now;
    d
}

fn record_loop(stop: Arc<AtomicBool>, running: Arc<AtomicBool>, events: Arc<Mutex<Vec<InputEvent>>>) {
    let device = DeviceState::new();
    // Patch capture runs on this dedicated worker, fed click (index, x, y) by the poll loop, so
    // the slow screenshot never blocks input polling (see the click handler). It fills each
    // click's `patch` field by index; we drop the sender and join it on stop so every patch is
    // present before the macro is read.
    let (cap_tx, cap_rx) = std::sync::mpsc::channel::<(usize, i32, i32)>();
    let cap_events = events.clone();
    let cap_handle = std::thread::spawn(move || {
        while let Ok((idx, x, y)) = cap_rx.recv() {
            let patch = capture_click_patch(x, y);
            if patch.is_some() {
                if let Ok(mut e) = cap_events.lock() {
                    if let Some(InputEvent::Click { patch: slot, .. }) = e.get_mut(idx) {
                        *slot = patch;
                    }
                }
            }
        }
    });
    let start = Instant::now();
    let mut last_event = start;
    let mut prev_left = false;
    let mut prev_right = false;
    let mut prev_keys: HashSet<Keycode> = HashSet::new();
    // Tracks the previous click for multi-click detection: (time, x, y, count, button).
    let mut last_click: Option<(Instant, i32, i32, u8, ClickButton)> = None;
    // Window-id set as of the previous click, to detect dialogs/menus that just appeared.
    let mut prev_click_window_ids: HashSet<u32> = Window::all()
        .map(|ws| ws.iter().map(|w| w.id()).collect())
        .unwrap_or_default();

    loop {
        if stop.load(Ordering::Relaxed) || start.elapsed() > MAX_RECORD {
            break;
        }
        let mouse = device.get_mouse();
        let left = *mouse.button_pressed.get(1).unwrap_or(&false);
        let right = *mouse.button_pressed.get(2).unwrap_or(&false);
        let (x, y) = mouse.coords;

        let clicked = if left && !prev_left {
            Some(ClickButton::Left)
        } else if right && !prev_right {
            Some(ClickButton::Right)
        } else {
            None
        };
        if let Some(button) = clicked {
            let now = Instant::now();
            let clicks = match last_click {
                // A continuation must be the *same button*: a right-click right after a
                // left triple-click is a new click, not a 4th press of the group.
                Some((t, lx, ly, c, b))
                    if b == button
                        && now.duration_since(t) < MULTI_CLICK_INTERVAL
                        && (x - lx).abs() <= MULTI_CLICK_DIST
                        && (y - ly).abs() <= MULTI_CLICK_DIST =>
                {
                    c.saturating_add(1)
                }
                _ => 1,
            };
            last_click = Some((now, x, y, clicks, button));
            let delay_ms = stamp(&mut last_event);
            // Resolve the click against the current windows and detect whether it landed in
            // a window that wasn't open at the previous click (a just-appeared dialog/menu).
            let windows = Window::all().unwrap_or_default();
            let current_ids: HashSet<u32> = windows.iter().map(|w| w.id()).collect();
            let target = windows.into_iter().find(|w| {
                let (wx, wy, ww, wh) = (w.x(), w.y(), w.width() as i32, w.height() as i32);
                ww > 0 && wh > 0 && x >= wx && x < wx + ww && y >= wy && y < wy + wh
            });
            let (win_id, off_x, off_y) = target
                .map(|w| (w.id(), x - w.x(), y - w.y()))
                .unwrap_or((0, 0, 0));
            let wait_for_window = win_id != 0 && !prev_click_window_ids.contains(&win_id);
            prev_click_window_ids = current_ids;
            let ev = InputEvent::Click {
                button,
                win_id,
                off_x,
                off_y,
                abs_x: x,
                abs_y: y,
                clicks,
                wait_for_window,
                // Filled in off-thread by the capture worker (see below).
                patch: None,
                delay_ms,
            };
            let idx = if let Ok(mut e) = events.lock() {
                let idx = e.len();
                e.push(ev);
                Some(idx)
            } else {
                None
            };
            // Capture the screen patch for image-verified replay on the worker thread, never
            // inline: capture_image() takes hundreds of ms, and blocking this poll loop that
            // long would miss the fast successive presses of a double/triple-click (collapsing
            // it to a single click). Only the first click of a group needs a patch —
            // continuations re-click its location at replay.
            if clicks == 1 {
                if let Some(idx) = idx {
                    let _ = cap_tx.send((idx, x, y));
                }
            }
        }

        let keys: HashSet<Keycode> = device.get_keys().into_iter().collect();
        // Escape is ignored entirely: it's not recorded (replaying it would close plug-in
        // windows) and no longer stops recording (Esc closed the focused FX window, breaking
        // the flow). Recording stops via the UI "Stop" button (the `stop` flag) or timeout.
        for k in keys.difference(&prev_keys) {
            if *k == Keycode::Escape {
                continue;
            }
            if let Some(raw) = keycode_to_raw(*k) {
                let delay_ms = stamp(&mut last_event);
                if let Ok(mut e) = events.lock() {
                    e.push(InputEvent::KeyDown { raw, delay_ms });
                }
            }
        }
        for k in prev_keys.difference(&keys) {
            if *k == Keycode::Escape {
                continue;
            }
            if let Some(raw) = keycode_to_raw(*k) {
                let delay_ms = stamp(&mut last_event);
                if let Ok(mut e) = events.lock() {
                    e.push(InputEvent::KeyUp { raw, delay_ms });
                }
            }
        }

        prev_left = left;
        prev_right = right;
        prev_keys = keys;
        std::thread::sleep(POLL_INTERVAL);
    }
    // Stop feeding the capture worker and wait for in-flight patches to finish, so the macro is
    // complete (all patches attached) before stop_recording() reads it.
    drop(cap_tx);
    let _ = cap_handle.join();
    running.store(false, Ordering::Relaxed);
}

/// Captures a grayscale screen patch around the logical click point `(x, y)`, for
/// image-verified replay. Converts the logical cursor coordinate into the capturing monitor's
/// physical pixel space (via its scale factor), crops a [`PATCH_W`]×[`PATCH_H`] region centred
/// on it (clamped at screen edges), and stores where the click sits inside that crop. Returns
/// `None` if the monitor can't be found/captured or the usable crop is too small to match on.
pub fn capture_click_patch(x: i32, y: i32) -> Option<ClickPatch> {
    let monitor = Monitor::from_point(x, y).ok()?;
    let scale = monitor.scale_factor().max(0.01);
    let gray = DynamicImage::ImageRgba8(monitor.capture_image().ok()?).into_luma8();
    let (iw, ih) = (gray.width() as i32, gray.height() as i32);
    // Click point in the monitor image's physical pixels.
    let px = (((x - monitor.x()) as f32) * scale).round() as i32;
    let py = (((y - monitor.y()) as f32) * scale).round() as i32;
    // Crop rectangle, centred on the click and clamped to the image.
    let left = (px - PATCH_W as i32 / 2).clamp(0, (iw - 1).max(0));
    let top = (py - PATCH_H as i32 / 2).clamp(0, (ih - 1).max(0));
    let w = PATCH_W.min((iw - left).max(0) as u32);
    let h = PATCH_H.min((ih - top).max(0) as u32);
    if w < 16 || h < 16 {
        return None;
    }
    let patch = image::imageops::crop_imm(&gray, left as u32, top as u32, w, h).to_image();
    Some(ClickPatch {
        bytes: patch.into_raw(),
        w,
        h,
        click_x: (px - left).clamp(0, w as i32 - 1) as u32,
        click_y: (py - top).clamp(0, h as i32 - 1) as u32,
    })
}

/// The topmost window containing the point. xcap returns windows front-to-back, so the
/// first containing window is the one the click hits.
pub fn topmost_window_at(x: i32, y: i32) -> Option<Window> {
    let windows = Window::all().ok()?;
    windows.into_iter().find(|w| {
        let (wx, wy, ww, wh) = (w.x(), w.y(), w.width() as i32, w.height() as i32);
        ww > 0 && wh > 0 && x >= wx && x < wx + ww && y >= wy && y < wy + wh
    })
}

/// device_query Keycode -> platform virtual keycode for `enigo::Key::Raw`.
///
/// Both name the same physical key POSITIONS, so replaying the raw code presses the same
/// physical key the user pressed, independent of their keyboard layout. macOS + Linux/X11
/// only; other targets return None (the host-API name path still works there).
#[cfg(target_os = "macos")]
fn keycode_to_raw(code: Keycode) -> Option<u16> {
    use Keycode::*;
    Some(match code {
        A => 0x00, B => 0x0B, C => 0x08, D => 0x02, E => 0x0E, F => 0x03, G => 0x05,
        H => 0x04, I => 0x22, J => 0x26, K => 0x28, L => 0x25, M => 0x2E, N => 0x2D,
        O => 0x1F, P => 0x23, Q => 0x0C, R => 0x0F, S => 0x01, T => 0x11, U => 0x20,
        V => 0x09, W => 0x0D, X => 0x07, Y => 0x10, Z => 0x06,
        Key1 => 0x12, Key2 => 0x13, Key3 => 0x14, Key4 => 0x15, Key5 => 0x17,
        Key6 => 0x16, Key7 => 0x1A, Key8 => 0x1C, Key9 => 0x19, Key0 => 0x1D,
        Meta => 0x37, LShift => 0x38, RShift => 0x3C, LControl => 0x3B, RControl => 0x3E,
        LAlt => 0x3A, RAlt => 0x3D,
        Enter => 0x24, Tab => 0x30, Space => 0x31, Backspace => 0x33,
        Delete => 0x75, Home => 0x73, End => 0x77, Left => 0x7B, Right => 0x7C,
        Down => 0x7D, Up => 0x7E,
        Grave => 0x32, Minus => 0x1B, Equal => 0x18, LeftBracket => 0x21,
        RightBracket => 0x1E, BackSlash => 0x2A, Semicolon => 0x29, Apostrophe => 0x27,
        Comma => 0x2B, Dot => 0x2F, Slash => 0x2C,
        _ => return None,
    })
}

/// X11 keycodes (US layout positions; `keycode = evdev scancode + 8`). The X server maps
/// these through the active layout on replay, so it's layout-independent like macOS.
#[cfg(target_os = "linux")]
fn keycode_to_raw(code: Keycode) -> Option<u16> {
    use Keycode::*;
    Some(match code {
        A => 38, B => 56, C => 54, D => 40, E => 26, F => 41, G => 42, H => 43,
        I => 31, J => 44, K => 45, L => 46, M => 58, N => 57, O => 32, P => 33,
        Q => 24, R => 27, S => 39, T => 28, U => 30, V => 55, W => 25, X => 53,
        Y => 29, Z => 52,
        Key1 => 10, Key2 => 11, Key3 => 12, Key4 => 13, Key5 => 14, Key6 => 15,
        Key7 => 16, Key8 => 17, Key9 => 18, Key0 => 19,
        LShift => 50, RShift => 62, LControl => 37, RControl => 105, LAlt => 64,
        RAlt => 108, Meta => 133,
        Enter => 36, Tab => 23, Space => 65, Backspace => 22,
        Delete => 119, Home => 110, End => 115, Left => 113, Right => 114,
        Down => 116, Up => 111,
        Grave => 49, Minus => 20, Equal => 21, LeftBracket => 34, RightBracket => 35,
        BackSlash => 51, Semicolon => 47, Apostrophe => 48, Comma => 59, Dot => 60,
        Slash => 61,
        _ => return None,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn keycode_to_raw(_code: Keycode) -> Option<u16> {
    None
}
