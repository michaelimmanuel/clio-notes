// The meeting view: start and stop a meeting, watch the live transcript with
// screen keyframes placed where they appeared, and read the notes as each
// section is written.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { refreshDevices } from "./devices";
import { $, errorText, h, toast } from "./dom";

// ---- Types mirrored from app/src-tauri/src/meeting.rs and crates/pipeline ----

type Speaker = "me" | "others";

interface TranscriptLine {
  speaker: Speaker;
  start_ms: number;
  end_ms: number;
  text: string;
  model: string;
}

type PipelineEvent =
  | { type: "detected"; speaker: Speaker; start_ms: number; end_ms: number }
  | { type: "transcribed"; line: TranscriptLine; latency_ms: number }
  | { type: "empty"; speaker: Speaker; start_ms: number; end_ms: number }
  | { type: "failed"; speaker: Speaker; start_ms: number; end_ms: number; message: string };

interface ScreenReading {
  kind: string;
  title: string;
  text: string;
  terms: string[];
  summary: string;
}

type SkipReason = "busy" | "cap" | "no_profile";

type ScreenEvent =
  | {
      type: "keyframe";
      t_ms: number;
      file: string;
      reason: "first" | "settled" | "moving";
      repeat_of: number | null;
      thumbnail: string;
    }
  | { type: "read"; t_ms: number; reading: ScreenReading; latency_ms: number; reads_used: number }
  | { type: "read_skipped"; t_ms: number; reason: SkipReason }
  | { type: "read_failed"; t_ms: number; message: string };

interface Bullet {
  text: string;
  source: "spoken" | "screen" | "both";
  kind: "point" | "decision" | "action";
}

interface Section {
  index: number;
  start_ms: number;
  end_ms: number;
  heading: string;
  bullets: Bullet[];
  image: { t_ms: number; file: string } | null;
  model: string;
}

type NotesEvent =
  | { type: "writing"; index: number; start_ms: number; end_ms: number }
  | { type: "section"; section: Section; latency_ms: number }
  | { type: "empty"; index: number }
  | { type: "failed"; index: number; start_ms: number; end_ms: number; message: string }
  | { type: "corrected"; speaker: Speaker; start_ms: number; text: string };

interface MeetingInfo {
  dir: string;
  started_at_unix_ms: number;
  mic: string;
  system: string;
  model: string;
  screen: string | null;
  vision_model: string | null;
  read_cap: number | null;
  notes_model: string | null;
}

interface MeetingStatus {
  meeting: MeetingInfo | null;
  stopping: boolean;
  elapsed_ms: number;
}

interface MeetingSummary {
  dir: string;
  lines: number;
  failed: number;
  keyframes: number;
  reads: number;
  sections: number;
  error: string | null;
}

interface SpeechSetup {
  profiles: { id: string; name: string; model: string }[];
  roles: { speech_to_text: string | null };
}

// ---- State ----

type Phase = "idle" | "starting" | "running" | "stopping";

interface Row {
  speaker: Speaker;
  start_ms: number;
  state: "pending" | "done" | "failed";
  text: string;
  latency_ms?: number;
  /// A term's spelling was fixed from the screen.
  corrected?: boolean;
}

interface NoteCard {
  index: number;
  start_ms: number;
  end_ms: number;
  state: "writing" | "done" | "failed";
  section?: Section;
  message?: string;
}

type Pane = "notes" | "transcript";

interface Shot {
  t_ms: number;
  thumbnail: string;
  repeat_of: number | null;
  state: "reading" | "read" | "skipped" | "failed" | "repeat";
  reading?: ScreenReading;
  note?: string;
}

let phase: Phase = "idle";
let meeting: MeetingInfo | null = null;
let speechLabel: string | null = null;
let lastSummary: MeetingSummary | null = null;
const rows = new Map<string, Row>();
const shots = new Map<number, Shot>();
const notes = new Map<number, NoteCard>();
let readsUsed = 0;
let pane: Pane = "transcript";
/// The notes model of the meeting on screen, kept after it ends.
let notesModel: string | null = null;
let timer: number | undefined;

