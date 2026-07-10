import assert from 'node:assert/strict'
import fs from 'node:fs'
import test from 'node:test'

const root = new URL('../', import.meta.url)
const source = fs.readFileSync(new URL('src-tauri/src/main.rs', root), 'utf8').replaceAll('\r\n', '\n')
const manifest = JSON.parse(fs.readFileSync(new URL('src-tauri/src/capability-manifest.v1.json', root), 'utf8'))

const expectedCapabilities = new Map([
  ['media.image.generate', ['/api/v1/generate-image', 'none']],
  ['media.image.edit', ['/api/v1/edit-image', 'none']],
  ['media.image.background-remove', ['/api/v1/remove-background', 'none']],
  ['media.image.upscale', ['/api/v1/upscale-image', 'none']],
  ['media.video.generate', ['/api/v1/queue-video', 'poll']],
  ['media.audio.music.generate', ['/api/v1/queue-music', 'poll']],
  ['media.audio.sfx.generate', ['/api/v1/queue-sfx', 'poll']],
  ['media.voice.generate', ['/api/v1/generate-speech', 'none']],
  ['media.transcribe', ['/api/v1/transcribe-audio', 'none']],
  ['media.models.list', ['/api/v1/state', 'none']],
  ['media.models.refresh', ['/api/v1/refresh-models', 'none']],
])

test('schema 1.0 manifest maps exactly to the existing media routes', () => {
  assert.equal(manifest.schemaVersion, '1.0')
  assert.equal(manifest.provider.id, 'venice-media-local')
  assert.equal(manifest.provider.kind, 'media')
  assert.deepEqual(new Set(manifest.capabilities.map(({ id }) => id)), new Set(expectedCapabilities.keys()))

  for (const capability of manifest.capabilities) {
    const [path, progressMode] = expectedCapabilities.get(capability.id)
    assert.equal(capability.invocation.path, path, capability.id)
    assert.match(source, new RegExp(`\\.route\\("${path.replaceAll('/', '\\/')}"`), `${path} must remain routed`)
    assert.equal(capability.progress.mode, progressMode, capability.id)
    assert.deepEqual(capability.cancellation, { supported: false }, capability.id)
    assert.equal(capability.inputSchema.$schema, 'https://json-schema.org/draft/2020-12/schema')
    assert.equal(capability.outputSchema.$schema, 'https://json-schema.org/draft/2020-12/schema')
    assert.ok(Array.isArray(capability.sideEffects) && capability.sideEffects.length > 0)
    assert.ok(typeof capability.riskClass === 'string' && capability.riskClass.length > 0)
    assert.ok(typeof capability.approvalClass === 'string' && capability.approvalClass.length > 0)
    assert.ok(Array.isArray(capability.artifacts))
  }
})

test('queued capabilities advertise polling but no provider cancellation', () => {
  const queued = manifest.capabilities.filter(({ progress }) => progress.mode === 'poll')
  assert.deepEqual(queued.map(({ id }) => id).sort(), [
    'media.audio.music.generate',
    'media.audio.sfx.generate',
    'media.video.generate',
  ])
  for (const capability of queued) {
    assert.equal(capability.progress.intervalMs, 2000)
    assert.equal(capability.invocation.poll.path, capability.progress.path)
    assert.equal(capability.invocation.poll.operationIdField, 'queueId')
    assert.equal(capability.cancellation.supported, false)
    assert.equal(capability.cancellation.path, undefined)
  }
})

