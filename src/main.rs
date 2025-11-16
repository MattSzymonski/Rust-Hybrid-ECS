mod ecs_core;
mod example;
mod world_view_example;

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("Usage:");
        println!("  cargo run -- unsafety       (Run unsafe aliasing demonstration)");
        println!("  cargo run -- safe-view      (Run safe split borrows with WorldView - HAS UB!)");
        println!("  cargo run -- truly-safe     (Run truly safe isolated updates - NO ALIASING)");
        println!(
            "  cargo run -- refcell-comp   (Run RefCell at component level - MUTABLE ACCESS!)"
        );
        println!("  cargo run -- show-ub        (Demonstrate UB examples with aliasing)");
        return;
    }

    match args[1].as_str() {
        "unsafety" => {
            example::run_unsafety_test();
        }
        "world-view" => {
            world_view_example::run_world_view_test();
        }
        _ => {
            println!("Unknown mode: {}", args[1]);
            println!("Use 'unsafety', 'safe-view', 'truly-safe', 'refcell-comp', or 'show-ub'");
        }
    }
}
