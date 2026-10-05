# devops/tests

The pass/fail checks for Rust-Hybrid-ECS. Every script here asserts and
returns a non-zero exit code on failure, and every script runs standalone
from any working directory. Performance measurement lives in
`devops/benchmarks/` because it reports numbers instead of asserting on them.

The scripts fall into three kinds:

| Kind | What it does | Edits your files? |
| --- | --- | --- |
| **Static checks** | Read sources or manifests and lint them. No toolchain, no host, seconds. | No |
| **End-to-end suites** | Start a real dev host, edit real source files, and judge the host by its log output. Minutes each. | **Yes, and restore them** |
| **Build and smoke checks** | Build a release, shipping or web binary and check that it runs. | Build output only |

Most of this document is about the second kind, because it is the one that
changes files in your working tree while it runs.

## Running

```powershell
# Everything, in order, with one PASS/FAIL summary at the end
python devops/tests/run_all.py
python devops/tests/run_all.py --list                  # print the run order
python devops/tests/run_all.py --only test_hot_reload_suite.py test_hot_reload_assets.py
python devops/tests/run_all.py --keep-going            # continue after a failure

# The hot-reload regression net: 8 scripts, timeouts scaled 1.5x by default
bash devops/ci_cd/run_hot_reload_tests.sh
bash devops/ci_cd/run_hot_reload_tests.sh --skip-build --timeout-scale 2.0

# One suite
python devops/tests/test_hot_reload_suite.py
python devops/tests/test_hot_reload_migration.py --cycles 3
```

`run_all.py` runs the cheap, host-free scripts first, so a broken matcher or a
reworded log line fails in seconds rather than twenty minutes in. By default it
stops at the first failure, because a suite that failed may have left the tree
mid-edit and the next suite's result would not be trustworthy.

`run_hot_reload_tests.sh` runs a fixed subset: `test_harness_parsing`,
`test_log_contract`, `test_hot_reload_suite`, `test_hot_reload_migration`,
`test_module_project_auto_reload`, `test_csharp_bridge`,
`test_reload_edit_during_build` and `test_hot_patch_coverage`. Expect 15 to 20
minutes plus the first host build.

Common flags on the end-to-end suites:

- `--timeout-scale S` multiplies every timeout. Use it on a slow or busy machine.
- `--skip-build` assumes `pill_standalone` is already built. Not every suite
  has it; those that don't always build what they need.

When an agent runs a suite, it goes through `.agents/tools/agent_run.py`
(see `AGENTS.md`), which records the output under `local/agent_runs/`.

### Where they run in CI

| Workflow | Runs |
| --- | --- |
| `ci.yml`, every push | `verify.py` steps. From this directory only the static checks: `test_coding_standards.py`, `test_renderer_boundaries.py` and `test_asset_metadata.py` (the last two with `--self-test` first). |
| `nightly.yml`, 04:00 UTC | Builds the windowed host, then `test_hot_reload_migration.py` and `test_hot_reload_suite.py`, and the hot-reload benchmark. Logs are uploaded as an artifact. |

Every other end-to-end suite is local only. Run the ones that cover the area
you changed (see the reference below, or the `pill-run-and-reload` skill).

## How an end-to-end suite works

All host-driving suites share their plumbing through
`devops/core/suite_common.py` (in `devops/core/`, because the benchmarks use it
too). A run goes through the same steps every time:

1. **Take the host lock.** `ensure_host_lock` takes an exclusive lock on
   `%TEMP%\pill_host_suite.lock`. If another suite holds it, this one prints a
   single `[WAIT]` line naming the holder's PID and queues. The OS releases the
   lock when the process exits, so a killed suite never blocks the next one.
   Host-free scripts don't take it and are safe to run beside anything.
2. **Kill stale hosts.** `kill_stale_hosts` runs `taskkill /IM
   pill_standalone.exe /F`. Doing this under the lock means it can only kill a
   leftover from an earlier run, never the host another suite is driving.
