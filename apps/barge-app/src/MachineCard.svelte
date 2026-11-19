<script lang="ts">
  import { Button, Card, DetailItem, Spinner, StatusIndicator, TimeAgo } from "@inflatable-cookie/poodle-svelte";
  import { offerLabel, primaryOffer, stateLabel, tone } from "./machineModel";
  import type { MachineView, Offer } from "./types";

  let { machine, now, busy, onOffer, onPair, onRemove }: {
    machine: MachineView;
    now: number;
    busy: boolean;
    onOffer: (offer: Offer) => void;
    onPair: () => void;
    onRemove: () => void;
  } = $props();

  let status = $derived(machine.status);
  let primary = $derived(primaryOffer(status));
  let unreachablePending = $derived(status.phase === "pending" && !machine.reachable);
</script>

<Card variant="outlined" ariaLabel={`${machine.name} machine`}>
  <div class="card-head">
    <h2>{machine.name}</h2>
    <span class="state">
      <StatusIndicator status={tone(status)} label={stateLabel(status, now)} />
      {#if status.waiting}<Spinner size="sm" ariaLabel="Waiting for {machine.name}" />{/if}
    </span>
  </div>
  <p class="muted">{machine.mode === "lan" ? "LAN" : "Tailscale"} · {machine.address}</p>
  {#if unreachablePending}
    <p class="note">Can't reach the machine. A cancel can't be confirmed until it answers again.</p>
  {/if}
  {#if status.last_seen_unix_ms !== null}
    <p class="muted">Last seen <TimeAgo datetime={status.last_seen_unix_ms} live /></p>
  {/if}
  {#if status.offers.length > 0}
    <div class="actions">
      {#each status.offers as offer (offer)}
        <Button
          variant={offer === primary ? "primary" : offer === "moonlight" ? "ghost" : "secondary"}
          disabled={busy}
          onClick={() => onOffer(offer)}
        >{offerLabel(offer)}</Button>
      {/each}
    </div>
  {/if}
  <details class="more">
    <summary>Details</summary>
    <DetailItem label="MAC" value={machine.mac} />
    <DetailItem label="Port" value={String(machine.daemon_port)} />
    <DetailItem label="Last observation">{#if machine.seen_at_unix_ms !== null}<TimeAgo datetime={machine.seen_at_unix_ms} live />{:else}Never{/if}</DetailItem>
    {#if machine.error}<p class="muted">Last error: {machine.error}</p>{/if}
    <div class="actions">
      <Button variant="ghost" disabled={busy} onClick={onPair}>Pair</Button>
      <Button variant="ghost" tone="danger" disabled={busy} onClick={onRemove}>Remove…</Button>
    </div>
  </details>
</Card>
