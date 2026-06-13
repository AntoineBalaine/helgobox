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

local function search_options_popup()
  if r.ImGui_Button(ctx, 'Options') then r.ImGui_OpenPopup(ctx, 'search_options') end
  if r.ImGui_BeginPopup(ctx, 'search_options') then
    r.ImGui_Text(ctx, 'Search fields')
    for idx, label in ipairs(SEARCH_FIELDS) do
      local field = idx - 1
      local on = r.HB_Pot_GetSearchField(field) ~= 0
      local changed, new_on = r.ImGui_Checkbox(ctx, label, on)
      if changed then r.HB_Pot_SetSearchField(field, new_on and 1 or 0) end
    end
    r.ImGui_Separator(ctx)
    local wc = r.HB_Pot_GetUseWildcards() ~= 0
    local wc_changed, new_wc = r.ImGui_Checkbox(ctx, 'Wildcards (* and ?)', wc)
    if wc_changed then r.HB_Pot_SetUseWildcards(new_wc and 1 or 0) end
    r.ImGui_EndPopup(ctx)
  end
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
  search_options_popup()

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
  if r.HB_Pot_IsBusy() ~= 0 then
    r.ImGui_SameLine(ctx)
    r.ImGui_Text(ctx, 'scanning...')
  end
end

local function frame()
  toolbar()
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
  preset_table()
end

local function loop()
  if not refreshed_once and r.HB_Pot_IsAvailable() ~= 0 then
    refreshed_once = true
    r.HB_Pot_Refresh()
  end
  r.ImGui_SetNextWindowSize(ctx, 820, 560, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Pot Browser (Lua)', true)
  if visible then
    frame()
    r.ImGui_End(ctx)
  end
  if open then r.defer(loop) end
end

r.defer(loop)
