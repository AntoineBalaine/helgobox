use crate::preset_recorder::{topmost_window_at, ClickButton, ClickPatch, InputEvent, RecordedMacro};
use crate::{
    parse_vst2_magic_number, parse_vst3_uid, pot_db, EscapeCatcher, PersistentPresetId, PluginId,
};
use base::enigo::EnigoMouse;
use base::future_util::millis;
use base::hash_util::{NonCryptoHashMap, NonCryptoIndexMap};
use base::{blocking_lock_arc, file_util, hash_util};
use base::{Mouse, MouseCursorPosition};
use camino::{Utf8Path, Utf8PathBuf};
use helgobox_api::persistence::MouseButton;
use image::DynamicImage;
use imageproc::template_matching::{find_extremes, match_template, MatchTemplateMethod};
use reaper_high::{Fx, FxInfo, Reaper};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Replay pacing. Each action waits the *recorded* inter-event delay before firing, so the
/// macro paces itself the way it was demonstrated: a step that opens a menu/dialog keeps the
/// pause the user left while it appeared (a human can't click a menu item before the menu has
/// drawn, so the recorded delay is a safe lower bound for "the UI was visible"). The delay is
/// clamped to a floor — so a brisk recording still leaves the UI time to draw a menu or
/// right-click pop-up that is *not* a separate OS window (which the window-wait can't detect) —
/// and a cap, so long human idle doesn't drag the crawl. Multi-click continuations (the 2nd/3rd
/// press of a double/triple-click) bypass the floor so they stay within the OS multi-click
/// interval. On top of this, a click into a just-appeared window additionally waits for that
/// window to actually exist (see [`resolve_replay_target`]).
const MIN_STEP_MS: u64 = 200;
const MAX_STEP_MS: u64 = 1500;
const DIALOG_POLL_MS: u64 = 15;
const DIALOG_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Image-**gated** replay (coordinate-first). Each recorded click carries a grayscale screen
/// patch; at replay we click the *recorded coordinate* and use the patch only to wait until that
/// image has reappeared **at that coordinate** (the menu/dialog has rendered). Windows don't move
/// between record and replay, so the coordinate is authoritative — we never relocate the click to
/// a best match elsewhere (which lands on similar-looking sibling menu rows). The patch replaces
/// the old fixed delay with a real readiness signal while keeping coordinate accuracy. Knobs:
/// - confirm presence within `MATCH_TOLERANCE` logical px of the recorded point (small: just
///   enough to absorb rendering jitter; we are *checking*, not searching);
/// - consider the target present when the normalized sum-of-squared-errors is below
///   `MATCH_MAX_SSE` (0 = identical; same machine, so a rendered target scores near 0);
/// - poll every `MATCH_POLL_MS` (a full-monitor screenshot per poll isn't cheap);
/// - a target expected to be already present gives up after `MATCH_SETTLED_TIMEOUT` (then clicks
///   the recorded coordinate anyway), whereas a `wait_for_window` click waits `DIALOG_WAIT_TIMEOUT`.
const MATCH_TOLERANCE: i32 = 24;
const MATCH_MAX_SSE: f32 = 0.20;
const MATCH_POLL_MS: u64 = 60;
const MATCH_SETTLED_TIMEOUT: Duration = Duration::from_millis(700);
/// Match at 1/N resolution. Naive template matching is O(search-area × template-area), which
/// is ~1.3 billion ops over a 480×480 region with a 120×72 patch — seconds per match in a debug
/// build. Downscaling both by N cuts that by ~N⁴ and costs only ~N px of location precision
/// (negligible for clicking a button/menu item).
const MATCH_DOWNSCALE: u32 = 3;

/// Crawler diagnostics, printed to **stdout** (REAPER is launched from a terminal while
/// debugging the crawl). Kept to per-iteration / per-click granularity — never per poll — so it
/// traces the flow without flooding. The `[pot-crawler]` prefix makes it greppable.
fn crawl_log(msg: impl std::fmt::Display) {
    println!("[pot-crawler] {msg}");
}

pub type SharedPresetCrawlingState = Arc<Mutex<PresetCrawlingState>>;

#[derive(Debug)]
pub struct PresetCrawlingState {
    crawled_presets: NonCryptoIndexMap<String, CrawledPreset>,
    duplicate_preset_names: Vec<String>,
    same_preset_name_in_a_row: Option<String>,
    same_preset_name_in_a_row_attempts: u32,
    same_preset_names_like_beginning: Vec<String>,
    same_preset_name_like_beginning_attempts: u32,
    total_bytes_crawled: usize,
}

#[derive(Debug)]
pub struct PresetCrawlingOutcome {
    /// One temporary file that holds the chunks of all FXs when crawling finished.
    /// Will be copied to separate destination files at a later stage.
    /// If not set when stopped, this means at first it's a failure. Later we take the
    /// file out of here for processing, in that case it's also `None`.
    pub chunks_file: File,
    pub reason: PresetCrawlerStopReason,
}

impl PresetCrawlingOutcome {
    pub fn new(chunks_file: File, reason: PresetCrawlerStopReason) -> Self {
        Self {
            chunks_file,
            reason,
        }
    }
}

impl PresetCrawlingState {
    pub fn new() -> SharedPresetCrawlingState {
        let state = Self {
            crawled_presets: Default::default(),
            duplicate_preset_names: Default::default(),
            same_preset_name_in_a_row: None,
            same_preset_name_in_a_row_attempts: 0,
            same_preset_names_like_beginning: Default::default(),
            same_preset_name_like_beginning_attempts: 0,
            total_bytes_crawled: 0,
        };
        Arc::new(Mutex::new(state))
    }

    pub fn last_crawled_preset(&self) -> Option<&CrawledPreset> {
        let last = self.crawled_presets.last()?;
        Some(last.1)
    }

    pub fn pop_crawled_preset(&mut self) -> Option<CrawledPreset> {
        let last = self.crawled_presets.pop()?;
        Some(last.1)
    }

    pub fn bytes_crawled(&self) -> usize {
        self.total_bytes_crawled
    }

    pub fn crawled_presets(&self) -> &NonCryptoIndexMap<String, CrawledPreset> {
        &self.crawled_presets
    }

    pub fn preset_count(&self) -> u32 {
        self.crawled_presets.len() as _
    }

    pub fn duplicate_preset_name_count(&self) -> u32 {
        self.duplicate_preset_names.len() as _
    }

    pub fn duplicate_preset_names(&self) -> &[String] {
        &self.duplicate_preset_names
    }

