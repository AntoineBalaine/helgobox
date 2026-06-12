use base::{blocking_lock_arc, NamedChannelSender, SenderToNormalThread};
use helgobox_api::persistence::PotFilterKind;
use pot::{
    FilterItemId, LoadPresetError, LoadPresetOptions, PotPreset, PresetId, SharedRuntimePotUnit,
};
use reaper_high::{Fx, FxParameter};
use reaper_medium::{ReaperNormalizedFxParamValue, ReaperVolumeValue};
use swell_ui::Window;

/// A REAPER-mutating action requested by the render code.
///
/// The render code must not call into REAPER itself (on some platforms it doesn't run on
/// the main thread), so it pushes commands onto a queue instead. The host executes them on
/// the main thread after the frame, via [`crate::process_pending_reaper_commands`].
#[derive(Debug)]
pub enum ReaperCommand {
    LoadPreset {
        preset: Box<PotPreset>,
        options: LoadPresetOptions,
        /// Window that should regain focus after loading.
        os_window: Window,
    },
    PlayPreview(PresetId),
    /// Sets an FX parameter to a REAPER-normalized value (macro-param slider drag).
    SetFxParameter { param: FxParameter, value: f64 },
    /// Sets the preview playback volume.
    SetPreviewVolume(ReaperVolumeValue),
    /// Shows the given FX in its FX chain window.
    ShowFxChain(Fx),
    /// Shows the given FX in a floating window.
    ShowFx(Fx),
    /// Adds a filter item to / removes it from the global exclude list (which lives in
    /// main-thread-only state).
    IncludeFilterItem {
        kind: PotFilterKind,
        id: FilterItemId,
        include: bool,
    },
}

/// Feedback from main-thread command execution back to the render code.
///
/// Arrives one frame after the command was issued.
#[derive(Debug)]
pub enum UiFeedback {
    Error(String),
    /// Loading failed because of an unsupported preset format.
    UnsupportedPresetFormat {
        file_extension: String,
        is_shim_preset: bool,
    },
}

/// Executes one REAPER command. Must be called on REAPER's main thread.
pub(crate) fn execute_reaper_command(
    command: ReaperCommand,
    shared_pot_unit: &SharedRuntimePotUnit,
    feedback_sender: &SenderToNormalThread<UiFeedback>,
) {
    match command {
        ReaperCommand::LoadPreset {
            preset,
            options,
            os_window,
        } => {
            let mut pot_unit = blocking_lock_arc(shared_pot_unit, "execute LoadPreset");
            if let Err(e) = pot_unit.load_preset(&preset, options) {
                let feedback = match e {
                    LoadPresetError::UnsupportedPresetFormat {
                        file_extension,
                        is_shim_preset,
                    } => UiFeedback::UnsupportedPresetFormat {
                        file_extension,
                        is_shim_preset,
                    },
                    e => UiFeedback::Error(e.to_string()),
                };
                feedback_sender.send_complaining(feedback);
            }
            os_window.focus_first_child();
        }
        ReaperCommand::PlayPreview(preset_id) => {
            let mut pot_unit = blocking_lock_arc(shared_pot_unit, "execute PlayPreview");
            if let Err(e) = pot_unit.play_preview(preset_id) {
                feedback_sender.send_complaining(UiFeedback::Error(e.to_string()));
            }
        }
        ReaperCommand::SetFxParameter { param, value } => {
            if let Err(e) =
                param.set_reaper_normalized_value(ReaperNormalizedFxParamValue::new(value))
            {
                feedback_sender.send_complaining(UiFeedback::Error(e.to_string()));
            }
        }
        ReaperCommand::SetPreviewVolume(volume) => {
            let mut pot_unit = blocking_lock_arc(shared_pot_unit, "execute SetPreviewVolume");
            pot_unit.set_preview_volume(volume);
        }
        ReaperCommand::ShowFxChain(fx) => {
            if let Err(e) = fx.show_in_chain() {
                feedback_sender.send_complaining(UiFeedback::Error(e.to_string()));
            }
        }
        ReaperCommand::ShowFx(fx) => {
            let _ = fx.show_in_floating_window();
        }
        ReaperCommand::IncludeFilterItem { kind, id, include } => {
            let mut pot_unit = blocking_lock_arc(shared_pot_unit, "execute IncludeFilterItem");
            pot_unit.include_filter_item(kind, id, include, shared_pot_unit.clone());
        }
    }
}
