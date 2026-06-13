//! ReaScript-callable API for the Pot preset browser engine (`HB_Pot_*`).
//!
//! This exposes the browsing surface of Pot (refresh, filters, search, preset list,
//! preview, load) to ReaScript (Lua/EEL/Python) and to other extensions, so that
//! alternative UIs — e.g. a ReaImGui script — can be built on top of the engine
//! without touching the built-in egui browser.
//!
//! Conventions (following the usual REAPER extension API style):
//! - Iteration is count + by-index: `HB_Pot_GetPresetCount()` /
//!   `HB_Pot_GetPresetName(index, buf, buf_sz)`.
//! - Strings are returned through caller-provided buffers. From Lua, REAPER handles the
//!   buffers automatically: `local ok, name = reaper.HB_Pot_GetPresetName(i)`.
//! - Filter kinds are addressed by name: "database", "is_available", "is_supported",
//!   "is_user", "product_kind", "is_favorite", "project", "bank", "sub_bank",
//!   "category", "sub_category", "mode", "has_preview".
//! - All functions must be called from the main thread (ReaScript always does).
//!
//! All functions operate on a global, instance-independent pot unit, so the API works
//! with no Helgobox VST instance in the project.
#![allow(non_snake_case)]

use crate::standalone_unit::standalone_pot_unit;
use base::blocking_lock_arc;
use helgobox_api::persistence::PotFilterKind;
use pot::{
    pot_db, preview_exists, ChangeHint, Debounce, LoadPresetOptions, PotPreset, PotPresetKind,
    PresetId, RuntimePotUnit, SharedRuntimePotUnit,
};
use reaper_high::Reaper;
use reaper_medium::ReaperVolumeValue;
use reaper_low::raw::ApiVararg;
use reaper_medium::RegistrationObject;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};

// ============================================================================
// Pot unit access
// ============================================================================

/// Runs the given function with the global, instance-independent pot unit.
///
/// The API deliberately doesn't bind to a ReaLearn instance's pot unit: browsing,
/// filtering, previewing and loading don't need one, so the API works in an empty
/// project. (Controller-driven browsing via ReaLearn Pot targets keeps operating on the
/// per-instance units — those are separate worlds by design.)
fn with_pot_unit<R>(f: impl FnOnce(&SharedRuntimePotUnit, &mut RuntimePotUnit) -> R) -> Option<R> {
    // Wrap in firewall: these run behind an `extern "C"` boundary, where a panic would
    // abort REAPER rather than unwind. firewall catches any panic and turns it into None.
    reaper_low::firewall(|| {
        let shared = standalone_pot_unit().ok()?;
        let mut unit = blocking_lock_arc(&shared, "pot API");
        Some(f(&shared, &mut unit))
    })
    .flatten()
}

// ============================================================================
// Preset field cache
// ============================================================================
//
// Looking up a preset by ID can hit a database (e.g. SQLite for Komplete), and a UI
// lists the visible rows on every frame. Cache the displayed fields per preset ID,
// invalidated whenever the pot database revision changes (= after a refresh).

#[derive(Clone)]
struct PresetFields {
    name: String,
    product: String,
    file_ext: String,
    has_preview: bool,
    /// Secondary context label (e.g. the project a project-preset came from). Empty if none.
    context_name: String,
    /// Filesystem path of the preset file, or empty for non-file-based presets.
    path: String,
    /// Filesystem path of the preview file, or empty if none exists.
    preview_path: String,
}

thread_local! {
    static PRESET_FIELD_CACHE: RefCell<(u8, HashMap<PresetId, Option<PresetFields>>)> =
        RefCell::new((0, HashMap::new()));
}

fn preset_fields_at(unit: &RuntimePotUnit, index: i32) -> Option<PresetFields> {
    let index = u32::try_from(index).ok()?;
    let preset_id = unit.find_preset_id_at_index(index)?;
    PRESET_FIELD_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let revision = pot_db().revision();
        if cache.0 != revision {
            cache.0 = revision;
            cache.1.clear();
        }
        cache
            .1
            .entry(preset_id)
            .or_insert_with(|| {
                let preset: PotPreset = pot_db().try_find_preset_by_id(preset_id).ok()??;
                let resource_path = Reaper::get().resource_path();
                let file_ext = match &preset.kind {
                    PotPresetKind::FileBased(k) => k.file_ext.clone(),
                    _ => String::new(),
                };
                let path = match &preset.kind {
                    PotPresetKind::FileBased(k) => k.path.to_string(),
                    _ => String::new(),
                };
                let has_preview = preview_exists(&preset, &resource_path);
                let preview_path = pot::find_preview_file(&preset, &resource_path)
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                Some(PresetFields {
                    name: preset.name().to_string(),
                    product: preset.common.product_name.clone().unwrap_or_default(),
                    file_ext,
                    has_preview,
                    context_name: preset.common.context_name.clone().unwrap_or_default(),
                    path,
                    preview_path,
                })
            })
            .clone()
    })
}