3. **Back up every file it may touch** (see [Files the suites
   edit](#files-the-suites-edit)).
4. **Build** `pill_standalone` (unless `--skip-build`).
5. **Launch the host** with `PROJECT_PATH` pointing at the project under test,
   either as `modules/target/debug/pill_standalone.exe` or through `cargo run`.
   Some suites first write their own `project_settings.yaml` so the extension
   list is exactly what the scenario needs. `PILL_CARGO_TIMINGS=1` is set so the
   host reports per-crate compile times.
6. **Read the host's output.** An `OutputMonitor` thread reads merged
   stdout/stderr into a rolling buffer and echoes each line as `[std] ...`.
   Every line gets a sequence number, so a scenario can ask for "output since
   line N" even after old lines have been dropped. The project's `counter
   tick` lines arrive thousands of times a second, so they go into a separate
   small tail instead and are not echoed. Otherwise they would push the reload
   lines out of the buffer.
7. **Run the scenarios.** Each scenario edits a file, waits for a log token
   (`[analytics] reload project`, `extension hot reload complete`, ...), then
   checks the output since the edit: required tokens present, forbidden tokens
   absent, and no crash signal (`panicked at`, `STATUS_ACCESS_VIOLATION`).
8. **Put the file back between scenarios** with `restore_one`, and wait for the
   reload that causes, so its output can't leak into the next scenario's window.
9. **Clean up in `finally`.** Restore every backed-up file, stop the host
   (terminate, then kill after 5 s), remove any probe files the suite created.

### The host's log is the contract

The suites never inspect engine state directly. Every verdict is a substring
search over the host's console output, using tokens declared in
`suite_common.py` and in each suite. A reworded log line therefore breaks the
suites without any compiler error. Two scripts guard against that:

- `test_log_contract.py` checks that every `*_TOKEN` literal the suites define
  still appears verbatim somewhere in the Rust sources (or is listed as
  produced elsewhere, with a reason). It runs in under a second.
- `test_harness_parsing.py` unit-tests the parsing the suites rely on (the
  analytics-line regex, patch routes, the brace scanner) against real captured
  host output.

Two tokens come from code you are likely to edit by hand:
`MODULE_REGISTERED_MESSAGE` (printed by `pill_spline`) and
`PROJECT_PROBE_PREFIX`, `[project] xxsees`, printed by `examples/project_rs`.
Changing either text makes the suites report `Missing required token`, which
looks like a reload failure but isn't: the reload worked and the text moved.

## Files the suites edit

The end-to-end suites edit **real tracked files in your working tree**, and you
can watch the changes appear in your editor or in `git status` while a suite
runs. Every suite puts its files back when it finishes, including when it fails.

| Suite | Files it changes | How it restores them |
| --- | --- | --- |
| `test_hot_reload_suite.py` | `devops/tests/project/src/lib.rs`, `modules/extensions/pill_spline/src/pill_spline.rs`, and `project_settings.yaml` of both `devops/tests/project` and `examples/project_rs` | `BACKUP`, original bytes and timestamp. The two settings files are then written once more with a fresh timestamp. |
| `test_hot_reload_migration.py` | `devops/tests/project/src/lib.rs` | Its own text copy, fresh timestamp |
| `test_module_project_auto_reload.py` | `pill_spline.rs`, `examples/project_rs/project_settings.yaml` | Its own text copies, fresh timestamp |
| `test_editor_revision.py` | `pill_spline.rs`, `examples/project_rs/project_settings.yaml` | Its own text copies, fresh timestamp |
| `test_shared_component_identity.py` | `pill_spline.rs`, `examples/project_rs/src/lib.rs`, `examples/project_rs/project_settings.yaml` | `BackupRegistry` for the sources (fresh timestamp), its own text copy for the settings |
| `test_csharp_bridge.py` | `examples/project_cs/project_settings.yaml`, `src/Components.cs`, `src/Systems.cs`, `pill_spline.rs`, the generated C# mirror files (deleted once to check they regenerate); creates `src/BridgeProbe.cs` | `BACKUP`, original bytes and timestamp; the probe file is deleted |
| `test_csharp_analyzer.py` | Creates `examples/project_cs/src/_AnalyzerProbe.cs` | Deleted at the end unless `--keep-probe` |
| `test_reload_edit_during_build.py` | `pill_spline.rs` | `BackupRegistry`, fresh timestamp |
| `test_hot_reload_assets.py` | In `examples/master_renderer_test/res/`: `textures/helmet_emissive.jpg` (overwritten with junk bytes), its `.meta`, `materials/helmet.material`; moves the image pair into `textures/moved_by_suite/` and back. Also `pill_master_renderer_data.rs` (a comment appended to force a rebuild) | `BackupRegistry`, fresh timestamp; the move is undone in its own `finally` |
| `test_hot_patch_coverage.py` | One function body in every crate listed in `examples/project_rs/project_settings.yaml`, plus the project | `BackupRegistry`, original bytes and timestamp |
| `test_patch_bookkeeping.py` | A system in `examples/project_rs/src/` (a body edit, then a broken tail); creates `modules/target/hot/rollback.request` | `BackupRegistry`, fresh timestamp; the request file is deleted |
| `test_wrapper_entry_points.py` | Writes `modules/extensions/host_module_wrapper_probe/` (a generated-namespace crate) | Deleted at the end unless `--keep-probe`; a host run prunes it as a stale wrapper too |
| `test_web_smoke.py` | Regenerates `build/pill_shipping_bundle/`; writes `examples/master_renderer_test/build/web/` | The bundle is snapshotted and restored |
| `test_shipping_smoke.py` | Regenerates `build/pill_shipping_bundle/` (gitignored) | Not restored |

### Edits and restores

- **Edits.** `atomic_write` writes a temporary file and renames it over the
  original, so the host's watcher never sees a half-written file. Suites that
  must preserve line endings exactly (`test_hot_patch_coverage`,
  `test_patch_bookkeeping`, `test_reload_edit_during_build`,
  `test_hot_reload_assets`) write raw bytes instead: Python's text mode
  turns LF into CRLF on Windows, and the host would then see the whole file
  as changed.
- **Backups.** `BackupRegistry.capture(path)` records a file's bytes and
  modification time the first time it is called for that path. Later calls do
  nothing, so the saved copy is always the state before the suite started.
- **`restore_one(path)`** runs mid-suite. It writes the original bytes with a
  fresh timestamp **on purpose**, so the watcher notices and reloads the
  original code before the next scenario.
- **`restore_all()`** runs in `finally`. It writes back every file byte for byte
  and by default rewinds the timestamp too. Cargo decides what to rebuild from
  timestamps, so a rewound file matches the artifact already built from it and
  the next host start skips the rebuild (`pill_spline` alone saves about 8 s).
- **`restore_all(reset_mtime=False)`** is used when the last thing the host
  built was the *edited* or broken code and nothing rebuilt the original
  afterwards. Rewinding the timestamp there would make cargo trust that stale
  artifact, so the file keeps a fresh timestamp and the next host start
  rebuilds it. Expect that rebuild after these suites; it is intended.

### Rules while a suite is running

- **Don't edit the files in the table above.** The final restore writes back
  the content captured at the start, and silently overwrites anything you
  typed in the meantime.
- **Ctrl+C is safe.** It raises `KeyboardInterrupt`, the `finally` blocks still
  run, and the files are restored.
- **A hard kill skips the restore.** Task Manager, closing the terminal, or an
  agent run reaching its `--timeout` all leave the edits on disk. Recover
  tracked files with `git status` and `git checkout -- <file>`. Untracked files
  (for example a `.meta` you haven't committed) can't be recovered that way, so
  commit or copy them before running `test_hot_reload_assets.py`.
- **Run suites one at a time.** The host lock already queues them, but two
  suites editing `pill_spline.rs` in sequence still depend on the first one
  restoring it.

## Suite reference

### Static checks (no host)

| Script | Checks |
| --- | --- |
| `test_harness_parsing.py` | The parsing logic the end-to-end suites decide pass or fail with, against real captured host output. A matcher that stops matching returns nothing rather than raising, so this is the only place that failure is visible. |
| `test_log_contract.py` | Every log token a suite matches on still exists in the Rust sources. |
| `test_coding_standards.py` | The comment and layout rules over every `.rs` file: `//!` header with `# Responsibilities`, `// SAFETY:` on `unsafe`, `///` on `pub` items, ordered import groups, `mod tests` last. Exit 0 clean, 1 violations, 2 usage error. |
| `test_renderer_boundaries.py` | The renderer split's dependency rules from the Cargo manifests: only a GPU module depends on `wgpu`, `pill_renderer_api` stays renderer-free, the host reaches renderer crates only through `*_dependency_graph` dependencies, and no project or data crate depends on a GPU module. `--self-test` proves each rule fails on a generated broken tree. |
| `test_asset_metadata.py` | Under `examples/*/res`: no orphaned `.meta` files and no two assets sharing a guid. `--self-test` proves it catches both. |
| `test_wrapper_entry_points.py` | The wrapper path exports the whole loadable-artifact entry-point set: writes a throwaway `host_module_wrapper_probe` crate around `pill_dummy_color`, builds it and compares its PE export directory against the expected names. `--self-test` covers the reader; Windows-only. |
| `test_csharp_analyzer.py` | The `PILLxxxx` Roslyn analyzer rules (needs .NET 8 SDK): the real C# project builds with no PILL diagnostic, and a temporary probe that breaks every rule produces every diagnostic and fails the build. |

### End-to-end suites (start a host, edit files)

| Suite | Covers |
| --- | --- |
| `test_hot_reload_suite.py` | The main suite, in two sessions. **A** (`devops/tests/project` + `pill_spline`): project reload with data surviving, schema migration, types removed from the project, module reload keeping its data (`existing=1`), repeated reloads staying stable, a removed module type dropped and re-seeded (`existing=0` after the restore), rollback when init fails. **B** (`examples/project_rs` + `pill_spline`): the module-to-project cascade and both sides seeing the same splines. Also checks a clean restart. |
| `test_hot_reload_migration.py` | Component schema changes on reload: unchanged shape (fast path), added, removed, renamed and reordered fields, persistable downgraded to plain, a widened `Vec` element type, a retyped one whose snapshot no longer parses (row reset), and an alignment change that must be refused and rolled back. Asserts migrated entity counts and values. `--cycles N` repeats it. |
| `test_module_project_auto_reload.py` | The cascade on its own: editing `pill_spline` reloads the module, the host queues a project reload, and the project reports the new value. |
| `test_shared_component_identity.py` | A component and a resource linked by both the project and a module DLL, so each binary has its own `TypeId`. With `#[pill(shared)]` they bind to one registration; without it startup must fail with a collision error. Also: a module reload with two registrants, a resource inserted by one DLL and read by the other, and releasing values after a failed reload or failed first load. Runs the windowed host, because `project_rs` links the renderer. |
| `test_csharp_bridge.py` | The C# side (needs .NET 8 SDK): the generated mirror's exact layout, a warning-free managed build, calls in both directions, assembly swaps keeping state, three swaps in one session (the old `AssemblyLoadContext` must unload), component layout migration, systems re-registered after a signature change, and a changed startup method refused while the old assembly keeps running. Also checks the committed mirror matches what the host generates. |
| `test_reload_edit_during_build.py` | Two saves, the second during the build started by the first: the newer save must win and actually be built. `--second-edit-delay` tunes the gap. |
| `test_hot_reload_assets.py` | Live asset reimport in the headless host on `examples/master_renderer_test`: a `.meta` edit reimports in place, a broken image keeps the loaded value and recovers when restored, reimport still works after the renderer data crate reloads, a moved image and `.meta` are followed without a new import, and a material edit applies. |
| `test_hot_patch_coverage.py` | Live patching per crate. Makes one body-only edit in every loaded crate (discovered from the project settings) and reports the host's verdict: `PATCHED`, `FELL BACK` with the reason, or `NO FAST PATH`. Also checks the cached compiler flags for split arguments. `--strict` fails on crates with no fast path. |
| `test_patch_bookkeeping.py` | A live patch, then a reload that fails to build: the failed reload must keep the patch records, so rolling the patch back still works. |
| `test_editor_revision.py` | A module reload bumps the host's editor revision counter (`editor revision bumped`), which the editor uses to drop its caches. |

### Build and smoke checks

| Script | Checks |
| --- | --- |
| `test_examples.py` | Builds every example under `examples/` in release (found by `Cargo.toml` or `*.csproj`) and reports artifact sizes. |
| `test_shipping_smoke.py` | Builds the statically linked shipping binary for `examples/project_rs`, then checks it reaches the project loop, initializes its extensions, and never starts cargo or any reload machinery. |
| `test_web_smoke.py` | Builds `examples/master_renderer_test` for the browser, serves it, and opens it in headless Chrome or Edge with WebGPU. Checks the asset pack mounts, frames are presented with a camera, and no engine, WebGPU or page error occurs. Needs the wasm target and `wasm-pack`; `PILL_WEB_BROWSER` picks the browser. |
| `test_basic.py` | `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test --workspace` with and without `hot_patch`. The launcher-driven checks in it skip in this repository. Last in `run_all.py` because it is the slowest. |

### Manual scripts (not in `run_all.py`)

| Script | Checks |
| --- | --- |
| `test_pbr_native.py <binary>` | The windowed frontend on Windows: first frame presented, resize, minimise and restore, clean shutdown. Logs to `local/renderer-validation/`. Only touches the process it starts. |
| `test_renderer_assets.py <cooker>` | The asset cooker's watch mode on a scratch copy under `local/renderer-validation/`: an edit keeps the asset ID, a failed cook leaves the manifest alone, a deletion removes the asset. |

## Writing a new end-to-end suite

- Import from `core.suite_common` after putting `devops/` on `sys.path`, as the
  existing suites do.
- Launch the host with `launch_process` (it takes the host lock) and call
  `kill_stale_hosts` first.
- `capture` every file you will edit **before** the first edit, use a
  `BackupRegistry` (or the shared `BACKUP`), and call `restore_all` in a
  `finally`. Decide `reset_mtime` by whether the host's last build was of the
  original content.
- Edit with raw bytes when line endings matter, otherwise with `atomic_write`.
- Take `start_index = monitor.line_count` before each edit and only search
  output after it, so an earlier reload's line can't satisfy the wait.
- Put new tokens in constants named `*_TOKEN` so `test_log_contract.py` checks them.
- Add the suite to `SUITE_ORDER` in `run_all.py`, and to the table above.

## Gate matrix

The gates a change must keep green. Run them from `modules/`. The counts were
measured on 2026-09-17; record what you actually measure and update a row in
the same commit that changes it.

| Gate | Command | Expected |
| --- | --- | --- |
| Engine unit tests | `cargo test -p pill_engine --lib --offline` | 322 passed |
| Engine, all targets | `cargo test -p pill_engine --offline` | all targets pass |
| Host, default posture | `cargo test -p pill_host --lib --offline` | 98 passed |
| Host, rendering | `cargo test -p pill_host --features rendering --lib --offline` | 152 passed |
| Host, shipping posture | `cargo test -p pill_host --no-default-features --lib --offline` | 25 passed |
| Clippy, formatting, lints | `python devops/ci_cd/verify.py --quick` | clean |
| End-to-end suites | `python devops/tests/run_all.py` | all pass |

## Generated C# mirrors

`modules/extensions/<module>/generated/<module>_Components.g.cs` files are
**committed**. They are build inputs, not artifacts: `examples/project_cs`
compiles against them, so the C# side must build without a host run. The host
regenerates each mirror on start and reload but writes only when the content
differs, so a matching file and its timestamp are left alone.

`test_csharp_bridge.py` catches drift: it records the committed file's bytes,
deletes the file, restarts the host, and requires the regenerated file to be
identical. When a module's layout really changes, run the host once and commit
the regenerated file.

## Known quirks

- **Two crates named `project`.** `devops/tests/project` and
  `examples/project_rs` both build `target/debug/project.dll`, so a DLL left
  from the other project can be loaded before the fresh build replaces it
  ("Counter did not tick"). Delete `modules/target/debug/project.dll` and rerun.
- **Text-mode restores.** `test_hot_reload_migration`,
  `test_module_project_auto_reload`, `test_editor_revision` and the settings
  file in `test_shared_component_identity` restore through `read_text` and
  `write_text` rather than `BackupRegistry`. On Windows that writes CRLF line
  endings over files `.gitattributes` keeps as LF, and leaves a fresh
  timestamp. The content is the same, but expect an extra rebuild, and
  possibly the files showing in `git status` until git refreshes its index.
- **Migration counts are cumulative.** The project re-seeds entities on every
  reload, so adding a scenario to `test_hot_reload_migration.py` changes the
  expected counts of the ones after it.
- **Keep the separate tick tail.** Any change to `OutputMonitor` must keep
  `counter tick` lines out of the main buffer, or they push out the reload
  lines the scenarios search for.

## Performance measurement

Hot-reload timing is in `devops/benchmarks/hot_reload.py` and
`hot_reload_harness.py`, driven by Pill Lab
(`python devops/pill_lab/pill_lab.py hot-reload --iterations 5`). It uses the
same `suite_common.py` plumbing, so a reworded log token must keep both working.
See `devops/pill_lab/README.md`.
