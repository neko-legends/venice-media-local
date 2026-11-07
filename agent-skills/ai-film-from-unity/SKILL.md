---
name: ai-film-from-unity
description: Make a consistent AI short film, music video or marketing clip from a Unity scene: render Unity layouts or choreographed mannequin takes, build cast sheets and keyframes with GPT Sunburst edit, animate with Flux 3 / Seedance / MiniMax Recast (incl. single-take video-to-video), add ElevenLabs music, subtitles and a Unity-vs-AI before/after slider, all through Venice Media Local.
---

# AI film / music video from a Unity scene

These films all used the AnimeStreet scene from the Anime Japanese Shopping Street asset as the set:
- "Kuro" (3D CG)
- "The Last Roll" (realistic)
- the 「遠くても、抱きしめて」 music video (realistic, snowy)
- "Bullet Time" (anime; a single-take 360° fight with real building destruction)

The pitch: a stable 3D scene gives every shot the same buildings, signs and geography, so the AI film stays consistent.

## Tools

- **Venice Media Local (VML)**: the human's desktop app; it holds the Venice key. Use the `venice-local-gui` skill
  (`scripts/vml.py`) for the connection, routes and pitfalls. Run calls from a machine on the human's tailnet (the
  linked-device shell), not from a cloud sandbox.
- **Unity MCP** (`execute_code`): renders layout frames and choreographed takes from the open scene.
- **ffmpeg + Python/PIL**: on the human's machine, for the edit, subtitles and end cards.

## Models that worked

| Job | Model | Notes |
|---|---|---|
| Cast sheets, keyframes | `gpt-image-2-5-sunburst-edit` (max 3 input images) | Turns a low-detail Unity render into a finished frame while keeping the layout and poses. Output is 1536x864. |
| Text-only images | `gpt-image-2-5-sunburst` | |
| Video | `flux-3-max-image-to-video` | 5–20 s in 1 s steps, 720p/1080p. 1080p comes back as 1920x1088, so crop to 1080. |
| Start/end-frame video | `flux-3-first-last-frame-to-video` | 5/10/15/20 s only. Send `endImage` (needs VML ≥ 26.10.27, which forwards it as `end_image_url`). |
| **Single-take video-to-video (stylized)** | `seedance-2-5-reference-to-video-basic` | Best result for "one continuous shot". Needs `aspectRatio` ("16:9"); up to 9 images, 3 videos, 3 audio (VML ≥ 26.10.29). Refer to inputs as `<Image N>` / `<Video 1>` in the prompt. 15 s at 1080p worked. Takes ~20 min. |
| **Video-to-video, keep the Unity look** | `minimax-h3-max-recast` | Replaces *people only* and keeps the background. Fails with "No people were found" if the source has no humans (use mannequins). Up to 4 reference images, resolution `768P`/`1080P`, 15 s input worked. Takes ~10 min. |
| Music | `elevenlabs-music-v2-5` via `/queue-music`, `forceInstrumental: true` | Cues come back ~59 s regardless of the requested length. A prompt with timestamped sections (0:00–0:08 …) is roughly honored. |
| Transcription | `elevenlabs/scribe-v2` via `/transcribe-audio`, `responseFormat: "json"`, `timestamps: true` | Word timings for subtitles and to confirm lines were spoken. `verbose_json` is rejected. |

Models that did **not** help:
- **`flux-3-image-edit` vs. Sunburst:** Flux 3 Image is not a substitute when the input is a Unity render. It kept
  the game look and pasted people on top. It is fine for images with no layout reference, and a Sunburst source
  animates just as well in Flux video.
- **`grok-imagine-video-to-video-private`:** input must be under ~10 s, and through VML it returned COMPLETED with no
  media.
- **Kling v4 (image and reference):** gave nothing better than Flux for these shots, and the reference model added a
  white vignette.

## Pipeline A: multi-shot film (story-driven)

1. **Story first.** Pick a short story with a setup, a reversal and a payoff, and explain *why* it is emotional
   before building. Prefer quiet human or animal moments: in realistic style, danger or injury prompts often trip
   the content filter (anime-style action fights went through fine).
