<#
.SYNOPSIS
  Unregisters the "EQ ASIO Router" virtual ASIO driver (self-elevating).
.EXAMPLE
  .\scripts\uninstall-driver.ps1                      # 64-bit, release path
  .\scripts\uninstall-driver.ps1 -Both                # 64-bit + 32-bit
  .\scripts\uninstall-driver.ps1 -Both -Configuration debug
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
    if (Test-Path $dll64) {
        Write-Host "Unregistering 64-bit: $dll64"
        Start-Process $reg64 -ArgumentList '/u', '/s', "`"$dll64`"" -Wait | Out-Null
    }
    else { Write-Host "64-bit DLL not found at $dll64 (skipping regsvr32); removing keys directly." }

    if ($Both -and (Test-Path $dll32)) {
        Write-Host "Unregistering 32-bit: $dll32"
        Start-Process $reg32 -ArgumentList '/u', '/s', "`"$dll32`"" -Wait | Out-Null
    }

    # Fallback / cleanup in case the DLL was moved or deleted: remove our keys in both views.
    $clsid = '{D9568DC8-2E04-429B-ABD0-03852E2931DF}'
    $keys = @("HKLM:\SOFTWARE\ASIO\EQ ASIO Router", "HKLM:\SOFTWARE\Classes\CLSID\$clsid")
    if ($Both) { $keys += @("HKLM:\SOFTWARE\WOW6432Node\ASIO\EQ ASIO Router", "HKLM:\SOFTWARE\WOW6432Node\Classes\CLSID\$clsid", "HKLM:\SOFTWARE\Classes\WOW6432Node\CLSID\$clsid") }
    foreach ($k in $keys) {
        if (Test-Path $k) { Remove-Item $k -Recurse -Force; Write-Host "  removed $k" }
    }
    Write-Host 'Done.'
}
catch {
    Write-Host "ERROR: $_" -ForegroundColor Red
    exit 1
}
finally {
    if (-not $env:EQ_ASIO_NO_PAUSE -and $Host.Name -eq 'ConsoleHost') { Read-Host 'Press Enter to close' | Out-Null }
}
