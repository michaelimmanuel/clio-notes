# Clio Notes — Development Plan

A local Windows desktop app that captures a meeting's audio and the shared screen, and writes
readable meeting notes with screenshots while the meeting is happening.

## Fixed decisions

| Topic | Decision |
|---|---|
| Capture | Local, on the user's machine (no meeting bot) |
| Mode | Live notes during the meeting, plus a final clean-up pass at the end |
| Language (v1) | English only, for both speech and notes |
| Main output | Markdown meeting notes with embedded screenshots |
| Implementation | Rust core, Tauri 2 shell, thin web UI |
| AI providers | Bring your own API key; no provider is hard-wired |
| Platform (v1) | Windows 10/11 |

## Out of scope for v1

- Indonesian and mixed-language speech (the speech layer is built so it can be added later)
- Telling remote speakers apart (v1 labels only "Me" and "Others")
- Fully offline / local models
- macOS and Linux
- Calendar integration, auto-join, cloud sync, sharing

## Architecture

```
mic task ─────┐
loopback task ┴► VAD ► SpeechToText ──► TranscriptEvent ─┐
                                                         ├► timeline (SQLite) ► segmenter
screen task ► differ ► KeyframeEvent ► screen reading ───┘                         │
                                                                corrector ◄────────┘
                                                                    │
                                                             section writer ► live UI
                                                                    │ (meeting end)
                                                               final pass ► notes.md + images/
```

- Each box is an independent `tokio` task; tasks talk through channels.
- Every event carries a timestamp from one shared monotonic clock.
- Audio is never blocked. If vision or the LLM falls behind, intermediate keyframes are dropped
  and the newest is kept.

### Workspace layout

```
crates/
  capture/     mic, system-audio loopback, screen capture, shared clock
  pipeline/    VAD, differ, timeline, segmenter, corrector, section writer, final pass
  providers/   provider traits and adapters
  storage/     SQLite timeline, image files, notes export
  replay/      CLI that feeds a recorded meeting through the pipeline
app/           Tauri shell and web UI
fixtures/      recorded test meetings (audio + frames), not committed if they contain real content
```

## Provider layer (bring your own key)

Two traits and three roles. The pipeline only ever sees the traits. Screen reading is a
`TextGenerator` call with images attached, assigned to a profile whose model accepts images.

| Trait | Job | Used by |
|---|---|---|
| `SpeechToText` | audio clip in, text (and segment timestamps where the provider gives them) out | live transcription |
| `TextGenerator` with images (role: screen reading) | keyframe image in, structured screen content out | screen pipeline |
| `TextGenerator` (role: text generation) | prompt in, text or JSON out | corrector, section writer, final pass |

Adapters planned for v1:

| Adapter | Covers |
|---|---|
| Anthropic Messages API | `TextGenerator` (screen reading, text) |
| OpenAI-compatible chat completions (configurable base URL) | `TextGenerator` for OpenAI, OpenRouter, Gemini's compatible endpoint, Ollama and similar |
| OpenAI-compatible `/audio/transcriptions`, chunked on voice-activity boundaries | `SpeechToText` for OpenAI, Groq, self-hosted Whisper servers and similar |
| OpenRouter (same adapter, own key check, model lookup, `keyterms[]` vocabulary and in-body errors) | `TextGenerator` and `SpeechToText` through one key for many providers |
| One native streaming speech provider over WebSocket (chosen in M2) | `SpeechToText` with lower latency |

Rules:

- A provider profile is: adapter type, base URL, model name, API key. Each of the three roles
  picks a profile independently, so a user can mix providers.
- Keys are stored in Windows Credential Manager (`keyring` crate). They are never written to the
  config file, the database or the logs.
- Settings has a "Test connection" button per profile that checks the key and model with the
  provider's free model-lookup endpoint and reports the real error.
- Settings refuses role assignments a provider type can never serve (Anthropic for speech to
  text). Whether an OpenAI-compatible model accepts images cannot be known in advance, so a
  wrong model for screen reading shows up as a provider error on first use.
