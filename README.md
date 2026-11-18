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
  into Obsidian, Confluence or an email.
- **Edit before you share.** Fix a name, reword a point, add or remove decisions, action items,
  topics and screenshots. `notes.md` is updated when you save.
- **Ask about a meeting.** Ask "what did we decide about pricing?" or "what are my action
  items?" and get an answer from that meeting, with the times it comes from. Click a time to
  see that point of the transcript.
- **Search and ask across meetings.** Search every meeting's notes, screens and transcript
  from the meeting list, or ask "what did I agree to do this week?" and get an answer from all
  your meetings, with links to the meetings and times it comes from. Optionally, search by
  meaning also finds meetings that use other words for the same thing.
- **Download or send to Notion.** Save a meeting's notes as Markdown or as a zip, or export
  them as a Notion page with their screenshots.
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

Clio Notes has three jobs, and a fourth that is optional. Each can use a different provider and
model:

| Job | What it does | Providers |
|---|---|---|
| Speech to text | Turns the audio into a transcript | OpenAI, OpenRouter, Groq, a self-hosted Whisper server, or any OpenAI-compatible transcription endpoint |
| Screen reading | Reads each new slide or screen | Any model that accepts images: Anthropic, OpenAI, OpenRouter, Gemini, Ollama and other OpenAI-compatible endpoints |
| Notes writing | Writes the live sections and the final notes | Anthropic, OpenAI, OpenRouter, Gemini, Ollama and other OpenAI-compatible endpoints |
| Search by meaning (optional) | Finds meetings by meaning when you ask across meetings | Any embedding model behind an OpenAI-compatible `/embeddings` endpoint: OpenAI, Gemini, Mistral, Voyage, Ollama. OpenRouter works where it offers the model. Not Anthropic, which has no embeddings API |

One OpenRouter key can cover all three jobs. **Settings > Jobs** has a "Test connection" button
for each provider and lists its models.

API keys are kept in your system's key store (Windows Credential Manager, the macOS Keychain or
the Linux Secret Service). They are never written to settings files, meeting folders or logs.

## Editing, download and Notion

On a meeting under **Meeting notes**, once its final notes are written:

- **Edit notes** turns the final notes into a form: title, summary, decisions, action items with
  owners, and each topic's heading, points and screenshot. **Save changes** rewrites `notes.md`
  and its images. **Rewrite final notes** asks first, since it replaces your edits.

- **Download > Markdown (.md)** saves the notes with a folder of their screenshots next to them.
- **Download > Zip (.zip)** saves the notes, screenshots and transcript in one file. Notion's
  own Import (Markdown & CSV) also accepts it.
- **Export to Notion** creates a new Notion page with the title, summary, decisions, action
  items as to-dos, and each topic with its screenshot. The meeting then shows a link to it.

To connect Notion, open **Settings > Notion** and follow the steps there:

