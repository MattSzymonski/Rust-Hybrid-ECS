# C# Component Architecture: Previous and Current Approach

This document explains how C# components cross the managed/native boundary,
why the original implementation was hardcoded, what was changed, and how the
current component, query, storage, scheduler, and change-detection paths work.

The most important result is:

> A component defined in `game_cs` can now be discovered, validated,
> registered, stored, queried, moved between archetypes, and change-tracked
> without declaring that component as a Rust type or adding a match arm to the
> host.

There are now two intentional component categories:

| Category | Type owner | Storage owner | Example |
|---|---|---|---|
| Shared native component | Rust engine, with a canonical C# mirror in `cs_runtime` | Typed Rust storage | `Position`, `Sprite` |
| Dynamic game component | `game_cs` | Type-erased Rust storage described by the managed manifest | `PhysicsState`, `BallTag` |

`Color` is also mirrored in `cs_runtime`, but it is currently a nested field of
`Sprite`, not an independently registered ECS component.

---

## 1. The previous setup

Previously, the C# and Rust sides both declared each gameplay component.

The C# game contained declarations similar to:

```csharp
[StructLayout(LayoutKind.Sequential)]
public struct Position { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct PhysicsState
{
    public float DeltaTime;
    public float PositionX;
    // ...
}
```

The host then needed corresponding concrete Rust types:

```rust
#[repr(C)]
struct PhysicsState {
    delta_time: f32,
    position_x: f32,
    // ...
}

impl Component for PhysicsState {}
```

This duplication was only the first hardcoded layer. The bridge also had to:

1. Register every concrete Rust component type.
2. Convert a managed component key to a Rust `ComponentId` with explicit match
   arms.
3. Select a concrete generic native chunk callback with another explicit
   match.
4. Update the host whenever a new C# component was introduced.

Conceptually, the old dispatch looked like this:

```rust
fn component_id(key: u64) -> Option<ComponentId> {
    match key {
        POSITION_KEY => Some(ComponentId::of::<Position>()),
        SPRITE_KEY => Some(ComponentId::of::<Sprite>()),
        PHYSICS_KEY => Some(ComponentId::of::<PhysicsState>()),
        _ => None,
    }
}
```

The query callback had a similar per-component match to call
`get_component_chunk::<Position>`, `get_component_chunk::<Sprite>`, or
`get_component_chunk::<PhysicsState>`.

### Why that approach was limiting

- `game_cs` could not independently define the component universe.
- Adding one unmanaged C# struct required editing and recompiling Rust.
- The host had gameplay-specific knowledge.
- A layout could be accidentally changed on one side without a complete field
  comparison.
- The old identity was a single FNV-1a hash of `Type.Name`. Types such as
  `Foo.Position` and `Bar.Position` therefore had the same hash input.
- Rust archetype storage could only allocate a column when it knew the concrete
  Rust type at compile time.

The old setup was workable for a fixed demo, but it was not a scripting
pipeline.

---

## 2. The current setup at a glance

The current startup flow is:

```text
game_cs.dll
  |
  | reflection finds [EcsSystem] methods and their Query<...> terms
  v
cs_runtime
  |
  | builds a UTF-8 JSON component manifest
  | exports system access declarations
  v
Rust host
  |
  | validates the manifest
  | binds shared native components
  | registers game components dynamically
  | runs [EcsStartup] with the native CommandQueue
  | converts managed access declarations to SystemAccess
  v
ECS engine
  |
  | creates typed or type-erased archetype columns
  | schedules systems using the same ComponentId values
  v
scheduled C# system
  |
  | borrows native columns temporarily
  | reads/writes rows through stack-only query views
  v
Rust-owned component and tick storage
```

The ordering matters. Components are registered before systems are added to
the scheduler and before the first frame runs.

The main implementation files are:

- [`cs_runtime/src/ComponentManifest.cs`](cs_runtime/src/ComponentManifest.cs)
- [`cs_runtime/src/SharedComponents.cs`](cs_runtime/src/SharedComponents.cs)
- [`cs_runtime/src/Engine.cs`](cs_runtime/src/Engine.cs)
- [`cs_runtime/src/GameHost.cs`](cs_runtime/src/GameHost.cs)
- [`cs_runtime/src/LoaderInterop.cs`](cs_runtime/src/LoaderInterop.cs)
- [`host/src/cs/cs_api.rs`](host/src/cs/cs_api.rs)
- [`engine/src/component.rs`](engine/src/component.rs)
- [`engine/src/archetype.rs`](engine/src/archetype.rs)
- [`engine/src/world.rs`](engine/src/world.rs)

