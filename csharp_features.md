# C# ECS Pipeline Roadmap

This document tracks the work required to turn the current C# demo bridge into
a general ECS scripting backend. Items are ordered by correctness and by how
many later features depend on them.

For a detailed explanation of the previous and current component architecture,
see [`components_approach.md`](components_approach.md).

## Current pipeline

```text
Rust host
  -> starts .NET through hostfxr
  -> loads csharp_runtime.dll
  -> csharp_runtime loads game_cs.dll in a collectible AssemblyLoadContext
  -> query parameters are reflected into Rust SystemAccess
  -> Rust scheduler invokes managed systems
  -> managed query iterators borrow native archetype columns
```

The pipeline now registers every unmanaged component used by discovered C#
systems from a managed manifest. Renderer-owned components use stable mirrors
from `csharp_runtime`; game-owned components use type-erased native storage. Demo
world initialization is still native and remains the next major hardcoded part.
Composable queries now derive scheduler access automatically, expose entity
IDs through `EntityTerm`, and update per-row change ticks for managed writes.

## Implementation phases

- [x] Phase 1: Correctness and stable metadata
  - [x] Track managed component writes.
  - [x] Introduce collision-resistant component identities.
  - [x] Validate complete component layouts.
- [ ] Phase 2: Dynamic game-defined worlds
  - [x] Export a component manifest from `game_cs`.
  - [x] Register unmanaged components dynamically.
  - [x] Expose entity IDs in managed queries.
  - [x] Add deferred entity/component commands.
  - [ ] Move game initialization into C#.
- [ ] Phase 3: General system parameters
  - [x] Replace specialized query classes with composable query descriptors.
  - [ ] Add resources and query filters.
  - [ ] Support multiple system parameters.
- [ ] Phase 4: Complete hot reload
  - [x] Reject incompatible query/manifest reloads without committing them.
  - [ ] Rebuild scheduler metadata when system signatures change.
  - [ ] Add component schema migration.
  - [ ] Improve dependency and PDB loading.
- [ ] Phase 5: Diagnostics, performance, and hardening
  - [x] Add manifest, scheduler, write-tracking, and query regression tests.
  - [ ] Export managed system names and propagate errors.
  - [ ] Replace repeated chunk joins with one native query cursor.
  - [ ] Add automated hostfxr/reload lifecycle tests and improve .NET discovery.
  - [ ] Define the trust and sandboxing model.

---

## 1. Managed write tracking

**Priority:** Critical

**Status:** Implemented

### Problem

Managed queries receive direct pointers to component arrays. Writing through a
C# `Span<T>` bypasses Rust's `Mut<T>` wrapper, so the component's `changed`
tick is never updated. Rust systems using `Changed<T>` can therefore miss C#
mutations.

### Implementation

Implemented. `NativeComponentChunk` now carries the component's parallel
`ComponentTicks` column and the world's change tick captured while the managed
system is active. Rust and C# use explicit ABI-compatible tick layouts.

Required writes mark the exact row when `QueryRow.Write<T>()` is requested:

```csharp
ref T value = ref row.Write<T>();
```

Rows that are merely yielded are not marked. Optional writes use a stack-only
`OptionalWriteRef<T>` and mark only when its `.Value` ref is requested;
checking `.HasValue` does not count as a write. `Read<T>()` and
`OptionalRead<T>()` never update ticks.

This has the same conservative behavior as Rust's `Mut<T>`: requesting a
writable reference marks the row even if gameplay subsequently chooses not to
change the value. Precisely detecting assignments after returning a raw C#
`ref T` is not possible without replacing it with a more restrictive wrapper.

### Affected files

- `engine/src/world.rs`
- `engine/src/query/change_detection.rs`
- `host/src/csharp/abi.rs`
- `csharp_runtime/src/EngineApi.cs`
- `csharp_runtime/src/Engine.cs`

### Acceptance tests

- [x] A C# system writes one entity and a following Rust `Changed<T>` query returns
  exactly that entity.
- [x] Read-only C# queries do not set changed ticks.
- [x] Parallel writes to different component types update the correct tick columns.

---

## 2. Dynamic component registration

**Priority:** Critical

**Status:** Implemented

### Problem

The original Rust bridge contained explicit Rust definitions and match arms for
every C# component. Adding a component required modifying and recompiling the
host, so C# could not define the game's component universe.

