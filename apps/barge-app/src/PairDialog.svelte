<script lang="ts">
  import { Button, Callout, Dialog, Spinner } from "@inflatable-cookie/poodle-svelte";
  import type { PairOutcome } from "./types";

  export interface PairingState {
    requestId: string;
    machineName: string;
    code: string;
    expiresAtUnixMs: number;
    status: PairOutcome | "error";
    error?: string;
    retry: () => void;
  }

  let { pairing, onClose }: {
    pairing: PairingState | null;
    onClose: () => void;
  } = $props();
</script>

<Dialog
  open={pairing !== null}
  onOpenChange={(open) => { if (!open) onClose(); }}
  title={pairing ? `Pair ${pairing.machineName}` : "Pair machine"}
  description={pairing?.status === "pending" ? "Confirm this six-digit code matches" : undefined}
  width="sm"
>
  {#if pairing}
    {#if pairing.status === "pending"}
      <div class="pair-code-panel">
        <div class="pair-code-box">
          <span class="pair-code" aria-label="Pairing code">{pairing.code}</span>
        </div>
        <p class="pair-instruction">Approve on the PC or another paired device</p>
        <p class="pair-waiting">
          <Spinner size="sm" variant="ring" />
          <span>Waiting for approval…</span>
        </p>
      </div>
    {:else if pairing.status === "approved"}
      <Callout
        tone="success"
        announceMode="polite"
        title="Paired"
        message={`${pairing.machineName} is paired and ready.`}
      />
    {:else if pairing.status === "denied"}
      <Callout
        tone="danger"
        announceMode="assertive"
        title="Pairing denied"
        message={`The pairing request for ${pairing.machineName} was denied.`}
      />
    {:else if pairing.status === "expired"}
      <Callout
        tone="warning"
        announceMode="polite"
        title="Pairing expired"
        message={`The pairing request for ${pairing.machineName} expired before it was approved.`}
      />
    {:else}
      <Callout
        tone="danger"
        announceMode="assertive"
        title="Pairing failed"
        message={pairing.error ?? "Pairing could not be completed."}
      />
    {/if}
  {/if}
  {#snippet footer()}
    {#if pairing}
      <div class="dialog-actions dialog-actions-end">
        {#if pairing.status === "pending"}
          <Button variant="secondary" onClick={onClose}>Cancel</Button>
        {:else if pairing.status === "approved"}
          <Button variant="primary" onClick={onClose}>Done</Button>
        {:else}
          <Button variant="secondary" onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={pairing.retry}>Try again</Button>
        {/if}
      </div>
    {/if}
  {/snippet}
</Dialog>
