# Pot Browser — Architecture and Development Notes

## Planned changes

### 1. Linux support via pre-frame data preparation

Refactor the render closure to be a pure function with no `Reaper::get()` calls, on all platforms. Introduce a `ReaperFrame` struct populated on the main thread before each frame. Replace the two `show_console_msg` error fallbacks with `egui-toast` notifications. Enable the pot browser on Linux. Verify against the repeated-open crash (nih-plug issue #98).

**Status: Stage 1 (reads) and Stage 2 (command routing) are implemented.**

- Stage 1: `pot-browser` no longer contains any direct `Reaper::get()` call. `ReaperFrame` lives in `pot-browser/src/reaper_frame.rs` and is captured by the host in `main/src/infrastructure/ui/pot_browser_panel.rs` at the top of the render closure. The console fallbacks are toasts now (via a thread-local pending-error queue, drained each frame).
- Stage 2: REAPER-mutating actions (`load_preset` from all entry points — table double-click, Enter key, pre-play of preview files — and `play_preview` from all entry points) no longer execute inline. They are queued as `ReaperCommand`s (`pot-browser/src/reaper_commands.rs`) and executed by `process_pending_reaper_commands()`, which the host calls on the main thread right after each frame. Errors flow back as `UiFeedback` through a channel and surface as toasts/dialogs one frame later. `MainThreadSpawner` now uses the from-any-thread `spawn_in_main_thread` variant.

**Stage 3 is implemented (pending manual testing on Linux):**

- The current-FX resolution and the entire macro-parameter top panel are snapshotted into `ReaperFrame` (`CurrentPresetPanelSnapshot`): bank labels, per-slot macro names, hover texts, current values and plug-in-formatted value strings. Slider drags become `ReaperCommand::SetFxParameter`; the formatted display catches up one frame later. The `with_current_fx_preset` `RefCell` borrow happens during capture, on the main thread. The bank index is an `Arc<AtomicU32>` shared between the UI and the capture.
- Preview volume, "Chain"/"FX" buttons are commands now too. The crawler Ready dialog carries the FX name as a string instead of asking REAPER.
- `HostBridge` (cheap-to-clone: pot unit, bank index, command receiver, feedback sender) lets the host capture frames and drain commands without touching the render-thread `State`. `State` is statically asserted to be `Send`.
- `egui_views::open_with_send_state` hosts Send state without the `Fragile` wrapper; `open` now delegates to it.
- On Linux, `PotBrowserPanel` starts a 30 ms SWELL timer (`View::timer`, main thread) that executes queued commands and captures a fresh `ReaperFrame` into an `Arc<Mutex<_>>` which the render thread reads. On Windows/macOS, capture/drain happen inline in the render closure as before. Lock ordering is safe: the timer never holds the frame lock and the pot-unit lock simultaneously.

There is no `target_os` gate on the pot browser — building with `--features egui` on Linux enables it. The known upstream risk remains the crash-on-repeated-open issue in egui-baseview/baseview on Debian (nih-plug issue #98): test open → close → reopen explicitly.

**Linux smoke-test results (manual testing on Arch, X11):** the browser renders, filters work and filter the preset list, presets are clickable, and the Options/Tools menus work. Issues found and fixed during bring-up:

- `swell_ui::Window::get_xlib_handle` was an unimplemented stub; baseview only needs the X window id (it opens its own display connection), so `XBridgeWindow::raw_window_handle` now supplies just that.
- `Backbone::get()` is `Fragile` (main-thread-only) and was reached from the render thread via two paths: `with_pot_filter_exclude_list` (filter panels) and — the subtle one — `create_build_input → PotIntegration::exclude_list` on every collection rebuild (filter clicks, search keystrokes). Fixed at the root: the exclude list moved from `Backbone`'s `RefCell` to `AnyThreadBackboneState` behind an `RwLock` (same pattern as `pot_favorites`), and the pot trait signatures now document the any-thread requirement.
- `SliderVolume`'s `Display` impl calls REAPER's main-thread-only `mk_vol_str`; the preview-volume formatter now does plain dB math.
- The preview-template path resolution touches `BackboneShell` (also Fragile); it's captured into `ReaperFrame` now.
- Frame-mutex poisoning after a caught panic bricked the window permanently; both lock sites are poison-tolerant now (a `ReaperFrame` is plain data).
- baseview's X11 render thread doesn't stop when the parent window closes; the panel now stores the window handle and closes it in `on_destroy`, plus kills the frame timer.
- baseview's X11 keyboard handling translates keycodes through a hardcoded US-QWERTY table (druid heritage, with a TODO admitting it). The local baseview fork (`../baseview`, wired in via `[patch]`) now queries the X server's keyboard mapping per key press, honoring the user's actual layout, with the old table as fallback for non-character keys.
- Debug builds chain a stderr panic hook (with per-message dedup) onto the REAPER-console crash handler, so panics are copyable from the terminal even when a render loop would repaint the console.

Still pending manual testing: the preset crawler (including the new "Save Preset As" scraping flow), the preview recorder, the macro-param panel with a parameter-bank preset, and the open → close → reopen cycle.

See the [Linux support](#linux-support) section for full design details.

### 2. Preset crawler: "Save Preset As" dialog scraping

Extend the preset crawler to retrieve preset names from plugins that do not expose them via the REAPER API, by scraping the plugin's own "Save Preset As" dialog.

**Status: implemented (pending manual testing in REAPER).**

- `CrawlPresetArgs` has an optional `save_as_dialog: Option<SaveAsDialogScraping>` (a pair of button positions). When set, the crawl loop obtains each preset name via `scrape_preset_name_via_save_as_dialog()` in `pot/src/preset_crawler.rs` instead of `fx.preset_name()`.
- Dialog detection uses `xcap` window-title counting (counts, not a set — handles duplicate/empty titles), polled every 50 ms with escape checks and a 10-second timeout. The diff logic is pure and unit tested.
- The name is copied via Ctrl+A/Ctrl+C (`EnigoMouse::select_all_and_copy`, Cmd on macOS) and read with `arboard`, with a clipboard sentinel to detect copy failures.
- The crawler UI flow has a "Scrape preset names" checkbox in the Ready dialog plus two extra mouse-countdown steps that capture the "Save preset as..." and "Cancel" button positions.

See the [Preset Crawler](#preset-crawler) section for full design details.

---

## What it is

Pot Browser is a preset discovery and management system embedded in Helgobox/ReaLearn. It presents a unified browser over presets scattered across multiple sources — Native Instruments Komplete, REAPER FX chains, track templates, project-local presets, and REAPER's own factory defaults — and lets the user search, filter, and load presets directly into FX slots on tracks.

It also ships two companion tools:

- **Preset Crawler** — uses mouse automation to step through a plugin's own preset navigation UI and snapshot each preset as an RfxChain file.
- **Preview Recorder** — opens a template REAPER project, loads each preset, and records a short audio preview as an `.ogg` file.

---

## Crate structure

| Crate | Role |
|---|---|
| `pot/` | Core library: data model, database providers, filter/query engine, crawler, preview recorder |
| `pot-browser/` | UI layer: egui immediate-mode panel, dialogs, preset cache |
| `api/` (`helgobox-api`) | Shared types: `PotFilterKind`, `MouseButton`, etc. |
| `base/` | Utility library: locking helpers, hashing, mouse automation (`EnigoMouse`), file utilities |

`pot` and `pot-browser` have no dependency on `main` (the ReaLearn plugin crate). The `PotBrowserIntegration` trait in `pot-browser` is the explicit seam: ReaLearn implements it in `main/`, but `pot-browser` defines and calls it generically. This makes the pot browser extractable as a standalone REAPER extension without touching the core crates.

---

## Data model

A `PotPreset` carries:

- `PersistentPresetId` — stable across rescans; derived from file path or content hash (xxh3 128-bit), not a database row ID
- Name, product name, author, vendor, comment, file size, modification date
- One or more `PluginId` / `ProductId` associations
- `is_supported` / `is_available` flags (a preset can exist but be unloadable if the plugin is missing)
- A `PotPresetKind`: `FileBased` (`.nks`, `.RfxChain`, etc.), `ProjectBased`, `Internal`, or `DefaultFactory`

---

## Database providers

Each provider implements the `Database` trait. All are registered in `PotDatabase`, which is a `once_cell::Lazy` singleton. Each provider is held behind its own `RwLock`, allowing concurrent reads across providers.

| Provider | Source |
|---|---|
| `KompleteDatabase` | `{data_local_dir}/Native Instruments/Komplete Kontrol/komplete.db3` (read-only; written by Komplete Kontrol) |
| `DirectoryDatabase` | `{reaper_resource}/FXChains/` and `{reaper_resource}/TrackTemplates/` |
| `IniDatabase` | `{reaper_resource}/presets/` |
| `DefaultsDatabase` | REAPER's built-in factory presets |
| `ProjectDatabase` | Per-project presets added manually |

`{data_local_dir}` resolves via `dirs::data_local_dir()`: `~/.local/share` on Linux, `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on Windows.

`{reaper_resource}` is `Reaper::get().resource_path()`: typically `~/.config/REAPER` on Linux.

**Every `refresh()` is a full re-scan from scratch.** There is no persistent cache or incremental update between REAPER sessions. Refresh is triggered explicitly by the user, not on a timer or file watcher.

---

## Query pipeline

`RuntimePotUnit` runs queries through five phases:

1. **Refresh** — all providers re-scan their sources
2. **Filter build** — collect available `FilterItem` values per `PotFilterKind`
3. **Preset query** — apply current filter state; emit matching presets as an `IndexSet`
4. **Preview filter** — mark which presets have `.ogg` preview files on disk
5. **Sort + index** — sort filter lists and preset list; build lookup indexes

Rapid filter changes are debounced. A `revision` counter detects stale results from in-flight queries.

---

## UI layer

`PotBrowserPanel` in `pot-browser/` is rendered by **egui** (immediate-mode), hosted inside a REAPER dialog window via **egui-baseview** (a forked OpenGL child window embedder). The render loop is driven by baseview autonomously — REAPER's timer infrastructure is not involved.

On macOS and Windows, rendering runs on the main thread. On Linux, baseview runs on its own thread, which causes conflicts with REAPER's API; the `egui` feature is therefore **disabled on Linux**.

The `PotBrowserIntegration` trait has six methods, all straightforward REAPER API calls:

```rust
pub trait PotBrowserIntegration {
    fn get_track_label(&self, track: &Track) -> String;
    fn pot_preview_template_path(&self) -> Option<&'static Utf8Path>;
    fn pot_favorites(&self) -> &'static RwLock<PotFavorites>;
    fn with_current_fx_preset(&self, fx: &Fx, f: impl FnOnce(Option<&CurrentPreset>));
    fn with_pot_filter_exclude_list(&self, f: impl FnOnce(&PotFilterExcludes));
}
```

---

## Preset Crawler

### How it works today

`crawl_presets()` in `pot/src/preset_crawler.rs` runs an async loop on the REAPER main thread:

1. Read the current preset name via `fx.preset_name()`
2. Snapshot the plugin state via `fx.chunk()` and append it to a temporary file
3. Click the "Next preset" button at a user-supplied cursor position using `EnigoMouse`
4. Wait ~100ms
5. Repeat

The loop stops when the preset name stops changing (same name seen more than `MAX_SAME_PRESET_NAME_IN_A_ROW_ATTEMPTS` = 10 times in a row) or when names cycle back to the beginning of the list (matched against the first `MAX_SAME_PRESET_NAME_LIKE_BEGINNING_ATTEMPTS` = 10 presets crawled). The user can also interrupt with Escape.

Crawled presets are saved as `.RfxChain` files under `{reaper_resource}/FXChains/Pot/{PluginName}/{PresetName}.RfxChain`, or as shim files under `{reaper_resource}/Helgoboss/Pot/shims/` if the preset matches an existing but unsupported entry in the Komplete database.

### Extending the crawler: plugins that do not expose preset names via the REAPER API

Some plugins do not surface their preset names through REAPER's `fx.preset_name()` call. For these, the crawl loop gets an empty or meaningless name, making it impossible to deduplicate or name the saved files correctly.

**Proposed approach: "Save Preset As" dialog scraping**

Many such plugins expose the current preset name through their own "Save Preset As" dialog, which pre-fills the name field with the current preset name. The approach:

1. Before clicking "Next Preset", click the plugin's "Save Preset As" button (user-supplied cursor position).
2. Wait for a new OS window to appear — **do not use a fixed sleep**. Instead, snapshot the list of open windows via `xcap::Window::all()` before clicking, then poll in a tight loop (50ms intervals) until a window appears that was not in the snapshot. This is robust across plugins that open their dialogs at different speeds.
3. Once the dialog is open, send `Ctrl+A` then `Ctrl+C` to select all text in the name field and copy it to the clipboard.
4. Click the "Cancel" button (user-supplied cursor position) to dismiss the dialog without saving.
5. Read the clipboard via `arboard::Clipboard` — this is the preset name.
6. Proceed with snapshotting the plugin state and saving as before.

**Implementation notes**

- `xcap` is already a workspace dependency (used in the integration test screenshot infrastructure).
- `arboard` is already a workspace dependency (used elsewhere in helgobox).
- `EnigoMouse` (already used by the crawler) handles the button clicks.
- `CrawlPresetArgs` would gain two optional fields: `save_as_button_pos: Option<MouseCursorPosition>` and `cancel_button_pos: Option<MouseCursorPosition>`. When both are `None`, the crawler falls back to the existing `fx.preset_name()` path.
- The window poll loop should also check `escape_catcher.escape_was_pressed()` on each iteration so the user can abort if the dialog never appears.

Rough loop sketch:

```rust
let name = if let (Some(save_pos), Some(cancel_pos)) = (args.save_as_pos, args.cancel_pos) {
    // Snapshot open windows before clicking
    let before: HashSet<String> = xcap::Window::all()?
        .iter()
        .map(|w| w.title().to_string())
        .collect();
    // Open the "Save Preset As" dialog
    mouse.set_cursor_position(save_pos)?;
    mouse.press(MouseButton::Left)?;
    moment().await;
    mouse.release(MouseButton::Left)?;
    // Wait for the dialog window to appear
    loop {
        millis(50).await;
        if escape_catcher.escape_was_pressed() { return interrupted(); }
        let after = xcap::Window::all()?;
        if after.iter().any(|w| !before.contains(w.title())) { break; }
    }
    // Copy the name field contents
    enigo.key_sequence_parse("{+ctrl}a{-ctrl}");
    enigo.key_sequence_parse("{+ctrl}c{-ctrl}");
    moment().await;
    let name = arboard::Clipboard::new()?.get_text()?;
    // Dismiss the dialog
    mouse.set_cursor_position(cancel_pos)?;
    mouse.press(MouseButton::Left)?;
    moment().await;
    mouse.release(MouseButton::Left)?;
    a_bit_longer().await;
    name
} else {
    args.fx.preset_name().ok_or("couldn't get preset name")?.into_string()
};
```

---

## Standalone build

`pot` and `pot-browser` have no dependency on `main` (ReaLearn). Extracting them as a standalone REAPER extension requires:

1. A new Cargo workspace containing `pot`, `pot-browser`, `base`, `swell-ui`, and `api` copied verbatim.
2. A thin `pot-standalone` crate (~200–300 lines) that:
   - Implements `PotBrowserIntegration` (six REAPER API calls)
   - Hosts the egui window via `egui_views::open()` (copy the pattern from `main/src/infrastructure/ui/egui_views/mod.rs`)
   - Registers itself as a REAPER extension with a single action to open the browser window
3. The `egui-baseview` and `baseview` forks (`branch = "realearn"` in helgoboss's GitHub) must be referenced as git dependencies — vendor them if a fully self-contained build is required.

No changes to `pot` or `pot-browser` are needed.

---

## Linux support

The pot browser is currently disabled on Linux. The root cause is that baseview's X11 backend runs its event loop on a background thread, not the calling thread. This breaks REAPER API calls made from within the egui render closure, since nearly all REAPER API functions must be called from the main thread.

The upstream baseview repo (RustAudio/baseview) has not addressed this threading architecture as of mid-2026. There is no proposal in flight to change it.

ReaLearn itself already ships a working Linux egui implementation for its other egui views (advanced script editor, target filter panel). It uses GTK3 to extract the X window ID and display pointer from REAPER's SWELL-provided host window and passes those handles into egui-baseview. The pot browser was excluded specifically because its render closure makes REAPER API calls, which would run on the wrong thread.

### Fix: pre-frame data preparation

The correct fix is to make the render closure a pure function with no REAPER coupling on any platform, not just Linux. This is better design regardless of the Linux problem.

The approach is a `ReaperFrame` struct that is populated on the main thread immediately before each egui frame, then passed into the render closure as read-only data:

```rust
struct ReaperFrame {
    focused_fx: Option<FocusedFx>,
    track_count: usize,
    tracks: Vec<(usize, String)>,
    resource_path: Utf8PathBuf,
}
```

The render closure signature becomes:

```rust
fn run_ui(ctx: &egui::Context, state: &mut State, integration: &I, frame: &ReaperFrame)
```

The six `Reaper::get()` call sites in `pot_browser_panel.rs` reduce to:

| Call site | Resolution |
|---|---|
| `Reaper::get().resource_path()` (×2) | Read from `frame.resource_path`, populated once at startup |
| `Reaper::get().focused_fx()` | Read from `frame.focused_fx`, populated each timer tick |
| `Reaper::get().current_project().track_count()` and track list | Read from `frame.tracks`, populated each timer tick |
| `Reaper::get().show_console_msg()` (×2, error fallbacks) | Replaced with `egui-toast` notifications — already a `pot-browser` dependency, better UX, no REAPER involvement |

On macOS and Windows the `ReaperFrame` is populated inline in the existing render path. On Linux it is populated by REAPER's timer callback (via `plugin_register("timer", ...)`, which fires on the main thread at ~33fps) and stored in an `Arc<Mutex<ReaperFrame>>` that the render closure reads.

Implementing this uniformly across all platforms gives:

- A single code path with no `#[cfg(target_os = "linux")]` guards inside the render closure
- A render closure that is a pure function of `(&egui::Context, &mut State, &ReaperFrame)` — unit testable by constructing a fake `ReaperFrame` without a running REAPER instance
- A clear, type-enforced boundary between REAPER world and egui world
- The standalone build benefits immediately: the render closure has no REAPER coupling at all

The crash-on-repeated-open bug on Debian/Ubuntu (nih-plug issue #98, still open as of mid-2026) affects egui-baseview on Linux generally and should be verified before shipping Linux support.

---

## Testing

### What is tested today

The unit tests in `pot/` cover pure logic only:

- `plugin_id.rs` — VST2/VST3/CLAP/JS plugin ID parsing from REAPER's RPP XML format
- `api.rs` — `PersistentPresetId` serialisation and round-trip parsing
- `plugins.rs` — plugin name matching and product association
- `providers/komplete.rs` — NKS bank hierarchy construction from flat SQL rows

There are **no tests** for the crawler, the preview recorder, or any UI behaviour. The integration test suite in `main/src/infrastructure/test/` covers ReaLearn MIDI mapping mechanics only and has no pot browser coverage.

### What requires manual testing today

Everything involving REAPER at runtime:

- Database refresh and preset discovery (requires REAPER to be running with actual plugins installed)
- Preset loading into FX slots
- The preset crawler (requires a running plugin UI to click through)
- The preview recorder (requires REAPER's audio engine)
- The egui UI itself

### Path toward automated testing for the crawler

The crawler's core logic — stop heuristics, duplicate detection, preset accumulation — is pure state manipulation in `PresetCrawlingState` and is unit tested in `pot/src/preset_crawler.rs` (`mod tests`): distinct-name accumulation, intermediate duplicates, same-name-in-a-row tolerance and stop, wrap-around-to-beginning stop, and never-stop mode.

The mouse automation and window-detection portions are inherently integration-level. A practical testing approach:

- Build a minimal stub VST (or use a JS plugin in REAPER) with a known, finite preset list and known button positions. Run the crawler against it in a headless REAPER instance.
- For the "Save Preset As" dialog scraping extension: a stub plugin that opens a predictable dialog with a known preset name in the name field would allow the full path to be exercised automatically.

The `xcap` call and `arboard` clipboard read are third-party library concerns — testing whether they work correctly on a given OS is their responsibility, not ours. What is worth unit testing is the logic that sits on top: given a known `before` set and a known `after` set, does the detection code correctly identify the new window, handle the case where no new window appears, and honour the escape signal? That logic can be tested by injecting fake window title sets directly, with no xcap or OS involvement. The full end-to-end path (real plugin, real dialog, real button positions) will always require a manual smoke test.
