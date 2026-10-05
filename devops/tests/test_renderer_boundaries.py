#!/usr/bin/env python3
"""
Renderer boundary check: the dependency rules of the renderer data split,
read from the Cargo manifests.

REQUIREMENTS: Python 3.11+ (standard library only: `tomllib`).

DESCRIPTION
    Each renderer comes as two crates: a GPU module (`extensions/<renderer>`)
    and its data crate (`extensions/<renderer>_data`). Both are found by that
    naming convention, never listed here. The shared contract crate is
    `pill_renderer_api`. This check reads the workspace members' manifests
    (and the example projects') and enforces:

      1. wgpu          - only a GPU module depends on `wgpu`, and at least one
                         does.
      2. api deps      - `pill_renderer_api` depends only on the contract's
                         crates: pill_core, pill_core_macros, pill_engine,
                         serde, thiserror, miette, raw-window-handle; and has no
                         build dependencies.
      3. host          - `pill_runtime`, `pill_csharp_bridge`, `pill_host` and
                         `pill_standalone` name no data crate
                         or GPU module in code. A dependency on one is allowed
                         only as a graph-only dependency (a key ending in
                         `_dependency_graph`, which keeps the host's resolved
                         graph equal to a module's), and no non-test source of
                         the crate may mention that key.
      4. projects      - no example project, and no generated project crate
                         (`extensions/host_project_*`), depends on a GPU
                         module.
      5. data crates   - no data crate depends on a GPU module.

    Crates outside the workspace (`extensions/rendering/*`, the older
    renderers) are not checked.

USAGE
  python devops/tests/test_renderer_boundaries.py [--root <repository>]
  python devops/tests/test_renderer_boundaries.py --self-test

EXAMPLE USAGE
  python devops/tests/test_renderer_boundaries.py
  python devops/tests/test_renderer_boundaries.py --self-test

  Exit status: 0 when every rule holds, 1 when at least one is broken, 2 on a
  usage error or an unreadable manifest.

--- SCRIPT ---
"""

import argparse
import sys
import tempfile
import tomllib
from pathlib import Path

# The contract crate and what it may depend on (rule 2).
API_CRATE = "pill_renderer_api"
API_ALLOWED_DEPENDENCIES = {
    "pill_core",
    "pill_core_macros",
    "pill_engine",
    "serde",
    "thiserror",
    "miette",
    "raw-window-handle",
}

# Crates that must not name renderer data or GPU code (rule 3).
HOST_CRATES = ("pill_runtime", "pill_csharp_bridge", "pill_host", "pill_standalone")

# Suffix that marks a data crate, and one that marks a graph-only dependency.
DATA_SUFFIX = "_data"
GRAPH_ONLY_SUFFIX = "_dependency_graph"

# The graphics crate only a GPU module may link (rule 1).
GRAPHICS_CRATE = "wgpu"


class Crate:
    """One manifest: its package name, directory and dependencies."""

    def __init__(self, manifest_path: Path):
        self.manifest_path = manifest_path
        self.directory = manifest_path.parent
        with manifest_path.open("rb") as manifest_file:
            self.manifest = tomllib.load(manifest_file)
        self.name = self.manifest.get("package", {}).get("name", self.directory.name)

    # Every dependency as (key, package name, kind), from every table:
    # `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and their
    # `[target.<cfg>.*]` forms. `kind` is "normal", "dev" or "build".
    def dependencies(self):
        tables = [self.manifest]
        for target in self.manifest.get("target", {}).values():
            tables.append(target)
        found = []
        for table in tables:
            for section, kind in (("dependencies", "normal"), ("dev-dependencies", "dev"),
                                  ("build-dependencies", "build")):
                for key, value in table.get(section, {}).items():
                    package = value.get("package", key) if isinstance(value, dict) else key
                    found.append((key, package, kind))
        return found


# The workspace members under `modules/`, expanding `dir/*` globs and skipping
# `exclude`d paths.
def workspace_members(modules: Path):
    with (modules / "Cargo.toml").open("rb") as manifest_file:
        workspace = tomllib.load(manifest_file)["workspace"]
    excluded = {(modules / path).resolve() for path in workspace.get("exclude", [])}
    members = []
    for member in workspace.get("members", []):
        directories = sorted((modules / member[:-2]).iterdir()) if member.endswith("/*") else [modules / member]
        for directory in directories:
            manifest = directory / "Cargo.toml"
            if directory.resolve() in excluded or not manifest.is_file():
                continue
            members.append(Crate(manifest))
    return members


