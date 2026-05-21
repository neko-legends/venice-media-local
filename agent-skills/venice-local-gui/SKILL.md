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

## Configuration (Human sets this once)

Add to your `~/.hermes/config.yaml` or pass via environment / memory:

```yaml
venice_local:
  target_host: "100.64.12.34"      # Tailscale IP of the Windows machine running the app
  port: 9876
  token: "super-secret-long-token-here"   # shown in the app Settings panel
  output_dir_hint: "C:\\Users\\flash\\Desktop\\VeniceMedia"   # optional, for your reference
```

Store the current target in memory so you don't have to ask every time:
- Use the `memory` tool with key `venice_local_target`.

The app writes a small discovery file on startup:
`%APPDATA%\\venice-media-local\\control-api.json`

Example content:
```json
{
  "address": "100.64.12.34:9876",
  "token": "the-token",
  "version": "2026.x"
}
```

## Available Control Endpoints (v1)

All endpoints are POST (except state) and live under `http://<host>:<port>/api/v1`

Auth header (required for now):
`Authorization: Bearer <token>`

### Core actions (these update the live GUI when we emit events)

- `POST /api/v1/generate-image`
  Body: ImageGenerationRequest (see types below)

- `POST /api/v1/multi-edit-image`
- `POST /api/v1/remove-background`
- `POST /api/v1/upscale-image`

- `POST /api/v1/queue-video`
- `POST /api/v1/retrieve-video`   (poll with queue_id)

- `POST /api/v1/queue-audio`
- `POST /api/v1/retrieve-audio`

- `POST /api/v1/generate-speech`
- `POST /api/v1/transcribe-audio`

### Management

- `GET  /api/v1/state`                 → full AppState (models, settings, key status)
- `POST /api/v1/refresh-models`
- `GET  /api/v1/models`
- `POST /api/v1/burn-folder`           { seed?: string }
- `GET  /api/v1/burn-stats`
- `POST /api/v1/move-to-burn`          { paths: string[] }

### Types (abbreviated — match the Rust structs)

See the full request structs in the VeniceMediaLocal repo:
- ImageGenerationRequest
- QueueMediaRequest
- SpeechRequest
- etc.

The skill will keep the latest shapes.

## How to actually call it (agent usage)

Preferred: Use the small client helper that this skill will provide.

Fallback (always works):
```bash
curl -X POST \
  http://100.64.12.34:9876/api/v1/generate-image \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "flux-2-max",
    "prompt": "cyberpunk cat on a neon motorcycle",
    "variants": 2,
    "aspect_ratio": "16:9"
  }'
```

The response will contain the saved file paths on the Windows machine.

After a successful call, tell the human:
"Check the Venice Media Local window on the ripper — I just generated two new images and they should be visible now."

## Current Status (as of this skill version)

- HTTP server + basic handlers: **in progress** (being built in the VeniceMediaLocal repo)
- Live GUI event emission so cards appear automatically: planned for v1
- Full parity with all 24 Tauri commands: goal
- MCP integration (so you can use normal mcp_venice tools that route here): future

## Safety Rules for the Agent

- Never burn the folder without explicit human confirmation.
- When doing long-running queue/retrieve, give progress updates.
- If the target is unreachable, fall back to the normal direct `venice-media` skill and tell the human.

## Next Steps for the Human

1. Tell me the current Tailscale IP of the "ripper" + the port/token once the app has the server running.
2. I will store it in durable memory.
3. Then you can say things like:
   "Use the local app on the ripper to generate 3 variants of a cyberpunk city at night with flux-2-max"

I will handle the rest and you will see it happen in the GUI.

---

**This skill is the single source of truth for the agent about how to talk to your local Venice GUI.**
