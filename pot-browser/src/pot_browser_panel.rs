use anyhow::anyhow;
use base::enigo::EnigoMouse;
use base::{
    blocking_lock, blocking_lock_arc, blocking_read_lock, NamedChannelSender, SenderToNormalThread,
};
use base::{Mouse, MouseCursorPosition};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Local, Utc};
use crossbeam_channel::Receiver;
use egui::collapsing_header::CollapsingState;
use egui::{
    popup_below_widget, vec2, Align, Align2, Button, CentralPanel, Color32, DragValue, Event,
    FontFamily, FontId, Frame, InputState, Key, Label, Layout, RichText, ScrollArea, TextEdit,
    TextStyle, TopBottomPanel, Ui, Visuals, Widget, WidgetText,
};
use egui::{Context, SidePanel};
use egui_extras::{Column, Size, StripBuilder, TableBuilder};
use egui_toast::Toasts;
use helgobox_api::persistence::PotFilterKind;
use lru::LruCache;
use pot::preset_crawler::{
    crawl_presets, import_crawled_presets, CrawlPresetArgs, PresetCrawlerStopReason,
    PresetCrawlingState, SaveAsDialogScraping, SharedPresetCrawlingState,
};
use pot::preview_recorder::{
    prepare_preview_recording, record_previews, ExportPreviewOutputConfig, PreviewOutputConfig,
    PreviewRecorderFailure, PreviewRecorderState, RecordPreviewsArgs, SharedPreviewRecorderState,
};
use pot::providers::projects::{ProjectDatabase, ProjectDbConfig};
use pot::{
    create_plugin_factory_preset, find_preview_file, pot_db, spawn_in_pot_worker, ChangeHint,
    Debounce, DestinationTrackDescriptor, FiledBasedPotPresetKind, Filters,
    LoadAudioSampleBehavior, LoadPresetOptions, LoadPresetWindowBehavior,
    MainThreadDispatcher, MainThreadSpawner, OptFilter, PersistentDatabaseId,
    PotFavorites, PotFilterExcludes, PotPreset, PotPresetKind, PotWorkerDispatcher,
    PotWorkerSpawner, PresetWithId, RuntimePotUnit, SearchField, SharedRuntimePotUnit,
    WorkerDispatcher,
};
use crate::reaper_commands::{ReaperCommand, UiFeedback};
use crate::reaper_frame::{CurrentPresetPanelSnapshot, DestinationSnapshot, ReaperFrame};
use pot::{FilterItemId, PresetId};
use reaper_high::{Fx, Track};
use reaper_medium::ReaperVolumeValue;
use std::borrow::Cow;
use std::error::Error;
use std::fs::File;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, MutexGuard, RwLock};
use std::time::{Duration, Instant};
use std::{fs, mem};
use strum::IntoEnumIterator;
use swell_ui::Window;
use url::Url;

pub trait PotBrowserIntegration {
    fn get_track_label(&self, track: &Track) -> String;
    fn pot_preview_template_path(&self) -> Option<&'static Utf8Path>;
    fn pot_favorites(&self) -> &'static RwLock<PotFavorites>;
    fn with_current_fx_preset(&self, fx: &Fx, f: impl FnOnce(Option<&pot::CurrentPreset>));
    fn with_pot_filter_exclude_list(&self, f: impl FnOnce(&PotFilterExcludes));
}

#[derive(Debug)]
pub struct State {
    page: Page,
    dont_show_warning_again: bool,
    main_state: TopLevelMainState,
}

/// On X11, the state lives on baseview's render thread, so it must be `Send`.
/// This assertion makes any non-Send member a compile error instead of a runtime panic.
trait AssertSend: Send {}
impl AssertSend for State {}

impl State {
    /// `show_warning` should be `!warning_was_acknowledged(...)` — checked by the host
    /// because the render code can't necessarily resolve the resource path that early.
    pub fn new(pot_unit: SharedRuntimePotUnit, os_window: Window, show_warning: bool) -> Self {
        Self {
            page: if show_warning {
                Page::Warning
            } else {
                Page::Main
            },
            dont_show_warning_again: false,
            main_state: TopLevelMainState::new(pot_unit, os_window),
        }
    }
}

#[derive(Debug, Default)]
enum Page {
    #[default]
    Warning,
    Main,
}

#[derive(Debug)]
pub struct TopLevelMainState {
    pot_worker_dispatcher: CustomPotWorkerDispatcher,
    main_thread_dispatcher: CustomMainThreadDispatcher,
    main_state: MainState,
}

impl TopLevelMainState {
    pub fn new(pot_unit: SharedRuntimePotUnit, os_window: Window) -> Self {
        Self {
            pot_worker_dispatcher: WorkerDispatcher::new(PotWorkerSpawner),
            main_thread_dispatcher: WorkerDispatcher::new(MainThreadSpawner),
            main_state: MainState::new(pot_unit, os_window),
        }
    }
}

#[derive(Debug)]
pub struct MainState {
    pot_unit: SharedRuntimePotUnit,
    os_window: Window,
    auto_preview: bool,
    auto_hide_sub_filters: bool,
    show_stats: bool,
    paint_continuously: bool,
    last_preset_id: Option<PresetId>,
    last_filters: Filters,
    /// Shared with [`HostBridge`]: the frame capture on the main thread needs to know
    /// which macro-param bank is selected.
    bank_index: Arc<AtomicU32>,
    preset_cache: PresetCache,
    dialog: Option<Dialog>,
    mouse: EnigoMouse,
    has_shown_legacy_vst3_scan_warning: bool,
    /// REAPER-mutating commands queued by the render code, executed on the main thread
    /// via [`process_pending_reaper_commands`].
    command_sender: SenderToNormalThread<ReaperCommand>,
    command_receiver: Receiver<ReaperCommand>,
    /// Feedback from main-thread command execution, drained by the render code each frame.
    feedback_sender: SenderToNormalThread<UiFeedback>,
    feedback_receiver: Receiver<UiFeedback>,
}

type CustomPotWorkerDispatcher = PotWorkerDispatcher<MainState>;
type CustomMainThreadDispatcher = MainThreadDispatcher<MainState>;

