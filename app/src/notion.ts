// Settings > Notion: connect an integration secret and choose the page that
// exported notes go under. The secret goes straight to the system key store
// (app/src-tauri/src/notion.rs) and is never shown again.

import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { $, errorText, h, toast } from "./dom";

interface NotionStatus {
  connected: boolean;
  workspace: string | null;
  parent_id: string | null;
  parent_title: string | null;
}

interface NotionPage {
  id: string;
  title: string;
  url: string;
}

let status: NotionStatus | null = null;
let picking = false;
let pages: NotionPage[] | null = null;
let busy = false;
let error: string | null = null;

function link(text: string, onclick: () => void): HTMLElement {
  return h(
    "a",
    {
      href: "#",
      onclick: (e: Event) => {
        e.preventDefault();
        onclick();
      },
    },
    text,
  );
}

function connectView(): HTMLElement {
  const secret = h("input", {
    type: "password",
    id: "notion-secret",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: "ntn_…",
  }) as HTMLInputElement;
  const connect = async () => {
    busy = true;
    error = null;
    render();
    try {
      status = await invoke<NotionStatus>("notion_connect", { secret: secret.value });
      secret.value = "";
      picking = true;
      await loadPages();
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
      render();
    }
  };
  secret.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void connect();
  });
  return h(
    "div",
    { class: "card notion" },
    h(
      "ol",
      { class: "steps" },
      h(
        "li",
        {},
        "Open ",
        link("notion.so/profile/integrations", () => void openUrl("https://www.notion.so/profile/integrations")),
        " and create an internal integration for your workspace.",
      ),
      h("li", {}, "Copy its secret and paste it below."),
      h(
        "li",
        {},
        "In Notion, open the page your meeting notes should go under, click ••• > Connections and add the integration.",
      ),
    ),
    h(
      "div",
      { class: "field" },
      h("label", { for: "notion-secret" }, "Integration secret"),
      h(
        "div",
        { class: "inline" },
        secret,
        h("button", { class: "btn primary", type: "button", disabled: busy, onclick: () => void connect() }, busy ? "Connecting…" : "Connect"),
      ),
    ),
    error ? h("div", { class: "result err" }, error) : null,
  );
}

async function loadPages(query = "") {
  try {
    pages = await invoke<NotionPage[]>("notion_pages", { query });
    error = null;
  } catch (e) {
    pages = [];
    error = errorText(e);
  }
}

function pickerView(): HTMLElement {
  const search = h("input", { type: "search", placeholder: "Search pages", "aria-label": "Search Notion pages" }) as HTMLInputElement;
  let timer: number | undefined;
  search.addEventListener("input", () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(async () => {
      await loadPages(search.value);
      renderList();
    }, 300);
  });
  const list = h("div", { class: "notion-pages", role: "listbox" });
  const renderList = () => {
    if (pages === null) {
      list.replaceChildren(h("p", { class: "fineprint" }, "Loading pages…"));
    } else if (pages.length === 0) {
      list.replaceChildren(
        h(
          "p",
          { class: "fineprint" },
          "No pages found. In Notion, add the integration to a page under ••• > Connections, then search again.",
        ),
      );
    } else {
      list.replaceChildren(
        ...pages.map((p) =>
          h(
            "button",
            {
              type: "button",
              role: "option",
              class: "notion-page",
              "aria-selected": String(p.id === status?.parent_id),
              onclick: async () => {
                try {
                  status = await invoke<NotionStatus>("notion_set_parent", { id: p.id, title: p.title });
                  picking = false;
                  toast(`Notes will be exported under “${p.title}”`);
                } catch (e) {
                  toast(errorText(e), true);
                }
                render();
              },
            },
            p.title,
          ),
        ),
      );
    }
  };
  renderList();
  return h(
    "div",
    { class: "notion-picker" },
    h("div", { class: "inline" }, search, status?.parent_id ? h("button", { class: "btn", type: "button", onclick: () => ((picking = false), render()) }, "Cancel") : null),
    list,
  );
}

function connectedView(): HTMLElement {
  return h(
    "div",
    { class: "card notion" },
    h(
      "div",
      { class: "row-between" },
      h("div", {}, h("strong", {}, "Connected"), status?.workspace ? ` to ${status.workspace}` : ""),
      h(
        "button",
        {
          class: "btn danger-text",
          type: "button",
          onclick: async () => {
            try {
              status = await invoke<NotionStatus>("notion_disconnect");
              picking = false;
              pages = null;
            } catch (e) {
              toast(errorText(e), true);
            }
            render();
          },
        },
        "Disconnect",
      ),
    ),
    picking || !status?.parent_id
      ? pickerView()
      : h(
          "p",
          { class: "notion-parent" },
          "Notes are exported as new pages under ",
          h("strong", {}, status.parent_title ?? "the chosen page"),
          ". ",
          link("Change", () => {
            picking = true;
            pages = null;
            render();
            void loadPages().then(render);
          }),
        ),
    error ? h("div", { class: "result err" }, error) : null,
  );
}

function render() {
  const slot = $("notion");
  if (!status) {
    slot.replaceChildren(h("p", { class: "fineprint" }, "Loading…"));
    return;
  }
  slot.replaceChildren(status.connected ? connectedView() : connectView());
}

export async function initNotion() {
  try {
    status = await invoke<NotionStatus>("notion_status");
    if (status.connected && !status.parent_id) {
      picking = true;
      void loadPages().then(render);
    }
  } catch (e) {
    error = errorText(e);
    status = { connected: false, workspace: null, parent_id: null, parent_title: null };
  }
  render();
}
