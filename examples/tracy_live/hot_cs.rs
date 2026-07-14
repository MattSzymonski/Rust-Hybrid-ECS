//! Hot-reload support for `--cs_scripting`: the native `EngineApi` C#
//! calls into, the one-time build + hostfxr bootstrap, and the watchdog
//! thread that keeps a hung `Update()` from freezing the host.
//!
//! Unlike `hot.rs` (the `--rs_scripting` path), there's no `notify` file
//! watcher here — reload detection is `TracyLive.Loader.GameHost`'s job (it
//! polls `tracy_live_game_cs.dll`'s last-write time). After editing
//! `examples/tracy_live_game_cs/src/Systems.cs`, rebuild it yourself:
//!
//! ```text
//! dotnet build examples/tracy_live_game_cs -c Release
//! ```

use std::path::PathBuf;
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

/// Mirror of `TracyLive.EngineApi` (`tracy_live_game_cs_loader/src/
/// EngineApi.cs`). Field order and signatures must stay in lockstep.
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

/// Set once in `start()`, before the worker thread is spawned. Read by the
/// getters below from whichever thread is currently "holding the baton" —
/// see the watchdog protocol in `CsGame::update`; in the non-hang path
/// that's always the `cs-script-worker` thread, never the main thread
/// concurrently.
static ENGINE_PTR: AtomicPtr<Engine> = AtomicPtr::new(std::ptr::null_mut());

fn engine_mut() -> &'static mut Engine {
    let ptr = ENGINE_PTR.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "EngineApi called before hot_cs::start()");
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

fn load_managed() -> Result<(HostfxrContext, InitFn, UpdateFn), Box<dyn std::error::Error>> {
    build_dotnet_project("tracy_live_game_cs_loader").map_err(Box::<dyn std::error::Error>::from)?;
    build_dotnet_project("tracy_live_game_cs").map_err(Box::<dyn std::error::Error>::from)?;

    let loader_dir = assembly_dir("tracy_live_game_cs_loader");
    let assembly = loader_dir.join("tracy_live_game_cs_loader.dll");
    let runtime_config = loader_dir.join("tracy_live_game_cs_loader.runtimeconfig.json");

    // The loader reads this to find tracy_live_game_cs.dll, same trick as
    // FLAPPY_MANAGED_DIR in `flappy/src/main.rs`.
    std::env::set_var("TRACY_LIVE_MANAGED_DIR", assembly_dir("tracy_live_game_cs"));

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

/// Keeps hostfxr + the loaded runtime alive, and drives `Update` through a
/// watchdog so a hang in C# can't freeze the host. See this module's doc
/// comment and `CS_SCRIPTING_PROPOSAL.md`'s "Sandboxing" section.
pub struct CsGame {
    request_tx: mpsc::Sender<f32>,
    response_rx: mpsc::Receiver<()>,
    disabled: bool,
    _hostfxr: HostfxrContext,
    _api: Box<EngineApi>, // keep the table alive; C# holds the raw pointer
}

impl CsGame {
    /// Send `dt` to the worker thread and wait up to [`UPDATE_TIMEOUT`].
    ///
    /// On timeout: logs it once, and permanently stops calling into C# for
    /// the rest of this process's life. The worker thread is *not* killed —
    /// .NET gives no safe way to reclaim a wedged thread — it's simply
    /// abandoned. Because nothing else in this mode resizes the component
    /// arrays it might still be writing into (no spawn/destroy after
    /// startup, zero registered Rust systems), the abandoned thread's writes
    /// stay confined to those arrays: a real, technically-UB data race, but
    /// one that can't corrupt anything else or crash the process.
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

/// Builds both C# projects (blocking), boots hostfxr, calls `Init` once,
/// and spawns the `cs-script-worker` thread `update()` hands frames to.
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
