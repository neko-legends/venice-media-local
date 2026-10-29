---
name: ai-film-from-unity
description: Make a consistent AI short film or music video from a Unity scene: render Unity layouts, build cast sheets and keyframes with GPT Sunburst edit, animate with Flux 3 video (incl. first/last frame), add Venice music, subtitles and a Unity-vs-AI before/after slider, all through Venice Media Local.
---

# AI film / music video from a Unity scene

This is how the Neko Legends films were made: "Kuro" (3D CG), "The Last Roll" (realistic) and the
「遠くても、抱きしめて」 music video (realistic, snowy). All used the AnimeStreet scene from the
Anime Japanese Shopping Street asset as the set. The pitch: a stable 3D scene gives every shot the same
buildings, signs and geography, so the AI film stays consistent.

## Tools

- **Venice Media Local (VML)**: the human's desktop app; it holds the Venice key. Use the
  `venice-local-gui` skill (`scripts/vml.py`) for the connection, routes and pitfalls. Calls must run from a
  machine on the human's tailnet (the linked-device shell), not from a cloud sandbox.
- **Unity MCP** (`execute_code`): renders layout frames from the open scene.
- **ffmpeg + Python/PIL**: on the human's machine for the edit, subtitles and end cards.

## Models that worked

| Job | Model | Notes |
|---|---|---|
| Cast sheets, keyframes | `gpt-image-2-5-sunburst-edit` (max 3 input images) | Turns a low-detail Unity render into a real-looking place while keeping the layout. Output is 1536x864. |
| Text-only images | `gpt-image-2-5-sunburst` | |
| Video | `flux-3-max-image-to-video` | 5–20 s in 1 s steps, 720p/1080p. 1080p comes back as 1920x1088, so crop to 1080. |
| Start/end-frame video | `flux-3-first-last-frame-to-video` | 5/10/15/20 s only. Send `endImage` (needs VML ≥ 26.10.27, which forwards it as `end_image_url`). |
| Music | `elevenlabs-music-v2-5` via `/queue-music`, `forceInstrumental: true` | Cues came back ~59 s regardless of the requested length. Plan one cue per section. |
| Transcription | `elevenlabs/scribe-v2` via `/transcribe-audio`, `responseFormat: "json"`, `timestamps: true` | Word timings for subtitles and to confirm lines were spoken. `verbose_json` is rejected. |

**Flux 3 Image (`flux-3-image-edit`) is not a substitute for Sunburst when the input is a Unity render.**
In a side-by-side it kept the game-render look and pasted realistic people on top. It is fine for images
with no layout reference. A Sunburst source animates just as well in Flux video.

## Pipeline

1. **Story first.** Pick a short story with a setup, a reversal and a payoff, and explain *why* it is
   emotional before building. Prefer quiet human or animal moments: in realistic style, danger or
   injury prompts often trip the content filter.
2. **Find the set in Unity.** List buildings
   (`FindObjectsByType<NekoLegends.BuildingPhysics.BuildingStructure>` gives name, position and facing).
   Vary the shops across shots; don't reuse one hero shop everywhere.
3. **Render layouts.** Temporarily move `Camera.main`, render to a 1920x1080 RenderTexture, save a PNG, and
   restore the camera. Probe several angles, look at a contact sheet, then keep the good ones.
   Interiors exist (counters, stairs, upstairs rooms) and are rough but usable as layouts.
   Day renders are fine: Sunburst relights them to night or snow.
