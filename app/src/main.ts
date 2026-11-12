import { invoke } from "@tauri-apps/api/core";
import { $, errorText, h, toast } from "./dom";
import { initDevices } from "./devices";
import { initMeeting } from "./meeting";

// ---- Types mirrored from app/src-tauri/src/settings.rs ----

type Kind = "anthropic" | "openai_compatible" | "openrouter";
type Role = "speech_to_text" | "vision" | "text";

interface Profile {
  id: string;
  name: string;
  kind: Kind;
  base_url?: string;
  model: string;
  effort?: string;
  legacy_max_tokens?: boolean;
  has_key: boolean;
  requires_key: boolean;
  default_base_url: string;
}

interface SettingsView {
  profiles: Profile[];
  roles: Record<Role, string | null>;
  path: string;
}

interface ProfileInput {
  id: string | null;
  name: string;
  kind: Kind;
  base_url: string | null;
  model: string;
  effort: string | null;
  legacy_max_tokens: boolean;
}

interface CheckResult {
  model: string;
  display_name?: string | null;
}

const ROLES: { id: Role; name: string; desc: string }[] = [
  {
    id: "speech_to_text",
    name: "Speech to text",
    desc: "Turns meeting audio into text. Needs an OpenAI or OpenRouter profile with a transcription model, such as gpt-transcribe or openai/whisper-large-v3.",
  },
  {
    id: "vision",
    name: "Screen reading",
    desc: "Reads slides and shared screens. The model must accept images.",
  },
  {
    id: "text",
    name: "Text generation",
    desc: "Fixes terms in the transcript, writes the notes and the final summary.",
  },
];

const KIND_LABEL: Record<Kind, string> = {
  anthropic: "Anthropic",
  openai_compatible: "OpenAI-compatible",
  openrouter: "OpenRouter",
};

const DEFAULT_URL: Record<Kind, string> = {
  anthropic: "https://api.anthropic.com",
  openai_compatible: "https://api.openai.com/v1",
  openrouter: "https://openrouter.ai/api/v1",
};

interface Preset {
  label: string;
  name: string;
  kind: Kind;
  model: string;
  base_url?: string;
}

const PRESETS: Preset[] = [
  { label: "Anthropic", name: "Claude", kind: "anthropic", model: "claude-opus-5-5" },
  { label: "OpenAI speech", name: "OpenAI speech", kind: "openai_compatible", model: "gpt-transcribe" },
  { label: "OpenAI chat", name: "OpenAI", kind: "openai_compatible", model: "" },
  { label: "OpenRouter", name: "OpenRouter", kind: "openrouter", model: "" },
  {
    label: "Ollama (local)",
    name: "Ollama",
    kind: "openai_compatible",
    model: "",
    base_url: "http://localhost:11434/v1",
  },
];

// ---- State ----

let settings: SettingsView = { profiles: [], roles: { speech_to_text: null, vision: null, text: null }, path: "" };
/** Test results shown under each profile card, by profile id. */
const results = new Map<string, Result>();
/** Profile whose delete button has been pressed once. */
let armedDelete: string | null = null;
let formOpen = false;

type Result = { state: "ok" | "err" | "pending"; text: string };

function supports(p: { kind: Kind }, role: Role): boolean {
  return !(p.kind === "anthropic" && role === "speech_to_text");
}

function toInput(p: Profile): ProfileInput {
  return {
    id: p.id,
    name: p.name,
    kind: p.kind,
    base_url: p.base_url ?? null,
    model: p.model,
    effort: p.effort ?? null,
    legacy_max_tokens: p.legacy_max_tokens ?? false,
  };
}

function resultLine(r: Result | undefined): HTMLElement | null {
  return r ? h("div", { class: `result ${r.state}` }, r.text) : null;
}

function checkMessage(r: CheckResult): string {
  return `Connected. ${r.display_name ? `${r.display_name} (${r.model})` : r.model} is available to this key.`;
}

// ---- Rendering ----

function render() {
  renderRoles();
  renderProfiles();
  $("settings-path").textContent = settings.path ? `Settings file: ${settings.path}` : "";
}

