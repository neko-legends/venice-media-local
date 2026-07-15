import fs from 'node:fs'
import path from 'node:path'
import { execFileSync, spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { fileURLToPath } from 'node:url'
import {
  createColdActivationEngine,
  COLD_ACTIVATION_ACTION,
  COLD_ACTIVATION_REASON,
  evaluateLocalWorkState,
  canonicalJson,
  sha256,
} from './phase5h-cold-activation.mjs'

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
function fail(message, code = 'WINDOWS_COLD_ACTIVATION_INVALID') { const error = new Error(message); error.code = code; throw error }
function realFile(target, label) {
  const absolute = path.resolve(target); const stat = fs.lstatSync(absolute)
  if (!stat.isFile() || stat.isSymbolicLink()) fail(`${label} must be a regular unlinked file`)
  return { path: absolute, sizeBytes: stat.size, sha256: sha256(fs.readFileSync(absolute)), lastWriteUtc: stat.mtime.toISOString() }
}
function atomicWrite(target, bytes) {
  fs.mkdirSync(path.dirname(target), { recursive: true })
  const temporary = `${target}.${process.pid}.${randomUUID()}.tmp`
  const handle = fs.openSync(temporary, 'wx', 0o600)
  try { fs.writeFileSync(handle, bytes); fs.fsyncSync(handle) } finally { fs.closeSync(handle) }
  fs.renameSync(temporary, target)
}
function inspectWindows(processName, port) {
  const script = `$p=@(Get-CimInstance Win32_Process|Where-Object{$_.Name -ieq $env:VML_PROCESS}|Select-Object ProcessId,ExecutablePath);$l=@(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue|Where-Object{$_.LocalPort -eq [int]$env:VML_PORT});[ordered]@{processes=$p;listenerCount=$l.Count}|ConvertTo-Json -Depth 4 -Compress`
  const env = { ...process.env, VML_PROCESS: processName, VML_PORT: String(port) }
  const output = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8', env, windowsHide: true, timeout: 15000 })
  const parsed = JSON.parse(output || '{}'); return { processes: Array.isArray(parsed.processes) ? parsed.processes : (parsed.processes ? [parsed.processes] : []), listenerCount: Number(parsed.listenerCount || 0) }
}
function removePathIfExists(target) {
  if (!fs.existsSync(target)) return
  const stat = fs.lstatSync(target)
  if (stat.isDirectory() && !stat.isSymbolicLink()) fs.rmSync(target, { recursive: true, force: false })
  else fs.rmSync(target, { force: false })
}
async function jsonRequest(url, { method = 'GET', bearer, body, timeoutMs = 10000 } = {}) {
  const controller = new AbortController(); const timer = setTimeout(() => controller.abort(), timeoutMs)
  try {
    const response = await fetch(url, { method, signal: controller.signal, headers: { Authorization: `Bearer ${bearer}`, ...(body ? { 'Content-Type': 'application/json' } : {}) }, ...(body ? { body: JSON.stringify(body) } : {}) })
    const value = await response.json().catch(() => ({}))
    if (!response.ok) {
      const code = typeof value?.code === 'string' && value.code ? value.code : 'AUTHENTICATED_REQUEST_FAILED'
      fail(`Authenticated request failed with HTTP ${response.status}${value?.error ? `: ${value.error}` : ''}`, code)
    }
    return { status: response.status, value }
  } finally { clearTimeout(timer) }
}

class CoreAuthority {
  constructor(baseUrl, bearer) { this.base = baseUrl.replace(/\/$/, ''); this.bearer = bearer }
  async sample(hostEvidenceDigest) {
    return (await jsonRequest(`${this.base}/api/phase5h/venice-maintenance-activation/authorizations/samples`, { method: 'POST', bearer: this.bearer, body: { hostEvidenceDigest } })).value
  }
  async issue(binding) {
    return (await jsonRequest(`${this.base}/api/phase5h/venice-maintenance-activation/authorizations`, { method: 'POST', bearer: this.bearer, body: binding })).value
  }
  async consume(id, bindingDigest) {
    return (await jsonRequest(`${this.base}/api/phase5h/venice-maintenance-activation/authorizations/${encodeURIComponent(id)}/consume`, { method: 'POST', bearer: this.bearer, body: { bindingDigest } })).value
  }
  async provider(providerId, instanceId) {
    return (await jsonRequest(`${this.base}/api/capability-providers/v1/providers/${encodeURIComponent(providerId)}/instances/${encodeURIComponent(instanceId)}`, { bearer: this.bearer })).value?.provider
  }
}

class WindowsColdHost {
  constructor(config, core, agentBearer) { this.config = config; this.core = core; this.agentBearer = agentBearer; this.saved = null; this.child = null; this.ownedLock = null }
  files(expected) {
    const retained = realFile(expected.retained.path, 'Retained executable')
    const staged = Object.fromEntries(['portable', 'installer', 'manifest'].map((key) => [key, realFile(path.join(expected.staged.slot, expected.staged[key].filename), `Staged ${key}`)]))
    return { retained, staged }
  }
  async sample(expected) {
    const inspect = inspectWindows(expected.expectedHost.processName, expected.expectedHost.port)
    const files = this.files(expected)
    const localWork = evaluateLocalWorkState({
      appDataRoot: this.config.appDataRoot,
      providerLedgerPath: this.config.providerLedgerPath,
      retained: {
        version: expected.retained.version,
        sizeBytes: files.retained.sizeBytes,
        sha256: files.retained.sha256,
      },
    })
    const discovery = realFile(this.config.discoveryPath, 'Stale discovery')
    const sample = {
      observedAt: new Date().toISOString(), processCount: inspect.processes.length, listenerCount: inspect.listenerCount,
      activeProviderOperationCount: localWork.activeProviderOperationCount, unsettledJobCount: localWork.unsettledJobCount,
      transitionInProgress: fs.existsSync(this.config.transitionLockPath) && !this.ownedLock,
      retained: { sizeBytes: files.retained.sizeBytes, sha256: files.retained.sha256 },
      staged: Object.fromEntries(Object.entries(files.staged).map(([key, value]) => [key, { sizeBytes: value.sizeBytes, sha256: value.sha256 }])),
      expectedProcessName: expected.expectedHost.processName, expectedPort: expected.expectedHost.port,
      staleDiscovery: { present: true, sizeBytes: discovery.sizeBytes, sha256: discovery.sha256, lastWriteUtc: discovery.lastWriteUtc },
      localWorkMode: localWork.mode,
      ledgerPresent: localWork.ledgerPresent,
      appDataInventoryDigest: localWork.appDataInventoryDigest,
      persistedWorkDigest: localWork.evidenceDigest,
    }
    sample.digest = sha256(canonicalJson(sample)); return sample
  }
  async acquireTransitionLock() {
    fs.mkdirSync(path.dirname(this.config.transitionLockPath), { recursive: true })
    try { this.ownedLock = { fd: fs.openSync(this.config.transitionLockPath, 'wx', 0o600) }; return this.ownedLock } catch (error) { if (error.code === 'EEXIST') return null; throw error }
  }
  async releaseTransitionLock(lock) { fs.closeSync(lock.fd); fs.rmSync(this.config.transitionLockPath, { force: false }); this.ownedLock = null }
  async activate(expected) {
    const pointerPath = this.config.pointerPath
    const ledgerPath = path.resolve(this.config.providerLedgerPath)
    const providerRoot = path.dirname(ledgerPath)
    this.saved = {
      pointer: fs.existsSync(pointerPath) ? fs.readFileSync(pointerPath) : null,
      discovery: fs.existsSync(this.config.discoveryPath) ? fs.readFileSync(this.config.discoveryPath) : null,
      ledgerExisted: fs.existsSync(ledgerPath),
      ledgerBytes: fs.existsSync(ledgerPath) ? fs.readFileSync(ledgerPath) : null,
      providerRootExisted: fs.existsSync(providerRoot),
    }
    const pointer = { schemaVersion: 1, slot: expected.staged.slot, previous: this.config.previousSlot || null, portableSha256: expected.staged.portable.sha256, manifestSha256: expected.staged.manifest.sha256, activatedAt: new Date().toISOString() }
    atomicWrite(pointerPath, Buffer.from(`${JSON.stringify(pointer, null, 2)}\n`))
    const executable = path.join(expected.staged.slot, expected.staged.portable.filename)
    this.child = spawn(executable, [], { cwd: expected.staged.slot, detached: true, stdio: 'ignore', windowsHide: true }); this.child.unref()
  }
  async verifyActivated(expected) {
    const deadline = Date.now() + 60000; let last = null
    while (Date.now() < deadline) {
      try {
        const health = (await jsonRequest(`${this.config.agentControlBaseUrl}/api/v1/health`, { bearer: this.agentBearer })).value
        const manifest = (await jsonRequest(`${this.config.agentControlBaseUrl}/api/v1/capabilities`, { bearer: this.agentBearer })).value
        const provider = await this.core.provider(expected.replacement.providerId, expected.replacement.instanceId)
        const inspect = inspectWindows(expected.expectedHost.processName, expected.expectedHost.port)
        const running = inspect.processes.length === 1 ? realFile(inspect.processes[0].ExecutablePath, 'Running executable') : null
        const releaseManifest = realFile(path.join(expected.staged.slot, expected.staged.manifest.filename), 'Running release manifest')
        const postLedger = evaluateLocalWorkState({
          appDataRoot: this.config.appDataRoot,
          providerLedgerPath: this.config.providerLedgerPath,
          retained: {
            version: expected.replacement.version,
            sizeBytes: expected.staged.portable.sizeBytes,
            sha256: expected.staged.portable.sha256,
          },
        })
        return {
          ready: health.status === 'ready',
          identityMatched: health.provider?.id === expected.replacement.providerId && health.provider?.instanceId === expected.replacement.instanceId && health.provider?.machineId === expected.replacement.machineId && health.provider?.version === expected.replacement.version,
          manifestMatched: provider?.manifestDigest?.toLowerCase() === expected.replacement.manifestDigest.toLowerCase() && manifest?.provider?.id === expected.replacement.providerId,
          routingEligible: provider?.routingEligible === true,
          activeProviderOperationCount: Number(health.activeOperationCount),
          unsettledJobCount: Number(provider?.health?.detail?.unsettledJobCount || 0),
          runningExecutableHash: running?.sha256,
          runningManifestHash: releaseManifest.sha256,
          ledgerPresent: postLedger.ledgerPresent === true,
          localLedgerActiveCount: postLedger.activeProviderOperationCount,
          localLedgerUnsettledCount: postLedger.unsettledJobCount,
        }
      } catch (error) { last = error; await sleep(1000) }
    }
    throw last || new Error('Activated release did not become ready')
  }
  async rollback(expected) {
    const inspect = inspectWindows(expected.expectedHost.processName, expected.expectedHost.port)
    if (inspect.processes.length || inspect.listenerCount) {
      const now = new Date(); const expires = new Date(now.getTime() + 60000)
      const body = { schemaVersion: '1.0', type: 'application.shutdown', requestId: randomUUID(), idempotencyKey: randomUUID(), scope: 'application:shutdown', providerId: expected.replacement.providerId, instanceId: expected.replacement.instanceId, manifestDigest: expected.replacement.manifestDigest, requestedAt: now.toISOString(), expiresAt: expires.toISOString(), reason: COLD_ACTIVATION_REASON }
      const response = await jsonRequest(`${this.config.agentControlBaseUrl}/api/v1/actions/shutdown`, { method: 'POST', bearer: this.agentBearer, body })
      if (response.status !== 202 || response.value?.state !== 'shutting_down' || response.value?.replayed !== false) fail('Rollback shutdown response was invalid', 'ROLLBACK_SHUTDOWN_INVALID')
      const deadline = Date.now() + 30000
      while (Date.now() < deadline && inspectWindows(expected.expectedHost.processName, expected.expectedHost.port).processes.length) await sleep(500)
      if (inspectWindows(expected.expectedHost.processName, expected.expectedHost.port).processes.length) fail('Activated process did not exit gracefully', 'ROLLBACK_SHUTDOWN_TIMEOUT')
    }
    if (this.saved.pointer === null) fs.rmSync(this.config.pointerPath, { force: true }); else atomicWrite(this.config.pointerPath, this.saved.pointer)
    if (this.saved.discovery === null) fs.rmSync(this.config.discoveryPath, { force: true }); else atomicWrite(this.config.discoveryPath, this.saved.discovery)
    const ledgerPath = path.resolve(this.config.providerLedgerPath)
    const providerRoot = path.dirname(ledgerPath)
    if (this.saved.ledgerExisted) {
      atomicWrite(ledgerPath, this.saved.ledgerBytes)
    } else {
      removePathIfExists(ledgerPath)
      if (!this.saved.providerRootExisted) removePathIfExists(providerRoot)
    }
    const child = spawn(expected.retained.path, [], { cwd: path.dirname(expected.retained.path), detached: true, stdio: 'ignore', windowsHide: true }); child.unref()
    const deadline = Date.now() + 60000
    while (Date.now() < deadline) {
      try {
        const provider = await this.core.provider(expected.retained.providerId, expected.retained.instanceId)
        const running = inspectWindows(expected.expectedHost.processName, expected.expectedHost.port).processes
        if (running.length === 1 && provider?.routingEligible === true && provider?.manifestDigest === expected.retained.manifestDigest && Number(provider?.health?.activeOperationCount || 0) === 0) {
          return {
            passed: true,
            version: expected.retained.version,
            sha256: realFile(running[0].ExecutablePath, 'Rollback executable').sha256,
            routingEligible: true,
            activeProviderOperationCount: 0,
            unsettledJobCount: Number(provider?.health?.detail?.unsettledJobCount || 0),
            ledgerPresent: fs.existsSync(ledgerPath),
          }
        }
      } catch (_error) {}
      await sleep(1000)
    }
    return { passed: false, ledgerPresent: fs.existsSync(ledgerPath) }
  }
  async cleanup() { this.child = null }
}

function validateConfig(config) {
  for (const key of ['coreBaseUrl', 'agentControlBaseUrl', 'appDataRoot', 'providerLedgerPath', 'discoveryPath', 'transitionLockPath', 'pointerPath', 'evidenceDirectory', 'expected']) if (!config?.[key]) fail(`Configuration field ${key} is required`)
  return config
}
async function readSecrets() {
  const chunks = []; for await (const chunk of process.stdin) chunks.push(chunk)
  const value = JSON.parse(Buffer.concat(chunks).toString('utf8').replace(/^\uFEFF/, '').trim())
  if (typeof value.coreAuthorization !== 'string' || !value.coreAuthorization || typeof value.agentControlCredential !== 'string' || !value.agentControlCredential) fail('Two in-memory credentials are required on redirected standard input')
  return value
}
async function main() {
  if (process.platform !== 'win32') fail('The cold-activation operator requires Windows')
  const configPath = process.argv[2]; if (!configPath || process.argv.length !== 3) fail('Usage: node phase5h-cold-activation-windows.mjs <non-secret-config.json>')
  const config = validateConfig(JSON.parse(fs.readFileSync(path.resolve(configPath), 'utf8')))
  let secrets = await readSecrets(); const core = new CoreAuthority(config.coreBaseUrl, secrets.coreAuthorization); const host = new WindowsColdHost(config, core, secrets.agentControlCredential)
  secrets = null
  const reportBase = { schemaVersion: 1, operation: 'phase5h-venice-cold-activation', startedAt: new Date().toISOString(), configDigest: sha256(canonicalJson(config)) }
  let report
  try { report = { ...reportBase, ...(await createColdActivationEngine({ host, authority: core }).execute(config.expected)), completedAt: new Date().toISOString() } }
  catch (error) {
    report = {
      ...reportBase,
      disposition: 'failed',
      code: error.code || 'COLD_ACTIVATION_FAILED',
      details: { ...(error.details || {}), message: String(error.message || '').slice(0, 500) },
      completedAt: new Date().toISOString(),
    }
  }
  fs.mkdirSync(config.evidenceDirectory, { recursive: true }); const reportPath = path.join(config.evidenceDirectory, `cold-activation-${report.startedAt.replace(/[-:.]/g, '')}.json`); atomicWrite(reportPath, Buffer.from(`${JSON.stringify(report, null, 2)}\n`))
  process.stdout.write(`${JSON.stringify({ disposition: report.disposition, reportPath })}\n`)
  if (report.disposition !== 'activation-passed') process.exitCode = 1
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch((error) => { process.stderr.write(`${error.code || 'COLD_ACTIVATION_FAILED'}\n`); process.exitCode = 1 })

export { CoreAuthority, WindowsColdHost, validateConfig }
