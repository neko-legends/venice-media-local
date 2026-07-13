import { createHash } from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const SETTINGS_FIELDS = new Set([
  'theme', 'outputDir', 'writeMetadataSidecars', 'privateSession',
  'genericFilenames', 'showDiemBalance', 'windowWidth', 'windowHeight',
  'enableAgentControl', 'agentControlPort', 'agentControlBindAll',
  'agentControlToken', 'selectedModels',
])
const SECRET_FIELD = /(api.?key|secret|password|private.?key|credential|bearer|token)/i
const BASE_FILES = [
  'settings.json',
  'venice-models.json',
  'control-api.json',
  'capability-provider-instance-id',
  'provider-v1/lifecycle.json',
  'provider-v2/ledger.json',
]
const RELEASE_POLICY_KEYS = [
  'schemaVersion', 'lane', 'allowedAuthenticodeState', 'destinationMachines',
  'stageRoot', 'releaseRoot', 'rollbackRoot', 'publicDistributionAllowed',
]

function fail(message) {
  throw new Error(message)
}

export function validateReleasePolicy(value) {
  if (!value || Array.isArray(value) || typeof value !== 'object') fail('Release policy must be an object')
  const actualKeys = Object.keys(value).sort()
  const expectedKeys = [...RELEASE_POLICY_KEYS].sort()
  if (actualKeys.length !== expectedKeys.length || actualKeys.some((key, index) => key !== expectedKeys[index])) {
    fail('Release policy fields are invalid')
  }
  if (value.schemaVersion !== 1) fail('Release policy schemaVersion must be 1')
  if (value.lane !== 'owner-controlled-internal') fail('Phase 5H lane must be owner-controlled-internal')
  if (value.allowedAuthenticodeState !== 'NotSigned') fail('Allowed Authenticode state must be exactly NotSigned')
  if (value.publicDistributionAllowed !== false) fail('Public distribution must be disabled for Phase 5H')
  if (!Array.isArray(value.destinationMachines) || value.destinationMachines.length === 0) {
    fail('Destination machines must be a nonempty array')
  }
  const machines = new Set()
  for (const machine of value.destinationMachines) {
    if (typeof machine !== 'string' || !machine.trim() || machine !== machine.trim() || /[*?]/.test(machine)) {
      fail('Destination machines must contain explicit names without wildcards')
    }
    if (/^(REPLACE|CHANGEME|HUMAN[-_ ]DECISION|JUN[-_ ]CONTROLLED)/i.test(machine)) {
      fail('Destination machines must replace example placeholders with exact machine names')
    }
    const normalized = machine.toUpperCase()
    if (machines.has(normalized)) fail('Destination machines must be unique')
    machines.add(normalized)
  }
  const roots = ['stageRoot', 'releaseRoot', 'rollbackRoot'].map((key) => {
    if (typeof value[key] !== 'string' || !/^[A-Za-z]:[\\/]/.test(value[key]) || !path.win32.isAbsolute(value[key])) {
      fail(`${key} must be an absolute Windows path`)
    }
    const normalized = path.win32.normalize(value[key]).replace(/[\\/]+$/, '').toUpperCase()
    if (/^[A-Z]:$/.test(normalized) || normalized.split('\\').some((part) => [
      'APPDATA', 'COMMUNITY.VENICE.MEDIA.LOCAL', 'OUTPUT', 'OUTPUTS', 'DIST', 'TARGET',
      'REPO', 'REPOS', 'REPOSITORY', 'REPOSITORIES', 'VENICE-MEDIA-LOCAL',
    ].includes(part))) fail(`${key} must be outside app data, repository, build, and output roots`)
    return normalized
  })
  if (new Set(roots).size !== roots.length) fail('Stage, release, and rollback roots must be distinct')
  for (const root of roots) {
    if (roots.some((other) => other !== root && (root.startsWith(`${other}\\`) || other.startsWith(`${root}\\`)))) {
      fail('Stage, release, and rollback roots must not contain one another')
    }
  }
  return value
}

function assertDirectory(root, label) {
  const stat = fs.lstatSync(root)
  if (!stat.isDirectory() || stat.isSymbolicLink()) fail(`${label} must be a real directory`)
}

