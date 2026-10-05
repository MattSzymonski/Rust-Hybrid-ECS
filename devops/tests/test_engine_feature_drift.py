#!/usr/bin/env python3
"""
Engine feature drift check: every build the development hosts run resolves the
engine dylibs' dependency trees exactly as the host does.

REQUIREMENTS: Python 3.8+, cargo on PATH (runs `cargo tree --offline`).

DESCRIPTION
    `pill_core.dll` and `pill_engine_core.dll` are loaded once per process and
    imported by the host, every module and the project. The names they export
    hash the features their dependency tree resolved with, so a module whose
    build resolved one crate in that tree with a different feature set imports
    names the host's copy does not export, and fails to load with "procedure
    could not be found".

    For each host posture (headless `pill_standalone` with and without
    `hot_patch`, windowed `pill_standalone`, the editor), this resolves each
    engine dylib's subtree for the host alone, then for every extension's
    module build and every generated project build as the host runs them: the
    frontend selected as the anchor package, without its default features and
    with the host's own (`build_runner::apply_cargo_host_overrides`), beside the
    module (`<module>/module-abi`, plus `pill_engine/hot_patch` for a
    `hot_patch` host) or the project. Any crate whose features differ from the
    host's own resolution is reported.

    Extensions are found by convention (a `module-abi` feature under
    `extensions/*`), and generated projects are the `host_project_*` crates
    present at the time.

USAGE
  python devops/tests/test_engine_feature_drift.py [--verbose]
    --verbose  list the builds behind each reported difference

EXAMPLE USAGE
  python devops/tests/test_engine_feature_drift.py

  Exit status: 0 when no build drifts, 1 when one does, 2 when cargo tree fails.

--- SCRIPT ---
"""

import re
import subprocess
import sys
from pathlib import Path

MODULES = Path(__file__).resolve().parents[2] / "modules"

# The shared engine libraries whose subtrees must resolve identically.
ENGINE_DYLIBS = ("pill_core", "pill_engine_core")

# The host postures a module build can be anchored to: the host's own cargo
# selection, the frontend's features as `pill_host` names them for the anchor
# (`build_runner::host_posture_features`, without the anchor's defaults), and
# whether the host has `hot_patch` (which `pill_host` mirrors as
# `pill_engine/hot_patch` onto module and project builds).
FRONTENDS = {
    "headless": (["--package", "pill_standalone"], "pill_standalone", ["dev", "hot_patch"], True),
    "headless reload-only": (
        ["--package", "pill_standalone", "--no-default-features", "--features", "dev"],
        "pill_standalone", ["dev"], False,
    ),
    "windowed": (
        ["--package", "pill_standalone", "--features", "pill_standalone/rendering"],
        "pill_standalone", ["dev", "hot_patch", "rendering"], True,
    ),
    "editor": (["--package", "editor"], "editor", ["hot_patch", "rendering"], True),
}

# One `cargo tree --prefix depth -f "{p}|{f}"` line: depth, package, features.
TREE_LINE = re.compile(r"^(\d+)(\S+ v\S+)(?: \([^)]*\))?(?: \(\*\))?\|(.*)$")


class TreeError(Exception):
    """A `cargo tree` invocation that failed."""


# Resolves `selection` together with `dylib`; returns {package: features} for
# the dylib's own subtree, the dylib included.
def dylib_tree(dylib, selection):
    # `--no-dedupe`: cargo prints a repeated subtree once and marks later
    # occurrences `(*)` with no children, which would hide part of the dylib's
    # subtree whenever another selected root printed it first.
    completed = subprocess.run(
        ["cargo", "tree", "--offline", "--no-dedupe", "-e", "normal", "--prefix", "depth", "-f", "{p}|{f}",
         "--package", dylib, *selection],
        cwd=str(MODULES), capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    if completed.returncode != 0:
        raise TreeError(f"cargo tree failed for {dylib} with {selection}:\n{completed.stderr[-800:]}")
    packages, inside = {}, False
    for line in completed.stdout.splitlines():
        match = TREE_LINE.match(line.strip())
        if not match:
            continue
        depth, package, features = int(match.group(1)), match.group(2), match.group(3)
        if depth == 0:
            inside = package.startswith(f"{dylib} v")
        if inside:
            packages.setdefault(package, set()).update(feature for feature in features.split(",") if feature)
    return packages


# Every extension built as a module: a crate under `extensions/` declaring a
# `module-abi` feature.
def module_names():
    names = []
    for manifest in sorted(MODULES.glob("extensions/*/Cargo.toml")):
        if manifest.parent.name.startswith("host_project_"):
            continue
        if re.search(r"^module-abi\s*=", manifest.read_text(encoding="utf-8"), re.MULTILINE):
            names.append(manifest.parent.name)
    return names


# Every generated project crate currently present.
def project_names():
    return [path.parent.name for path in sorted(MODULES.glob("extensions/host_project_*/Cargo.toml"))]


# The builds a host anchored to `frontend` runs, as {label: cargo selection}.
def anchored_builds(frontend):
    _, anchor_package, anchor_features, hot_patch = FRONTENDS[frontend]
    anchor = ["--package", anchor_package, "--no-default-features",
              "--features", ",".join(f"{anchor_package}/{feature}" for feature in anchor_features)]
    engine_features = ["pill_engine/hot_patch"] if hot_patch else []
    builds = {}
    for module in module_names():
        builds[f"module {module}"] = [
            "--package", module, "--features", ",".join([f"{module}/module-abi", *engine_features]), *anchor]
    for project in project_names():
        project_features = ["--features", ",".join(engine_features)] if engine_features else []
        builds[f"project {project}"] = ["--package", project, *project_features, *anchor]
    return builds


# The differences between a build's subtree and the frontend's own, as
# {package: description}.
def differences(reference, measured):
    found = {}
    for package in sorted(set(reference) | set(measured)):
        if package not in reference:
            found[package] = "joins the tree"
        elif package not in measured:
            found[package] = "leaves the tree"
        elif reference[package] != measured[package]:
            gained = sorted(measured[package] - reference[package])
            lost = sorted(reference[package] - measured[package])
            parts = ([f"+{', +'.join(gained)}"] if gained else []) + ([f"-{', -'.join(lost)}"] if lost else [])
            found[package] = " ".join(parts)
    return found


def main():
    verbose = "--verbose" in sys.argv
    drift = {}
    measured_count = 0
    try:
        for dylib in ENGINE_DYLIBS:
            for frontend, (selection, _, _, _) in FRONTENDS.items():
                reference = dylib_tree(dylib, selection)
                for label, build in anchored_builds(frontend).items():
                    measured_count += 1
                    for package, description in differences(reference, dylib_tree(dylib, build)).items():
                        drift.setdefault((dylib, package, description), []).append(f"{label} ({frontend})")
    except TreeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"modules: {', '.join(module_names())}")
    print(f"generated projects: {', '.join(project_names()) or 'none present'}")
    print(f"builds measured: {measured_count}")
    for (dylib, package, description), builds in sorted(drift.items()):
        print(f"  FAIL  {dylib}: {package} {description} ({len(builds)} build(s))")
        if verbose:
            print(f"        from: {', '.join(builds)}")
    if drift:
        return 1
    print(f"  PASS  {', '.join(ENGINE_DYLIBS)} resolve identically in every anchored build")
    return 0


if __name__ == "__main__":
    sys.exit(main())
