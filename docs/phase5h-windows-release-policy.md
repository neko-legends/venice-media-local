# Phase 5H Windows Release Policy

Phase 5H is an `owner-controlled-internal` binary release lane. It does not change the normal deterministic Windows build command:

```powershell
.\Build-Windows.ps1
```

The installer and portable executable may be unsigned. On Windows, the operator must run `Get-AuthenticodeSignature` for each artifact and record its `Status` exactly as `NotSigned`; missing evidence is not equivalent to `NotSigned`.

## Retained evidence

Keep a local manifest or evidence file under Jun's control. For each artifact, record its filename, byte length, SHA-256, exact source commit, build timestamp, toolchain versions, architecture, and the explicitly named Jun-controlled destination machine. Record the observed `NotSigned` status too.

Recalculate and compare SHA-256 after staging and again immediately before activation. Hashes prove identity against the trusted retained manifest; they do not authenticate the publisher.

The Phase 5H binary must not be uploaded or distributed publicly. This restriction applies to this internal binary lane, not to public distribution of the repository's open-source code.

Validate the local policy before building:

```powershell
node scripts/phase5h-readiness.mjs validate-policy <policy.json>
```

## Deployment gates

### Legacy Agent Control credential migration

An older installation can contain a non-empty `agentControlToken` property in `settings.json`. It remains a live compatibility source until a replacement in the Windows credential store has been proven, so ordinary startup and settings writes must not delete it merely because the current build prefers secure storage.

Use `scripts/Invoke-Phase5HLegacyControlTokenMigration.ps1` with the exact staged candidate. The foreground operator requests the exact `venice-media-local:migrate-legacy-agent-control-token` verified action immediately before the change and supplies the short-lived Core session to the candidate through redirected standard input. The session must identify `user-jun`, human authentication, current `verified_action` trust, the exact action key, and a future expiry. It is never accepted from an argument, environment variable, file, log, evidence object, or hash.

For a Windows browser with the MetaMask extension, the operator opens Core's `connectorUrl` in the configured default browser. It must not open the mobile-only `walletUrl`, which can fall back to `metamask.io/download` instead of invoking the installed extension. The connector URL and identifier remain in process memory and are never printed or retained.

Before launching the candidate, the operator uses the in-memory bearer once against Core's authenticated session endpoint and requires Jun, human authentication, current `verified_action` trust, the exact migration action, and a future expiry. A rejection reports only the HTTP status. The candidate independently repeats that proof before reading or changing settings; its rejection diagnostics likewise contain only the HTTP status and no response body or credential material.

Redirected standard input is explicitly UTF-8 without a byte-order mark. The candidate defensively removes only a leading UTF-8 BOM plus transport whitespace before constructing the authorization header; this prevents Windows PowerShell's stream encoding marker from turning an otherwise valid bearer into a false 401 without creating any credential fallback.

The candidate applies this replacement-first sequence:

1. If a non-empty Windows credential-store entry already exists, prove it can be read and treat the JSON entry as obsolete without overwriting the replacement.
2. Otherwise copy the legacy value internally to the existing `venice-media-local` / `agent-control-token` credential-store entry and read it back for exact in-memory equality.
3. Only after either proof succeeds, atomically remove `agentControlToken` from `settings.json`, preserving unrelated JSON properties, and reread the file to prove sanitization.
4. Any authorization, store, read-back, serialization, atomic-write, or reread failure leaves the legacy JSON source in place. A replacement written before a later failure is safe rollback state; retry proves it before removal.

Retained evidence records only the action key, UTC transition times, candidate identity, settings path, non-secret result (`existing-replacement-proven`, `replacement-migrated`, or `already-sanitized`), and the sanitized-backup result. Never retain the credential, a derivative, a fingerprint, or a before-image containing it.

### npm audit disposition (2026-07-13)

