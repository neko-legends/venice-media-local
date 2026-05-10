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
import { ChangeEvent, DragEvent, FormEvent, ReactNode, useEffect, useMemo, useState } from 'react'

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
}

type AppState = {
  settings: AppSettings
  keyConfigured: boolean
  models: ModelCache
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

type Overrides = {
  hidden: Partial<Record<ModelKind, string[]>>
  custom: Partial<Record<ModelKind, ModelRecord[]>>
}

const STORAGE_OVERRIDES = 'veniceMediaLocal:modelOverrides:v1'
const EDIT_SOURCE_LIMIT = 3
const IMAGE_ASPECT_OPTIONS = ['1:1', '4:3', '3:4', '16:9', '9:16']
const VIDEO_DURATION_OPTIONS = ['5s', '10s']
const VIDEO_RESOLUTION_OPTIONS = ['480p', '720p', '1080p']
const VIDEO_ASPECT_OPTIONS = ['16:9', '9:16', '1:1']
const VOICE_OPTIONS = ['am_eric', 'af_bella', 'af_nova']
const EMPTY_OPTIONS: string[] = []
const MAX_IMAGE_SEED = 999_999_999
const TRANSCRIBE_FILE_ACCEPT = 'audio/*,video/*,.mp3,.m4a,.wav,.webm,.flac,.ogg,.aac,.mp4,.mpeg'

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

function isTranscribableFile(file: File): boolean {
  if (file.type.startsWith('audio/') || file.type.startsWith('video/')) return true
  return /\.(mp3|m4a|wav|webm|flac|ogg|aac|mp4|mpeg)$/i.test(file.name)
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

export function App() {
  const [mode, setMode] = useState<ModeId>('image')
  const [models, setModels] = useState<ModelCache>(fallbackModels)
  const [settings, setSettings] = useState<AppSettings>({ theme: 'eva-dark', outputDir: '' })
  const [keyConfigured, setKeyConfigured] = useState(false)
  const [apiKey, setApiKey] = useState('')
  const [status, setStatus] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [actionStartedAt, setActionStartedAt] = useState<number | null>(null)
  const [elapsedMs, setElapsedMs] = useState(0)
  const [lastActionMs, setLastActionMs] = useState<number | null>(null)
  const [refreshingModels, setRefreshingModels] = useState(false)
  const [overrides, setOverrides] = useState<Overrides>(() => readOverrides())
  const [resultGroups, setResultGroups] = useState<ResultGroup[]>([])
  const [queue, setQueue] = useState<QueueResult | null>(null)

  const imageModels = useMemo(() => modelList(models, overrides, 'image'), [models, overrides])
  const editModels = useMemo(() => modelList(models, overrides, 'edit'), [models, overrides])
  const videoModels = useMemo(() => modelList(models, overrides, 'video'), [models, overrides])
  const musicModels = useMemo(() => modelList(models, overrides, 'music'), [models, overrides])
  const sfxModels = useMemo(() => modelList(models, overrides, 'sfx'), [models, overrides])
  const voiceModels = useMemo(() => modelList(models, overrides, 'voice'), [models, overrides])
  const transcribeModels = useMemo(() => modelList(models, overrides, 'transcribe'), [models, overrides])

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
  const [randomSeed, setRandomSeed] = useState(false)
  const [hideWatermark, setHideWatermark] = useState(true)

  const [sourceImage, setSourceImage] = useState('')
  const [editSourceImages, setEditSourceImages] = useState<string[]>(() => Array(EDIT_SOURCE_LIMIT).fill(''))
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

  useEffect(() => {
    call<AppState>('get_app_state')
      .then((state) => {
        setSettings(state.settings)
        setKeyConfigured(state.keyConfigured)
        setModels(state.models)
      })
      .catch(() => {
        setLastActionMs(null)
        setStatus('Preview mode')
      })
  }, [])

  useEffect(() => {
    document.body.className = `theme-${settings.theme}`
  }, [settings.theme])

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
    if (!queue) return
    if (!['queued', 'pending', 'processing', 'running', 'in_progress'].includes(queue.status.toLowerCase())) return

    const timer = window.setInterval(() => {
      void pollQueue()
    }, 7000)
    return () => window.clearInterval(timer)
  }, [queue])

  useEffect(() => {
    if (actionStartedAt === null) return
    setElapsedMs(Date.now() - actionStartedAt)
    const timer = window.setInterval(() => {
      setElapsedMs(Date.now() - actionStartedAt)
    }, 250)
    return () => window.clearInterval(timer)
  }, [actionStartedAt])

  const currentImageModel = imageModels.find((model) => model.id === imageModel)
  const currentVideoModel = videoModels.find((model) => model.id === videoModel)
  const currentVoiceModel = voiceModels.find((model) => model.id === voiceModel)
  const currentTranscribeModel = transcribeModels.find((model) => model.id === transcribeModel)
  const imageRatios = controlArray(currentImageModel, 'sizeOptions', IMAGE_ASPECT_OPTIONS)
  const imageResolutions = controlArray(currentImageModel, 'resolutionOptions', EMPTY_OPTIONS)
  const selectedAspectRatio = imageRatios.includes(aspectRatio) ? aspectRatio : imageRatios[0] ?? '1:1'
  const selectedImageResolution = imageResolutions.includes(imageResolution) ? imageResolution : ''
  const videoDurations = controlArray(currentVideoModel, 'durationOptions', VIDEO_DURATION_OPTIONS)
  const videoResolutions = controlArray(currentVideoModel, 'resolutionOptions', VIDEO_RESOLUTION_OPTIONS)
  const videoRatios = controlArray(currentVideoModel, 'aspectRatioOptions', VIDEO_ASPECT_OPTIONS)
  const voiceOptions = controlArray(currentVoiceModel, 'voices', VOICE_OPTIONS)
  const transcribeResponseFormats = controlArray(currentTranscribeModel, 'responseFormats', ['json', 'text'])
  const supportsTranscribeLanguage = controlBool(currentTranscribeModel, 'supportsLanguage', true)
  const supportsTranscribeTimestamps = controlBool(currentTranscribeModel, 'supportsTimestamps', true)
  const selectedTranscribeResponseFormat = transcribeResponseFormats.includes(transcribeResponseFormat) ? transcribeResponseFormat : transcribeResponseFormats[0] ?? 'json'
  const resultCount = resultGroups.reduce((total, group) => total + group.results.length, 0)
  const resultFilePaths = resultGroups.flatMap((group) => group.results.map((result) => result.filePath))
  const hasEditSource = editSourceImages.some(Boolean)
  const activeElapsedLabel = actionStartedAt !== null ? formatElapsed(elapsedMs) : ''
  const completedElapsedLabel = actionStartedAt === null && lastActionMs !== null ? `Took ${formatElapsed(lastActionMs)}` : ''

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
    if (ok !== null) setKeyConfigured(false)
  }

  async function refreshModelCatalog() {
    setRefreshingModels(true)
    const cache = await runAction('Refreshing models', () => call<ModelCache>('refresh_models'))
    if (cache) setModels(cache)
    setRefreshingModels(false)
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

  function seedForImageRequest(): number | null {
    const trimmed = seed.trim()
    if (randomSeed || (lockSeed && !trimmed)) {
      const nextSeed = randomImageSeed()
      setSeed(String(nextSeed))
      return nextSeed
    }

    if (!trimmed) return null

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

  async function generateImage(event: FormEvent) {
    event.preventDefault()
    const output = await runAction('Generating image', () =>
      call<MediaResult[]>('generate_image', {
        request: {
          model: imageModel,
          prompt,
          negativePrompt,
          aspectRatio: selectedAspectRatio,
          resolution: selectedImageResolution || null,
          variants,
          steps,
          cfgScale,
          seed: seedForImageRequest(),
          hideWatermark,
          format: imageFormat,
        },
      }),
    )
    if (output) setResultGroups((existing) => [createResultGroup(output, 'Images'), ...existing])
  }

  async function removeBackground() {
    const backgroundSource = editSourceImages.find(Boolean) ?? ''
    if (!backgroundSource) {
      setError('Choose a source image first')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    const output = await runAction('Removing background', () =>
      call<MediaResult>('remove_background', {
        request: {
          sourceImage: backgroundSource,
        },
      }),
    )
    if (output) setResultGroups((existing) => [createResultGroup([output], 'Background Removed'), ...existing])
  }

  async function moveResultFilesToBurn(paths: string[], label: string) {
    const uniquePaths = Array.from(new Set(paths.filter(Boolean)))
    if (uniquePaths.length === 0) return
    if (!window.confirm(`Move ${label} to the burn folder? This removes it from Results but does not burn it yet.`)) return

    const moved = await runAction('Moving files to burn folder', () => call<string[]>('move_media_files_to_burn', { paths: uniquePaths }))
    if (!moved) return

    const movedSet = new Set(moved)
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
      setStatus('Burn folder is empty')
      return
    }

    const sizeLabel = formatByteCount(stats.totalBytes) || '0 KB'
    const confirmed = window.confirm(
      `Burn ${stats.fileCount.toLocaleString()} file${stats.fileCount === 1 ? '' : 's'} (${sizeLabel}) from the burn folder?\n\nCorrupts and deletes files from the burn folder, bypassing the Recycle Bin. Successfully overwritten files should be unreadable if recovered.\n\n${stats.burnDir}`,
    )
    if (!confirmed) {
      setLastActionMs(null)
      setStatus('Ready')
      return
    }

    const burned = await runAction('Burning files', () => call<BurnFolderStats>('burn_folder'))
    if (burned) {
      setStatus(`Burned ${burned.fileCount.toLocaleString()} file${burned.fileCount === 1 ? '' : 's'}`)
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

  function clearEditSourceImage(index: number) {
    setEditSourceImages((existing) => existing.map((source, sourceIndex) => (sourceIndex === index ? '' : source)))
  }

  async function queueVideo(event: FormEvent) {
    event.preventDefault()
    const output = await runAction('Queueing video', () =>
      call<QueueResult>('queue_video', {
        request: {
          model: videoModel,
          prompt,
          negativePrompt,
          sourceImage,
          duration: videoDuration,
          resolution: videoResolution,
          aspectRatio: videoAspectRatio,
        },
      }),
    )
    if (output) setQueue(output)
  }

  async function queueAudio(event: FormEvent, kind: 'music' | 'sfx') {
    event.preventDefault()
    const output = await runAction('Queueing audio', () =>
      call<QueueResult>('queue_audio', {
        request: {
          model: kind === 'music' ? musicModel : sfxModel,
          prompt,
          duration: audioDuration,
          lyricsPrompt: kind === 'music' ? lyrics : '',
          forceInstrumental: kind === 'music' ? instrumental : false,
          lyricsOptimizer: kind === 'music' ? lyricsOptimizer : false,
        },
      }),
    )
    if (output) setQueue(output)
  }

  async function pollQueue() {
    if (!queue) return
    const queueKind = mode === 'video' ? 'video' : 'audio'
    const output = await runAction('Checking queue', () =>
      call<RetrieveResult>(queueKind === 'video' ? 'retrieve_video' : 'retrieve_audio', {
        request: {
          queueId: queue.queueId,
          kind: queueKind,
          model: queueKind === 'video' ? videoModel : mode === 'music' ? musicModel : sfxModel,
          downloadUrl: queue.downloadUrl,
        },
      }),
    )
    if (!output) return
    setQueue((existing) => existing ? { ...existing, status: output.status, progressLabel: output.progressLabel } : existing)
    if (output.result) {
      const result = output.result
      setResultGroups((existing) => [createResultGroup([result], result.kind), ...existing])
      setQueue(null)
    }
  }

  async function generateVoice(event: FormEvent) {
    event.preventDefault()
    const output = await runAction('Generating voice', () =>
      call<MediaResult>('generate_speech', {
        request: {
          model: voiceModel,
          input: voiceText,
          voice: voiceName,
          speed: voiceSpeed,
          responseFormat: voiceFormat,
          stylePrompt: voiceStyle,
        },
      }),
    )
    if (output) setResultGroups((existing) => [createResultGroup([output], 'Voice'), ...existing])
  }

  async function transcribeSpeech(event: FormEvent) {
    event.preventDefault()
    if (!transcribeAudio) {
      setError('Choose an audio or video file to transcribe')
      setLastActionMs(null)
      setStatus('Needs attention')
      return
    }

    const output = await runAction('Transcribing speech', () =>
      call<MediaResult>('transcribe_audio', {
        request: {
          model: transcribeModel,
          audio: transcribeAudio,
          fileName: transcribeFileName || 'audio',
          mimeType: transcribeMimeType,
          responseFormat: selectedTranscribeResponseFormat,
          timestamps: supportsTranscribeTimestamps ? transcribeTimestamps : false,
          language: supportsTranscribeLanguage ? transcribeLanguage : '',
        },
      }),
    )
    if (output) setResultGroups((existing) => [createResultGroup([output], 'Speech -> Text'), ...existing])
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
    <div className="app-shell">
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
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div>
            <h1>{modes.find((item) => item.id === mode)?.label}</h1>
            <p>{keyConfigured ? 'API key ready' : 'API key missing'} · Models: {formatDate(models.lastFetched)}</p>
          </div>
          <div className="topbar-actions">
            <button className="icon-button" type="button" onClick={refreshModelCatalog} title="Get Latest From Venice">
              <RefreshCw size={18} className={refreshingModels ? 'spin' : ''} />
            </button>
            <button className="icon-button" type="button" onClick={() => setMode('settings')} title="Settings">
              <Settings size={18} />
            </button>
          </div>
        </header>

        {error && <div className="notice error">{error}</div>}
        {status && (
          <div className="notice">
            {loading ? <Loader2 className="spin" size={16} /> : null}
            <span className="notice-message">{status}</span>
            {activeElapsedLabel && <span className="elapsed-pill">{activeElapsedLabel}</span>}
            {completedElapsedLabel && <span className="elapsed-pill">{completedElapsedLabel}</span>}
          </div>
        )}

        <section className="content-grid">
          <div className="tool-surface">
            {mode === 'image' && (
              <form onSubmit={generateImage} className="tool-form">
                <ModelSelect label="Model" value={imageModel} onChange={setImageModel} models={imageModels} />
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
                <SubmitButton loading={loading} icon={Wand2}>Generate Image</SubmitButton>
              </form>
            )}

            {mode === 'edit' && (
              <form className="tool-form">
                <ModelSelect label="Model" value={editModel} onChange={setEditModel} models={editModels} />
                <div className="edit-source-layout">
                  <SourcePicker
                    className="edit-source-main"
                    label="Image 1"
                    source={editSourceImages[0]}
                    onFile={(file) => loadEditSourceImage(0, file)}
                    onClear={() => clearEditSourceImage(0)}
                  />
                  <div className="edit-source-row">
                    {[1, 2].map((index) => (
                      <SourcePicker
                        key={index}
                        label={`Image ${index + 1}`}
                        source={editSourceImages[index]}
                        onFile={(file) => loadEditSourceImage(index, file)}
                        onClear={() => clearEditSourceImage(index)}
                      />
                    ))}
                  </div>
                </div>
                <PromptArea value={prompt} onChange={setPrompt} />
                <div className="action-row">
                  <button className="secondary-action" type="button" disabled>
                    <Scissors size={18} />
                    Edit / Combine
                  </button>
                  <button className="primary-action" type="button" onClick={removeBackground} disabled={loading || !hasEditSource}>
                    {loading ? <Loader2 className="spin" size={18} /> : <Eraser size={18} />}
                    Remove Background
                  </button>
                </div>
              </form>
            )}

            {mode === 'video' && (
              <form onSubmit={queueVideo} className="tool-form">
                <ModelSelect label="Model" value={videoModel} onChange={setVideoModel} models={videoModels} />
                <SourcePicker label="Source Image" source={sourceImage} onFile={loadSourceImage} />
                <PromptArea label="Motion prompt" value={prompt} onChange={setPrompt} />
                <PromptArea label="Negative prompt" value={negativePrompt} onChange={setNegativePrompt} rows={3} />
                <div className="control-grid">
                  <SelectField label="Duration" value={videoDuration} onChange={setVideoDuration} options={videoDurations} />
                  <SelectField label="Resolution" value={videoResolution} onChange={setVideoResolution} options={videoResolutions} />
                  <SelectField label="Aspect" value={videoAspectRatio} onChange={setVideoAspectRatio} options={videoRatios} />
                </div>
                <SubmitButton loading={loading} icon={Video}>Queue Video</SubmitButton>
              </form>
            )}

            {mode === 'music' && (
              <form onSubmit={(event) => queueAudio(event, 'music')} className="tool-form">
                <ModelSelect label="Model" value={musicModel} onChange={setMusicModel} models={musicModels} />
                <PromptArea value={prompt} onChange={setPrompt} />
                <PromptArea label="Lyrics" value={lyrics} onChange={setLyrics} rows={4} />
                <div className="control-grid">
                  <TextField label="Duration seconds" value={audioDuration} onChange={setAudioDuration} />
                </div>
                <label className="toggle-row">
                  <input type="checkbox" checked={instrumental} onChange={(event) => setInstrumental(event.target.checked)} />
                  <span>Instrumental</span>
                </label>
                <label className="toggle-row">
                  <input type="checkbox" checked={lyricsOptimizer} onChange={(event) => setLyricsOptimizer(event.target.checked)} />
                  <span>Auto lyrics</span>
                </label>
                <SubmitButton loading={loading} icon={Music}>Queue Music</SubmitButton>
              </form>
            )}

            {mode === 'sfx' && (
              <form onSubmit={(event) => queueAudio(event, 'sfx')} className="tool-form">
                <ModelSelect label="Model" value={sfxModel} onChange={setSfxModel} models={sfxModels} />
                <PromptArea value={prompt} onChange={setPrompt} />
                <div className="control-grid">
                  <TextField label="Duration seconds" value={audioDuration} onChange={setAudioDuration} />
                </div>
                <SubmitButton loading={loading} icon={Volume2}>Queue SFX</SubmitButton>
              </form>
            )}

            {mode === 'voice' && (
              <form onSubmit={generateVoice} className="tool-form">
                <ModelSelect label="Model" value={voiceModel} onChange={setVoiceModel} models={voiceModels} />
                <PromptArea label="Text" value={voiceText} onChange={setVoiceText} rows={7} />
                <div className="control-grid">
                  <SelectField label="Voice" value={voiceName || voiceOptions[0] || ''} onChange={setVoiceName} options={voiceOptions} />
                  <NumberField label="Speed" value={voiceSpeed} min={0.25} max={4} step={0.05} onChange={setVoiceSpeed} />
                  <SelectField label="Format" value={voiceFormat} onChange={setVoiceFormat} options={['mp3', 'wav', 'flac', 'aac', 'opus']} />
                </div>
                <PromptArea label="Style prompt" value={voiceStyle} onChange={setVoiceStyle} rows={3} />
                <SubmitButton loading={loading} icon={Mic2}>Generate Voice</SubmitButton>
              </form>
            )}

            {mode === 'transcribe' && (
              <form onSubmit={transcribeSpeech} className="tool-form">
                <ModelSelect label="Model" value={transcribeModel} onChange={setTranscribeModel} models={transcribeModels} />
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
                </div>
                {supportsTranscribeTimestamps && (
                  <label className="toggle-row">
                    <input type="checkbox" checked={transcribeTimestamps} onChange={(event) => setTranscribeTimestamps(event.target.checked)} />
                    <span>Include timestamps</span>
                  </label>
                )}
                <SubmitButton loading={loading} icon={FileText}>Transcribe</SubmitButton>
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
              </div>
            )}
          </div>

          <aside className="result-panel">
            {queue && (
              <div className="queue-panel">
                <div>
                  <span className="eyebrow">Queued</span>
                  <strong>{queue.queueId}</strong>
                  <small>{queue.progressLabel || queue.status}</small>
                </div>
                <button className="icon-button" type="button" onClick={pollQueue} title="Poll queue">
                  <RefreshCw size={18} />
                </button>
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
                    <button className="icon-button compact danger" type="button" onClick={() => moveResultFilesToBurn(resultFilePaths, 'all result files')} title="Move all result files to burn folder">
                      <Trash2 size={16} />
                    </button>
                  </>
                )}
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
                      <button className="icon-button compact danger" type="button" onClick={() => moveResultFilesToBurn(group.results.map((result) => result.filePath), 'this result set')} title="Move result set to burn folder">
                        <Trash2 size={14} />
                      </button>
                    </div>
                  </div>
                  <div className={classNames('result-group-grid', group.kind !== 'images' && 'single')}>
                    {group.results.map((result) => (
                      <article className="result-item" key={result.id}>
                        {result.mimeType.startsWith('image/') && <img src={result.dataUrl} alt={result.name} />}
                        {result.mimeType.startsWith('video/') && <video src={result.dataUrl} controls />}
                        {result.mimeType.startsWith('audio/') && <audio src={result.dataUrl} controls />}
                        {result.mimeType.startsWith('text/') && <pre className="transcript-preview">{result.text}</pre>}
                        <div className="result-meta">
                          <strong>{result.name}</strong>
                          <small>{result.filePath}</small>
                          <div className="result-links">
                            <a href={result.dataUrl} download={result.name}>
                              <Download size={16} />
                              Save
                            </a>
                            <button className="link-button danger" type="button" onClick={() => moveResultFilesToBurn([result.filePath], 'this file')}>
                              <Trash2 size={14} />
                              Delete
                            </button>
                          </div>
                        </div>
                      </article>
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
}: {
  label: string
  value: string
  onChange: (value: string) => void
  models: ModelRecord[]
}) {
  return (
    <label className="field">
      <span>{label}</span>
      <select value={value} onChange={(event) => onChange(event.target.value)}>
        {models.map((model) => (
          <option key={model.id} value={model.id}>{model.name || model.id}</option>
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
  onClear,
}: {
  className?: string
  label: string
  source: string
  onFile: (file: File) => void | Promise<void>
  onClear?: () => void
}) {
  const [dragging, setDragging] = useState(false)

  function chooseFile(file: File | undefined) {
    if (!file || !file.type.startsWith('image/')) return
    void onFile(file)
  }

  function handleInput(event: ChangeEvent<HTMLInputElement>) {
    chooseFile(event.target.files?.[0])
    event.currentTarget.value = ''
  }

  function handleDrop(event: DragEvent<HTMLLabelElement>) {
    event.preventDefault()
    setDragging(false)
    chooseFile(Array.from(event.dataTransfer.files).find((file) => file.type.startsWith('image/')))
  }

  return (
    <div className={classNames('source-picker', className)}>
      <label
        className={classNames('source-input', dragging && 'dragging')}
        onDragEnter={(event) => {
          event.preventDefault()
          setDragging(true)
        }}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={() => setDragging(false)}
        onDrop={handleDrop}
      >
        <input type="file" accept="image/*" onChange={handleInput} />
        {source && <img src={source} alt={label} />}
        <span className={classNames('source-label', source && 'loaded')}>
          {!source && <ImageIcon size={18} />}
          {label}
        </span>
      </label>
      {source && onClear && (
        <button className="icon-button compact source-clear" type="button" onClick={onClear} title={`Clear ${label}`}>
          <Trash2 size={14} />
        </button>
      )}
    </div>
  )
}

function SubmitButton({
  loading,
  icon: Icon,
  children,
}: {
  loading: boolean
  icon: LucideIcon
  children: ReactNode
}) {
  return (
    <button className="primary-action" type="submit" disabled={loading}>
      {loading ? <Loader2 className="spin" size={18} /> : <Icon size={18} />}
      {children}
    </button>
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
