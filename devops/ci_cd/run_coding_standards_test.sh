#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, Python 3.8+ on PATH.

# DESCRIPTION: Thin runner for the Pill comment and layout lint. All of the
#   checking logic lives in devops/tests/test_coding_standards.py; this script
#   only locates the repository root and forwards its arguments, so the lint
#   behaves identically whether it is invoked here or directly:
#
#     python devops/tests/test_coding_standards.py --root modules
#
#   The lint checks every .rs file for a //! module header with a
#   '# Responsibilities' section, // SAFETY: justifications above unsafe
#   blocks, /// docs on public items, ordered import group headers, and a
#   'mod tests' that is the last top-level section.
#
#   Designed for both local development and GitHub Actions CI.

# USAGE: bash devops/ci_cd/run_coding_standards_test.sh [options] [path]
#
#   --root <path>   scan a directory instead of the repository root
#   --list          print every .rs file that would be checked, then exit
#   -h, --help      show this help
#
#   A single positional path (a file or a directory) may be given instead of
#   --root. Every argument is passed straight through to the Python script.

# EXAMPLE USAGE:
#   bash devops/ci_cd/run_coding_standards_test.sh
#   bash devops/ci_cd/run_coding_standards_test.sh --list
#   bash devops/ci_cd/run_coding_standards_test.sh --root modules/pill_engine/src
#   bash devops/ci_cd/run_coding_standards_test.sh modules/pill_core/src/lib.rs
#
# Exit status: 0 when every checked file complies, 1 when at least one
# violation is reported, 2 on a usage error.

# --- SCRIPT ---

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# The repository root is two levels up: devops/ci_cd -> devops -> root.
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Shortcuts for colored section headers
BOLD='\033[1m'
CYAN='\033[0;36m'
RED='\033[0;31m'
NC='\033[0m'

# All paths passed to the lint are relative to the project root.
cd "$PROJECT_ROOT"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '/^# USAGE/,/^# --- SCRIPT ---/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
echo -e "${BOLD}${CYAN}Pill comment & layout lint${NC}"
echo ""

set +e
python devops/tests/test_coding_standards.py "$@"
lint_exit=$?
set -e

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
if [ "$lint_exit" -eq 0 ]; then
    echo -e "${BOLD}${CYAN}Coding standards lint PASSED${NC}"
else
    echo -e "${BOLD}${RED}Coding standards lint FAILED (exit $lint_exit)${NC}"
fi
echo -e "${BOLD}${CYAN}===============================================================================${NC}"

exit "$lint_exit"
