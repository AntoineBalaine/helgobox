# Pot — SQLite scan cache + preview registry design

## Motivation

The driving goal is to **scale to 30k+ presets while keeping REAPER startup under 5s**.
Two distinct costs stand in the way, and they were initially conflated:

1. **Re-hashing files on every refresh.** The file-hashing providers
   (`directory.rs`, `ini.rs`, `projects.rs`) re-read and re-hash their files on every
   refresh, which `warm_up` runs at each REAPER startup. The Komplete/NKS provider does
   *not* hash — it reads NI's prebuilt SQLite database — so the GB-scale Kontakt-style
   library is already fast and is **not** what this addresses. The relevant providers are
   the ones that hash, with `ini.rs` the most important: it reads REAPER's FX preset
   `.ini` files, which is **exactly where the crawler's saved native FX presets land**.
   At 30k presets the bulk will be a few hundred `.ini` files, each holding many presets.

2. **Per-preset filesystem probing.** `find_preview_file` (`pot/src/lib.rs`) probes the
   filesystem once per preset during collection building to decide whether a preview
   exists. At 30k presets this is the dominant interactive cost, independent of startup.
   There is also no index of recorded previews, so orphaned preview files can never be
   enumerated.

This document specifies the SQLite-backed layer that fixes both: a **scan cache** so
unchanged files are never re-read, and a **preview registry** so preview existence is a
DB lookup and orphans are enumerable.

> Note: the original egui Pot Browser is being retired. Coexistence with it is therefore
> **not** a design constraint — the DB layer only needs to serve the Lua/standalone path.

---

## Scope

| In scope | Out of scope |
|---|---|
| Scan cache: skip unchanged preset files at startup | Render settings UI |
| Preview registry: fast lookup, orphan GC | UCS tagging backend |
| Vorbis comment metadata embedded in preview OGG files | Steinberg MediaBay integration |
| Orphan GC UI (list + confirm delete) | Automatic deletion of any file |

---

## Decisions (from design review, 2026-06)

- **Search stays cross-backend by fan-out, unchanged.** `gather_preset_ids_internal`
  already queries every provider and merges (pot_database.rs:348). NKS keeps querying NI's
  own SQLite; the file providers keep querying their in-memory `Vec<PresetEntry>`. A generic
  query already hits NKSFs, FX presets, RfxChains, and projects at once. No change needed.
- **Search stays in-memory.** At 30k presets the per-provider vectors total ~10–30 MB
  (metadata only — preset chunks are read from disk on demand, never held), and a linear
  scan per keystroke is sub-millisecond. RAM is not a constraint, so there is no reason to
  push querying into SQL. The SQLite DB earns its place for **persistence** (skip
  re-hashing at startup), not for memory or query.
- **Denormalized blob cache, not a normalized presets table.** Presets are stored in memory
  as heterogeneous, provider-private structs behind `Box<dyn Database>` (no in-memory
  variant enum to dump to rows). The cache mirrors that: one row per file holding a
  serialized snapshot of that file's `Vec<PresetEntry>`. At startup we deserialize the
  snapshot straight back into the same granular in-memory vectors — search runs over those,
  exactly as today. The blob is a load cache, never a query target.
- **Normalize only if/when we ingest NKS.** A normalized one-row-per-preset table earns its
  keep only when SQL becomes the search engine over *everything* — which requires ingesting
  NI's NKS index into a pot-owned table. We chose fan-out, so that prize is off the table
  and normalization is deferred. Because the cache is disposable derived data, switching
  later costs nothing: drop `preset_files` and rebuild from disk. See the normalization note
  under the `previews` schema.

---

## Database location

Single SQLite file at `<REAPER resource dir>/Helgoboss/Pot/pot.db`, opened in **WAL mode**
(`PRAGMA journal_mode=WAL`). WAL produces two harmless sidecar files next to it
(`pot.db-wal`, `pot.db-shm`).

