-- Overlay click-capture spike (ReaImGui)
--
-- Question this answers: can a borderless, full-screen, top-most, transparent ReaImGui
-- window sit ON TOP of *other* OS windows (e.g. a plug-in's floating FX window) and
-- capture the user's click position there? If yes, it's the basis for the Preset
-- Crawler's "point at the Next-preset button" capture step -- no countdown, no keypress.
--
-- ReaImGui has NO multi-viewport (ConfigFlags_ViewportsEnable is absent), so this relies
-- on the root host window itself going full-screen + borderless + top-most. Whether
-- ReaImGui honours that as a true cross-application overlay is exactly what we're testing.
--
-- How to run the test:
--   1. Open any plug-in in a FLOATING FX window, positioned somewhere on screen.
--   2. Run this script. In its small control window, click "Arm overlay".
--   3. Click on the plug-in. The overlay should be in front of the plug-in and should
--      record the click. The plug-in should NOT react (the overlay ate the click).
--   4. Read back the captured points. "native" is the OS coordinate enigo would use.
--      Press Esc while armed to disarm.
--
-- Verdict to report: (a) did the overlay visually cover the plug-in window, (b) were
-- clicks captured, (c) does the native coordinate look like the real screen position.

local r = reaper

if not r.ImGui_CreateContext then
  r.MB('This script requires ReaImGui (install via ReaPack).', 'Overlay spike', 0)
  return
end

local ctx = r.ImGui_CreateContext('Overlay Capture Spike')

local overlay_armed = false
local captured = {} -- list of { ix, iy, nx, ny }

-- Small normal window: arm/disarm the overlay and show captured points.
local function control_window()
  r.ImGui_SetNextWindowSize(ctx, 420, 300, r.ImGui_Cond_FirstUseEver())
  local visible, open = r.ImGui_Begin(ctx, 'Overlay Capture Spike', true)
  if visible then
    r.ImGui_TextWrapped(ctx,
      'Open a plug-in in a floating FX window, then click "Arm overlay" and click on '
      .. 'the plug-in. The overlay should cover the plug-in and capture the click. '
      .. 'Press Esc while armed to disarm.')
    r.ImGui_Separator(ctx)
    if r.ImGui_Button(ctx, overlay_armed and 'Disarm overlay' or 'Arm overlay') then
      overlay_armed = not overlay_armed
    end
    r.ImGui_SameLine(ctx)
    if r.ImGui_Button(ctx, 'Clear points') then
      captured = {}
    end
    r.ImGui_Separator(ctx)
    r.ImGui_Text(ctx, string.format('Captured points: %d', #captured))
    for i, p in ipairs(captured) do
      r.ImGui_Text(ctx, string.format('#%d   imgui = (%d, %d)   native = (%d, %d)',
        i, p.ix, p.iy, p.nx, p.ny))
    end
    r.ImGui_End(ctx)
  end
  return open
end

-- The full-screen, borderless, top-most, transparent overlay.
local function overlay_window()
  if not overlay_armed then return end

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

  local visible = r.ImGui_Begin(ctx, '##overlay', true, flags)
  if visible then
    local dl = r.ImGui_GetWindowDrawList(ctx)
    -- Light dim so it's obvious the overlay is active, but the plug-in still shows through.
    r.ImGui_DrawList_AddRectFilled(dl, vx, vy, vx + vw, vy + vh, 0x10183040)
    -- Crosshair at the cursor.
    local mx, my = r.ImGui_GetMousePos(ctx)
    r.ImGui_DrawList_AddLine(dl, mx - 14, my, mx + 14, my, 0xFFEE00FF, 1.5)
    r.ImGui_DrawList_AddLine(dl, mx, my - 14, mx, my + 14, 0xFFEE00FF, 1.5)
    local nx, ny = r.ImGui_PointConvertNative(ctx, mx, my, true)
    r.ImGui_DrawList_AddText(dl, vx + 24, vy + 24, 0xFFFFFFFF, string.format(
      'OVERLAY ARMED  -  click a target, Esc to disarm   |   imgui=(%d,%d)  native=(%d,%d)',
      mx, my, nx, ny))

    if r.ImGui_IsMouseClicked(ctx, 0) then
      captured[#captured + 1] = {
        ix = math.floor(mx + 0.5), iy = math.floor(my + 0.5),
        nx = math.floor(nx + 0.5), ny = math.floor(ny + 0.5),
      }
    end
    if r.ImGui_IsKeyPressed(ctx, r.ImGui_Key_Escape(), false) then
      overlay_armed = false
    end
    r.ImGui_End(ctx)
  end
end

local function loop()
  local open = control_window()
  overlay_window()
  if open then r.defer(loop) end
end

r.defer(loop)
