// Automatic updates from the GitHub releases (tauri-plugin-updater).
//
// The app checks once at startup and again every 6 hours. A new version shows
// a banner under the top bar; nothing is downloaded until the user clicks
// "Install and restart". Installing is refused while a meeting is recording or
// its final notes are being written, since restarting would cut them off.
// Updates are signed in the release workflow and checked against the public
// key in tauri.conf.json before they are installed.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { $, errorText, h, toast } from "./dom";
import { isRecording } from "./meeting";

const RECHECK_MS = 6 * 60 * 60 * 1000;

let pending: Update | null = null;
/** The version the user chose "Later" for; not shown again until a newer one. */
let dismissed: string | null = null;
let installing = false;

async function finalizing(): Promise<boolean> {
  try {
    const items = await invoke<{ status: string }[]>("list_meetings");
    return items.some((m) => m.status === "recording" || m.status === "finalizing");
  } catch {
    return false;
  }
}

function renderBanner(progress?: string) {
  const banner = $("update-banner");
  if (!pending || (pending.version === dismissed && !installing)) {
    banner.hidden = true;
    return;
  }
  banner.hidden = false;
  const notes = pending.body?.trim();
  const actions = installing
    ? []
    : [
        h(
          "div",
          { class: "update-actions" },
          h("button", { class: "btn primary small", type: "button", onclick: () => void install() }, "Install and restart"),
          h(
            "button",
            {
              class: "btn small",
              type: "button",
              onclick: () => {
                dismissed = pending?.version ?? null;
                renderBanner();
              },
            },
            "Later",
          ),
        ),
      ];
  banner.replaceChildren(
    h(
      "div",
      { class: "update-text" },
      h("strong", {}, `Clio Notes ${pending.version} is available.`),
      progress ? ` ${progress}` : notes ? ` ${firstLine(notes)}` : "",
    ),
    ...actions,
  );
}

function firstLine(text: string): string {
  const line = text.split("\n").find((l) => l.trim() && !l.startsWith("#")) ?? "";
  return line.length > 140 ? `${line.slice(0, 137)}…` : line;
}

async function install() {
  if (!pending || installing) return;
  if (isRecording() || (await finalizing())) {
    toast("Finish the meeting and wait for its final notes before updating.", true);
    return;
  }
  installing = true;
  let total = 0;
  let done = 0;
  renderBanner("Downloading…");
  try {
    await pending.downloadAndInstall((e) => {
      if (e.event === "Started") total = e.data.contentLength ?? 0;
      if (e.event === "Progress") {
        done += e.data.chunkLength;
        if (total > 0) renderBanner(`Downloading… ${Math.round((done / total) * 100)}%`);
      }
      if (e.event === "Finished") renderBanner("Installing…");
    });
    // On Windows the installer closes the app itself; elsewhere restart it.
    await relaunch();
  } catch (e) {
    installing = false;
    renderBanner();
    toast(`The update could not be installed: ${errorText(e)}`, true);
  }
}

/** Looks for a newer release. `manual` reports "up to date" and errors too. */
export async function checkForUpdate(manual = false): Promise<void> {
  const status = document.getElementById("update-status");
  if (manual && status) status.textContent = "Checking…";
  try {
    const update = await check();
    if (update) {
      pending = update;
      if (manual) dismissed = null;
      renderBanner();
      if (status) status.textContent = `Version ${update.version} is available.`;
    } else if (status && manual) {
      status.textContent = "You have the latest version.";
    }
  } catch (e) {
    // Offline or GitHub unreachable: stay quiet unless the user asked.
    if (manual && status) status.textContent = `Could not check for updates: ${errorText(e)}`;
  }
}

export async function initUpdates() {
  try {
    $("app-version").textContent = await getVersion();
  } catch {
    // Leave the placeholder.
  }
  $("check-updates").addEventListener("click", (e) => {
    e.preventDefault();
    void checkForUpdate(true);
  });
  // Let the rest of the app load first.
  window.setTimeout(() => void checkForUpdate(), 5000);
  window.setInterval(() => void checkForUpdate(), RECHECK_MS);
}
