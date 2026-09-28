#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, curl, ca-certificates. The system-package step needs
#               apt-get plus root (the script uses sudo when it is not already
#               root) and only runs on Debian/Ubuntu; every other step installs
#               per-user and needs no privileges. Run from anywhere; the script
#               locates the repository root itself.

# DESCRIPTION: Prepares a fresh Linux machine to build and run this workspace
#   with:
#       export PROJECT_PATH=../examples/project_cs
#       cd modules
#       cargo run --package pill_standalone --features rendering
#
#   It is the Linux counterpart of setup_windows_environment.ps1 and performs
#   the same four jobs, mapped onto this platform:
#
#   1. System libraries. The windowed host dlopens the windowing and GPU stack
#      at runtime and links the toolchain's shared `libstd` at load time, so a
#      desktop needs the Vulkan loader plus an ICD (or the GL/EGL fallback),
#      the X11/Wayland client libraries, and the C toolchain the build scripts
#      shell out to. The managed host additionally starts the .NET runtime,
#      which needs ICU and OpenSSL. The script installs only what is missing.
#
#   2. Rust toolchain (rustup). A plain `cargo run` needs it; the Windows
#      script installs it with winget, this one with rustup's own installer
#      into ~/.cargo.
#
#   3. .NET 8 SDK. This is the Linux stand-in for the Windows script's MSVC
#      step: the managed (`PROJECT_PATH=.../*.csproj`) backend builds the
#      project with `dotnet build` and boots CoreCLR through hostfxr. The host
#      looks for `libhostfxr.so` under DOTNET_ROOT, ~/.dotnet,
#      /usr/share/dotnet and /usr/lib/dotnet, and spawns the `dotnet`
#      executable from PATH - so the SDK is installed into ~/.dotnet (no root
#      needed) and DOTNET_ROOT/PATH are exported and appended to ~/.bashrc so
#      later shells inherit them.
#
#   4. Offline cargo registry. The host builds every extension and the project
#      with `cargo build --offline`, so a reload never waits on the registry
#      index. That only works once every dependency - including the pinned
#      `trait_type_map` git dependency - is already cached, so the script runs
#      `cargo fetch` in modules/ once while it still has the network.
#
#   Like the Windows script, this one is idempotent: every step detects what is
#   already present and does nothing.

# USAGE: devops/setup/setup_linux_environment.sh [--no-fetch]
#          (no arguments)   Prepare system packages, Rust, the .NET 8 SDK and
#                           the offline cargo cache
#          --no-fetch       Skip the one-time online `cargo fetch` (useful when
#                           the cache is already warm or the network is down)

# EXAMPLE USAGE:
#   bash devops/setup/setup_linux_environment.sh
#   export PROJECT_PATH=../examples/project_cs
#   cd modules && cargo run --package pill_standalone --features rendering

# --- SCRIPT ---

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODULES_DIR="$REPO_ROOT/modules"

# Coloured status lines, matching the rest of devops/.
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
RED=$'\033[0;31m'
NC=$'\033[0m'