The file is created on first run; the migration sequence runs at open. See
*Threading & concurrency* below for connection lifecycle — the connection is **not** a
single main-thread-bound handle, because the 30k-scale scan must run off the UI thread.

---

## Schema

### `schema_version`

```sql
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);
```

Seeded with `(1)` on creation. Migration code checks this on open and applies deltas in
order. Never dropped; version is bumped in place.

### `preset_files` — scan cache

The cache is keyed per **file**, but a single file can yield **many presets** — `ini.rs`
parses one FX-preset `.ini` into N presets, each with its own `content_hash` and plugin
set. So the cached value is a serialized **blob of all preset entries derived from that
file**, not a single hash. This one schema serves all three hashing providers uniformly:
`directory.rs` and `projects.rs` store a one-element blob, `ini.rs` an N-element blob.

```sql
CREATE TABLE IF NOT EXISTS preset_files (
    path          TEXT    NOT NULL PRIMARY KEY,
    mtime         INTEGER NOT NULL,   -- seconds since Unix epoch
    size          INTEGER NOT NULL,   -- bytes
    provider      TEXT    NOT NULL,   -- which provider parsed this file (directory|ini|projects)
    entries       TEXT    NOT NULL,   -- JSON: array of derived preset entries (see below)
    last_seen_at  INTEGER NOT NULL    -- Unix epoch seconds; updated each refresh pass
);
```

Each element of `entries` carries everything the provider needs to rebuild its in-memory
`PresetEntry` without touching the file again:

```jsonc
{
  "preset_name": "...",
  "relative_path": "...",          // or in-file preset index for ini.rs
  "content_hash": "<hex xxh3-128>",
  "plugin_cores": [                // full PluginCore, NOT just the id (see Resolved details)
    { "id": "...", "product_kind": "Instrument|Effect|null", "product_id": "..." }
  ]
}
```

**Refresh algorithm (replaces the current full walk+hash), per hashing provider:**

1. Walk the provider's root directory, collecting `(path, mtime, size)` via `stat` only.
2. For each file, look up `(path)` in `preset_files`.
3. If the row exists and `(mtime, size)` match → deserialize `entries` and rebuild all
   `PresetEntry`s from it, update `last_seen_at`, **never open the file**.
4. If missing or `(mtime, size)` differ → read+parse+hash the file, build the entries,
   upsert the row.
5. After the walk, rows for this provider with `last_seen_at` older than the current pass
   are stale (file deleted/moved) — delete them. They carry no user data; silent deletion
   is safe.

At 30k presets concentrated in a few hundred `.ini` files, a warm refresh is a few hundred
`stat` calls plus one bulk SELECT — no file reads — comfortably inside the 5s budget.

### `previews` — preview registry

```sql
CREATE TABLE IF NOT EXISTS previews (
    preview_hash    TEXT    NOT NULL PRIMARY KEY,  -- the PREVIEW-LOCATING hash (see note)
    file_path       TEXT    NOT NULL,              -- relative to the preview root
    preset_name     TEXT    NOT NULL,
    product         TEXT    NOT NULL,
    vendor          TEXT    NOT NULL,
    database_id     TEXT    NOT NULL,
    persistent_id   TEXT    NOT NULL,
    recorded_at     INTEGER NOT NULL               -- Unix epoch seconds
);
```

**Key is the preview-locating hash, not `content_hash`.** A preset's `content_hash` is
`Option`; when it is `None`, the preview file is named by a hash of the persistent
database+preset IDs instead (`get_preview_file_path_from_hash`). The key must be whichever
hash actually names the file on disk, so the lookup works for presets with no content hash.

`file_path` is relative to the preview root (e.g. `aa/bb/<hash>.ogg`) so the registry
survives the user moving the REAPER resource directory.

This table indexes **only pot-recorded previews** — files pot writes into its own preview
directory. A preset has two other possible preview sources that are *derivable* from the
preset and stay out of this table: (a) the preset is itself an audio file, and (b) the
database supplies its own preview path (`db_specific_preview_file`). Only `recorded`
previews correspond to files pot may delete, so orphan GC is naturally scoped to this
table's rows.