function renderRoles() {
  const container = $("roles");
  container.replaceChildren(
    ...ROLES.map((role) => {
      const candidates = settings.profiles.filter((p) => supports(p, role.id));
      const current = settings.roles[role.id];
      const assigned = settings.profiles.find((p) => p.id === current);

      const select = h(
        "select",
        {
          "aria-label": `Profile for ${role.name}`,
          disabled: candidates.length === 0,
          onchange: (e: Event) => assignRole(role.id, (e.target as HTMLSelectElement).value || null),
        },
        h("option", { value: "" }, candidates.length ? "Not assigned" : "No suitable profile yet"),
        ...candidates.map((p) => h("option", { value: p.id, selected: p.id === current }, `${p.name} · ${p.model}`)),
      );

      let warning: string | null = null;
      if (assigned && assigned.requires_key && !assigned.has_key) {
        warning = `"${assigned.name}" has no API key saved.`;
      }

      return h(
        "div",
        { class: "role" },
        h(
          "div",
          {},
          h("div", { class: "role-name" }, role.name),
          h("div", { class: "role-desc" }, role.desc),
          warning ? h("div", { class: "role-warn" }, warning) : null,
        ),
        select,
      );
    }),
  );
}

function renderProfiles() {
  const container = $("profiles");
  if (settings.profiles.length === 0) {
    container.replaceChildren(
      formOpen
        ? ""
        : h("div", { class: "empty" }, "No profiles yet. Add one for each provider whose key you want to use."),
    );
    return;
  }
  container.replaceChildren(...settings.profiles.map(profileCard));
}

function profileCard(p: Profile): HTMLElement {
  const roles = ROLES.filter((r) => settings.roles[r.id] === p.id);
  const keyBadge = !p.requires_key
    ? h("span", { class: "badge" }, "No key needed")
    : p.has_key
      ? h("span", { class: "badge ok" }, "Key saved")
      : h("span", { class: "badge warn" }, "No key");

  const armed = armedDelete === p.id;
  return h(
    "article",
    { class: "card profile" },
    h(
      "div",
      { class: "profile-top" },
      h("span", { class: "profile-name" }, p.name),
      h("span", { class: "badge" }, KIND_LABEL[p.kind]),
      keyBadge,
      ...roles.map((r) => h("span", { class: "badge role-badge" }, r.name)),
      h(
        "div",
        { class: "profile-actions" },
        h("button", { class: "btn small", type: "button", onclick: () => testSaved(p) }, "Test"),
        h("button", { class: "btn small", type: "button", onclick: () => openForm(p) }, "Edit"),
        h(
          "button",
          {
            class: `btn small danger${armed ? " armed" : ""}`,
            type: "button",
            onclick: () => deleteProfile(p),
            onblur: () => {
              if (armedDelete === p.id) {
                armedDelete = null;
                renderProfiles();
              }
            },
          },
          armed ? "Confirm delete" : "Delete",
        ),
      ),
    ),
    h(
      "div",
      { class: "profile-meta" },
      h("span", { class: "mono" }, p.model),
      p.base_url ? h("span", { class: "mono" }, p.base_url) : null,
      p.effort ? h("span", {}, `Effort: ${p.effort}`) : null,
    ),
    resultLine(results.get(p.id)),
  );
}

// ---- Actions on saved profiles ----

async function assignRole(role: Role, profileId: string | null) {
  try {
    settings = await invoke<SettingsView>("assign_role", { role, profileId });
    render();
  } catch (e) {
    toast(errorText(e), true);
    render();
  }
}

async function testSaved(p: Profile) {
  results.set(p.id, { state: "pending", text: "Checking the key and model…" });
  renderProfiles();
  try {
    const r = await invoke<CheckResult>("test_profile", { input: toInput(p), key: null });
    results.set(p.id, { state: "ok", text: checkMessage(r) });
  } catch (e) {
    results.set(p.id, { state: "err", text: errorText(e) });
  }
  renderProfiles();
}

async function deleteProfile(p: Profile) {
  if (armedDelete !== p.id) {
    armedDelete = p.id;
    renderProfiles();
    // Keep focus on the same button so blur can disarm it.
    (document.querySelector(".btn.danger.armed") as HTMLElement | null)?.focus();
    return;
  }
  armedDelete = null;
  try {
    settings = await invoke<SettingsView>("delete_profile", { id: p.id });
    results.delete(p.id);
    toast(`Deleted "${p.name}" and its saved key.`);
  } catch (e) {
    toast(errorText(e), true);
  }
  render();
}

// ---- Profile form ----