2. **Find the set in Unity.** List buildings
   (`FindObjectsByType<NekoLegends.BuildingPhysics.BuildingStructure>` gives name, position and facing). The real
   storefront plane can sit about 1 m inside `WorldBounds` (awnings and aprons stick out), so check the chunk
   renderers. Vary the shops across shots; don't reuse one hero shop everywhere.
3. **Render layouts.** Temporarily move `Camera.main`, render to a 1920x1080 RenderTexture, save a PNG, and restore
   the camera. Probe several angles on a contact sheet, then keep the good ones.
   - Interiors exist (counters, stairs, upstairs rooms) and are rough but usable as layouts.
   - Day renders are fine: Sunburst relights them to night or snow.
4. **Cast sheet.** One turnaround image per character (front/side/back/close-up, plain grey background, all outfits
   they'll wear), combined into one `cast_sheet.png` (a 2x2 or stacked grid). If the human gives a character image,
   derive the sheet from it with an edit.
5. **Keyframes** (Sunburst edit). Inputs: `[layout render, cast sheet, optional look anchor]`. The prompt pattern
   that worked:
   - a style line (e.g. "Photorealistic live-action ... 35mm Kodak Portra", "Pixar-quality 3D CG", or
     "ufotable-quality anime, golden-hour rim light")
   - "Image 1 is the set and camera layout: keep the same buildings, shop signs, street, lamps and camera angle,
     but turn it into a real photographed location."
   - "Image 2 is the character sheet; keep their faces, hair and outfits exactly."
   - the shot action, then the lighting/weather line (snow, dusk, summer flashback and so on)

   Review every keyframe on a contact sheet before spending video credits.
6. **Video** (Flux). The prompt is: a style line saying "keep the people, set and look exactly as in the image",
   then the motion and the dialogue in Japanese inside 「」, then sound.
   - **Audio is always generated and cannot be turned off.** End every prompt with "No music, no score, no
     soundtrack, no background music. Only dialogue and natural sound effects." For a music video use "No dialogue,
     no speaking, no singing, no music" and mute the clips.
   - Use **first/last-frame** wherever a shot must end on an exact picture:
     - reveals (a photo developing in a tray), hugs, a calendar filling up
     - joining two shots: end frame = the next shot's first frame, or start frame = the previous clip's last frame
       (extract it with `ffmpeg -sseof -0.05 ... -frames:v 1`)
   - Queue every shot, then poll `/retrieve-video` from parallel threads. Clips take ~2–4 min; FLF takes longer.
7. **Check the clips.** Make 6-frame strips per clip and look at them. Transcribe the dialogue clips: the
   transcript confirms each line was spoken and gives exact subtitle times. Regenerate only the broken shots.
8. **Music, edit, end cards:** see "Editing and end sequence" below.

## Pipeline B: single continuous take (360° orbit, bullet time, choreography)

When the point is to show the street staying consistent, the whole move must come from **one** video generation.
Stitching several clips was rejected: it looks like separate shots.

1. **Choreograph stand-in mannequins in Unity.**
   - Use the Amplify Animation Pack `SK_Mannequin` rig with its own clips. Retargeting the clips onto other
     characters distorted the poses, and hand-posing via `HumanPoseHandler` muscles looked broken.
   - Give mannequins plain URP Lit materials on **every** `sharedMaterials` slot (otherwise they render magenta).
     Use light grey for the hero and dark grey for the villain, so the AI can tell them apart.
2. **Poses must read as heroic, not ragdoll.**
   - Raw clip sampling has no IK, and frames caught mid-transition look limp. Instead, hold strong *peak* frames:
     sample a tiny range (e.g. c=0.42→0.48 of the clip) over 1–1.5 s, lift the body 0.3–0.9 m into the air, then
     **snap** to the next pose.
   - That gives the "glitching between poses, closer each time" look, and the AI animates the in-betweens.
   - Make a 4x4 pose-candidate contact sheet before choosing. Good airborne poses: Jump_in_Place ~0.45,
     Roll_Fwd ~0.3 (flip), Roll_L ~0.3 (inverted cartwheel), SS_Roll ~0.35 (barrel roll), Front_Kick ~0.4
     (lean-back dodge).
3. **Contact must be exact (kicks, punches).**
   - Don't guess distances. Sample the attacker at the strike's peak frame and read the foot bone position. Read
     the target's `Head` bone, then offset the attacker so foot = head (plus ~0.1 m toward the attacker and
     +0.08 m up).
   - Log both positions on the impact frame to confirm.
   - Find the real peak by scanning the clip: Combat_Spinning_Kick peaks at normalized ~0.40, not where you'd
     assume.
   - Move the victim's arms out of the way before impact (e.g. switch to a hip-aim pose), or the foot hits his
     hands, not his face.
   - Start the victim's hit reaction on the same frame, and move him with an ease-out (he flies ~4 m into the wall).
