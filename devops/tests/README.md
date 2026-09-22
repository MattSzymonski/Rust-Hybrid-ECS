# Hot-Reload Regression Net

The test infrastructure for the hot-reload / persistence architecture. This is
the **regression net** that must stay green while the audit's simplification
opportunities (`local/documents/data_audit.md`, critical review in
`local/documents/data_audit_review.md`) are implemented. Every fix lands behind
these tests.

This directory holds **pass/fail tests only**, and every suite in it runs
standalone from a console. Performance measurement lives in
`devops/benchmarks/`, because it reports numbers rather than asserting on them
- see "Performance measurement (moved)" below. Both sides share
`devops/core/`; see `devops/README.md` for the full layout.

## Suites

| Suite | Covers |
| --- | --- |
| `test_harness_parsing.py` | **Unit tests for the harness itself** - the only suite here that launches nothing and finishes in under a second. Every other suite decides pass or fail by matching strings in host output, so that matching logic is load-bearing and its failure mode is silence: a regex that stops matching returns nothing rather than raising, and the suite reports the wrong reason. Uses real captured host output as fixtures, because invented strings keep passing through exactly the drift that breaks the real ones. Covers the analytics-line regex (including function names containing spaces and commas, which a `\S+` group silently dropped), optional-field back-compat, the `PROVABLE_ROUTES` mirror of `PatchRoute::is_provable`, cargo-test totalling, and the coverage suite's brace scanner. |
| `test_hot_reload_suite.py` | Full suite, two sessions. Session A (devops/tests/project + pill_spline): project reload + data survival, schema migration, project forgotten-type, module reload with data survival (`existing=1`), **repeated same-config reload stability** (`module_double_reload`, pins per-artifact TypeId stability / no per-reload growth), module forgotten-type with **drop→re-seed** (`existing=0` after restore, pins drop-at-detection end-to-end), init-failure rollback. Session B (examples/project_rs + pill_spline): module→project **cascade** plus **module↔project coexistence** (`xxsees 1 spline(s)` after both reloads). Also verifies the up-to-date build fast path on restart. |
| `test_hot_reload_migration.py` | Table-driven schema-migration suite: fast path (shape unchanged), add field, remove field, rename field, field reorder, downgrade persistable→plain, widen a `Vec` element type, and a `Vec` element retype whose snapshot no longer parses (the fallback resets the row) - with per-component migrated entity-count assertions and value witnesses. One scenario is a refused alignment widening: the reload rolls back and the counter keeps ticking. Cycles repeatable (`--cycles N`). |
| `test_module_project_auto_reload.py` | The module→project cascade in isolation: editing `pill_spline` reloads the module, the host queues a project reload, and the project probe reports the new value. |
| `test_shared_component_identity.py` | **Component identity across the project/module boundary.** `pill_spline::Spline` is linked by the project (which depends on the crate directly so it can write `Query<&Spline>`) *and* by the module DLL loaded alongside it, so each binary gets its own `TypeId` for one type. Seven scenarios; the second gives the first its meaning, and the fifth does the same for the fourth: with `#[pill(shared)]` the two registrations bind to one component and startup is clean; **removing it must make startup fail** with the peer-collision error, which is only possible because the `TypeId`s genuinely differ - equal ones would make the second registration idempotent and silent. That control is also the regression guard: before the collision check existed, the second registration silently evicted the first's persist entries and every row it owned vanished at the next reload with no error. Runs with `--features rendering`, which is required rather than preferred - `examples/project_rs` links `pill_master_renderer`, so a host without it resolves `pill_core` differently from the project and the project DLL fails to load (os error 127). A third scenario covers the combination nothing else does: a **module reload with two registrants**. `test_hot_reload_suite.py` reloads `pill_spline` against the fixture project, which does not link the crate, so only one binary ever registers `Spline`; here the project links it too, so the swap makes both re-register against a column that already holds rows - the only path exercising `rehome_native_columns` with two registrants competing for the column's function table. Asserts `existing=1` (rows survived), no collision, and no crash. A fourth covers **resources**, which had the same identity problem plus one components never did - a `Box<dyn Any>` vtable belonging to the artifact that inserted the value, in a store nothing clears on reload: it declares a resource in `pill_spline`, inserts it from the module DLL, and reads it from the project, which returned `None` before the fix. Both binaries also register a *writer* system for that one resource, so the same run covers the scheduler across two DLLs: each side must report running, and the host must never report two live mutable borrows of it. Two more cover how a resource is **released**, which is where the resource work's defects were found by reading it back. The fifth is the control for the resource guard: a rival type in the project claims the module's shared name, and the conflict must fail the project's init - the guard is raised from the artifact's own registration code, after the engine's own component-registration drain, so recording it is not enough. The sixth drives a reload whose generation claims a resource of its own and then fails its init: the rollback has to release that value while the failing image is still mapped, and the host has to survive it. The module-reload scenario additionally reads back whether the engine's own `Time` outlived two swaps, which is what catches a generation retiring a resource it never owned - the failure mode that bug had was silent, and it took a second reload for a probe to see it. The seventh is that same failing generation as the module's *first* load, where there is no previous generation to keep: its systems and the world are released before its image goes, and removing that release makes the host fault on the way out instead of reporting the setup failure. |
| `test_csharp_bridge.py` | The C# side (needs .NET SDK 8 on PATH). **Bridge and codegen**: the host auto-generates a module's C# mirror with the exact layout (`Size = 196`, alignment pad), a component-less module writes **no** mirror, the managed build is warning-free, the bridge probe proves **both directions**, and the mirror is regenerated when deleted. **Hot reload**: a behavior-only edit swaps the assembly and state survives (`csharp_hot_reload`); three consecutive swaps in one session all succeed, which is the observable end of the collectible `AssemblyLoadContext` actually unloading (`csharp_repeated_reload_stability`); and a managed component layout change is now **migrated** on reload instead of refused (`csharp_migrates_component_layout_change`, which also asserts the bridge probe keeps streaming; `csharp_migrates_field_default` adds a field with a declared `[EcsFieldDefault]` and asserts the next generation reads it), while a changed **system signature** is now re-registered rather than refused (`csharp_reregisters_on_system_signature_change`: the host clears the project's systems, rebuilds them from the arriving assembly, and the renamed system keeps streaming). A changed **startup method** remains the one refused contract, because startups are not re-run on reload and a changed set would silently never execute - `csharp_rejects_startup_change` checks that the refusal names it AND that the previously loaded assembly keeps running. The run also exercises the **managed resource surface** without a dedicated scenario, because `examples/project_cs` depends on it: its `SimulationTime` is an `[EcsResource]` reached through `ResMut<T>`, so a host that failed to register the resource, to seed its value, or to reflect the access as a resource rather than a component cannot reach the project loop at all - which is exactly how two defects in that path were caught. The ECS report's `shared resources` line names it. |
| `test_csharp_analyzer.py` | The **compile-time** half of the C# scripting guards (needs .NET SDK 8). The `PILLxxxx` rules in `modules/pill_csharp_runtime/analyzers` are the only thing that catches several hazards before a host runs at all - an `await` whose continuation escapes its frame, a mutable static two systems in one parallel batch race on, a static event that roots the retiring `AssemblyLoadContext` - and a rule that quietly stops firing removes a protection nobody would notice was gone. Two scenarios: the real gameplay project must build with **no** PILL diagnostic, which is what keeps a false positive from being worse than no rule and also proves the analyzer is still wired into the project rather than silently absent; and a probe declaring exactly one violation of each rule must produce **every** diagnostic **and fail the build**, because a rule reported as a warning would let a hazardous project ship. The probe is not committed - it does not compile by construction - so it exists only for the duration of the run. Seconds, no host. |
| `test_reload_edit_during_build.py` | The save-during-rebuild case no other suite covers. The host cancels an in-flight build when a newer save arrives, so that the newer sources win; this asserts the newer save is then actually built. Before the fix it was not: the bookkeeping recorded a counter value read *after* the reload, which included the save that caused the cancellation and marked it handled, stranding the edit on disk uncompiled with no error printed. |
| `test_hot_patch_coverage.py` | Live-patch coverage across every crate the host loads. A patch that cannot be built falls back to a full reload, so the fast path can die for a whole crate with nothing failing - the edit still lands, just seconds later instead of milliseconds. This suite makes one body-only edit per crate and reads the host's own verdict: `PATCHED` (with the route and timing), `FELL BACK` (with the refusal code and reason), or `NO FAST PATH` for a crate with neither an annotation nor a `build.rs` inventory. Crates and edit targets are **discovered** - from the project's `project_settings.yaml` and by scanning for a literal inside a patchable function - so a module added later is covered without editing the suite. Also scans the host's compiler-flag caches for split arguments, which catches a malformed replayed `rustc` line even when the patch happened to succeed. `--strict` also fails on crates with no fast path. |
| `test_patch_bookkeeping.py` | The order between a project reload and the patch records it invalidates. A reload that **fails** (build error, load refusal, rolled-back init) keeps the current image, and with it every live patch installed in it - so the bookkeeping may only be dropped once the image actually changed. Patches a project system, breaks the project build, and then rolls the patch back through the failed reload: the rollback must succeed because the patch is still installed. Before the fix it refused with "has not been patched in this session", because the failed attempt had already cleared the records. |
| `test_basic.py` | The CI fast checks: `cargo fmt --check` and `cargo clippy -D warnings` over the workspace, **`cargo test --workspace` in both feature configurations** (default and `hot_patch` - the feature is additive, so the default run never compiles the live-patching code and left 62 tests outside every lane until this was added), plus launcher-driven native/WASM builds, the WASM size budget, a dev-server smoke test and the native performance benchmark. The three launcher-driven checks SKIP in this repository (no PillLauncher project layout); fmt, clippy and the tests run for real. |
| `test_coding_standards.py` | Pill comment & layout lint over every `.rs` file: `//!` module header with a `# Responsibilities` section, `// SAFETY:` above unsafe blocks, `///` docs on public items, ordered import-group headers, and `mod tests` as the last top-level section. Ported from `run_coding_standards_test.sh`, which now just invokes it. Exit 0 clean / 1 violations / 2 usage error. String-literal lookalikes are ignored: the host's codegen carries a C# keyword table that lists `unsafe`, and a quoted token is data rather than a declaration. |
| `test_examples.py` | Builds every example under `examples/` in release and reports artifact sizes. Examples are discovered by convention (a `Cargo.toml` or a `*.csproj`), so adding one needs no edit. Ported from `run_examples_tests.sh`, which now just invokes it. |
| `run_all.py` | Not a test either: the batch entry point. Runs every suite above in the documented order, streams each suite's own output, and ends with a PASS/FAIL summary and a non-zero exit when anything failed. `--list` prints the order, `--only NAME ...` picks suites, `--keep-going` runs the rest after a failure. |
| `devops/core/suite_common.py` | Not a test, and not in this directory. Single source of truth for paths, log tokens, timeouts, the color `print` wrapper, the `OutputMonitor` (rolling buffer + counter-tick tail), atomic source editing, and host process helpers. Shared by every suite (audit opportunity 5.14) **and** by the hot-reload harness and cold-start startup timing in `devops/benchmarks/`, which is why it lives in `devops/core/`; it also owns the machine-global host lock (`ensure_host_lock`) that serializes host-driving suites. A reworded host log token must keep both sides working. |