**Linkage model: a computed shared key, not a stored pointer.** There is deliberately no
preset→preview pointer column and no preview→preset foreign key. Given a preset, its
`preview_hash` is *computed* (`get_preview_file_path_from_hash`) and looked up in
`previews`; the row's existence is the "has a recorded preview" answer. The `persistent_id`
/ `database_id` columns on `previews` exist only for self-description and index rebuild —
not as a live back-pointer. One authoritative key, shared, no redundant two-way pointers,
no `source` discriminator. The other two preview kinds (the preset is itself an audio file;
the database supplies its own path) are derivable in-memory from the preset and are never
stored here.

> A real `previews.preset_id` foreign key would require presets to be **rows**, but today
> they live in the `preset_files.entries` blob, so there is nothing to reference. Promoting
> presets into a normalized `presets` table is the future architecture that would earn the
> FK — and would also turn the has-preview filter into a pure-SQL join (eliminating the
> per-preset fs probe entirely) and give the deferred user-tags table a real FK target.
> That is a substantial refactor of how `pot` serves presets (it currently builds in-memory
> per-provider collections), so it is **out of scope here** and noted as the destination,
> not this iteration.

A preview is inserted when the recorder writes the OGG file successfully. A preview row
is removed only when the user explicitly deletes the file through the orphan GC UI.

---

## Metadata in preview OGG files

After the recorder writes a preview, Rust writes Vorbis comments into the OGG. Note this
is a header rewrite, not an append — use a read/write tagging crate (`lofty` supports
Vorbis comments on OGG); read-only crates like `lewton` will not do. The DB row and the
embedded tags carry identical data; the embedded tags make each file self-describing and
allow a "rebuild index from files" recovery operation.

**Vorbis comment fields:**

```
PRESET_NAME=<name>
PRESET_PRODUCT=<plugin/instrument name>
PRESET_VENDOR=<vendor>
PRESET_DATABASE=<pot database id>
PRESET_PERSISTENT_ID=<uuid or pot-assigned id>
PRESET_CONTENT_HASH=<hex>
REAPER_POT_VERSION=1
RECORDED_AT=<ISO 8601 timestamp>
```

**Recovery:** "Rebuild preview index" walks the preview directory, reads Vorbis comments
from each OGG, and repopulates the `previews` table. This is a manual action exposed in
the browser UI, not automatic.

---

## Orphan GC

An orphaned preview is an OGG file in pot's preview directory that has no corresponding
row in `previews`.

**Rule:** if a file is not in `previews`, pot does not own it. It is never deleted
automatically, regardless of where it sits in the preview directory tree.

**UI flow:**

1. User clicks "Find orphaned previews" in the browser settings/maintenance panel.
2. Rust walks the preview root, collects paths not in `previews`, returns them to Lua
   with file sizes.
3. Lua renders the list: filename, size, checkbox (checked by default), running total
   size, a "Delete selected" button, and a "Cancel" button.
4. On confirm, Rust deletes only the checked files and removes any stale DB rows.
5. The result is shown inline (N files deleted, M skipped).

No file is ever deleted without this explicit user-driven flow.

**Adoption on first run.** When this feature first ships, any `.ogg` already in the
preview directory predates the registry and would otherwise be flagged as an orphan on the
first scan — a data-loss trap. Guard against it with a one-time adoption pass: register
every existing preview by deriving its `preview_hash` from the filename (the filename *is*
the hash, so no file read is needed). Metadata for adopted files is unknown until a
"rebuild index from files" pass reads their Vorbis tags (older files have none). With the
egui browser retired and no preview library currently on disk, this pass is effectively a
no-op today, but it must exist before orphan GC is allowed to delete anything — it is the
safeguard that keeps the "not in DB ⇒ not owned by pot" rule from misfiring on a fresh
upgrade.

---

## `HB_Pot_*` API additions

