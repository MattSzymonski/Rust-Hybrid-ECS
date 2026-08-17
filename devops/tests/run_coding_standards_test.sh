#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, find, grep, awk, sed, wc, sort.
#
# DESCRIPTION: Lints every Rust source file under the repository against the
#   Pill comment and layout rules (see
#   [Pill Guide - Comment and Layout Rules](https://guide.pill.rocks/contributing/coding-standards.html)).
#   The checks are grep/awk heuristics, so they are a fast first-pass signal,
#   not a substitute for the full per-file audit agents.
#
#   Checks performed:
#     1. module header   - every file opens with a `//!` block and contains a
#                          `# Responsibilities` section.
#     2. safety comments - every `unsafe` token has a `// SAFETY:` (or
#                          `# Safety`) justification within a bounded window
#                          above it.
#     3. public docs     - every `pub` item (fn/struct/enum/trait/const/static/
#                          type/mod) is preceded by a `///` doc comment.
#     4. import groups   - the `// Standard library`, `// External crates`, and
#                          `// Current crate` headers appear in that order.
#     5. tests at bottom - a `mod tests` module is the last top-level
#                          section of the file (no new section follows it).
#
# USAGE: bash devops/run_coding_standards_test.sh [--root <path>] [--list] [--help]
#        bash devops/run_coding_standards_test.sh [path/to/file.rs | path/to/dir]
#
# EXAMPLE USAGE:
#   bash devops/run_coding_standards_test.sh
#   bash devops/run_coding_standards_test.sh --list
#   bash devops/run_coding_standards_test.sh --root engine/src
#   bash devops/run_coding_standards_test.sh engine/src/world.rs
#
# Exit status: 0 when every checked file complies, 1 when at least one
# violation is reported, 2 on usage or internal errors.

# --- SCRIPT ---

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Tunable heuristic bounds (override via environment if needed).
SAFETY_WINDOW_LINES="${SAFETY_WINDOW_LINES:-30}"   # max lines between a SAFETY marker and an `unsafe` token
DOC_WINDOW_LINES="${DOC_WINDOW_LINES:-12}"         # max lines between a `///` line and the item it documents
# Colors only when the terminal supports them.
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; NC=''
fi

# ---------------------------------------------------------------------------
# Shared state
# ---------------------------------------------------------------------------

violation_count=0
files_with_violations=0
declare -a violation_files=()
declare -A file_seen=()

# Record one violation, print it, and remember the file for the summary.
record_violation() {
    local file="$1"
    local check="$2"
    local detail="$3"
    violation_count=$((violation_count + 1))
    if [[ -z "${file_seen["$file"]:-}" ]]; then
        file_seen["$file"]=1
        files_with_violations=$((files_with_violations + 1))
        violation_files+=("$file")
    fi
    echo -e "  ${RED}x${NC} ${file}"
    echo -e "      ${YELLOW}[${check}]${NC} ${detail}"
}

# Print a coloured section banner.
section() {
    local title="$1"
    echo ""
    echo -e "${BOLD}${CYAN}===============================================================================${NC}"
    echo -e "${BOLD}${CYAN}${title}${NC}"
}

# ---------------------------------------------------------------------------
# File discovery
# ---------------------------------------------------------------------------

# List every .rs file under the given root, skipping build output and VCS
# directories. A single file path also works because `find` accepts it.
find_rust_files() {
    local root="$1"
    find "$root" -type f -name '*.rs' \
        -not -path '*/target/*' \
        -not -path '*/node_modules/*' \
        -not -path '*/.git/*' \
        -not -path '*/standalone_temp/*' \
        2>/dev/null | LC_ALL=C sort
}

# ---------------------------------------------------------------------------
# Check 1/5 - module header
# ---------------------------------------------------------------------------

