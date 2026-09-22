# Shared and persistable types in Pill Engine

Pill Engine uses **shared identity** to let separate binaries agree on which
ECS data they access. It uses **persistence** to preserve and migrate data
across reloads. These are independent properties.

The API spelling is `persistable`: a type supports the engine's persistence
machinery. It does not mean that every instance is automatically saved to disk.

| Property | Question it answers |
| --- | --- |
| Shared | Do the project and extension DLLs refer to the same component or resource? |
| Persistable | Can the engine serialize and restore this data when its definition changes? |

This guide describes the current implementation. Suggested default policies
near the end are recommendations, not existing macro behavior.

## Why shared identity exists

Normally, native components and resources are identified using Rust's `TypeId`.
A project can link an extension as a Rust dependency while the host loads that
extension as a separate DLL. Different compilation inputs can give the two
copies of a type different `TypeId` values, despite their matching source code.

For example, without sharing:

```text
Project DLL creates AudioSourceComponent under identity A.
Audio DLL queries AudioSourceComponent under identity B.
The audio query cannot see the project's source.
```

A shared type declares a stable, namespaced name. The engine derives a 128-bit
identity from that name instead of using the artifact's native `TypeId`.
Both copies therefore resolve to the same component storage or resource slot.

There is no synchronization copy between DLLs: they access the same data in
the world. Ordinary ECS access rules and scheduler conflict tracking still apply.
Shared identity does not itself introduce locks or make arbitrary concurrent
access safe. It also does not change Rust's own `TypeId`; it changes the
identity used by the engine.

## Shared components

Use the component derive's `shared` attribute:

```rust
use pill_engine::PillComponent;

#[repr(C)]
#[derive(PillComponent)]
#[pill(shared)]
pub struct AudioLevel {
    pub volume: f32,
}
```

The default shared name is based on `module_path!()` and the type name. A type
can explicitly preserve an old identity when moving between modules:

```rust
#[pill(shared = "my_extension::AudioLevel")]
```

Use a unique, namespaced name. Reusing another type's name is a conflicting
claim, not a way to convert between types. Moving a type without preserving
its identity can make it appear to be a different component. An explicit
shared name is not, by itself, a general persistence migration for renames.

## Shared resources

A resource supplies its stable name through the `Resource` trait:

```rust
use pill_engine::Resource;

#[repr(C)]
pub struct AudioSettings {
    pub volume: f32,
}

impl Resource for AudioSettings {
    fn shared_name() -> Option<&'static str> {
        Some("my_extension::AudioSettings")
    }
}
```

The project and extension can then retrieve the same resource using their own
compiled copies of `AudioSettings`. The engine uses `ResourceId::Shared` for
this lookup.

Sharing does not decide who creates, resets, or destroys a resource. Give it
an owner and an initialization policy. Replacing the resource on every project
initialization can still reset state, even though its identity is shared.

## What the compatibility checks guarantee

Matching identity is necessary, but both binaries must also agree on the
meaning and layout of the stored bytes.

The engine checks shared registration claims and layout compatibility.
Component declarations produced by `PillComponent` supply field descriptors
used for structural checks. Hand-registered shared components without those
descriptors have weaker checks based on size and alignment.

Resources can provide `Resource::shared_schema_hash()`. The default is `None`;
the structural hash is compared when both declarations supply one. Size and
alignment alone cannot distinguish two floats from two integers of the same size.

These checks are not a universal Rust ABI guarantee. In particular:

- `#[repr(C)]` on an outer struct does not make nested Rust containers or trait
  objects a stable ABI across arbitrary compiler versions or dependency builds.
- Shared identity does not keep a DLL mapped while callbacks or destructors
  still point into it; that remains a lifecycle responsibility.
- Two simultaneously active declarations cannot safely disagree about a shared
  layout. Persistence does not waive that requirement.

Use shared types within the engine's coordinated build and reload model.
Prefer explicit, inspectable data at artifact boundaries.

## Persistable components

Mark a component as persistable and implement the required serialization traits:

```rust
use pill_engine::PillComponent;
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Default, Serialize, Deserialize, PillComponent)]
#[pill(persistable, shared)]
pub struct Health {
    pub current: f32,
    pub maximum: f32,
}
```

Persistence requires `Serialize`, `DeserializeOwned`, and `Default` in addition
to the component implementation. The derive registers serialization,
deserialization, insertion, and schema metadata with the world.

