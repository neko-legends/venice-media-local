[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$ConfigPath,
  [ValidateRange(60, 600)][int]$TimeoutSeconds = 300,
  [switch]$OpenVerificationBrowser
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$actionKey = 'venice-media-local:activate-release-slot'
$connector = $null; $status = $null; $authorization = $null; $agentCredential = $null
$process = $null; $mutex = $null; $ownsMutex = $false

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

  $request = [ordered]@{ desktopInstanceId='venice-media-local-phase5h-ripper'; purpose='verify-action'; intent=[ordered]@{ key=$actionKey; label='Authorize one exact Venice Media Local cold release-slot activation'; method='POST'; path='/api/phase5h/venice-maintenance-activation/authorizations' }; verificationDurationMs=300000 }
  $connector = Invoke-RestMethod -Method Post -Uri "$coreBaseUrl/api/auth/desktop-connector" -ContentType 'application/json' -Body ($request | ConvertTo-Json -Depth 5 -Compress)
  if ([string]::IsNullOrWhiteSpace([string]$connector.connectorToken) -or [string]::IsNullOrWhiteSpace([string]$connector.connectorUrl)) { throw 'Core did not return a usable verified-action connector.' }
  Write-Output ([ordered]@{status='verification-required';action=$actionKey;handoff='default-browser'}|ConvertTo-Json -Compress)
  if ($OpenVerificationBrowser) { Start-Process ([string]$connector.connectorUrl) }
  $deadline=[DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
  do { $status=Invoke-RestMethod -Method Get -Uri "$coreBaseUrl/api/auth/desktop-connector/$($connector.connectorToken)/status"; if([string]$status.status-eq'verified'){break}; if([string]$status.status-in@('expired','denied','cancelled','unavailable')){throw "Verified action ended with status $($status.status)."}; Start-Sleep -Seconds 2 } while([DateTimeOffset]::UtcNow-lt$deadline)
  if([string]$status.status-ne'verified'-or[string]::IsNullOrWhiteSpace([string]$status.token)-or[string]$status.trust.level-ne'verified_action'-or[string]$status.trust.action.key-ne$actionKey){throw 'Core did not return the exact verified action.'}
  $authorization=[string]$status.token
  $session=Invoke-RestMethod -Method Get -Uri "$coreBaseUrl/api/auth/session" -Headers @{Authorization="Bearer $authorization"}
  if([string]$session.user.id-ne'user-jun'-or[string]$session.user.type-ne'human'-or[string]$session.trust.level-ne'verified_action'-or[bool]$session.trust.needsReverification-or[string]$session.trust.action.key-ne$actionKey){throw 'Core returned mismatched cold-activation claims.'}
  $agentCredential=[Phase5HCredentialReader]::ReadGeneric('agent-control-token.venice-media-local')
  $nodeScript=Join-Path $PSScriptRoot 'phase5h-cold-activation-windows.mjs'
  $start=New-Object Diagnostics.ProcessStartInfo
  $start.FileName=(Get-Command node).Source; $start.Arguments="`"$nodeScript`" `"$ConfigPath`""; $start.UseShellExecute=$false; $start.CreateNoWindow=$true; $start.RedirectStandardInput=$true; $start.RedirectStandardOutput=$true; $start.RedirectStandardError=$true
  $process=New-Object Diagnostics.Process; $process.StartInfo=$start
  if(-not$process.Start()){throw 'Failed to start the cold-activation engine.'}
  $transport=[ordered]@{coreAuthorization=$authorization;agentControlCredential=$agentCredential}|ConvertTo-Json -Compress
  $process.StandardInput.WriteLine($transport); $process.StandardInput.Close()
  $transport=$null; $authorization=$null; $agentCredential=$null; $session=$null; $status=$null; $connector=$null
  if(-not$process.WaitForExit(180000)){throw 'Cold-activation engine exceeded its bounded runtime.'}
  $output=$process.StandardOutput.ReadToEnd(); $diagnostic=$process.StandardError.ReadToEnd()
  if($process.ExitCode-ne0){throw "Cold activation failed closed. $diagnostic"}
  Write-Output $output.Trim()
} finally {
  $authorization=$null; $agentCredential=$null; $status=$null; $connector=$null
  if($null-ne$process){$process.Dispose()}
  if($ownsMutex-and$null-ne$mutex){$mutex.ReleaseMutex()}; if($null-ne$mutex){$mutex.Dispose()}
}
