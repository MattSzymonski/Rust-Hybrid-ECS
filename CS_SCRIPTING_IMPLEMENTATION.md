# C# scripting for `tracy_live` — implementation plan

Supersedes the open questions in `CS_SCRIPTING_PROPOSAL.md` with concrete
answers, and turns the design into a step-by-step build plan. Read the
proposal first for the *why*; this is the *how*.

**Status: implemented and verified** (regression, hot-reload, and watchdog
tests all passed — see §6). Two corrections surfaced during implementation,
both reflected in the code and in this doc:

- **`Components.cs` lives in `tracy_live_game_cs_loader`, not
  `tracy_live_game_cs`.** `EngineApi.cs`'s delegate signatures reference
  `Position`/`Velocity`/etc., so the loader project needs those types
  declared — and the reference direction only goes one way
  (`tracy_live_game_cs` → `tracy_live_game_cs_loader`). Plain data structs
  carry no sandboxing risk regardless of which project declares them, so
  this doesn't weaken anything, it's just a compile-order fix. §3.1 below
  reflects this.
- **`GameContext.Load` can't resolve `tracy_live_game_cs_loader` via
  `AssemblyLoadContext.Default.Assemblies`.** hostfxr's component-hosting
  mode (`load_assembly_and_get_function_pointer`) does not load the
  requested assembly into `AssemblyLoadContext.Default` — verified by
  instrumenting it, `Default.Assemblies` doesn't include it at all. The
  correct fix is `typeof(GameHost).Assembly` inside the `Load` override:
  since that code is only ever running as part of the already-loaded
  `tracy_live_game_cs_loader`, this always returns the exact same instance
  regardless of which underlying context hostfxr actually used, which is
  also what correctness requires (`Systems.cs` must resolve `TracyLive.Engine`
  to the *same* type identity that `LoaderInterop.Init` called `Bind` on, or
  its static `_api` field is a different static and the whole hand-off breaks
  silently). See §3.1's `GameHost.cs`.

## Decisions locked in

1. **Design A** — zero-copy `Span<T>` over component storage (not batched
   copies).
2. **No entity destruction from C#** — `Movement`/`HealthDecay`/`Gravity`
   only; matches the existing demo's observed behavior since `cleanup_system`
   never destroys anything under default tuning anyway.
3. **CLI flags are required, not defaulted.** Exactly one of
   `--rs_scripting` / `--cs_scripting` must be passed. Zero or two is a
   startup error (a clear message + `exit(1)`, not a Rust panic/backtrace).
4. **Watchdog thread for `Update`** — hangs are contained by running
   `Update(dt)` on a dedicated worker thread with a timeout; a timeout
   permanently disables further C# calls for the rest of the process's life,
   but the engine keeps running. Full detail in §2.3.

## Repo layout after this change

```
Rust-Hybrid-ECS/
  src/world.rs                              <- + component_slice_mut<T>()
  examples/
    tracy_live/
      main.rs                               <- CLI flags, branches rs/cs
      hot.rs, watch.rs                      <- existing, unchanged
      hostfxr.rs                            <- NEW, ported from flappy
      hot_cs.rs                             <- NEW, EngineApi + watchdog + CsGame
      cs_components.rs                      <- NEW, repr(C) components + setup
    tracy_live_game/                        <- existing Rust cdylib, unchanged
    tracy_live_game_cs_loader/               <- NEW, stable, AllowUnsafeBlocks=true
      tracy_live_game_cs_loader.csproj
      src/
        EngineApi.cs                        <- P/Invoke struct mirror
        Components.cs                       <- blittable structs (lives here — see below)
        Engine.cs                           <- Span<T> facade (the only unsafe call sites)
        GameHost.cs                         <- reload polling (ported from game_cs_loader)
        LoaderInterop.cs                    <- stable native entry points
    tracy_live_game_cs/                      <- NEW, reloadable, AllowUnsafeBlocks=false
      tracy_live_game_cs.csproj             <- references the loader project
      src/
        Systems.cs                          <- ***the file you edit***
        Interop.cs                          <- [UnmanagedCallersOnly], IntPtr not pointers
```

