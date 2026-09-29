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

function Test-WingetPackage {
    param([Parameter(Mandatory = $true)][string] $Id)

    if (-not $script:WingetEnabled) {
        return $false
    }
    $result = & winget list --id $Id --exact --source winget --disable-interactivity 2>$null
    return ($LASTEXITCODE -eq 0 -and ($result -match [regex]::Escape($Id)))
}

function Install-WingetPackage {
    param(
        [Parameter(Mandatory = $true)][string] $Id,
        [string] $Override
    )

    if (Test-WingetPackage -Id $Id) {
        Write-Host "$Id is already installed."
        return
    }

    if (-not $script:WingetEnabled) {
        throw "Cannot install $Id because Windows Package Manager is disabled by Group Policy. Install it manually or ask your administrator to enable App Installer/Windows Package Manager, then rerun this script."
    }

    Write-Host "Installing $Id..."
    $arguments = @(
        'install', '--id', $Id, '--exact', '--source', 'winget',
        '--accept-package-agreements', '--accept-source-agreements'
    )
    if ($Override) {
        $arguments += @('--override', $Override)
    }
    & winget @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "winget failed to install $Id with exit code $LASTEXITCODE."
    }
    Update-SessionPath
}

function Test-CodeExtension {
    param([Parameter(Mandatory = $true)][string] $Id)

    if (-not (Get-Command code -ErrorAction SilentlyContinue)) {
        return $false
    }
    $installed = @(code --list-extensions 2>$null)
    return ($installed -contains $Id)
}

function Install-CodeExtension {
    param([Parameter(Mandatory = $true)][string] $Id)

    if (Test-CodeExtension -Id $Id) {
        Write-Host "VS Code extension $Id is already installed."
        return
    }

    Write-Host "Installing VS Code extension $Id..."
    & code --install-extension $Id
    if ($LASTEXITCODE -ne 0) {
        throw "VS Code failed to install extension $Id with exit code $LASTEXITCODE."
    }
}

function Test-VisualStudioCppBuildTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) {
        return $false
    }

    $installationPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Workload.VCTools -property installationPath 2>$null
    return (-not [string]::IsNullOrWhiteSpace(($installationPath -join '')))
}

function Test-Cdb {
    $cdbCandidates = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Debuggers\x64\cdb.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Debuggers\x86\cdb.exe'),
        (Join-Path $env:ProgramFiles 'Windows Kits\10\Debuggers\x64\cdb.exe'),
        (Join-Path $env:ProgramFiles 'Windows Kits\10\Debuggers\x86\cdb.exe')
    )
    return ($cdbCandidates | Where-Object { Test-Path -LiteralPath $_ } | Measure-Object).Count -gt 0
}

