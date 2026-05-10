import { invoke } from '@tauri-apps/api/core'
import { open } from '@tauri-apps/plugin-dialog'
import {
  Database,
  Download,
  Eraser,
  FileText,
  FolderOpen,
  Image as ImageIcon,
  KeyRound,
  Loader2,
  Mic2,
  Music,
  Plus,
  RefreshCw,
  Scissors,
  Settings,
  Trash2,
  Video,
  Volume2,
  Wand2,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import { ChangeEvent, ClipboardEvent, DragEvent, FocusEvent, FormEvent, MouseEvent, ReactNode, useEffect, useMemo, useRef, useState } from 'react'

type ModeId = 'image' | 'edit' | 'video' | 'music' | 'sfx' | 'voice' | 'transcribe' | 'models' | 'settings'
type ModelKind = 'image' | 'edit' | 'video' | 'music' | 'sfx' | 'voice' | 'transcribe'
type ThemeId = 'eva-dark' | 'pearl-white' | 'abyss-teal' | 'ember' | 'mosswood' | 'rose-noir'

type ModelRecord = {
  id: string
  name: string
  kind: ModelKind
  modes: string[]
  controls: Record<string, unknown>
  raw?: unknown
}

type ModelCache = {
  lastFetched: string
  imageModels: ModelRecord[]
  editModels: ModelRecord[]
  videoModels: ModelRecord[]
  musicModels: ModelRecord[]
  sfxModels: ModelRecord[]
  voiceModels: ModelRecord[]
  transcribeModels: ModelRecord[]
}

type AppSettings = {
  theme: ThemeId
  outputDir: string
  showDiemBalance: boolean
}

type AppState = {
  settings: AppSettings
  keyConfigured: boolean
  models: ModelCache
  buildVersion: string
}

type MediaResult = {
  id: string
  kind: string
  name: string
  mimeType: string
  dataUrl: string
  filePath: string
  text?: string
  metadata?: unknown
}

type ResultGroup = {
  id: string
  kind: string
  title: string
  results: MediaResult[]
}

type QueueResult = {
  queueId: string
  status: string
  progressLabel: string
  downloadUrl: string
  raw: unknown
}

type RetrieveResult = {
  status: string
  progressLabel: string
  result?: MediaResult | null
  raw: unknown
}

type BurnFolderStats = {
  fileCount: number
  totalBytes: number
  burnDir: string
}

type DiemBalanceSnapshot = {
  success: boolean
  diemBalance?: number | string | null
  usdBalance?: number | string | null
  diemEpochAllocation?: number | string | null
  percentRemaining?: number | string | null
  consumptionCurrency?: string | null
  canConsume?: boolean | null
  source?: string
  timestamp?: string
  warning?: string
  error?: string
}

type JobKind = 'image' | 'edit' | 'video' | 'music' | 'sfx' | 'voice' | 'transcribe'

type JobConcurrency = Record<JobKind, number>

type JobStats = Record<JobKind, {
  running: number
  queued: number
  completed: number
  failed: number
  lastMs: number | null
  oldestStartedAt: number | null
}>

type LocalJob = {
  id: string
  kind: JobKind
  label: string
  run: () => Promise<void>
}

type RemoteQueueJob = {
  id: string
  kind: 'video' | 'music' | 'sfx'
  queueId: string
  status: string
  progressLabel: string
  startedAt: number
}

type Overrides = {
  hidden: Partial<Record<ModelKind, string[]>>
  custom: Partial<Record<ModelKind, ModelRecord[]>>
}

type RecentModels = Partial<Record<ModelKind, string[]>>

const STORAGE_OVERRIDES = 'veniceMediaLocal:modelOverrides:v1'
const STORAGE_CONCURRENCY = 'veniceMediaLocal:concurrency:v1'
const STORAGE_RECENT_MODELS = 'veniceMediaLocal:recentModels:v1'
const EDIT_SOURCE_LIMIT = 3
const MAX_RECENT_MODELS = 5
const DIEM_POLL_MS = 3 * 60 * 1000
const IMAGE_ASPECT_OPTIONS = ['1:1', '4:3', '3:4', '16:9', '9:16']
const VIDEO_DURATION_OPTIONS = ['5s', '10s']
const VIDEO_RESOLUTION_OPTIONS = ['480p', '720p', '1080p']
const VIDEO_ASPECT_OPTIONS = ['16:9', '9:16', '1:1']
const VOICE_OPTIONS = ['am_eric', 'af_bella', 'af_nova']
const EMPTY_OPTIONS: string[] = []
const MAX_IMAGE_SEED = 999_999_999
const TRANSCRIBE_FILE_ACCEPT = 'audio/*,video/*,.mp3,.m4a,.wav,.webm,.flac,.ogg,.aac,.mp4,.mpeg,.mpg'
const TRANSCRIBE_FILE_EXTENSION = /\.(mp3|m4a|wav|webm|flac|ogg|aac|mp4|mpeg|mpg)$/i
const JOB_KINDS: JobKind[] = ['image', 'edit', 'video', 'music', 'sfx', 'voice', 'transcribe']
const DEFAULT_CONCURRENCY: JobConcurrency = {
  image: 4,
  edit: 2,
  video: 2,
  music: 2,
  sfx: 2,
  voice: 4,
  transcribe: 2,
}
const JOB_LABELS: Record<JobKind, string> = {
  image: 'Image',
  edit: 'Edit',
  video: 'Video',
  music: 'Music',
  sfx: 'SFX',
  voice: 'Voice',
  transcribe: 'Speech -> Text',
}
const ACTIVE_QUEUE_STATUSES = new Set(['queued', 'pending', 'processing', 'running', 'in_progress', 'created', 'submitted'])
const BURN_SEED_MASK = (1n << 64n) - 1n

const fallbackModels: ModelCache = {
  lastFetched: '',
  imageModels: [
    baseModel('gpt-image-2', 'GPT Image 2', 'image', { resolutionOptions: ['1K', '2K', '4K'] }),
    baseModel('flux-2-max', 'Flux 2 Max', 'image'),
    baseModel('qwen-image-2', 'Qwen Image 2', 'image'),
  ],
  editModels: [
    baseModel('gpt-image-2-edit', 'GPT Image 2 Edit', 'edit'),
    baseModel('qwen-image-2-edit', 'Qwen Image 2 Edit', 'edit'),
  ],
  videoModels: [
    baseModel('seedance-2-0-image-to-video', 'Seedance 2.0', 'video'),
    baseModel('seedance-2-0-text-to-video', 'Seedance 2.0 Text', 'video'),
    baseModel('wan-2-7-image-to-video', 'Wan 2.7', 'video'),
  ],
  musicModels: [
    baseModel('elevenlabs-music', 'ElevenLabs Music', 'music'),
    baseModel('stable-audio-25', 'Stable Audio 2.5', 'music'),
  ],
  sfxModels: [baseModel('elevenlabs-sound-effects-v2', 'ElevenLabs Sound Effects', 'sfx')],
  voiceModels: [
    baseModel('tts-kokoro', 'Kokoro TTS', 'voice'),
    baseModel('tts-chatterbox-hd', 'Chatterbox HD', 'voice'),
    baseModel('tts-xai-v1', 'xAI TTS', 'voice'),
  ],
  transcribeModels: [
    baseModel('fal-ai/wizper', 'fal.ai Wizper', 'transcribe', transcribeControls(true, true)),
    baseModel('nvidia/parakeet-tdt-0.6b-v3', 'NVIDIA Parakeet TDT 0.6B v3', 'transcribe', transcribeControls(false, true)),
    baseModel('openai/whisper-large-v3', 'Whisper Large v3', 'transcribe', transcribeControls(true, true)),
    baseModel('stt-xai-v1', 'xAI STT v1', 'transcribe', transcribeControls(true, true)),
    baseModel('elevenlabs/scribe-v2', 'ElevenLabs Scribe v2', 'transcribe', transcribeControls(true, true)),
  ],
}

function baseModel(id: string, name: string, kind: ModelKind, controls: Record<string, unknown> = {}): ModelRecord {
  return { id, name, kind, modes: [kind], controls }
}

function transcribeControls(supportsLanguage: boolean, supportsTimestamps: boolean): Record<string, unknown> {
  return {
    supportsLanguage,
    supportsTimestamps,
    responseFormats: ['json', 'text'],
    defaultResponseFormat: 'json',
  }
}

const modes = [
  { id: 'image', label: 'Image', icon: ImageIcon, kind: 'image' },
  { id: 'edit', label: 'Edit', icon: Scissors, kind: 'edit' },
  { id: 'video', label: 'Video', icon: Video, kind: 'video' },
  { id: 'music', label: 'Music', icon: Music, kind: 'music' },
  { id: 'sfx', label: 'SFX', icon: Volume2, kind: 'sfx' },
  { id: 'voice', label: 'Voice', icon: Mic2, kind: 'voice' },
  { id: 'transcribe', label: 'Speech -> Text', icon: FileText, kind: 'transcribe' },
  { id: 'models', label: 'Models', icon: Database },
  { id: 'settings', label: 'Settings', icon: Settings },
] as const

const themes: Array<{ id: ThemeId; name: string; colors: string[] }> = [
  { id: 'eva-dark', name: 'EVA Dark', colors: ['#313338', '#232428', '#5865f2'] },
  { id: 'pearl-white', name: 'Pearl White', colors: ['#ffffff', '#f0f0f6', '#635bdc'] },
  { id: 'abyss-teal', name: 'Abyss Teal', colors: ['#1f2a2c', '#161d1f', '#2ec4b6'] },
  { id: 'ember', name: 'Ember Glow', colors: ['#352c24', '#241e18', '#f2a65a'] },
  { id: 'mosswood', name: 'Mosswood', colors: ['#233028', '#18201b', '#4cc38a'] },
  { id: 'rose-noir', name: 'Rose Noir', colors: ['#322629', '#21191b', '#e07383'] },
]

type ModelCacheListKey = Exclude<keyof ModelCache, 'lastFetched'>

const kindToCacheKey: Record<ModelKind, ModelCacheListKey> = {
  image: 'imageModels',
  edit: 'editModels',
  video: 'videoModels',
  music: 'musicModels',
  sfx: 'sfxModels',
  voice: 'voiceModels',
  transcribe: 'transcribeModels',
}

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime()) {
    throw new Error('Tauri runtime is not active')
  }
  return invoke<T>(command, args)
}

function readOverrides(): Overrides {
  try {
    const raw = localStorage.getItem(STORAGE_OVERRIDES)
    return raw ? (JSON.parse(raw) as Overrides) : { hidden: {}, custom: {} }
  } catch {
    return { hidden: {}, custom: {} }
  }
}

function writeOverrides(value: Overrides) {
  localStorage.setItem(STORAGE_OVERRIDES, JSON.stringify(value))
}

function clampConcurrency(value: number): number {
  if (!Number.isFinite(value)) return 1
  return Math.min(12, Math.max(1, Math.trunc(value)))
}

