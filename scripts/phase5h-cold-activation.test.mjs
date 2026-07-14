import assert from 'node:assert/strict'
import test from 'node:test'
import { createColdActivationEngine, COLD_ACTIVATION_ACTION, authorizationBinding, canonicalJson, sha256 } from './phase5h-cold-activation.mjs'
import fs from 'node:fs'

function expected() {
  return {
    retained: { path: 'C:\\legacy\\venice-media-local.exe', version: '26.6.5', sizeBytes: 10, sha256: 'a'.repeat(64) },
    staged: { slot: 'D:\\stage\\slot', portable: { filename: 'venice-media-local.exe', sizeBytes: 20, sha256: 'b'.repeat(64) }, installer: { filename: 'setup.exe', sizeBytes: 30, sha256: 'c'.repeat(64) }, manifest: { filename: 'manifest.json', sizeBytes: 40, sha256: 'd'.repeat(64) } },
    expectedHost: { processName: 'venice-media-local.exe', port: 9876 },
    replacement: { version: '26.7.6', sourceCommit: 'e'.repeat(40), providerId: 'venice-media-local', instanceId: 'vml-test', machineId: 'ripper', manifestDigest: 'f'.repeat(64) },
    validitySeconds: 60,
  }
}

function sample(config, time, changes = {}) {
  const value = {
    observedAt: new Date(time).toISOString(), processCount: 0, listenerCount: 0,
    activeProviderOperationCount: 0, unsettledJobCount: 0, transitionInProgress: false,
    retained: { sizeBytes: config.retained.sizeBytes, sha256: config.retained.sha256 },
    staged: Object.fromEntries(['portable', 'installer', 'manifest'].map((key) => [key, { sizeBytes: config.staged[key].sizeBytes, sha256: config.staged[key].sha256 }])),
    expectedProcessName: config.expectedHost.processName, expectedPort: config.expectedHost.port,
    staleDiscovery: { present: true, sizeBytes: 12, sha256: '1'.repeat(64), lastWriteUtc: '2026-07-12T08:02:06.228Z' },
    persistedWorkDigest: '2'.repeat(64), ...changes,
  }
  value.digest = sha256(canonicalJson(value))
  return value
}

function harness(hooks = {}) {
  const config = expected(); let now = Date.parse('2026-07-14T18:00:00.000Z'); let calls = 0; let locked = false; let cleaned = false
  const host = {
    sample: async (_expected, phase) => { calls += 1; return sample(config, now + (calls - 1) * 5000, hooks.samples?.[phase] || {}) },
    acquireTransitionLock: async () => { if (hooks.locked) return null; locked = true; return { id: 1 } },
    releaseTransitionLock: async () => { locked = false },
    activate: async () => { if (hooks.startFailure) throw Object.assign(new Error('start'), { code: 'START_FAILED' }) },
    verifyActivated: async () => hooks.health || ({ ready: true, identityMatched: true, manifestMatched: true, routingEligible: true, activeProviderOperationCount: 0, unsettledJobCount: 0, runningExecutableHash: config.staged.portable.sha256, runningManifestHash: config.staged.manifest.sha256 }),
    rollback: async () => hooks.rollback || ({ passed: true, version: config.retained.version, sha256: config.retained.sha256, routingEligible: true, activeProviderOperationCount: 0, unsettledJobCount: 0 }),
    cleanup: async () => { cleaned = true },
  }
  let issued = null; let consumed = false
  const authority = {
    sample: async (hostEvidenceDigest) => ({ id: `sample-${calls}`, actionKey: COLD_ACTIVATION_ACTION, hostEvidenceDigest, evidenceDigest: sha256(`authority:${hostEvidenceDigest}`), observedAt: new Date(now).toISOString(), activeProviderOperationCount: hooks.authorityWork || 0, unsettledJobCount: 0 }),
    issue: async (binding) => {
      issued = binding
      return { id: 'auth-1', actionKey: hooks.wrongAction || COLD_ACTIVATION_ACTION, bindingDigest: hooks.wrongBinding || sha256(canonicalJson(binding)), issuedAt: new Date(now).toISOString(), expiresAt: new Date(now + (hooks.expiryMs ?? 60000)).toISOString() }
    },
    consume: async (_id, bindingDigest) => {
      if (hooks.consumeError) throw Object.assign(new Error('consume'), { code: hooks.consumeError })
      if (consumed) return { consumed: false, replayed: true }
      consumed = true
      return { consumed: true, replayed: false, actionKey: COLD_ACTIVATION_ACTION, bindingDigest, consumedAt: new Date(now + 10000).toISOString() }
    },
  }
  const engine = createColdActivationEngine({ host, authority, clock: () => now, wait: async (ms) => { now += ms } })
  return { config, engine, state: () => ({ locked, cleaned, issued, consumed }) }
}