The current component reload mechanism performs selective migration:

1. Capture registration metadata for the retiring generation.
2. Register the incoming generation and compare schemas.
3. For changed component types, serialize the old rows through the retiring
   generation's functions and rebuild them through the incoming generation.
4. Leave unchanged component data in place.

The migration representation uses JSON with field names. The engine uses the
new type's defaults when adding fields and ignores removed fields. Arbitrary
field-type changes or semantic changes are not automatically meaningful
conversions; incompatible data can fail migration.

Persistence is not per-frame serialization. It also does not promise that
every non-persistable component is destroyed on every reload: unchanged data
and schema-changing migration are different paths.

## Persistable resources

The current engine also supports resource persistence through explicit registration:

```rust
use pill_engine::{Resource, World};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct GameProgress {
    score: u32,
}

impl Resource for GameProgress {}

fn register_progress(world: &mut World) {
    world.register_persistable_resource::<GameProgress>();
    if world.get_resource::<GameProgress>().is_none() {
        world.insert_resource(GameProgress::default());
    }
}
```

Resource persistence likewise requires `Serialize + DeserializeOwned + Default`.
The implementation provides resource snapshots, restoration, and migration of
changed persistable resources. Registration and inserting a value are separate
operations. A resource can additionally declare a shared name; persistence
registration alone does not make it shared.

Keep logical state separate from runtime machinery. A score or audio volume is
suitable persistent data. An output stream, GPU handle, thread, or callback
usually needs explicit reconstruction and cleanup instead of serialization.

## The audio example

The native example enables `pill_audio` in `project_settings.yaml`. The host
initializes that extension before the project. The project does not directly
register the playback system.

```text
Host initializes pill_audio
    -> extension creates shared AudioLoadQueue and registers playback system
Project initializes its scene
    -> enqueues embedded MP3 bytes
    -> creates shared source and listener components
Audio system runs
    -> drains the queue and creates Sound assets in the audio artifact
    -> reads the source/listener components and starts playback
```

`AudioSourceComponent` and `AudioListenerComponent` are shared and persistable.
`AudioLoadQueue` is a shared resource; it is not registered as persistable.
It carries transient loading requests, not saved gameplay state.

`Sound` is an asset. The asset store currently keys asset types using ordinary
`TypeId`, so the queue lets the playback artifact create the assets it will
later resolve. Component/resource sharing does not automatically extend to assets.

The queue currently has no explicit resource schema hash. Its Rust containers
depend on compatible builds; it should not be treated as a general-purpose
foreign-language or version-independent ABI.

## Costs and default policy

| Mechanism | Normal execution | Registration and reload |
| --- | --- | --- |
| Shared component | The derive emits a constant identity; no per-entity name hashing or data copying | Shared-name and layout checks, additional metadata |
| Shared resource | Stable-identity lookup and resource access checks; the default identity implementation hashes the name unless optimized | Shared claims and layout/schema checks |
| Persistable component/resource | No automatic serialization on every frame | Extra registration metadata and generated code; serialization, allocation, and reconstruction when migration or snapshots require them |

The component macro explicitly computes shared identity at compile time.
A future resource derive should do the same rather than relying on optimization
of the default trait method. Actual timing and memory differences require
benchmarks; these observations describe the implementation, not measurements.

Shared and persistable defaults can simplify gameplay declarations, but retain
independent opt-outs:

- Shared + persistable suits gameplay data used across DLLs and reloads.
- Shared only suits transient communication such as an audio loading queue.
- Persistable only suits state owned by one artifact that still needs migration.
- Neither suits private, reconstructible implementation details.

Making every resource shared and persistable would turn private implementation
details into cross-artifact contracts and force serialization onto objects that
do not support it. Prefer defaults for well-defined categories of data rather
than a universal rule.

## Implementation references

- [Component identity and shared registration](../modules/pill_engine/src/component.rs)
- [Resource identity and compatibility](../modules/pill_engine/src/resource.rs)
- [Component derive implementation](../modules/pill_engine_macros/src/lib.rs)
- [Persistence and selective migration](../modules/pill_engine/src/persistence/mod.rs)
- [Resource persistence](../modules/pill_engine/src/persistence/resources.rs)
- [Audio loading queue](../modules/extensions/pill_audio/src/audio_load_queue.rs)
- [Example audio scene](../examples/project_rs/src/audio_scene.rs)
