<script lang="ts">
  import { onMount } from "svelte";
  import MachineCard from "./MachineCard.svelte";
  import ObservedAge from "./ObservedAge.svelte";
  import { AlertDialog, Button, Callout, DetailItem, Dialog, Field, FormDialog, Select, TextInput } from "@inflatable-cookie/poodle-svelte";
  import { needsConfirmation, offerLabel } from "./machineModel";
  import type { AppApi, MachineView, NewMachine, Offer } from "./types";

  /** Polls this often while a machine is waiting on an action (app.md, machines.md). */
  const FAST_POLL_MS = 2000;

  let { api, refreshInterval }: { api: AppApi; refreshInterval: number } = $props();
  let machines = $state<MachineView[]>([]);
  /** False until the first registry snapshot returns: never the empty state. */
  let loaded = $state(false);
  let pollError = $state("");
  let error = $state("");
  let working = $state("");
  let addOpen = $state(false);
  let pairOpen = $state(false);
  let target = $state("");
  /** The machine whose details dialog is open, by name, so a poll refreshes it. */
  let detailsFor = $state<string | null>(null);
  let details = $derived(machines.find(machine => machine.name === detailsFor) ?? null);
  /** A pending confirmation: `offer` is the action, or null to remove the machine. */
  let confirm = $state<{ name: string; offer: Offer | null } | null>(null);
  let mode = $state("lan");
  let name = $state("");
  let hostname = $state("");
  let mac = $state("");
  let tailnet = $state("");
  let key = $state("");
  let now = $state(Date.now());
  let busy = $derived(working !== "");
  const describe = (reason: unknown) => reason instanceof Error ? reason.message : String(reason);

  let timer: ReturnType<typeof setTimeout> | undefined;
  let mounted = true;

  /** What the registry names, without touching the network: shows at once. */
  async function snapshot() {
    try { machines = await api.machines(); }
    catch (reason) { pollError = `Could not read machines: ${describe(reason)}`; }
    finally { loaded = true; }
  }

  /** Poll every daemon and show what it says; a slow one holds only itself. */
  async function refresh() {
    try { machines = await api.refreshMachines(); pollError = ""; }
    catch (reason) { pollError = `Could not read machines: ${describe(reason)}`; }
    schedule();
  }

  /** Waiting machines poll fast; everything else waits the refresh interval. */
  function schedule() {
    clearTimeout(timer);
    if (!mounted) return;
    const delay = machines.some(machine => machine.status.waiting) ? FAST_POLL_MS : refreshInterval * 1000;
    timer = setTimeout(() => void refresh(), delay);
  }

  async function perform(machine: string, offer: Offer) {
    working = machine;
    try {
      // The shared state records the click here, so the card and the menu show
      // its wait before the command returns and before the next poll. A waiting
      // machine then polls fast.
      machines = await api.beginMachineAction(machine, offer);
      schedule();
      await api.machineAction(machine, offer);
      error = "";
    } catch (reason) {
      error = offer === "cancel"
        ? `Cancel could not be confirmed: ${describe(reason)}`
        : `${offerLabel(offer)} failed: ${describe(reason)}`;
    } finally {
      working = "";
      await refresh();
    }
  }

  function act(machine: MachineView, offer: Offer) {
    if (offer === "moonlight") {
      void api.moonlight().catch(reason => error = `Could not open Moonlight: ${describe(reason)}`);
    } else if (needsConfirmation(offer)) {
      confirm = { name: machine.name, offer };
    } else {
      void perform(machine.name, offer);
    }
  }

  async function confirmed() {
    const asked = confirm;
    confirm = null;
    if (!asked) return;
    if (asked.offer === null) {
      working = asked.name;
      try { await api.removeMachine(asked.name); error = ""; }
      catch (reason) { error = `Could not remove machine: ${describe(reason)}`; }
      finally { working = ""; await refresh(); }
    } else {
      await perform(asked.name, asked.offer);
    }
  }

  function openPair(machine: string) {
    detailsFor = null;
    target = machine; key = ""; error = "";
    pairOpen = true;
  }

  function openRemove(machine: string) {
    detailsFor = null;
    confirm = { name: machine, offer: null };
  }

  function openAdd() {
    error = "";
    addOpen = true;
  }

  async function add() {
    working = "new";
    const input: NewMachine = { name, hostname, mac, tailscale_name: mode === "tailscale" ? tailnet : null, key: mode === "lan" ? key : null };
    try {
      await api.addMachine(input);
      addOpen = false;
      name = ""; hostname = ""; mac = ""; tailnet = ""; key = ""; error = "";
      await refresh();
    } catch (reason) { error = `Could not add machine: ${describe(reason)}`; }
    finally { working = ""; }
  }

  async function pair() {
    working = target;
    try { await api.pairMachine(target, key); key = ""; pairOpen = false; error = ""; await refresh(); }
    catch (reason) { error = `Could not pair: ${describe(reason)}`; }
    finally { working = ""; }
  }

  onMount(() => {
    // The snapshot is local and fast, so the registered machines show before
    // the first poll lands; the refresh updates them when it does.
    void snapshot().then(() => { if (mounted) void refresh(); });
    const clock = setInterval(() => now = Date.now(), 1000);
    return () => {
      mounted = false;
      clearTimeout(timer);
      clearInterval(clock);
    };
  });
