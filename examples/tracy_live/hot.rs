//! Hot-reload support for `tracy_live_game`.
//!
//! Trimmed adaptation of `hot_reloading/flappy/src/hot_rs.rs`'s build/load/
//! patch loop, aimed at `tracy_live_game`'s single entry point instead of
//! `game_rs`'s four. See that crate for the fuller write-up of the technique.
//!
//! Unlike `flappy` (which reads the hot table's function pointers every
//! frame), `tracy_live_game` resets and rebuilds the whole world on every
//! reload rather than being called every frame — see `tracy_live_game`'s
//! crate docs for why. So this table is read on an edge (`take_pending_reload`)
//! rather than continuously.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ecs_hybrid::Engine;
use libloading::{Library, Symbol};

pub type GameSetupFn = extern "C" fn(*mut Engine);

// ---------------------------------------------------------------------------
// Hot function table — the host calls through this exclusively.
// ---------------------------------------------------------------------------

pub struct HotFnTable {
    setup: AtomicPtr<()>,
    pending: AtomicBool,
}

impl HotFnTable {
    fn new() -> Self {
        Self {
            setup: AtomicPtr::new(std::ptr::null_mut()),
            pending: AtomicBool::new(false),
        }
    }

    fn patch(&self, game: &LoadedGame) {
        self.setup.store(game.setup as *mut (), Ordering::Release);
        self.pending.store(true, Ordering::Release);
    }

    pub fn read_setup(&self) -> GameSetupFn {
        let ptr = self.setup.load(Ordering::Acquire);
        if ptr.is_null() {
            stub_setup
        } else {
            unsafe { std::mem::transmute(ptr) }
        }
    }

    /// Returns true (and clears the flag) exactly once per successful patch,
    /// so the host applies each reload exactly once instead of every frame.
    pub fn take_pending_reload(&self) -> bool {
        self.pending.swap(false, Ordering::Acquire)
    }
}

extern "C" fn stub_setup(_engine: *mut Engine) {}

// ---------------------------------------------------------------------------
// Build + load
// ---------------------------------------------------------------------------

static PATCH_VERSION: AtomicU32 = AtomicU32::new(1);

/// A unique library filename per patch, so the OS loader (and Windows' file
/// lock on the previous copy) never gets confused about which version is
/// which.
fn versioned_lib_name(base_dir: &Path) -> PathBuf {
    let ver = PATCH_VERSION.fetch_add(1, Ordering::Relaxed);
    base_dir.join(format!("tracy_live_game_v{ver}.dll"))
}

fn workspace_dir() -> PathBuf {
    // This example's manifest dir is `Rust-Hybrid-ECS`; the workspace root
    // (where `cargo build -p tracy_live_game` should run) is the same dir.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_game_lib(release: bool) -> Result<PathBuf, String> {
    let workspace = workspace_dir();
    let target_dir = workspace.join("target");

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "-p", "tracy_live_game"])
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(&workspace);
    if release {
        cmd.arg("--release");
    }

    let output = cmd.output().map_err(|e| format!("cargo build failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build -p tracy_live_game failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let profile = if release { "release" } else { "debug" };
    Ok(target_dir.join(profile).join("tracy_live_game.dll"))
}

struct LoadedGame {
    lib: Library,
    setup: GameSetupFn,
}

fn load_game(lib_path: &Path) -> Result<LoadedGame, Box<dyn std::error::Error>> {
    // Copy to a versioned name to avoid Windows DLL file-locking/caching.
    let versioned = versioned_lib_name(lib_path.parent().unwrap());
    std::fs::copy(lib_path, &versioned)?;

    // SAFETY: we trust the tracy_live_game library (it's our own code, built
    // moments ago from the same workspace).
    let lib = unsafe { Library::new(&versioned)? };

    // NOTE: this name must match the `#[no_mangle]` name in
    // `tracy_live_game/src/lib.rs` exactly.
    let setup = unsafe {
        let setup: Symbol<GameSetupFn> = lib.get(b"game_setup")?;
        *setup
    };

    Ok(LoadedGame { lib, setup })
}

fn build_and_load(release: bool) -> Result<LoadedGame, String> {
    let lib_path = build_game_lib(release)?;
    load_game(&lib_path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Keeps the hot table alive, plus everything that must outlive the running
/// host (the file watcher and every loaded library, so in-flight calls into
/// an "old" version never jump into unmapped memory).
pub struct HotGame {
    pub table: Arc<HotFnTable>,
    _old_libraries: Arc<Mutex<Vec<Library>>>,
    _watcher: notify::RecommendedWatcher,
}

/// Build `tracy_live_game` once (blocking), start watching its sources, and
/// return the shared table the host reads from.
pub fn start(release: bool) -> HotGame {
    let table = Arc::new(HotFnTable::new());
    let old_libraries: Arc<Mutex<Vec<Library>>> = Arc::new(Mutex::new(Vec::new()));

    match build_and_load(release) {
        Ok(game) => {
            table.patch(&game);
            old_libraries.lock().unwrap().push(game.lib);
            println!("[hot] tracy_live_game loaded (v{})", PATCH_VERSION.load(Ordering::Relaxed));
        }
        Err(e) => eprintln!("[hot] initial build of tracy_live_game failed:\n{e}"),
    }

    let watch_dir = workspace_dir().join("examples").join("tracy_live_game").join("src");
    let table_for_watch = table.clone();
    let old_libraries_for_watch = old_libraries.clone();

    let watcher = crate::watch::spawn(watch_dir, "rs", move || {
        println!("[hot] change detected — rebuilding tracy_live_game...");
        match build_and_load(release) {
            Ok(game) => {
                table_for_watch.patch(&game);
                old_libraries_for_watch.lock().unwrap().push(game.lib);
                println!("[hot] PATCHED (v{})", PATCH_VERSION.load(Ordering::Relaxed));
            }
            Err(e) => eprintln!("[hot] rebuild failed:\n{e}"),
        }
    })
    .expect("failed to start tracy_live_game watcher");

    HotGame {
        table,
        _old_libraries: old_libraries,
        _watcher: watcher,
    }
}
