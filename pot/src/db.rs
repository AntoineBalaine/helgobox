//! Pot's owner of the SQLite scan cache + preview registry (see `POT_DB_DESIGN.md`).
//!
//! This module holds the live connection (a process-global behind a `Mutex`) and exposes
//! **best-effort** wrappers around the `pot-db` crate: every function fails soft, returning
//! `None`/`false`/empty on any error so the engine simply falls back to its pre-cache
//! behaviour (re-reading files, probing the filesystem). The DB is an optimisation, never a
//! correctness dependency.
//!
//! It also owns the cache **DTOs**: stable, serde-friendly mirrors of the providers' private
//! preset entry types. We do not derive serde on the live types because `plugin_cores` is an
//! `IndexMap<PluginId, PluginCore>` and `PluginId` is an enum, which cannot be a JSON map
//! key. The DTOs represent plugins as a sequence and the 128-bit hash as a hex string, so
//! the on-disk format is explicit and decoupled from internal refactors.

use crate::api::ProductId;
use crate::plugin_id::PluginId;
use crate::plugins::{PluginCore, ProductKind};
use base::hash_util::{NonCryptoIndexMap, PersistentHash};
use reaper_high::Reaper;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub use pot_db::PreviewRow;

// ---------------------------------------------------------------------------------------
// Connection ownership (process-global, best-effort)
// ---------------------------------------------------------------------------------------

static POT_DB: OnceLock<Option<Mutex<rusqlite::Connection>>> = OnceLock::new();

fn cell() -> Option<&'static Mutex<rusqlite::Connection>> {
    POT_DB
        .get_or_init(|| {
            let dir = Reaper::get().resource_path().join("Helgoboss/Pot");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!("pot DB dir not creatable, cache disabled: {e}");
                return None;
            }
            let db_path = dir.join("pot.db");
            match pot_db::open(db_path.as_std_path()) {
                Ok(conn) => Some(Mutex::new(conn)),
                Err(e) => {
                    tracing::warn!("pot DB not openable, cache disabled: {e}");
                    None
                }
            }
        })
        .as_ref()
}

fn with_conn<R>(f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<R>) -> Option<R> {
    let mutex = cell()?;
    let guard = mutex.lock().ok()?;
    match f(&guard) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("pot DB operation failed: {e}");
            None
        }
    }
}

/// Monotonic-enough timestamp (epoch millis) marking one refresh pass, used to detect rows
/// not seen during the pass (i.e. files that disappeared).
pub fn pass_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `(mtime_secs, size_bytes)` for cache invalidation, or `None` if the file can't be stat'd.
pub fn file_stat(path: &Path) -> Option<(i64, i64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((mtime, md.len() as i64))
}

// ---- scan cache wrappers ----

pub fn cache_lookup(path: &str, mtime: i64, size: i64, pass: i64) -> Option<String> {
    with_conn(|c| pot_db::cache_lookup(c, path, mtime, size, pass)).flatten()
}

pub fn cache_store(path: &str, mtime: i64, size: i64, provider: &str, entries: &str, pass: i64) {
    with_conn(|c| pot_db::cache_store(c, path, mtime, size, provider, entries, pass));
}

pub fn cache_sweep(provider: &str, pass: i64) {
    with_conn(|c| pot_db::cache_sweep(c, provider, pass));
}

/// Read a small persisted setting from the `meta` key/value table (best-effort).
pub fn get_meta(key: &str) -> Option<String> {
    with_conn(|c| pot_db::meta_get(c, key)).flatten()
}

/// Persist a small setting into the `meta` key/value table (best-effort).
pub fn set_meta(key: &str, value: &str) {
    with_conn(|c| pot_db::meta_set(c, key, value));
}

/// If `token` differs from the last-stored value for `key`, drop the entire scan cache and
/// record the new token. Used to invalidate the cache when the installed plugin set changes
/// (cached entries store resolved plugins, which depend on the live plugin DB). Best-effort;
/// a one-shot full rescan on plugin changes is an acceptable, infrequent cost.
pub fn sync_generation(key: &str, token: &str) {
    with_conn(|c| {
        let previous = pot_db::meta_get(c, key)?;
        if previous.as_deref() != Some(token) {
            pot_db::cache_clear(c)?;
            pot_db::meta_set(c, key, token)?;
        }
        Ok(())
    });
}

// ---- preview registry wrappers ----

pub fn preview_exists(preview_hash: &str) -> bool {
    with_conn(|c| pot_db::preview_exists(c, preview_hash)).unwrap_or(false)
}

pub fn insert_preview(row: &PreviewRow) {
    with_conn(|c| pot_db::insert_preview(c, row));
}

pub fn all_previews() -> Vec<(String, String)> {
    with_conn(|c| pot_db::all_previews(c)).unwrap_or_default()
}

pub fn delete_preview(preview_hash: &str) {
    with_conn(|c| pot_db::delete_preview(c, preview_hash));
}