    fn add_preset(&mut self, preset: CrawledPreset, never_stop_crawling: bool) -> NextCrawlStep {
        // Give stop signal if we reached the end of the list or are at its beginning again.
        if !never_stop_crawling {
            if let Some(step) = self.make_stop_check(&preset) {
                return step;
            }
        }
        // Reset "same preset name attempts" logic
        self.same_preset_name_in_a_row_attempts = 0;
        if let Some(last_same_preset_name) = self.same_preset_name_in_a_row.take() {
            // Turns out that the last discovered same preset name was actually not the end
            // of the preset list but just an intermediate duplicate. Treat it as such!
            self.duplicate_preset_names.push(last_same_preset_name);
        }
        // Reset "same preset name like beginning" logic
        self.same_preset_name_like_beginning_attempts = 0;
        self.duplicate_preset_names
            .append(&mut self.same_preset_names_like_beginning);
        // Add or skip
        if self.crawled_presets.contains_key(&preset.name) {
            // Duplicate name. Skip preset!
            self.duplicate_preset_names.push(preset.name);
        } else {
            // Add preset
            self.total_bytes_crawled += preset.size_in_bytes;
            self.crawled_presets.insert(preset.name.clone(), preset);
        }
        NextCrawlStep::Continue
    }

    /// This executes a heuristic to check whether the end of the preset list has been reached and
    /// crawling should therefore stop.
    ///
    /// It looks at the preset names only. I also tried to take the chunk into account but it's not
    /// deterministic. Getting the chunk for one preset multiple times can yield different results!
    fn make_stop_check(&mut self, preset: &CrawledPreset) -> Option<NextCrawlStep> {
        // If we haven't crawled anything yet, there's nothing to check.
        let (_, last_preset) = self.crawled_presets.last()?;
        // Check if we get multiple equally named presets in a row.
        if preset.name == last_preset.name {
            // Same name like last crawled preset
            if self.same_preset_name_in_a_row_attempts <= MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS {
                // Let's tolerate that right now and still continue crawling.
                // It's possible that the plug-in crops the preset name and therefore
                // presets that seemingly have the same name, in fact have different ones
                // but have the same prefix. This happened with Zebra2 VSTi, for example.
                self.same_preset_name_in_a_row_attempts += 1;
                // Don't add it to the list of duplicates right away because it *might* really
                // turn out to be the end of the preset list! If it turns out it isn't, we still add
                // it to the list of duplicates later.
                self.same_preset_name_in_a_row = Some(preset.name.clone());
                return Some(NextCrawlStep::Continue);
            } else {
                // More than max same preset names in a row! That either means the
                // "Next preset" button doesn't work at all or we have reached the end of the
                // preset list.
                return Some(NextCrawlStep::Stop(
                    PresetCrawlerStopReason::PresetNameNotChangingAnymore,
                ));
            }
        }
        // Now check if the presets that we crawl are the same ones that we crawled in the beginning.
        if let Some((_, reference_preset)) = self
            .crawled_presets
            .get_index(self.same_preset_name_like_beginning_attempts as usize)
        {
            if preset.name == reference_preset.name {
                // This preset has the same name as the reference preset, which is one of the
                // presets crawled right at the beginning.
                if self.same_preset_name_like_beginning_attempts
                    <= MAX_SAME_PRESET_NAME_LIKE_BEGINNING_ATTEMPTS
                {
                    // Let's tolerate that right now and still continue crawling.
                    // It's possible that the plug-in doesn't navigate through the preset list in
                    // a linear way.
                    self.same_preset_name_like_beginning_attempts += 1;
                    // Don't add it to the list of duplicates right away because it *might* really
                    // turn out to be the beginning of the preset list! If it turns out it isn't,
                    // we still add it to the list of duplicates later.
                    self.same_preset_names_like_beginning
                        .push(preset.name.clone());
                    return Some(NextCrawlStep::Continue);
                } else {
                    // More than max matches with the beginning! That either means the plug-in
                    // navigates in a *very* non-linear fashion through the preset list or we have
                    // reached the end of the preset list and restarted at its beginning.
                    return Some(NextCrawlStep::Stop(
                        PresetCrawlerStopReason::PresetNameLikeBeginning,
                    ));
                }
            }
        }
        None
    }
}

enum NextCrawlStep {
    Continue,
    Stop(PresetCrawlerStopReason),
}

#[derive(Debug)]
pub struct CrawledPreset {
    name: String,
    offset: u64,
    size_in_bytes: usize,
    destination: Utf8PathBuf,
}

impl CrawledPreset {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn destination(&self) -> &Utf8Path {
        &self.destination
    }
}

pub struct CrawlPresetArgs<F> {
    pub fx: Fx,
    pub next_preset_cursor_pos: MouseCursorPosition,
    pub state: SharedPresetCrawlingState,
    pub stop_if_destination_exists: bool,
    pub never_stop_crawling: bool,
    /// If set, preset names are obtained by scraping the plug-in's own "Save Preset As"
    /// dialog instead of asking the REAPER API. For plug-ins that don't expose their
    /// preset names.
    pub save_as_dialog: Option<SaveAsDialogScraping>,
    /// If set, the "Next preset" action is performed by replaying this recorded macro
    /// (verified, window-relative) instead of clicking `next_preset_cursor_pos`.
    pub next_preset_macro: Option<RecordedMacro>,
    /// If set, preset names are scraped by replaying a recorded action macro (clicks +
    /// keystrokes) instead of the fixed two-position `save_as_dialog`. Takes precedence
    /// over `save_as_dialog` when both are set. See [`crate::preset_recorder`].
    pub save_as_macro: Option<RecordedMacro>,
    /// If set, presets are saved as native REAPER FX presets by replaying this recorded
    /// macro (REAPER's "+" -> "Save preset" -> paste name -> save) per new preset, instead
    /// of capturing FX chunks into `.RfxChain` files. No import step is needed.
    pub save_preset_macro: Option<RecordedMacro>,
    pub bring_focus_back_to_crawler: F,
}

/// Configuration for obtaining preset names by scraping the plug-in's own
/// "Save Preset As" dialog.
///
/// Works like this: Click the plug-in's "Save Preset As" button. Wait until the dialog
/// window appears. The dialog's name field contains the current preset name and has
/// keyboard focus, so select everything and copy it to the clipboard. Read the clipboard.
/// Click the dialog's "Cancel" button.
#[derive(Copy, Clone, Debug)]
pub struct SaveAsDialogScraping {
    /// Where the plug-in's "Save Preset As" button sits on the screen.
    pub save_as_button_pos: MouseCursorPosition,
    /// Where the dialog's "Cancel" button sits on the screen (once the dialog is open).
    pub cancel_button_pos: MouseCursorPosition,
}

