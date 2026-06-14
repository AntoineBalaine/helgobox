//! Standalone control surface over pot's preview recorder (`pot::preview_recorder`).
//!
//! Gathering the preset list can be slow, so it runs on the pot worker thread and the
//! result is handed back to the main thread via a global slot (polled from
//! [`crate::executor::run_tasks`]). The actual recording (`record_previews`) holds REAPER
//! handles, so it runs as a `!Send` future on the main thread.

use crate::standalone_unit::standalone_pot_unit;
use base::{blocking_lock_arc, blocking_read_lock, Global};
use camino::Utf8PathBuf;
use pot::preview_recorder::{
    prepare_preview_recording, record_previews, ExportPreviewOutputConfig, PreviewOutputConfig,
    PreviewRecorderState, RecordPreviewsArgs, SharedPreviewRecorderState,
};
use pot::{spawn_in_pot_worker, PresetWithId};
use reaper_high::Reaper;
use std::cell::RefCell;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Coarse wizard phase. The numeric codes are part of the `HB_Pot_*` ABI.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecPhase {
    Idle = 0,
    Preparing = 1,
    Ready = 2,
    Recording = 3,
    Done = 4,
    Failed = 5,
}

struct RecorderSession {
    config: PreviewOutputConfig,
    phase: RecPhase,
    /// Presets to record, available between prepare (Ready) and start.
    prepared: Option<Vec<PresetWithId>>,
    prepared_count: i32,
    /// Shared with the running record future; polled for live progress.
    state: Option<SharedPreviewRecorderState>,
    export_dir: Option<Utf8PathBuf>,
    error: Option<String>,
}

thread_local! {
    static RECORDER: RefCell<Option<RecorderSession>> = const { RefCell::new(None) };
}

/// Worker -> main handoff for the (potentially slow) preset gathering.
struct PrepareSlot {
    pending: bool,
    presets: Option<Vec<PresetWithId>>,
    error: Option<String>,
}

static PREPARE: LazyLock<Mutex<PrepareSlot>> = LazyLock::new(|| {
    Mutex::new(PrepareSlot {
        pending: false,
        presets: None,
        error: None,
    })
});

fn prepare_slot() -> MutexGuard<'static, PrepareSlot> {
    PREPARE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Built-in preview RPP template, embedded at compile time and materialized once into the
/// REAPER resource dir so the renderer can open it.
static TEMPLATE_PATH: LazyLock<Option<Utf8PathBuf>> = LazyLock::new(|| {
    let bytes: &[u8] =
        include_bytes!("../../resources/template-projects/pot-preview/pot-preview.RPP");
    let dir = Reaper::get().resource_path().join("Helgoboss/Pot");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("pot-preview-template.RPP");
    std::fs::write(&path, bytes).ok()?;
    Some(path)
});

fn builtin_template_path() -> Option<Utf8PathBuf> {
    TEMPLATE_PATH.clone()
}

fn export_base_dir() -> Utf8PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Reaper::get()
        .resource_path()
        .join("Helgoboss/SoundPot/Preview Exports")
        .join(stamp.to_string())
}

/// Gathers the presets to record. `mode`: 0 = record for Pot Browser playback, 1 = export.
/// Returns 1 if preparation started, 0 otherwise. Poll the phase for completion.
pub fn prepare(mode: i32) -> i32 {
    reaper_low::firewall(|| {
        let busy = RECORDER.with(|c| {
            matches!(
                c.borrow().as_ref().map(|s| s.phase),
                Some(RecPhase::Preparing | RecPhase::Recording)
            )
        });
        if busy {
            return 0;
        }
        let config = match mode {
            1 => PreviewOutputConfig::Export(ExportPreviewOutputConfig {
                base_dir: export_base_dir(),
            }),
            _ => PreviewOutputConfig::ForPotBrowserPlayback,
        };
        let Ok(shared) = standalone_pot_unit() else {
            return 0;
        };
        let build_input = blocking_lock_arc(&shared, "recorder build input").create_build_input();
        {
            let mut slot = prepare_slot();
            slot.pending = true;
            slot.presets = None;
            slot.error = None;
        }
        let export_dir = match &config {
            PreviewOutputConfig::Export(c) => Some(c.base_dir.clone()),
            PreviewOutputConfig::ForPotBrowserPlayback => None,
        };
        RECORDER.with(|c| {
            *c.borrow_mut() = Some(RecorderSession {
                config: config.clone(),
                phase: RecPhase::Preparing,
                prepared: None,
                prepared_count: 0,
                state: None,
                export_dir,
                error: None,
            });
        });
        spawn_in_pot_worker(async move {
            let presets = prepare_preview_recording(build_input, &config);
            let mut slot = prepare_slot();
            slot.presets = Some(presets);
            slot.pending = false;
            Ok(())
        });
        1
    })
    .unwrap_or(0)
}

