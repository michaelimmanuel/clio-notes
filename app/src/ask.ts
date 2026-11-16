// The Ask bar of a meeting: a bar floating at the bottom of a meeting's page
// for questions about that meeting, answered by the notes-writing model from
// the meeting's notes, screens and transcript. Focusing it opens a panel
// above it with the questions so far. Answers stream in as `meeting-answer`
// events; times like [12:34] in an answer jump to that point of the
// transcript.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { errorText, h, toast } from "./dom";

/** Mirrors ChatRow in crates/storage. */
interface ChatRow {
  id: number;
  question: string;
  answer: string;
  model: string;
  asked_at: number;
}

interface ChatState {
  rows: ChatRow[];
  loaded: boolean;
  /** The question being answered and the answer so far. */
  pending: { question: string; text: string } | null;
  /** The last question that failed, and why. */
  failed: { question: string; message: string } | null;
  draft: string;
  confirmingClear: boolean;
}

const SUGGESTIONS = [
  "What was decided?",
  "What are my action items?",
  "What questions were left open?",
  "Summarize this meeting in five points.",
];

const states = new Map<string, ChatState>();
let container: HTMLElement | null = null;
let shownId: string | null = null;
let shownLocked: string | null = null;
/** The panel above the bar is open. */
let expanded = false;
let jump: (ms: number) => void = () => {};

function state(id: string): ChatState {
  let s = states.get(id);
  if (!s) {
    s = { rows: [], loaded: false, pending: null, failed: null, draft: "", confirmingClear: false };
    states.set(id, s);
  }
  return s;
}

// ---- Answer text: paragraphs, bullet lists, **bold**, `code` and [times] ----

const TIME = String.raw`(?:\d{1,2}:)?\d{1,2}:\d{2}`;
const INLINE = new RegExp(
  String.raw`\*\*(.+?)\*\*|\x60([^\x60]+)\x60|\[(${TIME}(?:\s*(?:,|–|-|to)\s*${TIME})*)\]`,
  "g",
);

function parseTime(t: string): number {
  const parts = t.split(":").map(Number);
  let s = 0;
  for (const p of parts) s = s * 60 + p;
  return s * 1000;
}

