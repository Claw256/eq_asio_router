<#
.SYNOPSIS
  Registers the "EQ ASIO Router" virtual ASIO driver (self-elevating).
.DESCRIPTION
  Registers the 64-bit DLL by default (VBMatrix is x64). With -Both the
  32-bit DLL is registered too (for 32-bit ASIO hosts), via the 32-bit
  regsvr32 so it lands in the WOW6432Node registry views.
  DLLs are registered IN PLACE (path under target\...), so rebuilding
  requires the host (VBMatrix) to be closed first.
.EXAMPLE
  .\scripts\install-driver.ps1                       # 64-bit, release
  .\scripts\install-driver.ps1 -Configuration debug  # 64-bit, debug
  .\scripts\install-driver.ps1 -Both                 # 64-bit + 32-bit
#>
param(
    [ValidateSet('release', 'debug')][string]$Configuration = 'release',
    [switch]$Both
)

$ErrorActionPreference = 'Stop'

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Configuration', $Configuration)
    if ($Both) { $argList += '-Both' }
    Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -Wait
    exit $LASTEXITCODE
}

$root = Split-Path -Parent $PSScriptRoot
$dll32 = Join-Path $root "target\i686-pc-windows-msvc\$Configuration\eq_asio_driver.dll"
$dll64 = Join-Path $root "target\$Configuration\eq_asio_driver.dll"
$reg32 = Join-Path $env:WINDIR 'SysWOW64\regsvr32.exe'
$reg64 = Join-Path $env:WINDIR 'System32\regsvr32.exe'

try {
    if (-not (Test-Path $dll64)) {
        throw "Missing $dll64 -- build it first: cargo build -p eq-asio-driver" + $(if ($Configuration -eq 'release') { ' --release' } else { '' })
    }
    Write-Host "Registering 64-bit: $dll64"
    $p = Start-Process $reg64 -ArgumentList '/s', "`"$dll64`"" -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "regsvr32 (64-bit) failed with exit code $($p.ExitCode)" }

    if ($Both) {
        if (-not (Test-Path $dll32)) {
            throw "Missing $dll32 -- build it first: cargo build -p eq-asio-driver --target i686-pc-windows-msvc" + $(if ($Configuration -eq 'release') { ' --release' } else { '' })
        }
        Write-Host "Registering 32-bit: $dll32"
        $p = Start-Process $reg32 -ArgumentList '/s', "`"$dll32`"" -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "regsvr32 (32-bit) failed with exit code $($p.ExitCode)" }
    }

    Write-Host ''
    Write-Host 'Registry check:'
    foreach ($k in 'HKLM:\SOFTWARE\WOW6432Node\ASIO\EQ ASIO Router', 'HKLM:\SOFTWARE\ASIO\EQ ASIO Router') {
        if (Test-Path $k) { Write-Host "  OK   $k  CLSID=$((Get-ItemProperty $k).CLSID)" } else { Write-Host "  --   $k (not present)" }
    }
    Write-Host ''
    Write-Host 'Done. Start VBMatrix, open an ASIO slot and pick "EQ ASIO Router" (mark it Device Master so the matrix clocks from the driver).'
}
catch {
    Write-Host "ERROR: $_" -ForegroundColor Red
    exit 1
}
finally {
    if (-not $env:EQ_ASIO_NO_PAUSE -and $Host.Name -eq 'ConsoleHost') { Read-Host 'Press Enter to close' | Out-Null }
}
