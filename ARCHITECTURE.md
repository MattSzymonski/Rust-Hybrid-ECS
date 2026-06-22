# Rust Hybrid ECS — Architecture Guide

This document explains every subsystem in the ECS: what it does, why it
exists, how it works internally, and how it connects to everything else.

---

## Table of Contents

1. [Overview](#overview)
2. [Design Philosophy & Comparison](#design-philosophy--comparison)
3. [Entity & Component Basics](#entity--component-basics)
4. [Archetype Storage](#archetype-storage)
5. [World — The Central Hub](#world--the-central-hub)
6. [Queries](#queries)
7. [Change Detection](#change-detection)
8. [System Infrastructure](#system-infrastructure)
9. [Scheduler & Parallel Execution](#scheduler--parallel-execution)
10. [Deferred Commands](#deferred-commands)
11. [Resources](#resources)
12. [Script Components](#script-components)
13. [Frame Lifecycle](#frame-lifecycle)
14. [Thread Safety Model](#thread-safety-model)
15. [Module Map](#module-map)

---

## Overview

The ECS is built around four pillars:

1. **Archetype storage** — entities with the same set of components live in the
   same `Archetype`, which stores each component type in its own contiguous
   `Vec`. This Structure-of-Arrays (SoA) layout makes iteration cache-friendly.

2. **System parameters** — systems declare their data dependencies as function
   parameters (`Query`, `Res`, `Commands`, etc.). The engine resolves them
   automatically and builds an access graph for parallel scheduling.

3. **Deferred commands** — structural mutations (spawn, destroy, add/remove
   components) are queued during system execution and applied at the frame
   boundary. This prevents use-after-free when iterating archetypes while
   modifying the World.

4. **Change detection** — every component instance carries tick metadata.
   `Mut<T>` (the wrapper returned by `&mut T` queries) bumps the `changed` tick
   on `DerefMut`. Filters like `Changed<T>` compare this tick against each
   system's last-run timestamp to skip unchanged data — no manual dirty flags,
   no atomics.

---

## Design Philosophy & Comparison

ECS frameworks sit on a spectrum between two extremes: the intuitive,
immediate-mode OOP style of Unity, and the high-performance, data-oriented
style of Bevy. This project takes a deliberate position on that spectrum.

### The three approaches

```
Unity OOP                    This ECS                    Bevy/Pure ECS
(intuitive, slow)            (hybrid)                    (fast, steep curve)
    │                           │                            │
    ▼                           ▼                            ▼
```

### Unity (GameObject pattern)

Games are built from `GameObject` instances. Each object holds a list of
`MonoBehaviour` components. You call `GetComponent<T>()`, modify fields
directly, and changes are visible immediately.

**Strengths:**
- Extremely intuitive — matches how people think about "things in a scene"
- Immediate feedback: spawn an object, it exists right now
- Shallow learning curve for beginners

**Weaknesses:**
- Single-threaded by design — Unity's API is not thread-safe
- Reference-heavy memory layout — components scattered across the heap
- `GetComponent<T>()` is a dictionary lookup on every access
- Iterating "all entities with Transform + Velocity" requires visiting every
  GameObject and checking component presence

### Bevy / Pure archetype ECS

Entities are just IDs. Components are stored in archetypes — groups of
entities sharing the same set of component types. Systems are functions that
query for component combinations and run in automatically-determined parallel
batches.

**Strengths:**
- Cache-friendly: all `Transform` components in an archetype are contiguous
- Automatic parallelism: the scheduler analyzes access patterns
- Change detection built into queries

**Weaknesses:**
- Deferred commands: `commands.spawn(...)` doesn't create the entity until the
  **next** frame boundary — you can't immediately access what you just created
- Learning curve: you must think in terms of queries and archetypes, not objects
- No "main script" attached to an entity — logic is split across systems

### Where this ECS lands

This project is **not** a Unity API wrapper. It is a pure archetype ECS with
Bevy-style system parameters, automatic parallel scheduling, and change
detection. The "hybrid" in the name refers to blending ECS performance with
ergonomic design choices:

| Concern               | Unity                                    | Bevy ECS                      | This ECS                                                                                                         |
| --------------------- | ---------------------------------------- | ----------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| **Storage**           | Per-object heap allocations              | Archetypes (SoA)              | Archetypes (SoA)                                                                                                 |
| **Create entity**     | `Instantiate()` — immediate              | `commands.spawn()` — deferred | `world.create_entity().build()` — immediate via direct World access; also `Commands` for deferred use in systems |
| **Access components** | `GetComponent<T>()` — HashMap per object | `Query<&T>` — bulk iteration  | `Query<&T>` — bulk iteration, plus `world.get_component::<T>(entity)` for point lookups                          |
| **Parallelism**       | Manual only                              | Automatic via scheduler       | Automatic via scheduler (Rayon)                                                                                  |
| **Change detection**  | Manual dirty flags                       | `Changed<T>` filter           | `Changed<T>` / `Added<T>` filters (components + resources)                                                       |
| **Scripts**           | `MonoBehaviour.Update()`                 | N/A (systems, not scripts)    | `ScriptComponent::update()` — per-entity logic with safe deferred-command access                                 |
| **State consistency** | Always consistent                        | Frame-delayed (commands)      | Dual path: immediate via `&mut World`, deferred via `Commands`                                                   |

### Our design decisions

**Immediate + deferred, not just deferred.** Bevy's `commands.spawn()` is the
only way to create entities from systems, and they don't exist until the next
frame. This ECS provides both paths:

- **Direct World access** (`world.create_entity().build()`) — entity exists
  immediately, usable in the same frame. Used during setup or in sequential code.
- **Commands** (`commands.create_entity().with(...).build()`) — deferred,
  thread-safe. Used inside parallel systems.

The system parameter `Commands` exists specifically so parallel systems can
queue structural changes without holding `&mut World` (which would serialize
everything). But when you don't need parallelism — setup, loading, sequential
debug runs — direct World access is faster and simpler.

**Change detection without atomics.** Bevy uses atomic counters for change
detection. This ECS uses a simpler scheme: the scheduler guarantees disjoint
per-row access, so `Mut<T>::deref_mut()` can write `ticks.changed = this_run`
as a plain store — no `AtomicU32`, no CAS loops. Each thread owns its rows
exclusively.

**Script components fill the MonoBehaviour gap.** Pure ECS splits logic into
systems that query many entities. Sometimes you want per-entity update logic
attached directly to a component — a spinning animation, a countdown timer, a
self-destruct condition. `ScriptComponent::update()` provides this, with a
`sScriptContext` that only exposes deferred commands (no direct World mutation)
so scripts cannot cause archetype-migration UB during iteration.

**Resources are just global components.** Unity stores configuration on
`GameObject` instances or `ScriptableObject` assets. Bevy stores it in
`Resource`s. This ECS follows Bevy's model: resources are type-keyed singletons
in the World, accessible via `Res<T>` / `ResMut<T>`, tracked by the scheduler
for conflict detection, and now (post-audit) change-tracked via `Mut<T>`.

**The "object-oriented" part is the *system parameter* API, not a wrapper.**
When you write:

```rust
fn movement(mut q: Query<(&mut Transform, &Velocity)>, time: Res<GameTime>) {
    for (mut t, v) in q.iter_mut() {
        t.x += v.x * time.get().map(|t| t.delta).unwrap_or(0.016);
    }
}
```

...the function signature reads like a declaration of intent: "I need mutable
Transform and immutable Velocity for all entities, plus the global GameTime."
The engine wires up the dependencies automatically. There is no `Scene` object,
no `GameObject` wrapper — the ergonomic benefit comes from the parameter
resolution, not from hiding the ECS behind OOP abstractions.

---

## Entity & Component Basics

### Entity

```rust
pub struct Entity { pub(crate) id: u64, pub(crate) generation: u32 }
```

An `Entity` is a lightweight 12-byte handle. The `id` is a slot index; the
`generation` prevents dangling-handle bugs when IDs are recycled. When entity 5
is destroyed, its ID goes on a free list with `generation = 1`. The next
allocation may reuse ID 5 with `generation = 1`, invalidating any handle that
still holds `generation = 0`.

`Entity::default()` is deliberately **not** valid — it produces `{id: u64::MAX,
gen: u32::MAX}`, a tombstone that will never match a real entity.

### Component

```rust
pub trait Component: Send + 'static {}
impl Component for Position {}
impl Component for Velocity {}
```

Components are plain data. The `Send` bound is required because parallel
queries move component references across threads. There is intentionally no
`Sync` bound — see the [Resources](#resources) section for a detailed
explanation of why `Sync` is needed for resources but not components.

### ComponentId & ComponentMask

- `ComponentId(TypeId)` — unique per component type, used as a HashMap key.
- `ComponentMask(u128)` — each registered component gets a bit (0–127). Query
  matching between archetypes and queries is a single bitwise AND.

`ComponentRegistry` assigns bits at registration time and enforces the 128-type
cap (documented tradeoff: O(1) matching, stack-allocated masks, no heap
allocations).

---

## Archetype Storage

An **archetype** is a group of entities that share the exact same set of
component types. If you have entities with `(Position,)`, `(Position,
Velocity)`, and `(Position, Velocity, Health)`, that's three archetypes.

### Why archetypes?

The Structure-of-Arrays layout means every `Position` in an archetype lives
in a single `Vec<Position>`. When a movement system iterates `(&mut Position,
&Velocity)`, it streams through contiguous memory — excellent for CPU caches
and branch prediction.

### Archetype struct

```
Archetype {
    id: ArchetypeId,
    component_types: Vec<ComponentId>,     // sorted, defines the archetype
    component_mask: ComponentMask,         // fast O(1) matching
    component_storages: TraitTypeMap,      // one Vec per component type (SoA)
    entities: Vec<Entity>,                 // which entities are here
    component_ticks: HashMap<CompId, Vec<ComponentTicks>>, // change-detection
}
```

### Entity migration

When a component is added or removed, the entity **moves** to a different
archetype. The `move_entity_to_archetype` function:

1. Reads the old archetype's component storages (shared reference).
2. Writes to the new archetype's component storages (mutable reference).
3. Copies component data via registered `ComponentCopier` closures.
4. Swap-removes the entity from the old archetype.
5. Cleans up empty archetypes to prevent memory leaks.

Two raw pointers (`*const Archetype` and `*mut Archetype`) are used to hold
simultaneous references to two different HashMap entries. Safety is guaranteed
by a `debug_assert_ne!(old_id, new_id)` inside the unsafe block.

---

## World — The Central Hub

`World` is the single owner of all ECS state. It holds:

| Field                | Purpose                                                       |
| -------------------- | ------------------------------------------------------------- |
| `archetypes`         | All archetypes (HashMap for lookup by ID)                     |
| `archetype_lookup`   | ComponentMask → ArchetypeId for query matching                |
| `entity_locations`   | Entity → (ArchetypeId, index) for O(1) component access       |
| `resources`          | Global singleton data (Box\<dyn Any\>)                        |
| `resource_ticks`     | Change-detection ticks per resource                           |
| `component_registry` | Bit assignments and type names                                |
| `component_copiers`  | Closures that copy components between archetypes              |
| `free_entity_ids`    | Recycled entity IDs with incremented generations              |
| `change_tick`        | Monotonically increasing world tick (bumped once per frame)   |
| `system_last_run`    | Baseline tick for change-detection filters                    |
| `script_updaters`    | Function pointers for calling `update()` on script components |

### Entity Lifecycle

```
Allocate: free_entity_ids.pop() or next_free_entity_id++
Insert:   get_or_create_archetype() → insert_entity_with_components()
Modify:   add_component() / remove_component() → move_entity_to_archetype()
Destroy:  destroy_entity() → swap_remove from archetype → push to free list
```

---

## Queries

### QueryTarget — what data to fetch

```rust
pub trait QueryTarget {
    type Item<'a>;
    type State;                              // cached per-archetype pointers
    fn component_ids() -> Vec<ComponentId>;
    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>);
    fn init_state(archetype, this_run) -> State;
    fn fetch_with_state(state, index) -> Item<'_>;
}
```

Built-in implementations:

| Type                 | Yields                         | Access   |
| -------------------- | ------------------------------ | -------- |
| `Entity`             | `Entity` handle                | None     |
| `&T`                 | `&T`                           | Read     |
| `&mut T`             | `Mut<'_, T>` (change-tracking) | Write    |
| Tuples up to arity 4 | Tuple of the above             | Combined |

A duplicate-write guard (`has_duplicate_writes`) catches `(&mut T, &mut T)` at
system registration time in debug builds.

### QueryFilter — which rows to include

```rust
pub trait QueryFilter {
    type State;
    fn included_component_ids() -> Vec<ComponentId>;
    fn excluded_component_ids() -> Vec<ComponentId>;
    fn init_state(archetype, last_run, this_run) -> State;
    fn matches(state, index) -> bool;
}
```

Two-level filtering:

1. **Archetype level** (fast): `included_component_ids` and
   `excluded_component_ids` narrow the set of archetypes the query visits.
   `With<T>` adds T to the include set; `Without<T>` adds T to the exclude set.
2. **Row level** (per-entity): `matches(state, index)` tests each row against
   the filter predicate. `Changed<T>` checks whether `ticks[index].changed`
   falls between the system's `last_run` and `this_run`.

`Or<(A, B)>` ORs row-level predicates. The archetype-level include/exclude sets
are unioned (not intersected) so all inner filters can run safely.

### Query execution flow

```
Query::iter_mut()
├─ build_query_mask()      → union of Q::component_ids + F::included_component_ids
├─ build_exclusion_mask()  → F::excluded_component_ids
├─ filter archetypes       → HashMap iteration, sorted for determinism
├─ init_state per archetype → cache raw pointers to component storage
└─ iterate rows
   ├─ F::matches(row)      → row-level filter
   └─ Q::fetch_with_state  → construct Item from cached pointers
```

For parallel iteration (`par_iter_mut`), the same flow produces a flat
`Vec<FilteredArchetypeRange>` that Rayon distributes across threads.

---

## Change Detection

### The tick system

```
World.change_tick  — incremented once per frame
System.last_run    — the tick at which this system last started
ComponentTicks { added, changed }  — per-component-instance metadata
```

`Tick` is a newtype around `u32`. Wrapping arithmetic is used (overflow after
~828 days at 60 FPS — documented limitation). The comparison
`ticks.changed > last_run && ticks.changed <= this_run` answers "was this
component mutated since my system last ran?"

### Mut<T> — automatic tick bumping

```rust
pub struct Mut<'a, T> {
    value: &'a mut T,
    ticks: &'a mut ComponentTicks,
    this_run: Tick,
}

impl<T> DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.ticks.changed = self.this_run;  // ← bump on mutation
        self.value
    }
}
```

`&mut T` queries yield `Mut<T>` instead of bare `&mut T`. Existing code that
does `transform.x += 1.0` compiles unchanged — `DerefMut` handles the tick
bump transparently. No atomics are needed because the scheduler guarantees each
row is accessed by at most one thread.

`bypass_change_detection()` and `set_changed()` provide escape hatches for
internal bookkeeping.

### Resource change detection

`ResMut<T>::get_mut()` returns `Mut<'_, T>` (the same wrapper), so resource
mutations are also tracked.

### Implementation details — why `DerefMut` is cheap

At first glance, bumping a tick on every `DerefMut` looks expensive — an
extra store per mutated component, times thousands of entities, every frame.
In practice it's the cheapest operation in the loop.

**What `DerefMut` actually does:**

```rust
fn deref_mut(&mut self) -> &mut T {
    self.ticks.changed = self.this_run;  // one plain u32 store
    self.value                            // pointer already in a register
}
```

A single `mov` instruction (~1 cycle latency). No `AtomicU32`, no `LOCK`
prefix, no memory barrier.

**Why no atomics are needed.** Bevy uses `AtomicU32` for change detection
because its scheduler does not guarantee strict per-row exclusivity — two
systems in the same batch could touch different components of the same
entity, requiring synchronized tick updates. This ECS's scheduler is
stricter: systems in a parallel batch have **disjoint component-type
access**, and within that, each thread owns its entity index range
exclusively. No two threads ever observe the same row, so a plain `u32`
store is safe.

**The actual cost hierarchy** for a system processing 10,000 entities:

| Operation             | Cost per row                                                   |
| --------------------- | -------------------------------------------------------------- |
| `pos.x += vel.vx`     | ~3 memory accesses (load vel, load pos, store pos)             |
| Filter `matches(row)` | 1 memory access (load `ticks[i].changed`) + 2 integer compares |
| `DerefMut` tick bump  | 1 store to already-hot cache line                              |
| **Total**             | ~5 memory ops, all streaming through contiguous arrays         |

The data access (`pos.x`, `vel.vx`) dominates. The tick column lives in the
same archetype, parallel to the component data, and accessed with the same
index — the CPU's hardware prefetcher streams through both arrays together.
The tick store lands on a cache line that's already hot from the data access
on the same row.

For comparison, a manual `HashMap<Entity, bool>` dirty-flag approach would
add a hash computation, a bucket probe, and a likely cache miss per entity.

### Implementation details — cache behaviour of the filter scan

The change-detection filter reads the ticks column for **every** entity in an
archetype but only fetches the data column for entities that pass. This
two-column, filter-first design is deliberately tuned for how modern CPU
caches and hardware prefetchers work.

**The memory layout.** An archetype stores data in parallel `Vec`s:

```
component_storages   → Vec<Position>       [P0][P1][P2]...[P9999]  (8 bytes each)
component_ticks      → Vec<ComponentTicks> [T0][T1][T2]...[T9999]  (8 bytes each)
```

Each `ComponentTicks` is two `u32` fields (`added`, `changed`) — 8 bytes.
Each cache line (64 bytes) holds 8 ticks or 8 Positions. The two arrays
live at different heap addresses, map to different cache sets, and never
evict each other.

**The filter scan — optimal for hardware prefetchers.** The filter walks
the ticks array linearly with a constant stride of 8 bytes:

```
for index in 0..10_000:
    read ticks[index].changed   ← sequential, stride-8, 100% predictable
    compare against (last_run, this_run]
    if match → read positions[index]  ← sparse, unpredictable
```

The CPU's hardware prefetcher detects the sequential stride within the first
few iterations and begins fetching upcoming cache lines from RAM before the
loop reaches them. By the time `index = 8`, cache lines for indices 16–23
are already in L2. The entire 80 KB ticks column streams through cache at
memory-bandwidth speed — about 1.6 microseconds on modern hardware.

The positions array is accessed only for changed entities. These accesses
are sparse and unpredictable — the prefetcher cannot help. But that's the
point: the filter **eliminates** the position load for 9,950 of 10,000
entities. The few cache misses on the positions that did change are
dwarfed by the bandwidth saved on the ones that didn't.

**Why parallel arrays beat interleaving.** A common alternative is to
interleave data and metadata: `[P0][T0][P1][T1]...`. This would load
every `Position` into cache alongside its tick, even for entities that
the filter skips — wasting 50% of loaded bytes on unused data. Keeping
them in separate arrays means the filter scan touches nothing but the
small, dense ticks column.

**Bandwidth comparison** for 10,000 entities with 50 changed:

| Approach                    | Bytes read from RAM                                             |
| --------------------------- | --------------------------------------------------------------- |
| **Filter-first** (this ECS) | ~80 KB (all ticks) + 50 × 8 B (changed Positions) = ~80.4 KB    |
| Fetch-first then check      | ~80 KB (all ticks) + ~80 KB (all Positions) = ~160 KB           |
| Interleaved `[P][T]`        | ~160 KB (50% wasted on skipped Positions in shared cache lines) |

For larger components (e.g. a 64-byte `Transform` matrix), the gap widens
to 80 KB vs 720 KB — nearly an order of magnitude difference. The ticks
column is deliberately kept as small as possible (`u32` + `u32`) to
minimise the filter scan overhead.

**No intermediate arrays — the filter is inline.** The filter operates
directly inside the iterator's `next()` loop. No filtered index list is
ever materialised:

```
QueryIterMut::next()
  loop {
      index++
      if ticks[index].changed NOT in (last_run, this_run]:
          continue          ← just increment index, try next
      return fetch_with_state(index)  ← only reached for matching rows
  }
```

```
index:  0    1    2    3    4    5    ...
tick:  42   41   40   42   39   41   ...
         │    │    │    │    │    │
         ▼    ▼    ▼    ▼    ▼    ▼
       pass  skip skip pass skip skip
         │              │
         ▼              ▼
       yield          yield
       P0 data        P3 data
```

The only allocation in the entire query path is the `Vec<ArchetypeId>` of
matching archetypes (step 2, typically <50 entries). After that it's purely
integer index increments, pointer dereferences, and comparisons — all on
the stack, zero heap activity per row.

**Zero overhead when no filter is used.** The default filter is `()` (the
unit type). Its `matches` method returns `true` and is annotated
`#[inline(always)]`. When `F = ()` is monomorphised, the compiler inlines
the call to `true`, then eliminates the `if !true { continue }` branch as
dead code. No function call, no branch, no register comparison survives
into the generated machine code — the iterator behaves identically to a
raw `for` loop with no filter. The same applies to `With<T>` and
`Without<T>`, whose `matches` also unconditionally return `true` (they
filter purely at the archetype level). Only `Changed<T>` and `Added<T>`
introduce per-row runtime cost, and only when explicitly requested.

**Why the two arrays don't thrash.** `positions[index]` and `ticks[index]`
sit in different `Vec`s at unrelated heap addresses, so they map to
different L1/L2 cache sets. Accessing one never evicts the other. Even
in the rare case of set-aliasing (same cache set, different addresses),
the ticks scan moves forward linearly and never revisits old indices, so
evicting a just-read ticks line is harmless — it won't be needed again.

**The prefetcher's view during the scan:**

```
Time →
CPU:      [read T0-7] [read T8-15] [read T16-23] ...
Prefetch:              [fetch T24-31] [fetch T32-39] ...
                            ↑ ~128 bytes ahead = 2 cache lines of lookahead
```

The entire filter scan is compute-bound by the integer
comparisons, not memory-bound — the data is already in cache by the time
the CPU needs it.

## System Infrastructure

### System trait

```rust
pub trait System: Send {
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue);
}
```

Every registered system is `Box<dyn System>`. Closures that match the expected
parameter signature are auto-converted via blanket impls.

### SystemParam — automatic parameter resolution

```rust
pub trait SystemParam: Sized {
    fn fetch(world: &mut World, queue: &mut CommandQueue) -> Self;
    fn report_access(access: &mut SystemAccess);
}
```

When a system runs, the engine calls `SystemParam::fetch` for each parameter
type. The parameter types also report their access patterns during registration
so the scheduler can build the dependency graph.

**SAFETY NOTE:** `SystemParam::fetch` uses `std::mem::transmute` to convert
actual borrow lifetimes to `'static`. This is technically UB if the parameter
escapes the system function. The engine's architecture ensures this never
happens (systems are called as opaque functions, parameters are dropped before
the system returns), but there is no compile-time enforcement. This is a
documented, acknowledged risk with a clear safety contract.

### SystemParamFunction — tuple-to-closure conversion

A macro implements `SystemParamFunction` for tuples up to arity 6,
automatically converting `fn(A, B, C) { ... }` into a callable system that
receives `A::fetch(...)`, `B::fetch(...)`, `C::fetch(...)`.

---

## Scheduler & Parallel Execution

### SystemAccess — conflict detection

```rust
pub struct SystemAccess {
    pub reads: HashSet<ComponentId>,
    pub writes: HashSet<ComponentId>,
    pub uses_commands: bool,
    pub resource_reads: HashSet<ResourceId>,
    pub resource_writes: HashSet<ResourceId>,
}
```

`conflicts_with(other)` returns true if any of these hold:
- Either system uses Commands
- Write-write overlap on same component
- Read-write overlap on same component
- Write-write or read-write overlap on same resource

Read-read overlap is **not** a conflict — those systems can run in parallel.

### Batch building algorithm

A greedy O(n²) algorithm (documented tradeoff):
1. Start with all systems unscheduled.
2. For each system, check if it conflicts with any system already in the
   current batch.
3. If no conflict, add it. If conflict, skip it for this batch.
4. When the batch is full (no more systems can be added), start a new batch.
5. Repeat until all systems are scheduled.

The algorithm is always **correct** (no conflicting systems in the same batch)
though potentially suboptimal. For typical system counts (<100), O(n²) is
negligible.

### Parallel execution

`Engine::run_systems_parallel()`:
1. Iterates through batches from the execution graph.
2. For each batch, creates raw pointers to World, CommandQueue, and the systems
   slice — necessary because Rust can't express "these N systems each get
   `&mut World` but access disjoint data."
3. Uses `rayon::par_iter().for_each()` to distribute systems across threads.
4. Each thread sets a thread-local `last_run` override before invoking its
   system, so change-detection filters get the correct per-system baseline
   even when multiple systems run concurrently.

The scheduler guarantees that Commands-using systems are **alone in their
batch**, so the shared `&mut CommandQueue` is never actually accessed by
multiple threads simultaneously. Combined with the raw-pointer indirection
through `usize`, this avoids data races.

---

## Deferred Commands

### Why deferred?

If a system holding `&mut World` destroys an entity while another system is
iterating that entity's archetype, the iterator's pointers become dangling.
Deferred commands solve this by splitting each frame into two phases.

### CommandQueue

```rust
enum DeferredCommand {
    CreateEntity { component_adders: Vec<Box<dyn ComponentAdder>> },
    AddComponentToEntity { entity, component_adder },
    RemoveComponentFromEntity { entity, component_id },
    DestroyEntity { entity },
}
```

During Phase 1, systems call `commands.create_entity().with(...).build()` etc.
— these push `DeferredCommand` variants onto a plain `Vec`. No structural
changes happen yet.

During Phase 2, `CommandQueue::execute_queued_commands()` drains the queue and
calls the corresponding World methods (`allocate_entity`,
`move_entity_to_archetype`, `destroy_entity`). By this point all system
iterators have been dropped, so no dangling pointers exist.

### Commands system parameter

```rust
pub struct Commands<'a> { command_queue: &'a mut CommandQueue }
```

`Commands` is a `SystemParam` that reports `uses_commands = true`, which makes
the scheduler run the system exclusively (alone in its batch).

---

## Resources

Resources are global singleton data stored in the World rather than attached to
entities. Examples: `GameTime`, `InputState`, `AssetStore`, `Config`.

```rust
pub trait Resource: Send + Sync + 'static {}
```

Unlike components, resources require `Sync`. See [Implementation
details](#implementation-details-1) below for the full explanation.

### Access patterns

| System parameter | World method                                         | Scheduler tracking       |
| ---------------- | ---------------------------------------------------- | ------------------------ |
| `Res<T>`         | `get_resource::<T>()`                                | `add_resource_read`      |
| `ResMut<T>`      | `get_resource_mut_tracked::<T>()` — returns `Mut<T>` | `add_resource_write`     |
| `ResHandle<T>`   | `handle.get(&world)` / `handle.get_mut(&mut world)`  | Not tracked (manual API) |

`ResMut::get_mut()` returns `Mut<'_, T>` (same wrapper as component queries),
so `DerefMut` automatically bumps `resource_ticks[&id].changed`. This enables
resource change detection.

### ResHandle

`ResHandle<T>` is a zero-sized, `Copy` handle that stores type information
without borrowing the World. Useful for deferring resource access or passing
resource type information between systems.

### Implementation details — why `Resource: Sync` but not `Component`

The `Sync` difference comes down to **how data crosses thread boundaries**
in each case.

**Components** — raw pointers cross threads, not references. A parallel query
sends `SendPtr<Vec<Position>>` (a raw-pointer wrapper) to each Rayon worker.
The `&Position` reference is created *on the worker thread* from that raw
pointer via `unsafe { ptr.get(index) }`. The reference is born, lives, and
dies on a single thread — it never crosses a thread boundary. Raw pointers
are always `Send`, so `Component` doesn't need `Sync`.

**Resources** — the `Res<T>` system parameter itself crosses threads.
`Res<T>` contains `&World`, and `World` must be `Sync` for `&World: Send`.
When the worker calls `time.get()`, it creates `&GameTime` from the
(already-shared) `&World`. For that `&GameTime` to be usable on the worker
thread, `GameTime` must be `Sync` (because `&T: Send` iff `T: Sync`).

In short: component references never leave their birth thread (raw pointers
do the travelling); resource references are born from a `&World` that already
crossed threads, so the pointee must be `Sync`.

---

## Script Components

Script components are components that self-update each frame:

```rust
pub trait ScriptComponent: Component {
    fn update(&mut self, ctx: &mut ScriptContext);
}
```

### ScriptContext — safe, restricted access

`ScriptContext` provides:
- **Read-only access** to the World: `get_component::<T>(entity)`,
  `entity_exists(entity)`, `get_resource::<T>()`.
- **Mutable access to other components** via `get_component_mut::<T>(entity)` —
  this uses a raw-pointer path (`World::get_component_ptr_mut`) to bypass
  Rust's aliasing rules. The contract is that all structural changes are
  deferred, so the pointers remain valid for the duration of the script update.
- **Deferred commands** (same queue as systems): `create_entity()`,
  `destroy_entity()`, `add_component()`, `remove_component()`.

The key safety guarantee: a script **cannot** directly add/remove components or
destroy entities on the World. Those operations go through the command queue
and execute after all scripts finish, preventing use-after-free from archetype
migration during iteration.

### Script updater storage

`World.script_updaters` maps `ComponentId → fn(...)`. These are plain function
pointers (not closures), so they capture no state and cannot stash raw pointers
across calls. The pointers are passed fresh by `update_scripts()` on every
invocation.

---

## Frame Lifecycle

```
Engine::process_frame()
│
├─ world.increment_change_tick()
│   └─ change_tick = change_tick.wrapping_add(1)
│
├─ [if graph_dirty] scheduler.build_execution_graph()
│   └─ greedy batching of systems by access pattern
│
├─ Phase 1: Execute systems
│   ├─ [parallel mode] run_systems_parallel()
│   │   └─ for each batch:
│   │       └─ rayon::par_iter over batch
│   │           ├─ create raw pointers to world, queue, systems
│   │           ├─ set thread-local last_run override
│   │           ├─ system.run(&mut world, &mut queue)
│   │           └─ restore thread-local override
│   │
│   └─ [sequential mode] run_systems_sequential()
│       └─ for each enabled system:
│           ├─ world.system_last_run = system.last_run
│           ├─ system.run(&mut world, &mut queue)
│           └─ system.last_run = current_tick
│
├─ world.update_scripts(&mut queue)
│   └─ for each script component type:
│       ├─ find archetypes containing that component
│       ├─ collect (entity, archetype_id, index) — sorted for determinism
│       └─ call updater fn → component.update(&mut ScriptContext)
│
└─ Phase 2: Apply deferred commands
    └─ queue.execute_queued_commands(&mut world)
        ├─ CreateEntity → allocate + insert
        ├─ AddComponentToEntity → move_entity_to_archetype
        ├─ RemoveComponentFromEntity → move or destroy
        └─ DestroyEntity → swap_remove + free list
```

---

## Thread Safety Model

### Scheduler guarantees (Phase 1)

The scheduler is the **sole source of safety** for parallel execution. It
ensures:
1. No two systems in a batch write the same component type.
2. No system writes a component type while another reads it.
3. No two systems in a batch write the same resource type.
4. No system writes a resource type while another reads it.
5. Systems using `Commands` are alone in their batch.

### Raw pointer patterns

Three places use raw pointers to work around the borrow checker:

| Location                                  | Pattern                               | Safety guarantee                                                                             |
| ----------------------------------------- | ------------------------------------- | -------------------------------------------------------------------------------------------- |
| `engine.rs` — parallel world access       | `*mut World as usize`                 | Scheduler guarantees disjoint access; each thread gets different entity indices              |
| `engine.rs` — shared CommandQueue         | `*mut CommandQueue as usize`          | Scheduler ensures Commands systems are exclusive; non-Commands systems never touch the queue |
| `world.rs` — dual archetype access        | `*const Archetype` + `*mut Archetype` | `debug_assert_ne!(old_id, new_id)` — different HashMap keys                                  |
| `query/target.rs` — init_state for tuples | `*mut Archetype`                      | Scheduler guarantees exclusive World access during init                                      |

### Thread-local change-detection baseline

Parallel batches share a single `World.system_last_run` field. To give each
thread the correct baseline for its system, the engine uses a thread-local
`Cell<Option<Tick>>` override. Queries check the override first; if absent,
they fall back to the world-level field (used in sequential mode).

---

## Module Map

```
                    ┌──────────────┐
                    │    Engine    │  frame loop, system reg, parallel runner
                    └──────┬───────┘
                           │ owns
          ┌────────────────┼────────────────┐
          ▼                ▼                ▼
   ┌──────────┐    ┌──────────────┐   ┌───────────┐
   │  World   │    │ CommandQueue │   │ Scheduler │
   │ (state)  │    │ (deferred)   │   │ (batching)│
   └────┬─────┘    └──────┬───────┘   └───────────┘
        │                 │
   ┌────┼────────────┐    │
   ▼    ▼            ▼    ▼
┌──────┐ ┌────────┐ ┌──────────┐
│Arch. │ │Entity  │ │Resource  │
│(SoA) │ │(handle)│ │(singleton│
└──┬───┘ └───┬────┘ └────┬─────┘
   │         │            │
   └────┬────┘            │
        ▼                 ▼
   ┌──────────────────────────┐
   │        Query System       │
   │  QueryTarget, QueryFilter │
   │  iter_mut, par_iter_mut   │
   │  Mut<T>, Res<T>, ResMut   │
   └──────────────────────────┘
              │
              ▼
   ┌──────────────────────────┐
   │    System Infrastructure  │
   │  SystemParam, IntoSystem  │
   │  (lifetime transmutation)  │
   └──────────────────────────┘
              │
              ▼
   ┌──────────────────────────┐
   │    Script Components      │
   │  ScriptContext (safe API) │
   │  fn pointers (no capture) │
   └──────────────────────────┘
```

**Dependency flow:**
- `component.rs` ← used by everything (Component, Tick, ComponentMask, ComponentId)
- `entity.rs` ← used by World, Queries, Commands
- `archetype.rs` ← used by World, Queries
- `world.rs` ← used by Engine, Commands, Queries, Scripting
- `commands.rs` ← used by Engine, Systems, Scripting
- `scheduler.rs` ← used by Engine
- `system.rs` ← used by Engine (system registration)
- `query/*.rs` ← used by system.rs (SystemParam impls), Engine (via systems)
- `scripting.rs` ← used by World (update_scripts), Engine (process_frame)
- `resource.rs` ← used by World, Res/ResMut
