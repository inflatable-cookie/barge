<script lang="ts">
  import { Card, IconButton, StatusIndicator } from "@inflatable-cookie/poodle-svelte";
  import ObservedAge from "./ObservedAge.svelte";
  import { offerIcons } from "./icons";
  import { countdownSeconds, offerLabel, powerButton, secondaryOffers, stateLabel, tone } from "./machineModel";
  import type { MachineView, Offer } from "./types";

  let { machine, now, busy, onOffer, onDetails }: {
    machine: MachineView;
    now: number;
    busy: boolean;
    onOffer: (offer: Offer) => void;
    onDetails: () => void;
  } = $props();

  let status = $derived(machine.status);
  let power = $derived(powerButton(status, now));
  let secondary = $derived(secondaryOffers(status));
  let countdown = $derived(
    status.phase === "pending" && status.deadline_unix_ms !== null ? countdownSeconds(status.deadline_unix_ms, now) : null,
  );
  let unreachablePending = $derived(status.phase === "pending" && !machine.reachable);
</script>

<Card variant="outlined" ariaLabel={`${machine.name} machine`}>
  <div class="machine">
    <div class="power" data-tone={tone(status)} data-waiting={status.waiting}>
      <IconButton
        variant="secondary"
        icon={offerIcons[power.icon]}
        ariaLabel={power.label}
        tooltip={power.tooltip}
        tooltipPlacement="bottom"
        disabled={busy || power.offer === null}
        onClick={() => { if (power.offer) onOffer(power.offer); }}
      />
      {#if countdown !== null}<span class="countdown" aria-hidden="true">{countdown} s</span>{/if}
    </div>
    <div class="body">
      <h2>{machine.name}</h2>
      <StatusIndicator status={tone(status)} label={stateLabel(status, now)} />
      <p class="muted">{machine.mode === "lan" ? "LAN" : "Tailscale"} · {machine.address}</p>
      {#if status.last_seen_unix_ms !== null}
        <p class="muted">Last seen <ObservedAge at={status.last_seen_unix_ms} {now} /></p>
      {/if}
      {#if unreachablePending}
        <p class="note">Can't reach the machine. A cancel can't be confirmed until it answers again.</p>
      {/if}
      <div class="tools">
        {#each secondary as offer (offer)}
          <IconButton
            variant="ghost"
            size="sm"
            icon={offerIcons[offer]}
            ariaLabel={offerLabel(offer)}
            disabled={busy}
            onClick={() => onOffer(offer)}
          />
        {/each}
        <span class="details">
          <IconButton variant="ghost" size="sm" icon="info" ariaLabel="Details" tooltip={`${machine.name} details`} onClick={onDetails} />
        </span>
      </div>
    </div>
  </div>
</Card>
