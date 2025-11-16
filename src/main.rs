mod ecs_core;
mod example;

fn main() {
    // Check command line arguments
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "unsafety" {
        // Run the performance test
        example::run_unsafety_test();
    } else {
        // Run the basic console example
        //example::run_example();

        println!("\n\n==========");
        println!("\nTo run the unsafety test, run:");
        println!("  cargo run -- unsafety");
        println!("==========\n");
    }
}