---

## 3. Component identity

Managed and native code need an identity that is stable across collectible
assembly load contexts. A CLR `Type` object or Rust `TypeId` cannot be sent
across this boundary for a game-owned type.

The current managed identity is a 128-bit value derived from the component's
full managed name:

```text
canonical name = Type.FullName, falling back to Type.Name
low  = FNV-1a-64(canonical name, seed 0xcbf29ce484222325)
high = FNV-1a-64(canonical name, seed 0x84222325cbf29ce4)
id   = (high << 64) | low
```

For example, the hash input is `TracyLive.PhysicsState`, not merely
`PhysicsState`.

Both halves travel through every relevant ABI structure:

```csharp
public readonly record struct QueryTermDescriptor(
    Type? ComponentType,
    ulong ComponentKey,
    ulong ComponentKeyHigh,
    int ComponentSize,
    QueryAccess Access,
    bool Optional,
    bool IsEntity);
```

On the Rust side they are recombined into a `u128`.

The host recomputes the ID from `full_name` when loading the manifest. It also
rejects duplicate IDs. The world rejects re-registering an existing dynamic ID
with a different name or schema.

### Identity consequences

- Rebuilding the same full type name produces the same ID.
- A collectible `AssemblyLoadContext` does not affect the ID.
- Assembly version does not affect the ID.
- Two equal short names in different namespaces get different inputs.
- Renaming the namespace or type changes the ID.
- The current two-seed FNV construction is stable and collision-resistant for
  this registry, but is not a cryptographic identity scheme.

An explicit canonical-name attribute could later preserve identity across C#
type renames. It is not implemented yet.

---

## 4. How `cs_runtime` discovers components

`GameHost` reflects all static methods marked with `[EcsSystem]`. A method must
currently:

- return `void`;
- have exactly one parameter;
- use a supported `Query<...>` type;
- compose that query from `Read<T>`, `Write<T>`, `OptionalRead<T>`,
  `OptionalWrite<T>`, and/or `EntityTerm`.

For this system:

```csharp
[EcsSystem]
public static void Run(
    Query<EntityTerm, Write<PhysicsState>, Write<Position>, Write<Sprite>> query)
```

the query descriptor contains four ordered terms. `EntityTerm` has no component
ID and is excluded from scheduler component access. The remaining terms report
write access for the three components.

`ComponentManifestBuilder.Build` then:

1. Reads the query descriptor from every discovered system.
2. Removes `EntityTerm` entries.
3. Collects the referenced component `Type` objects.
4. Removes duplicate types.
5. Validates and describes each layout.
6. Sorts descriptors by their 128-bit identity for deterministic output.
7. Serializes the descriptors as UTF-8 JSON.

This means a game component is registered when it appears in at least one
discovered system query. Merely declaring an unused struct in `game_cs` does
not currently put it in the manifest.

---

## 5. The component manifest

Each top-level manifest entry contains:

```text
stable_id_low
stable_id_high
full_name
size
alignment
schema_hash
shared
fields[]
```

Each field contains:

```text
name
offset
size
primitive_type
fields[]            # recursive nested-struct schema
```

### Size and alignment

`Marshal.SizeOf(type)` supplies the managed storage size.

Alignment is measured with a sequential probe:

```csharp
[StructLayout(LayoutKind.Sequential)]
private struct AlignmentProbe<T> where T : unmanaged
{
    public byte Prefix;
    public T Value;
}
```

The offset of `Value` is the alignment required for `T` in that managed
layout.

### Field schema

Fields are ordered by `Marshal.OffsetOf`. Primitive entries record their CLR
primitive type name, so signedness and numeric kind are part of the schema.
Nested structs recursively record their own fields.

For `Position`, the canonical schema text is equivalent to:

```text
TracyLive.Position|8|4
|X@0:4:System.Single
|Y@4:4:System.Single
```

