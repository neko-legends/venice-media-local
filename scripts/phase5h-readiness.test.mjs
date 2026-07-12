import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import test from 'node:test'
import { activateReleaseSlot, backupProviderState, restoreProviderState } from './phase5h-readiness.mjs'

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'vml-5h-'))
  const source = path.join(root, 'synthetic-app-data')
  fs.mkdirSync(path.join(source, 'provider-v1'), { recursive: true })
  fs.mkdirSync(path.join(source, 'provider-v2', 'artifacts'), { recursive: true })
  fs.mkdirSync(path.join(source, 'provider-v2', 'uploads'), { recursive: true })
  fs.mkdirSync(path.join(source, 'provider-v2-execution'), { recursive: true })
  fs.writeFileSync(path.join(source, 'settings.json'), JSON.stringify({ theme: 'test', agentControlToken: null, harmlessFutureField: 'dropped' }))
  fs.writeFileSync(path.join(source, 'venice-models.json'), '{}')
  fs.writeFileSync(path.join(source, 'control-api.json'), '{}')
  fs.writeFileSync(path.join(source, 'capability-provider-instance-id'), 'synthetic-instance\n')
  fs.writeFileSync(path.join(source, 'provider-v1', 'lifecycle.json'), '{}')
  fs.writeFileSync(path.join(source, 'provider-v2', 'ledger.json'), 'opaque-ledger')
  fs.writeFileSync(path.join(source, 'provider-v2', 'artifacts', 'artifact.bin'), 'artifact')
  fs.writeFileSync(path.join(source, 'provider-v2', 'uploads', 'upload.bin'), 'upload')
  fs.writeFileSync(path.join(source, 'provider-v2-execution', 'denied.bin'), 'denied')
  fs.writeFileSync(path.join(source, '.env'), 'SYNTHETIC_SECRET=must-not-copy')
  fs.writeFileSync(path.join(source, 'unexpected.txt'), 'unexpected')
  return { root, source, backup: path.join(root, 'backup'), restore: path.join(root, 'restore') }
}

test('backup allowlists state, excludes denied and unexpected files, and restores exact hashes', () => {
  const data = fixture()
  const inventory = backupProviderState(data.source, data.backup)
  assert.deepEqual(inventory.map((entry) => entry.path), [
    'capability-provider-instance-id', 'control-api.json', 'provider-v1/lifecycle.json',
    'provider-v2/ledger.json', 'settings.json', 'venice-models.json',
  ])
  const text = fs.readFileSync(path.join(data.backup, 'settings.json'), 'utf8')
  assert.deepEqual(JSON.parse(text), { theme: 'test' })
  assert.equal(fs.existsSync(path.join(data.backup, '.env')), false)
  assert.equal(fs.existsSync(path.join(data.backup, 'unexpected.txt')), false)
  assert.equal(fs.existsSync(path.join(data.backup, 'provider-v2-execution')), false)
  restoreProviderState(data.backup, data.restore)
  for (const entry of inventory) {
    assert.deepEqual(fs.readFileSync(path.join(data.restore, entry.path)), fs.readFileSync(path.join(data.backup, entry.path)))
  }
})

test('provider-v2 artifacts and uploads require independent explicit flags', () => {
  const data = fixture()
  const inventory = backupProviderState(data.source, data.backup, { includeArtifacts: true })
  assert(inventory.some((entry) => entry.path === 'provider-v2/artifacts/artifact.bin'))
  assert(!inventory.some((entry) => entry.path.includes('/uploads/')))
  const second = fixture()
  const uploads = backupProviderState(second.source, second.backup, { includeUploads: true })
  assert(uploads.some((entry) => entry.path === 'provider-v2/uploads/upload.bin'))
  assert(!uploads.some((entry) => entry.path.includes('/artifacts/')))
})

test('settings secret rejection never emits the secret value', () => {
  const data = fixture()
  const secret = 'synthetic-value-never-print'
  fs.writeFileSync(path.join(data.source, 'settings.json'), JSON.stringify({ agentControlToken: secret }))
  assert.throws(() => backupProviderState(data.source, data.backup), (error) => {
    assert(!error.message.includes(secret))
    return /agentControlToken/.test(error.message)
  })
  const second = fixture()
  fs.writeFileSync(path.join(second.source, 'settings.json'), JSON.stringify({ futurePassword: secret }))
  assert.throws(() => backupProviderState(second.source, second.backup), (error) => {
    assert(!error.message.includes(secret))
    return /secret-like field name/.test(error.message)
  })
})

test('hash mismatch, nonempty restore, and linked source paths fail closed', () => {
  const data = fixture()
  backupProviderState(data.source, data.backup)
  fs.mkdirSync(data.restore)
  fs.writeFileSync(path.join(data.restore, 'occupied'), 'x')
  assert.throws(() => restoreProviderState(data.backup, data.restore), /must be empty/)
  fs.rmSync(data.restore, { recursive: true })
  fs.writeFileSync(path.join(data.backup, 'provider-v2', 'ledger.json'), 'tampered')
  assert.throws(() => restoreProviderState(data.backup, data.restore), /Inventory verification failed/)
  const traversal = fixture()
  backupProviderState(traversal.source, traversal.backup)
  const manifestPath = path.join(traversal.backup, 'inventory.json')
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
  manifest.files[0].path = 'provider-v2/artifacts/../../escape'
  fs.writeFileSync(manifestPath, JSON.stringify(manifest))
  assert.throws(() => restoreProviderState(traversal.backup, traversal.restore), /canonical and relative/)
  const linked = fixture()
  fs.rmSync(path.join(linked.source, 'venice-models.json'))
  fs.symlinkSync(path.join(linked.source, 'control-api.json'), path.join(linked.source, 'venice-models.json'))
  assert.throws(() => backupProviderState(linked.source, linked.backup), /Linked paths are forbidden/)
})

test('release slots activate and roll forward with an atomic pointer', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'vml-slots-'))
  assert.deepEqual(activateReleaseSlot(root, 'slot-a', Buffer.from('a')), { slot: 'slot-a', previous: null })
  assert.deepEqual(activateReleaseSlot(root, 'slot-b', Buffer.from('b')), { slot: 'slot-b', previous: 'slot-a' })
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(root, 'current.json'), 'utf8')), {
    schemaVersion: 1, slot: 'slot-b', previous: 'slot-a',
    sha256: '3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d',
  })
  assert.equal(fs.existsSync(path.join(root, 'current.json.tmp')), false)
})
