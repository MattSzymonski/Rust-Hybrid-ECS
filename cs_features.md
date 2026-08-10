# C# ECS Pipeline Roadmap

This document tracks the work required to turn the current C# demo bridge into
a general ECS scripting backend. Items are ordered by correctness and by how
many later features depend on them.

## Current pipeline

```text
Rust host
  -> starts .NET through hostfxr
  -> loads cs_runtime.dll
  -> cs_runtime loads game_cs.dll in a collectible AssemblyLoadContext
  -> query parameters are reflected into Rust SystemAccess
  -> Rust scheduler invokes managed systems
  -> managed query iterators borrow native archetype columns
```

The current pipeline is suitable for the bouncing-ball demo. Component types,
world initialization, and native query dispatch are still defined manually in
`host/src/cs/cs_api.rs`.

## Implementation phases

- [ ] Phase 1: Correctness and stable metadata
  - [ ] Track managed component writes.
  - [ ] Introduce collision-resistant component identities.
  - [ ] Validate complete component layouts.
- [ ] Phase 2: Dynamic game-defined worlds
  - [ ] Export a component manifest from `game_cs`.
  - [ ] Register unmanaged components dynamically.
  - [ ] Add entity IDs and deferred commands.
  - [ ] Move game initialization into C#.
- [ ] Phase 3: General system parameters
  - [x] Replace specialized query classes with composable query descriptors.
  - [ ] Add resources and query filters.
  - [ ] Support multiple system parameters.
- [ ] Phase 4: Complete hot reload
  - [ ] Rebuild scheduler metadata when system signatures change.
  - [ ] Add component schema migration.
  - [ ] Improve dependency and PDB loading.
- [ ] Phase 5: Diagnostics, performance, and hardening
  - [ ] Export managed system names and propagate errors.
  - [ ] Replace repeated chunk joins with one native query cursor.
  - [ ] Improve .NET discovery and end-to-end tests.
  - [ ] Define the trust and sandboxing model.

---

## 1. Managed write tracking

**Priority:** Critical

### Problem

Managed queries receive direct pointers to component arrays. Writing through a
C# `Span<T>` bypasses Rust's `Mut<T>` wrapper, so the component's `changed`
tick is never updated. Rust systems using `Changed<T>` can therefore miss C#
mutations.

### Implementation

1. Extend `NativeComponentChunk` with a pointer to its `ComponentTicks` column,
   or expose a separate native `MarkChunkChanged` callback.
2. Prefer marking individual rows rather than the entire chunk:

   ```csharp
   ref T value = ref row.Write;
   // Mark the row when writable access is first requested or mutated.
   ```

3. A simple first implementation may mark every row yielded by a writable
   query. A later implementation can use a managed `WriteRef<T>` wrapper that
   marks only rows actually mutated.
4. Use the world's current change tick supplied at system entry.
5. Never update ticks for `ReadOnlySpan<T>` access.

### Affected files

- `engine/src/world.rs`
- `engine/src/query/change_detection.rs`
- `host/src/cs/cs_api.rs`
- `cs_runtime/src/EngineApi.cs`
- `cs_runtime/src/Engine.cs`

### Acceptance tests

- A C# system writes one entity and a following Rust `Changed<T>` query returns
  exactly that entity.
- Read-only C# queries do not set changed ticks.
- Parallel writes to different component types update the correct tick columns.

---

## 2. Dynamic component registration

**Priority:** Critical

### Problem

The Rust bridge currently contains explicit Rust definitions and match arms for
every C# component. Adding a component requires modifying and recompiling the
host, which means C# does not yet define the game's component universe.

### Implementation

1. Have `cs_runtime` inspect unmanaged structs used by discovered systems.
2. Export a component manifest before system registration:

   ```text
   ComponentDescriptor
   - stable_id: 128-bit identifier
   - full_name: UTF-8 name
   - size: u32
   - alignment: u32
   - schema_hash: u64 or u128
   - fields: name, offset, primitive type, nested schema
   ```

3. Add type-erased component storage to the engine. The storage must support:
   allocation, swap-remove, move between archetypes, default construction, and
   optional serialization without a compile-time Rust type.
