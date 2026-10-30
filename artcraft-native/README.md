# ArtCraft Native

ArtCraft's desktop app in pure Rust: an [egui](https://github.com/emilk/egui) UI over the same
ArtCraft API client crates the Tauri app uses. There is no Tauri and no JavaScript; the only
webview is the Midjourney sign-in window, which exists to capture Midjourney's login cookies.

The look follows the brutalist webapp (`artcraft-services/frontend/apps/artcraft-webapp`). That
means near-black surfaces with white/15 hairlines and 3 px corners on controls. Type is Archivo for
display, Inter for body text and Geist Mono for labels. The brand blue marks the primary actions.

Create Image, Create Video and Create Audio are fully built. The other tools show a "coming soon"
page that links to the same tool in the web app.

## Running

```bash
cargo run -p artcraft_native --release
```

The workspace's default member is the Tauri app, so pass `-p artcraft_native`.

- **Sign-in.** On first launch the app reuses an existing ArtCraft login. It reads the session
  cookie from the Tauri app's jar (`<cache dir>/ai.artcraft.app/.cookies`) and keeps its own copy in
  `~/Artcraft/credentials/native_session.json`. Otherwise use "Sign in with browser" (device code)
  or a username and password.
- **API host.** `~/Artcraft/settings/env_configs.json` selects it, exactly as for the Tauri app.

## Architecture

| Module             | What it does                                                                   |
|--------------------|--------------------------------------------------------------------------------|
| `backend`          | Tokio runtime and every API call; results come back to the UI as `Event`s      |
| `backend::wire`    | Lenient response shapes; new server enum values can't break decoding           |
| `backend::session` | Credentials: own file, Tauri cookie import, env config                         |
| `backend::media_cache` | Thumbnails and previews: downloaded and decoded off-thread, evicted when unused |
| `backend::audio`   | One clip at a time: reference previews, feed tracks (pause, seek, volume), waveforms |
| `backend::providers` | Midjourney and fal: accounts, submitting, and a worker that finishes jobs into the library |
| `mcp`              | `--mcp`: headless MCP server on stdio (image and video generation tools)       |
| `models`           | Model catalog from OmniGen listings, plus names, families and limits           |
| `prompt_box`       | The shared prompt box: editor, reference deck, keyframes, pickers, model selector |
| `pages`            | Create Image, Video and Audio, Home, Library, and the shared page shell        |
| `feed`             | Generation feed: running, failed and finished jobs, library paging, grid and list |
| `overlays`         | Lightbox, library picker, sign-in, settings, provider setup and the Midjourney sign-in window |
| `shell`            | Custom title bar, sidebar flush to the window edge, content panel header       |
| `theme`, `ui`      | Tokens, Lucide-style icons, widgets, toasts, window chrome (drag, resize, caption buttons) |

How the backend works:

- **Generation.** It goes through OmniGen: `/v1/omni_gen/{models,cost,generate}/{image,video,audio}`.
  Model capabilities come from the server, so new models and options appear without an app update.
- **Typed client functions.** Model lists, uploads, credits, login and deletes use
  `artcraft_client`.
- **Lenient decoding.** Generate and cost requests are JSON maps, as in the Tauri app. Job polling
  (`/v1/jobs/session`), the audio model list, the library list, batches and prompts are decoded
  with lenient structs.
- **Identity headers.** Every request sends the desktop identity headers: `Origin` and
  `User-Agent`.
- **Other providers.** Images can also run on Midjourney (your own account) or fal (your own API
  key), through `artcraft_router` like the Tauri app. The accounts live in the same files
  (`~/Artcraft/credentials`), so connecting in either app works in both. Those jobs are tracked in
  `~/Artcraft/state/native_provider_jobs.json` and their results uploaded into your library, resuming
  after a restart.

## Prompt box parity

| Feature                                                         | Status                                  |
|-----------------------------------------------------------------|-----------------------------------------|
| Auto-growing prompt (~4 lines), expand/collapse, resize grip    | Done                                    |
| Focus mode (fullscreen editor with deck and toolbar)            | Done                                    |
| Length counter, red over the model's limit                      | Done                                    |
| Enter to generate (setting; Shift+Enter for a new line)         | Done                                    |
| `@Image1` / `@Video1` / `@Audio1` highlight and autocomplete    | Done                                    |
| Reference deck: fan, +N badge, hover-expand, reorder, clear all | Done                                    |
| Keyframe slots: first/last frame, tilt, swap                    | Done                                    |
| Upload, pick from library, drag & drop, paste image             | Done (video uploads must be MP4)        |
| Model selector with families, logos, capability badges          | Done                                    |
| Aspect ratio, resolution, quality, count                        | Done                                    |
| Bitrate, output format, duration slider, sound, input mode      | Done                                    |
| Live credit cost on the generate button                         | Done                                    |
| Validation: starting frame, text-only banner, limits, uploads   | Done                                    |
| Clear all (confirms when references are attached)               | Done                                    |
| `@Character` mentions and the Characters modal (create/edit/delete) | Done                                |
| Provider choice (Midjourney direct, fal), "Select Provider"     | Done (images; video runs on ArtCraft)   |
| In-app video playback                                           | Done with FFmpeg on the PATH; otherwise opens in the system player |
| Audio reference playback (click to play/stop)                   | Done                                    |
| Animated video previews in the feed (with a stills toggle)      | Done                                    |
| Audio: style, instrumental, keep lyrics, loop                   | Done                                    |
| Audio: Beat & Key (BPM, musical key) and Tuning (sample rate, speed, volume, pitch) | Done                |
| Audio references (play, 600 s cap) or one image, never both     | Done                                    |

