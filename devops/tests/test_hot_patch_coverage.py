"""
Hot-patch coverage test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - `pill_standalone` built with the `pill_host/hot_patch` feature (this script
    builds it unless --skip-build is given)

DESCRIPTION
    Detects crates whose live-patch fast path silently stopped working.

    The failure this exists for leaves no trace a person would notice. When a
    patch cannot be built, the host reports it and falls back to a full reload -
    the edit still reaches the running process, just seconds later instead of
    milliseconds. Nothing breaks, no test fails, and the fast path can be dead
    for an entire crate without anyone finding out. That is exactly what
    happened: for every crate that declares cargo features, the replayed `rustc`
    command line was mis-tokenized and the patch never compiled. Two modules
    happened to be unaffected, so live patching looked healthy.

    So this test asserts the thing the console tells you and no other test
    checks: that an edit was delivered BY A PATCH, not by a reload.

    Crates are discovered rather than listed - the project and the optional
    modules come from `examples/project_rs/project_settings.yaml`, so a module
    added later is
    covered without touching this file. The function to edit is discovered too:
    the scanner looks for a numeric literal inside a function body, preferring
    an annotated function when the crate has one.

    Each crate is reported in one of four states:

      PATCHED      an edit was delivered by the fast path (the expected result)
      FELL BACK    the fast path refused or failed - the failure this test hunts
      NO FAST PATH the crate has neither annotations nor a build script, so no
                   fast path exists for it (reported, not a failure)
      SKIPPED      no editable literal was found to drive the test with

    Exits non-zero when any crate with a fast path fell back.

USAGE
  python devops/tests/test_hot_patch_coverage.py [--timeout-scale S]
      [--skip-build] [--strict]
        --timeout-scale S  Multiply every timeout (slow machines)
        --skip-build       Assume pill_standalone is already built
        --strict           Also fail when a crate has NO FAST PATH at all

EXAMPLE USAGE
  python devops/tests/test_hot_patch_coverage.py
  python devops/tests/test_hot_patch_coverage.py --strict --timeout-scale 2

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Dict, List, Optional, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

# The host prints one of these for every change it processes. Which one appears
# is the entire assertion: the first means the fast path delivered the edit, the
# rest mean it did not and a full reload picked the change up instead.
PATCH_APPLIED_PATTERN = re.compile(r"\[hot\] (\S+) LIVE (\d+) ms \(generation \d+ via ([\w+-]+)")
PATCH_REFUSED_TOKEN = "fast patch refused"
PATCH_FAILED_TOKEN = "patch failed"
RELOAD_TRIGGERED_TOKEN = "hot reload triggered"

# How long to wait for the host to report an outcome for one edit. Generous
# because the first patch of a session re-derives compiler flags (~1.2 s) and
# a fallback runs a real cargo build.
OUTCOME_TIMEOUT = 90
BUILD_TIMEOUT_SECONDS = 600

# Functions never worth editing: load-time plumbing, and anything a patch is
# expected to refuse for reasons that are not a defect.
UNEDITABLE_FUNCTIONS = {"register", "main"}

# A numeric literal that is safe to nudge. Deliberately narrow: an integer
# without a suffix could be an index or a capacity, while a float literal in a
# body is almost always a tunable constant.
FLOAT_LITERAL_PATTERN = re.compile(r"\b(\d+)\.(\d+)\b")

# How far left of a literal to reach when making the edit text unique. Long
# enough to clear a repeated constant, short enough to stay on one line.
MAX_SNIPPET_LENGTH = 80


# =============================================================================
# Crate discovery
# =============================================================================


class Crate:
    """One crate the host loads, and how this test drives it."""

    def __init__(self, name: str, source_root: Path, kind: str) -> None:
        self.name = name
        self.source_root = source_root
        self.kind = kind
        self.has_build_script = (source_root.parent / "build.rs").is_file()
        self.annotated = False
        self.edit_file: Optional[Path] = None
        self.edit_from = ""
        self.edit_to = ""
        self.function = ""
        self.outcome = "SKIPPED"
        self.detail = ""
        self.route = ""
        self.milliseconds = 0

    @property
    def has_fast_path(self) -> bool:
        """Whether any patch mechanism can reach this crate at all.

        An annotation gives a function its own dispatch slot; a build script
        gives the crate an address inventory the prologue route resolves
        against. With neither, an edit has no fast path and a reload is the
        correct - and only - outcome.
        """
        return self.annotated or self.has_build_script


def discover_crates() -> List[Crate]:
    """Reads the example project's settings file to find the project and every
    module.

    Discovered rather than listed so a module added to `project_settings.yaml`
    is covered without editing this file - the same convention the host itself
    follows when deciding what to load. The project is the native example
    (PROJECT_PATH drives it at runtime; discovery assumes the same default).
    """
    crates: List[Crate] = []
    project_root = NATIVE_PROJECT_ROOT
    crates.append(Crate(project_root.name, project_root / "src", "project"))

    # `modules:` is a YAML list of quoted crate directory names under
    # `optional/`, ending at the next top-level key or end of file.
    config_text = read_source(project_settings_yaml(NATIVE_PROJECT_ROOT))
    modules_block = re.search(
        r"^modules:\s*$(.*?)(?=^\S|\Z)", config_text, re.MULTILINE | re.DOTALL
    )
    if modules_block:
        for name in re.findall(r'^\s*-\s*"([^"]+)"', modules_block.group(1), re.MULTILINE):
            crates.append(Crate(name, MODULES_ROOT / "optional" / name / "src", "module"))
    return crates


def find_function_bodies(source: str) -> List[Tuple[str, int, int]]:
    """Every `fn name(...) { ... }` in the file, as (name, body start, body end).

    A brace scanner rather than a parser: it only has to find a literal inside a
    body, and being approximate costs nothing here - a mis-scan yields no
    candidate literal and the crate reports SKIPPED rather than a false failure.
    Strings and comments are skipped so a brace inside either cannot unbalance
    the scan.
    """
    bodies: List[Tuple[str, int, int]] = []
    for match in re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(<]", source):
        name = match.group(1)
        opening = source.find("{", match.end())
        if opening < 0:
            continue
        depth, index, length = 0, opening, len(source)
        in_string = in_line_comment = in_block_comment = False
        while index < length:
            character = source[index]
            pair = source[index : index + 2]
            if in_line_comment:
                if character == "\n":
                    in_line_comment = False
            elif in_block_comment:
                if pair == "*/":
                    in_block_comment = False
                    index += 1
            elif in_string:
                if character == "\\":
                    index += 1
                elif character == '"':
                    in_string = False
            elif pair == "//":
                in_line_comment = True
                index += 1
            elif pair == "/*":
                in_block_comment = True
                index += 1
            elif character == '"':
                in_string = True
            elif character == "{":
                depth += 1
            elif character == "}":
                depth -= 1
                if depth == 0:
                    bodies.append((name, opening, index))
                    break
            index += 1
    return bodies


def declaration_is_annotated(source: str, body_start: int) -> bool:
    """Whether the declaration owning this body carries a hot-patch attribute."""
    window = source[max(0, body_start - 400) : body_start]
    return "#[pill_hot]" in window or "#[pill_hot_fn]" in window


def runtime_code_end(source: str) -> int:
    """Where this file's runtime code stops and its test module begins.

    A `#[cfg(test)]` module is not in the running artifact, so editing a literal
    there changes nothing the host is executing: the watcher fires, the patch
    finds no live function to replace, and the crate would be reported as broken
    when nothing is wrong with it.
    """
    marker = source.find("#[cfg(test)]")
    return len(source) if marker < 0 else marker


def declaration_is_a_test(source: str, body_start: int) -> bool:
    """Whether this declaration is a test rather than shipped code."""
    window = source[max(0, body_start - 400) : body_start]
    return "#[test]" in window


def unique_snippet(source: str, literal_start: int, literal: str) -> Optional[str]:
    """The shortest text containing this literal that appears once in the file.

    A bare literal is rarely unique - `2.0` occurs all over a physics module -
    and replacing the first occurrence would edit a different function than the
    one being reported, turning a healthy crate into a false failure. Growing
    leftwards until the text is unique keeps the edit anchored to the intended
    line while staying inside the body.

    Returns `None` when no window up to `MAX_SNIPPET_LENGTH` is unique, which
    leaves the function as an unusable candidate rather than an unsafe edit.
    """
    for reach in range(0, MAX_SNIPPET_LENGTH, 4):
        start = literal_start - reach
        if start < 0:
            break
        snippet = source[start : literal_start + len(literal)]
        # A snippet spanning a newline would still be anchored correctly, but
        # keeping it on one line makes the reported edit readable.
        if "\n" in snippet:
            break
        if source.count(snippet) == 1:
            return snippet
    return None


def plan_edit(crate: Crate) -> bool:
    """Choose one function body and a literal inside it to nudge.

    Prefers an annotated function, because that exercises a dispatch slot - the
    route with the strongest guarantee - and because a crate that has one is
    unambiguously expected to patch. Falls back to any function when the crate
    has an address inventory instead.
    """
    candidates: List[Tuple[bool, Path, str, str, str]] = []
    for path in sorted(crate.source_root.rglob("*.rs")):
        source = read_source(path)
        runtime_end = runtime_code_end(source)
        for name, body_start, body_end in find_function_bodies(source):
            if name in UNEDITABLE_FUNCTIONS:
                continue
            # Only code the running artifact actually contains.
            if body_start >= runtime_end or declaration_is_a_test(source, body_start):
                continue
            annotated = declaration_is_annotated(source, body_start)
            if annotated:
                crate.annotated = True
            # Only a function the host can actually reach. An annotation gives
            # the function its own slot; without one the prologue route needs
            # the crate's address inventory, which only a build script emits.
            # Editing anything else would be reported as broken when the crate
            # is simply out of scope for patching.
            if not (annotated or crate.has_build_script):
                continue
            for literal in FLOAT_LITERAL_PATTERN.finditer(source[body_start:body_end]):
                snippet = unique_snippet(source, body_start + literal.start(), literal.group(0))
                if snippet is None:
                    continue
                changed = snippet.replace(
                    literal.group(0), f"{int(literal.group(1)) + 1}.{literal.group(2)}", 1
                )
                candidates.append((annotated, path, name, snippet, changed))
                break

    # Note the annotation scan above must finish before choosing, so
    # `crate.annotated` reflects the whole crate rather than the chosen function.
    for path in sorted(crate.source_root.rglob("*.rs")):
        source = read_source(path)
        runtime_end = runtime_code_end(source)
        for _, body_start, _ in find_function_bodies(source):
            if body_start < runtime_end and declaration_is_annotated(source, body_start):
                crate.annotated = True

    if not candidates:
        return False
    candidates.sort(key=lambda entry: (not entry[0], str(entry[1]), entry[2]))
    _, path, name, original, changed = candidates[0]
    crate.edit_file, crate.function = path, name
    crate.edit_from, crate.edit_to = original, changed
    return True


# =============================================================================
# Driving the host
# =============================================================================


# The host is launched through cargo rather than by running the executable.
# `-C prefer-dynamic` means the binary needs the toolchain's `std-*.dll` and the
# workspace's `pill_core.dll` on the loader path, and `cargo run` is what sets
# that up - executed directly, the process dies in the loader before it can
# print anything. Building through the same command also guarantees the binary
# carries the `hot_patch` feature, which is the whole subject of this test.
HOST_LAUNCH_COMMAND = [
    "cargo",
    "run",
    "-p",
    "pill_standalone",
    "--features",
    "pill_host/hot_patch",
]


def build_host() -> bool:
    """Build the standalone host with the hot-patch feature enabled."""
    print("  [BUILD] cargo build -p pill_standalone --features pill_host/hot_patch")
    completed = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "pill_standalone",
            "--features",
            "pill_host/hot_patch",
        ],
        cwd=str(MODULES_ROOT),
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
    )
    if completed.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(completed.stderr[-2000:])
        return False
    print("  [OK] Host built.")
    return True


def classify_outcome(output: str, crate: Crate) -> Tuple[str, str, str, int]:
    """Read the host's own report for one edit.

    Returns (outcome, detail, route, milliseconds). The host says exactly what
    it did, so this reads its verdict rather than inferring one from behaviour.
    """
    applied = PATCH_APPLIED_PATTERN.search(output)
    if applied:
        return "PATCHED", applied.group(1), applied.group(3), int(applied.group(2))

    # A refusal names a stable code and a sentence; both are worth surfacing,
    # because the whole point of this test is to say WHY a crate lost its fast
    # path rather than only that it did.
    for token in (PATCH_FAILED_TOKEN, PATCH_REFUSED_TOKEN):
        if token in output:
            detail = re.search(r'detail="(.*?)"(?:\s|$)', output, re.DOTALL)
            code = re.search(r'code="([^"]+)"', output)
            reason = detail.group(1)[:400] if detail else "no detail reported"
            return "FELL BACK", f"{code.group(1) if code else 'unknown'}: {reason}", "", 0
    if RELOAD_TRIGGERED_TOKEN in output:
        return "FELL BACK", "reloaded without reporting a fast-path outcome", "", 0
    return "SKIPPED", "the host reported nothing for this edit", "", 0


def exercise(crate: Crate, monitor: OutputMonitor, backups: BackupRegistry) -> None:
    """Make one body-only edit and record how the host delivered it."""
    print(f"\n  [EDIT] {crate.name}: {crate.edit_file.name} "
          f"fn {crate.function}  {crate.edit_from} -> {crate.edit_to}")
    backups.capture(crate.edit_file)
    # Edited as bytes, deliberately. Reading with `read_source` and writing with
    # `atomic_write` round-trips through universal newlines, which rewrites every
    # line ending in an LF file to CRLF on Windows. The host would then be
    # correct to report the whole file as changed - and this test would blame the
    # engine for damage it had done itself.
    raw = crate.edit_file.read_bytes()
    edited = raw.replace(crate.edit_from.encode("utf-8"), crate.edit_to.encode("utf-8"), 1)
    if edited == raw:
        crate.outcome = "SKIPPED"
        crate.detail = f"edit text {crate.edit_from!r} not found as written"
        print(f"  [SKIP] {crate.name}: {crate.detail}")
        return
    crate.edit_file.write_bytes(edited)

    start_index = monitor.line_count
    deadline = time.time() + OUTCOME_TIMEOUT
    while time.time() < deadline:
        output = monitor.output_since(start_index)
        outcome, detail, route, milliseconds = classify_outcome(output, crate)
        if outcome != "SKIPPED":
            crate.outcome, crate.detail = outcome, detail
            crate.route, crate.milliseconds = route, milliseconds
            break
        if not monitor.process_alive():
            crate.outcome, crate.detail = "FELL BACK", "the host exited during this edit"
            break
        time.sleep(0.2)
    else:
        crate.outcome = "SKIPPED"
        crate.detail = f"no outcome within {OUTCOME_TIMEOUT}s"

    marker = {"PATCHED": "[OK]", "FELL BACK": "[FAIL]"}.get(crate.outcome, "[SKIP]")
    summary = (
        f"{crate.route}, {crate.milliseconds} ms" if crate.outcome == "PATCHED" else crate.detail
    )
    print(f"  {marker} {crate.name}: {crate.outcome} - {summary}")

    # Restoring is itself a source change: the watcher fires again and the host
    # patches or reloads its way back. Waiting for that to finish is what keeps
    # the next crate's measurement clean - without it the following edit is
    # classified against a snapshot the host has not caught up with, and a
    # healthy crate is reported as `outside-hot-body`.
    restore_index = monitor.line_count
    backups.restore_one(crate.edit_file)
    settle_deadline = time.time() + OUTCOME_TIMEOUT
    while time.time() < settle_deadline:
        output = monitor.output_since(restore_index)
        if classify_outcome(output, crate)[0] != "SKIPPED":
            break
        if not monitor.process_alive():
            break
        time.sleep(0.2)
    time.sleep(1.0)


def inspect_flag_caches() -> List[str]:
    """Look for split arguments in the compiler-flag caches the host wrote.

    A second, narrower net than the outcome check above, and the only one that
    does not depend on cargo's mood. Cargo chooses between single and double
    quoting for the same argument from run to run, so a tokenizer that mishandles
    one style shows up as a fallback only sometimes - which is exactly how the
    original bug survived. These files are the tokenizer's output, so a split
    argument is visible in them whether or not it happened to break a patch this
    time.

    A cache holds one token per line. A token that begins with a quote, or ends
    with an unbalanced one, is a fragment of a value that should have stayed
    whole.
    """
    problems: List[str] = []
    cache_directory = Path(tempfile.gettempdir())
    for cache in sorted(cache_directory.glob("pill_hotpatch_*.flags")):
        try:
            lines = cache.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        # Line 0 is the tokenizer version, line 1 the build command, line 2 the
        # compiler; the arguments follow.
        for token in lines[3:]:
            if token.startswith(('"', "'")) or token.count('"') % 2 == 1:
                problems.append(
                    f"{cache.name}: token {token[:60]!r} looks like half an argument"
                )
    return problems


# =============================================================================
# Reporting
# =============================================================================


def print_report(crates: List[Crate], strict: bool) -> bool:
    """Print the coverage matrix and decide the exit status."""
    print("\n" + "=" * 78)
    print("  HOT-PATCH COVERAGE")
    print("=" * 78)
    print(f"  {'crate':24s} {'kind':8s} {'fast path':11s} {'result':11s} detail")
    print("  " + "-" * 74)
    for crate in crates:
        capability = "annotated" if crate.annotated else (
            "inventory" if crate.has_build_script else "none"
        )
        detail = (
            f"{crate.route}, {crate.milliseconds} ms"
            if crate.outcome == "PATCHED"
            else crate.detail[:34]
        )
        print(f"  {crate.name:24s} {crate.kind:8s} {capability:11s} "
              f"{crate.outcome:11s} {detail}")

    fragments = inspect_flag_caches()
    if fragments:
        print("\n  [FAIL] The compiler-flag cache holds split arguments, so the")
        print("         replayed rustc line is malformed for at least one crate:")
        for problem in fragments:
            print(f"         {problem}")

    fell_back = [crate for crate in crates if crate.outcome == "FELL BACK"]
    no_fast_path = [crate for crate in crates if not crate.has_fast_path]
    skipped = [crate for crate in crates if crate.outcome == "SKIPPED"]
    patched = [crate for crate in crates if crate.outcome == "PATCHED"]
    print()
    print(f"  {len(patched)} patched, {len(fell_back)} fell back, "
          f"{len(no_fast_path)} without a fast path, {len(skipped)} not exercised")

    print()
    if fell_back:
        print(f"  [FAIL] {len(fell_back)} crate(s) lost the fast path:")
        for crate in fell_back:
            print(f"         {crate.name}: {crate.detail}")
    if no_fast_path:
        print(f"  [NOTE] {len(no_fast_path)} crate(s) have no fast path at all "
              f"(no annotation, no build script):")
        for crate in no_fast_path:
            print(f"         {crate.name} - add a build.rs calling "
                  f"pill_hot_scan::generate_function_inventory()")
    if skipped:
        print(f"  [NOTE] {len(skipped)} crate(s) could not be exercised:")
        for crate in skipped:
            print(f"         {crate.name}: {crate.detail}")

    failed = bool(fell_back) or bool(fragments) or (strict and bool(no_fast_path))
    return not failed


# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Discover the crates, exercise each one, and report."""
    parser = argparse.ArgumentParser(
        description="Detect crates whose live-patch fast path stopped working"
    )
    parser.add_argument("--timeout-scale", type=float, default=1.0,
                        help="Multiply every timeout (slow machines)")
    parser.add_argument("--skip-build", action="store_true",
                        help="Assume pill_standalone is already built")
    parser.add_argument("--strict", action="store_true",
                        help="Also fail when a crate has no fast path at all")
    args = parser.parse_args()

    global OUTCOME_TIMEOUT
    OUTCOME_TIMEOUT = int(OUTCOME_TIMEOUT * args.timeout_scale)
    startup_timeout = int(STARTUP_TIMEOUT * args.timeout_scale)

    print("=" * 78)
    print("  Hot-Patch Coverage Test")
    print(f"  Workspace: {WORKSPACE_ROOT}")
    print("=" * 78)

    kill_stale_hosts()
    if not args.skip_build and not build_host():
        sys.exit(1)

    crates = discover_crates()
    if not crates:
        print("  [FAIL] No crates discovered from project_settings.yaml.")
        sys.exit(1)
    print(f"\n  Discovered {len(crates)} crate(s): "
          f"{', '.join(crate.name for crate in crates)}")

    for crate in crates:
        if plan_edit(crate):
            continue
        # Told apart deliberately. A crate with no annotation and no build
        # script has no fast path to lose, which is a coverage gap worth naming;
        # one that has a fast path but offered nothing editable is a limitation
        # of this test, not of the engine.
        if crate.has_fast_path:
            crate.outcome = "SKIPPED"
            crate.detail = "no editable float literal in a patchable function"
        else:
            crate.outcome = "NO FAST PATH"
            crate.detail = "no annotation and no build.rs inventory"

    environment = os.environ.copy()
    environment["PROJECT_PATH"] = "../examples/project_rs"
    process, monitor = launch_process(HOST_LAUNCH_COMMAND, MODULES_ROOT, environment)
    backups = BackupRegistry()
    passed = False
    try:
        if not monitor.wait_for(STARTUP_TOKEN, startup_timeout):
            print(f"  [FAIL] Host did not start within {startup_timeout}s.")
            sys.exit(1)
        print("  [OK] Host running.\n")

        # One warm-up edit before measuring anything. The first patch of a
        # session re-derives compiler flags and can restage artifacts, so its
        # outcome reflects session startup rather than the crate under test.
        warm_up = next((crate for crate in crates if crate.edit_file), None)
        if warm_up:
            print("  [WARMUP] One discarded edit, so the flags cache is populated.")
            exercise(warm_up, monitor, backups)
            warm_up.outcome, warm_up.detail = "SKIPPED", ""
            warm_up.route, warm_up.milliseconds = "", 0

        for crate in crates:
            if crate.edit_file:
                exercise(crate, monitor, backups)
        passed = print_report(crates, args.strict)
    finally:
        print("\n  [CLEANUP] Restoring sources and stopping the host...")
        backups.restore_all()
        terminate_process(process, monitor)
        print("  [OK] Restored.")

    print("\n" + "=" * 78)
    print("  TEST PASSED" if passed else "  TEST FAILED")
    print("=" * 78)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    run_suite_with_timing(main)