# The example projects: every `examples/*/Cargo.toml`.
def example_projects(repository: Path):
    return [Crate(manifest) for manifest in sorted((repository / "examples").glob("*/Cargo.toml"))]


# Every broken rule under `repository`, as (rule number, message) pairs.
def check(repository: Path):
    modules = repository / "modules"
    members = workspace_members(modules)
    by_name = {crate.name: crate for crate in members}
    data_crates = {name for name in by_name if name.endswith(DATA_SUFFIX)}
    gpu_modules = {name[: -len(DATA_SUFFIX)] for name in data_crates if name[: -len(DATA_SUFFIX)] in by_name}
    renderer_crates = data_crates | gpu_modules
    problems = []

    # Rule 1: wgpu only in GPU modules, and in at least one.
    linking_graphics = sorted(crate.name for crate in members
                              if any(package == GRAPHICS_CRATE for _, package, _ in crate.dependencies()))
    for name in linking_graphics:
        if name not in gpu_modules:
            problems.append((1, f"{name} depends on {GRAPHICS_CRATE}; only a GPU module may"))
    if not any(name in gpu_modules for name in linking_graphics):
        problems.append((1, f"no GPU module depends on {GRAPHICS_CRATE}"))

    # Rule 2: the contract crate's dependency set.
    api = by_name.get(API_CRATE)
    if api is None:
        problems.append((2, f"{API_CRATE} is not a workspace member"))
    else:
        for key, package, kind in api.dependencies():
            if kind == "build":
                problems.append((2, f"{API_CRATE} has a build dependency ({package}); it needs no build script"))
            elif kind == "normal" and package not in API_ALLOWED_DEPENDENCIES:
                problems.append((2, f"{API_CRATE} depends on {package}, which the contract does not need"))

    # Rule 3: the host names no renderer data or GPU code.
    for host_name in HOST_CRATES:
        host = by_name.get(host_name)
        if host is None:
            continue
        for key, package, kind in host.dependencies():
            if kind == "dev" or package not in renderer_crates:
                continue
            if not key.endswith(GRAPH_ONLY_SUFFIX):
                problems.append((3, f"{host_name} depends on {package} as `{key}`; only a graph-only "
                                    f"`*{GRAPH_ONLY_SUFFIX}` dependency is allowed"))
                continue
            for source in sorted((host.directory / "src").rglob("*.rs")):
                if source.name == "tests.rs" or "tests" in source.relative_to(host.directory).parts[:-1]:
                    continue
                if key in source.read_text(encoding="utf-8", errors="replace"):
                    relative = source.relative_to(repository).as_posix()
                    problems.append((3, f"{relative} names the graph-only dependency `{key}` ({package})"))

    # Rule 4: no project depends on a GPU module. The one generated crate
    # allowed to is the renderer's own wrapper (host_module_<renderer>), whose
    # whole job is to carry that GPU module's loadable artifact; a wrapper for
    # any other module, like every project, must not.
    projects = example_projects(repository) + [
        crate for crate in members
        if crate.directory.name.startswith(("host_project_", "host_module_"))
    ]
    for project in projects:
        directory_name = project.directory.name
        for _, package, _ in project.dependencies():
            if package not in gpu_modules:
                continue
            if directory_name == f"host_module_{package}":
                continue
            relative = project.manifest_path.relative_to(repository).as_posix()
            problems.append((4, f"{relative} depends on the GPU module {package}"))

    # Rule 5: no data crate depends on a GPU module.
    for name in sorted(data_crates):
        for _, package, _ in by_name[name].dependencies():
            if package in gpu_modules:
                problems.append((5, f"{name} depends on the GPU module {package}"))

    return problems


# Print the outcome of `check` and return the exit status.
def report(repository: Path) -> int:
    problems = check(repository)
    for rule in range(1, 6):
        broken = [message for number, message in problems if number == rule]
        print(f"  [{'FAIL' if broken else 'PASS'}] rule {rule}")
        for message in broken:
            print(f"         {message}")
    if problems:
        print(f"FAIL: {len(problems)} renderer boundary violation(s).")
        return 1
    print("PASS: every renderer boundary rule holds.")
    return 0


# --- Self test ---------------------------------------------------------------

