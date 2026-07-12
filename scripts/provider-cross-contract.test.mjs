import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createRequire } from 'node:module'
import test from 'node:test'

const root = new URL('../', import.meta.url)
const manifest = JSON.parse(fs.readFileSync(new URL('src-tauri/src/capability-manifest.v1.json', root), 'utf8'))
const require = createRequire(import.meta.url)
const canonical = require('../../eva-core/server/capability-provider-protocol.js')
const wire = JSON.parse(fs.readFileSync(new URL('../eva-core/docs/venice-media-operation-v1-wire-fixture.json', root), 'utf8'))

function runtimeManifest() {
  return {
    ...manifest,
    provider: { ...manifest.provider, instanceId: 'vml-0123456789abcdef', machineId: 'ripper-windows', version: '26.7.6' },
    transport: { ...manifest.transport, baseUrl: 'http://100.64.0.10:9876', manifestUrl: 'http://100.64.0.10:9876/api/v1/capabilities' },
  }
}

test('revision-2 Venice manifest normalizes through the canonical Core protocol', () => {
  const normalized = canonical.normalizeManifest(runtimeManifest())
  assert.equal(normalized.schemaVersion, canonical.SCHEMA_VERSION)
  assert.equal(normalized.capabilities.length, manifest.capabilities.length)
  for (const capability of normalized.capabilities) {
    assert.equal(capability.revision, '2')
    assert.equal(capability.invocation.envelope, 'veniceMediaOperation.v1')
    assert.equal(capability.invocation.path, '/api/v1/operations')
    assert.deepEqual(Object.keys(capability.progress).sort(), ['eventReplayPath', 'mode', 'pollFallbackPath'])
    assert.equal(capability.cancellation.supported, true)
    assert.deepEqual(capability.cancellation.scope, ['pre_submission'])
  }
})

test('Core and Venice share the exact operation envelope and operation-bound grant route', () => {
  assert.equal(wire.type, 'veniceMediaOperation.v1')
  assert.equal(wire.submit.path, '/api/v1/operations')
  assert.equal(wire.grantRegistration.path, '/api/v1/operations/{providerOperationId}/transfer-grants')
  assert.equal(wire.digestAlgorithm, 'sha256(canonical-json({input,inputArtifacts}))')
})

test('corrected schemas reject controls and inline media that revision 2 does not accept', () => {
  const normalized = canonical.normalizeManifest(runtimeManifest())
  const byId = new Map(normalized.capabilities.map((capability) => [capability.id, capability]))
  assert.throws(() => canonical.validateCapabilityInput(byId.get('media.audio.music.generate'), { model: 'm', prompt: 'p', negativePrompt: 'not-supported' }))
  assert.throws(() => canonical.validateCapabilityInput(byId.get('media.image.edit'), { model: 'm', prompt: 'p', images: ['inline'] }))
  assert.throws(() => canonical.validateCapabilityInput(byId.get('media.video.generate'), { model: 'm', prompt: 'p', durationSeconds: 4 }))
  assert.doesNotThrow(() => canonical.validateCapabilityInput(byId.get('media.video.generate'), { model: 'm', prompt: 'p', duration: '4' }))
})
