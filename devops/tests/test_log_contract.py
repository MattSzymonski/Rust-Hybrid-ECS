"""
Guards the host log strings that the end-to-end suites match on.

REQUIREMENTS
  - Python 3.8+
  - No Rust toolchain, no host process, no .NET. Runs in well under a second.

DESCRIPTION
    Every end-to-end suite in this directory decides pass or fail by searching
    host console output for a literal token, declared as a `*_TOKEN` constant.
    Those constants make the host's log text an interface - but nothing on the
    Rust side says so, so a refactor that rewords a log line breaks the suites
    with no compiler error and no unit-test failure.

    That is not hypothetical. Unifying the project and optional-module reload
    paths behind one `ReloadTransaction` collapsed three distinct messages:

        "hot reload complete"                  also emitted by modules now
        "no longer registered by the project"  became "...by this module"
        "schema unchanged for all persistable component types"
                                               became "...persistable module types"

    The full workspace test suite stayed green at 412 passed. Only launching a
    host would have caught it, minutes into a run, as a confusing "no reload
    detected".

    This test closes that gap statically. It collects every `*_TOKEN` literal
    the suites define and requires each one to appear verbatim in the Rust
    sources - or to be listed in EXTERNALLY_PRODUCED below with the reason it
    cannot be, so that exemptions are stated rather than assumed.

USAGE
  python devops/tests/test_log_contract.py

EXAMPLE USAGE
  python devops/tests/test_log_contract.py

--- SCRIPT ---
"""

# Standard library
import re
import sys
from pathlib import Path
from typing import Callable, Dict, List, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.suite_common import run_suite_with_timing  # noqa: E402

# =============================================================================
# Constants
# =============================================================================

# Repository root, two levels above this file (devops/tests/<this>).
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]

# Directories whose Python modules declare host-output tokens.
TOKEN_SOURCE_DIRECTORIES = ("devops/tests", "devops/core", "devops/benchmarks")

# Matches `SOME_TOKEN = "literal"` at module level, capturing name and literal.
TOKEN_DEFINITION_RE = re.compile(
    r"^([A-Z][A-Z0-9_]*TOKEN[A-Z0-9_]*)\s*=\s*(\"|')(?P<literal>.*?)\2\s*$",
    re.MULTILINE,
)

# Tokens that legitimately never appear in Rust source, each with the reason.
# Anything not listed here must be greppable in `modules/**/*.rs`, so that a
# reworded log line fails this test instead of an end-to-end suite.
EXTERNALLY_PRODUCED: Dict[str, str] = {
    "STATUS_ACCESS_VIOLATION": "Windows prints this exit status, not the host.",
    "panicked at": "Emitted by the Rust panic runtime, not by our code.",
    "counter tick": "Printed by the example project in examples/project_rs.",
    "[project]": "Printed by the example project in examples/project_rs, which "
    "is outside the `modules/` tree this test scans.",
    "warning CS": "Emitted by the C# compiler.",
    "[csharp_runtime] reloaded project_cs.dll":
        "Emitted by the .NET bridge, whose source is C# rather than Rust.",
    "[csharp_runtime] reload failed:":
        "Emitted by the .NET bridge, whose source is C# rather than Rust.",
    "[analytics] reload project":
        "Composed at runtime: the '[analytics] reload' prefix is followed by "
        "the subject name held in a variable.",
    "[analytics] reload pill_spline":
        "Composed at runtime: the '[analytics] reload' prefix is followed by "
        "the subject name held in a variable.",
    "'project::FrameCounter' -> migrating":
        "Composed at runtime from the component's type name and its verdict.",
    "'project::SpatialPosition' -> migrating":
        "Composed at runtime from the component's type name and its verdict.",
    "'project::LinearVelocity' -> migrating":
        "Composed at runtime from the component's type name and its verdict.",
}

# Tokens whose whole point is to distinguish one reload subject from another.
# Collapsing either pair into a single string would leave the suites unable to
# tell a project reload from a module reload, so both must stay sourced.
SUBJECT_DISTINGUISHING_TOKENS: Tuple[Tuple[str, str], ...] = (
    ("hot reload complete", "optional module hot reload complete"),
    ("no longer registered by the project", "no longer registered by this module"),
)

# =============================================================================
# Free Functions
# =============================================================================


