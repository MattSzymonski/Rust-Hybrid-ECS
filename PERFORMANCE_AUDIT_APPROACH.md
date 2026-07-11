# Performance Audit Approach — Rust Hybrid ECS

**Date:** July 12, 2026
**Purpose:** Define the repeatable, evidence-driven methodology for all
performance engineering work on this project.
**Principle:** Measure first. Never optimize from intuition.

---

## 1. Performance Philosophy

1. **Correctness trumps speed.** A fast wrong answer is useless.
2. **Benchmarks are the only source of truth.** No optimization is accepted
   without supporting measurement.
3. **Optimise the common case.** Hot paths matter; cold paths don't.
4. **Understand what the machine is doing.** Read the generated assembly for
   any optimisation that claims more than a ~5% improvement.
5. **The compiler is smarter than you.** Before hand-optimizing, check whether
   LLVM already eliminates the issue.
6. **Every optimisation is a trade-off.** Document what is gained and what is
   lost (readability, maintainability, compile time, binary size).
7. **Eliminate waste before tuning instructions.** An allocation eliminated
   beats a branch hint every time.

---

## 2. Phase 1 — Understand the System

### 2.1 Architecture Map

The ECS has four critical execution layers:

```
┌─────────────────────────────────────────────────┐
│ Layer 4: Engine                                 │
│   process_frame() → run_systems_*()             │
│   Frequency: 1 call / frame                     │
│   Cost: dominated by system execution           │
├─────────────────────────────────────────────────┤
│ Layer 3: Scheduler + Parallel Execution         │
│   build_execution_graph(), for_each() batches   │
│   Frequency: 1 graph build / frame              │
│   Cost: O(n²) graph build; O(systems) execution │
├─────────────────────────────────────────────────┤
│ Layer 2: Queries                                │
│   Query::iter_mut(), Query::par_iter_mut()      │
│   Query::entity_count(), first(), is_empty()    │
│   Frequency: once per system per frame          │
│   Cost: O(archetypes) match + O(rows) iterate   │
├─────────────────────────────────────────────────┤
│ Layer 1: Archetype Storage + Component Access   │
│   get_component(), archetype_matches()           │
│   Frequency: per-row during iteration           │
│   Cost: HashMap lookups + Vec indexing          │
└─────────────────────────────────────────────────┘
```

### 2.2 Critical Execution Paths (ranked by impact)

| Rank | Path | CPU-bound | Memory-bound | Why |
|------|------|-----------|-------------|-----|
| 1 | `Query::iter_mut().next()` hot loop | Yes | Yes | Per-row, called millions/frame |
| 2 | `Query::par_iter_mut().for_each()` worker | Yes | Yes | Parallel equivalent of #1 |
| 3 | `Mut<T>::deref_mut()` tick bump | Yes | No | Called on every `&mut` write |
| 4 | `archetype_matches()` | Yes | No | Called per-archetype per-query |
| 5 | `get_or_create_archetype()` | No | Yes | HashMap insert on entity create |
| 6 | `move_entity_to_archetype()` | No | Yes | Clone + realloc on component add/remove |
| 7 | `Scheduler::build_execution_graph()` | Yes | No | O(n²) graph build, once per frame |
| 8 | `update_scripts()` | Yes | Yes | Per-script-entity per-frame |
| 9 | `execute_queued_commands()` | Yes | Yes | Entity lifecycle batch |

### 2.3 Query-Heavy Components

Every component type accessed via `Query<&T>` or `Query<&mut T>` is
hot-path. The most frequently accessed types in a typical game:

- `Transform` / `Position` — read/written by movement systems
- `Velocity` / `Acceleration` — read by physics
- `Health` / `Damage` — game logic
- Marker components (`Enemy`, `Player`, `Projectile`) — archetype filters

### 2.4 Iteration-Heavy Components

- `archetype.entities` — iterated per-query
- `archetype.component_storages.get_storage::<T>()` — indexed per-row
- `archetype.component_ticks` — read per-row for change detection
- `filter_pairs` — iterated per-archetype for query matching

### 2.5 Expected Workloads

