<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import MachineCard from "./MachineCard.svelte";
  import FoundMachineCard from "./FoundMachineCard.svelte";
  import PairDialog, { type PairingState } from "./PairDialog.svelte";
  import ObservedAge from "./ObservedAge.svelte";
  import {
    AlertDialog,
    Button,
    Callout,
    DetailItem,
    Dialog,
    Field,
    FormDialog,
    Pill,
    TextInput,
  } from "@inflatable-cookie/poodle-svelte";
  import { needsConfirmation, offerLabel } from "./machineModel";
  import type {
    AppApi,
    DiscoveredMachineView,
    MachineView,
    Offer,
    PairStartInput,
    PairedClientView,
    PendingPairPrompt,
  } from "./types";

  /** Polls this often while a machine is waiting on an action (app.md, machines.md). */
  const FAST_POLL_MS = 2000;
  /** Polls this often while waiting for a pairing decision. */
  const PAIR_POLL_MS = 500;

  let { api, refreshInterval }: { api: AppApi; refreshInterval: number } = $props();
  let machines = $state<MachineView[]>([]);
  let discovered = $state<DiscoveredMachineView[]>([]);
  let pendingPairs = $state<PendingPairPrompt[]>([]);
  /** False until the first registry snapshot returns: never the empty state. */
  let loaded = $state(false);
  let discovering = $state(false);
  let pollError = $state("");
  let error = $state("");
  let working = $state("");
  let actingPair = $state("");

  let addByAddressOpen = $state(false);
  let addressInput = $state("");
  let portInput = $state("9777");
  let nameInput = $state("");
  let addressError = $state("");

  let pairing = $state<PairingState | null>(null);

  /** The machine whose details dialog is open, by name, so a poll refreshes it. */
  let detailsFor = $state<string | null>(null);
  let details = $derived(machines.find((machine) => machine.name === detailsFor) ?? null);
  let clients = $state<PairedClientView[]>([]);
  let clientsLoading = $state(false);
  let clientsError = $state("");

  type PendingConfirm =
    | { kind: "power"; name: string; offer: Offer }
    | { kind: "remove"; name: string }
    | { kind: "revoke"; machine: string; client: PairedClientView };

  let confirm = $state<PendingConfirm | null>(null);
  let now = $state(Date.now());
  let busy = $derived(working !== "");
  const describe = (reason: unknown) => (reason instanceof Error ? reason.message : String(reason));

  let timer: ReturnType<typeof setTimeout> | undefined;
  let pairTimer: ReturnType<typeof setTimeout> | undefined;
  let mounted = true;
  let machineEpoch = 0;

  /** What the registry names, without touching the network: shows at once. */
  async function snapshot() {
    const epoch = machineEpoch;
    try {
      const next = await api.machines();
      if (epoch === machineEpoch) {
        machines = next;
        now = Date.now();
      }
    } catch (reason) {
      pollError = `Could not read machines: ${describe(reason)}`;
    } finally {
      loaded = true;
    }
  }

  /** Poll every daemon and pending pair requests; a slow one holds only itself. */
  async function refresh() {
    const epoch = machineEpoch;
    try {
      const nextMachines = await api.refreshMachines();
      if (epoch === machineEpoch) {
        machines = nextMachines;
        now = Date.now();
      }
      pollError = "";
      void api
        .pendingPairs()
        .then((nextPending) => {
          if (mounted) pendingPairs = nextPending;
        })
        .catch(() => {});
    } catch (reason) {
      pollError = `Could not read machines: ${describe(reason)}`;
    }
    schedule();
  }

  /** Browse `_barged._tcp` on the LAN for unregistered machines and moved addresses. */
  async function discover() {
    if (discovering) return;
    discovering = true;
    try {
      discovered = await api.discoverMachines();
    } catch (reason) {
      error = `Discovery failed: ${describe(reason)}`;
    } finally {
      discovering = false;
    }
  }

  /** Waiting machines poll fast; everything else waits the refresh interval. */
  function schedule() {
    clearTimeout(timer);
    if (!mounted) return;
    const delay = machines.some((machine) => machine.status.waiting)
      ? FAST_POLL_MS
      : refreshInterval * 1000;
    timer = setTimeout(() => {
      void refresh();
      void discover();
    }, delay);
  }

  async function perform(machine: string, offer: Offer) {
    working = machine;
    machineEpoch += 1;
    try {
      // The shared state records the click here, so the card and the menu show
      // its wait before the command returns and before the next poll. A waiting
      // machine then polls fast.
      machines = await api.beginMachineAction(machine, offer);
      now = Date.now();
      schedule();
      await api.machineAction(machine, offer);
      error = "";
    } catch (reason) {
      error =
        offer === "cancel"
          ? `Cancel could not be confirmed: ${describe(reason)}`
          : `${offerLabel(offer)} failed: ${describe(reason)}`;
    } finally {
      working = "";
      await refresh();
    }
  }

  function act(machine: MachineView, offer: Offer) {
    if (offer === "moonlight") {
      void api.moonlight().catch((reason) => (error = `Could not open Moonlight: ${describe(reason)}`));
    } else if (needsConfirmation(offer)) {
      confirm = { kind: "power", name: machine.name, offer };
    } else {
      void perform(machine.name, offer);
    }
  }

  async function confirmed() {
    const asked = confirm;
    confirm = null;
    if (!asked) return;
    if (asked.kind === "remove") {
      working = asked.name;
      try {
        await api.removeMachine(asked.name);
        error = "";
      } catch (reason) {
        error = `Could not remove machine: ${describe(reason)}`;
      } finally {
        working = "";
        await refresh();
      }
    } else if (asked.kind === "revoke") {
      working = asked.machine;
      try {
        await api.revokeClient(asked.machine, asked.client.id);
        error = "";
        if (asked.client.this_client) {
          clients = [];
          await snapshot();
          await refresh();
        } else if (detailsFor === asked.machine) {
          await loadClients(asked.machine);
        }
      } catch (reason) {
        clientsError = `Could not revoke client: ${describe(reason)}`;
      } finally {
        working = "";
      }
    } else {
      await perform(asked.name, asked.offer);
    }
  }

  async function loadClients(machineName: string) {
    clientsLoading = true;
    clientsError = "";
    try {
      const list = await api.machineClients(machineName);
      if (detailsFor === machineName) {
        clients = list;
      }
    } catch (reason) {
      if (detailsFor === machineName) {
        clientsError = `Could not load paired clients: ${describe(reason)}`;
      }
    } finally {
      if (detailsFor === machineName) {
        clientsLoading = false;
      }
    }
  }

  function openDetails(machine: MachineView) {
    detailsFor = machine.name;
    clients = [];
    clientsError = "";
    if (machine.paired) {
      void loadClients(machine.name);
    } else {
      clientsLoading = false;
    }
  }

  function openRemove(machine: string) {
    detailsFor = null;
    confirm = { kind: "remove", name: machine };
  }

  function openRevoke(machine: string, client: PairedClientView) {
    confirm = { kind: "revoke", machine, client };
  }

  function openAddByAddress() {
    addressError = "";
    addressInput = "";
    portInput = "9777";
    nameInput = "";
    addByAddressOpen = true;
  }

  async function startPairing(input: PairStartInput) {
    const label = input.existing_name ?? input.name ?? input.address;
    working = label;
    error = "";
    try {
      const started = await api.startPair(input);
      pairing = {
        requestId: started.request_id,
        machineName: started.machine_name,
        code: started.code,
        expiresAtUnixMs: started.expires_at_unix_ms,
        status: "pending",
        retry: () => void startPairing(input),
      };
      schedulePairPoll(started.request_id);
    } catch (reason) {
      error = `Could not start pairing with ${label}: ${describe(reason)}`;
    } finally {
      working = "";
    }
  }

  function schedulePairPoll(requestId: string) {
    clearTimeout(pairTimer);
    if (!mounted || pairing?.requestId !== requestId) return;
    pairTimer = setTimeout(() => void pollPair(requestId), PAIR_POLL_MS);
  }

  async function pollPair(requestId: string) {
    if (!mounted || pairing?.requestId !== requestId) return;
    try {
      const result = await api.pollPair(requestId);
      if (!mounted || pairing?.requestId !== requestId) return;
      if (result.status === "pending") {
        schedulePairPoll(requestId);
        return;
      }
      pairing = {
        ...pairing,
        status: result.status,
        machineName: result.machine_name,
      };
      if (result.status === "approved") {
        await snapshot();
        await refresh();
        await discover();
        if (detailsFor === result.machine_name) {
          await loadClients(result.machine_name);
        }
      }
    } catch (reason) {
      if (!mounted || pairing?.requestId !== requestId) return;
      pairing = {
        ...pairing,
        status: "error",
        error: describe(reason),
      };
    }
  }

  function closePairing() {
    clearTimeout(pairTimer);
    const active = pairing;
    pairing = null;
    if (active?.status === "pending") {
      void api.cancelPair(active.requestId).catch(() => {});
    }
  }

  function addDiscovered(found: DiscoveredMachineView) {
    void startPairing({
      address: found.address,
      port: found.port,
      name: found.name,
      machine_id: found.id,
      existing_name: null,
    });
  }

  function pairExisting(machine: MachineView) {
    detailsFor = null;
    void startPairing({
      address: machine.hostname,
      port: machine.daemon_port,
      name: machine.name,
      machine_id: null,
      existing_name: machine.name,
    });
  }

  function submitAddByAddress() {
    const trimmedAddress = addressInput.trim();
    if (!trimmedAddress) {
      addressError = "Enter a host or IP address.";
      return;
    }
    const parsedPort = Number(portInput.trim() || "9777");
    if (!Number.isInteger(parsedPort) || parsedPort < 1 || parsedPort > 65535) {
      addressError = "Port must be between 1 and 65535.";
      return;
    }
    const trimmedName = nameInput.trim();
    addByAddressOpen = false;
    addressError = "";
    void startPairing({
      address: trimmedAddress,
      port: parsedPort,
      name: trimmedName || null,
      machine_id: null,
      existing_name: null,
    });
  }

  async function decidePair(prompt: PendingPairPrompt, approve: boolean) {
    actingPair = prompt.request_id;
    try {
      if (approve) {
        await api.approvePair(prompt.machine_name, prompt.request_id);
      } else {
        await api.denyPair(prompt.machine_name, prompt.request_id);
      }
      pendingPairs = pendingPairs.filter((item) => item.request_id !== prompt.request_id);
      error = "";
    } catch (reason) {
      error = `Could not ${approve ? "approve" : "deny"} pairing: ${describe(reason)}`;
    } finally {
      actingPair = "";
    }
  }

  onMount(() => {
    let unlisten: (() => void) | undefined;
    if ("__TAURI_INTERNALS__" in window) {
      void listen<PendingPairPrompt[]>("pending-pairs-changed", (event) => {
        pendingPairs = event.payload;
      }).then((dispose) => {
        unlisten = dispose;
      });
    }
    // The snapshot is local and fast, so the registered machines show before
    // the first poll lands; refresh and discovery update them when they land.
    void snapshot().then(() => {
      if (mounted) {
        void refresh();
        void discover();
      }
    });
    const clock = setInterval(() => (now = Date.now()), 1000);
    return () => {
      mounted = false;
      clearTimeout(timer);
      clearTimeout(pairTimer);
      clearInterval(clock);
      unlisten?.();
    };
  });
