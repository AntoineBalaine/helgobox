//! A Pot unit owned by the extension itself, independent of any Helgobox VST instance
//! and of ReaLearn's `Backbone` state.
//!
//! Historically pot units are owned by ReaLearn instances (controller-driven preset
//! browsing via MIDI mappings). For mouse/script-driven browsing — the `HB_Pot_*` API
//! and the Lua browser — that's an artificial constraint. This module provides a global
//! pot unit with a minimal [`PotIntegration`]:
//! - favorites and the exclude list are this crate's own global state (so the crate has
//!   no dependency on ReaLearn's `Backbone`),
//! - change notifications have no instance-bound consumers, so they're no-ops,
//! - `set_current_fx_preset` is a no-op (there's no ReaLearn UI to update),
//! - the "protected FX" is a never-matching handle (the standalone unit owns no FX that
//!   must survive preset loading; the handle is only used for equality checks).

use helgobox_api::persistence::PotFilterKind;
use pot::{
    CurrentPreset, OptFilter, PotFavorites, PotFilterExcludes, PotIntegration, PotUnit, PresetId,
    SharedRuntimePotUnit,
};
use reaper_high::{Fx, Reaper};
use std::cell::RefCell;
use std::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Global favorites for the standalone pot unit.
static FAVORITES: LazyLock<RwLock<PotFavorites>> = LazyLock::new(Default::default);
/// Global filter exclude list for the standalone pot unit.
static EXCLUDE_LIST: LazyLock<RwLock<PotFilterExcludes>> = LazyLock::new(Default::default);

thread_local! {
    /// Main-thread-only on purpose: loading constructs REAPER handles, and all consumers
    /// (the ReaScript API) run on the main thread anyway.
    static STANDALONE_POT_UNIT: RefCell<PotUnit> = RefCell::new(PotUnit::default());
    /// The FX a preset was last loaded into, plus the preset's macro-parameter info.
    /// Drives the macro-parameter panel. (The full plugin keeps this in ReaLearn's
    /// Backbone; standalone keeps its own.)
    static CURRENT_FX_PRESET: RefCell<Option<(Fx, CurrentPreset)>> = const { RefCell::new(None) };
}

/// Runs `f` with the current preset of the given FX, if one was loaded into it.
pub fn with_current_preset<R>(fx: &Fx, f: impl FnOnce(Option<&CurrentPreset>) -> R) -> R {
    CURRENT_FX_PRESET.with(|c| {
        let cell = c.borrow();
        let cp = cell
            .as_ref()
            .filter(|(stored_fx, _)| stored_fx == fx)
            .map(|(_, preset)| preset);
        f(cp)
    })
}

/// Returns the global, instance-independent pot unit, loading it on first use.
///
/// Must be called from the main thread.
pub fn standalone_pot_unit() -> Result<SharedRuntimePotUnit, &'static str> {
    STANDALONE_POT_UNIT.with(|unit| {
        // Fast path: already loaded. Return the existing unit WITHOUT constructing a new
        // integration. Building one calls master_track() — that would run on every single
        // API call (wasteful), and a transient failure there must never make an
        // already-loaded unit report itself unavailable.
        if let PotUnit::Loaded(shared) = &*unit.borrow() {
            return Ok(shared.clone());
        }
        unit.borrow_mut()
            .loaded(Box::new(StandalonePotIntegration::new()?))
    })
}

/// Loads the standalone pot unit and kicks off a full database refresh (scan + collection
/// rebuild) in the background.
///
/// Call this once at extension startup, on the main thread. The scan itself runs on the
/// pot worker thread, so this returns immediately; by the time a browser opens, the
/// preset list is populated. Returns whether the unit could be loaded.
pub fn warm_up() -> bool {
    let Ok(shared) = standalone_pot_unit() else {
        return false;
    };
    let mut unit = base::blocking_lock_arc(&shared, "pot warm_up");
    unit.refresh_pot(shared.clone());
    true
}

/// The global favorites store used by the standalone pot unit. Exposed so a UI layer
/// (e.g. an egui browser hosted in the standalone extension) can implement its own
/// `PotBrowserIntegration` against the same state the API uses.
pub fn favorites() -> &'static RwLock<PotFavorites> {
    LazyLock::force(&FAVORITES)
}

/// Reads the global filter exclude list used by the standalone pot unit.
pub fn with_exclude_list<R>(f: impl FnOnce(&PotFilterExcludes) -> R) -> R {
    let guard = EXCLUDE_LIST
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&guard)
}

struct StandalonePotIntegration {
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
        &FAVORITES
    }

    fn set_current_fx_preset(&self, fx: Fx, preset: CurrentPreset) {
        // Remember it so the macro-parameter panel can show this preset's parameters
        // for the FX it was loaded into.
        CURRENT_FX_PRESET.with(|c| *c.borrow_mut() = Some((fx, preset)));
    }

    fn exclude_list(&self) -> RwLockReadGuard<PotFilterExcludes> {
        EXCLUDE_LIST
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn exclude_list_mut(&self) -> RwLockWriteGuard<PotFilterExcludes> {
        EXCLUDE_LIST
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify_preset_changed(&self, _id: Option<PresetId>) {}

    fn notify_filter_changed(&self, _kind: PotFilterKind, _filter: OptFilter) {}

    fn notify_indexes_rebuilt(&self) {}

    fn protected_fx(&self) -> &Fx {
        &self.protected_fx
    }
}
