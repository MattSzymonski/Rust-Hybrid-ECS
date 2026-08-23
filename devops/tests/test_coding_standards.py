#!/usr/bin/env python3
"""
Pill comment and layout lint for every Rust source file in the repository.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Lints every `.rs` file under the repository against the Pill comment and
    layout rules (see the Pill Guide, "Contributing / Coding standards"). The
    checks are line-oriented heuristics, so they are a fast first-pass signal,
    not a substitute for a full per-file audit.

    Ported from `devops/ci_cd/run_coding_standards_test.sh`, which now just
    invokes this script. The heuristics are reproduced exactly, including
    their deliberate exemptions, so the two report the same violations.

    Checks performed:
      1. module header   - every file opens with a `//!` block and contains a
                           `# Responsibilities` section.
      2. safety comments - every `unsafe` token has a `// SAFETY:` (or
                           `# Safety`) justification within a bounded window
                           above it.
      3. public docs     - every `pub` item (fn/struct/enum/trait/const/
                           static/type/mod) is preceded by a `///` doc comment.
      4. import groups   - the `// Standard library`, `// External crates` and
                           `// Current crate` headers appear in that order.
      5. tests at bottom - no new top-level section starts after `mod tests`.

    The two window sizes are tunable through the environment, matching the
    shell version: `SAFETY_WINDOW_LINES` (default 30) and `DOC_WINDOW_LINES`
    (default 12).

USAGE
  python devops/tests/test_coding_standards.py [--root <path>] [--list]
  python devops/tests/test_coding_standards.py <path/to/file.rs | path/to/dir>

EXAMPLE USAGE
  python devops/tests/test_coding_standards.py
  python devops/tests/test_coding_standards.py --list
  python devops/tests/test_coding_standards.py --root modules/pill_engine/src
  python devops/tests/test_coding_standards.py modules/pill_core/src/lib.rs

  Exit status: 0 when every checked file complies, 1 when at least one
  violation is reported, 2 on a usage error.

--- SCRIPT ---
"""

import argparse
import os
import re
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.paths import REPOSITORY_ROOT  # noqa: E402
from core.test_report import (  # noqa: E402
    ANSI_BOLD,
    ANSI_GREEN,
    ANSI_RED,
    ANSI_YELLOW,
    colorize,
    section,
)

# =============================================================================
# Tunable heuristic bounds
# =============================================================================

# Maximum lines between a SAFETY marker and the `unsafe` token it covers.
SAFETY_WINDOW_LINES = int(os.environ.get("SAFETY_WINDOW_LINES", "30"))
# Maximum lines between a `///` line and the item it documents.
DOC_WINDOW_LINES = int(os.environ.get("DOC_WINDOW_LINES", "12"))

# Directories whose contents are build output or vendored code, never sources.
EXCLUDED_DIRECTORY_NAMES = {
    "target",
    "node_modules",
    ".git",
    "pill_standalone_temp",
}

# =============================================================================
# Patterns
#
# Translated from the awk programs in the shell version. awk's `\b` is a
# backspace, so the original spelled word boundaries out as explicit character
# classes; those are kept verbatim rather than replaced with `\b`, so the two
# implementations cannot diverge on an edge case.
# =============================================================================

SAFETY_MARKER_PATTERN = re.compile(r"// *SAFETY:|# *Safety")
COMMENT_LINE_PATTERN = re.compile(r"^\s*//")
BLANK_LINE_PATTERN = re.compile(r"^\s*$")
UNSAFE_TOKEN_PATTERN = re.compile(r"(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)")
UNSAFE_BLOCK_PATTERN = re.compile(r"unsafe\s*\{")
UNSAFE_DECLARATION_PATTERN = re.compile(r"unsafe\s+(fn|extern|const|trait)")
DOC_COMMENT_PATTERN = re.compile(r"^///")
PUBLIC_ITEM_PATTERN = re.compile(
    r"^pub(\s*\((crate|super)\))?(\s+unsafe)?\s+"
    r"(fn|struct|enum|trait|const|static|type|mod)([^A-Za-z0-9_]|$)"
)
TESTS_MODULE_PATTERN = re.compile(r"^\s*mod tests(\s|;)")
TOP_LEVEL_SECTION_PATTERN = re.compile(r"^// ====")

IMPORT_GROUP_HEADERS = (
    ("// Standard library", "'// Standard library'"),
    ("// External crates", "'// External crates'"),
    ("// Current crate", "'// Current crate'"),
)

# A `mod tests` inside a file this short is not worth flagging.
TESTS_PLACEMENT_MINIMUM_LINES = 20


# =============================================================================
# Violation collection
# =============================================================================


class ViolationCollector:
    """Records violations, prints them as they are found, and totals them up."""

    def __init__(self) -> None:
        self.violation_count = 0
        self.files_with_violations: List[str] = []
        self._seen_files: Dict[str, bool] = {}

    def record(self, display_path: str, check: str, detail: str) -> None:
        """Records one violation against a file."""
        self.violation_count += 1
        if display_path not in self._seen_files:
            self._seen_files[display_path] = True
            self.files_with_violations.append(display_path)
        print(f"  {colorize('x', ANSI_RED)} {display_path}")
        print(f"      {colorize('[' + check + ']', ANSI_YELLOW)} {detail}")


