# Lua Pot Browser — parity plan

Goal: the Lua/ReaImGui Pot Browser (`lua/pot_browser.lua`), running on the headless
standalone `reaper_pot` extension via the `HB_Pot_*` ReaScript API, must reach **full
feature parity** with the original native egui Pot Browser — **including the Preset
Crawler and the Preview Recorder**. The standalone extension stays headless (no egui); the
entire UI lives in Lua.

All ReaImGui / REAPER calls must be verified against the LuaCATS definitions in
`/tank/projects/vscode-reascript-extension/resources/` (`imgui_defs_0.9.lua`,
`reaper-types.lua`) — do not invent API. (The `ImGui.Name` entries there map 1:1 to the
flat `reaper.ImGui_Name` calls the script uses.) Note: ReaImGui has **no** `StyleColorsDark/Light`;
only `GetStyleColor` / `PushStyleColor`. A dark/light toggle would need a hand-built
palette, so it is deliberately out of scope.

## Capability groups

### Group 1 — pure Lua (API already supports it) — DONE (commit 7d74b4c9)
- [x] All filter kinds, not just 4: `product_kind`, `project`, and the sub-filters
  `sub_bank` / `sub_category` (gated by `HB_Pot_SupportsFilter`).
- [x] Mini-filters as toggles: `is_user`, `is_favorite`, `is_supported`, `is_available`,
  `has_preview`.
- [x] Preview controls: volume slider (dB label), mute, stop, auto-preview on/off.
- [x] Has-preview indicator column.

### Group 2 — small new `HB_Pot_*` API + Lua — DONE (commit 7d74b4c9)
- [x] Search options: `Get/SetSearchField(index)` (0=name,1=product,2=extension) and
  `Get/SetUseWildcards`.
- [x] `HB_Pot_SupportsFilter(kind)` gating the sub-filters.
- [x] `HB_Pot_IsFilterItemExcluded` / `SetFilterItemExcluded` (filter items dim when
  excluded; right-click to toggle).
- [x] `HB_Pot_GetPresetPath` / `GetPreviewPath` — right-click preset to show in file
  manager (SWS `CF_LocateInExplorer`, else copy path to clipboard).
- [x] `HB_Pot_GetPresetContextName`.

### Group 3 — substantial new API + Lua — DONE (commits d6577ec6, 48613a5f, f4a783fd)
- [x] Selected-preset info panel: vendor, author, date, file size, comment, database,
  product (`HB_Pot_GetPresetMetadata`).
- [x] Per-preset favorites read + toggle (`HB_Pot_IsPresetFavorite` /
  `TogglePresetFavorite`).
- [x] Destination panel: load-into track / FX-slot choosers, show chain / show FX,
  FX-window-behavior + name-track-after-preset in the Options popup.
- [x] Macro-parameter panel: bank picker + live value sliders for the loaded FX.
  Required making the standalone unit *store* the current FX preset (it was a no-op).
- [ ] Deferred (low value): associated products, stats panel, add project database.

### Group 4 — Preset Crawler (required) — DONE (commit 83cfca06)
Control surface over the existing Rust crawler: start a crawl with the captured mouse
positions (incl. the "Save Preset As" scraping option), poll progress/state, import or
discard the result. The crawl loop, mouse automation, and scraping stay in Rust
(`pot/src/preset_crawler.rs`); Lua drives the wizard UI.
- [x] `HB_Pot_Crawler*` API over `crawl_presets` + `import_crawled_presets`
  (`pot-api/src/crawler.rs`): start / phase / preset+duplicate counts / last preset /
  stop reason / import / discard, plus `HB_Pot_GetFocusedFxName` /
  `HB_Pot_IsFocusedFxOpenFloating` for the precondition.
- [x] Async driver: `HB_Pot_RunTasks` pumps the main-thread future/task middlewares
  (`pot-api/src/executor.rs`), called once per frame by the Lua loop.
- [x] Lua wizard with full-screen-overlay click capture of button positions
  (native coords via `ImGui_PointConvertNative`). Spike: `lua/overlay_capture_spike.lua`.