#[derive(Debug)]
enum Dialog {
    GeneralError {
        title: Cow<'static, str>,
        msg: Cow<'static, str>,
    },
    AddProjectDatabase {
        folder: String,
        name: String,
    },
    PresetCrawlerIntro,
    PresetCrawlerBasics,
    PresetCrawlerMouse {
        creation_time: Instant,
    },
    /// Countdown for capturing the position of the plug-in's "Save Preset As" button
    /// (only used when preset names are scraped from the save dialog).
    PresetCrawlerMouseSaveAs {
        creation_time: Instant,
        fx: Fx,
        fx_name: String,
        next_preset_cursor_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
    },
    /// Countdown for capturing the position of the save dialog's "Cancel" button
    /// (only used when preset names are scraped from the save dialog).
    PresetCrawlerMouseCancel {
        creation_time: Instant,
        fx: Fx,
        fx_name: String,
        next_preset_cursor_pos: MouseCursorPosition,
        save_as_button_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
    },
    PresetCrawlerReady {
        fx: Fx,
        /// Captured at focus time so the dialog doesn't have to ask REAPER for it.
        fx_name: String,
        cursor_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
        use_save_as_dialog: bool,
        save_as_dialog: Option<SaveAsDialogScraping>,
    },
    PresetCrawlerFailure {
        short_msg: Cow<'static, str>,
        detail_msg: Cow<'static, str>,
    },
    PresetCrawlerCrawling {
        crawling_state: SharedPresetCrawlingState,
    },
    PresetCrawlerStopped {
        crawling_state: SharedPresetCrawlingState,
        stop_reason: PresetCrawlerStopReason,
        page: CrawlPresetsStoppedPage,
        chunks_file: Option<File>,
        crawled_preset_count: u32,
    },
    PresetCrawlerFinished {
        stop_reason: PresetCrawlerStopReason,
        crawled_preset_count: u32,
    },
    PreviewRecorderIntro,
    PreviewRecorderBasics,
    PreviewRecorderPreparing,
    PreviewRecorderReadyToRecord {
        presets: Vec<PresetWithId>,
        output_config: PreviewOutputConfig,
    },
    PreviewRecorderRecording {
        state: SharedPreviewRecorderState,
    },
    PreviewRecorderDone {
        state: SharedPreviewRecorderState,
        page: PreviewRecorderDonePage,
        output_config: PreviewOutputConfig,
    },
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CrawlPresetsStoppedPage {
    Presets,
    Duplicates,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PreviewRecorderDonePage {
    Todos,
    Failures,
}

impl Dialog {
    fn general_error(
        title: impl Into<Cow<'static, str>>,
        msg: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::GeneralError {
            title: title.into(),
            msg: msg.into(),
        }
    }

    fn add_project_database(folder: String) -> Self {
        let suggested_name = Path::new(&folder)
            .file_name()
            .and_then(|n| Some(n.to_str()?.to_string()))
            .unwrap_or_default();
        Self::AddProjectDatabase {
            folder,
            name: suggested_name,
        }
    }

    fn preset_crawler_basics() -> Self {
        Self::PresetCrawlerBasics
    }

    fn preset_crawler_mouse() -> Self {
        Self::PresetCrawlerMouse {
            creation_time: Instant::now(),
        }
    }

    fn preset_crawler_ready(fx: Fx, fx_name: String, cursor_pos: MouseCursorPosition) -> Self {
        Self::PresetCrawlerReady {
            fx,
            fx_name,
            cursor_pos,
            stop_if_destination_exists: false,
            never_stop_crawling: false,
            use_save_as_dialog: false,
            save_as_dialog: None,
        }
    }

    fn preset_crawler_mouse_save_as(
        fx: Fx,
        fx_name: String,
        next_preset_cursor_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
    ) -> Self {
        Self::PresetCrawlerMouseSaveAs {
            creation_time: Instant::now(),
            fx,
            fx_name,
            next_preset_cursor_pos,
            stop_if_destination_exists,
            never_stop_crawling,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn preset_crawler_mouse_cancel(
        fx: Fx,
        fx_name: String,
        next_preset_cursor_pos: MouseCursorPosition,
        save_as_button_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
    ) -> Self {
        Self::PresetCrawlerMouseCancel {
            creation_time: Instant::now(),
            fx,
            fx_name,
            next_preset_cursor_pos,
            save_as_button_pos,
            stop_if_destination_exists,
            never_stop_crawling,
        }
    }

    fn preset_crawler_ready_with_scraping(
        fx: Fx,
        fx_name: String,
        cursor_pos: MouseCursorPosition,
        stop_if_destination_exists: bool,
        never_stop_crawling: bool,
        save_as_dialog: SaveAsDialogScraping,
    ) -> Self {
        Self::PresetCrawlerReady {
            fx,
            fx_name,
            cursor_pos,
            stop_if_destination_exists,
            never_stop_crawling,
            use_save_as_dialog: true,
            save_as_dialog: Some(save_as_dialog),
        }
    }

    fn preset_crawler_failure(
        short_msg: impl Into<Cow<'static, str>>,
        detail_msg: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::PresetCrawlerFailure {
            short_msg: short_msg.into(),
            detail_msg: detail_msg.into(),
        }
    }

    fn preset_crawler_crawling(crawling_state: SharedPresetCrawlingState) -> Self {
        Self::PresetCrawlerCrawling { crawling_state }
    }

    fn preset_crawler_stopped(
        crawling_state: SharedPresetCrawlingState,
        stop_reason: PresetCrawlerStopReason,
        chunks_file: File,
        crawled_preset_count: u32,
    ) -> Self {
        Self::PresetCrawlerStopped {
            crawling_state,
            stop_reason,
            page: CrawlPresetsStoppedPage::Presets,
            chunks_file: Some(chunks_file),
            crawled_preset_count,
        }
    }

    fn preset_crawler_finished(
        crawled_preset_count: u32,
        stop_reason: PresetCrawlerStopReason,
    ) -> Self {
        Self::PresetCrawlerFinished {
            crawled_preset_count,
            stop_reason,
        }
    }

    fn preview_recorder_preparing() -> Self {
        Self::PreviewRecorderPreparing
    }

    fn preview_recorder_ready_to_record(
        presets: Vec<PresetWithId>,
        output_config: PreviewOutputConfig,
    ) -> Self {
        Self::PreviewRecorderReadyToRecord {
            presets,
            output_config,
        }
    }

    fn preview_recorder_recording(state: SharedPreviewRecorderState) -> Self {
        Self::PreviewRecorderRecording { state }
    }

    fn preview_recorder_done(
        state: SharedPreviewRecorderState,
        output_config: PreviewOutputConfig,
    ) -> Self {
        Self::PreviewRecorderDone {
            state,
            page: PreviewRecorderDonePage::Todos,
            output_config,
        }
    }
}

struct PresetCacheMessage {
    pot_db_revision: u8,
    preset_id: PresetId,
    preset_data: Option<PotPresetData>,
}

#[derive(Debug)]
enum PresetCacheEntry {
    Requested,
    NotFound,
    Found(Box<PotPresetData>),
}

#[derive(Debug)]
struct PotPresetData {
    preset: PotPreset,
    preview_file: Option<Utf8PathBuf>,
}

pub fn run_ui<I: PotBrowserIntegration>(
    ctx: &Context,
    state: &mut State,
    integration: &I,
    frame: &ReaperFrame,
) {
    match state.page {
        Page::Warning => {
            run_warning_ui(ctx, state, frame);
        }
        Page::Main => run_main_ui(ctx, &mut state.main_state, integration, frame),
    }
}

/// Path of the marker file that records that the user doesn't want to see the
/// experimental-state warning again.
fn warning_acknowledged_marker_path(reaper_resource_dir: &Utf8Path) -> Utf8PathBuf {
    reaper_resource_dir.join("Helgoboss/Pot/browser-warning-acknowledged")
}

/// Returns whether the user previously chose not to see the startup warning again.
///
/// The host should check this (on any thread — it's just a file probe) and pass the
/// result to [`State::new`].
pub fn warning_was_acknowledged(reaper_resource_dir: &Utf8Path) -> bool {
    warning_acknowledged_marker_path(reaper_resource_dir).exists()
}

fn run_warning_ui(ctx: &Context, state: &mut State, frame: &ReaperFrame) {
    CentralPanel::default().show(ctx, |_| {
        egui::Window::new("A word of caution")
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.style_mut().text_styles = {
                    use FontFamily as F;
                    use TextStyle as T;
                    [
                        (T::Heading, FontId::new(30.0, F::Proportional)),
                        (T::Body, FontId::new(15.0, F::Proportional)),
                        (T::Monospace, FontId::new(14.0, F::Proportional)),
                        (T::Button, FontId::new(20.0, F::Proportional)),
                        (T::Small, FontId::new(10.0, F::Proportional)),
                    ]
                    .into()
                };
                ui.vertical_centered(|ui| {
                    ui.label(
                        "Although being very useful already for daily preset browsing and preview \
                        recording, Pot Browser is still in its infancy. It will not save any of \
                        your settings!",
                    );
                    ui.add_space(20.0);
                    ui.label(
                        RichText::new(
                            "Therefore, better don't spend much time yet creating the perfect \
                            configuration! It will be gone after REAPER restarts.",
                        )
                        .strong(),
                    );
                    ui.add_space(20.0);
                    ui.checkbox(&mut state.dont_show_warning_again, "Don't show this again");
                    ui.add_space(10.0);
                    let button = Button::new("I understood. Really!");
                    if ui.add(button).clicked() {
                        if state.dont_show_warning_again {
                            // Plain file I/O, deliberately not a REAPER API call: this
                            // runs on the render thread.
                            let marker_path =
                                warning_acknowledged_marker_path(&frame.resource_path);
                            if let Some(parent) = marker_path.parent() {
                                let _ = fs::create_dir_all(parent);
                            }
                            let _ = fs::write(&marker_path, b"");
                        }
                        state.page = Page::Main;
                    }
                })
            });
    });
}

const PRESET_CRAWLER_TITLE: &str = "Preset Crawler";
const PREVIEW_RECORDER_TITLE: &str = "Preview Recorder";
const PRESET_CRAWLER_COUNTDOWN_DURATION: Duration = Duration::from_secs(10);

fn run_main_ui<I: PotBrowserIntegration>(
    ctx: &Context,
    state: &mut TopLevelMainState,
    integration: &I,
    frame: &ReaperFrame,
) {
    // Poll background task results
    state.pot_worker_dispatcher.poll(&mut state.main_state);
    state.main_thread_dispatcher.poll(&mut state.main_state);
    // We need the pot unit throughout the complete UI building process
    let pot_unit = &mut blocking_lock(
        &*state.main_state.pot_unit,
        "PotUnit from PotBrowserPanel run_ui 1",
    );
    // Query commonly used stuff
    let background_task_elapsed = pot_unit.background_task_elapsed();
    // Integrate cache worker results into local cache
    state
        .main_state
        .preset_cache
        .set_pot_db_revision(pot_db().revision());
    state
        .main_state
        .preset_cache
        .set_reaper_resource_dir(&frame.resource_path);
    while let Ok(message) = state.main_state.preset_cache.receiver.try_recv() {
        state.main_state.preset_cache.process_message(message);
    }
    // Integrate feedback from main-thread command execution
    while let Ok(feedback) = state.main_state.feedback_receiver.try_recv() {
        match feedback {
            UiFeedback::Error(msg) => report_ui_error(msg),
            UiFeedback::UnsupportedPresetFormat {
                file_extension,
                is_shim_preset,
            } => {
                if is_shim_preset {
                    report_ui_error(format!(
                        "Found shim preset for unsupported original format but even the shim \
                        seems to have an unsupported format: {file_extension}",
                    ));
                } else {
                    state.main_state.dialog = Some(Dialog::general_error(
                        "Can't open preset",
                        UNSUPPORTED_PRESET_FORMAT_TEXT,
                    ));
                }
            }
        }
    }
    // Prepare toasts
    let toast_margin = 10.0;
    let mut toasts = Toasts::new()
        .anchor(ctx.screen_rect().max - vec2(toast_margin, toast_margin))
        .direction(egui::Direction::RightToLeft)
        .align_to_end(true);
    // Warning dialog
    if !state.main_state.has_shown_legacy_vst3_scan_warning && pot_db().detected_legacy_vst3_scan()
    {
        state.main_state.has_shown_legacy_vst3_scan_warning = true;
        state.main_state.dialog = Some(Dialog::general_error("Warning", LEGACY_VST3_SCAN_WARNING));
    }
    // Process dialogs
    let mut change_dialog = None;
    if let Some(dialog) = state.main_state.dialog.as_mut() {
        let input = ProcessDialogsInput {
            shared_pot_unit: &state.main_state.pot_unit,
            pot_unit,
            dialog,
            mouse: &state.main_state.mouse,
            os_window: state.main_state.os_window,
            change_dialog: &mut change_dialog,
            pot_worker_dispatcher: &mut state.pot_worker_dispatcher,
            main_thread_dispatcher: &mut state.main_thread_dispatcher,
            frame,
        };
        process_dialogs(input, ctx);
    }
    if let Some(d) = change_dialog {
        state.main_state.dialog = d;
    }
    // Process keyboard
    let key_action =
        ctx.input_mut(|input| determine_key_action(input, &mut state.main_state.dialog));
    if let Some(key_action) = key_action {
        let key_input = KeyInput {
            auto_preview: state.main_state.auto_preview,
            os_window: state.main_state.os_window,
            pot_unit: state.main_state.pot_unit.clone(),
            command_sender: &state.main_state.command_sender,
        };
        execute_key_action(key_input, pot_unit, &mut toasts, key_action);
    }
    let current_fx = frame.current_fx.as_ref();
    // UI
    let panel_frame = Frame::central_panel(&ctx.style());
    // Upper panel (currently loaded preset with macro controls)
    if let Some(panel) = &frame.current_preset_panel {
        TopBottomPanel::top("top-bottom-panel")
            .frame(panel_frame)
            .min_height(50.0)
            .show(ctx, |ui| {
                show_current_preset_panel(
                    &state.main_state.bank_index,
                    panel,
                    &state.main_state.command_sender,
                    ui,
                );
            });
    }
    // Main panel
    CentralPanel::default()
        .frame(Frame::none())
        .show(ctx, |ui| {
            // Left pane
            SidePanel::left("left-panel")
                .frame(panel_frame)
                .default_width(ctx.available_rect().width() * 0.5)
                .show_inside(ui, |ui| {
                    ui.style_mut().text_styles.insert(
                        TextStyle::Heading,
                        FontId::new(15.0, FontFamily::Proportional),
                    );
                    // Toolbar
                    ui.horizontal(|ui| {
                        left_right(
                            ui,
                            pot_unit,
                            TOOLBAR_HEIGHT_WITH_MARGIN,
                            280.0,
                            // Left side: Toolbar
                            |ui, pot_unit| {
                                // Main options
                                let input = LeftOptionsDropdownInput {
                                    pot_unit,
                                    auto_hide_sub_filters: &mut state
                                        .main_state
                                        .auto_hide_sub_filters,
                                    paint_continuously: &mut state.main_state.paint_continuously,
                                    shared_pot_unit: &state.main_state.pot_unit,
                                };
                                add_left_options_dropdown(input, ui);
                                // Refresh button
                                ui.add_enabled_ui(!pot_unit.is_refreshing(), |ui| {
                                    if ui
                                        .button(RichText::new("🔃").size(TOOLBAR_HEIGHT))
                                        .on_hover_text(
                                            "Refreshes all databases (e.g. picks up new \
                                    files on disk)",
                                        )
                                        .clicked()
                                    {
                                        pot_unit.refresh_pot(state.main_state.pot_unit.clone());
                                    }
                                });
                                // Theme button
                                if ui
                                    .button(RichText::new("🌙").size(TOOLBAR_HEIGHT))
                                    .on_hover_text("Switches between light and dark theme")
                                    .clicked()
                                {
                                    let mut style: egui::Style = (*ctx.style()).clone();
                                    style.visuals = if style.visuals.dark_mode {
                                        Visuals::light()
                                    } else {
                                        Visuals::dark()
                                    };
                                    ctx.set_style(style);
                                }
                                // Help button
                                add_help_button(ui);
                                // Spinner
                                if background_task_elapsed.is_some() {
                                    ui.spinner();
                                }
                            },
                            // Right side: Mini filters
                            |ui, pot_unit| {
                                if pot_unit.filter_item_collections.are_filled_already() {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        add_mini_filters(&state.main_state.pot_unit, pot_unit, ui);
                                    });
                                }
                            },
                        );
                    });
                    // Filter panels
                    add_filter_panels(
                        &state.main_state.pot_unit,
                        pot_unit,
                        state.main_state.auto_hide_sub_filters,
                        ui,
                        &state.main_state.last_filters,
                        &mut state.main_state.dialog,
                        &FilterInput {
                            excludes: &frame.filter_excludes,
                            command_sender: &state.main_state.command_sender,
                        },
                        &frame.resource_path,
                    );
                });
            // Right pane
            CentralPanel::default()
                .frame(panel_frame)
                .show_inside(ui, |ui| {
                    // Toolbar
                    ui.horizontal(|ui| {
                        left_right(
                            ui,
                            pot_unit,
                            TOOLBAR_HEIGHT_WITH_MARGIN,
                            80.0,
                            // Left side: Toolbar
                            |ui, pot_unit| {
                                // Actions
                                ui.menu_button(RichText::new("Tools").size(TOOLBAR_HEIGHT), |ui| {
                                    if ui.button(PRESET_CRAWLER_TITLE).clicked() {
                                        state.main_state.dialog = Some(Dialog::PresetCrawlerIntro);
                                        ui.close_menu();
                                    }
                                    if ui.button(PREVIEW_RECORDER_TITLE).clicked() {
                                        state.main_state.dialog =
                                            Some(Dialog::PreviewRecorderIntro);
                                        ui.close_menu();
                                    }
                                });
                                // Options
                                let input = RightOptionsDropdownInput {
                                    pot_unit,
                                    shared_pot_unit: &state.main_state.pot_unit,
                                    show_stats: &mut state.main_state.show_stats,
                                    auto_preview: &mut state.main_state.auto_preview,
                                    command_sender: &state.main_state.command_sender,
                                };
                                add_right_options_dropdown(input, ui);
                                // Search field
                                let text_edit = TextEdit::singleline(
                                    &mut pot_unit.runtime_state.search_expression,
                                )
                                // .min_size(vec2(0.0, TOOLBAR_SIZE))
                                .desired_width(140.0)
                                .clip_text(false)
                                .hint_text("Enter search text!")
                                .font(TextStyle::Monospace);
                                ui.add_enabled(false, text_edit).on_disabled_hover_text(
                                    "Type anywhere to search!\nUse backspace to clear \
                        the last character\nand (Ctrl+Alt)/(Cmd)+Backspace to clear all.",
                                );
                                // Preset count
                                let preset_count = pot_unit.preset_count();
                                ui.label(format!("➡ {preset_count} presets"));
                            },
                            // Right side: Mini filters
                            |ui, pot_unit| {
                                add_filter_view_content_as_icons(
                                    &state.main_state.pot_unit,
                                    pot_unit,
                                    PotFilterKind::HasPreview,
                                    ui,
                                );
                            },
                        );
                    });
                    // Stats
                    if state.main_state.show_stats {
                        ui.separator();
                        ui.horizontal(|ui| {
                            add_stats_panel(pot_unit, background_task_elapsed, ui);
                        });
                    }
                    // Info about selected preset
                    let current_preset_id = pot_unit.preset_id();
                    let current_preset_id_and_data = current_preset_id.and_then(|id| {
                        match state.main_state.preset_cache.find_preset(id) {
                            PresetCacheEntry::Requested => None,
                            PresetCacheEntry::NotFound => None,
                            PresetCacheEntry::Found(data) => Some((id, data)),
                        }
                    });
                    ui.separator();
                    let widget_id = ui.make_persistent_id("selected-preset");
                    CollapsingState::load_with_default_open(ui.ctx(), widget_id, false)
                        .show_header(ui, |ui| {
                            left_right(
                                ui,
                                pot_unit,
                                ui.available_height(),
                                50.0,
                                // Left side of preset info
                                |ui, _| {
                                    ui.strong("Selected preset:");
                                    if let Some((preset_id, preset_data)) =
                                        current_preset_id_and_data
                                    {
                                        ui.label(preset_data.preset.name());
                                        let _ = pot_db().try_with_db(preset_id.database_id, |db| {
                                            ui.strong("from");
                                            ui.label(db.name());
                                        });
                                        if let Some(product_name) =
                                            &preset_data.preset.common.product_name
                                        {
                                            ui.strong("for");
                                            ui.label(product_name);
                                        }
                                    } else {
                                        ui.label("-");
                                    }
                                },
                                // Right side of preset info
                                |ui, _pot_unit| {
                                    let Some((preset_id, preset_data)) = current_preset_id_and_data
                                    else {
                                        return;
                                    };
                                    // Favorite button
                                    let favorites = integration.pot_favorites();
                                    let toggle = if let Ok(favorites) = favorites.try_read() {
                                        let mut is_favorite = favorites.is_favorite(preset_id);
                                        let icon = if is_favorite { "★" } else { "☆" };
                                        ui.toggle_value(&mut is_favorite, icon).changed()
                                    } else {
                                        false
                                    };
                                    if toggle {
                                        show_info_toast(
                                            "This feature is not available.",
                                            &mut toasts,
                                        );
                                        // pot_unit.toggle_favorite(preset_id, state.main_state.pot_unit.clone());
                                    }
                                    // Preview button
                                    let preview_button = Button::new("🔊");
                                    let preview_button_response = ui.add_enabled(
                                        preset_data.preview_file.is_some(),
                                        preview_button,
                                    );
                                    if preview_button_response
                                        .on_hover_text("Play preset preview")
                                        .on_disabled_hover_text("Preset preview not available")
                                        .clicked()
                                    {
                                        state
                                            .main_state
                                            .command_sender
                                            .send_complaining(ReaperCommand::PlayPreview(
                                                preset_id,
                                            ));
                                    }
                                },
                            );
                        })
                        .body(|ui| {
                            if let Some((_, preset_data)) = current_preset_id_and_data {
                                let metadata = &preset_data.preset.common.metadata;
                                ui.horizontal(|ui| {
                                    ui.strong("Vendor:");
                                    ui.label(optional_string(metadata.vendor.as_deref()));
                                    ui.strong("Author:");
                                    ui.label(optional_string(metadata.author.as_deref()));
                                    ui.strong("Extension:");
                                    ui.label(optional_string(
                                        preset_data.preset.kind.file_extension(),
                                    ));
                                });
                                ui.horizontal(|ui| {
                                    ui.strong("File size:");
                                    let fmt_size = metadata
                                        .file_size_in_bytes
                                        .map(|s| bytesize::ByteSize(s).to_string());
                                    ui.label(optional_string(fmt_size.as_deref()));
                                    ui.strong("Date modified:");
                                    let fmt_date = metadata.modification_date.and_then(|s| {
                                        let utc = s.and_local_timezone(Utc).single()?;
                                        let local: DateTime<Local> = utc.into();
                                        Some(local.format("%Y-%m-%d %H:%M:%S").to_string())
                                    });
                                    ui.label(optional_string(fmt_date.as_deref()));
                                });
                                ui.horizontal(|ui| {
                                    ui.strong("Comment:");
                                    // This is a fix of invalid usage of line breaks in some preset
                                    // commons, e.g. in some u-he presets.
                                    let text =
                                        metadata.comment.as_ref().map(|c| c.replace("\\n", ""));
                                    ui.label(optional_string(text.as_deref()));
                                });
                            }
                        });
                    // Destination info
                    ui.separator();
                    ui.horizontal(|ui| {
                        left_right(
                            ui,
                            pot_unit,
                            ui.available_height(),
                            75.0,
                            // Left side of destination info
                            |ui, pot_unit| {
                                add_destination_info_panel(ui, pot_unit, &frame.destination);
                            },
                            // Right side of destination info
                            |ui, _| {
                                if let Some(fx) = current_fx {
                                    if ui
                                        .small_button("Chain")
                                        .on_hover_text("Shows the FX chain")
                                        .clicked()
                                    {
                                        state.main_state.command_sender.send_complaining(
                                            ReaperCommand::ShowFxChain(fx.clone()),
                                        );
                                    }
                                    if ui
                                        .small_button("FX")
                                        .on_hover_text("Shows the FX")
                                        .clicked()
                                    {
                                        state
                                            .main_state
                                            .command_sender
                                            .send_complaining(ReaperCommand::ShowFx(fx.clone()));
                                    }
                                }
                            },
                        );
                    });
                    // Preset table
                    ui.separator();
                    let input = PresetTableInput {
                        pot_unit,
                        toasts: &mut toasts,
                        last_preset_id: state.main_state.last_preset_id,
                        auto_preview: state.main_state.auto_preview,
                        os_window: state.main_state.os_window,
                        command_sender: &state.main_state.command_sender,
                    };
                    add_preset_table(input, ui, &mut state.main_state.preset_cache);
                });
        });
    // Other stuff
    drain_ui_errors(&mut toasts);
    toasts.show(ctx);
    if state.main_state.paint_continuously {
        // Necessary e.g. in order to not just repaint on clicks or so but also when controller
        // changes pot stuff. But also for other things!
        ctx.request_repaint();
    }
    state.main_state.last_preset_id = pot_unit.preset_id();
    state.main_state.last_filters = *pot_unit.filters();
}

struct ProcessDialogsInput<'a> {
    shared_pot_unit: &'a SharedRuntimePotUnit,
    pot_unit: &'a mut RuntimePotUnit,
    dialog: &'a mut Dialog,
    mouse: &'a EnigoMouse,
    os_window: Window,
    change_dialog: &'a mut Option<Option<Dialog>>,
    pot_worker_dispatcher: &'a mut CustomPotWorkerDispatcher,
    main_thread_dispatcher: &'a mut CustomMainThreadDispatcher,
    frame: &'a ReaperFrame,
}