### Implementation

Implemented. `csharp_runtime` inspects every component term in every discovered
system plus supported unmanaged structs declared by the game assembly. The
latter makes components used only by startup/deferred commands available
before those commands run. It exports a UTF-8 JSON manifest before Rust reads
scheduler access.
Each descriptor contains:

```text
ComponentDescriptor
- stable_id: two u64 halves derived from the canonical full name
- full_name: UTF-8 managed full name
- size and alignment
- schema_hash: FNV-1a hash of the canonical field schema
- fields: name, offset, size, primitive kind, and nested fields
- shared: whether the type is owned by csharp_runtime
```

`World::register_dynamic_component` installs a type-erased storage factory.
`DynamicColumn` owns an aligned dense byte allocation and supports append,
zero initialization, row replacement, swap-remove, and copying during
archetype migration. Tick columns remain parallel to both native and dynamic
storage, so managed writes keep the same change-detection behavior.

The host loads and validates the manifest before running managed startup or
registering systems. Its runtime binding table is keyed by the full 128-bit
stable ID. A binding either dispatches to a generic native column callback or
to a dynamic column; `ffi_get_component_chunk` contains no per-game type match.

`Position`, `Color`, and `Sprite` now live in `csharp_runtime/SharedComponents.cs`.
The native registry explicitly binds the renderer-owned `Position` and
`Sprite` mirrors and checks size, alignment, and the complete field schema.
`PhysicsState` and `BallTag` live only in `game_cs`; both register from the
manifest without Rust component definitions. The managed `[EcsStartup]`
method now chooses exact entity composition and initializes all values through
deferred commands; the host has no demo-specific component attachment policy.

Reload remains behavior-only. `GameHost` compares the new manifest byte for
byte with the active one and rejects schema or identity changes, preserving the
old collectible context until the process restarts and can rebuild storage.

### Affected files

- `engine/src/component.rs`
- `engine/src/archetype.rs`
- `engine/src/world.rs`
- `host/src/csharp/components.rs`
- `csharp_runtime/src/ComponentManifest.cs`
- `csharp_runtime/src/SharedComponents.cs`
- `csharp_runtime/src/Engine.cs`
- `csharp_runtime/src/GameHost.cs`
- `csharp_runtime/src/LoaderInterop.cs`

### Acceptance tests

- [x] Adding `BallTag` only in `game_cs` requires no host match arm or Rust type.
- [x] Two dynamically registered components coexist in one archetype.
- [x] Dynamic bytes survive add/remove archetype migration.
- [x] Dynamic `added` and `changed` ticks survive add/remove archetype migration.
- [x] Invalid managed and native-shared layouts fail before the first frame.

---

## 3. Entity lifecycle and deferred commands

**Priority:** Critical

**Status:** Implemented

### Problem

C# systems previously could not create or destroy entities or add and remove
components. The Rust bridge therefore owned game-specific demo creation.

### Implementation

- [x] Added an ABI-stable managed `Entity` containing the native ID and
  generation.
- [x] Added `EntityTerm` and native entity chunks, so a managed query can read
  the entity corresponding to each component row.
- [x] Kept entity access out of scheduler component conflicts.

The native API now exports:

   ```text
   reserve_entity()
   queue_create(entity, component_blob_list)
   queue_destroy(entity)
   queue_add_component(entity, component_id, bytes)
   queue_remove_component(entity, component_id)
   ```

`Commands` and `DeferredEntityBuilder` copy unmanaged component values into
pinned blobs, reserve generation-checked handles, and synchronously submit the
blobs to the host. Native shared values are decoded into concrete
`ComponentAdder`s; game components remain byte-owned dynamic values. Both are
stored in the existing Rust `CommandQueue`, so structural changes remain
deferred until the current system phase ends.

System discovery accepts `Commands`, a composable query, or one of each.
`SystemUsesCommands` is exported separately and sets
`SystemAccess.uses_commands`; command-producing managed systems consequently
use the same scheduler exclusivity rule as Rust systems. Calls outside that
declared invocation are rejected by the thread-local capability guard.

One-shot startup methods use:

   ```csharp
   [EcsStartup]
   public static void Start(Commands commands) { ... }
   ```