</script>

<div class="heading">
  <h1>Machines</h1>
  {#if machines.length > 0}<Button onClick={openAdd}>Add machine</Button>{/if}
</div>
{#if pollError}<Callout tone="danger" title="Machine refresh error" message={pollError} />{/if}
{#if error}<Callout tone="danger" title="Machine error" message={error} />{/if}

{#if !loaded}
  <!-- The registry has not answered yet: never the empty state. -->
{:else if machines.length === 0}
  <div class="empty">
    <p>No machines yet. Add one to wake it and manage its power from here.</p>
    <Button onClick={openAdd}>Add machine</Button>
  </div>
{:else}
  <div class="cards">
    {#each machines as machine (machine.name)}
      <MachineCard
        {machine}
        {now}
        busy={working === machine.name}
        onOffer={(offer) => act(machine, offer)}
        onDetails={() => detailsFor = machine.name}
      />
    {/each}
  </div>
{/if}

<Dialog open={details !== null} onOpenChange={value => { if (!value) detailsFor = null; }} title={details?.name ?? "Machine"} description="Connection and pairing" width="sm">
  {#if details}
    <div class="facts">
      <DetailItem presentation="simple" label="Connection" value={details.mode === "lan" ? "LAN" : "Tailscale"} />
      <DetailItem presentation="simple" label="Address" value={details.address} />
      <DetailItem presentation="simple" label="Port" value={String(details.daemon_port)} />
      <DetailItem presentation="simple" label="MAC" value={details.mac} />
      <DetailItem presentation="simple" label="Last observation">{#if details.seen_at_unix_ms !== null}<ObservedAge at={details.seen_at_unix_ms} {now} />{:else}Never{/if}</DetailItem>
      {#if details.error}<DetailItem presentation="simple" label="Last error" value={details.error} />{/if}
    </div>
  {/if}
  {#snippet footer()}
    {#if details}
      {@const name = details.name}
      <div class="dialog-actions">
        <Button variant="ghost" tone="danger" disabled={busy} onClick={() => openRemove(name)}>Remove…</Button>
        <Button variant="secondary" disabled={busy} onClick={() => openPair(name)}>Pair…</Button>
      </div>
    {/if}
  {/snippet}
</Dialog>
<FormDialog open={addOpen} onOpenChange={value => addOpen = value} title="Add machine" submitLabel="Add machine" submitting={working === "new"} error={error || null} onSubmit={add}>
  <Field id="machine-name" label="Name" required><TextInput id="machine-name" value={name} onValueChange={value => name = value} /></Field>
  <Field id="machine-host" label="LAN hostname" required><TextInput id="machine-host" value={hostname} onValueChange={value => hostname = value} /></Field>
  <Field id="machine-mac" label="MAC address" required><TextInput id="machine-mac" value={mac} onValueChange={value => mac = value} /></Field>
  <Field id="machine-mode" label="Connection"><Select id="machine-mode" value={mode} options={[{ value: "lan", label: "LAN" }, { value: "tailscale", label: "Tailscale" }]} onValueChange={value => mode = value} /></Field>
  {#if mode === "lan"}<Field id="machine-key" label="Pairing key" required><TextInput id="machine-key" type="password" value={key} onValueChange={value => key = value} /></Field>
  {:else}<Field id="machine-tailnet" label="Tailscale name" required><TextInput id="machine-tailnet" value={tailnet} onValueChange={value => tailnet = value} /></Field>{/if}
</FormDialog>
<FormDialog open={pairOpen} onOpenChange={value => pairOpen = value} title={`Pair ${target || "machine"}`} submitLabel="Pair" submitting={busy} error={error || null} onSubmit={pair}>
  <Field id="pair-key" label="Pairing key" required><TextInput id="pair-key" type="password" value={key} onValueChange={value => key = value} /></Field>
</FormDialog>
<AlertDialog
  open={confirm !== null}
  onOpenChange={value => { if (!value) confirm = null; }}
  title={confirm?.offer == null ? `Remove ${confirm?.name ?? "machine"}?` : `${confirm.offer === "restart" ? "Restart" : "Shut down"} ${confirm.name}?`}
  description={confirm?.offer == null ? "This removes the machine and its pairing key from Barge." : "This interrupts work on the machine after a short countdown."}
  confirmLabel={confirm?.offer == null ? "Remove" : confirm.offer === "restart" ? "Restart" : "Shut down"}
  tone={confirm?.offer == null ? "danger" : "warning"}
  onConfirm={confirmed}
  onCancel={() => confirm = null}
/>
