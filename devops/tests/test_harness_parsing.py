"""
Unit tests for the test harness itself.

PROTECTS: The parsing the end-to-end suites decide pass or fail with, on real host output.
TIER: static
SETS: hot-reload

REQUIREMENTS
  - Python 3.8+
  - No Rust toolchain, no host process, no .NET. Runs in well under a second.

DESCRIPTION
    Tests the parsing logic the other suites depend on, against real captured
    output rather than invented strings.

    Every other suite in this directory is an end-to-end test: it launches a
    host, edits real sources, and decides pass or fail by matching strings in
    console output. That makes the matching logic load-bearing - and its failure
    mode is silence. A regex that stops matching does not raise; it returns
    nothing, and the suite reports "no reload detected" or quietly drops a
    measurement. The suite still exits 0 or 1, so nothing looks broken.

    That is not hypothetical. `RELOAD_LINE_RE` matched the reloaded function's
    name with `\\S+`, which silently dropped every line whose name contained a
    space or a comma:

        pill_spline::<Spline as ColorTweak>::tweak                   trait method
        pill_dummy_color::get_color_a, pill_dummy_color::Tint::mix   two bodies, one save

    Three of 128 real analytics lines were unparseable. The benchmark reported
    those reloads with no phase breakdown at all, which is indistinguishable
    from the C# category that legitimately has none.

    So these tests use REAL lines, copied verbatim from host output, as
    fixtures. Invented strings would have kept passing throughout that bug,
    because the bug was in the gap between what the host prints and what the
    harness expected.

    Two of these checks used to compare a copy of an engine rule against
    another copy of the same rule, both written here: the provable-route set
    was asserted against a literal that restated it, and the shared-slot rule
    was reimplemented in the test and then asserted against itself. Neither
    could fail when the engine changed - the failure mode this whole file
    exists to catch. Both now read the rule out of the engine's own source
    (`pill_development_host/src/analytics.rs`, `pill_development_host/src/build_runner.rs`) and compare
    it with what the harness and this file believe, so a route relabelled or
    reclassified in Rust fails here.

USAGE
  python devops/tests/test_harness_parsing.py

EXAMPLE USAGE
  python devops/tests/test_harness_parsing.py

--- SCRIPT ---
"""

import importlib.util
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable, List, Tuple

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.paths import MODULES_ROOT, REPOSITORY_ROOT  # noqa: E402
from core.suite_common import (  # noqa: E402
    apply_replacements,
    restore_paths,
    run_suite_with_timing,
    snapshot_paths,
)
from core.test_report import summarize_test_results  # noqa: E402
from core.workspace_graph import (  # noqa: E402
    Crate,
    ManifestError,
    example_projects,
    read_manifest,
    workspace_members,
)


def load_module(relative_path: str, name: str):
    """Import a devops module by path.

    The benchmark and suite files are standalone scripts rather than an
    installed package, so they are loaded by location instead of by import.
    """
    spec = importlib.util.spec_from_file_location(name, DEVOPS_ROOT / relative_path)
    module = importlib.util.module_from_spec(spec)
    sys.argv = [name]  # the modules parse argv at import time
    spec.loader.exec_module(module)
    return module


harness = load_module("benchmarks/hot_reload_harness.py", "hot_reload_harness")
coverage = load_module("tests/test_hot_patch_coverage.py", "test_hot_patch_coverage")

# The engine sources the two rules under test are defined in. Read as text: a
# rule that moves, is renamed or is reworded fails the check that reads it.
PILL_HOST_SOURCE = MODULES_ROOT / "hosting" / "pill_development_host" / "src"
ANALYTICS_SOURCE = PILL_HOST_SOURCE / "analytics.rs"
BUILD_RUNNER_SOURCE = PILL_HOST_SOURCE / "build_runner.rs"

# Cargo's configuration hash: `-<16 hex digits>` appended to a crate's filename
# when the artifact belongs to one resolved configuration. A name without it is
# the shared slot every build of that crate name overwrites.
CONFIGURATION_HASH_HEX_DIGITS = 16


# =============================================================================
# Fixtures - verbatim host output
# =============================================================================

