-- Pot Browser (Lua/ReaImGui demo)
--
-- A minimal alternative UI for Helgobox's Pot preset browser engine, built on the
-- HB_Pot_* ReaScript API. Requires:
--   - Helgobox (with the HB_Pot_* API, i.e. this fork)
--   - ReaImGui (install via ReaPack)
--   - At least one Helgobox/ReaLearn instance in the project
--
-- This is deliberately small: it's the starting point for UX iteration, not a finished
-- design. Edit, save, re-run the action - no recompilation, no REAPER restart.

local r = reaper

if not r.ImGui_CreateContext then
  r.MB('This script requires ReaImGui (install via ReaPack).', 'Pot Browser', 0)
  return
end
if not r.HB_Pot_IsAvailable then
  r.MB('This script requires Helgobox with the HB_Pot_* API.', 'Pot Browser', 0)
  return
end

local ctx = r.ImGui_CreateContext('Pot Browser (Lua)')
local clipper = r.ImGui_CreateListClipper(ctx)
r.ImGui_Attach(ctx, clipper)

-- Filter kinds shown as combos, in order. Extend freely - any kind name from the API
-- docs works: database, bank, sub_bank, category, sub_category, mode, product_kind,
-- is_user, is_favorite, has_preview, is_available, is_supported, project.
local FILTER_KINDS = {
  { id = 'database', label = 'Database' },
  { id = 'bank', label = 'Product' },
  { id = 'category', label = 'Type' },
  { id = 'mode', label = 'Character' },
}

local search_text = nil -- lazily initialized from the engine
local auto_preview = true
local volume_before_mute = nil -- non-nil while muted

-- Theme handling: 'auto' follows REAPER's appearance (via HB_Pot_DarkModeEnabled),
-- 'light'/'dark' are explicit choices. Persisted across restarts via ExtState.
local THEME_EXT_SECTION, THEME_EXT_KEY = 'pot_browser_lua', 'theme'
local THEME_CHOICES = { 'auto', 'light', 'dark' }
local theme_choice = r.GetExtState(THEME_EXT_SECTION, THEME_EXT_KEY)
if theme_choice ~= 'light' and theme_choice ~= 'dark' then theme_choice = 'auto' end
local applied_dark = nil -- which palette is currently applied

local function effective_dark()
  if theme_choice == 'dark' then return true end
  if theme_choice == 'light' then return false end
  return r.HB_Pot_DarkModeEnabled() ~= 0
end

local function apply_theme_if_needed()
  local dark = effective_dark()
  if dark == applied_dark then return end
  applied_dark = dark
  if dark then
    r.ImGui_StyleColorsDark(ctx)
  else
    r.ImGui_StyleColorsLight(ctx)
  end
end

local function filter_combo(kind)
  local count = r.HB_Pot_GetFilterItemCount(kind.id)
  if count < 0 then return end
  local current = r.HB_Pot_GetFilter(kind.id)
  local current_label = '<Any>'
  if current >= 0 then
    local ok, name = r.HB_Pot_GetFilterItemName(kind.id, current)
    if ok ~= 0 then current_label = name end
  end
  r.ImGui_SetNextItemWidth(ctx, 170)
  if r.ImGui_BeginCombo(ctx, kind.label, current_label) then
    if r.ImGui_Selectable(ctx, '<Any>', current < 0) then
      r.HB_Pot_SetFilter(kind.id, -1)
    end
    for i = 0, count - 1 do
      local ok, name = r.HB_Pot_GetFilterItemName(kind.id, i)
      if ok ~= 0 then
        if r.ImGui_Selectable(ctx, name .. '##' .. i, i == current) then
          r.HB_Pot_SetFilter(kind.id, i)
        end
      end
    end
    r.ImGui_EndCombo(ctx)
  end
end

local function preset_table()
  local count = r.HB_Pot_GetPresetCount()
  if count < 0 then
    r.ImGui_Text(ctx, 'No Helgobox instance found. Add ReaLearn to a track first.')
    return
  end
  local selected = r.HB_Pot_GetSelectedPresetIndex()
  local flags = r.ImGui_TableFlags_RowBg()
      | r.ImGui_TableFlags_BordersInnerV()
      | r.ImGui_TableFlags_ScrollY()
  if r.ImGui_BeginTable(ctx, 'presets', 4, flags) then
    r.ImGui_TableSetupColumn(ctx, 'Name', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'Product', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'Ext', r.ImGui_TableColumnFlags_WidthFixed(), 50)
    r.ImGui_TableSetupColumn(ctx, 'Prev', r.ImGui_TableColumnFlags_WidthFixed(), 40)
    r.ImGui_TableSetupScrollFreeze(ctx, 0, 1)
    r.ImGui_TableHeadersRow(ctx)
    -- The clipper means only visible rows query the API - lists with tens of
    -- thousands of presets stay cheap.
    r.ImGui_ListClipper_Begin(clipper, count)
    while r.ImGui_ListClipper_Step(clipper) do
      local first, last = r.ImGui_ListClipper_GetDisplayRange(clipper)
      for i = first, last - 1 do
        r.ImGui_TableNextRow(ctx)
        r.ImGui_TableNextColumn(ctx)
        local ok, name = r.HB_Pot_GetPresetName(i)
        if ok == 0 then name = '...' end
        local row_flags = r.ImGui_SelectableFlags_SpanAllColumns()
        if r.ImGui_Selectable(ctx, name .. '##' .. i, i == selected, row_flags) then
          r.HB_Pot_SetSelectedPresetIndex(i)
          if auto_preview then
            r.HB_Pot_PlayPreview(i)
          end
        end
        if r.ImGui_IsItemHovered(ctx) and r.ImGui_IsMouseDoubleClicked(ctx, 0) then
          r.HB_Pot_LoadPreset(i)
        end
        r.ImGui_TableNextColumn(ctx)
        local ok2, product = r.HB_Pot_GetPresetProduct(i)
        r.ImGui_Text(ctx, ok2 ~= 0 and product or '')
        r.ImGui_TableNextColumn(ctx)
        local ok3, ext = r.HB_Pot_GetPresetFileExt(i)
        r.ImGui_Text(ctx, ok3 ~= 0 and ext or '')
        r.ImGui_TableNextColumn(ctx)
        r.ImGui_Text(ctx, r.HB_Pot_HasPreview(i) ~= 0 and '\u{266A}' or '')
      end
    end
    r.ImGui_EndTable(ctx)
  end
