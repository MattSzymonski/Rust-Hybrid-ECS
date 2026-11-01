# Test fixtures

Three projects, one per kind of anchor. Every suite drives one of them, and no
suite spells a path to one: the roots live in `devops/core/projects.py` as the
role groups `NATIVE_PROJECT_*`, `SCENE_PROJECT_*` and `MANAGED_PROJECT_*`
(`test_suite_standards.py`'s rule 5 refuses a path written inside a suite).

A fixture here is test-owned rather than an example: a suite edits its sources,
asserts its log lines and restores them, so a clean checkout can run the gate
without a developer's `examples/local` checkout.

| Fixture    | Kind                     | What it anchors                                                                                                                                                                                                                                                                                                                                                 |
| ---------- | ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `native/`  | Rust (`cdylib` + `rlib`) | Everything that mutates and reloads: hot reload and hot patching, schema migration, a module's data surviving a reload, a forgotten type's column being dropped, the committed render chain, and the shipping bundle. Loads `pill_spline` and `pill_audio`.                                                                                                          |
| `scene/`   | Rust (`cdylib` + `rlib`) | Everything read-mostly: the editor, scene picking, extra surfaces, the browser build, asset reimport, and the UI theme pipeline. Four entities (camera, three rotating models), one mesh drawn three times, one committed material, a sample map no material binds, a themed UI layer, one committed render pass and an editor project layer. Loads no modules. |
| `managed/` | C#                       | The Rust/C# bridge, the scripting analyzers and the managed iteration benchmark. One project: the bridge surface (components, a resource, systems, asset imports), the extension mirrors, and three gated query shapes that measure only under `PILL_BENCH_ENTITIES`.                                                                                           |

Together the three weigh 2.3 MB in 96 files - `native` 110 KB, `scene` 1.1 MB,
`managed` 1.0 MB - against the 37.4 MB the seven projects they replaced held.
Build output (`target/`, `build/`) is not counted and is gitignored.

## Conventions

- A fixture is named after its directory (`native`, `scene`, `managed`), because
  the host keys staged DLLs by package name: a project named after something
  else would share a DLL with whichever fixture carried that name.
- `res` is the project's asset root. `res/editor/` is never packed into a game
  build, so an editor theme stays a development-time document.
- Assets that have a `.meta` sidecar keep it; the store resolves their identity
  through it, and the asset suites edit, corrupt, move and restore those files.
- A managed fixture's `modules:` list drives C# mirror generation. List only
  modules that expose components or mirrored value types.