function openForm(existing?: Profile) {
  formOpen = true;
  const slot = $("form-slot");
  const editing = existing ?? null;

  const name = h("input", { id: "f-name", value: editing?.name ?? "", autocomplete: "off", required: true });
  const kind = h(
    "select",
    { id: "f-kind" },
    ...(Object.keys(KIND_LABEL) as Kind[]).map((k) =>
      h("option", { value: k, selected: (editing?.kind ?? "anthropic") === k }, KIND_LABEL[k]),
    ),
  );
  const model = h("input", {
    id: "f-model",
    value: editing?.model ?? "",
    list: "f-model-options",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: "Type a model or load the list",
  });
  const modelOptions = h("datalist", { id: "f-model-options" });
  const key = h("input", {
    id: "f-key",
    type: "password",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: editing?.has_key ? "Saved. Leave blank to keep it." : "Paste your API key",
  });
  const baseUrl = h("input", {
    id: "f-base-url",
    value: editing?.base_url ?? "",
    autocomplete: "off",
    spellcheck: "false",
  });
  const effort = h(
    "select",
    { id: "f-effort" },
    ...["", "low", "medium", "high"].map((v) =>
      h("option", { value: v, selected: (editing?.effort ?? "") === v }, v === "" ? "Provider default" : v),
    ),
  );
  const legacy = h("input", { id: "f-legacy", type: "checkbox", checked: editing?.legacy_max_tokens ?? false });
  const legacyRow = h(
    "label",
    { class: "check" },
    legacy,
    "Server only accepts max_tokens (some older OpenAI-compatible servers)",
  );
  const keyHint = h("div", { class: "hint" });
  const result = h("div");

  const read = (): { input: ProfileInput; key: string | null } => ({
    input: {
      id: editing?.id ?? null,
      name: name.value.trim(),
      kind: kind.value as Kind,
      base_url: baseUrl.value.trim() || null,
      model: model.value.trim(),
      effort: effort.value || null,
      legacy_max_tokens: legacy.checked,
    },
    // A typed key lets Test and Load models work before the profile is saved.
    key: key.value.trim() || null,
  });

  const syncKind = () => {
    const k = kind.value as Kind;
    baseUrl.placeholder = DEFAULT_URL[k];
    legacyRow.style.display = k === "openai_compatible" ? "" : "none";
    keyHint.textContent = {
      anthropic: "From console.anthropic.com → API keys.",
      openrouter: "From openrouter.ai/keys. One key reaches models from many providers.",
      openai_compatible: "From your provider's dashboard. Not needed for local servers such as Ollama.",
    }[k];
  };
  kind.addEventListener("change", syncKind);
  syncKind();

  const setResult = (r: Result | undefined) => result.replaceChildren(resultLine(r) ?? "");

  const applyPreset = (p: Preset) => {
    name.value = p.name;
    kind.value = p.kind;
    model.value = p.model;
    baseUrl.value = p.base_url ?? "";
    syncKind();
    (p.model ? key : model).focus();
  };

  const test = async () => {
    const { input, key: k } = read();
    if (!input.model) return setResult({ state: "err", text: "Enter a model first." });
    setResult({ state: "pending", text: "Checking the key and model…" });
    try {
      setResult({ state: "ok", text: checkMessage(await invoke<CheckResult>("test_profile", { input, key: k })) });
    } catch (e) {
      setResult({ state: "err", text: errorText(e) });
    }
  };

  const loadModels = async () => {
    const { input, key: k } = read();
    // The model is only used for error messages when listing.
    input.model ||= "(none)";
    setResult({ state: "pending", text: "Loading the models this key can use…" });
    try {
      const ids = await invoke<string[]>("list_models", { input, key: k });
      modelOptions.replaceChildren(...ids.map((id) => h("option", { value: id })));
      setResult({
        state: "ok",
        text: `Found ${ids.length} models. Click the model field to pick one.`,
      });
      model.focus();
    } catch (e) {
      setResult({ state: "err", text: errorText(e) });
    }
  };

  const save = async (ev: Event) => {
    ev.preventDefault();
    const { input, key: k } = read();
    if (!input.name || !input.model) {
      return setResult({ state: "err", text: "Name and model are required." });
    }
    const before = new Set(settings.profiles.map((p) => p.id));
    try {
      settings = await invoke<SettingsView>("save_profile", { input, key: k });
    } catch (e) {
      return setResult({ state: "err", text: errorText(e) });
    }
    const created = settings.profiles.find((p) => !before.has(p.id));
    if (created) await autoAssign(created);
    closeForm();
    toast(`Saved "${input.name}".`);
  };

  const form = h(
    "form",
    { class: "card form", onsubmit: save },
    h("h3", { class: "form-title" }, editing ? `Edit "${editing.name}"` : "New profile"),
    editing
      ? null
      : h(
          "div",
          { class: "presets" },
          h("span", {}, "Start from:"),
          ...PRESETS.map((p) =>
            h("button", { class: "btn small", type: "button", onclick: () => applyPreset(p) }, p.label),
          ),
        ),
    h(
      "div",
      { class: "grid-2" },
      h("div", { class: "field" }, h("label", { for: "f-name" }, "Name"), name),
      h("div", { class: "field" }, h("label", { for: "f-kind" }, "Provider"), kind),
    ),
    h(
      "div",
      { class: "field" },
      h("label", { for: "f-model" }, "Model"),
      h(
        "div",
        { class: "inline" },
        model,
        h("button", { class: "btn", type: "button", onclick: loadModels }, "Load models"),
      ),
      modelOptions,
    ),
    h("div", { class: "field" }, h("label", { for: "f-key" }, "API key"), key, keyHint),
    h(
      "div",
      { class: "grid-2" },
      h(
        "div",
        { class: "field" },
        h("label", { for: "f-base-url" }, "Base URL (optional)"),
        baseUrl,
        h("div", { class: "hint" }, "Leave blank for the official endpoint."),
      ),
      h(
        "div",
        { class: "field" },
        h("label", { for: "f-effort" }, "Reasoning effort"),
        effort,
        h("div", { class: "hint" }, "Lower is faster and cheaper."),
      ),
    ),
    legacyRow,
    result,
    h(
      "div",
      { class: "form-actions" },
      h("button", { class: "btn", type: "button", onclick: test }, "Test connection"),
      h("span", { class: "spacer" }),
      h("button", { class: "btn", type: "button", onclick: closeForm }, "Cancel"),
      h("button", { class: "btn primary", type: "submit" }, editing ? "Save changes" : "Save profile"),
    ),
  );

  slot.replaceChildren(form);
  ($("add-profile") as HTMLButtonElement).disabled = true;
  renderProfiles();
  name.focus();
  form.scrollIntoView({ block: "nearest" });
}

