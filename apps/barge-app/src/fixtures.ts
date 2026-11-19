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
  localMachine: async () => ({ name: "Tom’s Mac", state: "awake", observed_at_unix_ms: Date.now(), model: "Mac15,7", uptime_seconds: 54000, volumes, doctor_findings: 0, storage_error: null }),
  machines: async () => scenario === "empty" ? [] : [{ name: "bazzite", hostname: "bazzite.local", mac: "00:11:22:33:44:55", mode: "lan", address: "bazzite.local", reachable: scenario !== "unreachable", error: scenario === "unreachable" ? "Connection timed out" : null, observation: { power: "awake", observed_at_unix_ms: Date.now() - 12000, pending: scenario === "pending" ? { action: "sleep", deadline_unix_ms: Date.now() + 12000 } : null } }],
  addMachine: async () => {}, pairMachine: async () => {}, removeMachine: async () => {}, machineAction: async () => {}, moonlight: async () => {},
};

const target = document.getElementById("app");
if (target) mount(App, { target, props: { api, initialView: view } });

function press(label: string) {
  const button = [...document.querySelectorAll("button")].find(node => node.textContent?.trim().startsWith(label));
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

if (["awake", "unreachable", "pending"].includes(scenario)) setTimeout(() => press(`bazzite · LAN · ${scenario === "unreachable" ? "Unreachable" : scenario === "pending" ? "Action pending" : "Awake"}`), 150);
