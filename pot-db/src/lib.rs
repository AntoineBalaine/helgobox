//! SQLite-backed scan cache + preview registry for the pot engine.
//!
//! This crate is deliberately ignorant of all `pot` types. It stores opaque serialized
//! blobs (the `entries` column) plus primitive bookkeeping columns, so the `pot` crate
//! owns the live [`Connection`] and does all (de)serialization of provider preset entries.
//! See `POT_DB_DESIGN.md` for the rationale (denormalized blob cache + recorded-preview
//! registry, fan-out search kept in memory).
//!
//! Two tables:
//! - `preset_files` — scan cache. One row per file, keyed on `path`, invalidated by
//!   `(mtime, size)`. `entries` is the provider's serialized snapshot of everything that
//!   file parsed into (one element for directory/projects, many for ini). Disposable
//!   derived data: it can always be dropped and rebuilt from disk.
//! - `previews` — registry of pot-recorded previews, keyed on the preview-locating hash.
//!
//! Schema versioning uses the built-in `PRAGMA user_version` rather than a bespoke table:
//! it avoids the bootstrap chicken-and-egg and is set transactionally with each migration.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Ordered list of schema migrations. The DB's `user_version` is the count of migrations
/// already applied; [`run_migrations`] applies any with a higher index. Append-only: never
/// edit or reorder an existing entry, only add new ones at the end.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema.
    "
    CREATE TABLE IF NOT EXISTS preset_files (
        path         TEXT    NOT NULL PRIMARY KEY,
        mtime        INTEGER NOT NULL,
        size         INTEGER NOT NULL,
        provider     TEXT    NOT NULL,
        entries      TEXT    NOT NULL,
        last_seen_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS preset_files_provider ON preset_files (provider);

    CREATE TABLE IF NOT EXISTS previews (
        preview_hash  TEXT    NOT NULL PRIMARY KEY,
        file_path     TEXT    NOT NULL,
        preset_name   TEXT    NOT NULL,
        product       TEXT    NOT NULL,
        vendor        TEXT    NOT NULL,
        database_id   TEXT    NOT NULL,
        persistent_id TEXT    NOT NULL,
        recorded_at   INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT NOT NULL PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
];

/// Open (creating if needed) the pot database at `db_path`, set pragmas, and run migrations.
///
/// WAL mode is enabled so a future split into per-thread connections gets concurrent
/// readers-alongside-one-writer for free; `busy_timeout` makes the rare writer-vs-writer
/// contention wait rather than error.
pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    run_migrations(&mut conn)?;
    Ok(conn)
}

fn run_migrations(conn: &mut Connection) -> rusqlite::Result<()> {
    let mut version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    while (version as usize) < MIGRATIONS.len() {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATIONS[version as usize])?;
        // `user_version` is part of the file header and participates in the transaction, so
        // a crash mid-migration leaves the version unchanged and the migration retried.
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
        version += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Scan cache (`preset_files`)
// ---------------------------------------------------------------------------------------

/// Look up a file in the scan cache. Returns the stored `entries` blob **only if** the
/// cached `(mtime, size)` match the caller-supplied values, in which case the row's
/// `last_seen_at` is bumped to `pass_ts` to mark it alive for this refresh pass.
///
/// A miss (no row, or stale stat) returns `None`; the caller then re-parses the file and
/// calls [`cache_store`].
pub fn cache_lookup(
    conn: &Connection,
    path: &str,
    mtime: i64,
    size: i64,
    pass_ts: i64,
) -> rusqlite::Result<Option<String>> {
    let hit: Option<String> = conn
        .query_row(
            "SELECT entries FROM preset_files WHERE path = ?1 AND mtime = ?2 AND size = ?3",
            params![path, mtime, size],
            |row| row.get(0),
        )
        .optional()?;
    if hit.is_some() {
        conn.execute(
            "UPDATE preset_files SET last_seen_at = ?2 WHERE path = ?1",
            params![path, pass_ts],
        )?;
    }
    Ok(hit)
}

/// Upsert a file's parsed result into the scan cache, marking it alive for this pass.
pub fn cache_store(
    conn: &Connection,
    path: &str,
    mtime: i64,
    size: i64,
    provider: &str,
    entries: &str,
    pass_ts: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO preset_files (path, mtime, size, provider, entries, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
             mtime = ?2, size = ?3, provider = ?4, entries = ?5, last_seen_at = ?6",
        params![path, mtime, size, provider, entries, pass_ts],
    )?;
    Ok(())
}

/// Delete rows for `provider` not touched during the pass that started at `pass_ts`
/// (i.e. files that disappeared). Returns the number of rows removed. The cache holds only
/// derived data, so this is always safe.
pub fn cache_sweep(conn: &Connection, provider: &str, pass_ts: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM preset_files WHERE provider = ?1 AND last_seen_at < ?2",
        params![provider, pass_ts],
    )
}

/// Drop the entire scan cache (e.g. for a forced full rescan). Disposable derived data.
pub fn cache_clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DELETE FROM preset_files")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Meta key/value store (small bits of cache-validity state, e.g. the plugin-set token)
// ---------------------------------------------------------------------------------------

