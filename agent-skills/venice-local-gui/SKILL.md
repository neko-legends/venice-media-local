---
name: venice-local-gui
description: Generate and edit images (and queue video, music, sound effects, speech) through the human's own running Venice Media Local app instead of asking for a Venice API key. Use whenever a task needs AI image generation or image editing, or any Venice media model. Results appear live in the open app window and are saved in its output folder.
tags: [venice, media, image-generation, image-edit, gui, remote-control, tailscale]
---

# Venice Media Local: agent control

The human runs **Venice Media Local** (a desktop app) on their Windows machine. It holds their Venice API key and
exposes a small HTTP control API. **You never need a Venice API key**: you send requests to the app, it calls Venice,
saves the result in its output folder, and shows the card live in its window (the human watches it happen).

Use this skill for any AI image work (new images, edits, combining images, background removal, upscaling) and for
Venice video, music, SFX, speech and transcription.

## 1. Connect

You need two things from the human: the **control address** (e.g. `http://100.64.x.x:9876`, a Tailscale IP) and the
**control token**. The app shows the address in **Settings > AI Agent Control** (toggle "Enable AI Agent Remote
Control"); the token is provisioned there and is never returned by the API or the discovery file.

- Ask the human once if you don't have them. Keep them **out of every repo and commit**: pass them as environment
  variables (`VML_URL`, `VML_TOKEN`) for the command, or keep them in `~/.config/venice-media-local/agent.json`
  (`{"url": "...", "token": "..."}`), which `scripts/vml.py` reads.
- **Where to run calls:** from a machine on the human's tailnet. A cloud sandbox usually can't reach a `100.64.x.x`
  address; if you have a shell on the human's own computer (a linked device), run the calls there.
- Check it's up: `python3 scripts/vml.py state` (version, key configured, output folder, model counts).

How to read failures:

| What you see | Meaning |
|---|---|
| `404` with an empty body | Wrong path. Everything is under **`/api/v1/`** (`/api/v1/state`, not `/state` or `/health`). It is not an auth problem. |
| `401 Invalid bearer token` | Wrong token. Ask the human for the current one. |
| Connection refused / timeout | App closed, agent control toggled off, or no route to the tailnet. See `references/remote-access-troubleshooting.md`. |

## 2. Do the work: `scripts/vml.py` (stdlib Python, recommended)

```bash
python3 scripts/vml.py state
python3 scripts/vml.py models edit                       # model ids the app lists (see the note on edit models below)
python3 scripts/vml.py generate "a voxel cat knight at dawn" --aspect 16:9 --variants 2 --out out/
python3 scripts/vml.py edit "open the moon's eyes, keep everything else the same" --image ending.png --out out/
python3 scripts/vml.py compare "same instruction" --image ending.png --models gpt-image-2-5-flare-edit,nano-banana-2-1-edit --out out/
```

It saves every result to `--out` (`gen_0.png`, `edit_0.png`, `<model>_0.png` ...), prints a short JSON summary with
timings and the file the app saved (`appFile`), and never prints image data. `compare` runs the same edit through
several models in parallel so you can show the human a side-by-side with real timings.

### Default models (the house picks; change them when better ones ship)

| Job | Default | Notes |
|---|---|---|
| Generate (text -> image) | `gpt-image-2-5-flare` | |
| Edit (image + instruction) | `gpt-image-2-5-flare-edit` | Chosen 2026-10-09 over `nano-banana-2-1-edit` in a side-by-side: brighter, happier, kept the scene intact; ~20 s vs ~16 s for 1280x720. |

The defaults live at the top of `scripts/vml.py` (`DEFAULT_MODELS`); `--model` overrides per call. When the human
asks to try a new model, run `compare` against the current default and let them pick; then update the table and
`DEFAULT_MODELS`. See `references/models.md` for how to find model ids.

**Edit models the app doesn't list still work.** The app's edit list (`models edit`) only includes Venice models it
classifies as edit-capable image models. Venice's newer edit models are typed `inpaint` by Venice (e.g.
`gpt-image-2-5-flare-edit`, `nano-banana-2-1-edit`, `nano-banana-pro-edit`) and may be missing from that list, but
`/api/v1/edit-image` passes the model id straight to Venice, so they work when named. The public list of Venice
model ids is `https://api.venice.ai/api/v1/models?type=all` (no key needed).

## 3. Rules that save time and credits

- **Edits take images as data URLs, not file paths** (`data:image/png;base64,...`, up to 3 images). `vml.py` does
  this for you. If you call the API yourself, write the JSON body to a file and send it with
  `curl --data-binary @body.json`: the body is far too big for a command-line argument.
- **Responses are huge.** Each result carries the full image as a base64 `dataUrl`, and edit results also echo the
  input images back under `metadata.request.images`. Never print a raw response; extract `dataUrl` and write the file.
- **Keep every result.** The edit endpoint needs the image itself, so save outputs as you go to chain
  generate -> edit -> upscale without regenerating. To get an image back later, re-run generate with the same `seed`
  or read the file from the app's output folder.
- **Output size can differ from the input** (a 1280x720 input came back 1536x864). Resize to the size you need before
  placing it in a project.
- **For a faithful edit**, say what must stay the same ("keep everything exactly the same: ... Only change ...").
- One request at a time per image job is fine; the app serializes heavy work. Edits take ~15-25 s.
- Never burn/clear the human's output folder (`burn-folder`, `clear-results`, `move-to-burn`) without their explicit OK.
- After a visible action, tell the human it's in the app: "It's in the Venice Media Local window now: N images from
  <model>." (They like watching the cards appear: "theater mode".)

## 4. The API (all under `http://<host>:<port>/api/v1`, header `Authorization: Bearer <token>`)

Images: `POST /generate-image` {model, prompt, variants 1-4, aspectRatio?, resolution?, seed?, negativePrompt?,
format?} · `POST /edit-image` {model, prompt, images:[dataUrl x1-3], aspectRatio?, resolution?} ·
`POST /remove-background` {sourceImage} · `POST /upscale-image` {sourceImage, scale: 2|4}

Video/audio (queue, then poll): `POST /queue-video` · `POST /retrieve-video` {queueId} · `POST /queue-music` ·
`POST /queue-sfx` · `POST /retrieve-audio` {queueId} · `POST /generate-speech` · `POST /transcribe-audio`

App: `GET /state` · `POST /refresh-models` · `POST /navigate` · `POST /open-output-folder` · `POST /open-burn-folder`
· `POST /clear-results` · `POST /burn-folder` · `GET /burn-folder-stats` · `POST /move-to-burn` {paths}

Any POST may add `"navigate": true` to switch the app to the matching tab. Exact request structs and examples:
`references/venice-local-gui-request-types.md`. Server internals and how to add endpoints:
`references/implementation-notes.md`. The routes above are revision-1 compatibility routes; revision-2 operations
(event replay, sealed uploads, artifact streaming, transfer grants) are described in the implementation notes.

## 5. Hermes users

Hermes can keep the connection in `~/.hermes/config.yaml`:

```yaml
venice_local:
  target_host: "100.64.x.x"   # Tailscale IP of the machine running the app
  port: 9876
  token: "<locally provisioned>"
```

and export it as `VML_URL` / `VML_TOKEN` before running `scripts/vml.py`.
