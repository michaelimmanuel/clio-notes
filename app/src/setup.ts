// First-run setup: walks a new user from an empty install to a working setup.
//
// 1. Pick a provider and paste a key; the key is checked by listing the
//    models it can use (free).
// 2. Pick a model for each job that provider can do. One profile is saved
//    per model, with the key in Windows Credential Manager, and assigned.
//    Anthropic cannot transcribe audio, so step 1 repeats for speech.
// 3. Done: what each job uses, and a reminder about consent.
//
// Shown at start while no profile exists, and from Settings > Jobs.

import { invoke } from "@tauri-apps/api/core";
import { $, errorText, h } from "./dom";

type Kind = "anthropic" | "openai_compatible" | "openrouter";
type Role = "speech_to_text" | "vision" | "text";

interface SettingsView {
  profiles: { id: string; name: string; model: string; kind: Kind }[];
  roles: Record<Role, string | null>;
}

interface Provider {
  id: string;
  label: string;
  kind: Kind;
  base_url: string | null;
  keyHint: string;
  /** Jobs it can do. */
  roles: Role[];
  /** Models to pick first, by job, when the key can use them. */
  prefer: Partial<Record<Role, string[]>>;
  keyOptional?: boolean;
}

const PROVIDERS: Provider[] = [
  {
    id: "openai",
    label: "OpenAI",
    kind: "openai_compatible",
    base_url: null,
    keyHint: "From platform.openai.com → API keys. One key covers all three jobs.",
    roles: ["speech_to_text", "vision", "text"],
    prefer: { speech_to_text: ["gpt-transcribe", "whisper-1"] },
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    kind: "openrouter",
    base_url: null,
    keyHint: "From openrouter.ai/keys. One key reaches models from many providers.",
    roles: ["speech_to_text", "vision", "text"],
    prefer: { speech_to_text: ["openai/gpt-transcribe", "openai/whisper-large-v3"] },
  },
  {
    id: "anthropic",
    label: "Anthropic",
    kind: "anthropic",
    base_url: null,
    keyHint: "From console.anthropic.com → API keys. Claude reads screens and writes notes; speech needs a second provider.",
    roles: ["vision", "text"],
    prefer: { vision: ["claude-opus-5-5", "claude-sonnet-5-5"], text: ["claude-opus-5-5", "claude-sonnet-5-5"] },
  },
  {
    id: "other",
    label: "Other (OpenAI-compatible)",
    kind: "openai_compatible",
    base_url: "",
    keyHint: "Any server with the OpenAI API, such as Groq, Gemini’s compatible endpoint or a local Ollama.",
    roles: ["speech_to_text", "vision", "text"],
    prefer: {},
    keyOptional: true,
  },
];

const JOB: Record<Role, { name: string; desc: string }> = {
  speech_to_text: { name: "Speech to text", desc: "Turns the meeting audio into text. Needs a transcription model." },
  vision: { name: "Screen reading", desc: "Reads slides and shared screens. The model must accept images." },
  text: { name: "Text generation", desc: "Writes the notes and the final summary." },
};

const SPEECH = /transcrib|whisper|speech|stt|audio-to-text/i;
const NOT_CHAT = /embed|tts|dall-e|image-gen|moderation|realtime|search|rerank|davinci|babbage/i;

type Step =
  | { kind: "key"; message?: string }
  | { kind: "models"; provider: Provider; key: string; baseUrl: string | null; models: string[] }
  | { kind: "done" };

let step: Step = { kind: "key" };
let settings: SettingsView | null = null;
let error: string | null = null;
let busy = false;
let chosen: Provider = PROVIDERS[0];

function missing(): Role[] {
  if (!settings) return ["speech_to_text", "vision", "text"];
  return (Object.keys(JOB) as Role[]).filter((r) => !settings!.roles[r]);
}

function suggest(provider: Provider, role: Role, models: string[]): string {
  for (const m of provider.prefer[role] ?? []) if (models.includes(m)) return m;
  const fitting = models.filter((m) => (role === "speech_to_text" ? SPEECH.test(m) : !SPEECH.test(m) && !NOT_CHAT.test(m)));
  return fitting[0] ?? "";
}

// ---- Rendering ----