# Copied unmodified from real runs. Do not "tidy" these: their exact shape,
# including the spaces and commas inside function names, is the thing under
# test.
REAL_ANALYTICS_LINES = [
    # A whole-artifact reload.
    "[analytics] reload project (reload #1) | build=1.91s | stage=3.2ms | load=90.2ms"
    " | init=0.4ms | migrate=0.0ms | size=767.5KB | kind=reload",
    # A patch through a per-artifact slot, with the route fields.
    "[analytics] reload pill_dummy_color::get_color_a (reload #1) | build=479ms"
    " | stage=28.0ms | load=24.1ms | init=0.3ms | migrate=0.0ms | size=45.5KB"
    " | kind=patch | route=artifact-slot | copies=3",
    # A trait method: the name contains spaces inside `<Type as Trait>`.
    "[analytics] reload pill_spline::<Spline as ColorTweak>::tweak (reload #1)"
    " | build=507ms | stage=99.6ms | load=49.6ms | init=1.6ms | migrate=0.0ms"
    " | size=516.0KB | kind=patch | route=prologue | copies=2",
    # Two bodies patched in one save: the name contains a comma and a space.
    "[analytics] reload pill_dummy_color::get_color_a, pill_dummy_color::Tint::mix"
    " (reload #1) | build=360ms | stage=22.7ms | load=19.8ms | init=1.6ms"
    " | migrate=0.0ms | size=45.5KB | kind=patch",
    # A host predating the kind/route fields, and predating the export-column
    # removal: the optional groups are what keep stored measurements readable.
    "[analytics] reload pill_spline (reload #2) | build=401ms | stage=3.2ms"
    " | load=76.6ms | init=0.3ms | migrate=0.0ms | size=510.0KB | exports=9",
]

# Real `cargo test` output, one line per test binary, as `rust_tests` sees it.
REAL_CARGO_TEST_OUTPUT = """\
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_core-449f.exe)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_engine-3f3e.exe)
test result: ok. 160 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 14.69s
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_development_host-082c.exe)
test result: FAILED. 70 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.26s
"""

# Real Rust source, in the shapes the coverage suite's scanner must handle.
REAL_RUST_SOURCE = """\
pub struct Spline(u32);

impl Spline {
    /// A brace `{` inside a doc comment must not unbalance the scan.
    pub fn get_color_a(&self) -> f32 {
        113.0
    }
}

impl Default for Spline {
    fn default() -> Self {
        Spline(1)
    }
}

fn helper(value: f32) -> f32 {
    let text = "a brace } in a string must not unbalance it either";
    value * 0.5
}

#[cfg(test)]
mod tests {
    #[test]
    fn not_runtime_code() {
        assert_eq!(1.0, 1.0);
    }
}
"""


# =============================================================================
# Checks
# =============================================================================


def fail(message: str) -> None:
    """Fail the running check.

    Explicit rather than `assert`: `python -O` compiles an `assert` away, which
    would turn every check in this file into a silent pass.
    """
    raise AssertionError(message)


def rust_block(source: str, marker: str) -> str:
    """The inside of the first `{ ... }` block at or after `marker`.

    Rustfmt puts a top-level item's closing brace in column 0, so the first
    column-0 `}` after the opening brace ends it.
    """
    start = source.find(marker)
    if start < 0:
        fail(f"{marker!r} is gone from {ANALYTICS_SOURCE.name} / {BUILD_RUNNER_SOURCE.name}")
    opening = source.find("{", start)
    if opening < 0:
        fail(f"{marker!r} has no body")
    end = source.find("\n}", opening)
    if end < 0:
        fail(f"{marker!r} has no closing brace")
    return source[opening + 1 : end]


def check_every_real_analytics_line_parses() -> None:
    """Each fixture line must match, and expose its phase numbers."""
    for line in REAL_ANALYTICS_LINES:
        match = harness.RELOAD_LINE_RE.search(line)
        if match is None:
            fail(f"unparsed analytics line: {line[:90]}")
        if float(match.group(3)) < 0:
            fail(f"stage did not parse as a number: {match.group(3)!r}")