# =============================================================================
# File discovery
# =============================================================================


def find_rust_files(root: Path) -> List[Path]:
    """Returns every `.rs` file under a root, skipping build and VCS output.

    A single `.rs` file is a valid root, mirroring `find` accepting a file
    path. Results are sorted by their POSIX string so the order matches the
    shell version's `LC_ALL=C sort`.
    """
    if root.is_file():
        return [root] if root.suffix == ".rs" else []

    discovered: List[Path] = []
    for path in root.rglob("*.rs"):
        if EXCLUDED_DIRECTORY_NAMES.intersection(path.parts):
            continue
        discovered.append(path)
    return sorted(discovered, key=lambda path: path.as_posix())


def display_path_for(path: Path) -> str:
    """Renders a path relative to the repository root when possible."""
    try:
        return path.resolve().relative_to(REPOSITORY_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def read_lines(path: Path) -> Tuple[List[str], str]:
    """Reads a source file as a line list plus its raw text.

    Undecodable bytes are replaced rather than raising: a lint should report
    on a file with a stray byte, not abort the whole run.
    """
    text = path.read_text(encoding="utf-8", errors="replace")
    return text.splitlines(), text


# =============================================================================
# Check 1/5 - module header
# =============================================================================


def check_module_header(
    lines: List[str], text: str, display_path: str, collector: ViolationCollector
) -> None:
    """Requires a `//!` opening block and a `# Responsibilities` section."""
    first_content_line: Optional[str] = None
    for line in lines:
        if line.strip():
            first_content_line = line.lstrip()
            break

    if first_content_line is None:
        collector.record(display_path, "module header", "file is empty or whitespace-only")
        return
    if not first_content_line.startswith("//!"):
        collector.record(
            display_path, "module header", "first content line is not a //! module header"
        )
    if "# Responsibilities" not in text:
        collector.record(
            display_path, "module header", "missing '# Responsibilities' section"
        )


# =============================================================================
# Check 2/5 - safety comments near every `unsafe` token
# =============================================================================


def check_safety_comments(
    lines: List[str], display_path: str, collector: ViolationCollector
) -> None:
    """Requires a SAFETY justification above each `unsafe` block or impl.

    `unsafe fn` / `extern` / `const` / `trait` signature declarations are
    exempt: their contract belongs in a `/// # Safety` doc, not in a
    `// SAFETY:` comment above the signature.
    """
    last_safety_line = 0
    for line_number, line in enumerate(lines, start=1):
        if SAFETY_MARKER_PATTERN.search(line):
            last_safety_line = line_number
            continue
        if COMMENT_LINE_PATTERN.search(line):
            continue
        if BLANK_LINE_PATTERN.search(line):
            continue
        if not UNSAFE_TOKEN_PATTERN.search(line):
            continue
        if not UNSAFE_BLOCK_PATTERN.search(line) and UNSAFE_DECLARATION_PATTERN.search(line):
            continue
        if last_safety_line == 0 or line_number - last_safety_line > SAFETY_WINDOW_LINES:
            collector.record(
                display_path,
                "safety comments",
                f"line {line_number}: 'unsafe' has no // SAFETY: (or # Safety) "
                f"within {SAFETY_WINDOW_LINES} lines above",
            )


# =============================================================================
# Check 3/5 - public items carry a `///` doc comment
# =============================================================================


def check_public_item_docs(
    lines: List[str], display_path: str, collector: ViolationCollector
) -> None:
    """Requires a `///` doc comment above every public item."""
    last_doc_line = 0
    for line_number, raw_line in enumerate(lines, start=1):
        line = raw_line.strip()
        if DOC_COMMENT_PATTERN.search(line):
            last_doc_line = line_number
            continue
        # Blank lines and attributes sit between a doc comment and its item,
        # so they must not reset the window.
        if line == "" or line.startswith("#"):
            continue
        if not PUBLIC_ITEM_PATTERN.search(line):
            continue
        if last_doc_line == 0 or line_number - last_doc_line > DOC_WINDOW_LINES:
            collector.record(
                display_path,
                "public item docs",
                f"line {line_number}: '{line}' has no /// doc comment above",
            )


# =============================================================================
# Check 4/5 - import group headers appear in order
# =============================================================================


def check_import_group_order(
    lines: List[str], display_path: str, collector: ViolationCollector
) -> None:
    """Requires standard library, external crates, then current crate."""
    positions: Dict[str, Optional[int]] = {}
    for header, _ in IMPORT_GROUP_HEADERS:
        positions[header] = None
        for line_number, line in enumerate(lines, start=1):
            if line.startswith(header):
                positions[header] = line_number
                break

    # Every ordered pair is compared, so a file with only two of the three
    # headers is still checked.
    for index, (earlier_header, earlier_label) in enumerate(IMPORT_GROUP_HEADERS):
        for later_header, later_label in IMPORT_GROUP_HEADERS[index + 1 :]:
            earlier_line = positions[earlier_header]
            later_line = positions[later_header]
            if earlier_line is None or later_line is None:
                continue
            if earlier_line > later_line:
                collector.record(
                    display_path,
                    "import groups",
                    f"{earlier_label} (line {earlier_line}) appears after "
                    f"{later_label} (line {later_line})",
                )


# =============================================================================
# Check 5/5 - tests section sits at the bottom
# =============================================================================


def check_tests_at_bottom(
    lines: List[str], text: str, display_path: str, collector: ViolationCollector
) -> None:
    """Requires that no new top-level section follows `mod tests`.

    A column-0 `// ====` separator after the tests module means another
    section starts below it. Content inside `mod tests { }` is indented, so a
    large test module that is still last passes.
    """
    # Newline count, matching `wc -l` in the shell version.
    if text.count("\n") < TESTS_PLACEMENT_MINIMUM_LINES:
        return

    tests_line: Optional[int] = None
    for line_number, line in enumerate(lines, start=1):
        if TESTS_MODULE_PATTERN.search(line):
            tests_line = line_number
            break
    if tests_line is None:
        return

    for line_number, line in enumerate(lines, start=1):
        if line_number <= tests_line:
            continue
        if TOP_LEVEL_SECTION_PATTERN.search(line):
            collector.record(
                display_path,
                "tests placement",
                f"a new top-level section starts at line {line_number}, after "
                f"the tests module at line {tests_line}",
            )
            return


# =============================================================================
# Entry point
# =============================================================================


def build_parser() -> argparse.ArgumentParser:
    """Builds the command-line parser."""
    parser = argparse.ArgumentParser(
        prog="test_coding_standards.py",
        description=(
            "Checks .rs files against the Pill comment and layout rules. "
            "Exit 0 when every file complies, 1 on violations, 2 on a usage error."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  test_coding_standards.py\n"
            "  test_coding_standards.py --list\n"
            "  test_coding_standards.py --root modules/pill_engine/src\n"
            "  test_coding_standards.py modules/pill_core/src/lib.rs\n"
        ),
    )
    parser.add_argument(
        "--root",
        default=None,
        metavar="PATH",
        help="Scan a directory instead of the repository root",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="Print every .rs file that would be checked, then exit",
    )
    parser.add_argument(
        "target",
        nargs="*",
        metavar="PATH",
        help="A single file or directory to scan, instead of --root",
    )
    return parser


def resolve_root(arguments: argparse.Namespace) -> Path:
    """Resolves the scan target from `--root` or a positional path.

    Raises `SystemExit(2)` on a usage error, matching the shell version's
    exit-status contract.
    """
    if len(arguments.target) > 1:
        print("ERROR: only one scan target is accepted", file=sys.stderr)
        raise SystemExit(2)
    if arguments.target:
        root = Path(arguments.target[0])
    elif arguments.root:
        root = Path(arguments.root)
    else:
        root = REPOSITORY_ROOT
    if not root.exists():
        print(f"ERROR: scan target '{root}' does not exist", file=sys.stderr)
        raise SystemExit(2)
    return root


def main() -> int:
    """Scans the target and reports every violation found."""
    arguments = build_parser().parse_args()
    root = resolve_root(arguments)
    files = find_rust_files(root)

    if arguments.list:
        for path in files:
            print(display_path_for(path))
        print()
        print(f"Total: {len(files)} .rs file(s) under {display_path_for(root)}")
        return 0

    print(colorize("Pill comment & layout lint", ANSI_BOLD))
    print(f"Scanning {len(files)} .rs file(s) under {display_path_for(root)}")
    if not files:
        print("No Rust source files found.")
        return 0

    collector = ViolationCollector()
    # Every file is read once and its lines reused by all five checks; the
    # shell version re-read each file per check.
    sources = [(path, display_path_for(path), *read_lines(path)) for path in files]

    section("(1/5) Module header")
    for _, display_path, lines, text in sources:
        check_module_header(lines, text, display_path, collector)

    section("(2/5) Safety comments")
    for _, display_path, lines, _text in sources:
        check_safety_comments(lines, display_path, collector)

    section("(3/5) Public item docs")
    for _, display_path, lines, _text in sources:
        check_public_item_docs(lines, display_path, collector)

    section("(4/5) Import group order")
    for _, display_path, lines, _text in sources:
        check_import_group_order(lines, display_path, collector)

    section("(5/5) Tests at the bottom")
    for _, display_path, lines, text in sources:
        check_tests_at_bottom(lines, text, display_path, collector)

    print()
    if collector.violation_count == 0:
        print(
            colorize(
                f"PASS: all {len(files)} files comply with the Pill comment & "
                "layout rules.",
                ANSI_GREEN,
            )
        )
        return 0

    print(
        colorize(
            f"FAIL: {collector.violation_count} violation(s) across "
            f"{len(collector.files_with_violations)} file(s).",
            ANSI_RED,
        )
    )
    print()
    print("Files that need attention:")
    for display_path in collector.files_with_violations:
        print(f"  - {display_path}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