The actual text has no line breaks. Its FNV-1a-64 hash becomes `schema_hash`.

### Managed layout restrictions

Manifest discovery rejects a component before the first frame if it:

- is not a value type;
- uses automatic layout;
- recursively contains itself;
- contains `bool` or `char`;
- contains a managed reference;
- contains a pointer or by-reference field.

Enums are represented by their fixed underlying primitive type. Nested
unmanaged value types are supported and recursively described.

`bool` is deliberately rejected because its native representation is easy to
misunderstand across ABI boundaries. Use `byte` for an explicit one-byte flag,
as `PhysicsState.Active` does.

---

## 6. Shared native components

Some components already exist as native engine types and must not receive a
second dynamic storage column. The renderer's `Position` and `Sprite` are the
current examples.

Their canonical C# mirrors live in
[`cs_runtime/src/SharedComponents.cs`](cs_runtime/src/SharedComponents.cs):

```csharp
[StructLayout(LayoutKind.Sequential)]
public struct Position
{
    public float X;
    public float Y;
}
```

The gameplay assembly references these definitions from `cs_runtime` instead
of redeclaring them in `game_cs`. This gives every C# game one authoritative
managed definition.

The manifest marks a component as shared when its type comes from the
`cs_runtime` assembly:

```csharp
type.Assembly == typeof(Engine).Assembly
```

The host's explicit shared registry maps the managed identity to the concrete
Rust component:

```text
TracyLive.Position -> native Position ComponentId and typed chunk callback
TracyLive.Sprite   -> native Sprite ComponentId and typed chunk callback
```

Shared registration compares:

- total size;
- alignment;
- the full canonical field-schema hash.

Therefore an equal-sized but reordered or differently typed C# mirror is
rejected.

The shared registry is intentionally explicit. Adding a new engine-owned
component requires adding its canonical mirror to `cs_runtime` and its native
binding to the host. Adding a game-owned component does not.

In a rendering build, the host binds the actual renderer types. In a headless
build, it supplies layout-identical local `Position` and `Sprite` mirrors so
the C# pipeline and its tests remain usable without the renderer feature.

---

## 7. Game-owned dynamic components

`PhysicsState` and `BallTag` are currently declared only in
[`game_cs/src/Components.cs`](game_cs/src/Components.cs). There are no concrete
Rust declarations for these types.

When the host sees a manifest entry that has no shared native binding and is
not marked `shared`, it calls:

```rust
world.register_dynamic_component(
    stable_id,
    full_name,
    size,
    alignment,
    schema_hash,
)
```

The returned ID is:

```rust
ComponentId::Dynamic(stable_id)
```

Native Rust components use:

```rust
ComponentId::Native(TypeId)
```

Both variants can coexist in the same component registry, archetype component
list, component mask, change-tick map, and scheduler access set.

The registry still assigns each component one bit in the engine's `u128`
component mask. Consequently, the current world supports at most 128 total
registered component types, counting native and dynamic types together.

---

## 8. Type-erased archetype storage

Before this change, creating a component column required a concrete Rust type
and a typed `Vec<T>` registered in `TraitTypeMap`.

The storage factory is now an enum:

```rust
pub enum StorageFactory {
    Native(typed_factory),
    Dynamic(DynamicComponentLayout),
}
```

Each archetype has both storage maps:

```rust
component_storages: TraitTypeMap<dyn Component, VecFamily>
dynamic_component_storages: HashMap<ComponentId, DynamicColumn>
```

Native types continue using their existing typed storage. Dynamic components
use `DynamicColumn`.

### `DynamicColumn`

A dynamic column owns:

```text
layout: size, alignment, schema hash
data: aligned allocation pointer
len: initialized row count
capacity: allocated row capacity
```

Rows are laid out densely:

```text
data + row_index * component_size
```

This is still structure-of-arrays storage. A `PhysicsState` query walks one
contiguous `PhysicsState` byte column, just as a native typed query walks a
contiguous `Vec<PhysicsState>`.

The column supports:

- aligned allocation and growth;
- appending zero-initialized rows;
- appending validated raw bytes;
- copying a row to another archetype column;
- replacing one row's bytes;
- exposing one row as bytes for optional serialization/tooling;
- swap-removing a row;
- releasing its allocation on drop.

