-- Pot Browser (Lua/ReaImGui)
--
-- Alternative UI for Helgobox's Pot preset browser engine, built on the HB_Pot_*
-- ReaScript API exposed by the standalone reaper_pot extension (or the full Helgobox
-- plugin). Requires ReaImGui (install via ReaPack).
--
-- All ReaImGui / REAPER calls are verified against the LuaCATS definitions in
-- vscode-reascript-extension/resources (imgui_defs_0.9.lua, reaper-types.lua).

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

-- Hierarchical filter kinds shown as fuzzy-search combos. `supports`-gated ones are only
-- shown when HB_Pot_SupportsFilter says they are relevant for the current results.
local FILTER_KINDS = {
  { id = 'database',     label = 'Database' },
  { id = 'product_kind', label = 'Product type' },
  { id = 'project',      label = 'Project',  gated = true },
  { id = 'bank',         label = 'FX',       gated = true },
  { id = 'sub_bank',     label = 'Bank',     gated = true },
  { id = 'category',     label = 'Type',     gated = true },
  { id = 'sub_category', label = 'Sub type', gated = true },
  { id = 'mode',         label = 'Character', gated = true },
}

-- Boolean-ish "mini" filter kinds, shown as a row of toggle buttons (like the original's
-- icon mini-filters).
local MINI_FILTERS = {
  { id = 'is_user',      label = 'User/Factory' },
  { id = 'is_favorite',  label = 'Favorite' },
  { id = 'is_supported', label = 'Supported' },
  { id = 'is_available', label = 'Available' },
  { id = 'has_preview',  label = 'Preview' },
}

local SEARCH_FIELDS = { 'Name', 'Product', 'Extension' } -- indices 0,1,2 for the API

local search_text = nil -- lazily initialized from the engine
local refreshed_once = false
local focus_search_on_next_frame = false
local auto_preview = true
local volume_before_mute = nil -- non-nil while muted

local filter_search_state = {}

-- Preset Crawler wizard state. `step`: intro | capture | crawling | stopped | importing
-- | done | failed. `points`: captured native screen coords in order
-- {next-preset, save-as button, cancel button}.
local crawler = {
  open = false,
  step = 'intro',
  stop_if_dest = false,
  never_stop = false,
  use_save_as = false,
  points = {},
  overlay_armed = false,
}

-- Preview Recorder wizard state. `step`: intro | preparing | ready | recording | done
-- | failed. `mode`: 0 = record for Pot Browser playback, 1 = export.
local recorder = {
  open = false,
  step = 'intro',
  mode = 0,
}

local FILTER_WIDTH = 170
local POPUP_WIDTH = 240
local LIST_HEIGHT = 200

-- Subsequence fuzzy match (unchanged): true if `search` is a subsequence of `name`, with
-- a score favoring consecutive matches and prefix matches.
local function fuzzy_match(search, name)
  if not search or search == '' then return true, 0 end
  search = search:lower()
  name = name:lower()
  local si, score, streak, first = 1, 0, 0, nil
  for i = 1, #name do
    if si <= #search and name:sub(i, i) == search:sub(si, si) then
      if not first then first = i end
      streak = streak + 1
      score = score + 1 + streak
      si = si + 1
    else
      streak = 0
    end
  end
  local matched = si > #search
  if matched then
    if first == 1 then score = score + 5 end
    score = score - (first or 0) * 0.1
  end
  return matched, score
end