def check_function_names_with_spaces_and_commas_survive() -> None:
    """The two shapes that a `\\S+` name group silently dropped.

    Both are produced by features this repo actually has - trait-method patching
    and patching several bodies from one save - so neither is exotic.
    """
    trait_line = next(line for line in REAL_ANALYTICS_LINES if "ColorTweak" in line)
    trait_name = harness.RELOAD_LINE_RE.search(trait_line).group(1)
    if trait_name != "pill_spline::<Spline as ColorTweak>::tweak":
        fail(f"a trait-method name must be captured whole, spaces included: {trait_name!r}")

    multi_line = next(line for line in REAL_ANALYTICS_LINES if "Tint::mix" in line)
    multi_name = harness.RELOAD_LINE_RE.search(multi_line).group(1)
    if multi_name != "pill_dummy_color::get_color_a, pill_dummy_color::Tint::mix":
        fail(f"a multi-function name must be captured whole, comma included: {multi_name!r}")


def check_optional_fields_are_really_optional() -> None:
    """A host predating `kind=` and `route=` must still parse.

    The harness is expected to read output from an older binary; the groups are
    optional precisely so an upgrade does not invalidate stored measurements.
    """
    old_line = next(line for line in REAL_ANALYTICS_LINES if "kind=" not in line)
    match = harness.RELOAD_LINE_RE.search(old_line)
    if match is None:
        fail("a line without the optional fields must still match")
    if match.group(8) is not None:
        fail("kind must be absent, not defaulted")
    if match.group(9) is not None:
        fail("route must be absent, not defaulted")


def check_route_is_read_only_when_present() -> None:
    """`route` and `copies` parse together, or not at all."""
    with_route = next(line for line in REAL_ANALYTICS_LINES if "route=" in line)
    match = harness.RELOAD_LINE_RE.search(with_route)
    if match.group(9) != "artifact-slot":
        fail(f"route was {match.group(9)!r}")
    if match.group(10) != "3":
        fail(f"copies was {match.group(10)!r}")


def check_provable_routes_match_the_engine() -> None:
    """The harness's idea of a provable route must match the engine's.

    `PatchRoute::is_provable` in `pill_development_host/src/analytics.rs` is the definition;
    the harness's `PROVABLE_ROUTES` mirrors it. If they drift, the benchmark
    silently reclassifies a best-effort patch as a guaranteed one - so both
    sides are read here, and the labels the engine actually prints are the ones
    compared.
    """
    source = ANALYTICS_SOURCE.read_text(encoding="utf-8")
    variants = {
        line.strip().rstrip(",")
        for line in rust_block(source, "enum PatchRoute").splitlines()
        if re.fullmatch(r"[A-Z]\w*,?", line.strip())
    }
    if not variants:
        fail("no PatchRoute variants parsed: the enum's shape changed")

    implementation = rust_block(source, "impl PatchRoute")
    labels = dict(re.findall(r'Self::(\w+)\s*=>\s*"([\w+-]+)"', implementation))
    if set(labels) != variants:
        fail(
            "PatchRoute::label does not name every variant: "
            f"{sorted(variants ^ set(labels))} is on one side only"
        )

    match = re.search(r"matches!\(\s*self\s*,\s*(.+?)\)", implementation, re.DOTALL)
    if match is None:
        fail("PatchRoute::is_provable no longer states its routes as a matches! arm")
    provable = {
        labels[name] for name in re.findall(r"Self::(\w+)", match.group(1)) if name in labels
    }

    if provable != set(harness.PROVABLE_ROUTES):
        fail(
            "PROVABLE_ROUTES drifted from PatchRoute::is_provable in analytics.rs: "
            f"engine says {sorted(provable)}, harness says {sorted(harness.PROVABLE_ROUTES)}"
        )


def check_cargo_test_totals_are_summed() -> None:
    """The cargo summary must total every binary's line, including failures."""
    summary = summarize_test_results(REAL_CARGO_TEST_OUTPUT)
    if "244 passed" not in summary:
        fail(f"expected 14+160+70=244, got: {summary}")
    if "2 failed" not in summary:
        fail(f"a failing binary must be counted: {summary}")
    if "3 ignored" not in summary:
        fail(f"ignored must be totalled: {summary}")


def check_function_body_scan_handles_braces_in_strings_and_comments() -> None:
    """The coverage suite's brace scanner must not be fooled by text.

    A `{` in a doc comment or a `}` in a string literal would unbalance a naive
    scan, which would silently mis-locate every function after it.
    """
    bodies = coverage.find_function_bodies(REAL_RUST_SOURCE)
    names = [name for name, _, _, _ in bodies]
    for expected in ("get_color_a", "default", "helper"):
        if expected not in names:
            fail(f"missed {expected}: {names}")

    # The body of `helper` must end at its own closing brace, not run on.
    _, _, start, end = next(body for body in bodies if body[0] == "helper")
    body = REAL_RUST_SOURCE[start:end]
    if "0.5" not in body:
        fail("helper's body must contain its literal")
    if "mod tests" in body:
        fail("helper's body ran past its closing brace")