pub async fn crawl_presets<F>(
    args: CrawlPresetArgs<F>,
) -> Result<PresetCrawlingOutcome, Box<dyn Error + Send + Sync>>
where
    F: Fn() + 'static,
{
    let reaper_resource_dir = Reaper::get().resource_path();
    // No need to fall back to chunk-based FX info because Pot is experimental, and we can assume it's used
    // with recent REAPER versions.
    let fx_info = args.fx.info()?;
    let plugin_id = get_plugin_id_from_fx_info(&fx_info);
    let mut mouse = EnigoMouse::new();
    let escape_catcher = EscapeCatcher::new();
    let mut chunks_file = tempfile::tempfile()?;
    let mut current_file_offset = 0u64;
    let mut iteration = 0u32;
    crawl_log(format!(
        "crawl start: fx={:?}, never_stop={}, macros: next={} save_as={} save_preset={}",
        fx_info.effect_name,
        args.never_stop_crawling,
        args.next_preset_macro.is_some(),
        args.save_as_macro.is_some(),
        args.save_preset_macro.is_some(),
    ));
    loop {
        // Check if escape has been pressed
        if escape_catcher.escape_was_pressed() {
            crawl_log("interrupted (escape)");
            return Ok(PresetCrawlingOutcome::new(
                chunks_file,
                PresetCrawlerStopReason::Interrupted,
            ));
        }
        iteration += 1;
        crawl_log(format!("--- iteration {iteration} ---"));
        // Get preset name. Preference: recorded macro > fixed-position scraping > host API.
        let scraped = if let Some(macro_events) = &args.save_as_macro {
            // Make sure the plug-in window is visible/front before replaying into it.
            args.fx.show_in_floating_window()?;
            Some(replay_macro_and_scrape(&mut mouse, macro_events, &escape_catcher).await?)
        } else if let Some(config) = &args.save_as_dialog {
            args.fx.show_in_floating_window()?;
            let config = *config;
            Some(scrape_preset_name_via_save_as_dialog(&mut mouse, config, &escape_catcher).await?)
        } else {
            None
        };
        let name = match scraped {
            None => args
                .fx
                .preset_name()
                .ok_or("couldn't get preset name")?
                .into_string(),
            Some(ScrapedName::Name(name)) => name,
            Some(ScrapedName::Interrupted) => {
                crawl_log("interrupted while scraping name");
                return Ok(PresetCrawlingOutcome::new(
                    chunks_file,
                    PresetCrawlerStopReason::Interrupted,
                ));
            }
        };
        crawl_log(format!("preset name = {name:?}"));
        if let Some(save_macro) = &args.save_preset_macro {
            // Native-preset mode: dedup / detect end-of-list by NAME *before* saving, so a
            // wrap-around duplicate is never re-saved (which could trip REAPER's "overwrite?"
            // prompt). Only genuinely new presets are saved, by replaying the recorded "Save
            // preset" macro (which pastes the name from the clipboard into REAPER's
            // Save-preset dialog). No FX chunk is captured and there's no import step.
            let dummy = CrawledPreset {
                name: name.clone(),
                offset: 0,
                size_in_bytes: 0,
                destination: Utf8PathBuf::new(),
            };
            let before = blocking_lock_arc(&args.state, "crawl native 1").preset_count();
            let next_step = blocking_lock_arc(&args.state, "crawl native 2")
                .add_preset(dummy, args.never_stop_crawling);
            if let NextCrawlStep::Stop(reason) = next_step {
                crawl_log(format!("STOP: {reason:?} (captured {before} presets)"));
                return Ok(PresetCrawlingOutcome::new(chunks_file, reason));
            }
            let is_new = blocking_lock_arc(&args.state, "crawl native 3").preset_count() > before;
            crawl_log(format!(
                "dedup: is_new={is_new}, preset_count {before} -> {}",
                blocking_lock_arc(&args.state, "crawl native log").preset_count()
            ));
            if is_new {
                // The save macro pastes the name from the clipboard. After a name scrape it's
                // already there; for host-provided names, put it there now.
                if args.save_as_macro.is_none() && args.save_as_dialog.is_none() {
                    set_clipboard_text(&name)?;
                }
                args.fx.show_in_floating_window()?;
                crawl_log(format!("saving new preset {name:?}"));
                if !replay_macro(&mut mouse, save_macro, &escape_catcher, "save-preset").await? {
                    crawl_log("interrupted while saving preset");
                    return Ok(PresetCrawlingOutcome::new(
                        chunks_file,
                        PresetCrawlerStopReason::Interrupted,
                    ));
                }
            }
        } else {
            // Chunk / RfxChain mode (the egui browser path).
            let fx_chunk = args.fx.chunk()?;
            let fx_chunk_content = fx_chunk.content();
            let fx_chunk_bytes = fx_chunk_content.as_bytes();
            chunks_file.write_all(fx_chunk_bytes)?;
            chunks_file.flush()?;
            // Determine where on the disk the RfxChain file should end up
            let destination = determine_preset_file_destination(
                &fx_info,
                &reaper_resource_dir,
                &name,
                plugin_id.as_ref(),
            );
            if args.stop_if_destination_exists && destination.exists() {
                return Ok(PresetCrawlingOutcome::new(
                    chunks_file,
                    PresetCrawlerStopReason::DestinationFileExists,
                ));
            }
            // Build crawled preset
            let crawled_preset = CrawledPreset {
                destination,
                name,
                offset: current_file_offset,
                size_in_bytes: fx_chunk_bytes.len(),
            };
            current_file_offset += fx_chunk_bytes.len() as u64;
            let next_step = blocking_lock_arc(&args.state, "crawl_presets 3")
                .add_preset(crawled_preset, args.never_stop_crawling);
            match next_step {
                NextCrawlStep::Stop(reason) => {
                    crawl_log(format!("STOP: {reason:?}"));
                    return Ok(PresetCrawlingOutcome::new(chunks_file, reason));
                }
                NextCrawlStep::Continue => {}
            }
        }
        // Advance to the next preset: replay the recorded macro if present (verified,
        // window-relative), otherwise click the single captured position.
        args.fx.show_in_floating_window()?;
        crawl_log("advancing to next preset");
        if let Some(next_macro) = &args.next_preset_macro {
            if !replay_macro(&mut mouse, next_macro, &escape_catcher, "next-preset").await? {
                crawl_log("interrupted while advancing");
                return Ok(PresetCrawlingOutcome::new(
                    chunks_file,
                    PresetCrawlerStopReason::Interrupted,
                ));
            }
        } else {
            click_at(&mut mouse, args.next_preset_cursor_pos).await?;
        }
        a_bit_longer().await;
    }
}

