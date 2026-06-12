use egui::{Context, Visuals};
use fragile::Fragile;
use reaper_low::firewall;
use swell_ui::{SwellWindow, Window};

pub mod advanced_script_editor;
pub mod target_filter_panel;

pub fn open<S: 'static>(
    window: Window,
    title: impl Into<String>,
    state: S,
    run_ui: impl Fn(&egui::Context, &mut S) + Send + Sync + 'static,
) {
    // egui-baseview requires the state to be Send but we want to support state that is not Send,
    // that's why we use Fragile, which checks that we access the state from the creating thread
    // only.
    //
    // It's slightly inconvenient to require Send because the Advanced Script Editor has state that
    // contains `ScriptEditorInput` which uses a Box<dyn ScriptEngine>. We would have to make this
    // Send and this would either require some restructuring (viable but too lazy now) or make mlua
    // use the "send" feature, which would have a big effect on the Lua integration (that would be
    // possible but would make everything harder for no win, because we currently don't want to use Lua in
    // any other thread than the main thread anyway).
    // Fortunately, at least on macOS and Windows, the Send requirement of egui-baseview seems to be
    // not really necessary since the state is only being used from the main thread anyway.
    // With baseview for X11, it's a different story: the render loop runs on its own thread there,
    // so accessing Fragile state would panic. Views that should work on Linux must provide
    // genuinely Send state and use [`open_with_send_state`] instead.
    let fragile_state = Fragile::new(state);
    // The window handle is deliberately dropped here: on Windows and macOS (the platforms
    // this function is used on), the render loop stops when the parent window is
    // destroyed, as it always has.
    let _ = open_with_send_state(window, title, fragile_state, move |ctx, state| {
        run_ui(ctx, state.get_mut())
    });
}

/// Like [`open`], but for state that is genuinely `Send`.
///
/// Use this for views that should work on platforms where baseview runs the render loop
/// on a separate thread (X11). The render callback may then be invoked from that thread,
/// so it must not call REAPER functions or touch other main-thread-only state.
///
/// Returns the baseview window handle. On X11, the host should keep it and call
/// `close()` on it when the parent window is destroyed — the render-loop thread doesn't
/// stop by itself when the parent goes away.
pub fn open_with_send_state<S: Send + 'static>(
    window: Window,
    title: impl Into<String>,
    state: S,
    run_ui: impl Fn(&egui::Context, &mut S) + Send + Sync + 'static,
) -> baseview::WindowHandle {
    let title = title.into();
    window.set_text(title.as_str());
    let window_size = window.client_size();
    #[cfg(windows)]
    let (width, height, scale) = {
        let dpi_factor = window.dpi_scaling_factor();
        let width = window_size.width.get() as f64 / dpi_factor;
        let height = window_size.height.get() as f64 / dpi_factor;
        let scale = baseview::WindowScalePolicy::ScaleFactor(dpi_factor);
        (width, height, scale)
    };
    #[cfg(not(windows))]
    let (width, height, scale) = {
        let width = window_size.width.get() as f64;
        let height = window_size.height.get() as f64;
        let scale = baseview::WindowScalePolicy::SystemScaleFactor;
        (width, height, scale)
    };
    let settings = baseview::WindowOpenOptions {
        title,
        size: baseview::Size::new(width, height),
        scale,
        gl_config: Some(Default::default()),
    };
    let window = get_egui_parent_window(window);
    egui_baseview::EguiWindow::open_parented(
        &window,
        settings,
        state,
        |ctx: &egui::Context, _queue: &mut egui_baseview::Queue, _state: &mut S| {
            firewall(|| {
                init_ui(ctx, Window::dark_mode_is_enabled());
            });
        },
        move |ctx: &egui::Context, _queue: &mut egui_baseview::Queue, state: &mut S| {
            firewall(|| {
                run_ui(ctx, state);
            });
        },
    )
}

fn get_egui_parent_window(window: Window) -> SwellWindow {
    #[cfg(target_os = "linux")]
    {
        let x_bridge_window = swell_ui::XBridgeWindow::create(window).unwrap();
        SwellWindow::XBridge(x_bridge_window)
    }
    #[cfg(not(target_os = "linux"))]
    {
        SwellWindow::Normal(window)
    }
}

/// To be called in the `resized` callback of the container window. Takes care of resizing
/// the egui child window to exactly the same size as parent. Works at least on Windows.
pub fn on_parent_window_resize(parent: Window) -> bool {
    #[cfg(windows)]
    {
        parent.resize_first_child_according_to_parent();
        true
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        false
    }
}

fn init_ui(ctx: &Context, dark_mode_is_enabled: bool) {
    let mut style: egui::Style = (*ctx.style()).clone();
    style.visuals = if dark_mode_is_enabled {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    ctx.set_style(style);
}
