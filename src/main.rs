use std::io::{self, Write};

// ----------------------------------------------------------------------------
// Archetype-based ECS - Stress Test
// ----------------------------------------------------------------------------

// Include example modules
mod examples;

fn main() {
    // Picker for selecting which example to run
    println!("Select an example to run:");
    println!("1. Iterators Stress Test");
    println!("2. Scripting Basic");
    println!("3. Parallel Systems");
    println!("4. Change Detection");
    println!("5. Resources + Components + Systems");
    print!("Enter your choice (1-5): ");

    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    match input.trim() {
        "1" => examples::iterators_stress_test::main(),
        "2" => examples::scripting_demo::main(),
        "3" => examples::parallel_systems_demo::main(),
        "4" => examples::change_detection_demo::main(),
        "5" => examples::resources_demo::main(),
        _ => println!("Invalid choice!"),
    }
}
