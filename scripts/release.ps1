<#
  Baut eine signierte Version der Desktop-App und bereitet das Update vor.

  Ablauf für ein Update:
    1. Version erhöhen: src-tauri/tauri.conf.json, src-tauri/Cargo.toml, package.json
       (alle drei gleich – der Updater vergleicht mit der installierten Version).
    2. .\scripts\release.ps1 -Notes "Was ist neu?"
       -> baut, signiert und schreibt release\latest.json
    3. Dasselbe mit -Publish: legt zusätzlich ein GitHub-Release v<Version> an und lädt
       Installer, Signatur und latest.json hoch. Ab dann bieten sich laufende
       Apps das Update an (Prüfung beim Start und alle 6 Stunden).

  Der private Signaturschlüssel liegt außerhalb des Projekts (%USERPROFILE%\.tauri\erlc-cad.key).
  Er gehört nie ins Repo. Geht er verloren, können bestehende Installationen kein Update
  mehr annehmen – dann hilft nur eine Neuinstallation. Bitte sichere ihn.
#>
param(
  [string]$Notes = "",
  [switch]$Publish,
  [string]$Repo = "syncyxyc/erlc-cad-desktop",
  [string]$KeyPath = "$env:USERPROFILE\.tauri\erlc-cad.key"
)

$ErrorActionPreference = "Stop"
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User") + ";$env:USERPROFILE\.cargo\bin"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $KeyPath)) { throw "Signaturschluessel fehlt: $KeyPath" }

$conf = Get-Content "src-tauri\tauri.conf.json" -Raw | ConvertFrom-Json
$version = $conf.version
$cargo = (Select-String -Path "src-tauri\Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$package = (Get-Content "package.json" -Raw | ConvertFrom-Json).version
if ($cargo -ne $version -or $package -ne $version) {
  throw "Versionen passen nicht zusammen: tauri.conf.json=$version, Cargo.toml=$cargo, package.json=$package"
}

$env:TAURI_SIGNING_PRIVATE_KEY = $KeyPath
if (-not $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) { $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "" }

npx tauri build
if ($LASTEXITCODE -ne 0) { throw "Build fehlgeschlagen" }

$nsis = "src-tauri\target\release\bundle\nsis"
$installer = Get-ChildItem $nsis -Filter "*_${version}_x64-setup.exe" | Select-Object -First 1
if (-not $installer) { throw "Installer fuer $version nicht gefunden in $nsis" }
$sigFile = "$($installer.FullName).sig"
if (-not (Test-Path $sigFile)) { throw "Signatur fehlt: $sigFile (createUpdaterArtifacts und Schluessel pruefen)" }

# Leerzeichen im Dateinamen werden bei GitHub-Release-Dateien zu Punkten.
$assetName = $installer.Name -replace " ", "."
$latest = [ordered]@{
  version   = $version
  notes     = $Notes
  pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
  platforms = [ordered]@{
    "windows-x86_64" = [ordered]@{
      signature = (Get-Content $sigFile -Raw).Trim()
      url       = "https://github.com/$Repo/releases/download/v$version/$assetName"
    }
  }
}

New-Item -ItemType Directory -Force "release" | Out-Null
Copy-Item $installer.FullName "release\$assetName" -Force
Copy-Item $sigFile "release\$assetName.sig" -Force
# Dieselbe Datei mit festem Namen ohne Version: darauf zeigt der Download-Knopf der Website
# (.../releases/latest/download/UCS-Setup.exe), der so nie veraltet.
$stableName = "UCS-Setup.exe"
Copy-Item $installer.FullName "release\$stableName" -Force
# Ohne BOM: Windows PowerShell 5.1 schreibt mit `-Encoding utf8` eins an den Anfang,
# und der JSON-Parser des Updaters lehnt die Datei dann ab.
[IO.File]::WriteAllText(
  (Join-Path (Get-Location) "release\latest.json"),
  ($latest | ConvertTo-Json -Depth 5),
  (New-Object System.Text.UTF8Encoding($false))
)
Write-Host "Fertig: release\$assetName, .sig und latest.json (Version $version)"

if ($Publish) {
  gh release create "v$version" "release\$assetName" "release\$assetName.sig" "release\latest.json" "release\$stableName" `
    --repo $Repo --title "UCS Desktop $version" --notes ($(if ($Notes) { $Notes } else { "Version $version" }))
  if ($LASTEXITCODE -ne 0) { throw "Release konnte nicht angelegt werden" }
  Write-Host "Veroeffentlicht: https://github.com/$Repo/releases/tag/v$version"
}
