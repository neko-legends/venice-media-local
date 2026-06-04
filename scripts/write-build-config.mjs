import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const outputPath = resolve(root, 'src-tauri', 'tauri.version.conf.json')

function pad(value) {
  return String(value).padStart(2, '0')
}

function command(args) {
  return execFileSync(args[0], args.slice(1), {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'ignore'],
  }).trim()
}

function normalizePath(value) {
  return resolve(value).toLowerCase()
}

function resolveGitInfo() {
  try {
    const topLevel = command(['git', 'rev-parse', '--show-toplevel'])
    if (normalizePath(topLevel) !== normalizePath(root)) {
      return { commit: 'nogit', dirty: false }
    }

    const commit = command(['git', 'rev-parse', '--short=8', 'HEAD'])
    let dirty = false
    try {
      command(['git', 'diff-index', '--quiet', 'HEAD', '--'])
    } catch {
      dirty = true
    }
    return { commit: commit || 'nogit', dirty }
  } catch {
    return { commit: 'nogit', dirty: false }
  }
}

const now = new Date()
const year = now.getFullYear()
const month = now.getMonth() + 1
const day = now.getDate()
const hour = now.getHours()
const minute = now.getMinutes()
const second = now.getSeconds()
const patch = Number(`${day}${pad(hour)}${pad(minute)}${pad(second)}`)
const builtAt = `${year}-${pad(month)}-${pad(day)} ${pad(hour)}:${pad(minute)}:${pad(second)}`
const git = resolveGitInfo()
const metadata = git.dirty ? `g${git.commit}.dirty` : `g${git.commit}`
const packageInfo = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8'))
const version = packageInfo.version

const config = {
  version,
  bundle: {
    shortDescription: `Local Venice media generator (${builtAt}, ${metadata})`,
    longDescription: `A local desktop app for generating images, video, music, sound effects, and voice through the Venice API. Build ${builtAt}, ${metadata}.`,
  },
}

writeFileSync(outputPath, `${JSON.stringify(config, null, 2)}\n`)
console.log(`Generated ${outputPath}`)
console.log(`Build version: ${version}`)
