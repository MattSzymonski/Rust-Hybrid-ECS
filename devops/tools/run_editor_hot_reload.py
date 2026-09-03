#!/usr/bin/env python3
"""
REQUIREMENTS:
  - Windows (the editor's development loop targets Windows/MSVC).
  - Python 3.10 or newer.
  - Rust toolchain with the `editor` crate under modules/pill_editor.
  - `dx` (dioxus-cli) version 0.7.10 matching dioxus 0.7.10; install with
    `cargo install dioxus-cli --version 0.7.10 --locked`.

DESCRIPTION:
  Run the Pill editor in development mode with Dioxus rsx hot reload via
  `dx serve` (the dioxus CLI's "build, watch, and serve" command). The rsx
  hot-reload machinery lives in dx's dev server, NOT in `dx run`: dx run never
  starts the file watcher, so saved markup edits do nothing there. Plain
  `cargo run -p editor` cannot hot-reload at all, because Dioxus 0.7 only
  activates hot reload when the CLI launches the app (it sets the DIOXUS_*
  devserver environment variables and compiles with the `dioxus_hot_reload`
  cfg). The engine's own module hot reload (examples/project_rs, optional
  modules) is unaffected and keeps working exactly as under `cargo run`.

  dx needs three machine-specific workarounds on this repository, and this
  script applies all of them for the duration of the session:
    1. The user-level ~/.cargo/config.toml sets `rustc-wrapper = "sccache"`.
       dx composes `sccache dx.exe rustc -vV` (it proxies rustc through its
       own binary) and sccache cannot treat dx.exe as a compiler. The wrapper
       line is commented out before dx starts and restored afterwards.
    2. The repository config sets `linker = "rust-lld"`; dx's linker proxy
       then runs lld as the generic driver ("lld is a generic driver"). The
       MSVC linker is overridden to link.exe via the environment.
    3. dx stages the built executable under target/dx/.../app WITHOUT copying
       the engine dylibs (pill_core.dll, the std dylib) beside it, so the app
       exits with 0xc0000135 (DLL not found). The dx build directory
       (target/x86_64-pc-windows-msvc/desktop-dev and its deps subfolder) is
       prepended to PATH so the launched app can resolve them.

USAGE:
  python devops/tools/run_editor_hot_reload.py [options] [-- dx arguments...]

FLAGS:
  --project PATH   Project module directory used for the PROJECT_PATH
                   environment variable (default: <repo>/examples/project_rs).
                   Accepts absolute paths or paths relative to the repo root.
  --dx PATH        Full path to the dx executable when it is not on PATH.
  --port PORT      Port for the dioxus dev server (default: 34115). Pick one
                   that is free on the machine.
  -h, --help       Print this help and exit.

  Anything after `--` is forwarded to dx serve verbatim.

EXAMPLE USAGE:
  python devops/tools/run_editor_hot_reload.py
  python devops/tools/run_editor_hot_reload.py --project examples/project_cs
--- SCRIPT ---
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parents[2]
EDITOR_DIRECTORY = REPO_ROOT / "modules" / "pill_editor"
DEFAULT_PROJECT = REPO_ROOT / "examples" / "project_rs"
DX_BUILD_DIRECTORY = (
    REPO_ROOT
    / "modules"
    / "target"
    / "x86_64-pc-windows-msvc"
    / "desktop-dev"
)
DX_DEPS_DIRECTORY = DX_BUILD_DIRECTORY / "deps"

# The dioxus dev server port. Picked away from the default 8080 (the local
# tooling server already uses it) and from other common dioxus ports.
DEFAULT_DEV_SERVER_PORT = "34115"

# dx builds under a launcher-injected profile and that profile name becomes the
# module-build profile; the repo's rust-lld linker is incompatible with dx's
# linker proxy, so the standard MSVC linker is forced through the environment.
CARGO_LINKER_ENVIRONMENT_KEY = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"
CARGO_LINKER_VALUE = "link.exe"

# A marker comment used when disabling the sccache wrapper, so a later crash
# leaves the config in a state that is easy to spot and revert by hand.
SCCACHE_WRAPPER_MARKER = '# rustc-wrapper = "sccache" (disabled by run_editor_hot_reload.py)'

# Matches one uncommented `rustc-wrapper = "sccache"` line (with optional
# leading whitespace) and captures its own line ending so it can be preserved.
SCCACHE_WRAPPER_PATTERN = re.compile(
    r'^([ \t]*)rustc-wrapper\s*=\s*"[^"]*"[^\r\n]*(?:\r\n|\n|$)',
    re.MULTILINE,
)


# ---------------------------------------------------------------------------
# Cargo config helper (temporary sccache wrapper disable)
# ---------------------------------------------------------------------------

def cargo_config_path() -> Path:
    """Return the user-level cargo config file path."""
    home = Path.home()
    return home / ".cargo" / "config.toml"


def disable_sccache_wrapper(config_path: Path) -> bytes | None:
    """Comment out the sccache rustc-wrapper line and return the original text.

    Returns the original file bytes when the file was changed, and None when
    there was nothing to change (missing file or no active wrapper line).

    The file is rewritten as bytes so the other lines keep their exact line
    endings: a text-mode write on Windows would translate every `\\n` to
    `\\r\\n` and double the `\\r` of an already-CRLF file, corrupting it.
    """
    if not config_path.is_file():
        return None
    original_bytes = config_path.read_bytes()
    text = original_bytes.decode("utf-8", errors="replace")

    def replace_wrapper_line(match: re.Match) -> str:
        indent = match.group(1)
        line_text = match.group(0)
        ending = "\r\n" if line_text.endswith("\r\n") else (
            "\n" if line_text.endswith("\n") else ""
        )
        return indent + SCCACHE_WRAPPER_MARKER + ending

    new_text, replacements = SCCACHE_WRAPPER_PATTERN.subn(
        replace_wrapper_line, text, count=1
    )
    if replacements == 0:
        return None
    config_path.write_bytes(new_text.encode("utf-8"))
    return original_bytes


def restore_cargo_config(config_path: Path, original_bytes: bytes | None) -> None:
    """Restore the cargo config to its original content, if it was modified."""
    if original_bytes is None:
        return
    try:
        config_path.write_bytes(original_bytes)
    except OSError as error:
        print(f"[run_editor_hot_reload] warning: could not restore {config_path}: {error}")


# ---------------------------------------------------------------------------
# Environment preparation
# ---------------------------------------------------------------------------

def build_environment(project_directory: Path) -> dict:
    """Return the child environment with the dx workarounds applied."""
    environment = os.environ.copy()
    environment["PROJECT_PATH"] = str(project_directory)
    environment[CARGO_LINKER_ENVIRONMENT_KEY] = CARGO_LINKER_VALUE
    dll_directories = [str(DX_BUILD_DIRECTORY), str(DX_DEPS_DIRECTORY)]
    existing_path = environment.get("PATH", "")
    environment["PATH"] = os.pathsep.join(dll_directories + [existing_path])
    return environment


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def print_help() -> None:
    """Print the module docstring as usage help."""
    print(__doc__)


def parse_arguments(arguments: list) -> tuple:
    """Split script options from dx-forwarded arguments."""
    project_directory = DEFAULT_PROJECT
    dx_executable = "dx"
    dev_server_port = DEFAULT_DEV_SERVER_PORT
    remaining = list(arguments)
    forwarded = []
    while remaining:
        argument = remaining.pop(0)
        if argument == "--":
            forwarded.extend(remaining)
            break
        if argument in ("-h", "--help"):
            return None, None, None, None, None
        if argument == "--project":
            if not remaining:
                raise SystemExit("--project requires a path argument")
            project_directory = Path(remaining.pop(0))
        elif argument == "--dx":
            if not remaining:
                raise SystemExit("--dx requires a path argument")
            dx_executable = remaining.pop(0)
        elif argument == "--port":
            if not remaining:
                raise SystemExit("--port requires a number argument")
            dev_server_port = remaining.pop(0)
        else:
            forwarded.append(argument)
    if not project_directory.is_absolute():
        project_directory = (REPO_ROOT / project_directory).resolve()
    return dx_executable, project_directory, forwarded, dev_server_port, False


def main() -> int:
    (dx_executable, project_directory, forwarded_arguments, dev_server_port,
     _) = parse_arguments(sys.argv[1:])
    if dx_executable is None:
        print_help()
        return 0

    if not project_directory.is_dir():
        print(
            f"[run_editor_hot_reload] error: project directory does not exist: "
            f"{project_directory}"
        )
        return 2

    config_path = cargo_config_path()
    original_config = disable_sccache_wrapper(config_path)
    if original_config is not None:
        print(
            "[run_editor_hot_reload] sccache rustc-wrapper disabled for this "
            "session; the cargo config is restored when dx exits."
        )

    # `dx serve` is the dioxus CLI command that starts the file watcher and
    # the dev server; `dx run` never watches, so hot reload cannot work there.
    command = [
        dx_executable,
        "serve",
        "--watch",
        "true",
        "--hot-reload",
        "true",
        "--open",
        "false",
        "--port",
        dev_server_port,
    ] + forwarded_arguments
    environment = build_environment(project_directory)
    print(f"[run_editor_hot_reload] starting: {' '.join(command)}")
    print(f"[run_editor_hot_reload] project:   {project_directory}")
    print(f"[run_editor_hot_reload] working dir: {EDITOR_DIRECTORY}")
    print("[run_editor_hot_reload] press Ctrl+C to stop dx (config is restored).")

    process = subprocess.Popen(
        command,
        cwd=str(EDITOR_DIRECTORY),
        env=environment,
    )
    try:
        process.wait()
    except KeyboardInterrupt:
        print("\n[run_editor_hot_reload] stopping dx...")
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
    finally:
        restore_cargo_config(config_path, original_config)

    return process.returncode if process.returncode is not None else 1


if __name__ == "__main__":
    sys.exit(main())