local function get_filter_matches(kind_id, search)
  local count = r.HB_Pot_GetFilterItemCount(kind_id)
  if count < 0 then return {} end
  local matches = {}
  for i = 0, count - 1 do
    local ok, name = r.HB_Pot_GetFilterItemName(kind_id, i)
    if ok ~= 0 then
      if search == '' then
        matches[#matches + 1] = { index = i, name = name, score = 0 }
      else
        local matched, score = fuzzy_match(search, name)
        if matched then matches[#matches + 1] = { index = i, name = name, score = score } end
      end
    end
  end
  if search ~= '' then
    table.sort(matches, function(a, b)
      if a.score ~= b.score then return a.score > b.score end
      return a.index < b.index
    end)
  end
  return matches
end

local function typed_char()
  for code = string.byte('A'), string.byte('Z') do
    local letter = string.char(code)
    local key_func = r['ImGui_Key_' .. letter]
    if key_func and r.ImGui_IsKeyPressed(ctx, key_func(), false) then return letter:lower() end
  end
  for digit = 0, 9 do
    local main = r['ImGui_Key_' .. digit]
    local pad = r['ImGui_Key_Keypad' .. digit]
    if (main and r.ImGui_IsKeyPressed(ctx, main(), false))
        or (pad and r.ImGui_IsKeyPressed(ctx, pad(), false)) then
      return tostring(digit)
    end
  end
  return nil
end

-- One filter combo (trigger button + fuzzy-search popup). Returns an action payload or nil.
local function filter_combo(kind, state)
  local count = r.HB_Pot_GetFilterItemCount(kind.id)
  if count < 0 then return nil end

  local current = r.HB_Pot_GetFilter(kind.id)
  local preview = '<None>'
  if current >= 0 then
    local ok, name = r.HB_Pot_GetFilterItemName(kind.id, current)
    if ok ~= 0 then preview = name end
  end

  local popup_id = 'filter_popup_' .. kind.id
  local action = nil

  r.ImGui_AlignTextToFramePadding(ctx)
  r.ImGui_Text(ctx, kind.label)
  r.ImGui_SameLine(ctx)

  local open_popup = false
  if state.focus_button then
    r.ImGui_SetKeyboardFocusHere(ctx)
    state.focus_button = false
  end
  if r.ImGui_Button(ctx, preview .. '##filter_btn_' .. kind.id, FILTER_WIDTH, 0) then
    open_popup = true
  end
  local btn_min_x, btn_min_y = r.ImGui_GetItemRectMin(ctx)
  local btn_max_x, btn_max_y = r.ImGui_GetItemRectMax(ctx)
  local dl = r.ImGui_GetWindowDrawList(ctx)
  local col = r.ImGui_GetColor(ctx, r.ImGui_Col_Text())
  local cx = btn_max_x - 12
  local cy = (btn_min_y + btn_max_y) / 2
  r.ImGui_DrawList_AddTriangleFilled(dl, cx - 3, cy - 2, cx + 3, cy - 2, cx, cy + 2, col)
  if r.ImGui_IsItemFocused(ctx) then
    local mods = r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Ctrl())
        or r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Alt())
        or r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Super())
    local ch = (not mods) and typed_char() or nil
    if ch then
      state.search = ch
      open_popup = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_DownArrow(), false) then
      state.search = ''
      open_popup = true
    end
  end

  if open_popup then
    state.search = state.search or ''
    state.matches = get_filter_matches(kind.id, state.search)
    state.selected = state.search == '' and -1 or 0
    state.focus_input = true
    state.scroll_to_selected = false
    r.ImGui_OpenPopup(ctx, popup_id)
  end

  r.ImGui_SetNextWindowPos(ctx, btn_min_x, btn_max_y)
  r.ImGui_SetNextWindowSize(ctx, POPUP_WIDTH, 0)
  if r.ImGui_BeginPopup(ctx, popup_id) then
    local abort_to_search = r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Ctrl())
        and r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_F(), false)
    if abort_to_search then r.ImGui_CloseCurrentPopup(ctx) end
    if not abort_to_search and (state.focus_input or not state.input_active) then
      r.ImGui_SetKeyboardFocusHere(ctx)
      state.focus_input = false
    end
    r.ImGui_SetNextItemWidth(ctx, POPUP_WIDTH - 16)
    local changed, new_text = r.ImGui_InputText(ctx, '##search_' .. kind.id, state.search)
    state.input_active = r.ImGui_IsItemActive(ctx)
    if changed then
      state.search = new_text
      state.matches = get_filter_matches(kind.id, state.search)
      state.selected = state.search == '' and -1 or 0
      state.scroll_to_selected = false
    end

    local n = #state.matches
    local lowest = state.search == '' and -1 or 0
    local PAGE = 10
    if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_DownArrow(), false) then
      state.selected = math.min(state.selected + 1, n - 1); state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_UpArrow(), false) then
      state.selected = math.max(state.selected - 1, lowest); state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_PageDown(), false) then
      state.selected = math.min(state.selected + PAGE, n - 1); state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_PageUp(), false) then
      state.selected = math.max(state.selected - PAGE, lowest); state.scroll_to_selected = true
    end
    if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_Escape(), false) then
      state.focus_button = true
      r.ImGui_CloseCurrentPopup(ctx)
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_Enter(), false) then
      if state.search == '' then
        action = { action = 'set_filter', filter_id = kind.id, value = -1 }
      elseif n > 0 and state.selected >= 0 then
        action = { action = 'set_filter', filter_id = kind.id, value = state.matches[state.selected + 1].index }
      end
      state.focus_button = true
      r.ImGui_CloseCurrentPopup(ctx)
    end

    local list_flags = r.ImGui_WindowFlags_NoNav()
    if r.ImGui_BeginChild(ctx, 'filter_list_' .. kind.id, 0, LIST_HEIGHT, 0, list_flags) then
      for idx, match in ipairs(state.matches) do
        local is_selected = (idx - 1) == state.selected
        local excluded = r.HB_Pot_IsFilterItemExcluded(kind.id, match.index) ~= 0
        -- Excluded items are dimmed (like the original weakens them).
        if excluded then
          r.ImGui_PushStyleColor(ctx, r.ImGui_Col_Text(),
            r.ImGui_GetColor(ctx, r.ImGui_Col_TextDisabled()))
        end
        if r.ImGui_Selectable(ctx, match.name .. '##' .. match.index, is_selected) then
          action = { action = 'set_filter', filter_id = kind.id, value = match.index }
          r.ImGui_CloseCurrentPopup(ctx)
        end
        if excluded then r.ImGui_PopStyleColor(ctx) end
        -- Right-click: exclude/include this filter item globally.
        if r.ImGui_BeginPopupContextItem(ctx, 'fexcl_' .. kind.id .. '_' .. match.index) then
          local lbl = excluded and 'Include again (globally)' or 'Exclude (globally)'
          if r.ImGui_MenuItem(ctx, lbl) then
            r.HB_Pot_SetFilterItemExcluded(kind.id, match.index, excluded and 0 or 1)
          end
          r.ImGui_EndPopup(ctx)
        end
        if is_selected and state.scroll_to_selected then r.ImGui_SetScrollHereY(ctx, 0.5) end
      end
    end
    r.ImGui_EndChild(ctx)
    state.scroll_to_selected = false
    r.ImGui_EndPopup(ctx)
  end

  return action
