<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { SidebarNav, EmptyState, Button, Callout, Select, Switch, TimeAgo } from "@inflatable-cookie/poodle-svelte";
  import { applyThemeAttributes } from "@inflatable-cookie/poodle-core/tokens/runtime";
  import type { AppApi, Preferences, Theme, VolumeSummary } from "./types";

  let { api, popover = false, initialView = "machines" }: { api: AppApi; popover?: boolean; initialView?: string } = $props();

  const savedDefaults: Preferences = { launch_at_login: false, refresh_interval: 10, theme: "system", show_window_at_launch: true };
  const groups = [{ id: "main", items: ["Machines", "Storage", "Activity", "Settings"].map(label => ({ label, value: label.toLowerCase() })) }];
  const intervals = [10, 30, 60].map(seconds => ({ value: String(seconds), label: seconds === 60 ? "1 minute" : `${seconds} seconds` }));
  const themes: { value: Theme; label: string }[] = [
    { value: "system", label: "System" },
    { value: "light", label: "Light" },
    { value: "dark", label: "Dark" },
  ];

  // Only the initial view matters: navigation takes over afterwards.
  let view = $state(untrack(() => initialView));
  let volumes = $state<VolumeSummary[]>([]);
  // The value the store holds. A failed save falls back to this.
  let saved = $state<Preferences>({ ...savedDefaults });
  let preferences = $state<Preferences>({ ...savedDefaults });
  let settingsError = $state("");
  let saving = $state(false);
  let storageError = $state("");
  let observedAt = $state<number | null>(null);
  let loading = $state(true);

  /** Tauri rejects with a string; a browser or test rejects with an Error. */
  function describe(reason: unknown) {
    return reason instanceof Error ? reason.message : String(reason);
  }

  function applyTheme() {
    const dark = preferences.theme === "dark" || (preferences.theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    applyThemeAttributes(document.documentElement, { theme: dark ? "graphite" : "iceberg" });
  }

  /**
   * Reads storage: on open, and when the operator asks for a refresh. It is
   * never polled (app.md, Architecture): a poll only ever covers machine state.
   */
  async function readStorage() {
    try {
      volumes = await api.volumes();
      observedAt = Date.now();
      storageError = "";
    } catch (reason) {
      // Keep the last observation and say when it was, rather than implying the
      // volumes are current.
      storageError = `Could not read storage: ${describe(reason)}`;
    } finally {
      loading = false;
    }
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

  function space(volume: VolumeSummary) {
    if (volume.presence === "absent") return "Absent";
    if (volume.presence === "mismatch") return "Different volume at mount";
    return `${((volume.free_bytes ?? 0) / 1024 ** 3).toFixed(1)} GiB free`;
  }

  onMount(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const changed = () => applyTheme();
    // The popover exists for the whole session and is shown on demand, so a
    // focus event is the popover opening: read storage then, and only then.
    const opened = () => { if (popover) void readStorage(); };
    media.addEventListener("change", changed);
    window.addEventListener("focus", opened);
    void api.preferences()
      .then(value => { preferences = value; saved = value; applyTheme(); })
      .catch(reason => { settingsError = `Could not load settings: ${describe(reason)}`; });
    void readStorage();
    return () => {
      media.removeEventListener("change", changed);
      window.removeEventListener("focus", opened);
    };
  });
</script>

{#snippet observed(at: number)}
  <p class="observed">Last read <TimeAgo datetime={at} live={false} />{#if storageError} · showing an earlier observation{/if}</p>
{/snippet}

{#snippet storageErrorNote()}
  {#if storageError}<Callout tone="danger" announceMode="assertive" title="Storage could not be read" message={storageError} />{/if}
{/snippet}

{#if popover}
  <main class="popover">
    <h1>Barge</h1>
    <section aria-label="Machines"><h2>Machines</h2><p>No machines yet</p></section>
    <section aria-label="Storage"><h2>Storage</h2>
      {#if loading}<p>Checking storage…</p>
      {:else}
        {@render storageErrorNote()}
        {#if observedAt !== null}
          {@render observed(observedAt)}
          {#if volumes.length === 0}<p>No volumes registered</p>{:else}{#each volumes as volume}<p><strong>{volume.name}</strong> · {space(volume)}</p>{/each}{/if}
        {:else if !storageError}<p>No storage read yet</p>{/if}
      {/if}
    </section>
    <div class="actions"><Button onClick={() => void api.openWindow()}>Open Barge</Button><Button variant="ghost" onClick={() => void api.quit()}>Quit</Button></div>
  </main>
{:else}
  <div class="shell">
    <aside><div class="brand">Barge</div><SidebarNav {groups} bind:value={view} ariaLabel="Barge navigation" /></aside>
    <main class="content">
      {#if view === "machines"}
        <h1>Machines</h1><EmptyState title="No machines yet" message="Machines registered with Barge will appear here." />
      {:else if view === "storage"}
        <div class="heading"><h1>Storage</h1><Button variant="ghost" onClick={() => void readStorage()}>Refresh</Button></div>
        {#if loading}<p>Checking storage…</p>
        {:else}
          {@render storageErrorNote()}
          {#if observedAt !== null}
            {@render observed(observedAt)}
            {#if volumes.length === 0}
              <EmptyState title="No volumes registered" message="Register a volume with Barge to see it here." />
            {:else}
              <ul class="volumes">{#each volumes as volume}<li><strong>{volume.name}</strong><span>{space(volume)}</span></li>{/each}</ul>
            {/if}
          {/if}
        {/if}
      {:else if view === "activity"}
        <h1>Activity</h1><EmptyState title="No activity yet" message="Actions taken in Barge will appear here. CLI actions are not shown." />
      {:else}
        <h1>Settings</h1>
        {#if settingsError}<Callout tone="danger" announceMode="assertive" message={settingsError} />{/if}
        <div class="settings">
          <div class="setting"><span>Show window at launch</span>
            <Switch ariaLabel="Show window at launch" checked={preferences.show_window_at_launch} disabled={saving}
              onCheckedChange={(checked) => void save({ ...preferences, show_window_at_launch: checked })} /></div>
          <div class="setting"><span>Launch at login</span>
            <Switch ariaLabel="Launch at login" checked={preferences.launch_at_login} disabled={saving}
              onCheckedChange={(checked) => void save({ ...preferences, launch_at_login: checked })} /></div>
          <div class="setting"><span>Refresh interval</span>
            <Select ariaLabel="Refresh interval" options={intervals} value={String(preferences.refresh_interval)} disabled={saving}
              onValueChange={(value) => void save({ ...preferences, refresh_interval: Number(value) })} /></div>
          <div class="setting"><span>Theme</span>
            <Select ariaLabel="Theme" options={themes} value={preferences.theme} disabled={saving}
              onValueChange={(value) => void save({ ...preferences, theme: value as Theme })} /></div>
        </div>
      {/if}
      <footer class="note">
        Barge lives in the menu bar at the top of the screen. macOS can hide its item when the menu bar is full,
        which is why the window opens on launch.
      </footer>
    </main>
  </div>
{/if}