async fn click_at(
    mouse: &mut EnigoMouse,
    pos: MouseCursorPosition,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    mouse.set_cursor_position(pos)?;
    moment().await;
    mouse.press(MouseButton::Left)?;
    moment().await;
    mouse.release(MouseButton::Left)?;
    Ok(())
}

enum ScrapedName {
    Name(String),
    /// The user pressed escape while we waited for the dialog.
    Interrupted,
}

/// Obtains the current preset name by scraping the plug-in's "Save Preset As" dialog.
///
/// See [`SaveAsDialogScraping`] for how this works.
async fn scrape_preset_name_via_save_as_dialog(
    mouse: &mut EnigoMouse,
    config: SaveAsDialogScraping,
    escape_catcher: &EscapeCatcher,
) -> Result<ScrapedName, Box<dyn Error + Send + Sync>> {
    // Snapshot the currently open windows, so we can detect the dialog appearing.
    let windows_before = count_window_titles(window_titles()?.into_iter());
    // Open the "Save Preset As" dialog
    click_at(mouse, config.save_as_button_pos).await?;
    // Wait for the dialog window to appear. Don't use a fixed sleep: plug-ins open their
    // dialogs at very different speeds.
    let waiting_start = Instant::now();
    loop {
        millis(50).await;
        if escape_catcher.escape_was_pressed() {
            return Ok(ScrapedName::Interrupted);
        }
        if waiting_start.elapsed() > SAVE_AS_DIALOG_APPEARANCE_TIMEOUT {
            return Err("the \"Save Preset As\" dialog didn't appear in time. \
                Is the configured button position correct?"
                .into());
        }
        let after = window_titles()?;
        if detect_new_window_title(&windows_before, &after).is_some() {
            break;
        }
    }
    // The dialog's name field contains the current preset name and usually has keyboard
    // focus right away. Put a sentinel into the clipboard first, so we can tell whether
    // the copy actually worked. The clipboard handle is deliberately not kept across
    // await points (it's not Send on all platforms).
    set_clipboard_text(CLIPBOARD_SENTINEL)?;
    moment().await;
    mouse.select_all_and_copy();
    moment().await;
    let name = clipboard_text()?;
    // Dismiss the dialog, whatever the outcome
    click_at(mouse, config.cancel_button_pos).await?;
    a_bit_longer().await;
    if name == CLIPBOARD_SENTINEL {
        return Err("couldn't copy the preset name out of the \"Save Preset As\" dialog. \
            Maybe the dialog's name field doesn't have keyboard focus?"
            .into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("the \"Save Preset As\" dialog yielded an empty preset name".into());
    }
    Ok(ScrapedName::Name(name.to_string()))
}

/// Finds the currently-open window with the given xcap id, if any.
fn find_window_by_id(id: u32) -> Option<xcap::Window> {
    xcap::Window::all().ok()?.into_iter().find(|w| w.id() == id)
}

/// Snapshot of the currently-open window ids (empty if enumeration fails).
fn current_window_ids() -> HashSet<u32> {
    xcap::Window::all()
        .map(|ws| ws.iter().map(|w| w.id()).collect())
        .unwrap_or_default()
}

/// Posts a mouse click at `(x, y)` carrying the macOS multi-click state (1=single,
/// 2=double, 3=triple). enigo always posts click-state 1, so a recorded triple-click would
/// otherwise replay as three single clicks and never select text in a native dialog.
#[cfg(target_os = "macos")]
fn synth_click_macos(x: i32, y: i32, button: ClickButton, clicks: u8) {
    use core_graphics::event::{
        CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
        return;
    };
    let (down_type, up_type, btn) = match button {
        ClickButton::Left => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
        ClickButton::Right => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
    };
    let point = CGPoint::new(x as f64, y as f64);
    for event_type in [down_type, up_type] {
        if let Ok(event) = CGEvent::new_mouse_event(source.clone(), event_type, point, btn) {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks as i64);
            event.post(CGEventTapLocation::HID);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The recorded delay (ms since the previous event) for any event variant.
fn event_delay_ms(ev: &InputEvent) -> u64 {
    match ev {
        InputEvent::Click { delay_ms, .. }
        | InputEvent::KeyDown { delay_ms, .. }
        | InputEvent::KeyUp { delay_ms, .. } => *delay_ms,
    }
}

/// True for the 2nd/3rd press of a double/triple-click (`clicks > 1`): these must replay in
/// quick succession to register as a multi-click, so they bypass the inter-action floor.
fn is_multiclick_continuation(ev: &InputEvent) -> bool {
    matches!(ev, InputEvent::Click { clicks, .. } if *clicks > 1)
}

/// Replays a recorded action macro. Returns `Ok(false)` if the user pressed Escape.
///
/// Paces each action by its recorded inter-event delay (clamped to [`MIN_STEP_MS`,
/// `MAX_STEP_MS`]) so menus and pop-ups have time to appear, and before a click into a
/// just-appeared window it additionally *waits for that dialog/menu to actually appear* (and
/// anchors the click to it, wherever it reopened). Stable-window clicks are verified to be
/// topmost first (abort if a different window covers the target). Keys replay by physical
/// position; on macOS clicks carry the multi-click state.
async fn replay_macro(
    mouse: &mut EnigoMouse,
    macro_events: &[InputEvent],
    escape_catcher: &EscapeCatcher,
    label: &str,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    crawl_log(format!(
        "replay[{label}]: {} events",
        macro_events.len()
    ));
    // The transient dialog/menu most recently waited for; subsequent clicks in it re-anchor here.
    let mut tracked_dialog: Option<u32> = None;
    // Window-id set captured before the *previous* action ran. When a click targets a
    // just-appeared window, this is the set from before the click that opened it — so the
    // dialog reads as "new" even if it appeared faster than we got here (snapshotting the
    // baseline only once we're ready to wait would race the dialog already being up).
    crawl_log(format!("replay[{label}] pre-loop: enumerating windows…"));
    let mut ids_before_prev_action = current_window_ids();
    crawl_log(format!(
        "replay[{label}] pre-loop: {} windows",
        ids_before_prev_action.len()
    ));
    // The location the previous click actually landed on, so a multi-click continuation
    // re-clicks the exact same spot (keeping the OS multi-click together) without re-matching.
    let mut last_click_pos: Option<(i32, i32)> = None;
    for (idx, ev) in macro_events.iter().enumerate() {
        if escape_catcher.escape_was_pressed() {
            crawl_log(format!("replay[{label}]: escape at event {idx}"));
            return Ok(false);
        }
        let kind = match ev {
            InputEvent::Click { .. } => "click",
            InputEvent::KeyDown { .. } => "keydown",
            InputEvent::KeyUp { .. } => "keyup",
        };
        // Multi-click continuations must stay fast (within the OS multi-click interval), so
        // they skip the floor; every other action gets the floor so the UI it targets has had
        // time to appear.
        let delay_ms = if is_multiclick_continuation(ev) {
            event_delay_ms(ev).min(MAX_STEP_MS)
        } else {
            event_delay_ms(ev).clamp(MIN_STEP_MS, MAX_STEP_MS)
        };
        crawl_log(format!("replay[{label}] event {idx} ({kind}): sleep {delay_ms}ms"));
        millis(delay_ms).await;
        crawl_log(format!("replay[{label}] event {idx}: enumerating windows…"));
        let ids_before_this_action = current_window_ids();
        match ev {
            InputEvent::Click {
                button,
                win_id,
                off_x,
                off_y,
                abs_x,
                abs_y,
                clicks,
                wait_for_window,
                patch,
                ..
            } => {
                let (tx, ty, how): (i32, i32, &str) = if is_multiclick_continuation(ev) {
                    // Land exactly where the click it continues landed, so the OS still reads
                    // them as one multi-click.
                    let (x, y) = last_click_pos.unwrap_or((*abs_x, *abs_y));
                    (x, y, "multiclick-continuation")
                } else if let Some(p) = patch {
                    // Coordinate-first, image-gated: click the RECORDED coordinate, using the
                    // patch only to *wait until the recorded image reappears at that location*
                    // (the menu/dialog has rendered). Windows don't move between record and
                    // replay, so the recorded coordinate is authoritative — we never relocate
                    // the click to a best-match elsewhere (which would land on a similar-looking
                    // sibling menu row). The patch replaces the old fixed delay with a real
                    // readiness signal while keeping coordinate accuracy.
                    let timeout = if *wait_for_window {
                        DIALOG_WAIT_TIMEOUT
                    } else {
                        MATCH_SETTLED_TIMEOUT
                    };
                    let ready = wait_for_patch(p, *abs_x, *abs_y, timeout, label, escape_catcher).await;
                    let how = if ready { "image-gated" } else { "image-gate-timeout" };
                    (*abs_x, *abs_y, how)
                } else {
                    // No patch captured (rare: capture failed). Fall back to window-relative
                    // resolution / recorded absolute position.
                    let (x, y) = resolve_replay_target(
                        *win_id,
                        *off_x,
                        *off_y,
                        *abs_x,
                        *abs_y,
                        *wait_for_window,
                        &ids_before_prev_action,
                        &mut tracked_dialog,
                        escape_catcher,
                    )
                    .await?;
                    (x, y, "resolve-no-patch")
                };
                crawl_log(format!(
                    "replay[{label}] event {idx}: {:?} click (n={clicks}, wait_win={wait_for_window}, \
                     patch={}) recorded@({abs_x},{abs_y}) -> ({tx},{ty}) via {how}",
                    button,
                    patch.is_some(),
                ));
                last_click_pos = Some((tx, ty));
                let pos = MouseCursorPosition::new(tx.max(0) as u32, ty.max(0) as u32);
                mouse.set_cursor_position(pos)?;
                millis(12).await;
                // On macOS the click must carry the multi-click state so the OS treats a
                // recorded triple-click as a triple-click (enigo can't set it).
                #[cfg(target_os = "macos")]
                {
                    synth_click_macos(tx, ty, *button, *clicks);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = clicks;
                    let b = match button {
                        ClickButton::Left => MouseButton::Left,
                        ClickButton::Right => MouseButton::Right,
                    };
                    mouse.press(b)?;
                    millis(12).await;
                    mouse.release(b)?;
                }
            }
            InputEvent::KeyDown { raw, .. } => {
                crawl_log(format!("replay[{label}] event {idx}: key down raw={raw}"));
                mouse.press_raw_key(*raw);
            }
            InputEvent::KeyUp { raw, .. } => {
                crawl_log(format!("replay[{label}] event {idx}: key up raw={raw}"));
                mouse.release_raw_key(*raw);
            }
        }
        ids_before_prev_action = ids_before_this_action;
    }
    Ok(true)
}

/// Resolves where a recorded click should land:
/// - stable window still open -> verify it's the topmost window there (abort otherwise);
/// - a click into a just-appeared window (`wait_for_window`) -> wait for that dialog/menu and
///   anchor to it; subsequent clicks in the same dialog re-anchor to it (so it tracks wherever
///   the dialog reopened);
/// - otherwise fall back to the recorded absolute position.
#[allow(clippy::too_many_arguments)]
async fn resolve_replay_target(
    win_id: u32,
    off_x: i32,
    off_y: i32,
    abs_x: i32,
    abs_y: i32,
    wait_for_window: bool,
    baseline_ids: &HashSet<u32>,
    tracked_dialog: &mut Option<u32>,
    escape_catcher: &EscapeCatcher,
) -> Result<(i32, i32), Box<dyn Error + Send + Sync>> {
    if win_id != 0 {
        if let Some(win) = find_window_by_id(win_id) {
            let (tx, ty) = (win.x() + off_x, win.y() + off_y);
            match topmost_window_at(tx, ty) {
                Some(top) if top.id() == win_id => {}
                Some(top) => {
                    return Err(format!(
                        "expected window {win_id} at ({tx},{ty}) but '{}' is on top — aborting \
                         to avoid clicking the wrong window",
                        top.app_name()
                    )
                    .into());
                }
                None => return Err(format!("no window at ({tx},{ty}) during replay").into()),
            }
            *tracked_dialog = None;
            crawl_log("  resolve: stable window-relative");
            return Ok((tx, ty));
        }
    }
    // Transient window (reopened with a new id). Wait for it to appear if it's just opening.
    if wait_for_window {
        if let Some(id) = wait_for_new_window(baseline_ids, escape_catcher).await {
            *tracked_dialog = Some(id);
            crawl_log("  resolve: waited, new window appeared");
        } else {
            crawl_log("  resolve: waited, NO new window appeared");
        }
    }
    if let Some((dx, dy)) = (*tracked_dialog)
        .and_then(find_window_by_id)
        .map(|d| (d.x() + off_x, d.y() + off_y))
    {
        crawl_log("  resolve: transient-dialog anchor");
        return Ok((dx, dy));
    }
    crawl_log("  resolve: recorded absolute position");
    Ok((abs_x, abs_y))
}

/// Polls for a window absent from `baseline` (the dialog/menu the opener click is bringing
/// up) to appear, returning its id. `baseline` is the window set from *before* the opener
/// click, so a dialog that already appeared still reads as new and is found on the first poll;
/// a slow one is waited for. Returns None on timeout or Escape. (xcap windows are turned into
/// ids immediately so no `!Send` handle is held across an await.)
async fn wait_for_new_window(
    baseline: &HashSet<u32>,
    escape_catcher: &EscapeCatcher,
) -> Option<u32> {
    let start = Instant::now();
    loop {
        if escape_catcher.escape_was_pressed() {
            return None;
        }
        if let Ok(windows) = xcap::Window::all() {
            if let Some(id) = windows.iter().map(|w| w.id()).find(|id| !baseline.contains(id)) {
                return Some(id);
            }
        }
        if start.elapsed() >= DIALOG_WAIT_TIMEOUT {
            return None;
        }
        millis(DIALOG_POLL_MS).await;
    }
}

/// Checks whether the recorded click patch is present **at** its recorded position, returning the
/// best normalized-SSE score (lower = better) within a small [`MATCH_TOLERANCE`] window around it,
/// or `None` if the screen couldn't be captured. This is a presence test, not a search: the
/// caller clicks the recorded coordinate, never a relocated best-match. All xcap/image handles are
/// dropped before returning, so no `!Send` value is held across the caller's await points.
fn match_patch_on_screen(patch: &ClickPatch, exp_x: i32, exp_y: i32) -> Option<f32> {
    crawl_log(format!("  [capture-thread] Monitor::from_point({exp_x},{exp_y})…"));
    let monitor = match xcap::Monitor::from_point(exp_x, exp_y) {
        Ok(m) => m,
        Err(e) => {
            crawl_log(format!("  [capture-thread] from_point failed: {e}"));
            return None;
        }
    };
    let scale = monitor.scale_factor().max(0.01);
    crawl_log(format!(
        "  [capture-thread] monitor @({},{}) {}x{} scale {scale}; capture_image()…",
        monitor.x(),
        monitor.y(),
        monitor.width(),
        monitor.height(),
    ));
    let rgba = match monitor.capture_image() {
        Ok(img) => img,
        Err(e) => {
            crawl_log(format!("  [capture-thread] capture_image failed: {e}"));
            return None;
        }
    };
    crawl_log(format!(
        "  [capture-thread] captured {}x{}; matching…",
        rgba.width(),
        rgba.height()
    ));
    let gray = DynamicImage::ImageRgba8(rgba).into_luma8();
    let (iw, ih) = (gray.width() as i32, gray.height() as i32);
    // Search only a small tolerance window *centred on the recorded click point*, in physical
    // pixels. The patch was captured centred on the click (its click point is at click_x/click_y
    // inside it), so aligning the patch's click point to the recorded location puts the patch
    // top-left at (epx - click_x, epy - click_y); we widen by `tol` on every side so the match
    // can slide a little to absorb rendering jitter.
    let epx = (((exp_x - monitor.x()) as f32) * scale).round() as i32;
    let epy = (((exp_y - monitor.y()) as f32) * scale).round() as i32;
    let tol = (MATCH_TOLERANCE as f32 * scale).round() as i32;
    let sx = (epx - patch.click_x as i32 - tol).clamp(0, (iw - 1).max(0));
    let sy = (epy - patch.click_y as i32 - tol).clamp(0, (ih - 1).max(0));
    let sw = (patch.w as i32 + 2 * tol).min(iw - sx).max(0) as u32;
    let sh = (patch.h as i32 + 2 * tol).min(ih - sy).max(0) as u32;
    // match_template panics unless the template is strictly smaller than the searched image.
    if sw <= patch.w || sh <= patch.h {
        return None;
    }
    let region = image::imageops::crop_imm(&gray, sx as u32, sy as u32, sw, sh).to_image();
    let template = image::GrayImage::from_raw(patch.w, patch.h, patch.bytes.clone())?;
    // Match at reduced resolution (see MATCH_DOWNSCALE). Same filter on both keeps them aligned.
    let ds = MATCH_DOWNSCALE.max(1);
    let (rw, rh) = (region.width() / ds, region.height() / ds);
    let (tw, th) = (template.width() / ds, template.height() / ds);
    if tw == 0 || th == 0 || rw <= tw || rh <= th {
        return None;
    }
    let region = image::imageops::resize(&region, rw, rh, image::imageops::FilterType::Triangle);
    let template =
        image::imageops::resize(&template, tw, th, image::imageops::FilterType::Triangle);
    let t = Instant::now();
    let scores = match_template(
        &region,
        &template,
        MatchTemplateMethod::SumOfSquaredErrorsNormalized,
    );
    let best = find_extremes(&scores).min_value;
    crawl_log(format!(
        "  [capture-thread] match done in {}ms (1/{ds} scale, region {rw}x{rh}), best score {:.3}",
        t.elapsed().as_millis(),
        best,
    ));
    Some(best)
}

/// Runs [`match_patch_on_screen`] on a throwaway background thread and awaits the score.
///
/// The screen grab + template match is heavy (a full-monitor `capture_image()` plus matching),
/// and the crawl future runs on REAPER's main thread, pumped once per frame — doing this work
/// inline would stall REAPER's UI for the duration of every poll. Running it on a worker thread
/// and polling for the result keeps the main loop responsive. Only the `Send` score crosses back,
/// so no `!Send` xcap handle escapes the worker.
async fn capture_and_match(patch: &ClickPatch, exp_x: i32, exp_y: i32) -> Option<f32> {
    let patch = patch.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(match_patch_on_screen(&patch, exp_x, exp_y));
    });
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Empty) => millis(10).await,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
        }
    }
}

