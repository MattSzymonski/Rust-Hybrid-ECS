#!/usr/bin/env python3

# REQUIREMENTS: Python 3.8+, PyYAML (for the bundle generator), the Rust
#               toolchain with the `wasm32-unknown-unknown` target
#               (`rustup target add wasm32-unknown-unknown`), and `wasm-pack`
#               on PATH (`cargo install wasm-pack`). On its first run wasm-pack
#               downloads the `wasm-bindgen` CLI matching the crate version,
#               and in a release build also `wasm-opt` unless `--no-wasm-opt`.

# DESCRIPTION: Builds a Rust project for the browser. It regenerates the
#   shipping bundle and the web app crate (`generate_shipping_bundle.py
#   --web`), builds the app for `wasm32-unknown-unknown` with wasm-pack, and
#   puts the page (`devops/tools/web/index.html`) beside the generated
#   `pkg/` in `<project>/build/web/`. Serve that directory over HTTP to play.
#
#   The web app crate has a workspace of its own and is built from its own
#   directory, so the engine workspace's `.cargo/config.toml` (whose
#   `-C prefer-dynamic` a wasm build cannot use) does not apply. The engine
#   workspace's `Cargo.lock` is copied in first, so the browser build resolves
#   the same dependency versions as the native one. `RUSTC_WRAPPER` is cleared
#   because sccache cannot spawn `web-sys`'s very long command line on Windows.
#
#   Like a release build, this regenerates `build/pill_shipping_bundle`;
#   restore the committed stub afterwards with
#   `git checkout -- build/pill_shipping_bundle`.
#
# USAGE: python devops/tools/build_web.py [--dev] [--no-wasm-opt] [project_path]
#          --dev           unoptimized build with debug info (faster to build)
#          --no-wasm-opt   skip wasm-opt in a release build
#          [project_path]  workspace-relative project directory; defaults to
#                          the PROJECT_PATH environment variable
#
# EXAMPLE USAGE:
#   python devops/tools/build_web.py examples/master_renderer_test
#   python devops/tools/build_web.py --dev examples/master_renderer_test
#   python -m http.server 8000 -d examples/master_renderer_test/build/web

# --- SCRIPT ---

# Standard library
import os
import shutil
import subprocess
import sys
from pathlib import Path

# Locations, relative to the repository root.
GENERATOR = Path("devops") / "tools" / "generate_shipping_bundle.py"
PAGE_TEMPLATE = Path("devops") / "tools" / "web" / "index.html"
WEB_APP_DIRECTORY = Path("build") / "pill_web_app"
WORKSPACE_LOCK = Path("modules") / "Cargo.lock"
WEB_TARGET_DIRECTORY = Path("build") / "build_meta" / "web_target"
# The module wasm-pack writes, named after the web app crate.
WEB_MODULE_NAME = "pill_web_app_bg.wasm"


def repository_root() -> Path:
    """The repository root, located from this script."""
    return Path(__file__).resolve().parent.parent.parent


def run(command: list, working_directory: Path, environment=None) -> int:
    """Runs `command`, echoing it first; returns its exit code."""
    print("$ " + " ".join(str(part) for part in command), flush=True)
    return subprocess.call(command, cwd=working_directory, env=environment)


def main() -> int:
    """Builds the web version of a project; returns the process exit code."""
    # Parse the flags and the optional project path.
    development = False
    wasm_opt = True
    positional = []
    for argument in sys.argv[1:]:
        if argument == "--dev":
            development = True
        elif argument == "--no-wasm-opt":
            wasm_opt = False
        else:
            positional.append(argument)
    if len(positional) > 1:
        print("usage: build_web.py [--dev] [--no-wasm-opt] [project_path]", file=sys.stderr)
        return 2
    project_path = positional[0] if positional else os.environ.get("PROJECT_PATH", "")
    if not project_path:
        print("error: no project given and PROJECT_PATH is not set", file=sys.stderr)
        return 2
    root = repository_root()
    project_root = (root / project_path).resolve()

    # Step 1: regenerate the shipping bundle and the web app crate.
    status = run([sys.executable, root / GENERATOR, "--web", project_path], root)
    if status != 0:
        return status

    # Step 2: pin the dependency versions the native build uses.
    web_app_directory = root / WEB_APP_DIRECTORY
    shutil.copyfile(root / WORKSPACE_LOCK, web_app_directory / "Cargo.lock")

    # Step 3: build for the browser. The output lands straight in the web
    # build directory; the page is copied next to it.
    output_directory = project_root / "build" / "web"
    package_directory = output_directory / "pkg"
    environment = dict(
        os.environ,
        RUSTC_WRAPPER="",
        RUSTFLAGS="",
        CARGO_TARGET_DIR=str(root / WEB_TARGET_DIRECTORY),
    )
    command = [
        "wasm-pack",
        "build",
        "--target",
        "web",
        "--no-typescript",
        "--no-pack",
        "--out-dir",
        str(package_directory),
        "--dev" if development else "--release",
    ]
    if not development and not wasm_opt:
        command.append("--no-opt")
    status = run(command, web_app_directory, environment)
    if status != 0:
        return status
    shutil.copyfile(root / PAGE_TEMPLATE, output_directory / "index.html")

    # Step 4: report where it is and how to open it.
    module_path = package_directory / WEB_MODULE_NAME
    size_megabytes = module_path.stat().st_size / (1024 * 1024)
    print(f"web build: {output_directory}")
    print(f"  {WEB_MODULE_NAME}: {size_megabytes:.1f} MB")
    print(f"  serve it with: python -m http.server 8000 -d \"{output_directory}\"")
    return 0


if __name__ == "__main__":
    sys.exit(main())
