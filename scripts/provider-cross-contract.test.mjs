import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createRequire } from 'node:module'
import test from 'node:test'

const root = new URL('../', import.meta.url)
const source = fs.readFileSync(new URL('src-tauri/src/main.rs', root), 'utf8').replaceAll('\r\n', '\n')
const manifest = JSON.parse(fs.readFileSync(new URL('src-tauri/src/capability-manifest.v1.json', root), 'utf8'))
const require = createRequire(import.meta.url)
const canonical = require('../../eva-core/server/capability-provider-protocol.js')

function runtimeManifest() {
  return {
    ...manifest,
    provider: {
      ...manifest.provider,
      instanceId: 'vml-0123456789abcdef',
      machineId: 'ripper-windows',
      version: '26.7.6',
    },
    transport: {
      ...manifest.transport,
      baseUrl: 'http://100.64.0.10:9876',
      manifestUrl: 'http://100.64.0.10:9876/api/v1/capabilities',
    },
  }
}

test('embedded Venice manifest normalizes through eva-core canonical protocol', () => {
  const normalized = canonical.normalizeManifest(runtimeManifest())

  assert.equal(normalized.schemaVersion, canonical.SCHEMA_VERSION)
  assert.equal(normalized.provider.kind, 'media')
  assert.equal(normalized.transport.type, 'http')
  assert.equal(normalized.health.mode, 'poll')
  assert.equal(normalized.health.path, '/api/v1/health')
  assert.deepEqual(
    normalized.capabilities.map(({ id }) => id),
    manifest.capabilities.map(({ id }) => id),
  )

  const vocabulary = new Set(canonical.CAPABILITY_VOCABULARY)
  for (const capability of normalized.capabilities) {
    assert.ok(vocabulary.has(capability.id), capability.id)
    assert.deepEqual(Object.keys(capability.progress).sort(),
      capability.progress.mode === 'poll' ? ['intervalMs', 'mode', 'path'] : ['mode'])
    assert.deepEqual(capability.cancellation, { supported: false })
    for (const artifact of capability.artifacts) {
      assert.deepEqual(Object.keys(artifact).sort(), ['deliveryModes', 'kinds', 'mimeTypes', 'role'])
    }
  }
})

test('every canonical invocation and poll path is an exact existing route', () => {
  const routes = new Map(
    [...source.matchAll(/\.route\("([^"]+)",\s*(get|post)\(/g)]
      .map(([, path, method]) => [path, method.toUpperCase()]),
  )

  for (const capability of manifest.capabilities) {
    assert.equal(routes.get(capability.invocation.path), capability.invocation.method, capability.id)
    if (capability.progress.mode === 'poll') {
      assert.equal(capability.invocation.poll.path, capability.progress.path, capability.id)
      assert.equal(routes.get(capability.progress.path), capability.invocation.poll.method, `${capability.id} poll`)
      assert.equal(capability.invocation.poll.operationIdField, 'queueId', capability.id)
    } else {
      assert.equal(capability.invocation.poll, undefined, capability.id)
    }
  }
})
