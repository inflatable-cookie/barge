<script lang="ts">
  import { onMount } from "svelte";
  import DoctorStatusView from "./DoctorStatus.svelte";
  import { Button, Callout, EmptyState, TimeAgo } from "@inflatable-cookie/poodle-svelte";
  import type { AppApi, DoctorStatus, VolumeSummary } from "./types";

  let { api }: { api: AppApi } = $props();

  let volumes = $state<VolumeSummary[]>([]);
  let doctor = $state<DoctorStatus | null>(null);
  let storageError = $state("");
  let observedAt = $state<number | null>(null);
  let loading = $state(true);

  /** Tauri rejects with a string; a browser or test rejects with an Error. */
  function describe(reason: unknown) {
    return reason instanceof Error ? reason.message : String(reason);
  }

  function space(volume: VolumeSummary) {
    if (volume.presence === "absent") return "Absent";
    if (volume.presence === "mismatch") return "Different volume at mount";
    return `${((volume.free_bytes ?? 0) / 1024 ** 3).toFixed(1)} GiB free`;
  }

  /**
   * Reads storage on open, and when the operator asks for a refresh. It is
   * never polled (app.md, Architecture): a poll only ever covers machine state.
   */
  async function readStorage() {
    try {
      volumes = await api.volumes();
      try {
        doctor = await api.doctorStatus();
      } catch (reason) {
        doctor = { observation: doctor?.observation ?? null, error: describe(reason) };
      }
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

  onMount(() => {
    void readStorage();
  });
</script>

{#snippet observed(at: number)}
  <p class="observed">Last read <TimeAgo datetime={at} live={false} />{#if storageError} · showing an earlier observation{/if}</p>
{/snippet}

{#snippet storageErrorNote()}
  {#if storageError}<Callout tone="danger" announceMode="assertive" title="Storage could not be read" message={storageError} />{/if}
{/snippet}

<div class="heading">
  <h1>Storage</h1>
  <Button variant="ghost" onClick={() => void readStorage()}>Refresh</Button>
</div>
{#if loading}
  <p>Checking storage…</p>
{:else}
  {@render storageErrorNote()}
  <DoctorStatusView status={doctor} />
  {#if observedAt !== null}
    {@render observed(observedAt)}
    {#if volumes.length === 0}
      <EmptyState title="No volumes registered" message="Register a volume with Barge to see it here." />
    {:else}
      <ul class="volumes">
        {#each volumes as volume}
          <li><strong>{volume.name}</strong><span>{space(volume)}</span></li>
        {/each}
      </ul>
    {/if}
  {/if}
{/if}
