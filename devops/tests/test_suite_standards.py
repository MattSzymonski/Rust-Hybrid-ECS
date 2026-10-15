#!/usr/bin/env python3
"""
Suite lint for `devops/tests`: the header `run_all.py` reads, and the rules
that keep what W-0010 deduplicated from coming back.

PROTECTS: Suites keep their run_all header and stop re-forking the core helpers.
TIER: static

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    The end-to-end suites are the sharpest tools in the repository and the
    easiest to fork by accident: a private `read_log` in one file, a bare
    settle in another, and the shared helpers quietly stop being the only way
    to drive a host. This lint holds every `devops/tests/test_*.py` to three
    rules:

      1. header   - the docstring declares the `PROTECTS:` line and a valid
                    `TIER:` that `run_all.py` reads. Checked by calling
                    run_all's own discovery, so the lint and the runner can
                    never disagree about what a header must look like.
      2. no forks - no suite defines its own `read_log`, `wait_token`,
                    `atomic_write` or `kill_*` helper. The lock, the output
                    monitor and the atomic source edit live in `devops/core/`,
                    and a second copy is how the suites drifted apart.
      3. sleep    - every `time.sleep` call carries a comment above it (or on
                    it) saying why it waits. A suite that cannot say why it
                    sleeps is a suite that will one day sleep for nothing.

    Each rule ships with a planted-violation self-test (`--self-test`), so a
    rule that stopped matching fails here, where it is cheap to see, instead
    of letting a forked helper through into a green run.

USAGE
  python devops/tests/test_suite_standards.py
  python devops/tests/test_suite_standards.py --self-test

EXAMPLE USAGE
  python devops/tests/test_suite_standards.py

  Exit status: 0 when every suite complies, 1 on violations, 2 on a usage
  error.

--- SCRIPT ---
"""

import argparse
import re
import sys
from pathlib import Path
from typing import List, Tuple

# Standalone-runnable: `run_all` and the suites this lints sit beside this
# file, and run_all puts `devops/` on `sys.path` itself for `core`.
SUITES_DIRECTORY = Path(__file__).resolve().parent
sys.path.insert(0, str(SUITES_DIRECTORY))

import run_all  # noqa: E402

# A helper under one of these names belongs to `devops/core/`. `read_log` and
# `atomic_write` moved to `core/suite_common.py`, `wait_token` to the editor
# driver (Stage 13), and `kill_*` covers stray-host cleanup, which may only
# run under the lock.
FORKED_HELPER_PATTERN = re.compile(
    r"^\s*def (read_log|wait_token|atomic_write|kill_[a-z_]*)\s*\(", re.MULTILINE
)

# A real call, as code. Matched anywhere in the line, because a suite may
# guard one with `if` on the same line.
SLEEP_CALL_PATTERN = re.compile(r"time\.sleep\s*\(")


# Every suite this lint covers: the same glob `run_all.py` discovers.
def discover_files() -> List[Path]:
    return sorted(SUITES_DIRECTORY.glob("test_*.py"))


# Rule 1's findings, from run_all's own discovery - the reader both sides
# share, so a header rule change cannot land in one and not the other.
def header_findings() -> List[str]:
    _suites, errors = run_all.discover_suites()
    return errors


# Rule 2: a suite may call these helpers, never define them.
def fork_findings(path: Path, text: str) -> List[str]:
    found = []
    for match in FORKED_HELPER_PATTERN.finditer(text):
        line = text.count("\n", 0, match.start()) + 1
        found.append(
            f"{path.name}:{line}: defines `{match.group(1)}`; the shared helper in "
            "devops/core is the only copy"
        )
    return found


# Rule 3: a sleep says why it waits, on the line or directly above it.
#
# Directly above, not somewhere above: the suites document a wait with one
# comment per call site. A rule that accepted a comment three lines up would
# pass a loop whose sleep has drifted away from the reason it was written for.
def sleep_findings(path: Path, text: str) -> List[str]:
    found = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if SLEEP_CALL_PATTERN.search(line) is None or line.lstrip().startswith("#"):
            continue
        if "#" in line.split("time.sleep", 1)[-1]:
            continue
        above = next((candidate for candidate in reversed(lines[:index]) if candidate.strip()), "")
        if not above.lstrip().startswith("#"):
            found.append(f"{path.name}:{index + 1}: a sleep with no comment above it saying why")
    return found


# Every rule's findings, in report order.
def collect() -> List[Tuple[str, List[str]]]:
    sources = [(path, path.read_text(encoding="utf-8")) for path in discover_files()]
    return [
        ("rule 1: every suite has the header run_all reads", header_findings()),
        (
            "rule 2: no suite forks a core helper",
            [finding for path, text in sources for finding in fork_findings(path, text)],
        ),
        (
            "rule 3: every sleep says why it waits",
            [finding for path, text in sources for finding in sleep_findings(path, text)],
        ),
    ]


# --- Self test ---------------------------------------------------------------

# Assembled from parts so this lint does not flag its own fixture text: the
# rules above scan every suite, and this file is one of them.
BARE_SLEEP = "time" + ".sleep(0.5)"
COMMENTED_SLEEP = "# Let the watcher see the write before the next edit.\n" + BARE_SLEEP


# Every rule accepts its compliant fixture and fails its planted violation.
def self_test() -> int:
    failures = 0

    def expect(description: str, ok: bool) -> None:
        nonlocal failures
        print(f"  [{'PASS' if ok else 'FAIL'}] {description}")
        if not ok:
            failures += 1

    fixture = SUITES_DIRECTORY / "test_suite_standards.py"
    expect(
        "a forked helper is caught",
        len(fork_findings(fixture, "def " + "read_log(path):\n    return []\n")) == 1,
    )
    expect(
        "calling a shared helper passes",
        fork_findings(fixture, "    lines = read_log(path)\n") == [],
    )
    expect("a bare sleep is caught", len(sleep_findings(fixture, "    " + BARE_SLEEP + "\n")) == 1)
    expect(
        "a comment above the sleep passes",
        sleep_findings(fixture, "    " + COMMENTED_SLEEP + "\n") == [],
    )
    expect(
        "a trailing comment passes",
        sleep_findings(fixture, "    " + BARE_SLEEP + "  # give the host a frame\n") == [],
    )
    expect(
        "a comment two lines above does not count",
        len(sleep_findings(fixture, "    # Poll until the marker shows.\n    check()\n    " + BARE_SLEEP + "\n"))
        == 1,
    )
    expect(
        "a missing PROTECTS line is caught",
        run_all.header_field('"""\nTIER: static\n"""', "PROTECTS") == "",
    )
    expect(
        "an unknown tier is caught",
        run_all.header_field('"""\nTIER: nightly\n"""', "TIER") not in run_all.TIER_ORDER,
    )
    expect("the real suites pass the header rule", header_findings() == [])
    print("PASS: self test." if failures == 0 else f"FAIL: {failures} self test case(s).")
    return 0 if failures == 0 else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Lint the devops test suites' own rules.")
    parser.add_argument("--self-test", action="store_true", help="check the checker itself")
    arguments = parser.parse_args()
    if arguments.self_test:
        return self_test()

    failures = 0
    for title, findings in collect():
        print(f"  [{'FAIL' if findings else 'PASS'}] {title}")
        for finding in findings:
            print(f"         {finding}")
        failures += len(findings)
    if failures:
        print(f"FAIL: {failures} suite-standards violation(s).")
        return 1
    print("PASS: every suite follows the suite rules.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