function render() {
  const root = $("setup");
  const total = h("div", { class: "setup-steps" }, ...["Add a key", "Pick models", "Ready"].map((label, i) => {
    const current = step.kind === "key" ? 0 : step.kind === "models" ? 1 : 2;
    return h("span", { class: `setup-step${i === current ? " current" : i < current ? " past" : ""}` }, `${i + 1}. ${label}`);
  }));
  const body = step.kind === "key" ? keyStep(step.message) : step.kind === "models" ? modelStep(step) : doneStep();
  root.replaceChildren(total, body);
}

function keyStep(message?: string): HTMLElement {
  const need = missing();
  const usable = PROVIDERS.filter((p) => p.roles.some((r) => need.includes(r)));
  if (!usable.includes(chosen)) chosen = usable[0];

  const key = h("input", { type: "password", autocomplete: "off", spellcheck: "false", placeholder: "Paste your API key" });
  const baseUrl = h("input", {
    autocomplete: "off",
    spellcheck: "false",
    placeholder: "https://api.groq.com/openai/v1",
    value: "",
  });
  const hint = h("div", { class: "hint" }, chosen.keyHint);
  const urlField = h("div", { class: "field" }, h("label", {}, "Base URL"), baseUrl);
  urlField.hidden = chosen.base_url === null;

  const choices = h(
    "div",
    { class: "provider-choices", role: "radiogroup", "aria-label": "Provider" },
    ...usable.map((p) =>
      h(
        "button",
        {
          type: "button",
          class: "provider-choice",
          role: "radio",
          "aria-checked": String(p === chosen),
          onclick: () => {
            chosen = p;
            error = null;
            render();
          },
        },
        h("strong", {}, p.label),
        h("span", {}, p.roles.map((r) => JOB[r].name).join(" · ")),
      ),
    ),
  );

  const next = async (ev: Event) => {
    ev.preventDefault();
    const k = key.value.trim();
    const url = chosen.base_url === null ? null : baseUrl.value.trim();
    if (!k && !chosen.keyOptional) return showError("Paste the API key first.");
    if (url !== null && !url) return showError("Enter the server’s base URL.");
    busy = true;
    error = null;
    render();
    try {
      const models = await invoke<string[]>("list_models", {
        input: draft(chosen, url, "(none)"),
        key: k || null,
      });
      models.sort();
      step = { kind: "models", provider: chosen, key: k, baseUrl: url, models };
    } catch (e) {
      error = errorText(e);
    }
    busy = false;
    render();
  };

  return h(
    "form",
    { class: "card setup-card", onsubmit: next },
    h("h2", {}, settings && settings.profiles.length > 0 ? "Add another provider" : "Welcome to Clio Notes"),
    h(
      "p",
      {},
      message ??
        "Clio Notes runs on your own AI provider key. Pick a provider and paste a key; it is kept in " +
          "Windows Credential Manager and only sent to that provider.",
    ),
    choices,
    urlField,
    h("div", { class: "field" }, h("label", {}, `${chosen.label} API key${chosen.keyOptional ? " (if needed)" : ""}`), key, hint),
    error ? h("div", { class: "result err" }, error) : null,
    h(
      "div",
      { class: "detail-actions" },
      h("button", { class: "btn primary", type: "submit", disabled: busy }, busy ? "Checking the key…" : "Check key"),
      h("button", { class: "btn", type: "button", onclick: finish }, settings && settings.profiles.length > 0 ? "Skip" : "Set up later in Settings"),
    ),
  );
}