`Components.cs` lives in the loader project, not the reloadable one — see
the "Status" corrections note at the top of this document for why
(`EngineApi.cs` needs those types, and the reference direction only goes
`tracy_live_game_cs` → `tracy_live_game_cs_loader`).

`trait_type_map` needed **no change**: `VecStorage<T, Dyn>`'s `data` field
is already `pub`, so `ecs_hybrid::World::component_slice_mut` (§1 below)
accesses `.data.as_mut_slice()` directly. The original plan's proposed
`as_slice`/`as_mut_slice` accessor methods turned out to be unnecessary.

## 1. `ecs_hybrid::World::component_slice_mut`

`src/world.rs`, alongside the other component-access methods:

```rust
/// Mutable slice over every entity's `T` component — for FFI-facing bulk
/// access (see `examples/tracy_live/hot_cs.rs`), not a general query.
///
/// Returns `None` unless every entity carrying `T` lives in exactly one
/// archetype. `tracy_live`'s C# path relies on this holding (its component
/// set never changes at runtime), but this method itself makes no such
/// assumption — it just refuses to guess when there's more than one
/// archetype to pick from.
pub fn component_slice_mut<T: Component + 'static>(&mut self) -> Option<&mut [T]> {
    let mut found: Option<&mut crate::archetype::Archetype> = None;
    for archetype in self.archetypes.values_mut() {
        if archetype.has_component::<T>() {
            if found.is_some() {
                return None;
            }
            found = Some(archetype);
        }
    }
    found.map(|a| a.component_storages.get_storage_mut::<T>().as_mut_slice())
}
```

## 2. Host-side Rust (`examples/tracy_live/`)

### 2.1 `cs_components.rs`

Components defined here (host, never rebuilt while running) because C#
mirrors their exact layout — unlike `tracy_live_game`'s components, these
must stay byte-stable for the whole process lifetime.

```rust
use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}
impl Component for Position {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
}
impl Component for Velocity {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Health(pub f32);
impl Component for Health {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Mass(pub f32);
impl Component for Mass {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GravityForce {
    pub x: f32,
    pub y: f32,
}
impl Component for GravityForce {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Mass, GravityForce);

/// Fast LCG random f32 — identical helper to `tracy_live_game::game::lcg`,
/// duplicated rather than shared; it's a 15-line leaf helper and these two
/// crates are never built together.
fn lcg() -> f32 {
    #[cfg(target_arch = "x86_64")]
    fn seed() -> u64 {
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn seed() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }
    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = Cell::new(seed().wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    S.with(|s| {
        let mut x = s.get();
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        s.set(x);
        (x >> 32) as f32 / u32::MAX as f32
    })
}

/// Registers components and spawns the same 30000-entity population as
/// `tracy_live_game::game::setup` — called once at startup, never again
/// (the C# path has no world-reset-on-reload; only `Systems.cs`'s code
/// gets swapped, the entity population underneath stays put).
pub fn setup(engine: &mut Engine) {
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Mass>();
    engine.world_mut().register_component::<GravityForce>();

    engine.world_mut().reserve_entities(32000);
    for _ in 0..30000 {
        let _ = engine
            .world_mut()
            .create_entity()
            .with(Position { x: (lcg() - 0.5) * 1000.0, y: (lcg() - 0.5) * 1000.0 })
            .with(Velocity { x: (lcg() - 0.5) * 0.2, y: (lcg() - 0.5) * 0.2 })
            .with(Health(100.0))
            .with(Mass(1.0 + lcg() * 9.0))
            .with(GravityForce { x: 0.0, y: 0.0 })
            .build();
    }
}
```

