# C# Access to Rust Assets: Implementation Plan

Remove renderer- and audio-specific code from the C# bridge and runtime, and
generate a C# API for extension asset types that reads like the Rust one:

```csharp
[EcsStartup]
public static void LoadAssets(ResMut<AssetManager> assets)
{
    byte[] obj = AssetLoader.Path("models/chimpanzini_bananini.obj").Load();
    Handle<Mesh> mesh = assets.AddNamed(
        "italian_brainrot.mesh",
        Mesh.FromObjBytes("chimpanzini_bananini", obj));
    Handle<Texture> color = assets.AddNamed(
        "italian_brainrot.color",
        Texture.New("chimpanzini_bananini", TextureType.Color,
                    AssetLoader.Path("textures/chimpanzini_bananini_color.png")));
    Handle<Shader> unlit = assets.AddNamed(
        "italian_brainrot.shader.unlit",
        Shader.New("italian_brainrot_unlit")
            .WithVertexSource(AssetLoader.Path("shaders/default_vertex.wgsl"))
            .WithFragmentSource(AssetLoader.Path("shaders/unlit_fragment.wgsl"))
            .WithParameterSlots([new ShaderParameterSlot("tint", ShaderParameterType.Color)])
            .WithTextureSlots([new ShaderTextureSlot("color", TextureType.Color, (0, 1))])
            .WithEngineParameters(true)
            .WithCameraParameters(true)
            .Build());
}
```

The C# for `Mesh`, `Texture`, `Shader` and the rest is generated from
annotations on the Rust types, the same way component mirrors are today. A
Rust error (`?`) becomes a C# exception carrying its message.