4. Register manifest components before systems are registered.
5. Replace `component_id()` and the hardcoded callback match in `cs_api.rs`
   with a runtime registry keyed by `stable_id`.
6. Keep built-in native components such as renderer `Position` and `Sprite` in
   an explicit shared-component registry so managed types can bind to them.

### Affected files

- `engine/src/component.rs`
- `engine/src/archetype.rs`
- `engine/src/world.rs`
- `host/src/cs/cs_api.rs`
- `cs_runtime/src/GameHost.cs`
- `cs_runtime/src/LoaderInterop.cs`

### Acceptance tests

- Adding a new unmanaged struct only in `game_cs` requires no host edit.
- Two dynamically registered components can coexist in one archetype.
- Dynamic components survive entity migration between archetypes.
- Invalid managed layouts are rejected before the first frame.

---

## 3. Entity lifecycle and deferred commands

**Priority:** Critical

### Problem

C# systems cannot create or destroy entities or add and remove components.
`setup_world()` in the Rust bridge currently creates the demo entities.

### Implementation

1. Add an opaque managed `Entity` value matching the native entity ID and
   generation layout.
2. Add command callbacks to the native API:

   ```text
   reserve_entity()
   queue_create(entity, component_blob_list)
   queue_destroy(entity)
   queue_add_component(entity, component_id, bytes)
   queue_remove_component(entity, component_id)
   ```

3. Implement a managed `Commands` system parameter that records requests into
   the existing Rust `CommandQueue`.
4. Include `uses_commands` in reflected `SystemAccess`. The scheduler must keep
   command-producing systems under the same exclusivity rules as Rust systems.
5. Add a managed startup method, for example:

   ```csharp
   [EcsStartup]
   public static void Start(Commands commands) { ... }
   ```

6. Remove demo-specific entity creation from `setup_world()` after startup
   commands are available.

### Affected files

- `engine/src/commands.rs`
- `host/src/cs/cs_api.rs`
- `cs_runtime/src/EngineApi.cs`
- `cs_runtime/src/Engine.cs`
- `cs_runtime/src/GameHost.cs`
- `game_cs/src`

### Acceptance tests

- C# startup creates exactly 100 balls without Rust demo code.
- A C# system can despawn an entity safely during deferred command execution.
- Adding/removing a component migrates the entity to the correct archetype.
- Stale entity generations are rejected.

---

## 4. Composable generic queries

**Priority:** High

### Problem

The runtime currently needs a new class and reflection branch for each shape,
such as `WriteQuery`, `WriteReadQuery`, and `Write3Query`. This grows
combinatorially and does not match Rust tuple queries.

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
`EntityTerm`. Reflection occurs only once while a closed descriptor is built,
never while rows are iterated. A source generator is therefore not required
for the hot path.

`QueryRow`, `QueryEnumerator`, and optional reference wrappers are `ref struct`
types, preventing native pointers from being boxed, captured, or retained on
the managed heap. Entity chunks are provided by the native ECS adapter and
also drive queries containing only optional terms.

### Acceptance tests

- Arbitrary read/write combinations work without modifying `GameHost`.
- Duplicate or contradictory component terms fail during discovery.
- Scheduler access exactly matches every query term.
- Query rows remain stack-only and cannot retain native spans.

---

## 5. Scheduler rebuild during hot reload

**Priority:** High

### Problem

Managed behavior can reload, but changing a method name or query signature is
rejected because the Rust scheduler graph was built only at startup.

### Implementation

1. Split managed reload into prepare and commit phases.
2. Prepare a new collectible context and export its complete system manifest.
3. At a frame boundary:
   - stop scheduling old managed systems;
   - validate all new component and system metadata;
   - clear only managed system registrations;
   - register new closures and rebuild the execution graph;
   - commit the new managed context.
4. Keep the old context active if any validation or registration step fails.
5. Assign stable system IDs so unchanged systems preserve enabled/disabled
   state and timing information.

### Acceptance tests

- Changing `Read<T>` to `Write<T>` rebuilds the scheduler without restart.
- Adding and removing a system works at the next frame boundary.
- A failed reload leaves the old systems running.
- No closure retains a function pointer into an unloaded context.

---