Note: no `Enemy` marker component — nothing in the C# path filters on it
(there's no `enemy_ai_system` equivalent), so it's simply omitted rather than
carried over as dead weight.

### 2.2 `hostfxr.rs`

Copy `flappy/src/hostfxr.rs` verbatim into `examples/tracy_live/hostfxr.rs`
— it's already fully generic (`HostfxrContext`, `find_hostfxr`,
`get_unmanaged_fn`), nothing Flappy-specific to change.

### 2.3 `hot_cs.rs` — the C#-path harness

```rust
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use ecs_hybrid::Engine;

use crate::cs_components::{GravityForce, Health, Mass, Position, Velocity};
use crate::hostfxr::HostfxrContext;

// ---------------------------------------------------------------------------
// Native EngineApi — the function-pointer table handed to C#.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct EngineApi {
    pub entity_count: extern "C" fn() -> u32,
    pub get_positions: extern "C" fn(*mut *mut Position, *mut u32),
    pub get_velocities: extern "C" fn(*mut *mut Velocity, *mut u32),
    pub get_healths: extern "C" fn(*mut *mut Health, *mut u32),
    pub get_masses: extern "C" fn(*mut *mut Mass, *mut u32),
    pub get_gravity_forces: extern "C" fn(*mut *mut GravityForce, *mut u32),
}

impl EngineApi {
    fn new() -> Self {
        Self {
            entity_count: ffi_entity_count,
            get_positions: ffi_get_positions,
            get_velocities: ffi_get_velocities,
            get_healths: ffi_get_healths,
            get_masses: ffi_get_masses,
            get_gravity_forces: ffi_get_gravity_forces,
        }
    }
}

/// Set once before the worker thread starts; the getters below read it from
/// whichever thread is currently "holding the baton" (see the watchdog
/// protocol in `CsGame::update` — never both at once in the non-hang path).
static ENGINE_PTR: AtomicPtr<Engine> = AtomicPtr::new(std::ptr::null_mut());

fn engine_mut() -> &'static mut Engine {
    let ptr = ENGINE_PTR.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "EngineApi called before Init");
    unsafe { &mut *ptr }
}

macro_rules! component_getter {
    ($name:ident, $ty:ty) => {
        extern "C" fn $name(out_ptr: *mut *mut $ty, out_len: *mut u32) {
            match engine_mut().world_mut().component_slice_mut::<$ty>() {
                Some(slice) => unsafe {
                    *out_ptr = slice.as_mut_ptr();
                    *out_len = slice.len() as u32;
                },
                None => unsafe {
                    *out_ptr = std::ptr::null_mut();
                    *out_len = 0;
                },
            }
        }
    };
}

component_getter!(ffi_get_positions, Position);
component_getter!(ffi_get_velocities, Velocity);
component_getter!(ffi_get_healths, Health);
component_getter!(ffi_get_masses, Mass);
component_getter!(ffi_get_gravity_forces, GravityForce);

extern "C" fn ffi_entity_count() -> u32 {
    engine_mut().world().entity_count() as u32
}

// ---------------------------------------------------------------------------
// Build + hostfxr bootstrap
// ---------------------------------------------------------------------------

type InitFn = extern "system" fn(*const EngineApi);
type UpdateFn = extern "system" fn(f32);

fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_dotnet_project(name: &str) -> Result<(), String> {
    let project = workspace_dir().join("examples").join(name);
    let status = Command::new("dotnet")
        .args(["build", "-c", "Release", "--nologo"])
        .arg(&project)
        .status()
        .map_err(|e| format!("failed to run `dotnet build {name}`: {e}"))?;
    if !status.success() {
        return Err(format!("`dotnet build {name} -c Release` failed"));
    }
    Ok(())
}

fn assembly_dir(name: &str) -> PathBuf {
    workspace_dir()
        .join("examples")
        .join(name)
        .join("bin")
        .join("Release")
        .join("net8.0")
}

fn load_managed(
) -> Result<(HostfxrContext, InitFn, UpdateFn), Box<dyn std::error::Error>> {
    build_dotnet_project("tracy_live_game_cs_loader").map_err(Box::<dyn std::error::Error>::from)?;
    build_dotnet_project("tracy_live_game_cs").map_err(Box::<dyn std::error::Error>::from)?;

    let loader_dir = assembly_dir("tracy_live_game_cs_loader");
    let assembly = loader_dir.join("tracy_live_game_cs_loader.dll");
    let runtime_config = loader_dir.join("tracy_live_game_cs_loader.runtimeconfig.json");

    // The loader reads this to find tracy_live_game_cs.dll, same trick as
    // FLAPPY_MANAGED_DIR in `flappy/src/main.rs`.
    std::env::set_var(
        "TRACY_LIVE_MANAGED_DIR",
        assembly_dir("tracy_live_game_cs"),
    );

    let hostfxr = HostfxrContext::new(&runtime_config)?;
    let init = hostfxr.get_unmanaged_fn::<InitFn>(
        &assembly,
        "TracyLive.Loader.LoaderInterop, tracy_live_game_cs_loader",
        "Init",
    )?;
    let update = hostfxr.get_unmanaged_fn::<UpdateFn>(
        &assembly,
        "TracyLive.Loader.LoaderInterop, tracy_live_game_cs_loader",
        "Update",
    )?;

    Ok((hostfxr, init, update))
}

// ---------------------------------------------------------------------------
// Watchdog — Update() runs on its own thread with a timeout.
// ---------------------------------------------------------------------------

const UPDATE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct CsGame {
    request_tx: mpsc::Sender<f32>,
    response_rx: mpsc::Receiver<()>,
    disabled: bool,
    _hostfxr: HostfxrContext, // keep hostfxr + the loaded runtime alive
    _api: Box<EngineApi>,     // keep the table alive; C# holds the raw pointer
}

impl CsGame {
    /// Send `dt` to the worker thread and wait up to `UPDATE_TIMEOUT`.
    ///
    /// On timeout: logs it once, and permanently stops calling into C# for
    /// the rest of this process's life. The worker thread is *not* killed —
    /// .NET gives no safe way to reclaim a wedged thread — it's simply
    /// abandoned. Because nothing else in this mode resizes the component
    /// arrays it might still be writing into (no spawn/destroy after
    /// startup, zero registered Rust systems), the abandoned thread's writes
    /// stay confined to those arrays: a real, technically-UB data race, but
    /// one that can't corrupt anything else or crash the process. See
    /// `CS_SCRIPTING_PROPOSAL.md`'s "Sandboxing" section for the full
    /// reasoning.
    pub fn update(&mut self, dt: f32) {
        if self.disabled {
            return;
        }
        if self.request_tx.send(dt).is_err() {
            eprintln!("[cs] worker thread is gone; disabling C# scripting");
            self.disabled = true;
            return;
        }
        match self.response_rx.recv_timeout(UPDATE_TIMEOUT) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "[cs] Update() did not return within {:?} — assuming a hang. \
                     Disabling C# scripting for the rest of this run; the engine \
                     keeps running, but the C#-scripted population is now frozen.",
                    UPDATE_TIMEOUT
                );
                self.disabled = true;
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[cs] worker thread channel disconnected; disabling C# scripting");
                self.disabled = true;
            }
        }
    }
}

pub fn start(engine: &mut Engine) -> Result<CsGame, String> {
    ENGINE_PTR.store(engine as *mut Engine, Ordering::Release);

    let (hostfxr, init, update) = load_managed().map_err(|e| e.to_string())?;

    let api = Box::new(EngineApi::new());
    init(api.as_ref() as *const EngineApi);

    let (request_tx, request_rx) = mpsc::channel::<f32>();
    let (response_tx, response_rx) = mpsc::channel::<()>();

    std::thread::Builder::new()
        .name("cs-script-worker".into())
        .spawn(move || {
            for dt in request_rx {
                update(dt);
                // Ignore send errors: the main thread may have already
                // given up waiting (timeout) and dropped its receiver.
                let _ = response_tx.send(());
            }
        })
        .map_err(|e| format!("failed to spawn cs-script-worker thread: {e}"))?;

    Ok(CsGame {
        request_tx,
        response_rx,
        disabled: false,
        _hostfxr: hostfxr,
        _api: api,
    })
}
```

