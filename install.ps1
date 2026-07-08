#!/usr/bin/env pwsh
# APVM Installer for Windows
# https://github.com/wp-media/automation-plugin-version-manager
#
# Downloads the latest pre-built APVM CLI binary for Windows
# and installs it to ~/.apvm/bin/ (or $env:APVM_INSTALL/bin/).
#
# Usage (PowerShell 5.1+ on Windows 10+):
#   irm https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.ps1 | iex
#
# Environment variables:
#   APVM_INSTALL — Override the install directory (default: $HOME\.apvm)
#
# Requirements:
#   - PowerShell 5.1+ (ships with Windows 10+)
#   - Internet connection
#
# Sources:
#   - GitHub release download pattern:
#     https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases
#   - Invoke-RestMethod:
#     https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/invoke-restmethod
#   - Get-FileHash:
#     https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/get-filehash
#   - Registry-based PATH modification (same pattern as Bun/Deno installers):
#     https://learn.microsoft.com/en-us/powershell/scripting/samples/working-with-registry-entries
#   - SendMessageTimeout for WM_SETTINGCHANGE broadcast:
#     https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendmessagetimeoutw

$ErrorActionPreference = "Stop"

# ── Configuration ──────────────────────────────────────────────────────────

$Repo = "wp-media/automation-plugin-version-manager"
$BinaryName = "apvm.exe"
$InstallDir = if ($env:APVM_INSTALL) { $env:APVM_INSTALL } else { "$Home\.apvm" }
$BinDir = "$InstallDir\bin"
$BaseUrl = "https://github.com/$Repo/releases/latest/download"

# ── Colors ─────────────────────────────────────────────────────────────────

# Check if the terminal supports ANSI escape codes (Windows 10 1511+ / PowerShell 7+).
# SupportsVirtualTerminal is a virtual property on PSHostUserInterface; custom host
# implementations (ISE, CI runners, third-party tools) may throw NotImplementedException.
# Wrap in try/catch so a misbehaving host does not abort the script before it starts.
# Source: https://learn.microsoft.com/en-us/dotnet/api/system.management.automation.host.pshostuserinterface.supportsvirtualterminal
# https://learn.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences
$VtSupport = try { $Host.UI.SupportsVirtualTerminal } catch { $false }
$SupportsAnsi = $VtSupport -or [bool]$env:WT_SESSION -or [bool]$env:TERM_PROGRAM

$ESC = [char]27
if ($SupportsAnsi) {
    $C_RED    = "$ESC[0;31m"
    $C_GREEN  = "$ESC[0;32m"
    $C_YELLOW = "$ESC[0;33m"
    $C_BLUE   = "$ESC[0;34m"
    $C_BOLD   = "$ESC[1m"
    $C_DIM    = "$ESC[2m"
    $C_RESET  = "$ESC[0m"
} else {
    $C_RED = ""; $C_GREEN = ""; $C_YELLOW = ""; $C_BLUE = ""
    $C_BOLD = ""; $C_DIM = ""; $C_RESET = ""
}

# ── Logging ────────────────────────────────────────────────────────────────

function Write-Info    { param([string]$Msg) Write-Output "${C_BLUE}info${C_RESET}  $Msg" }
function Write-Success { param([string]$Msg) Write-Output "${C_GREEN}  +${C_RESET}  $Msg" }
function Write-Warn    { param([string]$Msg) Write-Output "${C_YELLOW}warn${C_RESET}  $Msg" }
function Write-Err     { param([string]$Msg) Write-Output "${C_RED}error${C_RESET} $Msg" }

# ── Registry-based environment helpers ─────────────────────────────────────
# Modifying the user PATH via the registry ensures persistence across reboots.
# Broadcasting WM_SETTINGCHANGE tells Explorer and other apps to reload env vars.
# Pattern sourced from Bun (https://bun.sh/install.ps1) and pixi installers.

function Publish-Env {
    if (-not ("Win32.NativeMethods" -as [Type])) {
        Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
    }
    # HWND_BROADCAST = 0xffff, WM_SETTINGCHANGE = 0x1a
    # https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendmessagetimeoutw
    $HWND_BROADCAST = [IntPtr]0xffff
    $WM_SETTINGCHANGE = 0x1a
    $result = [UIntPtr]::Zero
    [Win32.NativeMethods]::SendMessageTimeout(
        $HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero,
        "Environment", 2, 5000, [ref]$result
    ) | Out-Null
}

function Write-Env {
    param([string]$Key, [string]$Value)
    # Write to HKCU:\Environment (user-level, no admin required).
    # https://learn.microsoft.com/en-us/powershell/scripting/samples/working-with-registry-entries
    $RegKey = Get-Item -Path 'HKCU:'
    $EnvKey = $RegKey.OpenSubKey('Environment', $true)
    # OpenSubKey returns $null if the key doesn't exist; create it in that case.
    # https://learn.microsoft.com/en-us/dotnet/api/microsoft.win32.registrykey.opensubkey
    if ($null -eq $EnvKey) {
        $EnvKey = $RegKey.CreateSubKey('Environment')
    }
    if ($null -eq $Value) {
        $EnvKey.DeleteValue($Key)
    } else {
        $Kind = if ($Value.Contains('%')) {
            [Microsoft.Win32.RegistryValueKind]::ExpandString
        } elseif ($EnvKey.GetValue($Key)) {
            $EnvKey.GetValueKind($Key)
        } else {
            [Microsoft.Win32.RegistryValueKind]::String
        }
        $EnvKey.SetValue($Key, $Value, $Kind)
    }
    Publish-Env
}