fn process_dialogs(input: ProcessDialogsInput, ctx: &Context) {
    match input.dialog {
        Dialog::GeneralError { title, msg } => show_dialog(
            ctx,
            title,
            input.change_dialog,
            |ui, _| {
                add_markdown(ui, msg, DIALOG_CONTENT_MAX_HEIGHT);
            },
            |ui, change_dialog| {
                if ui.button("Ok").clicked() {
                    *change_dialog = Some(None);
                };
            },
        ),
        Dialog::AddProjectDatabase { folder, name } => {
            show_dialog(
                ctx,
                "Add project database",
                &mut (input.change_dialog, name, folder),
                |ui, (_, name, folder)| {
                    ui.strong("Caution:");
                    ui.label("Choosing a folder with lots of subdirectories can lead to *very* long refresh times!");
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.strong("Folder:");
                        ui.text_edit_singleline(*folder);
                    });
                    ui.horizontal(|ui| {
                        ui.strong("Name:");
                        ui.text_edit_singleline(*name);
                    });
                },
                |ui, (change_dialog, name, folder)| {
                    if ui.button("Cancel").clicked() {
                        **change_dialog = Some(None);
                    };
                    if ui.button("Add").clicked() {
                        let config = ProjectDbConfig {
                            persistent_id: PersistentDatabaseId::random(),
                            root_dir: Path::new(*folder).to_path_buf(),
                            name: name.clone(),
                        };
                        match ProjectDatabase::open(config) {
                            Ok(db) => {
                                **change_dialog = Some(None);
                                pot_db().add_database(db);
                                input.pot_unit.refresh_pot(input.shared_pot_unit.clone());
                            }
                            Err(e) => {
                                let error_dialog = Dialog::general_error(e.to_string(), "");
                                **change_dialog = Some(Some(error_dialog));
                            }
                        }
                    }
                },
            );
        }
        Dialog::PresetCrawlerIntro => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            input.change_dialog,
            |ui, _| {
                add_markdown(ui, PRESET_CRAWLER_INTRO_TEXT, DIALOG_CONTENT_MAX_HEIGHT);
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
                if ui.button("Continue").clicked() {
                    *change_dialog = Some(Some(Dialog::preset_crawler_basics()));
                }
            },
        ),
        Dialog::PresetCrawlerBasics => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            input.change_dialog,
            |ui, _| {
                add_markdown(ui, PRESET_CRAWLER_BASICS_TEXT, DIALOG_CONTENT_MAX_HEIGHT);
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
                if ui.button("Continue").clicked() {
                    *change_dialog = Some(Some(Dialog::preset_crawler_mouse()));
                }
            },
        ),
        Dialog::PresetCrawlerMouse { creation_time } => match input.mouse.cursor_position() {
            // Capturing current cursor position successful
            Ok(p) => show_dialog(
                ctx,
                PRESET_CRAWLER_TITLE,
                input.change_dialog,
                |ui, change_dialog| {
                    ui.add(Label::new(PRESET_CRAWLER_MOUSE_TEXT).wrap(true));
                    let elapsed = creation_time.elapsed();
                    ui.horizontal(|ui| {
                        ui.strong("Current mouse cursor position:");
                        ui.label(format_mouse_cursor_pos(p));
                    });
                    ui.horizontal(|ui| {
                        ui.strong("Countdown:");
                        let countdown = PRESET_CRAWLER_COUNTDOWN_DURATION.saturating_sub(elapsed);
                        ui.label(format!("{}s", countdown.as_secs()));
                    });
                    if elapsed >= PRESET_CRAWLER_COUNTDOWN_DURATION {
                        // Countdown finished
                        input.os_window.focus_first_child();
                        let next_dialog = if let Some(fx) = &input.frame.focused_fx {
                            if fx.is_open_in_floating_window {
                                Dialog::preset_crawler_ready(fx.fx.clone(), fx.name.clone(), p)
                            } else {
                                Dialog::preset_crawler_failure(
                                    format!("Identified FX \"{}\" but it's not open in a floating window.", fx.name),
                                    "Please use the floating window to point the mouse to the \"Next preset\" button!",
                                )
                            }
                        } else {
                            Dialog::preset_crawler_failure(PRESET_CRAWLER_MOUSE_FAILURE_TEXT, "")
                        };
                        *change_dialog = Some(Some(next_dialog));
                    }
                },
                |ui, change_dialog| {
                    if ui.button("Cancel").clicked() {
                        *change_dialog = Some(None);
                    };
                    if ui.button("Try again").clicked() {
                        *change_dialog = Some(Some(Dialog::preset_crawler_mouse()));
                    };
                },
            ),
            // Capturing current cursor position failed
            Err(e) => {
                *input.change_dialog = Some(Some(Dialog::preset_crawler_failure(
                    "Sorry, capturing the mouse position failed.",
                    e,
                )));
            }
        },
        Dialog::PresetCrawlerFailure {
            short_msg,
            detail_msg,
        } => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            input.change_dialog,
            |ui, _| {
                ui.label(&**short_msg);
                if !detail_msg.is_empty() {
                    ui.add_space(3.0);
                    ui.label(&**detail_msg);
                }
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
                if ui.button("Try again").clicked() {
                    *change_dialog = Some(Some(Dialog::preset_crawler_mouse()));
                };
            },
        ),
        Dialog::PresetCrawlerMouseSaveAs {
            creation_time,
            fx,
            fx_name,
            next_preset_cursor_pos,
            stop_if_destination_exists,
            never_stop_crawling,
        } => match input.mouse.cursor_position() {
            Ok(p) => show_dialog(
                ctx,
                PRESET_CRAWLER_TITLE,
                input.change_dialog,
                |ui, change_dialog| {
                    ui.add(Label::new(PRESET_CRAWLER_MOUSE_SAVE_AS_TEXT).wrap(true));
                    let elapsed = creation_time.elapsed();
                    ui.horizontal(|ui| {
                        ui.strong("Current mouse cursor position:");
                        ui.label(format_mouse_cursor_pos(p));
                    });
                    ui.horizontal(|ui| {
                        ui.strong("Countdown:");
                        let countdown = PRESET_CRAWLER_COUNTDOWN_DURATION.saturating_sub(elapsed);
                        ui.label(format!("{}s", countdown.as_secs()));
                    });
                    if elapsed >= PRESET_CRAWLER_COUNTDOWN_DURATION {
                        // Countdown finished, "Save Preset As" button position captured
                        *change_dialog = Some(Some(Dialog::preset_crawler_mouse_cancel(
                            fx.clone(),
                            fx_name.clone(),
                            *next_preset_cursor_pos,
                            p,
                            *stop_if_destination_exists,
                            *never_stop_crawling,
                        )));
                    }
                },
                |ui, change_dialog| {
                    if ui.button("Cancel").clicked() {
                        *change_dialog = Some(None);
                    };
                    if ui.button("Try again").clicked() {
                        *change_dialog = Some(Some(Dialog::preset_crawler_mouse_save_as(
                            fx.clone(),
                            fx_name.clone(),
                            *next_preset_cursor_pos,
                            *stop_if_destination_exists,
                            *never_stop_crawling,
                        )));
                    };
                },
            ),
            Err(e) => {
                *input.change_dialog = Some(Some(Dialog::preset_crawler_failure(
                    "Sorry, capturing the mouse position failed.",
                    e,
                )));
            }
        },
        Dialog::PresetCrawlerMouseCancel {
            creation_time,
            fx,
            fx_name,
            next_preset_cursor_pos,
            save_as_button_pos,
            stop_if_destination_exists,
            never_stop_crawling,
        } => match input.mouse.cursor_position() {
            Ok(p) => show_dialog(
                ctx,
                PRESET_CRAWLER_TITLE,
                input.change_dialog,
                |ui, change_dialog| {
                    ui.add(Label::new(PRESET_CRAWLER_MOUSE_CANCEL_TEXT).wrap(true));
                    let elapsed = creation_time.elapsed();
                    ui.horizontal(|ui| {
                        ui.strong("Current mouse cursor position:");
                        ui.label(format_mouse_cursor_pos(p));
                    });
                    ui.horizontal(|ui| {
                        ui.strong("Countdown:");
                        let countdown = PRESET_CRAWLER_COUNTDOWN_DURATION.saturating_sub(elapsed);
                        ui.label(format!("{}s", countdown.as_secs()));
                    });
                    if elapsed >= PRESET_CRAWLER_COUNTDOWN_DURATION {
                        // Countdown finished, "Cancel" button position captured
                        let scraping = SaveAsDialogScraping {
                            save_as_button_pos: *save_as_button_pos,
                            cancel_button_pos: p,
                        };
                        *change_dialog = Some(Some(Dialog::preset_crawler_ready_with_scraping(
                            fx.clone(),
                            fx_name.clone(),
                            *next_preset_cursor_pos,
                            *stop_if_destination_exists,
                            *never_stop_crawling,
                            scraping,
                        )));
                    }
                },
                |ui, change_dialog| {
                    if ui.button("Cancel").clicked() {
                        *change_dialog = Some(None);
                    };
                    if ui.button("Try again").clicked() {
                        *change_dialog = Some(Some(Dialog::preset_crawler_mouse_cancel(
                            fx.clone(),
                            fx_name.clone(),
                            *next_preset_cursor_pos,
                            *save_as_button_pos,
                            *stop_if_destination_exists,
                            *never_stop_crawling,
                        )));
                    };
                },
            ),
            Err(e) => {
                *input.change_dialog = Some(Some(Dialog::preset_crawler_failure(
                    "Sorry, capturing the mouse position failed.",
                    e,
                )));
            }
        },
        Dialog::PresetCrawlerReady {
            fx,
            fx_name,
            cursor_pos,
            stop_if_destination_exists,
            never_stop_crawling,
            use_save_as_dialog,
            save_as_dialog,
        } => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            &mut (
                input.change_dialog,
                stop_if_destination_exists,
                never_stop_crawling,
                use_save_as_dialog,
            ),
            |ui, (_, stop_if_destination_exists, never_stop_crawling, use_save_as_dialog)| {
                add_markdown(
                    ui,
                    PRESET_CRAWLER_READY_TEXT,
                    DIALOG_CONTENT_MAX_HEIGHT - 100.0,
                );
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("Plug-in to be crawled:");
                    ui.label(fx_name.as_str());
                });
                ui.horizontal(|ui| {
                    ui.strong("Mouse cursor position to be repeatedly clicked:");
                    ui.label(format_mouse_cursor_pos(*cursor_pos));
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.checkbox(stop_if_destination_exists, "Stop if destination exists")
                        .on_hover_text("By default, Preset Crawler crawls presets even if they already exist in your \"FXChains\" folder.\nIf you tick this checkbox, it will stop crawling as soon as it sees that a preset already exists.");
                    ui.checkbox(never_stop_crawling, "Never stop crawling")
                        .on_hover_text("By default, Preset Crawler stops when it guesses that the last preset has been crawled.\nSometimes, this guess is incorrect. By ticking this checkbox, you can make the crawling infinite.\nYou need to press \"Escape\" as soon as you think that all presets have been crawled.")
                });
                ui.horizontal(|ui| {
                    ui.checkbox(use_save_as_dialog, "Scrape preset names from \"Save preset as...\" dialog")
                        .on_hover_text("Use this if the plug-in doesn't expose its preset names (the crawler would see the same name for every preset).\nThe crawler will then open the plug-in's own \"Save preset as...\" dialog before each step, copy the preset name out of its name field and close the dialog again.\nThis needs the screen positions of two more buttons, which you capture in the next step.");
                    if **use_save_as_dialog {
                        if let Some(s) = save_as_dialog.as_ref() {
                            ui.strong("\"Save as\" button:");
                            ui.label(format_mouse_cursor_pos(s.save_as_button_pos));
                            ui.strong("\"Cancel\" button:");
                            ui.label(format_mouse_cursor_pos(s.cancel_button_pos));
                        } else {
                            ui.label("(button positions not captured yet)");
                        }
                    }
                });
            },
            |ui, (change_dialog, stop_if_destination_exists, never_stop_crawling, use_save_as_dialog)| {
                if ui.button("Cancel").clicked() {
                    **change_dialog = Some(None);
                };
                if ui.button("Try again").clicked() {
                    **change_dialog = Some(Some(Dialog::preset_crawler_mouse()));
                };
                if **use_save_as_dialog && save_as_dialog.is_none() {
                    if ui.button("Capture dialog button positions").clicked() {
                        **change_dialog = Some(Some(Dialog::preset_crawler_mouse_save_as(
                            fx.clone(),
                            fx_name.clone(),
                            *cursor_pos,
                            **stop_if_destination_exists,
                            **never_stop_crawling,
                        )));
                    }
                } else if ui.button("Start crawling").clicked() {
                    let crawling_state = PresetCrawlingState::new();
                    let os_window = input.os_window;
                    let args = CrawlPresetArgs {
                        fx: fx.clone(),
                        next_preset_cursor_pos: *cursor_pos,
                        state: crawling_state.clone(),
                        stop_if_destination_exists: **stop_if_destination_exists,
                        never_stop_crawling: **never_stop_crawling,
                        save_as_dialog: if **use_save_as_dialog {
                            *save_as_dialog
                        } else {
                            None
                        },
                        bring_focus_back_to_crawler: move || {
                            os_window.focus_first_child();
                        },
                    };
                    **change_dialog = Some(Some(Dialog::preset_crawler_crawling(
                        crawling_state.clone(),
                    )));
                    input.main_thread_dispatcher.do_in_background_and_then(
                        async move { crawl_presets(args).await },
                        |context, result| {
                            let next_dialog = match result {
                                Ok(o) => {
                                    let crawled_preset_count = blocking_lock_arc(
                                        &crawling_state,
                                        "crawling finished state",
                                    )
                                    .preset_count();
                                    if crawled_preset_count == 0 {
                                        Dialog::preset_crawler_finished(
                                            crawled_preset_count,
                                            o.reason,
                                        )
                                    } else {
                                        Dialog::preset_crawler_stopped(
                                            crawling_state,
                                            o.reason,
                                            o.chunks_file,
                                            crawled_preset_count,
                                        )
                                    }
                                }
                                Err(e) => Dialog::preset_crawler_failure(
                                    "Failure while crawling",
                                    e.to_string(),
                                ),
                            };
                            context.dialog = Some(next_dialog);
                        },
                    );
                }
            },
        ),
        Dialog::PresetCrawlerCrawling { crawling_state } => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            input.change_dialog,
            |ui, _| {
                ui.heading("Crawling in process...");
                let state = blocking_lock_arc(crawling_state, "run_main_ui crawling state");
                ui.horizontal(|ui| {
                    ui.strong("Presets crawled so far:");
                    ui.label(format_preset_count(&state));
                });
                ui.horizontal(|ui| {
                    ui.strong("Presets skipped so far (because duplicate name):");
                    ui.label(state.duplicate_preset_name_count().to_string());
                });
                ui.horizontal(|ui| {
                    ui.strong("Last crawled preset:");
                    let text = if let Some(p) = state.last_crawled_preset() {
                        p.name()
                    } else {
                        "-"
                    };
                    ui.label(text);
                });
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
            },
        ),
        Dialog::PresetCrawlerStopped {
            crawling_state,
            stop_reason,
            page,
            chunks_file,
            crawled_preset_count,
        } => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            &mut (input.change_dialog, input.pot_worker_dispatcher),
            |ui, _| {
                let cs = blocking_lock_arc(crawling_state, "run_main_ui crawling state 2");
                add_crawl_presets_stopped_dialog_contents(
                    *stop_reason,
                    &cs,
                    page,
                    *crawled_preset_count,
                    ui,
                );
            },
            |ui, (change_dialog, pot_worker_dispatcher)| {
                if ui.button("Discard crawl results").clicked() {
                    **change_dialog = Some(None);
                }
                *chunks_file = if let Some(chunks_file) = chunks_file.take() {
                    if ui.button("Import").clicked() {
                        let cloned_crawling_state = crawling_state.clone();
                        let crawled_preset_count = *crawled_preset_count;
                        let stop_reason = *stop_reason;
                        pot_worker_dispatcher.do_in_background_and_then(
                            async move {
                                import_crawled_presets(cloned_crawling_state, chunks_file).await
                            },
                            move |context, output| {
                                let next_dialog = match output {
                                    Ok(_) => {
                                        let cloned_pot_unit = context.pot_unit.clone();
                                        let mut pot_unit = blocking_lock(
                                            &*context.pot_unit,
                                            "PotUnit from background result handler",
                                        );
                                        pot_unit.refresh_pot(cloned_pot_unit);
                                        Dialog::preset_crawler_finished(
                                            crawled_preset_count,
                                            stop_reason,
                                        )
                                    }
                                    Err(e) => Dialog::preset_crawler_failure(
                                        "Sorry, preset import failed.",
                                        e.to_string(),
                                    ),
                                };
                                context.dialog = Some(next_dialog);
                            },
                        );
                        None
                    } else {
                        Some(chunks_file)
                    }
                } else {
                    None
                };
            },
        ),
        Dialog::PresetCrawlerFinished {
            crawled_preset_count,
            stop_reason,
        } => show_dialog(
            ctx,
            PRESET_CRAWLER_TITLE,
            input.change_dialog,
            |ui, _| {
                if *crawled_preset_count == 0 {
                    let markdown =
                        get_preset_crawler_stopped_markdown(stop_reason, *crawled_preset_count);
                    add_markdown(ui, markdown, DIALOG_CONTENT_MAX_HEIGHT - 30.0);
                    ui.heading("Nothing to import!");
                } else {
                    ui.strong(format!(
                        "Successfully imported {} presets!",
                        *crawled_preset_count
                    ));
                }
            },
            |ui, change_dialog| {
                if ui.button("Close").clicked() {
                    *change_dialog = Some(None);
                }
            },
        ),
        Dialog::PreviewRecorderIntro => show_dialog(
            ctx,
            PREVIEW_RECORDER_TITLE,
            input.change_dialog,
            |ui, _| {
                add_markdown(ui, PREVIEW_RECORDER_INTRO_TEXT, DIALOG_CONTENT_MAX_HEIGHT);
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
                if ui.button("Continue").clicked() {
                    *change_dialog = Some(Some(Dialog::PreviewRecorderBasics));
                }
            },
        ),
        Dialog::PreviewRecorderBasics => show_dialog(
            ctx,
            PREVIEW_RECORDER_TITLE,
            &mut (input.change_dialog, input.pot_worker_dispatcher),
            |ui, _| {
                add_markdown(ui, PREVIEW_RECORDER_BASICS_TEXT, DIALOG_CONTENT_MAX_HEIGHT);
            },
            |ui, (change_dialog, pot_worker_dispatcher)| {
                if ui.button("Cancel").clicked() {
                    **change_dialog = Some(None);
                };
                // Determine output config
                let record_and_export_button = ui.button("Record and export");
                let record_for_pot_browser_button =
                    ui.button("Record for playback within Pot Browser");
                let output_config = if record_for_pot_browser_button.clicked() {
                    PreviewOutputConfig::ForPotBrowserPlayback
                } else if record_and_export_button.clicked() {
                    let parent_dir = os_document_or_reaper_resource_dir(&input.frame.resource_path);
                    let dir_name = Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
                    let base_dir = parent_dir
                        .join("Helgobox/SoundPot/Preview Exports")
                        .join(dir_name);
                    let config = ExportPreviewOutputConfig { base_dir };
                    PreviewOutputConfig::Export(config)
                } else {
                    // No button clicked yet
                    return;
                };
                // One of the continue buttons has been clicked
                let build_input = input.pot_unit.create_build_input();
                let output_config_clone = output_config.clone();
                pot_worker_dispatcher.do_in_background_and_then(
                    async move { prepare_preview_recording(build_input, &output_config_clone) },
                    |context, output| {
                        if matches!(context.dialog, Some(Dialog::PreviewRecorderPreparing)) {
                            context.dialog = Some(Dialog::preview_recorder_ready_to_record(
                                output,
                                output_config,
                            ));
                        }
                    },
                );
                **change_dialog = Some(Some(Dialog::preview_recorder_preparing()));
            },
        ),
        Dialog::PreviewRecorderPreparing => show_dialog(
            ctx,
            PREVIEW_RECORDER_TITLE,
            input.change_dialog,
            |ui, _| {
                ui.label("Aggregating presets to be recorded (this may take a while)...");
                ui.spinner();
            },
            |ui, change_dialog| {
                if ui.button("Cancel").clicked() {
                    *change_dialog = Some(None);
                };
            },
        ),
        Dialog::PreviewRecorderReadyToRecord {
            presets,
            output_config,
        } => show_dialog(
            ctx,
            PREVIEW_RECORDER_TITLE,
            &mut (input.change_dialog, presets, input.main_thread_dispatcher),
            |ui, (_, presets, _)| {
                add_markdown(
                    ui,
                    PREVIEW_RECORDER_READY_TEXT,
                    DIALOG_CONTENT_MAX_HEIGHT / 2.0,
                );
                ui.separator();
                ui.label(format!("Ready to record {} presets:", presets.len()));
                add_item_table(ui, presets, DIALOG_CONTENT_MAX_HEIGHT / 2.0);
            },
            |ui, (change_dialog, presets, main_thread_dispatcher)| {
                if ui.button("Cancel").clicked() {
                    **change_dialog = Some(None);
                };
                if !ui.button("Continue").clicked() {
                    return;
                }
                let preview_rpp =
                    get_preview_rpp_path(input.frame.preview_template_path, output_config);
                let preview_rpp = match preview_rpp {
                    Ok(f) => f,
                    Err(e) => {
                        **change_dialog = Some(Some(Dialog::general_error(
                            PREVIEW_RECORDER_TITLE,
                            e.to_string(),
                        )));
                        return;
                    }
                };
                let presets = mem::take(*presets);
                let state = PreviewRecorderState::new(presets);
                let state = Arc::new(RwLock::new(state));
                **change_dialog = Some(Some(Dialog::preview_recorder_recording(state.clone())));
                let cloned_state = state.clone();
                let shared_pot_unit = input.shared_pot_unit.clone();
                let output_config_clone_1 = output_config.clone();
                let output_config_clone_2 = output_config.clone();
                main_thread_dispatcher.do_in_background_and_then(
                    async move {
                        let args = RecordPreviewsArgs {
                            shared_pot_unit,
                            state,
                            preview_rpp: &preview_rpp,
                            config: output_config_clone_1,
                        };
                        record_previews(args).await.map_err(|e| e.to_string())?;
                        Ok(())
                    },
                    |context, _: Result<(), String>| {
                        let cloned_pot_unit = context.pot_unit.clone();
                        let mut pot_unit = blocking_lock(
                            &*context.pot_unit,
                            "PotUnit from preview background result handler",
                        );
                        pot_unit.refresh_pot(cloned_pot_unit);
                        context.dialog = Some(Dialog::preview_recorder_done(
                            cloned_state,
                            output_config_clone_2,
                        ));
                    },
                );
            },
        ),
        Dialog::PreviewRecorderRecording { state } => show_dialog(
            ctx,
            PREVIEW_RECORDER_TITLE,
            input.change_dialog,
            |ui, _| {
                let state = blocking_read_lock(state, "preview recorder UI");
                ui.heading("Preview recording in process...");
                ui.horizontal(|ui| {
                    ui.strong("Presets still left to be recorded:");
                    ui.label(state.todos.len().to_string());
                });
                ui.horizontal(|ui| {
                    ui.strong("Presets failed:");
                    ui.label(state.failures.len().to_string());
                });
                add_item_table(ui, &state.todos, DIALOG_CONTENT_MAX_HEIGHT - 40.0);
            },
            |_, _| {},
        ),
        Dialog::PreviewRecorderDone {
            state,
            page,
            output_config,
        } => {
            let output_config_clone = output_config.clone();
            show_dialog(
                ctx,
                PREVIEW_RECORDER_TITLE,
                input.change_dialog,
                |ui, _| {
                    let state = blocking_read_lock(state, "preview recorder UI");
                    add_preview_recorder_done_dialog_contents(&state, page, output_config, ui);
                },
                |ui, change_dialog| {
                    if ui.button("Close").clicked() {
                        if let PreviewOutputConfig::Export(c) = output_config_clone {
                            reveal_path(c.base_dir.as_std_path());
                        }
                        *change_dialog = Some(None);
                    };
                },
            )
        }
    }
}