Notes:
- `entity_count`/getters run on whichever thread calls into C# — always the
  `cs-script-worker` thread once started, since `Init` happens before the
  worker spawns and nothing else calls `EngineApi` functions.
- `load_managed`'s two `dotnet build` calls are blocking and happen once, at
  `start()`. There's no Rust-side file watcher for the C# path (unlike
  `hot.rs`'s `notify` watcher) — reload detection is `GameHost.cs`'s job,
  exactly like today's C# Flappy path; you rerun `dotnet build
  examples/tracy_live_game_cs -c Release` yourself after editing
  `Systems.cs`.
- If the initial build or hostfxr init fails, `start()` returns `Err` and
  `main.rs` should print it and `exit(1)` — unlike the Rust path, there's no
  meaningful "run with a stub" state here, since without a single successful
  `Init` there's no `Update` fn to call at all.

### 2.4 `main.rs` — CLI flags + branching

```rust
use ecs_hybrid::Engine;
use std::time::Instant;

mod cs_components;
mod hostfxr;
mod hot;
mod hot_cs;
mod watch;

enum Mode {
    Rust,
    CSharp,
}

fn parse_mode() -> Mode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rs = args.iter().any(|a| a == "--rs_scripting");
    let cs = args.iter().any(|a| a == "--cs_scripting");
    match (rs, cs) {
        (true, false) => Mode::Rust,
        (false, true) => Mode::CSharp,
        (true, true) => {
            eprintln!("error: pass exactly one of --rs_scripting / --cs_scripting, not both");
            std::process::exit(1);
        }
        (false, false) => {
            eprintln!("error: pass one of --rs_scripting or --cs_scripting");
            std::process::exit(1);
        }
    }
}

