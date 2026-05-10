$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$npm = 'C:\Program Files\nodejs\npm.cmd'
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'

$env:Path = "$cargoBin;$env:Path"
$env:npm_config_prefix = 'C:\Program Files\nodejs'

Set-Location -LiteralPath $root
& $npm run tauri -- dev
