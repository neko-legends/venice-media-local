# Phase 5H Venice Media Local cold-activation remediation

Status: implementation specification
Scope: maintenance activation tooling only; no production activation, service start/stop, catalog request, media request, import, Core restart, or Phase 6 work

## Problem

The existing Phase 5H release contract assumes a live Venice Media Local process. It requires two authenticated health samples and an authenticated `application:shutdown` request before changing the active release. That is correct for a live instance, but it cannot be satisfied when the retained legacy package is already stopped and does not implement Agent Control shutdown.

The failed 2026-07-14 attempt correctly stopped before mutation. The installed retained executable was 26.6.5 with SHA-256 `B46D8EE942020EC6FEA81DB298F4D83E32975177838E2CC0ABD24DA76001E64D`; no Venice process or port-9876 listener existed. Stale discovery metadata claimed 26.7.6 and was therefore non-authoritative. The target immutable slot and its three retained hashes passed, but live health, routing, and shutdown permission could not be fabricated.

## Safety invariant

The normal live-instance path remains unchanged. If the expected process or listener exists during either cold sample, authorization, lock acquisition, final decision, or first mutation, cold activation fails closed and the operator must use authenticated graceful shutdown.

Cold activation substitutes independently verifiable host and persisted-work evidence only for facts that cannot exist while the application is stopped. It never represents stale discovery as health, routing, permission, shutdown, or installed-version proof.

## Exact permission and server-held authorization

Cold activation uses the new exact verified action:

`venice-media-local:activate-release-slot`

Core exposes two normal-human-session endpoints:

- `POST /api/phase5h/venice-maintenance-activation/authorizations/samples`
- `POST /api/phase5h/venice-maintenance-activation/authorizations`
- `POST /api/phase5h/venice-maintenance-activation/authorizations/:id/consume`

Both require a current normal human web session with that exact action. Owner status does not bypass it. Service tokens, Eva Desktop tokens, provider credentials, legacy sessions, identity-only sessions, wrong actions, and wildcard/broader action grants are rejected.

The create request is strict JSON and contains only non-secret facts:

- operation type `cold-activation`;
- reason `phase5h-release-slot-transition`;
- retained executable canonical path, version, byte length, and SHA-256;
- staged-slot canonical path and portable, installer, and manifest filenames, byte lengths, and SHA-256 values;
- expected process name and TCP port;
- intended replacement version, source commit, instance identity, and manifest digest;
- both cold-state sample digests and timestamps;
- observed stale-discovery digest and timestamp, without treating it as authoritative;
- authoritative persisted-work evidence digest and counts.

For each host sample the operator first submits only its non-secret evidence digest to the sample endpoint. Core independently queries its authoritative persisted orchestration tables and stores a server-held sample bound to the host digest, exact human session, counts, and timestamp. Active provider operations are rows whose state is not `succeeded`, `failed`, `canceled`, `lost`, or `reconciliation_required`. Unsettled jobs are rows whose state is not `completed`, `failed`, or `canceled`. Both counts must be zero. A missing table or failed query is not zero and fails closed. Issuance requires two unused server-held samples from the same session at least five seconds apart and atomically binds them to the authorization. Consumption performs one more independent query.

Core stores only the authorization record, canonical binding JSON/digest, issuance and expiry timestamps, issuing user/session, consume timestamp, and decision. It returns a record valid for at most 60 seconds. Consumption atomically changes an unused, unexpired, exact-binding record to consumed after rechecking authoritative zero-work state. Reuse, mismatch, expiry, or concurrent consumption fails. The browser/session bearer remains only in the supported in-memory transport and is never logged, fingerprinted, persisted, placed in JSON evidence, environment variables, arguments, or process listings.

## Windows cold-state operator

The foreground 64-bit Windows PowerShell 5.1 operator requests the exact action through Core's supported connector URL, retains the resulting session only in memory, and passes it to the Node operator through redirected standard input. It opens the connector URL in the default browser and never opens a wallet-download URL.

The Node cold-activation engine has a dependency-injected host adapter for deterministic tests and a production Windows adapter for the separately authorized live run. It performs:

