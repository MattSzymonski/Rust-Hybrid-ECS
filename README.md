- Hot-reloading (standalone console, standalone window, editor)
    - Engine
    - Project
    - Renderer
- Speed of loading (especially editor)
- Persistance/serialization works









The standalone host is a generic launcher: it reads one variable, `PROJECT_PATH`,
which points at the project directory relative to the workspace root (run from
`modules/`). Everything else — backend, name, watch directory, build command,
and output paths — is inferred from the project's manifest, so no project
identity is compiled into the host.

Rust project (headless):

- `$env:PROJECT_PATH = "../examples/project_rs"; cargo run --package pill_standalone`

Rust project (windowed): same variable, run
`cargo run --package pill_standalone --features rendering`.

C# project: point `PROJECT_PATH` at the directory containing the `.csproj`,
e.g. `$env:PROJECT_PATH = "../examples/project_cs"; cargo run --package pill_standalone`.

- `cargo run --package editor` — Run the Rust project in the editor.

- `dotnet build examples/project_cs/project_cs.csproj -c Release --nologo` — Build the C# project and its `csharp_runtime` dependency.

- `dotnet run --project modules/pill_csharp_runtime/tests/csharp_runtime_tests.csproj -c Release` — Run the C# system discovery and scheduler-access tests.

- `cargo test --workspace` — Run all Rust workspace tests.

- `cargo check --workspace` — Type-check the complete Rust workspace without producing release binaries.

