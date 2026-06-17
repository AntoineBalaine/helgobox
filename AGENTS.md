# AGENTS.md

Notes for agents working in this repo.

## Type-checking the Lua Pot Browser (`lua/pot_browser.lua`)

The Lua browser calls the flat ReaScript API (`reaper.ImGui_*` and our own
`reaper.HB_Pot_*`). It can be statically checked with `lua-language-server`
(LuaLS) against the LuaCATS definitions in
`/tank/projects/vscode-reascript-extension/resources/`
(`reaper-types.lua`, `imgui_defs_0.10.lua`).

### Tooling

LuaLS is installed at `/tank/projects/.toolchains/lua-language-server/`
(binary: `bin/lua-language-server`). A ready-made check workspace lives at
`/tank/projects/.toolchains/lua-check-pot/` containing:

- a copy of `pot_browser.lua` (refresh it from `lua/pot_browser.lua` before each run),
- `.luarc.json` pointing `workspace.library` at the reascript `resources` dir,
- `meta/hb_pot.lua` — a generated stub declaring every `reaper.HB_Pot_*`
  function so our own API isn't reported as unknown. Regenerate it from
  `pot-api/src/api.rs` whenever the API surface changes:

  ```bash
  {
    echo '---@meta'
    grep -oE 'HB_Pot_[A-Za-z0-9_]+' pot-api/src/api.rs | sort -u | while read fn; do
      printf -- '---@return any\n---@return any\n---@return any\nfunction reaper.%s(...) end\n' "$fn"
    done
  } > /tank/projects/.toolchains/lua-check-pot/meta/hb_pot.lua
  ```

  (The three `---@return any` lines stop multi-return calls like
  `local ok, name = reaper.HB_Pot_GetFilterItemName(...)` from tripping
  `param-type-mismatch` when their results feed integer/string parameters.)

### Running the check

```bash
LLS=/tank/projects/.toolchains/lua-language-server/bin/lua-language-server
WS=/tank/projects/.toolchains/lua-check-pot
cp lua/pot_browser.lua "$WS/pot_browser.lua"
rm -rf "$WS/logs"
"$LLS" --check "$WS" --checklevel=Warning --logpath="$WS/logs"
```

Diagnostics print to **stderr** in pretty form; `logs/check.json` is often
empty, so read the terminal output, not the JSON file.

### Two non-obvious gotchas (both will silently produce a false "no problems found")

1. **`--check` must target the workspace *directory*, not a single `.lua`
   file.** Pointed at a file, LuaLS loads no `workspace.library`, every
   `reaper.*` call resolves to an unknown global, and it reports zero problems —
   a false pass. Always pass the directory. Sanity-check the harness has teeth
   by temporarily appending a bogus wrong-arity call (e.g.
   `reaper.ImGui_Text(ctx, "a", "b", "c")`) and confirming `redundant-parameter`
   fires.

2. **`undefined-field` does not catch nonexistent functions here.**
   `reaper-types.lua` declares `reaper = {}` as a plain table (not an
   `@class`), and LuaLS allows arbitrary fields on plain tables. So a typo'd
   `reaper.ImGui_DoesNotExist()` is **not** flagged. Cover that gap with a grep
   existence check instead:

   ```bash
   for fn in $(grep -oE 'ImGui_[A-Za-z0-9_]+' lua/pot_browser.lua | sort -u); do
     grep -q "function reaper.${fn}(" /tank/projects/vscode-reascript-extension/resources/reaper-types.lua \
       || echo "MISSING: reaper.${fn}"
   done
   ```

   (Ignore the dynamic prefixes `ImGui_Key_` and `ImGui_Key_Keypad`, which the
   script builds at runtime as `ImGui_Key_<LETTER>` / `ImGui_Key_Keypad<DIGIT>`.)

### Interpreting results: `reaper-types.lua` is older than ReaImGui 0.10

The flat `reaper.ImGui_*` definitions in `reaper-types.lua` are an older
snapshot than `imgui_defs_0.10.lua`, and the 0.10 file is written in the
`ImGui.*` class/shim form (`@field`/`function ImGui.X`), so the checker
resolves flat calls against the **older** `reaper-types.lua`. That produces
false positives where the flat API changed between versions. Known ones:

- **`ImGui_Attach(ctx, obj)`** — `reaper-types.lua` narrows `obj` to
  `Font | Image`; 0.10 widens it to `ImGui_Resource`, of which
  `ImGui_ListClipper` is a subtype. Attaching a clipper is valid in 0.10.
- **`ImGui_BeginChild(ctx, str_id, w, h, child_flags, window_flags)`** —
  `reaper-types.lua` still has the pre-0.9 5th param `border` (boolean); since
  0.9 it is `child_flags` (integer). Passing `0` is correct for 0.10.

When the checker flags an `ImGui_*` signature, confirm it against
`imgui_defs_0.10.lua` before treating it as a real defect. For a fully
0.10-accurate automated check, transform `imgui_defs_0.10.lua` from its
`ImGui.X` / `@field X` class form into flat `reaper.ImGui_X` function stubs and
use that as the library instead of `reaper-types.lua`.

## Building the standalone Pot extension

`./build-pot-extension.sh [--release]` builds `pot-extension` and renames
cargo's `libreaper_pot.so` to `reaper_pot.so` under `target/debug` (or
`target/release-strip`). REAPER loads extensions from its `UserPlugins`
directory at startup, **not** from `target/` — copy the artifact there and
restart REAPER (a script reload alone won't pick up new `HB_Pot_*` functions,
since the API registers at startup). Before launching, verify you copied a
current build:

```bash
nm target/debug/reaper_pot.so | grep -c HB_Pot_GetDestinationTrack   # expect > 0
```

## Type-checking in the dev sandbox (cargo)

The shared cargo registry (`/opt/rust/cargo`) has cross-user permission holes:
extracted crate *sources* are sometimes unreadable, so a plain `cargo check`
dies with `Permission denied` reading some crate's `src/lib.rs`. Use a **private
`CARGO_HOME` + `CARGO_TARGET_DIR`** so cargo re-extracts sources where you own
them (seed the private home by copying the readable `registry/cache`,
`registry/index`, and `git` dirs so it works offline):

```bash
export CARGO_HOME="$HOME/.pot-cargo" CARGO_TARGET_DIR="$HOME/.pot-target"
cargo check -p pot -p pot-api -p pot-extension   # type-checks; pot-db has unit tests
```

`cargo check` is the right bar here — the sandbox **cannot link** the cdylib:
`pot-extension` pulls in `enigo` (crawler input), which needs `-lxdo`, and
`libxdo` isn't installed (no root to `apt-get` it). So the final `.so` link and
any `nm` symbol check happen on the user's machine, not here.

## Committing

Commit finished, verified work without asking first (this supersedes any earlier
one-off "don't commit yet" — that applied only while the user was squashing /
reverting a removed feature). Still: don't push unless asked, and pause commits
again if the user says they're mid-rebase. Record durable project facts and
preferences **here in AGENTS.md**, not in a separate memory store.