end

local function frame()
  -- Toolbar
  if r.ImGui_Button(ctx, 'Refresh') then
    r.HB_Pot_Refresh()
  end
  r.ImGui_SameLine(ctx)
  if r.ImGui_Button(ctx, 'Stop') then
    r.HB_Pot_StopPreview()
  end
  r.ImGui_SameLine(ctx)
  if search_text == nil then
    local _, current = r.HB_Pot_GetSearchText()
    search_text = current or ''
  end
  r.ImGui_SetNextItemWidth(ctx, 250)
  local changed, new_text = r.ImGui_InputText(ctx, 'Search', search_text)
  if changed then
    search_text = new_text
    r.HB_Pot_SetSearchText(new_text)
  end
  -- Preview volume (engine stores raw gain permille; display as dB)
  r.ImGui_SameLine(ctx)
  r.ImGui_SetNextItemWidth(ctx, 120)
  local vol = r.HB_Pot_GetPreviewVolume()
  if vol >= 0 then
    local db_label = vol == 0 and '-inf dB'
        or string.format('%.1f dB', 20 * math.log(vol / 1000, 10))
    local vol_changed, new_vol = r.ImGui_SliderInt(ctx, 'Vol', vol, 0, 1000, db_label)
    if vol_changed then
      r.HB_Pot_SetPreviewVolume(new_vol)
      volume_before_mute = nil -- manual change unmutes
    end
    -- Mute toggle: remembers the volume and restores it on unmute
    r.ImGui_SameLine(ctx)
    local muted = volume_before_mute ~= nil
    if r.ImGui_Button(ctx, muted and 'Unmute' or 'Mute') then
      if muted then
        r.HB_Pot_SetPreviewVolume(volume_before_mute)
        volume_before_mute = nil
      else
        volume_before_mute = vol
        r.HB_Pot_SetPreviewVolume(0)
      end
    end
  end
  -- Auto-preview toggle: when off, clicking a preset only selects it
  r.ImGui_SameLine(ctx)
  local ap_changed, ap = r.ImGui_Checkbox(ctx, 'Auto-preview', auto_preview)
  if ap_changed then auto_preview = ap end
  -- Theme selector (persisted)
  r.ImGui_SameLine(ctx)
  r.ImGui_SetNextItemWidth(ctx, 70)
  if r.ImGui_BeginCombo(ctx, 'Theme', theme_choice) then
    for _, choice in ipairs(THEME_CHOICES) do
      if r.ImGui_Selectable(ctx, choice, choice == theme_choice) then
        theme_choice = choice
        r.SetExtState(THEME_EXT_SECTION, THEME_EXT_KEY, choice, true)
      end
    end
    r.ImGui_EndCombo(ctx)
  end
  if r.HB_Pot_IsBusy() ~= 0 then
    r.ImGui_SameLine(ctx)
    r.ImGui_Text(ctx, 'scanning...')
  end
  -- Filters
  for _, kind in ipairs(FILTER_KINDS) do
    filter_combo(kind)
    r.ImGui_SameLine(ctx)
  end
  -- Has-preview mini filter as toggle buttons (like the egui browser's mini filters).
  -- Item 0 = "No preview", item 1 = "Has preview" (see create_filter_items_has_preview).
  local hp_current = r.HB_Pot_GetFilter('has_preview')
  local function hp_toggle(label, item_index)
    local active = hp_current == item_index
    if active then
      r.ImGui_PushStyleColor(ctx, r.ImGui_Col_Button(),
        r.ImGui_GetStyleColor(ctx, r.ImGui_Col_ButtonActive()))
    end
    if r.ImGui_Button(ctx, label) then
      r.HB_Pot_SetFilter('has_preview', active and -1 or item_index)
    end
    if active then r.ImGui_PopStyleColor(ctx) end
  end
  hp_toggle('No prev', 0)
  r.ImGui_SameLine(ctx)
  hp_toggle('Has prev', 1)
  r.ImGui_NewLine(ctx)
  r.ImGui_Separator(ctx)
  -- Preset list
  preset_table()
end

local function loop()
  -- No initial refresh needed: Helgobox warms up the preset databases in the
  -- background at startup.
  apply_theme_if_needed()
  r.ImGui_SetNextWindowSize(ctx, 700, 500, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Pot Browser (Lua)', true)
  if visible then
    frame()
    r.ImGui_End(ctx)
  end
  if open then
    r.defer(loop)
  end
end

r.defer(loop)
