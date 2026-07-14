import { createHash } from 'node:crypto'

export const COLD_ACTIVATION_ACTION = 'venice-media-local:activate-release-slot'
export const COLD_ACTIVATION_REASON = 'phase5h-release-slot-transition'
const HASH = /^[a-f0-9]{64}$/i

export class ColdActivationError extends Error {
  constructor(message, code = 'COLD_ACTIVATION_FAILED', details = {}) {
    super(message)
    this.code = code
    this.details = details
  }
}

function fail(message, code, details) { throw new ColdActivationError(message, code, details) }
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical)
  if (!value || typeof value !== 'object') return value
  return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]))
}
export function canonicalJson(value) { return JSON.stringify(canonical(value)) }
export function sha256(value) { return createHash('sha256').update(value).digest('hex') }
function normalizedHash(value, label) {
  if (typeof value !== 'string' || !HASH.test(value)) fail(`${label} is not SHA-256`, 'COLD_BINDING_INVALID')
  return value.toLowerCase()
}
function sameArtifact(actual, expected, label) {
  if (!actual || actual.sizeBytes !== expected.sizeBytes || normalizedHash(actual.sha256, `${label}.sha256`) !== normalizedHash(expected.sha256, `${label}.sha256`)) {
    fail(`${label} identity changed`, 'COLD_ARTIFACT_MISMATCH', { label })
  }
}

function assertColdSnapshot(sample, expected, label) {
  if (!sample || typeof sample !== 'object') fail(`${label} is missing`, 'COLD_SAMPLE_MISSING')
  if (sample.processCount !== 0) fail('Production process exists; authenticated shutdown is required', 'COLD_PROCESS_PRESENT')
  if (sample.listenerCount !== 0) fail('Production listener exists; authenticated shutdown is required', 'COLD_LISTENER_PRESENT')
  if (sample.activeProviderOperationCount !== 0) fail('Provider work is active', 'COLD_PROVIDER_WORK_ACTIVE')
  if (sample.unsettledJobCount !== 0) fail('Provider work is unsettled', 'COLD_JOB_UNSETTLED')
  if (sample.transitionInProgress === true) fail('Another release transition is in progress', 'COLD_TRANSITION_IN_PROGRESS')
  if (sample.staleDiscovery?.present !== true) fail('Stale discovery evidence is required', 'COLD_DISCOVERY_EVIDENCE_MISSING')
  sameArtifact(sample.retained, expected.retained, 'retained')
  for (const key of ['portable', 'installer', 'manifest']) sameArtifact(sample.staged?.[key], expected.staged[key], `staged.${key}`)
  if (sample.expectedProcessName !== expected.expectedHost.processName || sample.expectedPort !== expected.expectedHost.port) fail('Host binding changed', 'COLD_HOST_BINDING_MISMATCH')
  if (!Number.isFinite(Date.parse(sample.observedAt))) fail('Sample time is invalid', 'COLD_SAMPLE_TIME_INVALID')
  const proof = { ...sample }
  delete proof.digest
  const calculated = sha256(canonicalJson(proof))
  if (sample.digest !== calculated) fail('Cold sample digest is invalid', 'COLD_SAMPLE_DIGEST_INVALID')
  return sample
}

export function authorizationBinding(expected, first, second, firstAuthority, secondAuthority) {
  const retained = { path: expected.retained.path, version: expected.retained.version, sizeBytes: expected.retained.sizeBytes, sha256: normalizedHash(expected.retained.sha256, 'retained.sha256') }
  const staged = { slot: expected.staged.slot }
  for (const key of ['portable', 'installer', 'manifest']) staged[key] = { filename: expected.staged[key].filename, sizeBytes: expected.staged[key].sizeBytes, sha256: normalizedHash(expected.staged[key].sha256, `staged.${key}.sha256`) }
  const replacement = { ...expected.replacement, sourceCommit: expected.replacement.sourceCommit.toLowerCase(), manifestDigest: normalizedHash(expected.replacement.manifestDigest, 'replacement.manifestDigest') }
  return {
    schemaVersion: '1.0', operationType: 'cold-activation', reason: COLD_ACTIVATION_REASON,
    retained,
    staged,
    expectedHost: expected.expectedHost,
    persistedWork: {
      observedAt: secondAuthority.observedAt,
      activeProviderOperationCount: secondAuthority.activeProviderOperationCount,
      unsettledJobCount: secondAuthority.unsettledJobCount,
      evidenceDigest: secondAuthority.evidenceDigest,
    },
    coldSamples: [{ observedAt: firstAuthority.observedAt, digest: firstAuthority.evidenceDigest }, { observedAt: secondAuthority.observedAt, digest: secondAuthority.evidenceDigest }],
    staleDiscovery: second.staleDiscovery,
    replacement,
    validitySeconds: Math.min(60, expected.validitySeconds ?? 60),
  }
}

