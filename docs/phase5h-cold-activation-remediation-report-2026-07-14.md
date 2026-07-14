# Phase 5H Venice cold-activation remediation report

Date: 2026-07-14

Disposition: implementation and isolated verification passed; production was not activated, started, stopped, replaced, or otherwise mutated.

## Root cause

The retained Venice Media Local 26.6.5 package was already stopped and port 9876 had no listener. The normal maintenance contract correctly required authenticated live-instance health, routing, Agent Control permission, and graceful-shutdown evidence. Those facts cannot be produced for an absent legacy process. Stale discovery metadata identifying 26.7.6 was not authoritative for the installed 26.6.5 executable, so the previous attempt correctly failed closed before mutation.

The cross-repository provider harness also had a real Windows race: it read the canonical provider ledger during the brief atomic replacement interval and could receive `ENOENT`. The harness now retries only that canonical read for a bounded two-second interval and still requires the real provider, registration, work, and shutdown assertions.

## Contract changes

Eva Core commit `bdb085af15d8b3602547ba80fa7fdf3eb1381ac7` adds:

- exact verified action `venice-media-local:activate-release-slot` for a normal human session; owner bypass is disabled and service, Desktop, provider, legacy, wrong-action, and broad-action credentials remain rejected;
- server-held cold-state samples bound to the authenticated user/session and authoritative Core persisted-work counts;
- two unused samples from the same session, at least five seconds apart, with the latest no more than 30 seconds old and both proving zero active operations and zero unsettled jobs;
- a maximum-60-second, exact-binding, single-use cold-activation authorization whose samples are claimed atomically;
- atomic authorization consumption with replay, expiry, user/session/action/binding, and authoritative zero-work rechecks;
- additive SQLite storage only: `phase5h_venice_activation_samples` and `phase5h_venice_activation_authorizations`, plus the authorization-expiry index;
- routes under `/api/phase5h/venice-maintenance-activation/authorizations` for samples, issuance, and single-use consumption.

Venice Media Local commit `56edf6978e2909981a02411a1527d15dc1c5c0ff` adds:

- an explicit cold-only activation engine and Windows operator;
- independent host samples proving the process and listener are absent, exact retained/staged hashes, stale discovery recorded as non-authoritative, local zero-work state, and no transition already in progress;
- an exclusive transition lock, followed by a final local recheck and Core authorization consumption before the first mutation;
- fail-closed diversion to the existing authenticated graceful-shutdown lane whenever a process or listener is present;
- atomic release selection, exact post-start health/identity/manifest/routing/hash/zero-work verification, and automatic rollback;
- discovery replacement only after successful activation; rollback restores legacy selection, discovery, registration identity, and routing;
- distinct evidence dispositions for no mutation, successful rollback, and hard rollback failure;
- a Windows PowerShell 5.1 foreground handoff that keeps Core and Agent Control credentials in memory/stdin, retrieves Agent Control material through Windows Credential Manager, and never places credential material in arguments, environment, evidence, logs, hashes, or process listings.

Primary files are `server/phase5h-venice-activation-service.js`, `server/routes/phase5h-venice-activation-routes.js`, `scripts/phase5h-cold-activation.mjs`, `scripts/phase5h-cold-activation-windows.mjs`, and `scripts/Invoke-Phase5HColdActivation.ps1`. Their adjacent specifications and test files contain the full bindings and adversarial acceptance criteria.

## Verification

- Core focused activation service, routes, trust-policy, and lifecycle tests: passed.
- Core additive SQLite migration test: passed.
- Core relevant contract/test set: 121 checks passed, including the isolated Control Center dependency checks.
- Core production client build: passed (warnings only).
- Venice readiness contract: 1/1 passed.
- Venice provider contract: 13/13 passed.
- Venice Phase 5H suite: 28/28 passed.
- Venice frontend production build: passed.
- Rust provider kernel: 48/48 passed.
- Rust provider-kernel-tests: 12/12 passed.
- PowerShell parser validation: passed.
- Cross-repository real-provider harness: three consecutive runs passed; final audit found zero headless processes, temporary harness roots, or listeners on port 39876.
- Cold-activation harness: three consecutive runs passed. Every run ended with leaked processes 0, active provider operations 0, unsettled jobs 0, listeners 0, and released temporary state and transition lock.
- Secret-like literal scan of new security-sensitive files: zero findings.
- Staged Windows hashes, byte lengths, NotSigned state, and non-reparse paths: reverified.