</script>

<div class="heading">
  <h1>Machines</h1>
  <div class="heading-actions">
    {#if machines.length > 0 || discovered.length > 0}
      <Button variant="ghost" size="sm" disabled={discovering} onClick={() => void discover()}>
        {discovering ? "Scanning…" : "Scan"}
      </Button>
      <Button variant="secondary" size="sm" onClick={openAddByAddress}>Add by address</Button>
    {/if}
  </div>
</div>

{#if pollError}<Callout tone="danger" title="Machine refresh error" message={pollError} />{/if}
{#if error}<Callout tone="danger" title="Machine error" message={error} />{/if}

{#if pendingPairs.length > 0}
  <div class="pair-prompts" aria-label="Pairing requests">
    {#each pendingPairs as prompt (prompt.request_id)}
      <Callout
        tone="info"
        announceMode="polite"
        title={`${prompt.client_name} wants to pair with ${prompt.machine_name}`}
      >
        <p>Verification code <strong class="pair-code-inline">{prompt.code}</strong></p>
        {#snippet actions()}
          <Button
            variant="secondary"
            size="sm"
            disabled={actingPair === prompt.request_id}
            onClick={() => void decidePair(prompt, false)}
          >
            Deny
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={actingPair === prompt.request_id}
            onClick={() => void decidePair(prompt, true)}
          >
            Approve
          </Button>
        {/snippet}
      </Callout>
    {/each}
  </div>
{/if}

{#if !loaded}
  <!-- The registry has not answered yet: never the empty state. -->
{:else if machines.length === 0 && discovered.length === 0}
  <div class="empty">
    <p>No machines found on the network. Make sure barged is running on your PC, or add one by address.</p>
    <Button variant="secondary" onClick={openAddByAddress}>Add by address</Button>
  </div>
{:else}
  {#if machines.length > 0}
    <div class="cards">
      {#each machines as machine (machine.name)}
        <MachineCard
          {machine}
          {now}
          busy={working === machine.name}
          onOffer={(offer) => act(machine, offer)}
          onDetails={() => openDetails(machine)}
        />
      {/each}
    </div>
  {/if}

  {#if discovered.length > 0}
    <h2 class="section-label">Found on network</h2>
    <div class="cards">
      {#each discovered as found (found.id)}
        <FoundMachineCard
          machine={found}
          busy={busy}
          onAdd={() => addDiscovered(found)}
        />
      {/each}
    </div>
  {/if}
{/if}

<Dialog
  open={details !== null}
  onOpenChange={(value) => {
    if (!value) detailsFor = null;
  }}
  title={details?.name ?? "Machine"}
  description="Connection, pairing and clients"
  width="sm"
>
  {#if details}
    <div class="facts">
      <DetailItem presentation="simple" label="Connection" value={details.mode === "lan" ? "LAN" : "Tailscale"} />
      <DetailItem presentation="simple" label="Address" value={details.address} />
      <DetailItem presentation="simple" label="Port" value={String(details.daemon_port)} />
      <DetailItem presentation="simple" label="MAC" value={details.mac || "Unknown"} />
      <DetailItem presentation="simple" label="Last observation">
        {#if details.seen_at_unix_ms !== null}
          <ObservedAge at={details.seen_at_unix_ms} {now} />
        {:else}
          Never
        {/if}
      </DetailItem>
      {#if details.error}<DetailItem presentation="simple" label="Last error" value={details.error} />{/if}
    </div>

    <section class="clients-section" aria-label="Paired clients">
      <h3>Paired clients</h3>
      {#if !details.paired}
        <p class="muted">Pair this machine in LAN mode to manage clients.</p>
      {:else if clientsLoading}
        <p class="muted">Loading paired clients…</p>
      {:else if clientsError}
        <Callout tone="danger" message={clientsError} />
      {:else if clients.length === 0}
        <p class="muted">No paired clients.</p>
      {:else}
        <ul class="clients-list">
          {#each clients as client (client.id)}
            <li>
              <div class="client-info">
                <span class="client-name">
                  <span>{client.name}</span>
                  {#if client.this_client}
                    <Pill size="sm" tone="neutral">This Mac</Pill>
                  {/if}
                </span>
                <span class="client-meta">
                  Paired <ObservedAge at={client.paired_at_unix_ms} {now} />
                </span>
              </div>
              <Button
                variant="ghost"
                tone="danger"
                size="sm"
                disabled={busy}
                onClick={() => openRevoke(details.name, client)}
              >
                Revoke…
              </Button>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}
  {#snippet footer()}
    {#if details}
      {@const current = details}
      <div class="dialog-actions">
        <Button variant="ghost" tone="danger" disabled={busy} onClick={() => openRemove(current.name)}>
          Remove…
        </Button>
        <Button variant="secondary" disabled={busy} onClick={() => pairExisting(current)}>
          Pair…
        </Button>
      </div>
    {/if}
  {/snippet}
</Dialog>

<FormDialog
  open={addByAddressOpen}
  onOpenChange={(value) => (addByAddressOpen = value)}
  title="Add by address"
  description="For networks where mDNS discovery is unavailable"
  submitLabel="Pair"
  submitting={busy}
  error={addressError || null}
  onSubmit={submitAddByAddress}
>
  <Field id="machine-address" label="Address" required>
    <TextInput
      id="machine-address"
      placeholder="192.168.1.20 or bazzite.local"
      value={addressInput}
      onValueChange={(value) => (addressInput = value)}
    />
  </Field>
  <Field id="machine-port" label="Port">
    <TextInput
      id="machine-port"
      inputMode="numeric"
      value={portInput}
      onValueChange={(value) => (portInput = value)}
    />
  </Field>
  <Field id="machine-label" label="Name">
    <TextInput
      id="machine-label"
      placeholder="Optional display name"
      value={nameInput}
      onValueChange={(value) => (nameInput = value)}
    />
  </Field>
</FormDialog>

<PairDialog {pairing} onClose={closePairing} />

<AlertDialog
  open={confirm !== null}
  onOpenChange={(value) => {
    if (!value) confirm = null;
  }}
  title={confirm?.kind === "remove"
    ? `Remove ${confirm.name}?`
    : confirm?.kind === "revoke"
      ? `Revoke ${confirm.client.name}?`
      : confirm?.kind === "power"
        ? `${confirm.offer === "restart" ? "Restart" : "Shut down"} ${confirm.name}?`
        : ""}
  description={confirm?.kind === "remove"
    ? "This removes the machine and its pairing key from Barge."
    : confirm?.kind === "revoke"
      ? confirm.client.this_client
        ? `This removes this Mac's access to ${confirm.machine}. You will need to pair again to control it.`
        : `${confirm.client.name} will no longer be able to control ${confirm.machine}.`
      : "This interrupts work on the machine after a short countdown."}
  confirmLabel={confirm?.kind === "remove"
    ? "Remove"
    : confirm?.kind === "revoke"
      ? "Revoke"
      : confirm?.kind === "power" && confirm.offer === "restart"
        ? "Restart"
        : "Shut down"}
  tone={confirm?.kind === "remove" || confirm?.kind === "revoke" ? "danger" : "warning"}
  onConfirm={confirmed}
  onCancel={() => (confirm = null)}
/>
