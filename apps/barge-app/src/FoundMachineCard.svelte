<script lang="ts">
  import { Button, Card, Icon, StatusIndicator } from "@inflatable-cookie/poodle-svelte";
  import type { DiscoveredMachineView } from "./types";

  let { machine, busy, onAdd }: {
    machine: DiscoveredMachineView;
    busy: boolean;
    onAdd: () => void;
  } = $props();
</script>

<Card variant="outlined" ariaLabel={`${machine.name} discovered machine`}>
  <div class="machine">
    <div class="found-avatar" aria-hidden="true">
      <Icon name="monitor-play" size="lg" />
    </div>
    <div class="body">
      <div class="head">
        <h2>{machine.name}</h2>
        <Button variant="primary" size="sm" disabled={busy} onClick={onAdd}>Add</Button>
      </div>
      <StatusIndicator status="info" label="Found on network" />
      <p class="muted">{machine.mode.toUpperCase()} · {machine.address}:{machine.port}</p>
    </div>
  </div>
</Card>
