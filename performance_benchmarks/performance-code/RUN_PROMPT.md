You are an AI coding agent performing iterative runtime performance optimization on the
`ecs_hybrid` Rust ECS crate.

## Your task

Identify, measure, and improve runtime performance in the hot paths of this ECS.
Work iteratively: each optimization must be measured before and after using the
performance measurement pipeline. Do not land an optimization without evidence.

## Workflow (repeat for each optimization)

### Phase 1 — Establish baseline

```bash
python performance_benchmarks/performance/performance_measurement_pipeline.py doctor

python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before \
  --bench query_iteration \
  --bench-filter query_iter_unfiltered/100000 \
  -r 10 -w 5
```

Also collect supplementary baselines for other key benchmarks:

```bash
# Mutable iteration
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before_mut \
  --bench query_iteration \
  --bench-filter query_iter_mutable/100000 \
  -r 10 -w 5

# Entity lifecycle (create + destroy)
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before_lifecycle \
  --bench entity_lifecycle \
  --bench-filter entity_lifecycle/10000 \
  -r 10 -w 5

# Archetype migration (add/remove component)
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before_migration \
  --bench archetype_migration \
  --bench-filter archetype_migration/10000 \
  -r 10 -w 5

# Full frame loop (end-to-end)
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before_frame \
  --bench frame_loop \
  --bench-filter frame_loop/10000 \
  -r 10 -w 5

# Scheduler graph building
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline \
  -n before_scheduler \
  --bench scheduler_graph \
  -r 10 -w 5

# Also run Criterion natively for HTML reports and cross-reference
cargo bench --bench query_iteration -- --save-baseline before
cargo bench --bench frame_loop -- --save-baseline before
```

### Phase 2 — Profile and identify offenders

Inspect the baseline:

```bash
python performance_benchmarks/performance/performance_measurement_pipeline.py analyze --run before
```

Read the generated report:

```bash
type performance_benchmarks\performance\artifacts\baselines\before\report.md
```

Combine with profiling tools:

```bash
# Linux: hardware counters on the Criterion benchmark binary
cargo bench --bench query_iteration --no-run

# Find the benchmark binary
BENCH_BIN=$(ls target/release/deps/query_iteration-* | head -1)

# perf stat — aggregate counters
perf stat \
  -e cycles,instructions,cache-references,cache-misses,\
  L1-dcache-loads,L1-dcache-load-misses,\
  LLC-loads,LLC-load-misses,\
  branches,branch-misses,\
  stalled-cycles-frontend,stalled-cycles-backend \
  $BENCH_BIN query_iter_unfiltered/100000

# perf record — sampling profiler with call stacks
perf record -e cycles:pp -g --call-graph dwarf \
  $BENCH_BIN query_iter_unfiltered/100000
perf report -g graph

# Flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg

# Assembly inspection for the hot iterator loop
cargo asm --lib "ecs_hybrid::query::iter::QueryIterMut::next"
cargo asm --lib "ecs_hybrid::archetype::Archetype::new"
```

**What to look for in the data:**

- **`ipc` (instructions per cycle)** — IPC > 2.0 is excellent, < 1.0 indicates stalls
- **`branch_mispredict_pct`** — > 2% in hot loops is worth investigating
- **`frontend_stall_pct`** — high values suggest I-cache misses or poor inlining
- **`backend_stall_pct`** — high values suggest data-cache misses or memory latency
- **`cache_misses`** — high absolute count per iteration suggests poor data layout
- **`cycles` vs `instructions`** — divergence = stalls (check frontend/backend breakdown)
- **`time_ns_per_item`** — absolute cost per entity; compare across benchmarks
- **Variance (CV)** — > 10% suggests system noise; increase repetitions or pin affinity

**Classify your findings honestly:**

| Classification | Meaning |
|---|---|
| **Measured** | Directly observed in hardware counter data |
| **Derived** | Computed from measurements (e.g. IPC = instructions / cycles) |
| **Static analysis** | Observed in source or assembly without runtime data |
| **Hypothesis** | Plausible explanation requiring an experiment to confirm |

Do not declare that a function is slow merely because it is called frequently.
Use `perf report` or flamegraphs to see where CPU time is actually spent.

### Phase 3 — Formulate a hypothesis

