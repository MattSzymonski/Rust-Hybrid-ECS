#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, Python 3.8+, Rust toolchain (cargo) on PATH. A C#
#               example additionally needs the .NET SDK; without it those
#               examples are skipped rather than failed.

# DESCRIPTION: Thin runner for the example build check. All of the build and
#   size-reporting logic lives in devops/tests/test_examples.py; this script
#   only locates the repository root and forwards its arguments, so the check
#   behaves identically whether it is invoked here or directly:
#
#     python devops/tests/test_examples.py examples/project_rs
#
#   Every immediate subdirectory of examples/ carrying a build manifest is
#   built in release mode and its artifact sizes reported. Examples are
#   discovered by convention (Cargo.toml or *.csproj), so adding one needs no
#   change to this script or to the Python one.
#
#   Designed for both local development and GitHub Actions CI.

# USAGE: bash devops/ci_cd/run_examples_tests.sh [all|<example-path>] [--list]
#
#   all                        build every discovered example (default)
#   examples/project_rs        build a single example
#   --list                     print what would be built, then exit
#
#   Every argument is passed straight through to the Python script.

# EXAMPLE USAGE:
#   bash devops/ci_cd/run_examples_tests.sh all
#   bash devops/ci_cd/run_examples_tests.sh examples/project_rs
#   bash devops/ci_cd/run_examples_tests.sh --list
#
# Exit status: 0 when every example built, 1 when any failed, 2 on a usage
# error.

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

# All paths passed to the check are relative to the project root.
cd "$PROJECT_ROOT"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '/^# USAGE/,/^# --- SCRIPT ---/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
echo -e "${BOLD}${CYAN}Example project builds (release)${NC}"
echo ""

set +e
python devops/tests/test_examples.py "$@"
examples_exit=$?
set -e

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
if [ "$examples_exit" -eq 0 ]; then
    echo -e "${BOLD}${CYAN}Example builds PASSED${NC}"
else
    echo -e "${BOLD}${RED}Example builds FAILED (exit $examples_exit)${NC}"
fi
echo -e "${BOLD}${CYAN}===============================================================================${NC}"

exit "$examples_exit"