## Quick start

```powershell
# Full net via one entry point (recommended)
bash devops/ci_cd/run_hot_reload_tests.sh                 # 4 suites
bash devops/ci_cd/run_hot_reload_tests.sh --skip-build    # fastest lane

# Every suite, in order, with one summary
python devops/tests/run_all.py
python devops/tests/run_all.py --only test_hot_reload_migration.py

# Individual suites
python devops/tests/test_hot_reload_suite.py
python devops/tests/test_hot_reload_migration.py --cycles 2
python devops/tests/test_module_project_auto_reload.py
python devops/tests/test_shared_component_identity.py
python devops/tests/test_csharp_bridge.py
python devops/tests/test_hot_patch_coverage.py
python devops/tests/test_patch_bookkeeping.py

# Static, build and CI checks (no host launch)
python devops/tests/test_coding_standards.py
python devops/tests/test_examples.py
python devops/tests/test_basic.py code_linting
```

The shell wrappers in `devops/ci_cd/` forward every argument through, so
`bash devops/ci_cd/run_coding_standards_test.sh --root modules` and
`python devops/tests/test_coding_standards.py --root modules` are equivalent.

All suites accept `--timeout-scale S` for slow machines. Every file the suites
touch (`examples/project_rs/project_settings.yaml`, `devops/tests/project/src/lib.rs`,
`examples/project_rs/src/lib.rs`, `examples/project_cs/src/Systems.cs`,
`modules/extensions/pill_spline/src/lib.rs`, the generated mirror files) is
backed up at startup and restored afterwards.