pub fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Preview registry (`previews`)
// ---------------------------------------------------------------------------------------

/// One recorded-preview row. `preview_hash` is the preview-locating hash (the preset's
/// `content_hash` when present, else the hash of its persistent IDs) — i.e. whatever names
/// the file on disk. `file_path` is relative to the preview root.
#[derive(Clone, Debug)]
pub struct PreviewRow {
    pub preview_hash: String,
    pub file_path: String,
    pub preset_name: String,
    pub product: String,
    pub vendor: String,
    pub database_id: String,
    pub persistent_id: String,
    pub recorded_at: i64,
}

/// Insert or replace a preview registry row (called after a preview OGG is written).
pub fn insert_preview(conn: &Connection, row: &PreviewRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO previews
            (preview_hash, file_path, preset_name, product, vendor, database_id,
             persistent_id, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(preview_hash) DO UPDATE SET
            file_path = ?2, preset_name = ?3, product = ?4, vendor = ?5,
            database_id = ?6, persistent_id = ?7, recorded_at = ?8",
        params![
            row.preview_hash,
            row.file_path,
            row.preset_name,
            row.product,
            row.vendor,
            row.database_id,
            row.persistent_id,
            row.recorded_at,
        ],
    )?;
    Ok(())
}

/// Whether a recorded preview is registered for the given preview-locating hash. This is
/// the fast replacement for the per-preset filesystem probe.
pub fn preview_exists(conn: &Connection, preview_hash: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM previews WHERE preview_hash = ?1",
            params![preview_hash],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// All registered `(preview_hash, file_path)` pairs — used by the orphan scan to compare
/// the registry against the files actually on disk.
pub fn all_previews(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT preview_hash, file_path FROM previews")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Remove a preview registry row (called when its file is deleted via orphan GC).
pub fn delete_preview(conn: &Connection, preview_hash: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM previews WHERE preview_hash = ?1",
        params![preview_hash],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        conn
    }

    #[test]
    fn migrations_are_idempotent_and_set_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        run_migrations(&mut conn).unwrap(); // second run is a no-op
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
    }

    #[test]
    fn cache_hit_only_on_matching_stat() {
        let conn = mem();
        cache_store(&conn, "/a.ini", 100, 2000, "ini", "[{\"n\":1}]", 1).unwrap();
        // Matching stat → hit, and last_seen bumped.
        assert_eq!(
            cache_lookup(&conn, "/a.ini", 100, 2000, 2).unwrap().as_deref(),
            Some("[{\"n\":1}]")
        );
        // Changed mtime → miss.
        assert_eq!(cache_lookup(&conn, "/a.ini", 101, 2000, 3).unwrap(), None);
        // Changed size → miss.
        assert_eq!(cache_lookup(&conn, "/a.ini", 100, 2001, 3).unwrap(), None);
    }

    #[test]
    fn sweep_removes_only_untouched_rows_of_provider() {
        let conn = mem();
        cache_store(&conn, "/keep.ini", 1, 1, "ini", "x", 10).unwrap();
        cache_store(&conn, "/gone.ini", 1, 1, "ini", "x", 5).unwrap();
        cache_store(&conn, "/other.rfx", 1, 1, "directory", "x", 5).unwrap();
        // New pass at ts=10 touched keep.ini only; sweep ini-rows older than 10.
        let removed = cache_sweep(&conn, "ini", 10).unwrap();
        assert_eq!(removed, 1); // gone.ini
        assert!(cache_lookup(&conn, "/keep.ini", 1, 1, 11).unwrap().is_some());
        // Different provider untouched by the ini sweep.
        assert!(cache_lookup(&conn, "/other.rfx", 1, 1, 11).unwrap().is_some());
    }

    #[test]
    fn meta_roundtrip_and_upsert() {
        let conn = mem();
        assert_eq!(meta_get(&conn, "plugin_set").unwrap(), None);
        meta_set(&conn, "plugin_set", "abc").unwrap();
        assert_eq!(meta_get(&conn, "plugin_set").unwrap().as_deref(), Some("abc"));
        meta_set(&conn, "plugin_set", "def").unwrap();
        assert_eq!(meta_get(&conn, "plugin_set").unwrap().as_deref(), Some("def"));
    }

    #[test]
    fn preview_registry_roundtrip() {
        let conn = mem();
        assert!(!preview_exists(&conn, "abcd").unwrap());
        let row = PreviewRow {
            preview_hash: "abcd".into(),
            file_path: "ab/cd/abcd.ogg".into(),
            preset_name: "Hypersaw".into(),
            product: "Serum".into(),
            vendor: "Xfer".into(),
            database_id: "db1".into(),
            persistent_id: "pid1".into(),
            recorded_at: 42,
        };
        insert_preview(&conn, &row).unwrap();
        assert!(preview_exists(&conn, "abcd").unwrap());
        assert_eq!(all_previews(&conn).unwrap(), vec![("abcd".into(), "ab/cd/abcd.ogg".into())]);
        delete_preview(&conn, "abcd").unwrap();
        assert!(!preview_exists(&conn, "abcd").unwrap());
    }
}
