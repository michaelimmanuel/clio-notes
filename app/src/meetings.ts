// The Meetings view: past meetings, their final notes, live notes and
// transcript, and the actions on them (open folder, rewrite final notes,
// move to the Recycle Bin).

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { $, errorText, h, toast } from "./dom";
import { type MeetingUsage, usageLines } from "./usage";

// ---- Types mirrored from app/src-tauri/src/meetings.rs and crates/pipeline ----

type Status = "recording" | "finalizing" | "done" | "no_final" | "interrupted";
type Source = "spoken" | "screen" | "both";

interface MeetingItem {
  id: string;
  dir: string;
  started_at_unix_ms: number | null;
  duration_ms: number;
  title: string | null;
  lines: number;
  keyframes: number;
  sections: number;
  status: Status;
}

interface Item {
  text: string;
  source: Source;
}

interface FinalNotes {
  title: string;
  summary: string;
  decisions: Item[];
  actions: (Item & { owner: string })[];
  topics: {
    heading: string;
    start_ms: number;
    end_ms: number;
    bullets: Item[];
    image: { t_ms: number; keyframe: string; file: string } | null;
  }[];
  model: string | null;
  note: string | null;
}

interface MeetingDetail {
  item: MeetingItem;
  final_notes: FinalNotes | null;
  sections: { start_ms: number; end_ms: number; heading: string; bullets: (Item & { kind: string })[] }[];
  transcript: { speaker: "me" | "others"; start_ms: number; text: string }[];
  keyframes: { t_ms: number; file: string; title: string }[];
  usage: MeetingUsage | null;
}

type FinalEvent =
  | { type: "started"; id: string }
  | { type: "done"; id: string; title: string; note: string | null }
  | { type: "failed"; id: string; message: string };

// ---- State ----

type Pane = "final" | "live" | "transcript";

let items: MeetingItem[] = [];
let detail: MeetingDetail | null = null;
let openId: string | null = null;
let pane: Pane = "final";
let confirmingDelete = false;
let busy = false;
const images = new Map<string, string>();

const STATUS_LABEL: Record<Status, string> = {
  recording: "Recording",
  finalizing: "Writing final notes…",
  done: "Final notes",
  no_final: "No final notes",
  interrupted: "Interrupted",
};

const SOURCE_LABEL: Record<Source, string | null> = {
  spoken: null,
  screen: "on screen",
  both: "said and shown",
};

function clock(ms: number): string {
  const s = Math.floor(ms / 1000);
  const hh = Math.floor(s / 3600);
  const mm = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  return hh > 0 ? `${hh}:${mm}:${ss}` : `${mm}:${ss}`;
}