fn add_crawl_presets_stopped_dialog_contents(
    stop_reason: PresetCrawlerStopReason,
    cs: &PresetCrawlingState,
    page: &mut CrawlPresetsStoppedPage,
    crawled_preset_count: u32,
    ui: &mut Ui,
) {
    let preset_count = cs.preset_count();
    let markdown = get_preset_crawler_stopped_markdown(&stop_reason, crawled_preset_count);
    add_markdown(ui, markdown, DIALOG_CONTENT_MAX_HEIGHT / 2.0 - 40.0);
    ui.separator();
    ui.strong(PRESET_CRAWLER_IMPORT_OR_DISCARD);
    ui.horizontal(|ui| {
        ui.strong("Crawled presets ready for import:");
        ui.label(format_preset_count(cs));
    });
    ui.horizontal(|ui| {
        ui.strong("Skipped presets (because duplicate name):");
        ui.label(cs.duplicate_preset_name_count().to_string());
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Show:");
        ui.selectable_value(page, CrawlPresetsStoppedPage::Presets, "Presets");
        ui.selectable_value(page, CrawlPresetsStoppedPage::Duplicates, "Duplicates");
    });
    let table_height = DIALOG_CONTENT_MAX_HEIGHT;
    match *page {
        CrawlPresetsStoppedPage::Presets => {
            let text_height = get_text_height(ui);
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .min_scrolled_height(table_height)
                .max_scroll_height(table_height)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::auto())
                .column(Column::initial(200.0).clip(true))
                .header(text_height, |mut header| {
                    header.col(|ui| {
                        ui.strong("Name");
                    });
                    header.col(|ui| {
                        ui.strong("Destination");
                    });
                })
                .body(|body| {
                    body.rows(text_height, preset_count as usize, |row_index, mut row| {
                        let entry = cs.crawled_presets().get_index(row_index);
                        if let Some((_, preset)) = entry {
                            row.col(|ui| {
                                ui.label(preset.name());
                            });
                            row.col(|ui| {
                                let dest = preset.destination().as_str();
                                ui.label(dest).on_hover_text(dest);
                            });
                        }
                    });
                });
        }
        CrawlPresetsStoppedPage::Duplicates => {
            show_as_list(ui, cs.duplicate_preset_names(), table_height);
        }
    }
}

fn get_preset_crawler_stopped_markdown(
    stop_reason: &PresetCrawlerStopReason,
    crawled_preset_count: u32,
) -> &'static str {
    match stop_reason {
        PresetCrawlerStopReason::Interrupted => PRESET_CRAWLER_INTERRUPTED,
        PresetCrawlerStopReason::DestinationFileExists => PRESET_CRAWLER_DESTINATION_FILE_EXISTS,
        PresetCrawlerStopReason::PresetNameNotChangingAnymore => {
            if crawled_preset_count < 2 {
                PRESET_CRAWLER_INCOMPATIBLE_PLUGIN_TEXT
            } else {
                PRESET_CRAWLER_PRESET_NAME_NOT_CHANGING
            }
        }
        PresetCrawlerStopReason::PresetNameLikeBeginning => PRESET_CRAWLER_PRESET_NAME_LIKE_FIRST,
    }
}

fn add_preview_recorder_done_dialog_contents(
    state: &PreviewRecorderState,
    page: &mut PreviewRecorderDonePage,
    output_config: &PreviewOutputConfig,
    ui: &mut Ui,
) {
    let markdown = match (output_config, state.todos.is_empty()) {
        (PreviewOutputConfig::ForPotBrowserPlayback, false) => {
            PREVIEW_RECORDER_DONE_INTERNAL_INCOMPLETE_TEXT
        }
        (PreviewOutputConfig::ForPotBrowserPlayback, true) => {
            PREVIEW_RECORDER_DONE_INTERNAL_COMPLETE_TEXT
        }
        (PreviewOutputConfig::Export(_), false) => PREVIEW_RECORDER_DONE_EXPORT_INCOMPLETE_TEXT,
        (PreviewOutputConfig::Export(_), true) => PREVIEW_RECORDER_DONE_EXPORT_COMPLETE_TEXT,
    };
    add_markdown(ui, markdown, DIALOG_CONTENT_MAX_HEIGHT / 2.0 - 80.0);
    if !state.failures.is_empty() {
        ui.label("Some previews couldn't be recorded. See the list of failures below.");
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Number of previews not yet recorded:");
        ui.label(state.todos.len().to_string());
    });
    ui.horizontal(|ui| {
        ui.strong("Number of failures:");
        ui.label(state.failures.len().to_string());
    });
    ui.horizontal(|ui| {
        ui.strong("Show:");
        ui.selectable_value(page, PreviewRecorderDonePage::Todos, "Remaining");
        ui.selectable_value(page, PreviewRecorderDonePage::Failures, "Failures");
    });
    match *page {
        PreviewRecorderDonePage::Todos => {
            add_item_table(ui, &state.todos, DIALOG_CONTENT_MAX_HEIGHT / 2.0);
        }
        PreviewRecorderDonePage::Failures => {
            add_item_table(ui, &state.failures, DIALOG_CONTENT_MAX_HEIGHT / 2.0);
        }
    }
}

struct PresetTableInput<'a> {
    pot_unit: &'a mut RuntimePotUnit,
    toasts: &'a mut Toasts,
    last_preset_id: Option<PresetId>,
    auto_preview: bool,
    os_window: Window,
    command_sender: &'a SenderToNormalThread<ReaperCommand>,
}