def check_associated_functions_are_not_offered_as_candidates() -> None:
    """An associated function has no `Self`, so the host refuses it by construction.

    Offering one as a candidate made the coverage suite report a healthy crate
    as broken (`associated-without-receiver`), so the picker must skip it while
    still accepting a method and a free function from the same file.
    """
    ranges = coverage.impl_block_ranges(REAL_RUST_SOURCE)
    verdicts = {
        name: coverage.is_associated_without_receiver(REAL_RUST_SOURCE, keyword, start, ranges)
        for name, keyword, start, _ in coverage.find_function_bodies(REAL_RUST_SOURCE)
    }
    if not verdicts.get("default"):
        fail(f"`default` is an associated function and must be skipped: {verdicts}")
    if verdicts.get("get_color_a"):
        fail(f"`get_color_a(&self)` has a receiver and must stay a candidate: {verdicts}")
    if verdicts.get("helper"):
        fail(f"`helper` is a free function and must stay a candidate: {verdicts}")


def check_test_module_is_excluded_from_runtime_code() -> None:
    """`#[cfg(test)]` code is not in the running artifact.

    Editing a literal there changes nothing the host executes, so the coverage
    suite would report a healthy crate as broken.
    """
    end = coverage.runtime_code_end(REAL_RUST_SOURCE)
    if "mod tests" not in REAL_RUST_SOURCE[end:]:
        fail("the test module must be excluded")
    if "get_color_a" not in REAL_RUST_SOURCE[:end]:
        fail("runtime code must be included")


def check_unique_snippet_anchors_to_one_place() -> None:
    """An edit anchor must identify exactly one location, or none."""
    source = "fn a() { x * 2.0 }\nfn b() { y * 2.0 }\n"
    literal_start = source.index("2.0")
    snippet = coverage.unique_snippet(source, literal_start, "2.0")
    if snippet is None:
        fail("a unique anchor exists here")
    if source.count(snippet) != 1:
        fail(f"anchor {snippet!r} is not unique")
    if "2.0" not in snippet:
        fail(f"anchor {snippet!r} does not contain the edited literal")


def check_shared_slot_detection_matches_cargo_naming() -> None:
    """Which artifacts another build can overwrite in place.

    The rule lives in `is_shared_slot_rlib` (`pill_development_host/src/build_runner.rs`):
    a `.rlib` whose stem carries no `-<16 hex digits>` suffix is the shared
    per-crate slot, and the only kind another build silently replaces. The patch
    linker and the dependency staging both call it, so a change there has to be
    noticed here; this reads the rule out of the function instead of restating
    it.
    """
    body = rust_block(BUILD_RUNNER_SOURCE.read_text(encoding="utf-8"), "fn is_shared_slot_rlib(")
    if 'strip_suffix(".rlib")' not in body:
        fail("is_shared_slot_rlib no longer keys on the .rlib suffix")
    if "is_ascii_hexdigit" not in body:
        fail("is_shared_slot_rlib no longer checks the suffix for hex digits")
    suffix_length = re.search(r"suffix\.len\(\)\s*==\s*(\d+)", body)
    if suffix_length is None:
        fail("is_shared_slot_rlib no longer compares the suffix length")
    if int(suffix_length.group(1)) != CONFIGURATION_HASH_HEX_DIGITS:
        fail(
            "the configuration hash is no longer "
            f"{CONFIGURATION_HASH_HEX_DIGITS} hex digits but {suffix_length.group(1)}, "
            "so every name below is classified against the wrong rule"
        )

    def is_shared(file_name: str) -> bool:
        """`is_shared_slot_rlib`'s rule, with its length read from its source."""
        stem = file_name[: -len(".rlib")]
        _, separator, suffix = stem.rpartition("-")
        if not separator:
            return True
        return not (
            len(suffix) == CONFIGURATION_HASH_HEX_DIGITS
            and all(character in "0123456789abcdef" for character in suffix)
        )

    # Cargo's own names: the shared slot a patch can be linked against, one
    # configuration's artifact, a short suffix and a non-hex one.
    cases = {
        "libpill_dummy_color.rlib": True,
        "libpill_engine-190d6c0e2d2eaf24.rlib": False,
        "libpill_engine-190d6c0e2d2eaf2.rlib": True,
        "libpill_engine-190d6c0e2d2eaf2Z.rlib": True,
    }
    for name, expected in cases.items():
        if is_shared(name) != expected:
            wanted = "the shared slot" if expected else "configuration-specific"
            fail(f"{name} must be {wanted}")


