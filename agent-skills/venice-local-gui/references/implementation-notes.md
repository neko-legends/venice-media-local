# Venice Local GUI — Implementation Notes (May 2026)

This captures the working architecture for remote control of the Tauri app so agents can drive it and the human sees live results in the open GUI ("theater mode").

## Key Design Decisions (user preferences encoded)
- **Off by default + live toggle** (no restart): The setting `enable_agent_control` + `agent_control_token` live in AppSettings. `save_settings` detects the flip and calls `start_agent_control_server` / `stop_agent_control_server` immediately.
- **HTTP inside the Tauri process** (not separate binary or full MCP): This is the only way results appear in the *open* Windows GUI via the existing internal functions. The agent calls over Tailscale; the human watches cards appear.
- **Exact UI text** (confirmed with user):
  - Section: "AI Agent Control"
  - Toggle: "Enable AI Agent Remote Control"
  - Help: "Starts a local HTTP API. AI agents on the same Tailscale network (recommended) or trusted local LAN can trigger generations and edits. Results appear live in this window. A discovery file is written automatically."
- Token shown with copy button only when the toggle is on.
- Discovery written to the real app_data_dir as `control-api.json`.
- Skill bundled in-repo at `agent-skills/venice-local-gui/` for open-source parity.

## Current Router (expanded by user)
The router now includes a wide surface (as of the May 2026 update):

```rust
.route("/api/v1/state", get(agent_get_state))
.route("/api/v1/navigate", post(agent_navigate))           // NEW - switch tabs/modes
.route("/api/v1/generate-image", post(agent_generate_image))
.route("/api/v1/edit-image", post(agent_edit_image))
.route("/api/v1/remove-background", post(agent_remove_background))
.route("/api/v1/upscale-image", post(agent_upscale_image))
.route("/api/v1/queue-video", post(agent_queue_video))
.route("/api/v1/retrieve-video", post(agent_retrieve_video))
.route("/api/v1/queue-music", post(agent_queue_music))
.route("/api/v1/queue-sfx", post(agent_queue_sfx))
.route("/api/v1/retrieve-audio", post(agent_retrieve_audio))
.route("/api/v1/generate-speech", post(agent_generate_speech))
.route("/api/v1/transcribe-audio", post(agent_transcribe_audio))
.route("/api/v1/refresh-models", post(agent_refresh_models))
.route("/api/v1/burn-folder", post(agent_burn_folder))
.route("/api/v1/move-to-burn", post(agent_move_to_burn))
... (additional management endpoints)
```

Many handlers now call `emit_agent_navigate()` and `emit_agent_results()` so the GUI can auto-switch tabs and display live result cards.

## Navigation Feature
The `/api/v1/navigate` endpoint + `agent:navigate` Tauri event allows the agent to switch the active mode (image, edit, video, settings, etc.). Some generation handlers automatically trigger navigation for better "theater mode" experience.

## Debugging Remote Access (Tailscale + Windows)
When the server is listening locally but not reachable over Tailscale:
1. Confirm local `127.0.0.1:<port>` works on the Windows machine.
2. Confirm the machine can reach itself on its own Tailscale IP.
3. Run `tailscale ping <other-machine>` both directions.
4. Add specific Windows Firewall inbound rule (configured TCP port, Tailscale interface, exact remote Tailscale IP).
5. Add Tailscale ACL allowing the source tag/machine to the destination on `tcp:<port>`.

This pattern was required to make the feature work between the Linux Hermes machine and the Windows "ripper".

## Safety & Preferences Captured
- Burn only with explicit human confirmation.
- Always say "Check the Venice Media Local window on the ripper" after remote actions.
- Prefer this over direct Venice API when the human wants to *see* the work happening in the familiar GUI.

## How the Repo Bundle Works
The VeniceMediaLocal repo ships `agent-skills/venice-local-gui/SKILL.md` + README. Users copy it into `~/.hermes/skills/creative/venice-local-gui/`.

This skill is now the authoritative reference for controlling the local Venice GUI.