function inline(text: string): (Node | string)[] {
  const out: (Node | string)[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    if (m.index! > last) out.push(text.slice(last, m.index));
    if (m[1] !== undefined) out.push(h("strong", {}, ...inline(m[1])));
    else if (m[2] !== undefined) out.push(h("code", {}, m[2]));
    else {
      const times = m[3].match(new RegExp(TIME, "g")) ?? [];
      const chips = times.map((t) =>
        h(
          "button",
          {
            class: "time-chip mono",
            type: "button",
            title: `Show ${t} in the transcript`,
            onclick: () => {
              collapse();
              jump(parseTime(t));
            },
          },
          t,
        ),
      );
      out.push(h("span", { class: "time-chips" }, ...chips));
    }
    last = m.index! + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

function answerBody(text: string): HTMLElement {
  const body = h("div", { class: "answer-body" });
  let list: HTMLElement | null = null;
  let para: string[] = [];
  const flushPara = () => {
    if (para.length) body.append(h("p", {}, ...inline(para.join(" "))));
    para = [];
  };
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    const bullet = line.match(/^(?:[-*•]|\d+[.)])\s+(.*)$/);
    if (bullet) {
      flushPara();
      const ordered = /^\d/.test(line);
      if (!list || (list.tagName === "OL") !== ordered) {
        list = h(ordered ? "ol" : "ul", {});
        body.append(list);
      }
      list.append(h("li", {}, ...inline(bullet[1])));
    } else if (line === "") {
      flushPara();
      list = null;
    } else {
      // Headings are not asked for; show one as a bold line if it comes.
      const heading = line.match(/^#{1,6}\s+(.*)$/);
      if (heading) {
        flushPara();
        list = null;
        body.append(h("p", {}, h("strong", {}, ...inline(heading[1]))));
      } else {
        list = null;
        para.push(line);
      }
    }
  }
  flushPara();
  return body;
}

// ---- View ----

function exchange(question: string, answer: HTMLElement, footer: HTMLElement | null): HTMLElement {
  return h(
    "div",
    { class: "exchange" },
    h("div", { class: "question" }, question),
    h("div", { class: "answer" }, answer, footer),
  );
}

function copyButton(text: string): HTMLElement {
  return h(
    "button",
    {
      class: "btn small ghost",
      type: "button",
      onclick: async () => {
        try {
          await navigator.clipboard.writeText(text);
          toast("Answer copied");
        } catch (e) {
          toast(errorText(e), true);
        }
      },
    },
    "Copy",
  );
}

function paint() {
  if (!container || !shownId) return;
  const id = shownId;
  const s = state(id);
  const busy = s.pending !== null || !s.loaded;
  const locked = shownLocked;

  const thread = h("div", { class: "thread" });
  for (const row of s.rows) {
    thread.append(
      exchange(
        row.question,
        answerBody(row.answer),
        h("div", { class: "answer-foot" }, h("span", { class: "fineprint tight" }, row.model), copyButton(row.answer)),
      ),
    );
  }
  if (s.pending) {
    const body = s.pending.text
      ? answerBody(s.pending.text)
      : h("div", { class: "answer-body thinking" }, h("span", { class: "dots" }), "Reading the meeting…");
    thread.append(exchange(s.pending.question, h("div", { class: "pending-answer" }, body), null));
  }
  if (s.failed) {
    const failed = s.failed;
    thread.append(
      exchange(
        failed.question,
        h(
          "div",
          { class: "answer-error" },
          h("p", {}, failed.message),
          h(
            "button",
            { class: "btn small", type: "button", disabled: busy || !!locked, onclick: () => void send(id, failed.question) },
            "Try again",
          ),
        ),
        null,
      ),
    );
  }

  const empty = s.loaded && s.rows.length === 0 && !s.pending && !s.failed;
  const suggestions = empty
    ? h(
        "div",
        { class: "suggestions" },
        h("p", { class: "fineprint tight" }, "Ask anything about this meeting, or start with:"),
        h(
          "div",
          { class: "chips" },
          ...SUGGESTIONS.map((q) =>
            h("button", { class: "btn small", type: "button", disabled: !!locked, onclick: () => void send(id, q) }, q),
          ),
        ),
      )
    : null;

  const box = h("textarea", {
    class: "ask-input",
    rows: 1,
    placeholder: locked ?? "Ask about this meeting…",
    "aria-label": "Ask about this meeting",
    disabled: !!locked,
    value: s.draft,
    onfocus: () => {
      if (!expanded) {
        expanded = true;
        paint();
      }
    },
    oninput: (e: Event) => {
      s.draft = (e.target as HTMLTextAreaElement).value;
      grow(box);
      button.disabled = busy || !s.draft.trim();
    },
    onkeydown: (e: KeyboardEvent) => {
      if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        if (!busy && s.draft.trim()) void send(id, s.draft);
      } else if (e.key === "Escape") {
        e.preventDefault();
        collapse();
        box.blur();
      }
    },
  }) as HTMLTextAreaElement;
  const button = h(
    "button",
    {
      class: "btn primary ask-send",
      type: "button",
      disabled: busy || !s.draft.trim() || !!locked,
      onclick: () => void send(id, s.draft),
    },
    s.pending ? "Answering…" : "Ask",
  ) as HTMLButtonElement;

  const clear = s.rows.length === 0
    ? null
    : s.confirmingClear
      ? h(
          "span",
          { class: "clear-confirm" },
          "Delete every question?",
          h("button", { class: "btn small danger", type: "button", onclick: () => void clearChat(id) }, "Delete"),
          h("button", { class: "btn small", type: "button", onclick: () => ((s.confirmingClear = false), paint()) }, "Cancel"),
        )
      : h(
          "button",
          { class: "btn small ghost", type: "button", disabled: busy, onclick: () => ((s.confirmingClear = true), paint()) },
          "Clear chat",
        );

  const panel = expanded
    ? h(
        "div",
        { class: "ask-panel", role: "region", "aria-label": "Questions about this meeting" },
        h(
          "div",
          { class: "ask-panel-head" },
          h("strong", {}, "Ask about this meeting"),
          h(
            "div",
            { class: "ask-head-actions" },
            clear,
            h("button", { class: "btn small ghost", type: "button", title: "Close (Esc)", onclick: collapse }, "Close"),
          ),
        ),
        h(
          "div",
          { class: "ask-scroll" },
          s.loaded ? thread : h("div", { class: "fineprint tight" }, "Loading…"),
          suggestions,
        ),
        h(
          "p",
          { class: "fineprint tight ask-note" },
          "Answers come only from this meeting, written by the notes-writing model in Settings. Click a time to see it in the transcript.",
        ),
      )
    : null;

  // Closed with earlier answers: a way back to them without typing.
  const reopen = !expanded && (s.rows.length > 0 || s.pending || s.failed)
    ? h(
        "button",
        { class: "btn small ghost ask-reopen", type: "button", onclick: expand },
        s.pending ? "Answering…" : s.failed ? "1 failed" : `${s.rows.length} answer${s.rows.length === 1 ? "" : "s"}`,
      )
    : null;

  const hadFocus = document.activeElement?.classList.contains("ask-input") ?? false;
  container.classList.toggle("open", expanded);
  container.replaceChildren(
    panel ?? "",
    h("div", { class: "ask-bar" }, h("span", { class: "ask-icon", "aria-hidden": "true" }, "✦"), box, reopen, button),
  );
  grow(box);
  const scroll = container.querySelector<HTMLElement>(".ask-scroll");
  if (scroll) scroll.scrollTop = scroll.scrollHeight;
  // Focus opens the panel, so it is given back only while the panel is open.
  if (hadFocus && !locked && expanded) {
    box.focus();
    box.setSelectionRange(box.value.length, box.value.length);
  }
}