## 6. Stable component identity

**Priority:** High

### Problem

The current key is an FNV-1a hash of `Type.Name`. `Foo.Position` and
`Bar.Position` collide by construction, and ordinary hash collisions are not
detected.

### Implementation

1. Introduce an explicit attribute:

   ```csharp
   [EcsComponent("game.physics.position", Version = 1)]
   public struct Position { ... }
   ```

2. Derive a 128-bit ID from the explicit canonical name, or store the canonical
   string in the native registry and use an assigned runtime integer afterward.
3. Reserve canonical IDs for shared native components.
4. Detect duplicate IDs during manifest loading and report both managed types.
5. Do not use assembly version or collectible load-context identity in the ID;
   an unchanged component must keep its identity across hot reload.

### Acceptance tests

- Equal short names in different namespaces register independently.
- Duplicate explicit IDs fail deterministically.
- IDs remain stable across rebuilds and process architectures.

---

## 7. Complete ABI and schema validation

**Priority:** High

### Problem

Only `sizeof(T)` is checked today. Equal-sized structs can still disagree on
alignment, offsets, primitive types, signedness, or nested layout.

### Implementation

1. Generate a schema descriptor from managed reflection using
   `Marshal.OffsetOf`, `Unsafe.SizeOf`, field types, and explicit packing.
2. Generate the equivalent descriptor for shared Rust components.
3. Hash canonical field metadata into a schema hash.
4. Validate size, alignment, field count, offsets, primitive kinds, nesting,
   and schema hash during startup.
5. Restrict components to blittable unmanaged fields. Require explicit byte
   representation for booleans and enums unless their representation is fixed.
6. Add `#[repr(C)]` to every native type shared with managed code.

### Acceptance tests

- Reordered equal-sized fields are rejected.
- Incorrect enum/boolean representation is rejected.
- Nested `Color` inside `Sprite` validates on x64 and supported platforms.

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

### Problem

Managed rows expose component values but not the entity owning the row.

### Implementation

1. Extend `NativeComponentChunk` with the archetype's entity pointer.
2. Add an ABI-stable managed `Entity` struct.
3. Expose entity access as a query term or as a property common to every row.
4. Treat entity access as metadata, not a component read/write.

### Acceptance tests

- Each managed row returns the same entity ID as the corresponding Rust query.
- Entity IDs remain aligned with component rows after swap-remove and migration.

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

The collectible context explicitly resolves only `cs_runtime`. Gameplay
assemblies with additional NuGet or project dependencies may fail to load or
may bind accidentally to the default context.

### Implementation

1. Construct an `AssemblyDependencyResolver` from the game assembly path.
2. Resolve managed and unmanaged dependencies inside `GameContext`.
3. Continue returning the already-loaded `cs_runtime` assembly for its shared
   contract types.
4. Define which dependencies are shared and which are collectible.
5. Report unresolved dependency names and searched locations.

### Acceptance tests

- A game can reference a secondary managed project and a NuGet library.
- Reload unloads collectible dependencies with the game context.
- `cs_runtime` is never loaded twice into incompatible contexts.

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

### Problem

Current tests cover access reflection and a renderer query, but not the complete
native/managed lifecycle or parallel execution behavior.

### Implementation

Add automated scenarios for:

- hostfxr startup and managed export resolution;
- component manifest registration and layout rejection;
- Rust/C# read-read parallelism and read-write exclusion;
- managed writes observed by `Changed<T>`;
- commands and entity migration;
- behavior-only reload;
- scheduler-signature reload;
- failed reload rollback;
- exception propagation;
- collectible-context unloading;
- dependency and PDB loading;
- repeated startup/shutdown and multiple frame execution.

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

The first useful milestone should deliver these together:

1. Correct managed change ticks.
2. Stable component IDs and full layout validation.
3. A game-exported component/system manifest.
4. Dynamic native storage for unmanaged C# components.
5. Managed startup plus deferred commands.
6. Removal of the demo-specific `setup_world()` implementation.

At that point, a new C# component and its initial entities can be added without
editing or recompiling the Rust host. That is the point where C# becomes a true
game scripting backend rather than a managed behavior layer over a Rust-defined
demo world.