// ============================================================================
// Small FFI helpers
// ============================================================================

/// Copies a string into a caller-provided buffer, NUL-terminated, truncating if needed.
unsafe fn copy_to_buf(s: &str, buf: *mut c_char, buf_sz: c_int) -> bool {
    if buf.is_null() || buf_sz <= 0 {
        return false;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(buf_sz as usize - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
    *buf.add(n) = 0;
    true
}

unsafe fn int_arg(args: *mut *mut c_void, count: c_int, i: usize) -> c_int {
    if i >= count as usize {
        return 0;
    }
    *args.add(i) as isize as c_int
}

unsafe fn str_arg<'a>(args: *mut *mut c_void, count: c_int, i: usize) -> Option<&'a CStr> {
    if i >= count as usize {
        return None;
    }
    let ptr = *args.add(i) as *const c_char;
    if ptr.is_null() {
        None
    } else {
        Some(CStr::from_ptr(ptr))
    }
}

unsafe fn buf_arg(args: *mut *mut c_void, count: c_int, i: usize) -> *mut c_char {
    if i >= count as usize {
        return std::ptr::null_mut();
    }
    *args.add(i) as *mut c_char
}

fn ret_int(v: c_int) -> *mut c_void {
    v as isize as *mut c_void
}

fn parse_filter_kind(s: &CStr) -> Option<PotFilterKind> {
    use PotFilterKind::*;
    let kind = match s.to_str().ok()? {
        "database" => Database,
        "is_available" => IsAvailable,
        "is_supported" => IsSupported,
        "is_user" => IsUser,
        "product_kind" => ProductKind,
        "is_favorite" => IsFavorite,
        "project" => Project,
        "bank" => Bank,
        "sub_bank" => SubBank,
        "category" => Category,
        "sub_category" => SubCategory,
        "mode" => Mode,
        "has_preview" => HasPreview,
        _ => return None,
    };
    Some(kind)
}

// ============================================================================
// Typed API functions (extension consumers) + vararg shims (ReaScript consumers)
// ============================================================================

extern "C" fn HB_Pot_IsAvailable() -> c_int {
    reaper_low::firewall(|| standalone_pot_unit().is_ok() as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_IsAvailable(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_IsAvailable())
}

extern "C" fn HB_Pot_Refresh() -> c_int {
    with_pot_unit(|shared, unit| unit.refresh_pot(shared.clone())).is_some() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_Refresh(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_Refresh())
}

extern "C" fn HB_Pot_IsBusy() -> c_int {
    with_pot_unit(|_, unit| unit.background_task_elapsed().is_some() || unit.is_refreshing())
        .unwrap_or(false) as c_int
}
unsafe extern "C" fn vararg_HB_Pot_IsBusy(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_IsBusy())
}

extern "C" fn HB_Pot_GetPresetCount() -> c_int {
    with_pot_unit(|_, unit| unit.preset_count() as c_int).unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetPresetCount())
}

extern "C" fn HB_Pot_GetPresetName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(fields) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&fields.name, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetPresetName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetPresetProduct(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(fields) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&fields.product, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetProduct(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_GetPresetProduct(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetPresetFileExt(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(fields) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&fields.file_ext, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetFileExt(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_GetPresetFileExt(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetSelectedPresetIndex() -> c_int {
    with_pot_unit(|_, unit| {
        unit.preset_id()
            .and_then(|id| unit.find_index_of_preset(id))
            .map(|i| i as c_int)
            .unwrap_or(-1)
    })
    .unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetSelectedPresetIndex(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_GetSelectedPresetIndex())
}

extern "C" fn HB_Pot_SetSelectedPresetIndex(index: c_int) {
    with_pot_unit(|_, unit| {
        let id = u32::try_from(index)
            .ok()
            .and_then(|i| unit.find_preset_id_at_index(i));
        unit.set_preset_id(id);
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetSelectedPresetIndex(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    HB_Pot_SetSelectedPresetIndex(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_PlayPreview(index: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(id) = u32::try_from(index)
            .ok()
            .and_then(|i| unit.find_preset_id_at_index(i))
        else {
            return 0;
        };
        unit.play_preview(id).is_ok() as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_PlayPreview(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_PlayPreview(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_HasPreview(index: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        preset_fields_at(unit, index)
            .map(|f| f.has_preview as c_int)
            .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_HasPreview(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_HasPreview(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_GetPreviewVolume() -> c_int {
    with_pot_unit(|_, unit| (unit.preview_volume().get() * 1000.0).round() as c_int).unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetPreviewVolume(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetPreviewVolume())
}

extern "C" fn HB_Pot_SetPreviewVolume(volume_permille: c_int) {
    let raw = (volume_permille.clamp(0, 1000) as f64) / 1000.0;
    with_pot_unit(|_, unit| unit.set_preview_volume(ReaperVolumeValue::new_panic(raw)));
}
unsafe extern "C" fn vararg_HB_Pot_SetPreviewVolume(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    HB_Pot_SetPreviewVolume(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_DarkModeEnabled() -> c_int {
    reaper_low::firewall(|| swell_ui::Window::dark_mode_is_enabled() as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_DarkModeEnabled(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_DarkModeEnabled())
}

extern "C" fn HB_Pot_StopPreview() {
    with_pot_unit(|_, unit| {
        let _ = unit.stop_preview();
    });
}
unsafe extern "C" fn vararg_HB_Pot_StopPreview(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_StopPreview();
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_LoadPreset(index: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(id) = u32::try_from(index)
            .ok()
            .and_then(|i| unit.find_preset_id_at_index(i))
        else {
            return 0;
        };
        let Ok(Some(preset)) = pot_db().try_find_preset_by_id(id) else {
            return 0;
        };
        unit.load_preset(&preset, LoadPresetOptions::default()).is_ok() as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_LoadPreset(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_LoadPreset(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_GetFilterItemCount(kind: *const c_char) -> c_int {
    if kind.is_null() {
        return -1;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return -1;
    };
    with_pot_unit(|_, unit| unit.filter_item_collections.get(kind).len() as c_int).unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetFilterItemCount(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_GetFilterItemCount(kind))
}

extern "C" fn HB_Pot_GetFilterItemName(
    kind: *const c_char,
    index: c_int,
    buf: *mut c_char,
    buf_sz: c_int,
) -> c_int {
    if kind.is_null() {
        return 0;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        let Some(item) = usize::try_from(index)
            .ok()
            .and_then(|i| unit.filter_item_collections.get(kind).get(i))
        else {
            return 0;
        };
        let name = item.effective_leaf_name();
        unsafe { copy_to_buf(&name, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetFilterItemName(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_GetFilterItemName(
        kind,
        int_arg(args, n, 1),
        buf_arg(args, n, 2),
        int_arg(args, n, 3),
    ))
}

extern "C" fn HB_Pot_SetFilter(kind: *const c_char, index: c_int) -> c_int {
    if kind.is_null() {
        return 0;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return 0;
    };
    with_pot_unit(|shared, unit| {
        let filter = if index < 0 {
            None
        } else {
            let Some(item) = unit.filter_item_collections.get(kind).get(index as usize) else {
                return 0;
            };
            Some(item.id)
        };
        unit.set_filter(kind, filter, shared.clone(), Debounce::No);
        1
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_SetFilter(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_SetFilter(kind, int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_GetFilter(kind: *const c_char) -> c_int {
    if kind.is_null() {
        return -1;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return -1;
    };
    with_pot_unit(|_, unit| {
        let Some(current) = unit.get_filter(kind) else {
            return -1;
        };
        unit.filter_item_collections
            .get(kind)
            .iter()
            .position(|item| item.id == current)
            .map(|i| i as c_int)
            .unwrap_or(-1)
    })
    .unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetFilter(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_GetFilter(kind))
}

extern "C" fn HB_Pot_SetSearchText(text: *const c_char) {
    let text = if text.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    };
    with_pot_unit(|shared, unit| {
        unit.runtime_state.search_expression = text;
        unit.rebuild_collections(shared.clone(), ChangeHint::SearchExpression, Debounce::Yes);
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetSearchText(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    let text = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    HB_Pot_SetSearchText(text);
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_GetSearchText(buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| unsafe {
        copy_to_buf(&unit.runtime_state.search_expression, buf, buf_sz) as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetSearchText(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetSearchText(buf_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_GetPresetContextName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(f) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&f.context_name, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetContextName(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_GetPresetContextName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetPresetPath(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(f) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&f.path, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetPath(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetPresetPath(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetPreviewPath(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(f) = preset_fields_at(unit, index) else {
            return 0;
        };
        unsafe { copy_to_buf(&f.preview_path, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPreviewPath(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetPreviewPath(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_SupportsFilter(kind: *const c_char) -> c_int {
    if kind.is_null() {
        return 0;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return 0;
    };
    with_pot_unit(|_, unit| unit.supports_filter_kind(kind) as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_SupportsFilter(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_SupportsFilter(kind))
}

/// Search field index: 0 = preset name, 1 = product name, 2 = file extension.
fn search_field_at(index: c_int) -> Option<pot::SearchField> {
    use pot::SearchField::*;
    match index {
        0 => Some(PresetName),
        1 => Some(ProductName),
        2 => Some(FileExtension),
        _ => None,
    }
}

extern "C" fn HB_Pot_GetSearchField(index: c_int) -> c_int {
    let Some(field) = search_field_at(index) else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        unit.runtime_state
            .search_options
            .search_fields
            .contains(field) as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetSearchField(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetSearchField(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_SetSearchField(index: c_int, on: c_int) {
    let Some(field) = search_field_at(index) else {
        return;
    };
    with_pot_unit(|shared, unit| {
        if on != 0 {
            unit.runtime_state.search_options.search_fields.insert(field);
        } else {
            unit.runtime_state.search_options.search_fields.remove(field);
        }
        unit.rebuild_collections(shared.clone(), ChangeHint::SearchExpression, Debounce::No);
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetSearchField(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetSearchField(int_arg(args, n, 0), int_arg(args, n, 1));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_GetUseWildcards() -> c_int {
    with_pot_unit(|_, unit| unit.runtime_state.search_options.use_wildcards as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetUseWildcards(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetUseWildcards())
}

extern "C" fn HB_Pot_SetUseWildcards(on: c_int) {
    with_pot_unit(|shared, unit| {
        unit.runtime_state.search_options.use_wildcards = on != 0;
        unit.rebuild_collections(shared.clone(), ChangeHint::SearchExpression, Debounce::No);
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetUseWildcards(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetUseWildcards(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_IsFilterItemExcluded(kind: *const c_char, index: c_int) -> c_int {
    if kind.is_null() {
        return 0;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        let Some(item) = usize::try_from(index)
            .ok()
            .and_then(|i| unit.filter_item_collections.get(kind).get(i))
        else {
            return 0;
        };
        let id = item.id;
        crate::standalone_unit::with_exclude_list(|excludes| excludes.contains(kind, id) as c_int)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_IsFilterItemExcluded(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_IsFilterItemExcluded(kind, int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_SetFilterItemExcluded(kind: *const c_char, index: c_int, excluded: c_int) {
    if kind.is_null() {
        return;
    }
    let Some(kind) = parse_filter_kind(unsafe { CStr::from_ptr(kind) }) else {
        return;
    };
    with_pot_unit(|shared, unit| {
        if let Some(item) = usize::try_from(index)
            .ok()
            .and_then(|i| unit.filter_item_collections.get(kind).get(i))
        {
            let id = item.id;
            // include = !excluded
            unit.include_filter_item(kind, id, excluded == 0, shared.clone());
        }
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetFilterItemExcluded(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    let kind = str_arg(args, n, 0).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    HB_Pot_SetFilterItemExcluded(kind, int_arg(args, n, 1), int_arg(args, n, 2));
    std::ptr::null_mut()
}

// ============================================================================
// Registration
// ============================================================================

struct PotApiFn {
    name: &'static str,
    typed: *mut c_void,
    vararg: ApiVararg,
    /// REAPER API definition string: return type, parameter types, parameter names and
    /// help text, separated by NUL bytes. REAPER uses this for the ReaScript docs and
    /// for marshalling output-string buffers in scripting languages.
    def: &'static [u8],
}

macro_rules! pot_api_fns {
    ($( $name:ident: $def:expr; )+) => {
        vec![
            $(
                PotApiFn {
                    name: stringify!($name),
                    typed: $name as *mut c_void,
                    vararg: paste_vararg!($name),
                    def: $def,
                },
            )+
        ]
    };
}

// Tiny helper because we can't concat identifiers in declarative macros without paste.
macro_rules! paste_vararg {
    (HB_Pot_IsAvailable) => { vararg_HB_Pot_IsAvailable };
    (HB_Pot_Refresh) => { vararg_HB_Pot_Refresh };
    (HB_Pot_IsBusy) => { vararg_HB_Pot_IsBusy };
    (HB_Pot_GetPresetCount) => { vararg_HB_Pot_GetPresetCount };
    (HB_Pot_GetPresetName) => { vararg_HB_Pot_GetPresetName };
    (HB_Pot_GetPresetProduct) => { vararg_HB_Pot_GetPresetProduct };
    (HB_Pot_GetPresetFileExt) => { vararg_HB_Pot_GetPresetFileExt };
    (HB_Pot_GetSelectedPresetIndex) => { vararg_HB_Pot_GetSelectedPresetIndex };
    (HB_Pot_SetSelectedPresetIndex) => { vararg_HB_Pot_SetSelectedPresetIndex };
    (HB_Pot_PlayPreview) => { vararg_HB_Pot_PlayPreview };
    (HB_Pot_StopPreview) => { vararg_HB_Pot_StopPreview };
    (HB_Pot_HasPreview) => { vararg_HB_Pot_HasPreview };
    (HB_Pot_DarkModeEnabled) => { vararg_HB_Pot_DarkModeEnabled };
    (HB_Pot_GetPreviewVolume) => { vararg_HB_Pot_GetPreviewVolume };
    (HB_Pot_SetPreviewVolume) => { vararg_HB_Pot_SetPreviewVolume };
    (HB_Pot_LoadPreset) => { vararg_HB_Pot_LoadPreset };
    (HB_Pot_GetFilterItemCount) => { vararg_HB_Pot_GetFilterItemCount };
    (HB_Pot_GetFilterItemName) => { vararg_HB_Pot_GetFilterItemName };
    (HB_Pot_SetFilter) => { vararg_HB_Pot_SetFilter };
    (HB_Pot_GetFilter) => { vararg_HB_Pot_GetFilter };
    (HB_Pot_SetSearchText) => { vararg_HB_Pot_SetSearchText };
    (HB_Pot_GetSearchText) => { vararg_HB_Pot_GetSearchText };
    (HB_Pot_GetPresetContextName) => { vararg_HB_Pot_GetPresetContextName };
    (HB_Pot_GetPresetPath) => { vararg_HB_Pot_GetPresetPath };
    (HB_Pot_GetPreviewPath) => { vararg_HB_Pot_GetPreviewPath };
    (HB_Pot_SupportsFilter) => { vararg_HB_Pot_SupportsFilter };
    (HB_Pot_GetSearchField) => { vararg_HB_Pot_GetSearchField };
    (HB_Pot_SetSearchField) => { vararg_HB_Pot_SetSearchField };
    (HB_Pot_GetUseWildcards) => { vararg_HB_Pot_GetUseWildcards };
    (HB_Pot_SetUseWildcards) => { vararg_HB_Pot_SetUseWildcards };
    (HB_Pot_IsFilterItemExcluded) => { vararg_HB_Pot_IsFilterItemExcluded };
    (HB_Pot_SetFilterItemExcluded) => { vararg_HB_Pot_SetFilterItemExcluded };
}

fn pot_api_fns() -> Vec<PotApiFn> {
    pot_api_fns![
        HB_Pot_IsAvailable:
            b"int\0\0\0Returns 1 if a Pot unit is available (requires at least one Helgobox instance).\0";
        HB_Pot_Refresh:
            b"int\0\0\0Rescans all preset databases. Returns 0 if no Pot unit is available.\0";
        HB_Pot_IsBusy:
            b"int\0\0\0Returns 1 while a refresh or query rebuild is running in the background.\0";
        HB_Pot_GetPresetCount:
            b"int\0\0\0Returns the number of presets matching the current filters and search, or -1 if no Pot unit is available.\0";
        HB_Pot_GetPresetName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets the name of the preset at the given index. Returns 0 on failure.\0";
        HB_Pot_GetPresetProduct:
            b"int\0int,char*,int\0index,productOut,productOut_sz\0Gets the product name of the preset at the given index. Returns 0 on failure.\0";
        HB_Pot_GetPresetFileExt:
            b"int\0int,char*,int\0index,extOut,extOut_sz\0Gets the file extension of the preset at the given index (empty for non-file-based presets). Returns 0 on failure.\0";
        HB_Pot_GetSelectedPresetIndex:
            b"int\0\0\0Returns the index of the currently selected preset, or -1.\0";
        HB_Pot_SetSelectedPresetIndex:
            b"void\0int\0index\0Selects the preset at the given index (-1 clears the selection).\0";
        HB_Pot_PlayPreview:
            b"int\0int\0index\0Plays the audio preview of the preset at the given index. Returns 0 if there's no preview.\0";
        HB_Pot_StopPreview:
            b"void\0\0\0Stops audio preview playback.\0";
        HB_Pot_HasPreview:
            b"int\0int\0index\0Returns 1 if the preset at the given index has an audio preview file.\0";
        HB_Pot_DarkModeEnabled:
            b"int\0\0\0Returns 1 if a dark UI theme fits REAPER's current appearance (based on the theme's window background on Windows/Linux, the OS setting on macOS).\0";
        HB_Pot_GetPreviewVolume:
            b"int\0\0\0Returns the preview playback volume as permille of raw gain (0-1000), or -1 if no Pot unit is available.\0";
        HB_Pot_SetPreviewVolume:
            b"void\0int\0volume_permille\0Sets the preview playback volume as permille of raw gain (0-1000).\0";
        HB_Pot_LoadPreset:
            b"int\0int\0index\0Loads the preset at the given index into the configured destination. Returns 0 on failure.\0";
        HB_Pot_GetFilterItemCount:
            b"int\0const char*\0kind\0Returns the number of items for the given filter kind (e.g. \"database\", \"bank\", \"category\"), or -1 for an unknown kind.\0";
        HB_Pot_GetFilterItemName:
            b"int\0const char*,int,char*,int\0kind,index,nameOut,nameOut_sz\0Gets the display name of a filter item. Returns 0 on failure.\0";
        HB_Pot_SetFilter:
            b"int\0const char*,int\0kind,index\0Sets the filter of the given kind to the item at the given index (-1 clears it). Returns 0 on failure.\0";
        HB_Pot_GetFilter:
            b"int\0const char*\0kind\0Returns the index of the currently active filter item of the given kind, or -1.\0";
        HB_Pot_SetSearchText:
            b"void\0const char*\0text\0Sets the search expression and rebuilds the preset list (debounced).\0";
        HB_Pot_GetSearchText:
            b"int\0char*,int\0textOut,textOut_sz\0Gets the current search expression.\0";
        HB_Pot_GetPresetContextName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets the secondary context label of the preset at the given index (e.g. the source project), empty if none. Returns 0 on failure.\0";
        HB_Pot_GetPresetPath:
            b"int\0int,char*,int\0index,pathOut,pathOut_sz\0Gets the filesystem path of the preset file at the given index (empty for non-file-based presets). Returns 0 on failure.\0";
        HB_Pot_GetPreviewPath:
            b"int\0int,char*,int\0index,pathOut,pathOut_sz\0Gets the filesystem path of the preset's preview file at the given index (empty if none). Returns 0 on failure.\0";
        HB_Pot_SupportsFilter:
            b"int\0const char*\0kind\0Returns 1 if the given filter kind is relevant for the current results (use this to show/hide sub-filters).\0";
        HB_Pot_GetSearchField:
            b"int\0int\0field\0Returns 1 if the given search field is enabled. field: 0=preset name, 1=product, 2=extension.\0";
        HB_Pot_SetSearchField:
            b"void\0int,int\0field,on\0Enables or disables a search field (0=preset name, 1=product, 2=extension) and rebuilds the list.\0";
        HB_Pot_GetUseWildcards:
            b"int\0\0\0Returns 1 if wildcard search is enabled.\0";
        HB_Pot_SetUseWildcards:
            b"void\0int\0on\0Enables or disables wildcard search and rebuilds the list.\0";
        HB_Pot_IsFilterItemExcluded:
            b"int\0const char*,int\0kind,index\0Returns 1 if the filter item at the given index of the given kind is globally excluded.\0";
        HB_Pot_SetFilterItemExcluded:
            b"void\0const char*,int,int\0kind,index,excluded\0Globally excludes (excluded=1) or re-includes (excluded=0) the given filter item.\0";
    ]
}

pub fn register_pot_api<E>(
    op: &mut impl FnMut(RegistrationObject) -> Result<(), E>,
) -> Result<(), E> {
    for f in pot_api_fns() {
        op(RegistrationObject::api(f.name, f.typed))?;
        op(RegistrationObject::api_vararg(f.name, f.vararg))?;
        op(RegistrationObject::api_def(
            f.name,
            f.def.as_ptr() as *const c_char,
        ))?;
    }
    Ok(())
}
