#!/usr/bin/env python3
"""
Pill comment and layout lint for every Rust source file in the repository.

PROTECTS: Every .rs file follows the Pill comment and layout rules.
TIER: static

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Lints every `.rs` file under the repository against the Pill comment and
    layout rules (see the Pill Guide, "Contributing / Coding standards"). The
    checks are line-oriented heuristics, so they are a fast first-pass signal,
    not a substitute for a full per-file audit.

    The heuristics were ported from an earlier shell script, deliberate
    exemptions included.

    Checks performed:
      1. module header   - every file opens with a `//!` block and contains a
                           `# Responsibilities` section.
      2. safety comments - every `unsafe` token outside a string literal has a
                           `// SAFETY:` (or `# Safety`) justification within a
                           bounded window
                           above it.
      3. public docs     - every `pub` item (fn/struct/enum/trait/const/
                           static/type/mod) is preceded by a `///` doc comment.
      4. import groups   - the `// Standard library`, `// External crates` and
                           `// Current crate` headers appear in that order.
      5. tests at bottom - no new top-level section starts after `mod tests`.
      6. egui state     - a reloadable crate (`extensions/**`, `examples/**`)
                          never stores its own types or callbacks inside
                          egui's `Context` (plan decision E4, W-0017): no
                          `data`/`data_mut`, repaint callbacks, plugin
                          callbacks or `PaintCallback` shapes.

    The two window sizes are tunable through the environment, matching the
    shell version: `SAFETY_WINDOW_LINES` (default 30) and `DOC_WINDOW_LINES`
    (default 12).

USAGE
  python devops/tests/test_coding_standards.py [--root <path>] [--list]
                                              [--self-test] [--report <path>]
                                              [--exclude <path>]...
  python devops/tests/test_coding_standards.py <path/to/file.rs | path/to/dir> ...

EXAMPLE USAGE
  python devops/tests/test_coding_standards.py
  python devops/tests/test_coding_standards.py --list
  python devops/tests/test_coding_standards.py --self-test
  python devops/tests/test_coding_standards.py --root modules/pill_engine/src
  python devops/tests/test_coding_standards.py modules/pill_core/src/lib.rs
  python devops/tests/test_coding_standards.py modules/pill_core/src/lib.rs \
      examples/project_rs/src/lib.rs --report local/lint.json
  python devops/tests/test_coding_standards.py --root modules \
      --exclude modules/extensions/pill_master_renderer

  Exit status: 0 when every checked file complies, 1 when at least one
  violation is reported, 2 on a usage error.

--- SCRIPT ---
"""

import argparse
import json
import os
import re
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.paths import REPOSITORY_ROOT  # noqa: E402
from core.suite_common import run_suite_with_timing  # noqa: E402
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
# `build` covers cargo OUT_DIR staging under `examples/*/build/` and the
# generated shipping bundle scratch under the repo-root `build/` - with one
# exception, the tracked stub crate `build/pill_shipping_bundle` (see
# `is_linted`).
EXCLUDED_DIRECTORY_NAMES = {
    "target",
    "node_modules",
    ".git",
    "pill_standalone_temp",
    "build",
    # The host writes `host_module_*` wrappers and `host_project_*` members at
    # runtime; both are gitignored, the guard hook refuses edits to them, and
    # a git-ignored file can never reach the changed-files hook - so linting
    # them in CI would gate on files nobody can fix there.
    "generated",
    "projects_generated",
    # `local/` is gitignored scratch - experiment labs, notes, one-off probes.
    # It is not engine source and is not tracked, so holding it to the engine's
    # comment and layout rules only ever produced noise. Excluding it here
    # rather than in one caller is what makes a bare run and a `--root modules`
    # run agree, which is the disagreement that made `run_all.py` red while the
    # documented gate was green.
    "local",
}

# The repository's own `build/` holds generated scratch - and one tracked
# crate whose `.gitignore` negations keep its source in the tree: the shipping
# bundle stub the generator overwrites. A `.rs` in there is source and is
# linted; everything else under a `build/`, everywhere, is not.
BUILD_SOURCE_CRATES = ("pill_shipping_bundle",)

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

