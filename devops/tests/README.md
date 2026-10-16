# devops/tests

## Summary

1. **What this is.** The automated testing pipeline: Python scripts that each check one thing about the engine and report pass or fail.
2. **Two kinds of check.** Static scripts read sources in seconds; end-to-end suites start a real host and take minutes.
3. **Real files.** The big suites edit real files and put them back afterwards - Ctrl+C is safe, a hard kill is not.
4. **Logs are the verdict.** Suites read the host's log lines and wait for specific text to appear.
5. **Kept honest.** Guard tests check that those log lines still exist in the Rust sources, and that every suite keeps its header.
6. **Just what changed.** `check_relevant.py` runs only the checks your change needs.
7. **In tiers.** `run_all.py` runs everything in order: static, host, editor, build.
8. **Cheap first.** Static checks run before the slow suites, so a stale log token fails in seconds.
9. **One at a time.** Suites queue on a host lock instead of clashing over the same files.
10. **One, or all.** Any suite runs alone (`python devops/tests/test_...py`); `run_all.py` runs the whole set.
11. **Manual checks.** A few suites need a real screen or mouse, so they never run by default.
12. **Found, not listed.** A new `test_*.py` with a header joins the pipeline automatically.
13. **Results.** Runs record under `.construction_site/runs/`; `run_all.py --catalogue` shows each suite's last result.
14. **Agents wrap their runs.** An agent starts suites and gates through `agent_run.py`, which records the status, full log and notes under `.construction_site/runs/`.
15. **Tagged for history.** Run tags (`--item`, `--check`) feed `work ingest` and `work checks`, so the check's recorded time is a median of real runs.
16. **Timeouts always.** Agent-started runs carry a `--timeout`, so a wait on the wrong log text ends instead of hanging.
17. **Scoped first for agents.** Agents use `check_relevant.py` while iterating and the full gate only at milestones - and after a failure they re-run the scoped selection, not the gate.
18. **Lint before done.** `check_changed.py` formats and lints the changed Rust files before an agent calls work finished.
19. **Keep out while it runs.** Don't edit a file a running suite owns - its restore would overwrite it. That applies to agents too.
20. **Docs come from the suites.** The catalogue below is generated from each suite's own header (`--write-catalogue`).
21. **Numbers live elsewhere.** Performance measurement is in `devops/benchmarks/`, not here.
22. **The full gate.** `verify.py` runs this directory plus every other CI check - it is what CI calls.
23. **Editor rule.** Build `pill_editor` right before its suites, with nothing in between.

The pass/fail checks for Rust-Hybrid-ECS. Every script here asserts and
returns a non-zero exit code on failure, and every script runs standalone
from any working directory. Performance measurement lives in
`devops/benchmarks/` because it reports numbers instead of asserting on them.

The scripts fall into three kinds:

| Kind                       | What it does                                                                                       | Edits your files?         |
| -------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------- |
| **Static checks**          | Read sources or manifests and lint them. No toolchain, no host, seconds.                           | No                        |
| **End-to-end suites**      | Start a real dev host, edit real source files, and judge the host by its log output. Minutes each. | **Yes, and restore them** |
| **Build and smoke checks** | Build a release, shipping or web binary and check that it runs.                                    | Build output only         |

Most of this document is about the second kind, because it is the one that
changes files in your working tree while it runs. `run_all.py` orders the
scripts by **tier** - `static`, then `host`, `editor`, then `build`, with the
manual ones set aside - and that is the order they run in.

## Contents