function Install-VisualStudioCppBuildTools {
    if (Test-VisualStudioCppBuildTools) {
        Write-Host 'Visual Studio C++ Build Tools and a C++ workload are already installed.'
        return
    }

    $installer = Join-Path ([IO.Path]::GetTempPath()) 'vs_buildtools.exe'
    Write-Host 'Downloading the official Visual Studio Build Tools installer...'
    Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vs_buildtools.exe' -OutFile $installer
    try {
        Write-Host 'Installing Visual Studio C++ Build Tools and Windows SDK...'
        & $installer --wait --quiet `
            --add Microsoft.VisualStudio.Workload.VCTools `
            --add Microsoft.VisualStudio.Component.Windows11SDK.22621 `
            --includeRecommended
        if ($LASTEXITCODE -ne 0) {
            throw "Visual Studio Build Tools installer failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Remove-Item -LiteralPath $installer -Force -ErrorAction SilentlyContinue
    }
}

function Test-DotNet8Sdk {
    if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
        return $false
    }
    $sdks = @(dotnet --list-sdks 2>$null)
    return ($sdks -match '^8\.')
}

function Install-VsCodeAndDotNetFallbacks {
    if (-not (Get-Command code -ErrorAction SilentlyContinue)) {
        $codeInstaller = Join-Path ([IO.Path]::GetTempPath()) 'VSCodeUserSetup.exe'
        Write-Host 'Downloading the official VS Code installer because Winget installation is unavailable...'
        Invoke-WebRequest -Uri 'https://update.code.visualstudio.com/latest/win32-x64-user/stable' -OutFile $codeInstaller
        try {
            Start-Process -FilePath $codeInstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
        }
        finally {
            Remove-Item -LiteralPath $codeInstaller -Force -ErrorAction SilentlyContinue
        }
        Update-SessionPath
    }
    else {
        Write-Host 'Visual Studio Code is already installed.'
    }

    if (-not (Test-DotNet8Sdk)) {
        $dotnetInstaller = Join-Path ([IO.Path]::GetTempPath()) 'dotnet-install.ps1'
        Write-Host 'Downloading the official .NET SDK installer because Winget installation is unavailable...'
        Invoke-WebRequest -Uri 'https://dot.net/v1/dotnet-install.ps1' -OutFile $dotnetInstaller
        try {
            & $dotnetInstaller -Channel 8.0 -InstallDir (Join-Path $env:USERPROFILE '.dotnet')
            if ($LASTEXITCODE -ne 0) {
                throw "dotnet-install failed with exit code $LASTEXITCODE."
            }
        }
        finally {
            Remove-Item -LiteralPath $dotnetInstaller -Force -ErrorAction SilentlyContinue
        }
        $env:PATH = "$(Join-Path $env:USERPROFILE '.dotnet');$env:PATH"
    }
    else {
        Write-Host '.NET 8 SDK is already installed.'
    }
}

function Read-YesNo {
    param(
        [Parameter(Mandatory = $true)][string] $Prompt,
        [bool] $Default = $false
    )

    $suffix = if ($Default) { '[Y/n]' } else { '[y/N]' }
    $answer = Read-Host "$Prompt $suffix"
    if ([string]::IsNullOrWhiteSpace($answer)) {
        return $Default
    }
    return $answer.Trim().ToLowerInvariant() -in @('y', 'yes')
}

$script:WingetEnabled = $false
if (Get-Command winget -ErrorAction SilentlyContinue) {
    & winget --version 2>$null | Out-Null
    $script:WingetEnabled = ($LASTEXITCODE -eq 0)
}

if (-not $script:WingetEnabled) {
    Write-Warning "Windows Package Manager is unavailable or disabled by Group Policy. Winget-based installations will be skipped."
    Write-Warning "Rust has a direct installer fallback; Visual Studio Build Tools and optional VS Code setup require manual installation when Winget is blocked."
}

# 1. Rust toolchain (rustup installs the stable-x86_64-pc-windows-msvc toolchain by default).
$cargoPath = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (Test-Path -LiteralPath $cargoPath) {
    Write-Host "Rust toolchain already installed."
}
elseif ($script:WingetEnabled) {
    Install-WingetPackage -Id 'Rustlang.Rustup'
}
else {
    # Rustup's official installer is the fallback when an enterprise policy
    # disables Winget. It does not require administrator rights.
    $rustupInstaller = Join-Path ([IO.Path]::GetTempPath()) 'rustup-init.exe'
    Write-Host 'Downloading the official Rust installer because Winget is disabled...'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustupInstaller
    try {
        & $rustupInstaller -y --default-toolchain stable-x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) {
            throw "rustup failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Remove-Item -LiteralPath $rustupInstaller -Force -ErrorAction SilentlyContinue
    }
    Update-SessionPath
}

# 2. MSVC build tools + Windows SDK (needed to link, even though the workspace
#    uses rust-lld as the linker - rust-lld still needs the MSVC/Windows SDK
#    import libraries, only replaces link.exe).
if (Test-VisualStudioCppBuildTools) {
    Write-Host 'Visual Studio C++ Build Tools and a C++ workload are already installed.'
}
else {
    # Use the official bootstrapper directly. It works even when an enterprise
    # policy permits querying Winget but blocks its install operation.
    Install-VisualStudioCppBuildTools
}

# CDB is distributed by the separate Windows SDK installer, not necessarily by
# the Visual Studio Build Tools installer. It is needed only by the manual
# stack-sampling check, so report the prerequisite without stopping setup.
if (Test-Cdb) {
    Write-Host 'Windows debugging tools (cdb.exe) are already installed.'
}
else {
    Write-Warning 'cdb.exe is not installed. Download the Windows SDK from https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/ and select only Debugging Tools for Windows. This is required for stack_sample.ps1.'
}

# 3. Windows Smart App Control (SAC) blocks LoadLibrary of the DLLs this
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
}
else {
    Write-Host "Smart App Control is off."
}

# 4. Optional VS Code debugging setup.
$installVsCodeDebugging = Read-YesNo -Prompt 'Install the VS Code debugging setup and required tools?' -Default $true
if ($installVsCodeDebugging) {
    # VS Code's cppvsdbg provider is required by the native launch profiles;
    # coreclr is supplied by the C# extension. CodeLLDB supports the alternate
    # launch profile and the remaining extensions support editing/inspection.
    if ($script:WingetEnabled) {
        try {
            Install-WingetPackage -Id 'Microsoft.VisualStudioCode'
            Install-WingetPackage -Id 'Microsoft.DotNet.SDK.8'
        }
        catch {
            Write-Warning "Winget installation was blocked: $($_.Exception.Message)"
            Install-VsCodeAndDotNetFallbacks
        }
    }
    else {
        Install-VsCodeAndDotNetFallbacks
    }

    $debugExtensions = @(
        'rust-lang.rust-analyzer',
        'ms-vscode.cpptools',
        'ms-dotnettools.csharp',
        'ms-dotnettools.csdevkit',
        'ms-vscode.hexeditor',
        'tamasfe.even-better-toml',
        'vadimcn.vscode-lldb'
    )
    foreach ($extensionId in $debugExtensions) {
        Install-CodeExtension -Id $extensionId
    }

    if (-not (Get-Command dotnet-trace -ErrorAction SilentlyContinue)) {
        Write-Host 'Installing dotnet-trace...'
        & dotnet tool install --global dotnet-trace
        if ($LASTEXITCODE -ne 0) {
            throw "dotnet-trace installation failed with exit code $LASTEXITCODE."
        }
        Update-SessionPath
    }
    else {
        Write-Host 'dotnet-trace is already installed.'
    }
}
else {
    Write-Host 'Skipping VS Code debugging setup.'
}

# 5. Populate the offline cargo registry cache (the --offline run needs every
#    dependency already downloaded once; this also fetches the pinned
#    `trait_type_map` git dependency).
Update-SessionPath
$repoRoot = $PSScriptRoot
Push-Location (Join-Path $repoRoot "modules")
Write-Host "Fetching cargo dependencies (online, one-time)..."
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fetch
Pop-Location

Write-Host ""
Write-Host "Setup complete. Run the project with:"
Write-Host '  $env:PROJECT_PATH = "../examples/project_rs"'
Write-Host "  cd modules"
Write-Host "  cargo run --package pill_standalone --features rendering --offline"