fn add_preset_table(mut input: PresetTableInput, ui: &mut Ui, preset_cache: &mut PresetCache) {
    let text_height = get_text_height(ui);
    let preset_count = input.pot_unit.preset_count();
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        // Preset name
        .column(Column::auto())
        // Plug-in or product
        .column(Column::initial(200.0).clip(true).at_least(100.0))
        // Extension
        .column(Column::initial(50.0))
        // Context
        .column(Column::remainder())
        .min_scrolled_height(0.0);
    if input.pot_unit.preset_id() != input.last_preset_id {
        let scroll_index = match input.pot_unit.preset_id() {
            None => 0,
            Some(id) => input.pot_unit.find_index_of_preset(id).unwrap_or(0),
        };
        table = table.scroll_to_row(scroll_index as usize, None);
    }
    table
        .header(20.0, |mut header| {
            header.col(|ui| {
                ui.strong("Name");
            });
            header.col(|ui| {
                ui.strong("Plug-in/product");
            });
            header.col(|ui| {
                ui.strong("Extension");
            });
            header.col(|ui| {
                ui.strong("Context");
            });
        })
        .body(|body| {
            body.rows(text_height, preset_count as usize, |row_index, mut row| {
                let preset_id = input
                    .pot_unit
                    .find_preset_id_at_index(row_index as u32)
                    .unwrap();
                let cache_entry = preset_cache.find_preset(preset_id);
                // Name
                row.col(|ui| {
                    // Decide about text to display
                    let text: Cow<str> = match cache_entry {
                        PresetCacheEntry::Requested => "⏳".into(),
                        PresetCacheEntry::NotFound => "<Preset not found>".into(),
                        PresetCacheEntry::Found(d) => shorten_preset_name(d.preset.name()),
                    };
                    let mut button = Button::new(text).small().fill(Color32::TRANSPARENT);
                    if let PresetCacheEntry::Found(data) = cache_entry {
                        if data.preview_file.is_some() {
                            button = button.shortcut_text("🔊");
                        }
                    };
                    // Highlight currently selected preset
                    if Some(preset_id) == input.pot_unit.preset_id() {
                        button = button.fill(ui.style().visuals.selection.bg_fill);
                    }
                    let mut button = ui.add_sized(ui.available_size(), button);
                    // Only if preset found in cache
                    if let PresetCacheEntry::Found(data) = cache_entry {
                        // Hover text
                        button = button.on_hover_text(data.preset.name());
                        // Context menu
                        button = button.context_menu(|ui| {
                            // Open in
                            if let Some(preview_file) = &data.preview_file {
                                if ui
                                    .button("Pre-play")
                                    .on_hover_text("Opens the preview file in RS5k sampler in a \"playable\" way")
                                    .clicked()
                                {
                                    let preset = PotPreset {
                                        common: data.preset.common.clone(),
                                        kind: PotPresetKind::FileBased(FiledBasedPotPresetKind {
                                            path: preview_file.clone(),
                                            file_ext: "ogg".to_string(),
                                        }),
                                    };
                                    load_preset_and_regain_focus(
                                        &preset,
                                        input.os_window,
                                        input.command_sender,
                                        LoadPresetOptions {
                                            window_behavior_override: Some(LoadPresetWindowBehavior::NeverShow),
                                            audio_sample_behavior: LoadAudioSampleBehavior {
                                                // At the moment, previews are always C4.
                                                root_pitch: Some(-60),
                                                obey_note_off: true,
                                            },
                                        },
                                    );
                                    ui.close_menu();
                                }
                            }
                            // Open plug-in
                            let has_associated_products =
                                !data.preset.common.product_ids.is_empty();
                            ui.add_enabled_ui(has_associated_products, |ui| {
                                ui.menu_button("Associated products", |ui| {
                                    create_product_plugin_menu(&mut input, data, ui);
                                });
                            });
                            // Reveal preset in file manager
                            #[cfg(any(
                            all(target_os = "windows", target_arch = "x86_64"),
                            target_os = "macos"
                            ))]
                            {
                                if let pot::PotPresetKind::FileBased(k) = &data.preset.kind {
                                    if ui.button("Show preset in file manager").clicked() {
                                        if k.path.exists() {
                                            reveal_path(&k.path);
                                        } else {
                                            show_error_toast(
                                                "Preset file doesn't exist",
                                                input.toasts,
                                            );
                                        }
                                        ui.close_menu();
                                    }
                                }
                                if let Some(preview_file) = &data.preview_file {
                                    if ui.button("Show preview in file manager").clicked() {
                                        reveal_path(preview_file);
                                        ui.close_menu();
                                    }
                                }
                            }
                        });
                        // What to do when clicked
                        if button.clicked() {
                            if input.auto_preview {
                                input
                                    .command_sender
                                    .send_complaining(ReaperCommand::PlayPreview(preset_id));
                            }
                            input.pot_unit.set_preset_id(Some(preset_id));
                        }
                        // What to do when double-clicked
                        if button.double_clicked() {
                            load_preset_and_regain_focus(
                                &data.preset,
                                input.os_window,
                                input.command_sender,
                                LoadPresetOptions::default(),
                            );
                        }
                    }
                });
                // Make other columns empty if preset info not available yet
                let PresetCacheEntry::Found(data) = cache_entry else {
                    row.col(|_| {});
                    row.col(|_| {});
                    return;
                };
                let preset = &data.preset;
                // Product
                row.col(|ui| {
                    if let Some(n) = preset.common.product_name.as_ref() {
                        ui.label(n).on_hover_text(n);
                    }
                });
                // Extension
                row.col(|ui| {
                    let text = preset.kind.file_extension().unwrap_or("");
                    ui.label(text);
                });
                // Context
                row.col(|ui| {
                    let text = preset.common.context_name.as_deref().unwrap_or("");
                    ui.label(text);
                });
            });
        });
}

trait DisplayItem {
    fn prop_count() -> u32;
    fn prop_label(prop_index: u32) -> &'static str;
    fn prop_value(&self, prop_index: u32) -> Option<Cow<str>>;
}

impl DisplayItem for PreviewRecorderFailure {
    fn prop_count() -> u32 {
        PresetWithId::prop_count() + 1
    }

    fn prop_label(prop_index: u32) -> &'static str {
        match prop_index {
            i if i < PresetWithId::prop_count() => PresetWithId::prop_label(i),
            4 => "Reason",
            _ => "",
        }
    }

    fn prop_value(&self, prop_index: u32) -> Option<Cow<str>> {
        match prop_index {
            i if i < PresetWithId::prop_count() => self.preset.prop_value(i),
            3 => Some(self.reason.as_str().into()),
            _ => None,
        }
    }
}

impl DisplayItem for PresetWithId {
    fn prop_count() -> u32 {
        3
    }

    fn prop_label(prop_index: u32) -> &'static str {
        match prop_index {
            0 => "Name",
            1 => "Plug-in/product",
            2 => "Extension",
            _ => "",
        }
    }

    fn prop_value(&self, prop_index: u32) -> Option<Cow<str>> {
        match prop_index {
            0 => Some(shorten_preset_name(self.preset.name())),
            1 => self.preset.common.product_name.as_ref().map(|s| s.into()),
            2 => self.preset.kind.file_extension().map(|s| s.into()),
            _ => None,
        }
    }
}

fn add_item_table<T: DisplayItem>(ui: &mut Ui, items: &[T], max_height: f32) {
    let text_height = get_text_height(ui);
    let item_count = items.len();
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .min_scrolled_height(0.0)
        .min_scrolled_height(max_height)
        .max_scroll_height(max_height);
    for i in 0..T::prop_count() {
        let col = if i == 0 {
            Column::auto()
        } else {
            Column::remainder()
        };
        table = table.column(col);
    }
    table
        .header(20.0, |mut header| {
            for i in 0..T::prop_count() {
                header.col(|ui| {
                    ui.strong(T::prop_label(i));
                });
            }
        })
        .body(|body| {
            body.rows(text_height, item_count, |row_index, mut row| {
                let item = items.get(item_count - row_index - 1).unwrap();
                for i in 0..T::prop_count() {
                    row.col(|ui| {
                        if let Some(val) = item.prop_value(i) {
                            ui.label(val);
                        }
                    });
                }
            });
        });
}

const DIALOG_CONTENT_MAX_HEIGHT: f32 = 300.0;

fn create_product_plugin_menu(input: &mut PresetTableInput, data: &PotPresetData, ui: &mut Ui) {
    pot_db().with_plugin_db(|db| {
        for product_id in &data.preset.common.product_ids {
            let Some(product) = db.find_product_by_id(product_id) else {
                continue;
            };
            ui.menu_button(&product.name, |ui| {
                let product_plugins = db
                    .plugins()
                    .filter(|p| p.common.core.product_id == *product_id);
                for plugin in product_plugins {
                    if ui.button(plugin.common.to_string()).clicked() {
                        let factory_preset = create_plugin_factory_preset(
                            &plugin.common,
                            data.preset.common.persistent_id.clone(),
                            data.preset.common.name.clone(),
                        );
                        let options = LoadPresetOptions {
                            window_behavior_override: Some(LoadPresetWindowBehavior::AlwaysShow),
                            ..Default::default()
                        };
                        if let Err(e) = input.pot_unit.load_preset(&factory_preset, options) {
                            process_error(&e, input.toasts);
                        }
                        ui.close_menu();
                    }
                }
            });
        }
    });
}

fn add_destination_info_panel(
    ui: &mut Ui,
    pot_unit: &mut RuntimePotUnit,
    destination: &DestinationSnapshot,
) {
    // Track descriptor
    {
        const SPECIAL_TRACK_COUNT: usize = 2;
        ui.strong("Load into");
        let track_count = destination.track_count;
        let old_track_code = match &mut pot_unit.destination_descriptor.track {
            DestinationTrackDescriptor::SelectedTrack => 0usize,
            DestinationTrackDescriptor::MasterTrack => 1usize,
            DestinationTrackDescriptor::Track(i) => {
                // If configured track index too high, set it to
                // "new track at end of project".
                *i = (*i).min(track_count);
                *i as usize + SPECIAL_TRACK_COUNT
            }
        };
        let mut new_track_code = old_track_code;
        egui::ComboBox::from_id_source("tracks").show_index(
            ui,
            &mut new_track_code,
            SPECIAL_TRACK_COUNT + track_count as usize + 1,
            |code| match code {
                0 => "<Selected track>".to_string(),
                1 => "<Master track>".to_string(),
                _ => {
                    if let Some(label) = destination.track_labels.get(code - SPECIAL_TRACK_COUNT) {
                        label.clone()
                    } else {
                        "<New track>".to_string()
                    }
                }
            },
        );
        if new_track_code != old_track_code {
            let track_desc = match new_track_code {
                0 => DestinationTrackDescriptor::SelectedTrack,
                1 => DestinationTrackDescriptor::MasterTrack,
                c => DestinationTrackDescriptor::Track(c as u32 - SPECIAL_TRACK_COUNT as u32),
            };
            pot_unit.destination_descriptor.track = track_desc;
        }
    }
    // Resolved track (if displaying it makes sense)
    // The snapshot was taken at the beginning of the frame, so right after changing the
    // destination in the combo box above, the resolved-track display below is one frame
    // behind. That's fine in an immediate-mode UI.
    if pot_unit.destination_descriptor.track.is_dynamic() {
        ui.label("=");
        let caption = match &destination.resolved_track_label {
            Some(label) => {
                format!("\"{label}\"")
            }
            None => "None (add new)".to_string(),
        };
        let short_caption = shorten(caption.as_str().into(), 14);
        ui.label(short_caption).on_hover_text(caption);
    }
    // FX descriptor
    {
        if destination.resolved_track_label.is_some() {
            ui.label("at");
            let fx_count = destination.fx_names.len() as u32;
            // If configured FX index too high, set it to "new FX at end of chain".
            pot_unit.destination_descriptor.fx_index =
                pot_unit.destination_descriptor.fx_index.min(fx_count);
            let mut fx_code = pot_unit.destination_descriptor.fx_index as usize;
            egui::ComboBox::from_id_source("fxs").show_index(
                ui,
                &mut fx_code,
                fx_count as usize + 1,
                |code| match destination.fx_names.get(code) {
                    None => "<New FX>".to_string(),
                    Some(name) => {
                        format!("{}. {}", code + 1, name)
                    }
                },
            );
            pot_unit.destination_descriptor.fx_index = fx_code as _;
        }
    }
}

fn add_stats_panel(
    pot_unit: &mut RuntimePotUnit,
    background_task_elapsed: Option<Duration>,
    ui: &mut Ui,
) {
    ui.strong("Last query: ");
    let total_duration = background_task_elapsed.unwrap_or(pot_unit.stats.total_query_duration());
    ui.label(format!("{}ms", total_duration.as_millis()));
    if background_task_elapsed.is_none() {
        ui.label("(= ");
        ui.label(pot_unit.stats.refresh_duration.as_millis().to_string())
            .on_hover_text("Refresh databases");
        ui.label(" + ");
        ui.label(pot_unit.stats.filter_query_duration.as_millis().to_string())
            .on_hover_text("Building filters");
        ui.label(" + ");
        ui.label(pot_unit.stats.preset_query_duration.as_millis().to_string())
            .on_hover_text("Building presets");
        ui.label(" + ");
        ui.label(
            pot_unit
                .stats
                .preview_filter_duration
                .as_millis()
                .to_string(),
        )
        .on_hover_text("Checking previews");
        ui.label(" + ");
        ui.label(pot_unit.stats.sort_duration.as_millis().to_string())
            .on_hover_text("Sorting filters and presets");
        ui.label(" + ");
        ui.label(pot_unit.stats.sort_duration.as_millis().to_string())
            .on_hover_text("Indexing presets");
        ui.label(")");
    }
    ui.strong("Wasted runs/time: ");
    ui.label(format!(
        "{}/{}ms",
        pot_unit.wasted_runs,
        pot_unit.wasted_duration.as_millis()
    ));
}

struct RightOptionsDropdownInput<'a> {
    pot_unit: &'a mut RuntimePotUnit,
    shared_pot_unit: &'a SharedRuntimePotUnit,
    show_stats: &'a mut bool,
    auto_preview: &'a mut bool,
    command_sender: &'a SenderToNormalThread<ReaperCommand>,
}

fn add_right_options_dropdown(input: RightOptionsDropdownInput, ui: &mut Ui) {
    ui.menu_button(RichText::new("Options").size(TOOLBAR_HEIGHT), |ui| {
        // FX window behavior
        ui.menu_button("FX window behavior", |ui| {
            for behavior in LoadPresetWindowBehavior::iter() {
                ui.selectable_value(
                    &mut input.pot_unit.default_load_preset_window_behavior,
                    behavior,
                    behavior.as_ref(),
                );
            }
        })
        .response
        .on_hover_text("Under which conditions to show the FX window when loading a preset");
        // Search fields
        ui.menu_button("Search fields", |ui| {
            let search_fields = &mut input.pot_unit.runtime_state.search_options.search_fields;
            for search_field in SearchField::iter() {
                let mut checked = search_fields.contains(search_field);
                ui.checkbox(&mut checked, search_field.as_ref());
                if checked {
                    search_fields.insert(search_field);
                } else {
                    search_fields.remove(search_field);
                }
            }
        })
        .response
        .on_hover_text("Which columns to search");
        // Wildcards
        let old_wildcard_setting = input.pot_unit.runtime_state.search_options.use_wildcards;
        ui.checkbox(
            &mut input.pot_unit.runtime_state.search_options.use_wildcards,
            "Wildcards",
        )
        .on_hover_text(
            "Allows more accurate search by enabling wildcards: Use * to match any \
        string and ? to match any letter!",
        );
        if input.pot_unit.runtime_state.search_options.use_wildcards != old_wildcard_setting {
            input.pot_unit.rebuild_collections(
                input.shared_pot_unit.clone(),
                ChangeHint::SearchExpression,
                Debounce::No,
            );
        }
        // Stats
        ui.checkbox(input.show_stats, "Display stats")
            .on_hover_text("Show query statistics");
        // Preview
        ui.horizontal(|ui| {
            ui.checkbox(input.auto_preview, "Auto-preview")
                .on_hover_text(
                    "Automatically previews a sound when it's selected via mouse or keyboard",
                );
            // Preview volume
            let old_volume = input.pot_unit.preview_volume();
            let mut new_volume_raw = old_volume.get();
            egui::DragValue::new(&mut new_volume_raw)
                .speed(0.01)
                .custom_formatter(|v, _| {
                    // Formatted with plain math instead of REAPER's mk_vol_str: this runs
                    // on the render thread and mk_vol_str is main-thread-only.
                    format_volume_as_db(v)
                })
                .clamp_range(0.0..=1.0)
                .ui(ui)
                .on_hover_text("Change volume of the sound previews");
            let new_volume = ReaperVolumeValue::new_panic(new_volume_raw);
            if new_volume != old_volume {
                input
                    .command_sender
                    .send_complaining(ReaperCommand::SetPreviewVolume(new_volume));
            }
        });
        // Name track after preset
        ui.checkbox(
            &mut input.pot_unit.name_track_after_preset,
            "Name track after preset",
        )
        .on_hover_text(
            "When enabled, Pot Browser will rename the track to reflect the name of \
            the preset.",
        );
    });
}

/// Read-only inputs for the filter panels.
struct FilterInput<'a> {
    /// Globally excluded filter items, from the frame snapshot.
    excludes: &'a PotFilterExcludes,
    /// For mutations of main-thread-only state (the exclude list).
    command_sender: &'a SenderToNormalThread<ReaperCommand>,
}