const SKIP_NOTE: Record<SkipReason, string> = {
  busy: "Not read: screens changed faster than they could be read.",
  cap: "Not read: this meeting reached its limit on screen reads.",
  no_profile: "Not read: no profile is assigned to screen reading.",
};

const SPEAKER_LABEL: Record<Speaker, string> = { me: "Me", others: "Others" };

const rowKey = (speaker: Speaker, start_ms: number) => `${speaker}:${start_ms}`;

function clock(ms: number): string {
  const s = Math.floor(ms / 1000);
  const hh = Math.floor(s / 3600);
  const mm = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  return hh > 0 ? `${hh}:${mm}:${ss}` : `${mm}:${ss}`;
}

// ---- Rendering ----

function renderControls() {
  const slot = $("meeting-controls");
  const elapsed = meeting ? Date.now() - meeting.started_at_unix_ms : 0;

  let status: HTMLElement;
  let button: HTMLButtonElement;
  if (phase === "idle") {
    status = speechLabel
      ? h("div", { class: "meeting-status" }, h("span", { class: "dot" }), `Ready. Speech to text: ${speechLabel}`)
      : h(
          "div",
          { class: "meeting-status warn" },
          "Choose a speech to text profile in ",
          h("a", { href: "#", onclick: (e: Event) => (e.preventDefault(), showView("settings")) }, "Settings"),
          " before starting.",
        );
    button = h(
      "button",
      { class: "btn primary big", type: "button", disabled: !speechLabel, onclick: start },
      "Start meeting",
    );
  } else if (phase === "starting") {
    status = h("div", { class: "meeting-status" }, "Starting capture…");
    button = h("button", { class: "btn primary big", type: "button", disabled: true }, "Start meeting");
  } else {
    const live = phase === "running";
    status = h(
      "div",
      { class: "meeting-status" },
      h("span", { class: `dot ${live ? "live" : ""}` }),
      h("span", { class: "elapsed mono" }, clock(elapsed)),
      live
        ? ` Recording microphone, system audio${meeting?.screen ? " and screen" : ""}`
        : " Stopped. Finishing the last lines and screens…",
    );
    button = h(
      "button",
      { class: "btn danger big", type: "button", disabled: !live, onclick: stop },
      live ? "Stop" : "Finishing…",
    );
  }

  const details = meeting
    ? h(
        "div",
        { class: "fineprint" },
        `Mic: ${meeting.mic} · System audio: ${meeting.system} · Speech model: ${meeting.model}`,
        h("br"),
        meeting.screen
          ? `Screen: ${meeting.screen} · ${screenCounts()}`
          : "Screen: not captured (turned off in Settings)",
        h("br"),
        meeting.notes_model
          ? `Notes: ${writtenSections()} section${writtenSections() === 1 ? "" : "s"} written (${meeting.notes_model})`
          : "Notes: off. Assign a profile to text generation in Settings to get notes.",
      )
    : lastSummary
      ? h(
          "div",
          { class: lastSummary.error ? "result err" : "fineprint" },
          lastSummary.error ??
            `Last meeting: ${lastSummary.lines} lines${lastSummary.failed ? ` (${lastSummary.failed} failed)` : ""}, ` +
              `${lastSummary.keyframes} keyframes (${lastSummary.reads} read), ` +
              `${lastSummary.sections} notes section${lastSummary.sections === 1 ? "" : "s"}. Saved in ${lastSummary.dir}`,
        )
      : null;

  slot.replaceChildren(h("div", { class: "row-between" }, status, button), ...(details ? [details] : []));
}

function writtenSections(): number {
  return [...notes.values()].filter((n) => n.state === "done").length;
}

