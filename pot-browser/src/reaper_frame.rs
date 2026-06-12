use crate::PotBrowserIntegration;
use base::blocking_lock_arc;
use camino::{Utf8Path, Utf8PathBuf};
use pot::{CurrentPreset, PotFilterExcludes, RuntimePotUnit, SharedRuntimePotUnit};
use reaper_high::{Fx, FxParameter, Reaper};

/// A snapshot of all REAPER state that the render code reads.
///
/// This must be captured on REAPER's main thread, before the frame is rendered. The render
/// code itself must not call into REAPER because on some platforms (baseview on X11), it
/// doesn't run on the main thread.
#[derive(Debug)]
pub struct ReaperFrame {
    /// REAPER's resource path. Constant for the lifetime of the process.
    pub resource_path: Utf8PathBuf,
    /// The FX that currently has keyboard focus, if any.
    pub focused_fx: Option<FocusedFxSnapshot>,
    /// Data for the "Load into" destination panel.
    pub destination: DestinationSnapshot,
    /// The FX that the destination currently points to (= the "currently loaded" FX).
    pub current_fx: Option<Fx>,
    /// Data for the macro-parameter top panel. Only `Some` if there's a current FX *and*
    /// the integration knows a current preset for it.
    pub current_preset_panel: Option<CurrentPresetPanelSnapshot>,
    /// Globally excluded filter items. Lives in main-thread-only ReaLearn state
    /// (`Backbone` is `Fragile`-protected), so it must be cloned out during capture.
    pub filter_excludes: PotFilterExcludes,
    /// Path to the bundled preview RPP template. The first resolution touches
    /// main-thread-only state, so it's captured here instead of being read during
    /// rendering.
    pub preview_template_path: Option<&'static Utf8Path>,
}

/// Snapshot of everything the macro-parameter top panel displays.
#[derive(Debug)]
pub struct CurrentPresetPanelSnapshot {
    pub preset_name: String,
    pub has_params: bool,
    pub bank_count: u32,
    /// Display labels of all banks, by index (for the bank picker).
    pub bank_labels: Vec<String>,
    /// The currently selected bank. `None` if the selected bank index doesn't exist.
    pub current_bank: Option<BankSnapshot>,
}

#[derive(Debug)]
pub struct BankSnapshot {
    /// One entry per macro-param slot. `None` = empty slot (renders as empty column).
    pub slots: Vec<Option<ParamSlotSnapshot>>,
}

#[derive(Debug)]
pub struct ParamSlotSnapshot {
    pub section: String,
    pub macro_name: String,
    /// Tooltip: the real FX parameter name, or an explanation if the mapped parameter
    /// doesn't exist in the actual plug-in.
    pub hover_text: String,
    /// Handle for writing the parameter (via command, on the main thread).
    /// `None` if the mapped parameter doesn't resolve in the actual plug-in.
    pub fx_param: Option<FxParameter>,
    /// Current REAPER-normalized value.
    pub value: f64,
    /// The current value, formatted by the plug-in.
    pub formatted_value: String,
}

#[derive(Debug)]
pub struct FocusedFxSnapshot {
    /// FX handle, kept for later use. Methods on this handle talk to REAPER, so it must
    /// only be *used* on the main thread. Holding it is fine on any thread.
    pub fx: Fx,
    pub name: String,
    pub is_open_in_floating_window: bool,
}

#[derive(Debug, Default)]
pub struct DestinationSnapshot {
    /// Number of tracks in the current project.
    pub track_count: u32,
    /// Labels of all tracks in the current project, by index.
    pub track_labels: Vec<String>,
    /// Label of the resolved destination track. `None` if the destination track can't be
    /// resolved (e.g. refers to a track that doesn't exist anymore).
    pub resolved_track_label: Option<String>,
    /// Names of the FX instances in the resolved destination track's normal FX chain.
    /// Empty if the destination track can't be resolved.
    pub fx_names: Vec<String>,
}

impl ReaperFrame {
    /// Captures a snapshot of everything the pot browser render pass needs from REAPER.
    ///
    /// Must be called on REAPER's main thread. Locks the given pot unit for a short time
    /// (it needs the destination descriptor in order to resolve the destination track).
    pub fn capture<I: PotBrowserIntegration>(
        integration: &I,
        shared_pot_unit: &SharedRuntimePotUnit,
        bank_index: u32,
    ) -> Self {
        let pot_unit = blocking_lock_arc(shared_pot_unit, "ReaperFrame capture");
        Self::capture_internal(integration, &pot_unit, bank_index)
    }