def check_source_edits_keep_line_endings() -> None:
    """An edit through the shared helpers changes only the edited line.

    Without `newline=""` Windows wrote every LF back as CRLF, so a one-line
    edit looked to the host like a whole-file change.
    """
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        for newline in ("\n", "\r\n"):
            path = Path(directory) / "edited.rs"
            original = newline.join(["fn a() {}", "fn b() { 1 }", "fn c() {}", ""]).encode("utf-8")
            path.write_bytes(original)
            if not apply_replacements(path, [("{ 1 }", "{ 2 }")]):
                fail("the edit anchor must be found")
            expected = original.replace(b"{ 1 }", b"{ 2 }")
            if path.read_bytes() != expected:
                fail(f"line endings changed for {newline!r}")


def check_snapshot_restores_files_exactly() -> None:
    """`restore_paths` puts back changed files and removes new ones."""
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        tracked = root / "bundle"
        (tracked / "src").mkdir(parents=True)
        (tracked / "src" / "lib.rs").write_bytes(b"original\n")
        lockfile = root / "Cargo.lock"
        lockfile.write_bytes(b"lock\n")
        paths = (tracked, lockfile)

        snapshot = snapshot_paths(paths)
        (tracked / "src" / "lib.rs").write_bytes(b"regenerated\r\n")
        (tracked / "src" / "extra.rs").write_bytes(b"new\n")
        lockfile.write_bytes(b"changed\n")
        restore_paths(paths, snapshot)

        if (tracked / "src" / "lib.rs").read_bytes() != b"original\n":
            fail("a changed file was not restored")
        if (tracked / "src" / "extra.rs").exists():
            fail("a new file must be removed")
        if lockfile.read_bytes() != b"lock\n":
            fail("the lockfile was not restored")


def check_editor_runs_get_their_own_folders() -> None:
    """Every suite's run lands in a folder of its own, cached per process."""
    import os

    from core.editor_session import (
        EDITOR_VALIDATION_ROOT,
        EditorSession,
        run_directory,
    )

    first = run_directory("probe-folders")
    if run_directory("probe-folders") != first:
        fail("run_directory must stay stable for one suite")
    if run_directory("probe-folders-other") == first:
        fail("two suites must never share a run folder")
    if first.parent != EDITOR_VALIDATION_ROOT:
        fail(f"run folders live under {EDITOR_VALIDATION_ROOT}, not {first.parent}")
    if not first.name.startswith("probe-folders-"):
        fail(f"the run folder is named suite + start time, got {first.name}")
    session = EditorSession("probe-folders")
    if session.run_directory != first:
        fail("a session must use its suite's run folder")
    for artifact in (session.log_path, session.commands_path, session.sandbox):
        if artifact.parent != first:
            fail(f"{artifact.name} must live inside the run folder")
    if EditorSession.supported() != (os.name == "nt"):
        fail("only Windows runs the native editor")


def check_editor_waits_read_from_an_offset() -> None:
    """`wait` searches from its offset; `wait_token` escapes the token."""
    import re as regex

    from core.editor_session import EditorSession

    session = EditorSession("probe-waits")
    session.run_directory.mkdir(parents=True, exist_ok=True)
    session.log_path.write_text("boot line\n", encoding="utf-8")
    length = session.wait(r"boot line", 5.0, "boot line")
    if length != len("boot line\n"):
        fail("wait must return the log length it read")
    if session.find(r"boot line") is None:
        fail("find must search the whole log")
    with session.log_path.open("a", encoding="utf-8") as log:
        log.write("[editor] second token\n")
    if session.wait(regex.escape("[editor] second token"), 5.0, "second", length) != len(
        session.read_log()
    ):
        fail("wait after an offset must find the line written later")
    if session.wait_token("[editor] second token", 5.0, "escaped", length) <= length:
        fail("wait_token must find the token after the offset")


