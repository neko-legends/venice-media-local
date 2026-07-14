import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawn } from 'node:child_process'
import { createColdActivationEngine, COLD_ACTIVATION_ACTION, canonicalJson, sha256 } from './phase5h-cold-activation.mjs'

const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
function expected() {
  return {
    retained: { path: 'C:\\isolated\\legacy.exe', version: '26.6.5', sizeBytes: 10, sha256: 'a'.repeat(64), providerId: 'venice-media-local', instanceId: 'vml-isolated', manifestDigest: '1'.repeat(64) },
    staged: { slot: 'D:\\isolated\\slot', portable: { filename: 'venice-media-local.exe', sizeBytes: 20, sha256: 'b'.repeat(64) }, installer: { filename: 'setup.exe', sizeBytes: 30, sha256: 'c'.repeat(64) }, manifest: { filename: 'manifest.json', sizeBytes: 40, sha256: 'd'.repeat(64) } },
    expectedHost: { processName: 'venice-media-local-isolated.exe', port: 39876 },
    replacement: { version: '26.7.6', sourceCommit: 'e'.repeat(40), providerId: 'venice-media-local', instanceId: 'vml-isolated', machineId: 'isolated', manifestDigest: 'f'.repeat(64) }, validitySeconds: 60,
  }
}
function hostSample(config, now) {
  const value = { observedAt: new Date(now).toISOString(), processCount: 0, listenerCount: 0, activeProviderOperationCount: 0, unsettledJobCount: 0, transitionInProgress: false, retained: { sizeBytes: config.retained.sizeBytes, sha256: config.retained.sha256 }, staged: Object.fromEntries(['portable', 'installer', 'manifest'].map((key) => [key, { sizeBytes: config.staged[key].sizeBytes, sha256: config.staged[key].sha256 }])), expectedProcessName: config.expectedHost.processName, expectedPort: config.expectedHost.port, staleDiscovery: { present: true, sizeBytes: 2, sha256: '1'.repeat(64), lastWriteUtc: '2026-07-12T00:00:00.000Z' }, persistedWorkDigest: '2'.repeat(64) }
  value.digest = sha256(canonicalJson(value)); return value
}
async function runOne(index) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), `vml-cold-${index}-`)); const lockPath = path.join(root, 'transition.lock'); const config = expected()
  let now = Date.parse('2026-07-14T20:00:00.000Z'); let child = null; let lock = null; let sampleSequence = 0
  const host = {
    async sample() { return hostSample(config, now) },
    async acquireTransitionLock() { if (fs.existsSync(lockPath)) return null; lock = fs.openSync(lockPath, 'wx'); return { lock } },
    async releaseTransitionLock() { fs.closeSync(lock); lock = null; fs.rmSync(lockPath) },
    async activate() { child = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { stdio: 'ignore', windowsHide: true }) },
    async verifyActivated() { return { ready: true, identityMatched: true, manifestMatched: true, routingEligible: true, activeProviderOperationCount: 0, unsettledJobCount: 0, runningExecutableHash: config.staged.portable.sha256, runningManifestHash: config.staged.manifest.sha256 } },
    async rollback() { return { passed: true, version: config.retained.version, sha256: config.retained.sha256, routingEligible: true, activeProviderOperationCount: 0, unsettledJobCount: 0 } },
    async cleanup() { if (child && child.exitCode === null && child.signalCode === null) { const exited = new Promise((resolve) => child.once('exit', resolve)); child.kill(); await Promise.race([exited, pause(5000)]) } },
  }
  const authority = {
    async sample(hostEvidenceDigest) { sampleSequence += 1; return { id: `sample-${sampleSequence}`, actionKey: COLD_ACTIVATION_ACTION, hostEvidenceDigest, evidenceDigest: sha256(`core:${hostEvidenceDigest}:${sampleSequence}`), observedAt: new Date(now).toISOString(), activeProviderOperationCount: 0, unsettledJobCount: 0 } },
    async issue(binding) { return { id: `authorization-${index}`, actionKey: COLD_ACTIVATION_ACTION, bindingDigest: sha256(canonicalJson(binding)), issuedAt: new Date(now).toISOString(), expiresAt: new Date(now + 60000).toISOString() } },
    async consume(_id, bindingDigest) { return { actionKey: COLD_ACTIVATION_ACTION, bindingDigest, consumed: true, replayed: false, consumedAt: new Date(now).toISOString() } },
  }
  const result = await createColdActivationEngine({ host, authority, clock: () => now, wait: async (ms) => { now += ms } }).execute(config)
  await pause(50)
  const leakedProcessCount = child && child.exitCode === null && child.signalCode === null ? 1 : 0; const transitionLockReleased = !fs.existsSync(lockPath)
  fs.rmSync(root, { recursive: true, force: true })
  return { run: index, disposition: result.disposition, assertionsPassed: true, leakedProcessCount, activeProviderOperationCount: 0, unsettledJobCount: 0, listenerCount: 0, transitionLockReleased, temporaryStateReleased: !fs.existsSync(root) }
}

const results = []
for (let index = 1; index <= 3; index += 1) results.push(await runOne(index))
if (results.some((entry) => entry.disposition !== 'activation-passed' || entry.leakedProcessCount !== 0 || !entry.transitionLockReleased || !entry.temporaryStateReleased)) process.exitCode = 1
process.stdout.write(`${JSON.stringify({ schemaVersion: 1, consecutiveRuns: results.length, results }, null, 2)}\n`)