function screenCounts(): string {
  const keyframes = [...shots.values()].filter((s) => s.repeat_of === null).length;
  const reading = meeting?.vision_model
    ? `${readsUsed}${meeting.read_cap !== null ? ` of ${meeting.read_cap}` : ""} screen reads (${meeting.vision_model})`
    : "not read: no screen-reading profile";
  return `${keyframes} keyframe${keyframes === 1 ? "" : "s"}, ${reading}`;
}

function lineView(r: Row): HTMLElement {
  return h(
    "div",
    { class: `line ${r.state}` },
    h("span", { class: "line-time mono" }, clock(r.start_ms)),
    h("span", { class: `speaker ${r.speaker}` }, SPEAKER_LABEL[r.speaker]),
    h(
      "span",
      { class: "line-text" },
      r.state === "pending" ? "…" : r.text,
      r.corrected
        ? h("span", { class: "fixed", title: "A term in this line was respelled as it appears on screen." }, "corrected")
        : null,
    ),
  );
}

function shotView(s: Shot): HTMLElement {
  let body: HTMLElement;
  if (s.state === "repeat") {
    body = h("div", { class: "shot-note" }, `Back to the screen from ${clock(s.repeat_of ?? 0)}`);
  } else if (s.state === "read" && s.reading) {
    const r = s.reading;
    body = h(
      "div",
      {},
      r.title ? h("div", { class: "shot-title" }, r.title) : null,
      h("div", { class: "shot-summary" }, r.summary),
      r.terms.length
        ? h("div", { class: "shot-terms" }, ...r.terms.slice(0, 12).map((t) => h("span", { class: "term" }, t)))
        : null,
    );
  } else {
    body = h(
      "div",
      { class: `shot-note ${s.state === "failed" ? "err" : ""}` },
      s.state === "reading" ? "Reading the screen…" : (s.note ?? ""),
    );
  }
  return h(
    "div",
    { class: `line shot ${s.state}` },
    h("span", { class: "line-time mono" }, clock(s.t_ms)),
    h("span", { class: "speaker screen" }, "Screen"),
    h(
      "div",
      { class: "shot-body" },
      h("img", { class: `shot-thumb ${s.state === "repeat" ? "small" : ""}`, src: s.thumbnail, alt: "Keyframe" }),
      body,
    ),
  );
}

function renderTranscript() {
  const list = $("transcript");
  const nearBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  const items: { t: number; view: () => HTMLElement }[] = [
    ...[...rows.values()].map((r) => ({ t: r.start_ms, view: () => lineView(r) })),
    ...[...shots.values()].map((s) => ({ t: s.t_ms, view: () => shotView(s) })),
  ];
  const sorted = items.sort((a, b) => a.t - b.t);
  if (sorted.length === 0) {
    list.replaceChildren(
      h(
        "div",
        { class: "empty" },
        phase === "idle"
          ? "The transcript appears here while the meeting runs."
          : "Listening. Lines appear a moment after each pause in speech.",
      ),
    );
    return;
  }
  list.replaceChildren(...sorted.map((i) => i.view()));
  if (nearBottom && phase !== "idle") window.scrollTo({ top: document.body.scrollHeight });
}

const BULLET_LABEL: Record<Bullet["kind"], string | null> = {
  point: null,
  decision: "Decision",
  action: "Action",
};

const SOURCE_LABEL: Record<Bullet["source"], string | null> = {
  spoken: null,
  screen: "on screen",
  both: "said and shown",
};

