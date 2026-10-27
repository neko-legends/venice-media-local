# venice-local-gui agent skill

This skill lets an AI agent (Claude, Hermes, or any agent that reads SKILL.md files) control a running Venice Media Local app over the network — especially over Tailscale — so generations, edits, and other media actions appear live in the open GUI window on the target machine (theater mode).

## Installation

1. In Venice Media Local, go to **Settings > AI Agent Control** and turn on **Enable AI Agent Remote Control**.
2. Copy this entire folder (`agent-skills/venice-local-gui/`) into your agent's skills directory (e.g. `~/.claude/skills/` or `~/.hermes/skills/`).
3. Give the agent the control address and token: environment variables `VML_URL` / `VML_TOKEN`, or `~/.config/venice-media-local/agent.json` (`{"url": ..., "token": ...}`), or the `venice_local` section of `~/.hermes/config.yaml`. Discovery provides the address and credential fingerprint, never the credential value. Never commit the token.

Once installed, you can say things like:

> "Use the local app on the ripper to generate 4 variants of a cyberpunk cat with flux-2-max"
> "Edit that image — put the cat in Venice"
> "Upscale the result 2x"

Results appear live in the open Venice Media Local window. The agent handles the full generate→edit→upscale chain.

## What's in this folder

```
SKILL.md                                     ← main skill
scripts/
  vml.py                                     ← helper: state, models, generate, edit, compare (stdlib Python)
references/
  models.md                                  ← default models, how to find and compare new ones
  implementation-notes.md                    ← Rust wiring, event emission, design decisions
  remote-access-troubleshooting.md           ← Tailscale ACL, firewall, discovery file edge cases
  venice-local-gui-request-types.md          ← exact API request shapes + proven examples
```

## Key pitfalls (read these before first use)

**Cache the dataUrl after every generate call.** The generate response includes a base64 `dataUrl` for each output. The edit endpoint needs a dataUrl — not a file path. Save it to a temp file immediately so the agent can chain generate→edit without a redundant re-generation.

**Send edit payloads via a file.** Edit bodies contain the full base64 dataUrl and can be 300–400 KB — too large for shell arguments. Always write the JSON to a temp file and use `--data-binary @file` with curl.

See `SKILL.md` and `references/venice-local-gui-request-types.md` for the full documented workflow.
