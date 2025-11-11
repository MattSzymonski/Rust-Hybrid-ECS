# Rust Function Overloading Pattern

A demonstration of simulating (the closest we can get) function overloading in Rust using the `Drop` trait.

## How It Works

Rust doesn't support traditional function overloading, but we can simulate it using a builder pattern combined with the `Drop` trait:

```rust
e.do_something("A").delay(); // Delayed execution
e.do_something("B");         // Immediate execution (Drop trait)
e.execute_commands();
```

## The Pattern

1. **Method returns a builder** - `do_something()` returns a `DoSomethingCall` struct
2. **Optional `.delay()`** - Explicitly queue the command for later
3. **Drop trait magic** - If `.delay()` is not called, `Drop` executes immediately at the semicolon

## Key Techniques

- **Builder Pattern** - Fluent API for method chaining
- **Drop Trait** - Automatic cleanup/execution when value goes out of scope
- **State Tracking** - `executed` flag prevents double execution

This approach provides clean syntax while maintaining Rust's safety guarantees.