Companion plan: [csharp_asset_support_plan.md](csharp_asset_support_plan.md)
(C#-declared asset types). Both plans use the same `Handle<T>`,
`AssetManager` and `ResMut<AssetManager>` on the C# side; whichever lands
first builds them.

## Implementation notes

**Status: implemented.** `examples/italian_brainrot_cs` now loads its assets
with the code above, and renders the same scene as `examples/italian_brainrot`.

Where the implementation differs from the stages below:

- **The temporary hand-written layer was skipped.** Stage 2 (`NativeExports`)
  and Stage 3 (`managed/RendererAssets.cs`) only existed to be deleted again in
  Stage 12. The bridge and runtime went straight to the generated API; the
  renderer's and audio's hand-written exports (`csharp_assets.rs`) and
  `pill_engine::asset_ffi` are gone.
- **No world token.** A mirrored function that takes a resource (`&mut
  AssetManager`, `&mut RenderingManager`) receives that resource's address,
  fetched per call by `ffi_get_native_resource` under the running system's
  declared access. No pointer to the whole `World` ever reaches C#.
- **Startups hold the world.** A startup runs in an exclusive scope, so its
  `Res<T>`/`ResMut<T>` parameters need no declared access.
- **Trampolines are not exported.** Each descriptor carries its trampoline's
  address, so neither the development host nor a shipping build resolves a
  symbol, and two crates mirroring same-named types cannot collide.
- **C# names.** Objects, enums and resource markers live in their crate's root
  namespace - `pill_master_renderer_data`, the renderer data crate's library
  name - and value types keep their component namespace. An associated `new`
  that returns the type also generates a C# constructor, so both
  `new ShaderParameterSlot(...)` and `ShaderParameterSlot.New(...)` work.
- **Option encoding.** An optional argument carries its presence byte at
  offset 15 of its slot; an optional result carries it at offset 0 with the
  value at offset 16.
- **Typed handle fields** come from the component field tag
  `struct:Handle<Mesh>`: every reader that folds or resolves `struct:` tags
  treats it as before, and the codegen types the field when the asset is
  mirrored.

How the open questions were settled:

- **Error type:** one `EngineException` carrying the Rust error's text.
- **Object lifetime:** `RustObject` implements `IDisposable` and a finalizer;
  either drops the Rust value once. No analyzer rule enforces disposal.
- **Leaks on reload:** an object created before any module reload throws on
  use and is leaked (logged once per type) rather than dropped by code that
  may be unloaded.
- **Shared asset types:** C# reaches a type through the trampolines of
  whichever binary last published it; asset store operations go through the
  shared column either way.

## Remaining issues

### Gaps in the implementation

- **Suites not run yet.** `test_hot_reload_suite`, `test_hot_reload_assets`,
  `test_renderer_assets`, `test_shipping_smoke`, `test_web_smoke` and the WASM
  build all touch changed code - in particular asset rehoming and the new
  shared name on `AssetManager`.
- **Audio not run end to end.** `Sound` is mirrored and compiles, but no
  project has imported a sound through the generated path.
- **Coarse stale-object check.** Any extension reload or C# assembly swap marks
  every live `RustObject` as stale, not only objects of the module that
  reloaded. This is safe (stale objects are leaked, never dropped by unloaded
  code) but stricter than necessary.
- **Finalizer drop only tested with a mock.** No test lets the garbage
  collector drop a real Rust object on the finalizer thread.
- **`.Value` on `Res<AssetManager>` fails only at runtime.** It throws a clear
  error; an analyzer rule could catch it at compile time.
- **Uncovered value shapes.** Tuples of enums are not supported, and optional
  value-type results are generated but not exercised by any test.
- **Leftover API.** `MirrorMethods.Resolve<TDelegate>` is no longer used by
  generated code; it remains only for existing runtime tests and could be
  removed or made internal.

### Pre-existing problems

- **`pill_spline` tests.** `get_location_at` computes
  `local_t = ... + 4.0` (commit `8be6515f`), which fails 5 of the crate's
  math tests.
- **Heap-field accessors in shipping builds.** `Vec`/`String` field accessors
  are still resolved by symbol in the development host only; mirrored
  functions now work in shipping builds, these accessors do not.
- **Locale-dependent bridge suite.** `test_csharp_bridge.py` expects `7.5` and
  fails where the system locale prints `7,5`; run it with
  `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1` on such machines.
- **Coding-standards violations.** 16 remain, all in example files untouched
  by this work.
- **AOT generator warnings.** The generated AOT registry produces CS8669
  nullable-annotation warnings.

### Environment requirements

- **NativeAOT publish** needs `vswhere.exe` on `PATH` (it lives in
  `C:\Program Files (x86)\Microsoft Visual Studio\Installer`).
- **`test_renderer_boundaries.py`** needs Python 3.11 or later (`tomllib`).

## Current state

**The renderer's C# asset API is spread across four layers:**

| Layer | Renderer- and audio-specific code |
| --- | --- |
| C# runtime | `Engine.LoadMeshObj`, `LoadTexturePng`, `LoadShader`, `CreateMaterial`, `ImportMaterial`, `SetSkybox`, `ClearRenderPipeline`, `ImportAsset`; `ShaderParameter`, `ShaderTextureBinding`, `MaterialTextureBinding`, `MaterialScalarParameter`, `MaterialColorParameter`, `TextureAsset`/`MeshAsset`/`SoundAsset` in `Assets.cs` |
| ABI table | One `CsEngineApi` slot per function (`EngineApi.cs`, `abi.rs`) |
| Bridge | `pill_csharp_bridge/src/assets.rs`: `ASSET_EXPORT_NAMES` lists 10 export names; one forwarding `ffi_asset_*` per function |
| Extensions | `#[no_mangle]` functions in `pill_master_renderer_data/src/csharp_assets.rs` and `pill_audio/src/csharp_assets.rs` |

**What already exists to build on:**
- **Extension-owned C#:** every C# project compiles
  `modules/extensions/*/generated/*.g.cs` and `*/managed/*.cs`.
- **Mirror macros:** `#[pill_mirror_impl]`, `#[pill_mirror_method]` and
  `#[pill_mirror_fn]` (`pill_engine_macros`) emit C-ABI trampolines and
  `PillMethodDescriptor` entries.
- **Mirror publishing:** the host resolves those trampolines
  (`publish_mirror_methods`), `pill_host/src/csharp/codegen.rs` writes the
  C#, and `MirrorMethods.Resolve` finds the addresses again after a reload
  (`MirrorMethods.Generation`).

**Limits of the mirror mechanism:**
- **Primitive types only:** arguments and returns are `u8`..`u64`,
  `i8`..`i64`, `f32`, `f64`, `bool`, `usize`, `isize` or `()`
  (`mirror_method_type_tag`).
- **Receivers:** a method needs `&self` or `&mut self`; no constructors and
  no `self` by value.
- **Development host only:** `publish_mirror_methods` is called only by
  `pill_host`. The shipping runtime (`pill_runtime`) never publishes
  mirrors, so they fail in shipping builds.

## Stage overview

| Phase | Stage | Outcome |
| --- | --- | --- |
| 1. Decouple | 1. Engine-wide C# asset primitives | World token, last-error channel, `AssetLoader`, `Handle<T>`, `ResMut<AssetManager>` |
| | 2. Named exports for any extension | Extensions' C# calls their own exports; no bridge list |
| | 3. Move the renderer and audio C# API into their crates | Bridge and runtime hold no renderer or audio code |
| 2. Generated API | 4. Mirrors in shipping builds | Mirror publishing in `pill_runtime` |
| | 5. Trampoline infrastructure | Panic catching, `Result` to exception, constructors |
| | 6. Strings, bytes, enums and tuples | Borrowed inputs and plain enums |
| | 7. Rust-owned objects in C# | Opaque classes, ownership, reload safety |
| | 8. `impl Trait` arguments and collections | `impl Into<String>`, `impl IntoIterator`, `Vec<T>` |
| | 9. Resource parameters and `Handle<T>` | `&mut AssetManager` from `ResMut<AssetManager>` |
| | 10. Asset type operations | `assets.AddNamed(name, value)` for any mirrored asset |
| | 11. Annotate the renderer and audio | `Mesh`, `Texture`, `Shader`, `Material`, sounds |
| | 12. Replace the hand-written layer | Demos on the new syntax; Stage 3's C# deleted |

Each stage lands separately and leaves the tree working.

---

## Phase 1: Decouple

**Goal:** no renderer or audio knowledge in `pill_csharp_bridge` or
`pill_csharp_runtime`. The C# API keeps its current shape in this phase;
only its location and plumbing change. Phase 2 replaces it.

### Stage 1: Engine-wide C# asset primitives

These belong to the engine, not to any extension, so they stay in the bridge
and runtime.

**World token:**
- New ABI slot `GetActiveWorld(mode, out token) -> status`. It returns an
  opaque pointer to the running invocation's `World`, valid only for the
  current call.
  - In a system: refused unless the system declared `Res<AssetManager>`
    (read) or `ResMut<AssetManager>` (write).
  - In a startup: always allowed; startups have exclusive world access.
- C# wraps it in `WorldToken`, a `ref struct`, so it cannot be stored in a
  field or captured across frames.

**Last-error channel:**
- A thread-local error slot in `pill_engine`: `set_last_error(String)` and
  `take_last_error() -> Option<String>`. Put it in `pill_engine`, not the
  bridge, so extension trampolines can write to it without depending on the
  bridge.
- New ABI slot `TakeLastError(out ptr, out len)`. C# turns a non-zero
  status into an `EngineException` carrying that message.
- This replaces the per-call status tables in `Engine.ValidateAssetStatus`
  and in the renderer's `csharp_assets.rs`.

**`AssetLoader`:**
- C# `AssetLoader` with `Path(string)`, `Bytes(byte[])` and `Load()`.
  - `Load()` reads through `pill_engine::asset_store`
    (`AssetLoader::load`): the pack in shipping and web builds, the file in
    development.
  - New ABI slot `AssetLoad(path, len, out buffer)`, using the existing
    two-call buffer protocol in `managed_buffer.rs`.

**`Handle<T>` and `AssetManager`:**
- C# `Handle<T>`: `(uint Index, uint Generation)`, the same layout as Rust's
  `Handle<T>`, with `Invalid`, and convertible to and from the untyped
  `AssetHandle` and the generated `pill_master_renderer.component.Handle`.
- C# `AssetManager` marker type, and `Res<AssetManager>` /
  `ResMut<AssetManager>` declaring access to the native `AssetManager`
  resource:
  - The bridge maps the marker's identity to the native `ResourceId`
    (`resolve_resource_access`, `backend.rs:1531`).
  - `ffi_get_resource_view` refuses that identity, so `.Value` cannot
    expose Rust memory.
  - **Check first:** `AssetManager` implements `Resource` without a
    `shared_name`, so its `ResourceId` is `TypeId`-based. Give it a
    `shared_name` if that is not stable across binaries.
- **Startup parameters:** startups carry no parameters today
  (`ManagedStartup(Name, Action Run)` in `ProjectHost.cs`, and
  `AotStartupRegistration` in the AOT generator). Add parameter binding so a
  startup can take `ResMut<AssetManager>`, with the same discovery in
  reflection (`ProjectHost.cs`) and AOT (`EcsAotRegistryGenerator.cs`).

**Tests:**
- Bridge: token refused without declared access; error round trip;
  `AssetLoad` from a mounted directory and from a pack.
- Runtime: a startup with a `ResMut<AssetManager>` parameter, in both the
  reflection and the AOT registry.

### Stage 2: Named exports for any extension

**Goal:** an extension's C# can call that extension's own `extern "C"`
functions without the bridge knowing their names.

**Changes:**
- Generalise the bridge's export lookup:
  - **Development:** the host already resolves names from loaded modules.
    Publish every `PillExportDescriptor` a module submits instead of the
    fixed `ASSET_EXPORT_NAMES` list.
  - **Shipping:** use `component_registry::find_export`, as today.
- C# `NativeExports.Resolve(string name) -> IntPtr`. Like `MirrorMethods`,
  it is cleared on rebind and tied to a generation counter, so a cached
  pointer is re-resolved after an extension reload.
- The existing renderer and audio exports already submit
  `PillExportDescriptor`s for static linking; check that every function in
  both `csharp_assets.rs` files has one.

**Tests:** an export resolves in development and shipping; a cached address
is refreshed after an extension reload (`test_hot_reload_suite.py`).

### Stage 3: Move the renderer and audio C# API into their crates

**Changes:**
- Create `pill_master_renderer_data/managed/RendererAssets.cs` with
  today's API shape, implemented over `NativeExports.Resolve`,
  `WorldToken` and the last-error channel:
  - `LoadMeshObj`, `LoadTexturePng`, `LoadShader`, `CreateMaterial`,
    `ImportMaterial`, `SetSkybox`, `ClearRenderPipeline`, and the
    texture and mesh imports;
  - the argument structs (`ShaderParameter`, `MaterialTextureBinding`,
    ...).
- Create `pill_audio/managed/AudioAssets.cs` with the sound import.
- Change the export signatures to take a `WorldToken` pointer and report
  errors through the last-error slot. They are free to change because
  only their own crate's C# calls them now.
- **Delete:**
  - `pill_csharp_bridge/src/assets.rs` and `ASSET_EXPORT_NAMES`;
  - the asset slots in `CsEngineApi` (`abi.rs`, `EngineApi.cs`). Keep the
    slot order rule: retire slots by leaving placeholders, or bump the ABI
    version check in `LoaderInterop.cs`;
  - the renderer methods in `Engine.cs` and the renderer and audio types in
    `Assets.cs`.
- Update `italian_brainrot_cs` and `project_cs` to the moved API.
- **Optional:** keep `[Obsolete]` forwarders in the runtime for one release.
  They can't call into the extension directly, so they would only throw a
  "moved to ..." message.

**Tests:**
- The C# examples run unchanged in behaviour.
- `test_csharp_bridge.py`, `test_renderer_assets.py`,
  `test_shipping_smoke.py` and `test_web_smoke.py` pass.

**Done when:** `rg -i "mesh|texture|shader|material|sound|skybox"` finds no
renderer or audio code in `pill_csharp_bridge/src` or
`pill_csharp_runtime/src`.

---

## Phase 2: Generated API

**Goal:** annotate the Rust API once; the host generates the C# shown at the
top of this file.

### Stage 4: Mirrors in shipping builds

**Changes:**
- In `pill_runtime` (shipping), collect the statically linked
  `PillMethodDescriptor`s and call `publish_mirror_methods`, as `pill_host`
  does in `runtime.rs:2378`.
- Generated C# is committed (`generated/*.g.cs`) and compiled into the AOT
  image. Check that `MirrorMethods.Resolve<TDelegate>` and
  `[UnmanagedFunctionPointer]` delegates work under NativeAOT; if they do
  not, generate `delegate* unmanaged[Cdecl]` function pointers instead,
  which AOT supports directly.

**Tests:** a mirrored method from `pill_spline` (`Spline.GetLocationX`)
called from `project_cs` in `test_shipping_smoke.py` and `test_web_smoke.py`.

### Stage 5: Trampoline infrastructure

**Goal:** the shared machinery every richer type relies on.

**Changes in `pill_engine_macros` (`emit_mirrored_method_trampoline`,
`pill_mirror_fn`):**
- **Status return:** every trampoline returns `u8` status and writes the
  real return value through an out pointer.
  - Status `0` is success; `1` means an error was written to the last-error
    slot (Stage 1).
  - Existing primitive mirrors switch to this shape. The C# side is
    generated, so nothing hand-written breaks.
- **Panics:** wrap every trampoline body in `catch_unwind`, so a panic
  becomes an error message rather than unwinding across `extern "C"`.
- **`Result<T, E>` returns** where `E: Display`: `Err` writes
  `e.to_string()` to the last-error slot. The generated C# throws
  `EngineException(message)`.
- **Functions with no receiver** inside `#[pill_mirror_impl]`
  (`Mesh::from_obj_bytes`, `Shader::new`) become C# `static` methods on the
  mirrored type.
- **Descriptor:** extend `PillMethodDescriptor` with `receiver`
  (`None | Ref | Mut | Value`), `returns_result` and `can_fail`. Update
  `ResolvedMirrorMethod` and the codegen to match. The ABI between macro and
  host changes, so bump the descriptor version and check that both sides
  agree.

**Tests:** macro unit tests (expansion compiles; unsupported shapes give a
clear compile error); a bridge test where a mirrored function returns `Err`
and C# sees the message; a panicking mirror does not abort the host.

### Stage 6: Strings, bytes, enums and tuples

**New argument types:**

| Rust | C# | ABI |
| --- | --- | --- |
| `&str` | `string` (UTF-8 encoded on the C# side, pooled or `stackalloc` buffer) | `(*const u8, u32)` |
| `&[u8]` | `ReadOnlySpan<byte>` | `(*const u8, u32)` |
| `&[T]` where `T` is a mirrored value type or primitive | `ReadOnlySpan<T>` | `(*const T, u32)` |
| Plain enum with `#[derive(PillMirror)]` and `#[repr(u8 / u16 / u32)]` | C# `enum` with the same underlying type and values | the integer |
| Tuple of primitives or enums, e.g. `(u32, u32)` | `ValueTuple<...>` | each element as its own argument |

**New return types:**

| Rust | C# |
| --- | --- |
| `String` | `string`, copied out through the two-call buffer protocol |
| `&str` | `string`, copied |
| enum | enum |

**Changes:**
- Extend `#[derive(PillMirror)]` to fieldless enums: emit a
  `PillValueTypeDescriptor` variant listing variant names and discriminants,
  and generate a C# `enum`. Refuse enums without an explicit `repr` or
  with data variants.
- Add `TextureType` and `ShaderParameterType` here, once the macro supports
  them (check their current derives).

**Tests:** codegen golden files for each type; round-trip tests through a
test extension (`pill_test`).

### Stage 7: Rust-owned objects in C#

**Goal:** a Rust value that is not plain data (`Mesh`, `ShaderBuilder`,
`ShaderParameterSlot`) can be created, passed, consumed and dropped from C#.

**Rust side:**
- `#[pill_mirror_object]` on a type, or a `PillObject` derive. It emits:
  - a type descriptor (name, crate, `drop` trampoline symbol);
  - `pill_drop_<Type>(ptr)`, which drops the `Box<Type>`.
  The type must be `Send`, so C# can release it from the finalizer thread.
- **New argument and return tags** in mirrored signatures:

  | Rust | Meaning in C# |
  | --- | --- |
  | `Type` returned by value | boxed; C# receives a new object |
  | `Type` taken by value (`self` or argument) | the C# object is consumed and its pointer cleared |
  | `&Type` / `&mut Type` (`self` or argument) | borrowed for the call |

**C# side:**
- Generated as a `sealed class Mesh : RustObject`. `RustObject` lives in the
  runtime and holds the pointer, the type name and the module generation it
  was created in. It implements `IDisposable` and a finalizer.
- Using a consumed object throws `ObjectDisposedException("... was moved
  into ...")`.
- Builder chains work naturally: `Shader.New(..)` returns `ShaderBuilder`;
  each `WithX` consumes it and returns a new `ShaderBuilder` object;
  `Build()` consumes it and returns `Shader`.

**Reload safety:**
- An object created before its module reloaded must not run the old
  module's drop code. `RustObject` compares its creation generation with
  `MirrorMethods.Generation`. On a mismatch, every use throws, and
  dispose or finalize **leaks** the allocation and logs it once per type,
  rather than calling a possibly unmapped drop function.
- Document that these objects are meant to be short-lived (built and handed
  to `AssetManager` in one call chain).

**Tests:**
- Create, consume and dispose without leaks (count drops through a test
  hook in `pill_test`).
- Use after move throws.
- An object surviving an extension reload is leaked, not dropped
  (`test_hot_reload_suite.py`).
- Finalizer-thread drop is safe.

### Stage 8: `impl Trait` arguments and collections

The Rust API takes `impl Into<String>`, `impl IntoIterator<Item = T>` and
`AssetLoader`. Rewriting these signatures only for C# is not the goal, so
the macro supports these common patterns:

| Rust parameter | C# parameter | Trampoline converts with |
| --- | --- | --- |
| `impl Into<String>`, `impl AsRef<str>` | `string` | `&str` then `.into()` / passthrough |
| `impl IntoIterator<Item = T>`, `Vec<T>` where `T` is a primitive or mirrored value type | `ReadOnlySpan<T>` | `slice.to_vec()` / `iter().copied()` |
| `impl IntoIterator<Item = T>`, `Vec<T>` where `T` is a mirrored object | `T[]` / `ReadOnlySpan<T>` | each element consumed (moved) into a `Vec` |
| `AssetLoader` | `AssetLoader` (Stage 1's C# type) | rebuilt as `AssetLoader::Path` / `AssetLoader::Bytes` from its two fields |
| `&Handle<T>`, `Handle<T>` | `Handle<T>` (Stage 9) | rebuilt from index and generation |

Anything else stays a compile error naming the parameter and suggesting a
mirror-friendly overload.

**Tests:** one mirrored function per row through `pill_test`, plus codegen
golden files.

### Stage 9: Resource parameters and `Handle<T>`

**Resource parameters:**
- A mirrored function may take `&T` or `&mut T` where `T: Resource`, e.g.
  `&mut AssetManager` or `&mut RenderingManager`. In C# this becomes a
  `Res<T>` / `ResMut<T>` parameter.
- The generated C# passes Stage 1's `WorldToken`, after the same access
  check. The trampoline fetches the resource from the world and reports
  "resource missing" through the last-error slot.
- This is what lets `RenderingManager` functions (`clear_render_pipeline`,
  `set_skybox`) be mirrored without hand-written exports.
- Native resources need C# marker types and identities like `AssetManager`
  (Stage 1). Generate them from a `#[pill_mirror_resource]` marker on the
  Rust resource type.

**`Handle<T>`:**
- `Handle<T>` becomes a mirrored generic: `T` must itself be a mirrored
  object or a C# asset type. The C# side is Stage 1's `Handle<T>`.
- Component fields typed `Handle<Mesh>` should then generate as
  `Handle<Mesh>` rather than the opaque `pill_master_renderer.component.Handle`.
  Keep the old struct as a conversion target for one release.

**Tests:**
- A mirrored function taking `&mut AssetManager` is refused without declared
  access and works with it.
- `MeshRendererComponent.Mesh` is a typed `Handle<Mesh>` in generated
  output.

### Stage 10: Asset type operations

**Goal:** `assets.AddNamed(name, mesh)` works for any mirrored Rust asset
type with no per-type hand-written code.

**Rust side:**
- On a type that is both `Asset` and a mirrored object, an attribute
  (`#[pill_mirror_object(asset)]`, or a `PillAsset` derive) emits these
  trampolines, each taking a `WorldToken`:
  - `add(token, box) -> handle`
  - `add_named(token, name, box) -> handle`, refusing a name in use through
    the last-error slot
  - `add_named_with_guid(token, name, guid, box) -> handle`
  - `remove(token, handle) -> box or null`
  - `contains`, `handle_by_name` and `handle_by_guid`
- If the type implements `ImportedAsset`: `import(token, path, policy,
  settings_json) -> handle, guid, already_loaded`, reusing
  `asset_ffi::import_for_ffi`.
- If the type implements `StandaloneAsset`: `import_standalone(token, path)`,
  reusing `import_standalone_for_ffi`.

**C# side:**
- The generated class implements `IRustAsset<TSelf>`, whose static abstract
  members point at those trampolines.
- Runtime extension methods on `Res<AssetManager>` and
  `ResMut<AssetManager>`: `Add`, `AddNamed`, `AddNamedWithGuid`, `Remove`,
  `Contains`, `Find`, `FindByGuid`, `Import<T>(path, policy, settings)`
  and `ImportStandalone<T>(path)`. They dispatch through
  `T`'s static members, so the runtime names no asset type.
- `Assets.Import<T>` and the `TextureAsset` / `MeshAsset` / `SoundAsset`
  markers are replaced by `assets.Import<Texture>(...)` etc.
- Typed import settings (e.g. `TextureImportSettings`) can be mirrored value
  types where they are plain data; otherwise they stay JSON strings.

**Tests:**
- Add, find, remove and import a `Mesh` from C#.
- A name already in use throws with Rust's message.
- `Remove` returns an object that can be disposed.

### Stage 11: Annotate the renderer and audio

**`pill_master_renderer_data`:**
- **Asset objects:** `Mesh`, `Texture`, `Shader` and `Material`:
  - `Mesh`: `from_obj_bytes`, `from_data` (if its vertex type can be a
    mirrored value type) and `triangle`.
  - `Texture`: `new`, `from_rgba` and `from_rgba_f32`.
  - `Shader`: `new` (returning `ShaderBuilder`).
  - `Material`: `builder` (returning `MaterialBuilder`).
- **Builder objects:**
  - `ShaderBuilder`: every `with_*` and `build`.
  - `MaterialBuilder`: `shader`, `texture`, `scalar_parameter`,
    `bool_parameter`, `color_parameter`, `rendering_order` and `build`.
- **Slot objects:** `ShaderParameterSlot::new` and
  `ShaderTextureSlot::new`.
- **Enums:** `TextureType` and `ShaderParameterType`.
- **Rendering manager:** the rendering pipeline functions now behind
  `pill_render_data_clear_render_pipeline` and `pill_render_data_set_skybox`,
  as mirrored functions taking `&mut RenderingManager` / `&AssetManager`.

**`pill_audio`:**
- `Sound` as a mirrored asset with import support.

**Codegen:**
- Check the generated names against the target syntax at the top of this
  file (`Shader.New`, `WithVertexSource`, `Texture.New`, `Mesh.FromObjBytes`).
- `Shader::new` becomes `Shader.New`. Decide whether to emit a C#
  `new Shader(...)` constructor as well. Constructors cannot return a
  different type (`ShaderBuilder`), so keep `New` as a static method.

**Tests:** codegen golden file for `pill_master_renderer_data`; the
renderer's existing Rust tests unchanged.

### Stage 12: Replace the hand-written layer

**Changes:**
- Port `italian_brainrot_cs` and `project_cs` to the generated API. The
  `italian_brainrot_cs` loader should read the same as
  `examples/italian_brainrot/src/asset_loading.rs`.
- Delete Stage 3's `managed/RendererAssets.cs` and
  `managed/AudioAssets.cs`, the hand-written renderer and audio exports in
  both `csharp_assets.rs` files, and Stage 2's `NativeExports` if nothing
  else uses it.

**Tests:**
- `test_csharp_bridge.py`, `test_renderer_assets.py`,
  `test_hot_reload_suite.py`, `test_shipping_smoke.py` and
  `test_web_smoke.py`.
- Visual check of `italian_brainrot_cs` against `italian_brainrot`.

---

## Open questions

- **Error type:** is `EngineException` one type, or should the generated
  code map known Rust error enums (`AssetLoadError`,
  `AssetBindingError`) to specific C# exceptions?
- **Object lifetime:** should `RustObject` require `using` / explicit
  disposal (an analyzer rule could enforce it), or is finalizer cleanup
  acceptable for builders?
- **Leaks on reload:** is leaking objects that survive a module reload
  acceptable, or should the reload graveyard keep the old module mapped
  until all its objects are released?
- **Shared asset types:** if a mirrored asset type is also `shared`
  (`Asset::shared_name`), which binary's trampolines does C# use after one
  of them reloads?

## Risks

| Risk | Mitigation |
| --- | --- |
| Mirrors don't work in shipping builds today | Stage 4 is first in Phase 2 and gated by shipping and web smoke tests |
| Changing the trampoline shape breaks existing mirrors (`pill_spline`) | All existing mirror C# is generated; regenerate and run `project_cs` in Stage 5 |
| An object outlives its module and its drop code | Generation check; leak instead of drop; logged |
| Panics crossing `extern "C"` abort the process | `catch_unwind` in every trampoline (Stage 5) |
| Macro complexity grows with every supported pattern | A closed set of type tags; anything else is a compile error with a suggestion |
| `impl Trait` patterns not covered by Stage 8 | Compile error suggests adding a mirror-friendly overload in Rust |
| The ABI change between macro and host breaks a stale extension build | Descriptor version check in the host (Stage 5) |
