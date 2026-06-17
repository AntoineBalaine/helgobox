-- peaks_probe_spike.lua
--
-- Diagnostic spike for the waveform-preview design. Answers two questions empirically,
-- rather than by assumption:
--   1. Does PCM_Source_GetPeaks ALONE (no BuildPeaks) return COMPLETE peaks for a file?
--   2. Does obtaining peaks create a peak (.reapeaks) file on disk, and if so, WHERE
--      (a sidecar next to the audio, or REAPER's alternate hashed cache path)?
--
-- Run it as a ReaScript (Actions -> Load -> run). It prompts for an audio file path and
-- prints its findings to the ReaScript console. Point it at a real recorded pot preview
-- OGG (…/Helgoboss/Pot/previews/aa/bb/<hash>.ogg) to test the actual case.

local r = reaper

local function msg(s) r.ShowConsoleMsg(tostring(s) .. "\n") end

local function dir_of(path) return (path:match("^(.*)[/\\][^/\\]*$")) or "" end

local function decode_retval(retval)
  local rv = math.floor(retval)
  return {
    returned     = rv & 0xfffff,            -- low 20 bits: peak samples actually returned
    output_mode  = (rv & 0xf00000) >> 20,   -- 4 bits
    extra_avail  = (rv & 0x1000000) ~= 0,   -- spectral/extra was available
  }
end

local BUCKETS = 400      -- numsamplesperchannel we request (≈ a waveform's pixel width)

-- Run GetPeaks with a fresh buffer and report. Returns the decoded retval table.
local function probe_getpeaks(src, length, nch, want_extra, label)
  local peakrate = BUCKETS / length                       -- "pixels per second"
  local buf = r.new_array(nch * BUCKETS * 3)               -- max + min + extra blocks
  local retval = r.PCM_Source_GetPeaks(src, peakrate, 0, nch, BUCKETS, want_extra, buf)
  local d = decode_retval(retval)
  msg(string.format(
    "%s: requested=%d  returned=%d  complete=%s  output_mode=%d  extra_available=%s",
    label, BUCKETS, d.returned, tostring(d.returned >= BUCKETS), d.output_mode,
    tostring(d.extra_avail)))
  if want_extra == 0 and d.returned > 0 then
    local s = {}
    for i = 1, math.min(6, d.returned) do s[#s + 1] = string.format("%.3f", buf[i]) end
    msg("    first max-block values: " .. table.concat(s, ", "))
  end
  return d
end

-- 1. Get the file path.
local ok, path = r.GetUserInputs("GetPeaks probe", 1, "Audio file path:", "")
if not ok or path == "" then return end
path = path:gsub("^%s+", ""):gsub("%s+$", "")

r.ClearConsole()
msg("=== GetPeaks / peak-file probe ===")
msg("File: " .. path)
if not r.file_exists(path) then msg("File not found, aborting.") return end

-- 2. Create the source, report basics.
local src = r.PCM_Source_CreateFromFile(path)
if not src then msg("Could not create PCM source, aborting.") return end
local length = r.GetMediaSourceLength(src)
local nch = r.GetMediaSourceNumChannels(src)
local sr = r.GetMediaSourceSampleRate(src)
msg(string.format("Source: length=%.3fs  channels=%d  samplerate=%d", length, nch, sr))
if length <= 0 or nch <= 0 then
  msg("Bad source metadata, aborting.")
  r.PCM_Source_Destroy(src)
  return
end

-- 3. Where would REAPER put the peak file? Sidecar (our folder) or elsewhere?
local peakfile = r.GetPeakFileName(path) or ""
msg("")
msg("GetPeakFileName -> " .. (peakfile ~= "" and peakfile or "(empty)"))
if peakfile ~= "" then
  local sidecar = dir_of(peakfile):lower() == dir_of(path):lower()
  msg("    location: " .. (sidecar
    and "SIDECAR — same folder as the audio (would pollute the preview folder)"
    or "alternate cache path — NOT the audio's folder (no pollution)"))
end
local existed_before = peakfile ~= "" and r.file_exists(peakfile)
msg("    peak file exists BEFORE GetPeaks: " .. tostring(existed_before))

-- 4. GetPeaks directly — NO BuildPeaks. The key test.
msg("")
local d = probe_getpeaks(src, length, nch, 0, "GetPeaks (no BuildPeaks)")
local exists_after_get = peakfile ~= "" and r.file_exists(peakfile)
msg("    peak file exists AFTER GetPeaks: " .. tostring(exists_after_get)
  .. ((exists_after_get and not existed_before) and "   <-- GetPeaks CREATED it" or ""))

-- 5. If GetPeaks came back incomplete, drive BuildPeaks to completion and re-read.
if d.returned < BUCKETS then
  msg("")
  msg("Incomplete -> running PCM_Source_BuildPeaks to completion, then re-reading...")
  r.PCM_Source_BuildPeaks(src, 0)
  local iters, MAXI = 0, 500000
  while r.PCM_Source_BuildPeaks(src, 1) ~= 0 and iters < MAXI do iters = iters + 1 end
  r.PCM_Source_BuildPeaks(src, 2)
  msg("    BuildPeaks chunk-iterations: " .. iters)
  probe_getpeaks(src, length, nch, 0, "GetPeaks (after BuildPeaks)")
  local exists_after_build = peakfile ~= "" and r.file_exists(peakfile)
  msg("    peak file exists AFTER BuildPeaks: " .. tostring(exists_after_build)
    .. ((exists_after_build and not exists_after_get) and "   <-- BuildPeaks CREATED it" or ""))
end

-- 6. Spectral availability probe (informational; coloring is deferred).
msg("")
probe_getpeaks(src, length, nch, 115, "Spectral probe (want_extra_type=115)")

r.PCM_Source_Destroy(src)
msg("")
msg("Done. Read off: (a) was GetPeaks alone complete, (b) was a peak file created and where.")