end

-- Mini-filter: a small toggle button per item of a boolean-ish filter kind. Clicking an
-- item sets that filter; clicking the active one clears it.
local function mini_filter(kind)
  local count = r.HB_Pot_GetFilterItemCount(kind.id)
  if count <= 0 then return end
  local current = r.HB_Pot_GetFilter(kind.id)
  r.ImGui_Text(ctx, kind.label .. ':')
  for i = 0, count - 1 do
    r.ImGui_SameLine(ctx)
    local ok, name = r.HB_Pot_GetFilterItemName(kind.id, i)
    if ok == 0 then name = tostring(i) end
    local active = current == i
    if active then
      r.ImGui_PushStyleColor(ctx, r.ImGui_Col_Button(),
        r.ImGui_GetColor(ctx, r.ImGui_Col_ButtonActive()))
    end
    if r.ImGui_SmallButton(ctx, name .. '##mini_' .. kind.id .. '_' .. i) then
      r.HB_Pot_SetFilter(kind.id, active and -1 or i)
    end
    if active then r.ImGui_PopStyleColor(ctx) end
  end
end

local function db_label(permille)
  if permille <= 0 then return '-inf dB' end
  return string.format('%.1f dB', 20 * math.log(permille / 1000, 10))
end

local function reveal_or_copy(path)
  if path == nil or path == '' then return end
  if r.CF_LocateInExplorer then
    r.CF_LocateInExplorer(path)
  else
    -- No SWS: fall back to putting the path on the clipboard.
    r.ImGui_SetClipboardText(ctx, path)
  end
end

local function meta(i, field)
  local ok, v = r.HB_Pot_GetPresetMetadata(i, field)
  return (ok ~= 0) and v or ''
end

