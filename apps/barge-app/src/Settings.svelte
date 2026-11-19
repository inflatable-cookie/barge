<script lang="ts">
  import { Callout, Select, Switch } from "@inflatable-cookie/poodle-svelte";
  import type { Preferences, Theme } from "./types";

  let {
    preferences,
    saving,
    settingsError,
    onSave,
  }: {
    preferences: Preferences;
    saving: boolean;
    settingsError: string;
    onSave: (next: Preferences) => void;
  } = $props();

  const intervals = [10, 30, 60].map((seconds) => ({
    value: String(seconds),
    label: seconds === 60 ? "1 minute" : `${seconds} seconds`,
  }));
  const themes: { value: Theme; label: string }[] = [
    { value: "system", label: "System" },
    { value: "light", label: "Light" },
    { value: "dark", label: "Dark" },
  ];
</script>

<h1>Settings</h1>
{#if settingsError}
  <Callout tone="danger" announceMode="assertive" message={settingsError} />
{/if}
<div class="settings">
  <div class="setting">
    <span>Show window at launch</span>
    <Switch
      ariaLabel="Show window at launch"
      checked={preferences.show_window_at_launch}
      disabled={saving}
      onCheckedChange={(checked) => onSave({ ...preferences, show_window_at_launch: checked })}
    />
  </div>
  <div class="setting">
    <span>Launch at login</span>
    <Switch
      ariaLabel="Launch at login"
      checked={preferences.launch_at_login}
      disabled={saving}
      onCheckedChange={(checked) => onSave({ ...preferences, launch_at_login: checked })}
    />
  </div>
  <div class="setting">
    <span>Refresh interval</span>
    <Select
      ariaLabel="Refresh interval"
      options={intervals}
      value={String(preferences.refresh_interval)}
      disabled={saving}
      onValueChange={(value) => onSave({ ...preferences, refresh_interval: Number(value) })}
    />
  </div>
  <div class="setting">
    <span>Theme</span>
    <Select
      ariaLabel="Theme"
      options={themes}
      value={preferences.theme}
      disabled={saving}
      onValueChange={(value) => onSave({ ...preferences, theme: value as Theme })}
    />
  </div>
</div>