/// Polls [`capture_and_match`] until the recorded patch is present at the recorded location
/// (score within threshold) or `timeout` elapses (or Escape), logging the best score on the way.
/// Returns `true` if the target was confirmed present, `false` on timeout/Escape — either way the
/// caller clicks the recorded coordinate; this just decides *when* (after the menu/dialog renders).
async fn wait_for_patch(
    patch: &ClickPatch,
    exp_x: i32,
    exp_y: i32,
    timeout: Duration,
    label: &str,
    escape_catcher: &EscapeCatcher,
) -> bool {
    crawl_log(format!(
        "replay[{label}] patch gate begin at ({exp_x},{exp_y}), capturing screen (timeout {}ms)…",
        timeout.as_millis()
    ));
    let start = Instant::now();
    let mut best = f32::INFINITY;
    let mut polls = 0u32;
    let mut logged_capture_fail = false;
    loop {
        if escape_catcher.escape_was_pressed() {
            return false;
        }
        polls += 1;
        match capture_and_match(patch, exp_x, exp_y).await {
            Some(score) => {
                if score < best {
                    best = score;
                }
                if score <= MATCH_MAX_SSE {
                    crawl_log(format!(
                        "replay[{label}] patch {}x{} present at ({exp_x},{exp_y}) \
                         score {score:.3} (<= {MATCH_MAX_SSE}) after {polls} poll(s), {} ms",
                        patch.w,
                        patch.h,
                        start.elapsed().as_millis(),
                    ));
                    return true;
                }
            }
            None => {
                // Log once per click: a persistent failure floods otherwise.
                if !logged_capture_fail {
                    logged_capture_fail = true;
                    crawl_log(format!(
                        "replay[{label}] patch search failed (no monitor capture / region too \
                         small) near ({exp_x},{exp_y})"
                    ));
                }
            }
        }
        if start.elapsed() >= timeout {
            crawl_log(format!(
                "replay[{label}] patch {}x{} NOT present at ({exp_x},{exp_y}) after {} ms \
                 ({polls} polls); best score {best:.3} > {MATCH_MAX_SSE} -> clicking recorded coord anyway",
                patch.w,
                patch.h,
                start.elapsed().as_millis(),
            ));
            return false;
        }
        millis(MATCH_POLL_MS).await;
    }
}