# A minimal repository that satisfies every rule: a renderer `r` with its data
# crate, the contract crate, a host with graph-only dependencies, a project.
VALID_TREE = {
    "modules/Cargo.toml": '[workspace]\nmembers = ["pill_renderer_api", "pill_host", "pill_standalone", "extensions/*"]\n'
                          'exclude = ["extensions/rendering"]\n',
    "modules/pill_renderer_api/Cargo.toml": '[package]\nname = "pill_renderer_api"\n[dependencies]\n'
                                           'pill_engine = { path = "x" }\nserde = "1"\n',
    "modules/pill_host/Cargo.toml": '[package]\nname = "pill_host"\n[dependencies]\n'
                                   'renderer_data_dependency_graph = { package = "r_data", path = "x" }\n'
                                   'renderer_dependency_graph = { package = "r", path = "x" }\n',
    "modules/pill_host/src/lib.rs": "//! Host.\n",
    "modules/pill_host/src/tests.rs": "use renderer_data_dependency_graph as r_data;\n",
    "modules/pill_standalone/Cargo.toml": '[package]\nname = "pill_standalone"\n[dependencies]\npill_host = { path = "x" }\n',
    "modules/extensions/r/Cargo.toml": '[package]\nname = "r"\n[dependencies]\nwgpu = "25"\nr_data = { path = "x" }\n',
    "modules/extensions/r_data/Cargo.toml": '[package]\nname = "r_data"\n[dependencies]\npill_renderer_api = { path = "x" }\n',
    "modules/extensions/host_module_r/Cargo.toml": '[package]\nname = "host_module_r"\n[dependencies]\nr = { path = "x" }\n',
    "modules/extensions/rendering/old/Cargo.toml": '[package]\nname = "old"\n[dependencies]\nwgpu = "26"\n',
    "examples/p/Cargo.toml": '[package]\nname = "p"\n[dependencies]\nr_data = { path = "x" }\n',
}

# One broken variant per rule: the files to overwrite, and the rule it breaks.
BROKEN_VARIANTS = [
    (1, {"modules/extensions/r_data/Cargo.toml": '[package]\nname = "r_data"\n[dependencies]\nwgpu = "25"\n'}),
    (1, {"modules/extensions/r/Cargo.toml": '[package]\nname = "r"\n[dependencies]\nr_data = { path = "x" }\n'}),
    (2, {"modules/pill_renderer_api/Cargo.toml": '[package]\nname = "pill_renderer_api"\n[dependencies]\nglam = "0.33"\n'}),
    (2, {"modules/pill_renderer_api/Cargo.toml": '[package]\nname = "pill_renderer_api"\n[build-dependencies]\npill_assets = "1"\n'}),
    (3, {"modules/pill_standalone/Cargo.toml": '[package]\nname = "pill_standalone"\n[dependencies]\nr_data = { path = "x" }\n'}),
    (3, {"modules/pill_host/src/lib.rs": "//! Host.\nuse renderer_data_dependency_graph::Mesh;\n"}),
    (4, {"examples/p/Cargo.toml": '[package]\nname = "p"\n[dependencies]\nr = { path = "x" }\n'}),
    (4, {"modules/extensions/host_module_o/Cargo.toml": '[package]\nname = "host_module_o"\n[dependencies]\nr = { path = "x" }\n'}),
    (5, {"modules/extensions/r_data/Cargo.toml": '[package]\nname = "r_data"\n[dependencies]\nr = { path = "x" }\n'}),
]


# Write `files` (relative path -> text) under `root`.
def write_tree(root: Path, files: dict) -> None:
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


# The valid tree passes, and each broken variant fails its own rule and only it.
def self_test() -> int:
    failures = 0
    with tempfile.TemporaryDirectory() as scratch:
        valid = Path(scratch) / "valid"
        write_tree(valid, VALID_TREE)
        problems = check(valid)
        if problems:
            print(f"  [FAIL] the valid tree reports {problems}")
            failures += 1
        else:
            print("  [PASS] the valid tree passes")
        for index, (rule, overrides) in enumerate(BROKEN_VARIANTS):
            root = Path(scratch) / f"broken_{index}"
            write_tree(root, {**VALID_TREE, **overrides})
            rules = sorted({number for number, _ in check(root)})
            if rules == [rule]:
                print(f"  [PASS] broken variant {index} fails rule {rule}")
            else:
                print(f"  [FAIL] broken variant {index} should fail rule {rule}, reports rules {rules}")
                failures += 1
    print("PASS: self test." if failures == 0 else f"FAIL: {failures} self test case(s).")
    return 0 if failures == 0 else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Check the renderer dependency rules from the manifests.")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2],
                        help="repository root (default: this script's repository)")
    parser.add_argument("--self-test", action="store_true",
                        help="check the checker against generated valid and broken trees")
    arguments = parser.parse_args()
    if arguments.self_test:
        return self_test()
    try:
        return report(arguments.root.resolve())
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        print(f"error: cannot read the manifests: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
