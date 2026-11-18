/**
 * Fixture harness for renderer screenshots: `/fixtures.html?view=storage&scenario=stale`.
 *
 * It mounts the real App against a fake api, so a screenshot shows the same
 * markup and Poodle styles the app renders. Not part of the app build.
 */
import { mount } from "svelte";
import App from "./App.svelte";
import type { AppApi, Preferences, VolumeSummary } from "./types";
import "@inflatable-cookie/poodle-core/tokens/styles.css";
import "@inflatable-cookie/poodle-core/tokens/themes.css";
import "./style.css";

const params = new URLSearchParams(location.search);
const view = params.get("view") ?? "machines";
const scenario = params.get("scenario") ?? "storage";
const popover = params.get("popover") === "1";

const volumes: VolumeSummary[] = [
  { name: "Builds", presence: "present", free_bytes: 214 * 1024 ** 3 },
  { name: "Archive", presence: "absent", free_bytes: null },
  { name: "Media", presence: "mismatch", free_bytes: null },
];
const preferences: Preferences = { launch_at_login: true, refresh_interval: 10, theme: "system", show_window_at_launch: true };

let reads = 0;
const api: AppApi = {
  volumes: async () => {
    reads += 1;
    if (scenario === "stale" && reads > 1) throw new Error("the volume is not mounted");
    if (scenario === "failed") throw new Error("the account state is unreadable");
    return scenario === "empty" ? [] : volumes;
  },
  preferences: async () => preferences,
  savePreferences: async (next: Preferences) => {
    if (scenario === "save-failed") throw new Error("the config store is read-only");
    return next;
  },
  openWindow: async () => {},
  quit: async () => {},
};

const target = document.getElementById("app");
if (target) mount(App, { target, props: { api, popover, initialView: view } });

function press(label: string) {
  const button = [...document.querySelectorAll("button")].find(node => node.textContent?.trim() === label);
  button?.click();
}

// Reach the states a screenshot needs to show.
if (scenario === "stale") {
  setTimeout(() => press("Refresh"), 150);
}
if (scenario === "save-failed") {
  setTimeout(() => {
    document.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Launch at login"]')?.click();
  }, 150);
}
