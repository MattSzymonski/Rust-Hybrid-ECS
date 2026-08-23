# Benchmarks

Criterion benchmarks for the Hybrid ECS engine. Each file targets a specific subsystem.

## Running

The simplest way is through Pill Lab, which runs the benchmarks, applies the
profile overrides described below, and stores the result as a JSON measurement
you can browse and compare:

```bash
python devops/pill_lab/pill_lab.py engine                          # everything
python devops/pill_lab/pill_lab.py engine --bench minimal --quick  # fast check
python devops/pill_lab/pill_lab.py serve                           # view results
```

Running cargo directly:

```bash
cargo bench                          # all benchmarks
cargo bench --bench <name>           # single benchmark
cargo bench --bench <name> -- --quick  # quick check, no saved results
cargo bench --bench <name> --no-run  # build only
```

### Profile overrides (required on Windows)

A bare `cargo bench` currently fails in this workspace, for two independent
reasons:

1. `modules/.cargo/config.toml` sets `-C prefer-dynamic` so the host and every
   optional module share one `pill_core`. rustc refuses to combine that with
   the release profile's `lto = "fat"`:
   `linker plugin based LTO is not supported together with -C prefer-dynamic
   when targeting Windows-like targets`.
2. The release profile (which `bench` inherits) sets `panic = "abort"`, while
   Criterion's harness links the unwinding panic runtime:
   `the linked panic runtime panic_unwind is not compiled with this crate's
   panic strategy abort`.

Clear both for the benchmark invocation only - no file needs changing:

```powershell
$env:RUSTFLAGS = " "
cargo bench --package pill_engine --config 'profile.release.panic="unwind"'
```

`pill_lab.py engine` does exactly this automatically (opt out with
`--no-profile-overrides`).

To run the `minimal` benchmark 10 times and write the averaged Criterion tree
to `target/criterion/`:

```bash
python engine/run_minimal_benchmark.py
```

Individual runs use an isolated temporary directory and are deleted after a
successful average. Pass `--keep-runs target/minimal-runs` to retain them.

## Files

### `minimal.rs`
Four focused hot-path cases at fixed sizes: `query_iter_unfiltered`,
`query_iter_changed`, `query_par_iter_unfiltered`, and
`archetype_add_component`.

### `entity_lifecycle.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `entity_create` | 100, 1K, 10K | Fresh-world entity creation with 3 components |
| `entity_create_reserved` | 1K, 10K, 100K | Same but with `world.reserve_entities()` pre-allocation |
| `entity_destroy` | 100, 1K, 10K | Single-component entity destruction |
| `entity_reuse_cycle` | 100, 1K, 10K | Create → destroy → create (free-list reuse path) |
| `entity_create_many_components` | 100, 1K, 10K | 6-component entity creation |

### `archetype_migration.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `archetype_add_component` | 1K, 10K | Add single component to existing entity |
| `archetype_remove_component` | 1K, 10K | Remove single component |
| `archetype_add_multi_component` | 1K, 10K | Add 3 components sequentially (multi-step migration) |
| `archetype_remove_multi_component` | 1K, 10K | Remove 3 components sequentially |
| `archetype_explosion` | 100, 1K | Entities with unique component subsets → many archetypes |

### `query_iteration.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `query_iter_unfiltered` | 1K, 10K, 100K | Sequential read-only (`&Pos, &Vel`) |
| `query_iter_mutable` | 1K, 10K, 100K | Sequential mutable (`&mut Pos, &Vel`) |
| `query_iter_changed` | 1K, 10K, 100K | `Changed<Position>` filter |
| `query_iter_with` | 1K, 10K, 100K | `With<Enemy>` filter (25% match) |
| `query_iter_without` | 1K, 10K, 100K | `Without<Frozen>` filter (~86% match) |
| `query_iter_or` | 1K, 10K, 100K | `Or<(With<Enemy>, With<Frozen>)>` filter |
| `query_iter_added` | 1K, 10K, 100K | `Added<Position>` filter |
| `query_entity_only` | 10K, 100K | `Query<Entity>` - no component data |
| `query_multi_component` | 10K, 100K | `(&Pos, &Vel, &Health)` - 3 components |
| `query_get_component` | 10K | Random access via `world.get_component()` / `get_component_mut()` |
| `query_par_iter_unfiltered` | 10K, 100K, 1M | Parallel read-only |
| `query_par_batch_size` | 10K | Batch sizes 1–1024 |
| `query_par_with` | 10K, 100K | Parallel `With<Enemy>` filter |
| `query_crossover` | 1K–100K | Seq vs par crossover point |
| `query_helpers` | 10K | `entity_count()`, `is_empty()`, `first()` |
| `query_large_component` | 10K–2M | 64 B and 256 B component cache pressure |

### `scheduler_graph.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `scheduler_graph_build` | 10, 50, 100, 200 | O(n²) conflict analysis + graph build |
| `scheduler_batch_execution` | 10, 50, 100, 200 | End-to-end frame dispatch with 20 component types, 100 entities |

### `frame_loop.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `standard` | 100K, 500K | 3 light systems, small components |
| `large_cache` | 100K, 500K | Standard + 256 B `RenderData` |
| `light` | 10K, 100K, 500K | Movement + health_decay + collision (tracy_live) |
| `heavy_compute` | 10K, 50K, 100K | Gravity + cleanup (sqrt/div/cbrt) |
| `large_components` | 10K, 50K | Render + physics (256 B + 128 B reads) |
| `full` | 10K, 30K | All 7 tracy_live systems |

Every profile has a `_sequential` variant with parallel execution disabled.

### `resource_commands.rs`
| Group | Counts | Description |
|-------|--------|-------------|
| `resource_insert` | 100, 1K, 10K | `world.insert_resource()` throughput |
| `resource_get` | 1K, 10K, 100K | `world.get_resource()` throughput |
| `resource_get_mut` | 1K, 10K, 100K | `world.get_resource_mut()` throughput |
| `resource_remove` | 100, 1K, 10K | `world.remove_resource()` throughput |
| `commands_create_entity` | 100, 1K, 10K | Deferred entity creation via `Commands` |
| `commands_destroy_entity` | 100, 1K, 10K | Deferred entity destruction via `Commands` |
| `commands_add_component` | 100, 1K, 10K | Deferred `add_component_to_entity` via `Commands` |

## Profiles

All benchmarks use the `bench` profile (`opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`).

For higher-throughput testing: `cargo bench --profile release-fast` (`opt-level = 3`).

## HTML reports

Criterion outputs to `target/criterion/`. Open `target/criterion/report/index.html`.

`reports/gen_bench_report.py` still generates its richer self-contained HTML
report from the same directory. Its Criterion parsing and machine detection now
come from `devops/core/`, so there is one parser shared with
Pill Lab rather than two that can drift:

```bash
python pill_engine/benches/reports/gen_bench_report.py --criterion-dir target/criterion
```

For browsing history and comparing runs, prefer Pill Lab
(`python devops/pill_lab/pill_lab.py serve`); its Engine Performance view is
the port of that report.