/// Replays a recorded save-as macro, then reads the preset name from the clipboard.
async fn replay_macro_and_scrape(
    mouse: &mut EnigoMouse,
    macro_events: &[InputEvent],
    escape_catcher: &EscapeCatcher,
) -> Result<ScrapedName, Box<dyn Error + Send + Sync>> {
    // Sentinel so we can tell whether the copy actually happened.
    set_clipboard_text(CLIPBOARD_SENTINEL)?;
    if !replay_macro(mouse, macro_events, escape_catcher, "scrape").await? {
        return Ok(ScrapedName::Interrupted);
    }
    a_bit_longer().await;
    let name = clipboard_text()?;
    crawl_log(format!("scrape: clipboard = {name:?}"));
    if name == CLIPBOARD_SENTINEL {
        return Err("the recorded actions didn't copy a preset name to the clipboard".into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("the recorded actions yielded an empty preset name".into());
    }
    Ok(ScrapedName::Name(name.to_string()))
}

const CLIPBOARD_SENTINEL: &str = "__POT_PRESET_CRAWLER_SENTINEL__";
const SAVE_AS_DIALOG_APPEARANCE_TIMEOUT: Duration = Duration::from_secs(10);

fn set_clipboard_text(text: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(text).map_err(|e| e.to_string())?;
    Ok(())
}

fn clipboard_text() -> Result<String, Box<dyn Error + Send + Sync>> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let text = clipboard.get_text().map_err(|e| e.to_string())?;
    Ok(text)
}