4. **Sampling details.**
   - Edit mode: `AnimationMode.StartAnimationMode` + `SampleAnimationClip` inside Begin/EndSampling. Sampling
     resets the root, so set rotation/position **after** sampling, then correct the hips XZ back onto the target
     position.
   - Play mode: use `clip.SampleAnimation(go, t)`. A `PlayableGraph` with an Animator output writes the bones a
     frame late, so bone reads (hips correction, contact) are wrong.
5. **Real destruction (NekoBuildingPhysics).** It only runs in Play mode, so drive the whole take from a temporary
   runtime MonoBehaviour:
   - Set `Time.captureFramerate = 24`.
   - Each frame in `LateUpdate`: pose the characters, move the camera, `cam.Render()` into a RenderTexture, and
     write a JPG.
   - At impact call `world.ApplyImpact(building, pointOnStorefront, velocity, mass≈250, radius≈1.6)`. Call
     `world.PrepareImpactTest(building)` in `Start` so it is ready.
   - `FireCannonball`/`Launch` also exist, but they add a visible projectile.
   - Start it by adding the component to a temp GameObject and setting `EditorApplication.isPlaying = true`. The
     script sets `isPlaying = false` when it finishes.
6. **Camera orbit.**
   - Use an ellipse around the action midpoint: narrow radius across the street (~3 m), wide radius along it
     (~5 m), clamp the camera inside the shop fronts, and rise ~1.8 m when passing over the street sides.
   - Choose the start angle so the **impact frame is seen side-on** (camera perpendicular to the strike). From
     behind, the contact is hidden.
   - After impact, let the look-at point drift toward the victim and the wreck.
   - 360° over 15 s at 24 fps.
7. **Slow-motion bullets:** elongated unlit yellow spheres (0.035 x 0.035 x 0.55) fired every 0.7 s from the
   gun hand at ~2.6 m/s, aimed to just miss.
8. **Keyframes from the take.** Sunburst-edit 4 frames of the mannequin render (two dodge poses, the impact frame,
   the wreck) with the cast sheet. These have the exact geometry.
   - Say who is where: "The enforcer lies crumpled inside the shop; NO one else stands in the street", or the
     model adds an extra standing character.
9. **Generate the single take.**
   - **Seedance R2V:** use the mannequin video as `<Video 1>`, the 4 keyframes as `<Image 1-4>`, and the hero and
     villain sheets as `<Image 5-6>`.
   - Prompt Seedance with: "ONE single continuous unbroken shot, one take, absolutely no cuts… follow <Video 1>
     camera orbit, timing and movements frame by frame. The white mannequin is the hero <Image 5>…", then the
     story beats, then sound and the no-music line.
   - Venice returns 413 if the payload is too big, so shrink the refs to ~1280 px JPGs and the reference video to
     960 px crf30 (~1.6 MB).
   - **Recast** on a 720p crf22 copy (~9 MB) keeps the Unity look. That's useful as the "real scene" version.

## Editing and end sequence

1. **Intro before the hero shot:** an establishing shot with the title, entrances (portal/teleport), close-ups
   and the draw. Then the single take. Fade the take's last second.
2. **Music.**
   - **Multi-shot story film:** one cue per section (setup / tension / ending, plus one for the end cards if
     needed). Loudnorm the bed to about -26 to -27 LUFS and dialogue to about -20, add fades, and place cues with
     `adelay`. Leaving a key dialogue moment without music can land harder.
   - **Music video:** the song is the timeline. Transcribe it for line timings, place each shot to its lyric line,
     and use Japanese plus English lyric subtitles.
   - **Action / single take:** follow the rest of this step.
   - Generate 2 variants and measure their loudness envelope (RMS per second with numpy) to find the drops and the
     ending.
   - Pick the variant and offset whose drop lands inside the action and whose final hit lands just before the
     logo card.
   - Levels: clip SFX loudnorm about -19 to -22 LUFS. Music bed at volume ~0.6 under the action, ~0.85 on the end
     cards. `amix normalize=0` + `alimiter`. Measure each section's LUFS for clip vs. music before the final mix.
