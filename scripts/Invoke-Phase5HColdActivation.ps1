[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$ConfigPath,
  [ValidateRange(60, 600)][int]$TimeoutSeconds = 300,
  [switch]$OpenVerificationBrowser
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$actionKey = 'venice-media-local:activate-release-slot'
$authorization = $null; $agentCredential = $null
$process = $null; $mutex = $null; $ownsMutex = $false
$handoff = $null; $claim = $null

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Phase5HCredentialReader {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  private struct CREDENTIAL { public UInt32 Flags; public UInt32 Type; public IntPtr TargetName; public IntPtr Comment; public System.Runtime.InteropServices.ComTypes.FILETIME LastWritten; public UInt32 CredentialBlobSize; public IntPtr CredentialBlob; public UInt32 Persist; public UInt32 AttributeCount; public IntPtr Attributes; public IntPtr TargetAlias; public IntPtr UserName; }
  [DllImport("advapi32.dll", EntryPoint="CredReadW", CharSet=CharSet.Unicode, SetLastError=true)] private static extern bool CredRead(string target, uint type, uint flags, out IntPtr credential);
  [DllImport("advapi32.dll", SetLastError=true)] private static extern void CredFree(IntPtr credential);
  public static string ReadGeneric(string target) {
    IntPtr pointer;
    if (!CredRead(target, 1, 0, out pointer)) throw new InvalidOperationException("Required Windows secure-store entry is unavailable.");
    try { var value = (CREDENTIAL)Marshal.PtrToStructure(pointer, typeof(CREDENTIAL)); if (value.CredentialBlob == IntPtr.Zero || value.CredentialBlobSize == 0) throw new InvalidOperationException("Required Windows secure-store entry is empty."); byte[] bytes = new byte[value.CredentialBlobSize]; Marshal.Copy(value.CredentialBlob, bytes, 0, bytes.Length); return System.Text.Encoding.UTF8.GetString(bytes); }
    finally { CredFree(pointer); }
  }
}
'@

try {
  if ($PSVersionTable.PSEdition -eq 'Core' -or $PSVersionTable.PSVersion.Major -ne 5 -or -not [Environment]::Is64BitProcess) { throw 'The cold-activation operator requires foreground 64-bit Windows PowerShell 5.1.' }
  $expectedHost = [IO.Path]::GetFullPath((Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'))
  $actualHost = [IO.Path]::GetFullPath([Diagnostics.Process]::GetCurrentProcess().MainModule.FileName)
  if (-not [string]::Equals($actualHost, $expectedHost, [StringComparison]::OrdinalIgnoreCase)) { throw 'The cold-activation operator must run in the trusted Windows PowerShell executable.' }
  $ConfigPath = [IO.Path]::GetFullPath($ConfigPath)
  if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) { throw 'Cold-activation configuration is missing.' }
  $config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
  $coreBaseUrl = ([string]$config.coreBaseUrl).TrimEnd('/')
  if (-not $coreBaseUrl.StartsWith('http://') -and -not $coreBaseUrl.StartsWith('https://')) { throw 'Core URL is invalid.' }
  $mutex = New-Object Threading.Mutex($false, 'Local\VeniceMediaLocalPhase5HColdActivationOperator')
  try { $ownsMutex = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { throw 'An abandoned cold-activation operator was detected; inspect and clear transition state manually.' }
  if (-not $ownsMutex) { throw 'Another cold-activation operator is running.' }

  # Human-web operator handoff only.
  $handoff = Invoke-RestMethod -Method Post -Uri "$coreBaseUrl/api/phase5h/venice-maintenance-activation/operator-handoff/begin" -ContentType 'application/json' -Body '{}'
  if ([string]::IsNullOrWhiteSpace([string]$handoff.handoffId) -or [string]::IsNullOrWhiteSpace([string]$handoff.userCode) -or [string]::IsNullOrWhiteSpace([string]$handoff.pollSecret)) {
    throw 'Core did not return a usable operator handoff.'
  }
  if ([string]$handoff.actionKey -ne $actionKey) { throw 'Core returned a mismatched operator handoff action.' }
  $browserBase = $coreBaseUrl
  # Chat hosts the handoff modal. /settings is not a SPA route and redirects to login.
  $browserUrl = "$browserBase/eva-orchestrator/chat?phase5hVeniceActivationHandoff=$([uri]::EscapeDataString([string]$handoff.userCode))"
  if ([string]$handoff.browserPath -match '/eva-orchestrator/chat\?') {
    $browserUrl = "$browserBase$($handoff.browserPath)"
  }
  Write-Output ([ordered]@{
    status = 'verification-required'
    action = $actionKey
    handoff = 'normal-human-web-session'
    userCode = [string]$handoff.userCode
    browserUrl = $browserUrl
    note = 'Approve exactly venice-media-local:activate-release-slot in the Core web app, then complete the operator handoff with the displayed user code.'
  } | ConvertTo-Json -Compress)
  if ($OpenVerificationBrowser) { Start-Process $browserUrl }

  $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
  do {
    $claim = Invoke-RestMethod -Method Post -Uri "$coreBaseUrl/api/phase5h/venice-maintenance-activation/operator-handoff/claim" `
      -ContentType 'application/json' -Body (@{ handoffId = [string]$handoff.handoffId; pollSecret = [string]$handoff.pollSecret } | ConvertTo-Json -Compress)
    if ([string]$claim.status -eq 'claimed') { break }
    if ([string]$claim.status -notin @('pending')) { throw "Operator handoff ended with status $($claim.status)." }
    Start-Sleep -Seconds 2
  } while ([DateTimeOffset]::UtcNow -lt $deadline)

  if ([string]$claim.status -ne 'claimed' -or [string]::IsNullOrWhiteSpace([string]$claim.token)) {
    throw 'Timed out waiting for the normal human-web operator handoff claim.'
  }
  if ([string]$claim.actionKey -ne $actionKey) { throw 'Claimed operator handoff action mismatch.' }

  $authorization = [string]$claim.token
  $session = Invoke-RestMethod -Method Get -Uri "$coreBaseUrl/api/auth/session" -Headers @{ Authorization = "Bearer $authorization" }
  $scopes = @($session.authContext.scopes)
  if (-not $scopes) { $scopes = @($session.scopes) }
  if ($scopes -contains 'eva-desktop' -or [string]$session.authContext.mode -eq 'eva-desktop' -or [string]$session.mode -eq 'eva-desktop') {
    throw 'Claimed session is desktop-scoped; refusing to continue.'
  }
  if ([string]$session.user.id -ne 'user-jun' -or [string]$session.user.type -ne 'human' -or
      [string]$session.trust.level -ne 'verified_action' -or [bool]$session.trust.needsReverification -or
      [string]$session.trust.action.key -ne $actionKey) {
    throw 'Core returned mismatched normal human-web activation claims after handoff claim.'
  }

  # Preflight: first cold-sample must succeed on the human-web token before host mutation.
  $preflightDigest = -join ((1..64) | ForEach-Object { '{0:x}' -f (Get-Random -Maximum 16) })
  try {
    Invoke-RestMethod -Method Post -Uri "$coreBaseUrl/api/phase5h/venice-maintenance-activation/authorizations/samples" `
      -Headers @{ Authorization = "Bearer $authorization" } -ContentType 'application/json' `
      -Body (@{ hostEvidenceDigest = $preflightDigest } | ConvertTo-Json -Compress) | Out-Null
  } catch {
    $code = $null
    try { $code = [int]$_.Exception.Response.StatusCode } catch { }
    throw "Human-web cold-sample preflight failed before activation (HTTP $code). No host mutation was attempted."
  }

  $agentCredential = [Phase5HCredentialReader]::ReadGeneric('agent-control-token.venice-media-local')
  $nodeScript = Join-Path $PSScriptRoot 'phase5h-cold-activation-windows.mjs'
  $start = New-Object Diagnostics.ProcessStartInfo
  $start.FileName = (Get-Command node).Source
  $start.Arguments = "`"$nodeScript`" `"$ConfigPath`""
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.RedirectStandardInput = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  $process = New-Object Diagnostics.Process
  $process.StartInfo = $start
  if (-not $process.Start()) { throw 'Failed to start the cold-activation engine.' }
  $transport = [ordered]@{ coreAuthorization = $authorization; agentControlCredential = $agentCredential } | ConvertTo-Json -Compress
  $process.StandardInput.WriteLine($transport)
  $process.StandardInput.Close()
  $transport = $null; $authorization = $null; $agentCredential = $null; $session = $null; $claim = $null; $handoff = $null
  if (-not $process.WaitForExit(180000)) { throw 'Cold-activation engine exceeded its bounded runtime.' }
  $output = $process.StandardOutput.ReadToEnd()
  $diagnostic = $process.StandardError.ReadToEnd()
  if ($process.ExitCode -ne 0) { throw "Cold activation failed closed. $diagnostic" }
  Write-Output $output.Trim()
} finally {
  $authorization = $null; $agentCredential = $null; $claim = $null; $handoff = $null
  if ($null -ne $process) { $process.Dispose() }
  if ($ownsMutex -and $null -ne $mutex) { $mutex.ReleaseMutex() }
  if ($null -ne $mutex) { $mutex.Dispose() }
}