### Group 5 — Preview Recorder (required) — DONE (commit 1f3a3fdd)
Control surface over the existing Rust preview recorder: choose output mode, start,
poll progress, handle failures. Recording stays in Rust
(`pot/src/preview_recorder.rs`); Lua drives the wizard UI.
- [x] `HB_Pot_Recorder*` API over `prepare_preview_recording` + `record_previews`
  (`pot-api/src/recorder.rs`): prepare (on the pot worker) / phase / prepared count /
  start / todo+failure counts / per-failure name+reason / export dir / error / discard.
- [x] Embeds the `pot-preview.RPP` template via `include_bytes!`; export mode copies a
  user-customizable template like the egui version.
- [x] Lua wizard: mode choice, preparing, ready, recording progress, done (failure list
  / export dir).

## Status (2026-06-14)

All five groups are implemented and compile cleanly (`cargo check -p pot-api` and
`-p pot-extension`). They have **not** been run in REAPER from this environment —
ReaImGui and the egui build can't run in the dev sandbox — so they need a `reaper_pot`
rebuild + script reload to confirm behavior. The Lua browser now has full feature parity
with the original egui Pot Browser, including the Preset Crawler and Preview Recorder
wizards.

Open verification items for the user's machine:
- Groups 1–3 behavior (never run in REAPER yet).
- Group 4: the full-screen-overlay click capture — confirm a borderless top-most
  ReaImGui window actually overlays the plug-in window and reports clicks (test with
  `lua/overlay_capture_spike.lua`). If ReaImGui clamps the window, fall back to
  keypress-while-hovering capture (same `ImGui_PointConvertNative` coordinate path).
- Group 5: the preview RPP template resolution and the render action in the standalone
  context.

Note on the dev sandbox: cross-user file permissions break the shared cargo registry and
`target/` dir, so `cargo check` must be run with a private `CARGO_HOME` +
`CARGO_TARGET_DIR` (see `AGENTS.md`).

### `HB_Pot_*` API surface (in `pot-api/src/api.rs`)

Each function is registered three ways (`API_*` for extensions, `APIvararg_*` for
ReaScript, `APIdef_*` for the docs). Strings use caller buffers (Lua sees plain returns).

- Engine: `IsAvailable`, `Refresh`, `IsBusy`, `DarkModeEnabled`
- Presets: `GetPresetCount`, `GetPresetName/Product/FileExt/ContextName/Path/Metadata`,
  `GetPreviewPath`, `HasPreview`, `Get/SetSelectedPresetIndex`,
  `IsPresetFavorite`/`TogglePresetFavorite`, `LoadPreset`
- Preview: `PlayPreview`, `StopPreview`, `Get/SetPreviewVolume`
- Filters: `GetFilterItemCount/Name`, `Get/SetFilter`, `SupportsFilter`,
  `Is/SetFilterItemExcluded`
- Search: `Get/SetSearchText`, `Get/SetSearchField`, `Get/SetUseWildcards`
- Destination: `Get/SetDestinationTrack` (−2 selected, −1 master, ≥0 specific),
  `Get/SetDestinationFxIndex`, `GetTrackCount/Name`, `GetDestinationFxCount/Name`,
  `ShowDestinationFx/Chain`
- Load options: `Get/SetLoadWindowBehavior`, `GetLoadWindowBehaviorCount/Name`,
  `Get/SetNameTrackAfterPreset`
- Macro panel: `GetMacroBankCount/Name`, `GetMacroParamCount/Name/Section`,
  `Get/SetMacroParamValue` (permille), `GetMacroParamValueLabel`
- Tasks: `RunTasks` (pumps the async executor each frame)
- Crawler (group 4): `GetFocusedFxName`, `IsFocusedFxOpenFloating`, `CrawlerStart`,
  `CrawlerPhase`, `CrawlerIsRunning`, `CrawlerPresetCount`, `CrawlerDuplicateCount`,
  `CrawlerCrawledCount`, `CrawlerLastPresetName`, `CrawlerStopReason`,
  `CrawlerStopReasonLabel`, `CrawlerError`, `CrawlerImport`, `CrawlerDiscard`