fn main() {
    ecs_hybrid::profile_init!();
    ecs_hybrid::profile_thread!("main");
    std::thread::sleep(std::time::Duration::from_millis(200));

    match parse_mode() {
        Mode::Rust => run_rs_scripting(),
        Mode::CSharp => run_cs_scripting(),
    }
}

fn run_rs_scripting() {
    // Unchanged from today's main.rs: hot::start(), edge-triggered
    // game_setup calls, engine.process_frame() loop, FPS reporting.
    // ...
}

fn run_cs_scripting() {
    let mut engine = Engine::new();
    engine.set_parallel_execution(true); // harmless: 0 systems in this mode
    engine.trace_frame_wait = false;

    cs_components::setup(&mut engine);

    let mut cs = match hot_cs::start(&mut engine) {
        Ok(cs) => cs,
        Err(e) => {
            eprintln!("failed to start C# scripting: {e}");
            std::process::exit(1);
        }
    };

    println!("=== Tracy Live Profiling Demo (C# scripting) ===");
    println!("Edit examples/tracy_live_game_cs/src/Systems.cs, then run:");
    println!("  dotnet build examples/tracy_live_game_cs -c Release");
    println!("in another terminal to hot-reload it.");
    println!();

    let mut last_frame = Instant::now();
    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        let dt = last_frame.elapsed().as_secs_f32();
        last_frame = Instant::now();

        cs.update(dt);
        engine.process_frame().unwrap();
        count += 1;

        let report_dt = last_report.elapsed().as_secs_f64();
        if report_dt >= 2.0 {
            let fps = count as f64 / report_dt;
            let entities = engine.world().entity_count();
            println!("  {:>6.0} FPS | {:>5} entities", fps, entities);
            count = 0;
            last_report = Instant::now();
        }
    }
}
```

`run_rs_scripting` is today's `main` body, extracted unchanged into its own
function — pure refactor, no behavior change.

## 3. C# projects

### 3.1 `tracy_live_game_cs_loader` (stable, `AllowUnsafeBlocks=true`)

`tracy_live_game_cs_loader.csproj` — copied from `game_cs_loader.csproj`
verbatim (same `net8.0`/`AllowUnsafeBlocks`/`GenerateRuntimeConfigurationFiles`
settings).

`src/Components.cs` — blittable structs, **lives here** (not in the
reloadable project — see the corrections note at the top of this document):
```csharp
using System.Runtime.InteropServices;

