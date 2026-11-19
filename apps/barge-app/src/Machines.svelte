<script lang="ts">
  import { onMount } from "svelte";
  import { AlertDialog, Button, Callout, DetailItem, DetailSection, Field, FormDialog, Select, StatusIndicator, TextInput, TimeAgo } from "@inflatable-cookie/poodle-svelte";
  import type { AppApi, LocalMachine, MachineView, NewMachine } from "./types";

  let { api, refreshInterval }: { api: AppApi; refreshInterval: number } = $props();
  let machines = $state<MachineView[]>([]);
  let local = $state<LocalMachine | null>(null);
  let selected = $state("");
  let error = $state("");
  let pollError = $state("");
  let localError = $state("");
  let busy = $state(false);
  let addOpen = $state(false);
  let pairOpen = $state(false);
  let confirm = $state("");
  let mode = $state("lan");
  let name = $state("");
  let hostname = $state("");
  let mac = $state("");
  let tailnet = $state("");
  let key = $state("");
  let now = $state(Date.now());
  let current = $derived(machines.find(machine => machine.name === selected));
  let pending = $derived(current?.observation?.pending ?? null);
  let seconds = $derived(pending ? Math.max(0, Math.ceil((pending.deadline_unix_ms - now) / 1000)) : 0);
  let allowPower = $derived(!!current?.reachable && current.observation?.power === "awake" && !pending && !busy);
  const describe = (reason: unknown) => reason instanceof Error ? reason.message : String(reason);

  async function refresh() {
    try { machines = await api.machines(); pollError = ""; }
    catch (reason) { pollError = `Could not read machines: ${describe(reason)}`; }
  }
  async function run(operation: string) {
    if (!current) return;
    busy = true;
    try {
      await api.machineAction(current.name, operation);
      error = "";
      await refresh();
    } catch (reason) { error = `${operation === "cancel" ? "Cancel could not be confirmed" : "Action failed"}: ${describe(reason)}`; }
    finally { busy = false; }
  }
  async function add() {
    busy = true;
    const input: NewMachine = { name, hostname, mac, tailscale_name: mode === "tailscale" ? tailnet : null, key: mode === "lan" ? key : null };
    try {
      await api.addMachine(input); addOpen = false; selected = name;
      name = ""; hostname = ""; mac = ""; tailnet = ""; key = ""; error = ""; await refresh();
    } catch (reason) { error = `Could not add machine: ${describe(reason)}`; }
    finally { busy = false; }
  }
  async function pair() {
    if (!current) return;
    busy = true;
    try { await api.pairMachine(current.name, key); key = ""; pairOpen = false; error = ""; await refresh(); }
    catch (reason) { error = `Could not pair: ${describe(reason)}`; }
    finally { busy = false; }
  }
  async function confirmed() {
    const action = confirm; confirm = "";
    if (action === "remove" && current) {
      busy = true;
      try { await api.removeMachine(current.name); selected = ""; error = ""; await refresh(); }
      catch (reason) { error = `Could not remove machine: ${describe(reason)}`; }
      finally { busy = false; }
    } else await run(action);
  }
  function stateLabel(machine: MachineView) {
    if (!machine.reachable) return "Unreachable";
    if (machine.observation?.pending) return "Action pending";
    return machine.observation?.power === "awake" ? "Awake" : machine.observation?.power === "asleep" ? "Asleep" : "Unknown";
  }
  onMount(() => {
    const refreshLocal = () => { void api.localMachine().then(value => { local = value; localError = ""; }).catch(reason => localError = `Could not read this Mac: ${describe(reason)}`); };
    refreshLocal();
    void refresh();
    let last = Date.now();
    const timer = setInterval(() => { now = Date.now(); if (now - last >= refreshInterval * 1000) { last = now; void refresh(); refreshLocal(); } }, 1000);
    return () => clearInterval(timer);
  });
</script>

