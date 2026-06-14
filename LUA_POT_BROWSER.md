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

### Group 4 — Preset Crawler (required)
Control surface over the existing Rust crawler: start a crawl with the captured mouse
positions (incl. the "Save Preset As" scraping option), poll progress/state, import or
discard the result. The crawl loop, mouse automation, and scraping stay in Rust
(`pot/src/preset_crawler.rs`); Lua drives the wizard UI.

### Group 5 — Preview Recorder (required)
Control surface over the existing Rust preview recorder: choose output mode, start,
poll progress, handle failures. Recording stays in Rust
(`pot/src/preview_recorder.rs`); Lua drives the wizard UI.

## Status (2026-06-14)

Groups 1, 2 and 3 are implemented and compile cleanly (`cargo check -p pot-api`).
They have **not** been run in REAPER from this environment — ReaImGui and the egui build
can't run in the dev sandbox — so they need a `reaper_pot` rebuild + script reload to
confirm behavior. The Lua browser now has parity with the original egui Pot Browser for
everything except the two wizards (groups 4 and 5), which are not started.

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

### Architecture notes
- `pot-api` owns a global, instance-independent pot unit (`standalone_unit.rs`) with a
  minimal `PotIntegration`: its own favorites/exclude statics, no-op notifications, a
  never-matching protected FX, and (since group 3) it stores the current FX preset so the
  macro panel has a data source.
- `pot-extension` is the headless cdylib (`reaper_pot`). At REAPER startup it sets up
  SWELL + reaper-high, registers the API, and runs the database scan (`warm_up`).
- All API entry points are wrapped in `reaper_low::firewall`, so a panic returns safely
  instead of aborting REAPER across the C ABI.

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
- Group 4 (Preset Crawler) and Group 5 (Preview Recorder): each needs a control-surface
  `HB_Pot_*` API over the existing Rust implementations (`pot/src/preset_crawler.rs`,
  `pot/src/preview_recorder.rs`); the crawl/record loops and mouse automation stay in Rust.
- Manual REAPER verification of groups 1–3 on the user's machine.