/// Moves a finished prepare result from the worker slot into the session. Called each
/// frame from [`crate::executor::run_tasks`].
pub fn poll() {
    reaper_low::firewall(|| {
        let transition = {
            let mut slot = prepare_slot();
            if slot.pending {
                None
            } else if let Some(e) = slot.error.take() {
                Some(Err(e))
            } else {
                slot.presets.take().map(Ok)
            }
        };
        let Some(result) = transition else {
            return;
        };
        RECORDER.with(|c| {
            let mut cell = c.borrow_mut();
            let Some(s) = cell.as_mut() else {
                return;
            };
            if s.phase != RecPhase::Preparing {
                return;
            }
            match result {
                Ok(presets) => {
                    s.prepared_count = presets.len() as i32;
                    s.prepared = Some(presets);
                    s.phase = RecPhase::Ready;
                }
                Err(e) => {
                    s.error = Some(e);
                    s.phase = RecPhase::Failed;
                }
            }
        });
    });
}

fn set_failed(msg: String) {
    RECORDER.with(|c| {
        if let Some(s) = c.borrow_mut().as_mut() {
            s.error = Some(msg);
            s.phase = RecPhase::Failed;
        }
    });
}

fn resolve_preview_rpp(config: &PreviewOutputConfig) -> Result<Utf8PathBuf, String> {
    let template = builtin_template_path().ok_or_else(|| "preview template unavailable".to_string())?;
    match config {
        PreviewOutputConfig::ForPotBrowserPlayback => Ok(template),
        PreviewOutputConfig::Export(c) => {
            let parent = c
                .base_dir
                .parent()
                .ok_or_else(|| "export base dir has no parent".to_string())?;
            let custom = parent.join("pot-preview.RPP");
            if custom.exists() {
                Ok(custom)
            } else {
                let _ = std::fs::create_dir_all(parent);
                std::fs::copy(&template, &custom).map_err(|e| e.to_string())?;
                Err(format!(
                    "A customizable preview template was created at {custom}. Open it in \
                     REAPER, adjust the render settings, save it, then start recording again."
                ))
            }
        }
    }
}

/// Starts recording the prepared presets. Returns 1 if started, 0 otherwise.
pub fn start() -> i32 {
    reaper_low::firewall(|| {
        let prep = RECORDER.with(|c| {
            let mut cell = c.borrow_mut();
            let s = cell.as_mut()?;
            if s.phase != RecPhase::Ready {
                return None;
            }
            let presets = s.prepared.take()?;
            Some((presets, s.config.clone()))
        });
        let Some((presets, config)) = prep else {
            return 0;
        };
        let rpp = match resolve_preview_rpp(&config) {
            Ok(p) => p,
            Err(e) => {
                set_failed(e);
                return 0;
            }
        };
        let Ok(shared_pot_unit) = standalone_pot_unit() else {
            set_failed("no pot unit available".to_string());
            return 0;
        };
        let state: SharedPreviewRecorderState =
            Arc::new(RwLock::new(PreviewRecorderState::new(presets)));
        RECORDER.with(|c| {
            if let Some(s) = c.borrow_mut().as_mut() {
                s.state = Some(state.clone());
                s.phase = RecPhase::Recording;
            }
        });
        Global::future_support().spawn_in_main_thread_from_main_thread(async move {
            // `rpp` is owned by this future; borrowing it here is valid for the await.
            let result = {
                let args = RecordPreviewsArgs {
                    shared_pot_unit,
                    state,
                    preview_rpp: &rpp,
                    config,
                };
                record_previews(args).await
            };
            finish_record(result);
            Ok(())
        });
        1
    })
    .unwrap_or(0)
}