1. Create an internal integration at
   [notion.so/profile/integrations](https://www.notion.so/profile/integrations) and copy its
   secret.
2. Paste the secret in Settings and click **Connect**.
3. In Notion, open the page your notes should go under, click **••• > Connections** and add the
   integration. Then pick that page in Settings.

The secret is kept in your system's key store, like your AI keys.

## Asking about a meeting

Open a meeting under **Meeting notes** and click the **Ask about this meeting** bar at the bottom
of the window. Type a question, or start with one of the suggestions. The answer is written as
you watch, above the bar, and follow-up questions work ("and who owns that?"). Press Esc or
click elsewhere to close the answers; the bar shows how many there are, so you can reopen them.

- Answers come only from that meeting: its notes, the text read from its screens, and the
  transcript. When the meeting doesn't say, the answer says so.
- Each point cites its time, like `[12:34]`. Click it to jump to that line of the transcript.
- Ask in English or Indonesian; the answer is in the language of the question.
- Questions are answered by the **Notes writing** model you chose in Settings, so the meeting is
  sent only to a provider that already writes its notes. With Anthropic, and with Anthropic
  or Gemini models through OpenRouter, the meeting is cached between questions, so follow-ups
  cost less.
- Questions and answers are kept with the meeting, in `timeline.sqlite`. They are not added to
  `notes.md`, downloads or Notion exports. **Clear chat** deletes them.
- Questions are counted under **Usage** as "Questions".

A meeting's transcript of more than about six hours is shortened to fit: its start and end are
kept, and the notes still cover the middle.

## Searching and asking across meetings

The **Meeting notes** list has a search box at the top (Ctrl+F) and an **Ask about all your
meetings** bar at the bottom.

**Search** finds the meetings that have every word you type somewhere in their notes, screens
or transcript, and shows the best matching lines with the words marked. Click a line to open
the meeting at that point. Search runs on your computer and sends nothing to a provider.

**Ask** answers a question from all your meetings:

1. The **Notes writing** model turns the question into search words, in English and
   Indonesian, and a date range when the question names one ("last week", "in October").
2. Your meetings are searched, and the notes of the best matches (up to 12) are sent with the
   parts of their transcript and screens that matched. Other meetings in the date range are
   listed by title.
3. The answer cites each point with its meeting and time. Click one to open that meeting there.

Answers are only as good as the search. Without search by meaning, a meeting that uses none of
the search words is read only when nothing else matches, so name the topic, person or product
in your question. A question takes two calls and sends up to about 50,000 tokens, a few times
more than a question about one meeting.

### Search by meaning (optional)

Words alone miss a meeting that said "cost" when you asked about "pricing", or "postpone the
launch" when you asked about delays. Search by meaning finds those too. It is off until you turn
it on, and everything else works without it.

To turn it on, add a profile with an embedding model under **Settings > Jobs**, such as
`text-embedding-3-small` from OpenAI (the **OpenAI embeddings** preset), or `bge-m3` in Ollama
on your own computer. Then choose that profile for **Search by meaning**.

- **Indexing.** Clio Notes splits every meeting into pieces (notes, topics, screens and stretches
  of transcript) and has the embedding model turn each into a vector that stands for its
  meaning. This runs in the background, newest meetings first, and Settings shows how far it
  is. New meetings are indexed when their notes are written. A piece is sent only once: when a
  meeting changes, only its changed pieces are sent again.
- **Asking.** Your question is turned into a vector too, and compared with every piece.
  Pieces close in meaning and pieces that share the question's words are ranked together, so a
  piece found both ways comes first. Meetings not indexed yet are still found by their words.
- **Privacy.** Turning it on sends the text of every meeting (notes, screen text and
  transcripts) to the embedding provider, which may be one that does not see your meetings
  otherwise. With Ollama, nothing leaves your computer. The vectors are kept in
  `library.sqlite`.
- **Cost.** Indexing a one-hour meeting sends roughly 20,000 tokens, and each question adds a
  few dozen. Embedding models cost a small fraction of what chat models do; check your
  provider's price.
- **Changing the model.** Vectors from different models cannot be compared, so choosing another
  embedding model indexes every meeting again and drops the old vectors.
- **Problems.** If the provider fails (a wrong key, no credit, a model name it does not
  know), indexing stops, Settings shows why with a **Try again** button, and it is tried
  again every 15 minutes. Questions keep working by words alone. A piece the provider
  refuses, such as one that is too long, is skipped.
- **Vector size.** OpenAI's `text-embedding-3` models are asked for vectors of 512 numbers
  and Gemini's embedding models for 768, directly or through OpenRouter, which keeps
  `library.sqlite` small and questions quick. Other models use their own size.

Questions across meetings and their answers are kept in `library.sqlite` and deleted with
**Clear chat**. The meeting being recorded is left out until it stops.

## Reliability

- If a provider connection drops, recording continues. Calls are retried for up to 20 minutes,
  and the notes catch up when the connection is back. A banner shows while a connection is down.
- If the app or the computer crashes mid-meeting, the recording is kept. The next time the app
  opens, the meeting shows as interrupted and its notes can still be written.
- While recording, a red REC badge with the time is shown on every tab, and the window cannot
  be closed until the meeting is stopped.

## Updates

Clio Notes checks for a new version when it opens and every few hours. When one is out, a banner
offers **Install and restart**; nothing is downloaded until you click it, and it waits until
your meeting and its final notes are done. **Settings > About** shows your version and has a
**Check for updates** link. Updates are signed, and the app installs only ones signed with the
project's key.

Automatic updates work on Windows, macOS and the Linux AppImage. With the `.deb`, install the
new `.deb` from the Releases page. Version 0.1.0 has no updater, so update it by hand once.

## Where files are kept

| | Windows | macOS | Linux |
|---|---|---|---|
| Meetings | `%APPDATA%\com.clionotes.app\meetings` | `~/Library/Application Support/Clio Notes/meetings` | `~/.local/share/com.clionotes.app/meetings` |
| Settings | `%APPDATA%\com.clionotes.app` | `~/Library/Application Support/Clio Notes` | `~/.config/com.clionotes.app` |
| Logs | next to `meetings`, in `logs/` | same | same |
| Search index | next to `meetings`, in `library.sqlite` | same | same |

Logs hold counts, timings, device names, model names and errors only. Keys and meeting content
are filtered out. Settings has a link to the log folder.

`library.sqlite` holds a copy of every meeting's text for searching, the vectors for search by
meaning when it is on, and the questions asked across meetings. The copy is updated when a
meeting changes and dropped when it is deleted. If you delete the file, the search index is
rebuilt and the vectors are made again; only the questions are lost.

Each meeting folder holds:

```
meeting-<date and time>/
  notes.md          final notes
  notes-live.md     the live notes as they were written
  transcript.md     the full transcript, with screen changes marked
  images/           the screenshots used in notes.md
  keyframes/        every screen change captured
  mic.opus          your microphone
  system.opus       the computer's sound
  timeline.sqlite   everything above, with timings, for rewriting the notes
```

While a meeting is recorded, its sound is saved as `mic.wav` and `system.wav`, which survive a
crash. Once it stops, they are compressed in the background to `mic.opus` and `system.opus`:
about 30 MB per hour of meeting instead of about 700 MB, with speech as clear as before. Each
WAV is deleted only after its compressed file is written and checked; if compressing fails, the
WAV is kept and tried again later. Opus files play in VLC, web browsers and most audio players.

## Languages

**Settings > Recording** has two language settings:

- **Meeting language:** what is spoken. Choose English, Indonesian, or **Indonesian and English
  (mixed)** for meetings that switch between the two, even mid-sentence. With "mixed", the
  speech model detects the language of each sentence, so English stays English. Indonesian
  needs a speech model that supports it, such as Whisper large-v3 or large-v3-turbo.
- **Notes language:** English or Indonesian. The notes are written in this language whatever
  is spoken. Names, product names and technical terms are kept as they were said or shown.

Each meeting keeps the languages it was recorded with, so rewriting its notes later gives
notes in the same language.

## Limits

- Speech in English and Indonesian (or a mix of both); notes in English or Indonesian.
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

The installer lands in `target/release/bundle/`. Release builds also sign the update files,
so set `TAURI_SIGNING_PRIVATE_KEY` to the updater key (or its path) first, and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to its password, which may be empty. Each system's installer must be built on that
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
attaches them to a draft release, together with the signed update files and `latest.json`.
Installed apps see the update once the release is published.

To ship an update:

1. Raise the version in `app/src-tauri/tauri.conf.json` (and in `app/package.json` and the
   root `Cargo.toml` to keep them in step), then commit.
2. Push a tag with that version, such as `git tag v0.2.0 && git push origin v0.2.0`.
3. When the workflow is done, write the release notes on the draft release and publish it.

The workflow needs the repository secret `TAURI_SIGNING_PRIVATE_KEY` (the content of the
updater private key), and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if the key has a password. Keep the private key safe: if it
is lost, installed apps can no longer be updated automatically.

To sign and notarize the macOS app, add the secrets `APPLE_CERTIFICATE`,
`APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and
`APPLE_TEAM_ID` to the repository.

## License

Clio Notes is open source under the [MIT License](LICENSE). You may use, copy, change and share
it, including in commercial work, as long as the copyright notice and license text are kept.

Contributions are welcome: open an issue or a pull request.