The locked Phase 5H tree reports `@babel/core` 7.29.0 (low, GHSA-4x5r-pxfx-6jf8), `esbuild` 0.21.5 (moderate, GHSA-67mh-4wv8-2f99), and the direct development dependency Vite 5.4.21 (aggregate high: GHSA-4w7w-66w2-5vf9, GHSA-v6wh-96g9-6wx3, GHSA-fx2h-pf6j-xcff, plus the esbuild advisory). The exact paths are `@vitejs/plugin-react -> @babel/core`, direct `vite`, and `vite -> esbuild`.

These packages are development/build-server tooling only: all are in `devDependencies`; Vite runs only as `beforeDevCommand` or `beforeBuildCommand`; and the Tauri package embeds the already-built `../dist` static frontend. The packaged Rust/WebView runtime has no Node, Vite dev/preview server, Babel transform, esbuild service, launch-editor endpoint, or `server.fs` route. Consequently the cited request/path traversal and dev-server behaviors are unreachable in the packaged production runtime. npm's suggested remediation is Vite 8.1.4, a semver-major build-tool change, so Phase 5H does not apply it. Reassess before exposing a development/preview server or changing the packaging model.

1. Require authenticated health to identify the expected provider instance and report ready, with `activeOperationCount = 0` in two samples at least five seconds apart. Require the running `instanceId` and `manifestDigest` to match the active pointer and trusted retained release evidence before shutdown.
2. Bind shutdown to that exact running `instanceId` and `manifestDigest`. The Agent Control credential must have server-assigned `application:shutdown`; body scope alone is not authorization.
3. Send the strict `POST /api/v1/actions/shutdown` request with fresh request/idempotency identities, a validity interval no longer than 60 seconds, and reason `phase5h-release-slot-transition`. Require `202 Accepted`, `state = shutting_down`, and `replayed = false`. Allow at most 20 seconds for response drain and 30 seconds for external process absence. A permission/owner rejection, drain timeout, teardown failure, or process-exit timeout leaves the old pointer unchanged. Drain timeout keeps admission closed and withholds teardown/exit. Never use forced termination.
4. Back up retained state and preserve the previous immutable slot. Stage the candidate in a new immutable `releaseRoot\slots\<version>-<sha256-prefix>` slot and never alter an active slot in place.
5. Recheck every staged byte length, SHA-256, and explicit `NotSigned` observation against retained evidence. Atomically replace the same-volume `current.json` pointer with a write-through temporary file and rename only after all checks pass. The pointer records schema version, slot, previous slot, and artifact SHA-256.
6. Launch only the exact path named by the new pointer. Within 60 seconds require authenticated ready health, the expected instance/version/manifest identity, and `activeOperationCount = 0`. Only after those checks pass, retain the prior pointer and manifest as immutable rollback evidence.
7. On any failure, shut down through the same authenticated action, atomically restore the prior pointer, launch the exact retained prior artifact, and require the same identity, health, and idle gates. Preserve the failed slot and evidence. If rollback health fails, stop and report both slot identities; do not recurse automatically.

## Windows checklist

```powershell
.\Build-Windows.ps1
git rev-parse HEAD
node --version
rustc --version
cargo tauri --version
(Get-Date).ToUniversalTime().ToString("o")
Get-Item <artifact> | Select-Object Name, Length
Get-FileHash <artifact> -Algorithm SHA256
Get-AuthenticodeSignature <artifact> | Select-Object Status
# Record architecture and the exact Jun-controlled destination machine.
# Re-run Get-FileHash after staging and immediately before activation.
```

## Future public distribution

Public binary distribution is not part of Phase 5H. A future public lane must use one approved publisher certificate, pin the exact signer identity and certificate thumbprint, use an RFC 3161 timestamp, and require `Get-AuthenticodeSignature` to report `Status` exactly as `Valid`. Those requirements belong here as future policy, not as placeholder fields in the Phase 5H configuration.

The existing backup, restore, immutable-slot, authenticated shutdown, rollback, pointer, and health behavior remains authoritative. `npm run test:phase5h` exercises the local policy validator and the previously committed synthetic backup/restore/slot harness; it does not build, sign, deploy, activate, or access live systems.
