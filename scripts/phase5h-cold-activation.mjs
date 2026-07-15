import { createHash } from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'

export const COLD_ACTIVATION_ACTION = 'venice-media-local:activate-release-slot'
export const COLD_ACTIVATION_REASON = 'phase5h-release-slot-transition'
export const LEGACY_PRE_LEDGER_RETAINED = Object.freeze({
  version: '26.6.5',
  sha256: 'b46d8ee942020ec6fea81db298f4d83e32975177838e2cc0abd24da76001e64d',
  sizeBytes: 15109632,
})
export const LOCAL_WORK_MODE_LEDGER = 'provider-v2-ledger'
export const LOCAL_WORK_MODE_LEGACY_PRE_LEDGER = 'legacy-pre-ledger-26.6.5'
const HASH = /^[a-f0-9]{64}$/i
const TERMINAL = new Set(['succeeded', 'failed', 'canceled', 'lost', 'reconciliation_required'])
const APP_DATA_STATE_FILES = Object.freeze([
  'settings.json',
  'venice-models.json',
  'control-api.json',
  'capability-provider-instance-id',
])
const APP_DATA_ALLOWED_TOP_LEVEL = new Set([...APP_DATA_STATE_FILES, 'outputs'])
const FORBIDDEN_WORK_RELATIVE = Object.freeze([
  'provider-v1',
  'provider-v2',
  'provider-v2-execution',
])

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
function isLegacyPreLedgerRetained(retained) {
  return retained
    && retained.version === LEGACY_PRE_LEDGER_RETAINED.version
    && Number(retained.sizeBytes) === LEGACY_PRE_LEDGER_RETAINED.sizeBytes
    && typeof retained.sha256 === 'string'
    && retained.sha256.toLowerCase() === LEGACY_PRE_LEDGER_RETAINED.sha256
}

function assertNoForbiddenWorkPaths(appDataRoot) {
  for (const relative of FORBIDDEN_WORK_RELATIVE) {
    const absolute = path.join(appDataRoot, ...relative.split('/'))
    if (fs.existsSync(absolute)) fail(`Unexpected work-state path present: ${relative}`, 'COLD_UNEXPECTED_WORK_STATE', { relative })
  }
}

function assertAppDataTopLevel(appDataRoot, { allowProviderV2 = false } = {}) {
  const allowed = new Set(APP_DATA_ALLOWED_TOP_LEVEL)
  if (allowProviderV2) allowed.add('provider-v2')
  const entries = fs.readdirSync(appDataRoot, { withFileTypes: true })
  for (const entry of entries) {
    if (entry.isSymbolicLink?.() || fs.lstatSync(path.join(appDataRoot, entry.name)).isSymbolicLink()) {
      fail(`Linked app-data entry is forbidden: ${entry.name}`, 'COLD_UNEXPECTED_WORK_STATE')
    }
    if (!allowed.has(entry.name)) {
      fail(`Unexpected app-data entry: ${entry.name}`, 'COLD_UNEXPECTED_WORK_STATE', { name: entry.name })
    }
  }
}

function inventoryAppData(appDataRoot, ledgerPresent) {
  const files = []
  for (const relative of APP_DATA_STATE_FILES) {
    const absolute = path.join(appDataRoot, ...relative.split('/'))
    if (!fs.existsSync(absolute)) {
      files.push({ path: relative, present: false })
      continue
    }
    const stat = fs.lstatSync(absolute)
    if (!stat.isFile() || stat.isSymbolicLink()) fail(`App-data state path must be a regular file: ${relative}`, 'COLD_UNEXPECTED_WORK_STATE')
    files.push({ path: relative, present: true, sizeBytes: stat.size, sha256: sha256(fs.readFileSync(absolute)) })
  }
  files.push({ path: 'provider-v2/ledger.json', present: ledgerPresent === true })
  const inventory = { schemaVersion: 1, files: files.sort((a, b) => a.path.localeCompare(b.path)) }
  return { inventory, inventoryDigest: sha256(canonicalJson(inventory)) }
}