3. **Normalize each clip:** scale/crop to 1920x1080, 24 fps, trim, and fade where scenes change time ("翌朝 /
   The next morning", "数週間後 / Weeks later" cards).
4. **Subtitles** (if there's dialogue) are an ASS file burned in with `ass=subs.ass:fontsdir=assets`:
   - Use Zen Maru Gothic for Japanese and Poppins for English.
   - Check each font's real family name with `fc-scan`; a wrong name silently falls back.
   - Poppins has no "→" glyph.
5. **End sequence**, rendered frame by frame with PIL and piped to ffmpeg:
   - a "Made with AI" card that names the Unity asset as the set and layout, explains what Unity did (layout,
     choreography, destruction physics), and says AI content isn't included in the asset
   - a **before/after slider**: the Unity render vs. the AI frame from the *same* camera and pose (the mannequin
     frame vs. its Sunburst keyframe is ideal), 3 s each over 4 moments
   - a logo card listing what the asset ships with
6. Concatenate with the concat demuxer, then mux the mix.
7. **Verify** by grabbing frames at the title, subtitles, the impact, mid-slider (not slider boundaries, which are
   black fades) and the logo.
8. Send a crf24 copy if the master is over ~35 MB.

## Continuity rules learned the hard way

- **Screen direction.** Decide where everyone is (who comes from screen left or right) and make every keyframe obey
  it. A cat once "ran the wrong way" because the leap keyframe pointed away from the boy.
- **Sides of the body.** Say "her LEFT front paw (the paw on the RIGHT side of the image from the viewer's point of
  view)". The model flips left and right otherwise.
- **Characters must be set up before they appear.** Show the cat asleep on the vending machine in the wide shot
  before cutting to her waking up there.
- **Unprompted props and characters.** A cat appeared inside a shop where cats weren't allowed, and an extra
  standing enforcer appeared in an aftermath frame. Add "there is NO cat / NO one else in this shot" when it
  matters.
- **Story gaps.** If a character walks away and then is suddenly held, add the missing beat (a witness explains,
  the next morning) instead of cutting around it.
- Keep text on screen readable and generic: no brand logos, no years (the calendar first showed "2024").

## Practical pitfalls

- **Unity MCP `execute_code` is CodeDom, C# 6:**
  - no local functions (use `System.Func` lambdas)
  - no reusing a variable name that a lambda also uses
  - a method group can't be used as a value (`WorldBounds` is a property here)
- **Calling a temp editor script:** put it in `Assets/_BTTemp/Editor/`, copy the source from a `.cs.txt` kept
  outside Assets, call `refresh_unity` (compile), and find the type via reflection over the loaded assemblies.
  - `public static` fields of a private nested type won't compile.
  - After a refresh, the old type may still be loaded until the domain reload finishes; check for the new method
    before calling it.
- Unity MCP blocks `AssetDatabase.DeleteAsset`. To clean up, move temp folders (and their `.meta`) out of Assets
  with `System.IO`, then `AssetDatabase.Refresh()`.
- Keep temp objects (`__BT_*`) out of saved scenes. Delete them before the human saves.
- Linked-device shell calls time out at about 180 s, and background processes die when the call ends:
  - batch 4–10 image jobs per call
  - poll long video jobs with `sleep 170` calls
  - split long encodes into two parts. For the second part, offset the subtitles with
    `setpts=PTS+O/TB,ass=...,setpts=PTS-O/TB`, then concatenate with `-c copy`.
- A poll loop over old queue entries can run past the timeout. Check whether the output file exists first.
- Deleting is blocked in connected folders, so move old files to an `old/` subfolder.
- `device_commit_files` rejected files of ~39 MB or more; send compressed audio (mp3) instead.
- Keep work folders outside the shipped asset package (e.g. `_StoreExcluded/`), or under a `~` folder, which Unity
  ignores.
- Never put the VML control token or address in a repo or a public skill.
- GitHub releases for VML: build a `.bat` the human runs; don't publish from the session.