#[allow(unused_variables)] // reaper_resource_dir is only used on Windows and Linux
fn add_filter_panels(
    shared_unit: &SharedRuntimePotUnit,
    pot_unit: &mut RuntimePotUnit,
    auto_hide_sub_filters: bool,
    ui: &mut Ui,
    last_filters: &Filters,
    dialog: &mut Option<Dialog>,
    filter_input: &FilterInput,
    reaper_resource_dir: &Utf8Path,
) {
    let heading_height = ui.text_style_height(&TextStyle::Heading);
    // Database
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Database").heading().size(heading_height));
        ui.menu_button("➕", |ui| {
            if ui.button("Project database...").clicked() {
                ui.close_menu();
                let folder = {
                    // On macOS, the blocking file dialog works nicely.
                    #[cfg(target_os = "macos")]
                    {
                        rfd::FileDialog::new()
                            .pick_folder()
                            .and_then(|dir| Utf8PathBuf::from_path_buf(dir).ok())
                    }
                    // On Windows, we run into the borrow_mut error because of RefCells combined
                    // with reentrancy in baseview. Tried async dialog as well with main thread
                    // dispatcher but that closes Pot Browser after choosing file. So we fall back to
                    // manual entry of project path.
                    // On some Linux distributions (especially those used in cross 2.5) we don't
                    // have an up-to-date glib but rfd uses glib-sys and this one needs a new glib.
                    #[cfg(any(target_os = "windows", target_os = "linux"))]
                    {
                        Some(os_document_or_reaper_resource_dir(reaper_resource_dir))
                    }
                };
                if let Some(folder) = folder {
                    *dialog = Some(Dialog::add_project_database(folder.to_string()));
                }
            }
        });
    });
    add_filter_view_content(
        shared_unit,
        pot_unit,
        PotFilterKind::Database,
        ui,
        true,
        last_filters.get(PotFilterKind::Database),
        filter_input,
    );
    // Product type
    ui.separator();
    ui.label(RichText::new("Product type").heading().size(heading_height));
    add_filter_view_content(
        shared_unit,
        pot_unit,
        PotFilterKind::ProductKind,
        ui,
        false,
        None,
        filter_input,
    );
    // Add dependent filter views
    ui.separator();
    let show_projects = pot_unit.supports_filter_kind(PotFilterKind::Project);
    let show_banks = pot_unit.supports_filter_kind(PotFilterKind::Bank);
    let show_sub_banks = show_banks
        && pot_unit.supports_filter_kind(PotFilterKind::SubBank)
        && (!auto_hide_sub_filters
            || (pot_unit
                .filters()
                .is_set_to_concrete_value(PotFilterKind::Bank)
                || pot_unit.get_filter(PotFilterKind::SubBank).is_some()));
    let show_categories = pot_unit.supports_filter_kind(PotFilterKind::Category);
    let show_sub_categories = show_categories
        && pot_unit.supports_filter_kind(PotFilterKind::SubCategory)
        && (!auto_hide_sub_filters
            || (pot_unit
                .filters()
                .is_set_to_concrete_value(PotFilterKind::Category)
                || pot_unit.get_filter(PotFilterKind::SubCategory).is_some()));
    let show_modes = pot_unit.supports_filter_kind(PotFilterKind::Mode);
    let mut remaining_kind_count = 6;
    if !show_projects {
        remaining_kind_count -= 1;
    }
    if !show_banks {
        remaining_kind_count -= 1;
    }
    if !show_sub_banks {
        remaining_kind_count -= 1;
    }
    if !show_categories {
        remaining_kind_count -= 1;
    }
    if !show_sub_categories {
        remaining_kind_count -= 1;
    }
    if !show_modes {
        remaining_kind_count -= 1;
    }
    let mut added_one_view_already = false;
    let mut needs_separator = || {
        if added_one_view_already {
            true
        } else {
            added_one_view_already = true;
            false
        }
    };
    if remaining_kind_count > 0 {
        let filter_view_height = ui.available_height() / remaining_kind_count as f32;
        if show_projects {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::Project,
                needs_separator(),
                false,
                last_filters.get(PotFilterKind::Project),
                filter_input,
            );
        }
        if show_banks {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::Bank,
                needs_separator(),
                false,
                last_filters.get(PotFilterKind::Bank),
                filter_input,
            );
        }
        if show_sub_banks {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::SubBank,
                needs_separator(),
                true,
                last_filters.get(PotFilterKind::SubBank),
                filter_input,
            );
        }
        if show_categories {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::Category,
                needs_separator(),
                false,
                last_filters.get(PotFilterKind::Category),
                filter_input,
            );
        }
        if show_sub_categories {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::SubCategory,
                needs_separator(),
                true,
                last_filters.get(PotFilterKind::SubCategory),
                filter_input,
            );
        }
        if show_modes {
            add_filter_view(
                ui,
                filter_view_height,
                shared_unit,
                pot_unit,
                PotFilterKind::Mode,
                needs_separator(),
                false,
                last_filters.get(PotFilterKind::Mode),
                filter_input,
            );
        }
    }
}

fn add_mini_filters(
    shared_unit: &SharedRuntimePotUnit,
    pot_unit: &mut MutexGuard<RuntimePotUnit>,
    ui: &mut Ui,
) {
    add_filter_view_content_as_icons(shared_unit, pot_unit, PotFilterKind::IsUser, ui);
    ui.separator();
    add_filter_view_content_as_icons(shared_unit, pot_unit, PotFilterKind::IsFavorite, ui);
    ui.separator();
    add_filter_view_content_as_icons(shared_unit, pot_unit, PotFilterKind::IsSupported, ui);
    ui.separator();
    add_filter_view_content_as_icons(shared_unit, pot_unit, PotFilterKind::IsAvailable, ui);
}

fn add_help_button(ui: &mut Ui) {
    let help_button = ui.button(RichText::new("❓").size(TOOLBAR_HEIGHT));
    let help_id = ui.make_persistent_id("help");
    if help_button.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(help_id));
    }
    popup_below_widget(ui, help_id, &help_button, |ui| {
        TableBuilder::new(ui)
            .column(Column::auto().at_least(200.0))
            .column(Column::remainder())
            .cell_layout(Layout::left_to_right(Align::Center))
            .body(|mut body| {
                for (interaction, reaction) in HELP.iter() {
                    body.row(30.0, |mut row| {
                        row.col(|ui| {
                            ui.strong(format!("{interaction}:"));
                        });
                        row.col(|ui| {
                            ui.label(*reaction);
                        });
                    });
                }
            });
    });
}

struct LeftOptionsDropdownInput<'a> {
    pot_unit: &'a mut RuntimePotUnit,
    auto_hide_sub_filters: &'a mut bool,
    paint_continuously: &'a mut bool,
    shared_pot_unit: &'a SharedRuntimePotUnit,
}

fn add_left_options_dropdown(input: LeftOptionsDropdownInput, ui: &mut Ui) {
    ui.menu_button(RichText::new("Options").size(TOOLBAR_HEIGHT), |ui| {
        ui.checkbox(input.auto_hide_sub_filters, "Auto-hide sub filters")
            .on_hover_text(
                "Makes sure you are not confronted with dozens of child filters if \
                the corresponding top-level filter is set to <Any>",
            );
        {
            let old = input.pot_unit.show_excluded_filter_items();
            let mut new = input.pot_unit.show_excluded_filter_items();
            ui.checkbox(&mut new, "Show excluded filters")
                .on_hover_text(
                    "Shows all previously excluded filters again (via right click on \
                    filter item), so you can include them again if you want.",
                );
            if new != old {
                input
                    .pot_unit
                    .set_show_excluded_filter_items(new, input.shared_pot_unit.clone());
            }
        }
        ui.checkbox(input.paint_continuously, "Paint continuously (devs only)")
            .on_hover_text("Leave this enabled. This option is only intended for developers.");
    });
}

fn show_current_preset_panel(
    bank_index: &AtomicU32,
    panel: &CurrentPresetPanelSnapshot,
    command_sender: &SenderToNormalThread<ReaperCommand>,
    ui: &mut Ui,
) {
    ui.horizontal(|ui| {
        ui.heading(&panel.preset_name);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if panel.has_params {
                // Bank picker
                let old_bank_index = bank_index.load(Ordering::Relaxed);
                let mut new_bank_index = old_bank_index as usize;
                egui::ComboBox::from_id_source("banks").show_index(
                    ui,
                    &mut new_bank_index,
                    panel.bank_count as usize,
                    |i| {
                        panel
                            .bank_labels
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| format!("Bank {} (doesn't exist)", i + 1))
                    },
                );
                let new_bank_index = new_bank_index as u32;
                if new_bank_index != old_bank_index {
                    bank_index.store(new_bank_index, Ordering::Relaxed);
                }
                // ui.strong("Parameter bank:");
            }
        })
    });
    // Actual macro param display
    if panel.has_params {
        show_macro_params(ui, panel, command_sender);
        // Scroll handler. This must come at the end, otherwise ui_contains_pointer
        // works with a zero-sized UI!
        if ui.ui_contains_pointer() {
            let vertical_scroll = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    Event::Scroll(s) if s.y != 0.0 => Some(s.y),
                    _ => None,
                })
            });
            if let Some(s) = vertical_scroll {
                let amount = -s.signum() as i32;
                let current = bank_index.load(Ordering::Relaxed);
                bank_index.store(current.saturating_add_signed(amount), Ordering::Relaxed);
            }
        }
    }
}

struct KeyInput<'a> {
    auto_preview: bool,
    os_window: Window,
    pot_unit: SharedRuntimePotUnit,
    command_sender: &'a SenderToNormalThread<ReaperCommand>,
}

fn execute_key_action(
    input: KeyInput,
    pot_unit: &mut MutexGuard<RuntimePotUnit>,
    toasts: &mut Toasts,
    key_action: KeyAction,
) {
    let _ = toasts;
    match key_action {
        KeyAction::NavigateWithinPresets(amount) => {
            if let Some(next_preset_index) = pot_unit.find_next_preset_index(amount) {
                if let Some(next_preset_id) = pot_unit.find_preset_id_at_index(next_preset_index) {
                    pot_unit.set_preset_id(Some(next_preset_id));
                    if input.auto_preview {
                        input
                            .command_sender
                            .send_complaining(ReaperCommand::PlayPreview(next_preset_id));
                    }
                }
            }
        }
        KeyAction::LoadPreset => {
            if let Some((_, preset)) = pot_unit.preset_and_id() {
                load_preset_and_regain_focus(
                    &preset,
                    input.os_window,
                    input.command_sender,
                    LoadPresetOptions::default(),
                );
            }
        }
        KeyAction::ClearLastSearchExpressionChar => {
            pot_unit.runtime_state.search_expression.pop();
            pot_unit.rebuild_collections(
                input.pot_unit,
                ChangeHint::SearchExpression,
                Debounce::No,
            );
        }
        KeyAction::ClearSearchExpression => {
            pot_unit.runtime_state.search_expression.clear();
            pot_unit.rebuild_collections(
                input.pot_unit,
                ChangeHint::SearchExpression,
                Debounce::No,
            );
        }
        KeyAction::ExtendSearchExpression(text) => {
            pot_unit.runtime_state.search_expression.push_str(&text);
            pot_unit.rebuild_collections(
                input.pot_unit,
                ChangeHint::SearchExpression,
                Debounce::Yes,
            );
        }
    }
}

fn determine_key_action(input: &mut InputState, dialog: &mut Option<Dialog>) -> Option<KeyAction> {
    let mut action = None;
    input.events.retain_mut(|event| match event {
        Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => {
            if *pressed {
                match key {
                    Key::ArrowUp => action = Some(KeyAction::NavigateWithinPresets(-1)),
                    Key::ArrowDown => action = Some(KeyAction::NavigateWithinPresets(1)),
                    Key::Enter => action = Some(KeyAction::LoadPreset),
                    Key::Backspace if modifiers.command => {
                        action = Some(KeyAction::ClearSearchExpression)
                    }
                    Key::Backspace if !modifiers.command => {
                        action = Some(KeyAction::ClearLastSearchExpressionChar)
                    }
                    _ => {}
                }
            }
            false
        }
        Event::Text(text) => {
            if dialog.is_some() {
                true
            } else {
                action = Some(KeyAction::ExtendSearchExpression(mem::take(text)));
                false
            }
        }
        _ => true,
    });
    action
}

fn show_macro_params(
    ui: &mut Ui,
    panel: &CurrentPresetPanelSnapshot,
    command_sender: &SenderToNormalThread<ReaperCommand>,
) {
    // Added this UI just to not get duplicate table IDs
    ui.vertical(|ui| {
        if let Some(bank) = &panel.current_bank {
            let text_height = get_text_height(ui);
            let table = TableBuilder::new(ui)
                .striped(false)
                .resizable(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .columns(Column::remainder(), bank.slots.len())
                .vscroll(false);
            table
                .header(20.0, |mut header| {
                    for slot in &bank.slots {
                        header.col(|ui| {
                            let Some(param) = slot else {
                                // Empty slot yields empty column
                                return;
                            };
                            ui.vertical(|ui| {
                                ui.strong(&param.section);
                                let resp = ui.label(&param.macro_name);
                                resp.on_hover_ui(|ui| {
                                    ui.label(&param.hover_text);
                                });
                            });
                        });
                    }
                })
                .body(|mut body| {
                    body.row(text_height, |mut row| {
                        for slot in &bank.slots {
                            row.col(|ui| {
                                let Some(param) = slot else {
                                    // Empty slot yields empty column
                                    return;
                                };
                                if let Some(fx_param) = param.fx_param.as_ref() {
                                    let mut new_param_value_raw = param.value;
                                    DragValue::new(&mut new_param_value_raw)
                                        .speed(0.01)
                                        .custom_formatter(|_, _| {
                                            // The plug-in formats values, which we can't ask
                                            // for during rendering. We show the formatted
                                            // value from the frame snapshot; during a drag,
                                            // it catches up one frame later (after the write
                                            // command was executed and the next snapshot
                                            // captured).
                                            param.formatted_value.clone()
                                        })
                                        .clamp_range(0.0..=1.0)
                                        .ui(ui);
                                    if new_param_value_raw != param.value {
                                        command_sender.send_complaining(
                                            ReaperCommand::SetFxParameter {
                                                param: fx_param.clone(),
                                                value: new_param_value_raw,
                                            },
                                        );
                                    }
                                }
                            });
                        }
                    });
                });
        } else {
            ui.vertical_centered_justified(|ui| {
                ui.heading("Selected parameter bank doesn't exist");
            });
        };
    });
}

impl MainState {
    pub fn new(pot_unit: SharedRuntimePotUnit, os_window: Window) -> Self {
        let (command_sender, command_receiver) =
            SenderToNormalThread::new_unbounded_channel("pot browser reaper commands");
        let (feedback_sender, feedback_receiver) =
            SenderToNormalThread::new_unbounded_channel("pot browser command feedback");
        Self {
            pot_unit,
            auto_preview: true,
            auto_hide_sub_filters: true,
            show_stats: false,
            paint_continuously: true,
            os_window,
            last_preset_id: None,
            last_filters: Default::default(),
            bank_index: Arc::new(AtomicU32::new(0)),
            preset_cache: PresetCache::new(),
            dialog: Default::default(),
            mouse: EnigoMouse::new(),
            has_shown_legacy_vst3_scan_warning: false,
            command_sender,
            command_receiver,
            feedback_sender,
            feedback_receiver,
        }
    }
}

/// Cheap-to-clone bundle of handles that the host needs in order to capture REAPER frames
/// and execute queued REAPER commands on the main thread — independently of where the UI
/// state itself lives. (On X11, the UI state lives on baseview's render thread, so the
/// host can't reach into it; these handles are all it needs.)
#[derive(Clone)]
pub struct HostBridge {
    pot_unit: SharedRuntimePotUnit,
    bank_index: Arc<AtomicU32>,
    command_receiver: Receiver<ReaperCommand>,
    feedback_sender: SenderToNormalThread<UiFeedback>,
}

impl HostBridge {
    /// Captures a snapshot of everything the render pass reads from REAPER.
    ///
    /// Must be called on REAPER's main thread.
    pub fn capture_frame<I: PotBrowserIntegration>(&self, integration: &I) -> ReaperFrame {
        ReaperFrame::capture(
            integration,
            &self.pot_unit,
            self.bank_index.load(Ordering::Relaxed),
        )
    }

    /// Executes all REAPER-mutating commands that the render code queued up.
    ///
    /// Must be called on REAPER's main thread. On platforms where the render loop runs on
    /// the main thread (Windows, macOS), call it right after [`run_ui`]. On other
    /// platforms, call it from a main-thread timer.
    pub fn process_commands(&self) {
        while let Ok(command) = self.command_receiver.try_recv() {
            crate::reaper_commands::execute_reaper_command(
                command,
                &self.pot_unit,
                &self.feedback_sender,
            );
        }
    }
}

impl State {
    /// Returns the handles the host needs for main-thread frame capture and command
    /// execution. Call this before handing the state over to the window.
    pub fn host_bridge(&self) -> HostBridge {
        let main_state = &self.main_state.main_state;
        HostBridge {
            pot_unit: main_state.pot_unit.clone(),
            bank_index: main_state.bank_index.clone(),
            command_receiver: main_state.command_receiver.clone(),
            feedback_sender: main_state.feedback_sender.clone(),
        }
    }
}

#[derive(Debug)]
struct PresetCache {
    lru_cache: LruCache<PresetId, PresetCacheEntry>,
    sender: SenderToNormalThread<PresetCacheMessage>,
    receiver: Receiver<PresetCacheMessage>,
    pot_db_revision: u8,
    reaper_resource_dir: Utf8PathBuf,
}

impl PresetCache {
    pub fn new() -> Self {
        let (sender, receiver) =
            SenderToNormalThread::new_unbounded_channel("pot browser preset cache");
        Self {
            lru_cache: LruCache::new(NonZeroUsize::new(500).unwrap()),
            sender,
            receiver,
            pot_db_revision: 0,
            reaper_resource_dir: Default::default(),
        }
    }

    /// Informs the cache about REAPER's resource directory (which it needs for finding
    /// preview files). It comes from the frame snapshot, so the cache doesn't have to talk
    /// to REAPER itself.
    pub fn set_reaper_resource_dir(&mut self, dir: &Utf8Path) {
        if self.reaper_resource_dir != dir {
            self.reaper_resource_dir = dir.to_path_buf();
        }
    }

    /// This can be called very often in order to keep the cache informed about the revision
    /// of the current data that it's supposed to cache. This call only does something
    /// substantial when the revision changes, i.e. it invalidates the cache.
    pub fn set_pot_db_revision(&mut self, revision: u8) {
        if self.pot_db_revision != revision {
            self.pot_db_revision = revision;
            // Change of revision! We need to invalidate all results.
            self.lru_cache.clear();
        }
    }

    pub fn find_preset(&mut self, preset_id: PresetId) -> &PresetCacheEntry {
        self.lru_cache.get_or_insert(preset_id, || {
            let reaper_resource_dir = self.reaper_resource_dir.clone();
            let sender = self.sender.clone();
            let pot_db_revision = self.pot_db_revision;
            spawn_in_pot_worker(async move {
                let pot_db = pot_db();
                let preset = pot_db.try_find_preset_by_id(preset_id)?;
                let preset_data = preset.map(|p| {
                    let preview_file =
                        find_preview_file(&p, &reaper_resource_dir).map(|p| p.into_owned());
                    PotPresetData {
                        preset: p,
                        preview_file,
                    }
                });
                let message = PresetCacheMessage {
                    pot_db_revision,
                    preset_id,
                    preset_data,
                };
                sender.send_complaining(message);
                Ok(())
            });
            PresetCacheEntry::Requested
        })
    }

    pub fn process_message(&mut self, message: PresetCacheMessage) {
        if message.pot_db_revision != self.pot_db_revision {
            // This worker result is based on a previous revision of the pot unit.
            // Not valid anymore.
            return;
        }
        let data = match message.preset_data {
            None => PresetCacheEntry::NotFound,
            Some(data) => PresetCacheEntry::Found(Box::new(data)),
        };
        self.lru_cache.put(message.preset_id, data);
    }
}

#[allow(clippy::too_many_arguments)]
fn add_filter_view(
    ui: &mut Ui,
    max_height: f32,
    shared_pot_unit: &SharedRuntimePotUnit,
    pot_unit: &mut RuntimePotUnit,
    kind: PotFilterKind,
    add_separator: bool,
    indent: bool,
    last_filter: OptFilter,
    filter_input: &FilterInput,
) {
    let separator_height = if add_separator {
        if indent {
            let vertical_spacing = 6.0;
            ui.add_space(vertical_spacing);
            vertical_spacing
        } else {
            ui.separator().rect.height()
        }
    } else {
        0.0
    };
    let mut render = |ui: &mut Ui| {
        // let mut panel = TopBottomPanel::top(kind)
        //     .resizable(false)
        //     .frame(Frame::none());
        let h1_style_height = ui.text_style_height(&TextStyle::Heading);
        let heading_style_height = if indent {
            h1_style_height * 0.9
        } else {
            h1_style_height
        };
        let heading_height = ui
            .label(
                RichText::new(kind.to_string())
                    .text_style(TextStyle::Heading)
                    .size(heading_style_height),
            )
            .rect
            .height();
        // panel = panel.min_height(h).max_height(h);
        // panel.show_inside(ui, |ui| {
        ScrollArea::vertical()
            .id_source(kind)
            .max_height(max_height - heading_height - separator_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                add_filter_view_content(
                    shared_pot_unit,
                    pot_unit,
                    kind,
                    ui,
                    true,
                    last_filter,
                    filter_input,
                );
            });
        // });
    };
    if indent {
        ui.horizontal_top(|ui| {
            ui.add_space(15.0);
            // ui.separator();
            ui.vertical(|ui| {
                render(ui);
            });
        });
    } else {
        render(ui);
    }
}