/// Returns the titles of all windows that are currently open, system-wide.
fn window_titles() -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let windows = xcap::Window::all().map_err(|e| e.to_string())?;
    Ok(windows.iter().map(|w| w.title().to_string()).collect())
}

/// Counts how often each title occurs.
///
/// We count occurrences instead of using a set because window titles are not unique.
/// In particular, several windows might carry an empty title — and a freshly opened
/// dialog might, too.
fn count_window_titles(titles: impl Iterator<Item = String>) -> NonCryptoHashMap<String, u32> {
    let mut counts: NonCryptoHashMap<String, u32> = Default::default();
    for title in titles {
        *counts.entry(title).or_default() += 1;
    }
    counts
}

/// Returns the title of a window that's in `after` but wasn't there before, if any.
fn detect_new_window_title<'a>(
    before: &NonCryptoHashMap<String, u32>,
    after: &'a [String],
) -> Option<&'a str> {
    let mut seen: NonCryptoHashMap<&str, u32> = Default::default();
    for title in after {
        let n = seen.entry(title).or_default();
        *n += 1;
        if *n > before.get(title.as_str()).copied().unwrap_or(0) {
            return Some(title);
        }
    }
    None
}

fn determine_preset_file_destination(
    fx_info: &FxInfo,
    reaper_resource_dir: &Utf8Path,
    preset_name: &str,
    plugin_id: Option<&PluginId>,
) -> Utf8PathBuf {
    if let Some(persistent_preset_id) = find_shimmable_preset(plugin_id, preset_name) {
        // Matched with existing unsupported preset. Create RfxChain file, a so called shim file,
        // but not in the FX chain directory because we don't want it to show up in the FX chain
        // database. Instead, we want the original preset (probably in the Komplete database)
        // to become loadable. There's logic in our preset loading mechanism that looks for
        // a shim file if it realizes that the preset can't be loaded. A kind of fallback!
        get_shim_file_path(reaper_resource_dir, &persistent_preset_id)
    } else {
        // No match with existing unsupported preset
        let sanitized_effect_name = sanitize_filename::sanitize(&fx_info.effect_name);
        let file_name = format!("{}.RfxChain", &preset_name);
        let sanitized_file_name = sanitize_filename::sanitize(file_name);
        reaper_resource_dir
            .join("FXChains/Pot")
            .join(sanitized_effect_name)
            .join(sanitized_file_name)
    }
}

/// Returns the file name of the original preset.
fn find_shimmable_preset(
    plugin_id: Option<&PluginId>,
    preset_name: &str,
) -> Option<PersistentPresetId> {
    let plugin_id = plugin_id?;
    pot_db().find_unsupported_preset_matching(plugin_id, preset_name)
}

fn get_plugin_id_from_fx_info(fx_info: &FxInfo) -> Option<PluginId> {
    let plugin_id = match fx_info.sub_type_expression.as_str() {
        "VST" | "VSTi" => PluginId::vst2(parse_vst2_magic_number(&fx_info.id).ok()?),
        "VST3" | "VST3i" => PluginId::vst3(parse_vst3_uid(&fx_info.id).ok()?),
        // Komplete doesn't support CLAP or JS anyway, so not important right now.
        _ => return None,
    };
    Some(plugin_id)
}

