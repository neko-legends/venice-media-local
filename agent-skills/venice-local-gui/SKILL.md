---
name: venice-local-gui
description: Control a running VeniceMediaLocal desktop app over the network (Tailscale) so generations and edits appear live in the open GUI on the target Windows machine. The "theater mode" for agent-driven media work.
tags: [venice, media, gui, remote-control, tailscale]
---

# Venice Local GUI Control Skill

This skill lets the Hermes agent drive a **live VeniceMediaLocal GUI** running on another machine (e.g. the Windows "ripper" on the same Tailscale network).

The human sitting at the Windows machine will see cards, progress, and results appear in the app exactly as if they were using it themselves.

## Core Idea

- The Windows app runs normally (user has it open).
- It exposes a small HTTP control API on the Tailscale network.
- This skill teaches you (the agent) the API and gives you reliable ways to call it.
- All file saving, burn logic, model cache, and output organization still happen exactly as in the GUI.

See `references/implementation-notes.md` for the exact Rust wiring, live-toggle logic, how to add more endpoints, and the captured "theater mode" design decisions (including the exact UI text the user wanted and the preference for embedded HTTP so the human literally watches cards appear in the open app).

## Configuration (Human sets this once)

In the VeniceMediaLocal app, go to **Settings > AI Agent Control** and turn on the toggle labeled **"Enable AI Agent Remote Control"**.

The help text (exact user-preferred wording) is:

"Starts a local HTTP API. AI agents on the same Tailscale network (recommended) or trusted local LAN can trigger generations and edits. Results appear live in this window. A discovery file is written automatically."

When enabled, the app shows the listening address. Provision the bearer credential through the local app; it is stored in the OS credential store and is not returned by the HTTP API.

Then store the target:

**Preferred:** Add to `~/.hermes/config.yaml` under a `venice_local` section (example from verified 2026-05-21 instance):

```yaml
venice_local:
  target_host: "100.64.x.x"   # Tailscale IP of the Windows machine
  port: 9876                  # default; use the discovery file/settings value
  token: "<locally provisioned>" # never read this from discovery or API state
  # output dir on Windows (from /api/v1/state): C:\Users\you\Desktop\VeniceMedia
```

Fallback: use the memory tool with the same keys.

The app also writes the discovery file `%APPDATA%\community.venice.media.local\control-api.json` on startup when the feature is on. Use its `address`, `port`, manifest/health URLs, and credential fingerprint. It intentionally contains no bearer credential.

## Available Control Endpoints (v1)

All endpoints live under `http://<host>:<port>/api/v1`. The routes below are revision-1 compatibility routes; new automation should use revision-2 operations, event replay, sealed uploads, and artifact streaming.

Revision-2 transfers require both `X-Transfer-Grant-ID` and `X-Transfer-Grant`. Never reuse a broad operation secret: each grant is exact-bound to operation/attempt/assignment/capability, method/path/scope, resource identity and media facts, validity interval, and use count. Upload IDs and hashes must already appear in the admitted `inputArtifacts`; completing staging cannot add or alter operation input.