function closeForm() {
  formOpen = false;
  $("form-slot").replaceChildren();
  ($("add-profile") as HTMLButtonElement).disabled = false;
  render();
}

/** Fills empty roles with a newly added profile, so a first setup needs no extra clicks. */
async function autoAssign(p: Profile) {
  const isSpeechModel = /transcribe|whisper/i.test(p.model);
  const targets: Role[] = isSpeechModel ? ["speech_to_text"] : ["vision", "text"];
  for (const role of targets) {
    if (!settings.roles[role] && supports(p, role)) {
      try {
        settings = await invoke<SettingsView>("assign_role", { role, profileId: p.id });
      } catch {
        // Leave it unassigned; the user can pick it from the Jobs list.
      }
    }
  }
}

// ---- Start ----

window.addEventListener("DOMContentLoaded", async () => {
  $("add-profile").addEventListener("click", () => openForm());
  // `npm run dev` serves this page to a normal browser, where the Rust side
  // does not exist. Say so instead of failing on the first command.
  if (!("__TAURI_INTERNALS__" in window)) {
    ($("add-profile") as HTMLButtonElement).disabled = true;
    const notice = () =>
      h(
        "div",
        { class: "result err" },
        "This page only works inside the Clio Notes app window, not in a web browser. " +
          "Stop this server, then run `npm run tauri dev` in the app folder.",
      );
    $("profiles").replaceChildren(notice());
    $("meeting-controls").replaceChildren(notice());
    $("devices").replaceChildren(notice());
    return;
  }
  initDevices();
  initMeeting().catch((e) => toast(`Could not set up the meeting view: ${errorText(e)}`, true));
  try {
    settings = await invoke<SettingsView>("get_settings");
    render();
  } catch (e) {
    $("profiles").replaceChildren(
      h("div", { class: "result err" }, `Could not load settings: ${errorText(e)}`),
    );
  }
});
