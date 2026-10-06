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
    extension - through its generated `host_module_<name>` wrapper when that
    exists, and otherwise selected directly with the feature set the wrapper
    would enable - or the project, plus the engine features
    `build_runner::host_engine_features` mirrors from the running host. Any
    crate whose features differ from the host's own resolution is reported.

    The editor adds one thing to every spawned build: the cargo anchor
    `build_runner::apply_cargo_host_overrides` keeps for dioxus hosts, which
    selects the editor package itself (probed here with
    `--no-default-features` and the host's `hot_patch`/`rendering`) so the
    editor's host-side macro-graph unions are reproduced exactly.

    Extensions are found by convention (every `extensions/*` crate that is not
    a host-generated member), and generated projects are the `host_project_*`
    crates present at the time.

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

# The host postures module and project builds run under: the host's own cargo
# selection, and the engine features `build_runner::host_engine_features`
# mirrors from that host (the engine-affecting features `pill_host` declares:
# `hot_patch`, the profiling levels, `metrics`, `dev-logs`). The editor is the
# one posture that keeps a cargo anchor (see
# `build_runner::apply_cargo_host_overrides`): its macro graph unions host-side
# features that cannot be enumerated, so its builds also select the editor
# package, without its defaults and with the features the running host was
# built with.
FRONTENDS = {
    "headless": (["--package", "pill_standalone"], ["pill_engine/hot_patch"], None),
    "headless reload-only": (
        ["--package", "pill_standalone", "--no-default-features", "--features", "dev"],
        [],
        None,
    ),
    "windowed": (
        ["--package", "pill_standalone", "--features", "pill_standalone/rendering"],
        ["pill_engine/hot_patch"],
        None,
    ),
    "editor": (["--package", "editor"], ["pill_engine/hot_patch"], ("editor", ["hot_patch", "rendering"])),
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


# Every extension built as a loadable module: a crate under `extensions/`
# that is not a host-generated member. (`extensions/rendering` holds the old
# renderers, excluded from the workspace.)
def module_names():
    names = []
    for manifest in sorted(MODULES.glob("extensions/*/Cargo.toml")):
        name = manifest.parent.name
        if name.startswith(("host_project_", "host_module_")) or name == "rendering":
            continue
        names.append(name)
    return names


# The feature names an extension declares, minus the `default` key: the set a
# generated wrapper enables on its dependency edge.
def wrapper_features(name):
    text = (MODULES / "extensions" / name / "Cargo.toml").read_text(encoding="utf-8")
    section = re.search(r"^\[features\]\s*$(.*?)(^\[|\Z)", text, re.MULTILINE | re.DOTALL)
    if not section:
        return []
    declared = re.findall(r"^([A-Za-z0-9_-]+)\s*=", section.group(1), re.MULTILINE)
    return [feature for feature in declared if feature != "default"]


# Every generated project crate currently present.
def project_names():
    return [path.parent.name for path in sorted(MODULES.glob("extensions/host_project_*/Cargo.toml"))]


# The builds a host running `frontend` spawns, as {label: cargo selection}.
#
# An extension is measured through its generated wrapper when one exists (the
# host writes those on startup, so a checkout that ran the host covers this
# path), and otherwise through the extension selected directly with the same
# feature set the wrapper would enable.
def spawned_builds(frontend):
    _, engine_features, anchor = FRONTENDS[frontend]
    builds = {}
    for module in module_names():
        wrapper = f"host_module_{module}"
        if (MODULES / "extensions" / wrapper).is_dir():
            selection = ["--package", wrapper]
            features = list(engine_features)
        else:
            selection = ["--package", module]
            features = [f"{module}/{feature}" for feature in wrapper_features(module)]
            features += engine_features
        if features:
            selection += ["--features", ",".join(features)]
        if anchor:
            anchor_package, anchor_features = anchor
            selection += ["--package", anchor_package, "--no-default-features", "--features"]
            selection += [",".join(f"{anchor_package}/{feature}" for feature in anchor_features)]
        builds[f"module {module}"] = selection
    # The startup batch invocation selects, in one cargo call, every wrapper
    # the host will load plus its native project member (and, in a windowed
    # host, the renderer's wrapper - among the wrappers below). See
    # `build_runner::build_extension_batch`; its engine resolution must match
    # the host just like the per-module builds'. Only generated members can be
    # selected: a host batches exactly the modules it loads, whose members it
    # wrote on startup.
    batch_packages = [
        f"host_module_{module}"
        for module in module_names()
        if (MODULES / "extensions" / f"host_module_{module}").is_dir()
    ]
    batch_packages += project_names()
    if len(batch_packages) > 1:
        batch = []
        for package in batch_packages:
            batch += ["--package", package]
        if engine_features:
            batch += ["--features", ",".join(engine_features)]
        if anchor:
            anchor_package, anchor_features = anchor
            batch += ["--package", anchor_package, "--no-default-features", "--features"]
            batch += [",".join(f"{anchor_package}/{feature}" for feature in anchor_features)]
        builds["startup batch"] = batch
    for project in project_names():
        project_features = ["--features", ",".join(engine_features)] if engine_features else []
        selection = ["--package", project, *project_features]
        if anchor:
            anchor_package, anchor_features = anchor
            selection += ["--package", anchor_package, "--no-default-features", "--features"]
            selection += [",".join(f"{anchor_package}/{feature}" for feature in anchor_features)]
        builds[f"project {project}"] = selection
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
            for frontend, (selection, _, _) in FRONTENDS.items():
                reference = dylib_tree(dylib, selection)
                for label, build in spawned_builds(frontend).items():
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
    print(f"  PASS  {', '.join(ENGINE_DYLIBS)} resolve identically in every spawned build")
    return 0


if __name__ == "__main__":
    sys.exit(main())
