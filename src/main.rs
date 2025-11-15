mod ecs_core;
mod example;
mod renderer;

fn main() {
    // Check command line arguments
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "render" {
        // Run the rendering example with proper graphics initialization
        flo_draw::with_2d_graphics(|| {
            example::run_rendering_example();
        });
    } else if args.len() > 1 && args[1] == "perf" {
        // Run the performance test
        example::run_performance_test();
    } else {
        // Run the basic console example
        example::run_example();

        println!("\n\n==========");
        println!("To see the sprite rendering demo, run:");
        println!("  cargo run -- render");
        println!("\nTo run the performance test, run:");
        println!("  cargo run -- perf");
        println!("==========\n");
    }
}
