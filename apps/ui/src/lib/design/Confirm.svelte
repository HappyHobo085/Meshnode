<script lang="ts">
  import type { UiError } from "../api";
  import { toUiError } from "../api";
  import Button from "./Button.svelte";
  import Field from "./Field.svelte";
  import Sheet from "./Sheet.svelte";

  interface Props {
    title: string;
    /** The honest limit of the action, e.g. "A removed device can never rejoin…". */
    consequence: string;
    /** The device or service name the user must type, exactly. */
    typeToConfirm: string;
    actionLabel: string;
    onConfirm: () => void | Promise<void>;
    open?: boolean;
    onCancel?: () => void;
  }
  let {
    title,
    consequence,
    typeToConfirm,
    actionLabel,
    onConfirm,
    open = true,
    onCancel = () => {},
  }: Props = $props();

  let typed = $state("");
  let busy = $state(false);
  let error: UiError | null = $state(null);
  const matches = $derived(typed === typeToConfirm);

  // Closing forgets what was typed, so reopening starts disabled again.
  $effect(() => {
    if (!open) {
      typed = "";
      error = null;
    }
  });

  async function confirm(e?: SubmitEvent) {
    e?.preventDefault();
    if (!matches || busy) return;
    busy = true;
    error = null;
    try {
      await onConfirm();
    } catch (err) {
      error = toUiError(err);
    } finally {
      busy = false;
    }
  }
</script>

<Sheet {open} {title} onclose={onCancel}>
  <form class="confirm" onsubmit={confirm}>
    <p class="consequence">{consequence}</p>
    <Field
      label={`Type “${typeToConfirm}” to confirm`}
      bind:value={typed}
      autocomplete="off"
      autocapitalize="off"
      spellcheck={false}
      error={error?.message ?? null}
      hint={error?.hint ?? null}
    />
    <div class="actions">
      <Button variant="ghost" onclick={onCancel}>Cancel</Button>
      <Button variant="danger" type="submit" disabled={!matches} {busy}>{actionLabel}</Button>
    </div>
  </form>
</Sheet>

<style>
  .confirm {
    display: grid;
    gap: var(--s-5);
  }
  .consequence {
    color: var(--ink-2);
    max-width: 60ch;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--s-3);
  }
  @media (max-width: 639px) {
    .actions > :global(*) {
      flex: 1;
    }
  }
</style>