# The APIs that keep boxed `Any`/`Fn` values inside egui's `Context` (plan
# decision E4, W-0017). A reloadable crate that stores its own types or
# callbacks through them leaves dangling type IDs, drop code and vtables
# behind after a reload, so the egui shell keeps its long-lived state in a
# world resource instead; the contract is documented in `pill_editor_core`'s
# crate docs. The patterns are dot-prefixed so an unrelated `metadata::<`
# turbofish cannot trip the `data` rule, and the closure forms require a `|`
# after the parenthesis so a handle's `.data()` accessor stays legal.
#
# The map's own methods are listed as well as its two entry points: a write
# through a borrow taken elsewhere (`memory.data.insert_temp(..)`) never
# spells `.data_mut::<`, and those are the forms a refactor produces.
EGUI_CONTEXT_STORAGE_PATTERNS = (
    (re.compile(r"\.data_mut::<"), "stores a type in egui's Context (IdTypeMap)"),
    (re.compile(r"\.data::<"), "reads a type from egui's Context (IdTypeMap)"),
    (
        re.compile(r"\.data_mut\s*\(\s*\|"),
        "writes egui's Context data map from a closure (IdTypeMap)",
    ),
    (
        re.compile(r"\.data\s*\(\s*\|"),
        "reads egui's Context data map from a closure (IdTypeMap)",
    ),
    (re.compile(r"\.insert_temp\b"), "stores a value in egui's Context (IdTypeMap)"),
    (
        re.compile(r"\.insert_persisted\b"),
        "stores a persisted value in egui's Context (IdTypeMap)",
    ),
    (
        re.compile(r"\.get_temp(?:_mut_or)?\b"),
        "reads a value from egui's Context (IdTypeMap)",
    ),
    (
        re.compile(r"\.get_persisted(?:_mut_or)?\b"),
        "reads a persisted value from egui's Context (IdTypeMap)",
    ),
    (
        re.compile(r"\.remove_by_type\b"),
        "drops a type's values from egui's Context (IdTypeMap)",
    ),
    (re.compile(r"\.set_request_repaint_callback"), "stores a callback in egui's Context"),
    (re.compile(r"\.on_begin_pass"), "installs an egui plugin callback"),
    (re.compile(r"\.on_end_pass"), "installs an egui plugin callback"),
    (re.compile(r"PaintCallback"), "paints a callback shape carrying user data"),
    (
        re.compile(r"\bdyn Any\b"),
        "erases a value into a trait object whose drop code lives in this "
        "reloadable image",
    ),
)

# A line that deliberately touches the map says so: the marker, on the line
# itself or the one above, exempts that single line. Only a test planting a
# previous generation's value uses it - production code in a reloadable crate
# never does, and the exemption is per line so it cannot cover a module.
EGUI_STATE_MARKER_PATTERN = re.compile(r"egui-state-ok")


# =============================================================================
# Violation collection
# =============================================================================


class ViolationCollector:
    """Records violations, prints them as they are found, and totals them up."""

    def __init__(self) -> None:
        self.violation_count = 0
        self.files_with_violations: List[str] = []
        self.entries: List[Tuple[str, str, str]] = []
        self._seen_files: Dict[str, bool] = {}

    def record(self, display_path: str, check: str, detail: str) -> None:
        """Records one violation against a file."""
        self.violation_count += 1
        self.entries.append((display_path, check, detail))
        if display_path not in self._seen_files:
            self._seen_files[display_path] = True
            self.files_with_violations.append(display_path)
        print(f"  {colorize('x', ANSI_RED)} {display_path}")
        print(f"      {colorize('[' + check + ']', ANSI_YELLOW)} {detail}")


# =============================================================================
# File discovery
# =============================================================================