1. Canonicalize and confine all retained, staged, pointer, discovery, report, and lock paths. Reject links/reparse points where immutable files or activation roots are required.
2. Verify exact retained and staged byte lengths and SHA-256 values.
3. Parse the staged manifest strictly and prove its intended identity and source commit.
4. Read stale discovery bytes only to record their SHA-256, byte length, and last-write timestamp. Do not use its claimed version, instance, manifest, routing, or health as authority and do not rewrite it during preflight.
5. Prove local zero work. For ledger-capable releases, read `provider-v2/ledger.json` without decrypting secrets and require zero nonterminal or ambiguous/lost operations. For the exact retained pre-ledger package only (`26.6.5`, SHA-256 `b46d8ee942020ec6fea81db298f4d83e32975177838e2cc0abd24da76001e64d`, 15109632 bytes), require process and listener absence, forbid `provider-v1`/`provider-v2`/`provider-v2-execution`, bind the allowlisted app-data inventory digest, and require Core active provider operations and unsettled work to be zero. File absence alone is never accepted for any other version or hash. The first `provider-v2` ledger may be created only during the authorized activation/start transaction; post-start health must observe a valid zero-work ledger.
6. Collect two complete cold-state samples at least five seconds apart. Each repeats process absence, listener absence, transition absence, retained hash, all staged hashes, and local persisted-work checks.
7. Create the server-held authorization bound to the two sample digests and all exact facts.
8. Acquire the exclusive transition lock. Lock contention or an abandoned/uncertain lock fails closed; it is not silently recovered during activation.
9. While holding the lock, repeat every local gate, require authorization freshness and exact binding, and atomically consume the server-held authorization. Consumption is the final authorization decision and is single use.
10. Only then begin mutation: preserve the prior pointer/discovery evidence, atomically select the staged slot, and launch only the executable named by that pointer.
11. Require authenticated readiness within the bounded startup window: intended instance, version, manifest digest, routing eligibility, zero active work, and running executable/manifest hashes must all match.

No process kill fallback exists.

## Discovery handling

Stale `control-api.json` is preserved byte-for-byte as pre-activation evidence and never accepted as installed or running identity. It remains untouched until the new process has produced authenticated ready health with the intended binding. At that point the generation-owned discovery file written by the new process replaces stale metadata and its digest is recorded.

If rollback is required, the new instance is stopped only through its authenticated `application:shutdown` route. The prior pointer and preserved discovery bytes are restored atomically before launching the retained package. If the retained package cannot emit the modern discovery format, the preserved prior bytes remain explicitly marked stale and non-authoritative; registration identity and routing are verified through Core, not inferred from that file.

## Atomic activation and rollback

No pointer, executable, manifest, discovery, registration, or routing mutation occurs before lock-held final consumption succeeds. After the first mutation, every failed start or post-activation identity, manifest, executable, routing, readiness, or zero-work gate enters rollback.

Rollback restores the exact prior executable selection and manifest/pointer evidence, reconciles discovery as described above, restores pre-activation app-data ledger absence when the retained package was pre-ledger 26.6.5, launches the retained 26.6.5 executable, and requires its expected identity, routing, and health contract. Evidence distinguishes:

- `no-mutation-rollback-unnecessary`;
- `rollback-required-passed`;
- `rollback-required-failed` (hard failure, no recursive retry).

The target slot and retained rollback package are never deleted or modified.

## Evidence

The operator writes bounded non-secret append-only attempt evidence outside immutable slots. It records authorization ID and binding digest, samples and timestamps, permission/action result, zero-work counts, lock acquisition/release, hashes, pointer transitions, start and health results, process/listener/temporary-state leak counts, rollback disposition, and final version. It never records any bearer, cookie, wallet proof, credential value, credential fingerprint, environment, or command line.

## Tests and acceptance

Adversarial coverage must include valid stopped legacy activation; process/listener races between samples and before mutation; work appearing before mutation; misleading stale discovery; retained/staged/manifest mismatch; expiry, binding mismatch, replay, wrong/missing exact permission; lock contention; start failure and post-health failure with successful rollback; hard rollback failure; and rejection of a live instance on the cold path.

The engine must also prove bounded/redacted diagnostics, linked/path escape rejection, no caller-controlled command execution, secret exclusion, and cleanup of every process, listener, temporary file, and lock on success, failure, timeout, cancellation, and parent exit.

Run the final isolated cold-activation harness three consecutive times. Each run must report all assertions passed, leaked processes zero, active provider operations zero, unsettled jobs zero, expected ports released, and temporary state/transition locks released.

Run Core trust, route, database, orchestration/provider contract tests; Venice provider, release-slot, readiness, backup/restore, frontend, Rust, Windows-package, and cross-repository harness tests. If production runtime artifacts change, build a new immutable slot and retain the old staged slot and 26.6.5 rollback package. Documentation/test/operator-only changes do not invalidate the already staged runtime artifacts, but their hashes must still be reverified before the next live attempt.

## Production boundary

This remediation task ends after implementation, tests, commits, pushes, and a new activation sheet. The next live activation requires one fresh `venice-media-local:activate-release-slot` verified action and a new server-held authorization. No prior project-check or activation authorization may be reused.