    fn capture_internal<I: PotBrowserIntegration>(
        integration: &I,
        pot_unit: &RuntimePotUnit,
        bank_index: u32,
    ) -> Self {
        let reaper = Reaper::get();
        let focused_fx = reaper.focused_fx().map(|res| FocusedFxSnapshot {
            name: res.fx.name().into_string(),
            is_open_in_floating_window: res.fx.floating_window().is_some(),
            fx: res.fx,
        });
        let current_project = reaper.current_project();
        let track_count = current_project.track_count();
        let track_labels = (0..track_count)
            .filter_map(|i| {
                let track = current_project.track_by_index(i)?;
                Some(integration.get_track_label(&track))
            })
            .collect();
        let resolved_track = pot_unit
            .destination_descriptor
            .track
            .resolve(current_project);
        let resolved_track_label = resolved_track
            .as_ref()
            .ok()
            .map(|t| integration.get_track_label(t));
        let fx_names = resolved_track
            .ok()
            .map(|t| {
                let chain = t.normal_fx_chain();
                (0..chain.fx_count())
                    .filter_map(|i| Some(chain.fx_by_index(i)?.name().into_string()))
                    .collect()
            })
            .unwrap_or_default();
        let current_fx = pot_unit
            .resolve_destination()
            .ok()
            .and_then(|inst| inst.get_existing().and_then(|dest| dest.resolve()));
        let current_preset_panel = current_fx.as_ref().and_then(|fx| {
            let mut snapshot = None;
            integration.with_current_fx_preset(fx, |current_preset| {
                if let Some(current_preset) = current_preset {
                    snapshot = Some(capture_current_preset_panel(fx, current_preset, bank_index));
                }
            });
            snapshot
        });
        let filter_excludes = {
            let mut excludes = PotFilterExcludes::default();
            integration.with_pot_filter_exclude_list(|list| excludes = list.clone());
            excludes
        };
        Self {
            resource_path: reaper.resource_path(),
            focused_fx,
            destination: DestinationSnapshot {
                track_count,
                track_labels,
                resolved_track_label,
                fx_names,
            },
            current_fx,
            current_preset_panel,
            filter_excludes,
            preview_template_path: integration.pot_preview_template_path(),
        }
    }
}

fn capture_current_preset_panel(
    fx: &Fx,
    current_preset: &CurrentPreset,
    bank_index: u32,
) -> CurrentPresetPanelSnapshot {
    let bank_count = current_preset.macro_param_bank_count();
    let bank_labels = (0..bank_count)
        .map(|i| match current_preset.find_macro_param_bank_at(i) {
            Some(bank) => format!("{}. {}", i + 1, bank.name()),
            None => format!("Bank {} (doesn't exist)", i + 1),
        })
        .collect();
    let current_bank = current_preset
        .find_macro_param_bank_at(bank_index)
        .map(|bank| BankSnapshot {
            slots: bank
                .params()
                .iter()
                .map(|macro_param| {
                    let pot_fx_param = macro_param.fx_param?;
                    let fx_param = pot_fx_param.resolved_param_index.and_then(|i| {
                        let fx_param = fx.parameter_by_index(i);
                        if fx_param.is_available() {
                            Some(fx_param)
                        } else {
                            None
                        }
                    });
                    let hover_text = if let Some(p) = &fx_param {
                        p.name().map(|n| n.into_string()).unwrap_or_default()
                    } else {
                        format!(
                            "Mapped parameter {} doesn't exist in actual plug-in",
                            pot_fx_param.param_id
                        )
                    };
                    let (value, formatted_value) = if let Some(p) = &fx_param {
                        let value = p.reaper_normalized_value();
                        let formatted = p
                            .format_reaper_normalized_value(value)
                            .map(|s| s.into_string())
                            .unwrap_or_default();
                        (value.get(), formatted)
                    } else {
                        (0.0, String::new())
                    };
                    Some(ParamSlotSnapshot {
                        section: macro_param.section.clone().unwrap_or_default(),
                        macro_name: macro_param.name.clone(),
                        hover_text,
                        fx_param,
                        value,
                        formatted_value,
                    })
                })
                .collect(),
        });
    CurrentPresetPanelSnapshot {
        preset_name: current_preset.preset().name().to_string(),
        has_params: current_preset.has_params(),
        bank_count,
        bank_labels,
        current_bank,
    }
}