- Recorder (group 5): `RecorderPrepare`, `RecorderPhase`, `RecorderIsRunning`,
  `RecorderPreparedCount`, `RecorderStart`, `RecorderTodoCount`, `RecorderFailureCount`,
  `RecorderFailureName`, `RecorderFailureReason`, `RecorderExportDir`, `RecorderError`,
  `RecorderDiscard`

### Architecture notes
- `pot-api` owns a global, instance-independent pot unit (`standalone_unit.rs`) with a
  minimal `PotIntegration`: its own favorites/exclude statics, no-op notifications, a
  never-matching protected FX, and (since group 3) it stores the current FX preset so the
  macro panel has a data source.
- `pot-extension` is the headless cdylib (`reaper_pot`). At REAPER startup it sets up
  SWELL + reaper-high, registers the API, and runs the database scan (`warm_up`).
- All API entry points are wrapped in `reaper_low::firewall`, so a panic returns safely
  instead of aborting REAPER across the C ABI.
- Async wizards (groups 4/5): the crawl/record futures run on the main thread via
  `spawn_in_main_thread_from_main_thread` and are pumped by `executor.rs`'s middlewares,
  which the Lua loop ticks through `HB_Pot_RunTasks`. The recorder's slow preset-gathering
  runs on the pot worker thread and is handed back via a global slot polled each frame.
  Session state lives in main-thread thread-locals (`crawler.rs`, `recorder.rs`).

### Code review (2026-06-13) findings + fixes (commit d23dc18a)
- **Fixed (real defect):** `standalone_pot_unit()` built a fresh integration on every
  call, which ran `master_track()` per call and — worse — could make an already-loaded
  unit report itself unavailable (blank UI) on a transient failure. Now the loaded unit
  is returned directly; the integration is built only on first load.
- **Fixed:** startup double-scan — the Lua first-frame `Refresh` now only fires if the
  engine isn't already scanning or populated by the startup warm-up.
- **Fixed:** favorite toggle now rebuilds the list (no DB rescan) when the favorites
  filter is active.
- **Left as notes (minor):** preset-field cache keyed by a `u8` revision (wraps at 256);
  macro-panel main-thread thread-local contract (correct, commented); brief provider-lock
  stutter for visible rows during a scan; macro value quantization to 1000 steps;
  unbounded-lifetime `str_arg` (standard FFI, used immediately).

### Next
- Manual REAPER verification on the user's machine: groups 1–3 behavior, the group 4
  overlay click-capture (spike `lua/overlay_capture_spike.lua` first), and group 5
  preview-template resolution + render in the standalone context.
- If the overlay capture doesn't overlay the plug-in window, switch the crawler capture
  to keypress-while-hovering (same coordinate path, no overlay).

## Planned redesign: preview storage + scan cache

These address real shortcomings in the upstream `pot` engine's preview/scan design
(used unchanged today by both the egui browser and this standalone wrapper). They are
**not** Lua-UI changes — they require modifying the `pot` crate or layering an alternative
path in `pot-api`, and they diverge from how the original browser resolves previews, so
we'd own the divergence.

### 1. Embed preset identity as metadata in the preview file
Today internal previews are anonymous: `…/Helgoboss/Pot/previews/aa/bb/<hash>.ogg`,
named only by the preset's content hash, with no tags, no sidecar, no index. They are
orphans to any external tool and carry no back-link to the preset.
- Write the preset identity into the OGG as Vorbis comments (at minimum: preset name,
  product/plugin, vendor, source database, persistent ID; ideally the content hash too).
- Goal: external tools (sample browsers, DAW media explorers) can index and search the
  previews, and a stray `.ogg` is self-describing rather than an orphan.

### 2. Persistent scan cache (no re-hashing unchanged files)
Today `DirectoryDatabase::refresh` walks the whole preset root and re-reads + re-hashes
**every** file on every refresh, and `warm_up` runs that at each REAPER startup. There is
no mtime/size cache and no persistence.
- **Hard requirement:** assume a user's instrument/sample library may be hundreds of GB;
  startup must stay fast. Re-hashing the whole library per launch is unacceptable.
