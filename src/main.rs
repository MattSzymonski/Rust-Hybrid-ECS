use std::io::{self, Write};

// ============================================================================
// Archetype-based ECS - Stress Test
// ============================================================================

// Include example modules
mod examples;

fn main() {
    // Picker for selecting which example to run
    println!("Select an example to run:");
    println!("1. Iterators Stress Test");
    println!("2. Scripting Basic");
    print!("Enter your choice (1-2): ");

    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    match input.trim() {
        "1" => examples::iterators_stress_test::main(),
        "2" => examples::scripting_basic::main(),
        _ => println!("Invalid choice!"),
    }
}
