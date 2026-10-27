# Choosing models

- **House defaults** live in `scripts/vml.py` (`DEFAULT_MODELS`) and the table in `SKILL.md`. Update both together.
- **What the app lists:** `python3 scripts/vml.py models [image|edit|video|music|sfx|voice|transcribe]`
  (refresh first with `POST /api/v1/refresh-models` if a model just launched).
- **What Venice offers:** `curl -s "https://api.venice.ai/api/v1/models?type=all"` (public, no key). Image edit models
  are `type: "inpaint"` there and often end in `-edit`; the app may not list them, but `/api/v1/edit-image` accepts
  any id Venice accepts.
- **Trying a new model:** `python3 scripts/vml.py compare "<instruction>" --image <file> --models <current default>,<new>`
  and show the human the outputs side by side with the timings. Let them choose.

## Comparisons on record

| Date | Task | Models | Result |
|---|---|---|---|
| 2026-10-09 | Edit a 1280x720 storybook illustration (open a sleeping moon's eyes, keep the rest) | `gpt-image-2-5-flare-edit` 21.1 s, `nano-banana-2-1-edit` 15.7 s | Human picked flare: brighter, happier look; both kept the characters intact. Outputs came back larger (1536x864 / 1365x768). |