pub async fn import_crawled_presets(
    state: SharedPresetCrawlingState,
    mut chunks_file: File,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        let p = blocking_lock_arc(&state, "import_crawled_presets").pop_crawled_preset();
        let Some(p) = p else {
            break;
        };
        let dest_file_path = &p.destination;
        let dest_dir_path = p.destination.parent().ok_or("destination without parent")?;
        fs::create_dir_all(dest_dir_path)?;
        chunks_file.seek(SeekFrom::Start(p.offset))?;
        let mut buf = vec![0; p.size_in_bytes];
        chunks_file.read_exact(&mut buf)?;
        fs::write(dest_file_path, buf)?;
    }
    Ok(())
}

async fn a_bit_longer() {
    millis(100).await;
}

async fn moment() {
    millis(50).await;
}

const MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS: u32 = 10;
const MAX_SAME_PRESET_NAME_LIKE_BEGINNING_ATTEMPTS: u32 = 10;

pub fn get_shim_file_path(
    reaper_resource_dir: &Utf8Path,
    preset_id: &PersistentPresetId,
) -> Utf8PathBuf {
    // We don't need to
    let hash =
        hash_util::calculate_persistent_non_crypto_hash_one_shot(preset_id.to_string().as_bytes());
    let file_name = file_util::convert_hash_to_dir_structure(hash, ".RfxChain");
    reaper_resource_dir
        .join("Helgoboss/Pot/shims")
        .join(file_name)
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PresetCrawlerStopReason {
    Interrupted,
    DestinationFileExists,
    PresetNameNotChangingAnymore,
    PresetNameLikeBeginning,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str) -> CrawledPreset {
        CrawledPreset {
            name: name.to_string(),
            offset: 0,
            size_in_bytes: 10,
            destination: Utf8PathBuf::from(format!("/tmp/pot-test/{name}.RfxChain")),
        }
    }

    fn add(state: &SharedPresetCrawlingState, name: &str) -> NextCrawlStep {
        blocking_lock_arc(state, "test").add_preset(preset(name), false)
    }

    #[test]
    fn distinct_names_continue_and_accumulate() {
        let state = PresetCrawlingState::new();
        for name in ["a", "b", "c"] {
            assert!(matches!(add(&state, name), NextCrawlStep::Continue));
        }
        let state = blocking_lock_arc(&state, "test");
        assert_eq!(state.preset_count(), 3);
        assert_eq!(state.duplicate_preset_name_count(), 0);
        assert_eq!(state.bytes_crawled(), 30);
    }

    #[test]
    fn intermediate_duplicate_is_skipped() {
        let state = PresetCrawlingState::new();
        add(&state, "a");
        add(&state, "b");
        // Duplicate of an earlier (non-adjacent) preset
        add(&state, "a");
        // Next one is fresh again, which proves "a" was an intermediate duplicate
        add(&state, "c");
        let state = blocking_lock_arc(&state, "test");
        assert_eq!(state.preset_count(), 3);
        assert_eq!(state.duplicate_preset_names(), &["a".to_string()]);
    }

    #[test]
    fn same_name_in_a_row_is_tolerated_within_limit() {
        let state = PresetCrawlingState::new();
        add(&state, "a");
        // The plug-in might crop preset names, so a few repeats are tolerated
        for _ in 0..MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS {
            assert!(matches!(add(&state, "a"), NextCrawlStep::Continue));
        }
    }

    #[test]
    fn same_name_in_a_row_stops_crawling_beyond_limit() {
        let state = PresetCrawlingState::new();
        add(&state, "a");
        let mut last_step = NextCrawlStep::Continue;
        for _ in 0..=MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS + 1 {
            last_step = add(&state, "a");
        }
        assert!(matches!(
            last_step,
            NextCrawlStep::Stop(PresetCrawlerStopReason::PresetNameNotChangingAnymore)
        ));
    }

    #[test]
    fn wrap_around_to_beginning_stops_crawling() {
        let state = PresetCrawlingState::new();
        // Crawl a healthy number of distinct presets
        let names: Vec<String> = (0..20).map(|i| format!("preset-{i}")).collect();
        for name in &names {
            assert!(matches!(add(&state, name), NextCrawlStep::Continue));
        }
        // Now the preset list wraps around to the beginning
        let mut last_step = NextCrawlStep::Continue;
        for name in &names {
            last_step = add(&state, name);
            if matches!(last_step, NextCrawlStep::Stop(_)) {
                break;
            }
        }
        assert!(matches!(
            last_step,
            NextCrawlStep::Stop(PresetCrawlerStopReason::PresetNameLikeBeginning)
        ));
    }

    fn titles(titles: &[&str]) -> Vec<String> {
        titles.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn detects_window_with_fresh_title() {
        let before = count_window_titles(titles(&["REAPER", "Zebra2"]).into_iter());
        let after = titles(&["REAPER", "Zebra2", "Save Preset"]);
        assert_eq!(detect_new_window_title(&before, &after), Some("Save Preset"));
    }

    #[test]
    fn detects_no_new_window_when_nothing_changed() {
        let before = count_window_titles(titles(&["REAPER", "Zebra2"]).into_iter());
        let after = titles(&["REAPER", "Zebra2"]);
        assert_eq!(detect_new_window_title(&before, &after), None);
    }

    #[test]
    fn detects_no_new_window_when_window_closed() {
        let before = count_window_titles(titles(&["REAPER", "Zebra2"]).into_iter());
        let after = titles(&["REAPER"]);
        assert_eq!(detect_new_window_title(&before, &after), None);
    }

    #[test]
    fn detects_additional_window_with_duplicate_title() {
        // Several windows often carry an empty title; a freshly opened dialog might, too.
        let before = count_window_titles(titles(&["REAPER", "", ""]).into_iter());
        let after = titles(&["REAPER", "", "", ""]);
        assert_eq!(detect_new_window_title(&before, &after), Some(""));
    }

    #[test]
    fn ignores_replaced_window_with_known_title() {
        // A window disappears while another one with an already-known title appears:
        // same count, no detection. This is intended — we only react to *more* windows
        // of a title than before.
        let before = count_window_titles(titles(&["REAPER", "Zebra2"]).into_iter());
        let after = titles(&["Zebra2", "REAPER"]);
        assert_eq!(detect_new_window_title(&before, &after), None);
    }

    #[test]
    fn never_stop_mode_records_duplicates_but_continues() {
        let state = PresetCrawlingState::new();
        {
            let mut state = blocking_lock_arc(&state, "test");
            state.add_preset(preset("a"), true);
            for _ in 0..MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS + 5 {
                let step = state.add_preset(preset("a"), true);
                assert!(matches!(step, NextCrawlStep::Continue));
            }
        }
        let state = blocking_lock_arc(&state, "test");
        assert_eq!(state.preset_count(), 1);
    }
}
