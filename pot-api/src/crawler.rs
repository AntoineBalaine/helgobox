//! Standalone control surface over pot's preset crawler (`pot::preset_crawler`).
//!
//! The crawl loop, mouse automation, and "Save Preset As" scraping stay in Rust
//! (`pot/src/preset_crawler.rs`); this module only drives a session — start, poll, then
//! import-or-discard — so a script UI can present the wizard. Screen positions for the
//! plug-in's "Next preset" button (and optionally its "Save Preset As" / "Cancel"
//! buttons) are captured by the UI and passed in as native coordinates.
//!
//! The crawl runs as a `!Send` future on the main thread (it holds an `Fx`), spawned via
//! `spawn_in_main_thread_from_main_thread`, and is pumped by [`crate::executor::run_tasks`].

use crate::standalone_unit::standalone_pot_unit;
use base::{blocking_lock_arc, Global, MouseCursorPosition};
use pot::preset_crawler::{
    crawl_presets, import_crawled_presets, CrawlPresetArgs, PresetCrawlingOutcome,
    PresetCrawlingState, PresetCrawlerStopReason, SaveAsDialogScraping, SharedPresetCrawlingState,
};
use reaper_high::Reaper;
use std::cell::RefCell;
use std::error::Error;
use std::fs::File;

/// Coarse wizard phase. The numeric codes are part of the `HB_Pot_*` ABI.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CrawlPhase {
    Idle = 0,
    Running = 1,
    Stopped = 2,
    Importing = 3,
    Done = 4,
    Failed = 5,
}

struct CrawlerSession {
    /// Shared with the running crawl future; polled for live progress.
    state: SharedPresetCrawlingState,
    phase: CrawlPhase,
    /// The temp file holding all crawled FX chunks. Present once crawling stopped
    /// successfully; taken out (moved into the importer) on import.
    chunks_file: Option<File>,
    stop_reason: Option<PresetCrawlerStopReason>,
    /// Number of distinct presets crawled, captured when crawling stopped.
    crawled_count: u32,
    error: Option<String>,
}

thread_local! {
    static CRAWLER: RefCell<Option<CrawlerSession>> = const { RefCell::new(None) };
}

fn pos(x: i32, y: i32) -> MouseCursorPosition {
    MouseCursorPosition::new(x.max(0) as u32, y.max(0) as u32)
}

/// Starts a crawl of the currently focused FX (which must be open in a floating window).
///
/// Returns 1 on success, 0 if there's no suitable focused FX or a crawl/import is already
/// in progress.
#[allow(clippy::too_many_arguments)]
pub fn start(
    next_x: i32,
    next_y: i32,
    stop_if_destination_exists: bool,
    never_stop_crawling: bool,
    use_save_as: bool,
    save_x: i32,
    save_y: i32,
    cancel_x: i32,
    cancel_y: i32,
) -> i32 {
    reaper_low::firewall(|| {
        // Don't start over a crawl/import that's still in flight.
        let busy = CRAWLER.with(|c| {
            matches!(
                c.borrow().as_ref().map(|s| s.phase),
                Some(CrawlPhase::Running | CrawlPhase::Importing)
            )
        });
        if busy {
            return 0;
        }
        // The crawler needs an FX open in a floating window (it clicks the plug-in's own
        // "Next preset" button via mouse automation).
        let Some(focused) = Reaper::get().focused_fx() else {
            return 0;
        };
        let fx = focused.fx;
        if fx.floating_window().is_none() {
            return 0;
        }
        let state = PresetCrawlingState::new();
        let save_as_dialog = if use_save_as {
            Some(SaveAsDialogScraping {
                save_as_button_pos: pos(save_x, save_y),
                cancel_button_pos: pos(cancel_x, cancel_y),
            })
        } else {
            None
        };
        let args = CrawlPresetArgs {
            fx,
            next_preset_cursor_pos: pos(next_x, next_y),
            state: state.clone(),
            stop_if_destination_exists,
            never_stop_crawling,
            save_as_dialog,
            // No crawler window to refocus in the standalone case.
            bring_focus_back_to_crawler: || {},
        };
        CRAWLER.with(|c| {
            *c.borrow_mut() = Some(CrawlerSession {
                state: state.clone(),
                phase: CrawlPhase::Running,
                chunks_file: None,
                stop_reason: None,
                crawled_count: 0,
                error: None,
            });
        });
        Global::future_support().spawn_in_main_thread_from_main_thread(async move {
            let result = crawl_presets(args).await;
            finish_crawl(result);
            Ok(())
        });
        1
    })
    .unwrap_or(0)
}

fn finish_crawl(result: Result<PresetCrawlingOutcome, Box<dyn Error + Send + Sync>>) {
    CRAWLER.with(|c| {
        let mut cell = c.borrow_mut();
        let Some(s) = cell.as_mut() else {
            return;
        };
        match result {
            Ok(outcome) => {
                s.crawled_count = blocking_lock_arc(&s.state, "crawl finish").preset_count();
                s.stop_reason = Some(outcome.reason);
                s.chunks_file = Some(outcome.chunks_file);
                s.phase = CrawlPhase::Stopped;
            }
            Err(e) => {
                s.error = Some(e.to_string());
                s.phase = CrawlPhase::Failed;
            }
        }
    });
}