function modelStep(s: Extract<Step, { kind: "models" }>): HTMLElement {
  const roles = s.provider.roles.filter((r) => missing().includes(r));
  const list = h("datalist", { id: "setup-models" }, ...s.models.map((m) => h("option", { value: m })));
  const inputs = new Map<Role, HTMLInputElement>();
  const rows = roles.map((role) => {
    const input = h("input", {
      list: "setup-models",
      autocomplete: "off",
      spellcheck: "false",
      value: suggest(s.provider, role, s.models),
      placeholder: "Leave empty to skip this job",
    });
    inputs.set(role, input);
    return h(
      "div",
      { class: "role" },
      h("div", {}, h("div", { class: "role-name" }, JOB[role].name), h("div", { class: "role-desc" }, JOB[role].desc)),
      input,
    );
  });

  const save = async (ev: Event) => {
    ev.preventDefault();
    const picks = [...inputs.entries()].map(([role, i]) => [role, i.value.trim()] as const).filter(([, m]) => m);
    if (picks.length === 0) return showError("Pick a model for at least one job, or go back.");
    busy = true;
    error = null;
    render();
    try {
      // One profile per model; jobs that share a model share its profile.
      const byModel = new Map<string, Role[]>();
      for (const [role, model] of picks) byModel.set(model, [...(byModel.get(model) ?? []), role]);
      for (const [model, rolesFor] of byModel) {
        const before = new Set(settings?.profiles.map((p) => p.id));
        const name = `${s.provider.id === "other" ? hostOf(s.baseUrl) : s.provider.label} · ${model}`;
        settings = await invoke<SettingsView>("save_profile", {
          input: { ...draft(s.provider, s.baseUrl, model), name },
          key: s.key || null,
        });
        const created = settings.profiles.find((p) => !before.has(p.id));
        if (!created) continue;
        for (const role of rolesFor) {
          settings = await invoke<SettingsView>("assign_role", { role, profileId: created.id });
        }
      }
      window.dispatchEvent(new Event("settings-changed"));
      step = missing().includes("speech_to_text")
        ? {
            kind: "key",
            message:
              `${s.provider.label} cannot turn speech into text. Add a key for a provider that can, ` +
              "such as OpenAI or OpenRouter, or skip this and add it later in Settings.",
          }
        : { kind: "done" };
    } catch (e) {
      error = errorText(e);
    }
    busy = false;
    render();
  };

  return h(
    "form",
    { class: "card setup-card", onsubmit: save },
    h("h2", {}, "Pick a model for each job"),
    h(
      "p",
      {},
      `The key works: it can use ${s.models.length} models. Suggestions are filled in; click a field to see the ` +
        "whole list, or type a model name.",
    ),
    list,
    h("div", { class: "roles" }, ...rows),
    error ? h("div", { class: "result err" }, error) : null,
    h(
      "div",
      { class: "detail-actions" },
      h("button", { class: "btn primary", type: "submit", disabled: busy }, busy ? "Saving…" : "Save and continue"),
      h(
        "button",
        {
          class: "btn",
          type: "button",
          onclick: () => {
            step = { kind: "key" };
            error = null;
            render();
          },
        },
        "Back",
      ),
    ),
  );
}

function doneStep(): HTMLElement {
  const assigned = (Object.keys(JOB) as Role[]).map((role) => {
    const p = settings?.profiles.find((p) => p.id === settings?.roles[role]);
    return h("li", {}, h("strong", {}, `${JOB[role].name}: `), p ? p.model : "not set (optional jobs can stay off)");
  });
  return h(
    "div",
    { class: "card setup-card" },
    h("h2", {}, "You’re ready"),
    h("ul", { class: "setup-summary" }, ...assigned),
    h(
      "p",
      {},
      "Clio Notes records your default microphone as “Me”, your computer’s sound as “Others”, and your main " +
        "display. You can change these in Settings → Recording.",
    ),
    h(
      "p",
      { class: "fineprint" },
      "Before each meeting, tell everyone that you are recording it. You will get a short reminder when you start.",
    ),
    h("div", { class: "detail-actions" }, h("button", { class: "btn primary", type: "button", onclick: finish }, "Go to my first meeting")),
  );
}

// ---- Helpers ----

function draft(p: Provider, baseUrl: string | null, model: string) {
  return {
    id: null,
    name: p.label,
    kind: p.kind,
    base_url: baseUrl || null,
    model,
    effort: null,
    legacy_max_tokens: false,
  };
}

function hostOf(url: string | null): string {
  try {
    return url ? new URL(url).host : "Server";
  } catch {
    return "Server";
  }
}

function showError(text: string) {
  error = text;
  render();
}

function finish() {
  window.dispatchEvent(new Event("settings-changed"));
  window.dispatchEvent(new CustomEvent("setup-finished"));
}

/** Opens the setup from its first step. */
export async function showSetup() {
  step = { kind: "key" };
  error = null;
  busy = false;
  try {
    settings = await invoke<SettingsView>("get_settings");
  } catch {
    settings = null;
  }
  render();
}

/** Whether a new user should see the setup: nothing is configured yet. */
export async function needsSetup(): Promise<boolean> {
  try {
    const s = await invoke<SettingsView>("get_settings");
    return s.profiles.length === 0;
  } catch {
    return false;
  }
}