def collect_declared_tokens() -> Dict[str, List[str]]:
    """
    Scans the devops Python suites for `*_TOKEN` literals.

    Returns a mapping from the literal string to the list of `file:CONSTANT`
    locations that declare it, so a failure can name every affected suite.
    """
    tokens: Dict[str, List[str]] = {}
    for directory in TOKEN_SOURCE_DIRECTORIES:
        for module_path in sorted((REPOSITORY_ROOT / directory).glob("*.py")):
            text = module_path.read_text(encoding="utf-8")
            for match in TOKEN_DEFINITION_RE.finditer(text):
                literal = match.group("literal")
                if not literal:
                    continue
                origin = f"{directory}/{module_path.name}:{match.group(1)}"
                tokens.setdefault(literal, []).append(origin)
    return tokens


def read_rust_sources() -> str:
    """
    Concatenates every Rust source file under `modules/` into one haystack.

    Decoding errors are replaced rather than raised so that a stray non-UTF-8
    byte in an unrelated file cannot mask a genuine contract break.
    """
    parts = []
    for source_path in sorted((REPOSITORY_ROOT / "modules").rglob("*.rs")):
        parts.append(source_path.read_text(encoding="utf-8", errors="replace"))
    return "\n".join(parts)


def check_every_declared_token_exists_in_rust() -> None:
    """Fails if a suite matches on a string the host no longer prints."""
    rust_sources = read_rust_sources()
    missing = []
    for literal, origins in sorted(collect_declared_tokens().items()):
        if literal in EXTERNALLY_PRODUCED or literal in rust_sources:
            continue
        missing.append(f"{literal!r} required by {', '.join(origins)}")
    assert not missing, (
        "these tokens appear in no Rust source, so the suites that match on "
        "them can only fail at runtime:\n    " + "\n    ".join(missing)
    )


def check_exemptions_are_still_needed() -> None:
    """
    Fails if an entry in EXTERNALLY_PRODUCED is stale.

    An exemption whose token is now findable in Rust is a hole: it would let a
    real regression through. An exemption no suite declares any more is dead
    weight that makes the list harder to trust.
    """
    rust_sources = read_rust_sources()
    declared = collect_declared_tokens()
    stale = [
        f"{literal!r} ({reason})"
        for literal, reason in sorted(EXTERNALLY_PRODUCED.items())
        if literal in rust_sources
    ]
    assert not stale, (
        "these exemptions are no longer needed - the token is now in Rust "
        "source, so remove the entry:\n    " + "\n    ".join(stale)
    )
    unused = [
        repr(literal)
        for literal in sorted(EXTERNALLY_PRODUCED)
        if literal not in declared
    ]
    assert not unused, (
        "these exemptions name tokens no suite declares any more:\n    "
        + "\n    ".join(unused)
    )


def check_subject_tokens_stay_distinguishable() -> None:
    """
    Fails if a project token and its module counterpart cannot be told apart.

    Both pairs differ only by wording, and one is a substring of the other
    ("hot reload complete" inside "optional module hot reload complete"). The
    requirement is that each still has its own distinct Rust literal.
    """
    rust_sources = read_rust_sources()
    for project_token, module_token in SUBJECT_DISTINGUISHING_TOKENS:
        for token in (project_token, module_token):
            assert token in rust_sources, (
                f"{token!r} is not printed by any Rust source; the suites can "
                "no longer tell a project reload from a module reload"
            )
        assert project_token != module_token, (
            f"{project_token!r} collapsed onto its counterpart"
        )


def check_migration_banners_are_printed() -> None:
    """
    Fails if a `[persistence]` migration banner a suite matches on is gone.

    `test_hot_reload_migration.py` and `suite_common.py` both key on these
    banners to decide whether a selective migration ran, so losing one turns a
    migration assertion into a silent no-op.
    """
    rust_sources = read_rust_sources()
    banners = [
        literal
        for literal in collect_declared_tokens()
        if literal.startswith("[persistence]") and "migration" in literal.lower()
    ]
    assert banners, "no [persistence] migration banners are declared any more"
    for banner in sorted(banners):
        assert banner in rust_sources, (
            f"{banner!r} is declared by a suite but printed by nothing"
        )


# =============================================================================
# Test Registration
# =============================================================================

CHECKS: List[Tuple[str, Callable[[], None]]] = [
    ("every declared token exists in Rust", check_every_declared_token_exists_in_rust),
    ("exemptions are still needed", check_exemptions_are_still_needed),
    ("project and module tokens stay distinct", check_subject_tokens_stay_distinguishable),
    ("migration banners are printed", check_migration_banners_are_printed),
]


def main() -> None:
    """Run every check and report a tally."""
    print("=" * 70)
    print("  Host Log Contract Tests")
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
