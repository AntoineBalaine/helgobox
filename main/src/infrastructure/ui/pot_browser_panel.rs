use crate::application::get_track_label;
use crate::domain::{AnyThreadBackboneState, Backbone};
use crate::infrastructure::plugin::BackboneShell;
use crate::infrastructure::ui::bindings::root;
use crate::infrastructure::ui::egui_views;
use camino::Utf8Path;
use derivative::Derivative;
use pot::{CurrentPreset, PotFavorites, PotFilterExcludes, SharedRuntimePotUnit};
use pot_browser::{run_ui, PotBrowserIntegration, State};
use reaper_high::{Fx, Track};
use reaper_low::raw;
use std::sync::RwLock;
use swell_ui::{SharedView, View, ViewContext, Window};

#[cfg(target_os = "linux")]
mod linux_host {
    use super::*;
    use pot_browser::{HostBridge, ReaperFrame};
    use std::sync::{Arc, Mutex};

    /// Timer ID for the main-thread tick that captures REAPER frames and executes queued
    /// REAPER commands while the render loop runs on baseview's own thread.
    pub const FRAME_TIMER_ID: usize = 322;
    pub const FRAME_TIMER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(30);

    pub struct LinuxHost {
        pub bridge: HostBridge,
        pub shared_frame: Arc<Mutex<ReaperFrame>>,
        /// Handle to the baseview window. Must be closed explicitly when the parent
        /// window is destroyed, otherwise the render-loop thread keeps running.
        pub window_handle: baseview::WindowHandle,
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct PotBrowserPanel {
    view: ViewContext,
    pot_unit: SharedRuntimePotUnit,
    #[cfg(target_os = "linux")]
    #[derivative(Debug = "ignore")]
    linux_host: std::cell::RefCell<Option<linux_host::LinuxHost>>,
}

impl PotBrowserPanel {
    pub fn new(pot_unit: SharedRuntimePotUnit) -> Self {
        Self {
            view: Default::default(),
            pot_unit,
            #[cfg(target_os = "linux")]
            linux_host: Default::default(),
        }
    }
}

impl View for PotBrowserPanel {
    fn dialog_resource_id(&self) -> u32 {
        root::ID_EMPTY_PANEL
    }

    fn view_context(&self) -> &ViewContext {
        &self.view
    }

    fn wants_raw_keyboard_input(&self) -> bool {
        true
    }

    fn opened(self: SharedView<Self>, window: Window) -> bool {
        window.size_and_center_on_screen(0.75, 0.75);
        let show_warning =
            !pot_browser::warning_was_acknowledged(&reaper_high::Reaper::get().resource_path());
        let state = State::new(self.pot_unit.clone(), window, show_warning);
        let bridge = state.host_bridge();
        #[cfg(not(target_os = "linux"))]
        {
            egui_views::open(
                window,
                "Pot Browser",
                state,
                move |context, state| {
                    // On Windows and macOS, this closure runs on the main thread, so we
                    // can capture the REAPER state snapshot right here and execute the
                    // queued REAPER commands right after the frame.
                    let frame = bridge.capture_frame(&RealearnPotBrowserIntegration);
                    run_ui(context, state, &RealearnPotBrowserIntegration, &frame);
                    bridge.process_commands();
                },
            );
        }
        #[cfg(target_os = "linux")]
        {
            use std::sync::{Arc, Mutex};
            // On Linux (X11), baseview runs the render loop on its own thread. REAPER
            // must only be accessed from the main thread, so a main-thread timer (see
            // `timer` below) captures the frame snapshots and executes the queued
            // commands. The render thread only reads the latest snapshot.
            let shared_frame = Arc::new(Mutex::new(
                bridge.capture_frame(&RealearnPotBrowserIntegration),
            ));
            window.set_timer(linux_host::FRAME_TIMER_ID, linux_host::FRAME_TIMER_INTERVAL);
            let cloned_shared_frame = shared_frame.clone();
            let window_handle = egui_views::open_with_send_state(
                window,
                "Pot Browser",
                state,
                move |context, state| {
                    // Tolerate poisoning: a ReaperFrame is plain data, so even if a panic
                    // occurred while the lock was held (the firewall catches those), the
                    // snapshot is still usable and the UI should keep running.
                    let frame = cloned_shared_frame
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    run_ui(context, state, &RealearnPotBrowserIntegration, &frame);
                },
            );
            *self.linux_host.borrow_mut() = Some(linux_host::LinuxHost {
                bridge,
                shared_frame,
                window_handle,
            });
        }
        true
    }

    #[cfg(target_os = "linux")]
    fn on_destroy(self: SharedView<Self>, window: Window) {
        window.kill_timer(linux_host::FRAME_TIMER_ID);
        if let Some(mut host) = self.linux_host.borrow_mut().take() {
            // Stop the baseview render-loop thread. It doesn't stop by itself when the
            // parent window is destroyed.
            host.window_handle.close();
        }
    }

    #[cfg(target_os = "linux")]
    fn timer(&self, id: usize) -> bool {
        if id != linux_host::FRAME_TIMER_ID {
            return false;
        }
        if let Some(host) = self.linux_host.borrow().as_ref() {
            // Execute REAPER commands that the render thread queued up.
            host.bridge.process_commands();
            // Capture a fresh snapshot for the render thread. Captured before taking the
            // lock, so the render thread is never blocked on REAPER calls.
            let frame = host.bridge.capture_frame(&RealearnPotBrowserIntegration);
            *host
                .shared_frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = frame;
        }
        true
    }

    fn button_clicked(self: SharedView<Self>, resource_id: u32) {
        match resource_id {
            // Escape key
            raw::IDCANCEL => self.close(),
            _ => {}
        }
    }

    fn resized(self: SharedView<Self>) -> bool {
        egui_views::on_parent_window_resize(self.view.require_window())
    }
}

struct RealearnPotBrowserIntegration;

impl PotBrowserIntegration for RealearnPotBrowserIntegration {
    fn get_track_label(&self, track: &Track) -> String {
        get_track_label(track)
    }

    fn pot_preview_template_path(&self) -> Option<&'static Utf8Path> {
        BackboneShell::realearn_pot_preview_template_path()
    }

    fn pot_favorites(&self) -> &'static RwLock<PotFavorites> {
        &AnyThreadBackboneState::get().pot_favorites
    }

    fn with_current_fx_preset(&self, fx: &Fx, f: impl FnOnce(Option<&CurrentPreset>)) {
        let target_state = Backbone::target_state().borrow();
        f(target_state.current_fx_preset(fx));
    }

    fn with_pot_filter_exclude_list(&self, f: impl FnOnce(&PotFilterExcludes)) {
        f(&base::blocking_read_lock(
            &AnyThreadBackboneState::get().pot_filter_exclude_list,
            "pot filter exclude list (frame capture)",
        ));
    }
}
