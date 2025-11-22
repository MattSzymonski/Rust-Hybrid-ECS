// ============================================================================
// Archetype-based ECS - Example Selector
// ============================================================================
//! Main entry point that allows users to select which example to run.

pub mod archetype;
pub mod commands;
pub mod component;
pub mod engine;
pub mod entity;
pub mod query;
pub mod system;
pub mod world;

// Re-export commonly used types
pub use commands::{CommandQueue, Commands};
pub use component::Component;
pub use engine::Engine;
pub use entity::Entity;
pub use query::{GlobalComponentQuery, Query, WorldQuery};
pub use system::{State, SystemState};
pub use world::{EntityBuilder, World};

use std::io::{self, Write};

// Include example modules
mod examples {
    pub mod simple_example;
    pub mod stress_test;
}

fn main() {
    println!("╔════════════════════════════════════════════════════╗");
    println!("║   Archetype-Based ECS - Example Selector          ║");
    println!("╚════════════════════════════════════════════════════╝\n");

    println!("Available examples:");
    println!("  1. Simple Example   - Basic ECS usage with 4 entities");
    println!("  2. Stress Test      - Performance test with 10,000 entities");
    println!("  0. Exit\n");

    loop {
        print!("Enter your choice (0-2): ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        match input.trim() {
            "1" => {
                println!("\n{}\n", "=".repeat(60));
                examples::simple_example::main();
                println!("\n{}\n", "=".repeat(60));
                break;
            }
            "2" => {
                println!("\n{}\n", "=".repeat(60));
                examples::stress_test::main();
                println!("\n{}\n", "=".repeat(60));
                break;
            }
            "0" => {
                println!("\nExiting...");
                break;
            }
            _ => {
                println!("Invalid choice. Please enter 0, 1, or 2.\n");
                return;
            }
        }
    }
}