No destructor callback is needed because managed component validation permits
only unmanaged data. Dynamic storage must never contain a CLR object reference.

### Tick storage remains separate

Every archetype also has:

```rust
component_ticks: HashMap<ComponentId, Vec<ComponentTicks>>
```

For every native or dynamic component column:

```text
component row i <-> ComponentTicks row i
```

Entity insertion, migration, and swap-removal keep these arrays in lockstep.

---

## 9. Entity creation and archetype migration

The world now exposes type-erased operations for dynamic components:

```rust
create_dynamic_entity(...)
add_dynamic_component(...)
add_dynamic_component_default(...)
remove_dynamic_component(...)
dynamic_component_bytes(...)
```

`add_dynamic_component_default` uses the registered size and creates an
all-zero component value. This is the dynamic equivalent of default
construction for the current unmanaged component model.

### Moving between archetypes

Adding or removing a component changes an entity's component set, so the entity
must move to another archetype.

During `move_entity_to_archetype`:

1. The destination archetype is found or created from the new component set.
2. Native components are copied with their registered typed copier functions.
3. Every dynamic destination column is handled generically:
   - copy the old row if the old archetype contained that component;
   - otherwise append a zero-initialized row.
4. Existing component ticks are preserved.
5. Newly attached components receive fresh added/changed ticks.
6. The entity location is updated.
7. The old entity, native rows, dynamic rows, and tick rows are swap-removed.
8. If another entity was swapped into the removed row, its location is fixed.

No dynamic component-specific branch is involved in this migration.

---

## 10. Host registration and binding table

The host maintains:

```rust
HashMap<StableComponentId, ComponentBinding>
```

A binding is one of:

```text
Native
  - engine ComponentId
  - generic typed chunk callback
  - size
  - alignment
  - expected shared schema hash

Dynamic
  - engine ComponentId::Dynamic
  - size
  - alignment
```

Manifest registration performs these checks before systems run:

1. JSON must deserialize successfully.
2. The 128-bit ID must match the declared full name.
3. IDs must be unique within the manifest.
4. Size must be nonzero and fit the chunk ABI's `u32` size.
5. Alignment must be a nonzero power of two.
6. Size and alignment must form a valid native allocation layout.
7. Every field range must fit inside its parent layout.
8. A shared entry must have a native shared binding.
9. Shared size, alignment, and schema must match the native definition.
10. Non-shared entries are registered as dynamic components.

For a game-owned dynamic type, the managed manifest is the authoritative
schema because there is intentionally no duplicate Rust struct to compare
against. The engine stores its schema hash so incompatible re-registration is
rejected.

---

## 11. Scheduler access

System access is derived from the same query descriptor used for iteration.
There is no second manual access list in game code.

For example:

```csharp
Query<EntityTerm, Write<PhysicsState>, Read<Position>, OptionalWrite<BallTag>>
```

exports:

```text
PhysicsState: write
Position:     read
BallTag:      write
```

`EntityTerm` is not a component access. Optionality changes archetype matching,
but it does not weaken scheduler safety: an `OptionalWrite<BallTag>` still
conflicts with any reader or writer of `BallTag` because that component may be
present.

The host resolves every 128-bit managed identity through the binding table and
adds the resulting native or dynamic `ComponentId` to `SystemAccess`.

The ordinary Rust scheduler then applies its existing rules:

- read/read of the same component may run in parallel;
- read/write conflicts;
- write/write conflicts;
- disjoint component sets may run in parallel;
- native and dynamic IDs are treated identically by conflict detection.

The scheduler therefore knows that a C# system writes `PhysicsState` even
though Rust has no `PhysicsState` type.

---

## 12. Runtime query execution

When the scheduler invokes a C# system, `ActiveSystemGuard` temporarily
publishes three thread-local values:

```text
active World pointer
that system's declared access array
the component binding table
```

They exist only for the scheduled call and are cleared by the guard's `Drop`
implementation.

The managed iterator requests chunks through `ffi_get_component_chunk` using:

```text
stable ID low
stable ID high
requested mode
chunk index
output pointer
```

The callback first verifies that:

