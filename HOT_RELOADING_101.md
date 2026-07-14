# Hot Reloading 101

How `examples/tracy_live` reloads its game logic — components, systems, and
entity spawning — while it's running, without a process restart. Edit
`examples/tracy_live_game/src/game.rs`, save, and the running process picks
it up on its own.

This pattern was ported from `Scripting-Language-Tests/hot_reloading`'s
`flappy` / `game_rs` split and adapted to this ECS's generic `Query`/`Commands`
API. If you want to make another example hot-reloadable, this is the doc to
copy from.

## Running it

```sh
cargo run --example tracy_live --release --features tracy
```

- `--release` — this is a profiling/stress demo (30000 entities); debug
  builds will still work but run much slower.
- `--features tracy` — optional. Without it, `profile_*!` calls in the host
  no-op and the demo just runs standalone; with it, start the Tracy GUI
  ([releases](https://github.com/wolfpld/tracy/releases)) first, then click
  **Connect** once the process prints "Connect Tracy now."

The first run builds both `ecs_hybrid` (host) and `tracy_live_game` (guest) —
expect a normal `cargo build`-length wait, plus the console printing
`[hot] tracy_live_game loaded (v1)` once the guest's first build finishes.
After that, every 2 seconds you'll see:

```
  428 FPS |  30000 entities
```

To hot-reload: with it still running, open
`examples/tracy_live_game/src/game.rs`, change something (tweak
`gravity_system`'s formula, `health_decay_system`'s increment, comment a
system out of `setup`, change the spawn count), and save. Watch the console:

```
[hot] change detected — rebuilding tracy_live_game...
[hot] PATCHED (v2)
[hot] applying reload...
```

The FPS/entity-count line keeps printing right after, now reflecting the new
code — the world was reset and respawned per the new `game.rs` (see "Reset,
not persist" below). A compile error in `game.rs` prints the `cargo` error
to the console instead and leaves the previously-loaded version running, so
the process never crashes from a bad edit.

Stop the process with Ctrl+C as usual.

## Why this needs an ABI boundary at all

Hot reloading in Rust means: compile part of your program into a `cdylib`,
load it at runtime with `libloading`, and swap it for a newer build without
restarting the host process. Rust has no stable ABI across separate
compilations in general — but two crates built **from the same workspace, in
the same build**, share bit-for-bit identical type layouts, because Cargo
just reuses the same compiled artifact for both. That's the loophole this
relies on: the host and the reloadable crate both depend on `ecs_hybrid`
(unchanged between reloads), so passing real Rust references to `Engine` /
`World` across the `cdylib` boundary is safe, not just "probably fine."

The only place a true FFI boundary is required is the handful of
`#[no_mangle] pub extern "C" fn` entry points the host calls through — those
need a stable calling convention and no generics, because the host resolves
them by name via `libloading` at runtime instead of the normal static linker.

## The three pieces

```
Rust-Hybrid-ECS/
  Cargo.toml                          <- workspace root (root package + members)
  src/                                 <- ecs_hybrid: the ECS itself, never reloaded
  examples/
    tracy_live/
      main.rs                         <- host: owns Engine, runs the frame loop
      hot.rs                          <- host: build + load + hot-swap the dylib
      watch.rs                        <- host: generic "watch a dir, debounce, callback"
    tracy_live_game/                  <- guest: the reloadable cdylib
      Cargo.toml                      <- crate-type = ["cdylib"]
      src/
        lib.rs                        <- the one exported extern "C" fn
        game.rs                       <- components, systems, spawning — edit this
```

| Crate             | Rebuilt on every save? | Owns                                                          |
| ------------------ | ----------------------- | -------------------------------------------------------------- |
| `ecs_hybrid`       | No                       | The ECS engine itself (`Engine`, `World`, `Query`, scheduler…) |
| `tracy_live` (host) | No                       | The frame loop, Tracy setup, the hot-reload harness            |
| `tracy_live_game`  | **Yes**                  | Components, systems, and initial entity spawning               |

## What actually happens on save

1. `watch.rs` watches `examples/tracy_live_game/src` for `*.rs` changes on a
   background thread, debouncing bursts of editor save events.
2. On a change, `hot.rs` runs `cargo build -p tracy_live_game` as a
   subprocess. If it fails to compile, the error is printed and the **old**
   code just keeps running untouched — no crash, no partial state.
3. On success, the new `.dll` is copied to a version-numbered filename
   (`tracy_live_game_v7.dll`, …) before loading — Windows locks the previous
   file while it's mapped, so reusing the same name would fail.
4. `libloading::Library::new` loads it, and the `game_setup` symbol is
   resolved and stashed in a lock-free table (`AtomicPtr` + a `pending` flag)
   that only the watcher thread writes to.
5. The host's frame loop, on its own thread, checks that `pending` flag once
   per iteration. When set, it calls the new `game_setup(&mut engine)` and
   clears the flag. This is the only place the new code is actually invoked —
   keeping all `Engine` mutation on one thread avoids any data race between
   the watcher and the running simulation.
6. Every loaded `Library` is kept alive forever in a `Vec` (never unloaded).
   Once a reload discards the old systems, nothing calls into their code
   again — but leaking the mapping is one less thing to get subtly wrong.

## Reset, not persist

Flappy's hot-reload keeps gameplay state (bird position, score) alive across
reloads, because it stores it in a fixed-size `repr(C)` buffer the host
allocates once. `tracy_live` doesn't do that — a reload **resets the whole
world** and respawns entities from scratch.

That's a deliberate trade, not a limitation carried over by accident: Rust's
`TypeId` for a type defined inside `tracy_live_game` is not guaranteed to
stay the same across separate rebuilds of that crate. If entities spawned
under the *old* build's `Position` `TypeId` were kept around after loading
the *new* build, they'd be silently unreachable under the new `Position`'s
`TypeId` — a real, easy-to-miss bug. Resetting the world on every reload
sidesteps it entirely, at the cost of restarting the simulation each time you
save. For a synthetic profiling demo that spawns a fresh entity population
every run anyway, that's a fine trade. It also means you're free to edit
*component definitions* here, not just system bodies — Flappy's split can't
offer that without the same `TypeId` risk.

`Engine::reset_world()` (in `src/engine.rs`) is the piece that makes this
possible: it clears registered systems, resets the scheduler, and replaces
the `World`, while leaving host-level config (parallel execution, FPS limit)
alone.

## The Tracy caveat

`tracy_live_game`'s `Cargo.toml` deliberately does **not** enable
`ecs_hybrid`'s `tracy` feature. `tracy-client`'s `Client` is a process-wide
`OnceLock` — but a `cdylib` statically links its own copy of every
dependency, including `ecs_hybrid`. Enabling `tracy` in both the host and the
guest would start two independent Tracy clients in one process, each trying
to open its own connection.

With it off in the guest, the `profile_scope!`/`profile_message!` calls
inside `game.rs`'s systems compile to no-ops. You still get full visibility
in Tracy: the host wraps every system call in its own zone
(`"system: gravity"`, `"system: cleanup"`, …) regardless of where that
system's code lives, since that instrumentation runs in host-compiled code.
You just lose the extra sub-zones/messages a system chooses to emit from
inside its own body.

## Extending this to another example

1. New `cdylib` crate under `examples/<name>_game`, depending on `ecs_hybrid`
   by path (default features — leave `tracy` off, per above). Add it to the
   root `Cargo.toml`'s `[workspace] members`.
2. Move components/systems/spawning into that crate's `game.rs`; export one
   `#[no_mangle] pub extern "C" fn game_setup(*mut Engine)` from `lib.rs`.
3. Copy `examples/tracy_live/hot.rs` and `watch.rs`, swap the crate name in
   `build_game_lib`/`versioned_lib_name`/the watch directory.
4. Host `main.rs`: `let hot = hot::start(is_release);`, then each loop
   iteration: `if hot.table.take_pending_reload() { (hot.table.read_setup())(&mut engine) }`
   before `engine.process_frame()`.

If your example needs gameplay state to *survive* a reload instead of
resetting (more like Flappy), that's a materially different design — see the
"reset vs. persist" trade-off above before reaching for it.