function noteView(n: NoteCard): HTMLElement {
  const range = `${clock(n.start_ms)} – ${clock(n.end_ms)}`;
  if (n.state !== "done" || !n.section) {
    return h(
      "div",
      { class: `card note ${n.state}` },
      h("div", { class: "note-time mono" }, range),
      h(
        "div",
        { class: `shot-note ${n.state === "failed" ? "err" : ""}` },
        n.state === "writing" ? "Writing this section…" : `Not written: ${n.message ?? ""}`,
      ),
    );
  }
  const s = n.section;
  const thumbnail = s.image ? shots.get(s.image.t_ms)?.thumbnail : undefined;
  return h(
    "div",
    { class: "card note" },
    h("div", { class: "note-time mono" }, range),
    h("h3", { class: "note-heading" }, s.heading),
    h(
      "div",
      { class: thumbnail ? "note-body with-image" : "note-body" },
      h(
        "ul",
        { class: "note-bullets" },
        ...s.bullets.map((b) =>
          h(
            "li",
            { class: b.kind },
            BULLET_LABEL[b.kind] ? h("span", { class: `tag ${b.kind}` }, BULLET_LABEL[b.kind]!) : null,
            b.text,
            SOURCE_LABEL[b.source] ? h("span", { class: "source" }, SOURCE_LABEL[b.source]!) : null,
          ),
        ),
      ),
      thumbnail ? h("img", { class: "note-image", src: thumbnail, alt: `Screen at ${clock(s.image!.t_ms)}` }) : null,
    ),
  );
}

function renderNotes() {
  const list = $("notes");
  const nearBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  const sorted = [...notes.values()].sort((a, b) => a.index - b.index);
  if (sorted.length === 0) {
    const off = phase !== "idle" && meeting !== null && !meeting.notes_model;
    list.replaceChildren(
      h(
        "div",
        { class: "card empty" },
        off || (phase === "idle" && rows.size > 0 && !notesModel)
          ? "Notes are off. Assign a profile to text generation in Settings to get notes."
          : phase === "idle"
            ? "Notes appear here section by section while the meeting runs."
            : "The first section appears shortly after the first topic or slide ends, or after three minutes.",
      ),
    );
  } else {
    list.replaceChildren(...sorted.map(noteView));
    if (nearBottom && phase !== "idle" && pane === "notes") window.scrollTo({ top: document.body.scrollHeight });
  }
  const count = writtenSections();
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".seg-btn")) {
    if (btn.dataset.pane === "notes") btn.textContent = count ? `Notes · ${count}` : "Notes";
  }
}

function showPane(next: Pane) {
  pane = next;
  $("notes").hidden = pane !== "notes";
  $("transcript").hidden = pane !== "transcript";
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".seg-btn")) {
    btn.setAttribute("aria-selected", String(btn.dataset.pane === pane));
  }
}

function render() {
  renderControls();
  renderTranscript();
  renderNotes();
}

// ---- Actions ----

async function refreshSpeechLabel() {
  try {
    const s = await invoke<SpeechSetup>("get_settings");
    const p = s.profiles.find((p) => p.id === s.roles.speech_to_text);
    speechLabel = p ? `${p.name} (${p.model})` : null;
  } catch {
    speechLabel = null;
  }
}

async function start() {
  phase = "starting";
  rows.clear();
  shots.clear();
  notes.clear();
  readsUsed = 0;
  lastSummary = null;
  render();
  try {
    meeting = await invoke<MeetingInfo>("start_meeting");
    notesModel = meeting.notes_model;
    phase = "running";
    timer = window.setInterval(renderControls, 1000);
  } catch (e) {
    phase = "idle";
    toast(errorText(e), true);
  }
  render();
}

async function stop() {
  try {
    await invoke("stop_meeting");
    phase = "stopping";
  } catch (e) {
    toast(errorText(e), true);
  }
  render();
}

function onEvent(e: PipelineEvent) {
  if (e.type === "transcribed") {
    const l = e.line;
    rows.set(rowKey(l.speaker, l.start_ms), {
      speaker: l.speaker,
      start_ms: l.start_ms,
      state: "done",
      text: l.text,
      latency_ms: e.latency_ms,
    });
  } else if (e.type === "detected") {
    rows.set(rowKey(e.speaker, e.start_ms), {
      speaker: e.speaker,
      start_ms: e.start_ms,
      state: "pending",
      text: "",
    });
  } else if (e.type === "empty") {
    rows.delete(rowKey(e.speaker, e.start_ms));
  } else {
    rows.set(rowKey(e.speaker, e.start_ms), {
      speaker: e.speaker,
      start_ms: e.start_ms,
      state: "failed",
      text: `Not transcribed: ${e.message}`,
    });
  }
  renderTranscript();
}

