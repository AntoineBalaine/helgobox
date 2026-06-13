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
  { id = 'bank', label = 'FX' },
  { id = 'category', label = 'Type' },
  { id = 'mode', label = 'Character' },
}

local search_text = nil -- lazily initialized from the engine
local refreshed_once = false
local focus_search_on_next_frame = false

local filter_search_state = {}

local FILTER_WIDTH = 170    -- width of the closed trigger button
local POPUP_WIDTH = 240     -- width of the open fuzzy-search popup
local LIST_HEIGHT = 200     -- height of the scrolling result list

-- Subsequence fuzzy match. Returns whether `search` is a subsequence of `name`
-- and a score where higher is better: consecutive matches and a prefix match are
-- rewarded, an earlier first-match position is mildly preferred.
local function fuzzy_match(search, name)
  if not search or search == '' then return true, 0 end
  search = search:lower()
  name = name:lower()
  local si = 1
  local score = 0
  local streak = 0
  local first = nil
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

-- Builds the list of items to show for a filter given the search string. Empty
-- search returns all items in natural order; otherwise only fuzzy matches, sorted
-- by score (ties broken by natural order).
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
        if matched then
          matches[#matches + 1] = { index = i, name = name, score = score }
        end
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

-- Returns the lowercased character of an alphanumeric key pressed this frame, or
-- nil. Used to open a focused filter into search mode by typing. Uses edge-detected
-- IsKeyPressed so a held key fires once.
local function typed_char()
  for code = string.byte('A'), string.byte('Z') do
    local letter = string.char(code)
    local key_func = r['ImGui_Key_' .. letter]
    if key_func and r.ImGui_IsKeyPressed(ctx, key_func(), false) then
      return letter:lower()
    end
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

-- Renders one filter as a combo-like trigger button plus a fuzzy-search popup.
-- Pure render: it only reads engine state and mutates its own `state` table (the
-- transient search buffer / selection / focus flags). Engine-affecting interactions
-- are returned as an action payload for the caller to apply; nil means no change.
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

  -- Label sits to the left of the trigger button
  r.ImGui_AlignTextToFramePadding(ctx)
  r.ImGui_Text(ctx, kind.label)
  r.ImGui_SameLine(ctx)

  -- Trigger button (stable width keeps the toolbar layout from shifting). After the
  -- popup closes via the keyboard we deterministically return focus here, so Tab can
  -- continue to the next filter (ImGui's implicit popup focus restoration is unreliable).
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
  -- Draw a downward chevron at the right edge so the control reads as a dropdown
  local dl = r.ImGui_GetWindowDrawList(ctx)
  local col = r.ImGui_GetColor(ctx, r.ImGui_Col_Text())
  local cx = btn_max_x - 12
  local cy = (btn_min_y + btn_max_y) / 2
  r.ImGui_DrawList_AddTriangleFilled(dl, cx - 3, cy - 2, cx + 3, cy - 2, cx, cy + 2, col)
  -- Type-to-open, or open downwards like a combo, while the button is focused. Ignore
  -- presses with Ctrl/Alt/Super held so app shortcuts (e.g. Ctrl+F to focus the main
  -- search) aren't misread here as typing, which would reopen the popup and steal focus.
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
    -- Ctrl+F abandons the dropdown (filter left unchanged) so the main search bar can
    -- take focus. Detected before the re-grab below so the popup doesn't fight the
    -- search box for focus; we deliberately do not set focus_button here.
    local abort_to_search = r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Ctrl())
        and r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_F(), false)
    if abort_to_search then
      r.ImGui_CloseCurrentPopup(ctx)
    end
    -- Keep keyboard focus on the search input the whole time the popup is open. With
    -- the input capturing the keyboard, ImGui suppresses navigation elsewhere, so the
    -- arrow/page keys below drive the list instead of leaking to the preset table
    -- behind. Re-grab only when the input isn't already active so typing isn't disrupted.
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

    -- Keyboard navigation. A single-line InputText ignores Up/Down, so they are
    -- free for moving the highlight; Enter confirms, Escape cancels.
    local n = #state.matches
    local lowest = state.search == '' and -1 or 0
    local PAGE = 10
    if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_DownArrow(), false) then
      state.selected = math.min(state.selected + 1, n - 1)
      state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_UpArrow(), false) then
      state.selected = math.max(state.selected - 1, lowest)
      state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_PageDown(), false) then
      state.selected = math.min(state.selected + PAGE, n - 1)
      state.scroll_to_selected = true
    elseif r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_PageUp(), false) then
      state.selected = math.max(state.selected - PAGE, lowest)
      state.scroll_to_selected = true
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

    -- Result list. NoNav keeps ImGui's keyboard navigation from pulling focus into
    -- this scroll container (which would bounce focus off the search input); we drive
    -- selection ourselves. EndChild is always called to keep the ImGui stack balanced.
    local list_flags = r.ImGui_WindowFlags_NoNav()
    if r.ImGui_BeginChild(ctx, 'filter_list_' .. kind.id, 0, LIST_HEIGHT, 0, list_flags) then
      for idx, match in ipairs(state.matches) do
        local is_selected = (idx - 1) == state.selected
        if r.ImGui_Selectable(ctx, match.name .. '##' .. match.index, is_selected) then
          action = { action = 'set_filter', filter_id = kind.id, value = match.index }
          r.ImGui_CloseCurrentPopup(ctx)
        end
        if is_selected and state.scroll_to_selected then
          r.ImGui_SetScrollHereY(ctx, 0.5)
        end
      end
    end
    r.ImGui_EndChild(ctx)
    state.scroll_to_selected = false

    r.ImGui_EndPopup(ctx)
  end

  return action
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
      | r.ImGui_TableFlags_Resizable()
  if r.ImGui_BeginTable(ctx, 'presets', 3, flags) then
    r.ImGui_TableSetupColumn(ctx, 'Name', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'FX', r.ImGui_TableColumnFlags_WidthStretch())
    r.ImGui_TableSetupColumn(ctx, 'Ext', r.ImGui_TableColumnFlags_WidthFixed(), 50)
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
          r.HB_Pot_PlayPreview(i)
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

  -- Detect Ctrl+F to focus search bar
  if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_F(), false) and r.ImGui_IsKeyDown(ctx, r.ImGui_Mod_Ctrl()) then
    focus_search_on_next_frame = true
  end

  if search_text == nil then
    local _, current = r.HB_Pot_GetSearchText()
    search_text = current or ''
  end
  r.ImGui_SetNextItemWidth(ctx, 250)

  -- Focus search bar if Ctrl+F was pressed
  if focus_search_on_next_frame then
    r.ImGui_SetKeyboardFocusHere(ctx)
    focus_search_on_next_frame = false
  end

  local changed, new_text = r.ImGui_InputText(ctx, 'Search', search_text)
  if changed then
    search_text = new_text
    r.HB_Pot_SetSearchText(new_text)
  end
  if r.HB_Pot_IsBusy() ~= 0 then
    r.ImGui_SameLine(ctx)
    r.ImGui_Text(ctx, 'scanning...')
  end
  -- Filters
  for _, kind in ipairs(FILTER_KINDS) do
    if not filter_search_state[kind.id] then
      filter_search_state[kind.id] = { search = '', matches = {}, selected = -1 }
    end
    local action = filter_combo(kind, filter_search_state[kind.id])
    if action and action.action == 'set_filter' then
      r.HB_Pot_SetFilter(action.filter_id, action.value)
    end
    r.ImGui_SameLine(ctx)
  end
  r.ImGui_NewLine(ctx)
  r.ImGui_Separator(ctx)
  -- Preset list
  preset_table()
end

local function loop()
  -- Trigger an initial database scan once (settings aren't persisted by the engine yet)
  if not refreshed_once and r.HB_Pot_IsAvailable() ~= 0 then
    refreshed_once = true
    r.HB_Pot_Refresh()
  end
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