fn finish_record(result: Result<(), Box<dyn std::error::Error>>) {
    RECORDER.with(|c| {
        if let Some(s) = c.borrow_mut().as_mut() {
            match result {
                Ok(()) => s.phase = RecPhase::Done,
                Err(e) => {
                    s.error = Some(e.to_string());
                    s.phase = RecPhase::Failed;
                }
            }
        }
    });
    // Make the freshly recorded previews show up (has-preview flags, etc.).
    if let Ok(shared) = standalone_pot_unit() {
        let mut unit = blocking_lock_arc(&shared, "recorder refresh");
        unit.refresh_pot(shared.clone());
    }
}

/// Discards the current recorder session.
pub fn discard() {
    reaper_low::firewall(|| {
        RECORDER.with(|c| *c.borrow_mut() = None);
        let mut slot = prepare_slot();
        slot.pending = false;
        slot.presets = None;
        slot.error = None;
    });
}

pub fn phase_code() -> i32 {
    RECORDER
        .with(|c| c.borrow().as_ref().map(|s| s.phase as i32))
        .unwrap_or(RecPhase::Idle as i32)
}

pub fn is_running() -> bool {
    RECORDER.with(|c| {
        matches!(
            c.borrow().as_ref().map(|s| s.phase),
            Some(RecPhase::Preparing | RecPhase::Recording)
        )
    })
}

pub fn prepared_count() -> i32 {
    RECORDER
        .with(|c| c.borrow().as_ref().map(|s| s.prepared_count))
        .unwrap_or(0)
}

pub fn todo_count() -> i32 {
    RECORDER
        .with(|c| {
            let cell = c.borrow();
            let s = cell.as_ref()?;
            let state = s.state.as_ref()?;
            let count = blocking_read_lock(state, "recorder todo_count").todos.len() as i32;
            Some(count)
        })
        .unwrap_or(-1)
}

pub fn failure_count() -> i32 {
    RECORDER
        .with(|c| {
            let cell = c.borrow();
            let s = cell.as_ref()?;
            let state = s.state.as_ref()?;
            let count = blocking_read_lock(state, "recorder failure_count").failures.len() as i32;
            Some(count)
        })
        .unwrap_or(0)
}

/// Writes the preset name of the i-th failure into `f`. Returns its result if present.
pub fn with_failure_name<R>(i: i32, f: impl FnOnce(&str) -> R) -> Option<R> {
    RECORDER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let state = s.state.as_ref()?;
        let guard = blocking_read_lock(state, "recorder failure_name");
        let failure = guard.failures.get(usize::try_from(i).ok()?)?;
        Some(f(&failure.preset.preset.common.name))
    })
}

/// Writes the reason of the i-th failure into `f`. Returns its result if present.
pub fn with_failure_reason<R>(i: i32, f: impl FnOnce(&str) -> R) -> Option<R> {
    RECORDER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let state = s.state.as_ref()?;
        let guard = blocking_read_lock(state, "recorder failure_reason");
        let failure = guard.failures.get(usize::try_from(i).ok()?)?;
        Some(f(&failure.reason))
    })
}

/// Writes the export directory into `f`, if this was an export session. Returns its result.
pub fn with_export_dir<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    RECORDER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let dir = s.export_dir.as_ref()?;
        Some(f(dir.as_str()))
    })
}

/// Writes the last error message into `f`, if any. Returns its result.
pub fn with_error<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    RECORDER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let e = s.error.as_deref()?;
        Some(f(e))
    })
}
