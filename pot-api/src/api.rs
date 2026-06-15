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
use pot::CurrentPreset;
use reaper_high::{Fx, FxParameter, Reaper};
use reaper_medium::{ReaperNormalizedFxParamValue, ReaperVolumeValue};
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
    /// Name of the database this preset belongs to.
    database: String,
    vendor: String,
    author: String,
    comment: String,
    /// Modification date, formatted, or empty.
    date: String,
    /// Human-readable file size, or empty.
    file_size: String,
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1 << 10 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
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
                let database = pot_db()
                    .try_with_db(preset_id.database_id, |db| db.name().to_string())
                    .unwrap_or_default();
                let m = &preset.common.metadata;
                Some(PresetFields {
                    name: preset.name().to_string(),
                    product: preset.common.product_name.clone().unwrap_or_default(),
                    file_ext,
                    has_preview,
                    context_name: preset.common.context_name.clone().unwrap_or_default(),
                    path,
                    preview_path,
                    database,
                    vendor: m.vendor.clone().unwrap_or_default(),
                    author: m.author.clone().unwrap_or_default(),
                    comment: m.comment.clone().unwrap_or_default(),
                    date: m.modification_date.map(|d| d.to_string()).unwrap_or_default(),
                    file_size: m.file_size_in_bytes.map(human_size).unwrap_or_default(),
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

extern "C" fn HB_Pot_GetPresetMetadata(
    index: c_int,
    field: *const c_char,
    buf: *mut c_char,
    buf_sz: c_int,
) -> c_int {
    if field.is_null() {
        return 0;
    }
    let field = unsafe { CStr::from_ptr(field) };
    with_pot_unit(|_, unit| {
        let Some(f) = preset_fields_at(unit, index) else {
            return 0;
        };
        let value = match field.to_str().unwrap_or("") {
            "vendor" => &f.vendor,
            "author" => &f.author,
            "comment" => &f.comment,
            "date" => &f.date,
            "database" => &f.database,
            "filesize" => &f.file_size,
            "context" => &f.context_name,
            _ => return 0,
        };
        unsafe { copy_to_buf(value, buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetPresetMetadata(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    let field = str_arg(args, n, 1).map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    ret_int(HB_Pot_GetPresetMetadata(
        int_arg(args, n, 0),
        field,
        buf_arg(args, n, 2),
        int_arg(args, n, 3),
    ))
}

extern "C" fn HB_Pot_IsPresetFavorite(index: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Some(id) = u32::try_from(index)
            .ok()
            .and_then(|i| unit.find_preset_id_at_index(i))
        else {
            return 0;
        };
        let favs = crate::standalone_unit::favorites()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        favs.is_favorite(id) as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_IsPresetFavorite(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_IsPresetFavorite(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_TogglePresetFavorite(index: c_int) {
    with_pot_unit(|shared, unit| {
        let Some(id) = u32::try_from(index)
            .ok()
            .and_then(|i| unit.find_preset_id_at_index(i))
        else {
            return;
        };
        {
            let mut favs = crate::standalone_unit::favorites()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            favs.toggle_favorite(id);
        }
        // If the list is filtered by favorites, its membership just changed, so rebuild
        // it (collections only — no database re-scan).
        if unit.get_filter(PotFilterKind::IsFavorite).is_some() {
            unit.rebuild_collections(
                shared.clone(),
                ChangeHint::Filter(PotFilterKind::IsFavorite),
                Debounce::No,
            );
        }
    });
}
unsafe extern "C" fn vararg_HB_Pot_TogglePresetFavorite(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    HB_Pot_TogglePresetFavorite(int_arg(args, n, 0));
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Destination panel
// ---------------------------------------------------------------------------
//
// Destination track is encoded as a single int: -2 = selected track, -1 = master track,
// >= 0 = specific track index.

use pot::DestinationTrackDescriptor as Dtd;

extern "C" fn HB_Pot_GetDestinationTrack() -> c_int {
    with_pot_unit(|_, unit| match unit.destination_descriptor.track {
        Dtd::SelectedTrack => -2,
        Dtd::MasterTrack => -1,
        Dtd::Track(i) => i as c_int,
    })
    .unwrap_or(-2)
}
unsafe extern "C" fn vararg_HB_Pot_GetDestinationTrack(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetDestinationTrack())
}

extern "C" fn HB_Pot_SetDestinationTrack(value: c_int) {
    with_pot_unit(|_, unit| {
        unit.destination_descriptor.track = match value {
            -2 => Dtd::SelectedTrack,
            -1 => Dtd::MasterTrack,
            i if i >= 0 => Dtd::Track(i as u32),
            _ => Dtd::SelectedTrack,
        };
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetDestinationTrack(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetDestinationTrack(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_GetDestinationFxIndex() -> c_int {
    with_pot_unit(|_, unit| unit.destination_descriptor.fx_index as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetDestinationFxIndex(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetDestinationFxIndex())
}

extern "C" fn HB_Pot_SetDestinationFxIndex(index: c_int) {
    with_pot_unit(|_, unit| {
        if index >= 0 {
            unit.destination_descriptor.fx_index = index as u32;
        }
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetDestinationFxIndex(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetDestinationFxIndex(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_GetTrackCount() -> c_int {
    reaper_low::firewall(|| Reaper::get().current_project().track_count() as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetTrackCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetTrackCount())
}

extern "C" fn HB_Pot_GetTrackName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    reaper_low::firewall(|| {
        let Some(i) = u32::try_from(index).ok() else {
            return 0;
        };
        let Some(track) = Reaper::get().current_project().track_by_index(i) else {
            return 0;
        };
        let name = track
            .name()
            .map(|n| n.into_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Track {}", i + 1));
        unsafe { copy_to_buf(&format!("{}. {}", i + 1, name), buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetTrackName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetTrackName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

/// Number of FX in the resolved destination track's chain (0 if it can't be resolved).
extern "C" fn HB_Pot_GetDestinationFxCount() -> c_int {
    with_pot_unit(|_, unit| {
        unit.destination_descriptor
            .track
            .resolve(Reaper::get().current_project())
            .map(|t| t.normal_fx_chain().fx_count() as c_int)
            .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetDestinationFxCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetDestinationFxCount())
}

extern "C" fn HB_Pot_GetDestinationFxName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    with_pot_unit(|_, unit| {
        let Ok(track) = unit
            .destination_descriptor
            .track
            .resolve(Reaper::get().current_project())
        else {
            return 0;
        };
        let Some(i) = u32::try_from(index).ok() else {
            return 0;
        };
        let Some(fx) = track.normal_fx_chain().fx_by_index(i) else {
            return 0;
        };
        unsafe { copy_to_buf(&fx.name().into_string(), buf, buf_sz) as c_int }
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetDestinationFxName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetDestinationFxName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

fn with_destination_fx(unit: &RuntimePotUnit, f: impl FnOnce(&reaper_high::Fx)) {
    if let Some(fx) = unit
        .resolve_destination()
        .ok()
        .and_then(|inst| inst.get_existing().and_then(|dest| dest.resolve()))
    {
        f(&fx);
    }
}

extern "C" fn HB_Pot_ShowDestinationFx() {
    with_pot_unit(|_, unit| {
        with_destination_fx(unit, |fx| {
            let _ = fx.show_in_floating_window();
        })
    });
}
unsafe extern "C" fn vararg_HB_Pot_ShowDestinationFx(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_ShowDestinationFx();
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_ShowDestinationChain() {
    with_pot_unit(|_, unit| {
        with_destination_fx(unit, |fx| {
            let _ = fx.show_in_chain();
        })
    });
}
unsafe extern "C" fn vararg_HB_Pot_ShowDestinationChain(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_ShowDestinationChain();
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Load options
// ---------------------------------------------------------------------------

extern "C" fn HB_Pot_GetLoadWindowBehaviorCount() -> c_int {
    use strum::IntoEnumIterator;
    pot::LoadPresetWindowBehavior::iter().count() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_GetLoadWindowBehaviorCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetLoadWindowBehaviorCount())
}

extern "C" fn HB_Pot_GetLoadWindowBehaviorName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    use strum::IntoEnumIterator;
    let Some(b) = usize::try_from(index)
        .ok()
        .and_then(|i| pot::LoadPresetWindowBehavior::iter().nth(i))
    else {
        return 0;
    };
    unsafe { copy_to_buf(b.as_ref(), buf, buf_sz) as c_int }
}
unsafe extern "C" fn vararg_HB_Pot_GetLoadWindowBehaviorName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetLoadWindowBehaviorName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetLoadWindowBehavior() -> c_int {
    use strum::IntoEnumIterator;
    with_pot_unit(|_, unit| {
        let current = unit.default_load_preset_window_behavior;
        pot::LoadPresetWindowBehavior::iter()
            .position(|b| b == current)
            .map(|i| i as c_int)
            .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetLoadWindowBehavior(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetLoadWindowBehavior())
}

extern "C" fn HB_Pot_SetLoadWindowBehavior(index: c_int) {
    use strum::IntoEnumIterator;
    with_pot_unit(|_, unit| {
        if let Some(b) = usize::try_from(index)
            .ok()
            .and_then(|i| pot::LoadPresetWindowBehavior::iter().nth(i))
        {
            unit.default_load_preset_window_behavior = b;
        }
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetLoadWindowBehavior(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetLoadWindowBehavior(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_GetNameTrackAfterPreset() -> c_int {
    with_pot_unit(|_, unit| unit.name_track_after_preset as c_int).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetNameTrackAfterPreset(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetNameTrackAfterPreset())
}

extern "C" fn HB_Pot_SetNameTrackAfterPreset(on: c_int) {
    with_pot_unit(|_, unit| unit.name_track_after_preset = on != 0);
}
unsafe extern "C" fn vararg_HB_Pot_SetNameTrackAfterPreset(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetNameTrackAfterPreset(int_arg(args, n, 0));
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Macro-parameter panel
// ---------------------------------------------------------------------------
//
// Shows the macro parameters of the preset that was loaded into the current destination
// FX. The preset's macro→FX-parameter mapping comes from the stored CurrentPreset; the
// live values are read/written on the resolved destination FX.

/// Runs `f` with the destination FX and the current preset loaded into it, if any.
fn with_macro_preset<R>(
    unit: &RuntimePotUnit,
    f: impl FnOnce(&Fx, &CurrentPreset) -> R,
) -> Option<R> {
    let fx = unit
        .resolve_destination()
        .ok()?
        .get_existing()
        .and_then(|d| d.resolve())?;
    crate::standalone_unit::with_current_preset(&fx, |cp| cp.map(|cp| f(&fx, cp)))
}

/// Resolves the live FX parameter behind a macro slot, if it exists in the actual plug-in.
fn macro_fx_param(fx: &Fx, cp: &CurrentPreset, bank: u32, slot: c_int) -> Option<FxParameter> {
    let slot = usize::try_from(slot).ok()?;
    let bank = cp.find_macro_param_bank_at(bank)?;
    let macro_param = bank.params().get(slot)?;
    let index = macro_param.fx_param?.resolved_param_index?;
    let param = fx.parameter_by_index(index);
    if param.is_available() {
        Some(param)
    } else {
        None
    }
}

extern "C" fn HB_Pot_GetMacroBankCount() -> c_int {
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |_, cp| cp.macro_param_bank_count() as c_int).unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroBankCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroBankCount())
}

extern "C" fn HB_Pot_GetMacroBankName(bank: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |_, cp| {
            let Some(b) = cp.find_macro_param_bank_at(bank) else {
                return 0;
            };
            unsafe { copy_to_buf(&b.name(), buf, buf_sz) as c_int }
        })
        .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroBankName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroBankName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_GetMacroParamCount(bank: c_int) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |_, cp| {
            cp.find_macro_param_bank_at(bank)
                .map(|b| b.param_count() as c_int)
                .unwrap_or(0)
        })
        .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroParamCount(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroParamCount(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_GetMacroParamName(
    bank: c_int,
    slot: c_int,
    buf: *mut c_char,
    buf_sz: c_int,
) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return 0;
    };
    let Some(slot) = usize::try_from(slot).ok() else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |_, cp| {
            let Some(p) = cp.find_macro_param_bank_at(bank).and_then(|b| b.params().get(slot)) else {
                return 0;
            };
            unsafe { copy_to_buf(&p.name, buf, buf_sz) as c_int }
        })
        .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroParamName(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroParamName(
        int_arg(args, n, 0),
        int_arg(args, n, 1),
        buf_arg(args, n, 2),
        int_arg(args, n, 3),
    ))
}

extern "C" fn HB_Pot_GetMacroParamSection(
    bank: c_int,
    slot: c_int,
    buf: *mut c_char,
    buf_sz: c_int,
) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return 0;
    };
    let Some(slot) = usize::try_from(slot).ok() else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |_, cp| {
            let Some(p) = cp.find_macro_param_bank_at(bank).and_then(|b| b.params().get(slot)) else {
                return 0;
            };
            let section = p.section.as_deref().unwrap_or("");
            unsafe { copy_to_buf(section, buf, buf_sz) as c_int }
        })
        .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroParamSection(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroParamSection(
        int_arg(args, n, 0),
        int_arg(args, n, 1),
        buf_arg(args, n, 2),
        int_arg(args, n, 3),
    ))
}

/// Returns the macro parameter's current value as permille (0-1000), or -1 if the slot
/// has no resolvable live FX parameter (i.e. nothing to control).
extern "C" fn HB_Pot_GetMacroParamValue(bank: c_int, slot: c_int) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return -1;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |fx, cp| {
            macro_fx_param(fx, cp, bank, slot)
                .map(|p| (p.reaper_normalized_value().get() * 1000.0).round() as c_int)
                .unwrap_or(-1)
        })
        .unwrap_or(-1)
    })
    .unwrap_or(-1)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroParamValue(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroParamValue(int_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_GetMacroParamValueLabel(
    bank: c_int,
    slot: c_int,
    buf: *mut c_char,
    buf_sz: c_int,
) -> c_int {
    let Some(bank) = u32::try_from(bank).ok() else {
        return 0;
    };
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |fx, cp| {
            let Some(p) = macro_fx_param(fx, cp, bank, slot) else {
                return 0;
            };
            let label = p
                .format_reaper_normalized_value(p.reaper_normalized_value())
                .map(|s| s.into_string())
                .unwrap_or_default();
            unsafe { copy_to_buf(&label, buf, buf_sz) as c_int }
        })
        .unwrap_or(0)
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetMacroParamValueLabel(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_GetMacroParamValueLabel(
        int_arg(args, n, 0),
        int_arg(args, n, 1),
        buf_arg(args, n, 2),
        int_arg(args, n, 3),
    ))
}

extern "C" fn HB_Pot_SetMacroParamValue(bank: c_int, slot: c_int, permille: c_int) {
    let Some(bank) = u32::try_from(bank).ok() else {
        return;
    };
    let value = (permille.clamp(0, 1000) as f64) / 1000.0;
    with_pot_unit(|_, unit| {
        with_macro_preset(unit, |fx, cp| {
            if let Some(p) = macro_fx_param(fx, cp, bank, slot) {
                let _ = p.set_reaper_normalized_value(ReaperNormalizedFxParamValue::new(value));
            }
        });
    });
}
unsafe extern "C" fn vararg_HB_Pot_SetMacroParamValue(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    HB_Pot_SetMacroParamValue(int_arg(args, n, 0), int_arg(args, n, 1), int_arg(args, n, 2));
    std::ptr::null_mut()
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

// ---------------------------------------------------------------------------
// Task pump
// ---------------------------------------------------------------------------

extern "C" fn HB_Pot_RunTasks() {
    crate::executor::run_tasks();
}
unsafe extern "C" fn vararg_HB_Pot_RunTasks(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_RunTasks();
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Focused FX (preset crawler precondition)
// ---------------------------------------------------------------------------

extern "C" fn HB_Pot_GetFocusedFxName(buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::crawler::with_focused_fx_name(|name| unsafe { copy_to_buf(name, buf, buf_sz) as c_int })
        .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_GetFocusedFxName(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_GetFocusedFxName(buf_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_IsFocusedFxOpenFloating() -> c_int {
    crate::crawler::focused_fx_is_floating() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_IsFocusedFxOpenFloating(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_IsFocusedFxOpenFloating())
}

// ---------------------------------------------------------------------------
// Preset crawler
// ---------------------------------------------------------------------------

extern "C" fn HB_Pot_CrawlerStart(
    stop_if_destination_exists: c_int,
    never_stop: c_int,
    use_save_as: c_int,
) -> c_int {
    crate::crawler::start(
        stop_if_destination_exists != 0,
        never_stop != 0,
        use_save_as != 0,
    )
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerStart(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_CrawlerStart(
        int_arg(args, n, 0),
        int_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_CrawlerRecordStart(target: c_int) {
    crate::crawler::record_start(target);
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerRecordStart(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    HB_Pot_CrawlerRecordStart(int_arg(args, n, 0));
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_CrawlerRecordStop() {
    crate::crawler::record_stop();
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerRecordStop(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    HB_Pot_CrawlerRecordStop();
    std::ptr::null_mut()
}

extern "C" fn HB_Pot_CrawlerIsRecording() -> c_int {
    crate::crawler::is_recording() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerIsRecording(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerIsRecording())
}

extern "C" fn HB_Pot_CrawlerRecordedCount(target: c_int) -> c_int {
    crate::crawler::recorded_count(target)
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerRecordedCount(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerRecordedCount(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_CrawlerPhase() -> c_int {
    crate::crawler::phase_code()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerPhase(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_CrawlerPhase())
}

extern "C" fn HB_Pot_CrawlerIsRunning() -> c_int {
    crate::crawler::is_running() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerIsRunning(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_CrawlerIsRunning())
}

extern "C" fn HB_Pot_CrawlerPresetCount() -> c_int {
    crate::crawler::preset_count()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerPresetCount(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerPresetCount())
}

extern "C" fn HB_Pot_CrawlerDuplicateCount() -> c_int {
    crate::crawler::duplicate_count()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerDuplicateCount(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerDuplicateCount())
}

extern "C" fn HB_Pot_CrawlerCrawledCount() -> c_int {
    crate::crawler::crawled_count()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerCrawledCount(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerCrawledCount())
}

extern "C" fn HB_Pot_CrawlerLastPresetName(buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::crawler::with_last_preset_name(|name| unsafe { copy_to_buf(name, buf, buf_sz) as c_int })
        .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerLastPresetName(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerLastPresetName(
        buf_arg(args, n, 0),
        int_arg(args, n, 1),
    ))
}

extern "C" fn HB_Pot_CrawlerStopReason() -> c_int {
    crate::crawler::stop_reason_code()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerStopReason(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerStopReason())
}

extern "C" fn HB_Pot_CrawlerStopReasonLabel(buf: *mut c_char, buf_sz: c_int) -> c_int {
    unsafe { copy_to_buf(crate::crawler::stop_reason_label(), buf, buf_sz) as c_int }
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerStopReasonLabel(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_CrawlerStopReasonLabel(
        buf_arg(args, n, 0),
        int_arg(args, n, 1),
    ))
}

extern "C" fn HB_Pot_CrawlerError(buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::crawler::with_error(|e| unsafe { copy_to_buf(e, buf, buf_sz) as c_int }).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerError(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_CrawlerError(buf_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_CrawlerImport() -> c_int {
    crate::crawler::import()
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerImport(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_CrawlerImport())
}

extern "C" fn HB_Pot_CrawlerDiscard() {
    crate::crawler::discard();
}
unsafe extern "C" fn vararg_HB_Pot_CrawlerDiscard(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_CrawlerDiscard();
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Preview recorder
// ---------------------------------------------------------------------------

extern "C" fn HB_Pot_RecorderPrepare(mode: c_int) -> c_int {
    crate::recorder::prepare(mode)
}
unsafe extern "C" fn vararg_HB_Pot_RecorderPrepare(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderPrepare(int_arg(args, n, 0)))
}

extern "C" fn HB_Pot_RecorderPhase() -> c_int {
    crate::recorder::phase_code()
}
unsafe extern "C" fn vararg_HB_Pot_RecorderPhase(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderPhase())
}

extern "C" fn HB_Pot_RecorderIsRunning() -> c_int {
    crate::recorder::is_running() as c_int
}
unsafe extern "C" fn vararg_HB_Pot_RecorderIsRunning(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderIsRunning())
}

extern "C" fn HB_Pot_RecorderPreparedCount() -> c_int {
    crate::recorder::prepared_count()
}
unsafe extern "C" fn vararg_HB_Pot_RecorderPreparedCount(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_RecorderPreparedCount())
}

extern "C" fn HB_Pot_RecorderStart() -> c_int {
    crate::recorder::start()
}
unsafe extern "C" fn vararg_HB_Pot_RecorderStart(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderStart())
}

extern "C" fn HB_Pot_RecorderTodoCount() -> c_int {
    crate::recorder::todo_count()
}
unsafe extern "C" fn vararg_HB_Pot_RecorderTodoCount(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderTodoCount())
}

extern "C" fn HB_Pot_RecorderFailureCount() -> c_int {
    crate::recorder::failure_count()
}
unsafe extern "C" fn vararg_HB_Pot_RecorderFailureCount(
    _: *mut *mut c_void,
    _: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_RecorderFailureCount())
}

extern "C" fn HB_Pot_RecorderFailureName(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::recorder::with_failure_name(index, |name| unsafe {
        copy_to_buf(name, buf, buf_sz) as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_RecorderFailureName(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_RecorderFailureName(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_RecorderFailureReason(index: c_int, buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::recorder::with_failure_reason(index, |reason| unsafe {
        copy_to_buf(reason, buf, buf_sz) as c_int
    })
    .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_RecorderFailureReason(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_RecorderFailureReason(
        int_arg(args, n, 0),
        buf_arg(args, n, 1),
        int_arg(args, n, 2),
    ))
}

extern "C" fn HB_Pot_RecorderExportDir(buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::recorder::with_export_dir(|dir| unsafe { copy_to_buf(dir, buf, buf_sz) as c_int })
        .unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_RecorderExportDir(
    args: *mut *mut c_void,
    n: c_int,
) -> *mut c_void {
    ret_int(HB_Pot_RecorderExportDir(buf_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_RecorderError(buf: *mut c_char, buf_sz: c_int) -> c_int {
    crate::recorder::with_error(|e| unsafe { copy_to_buf(e, buf, buf_sz) as c_int }).unwrap_or(0)
}
unsafe extern "C" fn vararg_HB_Pot_RecorderError(args: *mut *mut c_void, n: c_int) -> *mut c_void {
    ret_int(HB_Pot_RecorderError(buf_arg(args, n, 0), int_arg(args, n, 1)))
}

extern "C" fn HB_Pot_RecorderDiscard() {
    crate::recorder::discard();
}
unsafe extern "C" fn vararg_HB_Pot_RecorderDiscard(_: *mut *mut c_void, _: c_int) -> *mut c_void {
    HB_Pot_RecorderDiscard();
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
    (HB_Pot_GetPresetMetadata) => { vararg_HB_Pot_GetPresetMetadata };
    (HB_Pot_IsPresetFavorite) => { vararg_HB_Pot_IsPresetFavorite };
    (HB_Pot_TogglePresetFavorite) => { vararg_HB_Pot_TogglePresetFavorite };
    (HB_Pot_GetDestinationTrack) => { vararg_HB_Pot_GetDestinationTrack };
    (HB_Pot_SetDestinationTrack) => { vararg_HB_Pot_SetDestinationTrack };
    (HB_Pot_GetDestinationFxIndex) => { vararg_HB_Pot_GetDestinationFxIndex };
    (HB_Pot_SetDestinationFxIndex) => { vararg_HB_Pot_SetDestinationFxIndex };
    (HB_Pot_GetTrackCount) => { vararg_HB_Pot_GetTrackCount };
    (HB_Pot_GetTrackName) => { vararg_HB_Pot_GetTrackName };
    (HB_Pot_GetDestinationFxCount) => { vararg_HB_Pot_GetDestinationFxCount };
    (HB_Pot_GetDestinationFxName) => { vararg_HB_Pot_GetDestinationFxName };
    (HB_Pot_ShowDestinationFx) => { vararg_HB_Pot_ShowDestinationFx };
    (HB_Pot_ShowDestinationChain) => { vararg_HB_Pot_ShowDestinationChain };
    (HB_Pot_GetLoadWindowBehaviorCount) => { vararg_HB_Pot_GetLoadWindowBehaviorCount };
    (HB_Pot_GetLoadWindowBehaviorName) => { vararg_HB_Pot_GetLoadWindowBehaviorName };
    (HB_Pot_GetLoadWindowBehavior) => { vararg_HB_Pot_GetLoadWindowBehavior };
    (HB_Pot_SetLoadWindowBehavior) => { vararg_HB_Pot_SetLoadWindowBehavior };
    (HB_Pot_GetNameTrackAfterPreset) => { vararg_HB_Pot_GetNameTrackAfterPreset };
    (HB_Pot_SetNameTrackAfterPreset) => { vararg_HB_Pot_SetNameTrackAfterPreset };
    (HB_Pot_GetMacroBankCount) => { vararg_HB_Pot_GetMacroBankCount };
    (HB_Pot_GetMacroBankName) => { vararg_HB_Pot_GetMacroBankName };
    (HB_Pot_GetMacroParamCount) => { vararg_HB_Pot_GetMacroParamCount };
    (HB_Pot_GetMacroParamName) => { vararg_HB_Pot_GetMacroParamName };
    (HB_Pot_GetMacroParamSection) => { vararg_HB_Pot_GetMacroParamSection };
    (HB_Pot_GetMacroParamValue) => { vararg_HB_Pot_GetMacroParamValue };
    (HB_Pot_GetMacroParamValueLabel) => { vararg_HB_Pot_GetMacroParamValueLabel };
    (HB_Pot_SetMacroParamValue) => { vararg_HB_Pot_SetMacroParamValue };
    (HB_Pot_RunTasks) => { vararg_HB_Pot_RunTasks };
    (HB_Pot_GetFocusedFxName) => { vararg_HB_Pot_GetFocusedFxName };
    (HB_Pot_IsFocusedFxOpenFloating) => { vararg_HB_Pot_IsFocusedFxOpenFloating };
    (HB_Pot_CrawlerStart) => { vararg_HB_Pot_CrawlerStart };
    (HB_Pot_CrawlerPhase) => { vararg_HB_Pot_CrawlerPhase };
    (HB_Pot_CrawlerIsRunning) => { vararg_HB_Pot_CrawlerIsRunning };
    (HB_Pot_CrawlerPresetCount) => { vararg_HB_Pot_CrawlerPresetCount };
    (HB_Pot_CrawlerDuplicateCount) => { vararg_HB_Pot_CrawlerDuplicateCount };
    (HB_Pot_CrawlerCrawledCount) => { vararg_HB_Pot_CrawlerCrawledCount };
    (HB_Pot_CrawlerLastPresetName) => { vararg_HB_Pot_CrawlerLastPresetName };
    (HB_Pot_CrawlerStopReason) => { vararg_HB_Pot_CrawlerStopReason };
    (HB_Pot_CrawlerStopReasonLabel) => { vararg_HB_Pot_CrawlerStopReasonLabel };
    (HB_Pot_CrawlerError) => { vararg_HB_Pot_CrawlerError };
    (HB_Pot_CrawlerImport) => { vararg_HB_Pot_CrawlerImport };
    (HB_Pot_CrawlerDiscard) => { vararg_HB_Pot_CrawlerDiscard };
    (HB_Pot_CrawlerRecordStart) => { vararg_HB_Pot_CrawlerRecordStart };
    (HB_Pot_CrawlerRecordStop) => { vararg_HB_Pot_CrawlerRecordStop };
    (HB_Pot_CrawlerIsRecording) => { vararg_HB_Pot_CrawlerIsRecording };
    (HB_Pot_CrawlerRecordedCount) => { vararg_HB_Pot_CrawlerRecordedCount };
    (HB_Pot_RecorderPrepare) => { vararg_HB_Pot_RecorderPrepare };
    (HB_Pot_RecorderPhase) => { vararg_HB_Pot_RecorderPhase };
    (HB_Pot_RecorderIsRunning) => { vararg_HB_Pot_RecorderIsRunning };
    (HB_Pot_RecorderPreparedCount) => { vararg_HB_Pot_RecorderPreparedCount };
    (HB_Pot_RecorderStart) => { vararg_HB_Pot_RecorderStart };
    (HB_Pot_RecorderTodoCount) => { vararg_HB_Pot_RecorderTodoCount };
    (HB_Pot_RecorderFailureCount) => { vararg_HB_Pot_RecorderFailureCount };
    (HB_Pot_RecorderFailureName) => { vararg_HB_Pot_RecorderFailureName };
    (HB_Pot_RecorderFailureReason) => { vararg_HB_Pot_RecorderFailureReason };
    (HB_Pot_RecorderExportDir) => { vararg_HB_Pot_RecorderExportDir };
    (HB_Pot_RecorderError) => { vararg_HB_Pot_RecorderError };
    (HB_Pot_RecorderDiscard) => { vararg_HB_Pot_RecorderDiscard };
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
        HB_Pot_GetPresetMetadata:
            b"int\0int,const char*,char*,int\0index,field,valueOut,valueOut_sz\0Gets a metadata field of the preset at the given index. field: vendor, author, comment, date, database, filesize, context. Returns 0 on failure.\0";
        HB_Pot_IsPresetFavorite:
            b"int\0int\0index\0Returns 1 if the preset at the given index is marked as a favorite.\0";
        HB_Pot_TogglePresetFavorite:
            b"void\0int\0index\0Toggles the favorite mark of the preset at the given index.\0";
        HB_Pot_GetDestinationTrack:
            b"int\0\0\0Returns the destination track: -2 = selected track, -1 = master track, >=0 = specific track index.\0";
        HB_Pot_SetDestinationTrack:
            b"void\0int\0value\0Sets the destination track (-2 = selected, -1 = master, >=0 = specific track index).\0";
        HB_Pot_GetDestinationFxIndex:
            b"int\0\0\0Returns the destination FX slot index in the destination track's chain.\0";
        HB_Pot_SetDestinationFxIndex:
            b"void\0int\0index\0Sets the destination FX slot index.\0";
        HB_Pot_GetTrackCount:
            b"int\0\0\0Returns the number of tracks in the current project.\0";
        HB_Pot_GetTrackName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets a display label for the track at the given index. Returns 0 on failure.\0";
        HB_Pot_GetDestinationFxCount:
            b"int\0\0\0Returns the number of FX in the resolved destination track's chain (0 if unresolved).\0";
        HB_Pot_GetDestinationFxName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets the name of the FX at the given index in the destination chain. Returns 0 on failure.\0";
        HB_Pot_ShowDestinationFx:
            b"void\0\0\0Shows the currently targeted destination FX in a floating window.\0";
        HB_Pot_ShowDestinationChain:
            b"void\0\0\0Shows the destination track's FX chain.\0";
        HB_Pot_GetLoadWindowBehaviorCount:
            b"int\0\0\0Returns the number of available FX-window-behavior options for preset loading.\0";
        HB_Pot_GetLoadWindowBehaviorName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets the name of the FX-window-behavior option at the given index.\0";
        HB_Pot_GetLoadWindowBehavior:
            b"int\0\0\0Returns the index of the current FX-window-behavior option.\0";
        HB_Pot_SetLoadWindowBehavior:
            b"void\0int\0index\0Sets the FX-window-behavior option used when loading presets.\0";
        HB_Pot_GetNameTrackAfterPreset:
            b"int\0\0\0Returns 1 if the destination track is renamed after the loaded preset.\0";
        HB_Pot_SetNameTrackAfterPreset:
            b"void\0int\0on\0Enables or disables renaming the destination track after the loaded preset.\0";
        HB_Pot_GetMacroBankCount:
            b"int\0\0\0Returns the number of macro-parameter banks of the preset loaded into the destination FX (0 if none).\0";
        HB_Pot_GetMacroBankName:
            b"int\0int,char*,int\0bank,nameOut,nameOut_sz\0Gets the name of the given macro-parameter bank. Returns 0 on failure.\0";
        HB_Pot_GetMacroParamCount:
            b"int\0int\0bank\0Returns the number of parameter slots in the given macro-parameter bank.\0";
        HB_Pot_GetMacroParamName:
            b"int\0int,int,char*,int\0bank,slot,nameOut,nameOut_sz\0Gets the macro name of the given bank/slot. Returns 0 on failure.\0";
        HB_Pot_GetMacroParamSection:
            b"int\0int,int,char*,int\0bank,slot,sectionOut,sectionOut_sz\0Gets the section label of the given bank/slot (may be empty). Returns 0 on failure.\0";
        HB_Pot_GetMacroParamValue:
            b"int\0int,int\0bank,slot\0Returns the macro slot's current value as permille (0-1000), or -1 if it has no live FX parameter.\0";
        HB_Pot_GetMacroParamValueLabel:
            b"int\0int,int,char*,int\0bank,slot,labelOut,labelOut_sz\0Gets the plug-in-formatted current value of the given macro slot. Returns 0 on failure.\0";
        HB_Pot_SetMacroParamValue:
            b"void\0int,int,int\0bank,slot,permille\0Sets the given macro slot's live FX parameter to the given permille value (0-1000).\0";
        HB_Pot_RunTasks:
            b"void\0\0\0Pumps the standalone extension's async task executor. Call once per frame from the script's defer loop so the crawler/recorder wizards make progress.\0";
        HB_Pot_GetFocusedFxName:
            b"int\0char*,int\0nameOut,nameOut_sz\0Gets the name of the currently focused FX, or 0 if none is focused.\0";
        HB_Pot_IsFocusedFxOpenFloating:
            b"int\0\0\0Returns 1 if there is a focused FX and it is open in a floating window (a precondition for the preset crawler).\0";
        HB_Pot_CrawlerStart:
            b"int\0int,int,int\0stop_if_destination_exists,never_stop,use_save_as\0Starts crawling the focused FX (must be open in a floating window). Requires a recorded Next-preset macro (HB_Pot_CrawlerRecordStart(0)/Stop). If use_save_as is 1, preset names are scraped by replaying the recorded save-as macro (record with target 1); otherwise names come from the host API. Returns 1 if started, 0 otherwise (e.g. missing recorded macro).\0";
        HB_Pot_CrawlerRecordStart:
            b"void\0int\0target\0Starts recording an action sequence (clicks + keystrokes) on a background thread. target: 0 = the \"Next preset\" click, 1 = the \"Save Preset As\" name-grab. Demonstrate it on the plug-in, then press Escape (or call HB_Pot_CrawlerRecordStop) to finish.\0";
        HB_Pot_CrawlerRecordStop:
            b"void\0\0\0Stops the current action recording and stores it for the next crawl.\0";
        HB_Pot_CrawlerIsRecording:
            b"int\0\0\0Returns 1 while an action recording is in progress (0 once Escape is pressed or it is stopped).\0";
        HB_Pot_CrawlerRecordedCount:
            b"int\0int\0target\0Returns the number of recorded actions for the given target (0 = next-preset, 1 = save-as): the live count while recording, otherwise the stored macro's length.\0";
        HB_Pot_CrawlerPhase:
            b"int\0\0\0Returns the crawler phase: 0=idle, 1=crawling, 2=stopped (ready to import/discard), 3=importing, 4=done, 5=failed.\0";
        HB_Pot_CrawlerIsRunning:
            b"int\0\0\0Returns 1 while a crawl or import is in progress.\0";
        HB_Pot_CrawlerPresetCount:
            b"int\0\0\0Returns the number of presets crawled so far.\0";
        HB_Pot_CrawlerDuplicateCount:
            b"int\0\0\0Returns the number of presets skipped so far because of a duplicate name.\0";
        HB_Pot_CrawlerCrawledCount:
            b"int\0\0\0Returns the number of distinct presets crawled, as captured when crawling stopped.\0";
        HB_Pot_CrawlerLastPresetName:
            b"int\0char*,int\0nameOut,nameOut_sz\0Gets the name of the most recently crawled preset. Returns 0 if none yet.\0";
        HB_Pot_CrawlerStopReason:
            b"int\0\0\0Returns the stop reason once crawling stopped: -1=none yet, 0=cancelled, 1=destination file exists, 2=preset name not changing, 3=wrapped to beginning.\0";
        HB_Pot_CrawlerStopReasonLabel:
            b"int\0char*,int\0labelOut,labelOut_sz\0Gets a human-readable explanation of the stop reason (empty if not stopped).\0";
        HB_Pot_CrawlerError:
            b"int\0char*,int\0errorOut,errorOut_sz\0Gets the last crawler error message. Returns 0 if there was none.\0";
        HB_Pot_CrawlerImport:
            b"int\0\0\0Imports the crawled presets to disk and refreshes the database. Returns 1 if the import was started, 0 if there's nothing to import.\0";
        HB_Pot_CrawlerDiscard:
            b"void\0\0\0Discards the current crawl session (drops the results and closes the temp file).\0";
        HB_Pot_RecorderPrepare:
            b"int\0int\0mode\0Gathers the presets to record. mode: 0=record for Pot Browser playback (only presets without a preview), 1=export to a folder. Returns 1 if preparation started. Poll HB_Pot_RecorderPhase for completion.\0";
        HB_Pot_RecorderPhase:
            b"int\0\0\0Returns the recorder phase: 0=idle, 1=preparing, 2=ready (prepared, not started), 3=recording, 4=done, 5=failed.\0";
        HB_Pot_RecorderIsRunning:
            b"int\0\0\0Returns 1 while preparing or recording.\0";
        HB_Pot_RecorderPreparedCount:
            b"int\0\0\0Returns the number of presets gathered by the last prepare step.\0";
        HB_Pot_RecorderStart:
            b"int\0\0\0Starts recording the prepared presets. Returns 1 if started, 0 otherwise (e.g. export template needs review first - see HB_Pot_RecorderError).\0";
        HB_Pot_RecorderTodoCount:
            b"int\0\0\0Returns the number of presets still left to record, or -1 if not recording.\0";
        HB_Pot_RecorderFailureCount:
            b"int\0\0\0Returns the number of presets that failed to record.\0";
        HB_Pot_RecorderFailureName:
            b"int\0int,char*,int\0index,nameOut,nameOut_sz\0Gets the preset name of the given failure. Returns 0 on failure.\0";
        HB_Pot_RecorderFailureReason:
            b"int\0int,char*,int\0index,reasonOut,reasonOut_sz\0Gets the reason of the given failure. Returns 0 on failure.\0";
        HB_Pot_RecorderExportDir:
            b"int\0char*,int\0dirOut,dirOut_sz\0Gets the export directory (export mode only). Returns 0 if not an export session.\0";
        HB_Pot_RecorderError:
            b"int\0char*,int\0errorOut,errorOut_sz\0Gets the last recorder error message. Returns 0 if there was none.\0";
        HB_Pot_RecorderDiscard:
            b"void\0\0\0Discards the current recorder session.\0";
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