```
HB_Pot_DbRebuildIndex()          -- re-scan preview dir from Vorbis tags, repopulate previews table
HB_Pot_DbOrphanCount()           -- returns count of orphan preview files found
HB_Pot_DbOrphanName(index)       -- orphan file path at index
HB_Pot_DbOrphanSize(index)       -- orphan file size in bytes
HB_Pot_DbScanOrphans()           -- trigger the orphan scan (async, polled by RunTasks)
HB_Pot_DbDeleteOrphans(indices)  -- delete the specified subset (space-separated index list)
```

The orphan scan runs on the pot worker thread and hands results back via a global slot
polled each frame by `HB_Pot_RunTasks`, matching the pattern used by the recorder.

---

## Threading & concurrency

The 30k-scale scan must not block the UI, so the DB layer is **not** single-threaded.
Use WAL mode with **one connection per thread** (SQLite connections are not `Sync`):

- **Worker thread** holds the writer connection. It runs the refresh/scan (the bulk of
  the work — stat, parse changed files, upsert `preset_files`) and the orphan scan.
- **Main/UI thread** holds a reader connection for small, fast queries (a single preview
  lookup, the selected-preset metadata).
- WAL allows concurrent readers alongside one writer, so the UI never stalls while a
  30k-row scan is in flight. SQLite serializes the rare writer-vs-writer case (a scan
  upsert vs. a preview insert) briefly; set `PRAGMA busy_timeout` so those wait rather
  than error.

A preview insert happens on whichever thread the recorder finishes on; route it through
the writer connection (the recorder already runs main-thread per the executor model, so in
practice it shares the UI-side path — confirm at implementation and use one writer).

## Crate ownership

A new **`pot-db`** crate holds the SQLite schema, migrations, and query helpers. The live
connections, however, are owned at the **`pot`** level, not `pot-api` — both wiring points
(`*::refresh` in the hashing providers, and the preview-registry write in
`pot::preview_recorder`) live in `pot`, and `pot` already depends on `rusqlite` for the
Komplete provider. Owning the connection in `pot-api` would be backwards: `pot` would have
no way to reach it during refresh. `pot-api` and the (now-retired) egui browser both go
through `pot`, so a pot-level owner serves everyone.

```
pot-db
  depends on: base (PersistentHash / hash_util types), camino (Utf8Path), rusqlite
  exports:    schema + migration runner, PresetFilesCache, PreviewRegistry query helpers
  used by:    pot — owns the connections, calls the cache from the hashing providers'
                    refresh and the registry write from preview_recorder
              pot-api — drives orphan-GC / rebuild-index control calls exposed to Lua
```

`rusqlite = "0.30.0"` is already a workspace dependency. The `pot` crate already declares
it with `features = ["bundled"]` in order to read NI's Komplete database — so SQLite's C
source is already compiled into the binary today. The new `pot-db` crate inherits the same
workspace dep; no build changes are needed and the binary size impact of adding our own
tables is zero (SQLite is already there). The bundled SQLite amalgamation contributes
roughly 350 KB to the compiled binary on x86-64 release builds.

---

## Migration strategy

All schema creation is done through numbered migrations run in order at open time:

```rust
const MIGRATIONS: &[&str] = &[
    // migration 1
    "CREATE TABLE IF NOT EXISTS schema_version ...; \
     CREATE TABLE IF NOT EXISTS preset_files ...; \
     CREATE TABLE IF NOT EXISTS previews ...; \
     INSERT OR IGNORE INTO schema_version VALUES (1);",
    // future migrations appended here
];
```

The current `schema_version` row determines which migrations to skip. Each migration runs
inside a transaction so a crash mid-migration can't leave a half-applied schema; the
version bump is part of the same transaction. Safe to run at every startup; `IF NOT EXISTS`
guards make them idempotent.

---

## Resolved implementation details

From codebase investigation (2026-06-15):