function readConcurrency(): JobConcurrency {
  try {
    const raw = localStorage.getItem(STORAGE_CONCURRENCY)
    const parsed = raw ? JSON.parse(raw) as Partial<JobConcurrency> : {}
    return JOB_KINDS.reduce((next, kind) => {
      next[kind] = clampConcurrency(parsed[kind] ?? DEFAULT_CONCURRENCY[kind])
      return next
    }, { ...DEFAULT_CONCURRENCY })
  } catch {
    return { ...DEFAULT_CONCURRENCY }
  }
}

function writeConcurrency(value: JobConcurrency) {
  localStorage.setItem(STORAGE_CONCURRENCY, JSON.stringify(value))
}

function normalizeRecentModelIds(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  const seen = new Set<string>()
  const ids: string[] = []
  for (const entry of value) {
    if (typeof entry !== 'string') continue
    const id = entry.trim()
    if (!id || seen.has(id)) continue
    seen.add(id)
    ids.push(id)
    if (ids.length >= MAX_RECENT_MODELS) break
  }
  return ids
}

function normalizeRecentModels(value: unknown): RecentModels {
  if (!value || typeof value !== 'object') return {}
  const raw = value as Record<string, unknown>
  return JOB_KINDS.reduce((recent, kind) => {
    const ids = normalizeRecentModelIds(raw[kind])
    if (ids.length > 0) recent[kind] = ids
    return recent
  }, {} as RecentModels)
}

function readRecentModels(): RecentModels {
  try {
    const raw = localStorage.getItem(STORAGE_RECENT_MODELS)
    return raw ? normalizeRecentModels(JSON.parse(raw)) : {}
  } catch {
    return {}
  }
}

function writeRecentModels(value: RecentModels) {
  localStorage.setItem(STORAGE_RECENT_MODELS, JSON.stringify(normalizeRecentModels(value)))
}

function promoteRecentModel(value: RecentModels, kind: ModelKind, modelId: string): RecentModels {
  const id = modelId.trim()
  if (!id) return value
  return {
    ...value,
    [kind]: [id, ...(value[kind] ?? []).filter((existing) => existing !== id)].slice(0, MAX_RECENT_MODELS),
  }
}

function createJobStats(): JobStats {
  return JOB_KINDS.reduce((stats, kind) => {
    stats[kind] = { running: 0, queued: 0, completed: 0, failed: 0, lastMs: null, oldestStartedAt: null }
    return stats
  }, {} as JobStats)
}

function createJobQueues(): Record<JobKind, LocalJob[]> {
  return JOB_KINDS.reduce((queues, kind) => {
    queues[kind] = []
    return queues
  }, {} as Record<JobKind, LocalJob[]>)
}

function createJobStartQueues(): Record<JobKind, number[]> {
  return JOB_KINDS.reduce((queues, kind) => {
    queues[kind] = []
    return queues
  }, {} as Record<JobKind, number[]>)
}

function createJobCounts(value = 0): Record<JobKind, number> {
  return JOB_KINDS.reduce((counts, kind) => {
    counts[kind] = value
    return counts
  }, {} as Record<JobKind, number>)
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms))
}

function formatDate(value: string): string {
  if (!value) return 'Never'
  const parsed = new Date(value)
  if (Number.isNaN(parsed.getTime())) return 'Unknown'
  return parsed.toLocaleString()
}

function modelList(cache: ModelCache, overrides: Overrides, kind: ModelKind): ModelRecord[] {
  const hidden = new Set(overrides.hidden[kind] ?? [])
  const stock = cache[kindToCacheKey[kind]] ?? []
  const custom = overrides.custom[kind] ?? []
  const byId = new Map<string, ModelRecord>()
  for (const model of [...stock, ...custom]) {
    if (!hidden.has(model.id)) byId.set(model.id, model)
  }
  return Array.from(byId.values()).sort((a, b) => a.name.localeCompare(b.name))
}

function sortModelsByRecent(models: ModelRecord[], recentIds: string[] = []): ModelRecord[] {
  if (recentIds.length === 0) return models
  const byId = new Map(models.map((model) => [model.id, model]))
  const used = new Set<string>()
  const recent = recentIds.flatMap((id) => {
    const model = byId.get(id)
    if (!model) return []
    used.add(id)
    return [model]
  })
  return [...recent, ...models.filter((model) => !used.has(model.id))]
}

function firstModelId(models: ModelRecord[]): string {
  return models[0]?.id ?? ''
}

function fileToDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result ?? ''))
    reader.onerror = () => reject(reader.error ?? new Error('File read failed'))
    reader.readAsDataURL(file)
  })
}

function controlArray(model: ModelRecord | undefined, key: string, fallback: string[]): string[] {
  const value = model?.controls?.[key]
  return Array.isArray(value) && value.every((entry) => typeof entry === 'string') && value.length > 0
    ? value
    : fallback
}

function controlBool(model: ModelRecord | undefined, key: string, fallback: boolean): boolean {
  const value = model?.controls?.[key]
  return typeof value === 'boolean' ? value : fallback
}

function formatFileSize(bytes: number): string {
  return formatByteCount(bytes)
}

function formatByteCount(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return ''
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024)).toLocaleString()} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function formatElapsed(ms: number): string {
  const seconds = Math.max(0, ms / 1000)
  if (seconds < 60) {
    return `${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)}s`
  }

  const wholeSeconds = Math.round(seconds)
  const minutes = Math.floor(wholeSeconds / 60)
  const remainingSeconds = String(wholeSeconds % 60).padStart(2, '0')
  return `${minutes}m ${remainingSeconds}s`
}

function metadataValue(metadata: unknown, path: string[]): unknown {
  let current = metadata
  for (const key of path) {
    if (!current || typeof current !== 'object' || !(key in current)) return undefined
    current = (current as Record<string, unknown>)[key]
  }
  return current
}

function metadataText(metadata: unknown, paths: string[][]): string {
  for (const path of paths) {
    const value = metadataValue(metadata, path)
    if (typeof value === 'string' && value.trim()) return value.trim()
  }
  return ''
}

function resultModelLabel(result: MediaResult): string {
  return metadataText(result.metadata, [
    ['model'],
    ['request', 'model'],
    ['raw', 'model'],
    ['raw', 'request', 'model'],
    ['raw', 'data', 'model'],
  ])
}

function isTranscribableFile(file: File): boolean {
  if (file.type.startsWith('audio/') || file.type.startsWith('video/')) return true
  return TRANSCRIBE_FILE_EXTENSION.test(file.name)
}

function isImageFile(file: File): boolean {
  return file.type.startsWith('image/')
}

function clipboardFile(items: DataTransferItemList): File | null {
  for (const item of Array.from(items)) {
    if (item.kind === 'file' && item.type.startsWith('image/')) {
      return item.getAsFile()
    }
  }
  return null
}

function tooltipElement(target: EventTarget | null): HTMLElement | null {
  return target instanceof HTMLElement ? target.closest<HTMLElement>('[data-tooltip], [title], [aria-label]') : null
}

function tooltipText(element: HTMLElement): string {
  const text = element.dataset.tooltip || element.getAttribute('title') || element.getAttribute('aria-label') || ''
  if (element.hasAttribute('title')) {
    element.dataset.tooltip = text
    if (!element.hasAttribute('aria-label')) {
      element.setAttribute('aria-label', text)
    }
    element.removeAttribute('title')
  }
  return text.trim()
}

function classNames(...items: Array<string | false | null | undefined>): string {
  return items.filter(Boolean).join(' ')
}

function createResultGroup(results: MediaResult[], title: string): ResultGroup {
  const kind = results[0]?.kind ?? 'media'
  return {
    id: `${kind}-${Date.now()}-${Math.random().toString(16).slice(2)}`,
    kind,
    title,
    results,
  }
}

function formatBuildVersion(version: string): string {
  const trimmed = version.trim()
  const match = trimmed.match(/^(\d{4})\.(\d{1,2})\.(\d{7,8})(?:\+(g[0-9a-f]+(?:\.dirty)?))?$/i)
  if (!match) return trimmed ? `ver ${trimmed}` : 'ver dev'

  const [, year, month, patch, commit] = match
  const day = patch.slice(0, -6).padStart(2, '0')
  const builtDate = `${year}-${month.padStart(2, '0')}-${day}`
  return commit ? `ver ${builtDate} · ${commit}` : `ver ${builtDate}`
}

function numberFromValue(value: unknown): number | null {
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (typeof value === 'string' && value.trim()) {
    const parsed = Number(value)
    return Number.isFinite(parsed) ? parsed : null
  }
  return null
}

function formatDiemBalance(value: unknown): string {
  const numericValue = numberFromValue(value)
  if (numericValue === null) return ''
  return numericValue.toLocaleString(undefined, {
    maximumFractionDigits: numericValue >= 10 ? 1 : 3,
  })
}

function formatDiemPercent(snapshot: DiemBalanceSnapshot | null): string {
  if (!snapshot) return ''
  const reportedPercent = numberFromValue(snapshot.percentRemaining)
  const balance = numberFromValue(snapshot.diemBalance)
  const allocation = numberFromValue(snapshot.diemEpochAllocation)
  const derivedPercent = reportedPercent ?? (
    balance !== null && allocation !== null && allocation > 0
      ? (balance / allocation) * 100
      : null
  )
  if (derivedPercent === null || !Number.isFinite(derivedPercent)) return ''
  const clamped = Math.max(0, Math.min(100, derivedPercent))
  return `${Math.round(clamped).toLocaleString()}%`
}

function diemRailLabel(
  snapshot: DiemBalanceSnapshot | null,
  refreshing: boolean,
  keyConfigured: boolean,
): string {
  if (!keyConfigured) return 'DIEM key needed'
  if (!snapshot) return refreshing ? 'DIEM checking...' : 'DIEM --'
  if (snapshot.error) return 'DIEM ERR'
  const percent = formatDiemPercent(snapshot)
  if (percent) return `${percent} DIEM left`
  const balance = formatDiemBalance(snapshot.diemBalance)
  return balance ? `${balance} DIEM` : 'DIEM --'
}

function diemRailTitle(
  snapshot: DiemBalanceSnapshot | null,
  keyConfigured: boolean,
): string {
  if (!keyConfigured) return 'Save a Venice API key to check DIEM left.'
  if (!snapshot) return 'DIEM left checks every 3 minutes when enabled.'
  if (snapshot.error) return snapshot.error
  if (snapshot.warning) return snapshot.warning
  return snapshot.percentRemaining !== null && snapshot.percentRemaining !== undefined
    ? 'DIEM percentage left. Refreshes every 3 minutes.'
    : 'DIEM percentage unavailable; showing balance fallback.'
}

function mixBurnSeed64(value: bigint): bigint {
  let mixed = value & BURN_SEED_MASK
  mixed = ((mixed ^ (mixed >> 30n)) * 0xbf58476d1ce4e5b9n) & BURN_SEED_MASK
  mixed = ((mixed ^ (mixed >> 27n)) * 0x94d049bb133111ebn) & BURN_SEED_MASK
  return (mixed ^ (mixed >> 31n)) & BURN_SEED_MASK
}