namespace TracyLive;

[StructLayout(LayoutKind.Sequential)]
public struct Position { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Velocity { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Health { public float Value; }

[StructLayout(LayoutKind.Sequential)]
public struct Mass { public float Value; }

[StructLayout(LayoutKind.Sequential)]
public struct GravityForce { public float X, Y; }
```

`src/EngineApi.cs`:
```csharp
using System.Runtime.InteropServices;

namespace TracyLive;

[StructLayout(LayoutKind.Sequential)]
public unsafe struct EngineApi
{
    public delegate* unmanaged[Cdecl]<uint> EntityCount;
    public delegate* unmanaged[Cdecl]<Position**, uint*, void> GetPositions;
    public delegate* unmanaged[Cdecl]<Velocity**, uint*, void> GetVelocities;
    public delegate* unmanaged[Cdecl]<Health**, uint*, void> GetHealths;
    public delegate* unmanaged[Cdecl]<Mass**, uint*, void> GetMasses;
    public delegate* unmanaged[Cdecl]<GravityForce**, uint*, void> GetGravityForces;
}
```

`src/Engine.cs` — the only unsafe call sites in the whole C# side. Note the
two `Bind` overloads: `Bind(IntPtr)` exists purely so `Interop.cs` (in the
unsafe-forbidden reloadable project) never needs an `unsafe` block itself —
it just calls `Engine.Bind(api)` and the cast happens here instead:
```csharp
namespace TracyLive;

public static unsafe class Engine
{
    private static EngineApi _api;

    public static void Bind(EngineApi* api) => _api = *api;
    public static void Bind(IntPtr api) => Bind((EngineApi*)api);

    public static uint EntityCount() => _api.EntityCount();

    public static Span<Position> Positions()
    {
        Position* ptr; uint len;
        _api.GetPositions(&ptr, &len);
        return new Span<Position>(ptr, (int)len);
    }

    public static Span<Velocity> Velocities()
    {
        Velocity* ptr; uint len;
        _api.GetVelocities(&ptr, &len);
        return new Span<Velocity>(ptr, (int)len);
    }

    public static Span<Health> Healths()
    {
        Health* ptr; uint len;
        _api.GetHealths(&ptr, &len);
        return new Span<Health>(ptr, (int)len);
    }

    public static Span<Mass> Masses()
    {
        Mass* ptr; uint len;
        _api.GetMasses(&ptr, &len);
        return new Span<Mass>(ptr, (int)len);
    }

