// The meeting view: start and stop a meeting and watch the live transcript.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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

interface MeetingInfo {
  dir: string;
  started_at_unix_ms: number;
  mic: string;
  system: string;
  model: string;
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
}

let phase: Phase = "idle";
let meeting: MeetingInfo | null = null;
let speechLabel: string | null = null;
let lastSummary: MeetingSummary | null = null;
const rows = new Map<string, Row>();
let timer: number | undefined;

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
      live ? " Recording microphone and system audio" : " Stopped. Finishing the last lines…",
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
        `Mic: ${meeting.mic} · System audio: ${meeting.system} · Model: ${meeting.model}`,
      )
    : lastSummary
      ? h(
          "div",
          { class: lastSummary.error ? "result err" : "fineprint" },
          lastSummary.error ??
            `Last meeting: ${lastSummary.lines} lines saved${lastSummary.failed ? `, ${lastSummary.failed} failed` : ""} in ${lastSummary.dir}`,
        )
      : null;

  slot.replaceChildren(h("div", { class: "row-between" }, status, button), ...(details ? [details] : []));
}

function renderTranscript() {
  const list = $("transcript");
  const nearBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  const sorted = [...rows.values()].sort((a, b) => a.start_ms - b.start_ms);
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
  list.replaceChildren(
    ...sorted.map((r) =>
      h(
        "div",
        { class: `line ${r.state}` },
        h("span", { class: "line-time mono" }, clock(r.start_ms)),
        h("span", { class: `speaker ${r.speaker}` }, SPEAKER_LABEL[r.speaker]),
        h("span", { class: "line-text" }, r.state === "pending" ? "…" : r.text),
      ),
    ),
  );
  if (nearBottom && phase !== "idle") window.scrollTo({ top: document.body.scrollHeight });
}

function render() {
  renderControls();
  renderTranscript();
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
  lastSummary = null;
  render();
  try {
    meeting = await invoke<MeetingInfo>("start_meeting");
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

function onFinished(summary: MeetingSummary) {
  window.clearInterval(timer);
  phase = "idle";
  meeting = null;
  lastSummary = summary;
  render();
  if (summary.error) toast(summary.error, true);
  else toast(`Meeting saved: ${summary.lines} lines`);
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
  await listen<MeetingSummary>("meeting-finished", (e) => onFinished(e.payload));

  // The window may have been reloaded while a meeting runs.
  const status = await invoke<MeetingStatus>("meeting_status");
  if (status.meeting) {
    meeting = status.meeting;
    phase = status.stopping ? "stopping" : "running";
    timer = window.setInterval(renderControls, 1000);
  }
  await refreshSpeechLabel();
  render();
}
