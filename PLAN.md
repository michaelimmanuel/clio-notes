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
| One native streaming speech provider over WebSocket (chosen in M2) | `SpeechToText` with lower latency |

Rules:

- A provider profile is: adapter type, base URL, model name, API key. Each of the three roles
  picks a profile independently, so a user can mix providers.
- Keys are stored in Windows Credential Manager (`keyring` crate). They are never written to the
  config file, the database or the logs.
- Settings has a "Test connection" button per profile that makes one minimal call and reports
  the real error.
- Each adapter declares its capabilities (streaming, image input, JSON output) so the UI can
  refuse an invalid choice, for example a text-only model as `VisionExtractor`.
- Prompts are provider-neutral and live in `pipeline/`. Adapters only translate the request
  format.
- Structured output is validated against a schema in our own code and retried once on failure;
  we do not rely on a provider-specific JSON mode.

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
- [x] OpenAI-compatible transcription adapter (`gpt-transcribe` keywords; segment timestamps on Whisper models)
- [x] Provider profiles, key storage in Credential Manager, role assignment
- [x] Settings screen with "Test connection" and "Load models"
- [x] Error mapping: bad key, no credit, rate limit, model not found, refusal, truncation, timeout and network
- [x] `provider-check` CLI: `cargo run -p providers --bin provider-check -- --help`
- [ ] Exit check with real keys

**Exit check:** with keys from two different providers, each role can be switched between them
in Settings without restarting, and a wrong key shows a clear error.

### M2 — Live transcription

- [ ] Resample to 16 kHz and run voice-activity detection
- [ ] Mic and loopback as separate streams labelled "Me" and "Others"
- [ ] Transcript events written to the SQLite timeline
- [ ] Live transcript view in the UI
- [ ] `replay` CLI: run a fixture through the pipeline at real-time or faster speed
- [ ] Compare two or three speech providers on the fixtures, then build the native streaming adapter for the best one

**Exit check:** in a live meeting, text appears within about 3 seconds of speech and a
30-minute session runs without drops or drift.

### M3 — Screen pipeline

- [ ] Change detection on downscaled frames (perceptual hash), with a tunable threshold
- [ ] Two modes: slide (one keyframe per change) and continuous (sample and deduplicate)
- [ ] Keep full-resolution keyframes as image files; record them in the timeline
- [ ] Vision extraction to a fixed schema: title, text, tables, chart readings, code, notable terms
- [ ] Backpressure: drop intermediate keyframes when extraction falls behind
- [ ] Per-meeting cap on vision calls, visible in the UI

**Exit check:** on a slide-deck fixture, there is exactly one keyframe per slide; on a demo
fixture, the keyframe count stays under the cap and no important screen is missed.

### M4 — Fusion and live notes

- [ ] Segmenter: close a segment on a meaningful screen change or after a time limit
- [ ] Corrector: fix transcript terms using the screen text, without rephrasing
- [ ] Section writer: heading, bullets and the matching screenshot for each closed segment
- [ ] Every note line marked internally as spoken or read from screen, so the two are never merged silently
- [ ] Live notes view that fills in section by section

**Exit check:** on three fixtures, a person who was not in the meeting can read the live notes
and correctly answer five questions about what was decided and shown.

### M5 — Final pass and export

- [ ] End-of-meeting pass: merge sections, remove repetition, choose the best image per topic
- [ ] Summary, decisions and action items at the top
- [ ] Export `notes.md` plus an `images/` folder into a per-meeting directory
- [ ] Meeting list: reopen, re-run the final pass, delete

**Exit check:** the exported folder opens correctly in any Markdown viewer, and the final notes
are shorter and clearer than the live notes for the same meeting.

### M6 — Hardening and release

- [ ] Reconnect and resume when a provider connection drops mid-meeting
- [ ] Crash safety: a meeting interrupted mid-way can still produce notes from the timeline
- [ ] Visible recording indicator and a consent reminder before capture starts
- [ ] Token and call usage shown per meeting, per role
- [ ] Logs with keys and transcript content redacted
- [ ] Installer and first-run setup that walks through adding a key

**Exit check:** a 90-minute meeting with one forced network interruption still yields complete
notes, and a new user can go from install to first notes without help.

## After v1

1. Indonesian and mixed English–Indonesian speech (new speech adapter, benchmark on real recordings)
2. Telling remote speakers apart
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
