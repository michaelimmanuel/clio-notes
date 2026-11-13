# Clio Notes

Clio Notes is a desktop app that writes your meeting notes while the meeting happens. It records
your microphone, your computer's sound and your screen, and turns them into readable notes with
screenshots of the slides and demos that were shown.

It runs on your own computer and joins no call: no bot is added to the meeting. It works with any
meeting app (Zoom, Teams, Meet, a webinar, a recorded video). You bring your own AI provider
keys, so the recordings and notes stay on your machine and are sent only to the providers you
choose.

## What you get

- **Live notes.** While the meeting runs, the transcript appears line by line and the notes fill
  in section by section, each with the screenshot that goes with it.
- **Final notes.** When you stop, one more pass merges the sections into a clean document with a
  title, a summary, decisions and action items (with the owner, when someone said who).
- **Plain files.** Every meeting is a folder with `notes.md`, the screenshots it uses in
  `images/`, and the full `transcript.md`. They open in any Markdown viewer and can be pasted
  into Notion, Obsidian, Confluence or an email.
- **Screens read, not just pictured.** Slide titles, text, tables, code and terms are read from
  the screen and used to fix the transcript, so a product name spelled on a slide is spelled
  the same way in the notes.
- **Who said it.** Lines are marked "Me" (your microphone) or "Others" (everyone you hear).
- **Usage per meeting.** Each meeting shows the calls, tokens and audio minutes it used, per job.

## Download

Installers are on the [Releases](https://github.com/michaelimmanuel/clio-notes/releases) page.

| System | File | Needs |
|---|---|---|
| Windows | `Clio.Notes_<version>_x64-setup.exe` | Windows 10 or 11 |
| macOS, Apple silicon | `Clio.Notes_<version>_aarch64.dmg` | macOS 14.6 or later |
| macOS, Intel | `Clio.Notes_<version>_x64.dmg` | macOS 14.6 or later |
| Linux (Ubuntu, Debian) | `Clio.Notes_<version>_amd64.deb` | Ubuntu 24.04 or similar, PulseAudio or PipeWire |
| Linux (other) | `Clio.Notes_<version>_amd64.AppImage` | as above |

**Windows:** the installer is per user and needs no administrator rights. It installs the WebView2
runtime if it is missing.

**macOS:** the app is not signed with an Apple Developer certificate yet, so the first time you
open it macOS refuses. Open System Settings, then Privacy & Security, and click "Open Anyway".
On the first recording, allow the microphone, system audio and screen recording.

**Linux:** the computer's sound is recorded from the speaker's monitor source, which needs
PulseAudio or PipeWire (most current desktops have one). Screen capture may be limited on
Wayland desktops.

## Getting started

1. Open Clio Notes. The first time, a short setup asks for a provider and an API key, and picks
   a model for each job.
2. Optionally open **Settings > Recording** to choose the microphone, the display to capture,
   and whether to take screenshots of slides only or of demos too.
3. On the **Meeting** tab, press **Start**. The app reminds you to tell everyone you are
   recording (you can turn the reminder off), then starts.
4. Press **Stop** when the meeting ends. The final notes are written in a few seconds and appear
   under **Meeting notes**, where you can also open the folder, rewrite the notes or delete a
   meeting.

> Recording a conversation usually needs the consent of everyone in it. Ask before you record.

## AI providers

Clio Notes has three jobs, and each can use a different provider and model:

| Job | What it does | Providers |
|---|---|---|
| Speech to text | Turns the audio into a transcript | OpenAI, OpenRouter, Groq, a self-hosted Whisper server, or any OpenAI-compatible transcription endpoint |
| Screen reading | Reads each new slide or screen | Any model that accepts images: Anthropic, OpenAI, OpenRouter, Gemini, Ollama and other OpenAI-compatible endpoints |
| Notes writing | Writes the live sections and the final notes | Anthropic, OpenAI, OpenRouter, Gemini, Ollama and other OpenAI-compatible endpoints |

One OpenRouter key can cover all three jobs. **Settings > Jobs** has a "Test connection" button
for each provider and lists its models.

API keys are kept in your system's key store (Windows Credential Manager, the macOS Keychain or
the Linux Secret Service). They are never written to settings files, meeting folders or logs.

## Reliability

- If a provider connection drops, recording continues. Calls are retried for up to 20 minutes,
  and the notes catch up when the connection is back. A banner shows while a connection is down.
- If the app or the computer crashes mid-meeting, the recording is kept. The next time the app
  opens, the meeting shows as interrupted and its notes can still be written.
- While recording, a red REC badge with the time is shown on every tab, and the window cannot
  be closed until the meeting is stopped.

## Where files are kept

| | Windows | macOS | Linux |
|---|---|---|---|
| Meetings | `%APPDATA%\com.clionotes.app\meetings` | `~/Library/Application Support/Clio Notes/meetings` | `~/.local/share/com.clionotes.app/meetings` |
| Settings | `%APPDATA%\com.clionotes.app` | `~/Library/Application Support/Clio Notes` | `~/.config/com.clionotes.app` |
| Logs | next to `meetings`, in `logs/` | same | same |

Logs hold counts, timings, device names, model names and errors only. Keys and meeting content
are filtered out. Settings has a link to the log folder.

Each meeting folder holds:

```
meeting-<date and time>/
  notes.md          final notes
  notes-live.md     the live notes as they were written
  transcript.md     the full transcript, with screen changes marked
  images/           the screenshots used in notes.md
  keyframes/        every screen change captured
  mic.wav           your microphone
  system.wav        the computer's sound
  timeline.sqlite   everything above, with timings, for rewriting the notes
```

## Limits

- English only for now.
- Remote speakers are not told apart: everyone you hear is "Others".
- It needs an internet connection to your AI providers. Fully offline models are not supported
  yet, though a local Ollama or Whisper server works through the OpenAI-compatible option.

## Building from source

You need [Rust](https://rustup.rs) (stable) and [Node.js](https://nodejs.org) 24 or later. On
Linux, also install the libraries listed under `LINUX_PACKAGES` in
[.github/workflows/build.yml](.github/workflows/build.yml).

Run the app in development mode:

```bash
cd app
npm install
npm run tauri dev
```

Build an installer for the system you are on:

```bash
cd app
npm run tauri build
```

The installer lands in `target/release/bundle/`. Each system's installer must be built on that
system; the GitHub Actions workflow builds all of them.

Run the checks:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Project layout

```
app/               Tauri 2 shell (src-tauri/) and the web UI (src/, vanilla TypeScript and Vite)
crates/capture/    microphone, system audio and screen capture on one shared clock
crates/pipeline/   voice detection, transcription, screen change detection, notes and final pass
crates/providers/  provider adapters (Anthropic, OpenAI-compatible, OpenRouter) and key storage
crates/storage/    meeting folders and the SQLite timeline
crates/replay/     runs a recorded meeting through the pipeline
fixtures/          recorded test meetings
```

Command-line tools for development:

```bash
cargo run -p capture --bin record -- --help             # record a session as a test fixture
cargo run -p providers --bin provider-check -- --help   # test a provider key and model
cargo run -p replay -- fixtures/<session>               # replay a fixture through the pipeline
```

[PLAN.md](PLAN.md) has the design and the milestones.

### Releases

The [Build](.github/workflows/build.yml) workflow checks every push on Windows, macOS and Linux.
Pushing a tag like `v0.2.0`, or starting the workflow by hand, also builds every installer and
attaches them to a draft release. To sign and notarize the macOS app, add the secrets
`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`,
`APPLE_PASSWORD` and `APPLE_TEAM_ID` to the repository.

## License

All rights reserved. This project is not open source.
