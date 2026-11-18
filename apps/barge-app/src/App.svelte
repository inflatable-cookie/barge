<script lang="ts">
  import { onMount } from "svelte";
  import { SidebarNav, EmptyState, Button } from "@inflatable-cookie/poodle-svelte";
  import { applyThemeAttributes } from "@inflatable-cookie/poodle-core/tokens/runtime";
  import type { AppApi, Preferences, VolumeSummary } from "./types";

  let { api, popover = false }: { api: AppApi; popover?: boolean } = $props();
  let view = $state("machines");
  let volumes = $state<VolumeSummary[]>([]);
  let preferences = $state<Preferences>({ launch_at_login: false, refresh_interval: 10, theme: "system" });
  let error = $state("");
  let loading = $state(true);
  const groups = [{ id: "main", items: ["Machines", "Storage", "Activity", "Settings"].map(label => ({ label, value: label.toLowerCase() })) }];

  function applyTheme() {
    const dark = preferences.theme === "dark" || (preferences.theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    applyThemeAttributes(document.documentElement, { theme: dark ? "graphite" : "iceberg" });
  }
  async function refresh() {
    try { volumes = await api.volumes(); error = ""; }
    catch (reason) { error = `Could not read storage: ${String(reason)}`; }
    finally { loading = false; }
  }
  async function save() {
    try { await api.savePreferences(preferences); error = ""; applyTheme(); }
    catch (reason) { error = `Could not save settings: ${String(reason)}`; }
  }
  function space(volume: VolumeSummary) {
    if (volume.presence === "absent") return "Absent";
    if (volume.presence === "mismatch") return "Different volume at mount";
    return `${((volume.free_bytes ?? 0) / 1024 ** 3).toFixed(1)} GB free`;
  }
  onMount(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const changed = () => applyTheme();
    const opened = () => { if (popover) void refresh(); };
    media.addEventListener("change", changed);
    window.addEventListener("focus", opened);
    void api.preferences().then(value => { preferences = value; applyTheme(); }).catch(reason => { error = `Could not load settings: ${String(reason)}`; });
    void refresh();
    return () => {
      media.removeEventListener("change", changed);
      window.removeEventListener("focus", opened);
    };
  });
  $effect(() => {
    const timer = window.setInterval(() => { if (document.visibilityState === "visible") void refresh(); }, preferences.refresh_interval * 1000);
    return () => window.clearInterval(timer);
  });
</script>

{#if popover}
  <main class="popover">
    <h1>Barge</h1>
    <section aria-label="Machines"><h2>Machines</h2><p>No machines yet</p></section>
    <section aria-label="Storage"><h2>Storage</h2>
      {#if loading}<p>Checking storage…</p>{:else if volumes.length === 0}<p>No volumes registered</p>{:else}{#each volumes as volume}<p><strong>{volume.name}</strong> · {space(volume)}</p>{/each}{/if}
    </section>
    {#if error}<p role="alert">{error}</p>{/if}
    <div class="actions"><Button onClick={() => void api.openWindow()}>Open Barge</Button><Button variant="ghost" onClick={() => void api.quit()}>Quit</Button></div>
  </main>
{:else}
  <div class="shell">
    <aside><div class="brand">Barge</div><SidebarNav {groups} bind:value={view} ariaLabel="Barge navigation" /></aside>
    <main class="content">
      {#if error}<p role="alert">{error}</p>{/if}
      {#if view === "machines"}
        <h1>Machines</h1><EmptyState title="No machines yet" message="Machines registered with Barge will appear here." />
      {:else if view === "storage"}
        <div class="heading"><h1>Storage</h1><Button variant="ghost" onClick={() => void refresh()}>Refresh</Button></div>
        {#if loading}<p>Checking storage…</p>{:else if volumes.length === 0}<EmptyState title="No volumes registered" message="Register a volume with Barge to see it here." />{:else}<ul class="volumes">{#each volumes as volume}<li><strong>{volume.name}</strong><span>{space(volume)}</span></li>{/each}</ul>{/if}
      {:else if view === "activity"}
        <h1>Activity</h1><EmptyState title="No activity yet" message="Actions taken in Barge will appear here. CLI actions are not shown." />
      {:else}
        <h1>Settings</h1><div class="settings">
          <label><span>Launch at login</span><input type="checkbox" bind:checked={preferences.launch_at_login} onchange={() => void save()} /></label>
          <label><span>Refresh interval</span><select bind:value={preferences.refresh_interval} onchange={() => void save()}><option value={10}>10 seconds</option><option value={30}>30 seconds</option><option value={60}>1 minute</option></select></label>
          <label><span>Theme</span><select bind:value={preferences.theme} onchange={() => void save()}><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label>
        </div>
      {/if}
    </main>
  </div>
{/if}