function assertSafeEntry(root, relative, expectedType) {
  if (path.posix.normalize(relative) !== relative || relative.startsWith('/') || relative.split('/').includes('..')) {
    fail('Path must be canonical and relative')
  }
  let current = root
  for (const part of relative.split('/')) {
    current = path.join(current, part)
    const stat = fs.lstatSync(current)
    if (stat.isSymbolicLink()) fail(`Linked paths are forbidden: ${relative}`)
  }
  const stat = fs.lstatSync(current)
  if (expectedType === 'file' && !stat.isFile()) fail(`Expected a regular file: ${relative}`)
  if (expectedType === 'directory' && !stat.isDirectory()) fail(`Expected a directory: ${relative}`)
  return current
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function sanitizedSettings(bytes) {
  let value
  try {
    value = JSON.parse(bytes.toString('utf8'))
  } catch {
    fail('settings.json must contain valid JSON')
  }
  if (!value || Array.isArray(value) || typeof value !== 'object') fail('settings.json must contain an object')
  if (typeof value.agentControlToken === 'string' && value.agentControlToken.trim()) {
    fail('settings.json contains a non-empty legacy agentControlToken')
  }
  for (const key of Object.keys(value)) {
    if (!SETTINGS_FIELDS.has(key) && SECRET_FIELD.test(key)) {
      fail(`settings.json contains forbidden secret-like field name: ${key}`)
    }
  }
  const clean = {}
  for (const key of [...SETTINGS_FIELDS].sort()) {
    if (key !== 'agentControlToken' && Object.hasOwn(value, key)) clean[key] = value[key]
  }
  return Buffer.from(`${JSON.stringify(clean, null, 2)}\n`)
}

function collectTree(root, relative, output) {
  const directory = assertSafeEntry(root, relative, 'directory')
  for (const name of fs.readdirSync(directory).sort()) {
    const child = `${relative}/${name}`
    const childPath = assertSafeEntry(root, child)
    const stat = fs.lstatSync(childPath)
    if (stat.isDirectory()) collectTree(root, child, output)
    else if (stat.isFile()) output.push(child)
    else fail(`Unsupported filesystem entry: ${child}`)
  }
}

function writeAtomicJson(target, value) {
  const temporary = `${target}.tmp`
  fs.writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx' })
  fs.renameSync(temporary, target)
}

export function backupProviderState(source, destination, options = {}) {
  assertDirectory(source, 'Source')
  if (fs.existsSync(destination)) fail('Backup destination must not exist')
  const relativeFiles = BASE_FILES.filter((relative) => fs.existsSync(path.join(source, relative)))
  if (options.includeArtifacts && fs.existsSync(path.join(source, 'provider-v2/artifacts'))) {
    collectTree(source, 'provider-v2/artifacts', relativeFiles)
  }
  if (options.includeUploads && fs.existsSync(path.join(source, 'provider-v2/uploads'))) {
    collectTree(source, 'provider-v2/uploads', relativeFiles)
  }
  const prepared = []
  for (const relative of [...new Set(relativeFiles)].sort()) {
    const sourcePath = assertSafeEntry(source, relative, 'file')
    const bytes = relative === 'settings.json' ? sanitizedSettings(fs.readFileSync(sourcePath)) : fs.readFileSync(sourcePath)
    prepared.push({ relative, bytes })
  }
  fs.mkdirSync(destination)
  const inventory = []
  for (const { relative, bytes } of prepared) {
    const destinationPath = path.join(destination, ...relative.split('/'))
    fs.mkdirSync(path.dirname(destinationPath), { recursive: true })
    fs.writeFileSync(destinationPath, bytes, { flag: 'wx' })
    inventory.push({ path: relative, size: bytes.length, sha256: sha256(bytes) })
  }
  writeAtomicJson(path.join(destination, 'inventory.json'), { schemaVersion: 1, files: inventory })
  return inventory
}

export function restoreProviderState(backup, destination) {
  assertDirectory(backup, 'Backup')
  if (fs.existsSync(destination)) {
    assertDirectory(destination, 'Restore destination')
    if (fs.readdirSync(destination).length) fail('Restore destination must be empty')
  } else {
    fs.mkdirSync(destination)
  }
  const inventoryPath = assertSafeEntry(backup, 'inventory.json', 'file')
  const inventory = JSON.parse(fs.readFileSync(inventoryPath, 'utf8'))
  if (inventory.schemaVersion !== 1 || !Array.isArray(inventory.files)) fail('Inventory format is invalid')
  for (const entry of inventory.files) {
    if (!entry || typeof entry.path !== 'string' || !BASE_FILES.includes(entry.path) &&
      !entry.path.startsWith('provider-v2/artifacts/') && !entry.path.startsWith('provider-v2/uploads/')) {
      fail('Inventory contains a forbidden path')
    }
    const bytes = fs.readFileSync(assertSafeEntry(backup, entry.path, 'file'))
    if (bytes.length !== entry.size || sha256(bytes) !== entry.sha256) fail(`Inventory verification failed: ${entry.path}`)
    const target = path.join(destination, ...entry.path.split('/'))
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.writeFileSync(target, bytes, { flag: 'wx' })
  }
  return inventory.files
}

export function activateReleaseSlot(root, slot, artifactBytes) {
  if (!/^[a-z0-9][a-z0-9._-]*$/i.test(slot)) fail('Release slot name is invalid')
  fs.mkdirSync(root, { recursive: true })
  const slots = path.join(root, 'slots')
  fs.mkdirSync(slots, { recursive: true })
  const slotRoot = path.join(slots, slot)
  if (fs.existsSync(slotRoot)) fail('Release slot already exists')
  fs.mkdirSync(slotRoot)
  fs.writeFileSync(path.join(slotRoot, 'venice-media-local.exe'), artifactBytes, { flag: 'wx' })
  const previous = fs.existsSync(path.join(root, 'current.json'))
    ? JSON.parse(fs.readFileSync(path.join(root, 'current.json'), 'utf8')).slot
    : null
  writeAtomicJson(path.join(root, 'current.json'), { schemaVersion: 1, slot, previous, sha256: sha256(artifactBytes) })
  return { slot, previous }
}

function parseCli(argv) {
  const [command, source, destination, ...flags] = argv
  if (command === 'validate-policy' && source && destination === undefined && flags.length === 0) {
    const policyPath = path.resolve(source)
    validateReleasePolicy(JSON.parse(fs.readFileSync(policyPath, 'utf8')))
  } else if (command === 'backup' && source && destination) {
    backupProviderState(path.resolve(source), path.resolve(destination), {
      includeArtifacts: flags.includes('--include-artifacts'),
      includeUploads: flags.includes('--include-uploads'),
    })
  } else if (command === 'restore' && source && destination && flags.length === 0) {
    restoreProviderState(path.resolve(source), path.resolve(destination))
  } else {
    fail('Usage: phase5h-readiness.mjs validate-policy <policy.json> | backup <synthetic-source> <new-backup> [--include-artifacts] [--include-uploads] | restore <backup> <new-empty-destination>')
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    parseCli(process.argv.slice(2))
  } catch (error) {
    console.error(error.message)
    process.exitCode = 1
  }
}
