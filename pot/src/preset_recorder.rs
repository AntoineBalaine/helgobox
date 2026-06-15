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
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use xcap::Window;

const POLL_INTERVAL: Duration = Duration::from_millis(3);
const MAX_RECORD: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickButton {
    Left,
    Right,
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
    let macro_events = session
        .events
        .lock()
        .map(|e| e.clone())
        .unwrap_or_default();
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
    let start = Instant::now();
    let mut last_event = start;
    let mut prev_left = false;
    let mut prev_right = false;
    let mut prev_keys: HashSet<Keycode> = HashSet::new();

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
            let delay_ms = stamp(&mut last_event);
            let ev = resolve_click(button, x, y, delay_ms);
            if let Ok(mut e) = events.lock() {
                e.push(ev);
            }
        }

        let keys: HashSet<Keycode> = device.get_keys().into_iter().collect();
        // Escape ends recording (and is not itself recorded).
        if keys.contains(&Keycode::Escape) && !prev_keys.contains(&Keycode::Escape) {
            break;
        }
        for k in keys.difference(&prev_keys) {
            if let Some(raw) = keycode_to_raw(*k) {
                let delay_ms = stamp(&mut last_event);
                if let Ok(mut e) = events.lock() {
                    e.push(InputEvent::KeyDown { raw, delay_ms });
                }
            }
        }
        for k in prev_keys.difference(&keys) {
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
    running.store(false, Ordering::Relaxed);
}

fn resolve_click(button: ClickButton, x: i32, y: i32, delay_ms: u64) -> InputEvent {
    let (win_id, off_x, off_y) = topmost_window_at(x, y)
        .map(|w| (w.id(), x - w.x(), y - w.y()))
        .unwrap_or((0, 0, 0));
    InputEvent::Click {
        button,
        win_id,
        off_x,
        off_y,
        abs_x: x,
        abs_y: y,
        delay_ms,
    }
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
