# Venice Media Local

Local Tauri desktop app for Venice-powered media generation.

This app is intended to be shared with Venice community users as a local desktop tool. The UI is React/Vite, and the native shell/API layer is Tauri 2 with Rust.

## For Humans

Venice Media Local is a desktop media generator for people who want one local place to use Venice's media API.

You build the app locally, paste in your own Venice API key, and generate media from your machine. The key stays on your computer in the operating system credential store. There is no shared server operated by this project.

Security note:

This repo does not provide a prebuilt `.exe` by default. That is intentional. For a local API-key app, the safer path is to build from source on your own machine instead of installing random binaries from the internet. If you are not a developer, ask your AI coding agent to install the prerequisites, inspect the repo, and build it locally for you.

Plain-English build path:

1. Install Node.js 20+ and Rust.
2. Clone this repo.
3. Run `npm install`.
4. Run `.\Build-Windows.ps1`.
5. Open the locally built app at `src-tauri\target\release\venice-media-local.exe`, or use the locally built installer in `src-tauri\target\release\bundle\nsis\`.
6. Paste your Venice API key when the app asks for it.
7. Choose a media type from the left side, refresh models if needed, and generate media.

The goal is simple:

- Image, video, music, sound effects, and voice in one app.
- No chat UI and no text-agent clutter.
- Results save to local files automatically. Images default to WebP to keep files smaller.
- Clear only removes result cards from the app. Trash moves generated files into the output folder's `burn` subfolder.
- The burn button corrupts and deletes files from the burn folder, bypassing the Recycle Bin. Successfully overwritten files should be unreadable if recovered.
- Models can be refreshed from Venice and managed locally.
- The app can expose Venice API features in one place, even when different Venice clients expose those features differently.
- The interface can be tuned for creators who generate a lot of media, instead of behaving like a general chat or social feed.

Why this can be nicer than using a hosted web UI:

- **Your API key stays local.** The app calls Venice directly from the local Tauri backend.
- **Your outputs are local files.** Generated media is saved on disk instead of being trapped in a browser session.
- **No browser storage quota wall.** Browser apps can hit local storage, IndexedDB, or cache limits when lots of media piles up. This app writes outputs to normal folders on disk, so practical storage is limited by the drive you choose.
- **Cleaner local cleanup.** Result cards can be cleared without touching files, moved to the local burn folder with trash buttons, or burned later when the user wants to corrupt and delete the files.
- **One media workspace.** Image, video, music, SFX, and voice are grouped by media type in the left rail.
- **Less browser friction.** No browser tabs, browser profile issues, extension conflicts, or accidental refreshes.
- **Better long-session potential.** Hosted web UIs can get laggy when a lot of images/results pile up. This app already saves outputs locally, and the UI can be optimized around local history, paging, folders, and cleanup.
- **Model control.** Use **Get Latest From Venice** to refresh models, then add/remove models locally for your own workflow.
- **Community modifiable.** Because this is an open local app, the community can add presets, batch tools, naming rules, galleries, export packs, prompt templates, or workflow shortcuts without waiting for the official UI.
- **Themeable.** The included themes give the app a few polished dark and light looks.
- **Locally buildable.** It can produce a normal Windows setup executable from source, with macOS/Linux builds possible through Tauri.

This is not meant to replace Venice. It is a local power-user workbench for Venice media generation.

## For AI Agents

If you are an AI coding agent helping a human with this repo:

1. Do not commit API keys, generated media, build output, or local app data.
2. Use the helper scripts on Windows because this machine may have a broken user-level `npm` shim.
3. Use the committed lockfiles: `package-lock.json` and `src-tauri/Cargo.lock`.
4. Build with Tauri before handing off an installer.
5. Tell the human where the installer was produced.
6. For release builds, commit first, then run the Windows build script so the generated version includes the release commit hash.
7. Preserve the output cleanup behavior: clear only clears UI results, trash moves files to `<output folder>\burn`, and burn corrupts/deletes files from that burn folder.

Agent install/build handoff:

1. Do not tell users to download a random `.exe`; this repo is source-first for security.
2. If building locally on Windows, run `.\Build-Windows.ps1` from the repo root.
3. After a successful build, the installer is in `src-tauri\target\release\bundle\nsis\`.
4. The direct executable is in `src-tauri\target\release\`.
5. Do not commit `dist/`, `src-tauri/target/`, `node_modules/`, `.env*`, or generated media.

The Venice API key is stored through the OS credential store at runtime. It is not written into the repo. `VENICE_API_KEY` is supported as a developer fallback, but `.env*` files are ignored and must not be committed.

Credential store service/account:

```text
venice-media-local / venice-api-key
```

## Prerequisites

- Node.js 20+.
- Rust/Cargo stable.
- Windows WebView2 runtime for Windows users.
- Network access for first dependency install and first Tauri bundler download.

On this Windows workstation, known-good paths are:

```text
C:\Program Files\nodejs\npm.cmd
C:\Users\flash\.cargo\bin\cargo.exe
```

## Install Dependencies

Fresh clone:

```powershell
npm install
```

If `npm` resolves to a broken user shim on Windows, use:

```powershell
& "C:\Program Files\nodejs\npm.cmd" install
```

## Run In Dev

```powershell
.\Start-Dev.ps1
```

Generic command:

```powershell
npm run tauri -- dev
```

## Build Windows Installer

```powershell
.\Build-Windows.ps1
```

Generic command:

```powershell
npm run version:build
npm run tauri -- build --config src-tauri/tauri.version.conf.json
```

The helper script generates a temporary Tauri config at:

```text
src-tauri\tauri.version.conf.json
```

That file is ignored by git. It gives the installer a build-time version shaped like:

```text
2026.5.9221530+gabcdef12
```

Meaning:

- `2026` = year
- `5` = month
- `9221530` = day + time, here day 9 at 22:15:30
- `gabcdef12` = git commit hash

If the project has not been committed in its own repo yet, the hash part becomes `gnogit`. Commit first for real public release installers.

Keeping the app identifier stable and increasing this version on each build lets the Windows setup executable install over/upgrade an existing install instead of looking like the same old build.

Note: Windows file metadata requires numeric version pieces, so Windows may strip the commit hash from the low-level `VIProductVersion`. The setup filename and generated Tauri version still include the hash metadata.

Current bundle output:

```text
src-tauri\target\release\bundle\nsis\Venice Media Local_<build-version>_x64-setup.exe
```

The direct executable is also produced at:

```text
src-tauri\target\release\venice-media-local.exe
```

## Build Verification

Run these before handing off:

```powershell
npm run build
cd src-tauri
cargo check
cd ..
npm run version:build
npm run tauri -- build --config src-tauri/tauri.version.conf.json
```

On this workstation, use the helper scripts or set PATH first:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
& "C:\Program Files\nodejs\npm.cmd" run tauri -- build
```

## Public Repo Hygiene

Expected source files to commit include:

- `src/`
- `src-tauri/src/`
- `src-tauri/capabilities/`
- `src-tauri/icons/icon.ico`
- `package.json`
- `package-lock.json`
- `src-tauri/Cargo.toml`
- `src-tauri/Cargo.lock`
- `src-tauri/tauri.conf.json`
- helper scripts and docs

Ignored/generated files include:

- `node_modules/`
- `dist/`
- `src-tauri/target/`
- `src-tauri/gen/`
- `.env*`
- `outputs/`, `generated/`, `media/`
- logs and temp files

## Current media surface

- Image generation saves local image files and displays result cards.
- Video, music, and SFX queue jobs through Venice and include polling hooks.
- Voice generation calls Venice speech and saves local audio files.
- Output cleanup supports clearing UI cards, moving generated files to the local `burn` folder, and burning that folder.
- Model refresh calls Venice model catalog endpoints and caches normalized model lists locally.
- Model manager supports local add/remove overrides.
- Includes multiple dark and light color themes.