- Prompts are provider-neutral and live in `pipeline/`. Adapters only translate the request
  format.
- Structured output uses the provider's schema mode where it exists (Anthropic
  `output_config.format`, OpenAI `response_format`), and is still validated in our own code and
  retried once on failure.

## Milestones

Each milestone ends with something runnable. Do not start the next one until the exit check
passes.

### M0 — Skeleton and capture spikes

- [x] Cargo workspace and Tauri app that opens a window
- [x] Capture microphone to a WAV file
- [x] Capture system audio (WASAPI loopback) to a WAV file
- [x] Capture a chosen window or monitor to PNG frames at 1–2 fps
- [x] Shared monotonic clock stamped on all three streams
- [x] Recorder mode that saves a whole session as a fixture (two WAVs, frames, timestamps):
      `cargo run -p capture --bin record -- --help`
- [x] Exit check on a real meeting

**Exit check:** record a 10-minute real meeting; audio from both sides is clear, frames are
readable, and timestamps line up when played back.

### M1 — Provider layer and settings

- [x] Provider traits and roles (Anthropic profiles cannot be assigned to speech to text)
- [x] Anthropic adapter and OpenAI-compatible chat adapter
- [x] OpenRouter provider (all three roles)
- [x] OpenAI-compatible transcription adapter (`gpt-transcribe` keywords; segment timestamps on Whisper models)
- [x] Provider profiles, key storage in Credential Manager, role assignment
- [x] Settings screen with "Test connection" and "Load models"
- [x] Error mapping: bad key, no credit, rate limit, model not found, refusal, truncation, timeout and network
- [x] `provider-check` CLI: `cargo run -p providers --bin provider-check -- --help`
- [x] Exit check with real keys

**Exit check:** with keys from two different providers, each role can be switched between them
in Settings without restarting, and a wrong key shows a clear error.

### M2 — Live transcription

- [x] Resample to 16 kHz and run voice-activity detection (earshot, pure Rust); cut utterances
      at 600 ms pauses, or at the quietest point after 12 s of continuous speech
- [x] Mic and loopback as separate streams labelled "Me" and "Others"
- [x] Transcript events written to the SQLite timeline (`meetings/meeting-<time>/timeline.sqlite`,
      with a speaker field ready for named speakers)
- [x] Live transcript view in the UI (Meeting tab: Start, Stop, live lines)
- [x] `replay` CLI: `cargo run -p replay -- fixtures/<session> [--speed 0] [--model <id>]`
- [x] First live meeting in the app
- [x] Exit check

