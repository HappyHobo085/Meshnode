<script lang="ts">
  import { api } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Confirm from "../lib/design/Confirm.svelte";
  import { refreshNow, status, toast } from "../lib/stores";
  import { RESET_WORD, resetConfirmText } from "./ResetIdentity.svelte";

  interface Props {
    /** The daemon declares only a running node (and refuses while it re-bootstraps). */
    disabled?: boolean;
  }
  let { disabled = false }: Props = $props();

  let open = $state(false);
  const name = $derived($status?.name?.trim() || null);
  const typed = $derived(resetConfirmText(name));

  async function declare() {
    await api.declareFresh();
    open = false;
    await refreshNow();
    toast("Declared up to date");
  }
</script>

<Button variant="danger" {disabled} onclick={() => (open = true)}>Declare up to date…</Button>

<Confirm
  {open}
  title={name ? `Declare ${name} up to date?` : "Declare this device up to date?"}
  consequence={"This device then counts as a fresh copy of the mesh's data, although it hasn't synced with another host for over 80 days, and it starts cleaning up deleted data again. The risk: deletions that other hosts already cleaned up may come back." +
    (name ? "" : ` This device's name isn't available right now, so type “${RESET_WORD}”.`)}
  typeToConfirm={typed}
  actionLabel="Declare up to date"
  onConfirm={declare}
  onCancel={() => (open = false)}
/>