test('valid stopped legacy cold activation passes and cleans ownership', async () => {
  const h = harness(); const result = await h.engine.execute(h.config)
  assert.equal(result.disposition, 'activation-passed'); assert.equal(result.rollback, 'no-rollback-required')
  assert.deepEqual(h.state(), { locked: false, cleaned: true, issued: h.state().issued, consumed: true })
})

for (const [name, phase, change, code] of [
  ['process between samples', 'second', { processCount: 1 }, 'COLD_PROCESS_PRESENT'],
  ['listener between samples', 'second', { listenerCount: 1 }, 'COLD_LISTENER_PRESENT'],
  ['process after authorization', 'final', { processCount: 1 }, 'COLD_PROCESS_PRESENT'],
  ['listener after authorization', 'final', { listenerCount: 1 }, 'COLD_LISTENER_PRESENT'],
  ['active operation before mutation', 'final', { activeProviderOperationCount: 1 }, 'COLD_PROVIDER_WORK_ACTIVE'],
  ['unsettled job before mutation', 'final', { unsettledJobCount: 1 }, 'COLD_JOB_UNSETTLED'],
  ['retained hash mismatch', 'first', { retained: { sizeBytes: 10, sha256: '9'.repeat(64) } }, 'COLD_ARTIFACT_MISMATCH'],
  ['staged artifact mismatch', 'second', { staged: { portable: { sizeBytes: 20, sha256: '9'.repeat(64) }, installer: { sizeBytes: 30, sha256: 'c'.repeat(64) }, manifest: { sizeBytes: 40, sha256: 'd'.repeat(64) } } }, 'COLD_ARTIFACT_MISMATCH'],
  ['manifest mismatch', 'final', { staged: { portable: { sizeBytes: 20, sha256: 'b'.repeat(64) }, installer: { sizeBytes: 30, sha256: 'c'.repeat(64) }, manifest: { sizeBytes: 40, sha256: '9'.repeat(64) } } }, 'COLD_ARTIFACT_MISMATCH'],
  ['live instance cannot use cold path', 'first', { processCount: 1, listenerCount: 1 }, 'COLD_PROCESS_PRESENT'],
]) test(name, async () => { const h = harness({ samples: { [phase]: change } }); await assert.rejects(() => h.engine.execute(h.config), { code }); assert.equal(h.state().consumed, false); assert.equal(h.state().cleaned, true) })

test('misleading stale discovery is evidence only and does not establish target identity', async () => {
  const h = harness({ samples: { first: { staleDiscovery: { present: true, sizeBytes: 99, sha256: '3'.repeat(64), lastWriteUtc: '2026-07-12T08:02:06.228Z', claimedVersion: '26.7.6' } } } })
  assert.equal((await h.engine.execute(h.config)).disposition, 'activation-passed')
})

