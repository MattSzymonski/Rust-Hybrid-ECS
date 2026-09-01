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
| `test_hot_reload_migration.py` | Table-driven schema-migration suite: fast path (shape unchanged), add field, remove field, rename field, downgrade persistable→plain, with per-component migrated entity-count assertions. Cycles repeatable (`--cycles N`). |
| `test_module_project_auto_reload.py` | The module→project cascade in isolation: editing `pill_spline` reloads the module, the host queues a project reload, and the project probe reports the new value. |
| `test_csharp_bridge.py` | The C# side (needs .NET SDK 8 on PATH). **Bridge and codegen**: the host auto-generates a module's C# mirror with the exact layout (`Size = 196`, alignment pad), a component-less module writes **no** mirror, the managed build is warning-free, the bridge probe proves **both directions**, and the mirror is regenerated when deleted. **Hot reload**: a behavior-only edit swaps the assembly and state survives (`csharp_hot_reload`); three consecutive swaps in one session all succeed, which is the observable end of the collectible `AssemblyLoadContext` actually unloading (`csharp_repeated_reload_stability`); and each of the three contracts the loader refuses to let a reload break is asserted individually - component layout, system signature, and startup methods - each checking that the refusal names the right contract AND that the previously loaded assembly keeps running (`csharp_rejects_*`). |
| `test_reload_edit_during_build.py` | The save-during-rebuild case no other suite covers. The host cancels an in-flight build when a newer save arrives, so that the newer sources win; this asserts the newer save is then actually built. Before the fix it was not: the bookkeeping recorded a counter value read *after* the reload, which included the save that caused the cancellation and marked it handled, stranding the edit on disk uncompiled with no error printed. |
| `test_hot_patch_coverage.py` | Live-patch coverage across every crate the host loads. A patch that cannot be built falls back to a full reload, so the fast path can die for a whole crate with nothing failing - the edit still lands, just seconds later instead of milliseconds. This suite makes one body-only edit per crate and reads the host's own verdict: `PATCHED` (with the route and timing), `FELL BACK` (with the refusal code and reason), or `NO FAST PATH` for a crate with neither an annotation nor a `build.rs` inventory. Crates and edit targets are **discovered** - from the project's `project_settings.yaml` and by scanning for a literal inside a patchable function - so a module added later is covered without editing the suite. Also scans the host's compiler-flag caches for split arguments, which catches a malformed replayed `rustc` line even when the patch happened to succeed. `--strict` also fails on crates with no fast path. |
| `test_basic.py` | The CI fast checks: `cargo fmt --check` and `cargo clippy -D warnings` over the workspace, **`cargo test --workspace` in both feature configurations** (default and `hot_patch` - the feature is additive, so the default run never compiles the live-patching code and left 62 tests outside every lane until this was added), plus launcher-driven native/WASM builds, the WASM size budget, a dev-server smoke test and the native performance benchmark. The three launcher-driven checks SKIP in this repository (no PillLauncher project layout); fmt, clippy and the tests run for real. |
| `test_coding_standards.py` | Pill comment & layout lint over every `.rs` file: `//!` module header with a `# Responsibilities` section, `// SAFETY:` above unsafe blocks, `///` docs on public items, ordered import-group headers, and `mod tests` as the last top-level section. Ported from `run_coding_standards_test.sh`, which now just invokes it. Exit 0 clean / 1 violations / 2 usage error. |
| `test_examples.py` | Builds every example under `examples/` in release and reports artifact sizes. Examples are discovered by convention (a `Cargo.toml` or a `*.csproj`), so adding one needs no edit. Ported from `run_examples_tests.sh`, which now just invokes it. |
| `devops/core/suite_common.py` | Not a test, and not in this directory. Single source of truth for paths, log tokens, timeouts, the color `print` wrapper, the `OutputMonitor` (rolling buffer + counter-tick tail), atomic source editing, and host process helpers. Shared by all four suites (audit opportunity 5.14) **and** by the hot-reload harness and cold-start startup timing in `devops/benchmarks/`, which is why it lives in `devops/core/`. A reworded host log token must keep both sides working. |

## Quick start

```powershell
# Full net via one entry point (recommended)
bash devops/ci_cd/run_hot_reload_tests.sh                 # 4 suites
bash devops/ci_cd/run_hot_reload_tests.sh --skip-build    # fastest lane

# Individual suites
python devops/tests/test_hot_reload_suite.py
python devops/tests/test_hot_reload_migration.py --cycles 2
python devops/tests/test_module_project_auto_reload.py
python devops/tests/test_csharp_bridge.py
python devops/tests/test_hot_patch_coverage.py

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
`modules/optional/pill_spline/src/lib.rs`, the generated mirror files) is
backed up at startup and restored afterwards.

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
- **Dynamic component coexistence** with native components in one archetype
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