def check_editor_waits_time_out_loudly() -> None:
    """A pattern that never appears raises `TimeoutError`, not a pass."""
    from core.editor_session import EditorSession

    session = EditorSession("probe-timeout")
    session.run_directory.mkdir(parents=True, exist_ok=True)
    session.log_path.write_text("nothing to see\n", encoding="utf-8")
    try:
        session.wait(r"never printed", 1.0, "never printed")
    except TimeoutError:
        return
    fail("wait must raise TimeoutError when the pattern never appears")


def check_editor_edits_keep_lf_and_restore() -> None:
    """`edit_source` keeps LF; `restore_sources` puts the original back."""
    import tempfile

    from core.editor_session import EditorSession

    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "edited.rs"
        original = b"fn a() {}\n"
        path.write_bytes(original)
        session = EditorSession("probe-edits")
        session.edit_source(path, "fn a() {}\n// probe\n")
        if path.read_bytes() != b"fn a() {}\n// probe\n":
            fail("edit_source must keep the file's LF endings")
        # A second edit keeps the FIRST capture as the restore target.
        session.edit_source(path, "fn a() {}\n// probe again\n")
        session.restore_sources()
        if path.read_bytes() != original:
            fail("restore_sources must put the original bytes back")


def check_workspace_members_expand_globs_and_excludes() -> None:
    """The member globs expand as cargo's do: the explicit crates, the
    `frontends/*` and `extensions/*/*` globs - and nothing under the excluded
    `extensions/rendering`, whose old renderers no build uses."""
    members = workspace_members(MODULES_ROOT)
    names = [member.name for member in members]
    for expected in ("pill_engine_core", "pill_engine", "pill_standalone", "pill_spline"):
        if expected not in names:
            fail(f"{expected} is missing from the expanded workspace members")
    if any("extensions/rendering" in member.directory.as_posix() for member in members):
        fail("a member under the excluded extensions/rendering was kept")
    if any(not name or name.startswith("_") for name in names):
        fail(f"a workspace anchor leaked in as a member: {names}")
    if len(names) != len(set(names)):
        fail("a member directory is double-counted")


def check_dependency_records_carry_kind_and_directory() -> None:
    """Kinds, renamed packages and resolved path directories, from a small
    tree that exercises every table shape plus the reader's error path."""
    with tempfile.TemporaryDirectory() as temporary:
        modules = Path(temporary) / "modules"
        (modules / "a" / "src").mkdir(parents=True)
        (modules / "b" / "src").mkdir(parents=True)
        (modules / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["a", "b"]\nexclude = ["b"]\n', encoding="utf-8"
        )
        (modules / "a" / "Cargo.toml").write_text(
            '[package]\nname = "a"\n'
            '[dependencies]\n'
            'b = { path = "../b" }\n'
            'renamed = { package = "b", path = "../b" }\n'
            'serde = "1"\n'
            '[dev-dependencies]\n'
            'dev_only = { package = "b", path = "../b" }\n'
            '[build-dependencies]\n'
            'build_only = { package = "b", path = "../b" }\n'
            "[target.'cfg(windows)'.dependencies]\n"
            'target_only = { package = "b", path = "../b" }\n',
            encoding="utf-8",
        )
        (modules / "b" / "Cargo.toml").write_text('[package]\nname = "b"\n', encoding="utf-8")
        crate = Crate(modules / "a" / "Cargo.toml")
        by_key = {dependency.key: dependency for dependency in crate.dependencies()}
        expected = {
            "b": ("b", "normal"),
            "renamed": ("b", "normal"),
            "serde": ("serde", "normal"),
            "dev_only": ("b", "dev"),
            "build_only": ("b", "build"),
            "target_only": ("b", "normal"),
        }
        if set(by_key) != set(expected):
            fail(f"dependency keys read as {sorted(by_key)}, expected {sorted(expected)}")
        for key, (package, kind) in expected.items():
            if (by_key[key].package, by_key[key].kind) != (package, kind):
                fail(
                    f"{key}: read as {by_key[key].package!r}/{by_key[key].kind!r}, "
                    f"expected {package!r}/{kind!r}"
                )
        if by_key["b"].directory != (modules / "b").resolve():
            fail(f"the path dependency resolved to {by_key['b'].directory}")
        if by_key["serde"].directory is not None:
            fail("a registry dependency must have no directory")
        linked_kinds = {dependency.kind for dependency in crate.linked()}
        if linked_kinds != {"normal", "build"}:
            fail(f"linked() must keep normal and build, not dev: {sorted(linked_kinds)}")
        if [member.name for member in workspace_members(modules)] != ["a"]:
            fail("the excluded member was kept by the expansion")
        broken = modules / "broken" / "Cargo.toml"
        broken.parent.mkdir()
        broken.write_text("[package\nname = 'x'\n", encoding="utf-8")
        try:
            read_manifest(broken)
        except ManifestError:
            pass
        else:
            fail("a broken manifest must raise ManifestError")