export function createColdActivationEngine({ host, authority, clock = () => Date.now(), wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms)) } = {}) {
  if (!host || !authority) throw new Error('host and authority are required')
  return {
    async execute(expected) {
      let lock = null
      let mutationStarted = false
      let rollback = 'no-mutation-rollback-unnecessary'
      let authorization = null
      const evidence = { schemaVersion: 1, action: COLD_ACTIVATION_ACTION, reason: COLD_ACTIVATION_REASON, samples: [], mutationStarted: false }
      try {
        const first = assertColdSnapshot(await host.sample(expected, 'first'), expected, 'first sample')
        const firstAuthority = await authority.sample(first.digest)
        if (firstAuthority?.actionKey !== COLD_ACTIVATION_ACTION || firstAuthority.hostEvidenceDigest !== first.digest || firstAuthority.activeProviderOperationCount !== 0 || firstAuthority.unsettledJobCount !== 0) fail('First server-held cold-state sample is invalid', 'COLD_AUTHORITY_SAMPLE_INVALID')
        evidence.samples.push({ host: first, authority: firstAuthority })
        await wait(5000)
        const second = assertColdSnapshot(await host.sample(expected, 'second'), expected, 'second sample')
        if (Date.parse(second.observedAt) - Date.parse(first.observedAt) < 5000) fail('Cold samples are less than five seconds apart', 'COLD_SAMPLES_TOO_CLOSE')
        const secondAuthority = await authority.sample(second.digest)
        if (secondAuthority?.actionKey !== COLD_ACTIVATION_ACTION || secondAuthority.hostEvidenceDigest !== second.digest || secondAuthority.activeProviderOperationCount !== 0 || secondAuthority.unsettledJobCount !== 0 || Date.parse(secondAuthority.observedAt) - Date.parse(firstAuthority.observedAt) < 5000) fail('Second server-held cold-state sample is invalid', 'COLD_AUTHORITY_SAMPLE_INVALID')
        evidence.samples.push({ host: second, authority: secondAuthority })
        const binding = authorizationBinding(expected, first, second, firstAuthority, secondAuthority)
        authorization = await authority.issue(binding)
        if (!authorization || authorization.actionKey !== COLD_ACTIVATION_ACTION || authorization.bindingDigest !== sha256(canonicalJson(binding))) fail('Server authorization binding mismatch', 'COLD_AUTHORIZATION_MISMATCH')
        if (Date.parse(authorization.expiresAt) <= clock() || Date.parse(authorization.expiresAt) - Date.parse(authorization.issuedAt) > 60000) fail('Server authorization freshness is invalid', 'COLD_AUTHORIZATION_EXPIRED')
        evidence.authorizationId = authorization.id
        evidence.authorizationBindingDigest = authorization.bindingDigest
        lock = await host.acquireTransitionLock(expected)
        if (!lock) fail('Transition lock is unavailable', 'COLD_TRANSITION_LOCKED')
        const final = assertColdSnapshot(await host.sample(expected, 'final'), expected, 'final sample')
        if (Date.parse(authorization.expiresAt) <= clock()) fail('Server authorization expired before mutation', 'COLD_AUTHORIZATION_EXPIRED')
        const consumed = await authority.consume(authorization.id, authorization.bindingDigest)
        if (!consumed || consumed.consumed !== true || consumed.replayed !== false || consumed.bindingDigest !== authorization.bindingDigest || consumed.actionKey !== COLD_ACTIVATION_ACTION) fail('Server authorization consumption failed', 'COLD_AUTHORIZATION_CONSUME_FAILED')
        evidence.finalSample = final
        evidence.consumedAt = consumed.consumedAt
        mutationStarted = true
        evidence.mutationStarted = true
        await host.activate(expected, { authorizationId: authorization.id, bindingDigest: authorization.bindingDigest })
        const health = await host.verifyActivated(expected)
        if (!health || health.ready !== true || health.identityMatched !== true || health.manifestMatched !== true || health.routingEligible !== true || health.activeProviderOperationCount !== 0 || health.unsettledJobCount !== 0 || health.runningExecutableHash?.toLowerCase() !== expected.staged.portable.sha256.toLowerCase() || health.runningManifestHash?.toLowerCase() !== expected.staged.manifest.sha256.toLowerCase()) {
          fail('Activated release failed authenticated post-activation gates', 'COLD_POST_ACTIVATION_INVALID')
        }
        rollback = 'no-rollback-required'
        evidence.postActivation = health
        evidence.disposition = 'activation-passed'
        return { disposition: 'activation-passed', rollback, evidence }
      } catch (error) {
        if (mutationStarted) {
          try {
            const restored = await host.rollback(expected)
            if (!restored || restored.passed !== true || restored.version !== expected.retained.version || restored.sha256?.toLowerCase() !== expected.retained.sha256.toLowerCase() || restored.routingEligible !== true || restored.activeProviderOperationCount !== 0 || restored.unsettledJobCount !== 0) fail('Rollback verification failed', 'COLD_ROLLBACK_FAILED')
            rollback = 'rollback-required-passed'
          } catch (rollbackError) {
            rollback = 'rollback-required-failed'
            throw new ColdActivationError('Activation failed and rollback failed', 'COLD_ROLLBACK_HARD_FAILURE', { activationCode: error.code || 'UNKNOWN', rollbackCode: rollbackError.code || 'UNKNOWN', rollback })
          }
        }
        error.details = { ...(error.details || {}), rollback }
        throw error
      } finally {
        if (lock) await host.releaseTransitionLock(lock)
        await host.cleanup?.()
      }
    },
  }
}
