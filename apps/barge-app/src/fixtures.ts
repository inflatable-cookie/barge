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
import type {
  AppApi,
  DiscoveredMachineView,
  MachineStatus,
  MachineView,
  PairedClientView,
  PendingPairPrompt,
  Preferences,
  Theme,
  VolumeSummary,
} from "./types";
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
const preferences: Preferences = {
  launch_at_login: true,
  refresh_interval: 10,
  theme,
  show_window_at_launch: true,
};

const now = Date.now();
const awakeOffers: MachineStatus["offers"] = ["sleep", "restart", "shutdown", "moonlight"];
const status = (fields: Partial<MachineStatus> & Pick<MachineStatus, "phase">): MachineStatus => ({
  action: null,
  deadline_unix_ms: null,
  last_seen_unix_ms: null,
  offers: [],
  waiting: false,
  ...fields,
});
const linuxPc = (fields: Partial<MachineView> & Pick<MachineView, "status">): MachineView => ({
  name: "linux-pc",
  hostname: "linux-pc.local",
  mac: "00:11:22:33:44:55",
  mode: "lan",
  paired: true,
  address: "linux-pc.local",
  daemon_port: 9777,
  reachable: true,
  error: null,
  seen_at_unix_ms: now,
  ...fields,
});

const foundLinuxPc: DiscoveredMachineView = {
  id: "9b2a4f01d83c4e21a6f8c01234567890",
  name: "Linux PC",
  mode: "lan",
  protocol_version: "1",
  address: "192.0.2.42",
  port: 9777,
};

const approvalPrompt: PendingPairPrompt = {
  machine_name: "linux-pc",
  request_id: "req-deck-1",
  client_name: "Steam Deck",
  code: "340092",
  created_at_unix_ms: now - 12_000,
  expires_at_unix_ms: now + 108_000,
};

const clientsFixture: PairedClientView[] = [
  { id: "cid-mac", name: "MacBook Pro", paired_at_unix_ms: now - 3 * 86_400_000, this_client: true },
  { id: "cid-deck", name: "Steam Deck", paired_at_unix_ms: now - 2 * 3_600_000, this_client: false },
];

/** The machine each Machines scenario shows. */
const machineFor: Record<string, MachineView[]> = {
  offline: [linuxPc({
    reachable: false,
    error: "Connection timed out",
    seen_at_unix_ms: now - 12 * 60_000,
    status: status({ phase: "offline", offers: ["wake"], last_seen_unix_ms: now - 12 * 60_000 }),
  })],
  waking: [linuxPc({
    reachable: false,
    error: "Connection timed out",
    seen_at_unix_ms: now - 12 * 60_000,
    status: status({ phase: "waking", waiting: true, last_seen_unix_ms: now - 12 * 60_000 }),
  })],
  awake: [linuxPc({ status: status({ phase: "awake", offers: awakeOffers }) })],
  pending: [linuxPc({
    status: status({ phase: "pending", action: "sleep", deadline_unix_ms: now + 12_000, offers: ["cancel"], waiting: true }),
  })],
  approval: [linuxPc({ status: status({ phase: "awake", offers: awakeOffers }) })],
  found: [],
  pairing: [],
};

const discoveredFor: Record<string, DiscoveredMachineView[]> = {
  found: [foundLinuxPc],
  pairing: [foundLinuxPc],
};

const pendingFor: Record<string, PendingPairPrompt[]> = {
  approval: [approvalPrompt],
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
  discoverMachines: async () => discoveredFor[scenario] ?? [],
  startPair: async (input) => ({
    request_id: "req-fixture-1",
    code: "340092",
    expires_at_unix_ms: Date.now() + 120_000,
    machine_name: input.name ?? input.address,
  }),
  pollPair: async () => ({ status: "pending", machine_name: "Linux PC" }),
  cancelPair: async () => {},
  pendingPairs: async () => pendingFor[scenario] ?? [],
  approvePair: async () => {},
  denyPair: async () => {},
  machineClients: async () => clientsFixture,
  revokeClient: async () => {},
  removeMachine: async () => {},
  beginMachineAction: async () => machineFor[scenario] ?? [],
  machineAction: async () => {},
  moonlight: async () => {},
};

const target = document.getElementById("app");
if (target) mount(App, { target, props: { api, initialView: view } });

// Reach the states a screenshot needs to show.
if (scenario === "stale") {
  setTimeout(() => [...document.querySelectorAll("button")].find((node) => node.textContent?.trim() === "Refresh")?.click(), 150);
}
if (scenario === "save-failed") {
  setTimeout(() => {
    document.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Launch at login"]')?.click();
  }, 150);
}
if (scenario === "pairing") {
  setTimeout(() => {
    [...document.querySelectorAll<HTMLButtonElement>("button")]
      .find((node) => node.textContent?.trim() === "Add")
      ?.click();
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