def check_example_projects_come_from_manifests() -> None:
    """The example projects are the manifests under `examples/`, with their
    package names read from them - one directory, one crate."""
    projects = example_projects(REPOSITORY_ROOT)
    directories = {crate.directory.name for crate in projects}
    for directory in ("project_rs", "master_renderer_test"):
        if directory not in directories:
            fail(f"examples/{directory} is missing from the example projects")
    if "project" not in {crate.name for crate in projects}:
        fail("examples/project_rs must read as package `project`")
    if any(not crate.manifest_path.is_file() for crate in projects):
        fail("an example project was constructed without a manifest")


def check_hook_and_ci_lint_the_same_files() -> None:
    """The hook and CI select the same files: one scope predicate.

    CI scans the repository through the lint's `is_linted`, and the
    changed-files hook imports that same predicate, so a file can no longer
    pass one and fail the other. The CI step must run from the repository
    root: a `--root modules` scan leaves tracked files outside `modules/`
    unlinted while the hook still flags them.
    """
    verify_steps = load_module("ci_cd/verify.py", "verify_steps")
    step = next((step for step in verify_steps.STEPS if step.name == "coding-standards"), None)
    if step is None:
        fail("verify.py no longer has a coding-standards step")
    command = [token for tokens in step.commands for token in tokens]
    if "--root" in command:
        fail("the coding-standards step scans a subtree; it must scan the repository root")
    if "devops/tests/test_coding_standards.py" not in command:
        fail(f"the coding-standards step runs {command}, not the lint")

    lint = load_module("tests/test_coding_standards.py", "test_coding_standards_scope")
    scanned = {path.resolve() for path in lint.find_rust_files(REPOSITORY_ROOT)}

    tracked = subprocess.run(
        ["git", "ls-files", "*.rs"], cwd=REPOSITORY_ROOT, capture_output=True, text=True
    ).stdout.splitlines()
    if not tracked:
        fail("git ls-files returned no Rust files")
    for name in tracked:
        path = REPOSITORY_ROOT / name
        if not lint.is_linted(path):
            fail(f"a tracked Rust file is outside the lint's scope: {name}")
        if path.resolve() not in scanned:
            fail(f"a tracked Rust file is not in the CI scan: {name}")

    # A git-ignored file can never reach the hook's candidate list, so the
    # lint must not check it either: CI may not gate on what nobody can fix
    # through the hook.
    ignored = subprocess.run(
        ["git", "ls-files", "--others", "--ignored", "--exclude-standard", "--", "*.rs"],
        cwd=REPOSITORY_ROOT,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    for name in ignored:
        if lint.is_linted(REPOSITORY_ROOT / name):
            fail(f"an ignored Rust file would be linted: {name}")

    # The hook imports the lint's predicate; load the hook the way its
    # harnesses do and make the two answer together, on the shapes that used
    # to disagree plus paths that only ever exist locally.
    hooks_directory = REPOSITORY_ROOT / ".agents" / "hooks"
    sys.path.insert(0, str(hooks_directory))
    spec = importlib.util.spec_from_file_location(
        "check_changed_hook", hooks_directory / "check_changed.py"
    )
    hook = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(hook)
    hook_lint = hook.load_lint()
    probes = [REPOSITORY_ROOT / name for name in tracked[:3] + tracked[-3:] + ignored]
    probes += [
        REPOSITORY_ROOT / "local" / "temp" / "probe.rs",
        REPOSITORY_ROOT / "examples" / "project_rs" / "src" / "lib.rs",
    ]
    for probe in probes:
        if lint.is_linted(probe) != hook_lint.is_linted(probe):
            fail(f"the hook and the lint disagree about {probe}")
    skipped = [probe for probe in probes if not lint.is_linted(probe)]
    if hook.lint_problems(skipped, hook_lint):
        fail("the hook must skip every non-lintable path without running the lint")


def check_the_gate_denies_warnings_everywhere() -> None:
    """`-D warnings` reaches every target and every posture.

    The workspace's `Cargo.toml` promises a zero-warning build in every
    feature configuration, but a lint only fails a build when the command
    denies it. `cargo test` and nextest do not, and neither does `cargo check`,
    so the default clippy step must add `--all-targets` (the test, example and
    bench targets) and each posture step must pass `-D warnings` on to rustc.
    """
    verify_steps = load_module("ci_cd/verify.py", "verify_warning_steps")
    steps = {step.name: step for step in verify_steps.STEPS}
    for name in ("clippy", "check-hot-patch", "check-shipping", "check-rendering"):
        if name not in steps:
            fail(f"verify.py no longer has a {name} step")
    clippy_command = [token for tokens in steps["clippy"].commands for token in tokens]
    if "--all-targets" not in clippy_command:
        fail(f"the clippy step runs {clippy_command}, which leaves the test targets unlinted")
    for name in ("check-hot-patch", "check-shipping", "check-rendering"):
        for command in steps[name].commands:
            if not command or command[0] != "cargo":
                continue
            if "--" not in command or command[command.index("--") + 1:] != ["-D", "warnings"]:
                fail(f"the {name} step runs {command}, which does not deny warnings")


CHECKS: List[Tuple[str, Callable[[], None]]] = [
    ("every real analytics line parses", check_every_real_analytics_line_parses),
    ("names with spaces and commas survive", check_function_names_with_spaces_and_commas_survive),
    ("optional fields are optional", check_optional_fields_are_really_optional),
    ("route and copies read together", check_route_is_read_only_when_present),
    ("provable routes match the engine", check_provable_routes_match_the_engine),
    ("cargo test totals are summed", check_cargo_test_totals_are_summed),
    ("brace scan survives strings and comments",
     check_function_body_scan_handles_braces_in_strings_and_comments),
    ("associated functions are not candidates",
     check_associated_functions_are_not_offered_as_candidates),
    ("cfg(test) code is excluded", check_test_module_is_excluded_from_runtime_code),
    ("edit anchors are unique", check_unique_snippet_anchors_to_one_place),
    ("shared-slot rule matches the engine", check_shared_slot_detection_matches_cargo_naming),
    ("source edits keep line endings", check_source_edits_keep_line_endings),
    ("snapshot restores files exactly", check_snapshot_restores_files_exactly),
    ("editor runs get their own folders", check_editor_runs_get_their_own_folders),
    ("editor waits read from an offset", check_editor_waits_read_from_an_offset),
    ("editor waits time out loudly", check_editor_waits_time_out_loudly),
    ("editor edits keep LF and restore", check_editor_edits_keep_lf_and_restore),
    ("workspace members expand like cargo", check_workspace_members_expand_globs_and_excludes),
    ("dependency records carry kind and directory",
     check_dependency_records_carry_kind_and_directory),
    ("example projects come from manifests", check_example_projects_come_from_manifests),
    ("hook and CI lint the same files", check_hook_and_ci_lint_the_same_files),
    ("the gate denies warnings everywhere", check_the_gate_denies_warnings_everywhere),
]


def main() -> None:
    """Run every check and report a tally."""
    print("=" * 70)
    print("  Harness Parsing Unit Tests")
    print("=" * 70)
    failed = 0
    for name, check in CHECKS:
        try:
            check()
            print(f"  [OK]   {name}")
        except AssertionError as error:
            failed += 1
            print(f"  [FAIL] {name}")
            print(f"         {error}")
        except Exception as error:  # noqa: BLE001 - report, do not mask
            failed += 1
            print(f"  [FAIL] {name} raised {type(error).__name__}: {error}")
    print("-" * 70)
    print(f"  {len(CHECKS) - failed} passed, {failed} failed")
    print("=" * 70)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    run_suite_with_timing(main)