Suites that drive a host serialize themselves. `ensure_host_lock`
(`devops/core/suite_common.py`) takes a machine-global exclusive lock before a
suite kills stale hosts or launches one, prints a single `[WAIT]` line while
another suite holds it, and is released by process exit - so two host-driving
suites started at once queue instead of each one's stale-host cleanup killing
the other's host, which is how a migration scenario once read as flaky. The
host-free suites (`test_harness_parsing.py`, `test_coding_standards.py`,
`test_log_contract.py`) do not take the lock and stay safe to run beside
anything.

## Gate matrix

The gates a change must keep green, and what they report at `b4fc353`. Run
them from `modules/`, with `$env:CARGO_BUILD_RUSTC_WRAPPER=""` (sccache is
broken machine-wide) and `--offline` for host builds. Record the numbers you
actually measure - this table is a starting point, not a promise - and update
a row in the same commit that moves it.

| Gate | Command | Expected |
| --- | --- | --- |
| Engine unit tests | `cargo test -p pill_engine --lib --offline` | 322 passed |
| Engine, all targets | `cargo test -p pill_engine --offline` | all targets pass |
| Host, default posture | `cargo test -p pill_host --lib --offline` | 98 passed |
| Host, rendering | `cargo test -p pill_host --features rendering --lib --offline` | 152 passed |
| Host, shipping posture | `cargo test -p pill_host --no-default-features --lib --offline` | 25 passed |
| Clippy | `cargo clippy` for `pill_engine`, `pill_core`, `pill_host` in each posture | clean in every posture |
| Formatting | `cargo fmt --check` | clean |
| Comment & layout lint | `python devops/tests/test_coding_standards.py --root modules` | 0 violations |
| End-to-end suites | `python devops/tests/run_all.py` | all pass; the suites serialize themselves |

