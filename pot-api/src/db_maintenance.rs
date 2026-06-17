//! Standalone control surface over pot's preview-directory maintenance: orphan GC and
//! registry hygiene (see `POT_DB_DESIGN.md`).
//!
//! The orphan scan result is cached in a main-thread thread-local so the Lua UI can page
//! through it (count / name / size) and then delete a user-selected subset by path. The scan
//! runs synchronously on the calling (main) thread — the preview directory is bounded by the
//! number of recorded previews, not the (much larger) preset count, so this is cheap.

use pot::preview_maintenance::{self, OrphanPreview};
use std::cell::RefCell;

thread_local! {
    static ORPHANS: RefCell<Vec<OrphanPreview>> = const { RefCell::new(Vec::new()) };
}

/// Scan the preview directory for orphans, cache the result, and return the count.
pub fn scan_orphans() -> i32 {
    let orphans = preview_maintenance::scan_orphan_previews();
    let count = orphans.len() as i32;
    ORPHANS.with(|o| *o.borrow_mut() = orphans);
    count
}

/// Number of orphans found by the last [`scan_orphans`].
pub fn orphan_count() -> i32 {
    ORPHANS.with(|o| o.borrow().len() as i32)
}

/// Run `f` with the relative path of the orphan at `index`, or `None` if out of range.
pub fn with_orphan_name<R>(index: i32, f: impl FnOnce(&str) -> R) -> Option<R> {
    ORPHANS.with(|o| {
        let o = o.borrow();
        let item = o.get(usize::try_from(index).ok()?)?;
        Some(f(&item.rel_path))
    })
}

/// Size in bytes of the orphan at `index` (clamped to i32; previews are tiny), or -1.
pub fn orphan_size(index: i32) -> i32 {
    ORPHANS.with(|o| {
        o.borrow()
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .map(|item| item.size.min(i32::MAX as u64) as i32)
            .unwrap_or(-1)
    })
}

/// Delete a single orphan by its relative path (as returned by [`with_orphan_name`]). On
/// success it is also removed from the cached list. Returns 1 on success, 0 on failure.
pub fn delete_orphan(rel_path: &str) -> i32 {
    let (deleted, _failed) =
        preview_maintenance::delete_orphan_previews(std::slice::from_ref(&rel_path.to_string()));
    if deleted > 0 {
        ORPHANS.with(|o| o.borrow_mut().retain(|item| item.rel_path != rel_path));
        1
    } else {
        0
    }
}

/// Remove registry rows whose preview file no longer exists. Returns the number pruned.
pub fn prune_registry() -> i32 {
    preview_maintenance::prune_dangling_registry() as i32
}

/// Rebuild the registry from the preview files on disk (reads embedded Vorbis metadata).
/// Doubles as the one-time adoption pass for previews that predate the registry. Returns the
/// number of files registered.
pub fn rebuild_index() -> i32 {
    preview_maintenance::rebuild_registry_from_files() as i32
}