Deferred from M2: compare two or three speech providers on the fixtures with `replay`, then
build the native streaming adapter for the best one (prefer one with diarization, see "Feature:
speaker identification"). Chunked transcription is good enough to build on until then.

**Exit check:** in a live meeting, text appears within about 3 seconds of speech and a
30-minute session runs without drops or drift.

### M3 — Screen pipeline

- [x] Settings > Recording: choose the microphone and the display to capture (`capture.json`
      next to `providers.json`; system audio stays on the default output)
- [x] Change detection on a 128 × 72 brightness grid of each frame (1 fps), with tunable
      thresholds (`KeyframeConfig`). A change is taken once the screen holds still; cells that
      keep changing (webcam tiles, clocks, playing video) are ignored
- [x] Two modes: "Slides only" (settled screens) and "Slides and demos" (also samples a moving
      screen at most every 10 s). Small edits are batched; going back to an earlier screen is
      recorded as a repeat, with no new image or reading
- [x] Full-resolution keyframes in `keyframes/<t_ms>.png`; timeline schema v2 adds a `keyframe`
      table (v1 files are upgraded)
- [x] Screen reading to a fixed schema: kind, title, text, tables, charts, code, terms, summary
      (strict JSON schema, validated, retried once)
- [x] Backpressure: up to 3 readings in flight; when full, only the newest waiting keyframe is
      kept and older waiting ones are saved but not read
- [x] Per-meeting cap on screen readings (default 60, set in Settings), shown in the Meeting tab
- [x] Meeting tab shows keyframes in the transcript where they appeared; the app's own window
      is hidden from screen capture during a meeting
- [x] `replay` plays the frames too: `--only screen`, `--no-read`, `--screen-mode slides`
- [x] Exit check

Measured on the 59 s desktop fixture in real time: 7 keyframes from 59 frames, all read,
median 10.7 s per reading, about 2.9k input and 1.0k output tokens per screen (gpt-6-luna).

**Exit check:** on a slide-deck fixture, there is exactly one keyframe per slide; on a demo
fixture, the keyframe count stays under the cap and no important screen is missed.

### M4 — Fusion and live notes

- [x] Sectioner: a section ends when at least 30% of the screen changes (not sooner than 30 s
      in) or after 3 minutes. Lines and keyframes are assigned by their own time; a section is
      written once its utterances are back and its screens are read. Stretches with under 12
      words and no new screen fold into the next section
- [x] Corrector: phrases that match a screen term apart from case, spacing and punctuation
      take the screen's spelling locally; real mishearings come back from the section writer
      as "heard / should be" pairs and are applied only if the replacement is on screen. The
      text as transcribed is kept in the timeline next to the corrected one
- [x] Section writer: one text-generation call per section returns the heading, bullets, the
      screenshot to show and the corrections (strict JSON schema, validated, retried once);
      sections are written in order, each told the earlier headings
- [x] Every bullet carries a source (spoken, screen, both) and a kind (point, decision,
      action); a section with no screens cannot claim "screen" and one with no speech cannot
      claim a decision
- [x] Meeting tab: Notes view that fills in section by section, next to the transcript;
      corrected lines are marked. Sections are also appended to `notes-live.md` in the meeting
      folder and stored in the timeline (schema v3)
- [x] `replay` writes notes too: `--no-notes`, `--text-profile`, `--section-max-s`
- [x] First live meeting with notes in the app
- [x] Exit check

Measured on the 59 s desktop fixture in real time: 2 sections, about 10 s and 2.0k input /
0.9k output tokens each (gpt-6-luna), 1 line corrected ("cloud session" to "Claude session").

**Exit check:** on three fixtures, a person who was not in the meeting can read the live notes
and correctly answer five questions about what was decided and shown.

### M5 — Final pass and export

- [x] End-of-meeting pass, run automatically when a meeting stops: one text-generation call
      over the live sections and the screen list merges topics, removes repetition and picks
      one image per topic (strict JSON schema, validated, retried once). Section and image
      numbers are checked; spoken/screen marks are kept
- [x] Title, summary, decisions and action items (with owner when said) at the top
- [x] Export into the meeting folder: `notes.md`, `images/NN-topic.png` (only the images
      used, linked by relative paths) and `transcript.md` with screen changes marked. The notes
      are also stored in the timeline. Without a text profile, or if the call fails, `notes.md`
      is written from the live notes with a line saying so
- [x] Meetings tab: list with status (final notes, none, interrupted, recording); each
      meeting shows its final notes, live notes and transcript; open folder, rewrite final
      notes, move to the Recycle Bin (with confirmation)
- [x] `replay` runs the final pass too (`--no-final`)
- [x] First live meeting with final notes in the app
- [x] Exit check

Measured on the 59 s desktop fixture: 2 live sections became 2 topics with a title, summary and
1 action item; 12.5 s, about 1.1k input / 1.2k output tokens (gpt-6-luna).

**Exit check:** the exported folder opens correctly in any Markdown viewer, and the final notes
are shorter and clearer than the live notes for the same meeting.

### M6 — Hardening and release

- [x] Reconnect and resume when a provider connection drops mid-meeting: every client is
      wrapped so calls that can pass (network, timeout, rate limit, overload) are retried with
      growing pauses (2–30 s) for up to 20 minutes, 90 s once the meeting is stopping.
      Utterances wait in their own tasks, so audio keeps being cut; the notes wait for the
      transcript. The Meeting view shows a banner while a connection is down
      (`meeting-connection` events)
- [x] Crash safety: WAV files are flushed every 5 s; at startup a meeting that never stopped
      gets a stop time and repaired WAV headers and shows as interrupted, with a notice on the
      Meeting view. Its final pass first writes notes for the part after the last live
      section from the saved transcript and screens. Closing the window while recording is
      refused
- [x] Visible recording indicator and a consent reminder before capture starts: red REC badge
      with the time in the top bar, "● Recording" in the window title, red bar on the taskbar
      button; the reminder can be turned off in Settings > Recording
- [x] Token and call usage shown per meeting, per role (speech to text with audio minutes,
      screen reading, live notes, final notes), stored as `usage` in the timeline
- [x] Logs with keys and transcript content redacted: `logs/clio-notes.log` (2 MB, one old
      file kept) holds counts, timings, device and model names and errors only; every line is
      passed through a key/token filter; panics are logged. Settings links to the folder
- [x] Installer and first-run setup that walks through adding a key: NSIS installer
      (per-user, WebView2 bootstrapper); with no profiles the app opens a three-step setup
      (provider and key, model per job, ready), also under Settings > Jobs
- [x] First live meeting with a forced network interruption
- [x] Exit check

**Exit check:** a 90-minute meeting with one forced network interruption still yields complete
notes, and a new user can go from install to first notes without help.

## After v1

1. Indonesian and mixed English–Indonesian speech (new speech adapter, benchmark on real recordings)
2. Telling remote speakers apart (see "Feature: speaker identification" below)
3. Local speech model option (`whisper-rs`)
4. Ask questions about past meetings
5. HTML and PDF export
6. macOS

## Risks

| Risk | Mitigation |
|---|---|
| System-audio capture misses audio from some meeting apps or devices | Proven in M0 before anything else is built |
| Vision cost on demo-heavy meetings | Deduplication threshold and per-meeting cap (M3) |
| Notes state things nobody said or showed | Spoken/screen marking and the M4 reading test |
| Providers differ in streaming and structured-output behaviour | Capability model, own schema validation, chunked fallback adapter |
| Recording without participants knowing | Indicator and consent reminder (M6); the user stays responsible |

## Feature: speaker identification

Label each remote participant separately, by name where possible, instead of one "Others"
label. Planned for after v1.

**Today (v1):** "Me" comes from the microphone and "Others" from system audio. This is reliable
because it uses the source of the audio, not voice recognition. Every remote participant arrives
mixed into the one system-audio stream, so they cannot be told apart this way.

**Ways to separate remote speakers:**

| Approach | Gives | Trade-offs |
|---|---|---|
| Speech provider with diarization | "Speaker 1", "Speaker 2" with the transcript | Depends on the provider; short clips are unreliable, so it usually needs streaming or longer audio |
| Local diarization model | "Speaker 1", "Speaker 2" for any speech provider | Extra setup and CPU use on the user's machine |
| Read the meeting app's active-speaker highlight from the screen | Real names (Zoom, Teams and Meet show who is talking) | Only works when the gallery or speaker view is visible, not during screen sharing |

**Proposed design:**

- Diarization groups the system-audio stream by voice into anonymous speakers.
- When the meeting app's speaker view is on screen, the screen pipeline reads the highlighted
  name and links it to the anonymous speaker talking at that moment.
- Speakers not named this way can be renamed by the user after the meeting; a rename applies to
  the whole meeting.
- Notes say "Speaker 2" rather than guessing a name when the link is uncertain.

**Groundwork before then:**

- When choosing the native streaming speech provider in M2, prefer one that supports
  diarization, so adding this feature does not require switching providers.
- Keep a speaker field on transcript events in the timeline from M2 on ("Me" / "Others" for now),
  so labelled speakers fit without a schema change.

**Exit check:** on a recorded call with at least three remote participants, at least 90% of
transcript lines are given the right speaker, and every named speaker's name is correct.