test('expired, wrong-bound, replayed, and missing-permission authorizations fail closed', async () => {
  for (const [hooks, code] of [[{ expiryMs: 0 }, 'COLD_AUTHORIZATION_EXPIRED'], [{ wrongBinding: '9'.repeat(64) }, 'COLD_AUTHORIZATION_MISMATCH'], [{ wrongAction: 'application:shutdown' }, 'COLD_AUTHORIZATION_MISMATCH'], [{ consumeError: 'REPLAY_REJECTED' }, 'REPLAY_REJECTED']]) {
    const h = harness(hooks); await assert.rejects(() => h.engine.execute(h.config), { code }); assert.equal(h.state().cleaned, true)
  }
})

test('transition lock contention fails before consumption', async () => {
  const h = harness({ locked: true }); await assert.rejects(() => h.engine.execute(h.config), { code: 'COLD_TRANSITION_LOCKED' }); assert.equal(h.state().consumed, false)
})

test('start and post-health failures roll back successfully', async () => {
  for (const hooks of [{ startFailure: true }, { health: { ready: false } }]) {
    const h = harness(hooks); await assert.rejects(() => h.engine.execute(h.config), (error) => error.details.rollback === 'rollback-required-passed')
  }
})

test('rollback failure is a hard failure and all ownership is released', async () => {
  const h = harness({ startFailure: true, rollback: { passed: false } })
  await assert.rejects(() => h.engine.execute(h.config), { code: 'COLD_ROLLBACK_HARD_FAILURE' })
  assert.equal(h.state().locked, false); assert.equal(h.state().cleaned, true)
})

test('foreground operator keeps both credentials off arguments, environment, logs, hashes, and evidence', () => {
  const operator = fs.readFileSync(new URL('./Invoke-Phase5HColdActivation.ps1', import.meta.url), 'utf8')
  const runner = fs.readFileSync(new URL('./phase5h-cold-activation-windows.mjs', import.meta.url), 'utf8')
  assert.match(operator, /venice-media-local:activate-release-slot/)
  assert.match(operator, /RedirectStandardInput=\$true/)
  assert.match(operator, /StandardInput\.WriteLine\(\$transport\)/)
  assert.match(operator, /agent-control-token\.venice-media-local/)
  assert.doesNotMatch(operator, /Arguments=.*(?:authorization|agentCredential)/i)
  assert.doesNotMatch(operator, /SetEnvironmentVariable\([^\n]*(?:authorization|agentCredential)/i)
  assert.doesNotMatch(operator, /Write-(?:Host|Output)[^\n]*(?:authorization|agentCredential)/i)
  assert.doesNotMatch(operator, /Get-FileHash[^\n]*(?:authorization|agentCredential)/i)
  assert.match(runner, /configDigest: sha256\(canonicalJson\(config\)\)/)
  assert.doesNotMatch(runner, /JSON\.stringify\([^\n]*(?:coreAuthorization|agentControlCredential)[^\n]*report/i)
})

test('authorization binding projects only canonical Core fields and lowercase hashes', () => {
  const config = expected(); config.retained.sha256 = config.retained.sha256.toUpperCase(); config.retained.providerId = 'rollback-only'; config.staged.portable.sha256 = config.staged.portable.sha256.toUpperCase()
  const firstHost = sample(config, Date.parse('2026-07-14T18:00:00.000Z')); const secondHost = sample(config, Date.parse('2026-07-14T18:00:05.000Z'))
  const first = { observedAt: firstHost.observedAt, evidenceDigest: '1'.repeat(64), activeProviderOperationCount: 0, unsettledJobCount: 0 }
  const second = { observedAt: secondHost.observedAt, evidenceDigest: '2'.repeat(64), activeProviderOperationCount: 0, unsettledJobCount: 0 }
  const binding = authorizationBinding(config, firstHost, secondHost, first, second)
  assert.deepEqual(Object.keys(binding.retained).sort(), ['path', 'sha256', 'sizeBytes', 'version'])
  assert.equal(binding.retained.sha256, config.retained.sha256.toLowerCase())
  assert.equal(binding.staged.portable.sha256, config.staged.portable.sha256.toLowerCase())
})
