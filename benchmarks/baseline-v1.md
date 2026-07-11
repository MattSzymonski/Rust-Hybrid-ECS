# Performance Baselines — ecs_hybrid v0.9.11

**Date:** July 12, 2026
**Machine:** Windows, `opt-level=3`, `lto="thin"`, `codegen-units=1`
**Rust:** (see `rustc --version`)
**Commit:** ecs branch

---

## 1. Entity Lifecycle

| Benchmark | Entities | Time | Throughput |
|-----------|----------|------|------------|
| `entity_create` | 100 | 26.46 µs | 3.78M entities/s |
| `entity_create` | 1,000 | 249.08 µs | 4.01M entities/s |
| `entity_create` | 10,000 | 2.55 ms | 3.92M entities/s |
| `entity_destroy` | 100 | 6.22 µs | 16.1M entities/s |
| `entity_destroy` | 1,000 | 54.31 µs | 18.4M entities/s |
| `entity_destroy` | 10,000 | 653.62 µs | 15.3M entities/s |

## 2. Query Iteration

| Benchmark | Entities | Time | Per-entity |
|-----------|----------|------|------------|
| `iter_unfiltered` | 1,000 | 852.65 ns | 0.85 ns |
| `iter_unfiltered` | 10,000 | 8.00 µs | 0.80 ns |
| `iter_unfiltered` | 100,000 | 81.70 µs | 0.82 ns |
| `iter_mutable` | 1,000 | 1.57 µs | 1.57 ns |
| `iter_mutable` | 10,000 | 14.51 µs | 1.45 ns |
| `iter_mutable` | 100,000 | 160.89 µs | 1.61 ns |
| `iter_changed` | 1,000 | 1.08 µs | 1.08 ns |
| `iter_changed` | 10,000 | 9.41 µs | 0.94 ns |
| `iter_changed` | 100,000 | 94.51 µs | 0.95 ns |
| `par_iter_unfiltered` | 10,000 | 40.61 µs | 4.06 ns |
| `par_iter_unfiltered` | 100,000 | 67.93 µs | 0.68 ns |
| `par_iter_unfiltered` | 1,000,000 | 178.09 µs | 0.18 ns |
| `entity_count` (helper) | any | 50.66 ns | — |
| `is_empty` (helper) | any | 50.40 ns | — |
| `first` (helper) | any | 54.91 ns | — |

**Key observations:**
- Sequential iteration scales linearly: ~0.8 ns/entity (unfiltered), ~1.5 ns/entity (mutable with tick bump), ~0.95 ns/entity (changed-only)
- Parallel iteration shows sub-linear scaling at 10k (overhead dominates), excellent scaling at 1M (0.18 ns/entity)
- Mutable iteration is ~2× slower than unfiltered (tick bump cost)
- Changed-only filtering is faster than mutable (avoids tick writes)
- O(1) query helpers: ~50ns regardless of world size

## 3. Archetype Migration

| Benchmark | Entities | Time | Per-entity |
|-----------|----------|------|------------|
| `add_component` | 1,000 | 429.62 µs | 430 ns |
| `add_component` | 10,000 | 4.53 ms | 453 ns |
| `remove_component` | 1,000 | 376.27 µs | 376 ns |
| `remove_component` | 10,000 | 3.89 ms | 389 ns |

**Key observations:**
- Component removal is ~15% faster than addition (less allocation)
- Both scale linearly with entity count
- ~400 ns per migration — dominated by clone + HashMap insert + Vec realloc

## 4. Scheduler Graph Build

| Benchmark | Systems | Time | Per-system-pair |
|-----------|---------|------|-----------------|
| `build_graph` | 10 | 522.82 ns | ~5.8 ns/pair |
| `build_graph` | 50 | 2.24 µs | ~1.8 ns/pair |
| `build_graph` | 100 | 5.27 µs | ~1.1 ns/pair |
| `build_graph` | 200 | 13.89 µs | ~0.7 ns/pair |

**Key observations:**
- O(n²) growth confirmed: 200 systems → 19,900 pairwise checks
- Per-pair cost decreases with scale (better cache utilization of ComponentMask)
- Even at 200 systems, graph build is negligible (~14 µs)

## 5. End-to-End Frame Loop

| Benchmark | Entities | Time | Systems |
|-----------|----------|------|---------|
| `frame_loop` | 1,000 | 20.39 µs | 3 (movement + health + reporting) |
| `frame_loop` | 10,000 | 35.78 µs | 3 |
| `frame_loop` | 100,000 | 246.50 µs | 3 |

**Key observations:**
- Sub-linear scaling from 1k→10k (query overhead dominates small N)
- Linear scaling from 10k→100k (~2.5 µs/1k entities)
- 100k entities with 3 systems in ~250 µs → ~400k entities/ms throughput

---

## Hot Path Ranking (from baseline data)

| Rank | Path | Evidence | Impact |
|------|------|----------|--------|
| 1 | `Query::iter_mut()` tick bump | mutable 2× slower than unfiltered | High |
| 2 | Entity creation | 2.55 ms for 10k (most expensive single op) | Medium |
| 3 | Parallel dispatch overhead | 10k par slower than seq (40 vs 8 µs) | Medium |
| 4 | Archetype migration | 4.5 ms for 10k adds | Low |
| 5 | Scheduler graph build | 14 µs for 200 systems (already negligible) | None |

---

## Next Steps

- [ ] Profile `iter_mut` tick bump with `perf record` / flamegraph
- [ ] Investigate why 10k parallel iteration has 5× overhead vs sequential
- [ ] Run scalability benchmarks: varying thread counts (1→16)
- [ ] Add filtered query benchmarks (With/Without)
- [ ] Set up CI regression tracking against this baseline