    public static Span<GravityForce> GravityForces()
    {
        GravityForce* ptr; uint len;
        _api.GetGravityForces(&ptr, &len);
        return new Span<GravityForce>(ptr, (int)len);
    }
}
```

`src/GameHost.cs` — ported from `game_cs_loader/src/GameHost.cs`: rename
`Flappy` → `TracyLive`, drop `_draw`/`Draw()` entirely (headless demo),
`Init`/`_init` take `IntPtr` instead of `void*` (matching `Interop.cs`'s
unsafe-forbidden signature), `Update(float dt)`'s polling-then-forward shape
and the collectible `AssemblyLoadContext` reload logic otherwise unchanged.

One real bug found and fixed here: `GameContext.Load(AssemblyName)`
originally returned `null` for everything (as `game_cs_loader`'s does,
verbatim), on the assumption that would fall back to resolving
`tracy_live_game_cs_loader` from `AssemblyLoadContext.Default` the same way
BCL assemblies resolve. It doesn't — verified by instrumenting it,
`AssemblyLoadContext.Default.Assemblies` never contains
`tracy_live_game_cs_loader` at all, because hostfxr's component-hosting mode
(`load_assembly_and_get_function_pointer`) loads it into a different,
non-`Default` context. The fix:
```csharp
protected override Assembly? Load(AssemblyName assemblyName)
{
    // tracy_live_game_cs.dll references tracy_live_game_cs_loader (for
    // Engine.cs's Span<T> facade and the component structs) — it must
    // resolve to the exact same already-loaded instance this code is
    // itself running as, not a fresh copy loaded from disk. A second,
    // separately-loaded copy would mean TracyLive.Engine's static _api
    // field (bound once by LoaderInterop.Init) is a different static than
    // the one Systems.cs reads from, breaking the whole Init/Update
    // hand-off silently. typeof(GameHost).Assembly sidesteps the question
    // of which underlying context hostfxr actually used: it's always
    // exactly the assembly this code is currently executing as.
    if (assemblyName.Name == "tracy_live_game_cs_loader")
    {
        return typeof(GameHost).Assembly;
    }
    return null; // Let the default context resolve shared BCL assemblies.
}
```

`src/LoaderInterop.cs` — ported from `game_cs_loader/src/LoaderInterop.cs`:
rename `Flappy.Loader` → `TracyLive.Loader`, drop `Draw`, `Init` takes
`IntPtr` (not `void*`), point at `tracy_live_game_cs.dll` /
`TRACY_LIVE_MANAGED_DIR` env var (matches `hot_cs.rs`'s `load_managed`).

### 3.2 `tracy_live_game_cs` (reloadable, `AllowUnsafeBlocks=false`)

`tracy_live_game_cs.csproj`:
```xml
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
    <LangVersion>latest</LangVersion>
    <!-- Deliberately omitted: AllowUnsafeBlocks. This assembly must not be
         able to construct raw pointers — see CS_SCRIPTING_PROPOSAL.md's
         "Sandboxing" section. -->
    <GenerateRuntimeConfigurationFiles>true</GenerateRuntimeConfigurationFiles>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="../tracy_live_game_cs_loader/tracy_live_game_cs_loader.csproj" />
  </ItemGroup>
</Project>
```

`src/Systems.cs` — **the file you edit**, straight ports of the Rust bodies:
```csharp
namespace TracyLive;

public static class MovementSystem
{
    public static void Run()
    {
        var positions = Engine.Positions();
        var velocities = Engine.Velocities();
        for (int i = 0; i < positions.Length; i++)
        {
            positions[i].X += velocities[i].X;
            positions[i].Y += velocities[i].Y;
        }
    }
}

public static class HealthDecaySystem
{
    public static void Run()
    {
        var healths = Engine.Healths();
        for (int i = 0; i < healths.Length; i++)
        {
            healths[i].Value = MathF.Max(healths[i].Value + 0.1f, 0f);
        }
    }
}

public static class GravitySystem
{
    public static void Run()
    {
        var forces = Engine.GravityForces();
        var masses = Engine.Masses();
        for (int i = 0; i < forces.Length; i++)
        {
            float distanceSq = forces[i].X * forces[i].X + MathF.Sqrt(forces[i].Y) * forces[i].Y + 0.01f;
            float distance = MathF.Sqrt(distanceSq);
            float magnitude = masses[i].Value / (distanceSq * distance);
            forces[i].X = Math.Clamp(-forces[i].X * MathF.Sqrt(magnitude), -1f, 1f);
            forces[i].Y = Math.Clamp(-forces[i].Y * MathF.Sqrt(magnitude), -1f, 1f);
        }
    }
}
```

`src/Interop.cs` — signatures use `IntPtr`/`float` only, so this compiles
fine with `AllowUnsafeBlocks=false`. `Init` calls `Engine.Bind(api)` — the
`IntPtr` overload from `Engine.cs` (§3.1) — so no `unsafe` keyword appears
anywhere in this project at all:
```csharp
using System.Runtime.InteropServices;

namespace TracyLive;

public static class Interop
{
    [UnmanagedCallersOnly]
    public static void Init(IntPtr api)
    {
        try
        {
            Engine.Bind(api);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs] Init failed: {e}");
        }
    }