/** Fits the question box to its text, up to a few lines. */
function grow(box: HTMLTextAreaElement) {
  box.style.height = "auto";
  box.style.height = `${Math.min(box.scrollHeight, 140)}px`;
}

function expand() {
  if (expanded) return;
  expanded = true;
  paint();
  container?.querySelector<HTMLTextAreaElement>(".ask-input")?.focus();
}

function collapse() {
  if (!expanded) return;
  expanded = false;
  if (shownId) state(shownId).confirmingClear = false;
  paint();
}

/** Repaints only the answer being written, so typing is not disturbed. */
function paintPending(id: string) {
  if (!container || shownId !== id) return;
  const s = state(id);
  const el = container.querySelector(".pending-answer");
  if (!el || !s.pending) return paint();
  const scroll = container.querySelector<HTMLElement>(".ask-scroll");
  // Follow the answer unless the reader scrolled up.
  const atBottom = !scroll || scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 40;
  el.replaceChildren(answerBody(s.pending.text));
  if (scroll && atBottom) scroll.scrollTop = scroll.scrollHeight;
}

async function loadHistory(id: string) {
  const s = state(id);
  try {
    s.rows = await invoke<ChatRow[]>("chat_history", { id });
  } catch (e) {
    toast(`Could not load the questions: ${errorText(e)}`, true);
  }
  s.loaded = true;
  if (shownId === id) paint();
}

async function send(id: string, question: string) {
  const s = state(id);
  question = question.trim();
  if (!question || s.pending || !s.loaded) return;
  s.pending = { question, text: "" };
  s.failed = null;
  s.draft = "";
  s.confirmingClear = false;
  if (shownId === id) expanded = true;
  paint();
  try {
    const row = await invoke<ChatRow>("ask_meeting", { id, question });
    s.rows.push(row);
  } catch (e) {
    s.failed = { question, message: errorText(e) };
  } finally {
    s.pending = null;
  }
  if (shownId === id) paint();
}

async function clearChat(id: string) {
  const s = state(id);
  try {
    await invoke("clear_chat", { id });
    s.rows = [];
    s.failed = null;
  } catch (e) {
    toast(errorText(e), true);
  }
  s.confirmingClear = false;
  paint();
}

/**
 * The Ask bar for meeting `id`, to place at the end of its page. `locked` is
 * the reason questions cannot be asked right now, if any. `onJump` shows a
 * time of the transcript.
 */
export function askView(id: string, locked: string | null, onJump: (ms: number) => void): HTMLElement {
  jump = onJump;
  container ??= h("div", { class: "ask-dock" });
  shownLocked = locked;
  if (shownId !== id) {
    shownId = id;
    expanded = false;
    const s = state(id);
    if (!s.loaded) void loadHistory(id);
  }
  paint();
  return container;
}

export async function initAsk() {
  // Clicking elsewhere on the page, or Esc, closes the panel.
  document.addEventListener("mousedown", (e) => {
    if (expanded && container?.isConnected && !container.contains(e.target as Node)) collapse();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && expanded && container?.isConnected) collapse();
  });
  await listen<{ id: string; text: string }>("meeting-answer", (e) => {
    const s = states.get(e.payload.id);
    if (!s?.pending) return;
    s.pending.text += e.payload.text;
    paintPending(e.payload.id);
  });
}