fn add_filter_view_content(
    shared_pot_unit: &SharedRuntimePotUnit,
    pot_unit: &mut RuntimePotUnit,
    kind: PotFilterKind,
    ui: &mut Ui,
    wrapped: bool,
    last_filter: OptFilter,
    filter_input: &FilterInput,
) {
    enum UiAction {
        InOrExcludeFilter(PotFilterKind, FilterItemId, bool),
    }
    let mut action = None;
    let old_filter_item_id = pot_unit.get_filter(kind);
    let mut new_filter_item_id = old_filter_item_id;
    let render = |ui: &mut Ui| {
        {
            let exclude_list = filter_input.excludes;
            ui.selectable_value(&mut new_filter_item_id, None, "<Any>");
            for filter_item in pot_unit.filter_item_collections.get(kind) {
                let mut text = RichText::new(filter_item.effective_leaf_name());
                if exclude_list.contains(kind, filter_item.id) {
                    text = text.weak();
                };
                let mut resp =
                    ui.selectable_value(&mut new_filter_item_id, Some(filter_item.id), text);
                // Scroll to current if wrapped and changed from outside
                if wrapped {
                    let is_currently_selected_item = || new_filter_item_id == Some(filter_item.id);
                    let changed_from_outside = || {
                        old_filter_item_id == new_filter_item_id
                            && Some(filter_item.id) != last_filter
                    };
                    if is_currently_selected_item() && changed_from_outside() {
                        resp.scroll_to_me(None);
                    }
                }
                // Hover text
                if let Some(more_info) = filter_item.more_info.as_ref() {
                    resp = resp.on_hover_text(more_info);
                } else if let Some(parent_kind) = kind.parent() {
                    if let Some(parent_name) = filter_item.parent_name.as_ref() {
                        if !parent_name.is_empty() {
                            resp = resp.on_hover_ui(|ui| {
                                let tooltip = match &filter_item.name {
                                    None => {
                                        format!(
                                            "{parent_name} (directly associated with {parent_kind})"
                                        )
                                    }
                                    Some(n) => format!("{parent_name} / {n}"),
                                };
                                ui.label(tooltip);
                            });
                        }
                    }
                }
                // Context menu
                if kind.allows_excludes() {
                    resp.context_menu(|ui| {
                        let is_excluded = exclude_list.contains(kind, filter_item.id);
                        let (text, include) = if is_excluded {
                            ("Include again (globally)", true)
                        } else {
                            ("Exclude (globally)", false)
                        };
                        if ui.button(text).clicked() {
                            action =
                                Some(UiAction::InOrExcludeFilter(kind, filter_item.id, include));
                            ui.close_menu();
                        }
                    });
                }
            }
        }
    };
    if wrapped {
        ui.horizontal_wrapped(render);
    } else {
        ui.horizontal(render);
    }
    // Execute actions
    if new_filter_item_id != old_filter_item_id {
        pot_unit.set_filter(
            kind,
            new_filter_item_id,
            shared_pot_unit.clone(),
            Debounce::No,
        );
    }
    if let Some(act) = action {
        match act {
            UiAction::InOrExcludeFilter(kind, id, include) => {
                // Mutates main-thread-only ReaLearn state, so it must go through the
                // command queue.
                filter_input
                    .command_sender
                    .send_complaining(ReaperCommand::IncludeFilterItem { kind, id, include });
            }
        }
    }
}

/// Returns true if at least one filter was displayed.
fn add_filter_view_content_as_icons(
    shared_pot_unit: &SharedRuntimePotUnit,
    pot_unit: &mut RuntimePotUnit,
    kind: PotFilterKind,
    ui: &mut Ui,
) {
    let old_filter_item_id = pot_unit.get_filter(kind);
    let mut new_filter_item_id = old_filter_item_id;
    // Reverse because we use right-to-left order
    for filter_item in pot_unit.filter_item_collections.get(kind).iter().rev() {
        let currently_selected = old_filter_item_id == Some(filter_item.id);
        let mut text = RichText::new(filter_item.icon.unwrap_or('-')).size(TOOLBAR_HEIGHT);
        if !currently_selected {
            text = text.weak();
        }
        let resp = ui.button(text).on_hover_ui(|ui| {
            let name = filter_item.effective_leaf_name();
            let tooltip: Cow<str> = if let Some(more) = &filter_item.more_info {
                format!("{name}: {more}").into()
            } else {
                name
            };
            ui.label(tooltip);
        });
        if resp.clicked() {
            new_filter_item_id = if currently_selected {
                None
            } else {
                Some(filter_item.id)
            }
        };
    }
    if new_filter_item_id != old_filter_item_id {
        pot_unit.set_filter(
            kind,
            new_filter_item_id,
            shared_pot_unit.clone(),
            Debounce::No,
        );
    }
}

/// Queues loading of the given preset (and subsequent re-focusing of the pot browser
/// window) for execution on the main thread. Load errors arrive as [`UiFeedback`] one
/// frame later.
fn load_preset_and_regain_focus(
    preset: &PotPreset,
    os_window: Window,
    command_sender: &SenderToNormalThread<ReaperCommand>,
    options: LoadPresetOptions,
) {
    command_sender.send_complaining(ReaperCommand::LoadPreset {
        preset: Box::new(preset.clone()),
        options,
        os_window,
    });
}

fn process_error(error: &dyn Error, toasts: &mut Toasts) {
    show_error_toast(error.to_string(), toasts);
}

fn show_error_toast(text: impl Into<WidgetText>, toasts: &mut Toasts) {
    toasts.error(text, Duration::from_secs(3));
}

fn show_info_toast(text: &str, toasts: &mut Toasts) {
    toasts.info(text, Duration::from_secs(3));
}

const TOOLBAR_HEIGHT: f32 = 15.0;
const TOOLBAR_HEIGHT_WITH_MARGIN: f32 = TOOLBAR_HEIGHT + 5.0;

const HELP: &[(&str, &str)] = &[
    ("Click preset", "Select it (and preview it if enabled)"),
    ("Double-click preset", "Load it"),
    ("Up/down arrays", "Navigate in preset list"),
    ("Enter", "Load currently selected preset"),
    ("Type letters", "Enter search text"),
    ("(Ctrl+Alt)/(Cmd) + Backspace", "Clear search expression"),
];

fn shorten(text: Cow<str>, max_len: usize) -> Cow<str> {
    if text.len() > max_len {
        let mut s = text.into_owned();
        truncate_in_place(&mut s, max_len);
        s.push('…');
        s.into()
    } else {
        text
    }
}

fn truncate_in_place(s: &mut String, max_chars: usize) {
    let bytes = truncate(s, max_chars).len();
    s.truncate(bytes);
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}

fn left_right<A>(
    ui: &mut Ui,
    arg: &mut A,
    height: f32,
    right_width: f32,
    left: impl FnOnce(&mut Ui, &mut A),
    right: impl FnOnce(&mut Ui, &mut A),
) {
    // At first, we need to add a vertical strip in order to not take infinity height.
    StripBuilder::new(ui)
        .size(Size::exact(height))
        .clip(true)
        .vertical(|mut strip| {
            strip.strip(|strip| {
                strip
                    .size(Size::remainder())
                    .size(Size::exact(right_width))
                    .horizontal(|mut strip| {
                        // Left strip
                        strip.cell(|ui| {
                            ui.horizontal(|ui| {
                                left(ui, arg);
                            });
                        });
                        // Right strip
                        strip.cell(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                right(ui, arg);
                            });
                        });
                    });
            });
        });
}

enum KeyAction {
    NavigateWithinPresets(i32),
    LoadPreset,
    ClearSearchExpression,
    ClearLastSearchExpressionChar,
    ExtendSearchExpression(String),
}

fn show_dialog<V>(
    ctx: &Context,
    title: &str,
    value: &mut V,
    content: impl FnOnce(&mut Ui, &mut V),
    buttons: impl FnOnce(&mut Ui, &mut V),
) {
    egui::Window::new(title)
        .resizable(false)
        .collapsible(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.screen_rect().center())
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_min_width(500.0);
                // Content
                content(ui, value);
                // Space between content and buttons
                ui.add_space(10.0);
                // Buttons
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Right margin
                        ui.add_space(20.0);
                        // Buttons
                        buttons(ui, value);
                    });
                });
                // Bottom margin
                ui.add_space(10.0);
            })
        });
}

fn format_mouse_cursor_pos(pos: MouseCursorPosition) -> String {
    format!("{}, {}", pos.x, pos.y)
}

fn get_text_height(ui: &Ui) -> f32 {
    TextStyle::Body.resolve(ui.style()).size
}

fn show_as_list(ui: &mut Ui, entries: &[impl AsRef<str>], height: f32) {
    let text_height = get_text_height(ui);
    TableBuilder::new(ui)
        .striped(true)
        .resizable(false)
        .min_scrolled_height(height)
        .max_scroll_height(height)
        .column(Column::remainder())
        .body(|body| {
            body.rows(text_height, entries.len(), |i, mut row| {
                row.col(|ui| {
                    ui.label(entries[i].as_ref());
                });
            });
        });
}

fn shorten_preset_name(name: &str) -> Cow<str> {
    const MAX_PRESET_NAME_LEN: usize = 40;
    shorten(name.into(), MAX_PRESET_NAME_LEN)
}

fn add_markdown(ui: &mut Ui, markdown: &str, max_height: f32) {
    if markdown.trim().is_empty() {
        return;
    }
    use pulldown_cmark::*;
    let parser = Parser::new(markdown);
    let mut strong = false;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut list_item_index: Option<u64> = None;
    let mut spans: Vec<RichText> = vec![];
    let mut href: Option<String> = None;
    let insert_paragraph = |ui: &mut Ui, spans: &mut Vec<RichText>| {
        ui.horizontal_wrapped(|ui| {
            for span in spans.drain(..) {
                ui.label(span);
            }
        });
        ui.add_space(3.0);
    };
    ScrollArea::vertical()
        .id_source("markdown")
        .max_height(max_height)
        .show(ui, |ui| {
            for event in parser {
                match event {
                    Event::Start(Tag::Strong) => {
                        strong = true;
                    }
                    Event::End(Tag::Strong) => {
                        strong = false;
                    }
                    Event::Start(Tag::Link(_, url, _)) => {
                        href = Some(url.to_string());
                    }
                    Event::End(Tag::Link(_, _, _)) => {
                        href = None;
                    }
                    Event::Start(Tag::Heading(level, ..)) => {
                        heading_level = Some(level);
                    }
                    Event::End(Tag::Heading(..)) => {
                        heading_level = None;
                        ui.add_space(3.0);
                        insert_paragraph(ui, &mut spans);
                    }
                    Event::Start(Tag::List(start_index)) => {
                        list_item_index = start_index;
                    }
                    Event::Start(Tag::Item) => {
                        let decoration = if let Some(i) = list_item_index {
                            format!("{i}.")
                        } else {
                            "•".to_string()
                        };
                        spans.push(RichText::new(decoration).strong());
                    }
                    Event::End(Tag::Item) => {
                        if let Some(i) = &mut list_item_index {
                            *i += 1;
                        }
                        insert_paragraph(ui, &mut spans);
                    }
                    Event::End(Tag::Paragraph) => {
                        insert_paragraph(ui, &mut spans);
                    }
                    Event::Text(text) => {
                        if let Some(href) = href.take() {
                            if ui.button(text.as_ref()).clicked() {
                                open_link(&href);
                            }
                        } else {
                            use HeadingLevel as L;
                            let (size, always_strong) = match heading_level {
                                Some(L::H1) => (20.0, true),
                                Some(L::H2) => (16.0, true),
                                _ => (14.0, false),
                            };
                            let mut span = RichText::new(text.as_ref()).size(size);
                            if strong || always_strong {
                                span = span.strong();
                            }
                            spans.push(span);
                        }
                    }
                    _ => {}
                }
            }
        });
}