def is_linted(path: Path) -> bool:
    """Whether the lint checks this path: a `.rs` source outside every excluded
tree.

    One predicate for every caller. CI scans with it (through
    `find_rust_files`), the changed-files hook imports it to select what it
    passes in - and an explicitly named file is subject to it too, so a hook
    run and a CI run can never disagree about a file again. Before this was
    shared, the hook linted single files before any exclusions applied while CI
    scanned `modules/` only, so a file could pass one and fail the other.
    """
    if path.suffix != ".rs":
        return False
    try:
        relative = path.relative_to(REPOSITORY_ROOT)
    except ValueError:
        relative = path
    for index, part in enumerate(relative.parts):
        if part not in EXCLUDED_DIRECTORY_NAMES:
            continue
        # The repository's own `build/` keeps one tracked crate's sources: the
        # shipping bundle stub's `src/`. Everything else under a `build/` -
        # a stray `build.rs` in that crate included - is generated scratch.
        if (
            part == "build"
            and index == 0
            and relative.parts[1:2]
            and relative.parts[1] in BUILD_SOURCE_CRATES
            and relative.parts[2:3] == ("src",)
        ):
            continue
        return False
    return True


def find_rust_files(root: Path, excludes: Optional[List[Path]] = None) -> List[Path]:
    """Returns every `.rs` file under a root, skipping build and VCS output.

    A single `.rs` file is a valid root, mirroring `find` accepting a file
    path. Results are sorted by their POSIX string so the order matches the
    shell version's `LC_ALL=C sort`.

    `excludes` names files or directory subtrees to leave out entirely - the
    escape hatch for a tree that is being brought to the standard rather than
    expected to meet it (CI passes the in-progress renderer this way, so the
    rules still gate every other crate).
    """
    if root.is_file():
        return [root] if is_linted(root) else []

    resolved_excludes = [path.resolve() for path in (excludes or [])]
    discovered: List[Path] = []
    for path in root.rglob("*.rs"):
        if not is_linted(path):
            continue
        resolved = path.resolve()
        if any(
            resolved == exclude or exclude in resolved.parents
            for exclude in resolved_excludes
        ):
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


def first_code_match(pattern, line: str):
    """Returns the first match of `pattern` that is not inside a double-quoted string.

    The lint reads Rust as text, and `unsafe` appears inside string literals in
    this codebase - the host's codegen carries a C# keyword table that lists the
    word. A quoted token is data, not a declaration, so counting unescaped
    quotes before the `unsafe` word itself (an odd count means the word sits
    inside a string) tells the two apart without parsing Rust. The count starts
    at the word, not at the match, because the surrounding pattern deliberately
    includes the character on either side of it.
    """
    for match in pattern.finditer(line):
        word_start = match.start() + match.group(0).find("unsafe")
        prefix = line[:word_start]
        quotes = 0
        index = 0
        while index < len(prefix):
            if prefix[index] == "\\":
                index += 2
                continue
            if prefix[index] == '"':
                quotes += 1
            index += 1
        if quotes % 2 == 0:
            return match
    return None


