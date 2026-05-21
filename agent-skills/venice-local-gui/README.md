# venice-local-gui skill for Hermes

This skill allows your Hermes agent (or similar AI) to control a running Venice Media Local app over the network (especially over Tailscale).

## Installation

1. In Venice Media Local, go to Settings and turn on **AI Agent Control**.
2. Copy this entire folder (`agent-skills/venice-local-gui`) into your `~/.hermes/skills/` directory.
3. Tell your agent the Tailscale IP of the machine running the app (or let it read the `control-api.json` discovery file).

Once installed, you can say things like:

"Use the local app on the ripper to generate 4 variants of a cyberpunk cat"

The results will appear live in the open Venice Media Local window.

See SKILL.md for full details.
