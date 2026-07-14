//! # tracy_live_game — hot-reloadable ECS game logic
//!
//! Compiled as a **cdylib** and loaded at runtime by the `tracy_live` example
//! host (`examples/tracy_live/main.rs`), mirroring `hot_reloading/game_rs`'s
//! pattern: edit this crate's source while the host runs, save, and the host
//! rebuilds + hot-swaps the game on the next check. No restart.
//!
//! ## Reload model: reset, not persist
//!
//! Unlike `game_rs` (which keeps Flappy's gameplay state alive across
//! reloads in a fixed-size buffer), this crate owns the `World` setup
//! entirely — components, systems, *and* entity spawning all live here.
//! Every reload calls [`Engine::reset_world`], which drops the old `World`
//! and its registered systems, then this crate rebuilds everything from
//! scratch. That sidesteps a real hazard: Rust's `TypeId` for a type defined
//! in this crate is not guaranteed stable across separate rebuilds of the
//! cdylib, so component data spawned under the *old* build's `Position`
//! `TypeId` would become unreachable under the *new* build's `Position`
//! `TypeId` if we tried to keep it around. Resetting avoids that entirely,
//! at the cost of restarting the simulation (respawning entities) on every
//! save — fine for a synthetic profiling demo like this one.
//!
//! ## ABI
//!
//! This crate links `ecs_hybrid` directly as a normal Rust dependency (not a
//! hand-mirrored `repr(C)` struct like `game_rs::api::EngineApi`) — `Engine`,
//! `World`, `Query`, `Commands` etc. are too generic to flatten into a
//! C-friendly function table without gutting the ECS's performance model.
//! The host and this crate are built from the same workspace, so they share
//! an identical compiled definition of every `ecs_hybrid` type; the only
//! genuine FFI boundary is the single `#[no_mangle] extern "C"` entry point
//! below, which the host calls through a `libloading`-obtained function
//! pointer.
//!
//! ## Tracy caveat
//!
//! This crate deliberately does **not** enable `ecs_hybrid`'s `tracy`
//! feature. `tracy-client`'s `Client` is a process-wide `OnceLock`; since a
//! `cdylib` statically links its own copy of `ecs_hybrid`, turning `tracy`
//! on here as well as in the host would start two independent Tracy clients
//! in one process. With it off, the `profile_scope!`/`profile_message!`
//! calls inside `game.rs`'s systems compile to no-ops — the host's
//! per-system zones (`engine.rs`'s `"system: {name}"` spans) still show
//! every system in Tracy, since that instrumentation lives in host-compiled
//! code.

mod game;

use ecs_hybrid::Engine;

/// Called once at startup and again after every hot-reload.
///
/// Resets the world, then registers components, systems, and the initial
/// entity population — see `game::setup`.
#[no_mangle]
pub extern "C" fn game_setup(engine: *mut Engine) {
    if engine.is_null() {
        return;
    }
    let engine = unsafe { &mut *engine };
    engine.reset_world();
    game::setup(engine);
}