// ---------------------------------------------------------------------------------------
// Cache DTOs (stable on-disk representation of provider preset entries)
// ---------------------------------------------------------------------------------------

/// Serde mirror of [`PluginId`]. Stored as a tagged sequence element, never a map key.
#[derive(Serialize, Deserialize)]
pub(crate) enum CachedPluginId {
    Vst2(i32),
    Vst3([u32; 4]),
    Clap(String),
    Js(String),
}

impl CachedPluginId {
    fn from_id(id: &PluginId) -> Self {
        match id {
            PluginId::Vst2 { vst_magic_number } => Self::Vst2(*vst_magic_number),
            PluginId::Vst3 { vst_uid } => Self::Vst3(*vst_uid),
            PluginId::Clap { clap_id } => Self::Clap(clap_id.to_string()),
            PluginId::Js { js_id } => Self::Js(js_id.to_string()),
        }
    }

    fn to_id(&self) -> Option<PluginId> {
        let id = match self {
            Self::Vst2(n) => PluginId::vst2(*n),
            Self::Vst3(uid) => PluginId::vst3(*uid),
            Self::Clap(s) => PluginId::clap(s).ok()?,
            Self::Js(s) => PluginId::js(s).ok()?,
        };
        Some(id)
    }
}

/// Serde mirror of [`PluginCore`] (all three fields, so the in-memory map can be rebuilt
/// without re-reading the file).
#[derive(Serialize, Deserialize)]
pub(crate) struct CachedPluginCore {
    id: CachedPluginId,
    product_kind: Option<u8>,
    product_id: u32,
}

impl CachedPluginCore {
    fn from_core(core: &PluginCore) -> Self {
        Self {
            id: CachedPluginId::from_id(&core.id),
            product_kind: core.product_kind.map(product_kind_to_u8),
            product_id: core.product_id.0,
        }
    }

    fn to_core(&self) -> Option<PluginCore> {
        Some(PluginCore {
            id: self.id.to_id()?,
            product_kind: match self.product_kind {
                Some(n) => Some(product_kind_from_u8(n)?),
                None => None,
            },
            product_id: ProductId(self.product_id),
        })
    }
}

fn product_kind_to_u8(pk: ProductKind) -> u8 {
    match pk {
        ProductKind::Effect => 0,
        ProductKind::Instrument => 1,
        ProductKind::Loop => 2,
        ProductKind::OneShot => 3,
    }
}

fn product_kind_from_u8(n: u8) -> Option<ProductKind> {
    Some(match n {
        0 => ProductKind::Effect,
        1 => ProductKind::Instrument,
        2 => ProductKind::Loop,
        3 => ProductKind::OneShot,
        _ => return None,
    })
}

/// Convert a single live plugin core into its cache representation (for providers that carry
/// at most one plugin per preset, e.g. `ini`).
pub(crate) fn cached_core_from(core: &PluginCore) -> CachedPluginCore {
    CachedPluginCore::from_core(core)
}

/// Rebuild a single live plugin core from its cache representation. `None` on a malformed id.
pub(crate) fn cached_core_to(cached: &CachedPluginCore) -> Option<PluginCore> {
    cached.to_core()
}

/// Convert a live plugin-core map into its cache representation (a sequence).
pub(crate) fn cores_to_cached(map: &NonCryptoIndexMap<PluginId, PluginCore>) -> Vec<CachedPluginCore> {
    map.values().map(CachedPluginCore::from_core).collect()
}

/// Rebuild a live plugin-core map from the cache representation. Returns `None` if any entry
/// fails to reconstruct (e.g. a malformed cached id), forcing a re-parse of the file.
pub(crate) fn cores_from_cached(
    cached: &[CachedPluginCore],
) -> Option<NonCryptoIndexMap<PluginId, PluginCore>> {
    let mut map = NonCryptoIndexMap::default();
    for c in cached {
        let core = c.to_core()?;
        map.insert(core.id, core);
    }
    Some(map)
}

/// Serialise a 128-bit content hash as a zero-padded hex string (matches the preview
/// file-naming convention and round-trips losslessly, unlike u128-in-JSON).
pub(crate) fn hash_to_hex(h: PersistentHash) -> String {
    format!("{:032x}", h.get())
}

pub(crate) fn hash_from_hex(s: &str) -> Option<PersistentHash> {
    u128::from_str_radix(s, 16).ok().map(PersistentHash::from_raw)
}

/// Optional serde mirror of a content hash (some providers carry `Option<PersistentHash>`).
pub(crate) fn opt_hash_to_hex(h: Option<PersistentHash>) -> Option<String> {
    h.map(hash_to_hex)
}

pub(crate) fn opt_hash_from_hex(s: &Option<String>) -> Option<Option<PersistentHash>> {
    // Returns Some(None) when the cached value was absent, Some(Some(h)) on a good parse,
    // and None only when a present string fails to parse (cache miss → re-parse).
    match s {
        None => Some(None),
        Some(s) => hash_from_hex(s).map(Some),
    }
}