check_module_header() {
    local file="$1"
    local first_line
    first_line="$(grep -m1 -v '^[[:space:]]*$' "$file" 2>/dev/null | sed -e 's/^[[:space:]]*//' || true)"
    if [[ -z "$first_line" ]]; then
        record_violation "$file" "module header" "file is empty or whitespace-only"
        return
    fi
    if [[ "$first_line" != //!* ]]; then
        record_violation "$file" "module header" "first content line is not a //! module header"
    fi
    if ! grep -q '# Responsibilities' "$file" 2>/dev/null; then
        record_violation "$file" "module header" "missing '# Responsibilities' section"
    fi
}

# ---------------------------------------------------------------------------
# Check 2/5 - safety comments near every `unsafe` token
# ---------------------------------------------------------------------------

check_safety_comments() {
    local file="$1"
    local -a offending_lines=()
    local line_number
    mapfile -t offending_lines < <(
        awk -v window="$SAFETY_WINDOW_LINES" '
            /\/\/ *SAFETY:/ || /# *Safety/ { last_safety = NR; next }
            /^[[:space:]]*\/\// { next }
            /^[[:space:]]*$/ { next }
            # \b is a backspace in gawk, so match word boundaries explicitly.
            /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/ {
                # Skip `unsafe fn` / `unsafe extern` / `unsafe const` /
                # `unsafe trait` signature declarations: their safety
                # contract lives in `/// # Safety` docs (or none at all for
                # function-pointer type aliases). Only `unsafe {` blocks,
                # `unsafe impl`, and expression-position `unsafe` need a
                # `// SAFETY:` justification above them.
                if ($0 !~ /unsafe[[:space:]]*\{/ &&
                    $0 ~ /unsafe[[:space:]]+(fn|extern|const|trait)/) {
                    next
                }
                if (last_safety == 0 || NR - last_safety > window) {
                    print NR
                }
            }
        ' "$file"
    )
    for line_number in "${offending_lines[@]}"; do
        record_violation "$file" "safety comments" \
            "line $line_number: 'unsafe' has no // SAFETY: (or # Safety) within $SAFETY_WINDOW_LINES lines above"
    done
}

# ---------------------------------------------------------------------------
# Check 3/5 - public items carry a `///` doc comment
# ---------------------------------------------------------------------------

check_public_item_docs() {
    local file="$1"
    local -a offending_entries=()
    local entry line_number detail
    mapfile -t offending_entries < <(
        awk -v window="$DOC_WINDOW_LINES" '
            {
                line = $0
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
                if (line ~ /^\/\/\//) { last_doc = NR; next }
                if (line == "" || line ~ /^#/) { next }
                # \b is a backspace in gawk, so match the end of the item
                # keyword with an explicit class instead.
                if (line ~ /^pub([[:space:]]*\((crate|super)\))?([[:space:]]+unsafe)?[[:space:]]+(fn|struct|enum|trait|const|static|type|mod)([^A-Za-z0-9_]|$)/) {
                    if (last_doc == 0 || NR - last_doc > window) {
                        printf "%d:%s\n", NR, line
                    }
                }
            }
        ' "$file"
    )
    for entry in "${offending_entries[@]}"; do
        line_number="${entry%%:*}"
        detail="${entry#*:}"
        record_violation "$file" "public item docs" \
            "line $line_number: '${detail}' has no /// doc comment above"
    done
}

# ---------------------------------------------------------------------------
# Check 4/5 - import group headers appear in order
# ---------------------------------------------------------------------------

check_import_group_order() {
    local file="$1"
    local standard_line="" external_line="" current_line=""
    standard_line="$(grep -n -m1 '^// Standard library' "$file" 2>/dev/null | cut -d: -f1 || true)"
    external_line="$(grep -n -m1 '^// External crates' "$file" 2>/dev/null | cut -d: -f1 || true)"
    current_line="$(grep -n -m1 '^// Current crate' "$file" 2>/dev/null | cut -d: -f1 || true)"

    if [[ -n "$standard_line" && -n "$external_line" && "$standard_line" -gt "$external_line" ]]; then
        record_violation "$file" "import groups" \
            "'// Standard library' (line $standard_line) appears after '// External crates' (line $external_line)"
    fi
    if [[ -n "$external_line" && -n "$current_line" && "$external_line" -gt "$current_line" ]]; then
        record_violation "$file" "import groups" \
            "'// External crates' (line $external_line) appears after '// Current crate' (line $current_line)"
    fi
    if [[ -n "$standard_line" && -n "$current_line" && "$standard_line" -gt "$current_line" ]]; then
        record_violation "$file" "import groups" \
            "'// Standard library' (line $standard_line) appears after '// Current crate' (line $current_line)"
    fi
}

# ---------------------------------------------------------------------------
# Check 5/5 - tests section sits at the bottom
# ---------------------------------------------------------------------------

check_tests_at_bottom() {
    local file="$1"
    local total test_line after_line
    total="$(wc -l < "$file" | tr -d ' ')"
    # Small files are exempt: a `mod tests` in a 15-line file is not worth
    # flagging.
    if [[ "$total" -lt 20 ]]; then
        return
    fi
    # Find the canonical tests module. A bare `#[cfg(test)]` attribute on a
    # helper method is not a tests section, so only `mod tests` counts.
    test_line="$(grep -n -E '^\s*mod tests([[:space:]]|;)' "$file" 2>/dev/null | head -n 1 | cut -d: -f1 || true)"
    if [[ -z "$test_line" ]]; then
        return
    fi
    # The rule is "tests at the bottom": a column-0 `// ====…====` separator
    # AFTER the tests module means a new top-level section follows it. Content
    # inside `mod tests { }` is indented, so only genuine post-tests sections
    # match. Large test modules that are still the last section pass.
    after_line="$(awk -v start="$test_line" 'NR > start && /^\/\/ ====/ { print NR; exit }' "$file" || true)"
    if [[ -n "$after_line" ]]; then
        record_violation "$file" "tests placement" \
            "a new top-level section starts at line $after_line, after the tests module at line $test_line"
    fi
}

# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------

usage() {
    cat <<'EOF'
Usage: bash devops/check_comment_layout.sh [options] [path]

Checks .rs files against the Pill comment and layout rules.

Options:
  --root <path>   scan a directory instead of the repository root
  --list          print every .rs file that would be checked, then exit
  -h, --help      show this help

A single positional path (a file or a directory) may be given instead of
--root.

Examples:
  bash devops/check_comment_layout.sh
  bash devops/check_comment_layout.sh --list
  bash devops/check_comment_layout.sh --root engine/src
  bash devops/check_comment_layout.sh engine/src/world.rs
EOF
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

main() {
    local root="$REPO_ROOT"
    local mode="check"
    local -a positional_args=()

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --root)
                shift
                root="${1:-}"
                if [[ -z "$root" ]]; then
                    echo "ERROR: --root requires a path" >&2
                    return 2
                fi
                ;;
            --list)
                mode="list"
                ;;
            -h|--help)
                usage
                return 0
                ;;
            -*)
                echo "ERROR: unknown option '$1'" >&2
                usage >&2
                return 2
                ;;
            *)
                positional_args+=("$1")
                ;;
        esac
        shift
    done

    if [[ ${#positional_args[@]} -gt 0 ]]; then
        root="${positional_args[0]}"
        if [[ ${#positional_args[@]} -gt 1 ]]; then
            echo "ERROR: only one scan target is accepted" >&2
            return 2
        fi
    fi

    if [[ ! -e "$root" ]]; then
        echo "ERROR: scan target '$root' does not exist" >&2
        return 2
    fi

    local -a files=()
    mapfile -t files < <(find_rust_files "$root")

    if [[ "$mode" == "list" ]]; then
        for file in "${files[@]}"; do
            echo "$file"
        done
        echo ""
        echo "Total: ${#files[@]} .rs file(s) under $root"
        return 0
    fi

    echo -e "${BOLD}Pill comment & layout lint${NC}"
    echo "Scanning ${#files[@]} .rs file(s) under $root"
    if [[ ${#files[@]} -eq 0 ]]; then
        echo "No Rust source files found."
        return 0
    fi

    section "(1/5) Module header"
    for file in "${files[@]}"; do check_module_header "$file"; done

    section "(2/5) Safety comments"
    for file in "${files[@]}"; do check_safety_comments "$file"; done

    section "(3/5) Public item docs"
    for file in "${files[@]}"; do check_public_item_docs "$file"; done

    section "(4/5) Import group order"
    for file in "${files[@]}"; do check_import_group_order "$file"; done

    section "(5/5) Tests at the bottom"
    for file in "${files[@]}"; do check_tests_at_bottom "$file"; done

    echo ""
    if [[ "$violation_count" -eq 0 ]]; then
        echo -e "${GREEN}PASS: all ${#files[@]} files comply with the Pill comment & layout rules.${NC}"
        return 0
    fi
    echo -e "${RED}FAIL: $violation_count violation(s) across $files_with_violations file(s).${NC}"
    echo ""
    echo "Files that need attention:"
    for file in "${violation_files[@]}"; do
        echo "  - $file"
    done
    return 1
}

main "$@"