4. **Cast sheet.** One turnaround image per character (front/side/back/close-up, plain grey background,
   all outfits they'll wear), then combine them into one `cast_sheet.png` (a 2x2 or stacked grid). If the
   human gives a character image, derive the sheet from it with an edit.
5. **Keyframes** (Sunburst edit). Inputs: `[layout render, cast sheet, optional look anchor]`. The prompt
   pattern that worked:
   - a style line (e.g. "Photorealistic live-action ... 35mm Kodak Portra", or "Pixar-quality 3D CG")
   - "Image 1 is the set and camera layout: keep the same buildings, shop signs, street, lamps and camera
     angle, but turn it into a real photographed location."
   - "Image 2 is the character sheet; keep their faces, hair and outfits exactly."
   - the shot action, then the lighting/weather line (snow, dusk, summer flashback and so on)

   Review every keyframe on a contact sheet before spending video credits.
6. **Video** (Flux). The prompt is: a style line saying "keep the people, set and look exactly as in the
   image", then the motion and the dialogue in Japanese inside 「」, then sound.
   - **Audio is always generated and cannot be turned off.** End every prompt with
     "No music, no score, no soundtrack, no background music. Only dialogue and natural sound effects."
     For a music video use "No dialogue, no speaking, no singing, no music" and mute the clips.
   - Use **first/last-frame** wherever a shot must end on an exact picture: reveals (a photo developing
     in a tray), hugs, a calendar filling up, and joining two shots (end frame = the next shot's first
     frame, or start frame = the previous clip's last frame, extracted with
     `ffmpeg -sseof -0.05 ... -frames:v 1`).
   - Queue every shot, then poll `/retrieve-video` from parallel threads. Clips take ~2–4 min; FLF takes
     longer.
7. **Check the clips.** Make 6-frame strips per clip and look at them. Transcribe the dialogue clips:
   the transcript confirms each line was spoken and gives exact subtitle times. Regenerate only the
   broken shots.
8. **Music.** One cue per section (setup / tension / ending, plus one for the end cards if needed).
   Normalize with loudnorm (bed about -26 to -27 LUFS, dialogue about -20), add fades, and place cues
   with `adelay`. Leaving a key dialogue moment without music can land harder.
9. **Edit.**
   - Normalize each clip: scale/crop to 1920x1080, 24 fps, trim, and fade where scenes change time
     ("翌朝 / The next morning", "数週間後 / Weeks later" cards).
   - Subtitles are an ASS file burned in with `ass=subs.ass:fontsdir=assets`. Use Zen Maru Gothic for
     Japanese and Poppins for English. Check each font's real family name with `fc-scan`; a wrong name
     silently falls back. Poppins has no "→" glyph.
   - The end sequence is rendered frame by frame with PIL and piped to ffmpeg:
     - a "Made with AI" card naming the Unity asset as the set and layout, and saying AI content isn't included
     - a **before/after slider** (Unity render vs. finished keyframe, with a wipe handle) over about 4 shots
     - a logo card
   - Concatenate with the concat demuxer, then mux the audio mix.
   - For a music video, the song is the timeline: transcribe it for line timings, place each shot to its
     lyric line, and use Japanese plus English lyric subtitles.
10. **Verify** by grabbing frames at key timestamps (title, subtitles, cards, slider) and looking at them
    before delivering.

## Continuity rules learned the hard way

- **Screen direction.** Decide where everyone is (who comes from screen left or right) and make every
  keyframe obey it. A cat once "ran the wrong way" because the leap keyframe pointed away from the boy.
- **Sides of the body.** Say "her LEFT front paw (the paw on the RIGHT side of the image from the
  viewer's point of view)". The model flips left and right otherwise.
- **Characters must be set up before they appear.** Show the cat asleep on the vending machine in the
  wide shot before cutting to her waking up there.
- **Unprompted props and characters.** A cat appeared inside a shop where cats weren't allowed. Add
  "there is NO cat anywhere in this shot" when it matters.
- **Story gaps.** If a character walks away and then is suddenly held, add the missing beat (a witness
  explains, the next morning) instead of cutting around it.
- Keep text on screen readable and generic: no brand logos, no years (the calendar first showed "2024").

## Practical pitfalls

- Linked-device shell calls time out at about 180 s, and background processes are killed when the call
  ends. Keep each step under that: batch 6–10 image jobs per call, and split long encodes into two parts
  (for the second part, offset the subtitles with `setpts=PTS+O/TB,ass=...,setpts=PTS-O/TB`), then
  concatenate with `-c copy`.
- Deleting is blocked in connected folders, so move old files to an `old/` subfolder.
- `device_commit_files` rejected files of roughly 39 MB or more; send compressed audio (mp3) instead.
- Keep work folders outside the shipped asset package (e.g. `_StoreExcluded/`), or under a `~` folder,
  which Unity ignores.
- Never put the VML control token or address in a repo or a public skill.