function cryptoSeed64(): bigint {
  try {
    const values = new Uint32Array(2)
    window.crypto?.getRandomValues(values)
    return ((BigInt(values[0]) << 32n) ^ BigInt(values[1])) & BURN_SEED_MASK
  } catch {
    return 0n
  }
}

function createBurnSeed(): bigint {
  const timestamp = BigInt(Date.now())
  const precisionTime = BigInt(Math.floor(performance.now() * 1000))
  return mixBurnSeed64((timestamp << 20n) ^ precisionTime ^ cryptoSeed64())
}

function formatBurnSeed(seed: bigint): string {
  return `0x${(seed & BURN_SEED_MASK).toString(16).padStart(16, '0')}`
}

export function App() {
  const [mode, setMode] = useState<ModeId>('image')
  const [models, setModels] = useState<ModelCache>(fallbackModels)
  const [settings, setSettings] = useState<AppSettings>({ theme: 'eva-dark', outputDir: '', showDiemBalance: false })
  const [keyConfigured, setKeyConfigured] = useState(false)
  const [buildVersion, setBuildVersion] = useState('')
  const burnSeedRef = useRef(createBurnSeed())
  const [burnSeed, setBurnSeed] = useState(() => formatBurnSeed(burnSeedRef.current))
  const [apiKey, setApiKey] = useState('')
  const [status, setStatus] = useState('')
  const [error, setError] = useState('')
  const [tooltip, setTooltip] = useState('')
  const [loading, setLoading] = useState(false)
  const [actionStartedAt, setActionStartedAt] = useState<number | null>(null)
  const [elapsedMs, setElapsedMs] = useState(0)
  const [lastActionMs, setLastActionMs] = useState<number | null>(null)
  const [overrides, setOverrides] = useState<Overrides>(() => readOverrides())
  const [recentModels, setRecentModels] = useState<RecentModels>(() => readRecentModels())
  const [concurrency, setConcurrency] = useState<JobConcurrency>(() => readConcurrency())
  const [jobStats, setJobStats] = useState<JobStats>(() => createJobStats())
  const [remoteQueues, setRemoteQueues] = useState<RemoteQueueJob[]>([])
  const [diemSnapshot, setDiemSnapshot] = useState<DiemBalanceSnapshot | null>(null)
  const [diemRefreshing, setDiemRefreshing] = useState(false)
  const [jobClock, setJobClock] = useState(0)
  const [resultGroups, setResultGroups] = useState<ResultGroup[]>([])
  const jobQueuesRef = useRef<Record<JobKind, LocalJob[]>>(createJobQueues())
  const runningJobsRef = useRef<Record<JobKind, number>>(createJobCounts())
  const runningJobStartsRef = useRef<Record<JobKind, number[]>>(createJobStartQueues())
  const concurrencyRef = useRef<JobConcurrency>(concurrency)
  const pointerSeedRef = useRef(0n)
  const pointerFrameRef = useRef<number | null>(null)

  const rawImageModels = useMemo(() => modelList(models, overrides, 'image'), [models, overrides])
  const rawEditModels = useMemo(() => modelList(models, overrides, 'edit'), [models, overrides])
  const rawVideoModels = useMemo(() => modelList(models, overrides, 'video'), [models, overrides])
  const rawMusicModels = useMemo(() => modelList(models, overrides, 'music'), [models, overrides])
  const rawSfxModels = useMemo(() => modelList(models, overrides, 'sfx'), [models, overrides])
  const rawVoiceModels = useMemo(() => modelList(models, overrides, 'voice'), [models, overrides])
  const rawTranscribeModels = useMemo(() => modelList(models, overrides, 'transcribe'), [models, overrides])
  const imageModels = useMemo(() => sortModelsByRecent(rawImageModels, recentModels.image), [rawImageModels, recentModels.image])
  const editModels = useMemo(() => sortModelsByRecent(rawEditModels, recentModels.edit), [rawEditModels, recentModels.edit])
  const videoModels = useMemo(() => sortModelsByRecent(rawVideoModels, recentModels.video), [rawVideoModels, recentModels.video])
  const musicModels = useMemo(() => sortModelsByRecent(rawMusicModels, recentModels.music), [rawMusicModels, recentModels.music])
  const sfxModels = useMemo(() => sortModelsByRecent(rawSfxModels, recentModels.sfx), [rawSfxModels, recentModels.sfx])
  const voiceModels = useMemo(() => sortModelsByRecent(rawVoiceModels, recentModels.voice), [rawVoiceModels, recentModels.voice])
  const transcribeModels = useMemo(() => sortModelsByRecent(rawTranscribeModels, recentModels.transcribe), [rawTranscribeModels, recentModels.transcribe])

  const [imageModel, setImageModel] = useState('')
  const [editModel, setEditModel] = useState('')
  const [videoModel, setVideoModel] = useState('')
  const [musicModel, setMusicModel] = useState('')
  const [sfxModel, setSfxModel] = useState('')
  const [voiceModel, setVoiceModel] = useState('')
  const [transcribeModel, setTranscribeModel] = useState('')

  const [prompt, setPrompt] = useState('')
  const [negativePrompt, setNegativePrompt] = useState('')
  const [aspectRatio, setAspectRatio] = useState('1:1')
  const [imageResolution, setImageResolution] = useState('')
  const [imageFormat, setImageFormat] = useState('webp')
  const [variants, setVariants] = useState(1)
  const [steps, setSteps] = useState(28)
  const [cfgScale, setCfgScale] = useState(7.5)
  const [seed, setSeed] = useState('')
  const [lockSeed, setLockSeed] = useState(false)
  const [randomSeed, setRandomSeed] = useState(true)
  const [hideWatermark, setHideWatermark] = useState(true)

  const [sourceImage, setSourceImage] = useState('')
  const [editSourceImages, setEditSourceImages] = useState<string[]>(() => Array(EDIT_SOURCE_LIMIT).fill(''))
  const [editResolution, setEditResolution] = useState('')
  const [videoDuration, setVideoDuration] = useState('5s')
  const [videoResolution, setVideoResolution] = useState('720p')
  const [videoAspectRatio, setVideoAspectRatio] = useState('16:9')

  const [lyrics, setLyrics] = useState('')
  const [audioDuration, setAudioDuration] = useState('30')
  const [instrumental, setInstrumental] = useState(false)
  const [lyricsOptimizer, setLyricsOptimizer] = useState(false)

  const [voiceText, setVoiceText] = useState('The quick brown fox jumps over the lazy dog.')
  const [voiceName, setVoiceName] = useState('')
  const [voiceSpeed, setVoiceSpeed] = useState(1)
  const [voiceFormat, setVoiceFormat] = useState('mp3')
  const [voiceStyle, setVoiceStyle] = useState('')

  const [transcribeAudio, setTranscribeAudio] = useState('')
  const [transcribeFileName, setTranscribeFileName] = useState('')
  const [transcribeMimeType, setTranscribeMimeType] = useState('')
  const [transcribeFileSize, setTranscribeFileSize] = useState(0)
  const [transcribeLanguage, setTranscribeLanguage] = useState('')
  const [transcribeResponseFormat, setTranscribeResponseFormat] = useState('json')
  const [transcribeTimestamps, setTranscribeTimestamps] = useState(false)

  const [managerKind, setManagerKind] = useState<ModelKind>('image')
  const [customModelId, setCustomModelId] = useState('')
  const [customModelName, setCustomModelName] = useState('')

  function updateBurnSeed(extra: bigint) {
    const next = mixBurnSeed64(burnSeedRef.current ^ extra ^ BigInt(Date.now()))
    burnSeedRef.current = next
    setBurnSeed(formatBurnSeed(next))
  }

  function rememberModelUse(kind: ModelKind, modelId: string) {
    if (!modelId.trim()) return
    setRecentModels((existing) => {
      const next = promoteRecentModel(existing, kind, modelId)
      writeRecentModels(next)
      return next
    })
  }

  function mixPointerBurnSeed(event: MouseEvent<HTMLDivElement>) {
    const x = BigInt(Math.max(0, Math.trunc(event.clientX)))
    const y = BigInt(Math.max(0, Math.trunc(event.clientY)))
    const movementX = BigInt(Math.max(0, Math.trunc(event.movementX + 8192)))
    const movementY = BigInt(Math.max(0, Math.trunc(event.movementY + 8192)))
    pointerSeedRef.current ^= (x << 48n) ^ (y << 32n) ^ (movementX << 16n) ^ movementY ^ BigInt(Date.now())

    if (pointerFrameRef.current !== null) return
    pointerFrameRef.current = window.requestAnimationFrame(() => {
      pointerFrameRef.current = null
      const pointerSeed = pointerSeedRef.current
      pointerSeedRef.current = 0n
      updateBurnSeed(pointerSeed)
    })
  }

  useEffect(() => {
    call<AppState>('get_app_state')
      .then((state) => {
        setSettings({
          ...state.settings,
          showDiemBalance: Boolean(state.settings.showDiemBalance),
        })
        setKeyConfigured(state.keyConfigured)
        setModels(state.models)
        setBuildVersion(state.buildVersion)
      })
      .catch(() => {
        setLastActionMs(null)
        setStatus('Preview mode')
      })
  }, [])

  useEffect(() => {
    const timer = window.setInterval(() => {
      updateBurnSeed(BigInt(Date.now()))
    }, 1000)

    return () => window.clearInterval(timer)
  }, [])

  useEffect(() => {
    return () => {
      if (pointerFrameRef.current !== null) {
        window.cancelAnimationFrame(pointerFrameRef.current)
      }
    }
  }, [])

  useEffect(() => {
    document.body.className = `theme-${settings.theme}`
  }, [settings.theme])

  useEffect(() => {
    if (!settings.showDiemBalance || !keyConfigured) {
      setDiemSnapshot(null)
      setDiemRefreshing(false)
      return
    }

    let cancelled = false

    async function refreshDiemBalance() {
      setDiemRefreshing(true)
      try {
        const snapshot = await call<DiemBalanceSnapshot>('get_diem_balance')
        if (!cancelled) setDiemSnapshot(snapshot)
      } catch (err) {
        if (!cancelled) {
          setDiemSnapshot((current) => ({
            ...(current ?? { success: false }),
            success: false,
            error: err instanceof Error ? err.message : String(err),
          }))
        }
      } finally {
        if (!cancelled) setDiemRefreshing(false)
      }
    }

    refreshDiemBalance()
    const intervalId = window.setInterval(refreshDiemBalance, DIEM_POLL_MS)

    return () => {
      cancelled = true
      window.clearInterval(intervalId)
    }
  }, [keyConfigured, settings.showDiemBalance])

  useEffect(() => {
    if (!imageModel && imageModels.length > 0) setImageModel(firstModelId(imageModels))
    if (!editModel && editModels.length > 0) setEditModel(firstModelId(editModels))
    if (!videoModel && videoModels.length > 0) setVideoModel(firstModelId(videoModels))
    if (!musicModel && musicModels.length > 0) setMusicModel(firstModelId(musicModels))
    if (!sfxModel && sfxModels.length > 0) setSfxModel(firstModelId(sfxModels))
    if (!voiceModel && voiceModels.length > 0) setVoiceModel(firstModelId(voiceModels))
    if (!transcribeModel && transcribeModels.length > 0) setTranscribeModel(firstModelId(transcribeModels))
  }, [editModel, editModels, imageModel, imageModels, musicModel, musicModels, sfxModel, sfxModels, transcribeModel, transcribeModels, videoModel, videoModels, voiceModel, voiceModels])

  useEffect(() => {
    concurrencyRef.current = concurrency
    writeConcurrency(concurrency)
    for (const kind of JOB_KINDS) {
      pumpJobs(kind)
    }
  }, [concurrency])

  useEffect(() => {
    if (actionStartedAt === null) return
    setElapsedMs(Date.now() - actionStartedAt)
    const timer = window.setInterval(() => {
      setElapsedMs(Date.now() - actionStartedAt)
    }, 250)
    return () => window.clearInterval(timer)
  }, [actionStartedAt])

  const currentImageModel = imageModels.find((model) => model.id === imageModel)
  const currentEditModel = editModels.find((model) => model.id === editModel)
  const currentVideoModel = videoModels.find((model) => model.id === videoModel)
  const currentMusicModel = musicModels.find((model) => model.id === musicModel)
  const currentSfxModel = sfxModels.find((model) => model.id === sfxModel)
  const currentVoiceModel = voiceModels.find((model) => model.id === voiceModel)
  const currentTranscribeModel = transcribeModels.find((model) => model.id === transcribeModel)
  const imageRatios = controlArray(currentImageModel, 'sizeOptions', IMAGE_ASPECT_OPTIONS)
  const imageResolutions = controlArray(currentImageModel, 'resolutionOptions', EMPTY_OPTIONS)
  const editResolutions = controlArray(currentEditModel, 'resolutionOptions', EMPTY_OPTIONS)
  const selectedAspectRatio = imageRatios.includes(aspectRatio) ? aspectRatio : imageRatios[0] ?? '1:1'
  const selectedImageResolution = imageResolutions.includes(imageResolution) ? imageResolution : ''
  const selectedEditResolution = editResolutions.includes(editResolution) ? editResolution : editResolutions[0] ?? ''
  const videoDurations = controlArray(currentVideoModel, 'durationOptions', VIDEO_DURATION_OPTIONS)
  const videoResolutions = controlArray(currentVideoModel, 'resolutionOptions', VIDEO_RESOLUTION_OPTIONS)
  const videoRatios = controlArray(currentVideoModel, 'aspectRatioOptions', VIDEO_ASPECT_OPTIONS)
  const supportsMusicDuration = controlBool(currentMusicModel, 'supportsDurationSeconds', true)
  const supportsMusicLyrics = controlBool(currentMusicModel, 'supportsLyrics', true)
  const supportsMusicInstrumental = controlBool(currentMusicModel, 'supportsInstrumental', true)
  const supportsMusicLyricsOptimizer = controlBool(currentMusicModel, 'supportsLyricsOptimizer', true)
  const supportsSfxDuration = controlBool(currentSfxModel, 'supportsDurationSeconds', true)
  const voiceOptions = controlArray(currentVoiceModel, 'voices', VOICE_OPTIONS)
  const transcribeResponseFormats = controlArray(currentTranscribeModel, 'responseFormats', ['json', 'text'])
  const supportsTranscribeLanguage = controlBool(currentTranscribeModel, 'supportsLanguage', true)
  const supportsTranscribeTimestamps = controlBool(currentTranscribeModel, 'supportsTimestamps', true)
  const selectedTranscribeResponseFormat = transcribeResponseFormats.includes(transcribeResponseFormat) ? transcribeResponseFormat : transcribeResponseFormats[0] ?? 'json'
  const resultCount = resultGroups.reduce((total, group) => total + group.results.length, 0)
  const resultFilePaths = resultGroups.flatMap((group) => group.results.map((result) => result.filePath))
  const hasEditSource = editSourceImages.some(Boolean)
  const runningJobCount = JOB_KINDS.reduce((total, kind) => total + jobStats[kind].running, 0)
  const queuedJobCount = JOB_KINDS.reduce((total, kind) => total + jobStats[kind].queued, 0)
  const hasRunningJobs = runningJobCount > 0
  const activeElapsedLabel = actionStartedAt !== null ? formatElapsed(elapsedMs) : ''
  const completedElapsedLabel = actionStartedAt === null && lastActionMs !== null ? `Took ${formatElapsed(lastActionMs)}` : ''
  const jobNow = jobClock || Date.now()
  const diemLabel = diemRailLabel(diemSnapshot, diemRefreshing, keyConfigured)
  const diemTitle = diemRailTitle(diemSnapshot, keyConfigured)

  useEffect(() => {
    if (runningJobCount === 0 && remoteQueues.length === 0) return
    const timer = window.setInterval(() => {
      setJobClock(Date.now())
    }, 1000)
    return () => window.clearInterval(timer)
  }, [runningJobCount, remoteQueues.length])

  function refreshJobCounts() {
    setJobStats((existing) => {
      const next = { ...existing } as JobStats
      for (const kind of JOB_KINDS) {
        next[kind] = {
          ...next[kind],
          running: runningJobsRef.current[kind],
          queued: jobQueuesRef.current[kind].length,
          oldestStartedAt: runningJobStartsRef.current[kind].length > 0
            ? Math.min(...runningJobStartsRef.current[kind])
            : null,
        }
      }
      return next
    })
  }

  function updateConcurrency(kind: JobKind, value: number) {
    const nextValue = clampConcurrency(value)
    setConcurrency((existing) => ({ ...existing, [kind]: nextValue }))
  }

  function enqueueJob(kind: JobKind, label: string, run: () => Promise<void>) {
    const job: LocalJob = {
      id: `${kind}-${Date.now()}-${Math.random().toString(16).slice(2)}`,
      kind,
      label,
      run,
    }
    jobQueuesRef.current[kind].push(job)
    refreshJobCounts()
    setError('')
    setLastActionMs(null)
    setStatus(`${label} queued`)
    pumpJobs(kind)
  }

  function pumpJobs(kind: JobKind) {
    const limit = clampConcurrency(concurrencyRef.current[kind])
    const queue = jobQueuesRef.current[kind]
    while (runningJobsRef.current[kind] < limit && queue.length > 0) {
      const job = queue.shift()
      if (!job) break
      runningJobsRef.current[kind] += 1
      refreshJobCounts()
      void runQueuedJob(job)
    }
  }

  async function runQueuedJob(job: LocalJob) {
    const startedAt = Date.now()
    runningJobStartsRef.current[job.kind].push(startedAt)
    refreshJobCounts()
    setError('')
    setLastActionMs(null)
    setStatus(`${job.label} running`)
    try {
      await job.run()
      const duration = Date.now() - startedAt
      setLastActionMs(duration)
      setStatus(`${job.label} completed`)
      setJobStats((existing) => ({
        ...existing,
        [job.kind]: {
          ...existing[job.kind],
          completed: existing[job.kind].completed + 1,
          lastMs: duration,
        },
      }))
    } catch (err) {
      const duration = Date.now() - startedAt
      setLastActionMs(duration)
      setError(err instanceof Error ? err.message : String(err))
      setStatus(`${job.label} failed`)
      setJobStats((existing) => ({
        ...existing,
        [job.kind]: {
          ...existing[job.kind],
          failed: existing[job.kind].failed + 1,
          lastMs: duration,
        },
      }))
    } finally {
      runningJobsRef.current[job.kind] = Math.max(0, runningJobsRef.current[job.kind] - 1)
      runningJobStartsRef.current[job.kind] = runningJobStartsRef.current[job.kind].filter((value) => value !== startedAt)
      refreshJobCounts()
      pumpJobs(job.kind)
    }
  }

  function remoteQueueId(kind: 'video' | 'music' | 'sfx', queueId: string) {
    return `${kind}-${queueId}`
  }

  async function waitForQueuedMedia(kind: 'video' | 'music' | 'sfx', queued: QueueResult, model: string): Promise<MediaResult> {
    const id = remoteQueueId(kind, queued.queueId)
    const endpoint = kind === 'video' ? 'retrieve_video' : 'retrieve_audio'
    const retrieveKind = kind === 'video' ? 'video' : 'audio'
    setRemoteQueues((existing) => [
      { id, kind, queueId: queued.queueId, status: queued.status, progressLabel: queued.progressLabel, startedAt: Date.now() },
      ...existing.filter((entry) => entry.id !== id),
    ])

    let currentStatus = queued.status
    let currentProgress = queued.progressLabel
    let downloadUrl = queued.downloadUrl

    while (ACTIVE_QUEUE_STATUSES.has(currentStatus.toLowerCase())) {
      await sleep(7000)
      const output = await call<RetrieveResult>(endpoint, {
        request: {
          queueId: queued.queueId,
          kind: retrieveKind,
          model,
          downloadUrl,
        },
      })
      currentStatus = output.status
      currentProgress = output.progressLabel
      downloadUrl = downloadUrl || ''
      setRemoteQueues((existing) =>
        existing.map((entry) =>
          entry.id === id ? { ...entry, status: currentStatus, progressLabel: currentProgress } : entry,
        ),
      )
      if (output.result) {
        setRemoteQueues((existing) => existing.filter((entry) => entry.id !== id))
        return output.result
      }
    }

    setRemoteQueues((existing) => existing.filter((entry) => entry.id !== id))
    throw new Error(`${JOB_LABELS[kind]} queue ended with status ${currentStatus}`)
  }

  async function runAction<T>(label: string, action: () => Promise<T>): Promise<T | null> {
    const startedAt = Date.now()
    setError('')
    setStatus(label)
    setLoading(true)
    setActionStartedAt(startedAt)
    setElapsedMs(0)
    setLastActionMs(null)
    try {
      const value = await action()
      setStatus('Ready')
      return value
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      setStatus('Needs attention')
      return null
    } finally {
      const duration = Date.now() - startedAt
      setElapsedMs(duration)
      setLastActionMs(duration)
      setActionStartedAt(null)
      setLoading(false)
    }
  }

  async function saveKey(event: FormEvent) {
    event.preventDefault()
    const ok = await runAction('Saving key', () => call<boolean>('save_api_key', { apiKey }))
    if (ok) {
      setApiKey('')
      setKeyConfigured(true)
    }
  }

  async function clearKey() {
    const ok = await runAction('Clearing key', () => call<boolean>('clear_api_key'))
    if (ok !== null) {
      setKeyConfigured(false)
      setDiemSnapshot(null)
    }
  }

  async function refreshModelCatalog() {
    const cache = await runAction('Refreshing models', () => call<ModelCache>('refresh_models'))
    if (cache) setModels(cache)
  }

  async function persistSettings(next: AppSettings) {
    setSettings(next)
    await call<AppSettings>('save_settings', { request: next }).catch(() => undefined)
  }

  async function chooseOutputFolder() {
    const selected = await runAction('Choosing output folder', () =>
      open({
        directory: true,
        multiple: false,
        defaultPath: settings.outputDir || undefined,
        title: 'Choose output folder',
      }),
    )
    if (typeof selected === 'string') {
      await persistSettings({ ...settings, outputDir: selected })
    }
  }

  function randomImageSeed(): number {
    return Math.floor(Math.random() * (MAX_IMAGE_SEED + 1))
  }

  function seedForImageRequest(): number {
    const trimmed = seed.trim()
    if (randomSeed || !trimmed) {
      const nextSeed = randomImageSeed()
      setSeed(String(nextSeed))
      return nextSeed
    }

    const parsed = Number(trimmed)
    if (!Number.isFinite(parsed)) {
      throw new Error('Seed must be a number')
    }

    const normalized = Math.trunc(parsed)
    if (normalized < 0 || normalized > MAX_IMAGE_SEED) {
      throw new Error(`Seed must be between 0 and ${MAX_IMAGE_SEED}`)
    }

    if (String(normalized) !== trimmed) {
      setSeed(String(normalized))
    }
    return normalized
  }

  function generateImage(event: FormEvent) {
    event.preventDefault()
    let requestSeed: number
    try {
      requestSeed = seedForImageRequest()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    const request = {
      model: imageModel,
      prompt,
      negativePrompt,
      aspectRatio: selectedAspectRatio,
      resolution: selectedImageResolution || null,
      variants,
      steps,
      cfgScale,
      seed: requestSeed,
      hideWatermark,
      format: imageFormat,
    }

    const seedLabel = `seed ${requestSeed}`
    enqueueJob('image', `Image generation · ${seedLabel}`, async () => {
      const startedAt = Date.now()
      const output = await call<MediaResult[]>('generate_image', { request })
      rememberModelUse('image', request.model)
      setResultGroups((existing) => [createResultGroup(output, `Images · ${seedLabel} · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function editImage() {
    const images = editSourceImages.map((source) => source.trim()).filter(Boolean)
    if (images.length === 0) {
      setError('Choose at least one image first')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }
    if (!prompt.trim()) {
      setError('Enter an edit prompt first')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    const request = {
      model: editModel,
      prompt,
      images,
      resolution: selectedEditResolution || null,
    }

    enqueueJob('edit', 'Image edit/combine', async () => {
      const startedAt = Date.now()
      const output = await call<MediaResult>('multi_edit_image', { request })
      rememberModelUse('edit', request.model)
      setResultGroups((existing) => [createResultGroup([output], `Edit / Combine · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function removeBackground() {
    const backgroundSource = editSourceImages.find(Boolean) ?? ''
    if (!backgroundSource) {
      setError('Choose a source image first')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    enqueueJob('edit', 'Background removal', async () => {
      const startedAt = Date.now()
      const output = await call<MediaResult>('remove_background', {
        request: {
          sourceImage: backgroundSource,
        },
      })
      rememberModelUse('edit', editModel)
      setResultGroups((existing) => [createResultGroup([output], `Background Removed · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  async function moveResultFilesToBurn(paths: string[], label: string) {
    const uniquePaths = Array.from(new Set(paths.filter(Boolean)))
    if (uniquePaths.length === 0) return
    if (!window.confirm(`Move ${label} to the burn folder? This removes it from Results but does not burn it yet.`)) return

    const moved = await runAction('Moving files to burn folder', () => call<string[]>('move_media_files_to_burn', { paths: uniquePaths }))
    if (!moved) return

    const movedSet = new Set(moved)
    const burnStats = await call<BurnFolderStats>('get_burn_folder_stats').catch(() => null)
    const burnCount = burnStats?.fileCount ?? 0
    const burnCountLabel = burnStats ? ` Burn folder now has ${burnCount.toLocaleString()} file${burnCount === 1 ? '' : 's'}.` : ''
    setStatus(`Moved ${moved.length.toLocaleString()} file${moved.length === 1 ? '' : 's'} to the burn folder.${burnCountLabel}`)
    setResultGroups((existing) =>
      existing
        .map((group) => ({
          ...group,
          results: group.results.filter((result) => !movedSet.has(result.filePath)),
        }))
        .filter((group) => group.results.length > 0),
    )
  }

  async function burnFolder() {
    const stats = await runAction('Checking burn folder', () => call<BurnFolderStats>('get_burn_folder_stats'))
    if (!stats) return
    if (stats.fileCount === 0) {
      setLastActionMs(null)
      setStatus('Burn folder is empty. Use a trash button to move generated files there first.')
      return
    }

    const sizeLabel = formatByteCount(stats.totalBytes) || '0 KB'
    const confirmed = window.confirm(
      `Burn ${stats.fileCount.toLocaleString()} file${stats.fileCount === 1 ? '' : 's'} (${sizeLabel}) from the burn folder?\n\nCorrupts and deletes files from the burn folder, bypassing the Recycle Bin. Successfully overwritten files should be unreadable if recovered.\n\nBurn seed: ${burnSeed}\n${stats.burnDir}`,
    )
    if (!confirmed) {
      setLastActionMs(null)
      setStatus('Ready')
      return
    }

    const burned = await runAction('Burning files', () => call<BurnFolderStats>('burn_folder', { seed: burnSeed }))
    if (burned) {
      setStatus(`Burned ${burned.fileCount.toLocaleString()} file${burned.fileCount === 1 ? '' : 's'}`)
    }
  }

  async function openOutputFolder() {
    const folder = await runAction('Opening output folder', () => call<string>('open_output_folder'))
    if (folder) {
      setStatus(`Opened output folder: ${folder}`)
    }
  }

  function clearResults() {
    setResultGroups([])
  }

  async function loadSourceImage(file: File) {
    const dataUrl = await fileToDataUrl(file)
    setSourceImage(dataUrl)
  }

  async function loadTranscribeFile(file: File) {
    const dataUrl = await fileToDataUrl(file)
    setTranscribeAudio(dataUrl)
    setTranscribeFileName(file.name || 'audio')
    setTranscribeMimeType(file.type || 'application/octet-stream')
    setTranscribeFileSize(file.size || 0)
  }

  function clearTranscribeFile() {
    setTranscribeAudio('')
    setTranscribeFileName('')
    setTranscribeMimeType('')
    setTranscribeFileSize(0)
  }

  async function loadEditSourceImage(index: number, file: File) {
    const dataUrl = await fileToDataUrl(file)
    setEditSourceImages((existing) => existing.map((source, sourceIndex) => (sourceIndex === index ? dataUrl : source)))
  }

  function setEditSourceImage(index: number, dataUrl: string) {
    setEditSourceImages((existing) => existing.map((source, sourceIndex) => (sourceIndex === index ? dataUrl : source)))
  }

  function clearEditSourceImage(index: number) {
    setEditSourceImages((existing) => existing.map((source, sourceIndex) => (sourceIndex === index ? '' : source)))
  }

  function sendResultToEdit(result: MediaResult) {
    setEditSourceImage(0, result.dataUrl)
    setMode('edit')
    setStatus('Image loaded into edit slot 1')
    setLastActionMs(null)
  }

  function showTooltipFromTarget(target: EventTarget | null) {
    const element = tooltipElement(target)
    const text = element ? tooltipText(element) : ''
    if (text) setTooltip(text)
  }

  function handleTooltipOut(event: MouseEvent<HTMLDivElement>) {
    const element = tooltipElement(event.target)
    const related = event.relatedTarget instanceof HTMLElement ? event.relatedTarget : null
    if (element && related && element.contains(related)) return
    if (!related || !tooltipElement(related)) setTooltip('')
  }

  function handleTooltipBlur(event: FocusEvent<HTMLDivElement>) {
    const related = event.relatedTarget instanceof HTMLElement ? event.relatedTarget : null
    if (!related || !tooltipElement(related)) setTooltip('')
  }

  function queueVideo(event: FormEvent) {
    event.preventDefault()
    const request = {
      model: videoModel,
      prompt,
      negativePrompt,
      sourceImage,
      duration: videoDuration,
      resolution: videoResolution,
      aspectRatio: videoAspectRatio,
    }

    enqueueJob('video', 'Video generation', async () => {
      const startedAt = Date.now()
      const queued = await call<QueueResult>('queue_video', { request })
      rememberModelUse('video', request.model)
      const result = await waitForQueuedMedia('video', queued, request.model)
      setResultGroups((existing) => [createResultGroup([result], `Video · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function queueAudio(event: FormEvent, kind: 'music' | 'sfx') {
    event.preventDefault()
    const model = kind === 'music' ? musicModel : sfxModel
    const supportsDuration = kind === 'music' ? supportsMusicDuration : supportsSfxDuration
    const useLyricsOptimizer = kind === 'music' && supportsMusicLyricsOptimizer && lyricsOptimizer
    const request: Record<string, string | boolean> = {
      model,
      prompt,
    }
    if (supportsDuration && audioDuration.trim()) request.durationSeconds = audioDuration.trim()
    if (kind === 'music' && supportsMusicLyrics && lyrics.trim() && !useLyricsOptimizer) request.lyricsPrompt = lyrics.trim()
    if (kind === 'music' && supportsMusicInstrumental && instrumental) request.forceInstrumental = true
    if (useLyricsOptimizer) request.lyricsOptimizer = true

    enqueueJob(kind, `${JOB_LABELS[kind]} generation`, async () => {
      const startedAt = Date.now()
      const queued = await call<QueueResult>('queue_audio', { request })
      rememberModelUse(kind, model)
      const result = await waitForQueuedMedia(kind, queued, model)
      setResultGroups((existing) => [createResultGroup([result], `${JOB_LABELS[kind]} · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function generateVoice(event: FormEvent) {
    event.preventDefault()
    const request = {
      model: voiceModel,
      input: voiceText,
      voice: voiceName,
      speed: voiceSpeed,
      responseFormat: voiceFormat,
      stylePrompt: voiceStyle,
    }

    enqueueJob('voice', 'Voice generation', async () => {
      const startedAt = Date.now()
      const output = await call<MediaResult>('generate_speech', { request })
      rememberModelUse('voice', request.model)
      setResultGroups((existing) => [createResultGroup([output], `Voice · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function transcribeSpeech(event: FormEvent) {
    event.preventDefault()
    if (!transcribeAudio) {
      setError('Choose an audio or video file to transcribe')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    const request = {
      model: transcribeModel,
      audio: transcribeAudio,
      fileName: transcribeFileName || 'audio',
      mimeType: transcribeMimeType,
      responseFormat: selectedTranscribeResponseFormat,
      timestamps: supportsTranscribeTimestamps ? transcribeTimestamps : false,
      language: supportsTranscribeLanguage ? transcribeLanguage : '',
    }

    enqueueJob('transcribe', 'Speech transcription', async () => {
      const startedAt = Date.now()
      const output = await call<MediaResult>('transcribe_audio', { request })
      rememberModelUse('transcribe', request.model)
      setResultGroups((existing) => [createResultGroup([output], `Speech -> Text · ${formatElapsed(Date.now() - startedAt)}`), ...existing])
    })
  }

  function addCustomModel(event: FormEvent) {
    event.preventDefault()
    const id = customModelId.trim()
    if (!id) return
    const record: ModelRecord = {
      id,
      name: customModelName.trim() || id,
      kind: managerKind,
      modes: [managerKind],
      controls: {},
    }
    const next: Overrides = {
      hidden: overrides.hidden,
      custom: {
        ...overrides.custom,
        [managerKind]: [...(overrides.custom[managerKind] ?? []).filter((entry) => entry.id !== id), record],
      },
    }
    setOverrides(next)
    writeOverrides(next)
    setCustomModelId('')
    setCustomModelName('')
  }

  function hideModel(kind: ModelKind, id: string) {
    const next: Overrides = {
      custom: {
        ...overrides.custom,
        [kind]: (overrides.custom[kind] ?? []).filter((entry) => entry.id !== id),
      },
      hidden: {
        ...overrides.hidden,
        [kind]: Array.from(new Set([...(overrides.hidden[kind] ?? []), id])),
      },
    }
    setOverrides(next)
    writeOverrides(next)
  }

  return (
    <div
      className="app-shell"
      onMouseOver={(event) => showTooltipFromTarget(event.target)}
      onMouseOut={handleTooltipOut}
      onMouseMove={mixPointerBurnSeed}
      onFocus={(event) => showTooltipFromTarget(event.target)}
      onBlur={handleTooltipBlur}
    >
      <aside className="rail">
        <nav className="mode-nav">
          {modes.map((item) => {
            const Icon = item.icon
            return (
              <button
                key={item.id}
                className={classNames('mode-button', mode === item.id && 'active')}
                onClick={() => setMode(item.id)}
                type="button"
                title={item.label}
              >
                <Icon size={18} />
                <span>{item.label}</span>
              </button>
            )
          })}
        </nav>
        <div className="rail-footer">
          {settings.showDiemBalance && (
            <div className={classNames('diem-status', diemSnapshot?.error && 'error', diemSnapshot?.warning && 'warning')} title={diemTitle}>
              {diemLabel}
            </div>
          )}
          <div className="rail-build" title={formatBuildVersion(buildVersion)}>
            {formatBuildVersion(buildVersion)}
          </div>
        </div>
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div>
            <h1>{modes.find((item) => item.id === mode)?.label}</h1>
            <p>{keyConfigured ? 'API key ready' : 'API key missing'} · Models: {formatDate(models.lastFetched)}</p>
          </div>
          <div className="topbar-actions">
            <button className="icon-button" type="button" onClick={() => setMode('settings')} title="Settings">
              <Settings size={18} />
            </button>
          </div>
        </header>

        {error && <div className="notice error">{error}</div>}
        <div className="notice">
          {loading || hasRunningJobs ? <Loader2 className="spin" size={16} /> : null}
          <span className="notice-message">{status || 'Ready'}</span>
          {(runningJobCount > 0 || queuedJobCount > 0) && (
            <span className="elapsed-pill">{runningJobCount} running · {queuedJobCount} queued</span>
          )}
          {activeElapsedLabel && <span className="elapsed-pill">{activeElapsedLabel}</span>}
          {completedElapsedLabel && <span className="elapsed-pill">{completedElapsedLabel}</span>}
          {tooltip && <span className="tooltip-hint">{tooltip}</span>}
        </div>

        <section className="content-grid">
          <div className="tool-surface">
            {mode === 'image' && (
              <form onSubmit={generateImage} className="tool-form">
                <ModelSelect label="Model" value={imageModel} onChange={setImageModel} models={imageModels} recentModelIds={recentModels.image} />
                <PromptArea value={prompt} onChange={setPrompt} />
                <PromptArea label="Negative prompt" value={negativePrompt} onChange={setNegativePrompt} rows={3} />
                <div className="control-grid">
                  <SelectField label="Aspect" value={selectedAspectRatio} onChange={setAspectRatio} options={imageRatios} />
                  {imageResolutions.length > 0 && (
                    <label className="field compact">
                      <span>Resolution</span>
                      <select value={selectedImageResolution} onChange={(event) => setImageResolution(event.target.value)}>
                        <option value="">Auto</option>
                        {imageResolutions.map((option) => <option key={option} value={option}>{option}</option>)}
                      </select>
                    </label>
                  )}
                  <SelectField label="Format" value={imageFormat} onChange={setImageFormat} options={['webp', 'png', 'jpeg']} />
                  <NumberField label="Variants" value={variants} min={1} max={4} step={1} onChange={setVariants} />
                  <NumberField label="Steps" value={steps} min={1} max={80} step={1} onChange={setSteps} />
                  <NumberField label="CFG" value={cfgScale} min={1} max={20} step={0.5} onChange={setCfgScale} />
                  <TextField label="Seed" value={seed} onChange={setSeed} />
                  <NumberField label="Concurrent" value={concurrency.image} min={1} max={12} step={1} onChange={(value) => updateConcurrency('image', value)} />
                </div>
                <label className="toggle-row">
                  <input type="checkbox" checked={hideWatermark} onChange={(event) => setHideWatermark(event.target.checked)} />
                  <span>Hide watermark</span>
                </label>
                <label className="toggle-row">
                  <input
                    type="checkbox"
                    checked={lockSeed}
                    onChange={(event) => {
                      setLockSeed(event.target.checked)
                      if (event.target.checked) setRandomSeed(false)
                    }}
                  />
                  <span>Lock seed</span>
                </label>
                <label className="toggle-row">
                  <input
                    type="checkbox"
                    checked={randomSeed}
                    onChange={(event) => {
                      setRandomSeed(event.target.checked)
                      if (event.target.checked) setLockSeed(false)
                    }}
                  />
                  <span>Random seed</span>
                </label>
                <QueueSummary label="Image queue" stats={jobStats.image} limit={concurrency.image} now={jobNow} />
                <SubmitButton busy={jobStats.image.running > 0} icon={Wand2}>Generate Image</SubmitButton>
              </form>
            )}

            {mode === 'edit' && (
              <form className="tool-form">
                <ModelSelect label="Model" value={editModel} onChange={setEditModel} models={editModels} recentModelIds={recentModels.edit} />
                <div className="edit-source-layout">
                  <SourcePicker
                    className="edit-source-main"
                    label="Image 1"
                    source={editSourceImages[0]}
                    onFile={(file) => loadEditSourceImage(0, file)}
                    onSource={(value) => setEditSourceImage(0, value)}
                    onClear={() => clearEditSourceImage(0)}
                  />
                  <div className="edit-source-row">
                    {[1, 2].map((index) => (
                      <SourcePicker
                        key={index}
                        label={`Ref Image ${index + 1}`}
                        source={editSourceImages[index]}
                        onFile={(file) => loadEditSourceImage(index, file)}
                        onSource={(value) => setEditSourceImage(index, value)}
                        onClear={() => clearEditSourceImage(index)}
                      />
                    ))}
                  </div>
                </div>
                <PromptArea value={prompt} onChange={setPrompt} />
                <div className="control-grid">
                  {editResolutions.length > 0 && (
                    <SelectField label="Resolution" value={selectedEditResolution} onChange={setEditResolution} options={editResolutions} />
                  )}
                  <NumberField label="Concurrent" value={concurrency.edit} min={1} max={12} step={1} onChange={(value) => updateConcurrency('edit', value)} />
                </div>
                <div className="action-row">
                  <button className="secondary-action" type="button" onClick={editImage} disabled={!hasEditSource}>
                    {jobStats.edit.running > 0 ? <Loader2 className="spin" size={18} /> : <Scissors size={18} />}
                    Edit / Combine
                  </button>
                  <button className="primary-action" type="button" onClick={removeBackground} disabled={!hasEditSource}>
                    {jobStats.edit.running > 0 ? <Loader2 className="spin" size={18} /> : <Eraser size={18} />}
                    Remove Background
                  </button>
                </div>
                <QueueSummary label="Edit queue" stats={jobStats.edit} limit={concurrency.edit} now={jobNow} />
              </form>
            )}

            {mode === 'video' && (
              <form onSubmit={queueVideo} className="tool-form">
                <ModelSelect label="Model" value={videoModel} onChange={setVideoModel} models={videoModels} recentModelIds={recentModels.video} />
                <SourcePicker label="Source Image" source={sourceImage} onFile={loadSourceImage} onSource={setSourceImage} />
                <PromptArea label="Motion prompt" value={prompt} onChange={setPrompt} />
                <PromptArea label="Negative prompt" value={negativePrompt} onChange={setNegativePrompt} rows={3} />
                <div className="control-grid">
                  <SelectField label="Duration" value={videoDuration} onChange={setVideoDuration} options={videoDurations} />
                  <SelectField label="Resolution" value={videoResolution} onChange={setVideoResolution} options={videoResolutions} />
                  <SelectField label="Aspect" value={videoAspectRatio} onChange={setVideoAspectRatio} options={videoRatios} />
                  <NumberField label="Concurrent" value={concurrency.video} min={1} max={12} step={1} onChange={(value) => updateConcurrency('video', value)} />
                </div>
                <QueueSummary label="Video queue" stats={jobStats.video} limit={concurrency.video} now={jobNow} />
                <SubmitButton busy={jobStats.video.running > 0} icon={Video}>Queue Video</SubmitButton>
              </form>
            )}

            {mode === 'music' && (
              <form onSubmit={(event) => queueAudio(event, 'music')} className="tool-form">
                <ModelSelect label="Model" value={musicModel} onChange={setMusicModel} models={musicModels} recentModelIds={recentModels.music} />
                <PromptArea value={prompt} onChange={setPrompt} />
                {supportsMusicLyrics && !supportsMusicLyricsOptimizer && <PromptArea label="Lyrics" value={lyrics} onChange={setLyrics} rows={4} />}
                {supportsMusicLyrics && supportsMusicLyricsOptimizer && !lyricsOptimizer && <PromptArea label="Lyrics" value={lyrics} onChange={setLyrics} rows={4} />}
                <div className="control-grid">
                  {supportsMusicDuration && <TextField label="Duration seconds" value={audioDuration} onChange={setAudioDuration} />}
                  <NumberField label="Concurrent" value={concurrency.music} min={1} max={12} step={1} onChange={(value) => updateConcurrency('music', value)} />
                </div>
                {supportsMusicInstrumental && (
                  <label className="toggle-row">
                    <input type="checkbox" checked={instrumental} onChange={(event) => setInstrumental(event.target.checked)} />
                    <span>Instrumental</span>
                  </label>
                )}
                {supportsMusicLyricsOptimizer && (
                  <label className="toggle-row">
                    <input type="checkbox" checked={lyricsOptimizer} onChange={(event) => setLyricsOptimizer(event.target.checked)} />
                    <span>Auto lyrics</span>
                  </label>
                )}
                <QueueSummary label="Music queue" stats={jobStats.music} limit={concurrency.music} now={jobNow} />
                <SubmitButton busy={jobStats.music.running > 0} icon={Music}>Queue Music</SubmitButton>
              </form>
            )}

            {mode === 'sfx' && (
              <form onSubmit={(event) => queueAudio(event, 'sfx')} className="tool-form">
                <ModelSelect label="Model" value={sfxModel} onChange={setSfxModel} models={sfxModels} recentModelIds={recentModels.sfx} />
                <PromptArea value={prompt} onChange={setPrompt} />
                <div className="control-grid">
                  {supportsSfxDuration && <TextField label="Duration seconds" value={audioDuration} onChange={setAudioDuration} />}
                  <NumberField label="Concurrent" value={concurrency.sfx} min={1} max={12} step={1} onChange={(value) => updateConcurrency('sfx', value)} />
                </div>
                <QueueSummary label="SFX queue" stats={jobStats.sfx} limit={concurrency.sfx} now={jobNow} />
                <SubmitButton busy={jobStats.sfx.running > 0} icon={Volume2}>Queue SFX</SubmitButton>
              </form>
            )}

            {mode === 'voice' && (
              <form onSubmit={generateVoice} className="tool-form">
                <ModelSelect label="Model" value={voiceModel} onChange={setVoiceModel} models={voiceModels} recentModelIds={recentModels.voice} />
                <PromptArea label="Text" value={voiceText} onChange={setVoiceText} rows={7} />
                <div className="control-grid">
                  <SelectField label="Voice" value={voiceName || voiceOptions[0] || ''} onChange={setVoiceName} options={voiceOptions} />
                  <NumberField label="Speed" value={voiceSpeed} min={0.25} max={4} step={0.05} onChange={setVoiceSpeed} />
                  <SelectField label="Format" value={voiceFormat} onChange={setVoiceFormat} options={['mp3', 'wav', 'flac', 'aac', 'opus']} />
                  <NumberField label="Concurrent" value={concurrency.voice} min={1} max={12} step={1} onChange={(value) => updateConcurrency('voice', value)} />
                </div>
                <PromptArea label="Style prompt" value={voiceStyle} onChange={setVoiceStyle} rows={3} />
                <QueueSummary label="Voice queue" stats={jobStats.voice} limit={concurrency.voice} now={jobNow} />
                <SubmitButton busy={jobStats.voice.running > 0} icon={Mic2}>Generate Voice</SubmitButton>
              </form>
            )}

            {mode === 'transcribe' && (
              <form onSubmit={transcribeSpeech} className="tool-form">
                <ModelSelect label="Model" value={transcribeModel} onChange={setTranscribeModel} models={transcribeModels} recentModelIds={recentModels.transcribe} />
                <TranscribeFilePicker
                  fileName={transcribeFileName}
                  mimeType={transcribeMimeType}
                  fileSize={transcribeFileSize}
                  onFile={loadTranscribeFile}
                  onClear={clearTranscribeFile}
                />
                <div className="control-grid">
                  <SelectField label="Format" value={selectedTranscribeResponseFormat} onChange={setTranscribeResponseFormat} options={transcribeResponseFormats} />
                  {supportsTranscribeLanguage && (
                    <TextField label="Language" value={transcribeLanguage} onChange={setTranscribeLanguage} />
                  )}
                  <NumberField label="Concurrent" value={concurrency.transcribe} min={1} max={12} step={1} onChange={(value) => updateConcurrency('transcribe', value)} />
                </div>
                {supportsTranscribeTimestamps && (
                  <label className="toggle-row">
                    <input type="checkbox" checked={transcribeTimestamps} onChange={(event) => setTranscribeTimestamps(event.target.checked)} />
                    <span>Include timestamps</span>
                  </label>
                )}
                <QueueSummary label="Speech queue" stats={jobStats.transcribe} limit={concurrency.transcribe} now={jobNow} />
                <SubmitButton busy={jobStats.transcribe.running > 0} icon={FileText}>Transcribe</SubmitButton>
              </form>
            )}

            {mode === 'models' && (
              <div className="tool-form">
                <div className="inline-header">
                  <h2>Model Manager</h2>
                  <button className="secondary-action" type="button" onClick={refreshModelCatalog}>
                    <RefreshCw size={16} />
                    Get Latest From Venice
                  </button>
                </div>
                <form className="model-add" onSubmit={addCustomModel}>
                  <SelectField label="Type" value={managerKind} onChange={(value) => setManagerKind(value as ModelKind)} options={['image', 'edit', 'video', 'music', 'sfx', 'voice', 'transcribe']} />
                  <TextField label="Model ID" value={customModelId} onChange={setCustomModelId} />
                  <TextField label="Name" value={customModelName} onChange={setCustomModelName} />
                  <button className="icon-button add-button" type="submit" title="Add model">
                    <Plus size={18} />
                  </button>
                </form>
                <ModelTable kind={managerKind} models={modelList(models, overrides, managerKind)} onHide={hideModel} />
              </div>
            )}

            {mode === 'settings' && (
              <div className="tool-form">
                <form onSubmit={saveKey} className="settings-block">
                  <h2>API Key</h2>
                  <div className="key-row">
                    <input
                      value={apiKey}
                      onChange={(event) => setApiKey(event.target.value)}
                      type="password"
                      autoComplete="off"
                      placeholder={keyConfigured ? 'Stored in OS credential store' : 'Venice API key'}
                    />
                    <button className="icon-button" type="submit" title="Save API key">
                      <KeyRound size={18} />
                    </button>
                    <button className="icon-button danger" type="button" onClick={clearKey} title="Clear API key">
                      <Trash2 size={18} />
                    </button>
                  </div>
                </form>

                <div className="settings-block">
                  <h2>Theme</h2>
                  <div className="theme-grid">
                    {themes.map((theme) => (
                      <button
                        type="button"
                        key={theme.id}
                        className={classNames('theme-button', settings.theme === theme.id && 'active')}
                        onClick={() => persistSettings({ ...settings, theme: theme.id })}
                      >
                        <span className="swatches">
                          {theme.colors.map((color) => <span key={color} style={{ background: color }} />)}
                        </span>
                        <span>{theme.name}</span>
                      </button>
                    ))}
                  </div>
                </div>

                <div className="settings-block">
                  <h2>Output</h2>
                  <div className="key-row">
                    <input
                      value={settings.outputDir}
                      readOnly
                      placeholder="Desktop\\VeniceMedia"
                    />
                    <button className="icon-button" type="button" onClick={chooseOutputFolder} title="Choose output folder">
                      <FolderOpen size={18} />
                    </button>
                  </div>
                </div>

                <div className="settings-block">
                  <h2>DIEM</h2>
                  <label className="toggle-row">
                    <input
                      type="checkbox"
                      checked={settings.showDiemBalance}
                      onChange={(event) => persistSettings({ ...settings, showDiemBalance: event.target.checked })}
                    />
                    <span>Show DIEM left</span>
                  </label>
                  <small className="field-help">Refreshes once every 3 minutes. Shows percent left when Venice returns an epoch allocation, otherwise falls back to the DIEM balance.</small>
                </div>

                <div className="settings-block">
                  <h2>Burn Seed</h2>
                  <label className="field">
                    <span>Live entropy</span>
                    <input value={burnSeed} readOnly />
                    <small className="field-help">Updates every second and mixes mouse movement before burning files.</small>
                  </label>
                </div>
              </div>
            )}
          </div>

          <aside className="result-panel">
            {(runningJobCount > 0 || queuedJobCount > 0 || remoteQueues.length > 0) && (
              <div className="queue-stack">
                {(runningJobCount > 0 || queuedJobCount > 0) && (
                  <div className="queue-panel">
                    <div>
                      <span className="eyebrow">Local Queue</span>
                      <strong>{runningJobCount} running · {queuedJobCount} waiting</strong>
                      <small>{JOB_KINDS.filter((kind) => jobStats[kind].running > 0 || jobStats[kind].queued > 0).map((kind) => `${JOB_LABELS[kind]} ${jobStats[kind].running}/${jobStats[kind].queued}`).join(' · ')}</small>
                    </div>
                  </div>
                )}
                {remoteQueues.map((entry) => (
                  <div className="queue-panel" key={entry.id}>
                    <div>
                      <span className="eyebrow">{JOB_LABELS[entry.kind]} Venice Queue</span>
                      <strong>{entry.queueId}</strong>
                      <small>{entry.progressLabel || entry.status} · {formatElapsed(jobNow - entry.startedAt)}</small>
                    </div>
                    <Loader2 className="spin" size={18} />
                  </div>
                ))}
              </div>
            )}
            <div className="result-header">
              <div className="result-title">
                <h2>Results</h2>
                <span>{resultCount}</span>
              </div>
              <div className="result-actions">
                <button className="icon-button compact burn-button" type="button" onClick={burnFolder} title="Burn folder">
                  <BlackFlameIcon size={16} />
                </button>
                {resultCount > 0 && (
                  <>
                    <button className="icon-button compact" type="button" onClick={clearResults} title="Clear results">
                      <Eraser size={16} />
                    </button>
                    <button className="icon-button compact danger" type="button" onClick={() => moveResultFilesToBurn(resultFilePaths, 'all generated files')} title="Move all generated files to burn folder">
                      <Trash2 size={16} />
                    </button>
                  </>
                )}
                <button className="icon-button compact" type="button" onClick={openOutputFolder} title="Open output folder">
                  <FolderOpen size={16} />
                </button>
              </div>
            </div>
            <div className="results">
              {resultGroups.length === 0 && <div className="empty-results">No media yet</div>}
              {resultGroups.map((group) => (
                <section className="result-group" key={group.id}>
                  <div className="result-group-header">
                    <strong>{group.title}</strong>
                    <div className="result-actions">
                      <span>{group.results.length}</span>
                      <button className="icon-button compact danger" type="button" onClick={() => moveResultFilesToBurn(group.results.map((result) => result.filePath), 'this generated set')} title="Move generated set to burn folder">
                        <Trash2 size={14} />
                      </button>
                    </div>
                  </div>
                  <div className={classNames('result-group-grid', group.kind !== 'images' && 'single')}>
                    {group.results.map((result) => (
                      <ResultCard
                        key={`${result.id}-${result.filePath}`}
                        result={result}
                        onDelete={() => moveResultFilesToBurn([result.filePath], 'this file')}
                        onEdit={result.mimeType.startsWith('image/') ? () => sendResultToEdit(result) : undefined}
                      />
                    ))}
                  </div>
                </section>
              ))}
            </div>
          </aside>
        </section>
      </main>
    </div>
  )
}

function ResultCard({ result, onDelete, onEdit }: { result: MediaResult; onDelete: () => void; onEdit?: () => void }) {
  const modelLabel = resultModelLabel(result)

  return (
    <article className="result-item">
      {result.mimeType.startsWith('image/') && (
        <img
          src={result.dataUrl}
          alt={result.name}
          draggable
          onDragStart={(event) => {
            event.dataTransfer.setData('application/x-venice-image', result.dataUrl)
            event.dataTransfer.setData('text/plain', result.dataUrl)
            event.dataTransfer.setData('text/uri-list', result.dataUrl)
            event.dataTransfer.effectAllowed = 'copy'
          }}
        />
      )}
      {result.mimeType.startsWith('video/') && <video src={result.dataUrl} controls />}
      {result.mimeType.startsWith('audio/') && <audio src={result.dataUrl} controls />}
      {result.mimeType.startsWith('text/') && <pre className="transcript-preview">{result.text}</pre>}
      <div className="result-meta">
        <strong>{result.name}</strong>
        {modelLabel && <small>Model: {modelLabel}</small>}
        <small>{result.filePath}</small>
        <div className="result-links">
          <a href={result.dataUrl} download={result.name}>
            <Download size={16} />
            Save
          </a>
          {onEdit && (
            <button className="link-button" type="button" onClick={onEdit}>
              <Scissors size={14} />
              Edit
            </button>
          )}
          <button className="link-button danger" type="button" onClick={onDelete}>
            <Trash2 size={14} />
            Move to burn
          </button>
        </div>
      </div>
    </article>
  )
}

function BlackFlameIcon({ size = 16 }: { size?: number }) {
  return (
    <svg
      className="burn-icon"
      aria-hidden="true"
      viewBox="0 0 24 24"
      width={size}
      height={size}
      focusable="false"
    >
      <path d="M12.6 2.1c.4 3.1-.7 5-2.2 6.8-1.3 1.6-2.8 3.3-2.1 6 .3 1.1 1 2.1 2 2.7-.4-1.8.1-3.2 1.3-4.5.8-.8 1.5-1.7 1.7-3.1 2.5 2 3.6 4.1 3 6.4-.4 1.7-1.8 3-3.8 3.5 4.4-.2 7.4-3.1 7.4-7.2 0-3.1-1.9-5.5-4.2-7.6-.9-.8-1.8-1.6-3.1-3z" />
      <path d="M7.2 10.2c-2.1 1.4-3.1 3.2-3.1 5.2 0 3.1 2.4 5.5 6.1 5.9-2.1-1.2-3.3-3-3.4-5.2-.1-1.9.6-3.4.4-5.9z" />
    </svg>
  )
}

function ModelSelect({
  label,
  value,
  onChange,
  models,
  recentModelIds = [],
}: {
  label: string
  value: string
  onChange: (value: string) => void
  models: ModelRecord[]
  recentModelIds?: string[]
}) {
  const recentSet = new Set(recentModelIds)
  return (
    <label className="field">
      <span>{label}</span>
      <select value={value} onChange={(event) => onChange(event.target.value)}>
        {models.map((model) => (
          <option key={model.id} value={model.id}>
            {model.name || model.id}{recentSet.has(model.id) ? ' (Recently Used)' : ''}
          </option>
        ))}
      </select>
    </label>
  )
}

function PromptArea({
  label = 'Prompt',
  value,
  onChange,
  rows = 6,
}: {
  label?: string
  value: string
  onChange: (value: string) => void
  rows?: number
}) {
  return (
    <label className="field">
      <span>{label}</span>
      <textarea value={value} rows={rows} onChange={(event) => onChange(event.target.value)} />
    </label>
  )
}

function SelectField({
  label,
  value,
  onChange,
  options,
}: {
  label: string
  value: string
  onChange: (value: string) => void
  options: string[]
}) {
  return (
    <label className="field compact">
      <span>{label}</span>
      <select value={value} onChange={(event) => onChange(event.target.value)}>
        {options.map((option) => <option key={option} value={option}>{option}</option>)}
      </select>
    </label>
  )
}

function NumberField({
  label,
  value,
  onChange,
  min,
  max,
  step,
}: {
  label: string
  value: number
  onChange: (value: number) => void
  min: number
  max: number
  step: number
}) {
  return (
    <label className="field compact">
      <span>{label}</span>
      <input type="number" value={value} min={min} max={max} step={step} onChange={(event) => onChange(Number(event.target.value))} />
    </label>
  )
}

function TextField({
  label,
  value,
  onChange,
}: {
  label: string
  value: string
  onChange: (value: string) => void
}) {
  return (
    <label className="field compact">
      <span>{label}</span>
      <input value={value} onChange={(event) => onChange(event.target.value)} />
    </label>
  )
}

function TranscribeFilePicker({
  fileName,
  mimeType,
  fileSize,
  onFile,
  onClear,
}: {
  fileName: string
  mimeType: string
  fileSize: number
  onFile: (file: File) => void | Promise<void>
  onClear: () => void
}) {
  const [dragging, setDragging] = useState(false)

  function chooseFile(file: File | undefined) {
    if (!file || !isTranscribableFile(file)) return
    void onFile(file)
  }

  function handleInput(event: ChangeEvent<HTMLInputElement>) {
    chooseFile(event.target.files?.[0])
    event.currentTarget.value = ''
  }

  function handleDrop(event: DragEvent<HTMLLabelElement>) {
    event.preventDefault()
    setDragging(false)
    chooseFile(Array.from(event.dataTransfer.files).find(isTranscribableFile))
  }

  const sizeLabel = formatFileSize(fileSize)

  return (
    <div className="transcribe-picker">
      <label
        className={classNames('transcribe-dropzone', dragging && 'dragging')}
        onDragEnter={(event) => {
          event.preventDefault()
          setDragging(true)
        }}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={() => setDragging(false)}
        onDrop={handleDrop}
      >
        <input type="file" accept={TRANSCRIBE_FILE_ACCEPT} onChange={handleInput} />
        {fileName ? (
          <span className="transcribe-file-card">
            <strong>{fileName}</strong>
            <small>{[sizeLabel, mimeType].filter(Boolean).join(' · ')}</small>
          </span>
        ) : (
          <span className="transcribe-empty">
            <FileText size={24} />
            <strong>Audio / Video File</strong>
            <small>Drop or browse mp3, m4a, wav, webm, flac, ogg, aac, mp4, mpeg</small>
          </span>
        )}
      </label>
      {fileName && (
        <button className="secondary-action" type="button" onClick={onClear}>
          Clear File
        </button>
      )}
    </div>
  )
}

function SourcePicker({
  className,
  label,
  source,
  onFile,
  onSource,
  onClear,
}: {
  className?: string
  label: string
  source: string
  onFile: (file: File) => void | Promise<void>
  onSource?: (source: string) => void
  onClear?: () => void
}) {
  const [dragging, setDragging] = useState(false)
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null)
  const inputRef = useRef<HTMLInputElement | null>(null)

  function chooseFile(file: File | undefined) {
    if (!file || !isImageFile(file)) return
    void onFile(file)
  }

  function chooseSource(value: string) {
    const trimmed = value.trim()
    if (!trimmed.startsWith('data:image/')) return
    onSource?.(trimmed)
  }

  async function pasteFromClipboard() {
    setContextMenu(null)
    try {
      const items = await navigator.clipboard?.read?.()
      for (const item of items ?? []) {
        const imageType = item.types.find((type) => type.startsWith('image/'))
        if (!imageType) continue
        const blob = await item.getType(imageType)
        chooseFile(new File([blob], `${label.toLowerCase().replace(/\s+/g, '-')}.${imageType.split('/')[1] || 'png'}`, { type: imageType }))
        return
      }
    } catch {
      // The normal paste event still works when direct clipboard reads are blocked.
    }
  }

  function handleInput(event: ChangeEvent<HTMLInputElement>) {
    chooseFile(event.target.files?.[0])
    event.currentTarget.value = ''
  }

  function handleDrop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault()
    setDragging(false)
    chooseFile(Array.from(event.dataTransfer.files).find(isImageFile))
    chooseSource(event.dataTransfer.getData('application/x-venice-image') || event.dataTransfer.getData('text/uri-list') || event.dataTransfer.getData('text/plain'))
  }

  function handlePaste(event: ClipboardEvent<HTMLDivElement>) {
    const file = clipboardFile(event.clipboardData.items)
    if (file) {
      event.preventDefault()
      chooseFile(file)
      return
    }

    const text = event.clipboardData.getData('text/plain')
    if (text.trim().startsWith('data:image/')) {
      event.preventDefault()
      chooseSource(text)
    }
  }

  function handleContextMenu(event: MouseEvent<HTMLDivElement>) {
    event.preventDefault()
    setContextMenu({ x: event.clientX, y: event.clientY })
  }

  return (
    <div className={classNames('source-picker', className)}>
      <input ref={inputRef} className="source-file-input" type="file" accept="image/*" onChange={handleInput} />
      <div
        role="button"
        tabIndex={0}
        className={classNames('source-input', dragging && 'dragging')}
        onDragEnter={(event) => {
          event.preventDefault()
          setDragging(true)
        }}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={() => setDragging(false)}
        onDrop={handleDrop}
        onPaste={handlePaste}
        onContextMenu={handleContextMenu}
        onClick={() => setContextMenu(null)}
      >
        {source && <img src={source} alt={label} />}
        <span className={classNames('source-label', source && 'loaded')}>
          {!source && <ImageIcon size={18} />}
          {label}
          {!source && <small>Drag/Drop/Paste</small>}
        </span>
      </div>
      <button className="icon-button compact source-browse" type="button" onClick={() => inputRef.current?.click()} title={`Browse for ${label}`}>
        <FolderOpen size={14} />
      </button>
      {source && onClear && (
        <button className="icon-button compact source-clear" type="button" onClick={onClear} title={`Clear ${label}`}>
          <Trash2 size={14} />
        </button>
      )}
      {contextMenu && (
        <div className="source-context-menu" style={{ left: contextMenu.x, top: contextMenu.y }}>
          <button type="button" onClick={pasteFromClipboard}>
            Paste
          </button>
        </div>
      )}
    </div>
  )
}

function SubmitButton({
  busy = false,
  disabled = false,
  icon: Icon,
  children,
}: {
  busy?: boolean
  disabled?: boolean
  icon: LucideIcon
  children: ReactNode
}) {
  return (
    <button className="primary-action" type="submit" disabled={disabled}>
      {busy ? <Loader2 className="spin" size={18} /> : <Icon size={18} />}
      {children}
    </button>
  )
}

function QueueSummary({ label, stats, limit, now }: { label: string; stats: JobStats[JobKind]; limit: number; now: number }) {
  const runningLabel = stats.oldestStartedAt !== null ? ` · running ${formatElapsed(now - stats.oldestStartedAt)}` : ''
  return (
    <div className="queue-summary">
      <span>{label}</span>
      <strong>{stats.running}/{limit} running</strong>
      <small>{stats.queued} queued{runningLabel} · {stats.completed} done · {stats.failed} failed{stats.lastMs !== null ? ` · last ${formatElapsed(stats.lastMs)}` : ''}</small>
    </div>
  )
}

function ModelTable({
  kind,
  models,
  onHide,
}: {
  kind: ModelKind
  models: ModelRecord[]
  onHide: (kind: ModelKind, id: string) => void
}) {
  return (
    <div className="model-table">
      {models.map((model) => (
        <div className="model-row" key={model.id}>
          <div>
            <strong>{model.name}</strong>
            <small>{model.id}</small>
          </div>
          <button className="icon-button danger" type="button" onClick={() => onHide(kind, model.id)} title="Remove model">
            <Trash2 size={16} />
          </button>
        </div>
      ))}
    </div>
  )
}
