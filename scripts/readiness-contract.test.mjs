import assert from 'node:assert/strict'
import fs from 'node:fs'
import test from 'node:test'

test('agent state reports the live API key readiness value', () => {
  const source = fs.readFileSync(new URL('../src-tauri/src/main.rs', import.meta.url), 'utf8')
  const collectState = source.match(/fn collect_app_state[\s\S]*?Ok\(AppState \{[\s\S]*?\n    \}\)\n\}/)?.[0] || ''

  assert.match(collectState, /key_configured:\s*has_api_key\(\)/)
  assert.doesNotMatch(collectState, /key_configured:\s*false/)
})