| Class | Entities | Archetypes | Systems | Components/entity |
|-------|----------|------------|---------|-------------------|
| Small (unit tests) | 1–100 | 1–5 | 1–5 | 1–3 |
| Medium (simple game) | 1 k–10 k | 10–50 | 10–50 | 3–8 |
| Large (complex game) | 50 k–500 k | 50–500 | 50–200 | 5–15 |
| Pathological | 1 M+ | 1000+ | — | — |

### 2.6 Classification

| Code region | Classification | Bottleneck |
|-------------|---------------|------------|
| Component iteration | Memory-bound | Cache misses, Vec indexing |
| Change-detection ticks | CPU-bound | Branch mispredicts on filter |
| Archetype matching | CPU-bound | Bitwise AND, tiny |
| Entity creation/destruction | Memory-bound | HashMap inserts, Vec reallocs |
| Scheduler graph build | CPU-bound | O(n²) pairwise conflict checks |
| Parallel dispatch | Overhead-bound | Rayon task spawning |

### 2.7 Concurrency Boundaries

- Systems in the same Rayon batch share **nothing** (scheduler guarantee).
- `SendPtrMut` wrappers carry raw pointers across threads.
- `PER_THREAD_LAST_RUN_TICK` is thread-local — no contention.
- `CommandQueue` is single-threaded (Commands systems are exclusive).
- World-level `change_tick` is read-only during system execution.

---

## 3. Phase 2 — Benchmarking Framework

### 3.1 Benchmark Hierarchy

| Level | Tool | Scope | Example |
|-------|------|-------|---------|
| **Micro** | `criterion` | Single function, isolated | `ComponentMask::has_bit()` |
| **Function** | `criterion` | One public API call | `World::get_component()` |
| **Module** | `criterion` | Subsystem end-to-end | `Query::iter_mut()` across 10 k entities |
| **End-to-end** | Custom harness | Full frame loop | `Engine::process_frame()` with N entities |
| **Stress** | Custom harness | Extreme inputs | 1 M entities, single archetype |
| **Scalability** | Custom harness | Varying N or threads | entity_count: 100 → 1 M, threads: 1 → 16 |
| **Regression** | CI + criterion | Historical comparison | Every commit vs baseline |

#### Purpose of each level

- **Micro:** Catch regressions in fundamental operations. Isolate bottlenecks
  identified by profiling.
- **Function:** Validate API-level performance expectations.
- **Module:** Measure subsystem throughput (queries, scheduler, commands).
- **End-to-end:** Frame time under realistic workloads. The ultimate metric.
- **Stress:** Find asymptotic complexity problems and memory leaks.
- **Scalability:** Verify parallel scaling and O(n) claims.
- **Regression:** Prevent performance from degrading over time.

### 3.2 Representative Datasets

| Name | Entities | Archetypes | Components/entity | Purpose |
|------|----------|------------|-------------------|---------|
| `tiny` | 10 | 2 | 2 | Warm-up, smoke test |
| `small` | 1 k | 5 | 3 | Unit-test scale |
| `medium` | 10 k | 20 | 5 | Typical game |
| `large` | 100 k | 100 | 8 | Complex game |
| `huge` | 1 M | 500 | 10 | Stress test |
| `single_archetype` | 100 k | 1 | 1 | Best-case iteration |
| `fragmented` | 100 k | 500 | 2 | Worst-case archetype matching |
| `many_components` | 10 k | 100 | 15 | Worst-case migration |
| `churn` | 10 k created + destroyed | — | 3 | Entity lifecycle stress |

**Dataset construction rules:**
- Use deterministic seeding (fixed RNG seed) for reproducibility.
- Avoid all-equal values (branch predictors cheat).
- Include realistic distributions (spatial, health ranges, etc.).
- Pathological datasets must be documented as such.

### 3.3 Metrics

#### Primary (must collect)

| Metric | Tool | Why |
|--------|------|-----|
| Wall-clock time | `criterion` / `std::time::Instant` | User-visible latency |
| CPU cycles | `perf stat` | Hardware cost |
| Instructions retired | `perf stat` | Work done |
| IPC (instructions per cycle) | `perf stat` | Pipeline efficiency |
| Cache misses (L1, L2, L3) | `perf stat` | Memory bottleneck |
| Branch misses | `perf stat` | Branch predictor pressure |
| Allocations (count) | `DHAT` / custom `GlobalAlloc` | Allocation pressure |
| Allocation bytes | `DHAT` / custom `GlobalAlloc` | Memory churn |
| Peak heap | `Massif` / `heaptrack` | Memory footprint |

