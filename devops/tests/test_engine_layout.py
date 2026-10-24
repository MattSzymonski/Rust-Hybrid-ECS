#!/usr/bin/env python3
"""
Engine layout check: the rules that keep the engine core shared and the
`pill_engine` facade per DLL, read from the Cargo manifests and the core's
sources.

PROTECTS: The engine core never links the facade or reads a registry; shared dylibs stay UI-free.
TIER: static

REQUIREMENTS: Python 3.11+ (standard library only).

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
      3. no module-abi - the removed `module-abi` feature is not named anywhere
                        under `modules/` (manifests, sources; generated
                        members excluded). It was the marker for the in-crate
                        loadable artifact; the generated `host_module_*`
                        wrappers replaced it, and a build that turned it back
                        on would duplicate the entry-point symbols.
      4. shared UI    - each UI-independent shared dylib (`pill_draw`, the in-game
                        egui UI's `pill_project_ui`, and the egui editor's copy,
                        `pill_editor_core`) is built as a
                        `dylib` (one copy per process) and links no UI framework,
                        window system or GPU crate (`dioxus`, `tao`, `wry`,
                        `winit`, `wgpu`), followed through every path dependency.

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
from pathlib import Path

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.workspace_graph import Crate, ManifestError  # noqa: E402

# The engine core, and the facade it must never link (rule 1).
CORE_CRATE = "pill_engine_core"
FACADE_CRATE = "pill_engine"

# The registry crate the core must not use (rule 2).
REGISTRY_CRATE = "inventory"
REGISTRY_PATH_PREFIX = "inventory::"

# The UI-independent shared dylibs, by crate name and directory under
# `modules/`, and the crates they must never link (rule 4). A crate not yet
# present is skipped, so a later stage adds its entry before the crate exists.
SHARED_UI_INDEPENDENT_DYLIBS = (
    # The plain draw data the UI libraries and the renderer contract share
    # (W-0032): one shared dylib, no UI framework inside.
    ("pill_draw", "rendering/pill_draw"),
    # The in-game egui UI (W-0028): one shared dylib over egui alone, so a
    # project links the UI without the windowing or GPU crates the frontends
    # own.
    ("pill_project_ui", "rendering/pill_project_ui"),
    # The egui editor's copy (W-0017): one shared dylib, same rule.
    ("pill_editor_core", "frontents/pill_editor_core"),
)
UI_FRAMEWORK_CRATES = ("dioxus", "tao", "wry", "winit", "wgpu")


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
        for dependency in Crate(directory / "Cargo.toml").linked():
            if dependency.package == FACADE_CRATE:
                broken.append(chain + [dependency.package])
            elif dependency.directory is not None:
                pending.append((dependency.directory, chain + [dependency.package]))
    return broken


# Every place the core could read a registry: an `inventory` dependency in its
# manifest, and each non-comment source line naming `inventory::`.
def registry_uses(core_directory):
    found = []
    core = Crate(core_directory / "Cargo.toml")
    if any(dependency.package == REGISTRY_CRATE for dependency in core.linked()):
        found.append(f"{CORE_CRATE}/Cargo.toml depends on `{REGISTRY_CRATE}`")
    for source in sorted((core_directory / "src").rglob("*.rs")):
        lines = source.read_text(encoding="utf-8").splitlines()
        for number, line in enumerate(lines, 1):
            if REGISTRY_PATH_PREFIX in line and not line.lstrip().startswith("//"):
                relative = source.relative_to(core_directory).as_posix()
                found.append(f"{CORE_CRATE}/{relative}:{number}: {line.strip()}")
    return found


# Every reference to the removed `module-abi` feature: a manifest that
# declares or enables it, or a non-comment source line naming it (rule 3).
# Host-generated members are skipped: the host writes them at runtime, and a
# stale one must fail the build, not this check.
def module_abi_references(root):
    modules = root / "modules"
    found = []
    generated = ("host_project_", "host_module_")
    for manifest in sorted(modules.rglob("Cargo.toml")):
        relative = manifest.relative_to(root).as_posix()
        if "target/" in relative or "pill_standalone_temp/" in relative:
            continue
        if manifest.parent.name.startswith(generated):
            continue
        if "module-abi" in manifest.read_text(encoding="utf-8"):
            found.append(f"{relative} names the removed `module-abi` feature")
    for source in sorted(modules.rglob("*.rs")):
        relative = source.relative_to(root).as_posix()
        if "target/" in relative or "pill_standalone_temp/" in relative:
            continue
        if any(part.startswith(generated) for part in source.parts):
            continue
        for number, line in enumerate(source.read_text(encoding="utf-8").splitlines(), 1):
            if "module-abi" in line and not line.lstrip().startswith("//"):
                found.append(f"{relative}:{number} names the removed `module-abi` feature")
    return found


# Every break of rule 4: a UI-independent shared dylib that is not a `dylib`,
# or that links a UI framework, window or GPU crate through any path dependency.
def shared_dylib_failures(root):
    found = []
    for name, relative in SHARED_UI_INDEPENDENT_DYLIBS:
        directory = root / "modules" / relative
        if not (directory / "Cargo.toml").is_file():
            continue
        manifest = Crate(directory / "Cargo.toml").manifest
        crate_types = manifest.get("lib", {}).get("crate-type", [])
        if "dylib" not in crate_types:
            found.append(f"{name} is not built as a `dylib` (crate-type {crate_types})")
        visited = set()
        pending = [(directory.resolve(), [name])]
        while pending:
            current, chain = pending.pop()
            if current in visited:
                continue
            visited.add(current)
            for dependency in Crate(current / "Cargo.toml").linked():
                if dependency.package in UI_FRAMEWORK_CRATES:
                    found.append(f"{' -> '.join(chain + [dependency.package])}")
                elif dependency.directory is not None:
                    pending.append((dependency.directory, chain + [dependency.package]))
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
    failures.extend(
        f"no module-abi: {reference} (the wrappers replaced the feature)"
        for reference in module_abi_references(root)
    )
    failures.extend(
        f"shared UI: {failure} (a UI-independent shared dylib)"
        for failure in shared_dylib_failures(root)
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
        (
            "a cfg on the removed feature fails",
            {"pill_engine_core": []},
            {"pill_engine_core": '#[cfg(feature = "module-abi")]\nfn x() {}\n'},
            1,
        ),
        (
            "a comment naming the removed feature passes",
            {"pill_engine_core": []},
            {"pill_engine_core": "// wrappers replaced the module-abi feature\n"},
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

    # A manifest declaring the removed feature fails too.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        write_crate(root, "pill_dummy_color", [])
        manifest = root / "modules" / "pill_dummy_color" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "\n[features]\nmodule-abi = []\n",
            encoding="utf-8",
        )
        failures = check(root)
    outcome = "PASS" if len(failures) == 1 else "FAIL"
    passed &= outcome == "PASS"
    print(f"  {outcome}  a manifest declaring the removed feature fails")

    # Rule 4: a UI-independent shared dylib passes as a dylib with no UI crate,
    # and fails when it links one or is not a dylib.
    for description, crate_type, dependencies, expected_failures in (
        ("a UI-independent shared dylib passes", '["rlib", "dylib"]', [], 0),
        ("a shared dylib linking a UI framework fails", '["rlib", "dylib"]', ["winit"], 1),
        ("a shared dylib that is not a dylib fails", '["rlib"]', [], 1),
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            write_crate(root, "pill_engine_core", [])
            directory = root / "modules" / "frontents" / "pill_editor_core"
            (directory / "src").mkdir(parents=True, exist_ok=True)
            lines = [
                '[package]\nname = "pill_editor_core"\nversion = "0.1.0"\n',
                f"[lib]\ncrate-type = {crate_type}\n",
                "[dependencies]",
            ]
            lines.extend(f'{dependency} = "1"' for dependency in dependencies)
            (directory / "Cargo.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")
            (directory / "src" / "lib.rs").write_text("", encoding="utf-8")
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
    print(
        "  PASS  engine layout: the core does not link the facade, reads no registry, "
        "module-abi is gone, and the shared UI dylibs are UI-independent dylibs"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