-- Info block for the currently selected preset: favorite toggle, name, source database
-- and product, and the metadata fields the original shows.
local function selected_preset_info()
  local sel = r.HB_Pot_GetSelectedPresetIndex()
  if sel < 0 then
    r.ImGui_TextDisabled(ctx, 'No preset selected')
    return
  end
  local fav = r.HB_Pot_IsPresetFavorite(sel) ~= 0
  if r.ImGui_Button(ctx, (fav and '\u{2605}' or '\u{2606}') .. '##fav') then
    r.HB_Pot_TogglePresetFavorite(sel)
  end
  r.ImGui_SameLine(ctx)
  local ok, name = r.HB_Pot_GetPresetName(sel)
  r.ImGui_Text(ctx, ok ~= 0 and name or '')

  local db = meta(sel, 'database')
  local _, product = r.HB_Pot_GetPresetProduct(sel)
  if db ~= '' then
    r.ImGui_SameLine(ctx); r.ImGui_TextDisabled(ctx, 'from ' .. db)
  end
  if product and product ~= '' then
    r.ImGui_SameLine(ctx); r.ImGui_TextDisabled(ctx, 'for ' .. product)
  end

  -- Secondary metadata line: only show the fields that are present.
  local parts = {}
  for _, fld in ipairs({ 'vendor', 'author', 'date', 'filesize' }) do
    local v = meta(sel, fld)
    if v ~= '' then parts[#parts + 1] = v end
  end
  if #parts > 0 then r.ImGui_TextDisabled(ctx, table.concat(parts, '   |   ')) end
  local comment = meta(sel, 'comment')
  if comment ~= '' then r.ImGui_TextWrapped(ctx, comment) end
end

local function preset_table()
  local count = r.HB_Pot_GetPresetCount()
  if count < 0 then
    r.ImGui_Text(ctx, 'No Pot unit available. Load the reaper_pot extension or a Helgobox instance.')
    return
  end
  local selected = r.HB_Pot_GetSelectedPresetIndex()
  local flags = r.ImGui_TableFlags_RowBg()
      | r.ImGui_TableFlags_BordersInnerV()
      | r.ImGui_TableFlags_ScrollY()
      | r.ImGui_TableFlags_Resizable()
  if r.ImGui_BeginTable(ctx, 'presets', 4, flags) then
    r.ImGui_TableSetupColumn(ctx, 'Name', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'FX', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'Ext', r.ImGui_TableColumnFlags_WidthFixed(), 50)
    r.ImGui_TableSetupColumn(ctx, 'Prev', r.ImGui_TableColumnFlags_WidthFixed(), 40)
    r.ImGui_TableSetupScrollFreeze(ctx, 0, 1)
    r.ImGui_TableHeadersRow(ctx)
    r.ImGui_ListClipper_Begin(clipper, count)
    while r.ImGui_ListClipper_Step(clipper) do
      local first, last = r.ImGui_ListClipper_GetDisplayRange(clipper)
      for i = first, last - 1 do
        r.ImGui_TableNextRow(ctx)
        r.ImGui_TableNextColumn(ctx)
        local ok, name = r.HB_Pot_GetPresetName(i)
        if ok == 0 then name = '...' end
        if r.ImGui_Selectable(ctx, name .. '##' .. i, i == selected,
              r.ImGui_SelectableFlags_SpanAllColumns()) then
          r.HB_Pot_SetSelectedPresetIndex(i)
          if auto_preview then r.HB_Pot_PlayPreview(i) end
        end
        if r.ImGui_IsItemHovered(ctx) and r.ImGui_IsMouseDoubleClicked(ctx, 0) then
          r.HB_Pot_LoadPreset(i)
        end
        -- Right-click: show preset / preview in file manager (or copy path without SWS).
        if r.ImGui_BeginPopupContextItem(ctx, 'pctx_' .. i) then
          local _, ppath = r.HB_Pot_GetPresetPath(i)
          local _, vpath = r.HB_Pot_GetPreviewPath(i)
          if ppath and ppath ~= '' and r.ImGui_MenuItem(ctx, 'Show preset in file manager') then
            reveal_or_copy(ppath)
          end
          if vpath and vpath ~= '' and r.ImGui_MenuItem(ctx, 'Show preview in file manager') then
            reveal_or_copy(vpath)
          end
          r.ImGui_EndPopup(ctx)
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

local function options_popup()
  if r.ImGui_Button(ctx, 'Options') then r.ImGui_OpenPopup(ctx, 'options') end
  if r.ImGui_BeginPopup(ctx, 'options') then
    r.ImGui_Text(ctx, 'Search fields')
    for idx, label in ipairs(SEARCH_FIELDS) do
      local field = idx - 1
      local on = r.HB_Pot_GetSearchField(field) ~= 0
      local changed, new_on = r.ImGui_Checkbox(ctx, label, on)
      if changed then r.HB_Pot_SetSearchField(field, new_on and 1 or 0) end
    end
    local wc = r.HB_Pot_GetUseWildcards() ~= 0
    local wc_changed, new_wc = r.ImGui_Checkbox(ctx, 'Wildcards (* and ?)', wc)
    if wc_changed then r.HB_Pot_SetUseWildcards(new_wc and 1 or 0) end
    r.ImGui_Separator(ctx)
    r.ImGui_Text(ctx, 'Load options')
    -- FX window behavior
    local wb = r.HB_Pot_GetLoadWindowBehavior()
    local _, wb_name = r.HB_Pot_GetLoadWindowBehaviorName(wb)
    r.ImGui_SetNextItemWidth(ctx, 280)
    if r.ImGui_BeginCombo(ctx, 'FX window', wb_name or '') then
      for i = 0, r.HB_Pot_GetLoadWindowBehaviorCount() - 1 do
        local _, n = r.HB_Pot_GetLoadWindowBehaviorName(i)
        if r.ImGui_Selectable(ctx, (n or tostring(i)) .. '##wb' .. i, i == wb) then
          r.HB_Pot_SetLoadWindowBehavior(i)
        end
      end
      r.ImGui_EndCombo(ctx)
    end
    local nt = r.HB_Pot_GetNameTrackAfterPreset() ~= 0
    local nt_changed, new_nt = r.ImGui_Checkbox(ctx, 'Name track after preset', nt)
    if nt_changed then r.HB_Pot_SetNameTrackAfterPreset(new_nt and 1 or 0) end
    r.ImGui_EndPopup(ctx)
  end
end

local macro_bank_index = 0

-- Macro-parameter panel for the preset loaded into the destination FX. Only shows once a
-- preset with parameter banks has been loaded (double-click a preset).
local function macro_panel()
  local bank_count = r.HB_Pot_GetMacroBankCount()
  if bank_count <= 0 then return end
  if macro_bank_index >= bank_count then macro_bank_index = 0 end
  if bank_count > 1 then
    local _, bname = r.HB_Pot_GetMacroBankName(macro_bank_index)
    r.ImGui_SetNextItemWidth(ctx, 200)
    if r.ImGui_BeginCombo(ctx, 'Bank', bname or '') then
      for b = 0, bank_count - 1 do
        local _, n = r.HB_Pot_GetMacroBankName(b)
        if r.ImGui_Selectable(ctx, (n or tostring(b)) .. '##mb' .. b, b == macro_bank_index) then
          macro_bank_index = b
        end
      end
      r.ImGui_EndCombo(ctx)
    end
  end
  local pcount = r.HB_Pot_GetMacroParamCount(macro_bank_index)
  for slot = 0, pcount - 1 do
    if slot > 0 then r.ImGui_SameLine(ctx) end
    r.ImGui_BeginGroup(ctx)
    local _, section = r.HB_Pot_GetMacroParamSection(macro_bank_index, slot)
    if section and section ~= '' then r.ImGui_TextDisabled(ctx, section) end
    local _, pname = r.HB_Pot_GetMacroParamName(macro_bank_index, slot)
    r.ImGui_Text(ctx, pname or '')
    local val = r.HB_Pot_GetMacroParamValue(macro_bank_index, slot)
    if val < 0 then
      r.ImGui_TextDisabled(ctx, '(n/a)')
    else
      local _, label = r.HB_Pot_GetMacroParamValueLabel(macro_bank_index, slot)
      -- A plug-in value label may contain '%', which SliderInt would treat as a printf
      -- specifier; escape it.
      label = (label or ''):gsub('%%', '%%%%')
      r.ImGui_SetNextItemWidth(ctx, 90)
      local changed, new_val = r.ImGui_SliderInt(ctx, '##mp' .. slot, val, 0, 1000, label)
      if changed then r.HB_Pot_SetMacroParamValue(macro_bank_index, slot, new_val) end
    end
    r.ImGui_EndGroup(ctx)
  end
end

-- "Load into <track> at <fx>" plus show-chain / show-fx buttons.
local function destination_panel()
  local function track_label(code)
    if code == -2 then return '<Selected track>' end
    if code == -1 then return '<Master track>' end
    local ok, name = r.HB_Pot_GetTrackName(code)
    return (ok ~= 0 and name) or ('Track ' .. (code + 1))
  end
  r.ImGui_AlignTextToFramePadding(ctx)
  r.ImGui_Text(ctx, 'Load into')
  r.ImGui_SameLine(ctx)
  local cur = r.HB_Pot_GetDestinationTrack()
  r.ImGui_SetNextItemWidth(ctx, 180)
  if r.ImGui_BeginCombo(ctx, '##desttrack', track_label(cur)) then
    if r.ImGui_Selectable(ctx, '<Selected track>', cur == -2) then r.HB_Pot_SetDestinationTrack(-2) end
    if r.ImGui_Selectable(ctx, '<Master track>', cur == -1) then r.HB_Pot_SetDestinationTrack(-1) end
    for i = 0, r.HB_Pot_GetTrackCount() - 1 do
      if r.ImGui_Selectable(ctx, track_label(i) .. '##t' .. i, cur == i) then
        r.HB_Pot_SetDestinationTrack(i)
      end
    end
    r.ImGui_EndCombo(ctx)
  end

  r.ImGui_SameLine(ctx)
  r.ImGui_Text(ctx, 'at')
  r.ImGui_SameLine(ctx)
  local fx_count = r.HB_Pot_GetDestinationFxCount()
  local fx_idx = r.HB_Pot_GetDestinationFxIndex()
  local function fx_label(idx)
    if idx >= fx_count then return '<New FX>' end
    local ok, name = r.HB_Pot_GetDestinationFxName(idx)
    return (ok ~= 0 and ((idx + 1) .. '. ' .. name)) or ('FX ' .. (idx + 1))
  end
  r.ImGui_SetNextItemWidth(ctx, 160)
  if r.ImGui_BeginCombo(ctx, '##destfx', fx_label(fx_idx)) then
    for i = 0, fx_count do
      local label = (i < fx_count) and fx_label(i) or '<New FX>'
      if r.ImGui_Selectable(ctx, label .. '##fx' .. i, i == fx_idx) then
        r.HB_Pot_SetDestinationFxIndex(i)
      end
    end
    r.ImGui_EndCombo(ctx)
  end

  r.ImGui_SameLine(ctx)
  if r.ImGui_SmallButton(ctx, 'Chain') then r.HB_Pot_ShowDestinationChain() end
  r.ImGui_SameLine(ctx)
  if r.ImGui_SmallButton(ctx, 'FX') then r.HB_Pot_ShowDestinationFx() end
end

local function toolbar()
  if r.ImGui_Button(ctx, 'Refresh') then r.HB_Pot_Refresh() end
  r.ImGui_SameLine(ctx)
  if r.ImGui_Button(ctx, 'Stop') then r.HB_Pot_StopPreview() end
  r.ImGui_SameLine(ctx)

  if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_F(), false) and r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Ctrl()) then
    focus_search_on_next_frame = true
  end
  if search_text == nil then
    local _, current = r.HB_Pot_GetSearchText()
    search_text = current or ''
  end
  r.ImGui_SetNextItemWidth(ctx, 220)
  if focus_search_on_next_frame then
    r.ImGui_SetKeyboardFocusHere(ctx)
    focus_search_on_next_frame = false
  end
  local changed, new_text = r.ImGui_InputText(ctx, 'Search', search_text)
  if changed then
    search_text = new_text
    r.HB_Pot_SetSearchText(new_text)
  end
  r.ImGui_SameLine(ctx)
  options_popup()

  -- Preview volume + mute
  r.ImGui_SameLine(ctx)
  local vol = r.HB_Pot_GetPreviewVolume()
  if vol >= 0 then
    r.ImGui_SetNextItemWidth(ctx, 110)
    local vchanged, new_vol = r.ImGui_SliderInt(ctx, '##vol', vol, 0, 1000, db_label(vol))
    if vchanged then
      r.HB_Pot_SetPreviewVolume(new_vol)
      volume_before_mute = nil
    end
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
  r.ImGui_SameLine(ctx)
  local apc, ap = r.ImGui_Checkbox(ctx, 'Auto-preview', auto_preview)
  if apc then auto_preview = ap end
  -- Preset Crawler (only when the standalone extension exposes the crawler API).
  if r.HB_Pot_CrawlerStart then
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Crawler') then
      crawler.open = true
      crawler.step = 'intro'
    end
  end
  -- Preview Recorder (only when the standalone extension exposes the recorder API).
  if r.HB_Pot_RecorderPrepare then
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Recorder') then
      recorder.open = true
      recorder.step = 'intro'
    end
  end
  if r.HB_Pot_IsBusy() ~= 0 then
    r.ImGui_SameLine(ctx)
    r.ImGui_Text(ctx, 'scanning...')
  end