fn format_preset_count(state: &PresetCrawlingState) -> String {
    let fmt_size = bytesize::ByteSize(state.bytes_crawled() as u64);
    format!("{} ({fmt_size})", state.preset_count())
}

const PRESET_CRAWLER_INTRO_TEXT: &str = r#"
## Welcome to Pot Preset Crawler!

You might have plug-in presets that don't show up in the browser because they are only accessible from within the plug-in itself. Wouldn't it be nice if you could browse them, too?

One thing you could do is to manually load each preset from within the plug-in and save it, for example as a REAPER FX chain or REAPER FX preset. Then it will show up. But imagine doing that for hundreds of presets! What a tedious work! That's where Preset Crawler comes in. It tries to automate this process as far as possible.

## Preconditions

- The plug-in must have a button to navigate to the next preset. It must be accessible with just one click, not be buried in menus.
- The plug-in must correctly expose the name of the currently loaded internal preset. In my experience, this works with most VST2 plug-ins but unfortunately not with most VST3 plug-ins. Just check if REAPER's FX dropdown always shows the same preset name as the plug-in user interface.

## How it works 

**Step 1:** You show preset crawler where's the "Next preset" button.

**Step 2:** Preset crawler repeatedly clicks that button for you and memorizes the presets.

**Step 3:** At the end, it shows you the list of memorized presets. If you then click "Import", it will save a REAPER FX chain for each of them.

**Step 4:** That's it! Your newly crawled presets will show up in Pot Browser and even in REAPER's own FX browser. 

## Want to try it?

Then press "Continue" and follow the instructions!
"#;

const PREVIEW_RECORDER_INTRO_TEXT: &str = r#"
## Welcome to Pot Preview Recorder!

Wouldn't it be nice if you could get a quick impression of how your instrument preset sounds without actually loading it?

Preview Recorder has a feature for that: Previews. You know that a preset has a preview when it has the small 🔊 symbol right next to it.

Some preset databases such as Komplete ship with pre-recorded previews and Pot Browser supports them! But even if you own Komplete, you will find that many previews are missing, especially those for your own user presets.

Preview Recorder to the rescue! It can batch-record previews of your instrument presets, very conveniently, no matter where they come from.

## Preconditions

- The preset must be loadable by Pot Browser (Preview Recorder will simply skip presets that can't be loaded automatically).  

## How it works

**Step 1:** You use Pot Browser's filter and search features in order to define the set of presets to be recorded.
 
**Step 2:** Preview Recorder filters out unsuitable presets and creates an optimized recording plan, with as few plug-in reloads as possible.

**Step 3:** You review the plan and if you like it, you start the recording process.

**Step 4:** Preview Recorder opens a new project tab and renders the previews.

**Step 5:** All newly recorded previews are automatically available!

As an alternative, you can export the recorded previews to a directory of your choice.

## Want to try it?

Then press "Continue" and follow the instructions!
"#;

const PREVIEW_RECORDER_BASICS_TEXT: &str = r#"
## Okay, let's do this!

Have you used Pot Browser's filter and search features already to narrow down the set of presets? This step is optional but if you want to do it, now's the time. Simply press "Cancel" and reopen Preview Recorder when you are done filtering/searching.

Creating previews for hundreds of presets can take long. But no worries, you can stop anytime without losing your already recorded previews. Next time, Preview Recorder will automatically continue where you left off.

As soon as you press one of the record buttons, Preview Browser will narrow down your set of presets further and only include those that ...

- ... don't have previews yet
- ... are instrument presets
- ... are available
- ... and are automatically loadable.

## Ready?

Then press one of the record buttons, depending on what you want to do!
"#;

const PRESET_CRAWLER_BASICS_TEXT: &str = r#"
## Okay, let's do this!

Do the following things while leaving the Pot Browser window open. You can move it to the side if it's getting in the way.

1. At first, it's a good idea to save and close all open projects. Just in case.
2. Then open the plug-in whose presets you want to crawl - simply by adding it as FX to a track.
3. If you don't want to start crawling from the first preset, load the preset from which you want to start.
4. Make sure the plug-in window is visible, in particular its "Next preset" button. This is the button which makes the plug-in navigate to the next preset. 

After pressing "Continue", you will have to place the mouse cursor on top of the "Next preset" button of your plug-in.

## Ready?

Then press "Continue"! 
"#;

const PREVIEW_RECORDER_DONE_INTERNAL_COMPLETE_TEXT: &str = r#"
## Preview recording done!

Preview Recorder has recorded all previews. They will be automatically available in Preset Browser.

You may close the preview recording project tab (no need to save).
"#;

const PREVIEW_RECORDER_DONE_EXPORT_INCOMPLETE_TEXT: &str = r#"
## Preview recording stopped!

Preview Recorder has stopped recording previews. All the previews generated so far can be found in the
export directory (which will be opened in the next step).

You may close the preview recording project tab (no need to save).
"#;

const PREVIEW_RECORDER_DONE_EXPORT_COMPLETE_TEXT: &str = r#"
## Preview recording done!

Preview Recorder has recorded all previews. They can be found in the export directory (which will be opened in the next step).

You may close the preview recording project tab (no need to save).
"#;

const PREVIEW_RECORDER_DONE_INTERNAL_INCOMPLETE_TEXT: &str = r#"
## Preview recording stopped!

Preview Recorder has stopped recording previews. All the previews generated so far will be
automatically available in Preset Browser.

You may close the preview recording project tab (no need to save).
"#;

const PRESET_CRAWLER_MOUSE_TEXT: &str = r#"
Now you have 10 seconds to place the mouse cursor on top of the "Next preset" button.

When it's there, simply wait, don't move the mouse.
"#;

const PRESET_CRAWLER_MOUSE_SAVE_AS_TEXT: &str = r#"
Now you have 10 seconds to place the mouse cursor on top of the plug-in's "Save preset as..." button — the button that opens the plug-in's own save dialog.

Don't click it! When the cursor is there, simply wait, don't move the mouse.
"#;

const PRESET_CRAWLER_MOUSE_CANCEL_TEXT: &str = r#"
One more position to capture: the "Cancel" button of the save dialog.

Open the save dialog yourself now by clicking the plug-in's "Save preset as..." button. Then place the mouse cursor on top of the dialog's "Cancel" button and wait, without moving the mouse.

After the countdown, close the dialog again (click "Cancel") before starting to crawl.
"#;

const PRESET_CRAWLER_MOUSE_FAILURE_TEXT: &str = r#"
Preset Crawler couldn't figure out which plug-in you want to crawl!

Press "Try again" and make sure to give the plug-in focus before placing the mouse cursor, e.g. by clicking somewhere onto its surface.
"#;

const PRESET_CRAWLER_READY_TEXT: &str = r#"
## Congratulations!

Now, Preset Crawler knows which plug-in you want to crawl and where's the
"Next preset" button.

## Next step

Check if the plug-in and the mouse cursor position displayed below are correct. Make sure not to move the plug-in window anymore. You can still move the Pot Browser window, no problem. It's best to move it to the side so you can see this dialog.

When you are ready, press "Start crawling". No worries, at this point, Preset Crawler will not yet save FX chains into your "FXChains" folder. As soon as the preset crawling is finished, you can choose to import or discard the results. 

## Important

- As soon as you press "Start crawling", Preset Crawler will take over your mouse! That means you can't easily press "Cancel". Instead, just press the "Escape" key of your keyboard (the top-left key)!
- Try not to move the mouse during crawling!
"#;

const PREVIEW_RECORDER_READY_TEXT: &str = r#"
## Ready!

Preview Recorder is ready to record presets in the following order.

## Next step

- You might want to move the Pot Browser window to the side so you can see the progress, which would otherwise be covered by plug-in windows or REAPER's rendering dialog.
- When you are ready, press "Continue".

## Possible issues

- Some plug-ins have the bad habit to block the user interface by opening a dialog window. If this happens, it will pause the complete recording process. You will need to close the window in order to resume the recording process.

## Important

- If you want to cancel preview recording, press the "Escape" key of your keyboard (the top-left key)!
- It's best to not use the computer while previews are being generated.
"#;

const PRESET_CRAWLER_INCOMPATIBLE_PLUGIN_TEXT: &str = r#"
## Bad news

Preset Crawler detected that it's not possible to crawl your plug-in because it doesn't correctly update the current preset name in REAPER's preset menu.

## Troubleshooting

- Or maybe you didn't place the mouse cursor directly over the "Next preset" button? 
- Did you try to crawl a VST3 plug-in? It seems that many VST3 plug-ins don't correctly expose the current preset name, in general. You might have more success with crawling the VST2 version of your plug-in.
"#;

const PRESET_CRAWLER_PRESET_NAME_NOT_CHANGING: &str = r#"
## Crawling stopped

Preset Crawler detected that the preset name wasn't changing anymore. That's usually a sign that the end of the preset list has been reached.

## Troubleshooting

It's possible that Preset Crawler stopped prematurely. This happens in the following cases:

1. Multiple presets in a row had the same name (which made Preset Crawler guess that it's the last preset).
2. Multiple presets in a row had very similar names and only the ending was different, but the plug-in cropped the ending.

If this happened, please tick the "Never stop crawling" checkbox next time.

"#;

const PRESET_CRAWLER_PRESET_NAME_LIKE_FIRST: &str = r#"
## Crawling stopped

Preset Crawler detected that the preset names of the recently crawled presets are the same as the ones crawled at the beginning. That's usually a sign that the end of the preset list has been reached and we are back at its beginning. 

It can also mean that the plug-in navigates through its presets in a very non-linear fashion. If this happened, please tick the "Never stop crawling" checkbox next time.

"#;

const PRESET_CRAWLER_INTERRUPTED: &str = r#"
## Crawling interrupted

It seems you have pressed the "Escape" key, so we have interrupted crawling.

"#;

const PRESET_CRAWLER_DESTINATION_FILE_EXISTS: &str = r#"
## Crawling stopped

Preset Crawler detected that the destination file of the last-crawled preset already exists. You have chosen to stop crawling in that case, so here we are. 

"#;

const LEGACY_VST3_SCAN_WARNING: &str = r#"
## Attention

Pot Browser has detected that some of your VST3 plug-ins were scanned by a very old version of REAPER. The scan misses parts of information that are important for Pot Browser to work correctly. Therefore, Pot Browser will ignore those plug-ins for now!

## What can I do?

**In order to fix the issue, let REAPER do a full re-scan of your VST plug-ins!**

Options ➡ Preferences ➡ Plug-ins ➡ VST ➡ Re-scan... ➡ Clear cache and re-scan VST paths for all plug-ins

After this, press 🔃 in Pot Browser. Next time you start Pot Browser, this warning should not show up anymore. 
"#;

const UNSUPPORTED_PRESET_FORMAT_TEXT: &str = r#"
Unfortunately, Pot can't automatically open preset formats of Native Instrument's own plug-ins.

**If you don't want such unsupported presets to show up in Pot Browser, enable the ✔ filter in the toolbar.**

You have the following options to load the preset anyway.

## Option A: Load the preset manually

1. Load the plug-in, e.g. by right-clicking the preset and choosing "Associated products".
2. Then find the preset within the plug-in's user interface. If the plug-in supports drag'n'drop, e.g. Kontakt, you can alternatively reveal the preset file in the file explorer (available in right click menu as well) and drag it onto the plug-in.

## Option B: Preset crawling

Try to use Preset Crawler (menu "Tools") to crawl the presets of Native Instrument plug-ins. All presets that have been successfully crawled and imported should be loaded automatically in future. One issue is that crawling only seems to work with the VST2 versions of the NI plug-ins.

In both cases, you will not be able to take advantage of the preset parameter banks. That's just how it is for now.
"#;

const PRESET_CRAWLER_IMPORT_OR_DISCARD: &str =
    r#"You can now choose to import the crawled presets or discard them!"#;

fn optional_string(text: Option<&str>) -> &str {
    text.unwrap_or("-")
}

/// Formats a raw REAPER volume value (a linear gain factor) as decibels.
fn format_volume_as_db(volume: f64) -> String {
    if volume <= 0.0 {
        "-inf dB".to_string()
    } else {
        format!("{:.2} dB", 20.0 * volume.log10())
    }
}

fn os_document_or_reaper_resource_dir(reaper_resource_dir: &Utf8Path) -> Utf8PathBuf {
    dirs::document_dir()
        .and_then(|dir| Utf8PathBuf::from_path_buf(dir).ok())
        .unwrap_or_else(|| reaper_resource_dir.to_path_buf())
}

fn get_preview_rpp_path(
    built_in_path: Option<&Utf8Path>,
    output_config: &PreviewOutputConfig,
) -> anyhow::Result<Utf8PathBuf> {
    use anyhow::Context;
    let built_in_template_path =
        built_in_path.context("Built-in pot preview template not available")?;
    match output_config {
        PreviewOutputConfig::ForPotBrowserPlayback => Ok(built_in_template_path.to_path_buf()),
        PreviewOutputConfig::Export(c) => {
            let base_dir_parent = c.base_dir.parent().expect("base dir should have parent");
            let custom_template_path = base_dir_parent.join("pot-preview.RPP");
            if custom_template_path.exists() {
                Ok(custom_template_path)
            } else {
                let custom_template_path_url = Url::from_file_path(&custom_template_path)
                    .map_err(|_| anyhow!("Couldn't turn base dir parent into URL"))?;
                let _ = fs::create_dir_all(base_dir_parent);
                fs::copy(built_in_template_path, &custom_template_path)?;
                let msg = format!(
                    "You have chosen to render preset previews for export purposes. In order to give you full control about the audio format and other aspects of the preview rendering process, Pot Browser uses a user-customizable template RPP file to render the previews.\n\
                    \n\
                    Pot Browser copied the default template (export in OGG format) to the following location:\n\
                    \n\
                    [{custom_template_path}]({custom_template_path_url})\n\
                    \n\
                    Please review it and adjust it to your needs. Simply open it in REAPER, modify the render settings as desired, press \"Save Settings\" in the render panel and don't forget to save the project file itself after having made the adjustments. Then come back here again. You need to do this only once!\n\
                    \n\
                    Oh, and if you have messed things up, simply delete that file and Pot Browser will restore its default template ;)"
                );
                Err(anyhow!(msg))
            }
        }
    }
}

fn open_link(thing: &str) {
    if thing.starts_with("file://") {
        reveal_path(Path::new(thing));
    } else {
        #[cfg(not(all(target_os = "windows", target_arch = "x86")))]
        {
            if opener::open_browser(thing).is_err() {
                open_link_fallback(thing);
            }
        }
        #[cfg(all(target_os = "windows", target_arch = "x86"))]
        {
            open_link_fallback(thing);
        }
    }
}

fn open_link_fallback(link: &str) {
    report_ui_error(format!(
        "Failed to open the following link in your browser. Please open it manually:\n\n{link}"
    ));
}

thread_local! {
    /// Errors that occurred in deeply nested UI code that doesn't have access to the
    /// toasts instance. Drained into toasts at the end of each frame.
    static PENDING_UI_ERRORS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn report_ui_error(msg: String) {
    PENDING_UI_ERRORS.with(|q| q.borrow_mut().push(msg));
}

fn drain_ui_errors(toasts: &mut Toasts) {
    PENDING_UI_ERRORS.with(|q| {
        for msg in q.borrow_mut().drain(..) {
            toasts.error(msg, Duration::from_secs(10));
        }
    });
}

fn reveal_path(path: impl AsRef<Path>) {
    let path = path.as_ref();
    #[cfg(not(all(target_os = "windows", target_arch = "x86")))]
    {
        if opener::reveal(path).is_err() {
            reveal_path_fallback(path);
        }
    }
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    {
        reveal_path_fallback(path);
    }
}

fn reveal_path_fallback(path: &Path) {
    report_ui_error(
        format!("Failed to open the following path in your file manager. Please open it manually:\n\n{path:?}")
    );
}
