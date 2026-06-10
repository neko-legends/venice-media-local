# Troubleshooting Remote Access to VeniceMediaLocal Control Server

This document captures the diagnostic pattern that emerged when the embedded HTTP control server was running correctly but unreachable from the Hermes agent over Tailscale.

## When the Server Appears Up But the Agent Can't Reach It

### Confirmed working on target machine
- Local loopback test succeeds: `curl -H "Authorization: Bearer *** http://127.0.0.1:<port>/api/v1/state`
- Self-Tailscale-IP test succeeds from the target machine: `curl ... http://<target-tailscale-ip>:<port>/api/v1/state`
- `netstat` / `Get-NetTCPConnection` shows `0.0.0.0:<port>` LISTENING on the correct process
- Discovery file written and token visible in Settings

### Tests to run from the agent side (Linux)
Run these in order:

1. `tailscale ping <target-hostname-or-ip>`  
   - If this fails → Tailscale connectivity or ACL problem.

2. Verbose curl with short timeout:
   ```bash
   curl -v --connect-timeout 8 -H "Authorization: Bearer *** http://<target-ip>:<port>/api/v1/state
   ```

3. Raw TCP check:
   ```bash
   nc -vz <target-ip> <port>
   ```

### Common root causes when Tailscale ping works but port is blocked

- **Windows Firewall on the target** (most frequent in this household)
  - Generic "allow the exe" rules often do not cover traffic arriving on the Tailscale virtual adapter.
  - Fix: Either temporarily disable Windows Firewall for testing, or create an explicit inbound TCP rule for the configured port allowing the specific source Tailscale IP of the agent machine.

- Tailscale peer connection type
  - Check in the Tailscale GUI on the target: is the agent peer showing as **Direct** or **Relay**?
  - Relay connections can be less reliable for non-standard ports.

- Tailscale ACLs on the tailnet
  - The target machine's ACLs may restrict inbound traffic to specific ports or source identities.

- Container / namespace isolation on the agent side
  - If Hermes is running inside Docker/WSL2/LXC without the host's Tailscale routes, it will have no route to the target even if `tailscale ping` from the host succeeds.

## Recommended Diagnostic Flow (for future sessions)

When a user reports "I enabled the toggle but you can't reach it":

1. Ask the user to run the local 127.0.0.1 test and self-Tailscale-IP test on the target.
2. Run `tailscale ping <target>` from the agent machine.
3. Run the verbose curl + nc from the agent.
4. Ask for the Direct vs Relay status of the peer in the target's Tailscale GUI.
5. If network path is confirmed good but port is dead → guide the user through a Windows Firewall off test.

This pattern prevents wasting time debugging the Rust code when the problem is infrastructure between the two Tailscale nodes.

## Reference Commands (copy-paste ready)

From agent:
- `tailscale ping ripper`
- `curl -v --connect-timeout 8 -H "Authorization: Bearer *** http://<ip>:<port>/api/v1/state`
- `nc -vz <ip> <port>`

From target (Windows):
- Local: `curl -H "Authorization: Bearer *** http://127.0.0.1:<port>/api/v1/state`
- Self Tailscale: `curl -H "Authorization: Bearer *** http://<own-tailscale-ip>:<port>/api/v1/state`

## Related

See `references/implementation-notes.md` for the server implementation details.