#### Secondary (collect when relevant)

| Metric | Tool | When to use |
|--------|------|------------|
| L1/L2/L3 cache hit rate | `perf stat` | Investigating memory-boundness |
| TLB misses | `perf stat` | Large working sets |
| Context switches | `perf stat` | Concurrency overhead |
| Lock contention | `tracy` | Parallel bottlenecks |
| Frame time P95/P99 | Custom harness | Latency-sensitive applications |
| Throughput (entities/ms) | Custom harness | Bulk operation efficiency |
| Stack usage | `cargo call-stack` | Recursive or deep calls |

### 3.4 Profiling Tools

| Tool | Measures | When | Limitations | Output |
|------|----------|------|-------------|--------|
| **Criterion** | Wall-clock statistics | All benchmarks | Statistical noise; no hardware counters | HTML report with violin plots |
| **`perf stat`** | Hardware counters | Any Linux executable | Requires `perf` + kernel support | Tabular counter summary |
| **`perf record` + flamegraph** | Call stacks + frequency | Finding hot functions | Sampling artifacts; inlines flattened | SVG flame graph |
| **`cargo asm`** | Generated assembly | Verifying compiler output | Requires knowing what to look for | AT&T/Intel assembly listing |
| **Cachegrind** | Cache simulation | Cache-line analysis | Valgrind overhead (~20× slower) | Annotated source with miss counts |
| **Callgrind** | Call graph + instruction counts | Call-graph profiling | Valgrind overhead | Call graph with cost annotations |
| **DHAT** | Heap allocations + access patterns | Allocation profiling | Valgrind overhead | Annotated allocation sites |
| **Massif** | Heap snapshots over time | Memory leak / peak analysis | Valgrind overhead | Heap usage timeline |
| **Compiler Explorer** | Assembly for snippets | Isolated micro-analysis | Not full-program | Assembly + colour mapping |
| **`llvm-mca`** | CPU pipeline simulation | Basic blocks | Models only; not runtime | Instruction schedule + bottlenecks |
| **tracy** | Real-time tracing | Multi-threaded profiling | Requires instrumentation | Timeline with zone spans |
| **heaptrack** | Heap profiling (native) | Linux heap analysis | Requires heaptrack | GUI + flame graph of allocations |

#### When to use each

```
Hot function identified?
  → cargo asm to check what LLVM generated
  → perf record + flamegraph to see caller context
  → llvm-mca on the hot loop basic block
  → Cachegrind if cache misses are suspected

Allocation pressure suspected?
  → DHAT for per-site allocation counts
  → heaptrack or Massif for temporal heap profile

Concurrency issue?
  → tracy for thread-level timeline
  → perf stat -e context-switches

Regression detected?
  → criterion baseline comparison
  → perf diff between two binaries
```

### 3.5 Benchmark Quality Rules

| Rule | Rationale |
|------|-----------|
| **Build in release mode** (`opt-level=3`, `lto="thin"`, `codegen-units=1`) | Debug builds are not representative |
| **Warm up before measuring** (at least 1–2 seconds or 100 iterations) | Cold caches, CPU frequency ramp |
| **Minimum 50 samples per benchmark** | Statistical significance |
| **Use `criterion`'s statistical analysis** (bootstrap confidence intervals) | Avoids manual statistics errors |
| **Pin CPU frequency** (`cpupower frequency-set -g performance`) | Eliminates frequency-scaling noise |
| **Isolate from other workloads** (quiet machine) | Avoids interference |
| **Use deterministic inputs** (fixed seeds, fixed sizes) | Reproducibility |
| **`black_box()` inputs and outputs** | Prevents dead-code elimination |
| **Avoid measuring `println!`, `assert!`, `unwrap()`** in hot loops | These are not production paths |
| **Benchmarks must compile without warnings** | Clean baseline |
| **Document the machine** (CPU model, RAM, OS, Rust version) | Cross-machine comparison |
| **Store benchmark binaries + results** | Historical tracking |
| **Run benchmarks on dedicated CI hardware** (not shared runners) | Eliminates noisy-neighbour variance |

### 3.6 Performance Baselines

1. **Initial baseline:** Run all benchmarks on the current `ecs` branch.
   Commit results to `benchmarks/baseline-v1.json`.

