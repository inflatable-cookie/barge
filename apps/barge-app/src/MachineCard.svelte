<script lang="ts">
  import { Card, IconButton, Menu, StatusIndicator, type MenuItem } from "@inflatable-cookie/poodle-svelte";
  import ObservedAge from "./ObservedAge.svelte";
  import { offerIcons } from "./icons";
  import { offerLabel, powerButton, secondaryOffers, stateLabel, tone } from "./machineModel";
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
  /** The ring runs while a countdown or a send is in flight, and nothing else repeats it. */
  let ring = $derived(busy || status.waiting || status.phase === "pending");
  /** Everything the state offers beside the power button, then Details. */
  let actions = $derived(secondaryOffers(status));
  let menu = $derived<MenuItem[]>([
    ...actions.map((offer) => ({ value: offer, label: offerLabel(offer), disabled: busy })),
    ...(actions.length > 0 ? [{ value: "separator", label: "", kind: "separator" as const }] : []),
    { value: "details", label: "Details…" },
  ]);
  let unreachablePending = $derived(status.phase === "pending" && !machine.reachable);
</script>

<Card variant="outlined" ariaLabel={`${machine.name} machine`}>
  <div class="machine">
    <div class="power" data-tone={tone(status)} data-waiting={ring}>
      <IconButton
        variant="secondary"
        icon={offerIcons[power.icon]}
        ariaLabel={power.label}
        tooltip={power.tooltip}
        tooltipPlacement="bottom"
        disabled={busy || power.offer === null}
        onClick={() => { if (power.offer) onOffer(power.offer); }}
      />
    </div>
    <div class="body">
      <div class="head">
        <h2>{machine.name}</h2>
        <div class="actions-menu">
          <Menu items={menu} size="sm" placement="bottom-end" ariaLabel={`${machine.name} actions`} triggerAriaLabel={`Actions for ${machine.name}`} onAction={(value) => { if (value === "details") onDetails(); else onOffer(value as Offer); }}>
            {#snippet trigger()}
              <IconButton variant="ghost" size="sm" icon="ellipsis" ariaLabel={`Actions for ${machine.name}`} tooltip="More actions" />
            {/snippet}
          </Menu>
        </div>
      </div>
      <StatusIndicator status={tone(status)} label={stateLabel(status, now)} />
      <p class="muted">{machine.mode === "lan" ? "LAN" : "Tailscale"} · {machine.address}</p>
      {#if status.last_seen_unix_ms !== null}
        <p class="muted">Last seen <ObservedAge at={status.last_seen_unix_ms} {now} /></p>
      {/if}
      {#if unreachablePending}
        <p class="note">Can't reach the machine. A cancel can't be confirmed until it answers again.</p>
      {/if}
    </div>
  </div>
</Card>