function readLedgerWork(ledgerPath) {
  const absolute = path.resolve(ledgerPath)
  if (!fs.existsSync(absolute)) return null
  const stat = fs.lstatSync(absolute)
  if (!stat.isFile() || stat.isSymbolicLink()) fail('Provider ledger must be a regular unlinked file', 'COLD_LEDGER_INVALID')
  let value
  try { value = JSON.parse(fs.readFileSync(absolute, 'utf8')) } catch { fail('Provider ledger is malformed JSON', 'COLD_LEDGER_INVALID') }
  if (!value || Array.isArray(value) || typeof value !== 'object' || value.operations == null || typeof value.operations !== 'object' || Array.isArray(value.operations)) {
    fail('Provider ledger schema is invalid', 'COLD_LEDGER_INVALID')
  }
  const operations = Object.values(value.operations)
  const activeProviderOperationCount = operations.filter((entry) => !TERMINAL.has(String(entry?.state || '').toLowerCase())).length
  const unsettledJobCount = operations.filter((entry) => ['lost', 'submitted_ambiguous'].includes(String(entry?.state || entry?.submissionCertainty || '').toLowerCase())).length
  if (activeProviderOperationCount !== 0) fail('Provider work is active', 'COLD_PROVIDER_WORK_ACTIVE')
  if (unsettledJobCount !== 0) fail('Provider work is unsettled', 'COLD_JOB_UNSETTLED')
  return {
    mode: LOCAL_WORK_MODE_LEDGER,
    ledgerPresent: true,
    activeProviderOperationCount: 0,
    unsettledJobCount: 0,
    evidenceDigest: sha256(fs.readFileSync(absolute)),
    appDataInventoryDigest: null,
  }
}

