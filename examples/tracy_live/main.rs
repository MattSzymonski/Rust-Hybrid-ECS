// Tracy Profiling Demo — runs continuously for live profiling, with two
// interchangeable scripting backends selected at startup:
//
//   --rs_scripting   hot-reloads components/systems/spawning from
//                    `tracy_live_game`, a Rust cdylib built and watched by
//                    `hot.rs`/`watch.rs`. World resets on every reload.
//   --cs_scripting   hot-reloads `Systems.cs` from `tracy_live_game_cs`,
//                    hosted via hostfxr (`hot_cs.rs`). Entities live in
//                    Rust-owned memory the whole run, so they persist across
//                    a C# reload — only the code changes.
//
// Exactly one of the two flags must be passed.
//
// Usage:
//   1. Start Tracy GUI (Tracy.exe from https://github.com/wolfpld/tracy/releases)
//   2. cargo run --example tracy_live --release --features tracy -- --rs_scripting
//      (or --cs_scripting)
//   3. Click Connect in Tracy
//   4. Watch live CPU zones, frame times, and thread work distribution
//   5. --rs_scripting: edit examples/tracy_live_game/src/game.rs and save —
//      the running process rebuilds it, resets the world, and keeps going.
//      --cs_scripting: edit examples/tracy_live_game_cs/src/Systems.cs, then
//      run `dotnet build examples/tracy_live_game_cs -c Release` yourself in
//      another terminal — the loader picks it up within ~0.5s, no restart,
//      no world reset.
//
// Press Ctrl+C to stop.
//
// Reconnecting: after killing and restarting this program, Tracy auto-reconnects.
// If it doesn't pick up, click the "Connect" button in Tracy GUI again — sometimes
// the GUI stops listening after an abrupt disconnect.

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

    // Brief pause lets Tracy's background connection thread establish
    // the TCP link before we start flooding it with frame data.
    // Also avoids TIME_WAIT collisions on Windows after a restart.
    std::thread::sleep(std::time::Duration::from_millis(200));

    match parse_mode() {
        Mode::Rust => run_rs_scripting(),
        Mode::CSharp => run_cs_scripting(),
    }
}

fn run_rs_scripting() {
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);
    engine.trace_frame_wait = false;

    let is_release = !cfg!(debug_assertions);
    let hot = hot::start(is_release);

    println!("=== Tracy Live Profiling Demo (Rust hot-reload) ===");
    println!("6 systems, 30000 entities, parallel ON");
    println!("Edit examples/tracy_live_game/src/game.rs and save to hot-reload.");
    println!();
    println!("Connect Tracy now. Press Ctrl+C to stop.");
    println!();

    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        if hot.table.take_pending_reload() {
            println!("[hot] applying reload...");
            (hot.table.read_setup())(&mut engine as *mut Engine);
        }

        engine.process_frame().unwrap();
        count += 1;

        // Report every 2 seconds
        let dt = last_report.elapsed().as_secs_f64();
        if dt >= 2.0 {
            let fps = count as f64 / dt;
            let entities = engine.world().entity_count();
            println!("  {:>6.0} FPS | {:>5} entities", fps, entities);
            count = 0;
            last_report = Instant::now();
        }
    }
}

fn run_cs_scripting() {
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);
    engine.trace_frame_wait = false;

    cs_components::setup(&mut engine);

    let mut cs = match hot_cs::start(&mut engine) {
        Ok(cs) => cs,
        Err(e) => {
            eprintln!("failed to start C# scripting: {e}");
            std::process::exit(1);
        }
    };

    engine.print_execution_graph();

    println!("=== Tracy Live Profiling Demo (C# scripting) ===");
    println!("3 systems, 30000 entities, Rust-scheduled parallel C#");
    println!("Edit examples/tracy_live_game_cs/src/Systems.cs, then run:");
    println!("  dotnet build examples/tracy_live_game_cs -c Release");
    println!("in another terminal to hot-reload it.");
    println!();
    println!("Connect Tracy now. Press Ctrl+C to stop.");
    println!();

    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        cs.poll_reload();
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
