# Prepares a fresh Windows machine to build and run this workspace with:
#   cargo run --package pill_standalone --features rendering --offline
#
# Run this script from an ordinary (non-admin) PowerShell. It only requests
# elevation implicitly through winget's own installers, which prompt for UAC
# themselves when required.

$ErrorActionPreference = "Stop"

function Update-SessionPath {
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:PATH = "$machinePath;$userPath"
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    Write-Error "winget is not available. Install App Installer from the Microsoft Store, then run this script again."
    exit 1
}

# 1. Rust toolchain (rustup installs the stable-x86_64-pc-windows-msvc toolchain by default).
if (-not (Test-Path "$env:USERPROFILE\.cargo\bin\cargo.exe")) {
    Write-Host "Installing Rust (rustup)..."
    winget install --id Rustlang.Rustup -e --source winget --accept-package-agreements --accept-source-agreements
    Update-SessionPath
} else {
    Write-Host "Rust toolchain already installed."
}

# 2. MSVC build tools + Windows SDK (needed to link, even though the workspace
#    uses rust-lld as the linker - rust-lld still needs the MSVC/Windows SDK
#    import libraries, only replaces link.exe).
Write-Host "Installing Visual Studio Build Tools (C++ workload + Windows SDK)..."
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --source winget `
    --accept-package-agreements --accept-source-agreements `
    --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended"

# 3. Sibling dependency: pill_engine depends on a path dependency,
#    ../../../Trait-Type-Map relative to modules/, i.e. a sibling checkout next
#    to this repository's parent directory.
$repoRoot = $PSScriptRoot
$siblingDir = Split-Path $repoRoot -Parent
$traitTypeMapDir = Join-Path $siblingDir "Trait-Type-Map"
if (-not (Test-Path $traitTypeMapDir)) {
    Write-Host "Cloning sibling dependency Trait-Type-Map..."
    Push-Location $siblingDir
    git clone https://github.com/MattSzymonski/Trait-Type-Map.git
    Pop-Location
} else {
    Write-Host "Trait-Type-Map already present at $traitTypeMapDir."
}

# 4. Windows Smart App Control (SAC) blocks LoadLibrary of the DLLs this
#    workspace compiles on the fly for hot-reloaded modules/projects
#    ("An Application Control policy has blocked this file", os error 4551).
#    SAC can only be turned off through the Windows Security UI while it is
#    still in evaluation mode; it cannot be scripted (the registry key is
#    protected even from an elevated process), and once fully enforced it can
#    only be turned back off by reinstalling Windows.
$sacState = (Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy" -Name "VerifiedAndReputablePolicyState" -ErrorAction SilentlyContinue).VerifiedAndReputablePolicyState
if ($sacState -ne 0) {
    Write-Warning "Windows Smart App Control is still ON. Go to Settings > Privacy & security > Windows Security > App & browser control > Smart App Control settings and turn it Off, then re-run this script."
    Write-Warning "(If the Off option is greyed out, SAC is fully enforced and can only be disabled by reinstalling Windows.)"
} else {
    Write-Host "Smart App Control is off."
}

# 5. Populate the offline cargo registry cache (the --offline run needs every
#    dependency already downloaded once).
Update-SessionPath
Push-Location (Join-Path $repoRoot "modules")
Write-Host "Fetching cargo dependencies (online, one-time)..."
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fetch
Pop-Location

Write-Host ""
Write-Host "Setup complete. Run the project with:"
Write-Host '  $env:PROJECT_PATH = "../examples/project_rs"'
Write-Host "  cd modules"
Write-Host "  cargo run --package pill_standalone --features rendering --offline"