    [UnmanagedCallersOnly]
    public static void Update(float dt)
    {
        try
        {
            MovementSystem.Run();
            HealthDecaySystem.Run();
            GravitySystem.Run();
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs] Update failed: {e}");
        }
    }
}
```

## 4. Sandboxing — what's covered, concretely

| Failure | Covered? | Mechanism |
| --- | --- | --- |
| Bad `Span` index, null ref, div-by-zero, uncaught exception in `Systems.cs` | Yes | try/catch in `Interop.cs` |
| Raw pointer misuse / manual memory corruption from `Systems.cs` | Yes | `AllowUnsafeBlocks=false` on `tracy_live_game_cs` — the compiler refuses to build it |
| Infinite loop / hang in `Systems.cs` | Yes, contained | Watchdog thread + 1s timeout in `hot_cs.rs`; engine keeps running, that C# logic goes permanently inert |
| Stack overflow in `Systems.cs` | No | Uncatchable in any .NET version; out of scope |
| Equivalent Rust-side (`--rs_scripting`) protection | No | `panic = "abort"` in the release profile means a panic anywhere in the hot-reloaded Rust cdylib is a hard process kill; this is a known, accepted asymmetry between the two modes (see `CS_SCRIPTING_PROPOSAL.md`) |

## 5. Implementation order

1. `ecs_hybrid`: `World::component_slice_mut` (§1).
2. `examples/tracy_live_game_cs_loader`: csproj + `Components.cs` +
   `EngineApi.cs` + `Engine.cs` + `GameHost.cs` + `LoaderInterop.cs`.
3. `examples/tracy_live_game_cs`: csproj (with `ProjectReference`) +
   `Systems.cs` + `Interop.cs`.
4. `examples/tracy_live/hostfxr.rs`: port from `flappy`.
5. `examples/tracy_live/cs_components.rs`.
6. `examples/tracy_live/hot_cs.rs`: `EngineApi`, `ENGINE_PTR`, `load_managed`,
   watchdog + `CsGame`.
7. `examples/tracy_live/main.rs`: extract `run_rs_scripting`, add
   `parse_mode`/`run_cs_scripting`.
8. Build + verify (below).

## 6. Verification — all passed

1. `cargo build --workspace` / `cargo build --example tracy_live --release
   --features tracy` — clean build, only the one pre-existing unrelated
   warning in `src/query/iter.rs`.
2. `--rs_scripting` regression: unchanged — 30000 entities, ~400-1600 FPS,
   hot-reload messages as before.
3. Flag validation: no flags and both flags both print the expected error
   and `exit(1)`.
4. `--cs_scripting`: after fixing the two bugs described in the corrections
   note at the top of this document, runs cleanly — 30000 entities,
   ~2500-3000 FPS (lower than the Rust path, as expected — C# runs
   single-threaded).
5. Hot-reload: edited `HealthDecaySystem`'s increment while running, ran
   `dotnet build examples/tracy_live_game_cs -c Release` in another
   terminal, confirmed `[tracy_live_game_cs_loader] reloaded
   tracy_live_game_cs.dll` printed and FPS reporting continued uninterrupted;
   repeated a second time (revert + rebuild) to confirm round-tripping works,
   not just a one-shot fluke.
6. Watchdog: injected `while (true) { }` at the top of
   `MovementSystem.Run()`, rebuilt, confirmed: `[cs] Update() did not return
   within 1s — assuming a hang...` printed exactly once, FPS reporting kept
   going afterward (jumped higher, since `cs.update()` now short-circuits),
   entity count stayed at 30000, and the process stayed stable for several
   more seconds of observation. Reverted the injected hang and rebuilt clean
   afterward.