- [Run only what your change needs](#run-only-what-your-change-needs)
- [The suites themselves](#the-suites-themselves)
- [Where they run in CI](#where-they-run-in-ci)
- [How an end-to-end suite works](#how-an-end-to-end-suite-works)
- [Files the suites edit](#files-the-suites-edit)
- [Suite reference](#suite-reference)
- [Writing a new end-to-end suite](#writing-a-new-end-to-end-suite)
- [Gate matrix](#gate-matrix)
- [Generated C# mirrors](#generated-c-mirrors)
- [Known quirks](#known-quirks)
- [Performance measurement](#performance-measurement)

## Running

### Run only what your change needs

`devops/ci_cd/check_relevant.py` is the inner loop. It diffs the working tree
against `HEAD` (untracked files included), matches the paths against a rule
table of repository areas, and runs only that area's checks: format and
coding-standards on Rust changes, the area's cargo tests, the suites named for
it, the goldens, and the few `verify.py` steps that guard it. A file no rule
recognizes falls back to `verify.py --quick`, so no area passes unchecked, and
an editor selection gets its `cargo build -p pill_editor` prepended
automatically with the editor suites immediately after (see the ordering rule
below).

```powershell
python devops/ci_cd/check_relevant.py --dry-run     # select and print, run nothing
python devops/ci_cd/check_relevant.py               # run the selection
python devops/ci_cd/check_relevant.py --paths modules/pill_ui/src/layout.rs
python devops/ci_cd/check_relevant.py --list-rules  # the area table
```

The full gate (`python devops/ci_cd/verify.py`) and the full runs below are
for stage boundaries and milestones. When a scoped selection fails, fix and
re-run the selection, not the whole gate.

### The suites themselves

```powershell
# Every suite (manual ones aside), tier by tier, one PASS/FAIL summary at the end
python devops/tests/run_all.py
python devops/tests/run_all.py --list                  # print the run order
python devops/tests/run_all.py --tier static           # one tier: static, host, editor, build
python devops/tests/run_all.py --set hot-reload        # one named set
python devops/tests/run_all.py --only test_hot_reload_suite.py test_hot_reload_assets.py
python devops/tests/run_all.py --keep-going            # continue after a failure
python devops/tests/run_all.py --write-catalogue       # regenerate the catalogue below

# The hot-reload regression net (SETS: hot-reload), timeouts scaled 1.5x by default
bash devops/ci_cd/run_hot_reload_tests.sh
bash devops/ci_cd/run_hot_reload_tests.sh --skip-build --timeout-scale 2.0

# One suite
python devops/tests/test_hot_reload_suite.py
python devops/tests/test_hot_reload_migration.py --cycles 3
```

Roughly how long runs take on the development machine (the catalogue below
carries each suite's last recorded result):

| Run                                                      | Roughly                        |
| -------------------------------------------------------- | ------------------------------ |
| The static tier                                          | seconds                        |
| One editor suite                                         | 15 s to 190 s                  |
| `cargo build -p pill_editor` + `run_all.py --set editor`  | ~10 min                        |
| The hot-reload set (`run_hot_reload_tests.sh`)           | 15-20 min plus the first build |
| The full gate (`verify.py`)                              | ~2 h                           |

`run_all.py` finds the suites itself: every `test_*.py` here is one, and its
docstring declares `PROTECTS:` (what it guards) and `TIER:` (`static`, `host`,
`editor` or `build`), optionally `SETS:` (named groups) and `RUN: manual - <why>`
(not run by default: it needs an argument or an unattended screen;
`--include-manual` picks it up when the machine allows). A suite
without a valid tier stops `run_all.py` with a usage error, so a new suite
cannot silently drop out of the run. Static suites run first, so a broken
matcher or a reworded log line fails in seconds rather than twenty minutes in.
By default it stops at the first failure, because a suite that failed may have
left the tree mid-edit and the next suite's result would not be trustworthy.

`run_hot_reload_tests.sh` runs the suites tagged `SETS: hot-reload`
(`run_all.py --set hot-reload --list` prints them). Expect 15 to 20 minutes
plus the first host build. `--timeout-scale` and `--skip-build` reach only the
suites whose parser declares them, which is every suite built on
`add_common_suite_arguments` in `devops/core/suite_common.py`.

**The editor ordering rule.** Any cargo build or test between
`cargo build -p pill_editor` and the `SETS: editor` suites re-splits the
shared dylibs under the built binary, and the editor then dies at process
start (`0xC0000139 STATUS_ENTRYPOINT_NOT_FOUND`). Build the editor and run
its suites immediately after, with nothing in between - or let
`check_relevant.py` enforce the order for you. `verify.py`'s `editor` step
encodes the same order: the two host-tier editor suites first (they build a
narrowed `pill_standalone`), then `cargo build -p pill_editor`, then the
`SETS: editor` suites (`run_all.py --set editor`).

Common flags on the end-to-end suites:

- `--timeout-scale S` multiplies every timeout. Use it on a slow or busy machine.
- `--skip-build` reuses the built `pill_standalone` instead of building it.
  The binary is never taken on trust: the suite checks it was built from the
  same posture (`HostPosture`) and from sources no newer than it, and refuses
  to run otherwise.

When an agent runs a suite, it goes through `.agents/site/agent_run.py`
(see `AGENTS.md`), which records the output under `.construction_site/runs/`.

### Where they run in CI

| Workflow                           | Runs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ci.yml` (manual dispatch for now) | `verify.py`, every step. From this directory that includes the static guards (coding standards, renderer boundaries, engine layout, asset metadata, engine exports, feature drift, wrapper entry points). CI installs `cargo-nextest` first, so the two Rust test lanes run each test in its own process with per-test timings. The gate's `editor` step runs the host-tier editor suites, builds `pill_editor` and runs the `SETS: editor` suites end-to-end (the real executable; the manual drag-out session is reported as skipped). |
| `nightly.yml`, 04:00 UTC           | Builds the windowed host, then the suites tagged `SETS: nightly` (failures fail the nightly) and `SETS: nightly-allowed-to-fail` (known drift, logged only), and the hot-reload benchmark. Logs are uploaded as an artifact.                                                                                                                                                                                                                                                                                                                    |

Every other end-to-end suite is local only. Run the ones that cover the area
you changed: `check_relevant.py` selects them from the change set, and the
reference below (plus the `pill-run-and-reload` skill) maps areas to suites by
hand.

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
4. **Build** `pill_standalone` through the suite's `HostPosture` (unless
   `--skip-build`). One posture per suite covers both the build and the launch,
   so the host and the DLLs it loads always resolve the same feature graph; the
   posture is recorded next to the binary so `--skip-build` can prove the
   binary is the one this suite asked for.
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

| Suite                               | Files it changes                                                                                                                                                                                                                                                                            | How it restores them                                                                                              |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `test_hot_reload_suite.py`          | `devops/tests/project/src/lib.rs`, `modules/extensions/pill_spline/pill_spline/src/pill_spline.rs`, and `project_settings.yaml` of both `devops/tests/project` and `examples/project_rs`                                                                                                    | `BACKUP`, original bytes and timestamp. The two settings files are then written once more with a fresh timestamp. |
| `test_hot_reload_migration.py`      | `devops/tests/project/src/lib.rs`                                                                                                                                                                                                                                                           | Its own text copy, fresh timestamp                                                                                |
| `test_editor_crates.py`             | `pill_spline.rs`, `pill_spline_editor.rs`, `examples/project_rs/project_settings.yaml`                                                                                                                                                                                                      | Its own text copies                                                                                               |
| `test_editor_revision.py`           | `pill_spline.rs`, `examples/project_rs/project_settings.yaml`                                                                                                                                                                                                                               | Its own text copies, fresh timestamp                                                                              |
| `test_editor_native.py`             | The shell module's source (`pill_editor_shell_editor.rs`, one cadence constant); writes `local/editor-validation/`                                                                                                                                                                          | Its own text copy, in `finally`                                                                                   |
| `test_editor_commands.py`           | `pill_spline_editor.rs` (one appended comment); writes `local/editor-validation/` and a sandboxed `LOCALAPPDATA` there                                                                                                                                                                      | Its own text copy, in `finally`                                                                                   |
| `test_editor_undo.py`               | `pill_spline_editor.rs` (one appended comment); writes `local/editor-validation/` and a sandboxed `LOCALAPPDATA` there                                                                                                                                                                      | Its own text copy, in `finally`                                                                                   |
| `test_editor_panels.py`             | `pill_spline_editor.rs` (the panel heading, restored) and `pill_spline.rs` (one appended comment); writes `local/editor-validation/`                                                                                                                                                        | Its own text copies, in `finally`                                                                                 |
| `test_editor_builtin_panels.py`     | `pill_editor_shell_editor.rs` (the heartbeat cadence) and `shell.rs` (one appended comment); writes `local/editor-validation/` and a sandboxed `LOCALAPPDATA` there (its preferences file included)                                                                                         | Its own text copies, in `finally`                                                                                 |
| `test_editor_clipboard.py`          | Writes `local/editor-validation/` (commands file, sandboxed `LOCALAPPDATA`) and `local/temp/s18_drop.png`; the drop phase writes and then deletes `examples/project_rs/res/s18_drop.png` and its `.meta`, and removes the `res` directory the host creates                                  | Deletes the copy and its metadata in `finally`; the fixture under `local/temp/` stays for the next run            |
| `test_editor_reload_feedback.py`    | `pill_dummy_color.rs` (a type error appended, then removed) and `pill_editor_shell_editor.rs` (a `panic!` inserted into the update system's body, then removed); writes `local/editor-validation/` (commands file, sandboxed `LOCALAPPDATA`, the open-command recorder)                     | Its own text copies, in `finally`                                                                                 |
| `test_editor_selection.py`          | Writes `local/editor-validation/` (commands file, sandboxed `LOCALAPPDATA`)                                                                                                                                                                                                                 | Nothing tracked                                                                                                   |
| `test_shared_component_identity.py` | `pill_spline.rs`, `examples/project_rs/src/lib.rs`, `examples/project_rs/project_settings.yaml`                                                                                                                                                                                             | `BackupRegistry` for the sources (fresh timestamp), its own text copy for the settings                            |
| `test_csharp_bridge.py`             | `examples/project_cs/project_settings.yaml`, `src/Components.cs`, `src/Systems.cs`, `pill_spline.rs`, the generated C# mirror files (deleted once to check they regenerate); creates `src/BridgeProbe.cs`                                                                                   | `BACKUP`, original bytes and timestamp; the probe file is deleted                                                 |
| `test_csharp_analyzer.py`           | Creates `examples/project_cs/src/_AnalyzerProbe.cs`                                                                                                                                                                                                                                         | Deleted at the end unless `--keep-probe`                                                                          |
| `test_reload_edit_during_build.py`  | `pill_spline.rs`                                                                                                                                                                                                                                                                            | `BackupRegistry`, fresh timestamp                                                                                 |
| `test_hot_reload_assets.py`         | In `examples/master_renderer_test/res/`: `textures/helmet_emissive.jpg` (overwritten with junk bytes), its `.meta`, `materials/helmet.material`; moves the image pair into `textures/moved_by_suite/` and back. Also `pill_master_renderer_data.rs` (a comment appended to force a rebuild) | `BackupRegistry`, fresh timestamp; the move is undone in its own `finally`                                        |
| `test_hot_patch_coverage.py`        | One function body in every crate listed in `examples/project_rs/project_settings.yaml`, plus the project                                                                                                                                                                                    | `BackupRegistry`, original bytes and timestamp                                                                    |
| `test_patch_bookkeeping.py`         | A system in `examples/project_rs/src/` (a body edit, then a broken tail); creates `modules/target/hot/rollback.request`                                                                                                                                                                     | `BackupRegistry`, fresh timestamp; the request file is deleted                                                    |
| `test_wrapper_entry_points.py`      | Writes `modules/extensions/pill_dummy_color/generated/host_module_wrapper_probe/` (a generated-namespace crate)                                                                                                                                                                             | Deleted at the end unless `--keep-probe`; a host run prunes it as a stale wrapper too                             |
| `test_web_smoke.py`                 | Regenerates `build/pill_shipping_bundle/`; writes `examples/master_renderer_test/build/web/`                                                                                                                                                                                                | The bundle is snapshotted and restored                                                                            |
| `test_shipping_smoke.py`            | Regenerates `build/pill_shipping_bundle/` (gitignored)                                                                                                                                                                                                                                      | Not restored                                                                                                      |
| `test_editor_recovery.py`           | Writes `local/editor-validation/recovery-<run id>/`, a `LOCALAPPDATA` sandbox per scenario                                                                                                                                                                                                  | Nothing tracked                                                                                                   |
| `test_editor_viewports.py`          | Writes `local/editor-validation/viewports-<run id>/`, a `LOCALAPPDATA` sandbox per scenario                                                                                                                                                                                                 | Nothing tracked                                                                                                   |
| `test_editor_drag_out.py`           | Writes `local/editor-validation/drag-out-<run id>/`; moves the real mouse (manual)                                                                                                                                                                                                           | Nothing tracked                                                                                                   |

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

### Catalogue

Generated from the suites' own headers by `run_all.py --write-catalogue`;
edit the suite, not this table. `run_all.py --catalogue` prints the same table
with each suite's last recorded result from `.construction_site/runs/`.

<!-- catalogue:start (generated by run_all.py --write-catalogue) -->
| Suite                               | Tier            | Sets                                | Protects                                                                                                                                    |
| ----------------------------------- | --------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `test_asset_metadata.py`            | static          |                                     | Every .meta under examples/*/res has its asset, and no two assets share a guid.                                                             |
| `test_coding_standards.py`          | static          |                                     | Every .rs file follows the Pill comment and layout rules.                                                                                   |
| `test_engine_feature_drift.py`      | static          |                                     | Every build the host spawns resolves the same engine features as the host itself.                                                           |
| `test_engine_layout.py`             | static          |                                     | The engine core never links the facade or reads a registry; shared dylibs stay UI-free.                                                     |
| `test_harness_parsing.py`           | static          | hot-reload                          | The parsing the end-to-end suites decide pass or fail with, on real host output.                                                            |
| `test_log_contract.py`              | static          | hot-reload                          | Every host log token a suite waits for still exists in the Rust sources.                                                                    |
| `test_renderer_boundaries.py`       | static          |                                     | Only GPU modules link wgpu, the renderer API stays renderer-free, data crates GPU-free.                                                     |
| `test_suite_standards.py`           | static          |                                     | Suites keep their run_all header and stop re-forking the core helpers.                                                                      |
| `test_csharp_bridge.py`             | host            | hot-reload                          | C# hot reload: mirror layout, calls both ways, assembly swaps keeping state, layout migration.                                              |
| `test_editor_crates.py`             | host            |                                     | Extension editor crates load only in the editor posture and reload with their runtime crate.                                                |
| `test_editor_revision.py`           | host            |                                     | A module reload bumps the editor revision counter, so the editor drops its caches.                                                          |
| `test_hot_patch_coverage.py`        | host            | hot-reload                          | Body-only edits in every loaded crate are live-patched rather than reloaded.                                                                |
| `test_hot_reload_assets.py`         | host            |                                     | Live asset reimport: .meta edits, broken and restored images, moved files, renderer reloads.                                                |
| `test_hot_reload_migration.py`      | host            | hot-reload, nightly                 | Component schema changes on reload migrate the data, or are refused and rolled back.                                                        |
| `test_hot_reload_suite.py`          | host            | hot-reload, nightly-allowed-to-fail | Project and module hot reload keep data, migrate schemas, cascade and stay stable.                                                          |
| `test_multi_surface.py`             | host            |                                     | One renderer drives two windows; a renderer reload reattaches the second surface.                                                           |
| `test_patch_bookkeeping.py`         | host            |                                     | A failed reload keeps the live patch records, so rolling the patch back still works.                                                        |
| `test_pbr_native.py`                | host (manual)   |                                     | The windowed frontend presents frames and shuts down cleanly.                                                                               |
| `test_picking.py`                   | host            |                                     | Picking returns the entity under the cursor, or nothing, and outlines the hit entity.                                                       |
| `test_pill_ui.py`                   | host            |                                     | Exactly one pill_ui dylib loads in a dev host and it shapes and draws text; editing a stylesheet restyles the running UI without a compile. |
| `test_reload_edit_during_build.py`  | host            | hot-reload                          | A save made during a build wins: the newer edit is the one built and loaded.                                                                |
| `test_shared_component_identity.py` | host            |                                     | Types linked by two DLLs share one registration with #[pill(shared)], else startup fails.                                                   |
| `test_editor_builtin_panels.py`     | editor          | editor                              | The built-in panels: Inspector edits, system toggles, Console and Assets toolbar, undoable.                                                 |
| `test_editor_clipboard.py`          | editor          | editor                              | Clipboard and drag-and-drop: field copy and paste, entity round-trips and file drops.                                                       |
| `test_editor_commands.py`           | editor          | editor                              | The command layer: built-in commands by id, menus, the palette and live keymap presets.                                                     |
| `test_editor_drag_out.py`           | editor (manual) | editor                              | A real OS drag out of the editor carries the dragged asset's file path.                                                                     |
| `test_editor_native.py`             | editor          | editor                              | The native editor: startup, shell module, input routing, live patch, overlays, safe mode.                                                   |
| `test_editor_panels.py`             | editor          | editor                              | The panel registration API: an extension panel opens, records undo steps and live-patches.                                                  |
| `test_editor_recovery.py`           | editor          | editor                              | An unclean exit leaves a recovery document; the next start restores                                                                         |
| `test_editor_reload_feedback.py`    | editor          | editor                              | Reload feedback: failed reloads become Problems rows and toasts, and bad patches roll back.                                                 |
| `test_editor_selection.py`          | editor          | editor                              | Scene selection: clicks pick entities through the pick contract; gizmo drags are undoable.                                                  |
| `test_editor_undo.py`               | editor          | editor                              | Undo and redo: field edits, drags, component, entity and asset commands round-trip.                                                         |
| `test_editor_viewports.py`          | editor          | editor                              | Panels pop out into their own windows and redock; a popped-out                                                                              |
| `test_csharp_analyzer.py`           | build           |                                     | The PILL Roslyn analyzer rules: the C# project builds clean and a rule-breaking probe fails.                                                |
| `test_engine_exports.py`            | build           |                                     | Each engine dylib stays below the Windows limit of 65,535 exported symbols.                                                                 |
| `test_examples.py`                  | build           |                                     | Every example project builds in release.                                                                                                    |
| `test_shipping_smoke.py`            | build           |                                     | The shipping binary has no reload machinery, runs the project and never starts cargo.                                                       |
| `test_web_smoke.py`                 | build           |                                     | A project built for the browser mounts its assets and draws frames with WebGPU.                                                             |
| `test_wrapper_entry_points.py`      | build           |                                     | A wrapper DLL exports the full loadable-artifact entry-point set.                                                                           |
<!-- catalogue:end -->

### Static checks (no host)

| Script                         | Checks                                                                                                                                                                                                                                                                                                                                                |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test_harness_parsing.py`      | The parsing logic the end-to-end suites decide pass or fail with, against real captured host output. A matcher that stops matching returns nothing rather than raising, so this is the only place that failure is visible.                                                                                                                            |
| `test_log_contract.py`         | Every log token a suite matches on still exists in the Rust sources.                                                                                                                                                                                                                                                                                  |
| `test_coding_standards.py`     | The comment and layout rules over every `.rs` file: `//!` header with `# Responsibilities`, `// SAFETY:` on `unsafe`, `///` on `pub` items, ordered import groups, `mod tests` last. Exit 0 clean, 1 violations, 2 usage error.                                                                                                                       |
| `test_renderer_boundaries.py`  | The renderer split's dependency rules from the Cargo manifests: only a GPU module depends on `wgpu`, `pill_renderer_api` stays renderer-free, the host reaches renderer crates only through `*_dependency_graph` dependencies, and no project or data crate depends on a GPU module. `--self-test` proves each rule fails on a generated broken tree. |
| `test_asset_metadata.py`       | Under `examples/*/res`: no orphaned `.meta` files and no two assets sharing a guid. `--self-test` proves it catches both.                                                                                                                                                                                                                             |
| `test_wrapper_entry_points.py` | The wrapper path exports the whole loadable-artifact entry-point set: writes a throwaway `host_module_wrapper_probe` crate around `pill_dummy_color`, builds it and compares its PE export directory against the expected names. `--self-test` covers the reader; Windows-only.                                                                       |
| `test_csharp_analyzer.py`      | The `PILLxxxx` Roslyn analyzer rules (needs .NET 8 SDK): the real C# project builds with no PILL diagnostic, and a temporary probe that breaks every rule produces every diagnostic and fails the build.                                                                                                                                              |

### End-to-end suites (start a host, edit files)

| Suite                               | Covers                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test_hot_reload_suite.py`          | The main suite, in two sessions. **A** (`devops/tests/project` + `pill_spline`): project reload with data surviving, schema migration, types removed from the project, module reload keeping its data (`existing=1`), repeated reloads staying stable, a removed module type dropped and re-seeded (`existing=0` after the restore), rollback when init fails. **B** (`examples/project_rs` + `pill_spline`): the module-to-project cascade on its own - editing `pill_spline` reloads the module, the host queues a project reload, and the project's probe reports the new value, derived from the midpoint the running host printed (the check the folded `test_module_project_auto_reload.py` made alone). Also checks a clean restart.                                                                                                                                                                                                     |
| `test_hot_reload_migration.py`      | Component schema changes on reload: unchanged shape (fast path), added, removed, renamed and reordered fields, persistable downgraded to plain, a widened `Vec` element type, a retyped one whose snapshot no longer parses (row reset), and an alignment change that must be refused and rolled back. Asserts migrated entity counts and values. `--cycles N` repeats it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `test_multi_surface.py`             | One renderer, two windows: the standalone dev host's test frontend (`PILL_SECOND_VIEW=1`) opens surface 1 with its own camera; both present, a renderer reload reattaches surface 1, and closing its window detaches only it. Backs up and restores `pill_master_renderer/src/lib.rs`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `test_picking.py`                   | Picking and outlines: the standalone dev host's test frontend (`PILL_PICK_TEST=1`) picks the center and the corner of `master_renderer_test`'s window in one frame; the center answers with the model's entity, the corner with nothing, and the hit entity is outlined while the host keeps drawing without a GPU error.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `test_pill_ui.py [binary]`          | The shared `pill_ui` dylib and live restyling, in one suite: the standalone dev host's UI probe (`PILL_UI_PROBE=1`) shapes and draws a sample with the bundled font without a project - exactly one `pill_ui.dll`, beside one `pill_core.dll` and one `pill_engine_core.dll`, is mapped in the running process, and the probe reports glyph quads, atlas regions and vertices - and then the probe's style mode (`PILL_UI_PROBE_STYLE=<file>`) renders the UI Gallery through a stylesheet while the suite edits the file, waiting for the `restyled revision=1` render with the new rule count. The probe draws through `pill_ui` alone, so it needs no renderer: with no path argument it builds (or, under `--skip-build`, reuses) the default dev host posture.                                                                                                                                                                             |
| `test_editor_crates.py`             | Extension editor crates: an ordinary host never builds or loads `pill_spline_editor`; in the editor posture (`PILL_EDITOR_POSTURE=1`) it loads after `pill_spline`, reloads after every `pill_spline` reload, and an edit to it reloads only it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `test_shared_component_identity.py` | A component and a resource linked by both the project and a module DLL, so each binary has its own `TypeId`. With `#[pill(shared)]` they bind to one registration; without it startup must fail with a collision error. Also: a module reload with two registrants, a resource inserted by one DLL and read by the other, and releasing values after a failed reload or failed first load. Runs the windowed host, because `project_rs` links the renderer.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `test_csharp_bridge.py`             | The C# side (needs .NET 8 SDK): the generated mirror's exact layout, a warning-free managed build, calls in both directions, assembly swaps keeping state, three swaps in one session (the old `AssemblyLoadContext` must unload), component layout migration, systems re-registered after a signature change, and a changed startup method refused while the old assembly keeps running. Also checks the committed mirror matches what the host generates.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `test_reload_edit_during_build.py`  | Two saves, the second during the build started by the first: the newer save must win and actually be built. `--second-edit-delay` tunes the gap.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `test_hot_reload_assets.py`         | Live asset reimport in the headless host on `examples/master_renderer_test`: a `.meta` edit reimports in place, a broken image keeps the loaded value and recovers when restored, reimport still works after the renderer data crate reloads, a moved image and `.meta` are followed without a new import, and a material edit applies.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `test_hot_patch_coverage.py`        | Live patching per crate. Makes one body-only edit in every loaded crate (discovered from the project settings) and reports the host's verdict: `PATCHED`, `FELL BACK` with the reason, or `NO FAST PATH`. Also checks the cached compiler flags for split arguments. `--strict` fails on crates with no fast path.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `test_patch_bookkeeping.py`         | A live patch, then a reload that fails to build: the failed reload must keep the patch records, so rolling the patch back still works.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `test_editor_revision.py`           | A module reload bumps the host's editor revision counter (`editor revision bumped`), which the editor uses to drop its caches.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `test_editor_native.py`             | The native editor end to end: startup, the shell module loading (`pill_editor_shell_editor loaded`) and painting panels, scripted clicks routed to the scene surface or the UI (`test click ... target=`), a live patch of the shell's system with its frame counter continuing (state kept), and the clean exit code on five further runs. Needs the built `pill_editor` binary; skips off Windows until the platform layer grows its X11 and Wayland implementations (Stage 24).                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `test_editor_commands.py`           | The command layer end to end: the built-in commands (pause, step, save/reset layout) by id with the engine batch applied at the frame boundary, a menu title and item clicked with pointer events, the palette opened and clicked, a keymap preset switched live through the keybindings file with a single override on top and a conflict reported by both names, and the spline editor crate's command surviving a reload of that crate. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `test_editor_undo.py`               | The undo/redo history end to end, on a paused engine: a field edit undoes to its old value and redoes; a drag (three sets sharing a coalesce key) is one step; adding, editing and removing a component round-trips with the removed values restored; creating, destroying and restoring an entity remaps the fresh handle and keeps its values; the preferences' step cap trims the oldest entry; and an edit still undoes after the spline editor crate reloads. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                                                                                                                                                                                                                                                                                                                                                               |
| `test_editor_panels.py`             | The panel registration API end to end: `pill_spline_editor`'s Spline Tool registers and opens from the Window menu; its control-point command is one recorded undo step; re-seed runs through the module's system; a body-only edit of the panel live-patches (`LIVE ms`); editing `pill_spline` reloads it and the editor crate with the tab kept and the command re-registered; the harness' panic panel is contained while the shell keeps painting; an unregistered panel shows the placeholder. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                                                                                                                                                                                                                                                                                                                             |
| `test_editor_builtin_panels.py`     | The built-in panels end to end, driven through named widgets: selecting an entity fills the Inspector and a named field edit is one undoable step that undo restores; a system toggle is undoable too; the Console carries both channels, its level/channel buttons and search field work and its lines survive a shell reload; the Preferences panel and its file stay in step in both directions (read back from disk); the Assets panel lists the project's files; the shell live-patches on a body edit and a structural edit reloads its crate with frames, console and selection kept. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                                                                                                                                                                                                                                     |
| `test_editor_clipboard.py`          | The clipboard and drag-and-drop end to end (Stage 18), on a paused engine: a field's selected text copies with the field's own Ctrl+C and pastes into another field as one undo step; `editor.copy`/`editor.paste` round-trips an entity through the registered clipboard format plus its readable text, one undo step; the Inspector's Copy button pastes a component onto another entity with its values; a hovering file previews over the Assets panel and a drop copies it into `res/`, the pipeline imports it with metadata, and undo removes the copy; a drag out enters and leaves OLE's drag loop (the copy into Explorer itself is in the D26 manual checklist); a second editor instance pastes the entity copied by the first, from the system clipboard, with the copied component values. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                         |
| `test_editor_reload_feedback.py`    | Reload feedback end to end (Stage 19): a type error injected into `pill_dummy_color` fails its reload, appears as a parsed Problems row carrying the module, file, line and the compiler's message, raises a failure toast and bumps the panel revision, and the failed generation keeps running; `activate problems.open.0` opens the row's `file:line` through the configured command, proved by a recorder process; restoring the source clears the row (count 0), reports the reload toast with its build time and moves the panel revision again; then a `panic!` patched into the shell module's own update system is contained, logged once, rolled back automatically after three consecutive panicking frames, reported by a rollback toast and a Problems row, and frames keep arriving on the previous code until the restored body patches in again. Needs the built `pill_editor` binary; skips off Windows until Stage 24. |
| `test_editor_selection.py`          | Scene selection end to end (Stage 20, selection half): a 3x3 grid of scripted clicks inside the Scene tab's dock rectangle, through the renderer's pick contract, must find an entity - which moves the shared selection and publishes the renderer's outline set - and a click on the view's corner hits nothing and clears both. Needs the built `pill_editor` binary; skips off Windows until Stage 24.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |

### Build and smoke checks

| Script                   | Checks                                                                                                                                                                                                                                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `test_examples.py`       | Builds every example under `examples/` in release (found by `Cargo.toml` or `*.csproj`) and reports artifact sizes.                                                                                                                                                                                                |
| `test_shipping_smoke.py` | Builds the statically linked shipping binary for `examples/project_rs`, checks the binary contains no reload-only strings, then that it reaches the project loop, initializes its extensions, and never starts cargo or any reload machinery.                                                                      |
| `test_web_smoke.py`      | Builds `examples/master_renderer_test` for the browser, serves it, and opens it in headless Chrome or Edge with WebGPU. Checks the asset pack mounts, frames are presented with a camera, and no engine, WebGPU or page error occurs. Needs the wasm target and `wasm-pack`; `PILL_WEB_BROWSER` picks the browser. |

### Manual scripts (not in `run_all.py`)

| Script                        | Checks                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test_pbr_native.py <binary>` | The windowed frontend on Windows: first frame presented, resize, minimise and restore, clean shutdown. Logs to `local/renderer-validation/`. Only touches the process it starts.                                                                                                                                                                                                                                                                                                                    |
| `test_editor_drag_out.py`     | A real drag out of the editor (Stage 18): moves the real mouse onto the first asset row, presses, drags out of the window (the editor starts an OLE drag) and drops back on the editor - winit's own drop target parses the CF_HDROP file list and names the project's file. Needs an unattended screen (it moves the cursor and raises the editor window); dropping into a real Explorer window is the human step of the D26 checklist. Needs the built `pill_editor` binary; Windows only. |

## Writing a new end-to-end suite

- Import from `core.suite_common` after putting `devops/` on `sys.path`, as the
  existing suites do.
- Call `ensure_host_lock()` first in `main`, before anything that reads or
  writes a shared path. `launch_process` and `kill_stale_hosts` take it too, so
  a suite that never touches a host pays nothing for it.
- Declare one `HostPosture` and use it for both halves of the host's life:
  `prepare_host(posture, args)` builds it (or verifies the one `--skip-build`
  reuses) and `host_command(posture)` launches it. Declare the suite's
  arguments with `add_common_suite_arguments(parser)`, which adds the
  `--timeout-scale` and `--skip-build` that `run_all.py` passes through.
  Launch the host with `launch_process` and call `kill_stale_hosts` before it.
- `capture` every file you will edit **before** the first edit, use a
  `BackupRegistry` (or the shared `BACKUP`), and call `restore_all` in a
  `finally`. Decide `reset_mtime` by whether the host's last build was of the
  original content; `BACKUP.captured(path)` tells you whether cleanup owns a
  file at all.
- Edit with raw bytes when line endings matter, otherwise with `atomic_write`.
- Take `start_index = monitor.line_count` before each edit and only search
  output after it, so an earlier reload's line can't satisfy the wait.
- Put new tokens in constants named `*_TOKEN` so `test_log_contract.py` checks them.
- Give the file a `PROTECTS:` and a `TIER:` line: `run_all.py` finds every
  `test_*.py` here by itself and orders it by those, so there is no list to add
  it to. Add it to the table above by hand, or with
  `run_all.py --write-catalogue`.
- If the suite guards a path no rule in `devops/ci_cd/check_relevant.py`
  matches, add the area there (`area_checks` and `RULE_GLOBS` together, kept in
  step), or the scoped selection falls back to the quick gate for it.

## Gate matrix

The gates a change must keep green at stage boundaries and milestones. For
iteration, `check_relevant.py` runs the subset the change actually needs (see
[Run only what your change needs](#run-only-what-your-change-needs)). Run
these from `modules/`. The counts were measured on 2026-09-17; record what you
actually measure and update a row in the same commit that changes it.

| Gate                      | Command                                                         | Expected         |
| ------------------------- | --------------------------------------------------------------- | ---------------- |
| Engine unit tests         | `cargo test -p pill_engine --lib --offline`                     | 322 passed       |
| Engine, all targets       | `cargo test -p pill_engine --offline`                           | all targets pass |
| Host, default posture     | `cargo test -p pill_host --lib --offline`                       | 98 passed        |
| Host, rendering           | `cargo test -p pill_host --features rendering --lib --offline`  | 152 passed       |
| Host, shipping posture    | `cargo test -p pill_host --no-default-features --lib --offline` | 25 passed        |
| Clippy, formatting, lints | `python devops/ci_cd/verify.py --quick`                         | clean            |
| End-to-end suites         | `python devops/tests/run_all.py`                                | all pass         |

## Generated C# mirrors

`modules/extensions/<extension>/<module>/generated/<module>_Components.g.cs` files are
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
  `test_editor_revision` and the settings
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