test('manifest and health routes are bearer protected and credential-free', () => {
  for (const handler of ['agent_get_capabilities', 'agent_get_health']) {
    const body = source.match(new RegExp(`async fn ${handler}[\\s\\S]*?\\n\\}`))?.[0] || ''
    assert.match(body, /check_agent_token\(&state, &headers\)\?/)
  }

  const serialized = JSON.stringify(manifest).toLowerCase()
  for (const forbidden of ['token', 'api_key', 'apikey', 'cookie', 'password', 'settings', 'outputdir']) {
    assert.equal(serialized.includes(forbidden), false, `manifest leaked forbidden field: ${forbidden}`)
  }

  const health = source.match(/fn capability_health[\s\S]*?\n\}/)?.[0] || ''
  assert.doesNotMatch(health, /read_settings|agent_control_token|output_dir|VENICE_API_KEY|read_api_key/)
  assert.match(health, /key_configured = has_api_key\(\)/)
  assert.match(health, /models_loaded = model_cache_has_usable_models\(&read_model_cache\(app\)\)/)
  assert.match(health, /operations_ready = key_configured && models_loaded/)
  assert.match(health, /if operations_ready[\s\S]*"ready"[\s\S]*else if key_configured[\s\S]*"degraded"[\s\S]*else[\s\S]*"unavailable"/)
})

test('discovery keeps legacy fields and adds provider URLs and schema versions', () => {
  const writer = source.match(/fn write_agent_control_discovery[\s\S]*?\n\}/)?.[0] || ''
  for (const legacyField of ['address', 'bindAddress', 'bindAll', 'tailscaleIp', 'port', 'token', 'version', 'note']) {
    assert.match(writer, new RegExp(`"${legacyField}"\\s*:`), `missing legacy discovery field ${legacyField}`)
  }
  assert.match(writer, /"manifestUrl": format!\("\{\}\/api\/v1\/capabilities"/)
  assert.match(writer, /"healthUrl": format!\("\{\}\/api\/v1\/health"/)
  assert.match(writer, /"schemaVersions": \[CAPABILITY_SCHEMA_VERSION\]/)
  assert.match(source, /\.route\("\/api\/v1\/capabilities", get\(agent_get_capabilities\)\)/)
  assert.match(source, /\.route\("\/api\/v1\/health", get\(agent_get_health\)\)/)
})

test('artifact declarations match provider-local response delivery', () => {
  const producing = manifest.capabilities.filter(({ artifacts }) => artifacts.length > 0)
  assert.ok(producing.length > 0)
  for (const capability of producing) {
    for (const artifact of capability.artifacts) {
      assert.ok(artifact.role)
      assert.ok(artifact.kinds.length > 0)
      assert.equal(artifact.kind, undefined)
      assert.ok(artifact.mimeTypes.length > 0)
      assert.ok(artifact.deliveryModes.length > 0)
      for (const mode of artifact.deliveryModes) {
        assert.ok(['data-url', 'provider-path', 'inline', 'provider-reference', 'url', 'eva-core', 'project'].includes(mode), `${capability.id}: ${mode}`)
      }
    }
  }
})

test('installation instance ID is persisted separately from machine identity', () => {
  const idHelper = source.match(/fn installation_instance_id\([\s\S]*?\n\}/)?.[0] || ''
  const manifestHelper = source.match(/fn capability_manifest\([\s\S]*?\n\}/)?.[0] || ''
  const healthHelper = source.match(/fn capability_health\([\s\S]*?\n\}/)?.[0] || ''

  assert.match(source, /join\("capability-provider-instance-id"\)/)
  assert.match(source, /static INSTALLATION_INSTANCE_ID: OnceLock<Mutex<Option<String>>>/)
  assert.match(idHelper, /INSTALLATION_INSTANCE_ID\.get_or_init/)
  assert.match(idHelper, /fs::read_to_string\(&path\)/)
  assert.match(idHelper, /fs::write\(&path, format!\("\{instance_id\}\\n"\)\)/)
  assert.match(manifestHelper, /\["instanceId"\] = json!\(installation_instance_id\(app\)\?\)/)
  assert.match(manifestHelper, /\["machineId"\] = json!\(capability_machine_id\(\)\)/)
  assert.match(healthHelper, /"instanceId": installation_instance_id\(app\)\?/)
})