info() { printf '%s==>%s %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$YELLOW" "$NC" "$*" >&2; }
fail() { printf '%s[error]%s %s\n' "$RED" "$NC" "$*" >&2; exit 1; }

# Commands every build spawns. Kept separate from the library list only so the
# warning can name which half is missing; both are covered by APT_PACKAGES.
BUILD_COMMANDS=(cc pkg-config curl git)

# Shared libraries the windowed host loads at runtime.
RUNTIME_LIBRARIES=(
    libvulkan.so.1          # Vulkan loader (wgpu's primary backend)
    libGL.so.1              # GL fallback when no Vulkan ICD is present
    libEGL.so.1
    libxkbcommon.so.0       # keyboard handling (winit)
    libwayland-client.so.0  # Wayland windowing
    libX11.so.6             # X11 windowing
    libXcursor.so.1
    libicuuc.so             # ICU - the .NET runtime's globalization layer
    libssl.so.3             # OpenSSL 3 - the .NET runtime's crypto layer
)

# Debian Bookworm package names providing the commands and libraries above,
# plus the C toolchain and certificate store the build scripts assume. Names
# that a different distribution does not know (Ubuntu's libicu/libssl versions
# differ) are filtered out at install time rather than failing the whole run.
APT_PACKAGES=(
    build-essential pkg-config curl git ca-certificates
    libvulkan1 mesa-vulkan-drivers libgl1 libegl1 libgles2
    libxkbcommon0 libwayland-client0 libwayland-cursor0 libwayland-egl1
    libx11-6 libxcursor1 libxi6 libxrandr2 libxcb1
    libicu72 libssl3 zlib1g
)

# Run a command with root, reusing the caller's own privileges when they are
# already root and sudo otherwise. Returns 127 when neither is available, so
# the caller can fall back to printing the command instead of hanging on an
# interactive password prompt.
as_root() {
    if [[ "${EUID}" -eq 0 ]]; then
        "$@"
    elif command -v sudo > /dev/null 2>&1; then
        sudo "$@"
    else
        return 127
    fi
}

# Print the runtime libraries from the given list that ldconfig cannot resolve.
missing_runtime_libraries() {
    local library
    for library in "$@"; do
        ldconfig -p 2> /dev/null | grep -q "$library" || printf '%s\n' "$library"
    done
}

# --- 1. System packages ------------------------------------------------------

install_system_packages() {
    local tool
    local missing_commands=()
    for tool in "${BUILD_COMMANDS[@]}"; do
        command -v "$tool" > /dev/null 2>&1 || missing_commands+=("$tool")
    done

    local missing_libraries=()
    mapfile -t missing_libraries < <(missing_runtime_libraries "${RUNTIME_LIBRARIES[@]}")

    if [[ ${#missing_commands[@]} -eq 0 && ${#missing_libraries[@]} -eq 0 ]]; then
        info "system commands and runtime libraries already present"
        return
    fi
    if [[ ${#missing_commands[@]} -gt 0 ]]; then
        warn "missing build commands: ${missing_commands[*]}"
    fi
    if [[ ${#missing_libraries[@]} -gt 0 ]]; then
        warn "missing runtime libraries: ${missing_libraries[*]}"
    fi

    if ! command -v apt-get > /dev/null 2>&1; then
        warn "apt-get not found; install the equivalents with your package manager, then re-run:"
        printf '  packages: %s\n' "${APT_PACKAGES[*]}"
        return
    fi

    info "updating the apt package index..."
    if ! as_root apt-get update > /dev/null; then
        warn "could not run 'apt-get update' (needs root/sudo). Install manually, then re-run:"
        printf '  sudo apt-get install -y %s\n' "${APT_PACKAGES[*]}"
        return
    fi

    # Keep only the names this distribution knows, so a differing libicu/libssl
    # version name does not take the whole install down with it.
    local package
    local installable=()
    for package in "${APT_PACKAGES[@]}"; do
        if apt-cache show "$package" > /dev/null 2>&1; then
            installable+=("$package")
        else
            warn "package '$package' is unknown on this distribution; install its equivalent if the runtime needs it"
        fi
    done
    if [[ ${#installable[@]} -eq 0 ]]; then
        return
    fi

    info "installing system packages: ${installable[*]}"
    if ! as_root apt-get install -y "${installable[@]}"; then
        warn "package installation failed; install manually, then re-run:"
        printf '  sudo apt-get install -y %s\n' "${installable[*]}"
        return
    fi
    hash -r
    info "system packages installed"
}

# --- 2. Rust toolchain -------------------------------------------------------

install_rust() {
    if command -v cargo > /dev/null 2>&1; then
        info "Rust toolchain already installed ($(cargo --version))"
        return
    fi
    command -v curl > /dev/null 2>&1 || fail "curl is required to install Rust"
    info "installing Rust (rustup)..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable
    # rustup installs into ~/.cargo and drops its own env script there; the
    # current shell has to source it before `cargo` is on PATH.
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    hash -r
    info "Rust installed ($(cargo --version))"
}

# --- 3. .NET 8 SDK -----------------------------------------------------------

# Honour an existing DOTNET_ROOT, otherwise use the location the host already
# searches. Both the install and the PATH entry are derived from this value.
DOTNET_ROOT="${DOTNET_ROOT:-$HOME/.dotnet}"
export DOTNET_ROOT
export PATH="$DOTNET_ROOT:$PATH"

install_dotnet() {
    if command -v dotnet > /dev/null 2>&1 && dotnet --list-sdks 2> /dev/null | grep -q '^8\.'; then
        info ".NET 8 SDK already installed ($(dotnet --version))"
        return
    fi
    command -v curl > /dev/null 2>&1 || fail "curl is required to install the .NET SDK"
    info "installing the .NET 8 SDK into $DOTNET_ROOT ..."
    local installer
    installer="$(mktemp)"
    curl -sSL https://dot.net/v1/dotnet-install.sh -o "$installer"
    bash "$installer" --channel 8.0 --install-dir "$DOTNET_ROOT"
    rm -f "$installer"
    hash -r
    if ! dotnet --list-sdks 2> /dev/null | grep -q '^8\.'; then
        fail "the .NET SDK install did not produce a working 'dotnet' on PATH"
    fi
    info ".NET 8 SDK installed ($(dotnet --version))"
}

# Make the SDK visible to future shells, the way the Windows script refreshes
# the session PATH. Appended once, guarded by a marker comment.
persist_dotnet_environment() {
    local rc_file="$HOME/.bashrc"
    local marker="# Added by devops/setup/setup_linux_environment.sh"
    if [[ -f "$rc_file" ]] && grep -qF "$marker" "$rc_file"; then
        return
    fi
    {
        printf '\n%s\n' "$marker"
        printf 'export DOTNET_ROOT="%s"\n' "$DOTNET_ROOT"
        printf 'export PATH="$DOTNET_ROOT:$PATH"\n'
    } >> "$rc_file"
    info "added DOTNET_ROOT/PATH to $rc_file"
}

# --- 4. Offline cargo registry ----------------------------------------------

fetch_cargo_dependencies() {
    info "fetching cargo dependencies (online, one-time)..."
    (cd "$MODULES_DIR" && cargo fetch)
    info "cargo dependency cache populated"
}

# --- main -------------------------------------------------------------------

FETCH=1

usage() {
    sed -n 's/^# USAGE:/  /p; s/^# EXAMPLE USAGE:/  /p' "$0"
    printf '\n'
}

main() {
    local argument
    for argument in "$@"; do
        case "$argument" in
            --no-fetch) FETCH=0 ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) fail "unknown argument: $argument (try --help)" ;;
        esac
    done

    info "repository root: $REPO_ROOT"
    install_system_packages
    install_rust
    install_dotnet
    persist_dotnet_environment
    if [[ "$FETCH" == "1" ]]; then
        fetch_cargo_dependencies
    fi

    printf '\n%sSetup complete.%s Run the project with:\n' "$GREEN" "$NC"
    printf '  export DOTNET_ROOT="%s"\n' "$DOTNET_ROOT"
    printf '  export PATH="$DOTNET_ROOT:$PATH"\n'
    printf '  export PROJECT_PATH=../examples/project_cs\n'
    printf '  cd modules\n'
    printf '  cargo run --package pill_standalone --features rendering\n'
    printf '\n'
    printf 'Windows used to need Smart App Control off before the host could load\n'
    printf 'the DLLs it compiles at runtime. Linux has no equivalent, but the run\n'
    printf 'must have a display: the host opens a window, so run it from a desktop\n'
    printf 'session (X11 or Wayland), not a bare TTY.\n'
}

main "$@"