Measured on 2026-09-17, after plan items 2.14 (zero-sized components) and 2.15
(the gates that had regressed). Every row above is green; a red row is a
regression, not a known exception.

## Generated mirrors (policy)

`modules/extensions/<module>/generated/<module>_Components.g.cs` files are **tracked
as a committed bootstrap**. They are build inputs, not artifacts: `examples/project_cs`
links them (and so do standalone `dotnet build`s), so the C# side must compile without
a host run. The host regenerates every module's mirror on start and reload, but writes
only when the content differs (`pill_host/src/csharp/codegen.rs`), so a matching module
leaves the file - and its mtime - alone.

Drift between the tracked copy and what the host generates is therefore a real failure
mode, and `test_csharp_bridge.py` catches it: the `csharp_codegen_rebuild` scenario
captures the tracked file's bytes before the first host start, deletes the file,
restarts the host, and requires the regenerated file to be byte-identical to the
captured copy. When a module's real layout changes, run the host once and commit the
regenerated file; the suite fails with that instruction if you forget.

## Performance measurement (moved)

Hot-reload **performance measurement** is not a test and no longer lives here.
It reports timings rather than asserting on them, so it lives with the other
benchmarks:

    devops/benchmarks/hot_reload.py            (stores a measurement)
    devops/benchmarks/hot_reload_harness.py    (the raw harness)

