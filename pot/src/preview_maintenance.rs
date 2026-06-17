//! Preview-directory maintenance: orphan detection/GC and registry hygiene.
//!
//! The safety rule (see `POT_DB_DESIGN.md`): a preview file is "owned" by pot only if it is
//! in the `previews` registry. An **orphan** is an `.ogg` in pot's own preview directory that
//! has no registry row. Orphans are never deleted automatically — [`scan_orphan_previews`]
//! only *reports* them; deletion happens via [`delete_orphan_previews`] after the user
//! explicitly confirms a specific list.

use reaper_high::Reaper;
use std::collections::HashSet;
use walkdir::WalkDir;

/// An orphaned preview file found in pot's preview directory.
pub struct OrphanPreview {
    /// Absolute path on disk.
    pub abs_path: String,
    /// Path relative to the preview root, with forward slashes (matches the registry form).
    pub rel_path: String,
    /// File size in bytes (0 if unreadable).
    pub size: u64,
}

fn preview_root() -> camino::Utf8PathBuf {
    Reaper::get()
        .resource_path()
        .join("Helgoboss/Pot/previews")
}

/// Registry file paths use forward slashes (`convert_hash_to_dir_structure`); on-disk relative
/// paths may use the platform separator. Normalise so the two compare equal on every OS.
fn normalize_rel(rel: &str) -> String {
    rel.replace('\\', "/")
}

/// Walk pot's preview directory and return every `.ogg` that is **not** in the registry.
/// Pure reporting — deletes nothing.
pub fn scan_orphan_previews() -> Vec<OrphanPreview> {
    let root = preview_root();
    let registered: HashSet<String> = crate::db::all_previews()
        .into_iter()
        .map(|(_, file_path)| normalize_rel(&file_path))
        .collect();
    let mut orphans = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ogg") {
            continue;
        }
        let Some(rel) = path
            .strip_prefix(root.as_std_path())
            .ok()
            .and_then(|p| p.to_str())
            .map(normalize_rel)
        else {
            continue;
        };
        if registered.contains(&rel) {
            continue;
        }
        orphans.push(OrphanPreview {
            abs_path: path.to_string_lossy().into_owned(),
            rel_path: rel,
            size: entry.metadata().map(|m| m.len()).unwrap_or(0),
        });
    }
    orphans
}

/// Delete the given preview files (paths relative to the preview root, as returned by
/// [`scan_orphan_previews`]). Returns `(deleted, failed)`. Only ever called with a
/// user-confirmed list; orphans carry no registry row, so nothing in the DB needs touching.
pub fn delete_orphan_previews(rel_paths: &[String]) -> (usize, usize) {
    let root = preview_root();
    let mut deleted = 0;
    let mut failed = 0;
    for rel in rel_paths {
        // Defensive: never escape the preview root (no absolute paths, no `..`).
        if rel.contains("..") || rel.starts_with('/') || rel.starts_with('\\') {
            failed += 1;
            continue;
        }
        let abs = root.join(rel);
        match std::fs::remove_file(&abs) {
            Ok(()) => deleted += 1,
            Err(_) => failed += 1,
        }
    }
    (deleted, failed)
}

/// Remove registry rows whose preview file no longer exists on disk (e.g. deleted outside
/// pot). Keeps the has-preview filter honest. Returns the number of rows pruned.
pub fn prune_dangling_registry() -> usize {
    let root = preview_root();
    let mut pruned = 0;
    for (preview_hash, file_path) in crate::db::all_previews() {
        if !root.join(&file_path).exists() {
            crate::db::delete_preview(&preview_hash);
            pruned += 1;
        }
    }
    pruned
}

#[derive(Default)]
struct PreviewMeta {
    name: String,
    product: String,
    vendor: String,
    database: String,
    persistent_id: String,
}

/// Reconstruct a preview-locating hash (32 hex) from a `aa/bb/<28hex>.ogg` relative path.
/// Inverse of `base::file_util::convert_hash_to_dir_structure`: the first two path bytes are
/// the top two bytes of the 128-bit hash, the 28-hex stem is the low 112 bits.
fn preview_hash_from_rel(rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let first_byte = u8::from_str_radix(parts[0], 16).ok()?;
    let second_byte = u8::from_str_radix(parts[1], 16).ok()?;
    let stem = parts[2].strip_suffix(".ogg")?;
    if stem.len() != 28 {
        return None;
    }
    let remaining = u128::from_str_radix(stem, 16).ok()?;
    let hash =
        ((first_byte as u128) << 120) | ((second_byte as u128) << 112) | remaining;
    Some(format!("{hash:032x}"))
}

/// Best-effort read of the preset identity embedded as Vorbis comments in a preview OGG.
fn read_preview_metadata(abs_path: &std::path::Path) -> PreviewMeta {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile;
    use lofty::ogg::VorbisFile;
    let mut meta = PreviewMeta::default();
    let Ok(mut file) = std::fs::File::open(abs_path) else {
        return meta;
    };
    let Ok(vorbis) = VorbisFile::read_from(&mut file, ParseOptions::new()) else {
        return meta;
    };
    let vc = vorbis.vorbis_comments();
    let get = |key: &str| vc.get(key).unwrap_or("").to_string();
    meta.name = get("PRESET_NAME");
    meta.product = get("PRESET_PRODUCT");
    meta.vendor = get("PRESET_VENDOR");
    meta.database = get("PRESET_DATABASE");
    meta.persistent_id = get("PRESET_PERSISTENT_ID");
    meta
}

/// Rebuild the preview registry from the files on disk: walk pot's preview directory, derive
/// each `.ogg`'s hash from its filename, read its embedded Vorbis metadata (empty if absent),
/// and upsert a registry row. Returns the number of files registered.
///
/// This is both the recovery path (registry lost/corrupted) and the one-time **adoption**
/// pass: previews that predate the registry get registered so they are not mistaken for
/// orphans. Run it before using orphan GC against a library that predates this feature.
pub fn rebuild_registry_from_files() -> usize {
    let root = preview_root();
    let mut count = 0;
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ogg") {
            continue;
        }
        let Some(rel) = path
            .strip_prefix(root.as_std_path())
            .ok()
            .and_then(|p| p.to_str())
            .map(normalize_rel)
        else {
            continue;
        };
        let Some(preview_hash) = preview_hash_from_rel(&rel) else {
            continue;
        };
        let meta = read_preview_metadata(path);
        crate::db::insert_preview(&crate::db::PreviewRow {
            preview_hash,
            file_path: rel,
            preset_name: meta.name,
            product: meta.product,
            vendor: meta.vendor,
            database_id: meta.database,
            persistent_id: meta.persistent_id,
            recorded_at: 0,
        });
        count += 1;
    }
    count
}
