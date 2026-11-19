import { mount } from "svelte";
import { invoke } from "@tauri-apps/api/core";
import App from "./App.svelte";
import type { AppApi, Preferences, VolumeSummary, MachineView, DoctorStatus } from "./types";
import "@inflatable-cookie/poodle-core/tokens/styles.css";
import "@inflatable-cookie/poodle-core/tokens/themes.css";
import "./style.css";

const api: AppApi = {
  volumes: () => invoke<VolumeSummary[]>("storage_volumes"),
  doctorStatus: () => invoke<DoctorStatus>("doctor_status"),
  preferences: () => invoke<Preferences>("preferences_get"),
  savePreferences: (preferences) => invoke<Preferences>("preferences_set", { preferences }),
  openWindow: () => invoke("open_main_window"),
  quit: () => invoke("quit_app"),
  machines: () => invoke<MachineView[]>("machines_list"),
  addMachine: (input) => invoke("machines_add", { input }),
  pairMachine: (name, key) => invoke("machines_pair", { name, key }),
  removeMachine: (name) => invoke("machines_remove", { name }),
  beginMachineAction: (name, operation) => invoke<MachineView[]>("machines_begin", { name, operation }),
  machineAction: (name, operation) => invoke("machines_action", { name, operation }),
  moonlight: () => invoke("open_moonlight"),
};
// The window builder marks a frameless macOS window (commands/window.rs).
if ((window as { __BARGE_CHROME__?: string }).__BARGE_CHROME__ === "overlay") document.documentElement.dataset.chrome = "overlay";
const target = document.getElementById("app");
// The menu opens the window on a view by its hash (see commands/window.rs).
const views = ["machines", "storage", "activity", "settings"];
const hashView = location.hash.slice(1);
if (target) mount(App, { target, props: { api, initialView: views.includes(hashView) ? hashView : "machines" } });