Feed and lightbox:

- **Feed.** Pending cards show progress, time left and a batch banner. Failed cards show the reason
  and can be dismissed. The grid is masonry, with a list view alongside. Hover actions are Recreate,
  Make Video, Share and Download. Select mode does batch downloads. The last viewed item is marked.
- **Audio.** Audio cards play in place: a waveform (click to seek), the time and a volume flyout.
  One track plays at a time and stops when you leave the page. Audio offers Share and Download.
- **Lightbox.** Prev/next with the arrow keys, prompt copy and details. Actions are Recreate, Make
  Video, Download, Share and Delete. Audio plays in a panel with the full-size player.

## MCP server

`artcraft-native --mcp` runs headless as a [Model Context Protocol](https://modelcontextprotocol.io)
server on stdio, so agents can generate with the user's ArtCraft account. No window opens. It uses
the session the app saved (see Sign-in), read on every call, so signing in to the app takes effect
without restarting the server.

The tool set depends on the scope: `--scope photo|film|all`, or by default the executable's name
(`photocraft` → images, `filmcraft` → video, anything else → both).

| Tool                | Scope       | What it does                                                         |
|---------------------|-------------|----------------------------------------------------------------------|
| `account`           | all         | Who is signed in, and their credits                                  |
| `list_image_models` | photo       | Image models and the options each takes                              |
| `generate_image`    | photo       | Prompt, model options, reference images (paths or `mf_…` tokens)     |
| `list_video_models` | film        | Video models, durations, keyframe and reference support              |
| `generate_video`    | film        | Prompt, keyframes or references, duration, sound                     |
| `get_job`           | all         | Job status and results; can wait and download                        |
| `split_thumbnail`   | photo       | 50/50 split-screen still from two images or video frames (FFmpeg)    |
| `assemble_sequence` | film        | Renders clips into one sequence with ambient, SFX and crossfades     |

`assemble_sequence` and `split_thumbnail` run locally with FFmpeg and spend no credits. The
sequence puts clips in order on V1, each clip's own audio on A1 (default −12 dB) and an optional
SFX file under each clip on A2 (default −3 dB), with crossfades between clips (default 0.5 s), at
1080p24 by default. It writes the `.mp4` and a `.timeline.json` with every clip's timecodes.
FFmpeg is found through `ARTCRAFT_FFMPEG_DIR`, the PATH, `/opt/homebrew/bin`, `/usr/local/bin` or
`~/.local/bin`; MCP clients started from the Dock pass a minimal PATH, so those folders matter.

Generate calls validate options against the model, then wait up to `wait_seconds` (default 120)
for results and return their CDN links, downloading them into `save_dir` when given.
`estimate_only` prices a request without spending credits. Requests are built the same way as on
the Create Image and Create Video pages.

`script/artcraft/install_mcp_apps.sh` (macOS) builds the binary and installs it as
`/Applications/PhotoCraft.app` and `/Applications/FilmCraft.app`. That matches this client config:

```json
{
  "mcpServers": {
    "artcraft-photo": { "command": "/Applications/PhotoCraft.app/Contents/MacOS/photocraft", "args": ["--mcp"] },
    "artcraft-film": { "command": "/Applications/FilmCraft.app/Contents/MacOS/filmcraft", "args": ["--mcp"] }
  }
}
```

## Diagnostics

Set `ARTCRAFT_SCREENSHOT=<file.png>` to save a screenshot once the window has settled.
`ARTCRAFT_PAGE` (`home`, `create-image`, `create-video`, `create-audio` or `library`) picks the page,
`ARTCRAFT_OPEN` (`settings` or `midjourney-login`) opens a dialog, and `ARTCRAFT_SCREENSHOT_EXIT=1`
closes the app afterwards. The screenshot shows egui only, not the sign-in webview.

## Tests

```bash
cargo test -p artcraft_native
```

There are unit tests for the catalog, feed store, wire decoding and backend helpers. Headless UI
tests (`egui_kittest`) drive the prompt box (generate gating, Enter to generate, the pickers, `@`
mentions, clear-all), the Characters dialog and the audio toolbar (toggles, Beat & Key, Tuning).

## Fonts and brand assets

`assets/fonts` holds Inter, Archivo and Geist Mono, all under the SIL Open Font License; the
licences sit next to the files. The wordmark, app icon and creator logos are the Tauri app's own
(`frontend/apps/artcraft/app/public/resources`, `crates/desktop/artcraft/icons`); see `NOTICE`
for their terms.
