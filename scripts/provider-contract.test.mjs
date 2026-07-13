import assert from 'node:assert/strict'
import fs from 'node:fs'
import test from 'node:test'

const root = new URL('../', import.meta.url)
const main = fs.readFileSync(new URL('src-tauri/src/main.rs', root), 'utf8').replaceAll('\r\n', '\n')
const provider = fs.readFileSync(new URL('src-tauri/src/provider.rs', root), 'utf8').replaceAll('\r\n', '\n')
const kernel = fs.readFileSync(new URL('src-tauri/provider-kernel/src/lib.rs', root), 'utf8').replaceAll('\r\n', '\n')
const manifest = JSON.parse(fs.readFileSync(new URL('src-tauri/src/capability-manifest.v1.json', root), 'utf8'))
const wire = JSON.parse(fs.readFileSync(new URL('../eva-core/docs/venice-media-operation-v1-wire-fixture.json', root), 'utf8'))

const expected = [
  'media.image.generate', 'media.image.edit', 'media.image.background-remove', 'media.image.upscale',
  'media.video.generate', 'media.audio.music.generate', 'media.audio.sfx.generate', 'media.voice.generate',
  'media.transcribe', 'media.models.list', 'media.models.refresh',
]

test('schema 1.0 exposes unique routable revision-2 capabilities and deprecated revision-1 descriptors', () => {
  assert.equal(manifest.schemaVersion, '1.0')
  assert.deepEqual(manifest.capabilities.map(({ id }) => id), expected)
  assert.deepEqual(manifest.compatibilityCapabilities.map(({ id }) => id), expected)
  for (const capability of manifest.capabilities) {
    assert.equal(capability.revision, '2')
    assert.deepEqual(capability.invocation, { envelope: 'veniceMediaOperation.v1', method: 'POST', path: '/api/v1/operations' })
    assert.deepEqual(capability.progress, {
      mode: 'callback',
      pollFallbackPath: '/api/v1/operations/{providerOperationId}',
      eventReplayPath: '/api/v1/operations/{providerOperationId}/events',
    })
    assert.equal(capability.cancellation.supported, true)
    assert.equal(capability.cancellation.idempotent, true)
    assert.deepEqual(capability.cancellation.scope, ['pre_submission'])
    assert.equal(capability.inputSchema.additionalProperties, false)
  }
  for (const compatibility of manifest.compatibilityCapabilities) {
    assert.equal(compatibility.revision, '1')
    assert.equal(compatibility.deprecated, true)
    assert.match(main, new RegExp(`\\.route\\("${compatibility.path.replaceAll('/', '\\/')}"`))
  }
})