end

-- Order of the screen positions the crawler needs to capture by click.
local CRAWL_CAPTURE_LABELS = {
  'the plug-in\'s "Next preset" button',
  'the plug-in\'s "Save Preset As" button',
  'the Save-As dialog\'s "Cancel" button',
}

local function crawler_points_needed()
  return crawler.use_save_as and 3 or 1
end

-- Full-screen, borderless, top-most, transparent overlay that captures click positions in
-- native (OS) screen coordinates -- the coordinate space the Rust mouse automation uses.
local function crawler_overlay()
  local vp = r.ImGui_GetMainViewport(ctx)
  local vx, vy = r.ImGui_Viewport_GetPos(vp)
  local vw, vh = r.ImGui_Viewport_GetSize(vp)
  r.ImGui_SetNextWindowPos(ctx, vx, vy)
  r.ImGui_SetNextWindowSize(ctx, vw, vh)
  local flags = r.ImGui_WindowFlags_NoDecoration()
      | r.ImGui_WindowFlags_NoMove()
      | r.ImGui_WindowFlags_NoResize()
      | r.ImGui_WindowFlags_NoSavedSettings()
      | r.ImGui_WindowFlags_NoNav()
      | r.ImGui_WindowFlags_NoBackground()
      | r.ImGui_WindowFlags_TopMost()
  if r.ImGui_Begin(ctx, '##crawl_overlay', true, flags) then
    local dl = r.ImGui_GetWindowDrawList(ctx)
    r.ImGui_DrawList_AddRectFilled(dl, vx, vy, vx + vw, vy + vh, 0x10183050)
    local mx, my = r.ImGui_GetMousePos(ctx)
    r.ImGui_DrawList_AddLine(dl, mx - 14, my, mx + 14, my, 0xFFEE00FF, 1.5)
    r.ImGui_DrawList_AddLine(dl, mx, my - 14, mx, my + 14, 0xFFEE00FF, 1.5)
    local idx = #crawler.points + 1
    r.ImGui_DrawList_AddText(dl, vx + 24, vy + 24, 0xFFFFFFFF,
      'Click ' .. (CRAWL_CAPTURE_LABELS[idx] or 'the target') .. '   (Esc to cancel capture)')
    if r.ImGui_IsMouseClicked(ctx, 0) then
      local nx, ny = r.ImGui_PointConvertNative(ctx, mx, my, true)
      crawler.points[#crawler.points + 1] = { math.floor(nx + 0.5), math.floor(ny + 0.5) }
      if #crawler.points >= crawler_points_needed() then crawler.overlay_armed = false end
    end
    if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_Escape(), false) then
      crawler.overlay_armed = false
    end
    r.ImGui_End(ctx)
  end