Auth header (required for now):
`Authorization: Bearer ***

### Core actions (these update the live GUI when we emit events)

- `POST /api/v1/generate-image`
  Body: ImageGenerationRequest (see types below)

- `POST /api/v1/edit-image`          (ImageMultiEditRequest - body: model, prompt, images: [dataUrl], aspectRatio?)
- `POST /api/v1/remove-background`
- `POST /api/v1/upscale-image`

- `POST /api/v1/queue-video`
- `POST /api/v1/retrieve-video`   (poll with queue_id)

- `POST /api/v1/queue-music`
- `POST /api/v1/queue-sfx`
- `POST /api/v1/retrieve-audio`

- `POST /api/v1/generate-speech`
- `POST /api/v1/transcribe-audio`

### Management

- `GET  /api/v1/state`                 → full AppState (models, settings, key status)
- `POST /api/v1/refresh-models`
- Model discovery is available through revision-2 `media.models.list` operations. There is no `/api/v1/models` route.
- `POST /api/v1/navigate`
- `POST /api/v1/open-output-folder`
- `POST /api/v1/open-burn-folder`
- `POST /api/v1/clear-results`
- `POST /api/v1/burn-folder`           { seed?: string }
- `GET  /api/v1/burn-folder-stats`
- `POST /api/v1/move-to-burn`          { paths: string[] }
- `POST /api/v1/actions/shutdown`       dedicated release action; requires the server-assigned `application:shutdown` permission independently of the body scope, exact instance/manifest binding, <=60 second validity, no successful replay or forced fallback

### Types (authoritative, kept current)

See the dedicated support file for the exact structs extracted from the live app source during real usage:

`references/venice-local-gui-request-types.md`

This file contains the full ImageGenerationRequest, AgentRequest wrapper, proven examples, and the verification-first flow that succeeded in practice.

## How to actually call it (agent usage)

**Proven pattern (verified against real instance 2026-05-21):**
## How to actually call it (agent usage)

**Proven pattern (verified against real instance 2026-05-21):**

1. Always start with a quick health + model check:
   ```bash
   curl -H "Authorization: Bearer *** \
     http://<host>:<port>/api/v1/state
   ```
   This returns current models, settings, output dir, and confirms the server is reachable.

2. For image generation (primary use):
   Use the exact shapes in `references/venice-local-gui-request-types.md`.

   Minimal working example (succeeded in live test):
   ```bash
   curl -X POST \
     http://100.64.131.86:<port>/api/v1/generate-image \
     -H "Authorization: Bearer *** \
     -H "Content-Type: application/json" \
     -d '{
       "model": "flux-2-max",
       "prompt": "cyberpunk cat on a neon motorcycle",
       "variants": 2,
       "aspectRatio": "16:9"
     }'
   ```

3. **CRITICAL — save the dataUrl immediately after every generation:**

   The generation response contains the full base64 `dataUrl` of every output image.
   The edit endpoint requires a dataUrl — NOT a Windows file path.
   If you don't cache it, you will have to re-generate just to get the dataUrl back, wasting time and credits.

   Always do this right after a successful generate call:
   ```python
   import json
   result = json.loads(response_text)
   # result may be a list (one item per variant) or a single dict
   items = result if isinstance(result, list) else [result]
   for i, item in enumerate(items):
       path = f"/tmp/vml_last_generated_{i}.txt"
       with open(path, "w") as f:
           f.write(item["dataUrl"])
   # Also save name for reference
   with open("/tmp/vml_last_generated_name.txt", "w") as f:
       f.write(items[0].get("name", ""))
   ```

   Then for any subsequent edit, read it back:
   ```python
   with open("/tmp/vml_last_generated_0.txt") as f:
       dataurl = f.read().strip()
   ```

   For multi-variant generations, `_0`, `_1`, `_2`, `_3` correspond to each variant.
   These files persist for the session so you can chain generate → edit → upscale seamlessly.

4. **Sending edit payloads — always use a temp file, never --data-raw:**

   Edit payloads contain a full base64 dataUrl and will exceed shell argument limits.
   Always write the JSON body to a file and use `--data-binary @file`:
   ```python
   import json, subprocess
   payload = {"model": "...", "prompt": "...", "images": [dataurl]}
   with open("/tmp/vml_edit_payload.json", "w") as f:
       json.dump(payload, f)
   subprocess.run(["curl", "-s", "-X", "POST", url,
       "-H", "Authorization: Bearer ***
       "-H", "Content-Type: application/json",
       "--data-binary", "@/tmp/vml_edit_payload.json"], ...)
   ```

5. **Mandatory theater-mode feedback** (user preference, do not skip):

3. For image editing (`POST /api/v1/edit-image`):

   The `images` field requires **base64 data URLs** (e.g. `data:image/webp;base64,...`), NOT Windows file paths.

   **PITFALL:** Data URLs are hundreds of KB — far too large for curl's `--data` / `-d` argv. Always write the body to a temp file and use `--data-binary @/tmp/payload.json`:

   ```python
   import json, subprocess, tempfile
   payload = {
       "model": "grok-imagine-quality-edit",
       "prompt": "place the cat in Venice, Italy...",
       "images": [dataurl],   # full base64 data URL string
       "navigate": True
   }
   with open('/tmp/edit_payload.json', 'w') as f:
       json.dump(payload, f)
   subprocess.run(["curl", "-s", "-X", "POST",
       "http://<host>:<port>/api/v1/edit-image",
       "-H", "Authorization: Bearer ***
       "-H", "Content-Type: application/json",
       "--data-binary", "@/tmp/edit_payload.json"],
       capture_output=True, text=True, timeout=120)
   ```

   **Recovery tip:** If you didn't capture the dataUrl from generation, re-generate with the same `seed` value to get an identical image and its dataUrl. The seed is returned in the generation response.

4. **Mandatory theater-mode feedback** (user preference, do not skip):
   After any successful remote action (generate, edit, upscale, queue, etc.) say:
   > "Check the Venice Media Local window on the ripper — I just [generated N variants / edited / queued ...] using [model] and they should be visible now."

   This is the core of the "theater mode" experience the user wants.

The response from generate/edit endpoints contains the saved file paths on the Windows machine.

See `references/venice-local-gui-request-types.md` for the complete, up-to-date request structs and the recommended verification-first flow.

## Current Status (as of 2026-05-21 verification)

- Live toggle in Settings (off by default, starts/stops HTTP server immediately on flip, no app restart): **done**
- HTTP server on the configured Tailscale/loopback bind (or explicit all-interface opt-in) with constant-time bearer authentication and OS-protected credential storage: **done**
- Discovery file `control-api.json` written to app data dir automatically when enabled: **done**
- Core endpoints implemented and wired directly to the real internal Rust functions (GUI updates live):
  - GET /api/v1/state — **verified working**
  - POST /api/v1/generate-image — **verified working** (with full event emission for live cards)
  - POST /api/v1/refresh-models — **verified working**
- POST /api/v1/edit-image — **verified working** (body: model, prompt, images:[dataUrl]; images field requires base64 data URLs, NOT file paths; payload too large for curl argv — must write to file and use --data-binary @file)
- Additional endpoints (remove-background, upscale, video/audio queues, speech, burn, navigate): available and follow the same patterns (verified via source inspection)
- Tauri event emission for instant card updates in GUI: **working in practice** (theater mode confirmed)
- Skill is bundled inside the VeniceMediaLocal repo at `agent-skills/venice-local-gui/` (with README for open-source install)
- New support file `references/venice-local-gui-request-types.md` added with exact Rust structs and proven usage pattern from live session

This delivers the core "theater mode": the Hermes agent (on Linux via Tailscale) drives the live open Windows GUI and the human watches the cards appear in real time.

## Safety Rules for the Agent

- Never burn the folder without explicit human confirmation.
- When doing long-running queue/retrieve, give progress updates.
- If the target is unreachable, fall back to the normal direct `venice-media` skill and tell the human.
- Always surface the "Check the Venice Media Local window on the ripper" message after remote actions.

## Next Steps for the Human

1. Give the agent the discovery address plus the separately provisioned credential. Never expect discovery or API state to reveal credential material.
2. The agent will store it in `~/.hermes/config.yaml` under `venice_local`.
3. Then you can say things like:
   "Use the local app on the ripper to generate 3 variants of a cyberpunk city at night with flux-2-max"

The agent will handle the rest and you will see it happen in the GUI.

---

**This skill is the single source of truth for the agent about how to talk to your local Venice GUI.**
