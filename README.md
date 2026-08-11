- `cargo run --package standalone` — Run the Rust game in the headless standalone host.

- `cargo run --package standalone --features rendering` — Run the Rust game in the standalone host with rendering enabled.

- `cargo run --package editor` — Run the Rust game in the editor.

- `$env:ECS_HOT_RELOAD_MODULE="csharp"; cargo run --package standalone` — Build and run the C# game through `csharp_runtime` in the standalone host.

- `$env:ECS_HOT_RELOAD_MODULE="rs"; cargo run --package standalone` — Build and run the rust game in the standalone host.

- `dotnet build game_cs/game_cs.csproj -c Release --nologo` — Build the C# game and its `csharp_runtime` dependency.

- `dotnet run --project csharp_runtime/tests/csharp_runtime_tests.csproj -c Release` — Run the C# system discovery and scheduler-access tests.

- `cargo test --workspace` — Run all Rust workspace tests.

- `cargo check --workspace` — Type-check the complete Rust workspace without producing release binaries.

