# Phase 5H Windows Release and Recovery Policy

Status: verification-ready specification. No production action is performed by the readiness harness.

The reviewed release source commit must carry the workspace-required agent identity and `Agent:` trailer before it can be named by the LAND packet.

## Inputs and roots

- Accepted artifacts are the NSIS setup executable and portable executable produced by `Build-Windows.ps1`: `Venice Media Local_<version>_x64-setup.exe` and `Venice Media Local_<version>_x64-portable.exe`. Release archives are not accepted.
- A local, uncommitted policy file must be created from `config/phase5h-release-policy.example.json`. `signerSubject` and uppercase, space-free SHA-1 `signerThumbprint` must be supplied by the human release owner. They are identities, not secrets.
- `certificateReference` is a Windows certificate-store reference only, in the form `Cert:\CurrentUser\My\<thumbprint>` or `Cert:\LocalMachine\My\<thumbprint>`. A PFX path, password, private key, exported certificate, environment credential, or embedded secret is forbidden.
- The absolute `stageRoot`, `releaseRoot`, and `rollbackRoot` must be different local NTFS directories. User app data, `%APPDATA%\community.venice.media.local`, output folders, and repository build directories are forbidden as roots.
- Signing is **HUMAN DECISION REQUIRED** until a human supplies and approves the signer identity, certificate-store reference, and RFC 3161 timestamp URL. The repository does not choose or access a private key.

## Verification and manifest

1. Copy only the two accepted artifact formats into a new stage slot. Do not build in the stage or release roots.
2. Before and after signing, record each filename, byte size, and SHA-256. Signing must use Windows `signtool sign /fd SHA256 /td SHA256 /tr <timestampUrl> /sha1 <thumbprint>`. The certificate is resolved from the approved Windows store reference; no private value is accepted by script or policy.
3. Require `signtool verify /pa /all /v <artifact>` to succeed. Independently read `Get-AuthenticodeSignature`; require `Status = Valid`, exact case-insensitive signer subject equality, exact thumbprint equality, a countersignature/timestamp, SHA-256 file digest, and an RFC 3161 SHA-256 timestamp from the policy URL. An absent, expired-at-signing, mismatched, or unverifiable timestamp fails closed.
4. Write deterministic UTF-8 `SHA256SUMS` lines sorted by filename as `<lowercase-sha256>  <filename>`. Hash the finished manifest separately in deployment evidence. Never include credentials, environment values, logs, or app data.

## Stop, activation, and gates

1. Authenticate every control request with the separately provisioned Agent Control bearer credential. Never read it from backup data or print it. Require `GET /api/v1/health` to identify the expected provider instance and return healthy/ready, and require `activeOperationCount = 0` in two samples at least five seconds apart. A degraded, unauthenticated, unreachable, or busy provider blocks deployment.
2. Read the exact running provider `instanceId` and `manifestDigest` from authenticated health/capability evidence. They must match the currently active slot and release manifest; PID or process name is not an authorization binding.
3. The running Agent Control credential must have the server-assigned `application:shutdown` permission; body scope is an additional fixed intent binding, not authorization. Send the strict authenticated `POST /api/v1/actions/shutdown` envelope with that scope, provider `venice-media-local`, the exact instance ID and manifest digest, fresh unique request/idempotency identities, a current validity interval no longer than 60 seconds, and reason `phase5h-release-slot-transition`. Require `202 Accepted`, `state = shutting_down`, and `replayed = false`. The app atomically closes revision-2 and compatibility admission, persists acceptance before returning, and allows at most 20 seconds for Axum to flush/drain the response before any kernel resource/lifecycle teardown. It then unregisters lifecycle when configured, records `exit_requested`, and calls Tauri whole-app exit. Wait at most 30 seconds for external process absence. A drain timeout records failure, keeps admission closed, and withholds teardown/exit. Do not use `Stop-Process`, taskkill, service termination, window-title matching, or any forced fallback. Timeout, unavailable permission/owner, or recorded teardown failure blocks activation and leaves the old pointer unchanged.
4. Stage each release in immutable `releaseRoot\slots\<version>-<sha256-prefix>`. Verify signature and manifest in the final slot. Atomically replace the same-volume `releaseRoot\current.json` pointer with a write-through temporary file and rename. The pointer contains schema version, slot, previous slot, and artifact SHA-256. Never overwrite an existing slot.
5. Launch only the exact path named by the new pointer. Require authenticated health for the expected instance, exact version/hash evidence, ready status, and `activeOperationCount = 0` within 60 seconds. Only then copy the prior pointer and manifest into a new immutable `rollbackRoot` record and declare release success.

The authenticated whole-application shutdown blocker is mechanically closed by synthetic direct-command admission/counting, cancellation-safe permits, generation-safe dropped-waiter persistence, startup rollback, listener cleanup, shared Settings RMW, shutdown/Settings barrier, lifecycle rollback, terminal-latch, fallible lifecycle-state, task-reaping, healthy-lifetime, immutable generation, atomic persistence, replay, concurrency, bounded response-drain, emergency-audit, and static forced-termination tests. This does not prove Windows host behavior, package signing, production activation, Phase 5 completion, or Phase 6 readiness. Approved signer/timestamp inputs and every remaining LAND gate are still required.

## Rollback and forward recovery

- On failed post-activation gates, close the candidate through the same exact authenticated `POST /api/v1/actions/shutdown` procedure, atomically restore `current.json` to `previous`, launch that exact prior path/hash, and require the same authenticated health and idle gates. Preserve the failed slot and evidence; do not mutate or delete it.
- If rollback health fails, stop and report both slot identities and non-secret verification evidence. Do not recurse through older slots automatically.
- Forward recovery creates a new immutable slot from newly verified artifacts and activates it through the full procedure. Never repair an active slot in place.
- Provider-state restore is separate from binary rollback. Use only the synthetic-root harness contract below; it never imports OS credentials or decrypts `provider-v2/ledger.json`.

## Credential-safe state harness

`npm run test:phase5h` exercises `scripts/phase5h-readiness.mjs` on disposable roots. The CLI accepts only explicit source/destination paths:

```text
node scripts/phase5h-readiness.mjs backup <synthetic-source> <new-backup> [--include-artifacts] [--include-uploads]
node scripts/phase5h-readiness.mjs restore <backup> <new-empty-destination>
```

The allowlist is sanitized `settings.json`, `venice-models.json`, `control-api.json`, `capability-provider-instance-id`, `provider-v1/lifecycle.json`, and opaque `provider-v2/ledger.json`. `provider-v2/artifacts` and `provider-v2/uploads` require independent flags. The denylist includes environment and `.env` data, keyrings, private keys, whole-app-data recursion, logs, output/user data, and `provider-v2-execution` by default. Links are rejected. Restore verifies the size/SHA-256 inventory into a new empty destination and never decrypts the ledger or imports credentials.