def check_safety_comments(
    lines: List[str], display_path: str, collector: ViolationCollector
) -> None:
    """Requires a SAFETY justification above each `unsafe` block or impl.

    `unsafe fn` / `extern` / `const` / `trait` signature declarations are
    exempt: their contract belongs in a `/// # Safety` doc, not in a
    `// SAFETY:` comment above the signature. A token inside a string literal
    is data, not code, and is skipped entirely.
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
        if first_code_match(UNSAFE_TOKEN_PATTERN, line) is None:
            continue
        if first_code_match(UNSAFE_BLOCK_PATTERN, line) is None and first_code_match(
            UNSAFE_DECLARATION_PATTERN, line
        ) is not None:
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
    """Requires a `///` doc comment above every public item.

    Lines inside a raw string are skipped: test fixtures embed Rust source
    verbatim (`const PLAIN_SOURCE: &str = r#"..."#`), and the lint must read
    that text as data rather than as items to document.
    """
    last_doc_line = 0
    in_raw_string = False
    for line_number, raw_line in enumerate(lines, start=1):
        line = raw_line.strip()
        if in_raw_string:
            if '"#' in raw_line:
                in_raw_string = False
            continue
        raw_open = raw_line.find('r#"')
        if raw_open != -1:
            if '"#' not in raw_line[raw_open + 3 :]:
                in_raw_string = True
            continue
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
# Check 6/6 - no egui context storage in reloadable crates
# =============================================================================


# Whether a display path belongs to a crate that reloads while it runs:
# extension crates and example projects do; the engine, the hosts and the
# editor frontends do not.
def is_reloadable_source(display_path: str) -> bool:
    """Whether the egui-storage rule applies to this path."""
    parts = display_path.split("/")
    return "extensions" in parts or "examples" in parts


# Flags the egui context-storage APIs; see `EGUI_CONTEXT_STORAGE_PATTERNS`.
def check_egui_context_storage(
    lines: List[str], display_path: str, collector: ViolationCollector
) -> None:
    """Flags egui context-storage APIs in crates that reload at runtime.

    The same calls are fine in shared libraries and frontends, which never
    unload; the rule is scoped to `extensions/**` and `examples/**`, where
    the stored value may outlive the image that stored it. A line carrying
    `egui-state-ok` - or sitting right below one - is exempt: a test that
    plants a previous generation's value is the only legitimate user.
    """
    if not is_reloadable_source(display_path):
        return
    for index, line in enumerate(lines):
        if COMMENT_LINE_PATTERN.match(line):
            continue
        if EGUI_STATE_MARKER_PATTERN.search(line):
            continue
        if index > 0 and EGUI_STATE_MARKER_PATTERN.search(lines[index - 1]):
            continue
        for pattern, detail in EGUI_CONTEXT_STORAGE_PATTERNS:
            if pattern.search(line):
                collector.record(
                    display_path,
                    "egui state",
                    f"line {index + 1}: {detail}; keep long-lived egui state "
                    "in a world resource (pill_editor_core's reload-safety contract)",
                )


# =============================================================================
# Self test
# =============================================================================


class SilentCollector(ViolationCollector):
    """A collector for the self test: counts violations without printing."""

    def record(self, display_path: str, check: str, detail: str) -> None:
        """Counts one violation, skipping the per-file printout."""
        self.violation_count += 1
        self.entries.append((display_path, check, detail))


# Runs the egui-storage rule against planted sources: the rule must fire on
# a reloadable crate and stay quiet everywhere else.
def self_test() -> int:
    """Proves the egui-storage rule fails its plant and only its plant."""
    cases = (
        (
            "a plant in an extension crate fails",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "let stored = context.data_mut::<Widget>();\n",
            1,
        ),
        (
            "the same plant outside a reloadable crate passes",
            "modules/frontents/pill_editor/src/runner.rs",
            "let stored = context.data_mut::<Widget>();\n",
            0,
        ),
        (
            "a comment naming the rule passes",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "// never call context.data_mut::<Widget>() here\n",
            0,
        ),
        (
            "a paint callback in an example fails",
            "examples/acme/src/lib.rs",
            "let shape = egui::Shape::Callback(PaintCallback { .. });\n",
            1,
        ),
        (
            "an unrelated metadata turbofish passes",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "let value = entry.metadata::<Todo>();\n",
            0,
        ),
        (
            "a closure write to the context map fails",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "context.data_mut(|data| { let _ = data; });\n",
            1,
        ),
        (
            "a write through a borrowed map fails",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "memory.data.insert_temp(id, 1);\n",
            1,
        ),
        (
            "a read through a borrowed map fails",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "let row = memory.data.get_temp::<Row>(id);\n",
            1,
        ),
        (
            "a type-erased value in a reloadable crate fails",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "let held: Box<dyn Any> = Box::new(1u8);\n",
            1,
        ),
        (
            "a marked plant passes",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "// egui-state-ok: planting a previous generation's value\n"
            "memory.data.insert_temp(id, 1);\n",
            0,
        ),
        (
            "a handle's data() accessor passes",
            "modules/extensions/acme/acme_editor/src/lib.rs",
            "let shader = handle.data();\n",
            0,
        ),
    )
    passed = True
    for description, display_path, source, expected in cases:
        collector = SilentCollector()
        check_egui_context_storage(source.splitlines(), display_path, collector)
        outcome = "PASS" if collector.violation_count == expected else "FAIL"
        passed &= outcome == "PASS"
        print(f"  {outcome}  {description}")
    return 0 if passed else 1


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
            "  test_coding_standards.py --root modules --exclude "
            "modules/extensions/pill_master_renderer\n"
        ),
    )
    parser.add_argument(
        "--root",
        default=None,
        metavar="PATH",
        help="Scan a directory instead of the repository root",
    )
    parser.add_argument(
        "--exclude",
        action="append",
        default=[],
        metavar="PATH",
        help="Skip a file or directory subtree (repeatable)",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="Print every .rs file that would be checked, then exit",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        dest="self_test",
        help="Run the planted-violation self test of the egui-state rule, then exit",
    )
    parser.add_argument(
        "--report",
        metavar="PATH",
        help="Write a JSON report of the checked files and violations to PATH",
    )
    parser.add_argument(
        "target",
        nargs="*",
        metavar="PATH",
        help="Files or directories to scan, instead of --root (any number)",
    )
    return parser


def collect_files(targets: List[Path], excludes: List[Path]) -> List[Path]:
    """Every linted file under the targets, deduplicated, in path order.

    More than one target arrives from callers that already know their file
    list - the changed-files hook passes the files it may have touched - so a
    directory, a file and a mixture all work in one call.
    """
    found: List[Path] = []
    seen = set()
    for target in targets:
        for path in find_rust_files(target, excludes):
            key = path.resolve()
            if key in seen:
                continue
            seen.add(key)
            found.append(path)
    return sorted(found, key=lambda path: path.as_posix())


def write_report(path: Path, files: List[Path], collector: ViolationCollector) -> None:
    """Writes the machine-readable report `--report` promises.

    The human output stays exactly as it was - CI and `run_all.py` read it -
    and this gives callers a stable shape to attribute violations by file
    instead of scraping coloured text.
    """
    document = {
        "files_checked": [display_path_for(file) for file in files],
        "violations": [
            {"file": display_path, "check": check, "detail": detail}
            for display_path, check, detail in collector.entries
        ],
    }
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def resolve_targets(arguments: argparse.Namespace) -> List[Path]:
    """Resolves the scan targets from `--root` or the positional paths.

    Raises `SystemExit(2)` on a usage error, matching the shell version's
    exit-status contract.
    """
    if arguments.target:
        targets = [Path(entry) for entry in arguments.target]
    elif arguments.root:
        targets = [Path(arguments.root)]
    else:
        targets = [REPOSITORY_ROOT]
    for target in targets:
        if not target.exists():
            print(f"ERROR: scan target '{target}' does not exist", file=sys.stderr)
            raise SystemExit(2)
    return targets


def main() -> int:
    """Scans the targets and reports every violation found."""
    arguments = build_parser().parse_args()
    if arguments.self_test:
        print(colorize("Pill comment & layout lint - self test", ANSI_BOLD))
        return self_test()
    targets = resolve_targets(arguments)
    files = collect_files(targets, [Path(entry) for entry in arguments.exclude])

    if arguments.list:
        for path in files:
            print(display_path_for(path))
        print()
        print(f"Total: {len(files)} .rs file(s) under {display_path_for(targets[0])}" + (
            f" and {len(targets) - 1} more target(s)" if len(targets) > 1 else ""
        ))
        return 0

    under = ", ".join(display_path_for(target) for target in targets)
    print(colorize("Pill comment & layout lint", ANSI_BOLD))
    print(f"Scanning {len(files)} .rs file(s) under {under}")
    if not files:
        print("No Rust source files found.")
        return 0

    collector = ViolationCollector()
    # Every file is read once and its lines reused by all six checks; the
    # shell version re-read each file per check.
    sources = [(path, display_path_for(path), *read_lines(path)) for path in files]

    section("(1/6) Module header")
    for _, display_path, lines, text in sources:
        check_module_header(lines, text, display_path, collector)

    section("(2/6) Safety comments")
    for _, display_path, lines, _text in sources:
        check_safety_comments(lines, display_path, collector)

    section("(3/6) Public item docs")
    for _, display_path, lines, _text in sources:
        check_public_item_docs(lines, display_path, collector)

    section("(4/6) Import group order")
    for _, display_path, lines, _text in sources:
        check_import_group_order(lines, display_path, collector)

    section("(5/6) Tests at the bottom")
    for _, display_path, lines, text in sources:
        check_tests_at_bottom(lines, text, display_path, collector)

    section("(6/6) egui state (reloadable crates)")
    for _, display_path, lines, _text in sources:
        check_egui_context_storage(lines, display_path, collector)

    if arguments.report:
        write_report(Path(arguments.report), files, collector)

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
    sys.exit(run_suite_with_timing(main))