export function evaluateLocalWorkState({ appDataRoot, providerLedgerPath, retained }) {
  if (!appDataRoot || !providerLedgerPath || !retained) fail('Local work evaluation requires app data, ledger path, and retained identity', 'COLD_BINDING_INVALID')
  const root = path.resolve(appDataRoot)
  if (!fs.existsSync(root) || !fs.lstatSync(root).isDirectory() || fs.lstatSync(root).isSymbolicLink()) {
    fail('App-data root must be a real directory', 'COLD_UNEXPECTED_WORK_STATE')
  }
  const ledgerPath = path.resolve(providerLedgerPath)
  const ledgerExists = fs.existsSync(ledgerPath)
  assertAppDataTopLevel(root, { allowProviderV2: ledgerExists })
  const ledger = readLedgerWork(ledgerPath)
  if (ledger) {
    if (isLegacyPreLedgerRetained(retained)) fail('Exact pre-ledger 26.6.5 retained package cannot present a provider-v2 ledger', 'COLD_LEGACY_LEDGER_CONFLICT')
    const inventory = inventoryAppData(root, true)
    return { ...ledger, appDataInventoryDigest: inventory.inventoryDigest, inventory: inventory.inventory }
  }
  assertNoForbiddenWorkPaths(root)
  if (!isLegacyPreLedgerRetained(retained)) fail('Provider-v2 ledger is required for this retained package', 'COLD_LEDGER_REQUIRED')
  const inventory = inventoryAppData(root, false)
  if (!inventory.inventory.files.some((entry) => entry.path === 'capability-provider-instance-id' && entry.present === true)) {
    fail('Legacy pre-ledger installation is missing instance identity', 'COLD_LEGACY_IDENTITY_MISSING')
  }
  return {
    mode: LOCAL_WORK_MODE_LEGACY_PRE_LEDGER,
    ledgerPresent: false,
    activeProviderOperationCount: 0,
    unsettledJobCount: 0,
    evidenceDigest: inventory.inventoryDigest,
    appDataInventoryDigest: inventory.inventoryDigest,
    inventory: inventory.inventory,
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
  if (sample.localWorkMode === LOCAL_WORK_MODE_LEGACY_PRE_LEDGER) {
    if (!isLegacyPreLedgerRetained(expected.retained)) fail('Legacy pre-ledger mode is bound only to exact retained 26.6.5', 'COLD_LEGACY_BINDING_INVALID')
    if (sample.ledgerPresent !== false) fail('Legacy pre-ledger mode requires ledger absence', 'COLD_LEGACY_LEDGER_CONFLICT')
    if (typeof sample.appDataInventoryDigest !== 'string' || !HASH.test(sample.appDataInventoryDigest)) fail('Legacy app-data inventory digest is invalid', 'COLD_LEGACY_INVENTORY_INVALID')
    if (sample.persistedWorkDigest !== sample.appDataInventoryDigest) fail('Legacy work digest must equal inventory digest', 'COLD_LEGACY_INVENTORY_INVALID')
  } else if (sample.localWorkMode === LOCAL_WORK_MODE_LEDGER) {
    if (sample.ledgerPresent !== true) fail('Ledger mode requires a present provider-v2 ledger', 'COLD_LEDGER_REQUIRED')
    if (isLegacyPreLedgerRetained(expected.retained)) fail('Exact pre-ledger 26.6.5 retained package cannot use ledger mode', 'COLD_LEGACY_LEDGER_CONFLICT')
  } else {
    fail('Local work mode is invalid', 'COLD_LOCAL_WORK_MODE_INVALID')
  }
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
    localWork: {
      mode: second.localWorkMode,
      ledgerPresent: second.ledgerPresent === true,
      appDataInventoryDigest: second.appDataInventoryDigest || null,
      activeProviderOperationCount: second.activeProviderOperationCount,
      unsettledJobCount: second.unsettledJobCount,
      evidenceDigest: second.persistedWorkDigest,
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
        if (second.localWorkMode !== first.localWorkMode || second.appDataInventoryDigest !== first.appDataInventoryDigest || second.ledgerPresent !== first.ledgerPresent || second.persistedWorkDigest !== first.persistedWorkDigest) {
          fail('Local work evidence changed between samples', 'COLD_LOCAL_WORK_CHANGED')
        }
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
        if (final.localWorkMode !== second.localWorkMode || final.appDataInventoryDigest !== second.appDataInventoryDigest || final.ledgerPresent !== second.ledgerPresent || final.persistedWorkDigest !== second.persistedWorkDigest) {
          fail('Local work evidence changed before mutation', 'COLD_LOCAL_WORK_CHANGED')
        }
        if (Date.parse(authorization.expiresAt) <= clock()) fail('Server authorization expired before mutation', 'COLD_AUTHORIZATION_EXPIRED')
        const consumed = await authority.consume(authorization.id, authorization.bindingDigest)
        if (!consumed || consumed.consumed !== true || consumed.replayed !== false || consumed.bindingDigest !== authorization.bindingDigest || consumed.actionKey !== COLD_ACTIVATION_ACTION) fail('Server authorization consumption failed', 'COLD_AUTHORIZATION_CONSUME_FAILED')
        evidence.finalSample = final
        evidence.consumedAt = consumed.consumedAt
        mutationStarted = true
        evidence.mutationStarted = true
        await host.activate(expected, { authorizationId: authorization.id, bindingDigest: authorization.bindingDigest, preActivationLocalWork: { mode: final.localWorkMode, ledgerPresent: final.ledgerPresent } })
        const health = await host.verifyActivated(expected)
        if (!health || health.ready !== true || health.identityMatched !== true || health.manifestMatched !== true || health.routingEligible !== true || health.activeProviderOperationCount !== 0 || health.unsettledJobCount !== 0 || health.runningExecutableHash?.toLowerCase() !== expected.staged.portable.sha256.toLowerCase() || health.runningManifestHash?.toLowerCase() !== expected.staged.manifest.sha256.toLowerCase() || health.ledgerPresent !== true || health.localLedgerActiveCount !== 0 || health.localLedgerUnsettledCount !== 0) {
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
            if (!restored || restored.passed !== true || restored.version !== expected.retained.version || restored.sha256?.toLowerCase() !== expected.retained.sha256.toLowerCase() || restored.routingEligible !== true || restored.activeProviderOperationCount !== 0 || restored.unsettledJobCount !== 0) {
              fail('Rollback verification failed', 'COLD_ROLLBACK_FAILED')
            }
            if (isLegacyPreLedgerRetained(expected.retained) && restored.ledgerPresent !== false) {
              fail('Rollback left a provider-v2 ledger behind on pre-ledger legacy package', 'COLD_ROLLBACK_FAILED')
            }
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