**Hash algorithm:** XXH3 128-bit via `base::hash_util::PersistentHasher` (the `xxhash_rust`
crate). `content_hash` in `preset_files` stores the hex encoding of this 128-bit value —
same encoding already used by `convert_hash_to_dir_structure()` in `base/src/file_util.rs`.

**Hashing providers (cache applies to all three):**
- `pot/src/providers/directory.rs` — `RfxChain` / `RTrackTemplate`, one preset per file.
- `pot/src/providers/projects.rs` — REAPER project track presets, one per file.
- `pot/src/providers/ini.rs` — REAPER FX-preset `.ini` files, **many presets per file**,
  each individually hashed from its `Data*` properties (`ini.rs:182-202`). This is where
  the crawler's saved native FX presets land and the main 30k-scale contributor.

The Komplete provider (`komplete.rs`) is excluded — it reads NI's prebuilt DB and does not
hash. The cache intercepts each hashing provider's `refresh` before it opens a file: stat
first, and on a `(mtime, size)` match rebuild every `PresetEntry` from the cached blob.

**Entry `plugin_cores` format:** `PluginCore` (`pot/src/plugins.rs:161`) is a `Copy` struct
`{ id: PluginId, product_kind: Option<ProductKind>, product_id: ProductId }`. The cache
must serialise **all three fields**, not just the id — the in-memory `PresetEntry` needs
the full `NonCryptoIndexMap<PluginId, PluginCore>`, so storing only ids would force a
re-read on every cache hit and defeat the cache. All three are small and serialise cleanly.

**DB file path:** `Reaper::get().resource_path().join("Helgoboss/Pot/pot.db")` —
consistent with how the preview root and Komplete DB paths are resolved.

**`rusqlite` in workspace:** Already present at `0.30.0`, used by
`pot/src/providers/komplete.rs` for the NKS database. No new dep needed, just a new
crate member.

**Metadata available at preview record time** (`pot/src/preview_recorder.rs`): the full
`PotPreset` is in scope, providing `common.name`, `common.product_name`,
`common.persistent_id`, `common.content_hash`, and `common.metadata` (author, vendor,
comment, modification_date, file_size_in_bytes). All Vorbis comment fields listed above
are directly available.

**Preview lookup today** (`pot/src/lib.rs::find_preview_file()`): probes the filesystem
on every call, called during collection building for every preset with the `has_preview`
filter active. The `previews` table replaces this with a single DB lookup per preset.

**Schema evolution:** SQLite's `ALTER TABLE` supports adding columns but not removing or
renaming them. Removing or restructuring a column requires the standard three-step dance:
create new table, copy data, drop old table, rename new. This is handled as a numbered
migration entry — three SQL statements, no special tooling. The migration runner opens the
DB, reads `schema_version`, executes any entries above the stored version in order, then
bumps the version. This is roughly twenty lines of Rust and is written once.

---

## Implementation status (2026-06-16)