<div class="heading"><h1>Machines</h1><Button onClick={() => { error = ""; addOpen = true; }}>Add machine</Button></div>
{#if localError}<Callout tone="danger" title="Local refresh error" message={localError} />{/if}
{#if pollError}<Callout tone="danger" title="Machine refresh error" message={pollError} />{/if}
{#if error}<Callout tone="danger" title="Machine error" message={error} />{/if}
<div class="machines-layout">
  <nav aria-label="Machines" class="machine-list">
    <Button variant={selected === "" ? "secondary" : "ghost"} onClick={() => selected = ""}>This Mac · {local ? (local.state === "awake" ? "Awake" : "Unknown") : "Checking…"}{#if local} · <TimeAgo datetime={local.observed_at_unix_ms} live />{/if}</Button>
    {#each machines as machine (machine.name)}
      <Button variant={selected === machine.name ? "secondary" : "ghost"} onClick={() => selected = machine.name}>
        {machine.name} · {machine.mode === "lan" ? "LAN" : "Tailscale"} · {stateLabel(machine)}{#if machine.observation} · <TimeAgo datetime={machine.observation.observed_at_unix_ms} live />{/if}
      </Button>
    {/each}
  </nav>
  <section aria-label="Machine details">
    {#if !selected}
      <h2>This Mac</h2>
      <DetailSection title="System">
        <DetailItem label="Name" value={local?.name ?? "Checking…"} />
        <DetailItem label="Model" value={local?.model ?? null} />
        <DetailItem label="Uptime" value={local?.uptime_seconds != null ? `${Math.floor(local.uptime_seconds / 3600)} hours` : null} />
      </DetailSection>
      <DetailSection title="Storage">
        {#if local?.storage_error}<Callout tone="danger" message={local.storage_error} />
        {:else if local}
          {#each local.volumes as volume}<DetailItem label={volume.name} value={volume.presence === "present" ? `${((volume.free_bytes ?? 0) / 1024 ** 3).toFixed(1)} GiB free` : volume.presence} />{/each}
          {#if local.volumes.length === 0}<p>No volumes registered</p>{/if}
          <DetailItem label="Doctor" value={local.doctor_findings === null ? "Not checked" : local.doctor_findings === 0 ? "OK" : `${local.doctor_findings} findings`} />
        {/if}
      </DetailSection>
    {:else if current}
      <h2>{current.name}</h2>
      <StatusIndicator status={current.reachable ? "success" : "warning"} label={stateLabel(current)} />
      <DetailSection title="Connection">
        <DetailItem label="Mode" value={current.mode === "lan" ? "LAN" : "Tailscale"} />
        <DetailItem label="Address" value={current.address} />
        <DetailItem label="MAC" value={current.mac} />
        <DetailItem label="Last observation">{#if current.observation}<TimeAgo datetime={current.observation.observed_at_unix_ms} live />{:else}Never{/if}</DetailItem>
      </DetailSection>
      {#if current.error}<Callout tone="warning" title="Connection" message={current.error} />{/if}
      {#if pending}
        <p>{pending.action === "sleep" ? "Sleeping" : pending.action === "restart" ? "Restarting" : "Shutting down"} in {seconds} s</p>
        {#if !current.reachable}<p>Cancel cannot be confirmed until the connection returns.</p>{/if}
        <Button disabled={busy || !current.reachable} onClick={() => void run("cancel")}>Cancel action</Button>
      {:else}
        <div class="machine-actions">
          <Button variant="secondary" disabled={busy} onClick={() => void run("wake")}>Wake</Button>
          <Button variant="secondary" disabled={!allowPower} onClick={() => void run("sleep")}>Sleep</Button>
          <Button variant="secondary" disabled={!allowPower} onClick={() => confirm = "restart"}>Restart…</Button>
          <Button variant="secondary" disabled={!allowPower} onClick={() => confirm = "shutdown"}>Shut down…</Button>
        </div>
      {/if}
      <div class="machine-actions">
        <Button variant="ghost" onClick={() => void api.moonlight().catch(reason => error = `Could not open Moonlight: ${describe(reason)}`)}>Open Moonlight</Button>
        <Button variant="ghost" onClick={() => { key = ""; pairOpen = true; }}>Pair</Button>
        <Button variant="ghost" onClick={() => confirm = "remove"}>Remove…</Button>
      </div>
    {:else}<p>Machine no longer registered.</p>{/if}
  </section>
</div>

<FormDialog open={addOpen} onOpenChange={value => addOpen = value} title="Add machine" submitLabel="Add machine" submitting={busy} error={error || null} onSubmit={add}>
  <Field id="machine-name" label="Name" required><TextInput id="machine-name" value={name} onValueChange={value => name = value} /></Field>
  <Field id="machine-host" label="LAN hostname" required><TextInput id="machine-host" value={hostname} onValueChange={value => hostname = value} /></Field>
  <Field id="machine-mac" label="MAC address" required><TextInput id="machine-mac" value={mac} onValueChange={value => mac = value} /></Field>
  <Field id="machine-mode" label="Connection"><Select id="machine-mode" value={mode} options={[{ value: "lan", label: "LAN" }, { value: "tailscale", label: "Tailscale" }]} onValueChange={value => mode = value} /></Field>
  {#if mode === "lan"}<Field id="machine-key" label="Pairing key" required><TextInput id="machine-key" type="password" value={key} onValueChange={value => key = value} /></Field>
  {:else}<Field id="machine-tailnet" label="Tailscale name" required><TextInput id="machine-tailnet" value={tailnet} onValueChange={value => tailnet = value} /></Field>{/if}
</FormDialog>
<FormDialog open={pairOpen} onOpenChange={value => pairOpen = value} title={`Pair ${current?.name ?? "machine"}`} submitLabel="Pair" submitting={busy} error={error || null} onSubmit={pair}>
  <Field id="pair-key" label="Pairing key" required><TextInput id="pair-key" type="password" value={key} onValueChange={value => key = value} /></Field>
</FormDialog>
<AlertDialog open={!!confirm} onOpenChange={value => { if (!value) confirm = ""; }} title={confirm === "remove" ? `Remove ${current?.name}?` : `${confirm === "restart" ? "Restart" : "Shut down"} ${current?.name}?`} description={confirm === "remove" ? "This removes the machine and its pairing key from Barge." : "This interrupts work on the machine after a short countdown."} confirmLabel={confirm === "remove" ? "Remove" : confirm === "restart" ? "Restart" : "Shut down"} tone={confirm === "remove" ? "danger" : "warning"} onConfirm={confirmed} onCancel={() => confirm = ""} />
