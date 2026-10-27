#!/usr/bin/env python3
"""vml.py - drive a running Venice Media Local app from an agent (stdlib only, Python 3.8+).

Results appear live in the open app window and are saved in its output folder, exactly as if the human clicked.
This helper also saves every output next to you (--out) and prints a short JSON summary. It never prints image data
or the credential, because the app's responses carry the full base64 image and even echo the input images back.

Connection (first match wins):
  VML_URL / VML_TOKEN environment variables
  ~/.config/venice-media-local/agent.json   {"url": "http://100.64.x.x:9876", "token": "..."}   (keep it out of git)

Commands:
  vml.py state                                   is the app up? version, key, output folder, selected models
  vml.py models [image|edit|video|music|sfx|voice|transcribe]   model ids the app lists
  vml.py generate "prompt" [--model M] [--variants N] [--aspect 16:9] [--resolution R] [--seed S] [--out DIR]
  vml.py edit "prompt" --image a.png [--image b.png ...] [--model M] [--aspect A] [--out DIR]
  vml.py compare "prompt" --image a.png --models m1,m2[,...] [--out DIR]   same edit through several models at once, timed
  vml.py sfx "prompt" [--model M] [--seconds S] [--out DIR] [--stem NAME]          sound effect (queued, polled, saved)
  vml.py music "prompt" [--model M] [--seconds S] [--instrumental] [--out DIR] [--stem NAME]   music track (same flow)
  vml.py speech "text" [--model M] [--voice V] [--style "how to say it"] [--speed X] [--out DIR] [--stem NAME]   text to speech

Defaults: see DEFAULT_MODELS below. Change them there when better models arrive.
"""
import argparse, base64, json, mimetypes, os, sys, time, threading, urllib.request, urllib.error

# The house picks (2026-10). Swap these when a better model ships; --model always overrides.
DEFAULT_MODELS = {
    "image": "gpt-image-2-5-sunburst",    # text -> image (the user's pick, 2026-10)
    "edit": "gpt-image-2-5-flare-edit",   # image(s) + instruction -> image (won a side-by-side vs nano-banana-2-1-edit)
    "sfx": "elevenlabs-sound-effects-v2",  # text -> sound effect
    "music": "elevenlabs-music-v2-5",      # text -> music
    "voice": "tts-gemini-3-1-flash",       # text -> speech (takes a style prompt)
}


def config():
    url, token = os.environ.get("VML_URL"), os.environ.get("VML_TOKEN")
    path = os.path.expanduser("~/.config/venice-media-local/agent.json")
    if (not url or not token) and os.path.exists(path):
        with open(path) as f:
            c = json.load(f)
        url, token = url or c.get("url"), token or c.get("token")
    if not url or not token:
        sys.exit("vml: set VML_URL and VML_TOKEN (or ~/.config/venice-media-local/agent.json). Ask the human for them.")
    return url.rstrip("/"), token


def call(method, route, body=None, timeout=300):
    url, token = config()
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url + "/api/v1" + route, data=data, method=method,
                                 headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
    except urllib.error.HTTPError as e:
        msg = e.read()[:500].decode(errors="replace")
        hint = {401: "token rejected", 404: "no such route (everything lives under /api/v1)"}.get(e.code, "")
        raise SystemExit(f"vml: HTTP {e.code} {hint} {msg}".strip())
    except urllib.error.URLError as e:
        raise SystemExit(f"vml: cannot reach {url} ({e.reason}). Is the app open with Settings > AI Agent Control on?")
    return json.loads(raw) if raw else None


def data_url(path):
    mime = mimetypes.guess_type(path)[0] or "image/png"
    with open(path, "rb") as f:
        return f"data:{mime};base64," + base64.b64encode(f.read()).decode()


def save(items, out, stem):
    """write each result's dataUrl to out/stem_i.ext; return compact summaries (no image data, no echoed request)"""
    os.makedirs(out, exist_ok=True)
    rows = []
    for i, it in enumerate(items if isinstance(items, list) else [items]):
        du, local = it.get("dataUrl") or "", None
        if du.startswith("data:"):
            ext = {"jpeg": "jpg", "mpeg": "mp3", "x-wav": "wav", "wave": "wav"}.get(du.split(";")[0].split("/")[-1], du.split(";")[0].split("/")[-1])
            local = os.path.join(out, f"{stem}_{i}.{ext}")
            with open(local, "wb") as f:
                f.write(base64.b64decode(du.split(",", 1)[1]))
        rows.append({"saved": local, "appFile": it.get("filePath"), "model": it.get("model") or (it.get("metadata") or {}).get("model")})
    return rows


def cmd_state(a):
    s = call("GET", "/state")
    m, st = s.get("models", {}), s.get("settings", {})
    print(json.dumps({
        "version": s.get("buildVersion"), "address": s.get("agentControlAddress"), "veniceKeyConfigured": s.get("keyConfigured"),
        "outputDir": st.get("outputDir"), "selectedModels": st.get("selectedModels"), "modelsFetched": m.get("lastFetched"),
        "modelCounts": {k: len(v) for k, v in m.items() if isinstance(v, list)},
    }, indent=1))


def cmd_models(a):
    m = call("GET", "/state").get("models", {})
    kinds = [a.kind] if a.kind else ["image", "edit", "video", "music", "sfx", "voice", "transcribe"]
    print(json.dumps({k: [x.get("id") for x in m.get(k + "Models", [])] for k in kinds}, indent=1))