```powershell
# Store a measurement and browse/compare it
python devops/pill_lab/pill_lab.py hot-reload --iterations 5
python devops/pill_lab/pill_lab.py compare hot_reload
python devops/pill_lab/pill_lab.py serve

# Or run the harness directly, for the flags Pill Lab does not surface
python devops/benchmarks/hot_reload_harness.py --iterations 5 --csv perf.csv
python devops/benchmarks/hot_reload_harness.py --csharp-only --max-wall-ms 5000
```

Both the suites here and that harness import `devops/core/suite_common.py`
for the host process plumbing, so a reworded host log token has to keep both
working. See `devops/pill_lab/README.md` for what the benchmarks measure.

## Rust unit tests

`cargo test --package pill_engine --offline` (136 tests) pins the engine-level
invariants the audit fixes must preserve, including:

- **Idempotent registration** across the plain and persistable paths — one
  registry entry, one bit, one set of persist maps per type
  (`component_registry.rs`, `persistence.rs`).
- **Registry `remove` / re-register** — a forgotten type's entry is fully gone
  and re-registration allocates a fresh bit (`component.rs`).
- **Drop-at-detection** — `drop_forgotten_components` removes only the forgotten
  columns, survivors keep their data, re-seeding works (`persistence.rs`).
- **Descriptor component coexistence** with native components in one archetype
  (`world.rs`).

## Review notes (2026-08-23)

The suites were reviewed and expanded before starting the audit fixes:

**Gaps found and closed:**
1. No test that repeated same-config module reloads stay stable (data survives,
   no accumulation). → `module_double_reload` asserts `existing=1` twice.
2. Drop-at-detection was only asserted via the warning, never via the re-seed.
   → `module_forgotten_type` now asserts `existing=0` after the restore reload.
3. Module↔project coexistence (same type name, two TypeIds) was only implicit.
   → Session B asserts `xxsees 1 spline(s)` after the cascade.
4. Token / monitor / process plumbing was copy-pasted across three suites and
   had already drifted. → extracted into `devops/core/suite_common.py`.
5. The devops runner only ran one of the three suites. → now runs all three.

**Things intentionally NOT asserted (so fixes can land cleanly):**
- The schema-hash *implementation* (audit 4.3 will drop `TypeId`/`size` from it).
  The migration suite's fast-path and shape-driven scenarios pin the *intent*
  (shape changes migrate, shape-unchanged edits fast-path), independent of how
  the hash is computed.
- The two global registration-sequence logs (audit 3.2/4.1 will replace them
  with registration-scoped sets). The suites assert observable behavior
  (warnings, `existing=` counts, migration logs), not the log data structure.

**Known quirks (do not "fix" silently):**
- `tests/project` and `examples/project_rs` both produce a crate named
  `project`, so both write `target/debug/project.dll`. A stale `project.dll`
  can make the mtime fast path load the wrong artifact (the "Counter did not
  tick" flake). Workaround: delete `modules/target/debug/project.dll` before a
  suite run. A project-identity marker in the up-to-date check is a future fix.
- The host floods "counter tick" lines; the `OutputMonitor` routes them to a
  dedicated small tail so they cannot evict the reload lines scenarios assert
  on. Keep this design in any monitor change.
- The migration suite's entity counts are cumulative per cycle (the project
  re-seeds entities on every reload). Adding scenarios to
  `test_hot_reload_migration.py` changes the counts; add scenarios to the main
  suite instead unless a count change is intended.

---

## Hot-Reload Migration Tests

See the module docstring at the top of `test_hot_reload_migration.py`
for full documentation.

```powershell
python devops/tests/test_hot_reload_migration.py
python devops/tests/test_hot_reload_migration.py --cycles 5
```

## Module-project auto-reload test

`test_module_project_auto_reload.py` verifies that editing an optional module
the project links directly (for example `pill_spline`) reloads the project as
well, so the project's embedded copy of the module code picks up the change.

```powershell
python devops/tests/test_module_project_auto_reload.py
```