function duration(ms: number): string {
  const minutes = Math.round(ms / 60000);
  if (minutes < 1) return `${Math.round(ms / 1000)} s`;
  if (minutes < 60) return `${minutes} min`;
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

function when(item: MeetingItem): string {
  if (item.started_at_unix_ms === null) return "Unknown date";
  return new Date(item.started_at_unix_ms).toLocaleString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function badge(status: Status): HTMLElement {
  return h("span", { class: `status-badge ${status}` }, STATUS_LABEL[status]);
}

function sourceTag(source: Source): HTMLElement | null {
  const label = SOURCE_LABEL[source];
  return label ? h("span", { class: "source" }, label) : null;
}

// ---- Rendering ----

function render() {
  const root = $("meetings");
  if (openId && detail) root.replaceChildren(detailView(detail));
  else root.replaceChildren(listView());
}

function listView(): HTMLElement {
  if (items.length === 0) {
    return h(
      "div",
      { class: "card empty" },
      "No meetings yet. Meetings you record appear here with their final notes.",
    );
  }
  return h(
    "div",
    { class: "card meeting-list" },
    ...items.map((m) =>
      h(
        "button",
        { class: "meeting-row", type: "button", onclick: () => open(m.id) },
        h(
          "div",
          { class: "meeting-row-main" },
          h("div", { class: "meeting-row-title" }, m.title ?? "Untitled meeting"),
          h(
            "div",
            { class: "fineprint tight" },
            `${when(m)} · ${duration(m.duration_ms)} · ${m.lines} lines · ${m.keyframes} screens`,
          ),
        ),
        badge(m.status),
      ),
    ),
  );
}

function detailView(d: MeetingDetail): HTMLElement {
  const m = d.item;
  const locked = m.status === "recording" || m.status === "finalizing";
  const actions = confirmingDelete
    ? [
        h("span", { class: "confirm-text" }, "Move this meeting, its audio and images to the Recycle Bin?"),
        h("button", { class: "btn danger", type: "button", disabled: busy, onclick: remove }, "Move to Recycle Bin"),
        h("button", { class: "btn", type: "button", onclick: () => ((confirmingDelete = false), render()) }, "Cancel"),
      ]
    : [
        h("button", { class: "btn", type: "button", onclick: openFolder }, "Open folder"),
        h(
          "button",
          { class: "btn", type: "button", disabled: busy || locked, onclick: finalize },
          d.final_notes ? "Rewrite final notes" : "Write final notes",
        ),
        h(
          "button",
          { class: "btn danger-text", type: "button", disabled: locked, onclick: () => ((confirmingDelete = true), render()) },
          "Delete",
        ),
      ];

  const panes: [Pane, string][] = [
    ["final", "Final notes"],
    ["live", `Live notes · ${d.sections.length}`],
    ["transcript", `Transcript · ${d.transcript.length}`],
  ];

  return h(
    "div",
    { class: "meeting-detail" },
    h(
      "a",
      { href: "#", class: "back", onclick: (e: Event) => (e.preventDefault(), close()) },
      "← All meeting notes",
    ),
    h(
      "div",
      { class: "detail-head" },
      h("h2", { class: "detail-title" }, d.final_notes?.title || m.title || "Untitled meeting"),
      h("div", { class: "fineprint tight" }, `${when(m)} · ${duration(m.duration_ms)} · `, badge(m.status)),
      usageView(d.usage),
    ),
    h("div", { class: "detail-actions" }, ...actions),
    h(
      "div",
      { class: "seg", role: "tablist", "aria-label": "Meeting content" },
      ...panes.map(([id, label]) =>
        h(
          "button",
          {
            class: "seg-btn",
            type: "button",
            role: "tab",
            "aria-selected": String(pane === id),
            onclick: () => ((pane = id), render()),
          },
          label,
        ),
      ),
    ),
    pane === "final" ? finalView(d) : pane === "live" ? liveView(d) : transcriptView(d),
  );
}

function usageView(u: MeetingUsage | null): HTMLElement | null {
  const lines = usageLines(u);
  if (lines.length === 0) return null;
  return h(
    "details",
    { class: "usage" },
    h("summary", {}, "Usage"),
    h("ul", {}, ...lines.map((l) => h("li", {}, h("strong", {}, `${l.role}: `), l.text))),
  );
}

function finalView(d: MeetingDetail): HTMLElement {
  const n = d.final_notes;
  if (!n) {
    const text =
      d.item.status === "finalizing"
        ? "The final notes are being written…"
        : d.item.status === "recording"
          ? "Final notes are written when the meeting stops."
          : d.item.status === "interrupted"
            ? "This meeting was cut off because the app closed or crashed. Its recording, transcript and " +
              "screens are kept. “Write final notes” also writes the notes for the part after the last live section."
            : "This meeting has no final notes yet. Use “Write final notes” above.";
    return h("div", { class: "card empty" }, text);
  }
  const blocks: (HTMLElement | null)[] = [];
  if (n.note) blocks.push(h("div", { class: "result warn" }, n.note));
  if (n.summary) blocks.push(h("p", { class: "final-summary" }, n.summary));
  if (n.decisions.length) {
    blocks.push(
      h("h3", { class: "final-h" }, "Decisions"),
      h("ul", { class: "note-bullets" }, ...n.decisions.map((i) => h("li", {}, i.text, sourceTag(i.source)))),
    );
  }
  if (n.actions.length) {
    blocks.push(
      h("h3", { class: "final-h" }, "Action items"),
      h(
        "ul",
        { class: "note-bullets actions" },
        ...n.actions.map((a) =>
          h("li", {}, a.text, a.owner ? h("span", { class: "owner" }, ` — ${a.owner}`) : null, sourceTag(a.source)),
        ),
      ),
    );
  }
  const head = h("div", { class: "card final-head" }, ...blocks);
  const topics = n.topics.map((t) => {
    const img = h("img", { class: "note-image", alt: `Screen at ${t.image ? clock(t.image.t_ms) : ""}` });
    if (t.image) void loadImage(d.item.id, t.image.file || t.image.keyframe, img);
    return h(
      "div",
      { class: "card note" },
      h("div", { class: "note-time mono" }, `${clock(t.start_ms)} – ${clock(t.end_ms)}`),
      h("h3", { class: "note-heading" }, t.heading),
      h(
        "div",
        { class: t.image ? "note-body with-image" : "note-body" },
        h("ul", { class: "note-bullets" }, ...t.bullets.map((b) => h("li", {}, b.text, sourceTag(b.source)))),
        t.image ? img : null,
      ),
    );
  });
  const footer = h(
    "p",
    { class: "fineprint" },
    `Saved as notes.md with an images folder in ${d.item.dir}` + (n.model ? ` · written by ${n.model}` : ""),
  );
  return h("div", { class: "notes" }, blocks.length ? head : null, ...topics, footer);
}

function liveView(d: MeetingDetail): HTMLElement {
  if (d.sections.length === 0) return h("div", { class: "card empty" }, "No live notes were written.");
  return h(
    "div",
    { class: "notes" },
    ...d.sections.map((s) =>
      h(
        "div",
        { class: "card note" },
        h("div", { class: "note-time mono" }, `${clock(s.start_ms)} – ${clock(s.end_ms)}`),
        h("h3", { class: "note-heading" }, s.heading),
        h(
          "ul",
          { class: "note-bullets" },
          ...s.bullets.map((b) =>
            h(
              "li",
              {},
              b.kind === "decision" ? h("span", { class: "tag decision" }, "Decision") : null,
              b.kind === "action" ? h("span", { class: "tag action" }, "Action") : null,
              b.text,
              sourceTag(b.source),
            ),
          ),
        ),
      ),
    ),
  );
}

function transcriptView(d: MeetingDetail): HTMLElement {
  const entries: { t: number; el: HTMLElement }[] = [
    ...d.transcript.map((l) => ({
      t: l.start_ms,
      el: h(
        "div",
        { class: "line done" },
        h("span", { class: "line-time mono" }, clock(l.start_ms)),
        h("span", { class: `speaker ${l.speaker}` }, l.speaker === "me" ? "Me" : "Others"),
        h("span", { class: "line-text" }, l.text),
      ),
    })),
    ...d.keyframes.map((k) => ({
      t: k.t_ms,
      el: h(
        "div",
        { class: "line shot read" },
        h("span", { class: "line-time mono" }, clock(k.t_ms)),
        h("span", { class: "speaker screen" }, "Screen"),
        h("span", { class: "line-text shot-note" }, k.title || "Screen changed"),
      ),
    })),
  ].sort((a, b) => a.t - b.t);
  if (entries.length === 0) return h("div", { class: "card empty" }, "Nothing was transcribed.");
  return h("div", { class: "card transcript" }, ...entries.map((e) => e.el));
}

async function loadImage(id: string, file: string, img: HTMLImageElement) {
  const key = `${id}/${file}`;
  let src = images.get(key);
  if (!src) {
    try {
      src = await invoke<string>("meeting_image", { id, file, width: 640 });
      images.set(key, src);
    } catch {
      img.remove();
      return;
    }
  }
  img.src = src;
}

// ---- Actions ----

async function refreshList() {
  try {
    items = await invoke<MeetingItem[]>("list_meetings");
  } catch (e) {
    toast(`Could not list meetings: ${errorText(e)}`, true);
  }
}

async function load(id: string) {
  try {
    detail = await invoke<MeetingDetail>("get_meeting", { id });
  } catch (e) {
    toast(errorText(e), true);
    openId = null;
    detail = null;
    await refreshList();
  }
}

async function open(id: string) {
  openId = id;
  pane = "final";
  confirmingDelete = false;
  await load(id);
  render();
  window.scrollTo({ top: 0 });
}

async function close() {
  openId = null;
  detail = null;
  confirmingDelete = false;
  await refreshList();
  render();
}

async function openFolder() {
  if (!openId) return;
  try {
    await invoke("open_meeting_folder", { id: openId });
  } catch (e) {
    toast(errorText(e), true);
  }
}

async function finalize() {
  if (!openId) return;
  busy = true;
  render();
  try {
    await invoke("finalize_meeting", { id: openId });
  } catch (e) {
    toast(errorText(e), true);
  } finally {
    busy = false;
  }
  // `meeting-final` events reload the meeting.
}

async function remove() {
  if (!openId) return;
  busy = true;
  render();
  try {
    await invoke("delete_meeting", { id: openId });
    toast("Moved to the Recycle Bin");
    for (const key of [...images.keys()]) if (key.startsWith(`${openId}/`)) images.delete(key);
    await close();
  } catch (e) {
    toast(errorText(e), true);
    confirmingDelete = false;
    render();
  } finally {
    busy = false;
  }
}

async function onFinal(e: FinalEvent) {
  if (e.type === "done") {
    toast(e.note ? `Final notes saved (live notes only): ${e.title}` : `Final notes ready: ${e.title}`);
  } else if (e.type === "failed") {
    toast(`Could not write the final notes: ${e.message}`, true);
  }
  for (const key of [...images.keys()]) if (key.startsWith(`${e.id}/images/`)) images.delete(key);
  if ($("view-meetings").hidden) return;
  if (openId === e.id) await load(e.id);
  else await refreshList();
  render();
}

// ---- Entry points ----

/// Shows the list, or the meeting `id` when given.
export async function showMeetings(id?: string) {
  if (id) {
    await open(id);
    return;
  }
  if (openId) await load(openId);
  else await refreshList();
  render();
}

export async function initMeetings() {
  await listen<FinalEvent>("meeting-final", (e) => void onFinal(e.payload));
  await listen("meeting-finished", () => {
    if (!$("view-meetings").hidden) void showMeetings();
  });
}
