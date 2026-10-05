#!/usr/bin/env python3
"""
Engine layout check: the rules that keep the engine core shared and the
`pill_engine` facade per DLL, read from the Cargo manifests and the core's
sources.

REQUIREMENTS: Python 3.11+ (standard library only: `tomllib`).

DESCRIPTION
    The engine is two crates. `pill_engine_core` holds the engine; `pill_engine`
    is the facade every project and extension depends on, which re-exports the
    core and owns the per-DLL `inventory` registries. The facade only works if
    every DLL embeds its own copy of it, so it must never end up inside the
    core: rustc links each crate once, and a crate the core depends on is the
    core's copy for every DLL. This check enforces:

      1. core graph   - nothing `pill_engine_core` links (its normal, build and
                        target-specific dependencies, followed through every
                        path dependency) is `pill_engine`.
      2. no registry  - the core reads no `inventory` registry: it has no
                        linked `inventory` dependency, and no source line under
                        `pill_engine_core/src` names `inventory::` outside a
                        comment. Once the core is shared, a registry read from
                        it would see every DLL's entries.

USAGE
  python devops/tests/test_engine_layout.py [--root <repository>]
  python devops/tests/test_engine_layout.py --self-test

EXAMPLE USAGE
  python devops/tests/test_engine_layout.py
  python devops/tests/test_engine_layout.py --self-test

  Exit status: 0 when every rule holds, 1 when at least one is broken, 2 on a
  usage error or an unreadable manifest.

--- SCRIPT ---
"""

import argparse
import sys
import tempfile
import tomllib
from pathlib import Path

# The engine core, and the facade it must never link (rule 1).
CORE_CRATE = "pill_engine_core"
FACADE_CRATE = "pill_engine"

# The registry crate the core must not use (rule 2).
REGISTRY_CRATE = "inventory"
REGISTRY_PATH_PREFIX = "inventory::"

# Dependency tables that are linked into the crate itself. Dev-dependencies
# are left out: they build tests, never the library.
LINKED_TABLES = ("dependencies", "build-dependencies")


class ManifestError(Exception):
    """A manifest that could not be read or parsed."""


# Reads one Cargo.toml into a dictionary.
def read_manifest(path):
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ManifestError(f"{path}: {error}") from error


# Every linked dependency table of a manifest: the top-level ones and each
# `[target.'cfg(...)'.*]` one.
def linked_dependency_tables(manifest):
    tables = [manifest.get(name, {}) for name in LINKED_TABLES]
    for target in manifest.get("target", {}).values():
        tables.extend(target.get(name, {}) for name in LINKED_TABLES)
    return tables


# The package names a manifest links, with the directory of each path
# dependency (None for a registry or git dependency).
def linked_dependencies(manifest, manifest_directory):
    found = []
    for table in linked_dependency_tables(manifest):
        for key, specification in table.items():
            if isinstance(specification, dict):
                package = specification.get("package", key)
                path = specification.get("path")
                directory = (manifest_directory / path).resolve() if path else None
            else:
                package, directory = key, None
            found.append((package, directory))
    return found


# Walks every path dependency reachable from `start_directory`; returns the
# chains (lists of package names) that end at the facade.
def chains_to_facade(start_directory):
    broken = []
    visited = set()
    pending = [(start_directory, [CORE_CRATE])]
    while pending:
        directory, chain = pending.pop()
        if directory in visited:
            continue
        visited.add(directory)
        manifest = read_manifest(directory / "Cargo.toml")
        for package, dependency_directory in linked_dependencies(manifest, directory):
            if package == FACADE_CRATE:
                broken.append(chain + [package])
            elif dependency_directory is not None:
                pending.append((dependency_directory, chain + [package]))
    return broken


