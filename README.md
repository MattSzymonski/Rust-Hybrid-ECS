- Hot-reloading (standalone console, standalone window, editor)
    - Engine
    - Project
    - Renderer
- Speed of loading (especially editor)
- Persistance/serialization works









The standalone host is a generic launcher. It reads its configuration from
`modules/pill_config.yaml` (run from `modules/`), which declares the project
directory and the optional modules to load; a plain `cargo run --package
pill_standalone` therefore needs no environment variables. Everything else —
backend, name, watch directory, build command, and output paths — is inferred
from the project's manifest, so no project identity is compiled into the host.

The environment can still override the file for a single run:

- `PROJECT_PATH` overrides `project` in `pill_config.yaml`.
- `PILL_MODULES` overrides `modules` in `pill_config.yaml`.

Rust project (headless):

- `cargo run --package pill_standalone` (uses `pill_config.yaml`)
- or `$env:PROJECT_PATH = "../examples/project_rs"; cargo run --package pill_standalone`

Rust project (windowed): run
`cargo run --package pill_standalone --features rendering`.

C# project: point `project` in `pill_config.yaml` (or `PROJECT_PATH`) at the
directory containing the `.csproj`,
e.g. `../examples/project_cs`.

## Optional modules

Optional engine modules are crates inside `modules/optional/`, built as `cdylib`
and loaded by the host next to the project. Each is watched, rebuilt and swapped
on its own, so editing one module reloads only that module and leaves the
project and every other module running.

The `modules` list in `pill_config.yaml` selects which ones to load (in order),
and `PILL_MODULES` overrides it as a comma-separated list of crate directory
names. It defaults to `pill_test`; an empty value loads none.

- `modules: ["pill_spline"]` in `pill_config.yaml` — load the spline module.
- `$env:PILL_MODULES = "pill_test,pill_physics"` — several modules.
- `$env:PILL_MODULES = ""` — run with no optional modules.

### Adding a module

The workspace manifest globs `optional/*`, so a module is discovered by
existing; nothing lists it by name.

1. Create `modules/optional/<name>/` with a `Cargo.toml` declaring
   `crate-type = ["cdylib", "rlib"]` and depending on `pill_engine`.
2. Export `pill_module_abi_version` and `pill_module_init`, optionally
   `pill_module_update`.
3. Add `<name>` to `PILL_MODULES`.

Everything else — watch directory, build command, output path — is derived from
the directory name, so the host needs no changes. See
`modules/optional/pill_test` for the reference implementation and
`local/documents/modularity_implementation_plan.md` for the design and its
constraints.

Modules must live in this directory rather than anywhere on disk: sharing the
workspace lockfile is what makes a module resolve the same dependency graph as
the host, which keeps component type identities and the mangled symbols of the
shared `pill_core` library in agreement.

### Launching outside Cargo

The engine workspace links `pill_core` dynamically so the host and every loaded
module share one copy of its telemetry state. That also makes the built
executable depend on the toolchain's `std-<hash>.dll` at load time. The build
script in `pill_standalone` (and `pill_editor`) stages that dylib next to the
executable automatically, so `target\debug\pill_standalone.exe` runs directly
without any `PATH` adjustment:

```powershell
.\target\debug\pill_standalone.exe
```

If the staged dylib is missing (for example the toolchain was updated and the
binary was not rebuilt), the build script re-copies it on the next `cargo build`.

- `cargo run --package editor` — Run the Rust project in the editor.

- `dotnet build examples/project_cs/project_cs.csproj -c Release --nologo` — Build the C# project and its `csharp_runtime` dependency.

- `dotnet run --project modules/pill_csharp_runtime/tests/csharp_runtime_tests.csproj -c Release` — Run the C# system discovery and scheduler-access tests.

- `cargo test --workspace` — Run all Rust workspace tests.

- `cargo check --workspace` — Type-check the complete Rust workspace without producing release binaries.

