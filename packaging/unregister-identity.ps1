<#
.SYNOPSIS
  Undoes register-identity.ps1: removes the sparse package and the certificate it trusted.

.DESCRIPTION
  Removes the "SharkNotch" package registration, the loose-manifest folder used by -DeveloperMode,
  and any "CN=SharkNotch" certificate from the machine's Trusted People store (needs an elevated
  PowerShell) and from your personal store. shark-notch.exe itself is not touched.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

Get-AppxPackage -Name 'SharkNotch' -ErrorAction SilentlyContinue | Remove-AppxPackage
Remove-Item -LiteralPath (Join-Path $env:LOCALAPPDATA 'SharkNotch\identity') -Recurse -Force -ErrorAction SilentlyContinue

foreach ($store in 'Cert:\LocalMachine\TrustedPeople', 'Cert:\CurrentUser\My', 'Cert:\CurrentUser\TrustedPeople') {
    Get-ChildItem -LiteralPath $store -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq 'CN=SharkNotch' } |
        ForEach-Object {
            try { Remove-Item -LiteralPath $_.PSPath -Force }
            catch { Write-Warning "Could not remove the certificate from ${store}: $($_.Exception.Message) (run elevated?)" }
        }
}
Write-Host "Removed. Windows notifications no longer reach Shark Notch (the iPhone's still do)."