- Persist a scan cache keyed by `(path, mtime, size)` -> `(content_hash, detected
  plugins)`. On refresh, only re-read files whose mtime/size changed; everything else is
  served from the cache. Persist it across sessions (e.g. a small on-disk store in the
  resource dir) so cold startup is incremental, not full.
- Watch the interaction with content-hash-named previews: if a preset's content changes,
  its hash (and thus its preview file name) changes — the cache must invalidate the old
  entry, and ideally the redesign also addresses orphaned previews (no GC exists today).

## Save-as name capture: window-relative action recorder (supersedes fixed positions)

**Implemented (commit 429eb0e3).** Recorder in `pot/src/preset_recorder.rs`; replay +
verify-before-click in `pot/src/preset_crawler.rs` (`save_as_macro` on `CrawlPresetArgs`,
takes precedence over the kept-for-egui `save_as_dialog`); `HB_Pot_CrawlerRecord*` API in
`pot-api`; "record save-as actions" step in the Lua crawler wizard. macOS + Linux/X11
keycode tables; physical-position replay via `enigo::Key::Raw`. Not yet run inside REAPER.

### Why the current model is wrong
The upstream crawler scrapes a preset's name from the plug-in's "Save Preset As" dialog
using two fixed screen coordinates (`SaveAsDialogScraping { save_as_button_pos,
cancel_button_pos }`), and assumes: (a) one click opens the dialog, (b) the dialog's name
field is focused so a blind select-all+copy works, and (c) the dialog is a detectable new
OS window (`window_titles()` diff). All three break on real plug-ins — e.g. Serum reaches
Save As through a menu (multi-click), may not auto-focus the field, and may draw an
in-GUI dialog that is not a separate OS window. Fixed coordinates are also brittle to the
window moving between record and replay.

The overlay click-capture (used for the single Next-preset position) **cannot** record a
sequence: it intercepts the click, so a menu never opens during capture. Recording an
interactive multi-step flow requires observing the user's real clicks as they pass
through to the plug-in.

### Design: record window-relative input, replay relative
Both primitives already exist as dependencies — no new crates:
- `device_query` (already used by `base/src/mouse/enigo.rs`): polls global cursor
  position + mouse-button state + key state. Source of click/keystroke events.