Based on the measurements, state exactly:

- **What you believe is happening** (e.g. "the `clone()` in the migration hot path
  causes a heap allocation per component, adding ~50ns per entity")
- **Which metric would change and by how much** (e.g. "cycles per item should
  decrease from 120 to ~80, IPC should increase from 0.8 to 1.2")
- **What code change would test this** (specific file, function, and change)
- **What trade-off the change introduces** (e.g. "increases code complexity" or
  "requires unsafe pointer manipulation")
- **Which PERFORMANCE_OPTIMIZATION_101.md principle this relates to** (reference the section)

Follow the optimization pyramid from PERFORMANCE_OPTIMIZATION_101.md:
1. **Algorithmic changes** — most impactful: clone elimination, precomputation, caching
2. **Data layout** — SoA vs AoS, hot/cold splitting, cache-line alignment
3. **Implementation details** — branch elimination, iterator chains, allocation reuse
4. **Micro-optimizations** — last resort: bounds checks, inline hints, unsafe

### Phase 4 — Implement and measure

Make the minimal code change. Then:

```bash
python performance_benchmarks/performance/performance_measurement_pipeline.py measure \
  -n after_<descriptive_suffix> \
  -c before \
  --bench query_iteration \
  --bench-filter query_iter_unfiltered/100000 \
  -r 10 -w 5

# Compare with Criterion
cargo bench --bench query_iteration -- --baseline before
cargo bench --bench frame_loop -- --baseline before
```

Read the comparison verdict:

```bash
python performance_benchmarks/performance/performance_measurement_pipeline.py compare \
  --baseline before --candidate after_<suffix>
```

### Phase 5 — Interpret and decide

- **If verdict is IMPROVED** and the improvement matches your hypothesis:
  keep the change, set a new baseline, move to the next optimization area.

- **If verdict is UNCHANGED** but you expected improvement: your hypothesis
  was wrong. Re-examine with `perf record`/`perf report` and `cargo asm`.
  The compiler may have already optimized what you attempted. Revert and move on.

- **If verdict is INCONCLUSIVE** (small change < 2%, high variance): increase
  repetitions to 30+, pin CPU affinity, or measure on a quieter machine.

- **If verdict is REGRESSED**: revert immediately. The change made performance
  worse. Understand why before attempting a different approach.

- **If verdict is NOT_COMPARABLE**: you changed the build configuration,
  compiler version, or measurement setup between runs.

### Phase 6 — Document

For each optimization that lands, add a concise comment near the relevant code:

```rust
// Performance optimization: eliminated clone in archetype migration hot path.
// Before: 380 ns/item, IPC 0.9, 0.08 cache misses/item (rustc 1.95, i7-12700H)
// After:  305 ns/item, IPC 1.2, 0.04 cache misses/item
// Trade-off: none — move semantics replace clone, no change in behavior.
```

And record the pass in `performance_benchmarks/performance/OPTIMIZATION_LOG.md`.

## Areas to investigate (in priority order)

Based on PERFORMANCE_OPTIMIZATION_101.md principles, measure before changing anything.

### Algorithmic changes (highest impact)

1. **Clone elimination** — find every `.clone()` in hot paths. Can the value be
   moved instead? Can a reference be used? §7 "Clone Elimination"

2. **Precomputation** — what is computed every frame that could be computed once?
   Component masks, conflict matrices, archetype match lists. §7 "Precomputation"

3. **Generation-counter caching** — use `archetype_generation` or similar dirty
   flags to cache expensive scans. §14 "The Generation Counter Hack"

4. **Allocation reuse** — find `Vec::new()` or `HashMap::new()` called every frame.
   Pre-allocate and reuse via `.clear()`. §7 "Allocation Reuse"

### Data layout changes

5. **Struct field ordering** — check `#[repr(Rust)]` structs in hot paths.
   Are the most-accessed fields grouped together? §9 "Cache Lines"

6. **Hot/cold splitting** — are rarely-accessed fields (names, debug info)
   on the same cache line as hot fields? §9, Appendix A "Hot/Cold Splitting"

7. **False sharing** — check parallel code for thread-local data on shared
   cache lines. §9 "False Sharing", §11 "False Sharing (Revisited)"

### Implementation details

8. **Branch elimination** — find `if`/`else` in hot loops with unpredictable
   outcomes. Can branches be reordered, eliminated, or branchless? §7 "Branch Elimination"

9. **Cold-path extraction** — mark error paths and rare cases with `#[cold]`
   or `#[inline(never)]`. §7 "Cold-Path Extraction"

10. **Iterator chain optimization** — check for intermediate `.collect()` calls
    in iterator chains. Use lazy evaluation. §7 "Iterator Chain Optimization"

### ECS-specific patterns

11. **`TraitTypeMap::get_storage::<T>()` devirtualization** — check with
    `cargo asm` that this is fully inlined. §15 "Component Bitmask Operations"

12. **`QueryTarget::init_state` pointer caching** — are all cached pointers
    actually used in the hot loop? Unused cached pointers waste register
    pressure and cache space. §15 "Query Archetype Cache"

13. **Tick vector locality** — `component_ticks` is a `HashMap<ComponentId,
    Vec<ComponentTicks>>`. Are ticks in the same cache lines as component data?
    §15 "Change Detection Ticks"

14. **`DEFAULT_SLICE_ENTITIES` tuning** — 4096 entities per slice fits L1 for
    single-component queries. For multi-component queries, measure optimal
    batch sizes. §15 "Parallel Query Execution"

### Micro-optimizations (last resort)

15. **Bounds check elimination** — check `cargo asm` output for `cmp`+`jae`
    patterns. LLVM usually eliminates these; only add `unsafe` if proven
    necessary. §6 "Why Bounds Checks Survive"

16. **`#[inline]` hints** — add `#[inline]` to tiny functions called in hot
    loops that cross module boundaries. Remove `#[inline(always)]` from
    functions where LLVM makes better decisions. §8 "`#[inline]` Annotations"

17. **`panic = "abort"`** — already enabled in this project.

## Rules

- **Never optimize without measuring first.** Static analysis is not evidence.
- **Change one thing at a time.** Multiple changes obscure causality.
- **Revert immediately on regression.**
- **Prefer simpler code.** Equal performance → simpler wins.
- **Document every landed optimization** with before/after numbers.
- **Stop when improvements are within noise** (< 2% and CV > 5%).
- **Do not modify the measurement pipeline itself.**
- **Algorithmic changes before micro-optimizations.** Follow the pyramid.
- **Trust benchmarks over intuition.** Three out of four "obvious" optimizations
  made zero difference in this project's history.
- **`unsafe` must earn its place.** Only add unsafe when benchmarks prove it helps
  AND the safety invariant is clearly documented.

## Quick-reference: optimization checklist

```
□ Establish baseline         performance_measurement_pipeline.py baseline -n before
□ Read the report            type ...\baselines\before\report.md
□ Run perf stat              perf stat -e cycles,instructions,... $BENCH_BIN
□ Run perf record            perf record -e cycles:pp -g $BENCH_BIN
□ Generate flamegraph        perf script | flamegraph > flamegraph.svg
□ Inspect hot-loop assembly  cargo asm --lib "crate::hot_function"
□ Identify top bottleneck    (highest cycles, lowest IPC, or most time)
□ Form hypothesis            "If I do X, metric Y should change by Z"
□ Implement ONE change       (edit one source file)
□ Measure                    performance_measurement_pipeline.py measure -n after_X -c before
□ Compare                    performance_measurement_pipeline.py compare --baseline before --candidate after_X
□ Cross-check with Criterion cargo bench --bench query_iteration -- --baseline before
□ Interpret verdict          (IMPROVED → keep | UNCHANGED → revert | REGRESSED → revert)
□ Document                   (add comment + update OPTIMIZATION_LOG.md)
```

## Deliverables expected

For each successfully landed optimization:

1. The code change (in the appropriate `src/` file)
2. The `before` baseline name and `after_<suffix>` candidate name
3. The comparison output showing the improvement
4. A source comment with before/after metrics
5. An entry in `performance_benchmarks/performance/OPTIMIZATION_LOG.md`

For each rejected hypothesis:

1. A brief note explaining what was attempted, expected, measured, and why reverted
2. An entry in the optimization log so future work does not repeat the attempt

Begin by running `doctor` and creating the `before` baseline.