function Get-Env {
    param([string]$Key)
    $RegKey = Get-Item -Path 'HKCU:'
    $EnvKey = $RegKey.OpenSubKey('Environment')
    # OpenSubKey returns $null if the key doesn't exist (see MSDN RegistryKey.OpenSubKey).
    if ($null -eq $EnvKey) { return $null }
    $EnvKey.GetValue($Key, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
}

# ── Architecture Detection ─────────────────────────────────────────────────

function Get-Architecture {
    # Read from the registry — works reliably even under x64 emulation on ARM64.
    # Same approach used by Bun's installer.
    # https://learn.microsoft.com/en-us/windows/win32/winprog64/wow64-implementation-details
    try {
        $Arch = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
        if ($Arch) { return $Arch }
    } catch {
        # Registry read failed (restricted environment). Fall back to the process-level
        # environment variable. Note: $env:PROCESSOR_ARCHITECTURE may report x86 in
        # 32-bit processes on 64-bit Windows (WoW64), but since we only support AMD64
        # and ARM64, this fallback is sufficient for our purposes.
    }
    return $env:PROCESSOR_ARCHITECTURE
}

# ── Main Install Function ─────────────────────────────────────────────────

function Install-Apvm {
    Write-Output ""
    Write-Output "${C_BOLD}  +----------------------------------+${C_RESET}"
    Write-Output "${C_BOLD}  |   APVM Installer (Windows)       |${C_RESET}"
    Write-Output "${C_BOLD}  +----------------------------------+${C_RESET}"
    Write-Output "${C_DIM}  https://github.com/$Repo${C_RESET}"
    Write-Output ""

    # ── Step 1: Detect architecture ────────────────────────────────────────

    $Arch = Get-Architecture
    if ($Arch -ne "AMD64") {
        Write-Err "No pre-built binary available for architecture: $Arch"
        Write-Err "APVM provides a pre-built Windows binary for x64 (AMD64) only."
        if ($Arch -eq "ARM64") {
            Write-Err "ARM64 Windows support may be added in a future release."
        }
        Write-Err ""
        Write-Err "If you have the Rust toolchain installed (>= 1.96.1), build from source:"
        Write-Err "  cargo install --path crates/cli"
        Write-Err "  https://github.com/$Repo#install-from-source"
        exit 1
    }

    $Artifact = "apvm-win32-x64-msvc.exe"
    Write-Info "Detected platform: ${C_BOLD}Windows x64${C_RESET}"
    Write-Info "Artifact: $Artifact"

    # ── Step 2: Create temp directory ──────────────────────────────────────

    $TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "apvm-install-$([System.Guid]::NewGuid().ToString('N').Substring(0,8))"
    New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

    try {
        # ── Step 3: Download binary ────────────────────────────────────────

        $BinaryUrl = "$BaseUrl/$Artifact"
        $BinaryPath = Join-Path $TmpDir $Artifact

        Write-Info "Downloading from latest release..."

        try {
            # Prefer curl.exe if available — faster than PowerShell's Invoke-RestMethod.
            # Note: 'curl' in PowerShell is an alias for Invoke-WebRequest;
            # 'curl.exe' invokes the real curl shipped with Windows 10 1803+.
            # https://curl.se/windows/
            $curlExe = Get-Command curl.exe -ErrorAction SilentlyContinue
            if ($curlExe) {
                & curl.exe --proto '=https' --tlsv1.2 -fsSL $BinaryUrl -o $BinaryPath
                if ($LASTEXITCODE -ne 0) { throw "curl.exe failed with exit code $LASTEXITCODE" }
            } else {
                # Fallback: Invoke-RestMethod (works on all PowerShell 5.1+ systems)
                # https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/invoke-restmethod
                Invoke-RestMethod -Uri $BinaryUrl -OutFile $BinaryPath
            }
        } catch {
            Write-Err "Failed to download: $BinaryUrl"
            Write-Err ""
            Write-Err "Possible causes:"
            Write-Err "  - No internet connection"
            Write-Err "  - No release has been published yet"
            Write-Err "  - GitHub is experiencing an outage"
            Write-Err ""
            Write-Err "Check: https://github.com/$Repo/releases/latest"
            exit 1
        }

        if (-not (Test-Path $BinaryPath)) {
            Write-Err "Download succeeded but file not found at: $BinaryPath"
            Write-Err "An antivirus may have quarantined the file."
            exit 1
        }

        Write-Success "Downloaded binary"

        # ── Step 4: Verify checksum ────────────────────────────────────────

        $ChecksumsUrl = "$BaseUrl/checksums.txt"
        $ChecksumsPath = Join-Path $TmpDir "checksums.txt"

        Write-Info "Verifying checksum (SHA-256)..."

        $ChecksumOk = $false
        try {
            if ($curlExe) {
                & curl.exe --proto '=https' --tlsv1.2 -fsSL $ChecksumsUrl -o $ChecksumsPath 2>$null
                if ($LASTEXITCODE -ne 0) { throw "curl failed" }
            } else {
                Invoke-RestMethod -Uri $ChecksumsUrl -OutFile $ChecksumsPath
            }

            if (Test-Path $ChecksumsPath) {
                # Parse checksums.txt: each line is "<hash>  <filename>"
                $ExpectedLine = Get-Content $ChecksumsPath | Where-Object { $_ -match ("\s+" + [regex]::Escape($Artifact) + '$') }
                if ($ExpectedLine) {
                    $ExpectedHash = ($ExpectedLine -split '\s+')[0]

                    # Get-FileHash: built-in since PowerShell 4.0
                    # https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/get-filehash
                    $ActualHash = (Get-FileHash -Path $BinaryPath -Algorithm SHA256).Hash.ToLower()

                    if ($ActualHash -ne $ExpectedHash) {
                        Write-Err "Checksum verification failed!"
                        Write-Err "  Expected: $ExpectedHash"
                        Write-Err "  Actual:   $ActualHash"
                        Write-Err ""
                        Write-Err "The downloaded file may be corrupted or tampered with."
                        exit 1
                    }
                    $ChecksumOk = $true
                } else {
                    Write-Warn "Could not find checksum for '$Artifact' in checksums.txt"
                }
            }
        } catch {
            Write-Warn "Could not download checksums.txt - skipping verification"
        }

        if ($ChecksumOk) {
            Write-Success "Checksum verified"
        }

        # ── Step 5: Install binary ─────────────────────────────────────────

        # Create install directory
        New-Item -ItemType Directory -Path $BinDir -Force | Out-Null

        # Remove existing binary if present (allows updates/reinstalls)
        $TargetPath = Join-Path $BinDir $BinaryName
        if (Test-Path $TargetPath) {
            try {
                Remove-Item $TargetPath -Force
            } catch {
                # Accessing Process.Path (= MainModule.FileName) can throw Win32Exception
                # "Access is denied" for processes owned by other users or protected by AV/EDR.
                # Source: https://learn.microsoft.com/en-us/dotnet/api/system.diagnostics.process.mainmodule
                $RunningProcs = Get-Process -Name "apvm" -ErrorAction SilentlyContinue |
                    Where-Object { try { $_.Path -eq $TargetPath } catch { $false } }
                if ($RunningProcs.Count -gt 0) {
                    Write-Err "Cannot replace existing binary — apvm.exe is currently running."
                    Write-Err "Please close any running apvm processes and try again."
                    exit 1
                }
                Write-Err "Failed to remove existing binary at: $TargetPath"
                Write-Err $_
                exit 1
            }
        }

        Copy-Item $BinaryPath $TargetPath -Force
        Write-Success "Installed to $TargetPath"

        # ── Step 6: Add to PATH ────────────────────────────────────────────

        $PathUpdated = $false
        $UserPath = Get-Env -Key "Path"
        $PathEntries = if ($UserPath) { $UserPath -split ';' } else { @() }

        if ($PathEntries -notcontains $BinDir) {
            $PathEntries += $BinDir
            Write-Env -Key 'Path' -Value ($PathEntries -join ';')
            $env:PATH = "$BinDir;$env:PATH"
            $PathUpdated = $true
            Write-Success "Added $BinDir to user PATH"
        }

        # ── Step 7: Verify and print summary ───────────────────────────────

        Write-Output ""

        $VersionOutput = $null
        try {
            $VersionOutput = & $TargetPath --version 2>&1
        } catch {
            # Binary may fail for various reasons (missing DLLs, etc.)
        }

        if (-not $VersionOutput) {
            Write-Err "Installation completed but the binary could not be executed."
            Write-Err "This may indicate a platform mismatch or a missing dependency."
            Write-Err "Please report an issue: https://github.com/$Repo/issues"
            exit 1
        }

        Write-Output "${C_GREEN}${C_BOLD}  +----------------------------------+${C_RESET}"
        Write-Output "${C_GREEN}${C_BOLD}  | APVM installed successfully!     |${C_RESET}"
        Write-Output "${C_GREEN}${C_BOLD}  +----------------------------------+${C_RESET}"
        Write-Output ""
        Write-Output "  ${C_DIM}Version :${C_RESET}  $VersionOutput"
        Write-Output "  ${C_DIM}Binary  :${C_RESET}  $TargetPath"
        Write-Output ""

        if ($PathUpdated) {
            Write-Output "  ${C_YELLOW}Restart your terminal${C_RESET} to update your PATH, then run:"
        } else {
            Write-Output "  Run:"
        }

        Write-Output ""
        Write-Output "    ${C_BOLD}apvm --help${C_RESET}"
        Write-Output ""

    } finally {
        # Clean up temp directory
        if (Test-Path $TmpDir) {
            Remove-Item $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

Install-Apvm