The host runs every startup under a command-enabled scope after component
registration and flushes the queue before registering/running frame systems.
`GameStartup.Start` creates exactly 100 fully initialized balls. The former
Rust `setup_world()` demo bootstrap has been removed.

All entity-taking callbacks validate the complete ID/generation pair before
queueing. Creation additionally requires a handle reserved during the same
managed invocation and rejects duplicate components, unknown IDs, null data,
or layout-size mismatches before enqueueing anything.

### Affected files

- `engine/src/commands.rs`
- `host/src/csharp/commands.rs`
- `csharp_runtime/src/EngineApi.cs`
- `csharp_runtime/src/Engine.cs`
- `csharp_runtime/src/GameHost.cs`
- `game_cs/src`

### Acceptance tests

- [x] C# startup creates exactly 100 balls without Rust demo code.
- [x] A C# system can despawn an entity safely during deferred command execution.
- [x] Managed commands add/remove a component and migrate the entity correctly.
- [x] Managed commands reject stale entity generations.

---

## 4. Composable generic queries

**Priority:** High

**Status:** Implemented for query arities one through eight

### Problem

The original runtime needed a new class and reflection branch for each shape,
such as `WriteQuery`, `WriteReadQuery`, and `Write3Query`. This grew
combinatorially and did not match Rust tuple queries.

### Implementation

Implemented. Query intent is represented by independent terms:

   ```csharp
   Read<T>
   Write<T>
   OptionalRead<T>
   OptionalWrite<T>
   EntityTerm
   ```

Terms compose through query arities from one through eight:

   ```csharp
   Query<Write<Position>, Read<Velocity>>
   Query<EntityTerm, Write<Health>>
   ```

Every closed query implements `IQueryDescriptor`. Its cached `QueryDescriptor`
contains ordered component keys, native sizes, access modes, optional flags,
and entity terms. `GameHost.CreateSystem` consumes only this interface and no
longer branches on query arity or read/write shape.

All arities share one `QueryEnumerator` and one `QueryRow`. Gameplay retrieves
typed references with `row.Write<T>()`, `row.Read<T>()`,
`row.OptionalWrite<T>()`, and `row.OptionalRead<T>()`; `row.Entity` exposes an
`EntityTerm`. Reflection occurs only once while a closed descriptor or generic
component metadata cache is initialized. Row access reuses its cached stable
component ID, so it performs no type-name lookup, UTF-8 allocation, or hashing
while rows are iterated. A source generator is therefore not required for the
hot path.

`QueryRow`, `QueryEnumerator`, and optional reference wrappers are `ref struct`
types, preventing native pointers from being boxed, captured, or retained on
the managed heap. Entity chunks are provided by the native ECS adapter and
also drive queries containing only optional terms.

### Acceptance tests

- [x] Supported read/write combinations work without modifying `GameHost`.
- [x] Duplicate or contradictory component terms fail during discovery.
- [x] Scheduler access exactly matches every query term.
- [x] Query rows remain stack-only and cannot retain native spans.

---

## 5. Scheduler rebuild during hot reload

**Priority:** High

**Status:** Partially implemented; incompatible reloads are safe but require a restart

### Problem

Managed behavior can reload, but changing a method name or query signature is
rejected because the Rust scheduler graph was built only at startup.

### Implemented foundation

- [x] A candidate assembly is loaded into a new collectible context before the
  active context is replaced.
- [x] The candidate's ordered system signatures and complete component
  manifest are validated first.
- [x] Incompatible candidates are rejected and unloaded while the old context
  remains active.

### Remaining implementation

1. Export a complete native-consumable system manifest for the candidate
   context, including names and stable system IDs.
2. At a frame boundary:
   - stop scheduling old managed systems;
   - validate all new component and system metadata;
   - clear only managed system registrations;
   - register new closures and rebuild the execution graph;
   - commit the new managed context.
3. Make native registration transactional so a failure preserves both the old
   scheduler closures and old managed context.
4. Assign stable system IDs so unchanged systems preserve enabled/disabled
   state and timing information.

### Acceptance tests

- [ ] Changing `Read<T>` to `Write<T>` rebuilds the scheduler without restart.
- [ ] Adding and removing a system works at the next frame boundary.
- [ ] A failed native registration leaves the old systems running.
- [ ] No closure retains a function pointer into an unloaded context.

