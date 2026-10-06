<#
.SYNOPSIS
  Gives shark-notch.exe a package identity so Windows will share your notifications with it.

.DESCRIPTION
  Windows only lets a process that has "package identity" read other apps' notifications
  (UserNotificationListener). This registers a *sparse package*: a tiny manifest-only package that
  points at the exe where it already is. The exe is not repackaged, copied or modified.

  Two ways, pick one:

  (default)         Build a signed .msix from AppxManifest.xml with a throw-away self-signed
                    certificate and install it. Needs: an elevated Windows PowerShell 5.1 and the
                    Windows SDK (makeappx.exe, signtool.exe). The certificate's PRIVATE key is
                    non-exportable and is deleted right after signing; only the public certificate
                    stays, in "Trusted People", until you run unregister-identity.ps1.

  -DeveloperMode    Register the unsigned manifest directly. Needs Windows' Developer Mode switched
                    on (Settings > System > For developers); no SDK, no certificate.

  Afterwards quit Shark Notch from its tray icon and start it again; it then asks Windows for
  notification access. Read docs/NOTIFICATIONS.md first - it explains what this does to your
  machine and what has and has not been tested.

.PARAMETER ExePath
  Path to shark-notch.exe. Default: next to this script, else ..\target\release\shark-notch.exe.

.PARAMETER DeveloperMode
  Register the loose manifest (needs Developer Mode) instead of a signed package.

.EXAMPLE
  .\register-identity.ps1 -ExePath C:\Tools\SharkNotch\shark-notch.exe
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [switch]$DeveloperMode
)

$ErrorActionPreference = 'Stop'
$PackageName = 'SharkNotch'
$Subject = 'CN=SharkNotch'   # must equal Publisher in AppxManifest.xml and in the exe's manifest

function Find-SdkTool([string]$Name) {
    $roots = @("${env:ProgramFiles(x86)}\Windows Kits\10\bin", "${env:ProgramFiles}\Windows Kits\10\bin")
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        $hit = Get-ChildItem -LiteralPath $root -Recurse -Filter $Name -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } |
            Sort-Object FullName -Descending | Select-Object -First 1
        if ($hit) { return $hit.FullName }
    }
    return $null
}

# --- the executable -----------------------------------------------------------------------------
if (-not $ExePath) {
    $candidates = @(
        (Join-Path $PSScriptRoot 'shark-notch.exe'),
        (Join-Path $PSScriptRoot '..\target\release\shark-notch.exe')
    )
    $ExePath = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $ExePath) { throw "Cannot find shark-notch.exe. Pass its location with -ExePath." }
}
$exe = (Resolve-Path -LiteralPath $ExePath).Path
if ((Split-Path -Leaf $exe) -ne 'shark-notch.exe') {
    throw "The executable must be called shark-notch.exe: the package manifest refers to it by that name."
}
$exeDir = Split-Path -Parent $exe

$assets = Join-Path $PSScriptRoot 'Assets'
$manifest = Join-Path $PSScriptRoot 'AppxManifest.xml'
if (-not (Test-Path -LiteralPath $manifest) -or -not (Test-Path -LiteralPath $assets)) {
    throw "AppxManifest.xml and Assets\ must sit next to this script."
}

# An earlier registration is replaced, so the script can be run again after moving the exe.
Get-AppxPackage -Name $PackageName -ErrorAction SilentlyContinue | Remove-AppxPackage

if ($DeveloperMode) {
    # A loose registration keeps pointing at these files, so they live in a permanent folder.
    $identityDir = Join-Path $env:LOCALAPPDATA 'SharkNotch\identity'
    New-Item -ItemType Directory -Path $identityDir -Force | Out-Null
    Copy-Item -LiteralPath $manifest -Destination $identityDir -Force
    Copy-Item -LiteralPath $assets -Destination $identityDir -Recurse -Force
    Add-AppxPackage -Register (Join-Path $identityDir 'AppxManifest.xml') -ExternalLocation $exeDir
    Write-Host "Registered (developer mode). Restart Shark Notch from its tray icon."
    return
}

# --- signed package -----------------------------------------------------------------------------
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this from an elevated Windows PowerShell (the certificate has to be trusted machine-wide), or use -DeveloperMode."
}
$makeappx = Find-SdkTool 'makeappx.exe'
$signtool = Find-SdkTool 'signtool.exe'
if (-not $makeappx -or -not $signtool) {
    throw "makeappx.exe / signtool.exe not found. Install the 'Windows SDK' (Signing Tools and MSIX Packaging Tools), or use -DeveloperMode."
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("SharkNotch-identity-" + [guid]::NewGuid().ToString('N'))
$content = Join-Path $work 'content'
New-Item -ItemType Directory -Path $content -Force | Out-Null
$cert = $null
try {
    Copy-Item -LiteralPath $manifest -Destination $content
    Copy-Item -LiteralPath $assets -Destination (Join-Path $content 'Assets') -Recurse

    # A code-signing certificate whose key can never be exported (and is deleted below).
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Subject -KeyUsage DigitalSignature `
        -FriendlyName 'Shark Notch package signing' -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(5) `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')

    $msix = Join-Path $work 'SharkNotch.msix'
    # /nv: skip content validation, the exe is not inside a sparse package.
    & $makeappx pack /d $content /p $msix /nv /o
    if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)." }
    & $signtool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix
    if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)." }

    # Trust the public certificate (this is what lets Windows install the package).
    $cer = Join-Path $work 'SharkNotch.cer'
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null

    Add-AppxPackage -Path $msix -ExternalLocation $exeDir
    Write-Host "Registered. Restart Shark Notch from its tray icon; Windows may ask to allow notification access."
}
finally {
    # Nothing may be able to sign with this key again.
    if ($cert) { Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($cert.Thumbprint)" -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
