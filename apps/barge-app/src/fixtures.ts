/**
 * Fixture harness for renderer screenshots: `/fixtures.html?view=machines&scenario=awake&theme=dark`.
 * `&details` opens the first machine's details, `&menu` opens its actions menu,
 * and `&chrome=native` drops the macOS top strip.
 *
 * It mounts the real App against a fake api, so a screenshot shows the same
 * markup and Poodle styles the app renders. Not part of the app build. The
 * machine states are fixed here; the Rust state model is what produces them in
 * the app, and its tests cover each one.
 */
import { mount } from "svelte";
import App from "./App.svelte";
import type { AppApi, MachineStatus, MachineView, Preferences, Theme, VolumeSummary } from "./types";
import "@inflatable-cookie/poodle-core/tokens/styles.css";
import "@inflatable-cookie/poodle-core/tokens/themes.css";
import "./style.css";

const params = new URLSearchParams(location.search);
const view = params.get("view") ?? "machines";
const scenario = params.get("scenario") ?? "storage";
const theme = (params.get("theme") ?? "system") as Theme;
if (params.get("chrome") !== "native") document.documentElement.dataset.chrome = "overlay";

const volumes: VolumeSummary[] = [
  { name: "Builds", presence: "present", free_bytes: 214 * 1024 ** 3 },
  { name: "Archive", presence: "absent", free_bytes: null },
  { name: "Media", presence: "mismatch", free_bytes: null },
];
const preferences: Preferences = { launch_at_login: true, refresh_interval: 10, theme, show_window_at_launch: true };

const now = Date.now();
const awakeOffers: MachineStatus["offers"] = ["sleep", "restart", "shutdown", "moonlight"];
const status = (fields: Partial<MachineStatus> & Pick<MachineStatus, "phase">): MachineStatus => ({
  action: null, deadline_unix_ms: null, last_seen_unix_ms: null, offers: [], waiting: false, ...fields,
});
const bazzite = (fields: Partial<MachineView> & Pick<MachineView, "status">): MachineView => ({
  name: "bazzite", hostname: "bazzite.local", mac: "00:11:22:33:44:55", mode: "lan", address: "bazzite.local",
  daemon_port: 9777, reachable: true, error: null, seen_at_unix_ms: now, ...fields,
});

/** The machine each Machines scenario shows. */
const machineFor: Record<string, MachineView[]> = {
  offline: [bazzite({
    reachable: false, error: "Connection timed out", seen_at_unix_ms: now - 12 * 60_000,
    status: status({ phase: "offline", offers: ["wake"], last_seen_unix_ms: now - 12 * 60_000 }),
  })],
  waking: [bazzite({
    reachable: false, error: "Connection timed out", seen_at_unix_ms: now - 12 * 60_000,
    status: status({ phase: "waking", waiting: true, last_seen_unix_ms: now - 12 * 60_000 }),
  })],
  awake: [bazzite({ status: status({ phase: "awake", offers: awakeOffers }) })],
  pending: [bazzite({
    status: status({ phase: "pending", action: "sleep", deadline_unix_ms: now + 12_000, offers: ["cancel"], waiting: true }),
  })],
};

let reads = 0;
const api: AppApi = {
  volumes: async () => {
    reads += 1;
    if (scenario === "stale" && reads > 1) throw new Error("the volume is not mounted");
    if (scenario === "failed") throw new Error("the account state is unreadable");
    return scenario === "empty" ? [] : volumes;
  },
  doctorStatus: async () => ({ observation: { findings: 0, observed_at_unix_ms: Date.now() - 12000 }, error: null }),
  preferences: async () => preferences,
  savePreferences: async (next: Preferences) => {
    if (scenario === "save-failed") throw new Error("the config store is read-only");
    return next;
  },
  openWindow: async () => {},
  quit: async () => {},
  machines: async () => machineFor[scenario] ?? [],
  refreshMachines: async () => machineFor[scenario] ?? [],
  addMachine: async () => {}, pairMachine: async () => {}, removeMachine: async () => {},
  beginMachineAction: async () => machineFor[scenario] ?? [], machineAction: async () => {}, moonlight: async () => {},
};

const target = document.getElementById("app");
if (target) mount(App, { target, props: { api, initialView: view } });

// Reach the states a screenshot needs to show.
if (scenario === "stale") {
  setTimeout(() => [...document.querySelectorAll("button")].find(node => node.textContent?.trim() === "Refresh")?.click(), 150);
}
if (scenario === "save-failed") {
  setTimeout(() => {
    document.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Launch at login"]')?.click();
  }, 150);
}
if (params.has("details")) {
  // Details lives in the card's actions menu, like every action beside the power button.
  setTimeout(() => {
    document.querySelector<HTMLButtonElement>('button[aria-label^="Actions for"]')?.click();
    setTimeout(() => {
      [...document.querySelectorAll<HTMLElement>('[role="menuitem"]')]
        .find((item) => item.textContent?.trim() === "Details")
        ?.click();
    }, 150);
  }, 150);
}
if (params.has("menu")) {
  setTimeout(() => document.querySelector<HTMLButtonElement>('button[aria-label^="Actions for"]')?.click(), 150);
}