---

## 6. Stable component identity

**Priority:** High

**Status:** Implemented for full-name identities; explicit rename-stable IDs remain optional work

### Problem

The original key was one FNV-1a hash of `Type.Name`, so `Foo.Position` and
`Bar.Position` collided by construction. The manifest work replaced it with a
128-bit ID derived from `Type.FullName` and rejects duplicate IDs. An explicit
canonical-name attribute would still be useful for intentional type renames.

### Implementation

Implemented foundation: query metadata, manifests, native bindings, and FFI
calls all carry both halves of the 128-bit full-name ID. Duplicate IDs fail
during manifest loading, and collectible assembly identity/version is excluded.

Optional follow-up: introduce an explicit attribute:

   ```csharp
   [EcsComponent("game.physics.position", Version = 1)]
   public struct Position { ... }
   ```

The attribute's canonical name would replace `Type.FullName` as hash input,
allowing C# namespace/type refactors without changing persisted component IDs.

### Acceptance tests

- [x] Component IDs use `Type.FullName`, not the collision-prone short name.
- [x] Duplicate manifest IDs and incompatible ID re-registration fail.
- [x] Repeated ID generation for an unchanged full name is deterministic.
- [ ] Add a regression fixture with equal short names in different namespaces.
- [ ] Add explicit canonical IDs for rename-stable persisted schemas.
- [ ] Verify fixed ID fixtures on every supported process architecture.

---

## 7. Complete ABI and schema validation

**Priority:** High

**Status:** Implemented for current unmanaged components

### Problem

The original bridge checked only `sizeof(T)`, so equal-sized structs could
disagree on alignment, offsets, primitive types, signedness, or nested layout.

### Implementation

Implemented as part of dynamic registration. Managed reflection records size,
alignment, offsets, primitive kinds, and recursive nested fields in a canonical
schema. Components containing managed references, pointers, `bool`, `char`,
auto layout, or recursive value layouts are rejected during discovery.

The host validates every field range and uses the schema hash to compare shared
managed mirrors with the explicit native registry. Native shared types use
`#[repr(C)]`; game-owned dynamic types use their validated managed layout as
the authoritative schema.

### Acceptance tests

- [x] Reordered equal-sized shared fields are rejected.
- [x] Ambiguous managed `bool` and `char` fields are rejected.
- [x] Nested `Color` inside `Sprite` validates in the current x64 pipeline.
- [ ] Add explicit enum-underlying-type and cross-platform layout fixtures.

---

## 8. Resources (`Res<T>` and `ResMut<T>`)

**Priority:** Medium

### Problem

C# systems cannot access ECS singleton resources, and resource conflicts are
therefore absent from their scheduler declarations.

### Implementation

1. Add resource descriptors to the managed manifest.
2. Add native callbacks for scoped read/write resource access.
3. Implement managed parameters such as `ReadResource<T>` and
   `WriteResource<T>`.
4. Reflect them into `SystemAccess.resource_reads` and `resource_writes`.
5. Apply the same layout checks and change-tick behavior as components.

### Acceptance tests

- Two resource readers can run in parallel.
- A resource writer conflicts with readers and writers.
- Missing resources produce a descriptive system error.

---

## 9. Entity IDs in queries

**Priority:** Medium

**Status:** Implemented

### Problem

Managed rows originally exposed component values but not the entity owning the
row.

### Implementation

Implemented with a separate native entity-chunk callback. The managed
`Entity` struct mirrors the native ID/generation layout. `EntityTerm` requests
the entity column and `QueryRow.Entity` returns the entity at the current row.
The iterator joins entity and component columns by archetype ID and verifies
matching row counts.

`EntityTerm` is descriptor metadata rather than a component access, so it does
not add scheduler conflicts. Optional-only queries can use the entity column as
their driver.

### Acceptance tests

- [x] Managed query tests return the entity corresponding to each component row.
- [x] Native entity chunks are accessible only during the scheduled managed call.
- [ ] Add an end-to-end managed regression after native swap-remove and migration.

---

## 10. Query filters and change detection

**Priority:** Medium

### Problem

C# cannot express `With<T>`, `Without<T>`, `Changed<T>`, `Added<T>`, or logical
filter combinations.

### Implementation

