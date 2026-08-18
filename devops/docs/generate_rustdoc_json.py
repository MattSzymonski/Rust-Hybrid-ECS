"""Generate nightly rustdoc JSON for every package in this Cargo workspace.

Unlike ``cargo doc --workspace``, ``cargo rustdoc`` accepts custom rustdoc
arguments but operates on only one selected package target at a time. This
script bridges that gap: it discovers the workspace members, selects the
appropriate documentation target for each package, and invokes nightly
rustdoc once per target.

Rustdoc's JSON format is unstable, so both a nightly toolchain and Cargo's
``-Z unstable-options`` switch are required. The resulting files contain
structured API data for downstream documentation tools; they are not directly
viewable replacements for rustdoc's HTML pages.

Requirements:
  - Python 3.8+
  - Rustup with the nightly toolchain installed

Usage:
  python devops/docs/generate_rustdoc_json.py
  python devops/docs/generate_rustdoc_json.py --dry-run

Output:
  target/doc/<crate_name>.json
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List, Sequence, Tuple


# Resolve paths from this file instead of the caller's current directory. This
# lets developers invoke the script from the workspace root, another folder, or
# an IDE task without changing its behavior.
WORKSPACE_ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = WORKSPACE_ROOT / "Cargo.toml"

# Cargo metadata uses several target-kind labels for library-like targets. All
# of these are selected through Cargo's single ``--lib`` option, including
# procedural macro and C-compatible dynamic-library crates.
LIBRARY_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}

# ``profiling`` and ``profiling-minimal`` deliberately configure incompatible
# instrumentation modes in both pill_core and pill_engine. Cargo's
# ``--all-features`` enables both and triggers their compile-time guard. Build
# the nearly-all-features set explicitly so JSON docs cover full profiling and
# every other declared feature while omitting only the minimal alternative.
EXCLUDED_DOCUMENTATION_FEATURES = {"profiling-minimal"}


# =============================================================================
# Subprocess execution
# =============================================================================

# Keep external command execution in one helper so every Cargo invocation uses
# the same workspace, error behavior, encoding, and readable command logging.
def run(
    command: Sequence[str], *, capture_output: bool = False
) -> subprocess.CompletedProcess:
    """Run an external command from the Cargo workspace root.

    Args:
        command: Executable and arguments passed directly to ``subprocess``.
            A sequence is used instead of a shell string so paths and package
            names do not require platform-specific escaping.
        capture_output: Capture standard output when the caller needs to parse
            it. Standard error remains visible so Cargo warnings are not lost.

    Returns:
        The completed process. When ``capture_output`` is true, its ``stdout``
        attribute contains UTF-8 text.

    Raises:
        FileNotFoundError: The requested executable is unavailable.
        subprocess.CalledProcessError: The command exits unsuccessfully.
    """
    # Echo an exact, copyable representation before execution. Flushing keeps
    # this line ahead of Cargo output even when stdout is redirected by CI.
    print("+", subprocess.list2cmdline(command), flush=True)

    # Running from WORKSPACE_ROOT is important because Cargo discovers the
    # workspace's .cargo/config.toml by walking up from its current directory.
    return subprocess.run(
        command,
        cwd=WORKSPACE_ROOT,
        check=True,
        # Cargo metadata writes its JSON to stdout. Documentation commands keep
        # stdout attached to the terminal so build progress remains visible.
        stdout=subprocess.PIPE if capture_output else None,
        text=True,
        encoding="utf-8",
    )


# =============================================================================
# Workspace discovery
# =============================================================================

# Query Cargo rather than parsing Cargo.toml ourselves. Cargo metadata accounts
# for virtual workspaces, renamed packages, and any future manifest changes.
def workspace_packages() -> List[Dict[str, Any]]:
    """Return only this workspace's packages in Cargo metadata order.

    ``cargo metadata`` can include dependency packages in its ``packages``
    array. Although ``--no-deps`` minimizes that data, filtering by the stable
    package IDs in ``workspace_members`` makes the intent explicit and prevents
    dependencies from accidentally receiving their own rustdoc job.

    Returns:
        Cargo package objects belonging directly to the workspace.
    """
    # The metadata command itself does not need nightly. Stable Cargo provides
    # the package and target fields used by this script.
    result = run(
        [
            "cargo",
            "metadata",
            "--manifest-path",
            str(MANIFEST_PATH),
            "--no-deps",
            "--format-version",
            "1",
        ],
        capture_output=True,
    )

    # Parse only stdout; Cargo warnings are emitted separately on stderr and do
    # not contaminate the JSON document.
    metadata = json.loads(result.stdout)

    # Set membership avoids repeatedly scanning the workspace member list when
    # filtering larger workspaces.
    workspace_members = set(metadata["workspace_members"])
    return [
        package
        for package in metadata["packages"]
        if package["id"] in workspace_members
    ]


# Translate Cargo metadata targets into the selector syntax accepted by
# ``cargo rustdoc``. One package may describe libraries, binaries, examples,
# tests, and benchmarks, but the command can document only one at a time.
def documentation_targets(package: Dict[str, Any]) -> List[Tuple[str, str]]:
    """Select Cargo's normal documentation targets for one package.

    Library-like targets take precedence, matching ``cargo doc``'s default
    behavior. A package without a library falls back to every binary target
    whose Cargo metadata permits documentation. Examples, tests, and benches
    are intentionally excluded.

    Args:
        package: One package object returned by ``cargo metadata``.

    Returns:
        ``(selector, target_name)`` pairs. The selector is ``--lib`` or
        ``--bin``; the name is retained for status messages and is also passed
        after ``--bin`` when required by Cargo.
    """
    # A manifest can opt a target out with ``doc = false``. Older metadata may
    # omit the field, in which case Cargo's default is to allow documentation.
    targets = [target for target in package["targets"] if target.get("doc", True)]

    # Cargo represents each package's library as one logical target even when
    # it emits a specialized crate type such as cdylib or proc-macro.
    library_targets = [
        target
        for target in targets
        if LIBRARY_KINDS.intersection(target["kind"])
    ]
    if library_targets:
        # ``--lib`` does not accept a following name. Keep the metadata name in
        # the tuple only so progress output identifies the generated crate.
        return [("--lib", library_targets[0]["name"])]

    # Binary-only packages need an explicit name. Returning every eligible bin
    # also handles packages that declare more than one documented executable.
    return [
        ("--bin", target["name"])
        for target in targets
        if "bin" in target["kind"]
    ]


def documentation_feature_arguments(package: Dict[str, Any]) -> List[str]:
    """Select all package features except mutually exclusive alternatives.

    Cargo metadata exposes the package's complete feature-name map. Expanding
    that map into ``--features`` arguments gives this script the coverage of an
    all-features documentation build while allowing the full ``profiling`` mode
    to win over ``profiling-minimal``.

    Packages without declared features need no feature-selection arguments.
    """
    available_features = sorted(package.get("features", {}))
    selected_features = [
        feature
        for feature in available_features
        if feature not in EXCLUDED_DOCUMENTATION_FEATURES
    ]
    if not selected_features:
        return []

    # Cargo accepts a comma-separated list as one argument. Keeping it together
    # also makes dry-run output compact and directly copyable into a terminal.
    return ["--features", ",".join(selected_features)]


# =============================================================================
# Rustdoc command construction
# =============================================================================

# Construct commands as argument lists, keeping Cargo's arguments before the
# ``--`` delimiter and raw rustdoc arguments after it.
def generate_package(
    package: Dict[str, Any],
    selector: str,
    target_name: str,
    *,
    dry_run: bool,
) -> None:
    """Generate private-item JSON documentation for one package target.

    Args:
        package: Cargo metadata object for the owning package.
        selector: Cargo target selector, either ``--lib`` or ``--bin``.
        target_name: Human-readable target name, and the value for ``--bin``.
        dry_run: Print the command without invoking Cargo when true.
    """
    # ``+nightly`` is command-scoped. It does not change the repository's or
    # user's default Rust toolchain for normal builds and tests.
    command = [
        "cargo",
        "+nightly",
        "rustdoc",
        # Cargo currently gates its --output-format option independently from
        # rustdoc, so enabling Cargo's unstable options is also necessary.
        "-Z",
        "unstable-options",
        "--manifest-path",
        str(MANIFEST_PATH),
        "-p",
        package["name"],
        selector,
    ]
    if selector == "--bin":
        # Unlike --lib, --bin must be followed by the exact Cargo target name.
        command.append(target_name)

    # Enable every compatible feature before adding output options. This keeps
    # full ``profiling`` enabled but excludes the mutually exclusive
    # ``profiling-minimal`` mode in packages that declare it.
    command.extend(documentation_feature_arguments(package))

    # Options before ``--`` belong to Cargo. Options after it are forwarded
    # verbatim to the nightly rustdoc process.
    command.extend(
        [
            "--output-format",
            "json",
            "--",
            # Private functions, types, fields, and modules are included in the
            # JSON graph instead of restricting output to the public API.
            "--document-private-items",
        ]
    )

    if dry_run:
        # Dry-run mode still performs Cargo metadata discovery, ensuring these
        # printed commands correspond to the workspace's current packages.
        print("+", subprocess.list2cmdline(command), flush=True)
    else:
        run(command)


# =============================================================================
# Command-line orchestration and error reporting
# =============================================================================

# Keep main's return value as an integer so the module remains easy to call from
# tests while the __main__ guard translates it into a process exit status.
def main() -> int:
    """Parse arguments, build the documentation job list, and run each job."""
    parser = argparse.ArgumentParser(
        description="Generate nightly rustdoc JSON for every workspace package."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the rustdoc commands without running them",
    )
    args = parser.parse_args()

    try:
        # Expand package targets into a flat job list before doing any costly
        # compilation. This gives accurate progress counts from the first job.
        packages = workspace_packages()
        jobs = [
            (package, selector, target_name)
            for package in packages
            for selector, target_name in documentation_targets(package)
        ]

        if not jobs:
            # An empty successful run is usually a manifest/configuration error,
            # so report it as failure rather than silently doing nothing.
            print("No documentable workspace targets found.", file=sys.stderr)
            return 1

        # Run sequentially because separate Cargo processes already parallelize
        # compilation internally and would otherwise contend for target locks.
        for index, (package, selector, target_name) in enumerate(jobs, start=1):
            print(
                f"\n[{index}/{len(jobs)}] Generating {package['name']} "
                f"({target_name})",
                flush=True,
            )
            generate_package(
                package,
                selector,
                target_name,
                dry_run=args.dry_run,
            )
    except FileNotFoundError as error:
        # Usually indicates that Python cannot locate cargo/rustup's proxy on
        # PATH. Include the executable name to make setup failures actionable.
        print(f"Required executable not found: {error.filename}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        # Cargo already prints its detailed diagnostic. Preserve its status so
        # shells and CI jobs can reliably detect the failed documentation run.
        print(f"Command failed with exit code {error.returncode}.", file=sys.stderr)
        return error.returncode or 1
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        # Keep malformed or unexpectedly shaped metadata errors concise instead
        # of exposing an implementation traceback to normal script users.
        print(f"Could not parse Cargo workspace metadata: {error}", file=sys.stderr)
        return 1

    # Make dry runs unmistakable; otherwise users may look for files that the
    # script deliberately did not generate.
    if args.dry_run:
        print("\nDry run complete; no JSON was generated.")
    else:
        print(f"\nRustdoc JSON generated under {WORKSPACE_ROOT / 'target' / 'doc'}")
    return 0


# Convert main's result into the process exit code only during direct execution;
# importing this module for tests or tooling remains side-effect free.
if __name__ == "__main__":
    raise SystemExit(main())