Implemented on branch `pot-browser-linux`. `cargo check` is green for `pot-db`, `pot`,
`pot-api`, and `pot-extension`; `pot-db` unit tests pass; the Lua browser passes LuaLS
(only the documented `ImGui_BeginChild`/`ImGui_Attach` false positives). The final cdylib
link + on-device run must happen on the user's machine — this sandbox lacks `libxdo`
(needed by the crawler's `enigo` dependency) so `pot-extension` cannot be *linked* here,
only type-checked.

### Done
- **`pot-db` crate** — schema, migrations, scan-cache + preview-registry + meta KV helpers,
  with tests. Self-contained (only `rusqlite`).
- **Scan cache** wired into all three hashing providers (`directory`, `ini`, `projects`)
  via best-effort `pot::db` wrappers. `ini` caches a `Vec` blob per file (many presets per
  `.ini`); `projects` re-assigns `project_id` on load; `Proj` is path-derived, not cached.
- **Preview registry** — `preview_recorder` inserts a row after each pot-owned recording;
  `preview_exists` (the has-preview filter hot path) is now an indexed DB lookup instead of
  a per-preset filesystem stat.
- **Orphan GC + registry prune** — engine in `pot::preview_maintenance`, driven by new
  `HB_Pot_Db*` ReaScript functions, with a "Pot Database Maintenance" window in the Lua
  browser (scan → checkbox list with sizes → confirm → delete; plus prune).
- **Vorbis comment tagging** — after recording a pot-owned preview, the OGG is tagged (via
  `lofty`) with `PRESET_NAME/PRODUCT/VENDOR/DATABASE/PERSISTENT_ID/CONTENT_HASH`,
  `REAPER_POT_VERSION`, and `RECORDED_AT` (RFC 3339). Best-effort: a tagging failure is
  logged and never fails the recording (the registry still holds the same fields).
- **Rebuild index / adoption** — `rebuild_registry_from_files` walks the preview dir,
  derives each file's hash from its name, reads its embedded Vorbis metadata, and upserts a
  registry row. Exposed as `HB_Pot_DbRebuildIndex` and a "Rebuild index from files" button.
  This is both the recovery path *and* the one-time adoption pass (pre-existing previews get
  registered so they are not mistaken for orphans).

### Deviations from the plan above (intentional)
- **`PRAGMA user_version`** is used for migration versioning instead of a `schema_version`
  *table* — it avoids the bootstrap chicken-and-egg and is transactional. (The schema text
  above still describes a table; the code uses `user_version`.)
- **Plug-in-set invalidation (new, necessary).** Cached entries store *resolved* plugins,
  and both `directory` and `ini`/`projects` resolve plugins against the live plug-in DB
  (`find_plugin_by_id`). So `PotDatabase::refresh` computes an order-independent hash of the
  installed plug-in ids and, when it changes, drops the whole scan cache once (stored under
  the `meta` key `plugin_set`). This keeps `is_available`/product data correct across
  plug-in installs/removals at the cost of one full rescan when the plug-in set changes.
- **Connection: a single process-global `Mutex<Connection>` with WAL enabled**, not yet
  per-thread connections. Simpler and correct for v1; WAL is on so a later per-thread split
  is a drop-in optimisation. All `pot::db` calls are best-effort (fail soft → fall back to
  the pre-cache behaviour), so the DB is never a correctness dependency.
- **Cache DTOs** are explicit per-provider serde mirrors (plugins as a sequence, hash as
  hex) living in `pot::db` — the live types aren't serde and `IndexMap<PluginId, _>` can't
  be a JSON map.
- **Orphan scan is synchronous on the main thread** (the preview dir is bounded by the
  recorded-preview count, not the preset count). Not the worker-thread design; revisit only
  if it ever feels slow.

### Deferred (clearly out, with reason)
- **Automatic adoption on first run.** Adoption now exists as a manual action
  (`HB_Pot_DbRebuildIndex` / "Rebuild index from files"), which is sufficient given the
  current state (no previews on disk, egui retired). If a future build ships to users who
  already have an untagged preview library, consider running rebuild once automatically
  before the first orphan scan so they never see legitimate previews listed as orphans.
- **Per-thread WAL connections** (currently one `Mutex<Connection>`); **Vorbis tag write is
  a full-file rewrite** done synchronously after each render — both fine at current scale,
  revisit only if measured to matter.

### Files
- New: `pot-db/`, `pot/src/db.rs`, `pot/src/preview_maintenance.rs`,
  `pot-api/src/db_maintenance.rs`.
- Modified: workspace `Cargo.toml` (+ `lofty`), `pot/Cargo.toml`, `base/src/hash_util.rs`
  (`PersistentHash::from_raw`), `pot/src/api.rs` (`PersistentPresetId::db_id`),
  `pot/src/pot_database.rs` (plug-in token), `pot/src/providers/{directory,ini,projects}.rs`,
  `pot/src/lib.rs` (modules + `preview_exists`), `pot/src/preview_recorder.rs` (registry
  insert + Vorbis tagging), `pot-api/src/{api.rs,lib.rs}`, `lua/pot_browser.lua`
  (Maintenance window).
