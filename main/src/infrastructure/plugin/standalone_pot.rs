//! A Pot unit that belongs to the shell/extension itself, independent of any Helgobox
//! VST instance.
//!
//! Historically, pot units are owned by ReaLearn instances because the original vision
//! was controller-driven preset browsing (MIDI mappings with Pot targets). For plain
//! mouse/script-driven browsing — e.g. the `HB_Pot_*` ReaScript API and the Lua browser —
//! requiring a VST instance in the project is an artificial constraint. This module
//! provides a global pot unit with a minimal [`PotIntegration`] implementation instead:
//! favorites and the exclude list already live in global state, change notifications
//! have no instance-bound consumers, and there's no own FX that needs protecting during
//! preset loading.

use crate::domain::{AnyThreadBackboneState, Backbone};
use base::{blocking_read_lock, blocking_write_lock};
use helgobox_api::persistence::PotFilterKind;
use pot::{
    CurrentPreset, OptFilter, PotFavorites, PotFilterExcludes, PotIntegration, PotUnit, PresetId,
    SharedRuntimePotUnit,
};
use reaper_high::{Fx, Reaper};
use std::cell::RefCell;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

thread_local! {
    /// Main-thread-only on purpose: loading constructs REAPER handles, and all current
    /// consumers (ReaScript API, command executors) run on the main thread anyway.
    static STANDALONE_POT_UNIT: RefCell<PotUnit> = RefCell::new(PotUnit::default());
}

/// Returns the global, instance-independent pot unit, loading it on first use.
///
/// Must be called from the main thread.
pub fn standalone_pot_unit() -> Result<SharedRuntimePotUnit, &'static str> {
    STANDALONE_POT_UNIT.with(|unit| {
        unit.borrow_mut()
            .loaded(Box::new(StandalonePotIntegration::new()?))
    })
}

struct StandalonePotIntegration {
    /// An FX handle that never refers to an existing FX. The standalone unit has no own
    /// FX that must survive preset loading; the handle is only ever used for equality
    /// comparisons against real FX instances, which all fail.
    protected_fx: Fx,
}

impl StandalonePotIntegration {
    fn new() -> Result<Self, &'static str> {
        let master_track = Reaper::get()
            .current_project()
            .master_track()
            .map_err(|_| "couldn't get master track for standalone pot unit")?;
        let integration = Self {
            protected_fx: master_track
                .normal_fx_chain()
                .fx_by_index_untracked(u32::MAX),
        };
        Ok(integration)
    }
}

impl PotIntegration for StandalonePotIntegration {
    fn favorites(&self) -> &RwLock<PotFavorites> {
        &AnyThreadBackboneState::get().pot_favorites
    }

    fn set_current_fx_preset(&self, fx: Fx, preset: CurrentPreset) {
        Backbone::target_state()
            .borrow_mut()
            .set_current_fx_preset(fx, preset);
    }

    fn exclude_list(&self) -> RwLockReadGuard<PotFilterExcludes> {
        blocking_read_lock(
            &AnyThreadBackboneState::get().pot_filter_exclude_list,
            "standalone pot exclude list",
        )
    }

    fn exclude_list_mut(&self) -> RwLockWriteGuard<PotFilterExcludes> {
        blocking_write_lock(
            &AnyThreadBackboneState::get().pot_filter_exclude_list,
            "standalone pot exclude list mut",
        )
    }

    fn notify_preset_changed(&self, _id: Option<PresetId>) {
        // No instance-bound consumers.
    }

    fn notify_filter_changed(&self, _kind: PotFilterKind, _filter: OptFilter) {
        // No instance-bound consumers.
    }

    fn notify_indexes_rebuilt(&self) {
        // No instance-bound consumers.
    }

    fn protected_fx(&self) -> &Fx {
        &self.protected_fx
    }
}