test('async operation, upload, artifact, event, and cancel routes are all wired', () => {
  for (const [method, path, constant] of [
    ['post', '/api/v1/operations', 'OPERATIONS_PATH'],
    ['get', '/api/v1/operations/:operation_id', 'OPERATION_PATH'],
    ['get', '/api/v1/operations/:operation_id/events', 'OPERATION_EVENTS_PATH'],
    ['post', '/api/v1/operations/:operation_id/cancel', 'OPERATION_CANCEL_PATH'],
    ['post', '/api/v1/operations/:operation_id/execute', 'OPERATION_EXECUTE_PATH'],
    ['post', '/api/v1/operations/:operation_id/transfer-grants', 'OPERATION_GRANTS_PATH'],
    ['post', '/api/v1/artifact-uploads', 'UPLOADS_PATH'],
    ['put', '/api/v1/artifact-uploads/:upload_id/content', 'UPLOAD_CONTENT_PATH'],
    ['post', '/api/v1/artifact-uploads/:upload_id/complete', 'UPLOAD_COMPLETE_PATH'],
    ['delete', '/api/v1/artifact-uploads/:upload_id', 'UPLOAD_PATH'],
    ['get', '/api/v1/artifacts/:artifact_id', 'ARTIFACT_PATH'],
    ['get', '/api/v1/artifacts/:artifact_id/content', 'ARTIFACT_CONTENT_PATH'],
  ]) {
    if (constant) {
      assert.match(kernel, new RegExp(`pub const ${constant}: &str = "${path.replaceAll('/', '\\/')}"`), path)
      assert.match(kernel, new RegExp(`\\.route\\(\\s*${constant}[\\s\\S]*?${method}\\(`), path)
    } else {
      assert.match(provider, new RegExp(`\\.route\\(\\s*"${path.replaceAll('/', '\\/')}"[\\s\\S]*?${method}\\(`), path)
    }
  }
  assert.match(main, /\.merge\(kernel(?:\.clone\(\))?\.router\(\)\)/)
  assert.doesNotMatch(main, /\.merge\(provider::routes\(\)\)/)
  assert.doesNotMatch(main, /provider::recover\(/)
})

test('authenticated whole-application shutdown is narrow, response-first, and has no forced fallback', () => {
  assert.match(kernel, /pub const SHUTDOWN_PATH: &str = "\/api\/v1\/actions\/shutdown"/)
  assert.match(kernel, /pub const SHUTDOWN_SCOPE: &str = "application:shutdown"/)
  assert.match(kernel, /veniceMediaApplicationShutdown\.v1/)
  assert.match(kernel, /gate\.accepting = false/)
  assert.match(kernel, /SHUTDOWN_RECONCILIATION_REQUIRED/)
  assert.match(kernel, /SHUTDOWN_PERMISSION_DENIED/)
  assert.match(kernel, /token_scopes\.contains\(SHUTDOWN_SCOPE\)/)
  assert.match(kernel, /claim_compatibility/)
  assert.match(kernel, /active_work_count/)
  assert.match(kernel, /compatibility_in_flight/)
  assert.match(kernel, /REQUEST_ID_CONFLICT/)
  assert.match(kernel, /orchestrate_shutdown/)
  assert.match(kernel, /shutdown_resources/)
  assert.match(kernel, /LifecycleSupervisor/)
  assert.match(kernel, /TerminalShutdownLatch/)
  assert.match(kernel, /Global order: Settings\/lifecycle transaction barrier, then provider ledger/)
  assert.match(kernel, /shutdown_transaction\.lock\(\)\.await/)
  assert.match(kernel, /run_until_shutdown_then_drain/)
  assert.match(kernel, /run_post_kernel_startup/)
  assert.match(kernel, /transactional_disable_then_delete/)
  assert.match(kernel, /AgentControlOwnership/)
  assert.match(kernel, /GenerationOwnedJsonFile/)
  assert.match(kernel, /emergency-audit/)
  assert.match(kernel, /veniceMediaShutdownEmergencyAudit\.v1/)
  assert.match(kernel, /CommittedWithDurabilityWarning/)
  assert.match(kernel, /CommitStateUnknown/)
  assert.match(kernel, /LIFECYCLE_START_STALE/)
  assert.match(main, /save_settings_file\(&app, &settings\)[\s\S]*?start_agent_control_server/)
  assert.match(main, /settings\.enable_agent_control = false;[\s\S]*?save_settings_file\(&app, &settings\)/)
  assert.match(main, /else if !enable && was_enabled \{[\s\S]*?save_settings_file\(&app, &settings\)[\s\S]*?stop_agent_control_server[\s\S]*?unregister_lifecycle/)
  assert.match(main, /reserve_start[\s\S]*?StdTcpListener::bind/)
  assert.match(main, /tokio::spawn\(async move \{ server\.await[\s\S]*?publish_running/)
  assert.match(main, /GenerationOwnedJsonFile/)
  assert.match(main, /terminal[\s\S]*?ensure_open[\s\S]*?write_agent_control_discovery/)
  assert.match(main, /Storage::write_atomic\(&storage, "settings\.json"/)
  assert.match(provider, /pub async fn clear_lifecycle[\s\S]*?lifecycle_supervisor\(\)[\s\S]*?\.stop/)
  assert.match(provider, /configure_lifecycle_transactional/)
  assert.match(provider, /restore_lifecycle_worker/)
  assert.match(main, /transaction: venice_provider_kernel::SettingsTransaction/)
  assert.match(main, /persist_window_size[\s\S]*?transaction\.lock\(\)\.await/)
  assert.match(main, /rollback_agent_control_startup/)
  assert.match(main, /claim_direct_work/)
  assert.match(main, /activeCompatibilityDirectCount/)
  for (const controlPlaneCommand of [
    'save_settings',
    'rotate_agent_control_token',
    'configure_provider_lifecycle',
    'clear_provider_lifecycle',
  ]) {
    const body = main.match(new RegExp(`async fn ${controlPlaneCommand}\\([\\s\\S]*?\\n\\}`))?.[0] || ''
    assert.doesNotMatch(body, /claim_direct_work/, `${controlPlaneCommand} must not block an overlapping authenticated shutdown`)
  }
  const stopControl = main.match(/async fn stop_agent_control_server[\s\S]*?\n\}/)?.[0] || ''
  assert.doesNotMatch(stopControl, /set_lifecycle_generation/)
  assert.match(kernel, /SERVER_DRAIN_TIMEOUT/)
  assert.match(kernel, /Duration::from_secs\(20\)/)
  assert.match(kernel, /StatusCode::UNPROCESSABLE_ENTITY,\s*"ARTIFACT_INTEGRITY_MISMATCH"/)
  assert.match(main, /server\.await/)
  assert.match(kernel, /record_shutdown_stage\(&digest, "response_drained"/)
  assert.match(kernel, /record_shutdown_stage\(&digest, "exit_requested"/)
  assert.match(main, /app\.exit\(0\)/)
  const shutdownPath = kernel.match(/async fn shutdown_application[\s\S]*?\n\}/)?.[0] || ''
  assert.doesNotMatch(shutdownPath, /std::process|Command::new|taskkill|Stop-Process|kill\(/i)
  const orchestration = kernel.match(/pub async fn orchestrate_shutdown[\s\S]*?TeardownOutcome::Exited/)?.[0] || ''
  assert.ok(orchestration.indexOf('response_drained') < orchestration.indexOf('release_resources'))
  assert.ok(orchestration.indexOf('release_resources') < orchestration.indexOf('unregister_lifecycle'))
  assert.ok(orchestration.indexOf('unregister_lifecycle') < orchestration.indexOf('exit_requested'))
  assert.ok(orchestration.indexOf('exit_requested') < orchestration.indexOf('request_exit'))
  assert.match(main, /self\.app\.exit\(0\)/)
})

test('the executable provider uses the canonical Core wire ordering and field names', () => {
  assert.equal(wire.type, 'veniceMediaOperation.v1')
  assert.equal(wire.grantRegistration.path, '/api/v1/operations/{providerOperationId}/transfer-grants')
  assert.deepEqual(wire.uploadOrdering.slice(-2), [
    'POST /api/v1/artifact-uploads/{uploadId}/complete',
    'POST /api/v1/operations/{providerOperationId}/execute',
  ])
  for (const field of Object.keys(wire.grantRegistration.body)) {
    const rustField = field.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`)
    assert.match(kernel, new RegExp(`\\b${rustField}\\b`), field)
  }
  assert.match(kernel, /x-transfer-grant-id/)
  assert.match(kernel, /x-transfer-grant"/)
})

test('manifest, health, state, discovery, and ledgers exclude credentials', () => {
  const serialized = JSON.stringify(manifest).toLowerCase()
  for (const forbidden of ['api_key', 'apikey', 'cookie', 'password', 'authorization']) {
    assert.equal(serialized.includes(forbidden), false, forbidden)
  }
  assert.match(main, /agent_control_token: Option<String>/)
  assert.match(main, /persisted\.agent_control_token = None/)
  assert.match(main, /settings\.remove\("agentControlToken"\)/)
  assert.doesNotMatch(main.match(/let discovery = serde_json::json!\([\s\S]*?\n    \}\);/)?.[0] || '', /"token"\s*:/)
  assert.match(main, /"credentialId"\s*:/)
  assert.match(provider, /EncryptedSecrets/)
  assert.match(provider, /AES-256-GCM/)
  assert.match(provider, /keyring::Entry/)
  assert.match(kernel, /SecretProtector/)
  assert.match(kernel, /EncryptedSecret/)
  for (const field of ['grant_id', 'core_operation_id', 'attempt', 'assignment_revision', 'capability_id', 'method', 'path', 'scope', 'upload_id', 'artifact_id', 'expected_sha256', 'expected_byte_size', 'expected_mime_type', 'not_before', 'expires_at', 'max_uses', 'uses']) {
    assert.match(kernel, new RegExp(`\\b${field}\\b`), field)
  }
  assert.match(kernel, /verify_grant/)
  assert.match(provider, /register_lifecycle_once/)
  assert.match(provider, /send_lifecycle_heartbeat/)
  assert.match(provider, /unregister_lifecycle/)
})

test('health is compatibility-shaped but fallback-only catalogs cannot report ready', () => {
  const health = main.match(/fn capability_health[\s\S]*?\n\}/)?.[0] || ''
  for (const field of ['agentControl', 'veniceCredential', 'models', 'operations', 'operationLedger', 'callbackOutbox', 'artifactStore', 'disk']) {
    assert.match(health, new RegExp(`"${field}"`))
  }
  assert.match(health, /MODEL_CATALOG_FALLBACK_ONLY/)
  assert.match(health, /operations_ready = key_configured && models_loaded && ledger_ready && artifact_writable/)
  assert.match(health, /"availableBytes": null/)
})

test('provider ledger records admission before execution and never blindly resubmits', () => {
  assert.match(kernel, /submission_not_started/)
  assert.match(kernel, /submission_started/)
  assert.match(kernel, /submitted_confirmed/)
  assert.match(kernel, /executor\.submit/)
  assert.match(kernel, /executor\.resume/)
  assert.match(kernel, /Started submission cannot be invoked again/)
  assert.match(kernel, /SUBMISSION_OUTCOME_UNKNOWN/)
  assert.match(main, /TauriMediaExecutor/)
  assert.match(main, /\.merge\(kernel(?:\.clone\(\))?\.router\(\)\)/)
  assert.doesNotMatch(main, /\.merge\(provider::routes\(\)\)/)
})

test('sidecars and async records are bounded and descriptor-first while legacy dataUrl remains', () => {
  assert.match(main, /omittedMetadataSha256/)
  assert.match(main, /Media sidecar exceeds the 64 KiB safety limit/)
  assert.match(main, /atomic_write_bytes/)
  assert.doesNotMatch(main.match(/fn media_sidecar_json[\s\S]*?\n\}/)?.[0] || '', /unwrap_or_else\(\|\| metadata\.clone\(\)\)/)
  assert.match(main, /data_url: format!\("data:\{mime_type\};base64,\{encoded\}"\)/)
  for (const capability of manifest.capabilities.filter(({ artifacts }) => artifacts.length)) {
    assert.ok(capability.artifacts.every(({ deliveryModes }) => deliveryModes.includes('provider-reference')))
  }
})

test('production catalog binds model and model-less capabilities explicitly', () => {
  assert.match(main, /"capabilityIds": capability_ids/)
  for (const [id, capability] of [
    ['background-remove', 'media.image.background-remove'],
    ['upscale', 'media.image.upscale'],
    ['model-list', 'media.models.list'],
    ['model-refresh', 'media.models.refresh'],
  ]) {
    assert.match(main, new RegExp(`"id":"${id}"[\\s\\S]*?"capabilityIds":\\["${capability.replaceAll('.', '\\.')}"\\]`))
  }
})

test('shared ledger replacement is Windows-safe and recoverable', () => {
  assert.match(kernel, /create_new\(true\)/)
  assert.match(kernel, /sync_all\(\)/)
  assert.match(kernel, /random_id\(""\)/)
  assert.match(kernel, /with_extension\("bak"\)/)
  assert.match(kernel, /recover_atomic\("ledger\.json"\)/)
  assert.doesNotMatch(kernel, /let temp = path\.with_extension\("tmp"\)/)
})
