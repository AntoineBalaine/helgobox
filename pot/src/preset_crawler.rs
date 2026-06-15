use crate::preset_recorder::{topmost_window_at, ClickButton, InputEvent, RecordedMacro};
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
use reaper_high::{Fx, FxInfo, Reaper};
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    loop {
        // Check if escape has been pressed
        if escape_catcher.escape_was_pressed() {
            return Ok(PresetCrawlingOutcome::new(
                chunks_file,
                PresetCrawlerStopReason::Interrupted,
            ));
        }
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
                return Ok(PresetCrawlingOutcome::new(
                    chunks_file,
                    PresetCrawlerStopReason::Interrupted,
                ));
            }
        };
        {
            // Query chunk and save it in temporary file
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
                    return Ok(PresetCrawlingOutcome::new(chunks_file, reason));
                }
                NextCrawlStep::Continue => {}
            }
        }
        // Advance to the next preset: replay the recorded macro if present (verified,
        // window-relative), otherwise click the single captured position.
        args.fx.show_in_floating_window()?;
        if let Some(next_macro) = &args.next_preset_macro {
            if !replay_macro(&mut mouse, next_macro, &escape_catcher).await? {
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

/// Resolves a recorded click to absolute screen coordinates.
///
/// - Window still open: re-resolve its current position and **verify it's topmost** at the
///   target point; abort if a different window covers it (never click the wrong window).
/// - Window gone: it was transient (a dialog or context menu that reopens with a fresh id,
///   e.g. a native save panel or a right-click "Copy" menu). Fall back to the recorded
///   absolute position — it reopens at the same place.
///
/// xcap `Window` handles are dropped before returning (they may be `!Send`, so must not be
/// held across the caller's await points).
fn resolve_click_target(
    win_id: u32,
    off_x: i32,
    off_y: i32,
    abs_x: i32,
    abs_y: i32,
) -> Result<(i32, i32), Box<dyn Error + Send + Sync>> {
    if win_id == 0 {
        return Ok((abs_x, abs_y));
    }
    match find_window_by_id(win_id) {
        Some(win) => {
            let (tx, ty) = (win.x() + off_x, win.y() + off_y);
            match topmost_window_at(tx, ty) {
                Some(top) if top.id() == win_id => Ok((tx, ty)),
                Some(top) => Err(format!(
                    "expected window {win_id} at ({tx},{ty}) but '{}' is on top — aborting to \
                     avoid clicking the wrong window",
                    top.app_name()
                )
                .into()),
                None => Err(format!("no window at ({tx},{ty}) during replay").into()),
            }
        }
        None => Ok((abs_x, abs_y)),
    }
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

/// Replays a recorded action macro. Returns `Ok(false)` if the user pressed Escape.
///
/// Clicks are resolved/verified by [`resolve_click_target`]; keys replay by physical
/// position (`Key::Raw`), which reproduces the user's keystrokes under any keyboard layout.
/// Timing follows the recorded inter-event delays (with a fast down/up) so quick sequences
/// like triple-clicks replay as triple-clicks rather than separate clicks.
async fn replay_macro(
    mouse: &mut EnigoMouse,
    macro_events: &[InputEvent],
    escape_catcher: &EscapeCatcher,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    for ev in macro_events {
        if escape_catcher.escape_was_pressed() {
            return Ok(false);
        }
        match ev {
            InputEvent::Click {
                button,
                win_id,
                off_x,
                off_y,
                abs_x,
                abs_y,
                clicks,
                delay_ms,
            } => {
                millis((*delay_ms).min(5000)).await;
                let (tx, ty) = resolve_click_target(*win_id, *off_x, *off_y, *abs_x, *abs_y)?;
                let pos = MouseCursorPosition::new(tx.max(0) as u32, ty.max(0) as u32);
                mouse.set_cursor_position(pos)?;
                millis(12).await;
                // On macOS the click must carry the multi-click state so the OS treats a
                // recorded triple-click as a triple-click (enigo can't set it); elsewhere a
                // plain click with the recorded timing suffices.
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
            InputEvent::KeyDown { raw, delay_ms } => {
                millis((*delay_ms).min(5000)).await;
                mouse.press_raw_key(*raw);
            }
            InputEvent::KeyUp { raw, delay_ms } => {
                millis((*delay_ms).min(5000)).await;
                mouse.release_raw_key(*raw);
            }
        }
    }
    Ok(true)
}

/// Replays a recorded save-as macro, then reads the preset name from the clipboard.
async fn replay_macro_and_scrape(
    mouse: &mut EnigoMouse,
    macro_events: &[InputEvent],
    escape_catcher: &EscapeCatcher,
) -> Result<ScrapedName, Box<dyn Error + Send + Sync>> {
    // Sentinel so we can tell whether the copy actually happened.
    set_clipboard_text(CLIPBOARD_SENTINEL)?;
    if !replay_macro(mouse, macro_events, escape_catcher).await? {
        return Ok(ScrapedName::Interrupted);
    }
    a_bit_longer().await;
    let name = clipboard_text()?;
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
