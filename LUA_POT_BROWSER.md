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

### Group 1 — pure Lua (API already supports it)
- [ ] All filter kinds, not just 4: add `product_kind`, `project`, and the sub-filters
  `sub_bank` / `sub_category` (the latter two gated by `HB_Pot_SupportsFilter`, group 2).
- [ ] Mini-filters as toggles: `is_user`, `is_favorite`, `is_supported`, `is_available`,
  `has_preview` (filter kinds; `SetFilter` by index).
- [ ] Preview controls: volume slider, mute, stop button, auto-preview on/off
  (`Get/SetPreviewVolume`, `StopPreview`).
- [ ] Has-preview indicator column (`HB_Pot_HasPreview`).

### Group 2 — small new `HB_Pot_*` API + Lua
- [ ] Search options: `Get/SetSearchField(index)` (0=name,1=product,2=extension) and
  `Get/SetUseWildcards`.
- [ ] `HB_Pot_SupportsFilter(kind)` so the UI shows/hides sub-filters correctly.
- [ ] `HB_Pot_IsFilterItemExcluded` / `SetFilterItemExcluded` (the filter-item
  exclude-globally context menu).
- [ ] `HB_Pot_GetPresetPath` / `GetPreviewPath` (show in file manager).
- [ ] `HB_Pot_GetPresetContextName` (the secondary preset column).

### Group 3 — substantial new API + Lua
- Selected-preset info panel (vendor, author, date modified, comment, database).
- Per-preset favorites (read + toggle).
- Macro-parameter panel (bank picker + parameter sliders for the loaded FX).
- Destination panel (load-into track / FX slot, show chain / show FX,
  FX-window-behavior, name-track-after-preset).
- Associated products; stats panel; add project database.

### Group 4 — Preset Crawler (required)
Control surface over the existing Rust crawler: start a crawl with the captured mouse
positions (incl. the "Save Preset As" scraping option), poll progress/state, import or
discard the result. The crawl loop, mouse automation, and scraping stay in Rust
(`pot/src/preset_crawler.rs`); Lua drives the wizard UI.

### Group 5 — Preview Recorder (required)
Control surface over the existing Rust preview recorder: choose output mode, start,
poll progress, handle failures. Recording stays in Rust
(`pot/src/preview_recorder.rs`); Lua drives the wizard UI.

## Status
- Groups 1, 2 and 3: implemented (pending manual testing in REAPER).
- Groups 4 (crawler) and 5 (preview recorder): planned next.