1. Add filter descriptors independent from row terms.
2. Apply `With` and `Without` while selecting archetypes natively.
3. Apply `Changed` and `Added` per row using native tick columns and the
   managed system's last-run tick.
4. Support nested `And`/`Or` descriptors with validation identical to Rust.
5. Include filter-only component reads in scheduler metadata when required.

### Acceptance tests

- Managed filter results match equivalent Rust queries over the same world.
- `Changed<T>` observes writes performed by both Rust and C# systems.
- Filters referencing absent components never panic.

---

## 11. Multiple system parameters

**Priority:** Medium

### Problem

An `[EcsSystem]` method must currently accept exactly one query parameter.

### Implementation

1. Let `GameHost` inspect every parameter and require each to implement a
   managed system-parameter descriptor.
2. Merge component, resource, command, and filter access into one declaration.
3. Construct all parameter values once when compiling the method runner.
4. Reject conflicting parameters in the same method before registration.

   ```csharp
   [EcsSystem]
   static void Move(
       Query<Write<Position>, Read<Velocity>> query,
       ReadResource<FrameTime> time,
       Commands commands)
   ```

### Acceptance tests

- Access from all parameters appears in one native `SystemAccess`.
- Duplicate mutable borrows in separate parameters are rejected.
- Commands force the appropriate scheduler exclusivity.

---

## 12. Managed system names and controls

**Priority:** Medium

### Problem

Rust registers managed systems as `csharp_system_0`, hiding useful names from
profiling, diagnostics, and enable/disable APIs.

### Implementation

1. Export each system's UTF-8 fully qualified name with the system manifest.
2. Copy names into host-owned storage with a lifetime suitable for registration.
3. Add optional explicit stable IDs and display names to `[EcsSystem]`.
4. Preserve enabled state and timing history by stable ID during reload.

### Acceptance tests

- Tracy and logs show `TracyLive.BallPhysicsSystem.Run`.
- Systems can be enabled and disabled by their managed stable ID.

---

## 13. Managed error propagation

**Priority:** Medium

### Problem

`LoaderInterop.RunSystem` logs and swallows managed exceptions. Rust considers
the system successful even if it partially modified component data.

### Implementation

1. Return a status from `RunSystem` instead of `void`.
2. Store a managed error record containing system name, exception type,
   message, and formatted stack trace.
3. Add a native callback/export to copy the last error into host-owned memory.
4. Convert failures into the engine's frame error reporting policy.
5. Define whether one failed system stops the frame or allows independent
   scheduler batches to continue.

### Acceptance tests

- A thrown managed exception appears as a structured frame error.
- Exceptions never unwind across the unmanaged boundary.
- The error includes the actual managed system name and stack trace.

---

## 14. Managed dependency resolution

**Priority:** Medium

### Problem

The collectible context explicitly resolves only `csharp_runtime`. Gameplay
assemblies with additional NuGet or project dependencies may fail to load or
may bind accidentally to the default context.

### Implementation

1. Construct an `AssemblyDependencyResolver` from the game assembly path.
2. Resolve managed and unmanaged dependencies inside `GameContext`.
3. Continue returning the already-loaded `csharp_runtime` assembly for its shared
   contract types.
4. Define which dependencies are shared and which are collectible.
5. Report unresolved dependency names and searched locations.

### Acceptance tests

- A game can reference a secondary managed project and a NuGet library.
- Reload unloads collectible dependencies with the game context.
- `csharp_runtime` is never loaded twice into incompatible contexts.

---

## 15. PDB and debugging support

**Priority:** Medium

### Problem

The game is loaded from DLL bytes without its portable PDB, reducing source
locations in managed stack traces and debugger behavior.

### Implementation

1. Read the adjacent portable PDB with the same retry policy as the DLL.
2. When available, call `LoadFromStream(assemblyStream, pdbStream)`.
3. Treat a missing PDB as valid in release builds.
4. Include managed file/line information in propagated system errors.

### Acceptance tests

- A debug-build exception reports `Systems.cs` and a source line.
- Release builds without PDBs still load normally.

---

## 16. Native query cursor performance

**Priority:** Low

### Problem

`QueryEnumerator` independently scans chunks for each query term and joins them
by archetype ID. More terms increase repeated scans and managed/native calls.

### Implementation

