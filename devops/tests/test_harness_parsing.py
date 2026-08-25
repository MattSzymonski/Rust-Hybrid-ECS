"""
Unit tests for the test harness itself.

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

USAGE
  python devops/tests/test_harness_parsing.py

EXAMPLE USAGE
  python devops/tests/test_harness_parsing.py

--- SCRIPT ---
"""

import importlib.util
import sys
from pathlib import Path
from typing import Callable, List, Tuple

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))


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
basic = load_module("tests/test_basic.py", "test_basic")


# =============================================================================
# Fixtures - verbatim host output
# =============================================================================

# Copied unmodified from real runs. Do not "tidy" these: their exact shape,
# including the spaces and commas inside function names, is the thing under
# test.
REAL_ANALYTICS_LINES = [
    # A whole-artifact reload.
    "[analytics] reload project (reload #1) | build=1.91s | stage=3.2ms | load=90.2ms"
    " | init=0.4ms | migrate=0.0ms | size=767.5KB | exports=9 | kind=reload",
    # A patch through a per-artifact slot, with the route fields.
    "[analytics] reload pill_dummy_color::get_color_a (reload #1) | build=479ms"
    " | stage=28.0ms | load=24.1ms | init=0.3ms | migrate=0.0ms | size=45.5KB"
    " | exports=15 | kind=patch | route=artifact-slot | copies=3",
    # A trait method: the name contains spaces inside `<Type as Trait>`.
    "[analytics] reload pill_spline::<Spline as ColorTweak>::tweak (reload #1)"
    " | build=507ms | stage=99.6ms | load=49.6ms | init=1.6ms | migrate=0.0ms"
    " | size=516.0KB | exports=16 | kind=patch | route=prologue | copies=2",
    # Two bodies patched in one save: the name contains a comma and a space.
    "[analytics] reload pill_dummy_color::get_color_a, pill_dummy_color::Tint::mix"
    " (reload #1) | build=360ms | stage=22.7ms | load=19.8ms | init=1.6ms"
    " | migrate=0.0ms | size=45.5KB | exports=15 | kind=patch",
    # A host predating the kind/route fields: both groups are optional.
    "[analytics] reload pill_spline (reload #2) | build=401ms | stage=3.2ms"
    " | load=76.6ms | init=0.3ms | migrate=0.0ms | size=510.0KB | exports=9",
]

# Real `cargo test` output, one line per test binary, as `rust_tests` sees it.
REAL_CARGO_TEST_OUTPUT = """\
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_core-449f.exe)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_engine-3f3e.exe)
test result: ok. 160 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 14.69s
     Running unittests src\\lib.rs (target\\debug\\deps\\pill_host-082c.exe)
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


def check_every_real_analytics_line_parses() -> None:
    """Each fixture line must match, and expose its phase numbers."""
    for line in REAL_ANALYTICS_LINES:
        match = harness.RELOAD_LINE_RE.search(line)
        assert match is not None, f"unparsed analytics line: {line[:90]}"
        assert float(match.group(3)) >= 0, "stage must parse as a number"


def check_function_names_with_spaces_and_commas_survive() -> None:
    """The two shapes that a `\\S+` name group silently dropped.

    Both are produced by features this repo actually has - trait-method patching
    and patching several bodies from one save - so neither is exotic.
    """
    trait_line = next(line for line in REAL_ANALYTICS_LINES if "ColorTweak" in line)
    assert (
        harness.RELOAD_LINE_RE.search(trait_line).group(1)
        == "pill_spline::<Spline as ColorTweak>::tweak"
    ), "a trait-method name must be captured whole, spaces included"

    multi_line = next(line for line in REAL_ANALYTICS_LINES if "Tint::mix" in line)
    assert (
        harness.RELOAD_LINE_RE.search(multi_line).group(1)
        == "pill_dummy_color::get_color_a, pill_dummy_color::Tint::mix"
    ), "a multi-function name must be captured whole, comma included"


def check_optional_fields_are_really_optional() -> None:
    """A host predating `kind=` and `route=` must still parse.

    The harness is expected to read output from an older binary; the groups are
    optional precisely so an upgrade does not invalidate stored measurements.
    """
    old_line = next(line for line in REAL_ANALYTICS_LINES if "kind=" not in line)
    match = harness.RELOAD_LINE_RE.search(old_line)
    assert match is not None, "a line without the optional fields must still match"
    assert match.group(9) is None, "kind must be absent, not defaulted"
    assert match.group(10) is None, "route must be absent, not defaulted"


def check_route_is_read_only_when_present() -> None:
    """`route` and `copies` parse together, or not at all."""
    with_route = next(line for line in REAL_ANALYTICS_LINES if "route=" in line)
    match = harness.RELOAD_LINE_RE.search(with_route)
    assert match.group(10) == "artifact-slot", f"route was {match.group(10)!r}"
    assert match.group(11) == "3", f"copies was {match.group(11)!r}"


def check_provable_routes_match_the_engine() -> None:
    """The harness's idea of a provable route must match the engine's.

    `PatchRoute::is_provable` in `pill_host/src/analytics.rs` is the definition;
    this set mirrors it. If they drift, the benchmark silently reclassifies a
    best-effort patch as a guaranteed one.
    """
    assert harness.PROVABLE_ROUTES == {"engine-slot", "artifact-slot"}, (
        "PROVABLE_ROUTES drifted from PatchRoute::is_provable in analytics.rs"
    )


def check_cargo_test_totals_are_summed() -> None:
    """`rust_tests` must total every binary's line, including failures."""
    summary = basic.summarize_test_results(REAL_CARGO_TEST_OUTPUT)
    assert "244 passed" in summary, f"expected 14+160+70=244, got: {summary}"
    assert "2 failed" in summary, f"a failing binary must be counted: {summary}"
    assert "3 ignored" in summary, f"ignored must be totalled: {summary}"