2. **Versioning:** Each significant optimization gets a new baseline
   (`baseline-v2.json`, etc.).

3. **CI integration:**
   - Nightly benchmark run on dedicated hardware.
   - Compare against previous baseline.
   - Flag regressions exceeding **2%** for micro-benchmarks,
     **5%** for end-to-end frame time.
   - Post results as a CI artifact (not a pass/fail gate initially).

4. **Historical tracking:**
   - Store `criterion` HTML reports in `benchmarks/history/`.
   - Track frame-time trend over commits.

---

## 4. Phase 3 — Audit Procedure

### 4.1 Audit Levels (top-down)

For every module, inspect the following layers in order. Stop when a layer
reveals no actionable issues.

```
Level 1: Architecture
  └─ Does the overall design create inherent bottlenecks?
     (e.g., single-threaded phase after parallel work)

Level 2: Algorithms
  └─ Is the asymptotic complexity appropriate for expected N?
     (e.g., O(n²) scheduler graph build for 100+ systems)

Level 3: Data structures
  └─ Are the right containers used?
     (HashMap vs Vec, Vec vs SmallVec, BTree vs sort)

Level 4: Runtime behaviour
  └─ Profiler shows where time is actually spent.

Level 5: Machine code
  └─ cargo asm / Compiler Explorer for hot loops.

Level 6: CPU microarchitecture
  └─ llvm-mca, perf stat for pipeline bottlenecks.
```

### 4.2 Per-Module Inspection Checklist

For each source file, answer:

#### Algorithmic

- [ ] What is the worst-case time complexity of each public function?
- [ ] Are there any O(n²) or worse operations on critical paths?
- [ ] Is any work repeated that could be cached?
- [ ] Are there linear scans that could be hash lookups? (or vice versa if N is small)

#### Memory

- [ ] Are there unnecessary allocations? (`clone()`, `Vec::new()`, `Box::new()`)
- [ ] Can stack allocation replace heap allocation? (`SmallVec`, `ArrayVec`, fixed-size arrays)
- [ ] Are `Arc`/`Rc` used where borrows would suffice?
- [ ] Is data stored in AoS (Array of Structs) where SoA (Struct of Arrays) would be better?
- [ ] Are Vecs pre-allocated with `with_capacity()` where the size is known?

#### Cache

- [ ] Are hot data structures compact? (avoid padding, reorder fields)
- [ ] Is sequential access used where possible? (avoid random access patterns)
- [ ] Are linked structures (HashMap, LinkedList) used where contiguous storage (Vec) would work?
- [ ] Can data be prefetched? (`std::intrinsics::prefetch_read_data`)

#### Concurrency

- [ ] Is parallel work evenly distributed? (avoid straggler tasks)
- [ ] Is there false sharing? (`#[repr(align(64))]` or padding)
- [ ] Are atomics used where non-atomic would suffice?
- [ ] Is there lock contention? (Mutex, RwLock usage)

#### Compiler optimisation

- [ ] Are key functions `#[inline]`?
- [ ] Does `cargo asm` show unexpected codegen? (bounds checks, unexpected branches)
- [ ] Are there opportunities for `#[cold]` on error paths?
- [ ] Could `likely!`/`unlikely!` hints help branch prediction?
- [ ] Are there opportunities for SIMD? (auto-vectorization or explicit)
- [ ] Is dynamic dispatch (`dyn Trait`) on a hot path? Can it be monomorphized?

#### Iterator chains

- [ ] Are iterator chains fused? (`.filter().map().collect()`)
- [ ] Is `.collect()` creating an intermediate Vec that could be consumed directly?
- [ ] Could `.par_iter()` be used instead of `.iter()`?
- [ ] Are there nested loops that could be flattened?

### 4.3 Module-Specific Hotspots

Based on architecture analysis, these are the primary targets for the first audit:

| Module | Primary Metrics | Expected Bottleneck |
|--------|----------------|---------------------|
| `query/iter.rs` | Throughput (rows/s), cache misses | Memory: random component access |
| `query/target.rs` | Per-row overhead, pointer chasing | Memory: Tick vec + storage vec indexing |
| `query/filter.rs` | Branch misses on filter evaluation | CPU: Changed<T> per-row check |
| `world.rs` | Entity creation/destruction latency | Memory: HashMap + Vec realloc |
| `scheduler.rs` | Graph build time vs system count | CPU: O(n²) pairwise checks |
| `archetype.rs` | Archetype matching (mask AND) | CPU: trivial bitwise ops |
| `engine.rs` | Frame time distribution | Overhead: Rayon task spawning |