function onScreen(e: ScreenEvent) {
  if (e.type === "keyframe") {
    shots.set(e.t_ms, {
      t_ms: e.t_ms,
      thumbnail: e.thumbnail,
      repeat_of: e.repeat_of,
      state: e.repeat_of === null ? "reading" : "repeat",
    });
  } else {
    const s = shots.get(e.t_ms);
    if (!s) return;
    if (e.type === "read") {
      s.state = "read";
      s.reading = e.reading;
      readsUsed = Math.max(readsUsed, e.reads_used);
    } else if (e.type === "read_skipped") {
      s.state = "skipped";
      s.note = SKIP_NOTE[e.reason];
    } else {
      s.state = "failed";
      s.note = `Not read: ${e.message}`;
    }
  }
  renderTranscript();
  renderControls();
}

function onNotes(e: NotesEvent) {
  if (e.type === "corrected") {
    const row = rows.get(rowKey(e.speaker, e.start_ms));
    if (row) {
      row.text = e.text;
      row.corrected = true;
    }
    renderTranscript();
    return;
  }
  if (e.type === "writing") {
    notes.set(e.index, { index: e.index, start_ms: e.start_ms, end_ms: e.end_ms, state: "writing" });
  } else if (e.type === "section") {
    const s = e.section;
    notes.set(s.index, { index: s.index, start_ms: s.start_ms, end_ms: s.end_ms, state: "done", section: s });
  } else if (e.type === "empty") {
    notes.delete(e.index);
  } else {
    notes.set(e.index, {
      index: e.index,
      start_ms: e.start_ms,
      end_ms: e.end_ms,
      state: "failed",
      message: e.message,
    });
  }
  renderNotes();
  renderControls();
}

function onFinished(summary: MeetingSummary) {
  window.clearInterval(timer);
  phase = "idle";
  meeting = null;
  lastSummary = summary;
  render();
  if (summary.error) toast(summary.error, true);
  else
    toast(
      `Meeting saved: ${summary.lines} lines, ${summary.keyframes} keyframes, ${summary.sections} notes sections`,
    );
}

// ---- Views ----

export type View = "meeting" | "settings";

export function showView(view: View) {
  $("view-meeting").hidden = view !== "meeting";
  $("view-settings").hidden = view !== "settings";
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.setAttribute("aria-selected", String(tab.dataset.view === view));
  }
  if (view === "meeting") void showMeetingView();
  // Picks up devices plugged in since the list was last read.
  if (view === "settings") void refreshDevices();
}

export async function showMeetingView() {
  if (phase === "idle") {
    await refreshSpeechLabel();
    renderControls();
  }
}

export async function initMeeting() {
  for (const tab of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    tab.addEventListener("click", () => showView(tab.dataset.view as View));
  }
  await listen<PipelineEvent>("meeting-event", (e) => onEvent(e.payload));
  await listen<ScreenEvent>("meeting-screen", (e) => onScreen(e.payload));
  await listen<NotesEvent>("meeting-notes", (e) => onNotes(e.payload));
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".seg-btn")) {
    btn.addEventListener("click", () => showPane(btn.dataset.pane as Pane));
  }
  await listen<MeetingSummary>("meeting-finished", (e) => onFinished(e.payload));

  // The window may have been reloaded while a meeting runs.
  const status = await invoke<MeetingStatus>("meeting_status");
  if (status.meeting) {
    meeting = status.meeting;
    notesModel = meeting.notes_model;
    phase = status.stopping ? "stopping" : "running";
    timer = window.setInterval(renderControls, 1000);
  }
  await refreshSpeechLabel();
  render();
}
