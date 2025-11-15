mod components;
mod ecs_core;
mod example;
mod renderer;

fn main() {
    // Check command line arguments
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "perfscripts" {
        // Run the performance test
        example::run_performance_test_scripts();
    } else if args.len() > 1 && args[1] == "perfsystems" {
        // Run the performance test with pure ECS systems
        example::run_performance_test_systems();
    } else {
        // Run the basic console example
        //example::run_example();

        println!("\n\n==========");
        println!("\nTo run the performance test (scripts approach), run:");
        println!("  cargo run -- perfscripts");
        println!("\nTo run the performance test (systems approach), run:");
        println!("  cargo run -- perfsystems");
        println!("==========\n");
    }
}