- a managed system is currently active;
- the active system declared that component;
- the requested read/write mode is authorized.

It then uses the binding table:

- `Native` calls the stored generic typed chunk function.
- `Dynamic` obtains the raw `DynamicColumn` pointer and its tick column.

There is no `Position`/`Sprite`/`PhysicsState` match in the query callback.

### Returned chunk ABI

Each chunk reports:

```text
archetype ID low/high
component data pointer
row count
element size
parallel ComponentTicks pointer
current world change tick
```

The C# query enumerator chooses one required term as its driver, enumerates its
archetype chunks, and finds matching chunks for the other terms by archetype
ID. Required missing terms reject the archetype; optional missing terms produce
an absent optional view. All present columns must have the same row count.

`QueryRow`, `QueryEnumerator`, and optional references are `ref struct` values.
They cannot be boxed or retained on the managed heap, which helps keep native
pointer lifetimes inside the scheduled call.

The current join searches chunk lists term by term. It is correct, but a future
single native query cursor could reduce repeated chunk lookup overhead.

---

## 13. Managed write tracking

Dynamic registration uses the same write-tracking mechanism as shared native
components.

For a required write:

```csharp
ref PhysicsState physics = ref row.Write<PhysicsState>();
```

`Write<T>()` marks that row's `ComponentTicks.changed` value with the world
change tick before returning the writable reference.

For an optional write, checking `HasValue` does not mark anything. Accessing
the `.Value` reference marks the present row.

Read-only access never updates change ticks.

This is conservative in the same way as Rust's mutable component wrapper:
requesting a mutable reference counts as a change even if the caller ultimately
assigns the same value or performs no assignment.

Rust cannot write a compile-time `Changed<GameOwnedType>` query because that
type has no Rust definition. However, the underlying dynamic tick column is
maintained correctly and is available to managed queries and type-erased engine
code.

---

## 14. How the current bouncing-ball game uses this

The current ownership is:

```text
cs_runtime
  Position       shared native mirror
  Color          nested shared ABI type
  Sprite         shared native mirror

game_cs
  PhysicsState   dynamic game component
  BallTag        dynamic game component

Rust engine/host
  Position       concrete renderer component
  Sprite         concrete renderer component
  PhysicsState   no Rust type
  BallTag        no Rust type
```

After registration, the host invokes every `[EcsStartup]` method inside a
command-enabled native scope. `GameStartup.Start(Commands commands)` reserves
100 generation-checked entity handles and describes each ball with exactly
`PhysicsState`, `Position`, `Sprite`, and `BallTag`. Component blobs are copied
into the Rust `CommandQueue`; the host flushes that queue before the first
frame, so no partially built entity is observable.

Native shared blobs are decoded into concrete renderer components. Game-owned
blobs remain type-erased and populate `DynamicColumn` rows. The same mixed
creation command therefore builds one archetype without a host match arm for
`PhysicsState` or `BallTag`. `BallPhysicsSystem` receives fully initialized
state and no longer uses zero radius as an initialization sentinel.

---

## 15. Adding a new game-owned component

Create the component only in `game_cs`:

```csharp
using System.Runtime.InteropServices;

namespace TracyLive;

[StructLayout(LayoutKind.Sequential)]
public struct Lifetime
{
    public float RemainingSeconds;
}
```

Use it in a discovered system:

```csharp
public static class LifetimeSystem
{
    [EcsSystem]
    public static void Run(Query<Write<Lifetime>, Read<Position>> query)
    {
        foreach (var row in query)
        {
            ref var lifetime = ref row.Write<Lifetime>();
            ref readonly var position = ref row.Read<Position>();

            lifetime.RemainingSeconds -= 0.016f;
            _ = position.X;
        }
    }
}
```

On a fresh host start:

1. Reflection sees `Lifetime` in the query.
2. The manifest describes its ID and one `float` field.
3. Rust validates and registers `ComponentId::Dynamic`.
4. Archetypes allocate aligned type-erased `Lifetime` columns when needed.
5. The scheduler records a write to `Lifetime` and a read of native `Position`.
6. The C# iterator borrows both columns in one joined row.

No changes are needed in:

- `engine` Rust component declarations;
- the host's shared registry;
- the native query callback;
- scheduler match arms.

