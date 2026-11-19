<script lang="ts">
  import { Callout, TimeAgo } from "@inflatable-cookie/poodle-svelte";
  import type { DoctorStatus } from "./types";
  let { status }: { status: DoctorStatus | null } = $props();
</script>

{#if status?.error}
  <Callout tone="danger" title="Doctor could not be completed" message={status.error} />
{/if}
{#if status?.observation}
  <p>Last doctor result: {status.observation.findings === 0 ? "OK" : `${status.observation.findings} findings`} · <TimeAgo datetime={status.observation.observed_at_unix_ms} live />{#if status.error} · earlier result{/if}</p>
{:else if !status?.error}
  <p>Doctor has not been checked.</p>
{/if}
