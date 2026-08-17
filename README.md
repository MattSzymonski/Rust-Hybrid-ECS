- Hot-reloading (standalone console, standalone window, editor)
    - Engine
    - Project
    - Renderer
- Speed of loading (especially editor)
- Persistance/serialization works









- `cargo run --package standalone` — Run the Rust project in the headless standalone host.

- `cargo run --package standalone --features rendering` — Run the Rust project in the standalone host with rendering enabled.

- `cargo run --package editor` — Run the Rust project in the editor.

- `$env:ECS_HOT_RELOAD_MODULE="csharp"; cargo run --package standalone` — Build and run the C# project through `csharp_runtime` in the standalone host.

- `$env:ECS_HOT_RELOAD_MODULE="rs"; cargo run --package standalone` — Build and run the rust project in the standalone host.

- `dotnet build project_cs/project_cs.csproj -c Release --nologo` — Build the C# project and its `csharp_runtime` dependency.

- `dotnet run --project csharp_runtime/tests/csharp_runtime_tests.csproj -c Release` — Run the C# system discovery and scheduler-access tests.

- `cargo test --workspace` — Run all Rust workspace tests.

- `cargo check --workspace` — Type-check the complete Rust workspace without producing release binaries.