1. Send the complete query descriptor to Rust once.
2. Build a native cursor containing only matching archetypes.
3. Return all requested component pointers, entity pointers, tick pointers, and
   lengths in one callback per archetype.
4. Cache cursor plans by query signature and archetype generation.
5. Invalidate caches when world archetypes change.

### Acceptance tests

- One native callback returns each matching archetype regardless of term count.
- Managed and Rust queries return identical rows.
- Benchmarks compare one-, three-, and eight-term queries.

---

## 17. Cross-platform .NET discovery

**Priority:** Low

### Problem

Runtime discovery currently checks `DOTNET_ROOT` and Windows Program Files.
Default Linux and macOS installations may require manual configuration.

### Implementation

1. Prefer Microsoft's `nethost` `get_hostfxr_path` API rather than manually
   searching version directories.
2. Retain `DOTNET_ROOT` as an explicit override.
3. Add platform-specific fallback diagnostics showing searched locations.
4. Validate architecture compatibility between the host and .NET runtime.

### Acceptance tests

- Runtime startup succeeds on supported Windows, Linux, and macOS machines.
- Missing/incompatible installations produce actionable errors.

---

## 18. End-to-end and concurrency tests

**Priority:** Low, but required before calling the backend production-ready

**Status:** Partially implemented

### Problem

The suite now covers managed query execution, manifest generation, dynamic
storage and migration, scheduler conflict derivation, native renderer queries,
and managed write tracking. It still does not automate the complete hostfxr,
reload, dependency, error, and collectible-context lifecycle.

### Implementation

Add automated scenarios for:

- [ ] hostfxr startup and managed export resolution;
- [x] component manifest registration and layout rejection;
- [x] dynamic component coexistence and archetype migration;
- [x] Rust/C# read-read parallelism and read-write exclusion;
- [x] managed writes observed by `Changed<T>`;
- [x] entity and optional-term managed query iteration;
- [x] renderer-facing native `Position`/`Sprite` query compatibility;
- [ ] managed commands and their end-to-end entity migration;
- [ ] behavior-only reload;
- [ ] scheduler-signature reload;
- [ ] failed reload rollback;
- [ ] exception propagation;
- [ ] collectible-context unloading;
- [ ] dependency and PDB loading;
- [ ] repeated startup/shutdown and multiple-frame execution.

Where GPU automation is unavailable, test the renderer-facing
`Query<(&Position, &Sprite)>` without creating a surface.

### Acceptance tests

- Tests run unattended from one documented command.
- Parallel tests execute enough iterations to expose aliasing or race failures.
- Reload tests prove the previous version remains active after failure.

---

## 19. Trust and sandboxing model

**Priority:** Security decision required before accepting third-party scripts

### Problem

Managed gameplay executes in-process with native pointers. C# code can call the
filesystem, network, process APIs, reflection, unsafe code, and arbitrary native
libraries. `AssemblyLoadContext` supports unloading; it is not a security
sandbox.

### Implementation options

1. **Trusted game code:** document that C# assemblies have the same authority
   as the native host. Keep the current in-process architecture.
2. **Restricted third-party code:** move scripts into a separate process and
   communicate through validated IPC/snapshots. Apply OS-level process
   restrictions appropriate to each platform.
3. **Portable sandbox:** compile supported gameplay code to a sandboxed runtime
   such as WebAssembly and expose a capability-limited ECS API.

Do not attempt to secure this pipeline using only reflection checks,
`AssemblyLoadContext`, or removed .NET Code Access Security APIs.

### Acceptance criteria

- The chosen trust model is explicit in project documentation.
- Untrusted code never receives raw pointers to native ECS memory.
- Every exposed capability has a documented authorization boundary.

---

## Recommended first milestone

Current progress toward the first useful milestone:

- [x] Correct managed change ticks.
- [x] Stable component IDs and full layout validation.
- [x] A game-exported component/system manifest.
- [x] Dynamic native storage for unmanaged C# components.
- [x] Composable scheduler-aware queries and entity IDs.
- [x] Managed startup plus deferred commands.
- [x] Removal of the demo-specific `setup_world()` implementation.

A new C# component can now be added without editing or recompiling the Rust
host. C# owns component schemas, startup entity composition, and frame
behavior; structural mutations use the engine's normal deferred command phase.
