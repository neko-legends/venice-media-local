# venice-local-gui skill for Hermes

This skill allows your Hermes agent (or compatible skill loader) to control a running Venice Media Local app over the network — especially over Tailscale — so generations, edits, and other media actions appear live in the open GUI window on the target machine (theater mode).

## Installation

1. In Venice Media Local, go to **Settings > AI Agent Control** and turn on **Enable AI Agent Remote Control**.
2. Copy this entire folder (`agent-skills/venice-local-gui/`) into your `~/.hermes/skills/` directory.
3. In `~/.hermes/config.yaml`, add a `venice_local` section with the Tailscale IP, port, and locally provisioned credential. Discovery provides the address and credential fingerprint, never the credential value.

Once installed, you can say things like:

> "Use the local app on the ripper to generate 4 variants of a cyberpunk cat with flux-2-max"
> "Edit that image — put the cat in Venice"
> "Upscale the result 2x"

Results appear live in the open Venice Media Local window. The agent handles the full generate→edit→upscale chain.

## What's in this folder

```
SKILL.md                                     ← main skill (load this into Hermes)
references/
  implementation-notes.md                    ← Rust wiring, event emission, design decisions
  remote-access-troubleshooting.md           ← Tailscale ACL, firewall, discovery file edge cases
  venice-local-gui-request-types.md          ← exact API request shapes + proven examples
```

## Key pitfalls (read these before first use)

**Cache the dataUrl after every generate call.** The generate response includes a base64 `dataUrl` for each output. The edit endpoint needs a dataUrl — not a file path. Save it to a temp file immediately so the agent can chain generate→edit without a redundant re-generation.

**Send edit payloads via a file.** Edit bodies contain the full base64 dataUrl and can be 300–400 KB — too large for shell arguments. Always write the JSON to a temp file and use `--data-binary @file` with curl.

See `SKILL.md` and `references/venice-local-gui-request-types.md` for the full documented workflow.