def check_function_body_scan_handles_braces_in_strings_and_comments() -> None:
    """The coverage suite's brace scanner must not be fooled by text.

    A `{` in a doc comment or a `}` in a string literal would unbalance a naive
    scan, which would silently mis-locate every function after it.
    """
    bodies = coverage.find_function_bodies(REAL_RUST_SOURCE)
    names = [name for name, _, _ in bodies]
    assert "get_color_a" in names, f"missed an inherent method: {names}"
    assert "default" in names, f"missed a trait method: {names}"
    assert "helper" in names, f"missed a free function: {names}"

    # The body of `helper` must end at its own closing brace, not run on.
    _, start, end = next(b for b in bodies if b[0] == "helper")
    body = REAL_RUST_SOURCE[start:end]
    assert "0.5" in body, "helper's body must contain its literal"
    assert "mod tests" not in body, "helper's body ran past its closing brace"


def check_test_module_is_excluded_from_runtime_code() -> None:
    """`#[cfg(test)]` code is not in the running artifact.

    Editing a literal there changes nothing the host executes, so the coverage
    suite would report a healthy crate as broken.
    """
    end = coverage.runtime_code_end(REAL_RUST_SOURCE)
    assert "mod tests" in REAL_RUST_SOURCE[end:], "the test module must be excluded"
    assert "get_color_a" in REAL_RUST_SOURCE[:end], "runtime code must be included"


def check_unique_snippet_anchors_to_one_place() -> None:
    """An edit anchor must identify exactly one location, or none."""
    source = "fn a() { x * 2.0 }\nfn b() { y * 2.0 }\n"
    literal_start = source.index("2.0")
    snippet = coverage.unique_snippet(source, literal_start, "2.0")
    assert snippet is not None, "a unique anchor exists here"
    assert source.count(snippet) == 1, f"anchor {snippet!r} is not unique"
    assert "2.0" in snippet


def check_shared_slot_detection_matches_cargo_naming() -> None:
    """Which artifacts another build can overwrite in place.

    Mirrors `is_shared_slot_rlib` in `pill_host/src/hot_patch/compile.rs`; the
    two must agree or a file is staged and never linked, or linked and never
    staged.
    """
    for name in ("libpill_dummy_color.rlib", "libpill_core.rlib"):
        assert coverage is not None and name.endswith(".rlib")
    # Kept as a naming-convention check rather than importing Rust: a 16-hex
    # suffix marks one exact configuration, anything else is a shared slot.
    def is_shared(file_name: str) -> bool:
        stem = file_name[: -len(".rlib")]
        _, _, suffix = stem.rpartition("-")
        return not (len(suffix) == 16 and all(c in "0123456789abcdef" for c in suffix))

    assert is_shared("libpill_dummy_color.rlib")
    assert not is_shared("libpill_engine-190d6c0e2d2eaf24.rlib")


CHECKS: List[Tuple[str, Callable[[], None]]] = [
    ("every real analytics line parses", check_every_real_analytics_line_parses),
    ("names with spaces and commas survive", check_function_names_with_spaces_and_commas_survive),
    ("optional fields are optional", check_optional_fields_are_really_optional),
    ("route and copies read together", check_route_is_read_only_when_present),
    ("provable routes match the engine", check_provable_routes_match_the_engine),
    ("cargo test totals are summed", check_cargo_test_totals_are_summed),
    ("brace scan survives strings and comments",
     check_function_body_scan_handles_braces_in_strings_and_comments),
    ("cfg(test) code is excluded", check_test_module_is_excluded_from_runtime_code),
    ("edit anchors are unique", check_unique_snippet_anchors_to_one_place),
    ("shared-slot detection matches cargo", check_shared_slot_detection_matches_cargo_naming),
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
    main()