def cmd_generate(a):
    body = {"model": a.model or DEFAULT_MODELS["image"], "prompt": a.prompt, "variants": a.variants, "navigate": True}
    for k, v in (("aspectRatio", a.aspect), ("resolution", a.resolution), ("seed", a.seed)):
        if v is not None:
            body[k] = v
    t0 = time.time()
    r = call("POST", "/generate-image", body)
    print(json.dumps({"seconds": round(time.time() - t0, 1), "results": save(r, a.out, "gen")}, indent=1))


def edit_once(model, prompt, images, aspect, out, stem):
    body = {"model": model, "prompt": prompt, "images": images, "navigate": True}
    if aspect:
        body["aspectRatio"] = aspect
    t0 = time.time()
    r = call("POST", "/edit-image", body)
    return {"model": model, "seconds": round(time.time() - t0, 1), "results": save(r, out, stem)}


def cmd_edit(a):
    imgs = [data_url(p) for p in a.image]
    print(json.dumps(edit_once(a.model or DEFAULT_MODELS["edit"], a.prompt, imgs, a.aspect, a.out, "edit"), indent=1))


def cmd_compare(a):
    imgs, out, rows = [data_url(p) for p in a.image], {}, []
    def run(m):
        try:
            out[m] = edit_once(m, a.prompt, imgs, a.aspect, a.out, m)
        except SystemExit as e:
            out[m] = {"model": m, "error": str(e)}
    ts = [threading.Thread(target=run, args=(m,)) for m in a.models.split(",")]
    [t.start() for t in ts]; [t.join() for t in ts]
    print(json.dumps([out[m] for m in a.models.split(",")], indent=1))


def cmd_audio(a):
    """queue-music / queue-sfx, then poll retrieve-audio until the result arrives; save it"""
    kind = a.cmd
    model = a.model or DEFAULT_MODELS[kind]
    body = {"model": model, "prompt": a.prompt}
    if a.seconds:
        body["durationSeconds"] = str(a.seconds)
    if kind == "music" and a.instrumental:
        body["forceInstrumental"] = True
    t0 = time.time()
    q = call("POST", f"/queue-{kind}", body)
    qid = q.get("queueId")
    if not qid:
        sys.exit(f"vml: no queueId in the queue response (status {q.get('status')!r})")
    deadline = t0 + a.timeout
    while True:
        r = call("POST", "/retrieve-audio", {"queueId": qid, "model": model, "kind": kind, "downloadUrl": q.get("downloadUrl") or None})
        res = r.get("result")
        if res:
            rows = save(res, a.out, a.stem or kind)
            print(json.dumps({"kind": kind, "model": model, "seconds": round(time.time() - t0, 1), "results": rows}, indent=1))
            return
        if r.get("status", "").lower() in ("failed", "error", "cancelled"):
            sys.exit(f"vml: {kind} failed: {r.get('progressLabel') or r.get('status')}")
        if time.time() > deadline:
            sys.exit(f"vml: timed out waiting for {kind} {qid} (last status {r.get('status')!r})")
        time.sleep(3)


def cmd_speech(a):
    body = {"model": a.model or DEFAULT_MODELS["voice"], "input": a.text, "responseFormat": "mp3"}
    if a.voice: body["voice"] = a.voice
    if a.style: body["stylePrompt"] = a.style
    if a.speed: body["speed"] = a.speed
    t0 = time.time()
    r = call("POST", "/generate-speech", body)
    rows = save(r, a.out, a.stem or "speech")
    print(json.dumps({"kind": "speech", "model": body["model"], "seconds": round(time.time() - t0, 1), "results": rows}, indent=1))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("state").set_defaults(fn=cmd_state)
    s = sub.add_parser("models"); s.add_argument("kind", nargs="?"); s.set_defaults(fn=cmd_models)
    s = sub.add_parser("generate"); s.add_argument("prompt"); s.add_argument("--model"); s.add_argument("--variants", type=int, default=1)
    s.add_argument("--aspect"); s.add_argument("--resolution"); s.add_argument("--seed", type=int); s.add_argument("--out", default="vml-out"); s.set_defaults(fn=cmd_generate)
    for name, fn in (("edit", cmd_edit), ("compare", cmd_compare)):
        s = sub.add_parser(name); s.add_argument("prompt"); s.add_argument("--image", action="append", required=True)
        s.add_argument("--aspect"); s.add_argument("--out", default="vml-out")
        s.add_argument("--models", required=True) if name == "compare" else s.add_argument("--model")
        s.set_defaults(fn=fn)
    for name in ("sfx", "music"):
        s = sub.add_parser(name); s.add_argument("prompt"); s.add_argument("--model"); s.add_argument("--seconds", type=float)
        s.add_argument("--out", default="vml-out"); s.add_argument("--stem"); s.add_argument("--timeout", type=float, default=600)
        if name == "music": s.add_argument("--instrumental", action="store_true")
        s.set_defaults(fn=cmd_audio)
    s = sub.add_parser("speech"); s.add_argument("text"); s.add_argument("--model"); s.add_argument("--voice"); s.add_argument("--style")
    s.add_argument("--speed", type=float); s.add_argument("--out", default="vml-out"); s.add_argument("--stem"); s.set_defaults(fn=cmd_speech)
    a = p.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
