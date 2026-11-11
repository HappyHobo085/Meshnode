<script lang="ts" module>
  /** What to type when this device's name isn't known. */
  export const RESET_WORD = "reset";

  /**
   * The text the user types to confirm a reset: this device's name, or `RESET_WORD`. The
   * daemon reports the name only while it can read the node or `meshd.toml`
   * (`status_info`); the in-process status Android uses may leave it out.
   */
  export function resetConfirmText(name: string | null | undefined): string {
    const n = name?.trim();
    return n ? n : RESET_WORD;
  }
</script>

<script lang="ts">
  import { api } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Confirm from "../lib/design/Confirm.svelte";
  import { navigate } from "../lib/router";
  import { refreshNow, status, toast } from "../lib/stores";

  interface Props {
    /** The daemon accepts a reset only once the node was removed (`reset_identity_by`). */
    disabled?: boolean;
    variant?: "danger" | "primary";
  }
  let { disabled = false, variant = "danger" }: Props = $props();

  let open = $state(false);
  const name = $derived($status?.name?.trim() || null);
  const typed = $derived(resetConfirmText(name));

  async function reset() {
    await api.resetIdentity();
    open = false;
    await refreshNow();
    toast("Identity reset");
    navigate({ name: "first-run" }, { replace: true });
  }
</script>

<Button {variant} {disabled} onclick={() => (open = true)}>Reset identity…</Button>

<Confirm
  {open}
  title={name ? `Reset ${name}'s identity?` : "Reset this device's identity?"}
  consequence={"This deletes this device's identity and its copy of the mesh's data. It gets a new identity and starts again from first run. A removed device can never rejoin with the same identity." +
    (name ? "" : ` This device's name isn't available right now, so type “${RESET_WORD}”.`)}
  typeToConfirm={typed}
  actionLabel="Reset identity"
  onConfirm={reset}
  onCancel={() => (open = false)}
/>