/// Imports the crawled presets to disk, then refreshes the pot database. Returns 1 if the
/// import was started, 0 if there's nothing to import.
pub fn import() -> i32 {
    reaper_low::firewall(|| {
        let taken = CRAWLER.with(|c| {
            let mut cell = c.borrow_mut();
            let s = cell.as_mut()?;
            if s.phase != CrawlPhase::Stopped {
                return None;
            }
            let file = s.chunks_file.take()?;
            s.phase = CrawlPhase::Importing;
            Some((s.state.clone(), file))
        });
        let Some((state, file)) = taken else {
            return 0;
        };
        Global::future_support().spawn_in_main_thread_from_main_thread(async move {
            let result = import_crawled_presets(state, file).await;
            finish_import(result);
            Ok(())
        });
        1
    })
    .unwrap_or(0)
}

fn finish_import(result: Result<(), Box<dyn Error + Send + Sync>>) {
    CRAWLER.with(|c| {
        let mut cell = c.borrow_mut();
        if let Some(s) = cell.as_mut() {
            match result {
                Ok(()) => s.phase = CrawlPhase::Done,
                Err(e) => {
                    s.error = Some(e.to_string());
                    s.phase = CrawlPhase::Failed;
                }
            }
        }
    });
    // Make the newly imported presets visible.
    if let Ok(shared) = standalone_pot_unit() {
        let mut unit = blocking_lock_arc(&shared, "crawl import refresh");
        unit.refresh_pot(shared.clone());
    }
}

/// Discards the current crawl session (drops the crawl state and closes the temp file).
pub fn discard() {
    reaper_low::firewall(|| {
        CRAWLER.with(|c| *c.borrow_mut() = None);
    });
}

pub fn phase_code() -> i32 {
    CRAWLER
        .with(|c| c.borrow().as_ref().map(|s| s.phase as i32))
        .unwrap_or(CrawlPhase::Idle as i32)
}

pub fn is_running() -> bool {
    CRAWLER.with(|c| {
        matches!(
            c.borrow().as_ref().map(|s| s.phase),
            Some(CrawlPhase::Running | CrawlPhase::Importing)
        )
    })
}

pub fn preset_count() -> i32 {
    CRAWLER
        .with(|c| {
            let cell = c.borrow();
            let s = cell.as_ref()?;
            let count = blocking_lock_arc(&s.state, "crawl preset_count").preset_count() as i32;
            Some(count)
        })
        .unwrap_or(0)
}

pub fn duplicate_count() -> i32 {
    CRAWLER
        .with(|c| {
            let cell = c.borrow();
            let s = cell.as_ref()?;
            let count = blocking_lock_arc(&s.state, "crawl duplicate_count")
                .duplicate_preset_name_count() as i32;
            Some(count)
        })
        .unwrap_or(0)
}

/// Distinct presets crawled, as captured when crawling stopped.
pub fn crawled_count() -> i32 {
    CRAWLER
        .with(|c| c.borrow().as_ref().map(|s| s.crawled_count as i32))
        .unwrap_or(0)
}

/// Writes the name of the most recently crawled preset into `f`. Returns true if there is one.
pub fn with_last_preset_name<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    CRAWLER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let state = blocking_lock_arc(&s.state, "crawl last_preset");
        let last = state.last_crawled_preset()?;
        Some(f(last.name()))
    })
}

/// Stop-reason code once crawling stopped: -1 none yet, else see [`stop_reason_label`].
pub fn stop_reason_code() -> i32 {
    CRAWLER
        .with(|c| {
            c.borrow()
                .as_ref()
                .and_then(|s| s.stop_reason.map(reason_code))
        })
        .unwrap_or(-1)
}

fn reason_code(r: PresetCrawlerStopReason) -> i32 {
    match r {
        PresetCrawlerStopReason::Interrupted => 0,
        PresetCrawlerStopReason::DestinationFileExists => 1,
        PresetCrawlerStopReason::PresetNameNotChangingAnymore => 2,
        PresetCrawlerStopReason::PresetNameLikeBeginning => 3,
    }
}

/// Human-readable explanation of the current stop reason, or empty.
pub fn stop_reason_label() -> &'static str {
    CRAWLER.with(|c| {
        match c.borrow().as_ref().and_then(|s| s.stop_reason) {
            Some(PresetCrawlerStopReason::Interrupted) => "Stopped: cancelled (Escape).",
            Some(PresetCrawlerStopReason::DestinationFileExists) => {
                "Stopped: reached an already-existing destination file."
            }
            Some(PresetCrawlerStopReason::PresetNameNotChangingAnymore) => {
                "Stopped: the preset name stopped changing (end of list, or the Next-preset position is wrong)."
            }
            Some(PresetCrawlerStopReason::PresetNameLikeBeginning) => {
                "Stopped: wrapped around to the beginning of the preset list."
            }
            None => "",
        }
    })
}

/// Writes the last error message into `f`, if any. Returns true if there was one.
pub fn with_error<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    CRAWLER.with(|c| {
        let cell = c.borrow();
        let s = cell.as_ref()?;
        let e = s.error.as_deref()?;
        Some(f(e))
    })
}

/// Writes the focused FX's name into `f`, if an FX is focused. Returns its result.
pub fn with_focused_fx_name<R>(f: impl FnOnce(&str) -> R) -> Option<R> {
    reaper_low::firewall(|| {
        let focused = Reaper::get().focused_fx()?;
        let name = focused.fx.name().into_string();
        Some(f(&name))
    })
    .flatten()
}

/// Returns true if there is a focused FX and it's open in a floating window.
pub fn focused_fx_is_floating() -> bool {
    reaper_low::firewall(|| {
        Reaper::get()
            .focused_fx()
            .map(|focused| focused.fx.floating_window().is_some())
            .unwrap_or(false)
    })
    .unwrap_or(false)
}