# Every place the core could read a registry: an `inventory` dependency in its
# manifest, and each non-comment source line naming `inventory::`.
def registry_uses(core_directory):
    found = []
    manifest = read_manifest(core_directory / "Cargo.toml")
    if any(package == REGISTRY_CRATE for package, _ in linked_dependencies(manifest, core_directory)):
        found.append(f"{CORE_CRATE}/Cargo.toml depends on `{REGISTRY_CRATE}`")
    for source in sorted((core_directory / "src").rglob("*.rs")):
        lines = source.read_text(encoding="utf-8").splitlines()
        for number, line in enumerate(lines, 1):
            if REGISTRY_PATH_PREFIX in line and not line.lstrip().startswith("//"):
                relative = source.relative_to(core_directory).as_posix()
                found.append(f"{CORE_CRATE}/{relative}:{number}: {line.strip()}")
    return found


# Runs every rule against the repository at `root`; returns the failures.
def check(root):
    core_directory = root / "modules" / CORE_CRATE
    if not (core_directory / "Cargo.toml").is_file():
        return [f"{CORE_CRATE}: no manifest at {core_directory / 'Cargo.toml'}"]
    failures = [
        f"core graph: {' -> '.join(chain)} (the core must never link the facade)"
        for chain in chains_to_facade(core_directory)
    ]
    failures.extend(
        f"no registry: {use} (registries belong in the facade)" for use in registry_uses(core_directory)
    )
    return failures


# Writes a minimal crate under `root/modules/<name>`: a manifest and a
# `src/lib.rs` holding `source`. A dependency named `inventory` is written as a
# registry dependency, every other one as a path dependency.
def write_crate(root, name, dependencies, source=""):
    directory = root / "modules" / name
    (directory / "src").mkdir(parents=True, exist_ok=True)
    lines = [f'[package]\nname = "{name}"\nversion = "0.1.0"\n\n[dependencies]']
    for dependency in dependencies:
        if dependency == REGISTRY_CRATE:
            lines.append(f'{dependency} = "0.3"')
        else:
            lines.append(f'{dependency} = {{ path = "../{dependency}" }}')
    (directory / "Cargo.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (directory / "src" / "lib.rs").write_text(source, encoding="utf-8")


# Proves the checker accepts the intended layout and catches each break of
# both rules.
def self_test():
    registry_read = "fn names() { for entry in inventory::iter::<Entry> {} }\n"
    cases = [
        ("the intended layout passes", {"pill_engine_core": ["pill_core"], "pill_core": []}, {}, 0),
        ("a direct dependency fails", {"pill_engine_core": ["pill_engine"], "pill_engine": []}, {}, 1),
        (
            "a transitive dependency fails",
            {"pill_engine_core": ["helper"], "helper": ["pill_engine"], "pill_engine": []},
            {},
            1,
        ),
        ("an inventory dependency fails", {"pill_engine_core": ["inventory"]}, {}, 1),
        (
            "a registry read in the core's source fails",
            {"pill_engine_core": []},
            {"pill_engine_core": registry_read},
            1,
        ),
        (
            "a mention in a comment passes",
            {"pill_engine_core": []},
            {"pill_engine_core": "// the facade walks inventory::iter, the core never does\n"},
            0,
        ),
    ]
    passed = True
    for description, crates, sources, expected_failures in cases:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name, dependencies in crates.items():
                write_crate(root, name, dependencies, sources.get(name, ""))
            failures = check(root)
        outcome = "PASS" if len(failures) == expected_failures else "FAIL"
        passed &= outcome == "PASS"
        print(f"  {outcome}  {description}")
    return 0 if passed else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[1])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--self-test", action="store_true", help="check the checker itself")
    arguments = parser.parse_args()
    if arguments.self_test:
        return self_test()
    try:
        failures = check(arguments.root.resolve())
    except ManifestError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for failure in failures:
        print(f"  FAIL  {failure}")
    if failures:
        return 1
    print("  PASS  engine layout: the core does not link the facade and reads no registry")
    return 0


if __name__ == "__main__":
    sys.exit(main())
