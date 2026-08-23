# Hot-Reload Regression Net

The test infrastructure for the hot-reload / persistence architecture. This is
the **regression net** that must stay green while the audit's simplification
opportunities (`local/documents/data_audit.md`, critical review in
`local/documents/data_audit_review.md`) are implemented. Every fix lands behind
these tests.

## Suites

| Suite | Covers |
| --- | --- |
| `test_hot_reload_suite.py` | Full suite, two sessions. Session A (tests/project + pill_spline): project reload + data survival, schema migration, project forgotten-type, module reload with data survival (`existing=1`), **repeated same-config reload stability** (`module_double_reload`, pins per-artifact TypeId stability / no per-reload growth), module forgotten-type with **drop→re-seed** (`existing=0` after restore, pins drop-at-detection end-to-end), init-failure rollback. Session B (examples/project_rs + pill_spline): module→project **cascade** plus **module↔project coexistence** (`xxsees 1 spline(s)` after both reloads). Also verifies the up-to-date build fast path on restart. |
| `test_hot_reload_migration.py` | Table-driven schema-migration suite: fast path (shape unchanged), add field, remove field, rename field, downgrade persistable→plain, with per-component migrated entity-count assertions. Cycles repeatable (`--cycles N`). |
| `test_module_project_auto_reload.py` | The module→project cascade in isolation: editing `pill_spline` reloads the module, the host queues a project reload, and the project probe reports the new value. |
| `test_csharp_bridge.py` | The C# ↔ Rust connection (needs .NET SDK 8 on PATH). Launches the host with `examples/project_cs` + `pill_spline` + a component-less dummy module and asserts: the host auto-generates the module's C# mirror with the exact layout (`Size = 196`, alignment pad), a component-less module writes **no** mirror, the managed build is warning-free (`0 Warning(s)`, no `warning CS`), the bridge probe proves **both directions** (Rust→C# reads the module-seeded spline; C#→Rust sees its own spline in the same native column), a **behavior-only C# hot reload** (`[csharp_runtime] reloaded project_cs.dll` + `C# hot reload complete`, post-reload probe `sees 3 spline(s)`), and **mirror regeneration** when the file is deleted and the host restarts. |
| `suite_common.py` | Single source of truth for paths, log tokens, timeouts, the color `print` wrapper, the `OutputMonitor` (rolling buffer + counter-tick tail), atomic source editing, and host process helpers. Shared by all three suites (audit opportunity 5.14). |

## Quick start

```powershell
# Full net via one entry point (recommended)
bash devops/tests/run_hot_reload_tests.sh                 # 4 suites
bash devops/tests/run_hot_reload_tests.sh --skip-build    # fastest lane

# Individual suites
python tests/test_hot_reload_suite.py
python tests/test_hot_reload_migration.py --cycles 2
python tests/test_module_project_auto_reload.py
python tests/test_csharp_bridge.py
```

All suites accept `--timeout-scale S` for slow machines. Every file the suites
touch (`modules/pill_config.yaml`, `tests/project/src/lib.rs`,
`examples/project_rs/src/lib.rs`, `examples/project_cs/src/Systems.cs`,
`modules/optional/pill_spline/src/lib.rs`, the generated mirror files) is
backed up at startup and restored afterwards.

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
   had already drifted. → extracted into `suite_common.py`.
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
python tests/test_hot_reload_migration.py
python tests/test_hot_reload_migration.py --cycles 5
```

## Module-project auto-reload test

`test_module_project_auto_reload.py` verifies that editing an optional module
the project links directly (for example `pill_spline`) reloads the project as
well, so the project's embedded copy of the module code picks up the change.

```powershell
python tests/test_module_project_auto_reload.py
```