end

local function crawler_close()
  r.HB_Pot_CrawlerDiscard()
  crawler.open = false
end

local function crawler_render()
  if not crawler.open then return end
  -- The capture overlay sits above everything while armed.
  if crawler.step == 'capture' and crawler.overlay_armed then crawler_overlay() end

  r.ImGui_SetNextWindowSize(ctx, 480, 340, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Preset Crawler', true)
  if not open then crawler.open = false end
  if not visible then return end

  local step = crawler.step
  if step == 'intro' then
    r.ImGui_TextWrapped(ctx,
      'The crawler steps a plug-in through its presets by clicking its "Next preset" '
      .. 'button and saving each one. Open the plug-in in a FLOATING FX window first, then '
      .. 'capture the button positions.')
    r.ImGui_Separator(ctx)
    local fxok, fxname = r.HB_Pot_GetFocusedFxName()
    local floating = r.HB_Pot_IsFocusedFxOpenFloating() ~= 0
    if fxok ~= 0 then
      r.ImGui_Text(ctx, 'Focused FX: ' .. fxname)
    else
      r.ImGui_TextDisabled(ctx, 'No FX focused.')
    end
    if not floating then
      r.ImGui_TextColored(ctx, 0xFF6060FF, 'The focused FX must be open in a floating window.')
    end
    r.ImGui_Separator(ctx)
    local c1, v1 = r.ImGui_Checkbox(ctx, 'Stop if a destination file already exists', crawler.stop_if_dest)
    if c1 then crawler.stop_if_dest = v1 end
    local c2, v2 = r.ImGui_Checkbox(ctx, 'Never stop automatically (crawl until Escape)', crawler.never_stop)
    if c2 then crawler.never_stop = v2 end
    local c3, v3 = r.ImGui_Checkbox(ctx, 'Scrape names from the "Save Preset As" dialog', crawler.use_save_as)
    if c3 then crawler.use_save_as = v3 end
    r.ImGui_Separator(ctx)
    if r.ImGui_Button(ctx, 'Cancel') then crawler.open = false end
    r.ImGui_SameLine(ctx)
    if not floating then r.ImGui_BeginDisabled(ctx) end
    if r.ImGui_Button(ctx, 'Capture positions') then
      crawler.points = {}
      crawler.overlay_armed = true
      crawler.step = 'capture'
    end
    if not floating then r.ImGui_EndDisabled(ctx) end
  elseif step == 'capture' then
    r.ImGui_TextWrapped(ctx, 'Capturing screen positions by click. The overlay covers the '
      .. 'screen; click each requested target. Re-capture if you mis-clicked.')
    r.ImGui_Text(ctx, string.format('Captured %d / %d', #crawler.points, crawler_points_needed()))
    for i, p in ipairs(crawler.points) do
      r.ImGui_Text(ctx, string.format('  %d. %s  ->  (%d, %d)',
        i, CRAWL_CAPTURE_LABELS[i] or '?', p[1], p[2]))
    end
    if not crawler.overlay_armed then
      if r.ImGui_Button(ctx, 'Re-capture') then
        crawler.points = {}
        crawler.overlay_armed = true
      end
      r.ImGui_SameLine(ctx)
      if #crawler.points >= crawler_points_needed() then
        if r.ImGui_Button(ctx, 'Start crawling') then
          local p = crawler.points
          local sx, sy = (p[2] and p[2][1] or 0), (p[2] and p[2][2] or 0)
          local cx, cy = (p[3] and p[3][1] or 0), (p[3] and p[3][2] or 0)
          local started = r.HB_Pot_CrawlerStart(p[1][1], p[1][2],
            crawler.stop_if_dest and 1 or 0, crawler.never_stop and 1 or 0,
            crawler.use_save_as and 1 or 0, sx, sy, cx, cy)
          crawler.step = (started ~= 0) and 'crawling' or 'failed'
        end
        r.ImGui_SameLine(ctx)
      end
      if r.ImGui_Button(ctx, 'Back') then crawler.step = 'intro' end
    end
  elseif step == 'crawling' then
    r.ImGui_Text(ctx, 'Crawling... press Escape (over the plug-in) to stop.')
    r.ImGui_Separator(ctx)
    r.ImGui_Text(ctx, string.format('Presets crawled: %d', r.HB_Pot_CrawlerPresetCount()))
    r.ImGui_Text(ctx, string.format('Skipped (duplicate name): %d', r.HB_Pot_CrawlerDuplicateCount()))
    local lok, lname = r.HB_Pot_CrawlerLastPresetName()
    r.ImGui_Text(ctx, 'Last crawled: ' .. ((lok ~= 0) and lname or '-'))
    local phase = r.HB_Pot_CrawlerPhase()
    if phase == 2 then crawler.step = 'stopped'
    elseif phase == 5 then crawler.step = 'failed' end
  elseif step == 'stopped' then
    r.ImGui_Text(ctx, string.format('Crawled %d presets.', r.HB_Pot_CrawlerCrawledCount()))
    local sok, slabel = r.HB_Pot_CrawlerStopReasonLabel()
    if sok ~= 0 and slabel ~= '' then r.ImGui_TextWrapped(ctx, slabel) end
    r.ImGui_Separator(ctx)
    if r.ImGui_Button(ctx, 'Import') then
      crawler.step = (r.HB_Pot_CrawlerImport() ~= 0) and 'importing' or 'failed'
    end
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Discard') then crawler_close() end
  elseif step == 'importing' then
    r.ImGui_Text(ctx, 'Importing crawled presets...')
    local phase = r.HB_Pot_CrawlerPhase()
    if phase == 4 then crawler.step = 'done'
    elseif phase == 5 then crawler.step = 'failed' end
  elseif step == 'done' then
    r.ImGui_Text(ctx, string.format('Imported %d presets.', r.HB_Pot_CrawlerCrawledCount()))
    if r.ImGui_Button(ctx, 'Close') then crawler_close() end
  elseif step == 'failed' then
    r.ImGui_TextColored(ctx, 0xFF6060FF, 'Crawler failed or was cancelled.')
    local eok, emsg = r.HB_Pot_CrawlerError()
    if eok ~= 0 and emsg ~= '' then r.ImGui_TextWrapped(ctx, emsg) end
    if r.ImGui_Button(ctx, 'Close') then crawler_close() end
  end
  r.ImGui_End(ctx)
end

local function recorder_close()
  r.HB_Pot_RecorderDiscard()
  recorder.open = false
end

local function recorder_render()
  if not recorder.open then return end
  r.ImGui_SetNextWindowSize(ctx, 480, 360, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Preview Recorder', true)
  if not open then recorder.open = false end
  if not visible then return end

  local step = recorder.step
  if step == 'intro' then
    r.ImGui_TextWrapped(ctx,
      'The recorder loads each matching instrument preset, renders a short audio preview, '
      .. 'and saves it. Use "playback" to fill in previews missing from the browser, or '
      .. '"export" to render previews to a folder.')
    r.ImGui_Separator(ctx)
    if r.ImGui_Button(ctx, 'Cancel') then recorder.open = false end
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Record for Pot Browser playback') then
      recorder.mode = 0
      recorder.step = (r.HB_Pot_RecorderPrepare(0) ~= 0) and 'preparing' or 'failed'
    end
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Record and export') then
      recorder.mode = 1
      recorder.step = (r.HB_Pot_RecorderPrepare(1) ~= 0) and 'preparing' or 'failed'
    end
  elseif step == 'preparing' then
    r.ImGui_Text(ctx, 'Gathering presets...')
    local phase = r.HB_Pot_RecorderPhase()
    if phase == 2 then recorder.step = 'ready'
    elseif phase == 5 then recorder.step = 'failed' end
  elseif step == 'ready' then
    r.ImGui_Text(ctx, string.format('Ready to record %d presets.', r.HB_Pot_RecorderPreparedCount()))
    r.ImGui_TextWrapped(ctx, 'Recording opens a temporary project and renders each preset. '
      .. 'Leave REAPER alone while it runs; press Escape to stop early.')
    r.ImGui_Separator(ctx)
    if r.ImGui_Button(ctx, 'Start recording') then
      recorder.step = (r.HB_Pot_RecorderStart() ~= 0) and 'recording' or 'failed'
    end
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Cancel') then recorder_close() end
  elseif step == 'recording' then
    r.ImGui_Text(ctx, 'Recording previews... (press Escape to stop)')
    r.ImGui_Separator(ctx)
    r.ImGui_Text(ctx, string.format('Presets left: %d', r.HB_Pot_RecorderTodoCount()))
    r.ImGui_Text(ctx, string.format('Failures: %d', r.HB_Pot_RecorderFailureCount()))
    local phase = r.HB_Pot_RecorderPhase()
    if phase == 4 then recorder.step = 'done'
    elseif phase == 5 then recorder.step = 'failed' end
  elseif step == 'done' then
    r.ImGui_Text(ctx, 'Recording finished.')
    local left = r.HB_Pot_RecorderTodoCount()
    if left > 0 then r.ImGui_Text(ctx, string.format('Not recorded (stopped early): %d', left)) end
    local fcount = r.HB_Pot_RecorderFailureCount()
    r.ImGui_Text(ctx, string.format('Failures: %d', fcount))
    if recorder.mode == 1 then
      local dok, dir = r.HB_Pot_RecorderExportDir()
      if dok ~= 0 then r.ImGui_TextWrapped(ctx, 'Exported to: ' .. dir) end
    end
    if fcount > 0 then
      r.ImGui_Separator(ctx)
      if r.ImGui_BeginChild(ctx, 'rec_failures', 0, 140) then
        for i = 0, fcount - 1 do
          local nok, name = r.HB_Pot_RecorderFailureName(i)
          local rok, reason = r.HB_Pot_RecorderFailureReason(i)
          r.ImGui_TextWrapped(ctx, string.format('%s  -  %s',
            (nok ~= 0) and name or '?', (rok ~= 0) and reason or ''))
        end
      end
      r.ImGui_EndChild(ctx)
    end
    if r.ImGui_Button(ctx, 'Close') then recorder_close() end
  elseif step == 'failed' then
    r.ImGui_TextColored(ctx, 0xFF6060FF, 'Preview recorder stopped.')
    local eok, emsg = r.HB_Pot_RecorderError()
    if eok ~= 0 and emsg ~= '' then r.ImGui_TextWrapped(ctx, emsg) end
    if r.ImGui_Button(ctx, 'Close') then recorder_close() end
  end
  r.ImGui_End(ctx)
end

local function frame()
  toolbar()
  destination_panel()
  macro_panel()
  -- Hierarchical filters (gated ones only when relevant + non-empty)
  local shown = 0
  for _, kind in ipairs(FILTER_KINDS) do
    local show = true
    if kind.gated and r.HB_Pot_SupportsFilter(kind.id) == 0 then show = false end
    if show and r.HB_Pot_GetFilterItemCount(kind.id) <= 0 then show = false end
    if show then
      if not filter_search_state[kind.id] then
        filter_search_state[kind.id] = { search = '', matches = {}, selected = -1 }
      end
      local action = filter_combo(kind, filter_search_state[kind.id])
      if action and action.action == 'set_filter' then
        r.HB_Pot_SetFilter(action.filter_id, action.value)
      end
      r.ImGui_SameLine(ctx)
      shown = shown + 1
    end
  end
  if shown > 0 then r.ImGui_NewLine(ctx) end
  -- Mini-filters
  for _, kind in ipairs(MINI_FILTERS) do
    mini_filter(kind)
    r.ImGui_SameLine(ctx)
  end
  r.ImGui_NewLine(ctx)
  r.ImGui_Separator(ctx)
  selected_preset_info()
  r.ImGui_Separator(ctx)
  preset_table()
end

local function loop()
  -- Pump the standalone extension's async executor so the crawler/recorder wizards make
  -- progress. No-op on older binaries without the API.
  if r.HB_Pot_RunTasks then r.HB_Pot_RunTasks() end
  -- The extension scans the databases at REAPER startup. Only trigger a scan ourselves
  -- if, by the time the window first opens, the engine is neither already scanning nor
  -- populated (i.e. the startup warm-up didn't run) - this avoids a redundant rescan.
  if not refreshed_once and r.HB_Pot_IsAvailable() ~= 0 then
    if r.HB_Pot_IsBusy() == 0 and r.HB_Pot_GetPresetCount() <= 0 then
      r.HB_Pot_Refresh()
    end
    refreshed_once = true
  end
  r.ImGui_SetNextWindowSize(ctx, 820, 560, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Pot Browser (Lua)', true)
  if visible then
    frame()
    r.ImGui_End(ctx)
  end
  -- The wizards are their own windows, drawn outside the main window's scope.
  if crawler.open then crawler_render() end
  if recorder.open then recorder_render() end
  if open then r.defer(loop) end
end

r.defer(loop)
