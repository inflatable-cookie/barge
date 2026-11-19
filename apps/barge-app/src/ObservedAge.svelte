<script lang="ts">
  import { Tooltip } from "@inflatable-cookie/poodle-svelte";
  import { observedAge } from "./machineModel";

  /** An observation's age on the window's clock; never in the future (`observedAge`). */
  let { at, now }: { at: number; now: number } = $props();
  let when = $derived(new Date(at));
</script>

<Tooltip content={when.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "medium" })}>
  <time class="age" datetime={when.toISOString()}>{observedAge(at, now)}</time>
</Tooltip>
