// Settings > Recording: which microphone and display to capture, and how the
// screen is turned into keyframes and read.

import { invoke } from "@tauri-apps/api/core";
import { $, errorText, h, toast } from "./dom";

type AudioDevice = { name: string; is_default: boolean };
type Monitor = { index: number; name: string; width: number; height: number; is_primary: boolean };
type DisplayChoice = { name: string; index: number };
type CaptureSettings = {
  microphone: string | null;
  display: DisplayChoice | null;
  record_screen: boolean;
  screen_mode: "auto" | "slides";
  screen_read_cap: number | null;
};

const READ_CAPS: (number | null)[] = [20, 60, 120, null];
type DevicesView = {
  microphones: AudioDevice[];
  displays: Monitor[];
  settings: CaptureSettings;
  display_index: number | null;
};

let view: DevicesView | null = null;
let busy = false;

function row(name: string, desc: string, select: HTMLSelectElement, warning: string | null) {
  return h(
    "div",
    { class: "role" },
    h(
      "div",
      {},
      h("div", { class: "role-name" }, name),
      h("div", { class: "role-desc" }, desc),
      warning ? h("div", { class: "role-warn" }, warning) : null,
    ),
    select,
  );
}

function displayLabel(m: Monitor, all: Monitor[]): string {
  const twin = all.filter((o) => o.name === m.name).length > 1;
  const name = m.name || `Display ${m.index + 1}`;
  return `${name}${twin ? ` (${m.index + 1})` : ""} · ${m.width}×${m.height}`;
}

function render() {
  const container = $("devices");
  if (!view) return;
  const { microphones, displays, settings } = view;

  // Microphone: "" is the system default.
  const defaultMic = microphones.find((d) => d.is_default);
  const mic = settings.microphone;
  const micMissing = mic !== null && !microphones.some((d) => d.name === mic);
  const micSelect = h(
    "select",
    {
      "aria-label": "Microphone",
      disabled: busy,
      onchange: (e: Event) => {
        const value = (e.target as HTMLSelectElement).value;
        save({ ...settings, microphone: value || null });
      },
    },
    h("option", { value: "" }, `System default${defaultMic ? ` (${defaultMic.name})` : ""}`),
    ...microphones.map((d) => h("option", { value: d.name, selected: d.name === mic }, d.name)),
    micMissing ? h("option", { value: mic, selected: true }, `${mic} (not connected)`) : null,
  );

  // Display: "" is the primary display, "off" is no screen capture,
  // otherwise the monitor's index.
  const primary = displays.find((m) => m.is_primary);
  const chosen = settings.display;
  const screenOn = settings.record_screen;
  const displayMissing = screenOn && chosen !== null && view.display_index === null;
  const displaySelect = h(
    "select",
    {
      "aria-label": "Display",
      disabled: busy,
      onchange: (e: Event) => {
        const value = (e.target as HTMLSelectElement).value;
        if (value === "missing") return;
        if (value === "off") {
          save({ ...settings, record_screen: false });
          return;
        }
        const m = displays.find((d) => String(d.index) === value);
        save({ ...settings, record_screen: true, display: m ? { name: m.name, index: m.index } : null });
      },
    },
    h(
      "option",
      { value: "", selected: screenOn && chosen === null },
      `Primary display${primary ? ` (${displayLabel(primary, displays)})` : ""}`,
    ),
    ...displays.map((m) =>
      h(
        "option",
        { value: String(m.index), selected: screenOn && chosen !== null && m.index === view!.display_index },
        displayLabel(m, displays),
      ),
    ),
    displayMissing ? h("option", { value: "missing", selected: true }, `${chosen!.name} (not connected)`) : null,
    h("option", { value: "off", selected: !screenOn }, "Don’t capture the screen"),
  );

  const modeSelect = h(
    "select",
    {
      "aria-label": "Screen content",
      disabled: busy || !screenOn,
      onchange: (e: Event) =>
        save({ ...settings, screen_mode: (e.target as HTMLSelectElement).value as CaptureSettings["screen_mode"] }),
    },
    h("option", { value: "auto", selected: settings.screen_mode === "auto" }, "Slides and demos"),
    h("option", { value: "slides", selected: settings.screen_mode === "slides" }, "Slides only"),
  );

  const capSelect = h(
    "select",
    {
      "aria-label": "Screen reads per meeting",
      disabled: busy || !screenOn,
      onchange: (e: Event) => {
        const value = (e.target as HTMLSelectElement).value;
        save({ ...settings, screen_read_cap: value === "none" ? null : Number(value) });
      },
    },
    ...READ_CAPS.map((cap) =>
      h(
        "option",
        { value: cap === null ? "none" : String(cap), selected: settings.screen_read_cap === cap },
        cap === null ? "No limit" : `Up to ${cap}`,
      ),
    ),
    // A limit set by hand in capture.json.
    !READ_CAPS.includes(settings.screen_read_cap)
      ? h("option", { value: String(settings.screen_read_cap), selected: true }, `Up to ${settings.screen_read_cap}`)
      : null,
  );

  container.replaceChildren(
    row(
      "Microphone",
      "Recorded as “Me”.",
      micSelect,
      micMissing ? "This microphone is not connected. Meetings will not start until you plug it in or choose another." : null,
    ),
    row(
      "Display",
      "The screen whose slides and demos go into the notes.",
      displaySelect,
      displayMissing ? "This display is not connected. Meetings will not start until you choose another." : null,
    ),
    row(
      "Screen content",
      "“Slides only” keeps a screen once it holds still. “Slides and demos” also takes a sample of a moving screen every 10 seconds or so.",
      modeSelect,
      null,
    ),
    row(
      "Screen reads per meeting",
      "Each new screen is sent to the screen-reading model. Screens past the limit are still saved, just not read.",
      capSelect,
      null,
    ),
  );
}

async function save(settings: CaptureSettings) {
  busy = true;
  render();
  try {
    view = await invoke<DevicesView>("set_capture_devices", { settings });
    toast("Saved");
  } catch (e) {
    toast(`Could not save: ${errorText(e)}`, true);
  } finally {
    busy = false;
    render();
  }
}

export async function refreshDevices() {
  try {
    view = await invoke<DevicesView>("get_devices");
    render();
  } catch (e) {
    $("devices").replaceChildren(h("div", { class: "result err" }, `Could not list devices: ${errorText(e)}`));
  }
}

export function initDevices() {
  $("refresh-devices").addEventListener("click", () => {
    refreshDevices().then(() => toast("Device list updated"));
  });
  return refreshDevices();
}
