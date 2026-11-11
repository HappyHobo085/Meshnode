<script lang="ts">
  import type { HTMLInputAttributes } from "svelte/elements";

  interface Props extends Omit<HTMLInputAttributes, "value"> {
    label: string;
    value?: string;
    /** Help under the field. */
    help?: string;
    /** An error under the field (with its hint), announced to screen readers. */
    error?: string | null;
    hint?: string | null;
    /** Monospace input, for tickets, ids and addresses. */
    mono?: boolean;
  }
  let {
    label,
    value = $bindable(""),
    help,
    error = null,
    hint = null,
    mono = false,
    id,
    class: klass = "",
    ...rest
  }: Props = $props();

  const autoId = $props.id();
  const inputId = $derived(id ?? `field-${autoId}`);
  const describedBy = $derived(
    [help && `${inputId}-help`, error && `${inputId}-error`].filter(Boolean).join(" ") || undefined,
  );
</script>

<div class={["field", klass]}>
  <label for={inputId}>{label}</label>
  <input
    {...rest}
    id={inputId}
    class={{ mono, invalid: !!error }}
    bind:value
    aria-invalid={error ? "true" : undefined}
    aria-describedby={describedBy}
  />
  {#if help}<p class="help" id="{inputId}-help">{help}</p>{/if}
  {#if error}
    <p class="error" id="{inputId}-error" role="alert">
      {error}{#if hint}<span class="hint">{hint}</span>{/if}
    </p>
  {/if}
</div>

<style>
  .field {
    display: grid;
    gap: var(--s-2);
  }
  label {
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    color: var(--ink-2);
  }
  input {
    min-height: var(--touch);
    width: 100%;
    padding: 0 var(--s-3);
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    color: var(--ink);
    transition:
      border-color var(--dur-1) var(--ease-out),
      box-shadow var(--dur-1) var(--ease-out);
  }
  input::placeholder {
    color: var(--ink-3);
  }
  input:hover {
    border-color: var(--ink-3);
  }
  input:focus-visible,
  input:focus {
    border-color: var(--signal);
    box-shadow: 0 0 0 3px var(--signal-soft);
    border-radius: var(--r-sm);
  }
  input.mono {
    font-family: var(--font-mono);
    font-size: var(--fs-sm);
    font-variant-ligatures: none;
  }
  input.invalid {
    border-color: var(--danger);
  }
  .help {
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
</style>
