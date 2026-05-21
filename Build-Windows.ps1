$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$npm = 'C:\Program Files\nodejs\npm.cmd'
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'

$env:Path = "$cargoBin;$env:Path"
$env:npm_config_prefix = 'C:\Program Files\nodejs'

Set-Location -LiteralPath $root
& $npm run version:build
& $npm run tauri -- build --config src-tauri/tauri.version.conf.json

$versionConfig = Get-Content -LiteralPath (Join-Path $root 'src-tauri\tauri.version.conf.json') -Raw | ConvertFrom-Json
$version = [string]$versionConfig.version
$releaseDir = Join-Path $root 'src-tauri\target\release'
$bundleDir = Join-Path $releaseDir 'bundle\nsis'
$portableSource = Join-Path $releaseDir 'venice-media-local.exe'
$portableName = "Venice Media Local_${version}_x64-portable.exe"
$portableTarget = Join-Path $bundleDir $portableName

if (-not (Test-Path -LiteralPath $portableSource -PathType Leaf)) {
  throw "Portable source executable not found: $portableSource"
}

New-Item -ItemType Directory -Force -Path $bundleDir | Out-Null
for ($attempt = 1; $attempt -le 5; $attempt++) {
  try {
    Copy-Item -LiteralPath $portableSource -Destination $portableTarget -Force
    break
  } catch {
    if ($attempt -eq 5) {
      $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
      $portableTarget = Join-Path $bundleDir "Venice Media Local_${version}_${stamp}_x64-portable.exe"
      Copy-Item -LiteralPath $portableSource -Destination $portableTarget -Force
      break
    }
    Start-Sleep -Milliseconds 700
  }
}
Write-Host "Portable executable: $portableTarget"