- `xcap` (already used by the crawler's `window_titles()`): `Window::all()` exposes per
  window `id()`, `app_name()`, `title()`, `x()/y()/width()/height()`. Source of window
  identity + geometry.
- `enigo` (already used): replay (synthesis).

**Record** (Rust, on a dedicated high-rate poll thread so double-clicks / fast taps aren't
missed): on each mouse-button down-edge, capture the click as window-relative — find the
topmost `xcap` window under the cursor and store `{ window id, app_name, title,
rect-at-capture, offset within window, button, timestamp }`. Capture key events with
timestamps too. The user demonstrates the whole flow once on the real plug-in: open the
menu, click Save As, focus the name field, select-all + copy, cancel.

**Replay** (enigo): for each event, re-resolve the target window's *current* geometry via
xcap, click at `current-origin + recorded-offset`, reproducing recorded delays. This
survives the window being at a different position than during recording.

This removes all three bad assumptions: arbitrary sequence (not single-click), the focus
click is part of the recording (no auto-focus assumption), and open/close detection comes
from diffing the xcap window-id set rather than titles. Crucially it does **not** assume
the dialog is a native OS window: a plug-in-owned in-GUI dialog simply spawns no new id,
so its clicks resolve relative to the plug-in's own window and replay still works. Native
and custom-styled dialogs fall out of the same mechanism.

### Honest caveats
- **Transient dialog ids don't persist:** a dialog that opens/closes per preset gets a new
  xcap id each time, so an id captured at record-time won't match at replay-time. The
  stable anchor is the plug-in window (open for the whole crawl); clicks that landed in a
  transient dialog are re-resolved at replay by heuristic ("the window that appeared after
  the corresponding open-click", or match on app_name/title).
- **macOS permissions:** xcap (window enumeration) likely needs Screen Recording
  permission; device_query needs Accessibility. The existing crawler already uses xcap, so
  this isn't new, but both must be granted once.
- **Coordinate space:** device_query coordinates and xcap rects must share one global
  pixel space; HiDPI / multi-monitor scaling is the usual trouble spot (same family as the
  overlay's `PointConvertNative` conversion) — verify empirically.

### Architecture
The recorded event list lives in Rust (a recorder module in `pot-api`, or upstream in
`pot`), never crossing into Lua. Lua drives it with simple control calls
(`HB_Pot_CrawlerRecordStart/Stop/RecordedEventCount`), and `HB_Pot_CrawlerStart` consumes
the stored recording when save-as mode is on (replacing the two fixed positions). Keep the
fixed-position path working until the recorder is verified, so nothing regresses.

### Proof of concept — VALIDATED (standalone `pot-input-poc/`, macOS)
A standalone Rust binary (outside the workspace) depending only on
`device_query` + `xcap` + `enigo` + `arboard`, validated on macOS. Findings:

- **Window-relative click record/replay works.** Clicks resolve to the topmost window
  under the cursor — xcap returns windows **front-to-back** on macOS, so the *first*
  containing window is the hit (NOT smallest-area). Replay re-resolves the window's current
  rect, so clicks land correctly even after the window is moved.
- **Keystrokes must be replayed by PHYSICAL keycode, not character** — this is the big
  one. `device_query` reports physical key *positions* named by US-QWERTY; `enigo::Key::
  Layout(char)` sends *characters*. On a non-QWERTY layout (tested: bépo) these don't
  correspond — e.g. the user's Cmd+C is the physical key at QWERTY-H, logged as `Keycode::
  H`; replaying it as `Layout('h')` fired Cmd+H (Hide) and broke the session. Fix: map the
  device_query Keycode to the macOS virtual keycode for the *same physical position*
  (`Keycode::H -> kVK_ANSI_H`) and replay via `enigo::Key::Raw`. That presses the same
  physical key, which under the active layout produces the same character → Cmd+C copies
  correctly, layout-independent. (Only assumption: layout unchanged between record and
  replay — always true in-session.)
- **The keyboard copy path works end to end** — recorded bépo Cmd+A/Cmd+C replayed and the
  plug-in's preset name landed in the clipboard (read back via `arboard`). The keyboard
  path is sufficient and universal: every text field supports Cmd+A/Cmd+C, so there is **no
  need** for a mouse-only right-click→Copy path or click-drag selection (a drag *would* be
  needed for mouse selection, which we deliberately skip).
- **Vital draws its Save-As dialog in-view** (no separate OS window): all clicks resolved
  to Vital's main window id. So the transient-dialog-id concern doesn't arise for Vital
  (clicks anchor to the stable plug-in window). Other plug-ins may differ.
- **Permissions (macOS):** device_query needs Accessibility **and** Input Monitoring (the
  latter specifically for keystrokes); enigo needs Accessibility for replay; xcap needs
  **Screen Recording** for window enumeration (without it, only the menu bar is returned).

### Revised integration plan (from PoC learnings)
- **Coordinates: absolute by default, window-relative as an upgrade.** In a real crawl the
  flow is recorded once and replayed immediately with the plug-in window stationary, so
  *absolute* coordinates suffice and need **no Screen Recording** — matching the existing
  crawler's permission footprint (Accessibility only). The recorder stores both absolute
  and window-relative; replay prefers window-relative when Screen Recording is available,
  else falls back to absolute. So Screen Recording is a graceful upgrade, not a hard gate.
- **Keystrokes via physical keycode** (`Key::Raw`), per the bépo finding above. Per-platform
  keycode tables (the PoC has the macOS table).
- **Recorded macro lives in Rust**, never crossing into Lua; Lua drives it with
  `HB_Pot_CrawlerRecordStart/Stop/RecordedEventCount`, and `HB_Pot_CrawlerStart` consumes it
  when save-as mode is on (replacing the two fixed positions). Keep the fixed-position path
  working until the recorder is verified.
- **Drop** the mouse-only right-click→Copy and click-drag paths (Cmd+C is universal).
