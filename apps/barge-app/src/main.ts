import { mount } from "svelte";
import { invoke } from "@tauri-apps/api/core";
import App from "./App.svelte";
import type { AppApi, Preferences, VolumeSummary } from "./types";
import "@inflatable-cookie/poodle-core/tokens/styles.css";
import "@inflatable-cookie/poodle-core/tokens/themes.css";
import "./style.css";

const api: AppApi = {
  volumes: () => invoke<VolumeSummary[]>("storage_volumes"),
  preferences: () => invoke<Preferences>("preferences_get"),
  savePreferences: (preferences) => invoke<Preferences>("preferences_set", { preferences }),
  openWindow: () => invoke("open_main_window"),
  quit: () => invoke("quit_app"),
};
const target = document.getElementById("app");
if (target) mount(App, { target, props: { api, popover: location.hash === "#popover" } });