The adversarial suite covers a valid stopped-legacy transition; process/listener appearance between samples and before mutation; new active/unsettled work; misleading stale discovery; retained/staged hash mismatch; expiry; wrong binding; replay; missing permission; lock contention; activation and post-health failures with successful rollback; rollback hard failure; and rejection of live-instance state by the cold lane.

## Immutable staged release

No application runtime or packaged binary changed in this remediation, so no new slot was created and the existing immutable staged release remains exact:

- slot: `D:\eva-phase5h\stage\slots\venice-26.7.6-b2b3ce8e-0717aff4e386`
- portable `venice-media-local.exe`: 22,205,952 bytes; SHA-256 `0717AFF4E386C642959F800D3205C8F554FD8CBC4829A2B9B6439AE2C8B5CC5C`; Authenticode `NotSigned`
- installer `Venice Media Local_26.7.6_x64-setup.exe`: 5,106,940 bytes; SHA-256 `044776C975900B61475F713FD448B0987F3F3F0A621C6E34E57B2306463EE4FD`; Authenticode `NotSigned`
- `manifest.json`: 2,191 bytes; SHA-256 `D4F48F6220E7500EED58457D86C3F0415EEFF93089D5F70692C3360F1B977F65`
- manifest candidate source: `b2b3ce8ed09331af4828b318c79e79918127b02d`
- slot and artifacts are not reparse points.

Retained rollback remains unchanged:

- path: `C:\Users\flash\AppData\Local\Venice Media Local\venice-media-local.exe`
- version: 26.6.5
- size: 15,109,632 bytes
- SHA-256: `B46D8EE942020EC6FEA81DB298F4D83E32975177838E2CC0ABD24DA76001E64D`

The failed-attempt evidence remains append-only at `D:\eva-phase5h\evidence\maintenance-activation-attempt-20260714T164951Z.json`, SHA-256 `C73C592EB6306B8127DF524E16F743243AA027A9E90EA16EC968061112428431`.

## Maintenance-activation sheet

Fresh verified-action scope: exactly `venice-media-local:activate-release-slot` in a normal human browser session. Do not reuse `build-anything:run-project-check` or any prior authorization record.

Before a live Venice transition, Core commit `bdb085af15d8b3602547ba80fa7fdf3eb1381ac7` must first be deployed through the established immutable Core release lane and verified healthy. That controlled Core restart is outside this remediation authorization and is the only remaining implementation blocker.

After Core is deployed, the operator must derive and display the exact non-secret retained identity, replacement identity/manifest, port/process, stale-discovery digest, authoritative-work digest, and all artifact bindings. It must then obtain two fresh server-held samples, acquire the local exclusive transition lock, perform the final rechecks, and request the exact action above. Any mismatch fails before mutation. A live process or listener requires the pre-existing authenticated shutdown path instead.

Exact next runtime prompt:

`AUTHORIZE CORE ACTIVATION OF bdb085af15d8b3602547ba80fa7fdf3eb1381ac7 THROUGH THE ESTABLISHED IMMUTABLE RELEASE LANE WITH ONE FORWARD RESTART AND AUTOMATIC ROLLBACK RESTART ON FAILURE; DO NOT ACTIVATE OR START VENICE MEDIA LOCAL.`

After that Core release is healthy, the Venice step-up phrase is:

`BEGIN ONE VERIFIED venice-media-local:activate-release-slot COLD ACTIVATION FOR D:\eva-phase5h\stage\slots\venice-26.7.6-b2b3ce8e-0717aff4e386; REQUIRE THE EXACT DISPLAYED BINDINGS, TWO FRESH COLD-STATE SAMPLES, SINGLE-USE AUTHORIZATION, TRANSITION LOCK, POST-ACTIVATION HEALTH, AND AUTOMATIC 26.6.5 ROLLBACK; DO NOT RUN CATALOG, MEDIA, IMPORT, CORE RESTART, OR PHASE 6 WORK.`
