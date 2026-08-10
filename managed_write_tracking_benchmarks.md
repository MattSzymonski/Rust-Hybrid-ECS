# Managed Write-Tracking Performance

## Introduction

Exact per-entity change detection requires writable ECS queries to update a
second memory stream alongside component data. For every requested writable
component, both Rust's `Mut<T>` and C#'s `QueryRow.Write<T>()` update the
corresponding `ComponentTicks.changed` value.

This preserves correct `Changed<T>` behavior, but it may increase cache
misses, cache-line write traffic, memory bandwidth, and TLB pressure. Component
values and their ticks live in separate arrays, so a writable loop must keep
both streams in cache. The accesses are sequential and should benefit from
hardware prefetching, but the actual cost must be measured rather than assumed.

The C# path may have additional costs beyond the tick store, including native
chunk callbacks, archetype-column joins, access validation, and larger managed
row metadata. These costs must be measured separately from the fundamental
change-tracking cost shared with Rust.

## Action points

1. Add equivalent Rust and C# benchmarks for read-only and writable queries.
2. Measure one-, three-, and eight-component queries across increasing entity
   counts and archetype counts.
3. Compare these cases:
   - read-only iteration;
   - writable iteration with change tracking;
   - writable iteration with change tracking bypassed;
   - iteration that requests writable references for only some rows.
4. Record throughput, frame time, CPU time, and entities processed per second.
5. Use hardware counters where available to record:
   - L1, L2, and last-level cache misses;
   - cache-line read-for-ownership traffic;
   - memory bandwidth;
   - TLB misses;
   - branch misses.
6. Verify that read-only C# terms do not retrieve or touch tick columns.
7. Measure the cost of copying the enlarged `QueryColumn` metadata into each
   stack-only `QueryRow`.
8. Separate per-chunk FFI and archetype-join overhead from per-row tick-update
   overhead.
9. Test release builds with stable CPU affinity, fixed thread counts, warm-up
   iterations, and multiple samples. Report medians and tail percentiles.
10. Confirm every optimization preserves the existing `Changed<T>` correctness
    tests before comparing performance.

## Optimization experiments

- Store `added` and `changed` ticks in separate `u32` arrays so writable loops
  touch only the changed-tick stream.
- Skip tick-column lookup and export for `Read<T>` and `OptionalRead<T>`.
- Cache native query plans to avoid repeatedly scanning and joining archetypes.
- Compare per-row ticks with chunk-level dirty ticks, documenting the loss of
  exact per-entity `Changed<T>` results.
- Compare immediate tick writes with a changed-row bitset followed by a
  deferred tick update pass.
- Investigate a deliberate change-detection bypass API for internal systems
  that do not require `Changed<T>` visibility.

## Completion criteria

This investigation is complete when benchmarks quantify the independent cost
of tick updates, cache misses, and C# query bridging; identify the dominant
bottleneck at realistic entity counts; and provide evidence for keeping or
changing the current per-row tick design.
