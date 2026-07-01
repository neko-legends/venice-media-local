$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$nodeBin = 'C:\Program Files\nodejs'
$node = Join-Path $nodeBin 'node.exe'
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
$localBin = Join-Path $root 'node_modules\.bin'

$env:Path = "$localBin;$nodeBin;$cargoBin;$env:Path"

function Invoke-Checked {
  param(
    [Parameter(Mandatory = $true)][string]$Label,
    [Parameter(Mandatory = $true)][string]$FilePath,
    [Parameter(Mandatory = $true)][string[]]$Arguments
  )

  & $FilePath @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "$Label failed with exit code $LASTEXITCODE"
  }
}

Set-Location -LiteralPath $root
$buildStartedAt = Get-Date
Invoke-Checked 'Generate build config' $node @((Join-Path $root 'scripts\write-build-config.mjs'))
Invoke-Checked 'TypeScript build' $node @((Join-Path $root 'node_modules\typescript\bin\tsc'))
Invoke-Checked 'Vite build' $node @((Join-Path $root 'node_modules\vite\bin\vite.js'), 'build')
Invoke-Checked 'Tauri build' $node @((Join-Path $root 'node_modules\@tauri-apps\cli\tauri.js'), 'build', '--config', 'src-tauri/tauri.version.conf.json')

$versionConfig = Get-Content -LiteralPath (Join-Path $root 'src-tauri\tauri.version.conf.json') -Raw | ConvertFrom-Json
$version = [string]$versionConfig.version
$releaseDir = Join-Path $root 'src-tauri\target\release'
$bundleDir = Join-Path $releaseDir 'bundle\nsis'
$portableSource = Join-Path $releaseDir 'venice-media-local.exe'
$portableName = "Venice Media Local_${version}_x64-portable.exe"
$portableTarget = Join-Path $bundleDir $portableName
$installerName = "Venice Media Local_${version}_x64-setup.exe"
$installerSource = Join-Path $bundleDir $installerName
$standardInstallerDir = Join-Path $root 'release\installer'
$standardPortableDir = Join-Path $root 'release\portable'
$standardInstallerTarget = Join-Path $standardInstallerDir $installerName
$standardPortableTarget = Join-Path $standardPortableDir 'venice-media-local.exe'

if (-not (Test-Path -LiteralPath $portableSource -PathType Leaf)) {
  throw "Portable source executable not found: $portableSource"
}

$portableItem = Get-Item -LiteralPath $portableSource
if ($portableItem.LastWriteTime -lt $buildStartedAt) {
  throw "Portable source executable was not refreshed by this build: $portableSource"
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

if (-not (Test-Path -LiteralPath $installerSource -PathType Leaf)) {
  throw "Installer executable not found: $installerSource"
}

$installerItem = Get-Item -LiteralPath $installerSource
if ($installerItem.LastWriteTime -lt $buildStartedAt) {
  throw "Installer executable was not refreshed by this build: $installerSource"
}

New-Item -ItemType Directory -Force -Path $standardInstallerDir | Out-Null
New-Item -ItemType Directory -Force -Path $standardPortableDir | Out-Null
Copy-Item -LiteralPath $installerSource -Destination $standardInstallerTarget -Force
Copy-Item -LiteralPath $portableSource -Destination $standardPortableTarget -Force

Write-Host "Installer executable: $standardInstallerTarget"
Write-Host "Portable executable: $standardPortableTarget"
Write-Host "Versioned portable executable: $portableTarget"
