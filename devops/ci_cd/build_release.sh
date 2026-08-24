#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, Rust toolchain (cargo) on PATH. Run from anywhere; the
#               script locates the repository root itself.

# DESCRIPTION: Builds the workspace's release profile, which a plain
#   `cargo build --release` cannot do.
#
#   The obstacle is `-C prefer-dynamic` in `modules/.cargo/config.toml`. It is
#   there so the host executable and every optional module share one copy of
#   `pill_engine`, which is what keeps its statics, thread-locals and tracing
#   dispatcher single-instance across the DLL boundary. A release binary links
#   everything into one image and needs none of that - and rustc refuses
#   `-C prefer-dynamic` together with the release profile's `lto = "fat"` when
#   targeting Windows, so the flag has to go for a release build to happen at
#   all.
#
#   Cargo offers no way to make `build.rustflags` conditional on the profile.
#   `--config build.rustflags=[]` does not work either: cargo MERGES config
#   arrays under the same key, so an empty list joins with the existing one and
#   changes nothing. Clearing `RUSTFLAGS` in the environment is the only
#   mechanism that does, which is why this is a script rather than a cargo
#   alias - an alias cannot set environment variables.
#
#   Everything else about the build is ordinary cargo. Extra arguments are
#   forwarded, so this is a drop-in replacement for `cargo build --release`.

# USAGE: devops/ci_cd/build_release.sh [cargo arguments...]
#          --profile <name>   Build a different release profile
#                             (release-fast, release-with-debug)
#          Any other argument is passed straight to cargo.

# EXAMPLE USAGE:
#   devops/ci_cd/build_release.sh                          # whole workspace
#   devops/ci_cd/build_release.sh -p pill_standalone       # one package
#   devops/ci_cd/build_release.sh --profile release-fast   # throughput profile

# --- SCRIPT ---

set -euo pipefail

# The repository root is two levels above this script, so the build works from
# any working directory.
script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd "${script_directory}/../.." && pwd)"
workspace_directory="${repository_root}/modules"

if [[ ! -f "${workspace_directory}/Cargo.toml" ]]; then
    echo "error: no workspace at ${workspace_directory}" >&2
    exit 1
fi

# `--profile` may already be among the forwarded arguments; adding `--release`
# as well would make cargo reject the pair.
profile_already_chosen=0
for argument in "$@"; do
    if [[ "${argument}" == "--profile" || "${argument}" == --profile=* ]]; then
        profile_already_chosen=1
        break
    fi
done

profile_arguments=()
if [[ ${profile_already_chosen} -eq 0 ]]; then
    profile_arguments=(--release)
fi

echo "Building the release profile with RUSTFLAGS cleared."
echo "  workspace: ${workspace_directory}"

# An empty RUSTFLAGS replaces `build.rustflags` outright rather than merging
# with it, which is exactly what removing `-C prefer-dynamic` requires.
cd "${workspace_directory}"
RUSTFLAGS="" cargo build "${profile_arguments[@]}" "$@"