---

## 5. Phase 4 — Finding Template

Every performance finding must use this template:

```markdown
### [ID] [SEVERITY] Finding Title

**File:** `src/path/to/file.rs`
**Function:** `function_name()`
**Status:** ⬜ Measured  /  ⚠️ Suspected

#### Evidence

<!-- Profiler output, benchmark numbers, or hypothesis with reasoning -->

#### Impact

<!-- Estimated frame-time impact: negligible / minor / moderate / significant -->

#### Proposed Optimisation

<!-- Code before/after, or algorithm change description -->

#### Trade-offs

<!-- What is lost: readability, generality, safety, compile time? -->

#### Validation

<!-- Which benchmark(s) confirm the improvement? -->
<!-- What is the expected improvement threshold? -->
```

**Severity ranking:**
- **Critical:** >10% frame-time impact. Fix immediately.
- **High:** 3–10% frame-time impact. Fix this sprint.
- **Medium:** 1–3% frame-time impact. Fix when convenient.
- **Low:** <1% or cold-path. Document; fix opportunistically.

---

## 6. Optimisation Workflow

```
  ┌──────────┐     ┌──────────┐     ┌───────────┐     ┌──────────┐
  │ Profile  │────▶│ Identify │────▶│ Hypothesise│────▶│ Implement│
  │ (perf,   │     │ hotspot  │     │ root cause │     │ change   │
  │  crit.)  │     │          │     │            │     │          │
  └──────────┘     └──────────┘     └───────────┘     └──────────┘
                                                           │
       ┌───────────────────────────────────────────────────┘
       ▼
  ┌──────────┐     ┌──────────┐     ┌───────────┐
  │ Document │◀────│  Accept? │◀────│ Benchmark │
  │ finding  │     │          │     │ before/   │
  └──────────┘     └──────────┘     │ after     │
       │               │            └───────────┘
       │               │ reject
       │               ▼
       │          ┌──────────┐
       │          │ Discard  │
       │          │ or retry │
       │          │ approach │
       │          └──────────┘
       ▼
  ┌──────────┐
  │ Commit + │
  │ update   │
  │ baseline │
  └──────────┘
```

1. **Profile** — find where time actually goes.
2. **Identify** — pinpoint the exact function/loop.
3. **Hypothesise** — why is it slow? (cache? branches? allocations?)
4. **Implement** — make the smallest change that tests the hypothesis.
5. **Benchmark** — before vs after, statistical comparison.
6. **Accept/Reject** — does the data support the change? If no, revert.
7. **Document** — record the finding even if rejected (prevents re-investigation).

---

## 7. Immediate Roadmap

### Step 1 — Build the benchmark harness (Week 1)
- Add `criterion` as a dev-dependency to `Cargo.toml`.
- Create `benches/` directory with:
  - `benches/entity_lifecycle.rs` — create + destroy at scale
  - `benches/query_iteration.rs` — sequential + parallel, filtered + unfiltered
  - `benches/archetype_migration.rs` — add/remove component
  - `benches/scheduler_graph.rs` — build_execution_graph with N systems
  - `benches/frame_loop.rs` — end-to-end process_frame
- Establish baseline numbers on reference hardware.
- Commit baseline as `benchmarks/baseline-v1.json`.

### Step 2 — Profile the current baseline (Week 1–2)
- Run `perf record` + flamegraph on `frame_loop` benchmark.
- Run DHAT on `entity_lifecycle` benchmark.
- Document top-5 hotspots with evidence.

### Step 3 — First audit pass (Week 2–3)
- Apply the per-module checklist (Section 4.2) to each file.
- File findings using the template (Section 5).
- Prioritise by measured impact, not intuition.

### Step 4 — Iterate (ongoing)
- Fix highest-impact findings.
- Re-benchmark after each fix.
- Update baseline.
- Repeat.

---

*This document is the authoritative reference for all performance
engineering on the Rust Hybrid ECS project. All contributors proposing
performance changes must reference the methodology defined here.*
