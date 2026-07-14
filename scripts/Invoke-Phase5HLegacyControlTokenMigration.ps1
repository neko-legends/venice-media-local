[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$CandidateExecutable,
  [Parameter(Mandatory = $true)][string]$SettingsPath,
  [string]$CoreBaseUrl = 'http://eva:3456',
  [ValidateRange(60, 600)][int]$TimeoutSeconds = 300,
  [switch]$OpenVerificationBrowser
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$actionKey = 'venice-media-local:migrate-legacy-agent-control-token'
$connector = $null
$status = $null
$authorization = $null
$process = $null
$mutex = $null
$ownsMutex = $false

function Assert-SafeArgument([string]$Value, [string]$Label) {
  if ([string]::IsNullOrWhiteSpace($Value) -or $Value.Contains('"') -or $Value.Contains("`r") -or $Value.Contains("`n")) {
    throw "$Label is not a safe process argument."
  }
}

try {
  if ($PSVersionTable.PSEdition -eq 'Core' -or $PSVersionTable.PSVersion.Major -ne 5) {
    throw 'The migration operator requires foreground 64-bit Windows PowerShell 5.1.'
  }
  if (-not [Environment]::Is64BitProcess) { throw 'The migration operator requires a 64-bit process.' }
  $expectedHost = [IO.Path]::GetFullPath((Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'))
  $actualHost = [IO.Path]::GetFullPath([Diagnostics.Process]::GetCurrentProcess().MainModule.FileName)
  if (-not [string]::Equals($actualHost, $expectedHost, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The migration operator must run in the trusted foreground Windows PowerShell executable.'
  }
  $mutex = New-Object Threading.Mutex($false, 'Local\VeniceMediaLocalPhase5HLegacyCredentialMigration')
  try { $ownsMutex = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $ownsMutex = $true }
  if (-not $ownsMutex) { throw 'Another legacy credential migration operator is running.' }

  $CandidateExecutable = [IO.Path]::GetFullPath($CandidateExecutable)
  $SettingsPath = [IO.Path]::GetFullPath($SettingsPath)
  if (-not (Test-Path -LiteralPath $CandidateExecutable -PathType Leaf)) { throw 'Candidate executable is missing.' }
  if (-not (Test-Path -LiteralPath $SettingsPath -PathType Leaf)) { throw 'Settings file is missing.' }
  foreach ($pair in @(@($CoreBaseUrl, 'Core URL'), @($SettingsPath, 'Settings path'))) {
    Assert-SafeArgument ([string]$pair[0]) ([string]$pair[1])
  }

  $intent = [ordered]@{
    key = $actionKey
    label = 'Migrate the Venice local control credential into Windows secure storage'
    method = 'POST'
    path = '/phase5h/venice/legacy-agent-control-token/migrate'
  }
  $request = [ordered]@{
    desktopInstanceId = 'venice-media-local-phase5h-ripper'
    purpose = 'verify-action'
    intent = $intent
    verificationDurationMs = 300000
  }
  $connector = Invoke-RestMethod -Method Post -Uri "$($CoreBaseUrl.TrimEnd('/'))/api/auth/desktop-connector" -ContentType 'application/json' -Body ($request | ConvertTo-Json -Depth 5 -Compress)
  if ([string]::IsNullOrWhiteSpace([string]$connector.connectorToken) -or [string]::IsNullOrWhiteSpace([string]$connector.connectorUrl)) {
    throw 'Core did not return a usable verified-action connector.'
  }
  Write-Output ([ordered]@{ status = 'verification-required'; action = $actionKey; handoff = 'desktop-browser' } | ConvertTo-Json -Compress)
  if ($OpenVerificationBrowser) { Start-Process ([string]$connector.connectorUrl) }

  $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $status = Invoke-RestMethod -Method Get -Uri "$($CoreBaseUrl.TrimEnd('/'))/api/auth/desktop-connector/$($connector.connectorToken)/status"
    if ([string]$status.status -eq 'verified') { break }
    if ([string]$status.status -in @('expired', 'denied', 'cancelled', 'unavailable')) {
      throw "Verified action ended with status $($status.status)."
    }
    Start-Sleep -Seconds 2
  } while ([DateTimeOffset]::UtcNow -lt $deadline)
  if ([string]$status.status -ne 'verified' -or [string]::IsNullOrWhiteSpace([string]$status.token)) {
    throw 'Timed out waiting for the verified action.'
  }
  if ([string]$status.trust.level -ne 'verified_action' -or [string]$status.trust.action.key -ne $actionKey) {
    throw 'Core returned a mismatched verified action.'
  }

  $authorization = [string]$status.token
  $session = $null
  try {
    $session = Invoke-RestMethod -Method Get -Uri "$($CoreBaseUrl.TrimEnd('/'))/api/auth/session" -Headers @{ Authorization = "Bearer $authorization" }
  } catch {
    $statusCode = [int]$_.Exception.Response.StatusCode
    throw "Core rejected the in-memory migration authorization during session preflight (HTTP $statusCode)."
  }
  $expiry = [DateTimeOffset]::MinValue
  $expiryValid = [DateTimeOffset]::TryParse([string]$session.trust.expiresAt, [ref]$expiry) -and $expiry -gt [DateTimeOffset]::UtcNow
  if ([string]$session.user.id -ne 'user-jun' -or [string]$session.user.type -ne 'human' -or
      [string]$session.trust.level -ne 'verified_action' -or [bool]$session.trust.needsReverification -or
      [string]$session.trust.action.key -ne $actionKey -or -not $expiryValid) {
    throw 'Core returned mismatched migration authorization claims during session preflight.'
  }
  $session = $null
  $start = New-Object Diagnostics.ProcessStartInfo
  $start.FileName = $CandidateExecutable
  $start.Arguments = "--phase5h-migrate-legacy-agent-control-token `"$CoreBaseUrl`" `"$SettingsPath`""
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.RedirectStandardInput = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  $start.StandardInputEncoding = New-Object Text.UTF8Encoding($false)
  $process = New-Object Diagnostics.Process
  $process.StartInfo = $start
  if (-not $process.Start()) { throw 'Failed to start the migration candidate.' }
  $process.StandardInput.WriteLine($authorization)
  $process.StandardInput.Close()
  $authorization = $null
  $session = $null
  $status = $null
  $connector = $null
  if (-not $process.WaitForExit(60000)) { throw 'Migration candidate did not exit within 60 seconds.' }
  $output = $process.StandardOutput.ReadToEnd()
  $errorOutput = $process.StandardError.ReadToEnd()
  if ($process.ExitCode -ne 0) {
    throw "Migration candidate failed without changing the supported replacement contract. $errorOutput"
  }
  $result = $output | ConvertFrom-Json
  if ([string]$result.status -ne 'ok') { throw 'Migration candidate returned an invalid result.' }
  Write-Output ([ordered]@{ status = 'migration-complete'; result = [string]$result.migration; settingsPath = $SettingsPath } | ConvertTo-Json -Compress)
} finally {
  $authorization = $null
  $status = $null
  $connector = $null
  if ($null -ne $process) { $process.Dispose() }
  if ($ownsMutex -and $null -ne $mutex) { $mutex.ReleaseMutex() }
  if ($null -ne $mutex) { $mutex.Dispose() }
}