Because adding `Lifetime` changes both the component manifest and scheduler
signature, restart the host after adding it. The current behavior-only hot
reload path intentionally rejects this structural change.

---

## 16. Adding a new shared engine component

A shared component is different because an existing native engine subsystem
owns its typed Rust column.

For a new engine-owned component, intentionally do all of the following:

1. Define the native component with a stable C-compatible layout.
2. Add the canonical C# mirror to `cs_runtime`, not `game_cs`.
3. Add an explicit entry to `shared_component_bindings` in
   `host/src/cs/cs_api.rs`.
4. Provide the expected canonical schema used for startup validation.
5. Add ABI layout tests.

This explicit work prevents C# from silently inventing a second component that
looks like an engine component but is not the column consumed by the renderer
or another native subsystem.

---

## 17. Hot reload behavior

The component registry and scheduler graph are currently built at process
startup.

During a managed reload, `GameHost` builds the new system metadata and manifest
before replacing the active collectible context. It compares:

- ordered system names and access signatures;
- the complete serialized component manifest.

If either differs, reload is rejected and the old context remains active. A
restart is required.

This means the current hot reload supports method-body/gameplay changes that
preserve component layouts and query signatures. It does not yet support:

- adding or removing a component;
- changing fields, size, alignment, or identity;
- changing a system's read/write declaration;
- adding or removing a system;
- migrating existing dynamic storage to a new schema.

Rejecting these changes is necessary because existing archetype allocations
were created for the old size and alignment.

---

## 18. Tests that protect the approach

The implementation is covered at three levels.

### Engine tests

The world tests verify that:

- two dynamic components coexist in one archetype;
- values survive adding another component and moving archetypes;
- values survive removing a component and moving again;
- default construction produces zero bytes;
- invalid size and alignment are rejected;
- the same ID cannot be registered with a different name or schema;
- an invalid component byte length does not create an entity.

### Host tests

The host tests verify that:

- a manifest-only component registers and can be queried through the FFI;
- an equal-sized but field-incompatible shared mirror is rejected;
- managed read/write declarations produce the correct scheduler batches;
- read/read can run together;
- read/write and write/write are separated;
- disjoint dynamic/native accesses can run together;
- managed writes update the correct native tick rows;
- read-only access does not mark changes.

### Managed tests

The C# tests verify that:

- the real game discovers systems using shared and dynamic components;
- the manifest marks `Position` and `Sprite` shared;
- the manifest marks `PhysicsState` and `BallTag` dynamic;
- ABI sizes and offsets are correct;
- query access uses both halves of the 128-bit ID;
- duplicate or contradictory terms are rejected;
- query rows and enumerators are stack-only;
- invalid managed layouts are rejected during manifest construction.

The standalone C# game has also been run through actual frames with 100
entities, exercising mixed native/dynamic archetypes and managed queries end to
end.

---

## 19. Current limitations and next steps

The dynamic component foundation is complete for the current query pipeline,
but these limitations remain:

1. **Registration scans supported game structs.** Query component types are
   always included, and supported unmanaged structs declared in `game_cs` are
   also exported so command-only components can be registered before startup.
   This can register an otherwise unused game struct as a component.
2. **The world has a 128-component limit.** This comes from the existing
   `u128` component mask, not from C# interop.
3. **Components must be unmanaged.** Managed references and variable-lifetime
   values cannot live in raw Rust archetype memory.
4. **Structural hot reload requires a restart.** There is no schema migration
   yet.
5. **Dynamic persistence is not integrated with snapshots yet.** Dynamic rows
   can be exposed as raw bytes, but there is no managed versioned
   serializer/deserializer callback or snapshot registration yet.
6. **Rust typed queries cannot name game-only types.** Native systems need a
   type-erased query/filter API to consume arbitrary managed component schemas.
7. **Chunk joining is performed in managed code.** A native query cursor could
   reduce repeated archetype lookup work.
8. **Identity follows the managed full name.** An explicit canonical identity
    attribute would make namespace/type renames migration-friendly.

The central architectural boundary is nevertheless in place: shared engine
types remain explicit and strongly validated, while game-owned C# types are
described by a manifest and handled generically throughout Rust storage,
scheduling, iteration, migration, and change tracking.
