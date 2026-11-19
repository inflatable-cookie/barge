<script lang="ts">
  import { onMount, untrack } from "svelte";
  import Machines from "./Machines.svelte";
  import Storage from "./Storage.svelte";
  import Activity from "./Activity.svelte";
  import Settings from "./Settings.svelte";
  import { listen } from "@tauri-apps/api/event";
  import { SidebarNav } from "@inflatable-cookie/poodle-svelte";
  import { applyThemeAttributes } from "@inflatable-cookie/poodle-core/tokens/runtime";
  import type { AppApi, Preferences } from "./types";

  let { api, initialView = "machines" }: { api: AppApi; initialView?: string } = $props();

  const savedDefaults: Preferences = {
    launch_at_login: false,
    refresh_interval: 10,
    theme: "system",
    show_window_at_launch: true,
  };
  const views = ["machines", "storage", "activity", "settings"];
  const groups = [
    {
      id: "main",
      items: ["Machines", "Storage", "Activity", "Settings"].map((label) => ({
        label,
        value: label.toLowerCase(),
      })),
    },
  ];

  // Only the initial view matters: navigation takes over afterwards.
  let view = $state(untrack(() => initialView));
  // Bumped on each request from the menu, so Storage re-reads when it is opened.
  let visit = $state(0);
  // The value the store holds. A failed save falls back to this.
  let saved = $state<Preferences>({ ...savedDefaults });
  let preferences = $state<Preferences>({ ...savedDefaults });
  let settingsError = $state("");
  let saving = $state(false);

  /** Tauri rejects with a string; a browser or test rejects with an Error. */
  function describe(reason: unknown) {
    return reason instanceof Error ? reason.message : String(reason);
  }

  function applyTheme() {
    const dark =
      preferences.theme === "dark" ||
      (preferences.theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    applyThemeAttributes(document.documentElement, {
      theme: dark ? "graphite" : "iceberg",
    });
  }

  async function save(next: Preferences) {
    saving = true;
    preferences = next;
    try {
      const stored = await api.savePreferences(next);
      preferences = stored;
      saved = stored;
      settingsError = "";
    } catch (reason) {
      // The store kept the old value, so the UI does too.
      preferences = { ...saved };
      settingsError = `Could not save settings: ${describe(reason)}`;
    } finally {
      saving = false;
      applyTheme();
    }
  }

  onMount(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const changed = () => applyTheme();
    media.addEventListener("change", changed);
    let unlisten: (() => void) | undefined;
    if ("__TAURI_INTERNALS__" in window) {
      void listen<string>("show-view", (event) => {
        if (views.includes(event.payload)) {
          view = event.payload;
          visit += 1;
        }
      }).then((dispose) => {
        unlisten = dispose;
      });
    }
    void api
      .preferences()
      .then((value) => {
        preferences = value;
        saved = value;
        applyTheme();
      })
      .catch((reason) => {
        settingsError = `Could not load settings: ${describe(reason)}`;
      });
    return () => {
      media.removeEventListener("change", changed);
      unlisten?.();
    };
  });
</script>

<div class="shell">
  <aside>
    <div class="brand">Barge</div>
    <SidebarNav {groups} bind:value={view} ariaLabel="Barge navigation" />
  </aside>
  <main class="content">
    {#if view === "machines"}
      <Machines {api} refreshInterval={preferences.refresh_interval} />
    {:else if view === "storage"}
      {#key visit}<Storage {api} />{/key}
    {:else if view === "activity"}
      <Activity />
    {:else}
      <Settings {preferences} {saving} {settingsError} onSave={(next) => void save(next)} />
    {/if}
  </main>
</div>
